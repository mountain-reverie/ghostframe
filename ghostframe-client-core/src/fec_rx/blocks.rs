//! Block store: unbundle SOURCE / REPAIR / PLAIN frames, keep the coded symbols
//! of each block, rebuild lost sources and decide what to do when a block's
//! recovery window ends. Pure sans-IO: time comes in as `now_us`, actions go out.

use ghostframe_protocol::fec::codec;
use ghostframe_protocol::fec::feedback::RepairAsk;
use ghostframe_protocol::fec::frame::{self, Frame, PlainFrame, RepairFrame, SourceFrame};
use ghostframe_protocol::fec::repair_status::DeclineReason;

/// Recovery window while the server advertises nothing (plan decision 2).
pub const FEC_RX_DEFAULT_T_ADV_US: u64 = 33_000;
pub const FEC_RX_ALLOWANCE_US: u64 = 10_000;
/// Most blocks held at once.
pub const FEC_RX_MAX_OPEN_BLOCKS: usize = 64;
/// Most symbol bytes held across all blocks.
pub const FEC_RX_MAX_HELD_BYTES: usize = 4 * 1024 * 1024;
/// A block this many ids behind the newest is pruned.
pub const FEC_RX_BLOCK_WINDOW: u16 = 1024;

const WINDOW_US: u64 = FEC_RX_DEFAULT_T_ADV_US + FEC_RX_ALLOWANCE_US;

/// What one framed datagram produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct RxOutcome {
    /// The datagram's own bundle_seq if it parsed (SOURCE, REPAIR or PLAIN): an arrival to ACK.
    pub arrival: Option<u32>,
    /// Records to dispatch, in order: those of this datagram (if new) and of any source it let us rebuild.
    pub records: Vec<RxRecord>,
    /// bundle_seqs of sources rebuilt by this datagram (RECOVERED).
    pub rebuilt: Vec<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RxRecord {
    pub bundle_seq: u32,
    pub record_idx: u8,
    pub bytes: Vec<u8>,
    pub rebuilt: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseAction {
    /// Ask for more symbols (decision 6).
    Request(RepairAsk),
    /// Give the block up (decision 3, 4, 6). `missing[..ceil(k/8)]` is the bitmap to send.
    Decline {
        block_id: u16,
        reason: DeclineReason,
        k: u8,
        missing: [u8; 32],
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockStats {
    pub sources_rx: u64,
    pub repairs_rx: u64,
    pub plain_rx: u64,
    pub rebuilt: u64,
    pub repair_used: u64,
    pub repair_wasted: u64,
    pub rejected_symbols: u64,
    pub malformed: u64,
    /// SOURCE frames of a block too old to track: declined or released long ago, not dispatched.
    pub stale_dropped: u64,
    pub blocks_closed_complete: u64,
    pub blocks_unrecoverable: u64,
    pub requests: u64,
    pub declined: u64,
    pub duplicates: u64,
    pub held_bytes: u64,
}

struct Block {
    id: u16,
    k: Option<u8>,
    /// Coded symbols by source idx (received or rebuilt).
    sources: Vec<Option<Vec<u8>>>,
    /// `(repair_idx, payload)`.
    repairs: Vec<(u8, Vec<u8>)>,
    /// Payload length of the repairs (all equal).
    symbol_len: Option<usize>,
    /// Repairs a decode consumed.
    repairs_used: usize,
    held_sources: usize,
    bytes: usize,
    close_at: u64,
    requested: bool,
    /// Decoding failed: nothing more to try.
    unrecoverable: bool,
    /// Why a frame of this block was dropped (decision 4).
    sticky: Option<DeclineReason>,
}

impl Block {
    fn new(id: u16, now_us: u64) -> Self {
        Self {
            id,
            k: None,
            sources: Vec::new(),
            repairs: Vec::new(),
            symbol_len: None,
            repairs_used: 0,
            held_sources: 0,
            bytes: 0,
            close_at: now_us.saturating_add(WINDOW_US),
            requested: false,
            unrecoverable: false,
            sticky: None,
        }
    }

    fn held(&self) -> usize {
        self.held_sources + self.repairs.len()
    }

    fn has_source(&self, idx: u8) -> bool {
        self.sources
            .get(usize::from(idx))
            .is_some_and(Option::is_some)
    }

    fn put_source(&mut self, idx: u8, sym: Vec<u8>) {
        let i = usize::from(idx);
        if self.sources.len() <= i {
            self.sources.resize(i + 1, None);
        }
        self.bytes += sym.len();
        self.held_sources += 1;
        self.sources[i] = Some(sym);
    }

    fn complete(&self) -> bool {
        self.k.is_some_and(|k| self.held_sources >= usize::from(k))
    }

    fn highest_repair_idx(&self) -> Option<u8> {
        self.repairs.iter().map(|&(i, _)| i).max()
    }

    /// `(k, missing bitmap)`: k unknown uses the highest source idx seen + 1.
    fn missing_bitmap(&self) -> (u8, [u8; 32]) {
        let k = match self.k {
            Some(k) => usize::from(k),
            None => self.sources.len(),
        };
        let mut bm = [0u8; 32];
        for i in 0..k.min(256) {
            if !self.has_source(i as u8) {
                bm[i / 8] |= 1 << (i % 8);
            }
        }
        (u8::try_from(k).unwrap_or(u8::MAX), bm)
    }
}

/// Block was closed; remembered so a late datagram does not reopen it.
#[derive(Clone, Copy)]
struct Closed {
    id: u16,
    complete: bool,
}

pub struct BlockStore {
    /// Open blocks in order of first arrival.
    blocks: Vec<Block>,
    newest: Option<u16>,
    held_bytes: usize,
    closed: Vec<Option<Closed>>,
    pending: Vec<CloseAction>,
    last_now: u64,
    stats: BlockStats,
}

impl Default for BlockStore {
    fn default() -> Self {
        Self::new()
    }
}

/// `id` is no newer than `newest` by more than the window (wrapping).
fn too_old(newest: u16, id: u16) -> bool {
    let behind = newest.wrapping_sub(id);
    (FEC_RX_BLOCK_WINDOW..0x8000).contains(&behind)
}

fn ahead(id: u16, newest: u16) -> bool {
    let d = id.wrapping_sub(newest);
    d != 0 && d < 0x8000
}

/// Push the records of `bytes` into `out`; an overrun stops the walk.
fn dispatch(
    bytes: &[u8],
    bundle_seq: u32,
    rebuilt: bool,
    out: &mut Vec<RxRecord>,
    malformed: &mut u64,
) {
    for (i, rec) in frame::records(bytes).enumerate() {
        match rec {
            Ok(b) => out.push(RxRecord {
                bundle_seq,
                record_idx: u8::try_from(i).unwrap_or(u8::MAX),
                bytes: b.to_vec(),
                rebuilt,
            }),
            Err(_) => {
                *malformed += 1;
                break;
            }
        }
    }
}

impl BlockStore {
    pub fn new() -> Self {
        Self {
            blocks: Vec::new(),
            newest: None,
            held_bytes: 0,
            closed: vec![None; usize::from(FEC_RX_BLOCK_WINDOW)],
            pending: Vec::new(),
            last_now: 0,
            stats: BlockStats::default(),
        }
    }

    pub fn stats(&self) -> BlockStats {
        BlockStats {
            held_bytes: self.held_bytes as u64,
            ..self.stats
        }
    }

    fn slot(id: u16) -> usize {
        usize::from(id % FEC_RX_BLOCK_WINDOW)
    }

    fn closed_state(&self, id: u16) -> Option<bool> {
        self.closed[Self::slot(id)]
            .filter(|c| c.id == id)
            .map(|c| c.complete)
    }

    fn mark_closed(&mut self, id: u16, complete: bool) {
        self.closed[Self::slot(id)] = Some(Closed { id, complete });
    }

    /// Parse and absorb one framed datagram. Never panics on any bytes.
    pub fn on_datagram(&mut self, dg: &[u8], now_us: u64) -> RxOutcome {
        self.last_now = self.last_now.max(now_us);
        let mut out = RxOutcome::default();
        match frame::parse(dg) {
            Err(_) => self.stats.malformed += 1,
            Ok(Frame::Plain(PlainFrame {
                bundle_seq,
                records,
            })) => {
                self.stats.plain_rx += 1;
                out.arrival = Some(bundle_seq);
                dispatch(
                    records,
                    bundle_seq,
                    false,
                    &mut out.records,
                    &mut self.stats.malformed,
                );
            }
            Ok(Frame::Source(f)) => {
                self.stats.sources_rx += 1;
                out.arrival = Some(f.bundle_seq);
                self.on_source(f, now_us, &mut out);
            }
            Ok(Frame::Repair(f)) => {
                self.stats.repairs_rx += 1;
                out.arrival = Some(f.bundle_seq);
                self.on_repair(f, now_us, &mut out);
            }
        }
        out
    }

    /// Finds or opens the block; `None` for a block that is closed or too old.
    fn block_index(&mut self, id: u16, now_us: u64) -> Option<usize> {
        if let Some(i) = self.blocks.iter().position(|b| b.id == id) {
            return Some(i);
        }
        if self.closed_state(id).is_some() {
            return None;
        }
        if let Some(n) = self.newest {
            if too_old(n, id) {
                return None;
            }
            if ahead(id, n) {
                self.newest = Some(id);
                self.prune_window(id);
            }
        } else {
            self.newest = Some(id);
        }
        while self.blocks.len() >= FEC_RX_MAX_OPEN_BLOCKS {
            self.evict_oldest(DeclineReason::Pruned);
        }
        self.blocks.push(Block::new(id, now_us));
        Some(self.blocks.len() - 1)
    }

    fn prune_window(&mut self, newest: u16) {
        let mut i = 0;
        while i < self.blocks.len() {
            if too_old(newest, self.blocks[i].id) {
                self.give_up(i, DeclineReason::Pruned);
            } else {
                i += 1;
            }
        }
    }

    fn evict_oldest(&mut self, reason: DeclineReason) {
        if !self.blocks.is_empty() {
            self.give_up(0, reason);
        }
    }

    /// Removes block `i` and queues a DECLINED for it.
    fn give_up(&mut self, i: usize, reason: DeclineReason) {
        let b = self.blocks.remove(i);
        self.held_bytes -= b.bytes;
        self.mark_closed(b.id, false);
        let (k, missing) = b.missing_bitmap();
        self.stats.declined += 1;
        self.stats.blocks_unrecoverable += 1;
        self.pending.push(CloseAction::Decline {
            block_id: b.id,
            reason,
            k,
            missing,
        });
    }

    fn enforce_byte_cap(&mut self) {
        while self.held_bytes > FEC_RX_MAX_HELD_BYTES && !self.blocks.is_empty() {
            self.evict_oldest(DeclineReason::Evicted);
        }
    }

    fn on_source(&mut self, f: SourceFrame<'_>, now_us: u64, out: &mut RxOutcome) {
        let Some(bi) = self.block_index(f.block_id, now_us) else {
            // Closed or stale block. A block that closed complete has delivered
            // every source already; otherwise the records are new to us, unless
            // the block is so old that it was declined or released long ago and
            // the content would paint stale pixels.
            if self.blocks.iter().all(|b| b.id != f.block_id)
                && self.newest.is_some_and(|n| too_old(n, f.block_id))
            {
                self.stats.stale_dropped += 1;
            } else if self.closed_state(f.block_id) != Some(true) {
                dispatch(
                    f.records,
                    f.bundle_seq,
                    false,
                    &mut out.records,
                    &mut self.stats.malformed,
                );
            }
            return;
        };
        if self.blocks[bi].has_source(f.idx) {
            self.stats.duplicates += 1;
            return;
        }
        // Fix k from the LAST flag, cross-checked against what we know.
        let b = &mut self.blocks[bi];
        let last_k = f.last.then(|| u16::from(f.idx) + 1);
        let conflict = match (b.k, last_k) {
            // idx 255 with LAST would make k 256, which no block can have.
            (_, Some(256)) => true,
            (Some(k), Some(l)) => u16::from(k) != l,
            (Some(k), None) => f.idx >= k,
            (None, Some(l)) => b.sources.len() > usize::from(l),
            (None, None) => false,
        };
        dispatch(
            f.records,
            f.bundle_seq,
            false,
            &mut out.records,
            &mut self.stats.malformed,
        );
        if conflict {
            b.sticky = Some(DeclineReason::HeaderMismatch);
            self.stats.rejected_symbols += 1;
            return;
        }
        if b.k.is_none() {
            b.k = last_k.and_then(|l| u8::try_from(l).ok());
        }
        let sym = SourceFrame {
            last: f.last,
            bundle_seq: f.bundle_seq,
            block_id: f.block_id,
            idx: f.idx,
            records: f.records,
        }
        .coded_symbol();
        let n = sym.len();
        b.put_source(f.idx, sym);
        self.held_bytes += n;
        self.settle(bi, out);
        self.enforce_byte_cap();
    }

    fn on_repair(&mut self, f: RepairFrame<'_>, now_us: u64, out: &mut RxOutcome) {
        let Some(bi) = self.block_index(f.block_id, now_us) else {
            self.stats.repair_wasted += 1;
            return;
        };
        let b = &mut self.blocks[bi];
        if b.k.is_some_and(|k| k != f.k) {
            b.sticky = Some(DeclineReason::HeaderMismatch);
            self.stats.rejected_symbols += 1;
            return;
        }
        if b.symbol_len.is_some_and(|l| l != f.payload.len()) {
            b.sticky = Some(DeclineReason::LengthBound);
            self.stats.rejected_symbols += 1;
            return;
        }
        if b.sources.len() > usize::from(f.k) {
            // A source beyond this k is already held.
            b.sticky = Some(DeclineReason::HeaderMismatch);
            self.stats.rejected_symbols += 1;
            return;
        }
        if b.repairs.iter().any(|&(i, _)| i == f.repair_idx) {
            self.stats.duplicates += 1;
            return;
        }
        b.k = Some(f.k);
        b.symbol_len = Some(f.payload.len());
        b.bytes += f.payload.len();
        self.held_bytes += f.payload.len();
        b.repairs.push((f.repair_idx, f.payload.to_vec()));
        self.settle(bi, out);
        self.enforce_byte_cap();
    }

    /// Decode if enough symbols are held; close the block if it is complete.
    fn settle(&mut self, bi: usize, out: &mut RxOutcome) {
        let b = &mut self.blocks[bi];
        if let Some(k) = b.k.map(usize::from) {
            if !b.complete() && !b.unrecoverable && b.held() >= k {
                let need = k - b.held_sources;
                let srcs: Vec<Option<&[u8]>> = (0..k)
                    .map(|i| b.sources.get(i).and_then(|s| s.as_deref()))
                    .collect();
                let reps: Vec<(u8, &[u8])> = b
                    .repairs
                    .iter()
                    .take(need)
                    .map(|(i, p)| (*i, p.as_slice()))
                    .collect();
                let rebuilt = codec::recover(&srcs, &reps).ok().and_then(|miss| {
                    miss.into_iter()
                        .map(|(j, sym)| {
                            let (seq, recs) = frame::source_from_symbol(&sym).ok()?;
                            let recs = recs.to_vec();
                            Some((j, sym, seq, recs))
                        })
                        .collect::<Option<Vec<_>>>()
                });
                match rebuilt {
                    None => b.unrecoverable = true,
                    Some(items) => {
                        self.stats.repair_used += need as u64;
                        b.repairs_used = need;
                        for (j, sym, seq, recs) in items {
                            dispatch(
                                &recs,
                                seq,
                                true,
                                &mut out.records,
                                &mut self.stats.malformed,
                            );
                            out.rebuilt.push(seq);
                            self.stats.rebuilt += 1;
                            self.held_bytes += sym.len();
                            b.put_source(j as u8, sym);
                        }
                    }
                }
            }
        }
        if self.blocks[bi].complete() {
            let b = self.blocks.remove(bi);
            self.held_bytes -= b.bytes;
            self.mark_closed(b.id, true);
            self.stats.blocks_closed_complete += 1;
            self.stats.repair_wasted += (b.repairs.len() - b.repairs_used) as u64;
        }
    }

    /// Blocks whose window ended by `now_us`: returns the actions to send.
    pub fn on_timeout(&mut self, now_us: u64) -> Vec<CloseAction> {
        self.last_now = self.last_now.max(now_us);
        let mut actions = std::mem::take(&mut self.pending);
        let mut i = 0;
        while i < self.blocks.len() {
            if self.blocks[i].close_at > now_us {
                i += 1;
                continue;
            }
            let b = &mut self.blocks[i];
            let sticky = b.sticky.unwrap_or(DeclineReason::Pruned);
            let k_known = b.k.map(usize::from);
            if b.unrecoverable {
                self.give_up(i, DeclineReason::InvalidSymbol);
            } else if b.requested || b.sticky.is_some() || k_known.is_some_and(|k| b.held() >= k) {
                self.give_up(i, sticky);
            } else {
                let (have, missing) = match k_known {
                    Some(k) => (b.held(), k - b.held()),
                    None => (b.held(), 1),
                };
                b.requested = true;
                b.close_at = now_us.saturating_add(WINDOW_US);
                self.stats.requests += 1;
                actions.push(CloseAction::Request(RepairAsk {
                    block_id: b.id,
                    have: u8::try_from(have).unwrap_or(u8::MAX),
                    missing: u8::try_from(missing).unwrap_or(u8::MAX),
                    first_new_idx: b.highest_repair_idx().map_or(0, |h| h.saturating_add(1)),
                }));
                i += 1;
                continue;
            }
            actions.append(&mut self.pending);
        }
        actions
    }

    pub fn poll_timeout(&self) -> Option<u64> {
        if !self.pending.is_empty() {
            return Some(self.last_now);
        }
        self.blocks.iter().map(|b| b.close_at).min()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_protocol::fec::codec::encode_repairs;
    use ghostframe_protocol::fec::frame::{
        push_record, source_from_symbol, PlainFrame, RepairFrame, SourceFrame,
    };
    use ghostframe_protocol::fec::repair_status::DeclineReason;
    use proptest::prelude::*;

    const T: u64 = 1_000_000;
    const WINDOW: u64 = 43_000;

    fn recs(items: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for i in items {
            push_record(&mut v, i);
        }
        v
    }

    fn src(seq: u32, block: u16, idx: u8, last: bool, records: &[u8]) -> Vec<u8> {
        SourceFrame {
            last,
            bundle_seq: seq,
            block_id: block,
            idx,
            records,
        }
        .encode()
    }

    fn rep(seq: u32, block: u16, k: u8, r: u8, idx: u8, payload: &[u8]) -> Vec<u8> {
        RepairFrame {
            bundle_seq: seq,
            block_id: block,
            k,
            r,
            repair_idx: idx,
            payload,
        }
        .encode()
    }

    /// k sources (bundle_seq = base + idx), their datagrams and `r` repair datagrams.
    fn block(
        block_id: u16,
        base: u32,
        payloads: &[Vec<u8>],
        r: u8,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let k = payloads.len();
        let mut srcs = Vec::new();
        let mut syms = Vec::new();
        for (i, p) in payloads.iter().enumerate() {
            let f = SourceFrame {
                last: i + 1 == k,
                bundle_seq: base + i as u32,
                block_id,
                idx: i as u8,
                records: p,
            };
            syms.push(f.coded_symbol());
            srcs.push(f.encode());
        }
        let refs: Vec<&[u8]> = syms.iter().map(|s| s.as_slice()).collect();
        let rows: Vec<u8> = (0..r).collect();
        let reps = encode_repairs(&refs, &rows).unwrap();
        let reps = reps
            .iter()
            .enumerate()
            .map(|(i, p)| rep(base + 100 + i as u32, block_id, k as u8, r, i as u8, p))
            .collect();
        (srcs, reps)
    }

    #[test]
    fn plain_dispatches_every_record() {
        let mut s = BlockStore::new();
        let r = recs(&[b"a", b"bb", b""]);
        let dg = PlainFrame {
            bundle_seq: 9,
            records: &r,
        }
        .encode();
        let o = s.on_datagram(&dg, T);
        assert_eq!(o.arrival, Some(9));
        assert!(o.rebuilt.is_empty());
        let got: Vec<_> = o
            .records
            .iter()
            .map(|x| (x.record_idx, x.bytes.clone(), x.rebuilt, x.bundle_seq))
            .collect();
        assert_eq!(
            got,
            vec![
                (0, b"a".to_vec(), false, 9),
                (1, b"bb".to_vec(), false, 9),
                (2, vec![], false, 9)
            ]
        );
        assert_eq!(s.stats().plain_rx, 1);
        assert_eq!(s.poll_timeout(), None);
    }

    #[test]
    fn source_dispatches_at_once_and_duplicate_is_not_redispatched() {
        let mut s = BlockStore::new();
        let dg = src(5, 1, 0, false, &recs(&[b"x"]));
        let o = s.on_datagram(&dg, T);
        assert_eq!(o.arrival, Some(5));
        assert_eq!(o.records.len(), 1);
        assert!(s.poll_timeout().is_some());
        let o = s.on_datagram(&dg, T + 1);
        assert_eq!(o.arrival, Some(5));
        assert!(o.records.is_empty());
        assert_eq!(s.stats().duplicates, 1);
        assert_eq!(s.stats().sources_rx, 2);
    }

    #[test]
    fn repair_rebuilds_a_missing_source_with_its_records() {
        let payloads = vec![recs(&[b"one"]), recs(&[b"two", b"2b"]), recs(&[b"three"])];
        let (srcs, reps) = block(7, 50, &payloads, 1);
        let mut s = BlockStore::new();
        s.on_datagram(&srcs[0], T);
        s.on_datagram(&srcs[2], T);
        let o = s.on_datagram(&reps[0], T);
        assert_eq!(o.arrival, Some(150));
        assert_eq!(o.rebuilt, vec![51]);
        let got: Vec<_> = o
            .records
            .iter()
            .map(|r| (r.bundle_seq, r.record_idx, r.bytes.clone(), r.rebuilt))
            .collect();
        assert_eq!(
            got,
            vec![
                (51, 0, b"two".to_vec(), true),
                (51, 1, b"2b".to_vec(), true)
            ]
        );
        let st = s.stats();
        assert_eq!((st.rebuilt, st.repair_used, st.repair_wasted), (1, 1, 0));
        assert_eq!(st.blocks_closed_complete, 1);
        assert_eq!(st.held_bytes, 0);
        assert_eq!(s.poll_timeout(), None);
    }

    #[test]
    fn repair_into_a_complete_block_is_wasted() {
        let payloads = vec![recs(&[b"one"]), recs(&[b"two"])];
        let (srcs, reps) = block(7, 50, &payloads, 2);
        let mut s = BlockStore::new();
        s.on_datagram(&srcs[0], T);
        s.on_datagram(&srcs[1], T);
        let o = s.on_datagram(&reps[0], T);
        assert_eq!(o.arrival, Some(150));
        assert!(o.records.is_empty());
        assert_eq!(s.stats().repair_wasted, 1);
        assert_eq!(s.stats().repair_used, 0);
    }

    #[test]
    fn repair_held_before_completion_by_sources_counts_wasted() {
        let payloads = vec![recs(&[b"one"]), recs(&[b"two"])];
        let (srcs, reps) = block(7, 50, &payloads, 2);
        let mut s = BlockStore::new();
        s.on_datagram(&srcs[0], T);
        s.on_datagram(&reps[0], T); // k known now, one source short, needs 1: rebuilds
                                    // (a repair + a source is already k) so the block completed by decode:
        assert_eq!(s.stats().repair_used, 1);
        s.on_datagram(&reps[1], T);
        assert_eq!(s.stats().repair_wasted, 1);
    }

    #[test]
    fn last_flag_fixes_k_and_a_conflicting_repair_is_rejected() {
        let mut s = BlockStore::new();
        s.on_datagram(&src(1, 3, 0, false, &recs(&[b"a"])), T);
        s.on_datagram(&src(3, 3, 2, true, &recs(&[b"c"])), T); // k = 3
        let sym = vec![0u8; 12];
        let o = s.on_datagram(&rep(9, 3, 4, 1, 0, &sym), T); // k = 4: disagrees
        assert_eq!(o.arrival, Some(9));
        assert_eq!(s.stats().rejected_symbols, 1);
        // A marked block declines at its first close, with no request first.
        let a = s.on_timeout(T + WINDOW);
        assert_eq!(a.len(), 1);
        assert_eq!(s.stats().requests, 0);
        match a[0] {
            CloseAction::Decline {
                block_id,
                reason,
                k,
                missing,
            } => {
                assert_eq!((block_id, k), (3, 3));
                assert_eq!(reason, DeclineReason::HeaderMismatch);
                assert_eq!(missing[0], 0b010);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn repair_length_mismatch_is_rejected_and_declines_as_length_bound() {
        let mut s = BlockStore::new();
        s.on_datagram(&rep(9, 3, 5, 2, 0, &[0u8; 20]), T);
        s.on_datagram(&rep(10, 3, 5, 2, 1, &[0u8; 21]), T);
        assert_eq!(s.stats().rejected_symbols, 1);
        let a = s.on_timeout(T + WINDOW);
        assert!(matches!(
            a[0],
            CloseAction::Decline {
                reason: DeclineReason::LengthBound,
                ..
            }
        ));
    }

    #[test]
    fn late_original_after_rebuild_is_not_redispatched() {
        let payloads = vec![recs(&[b"one"]), recs(&[b"two"]), recs(&[b"three"])];
        let (srcs, reps) = block(7, 50, &payloads, 1);
        let mut s = BlockStore::new();
        s.on_datagram(&srcs[0], T);
        s.on_datagram(&srcs[2], T);
        let o = s.on_datagram(&reps[0], T);
        assert_eq!(o.rebuilt, vec![51]);
        let o = s.on_datagram(&srcs[1], T + 5);
        assert_eq!(o.arrival, Some(51));
        assert!(o.records.is_empty());
        assert!(o.rebuilt.is_empty());
    }

    #[test]
    fn hostile_input_is_counted_not_fatal() {
        let mut s = BlockStore::new();
        for dg in [&[][..], &[0x77, 1, 2][..], &[0x01, 0, 0][..]] {
            let o = s.on_datagram(dg, T);
            assert_eq!(o, RxOutcome::default());
        }
        assert_eq!(s.stats().malformed, 3);
        // A record overrun keeps the records before it and counts malformed.
        let mut r = recs(&[b"ok"]);
        r.extend_from_slice(&9u16.to_le_bytes());
        r.extend_from_slice(b"ab");
        let o = s.on_datagram(&src(1, 0, 0, false, &r), T);
        assert_eq!(o.arrival, Some(1));
        assert_eq!(o.records.len(), 1);
        assert_eq!(s.stats().malformed, 4);
    }

    #[test]
    fn undecodable_block_is_declined_invalid_symbol_without_a_request() {
        // A repair claiming a symbol shorter than its own source: recover errors.
        let mut s = BlockStore::new();
        s.on_datagram(&src(1, 4, 0, false, &recs(&[b"long long long"])), T);
        s.on_datagram(&rep(9, 4, 2, 1, 0, &[0u8; 3]), T);
        let a = s.on_timeout(T + WINDOW);
        assert!(matches!(
            a[0],
            CloseAction::Decline {
                block_id: 4,
                reason: DeclineReason::InvalidSymbol,
                ..
            }
        ));
        assert_eq!(s.stats().blocks_unrecoverable, 1);
    }

    #[test]
    fn sixty_fifth_block_evicts_the_oldest() {
        let mut s = BlockStore::new();
        for b in 0..65u16 {
            s.on_datagram(
                &src(u32::from(b), b, 0, false, &recs(&[b"x"])),
                T + u64::from(b),
            );
        }
        let a = s.on_timeout(T + 100); // before any window ends
        assert_eq!(a.len(), 1);
        assert!(matches!(
            a[0],
            CloseAction::Decline {
                block_id: 0,
                reason: DeclineReason::Pruned,
                ..
            }
        ));
        // And it is gone for good: no Request for block 0 later.
        let later = s.on_timeout(T + 10 * WINDOW);
        assert!(later.iter().all(|a| !matches!(
            a,
            CloseAction::Request(ask) if ask.block_id == 0
        )));
    }

    #[test]
    fn byte_cap_evicts_with_reason_evicted() {
        let mut s = BlockStore::new();
        let big = recs(&[&[7u8; 1100]]);
        let mut seq = 0u32;
        for blk in 0..60u16 {
            // Fat blocks: 100 sources each, so the byte cap bites before the count cap.
            for i in 0..100u8 {
                s.on_datagram(&src(seq, blk, i, false, &big), T + u64::from(blk));
                seq += 1;
            }
        }
        assert!(s.stats().held_bytes <= FEC_RX_MAX_HELD_BYTES as u64);
        let a = s.on_timeout(T + 100);
        assert!(!a.is_empty());
        assert!(a.iter().all(|a| matches!(
            a,
            CloseAction::Decline {
                reason: DeclineReason::Evicted,
                ..
            }
        )));
    }

    #[test]
    fn block_far_behind_the_newest_is_pruned_across_the_wrap() {
        let mut s = BlockStore::new();
        s.on_datagram(&src(1, 65_500, 0, false, &recs(&[b"x"])), T);
        s.on_datagram(&src(2, 65_535, 0, false, &recs(&[b"x"])), T);
        // 65_500 is 36 behind 65_535 and survives; the wrap puts block 5 ahead.
        s.on_datagram(&src(3, 5, 0, false, &recs(&[b"x"])), T);
        assert!(s.on_timeout(T + 1).is_empty());
        s.on_datagram(&src(4, 1100, 0, false, &recs(&[b"x"])), T);
        let a = s.on_timeout(T + 1);
        let ids: Vec<u16> = a
            .iter()
            .map(|a| match a {
                CloseAction::Decline {
                    block_id,
                    reason: DeclineReason::Pruned,
                    ..
                } => *block_id,
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(ids, vec![65_500, 65_535, 5]);
    }

    #[test]
    fn window_requests_once_then_declines_with_a_missing_bitmap() {
        let payloads: Vec<Vec<u8>> = (0..10).map(|i| recs(&[&[i; 8]])).collect();
        let (srcs, reps) = block(9, 0, &payloads, 3);
        let mut s = BlockStore::new();
        for i in [0usize, 1, 3, 4, 5, 6, 7, 8] {
            s.on_datagram(&srcs[i], T);
        }
        // Source 9 (LAST) is among those sent? no: we withheld 2 and 9. k is unknown.
        assert_eq!(s.poll_timeout(), Some(T + WINDOW));
        assert!(s.on_timeout(T + WINDOW - 1).is_empty());
        let a = s.on_timeout(T + WINDOW);
        match a[0] {
            CloseAction::Request(ask) => {
                assert_eq!((ask.block_id, ask.have, ask.missing), (9, 8, 1));
                assert_eq!(ask.first_new_idx, 0);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.stats().requests, 1);
        assert_eq!(s.poll_timeout(), Some(T + 2 * WINDOW));
        // One repair arrives: k = 10, held 9, still one short.
        s.on_datagram(&reps[1], T + WINDOW + 10);
        let a = s.on_timeout(T + 2 * WINDOW);
        match a[0] {
            CloseAction::Decline {
                block_id,
                reason,
                k,
                missing,
            } => {
                assert_eq!((block_id, reason, k), (9, DeclineReason::Pruned, 10));
                assert_eq!(missing[0], 0b100);
                assert_eq!(missing[1], 0b10);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(s.stats().blocks_unrecoverable, 1);
        assert_eq!(s.stats().declined, 1);
        assert_eq!(s.poll_timeout(), None);
        assert!(s.on_timeout(T + 10 * WINDOW).is_empty());
    }

    #[test]
    fn complete_block_closes_silently() {
        let payloads = vec![recs(&[b"a"]), recs(&[b"b"])];
        let (srcs, _) = block(2, 0, &payloads, 1);
        let mut s = BlockStore::new();
        s.on_datagram(&srcs[0], T);
        s.on_datagram(&srcs[1], T);
        assert_eq!(s.stats().blocks_closed_complete, 1);
        assert!(s.on_timeout(T + 10 * WINDOW).is_empty());
        // A late duplicate must not reopen the block.
        let o = s.on_datagram(&srcs[0], T + 5 * WINDOW);
        assert!(o.records.is_empty());
        assert!(s.on_timeout(T + 20 * WINDOW).is_empty());
    }

    #[test]
    fn ten_sources_three_lost_end_to_end() {
        let payloads: Vec<Vec<u8>> = (0..10u8)
            .map(|i| {
                let a = vec![i; 1 + usize::from(i) * 37];
                let b = vec![i ^ 0x55; usize::from(i) * 5];
                recs(&[&a, &b])
            })
            .collect();
        let (srcs, reps) = block(11, 1000, &payloads, 3);
        let mut dgs: Vec<Vec<u8>> = Vec::new();
        for (i, d) in srcs.iter().enumerate() {
            if ![2, 5, 9].contains(&i) {
                dgs.push(d.clone());
            }
        }
        dgs.extend(reps.iter().cloned());
        // Deterministic shuffle.
        let mut x = 0x2545_f491_4f6c_dd1du64;
        for i in (1..dgs.len()).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            dgs.swap(i, (x % (i as u64 + 1)) as usize);
        }
        let mut s = BlockStore::new();
        let mut out: Vec<RxRecord> = Vec::new();
        let mut rebuilt: Vec<u32> = Vec::new();
        for d in &dgs {
            let o = s.on_datagram(d, T);
            assert!(o.arrival.is_some());
            out.extend(o.records);
            rebuilt.extend(o.rebuilt);
        }
        rebuilt.sort_unstable();
        assert_eq!(rebuilt, vec![1002, 1005, 1009]);
        for (i, p) in payloads.iter().enumerate() {
            let seq = 1000 + i as u32;
            let mine: Vec<&RxRecord> = out.iter().filter(|r| r.bundle_seq == seq).collect();
            let want: Vec<Vec<u8>> = ghostframe_protocol::fec::frame::records(p)
                .map(|r| r.unwrap().to_vec())
                .collect();
            assert_eq!(mine.len(), want.len(), "bundle {seq}");
            for (j, (m, w)) in mine.iter().zip(&want).enumerate() {
                assert_eq!(m.record_idx as usize, j);
                assert_eq!(&m.bytes, w);
                assert_eq!(m.rebuilt, [2, 5, 9].contains(&i));
            }
        }
        // The helper used by the store agrees with the frame module.
        let sym = {
            let f = SourceFrame {
                last: false,
                bundle_seq: 1,
                block_id: 0,
                idx: 0,
                records: &payloads[0],
            };
            f.coded_symbol()
        };
        assert_eq!(source_from_symbol(&sym).unwrap().1, &payloads[0][..]);
        assert_eq!(s.stats().rebuilt, 3);
        assert_eq!(s.stats().blocks_closed_complete, 1);
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(64))]
        #[test]
        fn random_bytes_never_panic(
            dgs in proptest::collection::vec(proptest::collection::vec(any::<u8>(), 0..200), 0..40),
            seeds in proptest::collection::vec((0u8..3, any::<u16>(), any::<u8>(), any::<u8>()), 0..40),
        ) {
            let mut s = BlockStore::new();
            let mut now = T;
            for d in &dgs {
                now += 7_000;
                s.on_datagram(d, now);
                let _ = s.on_timeout(now);
                let _ = s.poll_timeout();
            }
            // Well-formed headers with random payloads reach deeper code.
            for (kind, block, a, b) in seeds {
                now += 3_000;
                let body = vec![b; usize::from(a) % 40];
                let dg = match kind {
                    0 => src(u32::from(a), block % 8, a % 12, b % 5 == 0, &body),
                    1 => rep(u32::from(a), block % 8, 1 + a % 12, 1 + b % 4, b % 3, &body),
                    _ => PlainFrame { bundle_seq: u32::from(a), records: &body }.encode(),
                };
                s.on_datagram(&dg, now);
                let _ = s.on_timeout(now + 50_000);
            }
        }
    }

    #[test]
    fn block_exactly_one_window_behind_is_stale_and_not_dispatched() {
        let w = FEC_RX_BLOCK_WINDOW;
        let mut s = BlockStore::new();
        let newest = 2000u16;
        s.on_datagram(&src(1, newest, 0, false, &recs(&[b"n"])), T);
        // One short of the window: still tracked, dispatched.
        let o = s.on_datagram(&src(2, newest - (w - 1), 0, false, &recs(&[b"a"])), T);
        assert_eq!(o.records.len(), 1);
        assert_eq!(s.stats().stale_dropped, 0);
        // Exactly the window behind: its ring slot is the newest block's.
        let o = s.on_datagram(&src(3, newest - w, 0, false, &recs(&[b"b"])), T);
        assert_eq!(o.arrival, Some(3));
        assert!(o.records.is_empty());
        assert_eq!(s.stats().stale_dropped, 1);
    }

    #[test]
    fn last_source_at_idx_255_is_a_header_mismatch() {
        let mut s = BlockStore::new();
        s.on_datagram(&src(1, 3, 255, true, &recs(&[b"z"])), T);
        assert_eq!(s.stats().rejected_symbols, 1);
        let acts = s.on_timeout(T + WINDOW);
        assert!(matches!(
            acts[..],
            [CloseAction::Decline {
                reason: DeclineReason::HeaderMismatch,
                ..
            }]
        ));
    }
}
