pub const TILE_SIZE: u32 = 32;
pub const BPP: u32 = 4;
pub const TILE_BYTES: usize = (TILE_SIZE * TILE_SIZE * BPP) as usize;

#[derive(Debug, Clone)]
pub struct TileGrid {
    pub width: u32,
    pub height: u32,
    pub cols: u32,
    pub rows: u32,
}

impl TileGrid {
    pub fn new(width: u32, height: u32) -> Self {
        let cols = width.div_ceil(TILE_SIZE);
        let rows = height.div_ceil(TILE_SIZE);
        Self { width, height, cols, rows }
    }

    pub fn tile_count(&self) -> u32 {
        self.cols * self.rows
    }

    /// Extract a 32x32 tile at grid position (tile_x, tile_y) from a packed pixel buffer.
    ///
    /// `stride` is the number of bytes per row in the source buffer.
    /// Edge tiles are zero-padded to the full `TILE_BYTES` size.
    pub fn extract_tile(&self, pixels: &[u8], stride: u32, tile_x: u32, tile_y: u32) -> Vec<u8> {
        let mut out = vec![0u8; TILE_BYTES];

        let origin_x = tile_x * TILE_SIZE;
        let origin_y = tile_y * TILE_SIZE;

        // How many pixels this tile actually covers (may be less at edges).
        let copy_w = (self.width.saturating_sub(origin_x)).min(TILE_SIZE);
        let copy_h = (self.height.saturating_sub(origin_y)).min(TILE_SIZE);

        for row in 0..copy_h {
            let src_row = origin_y + row;
            let src_offset = (src_row * stride + origin_x * BPP) as usize;
            let dst_offset = (row * TILE_SIZE * BPP) as usize;
            let row_bytes = (copy_w * BPP) as usize;
            // Only copy if source data is within bounds
            if src_offset + row_bytes <= pixels.len() {
                out[dst_offset..dst_offset + row_bytes]
                    .copy_from_slice(&pixels[src_offset..src_offset + row_bytes]);
            }
        }

        out
    }

    /// Iterate all tile coordinates (col, row) in row-major order.
    pub fn iter_coords(&self) -> impl Iterator<Item = (u32, u32)> {
        let cols = self.cols;
        let rows = self.rows;
        (0..rows).flat_map(move |row| (0..cols).map(move |col| (col, row)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_grid_dimensions() {
        let grid = TileGrid::new(1920, 1080);
        assert_eq!(grid.cols, 60);
        assert_eq!(grid.rows, 34);
        assert_eq!(grid.tile_count(), 2040);
    }

    #[test]
    fn tile_grid_exact_multiple() {
        let grid = TileGrid::new(640, 480);
        assert_eq!(grid.cols, 20);
        assert_eq!(grid.rows, 15);
    }

    #[test]
    fn tile_grid_small() {
        let grid = TileGrid::new(48, 48);
        assert_eq!(grid.cols, 2);
        assert_eq!(grid.rows, 2);
    }

    #[test]
    fn extract_tile_full_size() {
        // 64x64 frame; paint tile (1,0) — columns 32..64, rows 0..32 — red in BGRA.
        let stride = 64 * BPP;
        let mut pixels = vec![0u8; (64 * 64 * BPP) as usize];
        // Red in BGRA = [0, 0, 255, 255]
        for row in 0..32u32 {
            for col in 32..64u32 {
                let off = (row * stride + col * BPP) as usize;
                pixels[off] = 0;   // B
                pixels[off + 1] = 0;   // G
                pixels[off + 2] = 255; // R
                pixels[off + 3] = 255; // A
            }
        }

        let grid = TileGrid::new(64, 64);
        let tile = grid.extract_tile(&pixels, stride, 1, 0);

        assert_eq!(tile.len(), TILE_BYTES);
        // First pixel of extracted tile should be red BGRA.
        assert_eq!(&tile[0..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn extract_tile_bad_stride_no_panic() {
        let pixels = vec![0u8; (64 * 64 * BPP) as usize];
        let grid = TileGrid::new(64, 64);
        // Stride is intentionally too small -- extract should not panic
        let tile = grid.extract_tile(&pixels, 64, 0, 1);
        assert_eq!(tile.len(), TILE_BYTES);
    }

    #[test]
    fn extract_tile_edge_partial() {
        // 48x48 frame → tile (1,1) covers columns 32..48 (16 px wide) and rows 32..48 (16 px
        // tall), but must be padded to a full 32x32 tile.
        let stride = 48 * BPP;
        let pixels = vec![1u8; (48 * 48 * BPP) as usize];

        let grid = TileGrid::new(48, 48);
        let tile = grid.extract_tile(&pixels, stride, 1, 1);

        assert_eq!(tile.len(), TILE_BYTES);
    }
}
