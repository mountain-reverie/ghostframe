use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile representing monospace text — vertical strokes on a dark
/// background. Two distinct colours, high-contrast: the canonical input
/// for palettised RLE.
pub fn tile() -> Vec<u8> {
    let bg = [0x14, 0x14, 0x14, 0xFF]; // near-black
    let fg = [0xF5, 0xF5, 0xF5, 0xFF]; // near-white
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            // Vertical stroke pattern: column x is "ink" if x % 6 in {1, 2}
            // and y is within a glyph row band.
            let col_ink = matches!(x % 6, 1 | 2);
            let row_band = (y % 12) >= 2 && (y % 12) <= 9;
            let on = col_ink && row_band;
            let c = if on { fg } else { bg };
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&c);
        }
    }
    buf
}
