//! Server -> client frames (rev 6 §6.1): SOURCE, REPAIR and PLAIN.
//!
//! All integers are little-endian. `parse` checks the header only; the
//! records inside a frame are walked (and checked) by [`records`].

pub const FEC_FRAME_SOURCE: u8 = 0x01;
pub const FEC_FRAME_REPAIR: u8 = 0x02;
pub const FEC_FRAME_PLAIN: u8 = 0x03;
pub const FEC_SCHEME_RS_CAUCHY: u8 = 0;
pub const FEC_FLAG_LAST: u8 = 0x80;
pub const FEC_SOURCE_HEADER_LEN: usize = 9;
pub const FEC_REPAIR_HEADER_LEN: usize = 11;
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
    pub bundle_seq: u32,
    pub block_id: u16,
    pub idx: u8,
    pub records: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairFrame<'a> {
    pub bundle_seq: u32,
    pub block_id: u16,
    pub k: u8,
    pub r: u8,
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
    #[error("reserved bits set in flags byte {0:#04x}")]
    ReservedBits(u8),
    #[error("bad block shape: k={k} r={r} repair_idx={repair_idx}")]
    BadBlockShape { k: u8, r: u8, repair_idx: u8 },
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
    let Some(&kind) = dg.first() else {
        return Err(FrameError::Empty);
    };
    let need = match kind {
        FEC_FRAME_SOURCE => FEC_SOURCE_HEADER_LEN,
        FEC_FRAME_REPAIR => FEC_REPAIR_HEADER_LEN,
        FEC_FRAME_PLAIN => FEC_PLAIN_HEADER_LEN,
        other => return Err(FrameError::UnknownKind(other)),
    };
    if dg.len() < need {
        return Err(FrameError::Truncated {
            need,
            got: dg.len(),
        });
    }
    match kind {
        FEC_FRAME_SOURCE => {
            let flags = dg[1];
            let scheme = flags & 0x0f;
            if scheme != FEC_SCHEME_RS_CAUCHY {
                return Err(FrameError::UnknownScheme(scheme));
            }
            if flags & !(FEC_FLAG_LAST | 0x0f) != 0 {
                return Err(FrameError::ReservedBits(flags));
            }
            Ok(Frame::Source(SourceFrame {
                last: flags & FEC_FLAG_LAST != 0,
                bundle_seq: u32_at(dg, 2),
                block_id: u16::from_le_bytes([dg[6], dg[7]]),
                idx: dg[8],
                records: &dg[FEC_SOURCE_HEADER_LEN..],
            }))
        }
        FEC_FRAME_REPAIR => {
            let scheme = dg[1];
            if scheme != FEC_SCHEME_RS_CAUCHY {
                return Err(FrameError::UnknownScheme(scheme));
            }
            let (k, r, repair_idx) = (dg[8], dg[9], dg[10]);
            if k == 0
                || r == 0
                || usize::from(k) + usize::from(r) > 256
                || usize::from(k) + usize::from(repair_idx) > 255
            {
                return Err(FrameError::BadBlockShape { k, r, repair_idx });
            }
            Ok(Frame::Repair(RepairFrame {
                bundle_seq: u32_at(dg, 2),
                block_id: u16::from_le_bytes([dg[6], dg[7]]),
                k,
                r,
                repair_idx,
                payload: &dg[FEC_REPAIR_HEADER_LEN..],
            }))
        }
        // `kind` was matched above; PLAIN is the remaining frame kind.
        _ => Ok(Frame::Plain(PlainFrame {
            bundle_seq: u32_at(dg, 1),
            records: &dg[FEC_PLAIN_HEADER_LEN..],
        })),
    }
}

impl SourceFrame<'_> {
    pub fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(FEC_SOURCE_HEADER_LEN + self.records.len());
        v.push(FEC_FRAME_SOURCE);
        v.push(FEC_SCHEME_RS_CAUCHY | if self.last { FEC_FLAG_LAST } else { 0 });
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
        let mut v = Vec::with_capacity(FEC_REPAIR_HEADER_LEN + self.payload.len());
        v.push(FEC_FRAME_REPAIR);
        v.push(FEC_SCHEME_RS_CAUCHY);
        v.extend_from_slice(&self.bundle_seq.to_le_bytes());
        v.extend_from_slice(&self.block_id.to_le_bytes());
        v.extend_from_slice(&[self.k, self.r, self.repair_idx]);
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

    #[test]
    fn source_roundtrip_and_layout() {
        let r = recs(&[b"abc", b""]);
        let f = SourceFrame {
            last: true,
            bundle_seq: 0x0403_0201,
            block_id: 0x0605,
            idx: 7,
            records: &r,
        };
        let dg = f.encode();
        assert_eq!(&dg[..9], &[0x01, 0x80, 1, 2, 3, 4, 5, 6, 7]);
        assert_eq!(parse(&dg), Ok(Frame::Source(f)));
        let items: Vec<&[u8]> = records(f.records).map(Result::unwrap).collect();
        assert_eq!(items, vec![&b"abc"[..], &b""[..]]);
    }

    #[test]
    fn repair_roundtrip_and_layout() {
        let p = [9u8; 20];
        let f = RepairFrame {
            bundle_seq: 77,
            block_id: 3,
            k: 10,
            r: 2,
            repair_idx: 5,
            payload: &p,
        };
        let dg = f.encode();
        assert_eq!(&dg[..11], &[0x02, 0x00, 77, 0, 0, 0, 3, 0, 10, 2, 5]);
        assert_eq!(parse(&dg), Ok(Frame::Repair(f)));
    }

    #[test]
    fn plain_roundtrip_and_layout() {
        let r = recs(&[b"pong-0000"]);
        let f = PlainFrame {
            bundle_seq: 1,
            records: &r,
        };
        let dg = f.encode();
        assert_eq!(&dg[..5], &[0x03, 1, 0, 0, 0]);
        assert_eq!(parse(&dg), Ok(Frame::Plain(f)));
    }

    #[test]
    fn coded_symbol_roundtrip_through_padding() {
        let r = recs(&[b"tile bytes", b"frag"]);
        let f = SourceFrame {
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
            last: false,
            bundle_seq: 0,
            block_id: 0,
            idx: 0,
            records: &r,
        };
        assert!(src.encode().len() <= max);
        let payload = src.coded_symbol();
        let rep = RepairFrame {
            bundle_seq: 0,
            block_id: 0,
            k: 1,
            r: 1,
            repair_idx: 0,
            payload: &payload,
        };
        assert_eq!(rep.encode().len(), max);
    }

    #[test]
    fn rejects() {
        assert_eq!(parse(&[]), Err(FrameError::Empty));
        assert_eq!(parse(&[0x04, 0, 0]), Err(FrameError::UnknownKind(0x04)));
        assert_eq!(
            parse(&[0x01, 0, 0]),
            Err(FrameError::Truncated { need: 9, got: 3 })
        );
        assert_eq!(
            parse(&[0x01, 0x01, 0, 0, 0, 0, 0, 0, 0]),
            Err(FrameError::UnknownScheme(1))
        );
        assert_eq!(
            parse(&[0x01, 0x40, 0, 0, 0, 0, 0, 0, 0]),
            Err(FrameError::ReservedBits(0x40))
        );
        let bad = |k, r, i| {
            RepairFrame {
                bundle_seq: 0,
                block_id: 0,
                k,
                r,
                repair_idx: i,
                payload: &[],
            }
            .encode()
        };
        assert!(matches!(
            parse(&bad(0, 1, 0)),
            Err(FrameError::BadBlockShape { .. })
        ));
        assert!(matches!(
            parse(&bad(1, 0, 0)),
            Err(FrameError::BadBlockShape { .. })
        ));
        assert!(matches!(
            parse(&bad(250, 7, 0)),
            Err(FrameError::BadBlockShape { .. })
        ));
        assert!(matches!(
            parse(&bad(250, 6, 6)),
            Err(FrameError::BadBlockShape { .. })
        ));
        assert!(parse(&bad(250, 6, 5)).is_ok());
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

    proptest! {
        #[test]
        fn parse_never_panics(dg in proptest::collection::vec(any::<u8>(), 0..1300)) {
            if let Ok(Frame::Source(SourceFrame { records: r, .. }) | Frame::Plain(PlainFrame { records: r, .. })) = parse(&dg) {
                for item in records(r) { let _ = item; }
            }
        }

        #[test]
        fn source_roundtrip(seq in any::<u32>(), block in any::<u16>(), idx in any::<u8>(), last in any::<bool>(),
                            items in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..300), 0..5)) {
            let mut r = Vec::new();
            for i in &items { push_record(&mut r, i); }
            let f = SourceFrame { last, bundle_seq: seq, block_id: block, idx, records: &r };
            let dg = f.encode();
            prop_assert_eq!(parse(&dg), Ok(Frame::Source(f)));
            let got: Vec<Vec<u8>> = records(&r).map(|x| x.unwrap().to_vec()).collect();
            prop_assert_eq!(got, items);
        }
    }
}
