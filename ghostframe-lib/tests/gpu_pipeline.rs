//! Headless GPU integration test: DMA-BUF → SAD dirty detection + NV12 conversion → H.264 encode → fragment.
//!
//! Exercises the GPU pipeline without Docker, X11, or a browser.
//! Uses real dma-bufs (udmabuf, see `support/dmabuf_fixture.rs`). A plain
//! memfd is not a dma-buf, so these tests FAIL, never skip, when the GPU or
//! the dma-buf cannot be set up. Only a missing H.264 encoder skips. CI
//! excludes the `dmabuf_real_*` tests by name (no GPU/udmabuf there).
//!
//! Run: cargo test --test gpu_pipeline

#[path = "support/dmabuf_fixture.rs"]
mod dmabuf_fixture;

use dmabuf_fixture::{udmabuf_frame, udmabuf_solid, TestDmabuf};
use ghostframe_lib::capture::gpu_pipeline::{GpuFrameProcessor, IngestTier};
use ghostframe_lib::encoder::h264_vaapi::{FullFrameEncoded, FullFrameEncoder};
use ghostframe_lib::transport::protocol::{decode_frame_datagram, fragment_frame};

const NEEDS_GPU: &str =
    "GpuFrameProcessor::new failed: this test needs a Vulkan GPU and must not skip";

// ---------------------------------------------------------------------------
// dma-buf helpers (real dma-bufs via udmabuf; a memfd is NOT a dma-buf)
// ---------------------------------------------------------------------------

/// Real dma-buf filled with a solid BGRA color (alpha 255).
fn create_solid_dmabuf(width: u32, height: u32, b: u8, g: u8, r: u8) -> TestDmabuf {
    udmabuf_solid(width, height, [b, g, r, 255])
}

/// Color in BGRA layout (alpha is always 255).
struct BgraColor {
    b: u8,
    g: u8,
    r: u8,
}

/// Parameters for [`create_changed_dmabuf`].
struct ChangedDmabufParams {
    width: u32,
    height: u32,
    base: BgraColor,
    change_tile_x: u32,
    change_tile_y: u32,
    change: BgraColor,
}

/// Real dma-buf with one tile region changed to a different color.
fn create_changed_dmabuf(p: ChangedDmabufParams) -> TestDmabuf {
    // Tiles are 32x32 pixels.
    let (x0, y0) = (p.change_tile_x * 32, p.change_tile_y * 32);
    udmabuf_frame(p.width, p.height, p.width * 4, 0, |x, y| {
        if (x0..x0 + 32).contains(&x) && (y0..y0 + 32).contains(&y) {
            [p.change.b, p.change.g, p.change.r, 255]
        } else {
            [p.base.b, p.base.g, p.base.r, 255]
        }
    })
}

// ---------------------------------------------------------------------------
// Test 1: GPU dirty detection
// ---------------------------------------------------------------------------

/// Verify GpuFrameProcessor detects changes between two frames.
#[test]
fn dmabuf_real_gpu_pipeline_dirty_detection() {
    let mut tracker = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    let width = 128u32;
    let height = 128u32;
    let stride = width * 4;

    // Frame 1: solid blue
    let d1 = create_solid_dmabuf(width, height, 255, 0, 0); // B=255, G=0, R=0
    let first = tracker
        .diff(d1.raw(), width, height, stride)
        .expect("diff on a real dma-buf failed");
    assert_eq!(tracker.last_ingest_tier(), Some(IngestTier::DmabufBuffer));

    // First call: all tiles should be dirty (128/32 = 4 cols * 4 rows = 16 tiles)
    assert_eq!(
        first.len(),
        16,
        "first frame: all 16 tiles should be dirty, got: {first:?}"
    );

    // Frame 2: identical solid blue — no dirty tiles expected
    let d2 = create_solid_dmabuf(width, height, 255, 0, 0);
    let second = tracker
        .diff(d2.raw(), width, height, stride)
        .expect("second diff on a real dma-buf failed");

    assert_eq!(
        second.len(),
        0,
        "identical frames should produce no dirty tiles, got: {second:?}"
    );

    // Frame 3: tile (1,0) changed to red — only that tile should be dirty
    let d3 = create_changed_dmabuf(ChangedDmabufParams {
        width,
        height,
        base: BgraColor { b: 255, g: 0, r: 0 }, // blue
        change_tile_x: 1,
        change_tile_y: 0,
        change: BgraColor { b: 0, g: 0, r: 255 }, // red (B=0, G=0, R=255 in BGRA)
    });
    let third = tracker
        .diff(d3.raw(), width, height, stride)
        .expect("third diff on a real dma-buf failed");

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

// ---------------------------------------------------------------------------
// Test 2: Full-frame encode from a dma-buf
// ---------------------------------------------------------------------------

/// Verify FullFrameEncoder produces H.264 output from a real dma-buf.
/// Skips only if no encoder is available.
#[test]
fn dmabuf_real_gpu_pipeline_full_frame_encode() {
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

    // Solid green frame
    let d = create_solid_dmabuf(width, height, 0, 255, 0); // B=0, G=255, R=0

    // Encode — VA-API may buffer the first frame; try up to 3 frames.
    let mut got_output = false;
    let mut got_keyframe = false;
    for _ in 0..3 {
        match encoder
            .encode_frame(d.raw(), width, height, stride)
            .expect("encode_frame on a real dma-buf failed")
        {
            Some(encoded) => {
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
            None => continue,
        }
    }

    // Encode a second frame (same dma-buf is fine — static content)
    if got_output {
        match encoder.encode_frame(d.raw(), width, height, stride) {
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                eprintln!("Second encode_frame error (non-fatal): {e}");
            }
        }
    }

    assert!(
        got_output,
        "encoder should have produced output within 3 frames"
    );
    assert!(got_keyframe, "first output should be a keyframe");
}

// ---------------------------------------------------------------------------
// Test 3: Full pipeline end-to-end
// ---------------------------------------------------------------------------

/// Full pipeline: DMA-BUF → GPU process_frame (SAD + NV12) → encode_nv12_buffer → fragment.
/// This is the production path using HOST_VISIBLE NV12 output from the GPU compute shader.
#[test]
fn dmabuf_real_gpu_pipeline_end_to_end() {
    let width = 256u32;
    let height = 256u32;
    let stride = width * 4;

    // 1. Create both components. A missing GPU is a failure; a missing
    //    encoder is the only skip.
    let mut processor = GpuFrameProcessor::new(512).expect(NEEDS_GPU);

    let mut encoder = match FullFrameEncoder::new(width, height) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Skipping gpu_pipeline_end_to_end (no encoder): {e}");
            return;
        }
    };

    // 2. First frame: solid cyan
    let d1 = create_solid_dmabuf(width, height, 255, 255, 0); // B=255, G=255, R=0

    // 3. process_frame → dirty tiles + NV12 data
    let analysis = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !analysis.dirty_tiles.is_empty(),
        "first frame: some tiles must be dirty"
    );
    assert!(
        !analysis.nv12_data.is_null(),
        "NV12 data should not be null"
    );

    // 4. Encode via encode_nv12_buffer — try up to 3 attempts for VA-API buffering.
    let mut frame_output: Option<FullFrameEncoded> = None;
    for _ in 0..3 {
        match encoder
            .encode_nv12_buffer(
                analysis.nv12_data,
                analysis.nv12_width,
                analysis.nv12_height,
                analysis.nv12_y_stride,
                analysis.nv12_uv_stride,
                analysis.nv12_uv_offset,
            )
            .expect("encode_nv12_buffer failed")
        {
            Some(enc) => {
                frame_output = Some(enc);
                break;
            }
            None => continue,
        }
    }

    if let Some(enc) = frame_output {
        assert!(enc.is_keyframe, "first output frame should be a keyframe");
        assert!(!enc.payload.is_empty(), "encoded payload must not be empty");

        // Fragment it (MTU = 1200, max_fragment_payload = 1200 - 14 header bytes)
        let max_payload = 1200usize.saturating_sub(14);
        let datagrams = fragment_frame(1, 0, enc.is_keyframe, &enc.payload, max_payload);
        assert!(
            !datagrams.is_empty(),
            "fragmentation should produce at least one datagram"
        );

        // Verify we can decode the first datagram header back
        let (hdr, _payload) =
            decode_frame_datagram(&datagrams[0]).expect("first datagram should be decodable");
        assert_eq!(hdr.frame_seq, 1, "frame_seq should be 1");
        assert!(
            hdr.is_keyframe(),
            "first datagram should be marked keyframe"
        );
    }

    // 5. Second frame: same content → no dirty tiles expected
    let analysis2 = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("second process_frame failed");
    assert!(
        analysis2.dirty_tiles.is_empty(),
        "static second frame should have no dirty tiles, got: {:?}",
        analysis2.dirty_tiles
    );

    // 6. Third frame: change one tile → process_frame detects change → encode
    let d2 = create_changed_dmabuf(ChangedDmabufParams {
        width,
        height,
        base: BgraColor {
            b: 255,
            g: 255,
            r: 0,
        }, // cyan
        change_tile_x: 2,
        change_tile_y: 2,
        change: BgraColor { b: 0, g: 0, r: 255 }, // red (BGRA)
    });

    let analysis3 = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("third process_frame failed");
    assert!(
        !analysis3.dirty_tiles.is_empty(),
        "changed frame must have at least one dirty tile"
    );

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
}

// ---------------------------------------------------------------------------
// Test 4: NV12 conversion via process_frame
// ---------------------------------------------------------------------------

/// Verify that process_frame produces non-null NV12 data with correct layout.
/// Also checks that diff() still works as a wrapper around process_frame.
#[test]
fn dmabuf_real_gpu_pipeline_nv12_conversion() {
    let width = 128u32;
    let height = 128u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // Solid blue (BGRA: B=255, G=0, R=0, A=255)
    // Expected Y for pure blue (R=0,G=0,B=1.0): Y = 0.299*0 + 0.587*0 + 0.114*1 = 0.114 → ~29
    let d = create_solid_dmabuf(width, height, 255, 0, 0);

    let analysis = processor
        .process_frame(d.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));

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
    let y_slice =
        unsafe { std::slice::from_raw_parts(analysis.nv12_data, (width * height) as usize) };
    let y_sum: u64 = y_slice.iter().map(|&b| b as u64).sum();
    let y_avg = y_sum / y_slice.len() as u64;
    // BT.601 pure blue Y ≈ 29; allow generous tolerance for GPU rounding
    assert!(
        y_avg < 60,
        "Y for solid blue should be ~29 (low luminance), got avg {y_avg}"
    );

    // diff() wrapper should still work (returns only dirty tiles)
    let d2 = create_solid_dmabuf(width, height, 255, 0, 0); // same content
    let dirty = processor
        .diff(d2.raw(), width, height, stride)
        .expect("diff() wrapper failed on a real dma-buf");

    assert!(
        dirty.is_empty(),
        "identical frame via diff() should produce no dirty tiles, got: {dirty:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: tile_analysis on subsequent frame
// ---------------------------------------------------------------------------

/// Verify tile_analysis is populated on the first frame and again on the
/// second, with the expected solid-red contents.
#[test]
fn dmabuf_real_tile_analysis_populated_on_subsequent_frame() {
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // First frame: opaque red. tile_analysis runs even without a prev_image
    // (M3.2c), so it is already populated on the first frame.
    let d1 = create_solid_dmabuf(width, height, /*B=*/ 0, /*G=*/ 0, /*R=*/ 255);
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated (tile_analysis runs without a prev_image)"
    );
    assert_eq!(first.tile_analysis_len, 1, "32x32 frame → 1 tile");

    // Second frame: same opaque red. Now the analysis pipeline dispatches.
    let d2 = create_solid_dmabuf(width, height, 0, 0, 255);
    let second = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    assert!(
        !second.tile_analysis.is_null(),
        "second-frame analysis non-null"
    );
    assert_eq!(second.tile_analysis_len, 1, "32x32 frame → 1 tile");
    let slice = second.tile_analysis_slice();
    assert_eq!(slice.len(), 1);
    assert_eq!(slice[0].count, 1, "solid tile → count=1");
    assert_eq!(slice[0].colors[0], 0xFFFF0000u32);
    assert_eq!(slice[0].edge_density_thou, 0);
}
