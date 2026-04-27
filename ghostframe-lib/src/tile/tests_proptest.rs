//! Property-based invariants for `TileGrid` and `DirtyTracker`.
//!
//! These encode rules from §4.1 / §4.2 of the spec so that the M3 classifier
//! work — which builds on top of these primitives — has a regression net.

use proptest::prelude::*;

use super::proptest_strategies::dim;
use super::{TileGrid, BPP, TILE_BYTES, TILE_SIZE};

// ── TileGrid ────────────────────────────────────────────────────────────────

proptest! {
    /// Inv 1: tile_count == ceil(w/32) * ceil(h/32) for every dimension.
    #[test]
    fn tile_grid_count_matches_ceil_div(w in dim(), h in dim()) {
        let grid = TileGrid::new(w, h);
        let expected = w.div_ceil(TILE_SIZE) * h.div_ceil(TILE_SIZE);
        prop_assert_eq!(grid.tile_count(), expected);
        prop_assert_eq!(grid.cols, w.div_ceil(TILE_SIZE));
        prop_assert_eq!(grid.rows, h.div_ceil(TILE_SIZE));
    }

    /// Inv 1b: iter_coords yields exactly tile_count distinct (col, row) pairs,
    /// each within the half-open ranges [0, cols) and [0, rows).
    #[test]
    fn tile_grid_iter_coords_complete(w in dim(), h in dim()) {
        let grid = TileGrid::new(w, h);
        let coords: Vec<_> = grid.iter_coords().collect();
        prop_assert_eq!(coords.len() as u32, grid.tile_count());

        let mut sorted = coords.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), coords.len(), "iter_coords yielded duplicates");

        for &(col, row) in &coords {
            prop_assert!(col < grid.cols, "col {} out of range (cols={})", col, grid.cols);
            prop_assert!(row < grid.rows, "row {} out of range (rows={})", row, grid.rows);
        }
    }

    /// Inv 2: extract_tile is total — never panics for any in-range coord
    /// regardless of how short the pixel buffer or how odd the stride is.
    #[test]
    fn extract_tile_never_panics(
        w in dim(),
        h in dim(),
        stride_pad in 0u32..=8,
        buf_trunc in 0usize..=16,
    ) {
        let grid = TileGrid::new(w, h);
        let stride = w * BPP + stride_pad * BPP;
        let full_len = (stride * h) as usize;
        let actual_len = full_len.saturating_sub(buf_trunc);
        let pixels = vec![0u8; actual_len];

        for (tx, ty) in grid.iter_coords() {
            let tile = grid.extract_tile(&pixels, stride, tx, ty);
            prop_assert_eq!(tile.len(), TILE_BYTES);
        }
    }

    /// Inv 8: stride independence — padded vs packed produces identical
    /// extracted tiles for the same per-pixel data.
    #[test]
    fn extract_tile_stride_independent(
        w in dim(),
        h in dim(),
        pad_px in 1u32..=8,
    ) {
        // Build packed and padded buffers carrying identical visible pixels.
        let stride_packed = w * BPP;
        let stride_padded = (w + pad_px) * BPP;

        // Deterministic pixel content so the two buffers carry the same data.
        let mut packed = vec![0u8; (stride_packed * h) as usize];
        let mut padded = vec![0u8; (stride_padded * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = ((x ^ y) & 0xFF) as u8;
                let off_packed = (y * stride_packed + x * BPP) as usize;
                let off_padded = (y * stride_padded + x * BPP) as usize;
                packed[off_packed..off_packed + 4].copy_from_slice(&[v, v.wrapping_add(1), v.wrapping_add(2), 255]);
                padded[off_padded..off_padded + 4].copy_from_slice(&[v, v.wrapping_add(1), v.wrapping_add(2), 255]);
            }
        }

        let grid = TileGrid::new(w, h);
        for (tx, ty) in grid.iter_coords() {
            let a = grid.extract_tile(&packed, stride_packed, tx, ty);
            let b = grid.extract_tile(&padded, stride_padded, tx, ty);
            prop_assert_eq!(a, b);
        }
    }
}
