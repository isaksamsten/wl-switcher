//! Per-capture state: one ext_image_copy_capture session, two buffers. A
//! capture shows a window's preview in the strip.
//!
//! Frames are captured into the back buffer; on `ready` the buffer is handed
//! to the overlay (attached to the preview subsurface) and the buffers swap.
//! In live mode the next frame is requested right away; the compositor only
//! answers once the window has new content. A static capture stops after
//! one frame (`Paused`), and `set_live` resumes or pauses it, keeping the
//! session and its buffers.
//!
//! Buffers are dmabufs when the session offers a device and a format the
//! compositor accepts (the copy stays on the GPU), and shm otherwise. A
//! session whose dmabuf allocation or first dmabuf frame fails switches to
//! shm.

use std::sync::atomic::{AtomicU32, Ordering};

use smithay_client_toolkit::{dispatch2::Dispatch2, shm::raw::RawPool};
use wayland_client::{
    Connection, QueueHandle, WEnum,
    protocol::{wl_buffer::WlBuffer, wl_output::Transform, wl_shm},
};
use wayland_protocols::ext::{
    foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    image_capture_source::v1::client::ext_image_capture_source_v1::ExtImageCaptureSourceV1,
    image_copy_capture::v1::client::{
        ext_image_copy_capture_frame_v1::{self, ExtImageCopyCaptureFrameV1, FailureReason},
        ext_image_copy_capture_manager_v1::Options,
        ext_image_copy_capture_session_v1::{self, ExtImageCopyCaptureSessionV1},
    },
};

use crate::app::App;
use crate::dmabuf::{self, GpuBuffer};
use crate::globals::{Globals, Ignore, Shm};

const MAX_FAILURES: u32 = 3;

/// Identifies a capture's window across events. Unique for the whole
/// process, so late events for a previous show's captures never reach the
/// current one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct WindowId(u32);

impl WindowId {
    pub fn next() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// What the overlay needs to know about.
pub enum CaptureEvent {
    /// A new frame to show. Its buffer stays busy until the compositor
    /// releases it.
    Frame(Frame),
    /// Capturing gave up before any frame arrived.
    Failed(&'static str),
}

pub struct Frame {
    pub buffer: WlBuffer,
    pub width: u32,
    pub height: u32,
    pub transform: Transform,
}

enum State {
    /// Waiting for the first buffer constraints.
    Starting,
    /// Buffers are allocated; a frame is requested once the back buffer is free.
    Idle,
    /// A frame is in flight. `realloc`: new constraints arrived meanwhile and
    /// are applied when it finishes.
    InFlight {
        frame: ExtImageCopyCaptureFrameV1,
        realloc: bool,
    },
    /// Not live: no further frames until `set_live(true)`. `realloc`: new
    /// constraints arrived meanwhile.
    Paused { realloc: bool },
    /// No further frames will be requested.
    Done,
}

/// The latest buffer constraints from the session.
#[derive(Default)]
struct Constraints {
    width: u32,
    height: u32,
    shm_formats: Vec<wl_shm::Format>,
    /// dev_t of the device dmabufs must be allocated on.
    dmabuf_device: Option<u64>,
    /// (DRM format, modifiers).
    dmabuf_formats: Vec<(u32, Vec<u64>)>,
    /// A `done` ended the batch; the next constraint event starts a new one.
    complete: bool,
}

impl Constraints {
    fn begin_event(&mut self) {
        if self.complete {
            self.shm_formats.clear();
            self.dmabuf_device = None;
            self.dmabuf_formats.clear();
            self.complete = false;
        }
    }

    fn shm_format(&self) -> Option<wl_shm::Format> {
        use wl_shm::Format::*;
        self.shm_formats
            .iter()
            .copied()
            .find(|f| matches!(f, Argb8888 | Xrgb8888 | Abgr8888 | Xbgr8888))
    }

    /// The preferred dmabuf format and its modifiers that the compositor
    /// also accepts.
    fn dmabuf_kind(&self, g: &Globals) -> Option<Kind> {
        let dmabuf = g.dmabuf.as_ref()?;
        let device = self.dmabuf_device?;
        dmabuf::FORMATS.iter().find_map(|&fourcc| {
            let format = fourcc as u32;
            let offered: Vec<u64> = self
                .dmabuf_formats
                .iter()
                .filter(|(f, _)| *f == format)
                .flat_map(|(_, mods)| mods.iter().copied())
                .collect();
            let modifiers = dmabuf.accepted(format, &offered);
            (!modifiers.is_empty()).then_some(Kind::Dmabuf {
                device,
                format,
                modifiers,
            })
        })
    }
}

/// How a capture's buffers are allocated.
#[derive(Clone, Debug, PartialEq)]
enum Kind {
    Shm(wl_shm::Format),
    Dmabuf {
        device: u64,
        format: u32,
        modifiers: Vec<u64>,
    },
}

pub struct Capture {
    id: WindowId,
    live: bool,
    source: ExtImageCaptureSourceV1,
    session: Option<ExtImageCopyCaptureSessionV1>,
    constraints: Constraints,
    buffers: Option<Buffers>,
    state: State,
    transform: Transform,
    /// Consecutive failed frames.
    failures: u32,
    /// At least one frame was handed to the overlay.
    have_frame: bool,
    /// dmabuf didn't work for this window; use shm.
    no_dmabuf: bool,
}

impl Capture {
    /// Capture the window `toplevel`.
    pub fn window(
        id: WindowId,
        toplevel: &ExtForeignToplevelHandleV1,
        g: &Globals,
        live: bool,
        cursors: bool,
        qh: &QueueHandle<App>,
    ) -> Self {
        let source = g.source_manager.create_source(toplevel, qh, Ignore);
        Self::new(id, source, g, live, cursors, qh)
    }

    fn new(
        id: WindowId,
        source: ExtImageCaptureSourceV1,
        g: &Globals,
        live: bool,
        cursors: bool,
        qh: &QueueHandle<App>,
    ) -> Self {
        let options = if cursors {
            Options::PaintCursors
        } else {
            Options::empty()
        };
        let session = g
            .copy_manager
            .create_session(&source, options, qh, SessionData(id));
        Self {
            id,
            live,
            source,
            session: Some(session),
            constraints: Constraints::default(),
            buffers: None,
            state: State::Starting,
            transform: Transform::Normal,
            failures: 0,
            have_frame: false,
            no_dmabuf: false,
        }
    }

    /// Buffer size announced by the compositor, if any.
    pub fn size(&self) -> Option<(u32, u32)> {
        let Constraints { width, height, .. } = self.constraints;
        (width > 0 && height > 0).then_some((width, height))
    }

    /// "dmabuf" or "shm", once buffers are allocated.
    pub fn buffer_kind(&self) -> Option<&'static str> {
        self.buffers.as_ref().map(|b| match b.kind {
            Kind::Shm(_) => "shm",
            Kind::Dmabuf { .. } => "dmabuf",
        })
    }

    fn give_up(&mut self, why: &'static str) -> Option<CaptureEvent> {
        if let State::InFlight { frame, .. } = std::mem::replace(&mut self.state, State::Done) {
            frame.destroy();
        }
        (!self.have_frame).then_some(CaptureEvent::Failed(why))
    }

    /// Stop using dmabufs for this window.
    fn drop_dmabuf(&mut self, why: &str) {
        eprintln!("wl-switcher: dmabuf capture failed ({why}), using shm");
        self.no_dmabuf = true;
        if self
            .buffers
            .as_ref()
            .is_some_and(|b| matches!(b.kind, Kind::Dmabuf { .. }))
        {
            self.buffers = None;
        }
    }

    /// (Re)allocate buffers for the latest constraints and request a frame.
    fn apply_constraints(&mut self, g: &Globals, qh: &QueueHandle<App>) -> Option<CaptureEvent> {
        let Some((width, height)) = self.size() else {
            return self.give_up("zero buffer size");
        };
        let dmabuf = if self.no_dmabuf {
            None
        } else {
            self.constraints.dmabuf_kind(g)
        };
        let shm = self.constraints.shm_format().map(Kind::Shm);
        for kind in [dmabuf, shm].into_iter().flatten() {
            if self
                .buffers
                .as_ref()
                .is_some_and(|b| b.matches(width, height, &kind))
            {
                return self.start(qh);
            }
            self.buffers = None;
            match Buffers::new(g, self.id, width, height, &kind, qh) {
                Ok(b) => {
                    self.buffers = Some(b);
                    return self.start(qh);
                }
                Err(e) => match kind {
                    Kind::Dmabuf { .. } => self.drop_dmabuf(&e.to_string()),
                    Kind::Shm(_) => {
                        eprintln!("wl-switcher: shm allocation failed: {e}");
                        return self.give_up("shm allocation failed");
                    }
                },
            }
        }
        self.give_up("no supported buffer format")
    }

    fn start(&mut self, qh: &QueueHandle<App>) -> Option<CaptureEvent> {
        self.state = State::Idle;
        self.request_frame(qh);
        None
    }

    fn request_frame(&mut self, qh: &QueueHandle<App>) {
        let (State::Idle, Some(session), Some(bufs)) = (&self.state, &self.session, &self.buffers)
        else {
            return;
        };
        if bufs.busy[bufs.back] {
            return; // resumed from buffer_released
        }
        self.transform = Transform::Normal;
        let frame = session.create_frame(qh, FrameData(self.id));
        frame.attach_buffer(&bufs.slots[bufs.back]);
        frame.damage_buffer(0, 0, bufs.width as i32, bufs.height as i32);
        frame.capture();
        self.state = State::InFlight {
            frame,
            realloc: false,
        };
    }

    /// Takes the finished in-flight frame, leaving the capture idle.
    fn finish_frame(&mut self) -> Option<bool> {
        match std::mem::replace(&mut self.state, State::Idle) {
            State::InFlight { frame, realloc } => {
                frame.destroy();
                Some(realloc)
            }
            other => {
                self.state = other;
                None
            }
        }
    }

    fn next_frame(
        &mut self,
        realloc: bool,
        g: &Globals,
        qh: &QueueHandle<App>,
    ) -> Option<CaptureEvent> {
        if realloc {
            self.apply_constraints(g, qh)
        } else {
            self.request_frame(qh);
            None
        }
    }

    pub fn session_event(
        &mut self,
        event: ext_image_copy_capture_session_v1::Event,
        g: &Globals,
        qh: &QueueHandle<App>,
    ) -> Option<CaptureEvent> {
        use ext_image_copy_capture_session_v1::Event;
        match event {
            Event::BufferSize { width, height } => {
                self.constraints.begin_event();
                self.constraints.width = width;
                self.constraints.height = height;
            }
            Event::ShmFormat { format } => {
                self.constraints.begin_event();
                if let WEnum::Value(format) = format {
                    self.constraints.shm_formats.push(format);
                }
            }
            Event::DmabufDevice { device } => {
                self.constraints.begin_event();
                self.constraints.dmabuf_device = dmabuf::parse_dev(&device);
            }
            Event::DmabufFormat { format, modifiers } => {
                self.constraints.begin_event();
                let modifiers = dmabuf::parse_modifiers(&modifiers);
                self.constraints.dmabuf_formats.push((format, modifiers));
            }
            Event::Done => {
                self.constraints.complete = true;
                match &mut self.state {
                    State::Done => {}
                    // The in-flight frame will fail or complete; reallocate after it.
                    State::InFlight { realloc, .. } | State::Paused { realloc } => *realloc = true,
                    State::Starting | State::Idle => return self.apply_constraints(g, qh),
                }
            }
            Event::Stopped => {
                let event = self.give_up("session stopped");
                if let Some(session) = self.session.take() {
                    session.destroy();
                }
                return event;
            }
            _ => {}
        }
        None
    }

    pub fn frame_event(
        &mut self,
        event: ext_image_copy_capture_frame_v1::Event,
        g: &Globals,
        qh: &QueueHandle<App>,
    ) -> Option<CaptureEvent> {
        use ext_image_copy_capture_frame_v1::Event;
        match event {
            Event::Transform {
                transform: WEnum::Value(transform),
            } => self.transform = transform,
            Event::Ready => {
                let realloc = self.finish_frame()?;
                self.failures = 0;
                self.have_frame = true;
                let bufs = self.buffers.as_mut()?;
                let slot = bufs.back;
                bufs.back ^= 1;
                bufs.busy[slot] = true; // attached by the overlay
                let frame = Frame {
                    buffer: bufs.slots[slot].clone(),
                    width: bufs.width,
                    height: bufs.height,
                    transform: self.transform,
                };
                if self.live {
                    // Can only fail after the first frame, so nothing to report.
                    let _ = self.next_frame(realloc, g, qh);
                } else {
                    self.state = State::Paused { realloc };
                }
                return Some(CaptureEvent::Frame(frame));
            }
            Event::Failed { reason } => {
                let realloc = self.finish_frame()?;
                if reason == WEnum::Value(FailureReason::Stopped) {
                    return self.give_up("stopped");
                }
                // A dmabuf that never worked: don't count it, retry with shm.
                if !self.have_frame && self.buffer_kind() == Some("dmabuf") {
                    self.drop_dmabuf(&format!("{reason:?}"));
                    return self.apply_constraints(g, qh);
                }
                self.failures += 1;
                if self.failures > MAX_FAILURES {
                    return self.give_up(match reason {
                        WEnum::Value(FailureReason::BufferConstraints) => "buffer constraints",
                        _ => "unknown",
                    });
                }
                // On buffer_constraints the session sends new constraints; if
                // they arrived already, this reallocates. Otherwise retry.
                return self.next_frame(realloc, g, qh);
            }
            _ => {}
        }
        None
    }

    pub fn buffer_released(&mut self, slot: usize, qh: &QueueHandle<App>) {
        if let Some(bufs) = &mut self.buffers {
            bufs.busy[slot] = false;
        }
        self.request_frame(qh); // in case the capture was waiting for this buffer
    }

    /// Whether `buffer` is one of this capture's current buffers. Frames
    /// handed out before a reallocation are gone and must not be attached.
    pub fn owns(&self, buffer: &WlBuffer) -> bool {
        (self.buffers.as_ref()).is_some_and(|b| b.slots.contains(buffer))
    }

    /// The overlay replaced a frame it never attached, so the compositor
    /// won't release it: it is free again.
    pub fn discard(&mut self, buffer: &WlBuffer, qh: &QueueHandle<App>) {
        let slot = (self.buffers.as_ref()).and_then(|b| b.slots.iter().position(|s| s == buffer));
        if let Some(slot) = slot {
            self.buffer_released(slot, qh);
        }
    }

    /// The overlay attached `buffer` (again): it is busy until released.
    pub fn attached(&mut self, buffer: &WlBuffer) {
        if let Some(bufs) = &mut self.buffers
            && let Some(slot) = bufs.slots.iter().position(|s| s == buffer)
        {
            bufs.busy[slot] = true;
        }
    }

    /// Capture continuously, or pause after the frame in flight. Resuming
    /// applies constraints that changed while paused.
    pub fn set_live(&mut self, live: bool, g: &Globals, qh: &QueueHandle<App>) {
        if self.live == live {
            return;
        }
        self.live = live;
        match self.state {
            State::Paused { realloc } if live => {
                self.state = State::Idle;
                // Past the first frame: nothing to report.
                let _ = self.next_frame(realloc, g, qh);
            }
            // Waiting for a buffer to be released.
            State::Idle if !live && self.have_frame => {
                self.state = State::Paused { realloc: false };
            }
            _ => {}
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        if let State::InFlight { frame, .. } = &self.state {
            frame.destroy();
        }
        if let Some(session) = &self.session {
            session.destroy();
        }
        self.source.destroy();
    }
}

/// What backs the two buffers; only held to keep it alive.
#[allow(dead_code)]
enum Storage {
    Shm(RawPool),
    Dmabuf([GpuBuffer; 2]),
}

/// Two equally sized buffers.
struct Buffers {
    slots: [WlBuffer; 2],
    busy: [bool; 2],
    /// The slot the next frame is captured into.
    back: usize,
    width: u32,
    height: u32,
    kind: Kind,
    /// Dropped after the wl_buffers are destroyed.
    _storage: Storage,
}

impl Buffers {
    fn new(
        g: &Globals,
        window: WindowId,
        width: u32,
        height: u32,
        kind: &Kind,
        qh: &QueueHandle<App>,
    ) -> std::io::Result<Self> {
        let (slots, storage) = match kind {
            Kind::Shm(format) => shm_buffers(&g.shm, window, width, height, *format, qh)?,
            Kind::Dmabuf {
                device,
                format,
                modifiers,
            } => {
                let dmabuf = g.dmabuf.as_ref().ok_or_else(|| {
                    std::io::Error::other("zwp_linux_dmabuf_v1 missing") // checked by dmabuf_kind
                })?;
                let gpu = dmabuf
                    .gpu(*device)
                    .ok_or_else(|| std::io::Error::other("no GBM device"))?;
                let alloc = |slot| {
                    let data = BufferData { window, slot };
                    dmabuf.allocate(&gpu, width, height, *format, modifiers, data, qh)
                };
                let (b0, g0) = alloc(0)?;
                let (b1, g1) = match alloc(1) {
                    Ok(b) => b,
                    Err(e) => {
                        b0.destroy();
                        return Err(e);
                    }
                };
                ([b0, b1], Storage::Dmabuf([g0, g1]))
            }
        };
        Ok(Self {
            slots,
            busy: [false; 2],
            back: 0,
            width,
            height,
            kind: kind.clone(),
            _storage: storage,
        })
    }

    fn matches(&self, width: u32, height: u32, kind: &Kind) -> bool {
        (self.width, self.height) == (width, height) && self.kind == *kind
    }
}

fn shm_buffers(
    shm: &Shm,
    window: WindowId,
    width: u32,
    height: u32,
    format: wl_shm::Format,
    qh: &QueueHandle<App>,
) -> std::io::Result<([WlBuffer; 2], Storage)> {
    let too_large = || std::io::Error::other("buffer too large");
    let stride = width.checked_mul(4).ok_or_else(too_large)?;
    let size = stride.checked_mul(height).ok_or_else(too_large)?;
    let pool_size = size
        .checked_mul(2)
        .filter(|&s| s <= i32::MAX as u32)
        .ok_or_else(too_large)?;
    let mut pool = RawPool::new(pool_size as usize, shm).map_err(std::io::Error::other)?;
    let slots = [0, 1].map(|slot| {
        let offset = (size * slot as u32) as i32;
        let data = BufferData { window, slot };
        pool.create_buffer(
            offset,
            width as i32,
            height as i32,
            stride as i32,
            format,
            data,
            qh,
        )
    });
    Ok((slots, Storage::Shm(pool)))
}

impl Drop for Buffers {
    fn drop(&mut self) {
        // Destroying a buffer that is still attached is fine: the compositor
        // keeps showing its last contents until the next attach.
        for buffer in &self.slots {
            buffer.destroy();
        }
    }
}

// ---------------------------------------------------------------------------
// protocol glue: route events to the window's capture

pub struct SessionData(WindowId);
pub struct FrameData(WindowId);
pub struct BufferData {
    window: WindowId,
    slot: usize,
}

impl Dispatch2<ExtImageCopyCaptureSessionV1, App> for SessionData {
    fn event(
        &self,
        app: &mut App,
        _: &ExtImageCopyCaptureSessionV1,
        event: ext_image_copy_capture_session_v1::Event,
        _: &Connection,
        qh: &QueueHandle<App>,
    ) {
        app.capture_update(self.0, |cap, g| cap.session_event(event, g, qh));
    }
}

impl Dispatch2<ExtImageCopyCaptureFrameV1, App> for FrameData {
    fn event(
        &self,
        app: &mut App,
        _: &ExtImageCopyCaptureFrameV1,
        event: ext_image_copy_capture_frame_v1::Event,
        _: &Connection,
        qh: &QueueHandle<App>,
    ) {
        app.capture_update(self.0, |cap, g| cap.frame_event(event, g, qh));
    }
}

impl Dispatch2<WlBuffer, App> for BufferData {
    fn event(
        &self,
        app: &mut App,
        _: &WlBuffer,
        _: wayland_client::protocol::wl_buffer::Event,
        _: &Connection,
        qh: &QueueHandle<App>,
    ) {
        // The only event is release.
        app.capture_update(self.window, |cap, _| {
            cap.buffer_released(self.slot, qh);
            None
        });
    }
}
