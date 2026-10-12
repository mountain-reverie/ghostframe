use super::dmabuf_fixture::*;
use super::*;

const NEEDS_GPU: &str =
    "GpuFrameProcessor::new failed: this test needs a Vulkan GPU and must not skip";

#[test]
fn gpu_dirty_tracker_creates_successfully() {
    match GpuFrameProcessor::new(2048) {
        Ok(tracker) => {
            assert!(tracker.max_tiles > 0);
        }
        Err(e) => {
            eprintln!("Skipping GPU diff test (no Vulkan GPU?): {e}");
        }
    }
}

#[test]
fn dmabuf_real_identical_frames_produce_no_dirty_tiles() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    // Solid red pixel
    let pixel: [u8; 4] = [0, 0, 255, 255]; // BGRA: R

    let mut tracker = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // First call: establishes prev, returns all tiles dirty.
    let d1 = udmabuf_solid(width, height, pixel);
    let first = tracker
        .diff(d1.raw(), width, height, stride)
        .expect("diff on a real dma-buf failed");
    assert_eq!(tracker.last_ingest_tier(), Some(IngestTier::DmabufBuffer));

    // Expect all 4 tiles dirty (64/32 = 2 cols * 2 rows = 4 tiles)
    assert_eq!(first.len(), 4, "first frame should have all tiles dirty");

    // Second call with identical content: should have no dirty tiles
    let d2 = udmabuf_solid(width, height, pixel);
    let second = tracker
        .diff(d2.raw(), width, height, stride)
        .expect("second diff on a real dma-buf failed");

    assert_eq!(
        second.len(),
        0,
        "identical frames should produce no dirty tiles, got: {second:?}"
    );
}

#[test]
fn dmabuf_real_changed_pixel_detected() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;

    let mut tracker = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // Frame 1: all black.
    let black: [u8; 4] = [0, 0, 0, 255];
    let d1 = udmabuf_solid(width, height, black);
    let first = tracker
        .diff(d1.raw(), width, height, stride)
        .expect("diff on a real dma-buf failed");
    assert_eq!(tracker.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert_eq!(first.len(), 4, "first frame: all tiles dirty");

    // Frame 2: tile (1,0) changed to white (x=[32..63], y=[0..31]).
    let d2 = udmabuf_frame(width, height, stride, 0, |x, y| {
        if y < 32 && x >= 32 {
            [255, 255, 255, 255]
        } else {
            black
        }
    });
    let second = tracker
        .diff(d2.raw(), width, height, stride)
        .expect("second diff on a real dma-buf failed");

    // Only tile index 1 (col=1, row=0 → 0*2+1 = 1) should be dirty
    assert_eq!(
        second.len(),
        1,
        "only one tile should be dirty, got: {second:?}"
    );
    assert_eq!(second[0], 1, "dirty tile should be index 1 (col=1,row=0)");
}

/// Test that process_frame returns NV12 data with correct pointer and dimensions.
#[test]
fn dmabuf_real_process_frame_returns_nv12_data() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    // Solid red in BGRA: B=0, G=0, R=255, A=255
    let pixel: [u8; 4] = [0, 0, 255, 255];

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    let d = udmabuf_solid(width, height, pixel);
    let analysis = processor
        .process_frame(d.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));

    // Verify dimensions
    assert_eq!(analysis.nv12_width, width);
    assert_eq!(analysis.nv12_height, height);
    assert_eq!(analysis.nv12_y_stride, width);
    assert_eq!(analysis.nv12_uv_stride, width);
    assert_eq!(analysis.nv12_uv_offset, width * height);

    // First frame: all tiles dirty
    assert_eq!(analysis.dirty_tiles.len(), 4, "first frame: 4 tiles dirty");

    // Pointer must not be null
    assert!(
        !analysis.nv12_data.is_null(),
        "nv12_data pointer should not be null"
    );

    // Verify Y values for solid red (R=1.0, G=0, B=0):
    // Y = 0.299*1 + 0.587*0 + 0.114*0 = 0.299 → ~76
    let y_slice =
        unsafe { std::slice::from_raw_parts(analysis.nv12_data, (width * height) as usize) };
    let y_avg: u32 = y_slice.iter().map(|&b| b as u32).sum::<u32>() / y_slice.len() as u32;
    // Allow generous tolerance for GPU rounding differences
    assert!(
        y_avg > 50 && y_avg < 100,
        "Y for solid red should be ~76, got average {y_avg}"
    );
}

#[test]
fn tile_analysis_struct_has_expected_layout() {
    assert_eq!(
        std::mem::size_of::<TileAnalysis>(),
        80,
        "TileAnalysis must be 80 bytes"
    );
    assert_eq!(
        std::mem::align_of::<TileAnalysis>(),
        4,
        "TileAnalysis alignment"
    );

    // Offsets match std430 layout from the shader.
    let zero = TileAnalysis {
        count: 0,
        edge_density_thou: 0,
        _pad: [0; 2],
        colors: [0; 16],
    };
    let base = &zero as *const _ as usize;
    assert_eq!(&zero.count as *const _ as usize - base, 0);
    assert_eq!(&zero.edge_density_thou as *const _ as usize - base, 4);
    assert_eq!(&zero.colors as *const _ as usize - base, 16);
}

#[test]
fn frame_palette_entry_raw_has_expected_layout() {
    assert_eq!(
        std::mem::size_of::<FramePaletteEntryRaw>(),
        80,
        "FramePaletteEntryRaw must be 80 bytes (std430 layout)"
    );
    assert_eq!(
        std::mem::align_of::<FramePaletteEntryRaw>(),
        4,
        "FramePaletteEntryRaw alignment"
    );
    let zero = FramePaletteEntryRaw {
        count: 0,
        _pad: [0; 3],
        colors: [0; 16],
    };
    let base = &zero as *const _ as usize;
    assert_eq!(&zero.count as *const _ as usize - base, 0, "count offset");
    assert_eq!(&zero._pad as *const _ as usize - base, 4, "_pad offset");
    assert_eq!(
        &zero.colors as *const _ as usize - base,
        16,
        "colors offset"
    );
}

#[test]
fn frame_analysis_tile_analysis_slice_returns_correct_range() {
    // Build a fake FrameAnalysis backed by a heap Vec so we can exercise the
    // slice helper without spinning up Vulkan.
    let mut backing = vec![
        TileAnalysis {
            count: 1,
            edge_density_thou: 100,
            _pad: [0; 2],
            colors: [0xAAAAAAAAu32; 16],
        },
        TileAnalysis {
            count: 2,
            edge_density_thou: 200,
            _pad: [0; 2],
            colors: [0xBBBBBBBBu32; 16],
        },
    ];
    let analysis = FrameAnalysis {
        dirty_tiles: vec![],
        nv12_data: std::ptr::null(),
        nv12_width: 0,
        nv12_height: 0,
        nv12_y_stride: 0,
        nv12_uv_stride: 0,
        nv12_uv_offset: 0,
        tile_analysis: backing.as_mut_ptr() as *const TileAnalysis,
        tile_analysis_len: 2,
        palrle_compact_list: std::ptr::null(),
        palrle_compact_count: 0,
        frame_palette_set: std::ptr::null(),
        frame_palette_set_count: 0,
        per_tile_frame_palette_id: std::ptr::null(),
        folded_into: std::ptr::null(),
        index_buffer: std::ptr::null(),
    };
    let slice = analysis.tile_analysis_slice();
    assert_eq!(slice.len(), 2);
    assert_eq!(slice[0].count, 1);
    assert_eq!(slice[1].edge_density_thou, 200);
    assert_eq!(slice[1].colors[0], 0xBBBBBBBBu32);
}

#[test]
fn dmabuf_real_process_frame_returns_tile_analysis_for_solid_red() {
    // Solid red 32×32 → exactly one tile, count=1, edge_density_thou=0.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;
    // BGRA: B=0, G=0, R=255, A=255 → packed u32 = 0xFFFF0000
    let pixel: [u8; 4] = [0, 0, 255, 255];

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // First frame: tile_analysis runs even without a prev_image,
    // so the slice must be populated — unblocks static-content
    // classification (solid/PalRLE).
    let d1 = udmabuf_solid(width, height, pixel);
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 1);
    let first_slice = first.tile_analysis_slice();
    assert_eq!(first_slice.len(), 1);
    assert_eq!(first_slice[0].count, 1, "solid red tile → 1 unique color");
    assert_eq!(
        first_slice[0].colors[0], 0xFFFF0000u32,
        "BGRA(0,0,255,255) → 0xFFFF0000"
    );

    // Second frame: same content. Pipeline runs end-to-end.
    let d2 = udmabuf_solid(width, height, pixel);
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    assert!(
        !analysis.tile_analysis.is_null(),
        "tile_analysis pointer must not be null"
    );
    assert_eq!(analysis.tile_analysis_len, 1, "32x32 frame → 1 tile");
    let slice = analysis.tile_analysis_slice();
    assert_eq!(slice.len(), 1);
    assert_eq!(slice[0].count, 1, "solid tile → count=1");
    assert_eq!(
        slice[0].colors[0], 0xFFFF0000u32,
        "BGRA(0,0,255,255) → 0xFFFF0000"
    );
    assert_eq!(slice[0].edge_density_thou, 0, "solid tile → no edges");
}

#[test]
fn dmabuf_real_process_frame_tile_analysis_checkerboard() {
    // 32×32 frame with a 2×2-pixel-cell checkerboard: blue / white.
    //
    // The edge detector is a central difference on SUMMED RGB (see
    // tile_analysis.comp), so this fixture must satisfy two things the original
    // (1-px red/blue) one did not: neighbours at x-1 and x+1 must DIFFER (a
    // 1-px checker makes them equal, gradient 0), and the two colours must
    // differ in R+G+B (pure red and pure blue both sum to 255, gradient 0).
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    let make = || {
        udmabuf_frame(width, height, stride, 0, |x, y| {
            if ((x / 2) + (y / 2)) & 1 == 0 {
                [255, 0, 0, 255] // blue
            } else {
                [255, 255, 255, 255] // white
            }
        })
    };

    // First frame: tile_analysis runs (M3.2c). Seed prev_image and
    // verify first-frame slice is populated.
    let d1 = make();
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 1);

    // Second frame: same content. Now the analysis pipeline dispatches.
    let d2 = make();
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    let entry = &analysis.tile_analysis_slice()[0];
    assert_eq!(entry.count, 2, "checkerboard → 2 unique colors");

    // Both colors must appear; order is slot-traversal, not specified.
    let blue: u32 = 0xFF0000FF; // B=255, G=0, R=0, A=255 → 0xFF | 0 | 0 | 0xFF000000
    let white: u32 = 0xFFFFFFFF;
    assert!(
        (entry.colors[0] == blue || entry.colors[1] == blue)
            && (entry.colors[0] == white || entry.colors[1] == white),
        "expected both white and blue, got [{:#x}, {:#x}]",
        entry.colors[0],
        entry.colors[1]
    );

    // Most pixels border a different color on at least one cardinal axis;
    // edge pixels (clamped) reduce density slightly. Lower bound is
    // generous to absorb edge effects.
    assert!(
        entry.edge_density_thou > 700,
        "checkerboard → edge_density_thou should be high, got {}",
        entry.edge_density_thou
    );
}

#[test]
fn dmabuf_real_process_frame_tile_analysis_overflow_at_17_colors() {
    // Place 17 distinct colors spread across the tile. Expect count=17 sentinel.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    let colors: [[u8; 4]; 17] = [
        [10, 20, 30, 255],
        [40, 50, 60, 255],
        [70, 80, 90, 255],
        [100, 110, 120, 255],
        [130, 140, 150, 255],
        [160, 170, 180, 255],
        [190, 200, 210, 255],
        [220, 230, 240, 255],
        [5, 15, 25, 255],
        [35, 45, 55, 255],
        [65, 75, 85, 255],
        [95, 105, 115, 255],
        [125, 135, 145, 255],
        [155, 165, 175, 255],
        [185, 195, 205, 255],
        [215, 225, 235, 255],
        [245, 250, 254, 255],
    ];

    let make = || {
        // 17 distinct BGRA colors. Background = color 0.
        let mut frame = vec![0u8; (stride * height) as usize];
        for chunk in frame.chunks_exact_mut(4) {
            chunk.copy_from_slice(&colors[0]);
        }
        // Place each of the 17 colors at distinct pixel positions.
        for (i, c) in colors.iter().enumerate() {
            let x = (i as u32 * 7) % width; // stride 7 keeps placements spread out
            let y = (i as u32 * 11) % height;
            let off = ((y * stride) + x * 4) as usize;
            frame[off..off + 4].copy_from_slice(c);
        }
        udmabuf_from_tight(&frame, width, height, stride, 0)
    };

    // First frame: tile_analysis runs (M3.2c). Seed prev_image and
    // confirm slice is populated.
    let d1 = make();
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 1);

    // Second frame: same content. Now the analysis pipeline dispatches.
    let d2 = make();
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    let entry = &analysis.tile_analysis_slice()[0];
    assert_eq!(entry.count, 17, "17 distinct colors → overflow sentinel");
    // colors[] is undefined per contract — do not assert on it.
}

#[test]
fn dmabuf_real_process_frame_tile_analysis_frame_edge_denominator() {
    // 40×40 frame → 2x2 tile grid:
    //   tile (0,0): 32×32 full
    //   tile (1,0): 8×32 right-edge partial (256 pixels in-bounds)
    //   tile (0,1): 32×8 bottom-edge partial (256 pixels in-bounds)
    //   tile (1,1): 8×8 corner partial (64 pixels in-bounds)
    //
    // Fill everything red. Inject ONE off-color pixel into tile (1,0) at a
    // non-edge interior position. Gradient triggers there.
    //
    // Expected edge_density_thou for tile (1,0):
    //   The off-color pixel and ~2 immediate neighbors register gradient.
    //   With denom=256 and ~3 gradient-pixels, density ≈ 11. With a naive
    //   denom=1024 it'd be ≈ 2. Assertion is >5 (real) vs <5 (would catch
    //   bad denominator).
    let width = 40u32;
    let height = 40u32;
    let stride = width * 4;
    let _cols = 2u32; // 40px / 32px/tile = 2 tiles per row; used in index comment

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    let make = || {
        udmabuf_frame(width, height, stride, 0, |x, y| {
            // Inject one white pixel at (x=35, y=10) — interior of tile (1,0):
            // tile (1,0) covers x=32..40, y=0..32; (35,10) is in-bounds and not on
            // the frame edge.
            if x == 35 && y == 10 {
                // Must differ from red in R+G+B: the detector sums RGB, and
                // pure green (255) equals pure red (255), so green is invisible.
                [255, 255, 255, 255] // white BGRA
            } else {
                [0, 0, 255, 255] // red BGRA
            }
        })
    };

    // First frame: tile_analysis runs (M3.2c). Seed prev_image and
    // confirm slice is populated.
    let d1 = make();
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 4, "40x40 → 2x2 tile grid");

    // Second frame: same content. Now the analysis pipeline dispatches.
    let d2 = make();
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    let slice = analysis.tile_analysis_slice();
    assert_eq!(analysis.tile_analysis_len, 4, "40x40 → 2x2 tile grid");

    // Tile (1,0): row 0, col 1 of a `cols`-wide grid; index = 0*cols + 1 = 1.
    let tile_10_idx = 1_usize;
    let edge_tile = &slice[tile_10_idx];
    assert_eq!(edge_tile.count, 2, "tile (1,0) has red + white");
    // With proper denominator=256: density ≈ (~3 * 1000) / 256 ≈ 11
    // With naive denominator=1024: density ≈ (~3 * 1000) / 1024 ≈ 2
    assert!(
        edge_tile.edge_density_thou > 5,
        "frame-edge tile denominator wrong: density = {} (expected > 5 with denom=256)",
        edge_tile.edge_density_thou
    );
}

#[test]
fn dmabuf_real_process_frame_tile_analysis_xor_mask_handles_zero_bgra() {
    // BGRA = 0x00000000 (transparent black) — must round-trip through the
    // hash set despite raw 0 being the "empty slot" sentinel. The XOR mask
    // means 0x00000000 is stored as 0xA5A5A5A5 in the slot.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 0, 0];

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // First frame: tile_analysis runs (M3.2c). Seed prev_image and
    // confirm slice is populated.
    let d1 = udmabuf_solid(width, height, pixel);
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 1);

    // Second frame: same content. Now the analysis pipeline dispatches.
    let d2 = udmabuf_solid(width, height, pixel);
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    let entry = &analysis.tile_analysis_slice()[0];
    assert_eq!(entry.count, 1, "transparent-black tile → count=1");
    assert_eq!(
        entry.colors[0], 0x00000000u32,
        "BGRA(0,0,0,0) survives mask"
    );
    assert_eq!(entry.edge_density_thou, 0);
}

#[test]
fn dmabuf_real_process_frame_tile_analysis_multi_tile_independence() {
    // 64×64 frame → 2x2 tile grid:
    //   tile (0,0): solid red          → count=1
    //   tile (1,0): solid blue         → count=1
    //   tile (0,1): red-blue checker   → count=2
    //   tile (1,1): three colors       → count=3
    // Verifies the per-tile output slot is correctly addressed and there
    // is no cross-talk between tiles.
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    let red: [u8; 4] = [0, 0, 255, 255];
    let blue: [u8; 4] = [255, 0, 0, 255];
    let green: [u8; 4] = [0, 255, 0, 255];
    let yellow: [u8; 4] = [0, 255, 255, 255];

    let make = || {
        udmabuf_frame(width, height, stride, 0, |x, y| {
            match (x >= 32, y >= 32) {
                // tile (0,0): red
                (false, false) => red,
                // tile (1,0): blue
                (true, false) => blue,
                // tile (0,1): red/blue checker
                (false, true) => {
                    if (x + y) & 1 == 0 {
                        red
                    } else {
                        blue
                    }
                }
                // tile (1,1): three colors striped vertically (red/green/yellow)
                (true, true) => match (x - 32) / 11 {
                    0 => red,
                    1 => green,
                    _ => yellow,
                },
            }
        })
    };

    // First frame: tile_analysis runs (M3.2c). Seed prev_image and
    // confirm slice is populated.
    let d1 = make();
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 4);

    // Second frame: same content. Now the analysis pipeline dispatches.
    let d2 = make();
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    let slice = analysis.tile_analysis_slice();
    assert_eq!(analysis.tile_analysis_len, 4);
    // cols = 2; index = y * 2 + x
    assert_eq!(slice[0].count, 1, "tile (0,0) red");
    assert_eq!(slice[1].count, 1, "tile (1,0) blue");
    assert_eq!(slice[2].count, 2, "tile (0,1) checker");
    assert_eq!(slice[3].count, 3, "tile (1,1) three stripes");
}

#[test]
fn dmabuf_real_tile_analysis_colors_are_canonical_sorted() {
    // 64×32 frame → 2 tiles side by side (tile 0 left, tile 1 right).
    // Each tile has exactly 3 colors: red, green, blue.
    // The colors are placed in *different spatial order* in each tile to
    // verify the sort output is the same regardless of hash-table order.
    //
    // BGRA packed-u32 ascending order:
    //   blue  [255,0,0,255]   = 0xFF00_00FF
    //   green [0,255,0,255]   = 0xFF00_FF00
    //   red   [0,0,255,255]   = 0xFFFF_0000
    let width = 64u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // BGRA byte order: [B, G, R, A]
    let red: [u8; 4] = [0, 0, 255, 255];
    let green: [u8; 4] = [0, 255, 0, 255];
    let blue: [u8; 4] = [255, 0, 0, 255];

    // Seed frame stays zeroed; the real frame has the striped tiles.
    let make = |fill: bool| {
        udmabuf_frame(width, height, stride, 0, |x, _y| {
            if !fill {
                return [0, 0, 0, 0];
            }
            let lx = x % 32;
            if x < 32 {
                // Tile 0: red(x<11) | green(x<22) | blue(rest).
                if lx < 11 {
                    red
                } else if lx < 22 {
                    green
                } else {
                    blue
                }
            } else {
                // Tile 1: blue(x<11) | red(x<22) | green(rest).
                if lx < 11 {
                    blue
                } else if lx < 22 {
                    red
                } else {
                    green
                }
            }
        })
    };

    // First frame: tile_analysis runs (M3.2c). Seed frame is zeroed,
    // but the slice must still be populated.
    let d1 = make(false);
    let first = processor
        .process_frame(d1.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 2, "64x32 frame → 2 tiles");

    // Second frame: real content — triggers tile_analysis dispatch.
    let d2 = make(true);
    let analysis = processor
        .process_frame(d2.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    let slice = analysis.tile_analysis_slice();
    assert_eq!(slice.len(), 2, "64x32 frame → 2 tiles");

    // BGRA packed-u32 ascending order:
    //   blue  (B=255,G=0,R=0,A=255) = 0xFF00_00FF
    //   green (B=0,G=255,R=0,A=255) = 0xFF00_FF00
    //   red   (B=0,G=0,R=255,A=255) = 0xFFFF_0000
    let blue_p: u32 = 0xFF00_00FF;
    let green_p: u32 = 0xFF00_FF00;
    let red_p: u32 = 0xFFFF_0000;

    for (tile_i, entry) in slice.iter().enumerate() {
        assert_eq!(entry.count, 3, "tile {}: expected count 3", tile_i);
        assert_eq!(
            entry.colors[0], blue_p,
            "tile {}: slot 0 should be blue",
            tile_i
        );
        assert_eq!(
            entry.colors[1], green_p,
            "tile {}: slot 1 should be green",
            tile_i
        );
        assert_eq!(
            entry.colors[2], red_p,
            "tile {}: slot 2 should be red",
            tile_i
        );
    }
}

/// Verify that `palrle_compact_list` contains exactly the tiles that are
/// both SAD-dirty and have a feasible palette (count 1..=16).
///
/// Frame layout (96×32, 3 tiles of 32×32 each):
///   tile 0 (x 0..31):   solid red  → count=1, feasible
///   tile 1 (x 32..63):  4-color quadrant text → count=4, feasible
///   tile 2 (x 64..95):  photographic gradient → count>16, NOT feasible
///
/// Two-call pattern: seed frame (all zeros) then real frame.
#[test]
fn dmabuf_real_process_frame_returns_palrle_compact_list_for_text_tiles() {
    let width = 96u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // --- Build the seed frame (all zeros) ---
    let d_seed = udmabuf_solid(width, height, [0, 0, 0, 0]);
    let first = processor
        .process_frame(d_seed.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    // First frame: tile_analysis and compact_list are both populated
    //. The seed frame is all-black: 3 tiles each with
    // count=1 (solid black), all PalRLE-feasible.
    assert!(
        !first.tile_analysis.is_null(),
        "first-frame analysis must be populated"
    );
    assert_eq!(first.tile_analysis_len, 3, "96x32 → 3 tiles");
    assert!(
        !first.palrle_compact_list.is_null(),
        "first-frame compact list is now populated"
    );

    // --- Build the real frame ---
    let d_real = udmabuf_frame(width, height, stride, 0, |x, y| {
        if x < 32 {
            // tile 0 (x 0..31): solid red (BGRA: B=0,G=0,R=255,A=255)
            [0, 0, 255, 255]
        } else if x < 64 {
            // tile 1 (x 32..63): 4 colors in quadrants
            if y < 16 && x < 48 {
                [0u8, 0, 0, 255]
            } else if y < 16 {
                [255, 255, 255, 255]
            } else if x < 48 {
                [128, 128, 128, 255]
            } else {
                [200, 200, 200, 255]
            }
        } else {
            // tile 2 (x 64..95): gradient (many colors)
            let local_x = (x - 64) as u8;
            [local_x, y as u8, local_x.wrapping_add(y as u8), 255]
        }
    });

    let analysis = processor
        .process_frame(d_real.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    // Sanity-check that tile_analysis ran.
    assert!(
        !analysis.tile_analysis.is_null(),
        "second-frame tile_analysis must not be null"
    );
    assert_eq!(analysis.tile_analysis_len, 3, "96x32 → 3 tiles");

    // Compact list must be non-null.
    assert!(
        !analysis.palrle_compact_list.is_null(),
        "palrle_compact_list must not be null on second frame"
    );

    let list = analysis.palrle_compact_list_slice();

    // tile 0 (count=1) and tile 1 (count=4) must appear.
    assert!(
        list.contains(&0),
        "tile 0 (solid red, count=1) should be in compact list; got {list:?}"
    );
    assert!(
        list.contains(&1),
        "tile 1 (4-color, count=4) should be in compact list; got {list:?}"
    );
    // tile 2 has count > 16 → must NOT appear.
    assert!(
        !list.contains(&2),
        "tile 2 (gradient, count>16) must NOT be in compact list; got {list:?}"
    );
    assert_eq!(
        analysis.palrle_compact_count, 2,
        "only 2 tiles are PalRLE-feasible; got count={}",
        analysis.palrle_compact_count
    );
}

/// Stage 2a: 4 tiles each with identical 2-color palette {black, white}.
/// Expected: 4 compact entries, 1 unique frame palette, all per_tile_id == 0.
#[test]
fn dmabuf_real_process_frame_dedups_identical_palettes_within_frame() {
    let width = 128u32;
    let height = 32u32;
    let stride = width * 4;

    let mut proc = GpuFrameProcessor::new(128).expect(NEEDS_GPU);

    // --- Seed frame: all zeros ---
    let d_seed = udmabuf_solid(width, height, [0, 0, 0, 0]);
    let first = proc
        .process_frame(d_seed.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(proc.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    // First frame: frame_palette_set is now populated.
    assert!(
        !first.frame_palette_set.is_null(),
        "first-frame frame_palette_set is now populated"
    );

    // --- Real frame: 4 tiles (32x32 each), each with identical 2-color
    // checkerboard {black, white} ---
    let d_real = udmabuf_frame(width, height, stride, 0, |x, y| {
        // Tile-local x equals x % 32, so parity matches the original layout.
        if ((x % 32) + y) % 2 == 0 {
            [0u8, 0, 0, 255] // black BGRA
        } else {
            [255u8, 255, 255, 255] // white BGRA
        }
    });

    let analysis = proc
        .process_frame(d_real.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    // 4 tiles, all feasible (count=2 each).
    assert_eq!(
        analysis.palrle_compact_count, 4,
        "expected 4 compact tiles; got {}",
        analysis.palrle_compact_count
    );

    // Stage 2a: all 4 tiles share the same palette → 1 unique frame palette.
    // `frame_palette_set_count` is NOT that number: it is the high-water mark of
    // claimed hash slots (slot = fnv1a & 0xFF, see palette_fold.comp and the
    // doc on `frame_palette_set_slice`), so count the distinct ids instead.
    let ids = analysis.per_tile_frame_palette_id_slice();
    assert_eq!(ids.len(), 4, "per_tile_id length must equal compact_count");
    let distinct: std::collections::BTreeSet<u8> = ids.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        1,
        "4 identical-palette tiles → 1 unique frame palette; got ids {ids:?}"
    );
    // The slot is a hash, not 0; it must be a real (non-sentinel) slot that the
    // high-water mark covers.
    let id = ids[0];
    assert_ne!(id, 0xFF, "palette slot must not be the overflow sentinel");
    assert!(
        analysis.frame_palette_set_count > id as u32,
        "high-water mark {} must cover claimed slot {id}",
        analysis.frame_palette_set_count
    );
}

/// Stage 2b: two tiles where tile 0 has palette {black, white, grey} (3 colors)
/// and tile 1 has palette {black, white, grey, blue/red} (4 colors — superset of tile 0).
/// After dispatch, tile 0's folded_into entry should resolve to tile 1's palette slot.
#[test]
fn dmabuf_real_process_frame_folds_subset_palette() {
    let width = 64u32;
    let height = 32u32;
    let stride = width * 4;

    let mut proc = GpuFrameProcessor::new(64).expect(NEEDS_GPU);

    // Seed frame: all zeros (to initialize prev_image).
    let d_seed = udmabuf_solid(width, height, [0, 0, 0, 0]);
    let first = proc
        .process_frame(d_seed.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(proc.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    // First frame: folded_into is now populated.
    assert!(
        !first.folded_into.is_null(),
        "first-frame folded_into is now populated"
    );

    // Real frame:
    //   tile 0 (x=0..32): palette {black, white, grey} (3 colors)
    //   tile 1 (x=32..64): palette {black, white, grey, red/blue} (4 colors)
    // Tile 0's palette is a strict subset of tile 1's palette.
    let d_real = udmabuf_frame(width, height, stride, 0, |x, y| {
        if x < 32 {
            // tile 0: 3-color cycling pattern
            match (x + y * 32) % 3 {
                0 => [0, 0, 0, 255],       // black BGRA
                1 => [255, 255, 255, 255], // white BGRA
                _ => [128, 128, 128, 255], // grey BGRA
            }
        } else {
            // tile 1: 4-color cycling pattern (superset of tile 0)
            match (x + y * 32) % 4 {
                0 => [0, 0, 0, 255],       // black BGRA
                1 => [255, 255, 255, 255], // white BGRA
                2 => [128, 128, 128, 255], // grey BGRA
                _ => [255, 0, 0, 255],     // blue BGRA (B=255,G=0,R=0)
            }
        }
    });
    let analysis = proc
        .process_frame(d_real.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    // Both tiles must be PalRLE-feasible (count <= 16).
    assert_eq!(
        analysis.palrle_compact_count, 2,
        "expected 2 compact tiles; got {}",
        analysis.palrle_compact_count
    );

    let folded = analysis.folded_into_slice();
    assert_eq!(folded.len(), 256, "folded_into must have 256 entries");

    let ids = analysis.per_tile_frame_palette_id_slice();
    assert_eq!(ids.len(), 2, "per_tile_id length must equal compact_count");

    let id0 = ids[0];
    let id1 = ids[1];

    // Stage 2a: 2 distinct palettes. (`frame_palette_set_count` is a hash-slot
    // high-water mark, not a unique count; compare the ids.)
    assert_ne!(
        id0, id1,
        "tile 0 (3-color) and tile 1 (4-color) → 2 unique frame palettes; got ids {ids:?}"
    );
    assert!(
        analysis.frame_palette_set_count > id0.max(id1) as u32,
        "high-water mark {} must cover claimed slots {ids:?}",
        analysis.frame_palette_set_count
    );

    // Determine which compact slot is the subset vs the superset via
    // palette count. Workgroup execution order is non-deterministic.
    let palettes = analysis.frame_palette_set_slice();
    let (subset_id, superset_id) = if palettes[id0 as usize].count < palettes[id1 as usize].count {
        (id0, id1)
    } else {
        (id1, id0)
    };

    let resolved_for_subset = folded[subset_id as usize] & 0xFFu32;
    assert_eq!(
        resolved_for_subset as u8, superset_id,
        "subset palette {} should fold into superset {}",
        subset_id, superset_id
    );
    let resolved_for_superset = folded[superset_id as usize] & 0xFFu32;
    assert_eq!(
        resolved_for_superset as u8, superset_id,
        "superset palette {} should stay self-referential",
        superset_id
    );
}

/// Stage 3 GPU integration test: 32×32 tile with top half black (index 0)
/// and bottom half white (index 1). After dispatch, the 512 index bytes
/// should encode the half-and-half pattern as packed 4-bit nibbles.
///
/// Pixel layout: 32×32 = 1024 pixels. Each byte holds two 4-bit indices
/// (low nibble = even pixel, high nibble = odd pixel). First 512 pixels
/// (top half) → index 0; next 512 pixels (bottom half) → index 1.
/// Bytes 0..=255: low=0, high=0 (pixels 0..=511 all black → index 0)
/// Bytes 256..=511: low=1, high=1 (pixels 512..=1023 all white → index 1)
#[test]
fn dmabuf_real_process_frame_emits_correct_indices_for_two_color_tile() {
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut proc = GpuFrameProcessor::new(32).expect(NEEDS_GPU);

    // Seed frame: all zeros (to initialize prev_image).
    let d_seed = udmabuf_solid(width, height, [0, 0, 0, 0]);
    let first = proc
        .process_frame(d_seed.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(proc.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    // First frame: index_buffer is now populated.
    assert!(
        !first.index_buffer.is_null(),
        "first-frame index_buffer is now populated"
    );

    // Real frame: top half black (y<16), bottom half white (y>=16).
    let d_real = udmabuf_frame(width, height, stride, 0, |_x, y| {
        if y < 16 {
            [0, 0, 0, 255] // BGRA black
        } else {
            [255, 255, 255, 255] // BGRA white
        }
    });
    let analysis = proc
        .process_frame(d_real.raw(), width, height, stride)
        .expect("second process_frame on a real dma-buf failed");

    // The 32×32 tile has exactly 2 colors → palrle_compact_count == 1.
    assert_eq!(
        analysis.palrle_compact_count, 1,
        "32×32 two-color tile: compact_count should be 1, got {}",
        analysis.palrle_compact_count
    );
    assert!(
        !analysis.index_buffer.is_null(),
        "index_buffer must not be null after Stage 3 dispatch"
    );

    let indices = analysis.index_buffer_slice_for(0);
    assert_eq!(
        indices.len(),
        512,
        "index_buffer_slice_for must return 512 bytes"
    );

    // 32×32 = 1024 pixels total, packed 2 per byte (low nibble first).
    // Top half: pixels 0..511 → all black → index 0 → both nibbles 0.
    // Bottom half: pixels 512..1023 → all white → index 1 → both nibbles 1.
    // Bytes 0..=255 correspond to pixel pairs 0..=511 → index 0.
    // Bytes 256..=511 correspond to pixel pairs 512..=1023 → index 1.
    for (byte_idx, &val) in indices[0..256].iter().enumerate() {
        let low = val & 0x0F;
        let high = (val >> 4) & 0x0F;
        assert_eq!(
            low,
            0,
            "byte {} low nibble: pixel {} should be index 0 (black)",
            byte_idx,
            byte_idx * 2
        );
        assert_eq!(
            high,
            0,
            "byte {} high nibble: pixel {} should be index 0 (black)",
            byte_idx,
            byte_idx * 2 + 1
        );
    }
    for (i, &val) in indices[256..512].iter().enumerate() {
        let byte_idx = i + 256;
        let low = val & 0x0F;
        let high = (val >> 4) & 0x0F;
        assert_eq!(
            low,
            1,
            "byte {} low nibble: pixel {} should be index 1 (white)",
            byte_idx,
            byte_idx * 2
        );
        assert_eq!(
            high,
            1,
            "byte {} high nibble: pixel {} should be index 1 (white)",
            byte_idx,
            byte_idx * 2 + 1
        );
    }
}

#[test]
fn dmabuf_real_invalidate_baseline_drops_prev_image_and_sets_counter() {
    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // Counter starts at 0; prev_image starts None.
    assert_eq!(processor.force_all_dirty_remaining, 0);
    assert!(processor.prev_image.is_none());

    // Drive one frame to populate prev_image.
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 255, 255];
    let d = udmabuf_solid(width, height, pixel);
    processor
        .diff(d.raw(), width, height, stride)
        .expect("diff on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert!(
        processor.prev_image.is_some(),
        "diff should populate prev_image"
    );

    // The API under test.
    processor.invalidate_baseline(20);

    assert!(
        processor.prev_image.is_none(),
        "invalidate_baseline must drop prev_image"
    );
    assert_eq!(processor.force_all_dirty_remaining, 20);
}

#[test]
fn dmabuf_real_invalidate_baseline_cushion_keeps_all_dirty_and_prev_image_none() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 255, 255];
    let tile_count = (width / 32) * (height / 32); // 2 * 2 = 4

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // Prime: first frame populates prev_image and reports all dirty.
    let d = udmabuf_solid(width, height, pixel);
    let dirty = processor
        .diff(d.raw(), width, height, stride)
        .expect("diff on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));
    assert_eq!(dirty.len() as u32, tile_count, "first frame: all dirty");
    assert!(
        processor.prev_image.is_some(),
        "first frame populates prev_image"
    );

    // Sanity: second identical frame produces 0 dirty.
    let d = udmabuf_solid(width, height, pixel);
    let dirty = processor.diff(d.raw(), width, height, stride).unwrap();
    assert_eq!(dirty.len(), 0, "identical content: SAD reports 0 dirty");

    // Engage the cushion.
    processor.invalidate_baseline(3);
    assert!(processor.prev_image.is_none());

    // Cushion frames 1..=3: all dirty, prev_image stays None.
    for i in 1..=3 {
        let d = udmabuf_solid(width, height, pixel);
        let dirty = processor.diff(d.raw(), width, height, stride).unwrap();
        assert_eq!(
            dirty.len() as u32,
            tile_count,
            "cushion frame {i}: should be all dirty"
        );
        assert!(
            processor.prev_image.is_none(),
            "cushion frame {i}: prev_image must stay None during cushion"
        );
    }
    assert_eq!(processor.force_all_dirty_remaining, 0);

    // Frame 4: counter is 0, prev_image is still None → first-frame-with-snapshot.
    // All dirty AND prev_image becomes Some.
    let d = udmabuf_solid(width, height, pixel);
    let dirty = processor.diff(d.raw(), width, height, stride).unwrap();
    assert_eq!(
        dirty.len() as u32,
        tile_count,
        "post-cushion first commit frame: all dirty"
    );
    assert!(
        processor.prev_image.is_some(),
        "post-cushion first commit frame: prev_image must be Some"
    );

    // Frame 5: normal SAD against the just-committed snapshot.
    let d = udmabuf_solid(width, height, pixel);
    let dirty = processor.diff(d.raw(), width, height, stride).unwrap();
    assert_eq!(
        dirty.len(),
        0,
        "normal SAD resumed: 0 dirty for unchanged content"
    );
}

#[test]
fn dmabuf_real_cdf53_compact_filters_high_color_tiles_only() {
    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // 64×64 frame = 4 tiles (2×2 at 32×32 tile size).
    // Top-left tile (0,0) = gradient (high-color); others = solid blue (≤2 colors).
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let off = ((y * stride) + x * 4) as usize;
            let in_gradient = (x < 32) && (y < 32);
            if in_gradient {
                // Dense gradient: up to 32*32 = 1024 unique colors — well above 16.
                pixels[off] = (x * 3) as u8;
                pixels[off + 1] = (y * 3) as u8;
                pixels[off + 2] = (x + y) as u8;
            } else {
                // Solid blue — 1 unique color per tile.
                pixels[off] = 0;
                pixels[off + 1] = 0;
                pixels[off + 2] = 0xFF;
            }
            pixels[off + 3] = 0xFF;
        }
    }

    let d = udmabuf_from_tight(&pixels, width, height, stride, 0);
    processor
        .process_frame(d.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));

    let count = unsafe { *processor.cdf53_compact_count_ptr };
    assert_eq!(
        count, 1,
        "expected exactly 1 high-color tile in compact list; got {count}"
    );
    let list =
        unsafe { std::slice::from_raw_parts(processor.cdf53_compact_list_ptr, count as usize) };
    assert_eq!(
        list[0], 0,
        "expected tile index 0 (top-left gradient); got {}",
        list[0]
    );
}

#[test]
fn dmabuf_real_cdf53_forward_gpu_matches_cpu_reference() {
    use crate::encoder::cdf53;

    let mut processor = GpuFrameProcessor::new(256).expect(NEEDS_GPU);

    // 64×64 frame: tile (0,0) has high-color gradient (→ Cdf53 path),
    // other tiles have solid/low-color content.
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let off = ((y * stride) + x * 4) as usize;
            if x < 32 && y < 32 {
                // High-color gradient tile — will be classified Cdf53.
                pixels[off] = (x * 3 % 256) as u8; // B
                pixels[off + 1] = (y * 3 % 256) as u8; // G
                pixels[off + 2] = ((x + y) % 256) as u8; // R
            } else {
                // Solid blue on other tiles — PalRLE or Solid, not Cdf53.
                pixels[off] = 0xFF;
                pixels[off + 1] = 0;
                pixels[off + 2] = 0;
            }
            pixels[off + 3] = 0xFF; // A
        }
    }

    // Compute CPU reference for the gradient tile (tile index 0, col=0, row=0).
    let mut tile_bgra = vec![0u8; 32 * 32 * 4];
    for ty in 0..32usize {
        for tx in 0..32usize {
            let src = (ty * stride as usize) + tx * 4;
            let dst = (ty * 32 + tx) * 4;
            tile_bgra[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    let cpu_coefficients = cdf53::forward(&tile_bgra);

    // Run GPU pipeline.
    let d = udmabuf_from_tight(&pixels, width, height, stride, 0);
    processor
        .process_frame(d.raw(), width, height, stride)
        .expect("process_frame on a real dma-buf failed");
    assert_eq!(processor.last_ingest_tier(), Some(IngestTier::DmabufBuffer));

    // Verify cdf53_compact has exactly 1 tile (the gradient tile at index 0).
    let compact_count = unsafe { *processor.cdf53_compact_count_ptr };
    assert_ne!(
        compact_count, 0,
        "cdf53_compact_count=0: the gradient tile was not classified Cdf53"
    );

    // Read GPU coefficients for compact slot 0.
    // The coefficient buffer stores i32 per coefficient; Phase B casts to i16.
    let gpu_coeffs: Vec<i16> = unsafe {
        let raw =
            std::slice::from_raw_parts(processor.cdf53_coefficients_ptr, cdf53::CDF53_TOTAL_COEFFS);
        raw.iter().map(|&v| v as i16).collect()
    };

    // Compare exact: integer CDF 5/3 lifting must match the CPU reference.
    for (i, (&gpu, &cpu)) in gpu_coeffs.iter().zip(cpu_coefficients.iter()).enumerate() {
        if gpu != cpu {
            let channel = i / cdf53::CDF53_COEFFS_PER_CHANNEL;
            let in_channel = i % cdf53::CDF53_COEFFS_PER_CHANNEL;
            panic!(
                "coefficient mismatch at i={i} (channel={channel}, in_channel={in_channel}): \
                 GPU={gpu} vs CPU={cpu}"
            );
        }
    }
}

/// Same coefficient-equivalence assertion as
/// `cdf53_forward_gpu_matches_cpu_reference`, but drives the GPU pipeline
/// via `process_frame_from_pixels` (the CPU-upload path that goes through
/// `import_pixels`) instead of `process_frame` (the DMA-BUF path).
///
/// **Why this exists separately:** the live X11-on-AMD deployment runs
/// the daemon with `GHOSTFRAME_X11_CAPTURE_ONLY=1`, which routes
/// `FrameSubmission.pixels` (a `Vec<u8>` of BGRA bytes from XGetImage)
/// straight into `process_frame_from_pixels`. None of the previous
/// gradient/lossless e2e tests exercise that path — they all use
/// `--drm-direct` which goes through the DMA-BUF import. A symptom seen
/// in the field (canvas pitch-black despite the X-side capture having
/// the correct pixels and the encoder firing) matched the shape of an
/// `import_pixels` upload silently filling the staging image with
/// zeros, which the existing tests would not catch.
///
/// Test acts as a regression net: if the BGRA bytes ever stop reaching
/// the encoder's input image correctly (e.g. wrong memory type for the
/// host-coherent mapping, row-pitch mismatch, missing layout transition),
/// the GPU coefficients diverge from the CPU reference and the test
/// fails loudly here instead of going dark on a remote install.
#[test]
fn cdf53_forward_from_pixels_matches_cpu_reference() {
    use crate::encoder::cdf53;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping cdf53_forward_from_pixels equivalence test (no Vulkan?): {e}");
            return;
        }
    };

    // Identical 64×64 frame layout to the DMA-BUF sibling: tile (0,0) is a
    // high-color gradient (classifier → Cdf53), other tiles are solid blue.
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let mut pixels = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let off = ((y * stride) + x * 4) as usize;
            if x < 32 && y < 32 {
                pixels[off] = (x * 3 % 256) as u8; // B
                pixels[off + 1] = (y * 3 % 256) as u8; // G
                pixels[off + 2] = ((x + y) % 256) as u8; // R
            } else {
                pixels[off] = 0xFF;
                pixels[off + 1] = 0;
                pixels[off + 2] = 0;
            }
            pixels[off + 3] = 0xFF;
        }
    }

    // CPU reference for tile (0,0).
    let mut tile_bgra = vec![0u8; 32 * 32 * 4];
    for ty in 0..32usize {
        for tx in 0..32usize {
            let src = (ty * stride as usize) + tx * 4;
            let dst = (ty * 32 + tx) * 4;
            tile_bgra[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    let cpu_coefficients = cdf53::forward(&tile_bgra);

    // Run GPU pipeline via the CPU-upload path. `process_frame_from_pixels`
    // calls `import_pixels` under the hood — this is the exact code path
    // exercised by an X11-capture-only deployment.
    match processor.process_frame_from_pixels(&pixels, width, height, stride) {
        Ok(_) => {}
        Err(e) => panic!(
            "process_frame_from_pixels failed: {e} \
             (this is the bug we're trying to catch — should NOT silently \
             skip; if Vulkan was unavailable, processor creation above \
             would already have returned)"
        ),
    }

    let compact_count = unsafe { *processor.cdf53_compact_count_ptr };
    assert!(
        compact_count > 0,
        "cdf53_compact_count=0 — the gradient tile was not classified as \
         Cdf53. Most common cause: the staging image's pixels look like \
         constant content to the classifier (i.e. `import_pixels` \
         delivered zeros). On the DMA-BUF sibling this skips the test; \
         here it MUST fail, because uploading the same gradient via the \
         CPU path should produce the same classification."
    );

    let gpu_coeffs: Vec<i16> = unsafe {
        let raw =
            std::slice::from_raw_parts(processor.cdf53_coefficients_ptr, cdf53::CDF53_TOTAL_COEFFS);
        raw.iter().map(|&v| v as i16).collect()
    };

    // First-failure diagnostic: report up to 10 mismatches with their
    // positions so a regression yields actionable output, not just the
    // first divergence. Crucial because an `import_pixels` bug typically
    // produces ALL-coefficients-wrong, and seeing a few examples makes
    // the failure mode obvious (e.g. all coeffs = the DC term of zeros
    // → upload delivered all-black).
    let mut mismatches = Vec::new();
    for (i, (&gpu, &cpu)) in gpu_coeffs.iter().zip(cpu_coefficients.iter()).enumerate() {
        if gpu != cpu {
            let channel = i / cdf53::CDF53_COEFFS_PER_CHANNEL;
            let in_channel = i % cdf53::CDF53_COEFFS_PER_CHANNEL;
            mismatches.push(format!(
                "coeff[{i}] (channel={channel}, idx_in_channel={in_channel}): \
                 GPU={gpu} vs CPU={cpu}"
            ));
            if mismatches.len() >= 10 {
                break;
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "GPU coefficients (via process_frame_from_pixels) diverge from \
         CPU reference. First {} mismatch(es):\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );
}

/// Live-deployment scaling of `cdf53_forward_from_pixels_matches_cpu_reference`:
/// runs the CPU-upload path at 1920×1080 with max_tiles=2040, the actual
/// frame shape the X11-capture daemon sends. Catches bugs that only
/// manifest at production scale — e.g. row-pitch padding mismatches
/// the small test doesn't exercise, max_tiles-dependent staging-image
/// layouts, AMD's compute alignment requirements at large extents.
///
/// Puts the gradient (the only thing the classifier scores as Cdf53)
/// at tile (0, 0); the rest of the frame is solid blue, dominated by
/// PalRle/Solid classification. We still read back coefficients for
/// the gradient tile via the compact list and compare to the CPU
/// reference exactly.
#[test]
fn cdf53_forward_from_pixels_live_dims_matches_cpu_reference() {
    use crate::encoder::cdf53;

    // 2040 = ⌈1920/32⌉ × ⌈1080/32⌉ — what the live processor is built with.
    let mut processor = match GpuFrameProcessor::new(2040) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping cdf53_forward_from_pixels_live_dims test (no Vulkan?): {e}");
            return;
        }
    };

    let width = 1920u32;
    let height = 1080u32;
    let stride = width * 4;
    // 8 294 400 bytes — same shape as the X11 capture's BGRA payload on
    // the live install (verified against `/tmp/dump.bgra`'s file size).
    let mut pixels = vec![0u8; (stride * height) as usize];
    for y in 0..height {
        for x in 0..width {
            let off = ((y * stride) + x * 4) as usize;
            if x < 32 && y < 32 {
                pixels[off] = (x * 3 % 256) as u8;
                pixels[off + 1] = (y * 3 % 256) as u8;
                pixels[off + 2] = ((x + y) % 256) as u8;
            } else {
                pixels[off] = 0xFF;
                pixels[off + 1] = 0;
                pixels[off + 2] = 0;
            }
            pixels[off + 3] = 0xFF;
        }
    }

    let mut tile_bgra = vec![0u8; 32 * 32 * 4];
    for ty in 0..32usize {
        for tx in 0..32usize {
            let src = (ty * stride as usize) + tx * 4;
            let dst = (ty * 32 + tx) * 4;
            tile_bgra[dst..dst + 4].copy_from_slice(&pixels[src..src + 4]);
        }
    }
    let cpu_coefficients = cdf53::forward(&tile_bgra);

    match processor.process_frame_from_pixels(&pixels, width, height, stride) {
        Ok(_) => {}
        Err(e) => panic!("process_frame_from_pixels failed at live dims: {e}"),
    }

    let compact_count = unsafe { *processor.cdf53_compact_count_ptr };
    assert!(
        compact_count > 0,
        "cdf53_compact_count=0 at 1920×1080 — the gradient tile at (0,0) \
         was not classified as Cdf53. Probable cause: import_pixels at \
         the live-deployment row-pitch left the staging image zeroed."
    );

    let gpu_coeffs: Vec<i16> = unsafe {
        let raw =
            std::slice::from_raw_parts(processor.cdf53_coefficients_ptr, cdf53::CDF53_TOTAL_COEFFS);
        raw.iter().map(|&v| v as i16).collect()
    };

    let mut mismatches = Vec::new();
    for (i, (&gpu, &cpu)) in gpu_coeffs.iter().zip(cpu_coefficients.iter()).enumerate() {
        if gpu != cpu {
            let channel = i / cdf53::CDF53_COEFFS_PER_CHANNEL;
            let in_channel = i % cdf53::CDF53_COEFFS_PER_CHANNEL;
            mismatches.push(format!(
                "coeff[{i}] (channel={channel}, idx_in_channel={in_channel}): \
                 GPU={gpu} vs CPU={cpu}"
            ));
            if mismatches.len() >= 10 {
                break;
            }
        }
    }
    assert!(
        mismatches.is_empty(),
        "GPU coefficients at 1920×1080 diverge from CPU reference. First \
         {} mismatch(es):\n  {}",
        mismatches.len(),
        mismatches.join("\n  ")
    );
}

// ---------------------------------------------------------------------------
// Golden characterization test
// ---------------------------------------------------------------------------
//
// Pins the exact byte outputs of the GPU frame pipeline for a deterministic
// scripted sequence driven through `process_frame_from_pixels` (the path that
// actually runs on hosts where a memfd is not accepted as a DMA-BUF). It is a
// safety net for zero-behaviour-change refactors of `frame.rs`: every defined
// output surface is hashed after every frame and compared with constants that
// were captured from the pre-refactor code.
//
// To regenerate the table, run `cargo test -p ghostframe-lib --lib
// capture::gpu_pipeline::tests::golden -- --nocapture`: a mismatching run
// prints the observed rows between GOLDEN_TABLE_BEGIN/END; paste them into
// `GOLDEN_EXPECTED`. Only DEFINED regions are hashed: stale slots
// (index_buffer slots >= palrle_compact_count, palette slots not referenced by
// a compact tile, coefficient slots >= the compact count) are skipped, and
// every surface whose order depends on GPU atomic-append order (the compact
// lists) is canonicalised by sorting on the flat tile index.

struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 ^= x as u64;
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    fn i32s(&mut self, v: &[i32]) {
        for &x in v {
            self.bytes(&x.to_le_bytes());
        }
    }
}

fn golden_mix(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Fixed-seed LCG (no rand crate).
struct GoldenLcg(u64);
impl GoldenLcg {
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u32
    }
}

#[derive(Clone, Copy)]
enum GoldenKind {
    Solid(u32),
    /// Text-like palette tile: pixels drawn from `colors` in 2x2 blocks.
    Pal(&'static [u32], u64),
    Grad(u32),
    Noise,
}

const GC_WHITE: u32 = 0xFFFF_FFFF;
const GC_BLACK: u32 = 0xFF00_0000;
const GC_RED: u32 = 0xFF00_00C8;
const GC_BLUE: u32 = 0xFFC8_0000;
const GC_GREEN: u32 = 0xFF00_C800;
const GC_GREY: u32 = 0xFF80_8080;
static GP2: [u32; 2] = [GC_WHITE, GC_BLACK];
static GP3: [u32; 3] = [GC_WHITE, GC_BLACK, GC_RED];
static GP4: [u32; 4] = [GC_WHITE, GC_BLACK, GC_RED, GC_BLUE];
static GP3B: [u32; 3] = [GC_BLUE, GC_GREEN, GC_GREY];
static GP9: [u32; 9] = [
    GC_WHITE,
    GC_BLACK,
    GC_RED,
    GC_BLUE,
    GC_GREEN,
    GC_GREY,
    0xFF10_2030,
    0xFF30_2010,
    0xFF55_AA55,
];

/// Tile content for the scripted frames. `ver` selects the script revision;
/// `cols` makes the layout meaningful at both frame sizes.
fn golden_kind(tx: u32, ty: u32, cols: u32, ver: u32) -> GoldenKind {
    let idx = ty * cols + tx;
    let base = match idx {
        0 => GoldenKind::Grad(0),
        1 => GoldenKind::Solid(GC_WHITE),
        2 => GoldenKind::Pal(&GP2, 11),
        3 => GoldenKind::Pal(&GP3, 12),
        4 => GoldenKind::Pal(&GP4, 13),
        5 => GoldenKind::Pal(&GP2, 14),
        6 => GoldenKind::Noise,
        7 => GoldenKind::Grad(50),
        8 => GoldenKind::Solid(GC_BLUE),
        9 => GoldenKind::Pal(&GP3B, 15),
        10 => GoldenKind::Pal(&GP9, 16),
        _ => GoldenKind::Solid(0xFF00_0000 | ((idx * 0x0007_0509) & 0x00FF_FFFF)),
    };
    // Modifications relative to the base layout.
    match (ver, idx) {
        (0, _) => base,
        (1, 1) => GoldenKind::Pal(&GP2, 21),
        (1, 3) => GoldenKind::Pal(&GP4, 22),
        (1, 5) => GoldenKind::Pal(&GP3, 23),
        (1, 7) => GoldenKind::Grad(90),
        (1, 12) => GoldenKind::Pal(&GP9, 24),
        (1, 20) => GoldenKind::Solid(GC_GREY),
        (1, 23) => GoldenKind::Grad(200),
        (1, 34 | 18) => GoldenKind::Pal(&GP2, 25),
        (2, 0) => GoldenKind::Grad(33),
        (2, 2) => GoldenKind::Pal(&GP3, 31),
        (2, 6) => GoldenKind::Noise,
        (2, 15) => GoldenKind::Pal(&GP4, 32),
        _ => base,
    }
}

fn golden_render(w: u32, h: u32, ver: u32) -> Vec<u8> {
    let cols = w.div_ceil(32);
    let mut px = vec![0u8; (w * h * 4) as usize];
    let mut lcg = GoldenLcg(0x5eed_0000_0000_0000 ^ ((w as u64) << 16) ^ h as u64);
    for y in 0..h {
        for x in 0..w {
            let (tx, ty, lx, ly) = (x / 32, y / 32, x % 32, y % 32);
            let col: u32 = match golden_kind(tx, ty, cols, ver) {
                GoldenKind::Solid(c) => c,
                GoldenKind::Pal(cs, seed) => {
                    let r = golden_mix(
                        seed ^ ((tx as u64) << 40)
                            ^ ((ty as u64) << 32)
                            ^ (((lx / 2) as u64) << 8)
                            ^ ((ly / 2) as u64),
                    );
                    cs[(r % cs.len() as u64) as usize]
                }
                GoldenKind::Grad(s) => {
                    0xFF00_0000
                        | ((lx + ly + s) % 256) << 16
                        | ((ly * 3) % 256) << 8
                        | ((lx * 3 + s) % 256)
                }
                GoldenKind::Noise => {
                    let v = lcg.next();
                    0xFF00_0000 | (v & 0x00FF_FFFF)
                }
            };
            let off = ((y * w + x) * 4) as usize;
            px[off..off + 4].copy_from_slice(&col.to_le_bytes());
        }
    }
    px
}

unsafe fn golden_read_args(p: &GpuFrameProcessor, mem: vk::DeviceMemory) -> [u32; 3] {
    let ptr = p
        .device
        .map_memory(mem, 0, 12, vk::MemoryMapFlags::empty())
        .expect("map indirect-args memory") as *const u32;
    let range = vk::MappedMemoryRange::default()
        .memory(mem)
        .offset(0)
        .size(vk::WHOLE_SIZE);
    p.device.invalidate_mapped_memory_ranges(&[range]).ok();
    let out = [*ptr, *ptr.add(1), *ptr.add(2)];
    p.device.unmap_memory(mem);
    out
}

/// Hash every defined output of the last `process_frame_from_pixels` call.
/// Returns `(surface, hash)` rows plus a few counters for the non-vacuity
/// check.
unsafe fn golden_surfaces(
    p: &GpuFrameProcessor,
    a: &FrameAnalysis,
    w: u32,
    h: u32,
    esc_count: usize,
    stats: &mut GoldenStats,
) -> Vec<(&'static str, u64)> {
    let mut rows: Vec<(&'static str, u64)> = Vec::new();
    let cols = w.div_ceil(32);
    let tiles = (cols * h.div_ceil(32)) as usize;

    // dirty_tiles
    let mut f = Fnv::new();
    f.u32(a.dirty_tiles.len() as u32);
    for &t in &a.dirty_tiles {
        f.u32(t);
    }
    rows.push(("dirty", f.0));
    stats.dirty += a.dirty_tiles.len();

    // NV12: Y rows (w bytes each) then UV rows (2*ceil(w/2) bytes each).
    let mut f = Fnv::new();
    for y in 0..h as usize {
        f.bytes(std::slice::from_raw_parts(
            a.nv12_data.add(y * a.nv12_y_stride as usize),
            w as usize,
        ));
    }
    rows.push(("nv12_y", f.0));
    let mut f = Fnv::new();
    for y in 0..h.div_ceil(2) as usize {
        f.bytes(std::slice::from_raw_parts(
            a.nv12_data
                .add(a.nv12_uv_offset as usize + y * a.nv12_uv_stride as usize),
            (w.div_ceil(2) * 2) as usize,
        ));
    }
    rows.push(("nv12_uv", f.0));

    // tile_analysis (colors only valid for count 1..=16).
    let ta = a.tile_analysis_slice();
    assert_eq!(ta.len(), tiles);
    let mut f = Fnv::new();
    for t in ta {
        f.u32(t.count);
        f.u32(t.edge_density_thou);
        if (1..=16).contains(&t.count) {
            for &c in &t.colors[..t.count as usize] {
                f.u32(c);
            }
        }
        stats.analysed_colors += t.count as usize;
    }
    rows.push(("tile_analysis", f.0));

    // PalRLE compact + palette set + folded_into + index buffer, sorted by tile.
    let n = a.palrle_compact_count as usize;
    stats.palrle_max = stats.palrle_max.max(n);
    let list = a.palrle_compact_list_slice();
    let pids = a.per_tile_frame_palette_id_slice();
    let pset = a.frame_palette_set_slice();
    let folded = a.folded_into_slice();
    assert_eq!(list.len(), n);
    assert_eq!(pids.len(), n);
    let pal_hash = |f: &mut Fnv, id: usize| {
        let e = &pset[id];
        f.u32(e.count);
        for &c in &e.colors[..(e.count as usize).min(16)] {
            f.u32(c);
        }
    };
    let mut order: Vec<usize> = (0..n).collect();
    order.sort_by_key(|&c| list[c]);
    let mut f_list = Fnv::new();
    let mut f_pal = Fnv::new();
    let mut f_fold = Fnv::new();
    let mut f_idx = Fnv::new();
    f_list.u32(n as u32);
    f_pal.u32(a.frame_palette_set_count);
    for &c in &order {
        let tile = list[c];
        f_list.u32(tile);
        f_pal.u32(tile);
        pal_hash(&mut f_pal, pids[c] as usize);
        let fold_id = (folded[pids[c] as usize] & 0xFF) as usize;
        f_fold.u32(tile);
        f_fold.u32(folded[pids[c] as usize] >> 8);
        f_fold.u32((fold_id != pids[c] as usize) as u32);
        pal_hash(&mut f_fold, fold_id);
        if fold_id != pids[c] as usize {
            stats.folds += 1;
        }
        f_idx.u32(tile);
        f_idx.bytes(a.index_buffer_slice_for(c));
    }
    rows.push(("palrle_list", f_list.0));
    rows.push(("palette_set", f_pal.0));
    rows.push(("folded_into", f_fold.0));
    rows.push(("index_buffer", f_idx.0));

    // CDF 5/3 forward (L1+L2+L3) for the compact tiles, sorted by tile.
    let cn = *p.cdf53_compact_count_ptr as usize;
    stats.cdf_max = stats.cdf_max.max(cn);
    let clist = std::slice::from_raw_parts(p.cdf53_compact_list_ptr, cn);
    let mut corder: Vec<usize> = (0..cn).collect();
    corder.sort_by_key(|&c| clist[c]);
    let mut f_cl = Fnv::new();
    let mut f_cc = Fnv::new();
    f_cl.u32(cn as u32);
    for &c in &corder {
        f_cl.u32(clist[c]);
        f_cc.u32(clist[c]);
        let co = std::slice::from_raw_parts(p.cdf53_coefficients_ptr.add(c * 3072), 3072);
        if co.iter().any(|&v| v != 0) {
            stats.cdf_nonzero_tiles += 1;
        }
        f_cc.i32s(co);
    }
    rows.push(("cdf53_list", f_cl.0));
    rows.push(("cdf53_coeffs", f_cc.0));

    // Escalation forward pass (candidate order is CPU-defined, so raw order).
    let mut f = Fnv::new();
    f.u32(esc_count as u32);
    if esc_count > 0 {
        let co = std::slice::from_raw_parts(p.cdf53_escalation_coefficients_ptr, esc_count * 3072);
        if co.iter().any(|&v| v != 0) {
            stats.esc_nonzero += 1;
        }
        f.i32s(co);
    }
    rows.push(("escalation", f.0));

    // Indirect dispatch arguments written by the two *_indirect_args shaders.
    let mut f = Fnv::new();
    for v in golden_read_args(p, p.palrle_indirect_args_buf.memory) {
        f.u32(v);
    }
    for v in golden_read_args(p, p.cdf53_dispatch_args.memory) {
        f.u32(v);
    }
    rows.push(("indirect_args", f.0));
    rows
}

#[derive(Default)]
struct GoldenStats {
    dirty: usize,
    analysed_colors: usize,
    palrle_max: usize,
    folds: usize,
    cdf_max: usize,
    cdf_nonzero_tiles: usize,
    esc_nonzero: usize,
}

/// `(frame, surface, fnv1a64)` captured from the unmodified pipeline.
const GOLDEN_EXPECTED: &[(usize, &str, u64)] = &[
    (0, "dirty", 0x1fbd886ecf874ea5),
    (0, "nv12_y", 0x24f2470444498a7f),
    (0, "nv12_uv", 0x423e9028983ab29a),
    (0, "tile_analysis", 0x16818eaa08c2599d),
    (0, "palrle_list", 0x17db082b36de5477),
    (0, "palette_set", 0xb954be5aee491cb1),
    (0, "folded_into", 0xd2c24be1f366bc71),
    (0, "index_buffer", 0x6e409381c03a249f),
    (0, "cdf53_list", 0x7d183f858df0e7d7),
    (0, "cdf53_coeffs", 0xee49e08b3800f02e),
    (0, "escalation", 0x8a00d4cb41533a7f),
    (0, "indirect_args", 0x8df210e64d643c96),
    (1, "dirty", 0x4d25767f9dce13f5),
    (1, "nv12_y", 0x24f2470444498a7f),
    (1, "nv12_uv", 0x423e9028983ab29a),
    (1, "tile_analysis", 0x16818eaa08c2599d),
    (1, "palrle_list", 0x4d25767f9dce13f5),
    (1, "palette_set", 0x4d25767f9dce13f5),
    (1, "folded_into", 0xcbf29ce484222325),
    (1, "index_buffer", 0xcbf29ce484222325),
    (1, "cdf53_list", 0x4d25767f9dce13f5),
    (1, "cdf53_coeffs", 0xcbf29ce484222325),
    (1, "escalation", 0x4d25767f9dce13f5),
    (1, "indirect_args", 0x6fa37ebc366034e5),
    (2, "dirty", 0x1ff8f72c544488b3),
    (2, "nv12_y", 0x82ab1602c67f7b63),
    (2, "nv12_uv", 0x41dfe811a83af449),
    (2, "tile_analysis", 0x3a4037f02a40b784),
    (2, "palrle_list", 0x6807a5070ed3b4cd),
    (2, "palette_set", 0xd0d95b98821c08b1),
    (2, "folded_into", 0x4ace64933a4683b6),
    (2, "index_buffer", 0xe43745c26e188452),
    (2, "cdf53_list", 0xc6433a907e3e2df7),
    (2, "cdf53_coeffs", 0xeaf3c8d6d5c822c2),
    (2, "escalation", 0x1715fcc7dac20932),
    (2, "indirect_args", 0x3582fdfddc3de0a0),
    (3, "dirty", 0x4d25767f9dce13f5),
    (3, "nv12_y", 0x82ab1602c67f7b63),
    (3, "nv12_uv", 0x41dfe811a83af449),
    (3, "tile_analysis", 0x3a4037f02a40b784),
    (3, "palrle_list", 0x4d25767f9dce13f5),
    (3, "palette_set", 0x4d25767f9dce13f5),
    (3, "folded_into", 0xcbf29ce484222325),
    (3, "index_buffer", 0xcbf29ce484222325),
    (3, "cdf53_list", 0x4d25767f9dce13f5),
    (3, "cdf53_coeffs", 0xcbf29ce484222325),
    (3, "escalation", 0xa6e941a0ec6bb586),
    (3, "indirect_args", 0x6fa37ebc366034e5),
    (4, "dirty", 0x1fbd886ecf874ea5),
    (4, "nv12_y", 0x8f932450acfcca1a),
    (4, "nv12_uv", 0x7dc97916e172d80c),
    (4, "tile_analysis", 0xc2d90ea20009311b),
    (4, "palrle_list", 0x17db082b36de5477),
    (4, "palette_set", 0x86de5e095e93cbf7),
    (4, "folded_into", 0xa0f87e7569508665),
    (4, "index_buffer", 0xc1b1a0401286e97b),
    (4, "cdf53_list", 0x7d183f858df0e7d7),
    (4, "cdf53_coeffs", 0x5ab2ab9919fb0fee),
    (4, "escalation", 0xb69f9634eb15b371),
    (4, "indirect_args", 0x8df210e64d643c96),
    (5, "dirty", 0x1fbd886ecf874ea5),
    (5, "nv12_y", 0x8f932450acfcca1a),
    (5, "nv12_uv", 0x7dc97916e172d80c),
    (5, "tile_analysis", 0xc2d90ea20009311b),
    (5, "palrle_list", 0x17db082b36de5477),
    (5, "palette_set", 0x86de5e095e93cbf7),
    (5, "folded_into", 0xa0f87e7569508665),
    (5, "index_buffer", 0xc1b1a0401286e97b),
    (5, "cdf53_list", 0x7d183f858df0e7d7),
    (5, "cdf53_coeffs", 0x5ab2ab9919fb0fee),
    (5, "escalation", 0x4d25767f9dce13f5),
    (5, "indirect_args", 0x8df210e64d643c96),
    (6, "dirty", 0x1fbd886ecf874ea5),
    (6, "nv12_y", 0x8f932450acfcca1a),
    (6, "nv12_uv", 0x7dc97916e172d80c),
    (6, "tile_analysis", 0xc2d90ea20009311b),
    (6, "palrle_list", 0x17db082b36de5477),
    (6, "palette_set", 0x86de5e095e93cbf7),
    (6, "folded_into", 0xa0f87e7569508665),
    (6, "index_buffer", 0xc1b1a0401286e97b),
    (6, "cdf53_list", 0x7d183f858df0e7d7),
    (6, "cdf53_coeffs", 0x5ab2ab9919fb0fee),
    (6, "escalation", 0xf6ccfe9dcf8b9e4f),
    (6, "indirect_args", 0x8df210e64d643c96),
    (7, "dirty", 0xfb3dbba6c0a52cab),
    (7, "nv12_y", 0x82ab1602c67f7b63),
    (7, "nv12_uv", 0x41dfe811a83af449),
    (7, "tile_analysis", 0x3a4037f02a40b784),
    (7, "palrle_list", 0xb985458588aec3ee),
    (7, "palette_set", 0x83c52a18f31abb94),
    (7, "folded_into", 0x844be3727db0d6bb),
    (7, "index_buffer", 0xc8519dc04141749f),
    (7, "cdf53_list", 0x5c72edf88918efc6),
    (7, "cdf53_coeffs", 0x76d2cb63f90362d0),
    (7, "escalation", 0x70eadc68f2818f23),
    (7, "indirect_args", 0x8460f6739ef508bf),
    (8, "dirty", 0xd714d758fe1693cd),
    (8, "nv12_y", 0x44dbc2870e5b60f5),
    (8, "nv12_uv", 0xacf8430ffdaf489c),
    (8, "tile_analysis", 0x5a43dd969c383d8d),
    (8, "palrle_list", 0x753aca7fd1e1d381),
    (8, "palette_set", 0x5e2a06d866358bb7),
    (8, "folded_into", 0x2274cef655c982a8),
    (8, "index_buffer", 0xafffc5cbd0d15a70),
    (8, "cdf53_list", 0x7d183f858df0e7d7),
    (8, "cdf53_coeffs", 0x0cedc4a74a231b31),
    (8, "escalation", 0x5921fc86d680a393),
    (8, "indirect_args", 0x1eff9b62949cb243),
    (9, "dirty", 0xc7a3dfd7f80248f0),
    (9, "nv12_y", 0x7369b8fe727e9bc5),
    (9, "nv12_uv", 0xc60c35a986695217),
    (9, "tile_analysis", 0x3b8910e57f50668b),
    (9, "palrle_list", 0xd16d91085891afce),
    (9, "palette_set", 0x6a44f14e5f1eee08),
    (9, "folded_into", 0x4f2a976eab7c74ec),
    (9, "index_buffer", 0x2678fc675d6dca50),
    (9, "cdf53_list", 0xc6433a907e3e2df7),
    (9, "cdf53_coeffs", 0xeaf3c8d6d5c822c2),
    (9, "escalation", 0xd81f8f53fee936ac),
    (9, "indirect_args", 0x37cd250405efeec1),
    (10, "dirty", 0x4d25767f9dce13f5),
    (10, "nv12_y", 0x7369b8fe727e9bc5),
    (10, "nv12_uv", 0xc60c35a986695217),
    (10, "tile_analysis", 0x3b8910e57f50668b),
    (10, "palrle_list", 0x4d25767f9dce13f5),
    (10, "palette_set", 0x4d25767f9dce13f5),
    (10, "folded_into", 0xcbf29ce484222325),
    (10, "index_buffer", 0xcbf29ce484222325),
    (10, "cdf53_list", 0x4d25767f9dce13f5),
    (10, "cdf53_coeffs", 0xcbf29ce484222325),
    (10, "escalation", 0x4d25767f9dce13f5),
    (10, "indirect_args", 0x6fa37ebc366034e5),
    (11, "dirty", 0x1fbd886ecf874ea5),
    (11, "nv12_y", 0x8f932450acfcca1a),
    (11, "nv12_uv", 0x7dc97916e172d80c),
    (11, "tile_analysis", 0xc2d90ea20009311b),
    (11, "palrle_list", 0x17db082b36de5477),
    (11, "palette_set", 0x86de5e095e93cbf7),
    (11, "folded_into", 0xa0f87e7569508665),
    (11, "index_buffer", 0xc1b1a0401286e97b),
    (11, "cdf53_list", 0x7d183f858df0e7d7),
    (11, "cdf53_coeffs", 0x5ab2ab9919fb0fee),
    (11, "escalation", 0x1d2759b0f18c6fb3),
    (11, "indirect_args", 0x8df210e64d643c96),
];

#[test]
fn golden_from_pixels_sequence() {
    // The `cdf53-diag` skip variables (GHOSTFRAME_CDF53_SKIP_*) change the
    // coefficients, so a run with one set fails below as a golden mismatch.
    // They are not checked here: this crate reads the environment only in
    // config.rs, and CI enforces that on test code too.
    let mut p = match GpuFrameProcessor::new(64) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping golden_from_pixels_sequence (no Vulkan?): {e}");
            return;
        }
    };

    // (width, height, script version, invalidate_baseline(n) before, escalation candidates)
    struct Step {
        w: u32,
        h: u32,
        ver: u32,
        invalidate: u32,
        esc: &'static [u32],
    }
    let s = |w, h, ver, invalidate, esc| Step {
        w,
        h,
        ver,
        invalidate,
        esc,
    };
    let script = [
        s(200, 136, 0, 0, &[0, 7, 3]), // 0: first-frame path (+ escalation)
        s(200, 136, 0, 0, &[]),        // 1: identical -> nothing dirty
        s(200, 136, 1, 0, &[1, 5, 17, 34]), // 2: a few changed tiles
        s(200, 136, 1, 0, &[0]),       // 3: identical again
        s(200, 136, 2, 2, &[7, 23]),   // 4: no_commit cushion 1/2
        s(200, 136, 2, 0, &[]),        // 5: no_commit cushion 2/2
        s(200, 136, 2, 0, &[2]),       // 6: first real snapshot
        s(200, 136, 1, 0, &[0, 6]),    // 7: steady, changes vs 6
        s(192, 128, 0, 0, &[0, 7]),    // 8: resolution change (tile aligned)
        s(192, 128, 1, 0, &[1]),       // 9: steady at aligned size
        s(192, 128, 1, 0, &[]),        // 10: identical
        s(200, 136, 2, 0, &[3, 4, 5]), // 11: resolution change back
    ];

    let mut stats = GoldenStats::default();
    let mut got: Vec<(usize, &'static str, u64)> = Vec::new();
    for (i, st) in script.iter().enumerate() {
        if st.invalidate > 0 {
            p.invalidate_baseline(st.invalidate);
        }
        let px = golden_render(st.w, st.h, st.ver);
        p.set_escalation_candidates(st.esc);
        let a = match p.process_frame_from_pixels(&px, st.w, st.h, st.w * 4) {
            Ok(a) => a,
            Err(e) => panic!("golden frame {i}: process_frame_from_pixels failed: {e}"),
        };
        let rows = unsafe { golden_surfaces(&p, &a, st.w, st.h, st.esc.len(), &mut stats) };
        for (name, h) in rows {
            got.push((i, name, h));
        }
    }

    // Non-vacuity: the script must actually exercise the stages.
    eprintln!(
        "golden stats: dirty={} colors={} palrle_max={} folds={} cdf_max={} cdf_nonzero_tiles={} esc_nonzero={}",
        stats.dirty,
        stats.analysed_colors,
        stats.palrle_max,
        stats.folds,
        stats.cdf_max,
        stats.cdf_nonzero_tiles,
        stats.esc_nonzero
    );
    assert!(stats.dirty > 0, "no dirty tiles ever reported");
    assert!(stats.analysed_colors > 0, "tile analysis never populated");
    assert!(stats.palrle_max >= 4, "PalRLE compact list never populated");
    assert!(stats.folds > 0, "palette subset fold never triggered");
    assert!(stats.cdf_max > 0, "CDF53 compact list never populated");
    assert!(stats.cdf_nonzero_tiles > 0, "CDF53 coefficients all zero");
    assert!(stats.esc_nonzero > 0, "escalation coefficients all zero");

    // On any mismatch, print the whole observed table before failing, so an
    // intended change can be pasted into GOLDEN_EXPECTED from the test output.
    if got.as_slice() != GOLDEN_EXPECTED {
        eprintln!("GOLDEN_TABLE_BEGIN");
        for (i, n, h) in &got {
            eprintln!("    ({i}, \"{n}\", 0x{h:016x}),");
        }
        eprintln!("GOLDEN_TABLE_END");
    }
    for (k, g) in got.iter().enumerate() {
        match GOLDEN_EXPECTED.get(k) {
            Some(e) if e == g => {}
            e => panic!(
                "golden mismatch at (frame {}, surface {}): got 0x{:016x}, expected {:x?}",
                g.0, g.1, g.2, e
            ),
        }
    }
    assert_eq!(got.len(), GOLDEN_EXPECTED.len(), "golden row count changed");
}
