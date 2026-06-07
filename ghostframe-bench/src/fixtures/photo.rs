use super::{BPP, TILE_BYTES, TILE_SIZE};

/// 32×32 tile with high-entropy pseudo-random pixels — stresses any codec
/// that relies on spatial coherence.
pub fn tile() -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    // Linear-congruential PRNG so the fixture is deterministic without rand.
    let mut state: u32 = 0xC0FFEE_u32;
    for y in 0..TILE_SIZE {
        for x in 0..TILE_SIZE {
            state = state.wrapping_mul(1664525).wrapping_add(1013904223);
            let off = (y * TILE_SIZE * BPP + x * BPP) as usize;
            buf[off] = (state & 0xFF) as u8;
            buf[off + 1] = ((state >> 8) & 0xFF) as u8;
            buf[off + 2] = ((state >> 16) & 0xFF) as u8;
            buf[off + 3] = 0xFF;
        }
    }
    buf
}
