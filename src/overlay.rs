//! The switcher's layer surface and its scene.
//!
//! A fullscreen overlay-layer surface with exclusive keyboard focus. It is
//! mapped right away, with a transparent buffer, because it needs keyboard
//! focus to see the modifier being released. Nothing else is attached until
//! `show-delay` has passed (`reveal`), so a quick Alt+Tab never shows a
//! frame.
//!
//! Everything is a subsurface scaled with a viewport, in stacking order:
//! the dim tint, the panel (with its shadow), the highlight, one icon and
//! one dot row per app, and two labels that crossfade. The subsurfaces are
//! sync: each frame repositions and re-alphas them, then commits the parent
//! once, so the frame updates atomically.
//!
//! Movement runs on springs (anim.rs), advanced on frame callbacks by the
//! time measured since the last one. Input only retargets springs and
//! requests a frame; once everything has settled, no more frames are
//! requested, so an idle switcher costs nothing.
//!
//! Phase 2 is apps mode only. In windows mode (Alt+`) the bar stays, and
//! the label shows the selected window's title; the strip is phase 3.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use smithay_client_toolkit::{
    compositor::{FrameCallbackData, Region},
    shell::{
        WaylandSurface,
        wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerSurface},
    },
};
use wayland_client::{
    QueueHandle,
    protocol::{
        wl_buffer::WlBuffer, wl_output::WlOutput, wl_seat::WlSeat, wl_subsurface::WlSubsurface,
        wl_surface::WlSurface,
    },
};
use wayland_protocols::{
    ext::foreign_toplevel_list::v1::client::ext_foreign_toplevel_handle_v1::ExtForeignToplevelHandleV1,
    wp::{
        alpha_modifier::v1::client::wp_alpha_modifier_surface_v1::WpAlphaModifierSurfaceV1,
        keyboard_shortcuts_inhibit::zv1::client::zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
        viewporter::client::wp_viewport::WpViewport,
    },
};

use crate::anim::{Spring, lerp};
use crate::app::App;
use crate::bar::{self, Layout};
use crate::cache::{self, Cache, Shape, ShapeKey};
use crate::config::Config;
use crate::globals::{Globals, Ignore};
use crate::icons;
use crate::shadow;
use crate::switcher::{Mode, Step, Switcher, Window};
use crate::text;
use crate::worker::{Assets, LabelKey, PillRequest};

/// The bar's scale when it starts fading in.
const OPEN_SCALE: f64 = 0.92;
/// Spring responses, ms.
const OPEN_RESPONSE: f64 = 120.0;
pub const CLOSE_MS: u64 = 100;
/// Dots shown under an icon at most.
const MAX_DOTS: usize = 3;

/// The widest a label gets, logical pixels.
pub fn label_max_width(config: &Config) -> i32 {
    (config.icon_size as i32 * 3).max(200)
}

/// The label of `app_id` at `scale`; None with labels disabled.
pub fn label_key(app_id: &str, config: &Config, scale: i32) -> Option<LabelKey> {
    Some(LabelKey {
        app_id: app_id.to_string(),
        max_width: label_max_width(config),
        scale,
        style: config.label_style()?,
    })
}

/// A rect in surface-local logical pixels, as the protocol takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IRect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

impl IRect {
    /// Round the edges, not the size, so neighbors never overlap or part.
    fn round(r: bar::Rect) -> Self {
        let (x0, y0) = (r.x.round() as i32, r.y.round() as i32);
        let (x1, y1) = (r.right().round() as i32, r.bottom().round() as i32);
        Self {
            x: x0,
            y: y0,
            w: (x1 - x0).max(1),
            h: (y1 - y0).max(1),
        }
    }
}

/// A sync subsurface scaled with a viewport, without input, faded with
/// wp_alpha_modifier_v1 if the compositor has it.
struct Sub {
    surface: WlSurface,
    subsurface: WlSubsurface,
    viewport: WpViewport,
    alpha: Option<WpAlphaModifierSurfaceV1>,
    attached: Option<WlBuffer>,
    /// The multiplier last set.
    shown_alpha: f64,
    cropped: bool,
}

impl Sub {
    fn new(g: &Globals, parent: &WlSurface, no_input: &Region, qh: &QueueHandle<App>) -> Self {
        // Sync, the default: changes apply with the parent's commit.
        let (subsurface, surface) = g.subcompositor.create_subsurface(parent.clone(), qh);
        let viewport = g.viewporter.get_viewport(&surface, qh, Ignore);
        let alpha = (g.alpha_modifier.as_ref()).map(|m| m.get_surface(&surface, qh, Ignore));
        surface.set_input_region(Some(no_input.wl_region()));
        Self {
            surface,
            subsurface,
            viewport,
            alpha,
            attached: None,
            shown_alpha: 1.0,
            cropped: false,
        }
    }

    fn show(&mut self, buffer: &WlBuffer) {
        if self.attached.as_ref() != Some(buffer) {
            self.surface.attach(Some(buffer), 0, 0);
            self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
            self.attached = Some(buffer.clone());
        }
    }

    fn hide(&mut self) {
        if self.attached.take().is_some() {
            self.surface.attach(None, 0, 0);
        }
    }

    fn place(&self, r: IRect) {
        self.subsurface.set_position(r.x, r.y);
        self.viewport.set_destination(r.w, r.h);
    }

    /// Show only `src` (x, y, w, h in buffer pixels) of the buffer, or all
    /// of it.
    fn crop(&mut self, src: Option<[f64; 4]>) {
        match src {
            Some([x, y, w, h]) => {
                self.viewport.set_source(x, y, w, h);
                self.cropped = true;
            }
            None if self.cropped => {
                self.viewport.set_source(-1.0, -1.0, -1.0, -1.0);
                self.cropped = false;
            }
            None => {}
        }
    }

    fn set_alpha(&mut self, a: f64) {
        let a = a.clamp(0.0, 1.0);
        if let Some(alpha) = &self.alpha
            && a != self.shown_alpha
        {
            alpha.set_multiplier((a * u32::MAX as f64).round() as u32);
            self.shown_alpha = a;
        }
    }

    fn commit(&self) {
        self.surface.commit();
    }
}

impl Drop for Sub {
    fn drop(&mut self) {
        // Before the surface, or it is a protocol error.
        if let Some(alpha) = self.alpha.take() {
            alpha.destroy();
        }
        self.viewport.destroy();
        self.subsurface.destroy();
        self.surface.destroy();
    }
}

/// What a label shows.
#[derive(Clone, Debug, PartialEq)]
enum LabelWant {
    App(LabelKey),
    /// A window title, requested from the worker under `tag`.
    Title(u64, PillRequest),
}

struct ShownLabel {
    want: LabelWant,
    buffer: WlBuffer,
    width: i32,
    height: i32,
}

struct Style {
    icon_size: u32,
    gap: f64,
    max_width: f64,
    position: f64,
    /// None: no label (and no room for one).
    label: Option<text::Style>,
    /// The selected window's title, shown in the label's place.
    title: text::Style,
    label_max_width: i32,
    dots: bool,
    panel_color: u32,
    highlight_color: u32,
    dot_color: u32,
    shadow: Option<shadow::Settings>,
    icon_theme: Option<String>,
    font: String,
}

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    App(usize),
    /// The panel, between icons.
    Panel,
    Outside,
}

pub struct Overlay {
    // Field order is drop order: children before their parent surface.
    dim: Option<Sub>,
    panel: Sub,
    highlight: Sub,
    /// Per app.
    icons: Vec<Sub>,
    dots: Vec<Sub>,
    /// Two, to crossfade; `front` is the current one.
    labels: [Sub; 2],
    no_input: Region,
    viewport: WpViewport,
    layer: LayerSurface,
    transparent: WlBuffer,
    dim_buffer: Option<WlBuffer>,
    /// Title pills made during this show; any may still be attached.
    title_buffers: Vec<WlBuffer>,
    /// One per seat; destroyed before the surface.
    inhibitors: Vec<ZwpKeyboardShortcutsInhibitorV1>,

    pub switcher: Switcher,
    /// Each shown window's handle and ext identifier.
    handles: Vec<(ExtForeignToplevelHandleV1, String)>,
    cache: Rc<RefCell<Cache>>,
    assets: Rc<Assets>,
    g: Rc<Globals>,
    qh: QueueHandle<App>,
    style: Style,

    width: u32,
    height: u32,
    scale: i32,
    configured: bool,
    /// Past show-delay: content is attached and fades in.
    visible: bool,
    /// `reveal` came before the first configure.
    reveal_wanted: bool,
    layout: Option<Layout>,
    panel_shape: Option<Shape>,
    highlight_shape: Option<Shape>,
    /// Per app; None for apps with one window.
    dot_shapes: Vec<Option<Shape>>,
    shown_labels: [Option<ShownLabel>; 2],
    front: usize,
    /// The label the selection wants, until it arrives.
    label_wanted: Option<LabelWant>,
    next_tag: u64,

    /// Fading and scaling in, 0 to 1.
    open: Spring,
    /// Fading out on cancel, 1 to 0.
    close: Spring,
    /// The highlight's left edge, in unscrolled row coordinates.
    highlight_x: Spring,
    scroll: Spring,
    /// The front label's alpha; the back one has the rest.
    label_fade: Spring,
    frame_pending: bool,
    last_frame: Option<Instant>,
}

impl Overlay {
    /// Create the switcher for `windows` (MRU order, not empty), opened
    /// with `step`, on `output` (None: the compositor's choice) at a
    /// guessed `scale`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        g: Rc<Globals>,
        windows: Vec<(ExtForeignToplevelHandleV1, Window)>,
        step: Step,
        output: Option<&WlOutput>,
        scale: i32,
        config: &Config,
        cache: Rc<RefCell<Cache>>,
        assets: Rc<Assets>,
        qh: &QueueHandle<App>,
    ) -> std::io::Result<Self> {
        let handles = (windows.iter())
            .map(|(h, w)| (h.clone(), w.id.clone()))
            .collect();
        let switcher = Switcher::open(windows.into_iter().map(|(_, w)| w).collect(), step)
            .ok_or_else(|| std::io::Error::other("no windows"))?;
        let theme = config.theme();
        let transparent = cache::color_buffer(&g, 0, qh)?;
        let dim_buffer = (config.dim > 0.0)
            .then(|| cache::color_buffer(&g, (config.dim * 255.0).round() as u32, qh))
            .transpose()?;
        let no_input = Region::new(&g.compositor).map_err(std::io::Error::other)?;

        let surface = g.compositor.create_surface(qh);
        let viewport = g.viewporter.get_viewport(&surface, qh, Ignore);
        let layer = g.layer_shell.create_layer_surface(
            qh,
            surface,
            Layer::Overlay,
            Some("wl-switcher"),
            output,
        );
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);

        // Stacking is creation order.
        let parent = layer.wl_surface();
        let sub = || Sub::new(&g, parent, &no_input, qh);
        let dim = dim_buffer.is_some().then(sub);
        let panel = sub();
        let highlight = sub();
        let apps = switcher.apps().len();
        let icons = (0..apps).map(|_| sub()).collect();
        let dots = (0..apps).map(|_| sub()).collect();
        let labels = [sub(), sub()];

        // Initial commit without a buffer; mapped on configure.
        layer.commit();

        let response = config.response as f64;
        Ok(Self {
            dim,
            panel,
            highlight,
            icons,
            dots,
            labels,
            no_input,
            viewport,
            layer,
            transparent,
            dim_buffer,
            title_buffers: Vec::new(),
            inhibitors: Vec::new(),
            switcher,
            handles,
            cache,
            assets,
            g,
            qh: qh.clone(),
            style: Style {
                icon_size: config.icon_size,
                gap: config.icon_gap as f64,
                max_width: config.max_width,
                position: config.bar_position,
                label: config.label_style(),
                title: config.title_style(),
                label_max_width: label_max_width(config),
                dots: config.window_dots,
                panel_color: theme.panel_color,
                highlight_color: theme.highlight_color,
                dot_color: theme.dot_color,
                shadow: config.shadow(),
                icon_theme: theme.icon_theme.clone(),
                font: config.font.clone(),
            },
            width: 0,
            height: 0,
            scale: scale.max(1),
            configured: false,
            visible: false,
            reveal_wanted: false,
            layout: None,
            panel_shape: None,
            highlight_shape: None,
            dot_shapes: Vec::new(),
            shown_labels: [None, None],
            front: 0,
            label_wanted: None,
            next_tag: 0,
            open: Spring::new(0.0, OPEN_RESPONSE),
            close: Spring::new(1.0, CLOSE_MS as f64),
            highlight_x: Spring::new(0.0, response),
            scroll: Spring::new(0.0, response),
            label_fade: Spring::new(1.0, response),
            frame_pending: false,
            last_frame: None,
        })
    }

    pub fn surface(&self) -> &WlSurface {
        self.layer.wl_surface()
    }

    /// Ask the compositor to send its own bindings (Alt+Tab itself) to the
    /// switcher while it has the keyboard. Without the protocol, or if the
    /// compositor declines, they keep firing.
    pub fn inhibit_shortcuts(&mut self, seats: impl IntoIterator<Item = WlSeat>) {
        let Some(manager) = &self.g.shortcuts_inhibit else {
            return;
        };
        let surface = self.layer.wl_surface();
        self.inhibitors.extend(
            seats
                .into_iter()
                .map(|seat| manager.inhibit_shortcuts(surface, &seat, &self.qh, Ignore)),
        );
    }

    pub fn visible(&self) -> bool {
        self.visible
    }

    /// The ext identifier of the window with `handle`, if shown.
    pub fn window_of(&self, handle: &ExtForeignToplevelHandleV1) -> Option<&str> {
        (self.handles.iter())
            .find(|(h, _)| h == handle)
            .map(|(_, id)| id.as_str())
    }

    pub fn configure(&mut self, width: u32, height: u32) {
        let first = !self.configured;
        let resized = (width, height) != (self.width, self.height);
        self.configured = true;
        (self.width, self.height) = (width, height);
        if width > 0 && height > 0 {
            self.viewport.set_destination(width as i32, height as i32);
        }
        if first {
            // Map, invisibly.
            let surface = self.layer.wl_surface();
            surface.attach(Some(&self.transparent), 0, 0);
            surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        }
        if first || resized {
            self.relayout();
        }
        if self.reveal_wanted && !self.visible {
            self.reveal();
        } else if self.visible {
            self.render();
        } else {
            self.layer.commit();
        }
    }

    pub fn set_scale(&mut self, scale: i32) {
        if scale < 1 || scale == self.scale {
            return;
        }
        self.scale = scale;
        self.relayout();
        self.update_label();
        self.kick();
    }

    fn icon_key(&self, app_id: &str) -> icons::Key {
        icons::Key {
            app_id: app_id.to_string(),
            size: self.style.icon_size * self.scale as u32,
            theme: self.style.icon_theme.clone(),
        }
    }

    fn label_key(&self, app_id: &str) -> Option<LabelKey> {
        Some(LabelKey {
            app_id: app_id.to_string(),
            max_width: self.style.label_max_width,
            scale: self.scale,
            style: self.style.label.clone()?,
        })
    }

    /// Ask the worker for the icons and labels not in the cache.
    fn request_assets(&self) {
        let cache = self.cache.borrow();
        for app in self.switcher.apps() {
            let key = self.icon_key(&app.app_id);
            if cache.icon(&key).is_none() {
                self.assets.icon(&key, &self.style.font);
            }
            if let Some(key) = self.label_key(&app.app_id)
                && cache.label(&key).is_none()
            {
                self.assets.label(&key);
            }
        }
    }

    /// Lay the bar out for the output size, scale and apps, and get the
    /// shapes for it.
    fn relayout(&mut self) {
        if !self.configured || self.width == 0 || self.height == 0 {
            return;
        }
        self.request_assets();
        let apps = self.switcher.apps();
        let label = match &self.style.label {
            Some(style) => {
                let keys: Vec<_> = (apps.iter())
                    .filter_map(|a| self.label_key(&a.app_id))
                    .collect();
                let widest = self.cache.borrow().widest_label(&keys);
                (text::pill_height(style.size) as f64, widest as f64)
            }
            None => (0.0, 0.0),
        };
        let l = Layout::new(&bar::Params {
            apps: apps.len(),
            icon_size: self.style.icon_size as f64,
            gap: self.style.gap,
            max_width: self.style.max_width,
            position: self.style.position,
            label,
            dots: self.style.dots,
            output: (self.width as f64, self.height as f64),
        });

        let scale = self.scale;
        let mut cache = self.cache.borrow_mut();
        self.panel_shape = cache.shape(
            &self.g,
            ShapeKey::Panel {
                w: l.panel.w as i32,
                h: l.panel.h as i32,
                radius: l.panel_radius as i32,
                color: self.style.panel_color,
                shadow: self.style.shadow,
                scale,
            },
            &self.qh,
        );
        let hl = l.highlight_rect(0.0);
        self.highlight_shape = cache.shape(
            &self.g,
            ShapeKey::Rect {
                w: hl.w as i32,
                h: hl.h as i32,
                radius: l.highlight_radius as i32,
                color: self.style.highlight_color,
                scale,
            },
            &self.qh,
        );
        self.dot_shapes = (apps.iter())
            .map(|app| {
                let count = app.windows.len().min(MAX_DOTS);
                if !self.style.dots || count < 2 {
                    return None;
                }
                let key = ShapeKey::Dots {
                    count: count as u32,
                    size: l.dot as i32,
                    color: self.style.dot_color,
                    scale,
                };
                cache.shape(&self.g, key, &self.qh)
            })
            .collect();
        drop(cache);
        self.layout = Some(l);
        self.retarget();
    }

    /// Point the highlight and scroll springs at the selection. Before the
    /// switcher is visible, they jump there.
    fn retarget(&mut self) {
        let Some(l) = &self.layout else { return };
        let i = self.switcher.selected_app();
        let scroll = l.scroll_for(i, self.scroll.target);
        let x = l.icon_rect(i, 0.0).x;
        if self.visible {
            self.scroll.retarget(scroll);
            self.highlight_x.retarget(x);
        } else {
            self.scroll.snap(scroll);
            self.highlight_x.snap(x);
        }
    }

    /// Show-delay passed: attach everything and fade in.
    pub fn reveal(&mut self) {
        if self.visible {
            return;
        }
        if !self.configured {
            self.reveal_wanted = true;
            return;
        }
        self.retarget();
        self.update_label();
        self.visible = true;
        self.open.snap(0.0);
        self.open.retarget(1.0);
        self.last_frame = Some(Instant::now());
        self.render();
    }

    /// Fade out. False if there is nothing to fade (the outcome can go
    /// right away).
    pub fn start_close(&mut self) -> bool {
        if !self.visible || self.g.alpha_modifier.is_none() {
            return false;
        }
        self.close.retarget(0.0);
        self.kick();
        true
    }

    /// Fading out after a cancel: nothing can be picked anymore.
    pub fn closing(&self) -> bool {
        self.close.target == 0.0
    }

    // -- selection -------------------------------------------------------

    pub fn step(&mut self, step: Step) {
        self.switcher.step(step);
        self.selection_changed();
    }

    /// Select app `i` (pointer hover).
    pub fn select_app(&mut self, i: usize) {
        let before = (self.switcher.mode(), self.switcher.selected_app());
        self.switcher.select_app(i);
        if (self.switcher.mode(), self.switcher.selected_app()) != before {
            self.selection_changed();
        }
    }

    fn selection_changed(&mut self) {
        self.retarget();
        self.update_label();
        self.kick();
    }

    /// A window closed. False if none are left.
    pub fn remove(&mut self, id: &str) -> bool {
        let apps = self.switcher.apps().len();
        if !self.switcher.remove(id) {
            return false;
        }
        self.handles.retain(|(_, i)| i != id);
        let now = self.switcher.apps().len();
        if now != apps {
            let (g, qh) = (self.g.clone(), self.qh.clone());
            let parent = self.layer.wl_surface().clone();
            let sub = || Sub::new(&g, &parent, &self.no_input, &qh);
            self.icons = (0..now).map(|_| sub()).collect();
            self.dots = (0..now).map(|_| sub()).collect();
        }
        self.relayout();
        self.selection_changed();
        true
    }

    // -- labels ----------------------------------------------------------

    /// What the label should show now; None: nothing.
    fn wanted_label(&mut self) -> Option<LabelWant> {
        self.style.label.as_ref()?;
        let app = &self.switcher.apps()[self.switcher.selected_app()];
        let title = self.switcher.picked().title.trim().to_string();
        if self.switcher.mode() == Mode::Apps || title.is_empty() {
            return Some(LabelWant::App(self.label_key(&app.app_id)?));
        }
        // Phase 2: the selected window's title in place of the app name.
        let max_width = match &self.layout {
            Some(l) => (l.clip.1 - l.clip.0) as i32,
            None => self.style.label_max_width,
        };
        let request = PillRequest {
            text: title,
            max_width: max_width.max(text::MIN_PILL_WIDTH),
            scale: self.scale,
            style: self.style.title.clone(),
        };
        // The same title as wanted or shown keeps its tag.
        let same = |w: &LabelWant| matches!(w, LabelWant::Title(_, r) if *r == request);
        let known = (self.label_wanted.iter())
            .chain(self.shown_labels[self.front].as_ref().map(|l| &l.want))
            .find(|w| same(w))
            .cloned();
        Some(known.unwrap_or_else(|| {
            self.next_tag += 1;
            LabelWant::Title(self.next_tag, request)
        }))
    }

    /// Show the selection's label, or request it.
    fn update_label(&mut self) {
        let want = self.wanted_label();
        let front = self.shown_labels[self.front].as_ref().map(|l| &l.want);
        if front == want.as_ref() {
            self.label_wanted = None;
            return;
        }
        match want {
            None => self.swap_label(None),
            Some(LabelWant::App(key)) => {
                let cached = self.cache.borrow().label(&key).cloned();
                match cached {
                    Some(s) => self.swap_label(Some(ShownLabel {
                        want: LabelWant::App(key),
                        buffer: s.buffer,
                        width: s.width,
                        height: s.height,
                    })),
                    None => {
                        self.assets.label(&key);
                        self.label_wanted = Some(LabelWant::App(key));
                    }
                }
            }
            Some(LabelWant::Title(tag, request)) => {
                if self.label_wanted != Some(LabelWant::Title(tag, request.clone())) {
                    self.assets.pill(tag, request.clone());
                    self.label_wanted = Some(LabelWant::Title(tag, request));
                }
            }
        }
    }

    /// Crossfade to `label`.
    fn swap_label(&mut self, label: Option<ShownLabel>) {
        self.label_wanted = None;
        self.front ^= 1;
        self.shown_labels[self.front] = label;
        if self.visible {
            self.label_fade.snap(0.0);
            self.label_fade.retarget(1.0);
        } else {
            self.label_fade.snap(1.0);
        }
        self.kick();
    }

    /// The worker rendered an icon (now in the cache).
    pub fn icon_ready(&mut self, key: &icons::Key) {
        if (self.switcher.apps().iter()).any(|a| self.icon_key(&a.app_id) == *key) {
            self.kick();
        }
    }

    /// The worker rendered a label (now in the cache).
    pub fn label_ready(&mut self, key: &LabelKey) {
        let ours =
            (self.switcher.apps().iter()).any(|a| self.label_key(&a.app_id).as_ref() == Some(key));
        if !ours {
            return;
        }
        // A wider label may widen the panel.
        let wider = (self.cache.borrow().label(key)).is_some_and(|s| {
            self.layout
                .as_ref()
                .is_some_and(|l| s.width as f64 > l.clip.1 - l.clip.0)
        });
        if wider {
            self.relayout();
        }
        if self.label_wanted == Some(LabelWant::App(key.clone())) {
            self.update_label();
        }
        self.kick();
    }

    /// The desktop entries changed; the cache was cleared.
    pub fn icons_reloaded(&mut self) {
        self.request_assets();
    }

    /// A title pill arrived from the worker.
    pub fn pill_ready(&mut self, tag: u64, request: PillRequest, pill: text::Pill) {
        if self.label_wanted != Some(LabelWant::Title(tag, request.clone())) {
            return; // the selection moved on
        }
        let buffer = match cache::shm_buffer(&self.g, &pill.image, &self.qh) {
            Ok(buffer) => buffer,
            Err(e) => return eprintln!("wl-switcher: title buffer: {e}"),
        };
        self.title_buffers.push(buffer.clone());
        self.swap_label(Some(ShownLabel {
            want: LabelWant::Title(tag, request),
            buffer,
            width: pill.width,
            height: pill.height,
        }));
    }

    // -- frames ----------------------------------------------------------

    fn request_frame(&mut self) {
        let surface = self.layer.wl_surface();
        surface.frame(&self.qh, FrameCallbackData(surface.clone()));
        self.frame_pending = true;
    }

    /// Something changed: draw it on the next frame.
    fn kick(&mut self) {
        if !self.visible || self.frame_pending {
            return;
        }
        if self.last_frame.is_none() {
            self.last_frame = Some(Instant::now());
        }
        self.request_frame();
        self.layer.commit();
    }

    /// The frame callback: advance the springs and draw.
    pub fn frame_done(&mut self) {
        self.frame_pending = false;
        let now = Instant::now();
        let dt = self
            .last_frame
            .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        let mut moving = false;
        for s in [
            &mut self.open,
            &mut self.close,
            &mut self.highlight_x,
            &mut self.scroll,
            &mut self.label_fade,
        ] {
            moving |= s.advance(dt);
        }
        self.last_frame = moving.then_some(now);
        self.render();
    }

    /// Apply the current state to every subsurface and commit, asking for
    /// another frame while anything moves.
    fn render(&mut self) {
        if self.visible {
            self.place_all();
        }
        let moving = [
            &self.open,
            &self.close,
            &self.highlight_x,
            &self.scroll,
            &self.label_fade,
        ]
        .iter()
        .any(|s| !s.settled());
        if moving && !self.frame_pending {
            self.request_frame();
        }
        self.layer.commit();
    }

    fn place_all(&mut self) {
        let Some(l) = self.layout.clone() else { return };
        let alpha = self.open.value * self.close.value;
        let s = lerp(OPEN_SCALE, 1.0, self.open.value);
        let (cx, cy) = (l.panel.x + l.panel.w / 2.0, l.panel.y + l.panel.h / 2.0);
        // The whole bar scales around its center.
        let xf = |r: bar::Rect| {
            IRect::round(bar::Rect::new(
                cx + (r.x - cx) * s,
                cy + (r.y - cy) * s,
                r.w * s,
                r.h * s,
            ))
        };
        let scale = self.scale as f64;
        let scroll = self.scroll.value;

        if let (Some(dim), Some(buffer)) = (&mut self.dim, &self.dim_buffer) {
            dim.show(buffer);
            dim.place(IRect {
                x: 0,
                y: 0,
                w: self.width.max(1) as i32,
                h: self.height.max(1) as i32,
            });
            dim.set_alpha(alpha);
            dim.commit();
        }

        match &self.panel_shape {
            Some(shape) => {
                let r = shape.rect;
                let rect = bar::Rect::new(
                    l.panel.x + r.x as f64,
                    l.panel.y + r.y as f64,
                    r.w as f64,
                    r.h as f64,
                );
                self.panel.show(&shape.buffer);
                self.panel.place(xf(rect));
                self.panel.set_alpha(alpha);
            }
            None => self.panel.hide(),
        }
        self.panel.commit();

        let hl_x = self.highlight_x.value - scroll;
        let hl = l.highlight_rect(hl_x);
        place_clipped(
            &mut self.highlight,
            self.highlight_shape.as_ref().map(|s| {
                let size = (s.rect.w as f64 * scale, s.rect.h as f64 * scale);
                (&s.buffer, size)
            }),
            hl,
            l.clip,
            &xf,
            alpha,
        );

        let cache = self.cache.borrow();
        for (i, app) in self.switcher.apps().iter().enumerate() {
            let r = l.icon_rect(i, scroll);
            let key = self.icon_key(&app.app_id);
            let buffer = cache.icon(&key);
            let size = key.size as f64;
            let icon = buffer.as_ref().map(|b| (b, (size, size)));
            place_clipped(&mut self.icons[i], icon, r, l.clip, &xf, alpha);

            let dots = self.dot_shapes.get(i).and_then(|s| s.as_ref()).map(|s| {
                let rect = bar::Rect::new(
                    r.x + (r.w - s.rect.w as f64) / 2.0,
                    l.dots_y,
                    s.rect.w as f64,
                    s.rect.h as f64,
                );
                let size = (s.rect.w as f64 * scale, s.rect.h as f64 * scale);
                (&s.buffer, size, rect)
            });
            let sub = &mut self.dots[i];
            match dots {
                Some((buffer, size, rect)) => {
                    place_clipped(sub, Some((buffer, size)), rect, l.clip, &xf, alpha)
                }
                None => {
                    sub.hide();
                    sub.commit();
                }
            }
        }
        drop(cache);

        // The label follows the highlight, within the panel.
        let center = hl_x + l.icon / 2.0;
        let (lo, hi) = (l.clip.0 + l.highlight_pad, l.clip.1 - l.highlight_pad);
        for k in 0..2 {
            let sub = &mut self.labels[k];
            let Some(label) = &self.shown_labels[k] else {
                sub.hide();
                sub.commit();
                continue;
            };
            let w = label.width as f64;
            let x = if w >= hi - lo {
                l.panel.x + (l.panel.w - w) / 2.0
            } else {
                (center - w / 2.0).clamp(lo, hi - w)
            };
            let fade = self.label_fade.value;
            let a = if k == self.front { fade } else { 1.0 - fade };
            sub.show(&label.buffer);
            sub.place(xf(bar::Rect::new(x, l.label_y, w, label.height as f64)));
            sub.set_alpha(alpha * a);
            sub.commit();
        }
    }

    // -- input -----------------------------------------------------------

    /// What is at (`x`, `y`). Nothing is hit before the switcher is visible.
    pub fn hit(&self, x: f64, y: f64) -> Hit {
        let Some(l) = self.layout.as_ref().filter(|_| self.visible) else {
            return Hit::Panel;
        };
        match l.hit(x, y, self.scroll.value) {
            Some(i) => Hit::App(i),
            None if l.panel.contains(x, y) => Hit::Panel,
            None => Hit::Outside,
        }
    }
}

/// Show `buffer` (with its size in pixels) on `sub` at `rect`, cut off
/// horizontally outside `clip`, transformed by `xf`. Hidden without a
/// buffer or if nothing is left. Commits `sub`.
fn place_clipped(
    sub: &mut Sub,
    buffer: Option<(&WlBuffer, (f64, f64))>,
    rect: bar::Rect,
    clip: (f64, f64),
    xf: &dyn Fn(bar::Rect) -> IRect,
    alpha: f64,
) {
    let x0 = rect.x.max(clip.0);
    let x1 = rect.right().min(clip.1);
    match buffer {
        Some((buffer, (bw, bh))) if x1 > x0 => {
            sub.show(buffer);
            let src = (x0 > rect.x || x1 < rect.right()).then(|| {
                // wl_fixed has 1/256 precision; stay inside the buffer.
                let floor = |v: f64| (v * 256.0).floor() / 256.0;
                let sx = floor((x0 - rect.x) / rect.w * bw);
                let sw = floor((x1 - x0) / rect.w * bw).clamp(1.0 / 256.0, bw - sx);
                [sx, 0.0, sw, floor(bh)]
            });
            sub.crop(src);
            sub.place(xf(bar::Rect::new(x0, rect.y, x1 - x0, rect.h)));
            sub.set_alpha(alpha);
        }
        _ => sub.hide(),
    }
    sub.commit();
}

impl Drop for Overlay {
    fn drop(&mut self) {
        for inhibitor in self.inhibitors.drain(..) {
            inhibitor.destroy();
        }
        self.viewport.destroy();
        self.transparent.destroy();
        if let Some(b) = &self.dim_buffer {
            b.destroy();
        }
        // Like capture buffers, attached ones may be destroyed right away.
        for b in &self.title_buffers {
            b.destroy();
        }
    }
}
