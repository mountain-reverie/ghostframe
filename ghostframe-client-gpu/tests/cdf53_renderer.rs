//! CDF 5/3 through the `Renderer` with the GPU grid enabled: scheduling
//! (budget, throttle, skips), generations and paint order, against the CPU
//! decoder (`Cdf53TileState`) as the reference for what should be on screen.
//! Requires a GPU (no GPU is a failure, not a skip); not named in any CI
//! workflow.

use ghostframe_client_core::cdf53_prevalidate::{prevalidate_cdf53, PrevalidatedCdf53};
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_client_core::{Event, TileData};
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_protocol::codec::cdf53 as codec;
use std::collections::HashSet;

/// One tile as the wire carries it: `(pass, validated pass)` for every
/// present pass.
struct Wire {
    passes: Vec<PrevalidatedCdf53>,
}

fn bgra(kind: u32, seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(4096);
    let mut rng = seed.wrapping_mul(2654435761).wrapping_add(kind);
    for y in 0..32u32 {
        for x in 0..32u32 {
            rng = rng.wrapping_mul(48271).wrapping_add(1);
            let n = (rng >> 24) & 3;
            let (b, g, r) = match kind {
                0 => (0x80, 0x80, 0x80),
                1 => (
                    (x * 6 + seed * 11) & 0xFF,
                    (y * 7 + seed) & 0xFF,
                    128 + x + y,
                ),
                _ => ((x * 4 + y + seed) + n, (y * 5 + seed) + n, 200 - x - y + n),
            };
            v.extend_from_slice(&[b.min(255) as u8, g.min(255) as u8, r.min(255) as u8, 0xFF]);
        }
    }
    v
}

fn wire(kind: u32, seed: u32, generation: u8) -> Wire {
    let coeffs = codec::forward(&bgra(kind, seed));
    let (_, sparse) = codec::encode_passes_sparse(&coeffs);
    Wire {
        passes: sparse
            .iter()
            .map(|(p, payload)| prevalidate_cdf53(payload, generation, *p).expect("valid"))
            .collect(),
    }
}

fn pass_event(w: &Wire, i: usize, tx: u8, ty: u8, seq: u32, new_gen: bool) -> Event {
    let p = &w.passes[i];
    Event::TilePayload {
        frame_seq: seq,
        tile_x: tx,
        tile_y: ty,
        generation: p.generation,
        data: TileData::Cdf53 {
            pass_idx: p.pass_idx,
            bit_planes: p.bit_planes.clone(),
            present_passes: p.present_passes,
            new_generation: new_gen && i == 0,
        },
    }
}

fn solid_event(tx: u8, ty: u8, seq: u32, bgra: [u8; 4]) -> Event {
    Event::TilePayload {
        frame_seq: seq,
        tile_x: tx,
        tile_y: ty,
        generation: 0,
        data: TileData::Solid(bgra),
    }
}

struct Rig {
    ctx: WgpuContext,
    r: Renderer,
    w: u32,
}

impl Rig {
    fn new(w: u32, h: u32) -> Self {
        let ctx = WgpuContext::new().expect("wgpu context");
        let mut r = Renderer::new(&ctx, w, h, 3, &[], false, false).expect("renderer");
        r.enable_cdf53_gpu(&ctx);
        Rig { ctx, r, w }
    }

    fn apply(&mut self, ev: Event) {
        self.r.apply_event(&self.ctx, ev).expect("gpu ok");
    }

    fn flush(&mut self) {
        self.r.flush(&self.ctx).expect("gpu ok");
    }

    fn tile(&self, fb: &[u8], tx: u32, ty: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity(4096);
        for y in 0..32 {
            let o = (((ty * 32 + y) * self.w + tx * 32) * 4) as usize;
            out.extend_from_slice(&fb[o..o + 128]);
        }
        out
    }
}

/// The RGBA picture the CPU decoder shows for these passes of one tile.
fn cpu_picture(w: &Wire, upto: usize) -> Option<Vec<u8>> {
    let mut st = Cdf53TileState::new();
    let mut shown = None;
    for p in &w.passes[..upto] {
        if let Some(rgba) = st.integrate(0, 0, p) {
            shown = Some(rgba);
        }
    }
    shown
}

#[test]
fn a_flat_tile_reconstructs_its_flat_colour() {
    let mut rig = Rig::new(64, 64);
    let w = wire(0, 0, 1);
    for i in 0..w.passes.len() {
        rig.apply(pass_event(&w, i, 1, 1, 1, true));
    }
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    let t = rig.tile(&fb, 1, 1);
    assert!(
        t.chunks(4).all(|p| p == [0x80, 0x80, 0x80, 0xFF]),
        "{:?}",
        &t[..8]
    );
    let (stats, frames) = rig.r.take_cdf53_stats().unwrap();
    assert_eq!((stats.listed, frames), (1, 1));
}

#[test]
fn a_generation_bump_replaces_the_picture_without_blending() {
    let mut rig = Rig::new(32, 32);
    let a = wire(2, 5, 1);
    for i in 0..a.passes.len() {
        rig.apply(pass_event(&a, i, 0, 0, 1, true));
    }
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert_eq!(
        rig.tile(&fb, 0, 0),
        cpu_picture(&a, a.passes.len()).unwrap()
    );

    // Generation 2 is a sparser picture; its passes leave generation 1's
    // planes in the store.
    let b = wire(0, 0, 2);
    assert!(b.passes.len() < a.passes.len(), "precondition: sparser");
    for i in 0..b.passes.len() {
        rig.apply(pass_event(&b, i, 0, 0, 2, true));
    }
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert_eq!(
        rig.tile(&fb, 0, 0),
        cpu_picture(&b, b.passes.len()).unwrap()
    );
}

#[test]
fn the_picture_follows_the_cpu_decoder_pass_by_pass() {
    let mut rig = Rig::new(32, 32);
    let w = wire(2, 3, 1);
    let mut compared = 0;
    for i in 0..w.passes.len() {
        rig.apply(pass_event(&w, i, 0, 0, 1, true));
        // Two flushes: the throttle lets a partial tile wait one.
        rig.flush();
        rig.flush();
        let fb = rig.r.debug_read_framebuffer(&rig.ctx);
        if let Some(want) = cpu_picture(&w, i + 1) {
            assert_eq!(rig.tile(&fb, 0, 0), want, "after pass {i}");
            compared += 1;
        } else {
            assert!(
                rig.tile(&fb, 0, 0).iter().all(|&b| b == 0),
                "after pass {i}: the CPU decoder shows nothing, so must the grid"
            );
        }
    }
    assert!(compared >= 3, "only {compared} pictures compared");
}

#[test]
fn a_burst_over_the_budget_is_drawn_over_several_flushes_and_loses_nothing() {
    // 20 x 20 tiles of complete content: ~400 x 52 us is well over a flush.
    let mut rig = Rig::new(640, 640);
    let ws: Vec<Wire> = (0..4).map(|s| wire(1, s, 1)).collect();
    let mut want = Vec::new();
    for ty in 0..20u8 {
        for tx in 0..20u8 {
            let w = &ws[(tx as usize + ty as usize) % 4];
            for i in 0..w.passes.len() {
                rig.apply(pass_event(w, i, tx, ty, 1, true));
            }
            want.push(((tx, ty), cpu_picture(w, w.passes.len()).unwrap()));
        }
    }
    rig.flush();
    let (first, _) = rig.r.take_cdf53_stats().unwrap();
    assert!(first.listed > 0 && first.listed < 400, "{first:?}");
    assert!(first.deferred > 0);
    assert!(rig.r.has_deferred_work(), "work is left over");

    let mut flushes = 1;
    while rig.r.has_deferred_work() {
        rig.flush();
        flushes += 1;
        assert!(flushes < 20, "never drains");
    }
    assert!(flushes >= 3, "{flushes} flushes: budget did not bind");
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    for ((tx, ty), pic) in want {
        assert_eq!(rig.tile(&fb, tx as u32, ty as u32), pic, "tile ({tx},{ty})");
    }
}

#[test]
fn a_newer_solid_is_not_covered_by_an_older_pass_still_waiting() {
    let mut rig = Rig::new(32, 32);
    let w = wire(2, 1, 1);
    for i in 0..w.passes.len() {
        rig.apply(pass_event(&w, i, 0, 0, 4, true));
    }
    // The Solid is newer; the tile's pass has not been flushed yet.
    rig.apply(solid_event(0, 0, 6, [10, 20, 30, 255]));
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    let t = rig.tile(&fb, 0, 0);
    assert!(t.chunks(4).all(|p| p == [30, 20, 10, 255]), "{:?}", &t[..8]);
    // A late pass of the older frame does not cover it either.
    rig.apply(pass_event(&w, 0, 0, 0, 4, false));
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert!(rig
        .tile(&fb, 0, 0)
        .chunks(4)
        .all(|p| p == [30, 20, 10, 255]));
}

#[test]
fn a_pass_that_draws_nothing_does_not_supersede_an_older_solid_in_its_batch() {
    let mut rig = Rig::new(32, 32);
    let w = wire(2, 1, 1);
    // Solid from frame 5, then only pass 0 (signs, no magnitude) of frame 6:
    // the grid draws nothing for it, so the Solid is what must show.
    rig.apply(solid_event(0, 0, 5, [10, 20, 30, 255]));
    rig.apply(pass_event(&w, 0, 0, 0, 6, true));
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    let t = rig.tile(&fb, 0, 0);
    assert!(t.chunks(4).all(|p| p == [30, 20, 10, 255]), "{:?}", &t[..8]);
    let (stats, _) = rig.r.take_cdf53_stats().unwrap();
    assert_eq!(stats.listed, 0);
    assert_eq!(stats.skipped_no_magnitude, 1);
}

#[test]
fn a_pass_listed_this_flush_does_supersede_an_older_solid() {
    let mut rig = Rig::new(32, 32);
    let w = wire(2, 1, 1);
    rig.apply(solid_event(0, 0, 5, [10, 20, 30, 255]));
    for i in 0..w.passes.len() {
        rig.apply(pass_event(&w, i, 0, 0, 6, true));
    }
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert_eq!(
        rig.tile(&fb, 0, 0),
        cpu_picture(&w, w.passes.len()).unwrap()
    );
}

#[test]
fn passes_for_a_grid_too_large_for_the_store_decode_on_the_cpu() {
    // Not reachable at a size a test can allocate, so exercise the fallback
    // directly: a renderer without the GPU enabled decodes on the CPU.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = Renderer::new(&ctx, 32, 32, 3, &[], false, false).expect("renderer");
    let w = wire(2, 7, 1);
    for i in 0..w.passes.len() {
        r.apply_event(&ctx, pass_event(&w, i, 0, 0, 1, true))
            .unwrap();
    }
    r.flush(&ctx).unwrap();
    let fb = r.debug_read_framebuffer(&ctx);
    let want = cpu_picture(&w, w.passes.len()).unwrap();
    assert_eq!(&fb[..128], &want[..128]);
    assert!(r.take_cdf53_stats().is_none());
}

// ---- H.264 --------------------------------------------------------------

#[test]
fn an_h264_frame_cancels_cdf53_draws_still_waiting() {
    // Far more tiles than one flush draws, then the H.264 frame covers the
    // screen: nothing pending may land on top of it, now or later.
    let mut rig = Rig::new(640, 640);
    let w = wire(1, 1, 1);
    for ty in 0..20u8 {
        for tx in 0..20u8 {
            for i in 0..w.passes.len() {
                rig.apply(pass_event(&w, i, tx, ty, 1, true));
            }
        }
    }
    rig.flush(); // draws ~127 of 400
    assert!(rig.r.has_deferred_work());
    let before = rig.r.take_cdf53_stats().unwrap().0;

    rig.r.supersede_cdf53(10);
    assert!(!rig.r.has_deferred_work(), "nothing may stay pending");
    // Stand-in for the frame: paint every tile solid, newer than the burst.
    for ty in 0..20u8 {
        for tx in 0..20u8 {
            rig.apply(solid_event(tx, ty, 11, [1, 2, 3, 255]));
        }
    }
    for _ in 0..5 {
        rig.flush();
    }
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    let wrong = fb.chunks(4).filter(|p| p[..3] != [3, 2, 1]).count();
    assert_eq!(wrong, 0, "{wrong} pixels were painted over the newer frame");
    let after = rig.r.take_cdf53_stats().unwrap().0;
    assert_eq!(
        after.listed, 0,
        "a held-back tile was drawn after the frame"
    );
    assert!(before.listed > 0 && after.cancelled_superseded + after.dropped_superseded > 0);
}

#[test]
fn a_real_h264_frame_is_not_painted_over_by_a_pending_cdf53_burst() {
    use ghostframe_client_h264::testclip::gradient_clip;
    if !matches!(
        ghostframe_client_h264::driver_reports_h264_decode(
            &ghostframe_client_h264::probe::default_device()
        ),
        Some(true)
    ) {
        eprintln!("no H.264 decode here; the hook test above covers the logic");
        return;
    }
    let clip = gradient_clip(640, 640, 8);
    let feed = |rig: &mut Rig, base: u32| {
        for (i, au) in clip.iter().enumerate() {
            rig.apply(Event::NeedsH264 {
                frame_seq: base + i as u32,
                timestamp_us: i as u32 * 16_667,
                is_keyframe: i == 0,
                ref_frame_seq: u32::MAX,
                flags: 0,
                slots: 0,
                reset_decoder: i == 0,
                payload: au.clone(),
            });
        }
        // Frames come out through `apply_event` (VA-API) or, finished on
        // GStreamer's own threads, through `poll_h264`; no `finish_h264`, so
        // the paths a live session uses are the ones tested.
        for _ in 0..40 {
            rig.r.poll_h264(&rig.ctx).expect("poll");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    };
    // What the burst alone paints, once the grid has drained.
    let burst = |rig: &mut Rig| {
        let w = wire(1, 1, 1);
        for ty in 0..20u8 {
            for tx in 0..20u8 {
                for i in 0..w.passes.len() {
                    rig.apply(pass_event(&w, i, tx, ty, 1, true));
                }
            }
        }
    };
    let mut cdf_only = Rig::new(640, 640);
    burst(&mut cdf_only);
    for _ in 0..8 {
        cdf_only.flush();
    }
    let cdf = cdf_only.r.debug_read_framebuffer(&cdf_only.ctx);

    // The clip after a burst the grid cannot draw in one flush. (The decoder's
    // own output is not bit-stable from run to run -- which frame is last
    // depends on GStreamer's threads -- so compare against the burst, not
    // against another decode.)
    let mut rig = Rig::new(640, 640);
    burst(&mut rig);
    feed(&mut rig, 10);
    // The flushes the access units caused did not spend the grid's budget on
    // tiles the frame was about to cover.
    let (stats, _) = rig.r.take_cdf53_stats().unwrap();
    assert_eq!(stats.listed, 0, "{stats:?}");
    for _ in 0..8 {
        rig.flush();
    }
    let got = rig.r.debug_read_framebuffer(&rig.ctx);
    let same = got
        .chunks(4)
        .zip(cdf.chunks(4))
        .filter(|(a, b)| a == b)
        .count();
    let drawn = got.chunks(4).filter(|p| p[..3] != [0, 0, 0]).count();
    assert!(drawn > 100_000, "the clip drew almost nothing ({drawn} px)");
    assert!(
        same < 4_000,
        "{same} of {} pixels are the CDF 5/3 burst's: it landed on the H.264 picture",
        got.len() / 4
    );
}

// ---- GPU <-> CPU --------------------------------------------------------

#[test]
fn switching_between_gpu_and_cpu_decode_forgets_the_side_that_was_left() {
    let mut rig = Rig::new(64, 32); // 2 tiles of plane store = 10752 B
    rig.r.debug_set_cdf53_limit(Some(11_000));
    let a = wire(1, 3, 1);
    let b = wire(1, 9, 1);
    let pic_b = cpu_picture(&b, b.passes.len()).unwrap();
    assert_ne!(cpu_picture(&a, a.passes.len()).unwrap(), pic_b);
    let dims = |w, h| Event::FrameDimensions {
        width: w,
        height: h,
    };

    // GPU: picture A drawn by the grid.
    for i in 0..a.passes.len() {
        rig.apply(pass_event(&a, i, 0, 0, 1, true));
    }
    rig.flush();
    assert_eq!(rig.r.take_cdf53_stats().unwrap().0.listed, 1);

    // A frame the store cannot hold (4 tiles): CPU decode on the render
    // thread, which accumulates the first half of picture B.
    rig.apply(dims(128, 32));
    rig.w = 128;
    let half = b.passes.len() / 2;
    for i in 0..half {
        rig.apply(pass_event(&b, i, 0, 0, 2, i == 0));
    }
    // Back to a frame that fits: the GPU takes over (stats exist again) and
    // the CPU side's half-accumulated tile is forgotten.
    rig.apply(dims(64, 32));
    rig.w = 64;
    assert!(rig.r.take_cdf53_stats().is_some());
    // Out again. Had the CPU accumulator kept passes 0..half, the remaining
    // passes would now complete picture B; with nothing kept they have no
    // pass 0 and draw nothing.
    rig.apply(dims(128, 32));
    rig.w = 128;
    rig.flush();
    for i in half..b.passes.len() {
        rig.apply(pass_event(&b, i, 0, 0, 2, false));
    }
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert!(
        rig.tile(&fb, 0, 0) != pic_b,
        "stale CPU planes completed a picture after the switch"
    );

    // A fresh generation on the CPU side still works.
    for i in 0..b.passes.len() {
        rig.apply(pass_event(&b, i, 0, 0, 3, i == 0));
    }
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert!(rig.tile(&fb, 0, 0) == pic_b);
}

// ---- export ring --------------------------------------------------------

fn damaged_tiles(rects: &[ghostframe_client_gpu::coalesce::Rect]) -> HashSet<(u32, u32)> {
    let mut out = HashSet::new();
    for r in rects {
        for y in r.y / 32..(r.y + r.h).div_ceil(32) {
            for x in r.x / 32..(r.x + r.w).div_ceil(32) {
                out.insert((x, y));
            }
        }
    }
    out
}

#[test]
fn a_tile_drawn_by_a_later_flush_reaches_the_published_damage() {
    let mut rig = Rig::new(640, 640);
    let w = wire(1, 1, 1);
    for ty in 0..20u8 {
        for tx in 0..20u8 {
            for i in 0..w.passes.len() {
                rig.apply(pass_event(&w, i, tx, ty, 1, true));
            }
        }
    }
    let mut union: HashSet<(u32, u32)> = HashSet::new();
    let mut listed_first = 0;
    let mut per_publish = Vec::new();
    let mut publishes = 0;
    while publishes < 30 {
        let Some(pf) = rig.r.flush_and_publish(&rig.ctx).expect("flush") else {
            panic!("ring starved");
        };
        let rects = match &pf.since_previous {
            Some(sp) => sp.damage.clone(),
            None => pf.damage.clone(),
        };
        if publishes == 0 {
            listed_first = rig.r.take_cdf53_stats().unwrap().0.listed as usize;
        }
        let tiles = damaged_tiles(&rects);
        per_publish.push(tiles.len());
        union.extend(tiles);
        rig.r.release(pf.frame_id);
        publishes += 1;
        if !rig.r.has_deferred_work() {
            break;
        }
    }
    // The first publish is whole-frame; every later one carries exactly what
    // its flush drew, and together they are what the first flush left over.
    assert!(listed_first > 0 && listed_first < 400, "{listed_first}");
    assert!(per_publish.len() >= 3, "{per_publish:?}");
    assert!(
        per_publish[1..].iter().all(|&n| n > 0 && n < 400),
        "{per_publish:?}"
    );
    assert_eq!(
        per_publish[1..].iter().sum::<usize>(),
        400 - listed_first,
        "{per_publish:?}"
    );
    assert_eq!(
        union.len(),
        400,
        "tiles drawn later never reached the damage"
    );
}

#[test]
fn a_late_h264_picture_does_not_cancel_the_cdf53_tiles_of_a_later_frame() {
    // H.264 -> tiles at frame 301: its passes wait for a flush. Picture 300
    // comes out late (the decoder delays its output).
    let mut rig = Rig::new(32, 32);
    let w = wire(2, 1, 1);
    for i in 0..w.passes.len() {
        rig.apply(pass_event(&w, i, 0, 0, 301, true));
    }
    // The picture is older than a waiting pass: not drawn at all...
    assert!(!rig.r.debug_accept_h264_picture(300));
    // ...and were it drawn, it must not cancel the later frame's tile.
    rig.r.supersede_cdf53(300);
    assert!(rig.r.has_deferred_work(), "the 301 tile was cancelled");
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert_eq!(
        rig.tile(&fb, 0, 0),
        cpu_picture(&w, w.passes.len()).unwrap(),
        "the tiles of frame 301 were cancelled by picture 300"
    );
}

#[test]
fn an_h264_picture_older_than_a_tile_on_screen_is_not_drawn_and_a_newer_one_is() {
    let mut rig = Rig::new(32, 32);
    rig.apply(solid_event(0, 0, 301, [10, 20, 30, 255]));
    rig.flush();
    assert!(
        !rig.r.debug_accept_h264_picture(300),
        "stale: tile 301 is on screen"
    );
    assert!(rig.r.debug_accept_h264_picture(301), "equal is drawn");
    assert!(
        !rig.r.debug_accept_h264_picture(300),
        "older than the picture drawn"
    );
}

#[test]
fn h264_pictures_and_tiles_are_ordered_across_the_31_bit_wrap() {
    let mut rig = Rig::new(32, 32);
    rig.apply(solid_event(0, 0, 0x7FFF_FFFF, [10, 20, 30, 255]));
    rig.flush();
    assert!(rig.r.debug_accept_h264_picture(0x7FFF_FFFF));
    assert!(
        rig.r.debug_accept_h264_picture(0),
        "a post-wrap picture is newer"
    );
    assert!(!rig.r.debug_accept_h264_picture(0x7FFF_FFFE));
    // A post-wrap tile paints over the pre-wrap one, and is not dropped.
    rig.apply(solid_event(0, 0, 1, [1, 2, 3, 255]));
    rig.flush();
    let fb = rig.r.debug_read_framebuffer(&rig.ctx);
    assert!(rig.tile(&fb, 0, 0).chunks(4).all(|p| p == [3, 2, 1, 255]));
}
