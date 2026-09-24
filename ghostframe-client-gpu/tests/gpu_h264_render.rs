//! An access unit in, correct pixels in the framebuffer out.
//!
//! Requires a GPU and VA-API. Deliberately NOT named in any CI workflow.

use ghostframe_client_core::Event;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::testclip::gradient_clip;

#[test]
fn a_decoded_frame_lands_in_the_framebuffer() {
    // Otherwise `blit_h264_frame`'s `tracing::info!("H.264 import path: ...")`
    // goes nowhere, even under `--nocapture`: there is no default subscriber.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info")
        .try_init();

    let Ok(ctx) = WgpuContext::new() else {
        eprintln!("no usable GPU; skipping");
        return;
    };
    // Gate on the DRIVER's own claim, not on `vaapi_h264_decode_available()`.
    // That probe decodes a frame, so gating on it would mean a decoder
    // regression makes this test skip and report green -- the test would
    // vanish exactly when it should fail.
    match ghostframe_client_h264::vainfo_reports_h264_vld(
        ghostframe_client_h264::probe::RENDER_NODE,
    ) {
        Some(true) => {}
        Some(false) => {
            eprintln!("driver reports no H.264 VLD entrypoint; skipping");
            return;
        }
        None => {
            eprintln!("vainfo unavailable; cannot establish ground truth, skipping");
            return;
        }
    }

    let mut renderer = Renderer::new(&ctx, 640, 480, 3, &[], true).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: 640,
            height: 480,
        },
    );

    let clip = gradient_clip(640, 480, 4);
    for (i, au) in clip.iter().enumerate() {
        renderer.apply_event(
            &ctx,
            &Event::NeedsH264 {
                frame_seq: i as u32,
                timestamp_us: i as u32 * 16_667,
                is_keyframe: i == 0,
                payload: au.clone(),
            },
        );
    }
    renderer.flush(&ctx);

    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_eq!(pixels.len(), 640 * 480 * 4);

    // The clip is a gradient varying along both axes (see `gradient_clip`'s
    // doc), so a correctly decoded and blitted frame must show real
    // variation across pixels, not just "some pixel is non-black" -- a
    // pipeline that blitted one wrong-but-nonzero constant into every texel
    // (a broken bind group, a stuck sampler) would pass a mere
    // non-uniformity-blind check.
    let mut min = [255u8; 3];
    let mut max = [0u8; 3];
    let mut nonblack = 0usize;
    for p in pixels.chunks_exact(4) {
        for c in 0..3 {
            min[c] = min[c].min(p[c]);
            max[c] = max[c].max(p[c]);
        }
        if p[0] > 8 || p[1] > 8 || p[2] > 8 {
            nonblack += 1;
        }
    }
    assert!(
        nonblack > 640 * 480 / 4,
        "only {nonblack} of {} pixels are non-black -- the decode path drew nothing",
        640 * 480
    );
    let spread = (0..3).map(|c| max[c] - min[c]).max().unwrap();
    assert!(
        spread > 32,
        "framebuffer is nearly uniform (max channel spread {spread}); a gradient clip \
         must produce a non-uniform framebuffer -- min={min:?} max={max:?}"
    );
}
