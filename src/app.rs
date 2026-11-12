//! Application state and the SCTK handlers that don't belong to input: the
//! daemon's long-lived Wayland state and caches, and the show in progress.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use smithay_client_toolkit::{
    compositor::CompositorHandler,
    delegate_dispatch2, delegate_registry,
    foreign_toplevel_list::{ForeignToplevelList, ForeignToplevelListHandler},
    output::{OutputHandler, OutputState},
    reexports::calloop::{
        LoopHandle, RegistrationToken, channel,
        timer::{TimeoutAction, Timer},
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{SeatState, pointer::cursor_shape::CursorShapeManager},
    shell::wlr_layer::{LayerShellHandler, LayerSurface, LayerSurfaceConfigure},
};
use wayland_client::{
    Connection, QueueHandle,
    globals::GlobalList,
    protocol::{
        wl_output::{self, WlOutput},
        wl_surface::WlSurface,
    },
};
use wayland_protocols::ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1;

use crate::cache::Cache;
use crate::capture::{Capture, CaptureEvent, WindowId};
use crate::config::Config;
use crate::daemon::Incoming;
use crate::globals::Globals;
use crate::icons;
use crate::input::Input;
use crate::ipc::Request;
use crate::mru::Mru;
use crate::overlay::{self, Overlay};
use crate::switcher::Window;
use crate::toplevel::{self, Toplevel};
use crate::worker::{Assets, Done};

pub enum Outcome {
    /// The identifier of the picked window.
    Picked(String),
    Cancelled,
}

pub struct App {
    registry: RegistryState,
    pub(crate) seat_state: SeatState,
    output_state: OutputState,
    pub toplevels: ForeignToplevelList,
    pub(crate) cursor_shape: Option<CursorShapeManager>,
    pub(crate) loop_handle: LoopHandle<'static, App>,
    pub(crate) input: Input,
    g: Rc<Globals>,
    qh: QueueHandle<App>,
    /// The config in use, set by the daemon before each show.
    pub config: Config,
    /// The worker thread, started with the daemon.
    assets: Option<Rc<Assets>>,
    /// Icons, labels and shapes, kept between shows.
    cache: Rc<RefCell<Cache>>,
    /// Focus order (phase 5 feeds it from the mru-event-stream).
    pub mru: Mru,
    /// The switcher, while a show is in progress.
    pub overlay: Option<Overlay>,
    /// The show-delay or cancel fade timer of the current show.
    timer: Option<RegistrationToken>,
    /// Set once the user picked or cancelled.
    pub outcome: Option<Outcome>,
    /// Client events for the daemon loop.
    pub(crate) inbox: Vec<Incoming>,
}

impl App {
    pub fn new(
        globals: &GlobalList,
        g: Rc<Globals>,
        qh: &QueueHandle<App>,
        loop_handle: LoopHandle<'static, App>,
    ) -> Self {
        Self {
            registry: RegistryState::new(globals),
            seat_state: SeatState::new(globals, qh),
            output_state: OutputState::new(globals, qh),
            toplevels: ForeignToplevelList::new(globals, qh),
            cursor_shape: CursorShapeManager::bind(globals, qh).ok(),
            loop_handle,
            input: Input::default(),
            g,
            qh: qh.clone(),
            config: Config::default(),
            assets: None,
            cache: Rc::new(RefCell::new(Cache::default())),
            mru: Mru::default(),
            overlay: None,
            timer: None,
            outcome: None,
            inbox: Vec::new(),
        }
    }

    /// The worker, started on first use.
    fn assets(&mut self) -> Result<Rc<Assets>, String> {
        if let Some(assets) = &self.assets {
            return Ok(assets.clone());
        }
        let (assets, results) =
            Assets::spawn().map_err(|e| format!("failed to start the worker: {e}"))?;
        self.loop_handle
            .insert_source(results, |event, _, app| {
                if let channel::Event::Msg(done) = event {
                    app.worker_done(done);
                }
            })
            .map_err(|e| format!("failed to watch the worker: {e}"))?;
        let assets = Rc::new(assets);
        self.assets = Some(assets.clone());
        Ok(assets)
    }

    /// A worker result: into the cache, then to the overlay.
    fn worker_done(&mut self, done: Done) {
        if let Some(assets) = &self.assets {
            assets.received(&done);
        }
        match done {
            Done::Icon { key, image } => {
                self.cache
                    .borrow_mut()
                    .insert_icon(&self.g, key.clone(), &image, &self.qh);
                if let Some(overlay) = &mut self.overlay {
                    overlay.icon_ready(&key);
                }
            }
            Done::Label { key, pill } => {
                self.cache
                    .borrow_mut()
                    .insert_label(&self.g, key.clone(), &pill, &self.qh);
                if let Some(overlay) = &mut self.overlay {
                    overlay.label_ready(&key);
                }
            }
            Done::IconsReloaded => {
                self.cache.borrow_mut().clear_icons();
                match &mut self.overlay {
                    Some(overlay) => overlay.icons_reloaded(),
                    None => self.prefetch(),
                }
            }
            Done::Pill { tag, request, pill } => {
                if let Some(overlay) = &mut self.overlay {
                    overlay.pill_ready(tag, request, pill);
                }
            }
        }
    }

    /// The config changed: forget what was rendered with the old one.
    pub fn config_changed(&mut self) {
        let mut cache = self.cache.borrow_mut();
        cache.clear_icons();
        cache.clear_shapes();
        if let Some(assets) = &self.assets {
            assets.clear_icons();
        }
    }

    fn scales(&self) -> Vec<i32> {
        let mut scales: Vec<i32> = (self.output_state.outputs())
            .filter_map(|o| self.output_state.info(&o))
            .map(|info| info.scale_factor.max(1))
            .collect();
        scales.sort_unstable();
        scales.dedup();
        if scales.is_empty() {
            scales.push(1);
        }
        scales
    }

    /// Have the worker render the icon and label of every open window's app
    /// at every output scale, so the next show has them on its first frame.
    pub fn prefetch(&mut self) {
        let assets = match self.assets() {
            Ok(assets) => assets,
            Err(e) => return eprintln!("wl-switcher: {e}"),
        };
        let config = &self.config;
        let cache = self.cache.borrow();
        let mut app_ids: Vec<String> = (self.toplevels())
            .into_iter()
            .map(|(_, t)| t.app_id)
            .collect();
        app_ids.sort_unstable();
        app_ids.dedup();
        for scale in self.scales() {
            for app_id in &app_ids {
                let key = icons::Key::new(app_id, config, scale);
                if cache.icon(&key).is_none() {
                    assets.icon(&key, &config.font);
                }
                if let Some(key) = overlay::label_key(app_id, config, scale)
                    && cache.label(&key).is_none()
                {
                    assets.label(&key);
                }
            }
        }
    }

    /// A window appeared or changed its app: get its icon ready, unless a
    /// show is on (`after_show` catches up) or the daemon is still starting.
    fn toplevel_changed(&mut self) {
        if self.overlay.is_none() && self.assets.is_some() {
            self.prefetch();
        }
    }

    /// Open toplevels with their state, in announcement order.
    pub fn toplevels(&self) -> Vec<(ExtForeignToplevelHandleV1, Toplevel)> {
        self.toplevels
            .toplevels()
            .iter()
            .filter_map(|handle| {
                let info = self.toplevels.info(handle)?;
                let toplevel = Toplevel {
                    identifier: info.identifier,
                    app_id: info.app_id,
                    title: info.title,
                };
                Some((handle.clone(), toplevel))
            })
            .collect()
    }

    /// The output named `name` (None: the compositor's choice) and the
    /// scale to start with: the output's, or the largest.
    fn output(&self, name: Option<&str>, warn: &mut dyn FnMut(String)) -> (Option<WlOutput>, i32) {
        let largest = self.scales().last().copied().unwrap_or(1);
        let Some(name) = name else {
            return (None, largest);
        };
        let found = (self.output_state.outputs()).find_map(|o| {
            let info = self.output_state.info(&o)?;
            (info.name.as_deref() == Some(name)).then(|| (o, info.scale_factor.max(1)))
        });
        match found {
            Some((output, scale)) => (Some(output), scale),
            None => {
                warn(format!("no output named '{name}'"));
                (None, largest)
            }
        }
    }

    /// Open the switcher for `req` and start its show-delay timer.
    /// Warnings about the request go to `warn`.
    pub fn show(&mut self, req: &Request, warn: &mut dyn FnMut(String)) -> Result<(), String> {
        let (handles, toplevels): (Vec<_>, Vec<_>) = self.toplevels().into_iter().unzip();
        let shown = toplevel::shown(&toplevels, &req.ids, &self.mru, |id| {
            warn(format!("no window with ID '{id}'"));
        });
        if shown.is_empty() {
            return Err("no windows to show".into());
        }
        let windows: Vec<_> = (shown.iter())
            .map(|&i| {
                let t = &toplevels[i];
                let window = Window {
                    id: t.identifier.clone(),
                    app_id: t.app_id.clone(),
                    title: t.title.clone(),
                };
                (handles[i].clone(), window)
            })
            .collect();
        let (output, scale) = self.output(req.output.as_deref(), warn);
        let assets = self.assets()?;
        let mut overlay = Overlay::new(
            self.g.clone(),
            windows,
            req.step,
            output.as_ref(),
            scale,
            &self.config,
            self.cache.clone(),
            assets,
            &self.qh,
        )
        .map_err(|e| format!("failed to create the switcher: {e}"))?;
        overlay.inhibit_shortcuts(self.seat_state.seats());
        self.overlay = Some(overlay);
        self.outcome = None;

        let delay = Duration::from_millis(self.config.show_delay.into());
        self.set_timer(delay, |app| {
            if let Some(overlay) = &mut app.overlay {
                overlay.reveal();
            }
        })
    }

    fn set_timer(
        &mut self,
        after: Duration,
        f: impl FnOnce(&mut App) + 'static,
    ) -> Result<(), String> {
        if let Some(token) = self.timer.take() {
            self.loop_handle.remove(token);
        }
        let mut f = Some(f);
        let token = self
            .loop_handle
            .insert_source(Timer::from_duration(after), move |_, _, app| {
                app.timer = None;
                if let Some(f) = f.take() {
                    f(app);
                }
                TimeoutAction::Drop
            })
            .map_err(|e| format!("failed to start a timer: {e}"))?;
        self.timer = Some(token);
        Ok(())
    }

    /// Destroy the switcher. The caller should roundtrip before acting on
    /// the outcome, so it is gone by then.
    pub fn end_show(&mut self) {
        self.overlay = None;
        if let Some(token) = self.timer.take() {
            self.loop_handle.remove(token);
        }
        self.input.reset();
    }

    /// After the roundtrip that follows `end_show`: buffers no longer
    /// attached anywhere can go.
    pub fn after_show(&mut self) {
        self.cache.borrow_mut().trim();
        // Desktop entries installed since, and windows opened since, are
        // ready next time.
        if let Some(assets) = &self.assets {
            assets.refresh();
        }
        self.prefetch();
    }

    /// The first outcome wins.
    pub fn finish(&mut self, outcome: Outcome) {
        self.outcome.get_or_insert(outcome);
    }

    /// Pick the selection: close at once. Not once a cancel is fading out
    /// (releasing Alt after Alt+Escape).
    pub fn pick(&mut self) {
        let Some(overlay) = self.overlay.as_ref().filter(|o| !o.closing()) else {
            return;
        };
        let id = overlay.switcher.picked().id.clone();
        self.finish(Outcome::Picked(id));
    }

    /// Close without picking, after a short fade if visible.
    pub fn cancel(&mut self) {
        if self.outcome.is_some() {
            return;
        }
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        if !overlay.start_close() {
            return self.finish(Outcome::Cancelled);
        }
        let fade = Duration::from_millis(overlay::CLOSE_MS);
        if self
            .set_timer(fade, |app| app.finish(Outcome::Cancelled))
            .is_err()
        {
            self.finish(Outcome::Cancelled);
        }
    }

    /// Captures arrive with the strip (phase 3).
    pub fn capture_update(
        &mut self,
        _: WindowId,
        _: impl FnOnce(&mut Capture, &Globals) -> Option<CaptureEvent>,
    ) {
    }
}

impl ForeignToplevelListHandler for App {
    fn foreign_toplevel_list_state(&mut self) -> &mut ForeignToplevelList {
        &mut self.toplevels
    }

    fn new_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: ExtForeignToplevelHandleV1,
    ) {
        self.toplevel_changed();
    }

    fn update_toplevel(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: ExtForeignToplevelHandleV1,
    ) {
        self.toplevel_changed();
    }

    fn toplevel_closed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        handle: ExtForeignToplevelHandleV1,
    ) {
        let identifier = self.toplevels.info(&handle).map(|i| i.identifier);
        if let Some(id) = &identifier {
            self.mru.forget(id);
        }
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        let Some(id) = overlay.window_of(&handle).map(str::to_string) else {
            return;
        };
        if !overlay.remove(&id) {
            eprintln!("wl-switcher: all windows closed");
            self.finish(Outcome::Cancelled);
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.finish(Outcome::Cancelled);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if let Some(overlay) = &mut self.overlay {
            let (width, height) = configure.new_size;
            overlay.configure(width, height);
        }
    }
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &WlSurface,
        factor: i32,
    ) {
        if let Some(overlay) = &mut self.overlay
            && overlay.surface() == surface
        {
            overlay.set_scale(factor);
        }
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &WlSurface, _: u32) {
        if let Some(overlay) = &mut self.overlay
            && overlay.surface() == surface
        {
            overlay.frame_done();
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }

    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
delegate_dispatch2!(App);
