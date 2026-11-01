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

#[test]
fn full_frame_keyframe_interval() {
    let _vaapi = lock_vaapi();
    let _ = tracing_subscriber::fmt::try_init();

    let width: u32 = 128;
    let height: u32 = 128;

    // The production GOP is 3 s (180 frames), too long to cross in a unit
    // test; the parameter, not the constant, is what the encoder honours.
    const GOP: u32 = 11;
    let params = RateParams {
        bps: 2_000_000,
        gop_frames: GOP,
    };
    let mut encoder = match FullFrameEncoder::with_params(width, height, params) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping full_frame_keyframe_interval test (no encoder): {e}");
            return;
        }
    };
    assert_eq!(encoder.params(), params);

    // This test is about the GOP, so it feeds the production entry point
    // (`encode_nv12_buffer`) with ordinary heap memory: no file descriptor is
    // ever handed to the VA driver.
    let nv12 = vec![128u8; (width * height * 3 / 2) as usize];

    let mut keyframe_pts: Vec<i64> = Vec::new();
    let mut output_idx: i64 = 0;

    // Encode 14 frames (more than one full GOP of `GOP` = 11).
    for _ in 0..14 {
        match encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height)
        {
            Ok(Some(encoded)) => {
                assert!(!encoded.payload.is_empty());
                if encoded.is_keyframe {
                    keyframe_pts.push(output_idx);
                }
                output_idx += 1;
            }
            Ok(None) => {}
            Err(e) => panic!("encode_nv12_buffer failed: {e}"),
        }
    }

    eprintln!("keyframe output indices: {keyframe_pts:?}");

    // We must have gotten at least one keyframe (the very first frame).
    assert!(
        !keyframe_pts.is_empty(),
        "expected at least one keyframe in 14 frames"
    );

    // If we got at least two keyframes, check that the interval is ≈ GOP size.
    if keyframe_pts.len() >= 2 {
        let interval = keyframe_pts[1] - keyframe_pts[0];
        assert!(
            (GOP as i64 - 2..=GOP as i64 + 2).contains(&interval),
            "keyframe interval should be ~{GOP} frames, got {interval}"
        );
    }
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
    // PTS 1 normally would NOT be a keyframe (the GOP is 180 frames by default). Request one,
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
    // one must be a P-frame. Stop well before PTS 11 (next natural GOP boundary).
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

/// The default open parameters are the 3 s GOP at 60 fps and a CBR rate, and
/// the VBV is one second of the rate.
#[test]
fn default_rate_params_are_cbr_with_a_three_second_gop() {
    let p = RateParams::default();
    assert_eq!(p.gop_frames, 180, "3 s at 60 fps");
    assert!(p.bps >= 300_000);
    assert_eq!(p.vbv_bits() as u32, p.bps, "VBV is one second of the rate");
    assert_eq!(
        RateParams {
            bps: u32::MAX,
            gop_frames: 1
        }
        .vbv_bits(),
        i32::MAX,
        "an absurd rate must not overflow the VBV"
    );
}

/// CBR holds the output to the target: noisy content at a low rate must not
/// come out at the rate uncontrolled quality would give it (about 40 kB a frame
/// at this size, 4.8 MB over the 120 frames, 38 x the target). Bound: over 120
/// frames (2 s at the encoder's 60 fps) the output is at most 8 x the target
/// for those 2 s. The slack is the first IDR of noise, which a one-second VBV
/// lets through whole (a software x264 run, by accident, was above 2.5 x), plus
/// the rate control's convergence; 8 x is still a fifth of the uncontrolled
/// output. Not run in CI (needs the GPU lock); the orchestrator runs it with
/// the other encoder tests, and the bound is the first thing to adjust if it
/// is too tight on hardware.
#[test]
fn cbr_output_stays_near_the_target_rate_on_noisy_content() {
    let _vaapi = lock_vaapi();
    let (width, height) = (320u32, 240u32);
    let params = RateParams {
        bps: 500_000,
        gop_frames: 180,
    };
    let mut encoder = match FullFrameEncoder::with_params(width, height, params) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping cbr test (no encoder): {e}");
            return;
        }
    };
    let mut total = 0usize;
    let mut x: u32 = 0x1234_5678;
    for _ in 0..120 {
        let nv12: Vec<u8> = (0..(width * height * 3 / 2) as usize)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        if let Ok(Some(out)) =
            encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height)
        {
            total += out.payload.len();
        }
    }
    let target_bytes = (params.bps as usize / 8) * 2;
    eprintln!("cbr: {total} bytes in 2 s, target {target_bytes}");
    assert!(total > 0, "premise: the encoder produced output");
    assert!(
        total <= target_bytes * 8,
        "CBR output {total} bytes over 2 s against a target of {target_bytes}"
    );
}
