//! The apps-mode bar: a row of app icons on a rounded panel. Pure layout,
//! unit tested; overlay.rs places the subsurfaces.
//!
//! The panel is centered horizontally, its center at `position` of the
//! output height. Icons shrink (down to `MIN_ICON`) to keep the panel within
//! `max_width` of the output; past that, the row scrolls, and the overlay
//! keeps the selection in view with a spring on the offset.
//!
//! Vertically, from the top: padding, the icons, the window dots, the
//! selected app's label, padding. The highlight covers the icon and its
//! dots.

/// Icons never shrink below this (unless configured smaller).
pub const MIN_ICON: f64 = 48.0;

/// A rect in output-local logical pixels.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub fn new(x: f64, y: f64, w: f64, h: f64) -> Self {
        Self { x, y, w, h }
    }

    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }

    pub fn right(&self) -> f64 {
        self.x + self.w
    }

    pub fn bottom(&self) -> f64 {
        self.y + self.h
    }
}

pub struct Params {
    pub apps: usize,
    /// Configured icon size.
    pub icon_size: f64,
    pub gap: f64,
    /// Widest panel, as a fraction of the output width.
    pub max_width: f64,
    /// Vertical center, as a fraction of the output height.
    pub position: f64,
    /// Label height and widest label; 0 without labels.
    pub label: (f64, f64),
    pub dots: bool,
    /// Output size.
    pub output: (f64, f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub panel: Rect,
    pub panel_radius: f64,
    /// The icon size used.
    pub icon: f64,
    /// How far the highlight reaches around an icon.
    pub highlight_pad: f64,
    pub highlight_radius: f64,
    /// Distance from one icon to the next.
    pub step: f64,
    /// Where icon 0 starts at scroll offset 0.
    pub row_x: f64,
    pub icon_y: f64,
    /// Window dots: their top and size; 0 without dots.
    pub dots_y: f64,
    pub dot: f64,
    /// Label: its top; its height is the `label` param's.
    pub label_y: f64,
    /// What stays visible horizontally when the row scrolls: the panel
    /// minus its padding, plus room for the highlight.
    pub clip: (f64, f64),
    /// Scroll offsets go from 0 to this.
    pub max_scroll: f64,
    apps: usize,
    pad: f64,
}

impl Layout {
    pub fn new(p: &Params) -> Self {
        let n = p.apps.max(1) as f64;
        let (out_w, out_h) = p.output;
        let hl_pad = (p.icon_size * 0.1).round();
        let pad = hl_pad + (p.gap / 2.0).round().max(4.0);
        let row = |s: f64| n * s + (n - 1.0) * p.gap;
        let max_w = (out_w * p.max_width).floor().max(1.0);
        let min_icon = MIN_ICON.min(p.icon_size);
        let fits = |s: f64| row(s) + 2.0 * pad <= max_w;
        let icon = if fits(p.icon_size) {
            p.icon_size
        } else {
            ((max_w - 2.0 * pad - (n - 1.0) * p.gap) / n)
                .floor()
                .max(min_icon)
        };
        let content = row(icon);
        let (label_h, label_w) = p.label;
        let inner = content.max(label_w).min(max_w - 2.0 * pad).max(icon);
        let panel_w = inner + 2.0 * pad;

        let dot = if p.dots {
            (icon * 0.06).round().max(4.0)
        } else {
            0.0
        };
        let dot_gap = if p.dots { (hl_pad / 2.0).round() } else { 0.0 };
        let label_gap = if label_h > 0.0 { 6.0 } else { 0.0 };
        // Relative to the panel's top.
        let icon_y = pad;
        let dots_y = icon_y + icon + dot_gap;
        let cell_bottom = dots_y + dot;
        let label_y = cell_bottom + hl_pad + label_gap;
        let panel_h = label_y + label_h + (pad - hl_pad);

        let panel_x = ((out_w - panel_w) / 2.0).round();
        let center = out_h * p.position;
        let panel_y = (center - panel_h / 2.0)
            .min(out_h - panel_h)
            .max(0.0)
            .round();
        Self {
            panel: Rect::new(panel_x, panel_y, panel_w, panel_h),
            panel_radius: (icon * 0.2).round() + pad - hl_pad,
            icon,
            highlight_pad: hl_pad,
            highlight_radius: (icon * 0.2).round(),
            step: icon + p.gap,
            row_x: panel_x + pad + ((inner - content) / 2.0).max(0.0).round(),
            icon_y: panel_y + icon_y,
            dots_y: panel_y + dots_y,
            dot,
            label_y: panel_y + label_y,
            clip: (panel_x + pad - hl_pad, panel_x + panel_w - pad + hl_pad),
            max_scroll: (content - inner).max(0.0),
            apps: p.apps,
            pad,
        }
    }

    /// Icon `i` at scroll offset `scroll`.
    pub fn icon_rect(&self, i: usize, scroll: f64) -> Rect {
        let x = self.row_x + i as f64 * self.step - scroll;
        Rect::new(x, self.icon_y, self.icon, self.icon)
    }

    /// The highlight around an icon whose left edge is at `x`.
    pub fn highlight_rect(&self, x: f64) -> Rect {
        let p = self.highlight_pad;
        let bottom = self.dots_y + self.dot + p;
        Rect::new(
            x - p,
            self.icon_y - p,
            self.icon + 2.0 * p,
            bottom - (self.icon_y - p),
        )
    }

    /// The scroll offset that brings icon `i` into view from `current`,
    /// keeping some of its neighbors visible where there is room.
    pub fn scroll_for(&self, i: usize, current: f64) -> f64 {
        if self.max_scroll <= 0.0 {
            return 0.0;
        }
        let inner = (self.panel.x + self.pad, self.panel.right() - self.pad);
        let margin = (self.step / 2.0)
            .min((inner.1 - inner.0 - self.icon) / 2.0)
            .max(0.0);
        let r = self.icon_rect(i, current);
        let mut scroll = current;
        if r.x - margin < inner.0 {
            scroll -= inner.0 - (r.x - margin);
        } else if r.right() + margin > inner.1 {
            scroll += r.right() + margin - inner.1;
        }
        scroll.clamp(0.0, self.max_scroll)
    }

    /// The app whose icon (or the space around it) is at (`x`, `y`).
    pub fn hit(&self, x: f64, y: f64, scroll: f64) -> Option<usize> {
        let cell = self.highlight_rect(0.0);
        if x < self.clip.0 || x >= self.clip.1 || y < cell.y || y >= cell.bottom() {
            return None;
        }
        let gap = self.step - self.icon;
        let i = ((x - self.row_x + scroll + gap / 2.0) / self.step).floor();
        (i >= 0.0 && (i as usize) < self.apps).then_some(i as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(apps: usize) -> Params {
        Params {
            apps,
            icon_size: 96.0,
            gap: 16.0,
            max_width: 0.9,
            position: 0.5,
            label: (20.0, 0.0),
            dots: true,
            output: (1920.0, 1080.0),
        }
    }

    #[test]
    fn centered_full_size() {
        let l = Layout::new(&params(3));
        assert_eq!(l.icon, 96.0);
        assert_eq!(l.max_scroll, 0.0);
        let p = l.panel;
        assert_eq!(p.x + p.w / 2.0, 960.0);
        assert!((p.y + p.h / 2.0 - 540.0).abs() <= 1.0);
        // Icons evenly spaced, inside the panel with equal margins.
        let (first, last) = (l.icon_rect(0, 0.0), l.icon_rect(2, 0.0));
        assert_eq!(last.x - first.x, 2.0 * (96.0 + 16.0));
        assert_eq!(first.x - p.x, p.right() - last.right());
        // Top to bottom: icon, dots, label, all inside the panel.
        assert!(first.y > p.y && l.dots_y >= first.bottom());
        assert!(l.label_y >= l.dots_y + l.dot && l.label_y + 20.0 < p.bottom());
        let h = l.highlight_rect(first.x);
        assert!(h.y < first.y && h.bottom() > l.dots_y + l.dot && h.bottom() <= l.label_y);
    }

    #[test]
    fn compact_without_labels_and_dots() {
        let mut p = params(2);
        (p.label, p.dots) = ((0.0, 0.0), false);
        let l = Layout::new(&p);
        let icon = l.icon_rect(0, 0.0);
        // Symmetric padding.
        assert_eq!(icon.y - l.panel.y, l.panel.bottom() - icon.bottom());
    }

    #[test]
    fn wide_labels_widen_the_panel() {
        let mut p = params(1);
        p.label = (20.0, 300.0);
        let l = Layout::new(&p);
        assert!(l.panel.w >= 300.0);
        let icon = l.icon_rect(0, 0.0);
        assert_eq!(icon.x + icon.w / 2.0, 960.0);
        assert_eq!(l.max_scroll, 0.0);
    }

    #[test]
    fn shrinks_then_scrolls() {
        let l = Layout::new(&params(20));
        assert!(l.icon < 96.0 && l.icon >= MIN_ICON, "{}", l.icon);
        assert!(l.panel.w <= 1920.0 * 0.9);
        assert_eq!(l.max_scroll, 0.0);

        let l = Layout::new(&params(60));
        assert_eq!(l.icon, MIN_ICON);
        assert!(l.panel.w <= 1920.0 * 0.9);
        assert!(l.max_scroll > 0.0);
        let last = l.icon_rect(59, l.max_scroll);
        assert!(last.right() <= l.clip.1);
    }

    #[test]
    fn small_icons_do_not_grow() {
        let mut p = params(200);
        p.icon_size = 32.0;
        assert_eq!(Layout::new(&p).icon, 32.0);
    }

    #[test]
    fn scrolling_keeps_the_selection_in_view() {
        let l = Layout::new(&params(60));
        let inside = |i: usize, s: f64| {
            let r = l.icon_rect(i, s);
            r.x >= l.clip.0 && r.right() <= l.clip.1
        };
        let mut s = 0.0;
        for i in (0..60).chain((0..60).rev()) {
            s = l.scroll_for(i, s);
            assert!(inside(i, s), "{i} at {s}");
            assert!((0.0..=l.max_scroll).contains(&s));
        }
        // Already visible: no change.
        assert_eq!(l.scroll_for(3, 0.0), 0.0);
        let s = l.scroll_for(59, 0.0);
        assert_eq!(s, l.max_scroll);
        assert_eq!(l.scroll_for(58, s), s);
    }

    #[test]
    fn hits() {
        let l = Layout::new(&params(3));
        let r = l.icon_rect(1, 0.0);
        assert_eq!(l.hit(r.x + 1.0, r.y + 1.0, 0.0), Some(1));
        // The gap is split between neighbors.
        assert_eq!(l.hit(r.x - 7.0, r.y, 0.0), Some(1));
        assert_eq!(l.hit(r.x - 9.0, r.y, 0.0), Some(0));
        assert_eq!(l.hit(r.x, l.panel.y - 1.0, 0.0), None);
        assert_eq!(l.hit(l.panel.x - 1.0, r.y, 0.0), None);
        assert_eq!(l.hit(l.icon_rect(2, 0.0).right() + 9.0, r.y, 0.0), None);
        // Scrolled.
        let l = Layout::new(&params(60));
        let r = l.icon_rect(30, 500.0);
        assert_eq!(l.hit(r.x + 1.0, r.y + 1.0, 500.0), Some(30));
    }
}
