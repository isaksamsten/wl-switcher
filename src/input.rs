//! Seat handling: the keyboard (xkbcommon, with key repeat) and pointer.
//!
//! Once open, the switcher has the keyboard and handles every key itself
//! while the modifier is held:
//!
//! - Tab / Shift+Tab (ISO_Left_Tab): next / previous app.
//! - The grave key / with Shift: next / previous window. It is matched by
//!   evdev keycode (`KEY_GRAVE`), not keysym, so it works on any layout and
//!   with Shift held.
//! - Left / Right: previous / next app; Up / Down: previous / next window.
//! - Escape cancels, Return picks.
//!
//! Releasing the modifier picks (see `Hold`). The pointer selects what it
//! moves over; a click picks it, a click outside the bar cancels.

use smithay_client_toolkit::seat::{
    Capability, SeatHandler, SeatState,
    keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers},
    pointer::{PointerEvent, PointerEventKind, PointerHandler},
};
use wayland_client::{
    Connection, Proxy, QueueHandle,
    protocol::{
        wl_keyboard::WlKeyboard, wl_pointer::WlPointer, wl_seat::WlSeat, wl_surface::WlSurface,
    },
};
use wayland_protocols::wp::cursor_shape::v1::client::wp_cursor_shape_device_v1::{
    Shape, WpCursorShapeDeviceV1,
};

use crate::app::App;
use crate::config::Modifier;
use crate::overlay::Hit;
use crate::switcher::Step;

/// linux/input-event-codes.h
const BTN_LEFT: u32 = 0x110;
const KEY_GRAVE: u32 = 41;

#[derive(Default)]
pub struct Input {
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    cursor_shape: Option<WpCursorShapeDeviceV1>,
    /// Last pointer position on the overlay surface.
    pointer_pos: (f64, f64),
    pointer_inside: bool,
    shift: bool,
    pub hold: Hold,
}

/// The modifiers that matter for release-to-pick.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub alt: bool,
    pub ctrl: bool,
    pub logo: bool,
}

impl Mods {
    fn from(m: &Modifiers) -> Self {
        Self {
            alt: m.alt,
            ctrl: m.ctrl,
            logo: m.logo,
        }
    }

    /// Those of `self` that `which` cares about.
    fn only(self, which: Modifier) -> Self {
        let none = Self::default();
        match which {
            Modifier::Auto => self,
            Modifier::Alt => Self {
                alt: self.alt,
                ..none
            },
            Modifier::Ctrl => Self {
                ctrl: self.ctrl,
                ..none
            },
            Modifier::Super => Self {
                logo: self.logo,
                ..none
            },
        }
    }

    fn any(self) -> bool {
        self.alt || self.ctrl || self.logo
    }

    /// Whether any of `held` is still down in `self`.
    fn holds_any_of(self, held: Self) -> bool {
        (held.alt && self.alt) || (held.ctrl && self.ctrl) || (held.logo && self.logo)
    }
}

/// Release-to-pick. On keyboard enter, the next `modifiers` event says
/// which (non-Shift) modifiers are held; once all of them are released, it
/// is time to pick. If none are held when focus arrives (a very fast tap),
/// that is right away.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Hold {
    /// No focus, or picked already.
    #[default]
    Idle,
    /// Focus arrived; waiting for the modifier state.
    Entered,
    Holding(Mods),
}

impl Hold {
    pub fn enter(&mut self) {
        *self = Hold::Entered;
    }

    pub fn leave(&mut self) {
        *self = Hold::Idle;
    }

    /// The modifiers are now `mods`. True: released, pick.
    pub fn modifiers(&mut self, mods: Mods, which: Modifier) -> bool {
        match *self {
            Hold::Idle => false,
            Hold::Entered => {
                let held = mods.only(which);
                *self = if held.any() {
                    Hold::Holding(held)
                } else {
                    Hold::Idle
                };
                !held.any()
            }
            Hold::Holding(held) => {
                let released = !mods.holds_any_of(held);
                if released {
                    *self = Hold::Idle;
                }
                released
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Action {
    Cancel,
    Pick,
    Step(Step),
}

impl Action {
    /// `raw`: the evdev keycode.
    fn from_key(sym: Keysym, raw: u32, shift: bool) -> Option<Self> {
        // Shift reverses Tab and the grave key.
        let next = !shift;
        let app = |forward| {
            Action::Step(if forward {
                Step::NextApp
            } else {
                Step::PrevApp
            })
        };
        let window = |forward| {
            Action::Step(if forward {
                Step::NextWindow
            } else {
                Step::PrevWindow
            })
        };
        if raw == KEY_GRAVE {
            return Some(window(next));
        }
        Some(match sym {
            Keysym::Escape => Action::Cancel,
            Keysym::Return | Keysym::KP_Enter => Action::Pick,
            Keysym::Tab => app(next),
            Keysym::ISO_Left_Tab => app(false),
            Keysym::Right => app(true),
            Keysym::Left => app(false),
            Keysym::Down => window(true),
            Keysym::Up => window(false),
            _ => return None,
        })
    }

    /// Stepping repeats while the key is held; the rest doesn't.
    fn repeats(self) -> bool {
        matches!(self, Action::Step(_))
    }
}

impl App {
    fn key(&mut self, event: &KeyEvent, repeat: bool) {
        let Some(action) = Action::from_key(event.keysym, event.raw_code, self.input.shift) else {
            return;
        };
        if self.overlay.is_none() || (repeat && !action.repeats()) {
            return;
        }
        match action {
            Action::Cancel => self.cancel(),
            Action::Pick => self.pick(),
            Action::Step(step) => {
                if let Some(overlay) = &mut self.overlay {
                    overlay.step(step);
                }
            }
        }
    }

    fn release_keyboard(&mut self) {
        if let Some(kb) = self.input.keyboard.take()
            && kb.version() >= 3
        {
            kb.release();
        }
    }

    fn release_pointer(&mut self) {
        if let Some(device) = self.input.cursor_shape.take() {
            device.destroy();
        }
        if let Some(pointer) = self.input.pointer.take()
            && pointer.version() >= 3
        {
            pointer.release();
        }
        self.input.pointer_inside = false;
    }

    /// Whether `surface` is the open switcher's.
    fn ours(&self, surface: &WlSurface) -> bool {
        self.overlay
            .as_ref()
            .is_some_and(|o| o.surface() == surface)
    }
}

impl Input {
    /// The switcher closed; a new one gets a fresh enter.
    pub fn reset(&mut self) {
        self.pointer_inside = false;
        self.hold.leave();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        cap: Capability,
    ) {
        match cap {
            Capability::Keyboard if self.input.keyboard.is_none() => {
                let repeat = Box::new(|app: &mut App, _: &WlKeyboard, event: KeyEvent| {
                    app.key(&event, true);
                });
                let loop_handle = self.loop_handle.clone();
                match self
                    .seat_state
                    .get_keyboard_with_repeat(qh, &seat, None, loop_handle, repeat)
                {
                    Ok(kb) => self.input.keyboard = Some(kb),
                    Err(e) => eprintln!("wl-switcher: keyboard unavailable: {e}"),
                }
            }
            Capability::Pointer if self.input.pointer.is_none() => {
                match self.seat_state.get_pointer(qh, &seat) {
                    Ok(pointer) => {
                        self.input.cursor_shape = self
                            .cursor_shape
                            .as_ref()
                            .map(|m| m.get_shape_device(&pointer, qh));
                        self.input.pointer = Some(pointer);
                    }
                    Err(e) => eprintln!("wl-switcher: pointer unavailable: {e}"),
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: WlSeat,
        cap: Capability,
    ) {
        match cap {
            Capability::Keyboard => self.release_keyboard(),
            Capability::Pointer => self.release_pointer(),
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        if self.ours(surface) {
            self.input.hold.enter();
        }
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        surface: &WlSurface,
        _: u32,
    ) {
        // Without focus, the release can't be seen.
        if self.ours(surface) && self.input.hold != Hold::Idle {
            self.input.hold.leave();
            eprintln!("wl-switcher: lost keyboard focus");
            self.cancel();
        }
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(&event, false);
    }

    /// Compositor-side repeat (wl_keyboard v10).
    fn repeat_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.key(&event, true);
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: RawModifiers,
        _: u32,
    ) {
        self.input.shift = modifiers.shift;
        let which = self.config.modifier;
        if self.input.hold.modifiers(Mods::from(&modifiers), which) {
            self.pick();
        }
    }
}

impl PointerHandler for App {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            let Some(overlay) = &mut self.overlay else {
                return;
            };
            if &event.surface != overlay.surface() {
                continue;
            }
            let (x, y) = event.position;
            match event.kind {
                PointerEventKind::Enter { serial } => {
                    // Don't select on enter: the pointer may just happen to
                    // rest on an icon when the switcher appears.
                    self.input.pointer_pos = (x, y);
                    self.input.pointer_inside = true;
                    if let Some(device) = &self.input.cursor_shape {
                        device.set_shape(serial, Shape::Default);
                    }
                }
                PointerEventKind::Leave { .. } => self.input.pointer_inside = false,
                PointerEventKind::Motion { .. } => {
                    self.input.pointer_pos = (x, y);
                    if let Hit::App(i) = overlay.hit(x, y) {
                        overlay.select_app(i);
                    }
                }
                PointerEventKind::Press { button, .. } => {
                    if !overlay.visible() || !self.input.pointer_inside {
                        continue;
                    }
                    let (px, py) = self.input.pointer_pos;
                    match overlay.hit(px, py) {
                        Hit::Outside => self.cancel(),
                        Hit::App(i) if button == BTN_LEFT => {
                            overlay.select_app(i);
                            self.pick();
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALT: Mods = Mods {
        alt: true,
        ctrl: false,
        logo: false,
    };
    const NONE: Mods = Mods {
        alt: false,
        ctrl: false,
        logo: false,
    };

    #[test]
    fn release_picks() {
        let mut h = Hold::default();
        // Before focus, modifier changes mean nothing.
        assert!(!h.modifiers(NONE, Modifier::Auto));
        h.enter();
        assert!(!h.modifiers(ALT, Modifier::Auto));
        assert_eq!(h, Hold::Holding(ALT));
        // Shift (not tracked) and other changes while held: no pick.
        assert!(!h.modifiers(ALT, Modifier::Auto));
        assert!(h.modifiers(NONE, Modifier::Auto));
        // Once only.
        assert!(!h.modifiers(NONE, Modifier::Auto));
    }

    #[test]
    fn quick_tap_picks_on_focus() {
        let mut h = Hold::default();
        h.enter();
        assert!(h.modifiers(NONE, Modifier::Auto));
    }

    #[test]
    fn all_held_modifiers_must_be_released() {
        let mut h = Hold::default();
        h.enter();
        let both = Mods { ctrl: true, ..ALT };
        assert!(!h.modifiers(both, Modifier::Auto));
        assert!(!h.modifiers(ALT, Modifier::Auto));
        assert!(h.modifiers(NONE, Modifier::Auto));
    }

    #[test]
    fn configured_modifier_only() {
        let mut h = Hold::default();
        h.enter();
        let both = Mods { logo: true, ..ALT };
        assert!(!h.modifiers(both, Modifier::Super));
        // Alt is not the configured one: still holding Super.
        assert!(!h.modifiers(Mods { alt: false, ..both }, Modifier::Super));
        assert!(h.modifiers(ALT, Modifier::Super));
        // Super configured but not held when focus arrives: pick.
        h.enter();
        assert!(h.modifiers(ALT, Modifier::Super));
    }

    #[test]
    fn keys() {
        let k = |sym, shift| Action::from_key(sym, 0, shift);
        assert_eq!(k(Keysym::Escape, false), Some(Action::Cancel));
        assert_eq!(k(Keysym::Return, false), Some(Action::Pick));
        assert_eq!(k(Keysym::Tab, false), Some(Action::Step(Step::NextApp)));
        assert_eq!(k(Keysym::Tab, true), Some(Action::Step(Step::PrevApp)));
        assert_eq!(
            k(Keysym::ISO_Left_Tab, true),
            Some(Action::Step(Step::PrevApp))
        );
        assert_eq!(k(Keysym::Left, false), Some(Action::Step(Step::PrevApp)));
        assert_eq!(k(Keysym::Down, false), Some(Action::Step(Step::NextWindow)));
        assert_eq!(k(Keysym::a, false), None);
        // The grave key by keycode, whatever the layout maps it to.
        let grave = |sym, shift| Action::from_key(sym, KEY_GRAVE, shift);
        assert_eq!(
            grave(Keysym::grave, false),
            Some(Action::Step(Step::NextWindow))
        );
        assert_eq!(
            grave(Keysym::asciitilde, true),
            Some(Action::Step(Step::PrevWindow))
        );
        assert_eq!(
            grave(Keysym::section, false),
            Some(Action::Step(Step::NextWindow))
        );
        assert!(Action::Step(Step::NextApp).repeats() && !Action::Pick.repeats());
    }
}
