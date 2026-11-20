// SPDX-License-Identifier: MIT OR Apache-2.0
//! Server -> client frames: SOURCE, REPAIR and PLAIN. The header layout is
//! rev 7 (`docs/specs/fec-v2/fec-v2-rev7.md`), frozen; the rest is rev 6.
//!
//! All integers are little-endian. `parse` checks the header only; the
//! records inside a frame are walked (and checked) by [`records`].

/// Byte 0 bits 7-6: the frame kind. Kind 00 is reserved and rejected: it is
/// the escape for any later format (rev 7, a frozen header).
pub const FEC_KIND_MASK: u8 = 0xc0;
/// Kind 01. Byte 0 also holds LAST (bit 5), `t_adv` (bits 4-2) and the scheme (bits 1-0).
pub const FEC_FRAME_SOURCE: u8 = 0x40;
/// Kind 10. Byte 0 also holds the RTT hint code (bits 5-2) and the scheme (bits 1-0).
pub const FEC_FRAME_REPAIR: u8 = 0x80;
/// Kind 11. Bits 5-0 are reserved and must be zero.
pub const FEC_FRAME_PLAIN: u8 = 0xc0;
pub const FEC_SCHEME_RS_CAUCHY: u8 = 0;
/// SOURCE byte 0 bit 5: the last source of its block.
pub const FEC_FLAG_LAST: u8 = 0x20;
/// SOURCE byte 0 bits 4-2: the block's advertised span code (`timing::t_adv_us`).
const T_ADV_SHIFT: u32 = 2;
const T_ADV_MASK: u8 = 0x07;
/// REPAIR byte 0 bits 5-2: the RTT hint code.
const RTT_HINT_SHIFT: u32 = 2;
const RTT_HINT_MASK: u8 = 0x0f;
/// SOURCE and REPAIR byte 0 bits 1-0: the scheme.
const SCHEME_MASK: u8 = 0x03;
/// PLAIN byte 0 bits 5-0.
const PLAIN_RESERVED_MASK: u8 = 0x3f;
pub const FEC_SOURCE_HEADER_LEN: usize = 8;
pub const FEC_REPAIR_HEADER_LEN: usize = 9;
pub const FEC_PLAIN_HEADER_LEN: usize = 5;
pub const FEC_RECORD_LEN_PREFIX: usize = 2;
/// len u16 + bundle_seq u32 in front of the records in a coded symbol.
pub const FEC_SYMBOL_OVERHEAD: usize = 6;
/// A bundle's record bytes must be <= max_datagram - FEC_RECORDS_HEADROOM so the
/// block's repair (FEC_REPAIR_HEADER_LEN + FEC_SYMBOL_OVERHEAD + records) still fits.
pub const FEC_RECORDS_HEADROOM: usize = FEC_REPAIR_HEADER_LEN + FEC_SYMBOL_OVERHEAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceFrame<'a> {
    pub last: bool,
    /// Advertised span code, 0..=7 (0: nothing advertised); the same in every source of a block.
    pub t_adv: u8,
    pub bundle_seq: u32,
    pub block_id: u16,
    pub idx: u8,
    pub records: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairFrame<'a> {
    /// Round-trip hint code, 0..=15 (0: unknown).
    pub rtt_hint: u8,
    pub bundle_seq: u32,
    pub block_id: u16,
    pub k: u8,
    pub repair_idx: u8,
    pub payload: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlainFrame<'a> {
    pub bundle_seq: u32,
    pub records: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frame<'a> {
    Source(SourceFrame<'a>),
    Repair(RepairFrame<'a>),
    Plain(PlainFrame<'a>),
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FrameError {
    #[error("empty datagram")]
    Empty,
    #[error("unknown frame kind {0:#04x}")]
    UnknownKind(u8),
    #[error("frame truncated: need {need} bytes, got {got}")]
    Truncated { need: usize, got: usize },
    #[error("unknown FEC scheme {0}")]
    UnknownScheme(u8),
    #[error(
        "bad block shape: k={k} repair_idx={repair_idx} (need k >= 1 and k + repair_idx <= 255)"
    )]
    BadBlockShape { k: u8, repair_idx: u8 },
    #[error("PLAIN frame with reserved bits set in byte 0 ({0:#04x})")]
    PlainReservedBits(u8),
    #[error("record at offset {at} runs past the end of the frame")]
    RecordOverrun { at: usize },
    #[error("coded symbol of {0} bytes is shorter than its header")]
    SymbolTooShort(usize),
    #[error("coded symbol declares a length of {0}, below its 4-byte minimum")]
    SymbolDeclaredTooShort(usize),
    #[error("coded symbol declares {declared} bytes but holds {available}")]
    SymbolLength { declared: usize, available: usize },
}

fn u32_at(b: &[u8], i: usize) -> u32 {
    u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]])
}

/// Splits a datagram into its frame. Does not look inside the records.
pub fn parse(dg: &[u8]) -> Result<Frame<'_>, FrameError> {
    let Some(&b0) = dg.first() else {
        return Err(FrameError::Empty);
    };
    let kind = b0 & FEC_KIND_MASK;
    let need = match kind {
        FEC_FRAME_SOURCE => FEC_SOURCE_HEADER_LEN,
        FEC_FRAME_REPAIR => FEC_REPAIR_HEADER_LEN,
        FEC_FRAME_PLAIN => FEC_PLAIN_HEADER_LEN,
        // Kind 00, which every pre-rev-7 kind byte (0x01..=0x03) falls into.
        _ => return Err(FrameError::UnknownKind(b0)),
    };
    if dg.len() < need {
        return Err(FrameError::Truncated {
            need,
            got: dg.len(),
        });
    }
    match kind {
        FEC_FRAME_SOURCE => {
            let scheme = b0 & SCHEME_MASK;
            if scheme != FEC_SCHEME_RS_CAUCHY {
                return Err(FrameError::UnknownScheme(scheme));
            }
            Ok(Frame::Source(SourceFrame {
                last: b0 & FEC_FLAG_LAST != 0,
                t_adv: (b0 >> T_ADV_SHIFT) & T_ADV_MASK,
                bundle_seq: u32_at(dg, 1),
                block_id: u16::from_le_bytes([dg[5], dg[6]]),
                idx: dg[7],
                records: &dg[FEC_SOURCE_HEADER_LEN..],
            }))
        }
        FEC_FRAME_REPAIR => {
            let scheme = b0 & SCHEME_MASK;
            if scheme != FEC_SCHEME_RS_CAUCHY {
                return Err(FrameError::UnknownScheme(scheme));
            }
            let (k, repair_idx) = (dg[7], dg[8]);
            if k == 0 || usize::from(k) + usize::from(repair_idx) > 255 {
                return Err(FrameError::BadBlockShape { k, repair_idx });
            }
            Ok(Frame::Repair(RepairFrame {
                rtt_hint: (b0 >> RTT_HINT_SHIFT) & RTT_HINT_MASK,
                bundle_seq: u32_at(dg, 1),
                block_id: u16::from_le_bytes([dg[5], dg[6]]),
                k,
                repair_idx,
                payload: &dg[FEC_REPAIR_HEADER_LEN..],
            }))
        }
        // `kind` was matched above; PLAIN is the remaining frame kind.
        _ => {
            if b0 & PLAIN_RESERVED_MASK != 0 {
                return Err(FrameError::PlainReservedBits(b0));
            }
            Ok(Frame::Plain(PlainFrame {
                bundle_seq: u32_at(dg, 1),
                records: &dg[FEC_PLAIN_HEADER_LEN..],
            }))
        }
    }
}

impl SourceFrame<'_> {
    pub fn encode(&self) -> Vec<u8> {
        // A code that does not fit its bits would put another value on the
        // wire than the server's own deadlines use.
        debug_assert!(self.t_adv <= T_ADV_MASK, "t_adv code {}", self.t_adv);
        let mut v = Vec::with_capacity(FEC_SOURCE_HEADER_LEN + self.records.len());
        v.push(
            FEC_FRAME_SOURCE
                | ((self.t_adv & T_ADV_MASK) << T_ADV_SHIFT)
                | if self.last { FEC_FLAG_LAST } else { 0 }
                | FEC_SCHEME_RS_CAUCHY,
        );
        v.extend_from_slice(&self.bundle_seq.to_le_bytes());
        v.extend_from_slice(&self.block_id.to_le_bytes());
        v.push(self.idx);
        v.extend_from_slice(self.records);
        v
    }

    /// `len u16 | bundle_seq u32 | records`, unpadded (the codec pads).
    pub fn coded_symbol(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FEC_SYMBOL_OVERHEAD + self.records.len());
        v.extend_from_slice(
            &u16::try_from(4 + self.records.len())
                .expect("records fit a u16 length")
                .to_le_bytes(),
        );
        v.extend_from_slice(&self.bundle_seq.to_le_bytes());
        v.extend_from_slice(self.records);
        v
    }
}

impl RepairFrame<'_> {
    pub fn encode(&self) -> Vec<u8> {
        debug_assert!(
            self.rtt_hint <= RTT_HINT_MASK,
            "rtt hint code {}",
            self.rtt_hint
        );
        let mut v = Vec::with_capacity(FEC_REPAIR_HEADER_LEN + self.payload.len());
        v.push(
            FEC_FRAME_REPAIR
                | ((self.rtt_hint & RTT_HINT_MASK) << RTT_HINT_SHIFT)
                | FEC_SCHEME_RS_CAUCHY,
        );
        v.extend_from_slice(&self.bundle_seq.to_le_bytes());
        v.extend_from_slice(&self.block_id.to_le_bytes());
        v.extend_from_slice(&[self.k, self.repair_idx]);
        v.extend_from_slice(self.payload);
        v
    }
}

impl PlainFrame<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FEC_PLAIN_HEADER_LEN + self.records.len());
        v.push(FEC_FRAME_PLAIN);
        v.extend_from_slice(&self.bundle_seq.to_le_bytes());
        v.extend_from_slice(self.records);
        v
    }
}

/// Records of a rebuilt source: reads the len prefix, ignores the zero padding.
pub fn source_from_symbol(sym: &[u8]) -> Result<(u32, &[u8]), FrameError> {
    if sym.len() < FEC_SYMBOL_OVERHEAD {
        return Err(FrameError::SymbolTooShort(sym.len()));
    }
    let declared = usize::from(u16::from_le_bytes([sym[0], sym[1]]));
    if declared < 4 {
        return Err(FrameError::SymbolDeclaredTooShort(declared));
    }
    let available = sym.len() - FEC_RECORD_LEN_PREFIX;
    if declared > available {
        return Err(FrameError::SymbolLength {
            declared,
            available,
        });
    }
    Ok((
        u32_at(sym, 2),
        &sym[FEC_SYMBOL_OVERHEAD..FEC_RECORD_LEN_PREFIX + declared],
    ))
}

/// Appends `{len u16, bytes}`. Panics if `bytes.len() > u16::MAX` (callers size records to a datagram).
pub fn push_record(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u16::try_from(bytes.len()).expect("record longer than u16::MAX");
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(bytes);
}

/// Walks `{len u16, bytes}` records; stops after the first error.
pub fn records(bytes: &[u8]) -> Records<'_> {
    Records {
        rest: bytes,
        offset: 0,
        done: false,
    }
}

pub struct Records<'a> {
    rest: &'a [u8],
    offset: usize,
    done: bool,
}

impl<'a> Iterator for Records<'a> {
    type Item = Result<&'a [u8], FrameError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.rest.is_empty() {
            return None;
        }
        let at = self.offset;
        let len = match self.rest {
            [a, b, ..] => usize::from(u16::from_le_bytes([*a, *b])),
            _ => usize::MAX,
        };
        let end = FEC_RECORD_LEN_PREFIX.saturating_add(len);
        if end > self.rest.len() {
            self.done = true;
            return Some(Err(FrameError::RecordOverrun { at }));
        }
        let rec = &self.rest[FEC_RECORD_LEN_PREFIX..end];
        self.rest = &self.rest[end..];
        self.offset += end;
        Some(Ok(rec))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn recs(items: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for i in items {
            push_record(&mut v, i);
        }
        v
    }

    /// 0x74 = kind 01 | LAST 1 | t_adv 101 | scheme 00.
    #[test]
    fn golden_source() {
        let r = recs(&[b"ab"]);
        let f = SourceFrame {
            last: true,
            t_adv: 5,
            bundle_seq: 0x0403_0201,
            block_id: 0x0605,
            idx: 7,
            records: &r,
        };
        let want = vec![0x74, 1, 2, 3, 4, 5, 6, 7, 2, 0, b'a', b'b'];
        assert_eq!(f.encode(), want);
        assert_eq!(parse(&want), Ok(Frame::Source(f)));
        let items: Vec<&[u8]> = records(f.records).map(Result::unwrap).collect();
        assert_eq!(items, vec![&b"ab"[..]]);
    }

    /// 0xa8 = kind 10 | rtt hint 1010 | scheme 00.
    #[test]
    fn golden_repair() {
        let f = RepairFrame {
            rtt_hint: 10,
            bundle_seq: 0x0807_0605,
            block_id: 0x0a09,
            k: 0x0b,
            repair_idx: 0x0c,
            payload: &[0xde, 0xad],
        };
        let want = vec![0xa8, 5, 6, 7, 8, 9, 0x0a, 0x0b, 0x0c, 0xde, 0xad];
        assert_eq!(f.encode(), want);
        assert_eq!(parse(&want), Ok(Frame::Repair(f)));
    }

    /// 0xc0 = kind 11 | reserved 000000.
    #[test]
    fn golden_plain() {
        let r = recs(&[b"hi"]);
        let f = PlainFrame {
            bundle_seq: 0x0d0c_0b0a,
            records: &r,
        };
        let want = vec![0xc0, 0x0a, 0x0b, 0x0c, 0x0d, 2, 0, b'h', b'i'];
        assert_eq!(f.encode(), want);
        assert_eq!(parse(&want), Ok(Frame::Plain(f)));
    }

    #[test]
    fn header_lengths_are_the_rev7_sizes() {
        assert_eq!(FEC_SOURCE_HEADER_LEN, 8);
        assert_eq!(FEC_REPAIR_HEADER_LEN, 9);
        assert_eq!(FEC_PLAIN_HEADER_LEN, 5);
        assert_eq!(FEC_RECORDS_HEADROOM, 15);
    }

    #[test]
    fn coded_symbol_roundtrip_through_padding() {
        let r = recs(&[b"tile bytes", b"frag"]);
        let f = SourceFrame {
            t_adv: 0,
            last: false,
            bundle_seq: 42,
            block_id: 1,
            idx: 0,
            records: &r,
        };
        let mut sym = f.coded_symbol();
        assert_eq!(sym.len(), FEC_SYMBOL_OVERHEAD + r.len());
        sym.resize(1180, 0); // the codec pads to the block's longest symbol
        assert_eq!(source_from_symbol(&sym), Ok((42, &r[..])));
    }

    #[test]
    fn repair_fits_when_records_respect_the_headroom() {
        let max = 1200usize;
        let r = vec![0u8; max - FEC_RECORDS_HEADROOM];
        let src = SourceFrame {
            t_adv: 0,
            last: false,
            bundle_seq: 0,
            block_id: 0,
            idx: 0,
            records: &r,
        };
        assert!(src.encode().len() <= max);
        let payload = src.coded_symbol();
        let rep = RepairFrame {
            rtt_hint: 0,
            bundle_seq: 0,
            block_id: 0,
            k: 1,
            repair_idx: 0,
            payload: &payload,
        };
        assert_eq!(rep.encode().len(), max);
    }

    #[test]
    fn every_pre_rev7_kind_byte_is_rejected_as_kind_00() {
        for old in [0x01u8, 0x02, 0x03] {
            // Long enough for any header, so only the kind can be the reason.
            let mut dg = vec![old];
            dg.extend_from_slice(&[0u8; 16]);
            assert_eq!(parse(&dg), Err(FrameError::UnknownKind(old)));
        }
        // Kind 00 with any low bits.
        for b0 in 0..0x40u8 {
            assert_eq!(parse(&[b0; 16]), Err(FrameError::UnknownKind(b0)));
        }
    }

    #[test]
    fn rejects_empty_and_truncated() {
        assert_eq!(parse(&[]), Err(FrameError::Empty));
        for (b0, need) in [
            (FEC_FRAME_SOURCE, FEC_SOURCE_HEADER_LEN),
            (FEC_FRAME_REPAIR, FEC_REPAIR_HEADER_LEN),
            (FEC_FRAME_PLAIN, FEC_PLAIN_HEADER_LEN),
        ] {
            let mut dg = vec![0u8; need];
            dg[0] = b0;
            dg[7.min(need - 1)] = if b0 == FEC_FRAME_REPAIR { 1 } else { 0 };
            assert!(parse(&dg).is_ok(), "{b0:#04x} at its header length");
            assert_eq!(
                parse(&dg[..need - 1]),
                Err(FrameError::Truncated {
                    need,
                    got: need - 1
                })
            );
        }
    }

    #[test]
    fn rejects_a_scheme_other_than_zero() {
        for scheme in 1..4u8 {
            let mut src = [0u8; 8];
            src[0] = FEC_FRAME_SOURCE | scheme;
            assert_eq!(parse(&src), Err(FrameError::UnknownScheme(scheme)));
            let mut rep = [0u8; 9];
            rep[0] = FEC_FRAME_REPAIR | scheme;
            rep[7] = 1;
            assert_eq!(parse(&rep), Err(FrameError::UnknownScheme(scheme)));
            // Together with every hint / LAST / t_adv value.
            src[0] |= 0x3c;
            rep[0] |= 0x3c;
            assert_eq!(parse(&src), Err(FrameError::UnknownScheme(scheme)));
            assert_eq!(parse(&rep), Err(FrameError::UnknownScheme(scheme)));
        }
    }

    #[test]
    fn rejects_plain_reserved_bits() {
        for bit in 0..6u8 {
            let b0 = FEC_FRAME_PLAIN | (1 << bit);
            assert_eq!(
                parse(&[b0, 0, 0, 0, 0]),
                Err(FrameError::PlainReservedBits(b0))
            );
        }
        assert!(parse(&[FEC_FRAME_PLAIN, 0, 0, 0, 0]).is_ok());
    }

    #[test]
    fn rejects_bad_repair_shapes() {
        let bad = |k, i| {
            RepairFrame {
                rtt_hint: 0,
                bundle_seq: 0,
                block_id: 0,
                k,
                repair_idx: i,
                payload: &[],
            }
            .encode()
        };
        assert_eq!(
            parse(&bad(0, 0)),
            Err(FrameError::BadBlockShape {
                k: 0,
                repair_idx: 0
            })
        );
        // k + repair_idx = 256
        assert!(matches!(
            parse(&bad(250, 6)),
            Err(FrameError::BadBlockShape { .. })
        ));
        assert!(matches!(
            parse(&bad(255, 1)),
            Err(FrameError::BadBlockShape { .. })
        ));
        // k + repair_idx = 255 is the last valid row
        assert!(parse(&bad(250, 5)).is_ok());
        assert!(parse(&bad(1, 254)).is_ok());
        assert!(parse(&bad(255, 0)).is_ok());
    }

    #[test]
    fn record_and_symbol_errors() {
        let mut over = Vec::new();
        over.extend_from_slice(&5u16.to_le_bytes());
        over.extend_from_slice(b"ab");
        assert_eq!(
            records(&over).next(),
            Some(Err(FrameError::RecordOverrun { at: 0 }))
        );
        assert_eq!(source_from_symbol(&[1]), Err(FrameError::SymbolTooShort(1)));
        assert_eq!(
            source_from_symbol(&[2, 0, 0, 0, 0, 0]),
            Err(FrameError::SymbolDeclaredTooShort(2))
        );
        assert_eq!(
            source_from_symbol(&[9, 0, 0, 0, 0, 0]),
            Err(FrameError::SymbolLength {
                declared: 9,
                available: 4
            })
        );
    }

    /// A source of `records` with the given code and last flag.
    fn src_with(t_adv: u8, last: bool, records: &[u8]) -> SourceFrame<'_> {
        SourceFrame {
            last,
            t_adv,
            bundle_seq: 5,
            block_id: 2,
            idx: 1,
            records,
        }
    }

    #[test]
    fn every_t_adv_and_last_round_trips_and_sits_in_byte_0() {
        for t_adv in 0..8u8 {
            for last in [false, true] {
                let f = src_with(t_adv, last, b"xy");
                let dg = f.encode();
                assert_eq!(dg[0], 0x40 | (t_adv << 2) | if last { 0x20 } else { 0 });
                assert_eq!(parse(&dg), Ok(Frame::Source(f)));
            }
        }
    }

    #[test]
    fn every_rtt_hint_round_trips_in_byte_0() {
        for rtt_hint in 0..16u8 {
            let f = RepairFrame {
                rtt_hint,
                bundle_seq: 1,
                block_id: 2,
                k: 3,
                repair_idx: 0,
                payload: b"pp",
            };
            let dg = f.encode();
            assert_eq!(dg[0], 0x80 | (rtt_hint << 2));
            assert_eq!(parse(&dg), Ok(Frame::Repair(f)));
        }
    }

    /// Every byte-0 value parses or is rejected for exactly its stated reason.
    #[test]
    fn every_byte_0_parses_or_is_rejected_by_rule() {
        for b0 in 0..=255u8 {
            let dg = [b0, 0, 0, 0, 0, 0, 0, 1, 0];
            match (b0 >> 6, parse(&dg)) {
                (0, Err(FrameError::UnknownKind(x))) => assert_eq!(x, b0),
                (1, Ok(Frame::Source(f))) => {
                    assert_eq!(b0 & 3, 0);
                    assert_eq!(f.last, b0 & 0x20 != 0);
                    assert_eq!(f.t_adv, (b0 >> 2) & 7);
                }
                (2, Ok(Frame::Repair(f))) => {
                    assert_eq!(b0 & 3, 0);
                    assert_eq!(f.rtt_hint, (b0 >> 2) & 15);
                }
                (1 | 2, Err(FrameError::UnknownScheme(s))) => assert_eq!(s, b0 & 3),
                (3, Ok(Frame::Plain(_))) => assert_eq!(b0 & 0x3f, 0),
                (3, Err(FrameError::PlainReservedBits(x))) => {
                    assert_eq!(x, b0);
                    assert_ne!(b0 & 0x3f, 0);
                }
                other => panic!("byte 0 {b0:#04x}: {other:?}"),
            }
        }
    }

    #[test]
    fn the_parity_does_not_depend_on_the_advertised_code_or_hint() {
        let a = src_with(0, false, b"records").coded_symbol();
        for t_adv in 1..8u8 {
            assert_eq!(src_with(t_adv, true, b"records").coded_symbol(), a);
        }
    }

    proptest! {
        #[test]
        fn parse_never_panics(dg in proptest::collection::vec(any::<u8>(), 0..1300)) {
            if let Ok(Frame::Source(SourceFrame { records: r, .. }) | Frame::Plain(PlainFrame { records: r, .. })) = parse(&dg) {
                for item in records(r) { let _ = item; }
            }
        }

        #[test]
        fn source_roundtrip(seq in any::<u32>(), block in any::<u16>(), idx in any::<u8>(), last in any::<bool>(), t_adv in 0u8..8,
                            items in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 0..5)) {
            let mut r = Vec::new();
            for i in &items { push_record(&mut r, i); }
            let f = SourceFrame { last, t_adv, bundle_seq: seq, block_id: block, idx, records: &r };
            let dg = f.encode();
            prop_assert_eq!(dg.len(), FEC_SOURCE_HEADER_LEN + r.len());
            prop_assert_eq!(parse(&dg), Ok(Frame::Source(f)));
            let got: Vec<Vec<u8>> = records(&r).map(|x| x.unwrap().to_vec()).collect();
            prop_assert_eq!(got, items);
        }

        #[test]
        fn repair_roundtrip(seq in any::<u32>(), block in any::<u16>(), rtt_hint in 0u8..16,
                            k in 1u8..=255, row in 0u8..=254,
                            payload in proptest::collection::vec(any::<u8>(), 0..64)) {
            let repair_idx = row.min(255 - k);
            let f = RepairFrame { rtt_hint, bundle_seq: seq, block_id: block, k, repair_idx, payload: &payload };
            let dg = f.encode();
            prop_assert_eq!(dg.len(), FEC_REPAIR_HEADER_LEN + payload.len());
            prop_assert_eq!(parse(&dg), Ok(Frame::Repair(f)));
        }

        #[test]
        fn plain_roundtrip(seq in any::<u32>(), body in proptest::collection::vec(any::<u8>(), 0..64)) {
            let f = PlainFrame { bundle_seq: seq, records: &body };
            let dg = f.encode();
            prop_assert_eq!(dg.len(), FEC_PLAIN_HEADER_LEN + body.len());
            prop_assert_eq!(parse(&dg), Ok(Frame::Plain(f)));
        }
    }
}
