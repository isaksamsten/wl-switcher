//! Rendered images: premultiplied ARGB8888 pixels, ready for shm buffers.

/// A decoded or rendered image as premultiplied ARGB8888 (little endian: B,
/// G, R, A), packed rows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>,
}
