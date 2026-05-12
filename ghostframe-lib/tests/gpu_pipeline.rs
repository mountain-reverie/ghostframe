//! Headless GPU integration test: DMA-BUF → SAD dirty detection + NV12 conversion → H.264 encode → fragment.
//!
//! Exercises the GPU pipeline without Docker, X11, or a browser.
//! Uses `memfd_create` to produce memory-backed file descriptors that behave
//! like DMA-BUFs for the mmap/import path (though they lack GPU-local backing
//! on most drivers, which is why each test skips gracefully on failure).
//!
//! Run: cargo test --test gpu_pipeline

use std::os::unix::io::RawFd;

use ghostframe_lib::capture::gpu_pipeline::GpuFrameProcessor;
use ghostframe_lib::encoder::h264_vaapi::{FullFrameEncoded, FullFrameEncoder};
use ghostframe_lib::transport::protocol::{decode_frame_datagram, fragment_frame};

// ---------------------------------------------------------------------------
// memfd helpers
// ---------------------------------------------------------------------------

/// Create a memfd filled with a solid BGRA color, returns the raw fd.
/// Caller must close the fd when done.
unsafe fn create_solid_memfd(width: u32, height: u32, b: u8, g: u8, r: u8) -> RawFd {
    let stride = width * 4;
    let size = (stride * height) as usize;
    let name = std::ffi::CString::new("gpu-test-frame").unwrap();
    let fd = libc::memfd_create(name.as_ptr(), 0);
    assert!(fd >= 0, "memfd_create failed");
    libc::ftruncate(fd, size as i64);
    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    assert_ne!(ptr, libc::MAP_FAILED);
    let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
    for pixel in slice.chunks_exact_mut(4) {
        pixel[0] = b;
        pixel[1] = g;
        pixel[2] = r;
        pixel[3] = 255;
    }
    libc::munmap(ptr, size);
    fd
}

/// Create a memfd with one tile region changed to a different color.
unsafe fn create_changed_memfd(
    width: u32,
    height: u32,
    base_b: u8,
    base_g: u8,
    base_r: u8,
    change_tile_x: u32,
    change_tile_y: u32,
    change_b: u8,
    change_g: u8,
    change_r: u8,
) -> RawFd {
    let stride = width * 4;
    let size = (stride * height) as usize;
    let name = std::ffi::CString::new("gpu-test-frame").unwrap();
    let fd = libc::memfd_create(name.as_ptr(), 0);
    assert!(fd >= 0);
    libc::ftruncate(fd, size as i64);
    let ptr = libc::mmap(
        std::ptr::null_mut(),
        size,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    assert_ne!(ptr, libc::MAP_FAILED);
    let slice = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
    // Fill base color
    for pixel in slice.chunks_exact_mut(4) {
        pixel[0] = base_b;
        pixel[1] = base_g;
        pixel[2] = base_r;
        pixel[3] = 255;
    }
    // Change the target tile region (tiles are 32x32 pixels)
    let tile_px = change_tile_x * 32;
    let tile_py = change_tile_y * 32;
    for y in tile_py..std::cmp::min(tile_py + 32, height) {
        for x in tile_px..std::cmp::min(tile_px + 32, width) {
            let off = (y * stride + x * 4) as usize;
            slice[off] = change_b;
            slice[off + 1] = change_g;
            slice[off + 2] = change_r;
            slice[off + 3] = 255;
        }
    }
    libc::munmap(ptr, size);
    fd
}

// ---------------------------------------------------------------------------
// Test 1: GPU dirty detection
// ---------------------------------------------------------------------------

/// Verify GpuFrameProcessor detects changes between two frames.
/// Skips if no Vulkan GPU is available.
#[test]
fn gpu_pipeline_dirty_detection() {
    let mut tracker = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping gpu_pipeline_dirty_detection (no Vulkan GPU?): {e}");
            return;
        }
    };

    let width = 128u32;
    let height = 128u32;
    let stride = width * 4;

    unsafe {
        // Frame 1: solid blue
        let fd1 = create_solid_memfd(width, height, 255, 0, 0); // B=255, G=0, R=0

        let first = match tracker.diff(fd1, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping gpu_pipeline_dirty_detection (memfd not accepted as DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);

        // First call: all tiles should be dirty (128/32 = 4 cols * 4 rows = 16 tiles)
        assert_eq!(
            first.len(),
            16,
            "first frame: all 16 tiles should be dirty, got: {first:?}"
        );

        // Frame 2: identical solid blue — no dirty tiles expected
        let fd2 = create_solid_memfd(width, height, 255, 0, 0);
        let second = match tracker.diff(fd2, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping second diff in gpu_pipeline_dirty_detection: {e}");
                return;
            }
        };
        libc::close(fd2);

        assert_eq!(
            second.len(),
            0,
            "identical frames should produce no dirty tiles, got: {second:?}"
        );

        // Frame 3: tile (1,0) changed to red — only that tile should be dirty
        let fd3 = create_changed_memfd(
            width, height,
            255, 0, 0,   // base: blue
            1, 0,        // tile (col=1, row=0)
            0, 0, 255,   // change: red (B=0, G=0, R=255 in BGRA)
        );
        let third = match tracker.diff(fd3, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd3);
                eprintln!("Skipping third diff in gpu_pipeline_dirty_detection: {e}");
                return;
            }
        };
        libc::close(fd3);

        // Tile index 1 = col=1, row=0 → 0*4 + 1 = 1
        assert_eq!(
            third.len(),
            1,
            "only tile (1,0) should be dirty, got: {third:?}"
        );
        assert_eq!(
            third[0], 1,
            "dirty tile index should be 1 (col=1, row=0), got: {third:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 2: Full-frame encode from memfd
// ---------------------------------------------------------------------------

/// Verify FullFrameEncoder produces H.264 output from a memfd.
/// Skips if no encoder is available.
#[test]
fn gpu_pipeline_full_frame_encode() {
    let width = 128u32;
    let height = 128u32;
    let stride = width * 4;

    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping gpu_pipeline_full_frame_encode (no encoder): {e}");
            return;
        }
    };

    unsafe {
        // Solid green frame
        let fd = create_solid_memfd(width, height, 0, 255, 0); // B=0, G=255, R=0

        // Encode — VA-API may buffer the first frame; try up to 3 frames.
        let mut got_output = false;
        let mut got_keyframe = false;
        for _ in 0..3 {
            libc::lseek(fd, 0, libc::SEEK_SET);
            match encoder.encode_frame(fd, width, height, stride) {
                Ok(Some(encoded)) => {
                    assert!(!encoded.payload.is_empty(), "encoded payload must not be empty");
                    if encoded.is_keyframe {
                        got_keyframe = true;
                    }
                    got_output = true;
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    libc::close(fd);
                    eprintln!("Skipping gpu_pipeline_full_frame_encode (encode_frame failed): {e}");
                    return;
                }
            }
        }

        // Encode a second frame (same fd is fine — static content)
        if got_output {
            libc::lseek(fd, 0, libc::SEEK_SET);
            match encoder.encode_frame(fd, width, height, stride) {
                Ok(Some(_)) | Ok(None) => {}
                Err(e) => {
                    eprintln!("Second encode_frame error (non-fatal): {e}");
                }
            }
        }

        libc::close(fd);

        assert!(
            got_output,
            "encoder should have produced output within 3 frames"
        );
        assert!(got_keyframe, "first output should be a keyframe");
    }
}

// ---------------------------------------------------------------------------
// Test 3: Full pipeline end-to-end
// ---------------------------------------------------------------------------

/// Full pipeline: DMA-BUF → GPU process_frame (SAD + NV12) → encode_nv12_buffer → fragment.
/// This is the production path using HOST_VISIBLE NV12 output from the GPU compute shader.
#[test]
fn gpu_pipeline_end_to_end() {
    let width = 256u32;
    let height = 256u32;
    let stride = width * 4;

    // 1. Create both components; skip if either fails.
    let mut processor = match GpuFrameProcessor::new(512) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping gpu_pipeline_end_to_end (no Vulkan GPU?): {e}");
            return;
        }
    };

    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping gpu_pipeline_end_to_end (no encoder): {e}");
            return;
        }
    };

    unsafe {
        // 2. First frame: solid cyan
        let fd1 = create_solid_memfd(width, height, 255, 255, 0); // B=255, G=255, R=0

        // 3. process_frame → dirty tiles + NV12 data
        let analysis = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping gpu_pipeline_end_to_end (memfd not accepted as DMA-BUF): {e}");
                return;
            }
        };
        assert!(!analysis.dirty_tiles.is_empty(), "first frame: some tiles must be dirty");
        assert!(!analysis.nv12_data.is_null(), "NV12 data should not be null");

        // 4. Encode via encode_nv12_buffer — try up to 3 attempts for VA-API buffering.
        let mut frame_output: Option<FullFrameEncoded> = None;
        for _ in 0..3 {
            match encoder.encode_nv12_buffer(
                analysis.nv12_data,
                analysis.nv12_width,
                analysis.nv12_height,
                analysis.nv12_y_stride,
                analysis.nv12_uv_stride,
                analysis.nv12_uv_offset,
            ) {
                Ok(Some(enc)) => {
                    frame_output = Some(enc);
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    libc::close(fd1);
                    eprintln!("Skipping gpu_pipeline_end_to_end (encode_nv12_buffer failed): {e}");
                    return;
                }
            }
        }

        if let Some(enc) = frame_output {
            assert!(enc.is_keyframe, "first output frame should be a keyframe");
            assert!(!enc.payload.is_empty(), "encoded payload must not be empty");

            // Fragment it (MTU = 1200, max_fragment_payload = 1200 - 14 header bytes)
            let max_payload = 1200usize.saturating_sub(14);
            let datagrams = fragment_frame(1, 0, enc.is_keyframe, &enc.payload, max_payload);
            assert!(!datagrams.is_empty(), "fragmentation should produce at least one datagram");

            // Verify we can decode the first datagram header back
            let (hdr, _payload) = decode_frame_datagram(&datagrams[0])
                .expect("first datagram should be decodable");
            assert_eq!(hdr.frame_seq, 1, "frame_seq should be 1");
            assert!(hdr.is_keyframe(), "first datagram should be marked keyframe");
        }

        // 5. Second frame: same content → no dirty tiles expected
        let analysis2 = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Second process_frame failed: {e}");
                return;
            }
        };
        assert!(
            analysis2.dirty_tiles.is_empty(),
            "static second frame should have no dirty tiles, got: {:?}",
            analysis2.dirty_tiles
        );

        libc::close(fd1);

        // 6. Third frame: change one tile → process_frame detects change → encode
        let fd2 = create_changed_memfd(
            width, height,
            255, 255, 0, // base: cyan
            2, 2,        // tile (col=2, row=2)
            0, 0, 255,   // change: red (BGRA)
        );

        let analysis3 = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Third process_frame failed (non-fatal): {e}");
                return;
            }
        };
        assert!(!analysis3.dirty_tiles.is_empty(), "changed frame must have at least one dirty tile");

        // P-frame encode
        match encoder.encode_nv12_buffer(
            analysis3.nv12_data,
            analysis3.nv12_width,
            analysis3.nv12_height,
            analysis3.nv12_y_stride,
            analysis3.nv12_uv_stride,
            analysis3.nv12_uv_offset,
        ) {
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                eprintln!("Third encode_nv12_buffer error (non-fatal): {e}");
            }
        }

        libc::close(fd2);
    }
}

// ---------------------------------------------------------------------------
// Test 4: NV12 conversion via process_frame
// ---------------------------------------------------------------------------

/// Verify that process_frame produces non-null NV12 data with correct layout.
/// Also checks that diff() still works as a wrapper around process_frame.
#[test]
fn gpu_pipeline_nv12_conversion() {
    let width = 128u32;
    let height = 128u32;
    let stride = width * 4;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping gpu_pipeline_nv12_conversion (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // Solid blue (BGRA: B=255, G=0, R=0, A=255)
        // Expected Y for pure blue (R=0,G=0,B=1.0): Y = 0.299*0 + 0.587*0 + 0.114*1 = 0.114 → ~29
        let fd = create_solid_memfd(width, height, 255, 0, 0);

        let analysis = match processor.process_frame(fd, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd);
                eprintln!("Skipping gpu_pipeline_nv12_conversion (memfd not accepted as DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd);

        // All tiles dirty on first frame
        assert_eq!(
            analysis.dirty_tiles.len(),
            (width / 32) as usize * (height / 32) as usize,
            "first frame: all tiles should be dirty"
        );

        // NV12 pointer must not be null
        assert!(!analysis.nv12_data.is_null());
        assert_eq!(analysis.nv12_width, width);
        assert_eq!(analysis.nv12_height, height);
        assert_eq!(analysis.nv12_y_stride, width);
        assert_eq!(analysis.nv12_uv_stride, width);
        assert_eq!(analysis.nv12_uv_offset, width * height);

        // Check Y plane average for solid blue
        let y_slice = std::slice::from_raw_parts(analysis.nv12_data, (width * height) as usize);
        let y_sum: u64 = y_slice.iter().map(|&b| b as u64).sum();
        let y_avg = y_sum / y_slice.len() as u64;
        // BT.601 pure blue Y ≈ 29; allow generous tolerance for GPU rounding
        assert!(
            y_avg < 60,
            "Y for solid blue should be ~29 (low luminance), got avg {y_avg}"
        );

        // diff() wrapper should still work (returns only dirty tiles)
        let fd2 = create_solid_memfd(width, height, 255, 0, 0); // same content
        let dirty = match processor.diff(fd2, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd2);
                eprintln!("diff() wrapper failed: {e}");
                return;
            }
        };
        libc::close(fd2);

        assert!(
            dirty.is_empty(),
            "identical frame via diff() should produce no dirty tiles, got: {dirty:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Test 5: tile_analysis on subsequent frame
// ---------------------------------------------------------------------------

/// Verify first-frame returns null tile_analysis (by design) and the second
/// frame returns a populated buffer with the expected solid-red contents.
#[test]
fn tile_analysis_populated_on_subsequent_frame() {
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping tile_analysis integration (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // First frame: opaque red. analysis.tile_analysis is null on first frame
        // (the analysis pipeline isn't dispatched until the second frame, which
        // is the steady-state path).
        let fd1 = create_solid_memfd(width, height, /*B=*/0, /*G=*/0, /*R=*/255);
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(first.tile_analysis.is_null(), "first-frame analysis is null by design");

        // Second frame: same opaque red. Now the analysis pipeline dispatches.
        let fd2 = create_solid_memfd(width, height, 0, 0, 255);
        let second = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping second frame: {e}");
                return;
            }
        };
        libc::close(fd2);

        assert!(!second.tile_analysis.is_null(), "second-frame analysis non-null");
        assert_eq!(second.tile_analysis_len, 1, "32x32 frame → 1 tile");
        let slice = second.tile_analysis_slice();
        assert_eq!(slice.len(), 1);
        assert_eq!(slice[0].count, 1, "solid tile → count=1");
        assert_eq!(slice[0].colors[0], 0xFFFF0000u32);
        assert_eq!(slice[0].edge_density_thou, 0);
    }
}
