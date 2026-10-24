//! The dirty-list inverse must be pixel-identical to the full-grid inverse
//! it replaced, and must differ from it only where documented. Requires a
//! GPU (`WgpuContext::new().expect`: no GPU is a failure, not a skip).
//!
//! The comparison is honest because `Cdf53Pipeline::inverse_full_grid` is the
//! old behaviour (every slot dispatched, `tileGen == 0` early-exit) running
//! against identical integrate calls.

use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
use ghostframe_client_gpu::{
    framebuffer::Framebuffer,
    pipelines::cdf53::{Cdf53PassEntry, Cdf53Pipeline},
    wgpu_ctx::WgpuContext,
};
use ghostframe_protocol::codec::cdf53 as codec;

const COLS: u32 = 4;
const ROWS: u32 = 3;
const W: u32 = COLS * 32;
const H: u32 = ROWS * 32;

/// A tile that is not flat and differs per `seed`, so a pass applied to the
/// wrong tile or an off-by-one in the tile mapping cannot reconstruct to the
/// same bytes.
fn content(seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = (x * 5 + seed * 37 + (y * y) / 9) & 0xFF;
            let g = (y * 7 + seed * 53 + (x ^ y)) & 0xFF;
            let r = ((x * y) / 3 + seed * 91) & 0xFF;
            v.extend_from_slice(&[b as u8, g as u8, r as u8, 0xFF]);
        }
    }
    v
}

/// (pass_idx, bit_planes, present) for every sparse pass of tile `seed`.
fn passes(seed: u32) -> Vec<(u8, Vec<u8>, Option<u16>)> {
    let coeffs = codec::forward(&content(seed));
    let (present, sparse) = codec::encode_passes_sparse(&coeffs);
    sparse
        .iter()
        .map(|(p, payload)| {
            let pre = prevalidate_cdf53(payload, 1, *p).expect("prevalidates");
            (
                *p,
                pre.bit_planes,
                if *p == 0 { Some(present) } else { None },
            )
        })
        .collect()
}

fn entries(tile: (u8, u8), gen: u8, seed: u32, keep: impl Fn(u8) -> bool) -> Vec<Cdf53PassEntry> {
    passes(seed)
        .into_iter()
        .filter(|(p, _, _)| keep(*p))
        .map(|(p, bp, pp)| (tile.0, tile.1, gen, p, bp, pp))
        .collect()
}

fn expected_rgba(seed: u32) -> Vec<u8> {
    let bgr = codec::inverse(&codec::forward(&content(seed)));
    let mut rgba = vec![255u8; 32 * 32 * 4];
    for i in 0..1024 {
        rgba[i * 4] = bgr[i * 3 + 2];
        rgba[i * 4 + 1] = bgr[i * 3 + 1];
        rgba[i * 4 + 2] = bgr[i * 3];
    }
    rgba
}

fn tile_pixels(fb: &[u8], tx: u32, ty: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32 {
        let o = (((ty * 32 + y) * W + tx * 32) * 4) as usize;
        out.extend_from_slice(&fb[o..o + 32 * 4]);
    }
    out
}

struct Rig {
    fb: Framebuffer,
    pipe: Cdf53Pipeline,
    full: bool,
}

impl Rig {
    fn new(ctx: &WgpuContext, full: bool) -> Self {
        let mut fb = Framebuffer::new(&ctx.device, W, H);
        fb.debug_fill(&ctx.device, &ctx.queue, [1, 2, 3, 0xFF]);
        let mut pipe = Cdf53Pipeline::new(&ctx.device);
        pipe.resize(&ctx.device, &ctx.queue, COLS, ROWS);
        Rig { fb, pipe, full }
    }

    fn batch(&mut self, ctx: &WgpuContext, b: &[Cdf53PassEntry]) -> Vec<u8> {
        self.pipe.integrate(&ctx.device, &ctx.queue, b);
        self.inverse(ctx)
    }

    fn inverse(&mut self, ctx: &WgpuContext) -> Vec<u8> {
        if self.full {
            self.pipe
                .inverse_full_grid(&ctx.device, &ctx.queue, &self.fb);
        } else {
            self.pipe.inverse(&ctx.device, &ctx.queue, &self.fb);
        }
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        self.fb.debug_read(&ctx.device, &ctx.queue)
    }
}

/// Multi-batch, out-of-order, with duplicates, partial refinement, a
/// generation bump. Returns the framebuffer after every batch.
fn scenario(ctx: &WgpuContext, full: bool) -> Vec<Vec<u8>> {
    let mut rig = Rig::new(ctx, full);
    let mut snaps = Vec::new();

    // Batch 1: tiles A(0,0) B(1,0) C(2,1) D(3,2) with passes 0..=7,
    // interleaved across tiles, tile A's pass 2 sent twice in the batch.
    let a = entries((0, 0), 1, 10, |p| p <= 7);
    let b = entries((1, 0), 1, 11, |p| p <= 7);
    let c = entries((2, 1), 1, 12, |p| p <= 7);
    let d = entries((3, 2), 1, 13, |p| p <= 7);
    let mut b1 = Vec::new();
    let n = a.len().max(b.len()).max(c.len()).max(d.len());
    for i in 0..n {
        for t in [&a, &b, &c, &d] {
            if let Some(e) = t.get(i) {
                b1.push(e.clone());
            }
        }
    }
    // Duplicate a magnitude pass of A (sparse encoding may omit pass 2).
    let dup = a.iter().rev().find(|e| e.3 > 0).cloned();
    assert!(
        a.len() >= 2 && dup.is_some(),
        "fixture too thin to duplicate"
    );
    b1.push(dup.unwrap());
    snaps.push(rig.batch(ctx, &b1));

    // Batch 2, out of order, touching only a subset: C's remaining passes in
    // reverse, and a brand-new tile F(0,2) in full, reversed.
    let mut b2 = entries((2, 1), 1, 12, |p| p > 7);
    b2.extend(entries((0, 2), 1, 14, |_| true));
    b2.reverse();
    snaps.push(rig.batch(ctx, &b2));

    // Batch 3: A restarts at generation 2 with different content, B gets one
    // more pass.
    let mut b3 = entries((0, 0), 2, 15, |_| true);
    b3.extend(entries((1, 0), 1, 11, |p| p == 8));
    snaps.push(rig.batch(ctx, &b3));

    snaps
}

#[test]
fn dirty_list_inverse_is_byte_identical_to_the_full_grid_inverse() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let dirty = scenario(&ctx, false);
    let full = scenario(&ctx, true);
    assert_eq!(dirty.len(), 3);
    for (i, (d, f)) in dirty.iter().zip(&full).enumerate() {
        assert!(
            d == f,
            "batch {} differs between dirty-list and full-grid",
            i + 1
        );
    }
    // The comparison must not be vacuous: tiles really were painted, and
    // the batches really did change the framebuffer.
    assert_ne!(dirty[0], dirty[1]);
    assert_ne!(dirty[1], dirty[2]);
    assert_ne!(tile_pixels(&dirty[2], 0, 0), [1u8, 2, 3, 0xFF].repeat(1024));

    // And both equal the CPU oracle for the fully delivered tiles, exactly.
    let last = &dirty[2];
    assert_eq!(tile_pixels(last, 0, 0), expected_rgba(15), "A gen 2");
    assert_eq!(tile_pixels(last, 2, 1), expected_rgba(12), "C");
    assert_eq!(tile_pixels(last, 0, 2), expected_rgba(14), "F");
    // An untouched tile is untouched.
    assert_eq!(
        tile_pixels(last, 3, 0),
        [1u8, 2, 3, 0xFF].repeat(1024),
        "never-integrated tile was written"
    );
}

#[test]
fn inverse_with_nothing_dirty_leaves_the_framebuffer_alone() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    let first = rig.batch(&ctx, &entries((1, 1), 1, 20, |_| true));
    assert_eq!(tile_pixels(&first, 1, 1), expected_rgba(20));
    // Paint over the tile, then flush again with no new passes.
    rig.fb
        .debug_fill_tile(&ctx.device, &ctx.queue, 1, 1, [9, 8, 7, 0xFF]);
    let again = rig.inverse(&ctx);
    assert_eq!(tile_pixels(&again, 1, 1), [9u8, 8, 7, 0xFF].repeat(1024));
}

/// CHARACTERISATION of the one intended behaviour change. The renderer
/// flushes solid/PalRle/raw BEFORE Cdf53, and the old inverse re-ran every
/// previously integrated tile on every Cdf53 flush, so a tile that had been
/// Cdf53 and was later repainted by another codec was overwritten with its
/// stale wavelet content the next time any Cdf53 tile arrived. The dirty
/// list stops that.
#[test]
fn tile_repainted_by_another_codec_is_no_longer_resurrected() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let run = |full: bool| {
        let mut rig = Rig::new(&ctx, full);
        rig.batch(&ctx, &entries((1, 1), 1, 30, |_| true));
        // Another codec repaints tile (1,1).
        rig.fb
            .debug_fill_tile(&ctx.device, &ctx.queue, 1, 1, [9, 8, 7, 0xFF]);
        // An unrelated Cdf53 tile arrives.
        let fb = rig.batch(&ctx, &entries((3, 0), 1, 31, |_| true));
        tile_pixels(&fb, 1, 1)
    };
    let solid = [9u8, 8, 7, 0xFF].repeat(1024);
    assert_eq!(run(false), solid, "dirty path must keep the repaint");
    assert_eq!(
        run(true),
        expected_rgba(30),
        "full-grid path used to resurrect the stale Cdf53 tile"
    );
}

/// A later batch that refines a tile re-inverts it from ALL passes seen so
/// far (persistent coefficient state), not just that batch's.
#[test]
fn refinement_in_a_later_batch_uses_the_accumulated_passes() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    let early = rig.batch(&ctx, &entries((2, 2), 1, 40, |p| p <= 8));
    assert_ne!(tile_pixels(&early, 2, 2), expected_rgba(40));
    let done = rig.batch(&ctx, &entries((2, 2), 1, 40, |p| p > 8));
    assert_eq!(tile_pixels(&done, 2, 2), expected_rgba(40));
}

/// The wire generation is 4 bits and wraps through 0 (`bump_generation`), so
/// 0 is a real generation. The inverse shaders gate on `tileGen != 0` as
/// "never seen"; a tile at wire generation 0 must still be inverse-transformed.
#[test]
fn generation_zero_tile_is_painted() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    let fb = rig.batch(&ctx, &entries((1, 1), 0, 50, |_| true));
    assert_ne!(
        tile_pixels(&fb, 1, 1),
        [1u8, 2, 3, 0xFF].repeat(1024),
        "gen 0 tile left stale"
    );
    assert_eq!(tile_pixels(&fb, 1, 1), expected_rgba(50));
}

/// The wrap: a tile at generation 15 is superseded by generation 0.
#[test]
fn generation_wrap_15_to_0_is_painted() {
    for full in [false, true] {
        let ctx = WgpuContext::new().expect("wgpu context");
        let mut rig = Rig::new(&ctx, full);
        let a = rig.batch(&ctx, &entries((2, 0), 15, 51, |_| true));
        assert_eq!(
            tile_pixels(&a, 2, 0),
            expected_rgba(51),
            "gen 15 (full={full})"
        );
        let b = rig.batch(&ctx, &entries((2, 0), 0, 52, |_| true));
        assert_eq!(
            tile_pixels(&b, 2, 0),
            expected_rgba(52),
            "gen 0 after 15 (full={full})"
        );
    }
}

/// Not a gate: prints inverse cost (encode + submit + device idle) on a 64x64 grid (the context caps textures at 2048; half the tile count of 4K) for
/// 1 dirty tile, 1/10 of the grid, and the whole grid, against the old
/// full-grid dispatch. Run: `cargo test -p ghostframe-client-gpu --test
/// cdf53_dirty --release -- --ignored --nocapture inverse_cost`.
#[test]
#[ignore = "measurement, prints numbers"]
fn inverse_cost_4096_tiles() {
    use std::time::Instant;
    let ctx = WgpuContext::new().expect("wgpu context");
    let (cols, rows) = (64u32, 64u32);
    let mut fb = Framebuffer::new(&ctx.device, cols * 32, rows * 32);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);
    let mut pipe = Cdf53Pipeline::new(&ctx.device);
    pipe.resize(&ctx.device, &ctx.queue, cols, rows);
    let p0 = passes(10).into_iter().next().unwrap();
    let all: Vec<Cdf53PassEntry> = (0..rows)
        .flat_map(|y| (0..cols).map(move |x| (x, y)))
        .map(|(x, y)| (x as u8, y as u8, 1, p0.0, p0.1.clone(), p0.2))
        .collect();
    let wait = || {
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll")
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    // Steady state: every tile already integrated once, so the full-grid
    // path has real work for every slot (a never-integrated slot exits early).
    pipe.integrate(&ctx.device, &ctx.queue, &all);
    pipe.inverse(&ctx.device, &ctx.queue, &fb);
    wait();
    for (label, n) in [
        ("1 tile", 1usize),
        ("410 tiles (10%)", 410),
        ("4096 tiles (all)", 4096),
    ] {
        let mut dirty_ms = Vec::new();
        let mut full_ms = Vec::new();
        for _ in 0..15 {
            pipe.integrate(&ctx.device, &ctx.queue, &all[..n]);
            wait();
            let t = Instant::now();
            pipe.inverse(&ctx.device, &ctx.queue, &fb);
            wait();
            dirty_ms.push(Instant::now().duration_since(t).as_secs_f64() * 1e3);
            let t = Instant::now();
            pipe.inverse_full_grid(&ctx.device, &ctx.queue, &fb);
            wait();
            full_ms.push(Instant::now().duration_since(t).as_secs_f64() * 1e3);
        }
        println!(
            "2048x2048 (4096-tile) inverse, {label}: dirty-list {:.3} ms, full-grid {:.3} ms (median of 15)",
            median(dirty_ms),
            median(full_ms)
        );
    }
}
