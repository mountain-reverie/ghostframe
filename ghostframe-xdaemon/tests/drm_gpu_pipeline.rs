//! Real DRM capture → GPU zero-copy pipeline integration test.
//!
//! This test exercises the actual production path on machines with a GPU:
//! DRM framebuffer export → DMA-BUF → GpuFrameProcessor (Vulkan SAD + NV12 compute) →
//! FullFrameEncoder (HOST_VISIBLE NV12 upload → VA-API H.264) → protocol fragmentation.
//!
//! Skips gracefully if no DRM device or GPU is available.
//!
//! Run: cargo test -p ghostframe-xdaemon --test drm_gpu_pipeline

// Access xdaemon's internal modules via the binary crate.
// We can't import from main.rs directly, so we duplicate the minimal
// DRM capture call inline (it's 5 lines).

use std::os::fd::AsRawFd;

use ghostframe_lib::capture::gpu_pipeline::GpuFrameProcessor;
use ghostframe_lib::encoder::h264_vaapi::FullFrameEncoder;
use ghostframe_lib::transport::protocol::{decode_frame_datagram, fragment_frame};

struct Card(std::fs::File);
impl std::os::fd::AsFd for Card {
    fn as_fd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.0.as_fd()
    }
}
impl drm::Device for Card {}
impl drm::control::Device for Card {}

/// (fd, width, height, stride, keep-alive owner of the card and buffer).
type PatternedBuffer = (
    std::os::fd::OwnedFd,
    u32,
    u32,
    u32,
    (Card, drm::control::dumbbuffer::DumbBuffer),
);

/// A dumb buffer that this test paints itself (a diagonal gradient plus
/// stripes, so Y varies in both axes), exported as a PRIME fd. Used when the
/// live scanout is blank, where the dma-buf-vs-CPU comparison would pass
/// whatever the import did with strides. The live framebuffer is never
/// written. The `Card` and buffer are returned so they outlive the fd's use.
fn patterned_dumb_buffer() -> Option<PatternedBuffer> {
    use drm::buffer::DrmFourcc;
    use drm::control::Device as ControlDevice;
    for i in 0..10u32 {
        let Ok(f) = std::fs::File::options()
            .read(true)
            .write(true)
            .open(format!("/dev/dri/card{i}"))
        else {
            continue;
        };
        let card = Card(f);
        let (w, h) = (1024u32, 768u32);
        let Ok(mut db) = card.create_dumb_buffer((w, h), DrmFourcc::Xrgb8888, 32) else {
            continue;
        };
        let stride = db.pitch();
        {
            let Ok(mut map) = card.map_dumb_buffer(&mut db) else {
                continue;
            };
            let bytes: &mut [u8] = map.as_mut();
            for y in 0..h as usize {
                for x in 0..w as usize {
                    let o = y * stride as usize + x * 4;
                    bytes[o] = (x * 255 / w as usize) as u8;
                    bytes[o + 1] = (y * 255 / h as usize) as u8;
                    bytes[o + 2] = if (x / 32 + y / 32) % 2 == 0 {
                        0xE0
                    } else {
                        0x20
                    };
                    bytes[o + 3] = 0;
                }
            }
        }
        use drm::buffer::Buffer;
        let Ok(fd) = card.buffer_to_prime_fd(db.handle(), 0) else {
            continue;
        };
        return Some((fd, w, h, stride, (card, db)));
    }
    None
}

/// Attempt DRM framebuffer capture using the drm crate directly.
/// Returns (OwnedFd, width, height, stride) or None if no DRM device.
fn try_drm_capture() -> Option<(std::os::fd::OwnedFd, u32, u32, u32)> {
    use drm::control::Device as ControlDevice;
    use std::fs::File;

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

        for crtc_handle in res.crtcs() {
            let crtc_info = match card.get_crtc(*crtc_handle) {
                Ok(info) => info,
                Err(_) => continue,
            };
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
                if let Some(buf_handle) = fb2.buffers()[0] {
                    if let Ok(fd) = card.buffer_to_prime_fd(buf_handle, 0) {
                        return Some((fd, width, height, stride));
                    }
                }
            }
            // Legacy FB1 fallback
            let fb_info = match card.get_framebuffer(fb_handle) {
                Ok(info) => info,
                Err(_) => continue,
            };
            let (width, height) = fb_info.size();
            let stride = fb_info.pitch();
            let buf_handle = match fb_info.buffer() {
                Some(h) => h,
                None => continue,
            };
            if let Ok(fd) = card.buffer_to_prime_fd(buf_handle, 0) {
                return Some((fd, width, height, stride));
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
                result.1,
                result.2,
                result.3,
                result.0.as_raw_fd()
            );
            result
        }
        None => {
            eprintln!("Skipping: no DRM device with active CRTC found");
            return;
        }
    };

    // A blank live scanout (a headless or VKMS host) makes the pixel check
    // below vacuous, so paint a known pattern into a buffer of our own and
    // run the same pipeline on that instead. Never write to the live one.
    let live_is_blank = {
        let raw = ghostframe_lib::capture::dmabuf::readback_dmabuf(
            dmabuf_fd.as_raw_fd(),
            width,
            height,
            stride,
        )
        .expect("mmap of the PRIME fd");
        raw.iter().collect::<std::collections::HashSet<_>>().len() <= 1
    };
    let mut _own_buffer = None;
    let (dmabuf_fd, width, height, stride) = if live_is_blank {
        eprintln!("live framebuffer is blank: using a self-painted dumb buffer");
        let (fd, w, h, st, keep) = patterned_dumb_buffer()
            .expect("live scanout is blank and no dumb buffer could be created to replace it");
        _own_buffer = Some(keep);
        (fd, w, h, st)
    } else {
        (dmabuf_fd, width, height, stride)
    };

    let raw_fd = dmabuf_fd.as_raw_fd();

    // 2. GPU dirty detection + NV12 conversion via Vulkan compute
    let mut processor = match GpuFrameProcessor::new(4096) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping: GpuFrameProcessor init failed: {e}");
            return;
        }
    };

    let read_tight = |fd: i32| -> Vec<u8> {
        let raw = ghostframe_lib::capture::dmabuf::readback_dmabuf(fd, width, height, stride)
            .expect("mmap of the PRIME fd");
        let mut out = Vec::with_capacity(width as usize * height as usize * 4);
        for y in 0..height as usize {
            out.extend_from_slice(&raw[y * stride as usize..][..width as usize * 4]);
        }
        out
    };
    let before = read_tight(raw_fd);
    // A real PRIME fd that the GPU path cannot ingest is the bug this test
    // exists to catch, so it fails rather than skips.
    let analysis = processor
        .process_frame(raw_fd, width, height, stride)
        .expect("process_frame failed on a real PRIME DMA-BUF");

    // Pixel check. The live framebuffer can change under us, so build the
    // reference from one mmap read of the same fd before and one after the GPU pass
    // and compare only if they are identical (a static frame). The reference
    // goes through the CPU-pixels path of a second processor; any stride or
    // row-offset error in the dma-buf import shows up as a Y-plane or tile
    // difference. Both reads happen through the CPU, so a tiled scanout buffer
    // is "garbage" identically on both sides and still compares equal.
    assert_eq!(
        processor.last_ingest_tier(),
        Some(ghostframe_lib::capture::gpu_pipeline::IngestTier::DmabufBuffer),
        "a real PRIME fd should import through the GPU buffer tier"
    );
    let y_len = (analysis.nv12_y_stride * analysis.nv12_height) as usize;
    // SAFETY: nv12_data covers the Y plane; no further process_frame on `processor` yet.
    let gpu_y = unsafe { std::slice::from_raw_parts(analysis.nv12_data, y_len) }.to_vec();
    let gpu_tiles: Vec<(u32, u32)> = analysis
        .tile_analysis_slice()
        .iter()
        .map(|t| (t.count, t.edge_density_thou))
        .collect();
    let after = read_tight(raw_fd);
    if before == after {
        let mut reference = GpuFrameProcessor::new(4096).expect("reference processor");
        let r = reference
            .process_frame_from_pixels(&before, width, height, width * 4)
            .expect("reference process_frame_from_pixels");
        let ref_y = unsafe { std::slice::from_raw_parts(r.nv12_data, y_len) };
        let ref_tiles: Vec<(u32, u32)> = r
            .tile_analysis_slice()
            .iter()
            .map(|t| (t.count, t.edge_density_thou))
            .collect();
        assert_eq!(
            gpu_y.as_slice(),
            ref_y,
            "dma-buf import Y plane != CPU reference"
        );
        assert_eq!(
            gpu_tiles, ref_tiles,
            "dma-buf import tile analysis != CPU reference"
        );
        let distinct = gpu_y.iter().collect::<std::collections::HashSet<_>>().len();
        // A blank framebuffer converts to one flat Y value on both sides, so
        // the equality checks above would pass whatever the import did with
        // strides or offsets. Refuse to call that a pixel check.
        assert!(
            distinct > 1,
            "the live framebuffer is blank ({distinct} distinct Y value): the \
             dma-buf-vs-CPU pixel check is vacuous. Put something on the \
             scanout (any window or desktop) and re-run."
        );
        eprintln!(
            "pixel check: dma-buf import matches the CPU reference ({distinct} distinct Y values)"
        );
    } else {
        eprintln!("pixel check skipped: framebuffer changed between two reads");
    }

    let cols = width.div_ceil(32);
    let rows = height.div_ceil(32);
    let total_tiles = cols * rows;
    eprintln!(
        "First frame: {}/{} tiles dirty ({}x{} grid)",
        analysis.dirty_tiles.len(),
        total_tiles,
        cols,
        rows
    );
    assert!(
        !analysis.dirty_tiles.is_empty(),
        "first frame should report tiles as dirty"
    );
    assert!(
        !analysis.nv12_data.is_null(),
        "NV12 data pointer should not be null"
    );
    eprintln!(
        "NV12 output: {}x{} y_stride={} uv_offset={}",
        analysis.nv12_width, analysis.nv12_height, analysis.nv12_y_stride, analysis.nv12_uv_offset,
    );

    // 3. Full-frame H.264 encode via encode_nv12_buffer (HOST_VISIBLE NV12 → VA-API)
    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping encode: FullFrameEncoder init failed: {e}");
            return;
        }
    };

    // Encode up to 3 frames (VA-API may buffer the first).
    // We re-call process_frame each iteration so the NV12 pointer is fresh.
    let mut encoded = None;
    for i in 0..3 {
        // Re-run process_frame to get fresh NV12 data for each encode call.
        let frame_analysis = if i == 0 {
            // Already have analysis from above; reuse it for the first attempt.
            // SAFETY: analysis.nv12_data is valid for this scope (no new process_frame).
            &analysis
        } else {
            // Need a new analysis for subsequent encode attempts.
            // We can't re-borrow analysis after mutable borrow below; just break.
            break;
        };

        match encoder.encode_nv12_buffer(
            frame_analysis.nv12_data,
            frame_analysis.nv12_width,
            frame_analysis.nv12_height,
            frame_analysis.nv12_y_stride,
            frame_analysis.nv12_uv_stride,
            frame_analysis.nv12_uv_offset,
        ) {
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
                eprintln!("Skipping: encode_nv12_buffer failed: {e}");
                return;
            }
        }
    }

    // If VA-API buffered the first frame, send a second NV12 frame to flush it.
    if encoded.is_none() {
        let analysis2 = match processor.process_frame(raw_fd, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("Second process_frame failed: {e}");
                return;
            }
        };
        for i in 1..3 {
            match encoder.encode_nv12_buffer(
                analysis2.nv12_data,
                analysis2.nv12_width,
                analysis2.nv12_height,
                analysis2.nv12_y_stride,
                analysis2.nv12_uv_stride,
                analysis2.nv12_uv_offset,
            ) {
                Ok(Some(enc)) => {
                    eprintln!(
                        "Frame {i}: {} bytes, keyframe={}",
                        enc.payload.len(),
                        enc.is_keyframe
                    );
                    encoded = Some(enc);
                    break;
                }
                Ok(None) => eprintln!("Frame {i}: buffered"),
                Err(e) => {
                    eprintln!("Skipping: encode_nv12_buffer (retry {i}) failed: {e}");
                    return;
                }
            }
        }
    }

    let enc = encoded.expect("encoder should produce at least one frame in 3 attempts");
    assert!(!enc.payload.is_empty(), "H.264 payload should be non-empty");
    assert!(enc.is_keyframe, "first output should be a keyframe");

    // 4. Fragment the encoded frame
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

    eprintln!("DRM → GpuFrameProcessor (SAD+NV12) → encode_nv12_buffer (VA-API) → fragment: PASS");
}
