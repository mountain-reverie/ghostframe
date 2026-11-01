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
