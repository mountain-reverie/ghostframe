use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile drawn with 16 fixed BGRA palette entries — representative of
/// flat UI elements (toolbars, list rows). Uses a low-entropy block layout
/// so palettised RLE both has something realistic to compress.
pub fn tile() -> Vec<u8> {
    let palette: [[u8; 4]; 16] = [
        [0x1E, 0x1E, 0x1E, 0xFF],
        [0x2D, 0x2D, 0x30, 0xFF],
        [0x3F, 0x3F, 0x46, 0xFF],
        [0x55, 0x55, 0x5A, 0xFF],
        [0x68, 0x68, 0x70, 0xFF],
        [0x80, 0x80, 0x88, 0xFF],
        [0x9A, 0x9A, 0xA0, 0xFF],
        [0xB0, 0xB0, 0xB8, 0xFF],
        [0xC8, 0xC8, 0xD0, 0xFF],
        [0xE0, 0xE0, 0xE8, 0xFF],
        [0xF0, 0xF0, 0xF5, 0xFF],
        [0xFF, 0xFF, 0xFF, 0xFF],
        [0x00, 0x7A, 0xCC, 0xFF],
        [0xCC, 0x66, 0x00, 0xFF],
        [0x33, 0x99, 0x33, 0xFF],
        [0x99, 0x33, 0x99, 0xFF],
    ];
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            // Block-y palette index — sub-tile bands for run-length friendliness.
            let idx = (((x / 4) + (y / 8) * 4) as usize) % palette.len();
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&palette[idx]);
        }
    }
    buf
}
