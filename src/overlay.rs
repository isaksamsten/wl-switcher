//! The switcher's layer surface and its scene.
//!
//! A fullscreen overlay-layer surface with exclusive keyboard focus. It is
//! mapped right away, with a transparent buffer, because it needs keyboard
//! focus to see the modifier being released. Nothing else is attached until
//! `show-delay` has passed (`reveal`), so a quick Alt+Tab never shows a
//! frame.
//!
//! Everything is a subsurface scaled with a viewport, in stacking order:
//! the dim tint; the bar (panel with its shadow, highlight, one icon and one
//! dot row per app, two labels that crossfade); the strip (per card: four
//! shadow strips and the preview; the icon the selected card grows out of;
//! the selection border; two title pills that crossfade). The subsurfaces
//! are sync: each frame repositions and re-alphas them, then commits the
//! parent once, so the frame updates atomically.
//!
//! Movement runs on springs (anim.rs), advanced on frame callbacks by the
//! time measured since the last one. Input only retargets springs and
//! requests a frame; once everything has settled, no more frames are
//! requested, so an idle switcher costs nothing.
//!
//! Windows mode is a morph (`morph`, 0 to 1): the bar fades out and shrinks
//! a little toward the selected icon, while the strip's cards grow out of
//! that icon into their places (strip.rs), staggered by their distance from
//! the selected card. Tab runs it back toward the newly selected icon.
//!
//! Previews are captures (capture.rs), planned so the morph never shows
//! placeholders: one frame of the selected app's windows and its
//! neighbors' on reveal and when the selection settles (debounced by the
//! app), live captures for the strip's app while it is open.

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
        wl_buffer::WlBuffer,
        wl_output::{Transform, WlOutput},
        wl_seat::WlSeat,
        wl_subsurface::WlSubsurface,
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
use crate::capture::{Capture, CaptureEvent, WindowId};
use crate::config::{Config, Previews};
use crate::globals::{Globals, Ignore};
use crate::icons;
use crate::shadow;
use crate::strip;
use crate::switcher::{Mode, Step, Switcher, Window};
use crate::text;
use crate::worker::{Assets, LabelKey, PillRequest};

/// The bar's scale when it starts fading in.
const OPEN_SCALE: f64 = 0.92;
/// The bar's scale with the strip open.
const MORPH_SCALE: f64 = 0.96;
/// Spring responses, ms.
const OPEN_RESPONSE: f64 = 120.0;
pub const CLOSE_MS: u64 = 100;
/// Dots shown under an icon at most.
const MAX_DOTS: usize = 3;
/// Space between strip cards (plus room for the zoom), logical pixels.
const CARD_GAP: f64 = 16.0;
/// The selection border's width.
const BORDER: f64 = 3.0;
/// The title pill's distance from the bottom of its card.
const TITLE_INSET: f64 = 10.0;
/// How much the icon the selected card grows out of grows with it.
const MORPH_ICON_GROWTH: f64 = 1.5;

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
    transform: Transform,
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
            transform: Transform::Normal,
        }
    }

    /// Attach `buffer` unless it is attached already; true if it wasn't.
    fn show(&mut self, buffer: &WlBuffer) -> bool {
        if self.attached.as_ref() == Some(buffer) {
            return false;
        }
        self.surface.attach(Some(buffer), 0, 0);
        self.surface.damage_buffer(0, 0, i32::MAX, i32::MAX);
        self.attached = Some(buffer.clone());
        true
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

    fn set_transform(&mut self, transform: Transform) {
        if transform != self.transform {
            self.surface.set_buffer_transform(transform);
            self.transform = transform;
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

    /// Show `buffer` at `r` with `alpha`, or nothing if that is invisible
    /// (or there is no buffer). Commits.
    fn present(&mut self, buffer: Option<&WlBuffer>, r: bar::Rect, alpha: f64) {
        match buffer {
            Some(buffer) if alpha > 0.0 => {
                self.show(buffer);
                self.place(IRect::round(r));
                self.set_alpha(alpha);
            }
            _ => self.hide(),
        }
        self.commit();
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

/// Every subsurface, created in stacking order (bottom first). Rebuilt as
/// a whole when the number of apps or of strip cards changes, which keeps
/// the order.
struct Scene {
    dim: Option<Sub>,
    panel: Sub,
    highlight: Sub,
    /// Per app.
    icons: Vec<Sub>,
    dots: Vec<Sub>,
    /// Two, to crossfade.
    labels: [Sub; 2],
    /// Per strip card: top, bottom, left, right.
    shadows: Vec<[Sub; 4]>,
    /// Per strip card.
    previews: Vec<Sub>,
    /// The selected app's icon, which the selected card grows out of.
    morph_icon: Sub,
    /// Top, bottom, left, right.
    border: [Sub; 4],
    titles: [Sub; 2],
}

impl Scene {
    fn new(
        g: &Globals,
        parent: &WlSurface,
        no_input: &Region,
        dim: bool,
        apps: usize,
        cards: usize,
        qh: &QueueHandle<App>,
    ) -> Self {
        let sub = || Sub::new(g, parent, no_input, qh);
        Self {
            dim: dim.then(sub),
            panel: sub(),
            highlight: sub(),
            icons: (0..apps).map(|_| sub()).collect(),
            dots: (0..apps).map(|_| sub()).collect(),
            labels: [sub(), sub()],
            shadows: (0..cards).map(|_| [sub(), sub(), sub(), sub()]).collect(),
            previews: (0..cards).map(|_| sub()).collect(),
            morph_icon: sub(),
            border: [sub(), sub(), sub(), sub()],
            titles: [sub(), sub()],
        }
    }
}

/// What a label or title shows.
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

/// Text on two subsurfaces that crossfade when it changes.
struct Fade {
    shown: [Option<ShownLabel>; 2],
    /// The current one.
    front: usize,
    /// What should show, until it arrives.
    wanted: Option<LabelWant>,
    /// The front one's alpha; the back one has the rest.
    fade: Spring,
}

impl Fade {
    fn new(response: f64) -> Self {
        Self {
            shown: [None, None],
            front: 0,
            wanted: None,
            fade: Spring::new(1.0, response),
        }
    }

    fn front(&self) -> Option<&LabelWant> {
        self.shown[self.front].as_ref().map(|l| &l.want)
    }

    /// Crossfade to `label`, or switch at once if not `animate`.
    fn swap(&mut self, label: Option<ShownLabel>, animate: bool) {
        self.wanted = None;
        self.front ^= 1;
        self.shown[self.front] = label;
        if animate {
            self.fade.snap(0.0);
            self.fade.retarget(1.0);
        } else {
            self.fade.snap(1.0);
        }
    }

    fn alpha(&self, k: usize) -> f64 {
        if k == self.front {
            self.fade.value
        } else {
            1.0 - self.fade.value
        }
    }

    /// A title request that is wanted or shown already, with its tag.
    fn known_title(&self, request: &PillRequest) -> Option<LabelWant> {
        let same = |w: &&LabelWant| matches!(w, LabelWant::Title(_, r) if r == request);
        (self.wanted.iter()).chain(self.front()).find(same).cloned()
    }
}

/// A capture frame handed to the overlay.
struct Shown {
    buffer: WlBuffer,
    width: u32,
    height: u32,
    transform: Transform,
    /// Attached at least once, so the compositor will release it.
    used: bool,
}

/// A window's capture and its latest frame.
struct Preview {
    /// The window's ext identifier.
    window: String,
    id: WindowId,
    capture: Capture,
    frame: Option<Shown>,
}

impl Preview {
    /// The window's aspect ratio, once known.
    fn aspect(&self) -> Option<f64> {
        let (w, h, transform) = match (&self.frame, self.capture.size()) {
            (Some(f), _) => (f.width, f.height, f.transform),
            (None, Some((w, h))) => (w, h, Transform::Normal),
            (None, None) => return None,
        };
        let swaps = matches!(
            transform,
            Transform::_90 | Transform::_270 | Transform::Flipped90 | Transform::Flipped270
        );
        let (w, h) = if swaps { (h, w) } else { (w, h) };
        (w > 0 && h > 0).then(|| w as f64 / h as f64)
    }
}

/// The strip as laid out for one app.
struct Strip {
    app_id: String,
    /// Its windows' ext identifiers, in strip order.
    windows: Vec<String>,
    layout: strip::Layout,
    /// Per card, the sides of its shadow; empty without shadows.
    shadows: Vec<[Option<Shape>; 4]>,
}

struct Style {
    icon_size: u32,
    gap: f64,
    max_width: f64,
    position: f64,
    /// None: no label (and no room for one).
    label: Option<text::Style>,
    /// The selected window's title in the strip.
    title: text::Style,
    label_max_width: i32,
    dots: bool,
    panel_color: u32,
    highlight_color: u32,
    dot_color: u32,
    shadow: Option<shadow::Settings>,
    icon_theme: Option<String>,
    font: String,
    previews: Previews,
    card_height: f64,
    strip_max_width: f64,
    zoom: f64,
}

/// What the pointer is over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    App(usize),
    /// A card in the strip: the selected app's window `i`.
    Window(usize),
    /// The panel between icons, or the strip between cards.
    Panel,
    Outside,
}

pub struct Overlay {
    // Field order is drop order: children before their parent surface.
    scene: Scene,
    /// Before the buffers they may have attached.
    previews: Vec<Preview>,
    no_input: Region,
    viewport: WpViewport,
    layer: LayerSurface,
    transparent: WlBuffer,
    dim_buffer: Option<WlBuffer>,
    placeholder: WlBuffer,
    border_buffer: WlBuffer,
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
    /// The selected app's name in the bar.
    label: Fade,
    /// The selected window's title in the strip.
    title: Fade,
    next_tag: u64,
    /// The strip, once laid out; it stays while it collapses.
    strip: Option<Strip>,
    /// The selection moved in apps mode; captures follow after a debounce.
    captures_stale: bool,

    /// Fading and scaling in, 0 to 1.
    open: Spring,
    /// Fading out on cancel, 1 to 0.
    close: Spring,
    /// The highlight's left edge, in unscrolled row coordinates.
    highlight_x: Spring,
    scroll: Spring,
    /// Apps mode (0) to windows mode (1).
    morph: Spring,
    /// The strip column's top.
    strip_offset: Spring,
    /// The selected card, fractional while moving.
    strip_sel: Spring,
    frame_pending: bool,
    last_frame: Option<Instant>,
}

/// The most cards any app needs.
fn card_count(switcher: &Switcher) -> usize {
    (switcher.apps().iter())
        .map(|a| a.windows.len())
        .max()
        .unwrap_or(0)
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
        let placeholder = cache::color_buffer(&g, theme.placeholder, qh)?;
        let border_buffer = cache::color_buffer(&g, theme.border_color, qh)?;
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

        let scene = Scene::new(
            &g,
            layer.wl_surface(),
            &no_input,
            dim_buffer.is_some(),
            switcher.apps().len(),
            card_count(&switcher),
            qh,
        );

        // Initial commit without a buffer; mapped on configure.
        layer.commit();

        let response = config.response as f64;
        // Opened with Alt+`: the strip is there from the start.
        let morph = match switcher.mode() {
            Mode::Apps => 0.0,
            Mode::Windows => 1.0,
        };
        let mut overlay = Self {
            scene,
            previews: Vec::new(),
            no_input,
            viewport,
            layer,
            transparent,
            dim_buffer,
            placeholder,
            border_buffer,
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
                previews: config.previews,
                card_height: config.strip_card_height,
                strip_max_width: config.strip_max_width,
                zoom: config.zoom,
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
            label: Fade::new(response),
            title: Fade::new(response),
            next_tag: 0,
            strip: None,
            captures_stale: false,
            open: Spring::new(0.0, OPEN_RESPONSE),
            close: Spring::new(1.0, CLOSE_MS as f64),
            highlight_x: Spring::new(0.0, response),
            scroll: Spring::new(0.0, response),
            morph: Spring::new(morph, config.morph_response as f64),
            strip_offset: Spring::new(0.0, response),
            strip_sel: Spring::new(0.0, response),
            frame_pending: false,
            last_frame: None,
        };
        if overlay.switcher.mode() == Mode::Windows {
            // Previews are needed right away.
            overlay.update_captures();
        }
        Ok(overlay)
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

    /// Lay the bar and the strip out for the output size, scale and apps,
    /// and get the shapes for them.
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
        self.relayout_strip();
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

    // -- the strip -------------------------------------------------------

    /// The app the strip shows: the selected one in windows mode, else
    /// the one it showed last (while it collapses).
    fn strip_app(&self) -> Option<String> {
        match self.switcher.mode() {
            Mode::Windows => {
                let app = &self.switcher.apps()[self.switcher.selected_app()];
                Some(app.app_id.clone())
            }
            Mode::Apps => self.strip.as_ref().map(|s| s.app_id.clone()),
        }
    }

    /// Lay the strip out for its app's windows and their sizes, and point
    /// its springs at the selection.
    fn relayout_strip(&mut self) {
        if !self.configured || self.width == 0 || self.height == 0 {
            return;
        }
        let Some(app_id) = self.strip_app() else {
            return;
        };
        let Some(app) = (self.switcher.apps().iter()).find(|a| a.app_id == app_id) else {
            self.strip = None;
            return;
        };
        let all = self.switcher.windows();
        let windows: Vec<String> = (app.windows.iter()).map(|&i| all[i].id.clone()).collect();
        let (out_w, out_h) = (self.width as f64, self.height as f64);
        let off = self.style.previews == Previews::Off;
        let aspects = (windows.iter())
            .map(|id| match off {
                true => 1.0,
                false => (self.previews.iter())
                    .find(|p| p.window == *id)
                    .and_then(Preview::aspect)
                    .unwrap_or(strip::DEFAULT_ASPECT),
            })
            .collect();
        // Without previews, cards are icons: not larger than they are.
        let card_height = match off {
            true => (self.style.icon_size as f64).min(out_h * self.style.card_height),
            false => out_h * self.style.card_height,
        };
        let center_y = match &self.layout {
            Some(l) => l.panel.center().1,
            None => out_h * self.style.position,
        };
        let layout = strip::Layout::new(&strip::Params {
            aspects,
            card_height,
            max_width: out_w * self.style.strip_max_width,
            gap: CARD_GAP,
            zoom: self.style.zoom,
            center_y,
            output: (out_w, out_h),
        });
        let shadows = match self.style.shadow.filter(|_| !off) {
            Some(settings) => {
                let mut cache = self.cache.borrow_mut();
                (0..layout.len())
                    .map(|i| {
                        let c = layout.card(i, 0.0);
                        let size = (c.w as i32, c.h as i32);
                        cache.card_shadow(&self.g, size, settings, self.scale, &self.qh)
                    })
                    .collect()
            }
            None => Vec::new(),
        };
        // A new app's strip starts where it should be; so does one that
        // isn't showing yet.
        let fresh = self.strip.as_ref().is_none_or(|s| s.app_id != app_id)
            || !self.visible
            || self.morph.value <= 0.0;
        self.strip = Some(Strip {
            app_id,
            windows,
            layout,
            shadows,
        });
        self.retarget_strip(fresh);
        self.update_title();
    }

    /// Point the strip's springs at the selected card.
    fn retarget_strip(&mut self, snap: bool) {
        let Some(s) = &self.strip else { return };
        let Some(last) = s.layout.len().checked_sub(1) else {
            return;
        };
        let sel = match self.switcher.mode() {
            Mode::Windows => self.switcher.selected_window(),
            // Collapsing: the card it collapses from.
            Mode::Apps => self.strip_sel.target.round() as usize,
        }
        .min(last);
        let offset = s.layout.offset_for(sel);
        if snap {
            self.strip_offset.snap(offset);
            self.strip_sel.snap(sel as f64);
        } else {
            self.strip_offset.retarget(offset);
            self.strip_sel.retarget(sel as f64);
        }
    }

    // -- captures --------------------------------------------------------

    /// Whether the selection moved in apps mode since the captures were
    /// last planned. The app calls `update_captures` after a debounce.
    pub fn captures_stale(&self) -> bool {
        self.captures_stale
    }

    /// Capture the windows of the selected app and its neighbors (a frame
    /// each, kept until the show ends), and the strip's app live while the
    /// strip is open.
    pub fn update_captures(&mut self) {
        self.captures_stale = false;
        if self.style.previews == Previews::Off {
            return;
        }
        let apps = self.switcher.apps();
        let (n, sel) = (apps.len() as isize, self.switcher.selected_app() as isize);
        let mut wanted: Vec<usize> = Vec::new();
        for d in [0, 1, -1] {
            let a = (sel + d).rem_euclid(n) as usize;
            for &w in &apps[a].windows {
                if !wanted.contains(&w) {
                    wanted.push(w);
                }
            }
        }
        let live = self.switcher.mode() == Mode::Windows && self.style.previews == Previews::Live;
        let live: Vec<&str> = match live {
            true => (apps[sel as usize].windows.iter())
                .map(|&w| self.switcher.windows()[w].id.as_str())
                .collect(),
            false => Vec::new(),
        };
        for w in wanted {
            let id = &self.switcher.windows()[w].id;
            if self.previews.iter().any(|p| p.window == *id) {
                continue;
            }
            let Some((handle, _)) = self.handles.iter().find(|(_, i)| i == id) else {
                continue;
            };
            let wid = WindowId::next();
            let is_live = live.contains(&id.as_str());
            self.previews.push(Preview {
                window: id.clone(),
                id: wid,
                capture: Capture::window(wid, handle, &self.g, is_live, false, &self.qh),
                frame: None,
            });
        }
        for p in &mut self.previews {
            let is_live = live.contains(&p.window.as_str());
            p.capture.set_live(is_live, &self.g, &self.qh);
        }
    }

    /// Run `f` on a window's capture and apply what it reports.
    pub fn capture_update(
        &mut self,
        id: WindowId,
        f: impl FnOnce(&mut Capture, &Globals) -> Option<CaptureEvent>,
    ) {
        let Some(p) = self.previews.iter_mut().find(|p| p.id == id) else {
            return;
        };
        let before = p.aspect();
        match f(&mut p.capture, &self.g) {
            Some(CaptureEvent::Frame(frame)) => {
                // A frame never attached is never released: free it here.
                if let Some(old) = p.frame.take()
                    && !old.used
                {
                    p.capture.discard(&old.buffer, &self.qh);
                }
                p.frame = Some(Shown {
                    buffer: frame.buffer,
                    width: frame.width,
                    height: frame.height,
                    transform: frame.transform,
                    used: false,
                });
            }
            Some(CaptureEvent::Failed(why)) => {
                eprintln!("wl-switcher: capture of '{}' failed: {why}", p.window);
            }
            None => return,
        }
        let resized = p.aspect() != before;
        let window = p.window.clone();
        let shown = (self.strip.as_ref()).is_some_and(|s| s.windows.contains(&window));
        if shown {
            if resized {
                self.relayout_strip();
            }
            self.kick();
        }
    }

    // -- showing ---------------------------------------------------------

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
        self.retarget_strip(true);
        self.update_label();
        self.update_title();
        self.update_captures();
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
        let mode = self.switcher.mode();
        self.switcher.step(step);
        self.selection_changed(mode);
    }

    /// Select app `i` (pointer hover).
    pub fn select_app(&mut self, i: usize) {
        let before = (self.switcher.mode(), self.switcher.selected_app());
        self.switcher.select_app(i);
        if (self.switcher.mode(), self.switcher.selected_app()) != before {
            self.selection_changed(before.0);
        }
    }

    /// Select window `i` of the selected app (pointer hover in the strip).
    pub fn select_window(&mut self, i: usize) {
        let before = self.switcher.selected_window();
        self.switcher.select_window(i);
        if self.switcher.selected_window() != before {
            self.selection_changed(Mode::Windows);
        }
    }

    /// The selection changed, from mode `was`.
    fn selection_changed(&mut self, was: Mode) {
        let mode = self.switcher.mode();
        let morph = match mode {
            Mode::Apps => 0.0,
            Mode::Windows => 1.0,
        };
        if self.visible {
            self.morph.retarget(morph);
        } else {
            self.morph.snap(morph);
        }
        self.retarget();
        if mode == Mode::Windows {
            self.relayout_strip(); // a new app, or the same one: retargets
        } else {
            self.retarget_strip(false);
        }
        self.update_label();
        self.update_title();
        // The strip needs its previews now; Tab-cycling can wait.
        if mode == Mode::Windows || was != mode {
            self.update_captures();
        } else {
            self.captures_stale = true;
        }
        self.kick();
    }

    /// A window closed. False if none are left.
    pub fn remove(&mut self, id: &str) -> bool {
        let shape = (self.switcher.apps().len(), card_count(&self.switcher));
        let mode = self.switcher.mode();
        if !self.switcher.remove(id) {
            return false;
        }
        self.handles.retain(|(_, i)| i != id);
        self.previews.retain(|p| p.window != id);
        if (self.switcher.apps().len(), card_count(&self.switcher)) != shape {
            self.scene = Scene::new(
                &self.g,
                self.layer.wl_surface(),
                &self.no_input,
                self.dim_buffer.is_some(),
                self.switcher.apps().len(),
                card_count(&self.switcher),
                &self.qh,
            );
        }
        self.relayout();
        self.selection_changed(mode);
        true
    }

    // -- labels and titles -----------------------------------------------

    /// Show the selected app's label, or request it.
    fn update_label(&mut self) {
        let app = &self.switcher.apps()[self.switcher.selected_app()];
        let want = self.label_key(&app.app_id).map(LabelWant::App);
        if self.label.front() == want.as_ref() {
            self.label.wanted = None;
            return;
        }
        let animate = self.visible;
        match want {
            None => self.label.swap(None, animate),
            Some(LabelWant::App(key)) => {
                let cached = self.cache.borrow().label(&key).cloned();
                match cached {
                    Some(s) => self.label.swap(
                        Some(ShownLabel {
                            want: LabelWant::App(key),
                            buffer: s.buffer,
                            width: s.width,
                            height: s.height,
                        }),
                        animate,
                    ),
                    None => {
                        self.assets.label(&key);
                        self.label.wanted = Some(LabelWant::App(key));
                    }
                }
            }
            Some(LabelWant::Title(..)) => unreachable!("labels are app names"),
        }
        self.kick();
    }

    /// Show the selected window's title in the strip, or request it. Only
    /// in windows mode: a collapsing strip keeps its title.
    fn update_title(&mut self) {
        if self.switcher.mode() != Mode::Windows {
            return;
        }
        let want = self.wanted_title();
        if self.title.front() == want.as_ref() {
            self.title.wanted = None;
            return;
        }
        match want {
            None => {
                self.title.swap(None, self.visible);
                self.kick();
            }
            Some(LabelWant::Title(tag, request)) => {
                let want = LabelWant::Title(tag, request.clone());
                if self.title.wanted.as_ref() != Some(&want) {
                    self.assets.pill(tag, request);
                    self.title.wanted = Some(want);
                }
            }
            Some(LabelWant::App(_)) => unreachable!("titles are window titles"),
        }
    }

    fn wanted_title(&mut self) -> Option<LabelWant> {
        let strip = self.strip.as_ref()?;
        let i = self.switcher.selected_window();
        let title = self.switcher.picked().title.trim().to_string();
        if title.is_empty() || i >= strip.layout.len() {
            return None;
        }
        let card = strip.layout.card(i, 0.0);
        let max_width = (card.w - 2.0 * TITLE_INSET) as i32;
        let request = PillRequest {
            text: title,
            max_width: max_width.max(text::MIN_PILL_WIDTH),
            scale: self.scale,
            style: self.style.title.clone(),
        };
        // The same title as wanted or shown keeps its tag.
        Some(self.title.known_title(&request).unwrap_or_else(|| {
            self.next_tag += 1;
            LabelWant::Title(self.next_tag, request)
        }))
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
        if self.label.wanted == Some(LabelWant::App(key.clone())) {
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
        if self.title.wanted != Some(LabelWant::Title(tag, request.clone())) {
            return; // the selection moved on
        }
        let buffer = match cache::shm_buffer(&self.g, &pill.image, &self.qh) {
            Ok(buffer) => buffer,
            Err(e) => return eprintln!("wl-switcher: title buffer: {e}"),
        };
        self.title_buffers.push(buffer.clone());
        let label = ShownLabel {
            want: LabelWant::Title(tag, request),
            buffer,
            width: pill.width,
            height: pill.height,
        };
        self.title.swap(Some(label), self.visible);
        self.kick();
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

    fn springs(&mut self) -> [&mut Spring; 9] {
        [
            &mut self.open,
            &mut self.close,
            &mut self.highlight_x,
            &mut self.scroll,
            &mut self.label.fade,
            &mut self.title.fade,
            &mut self.morph,
            &mut self.strip_offset,
            &mut self.strip_sel,
        ]
    }

    /// The frame callback: advance the springs and draw.
    pub fn frame_done(&mut self) {
        self.frame_pending = false;
        let now = Instant::now();
        let dt = self
            .last_frame
            .map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        let mut moving = false;
        for s in self.springs() {
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
        let moving = self.springs().iter().any(|s| !s.settled());
        if moving && !self.frame_pending {
            self.request_frame();
        }
        self.layer.commit();
    }

    /// An alpha as shown: without wp_alpha_modifier_v1, things are either
    /// there or not.
    fn gate(&self, a: f64) -> f64 {
        let a = a.clamp(0.0, 1.0);
        match self.g.alpha_modifier {
            Some(_) => a,
            None if a >= 0.5 => 1.0,
            None => 0.0,
        }
    }

    fn place_all(&mut self) {
        let Some(l) = self.layout.clone() else { return };
        let base = self.open.value * self.close.value;
        let morph = self.morph.value.clamp(0.0, 1.0);
        let s_open = lerp(OPEN_SCALE, 1.0, self.open.value);
        let (pcx, pcy) = l.panel.center();
        let scroll = self.scroll.value;
        // The selected icon: where the strip grows from, and what the bar
        // shrinks toward.
        let icon = l
            .icon_rect(self.switcher.selected_app(), scroll)
            .scaled(s_open, pcx, pcy);
        let (icx, icy) = icon.center();
        let s_morph = lerp(1.0, MORPH_SCALE, morph);
        let xf =
            move |r: bar::Rect| IRect::round(r.scaled(s_open, pcx, pcy).scaled(s_morph, icx, icy));
        let scale = self.scale as f64;
        let alpha = self.gate(base * (1.0 - morph));
        let dim_alpha = self.gate(base);
        let scene = &mut self.scene;

        if let (Some(dim), Some(buffer)) = (&mut scene.dim, &self.dim_buffer) {
            let full = bar::Rect::new(0.0, 0.0, self.width as f64, self.height as f64);
            dim.present(Some(buffer), full, dim_alpha);
        }

        match &self.panel_shape {
            Some(shape) if alpha > 0.0 => {
                let r = shape.rect;
                let rect = bar::Rect::new(
                    l.panel.x + r.x as f64,
                    l.panel.y + r.y as f64,
                    r.w as f64,
                    r.h as f64,
                );
                scene.panel.show(&shape.buffer);
                scene.panel.place(xf(rect));
                scene.panel.set_alpha(alpha);
            }
            _ => scene.panel.hide(),
        }
        scene.panel.commit();

        let hl_x = self.highlight_x.value - scroll;
        let hl = l.highlight_rect(hl_x);
        place_clipped(
            &mut scene.highlight,
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
            let key = icons::Key {
                app_id: app.app_id.clone(),
                size: self.style.icon_size * self.scale as u32,
                theme: self.style.icon_theme.clone(),
            };
            let buffer = cache.icon(&key);
            let size = key.size as f64;
            let icon = buffer.as_ref().map(|b| (b, (size, size)));
            place_clipped(&mut scene.icons[i], icon, r, l.clip, &xf, alpha);

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
            let sub = &mut scene.dots[i];
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
            let sub = &mut scene.labels[k];
            let Some(label) = &self.label.shown[k] else {
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
            let a = alpha * self.label.alpha(k);
            if a <= 0.0 {
                sub.hide();
                sub.commit();
                continue;
            }
            sub.show(&label.buffer);
            sub.place(xf(bar::Rect::new(x, l.label_y, w, label.height as f64)));
            sub.set_alpha(a);
            sub.commit();
        }

        self.place_strip(icon, base, morph);
    }

    /// The strip's cards, each morphing from `icon` (the selected app's
    /// icon) to its place by `morph`, the selection border and the title.
    fn place_strip(&mut self, icon: bar::Rect, base: f64, morph: f64) {
        let gate = |a: f64| self.gate(a);
        let late = |m: f64| ((m - 0.5) * 2.0).clamp(0.0, 1.0);
        let border_alpha = gate(base * late(morph));
        let icon_alpha = match morph > 0.0 {
            true => gate(base * (1.0 - 2.0 * morph)),
            false => 0.0,
        };
        // The cards' app: while the strip collapses after Tab, the
        // selected app is the next one already.
        let strip_icon =
            (self.strip.as_ref()).and_then(|s| self.cache.borrow().icon(&self.icon_key(&s.app_id)));
        let card_alphas: Vec<f64> = match &self.strip {
            Some(s) => {
                let sel = self.strip_sel.target.round() as usize;
                (0..s.layout.len())
                    .map(|i| gate(base * strip::stagger(morph, i.abs_diff(sel))))
                    .collect()
            }
            None => Vec::new(),
        };
        let title_alphas = [0, 1].map(|k| gate(base * late(morph) * self.title.alpha(k)));
        let scene = &mut self.scene;

        let Some(s) = &self.strip else {
            let subs = (scene.shadows.iter_mut().flatten())
                .chain(&mut scene.previews)
                .chain([&mut scene.morph_icon])
                .chain(&mut scene.border)
                .chain(&mut scene.titles);
            for sub in subs {
                sub.hide();
                sub.commit();
            }
            return;
        };
        let (off, sel) = (self.strip_offset.value, self.strip_sel.value);
        let sel_i = self.strip_sel.target.round() as usize;

        for (i, (sub, shadow)) in (scene.previews.iter_mut())
            .zip(scene.shadows.iter_mut())
            .enumerate()
        {
            let Some(&a) = card_alphas.get(i) else {
                sub.hide();
                sub.commit();
                for side in shadow {
                    side.hide();
                    side.commit();
                }
                continue;
            };
            let local = strip::stagger(morph, i.abs_diff(sel_i));
            let rect = icon.lerp(s.layout.zoomed(i, off, sel), local);

            // The capture's frame, the icon without previews, or the
            // placeholder until the first frame.
            let preview = (self.previews.iter_mut())
                .find(|p| p.window == s.windows[i])
                .filter(|p| p.frame.as_ref().is_some_and(|f| p.capture.owns(&f.buffer)));
            let (buffer, transform) = match (&preview, self.style.previews) {
                (_, Previews::Off) => (strip_icon.as_ref(), Transform::Normal),
                (Some(p), _) => {
                    let f = p.frame.as_ref().expect("filtered");
                    (Some(&f.buffer), f.transform)
                }
                (None, _) => (Some(&self.placeholder), Transform::Normal),
            };
            // Hidden while transparent. Detaching releases the buffer; it
            // is marked busy again when re-attached.
            match buffer {
                Some(buffer) if a > 0.0 => {
                    sub.set_transform(transform);
                    if sub.show(buffer)
                        && let Some(p) = preview
                        && let Some(f) = &mut p.frame
                    {
                        f.used = true;
                        p.capture.attached(&f.buffer);
                    }
                    sub.place(IRect::round(rect));
                    sub.set_alpha(a);
                }
                _ => sub.hide(),
            }
            sub.commit();

            // The shadow scales with the card.
            let card = s.layout.card(i, 0.0);
            let (sx, sy) = (rect.w / card.w, rect.h / card.h);
            for (k, side) in shadow.iter_mut().enumerate() {
                let shape = s.shadows.get(i).and_then(|sides| sides[k].as_ref());
                let r = shape.map(|sh| {
                    let r = sh.rect;
                    let (x, y) = (rect.x + r.x as f64 * sx, rect.y + r.y as f64 * sy);
                    bar::Rect::new(x, y, r.w as f64 * sx, r.h as f64 * sy)
                });
                match (shape, r) {
                    (Some(sh), Some(r)) => side.present(Some(&sh.buffer), r, a),
                    _ => side.present(None, bar::Rect::default(), 0.0),
                }
            }
        }

        let Some(target) = s.layout.selection(sel, off) else {
            return;
        };
        let selected = icon.lerp(target, morph);

        // The icon on top of the selected card, fading out by halfway.
        let growth = lerp(1.0, MORPH_ICON_GROWTH, morph);
        let r = selected.centered_square(icon.w * growth);
        scene.morph_icon.present(strip_icon.as_ref(), r, icon_alpha);

        let b = BORDER;
        let (x, y, w, h) = (selected.x, selected.y, selected.w, selected.h);
        let sides = [
            bar::Rect::new(x - b, y - b, w + 2.0 * b, b),
            bar::Rect::new(x - b, y + h, w + 2.0 * b, b),
            bar::Rect::new(x - b, y, b, h),
            bar::Rect::new(x + w, y, b, h),
        ];
        for (sub, r) in scene.border.iter_mut().zip(sides) {
            sub.present(Some(&self.border_buffer), r, border_alpha);
        }

        // The title sits on the bottom of the selected card.
        let (cx, _) = selected.center();
        let titles = (scene.titles.iter_mut())
            .zip(&self.title.shown)
            .zip(title_alphas);
        for ((sub, title), a) in titles {
            let Some(title) = title else {
                sub.present(None, selected, 0.0);
                continue;
            };
            let (w, h) = (title.width as f64, title.height as f64);
            let r = bar::Rect::new(cx - w / 2.0, selected.bottom() - h - TITLE_INSET, w, h);
            sub.present(Some(&title.buffer), r, a);
        }
    }

    // -- input -----------------------------------------------------------

    /// What is at (`x`, `y`). Nothing is hit before the switcher is visible.
    pub fn hit(&self, x: f64, y: f64) -> Hit {
        let Some(l) = self.layout.as_ref().filter(|_| self.visible) else {
            return Hit::Panel;
        };
        if self.switcher.mode() == Mode::Windows {
            let Some(s) = &self.strip else {
                return Hit::Panel;
            };
            let off = self.strip_offset.value;
            return match s.layout.hit(x, y, off) {
                Some(i) => Hit::Window(i),
                None if s.layout.bounds(off).contains(x, y) => Hit::Panel,
                None => Hit::Outside,
            };
        }
        match l.hit(x, y, self.scroll.value) {
            Some(i) => Hit::App(i),
            None if l.panel.contains(x, y) => Hit::Panel,
            None => Hit::Outside,
        }
    }
}

/// Show `buffer` (with its size in pixels) on `sub` at `rect`, cut off
/// horizontally outside `clip`, transformed by `xf`. Hidden without a
/// buffer, if nothing is left, or if fully transparent. Commits `sub`.
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
        Some((buffer, (bw, bh))) if x1 > x0 && alpha > 0.0 => {
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
        self.placeholder.destroy();
        self.border_buffer.destroy();
        if let Some(b) = &self.dim_buffer {
            b.destroy();
        }
        // Like capture buffers, attached ones may be destroyed right away.
        for b in &self.title_buffers {
            b.destroy();
        }
    }
}
