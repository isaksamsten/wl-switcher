//! Text with cosmic-text: the title pill and the letter tiles that stand in
//! for missing icons. Everything is rendered at buffer scale into
//! premultiplied ARGB8888 images.
//!
//! cosmic-text shapes with font fallback (CJK, emoji) and rasterizes with
//! swash. It has no ellipsizing, so titles are cut at the longest prefix that
//! still fits with a trailing "…".

use cosmic_text::{Attrs, Buffer, Color, Family, FontSystem, Metrics, Shaping, SwashCache, Wrap};

use crate::pixels::Image;

/// Pill padding in logical pixels.
pub const PAD_X: i32 = 12;
pub const PAD_Y: i32 = 6;
/// Titles are cut to the thumbnail's width, but at least this wide (logical).
pub const MIN_PILL_WIDTH: i32 = 160;

/// Title pill appearance.
#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    pub font: String,
    /// Logical pixels.
    pub size: f64,
    /// 0xRRGGBBAA, not premultiplied.
    pub color: u32,
    pub background: u32,
}

/// Line height in logical pixels.
pub fn line_height(font_size: f64) -> i32 {
    (font_size * 1.4).ceil() as i32
}

/// Logical pill height: one line plus padding. Fixed by the font size alone,
/// so the layout can reserve it before any font is loaded.
pub fn pill_height(font_size: f64) -> i32 {
    line_height(font_size) + 2 * PAD_Y
}

/// A rendered title pill.
pub struct Pill {
    /// Logical size.
    pub width: i32,
    pub height: i32,
    /// `width` x `height` times the scale.
    pub image: Image,
}

/// The font system and glyph cache. Loading the system fonts takes a while,
/// so this is created on the worker thread.
pub struct Text {
    fonts: FontSystem,
    glyphs: SwashCache,
}

impl Text {
    pub fn new() -> Self {
        Self {
            fonts: FontSystem::new(),
            glyphs: SwashCache::new(),
        }
    }

    /// Shape one line of `text` (physical pixels); returns it and its width.
    fn shape(&mut self, text: &str, font: &str, size: f32, line_h: f32) -> (Buffer, f32) {
        let mut buffer = Buffer::new(&mut self.fonts, Metrics::new(size, line_h));
        buffer.set_wrap(&mut self.fonts, Wrap::None);
        buffer.set_size(&mut self.fonts, None, None);
        let attrs = Attrs::new().family(family(font));
        buffer.set_text(&mut self.fonts, text, &attrs, Shaping::Advanced, None);
        let width = buffer
            .layout_runs()
            .map(|run| run.line_w)
            .fold(0.0, f32::max);
        (buffer, width)
    }

    /// Draw `buffer` at (`x`, `y`) onto `img`, blending over its pixels.
    fn draw(&mut self, buffer: &Buffer, img: &mut Image, x: i32, y: i32, color: u32) {
        let [r, g, b, a] = color.to_be_bytes();
        let (w, h) = (img.width as i32, img.height as i32);
        buffer.draw(
            &mut self.fonts,
            &mut self.glyphs,
            Color::rgba(r, g, b, a),
            |gx, gy, gw, gh, c| {
                let alpha = c.a() as f64 / 255.0;
                if alpha <= 0.0 {
                    return;
                }
                let src = [c.b(), c.g(), c.r()].map(|v| v as f64 * alpha);
                for py in (y + gy).max(0)..(y + gy + gh as i32).min(h) {
                    for px in (x + gx).max(0)..(x + gx + gw as i32).min(w) {
                        let i = ((py * w + px) * 4) as usize;
                        blend(&mut img.data[i..i + 4], src, alpha);
                    }
                }
            },
        );
    }

    /// The title in a fully rounded pill, at most `max_width` logical pixels
    /// wide, rendered at `scale`.
    pub fn pill(&mut self, title: &str, style: &Style, max_width: i32, scale: i32) -> Pill {
        let s = scale.max(1);
        let size = (style.size * s as f64) as f32;
        let line_h = (line_height(style.size) * s) as f32;
        let max_text = ((max_width - 2 * PAD_X).max(1) * s) as f32;
        let title = clean(title);
        let mut measure = |t: &str| self.shape(t, &style.font, size, line_h).1;
        let text = ellipsize(&title, max_text, &mut measure);
        let (buffer, text_w) = self.shape(&text, &style.font, size, line_h);

        let width = ((text_w / s as f32).ceil() as i32 + 2 * PAD_X).min(max_width.max(2 * PAD_X));
        let height = pill_height(style.size);
        let mut image = rounded_rect(
            width * s,
            height * s,
            (height * s) as f64 / 2.0,
            style.background,
        );
        let x = ((width * s) as f32 - text_w) / 2.0;
        self.draw(
            &buffer,
            &mut image,
            x.round() as i32,
            PAD_Y * s,
            style.color,
        );
        Pill {
            width,
            height,
            image,
        }
    }

    /// A rounded tile of `size` pixels with the first letter of `name`,
    /// colored after `name`.
    pub fn letter(&mut self, name: &str, size: u32, font: &str) -> Image {
        let size = size.max(1) as i32;
        let letter: String = name
            .chars()
            .find(|c| c.is_alphanumeric())
            .map_or_else(|| "?".into(), |c| c.to_uppercase().collect());
        let mut image = rounded_rect(size, size, size as f64 * 0.22, tile_color(name));
        let font_size = size as f32 * 0.55;
        let line_h = (font_size * 1.3).ceil();
        let (buffer, w) = self.shape(&letter, font, font_size, line_h);
        let x = ((size as f32 - w) / 2.0).round() as i32;
        let y = ((size as f32 - line_h) / 2.0).round() as i32;
        self.draw(&buffer, &mut image, x, y, 0xffffffff);
        image
    }
}

/// CSS generic family names, or a family name.
fn family(name: &str) -> Family<'_> {
    match name {
        "sans-serif" | "sans" => Family::SansSerif,
        "serif" => Family::Serif,
        "monospace" => Family::Monospace,
        "cursive" => Family::Cursive,
        "fantasy" => Family::Fantasy,
        _ => Family::Name(name),
    }
}

/// Premultiplied `src` (B, G, R, 0-255) with `alpha` over a BGRA pixel.
fn blend(dst: &mut [u8], src: [f64; 3], alpha: f64) {
    let keep = 1.0 - alpha;
    for k in 0..3 {
        dst[k] = (src[k] + dst[k] as f64 * keep).round().min(255.0) as u8;
    }
    dst[3] = (alpha * 255.0 + dst[3] as f64 * keep).round().min(255.0) as u8;
}

/// One line: control characters (newlines, tabs) become spaces.
fn clean(title: &str) -> String {
    let s: String = title
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    s.trim().to_string()
}

/// `text`, or its longest prefix with "…" appended whose `measure` fits in
/// `max`. At least the ellipsis.
pub fn ellipsize(text: &str, max: f32, measure: &mut dyn FnMut(&str) -> f32) -> String {
    if measure(text) <= max {
        return text.to_string();
    }
    let ends: Vec<usize> = text.char_indices().map(|(i, _)| i).skip(1).collect();
    let with_ellipsis = |end: usize| format!("{}…", text[..end].trim_end());
    // Width grows with the prefix; find the last end that fits.
    let (mut lo, mut hi) = (0, ends.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        if measure(&with_ellipsis(ends[mid - 1])) <= max {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    match lo {
        0 => "…".to_string(),
        n => with_ellipsis(ends[n - 1]),
    }
}

/// A `w`x`h` image of an antialiased rounded rect with corner radius `r`,
/// filled with `color` (0xRRGGBBAA, not premultiplied).
pub fn rounded_rect(w: i32, h: i32, r: f64, color: u32) -> Image {
    let (w, h) = (w.max(1), h.max(1));
    let r = r.clamp(0.0, w.min(h) as f64 / 2.0);
    let [cr, cg, cb, ca] = color.to_be_bytes().map(|c| c as f64 / 255.0);
    let (hw, hh) = (w as f64 / 2.0, h as f64 / 2.0);
    let mut data = Vec::with_capacity((w * h * 4) as usize);
    let c = |v: f64, a: f64| (v * a * 255.0).round() as u8;
    let solid = [c(cb, ca), c(cg, ca), c(cr, ca), c(1.0, ca)];
    for y in 0..h {
        let dy = ((y as f64 + 0.5 - hh).abs() - (hh - r)).max(0.0);
        for x in 0..w {
            // Signed distance from the pixel center to the rounded rect.
            let dx = ((x as f64 + 0.5 - hw).abs() - (hw - r)).max(0.0);
            if dx == 0.0 && dy == 0.0 && r >= 0.5 {
                data.extend_from_slice(&solid); // outside the corners
                continue;
            }
            let d = (dx * dx + dy * dy).sqrt() - r;
            let a = ca * (0.5 - d).clamp(0.0, 1.0);
            data.extend_from_slice(&[c(cb, a), c(cg, a), c(cr, a), c(1.0, a)]);
        }
    }
    Image {
        width: w as u32,
        height: h as u32,
        data,
    }
}

/// A muted color picked by hashing `name` (0xRRGGBBAA).
fn tile_color(name: &str) -> u32 {
    let hash = name.bytes().fold(0x811c_9dc5_u32, |h, b| {
        (h ^ b as u32).wrapping_mul(0x0100_0193)
    });
    let hue = (hash % 360) as f64;
    let (s, l) = (0.45, 0.45);
    // HSL to RGB.
    let c = (1.0 - (2.0 * l - 1.0_f64).abs()) * s;
    let x = c * (1.0 - ((hue / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match hue as u32 / 60 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    let byte = |v: f64| ((v + m) * 255.0).round() as u32;
    byte(r) << 24 | byte(g) << 16 | byte(b) << 8 | 0xff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every char is 10 wide.
    fn measure(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn ellipsis() {
        let e = |t: &str, max: f32| ellipsize(t, max, &mut measure);
        assert_eq!(e("hello", 50.0), "hello");
        assert_eq!(e("hello world", 50.0), "hell…");
        // Trailing spaces before the ellipsis are dropped.
        assert_eq!(e("ab cdef", 40.0), "ab…");
        assert_eq!(e("hello", 5.0), "…");
        assert_eq!(e("日本語のタイトル", 40.0), "日本語…");
        for max in 0..200 {
            let max = max as f32;
            let out = e("The quick brown fox jumps", max);
            assert!(measure(&out) <= max.max(10.0), "{max}: {out}");
        }
    }

    #[test]
    fn cleaned() {
        assert_eq!(clean(" a\tb\nc "), "a b c");
    }

    #[test]
    fn pill_rect() {
        let img = rounded_rect(40, 20, 10.0, 0xff0000ff);
        assert_eq!(img.data.len(), 40 * 20 * 4);
        let px = |x: u32, y: u32| &img.data[((y * 40 + x) * 4) as usize..][..4];
        // Center is opaque red (B, G, R, A), corners are clear, ends are round.
        assert_eq!(px(20, 10), [0, 0, 255, 255]);
        assert_eq!(px(0, 0)[3], 0);
        assert_eq!(px(39, 19)[3], 0);
        assert!(px(0, 10)[3] > 100);
        // Symmetric.
        for y in 0..20 {
            for x in 0..40 {
                assert_eq!(px(x, y), px(39 - x, y));
                assert_eq!(px(x, y), px(x, 19 - y));
            }
        }
    }

    #[test]
    fn colors() {
        assert_eq!(tile_color("firefox"), tile_color("firefox"));
        assert_eq!(tile_color("x") & 0xff, 0xff);
        let mut dst = [0, 0, 0, 0];
        blend(&mut dst, [255.0, 0.0, 0.0], 1.0);
        assert_eq!(dst, [255, 0, 0, 255]);
        let mut dst = [0, 0, 200, 200];
        blend(&mut dst, [0.0, 0.0, 0.0], 0.5);
        assert_eq!(dst, [0, 0, 100, 228]);
    }

    #[test]
    fn heights() {
        assert_eq!(line_height(14.0), 20);
        assert_eq!(pill_height(14.0), 32);
    }
}
