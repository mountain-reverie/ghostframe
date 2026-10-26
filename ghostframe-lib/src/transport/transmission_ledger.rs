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
    /// only: loss is judged on departure (`departed`, `dep_idx`).
    #[allow(dead_code)]
    recorded_at: Instant,
    /// When it left quinn's FIFO. `None` until then.
    departed: Option<Instant>,
    /// The delivery-rate mark taken at departure (D1 PR-6b). Observability.
    rate: Option<RateMark>,
    tx: Transmission,
}

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
    /// The newest acknowledged departure: its index and when it left. Loss
    /// is only ever judged against this, never against the clock, so a queue
    /// that delays everything in order cannot produce a verdict (F4).
    largest_acked: Option<u64>,
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
            largest_acked: None,
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

    pub fn stats(&self) -> LedgerStats {
        LedgerStats {
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
            if let (Some(idx), Some(_)) = (rec.tx.dep_idx, rec.departed) {
                if self.largest_acked.is_none_or(|l| idx > l) {
                    self.largest_acked = Some(idx);
                }
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
            if let (Some(idx), Some(_)) = (tx.dep_idx, t.departed) {
                if self.largest_acked.is_none_or(|l| idx > l) {
                    self.largest_acked = Some(idx);
                }
            }
            let fec_possible = self.fec_possible(wire_seq, ack);
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
}
