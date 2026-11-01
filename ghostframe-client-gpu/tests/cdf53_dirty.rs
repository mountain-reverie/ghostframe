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
        .map(|(p, bp, pp)| (tile.0, tile.1, gen, p, bp, pp, 0))
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

/// A tile returns to CDF 5/3 on the 4-bit generation it already holds, with a
/// newer label: new content. The coefficients of the old must not survive
/// into it. Fails if the clear keys on the generation alone.
#[test]
fn a_new_label_under_the_same_generation_replaces_the_content() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    let at = |seed: u32, seq: u32| -> Vec<Cdf53PassEntry> {
        entries((2, 2), 1, seed, |_| true)
            .into_iter()
            .map(|mut e| {
                e.6 = seq;
                e
            })
            .collect()
    };
    let old = rig.batch(&ctx, &at(50, 100));
    assert_eq!(tile_pixels(&old, 2, 2), expected_rgba(50));
    let new = rig.batch(&ctx, &at(51, 200));
    assert_eq!(tile_pixels(&new, 2, 2), expected_rgba(51));
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
        .map(|(x, y)| (x as u8, y as u8, 1, p0.0, p0.1.clone(), p0.2, 0))
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

/// A new generation's pass 0 (signs only) must not be inverted: with no
/// magnitude bits the inverse produces black, which flashed over the tile's
/// previous content on every update. The tile keeps what the framebuffer
/// shows until a magnitude plane lands, then reconstructs exactly.
#[test]
fn a_tile_is_not_inverted_to_black_before_it_has_magnitudes() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    let before = [200u8, 100, 50, 0xFF];
    rig.fb.debug_fill(&ctx.device, &ctx.queue, before);

    let after_pass0 = rig.batch(&ctx, &entries((1, 1), 1, 7, |p| p == 0));
    assert!(
        tile_pixels(&after_pass0, 1, 1)
            .chunks_exact(4)
            .all(|p| p == before),
        "pass 0 alone repainted the tile (black flash)"
    );

    let complete = rig.batch(&ctx, &entries((1, 1), 1, 7, |p| p > 0));
    assert_eq!(tile_pixels(&complete, 1, 1), expected_rgba(7));
}

/// The rule above must not swallow a tile that really is black: sparse, it
/// is pass 0 alone and complete, so it is drawn.
#[test]
fn a_black_tile_is_still_drawn_from_pass_0() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    rig.fb
        .debug_fill(&ctx.device, &ctx.queue, [200, 100, 50, 0xFF]);

    let black: Vec<u8> = (0..1024).flat_map(|_| [0, 0, 0, 0xFF]).collect();
    let coeffs = codec::forward(&black);
    let (present, sparse) = codec::encode_passes_sparse(&coeffs);
    assert_eq!(sparse.len(), 1, "premise: a black tile is pass 0 only");
    let pre = prevalidate_cdf53(&sparse[0].1, 1, 0).expect("prevalidates");
    let fb = rig.batch(&ctx, &[(2, 0, 1, 0, pre.bit_planes, Some(present), 0)]);
    assert!(
        tile_pixels(&fb, 2, 0)
            .chunks_exact(4)
            .all(|p| p == [0, 0, 0, 0xFF]),
        "a complete black tile must be drawn"
    );
}

/// The CPU reference for a partly refined tile: `Cdf53TileState` fed the same
/// passes, its picture after the last one that changed it.
fn cpu_refine(
    st: &mut ghostframe_client_core::cdf53_tile_state::Cdf53TileState,
    tile: (u8, u8),
    seed: u32,
    keep: impl Fn(u8) -> bool,
    last: &mut Vec<u8>,
) {
    let coeffs = codec::forward(&content(seed));
    let (_, sparse) = codec::encode_passes_sparse(&coeffs);
    for (p, payload) in sparse.iter().filter(|(p, _)| keep(*p)) {
        let pre = prevalidate_cdf53(payload, 1, *p).expect("prevalidates");
        if let Some(rgba) = st.integrate(tile.0, tile.1, &pre) {
            *last = rgba;
        }
    }
}

/// The pass count the inverse levels read (`passesProcessed`) must be on the
/// GPU for every tile a batch touched, and intact for the ones inside the
/// written range that it did not. A tile refined only to a middle pass
/// reconstructs with a midpoint correction that depends on that count, so a
/// count left at zero is a visibly different picture, unlike a fully refined
/// tile, where it makes no difference.
///
/// The dirty-list inverse only reads the counts of tiles it lists, so the
/// untouched-but-in-range tile is checked through the full-grid inverse, which
/// reads every slot.
#[test]
fn pass_counts_of_partly_refined_tiles_reach_the_gpu() {
    use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    // Tile indices 0, 5 and 11: the middle one is inside the range of a batch
    // that touches only the outer two.
    let tiles = [((0u8, 0u8), 10u32), ((1, 1), 11), ((3, 2), 12)];
    let mut cpu: Vec<(Cdf53TileState, Vec<u8>)> = tiles
        .iter()
        .map(|_| (Cdf53TileState::new(), Vec::new()))
        .collect();

    let check = |rig: &mut Rig,
                 cpu: &mut Vec<(Cdf53TileState, Vec<u8>)>,
                 batch: Vec<Cdf53PassEntry>,
                 upto: [u8; 3],
                 from: [u8; 3]| {
        for (i, (tile, seed)) in tiles.iter().enumerate() {
            if upto[i] >= from[i] {
                let (st, last) = &mut cpu[i];
                cpu_refine(st, *tile, *seed, |p| p >= from[i] && p <= upto[i], last);
            }
        }
        let fb = rig.batch(&ctx, &batch);
        for (i, (tile, _)) in tiles.iter().enumerate() {
            assert_eq!(
                tile_pixels(&fb, tile.0 as u32, tile.1 as u32),
                cpu[i].1,
                "tile {tile:?} after a batch ending at pass {}",
                upto[i]
            );
        }
    };

    let mut b1 = Vec::new();
    for (tile, seed) in &tiles {
        b1.extend(entries(*tile, 1, *seed, |p| p <= 6));
    }
    check(&mut rig, &mut cpu, b1, [6, 6, 6], [0, 0, 0]);
    // The partial picture must differ from the finished one, or this test
    // could not tell a missing pass count from a present one.
    assert_ne!(cpu[0].1, expected_rgba(10), "fixture: pass 6 is not final");

    // Refine the outer two only; the middle tile's count must be left alone.
    let mut b2 = Vec::new();
    b2.extend(entries(tiles[0].0, 1, tiles[0].1, |p| (7..=9).contains(&p)));
    b2.extend(entries(tiles[2].0, 1, tiles[2].1, |p| (7..=9).contains(&p)));
    check(&mut rig, &mut cpu, b2, [9, 6, 9], [7, 7, 7]);

    // Re-invert every slot: the middle tile's count (inside b2's written range,
    // not in b2) must still be the one b1 wrote.
    rig.pipe.inverse_full_grid(&ctx.device, &ctx.queue, &rig.fb);
    let fb = rig.fb.debug_read(&ctx.device, &ctx.queue);
    for (i, (tile, _)) in tiles.iter().enumerate() {
        assert_eq!(
            tile_pixels(&fb, tile.0 as u32, tile.1 as u32),
            cpu[i].1,
            "tile {tile:?} after the full-grid inverse"
        );
    }

    // Now refine the middle one alone, re-inverting it from its own count.
    let b3 = entries(tiles[1].0, 1, tiles[1].1, |p| (7..=8).contains(&p));
    check(&mut rig, &mut cpu, b3, [9, 8, 9], [10, 7, 10]);
}

/// The inverse's last stage binds the framebuffer's view and caches that bind
/// group against its epoch; a resize replaces the texture (keeping the pixels),
/// and the cache must follow, or the second tile is inverted into a discarded
/// texture.
#[test]
fn inverse_draws_into_a_framebuffer_texture_that_replaced_the_one_it_cached() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut rig = Rig::new(&ctx, false);
    let first = rig.batch(&ctx, &entries((0, 0), 1, 40, |_| true));
    assert_eq!(tile_pixels(&first, 0, 0), expected_rgba(40));
    rig.fb.resize(&ctx.device, &ctx.queue, W, H + 32);
    let second = rig.batch(&ctx, &entries((1, 1), 1, 41, |_| true));
    assert_eq!(tile_pixels(&second, 1, 1), expected_rgba(41));
    assert_eq!(tile_pixels(&second, 0, 0), expected_rgba(40));
}
