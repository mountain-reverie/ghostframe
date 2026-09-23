use ghostframe_client_gpu::coalesce::{coalesce, Rect};
use ghostframe_client_gpu::dirty::DirtyGrid;
use proptest::prelude::*;
use std::collections::HashSet;

#[test]
fn a_horizontal_run_becomes_one_rect() {
    let mut g = DirtyGrid::new(8, 8);
    for x in 2..6 {
        g.set(x, 3);
    }
    assert_eq!(
        coalesce(&g),
        vec![Rect {
            x: 2,
            y: 3,
            w: 4,
            h: 1
        }]
    );
}

#[test]
fn stacked_identical_runs_merge_vertically() {
    let mut g = DirtyGrid::new(8, 8);
    for y in 1..4 {
        for x in 2..6 {
            g.set(x, y);
        }
    }
    assert_eq!(
        coalesce(&g),
        vec![Rect {
            x: 2,
            y: 1,
            w: 4,
            h: 3
        }]
    );
}

#[test]
fn runs_with_different_extents_do_not_merge() {
    let mut g = DirtyGrid::new(8, 8);
    for x in 2..6 {
        g.set(x, 1);
    }
    for x in 3..6 {
        g.set(x, 2);
    }
    let rects = coalesce(&g);
    assert_eq!(rects.len(), 2, "got {rects:?}");
}

#[test]
fn empty_grid_yields_no_rects() {
    assert!(coalesce(&DirtyGrid::new(8, 8)).is_empty());
}

#[test]
fn to_pixels_clamps_at_the_bottom_right_edge() {
    // 1920x1080 -> 60x34 tile grid (34 = ceil(1080/32)); row 33 covers
    // pixels 1056..1088, but the framebuffer stops at 1080, so h must
    // clamp to 24, not the full 32.
    let r = Rect {
        x: 0,
        y: 33,
        w: 1,
        h: 1,
    };
    let px = r.to_pixels(1920, 1080);
    assert_eq!(px.y, 1056);
    assert_eq!(px.h, 24, "bottom edge must clamp, got {px:?}");

    let r = Rect {
        x: 59,
        y: 0,
        w: 1,
        h: 1,
    };
    let px = r.to_pixels(1920, 1080);
    assert_eq!(px.x, 1888);
    assert_eq!(px.w, 32, "right edge is exact at 1920, no clamp needed");
}

proptest! {
    /// The invariant that actually matters: the rects must cover exactly
    /// the dirty tiles. Covering too few leaves stale pixels; covering too
    /// many hands the host a damage region claiming more changed than did.
    #[test]
    fn rects_cover_exactly_the_dirty_set(
        tiles in prop::collection::hash_set((0u32..16, 0u32..16), 0..80)
    ) {
        let mut g = DirtyGrid::new(16, 16);
        for &(x, y) in &tiles { g.set(x, y); }

        let mut covered: HashSet<(u32, u32)> = HashSet::new();
        for r in coalesce(&g) {
            for y in r.y..r.y + r.h {
                for x in r.x..r.x + r.w {
                    prop_assert!(covered.insert((x, y)), "rects overlap at ({x},{y})");
                }
            }
        }
        prop_assert_eq!(covered, tiles);
    }
}
