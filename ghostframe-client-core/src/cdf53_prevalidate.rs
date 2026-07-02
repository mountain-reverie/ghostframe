//! Port of `ghostframe-web-client/src/prevalidate_cdf53.ts`.
//!
//! Parses one CDF53 pass payload into 3 × 128-byte bit-planes (B, G, R
//! order) ready for GPU upload. Payload layout (M3.3a wire format):
//!   `[u16 BE len_B][rle_B][u16 BE len_G][rle_G][u16 BE len_R][rle_R]`

use crate::event::DecodeErrorCode;
use ghostframe_protocol::codec::cdf53::rle_decode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevalidatedCdf53 {
    pub generation: u8,
    pub pass_idx: u8,
    /// 384 = 3 channels × 128 bytes, packed in B, G, R order.
    pub bit_planes: Vec<u8>,
}

pub fn prevalidate_cdf53(
    payload: &[u8],
    generation: u8,
    pass_idx: u8,
) -> Result<PrevalidatedCdf53, DecodeErrorCode> {
    if pass_idx >= 14 {
        return Err(DecodeErrorCode::Cdf53BadPass);
    }

    let mut bit_planes = vec![0u8; 384];
    let mut offset = 0usize;
    for ch in 0..3usize {
        if offset + 2 > payload.len() {
            return Err(DecodeErrorCode::Cdf53Truncated);
        }
        let len = ((payload[offset] as usize) << 8) | (payload[offset + 1] as usize);
        offset += 2;
        if offset + len > payload.len() {
            return Err(DecodeErrorCode::Cdf53Truncated);
        }
        let decoded = rle_decode(&payload[offset..offset + len]);
        offset += len;
        if decoded.len() != 128 {
            return Err(DecodeErrorCode::Cdf53RleLength);
        }
        bit_planes[ch * 128..ch * 128 + 128].copy_from_slice(&decoded);
    }

    Ok(PrevalidatedCdf53 {
        generation,
        pass_idx,
        bit_planes,
    })
}
