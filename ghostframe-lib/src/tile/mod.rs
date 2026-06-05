pub const TILE_SIZE: u32 = 32;
pub const BPP: u32 = 4;
pub const TILE_BYTES: usize = (TILE_SIZE * TILE_SIZE * BPP) as usize;

/// Sentinel value for `TileMetrics::unique_colors` indicating the GPU compute
/// estimator has not run yet (M3.0 always uses this — backing lands in M3.3).
/// Classifier rules consulting `unique_colors` treat the sentinel as "unknown".
pub const UNIQUE_COLORS_UNKNOWN: u16 = u16::MAX;

/// Sentinel value for `TileMetrics::edge_density` indicating the GPU compute
/// estimator has not run yet (M3.0 always uses this — backing lands in M3.3).
/// Classifier rules consulting `edge_density` treat NaN as "unknown".
pub const EDGE_DENSITY_UNKNOWN: f32 = f32::NAN;

/// Per-tile metrics consumed by the classifier. See
/// `docs/specs/ghostframe-initial-spec.md` §4.2.
///
/// Note: `PartialEq` is intentionally partial — two default-initialized
/// `TileMetrics` values compare unequal because `edge_density` is NaN.
/// Callers testing equality must check `edge_density` with `.is_nan()`
/// separately when the sentinel may be in effect.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TileMetrics {
    /// Exponential moving average of change frequency (Hz). Alpha = 0.1.
    /// Updated each frame: `freq = freq * 0.9 + (changed ? 60.0 : 0.0) * 0.1`.
    pub change_freq_hz: f32,

    /// SAD of the most recent change normalized to [0.0, 1.0] per pixel.
    /// 0.0 means identical, 1.0 means every pixel maximally different.
    /// In M3.0 this field is always 0.0 — `MetricsTracker::record_frame` only
    /// updates `change_freq_hz` and `idle_frames` from the dirty bitmap.
    /// Populated in M3.1+ from GPU SAD output.
    pub change_magnitude: f32,

    /// Approximate distinct color count from a hash-based GPU estimator.
    /// On the CPU path stays at `UNIQUE_COLORS_UNKNOWN`; populated on the GPU
    /// path by `tile_analysis.comp` (pre-M3.2 phase onwards).
    pub unique_colors: u16,

    /// Fraction of pixels with high gradient. On the CPU path stays at
    /// `EDGE_DENSITY_UNKNOWN`; populated on the GPU path by `tile_analysis.comp`
    /// (pre-M3.2 phase onwards).
    pub edge_density: f32,

    /// Consecutive frames this tile has been clean (not in the dirty set).
    /// Reset to 0 when the tile becomes dirty.
    pub idle_frames: u32,

    /// Last-decided per-tile codec state. Acts as hysteresis input on the
    /// next call to `classify_tile()` — the classifier reads it before
    /// overwriting it.
    pub codec_state: CodecState,

    /// Has this tile been escalated to CDF 5/3 refinement during its current
    /// generation? Set by `IoBridge`'s per-frame escalation sweep when it
    /// enqueues refinement passes. Reset by `Scheduler::bump_generation`'s
    /// callback (or equivalent path in io_bridge) whenever the generation
    /// changes. Prevents the sweep from re-enqueueing the same tile every
    /// frame after escalation while it's still idle.
    pub already_escalated_this_gen: bool,
}

impl Default for TileMetrics {
    fn default() -> Self {
        Self {
            change_freq_hz: 0.0,
            change_magnitude: 0.0,
            unique_colors: UNIQUE_COLORS_UNKNOWN,
            edge_density: EDGE_DENSITY_UNKNOWN,
            idle_frames: 0,
            codec_state: CodecState::Skip,
            already_escalated_this_gen: false,
        }
    }
}

/// Per-tile codec assignment chosen by the classifier. Variant set is fixed
/// by the source spec. In M3.0 the classifier may return any of these values,
/// but the io_bridge emission layer maps every non-Skip variant to
/// `Codec::Raw` on the wire because no real per-tile encoders exist until
/// M3.1+.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecState {
    Skip,
    H264 { frames_in_h264: u32 },
    /// PalRLE-classified tile.
    ///
    /// `palette_id` semantics depend on lifecycle position (design D8):
    /// - **Pre-Phase-A** (immediately after `classify_tile`): `palette_id == 0`
    ///   is a *feasibility placeholder* meaning "this tile is PalRLE-feasible,
    ///   no persistent slot allocated yet".
    /// - **Post-Phase-A** (overwritten by `IoBridge::phase_a_palette_allocation`):
    ///   `palette_id` is the real persistent slot id 0..=255, used by Phase B
    ///   to encode the wire payload and by the client to look up the palette.
    ///
    /// The boundary is clear in code: Phase A is the only thing that
    /// overwrites this field. Other code paths that read `CodecState::PalRle`
    /// outside the Phase-A boundary should treat `palette_id` as the real id.
    PalRle { palette_id: u8 },
    Solid,
    Cdf53 { passes_sent: u8, max_passes: u8 },
    PixelPerfect,
}

/// Whole-frame emission mode chosen by the classifier per frame_seq.
/// `H264` means the full-frame VA-API encoder runs; `TileCodec` means each
/// dirty tile is emitted as its own tile datagram.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameMode {
    H264,
    TileCodec,
}

pub mod metrics_tracker;
pub use metrics_tracker::MetricsTracker;

pub mod classifier;
pub use classifier::{Classifier, CostModel};

pub mod escalation;
pub use escalation::detect_escalation_candidates;

#[derive(Debug, Clone)]
pub struct TileGrid {
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
}

impl TileGrid {
    pub fn new(width: u32, height: u32) -> Self {
        let cols = width.div_ceil(TILE_SIZE);
        let rows = height.div_ceil(TILE_SIZE);
        Self {
            width,
            height,
            cols,
            rows,
        }
    }

    pub fn tile_count(&self) -> u32 {
        self.cols * self.rows
    }

    /// Extract a 32x32 tile at grid position (tile_x, tile_y) from a packed pixel buffer.
    ///
    /// `stride` is the number of bytes per row in the source buffer.
    /// Edge tiles are zero-padded to the full `TILE_BYTES` size.
    pub fn extract_tile(&self, pixels: &[u8], stride: u32, tile_x: u32, tile_y: u32) -> Vec<u8> {
        let mut out = vec![0u8; TILE_BYTES];

        let origin_x = tile_x * TILE_SIZE;
        let origin_y = tile_y * TILE_SIZE;

        // How many pixels this tile actually covers (may be less at edges).
        let copy_w = (self.width.saturating_sub(origin_x)).min(TILE_SIZE);
        let copy_h = (self.height.saturating_sub(origin_y)).min(TILE_SIZE);

        for row in 0..copy_h {
            let src_row = origin_y + row;
            let src_offset = (src_row * stride + origin_x * BPP) as usize;
            let dst_offset = (row * TILE_SIZE * BPP) as usize;
            let row_bytes = (copy_w * BPP) as usize;
            // Only copy if source data is within bounds
            if src_offset + row_bytes <= pixels.len() {
                out[dst_offset..dst_offset + row_bytes]
                    .copy_from_slice(&pixels[src_offset..src_offset + row_bytes]);
            }
        }

        out
    }

    /// Iterate all tile coordinates (col, row) in row-major order.
    pub fn iter_coords(&self) -> impl Iterator<Item = (u32, u32)> {
        let cols = self.cols;
        let rows = self.rows;
        (0..rows).flat_map(move |row| (0..cols).map(move |col| (col, row)))
    }
}

/// Tracks which tiles changed between consecutive frames.
///
/// Uses a single flat `Vec<u8>` buffer to store all previous tile data,
/// avoiding one heap allocation per tile. On the first frame (when the
/// buffer is empty) every tile is reported as dirty.
pub struct DirtyTracker {
    cols: u32,
    rows: u32,
    prev_tiles: Vec<u8>,
    /// True once at least one committing update (`update` or `update_with_hints`)
    /// has been called. Used to detect the transition out of QUIC slow-start mode,
    /// where prev_tiles holds all-zeros rather than actual committed pixel data.
    committed: bool,
}

impl DirtyTracker {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            prev_tiles: Vec::new(),
            committed: false,
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.prev_tiles.clear();
        self.committed = false;
    }

    /// Clear all stored state so the next frame is treated as the first
    /// (all tiles reported dirty). Used when a new client connects.
    pub fn reset(&mut self) {
        self.prev_tiles.clear();
        self.committed = false;
    }

    /// Returns true if at least one committing update has been performed.
    /// False means prev_tiles holds all-zeros (QUIC slow-start baseline),
    /// not actual captured pixel data.
    pub fn has_been_committed(&self) -> bool {
        self.committed
    }

    pub fn update(
        &mut self,
        pixels: &[u8],
        stride: u32,
        width: u32,
        height: u32,
    ) -> Vec<(u32, u32)> {
        self.update_inner(pixels, stride, width, height, true)
    }

    /// Like `update`, but don't store current tiles as prev — dirty tiles will
    /// remain dirty on the next call. Used during QUIC slow-start where
    /// datagrams may be silently dropped by congestion control.
    pub fn update_no_commit(
        &mut self,
        pixels: &[u8],
        stride: u32,
        width: u32,
        height: u32,
    ) -> Vec<(u32, u32)> {
        self.update_inner(pixels, stride, width, height, false)
    }

    fn update_inner(
        &mut self,
        pixels: &[u8],
        stride: u32,
        width: u32,
        height: u32,
        commit: bool,
    ) -> Vec<(u32, u32)> {
        let grid = TileGrid::new(width, height);
        if grid.cols != self.cols || grid.rows != self.rows {
            self.resize(grid.cols, grid.rows);
        }
        let first_frame = self.prev_tiles.is_empty();
        if first_frame {
            let count = (self.cols * self.rows) as usize;
            self.prev_tiles.resize(count * TILE_BYTES, 0);
        }
        let mut dirty = Vec::new();
        for (tile_x, tile_y) in grid.iter_coords() {
            let idx = (tile_y * self.cols + tile_x) as usize;
            let current = grid.extract_tile(pixels, stride, tile_x, tile_y);
            let prev = &self.prev_tiles[idx * TILE_BYTES..(idx + 1) * TILE_BYTES];
            if first_frame || prev != current.as_slice() {
                if commit {
                    self.prev_tiles[idx * TILE_BYTES..(idx + 1) * TILE_BYTES]
                        .copy_from_slice(&current);
                }
                dirty.push((tile_x, tile_y));
            }
        }
        if commit {
            self.committed = true;
        }
        dirty
    }

    pub fn update_with_hints(
        &mut self,
        pixels: &[u8],
        stride: u32,
        width: u32,
        height: u32,
        hints: &[(u32, u32)],
    ) -> Vec<(u32, u32)> {
        self.update_with_hints_inner(pixels, stride, width, height, hints, true)
    }

    fn update_with_hints_inner(
        &mut self,
        pixels: &[u8],
        stride: u32,
        width: u32,
        height: u32,
        hints: &[(u32, u32)],
        commit: bool,
    ) -> Vec<(u32, u32)> {
        let grid = TileGrid::new(width, height);
        if grid.cols != self.cols || grid.rows != self.rows {
            self.resize(grid.cols, grid.rows);
        }
        let first_frame = self.prev_tiles.is_empty();
        if first_frame {
            let count = (self.cols * self.rows) as usize;
            self.prev_tiles.resize(count * TILE_BYTES, 0);
        }
        let mut dirty = Vec::new();
        for &(tile_x, tile_y) in hints {
            if tile_x >= self.cols || tile_y >= self.rows {
                continue;
            }
            let idx = (tile_y * self.cols + tile_x) as usize;
            let current = grid.extract_tile(pixels, stride, tile_x, tile_y);
            let prev = &self.prev_tiles[idx * TILE_BYTES..(idx + 1) * TILE_BYTES];
            if first_frame || prev != current.as_slice() {
                if commit {
                    self.prev_tiles[idx * TILE_BYTES..(idx + 1) * TILE_BYTES]
                        .copy_from_slice(&current);
                }
                dirty.push((tile_x, tile_y));
            }
        }
        if commit {
            self.committed = true;
        }
        dirty
    }
}

#[cfg(test)]
mod proptest_strategies;

#[cfg(test)]
mod tests_proptest;

#[cfg(test)]
mod dirty_tests {
    use super::*;

    #[test]
    fn first_frame_all_dirty() {
        let mut tracker = DirtyTracker::new(2, 2);
        let frame = vec![0u8; TILE_BYTES * 4]; // 2x2 grid, all zeros
        let dirty = tracker.update(&frame, 64 * 4, 64, 64);
        assert_eq!(dirty, vec![(0, 0), (1, 0), (0, 1), (1, 1)]);
    }

    #[test]
    fn unchanged_tiles_are_clean() {
        let mut tracker = DirtyTracker::new(2, 2);
        let frame = vec![42u8; TILE_BYTES * 4];
        let _ = tracker.update(&frame, 64 * 4, 64, 64);
        let dirty = tracker.update(&frame, 64 * 4, 64, 64);
        assert!(dirty.is_empty());
    }

    #[test]
    fn changed_tile_detected() {
        let mut tracker = DirtyTracker::new(2, 2);
        let frame = vec![0u8; 64 * 64 * 4];
        let _ = tracker.update(&frame, 64 * 4, 64, 64);

        let mut frame2 = frame.clone();
        frame2[33 * 4] = 255;
        let dirty = tracker.update(&frame2, 64 * 4, 64, 64);
        assert_eq!(dirty, vec![(1, 0)]);
    }

    #[test]
    fn damage_hints_narrow_check() {
        let mut tracker = DirtyTracker::new(2, 2);
        let frame = vec![0u8; 64 * 64 * 4];
        let _ = tracker.update(&frame, 64 * 4, 64, 64);

        let mut frame2 = frame.clone();
        for b in frame2.iter_mut() {
            *b = 99;
        }
        let dirty = tracker.update_with_hints(&frame2, 64 * 4, 64, 64, &[(0, 0)]);
        assert_eq!(dirty, vec![(0, 0)]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_grid_dimensions() {
        let grid = TileGrid::new(1920, 1080);
        assert_eq!(grid.cols, 60);
        assert_eq!(grid.rows, 34);
        assert_eq!(grid.tile_count(), 2040);
    }

    #[test]
    fn tile_grid_exact_multiple() {
        let grid = TileGrid::new(640, 480);
        assert_eq!(grid.cols, 20);
        assert_eq!(grid.rows, 15);
    }

    #[test]
    fn tile_grid_small() {
        let grid = TileGrid::new(48, 48);
        assert_eq!(grid.cols, 2);
        assert_eq!(grid.rows, 2);
    }

    #[test]
    fn extract_tile_full_size() {
        // 64x64 frame; paint tile (1,0) — columns 32..64, rows 0..32 — red in BGRA.
        let stride = 64 * BPP;
        let mut pixels = vec![0u8; (64 * 64 * BPP) as usize];
        // Red in BGRA = [0, 0, 255, 255]
        for row in 0..32u32 {
            for col in 32..64u32 {
                let off = (row * stride + col * BPP) as usize;
                pixels[off] = 0; // B
                pixels[off + 1] = 0; // G
                pixels[off + 2] = 255; // R
                pixels[off + 3] = 255; // A
            }
        }

        let grid = TileGrid::new(64, 64);
        let tile = grid.extract_tile(&pixels, stride, 1, 0);

        assert_eq!(tile.len(), TILE_BYTES);
        // First pixel of extracted tile should be red BGRA.
        assert_eq!(&tile[0..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn extract_tile_bad_stride_no_panic() {
        let pixels = vec![0u8; (64 * 64 * BPP) as usize];
        let grid = TileGrid::new(64, 64);
        // Stride is intentionally too small -- extract should not panic
        let tile = grid.extract_tile(&pixels, 64, 0, 1);
        assert_eq!(tile.len(), TILE_BYTES);
    }

    #[test]
    fn extract_tile_edge_partial() {
        // 48x48 frame → tile (1,1) covers columns 32..48 (16 px wide) and rows 32..48 (16 px
        // tall), but must be padded to a full 32x32 tile.
        let stride = 48 * BPP;
        let pixels = vec![1u8; (48 * 48 * BPP) as usize];

        let grid = TileGrid::new(48, 48);
        let tile = grid.extract_tile(&pixels, stride, 1, 1);

        assert_eq!(tile.len(), TILE_BYTES);
    }

    #[test]
    fn tile_metrics_default_uses_sentinels() {
        let m = TileMetrics::default();
        assert_eq!(m.unique_colors, UNIQUE_COLORS_UNKNOWN);
        assert!(m.edge_density.is_nan());
        assert_eq!(m.codec_state, CodecState::Skip);
        assert_eq!(m.idle_frames, 0);
        assert_eq!(m.change_freq_hz, 0.0);
        // Pin the NaN reflexivity trap: defaults compare unequal because of the
        // edge_density sentinel. Task 6 callers must avoid raw assert_eq! when
        // the sentinel may be in effect.
        assert_ne!(TileMetrics::default(), TileMetrics::default());
    }

    #[test]
    fn tile_metrics_default_has_already_escalated_false() {
        let m = TileMetrics::default();
        assert!(!m.already_escalated_this_gen);
    }
}
