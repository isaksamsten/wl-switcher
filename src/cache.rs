//! The daemon's buffer cache: everything a show attaches that does not
//! depend on the show itself, as ready `WlBuffer`s, so the first frame
//! renders nothing.
//!
//! - Icons and app name labels, by key (one per app and output scale).
//!   They are rendered by the worker; `App` turns the results into buffers
//!   as they arrive.
//! - Shapes: the panel with its shadow, the highlight and the window dots,
//!   rendered here on first use for a size, color and scale. The most
//!   recently used few are kept.
//!
//! Buffers are only destroyed between shows (`trim`): one replaced during a
//! show may still be attached, or about to be with the next parent commit.

use std::collections::HashMap;

use smithay_client_toolkit::shm::raw::RawPool;
use wayland_client::{
    QueueHandle,
    protocol::{wl_buffer::WlBuffer, wl_shm},
};

use crate::app::App;
use crate::globals::{Globals, Ignore};
use crate::icons;
use crate::pixels::Image;
use crate::shadow;
use crate::text;
use crate::worker::LabelKey;

/// Shapes kept between shows.
const MAX_SHAPES: usize = 12;

/// A buffer and its logical size.
#[derive(Clone, Debug)]
pub struct Sized {
    pub buffer: WlBuffer,
    pub width: i32,
    pub height: i32,
}

/// A rendered shape: its buffer, and where it goes relative to what it
/// was rendered for (a panel's shadow reaches past the panel).
#[derive(Clone, Debug)]
pub struct Shape {
    pub buffer: WlBuffer,
    /// Logical pixels.
    pub rect: shadow::Rect,
}

/// What a shape was rendered for.
#[derive(Clone, Debug, PartialEq)]
pub enum ShapeKey {
    Panel {
        w: i32,
        h: i32,
        radius: i32,
        color: u32,
        shadow: Option<shadow::Settings>,
        scale: i32,
    },
    /// A rounded rect.
    Rect {
        w: i32,
        h: i32,
        radius: i32,
        color: u32,
        scale: i32,
    },
    /// A row of `count` round dots of `size`, `size` apart.
    Dots {
        count: u32,
        size: i32,
        color: u32,
        scale: i32,
    },
}

impl ShapeKey {
    fn render(&self) -> (Image, shadow::Rect) {
        let rect = |w, h| shadow::Rect { x: 0, y: 0, w, h };
        match *self {
            ShapeKey::Panel {
                w,
                h,
                radius,
                color,
                shadow,
                scale,
            } => {
                let p = shadow::panel(w, h, radius as f64, color, shadow.as_ref(), scale);
                (p.image, p.rect)
            }
            ShapeKey::Rect {
                w,
                h,
                radius,
                color,
                scale,
            } => {
                let img = text::rounded_rect(w * scale, h * scale, (radius * scale) as f64, color);
                (img, rect(w, h))
            }
            ShapeKey::Dots {
                count,
                size,
                color,
                scale,
            } => {
                let count = count.max(1) as i32;
                let w = (2 * count - 1) * size;
                (dots(count, size * scale, color), rect(w, size))
            }
        }
    }
}

/// `count` round dots of `size` pixels, `size` apart, in one row.
fn dots(count: i32, size: i32, color: u32) -> Image {
    let size = size.max(1);
    let dot = text::rounded_rect(size, size, size as f64 / 2.0, color);
    let w = ((2 * count - 1) * size) as u32;
    let mut data = vec![0; (w * size as u32 * 4) as usize];
    let row = size as usize * 4;
    for k in 0..count as usize {
        for y in 0..size as usize {
            let dst = (y * w as usize + k * 2 * size as usize) * 4;
            data[dst..dst + row].copy_from_slice(&dot.data[y * row..(y + 1) * row]);
        }
    }
    Image {
        width: w,
        height: size as u32,
        data,
    }
}

#[derive(Default)]
pub struct Cache {
    icons: HashMap<icons::Key, WlBuffer>,
    labels: Vec<(LabelKey, Sized)>,
    /// Most recently used first.
    shapes: Vec<(ShapeKey, Shape)>,
    /// Replaced buffers, destroyed by `trim`.
    stale: Vec<WlBuffer>,
}

impl Cache {
    pub fn icon(&self, key: &icons::Key) -> Option<WlBuffer> {
        self.icons.get(key).cloned()
    }

    pub fn insert_icon(
        &mut self,
        g: &Globals,
        key: icons::Key,
        img: &Image,
        qh: &QueueHandle<App>,
    ) {
        match shm_buffer(g, img, qh) {
            Ok(buffer) => {
                if let Some(old) = self.icons.insert(key, buffer) {
                    self.stale.push(old);
                }
            }
            Err(e) => eprintln!("wl-switcher: icon buffer: {e}"),
        }
    }

    pub fn label(&self, key: &LabelKey) -> Option<&Sized> {
        self.labels.iter().find(|(k, _)| k == key).map(|(_, s)| s)
    }

    /// The widest cached label of these.
    pub fn widest_label<'a>(&self, keys: impl IntoIterator<Item = &'a LabelKey>) -> i32 {
        keys.into_iter()
            .filter_map(|k| self.label(k))
            .map(|s| s.width)
            .max()
            .unwrap_or(0)
    }

    pub fn insert_label(
        &mut self,
        g: &Globals,
        key: LabelKey,
        pill: &text::Pill,
        qh: &QueueHandle<App>,
    ) {
        let buffer = match shm_buffer(g, &pill.image, qh) {
            Ok(buffer) => buffer,
            Err(e) => return eprintln!("wl-switcher: label buffer: {e}"),
        };
        let sized = Sized {
            buffer,
            width: pill.width,
            height: pill.height,
        };
        match self.labels.iter_mut().find(|(k, _)| *k == key) {
            Some((_, old)) => self.stale.push(std::mem::replace(old, sized).buffer),
            None => self.labels.push((key, sized)),
        }
    }

    /// The shape for `key`, rendered now if it isn't cached.
    pub fn shape(&mut self, g: &Globals, key: ShapeKey, qh: &QueueHandle<App>) -> Option<Shape> {
        if let Some(i) = self.shapes.iter().position(|(k, _)| *k == key) {
            let entry = self.shapes.remove(i);
            let shape = entry.1.clone();
            self.shapes.insert(0, entry);
            return Some(shape);
        }
        let (img, rect) = key.render();
        let buffer = shm_buffer(g, &img, qh)
            .inspect_err(|e| eprintln!("wl-switcher: shape buffer: {e}"))
            .ok()?;
        let shape = Shape { buffer, rect };
        self.shapes.insert(0, (key, shape.clone()));
        Some(shape)
    }

    /// Forget icons and labels, e.g. because the config or the desktop
    /// entries changed.
    pub fn clear_icons(&mut self) {
        self.stale.extend(self.icons.drain().map(|(_, b)| b));
        self.stale
            .extend(self.labels.drain(..).map(|(_, s)| s.buffer));
    }

    /// Forget shapes, e.g. because the config changed.
    pub fn clear_shapes(&mut self) {
        self.stale
            .extend(self.shapes.drain(..).map(|(_, s)| s.buffer));
    }

    /// Between shows: destroy replaced buffers and the least recently used
    /// shapes.
    pub fn trim(&mut self) {
        while self.shapes.len() > MAX_SHAPES {
            if let Some((_, s)) = self.shapes.pop() {
                self.stale.push(s.buffer);
            }
        }
        for buffer in self.stale.drain(..) {
            buffer.destroy();
        }
    }
}

/// An shm buffer holding `img`. The pool is gone once this returns; the
/// buffer keeps the memory alive.
pub fn shm_buffer(g: &Globals, img: &Image, qh: &QueueHandle<App>) -> std::io::Result<WlBuffer> {
    let len = img.data.len();
    if len > i32::MAX as usize || len != img.width as usize * img.height as usize * 4 || len == 0 {
        return Err(std::io::Error::other("bad buffer size"));
    }
    let mut pool = RawPool::new(len, &g.shm).map_err(std::io::Error::other)?;
    pool.mmap()[..len].copy_from_slice(&img.data);
    Ok(pool.create_buffer(
        0,
        img.width as i32,
        img.height as i32,
        img.width as i32 * 4,
        wl_shm::Format::Argb8888,
        Ignore,
        qh,
    ))
}

/// A 1x1 buffer of an 0xRRGGBBAA color, for scaling with a viewport.
pub fn color_buffer(g: &Globals, rgba: u32, qh: &QueueHandle<App>) -> std::io::Result<WlBuffer> {
    // Both buffer kinds take premultiplied alpha.
    let [r, gr, b, a] = rgba.to_be_bytes().map(|c| c as f64 / 255.0);
    let [r, gr, b] = [r, gr, b].map(|c| c * a);
    if let Some(single_pixel) = &g.single_pixel {
        let c = |v: f64| (v * u32::MAX as f64) as u32;
        return Ok(single_pixel.create_u32_rgba_buffer(c(r), c(gr), c(b), c(a), qh, Ignore));
    }
    let c = |v: f64| (v * 255.0).round() as u8;
    let img = Image {
        width: 1,
        height: 1,
        data: vec![c(b), c(gr), c(r), c(a)],
    };
    shm_buffer(g, &img, qh)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dot_row() {
        let img = dots(3, 4, 0xffffffff);
        assert_eq!((img.width, img.height), (20, 4));
        let alpha = |x: u32, y: u32| img.data[((y * 20 + x) * 4 + 3) as usize];
        // Dot centers are opaque, the spaces between them clear.
        for k in 0..3 {
            assert_eq!(alpha(k * 8 + 2, 2), 255);
        }
        assert_eq!(alpha(5, 2), 0);
        assert_eq!(alpha(13, 2), 0);
    }

    #[test]
    fn shape_sizes() {
        let key = ShapeKey::Dots {
            count: 2,
            size: 5,
            color: 0xffffffff,
            scale: 2,
        };
        let (img, rect) = key.render();
        assert_eq!((rect.w, rect.h), (15, 5));
        assert_eq!((img.width, img.height), (30, 10));
        let key = ShapeKey::Rect {
            w: 30,
            h: 20,
            radius: 4,
            color: 0xffffffff,
            scale: 3,
        };
        let (img, rect) = key.render();
        assert_eq!((rect.w, rect.h), (30, 20));
        assert_eq!((img.width, img.height), (90, 60));
    }
}
