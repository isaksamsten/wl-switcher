//! The windows-mode strip: the selected app's windows as a vertical column
//! of cards. Pure layout, unit tested; overlay.rs places the subsurfaces
//! and runs the morph between the bar and the strip.
//!
//! Cards are centered horizontally: on the output, or on `set_center_x`
//! (the slot the bar makes for the strip). All share one height: the
//! configured card height, or less so the whole column fits the output,
//! but not below `MIN_CARD`; past that, the column scrolls. A card's width
//! follows its window's aspect ratio, capped at the widest allowed (a
//! capped card gets shorter instead).
//!
//! Vertically, a column that fits is centered on the bar's center (clamped
//! to the output), and stays put while the selection moves. A scrolling one
//! keeps the selected card centered on the output, clamped at the ends.
//! Either way, `offset_for` gives the column's top, which the overlay
//! follows with a spring.
//!
//! The gaps leave room for the selected card's zoom, so a zoomed card never
//! overlaps its neighbors.

use crate::bar::Rect;

/// The aspect ratio (width / height) of a window whose size isn't known.
pub const DEFAULT_ASPECT: f64 = 16.0 / 10.0;
/// Aspect ratios are clamped to this range, so an odd window can't make a
/// sliver of a card.
const ASPECTS: (f64, f64) = (0.25, 4.0);
/// Cards never get shorter than this (unless configured smaller).
pub const MIN_CARD: f64 = 96.0;
/// Morph delay per card of distance from the selected one, as a fraction
/// of the morph.
pub const STAGGER: f64 = 0.05;

pub struct Params {
    /// Each window's aspect ratio (width / height), in strip order.
    pub aspects: Vec<f64>,
    /// Wanted card height.
    pub card_height: f64,
    /// Widest a card gets.
    pub max_width: f64,
    pub gap: f64,
    /// Zoom factor of the selected card.
    pub zoom: f64,
    /// Where a column that fits is centered vertically.
    pub center_y: f64,
    /// Output size.
    pub output: (f64, f64),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    /// Unzoomed, with `x` relative to the column's center and `y` to its
    /// top.
    cards: Vec<Rect>,
    /// The column's height.
    pub height: f64,
    margin: f64,
    output_h: f64,
    center_x: f64,
    center_y: f64,
    zoom: f64,
}

/// The aspect ratio to lay out: `a`, sanitized.
fn aspect(a: f64) -> f64 {
    if a.is_finite() && a > 0.0 {
        a.clamp(ASPECTS.0, ASPECTS.1)
    } else {
        DEFAULT_ASPECT
    }
}

impl Layout {
    pub fn new(p: &Params) -> Self {
        let n = p.aspects.len().max(1) as f64;
        let (out_w, out_h) = p.output;
        let margin = (out_h * 0.05).round().max(8.0);
        let avail = (out_h - 2.0 * margin).max(1.0);
        let zoom = p.zoom.max(1.0);
        // A gap grows by half the zoom growth: the zoom is shared between
        // at most two neighbors, a full card's worth in total.
        let grow = (zoom - 1.0) / 2.0;
        // (+ 1: the gap is rounded up.)
        let fits = (avail - (n - 1.0) * (p.gap + 1.0)) / (n + (n - 1.0) * grow);
        let min = MIN_CARD.min(p.card_height);
        // Rounded so the rounded column still fits.
        let h = p.card_height.min(fits).max(min).max(1.0).floor();
        let gap = (p.gap + h * grow).ceil();
        let max_w = p.max_width.max(1.0);

        let mut cards = Vec::with_capacity(p.aspects.len());
        let mut y = 0.0;
        for &a in &p.aspects {
            let a = aspect(a);
            let (mut w, mut ch) = (h * a, h);
            if w > max_w {
                (w, ch) = (max_w, max_w / a);
            }
            let (w, ch) = (w.round().max(1.0), ch.round().max(1.0));
            cards.push(Rect::new((-w / 2.0).round(), y, w, ch));
            y += ch + gap;
        }
        Self {
            height: (y - gap).max(0.0),
            cards,
            margin,
            output_h: out_h,
            center_x: (out_w / 2.0).round(),
            center_y: p.center_y,
            zoom,
        }
    }

    /// Center the cards on `x`.
    pub fn set_center_x(&mut self, x: f64) {
        self.center_x = x.round();
    }

    /// The widest card's width, unzoomed.
    pub fn width(&self) -> f64 {
        self.cards.iter().map(|c| c.w).fold(0.0, f64::max)
    }

    pub fn len(&self) -> usize {
        self.cards.len()
    }

    /// Whether the whole column fits the output.
    pub fn fits(&self) -> bool {
        self.height <= self.output_h - 2.0 * self.margin
    }

    /// The column's top with card `i` selected.
    pub fn offset_for(&self, i: usize) -> f64 {
        let (lo, hi) = (self.margin, self.output_h - self.margin - self.height);
        if self.fits() {
            return (self.center_y - self.height / 2.0).clamp(lo, hi).round();
        }
        let Some(c) = self.cards.get(i) else {
            return lo;
        };
        // Scrolling: hi < lo here.
        (self.output_h / 2.0 - (c.y + c.h / 2.0))
            .clamp(hi, lo)
            .round()
    }

    /// Card `i`, unzoomed, with the column's top at `offset`.
    pub fn card(&self, i: usize, offset: f64) -> Rect {
        let c = self.cards[i];
        Rect::new(self.center_x + c.x, c.y + offset, c.w, c.h)
    }

    /// How much card `i` is zoomed with the selection at `sel` (fractional
    /// while it moves).
    pub fn zoom_of(&self, i: usize, sel: f64) -> f64 {
        1.0 + (self.zoom - 1.0) * (1.0 - (i as f64 - sel).abs()).max(0.0)
    }

    /// Card `i`, zoomed for the selection at `sel`.
    pub fn zoomed(&self, i: usize, offset: f64, sel: f64) -> Rect {
        self.card(i, offset).scaled_center(self.zoom_of(i, sel))
    }

    /// Where the selection is drawn with it at `sel`: between the two
    /// cards around it while it moves.
    pub fn selection(&self, sel: f64, offset: f64) -> Option<Rect> {
        let last = self.cards.len().checked_sub(1)?;
        let sel = sel.clamp(0.0, last as f64);
        let (a, b) = (sel.floor() as usize, sel.ceil() as usize);
        let (ra, rb) = (self.zoomed(a, offset, sel), self.zoomed(b, offset, sel));
        Some(ra.lerp(rb, sel - a as f64))
    }

    /// The card at (`x`, `y`).
    pub fn hit(&self, x: f64, y: f64, offset: f64) -> Option<usize> {
        (0..self.cards.len()).find(|&i| self.card(i, offset).contains(x, y))
    }

    /// The column's bounds: the widest card's width, its full height.
    pub fn bounds(&self, offset: f64) -> Rect {
        let x0 = self.cards.iter().map(|c| c.x).fold(f64::INFINITY, f64::min);
        let x1 = self
            .cards
            .iter()
            .map(|c| c.right())
            .fold(f64::NEG_INFINITY, f64::max);
        if x1 <= x0 {
            return Rect::default();
        }
        Rect::new(self.center_x + x0, offset, x1 - x0, self.height)
    }
}

/// The morph progress of a card `k` cards away from the selected one, at
/// overall progress `morph`: cards further away start later and catch up.
pub fn stagger(morph: f64, k: usize) -> f64 {
    let delay = (k as f64 * STAGGER).min(0.5);
    ((morph - delay) / (1.0 - delay)).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(n: usize) -> Params {
        Params {
            aspects: vec![16.0 / 9.0; n],
            card_height: 0.22 * 1080.0,
            max_width: 0.5 * 1920.0,
            gap: 16.0,
            zoom: 1.09,
            center_y: 540.0,
            output: (1920.0, 1080.0),
        }
    }

    #[test]
    fn few_windows_get_the_full_height_centered() {
        let l = Layout::new(&params(3));
        assert!(l.fits());
        let h = (0.22f64 * 1080.0).floor();
        let off = l.offset_for(0);
        for i in 0..3 {
            let c = l.card(i, off);
            assert_eq!(c.h, h);
            assert_eq!(c.w, (h * 16.0 / 9.0).round());
            assert!((c.x + c.w / 2.0 - 960.0).abs() <= 0.5);
        }
        // Centered on center_y, and moving the selection doesn't move it.
        assert!((off + l.height / 2.0 - 540.0).abs() <= 1.0);
        assert_eq!(l.offset_for(2), off);
        // Neighbors don't overlap, even zoomed.
        for i in 0..2 {
            let (a, b) = (l.zoomed(i, off, i as f64), l.zoomed(i + 1, off, i as f64));
            assert!(a.bottom() < b.y, "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_column_that_fits_stays_on_the_output() {
        let mut p = params(2);
        p.center_y = 1050.0;
        let l = Layout::new(&p);
        let off = l.offset_for(0);
        assert!(off + l.height <= 1080.0 - 8.0);
        p.center_y = 0.0;
        let l = Layout::new(&p);
        assert!(l.offset_for(0) >= 8.0);
    }

    #[test]
    fn more_windows_shrink_the_cards_to_fit() {
        let l = Layout::new(&params(5));
        assert!(l.fits());
        assert!(l.card(0, 0.0).h < 0.22 * 1080.0);
        assert!(l.card(0, 0.0).h >= MIN_CARD);
    }

    #[test]
    fn many_windows_scroll_keeping_the_selection_centered() {
        let l = Layout::new(&params(12));
        assert!(!l.fits());
        assert_eq!(l.card(0, 0.0).h, MIN_CARD);
        // The middle ones are centered on the output.
        let off = l.offset_for(6);
        let c = l.card(6, off);
        assert!((c.y + c.h / 2.0 - 540.0).abs() <= 1.0);
        // The ends stop at the margins.
        let first = l.card(0, l.offset_for(0));
        let last = l.card(11, l.offset_for(11));
        assert!(first.y > 0.0 && first.y < 540.0 - first.h / 2.0);
        assert!(last.bottom() < 1080.0 && last.bottom() > 540.0 + last.h / 2.0);
        // Every selection is fully visible.
        for i in 0..12 {
            let c = l.zoomed(i, l.offset_for(i), i as f64);
            assert!(c.y >= 0.0 && c.bottom() <= 1080.0, "{i}: {c:?}");
        }
    }

    #[test]
    fn widths_follow_aspects_and_are_capped() {
        let mut p = params(3);
        p.aspects = vec![0.5, 10.0, f64::NAN];
        p.max_width = 768.0;
        let l = Layout::new(&p);
        let h = (0.22f64 * 1080.0).floor();
        let tall = l.card(0, 0.0);
        assert_eq!((tall.w, tall.h), ((h * 0.5).round(), h));
        // 10 is clamped to 4, then capped at the widest: shorter.
        let wide = l.card(1, 0.0);
        assert_eq!(wide.w, 768.0);
        assert_eq!(wide.h, 192.0);
        let unknown = l.card(2, 0.0);
        assert_eq!(unknown.w, (h * DEFAULT_ASPECT).round());
        // Stacked by their own heights.
        assert_eq!(unknown.y, wide.bottom() + (wide.y - tall.bottom()));
    }

    #[test]
    fn selection_moves_between_cards() {
        let l = Layout::new(&params(3));
        let off = l.offset_for(0);
        assert_eq!(l.selection(1.0, off), Some(l.zoomed(1, off, 1.0)));
        let mid = l.selection(0.5, off).unwrap();
        let (a, b) = (l.card(0, off), l.card(1, off));
        assert!(mid.y > a.y && mid.y < b.y);
        // Zoom: full on the selection, none a card away.
        assert_eq!(l.zoom_of(1, 1.0), 1.09);
        assert_eq!(l.zoom_of(0, 1.0), 1.0);
        // assert!((l.zoom_of(0, 0.5) - 1.02).abs() < 1e-9);
        assert_eq!(Layout::new(&params(0)).selection(0.0, 0.0), None);
    }

    #[test]
    fn hits() {
        let l = Layout::new(&params(3));
        let off = l.offset_for(0);
        let c = l.card(1, off);
        assert_eq!(l.hit(c.x + 1.0, c.y + 1.0, off), Some(1));
        assert_eq!(l.hit(c.x + 1.0, c.y - 1.0, off), None);
        assert_eq!(l.hit(c.x - 1.0, c.y + 1.0, off), None);
        let b = l.bounds(off);
        assert!(b.contains(c.x + 1.0, c.y - 1.0));
    }

    #[test]
    fn recentered() {
        let mut p = params(2);
        p.aspects = vec![1.0, 2.0];
        let mut l = Layout::new(&p);
        let h = (0.22f64 * 1080.0).floor();
        assert_eq!(l.width(), (2.0 * h).round());
        l.set_center_x(400.0);
        for i in 0..2 {
            let c = l.card(i, 0.0);
            assert!((c.center().0 - 400.0).abs() <= 0.5, "{c:?}");
        }
        let b = l.bounds(0.0);
        assert!((b.center().0 - 400.0).abs() <= 0.5 && b.w == l.width());
    }

    #[test]
    fn staggered_morph() {
        assert_eq!(stagger(0.0, 0), 0.0);
        assert_eq!(stagger(0.5, 0), 0.5);
        assert_eq!(stagger(1.0, 0), 1.0);
        // Later cards start later, all finish together.
        assert_eq!(stagger(0.05, 1), 0.0);
        assert!(stagger(0.5, 2) < stagger(0.5, 1));
        assert_eq!(stagger(1.0, 30), 1.0);
        assert!(stagger(0.9, 30) > 0.0);
    }
}
