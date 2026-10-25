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
use std::time::Instant;

use crate::transport::reliable_emitter::EmitKey;

/// What was sent, and what it was sent for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transmission {
    /// Clock-relative microseconds, the same value stamped into the datagram
    /// header — so the estimator sees the epoch its other inputs use.
    pub emit_us: u64,
    pub wire_bytes: usize,
    /// Content identity, for the consumers that still think in tile-passes.
    pub key: EmitKey,
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
/// A late acknowledgement this many departures after the transmission could be
/// an FEC recovery: the group's parity leaves `PARITY_INTERLEAVE_OFFSET`
/// wire_seqs after the group starts, so before that distance no parity exists
/// to rebuild anything from.
const FEC_POSSIBLE_AFTER: u64 = crate::transport::reliable_emitter::PARITY_INTERLEAVE_OFFSET as u64;
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
        /// The acknowledgement came at least `PARITY_INTERLEAVE_OFFSET`
        /// departures after the transmission, so it may be the client acking a
        /// datagram it rebuilt from parity under its original `wire_seq`: a
        /// real network loss, not a premature verdict. Nothing on the wire
        /// says which, so the caller treats it as a loss that happened (no
        /// retraction, no threshold adaptation) and counts it apart. A late
        /// acknowledgement arriving sooner cannot be a recovery and is a
        /// genuinely premature verdict.
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
    departed: Option<Instant>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LedgerStats {
    /// Acknowledgements naming a `wire_seq` this ledger has no record of.
    /// Either a duplicate (the overlap entries every ACK batch carries, or a
    /// genuinely duplicated datagram) or an acknowledgement that arrived after
    /// its horizon expired. Counted rather than silent: a sustained rise means
    /// the horizon is too short, or a peer is confused.
    pub unknown_acks: u64,
    /// Records dropped by the hard cap rather than by the horizon. Non-zero
    /// means transmissions outran acknowledgements badly enough that the cap
    /// bound the ledger instead of time — the loss ratio is under-reported
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
}

#[derive(Debug, Clone, Copy)]
struct Record {
    /// When the ledger was told of it (enqueue). Identity and capacity order
    /// only: the loss clock is `departed`.
    #[allow(dead_code)]
    recorded_at: Instant,
    /// When it left quinn's FIFO. `None` until then.
    departed: Option<Instant>,
    tx: Transmission,
}

pub struct TransmissionLedger {
    records: HashMap<u32, Record>,
    /// Recording order, for the capacity cap: the oldest record goes first.
    /// Resolved entries are skipped lazily.
    order: VecDeque<u32>,
    /// Departure order, for the loss clock. A transmission enters when it is
    /// stamped, so the front is always the one that has been on the wire the
    /// longest and the walk in `expire_with_seq` stops at the first young
    /// one. Resolved entries are skipped lazily.
    departed_order: VecDeque<u32>,
    capacity: usize,
    stats: LedgerStats,
    /// `wire_seq -> Transmission` for expired records, retained so a late
    /// acknowledgement can still release its content. Bounded by the same
    /// capacity as `records`; oldest evicted first.
    tombstones: HashMap<u32, Tombstone>,
    tombstone_order: VecDeque<u32>,
    /// Next departure index to hand out.
    next_dep_idx: u64,
    /// The newest acknowledged departure: its index and when it left. Loss
    /// is only ever judged against this, never against the clock, so a queue
    /// that delays everything in order cannot produce a verdict (F4).
    largest_acked: Option<u64>,
    /// Current packet threshold, and what it adapts from: verdicts issued so
    /// far, and one bit per recent verdict (by verdict number modulo 64) set
    /// when that verdict was contradicted by a late acknowledgement.
    k_pkt: u64,
    freeze_k_pkt: bool,
    verdicts: u64,
    spurious_bits: u64,
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
            largest_acked: None,
            k_pkt: K_PKT_INITIAL,
            freeze_k_pkt: false,
            verdicts: 0,
            spurious_bits: 0,
        }
    }

    /// Keep `K_PKT` at its initial value whatever the spurious rate says. The
    /// mutation the adaptive-threshold gate is shown against.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub fn freeze_k_pkt(&mut self) {
        self.freeze_k_pkt = true;
    }

    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
            k_pkt: self.k_pkt,
            ..self.stats
        }
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
    /// Recording is identity only. The loss clock does not start until
    /// [`Self::mark_departed`]: a transmission still queued in the emitter or
    /// in quinn's FIFO has not been sent, so it cannot have been lost.
    pub fn record(&mut self, wire_seq: u32, at: Instant, tx: Transmission) {
        let rec = Record {
            recorded_at: at,
            departed: None,
            tx: Transmission {
                depart_us: None,
                dep_idx: None,
                ..tx
            },
        };
        if self.records.insert(wire_seq, rec).is_some() {
            self.stats.capacity_evictions += 1;
            self.order.retain(|w| *w != wire_seq);
        }
        self.order.push_back(wire_seq);
        while self.records.len() > self.capacity {
            match self.order.pop_front() {
                Some(oldest) => {
                    if let Some(r) = self.records.remove(&oldest) {
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
    /// the emitter's clock). From here the loss clock runs.
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
        self.departed_order.push_back(wire_seq);
        self.stats.departures_stamped += 1;
        true
    }

    /// Has `wire_seq` left quinn's FIFO? `None` if the ledger does not hold it.
    pub fn departed(&self, wire_seq: u32) -> Option<bool> {
        self.records.get(&wire_seq).map(|r| r.departed.is_some())
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
        self.largest_acked = None;
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
        if let Some(rec) = self.records.remove(&wire_seq) {
            if let (Some(idx), Some(_)) = (rec.tx.dep_idx, rec.departed) {
                if self.largest_acked.is_none_or(|l| idx > l) {
                    self.largest_acked = Some(idx);
                }
            }
            return Some(Resolution::Live(rec.tx));
        }
        if let Some(t) = self.tombstones.remove(&wire_seq) {
            self.stats.acked_after_declared_lost += 1;
            self.stats.spurious_losses += 1;
            let tx = t.tx;
            // An acknowledgement of a departure newer than any acknowledged so
            // far moves the reference the thresholds are judged against.
            if let (Some(idx), Some(_)) = (tx.dep_idx, t.departed) {
                if self.largest_acked.is_none_or(|l| idx > l) {
                    self.largest_acked = Some(idx);
                }
            }
            let fec_possible = tx
                .dep_idx
                .is_some_and(|i| self.next_dep_idx.saturating_sub(i) >= FEC_POSSIBLE_AFTER);
            if fec_possible {
                self.stats.late_acks_fec_possible += 1;
            } else if let Some(n) = t.packet_verdict_no {
                self.note_spurious(n);
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
        let Some(largest_idx) = self.largest_acked else {
            return lost;
        };
        while let Some(front) = self.departed_order.front().copied() {
            let (idx, at) = match self.records.get(&front) {
                // Already resolved, or the record was replaced by a newer one
                // under the same `wire_seq` that has not departed yet; drop
                // the stale order entry and continue.
                None | Some(Record { departed: None, .. }) => {
                    self.departed_order.pop_front();
                    continue;
                }
                Some(Record {
                    departed: Some(at),
                    tx,
                    ..
                }) => (tx.dep_idx.unwrap_or(u64::MAX), *at),
            };
            if idx >= largest_idx {
                break;
            }
            let reason = if idx + self.k_pkt <= largest_idx {
                LossReason::Packet
            } else if at + time_threshold <= now {
                LossReason::Time
            } else {
                break;
            };
            self.departed_order.pop_front();
            self.declare_lost(front, reason, &mut lost);
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
        let largest_idx = self.largest_acked?;
        while let Some(front) = self.departed_order.front().copied() {
            match self.records.get(&front) {
                Some(Record {
                    departed: Some(at),
                    tx,
                    ..
                }) => {
                    return (tx.dep_idx.unwrap_or(u64::MAX) < largest_idx)
                        .then(|| *at + time_threshold);
                }
                _ => {
                    self.departed_order.pop_front();
                }
            }
        }
        None
    }

    /// The newest outstanding transmissions that have departed, newest first:
    /// up to `n` of them, looking at no more than `max_scan` records from the
    /// back. Resolved entries at the back are dropped as they are met.
    ///
    /// What the probe timeout retransmits. A bounded scan, so an unlucky run
    /// of resolved entries behind the tail cannot make one probe expensive.
    pub fn newest_outstanding(&mut self, n: usize, max_scan: usize) -> Vec<(u32, Transmission)> {
        // Resolved entries at the very back are dead weight for every later
        // probe: drop them.
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
        let mut out = Vec::new();
        for ws in self.departed_order.iter().rev().take(max_scan) {
            if out.len() >= n {
                break;
            }
            if let Some(Record {
                departed: Some(_),
                tx,
                ..
            }) = self.records.get(ws)
            {
                out.push((*ws, *tx));
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
                departed: rec.departed,
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
    use std::time::Duration;

    const MS: Duration = Duration::from_millis(1);
    /// A time threshold nothing in these tests reaches unless it means to.
    const FAR: Duration = Duration::from_secs(3600);

    fn tx(n: u32) -> Transmission {
        Transmission {
            emit_us: n as u64 * 1000,
            wire_bytes: 1200,
            key: EmitKey::new(n, 0, 0, 0),
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
            matches!(l.resolve(0), Some(Resolution::Late { tx: t, .. }) if t.key == EmitKey::new(0, 0, 0, 0))
        );
        assert_eq!(l.stats().acked_after_declared_lost, 1);
        assert_eq!(l.stats().spurious_losses, 1);
        assert_eq!(l.stats().unknown_acks, 0);
        assert!(l.resolve(0).is_none(), "a tombstone resolves once");
        assert_eq!(l.stats().spurious_losses, 1, "and is not counted twice");
    }

    /// 70 sent, the last acknowledged: all but the last `K_PKT` are lost. Then
    /// the newest `spurious` of the verdicts are contradicted at once, so each
    /// late acknowledgement comes fewer than `PARITY_INTERLEAVE_OFFSET`
    /// departures after its transmission (too soon to be an FEC recovery).
    fn window(l: &mut TransmissionLedger, t0: Instant, base: u32, spurious: usize) {
        for ws in base..base + 70 {
            send(l, ws, t0);
        }
        assert!(l.resolve(base + 69).is_some());
        let lost = l.detect_losses(FAR);
        // Everything but the last K of these 70 (and whatever the previous
        // window left behind) is lost.
        assert!(lost.len() as u64 >= 70 - l.k_pkt(), "{}", lost.len());
        // The newest lost one is K + 1 departures behind; stay inside the FEC
        // distance of 20.
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

    /// The client acks a datagram it rebuilt from parity under its original
    /// `wire_seq`, which can only be after the group's parity left: at least
    /// `PARITY_INTERLEAVE_OFFSET` departures after it. That is a loss that
    /// happened, so it neither retracts nor adapts anything.
    #[test]
    fn a_loss_recovered_by_parity_does_not_raise_the_packet_threshold() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8192);
        // 12 windows, each: 70 sent, the last acked, and the OLDEST 17 verdicts
        // acked late: 50+ departures after their own.
        let mut base = 0u32;
        for _ in 0..12 {
            for ws in base..base + 70 {
                send(&mut l, ws, t0);
            }
            assert!(l.resolve(base + 69).is_some());
            let lost = l.detect_losses(FAR);
            for (ws, _, _) in lost.iter().take(17) {
                match l.resolve(*ws) {
                    Some(Resolution::Late { fec_possible, .. }) => assert!(fec_possible),
                    other => panic!("{other:?}"),
                }
            }
            base += 70;
        }
        assert_eq!(l.k_pkt(), 3, "parity recoveries are not reordering");
        assert_eq!(l.stats().k_pkt_raises, 0);
        assert!(l.stats().late_acks_fec_possible >= 12 * 17);
        // They are still counted as late acknowledgements.
        assert_eq!(l.stats().spurious_losses, l.stats().late_acks_fec_possible);
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
    }

    #[test]
    fn a_probe_that_finds_no_content_declares_the_transmission_lost() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        send(&mut l, 1, t0);
        assert_eq!(
            l.declare_lost_by_pto(1).map(|t| t.key),
            Some(EmitKey::new(1, 0, 0, 0))
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
}
