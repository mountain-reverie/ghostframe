//! Real-WGSL pipeline tests. Requires a GPU; NOT named in any CI workflow.

use ghostframe_client_gpu::{
    framebuffer::Framebuffer, pipelines::solid::SolidPipeline, wgpu_ctx::WgpuContext,
};

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
