//! Property-based invariants for `TileGrid`, `DirtyTracker`, and the per-tile classifier.
//!
//! These encode rules from §4.1 / §4.2 of the spec so that the M3 classifier
//! work — which builds on top of these primitives — has a regression net.

use proptest::prelude::*;

use super::classifier::classify_tile;
use super::proptest_strategies::{
    codec_state, dim, frame_packed, tile_metrics, tile_metrics_with_colors, MAX_DIM,
};
use super::{
    CodecState, DirtyTracker, TileGrid, TileMetrics, BPP, EDGE_DENSITY_UNKNOWN, TILE_BYTES,
    TILE_SIZE, UNIQUE_COLORS_UNKNOWN,
};

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
        packed[off_packed..off_packed + 4].copy_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);
        padded[off_padded..off_padded + 4].copy_from_slice(&[0xFF, 0x00, 0x00, 0xFF]);

        let mut dirty_packed = t_packed.update(&packed, stride_packed, w, h);
        let mut dirty_padded = t_padded.update(&padded, stride_padded, w, h);

        dirty_packed.sort_unstable();
        dirty_padded.sort_unstable();
        prop_assert_eq!(dirty_packed, dirty_padded);
    }
}

// ── M3 classifier invariants ────────────────────────────────────────────────
//
// C1, C2, C4, C5 land in M3.0 alongside the classifier. C3 (Solid) defers to
// M3.1 (which ships the Solid encoder). C6 (H.264 → idle traverses lossy
// intermediate before refinement) defers to M3.3 (which ships refinement).

proptest! {
    /// C1 — idle_frames > 0 ⇒ CodecState::Skip
    #[test]
    fn c1_idle_implies_skip(mut m in tile_metrics(), prev in codec_state()) {
        m.idle_frames = 1; // force idle — overrides whatever strategy generated
        prop_assert_eq!(classify_tile(&m, &prev), CodecState::Skip);
    }

    /// C2 — low frequency AND low magnitude with sentinel unique_colors
    /// must not pick H264 (regardless of prior state).
    /// Note: with sentinel colours, the §4.2 path goes through the fallback
    /// (Cdf53) — never H264 — for low-freq tiles. Hysteresis on H264 only
    /// applies in the medium-freq band (5–15 Hz).
    /// Varies freq across `[0.0, 5.0)` and mag across `[0.0, 0.1)` so the
    /// sentinel-only sub-region of Rule-7/8 fall-through is fully swept.
    #[test]
    fn c2_low_freq_low_magnitude_never_h264_sentinel_colors(
        freq in 0.0f32..5.0,
        mag in 0.0f32..0.1,
        prev in codec_state(),
    ) {
        let m = TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            idle_frames: 0,
            unique_colors: UNIQUE_COLORS_UNKNOWN,
            edge_density: EDGE_DENSITY_UNKNOWN,
            codec_state: CodecState::Skip,
        };
        let next = classify_tile(&m, &prev);
        prop_assert!(!matches!(next, CodecState::H264 { .. }),
            "low-freq low-magnitude tile must never classify as H264 (got {:?} for freq={}, mag={})",
            next, freq, mag);
    }

    /// C2b — same invariant as C2 but with concrete unique_colors values, so
    /// rules 6/7 (Solid / PalRle) participate alongside the Cdf53 fallback.
    #[test]
    fn c2_low_freq_low_magnitude_never_h264_concrete_colors(
        freq in 0.0f32..5.0,
        mag in 0.0f32..0.1,
        uc in 1u16..=256,
        prev in codec_state(),
    ) {
        let m = TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            idle_frames: 0,
            unique_colors: uc,
            edge_density: EDGE_DENSITY_UNKNOWN,
            codec_state: CodecState::Skip,
        };
        let next = classify_tile(&m, &prev);
        prop_assert!(!matches!(next, CodecState::H264 { .. }),
            "low-freq low-magnitude tile must never classify as H264 (got {:?} for freq={}, mag={}, uc={})",
            next, freq, mag, uc);
    }

    /// C4 — codec selection is a pure function of (TileMetrics, prev) — same
    /// inputs always yield the same output.
    #[test]
    fn c4_classify_is_pure(m in tile_metrics(), prev in codec_state()) {
        let a = classify_tile(&m, &prev);
        let b = classify_tile(&m, &prev);
        prop_assert_eq!(a, b);
    }

    /// C5 — high-frequency low-magnitude (cursor blink) never enters H.264.
    /// Rule 3 routes these to PalRle or BC1 unconditionally.
    /// Varies freq across `(15.0, 120.0]` and mag across `[0.0, 0.3]` so the
    /// entire low-magnitude sub-band of Rule 3 is swept.
    #[test]
    fn c5_cursor_blink_pattern_never_h264_sentinel_colors(
        freq in 15.001f32..=120.0,
        mag in 0.0f32..=0.3,
        prev in codec_state(),
    ) {
        let m = TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            idle_frames: 0,
            unique_colors: UNIQUE_COLORS_UNKNOWN,
            edge_density: EDGE_DENSITY_UNKNOWN,
            codec_state: CodecState::Skip,
        };
        let next = classify_tile(&m, &prev);
        prop_assert!(!matches!(next, CodecState::H264 { .. }),
            "cursor-blink pattern must never classify as H264 (got {:?} for freq={}, mag={})",
            next, freq, mag);
    }

    /// C5b — same invariant as C5 but with concrete unique_colors so the
    /// PalRle branch of Rule 3 participates alongside the BC1 fallback.
    #[test]
    fn c5_cursor_blink_pattern_never_h264_concrete_colors(
        freq in 15.001f32..=120.0,
        mag in 0.0f32..=0.3,
        uc in 1u16..=256,
        prev in codec_state(),
    ) {
        let m = TileMetrics {
            change_freq_hz: freq,
            change_magnitude: mag,
            idle_frames: 0,
            unique_colors: uc,
            edge_density: EDGE_DENSITY_UNKNOWN,
            codec_state: CodecState::Skip,
        };
        let next = classify_tile(&m, &prev);
        prop_assert!(!matches!(next, CodecState::H264 { .. }),
            "cursor-blink pattern must never classify as H264 (got {:?} for freq={}, mag={}, uc={})",
            next, freq, mag, uc);
    }

    /// C7 — Rule 6/7/8 colour-threshold table: at low freq + low magnitude,
    /// `unique_colors == 1` ⇒ Solid, `2..=16` ⇒ PalRle, `> 16` ⇒ Cdf53 fallback.
    /// Uses `tile_metrics_with_colors` to vary the non-sentinel colour-count axis.
    #[test]
    fn c7_rule_7_color_threshold(mut m in tile_metrics_with_colors()) {
        m.change_freq_hz = 2.0;
        m.change_magnitude = 0.05;
        m.idle_frames = 0;
        let next = classify_tile(&m, &CodecState::Skip);
        if m.unique_colors == 1 {
            prop_assert_eq!(next, CodecState::Solid);
        } else if m.unique_colors <= 16 {
            prop_assert_eq!(next, CodecState::PalRle { palette_id: 0 });
        } else {
            let is_cdf53 = matches!(next, CodecState::Cdf53 { .. });
            prop_assert!(is_cdf53, "expected Cdf53 fallback, got {:?}", next);
        }
    }

    // C3 (Solid: `unique_colors == 1 ⇒ Solid`) is covered by `c7_rule_7_color_threshold`
    // above under the rule-table conditions (low freq, low magnitude, idle_frames=0).
    // An unconditional form would conflict with C1 (idle ⇒ Skip) and the high-freq
    // routing rules. M3.1 design addendum (D1) keeps the classifier sentinel-gated;
    // C3's intent is satisfied by C7 plus the new `c3_solid_dispatch_through_encoder`
    // below, which verifies the encoder is wired end-to-end.
    //
    // C6 (H264 → idle traverses lossy intermediate before refinement) — deferred
    //   to M3.3; tracked in the design doc M3.3 testing section.

    /// C3 — when classifier picks Solid for a uniform tile, the encode/decode
    /// roundtrip exactly preserves the BGRA color. This binds the classifier
    /// decision to the encoder behavior, closing the loop deferred from M3.0.
    #[test]
    fn c3_solid_dispatch_through_encoder(
        b in 0u8..=255,
        g in 0u8..=255,
        r in 0u8..=255,
        a in 0u8..=255,
    ) {
        use crate::encoder::solid::{encode_solid, decode_solid};
        // A uniform 32×32 BGRA tile.
        let mut tile = vec![0u8; 32 * 32 * 4];
        for px in tile.chunks_exact_mut(4) {
            px.copy_from_slice(&[b, g, r, a]);
        }
        // Classifier setup mirroring C7's low-freq, low-mag, non-idle, uniform-color path.
        let m = TileMetrics {
            change_freq_hz: 2.0,
            change_magnitude: 0.05,
            idle_frames: 0,
            unique_colors: 1,
            edge_density: EDGE_DENSITY_UNKNOWN,
            codec_state: CodecState::Skip,
        };
        let next = classify_tile(&m, &CodecState::Skip);
        prop_assert_eq!(next, CodecState::Solid);

        let encoded = encode_solid(&tile);
        let decoded = decode_solid(&encoded).expect("valid 4-byte payload");
        prop_assert_eq!(decoded, [b, g, r, a]);
    }
}
