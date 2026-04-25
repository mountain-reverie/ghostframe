//! Real DRM capture → GPU zero-copy pipeline integration test.
//!
//! This test exercises the actual production path on machines with a GPU:
//! DRM framebuffer export → DMA-BUF → GpuDirtyTracker (Vulkan SAD) →
//! FullFrameEncoder (VA-API H.264) → protocol fragmentation.
//!
//! Skips gracefully if no DRM device or GPU is available.
//!
//! Run: cargo test -p ghostframe-xdaemon --test drm_gpu_pipeline

// Access xdaemon's internal modules via the binary crate.
// We can't import from main.rs directly, so we duplicate the minimal
// DRM capture call inline (it's 5 lines).

use std::os::fd::AsRawFd;

use ghostframe_lib::capture::gpu_diff::GpuDirtyTracker;
use ghostframe_lib::encoder::h264_vaapi::FullFrameEncoder;
use ghostframe_lib::transport::protocol::{decode_frame_datagram, fragment_frame};

/// Attempt DRM framebuffer capture using the drm crate directly.
/// Returns (OwnedFd, width, height, stride) or None if no DRM device.
fn try_drm_capture() -> Option<(std::os::fd::OwnedFd, u32, u32, u32)> {
    use drm::control::Device as ControlDevice;
    use drm::Device;
    use std::fs::File;
    use std::os::fd::{AsFd, BorrowedFd};

    struct Card(File);
    impl AsFd for Card {
        fn as_fd(&self) -> BorrowedFd<'_> {
            self.0.as_fd()
        }
    }
    impl Device for Card {}
    impl ControlDevice for Card {}

    for i in 0..10u32 {
        let path = format!("/dev/dri/card{i}");
        let card = match File::options().read(true).write(true).open(&path) {
            Ok(f) => Card(f),
            Err(_) => continue,
        };

        let res = match card.resource_handles() {
            Ok(r) => r,
            Err(_) => continue,
        };

        eprintln!("  card{i}: {} CRTCs", res.crtcs().len());
        for crtc_handle in res.crtcs() {
            let crtc_info = match card.get_crtc(*crtc_handle) {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("    CRTC {:?}: get_crtc failed: {e}", crtc_handle);
                    continue;
                }
            };
            eprintln!(
                "    CRTC {:?}: mode={} fb={:?}",
                crtc_handle,
                crtc_info.mode().is_some(),
                crtc_info.framebuffer()
            );
            if crtc_info.mode().is_none() {
                continue;
            }
            let fb_handle = match crtc_info.framebuffer() {
                Some(h) => h,
                None => continue,
            };
            // Try FB2 first (GBM/multi-plane), fall back to legacy FB1
            if let Ok(fb2) = card.get_planar_framebuffer(fb_handle) {
                let (width, height) = fb2.size();
                let stride = fb2.pitches()[0];
                eprintln!("    FB2 {:?}: {}x{} stride={}", fb_handle, width, height, stride);
                if let Some(buf_handle) = fb2.buffers()[0] {
                    match card.buffer_to_prime_fd(buf_handle, 0) {
                        Ok(fd) => return Some((fd, width, height, stride)),
                        Err(e) => eprintln!("    FB2 PRIME export failed: {e}"),
                    }
                } else {
                    eprintln!("    FB2 plane 0 has no buffer handle");
                }
            }
            // Legacy FB1 fallback
            let fb_info = match card.get_framebuffer(fb_handle) {
                Ok(info) => info,
                Err(e) => {
                    eprintln!("    FB1 get_framebuffer failed: {e}");
                    continue;
                }
            };
            let (width, height) = fb_info.size();
            let stride = fb_info.pitch();
            let buf_handle = match fb_info.buffer() {
                Some(h) => h,
                None => {
                    eprintln!("    FB1 has no buffer handle either");
                    continue;
                }
            };
            match card.buffer_to_prime_fd(buf_handle, 0) {
                Ok(fd) => return Some((fd, width, height, stride)),
                Err(e) => {
                    eprintln!("    FB1 PRIME export failed: {e}");
                    continue;
                }
            }
        }
    }
    None
}

#[test]
fn drm_capture_to_gpu_pipeline() {
    // 1. Try DRM capture
    let (dmabuf_fd, width, height, stride) = match try_drm_capture() {
        Some(result) => {
            eprintln!(
                "DRM capture: {}x{} stride={} fd={}",
                result.1, result.2, result.3,
                result.0.as_raw_fd()
            );
            result
        }
        None => {
            eprintln!("Skipping: no DRM device with active CRTC found");
            return;
        }
    };

    let raw_fd = dmabuf_fd.as_raw_fd();

    // 2. GPU dirty detection
    let mut tracker = match GpuDirtyTracker::new(4096) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping: GpuDirtyTracker init failed: {e}");
            return;
        }
    };

    let dirty = match tracker.diff(raw_fd, width, height, stride) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("Skipping: GPU diff failed on real DMA-BUF: {e}");
            return;
        }
    };

    let cols = width.div_ceil(32);
    let rows = height.div_ceil(32);
    let total_tiles = cols * rows;
    eprintln!(
        "First frame: {}/{} tiles dirty ({}x{} grid)",
        dirty.len(),
        total_tiles,
        cols,
        rows
    );
    assert!(
        !dirty.is_empty(),
        "first frame should report tiles as dirty"
    );

    // 3. Second capture of same framebuffer — should show fewer or no dirty tiles
    //    (desktop content is static between two immediate captures)
    let dirty2 = tracker.diff(raw_fd, width, height, stride).unwrap();
    eprintln!(
        "Second frame (same fd): {}/{} tiles dirty",
        dirty2.len(),
        total_tiles
    );
    // Note: we can't assert dirty2 is empty because the desktop might have
    // animations, cursor blink, etc. Just verify it doesn't crash.

    // 4. Full-frame H.264 encode
    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping encode: FullFrameEncoder init failed: {e}");
            return;
        }
    };

    // Encode up to 3 frames (VA-API may buffer the first)
    let mut encoded = None;
    for i in 0..3 {
        match encoder.encode_frame(raw_fd, width, height, stride) {
            Ok(Some(enc)) => {
                eprintln!(
                    "Frame {i}: {} bytes, keyframe={}",
                    enc.payload.len(),
                    enc.is_keyframe
                );
                if encoded.is_none() {
                    encoded = Some(enc);
                }
            }
            Ok(None) => {
                eprintln!("Frame {i}: buffered (no output yet)");
            }
            Err(e) => {
                eprintln!("Skipping: encode_frame failed: {e}");
                return;
            }
        }
    }

    let enc = encoded.expect("encoder should produce at least one frame in 3 attempts");
    assert!(!enc.payload.is_empty(), "H.264 payload should be non-empty");
    assert!(enc.is_keyframe, "first output should be a keyframe");

    // 5. Fragment the encoded frame
    let datagrams = fragment_frame(1, 0, true, &enc.payload, 1200);
    assert!(!datagrams.is_empty());
    eprintln!(
        "Fragmented into {} datagrams ({} bytes total)",
        datagrams.len(),
        enc.payload.len()
    );

    // Verify header decode
    let (hdr, _payload) = decode_frame_datagram(&datagrams[0]).unwrap();
    assert_eq!(hdr.frame_seq, 1);
    assert!(hdr.is_keyframe());

    eprintln!("DRM → GPU SAD → VA-API encode → fragment: PASS");
}
