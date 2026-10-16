//! Within one `flush` batch, a tile is painted by its newest `frame_seq`, not
//! by the order the codecs happen to be drawn in. Needs a real GPU, so not
//! named in any CI workflow (same rule as the other `gpu_*.rs` files).
//!
//! The renderer used to draw Raw immediately and then Solid, PalRle,
//! `TileReady` and CDF 5/3 in that fixed order at `flush`. When two server
//! frames landed in one batch, an older Solid could therefore sit on top of a
//! newer Raw, and an older `TileReady` on top of a newer Solid.

use ghostframe_client_core::{Event, TileData};
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_protocol::tile::TILE_SIZE;

const W: u32 = 1920;
const H: u32 = 1080;

fn renderer(ctx: &WgpuContext) -> Renderer {
    Renderer::new(ctx, W, H, 3, &[], false, false).expect("renderer")
}

fn payload(frame_seq: u32, data: TileData) -> Event {
    Event::TilePayload {
        frame_seq,
        tile_x: 2,
        tile_y: 3,
        generation: 0,
        data,
    }
}

fn solid(frame_seq: u32, bgra: [u8; 4]) -> Event {
    payload(frame_seq, TileData::Solid(bgra))
}

fn tile_ready(frame_seq: u32, rgba: [u8; 4]) -> Event {
    Event::TileReady {
        frame_seq,
        tile_x: 2,
        tile_y: 3,
        rgba: rgba
            .iter()
            .copied()
            .cycle()
            .take((TILE_SIZE * TILE_SIZE * 4) as usize)
            .collect(),
    }
}

/// Centre pixel of tile (2,3) after applying `events` and one flush.
fn centre(events: Vec<Event>) -> [u8; 4] {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    for ev in events {
        r.apply_event(&ctx, ev).expect("gpu ok");
    }
    r.flush(&ctx).expect("gpu ok");
    let fb = r.debug_read_framebuffer(&ctx);
    let (x, y) = (2 * TILE_SIZE + TILE_SIZE / 2, 3 * TILE_SIZE + TILE_SIZE / 2);
    let i = ((y * W + x) * 4) as usize;
    [fb[i], fb[i + 1], fb[i + 2], fb[i + 3]]
}

const RED: [u8; 4] = [0x00, 0x00, 0xFF, 0xFF]; // BGRA
const BLUE: [u8; 4] = [0xFF, 0x00, 0x00, 0xFF]; // BGRA

#[test]
fn a_newer_solid_tile_is_not_overwritten_by_an_older_tile_ready() {
    let newer = centre(vec![solid(6, RED)]);
    let older = centre(vec![tile_ready(5, [0, 0, 0xFF, 0xFF])]);
    assert_ne!(newer, older, "fixture must discriminate");

    // TileReady uploads run after the Solid draw in the fixed order.
    let got = centre(vec![solid(6, RED), tile_ready(5, [0, 0, 0xFF, 0xFF])]);
    assert_eq!(got, newer);
}

#[test]
fn arrival_order_does_not_matter_and_equal_seq_is_not_dropped() {
    let newer = centre(vec![solid(6, RED)]);
    assert_eq!(
        centre(vec![solid(6, RED), solid(5, BLUE)]),
        centre(vec![solid(6, RED)])
    );
    assert_eq!(
        centre(vec![tile_ready(5, [0, 0, 0xFF, 0xFF]), solid(6, RED)]),
        newer
    );
    // A tile whose only entry has the batch's newest seq is painted.
    assert_ne!(centre(vec![solid(6, RED)]), [0, 0, 0, 0]);
}
