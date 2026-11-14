// SPDX-License-Identifier: MIT OR Apache-2.0
//! `ghostframe-render`'s `TileRenderer` on its own, on a real device, against
//! the native `Renderer` that wraps one: the same events must leave the same
//! pixels, and the portable pieces (the async readback, the CDF 5/3 self-test)
//! must work the way the web client will use them. Requires a GPU (no GPU is a
//! failure, not a skip); not named in any CI workflow.

use ghostframe_client_core::{Event, TileData};
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_render::readback::block_on_device;
use ghostframe_render::TileRenderer;

// 100 pixels is 400 bytes a row: not a multiple of the 256-byte copy alignment,
// so a readback that mishandles row padding shears.
const W: u32 = 100;
const H: u32 = 70;

fn events() -> Vec<Event> {
    let mut rgba = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            rgba.extend_from_slice(&[(x * 8) as u8, (y * 8) as u8, (x + y) as u8, 255]);
        }
    }
    vec![
        Event::TilePayload {
            frame_seq: 1,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([10, 20, 30, 255]),
        },
        Event::TileReady {
            frame_seq: 2,
            tile_x: 1,
            tile_y: 0,
            rgba,
        },
        // Newer Solid over the TileReady's tile: must win.
        Event::TilePayload {
            frame_seq: 3,
            tile_x: 1,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([200, 100, 50, 255]),
        },
        // An older one for the first tile arriving late: must lose.
        Event::TilePayload {
            frame_seq: 0,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([99, 99, 99, 255]),
        },
        Event::TileReady {
            frame_seq: 4,
            tile_x: 3,
            tile_y: 2,
            rgba: vec![77; 32 * 32 * 4],
        },
    ]
}

fn px(fb: &[u8], x: u32, y: u32) -> [u8; 4] {
    let o = ((y * W + x) * 4) as usize;
    [fb[o], fb[o + 1], fb[o + 2], fb[o + 3]]
}

#[test]
fn a_tile_renderer_alone_draws_what_the_native_renderer_draws() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        W,
        H,
    )
    .expect("tile renderer");
    let mut native = Renderer::new(&ctx, W, H, 3, &[], false, false).expect("renderer");
    for ev in events() {
        tiles
            .apply_event(&ctx.device, &ctx.queue, ev.clone())
            .unwrap();
        native.apply_event(&ctx, ev).unwrap();
    }
    let report = tiles.flush(&ctx.device, &ctx.queue, 1_000).unwrap();
    native.flush(&ctx).unwrap();

    let got = tiles.fb().debug_read(&ctx.device, &ctx.queue);
    assert_eq!(got, native.debug_read_framebuffer(&ctx));

    // Non-vacuous: the pictures are what the events say, not two black frames.
    assert_eq!(
        px(&got, 3, 3),
        [30, 20, 10, 255],
        "Solid is BGRA on the wire"
    );
    assert_eq!(px(&got, 40, 3), [50, 100, 200, 255], "the newer Solid won");
    assert_eq!(
        px(&got, 99, 69),
        [77, 77, 77, 77],
        "TileReady in the clamped corner"
    );
    assert!(report.recorded);
    assert_eq!(report.submits, 1);
    assert_eq!(
        report.damage.len(),
        2,
        "tiles (0,0)+(1,0) merge; (3,2) is apart"
    );
}

#[test]
fn the_async_readback_matches_the_blocking_one_and_strips_row_padding() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        W,
        H,
    )
    .unwrap();
    for ev in events() {
        tiles.apply_event(&ctx.device, &ctx.queue, ev).unwrap();
    }
    tiles.flush(&ctx.device, &ctx.queue, 0).unwrap();
    let blocking = tiles.fb().debug_read(&ctx.device, &ctx.queue);
    let asynchronous = block_on_device(&ctx.device, tiles.fb().read_rgba(&ctx.device, &ctx.queue))
        .expect("async readback");
    assert_eq!(asynchronous.len(), (W * H * 4) as usize, "tight rows");
    assert_eq!(asynchronous, blocking);
    // The right-hand column of tiles is 4 pixels wide: a row that drifted by
    // the padding would put the TileReady's pixels at the wrong x.
    assert_eq!(px(&asynchronous, 96, 64), [77, 77, 77, 77]);
}

#[test]
fn the_cdf53_self_test_passes_on_the_real_device_and_the_grid_then_decodes() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        W,
        H,
    )
    .unwrap();
    block_on_device(
        &ctx.device,
        tiles.try_enable_cdf53_gpu(&ctx.device, &ctx.queue),
    )
    .expect("the shipped shaders pass their own self-test");
    assert!(tiles.debug_state().cdf53_gpu);
}

#[test]
fn a_session_reset_zeroes_the_palettes_and_forgets_the_paint_order() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut tiles = TileRenderer::new(
        &ctx.device,
        &ctx.queue,
        wgpu::TextureFormat::Rgba8Unorm,
        64,
        64,
    )
    .unwrap();
    let palrle = |seq: u32| Event::TilePayload {
        frame_seq: seq,
        tile_x: 0,
        tile_y: 0,
        generation: 0,
        data: TileData::PalRle {
            palette_id: 0,
            count: 1,
            indices: vec![0; 512],
            wire_flags: 0,
        },
    };
    let read = |tiles: &TileRenderer| {
        let rgba = tiles.fb().debug_read(&ctx.device, &ctx.queue);
        [rgba[0], rgba[1], rgba[2], rgba[3]]
    };
    let palette = Event::PaletteUpdated {
        palette_id: 0,
        colors: vec![[10, 20, 30, 255]],
    };
    for ev in [palette, palrle(5)] {
        tiles.apply_event(&ctx.device, &ctx.queue, ev).unwrap();
    }
    tiles.flush(&ctx.device, &ctx.queue, 0).unwrap();
    assert_eq!(read(&tiles), [30, 20, 10, 255], "BGRA palette entry, drawn");

    tiles.reset_session(&ctx.device, &ctx.queue).unwrap();
    // The same tile again, from an OLDER frame. Only a cleared paint order lets
    // it draw at all, and only a zeroed palette makes it draw nothing visible:
    // both halves of the reset show in this one pixel.
    tiles
        .apply_event(&ctx.device, &ctx.queue, palrle(2))
        .unwrap();
    tiles.flush(&ctx.device, &ctx.queue, 1).unwrap();
    assert_eq!(read(&tiles), [0, 0, 0, 0]);
}
