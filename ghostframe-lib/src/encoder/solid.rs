//! Solid tile codec — 4 bytes per tile.
//!
//! When the classifier reports `unique_colors == 1`, every pixel of the tile
//! is the same BGRA. We encode by sampling the first pixel; the client fills
//! the whole tile region with that BGRA via `ctx.fillRect`.

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SolidDecodeError {
    #[error("solid payload must be exactly 4 bytes; got {0}")]
    WrongSize(usize),
}

/// Encode a uniformly-colored tile as 4 BGRA bytes (sample of the first pixel).
///
/// Caller is responsible for ensuring the tile is truly uniform; the classifier
/// rule `unique_colors == 1` enforces this upstream.
pub fn encode_solid(tile_bgra: &[u8]) -> [u8; 4] {
    tile_bgra[..4]
        .try_into()
        .expect("tile must be at least 4 bytes")
}

/// Decode a 4-byte BGRA Solid payload. Returns the BGRA color the client
/// should fill the tile region with.
pub fn decode_solid(payload: &[u8]) -> Result<[u8; 4], SolidDecodeError> {
    if payload.len() != 4 {
        return Err(SolidDecodeError::WrongSize(payload.len()));
    }
    Ok(payload[..4].try_into().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_extracts_first_pixel_bgra() {
        // 32x32 BGRA tile, all #FF0080FF (BGRA = FF, 00, 80, FF).
        let mut tile = vec![0u8; 32 * 32 * 4];
        for px in tile.chunks_exact_mut(4) {
            px.copy_from_slice(&[0xFF, 0x00, 0x80, 0xFF]);
        }
        let out = encode_solid(&tile);
        assert_eq!(out, [0xFF, 0x00, 0x80, 0xFF]);
    }

    #[test]
    fn decode_returns_4_byte_array() {
        let payload = [0x11, 0x22, 0x33, 0x44];
        let out = decode_solid(&payload).expect("valid payload");
        assert_eq!(out, [0x11, 0x22, 0x33, 0x44]);
    }

    #[test]
    fn decode_rejects_short_payload() {
        assert!(decode_solid(&[0x11, 0x22, 0x33]).is_err());
    }

    #[test]
    fn decode_rejects_long_payload() {
        assert!(decode_solid(&[0; 5]).is_err());
    }

    #[test]
    fn encode_decode_roundtrip() {
        let mut tile = vec![0u8; 32 * 32 * 4];
        for px in tile.chunks_exact_mut(4) {
            px.copy_from_slice(&[1, 2, 3, 4]);
        }
        let encoded = encode_solid(&tile);
        let decoded = decode_solid(&encoded).unwrap();
        assert_eq!(decoded, [1, 2, 3, 4]);
    }
}
