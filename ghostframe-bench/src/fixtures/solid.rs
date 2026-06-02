use super::TILE_BYTES;

/// 32×32 tile filled with a single BGRA colour.
pub fn tile() -> Vec<u8> {
    let mut buf = Vec::with_capacity(TILE_BYTES);
    for _ in 0..(TILE_BYTES / 4) {
        buf.extend_from_slice(&[0x33, 0x66, 0x99, 0xFF]); // muted blue
    }
    buf
}
