//! Real-WGSL pipeline tests. Requires a GPU; NOT named in any CI workflow.

use ghostframe_client_core::cdf53_prevalidate::prevalidate_cdf53;
use ghostframe_client_gpu::{
    framebuffer::Framebuffer,
    pipelines::{cdf53::Cdf53Pipeline, palrle::PalRlePipeline, solid::SolidPipeline},
    wgpu_ctx::WgpuContext,
};
use ghostframe_protocol::codec::cdf53 as cdf53_codec;

fn px(fb_bytes: &[u8], fb_w: u32, x: u32, y: u32) -> [u8; 4] {
    let o = ((y * fb_w + x) * 4) as usize;
    [
        fb_bytes[o],
        fb_bytes[o + 1],
        fb_bytes[o + 2],
        fb_bytes[o + 3],
    ]
}

#[test]
fn solid_tile_fills_exactly_its_32x32_region_and_swizzles_bgra_to_rgba() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    let mut pipe = SolidPipeline::new(&ctx.device);
    pipe.set_canvas_size(&ctx.device, &ctx.queue, 64, 64);

    // Wire colour is BGRA: B=0x10 G=0x20 R=0x30 -> RGBA 0x30,0x20,0x10.
    pipe.draw(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(1u8, 1u8, [0x10, 0x20, 0x30, 0xFF])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    // Inside tile (1,1): pixels 32..63 in both axes.
    assert_eq!(
        px(&b, 64, 32, 32),
        [0x30, 0x20, 0x10, 0xFF],
        "channel order wrong"
    );
    assert_eq!(
        px(&b, 64, 63, 63),
        [0x30, 0x20, 0x10, 0xFF],
        "tile does not reach its far corner"
    );
    // Just outside must be untouched -- catches an off-by-one in the quad.
    assert_eq!(px(&b, 64, 31, 32), [0, 0, 0, 0xFF], "bled left of the tile");
    assert_eq!(px(&b, 64, 32, 31), [0, 0, 0, 0xFF], "bled above the tile");
}

#[test]
fn solid_draw_preserves_previously_written_tiles() {
    // The render pass must LOAD, not CLEAR. A Clear here would wipe every
    // tile another codec wrote earlier in the same frame, which is the most
    // damaging mistake available in this pipeline.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);
    fb.debug_fill_tile(&ctx.device, &ctx.queue, 0, 0, [0x99, 0x88, 0x77, 0xFF]);

    let mut pipe = SolidPipeline::new(&ctx.device);
    pipe.set_canvas_size(&ctx.device, &ctx.queue, 64, 64);
    pipe.draw(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(1u8, 1u8, [0x10, 0x20, 0x30, 0xFF])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    assert_eq!(
        px(&b, 64, 5, 5),
        [0x99, 0x88, 0x77, 0xFF],
        "solid draw cleared the framebuffer instead of loading it"
    );
    assert_eq!(
        px(&b, 64, 40, 40),
        [0x30, 0x20, 0x10, 0xFF],
        "new tile missing"
    );
}

#[test]
fn multiple_solid_tiles_draw_in_one_pass() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);
    let mut pipe = SolidPipeline::new(&ctx.device);
    pipe.set_canvas_size(&ctx.device, &ctx.queue, 64, 64);
    pipe.draw(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[
            (0u8, 0u8, [0x01, 0x02, 0x03, 0xFF]),
            (1u8, 1u8, [0x04, 0x05, 0x06, 0xFF]),
        ],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let b = fb.debug_read(&ctx.device, &ctx.queue);
    assert_eq!(px(&b, 64, 10, 10), [0x03, 0x02, 0x01, 0xFF]);
    assert_eq!(px(&b, 64, 40, 40), [0x06, 0x05, 0x04, 0xFF]);
}

#[test]
fn empty_solid_draw_is_a_no_op() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0x11, 0x22, 0x33, 0xFF]);
    let mut pipe = SolidPipeline::new(&ctx.device);
    pipe.set_canvas_size(&ctx.device, &ctx.queue, 64, 64);
    pipe.draw(&ctx.device, &ctx.queue, &fb, &[]);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let b = fb.debug_read(&ctx.device, &ctx.queue);
    assert_eq!(px(&b, 64, 5, 5), [0x11, 0x22, 0x33, 0xFF]);
}

#[test]
fn raw_tile_shorter_than_a_full_tile_uploads_only_its_rows() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    // Two rows' worth of BGRA: 2 * 32 * 4 = 256 bytes.
    let bgra = [0x10u8, 0x20, 0x30, 0xFF].repeat(64);
    ghostframe_client_gpu::pipelines::raw::upload_raw_tile(&ctx.queue, &fb, 0, 0, &bgra);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    assert_eq!(px(&b, 64, 0, 0), [0x30, 0x20, 0x10, 0xFF]);
    assert_eq!(px(&b, 64, 31, 1), [0x30, 0x20, 0x10, 0xFF]);
    // Row 2 was not covered by the payload and must be untouched.
    assert_eq!(px(&b, 64, 0, 2), [0, 0, 0, 0xFF], "wrote past the payload");
}

#[test]
fn palrle_decodes_low_nibble_first_against_the_palette() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    let mut pipe = PalRlePipeline::new(&ctx.device);

    // Palette 3: slot 0 blue-ish, slot 1 red-ish. Colours are BGRA.
    let mut palette = [[0u8; 4]; 16];
    palette[0] = [0xC0, 0x10, 0x20, 0xFF]; // B=C0 G=10 R=20
    palette[1] = [0x20, 0x10, 0xC0, 0xFF]; // B=20 G=10 R=C0
    pipe.upload_palette(&ctx.queue, 3, &palette);

    // 512 bytes, two 4-bit indices per byte, LOW NIBBLE FIRST.
    // 0x10 => pixel 0 -> slot 0, pixel 1 -> slot 1.
    let indices = vec![0x10u8; 512];
    pipe.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(0u8, 0u8, 3u8, 2u8, indices)],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    // Pixel 0 -> slot 0 -> RGBA 0x20,0x10,0xC0
    assert_eq!(
        px(&b, 64, 0, 0),
        [0x20, 0x10, 0xC0, 0xFF],
        "pixel 0 wrong (nibble order or swizzle)"
    );
    // Pixel 1 -> slot 1 -> RGBA 0xC0,0x10,0x20
    assert_eq!(
        px(&b, 64, 1, 0),
        [0xC0, 0x10, 0x20, 0xFF],
        "pixel 1 wrong (nibble order)"
    );
}

#[test]
fn palrle_covers_the_whole_tile_including_the_far_corner() {
    // The 2x2 workgroup arrangement means a wg.y/wg.z mix-up still paints
    // the top-left quadrant correctly. Check the far corner, which only
    // the (1,1) sub-workgroup reaches.
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);
    let mut pipe = PalRlePipeline::new(&ctx.device);
    let mut palette = [[0u8; 4]; 16];
    palette[0] = [0x11, 0x22, 0x33, 0xFF];
    pipe.upload_palette(&ctx.queue, 0, &palette);
    pipe.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(0u8, 0u8, 0u8, 1u8, vec![0x00u8; 512])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    for (x, y) in [(0u32, 0u32), (31, 0), (0, 31), (31, 31), (16, 16)] {
        assert_eq!(
            px(&b, 64, x, y),
            [0x33, 0x22, 0x11, 0xFF],
            "tile pixel ({x},{y}) not written"
        );
    }
    // Outside the tile stays untouched.
    assert_eq!(px(&b, 64, 32, 0), [0, 0, 0, 0xFF]);
}

#[test]
fn palrle_writes_the_correct_tile_offset() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);
    let mut pipe = PalRlePipeline::new(&ctx.device);
    let mut palette = [[0u8; 4]; 16];
    palette[0] = [0x44, 0x55, 0x66, 0xFF];
    pipe.upload_palette(&ctx.queue, 0, &palette);
    pipe.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(1u8, 1u8, 0u8, 1u8, vec![0x00u8; 512])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    assert_eq!(
        px(&b, 64, 32, 32),
        [0x66, 0x55, 0x44, 0xFF],
        "tile (1,1) not at pixel (32,32)"
    );
    assert_eq!(
        px(&b, 64, 0, 0),
        [0, 0, 0, 0xFF],
        "wrote to tile (0,0) instead"
    );
}

#[test]
fn palrle_reports_an_out_of_range_index() {
    // The shader stores code 5 (ERR_INDEX_OOB, matching
    // DecodeErrorCode::IndexOob) when an index is >= count. If this is not
    // surfaced, corrupt palette data decodes to arbitrary colours silently.
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut pipe = PalRlePipeline::new(&ctx.device);
    let mut palette = [[0u8; 4]; 16];
    palette[0] = [0x11, 0x22, 0x33, 0xFF];
    pipe.upload_palette(&ctx.queue, 0, &palette);

    // count = 1, but every index is 7 -> out of range.
    pipe.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(0u8, 0u8, 0u8, 1u8, vec![0x77u8; 512])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let errors = pipe.take_errors(&ctx.device, &ctx.queue);
    assert_eq!(
        errors,
        vec![(0u32, 5u32)],
        "expected ERR_INDEX_OOB for tile 0, got {errors:?}"
    );
}

#[test]
fn palrle_errors_do_not_leak_between_decodes() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let fb = Framebuffer::new(&ctx.device, 64, 64);
    let mut pipe = PalRlePipeline::new(&ctx.device);
    let mut palette = [[0u8; 4]; 16];
    palette[0] = [0x11, 0x22, 0x33, 0xFF];
    pipe.upload_palette(&ctx.queue, 0, &palette);

    pipe.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(0u8, 0u8, 0u8, 1u8, vec![0x77u8; 512])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert!(!pipe.take_errors(&ctx.device, &ctx.queue).is_empty());

    // A clean decode must not still report the previous failure.
    pipe.decode(
        &ctx.device,
        &ctx.queue,
        &fb,
        &[(0u8, 0u8, 0u8, 1u8, vec![0x00u8; 512])],
    );
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    assert!(
        pipe.take_errors(&ctx.device, &ctx.queue).is_empty(),
        "stale error leaked"
    );
}

#[test]
fn cdf53_flat_tile_reconstructs_its_flat_colour() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let mut fb = Framebuffer::new(&ctx.device, 64, 64);
    fb.debug_fill(&ctx.device, &ctx.queue, [0, 0, 0, 0xFF]);

    let mut pipe = Cdf53Pipeline::new(&ctx.device);
    pipe.resize(&ctx.device, &ctx.queue, 2, 2);

    // Build a wire-accurate pass-0..N payload for a uniform mid-grey tile
    // using the real CPU encoder (ghostframe_protocol::codec::cdf53), so
    // the fixture cannot drift from the wire format. BGRA, mid-grey.
    let bgra = [0x80u8, 0x80, 0x80, 0xFF].repeat(32 * 32);
    let coeffs = cdf53_codec::forward(&bgra);
    let (present, sparse) = cdf53_codec::encode_passes_sparse(&coeffs);

    let tiles: Vec<ghostframe_client_gpu::pipelines::cdf53::Cdf53PassEntry> = sparse
        .iter()
        .map(|(pass_idx, payload)| {
            let pre = prevalidate_cdf53(payload, 1, *pass_idx).expect("wire payload prevalidates");
            let present_passes = if *pass_idx == 0 { Some(present) } else { None };
            (0u8, 0u8, *pass_idx, pre.bit_planes, present_passes)
        })
        .collect();

    pipe.integrate(&ctx.device, &ctx.queue, &tiles);
    pipe.inverse(&ctx.device, &ctx.queue, &fb);
    ctx.device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");

    let b = fb.debug_read(&ctx.device, &ctx.queue);
    // EXACT, not approximate. CDF 5/3 is an integer-reversible lifting
    // transform and the inverse shaders use only i32; with every pass
    // delivered, reconstruction is lossless by construction. A tolerance
    // here would let a systematic reconstruction error hide -- and a
    // systematic error is exactly the failure mode this pipeline has
    // historically had.
    assert_eq!(
        px(&b, 64, 4, 4),
        [0x80, 0x80, 0x80, 0xFF],
        "flat tile did not reconstruct losslessly"
    );
    // A corner too: a partial inverse can be right in the tile interior
    // and wrong at the boundary, where the lifting filter needs its
    // symmetric extension.
    assert_eq!(
        px(&b, 64, 31, 31),
        [0x80, 0x80, 0x80, 0xFF],
        "flat tile wrong at the far corner"
    );
}
