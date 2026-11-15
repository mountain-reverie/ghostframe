// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `ghostframe-web-client/src/frame_limits.ts`.
//!
//! A `FrameDimensions` event drives a framebuffer allocation proportional to
//! width x height. The authoritative per-axis bound is the core's
//! `MAX_FRAME_DIMENSION` (read from there, not retyped); this adds the tile
//! count bound the old WebGPU dispatch had (4 workgroups a tile against a
//! 65535 dispatch limit => 16383 tiles).
//!
//! The TypeScript also rejected non-integers, negatives and NaN; a `u32`
//! cannot hold those, so those vitest cases have no Rust twin by construction.

use ghostframe_client_core::MAX_FRAME_DIMENSION;
use ghostframe_protocol::tile::TILE_SIZE;

pub const MAX_TILES: u32 = 16383;

/// `Ok` when a server-announced size is safe to allocate; otherwise why not.
pub fn check_frame_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!("empty size {width}x{height}"));
    }
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(format!(
            "size {width}x{height} exceeds the {MAX_FRAME_DIMENSION}px limit"
        ));
    }
    let tiles = width.div_ceil(TILE_SIZE) * height.div_ceil(TILE_SIZE);
    if tiles > MAX_TILES {
        return Err(format!(
            "size {width}x{height} needs more than {MAX_TILES} tiles"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_and_boundary_sizes() {
        for (w, h) in [
            (640, 480),
            (1920, 1080),
            (3840, 2160),
            (MAX_FRAME_DIMENSION, 1),
            (1, MAX_FRAME_DIMENSION),
        ] {
            assert!(check_frame_dimensions(w, h).is_ok(), "{w}x{h}");
        }
    }

    #[test]
    fn rejects_oversize_and_empty_sizes_and_too_many_tiles() {
        for (w, h) in [
            (MAX_FRAME_DIMENSION + 1, 480),
            (640, MAX_FRAME_DIMENSION + 1),
            (u32::MAX, u32::MAX),
            (0, 480),
            (640, 0),
            // fits per axis, needs more than 16383 tiles
            (MAX_FRAME_DIMENSION, 4320),
        ] {
            assert!(check_frame_dimensions(w, h).is_err(), "{w}x{h}");
        }
    }

    #[test]
    fn the_limit_covers_what_the_u8_tile_grid_can_address_and_not_much_more() {
        // Tile coordinates are u8 with two values reserved as sentinels.
        let addressable = (254 + 1) * TILE_SIZE;
        assert!(MAX_FRAME_DIMENSION >= addressable - TILE_SIZE);
        assert!(MAX_FRAME_DIMENSION <= addressable + TILE_SIZE * 8);
    }
}
