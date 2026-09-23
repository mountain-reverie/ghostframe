//! Raw codec: BGRA wire bytes uploaded straight into the framebuffer.
//!
//! `TileData::Raw` is payload-proportional and may be SHORTER than a full
//! 4096-byte tile (it is only guaranteed to be a multiple of 4). Upload only
//! the rows the payload actually covers; assuming a full tile reads past the
//! end of the slice.
//!
//! Unlike [`crate::pipelines::solid`], there is no shader in this path, so
//! the BGRA -> RGBA swizzle happens on the CPU here.

use crate::framebuffer::Framebuffer;
use ghostframe_protocol::tile::TILE_SIZE;

/// Bytes per row of a full-width tile row: `TILE_SIZE` BGRA pixels.
const ROW_BYTES: usize = (TILE_SIZE * 4) as usize;

/// Upload a `Codec::Raw` tile's BGRA wire bytes into `fb` at tile coordinates
/// `(tile_x, tile_y)`.
///
/// `bgra` covers zero or more whole rows of the 32x32 tile, top to bottom.
/// Only `bgra.len() / ROW_BYTES` whole rows are present; a trailing partial
/// row (fewer than `ROW_BYTES` leftover bytes) carries no complete pixel row
/// and is dropped rather than read out of bounds. The upload is also clamped
/// to the framebuffer's actual edge, mirroring
/// [`Framebuffer::debug_fill_tile`]'s clamp for a non-tile-aligned
/// bottom/right border.
pub fn upload_raw_tile(queue: &wgpu::Queue, fb: &Framebuffer, tile_x: u8, tile_y: u8, bgra: &[u8]) {
    let full_rows = (bgra.len() / ROW_BYTES).min(TILE_SIZE as usize);
    if full_rows == 0 {
        return;
    }

    let x = tile_x as u32 * TILE_SIZE;
    let y = tile_y as u32 * TILE_SIZE;
    let w = TILE_SIZE.min(fb.width.saturating_sub(x));
    let h = (full_rows as u32).min(fb.height.saturating_sub(y));
    if w == 0 || h == 0 {
        return;
    }

    // Swizzle BGRA -> RGBA on the CPU (no shader in this path) and clip to
    // `w` columns per row in case the framebuffer edge clamps narrower than
    // the tile.
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        let row_start = row * ROW_BYTES;
        for col in 0..w as usize {
            let px = row_start + col * 4;
            let b = bgra[px];
            let g = bgra[px + 1];
            let r = bgra[px + 2];
            let a = bgra[px + 3];
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: fb.texture(),
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        &rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(w * 4),
            rows_per_image: Some(h),
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
}
