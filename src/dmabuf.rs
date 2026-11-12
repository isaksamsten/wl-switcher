//! dmabuf capture buffers: the compositor copies windows on the GPU instead
//! of reading them back into shm.
//!
//! zwp_linux_dmabuf_v1 is bound at version 3 for its format/modifier table,
//! so buffers are only created for pairs the compositor accepts (a rejected
//! `create_immed` would be a protocol error). Buffers are allocated with GBM
//! on the render node of the device each capture session asks for.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::File;
use std::os::fd::AsFd;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use drm_fourcc::{DrmFourcc, DrmModifier};
use gbm::{BufferObject, BufferObjectFlags};
use smithay_client_toolkit::dispatch2::Dispatch2;
use wayland_client::{Connection, QueueHandle, globals::GlobalList, protocol::wl_buffer::WlBuffer};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::Flags,
    zwp_linux_dmabuf_v1::{self, ZwpLinuxDmabufV1},
};

use crate::app::App;
use crate::capture::BufferData;
use crate::globals::Ignore;

/// DRM_FORMAT_MOD_INVALID: the driver picks the layout implicitly.
pub const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Capture formats we can show, in order of preference.
pub const FORMATS: [DrmFourcc; 4] = [
    DrmFourcc::Argb8888,
    DrmFourcc::Xrgb8888,
    DrmFourcc::Abgr8888,
    DrmFourcc::Xbgr8888,
];

/// (format, modifier) pairs the compositor accepts.
type Table = Arc<Mutex<HashSet<(u32, u64)>>>;

pub struct TableData(Table);

impl Dispatch2<ZwpLinuxDmabufV1, App> for TableData {
    fn event(
        &self,
        _: &mut App,
        _: &ZwpLinuxDmabufV1,
        event: zwp_linux_dmabuf_v1::Event,
        _: &Connection,
        _: &QueueHandle<App>,
    ) {
        // Bound at version 3: `modifier` events are authoritative, including
        // MOD_INVALID for implicit layouts. `format` events are also sent but
        // don't say which modifiers work, so they are ignored.
        if let zwp_linux_dmabuf_v1::Event::Modifier {
            format,
            modifier_hi,
            modifier_lo,
        } = event
        {
            let modifier = (modifier_hi as u64) << 32 | modifier_lo as u64;
            self.0.lock().unwrap().insert((format, modifier));
        }
    }
}

pub struct Gpu {
    device: gbm::Device<File>,
}

pub struct Dmabuf {
    global: ZwpLinuxDmabufV1,
    table: Table,
    /// Opened (or failed) devices by dev_t.
    gpus: RefCell<Vec<(u64, Option<Rc<Gpu>>)>>,
}

/// A buffer object and the device it lives on. The object is dropped
/// before the device's fd closes.
pub struct GpuBuffer {
    _bo: BufferObject<()>,
    _gpu: Rc<Gpu>,
}

impl Dmabuf {
    /// Optional: without it (or below version 3), captures use shm.
    pub fn bind(globals: &GlobalList, qh: &QueueHandle<App>) -> Option<Self> {
        let table = Table::default();
        let global = globals.bind(qh, 3..=3, TableData(table.clone())).ok()?;
        Some(Self {
            global,
            table,
            gpus: RefCell::new(Vec::new()),
        })
    }

    /// Modifiers of `format` the compositor accepts, among `offered`.
    pub fn accepted(&self, format: u32, offered: &[u64]) -> Vec<u64> {
        let table = self.table.lock().unwrap();
        offered
            .iter()
            .copied()
            .filter(|&m| table.contains(&(format, m)))
            .collect()
    }

    /// The GBM device for `dev`, opened on first use.
    pub fn gpu(&self, dev: u64) -> Option<Rc<Gpu>> {
        let mut gpus = self.gpus.borrow_mut();
        if let Some((_, gpu)) = gpus.iter().find(|(d, _)| *d == dev) {
            return gpu.clone();
        }
        let gpu = open_gpu(dev)
            .inspect_err(|e| eprintln!("wl-switcher: dmabuf unavailable, using shm: {e}"))
            .ok()
            .map(Rc::new);
        gpus.push((dev, gpu.clone()));
        gpu
    }

    /// Allocate a `width`x`height` buffer of `format` with one of
    /// `modifiers` (all accepted by the compositor) and wrap it in a
    /// wl_buffer.
    #[allow(clippy::too_many_arguments)]
    pub fn allocate(
        &self,
        gpu: &Rc<Gpu>,
        width: u32,
        height: u32,
        format: u32,
        modifiers: &[u64],
        data: BufferData,
        qh: &QueueHandle<App>,
    ) -> std::io::Result<(WlBuffer, GpuBuffer)> {
        let fourcc = DrmFourcc::try_from(format)
            .map_err(|_| std::io::Error::other(format!("unknown format {format:#x}")))?;
        let bo = create_bo(&gpu.device, width, height, fourcc, modifiers)?;
        let modifier = if modifiers == [MOD_INVALID] {
            MOD_INVALID
        } else {
            u64::from(bo.modifier())
        };

        let params = self.global.create_params(qh, Ignore);
        for plane in 0..bo.plane_count() as i32 {
            let fd = bo.fd_for_plane(plane).map_err(std::io::Error::other)?;
            params.add(
                fd.as_fd(), // duplicated when the request is queued
                plane as u32,
                bo.offset(plane),
                bo.stride_for_plane(plane),
                (modifier >> 32) as u32,
                modifier as u32,
            );
        }
        let buffer = params.create_immed(
            width as i32,
            height as i32,
            format,
            Flags::empty(),
            qh,
            data,
        );
        params.destroy();
        Ok((
            buffer,
            GpuBuffer {
                _bo: bo,
                _gpu: gpu.clone(),
            },
        ))
    }
}

fn create_bo(
    device: &gbm::Device<File>,
    width: u32,
    height: u32,
    format: DrmFourcc,
    modifiers: &[u64],
) -> std::io::Result<BufferObject<()>> {
    let explicit: Vec<DrmModifier> = modifiers
        .iter()
        .filter(|&&m| m != MOD_INVALID)
        .map(|&m| DrmModifier::from(m))
        .collect();
    let mut last_err = None;
    if !explicit.is_empty() {
        let with_flags = device.create_buffer_object_with_modifiers2(
            width,
            height,
            format,
            explicit.iter().copied(),
            BufferObjectFlags::RENDERING,
        );
        // Older GBM lacks the variant with usage flags.
        let result = with_flags.or_else(|_| {
            device.create_buffer_object_with_modifiers(
                width,
                height,
                format,
                explicit.iter().copied(),
            )
        });
        match result {
            Ok(bo) => return Ok(bo),
            Err(e) => last_err = Some(e),
        }
    }
    if modifiers.contains(&MOD_INVALID) {
        return device.create_buffer_object(width, height, format, BufferObjectFlags::RENDERING);
    }
    Err(last_err.unwrap_or_else(|| std::io::Error::other("no usable modifier")))
}

fn open_gpu(dev: u64) -> std::io::Result<Gpu> {
    let path = render_node(dev)?;
    let file = File::options().read(true).write(true).open(&path)?;
    let device = gbm::Device::new(file)
        .map_err(|e| std::io::Error::other(format!("{}: {e}", path.display())))?;
    Ok(Gpu { device })
}

/// The render node of the DRM device `dev` (which may be a primary node).
fn render_node(dev: u64) -> std::io::Result<PathBuf> {
    let (major, minor) = major_minor(dev);
    let dir = format!("/sys/dev/char/{major}:{minor}/device/drm");
    for entry in std::fs::read_dir(&dir)? {
        let name = entry?.file_name();
        if name.to_string_lossy().starts_with("renderD") {
            return Ok(PathBuf::from("/dev/dri").join(name));
        }
    }
    Err(std::io::Error::other(format!("no render node in {dir}")))
}

/// glibc's dev_t encoding (gnu_dev_major/gnu_dev_minor); both are 32-bit.
fn major_minor(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & 0xffff_f000);
    let minor = (dev & 0xff) | ((dev >> 12) & 0xffff_ff00);
    (major, minor)
}

/// The dev_t of a `dmabuf_device` event.
pub fn parse_dev(bytes: &[u8]) -> Option<u64> {
    Some(u64::from_ne_bytes(bytes.try_into().ok()?))
}

/// The modifiers of a `dmabuf_format` event.
pub fn parse_modifiers(bytes: &[u8]) -> Vec<u64> {
    bytes
        .as_chunks::<8>()
        .0
        .iter()
        .map(|&c| u64::from_ne_bytes(c))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// glibc's makedev, the inverse of major_minor.
    fn makedev(major: u64, minor: u64) -> u64 {
        (major & 0xfff) << 8
            | (major & 0xffff_f000) << 32
            | (minor & 0xff)
            | (minor & 0xffff_ff00) << 12
    }

    #[test]
    fn dev_t() {
        // /dev/dri/renderD128
        assert_eq!(major_minor(0xe280), (226, 128));
        for (major, minor) in [
            (226, 0),
            (226, 128),
            (0x1234, 0x56789),
            (0xffff_ffff, 0xfff_ffff),
        ] {
            assert_eq!(major_minor(makedev(major, minor)), (major, minor));
        }
        assert_eq!(parse_dev(&0xe280u64.to_ne_bytes()), Some(0xe280));
        assert_eq!(parse_dev(&[1, 2, 3]), None);
    }

    #[test]
    fn modifiers() {
        let bytes: Vec<u8> = [0u64, MOD_INVALID]
            .iter()
            .flat_map(|m| m.to_ne_bytes())
            .collect();
        assert_eq!(parse_modifiers(&bytes), [0, MOD_INVALID]);
        assert!(parse_modifiers(&[0; 7]).is_empty());
    }
}
