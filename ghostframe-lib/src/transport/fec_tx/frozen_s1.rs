//! A frozen copy of the single-block packer as it was before R6-e2 S2
//! (commit 9ef34a50), kept only so a test can hold the new packer to it at
//! `m = 1`. Do not edit: the point is that it does not move. Test builds only.

#![allow(dead_code)]
use ghostframe_protocol::fec::frame::{
    push_record, PlainFrame, SourceFrame, FEC_PLAIN_HEADER_LEN, FEC_RECORDS_HEADROOM,
    FEC_RECORD_LEN_PREFIX, FEC_REPAIR_HEADER_LEN, FEC_SOURCE_HEADER_LEN,
};
use smallvec::SmallVec;

use super::blocks::{BlockSpan, ClosedBlock, SourceKind};
use super::packer::{BundleKind, RecordMeta, SealedBundle};
use crate::config::FecBlockParams;
use crate::transport::protocol::NO_ACK;
use crate::transport::reliable_emitter::Content;

#[derive(Debug)]
struct OldOpen {
    block_id: u16,
    symbols: Vec<Vec<u8>>,
    bundle_seqs: Vec<u32>,
    kinds: Vec<SourceKind>,
    first_sealed_at_us: u64,
    span: BlockSpan,
    /// Longest coded symbol so far, kept so the MTU test is O(1) per send.
    longest: usize,
}

#[derive(Debug, Default)]
pub(crate) struct OldBlockBuilder {
    /// The id the next new block takes; wraps at 65 535.
    next_id: u16,
    open: Option<OldOpen>,
}

impl OldBlockBuilder {
    /// The `(block_id, idx)` the next source will take.
    pub(crate) fn peek(&self) -> (u16, u8) {
        match &self.open {
            Some(o) => (
                o.block_id,
                u8::try_from(o.symbols.len()).expect("a block never holds 256 sources"),
            ),
            None => (self.next_id, 0),
        }
    }

    /// The span the next source advertises: the open block's own, or `fresh`
    /// when the source will open a block.
    pub(crate) fn span_or(&self, fresh: BlockSpan) -> BlockSpan {
        self.open.as_ref().map_or(fresh, |o| o.span)
    }

    /// Adds the source whose `(block_id, idx)` `peek` returned. `span` is
    /// stored if this source opens the block and ignored otherwise.
    pub(crate) fn add(
        &mut self,
        symbol: Vec<u8>,
        bundle_seq: u32,
        kind: SourceKind,
        now_us: u64,
        span: BlockSpan,
    ) {
        if self.open.is_none() {
            self.open = Some(OldOpen {
                block_id: self.next_id,
                symbols: Vec::new(),
                bundle_seqs: Vec::new(),
                kinds: Vec::new(),
                first_sealed_at_us: now_us,
                span,
                longest: 0,
            });
            self.next_id = self.next_id.wrapping_add(1);
        }
        let o = self.open.as_mut().expect("just opened");
        o.longest = o.longest.max(symbol.len());
        o.symbols.push(symbol);
        o.bundle_seqs.push(bundle_seq);
        o.kinds.push(kind);
    }

    /// Number of sources in the open block (0 when none is open).
    pub(crate) fn len(&self) -> usize {
        self.open.as_ref().map_or(0, |o| o.symbols.len())
    }

    /// The block's own T has elapsed since its first source was sealed.
    pub(crate) fn due(&self, now_us: u64) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| now_us >= o.first_sealed_at_us.saturating_add(o.span.t_us))
    }

    /// When the open block's own T elapses (microseconds).
    pub(crate) fn t_close_at_us(&self) -> Option<u64> {
        self.open
            .as_ref()
            .map(|o| o.first_sealed_at_us.saturating_add(o.span.t_us))
    }

    /// A repair of the open block's longest symbol no longer fits `max_dg`.
    pub(crate) fn outgrown_by(&self, max_dg: usize) -> bool {
        self.open
            .as_ref()
            .is_some_and(|o| o.longest + FEC_REPAIR_HEADER_LEN > max_dg)
    }

    /// Closes the open block, if it has sources.
    pub(crate) fn close(&mut self) -> Option<ClosedBlock> {
        let o = self.open.take()?;
        Some(ClosedBlock {
            block_id: o.block_id,
            k: u8::try_from(o.symbols.len()).expect("a block never holds 256 sources"),
            symbols: o.symbols,
            bundle_seqs: o.bundle_seqs,
            kinds: o.kinds,
            first_sealed_at_us: o.first_sealed_at_us,
            t_adv: o.span.t_adv,
            group: 0,
            group_m: 1,
        })
    }

    #[cfg(test)]
    pub(crate) fn set_next_id_for_test(&mut self, id: u16) {
        self.next_id = id;
    }

    /// Forgets the open block and restarts the id counter.
    pub(crate) fn reset(&mut self) {
        *self = Self::default();
    }
}

#[derive(Debug, Default)]
pub(crate) struct OldPacker {
    next_seq: u32,
    /// `{len, bytes}` records of the open bundle, without the frame header.
    body: Vec<u8>,
    records: SmallVec<[RecordMeta; 4]>,
    /// The open bundle will be a SOURCE (all its records are protected).
    open_protect: bool,
    /// `None`: every bundle is PLAIN.
    blocks: Option<FecBlockParams>,
    builder: OldBlockBuilder,
    closed: SmallVec<[ClosedBlock; 1]>,
}

impl OldPacker {
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
