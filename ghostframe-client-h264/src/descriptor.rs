//! DRM descriptor parsing. Filled in by Task 4.
//!
//! For now this holds only the plain-data boundary types that `lib.rs`
//! re-exports: no ffmpeg types, so `ghostframe-client-gpu` can be tested
//! against synthetic planes with no decoder in the loop.

/// One plane's position inside the dmabuf.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneDesc {
    pub offset: u64,
    pub pitch: u64,
}

/// An NV12 dmabuf: one fd, luma plane, chroma plane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmabufPlanes {
    /// Borrowed from the mapped `AVFrame`, which owns it. Do not close.
    pub fd: i32,
    pub modifier: u64,
    pub width: u32,
    pub height: u32,
    pub luma: PlaneDesc,
    /// Half width, half height, two bytes per sample (U and V interleaved).
    pub chroma: PlaneDesc,
}
