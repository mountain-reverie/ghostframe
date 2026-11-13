// SPDX-License-Identifier: AGPL-3.0-or-later
//! Per-transmission accounting, independent of whether the content survived.
//!
//! The congestion controller asks "did this packet arrive?". The retransmit
//! cache answers "did this content arrive?", and is legitimately emptied when
//! a tile is superseded — on a churning screen, most of the time. Answering
//! the first question from the second therefore loses most losses: measured,
//! goog_cc saw a loss fraction of 0.4-3% on a link shedding ~7% by bytes, far
//! too low to trigger backoff, leaving a 4x overestimate of a degraded link
//! standing.
//!
//! This ledger is the separate answer. `cancel_for_tile` does not touch it:
//! the packet's fate is still owed to the estimator once its content is
//! garbage.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::transport::delivery_sampler::RateMark;
use crate::transport::reliable_emitter::Content;

/// What was sent, and what it was sent for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transmission {
    /// Clock-relative microseconds, the same value stamped into the datagram
    /// header — so the estimator sees the epoch its other inputs use.
    pub emit_us: u64,
    pub wire_bytes: usize,
    /// What this transmission carried: a tile pass or an H.264 fragment. The
    /// consumers that think in tile passes read [`Content::tile`].
    pub content: Content,
    /// The emitter-clock microseconds (the clock `emit_us` is on) at which
    /// this transmission left quinn's FIFO for the wire. `None` while it is
    /// still queued, in the emitter or in quinn. `emit_us` is when it was
    /// built; this is when it was sent, and the difference is the time it
    /// sat in a queue.
    pub depart_us: Option<u64>,
    /// Position in the order datagrams left quinn's FIFO: 0, 1, 2, ... one
    /// per datagram, assigned by [`TransmissionLedger::mark_departed`] in the
    /// departure tracker's FIFO order. `None` while queued. `depart_us` is a
    /// per-turn stamp (every datagram of one `drain_outbound` shares it), so it
    /// cannot say which of two such datagrams went first; this can, and loss
    /// detection counts in it.
    pub dep_idx: Option<u64>,
}

/// Which rule judged a packet-threshold verdict, with respect to the
/// reordering window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReoBranch {
    /// The window was closed, or the newest acknowledged departure left at
    /// least `reo_wnd` after the transmission: the ordinary count rule.
    Open,
    /// Inside the window, at the learned depth.
    Depth,
    /// Inside the window, at the ordinary count, once the stream of later
    /// departures had been quiet for `reo_wnd`.
    Quiet,
}

/// Why a transmission was declared lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LossReason {
    /// `K_PKT` or more departures after it have been acknowledged (RFC 9002
    /// section 6.1.1).
    Packet,
    /// It departed more than the time threshold before the newest
    /// acknowledged departure (section 6.1.2).
    Time,
    /// The probe timeout found its content gone (superseded) while it was the
    /// newest outstanding: it cannot be probed, and it is not coming back.
    Pto,
}

/// RFC 9002 section 6.1.1: a transmission is lost when this many departures
/// after it have been acknowledged.
pub(crate) const K_PKT_INITIAL: u64 = 3;
/// The adaptive packet threshold never exceeds this (RACK's reordering window
/// is bounded the same way).
pub(crate) const K_PKT_MAX: u64 = 8;
/// Packet-threshold verdicts the adaptation looks back over.
const ADAPT_WINDOW: u64 = 64;
/// Verdicts the window must hold before `K_PKT` may be raised: a share of a
/// handful is no evidence.
const ADAPT_MIN_VERDICTS: u64 = 32;
/// The reordering window RFC 8985 section 6.2 calls `reo_wnd`, here a
/// departure-time gap. A packet-threshold verdict also needs the newest
/// acknowledged departure to have left at least this long after the
/// transmission. Zero until a spurious packet or time verdict (a late
/// acknowledgement) shows the path reorders; the time rule's threshold also
/// grows by it. It learns the departure gap a contradicted packet or time
/// verdict showed, in steps of [`REO_STEP_DIVISOR`]th of `min_rtt` at most one
/// per round trip (departures of one `drain_outbound` turn share a stamp, so
/// the measured gap of a datagram displaced inside a burst is zero, and one
/// outlier must not open the window to its cap).
const REO_STEP_DIVISOR: u32 = 8;
/// Inside the window (the newest acknowledged departure left less than
/// `reo_wnd` after a transmission, which includes everything stamped with the
/// same departure) the count rule stays, at the depth the path was seen to
/// displace a datagram by (plus one). A displacement past this saturates the
/// depth here (one outlier must not switch the rule off for the session: the
/// acknowledgement batching inflates a displacement, a whole frame can resolve
/// in one batch before detection runs). 64 and not 32: the bursty replay (40
/// datagram frames, 8 ms of reordering) makes 0.068 spurious verdicts per
/// datagram at a depth of 32 (0.022 with batched acknowledgements) and none
/// at 40 or more.
const REO_DEPTH_MAX: u64 = 64;
/// Step and ceiling when no `min_rtt` is known (tests).
const REO_STEP_FALLBACK: Duration = Duration::from_millis(5);
const REO_CAP_FALLBACK: Duration = Duration::from_millis(50);
/// The ceiling is `min_rtt / REO_CAP_DIVISOR`, never above [`REO_CAP_FALLBACK`].
const REO_CAP_DIVISOR: u32 = 2;
/// Quiet loss episodes (a verdict batch past the previous episode's recovery
/// point, with no late acknowledgement for two time thresholds before it)
/// after which `reo_wnd` returns to zero. RFC
/// 8985 section 6.2 keeps it for 16 loss recoveries.
const REO_PERSIST: u32 = 16;
/// An episode only counts toward that reset when no verdict has been
/// contradicted for this long: a path that reorders in bursts, with seconds
/// of calm between them, must not lose its window (and pay to learn it again,
/// on the legacy H.264 path a spurious loss IDR) after sixteen lossy frames.
const REO_QUIET_SPAN: Duration = Duration::from_secs(30);
/// Hard cap on the parity departures remembered. The real bound is time (the
/// caller's `retain`, at least 600 ms): at 200 Mbit/s and 1,200-byte datagrams
/// that is about 12,500 groups, so the cap is a backstop for rates beyond that,
/// not a rate-dependent limit on ordinary operation.
const PARITY_RING_CAP: usize = 16_384;

/// Resolved entries `scan_outstanding` steps over per call.
pub(crate) const SCAN_DEAD_BUDGET: usize = 4_096;
/// Spurious share of the window above which `K_PKT` goes up by one: 10 %.
const ADAPT_SPURIOUS_PERCENT: u64 = 10;

/// What an acknowledgement turned out to refer to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// Outstanding when the acknowledgement arrived — the normal case.
    Live(Transmission),
    /// Already expired and reported to the estimator as lost, but
    /// acknowledged after all.
    ///
    /// The content still has to be released, or its cache entry is stranded
    /// forever: nothing else ever clears it, and it retransmits at the
    /// backoff ceiling for the rest of the session. The timing, however, must
    /// **not** be fed to the estimator — that transmission has already been
    /// accounted for as a loss, and counting it again would report the same
    /// bytes twice.
    Late {
        tx: Transmission,
        /// This acknowledgement may be the client acking a datagram it
        /// rebuilt from parity under its original `wire_seq`: a real network
        /// loss, not a premature verdict. Nothing on the wire says which, so
        /// the ledger asks what would have to be true of a recovery, from
        /// what the server itself did:
        ///
        /// - the group's parity departed at least `min_rtt` before this
        ///   acknowledgement (the parity has to cross the path and the
        ///   acknowledgement come back), and
        /// - no other member of the group is outstanding or declared lost
        ///   (XOR parity with k = 10 rebuilds exactly one).
        ///
        /// Both are facts about departures and acknowledgements, so the test
        /// does not depend on the send rate or on how the group's parity came
        /// to leave (full interleave or the end-of-stream flush). Treated as
        /// a loss that happened (no retraction, no threshold adaptation) and
        /// counted apart. A late acknowledgement failing either test cannot
        /// be a recovery.
        fec_possible: bool,
    },
}

/// A verdict kept so a late acknowledgement can be recognised.
#[derive(Debug, Clone, Copy)]
struct Tombstone {
    tx: Transmission,
    /// Number of the packet-threshold verdict, for the adaptation window.
    /// `None` for time-threshold and probe verdicts, which do not adapt it.
    packet_verdict_no: Option<u64>,
    /// Why it was declared lost: only packet and time verdicts say anything
    /// about reordering (a probe-timeout verdict is a stall).
    reason: LossReason,
    /// The newest acknowledged departure when the verdict was issued: how
    /// far past this transmission the verdict thought the path had moved, free
    /// of what the acknowledgement batching added before the late ack.
    largest_idx_at_verdict: Option<u64>,
    largest_at_at_verdict: Option<Instant>,
    largest_arrival_at_verdict: Option<Instant>,
    departed: Option<Instant>,
    /// The delivery-rate mark the record carried, so a late acknowledgement
    /// is still a delivery sample (D1 PR-6b).
    rate: Option<RateMark>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LedgerStats {
    /// Acknowledgements naming a `wire_seq` this ledger has no record of.
    /// Either a duplicate (the overlap entries every ACK batch carries, or a
    /// genuinely duplicated datagram) or an acknowledgement for a verdict whose
    /// tombstone the ring has since evicted. Counted rather than silent: a
    /// sustained rise means verdicts are outrunning acknowledgements by more
    /// than the tombstone ring holds, or a peer is confused.
    pub unknown_acks: u64,
    /// Records dropped by the hard cap rather than by a verdict. Non-zero
    /// means transmissions outran acknowledgements badly enough that the cap
    /// bound the ledger instead of the detector — the loss ratio is under-reported
    /// while this is rising.
    pub capacity_evictions: u64,
    /// Acknowledgements that arrived after their transmission had been
    /// declared lost. A healthy link reads near zero; on the lossless 70 ms
    /// reproduction this read 1458 per 8-second scene, every one of which
    /// stranded a cache entry.
    pub acked_after_declared_lost: u64,
    /// Tombstones pushed out of the ring by newer ones. A late acknowledgement
    /// for an evicted one resolves to nothing (counted in `unknown_acks`), so
    /// `acked_after_declared_lost` is blind to it. Measurement only.
    pub tombstone_evictions: u64,
    /// Verdicts issued on a record that had not departed. Reachable only if
    /// the expiry walk stops following departure order (it walks
    /// `departed_order`, whose entries all carry a stamp), so this guards the
    /// walk order, NOT the departure tracker: that rests on the real-quinn
    /// proptest in `departure.rs`. It reads positive under the mutation "walk
    /// in recording order" (see the burst-loss P3 gate).
    pub expired_before_departure: u64,
    /// Departures stamped on a record (one per transmission, at most).
    pub departures_stamped: u64,
    /// Transmissions evicted by the capacity cap before they had departed.
    pub evicted_before_departure: u64,
    /// Records the sender took back before they departed
    /// ([`TransmissionLedger::forget_undeparted`]): superseded H.264
    /// fragments removed from the emission queue. Neither delivered nor lost.
    pub forgotten_before_departure: u64,
    /// Outstanding records the sender took back with no verdict
    /// ([`TransmissionLedger::forget_outstanding`]).
    pub forgotten_without_verdict: u64,
    /// Verdicts by reason. Their sum is the number of transmissions declared
    /// lost.
    pub lost_packet_threshold: u64,
    pub lost_time_threshold: u64,
    pub lost_pto: u64,
    /// Verdicts later contradicted: the transmission was acknowledged after
    /// being declared lost. Equal to `acked_after_declared_lost` (a tombstone
    /// resolves once); kept under the Eifel name.
    pub spurious_losses: u64,
    /// Of `spurious_losses`, those that arrived late enough to be an FEC
    /// recovery (see [`Resolution::Late`]). They do not retract the loss or
    /// adapt `K_PKT`.
    pub late_acks_fec_possible: u64,
    /// The packet threshold now (3 to 8) and how often it has been raised.
    pub k_pkt: u64,
    pub k_pkt_raises: u64,
    /// The reordering window now (microseconds) and how often it was raised.
    pub reo_wnd_us: u64,
    pub reo_wnd_raises: u64,
    /// Contradicted verdicts the window learned from, by the rule that made
    /// them (a probe-timeout verdict never teaches it).
    pub reo_learned_packet: u64,
    pub reo_learned_time: u64,
    /// Largest displacement, in departures, and in departure time, between the
    /// newest acknowledged departure and a transmission whose verdict a late
    /// acknowledgement contradicted. Measurement only.
    pub spurious_disp_max: u64,
    pub spurious_gap_us_max: u64,
    /// The depth of the in-window count rule now (0: none learned).
    pub reo_depth: u64,
    /// Late acknowledgements that taught nothing because they followed an
    /// ack-path stall (no acknowledgement advanced the reference since the
    /// verdict, and the gap was at least a time threshold).
    pub reo_stall_skips: u64,
    /// Packet verdicts judged inside the window at the learned depth, and by
    /// the stream going quiet.
    pub reo_judged_depth: u64,
    pub reo_judged_quiet: u64,
}

#[derive(Debug, Clone, Copy)]
struct Record {
    /// When the ledger was told of it (enqueue). Identity and capacity order
    /// only: loss is judged on departure (`departed`, `dep_idx`).
    #[allow(dead_code)]
    recorded_at: Instant,
    /// When it left quinn's FIFO. `None` until then.
    departed: Option<Instant>,
    /// The delivery-rate mark taken at departure (D1 PR-6b). Observability.
    rate: Option<RateMark>,
    tx: Transmission,
}

/// A datagram that departed inside a probe burst is judged lost by the time
/// rule after this many times the usual threshold.
const BURST_REORDER_FACTOR: u32 = 3;
/// Burst spans kept (a span is a probe window and its drain).
const MAX_PROBE_BURSTS: usize = 16;
/// How many patient records the loss scan steps over looking for verdicts
/// behind them.
const PATIENT_SCAN_LIMIT: usize = 2048;

pub struct TransmissionLedger {
    records: HashMap<u32, Record>,
    /// Recording order, for the capacity cap: the oldest record goes first.
    /// Resolved entries are skipped lazily.
    order: VecDeque<u32>,
    /// Departure order, for loss detection. A transmission enters when it is
    /// stamped, so the front is always the one that has been on the wire the
    /// longest and the walk in `detect_losses_at` stops at the first not lost
    /// one. Resolved entries are skipped lazily.
    departed_order: VecDeque<u32>,
    capacity: usize,
    stats: LedgerStats,
    /// `wire_seq -> Tombstone` for records declared lost, retained so a late
    /// acknowledgement can still release its content. Bounded by the same
    /// capacity as `records`; oldest evicted first.
    tombstones: HashMap<u32, Tombstone>,
    tombstone_order: VecDeque<u32>,
    /// Next departure index to hand out.
    next_dep_idx: u64,
    /// The span of a probe burst (its window plus the queue it leaves): a
    /// datagram that departed inside it gets a longer time threshold. See
    /// [`Self::mark_probe_burst`].
    probe_bursts: VecDeque<(Instant, Instant)>,
    /// The newest acknowledged departure: its index and when it left. Loss
    /// is only ever judged against this, never against the clock, so a queue
    /// that delays everything in order cannot produce a verdict (F4).
    largest_acked: Option<u64>,
    /// When `largest_acked` left quinn's FIFO.
    largest_acked_at: Option<Instant>,
    /// When the acknowledgement that last advanced `largest_acked` arrived
    /// (`None` without arrival times): the stream of later departures going
    /// quiet is what ends the wait inside the reordering window.
    largest_acked_arrival: Option<Instant>,
    /// See [`REO_STEP_DIVISOR`].
    reo_wnd: Duration,
    freeze_reo_wnd: bool,
    /// Quiet loss episodes since `reo_wnd` was last raised or a late
    /// acknowledgement was seen.
    reo_recoveries: u32,
    /// `next_dep_idx` when the last episode was counted: verdicts on earlier
    /// departures are the same recovery.
    reo_recovery_point: u64,
    /// Displacement (departures) a contradicted verdict showed, plus one, at
    /// most [`REO_DEPTH_MAX`].
    reo_depth: u64,
    /// The time threshold the last loss query used: what a stalled ack path
    /// is told from reordering by.
    last_time_threshold: Option<Duration>,
    /// Which branch judged each packet verdict (the replay reads and clears it).
    #[cfg(any(test, feature = "test-loss-injection"))]
    verdict_branches: Vec<(u32, ReoBranch)>,
    last_reo_raise: Option<Instant>,
    last_spurious_ack: Option<Instant>,
    /// When each group's parity left quinn's FIFO, by `group_first_wire_seq`.
    /// Bounded by time (see [`Self::note_parity_departed`]) and, as a
    /// backstop, by [`PARITY_RING_CAP`].
    parity_departed: HashMap<u32, Instant>,
    /// The same entries in departure order, for pruning.
    parity_order: VecDeque<(u32, Instant)>,
    /// Current packet threshold, and what it adapts from: verdicts issued so
    /// far, and one bit per recent verdict (by verdict number modulo 64) set
    /// when that verdict was contradicted by a late acknowledgement.
    k_pkt: u64,
    freeze_k_pkt: bool,
    verdicts: u64,
    spurious_bits: u64,
    /// Wire bytes of the records that have departed and not yet left the
    /// ledger (acknowledged, declared lost, evicted or replaced): the
    /// sampler's `bytes_in_flight`.
    departed_bytes: u64,
}

impl TransmissionLedger {
    pub fn new(capacity: usize) -> Self {
        Self {
            records: HashMap::new(),
            order: VecDeque::new(),
            departed_order: VecDeque::new(),
            capacity,
            stats: LedgerStats::default(),
            tombstones: HashMap::new(),
            tombstone_order: VecDeque::new(),
            next_dep_idx: 0,
            probe_bursts: VecDeque::new(),
            largest_acked: None,
            largest_acked_at: None,
            largest_acked_arrival: None,
            reo_wnd: Duration::ZERO,
            freeze_reo_wnd: false,
            reo_recoveries: 0,
            reo_recovery_point: 0,
            reo_depth: 0,
            last_time_threshold: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            verdict_branches: Vec::new(),
            last_reo_raise: None,
            last_spurious_ack: None,
            parity_departed: HashMap::new(),
            parity_order: VecDeque::new(),
            k_pkt: K_PKT_INITIAL,
            freeze_k_pkt: false,
            verdicts: 0,
            spurious_bits: 0,
            departed_bytes: 0,
        }
    }

    /// Keep `K_PKT` at its initial value whatever the spurious rate says. The
    /// mutation the adaptive-threshold gate is shown against.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn freeze_k_pkt(&mut self) {
        self.freeze_k_pkt = true;
    }

    /// Keep the reordering window at zero. The mutation the reorder-window
    /// gates are shown against.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn freeze_reo_wnd(&mut self) {
        self.freeze_reo_wnd = true;
    }

    pub fn reo_wnd(&self) -> Duration {
        self.reo_wnd
    }

    /// Which branch judged each packet verdict since the last call.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn take_verdict_branches(&mut self) -> Vec<(u32, ReoBranch)> {
        std::mem::take(&mut self.verdict_branches)
    }

    fn advance_largest_acked(&mut self, idx: u64, at: Instant, arrival: Option<Instant>) {
        if self.largest_acked.is_none_or(|l| idx > l) {
            self.largest_acked = Some(idx);
            self.largest_acked_at = Some(at);
            self.largest_acked_arrival = arrival;
        }
    }

    /// A late acknowledgement contradicted a packet or time verdict that is
    /// not an FEC recovery: the path reorders by at least what this one
    /// showed (`extent` in departure time, `disp` in departures, both as of
    /// the verdict). The window grows one step per round trip towards the
    /// extent, within its cap.
    fn learn_reordering(
        &mut self,
        now: Option<Instant>,
        min_rtt: Option<Duration>,
        extent: Duration,
        disp: u64,
    ) {
        self.reo_recoveries = 0;
        self.last_spurious_ack = now;
        if self.freeze_reo_wnd {
            return;
        }
        self.reo_depth = self.reo_depth.max((disp + 1).min(REO_DEPTH_MAX));
        let step = min_rtt.map_or(REO_STEP_FALLBACK, |m| m / REO_STEP_DIVISOR);
        let cap = min_rtt.map_or(REO_CAP_FALLBACK, |m| {
            (m / REO_CAP_DIVISOR).min(REO_CAP_FALLBACK)
        });
        let target = extent.max(step).min(cap);
        let round = min_rtt.unwrap_or(step * REO_STEP_DIVISOR);
        let due = match (self.last_reo_raise, now) {
            (Some(prev), Some(n)) => n.saturating_duration_since(prev) >= round,
            _ => true,
        };
        // Whole steps only (the last one stops at the cap), so a measurement
        // a hair above the window is a step and the raises stay few.
        let next = (self.reo_wnd + step).min(cap);
        if target >= self.reo_wnd + step / 2 && due {
            self.reo_wnd = next;
            self.last_reo_raise = now;
            self.stats.reo_wnd_raises += 1;
        }
    }

    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
            reo_wnd_us: self.reo_wnd.as_micros() as u64,
            reo_depth: self.reo_depth,
            k_pkt: self.k_pkt,
            ..self.stats
        }
    }

    /// A group's parity datagram left quinn's FIFO at `at`. `group_first` is
    /// the first `wire_seq` of the group it covers (the parity envelope's own
    /// field). Entries older than `retain` are forgotten: a recovery's
    /// acknowledgement comes about a round trip after the parity, so the
    /// caller passes a few round trips (never less than the loss window's
    /// settle time), whatever the send rate.
    pub fn note_parity_departed(&mut self, group_first: u32, at: Instant, retain: Duration) {
        while let Some(&(g, t)) = self.parity_order.front() {
            if self.parity_order.len() <= PARITY_RING_CAP
                && at.saturating_duration_since(t) <= retain
            {
                break;
            }
            self.parity_order.pop_front();
            // A newer departure of the same group first replaces it.
            if self.parity_departed.get(&g) == Some(&t) {
                self.parity_departed.remove(&g);
            }
        }
        self.parity_departed.insert(group_first, at);
        self.parity_order.push_back((group_first, at));
    }

    /// Could a late acknowledgement of `wire_seq` be an FEC recovery? See
    /// [`Resolution::Late::fec_possible`]. `ack` is `(now, min_rtt)`; `None`
    /// skips the parity-age test. O(group size).
    fn fec_possible(&self, wire_seq: u32, ack: Option<(Instant, Duration)>) -> bool {
        let k = crate::transport::reliable_emitter::FEC_GROUP_SIZE_K as u32;
        let Some((first, left)) = (0..k)
            .map(|i| wire_seq.wrapping_sub(i))
            .find_map(|first| self.parity_departed.get(&first).map(|t| (first, *t)))
        else {
            return false;
        };
        if let Some((now, min_rtt)) = ack {
            if now.saturating_duration_since(left) < min_rtt {
                return false;
            }
        }
        (0..k)
            .map(|i| first.wrapping_add(i))
            .filter(|m| *m != wire_seq)
            .all(|m| !self.records.contains_key(&m) && !self.tombstones.contains_key(&m))
    }

    pub fn k_pkt(&self) -> u64 {
        self.k_pkt
    }

    pub fn tombstone_len(&self) -> usize {
        self.tombstones.len()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Record a transmission as outstanding.
    ///
    /// `wire_seq` is unique per transmission (retransmissions allocate their
    /// own), so a collision here would mean the allocator wrapped all the way
    /// around while an entry was still outstanding. The old record is
    /// replaced and counted as a capacity eviction rather than silently
    /// dropped, since it can no longer be classified.
    ///
    /// Recording is identity only. The transmission is in no departure order until
    /// [`Self::mark_departed`]: a transmission still queued in the emitter or
    /// in quinn's FIFO has not been sent, so it cannot have been lost.
    pub fn record(&mut self, wire_seq: u32, at: Instant, tx: Transmission) {
        let rec = Record {
            recorded_at: at,
            departed: None,
            rate: None,
            tx: Transmission {
                depart_us: None,
                dep_idx: None,
                ..tx
            },
        };
        if let Some(old) = self.records.insert(wire_seq, rec) {
            self.leave(&old);
            self.stats.capacity_evictions += 1;
            self.order.retain(|w| *w != wire_seq);
        }
        self.order.push_back(wire_seq);
        while self.records.len() > self.capacity {
            match self.order.pop_front() {
                Some(oldest) => {
                    if let Some(r) = self.records.remove(&oldest) {
                        self.leave(&r);
                        self.stats.capacity_evictions += 1;
                        if r.departed.is_none() {
                            self.stats.evicted_before_departure += 1;
                        }
                    }
                }
                None => break,
            }
        }
        self.compact_if_sparse();
    }

    /// [`Self::record`] for a test that wants the transmission on the wire:
    /// stamped as departed at `at`, `depart_us` the same as its `emit_us`.
    #[cfg(test)]
    pub(crate) fn record_departed(&mut self, wire_seq: u32, at: Instant, tx: Transmission) {
        let us = tx.emit_us;
        self.record(wire_seq, at, tx);
        self.mark_departed(wire_seq, at, us);
    }

    /// Stamp `wire_seq` as having left quinn's FIFO at `at` (`depart_us` on
    /// the emitter's clock). From here it has a departure index and a time and can be judged.
    ///
    /// Returns whether it stamped a record: `false` for a transmission that
    /// is unknown (already acknowledged, expired, evicted) or already
    /// stamped. O(1).
    pub fn mark_departed(&mut self, wire_seq: u32, at: Instant, depart_us: u64) -> bool {
        let Some(rec) = self.records.get_mut(&wire_seq) else {
            return false;
        };
        if rec.departed.is_some() {
            return false;
        }
        rec.departed = Some(at);
        rec.tx.depart_us = Some(depart_us);
        rec.tx.dep_idx = Some(self.next_dep_idx);
        self.next_dep_idx += 1;
        self.departed_bytes += rec.tx.wire_bytes as u64;
        self.departed_order.push_back(wire_seq);
        self.stats.departures_stamped += 1;
        true
    }

    /// Wire bytes that have departed and are still outstanding. O(1).
    pub fn departed_bytes_in_flight(&self) -> u64 {
        self.departed_bytes
    }

    /// Take the delivery-rate mark for `wire_seq`, which has just been
    /// stamped by [`Self::mark_departed`]: `make(wire_bytes,
    /// bytes_in_flight_before, dep_idx)` builds it and the record keeps it.
    /// O(1).
    pub fn mark_rate(&mut self, wire_seq: u32, make: impl FnOnce(usize, u64, u64) -> RateMark) {
        let Some(rec) = self.records.get_mut(&wire_seq) else {
            return;
        };
        if rec.departed.is_none() || rec.rate.is_some() {
            return;
        }
        let bytes = rec.tx.wire_bytes;
        let before = self.departed_bytes.saturating_sub(bytes as u64);
        let Some(dep_idx) = rec.tx.dep_idx else {
            return;
        };
        rec.rate = Some(make(bytes, before, dep_idx));
    }

    /// The delivery-rate mark of `wire_seq`, outstanding or declared lost
    /// (its tombstone keeps it), and its departure instant, for the
    /// acknowledgement that is about to resolve it. O(1).
    pub fn rate_mark(&self, wire_seq: u32) -> Option<(RateMark, Instant)> {
        if let Some(rec) = self.records.get(&wire_seq) {
            return Some((rec.rate?, rec.departed?));
        }
        let t = self.tombstones.get(&wire_seq)?;
        Some((t.rate?, t.departed?))
    }

    /// A record leaves the ledger without being resolved by [`Self::resolve`]
    /// or declared lost: keep the in-flight tally honest.
    fn leave(&mut self, rec: &Record) {
        if rec.departed.is_some() {
            self.departed_bytes = self.departed_bytes.saturating_sub(rec.tx.wire_bytes as u64);
        }
    }

    /// Has `wire_seq` left quinn's FIFO? `None` if the ledger does not hold it.
    pub fn departed(&self, wire_seq: u32) -> Option<bool> {
        self.records.get(&wire_seq).map(|r| r.departed.is_some())
    }

    /// Outstanding records whose content `pred` accepts, for a test of the
    /// accounting identity (recorded = acknowledged + judged + forgotten +
    /// outstanding).
    #[cfg(test)]
    pub(crate) fn count_outstanding(&self, pred: impl Fn(&Content) -> bool) -> usize {
        self.records
            .values()
            .filter(|r| pred(&r.tx.content))
            .count()
    }

    /// Forget one record that has not departed: its datagram was taken out of
    /// the emission queue before it reached quinn (an H.264 fragment of a frame
    /// an IDR superseded). It was never sent, so it is neither delivered nor
    /// lost: no verdict, no tombstone. `false` if the record is unknown or has
    /// departed (a departed one is judged like any other).
    pub fn forget_undeparted(&mut self, wire_seq: u32) -> bool {
        if self
            .records
            .get(&wire_seq)
            .is_none_or(|r| r.departed.is_some())
        {
            return false;
        }
        self.records.remove(&wire_seq);
        self.stats.forgotten_before_departure += 1;
        true
    }

    /// [`Self::forget_undeparted`], returning what the record said it carried,
    /// for a caller that has to tell the content's owner. `None` if the record
    /// is unknown or has departed.
    pub fn take_undeparted(&mut self, wire_seq: u32) -> Option<Transmission> {
        if self
            .records
            .get(&wire_seq)
            .is_none_or(|r| r.departed.is_some())
        {
            return None;
        }
        let rec = self.records.remove(&wire_seq)?;
        self.stats.forgotten_before_departure += 1;
        Some(rec.tx)
    }

    /// Forget one outstanding record, departed or not, with no verdict and no
    /// tombstone: its content is gone (an H.264 fragment trimmed from the
    /// probe cache or superseded by an IDR), so there is nothing to send again
    /// and nothing known about whether it arrived. Not a loss: the estimators
    /// hear nothing. `None` if it is not outstanding.
    pub fn forget_outstanding(&mut self, wire_seq: u32) -> Option<Transmission> {
        let rec = self.records.remove(&wire_seq)?;
        self.leave(&rec);
        self.stats.forgotten_without_verdict += 1;
        Some(rec.tx)
    }

    /// Forget every record that has not departed. The session they were
    /// built for is gone: they are neither delivered nor lost, so no verdict
    /// is issued and no tombstone kept. Returns how many were forgotten.
    /// O(n); runs on a session change only.
    pub fn discard_undeparted(&mut self) -> usize {
        let before = self.records.len();
        self.records.retain(|_, r| r.departed.is_some());
        before - self.records.len()
    }

    /// A probe burst departs from `start` and its queue drains by `end`.
    /// A datagram that departed inside that span is delayed by the burst's own
    /// queue, and on a jittery path it can be overtaken by a later one: the
    /// time rule judges such a datagram lost only after
    /// [`BURST_REORDER_FACTOR`] times the usual threshold. Scoped to the
    /// burst; a datagram outside it, tile or frame, is judged as ever.
    pub fn mark_probe_burst(&mut self, start: Instant, end: Instant) {
        if let Some(last) = self.probe_bursts.back_mut() {
            if start <= last.1 {
                last.0 = last.0.min(start);
                last.1 = last.1.max(end);
                return;
            }
        }
        if self.probe_bursts.len() >= MAX_PROBE_BURSTS {
            self.probe_bursts.pop_front();
        }
        self.probe_bursts.push_back((start, end));
    }

    /// Whether `at` falls inside a marked probe burst.
    pub fn in_probe_burst(&self, at: Instant) -> bool {
        self.probe_bursts.iter().any(|&(s, e)| at >= s && at <= e)
    }

    /// Whether a record is probe material that gets the burst's patience: a
    /// datagram of the H.264 stream (a pad or a fragment; never a tile) that
    /// departed inside a marked burst. Tile datagrams are judged as ever even
    /// when a probe window of the tile path is open.
    fn is_patient(&self, tx: &Transmission, departed: Instant) -> bool {
        !matches!(tx.content, Content::Tile(_)) && self.in_probe_burst(departed)
    }

    /// Whether the verdict on `wire_seq` fell on a datagram that is probe
    /// material departing inside a marked burst (read from its tombstone).
    pub fn declared_lost_in_burst(&self, wire_seq: u32) -> bool {
        self.tombstones
            .get(&wire_seq)
            .is_some_and(|t| t.departed.is_some_and(|d| self.is_patient(&t.tx, d)))
    }

    /// Forget everything about the session that ended: the outstanding
    /// records (neither delivered nor lost to anyone who counts), the
    /// tombstones, the acknowledgement reference, the adapted threshold and
    /// the evidence behind it. A record left behind would be declared lost
    /// against the next connection's goog_cc, and a threshold raised by the old
    /// path's reordering would blunt the new path's. O(n), on a session change
    /// only.
    pub fn reset_session(&mut self) {
        self.records.clear();
        self.order.clear();
        self.departed_order.clear();
        self.tombstones.clear();
        self.tombstone_order.clear();
        self.next_dep_idx = 0;
        self.probe_bursts.clear();
        self.largest_acked = None;
        self.largest_acked_at = None;
        self.largest_acked_arrival = None;
        self.reo_wnd = Duration::ZERO;
        self.reo_recoveries = 0;
        self.reo_recovery_point = 0;
        self.reo_depth = 0;
        self.last_time_threshold = None;
        self.last_reo_raise = None;
        self.last_spurious_ack = None;
        self.parity_departed.clear();
        self.parity_order.clear();
        self.departed_bytes = 0;
        self.k_pkt = K_PKT_INITIAL;
        self.verdicts = 0;
        self.spurious_bits = 0;
    }

    /// Classify a transmission as received. Returns `None` if it is not
    /// outstanding and has no tombstone — already resolved, or the
    /// acknowledgement is unknown to this ledger entirely.
    ///
    /// goog_cc has no retraction, so a loss that has already been reported to
    /// it stays reported. A loss still waiting in the bridge's
    /// `pending_losses` can be withdrawn by the caller when the late
    /// acknowledgement arrives (see `IoBridge::retract_late_acked_losses`);
    /// this ledger only classifies the acknowledgement as `Resolution::Late`,
    /// which also lets the content be released.
    pub fn resolve(&mut self, wire_seq: u32) -> Option<Resolution> {
        self.resolve_at(wire_seq, None)
    }

    /// [`Self::resolve`] with the acknowledgement's arrival time and the
    /// path's `min_rtt`, which the FEC test needs (`None` skips its age test).
    pub fn resolve_at(
        &mut self,
        wire_seq: u32,
        ack: Option<(Instant, Duration)>,
    ) -> Option<Resolution> {
        if let Some(rec) = self.records.remove(&wire_seq) {
            // Probe padding is acknowledged (it is a measurement) but never
            // moves the reference real datagrams are judged against: a pad
            // that overtakes a real datagram on a jittery path would make the
            // real one look like a hole.
            let is_pad = matches!(rec.tx.content, Content::Pad(_));
            if let (Some(idx), Some(at), false) = (rec.tx.dep_idx, rec.departed, is_pad) {
                self.advance_largest_acked(idx, at, ack.map(|a| a.0));
            }
            self.leave(&rec);
            return Some(Resolution::Live(rec.tx));
        }
        if let Some(t) = self.tombstones.remove(&wire_seq) {
            self.stats.acked_after_declared_lost += 1;
            self.stats.spurious_losses += 1;
            let tx = t.tx;
            // An acknowledgement of a departure newer than any acknowledged so
            // far moves the reference the thresholds are judged against.
            let is_pad = matches!(tx.content, Content::Pad(_));
            // How far the newest acknowledged departure was past this one when
            // the verdict was issued, in departures and in departure time.
            let (disp, extent) = match (
                t.largest_idx_at_verdict,
                tx.dep_idx,
                t.largest_at_at_verdict,
                t.departed,
            ) {
                (Some(l), Some(i), Some(la), Some(d)) => {
                    (l.saturating_sub(i), la.saturating_duration_since(d))
                }
                _ => (0, Duration::ZERO),
            };
            // And how long after the acknowledgement that overtook it this one
            // came (includes the client's acknowledgement batching).
            let lag = match (ack, t.largest_arrival_at_verdict) {
                (Some((now, _)), Some(a)) => now.saturating_duration_since(a),
                _ => Duration::ZERO,
            };
            // No acknowledgement advanced the reference since the verdict, and
            // the late one came a time threshold after the one that overtook
            // it: a stalled ack path, not reordering.
            let stalled = self.largest_acked == t.largest_idx_at_verdict
                && self
                    .last_time_threshold
                    .is_some_and(|thr| !thr.is_zero() && lag >= thr);
            if let (Some(idx), Some(at), false) = (tx.dep_idx, t.departed, is_pad) {
                self.advance_largest_acked(idx, at, ack.map(|a| a.0));
            }
            let fec_possible = self.fec_possible(wire_seq, ack);
            if fec_possible {
                self.stats.late_acks_fec_possible += 1;
            } else {
                self.stats.spurious_disp_max = self.stats.spurious_disp_max.max(disp);
                self.stats.spurious_gap_us_max = self
                    .stats
                    .spurious_gap_us_max
                    .max(extent.as_micros() as u64);
                // A zero `min_rtt` is none known: nothing to size a step by.
                let min_rtt = ack.map(|a| a.1);
                let known = min_rtt.is_none_or(|m| !m.is_zero());
                if !is_pad && t.reason != LossReason::Pto && known {
                    if stalled {
                        self.stats.reo_stall_skips += 1;
                    } else {
                        if t.reason == LossReason::Packet {
                            self.stats.reo_learned_packet += 1;
                        } else {
                            self.stats.reo_learned_time += 1;
                        }
                        self.learn_reordering(ack.map(|a| a.0), min_rtt, extent.max(lag), disp);
                    }
                }
                if let Some(n) = t.packet_verdict_no {
                    self.note_spurious(n);
                }
            }
            return Some(Resolution::Late { tx, fec_possible });
        }
        self.stats.unknown_acks += 1;
        None
    }

    /// Declare lost every outstanding transmission that the acknowledgements
    /// received so far put out of contention (RFC 9002 section 6.1), oldest
    /// first. `time_threshold` is `9/8 x max(srtt, latest_rtt)`.
    ///
    /// A transmission is lost when it departed before the newest acknowledged
    /// departure **and** either
    /// - `K_PKT` or more departures separate them in departure index
    ///   ([`Transmission::dep_idx`], one per datagram), or
    /// - it left at least `time_threshold` ago (`loss_time` in RFC 9002: a hole
    ///   followed by fewer than `K_PKT` departures is still found, by the clock,
    ///   once something later has been acknowledged to make it a hole).
    ///
    /// Both need an *acknowledged* later departure to exist. A bottleneck that
    /// queues but neither drops nor reorders delivers acknowledgements in
    /// departure order, so nothing outstanding departed before the newest
    /// acknowledged one, and a growing queue produces no verdict however deep it
    /// gets. [`Self::next_loss_time`] says when the time rule next fires.
    ///
    /// Walks `departed_order` from the front and stops at the first record that
    /// is not lost: a later departure has a larger index and a later time, so
    /// it cannot be lost if this one is not. Resolved entries it passes are
    /// dropped. Amortised O(1) per transmission.
    pub fn detect_losses_at(
        &mut self,
        now: Instant,
        time_threshold: std::time::Duration,
    ) -> Vec<(u32, Transmission, LossReason)> {
        let mut lost = Vec::new();
        self.last_time_threshold = Some(time_threshold);
        let Some(largest_idx) = self.largest_acked else {
            return lost;
        };
        let largest_at = self.largest_acked_at;
        // At use, not when learned: `min_rtt` is a lifetime minimum, so a
        // window learned on a slow path must not outlive a move to a fast one.
        let reo = self.reo_wnd.min(time_threshold / 2);
        let quiet = self
            .largest_acked_arrival
            .is_none_or(|a| now.saturating_duration_since(a) >= reo);
        // Walk the departure order. A record that is not yet lost normally ends
        // the walk (everything behind it departed later, so cannot be lost if
        // it is not). A *patient* one (probe material that departed inside a
        // burst, judged by a longer threshold) must not hold up verdicts on
        // the ordinary records behind it: step over it, a bounded number of
        // them. A record declared lost behind the front stays in the order
        // until it reaches the front, where the stale entry is dropped.
        let mut i = 0usize;
        let mut stepped = 0usize;
        while i < self.departed_order.len() {
            let ws = self.departed_order[i];
            let (idx, at, patient) = match self.records.get(&ws) {
                // Already resolved, or the record was replaced by a newer one
                // under the same `wire_seq` that has not departed yet.
                None | Some(Record { departed: None, .. }) => {
                    if i == 0 {
                        self.departed_order.pop_front();
                    } else {
                        i += 1;
                    }
                    continue;
                }
                Some(Record {
                    departed: Some(at),
                    tx,
                    ..
                }) => (
                    tx.dep_idx.unwrap_or(u64::MAX),
                    *at,
                    self.is_patient(tx, *at),
                ),
            };
            if idx >= largest_idx {
                break;
            }
            let threshold = if patient {
                time_threshold * BURST_REORDER_FACTOR
            } else {
                time_threshold
            };
            // RFC 8985: the newest acknowledged departure must have left
            // `reo_wnd` after this one. Inside the window (one burst shares a
            // stamp) the count rule stays, as deep as displacement was seen.
            let inside = !reo.is_zero()
                && largest_at.is_none_or(|la| la.saturating_duration_since(at) < reo);
            // The count rule at the learned depth, or at the ordinary one once
            // the stream of later departures has been quiet for `reo_wnd`: a
            // straggler of a reordered burst has arrived by then.
            let branch = if !inside {
                (idx + self.k_pkt <= largest_idx).then_some(ReoBranch::Open)
            } else if idx.saturating_add(self.k_pkt.max(self.reo_depth)) <= largest_idx {
                Some(ReoBranch::Depth)
            } else if idx + self.k_pkt <= largest_idx && quiet {
                Some(ReoBranch::Quiet)
            } else {
                None
            };
            let reason = if let Some(b) = branch {
                match b {
                    ReoBranch::Depth => self.stats.reo_judged_depth += 1,
                    ReoBranch::Quiet => self.stats.reo_judged_quiet += 1,
                    ReoBranch::Open => {}
                }
                #[cfg(any(test, feature = "test-loss-injection"))]
                self.verdict_branches.push((ws, b));
                LossReason::Packet
            } else if at + threshold + reo <= now {
                LossReason::Time
            } else if patient && stepped < PATIENT_SCAN_LIMIT {
                stepped += 1;
                i += 1;
                continue;
            } else {
                break;
            };
            if i == 0 {
                self.departed_order.pop_front();
            } else {
                i += 1;
            }
            self.declare_lost(ws, reason, &mut lost);
        }
        if !self.reo_wnd.is_zero() {
            // A loss episode on a path that has been quiet of late
            // acknowledgements is one loss recovery (RFC 8985 section 6.2).
            // Verdicts on departures before the previous episode's recovery
            // point belong to that recovery: one burst is one episode however
            // many event-loop turns it takes to judge.
            let past = lost
                .iter()
                .filter_map(|(_, tx, _)| tx.dep_idx)
                .any(|i| i >= self.reo_recovery_point);
            let quiet = self.last_spurious_ack.is_none_or(|t| {
                now.saturating_duration_since(t) >= REO_QUIET_SPAN.max(time_threshold * 2)
            });
            if past && quiet {
                self.reo_recoveries += 1;
                self.reo_recovery_point = self.next_dep_idx;
                if self.reo_recoveries >= REO_PERSIST {
                    self.reo_wnd = Duration::ZERO;
                    self.reo_depth = 0;
                    self.reo_recoveries = 0;
                    self.last_reo_raise = None;
                }
            }
        }
        self.compact_if_sparse();
        lost
    }

    /// [`Self::detect_losses_at`] with the clock read here, for the tests that
    /// are not about the time rule.
    #[cfg(test)]
    pub(crate) fn detect_losses(
        &mut self,
        time_threshold: std::time::Duration,
    ) -> Vec<(u32, Transmission, LossReason)> {
        self.detect_losses_at(Instant::now(), time_threshold)
    }

    /// When the time rule next declares something lost: the oldest outstanding
    /// departure, if an acknowledged one is already later than it (so it is a
    /// hole), plus `time_threshold`. `None` when there is no such record. The
    /// event loop wakes for it, so a hole followed by fewer than `K_PKT`
    /// departures is found at about 9/8 of a round trip and not at the probe
    /// timeout. Drops resolved entries at the front to find out.
    pub fn next_loss_time(&mut self, time_threshold: std::time::Duration) -> Option<Instant> {
        self.last_time_threshold = Some(time_threshold);
        let largest_idx = self.largest_acked?;
        let reo = self.reo_wnd.min(time_threshold / 2);
        let mut best: Option<Instant> = None;
        let mut i = 0usize;
        let mut stepped = 0usize;
        while i < self.departed_order.len() {
            let ws = self.departed_order[i];
            match self.records.get(&ws) {
                Some(Record {
                    departed: Some(at),
                    tx,
                    ..
                }) => {
                    let (idx, at, patient) = (
                        tx.dep_idx.unwrap_or(u64::MAX),
                        *at,
                        self.is_patient(tx, *at),
                    );
                    if idx >= largest_idx {
                        break;
                    }
                    let threshold = if patient {
                        time_threshold * BURST_REORDER_FACTOR
                    } else {
                        time_threshold
                    };
                    // The count rule's `reo_wnd` hold ends at the time rule's
                    // deadline at the latest, which carries `reo_wnd` too.
                    let mut due = at + threshold + reo;
                    // Held back by the window but past the count: the stream
                    // of later departures going quiet ends the wait.
                    if idx + self.k_pkt <= largest_idx {
                        if let (false, Some(a)) = (reo.is_zero(), self.largest_acked_arrival) {
                            due = due.min(a + reo);
                        }
                    }
                    best = Some(best.map_or(due, |b| b.min(due)));
                    // An ordinary record ends the walk; a patient one is
                    // stepped over, as `detect_losses_at` does.
                    if !patient || stepped >= PATIENT_SCAN_LIMIT {
                        break;
                    }
                    stepped += 1;
                    i += 1;
                }
                _ => {
                    if i == 0 {
                        self.departed_order.pop_front();
                    } else {
                        i += 1;
                    }
                }
            }
        }
        best
    }

    /// The newest outstanding transmissions that have departed, newest first:
    /// up to `n` of them, looking at no more than `max_scan` outstanding
    /// records from the back. Resolved entries at the back are dropped as they
    /// are met.
    ///
    /// What the probe timeout retransmits. A bounded scan, so an unlucky run
    /// of resolved entries behind the tail cannot make one probe expensive.
    pub fn newest_outstanding(&mut self, n: usize, max_scan: usize) -> Vec<(u32, Transmission)> {
        self.scan_outstanding(&mut 0, n, max_scan)
    }

    /// [`Self::newest_outstanding`] from a positional `cursor`: the number of
    /// entries of the departure order, counted from the back, already
    /// examined. It starts at 0 and is advanced past everything this call
    /// examined, so a caller that removes records between calls (the probe
    /// declaring dead content lost) never reads an entry twice. Positions
    /// count from the back and removal is lazy, so they stay valid as the
    /// caller removes records; only the first call (cursor 0) trims the back.
    ///
    /// `max_scan` bounds the *outstanding* records examined. Resolved entries
    /// have their own budget, [`SCAN_DEAD_BUDGET`], so a long dead run neither
    /// hides the live records behind it nor costs unboundedly.
    pub fn scan_outstanding(
        &mut self,
        cursor: &mut usize,
        n: usize,
        max_scan: usize,
    ) -> Vec<(u32, Transmission)> {
        if *cursor == 0 {
            // Resolved entries at the very back are dead weight for every
            // later probe: drop them.
            while let Some(back) = self.departed_order.back().copied() {
                match self.records.get(&back) {
                    Some(Record {
                        departed: Some(_), ..
                    }) => break,
                    _ => {
                        self.departed_order.pop_back();
                    }
                }
            }
        }
        let mut out = Vec::new();
        let (mut live_seen, mut dead_seen) = (0usize, 0usize);
        for ws in self.departed_order.iter().rev().skip(*cursor) {
            if out.len() >= n || live_seen >= max_scan || dead_seen >= SCAN_DEAD_BUDGET {
                break;
            }
            *cursor += 1;
            match self.records.get(ws) {
                Some(Record {
                    departed: Some(_),
                    tx,
                    ..
                }) => {
                    live_seen += 1;
                    out.push((*ws, *tx));
                }
                _ => dead_seen += 1,
            }
        }
        out
    }

    /// When the newest outstanding transmission left quinn's FIFO, or `None`
    /// when nothing departed is outstanding. Drops resolved entries at the back
    /// to find out.
    pub fn latest_outstanding_departure(&mut self) -> Option<Instant> {
        while let Some(back) = self.departed_order.back().copied() {
            match self.records.get(&back) {
                Some(Record {
                    departed: Some(at), ..
                }) => return Some(*at),
                _ => {
                    self.departed_order.pop_back();
                }
            }
        }
        None
    }

    /// Declare one outstanding transmission lost by the probe timeout: the
    /// timer found it with nothing left to retransmit. `None` if it is not
    /// outstanding.
    pub fn declare_lost_by_pto(&mut self, wire_seq: u32) -> Option<Transmission> {
        let mut lost = Vec::new();
        self.declare_lost(wire_seq, LossReason::Pto, &mut lost);
        lost.pop().map(|(_, tx, _)| tx)
    }

    /// Verdict number `verdict_no` was contradicted by an acknowledgement. If
    /// it is still inside the last [`ADAPT_WINDOW`] verdicts, mark it; when more
    /// than 10 % of the window is marked, raise `K_PKT` by one and start the
    /// evidence over (RACK's reorder-window growth, RFC 8985 section 6.2).
    /// Bounded at [`K_PKT_MAX`]; the same session's threshold only goes up. O(1):
    /// a bit set and a popcount.
    fn note_spurious(&mut self, verdict_no: u64) {
        if self.verdicts - verdict_no >= ADAPT_WINDOW {
            return;
        }
        self.spurious_bits |= 1 << (verdict_no % ADAPT_WINDOW);
        let in_window = self.verdicts.min(ADAPT_WINDOW);
        let marked = u64::from(self.spurious_bits.count_ones());
        if in_window >= ADAPT_MIN_VERDICTS && marked * 100 > in_window * ADAPT_SPURIOUS_PERCENT {
            if !self.freeze_k_pkt && self.k_pkt < K_PKT_MAX {
                self.k_pkt += 1;
                self.stats.k_pkt_raises += 1;
            }
            self.spurious_bits = 0;
        }
    }

    /// Issue the verdict for `wire_seq`: remove its record, keep a tombstone
    /// so a late acknowledgement can still release the content, and hand it
    /// to the caller. The one place a loss is declared.
    fn declare_lost(
        &mut self,
        wire_seq: u32,
        reason: LossReason,
        lost: &mut Vec<(u32, Transmission, LossReason)>,
    ) {
        let Some(rec) = self.records.remove(&wire_seq) else {
            return;
        };
        self.leave(&rec);
        if rec.departed.is_none() {
            self.stats.expired_before_departure += 1;
        }
        match reason {
            LossReason::Packet => self.stats.lost_packet_threshold += 1,
            LossReason::Time => self.stats.lost_time_threshold += 1,
            LossReason::Pto => self.stats.lost_pto += 1,
        }
        if crate::transport::reliable_emitter::rto_probe_enabled() {
            eprintln!("RTOPROBE lost_ws ws={} reason={:?}", wire_seq, reason);
        }
        let tx = rec.tx;
        // Only packet-threshold verdicts adapt the packet threshold: a time or
        // probe verdict says nothing about reordering depth.
        let packet_verdict_no = (reason == LossReason::Packet).then(|| {
            self.verdicts += 1;
            // This verdict's bit starts clear: it replaces the one 64 ago.
            self.spurious_bits &= !(1 << (self.verdicts % ADAPT_WINDOW));
            self.verdicts
        });
        self.tombstones.insert(
            wire_seq,
            Tombstone {
                tx,
                packet_verdict_no,
                reason,
                largest_idx_at_verdict: self.largest_acked,
                largest_at_at_verdict: self.largest_acked_at,
                largest_arrival_at_verdict: self.largest_acked_arrival,
                departed: rec.departed,
                rate: rec.rate,
            },
        );
        self.tombstone_order.push_back(wire_seq);
        while self.tombstones.len() > self.capacity {
            if let Some(oldest) = self.tombstone_order.pop_front() {
                if self.tombstones.remove(&oldest).is_some() {
                    self.stats.tombstone_evictions += 1;
                }
            } else {
                break;
            }
        }
        lost.push((wire_seq, tx, reason));
    }

    /// The two order queues skip resolved entries lazily, which is amortised
    /// O(1) while the front keeps moving. A front that does not move (a
    /// record that never departs, a young one at the head of the walk) would
    /// let resolved entries pile up behind it, so rebuild when they outnumber
    /// the live ones by a wide margin.
    fn compact_if_sparse(&mut self) {
        let live = self.records.len();
        let slack = 2 * self.capacity + 64;
        if self.order.len() > live + slack {
            let records = &self.records;
            self.order.retain(|w| records.contains_key(w));
        }
        if self.departed_order.len() > live + slack {
            let records = &self.records;
            self.departed_order
                .retain(|w| records.get(w).is_some_and(|r| r.departed.is_some()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::reliable_emitter::EmitKey;
    use std::time::Duration;

    const MS: Duration = Duration::from_millis(1);
    /// A time threshold nothing in these tests reaches unless it means to.
    const FAR: Duration = Duration::from_secs(3600);

    fn tx(n: u32) -> Transmission {
        Transmission {
            emit_us: n as u64 * 1000,
            wire_bytes: 1200,
            content: Content::Tile(EmitKey::new(n, 0, 0, 0)),
            depart_us: None,
            dep_idx: None,
        }
    }

    /// Record `ws` and put it on the wire at `at` (all of one "turn" share the
    /// same instant, as the departure tracker's stamps do).
    fn send(l: &mut TransmissionLedger, ws: u32, at: Instant) {
        l.record(ws, at, tx(ws));
        assert!(l.mark_departed(ws, at, ws as u64));
    }

    fn lost_ws(v: &[(u32, Transmission, LossReason)]) -> Vec<u32> {
        v.iter().map(|(w, _, _)| *w).collect()
    }

    /// Departure index is per datagram, in departure order, even when the
    /// stamps are equal.
    #[test]
    fn departure_index_is_per_datagram_and_ignores_equal_stamps() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in [10u32, 11, 12] {
            send(&mut l, ws, t0);
        }
        let idx = |l: &mut TransmissionLedger, ws| match l.resolve(ws) {
            Some(Resolution::Live(t)) => t.dep_idx,
            other => panic!("{other:?}"),
        };
        assert_eq!(idx(&mut l, 11), Some(1));
        assert_eq!(idx(&mut l, 10), Some(0));
        assert_eq!(idx(&mut l, 12), Some(2));
    }

    #[test]
    fn a_resolved_transmission_is_classified_once_and_never_lost() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        send(&mut l, 7, t0);
        send(&mut l, 8, t0);
        assert!(matches!(l.resolve(7), Some(Resolution::Live(_))));
        assert_eq!(l.resolve(7), None, "a second resolve finds nothing");
        assert!(l.detect_losses(FAR).is_empty());
    }

    /// A pad is acknowledged and classified once like any datagram, but its
    /// acknowledgement is not the reference real datagrams are judged against:
    /// a pad that overtakes a real datagram would otherwise make it a hole.
    #[test]
    fn a_pad_acknowledgement_does_not_move_the_reference_real_datagrams_are_judged_by() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        send(&mut l, 1, t0); // real, will be reordered behind the pad
                             // The pad departs after it.
        let mut pad = tx(2);
        pad.content = Content::Pad(2);
        l.record(2, t0, pad);
        assert!(l.mark_departed(2, t0, 2));
        assert!(
            matches!(l.resolve(2), Some(Resolution::Live(_))),
            "classified once"
        );
        assert!(
            l.detect_losses_at(t0 + 10_000 * MS, MS).is_empty(),
            "the pad's acknowledgement is no reference: nothing is a hole yet"
        );
        // A real one after it, acknowledged: now 1 is a hole.
        send(&mut l, 3, t0);
        assert!(matches!(l.resolve(3), Some(Resolution::Live(_))));
        assert_eq!(lost_ws(&l.detect_losses_at(t0 + 10_000 * MS, MS)), vec![1]);
    }

    fn send_content(l: &mut TransmissionLedger, ws: u32, at: Instant, content: Content) {
        let mut t = tx(ws);
        t.content = content;
        l.record(ws, at, t);
        assert!(l.mark_departed(ws, at, ws as u64));
    }

    fn h264(n: u32) -> Content {
        Content::H264Frag {
            frame_seq: n,
            frag_idx: 0,
        }
    }

    /// Inside a marked probe burst the time rule waits three times as long for
    /// an H.264 datagram; a tile datagram gets no patience however open the
    /// burst (a probe window of the tile path marks one too); outside the
    /// burst nothing changes.
    #[test]
    fn the_time_rule_is_patient_only_with_h264_datagrams_that_departed_in_a_probe_burst() {
        let thr = 100 * MS;
        // (in_burst, content is a tile) -> is it declared lost at 150 ms?
        for (in_burst, tile, lost_at_150) in [
            (false, false, true),
            (true, false, false),
            (true, true, true),
            (false, true, true),
        ] {
            let t0 = Instant::now();
            let mut l = TransmissionLedger::new(64);
            if in_burst {
                l.mark_probe_burst(t0, t0 + 50 * MS);
            }
            let c = if tile {
                Content::Tile(EmitKey::new(1, 0, 0, 0))
            } else {
                h264(1)
            };
            send_content(&mut l, 1, t0, c);
            send(&mut l, 2, t0);
            assert!(matches!(l.resolve(2), Some(Resolution::Live(_))));
            let lost = l.detect_losses_at(t0 + 150 * MS, thr);
            assert_eq!(
                lost.len(),
                usize::from(lost_at_150),
                "in_burst={in_burst} tile={tile}"
            );
            if in_burst && !tile {
                assert_eq!(l.next_loss_time(thr), Some(t0 + 300 * MS));
                assert_eq!(l.detect_losses_at(t0 + 300 * MS, thr).len(), 1);
                assert!(
                    l.declared_lost_in_burst(1),
                    "and the verdict says it fell in the burst"
                );
            }
        }
    }

    /// A patient record at the head of the departure order does not hold up
    /// verdicts on ordinary records behind it.
    #[test]
    fn a_patient_record_does_not_block_verdicts_behind_it() {
        let thr = 100 * MS;
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.mark_probe_burst(t0, t0 + 10 * MS);
        send_content(&mut l, 1, t0, h264(1)); // patient: waits 300 ms
        send_content(
            &mut l,
            2,
            t0 + 20 * MS,
            Content::Tile(EmitKey::new(2, 0, 0, 0)),
        ); // ordinary, outside the burst
        send(&mut l, 3, t0 + 20 * MS);
        assert!(matches!(l.resolve(3), Some(Resolution::Live(_))));
        // 150 ms: 2 is a hole past its threshold; 1 is not yet (patient).
        let lost = l.detect_losses_at(t0 + 130 * MS, thr);
        assert_eq!(
            lost_ws(&lost),
            vec![2],
            "the ordinary record behind the patient one"
        );
        assert_eq!(
            l.next_loss_time(thr),
            Some(t0 + 300 * MS),
            "the patient one is still owed its wait"
        );
        let lost = l.detect_losses_at(t0 + 300 * MS, thr);
        assert_eq!(lost_ws(&lost), vec![1]);
    }

    /// With nothing acknowledged there is nothing to be lost *relative to*: no
    /// verdict however old the transmissions are.
    #[test]
    fn no_verdict_without_an_acknowledgement() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..20 {
            send(&mut l, ws, t0 + ws * 100 * MS);
        }
        assert!(l.detect_losses(MS).is_empty());
        assert_eq!(l.len(), 20);
    }

    /// Packet threshold: three departures after it acknowledged.
    #[test]
    fn three_later_departures_acknowledged_declare_a_transmission_lost() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..5 {
            send(&mut l, ws, t0);
        }
        // 4 is acknowledged: 0 and 1 are three or more places behind it.
        assert!(l.resolve(4).is_some());
        let lost = l.detect_losses(FAR);
        assert_eq!(lost_ws(&lost), vec![0, 1]);
        assert!(lost.iter().all(|(_, _, r)| *r == LossReason::Packet));
        // 2 and 3 are inside the threshold, and are not lost by the clock.
        assert_eq!(l.len(), 2);
        assert_eq!(l.stats().lost_packet_threshold, 2);
        assert!(l.detect_losses(FAR).is_empty(), "verdicts are issued once");
    }

    /// Time threshold: a hole followed by fewer than `K_PKT` departures is
    /// declared once it is `time_threshold` old, and not before.
    #[test]
    fn a_hole_is_declared_at_the_time_threshold_when_too_few_departures_follow() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..3u32 {
            send(&mut l, ws, t0 + ws * MS);
        }
        // Two later departures acknowledged: 0 is a hole, but two places back.
        assert!(l.resolve(1).is_some());
        assert!(l.resolve(2).is_some());
        let thr = 100 * MS;
        assert_eq!(l.next_loss_time(thr), Some(t0 + thr));
        assert!(l.detect_losses_at(t0 + 99 * MS, thr).is_empty());
        let lost = l.detect_losses_at(t0 + thr, thr);
        assert_eq!(lost_ws(&lost), vec![0]);
        assert_eq!(lost[0].2, LossReason::Time);
        assert_eq!(l.next_loss_time(thr), None);
    }

    /// With no acknowledged departure later than it, nothing is a hole and the
    /// clock alone declares nothing: a queue that is only slow.
    #[test]
    fn the_clock_alone_declares_nothing() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..3u32 {
            send(&mut l, ws, t0);
        }
        assert_eq!(l.next_loss_time(MS), None);
        assert!(l.detect_losses_at(t0 + 3600 * 1000 * MS, MS).is_empty());
        // Only the oldest being acknowledged leaves nothing behind it either.
        assert!(l.resolve(0).is_some());
        assert_eq!(l.next_loss_time(MS), None);
    }

    /// F4's structural claim: acknowledgements that arrive in departure order
    /// produce no verdict, at any queue depth and any pace. (The clock is not
    /// an input at all.)
    #[test]
    fn acknowledgements_in_departure_order_never_produce_a_verdict() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(4096);
        // 1000 datagrams departing 1 ms apart, acknowledged in order a growing
        // time later (up to 5 s of queue), a few at a time.
        for ws in 0..1000u32 {
            send(&mut l, ws, t0 + ws * MS);
        }
        let mut verdicts = 0;
        for ws in 0..1000u32 {
            assert!(l.resolve(ws).is_some());
            verdicts += l.detect_losses(MS).len();
        }
        assert_eq!(verdicts, 0);
        assert_eq!(
            l.stats().lost_packet_threshold + l.stats().lost_time_threshold,
            0
        );
    }

    /// And a real loss in the same stream is found as soon as three later
    /// departures are acknowledged.
    #[test]
    fn a_hole_in_an_in_order_stream_is_found_after_three_more() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..10u32 {
            send(&mut l, ws, t0 + ws * MS);
        }
        let mut found_after = None;
        for ws in (0..10u32).filter(|w| *w != 4) {
            assert!(l.resolve(ws).is_some());
            if !l.detect_losses(FAR).is_empty() && found_after.is_none() {
                found_after = Some(ws);
            }
        }
        assert_eq!(found_after, Some(7), "4 + 3 departures");
    }

    /// A late acknowledgement is visible, releases the content and is counted
    /// as a spurious verdict.
    #[test]
    fn an_acknowledgement_after_a_verdict_is_spurious_and_releases_content() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..5 {
            send(&mut l, ws, t0);
        }
        assert!(l.resolve(4).is_some());
        assert_eq!(l.detect_losses(FAR).len(), 2);
        assert!(
            matches!(l.resolve(0), Some(Resolution::Late { tx: t, .. }) if t.content == Content::Tile(EmitKey::new(0, 0, 0, 0)))
        );
        assert_eq!(l.stats().acked_after_declared_lost, 1);
        assert_eq!(l.stats().spurious_losses, 1);
        assert_eq!(l.stats().unknown_acks, 0);
        assert!(l.resolve(0).is_none(), "a tombstone resolves once");
        assert_eq!(l.stats().spurious_losses, 1, "and is not counted twice");
    }

    /// 70 sent, the last acknowledged: all but the last `K_PKT` are lost. Then
    /// the newest `spurious` of the verdicts are contradicted at once, so each
    /// late acknowledgement is of a transmission whose group's parity has not
    /// left (so it cannot be an FEC recovery).
    fn window(l: &mut TransmissionLedger, t0: Instant, base: u32, spurious: usize) {
        for ws in base..base + 70 {
            send(l, ws, t0);
        }
        assert!(l.resolve(base + 69).is_some());
        let lost = l.detect_losses(FAR);
        // Everything but the last K of these 70 (and whatever the previous
        // window left behind) is lost.
        assert!(lost.len() as u64 >= 70 - l.k_pkt(), "{}", lost.len());
        let n = spurious.min(19 - l.k_pkt() as usize);
        for (ws, _, _) in lost.iter().rev().take(n) {
            assert!(matches!(
                l.resolve(*ws),
                Some(Resolution::Late {
                    fec_possible: false,
                    ..
                })
            ));
        }
    }

    /// One contradicted verdict in sixty-odd is 1.5 %: no change. The
    /// threshold goes up one step each time more than 10 % of the last 64
    /// packet verdicts have been contradicted, and stays inside [3, 8].
    #[test]
    fn the_packet_threshold_adapts_to_reordering_and_is_bounded() {
        let t0 = Instant::now();
        let mut quiet = TransmissionLedger::new(8192);
        window(&mut quiet, t0, 0, 1);
        assert_eq!(quiet.k_pkt(), 3);

        let mut l = TransmissionLedger::new(8192);
        // This is about the count threshold alone: the departure-gap window
        // would hold these single-stamp windows back.
        l.freeze_reo_wnd();
        let mut seen = vec![l.k_pkt()];
        let mut base = 0;
        for _ in 0..12 {
            window(&mut l, t0, base, 15);
            base += 70;
            seen.push(l.k_pkt());
        }
        assert!(
            seen.windows(2).all(|w| w[1] >= w[0]),
            "never goes down: {seen:?}"
        );
        assert_eq!(l.k_pkt(), K_PKT_MAX, "bounded at K_PKT_MAX: {seen:?}");
        assert!(
            seen[1] > seen[0] && seen[1] <= 6,
            "a step per ~7 contradicted verdicts: {seen:?}"
        );
        assert!(l.stats().k_pkt_raises >= 5);
    }

    const RTT: Duration = Duration::from_millis(40);
    const KEEP: Duration = Duration::from_millis(600);

    /// Send `first..first + 10` as one group, then its parity at `parity_at`.
    fn send_group(l: &mut TransmissionLedger, first: u32, t0: Instant, parity_at: Instant) {
        for ws in first..first + 10 {
            send(l, ws, t0);
        }
        l.note_parity_departed(first, parity_at, KEEP);
    }

    fn late(l: &mut TransmissionLedger, ws: u32, at: Instant) -> bool {
        match l.resolve_at(ws, Some((at, RTT))) {
            Some(Resolution::Late { fec_possible, .. }) => fec_possible,
            other => panic!("{other:?}"),
        }
    }

    /// The client acks a datagram it rebuilt from parity under its original
    /// `wire_seq`, which can only be a round trip after the group's parity
    /// left, and only when it was the one member missing. That is a loss that
    /// happened, so it neither retracts nor adapts anything.
    #[test]
    fn a_loss_recovered_by_parity_does_not_raise_the_packet_threshold() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8192);
        // 12 windows of 7 groups: every member acked live but one per group
        // (position 3), the last acked, the holes declared lost by the
        // threshold, then acked late a round trip after their parity.
        let mut base = 0u32;
        for _ in 0..12 {
            for g in 0..7 {
                send_group(&mut l, base + g * 10, t0, t0);
            }
            for ws in base..base + 70 {
                if ws % 10 != 3 {
                    assert!(l.resolve(ws).is_some());
                }
            }
            let lost = l.detect_losses(FAR);
            assert!(lost.len() >= 6, "{}", lost.len());
            for (ws, _, _) in lost.iter() {
                assert!(late(&mut l, *ws, t0 + RTT), "ws {ws}");
            }
            base += 70;
        }
        assert_eq!(l.k_pkt(), 3, "parity recoveries are not reordering");
        assert_eq!(l.stats().k_pkt_raises, 0);
        assert!(l.stats().late_acks_fec_possible >= 12 * 6);
        // They are still counted as late acknowledgements.
        assert_eq!(l.stats().spurious_losses, l.stats().late_acks_fec_possible);
    }

    /// The age test, at every position in the group and on both sides of its
    /// boundary: the acknowledgement of a rebuilt datagram comes at least
    /// `min_rtt` after the parity left, whichever member it is. The group's
    /// parity leaves 10 to 19 departures after member p at a steady rate, a
    /// spread no count of departures can hold a line through.
    #[test]
    fn a_late_ack_is_a_possible_recovery_only_a_round_trip_after_the_parity() {
        for p in [0u32, 5, 8, 9] {
            let t0 = Instant::now();
            let mut l = TransmissionLedger::new(8192);
            send_group(&mut l, 0, t0, t0);
            for ws in (0..10).filter(|w| *w != p) {
                assert!(l.resolve(ws).is_some());
            }
            let _ = l.declare_lost_by_pto(p);
            let early = late(&mut l, p, t0 + RTT - Duration::from_micros(1));
            assert!(!early, "member {p}: a microsecond short of the round trip");
            // A tombstone resolves once; do the other side on a fresh ledger.
            let mut l = TransmissionLedger::new(8192);
            send_group(&mut l, 0, t0, t0);
            for ws in (0..10).filter(|w| *w != p) {
                assert!(l.resolve(ws).is_some());
            }
            let _ = l.declare_lost_by_pto(p);
            assert!(
                late(&mut l, p, t0 + RTT),
                "member {p}: exactly a round trip"
            );
        }
    }

    /// Parity rebuilds one member. If another is outstanding or lost, the late
    /// acknowledgement is a premature verdict (reordering), whatever the
    /// timing.
    #[test]
    fn a_late_ack_beside_another_missing_member_is_not_a_recovery() {
        let t0 = Instant::now();
        let after = t0 + RTT * 4;
        for other_lost in [false, true] {
            let mut l = TransmissionLedger::new(8192);
            send_group(&mut l, 0, t0, t0);
            for ws in 0..10 {
                if ws != 3 && ws != 6 {
                    assert!(l.resolve(ws).is_some());
                }
            }
            let _ = l.declare_lost_by_pto(3);
            if other_lost {
                let _ = l.declare_lost_by_pto(6);
            }
            assert!(!late(&mut l, 3, after), "other_lost={other_lost}");
        }
    }

    /// The memory of when parity left is bounded by time, not by a count of
    /// groups: at 200 Mbit/s a thousand groups leave in 5 ms, and a recovery on
    /// an 80 ms path is acknowledged 80 ms after its parity, thousands of groups
    /// later. And an entry older than `retain` is forgotten.
    #[test]
    fn the_parity_memory_is_a_span_of_time_not_a_number_of_groups() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(1 << 16);
        let rtt = Duration::from_millis(80);
        // Group 0 is the one under test: nine members acked, one lost.
        for ws in 0..10u32 {
            send(&mut l, ws, t0);
        }
        l.note_parity_departed(0, t0, KEEP);
        for ws in 1..10 {
            assert!(l.resolve(ws).is_some());
        }
        let _ = l.declare_lost_by_pto(0);
        // 15,000 further groups' parity in the next 75 ms (5 us apart).
        for g in 1..15_000u32 {
            l.note_parity_departed(g * 10, t0 + Duration::from_micros(g as u64 * 5), KEEP);
        }
        let late_ack = |l: &mut TransmissionLedger| match l.resolve_at(0, Some((t0 + rtt, rtt))) {
            Some(Resolution::Late { fec_possible, .. }) => fec_possible,
            other => panic!("{other:?}"),
        };
        assert!(late_ack(&mut l), "15,000 groups later, still remembered");

        // And forgotten past the retention.
        let mut l = TransmissionLedger::new(1 << 16);
        for ws in 0..10u32 {
            send(&mut l, ws, t0);
        }
        l.note_parity_departed(0, t0, KEEP);
        for ws in 1..10 {
            assert!(l.resolve(ws).is_some());
        }
        let _ = l.declare_lost_by_pto(0);
        l.note_parity_departed(10, t0 + KEEP + Duration::from_millis(1), KEEP);
        assert!(!late_ack(&mut l), "older than the retention");
    }

    /// The end-of-stream flush sends a frame's last groups' parity a few
    /// milliseconds after their members, far fewer than 20 departures later.
    /// The recovery is still a round trip after the parity left.
    #[test]
    fn a_flushed_group_is_judged_by_its_parity_not_by_a_departure_count() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8192);
        // 10 members, then 5 ms of quiet, then the flushed parity.
        send_group(&mut l, 0, t0, t0 + Duration::from_millis(5));
        for ws in 1..10 {
            assert!(l.resolve(ws).is_some());
        }
        let _ = l.declare_lost_by_pto(0);
        assert!(late(&mut l, 0, t0 + Duration::from_millis(5) + RTT));
    }

    /// Only a packet-threshold verdict says anything about reordering depth: a
    /// time or probe verdict contradicted soon after does not move the
    /// threshold.
    #[test]
    fn only_packet_verdicts_adapt_the_packet_threshold() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8192);
        for round in 0..100u32 {
            let base = round * 2;
            send(&mut l, base, t0);
            let _ = l.declare_lost_by_pto(base);
            // A prompt contradiction, well inside the FEC distance.
            assert!(matches!(
                l.resolve(base),
                Some(Resolution::Late {
                    fec_possible: false,
                    ..
                })
            ));
        }
        assert_eq!(l.k_pkt(), 3);
        assert_eq!(l.stats().spurious_losses, 100);
    }

    /// New session, clean slate: records, tombstones, the acknowledgement
    /// reference, the adapted threshold and its evidence.
    #[test]
    fn a_new_session_starts_with_an_empty_ledger_and_the_initial_threshold() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8192);
        l.freeze_reo_wnd();
        let mut base = 0;
        for _ in 0..4 {
            window(&mut l, t0, base, 15);
            base += 70;
        }
        assert!(l.k_pkt() > 3, "premise: the threshold adapted");
        assert!(
            !l.is_empty() && l.tombstone_len() > 0,
            "premise: state held"
        );

        l.reset_session();
        assert_eq!((l.len(), l.tombstone_len(), l.k_pkt()), (0, 0, 3));
        // Nothing of the old session can be declared lost any more, nor can a
        // stale acknowledgement of it be mistaken for anything.
        assert!(l.detect_losses(FAR).is_empty());
        assert!(l.resolve(1).is_none());
        // And the new session's first hole is judged against the new session's
        // acknowledgements only.
        for ws in 0..5 {
            send(&mut l, ws, t0);
        }
        assert!(
            l.detect_losses(FAR).is_empty(),
            "no acknowledged reference yet"
        );
        assert!(l.resolve(4).is_some());
        assert_eq!(l.detect_losses(FAR).len(), 2);
    }

    #[test]
    fn a_frozen_threshold_does_not_adapt() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8192);
        l.freeze_k_pkt();
        window(&mut l, t0, 0, 15);
        assert_eq!(l.k_pkt(), 3);
    }

    #[test]
    fn the_newest_outstanding_are_found_from_the_back_and_resolved_tails_are_dropped() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        for ws in 0..6u32 {
            send(&mut l, ws, t0 + ws * MS);
        }
        assert!(l.resolve(5).is_some());
        assert!(l.resolve(4).is_some());
        assert_eq!(l.latest_outstanding_departure(), Some(t0 + 3 * MS));
        let n = l.newest_outstanding(2, 32);
        assert_eq!(n.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![3, 2]);
        // A resolved one in the middle is skipped, not returned.
        assert!(l.resolve(2).is_some());
        let n = l.newest_outstanding(2, 32);
        assert_eq!(n.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![3, 1]);
        // The scan is bounded.
        assert_eq!(l.newest_outstanding(2, 1).len(), 1);
        // A cursor: the second call continues where the first stopped, and
        // resolved entries do not spend the outstanding-record budget.
        let mut cur = 0;
        let a = l.scan_outstanding(&mut cur, 1, 8);
        assert_eq!(a.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![3]);
        let b = l.scan_outstanding(&mut cur, 8, 1);
        assert_eq!(
            b.iter().map(|(w, _)| *w).collect::<Vec<_>>(),
            vec![1],
            "the resolved 2 is stepped over without using the budget of 1"
        );
        let c = l.scan_outstanding(&mut cur, 8, 8);
        assert_eq!(c.iter().map(|(w, _)| *w).collect::<Vec<_>>(), vec![0]);
        assert!(l.scan_outstanding(&mut cur, 8, 8).is_empty());
    }

    #[test]
    fn a_probe_that_finds_no_content_declares_the_transmission_lost() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        send(&mut l, 1, t0);
        assert_eq!(
            l.declare_lost_by_pto(1).map(|t| t.content),
            Some(Content::Tile(EmitKey::new(1, 0, 0, 0)))
        );
        assert_eq!(l.stats().lost_pto, 1);
        assert!(l.declare_lost_by_pto(1).is_none());
        assert!(matches!(l.resolve(1), Some(Resolution::Late { .. })));
    }

    #[test]
    fn tombstones_are_bounded() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8);
        for ws in 0..40u32 {
            send(&mut l, ws, t0);
            let _ = l.declare_lost_by_pto(ws);
        }
        assert!(l.tombstone_len() <= 8, "got {}", l.tombstone_len());
        assert!(matches!(l.resolve(39), Some(Resolution::Late { .. })));
    }

    #[test]
    fn an_unknown_wire_seq_still_resolves_to_nothing() {
        let mut l = TransmissionLedger::new(8);
        assert!(l.resolve(12345).is_none());
        assert_eq!(l.stats().unknown_acks, 1);
    }

    #[test]
    fn the_cap_evicts_oldest_first_and_counts() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(2);
        l.record(1, t0, tx(1));
        l.record(2, t0, tx(2));
        l.record(3, t0, tx(3));
        assert_eq!(l.len(), 2);
        assert_eq!(l.resolve(1), None, "oldest was evicted");
        assert_eq!(l.resolve(3), Some(Resolution::Live(tx(3))));
        assert_eq!(l.stats().capacity_evictions, 1);
    }

    /// A transmission still queued is on no clock and in no ordering.
    #[test]
    fn a_transmission_that_has_not_departed_is_never_declared_lost() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(1, t0, tx(1));
        send(&mut l, 2, t0);
        for ws in 3..10 {
            send(&mut l, ws, t0);
        }
        assert!(l.resolve(9).is_some());
        let lost = lost_ws(&l.detect_losses(FAR));
        assert!(!lost.contains(&1), "{lost:?}");
        assert_eq!(l.stats().expired_before_departure, 0);
        assert_eq!(l.departed(1), Some(false));
    }

    #[test]
    fn departure_is_stamped_once_and_only_for_a_known_record() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        assert!(!l.mark_departed(9, t0, 1), "unknown wire_seq");
        l.record(9, t0, tx(9));
        assert!(l.mark_departed(9, t0, 1));
        assert!(!l.mark_departed(9, t0 + 1000 * MS, 2), "twice");
        assert_eq!(l.departed(9), Some(true));
        assert_eq!(l.stats().departures_stamped, 1);
        assert!(matches!(
            l.resolve(9),
            Some(Resolution::Live(Transmission {
                depart_us: Some(1),
                dep_idx: Some(0),
                ..
            }))
        ));
        assert!(!l.mark_departed(9, t0, 3), "resolved");
    }

    #[test]
    fn eviction_of_a_queued_transmission_is_counted() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(2);
        l.record(1, t0, tx(1));
        l.record(2, t0, tx(2));
        l.mark_departed(2, t0, 0);
        l.record(3, t0, tx(3));
        assert_eq!(l.stats().capacity_evictions, 1);
        assert_eq!(l.stats().evicted_before_departure, 1);
    }

    /// A record that never departs sits at the front of the recording order
    /// forever. The resolved entries behind it must not pile up without bound.
    #[test]
    fn the_order_queues_stay_bounded_behind_a_stuck_front() {
        let t0 = Instant::now();
        let cap = 64;
        let mut l = TransmissionLedger::new(cap);
        l.record(0, t0, tx(0)); // never departs, never resolves
        for ws in 1..20_000u32 {
            l.record(ws, t0, tx(ws));
            l.mark_departed(ws, t0, 0);
            assert!(l.resolve(ws).is_some());
        }
        assert!(
            l.order.len() <= l.len() + 2 * cap + 64 + 1,
            "{}",
            l.order.len()
        );
        assert!(
            l.departed_order.len() <= l.len() + 2 * cap + 64 + 1,
            "{}",
            l.departed_order.len()
        );
    }

    // The invariant the whole design rests on: across any interleaving of
    // sending, acknowledging (in any order) and detecting, a transmission is
    // classified **exactly once** as received or lost, and a lost one is
    // received at most once more, as Late.
    proptest::proptest! {
        #[test]
        fn every_transmission_is_classified_exactly_once(
            ops in proptest::collection::vec((0u8..3, 0u32..40), 1..300)
        ) {
            use std::collections::HashSet;
            let t0 = Instant::now();
            let mut l = TransmissionLedger::new(8192);
            let mut outstanding: HashSet<u32> = HashSet::new();
            let mut lost: HashSet<u32> = HashSet::new();
            let mut next_ws = 0u32;
            let mut now = t0;
            for (op, arg) in ops {
                now += MS;
                match op {
                    0 => {
                        send(&mut l, next_ws, now);
                        outstanding.insert(next_ws);
                        next_ws += 1;
                    }
                    1 => {
                        let ws = arg % next_ws.max(1);
                        match l.resolve(ws) {
                            Some(Resolution::Live(_)) => {
                                proptest::prop_assert!(outstanding.remove(&ws), "live ack of non-outstanding");
                            }
                            Some(Resolution::Late { .. }) => {
                                proptest::prop_assert!(lost.remove(&ws), "late ack of non-lost");
                            }
                            None => {}
                        }
                    }
                    _ => {
                        for (ws, _, _) in l.detect_losses_at(now, 5 * MS) {
                            proptest::prop_assert!(outstanding.remove(&ws), "verdict on non-outstanding {}", ws);
                            proptest::prop_assert!(lost.insert(ws));
                        }
                    }
                }
            }
            // Nothing outstanding is also lost.
            proptest::prop_assert!(outstanding.is_disjoint(&lost));
            proptest::prop_assert_eq!(l.len(), outstanding.len());
        }
    }

    #[test]
    fn departed_bytes_tally_follows_every_way_a_record_leaves() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(3);
        let sized = |n: u32, bytes: usize| Transmission {
            wire_bytes: bytes,
            ..tx(n)
        };
        l.record(1, t0, sized(1, 100));
        assert_eq!(
            l.departed_bytes_in_flight(),
            0,
            "queued bytes are not in flight"
        );
        l.mark_departed(1, t0, 1);
        assert_eq!(l.departed_bytes_in_flight(), 100);
        l.record(2, t0, sized(2, 200));
        l.mark_departed(2, t0, 2);
        l.record(3, t0, sized(3, 400));
        l.mark_departed(3, t0, 3);
        assert_eq!(l.departed_bytes_in_flight(), 700);
        // Resolved: leaves.
        assert!(l.resolve(1).is_some());
        assert_eq!(l.departed_bytes_in_flight(), 600);
        // 3 is acknowledged ahead of 2, which makes 2 a hole; the time rule
        // (zero threshold) declares it lost. Declared lost: leaves, and its
        // late ack does not leave twice.
        assert!(l.resolve(3).is_some());
        assert_eq!(l.departed_bytes_in_flight(), 200);
        let lost = l.detect_losses_at(t0 + Duration::from_secs(5), Duration::ZERO);
        assert_eq!(lost.len(), 1);
        assert_eq!(l.departed_bytes_in_flight(), 0);
        assert!(matches!(l.resolve(2), Some(Resolution::Late { .. })));
        assert_eq!(l.departed_bytes_in_flight(), 0);
        // Replaced under the same wire_seq: the old departed record leaves.
        l.record(4, t0, sized(4, 50));
        l.mark_departed(4, t0, 4);
        l.record(4, t0, sized(4, 70));
        assert_eq!(l.departed_bytes_in_flight(), 0);
        // Evicted by the capacity cap: leaves.
        l.mark_departed(4, t0, 4);
        assert_eq!(l.departed_bytes_in_flight(), 70);
        for ws in 5..=7 {
            l.record(ws, t0, sized(ws, 10));
        }
        assert_eq!(l.departed_bytes_in_flight(), 0, "4 was the oldest and went");
    }

    #[test]
    fn a_rate_mark_is_taken_once_after_departure_and_read_before_resolve() {
        use crate::transport::delivery_rate::RateSamplePacketState;
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8);
        let mark = |bytes: usize, before: u64, dep_idx: u64| RateMark {
            pkt_num: bytes as u64 * 1000 + before + dep_idx,
            state: RateSamplePacketState::default(),
            pacer_limited: false,
        };
        l.record(1, t0, tx(1));
        l.mark_rate(1, mark);
        assert!(l.rate_mark(1).is_none(), "not departed: no mark");
        l.mark_departed(1, t0, 1);
        l.mark_rate(1, mark);
        let (m, at) = l.rate_mark(1).expect("marked");
        assert_eq!(
            (m.pkt_num, at),
            (1200 * 1000, t0),
            "bytes in flight before it: 0"
        );
        l.mark_rate(1, |_, _, _| mark(1, 1, 1));
        assert_eq!(l.rate_mark(1).unwrap().0, m, "taken once");
        l.record(2, t0, tx(2));
        l.mark_departed(2, t0, 2);
        l.mark_rate(2, mark);
        assert_eq!(l.rate_mark(2).unwrap().0.pkt_num, 1200 * 1000 + 1200 + 1);
        assert!(l.resolve(1).is_some());
        assert!(l.rate_mark(1).is_none());
    }

    /// A verdict keeps the delivery-rate mark in the tombstone, so a late
    /// acknowledgement of the lost transmission can still be sampled.
    #[test]
    fn a_tombstone_keeps_the_rate_mark_and_the_departure_instant() {
        use crate::transport::delivery_rate::RateSamplePacketState;
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8);
        let mark = |_: usize, _: u64, dep_idx: u64| RateMark {
            pkt_num: dep_idx + 1,
            state: RateSamplePacketState::default(),
            pacer_limited: false,
        };
        for ws in 1..=3 {
            l.record(ws, t0, tx(ws));
            l.mark_departed(ws, t0, ws as u64);
            l.mark_rate(ws, mark);
        }
        // 3 acknowledged ahead of 2: 2 is a hole and the time rule takes it.
        assert!(l.resolve(3).is_some());
        let lost = l.detect_losses_at(t0 + Duration::from_secs(5), Duration::ZERO);
        assert_eq!(lost.iter().map(|l| l.0).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(l.tombstone_len(), 2, "premise: both are tombstones");
        let (m, at) = l.rate_mark(2).expect("the tombstone kept the mark");
        assert_eq!((m.pkt_num, at), (2, t0));
        assert!(matches!(l.resolve(2), Some(Resolution::Late { .. })));
        assert!(l.rate_mark(2).is_none(), "taken with the tombstone");
    }

    // ---- the reordering window (RFC 8985 section 6.2) ----

    /// `n` datagrams from `first`, all stamped `at` (one drain turn).
    fn burst(l: &mut TransmissionLedger, first: u32, n: u32, at: Instant) {
        for ws in first..first + n {
            send(l, ws, at);
        }
    }

    fn ack(l: &mut TransmissionLedger, ws: u32, now: Instant) {
        assert!(l.resolve_at(ws, Some((now, RTT))).is_some());
    }

    /// A ledger that has seen one burst deep-reordered: `0..10` left together,
    /// `9` arrived first (so 0..=6 were judged lost) and `0` arrived late.
    /// Returns it with `reo_wnd` raised.
    fn learned(t0: Instant) -> TransmissionLedger {
        let mut l = TransmissionLedger::new(256);
        burst(&mut l, 0, 10, t0);
        ack(&mut l, 9, t0 + 40 * MS);
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, FAR).len(), 7);
        assert_eq!(l.reo_wnd(), Duration::ZERO, "premise: not yet learned");
        ack(&mut l, 0, t0 + 44 * MS);
        // 7 and 8 were never judged; they arrive.
        ack(&mut l, 7, t0 + 45 * MS);
        ack(&mut l, 8, t0 + 45 * MS);
        l
    }

    #[test]
    fn the_window_is_learned_from_a_contradicted_verdict_and_bounded() {
        let t0 = Instant::now();
        let l = learned(t0);
        // min_rtt / 8 on a zero measured gap (one stamp for the whole burst).
        assert_eq!(l.reo_wnd(), RTT / 8);
        assert_eq!(l.stats().reo_wnd_raises, 1);
        assert_eq!(l.stats().spurious_disp_max, 9);
        // A gap far past the ceiling does not open the window at once: one
        // step per round trip, up to min_rtt / 2. Four early departures, each
        // judged by count against a much later one.
        let mut l = TransmissionLedger::new(64);
        for i in 0..4u32 {
            send(&mut l, i, t0 + 500 * MS * i);
        }
        burst(&mut l, 4, 3, t0 + 2000 * MS);
        ack(&mut l, 6, t0 + 2040 * MS);
        assert_eq!(l.detect_losses_at(t0 + 2040 * MS, FAR).len(), 4);
        let s0 = t0 + 2100 * MS;
        ack(&mut l, 0, s0);
        assert_eq!(l.reo_wnd(), RTT / 8, "one outlier is one step");
        assert_eq!(l.stats().spurious_gap_us_max, 2_000_000);
        // Same round trip: no second step.
        ack(&mut l, 1, s0 + 10 * MS);
        assert_eq!(l.reo_wnd(), RTT / 8);
        // A round trip later the next contradicted verdict steps again.
        ack(&mut l, 2, s0 + 50 * MS);
        assert_eq!(l.reo_wnd(), RTT / 4);
        ack(&mut l, 3, s0 + 100 * MS);
        assert_eq!(l.reo_wnd(), RTT / 8 * 3);
    }

    #[test]
    fn a_probe_timeout_verdict_followed_by_a_late_ack_leaves_the_window_closed() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        burst(&mut l, 0, 3, t0);
        assert!(l.declare_lost_by_pto(0).is_some());
        ack(&mut l, 0, t0 + 300 * MS);
        assert_eq!(l.stats().spurious_losses, 1, "premise: a late ack");
        assert_eq!(l.reo_wnd(), Duration::ZERO);
        assert_eq!(l.stats().reo_wnd_raises, 0);
    }

    #[test]
    fn inside_the_window_the_count_rule_stays_at_the_depth_the_path_showed() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(256);
        // A burst of six, the last acknowledged first: 0..=2 judged by count,
        // then 0 arrives. Displacement 5, so depth 6.
        burst(&mut l, 0, 6, t0);
        ack(&mut l, 5, t0 + 40 * MS);
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, FAR).len(), 3);
        ack(&mut l, 0, t0 + 44 * MS);
        ack(&mut l, 3, t0 + 45 * MS);
        ack(&mut l, 4, t0 + 45 * MS);
        assert!(l.reo_wnd() > Duration::ZERO);
        // Another burst of ten, one stamp, ws 10 truly lost: judged when 6
        // later departures of its own burst are acknowledged, not 3 and not
        // only when the next burst comes.
        let b = t0 + 100 * MS;
        burst(&mut l, 10, 10, b);
        for ws in 11..=15 {
            ack(&mut l, ws, b + 40 * MS);
        }
        assert!(
            l.detect_losses_at(b + 40 * MS, FAR).is_empty(),
            "five behind is inside the depth"
        );
        ack(&mut l, 16, b + 40 * MS);
        assert_eq!(lost_ws(&l.detect_losses_at(b + 40 * MS, FAR)), vec![10]);
    }

    #[test]
    fn one_loss_burst_judged_over_several_turns_is_one_episode() {
        let t0 = Instant::now();
        let mut l = learned(t0);
        let thr = 100 * MS;
        let at = t0 + Duration::from_secs(40);
        // Five datagrams 2 ms apart (inside the window), then one that is
        // acknowledged. The time rule finds them one turn at a time.
        for i in 0..5u32 {
            send(&mut l, 10 + i, at + 2 * MS * i);
        }
        send(&mut l, 15, at + 10 * MS);
        ack(&mut l, 15, at + 50 * MS);
        let mut judged = 0;
        for i in 0..5u32 {
            let now = at + thr + l.reo_wnd() + 2 * MS * i;
            judged += l.detect_losses_at(now, thr).len();
        }
        assert_eq!(judged, 5, "premise: five verdicts over several turns");
        assert_eq!(l.reo_recoveries, 1);
        assert!(l.reo_wnd() > Duration::ZERO);
        // A new loss on later departures is a second episode.
        let at = t0 + Duration::from_secs(50);
        send(&mut l, 20, at);
        send(&mut l, 21, at + 30 * MS);
        ack(&mut l, 21, at + 60 * MS);
        assert_eq!(l.detect_losses_at(at + 200 * MS, thr).len(), 1);
        assert_eq!(l.reo_recoveries, 2);
    }

    #[test]
    fn a_reordered_burst_is_not_judged_lost_once_the_window_is_learned() {
        let t0 = Instant::now();
        let b = t0 + 100 * MS;
        let mut l = learned(t0);
        burst(&mut l, 10, 10, b);
        ack(&mut l, 19, b + 40 * MS);
        // 10..=16 are 3+ places behind 19 but left with it.
        assert!(l.detect_losses_at(b + 40 * MS, FAR).is_empty());
        // The same burst on a ledger that never learns: seven verdicts.
        let mut m = TransmissionLedger::new(256);
        m.freeze_reo_wnd();
        let mut m = {
            burst(&mut m, 0, 10, t0);
            ack(&mut m, 9, t0 + 40 * MS);
            m.detect_losses_at(t0 + 40 * MS, FAR);
            ack(&mut m, 0, t0 + 44 * MS);
            ack(&mut m, 7, t0 + 45 * MS);
            ack(&mut m, 8, t0 + 45 * MS);
            burst(&mut m, 10, 10, b);
            ack(&mut m, 19, b + 40 * MS);
            m
        };
        assert_eq!(m.reo_wnd(), Duration::ZERO);
        assert_eq!(m.detect_losses_at(b + 40 * MS, FAR).len(), 7);
    }

    #[test]
    fn a_real_loss_after_reordering_is_still_judged_by_the_next_burst_or_the_timer() {
        let t0 = Instant::now();
        let b = t0 + 100 * MS;
        let thr = 45 * MS;
        // Next burst 100 ms later acknowledges: gap >= reo_wnd, so the packet
        // rule judges the whole of the first.
        let mut l = learned(t0);
        burst(&mut l, 10, 10, b);
        ack(&mut l, 19, b + 40 * MS);
        burst(&mut l, 20, 10, b + 100 * MS);
        ack(&mut l, 29, b + 140 * MS);
        let lost = l.detect_losses_at(b + 140 * MS, FAR);
        assert!(lost.iter().all(|(_, _, r)| *r == LossReason::Packet));
        // 20..=26 left with the acknowledged 29 and are held back.
        assert_eq!(lost_ws(&lost), (10..=18).collect::<Vec<_>>());

        // With nothing after it, the stream of later departures going quiet
        // for `reo_wnd` judges it (the timer is armed for that).
        let mut l = learned(t0);
        burst(&mut l, 10, 10, b);
        ack(&mut l, 19, b + 40 * MS);
        let due = l
            .next_loss_time(thr)
            .expect("a held-back tail has a deadline");
        assert_eq!(due, b + 40 * MS + l.reo_wnd());
        assert!(l.detect_losses_at(due - MS, thr).is_empty());
        let lost = l.detect_losses_at(due, thr);
        // 17 and 18 are under `k_pkt` places behind 19: the clock's.
        assert_eq!(lost_ws(&lost), (10..=16).collect::<Vec<_>>());
        assert!(lost.iter().all(|(_, _, r)| *r == LossReason::Packet));
    }

    #[test]
    fn a_tail_too_short_for_the_count_is_judged_by_the_clock_plus_the_window() {
        let t0 = Instant::now();
        let b = t0 + 100 * MS;
        let thr = 45 * MS;
        let mut l = learned(t0);
        // Three datagrams of one burst, the last acknowledged: 10 and 11 are
        // under `k_pkt` places behind it, so only the clock can judge them.
        burst(&mut l, 10, 3, b);
        ack(&mut l, 12, b + 40 * MS);
        let due = l.next_loss_time(thr).expect("deadline");
        assert_eq!(due, b + thr + l.reo_wnd());
        assert!(l.detect_losses_at(due - MS, thr).is_empty());
        let lost = l.detect_losses_at(due, thr);
        assert_eq!(lost_ws(&lost), vec![10, 11]);
        assert!(lost.iter().all(|(_, _, r)| *r == LossReason::Time));
    }

    #[test]
    fn the_window_returns_to_zero_after_sixteen_quiet_loss_episodes() {
        let t0 = Instant::now();
        let mut l = learned(t0);
        assert!(l.reo_wnd() > Duration::ZERO);
        let thr = 100 * MS;
        let mut ws = 10u32;
        for n in 0..REO_PERSIST {
            assert!(l.reo_wnd() > Duration::ZERO, "episode {n}");
            // One datagram, then one far enough behind and later to clear
            // the window and the count: a real loss, never acknowledged.
            let at = t0 + Duration::from_secs(40 + n as u64);
            send(&mut l, ws, at);
            burst(&mut l, ws + 1, 4, at + 30 * MS);
            ack(&mut l, ws + 4, at + 70 * MS);
            let lost = l.detect_losses_at(at + 70 * MS, thr);
            assert_eq!(lost_ws(&lost), vec![ws], "episode {n}");
            for w in ws + 1..ws + 4 {
                ack(&mut l, w, at + 71 * MS);
            }
            ws += 5;
        }
        assert_eq!(l.reo_wnd(), Duration::ZERO);
    }

    #[test]
    fn a_late_acknowledgement_restarts_the_recovery_count() {
        let t0 = Instant::now();
        let mut l = learned(t0);
        let thr = 100 * MS;
        let mut ws = 10u32;
        for n in 0..REO_PERSIST - 1 {
            let at = t0 + Duration::from_secs(40 + n as u64);
            send(&mut l, ws, at);
            burst(&mut l, ws + 1, 4, at + 30 * MS);
            ack(&mut l, ws + 4, at + 70 * MS);
            assert_eq!(l.detect_losses_at(at + 70 * MS, thr).len(), 1);
            for w in ws + 1..ws + 4 {
                ack(&mut l, w, at + 71 * MS);
            }
            ws += 5;
        }
        // The last verdict is contradicted: 15 episodes do not count. (A newer
        // datagram was acknowledged since: that is reordering, not a stall.)
        send(&mut l, ws + 100, t0 + Duration::from_secs(59));
        ack(&mut l, ws + 100, t0 + Duration::from_secs(59) + 40 * MS);
        ack(&mut l, ws - 5, t0 + Duration::from_secs(60));
        let at = t0 + Duration::from_secs(100);
        send(&mut l, ws, at);
        burst(&mut l, ws + 1, 4, at + 30 * MS);
        ack(&mut l, ws + 4, at + 70 * MS);
        assert_eq!(l.detect_losses_at(at + 70 * MS, thr).len(), 1);
        assert!(l.reo_wnd() > Duration::ZERO, "one episode is not sixteen");
    }

    #[test]
    fn a_new_session_forgets_the_window() {
        let t0 = Instant::now();
        let mut l = learned(t0);
        assert!(l.reo_wnd() > Duration::ZERO, "premise");
        l.reset_session();
        assert_eq!(l.reo_wnd(), Duration::ZERO);
        // Judged by the count alone again: 0 and 1 left with 4, 2 and 3 too.
        burst(&mut l, 0, 5, t0);
        ack(&mut l, 4, t0 + 40 * MS);
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, FAR).len(), 2);
    }

    #[test]
    fn without_reordering_the_window_stays_zero_and_verdicts_are_unchanged() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        burst(&mut l, 0, 10, t0);
        // 3 is lost on the wire; the rest arrive in order.
        for ws in (0..10).filter(|w| *w != 3) {
            ack(&mut l, ws, t0 + 40 * MS);
        }
        assert_eq!(lost_ws(&l.detect_losses_at(t0 + 40 * MS, FAR)), vec![3]);
        assert_eq!(l.reo_wnd(), Duration::ZERO);
        assert_eq!(l.stats().reo_wnd_raises, 0);
        assert_eq!(l.next_loss_time(45 * MS), None);
    }

    #[test]
    fn a_displacement_past_the_depth_limit_saturates_instead_of_switching_the_rule_off() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(512);
        // A frame of 100 in one batch: the last resolves first, 0 comes back
        // late. Displacement 99, past REO_DEPTH_MAX.
        burst(&mut l, 0, 100, t0);
        ack(&mut l, 99, t0 + 40 * MS);
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, FAR).len(), 97);
        ack(&mut l, 0, t0 + 44 * MS);
        ack(&mut l, 97, t0 + 45 * MS);
        ack(&mut l, 98, t0 + 45 * MS);
        assert_eq!(l.stats().spurious_disp_max, 99, "premise");
        assert!(l.reo_wnd() > Duration::ZERO, "premise");
        assert_eq!(l.stats().reo_depth, REO_DEPTH_MAX);
        // Another frame: 200 truly lost; the datagrams after it are
        // acknowledged. At the saturated depth that is a verdict once
        // REO_DEPTH_MAX are behind it; with the rule off it would wait for the
        // stream to go quiet or the next frame.
        let b = t0 + 100 * MS;
        burst(&mut l, 200, 100, b);
        for ws in 201..=200 + REO_DEPTH_MAX as u32 - 1 {
            ack(&mut l, ws, b + 40 * MS);
        }
        assert!(
            l.detect_losses_at(b + 40 * MS, FAR).is_empty(),
            "inside the depth"
        );
        ack(&mut l, 200 + REO_DEPTH_MAX as u32, b + 40 * MS);
        assert_eq!(lost_ws(&l.detect_losses_at(b + 40 * MS, FAR)), vec![200]);
        assert_eq!(l.stats().reo_judged_depth, 1);
    }

    #[test]
    fn a_window_learned_on_a_slow_path_is_clamped_when_the_path_gets_fast() {
        let t0 = Instant::now();
        let slow = 400 * MS;
        let mut l = TransmissionLedger::new(256);
        burst(&mut l, 0, 10, t0);
        l.resolve_at(9, Some((t0 + 40 * MS, slow)));
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, FAR).len(), 7);
        l.resolve_at(0, Some((t0 + 44 * MS, slow)));
        assert_eq!(l.reo_wnd(), 50 * MS, "premise: a 50 ms window");
        for ws in [7, 8] {
            l.resolve_at(ws, Some((t0 + 45 * MS, slow)));
        }
        // Now a 5 ms path (time threshold 6 ms): one datagram, the next
        // acknowledged 10 ms later. The window may not exceed half the
        // threshold, 3 ms.
        let thr = 6 * MS;
        let at = t0 + Duration::from_secs(10);
        send(&mut l, 20, at);
        send(&mut l, 21, at + 10 * MS);
        l.resolve_at(21, Some((at + 11 * MS, 5 * MS)));
        assert_eq!(l.next_loss_time(thr), Some(at + thr + thr / 2));
        assert!(l
            .detect_losses_at(at + 9 * MS - Duration::from_micros(1), thr)
            .is_empty());
        let lost = l.detect_losses_at(at + 9 * MS, thr);
        assert_eq!(lost_ws(&lost), vec![20]);
        assert_eq!(
            l.reo_wnd(),
            50 * MS,
            "the learned value is kept, only use is clamped"
        );
    }

    #[test]
    fn a_late_ack_after_an_ack_path_stall_teaches_nothing() {
        let t0 = Instant::now();
        let thr = 45 * MS;
        let mut l = TransmissionLedger::new(256);
        burst(&mut l, 0, 10, t0);
        ack(&mut l, 9, t0 + 40 * MS);
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, thr).len(), 7);
        // The client stops acknowledging; 0 is acknowledged 200 ms later with
        // nothing newer in between.
        ack(&mut l, 0, t0 + 240 * MS);
        assert_eq!(l.stats().spurious_losses, 1, "premise: a late ack");
        assert_eq!(l.reo_wnd(), Duration::ZERO);
        assert_eq!(l.stats().reo_wnd_raises, 0);
        assert_eq!(l.stats().reo_stall_skips, 1);
        // The same gap with a newer datagram acknowledged in between is
        // reordering and teaches.
        let mut l = TransmissionLedger::new(256);
        burst(&mut l, 0, 10, t0);
        ack(&mut l, 9, t0 + 40 * MS);
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, thr).len(), 7);
        send(&mut l, 10, t0 + 100 * MS);
        ack(&mut l, 10, t0 + 140 * MS);
        ack(&mut l, 0, t0 + 240 * MS);
        assert!(l.reo_wnd() > Duration::ZERO);
        assert_eq!(l.stats().reo_stall_skips, 0);
    }

    #[test]
    fn an_unknown_min_rtt_teaches_nothing() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(256);
        burst(&mut l, 0, 10, t0);
        l.resolve_at(9, Some((t0 + 40 * MS, Duration::ZERO)));
        assert_eq!(l.detect_losses_at(t0 + 40 * MS, FAR).len(), 7);
        l.resolve_at(0, Some((t0 + 44 * MS, Duration::ZERO)));
        assert_eq!(l.stats().spurious_losses, 1, "premise: a late ack");
        assert_eq!(l.reo_wnd(), Duration::ZERO);
        assert_eq!(l.stats().reo_wnd_raises, 0);
        assert_eq!(l.stats().reo_depth, 0);
        assert_eq!(l.stats().reo_learned_packet, 0);
    }

    #[test]
    fn loss_episodes_do_not_close_the_window_while_reordering_is_recent() {
        // 20 loss episodes a second apart, the last contradicted verdict
        // under 30 s ago: none is a quiet episode, so the window stays.
        // (Without REO_QUIET_SPAN the two-threshold floor alone would count
        // them and the 16th would close it.)
        let t0 = Instant::now();
        let mut l = learned(t0);
        let thr = 100 * MS;
        let mut ws = 10u32;
        for n in 0..20u64 {
            let at = t0 + Duration::from_secs(1 + n);
            send(&mut l, ws, at);
            burst(&mut l, ws + 1, 4, at + 30 * MS);
            ack(&mut l, ws + 4, at + 70 * MS);
            let lost = l.detect_losses_at(at + 70 * MS, thr);
            assert_eq!(lost_ws(&lost), vec![ws], "episode {n}");
            for w in ws + 1..ws + 4 {
                ack(&mut l, w, at + 71 * MS);
            }
            ws += 5;
        }
        assert!(l.reo_wnd() > Duration::ZERO);
        assert_eq!(l.reo_recoveries, 0);
    }

    #[test]
    fn a_fec_recovery_does_not_open_the_window() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        send_group(&mut l, 0, t0, t0 + 2 * MS);
        for ws in 1..10 {
            ack(&mut l, ws, t0 + 40 * MS);
        }
        let _ = l.detect_losses_at(t0 + 40 * MS, FAR);
        // 0 comes back a round trip after its parity left: a recovery.
        assert!(late(&mut l, 0, t0 + 2 * MS + RTT));
        assert_eq!(l.reo_wnd(), Duration::ZERO);
    }
}
