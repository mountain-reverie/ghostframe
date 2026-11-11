//! Pure, sans-IO packer: inner datagrams ("records") into PLAIN bundles.

use ghostframe_protocol::fec::frame::{
    push_record, PlainFrame, FEC_PLAIN_HEADER_LEN, FEC_RECORDS_HEADROOM, FEC_RECORD_LEN_PREFIX,
};
use smallvec::SmallVec;

use crate::transport::protocol::NO_ACK;
use crate::transport::reliable_emitter::emission_queue::WireClass;
use crate::transport::reliable_emitter::Content;

/// What the ledger and billing need to know about one record in a bundle.
#[derive(Debug, Clone)]
pub(crate) struct RecordMeta {
    pub wire_seq: u32,
    pub content: Content,
    pub emit_us: u64,
    pub class: WireClass,
    pub is_pad: bool,
    /// The record's own length (the inner datagram), for billing.
    pub len: usize,
}

/// One finished bundle: the datagram bytes and the records it carries.
#[derive(Debug, Clone)]
pub(crate) struct SealedBundle {
    pub bundle_seq: u32,
    pub bytes: Vec<u8>,
    pub records: SmallVec<[RecordMeta; 4]>,
    /// Earliest emit time among `records` (0 when it has none).
    pub emit_us: u64,
}

#[derive(Debug, Default)]
pub(crate) struct Packer {
    next_seq: u32,
    /// `{len, bytes}` records of the open bundle, without the frame header.
    body: Vec<u8>,
    records: SmallVec<[RecordMeta; 4]>,
}

impl Packer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Bytes of the open bundle's records (its frame header not counted).
    pub(crate) fn open_bytes(&self) -> usize {
        self.body.len()
    }

    /// Longest inner datagram for a `max_dg` (plan decision 3).
    pub(crate) fn record_limit(max_dg: usize) -> usize {
        max_dg
            .saturating_sub(FEC_RECORDS_HEADROOM)
            .saturating_sub(FEC_RECORD_LEN_PREFIX)
    }

    fn alloc_seq(&mut self) -> u32 {
        if self.next_seq == NO_ACK {
            self.next_seq = 0;
        }
        let v = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        v
    }

    fn seal(&mut self) -> Option<SealedBundle> {
        if self.records.is_empty() {
            return None;
        }
        let body = std::mem::take(&mut self.body);
        let records = std::mem::take(&mut self.records);
        Some(self.finish(body, records))
    }

    fn finish(&mut self, body: Vec<u8>, records: SmallVec<[RecordMeta; 4]>) -> SealedBundle {
        let bundle_seq = self.alloc_seq();
        let bytes = PlainFrame {
            bundle_seq,
            records: &body,
        }
        .encode();
        let emit_us = records.iter().map(|r| r.emit_us).min().unwrap_or(0);
        SealedBundle {
            bundle_seq,
            bytes,
            records,
            emit_us,
        }
    }

    /// Appends one record. Returns the bundle(s) this sealed: the open one when
    /// `rec` did not fit, and for a pad also the pad's own lone bundle.
    pub(crate) fn push(
        &mut self,
        rec: &[u8],
        meta: RecordMeta,
        max_dg: usize,
    ) -> SmallVec<[SealedBundle; 2]> {
        let mut out = SmallVec::new();
        debug_assert!(
            rec.len() <= Self::record_limit(max_dg),
            "record of {} bytes exceeds record_limit {} for max_dg {}",
            rec.len(),
            Self::record_limit(max_dg),
            max_dg
        );
        // A pad is always alone: seal what is open, then seal the pad.
        if meta.is_pad {
            out.extend(self.seal());
            let mut body = Vec::with_capacity(rec.len() + FEC_RECORD_LEN_PREFIX);
            push_record(&mut body, rec);
            out.push(self.finish(body, SmallVec::from_elem(meta, 1)));
            return out;
        }
        let need = FEC_RECORD_LEN_PREFIX + rec.len();
        if !self.records.is_empty() && FEC_PLAIN_HEADER_LEN + self.body.len() + need > max_dg {
            out.extend(self.seal());
        }
        push_record(&mut self.body, rec);
        self.records.push(meta);
        // A caller-bug oversize record (debug_assert above) goes out alone.
        if FEC_PLAIN_HEADER_LEN + self.body.len() > max_dg {
            out.extend(self.seal());
        }
        out
    }

    /// Seals whatever is open.
    pub(crate) fn flush(&mut self) -> Option<SealedBundle> {
        self.seal()
    }

    /// Frames one control record (pong, eviction) alone, bypassing the open
    /// bundle: flushes first, then a lone PLAIN. The control record has no
    /// `RecordMeta`, so that bundle's `records` is empty.
    pub(crate) fn lone(&mut self, rec: &[u8], max_dg: usize) -> SmallVec<[SealedBundle; 2]> {
        debug_assert!(rec.len() <= Self::record_limit(max_dg));
        let mut out = SmallVec::new();
        out.extend(self.seal());
        let mut body = Vec::with_capacity(rec.len() + FEC_RECORD_LEN_PREFIX);
        push_record(&mut body, rec);
        out.push(self.finish(body, SmallVec::new()));
        out
    }

    /// Drops the open bundle and restarts the sequence (session reset).
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_protocol::fec::frame::{parse, records, Frame};
    use proptest::prelude::*;

    fn meta(ws: u32, emit_us: u64, is_pad: bool) -> RecordMeta {
        RecordMeta {
            wire_seq: ws,
            content: if is_pad {
                Content::Pad(ws)
            } else {
                Content::H264Frag {
                    frame_seq: ws,
                    frag_idx: 0,
                }
            },
            emit_us,
            class: WireClass::H264,
            is_pad,
            len: 0,
        }
    }

    fn recs_of(b: &SealedBundle) -> Vec<Vec<u8>> {
        match parse(&b.bytes).expect("plain frame") {
            Frame::Plain(p) => {
                assert_eq!(p.bundle_seq, b.bundle_seq);
                records(p.records).map(|r| r.unwrap().to_vec()).collect()
            }
            other => panic!("not plain: {other:?}"),
        }
    }

    fn rec(n: usize, tag: u8) -> Vec<u8> {
        vec![tag; n]
    }

    #[test]
    fn record_limit_leaves_room() {
        for max_dg in [0usize, 10, 30, 1200, 1400] {
            let l = Packer::record_limit(max_dg);
            if max_dg >= FEC_RECORDS_HEADROOM + FEC_RECORD_LEN_PREFIX {
                assert!(l + FEC_PLAIN_HEADER_LEN + FEC_RECORD_LEN_PREFIX <= max_dg);
                assert_eq!(l + FEC_RECORDS_HEADROOM + FEC_RECORD_LEN_PREFIX, max_dg);
            } else {
                assert_eq!(l, 0);
            }
        }
    }

    #[test]
    fn packs_in_order_and_seals_on_overflow() {
        let max_dg = 100;
        let mut p = Packer::new();
        let mut sealed = Vec::new();
        let mut sent = Vec::new();
        for i in 0..10u32 {
            let r = rec(30, i as u8);
            sent.push(r.clone());
            sealed.extend(p.push(&r, meta(i, 100 - i as u64, false), max_dg));
        }
        sealed.extend(p.flush());
        assert!(p.flush().is_none());
        assert!(sealed.len() > 1, "must have split");
        let mut got = Vec::new();
        for (i, b) in sealed.iter().enumerate() {
            assert!(b.bytes.len() <= max_dg);
            assert_eq!(b.bundle_seq, i as u32);
            assert_eq!(
                b.emit_us,
                b.records.iter().map(|r| r.emit_us).min().unwrap()
            );
            let rs = recs_of(b);
            assert_eq!(rs.len(), b.records.len());
            got.extend(rs);
        }
        assert_eq!(got, sent);
    }

    #[test]
    fn pad_is_alone() {
        let mut p = Packer::new();
        assert!(p.push(&rec(10, 1), meta(1, 5, false), 200).is_empty());
        assert!(p.push(&rec(10, 2), meta(2, 6, false), 200).is_empty());
        let out = p.push(&rec(50, 9), meta(3, 7, true), 200);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].records.len(), 2);
        assert_eq!(out[1].records.len(), 1);
        assert!(out[1].records[0].is_pad);
        assert_eq!(recs_of(&out[1]), vec![rec(50, 9)]);
        // Nothing left open; next record opens a fresh bundle.
        assert!(p.flush().is_none());
        assert!(p.push(&rec(10, 4), meta(4, 8, false), 200).is_empty());
        let b = p.flush().unwrap();
        assert_eq!(b.records.len(), 1);
        assert_eq!(b.bundle_seq, 2);
    }

    #[test]
    fn lone_flushes_then_frames_alone() {
        let mut p = Packer::new();
        p.push(&rec(10, 1), meta(1, 5, false), 200);
        let out = p.lone(&rec(8, 7), 200);
        assert_eq!(out.len(), 2);
        assert_eq!(recs_of(&out[0]), vec![rec(10, 1)]);
        assert!(out[1].records.is_empty());
        assert_eq!(recs_of(&out[1]), vec![rec(8, 7)]);
        assert_eq!(out[1].bundle_seq, out[0].bundle_seq + 1);
        // With nothing open, only the lone bundle comes back.
        assert_eq!(p.lone(&rec(8, 7), 200).len(), 1);
    }

    #[test]
    fn bundle_seq_skips_no_ack_and_reset_restarts() {
        let mut p = Packer::new();
        p.next_seq = NO_ACK - 1;
        let a = p.lone(&rec(4, 1), 100);
        let b = p.lone(&rec(4, 1), 100);
        let c = p.lone(&rec(4, 1), 100);
        assert_eq!(a[0].bundle_seq, NO_ACK - 1);
        assert_eq!(b[0].bundle_seq, 0, "NO_ACK skipped");
        assert_eq!(c[0].bundle_seq, 1);
        p.push(&rec(4, 1), meta(0, 0, false), 100);
        p.reset();
        assert!(p.flush().is_none());
        assert_eq!(p.lone(&rec(4, 1), 100)[0].bundle_seq, 0);
    }

    #[test]
    fn exact_fit_stays_in_one_bundle() {
        // Two records filling max_dg exactly.
        let max_dg = FEC_PLAIN_HEADER_LEN + 2 * (FEC_RECORD_LEN_PREFIX + 20);
        let mut p = Packer::new();
        assert!(p.push(&rec(20, 1), meta(1, 1, false), max_dg).is_empty());
        assert!(p.push(&rec(20, 2), meta(2, 1, false), max_dg).is_empty());
        let b = p.flush().unwrap();
        assert_eq!(b.bytes.len(), max_dg);
        let out = p.push(&rec(20, 3), meta(3, 1, false), max_dg);
        assert!(out.is_empty());
    }

    proptest! {
        #[test]
        fn random_sizes_roundtrip(
            max_dg in 60usize..1500,
            sizes in proptest::collection::vec((0usize..1500, any::<bool>()), 1..80),
        ) {
            let limit = Packer::record_limit(max_dg);
            let mut p = Packer::new();
            let mut sealed = Vec::new();
            let mut sent: Vec<Vec<u8>> = Vec::new();
            for (i, (n, pad)) in sizes.iter().enumerate() {
                let n = n % (limit + 1);
                let r = rec(n, i as u8);
                sent.push(r.clone());
                sealed.extend(p.push(&r, meta(i as u32, i as u64, *pad && i % 7 == 0), max_dg));
            }
            sealed.extend(p.flush());
            let mut got = Vec::new();
            let mut prev: Option<u32> = None;
            for b in &sealed {
                prop_assert!(b.bytes.len() <= max_dg);
                if let Some(q) = prev { prop_assert_eq!(b.bundle_seq, q + 1); }
                prev = Some(b.bundle_seq);
                if b.records.iter().any(|r| r.is_pad) { prop_assert_eq!(b.records.len(), 1); }
                let rs = recs_of(b);
                prop_assert_eq!(rs.len(), b.records.len());
                got.extend(rs);
            }
            prop_assert_eq!(got, sent);
        }
    }
}
