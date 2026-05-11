//! Reusable `proptest` strategies for tile-level testing.
//!
//! Frames are produced as `(pixels, stride, width, height)` tuples in the same
//! shape `DirtyTracker::update` expects. Sizes are bounded so generated frames
//! stay under ~1 MB per case and shrinking is fast.

use proptest::collection::vec;
use proptest::prelude::*;
use proptest::sample::subsequence;

use super::{CodecState, TileMetrics, BPP, EDGE_DENSITY_UNKNOWN, TILE_SIZE, UNIQUE_COLORS_UNKNOWN};

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
    #[allow(dead_code)]
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

/// Generate a frame with arbitrary stride padding in `[1, 16]` pixels per row.
#[allow(dead_code)]
pub fn frame_padded() -> impl Strategy<Value = Frame> {
    (dim(), dim(), 1u32..=16).prop_flat_map(|(width, height, pad_px)| {
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
/// pairs bounded to the grid implied by `(width, height)`. All returned pairs
/// are unique — no duplicate coordinates.
#[allow(dead_code)]
pub fn damage_hints(width: u32, height: u32) -> impl Strategy<Value = Vec<(u32, u32)>> {
    let cols = width.div_ceil(TILE_SIZE);
    let rows = height.div_ceil(TILE_SIZE);
    let total = (cols * rows) as usize;
    let max_hints = total.min(64);
    let all_coords: Vec<(u32, u32)> = (0..rows)
        .flat_map(|r| (0..cols).map(move |c| (c, r)))
        .collect();
    subsequence(all_coords, 0..=max_hints)
}

/// A `CodecState` strategy. PalRle and Cdf53 carry randomized inner fields.
pub fn codec_state() -> impl Strategy<Value = CodecState> {
    prop_oneof![
        Just(CodecState::Skip),
        any::<u32>().prop_map(|f| CodecState::H264 { frames_in_h264: f }),
        Just(CodecState::Bc1),
        any::<u8>().prop_map(|p| CodecState::PalRle { palette_id: p }),
        Just(CodecState::Solid),
        (any::<u8>(), 1u8..=9u8).prop_map(|(s, m)| {
            CodecState::Cdf53 {
                passes_sent: s.min(m),
                max_passes: m,
            }
        }),
        Just(CodecState::PixelPerfect),
    ]
}

/// A `TileMetrics` strategy with sentinel `unique_colors` and `edge_density`
/// (matching M3.0 reality where GPU-derived fields aren't populated yet).
pub fn tile_metrics() -> impl Strategy<Value = TileMetrics> {
    (
        0.0f32..=120.0, // change_freq_hz
        0.0f32..=1.0,   // change_magnitude
        0u32..=600,     // idle_frames
        codec_state(),
    )
        .prop_map(|(freq, mag, idle, st)| TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            unique_colors: UNIQUE_COLORS_UNKNOWN,
            edge_density: EDGE_DENSITY_UNKNOWN,
            idle_frames: idle,
            codec_state: st,
        })
}

/// A `TileMetrics` strategy with a known `unique_colors` value (no sentinel).
/// Used by rule-table invariants that depend on the colour-count axis.
pub fn tile_metrics_with_colors() -> impl Strategy<Value = TileMetrics> {
    (
        0.0f32..=120.0,
        0.0f32..=1.0,
        0u32..=600,
        1u16..=256,
        codec_state(),
    )
        .prop_map(|(freq, mag, idle, uc, st)| TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            unique_colors: uc,
            edge_density: EDGE_DENSITY_UNKNOWN,
            idle_frames: idle,
            codec_state: st,
        })
}
