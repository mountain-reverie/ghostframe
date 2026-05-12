use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile representing a frame from a video stream — strong horizontal
/// gradients with one moving high-contrast object. This is the canonical
/// input for H.264.
pub fn tile() -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            // Horizontal gradient backdrop.
            let r = (x * 8) as u8;
            let g = (y * 8) as u8;
            let b = ((x ^ y) * 4) as u8;
            // High-contrast object: a 6×6 white block off-centre.
            let in_obj = (10..16).contains(&x) && (12..18).contains(&y);
            let c = if in_obj {
                [0xFF, 0xFF, 0xFF, 0xFF]
            } else {
                [b, g, r, 0xFF]
            };
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&c);
        }
    }
    buf
}
