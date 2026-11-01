use super::*;
use crate::encoder::EncodedTile;
use crate::transport::protocol::Codec as GfCodec;

/// Serialises everything in this test binary that drives the GPU.
///
/// Each test here builds an encoder, which allocates VA-API surfaces. Running
/// them concurrently with each other (surface allocation fails intermittently
/// with `Failed to create surface from DRM object: 2`) or with the Vulkan
/// capture tests (a VCE VM fault during such a mix crashed the host on
/// 2026-10-14) is not safe. This is the one lock `GpuFrameProcessor::new`
/// also takes in test builds; it is reentrant per thread.
fn lock_vaapi() -> crate::gpu_test_lock::GpuTestGuard {
    crate::gpu_test_lock::lock()
}

#[test]
fn encode_solid_red_tile() {
    let _vaapi = lock_vaapi();
    let _ = tracing_subscriber::fmt::try_init();

    let mut encoder = match H264VaapiEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping H264 test (no encoder available): {e}");
            return;
        }
    };

    eprintln!(
        "encoder backend: {} ({}x{})",
        if encoder.use_vaapi {
            "h264_vaapi"
        } else {
            "libx264"
        },
        encoder.enc_w,
        encoder.enc_h,
    );

    let mut tile = vec![0u8; 32 * 32 * 4];
    for pixel in tile.chunks_exact_mut(4) {
        pixel[0] = 0; // B
        pixel[1] = 0; // G
        pixel[2] = 255; // R
        pixel[3] = 255; // A
    }

    let result = encoder.encode(&tile).unwrap();
    // VA-API may buffer the first frame (no zerolatency equivalent),
    // so accept None on VA-API and try a second frame.
    if encoder.use_vaapi && result.is_none() {
        eprintln!("VA-API buffered first frame, sending second...");
        let result2 = encoder.encode(&tile).unwrap();
        assert!(
            result2.is_some(),
            "VA-API should produce output after two frames"
        );
        let encoded = result2.unwrap();
        assert_eq!(encoded.codec, GfCodec::H264);
        assert!(!encoded.payload.is_empty());
    } else {
        assert!(
            result.is_some(),
            "first frame should produce output with zerolatency"
        );
        let encoded = result.unwrap();
        assert_eq!(encoded.codec, GfCodec::H264);
        assert!(!encoded.payload.is_empty());
        // Padded frames will be larger, relax the size check for VA-API.
        if !encoder.use_vaapi {
            assert!(encoded.payload.len() < 4096);
        }
    }
}

#[test]
fn encode_multiple_frames_produces_smaller_p_frames() {
    let _vaapi = lock_vaapi();
    let _ = tracing_subscriber::fmt::try_init();

    let mut encoder = match H264VaapiEncoder::new() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping H264 test (no encoder available): {e}");
            return;
        }
    };

    eprintln!(
        "encoder backend: {} ({}x{})",
        if encoder.use_vaapi {
            "h264_vaapi"
        } else {
            "libx264"
        },
        encoder.enc_w,
        encoder.enc_h,
    );

    let mut tile = vec![0u8; 32 * 32 * 4];
    for pixel in tile.chunks_exact_mut(4) {
        pixel[2] = 255;
        pixel[3] = 255;
    }

    // For VA-API, we may need to drain a few frames before we get output.
    let mut outputs: Vec<EncodedTile> = Vec::new();
    for _ in 0..4 {
        if let Some(encoded) = encoder.encode(&tile).unwrap() {
            outputs.push(encoded);
        }
        if outputs.len() >= 2 {
            break;
        }
    }

    assert!(
        outputs.len() >= 2,
        "expected at least 2 output packets, got {}",
        outputs.len()
    );

    let first = &outputs[0];
    let second = &outputs[1];
    assert!(
        second.payload.len() <= first.payload.len(),
        "P-frame of static content should be <= I-frame: {} vs {}",
        second.payload.len(),
        first.payload.len()
    );
}

// -----------------------------------------------------------------------
// FullFrameEncoder tests
// -----------------------------------------------------------------------

#[test]
fn dmabuf_real_full_frame_encode_from_dmabuf() {
    let _vaapi = lock_vaapi();
    let _ = tracing_subscriber::fmt::try_init();

    let width: u32 = 640;
    let height: u32 = 480;

    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping full_frame_encode test (no encoder): {e}");
            return;
        }
    };

    eprintln!(
        "FullFrameEncoder backend: {} ({}x{})",
        if encoder.use_vaapi {
            "h264_vaapi"
        } else {
            "libx264"
        },
        encoder.enc_w,
        encoder.enc_h,
    );

    assert_eq!(encoder.width(), width);
    assert_eq!(encoder.height(), height);

    // Solid red BGRA pixels in a REAL dma-buf (udmabuf), so
    // `try_drm_prime_import` hands the VA driver what it is built for. A plain
    // memfd is not a dma-buf and must never reach the hardware.
    let dmabuf = crate::capture::gpu_pipeline::dmabuf_fixture::udmabuf_solid(
        width,
        height,
        [0, 0, 255, 255],
    );
    let fd = dmabuf.raw();

    let stride = width * 4;

    // Encode twice — first should be a keyframe (pts == 0).
    // VA-API may buffer the first; try up to three frames.
    let mut got_output = false;
    let mut got_keyframe = false;
    for _ in 0..3 {
        match encoder.encode_frame(fd, width, height, stride) {
            Ok(Some(encoded)) => {
                assert!(
                    !encoded.payload.is_empty(),
                    "encoded payload must not be empty"
                );
                if encoded.is_keyframe {
                    got_keyframe = true;
                }
                got_output = true;
                break;
            }
            Ok(None) => continue,
            Err(e) => panic!("encode_frame failed: {e}"),
        }
    }

    drop(dmabuf);

    assert!(
        got_output,
        "encoder should have produced output within 3 frames"
    );
    assert!(got_keyframe, "first output should be a keyframe");
}

/// The encoder has no GOP of its own: the bridge decides when an IDR is due
/// (a time since the last IDR) and asks with `request_keyframe`. Fed far more
/// frames than the old 11-frame GOP, or a 3 s one at 30 fps, it makes exactly
/// one IDR (the first frame) and one more per request.
#[test]
fn full_frame_keyframe_interval() {
    let _vaapi = lock_vaapi();
    let _ = tracing_subscriber::fmt::try_init();

    let width: u32 = 128;
    let height: u32 = 128;
    let params = RateParams {
        bps: 2_000_000,
        fps: 30,
    };
    let mut encoder = match FullFrameEncoder::with_params(width, height, params) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping full_frame_keyframe_interval test (no encoder): {e}");
            return;
        }
    };
    assert_eq!(encoder.params(), params);

    // This test is about the IDRs, so it feeds the production entry point
    // (`encode_nv12_buffer`) with ordinary heap memory: no file descriptor is
    // ever handed to the VA driver.
    let nv12 = vec![128u8; (width * height * 3 / 2) as usize];

    let mut keyframes: Vec<i64> = Vec::new();
    let mut output_idx: i64 = 0;
    // 200 frames: more than the old GOP of 11 many times over, and more than
    // 3 s at 30 fps. A request is made at frame 120.
    for i in 0..200 {
        if i == 120 {
            encoder.request_keyframe();
        }
        match encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height)
        {
            Ok(Some(encoded)) => {
                assert!(!encoded.payload.is_empty());
                if encoded.is_keyframe {
                    keyframes.push(output_idx);
                }
                output_idx += 1;
            }
            Ok(None) => {}
            Err(e) => panic!("encode_nv12_buffer failed: {e}"),
        }
    }
    eprintln!("keyframe output indices: {keyframes:?}");
    assert_eq!(
        keyframes.len(),
        2,
        "the first frame and the requested one: {keyframes:?}"
    );
    assert_eq!(keyframes[0], 0);
    assert!(
        (118..=125).contains(&keyframes[1]),
        "the request was made at frame 120 (the encoder may buffer a few): {keyframes:?}"
    );
}

#[test]
fn request_keyframe_forces_idr_outside_gop_boundary() {
    let _vaapi = lock_vaapi();
    let width = 320;
    let height = 240;
    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping request_keyframe test (no encoder): {e}");
            return;
        }
    };
    // PTS 0 is automatically a keyframe; consume it.
    let nv12_size = (width * height * 3 / 2) as usize;
    let nv12 = vec![128u8; nv12_size];
    let _ = encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height);
    // PTS 1 normally would NOT be a keyframe (the encoder has no GOP of its own). Request one,
    // then drain frames until a packet emerges — the encoder may buffer the first
    // few PTS values before emitting the IDR. The first emitted packet must be a
    // keyframe; if no packet appears within a reasonable window the test fails.
    encoder.request_keyframe();
    let mut keyframe_observed = false;
    let mut keyframe_pts = None;
    for pts in 1..=4 {
        if let Ok(Some(out)) =
            encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height)
        {
            assert!(out.is_keyframe,
                "first packet after request_keyframe() must be IDR (got P-frame at drain pts={pts})");
            keyframe_observed = true;
            keyframe_pts = Some(pts);
            break;
        }
    }
    assert!(
        keyframe_observed,
        "encoder buffered all PTS 1-4 frames; no IDR ever emitted"
    );

    // Subsequent encodes (continuing PTS sequence after the keyframe) should NOT
    // all be keyframes — the latch was one-shot. Drain another 5 frames; at least
    // one must be a P-frame. Stop well before PTS 11.
    let start_pts = keyframe_pts.unwrap() + 1;
    let mut saw_p_frame = false;
    for _pts in start_pts..(start_pts + 5).min(10) {
        if let Ok(Some(out)) =
            encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height)
        {
            if !out.is_keyframe {
                saw_p_frame = true;
                break;
            }
        }
    }
    assert!(saw_p_frame, "latch must not persist beyond one frame");
}

/// The default open parameters are a CBR rate at the server's default 30 fps,
/// and the VBV is one second of the rate.
#[test]
fn default_rate_params_are_cbr_at_thirty_fps() {
    let p = RateParams::default();
    assert_eq!(p.fps, 30);
    assert!(p.bps >= 300_000);
    assert_eq!(p.vbv_bits() as u32, p.bps, "VBV is one second of the rate");
    assert_eq!(
        RateParams {
            bps: u32::MAX,
            fps: 30
        }
        .vbv_bits(),
        i32::MAX,
        "an absurd rate must not overflow the VBV"
    );
}

/// CBR holds the output to the target, on content that moves and compresses
/// the way a desktop does (a structured moving pattern, not noise).
///
/// 320x240 at 30 fps for 5 s at 1 Mbit/s. The checks, from the rate control's
/// own definition (the output of any window of a CBR stream is bounded by the
/// rate over the window plus what the VBV may hold):
///  * every sliding 1 s window (30 consecutive output frames) is at most
///    `1.2 x (bps + vbv) / 8` bytes, `vbv` being one second of the rate;
///  * the whole run is at most `1.5 x bps x 5 s / 8`.
///
/// Quality-controlled (CQP) output of this pattern is several times that.
///
/// To see it fail: mutate `try_open_vaapi` to open for CQP instead of CBR,
/// replacing `opts.set("rc_mode", "CBR");` with
/// `opts.set("rc_mode", "CQP"); opts.set("qp", "12");` (for the software
/// fallback, delete the `nal-hrd` option and the `set_bit_rate` calls).
///
/// Not run in CI (needs the GPU lock); run it on hardware.
#[test]
fn cbr_output_stays_near_the_target_rate_on_moving_content() {
    let _vaapi = lock_vaapi();
    let (width, height) = (320u32, 240u32);
    let params = RateParams {
        bps: 1_000_000,
        fps: 30,
    };
    let mut encoder = match FullFrameEncoder::with_params(width, height, params) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping cbr test (no encoder): {e}");
            return;
        }
    };
    let frames = 5 * params.fps as usize;
    let mut sizes: Vec<usize> = Vec::new();
    for t in 0..frames {
        // A diagonal XOR pattern drifting 3 pixels a frame, with text-like
        // 8x8 blocks of flat chroma that change every 10 frames.
        let mut nv12 = vec![128u8; (width * height * 3 / 2) as usize];
        for y in 0..height as usize {
            for x in 0..width as usize {
                nv12[y * width as usize + x] = (((x + 3 * t) ^ (2 * y + t)) & 0xFF) as u8;
            }
        }
        let uv = (width * height) as usize;
        for by in 0..(height as usize / 16) {
            for bx in 0..(width as usize / 8) {
                let v = (((bx * 7 + by * 13 + t / 10 * 29) * 37) & 0xFF) as u8;
                let row = by * width as usize + bx * 8;
                for k in 0..8 {
                    nv12[uv + row + k] = v;
                }
            }
        }
        if let Ok(Some(out)) =
            encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height)
        {
            sizes.push(out.payload.len());
        }
    }
    assert!(
        sizes.len() + 5 >= frames,
        "premise: the encoder produced the frames"
    );
    let total: usize = sizes.iter().sum();
    let bits_per_s = f64::from(params.bps);
    let window_bound = (1.2 * (bits_per_s + f64::from(params.vbv_bits())) / 8.0) as usize;
    let run_bound = (1.5 * bits_per_s * 5.0 / 8.0) as usize;
    let worst = sizes
        .windows(params.fps as usize)
        .map(|w| w.iter().sum::<usize>())
        .max()
        .expect("at least one window");
    eprintln!("cbr: total {total} B (bound {run_bound}), worst 1 s window {worst} B (bound {window_bound})");
    assert!(total > 0, "premise: output");
    assert!(
        worst <= window_bound,
        "a 1 s window carried {worst} B against a bound of {window_bound}"
    );
    assert!(
        total <= run_bound,
        "the run carried {total} B against a bound of {run_bound}"
    );
}
