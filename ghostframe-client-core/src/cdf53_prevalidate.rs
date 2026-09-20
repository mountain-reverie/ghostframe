//! Port of `ghostframe-web-client/src/prevalidate_cdf53.ts`.
//!
//! Parses one CDF53 pass payload into 3 × 128-byte bit-planes (B, G, R
//! order) ready for GPU upload. Payload layout (M3.3a wire format):
//!   `[u16 BE len_B][rle_B][u16 BE len_G][rle_G][u16 BE len_R][rle_R]`
//!
//! Pass 0's payload additionally carries a 2-byte big-endian
//! `present_passes` bitmap ahead of the 3-channel block (bit *i* set ⇒ pass
//! *i* is present on the wire for this tile/generation) -- sparse encoding
//! skips empty bit-planes, so the client needs to be told which of passes
//! 1..13 to actually expect rather than assuming all 14 will eventually
//! arrive.

use crate::event::DecodeErrorCode;
use ghostframe_protocol::codec::cdf53::rle_decode;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrevalidatedCdf53 {
    pub generation: u8,
    pub pass_idx: u8,
    /// 384 = 3 channels × 128 bytes, packed in B, G, R order.
    pub bit_planes: Vec<u8>,
    /// The tile's `present_passes` bitmap, parsed from pass 0's payload
    /// prefix. `Some` only for `pass_idx == 0`; `None` for passes 1..13,
    /// which carry no such prefix.
    pub present_passes: Option<u16>,
}

pub fn prevalidate_cdf53(
    payload: &[u8],
    generation: u8,
    pass_idx: u8,
) -> Result<PrevalidatedCdf53, DecodeErrorCode> {
    if pass_idx >= 14 {
        return Err(DecodeErrorCode::Cdf53BadPass);
    }

    let mut offset = 0usize;
    let mut present_passes: Option<u16> = None;
    if pass_idx == 0 {
        if payload.len() < 2 {
            return Err(DecodeErrorCode::Cdf53Truncated);
        }
        let bitmap = ((payload[0] as u16) << 8) | (payload[1] as u16);
        // Bit 0 names pass 0 itself -- the very pass carrying this bitmap.
        // A tile claiming not to include the pass that transmitted the
        // claim is malformed; wire-supplied values are never trusted.
        if bitmap & 1 == 0 {
            return Err(DecodeErrorCode::Cdf53BadPass);
        }
        present_passes = Some(bitmap);
        offset = 2;
    }

    let mut bit_planes = vec![0u8; 384];
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
        present_passes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass0_payload(present_passes: u16, rle_planes: [&[u8]; 3]) -> Vec<u8> {
        let mut payload = vec![(present_passes >> 8) as u8, present_passes as u8];
        for rle in rle_planes {
            let len = rle.len() as u16;
            payload.push((len >> 8) as u8);
            payload.push(len as u8);
            payload.extend_from_slice(rle);
        }
        payload
    }

    /// 0x80 | 127 = 0xFF → a single zero-run token covering all 128 bytes
    /// of an all-zero bit-plane.
    const RLE_EMPTY_PLANE: &[u8] = &[0xFFu8];

    #[test]
    fn pass0_bitmap_roundtrips() {
        let payload = pass0_payload(0x0041, [RLE_EMPTY_PLANE, RLE_EMPTY_PLANE, RLE_EMPTY_PLANE]);
        let parsed = prevalidate_cdf53(&payload, 3, 0).expect("valid pass0 payload");
        assert_eq!(parsed.present_passes, Some(0x0041));
        assert_eq!(parsed.bit_planes, vec![0u8; 384]);
    }

    #[test]
    fn pass0_payload_too_short_for_bitmap_is_rejected() {
        let err = prevalidate_cdf53(&[0x00], 0, 0).unwrap_err();
        assert_eq!(err, DecodeErrorCode::Cdf53Truncated);
    }

    #[test]
    fn pass0_bitmap_with_bit0_clear_is_rejected() {
        // Bit 0 clear: the tile claims pass 0 itself isn't present, despite
        // this being pass 0's own payload. Malformed.
        let payload = pass0_payload(0x0040, [RLE_EMPTY_PLANE, RLE_EMPTY_PLANE, RLE_EMPTY_PLANE]);
        let err = prevalidate_cdf53(&payload, 0, 0).unwrap_err();
        assert_eq!(err, DecodeErrorCode::Cdf53BadPass);
    }

    #[test]
    fn non_pass0_has_no_present_passes() {
        let mut payload = Vec::new();
        for rle in [RLE_EMPTY_PLANE, RLE_EMPTY_PLANE, RLE_EMPTY_PLANE] {
            let len = rle.len() as u16;
            payload.push((len >> 8) as u8);
            payload.push(len as u8);
            payload.extend_from_slice(rle);
        }
        let parsed = prevalidate_cdf53(&payload, 0, 7).expect("valid pass payload");
        assert_eq!(parsed.present_passes, None);
    }
}
