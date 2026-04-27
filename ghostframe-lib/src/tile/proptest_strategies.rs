//! Reusable `proptest` strategies for tile-level testing.
//!
//! Frames are produced as `(pixels, stride, width, height)` tuples in the same
//! shape `DirtyTracker::update` expects. Sizes are bounded so generated frames
//! stay under ~1 MB per case and shrinking is fast.

use proptest::collection::vec;
use proptest::prelude::*;

use super::{BPP, TILE_SIZE};

/// Maximum frame dimension generated. 256 = up to 8x8 tiles, plenty to
/// exercise edge tiles, multiple rows, and damage-hint subsets while keeping
/// each case small.
pub const MAX_DIM: u32 = 256;

/// A generated BGRA frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// Number of bytes the buffer must hold for `(stride, height)`.
    pub fn buf_len(&self) -> usize {
        (self.stride * self.height) as usize
    }
}

/// Generate a dimension in `[1, MAX_DIM]`.
pub fn dim() -> impl Strategy<Value = u32> {
    1u32..=MAX_DIM
}

/// Generate a frame with stride exactly `width * BPP` (no padding).
pub fn frame_packed() -> impl Strategy<Value = Frame> {
    (dim(), dim()).prop_flat_map(|(width, height)| {
        let stride = width * BPP;
        let len = (stride * height) as usize;
        vec(any::<u8>(), len).prop_map(move |pixels| Frame {
            pixels,
            stride,
            width,
            height,
        })
    })
}

/// Generate a frame with arbitrary stride padding in `[0, 16]` pixels per row.
pub fn frame_padded() -> impl Strategy<Value = Frame> {
    (dim(), dim(), 0u32..=16).prop_flat_map(|(width, height, pad_px)| {
        let stride = (width + pad_px) * BPP;
        let len = (stride * height) as usize;
        vec(any::<u8>(), len).prop_map(move |pixels| Frame {
            pixels,
            stride,
            width,
            height,
        })
    })
}

/// Generate a damage-hint set as a (possibly empty) Vec of `(tile_x, tile_y)`
/// pairs bounded to the grid implied by `(width, height)`.
pub fn damage_hints(width: u32, height: u32) -> impl Strategy<Value = Vec<(u32, u32)>> {
    let cols = width.div_ceil(TILE_SIZE);
    let rows = height.div_ceil(TILE_SIZE);
    let total = (cols * rows) as usize;
    let max_hints = total.min(64);
    vec((0..cols, 0..rows), 0..=max_hints)
}
