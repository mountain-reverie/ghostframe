//! The SAD reference is a per-tile last-committed reference (damage-driven
//! encoding, PR 3): a tile's reference pixels are replaced only when the tile
//! is committed dirty, so sub-threshold change accumulates against it.
#![allow(
    clippy::disallowed_methods,
    reason = "wall-clock measurement of real GPU time in the #[ignore]d report"
)]

use super::*;
use std::time::Instant;

const NEEDS_GPU: &str = "needs a Vulkan GPU";

fn run(gpu: &mut GpuFrameProcessor, px: &[u8], w: u32, h: u32) -> Vec<u32> {
    unsafe { gpu.process_frame_inner_from_pixels(px, w, h, w * 4) }
        .expect("process_frame")
        .dirty_tiles
}

/// Mid-grey frame with one pixel of tile 0 raised by `delta` over grey.
fn ramp_frame(w: u32, h: u32, delta: u8) -> Vec<u8> {
    let mut px = vec![128u8; (w * h * 4) as usize];
    for p in px.chunks_exact_mut(4) {
        p[3] = 255;
    }
    // One pixel of tile 0: SAD contribution is exactly `delta` per channel
    // (R only here), far below SAD_THRESHOLD per tick.
    px[2] = 128u8.saturating_add(delta);
    px
}

/// A ramp whose per-tick change is under the SAD threshold is sent once the
/// accumulated change against the committed reference crosses it. With the
/// previous-frame reference (master) the tile is never sent.
#[test]
fn drift_below_threshold_is_eventually_committed() {
    let (w, h) = (64u32, 64u32);
    let mut gpu = GpuFrameProcessor::new(64).expect(NEEDS_GPU);
    // First frame: everything dirty, becomes the reference.
    let first = run(&mut gpu, &ramp_frame(w, h, 0), w, h);
    assert_eq!(first.len(), 4);

    const STEP: u8 = 10; // per-tick SAD 10 < SAD_THRESHOLD (64)
    assert!((STEP as u32) < SAD_THRESHOLD);
    let mut sent_at = None;
    for tick in 1..=20u32 {
        let d = run(
            &mut gpu,
            &ramp_frame(w, h, (STEP as u32 * tick) as u8),
            w,
            h,
        );
        if d.contains(&0) {
            sent_at = Some(tick);
            break;
        }
        assert!(d.is_empty(), "only tile 0 changes; got {d:?}");
    }
    // Accumulated 10 * tick > 64 first at tick 7.
    assert_eq!(sent_at, Some(7), "tile must be sent when drift crosses 64");

    // After the commit the reference is the committed frame: the next small
    // step is again sub-threshold and not sent.
    let d = run(&mut gpu, &ramp_frame(w, h, (STEP as u32 * 8) as u8), w, h);
    assert!(d.is_empty(), "reference advanced on commit; got {d:?}");
}

/// Tiles that are not committed keep their reference: a tile that drifts
/// while its neighbour is committed is unaffected by the neighbour's commit.
#[test]
fn commit_updates_only_committed_tiles() {
    let (w, h) = (64u32, 32u32); // tiles 0 and 1
    let mut gpu = GpuFrameProcessor::new(64).expect(NEEDS_GPU);
    let mk = |t0: u8, t1: u8| {
        let mut px = vec![128u8; (w * h * 4) as usize];
        for p in px.chunks_exact_mut(4) {
            p[3] = 255;
        }
        px[2] = 128u8.saturating_add(t0); // tile 0, pixel 0, R
        px[(32 * 4) + 2] = 128u8.saturating_add(t1); // tile 1, pixel 0, R
        px
    };
    run(&mut gpu, &mk(0, 0), w, h);
    // Tile 1 drifts by 40 (kept), then tile 0 jumps by 100 (committed).
    assert!(run(&mut gpu, &mk(0, 40), w, h).is_empty());
    assert_eq!(run(&mut gpu, &mk(100, 40), w, h), vec![0]);
    // Tile 1 is now 40 + 40 = 80 away from ITS (old) reference.
    assert_eq!(run(&mut gpu, &mk(100, 80), w, h), vec![1]);
}

/// SAD of tile 0 as the shader last wrote it.
fn sad0(gpu: &GpuFrameProcessor) -> u32 {
    unsafe { *gpu.sad_buf.ptr }
}

/// Grey frame with the first `n` pixels of tile 0 (row-major within the tile) two levels up in R.
/// The shader scores `uint(abs(d) * 255.0)`, which truncates a one-level
/// change (float 0.99999) to 0 and a two-level change to 1, so each pixel
/// here contributes exactly 1 (measured, and asserted by the test below).
fn bump_frame(w: u32, h: u32, n: usize) -> Vec<u8> {
    let mut px = ramp_frame(w, h, 0);
    for i in 0..n {
        let (x, y) = (i % 32, i / 32); // fill tile 0 row by row
        px[(y * w as usize + x) * 4 + 2] = 130;
    }
    px
}

/// The commit threshold is strict: SAD == 64 is not dirty, SAD 65 is, and the
/// 64-SAD frame did not move the reference (C is measured against A).
#[test]
fn threshold_is_strict_and_boundary_is_measured_against_the_reference() {
    let (w, h) = (64u32, 32u32);
    let mut gpu = GpuFrameProcessor::new(64).expect(NEEDS_GPU);
    run(&mut gpu, &bump_frame(w, h, 0), w, h); // A
    let d = run(&mut gpu, &bump_frame(w, h, SAD_THRESHOLD as usize), w, h); // B
    assert_eq!(
        sad0(&gpu),
        SAD_THRESHOLD,
        "calibration: B must score exactly 64"
    );
    assert!(d.is_empty(), "SAD == threshold is not dirty; got {d:?}");
    let d = run(
        &mut gpu,
        &bump_frame(w, h, SAD_THRESHOLD as usize + 1),
        w,
        h,
    ); // C
    assert_eq!(
        sad0(&gpu),
        SAD_THRESHOLD + 1,
        "C is scored against A, not B"
    );
    assert_eq!(d, vec![0]);
}

/// Frame size not a multiple of 32: drift in the partial right and bottom
/// tiles accumulates and commits like any other.
#[test]
fn drift_in_partial_edge_tiles() {
    let (w, h) = (70u32, 40u32); // 3x2 tiles; x 64..70 and y 32..40 are partial
    let mut gpu = GpuFrameProcessor::new(64).expect(NEEDS_GPU);
    let mk = |delta: u8| {
        let mut px = ramp_frame(w, h, 0);
        for (x, y) in [(66u32, 0u32), (0, 36), (68, 36)] {
            px[((y * w + x) * 4 + 2) as usize] = 128 + delta;
        }
        px
    };
    assert_eq!(run(&mut gpu, &mk(0), w, h).len(), 6);
    let mut sent = None;
    for tick in 1..=12u32 {
        let d = run(&mut gpu, &mk((10 * tick) as u8), w, h);
        if !d.is_empty() {
            sent = Some((tick, d));
            break;
        }
    }
    assert_eq!(sent, Some((7, vec![2, 3, 5])));
    assert!(run(&mut gpu, &mk(80), w, h).is_empty());
}

fn dirty_frame(w: u32, h: u32, tiles_dirty: u32, n: u32) -> Vec<u8> {
    let cols = w / 32;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let t = (y / 32) * cols + x / 32;
            let v = if t < tiles_dirty {
                ((x + y + n * 37) % 251) as u8 // changes every tick
            } else {
                ((x * 3 + y) % 200) as u8 // static
            };
            let o = ((y * w + x) * 4) as usize;
            px[o..o + 4].copy_from_slice(&[v, v ^ 0x55, v.wrapping_mul(3), 255]);
        }
    }
    px
}

/// Across dirty fractions {0,1,5,25,100}% the committed set is exactly the
/// dirty set, and the reference it leaves is consistent with it: replaying the
/// same frame is entirely clean (every committed tile was written, no other
/// tile ever differed). The cost side of the property (time against committed
/// tiles) is `reference_report`.
#[test]
fn committed_tiles_track_dirty_fraction() {
    let (w, h) = (1920u32, 1088u32);
    let tiles = (w / 32) * h.div_ceil(32);
    let mut gpu = GpuFrameProcessor::new(tiles + 8).expect(NEEDS_GPU);
    for pct in [0u32, 1, 5, 25, 100] {
        let want = (tiles * pct).div_ceil(100);
        run(&mut gpu, &dirty_frame(w, h, 0, 0), w, h);
        let d = run(&mut gpu, &dirty_frame(w, h, want, 1), w, h);
        assert_eq!(d.len() as u32, want, "{pct}%");
        // The same frame again is entirely clean: the committed tiles were
        // written to the reference, the others never differed.
        assert!(run(&mut gpu, &dirty_frame(w, h, want, 1), w, h).is_empty());
        // Reverting to the original content differs from the committed
        // reference in exactly the committed tiles: dirty again. If a commit
        // had not written the reference, the reference would still be the
        // original and this would be empty.
        let back = run(&mut gpu, &dirty_frame(w, h, 0, 0), w, h);
        assert_eq!(back.len() as u32, want, "{pct}% revert");
    }
}

/// Per-frame wall time at several dirty fractions. A measurement, not a gate.
/// `gf-gpu-run cargo test -p ghostframe-lib --lib reference_report -- --ignored --nocapture`
#[test]
#[ignore]
fn reference_report() {
    for (label, w, h) in [("1080p", 1920u32, 1088u32), ("4K", 3840, 2160)] {
        let tiles = (w / 32) * h.div_ceil(32);
        let mut gpu = GpuFrameProcessor::new(tiles + 8).expect(NEEDS_GPU);
        for pct in [0u32, 1, 5, 25, 100] {
            let want = (tiles * pct).div_ceil(100);
            let frames: Vec<Vec<u8>> = (0..12).map(|n| dirty_frame(w, h, want, n + 1)).collect();
            run(&mut gpu, &dirty_frame(w, h, 0, 0), w, h);
            let mut t = Vec::new();
            for f in &frames {
                let t0 = Instant::now();
                let d = run(&mut gpu, f, w, h);
                t.push(t0.elapsed().as_secs_f64() * 1e3);
                assert_eq!(d.len() as u32, want);
            }
            t.drain(..2);
            t.sort_by(|a, b| a.partial_cmp(b).unwrap());
            println!(
                "REF {label} dirty={pct}% tiles={want} median_ms={:.3} min_ms={:.3}",
                t[t.len() / 2],
                t[0]
            );
        }
    }
}
