//! Pure, sans-IO packer: inner datagrams ("records") into PLAIN bundles, or,
//! with blocks on (`set_blocks`), protected records into SOURCE bundles that
//! build Reed-Solomon blocks (R6-d D2). Nothing here sends or encodes repairs;
//! a closed block is handed over through `take_closed`.

use ghostframe_protocol::fec::frame::{
    push_record, PlainFrame, SourceFrame, FEC_PLAIN_HEADER_LEN, FEC_RECORDS_HEADROOM,
    FEC_RECORD_LEN_PREFIX, FEC_REPAIR_HEADER_LEN, FEC_SOURCE_HEADER_LEN,
};
use smallvec::SmallVec;

use super::blocks::{BlockBuilder, BlockSpan, ClosedBlock, SourceKind};
use crate::config::FecBlockParams;
use crate::transport::protocol::NO_ACK;
use crate::transport::reliable_emitter::emission_queue::WireClass;
use crate::transport::reliable_emitter::Content;

/// What a bundle is on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BundleKind {
    /// A PLAIN frame: no block, no protection.
    Plain,
    /// A SOURCE frame of a block.
    Source,
    /// A REPAIR frame (built by the bridge, never by the packer).
    Repair,
}

/// What the ledger and billing need to know about one record in a bundle.
#[derive(Debug, Clone)]
pub(crate) struct RecordMeta {
    pub wire_seq: u32,
    pub content: Content,
    pub emit_us: u64,
    pub class: WireClass,
    pub is_pad: bool,
    /// The record carries a probe-cluster tag. The server counts a cluster's
    /// packets and bytes as it tags records, and goog_cc counts what it is
    /// acknowledged, one sample per bundle: a tagged record is therefore one
    /// packet of its own (alone in its bundle, like a pad), so that the
    /// cluster's size, `bytes_sent_before` and packet count mean the same to
    /// both. Untagged records still share bundles.
    pub is_probe: bool,
    /// The record's own length (the inner datagram), for billing.
    pub len: usize,
    /// A first send that a block protects (plan decision 2). Ignored while
    /// blocks are off; a pad is never protected.
    pub protect: bool,
}

/// One finished bundle: the datagram bytes and the records it carries.
#[derive(Debug, Clone)]
pub(crate) struct SealedBundle {
    pub bundle_seq: u32,
    pub bytes: Vec<u8>,
    pub records: SmallVec<[RecordMeta; 4]>,
    /// Earliest emit time among `records` (0 when it has none).
    pub emit_us: u64,
    pub kind: BundleKind,
    /// `(block_id, idx)` of a SOURCE, `None` otherwise.
    pub block: Option<(u16, u8)>,
}

#[derive(Debug, Default)]
pub(crate) struct Packer {
    next_seq: u32,
    /// `{len, bytes}` records of the open bundle, without the frame header.
    body: Vec<u8>,
    records: SmallVec<[RecordMeta; 4]>,
    /// The open bundle will be a SOURCE (all its records are protected).
    open_protect: bool,
    /// `None`: every bundle is PLAIN.
    blocks: Option<FecBlockParams>,
    builder: BlockBuilder,
    closed: SmallVec<[ClosedBlock; 1]>,
}

impl Packer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Turns block mode on or off. Meant for a packer with nothing open
    /// (session start); an open block is kept and closes by the usual rules.
    pub(crate) fn set_blocks(&mut self, params: Option<FecBlockParams>) {
        self.blocks = params;
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

    /// Bytes of the bundle that carries one record of `rec_len` alone: what
    /// goog_cc is told it received (`BweSample::size_bytes`), and so the unit
    /// a probe cluster's tagged bytes are counted in. For `Repair`, `rec_len`
    /// is the repair payload (a REPAIR frame has no record prefix).
    pub(crate) fn lone_bundle_len(rec_len: usize, kind: BundleKind) -> usize {
        match kind {
            BundleKind::Plain => FEC_PLAIN_HEADER_LEN + FEC_RECORD_LEN_PREFIX + rec_len,
            BundleKind::Source => FEC_SOURCE_HEADER_LEN + FEC_RECORD_LEN_PREFIX + rec_len,
            BundleKind::Repair => FEC_REPAIR_HEADER_LEN + rec_len,
        }
    }

    pub(crate) fn alloc_seq(&mut self) -> u32 {
        if self.next_seq == NO_ACK {
            self.next_seq = 0;
        }
        let v = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        v
    }

    /// Seals the open bundle. `closing` puts LAST on it and closes its block
    /// (only meaningful for a SOURCE).
    fn seal(&mut self, closing: bool, now_us: u64) -> Option<SealedBundle> {
        if self.records.is_empty() {
            return None;
        }
        let body = std::mem::take(&mut self.body);
        let records = std::mem::take(&mut self.records);
        let protect = std::mem::take(&mut self.open_protect);
        Some(self.finish(body, records, protect, closing, now_us))
    }

    fn finish(
        &mut self,
        body: Vec<u8>,
        records: SmallVec<[RecordMeta; 4]>,
        protect: bool,
        closing: bool,
        now_us: u64,
    ) -> SealedBundle {
        let bundle_seq = self.alloc_seq();
        let emit_us = records.iter().map(|r| r.emit_us).min().unwrap_or(0);
        let mut block = None;
        let (bytes, kind) = match self.blocks.filter(|_| protect) {
            None => (
                PlainFrame {
                    bundle_seq,
                    records: &body,
                }
                .encode(),
                BundleKind::Plain,
            ),
            Some(params) => {
                let (block_id, idx) = self.builder.peek();
                block = Some((block_id, idx));
                // The span of the block as it opened, not of the knob now.
                let span = self.builder.span_or(BlockSpan::for_t_ms(params.t_ms));
                let last = closing || usize::from(idx) + 1 >= usize::from(params.k_max);
                let frame = SourceFrame {
                    last,
                    t_adv: span.t_adv,
                    bundle_seq,
                    block_id,
                    idx,
                    records: &body,
                };
                let kind = if records
                    .iter()
                    .any(|r| matches!(r.content, Content::H264Frag { .. }))
                {
                    SourceKind::H264
                } else {
                    SourceKind::Tile
                };
                self.builder
                    .add(frame.coded_symbol(), bundle_seq, kind, now_us, span);
                if last {
                    self.closed.extend(self.builder.close());
                }
                (frame.encode(), BundleKind::Source)
            }
        };
        SealedBundle {
            bundle_seq,
            bytes,
            records,
            emit_us,
            kind,
            block,
        }
    }

    /// Appends one record at `now_us`. Returns the bundle(s) this sealed: the
    /// open one when `rec` did not fit (or is of another kind), and for a pad
    /// or probe-tagged record also its own lone bundle.
    pub(crate) fn push(
        &mut self,
        rec: &[u8],
        meta: RecordMeta,
        max_dg: usize,
        now_us: u64,
    ) -> SmallVec<[SealedBundle; 2]> {
        let mut out = SmallVec::new();
        debug_assert!(
            rec.len() <= Self::record_limit(max_dg),
            "record of {} bytes exceeds record_limit {} for max_dg {}",
            rec.len(),
            Self::record_limit(max_dg),
            max_dg
        );
        let protect = self.blocks.is_some() && meta.protect && !meta.is_pad;
        // A pad or a probe-tagged record is always alone: seal what is open,
        // then seal it.
        if meta.is_pad || meta.is_probe {
            out.extend(self.seal(false, now_us));
            let mut body = Vec::with_capacity(rec.len() + FEC_RECORD_LEN_PREFIX);
            push_record(&mut body, rec);
            out.push(self.finish(body, SmallVec::from_elem(meta, 1), protect, false, now_us));
            return out;
        }
        // One kind per bundle.
        if !self.records.is_empty() && self.open_protect != protect {
            out.extend(self.seal(false, now_us));
        }
        let overhead = if protect {
            FEC_RECORDS_HEADROOM
        } else {
            FEC_PLAIN_HEADER_LEN
        };
        let need = FEC_RECORD_LEN_PREFIX + rec.len();
        if !self.records.is_empty() && overhead + self.body.len() + need > max_dg {
            out.extend(self.seal(false, now_us));
        }
        push_record(&mut self.body, rec);
        self.records.push(meta);
        self.open_protect = protect;
        // A caller-bug oversize record (debug_assert above) goes out alone.
        if overhead + self.body.len() > max_dg {
            out.extend(self.seal(false, now_us));
        }
        out
    }

    /// Seals whatever is open. A SOURCE sealed here joins its block, which
    /// stays open.
    pub(crate) fn flush(&mut self, now_us: u64) -> Option<SealedBundle> {
        self.seal(false, now_us)
    }

    /// The Empty close: seals the open bundle and, when it is a SOURCE, marks
    /// it LAST and closes its block. With no open bundle it does nothing (a
    /// block whose sources are all out closes on T).
    pub(crate) fn flush_closing(&mut self, now_us: u64) -> Option<SealedBundle> {
        self.seal(true, now_us)
    }

    /// T close: the open block has been open `t_ms` since its first source.
    /// All its sources are already out, so no LAST. Returns whether it closed.
    pub(crate) fn close_if_due(&mut self, now_us: u64) -> bool {
        if self.blocks.is_none() || !self.builder.due(now_us) {
            return false;
        }
        self.close_now()
    }

    /// When the open block's T elapses, in the same microseconds as `now_us`.
    pub(crate) fn t_close_at_us(&self) -> Option<u64> {
        self.blocks?;
        self.builder.t_close_at_us()
    }

    /// The knob this packer runs with.
    pub(crate) fn params(&self) -> Option<FecBlockParams> {
        self.blocks
    }

    /// MTU shrink: a repair of the open block's longest symbol no longer fits
    /// `max_dg`, so the block closes at its current size.
    pub(crate) fn close_for_mtu(&mut self, max_dg: usize) -> bool {
        self.builder.outgrown_by(max_dg) && self.close_now()
    }

    /// Closes the open block, if it has sources (a hand-off or a decline for a
    /// block still sending). Returns whether it closed.
    pub(crate) fn close_now(&mut self) -> bool {
        match self.builder.close() {
            Some(b) => {
                self.closed.push(b);
                true
            }
            None => false,
        }
    }

    /// Id of the block still taking sources, if it has any.
    pub(crate) fn open_block_id(&self) -> Option<u16> {
        (self.builder.len() > 0).then(|| self.builder.peek().0)
    }

    /// Blocks closed since the last call, oldest first.
    pub(crate) fn take_closed(&mut self) -> SmallVec<[ClosedBlock; 1]> {
        std::mem::take(&mut self.closed)
    }

    /// Frames one control record (pong, eviction) alone, bypassing the open
    /// bundle: flushes first, then a lone PLAIN. The control record has no
    /// `RecordMeta`, so that bundle's `records` is empty.
    pub(crate) fn lone(
        &mut self,
        rec: &[u8],
        max_dg: usize,
        now_us: u64,
    ) -> SmallVec<[SealedBundle; 2]> {
        debug_assert!(rec.len() <= Self::record_limit(max_dg));
        let mut out = SmallVec::new();
        out.extend(self.seal(false, now_us));
        let mut body = Vec::with_capacity(rec.len() + FEC_RECORD_LEN_PREFIX);
        push_record(&mut body, rec);
        out.push(self.finish(body, SmallVec::new(), false, false, now_us));
        out
    }

    /// Drops the open bundle and block and restarts both counters (session
    /// reset). Block mode stays as configured.
    pub(crate) fn reset(&mut self) {
        let blocks = self.blocks;
        *self = Self::default();
        self.blocks = blocks;
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
            is_probe: false,
            len: 0,
            protect: false,
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
            sealed.extend(p.push(&r, meta(i, 100 - i as u64, false), max_dg, 0));
        }
        sealed.extend(p.flush(0));
        assert!(p.flush(0).is_none());
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
        assert!(p.push(&rec(10, 1), meta(1, 5, false), 200, 0).is_empty());
        assert!(p.push(&rec(10, 2), meta(2, 6, false), 200, 0).is_empty());
        let out = p.push(&rec(50, 9), meta(3, 7, true), 200, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].records.len(), 2);
        assert_eq!(out[1].records.len(), 1);
        assert!(out[1].records[0].is_pad);
        assert_eq!(recs_of(&out[1]), vec![rec(50, 9)]);
        // Nothing left open; next record opens a fresh bundle.
        assert!(p.flush(0).is_none());
        assert!(p.push(&rec(10, 4), meta(4, 8, false), 200, 0).is_empty());
        let b = p.flush(0).unwrap();
        assert_eq!(b.records.len(), 1);
        assert_eq!(b.bundle_seq, 2);
    }

    #[test]
    fn a_probe_tagged_record_is_alone_and_untagged_ones_still_share() {
        let mut p = Packer::new();
        let probe = |ws| RecordMeta {
            is_probe: true,
            ..meta(ws, 1, false)
        };
        let mut sealed = Vec::new();
        sealed.extend(p.push(&rec(10, 1), meta(1, 1, false), 1200, 0));
        sealed.extend(p.push(&rec(10, 2), probe(2), 1200, 0));
        sealed.extend(p.push(&rec(10, 3), probe(3), 1200, 0));
        sealed.extend(p.push(&rec(10, 4), meta(4, 1, false), 1200, 0));
        sealed.extend(p.push(&rec(10, 5), meta(5, 1, false), 1200, 0));
        sealed.extend(p.flush(0));
        let shape: Vec<Vec<u32>> = sealed
            .iter()
            .map(|b| b.records.iter().map(|r| r.wire_seq).collect())
            .collect();
        assert_eq!(shape, vec![vec![1], vec![2], vec![3], vec![4, 5]]);
        // The unit a cluster's bytes are counted in is the bundle goog_cc is
        // told about.
        assert_eq!(
            sealed[1].bytes.len(),
            Packer::lone_bundle_len(10, BundleKind::Plain)
        );
    }

    #[test]
    fn lone_flushes_then_frames_alone() {
        let mut p = Packer::new();
        p.push(&rec(10, 1), meta(1, 5, false), 200, 0);
        let out = p.lone(&rec(8, 7), 200, 0);
        assert_eq!(out.len(), 2);
        assert_eq!(recs_of(&out[0]), vec![rec(10, 1)]);
        assert!(out[1].records.is_empty());
        assert_eq!(recs_of(&out[1]), vec![rec(8, 7)]);
        assert_eq!(out[1].bundle_seq, out[0].bundle_seq + 1);
        // With nothing open, only the lone bundle comes back.
        assert_eq!(p.lone(&rec(8, 7), 200, 0).len(), 1);
    }

    #[test]
    fn bundle_seq_skips_no_ack_and_reset_restarts() {
        let mut p = Packer::new();
        p.next_seq = NO_ACK - 1;
        let a = p.lone(&rec(4, 1), 100, 0);
        let b = p.lone(&rec(4, 1), 100, 0);
        let c = p.lone(&rec(4, 1), 100, 0);
        assert_eq!(a[0].bundle_seq, NO_ACK - 1);
        assert_eq!(b[0].bundle_seq, 0, "NO_ACK skipped");
        assert_eq!(c[0].bundle_seq, 1);
        p.push(&rec(4, 1), meta(0, 0, false), 100, 0);
        p.reset();
        assert!(p.flush(0).is_none());
        assert_eq!(p.lone(&rec(4, 1), 100, 0)[0].bundle_seq, 0);
    }

    #[test]
    fn exact_fit_stays_in_one_bundle() {
        // Two records filling max_dg exactly.
        let max_dg = FEC_PLAIN_HEADER_LEN + 2 * (FEC_RECORD_LEN_PREFIX + 20);
        let mut p = Packer::new();
        assert!(p.push(&rec(20, 1), meta(1, 1, false), max_dg, 0).is_empty());
        assert!(p.push(&rec(20, 2), meta(2, 1, false), max_dg, 0).is_empty());
        let b = p.flush(0).unwrap();
        assert_eq!(b.bytes.len(), max_dg);
        let out = p.push(&rec(20, 3), meta(3, 1, false), max_dg, 0);
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
                sealed.extend(p.push(&r, meta(i as u32, i as u64, *pad && i % 7 == 0), max_dg, 0));
            }
            sealed.extend(p.flush(0));
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

    // ---- block mode (R6-d D2) ----

    use crate::transport::reliable_emitter::EmitKey;
    use ghostframe_protocol::fec::codec::{encode_repairs, recover};
    use ghostframe_protocol::fec::frame::{source_from_symbol, FEC_REPAIR_HEADER_LEN};

    fn params(k_max: u8, t_ms: u8, r: u8) -> Option<FecBlockParams> {
        Some(FecBlockParams {
            k_max,
            t_ms,
            r,
            m: 1,
        })
    }

    fn pmeta(ws: u32) -> RecordMeta {
        RecordMeta {
            protect: true,
            ..meta(ws, ws as u64, false)
        }
    }

    fn block_packer(k_max: u8, t_ms: u8, r: u8) -> Packer {
        let mut p = Packer::new();
        p.set_blocks(params(k_max, t_ms, r));
        p
    }

    /// `(last, block_id, idx, bundle_seq, records)` of a SOURCE bundle.
    fn src_of(b: &SealedBundle) -> (bool, u16, u8, u32, Vec<Vec<u8>>) {
        assert_eq!(b.kind, BundleKind::Source);
        match parse(&b.bytes).expect("source frame") {
            Frame::Source(s) => {
                assert_eq!(s.bundle_seq, b.bundle_seq);
                (
                    s.last,
                    s.block_id,
                    s.idx,
                    s.bundle_seq,
                    records(s.records).map(|r| r.unwrap().to_vec()).collect(),
                )
            }
            other => panic!("not a source: {other:?}"),
        }
    }

    /// One record per bundle: 60-byte records into a 100-byte datagram.
    const ONE_PER_BUNDLE: usize = 100;

    #[test]
    fn k_max_closes_with_last_on_exactly_the_closing_source() {
        let mut p = block_packer(4, 20, 2);
        let mut sealed = Vec::new();
        for i in 0..8u32 {
            sealed.extend(p.push(&rec(60, i as u8), pmeta(i), ONE_PER_BUNDLE, 7));
        }
        sealed.extend(p.flush(7));
        assert_eq!(sealed.len(), 8, "one record per bundle");
        let got: Vec<_> = sealed.iter().map(src_of).collect();
        let idxs: Vec<u8> = got.iter().map(|g| g.2).collect();
        assert_eq!(idxs, vec![0, 1, 2, 3, 0, 1, 2, 3]);
        let lasts: Vec<bool> = got.iter().map(|g| g.0).collect();
        assert_eq!(
            lasts,
            vec![false, false, false, true, false, false, false, true]
        );
        let ids: Vec<u16> = got.iter().map(|g| g.1).collect();
        assert_eq!(ids, vec![0, 0, 0, 0, 1, 1, 1, 1]);
        let closed = p.take_closed();
        assert_eq!(closed.len(), 2);
        assert!(closed.iter().all(|c| c.k == 4 && c.symbols.len() == 4));
        assert!(p.take_closed().is_empty(), "taken once");
    }

    #[test]
    fn empty_close_marks_the_final_bundle_last_and_does_nothing_when_none_is_open() {
        let mut p = block_packer(10, 20, 2);
        assert!(p.flush_closing(1).is_none());
        assert!(p.take_closed().is_empty());
        let mut sealed = Vec::new();
        for i in 0..3u32 {
            sealed.extend(p.push(&rec(60, i as u8), pmeta(i), ONE_PER_BUNDLE, 1));
        }
        // Two sealed, the third still open.
        assert_eq!(sealed.len(), 2);
        assert!(p.take_closed().is_empty());
        let last = p.flush_closing(1).expect("the open bundle");
        sealed.push(last);
        let got: Vec<_> = sealed.iter().map(src_of).collect();
        assert_eq!(
            got.iter().map(|g| g.0).collect::<Vec<_>>(),
            vec![false, false, true]
        );
        let closed = p.take_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].k, 3);
        // All sources out, nothing open: Empty again does nothing.
        assert!(p.flush_closing(2).is_none());
        assert!(p.take_closed().is_empty());
    }

    #[test]
    fn t_closes_without_last_and_the_next_source_opens_a_new_block() {
        let mut p = block_packer(10, 5, 1);
        let mut sealed = Vec::new();
        sealed.extend(p.push(&rec(10, 1), pmeta(1), 200, 1_000));
        sealed.extend(p.flush(1_000));
        sealed.extend(p.push(&rec(10, 2), pmeta(2), 200, 3_000));
        sealed.extend(p.flush(3_000));
        assert!(!p.close_if_due(5_999), "T counts from the first source");
        assert!(p.take_closed().is_empty());
        assert!(p.close_if_due(6_000));
        assert!(!p.close_if_due(9_000), "nothing is open any more");
        let closed = p.take_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].k, 2);
        assert_eq!(closed[0].first_sealed_at_us, 1_000);
        assert_eq!(closed[0].bundle_seqs, vec![0, 1]);
        assert!(sealed.iter().all(|b| !src_of(b).0), "no LAST on a T close");
        sealed.extend(p.push(&rec(10, 3), pmeta(3), 200, 7_000));
        sealed.extend(p.flush(7_000));
        let (_, id, idx, _, _) = src_of(sealed.last().unwrap());
        assert_eq!((id, idx), (1, 0));
    }

    fn t_adv_of(b: &SealedBundle) -> u8 {
        match parse(&b.bytes).expect("source frame") {
            Frame::Source(s) => s.t_adv,
            other => panic!("not a source: {other:?}"),
        }
    }

    /// A block opened under `t_ms = 5` advertises 15 ms (code 2) and closes at
    /// 5 ms, however the knob moves while it is open and a PLAIN bundle is open.
    #[test]
    fn a_block_keeps_the_span_it_opened_with_when_the_knob_moves() {
        let mut p = block_packer(10, 5, 1);
        let mut sealed = Vec::new();
        sealed.extend(p.push(&rec(10, 1), pmeta(1), 200, 1_000));
        sealed.extend(p.flush(1_000));
        assert_eq!(sealed.len(), 1);
        assert_eq!(
            t_adv_of(&sealed[0]),
            2,
            "premise: t_ms 5 advertises a non-zero code"
        );
        // A PLAIN bundle is open (an unprotected record), then the knob moves
        // to t_ms = 20, which advertises code 0.
        let plain = RecordMeta {
            protect: false,
            ..meta(2, 2, false)
        };
        assert!(p.push(&rec(10, 2), plain, 200, 2_000).is_empty());
        p.set_blocks(params(10, 20, 1));
        assert_eq!(
            p.t_close_at_us(),
            Some(6_000),
            "the block's own T, not the knob's"
        );
        sealed.extend(p.push(&rec(10, 3), pmeta(3), 200, 3_000));
        sealed.extend(p.flush(3_000));
        let sources: Vec<&SealedBundle> = sealed
            .iter()
            .filter(|b| b.kind == BundleKind::Source)
            .collect();
        assert_eq!(sources.len(), 2);
        assert!(
            sources.iter().all(|b| t_adv_of(b) == 2),
            "every source of the block carries the code it opened with"
        );
        assert!(!p.close_if_due(5_999));
        assert!(
            p.close_if_due(6_000),
            "closes at the span it opened with, not 20 ms"
        );
        let closed = p.take_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].t_adv, 2);
        // The next block opens under the knob now in force.
        let mut next = p.push(&rec(10, 4), pmeta(4), 200, 7_000);
        next.extend(p.flush(7_000));
        let next: Vec<&SealedBundle> = next
            .iter()
            .filter(|b| b.kind == BundleKind::Source)
            .collect();
        assert_eq!(t_adv_of(next[0]), 0);
        assert_eq!(p.t_close_at_us(), Some(27_000));
    }

    #[test]
    fn an_mtu_shrink_closes_at_the_current_size_without_last() {
        let mut p = block_packer(10, 20, 2);
        let mut sealed = Vec::new();
        sealed.extend(p.push(&rec(80, 1), pmeta(1), 200, 1));
        sealed.extend(p.flush(1));
        assert!(!p.close_for_mtu(200), "the repair still fits");
        let sym = 6 + 2 + 80;
        assert!(!p.close_for_mtu(sym + FEC_REPAIR_HEADER_LEN));
        assert!(p.close_for_mtu(sym + FEC_REPAIR_HEADER_LEN - 1));
        assert!(!p.close_for_mtu(10), "nothing open");
        let closed = p.take_closed();
        assert_eq!(closed.len(), 1);
        assert_eq!(closed[0].symbol_len(), sym);
        assert!(!src_of(&sealed[0]).0);
    }

    #[test]
    fn close_now_closes_a_block_still_sending_and_only_then() {
        let mut p = block_packer(10, 20, 2);
        assert!(!p.close_now());
        p.push(&rec(10, 1), pmeta(1), 200, 1);
        p.flush(1);
        assert!(p.close_now());
        assert!(!p.close_now());
        assert_eq!(p.take_closed().len(), 1);
    }

    #[test]
    fn every_sealed_source_parses_and_its_coded_symbol_is_what_the_block_stored() {
        let mut p = block_packer(5, 20, 2);
        let mut sealed = Vec::new();
        for i in 0..12u32 {
            sealed.extend(p.push(&rec((7 + 13 * i as usize) % 90, i as u8), pmeta(i), 120, 9));
        }
        sealed.extend(p.flush_closing(9));
        let closed = p.take_closed();
        assert!(closed.len() >= 2, "premise: more than one block");
        let (last, full) = closed.split_last().unwrap();
        assert!(full.iter().all(|c| c.k == 5), "k_max closes the full ones");
        assert!(last.k <= 5);
        for b in &sealed {
            let Frame::Source(s) = parse(&b.bytes).expect("parses") else {
                panic!("not a source")
            };
            let blk = closed.iter().find(|c| c.block_id == s.block_id).unwrap();
            assert_eq!(blk.symbols[s.idx as usize], s.coded_symbol());
            assert_eq!(blk.bundle_seqs[s.idx as usize], s.bundle_seq);
        }
    }

    #[test]
    fn a_source_carries_its_content_kind_for_the_deadline() {
        let mut p = block_packer(3, 20, 1);
        let tile = RecordMeta {
            content: Content::Tile(EmitKey::new(1, 1, 2, 0)),
            ..pmeta(1)
        };
        let mut sealed = Vec::new();
        sealed.extend(p.push(&rec(60, 1), tile, ONE_PER_BUNDLE, 1));
        sealed.extend(p.push(&rec(60, 2), pmeta(2), ONE_PER_BUNDLE, 1));
        assert!(p.flush(1).is_some());
        assert!(p.close_if_due(100_000_000));
        let c = &p.take_closed()[0];
        assert_eq!(c.kinds, vec![SourceKind::Tile, SourceKind::H264]);
    }

    #[test]
    fn a_change_of_kind_seals_the_open_bundle_and_the_block_stays_open() {
        let mut p = block_packer(10, 20, 2);
        assert!(p.push(&rec(10, 1), pmeta(1), 300, 1).is_empty());
        // An unprotected record (a retransmit) must not join the source.
        let out = p.push(&rec(10, 2), meta(2, 2, false), 300, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, BundleKind::Source);
        // And back again.
        let out = p.push(&rec(10, 3), pmeta(3), 300, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, BundleKind::Plain);
        assert_eq!(recs_of(&out[0]), vec![rec(10, 2)]);
        let b = p.flush(1).unwrap();
        assert_eq!(src_of(&b).2, 1, "idx continues in the same block");
        assert!(p.take_closed().is_empty());
        // A pad is never protected, whatever its meta says.
        let pad = RecordMeta {
            protect: true,
            ..meta(4, 4, true)
        };
        let out = p.push(&rec(10, 4), pad, 300, 1);
        assert_eq!(out[0].kind, BundleKind::Plain);
    }

    #[test]
    fn a_probe_tagged_source_is_alone_and_lone_bundle_len_counts_its_header() {
        let mut p = block_packer(10, 20, 2);
        let probe = RecordMeta {
            is_probe: true,
            ..pmeta(1)
        };
        let out = p.push(&rec(33, 1), probe, 300, 1);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, BundleKind::Source);
        assert_eq!(
            out[0].bytes.len(),
            Packer::lone_bundle_len(33, BundleKind::Source)
        );
        assert_eq!(
            Packer::lone_bundle_len(33, BundleKind::Source),
            Packer::lone_bundle_len(33, BundleKind::Plain) + 4
        );
    }

    #[test]
    fn with_blocks_off_a_protected_record_is_plain() {
        let mut p = Packer::new();
        let mut out = p.push(&rec(10, 1), pmeta(1), 300, 1);
        out.extend(p.flush(1));
        assert_eq!(out[0].kind, BundleKind::Plain);
        assert!(p.take_closed().is_empty());
        assert!(!p.close_if_due(u64::MAX));
    }

    #[test]
    fn reset_clears_the_block_and_keeps_the_mode() {
        let mut p = block_packer(10, 20, 2);
        p.push(&rec(10, 1), pmeta(1), 300, 1);
        p.flush(1);
        p.reset();
        assert!(!p.close_now(), "the open block is gone");
        let mut out = p.push(&rec(10, 1), pmeta(1), 300, 1);
        out.extend(p.flush(1));
        let (_, id, idx, seq, _) = src_of(&out[0]);
        assert_eq!((id, idx, seq), (0, 0, 0), "still in block mode, from zero");
    }

    #[test]
    fn the_block_id_wraps_from_65535_to_0() {
        let mut p = block_packer(1, 20, 1);
        p.builder.set_next_id_for_test(65_535);
        let mut out = Vec::new();
        for i in 0..3u32 {
            out.extend(p.push(&rec(10, i as u8), pmeta(i), 300, 1));
            out.extend(p.flush(1));
        }
        let ids: Vec<u16> = out.iter().map(|b| src_of(b).1).collect();
        assert_eq!(ids, vec![65_535, 0, 1]);
        assert!(
            out.iter().all(|b| src_of(b).0),
            "k_max 1: every one is LAST"
        );
    }

    /// Records a block's sources carry, in idx order.
    fn block_records(sealed: &[SealedBundle], block_id: u16) -> Vec<Vec<Vec<u8>>> {
        sealed
            .iter()
            .filter(|b| b.kind == BundleKind::Source)
            .map(src_of)
            .filter(|g| g.1 == block_id)
            .map(|g| g.4)
            .collect()
    }

    /// Encodes `rows` repairs over `blk`, drops the sources in `drop`, recovers
    /// them and returns each recovered source's records. `corrupt` flips one
    /// byte of the first repair payload (the mutation).
    fn recovered_records(
        blk: &ClosedBlock,
        rows: &[u8],
        drop: &[usize],
        corrupt: bool,
    ) -> Vec<(usize, Vec<Vec<u8>>)> {
        let srcs: Vec<&[u8]> = blk.symbols.iter().map(|s| s.as_slice()).collect();
        let mut repairs = encode_repairs(&srcs, rows).expect("encode");
        if corrupt {
            // Inside the records of every source (the symbol header is 6).
            repairs[0][8] ^= 0x55;
        }
        let have: Vec<Option<&[u8]>> = srcs
            .iter()
            .enumerate()
            .map(|(j, s)| (!drop.contains(&j)).then_some(*s))
            .collect();
        let reps: Vec<(u8, &[u8])> = rows
            .iter()
            .copied()
            .zip(repairs.iter().map(|v| v.as_slice()))
            .collect();
        recover(&have, &reps)
            .expect("recover")
            .into_iter()
            .map(|(j, sym)| {
                let (_, body) = source_from_symbol(&sym).expect("symbol");
                (j, records(body).map(|r| r.unwrap().to_vec()).collect())
            })
            .collect()
    }

    #[test]
    fn a_block_round_trips_through_the_protocol_codec_and_a_flipped_byte_breaks_it() {
        let mut p = block_packer(6, 20, 3);
        let mut sealed = Vec::new();
        for i in 0..16u32 {
            // Different lengths, two records in some bundles.
            let n = 20 + 5 * i as usize;
            sealed.extend(p.push(&rec(n, i as u8), pmeta(2 * i), 200, 1));
            sealed.extend(p.push(&rec(9, 90 + i as u8), pmeta(2 * i + 1), 200, 1));
        }
        sealed.extend(p.flush_closing(1));
        let closed = p.take_closed();
        let blk = &closed[0];
        let want = block_records(&sealed, blk.block_id);
        assert_eq!(want.len(), blk.k as usize);
        let drops = [0usize, 2, 5];
        let got = recovered_records(blk, &[0, 1, 2], &drops, false);
        assert_eq!(got.len(), 3);
        for (j, recs) in &got {
            assert_eq!(recs, &want[*j], "source {j}");
        }
        // Mutation: one flipped repair byte must change what comes back.
        let bad = recovered_records(blk, &[0, 1, 2], &drops, true);
        assert!(
            bad.iter().any(|(j, recs)| recs != &want[*j]),
            "a corrupted repair must not recover the original"
        );
        // The XOR path (one loss, row 0) too.
        let one = recovered_records(blk, &[0], &[3], false);
        assert_eq!(one[0].1, want[3]);
        let one_bad = recovered_records(blk, &[0], &[3], true);
        assert_ne!(one_bad[0].1, want[3]);
    }

    proptest! {
        #[test]
        fn block_streams_respect_max_dg_and_round_trip(
            max_dg in 60usize..1500,
            k_max in 1u8..12,
            r in 1u8..4,
            plan in proptest::collection::vec((0usize..1500, 0u8..8), 1..60),
            drop_seed in any::<u64>(),
        ) {
            let limit = Packer::record_limit(max_dg);
            let mut p = block_packer(k_max, 20, r);
            let mut sealed = Vec::new();
            for (i, (n, flavour)) in plan.iter().enumerate() {
                let n = n % (limit + 1);
                let m = RecordMeta {
                    protect: *flavour != 0,
                    is_probe: *flavour == 1,
                    ..meta(i as u32, i as u64, *flavour == 2)
                };
                sealed.extend(p.push(&rec(n, i as u8), m, max_dg, i as u64));
            }
            sealed.extend(p.flush_closing(99));
            let closed = p.take_closed();
            for b in &sealed {
                prop_assert!(b.bytes.len() <= max_dg);
            }
            for blk in &closed {
                prop_assert!(blk.symbol_len() + FEC_REPAIR_HEADER_LEN <= max_dg);
                prop_assert_eq!(blk.symbols.len(), blk.k as usize);
                prop_assert!(blk.k <= k_max);
                // Drop up to r sources, chosen by the seed, and recover them.
                let k = blk.k as usize;
                let nd = (drop_seed as usize % (usize::from(r) + 1)).min(k);
                let drops: Vec<usize> =
                    (0..nd).map(|d| (drop_seed as usize / 7 + d * 5) % k).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
                if drops.is_empty() { continue; }
                let rows: Vec<u8> = (0..drops.len() as u8).collect();
                let want = block_records(&sealed, blk.block_id);
                for (j, recs) in recovered_records(blk, &rows, &drops, false) {
                    prop_assert_eq!(&recs, &want[j]);
                }
            }
        }
    }
}
