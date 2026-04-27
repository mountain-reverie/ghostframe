//! Property-based invariants for `TileGrid` and `DirtyTracker`.
//!
//! These encode rules from §4.1 / §4.2 of the spec so that the M3 classifier
//! work — which builds on top of these primitives — has a regression net.

use proptest::prelude::*;

use super::proptest_strategies::{dim, frame_packed, MAX_DIM};
use super::{DirtyTracker, TileGrid, BPP, TILE_BYTES, TILE_SIZE};

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

// ── DirtyTracker ────────────────────────────────────────────────────────────

proptest! {
    /// Inv 3 (idempotence): same frame submitted twice → second update is empty.
    #[test]
    fn dirty_same_frame_twice_clean(frame in frame_packed()) {
        let grid = TileGrid::new(frame.width, frame.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);
        let again = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);
        prop_assert!(again.is_empty(), "expected no dirty tiles, got {:?}", again);
    }

    /// Inv 4 (first-frame): first update reports every tile coord exactly once.
    #[test]
    fn dirty_first_frame_all_dirty_unique(frame in frame_packed()) {
        let grid = TileGrid::new(frame.width, frame.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let dirty = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);

        prop_assert_eq!(dirty.len() as u32, grid.tile_count());

        let mut sorted = dirty.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), dirty.len(), "first-frame dirty list had duplicates");
    }

    /// Inv 5 (hint subset): update_with_hints only reports tiles in the
    /// hint set, even when un-hinted tiles also have changed pixels.
    #[test]
    fn dirty_with_hints_is_subset(
        frame_a in frame_packed(),
        // delta dirties many tiles indiscriminately so the test exercises
        // both hinted-and-changed and un-hinted-but-changed cases.
        delta in proptest::collection::vec(any::<u8>(), 0..1024),
        // hint_mask drives a strict subset of the grid through cycle().
        hint_mask in proptest::collection::vec(any::<bool>(), 1..=64usize),
    ) {
        let grid = TileGrid::new(frame_a.width, frame_a.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame_a.pixels, frame_a.stride, frame_a.width, frame_a.height);

        // frame_b: same buffer with bytes overwritten — likely dirties most tiles.
        let mut buf = frame_a.pixels.clone();
        for (i, b) in delta.iter().enumerate().take(buf.len()) {
            buf[i] = *b;
        }

        // Build the hint subset by zip+cycle of the bool mask over all coords.
        let all_coords: Vec<(u32, u32)> = grid.iter_coords().collect();
        let hints: Vec<(u32, u32)> = all_coords.iter()
            .zip(hint_mask.iter().cycle())
            .filter_map(|(c, &keep)| if keep { Some(*c) } else { None })
            .collect();
        let hint_set: std::collections::HashSet<(u32, u32)> = hints.iter().copied().collect();

        let dirty = tracker.update_with_hints(
            &buf, frame_a.stride, frame_a.width, frame_a.height, &hints,
        );

        for d in &dirty {
            prop_assert!(
                hint_set.contains(d),
                "{:?} reported but not in hint set {:?}",
                d, hints
            );
        }
    }

    /// Inv 6 (no_commit idempotent): two consecutive update_no_commit calls
    /// with the same frame return identical dirty sets.
    #[test]
    fn dirty_no_commit_idempotent(
        frame_a in frame_packed(),
        frame_b_seed in any::<u8>(),
    ) {
        let grid = TileGrid::new(frame_a.width, frame_a.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame_a.pixels, frame_a.stride, frame_a.width, frame_a.height);

        // Frame B differs by overwriting one pixel with a derived value.
        let mut buf = frame_a.pixels.clone();
        if !buf.is_empty() {
            buf[0] = buf[0].wrapping_add(frame_b_seed.max(1));
        }

        let first = tracker.update_no_commit(&buf, frame_a.stride, frame_a.width, frame_a.height);
        let second = tracker.update_no_commit(&buf, frame_a.stride, frame_a.width, frame_a.height);
        prop_assert_eq!(first, second);
    }

    /// Inv 7 (resize-on-mismatch): submitting a frame at a new (cols, rows)
    /// resets prior state, so the next update reports every tile dirty.
    #[test]
    fn dirty_resize_clears_state(
        frame_a in frame_packed(),
        // prop_assume! below filters cases where the new grid coincidentally
        // matches the original tile dimensions.
        new_w in (TILE_SIZE * 2)..=MAX_DIM,
        new_h in (TILE_SIZE * 2)..=MAX_DIM,
    ) {
        prop_assume!(
            new_w.div_ceil(TILE_SIZE) != frame_a.width.div_ceil(TILE_SIZE)
                || new_h.div_ceil(TILE_SIZE) != frame_a.height.div_ceil(TILE_SIZE)
        );

        let grid_a = TileGrid::new(frame_a.width, frame_a.height);
        let mut tracker = DirtyTracker::new(grid_a.cols, grid_a.rows);
        let _ = tracker.update(&frame_a.pixels, frame_a.stride, frame_a.width, frame_a.height);

        let stride_b = new_w * BPP;
        let buf_b = vec![0u8; (stride_b * new_h) as usize];
        let dirty = tracker.update(&buf_b, stride_b, new_w, new_h);

        let grid_b = TileGrid::new(new_w, new_h);
        prop_assert_eq!(dirty.len() as u32, grid_b.tile_count());

        let mut sorted = dirty.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), dirty.len(), "resize first-frame dirty list had duplicates");
    }

    /// Inv 5 supplement: any hint outside the grid is silently dropped.
    #[test]
    fn dirty_with_hints_out_of_range_dropped(
        frame in frame_packed(),
        bogus_x in (MAX_DIM / TILE_SIZE + 8)..(MAX_DIM / TILE_SIZE + 32),
        bogus_y in (MAX_DIM / TILE_SIZE + 8)..(MAX_DIM / TILE_SIZE + 32),
    ) {
        let grid = TileGrid::new(frame.width, frame.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);

        let hints = vec![(bogus_x, bogus_y)];
        let dirty = tracker.update_with_hints(
            &frame.pixels, frame.stride, frame.width, frame.height, &hints,
        );
        prop_assert!(dirty.is_empty(), "out-of-range hint produced dirty tiles: {:?}", dirty);
    }
}

proptest! {
    /// Inv 8 supplement: DirtyTracker on padded-stride frames produces the
    /// same dirty set as on the equivalent packed-stride frame.
    ///
    /// Builds two buffers carrying identical visible pixels — one packed
    /// (stride == width * BPP) and one with row padding — and asserts both
    /// trackers see the same per-tile change pattern.
    #[test]
    fn dirty_packed_vs_padded_match(
        w in dim(),
        h in dim(),
        pad_px in 1u32..=8,
        flip_x in 0u32..MAX_DIM,
        flip_y in 0u32..MAX_DIM,
    ) {
        let stride_packed = w * BPP;
        let stride_padded = (w + pad_px) * BPP;

        let mut packed = vec![0u8; (stride_packed * h) as usize];
        let mut padded = vec![0u8; (stride_padded * h) as usize];

        // Frame A — all zeros (already initialised). Frame B — flip one pixel.
        let fx = flip_x % w;
        let fy = flip_y % h;
        let off_packed = (fy * stride_packed + fx * BPP) as usize;
        let off_padded = (fy * stride_padded + fx * BPP) as usize;

        let grid = TileGrid::new(w, h);
        let mut t_packed = DirtyTracker::new(grid.cols, grid.rows);
        let mut t_padded = DirtyTracker::new(grid.cols, grid.rows);

        // Settle on frame A.
        let _ = t_packed.update(&packed, stride_packed, w, h);
        let _ = t_padded.update(&padded, stride_padded, w, h);

        // Apply the same visible pixel change to both.
        packed[off_packed] = 0xFF;
        padded[off_padded] = 0xFF;

        let dirty_packed = t_packed.update(&packed, stride_packed, w, h);
        let dirty_padded = t_padded.update(&padded, stride_padded, w, h);

        prop_assert_eq!(dirty_packed, dirty_padded);
    }
}
