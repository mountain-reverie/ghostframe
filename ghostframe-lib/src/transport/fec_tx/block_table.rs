//! Server-side block state machine (rev 6 step R6-d, task D3).
//!
//! Pure and sans-IO: no sockets, no `IoBridge`, time is an argument. The bridge
//! feeds it what happened (a block closed, an ACK, RECOVERED, DECLINED, a
//! REPAIR_REQUEST, a ledger verdict, a timer turn) and it answers with
//! [`Action`]s: encode a repair row, hand tile content to ARQ, tell the H.264
//! manager a fragment is lost, raise an alarm.
//!
//! Phases (rev 5 section 4.9.5): `Sending` (sources registered, the packer's
//! block still open) -> `Repair` (closed, waiting) -> `Deferred` (held(B) >= k:
//! the client's RECOVERED or DECLINED is on its way) -> closed. A closed block
//! is removed; its `by_bundle` entries stay in a tombstone ring so a late
//! verdict finds them and answers [`VerdictAction::Skip`].
//!
//! Every source ends in exactly one [`Outcome`] (acked, recovered, handed off
//! to ARQ, lost to the manager); outcomes are terminal.
//!
//! Interface notes for D2/D4b: this module defines its own minimal input type
//! ([`ClosedBlock`]); it uses no packer types. Source records are
//! `(wire_seq, Content)` pairs.

// Unused until D4b wires the table into the IoBridge.
#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use ghostframe_protocol::fec::feedback::RepairAsk;

use crate::transport::reliable_emitter::Content;

/// Blocks tracked at once (the client's own window).
pub(crate) const FEC_TX_MAX_BLOCKS: usize = 1024;
/// Bytes of cached source symbols.
pub(crate) const FEC_TX_MAX_CACHED_BYTES: usize = 16 << 20;
/// Tombstoned `by_bundle` entries (the size of `RecentBundles`).
pub(crate) const FEC_TX_TOMBSTONES: usize = 4096;
/// Mirrors client-core `FEC_RX_DEFAULT_T_ADV_US` (33 ms).
pub(crate) const CLIENT_T_ADV: Duration = Duration::from_millis(33);
/// Mirrors client-core `FEC_RX_ALLOWANCE_US` (10 ms). The client's request
/// window is `T_ADV + ALLOWANCE` = 43 ms from the block's first arrival.
pub(crate) const CLIENT_ALLOWANCE: Duration = Duration::from_millis(10);
pub(crate) const CLIENT_WINDOW: Duration = Duration::from_millis(43);
/// The web client batches datagrams once per animation frame (rev 6 2.2).
pub(crate) const RAF_ALLOWANCE: Duration = Duration::from_millis(17);
/// Deferred-tile alarm (rev 5 4.9.5).
pub(crate) const T_DEF: Duration = Duration::from_secs(60);
/// H.264 deferral alarm: `2 srtt` plus this.
pub(crate) const H264_DEFERRAL_SLACK: Duration = Duration::from_millis(100);
/// Extra repair rows a block may send beyond its planned `r`.
pub(crate) const EXTRA_ROWS_BEYOND_R: u16 = 4;

/// Live path numbers the deadlines are built from.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Rtt {
    pub srtt: Duration,
    pub rttvar: Duration,
    pub max_ack_delay: Duration,
}

impl Default for Rtt {
    fn default() -> Self {
        Rtt {
            srtt: Duration::from_millis(50),
            rttvar: Duration::from_millis(10),
            max_ack_delay: Duration::ZERO,
        }
    }
}

/// `last symbol sent + 2 x 43 ms + srtt + max_ack_delay`.
pub(crate) fn tile_deadline(last_sent: Instant, r: Rtt) -> Instant {
    last_sent + 2 * CLIENT_WINDOW + r.srtt + r.max_ack_delay
}
/// `last symbol sent + 66 ms + srtt + max_ack_delay + RAF_ALLOWANCE`.
pub(crate) fn h264_deadline(last_sent: Instant, r: Rtt) -> Instant {
    last_sent + 2 * CLIENT_T_ADV + r.srtt + r.max_ack_delay + RAF_ALLOWANCE
}
/// `last symbol sent + 43 ms + srtt + 2 rttvar`.
fn t_lr_at(last_sent: Instant, r: Rtt) -> Instant {
    last_sent + CLIENT_WINDOW + r.srtt + 2 * r.rttvar
}
fn h264_alarm_at(deferred_at: Instant, r: Rtt) -> Instant {
    deferred_at + 2 * r.srtt + H264_DEFERRAL_SLACK
}

pub(crate) type BlockId = u16;
/// `(wire_seq, content)` of one record of a source bundle.
pub(crate) type SourceRecord = (u32, Content);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Source { idx: u8 },
    Repair { row: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Acked,
    Recovered,
    HandedOff,
    Lost,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseReason {
    Accounted,
    Declined,
    HandedOff,
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerdictAction {
    /// Source of an open, unaccounted block: skip the manager and the retransmit.
    Park,
    /// The content outcome was already taken.
    Skip,
    /// A PLAIN bundle, a repair, or unknown: as today.
    Pass,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlarmKind {
    /// H.264 block stayed `Deferred` for `2 srtt + 100 ms`.
    DeferralTimeout,
    /// Tile block stayed `Deferred` for `T_DEF`; should stay 0.
    TDef,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Encode and send repair row `row` of the block (`symbols(block_id)`).
    Repair {
        block_id: BlockId,
        row: u8,
    },
    /// Send these tile records again as PLAIN `Handoff` retransmits.
    HandOffTile {
        block_id: BlockId,
        records: Vec<SourceRecord>,
    },
    /// Tell the acked-reference manager this fragment is lost.
    ContentLostH264 {
        frame_seq: u32,
        frag_idx: u16,
    },
    Alarm {
        block_id: BlockId,
        kind: AlarmKind,
    },
}

/// A block the packer closed. Sources were registered earlier with
/// [`BlockTable::on_source_sealed`].
#[derive(Debug, Clone)]
pub(crate) struct ClosedBlock {
    pub block_id: BlockId,
    pub k: u8,
    /// Planned repair rows `0..r`.
    pub r: u8,
    /// Coded symbols, one per source, for encoding repairs on demand.
    pub symbols: Vec<Vec<u8>>,
    /// `bundle_seq` of each repair actually sealed, row `i` at index `i`.
    /// Empty: every repair was dropped (MTU shrink) or `k == 255`.
    pub repair_bundles: Vec<u32>,
    /// When the last symbol of the block was sent.
    pub last_sent: Instant,
}

/// Counters; the names are the plan's.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BlockStats {
    pub block_evicted: u64,
    pub block_closed_late_trigger: u64,
    pub request_unknown: u64,
    pub request_before_close: u64,
    pub request_over_cap: u64,
    pub request_malformed: u64,
    pub have_mismatch: u64,
    pub declined_unknown: u64,
    pub declined_k_mismatch: u64,
    pub declined_bitmap_extra: u64,
    pub blocks_without_repairs: u64,
    pub deferral_alarm_h264: u64,
    pub t_def_fired: u64,
    pub t_lr_fired: u64,
    pub recovered_after_release: u64,
    pub late_ack_after_release: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Sending,
    Repair,
    Deferred,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Tile,
    H264,
}

#[derive(Debug)]
struct Src {
    idx: u8,
    bundle_seq: u32,
    records: Vec<SourceRecord>,
    kind: Kind,
}

#[derive(Debug)]
struct Block {
    born: u64,
    phase: Phase,
    k: u8,
    r: u8,
    sources: Vec<Src>,
    symbols: Vec<Vec<u8>>,
    sym_bytes: usize,
    /// Distinct ACKed bundles among sources and repairs.
    held: usize,
    accounted: usize,
    released: usize,
    next_row: u16,
    extra_used: u16,
    last_sent: Option<Instant>,
    deferred_at: Option<Instant>,
    t_lr_armed: bool,
    timed_out: bool,
}

#[derive(Debug)]
struct Entry {
    block_id: BlockId,
    role: Role,
    outcome: Option<Outcome>,
}

#[derive(Debug)]
pub(crate) struct BlockTable {
    blocks: HashMap<BlockId, Block>,
    by_bundle: HashMap<u32, Entry>,
    tombstones: VecDeque<u32>,
    closed: HashMap<BlockId, CloseReason>,
    closed_order: VecDeque<BlockId>,
    cached_bytes: usize,
    next_born: u64,
    rtt: Rtt,
    max_blocks: usize,
    max_bytes: usize,
    tomb_cap: usize,
    stats: BlockStats,
}

impl Default for BlockTable {
    fn default() -> Self {
        Self::new()
    }
}

fn kind_of(records: &[SourceRecord]) -> Kind {
    if records.iter().any(|(_, c)| matches!(c, Content::Tile(_))) {
        Kind::Tile
    } else {
        Kind::H264
    }
}

impl BlockTable {
    pub(crate) fn new() -> Self {
        Self::with_caps(
            FEC_TX_MAX_BLOCKS,
            FEC_TX_MAX_CACHED_BYTES,
            FEC_TX_TOMBSTONES,
        )
    }

    pub(crate) fn with_caps(max_blocks: usize, max_bytes: usize, tomb_cap: usize) -> Self {
        BlockTable {
            blocks: HashMap::new(),
            by_bundle: HashMap::new(),
            tombstones: VecDeque::new(),
            closed: HashMap::new(),
            closed_order: VecDeque::new(),
            cached_bytes: 0,
            next_born: 0,
            rtt: Rtt::default(),
            max_blocks,
            max_bytes,
            tomb_cap,
            stats: BlockStats::default(),
        }
    }

    pub(crate) fn stats(&self) -> &BlockStats {
        &self.stats
    }
    pub(crate) fn open_blocks(&self) -> usize {
        self.blocks.len()
    }
    pub(crate) fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }
    pub(crate) fn set_rtt(&mut self, rtt: Rtt) {
        self.rtt = rtt;
    }
    /// The coded symbols of an open block (freed once it closes).
    pub(crate) fn symbols(&self, block_id: BlockId) -> Option<&[Vec<u8>]> {
        self.blocks.get(&block_id).map(|b| b.symbols.as_slice())
    }
    pub(crate) fn closed_reason(&self, block_id: BlockId) -> Option<CloseReason> {
        self.closed.get(&block_id).copied()
    }
    /// The terminal outcome of a source, if it has one.
    pub(crate) fn outcome(&self, bundle_seq: u32) -> Option<Outcome> {
        self.by_bundle.get(&bundle_seq).and_then(|e| e.outcome)
    }
    pub(crate) fn is_open(&self, block_id: BlockId) -> bool {
        self.blocks.contains_key(&block_id)
    }

    // ---- registration -------------------------------------------------

    /// A source was sealed into `block_id`. If the block is already closed
    /// (declined while the packer's block was open), its content is released
    /// at once so nothing is stranded.
    pub(crate) fn on_source_sealed(
        &mut self,
        block_id: BlockId,
        idx: u8,
        bundle_seq: u32,
        records: Vec<SourceRecord>,
    ) -> Vec<Action> {
        let kind = kind_of(&records);
        let mut actions = Vec::new();
        if self.closed.contains_key(&block_id) {
            let outcome = release_records(block_id, kind, &records, &mut actions);
            self.by_bundle.insert(
                bundle_seq,
                Entry {
                    block_id,
                    role: Role::Source { idx },
                    outcome: Some(outcome),
                },
            );
            self.tombstone(bundle_seq);
            return actions;
        }
        let born = self.next_born;
        let b = self.blocks.entry(block_id).or_insert_with(|| Block {
            born,
            phase: Phase::Sending,
            k: 0,
            r: 0,
            sources: Vec::new(),
            symbols: Vec::new(),
            sym_bytes: 0,
            held: 0,
            accounted: 0,
            released: 0,
            next_row: 0,
            extra_used: 0,
            last_sent: None,
            deferred_at: None,
            t_lr_armed: false,
            timed_out: false,
        });
        if b.born == born {
            self.next_born += 1;
        }
        b.sources.push(Src {
            idx,
            bundle_seq,
            records,
            kind,
        });
        self.by_bundle.insert(
            bundle_seq,
            Entry {
                block_id,
                role: Role::Source { idx },
                outcome: None,
            },
        );
        actions
    }

    /// The packer closed the block (sources already registered).
    pub(crate) fn on_closed(&mut self, c: ClosedBlock, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        if self.closed.contains_key(&c.block_id) {
            return actions;
        }
        let Some(mut b) = self.blocks.remove(&c.block_id) else {
            return actions;
        };
        b.k = c.k;
        b.r = c.r;
        b.next_row = u16::from(c.r);
        b.sym_bytes = c.symbols.iter().map(Vec::len).sum();
        self.cached_bytes += b.sym_bytes;
        b.symbols = c.symbols;
        b.last_sent = Some(c.last_sent);
        b.phase = Phase::Repair;
        b.t_lr_armed = true;
        let none_sent = c.repair_bundles.is_empty();
        for (row, bs) in c.repair_bundles.iter().enumerate() {
            self.by_bundle.insert(
                *bs,
                Entry {
                    block_id: c.block_id,
                    role: Role::Repair { row: row as u8 },
                    outcome: None,
                },
            );
        }
        if none_sent {
            // Nothing to wait for: hand off at once.
            self.stats.blocks_without_repairs += 1;
            self.release_unresolved(c.block_id, &mut b, &mut actions);
        }
        self.settle(c.block_id, b, now, &mut actions);
        actions
    }

    // ---- inputs -------------------------------------------------------

    /// An ACK for `bundle_seq` arrived.
    pub(crate) fn on_arrival(&mut self, bundle_seq: u32, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(e) = self.by_bundle.get_mut(&bundle_seq) else {
            return actions;
        };
        if e.outcome.is_some() {
            if matches!(e.outcome, Some(Outcome::HandedOff | Outcome::Lost)) {
                self.stats.late_ack_after_release += 1;
            }
            if !self.blocks.contains_key(&e.block_id) {
                self.stats.block_closed_late_trigger += 1;
            }
            return actions;
        }
        e.outcome = Some(Outcome::Acked);
        let (id, role) = (e.block_id, e.role);
        let Some(mut b) = self.blocks.remove(&id) else {
            return actions;
        };
        b.held += 1;
        if matches!(role, Role::Source { .. }) {
            b.accounted += 1;
        }
        self.settle(id, b, now, &mut actions);
        actions
    }

    /// RECOVERED named this source (never an ACK arrival).
    pub(crate) fn on_recovered(&mut self, bundle_seq: u32, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(e) = self.by_bundle.get_mut(&bundle_seq) else {
            return actions;
        };
        if !matches!(e.role, Role::Source { .. }) {
            return actions;
        }
        match e.outcome {
            None => {}
            Some(Outcome::HandedOff | Outcome::Lost) => {
                self.stats.recovered_after_release += 1;
                return actions;
            }
            Some(_) => return actions,
        }
        e.outcome = Some(Outcome::Recovered);
        let id = e.block_id;
        if let Some(mut b) = self.blocks.remove(&id) {
            b.accounted += 1;
            self.settle(id, b, now, &mut actions);
        }
        actions
    }

    /// DECLINED: hand off every unaccounted source; the bitmap is only a
    /// cross-check (it covers `0..k` when k is known, else the highest idx
    /// seen + 1, so it can omit trailing lost sources).
    pub(crate) fn on_declined(
        &mut self,
        block_id: BlockId,
        k: u8,
        missing_bitmap: &[u8],
        now: Instant,
    ) -> Vec<Action> {
        let mut actions = Vec::new();
        let Some(b) = self.blocks.remove(&block_id) else {
            if self.closed.contains_key(&block_id) {
                self.stats.block_closed_late_trigger += 1;
            } else {
                self.stats.declined_unknown += 1;
            }
            return actions;
        };
        if b.phase != Phase::Sending && k != b.k {
            self.stats.declined_k_mismatch += 1;
        }
        for s in &b.sources {
            let named = missing_bitmap
                .get(usize::from(s.idx) / 8)
                .is_some_and(|byte| byte >> (s.idx % 8) & 1 != 0);
            if named
                && matches!(
                    self.by_bundle.get(&s.bundle_seq).and_then(|e| e.outcome),
                    Some(Outcome::Acked | Outcome::Recovered)
                )
            {
                self.stats.declined_bitmap_extra += 1;
            }
        }
        self.decline(block_id, b, now, &mut actions);
        actions
    }

    fn decline(&mut self, id: BlockId, mut b: Block, _now: Instant, actions: &mut Vec<Action>) {
        self.release_unresolved(id, &mut b, actions);
        self.finish(id, b, CloseReason::Declined);
    }

    /// A REPAIR_REQUEST entry.
    pub(crate) fn on_request(&mut self, ask: RepairAsk, now: Instant) -> Vec<Action> {
        let mut actions = Vec::new();
        let id = ask.block_id;
        let Some(mut b) = self.blocks.remove(&id) else {
            if self.closed.contains_key(&id) {
                self.stats.block_closed_late_trigger += 1;
            } else {
                self.stats.request_unknown += 1;
            }
            return actions;
        };
        if b.phase == Phase::Sending {
            self.stats.request_before_close += 1;
            self.blocks.insert(id, b);
            return actions;
        }
        if ask.have >= b.k {
            // Malformed: a client holding k symbols does not ask.
            self.stats.request_malformed += 1;
            self.decline(id, b, now, &mut actions);
            return actions;
        }
        b.t_lr_armed = false;
        if b.phase == Phase::Deferred {
            self.stats.have_mismatch += 1;
            b.phase = Phase::Repair;
            b.deferred_at = None;
        }
        let max_row = 255 - u16::from(b.k);
        let mut row = u16::from(ask.first_new_idx).max(b.next_row);
        let cap = u16::from(b.r) + EXTRA_ROWS_BEYOND_R;
        for _ in 0..ask.missing {
            if row > max_row {
                break;
            }
            if b.extra_used >= cap {
                self.stats.request_over_cap += 1;
                break;
            }
            actions.push(Action::Repair {
                block_id: id,
                row: row as u8,
            });
            b.extra_used += 1;
            row += 1;
            b.next_row = row;
        }
        self.blocks.insert(id, b);
        actions
    }

    /// The PTO wants one repair row of this block instead of a source. A
    /// block in `Deferred` (or unknown) is skipped.
    pub(crate) fn on_pto(&mut self, block_id: BlockId) -> Option<Action> {
        let b = self.blocks.get_mut(&block_id)?;
        if b.phase != Phase::Repair {
            return None;
        }
        next_extra_row(block_id, b)
    }

    /// What a ledger verdict on `bundle_seq` means for the content.
    pub(crate) fn on_verdict(&self, bundle_seq: u32) -> VerdictAction {
        match self.by_bundle.get(&bundle_seq) {
            None => VerdictAction::Pass,
            Some(Entry {
                role: Role::Repair { .. },
                ..
            }) => VerdictAction::Pass,
            Some(Entry {
                outcome: Some(_), ..
            }) => VerdictAction::Skip,
            Some(e) => {
                if self.blocks.contains_key(&e.block_id) {
                    VerdictAction::Park
                } else {
                    VerdictAction::Pass
                }
            }
        }
    }

    /// Service timers: `T_lr`, deadlines, deferral alarms.
    pub(crate) fn on_timer(&mut self, now: Instant, rtt: Rtt) -> Vec<Action> {
        self.rtt = rtt;
        let mut actions = Vec::new();
        let mut ids: Vec<BlockId> = self.blocks.keys().copied().collect();
        ids.sort_by_key(|i| self.blocks[i].born);
        for id in ids {
            let Some(mut b) = self.blocks.remove(&id) else {
                continue;
            };
            match b.phase {
                Phase::Sending => {}
                Phase::Repair => {
                    let last = b.last_sent.unwrap_or(now);
                    if b.t_lr_armed && now >= t_lr_at(last, rtt) {
                        b.t_lr_armed = false;
                        self.stats.t_lr_fired += 1;
                        if let Some(a) = next_extra_row(id, &mut b) {
                            actions.push(a);
                        }
                    }
                    let due: Vec<usize> = (0..b.sources.len())
                        .filter(|&i| self.src_open(&b, i))
                        .filter(|&i| {
                            let d = match b.sources[i].kind {
                                Kind::Tile => tile_deadline(last, rtt),
                                Kind::H264 => h264_deadline(last, rtt),
                            };
                            now >= d
                        })
                        .collect();
                    self.release_idxs(id, &mut b, &due, &mut actions);
                }
                Phase::Deferred => {
                    let at = b.deferred_at.unwrap_or(now);
                    let mut due = Vec::new();
                    let (mut h264_fired, mut tile_fired) = (false, false);
                    for i in 0..b.sources.len() {
                        if !self.src_open(&b, i) {
                            continue;
                        }
                        match b.sources[i].kind {
                            Kind::H264 if now >= h264_alarm_at(at, rtt) => {
                                h264_fired = true;
                                due.push(i);
                            }
                            Kind::Tile if now >= at + T_DEF => {
                                tile_fired = true;
                                due.push(i);
                            }
                            Kind::Tile | Kind::H264 => {}
                        }
                    }
                    if h264_fired {
                        self.stats.deferral_alarm_h264 += 1;
                        b.timed_out = true;
                        actions.push(Action::Alarm {
                            block_id: id,
                            kind: AlarmKind::DeferralTimeout,
                        });
                    }
                    if tile_fired {
                        self.stats.t_def_fired += 1;
                        actions.push(Action::Alarm {
                            block_id: id,
                            kind: AlarmKind::TDef,
                        });
                    }
                    self.release_idxs(id, &mut b, &due, &mut actions);
                }
            }
            self.settle(id, b, now, &mut actions);
        }
        actions
    }

    /// The earliest instant `on_timer` has work, from the last known RTT.
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        let rtt = self.rtt;
        let mut best: Option<Instant> = None;
        let mut take = |t: Instant| best = Some(best.map_or(t, |c| c.min(t)));
        for b in self.blocks.values() {
            match b.phase {
                Phase::Sending => {}
                Phase::Repair => {
                    let Some(last) = b.last_sent else { continue };
                    if b.t_lr_armed {
                        take(t_lr_at(last, rtt));
                    }
                    for (i, s) in b.sources.iter().enumerate() {
                        if self.src_open(b, i) {
                            take(match s.kind {
                                Kind::Tile => tile_deadline(last, rtt),
                                Kind::H264 => h264_deadline(last, rtt),
                            });
                        }
                    }
                }
                Phase::Deferred => {
                    let Some(at) = b.deferred_at else { continue };
                    for (i, s) in b.sources.iter().enumerate() {
                        if self.src_open(b, i) {
                            take(match s.kind {
                                Kind::Tile => at + T_DEF,
                                Kind::H264 => h264_alarm_at(at, rtt),
                            });
                        }
                    }
                }
            }
        }
        best
    }

    /// Evict the oldest blocks past the count or byte cap, as `HandedOff`.
    pub(crate) fn evict_over_cap(&mut self) -> Vec<Action> {
        let mut actions = Vec::new();
        while self.blocks.len() > self.max_blocks || self.cached_bytes > self.max_bytes {
            let Some(id) = self
                .blocks
                .iter()
                .min_by_key(|(_, b)| b.born)
                .map(|(id, _)| *id)
            else {
                break;
            };
            let mut b = self.blocks.remove(&id).expect("just found");
            self.stats.block_evicted += 1;
            self.release_unresolved(id, &mut b, &mut actions);
            self.finish(id, b, CloseReason::HandedOff);
        }
        actions
    }

    /// Session reset: forget every block. Counters survive.
    pub(crate) fn reset(&mut self) {
        self.blocks.clear();
        self.by_bundle.clear();
        self.tombstones.clear();
        self.closed.clear();
        self.closed_order.clear();
        self.cached_bytes = 0;
    }

    // ---- internals ----------------------------------------------------

    fn src_open(&self, b: &Block, i: usize) -> bool {
        self.by_bundle
            .get(&b.sources[i].bundle_seq)
            .is_some_and(|e| e.outcome.is_none())
    }

    fn release_unresolved(&mut self, id: BlockId, b: &mut Block, actions: &mut Vec<Action>) {
        let all: Vec<usize> = (0..b.sources.len())
            .filter(|&i| self.src_open(b, i))
            .collect();
        self.release_idxs(id, b, &all, actions);
    }

    fn release_idxs(
        &mut self,
        id: BlockId,
        b: &mut Block,
        idxs: &[usize],
        actions: &mut Vec<Action>,
    ) {
        for &i in idxs {
            let s = &b.sources[i];
            let o = release_records(id, s.kind, &s.records, actions);
            if let Some(e) = self.by_bundle.get_mut(&s.bundle_seq) {
                e.outcome = Some(o);
            }
            b.released += 1;
        }
    }

    /// Re-evaluate phase after a change; closes the block or puts it back.
    fn settle(&mut self, id: BlockId, mut b: Block, now: Instant, _actions: &mut Vec<Action>) {
        if b.phase == Phase::Sending {
            self.blocks.insert(id, b);
            return;
        }
        let n = b.sources.len();
        if b.accounted == n {
            self.finish(id, b, CloseReason::Accounted);
        } else if b.accounted + b.released == n {
            let why = if b.timed_out {
                CloseReason::TimedOut
            } else {
                CloseReason::HandedOff
            };
            self.finish(id, b, why);
        } else {
            if b.phase == Phase::Repair && b.held >= usize::from(b.k) {
                b.phase = Phase::Deferred;
                b.deferred_at = Some(now);
                b.t_lr_armed = false;
            }
            self.blocks.insert(id, b);
        }
    }

    fn finish(&mut self, id: BlockId, b: Block, why: CloseReason) {
        self.cached_bytes -= b.sym_bytes;
        for s in &b.sources {
            self.tombstone(s.bundle_seq);
        }
        // Repair entries: find them by id (few, tombstone by scan).
        let repairs: Vec<u32> = self
            .by_bundle
            .iter()
            .filter(|(_, e)| e.block_id == id && matches!(e.role, Role::Repair { .. }))
            .map(|(k, _)| *k)
            .collect();
        for r in repairs {
            self.tombstone(r);
        }
        self.closed.insert(id, why);
        self.closed_order.push_back(id);
        while self.closed_order.len() > self.max_blocks.max(1) {
            if let Some(old) = self.closed_order.pop_front() {
                self.closed.remove(&old);
            }
        }
    }

    fn tombstone(&mut self, bundle_seq: u32) {
        self.tombstones.push_back(bundle_seq);
        while self.tombstones.len() > self.tomb_cap {
            if let Some(old) = self.tombstones.pop_front() {
                self.by_bundle.remove(&old);
            }
        }
    }
}

fn next_extra_row(id: BlockId, b: &mut Block) -> Option<Action> {
    let max_row = 255 - u16::from(b.k);
    if b.next_row > max_row || b.extra_used >= u16::from(b.r) + EXTRA_ROWS_BEYOND_R {
        return None;
    }
    let row = b.next_row as u8;
    b.next_row += 1;
    b.extra_used += 1;
    Some(Action::Repair { block_id: id, row })
}

/// Emit the content actions for one source; return its outcome.
fn release_records(
    block_id: BlockId,
    kind: Kind,
    records: &[SourceRecord],
    actions: &mut Vec<Action>,
) -> Outcome {
    let mut tile = Vec::new();
    for &(ws, c) in records {
        match c {
            Content::Tile(_) => tile.push((ws, c)),
            Content::H264Frag {
                frame_seq,
                frag_idx,
            } => actions.push(Action::ContentLostH264 {
                frame_seq,
                frag_idx,
            }),
            Content::Pad(_) => {}
        }
    }
    if !tile.is_empty() {
        actions.push(Action::HandOffTile {
            block_id,
            records: tile,
        });
    }
    match kind {
        Kind::Tile => Outcome::HandedOff,
        Kind::H264 => Outcome::Lost,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::reliable_emitter::EmitKey;

    const BLK: BlockId = 7;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
    fn rtt(srtt_ms: u64) -> Rtt {
        Rtt {
            srtt: ms(srtt_ms),
            rttvar: ms(5),
            max_ack_delay: ms(0),
        }
    }
    fn tile(i: u32) -> Content {
        Content::Tile(EmitKey::new(i, 0, 0, 0))
    }
    fn frag(i: u32) -> Content {
        Content::H264Frag {
            frame_seq: 1,
            frag_idx: i as u16,
        }
    }
    fn src(id: BlockId, base: u32, i: u32) -> u32 {
        let _ = id;
        base + i
    }

    /// A closed block `BLK` of `k` sources (bundle_seq 100+i) and `r` repairs
    /// (200+j); content from `mk`.
    fn setup(k: u8, r: u8, mk: impl Fn(u32) -> Content, t0: Instant) -> BlockTable {
        setup_id(BLK, k, r, mk, t0, BlockTable::new())
    }
    fn setup_id(
        id: BlockId,
        k: u8,
        r: u8,
        mk: impl Fn(u32) -> Content,
        t0: Instant,
        mut t: BlockTable,
    ) -> BlockTable {
        for i in 0..u32::from(k) {
            let ws = 1000 + i;
            assert!(t
                .on_source_sealed(id, i as u8, src(id, 100, i), vec![(ws, mk(i))])
                .is_empty());
        }
        let acts = t.on_closed(
            ClosedBlock {
                block_id: id,
                k,
                r,
                symbols: vec![vec![0u8; 10]; usize::from(k)],
                repair_bundles: (0..u32::from(r)).map(|j| 200 + j).collect(),
                last_sent: t0,
            },
            t0,
        );
        assert!(acts.is_empty());
        t
    }
    fn handoffs(a: &[Action]) -> usize {
        a.iter()
            .filter(|x| matches!(x, Action::HandOffTile { .. }))
            .count()
    }
    fn losts(a: &[Action]) -> usize {
        a.iter()
            .filter(|x| matches!(x, Action::ContentLostH264 { .. }))
            .count()
    }
    fn ask(have: u8, missing: u8, first_new_idx: u8) -> RepairAsk {
        RepairAsk {
            block_id: BLK,
            have,
            missing,
            first_new_idx,
        }
    }

    #[test]
    fn acks_reaching_k_defer_then_all_accounted_closes_and_frees() {
        let t0 = Instant::now();
        let mut t = setup(4, 2, tile, t0);
        assert_eq!(t.cached_bytes(), 40);
        for s in [100, 101, 102] {
            t.on_arrival(s, t0);
        }
        assert_eq!(t.blocks[&BLK].phase, Phase::Repair);
        t.on_arrival(200, t0); // a repair counts toward held
        assert_eq!(t.blocks[&BLK].phase, Phase::Deferred);
        assert!(t.symbols(BLK).is_some());
        t.on_recovered(103, t0);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Accounted));
        assert!(t.symbols(BLK).is_none());
        assert_eq!(t.cached_bytes(), 0);
        // duplicate ACK of a closed block's source: counted, nothing else.
        assert!(t.on_arrival(100, t0).is_empty());
        assert_eq!(t.stats().block_closed_late_trigger, 1);
    }

    #[test]
    fn declined_before_and_after_held_reaches_k() {
        let t0 = Instant::now();
        // before: held 1 of k=3
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        let a = t.on_declined(BLK, 3, &[0b110], t0);
        assert_eq!(handoffs(&a), 2);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Declined));
        // after: Deferred, one RECOVERED, then DECLINED for the rest
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        t.on_arrival(101, t0);
        t.on_arrival(200, t0);
        assert_eq!(t.blocks[&BLK].phase, Phase::Deferred);
        let a = t.on_declined(BLK, 3, &[0b100], t0);
        assert_eq!(handoffs(&a), 1);
        assert_eq!(t.outcome(102), Some(Outcome::HandedOff));
        assert_eq!(t.outcome(100), Some(Outcome::Acked));
        // a second DECLINED is a late trigger
        assert!(t.on_declined(BLK, 3, &[0b100], t0).is_empty());
        assert_eq!(t.stats().block_closed_late_trigger, 1);
        assert!(t.on_declined(999, 3, &[1], t0).is_empty());
        assert_eq!(t.stats().declined_unknown, 1);
    }

    #[test]
    fn recovered_before_verdict_and_verdict_first() {
        let t0 = Instant::now();
        let mut t = setup(2, 1, frag, t0);
        // verdict first: parked, no manager call
        assert_eq!(t.on_verdict(100), VerdictAction::Park);
        // RECOVERED after the verdict: delivered, the verdict stays unused
        t.on_recovered(100, t0);
        assert_eq!(t.on_verdict(100), VerdictAction::Skip);
        assert_eq!(t.outcome(100), Some(Outcome::Recovered));
        // RECOVERED before the verdict on the other source
        t.on_recovered(101, t0);
        assert_eq!(t.on_verdict(101), VerdictAction::Skip);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Accounted));
        // late verdict on the closed block's source: Skip (entry outlived it)
        assert_eq!(t.on_verdict(100), VerdictAction::Skip);
        assert_eq!(t.on_verdict(200), VerdictAction::Pass); // repair
        assert_eq!(t.on_verdict(5555), VerdictAction::Pass); // PLAIN / unknown
    }

    #[test]
    fn deadline_in_repair_hands_off_tiles() {
        let t0 = Instant::now();
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        let r = rtt(50);
        let d = tile_deadline(t0, r);
        assert!(t.next_deadline().is_some());
        assert_eq!(handoffs(&t.on_timer(d - ms(1), r)), 0);
        let a = t.on_timer(d, r);
        assert_eq!(handoffs(&a), 2);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::HandedOff));
    }

    #[test]
    fn deferred_tiles_are_not_released_at_the_deadline() {
        let t0 = Instant::now();
        let mut t = setup(3, 1, tile, t0);
        for s in [100, 101, 102] {
            t.on_arrival(s, t0); // all accounted would close; use 2 + repair
            if s == 101 {
                break;
            }
        }
        t.on_arrival(200, t0);
        assert_eq!(t.blocks[&BLK].phase, Phase::Deferred);
        let r = rtt(50);
        let a = t.on_timer(tile_deadline(t0, r) + ms(1000), r);
        assert_eq!(handoffs(&a), 0);
        assert!(t.is_open(BLK));
        // T_def is the alarm: hands off and counts
        let a = t.on_timer(t0 + T_DEF + ms(1), r);
        assert_eq!(handoffs(&a), 1);
        assert_eq!(t.stats().t_def_fired, 1);
    }

    #[test]
    fn deferred_h264_waits_for_alarm_not_deadline_at_high_rtt() {
        let t0 = Instant::now();
        let r = rtt(150);
        let mut t = setup(3, 1, frag, t0);
        t.on_arrival(100, t0);
        t.on_arrival(101, t0);
        t.on_arrival(200, t0);
        assert_eq!(t.blocks[&BLK].phase, Phase::Deferred);
        // Property: from the close until just before the alarm, at every
        // 1 ms step, the manager is never told a fragment is lost.
        let alarm = t0 + 2 * r.srtt + H264_DEFERRAL_SLACK;
        let mut now = t0;
        while now < alarm {
            assert_eq!(losts(&t.on_timer(now, r)), 0, "lost at {:?}", now - t0);
            now += ms(1);
        }
        assert!(
            now > h264_deadline(t0, r),
            "the loop must cross the deadline"
        );
        let a = t.on_timer(alarm, r);
        assert_eq!(losts(&a), 1);
        assert!(a.iter().any(|x| matches!(
            x,
            Action::Alarm {
                kind: AlarmKind::DeferralTimeout,
                ..
            }
        )));
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::TimedOut));
        assert_eq!(t.stats().deferral_alarm_h264, 1);
    }

    #[test]
    fn h264_deadline_in_repair_reports_lost() {
        let t0 = Instant::now();
        let r = rtt(50);
        let mut t = setup(2, 1, frag, t0);
        let a = t.on_timer(h264_deadline(t0, r), r);
        assert_eq!(losts(&a), 2);
    }

    #[test]
    fn declined_with_unknown_k_bitmap_missing_trailing_sources_hands_off_all() {
        let t0 = Instant::now();
        let mut t = setup(6, 2, tile, t0);
        t.on_arrival(100, t0);
        // The client saw sources 0..=2 only (k unknown, bitmap covers 0..3):
        // it names idx 1 and says nothing of the lost 3, 4, 5.
        let a = t.on_declined(BLK, 3, &[0b0000_0010], t0);
        assert_eq!(handoffs(&a), 5, "every unaccounted source, not the bitmap");
        for s in 101..106 {
            assert_eq!(t.outcome(s), Some(Outcome::HandedOff));
        }
        assert_eq!(t.stats().declined_k_mismatch, 1);
        // Cross-check: the bitmap naming an accounted source is counted.
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        t.on_declined(BLK, 3, &[0b001], t0);
        assert_eq!(t.stats().declined_bitmap_extra, 1);
    }

    #[test]
    fn block_without_repairs_hands_off_at_once() {
        let t0 = Instant::now();
        let mut t = BlockTable::new();
        t.on_source_sealed(BLK, 0, 100, vec![(1, tile(0))]);
        t.on_source_sealed(BLK, 1, 101, vec![(2, frag(1))]);
        let a = t.on_closed(
            ClosedBlock {
                block_id: BLK,
                k: 2,
                r: 2,
                symbols: vec![vec![0; 4]; 2],
                repair_bundles: vec![],
                last_sent: t0,
            },
            t0,
        );
        assert_eq!((handoffs(&a), losts(&a)), (1, 1));
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::HandedOff));
        assert_eq!(t.cached_bytes(), 0);
        assert_eq!(t.stats().blocks_without_repairs, 1);
    }

    #[test]
    fn t_lr_fires_once_and_each_cancel_stops_it() {
        let t0 = Instant::now();
        let r = rtt(50);
        let due = t_lr_at(t0, r);
        // fires once, row = next unused (r = 2 -> row 2)
        let mut t = setup(4, 2, tile, t0);
        t.set_rtt(r);
        assert_eq!(t.next_deadline(), Some(due));
        let a = t.on_timer(due, r);
        assert_eq!(
            a,
            vec![Action::Repair {
                block_id: BLK,
                row: 2
            }]
        );
        assert!(t.on_timer(due + ms(5), r).is_empty());
        // cancelled by a REPAIR_REQUEST
        let mut t = setup(4, 2, tile, t0);
        t.on_request(ask(1, 0, 0), t0);
        assert!(t.on_timer(due, r).is_empty());
        // cancelled by entering Deferred
        let mut t = setup(4, 2, tile, t0);
        for s in [100, 101, 102] {
            t.on_arrival(s, t0);
        }
        t.on_arrival(200, t0);
        assert!(t.on_timer(due, r).is_empty());
        // cancelled by DECLINED / all accounted (block gone)
        let mut t = setup(2, 1, tile, t0);
        t.on_declined(BLK, 2, &[0], t0);
        assert!(t.on_timer(due, r).is_empty());
        let mut t = setup(2, 1, tile, t0);
        t.on_arrival(100, t0);
        t.on_arrival(101, t0);
        assert!(t.on_timer(due, r).is_empty());
    }

    #[test]
    fn eviction_by_count_and_by_bytes() {
        let t0 = Instant::now();
        let mut t = BlockTable::with_caps(2, usize::MAX, 4096);
        for id in 0..3u16 {
            let base = u32::from(id) * 10;
            t.on_source_sealed(id, 0, base, vec![(base, tile(base))]);
            t.on_closed(
                ClosedBlock {
                    block_id: id,
                    k: 1,
                    r: 1,
                    symbols: vec![vec![0; 8]],
                    repair_bundles: vec![base + 5],
                    last_sent: t0,
                },
                t0,
            );
        }
        let a = t.evict_over_cap();
        assert_eq!(handoffs(&a), 1);
        assert_eq!(t.closed_reason(0), Some(CloseReason::HandedOff));
        assert_eq!(t.open_blocks(), 2);
        assert_eq!(t.stats().block_evicted, 1);
        // late trigger on the evicted block: counted, inert
        assert!(t.on_arrival(0, t0).is_empty());
        assert_eq!(t.stats().block_closed_late_trigger, 1);
        // bytes: cap 12 with two 8-byte blocks -> oldest goes
        t.max_bytes = 12;
        let a = t.evict_over_cap();
        assert_eq!(handoffs(&a), 1);
        assert_eq!(t.open_blocks(), 1);
        assert_eq!(t.cached_bytes(), 8);
    }

    #[test]
    fn requests_serve_fresh_clamped_capped_rows() {
        let t0 = Instant::now();
        let rows = |a: &[Action]| -> Vec<u8> {
            a.iter()
                .map(|x| match x {
                    Action::Repair { row, .. } => *row,
                    _ => panic!(),
                })
                .collect()
        };
        let mut t = setup(10, 2, tile, t0);
        assert_eq!(rows(&t.on_request(ask(5, 2, 0), t0)), vec![2, 3]);
        // never reused, honours first_new_idx
        assert_eq!(rows(&t.on_request(ask(5, 1, 7), t0)), vec![7]);
        assert_eq!(rows(&t.on_request(ask(5, 1, 0), t0)), vec![8]);
        // cap: r + 4 = 6 extras; 4 used, 2 more then refused
        let a = t.on_request(ask(5, 5, 0), t0);
        assert_eq!(rows(&a), vec![9, 10]);
        assert_eq!(t.stats().request_over_cap, 1);
        // clamp to 255 - k
        let mut t = setup(250, 1, tile, t0);
        let a = t.on_request(ask(5, 9, 0), t0);
        assert_eq!(rows(&a), vec![1, 2, 3, 4, 5]);
        // PTO shares the cap and skips Deferred
        let mut t = setup(3, 1, tile, t0);
        assert!(t.on_pto(BLK).is_some());
        assert!(t.on_pto(99).is_none());
        // unknown block
        let mut x = ask(0, 1, 0);
        x.block_id = 42;
        assert!(t.on_request(x, t0).is_empty());
        assert_eq!(t.stats().request_unknown, 1);
    }

    #[test]
    fn request_have_ge_k_declines_and_have_lt_k_reopens_deferred() {
        let t0 = Instant::now();
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        let a = t.on_request(ask(3, 1, 0), t0);
        assert_eq!(handoffs(&a), 2);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Declined));
        assert_eq!(t.stats().request_malformed, 1);
        // Deferred, then a request with have < k: back to Repair
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        t.on_arrival(101, t0);
        t.on_arrival(200, t0);
        assert_eq!(t.blocks[&BLK].phase, Phase::Deferred);
        let a = t.on_request(ask(2, 1, 0), t0);
        assert_eq!(a.len(), 1);
        assert_eq!(t.blocks[&BLK].phase, Phase::Repair);
        assert_eq!(t.stats().have_mismatch, 1);
        // and the deadline acts again
        let r = rtt(50);
        assert_eq!(handoffs(&t.on_timer(tile_deadline(t0, r), r)), 1);
    }

    #[test]
    fn declined_while_sending_releases_and_later_sources_hand_off_at_once() {
        let t0 = Instant::now();
        let mut t = BlockTable::new();
        t.on_source_sealed(BLK, 0, 100, vec![(1, tile(0))]);
        let a = t.on_declined(BLK, 1, &[1], t0);
        assert_eq!(handoffs(&a), 1);
        let a = t.on_source_sealed(BLK, 1, 101, vec![(2, tile(1))]);
        assert_eq!(handoffs(&a), 1);
        assert_eq!(t.on_verdict(101), VerdictAction::Skip);
        assert!(t
            .on_closed(
                ClosedBlock {
                    block_id: BLK,
                    k: 2,
                    r: 1,
                    symbols: vec![],
                    repair_bundles: vec![9],
                    last_sent: t0
                },
                t0
            )
            .is_empty());
    }

    /// Randomised walk: every source of a closed block ends in exactly one
    /// outcome, a released source is never also delivered, no action repeats.
    #[test]
    fn property_every_source_has_exactly_one_outcome() {
        let mut seed = 0x9e3779b97f4a7c15u64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let t0 = Instant::now();
        let r = rtt(40);
        for round in 0..200u32 {
            let k = 2 + rnd(6) as u8;
            let nr = 1 + rnd(3) as u8;
            let h264 = rnd(2) == 0;
            let id = round as BlockId;
            let mut t = BlockTable::new();
            let base = 10_000;
            for i in 0..u32::from(k) {
                let c = if h264 { frag(i) } else { tile(i) };
                t.on_source_sealed(id, i as u8, base + i, vec![(i, c)]);
            }
            let _ = t.on_closed(
                ClosedBlock {
                    block_id: id,
                    k,
                    r: nr,
                    symbols: vec![vec![0; 4]; usize::from(k)],
                    repair_bundles: (0..u32::from(nr)).map(|j| 20_000 + j).collect(),
                    last_sent: t0,
                },
                t0,
            );
            let mut released = std::collections::HashSet::new();
            let mut now = t0;
            for _ in 0..40 {
                now += ms(rnd(60));
                let acts = match rnd(6) {
                    0 => t.on_arrival(base + rnd(u64::from(k)) as u32, now),
                    1 => t.on_arrival(20_000 + rnd(u64::from(nr)) as u32, now),
                    2 => t.on_recovered(base + rnd(u64::from(k)) as u32, now),
                    3 => t.on_declined(id, k, &[rnd(256) as u8], now),
                    4 => t.on_request(
                        RepairAsk {
                            block_id: id,
                            have: rnd(u64::from(k) + 1) as u8,
                            missing: 2,
                            first_new_idx: 0,
                        },
                        now,
                    ),
                    _ => t.on_timer(now, r),
                };
                for a in acts {
                    let (ws_list, _) = match &a {
                        Action::HandOffTile { records, .. } => {
                            (records.iter().map(|x| x.0).collect::<Vec<_>>(), ())
                        }
                        Action::ContentLostH264 { frag_idx, .. } => {
                            (vec![u32::from(*frag_idx)], ())
                        }
                        _ => continue,
                    };
                    for w in ws_list {
                        assert!(released.insert(w), "source {w} released twice");
                        let o = t.outcome(base + w).unwrap();
                        assert!(
                            matches!(o, Outcome::HandedOff | Outcome::Lost),
                            "released source {w} also {o:?}"
                        );
                    }
                }
            }
            // drive to the end
            t.on_timer(now + T_DEF + Duration::from_secs(1), r);
            t.on_declined(id, k, &[0], now);
            for i in 0..u32::from(k) {
                let o = t.outcome(base + i).expect("every source has an outcome");
                let rel = released.contains(&i);
                assert_eq!(
                    rel,
                    matches!(o, Outcome::HandedOff | Outcome::Lost),
                    "source {i}: {o:?}, released={rel}"
                );
            }
            assert!(!t.is_open(id));
        }
    }
}
