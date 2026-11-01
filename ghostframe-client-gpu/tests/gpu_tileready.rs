//! `Event::TileReady` -> framebuffer. Needs a real GPU, so not named in any
//! CI workflow (same rule as the other `gpu_*.rs` files).
//!
//! This path had no test at all. `gpu_oracle.rs` mentions `TileReady`, but only
//! to build its CPU *reference* -- it never feeds one to a `Renderer`. That gap
//! mattered the moment the uploads were batched: `upload_pending_rgba` merges
//! tiles into coalesced rectangles and assembles each into one buffer, which is
//! a lot of index arithmetic standing between a tile and its pixels.
//!
//! The cases here are chosen to break that arithmetic if it is wrong:
//! neighbouring tiles that coalesce into one rectangle, a distant tile that
//! cannot, a tile on the clamped right/bottom edge, and a short payload that
//! takes the individual fallback.

use ghostframe_client_core::Event;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_protocol::tile::TILE_SIZE;

const W: u32 = 1920;
const H: u32 = 1080;

fn solid_tile(rgba: [u8; 4]) -> Vec<u8> {
    rgba.iter()
        .copied()
        .cycle()
        .take((TILE_SIZE * TILE_SIZE * 4) as usize)
        .collect()
}

fn tile_ready(tile_x: u8, tile_y: u8, rgba: Vec<u8>) -> Event {
    Event::TileReady {
        tile_x,
        tile_y,
        rgba,
        frame_seq: 0,
    }
}

/// Pixel at `(x, y)` from a tight RGBA framebuffer readback.
fn px(fb: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [fb[i], fb[i + 1], fb[i + 2], fb[i + 3]]
}

fn renderer(ctx: &WgpuContext) -> Renderer {
    Renderer::new(ctx, W, H, 3, &[], false, false).expect("renderer")
}

#[test]
fn coalesced_and_isolated_tiles_land_at_their_own_coordinates() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);

    // (1,1) and (2,1) are neighbours and coalesce into one rectangle; (10,7)
    // is far away and must become its own. Distinct colours so a tile landing
    // in the wrong rectangle -- or at the rectangle's origin instead of its own
    // offset -- shows up as the wrong colour rather than merely "not black".
    let red = [0xFF, 0x00, 0x00, 0xFF];
    let green = [0x00, 0xFF, 0x00, 0xFF];
    let blue = [0x00, 0x00, 0xFF, 0xFF];
    r.apply_event(&ctx, tile_ready(1, 1, solid_tile(red)))
        .expect("gpu ok");
    r.apply_event(&ctx, tile_ready(2, 1, solid_tile(green)))
        .expect("gpu ok");
    r.apply_event(&ctx, tile_ready(10, 7, solid_tile(blue)))
        .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");

    let fb = r.debug_read_framebuffer(&ctx);

    // Centre of each tile, not its origin: an origin-only check passes even
    // when the row stride inside the assembled rectangle is wrong.
    let c = TILE_SIZE / 2;
    assert_eq!(px(&fb, TILE_SIZE + c, TILE_SIZE + c), red, "tile (1,1)");
    assert_eq!(
        px(&fb, 2 * TILE_SIZE + c, TILE_SIZE + c),
        green,
        "tile (2,1)"
    );
    assert_eq!(
        px(&fb, 10 * TILE_SIZE + c, 7 * TILE_SIZE + c),
        blue,
        "tile (10,7)"
    );

    // A tile nobody wrote must still be untouched.
    assert_eq!(
        px(&fb, 5 * TILE_SIZE + c, 5 * TILE_SIZE + c),
        [0, 0, 0, 0],
        "unwritten tile"
    );
}

#[test]
fn every_pixel_of_a_coalesced_rectangle_comes_from_its_own_tile() {
    // The failure this catches: assembling the rectangle with the wrong
    // destination stride, which leaves each tile correct at its first row and
    // progressively skewed after it. Checking all four corners of both tiles
    // in one rectangle is what makes a skew visible.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);

    let a = [0x11, 0x22, 0x33, 0xFF];
    let b = [0x44, 0x55, 0x66, 0xFF];
    r.apply_event(&ctx, tile_ready(3, 4, solid_tile(a)))
        .expect("gpu ok");
    r.apply_event(&ctx, tile_ready(4, 4, solid_tile(b)))
        .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");
    let fb = r.debug_read_framebuffer(&ctx);

    for (tx, want) in [(3u32, a), (4u32, b)] {
        let (x0, y0) = (tx * TILE_SIZE, 4 * TILE_SIZE);
        for (dx, dy) in [
            (0, 0),
            (TILE_SIZE - 1, 0),
            (0, TILE_SIZE - 1),
            (TILE_SIZE - 1, TILE_SIZE - 1),
        ] {
            assert_eq!(
                px(&fb, x0 + dx, y0 + dy),
                want,
                "tile ({tx},4) corner (+{dx},+{dy})"
            );
        }
    }
}

#[test]
fn edge_tiles_are_clamped_not_dropped() {
    // 1080 is 33.75 tiles, so tile row 33 is only 24px tall; 1920 is exactly 60
    // tiles. A rectangle touching the bottom edge must upload the rows that
    // exist and not overrun -- wgpu validation turns an overrun into a panic,
    // so this test fails loudly either way.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);

    let last_col = (W / TILE_SIZE - 1) as u8;
    let last_row = (H.div_ceil(TILE_SIZE) - 1) as u8;
    let colour = [0x9A, 0xBC, 0xDE, 0xFF];
    r.apply_event(&ctx, tile_ready(last_col, last_row, solid_tile(colour)))
        .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");

    let fb = r.debug_read_framebuffer(&ctx);
    assert_eq!(px(&fb, W - 1, H - 1), colour, "bottom-right pixel");
}

#[test]
fn a_short_payload_writes_only_the_rows_it_has() {
    // Short payloads take the individual-upload fallback precisely so they
    // cannot leave buffer fill over live pixels. Pre-fill the tile with a full
    // payload, then send a half-height one: the top half must change and the
    // bottom half must not.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);

    let base = [0x20, 0x40, 0x60, 0xFF];
    r.apply_event(&ctx, tile_ready(6, 6, solid_tile(base)))
        .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");

    let half = [0xAA, 0xBB, 0xCC, 0xFF];
    let rows = (TILE_SIZE / 2) as usize;
    let short: Vec<u8> = half
        .iter()
        .copied()
        .cycle()
        .take(rows * (TILE_SIZE * 4) as usize)
        .collect();
    r.apply_event(&ctx, tile_ready(6, 6, short))
        .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");

    let fb = r.debug_read_framebuffer(&ctx);
    let (x0, y0) = (6 * TILE_SIZE, 6 * TILE_SIZE);
    assert_eq!(px(&fb, x0 + 4, y0 + 4), half, "top half overwritten");
    assert_eq!(
        px(&fb, x0 + 4, y0 + TILE_SIZE - 4),
        base,
        "bottom half must survive a short payload"
    );
}

/// A hostile server's dimensions must neither allocate nor panic. 100000^2
/// RGBA would be 40 GB; the point is that it is refused before sizing.
#[test]
fn hostile_dimensions_are_refused_without_allocating() {
    let ctx = WgpuContext::new().expect("wgpu context");

    for (w, h) in [(100_000, 100_000), (0, 1080), (1920, 0), (8193, 1080)] {
        match Renderer::new(&ctx, w, h, 3, &[], false, false) {
            Err(ghostframe_client_gpu::GpuError::InvalidDimensions { width, height }) => {
                assert_eq!((width, height), (w, h));
            }
            Err(e) => panic!("{w}x{h}: wrong error {e}"),
            Ok(_) => panic!("{w}x{h} must be refused"),
        }
    }

    // Mid-session: the framebuffer keeps its size and stays usable.
    let mut r = renderer(&ctx);
    for (w, h) in [(100_000, 100_000), (0, 0), (u32::MAX, 1)] {
        r.apply_event(
            &ctx,
            Event::FrameDimensions {
                width: w,
                height: h,
            },
        )
        .expect("gpu ok");
    }
    let red = [0xFF, 0x00, 0x00, 0xFF];
    r.apply_event(&ctx, tile_ready(1, 1, solid_tile(red)))
        .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");
    let fb = r.debug_read_framebuffer(&ctx);
    assert_eq!(fb.len(), (W * H * 4) as usize, "framebuffer was resized");
    assert_eq!(px(&fb, 48, 48), red);
}

/// A PalRle tile the shader rejects must come out of the renderer as a
/// decode error naming that tile, and a clean tile in the same batch must not.
/// Before this, the native renderer never read the GPU's error slots back, so
/// the server could never learn a tile had failed to decode.
#[test]
fn gpu_palrle_failures_surface_as_decode_errors() {
    use ghostframe_client_core::{DecodeErrorCode, TileData};
    use ghostframe_client_gpu::renderer::GpuDecodeError;
    use ghostframe_protocol::protocol::Codec;

    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    r.set_collect_decode_errors(true);
    r.apply_event(
        &ctx,
        Event::PaletteUpdated {
            palette_id: 0,
            colors: vec![[1, 2, 3, 0xFF]],
        },
    )
    .expect("palette");
    let payload = |tile_x, tile_y, fill: u8| Event::TilePayload {
        frame_seq: 0,
        tile_x,
        tile_y,
        generation: 0,
        data: TileData::PalRle {
            palette_id: 0,
            count: 1,
            indices: vec![fill; 512],
            wire_flags: 0,
        },
    };
    // Tile (3,5) indexes past `count`; tile (4,5) is fine.
    r.apply_event(&ctx, payload(3, 5, 0x77)).expect("apply");
    r.apply_event(&ctx, payload(4, 5, 0x00)).expect("apply");
    r.flush(&ctx).expect("flush");

    assert_eq!(
        r.take_decode_errors(),
        vec![GpuDecodeError {
            codec: Codec::PalRle,
            tile_x: 3,
            tile_y: 5,
            code: DecodeErrorCode::IndexOob,
        }]
    );
    assert!(r.take_decode_errors().is_empty(), "drained");

    // A clean batch afterwards reports nothing (no stale slot).
    r.apply_event(&ctx, payload(4, 5, 0x00)).expect("apply");
    r.flush(&ctx).expect("flush");
    assert!(r.take_decode_errors().is_empty());
}

/// A tile whose every pixel is distinct and says where it came from, so a
/// pixel landing anywhere but its own place -- another row, another column,
/// another tile -- reads back wrong.
fn unique_tile(tile_x: u8, tile_y: u8, salt: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity((TILE_SIZE * TILE_SIZE * 4) as usize);
    for y in 0..TILE_SIZE as u8 {
        for x in 0..TILE_SIZE as u8 {
            v.extend_from_slice(&[
                x ^ tile_x.wrapping_mul(7),
                y ^ tile_y.wrapping_mul(13),
                salt,
                0xFF,
            ]);
        }
    }
    v
}

#[test]
fn both_upload_routes_put_every_pixel_where_it_belongs() {
    // `upload_pending_rgba` sends a rectangle of 32+ tiles through one
    // `write_texture` and everything smaller through the RGBA compute
    // pipeline. Compare the WHOLE framebuffer against a CPU composition, with
    // tiles taking each route, on both clamped edges, and one tile sent twice
    // in the batch (the later must win).
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    let mut want = vec![0u8; (W * H * 4) as usize];
    let mut put = |r: &mut Renderer, tx: u8, ty: u8, rgba: Vec<u8>| {
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let (fx, fy) = (tx as u32 * TILE_SIZE + x, ty as u32 * TILE_SIZE + y);
                if fx < W && fy < H {
                    let s = ((y * TILE_SIZE + x) * 4) as usize;
                    let d = ((fy * W + fx) * 4) as usize;
                    want[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
                }
            }
        }
        r.apply_event(&ctx, tile_ready(tx, ty, rgba))
            .expect("gpu ok");
    };

    // 8x4 = 32 tiles: the write_texture route.
    for ty in 2..6 {
        for tx in 20..28 {
            put(&mut r, tx, ty, unique_tile(tx, ty, 1));
        }
    }
    // Scattered: the pipeline route. Includes the bottom-right corner (24px
    // tall) and a diagonal no two of which touch.
    let last_col = (W / TILE_SIZE - 1) as u8;
    let last_row = (H.div_ceil(TILE_SIZE) - 1) as u8;
    for (tx, ty) in [
        (0, 0),
        (3, 9),
        (5, 11),
        (40, 20),
        (last_col, last_row),
        (0, last_row),
    ] {
        put(&mut r, tx, ty, unique_tile(tx, ty, 2));
    }
    // Sent twice: first one content, then another.
    put(&mut r, 9, 9, unique_tile(9, 9, 3));
    put(&mut r, 9, 9, unique_tile(9, 9, 4));
    r.flush(&ctx).expect("gpu ok");

    let fb = r.debug_read_framebuffer(&ctx);
    if let Some(i) = (0..want.len() / 4).find(|&i| fb[i * 4..i * 4 + 4] != want[i * 4..i * 4 + 4]) {
        let (x, y) = (i as u32 % W, i as u32 / W);
        panic!(
            "pixel ({x},{y}) in tile ({},{}): got {:?}, want {:?}",
            x / TILE_SIZE,
            y / TILE_SIZE,
            &fb[i * 4..i * 4 + 4],
            &want[i * 4..i * 4 + 4]
        );
    }
}

// ---------------------------------------------------------------------------
// The upload-slab route: tiles the decode thread wrote straight into a
// CPU-mapped texture, named to the renderer by (slab, slot).
// ---------------------------------------------------------------------------

use ghostframe_client_core::{TileData, TileSink};
use ghostframe_client_gpu::upload_pool::{SlotAllocator, SlotRef, UploadPool};

/// `UploadPool::with_slabs` makes 1024-slot slabs.
const SLOTS_PER_SLAB: u32 = 1024;

/// Write one 32x32 tile (tight RGBA) into the next slot, as the decode thread
/// does, and return the slot.
fn put_slot(alloc: &mut SlotAllocator, rgba: &[u8]) -> SlotRef {
    let mut out = alloc.acquire().expect("a free slot");
    for y in 0..TILE_SIZE as usize {
        out.row_mut(y)
            .copy_from_slice(&rgba[y * 128..(y + 1) * 128]);
    }
    alloc.commit(0, 0, 0)
}

/// A renderer with a pool attached, and the allocator that feeds it.
fn slab_renderer(ctx: &WgpuContext, slabs: usize) -> (Renderer, UploadPool, SlotAllocator) {
    let (pool, alloc) = UploadPool::with_slabs(ctx, slabs).expect("upload pool");
    let mut r = renderer(ctx);
    r.attach_upload_pool(ctx, &pool);
    (r, pool, alloc)
}

fn send_slot(r: &mut Renderer, alloc: &mut SlotAllocator, seq: u32, tx: u8, ty: u8, rgba: &[u8]) {
    let slot = put_slot(alloc, rgba);
    alloc.end_cpu_writes();
    assert!(r.apply_slot_tile(seq, tx, ty, slot), "pool attached");
}

fn send_vec(ctx: &WgpuContext, r: &mut Renderer, seq: u32, tx: u8, ty: u8, rgba: Vec<u8>) {
    r.apply_event(
        ctx,
        Event::TileReady {
            frame_seq: seq,
            tile_x: tx,
            tile_y: ty,
            rgba,
        },
    )
    .expect("gpu ok");
}

/// Composes tiles on the CPU in the order the test lists their winners.
struct Want(Vec<u8>);

impl Want {
    fn new() -> Self {
        Want(vec![0u8; (W * H * 4) as usize])
    }

    fn put(&mut self, tx: u8, ty: u8, rgba: &[u8]) {
        for y in 0..TILE_SIZE {
            for x in 0..TILE_SIZE {
                let (fx, fy) = (tx as u32 * TILE_SIZE + x, ty as u32 * TILE_SIZE + y);
                if fx < W && fy < H {
                    let s = ((y * TILE_SIZE + x) * 4) as usize;
                    let d = ((fy * W + fx) * 4) as usize;
                    self.0[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
                }
            }
        }
    }

    fn assert_matches(&self, fb: &[u8]) {
        if let Some(i) =
            (0..self.0.len() / 4).find(|&i| fb[i * 4..i * 4 + 4] != self.0[i * 4..i * 4 + 4])
        {
            let (x, y) = (i as u32 % W, i as u32 / W);
            panic!(
                "pixel ({x},{y}) in tile ({},{}): got {:?}, want {:?}",
                x / TILE_SIZE,
                y / TILE_SIZE,
                &fb[i * 4..i * 4 + 4],
                &self.0[i * 4..i * 4 + 4]
            );
        }
    }
}

#[test]
fn slab_route_puts_every_pixel_where_it_belongs() {
    // The slab route, whole framebuffer against a CPU composition: scattered
    // tiles, both clamped edges, a dense block, duplicates, and every
    // interleaving with Vec tiles and another codec that decides the winner.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, _pool, mut alloc) = slab_renderer(&ctx, 2);
    let mut want = Want::new();

    // A dense 8x4 block and a scatter, all from slabs.
    for ty in 2..6u8 {
        for tx in 20..28u8 {
            let t = unique_tile(tx, ty, 1);
            send_slot(&mut r, &mut alloc, 0, tx, ty, &t);
            want.put(tx, ty, &t);
        }
    }
    let last_col = (W / TILE_SIZE - 1) as u8;
    let last_row = (H.div_ceil(TILE_SIZE) - 1) as u8;
    for (tx, ty) in [
        (0, 0),
        (3, 9),
        (5, 11),
        (40, 20),
        (last_col, last_row),
        (0, last_row),
    ] {
        let t = unique_tile(tx, ty, 2);
        send_slot(&mut r, &mut alloc, 0, tx, ty, &t);
        want.put(tx, ty, &t);
    }
    // A Vec rectangle (the staged-copy route) and Vec singles beside them.
    for ty in 2..6u8 {
        for tx in 30..38u8 {
            let t = unique_tile(tx, ty, 6);
            send_vec(&ctx, &mut r, 0, tx, ty, t.clone());
            want.put(tx, ty, &t);
        }
    }
    for tx in [10u8, 11] {
        let t = unique_tile(tx, 2, 5);
        send_vec(&ctx, &mut r, 0, tx, 2, t.clone());
        want.put(tx, 2, &t);
    }
    // Sent twice from slabs: the later wins.
    send_slot(&mut r, &mut alloc, 0, 9, 9, &unique_tile(9, 9, 3));
    let t = unique_tile(9, 9, 4);
    send_slot(&mut r, &mut alloc, 0, 9, 9, &t);
    want.put(9, 9, &t);
    // Slab then Vec for one tile: the Vec arrived last and wins.
    send_slot(&mut r, &mut alloc, 0, 12, 12, &unique_tile(12, 12, 7));
    let t = unique_tile(12, 12, 8);
    send_vec(&ctx, &mut r, 0, 12, 12, t.clone());
    want.put(12, 12, &t);
    // Vec then slab: the slab wins.
    send_vec(&ctx, &mut r, 0, 13, 12, unique_tile(13, 12, 9));
    let t = unique_tile(13, 12, 10);
    send_slot(&mut r, &mut alloc, 0, 13, 12, &t);
    want.put(13, 12, &t);
    // Another codec, equal frame_seq: TileReady is drawn after Solid, so the
    // slab tile wins. At a newer frame_seq the Solid wins and the older slab
    // tile is dropped without being drawn.
    let solid = |seq, tx, ty, bgra| Event::TilePayload {
        frame_seq: seq,
        tile_x: tx,
        tile_y: ty,
        generation: 0,
        data: TileData::Solid(bgra),
    };
    r.apply_event(&ctx, solid(0, 14, 12, [1, 2, 3, 4]))
        .expect("ok");
    let t = unique_tile(14, 12, 11);
    send_slot(&mut r, &mut alloc, 0, 14, 12, &t);
    want.put(14, 12, &t);
    send_slot(&mut r, &mut alloc, 0, 15, 12, &unique_tile(15, 12, 12));
    r.apply_event(&ctx, solid(1, 15, 12, [10, 20, 30, 255]))
        .expect("ok");
    want.put(15, 12, &solid_tile([30, 20, 10, 255]));
    // And a Solid on a tile no slab touches still draws.
    r.apply_event(&ctx, solid(0, 16, 12, [50, 60, 70, 255]))
        .expect("ok");
    want.put(16, 12, &solid_tile([70, 60, 50, 255]));

    r.flush(&ctx).expect("gpu ok");
    want.assert_matches(&r.debug_read_framebuffer(&ctx));
    // The frame used one submit however many routes it took.
    assert_eq!(r.last_flush_submits(), 1);
}

#[test]
fn slab_tiles_land_across_a_slab_boundary() {
    // 1100 tiles: slot indices wrap from the last slot of slab 0 to slot 0 of
    // slab 1 mid-batch, so one flush needs two dispatches and two work tables
    // at different offsets.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, pool, mut alloc) = slab_renderer(&ctx, 2);
    let mut want = Want::new();
    let mut slabs_used = std::collections::BTreeSet::new();
    for i in 0..1100u32 {
        let (tx, ty) = ((i % 60) as u8, (i / 60) as u8);
        let t = unique_tile(tx, ty, 21);
        let slot = put_slot(&mut alloc, &t);
        slabs_used.insert(slot.slab());
        alloc.end_cpu_writes();
        assert!(r.apply_slot_tile(0, tx, ty, slot));
        want.put(tx, ty, &t);
    }
    assert_eq!(slabs_used.len(), 2, "the batch must span both slabs");
    r.flush(&ctx).expect("gpu ok");
    want.assert_matches(&r.debug_read_framebuffer(&ctx));
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!(
        (0..2).map(|s| pool.consumed(s)).sum::<u32>(),
        1100,
        "every slot of the flush is retired once the GPU is done"
    );
}

#[test]
fn a_slab_is_reused_only_after_the_gpu_has_consumed_it() {
    // One slab: once its 1024 slots are issued there is nothing else to hand
    // out, so reuse can only come from retirement.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, pool, mut alloc) = slab_renderer(&ctx, 1);
    let mut want = Want::new();
    let colour = |i: u32| solid_tile([(i % 251) as u8, 7, 9, 255]);
    for i in 0..SLOTS_PER_SLAB {
        let (tx, ty) = ((i % 60) as u8, (i / 60) as u8);
        let t = colour(i);
        send_slot(&mut r, &mut alloc, 0, tx, ty, &t);
        want.put(tx, ty, &t);
    }
    // Issued and announced, not yet drawn: not reusable, and the allocator
    // says so rather than blocking.
    assert!(alloc.acquire().is_none(), "full slab, nothing consumed");
    assert_eq!(pool.consumed(0), 0);
    assert_eq!(alloc.exhausted(), 1);

    r.flush(&ctx).expect("gpu ok");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!(pool.consumed(0), SLOTS_PER_SLAB, "retired after completion");
    // Retired: the slab comes back (under a new generation) and holds new
    // content, while the framebuffer keeps what the first use drew.
    let old = put_slot_then_gen(&mut alloc);
    assert_ne!(old.1, 1, "second use of the slab has a new generation");
    want.assert_matches(&r.debug_read_framebuffer(&ctx));
}

/// Take a slot and return it with the generation it came with.
fn put_slot_then_gen(alloc: &mut SlotAllocator) -> (SlotRef, u32) {
    let s = put_slot(alloc, &solid_tile([1, 1, 1, 255]));
    alloc.end_cpu_writes();
    (s, s.generation())
}

#[test]
fn an_exhausted_pool_falls_back_to_vec_tiles_in_arrival_order() {
    // The real decode path: `decode_into` over the allocator. After the one
    // slab is spent, tiles arrive as `TileReady`; mixed on one tile, the later
    // arrival wins whichever kind it is.
    use ghostframe_client_core::{Decoded, TileDecoder};
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, _pool, mut alloc) = slab_renderer(&ctx, 1);
    let mut dec = TileDecoder::new();
    let mut want = Want::new();
    let payload = |tx: u8, ty: u8, bgra| Event::TilePayload {
        frame_seq: 0,
        tile_x: tx,
        tile_y: ty,
        generation: 0,
        data: TileData::Solid(bgra),
    };
    let mut apply = |r: &mut Renderer, alloc: &mut SlotAllocator, tx: u8, ty: u8, c: u8| match dec
        .decode_into(payload(tx, ty, [c, c, c, 255]), alloc)
        .expect("tile")
    {
        Decoded::Placed {
            frame_seq,
            tile_x,
            tile_y,
            placed,
        } => {
            alloc.end_cpu_writes();
            assert!(r.apply_slot_tile(frame_seq, tile_x, tile_y, placed));
            true
        }
        Decoded::Event(ev) => {
            r.apply_event(&ctx, ev).expect("gpu ok");
            false
        }
    };
    // Spend the slab on tiles that do not collide with the test below.
    for i in 0..SLOTS_PER_SLAB {
        let (tx, ty) = (20 + (i % 20) as u8, (i / 20) as u8 % 20 + 14);
        assert!(
            apply(&mut r, &mut alloc, tx, ty, 9),
            "slot {i} should be a slab tile"
        );
        want.put(tx, ty, &solid_tile([9, 9, 9, 255]));
    }
    // Now Vec tiles, the same place twice: second wins.
    assert!(!apply(&mut r, &mut alloc, 2, 2, 100), "pool is exhausted");
    assert!(!apply(&mut r, &mut alloc, 2, 2, 200));
    want.put(2, 2, &solid_tile([200, 200, 200, 255]));
    assert!(alloc.exhausted() >= 2);
    r.flush(&ctx).expect("gpu ok");
    want.assert_matches(&r.debug_read_framebuffer(&ctx));
}

#[test]
fn dropping_a_renderer_retires_the_slots_it_held() {
    // The pool outlives a renderer (a session ends, a new one starts): slots
    // buffered but never flushed must not pin their slab forever.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, pool, mut alloc) = slab_renderer(&ctx, 1);
    for i in 0..10u8 {
        send_slot(&mut r, &mut alloc, 0, i, 0, &solid_tile([i, 0, 0, 255]));
    }
    assert_eq!(pool.consumed(0), 0);
    drop(r);
    assert_eq!(pool.consumed(0), 10);
}

#[test]
fn superseded_slab_tiles_are_retired_without_being_drawn() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, pool, mut alloc) = slab_renderer(&ctx, 1);
    // Green slab tile, red Solid: the pixel check below tells them apart.
    send_slot(&mut r, &mut alloc, 1, 4, 4, &solid_tile([0, 200, 0, 255]));
    // A newer Solid for the tile: the slab tile is dropped at flush.
    r.apply_event(
        &ctx,
        Event::TilePayload {
            frame_seq: 2,
            tile_x: 4,
            tile_y: 4,
            generation: 0,
            data: TileData::Solid([0, 0, 200, 255]),
        },
    )
    .expect("ok");
    r.flush(&ctx).expect("gpu ok");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!(pool.consumed(0), 1, "the dropped slot is still retired");
    let fb = r.debug_read_framebuffer(&ctx);
    assert_eq!(
        px(&fb, 4 * TILE_SIZE + 5, 4 * TILE_SIZE + 5),
        [200, 0, 0, 255]
    );
}

#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "recycled")]
fn a_slot_from_a_recycled_slab_is_caught() {
    // Simulates the bug the generation exists for: a slot named to the
    // renderer after its slab went round again.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, pool, mut alloc) = slab_renderer(&ctx, 1);
    let stale = put_slot(&mut alloc, &solid_tile([1, 2, 3, 255]));
    // Fill and retire the slab so the allocator takes it again.
    for _ in 1..SLOTS_PER_SLAB {
        put_slot(&mut alloc, &solid_tile([1, 2, 3, 255]));
    }
    alloc.end_cpu_writes();
    pool.retire_now(&[SLOTS_PER_SLAB]);
    let fresh = put_slot(&mut alloc, &solid_tile([4, 5, 6, 255]));
    assert_ne!(stale.generation(), fresh.generation());
    let _ = r.apply_slot_tile(0, 0, 0, stale);
}

#[test]
fn retirement_waits_for_the_queue_not_for_the_caller() {
    // `retire_when_submitted_work_done` must not count the slots until wgpu
    // reports the work done -- that is what lets a flush that never publishes
    // (and so never waits) still recycle correctly. Called on an idle queue,
    // nothing has run the callback yet, so the count must still be zero.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (pool, _alloc) = UploadPool::with_slabs(&ctx, 1).expect("pool");
    pool.retire_when_submitted_work_done(&ctx.queue, vec![3]);
    assert_eq!(
        pool.consumed(0),
        0,
        "retired before the queue reported done"
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!(pool.consumed(0), 3, "retired once the queue reported done");
}

#[test]
fn a_flush_that_does_not_publish_retires_every_slot_after_the_gpu_is_done() {
    // `flush` (not `flush_and_publish`) never waits on the device, so
    // retirement must come from the queue's completion callback alone. wgpu
    // registers it after its own post-submit maintenance, so right after
    // `flush` returns nothing has been retired -- exactly zero -- and once the
    // device is polled to idle every slot has: the 10 slots that were drawn
    // (tiles (0..10, 1); (0,1) is the one sent four times) plus the 3 earlier
    // copies of tile (0,1) that lost to the last one.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (mut r, pool, mut alloc) = slab_renderer(&ctx, 1);
    for i in 0..10u8 {
        send_slot(&mut r, &mut alloc, 0, i, 1, &solid_tile([i, 9, 9, 255]));
    }
    // Tile (0,1) again, three more times: 4 copies in all, 3 of them losers.
    for salt in 0..3u8 {
        send_slot(
            &mut r,
            &mut alloc,
            0,
            0,
            1,
            &solid_tile([100 + salt, 9, 9, 255]),
        );
    }
    r.flush(&ctx).expect("gpu ok");
    assert_eq!(pool.consumed(0), 0, "retired before the GPU was known done");
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!(
        pool.consumed(0),
        13,
        "13 slots issued: 10 drawn + 3 deduplicated losers"
    );
    let fb = r.debug_read_framebuffer(&ctx);
    assert_eq!(
        px(&fb, 5, 40),
        [102, 9, 9, 255],
        "the last copy of (0,1) won"
    );
}

/// Sync calls made by writing one tile and closing the window, on a one-slab
/// pool for `driver`, with the slab first made busy (an announced, unconsumed
/// slot) when `busy`.
fn sync_calls_for(driver: &str, busy: bool) -> u64 {
    let ctx = WgpuContext::new().expect("wgpu context");
    let (pool, mut alloc) = UploadPool::with_config(&ctx, 16, 1, Some(driver)).expect("pool");
    let tile = solid_tile([1, 2, 3, 255]);
    if busy {
        put_slot(&mut alloc, &tile);
        alloc.end_cpu_writes();
    }
    let before = pool.stats().sync_calls;
    put_slot(&mut alloc, &tile);
    alloc.end_cpu_writes();
    pool.stats().sync_calls - before
}

#[test]
fn drivers_without_cpu_access_hooks_skip_the_sync_ioctls_and_others_keep_them() {
    // panfrost/panthor exporters have no begin/end_cpu_access hooks (START only
    // waits for fences, END does nothing), so the decode thread makes no call.
    for driver in ["panfrost", "panthor"] {
        assert_eq!(sync_calls_for(driver, false), 0, "{driver}, idle");
        assert_eq!(
            sync_calls_for(driver, true),
            0,
            "{driver}, GPU may be reading"
        );
    }
    // Anywhere else both ends of the bracket stay, busy or not.
    for driver in ["i915", "amdgpu", "unknown-driver"] {
        assert_eq!(sync_calls_for(driver, false), 2, "{driver}, idle");
        assert_eq!(
            sync_calls_for(driver, true),
            2,
            "{driver}, GPU may be reading"
        );
    }
}

#[test]
fn the_pool_grows_for_a_bigger_frame_without_disturbing_slots_in_flight() {
    // Two 256-slot slabs, both spent, tiles announced and not yet drawn: the
    // allocator is out of slots. A bigger FrameDimensions appends slabs; the
    // slots already issued still draw correctly from their (unmoved) slabs,
    // are retired against the right slabs, and the allocator can now take the
    // new ones.
    let ctx = WgpuContext::new().expect("wgpu context");
    let (pool, mut alloc) = UploadPool::with_config(&ctx, 4, 2, Some("panfrost")).expect("pool");
    let mut r = renderer(&ctx);
    r.attach_upload_pool(&ctx, &pool);
    let mut want = Want::new();
    let slots = pool.slots_per_slab();
    assert_eq!(slots, 256);
    for i in 0..2 * slots {
        let (tx, ty) = ((i % 60) as u8, (i / 60) as u8);
        let t = unique_tile(tx, ty, (i / 256) as u8 + 40);
        send_slot(&mut r, &mut alloc, 0, tx, ty, &t);
        want.put(tx, ty, &t);
    }
    assert!(alloc.acquire().is_none(), "both slabs spent and unconsumed");
    let before = pool.slab_count();
    assert_eq!(before, 2);

    let added = pool.ensure_for(&ctx, 3840, 2160).expect("grow");
    assert!(added > 0 && pool.slab_count() == before + added);
    assert_eq!(
        pool.ensure_for(&ctx, 1920, 1080).expect("no-op"),
        0,
        "never shrinks"
    );

    // The allocator uses a new slab, not an old one.
    let fresh = put_slot(&mut alloc, &solid_tile([9, 9, 9, 255]));
    assert!(
        fresh.slab() >= before,
        "slot came from slab {}",
        fresh.slab()
    );
    alloc.end_cpu_writes();
    assert!(r.apply_slot_tile(0, 59, 33, fresh));
    want.put(59, 33, &solid_tile([9, 9, 9, 255]));

    // Everything in flight draws correctly, then is retired against its slab.
    r.flush(&ctx).expect("gpu ok");
    want.assert_matches(&r.debug_read_framebuffer(&ctx));
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert_eq!((pool.consumed(0), pool.consumed(1)), (slots, slots));
    assert_eq!(pool.consumed(fresh.slab()), 1);
}
