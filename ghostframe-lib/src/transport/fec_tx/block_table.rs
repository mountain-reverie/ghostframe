// SPDX-License-Identifier: AGPL-3.0-or-later
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
//! verdict finds them and answers [`VerdictAction::Skip`]. An entry is kept
//! until the ledger resolves its bundle (an ACK, or the verdict itself, via
//! [`BlockTable::on_ledger_resolved`]), at most [`TOMBSTONE_RETENTION`], and
//! never more than the hard cap: a count of bundles says nothing about time.
//!
//! Every source ends in exactly one [`Outcome`] (acked, recovered, handed off
//! to ARQ, lost to the manager); outcomes are terminal.
//!
//! Interface notes for D2/D4b: this module defines its own minimal input type
//! ([`ClosedBlock`]); it uses no packer types. Source records are
//! `(wire_seq, Content)` pairs.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::time::{Duration, Instant};

use ghostframe_protocol::fec::feedback::RepairAsk;
use ghostframe_protocol::fec::timing;

use crate::transport::reliable_emitter::Content;

/// Blocks tracked at once (the client's own window).
pub(crate) const FEC_TX_MAX_BLOCKS: usize = 1024;
/// Bytes of cached source symbols.
pub(crate) const FEC_TX_MAX_CACHED_BYTES: usize = 16 << 20;
/// Hard cap on retained `by_bundle` entries of closed blocks. Memory only: a
/// 100 Mbit/s stream seals ~11k bundles a second, so this is ~25 s of them. The
/// real retention is [`TOMBSTONE_RETENTION`] and the ledger's resolution.
pub(crate) const FEC_TX_TOMBSTONES: usize = 1 << 18;
/// Resolved entries (the ledger is done with them) are kept this many, only
/// for the late-trigger counters; an unresolved one is never dropped for count.
pub(crate) const RESOLVED_TOMBSTONES: usize = 4096;
/// How long a closed block's bundle entries are kept for a late verdict if the
/// ledger never resolves the bundle. Past the longest verdict lag seen (2.6 s)
/// and the ledger's own retention.
pub(crate) const TOMBSTONE_RETENTION: Duration = Duration::from_secs(10);
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
    /// The estimator has a sample. Without one no hint was sent, so the client
    /// uses its unadvertised request round and so must the server.
    pub has_sample: bool,
    /// The largest RTT hint code the server sent in the last 2 s (a client
    /// keeps the last hint it saw, which can lag the fresh srtt). Read only
    /// with `has_sample`.
    pub hint_code: u8,
}

impl Default for Rtt {
    fn default() -> Self {
        Rtt {
            srtt: Duration::from_millis(50),
            rttvar: Duration::from_millis(10),
            max_ack_delay: Duration::ZERO,
            has_sample: false,
            hint_code: 0,
        }
    }
}

fn us(v: u64) -> Duration {
    Duration::from_micros(v)
}

impl Rtt {
    /// How long the client waits for a requested row: its request round for
    /// the hint code the server sent, and never less than the round of a client
    /// that has seen no hint at all (43 ms, longer than a hint of up to 11 ms
    /// gives): the first blocks after a session starts can reach a client that
    /// has not yet received a REPAIR, whatever the server has queued.
    fn round(&self) -> Duration {
        let hint = if self.has_sample { self.hint_code } else { 0 };
        us(timing::request_round_us(hint).max(timing::request_round_us(0)))
    }
}

/// The span before the client asks: `T_adv` plus the allowance, from the
/// block's first arrival.
fn first_window(t_adv: u8) -> Duration {
    us(timing::first_window_us(t_adv))
}

/// `last symbol sent + a + R + srtt + max_ack_delay`: the client's first
/// window, then its request round (`R`), then the path back.
pub(crate) fn tile_deadline(last_sent: Instant, r: Rtt, t_adv: u8) -> Instant {
    last_sent + first_window(t_adv) + r.round() + r.srtt + r.max_ack_delay
}
/// `last symbol sent + T_adv + (R - A) + srtt + max_ack_delay + RAF_ALLOWANCE`:
/// three milliseconds before the tile deadline for every code and hint (`2 A - RAF`).
pub(crate) fn h264_deadline(last_sent: Instant, r: Rtt, t_adv: u8) -> Instant {
    last_sent
        + us(timing::t_adv_us(t_adv))
        + r.round().saturating_sub(us(timing::allowance_us()))
        + r.srtt
        + r.max_ack_delay
        + RAF_ALLOWANCE
}
/// `last symbol sent + a + srtt + 2 rttvar`.
fn t_lr_at(last_sent: Instant, r: Rtt, t_adv: u8) -> Instant {
    last_sent + first_window(t_adv) + r.srtt + 2 * r.rttvar
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
    /// The client declined a block whose REPAIR_REQUEST this table served
    /// (at least one row went out). The decline then measures the request
    /// round, not the proactive `r`, so [`DeclinedTrim`] does not count it.
    /// Known cost: a genuine `r` shortfall after a served request lands here
    /// too, which is why `closed_declined_late` is reported beside
    /// `closed_declined`.
    DeclinedLate,
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

/// Why the table tells the manager a fragment is lost (`h264 stats`:
/// `content_lost_decline` / `content_lost_deadline`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LostCause {
    /// The client said DECLINED (or the block was already given up on).
    Decline,
    /// A deadline, the deferral alarm, an eviction or an all-repairs-dropped hand-off.
    Deadline,
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
        /// The transmission that carried it (an IDR fragment is sent again
        /// from here, as a `Handoff`).
        wire_seq: u32,
        cause: LostCause,
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
    /// Repair rows queued whose `bundle_seq` is allocated when each is sent
    /// (`register_repair`). With no `repair_bundles` and none queued the block
    /// has nothing to wait for.
    pub repairs_queued: usize,
    /// When the last symbol of the block was sent.
    pub last_sent: Instant,
    /// The `T_adv` code the block's sources carried (the client's window follows it).
    pub t_adv: u8,
}

/// Counters; the names are the plan's.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct BlockStats {
    pub block_evicted: u64,
    /// The most symbol bytes the table held at once (read after a block
    /// closes, before the cap evicts), against `FEC_TX_MAX_CACHED_BYTES`.
    pub cached_bytes_peak: u64,
    pub block_closed_late_trigger: u64,
    pub request_unknown: u64,
    pub request_before_close: u64,
    pub request_over_cap: u64,
    pub request_malformed: u64,
    /// Requests with have == k (all sources held, k unknown to the client).
    pub request_holds_all: u64,
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
    /// Blocks closed, by outcome (every `finish`): `Accounted` is the good
    /// ending, the other three are what the E11 sweep reports as shares.
    pub closed_accounted: u64,
    /// Includes `closed_declined_late`.
    pub closed_declined: u64,
    /// The subset of `closed_declined` whose request had been served.
    pub closed_declined_late: u64,
    pub closed_handed_off: u64,
    pub closed_timed_out: u64,
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

/// The table's timers. Each has a key independent of the live RTT (an
/// instant plus a constant) and an RTT-dependent slack added when read, so a
/// per-kind min-heap stays ordered whatever the RTT does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TimerKind {
    TLr,
    TileRelease,
    H264Release,
    H264Alarm,
    TDef,
}

const TIMER_KINDS: [TimerKind; 5] = [
    TimerKind::TLr,
    TimerKind::TileRelease,
    TimerKind::H264Release,
    TimerKind::H264Alarm,
    TimerKind::TDef,
];

impl TimerKind {
    fn idx(self) -> usize {
        self as usize
    }
    /// The constant part of the deadline, added to `last_sent` (or, for the
    /// alarms, `deferred_at`). It is the block's own: the client's window
    /// follows the span the block advertised.
    fn offset(self, b: &Block) -> Duration {
        match self {
            TimerKind::TLr | TimerKind::TileRelease => first_window(b.t_adv),
            TimerKind::H264Release => us(timing::t_adv_us(b.t_adv)) + RAF_ALLOWANCE,
            TimerKind::H264Alarm => H264_DEFERRAL_SLACK,
            TimerKind::TDef => T_DEF,
        }
    }
    /// The part that follows the path, the same for every block.
    fn slack(self, r: Rtt) -> Duration {
        match self {
            TimerKind::TLr => r.srtt + 2 * r.rttvar,
            TimerKind::TileRelease => r.round() + r.srtt + r.max_ack_delay,
            TimerKind::H264Release => {
                r.round().saturating_sub(us(timing::allowance_us())) + r.srtt + r.max_ack_delay
            }
            TimerKind::H264Alarm => 2 * r.srtt,
            TimerKind::TDef => Duration::ZERO,
        }
    }
    fn release(kind: Kind) -> TimerKind {
        match kind {
            Kind::Tile => TimerKind::TileRelease,
            Kind::H264 => TimerKind::H264Release,
        }
    }
    fn alarm(kind: Kind) -> TimerKind {
        match kind {
            Kind::Tile => TimerKind::TDef,
            Kind::H264 => TimerKind::H264Alarm,
        }
    }
}

/// `(key, block, born)`: `born` tells a reused `BlockId` from the block the
/// timer was armed for.
type TimerEntry = Reverse<(Instant, BlockId, u64)>;

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
    /// The `T_adv` code of the block's sources; set when the block closes.
    t_adv: u8,
    /// A REPAIR_REQUEST for this block produced at least one repair action.
    /// Survives `Deferred` -> `Repair`.
    request_served: bool,
    sources: Vec<Src>,
    symbols: Vec<Vec<u8>>,
    sym_bytes: usize,
    /// Distinct ACKed bundles among sources and repairs.
    held: usize,
    accounted: usize,
    released: usize,
    next_row: u16,
    extra_used: u16,
    /// `bundle_seq` of each repair registered, so closing is O(repairs).
    repairs: Vec<u32>,
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
    /// The ledger is done with this bundle (ACK or verdict), so no later
    /// verdict can ask about it and the tombstone may go.
    resolved: bool,
}

/// Close reasons kept for the controller between two reads.
const CLOSE_LOG_CAP: usize = 1024;

/// The largest RTT hint code sent in a recent window: two buckets of one
/// second, so a sample is kept at least 1 s and at most 2 s. Releases are
/// computed from it, not from the fresh srtt, because the client keeps the
/// last hint it saw, which can be seconds behind a rising round trip.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HintTracker {
    start: Option<Instant>,
    cur: u8,
    prev: u8,
}

impl HintTracker {
    const BUCKET: Duration = Duration::from_secs(1);

    fn roll(&mut self, now: Instant) {
        let Some(start) = self.start else {
            self.start = Some(now);
            return;
        };
        let n = now.saturating_duration_since(start).as_nanos() / Self::BUCKET.as_nanos();
        match n {
            0 => {}
            1 => {
                self.prev = self.cur;
                self.cur = 0;
                self.start = Some(start + Self::BUCKET);
            }
            _ => {
                self.prev = 0;
                self.cur = 0;
                self.start = Some(now);
            }
        }
    }

    pub(crate) fn note(&mut self, code: u8, now: Instant) {
        self.roll(now);
        self.cur = self.cur.max(code);
    }

    pub(crate) fn max(&mut self, now: Instant) -> u8 {
        self.roll(now);
        self.cur.max(self.prev)
    }
}

#[derive(Debug)]
pub(crate) struct BlockTable {
    hints: HintTracker,
    blocks: HashMap<BlockId, Block>,
    by_bundle: HashMap<u32, Entry>,
    /// Newest source carrying each tile pass; stale values are checked
    /// against `by_bundle` on lookup (key space is bounded by 256^2 x passes).
    by_tile: HashMap<(u8, u8, u8), u32>,
    /// Closed blocks' bundles, oldest first, with when they closed.
    tombstones: VecDeque<(u32, Instant)>,
    /// The newest `now` any input carried (the table's own clock).
    now: Instant,
    timers: [BinaryHeap<TimerEntry>; 5],
    closed: HashMap<BlockId, CloseReason>,
    /// Close reasons not yet read by the adaptive controller (bounded).
    close_log: Vec<CloseReason>,
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

/// The packer's `SourceKind` rule: a bundle with any H.264 fragment is an
/// H.264 source (its deadline and alarm are the shorter H.264 ones).
fn kind_of(records: &[SourceRecord]) -> Kind {
    if records
        .iter()
        .any(|(_, c)| matches!(c, Content::H264Frag { .. }))
    {
        Kind::H264
    } else {
        Kind::Tile
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
            by_tile: HashMap::new(),
            tombstones: VecDeque::new(),
            now: Instant::now(),
            timers: Default::default(),
            closed: HashMap::new(),
            close_log: Vec::new(),
            closed_order: VecDeque::new(),
            cached_bytes: 0,
            next_born: 0,
            rtt: Rtt::default(),
            hints: HintTracker {
                start: None,
                cur: 0,
                prev: 0,
            },
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
    #[cfg(test)]
    pub(crate) fn cached_bytes(&self) -> usize {
        self.cached_bytes
    }
    /// A REPAIR left carrying RTT hint `code`.
    pub(crate) fn note_hint_sent(&mut self, code: u8, now: Instant) {
        self.hints.note(code, now);
    }
    /// The largest hint code sent in the last 1 to 2 s (0 when none).
    pub(crate) fn recent_hint(&mut self, now: Instant) -> u8 {
        self.hints.max(now)
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
    #[cfg(test)]
    pub(crate) fn outcome(&self, bundle_seq: u32) -> Option<Outcome> {
        self.by_bundle.get(&bundle_seq).and_then(|e| e.outcome)
    }
    /// `(k, r)` of a closed-for-sources block still held (rows are encoded
    /// with these in the header).
    pub(crate) fn params(&self, block_id: BlockId) -> Option<(u8, u8)> {
        self.blocks
            .get(&block_id)
            .filter(|b| b.phase != Phase::Sending)
            .map(|b| (b.k, b.r))
    }

    /// An extra repair row (request, PTO, `T_lr`) was sealed as `bundle_seq`,
    /// so its ACK counts towards held(B). A block no longer open ignores it.
    pub(crate) fn register_repair(&mut self, block_id: BlockId, row: u8, bundle_seq: u32) {
        let Some(b) = self.blocks.get_mut(&block_id) else {
            return;
        };
        b.repairs.push(bundle_seq);
        self.by_bundle.insert(
            bundle_seq,
            Entry {
                block_id,
                role: Role::Repair { row },
                outcome: None,
                resolved: false,
            },
        );
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
            let outcome =
                release_records(block_id, kind, &records, LostCause::Decline, &mut actions);
            self.by_bundle.insert(
                bundle_seq,
                Entry {
                    block_id,
                    role: Role::Source { idx },
                    outcome: Some(outcome),
                    resolved: false,
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
            t_adv: 0,
            request_served: false,
            sources: Vec::new(),
            symbols: Vec::new(),
            sym_bytes: 0,
            held: 0,
            accounted: 0,
            released: 0,
            next_row: 0,
            extra_used: 0,
            repairs: Vec::new(),
            last_sent: None,
            deferred_at: None,
            t_lr_armed: false,
            timed_out: false,
        });
        if b.born == born {
            self.next_born += 1;
        }
        for (_, c) in &records {
            if let Content::Tile(k) = c {
                self.by_tile
                    .insert((k.tile_x, k.tile_y, k.pass_idx), bundle_seq);
            }
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
                resolved: false,
            },
        );
        actions
    }

    /// The packer closed the block (sources already registered).
    pub(crate) fn on_closed(&mut self, c: ClosedBlock, now: Instant) -> Vec<Action> {
        self.tick(now);
        let mut actions = Vec::new();
        if self.closed.contains_key(&c.block_id) {
            return actions;
        }
        let Some(mut b) = self.blocks.remove(&c.block_id) else {
            return actions;
        };
        b.k = c.k;
        b.r = c.r;
        b.t_adv = c.t_adv;
        b.next_row = u16::from(c.r);
        b.sym_bytes = c.symbols.iter().map(Vec::len).sum();
        self.cached_bytes += b.sym_bytes;
        self.stats.cached_bytes_peak = self.stats.cached_bytes_peak.max(self.cached_bytes as u64);
        b.symbols = c.symbols;
        b.last_sent = Some(c.last_sent);
        b.phase = Phase::Repair;
        b.t_lr_armed = true;
        let none_sent = c.repair_bundles.is_empty() && c.repairs_queued == 0;
        b.repairs = c.repair_bundles.clone();
        for (row, bs) in c.repair_bundles.iter().enumerate() {
            self.by_bundle.insert(
                *bs,
                Entry {
                    block_id: c.block_id,
                    role: Role::Repair { row: row as u8 },
                    outcome: None,
                    resolved: false,
                },
            );
        }
        if none_sent {
            // Nothing to wait for: hand off at once.
            self.stats.blocks_without_repairs += 1;
            self.release_unresolved(c.block_id, &mut b, LostCause::Deadline, &mut actions);
        }
        self.settle(c.block_id, b, now, &mut actions);
        self.arm(c.block_id);
        actions
    }

    fn tick(&mut self, now: Instant) {
        self.now = self.now.max(now);
    }

    // ---- inputs -------------------------------------------------------

    /// An ACK for `bundle_seq` arrived.
    pub(crate) fn on_arrival(&mut self, bundle_seq: u32, now: Instant) -> Vec<Action> {
        self.tick(now);
        let mut actions = Vec::new();
        let Some(e) = self.by_bundle.get_mut(&bundle_seq) else {
            return actions;
        };
        e.resolved = true;
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
        self.tick(now);
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
        self.tick(now);
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
        self.release_unresolved(id, &mut b, LostCause::Decline, actions);
        let why = if b.request_served {
            CloseReason::DeclinedLate
        } else {
            CloseReason::Declined
        };
        self.finish(id, b, why);
    }

    /// A REPAIR_REQUEST entry.
    pub(crate) fn on_request(&mut self, ask: RepairAsk, now: Instant) -> Vec<Action> {
        self.tick(now);
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
        if ask.have > b.k {
            // Malformed: a client cannot hold more than k symbols.
            self.stats.request_malformed += 1;
            self.decline(id, b, now, &mut actions);
            return actions;
        }
        // have == k is benign: a time-closed block carries no LAST, so a
        // client whose repairs were all lost holds every source without
        // knowing k and asks for one row. Answer with one row (a REPAIR frame
        // carries k, so the block completes there); this is not a decline.
        let holds_all = ask.have == b.k;
        if holds_all {
            self.stats.request_holds_all += 1;
        }
        b.t_lr_armed = false;
        let back_to_repair = b.phase == Phase::Deferred;
        if back_to_repair {
            self.stats.have_mismatch += 1;
            b.phase = Phase::Repair;
            b.deferred_at = None;
        }
        let max_row = 255 - u16::from(b.k);
        let mut row = u16::from(ask.first_new_idx).max(b.next_row);
        let cap = u16::from(b.r) + EXTRA_ROWS_BEYOND_R;
        // The server knows k; a client that has not seen LAST or a repair
        // does not, and always asks for one row. Serve what the block needs
        // to reach k from what the client holds.
        let needed =
            b.k.saturating_sub(ask.have)
                .max(ask.missing)
                .max(u8::from(holds_all));
        for _ in 0..needed {
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
            b.request_served = true;
            row += 1;
            b.next_row = row;
        }
        self.blocks.insert(id, b);
        if back_to_repair {
            self.arm(id);
        }
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

    /// The ledger is done with `bundle_seq`: its verdict was handled (an ACK
    /// is noted by `on_arrival`). Its tombstone may now be dropped.
    pub(crate) fn on_ledger_resolved(&mut self, bundle_seq: u32) {
        if let Some(e) = self.by_bundle.get_mut(&bundle_seq) {
            e.resolved = true;
        }
    }

    /// The block whose content outcome a source still owes (`on_verdict` is
    /// `Park` or `Skip`): the loss path retransmits nothing for it. `None` for
    /// a PLAIN bundle, a repair (it carries no records) or an unknown bundle.
    pub(crate) fn protected_block(&self, bundle_seq: u32) -> Option<BlockId> {
        match self.on_verdict(bundle_seq) {
            VerdictAction::Pass => None,
            VerdictAction::Park | VerdictAction::Skip => {
                self.by_bundle.get(&bundle_seq).map(|e| e.block_id)
            }
        }
    }

    /// Whether the tile pass `(x, y, pass)` has a source still in flight in an
    /// unaccounted block (a TILE_NACK for it is then likely redundant).
    pub(crate) fn tile_protected(&self, x: u8, y: u8, pass: u8) -> bool {
        self.by_tile
            .get(&(x, y, pass))
            .is_some_and(|&b| self.on_verdict(b) == VerdictAction::Park)
    }

    /// Service timers: `T_lr`, deadlines, deferral alarms. Touches only the
    /// blocks with a due timer.
    pub(crate) fn on_timer(&mut self, now: Instant, rtt: Rtt) -> Vec<Action> {
        self.tick(now);
        self.rtt = rtt;
        let mut actions = Vec::new();
        let mut due_ids: Vec<(u64, BlockId)> = Vec::new();
        for kind in TIMER_KINDS {
            let slack = kind.slack(rtt);
            while let Some(&Reverse((key, id, born))) = self.timers[kind.idx()].peek() {
                if key + slack > now {
                    break;
                }
                self.timers[kind.idx()].pop();
                if self.timer_valid(kind, key, id, born) {
                    due_ids.push((born, id));
                }
            }
        }
        due_ids.sort_unstable();
        due_ids.dedup();
        for (_, id) in due_ids {
            let Some(mut b) = self.blocks.remove(&id) else {
                continue;
            };
            match b.phase {
                Phase::Sending => {}
                Phase::Repair => {
                    let last = b.last_sent.unwrap_or(now);
                    if b.t_lr_armed && now >= t_lr_at(last, rtt, b.t_adv) {
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
                                Kind::Tile => tile_deadline(last, rtt, b.t_adv),
                                Kind::H264 => h264_deadline(last, rtt, b.t_adv),
                            };
                            now >= d
                        })
                        .collect();
                    self.release_idxs(id, &mut b, &due, LostCause::Deadline, &mut actions);
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
                    self.release_idxs(id, &mut b, &due, LostCause::Deadline, &mut actions);
                }
            }
            self.settle(id, b, now, &mut actions);
        }
        self.prune_tombstones();
        actions
    }

    /// The earliest instant `on_timer` has work, from the last known RTT.
    /// O(log n) amortised: a min-heap per timer kind, stale entries dropped
    /// as they reach the top.
    pub(crate) fn next_deadline(&mut self) -> Option<Instant> {
        let rtt = self.rtt;
        let mut best: Option<Instant> = None;
        for kind in TIMER_KINDS {
            while let Some(&Reverse((key, id, born))) = self.timers[kind.idx()].peek() {
                if !self.timer_valid(kind, key, id, born) {
                    self.timers[kind.idx()].pop();
                    continue;
                }
                let t = key + kind.slack(rtt);
                best = Some(best.map_or(t, |c| c.min(t)));
                break;
            }
        }
        best
    }

    /// Whether the timer `kind` (key `key`) armed for block `id` (born `born`) still has
    /// work: the block is there, in the phase, with a source of that kind open.
    fn timer_valid(&self, kind: TimerKind, key: Instant, id: BlockId, born: u64) -> bool {
        let Some(b) = self.blocks.get(&id).filter(|b| b.born == born) else {
            return false;
        };
        // The key must be the one the block's own times give now (a block that
        // went Deferred, back to Repair and Deferred again has a newer
        // `deferred_at`; the older entry is stale).
        let base = match kind {
            TimerKind::TLr | TimerKind::TileRelease | TimerKind::H264Release => b.last_sent,
            TimerKind::H264Alarm | TimerKind::TDef => b.deferred_at,
        };
        if base.map(|t| t + kind.offset(b)) != Some(key) {
            return false;
        }
        let open_of = |k: Kind| {
            b.sources
                .iter()
                .enumerate()
                .any(|(i, s)| s.kind == k && self.src_open(b, i))
        };
        match kind {
            TimerKind::TLr => b.phase == Phase::Repair && b.t_lr_armed,
            TimerKind::TileRelease => b.phase == Phase::Repair && open_of(Kind::Tile),
            TimerKind::H264Release => b.phase == Phase::Repair && open_of(Kind::H264),
            TimerKind::H264Alarm => b.phase == Phase::Deferred && open_of(Kind::H264),
            TimerKind::TDef => b.phase == Phase::Deferred && open_of(Kind::Tile),
        }
    }

    /// Arms the timers of block `id` for the phase it is in. Called on entering
    /// `Repair` and `Deferred` only, so the heaps do not grow with ACKs.
    fn arm(&mut self, id: BlockId) {
        let Some(b) = self.blocks.get(&id) else {
            return;
        };
        let mut push: Vec<(TimerKind, Instant)> = Vec::new();
        let kinds = |k: Kind| b.sources.iter().any(|s| s.kind == k);
        match b.phase {
            Phase::Sending => {}
            Phase::Repair => {
                let Some(last) = b.last_sent else { return };
                if b.t_lr_armed {
                    push.push((TimerKind::TLr, last + TimerKind::TLr.offset(b)));
                }
                for k in [Kind::Tile, Kind::H264] {
                    if kinds(k) {
                        let t = TimerKind::release(k);
                        push.push((t, last + t.offset(b)));
                    }
                }
            }
            Phase::Deferred => {
                let Some(at) = b.deferred_at else { return };
                for k in [Kind::Tile, Kind::H264] {
                    if kinds(k) {
                        let t = TimerKind::alarm(k);
                        push.push((t, at + t.offset(b)));
                    }
                }
            }
        }
        let born = b.born;
        for (t, key) in push {
            self.timers[t.idx()].push(Reverse((key, id, born)));
        }
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
            self.release_unresolved(id, &mut b, LostCause::Deadline, &mut actions);
            self.finish(id, b, CloseReason::HandedOff);
        }
        actions
    }

    /// Session reset: forget every block. Counters survive.
    pub(crate) fn reset(&mut self) {
        self.blocks.clear();
        self.by_bundle.clear();
        self.by_tile.clear();
        self.tombstones.clear();
        self.timers = Default::default();
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

    fn release_unresolved(
        &mut self,
        id: BlockId,
        b: &mut Block,
        cause: LostCause,
        actions: &mut Vec<Action>,
    ) {
        let all: Vec<usize> = (0..b.sources.len())
            .filter(|&i| self.src_open(b, i))
            .collect();
        self.release_idxs(id, b, &all, cause, actions);
    }

    fn release_idxs(
        &mut self,
        id: BlockId,
        b: &mut Block,
        idxs: &[usize],
        cause: LostCause,
        actions: &mut Vec<Action>,
    ) {
        for &i in idxs {
            let s = &b.sources[i];
            let o = release_records(id, s.kind, &s.records, cause, actions);
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
                self.blocks.insert(id, b);
                self.arm(id);
                return;
            }
            self.blocks.insert(id, b);
        }
    }

    /// Takes the close reasons since the last call, oldest first. Capped: an
    /// unread log stops growing rather than growing without bound.
    pub(crate) fn take_close_log(&mut self) -> Vec<CloseReason> {
        std::mem::take(&mut self.close_log)
    }

    fn finish(&mut self, id: BlockId, b: Block, why: CloseReason) {
        self.cached_bytes -= b.sym_bytes;
        for s in &b.sources {
            self.tombstone(s.bundle_seq);
        }
        for &r in &b.repairs {
            self.tombstone(r);
        }
        match why {
            CloseReason::Accounted => self.stats.closed_accounted += 1,
            CloseReason::Declined => self.stats.closed_declined += 1,
            CloseReason::DeclinedLate => {
                self.stats.closed_declined += 1;
                self.stats.closed_declined_late += 1;
            }
            CloseReason::HandedOff => self.stats.closed_handed_off += 1,
            CloseReason::TimedOut => self.stats.closed_timed_out += 1,
        }
        self.closed.insert(id, why);
        if self.close_log.len() < CLOSE_LOG_CAP {
            self.close_log.push(why);
        }
        self.closed_order.push_back(id);
        while self.closed_order.len() > self.max_blocks.max(1) {
            if let Some(old) = self.closed_order.pop_front() {
                self.closed.remove(&old);
            }
        }
    }

    fn tombstone(&mut self, bundle_seq: u32) {
        self.tombstones.push_back((bundle_seq, self.now));
        self.prune_tombstones();
    }

    /// Drops tombstones from the oldest end: resolved ones past
    /// [`RESOLVED_TOMBSTONES`], any that outlived [`TOMBSTONE_RETENTION`]; then the hard cap, whatever
    /// they are. An unresolved young entry at the front holds the ones behind it
    /// (bounded by the retention and the cap).
    fn prune_tombstones(&mut self) {
        while let Some(&(seq, at)) = self.tombstones.front() {
            let over_soft = self.tombstones.len() > RESOLVED_TOMBSTONES;
            let gone = match self.by_bundle.get(&seq) {
                None => true,
                Some(e) => {
                    (e.resolved && over_soft)
                        || self.now.saturating_duration_since(at) >= TOMBSTONE_RETENTION
                }
            };
            if !gone && self.tombstones.len() <= self.tomb_cap {
                break;
            }
            self.tombstones.pop_front();
            self.by_bundle.remove(&seq);
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
    cause: LostCause,
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
                wire_seq: ws,
                cause,
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
            has_sample: false,
            hint_code: 0,
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
                repairs_queued: 0,
                last_sent: t0,
                t_adv: 0,
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
        // Counted by outcome: this one and none of the others.
        let s = t.stats();
        assert_eq!(
            (
                s.closed_accounted,
                s.closed_declined,
                s.closed_handed_off,
                s.closed_timed_out
            ),
            (1, 0, 0, 0)
        );
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
        // Counted by outcome: this one and none of the others.
        let s = t.stats();
        assert_eq!(
            (
                s.closed_accounted,
                s.closed_declined,
                s.closed_handed_off,
                s.closed_timed_out
            ),
            (0, 1, 0, 0)
        );
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
        let d = tile_deadline(t0, r, 0);
        assert!(t.next_deadline().is_some());
        assert_eq!(handoffs(&t.on_timer(d - ms(1), r)), 0);
        let a = t.on_timer(d, r);
        assert_eq!(handoffs(&a), 2);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::HandedOff));
        // Counted by outcome: this one and none of the others.
        let s = t.stats();
        assert_eq!(
            (
                s.closed_accounted,
                s.closed_declined,
                s.closed_handed_off,
                s.closed_timed_out
            ),
            (0, 0, 1, 0)
        );
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
        let a = t.on_timer(tile_deadline(t0, r, 0) + ms(1000), r);
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
            now > h264_deadline(t0, r, 0),
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
        // Counted by outcome: this one and none of the others.
        let s = t.stats();
        assert_eq!(
            (
                s.closed_accounted,
                s.closed_declined,
                s.closed_handed_off,
                s.closed_timed_out
            ),
            (0, 0, 0, 1)
        );
        assert_eq!(t.stats().deferral_alarm_h264, 1);
    }

    #[test]
    fn h264_deadline_in_repair_reports_lost() {
        let t0 = Instant::now();
        let r = rtt(50);
        let mut t = setup(2, 1, frag, t0);
        let a = t.on_timer(h264_deadline(t0, r, 0), r);
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
                repairs_queued: 0,
                last_sent: t0,
                t_adv: 0,
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
        let due = t_lr_at(t0, r, 0);
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
                    repairs_queued: 0,
                    last_sent: t0,
                    t_adv: 0,
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
        assert_eq!(rows(&t.on_request(ask(8, 2, 0), t0)), vec![2, 3]);
        // never reused, honours first_new_idx
        assert_eq!(rows(&t.on_request(ask(9, 1, 7), t0)), vec![7]);
        assert_eq!(rows(&t.on_request(ask(9, 1, 0), t0)), vec![8]);
        // cap: r + 4 = 6 extras; 4 used, 2 more then refused
        let a = t.on_request(ask(5, 5, 0), t0);
        assert_eq!(rows(&a), vec![9, 10]);
        assert_eq!(t.stats().request_over_cap, 1);
        // a client that asks for one row but is k - have short is served k - have
        let mut t = setup(10, 2, tile, t0);
        assert_eq!(rows(&t.on_request(ask(7, 1, 0), t0)), vec![2, 3, 4]);
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
        let a = t.on_request(ask(4, 1, 0), t0);
        assert_eq!(handoffs(&a), 2);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Declined));
        assert_eq!(t.stats().request_malformed, 1);
        assert_eq!(t.stats().request_holds_all, 0);
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
        assert_eq!(handoffs(&t.on_timer(tile_deadline(t0, r, 0), r)), 1);
    }

    /// A time-closed block whose repairs were all lost: the client holds all
    /// k sources, does not know k, asks with have == k. Not a decline, not
    /// malformed; one row goes out so the client learns k.
    #[test]
    fn request_have_eq_k_is_benign_not_a_decline() {
        let t0 = Instant::now();
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        let a = t.on_request(ask(3, 1, 0), t0);
        assert_eq!(repair_rows(&a), 1);
        assert_eq!(t.closed_reason(BLK), None);
        assert!(t.blocks.contains_key(&BLK));
        assert_eq!(t.stats().request_malformed, 0);
        assert_eq!(t.stats().request_holds_all, 1);
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
                    repairs_queued: 0,
                    last_sent: t0,
                    t_adv: 0
                },
                t0
            )
            .is_empty());
    }

    fn repair_rows(a: &[Action]) -> usize {
        a.iter()
            .filter(|x| matches!(x, Action::Repair { .. }))
            .count()
    }

    /// A decline after a served request closes `DeclinedLate`; one with no
    /// request, a request refused over the cap, or a malformed request
    /// closes `Declined`. `closed_declined` counts both, the late subset
    /// separately.
    #[test]
    fn a_decline_after_a_served_request_closes_declined_late() {
        let t0 = Instant::now();
        // Served: one request produced rows, then the client gave up.
        let mut t = setup(5, 1, tile, t0);
        assert!(repair_rows(&t.on_request(ask(3, 2, 0), t0)) > 0);
        t.on_declined(BLK, 5, &[0b11111], t0);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::DeclinedLate));
        let s = t.stats();
        assert_eq!((s.closed_declined, s.closed_declined_late), (1, 1));

        // Never requested.
        let mut t = setup(5, 1, tile, t0);
        t.on_declined(BLK, 5, &[0b11111], t0);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Declined));
        let s = t.stats();
        assert_eq!((s.closed_declined, s.closed_declined_late), (1, 0));

        // Refused over the cap: PTO rows used the cap up (r + 4 = 5 here),
        // so the request produced no row; the block was never served.
        let mut t = setup(5, 1, tile, t0);
        for _ in 0..5 {
            assert!(t.on_pto(BLK).is_some());
        }
        assert_eq!(repair_rows(&t.on_request(ask(3, 2, 0), t0)), 0);
        assert_eq!(t.stats().request_over_cap, 1, "premise: the cap refused it");
        t.on_declined(BLK, 5, &[0b11111], t0);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Declined));

        // Malformed (have > k): declines at once, was not served.
        let mut t = setup(3, 1, tile, t0);
        t.on_request(ask(4, 1, 0), t0);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::Declined));
        assert_eq!(t.stats().closed_declined_late, 0);
    }

    /// A block requested while `Deferred` that went back to `Repair` is still
    /// served when the client then declines it.
    #[test]
    fn a_request_from_deferred_back_to_repair_is_still_served() {
        let t0 = Instant::now();
        let mut t = setup(3, 1, tile, t0);
        t.on_arrival(100, t0);
        t.on_arrival(101, t0);
        t.on_arrival(200, t0); // held 3 of k = 3: Deferred
        assert_eq!(t.blocks[&BLK].phase, Phase::Deferred, "premise");
        assert!(repair_rows(&t.on_request(ask(1, 2, 0), t0)) > 0);
        assert_eq!(t.blocks[&BLK].phase, Phase::Repair, "premise: back");
        t.on_declined(BLK, 3, &[0b100], t0);
        assert_eq!(t.closed_reason(BLK), Some(CloseReason::DeclinedLate));
    }

    /// Over random interleavings of requests, PTO rows, ACKs and a decline, the
    /// close reason is `DeclinedLate` exactly when some request returned a row,
    /// whatever the timing.
    #[test]
    fn property_declined_late_follows_a_served_request_not_timing() {
        let mut seed = 0x2545f4914f6cdd1du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let t0 = Instant::now();
        let (mut late, mut plain) = (0, 0);
        for round in 0..300u32 {
            let k = 2 + rnd(6) as u8;
            let nr = 1 + rnd(3) as u8;
            let id = round as BlockId;
            let mut t = BlockTable::new();
            for i in 0..u32::from(k) {
                t.on_source_sealed(id, i as u8, 10_000 + i, vec![(i, tile(i))]);
            }
            t.on_closed(
                ClosedBlock {
                    block_id: id,
                    k,
                    r: nr,
                    symbols: vec![vec![0; 4]; usize::from(k)],
                    repair_bundles: (0..u32::from(nr)).map(|j| 20_000 + j).collect(),
                    repairs_queued: 0,
                    last_sent: t0,
                    t_adv: 0,
                },
                t0,
            );
            let mut served = false;
            let mut now = t0;
            for _ in 0..(1 + rnd(8)) {
                now += ms(rnd(120));
                match rnd(4) {
                    0 => {
                        let a = t.on_request(
                            RepairAsk {
                                block_id: id,
                                have: rnd(u64::from(k) + 1) as u8,
                                missing: 1 + rnd(3) as u8,
                                first_new_idx: 0,
                            },
                            now,
                        );
                        served |= repair_rows(&a) > 0;
                    }
                    1 => {
                        t.on_pto(id);
                    }
                    2 => {
                        t.on_arrival(10_000 + rnd(u64::from(k)) as u32, now);
                    }
                    _ => {
                        t.on_arrival(20_000 + rnd(u64::from(nr)) as u32, now);
                    }
                }
            }
            if t.closed_reason(id).is_some() {
                continue; // accounted, or the malformed request declined it
            }
            t.on_declined(id, k, &[0xff], now);
            let want = if served {
                CloseReason::DeclinedLate
            } else {
                CloseReason::Declined
            };
            assert_eq!(t.closed_reason(id), Some(want), "round {round}");
            if served {
                late += 1;
            } else {
                plain += 1;
            }
        }
        assert!(
            late > 20 && plain > 20,
            "premise: both cases ran {late}/{plain}"
        );
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
                    repairs_queued: 0,
                    last_sent: t0,
                    t_adv: 0,
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
            // Inside the tombstone retention, past every deadline but T_def.
            let late = now + ms(500);
            for a in t.on_timer(late, r) {
                match a {
                    Action::HandOffTile { records, .. } => {
                        for x in records {
                            assert!(released.insert(x.0), "source {} released twice", x.0);
                        }
                    }
                    Action::ContentLostH264 { wire_seq, .. } => {
                        assert!(
                            released.insert(wire_seq),
                            "source {wire_seq} released twice"
                        );
                    }
                    _ => {}
                }
            }
            for a in t.on_declined(id, k, &[0], late) {
                match a {
                    Action::HandOffTile { records, .. } => {
                        for x in records {
                            assert!(released.insert(x.0), "source {} released twice", x.0);
                        }
                    }
                    Action::ContentLostH264 { wire_seq, .. } => {
                        assert!(
                            released.insert(wire_seq),
                            "source {wire_seq} released twice"
                        );
                    }
                    _ => {}
                }
            }
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

    /// A block closed through `on_closed` with one acked H.264/tile source
    /// per block: the churn that fills the old count-based ring.
    fn churn(t: &mut BlockTable, n: u32, first_id: BlockId, base: u32, now: Instant) {
        for i in 0..n {
            let id = first_id + i as BlockId;
            let bs = base + i;
            t.on_source_sealed(id, 0, bs, vec![(bs, tile(bs))]);
            t.on_closed(
                ClosedBlock {
                    block_id: id,
                    k: 1,
                    r: 0,
                    symbols: vec![vec![0; 4]],
                    repair_bundles: vec![],
                    repairs_queued: 0,
                    last_sent: now,
                    t_adv: 0,
                },
                now,
            );
            // r = 0: released at once; nothing to ack. Resolve it as the
            // ledger would.
            t.on_ledger_resolved(bs);
        }
    }

    #[test]
    fn a_verdict_lagging_past_4096_later_bundles_still_skips() {
        let t0 = Instant::now();
        let r = rtt(50);
        let mut t = setup(2, 1, frag, t0);
        // Nothing arrives: the deadline releases both H.264 sources to the manager.
        let a = t.on_timer(h264_deadline(t0, r, 0), r);
        assert_eq!(losts(&a), 2);
        assert_eq!(t.on_verdict(100), VerdictAction::Skip);
        // 5000 later bundles within the 2.6 s the ledger can lag.
        let later = h264_deadline(t0, r, 0) + ms(2000);
        churn(&mut t, 5000, 1000, 50_000, later);
        assert_eq!(
            t.on_verdict(100),
            VerdictAction::Skip,
            "an unresolved bundle outlives the count of later bundles"
        );
        // The ledger resolves it, and the churn that follows may drop it.
        t.on_ledger_resolved(100);
        t.on_ledger_resolved(101);
        churn(&mut t, 5000, 7000, 100_000, later);
        assert_eq!(
            t.on_verdict(100),
            VerdictAction::Pass,
            "resolved entries age out"
        );
    }

    #[test]
    fn an_unresolved_tombstone_goes_at_the_retention_and_the_hard_cap_is_memory_only() {
        let t0 = Instant::now();
        let r = rtt(50);
        let mut t = setup(2, 1, frag, t0);
        t.on_timer(h264_deadline(t0, r, 0), r);
        // Younger than the retention: kept. Past it: gone.
        t.on_timer(t0 + TOMBSTONE_RETENTION - ms(1), r);
        t.prune_tombstones();
        assert_eq!(t.on_verdict(100), VerdictAction::Skip);
        let mut x = BlockTable::new();
        let t1 = t0 + TOMBSTONE_RETENTION * 2;
        x.on_source_sealed(1, 0, 5, vec![(5, tile(5))]);
        x.on_closed(
            ClosedBlock {
                block_id: 1,
                k: 1,
                r: 0,
                symbols: vec![vec![0; 4]],
                repair_bundles: vec![],
                repairs_queued: 0,
                last_sent: t0,
                t_adv: 0,
            },
            t0,
        );
        assert_eq!(x.on_verdict(5), VerdictAction::Skip);
        x.on_timer(t1, r);
        assert_eq!(x.on_verdict(5), VerdictAction::Pass, "past the retention");
        // The hard cap drops even an unresolved entry.
        let mut c = BlockTable::with_caps(1024, usize::MAX, 8);
        churn_unresolved(&mut c, 20, t0);
        assert_eq!(c.on_verdict(0), VerdictAction::Pass);
        assert_eq!(c.on_verdict(19), VerdictAction::Skip);
    }

    fn churn_unresolved(t: &mut BlockTable, n: u32, now: Instant) {
        for i in 0..n {
            t.on_source_sealed(i as BlockId, 0, i, vec![(i, tile(i))]);
            t.on_closed(
                ClosedBlock {
                    block_id: i as BlockId,
                    k: 1,
                    r: 0,
                    symbols: vec![vec![0; 4]],
                    repair_bundles: vec![],
                    repairs_queued: 0,
                    last_sent: now,
                    t_adv: 0,
                },
                now,
            );
        }
    }

    #[test]
    fn a_mixed_tile_and_h264_bundle_is_an_h264_source() {
        let t0 = Instant::now();
        let r = rtt(50);
        let mut t = BlockTable::new();
        t.on_source_sealed(BLK, 0, 100, vec![(1000, tile(0)), (1001, frag(1))]);
        t.on_source_sealed(BLK, 1, 101, vec![(1002, tile(2))]);
        t.on_closed(
            ClosedBlock {
                block_id: BLK,
                k: 2,
                r: 1,
                symbols: vec![vec![0; 4]; 2],
                repair_bundles: vec![200],
                repairs_queued: 0,
                last_sent: t0,
                t_adv: 0,
            },
            t0,
        );
        t.set_rtt(r);
        // T_lr (43 ms + srtt + 2 rttvar) comes first; past it the next is the
        // H.264 release, ahead of the tile one.
        t.on_timer(t_lr_at(t0, r, 0), r);
        assert_eq!(t.next_deadline(), Some(h264_deadline(t0, r, 0)));
        assert!(h264_deadline(t0, r, 0) < tile_deadline(t0, r, 0), "premise");
        // At the H.264 deadline the mixed bundle goes (to the manager and to
        // ARQ); the pure tile source does not.
        let a = t.on_timer(h264_deadline(t0, r, 0), r);
        assert_eq!((losts(&a), handoffs(&a)), (1, 1));
        assert_eq!(t.outcome(100), Some(Outcome::Lost));
        assert_eq!(t.outcome(101), None);
    }

    /// The old whole-table walk, for the property test to compare against.
    // The documented numbers, written out: the oracle must be able to disagree
    // with the production helpers, so it does not call them.
    const ORACLE_T_ADV_MS: [u64; 8] = [33, 10, 15, 20, 25, 40, 50, 66];
    const ORACLE_HINT_US: [u64; 16] = [
        0, 4_000, 5_657, 8_000, 11_314, 16_000, 22_627, 32_000, 45_255, 64_000, 90_510, 128_000,
        181_019, 256_000, 362_039, 512_000,
    ];
    fn oracle_round(r: Rtt) -> Duration {
        let h = if r.has_sample { r.hint_code } else { 0 };
        // hint + 30 ms, and never below the 43 ms of a client that has seen no hint.
        (Duration::from_micros(ORACLE_HINT_US[usize::from(h)]) + ms(30)).max(ms(43))
    }
    fn oracle_tile(last: Instant, r: Rtt, code: u8) -> Instant {
        last + ms(ORACLE_T_ADV_MS[usize::from(code)] + 10)
            + oracle_round(r)
            + r.srtt
            + r.max_ack_delay
    }
    fn oracle_h264(last: Instant, r: Rtt, code: u8) -> Instant {
        last + ms(ORACLE_T_ADV_MS[usize::from(code)])
            + (oracle_round(r) - ms(10))
            + r.srtt
            + r.max_ack_delay
            + ms(17)
    }
    fn oracle_t_lr(last: Instant, r: Rtt, code: u8) -> Instant {
        last + ms(ORACLE_T_ADV_MS[usize::from(code)] + 10) + r.srtt + 2 * r.rttvar
    }

    fn brute_next_deadline(t: &BlockTable) -> Option<Instant> {
        let rtt = t.rtt;
        let mut best: Option<Instant> = None;
        let mut take = |x: Instant| best = Some(best.map_or(x, |c| c.min(x)));
        for b in t.blocks.values() {
            match b.phase {
                Phase::Sending => {}
                Phase::Repair => {
                    let Some(last) = b.last_sent else { continue };
                    if b.t_lr_armed {
                        take(oracle_t_lr(last, rtt, b.t_adv));
                    }
                    for (i, s) in b.sources.iter().enumerate() {
                        if t.src_open(b, i) {
                            take(match s.kind {
                                Kind::Tile => oracle_tile(last, rtt, b.t_adv),
                                Kind::H264 => oracle_h264(last, rtt, b.t_adv),
                            });
                        }
                    }
                }
                Phase::Deferred => {
                    let Some(at) = b.deferred_at else { continue };
                    for (i, s) in b.sources.iter().enumerate() {
                        if t.src_open(b, i) {
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

    /// Randomised walk over two blocks of one table: events while a block is
    /// still `Sending`, eviction, every input, ledger resolution, verdicts.
    /// Every source ends released at most once, an acked or recovered one is
    /// never released, a verdict on a retained source is never `Pass`, and the
    /// heap-driven `next_deadline` equals the brute-force walk.
    #[test]
    fn property_exactly_once_with_eviction_sending_events_verdicts_and_retention() {
        let mut seed = 0x2545f4914f6cdd1du64;
        let mut rnd = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        let t0 = Instant::now();
        let mut released_total = 0usize;
        let mut checked_verdicts = 0usize;
        for round in 0..300u32 {
            // Blocks of mixed spans, and a path with or without a sample and hint.
            let mut r = rtt(40);
            r.has_sample = rnd(2) == 0;
            r.hint_code = rnd(16) as u8;
            let codes = [rnd(8) as u8, rnd(8) as u8];
            let mut t = BlockTable::with_caps(1, usize::MAX, FEC_TX_TOMBSTONES);
            t.set_rtt(r);
            let ids: [BlockId; 2] = [10, 11];
            let bases = [10_000u32, 30_000];
            let ks = [2 + rnd(5) as u8, 2 + rnd(5) as u8];
            let h264 = [rnd(2) == 0, rnd(2) == 0];
            let nr = 1 + rnd(2) as u8;
            let mut released: std::collections::HashMap<u32, u32> = Default::default();
            let note = |acts: &[Action], released: &mut std::collections::HashMap<u32, u32>| {
                for a in acts {
                    match a {
                        Action::HandOffTile { records, .. } => {
                            for x in records {
                                *released.entry(x.0).or_default() += 1;
                            }
                        }
                        Action::ContentLostH264 { wire_seq, .. } => {
                            *released.entry(*wire_seq).or_default() += 1;
                        }
                        _ => {}
                    }
                }
            };
            for bi in 0..2 {
                for i in 0..u32::from(ks[bi]) {
                    let ws = bases[bi] + i;
                    let c = if h264[bi] { frag(ws) } else { tile(ws) };
                    t.on_source_sealed(ids[bi], i as u8, ws, vec![(ws, c)]);
                }
            }
            let mut closed = [false; 2];
            let close = |t: &mut BlockTable, bi: usize, now: Instant| {
                t.on_closed(
                    ClosedBlock {
                        block_id: ids[bi],
                        k: ks[bi],
                        r: nr,
                        symbols: vec![vec![0; 4]; usize::from(ks[bi])],
                        repair_bundles: (0..u32::from(nr))
                            .map(|j| 20_000 + 1000 * bi as u32 + j)
                            .collect(),
                        repairs_queued: 0,
                        last_sent: now,
                        t_adv: codes[bi],
                    },
                    now,
                )
            };
            let mut now = t0;
            for _ in 0..30 {
                now += ms(rnd(60));
                let bi = rnd(2) as usize;
                let acts = match rnd(10) {
                    0 => t.on_arrival(bases[bi] + rnd(u64::from(ks[bi])) as u32, now),
                    1 => t.on_arrival(20_000 + 1000 * bi as u32 + rnd(u64::from(nr)) as u32, now),
                    2 => t.on_recovered(bases[bi] + rnd(u64::from(ks[bi])) as u32, now),
                    3 if rnd(3) == 0 => t.on_declined(ids[bi], ks[bi], &[rnd(256) as u8], now),
                    4 => t.on_request(
                        RepairAsk {
                            block_id: ids[bi],
                            have: rnd(u64::from(ks[bi]) + 1) as u8,
                            missing: 1,
                            first_new_idx: 0,
                        },
                        now,
                    ),
                    5 if !closed[bi] => {
                        closed[bi] = true;
                        close(&mut t, bi, now)
                    }
                    6 => t.evict_over_cap(),
                    7 => {
                        t.on_ledger_resolved(bases[bi] + rnd(u64::from(ks[bi])) as u32);
                        vec![]
                    }
                    _ => t.on_timer(now, r),
                };
                note(&acts, &mut released);
                assert_eq!(t.next_deadline(), brute_next_deadline(&t), "round {round}");
                for bi in 0..2 {
                    for i in 0..u32::from(ks[bi]) {
                        let ws = bases[bi] + i;
                        let o = t.outcome(ws);
                        let n = released.get(&ws).copied().unwrap_or(0);
                        let was_released = matches!(o, Some(Outcome::HandedOff | Outcome::Lost));
                        assert_eq!(
                            n,
                            u32::from(was_released),
                            "source {ws}: {o:?}, released {n}"
                        );
                        let v = t.on_verdict(ws);
                        match o {
                            None => assert_eq!(v, VerdictAction::Park, "source {ws} still owes"),
                            Some(_) => assert_eq!(v, VerdictAction::Skip, "source {ws} is settled"),
                        }
                        checked_verdicts += 1;
                    }
                }
            }
            let late = now + ms(500);
            for (bi, was_closed) in closed.iter().enumerate() {
                if !was_closed {
                    let a = close(&mut t, bi, late);
                    note(&a, &mut released);
                }
            }
            let a = t.on_timer(late, r);
            note(&a, &mut released);
            for bi in 0..2 {
                let a = t.on_declined(ids[bi], ks[bi], &[0], late);
                note(&a, &mut released);
            }
            let a = t.evict_over_cap();
            note(&a, &mut released);
            for bi in 0..2 {
                for i in 0..u32::from(ks[bi]) {
                    let ws = bases[bi] + i;
                    let o = t.outcome(ws).expect("every source has an outcome");
                    let n = released.get(&ws).copied().unwrap_or(0);
                    let was_released = matches!(o, Outcome::HandedOff | Outcome::Lost);
                    assert_eq!(
                        n,
                        u32::from(was_released),
                        "source {ws}: {o:?}, released {n}"
                    );
                    released_total += n as usize;
                    // The tombstone keeps it: a late verdict never passes.
                    assert_eq!(t.on_verdict(ws), VerdictAction::Skip);
                    checked_verdicts += 1;
                }
                assert!(!t.is_open(ids[bi]));
            }
        }
        assert!(
            released_total > 100,
            "premise: releases happened ({released_total})"
        );
        assert!(checked_verdicts > 1000);
    }

    /// With code 0 and no hint the deadlines are today's values exactly. This
    /// replaces the test that mirrored the client's constants.
    #[test]
    fn with_code_zero_and_no_hint_the_deadlines_equal_the_pre_s1_values() {
        let t0 = Instant::now();
        for (srtt, var, mad) in [(50u64, 10u64, 0u64), (0, 0, 0), (20, 5, 7), (300, 40, 25)] {
            let r = Rtt {
                srtt: ms(srtt),
                rttvar: ms(var),
                max_ack_delay: ms(mad),
                has_sample: false,
                hint_code: 0,
            };
            assert_eq!(tile_deadline(t0, r, 0), t0 + ms(86 + srtt + mad));
            assert_eq!(h264_deadline(t0, r, 0), t0 + ms(83 + srtt + mad));
            assert_eq!(t_lr_at(t0, r, 0), t0 + ms(43 + srtt + 2 * var));
            // A sample with no hint sent yet changes nothing either.
            let with = Rtt {
                has_sample: true,
                ..r
            };
            assert_eq!(tile_deadline(t0, with, 0), tile_deadline(t0, r, 0));
        }
    }

    /// Before an RTT sample the round is the unadvertised 43 ms, whatever the
    /// tracker says; the has-sample flag is what says so.
    #[test]
    fn the_request_round_is_43_ms_until_there_is_a_sample() {
        let t0 = Instant::now();
        let base = Rtt {
            hint_code: 15,
            ..rtt(40)
        };
        assert!(!base.has_sample);
        assert_eq!(tile_deadline(t0, base, 0), t0 + ms(86 + 40));
        let sampled = Rtt {
            has_sample: true,
            ..base
        };
        // Hint 15 is 512 ms, plus the 30 ms margin, in place of 43.
        assert_eq!(tile_deadline(t0, sampled, 0), t0 + ms(43 + 512 + 30 + 40));
    }

    /// `h264_deadline` is three milliseconds before `tile_deadline` for every
    /// code and hint, and neither falls inside the first window.
    #[test]
    fn the_h264_deadline_is_three_ms_before_the_tile_one_for_every_code_and_hint() {
        let t0 = Instant::now();
        for code in 0..8u8 {
            for hint in 0..16u8 {
                for has_sample in [false, true] {
                    let r = Rtt {
                        has_sample,
                        hint_code: hint,
                        ..rtt(25)
                    };
                    let (tile, h264) = (tile_deadline(t0, r, code), h264_deadline(t0, r, code));
                    assert_eq!(tile - h264, ms(3), "code {code} hint {hint}");
                    let a = ms(ORACLE_T_ADV_MS[usize::from(code)] + 10);
                    assert!(tile >= t0 + a + r.srtt);
                    assert!(h264 >= t0 + a + r.srtt);
                    assert_eq!(tile, oracle_tile(t0, r, code));
                    assert_eq!(h264, oracle_h264(t0, r, code));
                    assert_eq!(t_lr_at(t0, r, code), oracle_t_lr(t0, r, code));
                }
            }
        }
    }

    /// A client keeps the last hint it received, which can be older than the
    /// server's view of the path. Whatever hint the client holds (`c`) and the
    /// server's recent maximum (`h >= c`), the server's release comes after
    /// the client's own window and request round.
    #[test]
    fn the_deadline_covers_a_client_holding_a_stale_hint() {
        let t0 = Instant::now();
        let (mut client_lower, mut client_higher) = (0usize, 0usize);
        for code in 0..8u8 {
            for srtt_ms in [4u64, 30, 90, 300] {
                // The hint the fresh srtt alone would give.
                let fresh = (1..16usize)
                    .find(|&i| ORACLE_HINT_US[i] >= srtt_ms * 1_000)
                    .unwrap_or(15) as u8;
                for c in 0..16u8 {
                    for h in c..16u8 {
                        let r = Rtt {
                            has_sample: true,
                            hint_code: h,
                            ..rtt(srtt_ms)
                        };
                        // What the client waits: its window, then its round for `c`.
                        let client_round = match ORACLE_HINT_US[usize::from(c)] {
                            0 => ms(43),
                            us => Duration::from_micros(us) + ms(30),
                        };
                        let first = ms(ORACLE_T_ADV_MS[usize::from(code)] + 10);
                        let need = t0 + first + client_round + r.srtt;
                        assert!(
                            tile_deadline(t0, r, code) >= need,
                            "code {code} srtt {srtt_ms} client hint {c} server hint {h}"
                        );
                        if c != 0 && c < fresh {
                            client_lower += 1;
                        }
                        if c > fresh {
                            client_higher += 1;
                        }
                    }
                }
            }
        }
        assert!(client_lower > 100, "premise: clients below the fresh hint");
        assert!(client_higher > 100, "premise: clients above the fresh hint");
    }

    #[test]
    fn the_hint_tracker_keeps_a_sample_between_one_and_two_seconds() {
        let t0 = Instant::now();
        let mut h = HintTracker {
            start: None,
            cur: 0,
            prev: 0,
        };
        assert_eq!(h.max(t0), 0);
        h.note(9, t0);
        h.note(4, t0 + ms(100));
        assert_eq!(h.max(t0 + ms(200)), 9, "the largest, not the last");
        assert_eq!(h.max(t0 + ms(1_900)), 9, "kept for at least one second");
        // Past two seconds with nothing new, it is gone.
        assert_eq!(h.max(t0 + ms(3_100)), 0);
        // A lower later hint does not hide a higher recent one, and a sample
        // dies without being refreshed.
        h.note(12, t0 + ms(4_000));
        h.note(3, t0 + ms(4_900));
        assert_eq!(
            h.max(t0 + ms(4_999)),
            12,
            "a sample under a second old is kept"
        );
        assert_eq!(h.max(t0 + ms(5_100)), 3, "the 12 aged out, the 3 is recent");
    }

    #[test]
    fn the_table_reports_the_recent_hint_it_was_told_of() {
        let t0 = Instant::now();
        let mut t = BlockTable::new();
        assert_eq!(t.recent_hint(t0), 0);
        t.note_hint_sent(7, t0);
        assert_eq!(t.recent_hint(t0 + ms(10)), 7);
    }
}
