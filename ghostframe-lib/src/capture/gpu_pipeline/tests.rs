/// Create a memfd of `size` bytes filled with `pixel` repeated, return the fd.
///
/// Helper shared by tests.
unsafe fn make_memfd(width: u32, height: u32, pixel: [u8; 4]) -> std::os::unix::io::RawFd {
    let stride = width * 4;
    let size = (stride * height) as usize;
    let name = std::ffi::CString::new("ghost-test").unwrap();
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
    for chunk in slice.chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }
    libc::munmap(ptr, size);
    fd
}

use super::*;

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
fn identical_frames_produce_no_dirty_tiles() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    // Solid red pixel
    let pixel: [u8; 4] = [0, 0, 255, 255]; // BGRA: R

    let mut tracker = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping identical_frames test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // First call: establishes prev, returns all tiles dirty.
        let fd1 = make_memfd(width, height, pixel);
        let first = match tracker.diff(fd1, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping identical_frames (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);

        // Expect all 4 tiles dirty (64/32 = 2 cols * 2 rows = 4 tiles)
        assert_eq!(first.len(), 4, "first frame should have all tiles dirty");

        // Second call with identical content: should have no dirty tiles
        let fd2 = make_memfd(width, height, pixel);
        let second = match tracker.diff(fd2, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping identical_frames second diff: {e}");
                return;
            }
        };
        libc::close(fd2);

        assert_eq!(
            second.len(),
            0,
            "identical frames should produce no dirty tiles, got: {second:?}"
        );
    }
}

#[test]
fn changed_pixel_detected() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;

    let mut tracker = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping changed_pixel test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // Frame 1: all black.
        let black: [u8; 4] = [0, 0, 0, 255];
        let fd1 = make_memfd(width, height, black);
        let first = match tracker.diff(fd1, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping changed_pixel (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert_eq!(first.len(), 4, "first frame: all tiles dirty");

        // Frame 2: tile (1,0) changed to white.
        let size = (stride * height) as usize;
        let name = std::ffi::CString::new("ghost-test2").unwrap();
        let fd2 = libc::memfd_create(name.as_ptr(), 0);
        assert!(fd2 >= 0);
        libc::ftruncate(fd2, size as i64);

        let ptr = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd2,
            0,
        );
        assert_ne!(ptr, libc::MAP_FAILED);
        let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);

        // Fill all black first
        for chunk in frame.chunks_exact_mut(4) {
            chunk.copy_from_slice(&black);
        }
        // Paint tile (col=1, row=0) white: x=[32..63], y=[0..31]
        for row in 0..32u32 {
            for col in 32..64u32 {
                let offset = ((row * stride) + col * 4) as usize;
                frame[offset..offset + 4].copy_from_slice(&[255u8, 255, 255, 255]);
            }
        }
        libc::munmap(ptr, size);

        let second = match tracker.diff(fd2, width, height, stride) {
            Ok(v) => v,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping changed_pixel second diff: {e}");
                return;
            }
        };
        libc::close(fd2);

        // Only tile index 1 (col=1, row=0 → 0*2+1 = 1) should be dirty
        assert_eq!(
            second.len(),
            1,
            "only one tile should be dirty, got: {second:?}"
        );
        assert_eq!(second[0], 1, "dirty tile should be index 1 (col=1,row=0)");
    }
}

/// Test that process_frame returns NV12 data with correct pointer and dimensions.
#[test]
fn process_frame_returns_nv12_data() {
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    // Solid red in BGRA: B=0, G=0, R=255, A=255
    let pixel: [u8; 4] = [0, 0, 255, 255];

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping process_frame_returns_nv12_data (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        let fd = make_memfd(width, height, pixel);
        let analysis = match processor.process_frame(fd, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd);
                eprintln!(
                    "Skipping process_frame_returns_nv12_data (memfd not a real DMA-BUF): {e}"
                );
                return;
            }
        };
        libc::close(fd);

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
        let y_slice = std::slice::from_raw_parts(analysis.nv12_data, (width * height) as usize);
        let y_avg: u32 = y_slice.iter().map(|&b| b as u32).sum::<u32>() / y_slice.len() as u32;
        // Allow generous tolerance for GPU rounding differences
        assert!(
            y_avg > 50 && y_avg < 100,
            "Y for solid red should be ~76, got average {y_avg}"
        );
    }
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
    assert_eq!(&zero.colors as *const _ as usize - base, 16, "colors offset");
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
fn process_frame_returns_tile_analysis_for_solid_red() {
    // Solid red 32×32 → exactly one tile, count=1, edge_density_thou=0.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;
    // BGRA: B=0, G=0, R=255, A=255 → packed u32 = 0xFFFF0000
    let pixel: [u8; 4] = [0, 0, 255, 255];

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping process_frame_returns_tile_analysis (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // First frame: tile_analysis runs even without a prev_image,
        // so the slice must be populated — unblocks static-content
        // classification (solid/PalRLE).
        let fd1 = make_memfd(width, height, pixel);
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
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
        let fd2 = make_memfd(width, height, pixel);
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

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
}

#[test]
fn process_frame_tile_analysis_checkerboard() {
    // 32×32 frame with 1-pixel checkerboard: pixel (x,y) is red if (x+y)&1, else blue.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping checkerboard test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        let make_fd = |name_suffix: &str| -> std::os::unix::io::RawFd {
            let size = (stride * height) as usize;
            let name =
                std::ffi::CString::new(format!("ghost-test-checkerboard-{}", name_suffix))
                    .unwrap();
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            for y in 0..height {
                for x in 0..width {
                    let offset = ((y * stride) + x * 4) as usize;
                    let bgra = if (x + y) & 1 == 0 {
                        [255, 0, 0, 255] // blue
                    } else {
                        [0, 0, 255, 255] // red
                    };
                    frame[offset..offset + 4].copy_from_slice(&bgra);
                }
            }
            libc::munmap(ptr, size);
            fd
        };

        // First frame: tile_analysis runs (M3.2c). Seed prev_image and
        // verify first-frame slice is populated.
        let fd1 = make_fd("seed");
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping checkerboard (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(
            !first.tile_analysis.is_null(),
            "first-frame analysis must be populated"
        );
        assert_eq!(first.tile_analysis_len, 1);

        // Second frame: same content. Now the analysis pipeline dispatches.
        let fd2 = make_fd("real");
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping checkerboard (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

        let entry = &analysis.tile_analysis_slice()[0];
        assert_eq!(entry.count, 2, "checkerboard → 2 unique colors");

        // Both colors must appear; order is slot-traversal, not specified.
        let blue: u32 = 0xFF0000FF; // B=255, G=0, R=0, A=255 → 0xFF | 0 | 0 | 0xFF000000
        let red: u32 = 0xFFFF0000; // B=0, G=0, R=255, A=255
        assert!(
            (entry.colors[0] == blue || entry.colors[1] == blue)
                && (entry.colors[0] == red || entry.colors[1] == red),
            "expected both red and blue, got [{:#x}, {:#x}]",
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
}

#[test]
fn process_frame_tile_analysis_overflow_at_17_colors() {
    // Place 17 distinct colors spread across the tile. Expect count=17 sentinel.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping overflow test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
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

        let make_fd = |name_suffix: &str| -> std::os::unix::io::RawFd {
            let size = (stride * height) as usize;
            let name =
                std::ffi::CString::new(format!("ghost-test-overflow-{}", name_suffix)).unwrap();
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            // 17 distinct BGRA colors. Background = color 0.
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
            libc::munmap(ptr, size);
            fd
        };

        // First frame: tile_analysis runs (M3.2c). Seed prev_image and
        // confirm slice is populated.
        let fd1 = make_fd("seed");
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping overflow (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(
            !first.tile_analysis.is_null(),
            "first-frame analysis must be populated"
        );
        assert_eq!(first.tile_analysis_len, 1);

        // Second frame: same content. Now the analysis pipeline dispatches.
        let fd2 = make_fd("real");
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping overflow (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

        let entry = &analysis.tile_analysis_slice()[0];
        assert_eq!(entry.count, 17, "17 distinct colors → overflow sentinel");
        // colors[] is undefined per contract — do not assert on it.
    }
}

#[test]
fn process_frame_tile_analysis_frame_edge_denominator() {
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
    let cols = 2u32;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping frame-edge test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        let make_fd = |name_suffix: &str| -> std::os::unix::io::RawFd {
            let size = (stride * height) as usize;
            let name =
                std::ffi::CString::new(format!("ghost-test-edge-{}", name_suffix)).unwrap();
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            for chunk in frame.chunks_exact_mut(4) {
                chunk.copy_from_slice(&[0, 0, 255, 255]); // red BGRA
            }
            // Inject one bright-green pixel at (x=35, y=10) — interior of tile (1,0):
            // tile (1,0) covers x=32..40, y=0..32; (35,10) is in-bounds and not on
            // the frame edge.
            let off = ((10u32 * stride) + 35 * 4) as usize;
            frame[off..off + 4].copy_from_slice(&[0, 255, 0, 255]); // green BGRA
            libc::munmap(ptr, size);
            fd
        };

        // First frame: tile_analysis runs (M3.2c). Seed prev_image and
        // confirm slice is populated.
        let fd1 = make_fd("seed");
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping frame-edge (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(
            !first.tile_analysis.is_null(),
            "first-frame analysis must be populated"
        );
        assert_eq!(first.tile_analysis_len, 4, "40x40 → 2x2 tile grid");

        // Second frame: same content. Now the analysis pipeline dispatches.
        let fd2 = make_fd("real");
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping frame-edge (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

        let slice = analysis.tile_analysis_slice();
        assert_eq!(analysis.tile_analysis_len, 4, "40x40 → 2x2 tile grid");

        // Tile (1,0): row 0, col 1 of a `cols`-wide grid.
        let tile_10_idx = (0 * cols + 1) as usize;
        let edge_tile = &slice[tile_10_idx];
        assert_eq!(edge_tile.count, 2, "tile (1,0) has red + green");
        // With proper denominator=256: density ≈ (~3 * 1000) / 256 ≈ 11
        // With naive denominator=1024: density ≈ (~3 * 1000) / 1024 ≈ 2
        assert!(
            edge_tile.edge_density_thou > 5,
            "frame-edge tile denominator wrong: density = {} (expected > 5 with denom=256)",
            edge_tile.edge_density_thou
        );
    }
}

#[test]
fn process_frame_tile_analysis_xor_mask_handles_zero_bgra() {
    // BGRA = 0x00000000 (transparent black) — must round-trip through the
    // hash set despite raw 0 being the "empty slot" sentinel. The XOR mask
    // means 0x00000000 is stored as 0xA5A5A5A5 in the slot.
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 0, 0];

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping XOR-mask test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // First frame: tile_analysis runs (M3.2c). Seed prev_image and
        // confirm slice is populated.
        let fd1 = make_memfd(width, height, pixel);
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping XOR-mask (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(
            !first.tile_analysis.is_null(),
            "first-frame analysis must be populated"
        );
        assert_eq!(first.tile_analysis_len, 1);

        // Second frame: same content. Now the analysis pipeline dispatches.
        let fd2 = make_memfd(width, height, pixel);
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping XOR-mask (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

        let entry = &analysis.tile_analysis_slice()[0];
        assert_eq!(entry.count, 1, "transparent-black tile → count=1");
        assert_eq!(
            entry.colors[0], 0x00000000u32,
            "BGRA(0,0,0,0) survives mask"
        );
        assert_eq!(entry.edge_density_thou, 0);
    }
}

#[test]
fn process_frame_tile_analysis_multi_tile_independence() {
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

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping multi-tile test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        let red: [u8; 4] = [0, 0, 255, 255];
        let blue: [u8; 4] = [255, 0, 0, 255];
        let green: [u8; 4] = [0, 255, 0, 255];
        let yellow: [u8; 4] = [0, 255, 255, 255];

        let make_fd = |name_suffix: &str| -> std::os::unix::io::RawFd {
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new(format!("ghost-test-multitile-{}", name_suffix))
                .unwrap();
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);

            // Fill helper.
            let mut put = |x: u32, y: u32, c: [u8; 4]| {
                let off = ((y * stride) + x * 4) as usize;
                frame[off..off + 4].copy_from_slice(&c);
            };

            for y in 0..32 {
                // tile (0,0): red
                for x in 0..32 {
                    put(x, y, red);
                }
                // tile (1,0): blue
                for x in 32..64 {
                    put(x, y, blue);
                }
            }
            for y in 32..64 {
                // tile (0,1): red/blue checker
                for x in 0..32 {
                    put(x, y, if (x + y) & 1 == 0 { red } else { blue });
                }
                // tile (1,1): three colors striped vertically (red/green/yellow)
                for x in 32..64 {
                    let c = match (x - 32) / 11 {
                        0 => red,
                        1 => green,
                        _ => yellow,
                    };
                    put(x, y, c);
                }
            }
            libc::munmap(ptr, size);
            fd
        };

        // First frame: tile_analysis runs (M3.2c). Seed prev_image and
        // confirm slice is populated.
        let fd1 = make_fd("seed");
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping multi-tile (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(
            !first.tile_analysis.is_null(),
            "first-frame analysis must be populated"
        );
        assert_eq!(first.tile_analysis_len, 4);

        // Second frame: same content. Now the analysis pipeline dispatches.
        let fd2 = make_fd("real");
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping multi-tile (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

        let slice = analysis.tile_analysis_slice();
        assert_eq!(analysis.tile_analysis_len, 4);
        // cols = 2; index = y * 2 + x
        assert_eq!(slice[0].count, 1, "tile (0,0) red");
        assert_eq!(slice[1].count, 1, "tile (1,0) blue");
        assert_eq!(slice[2].count, 2, "tile (0,1) checker");
        assert_eq!(slice[3].count, 3, "tile (1,1) three stripes");
    }
}

#[test]
fn tile_analysis_colors_are_canonical_sorted() {
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

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping canonical-sort test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // BGRA byte order: [B, G, R, A]
        let red:   [u8; 4] = [0, 0, 255, 255];
        let green: [u8; 4] = [0, 255, 0, 255];
        let blue:  [u8; 4] = [255, 0, 0, 255];

        let make_fd = |name_suffix: &str, fill: bool| -> std::os::unix::io::RawFd {
            let size = (stride * height) as usize;
            let name = std::ffi::CString::new(format!("ghost-sort-test-{}", name_suffix))
                .unwrap();
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);

            if fill {
                let mut put = |x: u32, y: u32, c: [u8; 4]| {
                    let off = ((y * stride) + x * 4) as usize;
                    frame[off..off + 4].copy_from_slice(&c);
                };
                for y in 0..32u32 {
                    for x in 0..32u32 {
                        // Tile 0: red(x<11) | green(x<22) | blue(rest).
                        let c = if x < 11 { red } else if x < 22 { green } else { blue };
                        put(x, y, c);
                        // Tile 1 (x+32): blue(x<11) | red(x<22) | green(rest).
                        let c2 = if x < 11 { blue } else if x < 22 { red } else { green };
                        put(x + 32, y, c2);
                    }
                }
            }
            // seed frame stays zeroed

            libc::munmap(ptr, size);
            fd
        };

        // First frame: tile_analysis runs (M3.2c). Seed frame is zeroed,
        // but the slice must still be populated.
        let fd1 = make_fd("seed", false);
        let first = match processor.process_frame(fd1, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd1);
                eprintln!("Skipping canonical-sort test (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd1);
        assert!(
            !first.tile_analysis.is_null(),
            "first-frame analysis must be populated"
        );
        assert_eq!(first.tile_analysis_len, 2, "64x32 frame → 2 tiles");

        // Second frame: real content — triggers tile_analysis dispatch.
        let fd2 = make_fd("real", true);
        let analysis = match processor.process_frame(fd2, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd2);
                eprintln!("Skipping canonical-sort test (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd2);

        let slice = analysis.tile_analysis_slice();
        assert_eq!(slice.len(), 2, "64x32 frame → 2 tiles");

        // BGRA packed-u32 ascending order:
        //   blue  (B=255,G=0,R=0,A=255) = 0xFF00_00FF
        //   green (B=0,G=255,R=0,A=255) = 0xFF00_FF00
        //   red   (B=0,G=0,R=255,A=255) = 0xFFFF_0000
        let blue_p:  u32 = 0xFF00_00FF;
        let green_p: u32 = 0xFF00_FF00;
        let red_p:   u32 = 0xFFFF_0000;

        for (tile_i, entry) in slice.iter().enumerate() {
            assert_eq!(entry.count, 3, "tile {}: expected count 3", tile_i);
            assert_eq!(entry.colors[0], blue_p,  "tile {}: slot 0 should be blue",  tile_i);
            assert_eq!(entry.colors[1], green_p, "tile {}: slot 1 should be green", tile_i);
            assert_eq!(entry.colors[2], red_p,   "tile {}: slot 2 should be red",   tile_i);
        }
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
fn process_frame_returns_palrle_compact_list_for_text_tiles() {
    let width = 96u32;
    let height = 32u32;
    let stride = width * 4;

    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Skipping palrle compact test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // --- Build the seed frame (all black zeros) ---
        let size = (stride * height) as usize;
        let name_seed = std::ffi::CString::new("ghost-palrle-seed").unwrap();
        let fd_seed = libc::memfd_create(name_seed.as_ptr(), 0);
        assert!(fd_seed >= 0);
        libc::ftruncate(fd_seed, size as i64);
        let ptr_seed = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd_seed,
            0,
        );
        assert_ne!(ptr_seed, libc::MAP_FAILED);
        // Zero-fill (all pixels black).
        std::ptr::write_bytes(ptr_seed as *mut u8, 0, size);
        libc::munmap(ptr_seed, size);

        let first = match processor.process_frame(fd_seed, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_seed);
                eprintln!("Skipping palrle compact test (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd_seed);
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
        let name_real = std::ffi::CString::new("ghost-palrle-real").unwrap();
        let fd_real = libc::memfd_create(name_real.as_ptr(), 0);
        assert!(fd_real >= 0);
        libc::ftruncate(fd_real, size as i64);
        let ptr_real = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd_real,
            0,
        );
        assert_ne!(ptr_real, libc::MAP_FAILED);
        let frame = std::slice::from_raw_parts_mut(ptr_real as *mut u8, size);

        // tile 0 (x 0..31): solid red (BGRA: B=0,G=0,R=255,A=255)
        for y in 0..32u32 {
            for x in 0..32u32 {
                let off = ((y * stride) + x * 4) as usize;
                frame[off..off + 4].copy_from_slice(&[0, 0, 255, 255]);
            }
        }
        // tile 1 (x 32..63): 4 colors in quadrants
        for y in 0..32u32 {
            for x in 32..64u32 {
                let off = ((y * stride) + x * 4) as usize;
                let c = if y < 16 && x < 48 {
                    [0u8, 0, 0, 255]
                } else if y < 16 {
                    [255, 255, 255, 255]
                } else if x < 48 {
                    [128, 128, 128, 255]
                } else {
                    [200, 200, 200, 255]
                };
                frame[off..off + 4].copy_from_slice(&c);
            }
        }
        // tile 2 (x 64..95): gradient (many colors)
        for y in 0..32u32 {
            for x in 64..96u32 {
                let off = ((y * stride) + x * 4) as usize;
                let local_x = (x - 64) as u8;
                frame[off..off + 4]
                    .copy_from_slice(&[local_x, y as u8, local_x.wrapping_add(y as u8), 255]);
            }
        }

        libc::munmap(ptr_real, size);

        let analysis = match processor.process_frame(fd_real, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_real);
                eprintln!("Skipping palrle compact test second frame (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd_real);

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
}

/// Stage 2a: 4 tiles each with identical 2-color palette {black, white}.
/// Expected: 4 compact entries, 1 unique frame palette, all per_tile_id == 0.
#[test]
fn process_frame_dedups_identical_palettes_within_frame() {
    let width = 128u32;
    let height = 32u32;
    let stride = width * 4;

    let mut proc = match GpuFrameProcessor::new(128) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping dedup test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        // --- Seed frame: all black ---
        let size = (stride * height) as usize;
        let name_seed = std::ffi::CString::new("ghost-dedup-seed").unwrap();
        let fd_seed = libc::memfd_create(name_seed.as_ptr(), 0);
        assert!(fd_seed >= 0);
        libc::ftruncate(fd_seed, size as i64);
        let ptr_seed = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd_seed,
            0,
        );
        assert_ne!(ptr_seed, libc::MAP_FAILED);
        std::ptr::write_bytes(ptr_seed as *mut u8, 0, size);
        libc::munmap(ptr_seed, size);

        let first = match proc.process_frame(fd_seed, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_seed);
                eprintln!("Skipping dedup test (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd_seed);
        // First frame: frame_palette_set is now populated.
        assert!(
            !first.frame_palette_set.is_null(),
            "first-frame frame_palette_set is now populated"
        );

        // --- Real frame: 4 tiles (32x32 each), each with identical 2-color
        // checkerboard {black, white} ---
        let name_real = std::ffi::CString::new("ghost-dedup-real").unwrap();
        let fd_real = libc::memfd_create(name_real.as_ptr(), 0);
        assert!(fd_real >= 0);
        libc::ftruncate(fd_real, size as i64);
        let ptr_real = libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd_real,
            0,
        );
        assert_ne!(ptr_real, libc::MAP_FAILED);
        let frame = std::slice::from_raw_parts_mut(ptr_real as *mut u8, size);

        for tile_x in 0..4u32 {
            for y in 0..32u32 {
                for x in 0..32u32 {
                    let off = ((y * stride) + (tile_x * 32 + x) * 4) as usize;
                    let c = if (x + y) % 2 == 0 {
                        [0u8, 0, 0, 255]     // black BGRA
                    } else {
                        [255u8, 255, 255, 255] // white BGRA
                    };
                    frame[off..off + 4].copy_from_slice(&c);
                }
            }
        }
        libc::munmap(ptr_real, size);

        let analysis = match proc.process_frame(fd_real, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_real);
                eprintln!("Skipping dedup test second frame (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd_real);

        // 4 tiles, all feasible (count=2 each).
        assert_eq!(
            analysis.palrle_compact_count, 4,
            "expected 4 compact tiles; got {}",
            analysis.palrle_compact_count
        );

        // Stage 2a: all 4 tiles share the same palette → 1 unique frame palette.
        assert_eq!(
            analysis.frame_palette_set_count, 1,
            "4 identical-palette tiles → 1 unique frame palette; got {}",
            analysis.frame_palette_set_count
        );

        // All per-tile IDs must map to frame palette slot 0.
        let ids = analysis.per_tile_frame_palette_id_slice();
        assert_eq!(ids.len(), 4, "per_tile_id length must equal compact_count");
        for (i, &id) in ids.iter().enumerate() {
            assert_eq!(id, 0, "tile {} per_tile_id should be 0, got {}", i, id);
        }
    }
}

/// Stage 2b: two tiles where tile 0 has palette {black, white, grey} (3 colors)
/// and tile 1 has palette {black, white, grey, blue/red} (4 colors — superset of tile 0).
/// After dispatch, tile 0's folded_into entry should resolve to tile 1's palette slot.
#[test]
fn process_frame_folds_subset_palette() {
    let width = 64u32;
    let height = 32u32;
    let stride = width * 4;

    let mut proc = match GpuFrameProcessor::new(64) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping subset-fold test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        let size = (stride * height) as usize;

        // Helper: allocate a memfd, fill it, and return the fd.
        let make_frame_fd = |name: &str, fill: &dyn Fn(&mut [u8])| -> std::os::unix::io::RawFd {
            let cname = std::ffi::CString::new(name).unwrap();
            let fd = libc::memfd_create(cname.as_ptr(), 0);
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            fill(frame);
            libc::munmap(ptr, size);
            fd
        };

        // Seed frame: all black (to initialize prev_image).
        let fd_seed = make_frame_fd("ghost-subset-seed", &|frame| {
            std::ptr::write_bytes(frame.as_mut_ptr(), 0, size);
        });
        let first = match proc.process_frame(fd_seed, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_seed);
                eprintln!("Skipping subset-fold (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd_seed);
        // First frame: folded_into is now populated.
        assert!(
            !first.folded_into.is_null(),
            "first-frame folded_into is now populated"
        );

        // Real frame:
        //   tile 0 (x=0..32): palette {black, white, grey} (3 colors)
        //   tile 1 (x=32..64): palette {black, white, grey, red/blue} (4 colors)
        // Tile 0's palette is a strict subset of tile 1's palette.
        let fd_real = make_frame_fd("ghost-subset-real", &|frame| {
            // tile 0: 3-color cycling pattern
            for y in 0..32u32 {
                for x in 0..32u32 {
                    let off = ((y * stride) + x * 4) as usize;
                    let c: [u8; 4] = match (x + y * 32) % 3 {
                        0 => [0, 0, 0, 255],       // black BGRA
                        1 => [255, 255, 255, 255],  // white BGRA
                        _ => [128, 128, 128, 255],  // grey BGRA
                    };
                    frame[off..off + 4].copy_from_slice(&c);
                }
            }
            // tile 1: 4-color cycling pattern (superset of tile 0)
            for y in 0..32u32 {
                for x in 32..64u32 {
                    let off = ((y * stride) + x * 4) as usize;
                    let c: [u8; 4] = match (x + y * 32) % 4 {
                        0 => [0, 0, 0, 255],       // black BGRA
                        1 => [255, 255, 255, 255],  // white BGRA
                        2 => [128, 128, 128, 255],  // grey BGRA
                        _ => [255, 0, 0, 255],      // blue BGRA (B=255,G=0,R=0)
                    };
                    frame[off..off + 4].copy_from_slice(&c);
                }
            }
        });
        let analysis = match proc.process_frame(fd_real, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_real);
                eprintln!(
                    "Skipping subset-fold second frame (memfd not a real DMA-BUF): {e}"
                );
                return;
            }
        };
        libc::close(fd_real);

        // Both tiles must be PalRLE-feasible (count <= 16).
        assert_eq!(
            analysis.palrle_compact_count, 2,
            "expected 2 compact tiles; got {}",
            analysis.palrle_compact_count
        );

        // Stage 2a: 2 distinct palettes.
        assert_eq!(
            analysis.frame_palette_set_count, 2,
            "tile 0 (3-color) and tile 1 (4-color) → 2 unique frame palettes; got {}",
            analysis.frame_palette_set_count
        );

        let folded = analysis.folded_into_slice();
        assert_eq!(folded.len(), 256, "folded_into must have 256 entries");

        let ids = analysis.per_tile_frame_palette_id_slice();
        assert_eq!(ids.len(), 2, "per_tile_id length must equal compact_count");

        let id0 = ids[0];
        let id1 = ids[1];

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
            "subset palette {} should fold into superset {}", subset_id, superset_id
        );
        let resolved_for_superset = folded[superset_id as usize] & 0xFFu32;
        assert_eq!(
            resolved_for_superset as u8, superset_id,
            "superset palette {} should stay self-referential", superset_id
        );
    }
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
fn process_frame_emits_correct_indices_for_two_color_tile() {
    let width = 32u32;
    let height = 32u32;
    let stride = width * 4;

    let mut proc = match GpuFrameProcessor::new(32) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping index emission test (no Vulkan GPU?): {e}");
            return;
        }
    };

    unsafe {
        let size = (stride * height) as usize;

        let make_frame_fd = |name: &str, fill: &dyn Fn(&mut [u8])| -> std::os::unix::io::RawFd {
            let cname = std::ffi::CString::new(name).unwrap();
            let fd = libc::memfd_create(cname.as_ptr(), 0);
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
            let frame = std::slice::from_raw_parts_mut(ptr as *mut u8, size);
            fill(frame);
            libc::munmap(ptr, size);
            fd
        };

        // Seed frame: all black (to initialize prev_image).
        let fd_seed = make_frame_fd("ghost-idx-seed", &|frame| {
            std::ptr::write_bytes(frame.as_mut_ptr(), 0, size);
        });
        let first = match proc.process_frame(fd_seed, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_seed);
                eprintln!("Skipping index emission test (memfd not a real DMA-BUF): {e}");
                return;
            }
        };
        libc::close(fd_seed);
        // First frame: index_buffer is now populated.
        assert!(
            !first.index_buffer.is_null(),
            "first-frame index_buffer is now populated"
        );

        // Real frame: top half black (y<16), bottom half white (y>=16).
        let fd_real = make_frame_fd("ghost-idx-real", &|frame| {
            for y in 0..height {
                for x in 0..width {
                    let off = ((y * stride) + x * 4) as usize;
                    let c: [u8; 4] = if y < 16 {
                        [0, 0, 0, 255]     // BGRA black
                    } else {
                        [255, 255, 255, 255] // BGRA white
                    };
                    frame[off..off + 4].copy_from_slice(&c);
                }
            }
        });
        let analysis = match proc.process_frame(fd_real, width, height, stride) {
            Ok(a) => a,
            Err(e) => {
                libc::close(fd_real);
                eprintln!(
                    "Skipping index emission test second frame (memfd not a real DMA-BUF): {e}"
                );
                return;
            }
        };
        libc::close(fd_real);

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
        assert_eq!(indices.len(), 512, "index_buffer_slice_for must return 512 bytes");

        // 32×32 = 1024 pixels total, packed 2 per byte (low nibble first).
        // Top half: pixels 0..511 → all black → index 0 → both nibbles 0.
        // Bottom half: pixels 512..1023 → all white → index 1 → both nibbles 1.
        // Bytes 0..=255 correspond to pixel pairs 0..=511 → index 0.
        // Bytes 256..=511 correspond to pixel pairs 512..=1023 → index 1.
        for byte_idx in 0..256usize {
            let low = indices[byte_idx] & 0x0F;
            let high = (indices[byte_idx] >> 4) & 0x0F;
            assert_eq!(
                low, 0,
                "byte {} low nibble: pixel {} should be index 0 (black)",
                byte_idx, byte_idx * 2
            );
            assert_eq!(
                high, 0,
                "byte {} high nibble: pixel {} should be index 0 (black)",
                byte_idx, byte_idx * 2 + 1
            );
        }
        for byte_idx in 256..512usize {
            let low = indices[byte_idx] & 0x0F;
            let high = (indices[byte_idx] >> 4) & 0x0F;
            assert_eq!(
                low, 1,
                "byte {} low nibble: pixel {} should be index 1 (white)",
                byte_idx, byte_idx * 2
            );
            assert_eq!(
                high, 1,
                "byte {} high nibble: pixel {} should be index 1 (white)",
                byte_idx, byte_idx * 2 + 1
            );
        }
    }
}

#[test]
fn reset_for_session_drops_prev_image_and_sets_counter() {
    let mut processor = match GpuFrameProcessor::new(256) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("Skipping reset_for_session test (no Vulkan GPU?): {e}");
            return;
        }
    };

    // Counter starts at 0; prev_image starts None.
    assert_eq!(processor.force_all_dirty_remaining, 0);
    assert!(processor.prev_image.is_none());

    // Drive one frame to populate prev_image.
    let width = 64u32;
    let height = 64u32;
    let stride = width * 4;
    let pixel: [u8; 4] = [0, 0, 255, 255];
    unsafe {
        let fd = make_memfd(width, height, pixel);
        let result = processor.diff(fd, width, height, stride);
        libc::close(fd);
        match result {
            Ok(_) => {}
            Err(e) => {
                eprintln!("Skipping reset_for_session test (memfd not DMA-BUF?): {e}");
                return;
            }
        }
    }
    assert!(processor.prev_image.is_some(), "diff should populate prev_image");

    // The API under test.
    processor.reset_for_session(20);

    assert!(processor.prev_image.is_none(), "reset_for_session must drop prev_image");
    assert_eq!(processor.force_all_dirty_remaining, 20);
}
