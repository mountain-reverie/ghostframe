// SPDX-License-Identifier: MIT OR Apache-2.0
//! A flush is one GPU submit however many codecs it carries, and joining the
//! queue-ordered paths into it did not change what is painted. Requires a GPU
//! (`WgpuContext::new().expect`: no GPU is a failure, not a skip); not named
//! in any CI workflow.
//!
//! Each submit is a `glFlush` and a fence on GLES, and on panfrost the end of
//! a job batch. `Renderer::last_flush_submits` counts the submits a flush made
//! for its own GPU work.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, TileData, TileDelivery};
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::testdata;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_protocol::tile::TILE_SIZE;

const W: u32 = 1920;
const H: u32 = 1080;

fn renderer(ctx: &WgpuContext) -> Renderer {
    let mut r = Renderer::new(ctx, W, H, 3, &[], false, false).expect("renderer");
    r.enable_cdf53_gpu(ctx);
    r
}

/// Solid, PalRle and CDF 5/3 payloads from the shared capture (tiles in the
/// top-left corner).
fn capture_events() -> Vec<Event> {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: false,
            supports_h264: false,
            h264_acked_ref: false,
            tile_delivery: TileDelivery::Payload,
            cdf53_wire: false,
            display: None,
        },
        0,
    );
    let mut events = Vec::new();
    for (i, d) in testdata::mixed_codec_capture().iter().enumerate() {
        events.extend(core.handle_datagram(d, i as u64 * 1_000));
    }
    events
}

fn tile_ready(frame_seq: u32, tx: u8, ty: u8, rgba: [u8; 4]) -> Event {
    Event::TileReady {
        frame_seq,
        tile_x: tx,
        tile_y: ty,
        rgba: rgba
            .iter()
            .copied()
            .cycle()
            .take((TILE_SIZE * TILE_SIZE * 4) as usize)
            .collect(),
    }
}

fn px(fb: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [fb[i], fb[i + 1], fb[i + 2], fb[i + 3]]
}

/// Every route a flush has -- CDF 5/3 (two stages), Solid, PalRle, the
/// scattered `TileReady` dispatch and the large-rectangle copy -- in one
/// batch.
fn every_codec_batch() -> Vec<Event> {
    let mut evs = capture_events();
    let kinds = |evs: &[Event]| {
        let (mut solid, mut palrle, mut cdf53) = (false, false, false);
        for ev in evs {
            if let Event::TilePayload { data, .. } = ev {
                match data {
                    TileData::Solid(_) => solid = true,
                    TileData::PalRle { .. } => palrle = true,
                    TileData::Cdf53 { .. } | TileData::Cdf53Wire { .. } => cdf53 = true,
                }
            }
        }
        (solid, palrle, cdf53)
    };
    // Guard against the fixture quietly losing a codec, which would make the
    // submit count below prove less than it says.
    assert_eq!(kinds(&evs), (true, true, true), "capture lost a codec");
    // Two isolated tiles (the dispatch route) and an 8x4 block (the copy).
    evs.push(tile_ready(9, 20, 20, [1, 2, 3, 255]));
    evs.push(tile_ready(9, 30, 25, [4, 5, 6, 255]));
    for ty in 10..14u8 {
        for tx in 10..18u8 {
            evs.push(tile_ready(9, tx, ty, [7, 8, 9, 255]));
        }
    }
    evs
}

#[test]
fn a_flush_with_every_codec_is_one_submit() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    for ev in every_codec_batch() {
        r.apply_event(&ctx, ev).expect("gpu ok");
    }
    r.flush(&ctx).expect("gpu ok");
    assert_eq!(r.last_flush_submits(), 1);

    // The batch really reached the framebuffer through every route; one
    // submit that drew nothing would satisfy the count alone.
    let fb = r.debug_read_framebuffer(&ctx);
    let c = TILE_SIZE / 2;
    assert_eq!(
        px(&fb, 20 * 32 + c, 20 * 32 + c),
        [1, 2, 3, 255],
        "dispatch"
    );
    assert_eq!(px(&fb, 14 * 32 + c, 11 * 32 + c), [7, 8, 9, 255], "copy");
    let top_left = (0..2 * 32u32)
        .flat_map(|y| (0..2 * 32u32).map(move |x| (x, y)))
        .any(|(x, y)| px(&fb, x, y) != [0, 0, 0, 0]);
    assert!(top_left, "solid/palrle/cdf53 tiles drew nothing");
}

#[test]
fn flush_and_publish_is_one_submit_and_publishes() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    for ev in every_codec_batch() {
        r.apply_event(&ctx, ev).expect("gpu ok");
    }
    let frame = r
        .flush_and_publish(&ctx)
        .expect("gpu ok")
        .expect("a free export buffer");
    assert_eq!(r.last_flush_submits(), 1, "the blit rides the same submit");
    assert!(!frame.damage.is_empty());

    // Nothing pending: only the blit of whatever the next buffer lacks. The
    // other two buffers are still unfilled, so there is one, and it is one
    // submit.
    r.flush_and_publish(&ctx).expect("gpu ok").expect("buffer");
    assert_eq!(r.last_flush_submits(), 1);
}

#[test]
fn a_flush_with_nothing_to_do_submits_nothing() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    r.flush(&ctx).expect("gpu ok");
    assert_eq!(r.last_flush_submits(), 0);
}

#[test]
fn a_large_tile_ready_rectangle_is_still_painted_after_solid() {
    // Same `frame_seq`, so the paint-order ledger drops neither, and the fixed
    // order decides: TileReady goes after Solid. A rectangle uploaded through
    // the queue instead of a recorded copy would run before the Solid draw and
    // lose.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    let blue_bgra = [0xFF, 0x00, 0x00, 0xFF];
    r.apply_event(
        &ctx,
        Event::TilePayload {
            frame_seq: 4,
            tile_x: 12,
            tile_y: 11,
            generation: 0,
            data: TileData::Solid(blue_bgra),
        },
    )
    .expect("gpu ok");
    let green = [0x00, 0xFF, 0x00, 0xFF];
    for ty in 10..14u8 {
        for tx in 10..18u8 {
            r.apply_event(&ctx, tile_ready(4, tx, ty, green))
                .expect("gpu ok");
        }
    }
    r.flush(&ctx).expect("gpu ok");
    assert_eq!(r.last_flush_submits(), 1);
    let fb = r.debug_read_framebuffer(&ctx);
    let c = TILE_SIZE / 2;
    assert_eq!(px(&fb, 12 * 32 + c, 11 * 32 + c), green);
}

#[test]
fn a_short_tile_ready_payload_is_still_painted_after_solid() {
    // As above, for the short-payload route: 16 of 32 rows, same `frame_seq` as
    // a Solid over the same tile. The upper rows must be the TileReady's, the
    // rest the Solid's. A queue write would run before the Solid draw and lose.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = renderer(&ctx);
    r.apply_event(
        &ctx,
        Event::TilePayload {
            frame_seq: 4,
            tile_x: 12,
            tile_y: 11,
            generation: 0,
            data: TileData::Solid([0xFF, 0x00, 0x00, 0xFF]),
        },
    )
    .expect("gpu ok");
    let green = [0x00, 0xFF, 0x00, 0xFF];
    let Event::TileReady { rgba, .. } = tile_ready(4, 12, 11, green) else {
        unreachable!()
    };
    r.apply_event(
        &ctx,
        Event::TileReady {
            frame_seq: 4,
            tile_x: 12,
            tile_y: 11,
            rgba: rgba[..(16 * TILE_SIZE * 4) as usize].to_vec(),
        },
    )
    .expect("gpu ok");
    r.flush(&ctx).expect("gpu ok");
    assert_eq!(r.last_flush_submits(), 1);
    let fb = r.debug_read_framebuffer(&ctx);
    let (x, y0) = (12 * 32 + 16, 11 * 32);
    assert_eq!(px(&fb, x, y0 + 5), green, "a row the payload covers");
    assert_eq!(
        px(&fb, x, y0 + 20),
        [0x00, 0x00, 0xFF, 0xFF],
        "a row it does not cover keeps the Solid"
    );
}

/// An export buffer read back as BGRA bytes: (bytes, stride, offset). The CPU
/// mmap when the dmabuf is LINEAR, a wgpu texture copy otherwise (which proves
/// the blit landed, not the dmabuf's byte layout). Same shape as `gpu_export.rs`.
fn read_export(
    ctx: &WgpuContext,
    img: &ghostframe_client_gpu::export::ExportedImage,
) -> (Vec<u8>, usize, usize) {
    if img.modifier == 0 {
        let bytes = img.map_read().expect("map dmabuf");
        return (
            bytes,
            img.planes[0].stride as usize,
            img.planes[0].offset as usize,
        );
    }
    let tex = img
        .as_wgpu_texture(&ctx.device)
        .expect("wrap export as texture");
    let stride = (img.width as usize * 4).div_ceil(256) * 256;
    let staging = ctx.device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("export-readback"),
        size: (stride * img.height as usize) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = ctx
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    encoder.copy_texture_to_buffer(
        tex.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(stride as u32),
                rows_per_image: Some(img.height),
            },
        },
        wgpu::Extent3d {
            width: img.width,
            height: img.height,
            depth_or_array_layers: 1,
        },
    );
    ctx.queue.submit(std::iter::once(encoder.finish()));
    let slice = staging.slice(..);
    slice.map_async(wgpu::MapMode::Read, |r| r.expect("map readback"));
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll for map");
    let bytes = slice.get_mapped_range().expect("range").to_vec();
    (bytes, stride, 0)
}

/// Pixel `(x, y)` of a [`read_export`] result, as BGRA.
fn export_px(read: &(Vec<u8>, usize, usize), x: u32, y: u32) -> [u8; 4] {
    let (bytes, stride, offset) = read;
    let i = offset + y as usize * stride + x as usize * 4;
    [bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]
}

fn bgra_of(rgba: [u8; 4]) -> [u8; 4] {
    [rgba[2], rgba[1], rgba[0], rgba[3]]
}

#[test]
fn the_export_blit_sees_the_pixels_the_same_flush_drew() {
    // The blit is recorded last in the flush's encoder. Recorded earlier, it
    // would copy the previous frame's pixels, and the ring would still mark
    // the buffer filled for this generation -- the tiles would be missing from
    // the export until something repainted them.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = Renderer::new(&ctx, 256, 128, 3, &[], true, false).expect("renderer");
    let solid = [0x11, 0x22, 0x33, 0xFF]; // BGRA on the wire
    r.apply_event(
        &ctx,
        Event::TilePayload {
            frame_seq: 1,
            tile_x: 1,
            tile_y: 1,
            generation: 0,
            data: TileData::Solid(solid),
        },
    )
    .expect("gpu ok");
    let ready = [0x44, 0x55, 0x66, 0xFF]; // RGBA
    r.apply_event(&ctx, tile_ready(1, 5, 2, ready))
        .expect("gpu ok");
    let frame = r
        .flush_and_publish(&ctx)
        .expect("gpu ok")
        .expect("a free export buffer");
    let read = read_export(&ctx, r.export_buffer(frame.buffer_id));
    let c = TILE_SIZE / 2;
    assert_eq!(export_px(&read, 32 + c, 32 + c), solid, "solid tile");
    assert_eq!(
        export_px(&read, 5 * 32 + c, 2 * 32 + c),
        bgra_of(ready),
        "TileReady tile"
    );
}

#[test]
fn every_cached_bind_group_follows_a_framebuffer_resize() {
    // Draw at one size so each codec caches a bind group (and the blit its
    // source), resize, then draw the same codecs again. A cache that ignored
    // the new framebuffer would keep drawing into the old texture: the new
    // one, and the export made from it, would stay blank.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut r = Renderer::new(&ctx, 256, 128, 3, &[], true, false).expect("renderer");
    let batch = |seq_rgba: [u8; 4]| -> Vec<Event> {
        let mut evs = capture_events();
        // Two isolated tiles take the scattered-dispatch route.
        evs.push(tile_ready(9, 6, 3, seq_rgba));
        evs.push(tile_ready(9, 4, 1, seq_rgba));
        evs
    };
    for ev in batch([1, 1, 1, 255]) {
        r.apply_event(&ctx, ev).expect("gpu ok");
    }
    let before = r.flush_and_publish(&ctx).expect("gpu ok").expect("buffer");
    r.release(before.frame_id);

    r.apply_event(
        &ctx,
        Event::FrameDimensions {
            width: 320,
            height: 192,
        },
    )
    .expect("gpu ok");
    let marker = [9, 80, 200, 255];
    for ev in batch(marker) {
        r.apply_event(&ctx, ev).expect("gpu ok");
    }
    let frame = r.flush_and_publish(&ctx).expect("gpu ok").expect("buffer");

    // The oracle: a renderer born at the new size and fed only the second
    // batch. Compared on the tiles that batch touched.
    let mut fresh = Renderer::new(&ctx, 320, 192, 3, &[], false, false).expect("renderer");
    for ev in batch(marker) {
        fresh.apply_event(&ctx, ev).expect("gpu ok");
    }
    fresh.flush(&ctx).expect("gpu ok");

    let got = r.debug_read_framebuffer(&ctx);
    let want = fresh.debug_read_framebuffer(&ctx);
    let export = read_export(&ctx, r.export_buffer(frame.buffer_id));
    let mut tiles: Vec<(u32, u32)> = vec![(6, 3), (4, 1)];
    tiles.extend((0..2).flat_map(|y| (0..2).map(move |x| (x, y))));
    let mut nonblank = 0;
    for (tx, ty) in tiles {
        for (dx, dy) in [(1, 1), (16, 16), (30, 30)] {
            let (x, y) = (tx * 32 + dx, ty * 32 + dy);
            let w = 320;
            let i = ((y * w + x) * 4) as usize;
            let want_px = [want[i], want[i + 1], want[i + 2], want[i + 3]];
            assert_eq!(
                [got[i], got[i + 1], got[i + 2], got[i + 3]],
                want_px,
                "framebuffer at tile ({tx},{ty}) +({dx},{dy})"
            );
            assert_eq!(
                export_px(&export, x, y),
                bgra_of(want_px),
                "export at tile ({tx},{ty}) +({dx},{dy})"
            );
            nonblank += usize::from(want_px != [0, 0, 0, 0]);
        }
    }
    assert!(nonblank >= 6, "the reference drew almost nothing");
}

#[test]
fn pipelines_draw_into_a_framebuffer_texture_that_replaced_the_one_they_cached() {
    // The caches key on the framebuffer's epoch. Resizing replaces the texture
    // (keeping the old pixels), so a pipeline that kept its bind group would
    // draw into the discarded one. Driven without a `Renderer`, so nothing
    // else invalidates the caches.
    use ghostframe_client_gpu::framebuffer::Framebuffer;
    use ghostframe_client_gpu::pipelines::{palrle::PalRlePipeline, rgba::RgbaPipeline};

    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 128, 64);
    let mut rgba = RgbaPipeline::new(&ctx.device);
    let mut palrle = PalRlePipeline::new(&ctx.device);
    palrle.upload_palette(&ctx.queue, 1, &[[0x10, 0x20, 0x30, 0xFF]; 16]);
    let pixels = vec![0x77u8; 32 * 32 * 4];
    let indices = vec![0u8; 512];

    rgba.draw(&ctx.device, &ctx.queue, &fb, &[(0, 0, &pixels[..])]);
    palrle.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(0, 1, 1, 16, indices.clone())],
    );

    fb.resize(&ctx.device, &ctx.queue, 160, 96);
    rgba.draw(&ctx.device, &ctx.queue, &fb, &[(1, 0, &pixels[..])]);
    palrle.decode(&ctx.device, &ctx.queue, &fb, &[(1, 1, 1, 16, indices)]);

    let got = fb.debug_read(&ctx.device, &ctx.queue);
    let at = |x: u32, y: u32| {
        let i = ((y * 160 + x) * 4) as usize;
        [got[i], got[i + 1], got[i + 2], got[i + 3]]
    };
    assert_eq!(at(32 + 16, 16), [0x77; 4], "rgba tile after the resize");
    let old = at(16, 32 + 16);
    assert_ne!(old, [0, 0, 0, 0], "palrle tile before the resize survived");
    assert_eq!(at(32 + 16, 32 + 16), old, "palrle tile after the resize");
}
