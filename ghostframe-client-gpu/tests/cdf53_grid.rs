// SPDX-License-Identifier: MIT OR Apache-2.0
//! The full-grid CDF 5/3 decoder (`pipelines::cdf53_grid`) against the CPU
//! reference: `rle_decode` -> `decode_planes` -> `inverse`, bit for bit.
//! Requires a GPU (`WgpuContext::new().expect`: no GPU is a failure, not a
//! skip, as in the other GPU tests of this crate).
//!
//! The expected pixels are built from the codec's own functions, not from the
//! pipeline's inputs, so a mask, prefix or layout mistake on either side shows
//! as a mismatch rather than cancelling out.

use ghostframe_client_gpu::{
    framebuffer::Framebuffer, pipelines::cdf53_grid::Cdf53Grid, wgpu_ctx::WgpuContext,
};
use ghostframe_protocol::codec::cdf53 as codec;

const SENTINEL: [u8; 4] = [1, 2, 3, 0xFF];

// ---- fixtures ------------------------------------------------------------

fn lcg(state: &mut u32) -> u32 {
    *state = state.wrapping_mul(48271).wrapping_add(1);
    *state >> 8
}

/// 32x32 BGRA tile. Kinds: 0 smooth, 1 noise, 2 text-like, 3 black, 4 sparse
/// (flat with a few pixels off), 5 flat white, 6 smooth with a little noise.
fn content(kind: u32, seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(4096);
    let mut rng = seed.wrapping_mul(2654435761).wrapping_add(kind);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let (b, g, r) = match kind {
                0 => (
                    (x * 6 + seed * 11) & 0xFF,
                    (y * 7 + seed * 3 + x) & 0xFF,
                    (128 + x * 2 + y * 2) & 0xFF,
                ),
                1 => (
                    lcg(&mut rng) & 0xFF,
                    lcg(&mut rng) & 0xFF,
                    lcg(&mut rng) & 0xFF,
                ),
                2 => {
                    // Black glyph strokes on white.
                    let on =
                        ((x / 3 + y / 5 + seed).is_multiple_of(3) && (x % 6) < 4) || (y % 11 == 4);
                    let c = if on { 0x10 } else { 0xF0 };
                    (c, c, c)
                }
                3 => (0, 0, 0),
                4 => {
                    let hit = (x * 5 + y * 3 + seed).is_multiple_of(97);
                    if hit {
                        (250, 3, 90)
                    } else {
                        (40, 40, 40)
                    }
                }
                5 => (255, 255, 255),
                _ => (
                    ((x * 4 + y + seed) & 0xFF) + (lcg(&mut rng) & 3),
                    ((y * 5 + seed) & 0xFF) + (lcg(&mut rng) & 3),
                    (200 - x - y) + (lcg(&mut rng) & 3),
                ),
            };
            v.extend_from_slice(&[b.min(255) as u8, g.min(255) as u8, r.min(255) as u8, 0xFF]);
        }
    }
    v
}

/// One tile as the server would send it: per pass, three RLE channel streams.
#[derive(Clone)]
struct TileData {
    present: u16,
    streams: Vec<Option<[Vec<u8>; 3]>>,
}

/// Split a pass payload into its three channel RLE streams (pass 0 first drops
/// the 2-byte `present_passes` prefix).
fn split_payload(pass: u8, payload: &[u8]) -> [Vec<u8>; 3] {
    let mut i = if pass == 0 { 2 } else { 0 };
    let mut out: [Vec<u8>; 3] = Default::default();
    for ch in &mut out {
        let len = u16::from_be_bytes([payload[i], payload[i + 1]]) as usize;
        *ch = payload[i + 2..i + 2 + len].to_vec();
        i += 2 + len;
    }
    assert_eq!(i, payload.len());
    out
}

fn tile_from_coeffs(coeffs: &[i16], sparse: bool) -> TileData {
    let (present, passes) = if sparse {
        codec::encode_passes_sparse(coeffs)
    } else {
        let all = codec::encode_passes(coeffs);
        (
            0x3FFF,
            all.into_iter()
                .enumerate()
                .map(|(p, pl)| (p as u8, pl))
                .collect(),
        )
    };
    let mut streams: Vec<Option<[Vec<u8>; 3]>> = vec![None; 14];
    for (p, payload) in passes {
        streams[p as usize] = Some(split_payload(p, &payload));
    }
    TileData { present, streams }
}

fn tile_from_content(kind: u32, seed: u32, sparse: bool) -> TileData {
    tile_from_coeffs(&codec::forward(&content(kind, seed)), sparse)
}

/// Passes of `t` that have arrived when the resolved prefix is `k`.
fn arrived(t: &TileData, k: u32) -> u32 {
    t.present as u32 & ((1u32 << k) - 1)
}

/// CPU reference: the RGBA tile, or `None` where the tile must not be drawn.
fn expected(t: &TileData, k: u32) -> Option<Vec<u8>> {
    let planes: Vec<Vec<u8>> = (0..k as usize)
        .map(|p| match (&t.streams[p], arrived(t, k) >> p & 1) {
            (Some(s), 1) => s
                .iter()
                .flat_map(|rle| {
                    let d = codec::rle_decode(rle);
                    assert_eq!(d.len(), 128);
                    d
                })
                .collect(),
            _ => vec![0u8; 384],
        })
        .collect();
    let refs: Vec<&[u8]> = planes.iter().map(Vec::as_slice).collect();
    let coeffs = codec::decode_planes(&refs);
    if k < 14 && coeffs.iter().all(|&c| c == 0) {
        return None;
    }
    let bgr = codec::inverse(&coeffs);
    let mut rgba = vec![0xFFu8; 4096];
    for i in 0..1024 {
        rgba[i * 4] = bgr[i * 3 + 2];
        rgba[i * 4 + 1] = bgr[i * 3 + 1];
        rgba[i * 4 + 2] = bgr[i * 3];
    }
    Some(rgba)
}

// ---- rig -----------------------------------------------------------------

struct Rig {
    ctx: WgpuContext,
    grid: Cdf53Grid,
    fb: Framebuffer,
    cols: u32,
}

impl Rig {
    fn new(w: u32, h: u32) -> Self {
        let ctx = WgpuContext::new().expect("wgpu context");
        let cols = w.div_ceil(32);
        let rows = h.div_ceil(32);
        let mut grid = Cdf53Grid::new(&ctx.device);
        grid.resize(&ctx.device, cols, rows).expect("plane store");
        let mut fb = Framebuffer::new(&ctx.device, w, h);
        fb.debug_fill(&ctx.device, &ctx.queue, SENTINEL);
        Rig {
            ctx,
            grid,
            fb,
            cols,
        }
    }

    fn wait(&self) {
        self.ctx
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
    }

    /// One frame: queue the arrived planes of each `(tile, data, k)` and list
    /// the tile; returns the framebuffer afterwards.
    fn frame(&mut self, tiles: &[(u32, &TileData, u32)]) -> Vec<u8> {
        self.grid.begin_frame();
        for &(idx, t, k) in tiles {
            let mask = arrived(t, k);
            for p in 0..14u8 {
                if mask >> p & 1 == 1 {
                    let plane: Vec<u8> = t.streams[p as usize]
                        .as_ref()
                        .unwrap()
                        .iter()
                        .flat_map(|rle| codec::rle_decode(rle))
                        .collect();
                    self.grid.push_plane(idx, p, &plane);
                }
            }
            self.grid.push_tile(idx, mask, k);
        }
        self.submit()
    }

    fn submit(&mut self) -> Vec<u8> {
        let mut enc = self
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        self.grid
            .record(&self.ctx.device, &self.ctx.queue, &mut enc, &self.fb);
        self.ctx.queue.submit(std::iter::once(enc.finish()));
        self.wait();
        self.fb.debug_read(&self.ctx.device, &self.ctx.queue)
    }

    fn tile_rgba(&self, fb: &[u8], idx: u32) -> Vec<u8> {
        let (tx, ty) = (idx % self.cols, idx / self.cols);
        let mut out = vec![0u8; 4096];
        for y in 0..32 {
            for x in 0..32 {
                let (px, py) = (tx * 32 + x, ty * 32 + y);
                if px < self.fb.width && py < self.fb.height {
                    let o = ((py * self.fb.width + px) * 4) as usize;
                    out[((y * 32 + x) * 4) as usize..][..4].copy_from_slice(&fb[o..o + 4]);
                } else {
                    out[((y * 32 + x) * 4) as usize..][..4].copy_from_slice(&[0, 0, 0, 0]);
                }
            }
        }
        out
    }

    /// Assert tile `idx` of `fb` is `want` (clipped to the framebuffer) or, for
    /// `None`, still the sentinel.
    fn check(&self, fb: &[u8], idx: u32, want: Option<Vec<u8>>, what: &str) {
        let got = self.tile_rgba(fb, idx);
        let want = match want {
            Some(mut w) => {
                let (tx, ty) = (idx % self.cols, idx / self.cols);
                for y in 0..32 {
                    for x in 0..32 {
                        if tx * 32 + x >= self.fb.width || ty * 32 + y >= self.fb.height {
                            w[((y * 32 + x) * 4) as usize..][..4].copy_from_slice(&[0, 0, 0, 0]);
                        }
                    }
                }
                w
            }
            None => {
                let mut w = vec![0u8; 4096];
                let (tx, ty) = (idx % self.cols, idx / self.cols);
                for y in 0..32 {
                    for x in 0..32 {
                        if tx * 32 + x < self.fb.width && ty * 32 + y < self.fb.height {
                            w[((y * 32 + x) * 4) as usize..][..4].copy_from_slice(&SENTINEL);
                        }
                    }
                }
                w
            }
        };
        if got != want {
            let i = got.iter().zip(&want).position(|(a, b)| a != b).unwrap();
            panic!(
                "{what}: tile {idx} differs at byte {i} (pixel {}): got {:?}, want {:?}",
                i / 4,
                &got[i / 4 * 4..i / 4 * 4 + 4],
                &want[i / 4 * 4..i / 4 * 4 + 4]
            );
        }
    }
}

// ---- correctness ---------------------------------------------------------

#[test]
fn every_prefix_matches_the_cpu_reference() {
    let mut rig = Rig::new(32, 32);
    let mut drawn = 0;
    let mut skipped = 0;
    for kind in 0..7 {
        for sparse in [false, true] {
            let t = tile_from_content(kind, 3 + kind, sparse);
            for k in 1..=14 {
                rig.fb.debug_fill(&rig.ctx.device, &rig.ctx.queue, SENTINEL);
                let fb = rig.frame(&[(0, &t, k)]);
                let want = expected(&t, k);
                if want.is_some() {
                    drawn += 1;
                } else {
                    skipped += 1;
                }
                rig.check(&fb, 0, want, &format!("kind {kind} sparse {sparse} k {k}"));
            }
        }
    }
    // The comparison must have exercised both outcomes, not just one.
    assert!(drawn > 50 && skipped > 5, "drawn {drawn} skipped {skipped}");
}

#[test]
fn extreme_coefficients_match_the_cpu_reference() {
    // Saturated magnitudes and alternating signs drive the lifting far outside
    // 0..255, so the final clamp and the midpoint add are both exercised.
    let mut rig = Rig::new(32, 32);
    let mut coeffs = vec![0i16; 3072];
    for (i, c) in coeffs.iter_mut().enumerate() {
        *c = match i % 7 {
            0 => 8191,
            1 => -8191,
            2 => 4096,
            3 => -1,
            4 => 1,
            _ => 0,
        };
    }
    let t = tile_from_coeffs(&coeffs, true);
    for k in [2, 5, 13, 14] {
        rig.fb.debug_fill(&rig.ctx.device, &rig.ctx.queue, SENTINEL);
        let fb = rig.frame(&[(0, &t, k)]);
        rig.check(&fb, 0, expected(&t, k), &format!("extreme k {k}"));
    }
}

#[test]
fn many_tiles_at_different_prefixes_and_clipped_edges_in_one_dispatch() {
    // 100 x 70: a 4 x 3 grid whose last column is 4 px wide and last row 6 px.
    let mut rig = Rig::new(100, 70);
    let tiles: Vec<(TileData, u32)> = (0..12u32)
        .map(|i| {
            (
                tile_from_content(i % 7, 20 + i, i % 2 == 0),
                if i == 0 { 1 } else { 5 + (i * 7) % 10 },
            )
        })
        .collect();
    let list: Vec<(u32, &TileData, u32)> = tiles
        .iter()
        .enumerate()
        .map(|(i, (t, k))| (i as u32, t, *k))
        .collect();
    let fb = rig.frame(&list);
    for (i, (t, k)) in tiles.iter().enumerate() {
        rig.check(&fb, i as u32, expected(t, *k), &format!("tile {i} k {k}"));
    }
    // The clipped edge tiles (right column 3, 7, 11; bottom row 8..=11) must
    // have been drawn, or the clipping went unexercised; tile 0 (k = 1) is the
    // one that is not.
    let drawn_edges = [3usize, 7, 8, 9, 10, 11]
        .iter()
        .filter(|&&i| expected(&tiles[i].0, tiles[i].1).is_some())
        .count();
    assert!(drawn_edges >= 4, "only {drawn_edges} clipped tiles drawn");
    assert!(expected(&tiles[0].0, tiles[0].1).is_none());
}

#[test]
fn a_new_generation_is_selected_by_the_mask_alone() {
    let mut rig = Rig::new(32, 32);
    // Generation 1: all 14 planes of dense noise land in the store.
    let old = tile_from_content(1, 5, false);
    let fb = rig.frame(&[(0, &old, 14)]);
    rig.check(&fb, 0, expected(&old, 14), "generation 1");

    // Generation 2: a flat tile, whose sparse encoding omits most magnitude
    // planes -- the store still holds generation 1's bits there.
    let new = tile_from_content(5, 0, true);
    let stale: Vec<usize> = (1..14)
        .filter(|&p| old.present >> p & 1 == 1 && new.present >> p & 1 == 0)
        .collect();
    assert!(stale.len() >= 4, "precondition: stale planes {stale:?}");

    rig.fb.debug_fill(&rig.ctx.device, &rig.ctx.queue, SENTINEL);
    let fb = rig.frame(&[(0, &new, 14)]);
    rig.check(&fb, 0, expected(&new, 14), "generation 2");

    // Control: the same frame with the stale planes in the mask must NOT give
    // the new picture, or the test above proves nothing about the mask.
    rig.fb.debug_fill(&rig.ctx.device, &rig.ctx.queue, SENTINEL);
    rig.grid.begin_frame();
    rig.grid.push_tile(0, 0x3FFF, 14);
    let fb = rig.submit();
    assert_ne!(
        rig.tile_rgba(&fb, 0),
        expected(&new, 14).unwrap(),
        "control: stale planes had no effect"
    );
}

/// Push every plane of `t` (whatever `k` says) and list the tile with `mask`.
fn frame_with_mask(rig: &mut Rig, t: &TileData, mask: u32, k: u32) -> Vec<u8> {
    rig.grid.begin_frame();
    for p in 0..14u8 {
        if t.present >> p & 1 == 1 {
            let plane: Vec<u8> = t.streams[p as usize]
                .as_ref()
                .unwrap()
                .iter()
                .flat_map(|rle| codec::rle_decode(rle))
                .collect();
            rig.grid.push_plane(0, p, &plane);
        }
    }
    rig.grid.push_tile(0, mask, k);
    rig.submit()
}

#[test]
fn mask_bits_at_or_beyond_k_are_ignored() {
    // The store holds all 14 planes of dense noise. A prefix of k < 14 must
    // draw from planes 0..k only, however many bits the mask carries: the
    // shader's `mask & ((1 << k) - 1)` is what keeps a later plane's data out
    // of an earlier prefix.
    let mut rig = Rig::new(32, 32);
    let t = tile_from_content(1, 9, false);
    let mut differing = 0;
    for k in [2u32, 3, 6, 9, 13] {
        for mask in [0x3FFFu32, 0xFFFF] {
            rig.fb.debug_fill(&rig.ctx.device, &rig.ctx.queue, SENTINEL);
            let fb = frame_with_mask(&mut rig, &t, mask, k);
            rig.check(&fb, 0, expected(&t, k), &format!("mask {mask:#x} k {k}"));
        }
        // The prefix really differs from the full picture, so a mask that
        // leaked later planes would be seen.
        if expected(&t, k) != expected(&t, 14) {
            differing += 1;
        }
    }
    assert_eq!(differing, 5);
}

#[test]
fn a_tile_outside_the_plane_store_is_not_drawn() {
    // The framebuffer is 2 x 2 tiles but the store is sized for 2 (cols = 2,
    // rows = 1), so tile 2 lands inside the framebuffer at (0, 1) yet has no
    // planes behind it. Listing it must draw nothing (an unchecked read would
    // be clamped to some other tile's bits, or zero, and a complete tile of
    // that is drawn), while tile 0, listed beside it, is.
    let mut rig = Rig::new(64, 64);
    rig.grid.resize(&rig.ctx.device, 2, 1).expect("fits");
    let t = tile_from_content(1, 4, false);
    rig.grid.begin_frame();
    for p in 0..14u8 {
        let plane: Vec<u8> = t.streams[p as usize]
            .as_ref()
            .unwrap()
            .iter()
            .flat_map(|rle| codec::rle_decode(rle))
            .collect();
        rig.grid.push_plane(0, p, &plane);
    }
    rig.grid.push_tile(0, 0x3FFF, 14);
    rig.grid.push_tile(2, 0x3FFF, 14);
    let fb = rig.submit();
    rig.check(&fb, 0, expected(&t, 14), "the listed tile in the store");
    rig.check(&fb, 2, None, "the tile outside the store");
}

#[test]
fn an_oversized_grid_is_refused_and_leaves_the_grid_unusable() {
    let mut rig = Rig::new(32, 32);
    assert!(rig.grid.is_usable());
    // 256 x 256 tiles (the 8192 px limit) is 352 MB of planes.
    let err = rig
        .grid
        .resize(&rig.ctx.device, 256, 256)
        .expect_err("larger than any binding limit this test GPU has");
    assert!(err.bytes > err.limit);
    assert!(!rig.grid.is_usable());
    // Recording on an unusable grid does nothing and does not panic.
    rig.grid.begin_frame();
    rig.grid.push_tile(0, 1, 1);
    let mut enc = rig
        .ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    assert!(!rig
        .grid
        .record(&rig.ctx.device, &rig.ctx.queue, &mut enc, &rig.fb));
    // And a later, smaller resize brings it back.
    rig.grid.resize(&rig.ctx.device, 1, 1).expect("fits");
    assert!(rig.grid.is_usable());
}

#[test]
fn the_frame_buffer_regrows_between_frames_without_corrupting_either() {
    // A one-tile frame, then one that needs a far bigger upload buffer (and so
    // new bind groups), then the small one again.
    let mut rig = Rig::new(32 * 10, 32 * 4);
    let big: Vec<TileData> = (0..40u32)
        .map(|i| tile_from_content(i % 7, 40 + i, i % 2 == 0))
        .collect();
    let one = tile_from_content(2, 3, false);
    let fb = rig.frame(&[(5, &one, 14)]);
    rig.check(&fb, 5, expected(&one, 14), "small, first");
    let list: Vec<(u32, &TileData, u32)> = big
        .iter()
        .enumerate()
        .map(|(i, t)| (i as u32, t, 14))
        .collect();
    let fb = rig.frame(&list);
    for (i, t) in big.iter().enumerate() {
        rig.check(&fb, i as u32, expected(t, 14), &format!("big, tile {i}"));
    }
    let fb = rig.frame(&[(7, &one, 8)]);
    rig.check(&fb, 7, expected(&one, 8), "small, again");
}

#[test]
fn an_all_zero_prefix_leaves_the_texture_untouched() {
    let mut rig = Rig::new(32, 32);
    // Black: only the (empty) sign plane is sent. Prefixes 1..13 are zero.
    let black = tile_from_content(3, 0, true);
    assert_eq!(black.present, 1);
    // Content whose magnitudes live only in the last planes: a prefix of 6 has
    // sign bits and zero magnitude.
    let mut coeffs = vec![0i16; 3072];
    coeffs[5] = 1;
    coeffs[2000] = -2;
    let faint = tile_from_coeffs(&coeffs, true);
    for (name, t, ks) in [("black", &black, 1..=13), ("faint", &faint, 1..=9)] {
        for k in ks {
            rig.fb.debug_fill(&rig.ctx.device, &rig.ctx.queue, SENTINEL);
            let fb = rig.frame(&[(0, t, k)]);
            assert!(expected(t, k).is_none(), "{name} k {k}: reference draws");
            rig.check(&fb, 0, None, &format!("{name} k {k}"));
        }
    }
    // A complete black tile is a picture and is drawn.
    let fb = rig.frame(&[(0, &black, 14)]);
    rig.check(&fb, 0, expected(&black, 14), "black complete");
    assert!(rig
        .tile_rgba(&fb, 0)
        .chunks(4)
        .all(|p| p == [0, 0, 0, 0xFF]));
}

// ---- benchmark -----------------------------------------------------------

/// Cost of the two stages on this GPU, wall time of submit + poll(Wait) minus
/// an empty submit, median of 21. Run:
/// `cargo test -p ghostframe-client-gpu --no-default-features --features
/// gles,decode-gstreamer,test-support --release --test cdf53_grid -- --ignored
/// --nocapture grid_cost`. `GRID_RECON_SHADER` names a WGSL file to benchmark in
/// place of the shipped reconstruct shader (a variant).
#[test]
#[ignore = "measurement, prints numbers"]
fn grid_cost() {
    use std::time::Instant;
    let ctx = WgpuContext::new().expect("wgpu context");
    let src = |var: &str, default: &str| match std::env::var(var) {
        Ok(path) => std::fs::read_to_string(path).expect("variant shader"),
        Err(_) => default.to_string(),
    };
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../shaders/client/");
    let mut grid = Cdf53Grid::with_shader_sources(
        &ctx.device,
        &src(
            "GRID_RECON_SHADER",
            &std::fs::read_to_string(format!("{root}cdf53_reconstruct.wgsl")).unwrap(),
        ),
    );
    let (cols, rows) = (64u32, 64u32);
    grid.resize(&ctx.device, cols, rows).expect("plane store");
    let mut fb = Framebuffer::new(&ctx.device, cols * 32, rows * 32);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    // A mix of screen-like content: text, smooth gradients, photo-ish, a
    // little noise. Dense encoding so every plane of every tile has a stream.
    let kinds = [2u32, 2, 2, 2, 0, 0, 0, 6, 6, 1];
    let set: Vec<TileData> = (0..40u32)
        .map(|i| tile_from_content(kinds[i as usize % kinds.len()], i, false))
        .collect();
    let tile_of = |i: u32| &set[(i as usize * 7) % set.len()];

    let wait = || {
        ctx.device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll")
    };
    let median = |mut v: Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    let time = |f: &mut dyn FnMut()| {
        let t = Instant::now();
        f();
        Instant::now().duration_since(t).as_secs_f64() * 1e3
    };
    let baseline = median(
        (0..41)
            .map(|_| {
                time(&mut || {
                    let enc = ctx
                        .device
                        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                    ctx.queue.submit(std::iter::once(enc.finish()));
                    wait();
                })
            })
            .collect(),
    );
    println!("empty submit + poll baseline: {baseline:.3} ms");

    let fb_size = (fb.width, fb.height);
    let run = |grid: &mut Cdf53Grid, fb: &Framebuffer| {
        let mut enc = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        grid.record_reconstruct(&ctx.device, &mut enc, fb);
        ctx.queue.submit(std::iter::once(enc.finish()));
        wait();
    };
    // Fill the store with every plane of every tile, so stage B reads real
    // data (planes go through the production route, a few hundred tiles at a
    // time).
    for chunk in (0..cols * rows).collect::<Vec<_>>().chunks(512) {
        grid.begin_frame();
        for &i in chunk {
            let t = tile_of(i);
            for p in 0..14u8 {
                let plane: Vec<u8> = t.streams[p as usize]
                    .as_ref()
                    .unwrap()
                    .iter()
                    .flat_map(|rle| codec::rle_decode(rle))
                    .collect();
                grid.push_plane(i, p, &plane);
            }
        }
        let mut enc = ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        grid.upload(&ctx.device, &ctx.queue, fb_size);
        grid.record_planes(&ctx.device, &mut enc);
        ctx.queue.submit(std::iter::once(enc.finish()));
        wait();
    }

    // The renderer's route: decoded planes pushed on the render thread and
    // written in ONE queue write. Host cost per frame, no GPU wait.
    let plane = vec![0x5Au8; 384];
    for per_tile in [1usize, 14] {
        for n in [64u32, 512, 2040] {
            let push = median(
                (0..21)
                    .map(|_| {
                        time(&mut || {
                            grid.begin_frame();
                            for i in 0..n {
                                for p in 0..per_tile {
                                    grid.push_plane(i, p as u8, &plane);
                                }
                                grid.push_tile(i, (1 << per_tile) - 1, per_tile as u32);
                            }
                        })
                    })
                    .collect(),
            );
            let bytes = grid.upload_bytes();
            let up = median(
                (0..21)
                    .map(|_| time(&mut || grid.upload(&ctx.device, &ctx.queue, fb_size)))
                    .collect(),
            );
            let both = median(
                (0..21)
                    .map(|_| {
                        time(&mut || {
                            grid.begin_frame();
                            for i in 0..n {
                                for p in 0..per_tile {
                                    grid.push_plane(i, p as u8, &plane);
                                }
                                grid.push_tile(i, (1 << per_tile) - 1, per_tile as u32);
                            }
                            grid.upload(&ctx.device, &ctx.queue, fb_size);
                        })
                    })
                    .collect(),
            );
            // Drain so the next write is not queued behind a backlog.
            wait();
            println!(
                "render-thread CPU, {n:4} tiles x {per_tile:2} planes ({} KB): push {push:.3} ms, upload (1 write) {up:.3} ms, push+upload {both:.3} ms",
                bytes / 1024
            );
        }
    }

    // Stage B: the store holds every plane of every tile now.
    for k in [1u32, 6, 14] {
        for n in [64u32, 512, 2040, 4096] {
            let cpu = median(
                (0..21)
                    .map(|_| {
                        time(&mut || {
                            grid.begin_frame();
                            for i in 0..n {
                                grid.push_tile(i, (1 << k) - 1, k);
                            }
                        })
                    })
                    .collect(),
            );
            grid.upload(&ctx.device, &ctx.queue, fb_size);
            run(&mut grid, &fb); // warm
            let gpu = median((0..21).map(|_| time(&mut || run(&mut grid, &fb))).collect());
            println!(
                "stage B K={k:2} N={n:4}: gpu {gpu:.3} ms (net {:.3}, {:.1} us/tile), cpu list {cpu:.3} ms",
                gpu - baseline,
                (gpu - baseline) * 1e3 / n as f64
            );
        }
    }
}

// ---- self-test -----------------------------------------------------------

#[test]
fn the_probe_passes_on_the_shipped_shaders_and_measures_a_cost() {
    use ghostframe_client_gpu::pipelines::cdf53_grid::probe;
    let ctx = WgpuContext::new().expect("wgpu context");
    let probed = probe(&ctx.device, &ctx.queue).expect("shipped shaders");
    assert!(
        probed.us_per_tile_k14 > 0.5 && probed.us_per_tile_k14 < 500.0,
        "{}",
        probed.us_per_tile_k14
    );
    eprintln!("probe: {:.1} us/tile at k=14", probed.us_per_tile_k14);
    assert!(probed.grid.is_usable());
}

#[test]
fn the_device_gets_the_adapters_storage_binding_limit() {
    // Not the 128 MiB of `downlevel_defaults`: that would send an 8K frame's
    // plane store to the CPU on a GPU that could hold it.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (dev, adapter) = (ctx.device.limits(), ctx.adapter.limits());
    assert_eq!(
        dev.max_storage_buffer_binding_size,
        adapter.max_storage_buffer_binding_size
    );
    assert_eq!(dev.max_buffer_size, adapter.max_buffer_size);
    eprintln!(
        "storage binding {} MiB, max buffer {} MiB",
        dev.max_storage_buffer_binding_size >> 20,
        dev.max_buffer_size >> 20
    );
}

fn recon_source() -> String {
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/../shaders/client/");
    std::fs::read_to_string(format!("{root}cdf53_reconstruct.wgsl")).unwrap()
}

fn probe_error(variant: &str) -> String {
    use ghostframe_client_gpu::pipelines::cdf53_grid::probe_with;
    let ctx = WgpuContext::new().expect("wgpu context");
    match probe_with(&ctx.device, &ctx.queue, |d| {
        Cdf53Grid::with_shader_sources(d, variant)
    }) {
        Ok(_) => panic!("the probe accepted a broken shader"),
        Err(e) => e,
    }
}

#[test]
fn the_probe_rejects_a_shader_that_writes_nothing() {
    // Compiles, binds, writes nothing: what a miscompile looks like.
    let empty = "@group(0) @binding(0) var<storage, read> planes: array<u32>;\n\
        @group(0) @binding(1) var<storage, read> frame: array<u32>;\n\
        @group(0) @binding(2) var framebuffer: texture_storage_2d<rgba8unorm, write>;\n\
        @group(0) @binding(3) var<uniform> slots: array<vec4<u32>, 128>;\n\
        @compute @workgroup_size(96) fn main() { let x = planes[0] + frame[0] + slots[0].x; \
        if (x == 0xFFFFFFFFu) { textureStore(framebuffer, vec2<u32>(0u), vec4<f32>(0.0)); } }\n";
    let err = probe_error(empty);
    assert!(err.contains("frame 1") && err.contains("pixel"), "{err}");
}

#[test]
fn the_probe_rejects_a_shader_that_is_wrong_only_on_the_second_frame() {
    // Midpoint correction removed: complete prefixes (frame 1) are right, the
    // partial one (frame 2) is not.
    let src = recon_source();
    let wrong = src.replace("midpoint = 1u << (13u - k_prefix);", "midpoint = 0u;");
    assert_ne!(wrong, src, "precondition: the edit applied");
    let err = probe_error(&wrong);
    assert!(err.contains("frame 2"), "frame 1 should have passed: {err}");
}

#[test]
fn the_probe_rejects_a_shader_that_draws_nothing_on_the_second_frame() {
    // Partial prefixes are skipped. Frame 2 then leaves frame 1's picture in
    // place; the probe must see that it is not the partial one.
    let src = recon_source();
    let wrong = src.replace(
        "if (k_prefix == 0u ||",
        "if (k_prefix == 0u || k_prefix < 14u ||",
    );
    assert_ne!(wrong, src, "precondition: the edit applied");
    let err = probe_error(&wrong);
    assert!(err.contains("frame 2"), "{err}");
}

#[test]
fn a_shader_that_does_not_compile_is_an_error_not_a_panic() {
    let err = probe_error("this is not wgsl");
    assert!(!err.is_empty());
}

// ---- host contract ---------------------------------------------------------
//
// `debug_assert!`s: they exist in debug builds only (`just test-client-gles`
// runs them; a `--release` run compiles them out).

#[cfg(debug_assertions)]
mod contract {
    use super::*;

    fn rig() -> Rig {
        Rig::new(64, 32)
    }

    #[test]
    #[should_panic(expected = "listed twice")]
    fn a_tile_listed_twice_in_a_frame_is_refused() {
        let mut r = rig();
        r.grid.begin_frame();
        r.grid.push_tile(0, 1, 1);
        r.grid.push_tile(0, 1, 2);
    }

    #[test]
    #[should_panic(expected = "given twice")]
    fn a_plane_given_twice_in_a_frame_is_refused() {
        let mut r = rig();
        r.grid.begin_frame();
        r.grid.push_plane(0, 3, &[0; 384]);
        r.grid.push_plane(0, 3, &[0; 384]);
    }

    #[test]
    #[should_panic(expected = "> 14")]
    fn a_prefix_longer_than_fourteen_is_refused() {
        let mut r = rig();
        r.grid.begin_frame();
        r.grid.push_tile(0, 0x3FFF, 15);
    }

    #[test]
    #[should_panic(expected = "twice without begin_frame")]
    fn recording_a_frame_twice_is_refused() {
        let mut r = rig();
        r.grid.begin_frame();
        r.grid.push_tile(0, 1, 1);
        let mut enc = r
            .ctx
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        r.grid.record(&r.ctx.device, &r.ctx.queue, &mut enc, &r.fb);
        r.grid.record(&r.ctx.device, &r.ctx.queue, &mut enc, &r.fb);
    }
}
