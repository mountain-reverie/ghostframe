//! An access unit in, a plausible image in the framebuffer out.
//!
//! "Plausible", not "correct": this asserts wiring (something drew, luma
//! and chroma both actually landed) rather than exact pixels. Task 8's Tier
//! A/B oracles in `nv12_oracle_tests.rs` already cover the NV12->RGBA
//! arithmetic bit-exactly against a real GPU target; re-deriving that here
//! from a lossy H.264 clip would only add tolerance, not coverage.
//!
//! Also hosts the M3 §10 manual measurement harnesses
//! (`decode_and_publish_timing_at_1920x1080_{unpaced,paced_60hz}`) -- not
//! correctness tests, but they reuse [`assert_looks_like_gradient`] so a run
//! that measured nothing (every `decode()` silently erroring, say) still
//! fails loudly instead of reporting a plausible-looking empty sample set.
//!
//! Requires a GPU and VA-API. Deliberately NOT named in any CI workflow.

use std::time::{Duration, Instant};

use ghostframe_client_core::Event;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::testclip::gradient_clip;

/// Shared skip gate for every test in this file: a usable GPU, and the
/// driver's own claim of an H.264 VLD entrypoint.
///
/// Gates on `vainfo_reports_h264_vld`, not `vaapi_h264_decode_available()`:
/// the latter decodes a frame to answer, so gating on it would mean a
/// decoder regression makes every test here skip and report green -- the
/// coverage would vanish exactly when it should fail.
fn gpu_and_vaapi_or_skip() -> Option<WgpuContext> {
    let ctx = match WgpuContext::new() {
        Ok(ctx) => ctx,
        Err(_) => {
            eprintln!("no usable GPU; skipping");
            return None;
        }
    };
    match ghostframe_client_h264::vainfo_reports_h264_vld(
        ghostframe_client_h264::probe::RENDER_NODE,
    ) {
        Some(true) => Some(ctx),
        Some(false) => {
            eprintln!("driver reports no H.264 VLD entrypoint; skipping");
            None
        }
        None => {
            eprintln!("vainfo unavailable; cannot establish ground truth, skipping");
            None
        }
    }
}

/// Assert `pixels` (tightly packed RGBA8, `w`x`h`) looks like a decoded
/// `gradient_clip` frame, not a broken pipeline's output.
///
/// Three checks, each closing a hole the others miss:
/// - enough non-black pixels that something drew at all;
/// - real channel spread, so a pipeline that blitted one wrong-but-nonzero
///   constant into every texel (a broken bind group, a stuck sampler) still
///   fails, not just "some pixel is non-black";
/// - R/B diverging from G, so chroma dropped entirely (or bound to the
///   wrong slot) still fails. This does not catch a U/V swap -- both still
///   diverge from G either way -- it only closes the
///   chroma-not-reaching-the-framebuffer-at-all hole the first two checks
///   cannot.
fn assert_looks_like_gradient(pixels: &[u8], w: u32, h: u32) {
    assert_eq!(pixels.len(), w as usize * h as usize * 4);

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
    let total = (w * h) as usize;
    assert!(
        nonblack > total / 4,
        "only {nonblack} of {total} pixels are non-black -- the decode path drew nothing"
    );
    let spread = (0..3).map(|c| max[c] - min[c]).max().unwrap();
    assert!(
        spread > 32,
        "framebuffer is nearly uniform (max channel spread {spread}); a gradient clip \
         must produce a non-uniform framebuffer -- min={min:?} max={max:?}"
    );

    let chroma_diverges = pixels
        .chunks_exact(4)
        .any(|p| p[0].abs_diff(p[1]) > 16 || p[2].abs_diff(p[1]) > 16);
    assert!(
        chroma_diverges,
        "R and B never diverge from G -- chroma is not reaching the framebuffer"
    );
}

#[test]
fn a_decoded_frame_lands_in_the_framebuffer() {
    // Otherwise `blit_h264_frame`'s `tracing::info!("H.264 import path: ...")`
    // goes nowhere, even under `--nocapture`: there is no default subscriber.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info")
        .try_init();

    let Some(ctx) = gpu_and_vaapi_or_skip() else {
        return;
    };

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
    // libx264 defaults to B-frames, so the last access unit(s) fed above can
    // still be buffered by reordering rather than decoded yet -- drain them
    // so the assertions below see the whole clip, not however much of it
    // happened to have already left the decoder.
    renderer.finish_h264(&ctx);
    renderer.flush(&ctx);

    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_looks_like_gradient(&pixels, 640, 480);
}

/// Not a correctness test: this drives `Renderer` through enough 1920x1080
/// decode/flush/publish/release cycles, at the production resolution, to
/// give M3 design doc §10 real percentile samples -- `decode_h264`'s
/// `decode_us` and `H264Decoder::new`'s `new_us` debug lines,
/// `blit_h264_frame`'s `map_us` (the `vaSyncSurface` wait -- see that call
/// site's doc comment for why `decode_us` alone understates the real decode
/// cost), and `ring::publish`'s `poll_us`, all from the same run.
///
/// `host_visible: false` on the `Renderer::new` call below, matching
/// production (`Config::debug_map_frames` defaults false, per that
/// parameter's own doc comment): export buffers are device-local, the same
/// memory type `fb.blit_rects`/`poll_us` measures in production. An earlier
/// version of this harness passed `true`, which made the export buffers
/// CPU-mappable and put a different memory type than production behind the
/// `poll_us` numbers.
///
/// Two entry points below share this body, `paced` distinguishing them:
/// `paced = false` decodes as fast as the hardware allows, back-to-back;
/// `paced = true` schedules each frame's arrival at the real 16.667ms (60Hz)
/// cadence production actually delivers at, sleeping off whatever headroom
/// processing that frame left. Both are worth having: unpaced shows the
/// hardware's raw throughput ceiling, paced shows what a real session's
/// steady arrival rate actually costs the render thread, which is the
/// number the frame-budget question in design doc §10.6 needs.
///
/// Run with e.g.
/// `RUST_LOG=ghostframe_client_gpu::renderer=debug,ghostframe_client_gpu::ring=debug
/// cargo test -p ghostframe-client-gpu --test gpu_h264_render \
/// decode_and_publish_timing -- --ignored --nocapture` and parse the emitted
/// `decode_us` / `new_us` / `map_us` / `poll_us` fields.
///
/// `#[ignore]`d: these are manual measurement harnesses, not part of the
/// default `cargo test` run -- each synthesizes and decodes ~120 1080p
/// frames (the paced one also takes ~2s of real wall-clock sleep), and the
/// point is the debug-log side effects a normal test run discards.
fn run_decode_and_publish_timing(paced: bool) {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ghostframe_client_gpu=debug".into()),
        )
        .try_init();

    let Some(ctx) = gpu_and_vaapi_or_skip() else {
        return;
    };

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FRAME_COUNT: usize = 120;
    const ARRIVAL_INTERVAL: Duration = Duration::from_micros(16_667);

    eprintln!(
        "=== decode_and_publish_timing: paced={paced} ({} frames at {WIDTH}x{HEIGHT}) ===",
        FRAME_COUNT
    );

    let mut renderer = Renderer::new(&ctx, WIDTH, HEIGHT, 3, &[], false).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: WIDTH,
            height: HEIGHT,
        },
    );

    let clip = gradient_clip(WIDTH, HEIGHT, FRAME_COUNT);
    // Real wall-clock scheduling on purpose: this reproduces production's
    // actual frame-arrival cadence, not a virtual clock -- there is no
    // paused/virtual clock in this process for it to silently desync from.
    let run_start = Instant::now();
    for (i, au) in clip.iter().enumerate() {
        if paced {
            let deadline = run_start + ARRIVAL_INTERVAL * i as u32;
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
        }
        renderer.apply_event(
            &ctx,
            &Event::NeedsH264 {
                frame_seq: i as u32,
                timestamp_us: i as u32 * 16_667,
                is_keyframe: i == 0,
                payload: au.clone(),
            },
        );
        // Mirrors `render_thread.rs`'s real per-event loop (decode ->
        // flush -> publish -> release) rather than batching every frame's
        // decode ahead of a single flush/publish, so `ring::publish`'s
        // `poll_us` samples are the same shape the production render
        // thread produces: one publish per decoded frame, interleaved with
        // decode work, not decode work run in isolation from it.
        renderer.flush(&ctx);
        if let Some(pf) = renderer.publish(&ctx) {
            renderer.release(pf.frame_id);
        }
    }
    renderer.finish_h264(&ctx);
    renderer.flush(&ctx);

    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_looks_like_gradient(&pixels, WIDTH, HEIGHT);
}

#[test]
#[ignore = "manual measurement harness for design doc §10, see run_decode_and_publish_timing's doc"]
fn decode_and_publish_timing_at_1920x1080_unpaced() {
    run_decode_and_publish_timing(false);
}

#[test]
#[ignore = "manual measurement harness for design doc §10, see run_decode_and_publish_timing's doc"]
fn decode_and_publish_timing_at_1920x1080_paced_60hz() {
    run_decode_and_publish_timing(true);
}
