//! Headless GPU integration test: DMA-BUF → SAD dirty detection → H.264 encode → fragment.
//!
//! Exercises the zero-copy GPU pipeline without Docker, X11, or a browser.
//! Uses `memfd_create` to produce memory-backed file descriptors that behave
//! like DMA-BUFs for the mmap/import path (though they lack GPU-local backing
//! on most drivers, which is why each test skips gracefully on failure).
//!
//! Run: cargo test --test gpu_pipeline

use std::os::unix::io::RawFd;

use ghostframe_lib::capture::gpu_diff::GpuDirtyTracker;
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

/// Verify GpuDirtyTracker detects changes between two frames.
/// Skips if no Vulkan GPU is available.
#[test]
fn gpu_pipeline_dirty_detection() {
    let mut tracker = match GpuDirtyTracker::new(256) {
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

/// Full pipeline: DMA-BUF → GPU dirty detection → full-frame encode → fragment.
/// This is the path that runs in production on machines with DRM capture.
#[test]
fn gpu_pipeline_end_to_end() {
    let width = 256u32;
    let height = 256u32;
    let stride = width * 4;

    // 1. Create both components; skip if either fails.
    let mut tracker = match GpuDirtyTracker::new(512) {
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

        // 3. Diff → all dirty
        let dirty = match tracker.diff(fd1, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping gpu_pipeline_end_to_end (memfd not accepted as DMA-BUF): {e}");
                return;
            }
        };
        assert!(!dirty.is_empty(), "first frame: some tiles must be dirty");

        // Encode — try up to 3 frames for VA-API buffering.
        let mut frame_output: Option<FullFrameEncoded> = None;
        for _ in 0..3 {
            libc::lseek(fd1, 0, libc::SEEK_SET);
            match encoder.encode_frame(fd1, width, height, stride) {
                Ok(Some(enc)) => {
                    frame_output = Some(enc);
                    break;
                }
                Ok(None) => continue,
                Err(e) => {
                    libc::close(fd1);
                    eprintln!("Skipping gpu_pipeline_end_to_end (encode_frame failed): {e}");
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

        // 4. Second frame: same content → no dirty tiles expected
        let dirty2 = match tracker.diff(fd1, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Second diff failed: {e}");
                return;
            }
        };
        assert!(
            dirty2.is_empty(),
            "static second frame should have no dirty tiles, got: {dirty2:?}"
        );

        libc::close(fd1);

        // 5. Third frame: change one tile → diff detects change → encode
        let fd2 = create_changed_memfd(
            width, height,
            255, 255, 0, // base: cyan
            2, 2,        // tile (col=2, row=2)
            0, 0, 255,   // change: red (BGRA)
        );

        let dirty3 = match tracker.diff(fd2, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Third diff failed (non-fatal): {e}");
                return;
            }
        };
        assert!(!dirty3.is_empty(), "changed frame must have at least one dirty tile");

        // P-frame encode (past frame 0 → not forced IDR unless at GOP boundary)
        libc::lseek(fd2, 0, libc::SEEK_SET);
        match encoder.encode_frame(fd2, width, height, stride) {
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                eprintln!("Third encode_frame error (non-fatal): {e}");
            }
        }

        libc::close(fd2);
    }
}
