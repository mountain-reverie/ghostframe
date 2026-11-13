// SPDX-License-Identifier: MIT OR Apache-2.0
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
use ghostframe_protocol::codec::cdf53::{rle_decode_into, rle_expanded_len};

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

/// What [`validate_cdf53`] learned about a pass without decoding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cdf53WireInfo {
    /// As [`PrevalidatedCdf53::present_passes`].
    pub present_passes: Option<u16>,
    /// The three framed channel blocks within the payload: pass 0's bitmap
    /// prefix and anything after channel 3 are outside it. Pass this slice to
    /// [`decode_cdf53_wire`].
    pub streams: std::ops::Range<usize>,
}

/// The pass-level checks shared by every entry point: `pass_idx`, pass 0's
/// bitmap prefix. Returns the bitmap and the offset of the channel blocks.
fn parse_prefix(payload: &[u8], pass_idx: u8) -> Result<(Option<u16>, usize), DecodeErrorCode> {
    if pass_idx >= 14 {
        return Err(DecodeErrorCode::Cdf53BadPass);
    }
    if pass_idx != 0 {
        return Ok((None, 0));
    }
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
    Ok((Some(bitmap), 2))
}

/// Walk the three `[u16 BE len][rle]` channel blocks starting at `offset`,
/// calling `each(channel, rle)` for each, in order. Returns the offset past
/// the third block. The error order is the wire order: a channel's framing is
/// checked, then its stream, before the next channel's framing.
fn walk_channels(
    payload: &[u8],
    mut offset: usize,
    mut each: impl FnMut(usize, &[u8]) -> Result<(), DecodeErrorCode>,
) -> Result<usize, DecodeErrorCode> {
    for ch in 0..3usize {
        if offset + 2 > payload.len() {
            return Err(DecodeErrorCode::Cdf53Truncated);
        }
        let len = ((payload[offset] as usize) << 8) | (payload[offset + 1] as usize);
        offset += 2;
        if offset + len > payload.len() {
            return Err(DecodeErrorCode::Cdf53Truncated);
        }
        each(ch, &payload[offset..offset + len])?;
        offset += len;
    }
    Ok(offset)
}

/// Accept or reject a pass exactly as [`prevalidate_cdf53`] does -- same
/// errors, same trailing-bytes tolerance -- without producing the planes:
/// no output buffer, no allocation. For a thread that only has to decide
/// whether to ACK; [`decode_cdf53_wire`] does the expansion elsewhere.
pub fn validate_cdf53(payload: &[u8], pass_idx: u8) -> Result<Cdf53WireInfo, DecodeErrorCode> {
    let (present_passes, start) = parse_prefix(payload, pass_idx)?;
    let end = walk_channels(payload, start, |_, rle| {
        if rle_expanded_len(rle) != 128 {
            return Err(DecodeErrorCode::Cdf53RleLength);
        }
        Ok(())
    })?;
    Ok(Cdf53WireInfo {
        present_passes,
        streams: start..end,
    })
}

/// Expand the `streams` of a pass that [`validate_cdf53`] accepted into the
/// 384-byte planes (B, G, R, 128 each). Fails on exactly what validation
/// fails on, so for a slice taken from it, never.
pub fn decode_cdf53_wire(streams: &[u8], planes: &mut [u8; 384]) -> Result<(), DecodeErrorCode> {
    walk_channels(streams, 0, |ch, rle| {
        let dst: &mut [u8; 128] = (&mut planes[ch * 128..ch * 128 + 128])
            .try_into()
            .expect("128-byte channel slice");
        if rle_decode_into(rle, dst) != 128 {
            return Err(DecodeErrorCode::Cdf53RleLength);
        }
        Ok(())
    })?;
    Ok(())
}

pub fn prevalidate_cdf53(
    payload: &[u8],
    generation: u8,
    pass_idx: u8,
) -> Result<PrevalidatedCdf53, DecodeErrorCode> {
    let (present_passes, start) = parse_prefix(payload, pass_idx)?;
    let mut bit_planes = vec![0u8; 384];
    walk_channels(payload, start, |ch, rle| {
        let dst: &mut [u8; 128] = (&mut bit_planes[ch * 128..ch * 128 + 128])
            .try_into()
            .expect("128-byte channel slice");
        if rle_decode_into(rle, dst) != 128 {
            return Err(DecodeErrorCode::Cdf53RleLength);
        }
        Ok(())
    })?;

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

    /// The implementation these functions replaced, verbatim, as the oracle.
    fn legacy(payload: &[u8], pass_idx: u8) -> Result<(Vec<u8>, Option<u16>), DecodeErrorCode> {
        use ghostframe_protocol::codec::cdf53::rle_decode;
        if pass_idx >= 14 {
            return Err(DecodeErrorCode::Cdf53BadPass);
        }
        let mut offset = 0usize;
        let mut present_passes = None;
        if pass_idx == 0 {
            if payload.len() < 2 {
                return Err(DecodeErrorCode::Cdf53Truncated);
            }
            let bitmap = ((payload[0] as u16) << 8) | (payload[1] as u16);
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
        Ok((bit_planes, present_passes))
    }

    /// All three entry points against the oracle: same verdict, same error,
    /// same planes, and the wire route reproduces the planes.
    fn check_all(payload: &[u8], pass_idx: u8) {
        let want = legacy(payload, pass_idx);
        let pre = prevalidate_cdf53(payload, 5, pass_idx);
        let val = validate_cdf53(payload, pass_idx);
        match &want {
            Err(e) => {
                assert_eq!(pre.as_ref().err(), Some(e), "prevalidate error");
                assert_eq!(val.as_ref().err(), Some(e), "validate error");
            }
            Ok((planes, pp)) => {
                let pre = pre.expect("prevalidate accepts");
                assert_eq!(&pre.bit_planes, planes);
                assert_eq!(pre.present_passes, *pp);
                let info = val.expect("validate accepts");
                assert_eq!(info.present_passes, *pp);
                let mut got = [0u8; 384];
                decode_cdf53_wire(&payload[info.streams.clone()], &mut got).expect("wire decode");
                assert_eq!(&got[..], &planes[..]);
            }
        }
    }

    fn framed(streams: [&[u8]; 3]) -> Vec<u8> {
        let mut v = Vec::new();
        for s in streams {
            v.extend_from_slice(&(s.len() as u16).to_be_bytes());
            v.extend_from_slice(s);
        }
        v
    }

    #[test]
    fn named_edge_cases_agree_with_the_oracle() {
        let z = RLE_EMPTY_PLANE;
        let good = framed([z, z, z]);
        let mut p0 = vec![0, 1];
        p0.extend_from_slice(&good);
        let mut trailing = good.clone();
        trailing.extend_from_slice(&[1, 2, 3]);
        let mut p0_trailing = p0.clone();
        p0_trailing.extend_from_slice(&[9; 5]);
        let cases: Vec<(Vec<u8>, u8)> = vec![
            (good.clone(), 1),
            (good.clone(), 13),
            (good.clone(), 14),
            (good.clone(), 255),
            (trailing, 3),
            (p0.clone(), 0),
            (p0_trailing, 0),
            (good.clone(), 0), // pass 0 without prefix
            (vec![0, 0].into_iter().chain(good.clone()).collect(), 0), // bit 0 clear
            (vec![], 0),
            (vec![], 4),
            (vec![0], 0),
            (framed([z, z, &[0x7F]]), 2), // lone escape after 128: accepted
            (framed([z, &[0x7F, 0x90], z]), 2), // 1-byte stream, short
            (framed([z, z, &[0xFF, 0x00]]), 2), // overrun
            (framed([z, z, &[0xFE]]), 2), // 127 bytes
            (framed([z, z, &[]]), 2),     // zero-length channel
            (framed([&[], &[], &[]]), 2),
            (vec![0, 5, 1, 2], 2),       // length past the end
            (vec![0, 1, 0xFF, 0, 1], 2), // channel 2 header cut
        ];
        for (p, idx) in cases {
            check_all(&p, idx);
        }
        // The trailing lone escape really is accepted (kept as before).
        assert!(validate_cdf53(&framed([z, z, &[0xFF, 0x7F]]), 2).is_ok());
        assert!(validate_cdf53(&framed([z, z, &[0xFE]]), 2).is_err());
    }

    mod prop {
        use super::*;
        use proptest::prelude::*;

        fn stream() -> impl Strategy<Value = Vec<u8>> {
            prop_oneof![
                // Mostly-valid: tokens whose lengths sum near 128.
                3 => proptest::collection::vec(
                    prop_oneof![Just(0xFFu8), Just(0x80), Just(0x7F), 0u8..0x7F, any::<u8>()],
                    0..140),
                1 => proptest::collection::vec(any::<u8>(), 0..300),
                2 => Just(vec![0xFFu8]),
            ]
        }

        proptest! {
            #[test]
            fn random_payloads(
                payload in proptest::collection::vec(any::<u8>(), 0..600),
                idx in 0u8..20,
            ) {
                check_all(&payload, idx);
            }

            #[test]
            fn structured_and_mutated_payloads(
                s in [stream(), stream(), stream()],
                prefix in prop_oneof![Just(0x0001u16), Just(0x0000), any::<u16>(), Just(0x3FFF)],
                idx in prop_oneof![Just(0u8), 1u8..14, 14u8..40],
                trailing in proptest::collection::vec(any::<u8>(), 0..4),
                cut in proptest::option::of(0usize..700),
                flip in proptest::option::of((0usize..700, any::<u8>())),
            ) {
                let mut p = Vec::new();
                if idx == 0 || prefix % 2 == 0 {
                    p.extend_from_slice(&prefix.to_be_bytes());
                }
                p.extend_from_slice(&framed([&s[0], &s[1], &s[2]]));
                p.extend_from_slice(&trailing);
                if let Some((at, v)) = flip { if at < p.len() { p[at] ^= v; } }
                if let Some(c) = cut { p.truncate(c.min(p.len())); }
                check_all(&p, idx);
            }
        }
    }
}
