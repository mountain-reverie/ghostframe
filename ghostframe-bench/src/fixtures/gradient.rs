use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile carrying a smooth diagonal gradient — the canonical input
/// for BC1 / CDF 5/3 wavelet.
pub fn tile() -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            let t = ((x + y) as f32) / ((TILE_SIZE * 2 - 2) as f32);
            let v = (t * 255.0) as u8;
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off..off + 4].copy_from_slice(&[v, 255 - v, v / 2, 0xFF]);
        }
    }
    buf
}
