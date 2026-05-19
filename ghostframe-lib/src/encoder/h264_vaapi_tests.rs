use super::*;

#[test]
fn encode_solid_red_tile() {
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

/// Create a memfd of the given size, fill it with `fill_fn`, and return
/// the raw file descriptor.  The caller is responsible for closing it.
fn make_bgra_memfd(width: u32, height: u32, fill_fn: impl Fn(usize) -> [u8; 4]) -> RawFd {
    use std::ffi::CString;
    let name = CString::new("test_frame").unwrap();
    let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 0) as RawFd };
    assert!(fd >= 0, "memfd_create failed");

    let stride = width * 4;
    let total = (height * stride) as usize;
    let mut buf = vec![0u8; total];
    for (i, pixel) in buf.chunks_exact_mut(4).enumerate() {
        let rgba = fill_fn(i);
        pixel.copy_from_slice(&rgba);
    }

    // Write the buffer to the memfd.
    let written = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, total) };
    assert_eq!(written as usize, total, "write to memfd failed");
    // Seek back to beginning so mmap can read from offset 0.
    unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

    fd
}

#[test]
fn full_frame_encode_from_memfd() {
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

    // Solid red BGRA pixels.
    let fd = make_bgra_memfd(width, height, |_| [0, 0, 255, 255]);

    let stride = width * 4;

    // Encode twice — first should be a keyframe (pts == 0).
    // VA-API may buffer the first; try up to three frames.
    let mut got_output = false;
    let mut got_keyframe = false;
    for _ in 0..3 {
        // Re-seek before each encode call (mmap reads from beginning).
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
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

    unsafe { libc::close(fd) };

    assert!(
        got_output,
        "encoder should have produced output within 3 frames"
    );
    assert!(got_keyframe, "first output should be a keyframe");
}

#[test]
fn full_frame_keyframe_interval() {
    let _ = tracing_subscriber::fmt::try_init();

    let width: u32 = 128;
    let height: u32 = 128;

    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping full_frame_keyframe_interval test (no encoder): {e}");
            return;
        }
    };

    let fd = make_bgra_memfd(width, height, |_| [0, 255, 0, 255]); // solid green
    let stride = width * 4;

    let mut keyframe_pts: Vec<i64> = Vec::new();
    let mut output_idx: i64 = 0;

    // Encode 14 frames (more than one full GOP of 11).
    for _ in 0..14 {
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
        match encoder.encode_frame(fd, width, height, stride) {
            Ok(Some(encoded)) => {
                assert!(!encoded.payload.is_empty());
                if encoded.is_keyframe {
                    keyframe_pts.push(output_idx);
                }
                output_idx += 1;
            }
            Ok(None) => {}
            Err(e) => panic!("encode_frame failed: {e}"),
        }
    }

    unsafe { libc::close(fd) };

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
            interval >= 9 && interval <= 13,
            "keyframe interval should be ~11 frames, got {interval}"
        );
    }
}

#[test]
fn request_keyframe_forces_idr_outside_gop_boundary() {
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
    let _ =
        encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height);
    // PTS 1 normally would NOT be a keyframe (FULL_FRAME_GOP = 11). Request one,
    // then drain frames until a packet emerges — the encoder may buffer the first
    // few PTS values before emitting the IDR. The first emitted packet must be a
    // keyframe; if no packet appears within a reasonable window the test fails.
    encoder.request_keyframe();
    let mut keyframe_observed = false;
    let mut keyframe_pts = None;
    for pts in 1..=4 {
        if let Ok(Some(out)) = encoder.encode_nv12_buffer(
            nv12.as_ptr(),
            width,
            height,
            width,
            width,
            width * height,
        ) {
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
        if let Ok(Some(out)) = encoder.encode_nv12_buffer(
            nv12.as_ptr(),
            width,
            height,
            width,
            width,
            width * height,
        ) {
            if !out.is_keyframe {
                saw_p_frame = true;
                break;
            }
        }
    }
    assert!(saw_p_frame, "latch must not persist beyond one frame");
}
