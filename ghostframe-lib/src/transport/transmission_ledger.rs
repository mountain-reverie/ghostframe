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
}

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
    Late(Transmission),
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
    /// Transmissions declared lost while they had not left quinn's FIFO. Zero
    /// by construction: the loss clock starts at departure, so a queued
    /// transmission is never on it. Counted where a verdict is issued, from
    /// the record's own state, so a clock that starts early shows here.
    pub expired_before_departure: u64,
    /// Departures stamped on a record (one per transmission, at most).
    pub departures_stamped: u64,
    /// Transmissions evicted by the capacity cap before they had departed.
    pub evicted_before_departure: u64,
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
    tombstones: HashMap<u32, Transmission>,
    tombstone_order: VecDeque<u32>,
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
        }
    }

    pub fn stats(&self) -> LedgerStats {
        self.stats
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
            return Some(Resolution::Live(rec.tx));
        }
        if let Some(tx) = self.tombstones.remove(&wire_seq) {
            self.stats.acked_after_declared_lost += 1;
            return Some(Resolution::Late(tx));
        }
        self.stats.unknown_acks += 1;
        None
    }

    /// Classify everything that has been on the wire longer than `horizon`
    /// as lost.
    pub fn expire(&mut self, now: Instant, horizon: std::time::Duration) -> Vec<Transmission> {
        self.expire_with_seq(now, horizon)
            .into_iter()
            .map(|(_, tx)| tx)
            .collect()
    }

    /// [`Self::expire`], with each lost transmission's `wire_seq` so the
    /// caller can later match a late acknowledgement to it exactly.
    ///
    /// The clock is the departure stamp, not the recording time: a
    /// transmission that has not left quinn's FIFO is never on it. Departure
    /// order is the walk order, so it stops at the first transmission that
    /// has not been on the wire for `horizon` yet.
    pub fn expire_with_seq(
        &mut self,
        now: Instant,
        horizon: std::time::Duration,
    ) -> Vec<(u32, Transmission)> {
        let mut lost = Vec::new();
        while let Some(front) = self.departed_order.front().copied() {
            match self.records.get(&front) {
                // Already resolved, or the record was replaced by a newer one
                // under the same `wire_seq` that has not departed yet; drop
                // the stale order entry and continue.
                None => {
                    self.departed_order.pop_front();
                }
                Some(Record { departed: None, .. }) => {
                    self.departed_order.pop_front();
                }
                Some(Record {
                    departed: Some(at), ..
                }) => {
                    if now.saturating_duration_since(*at) < horizon {
                        break;
                    }
                    self.departed_order.pop_front();
                    self.declare_lost(front, &mut lost);
                }
            }
        }
        self.compact_if_sparse();
        lost
    }

    /// Issue the verdict for `wire_seq`: remove its record, keep a tombstone
    /// so a late acknowledgement can still release the content, and hand it
    /// to the caller. The one place a loss is declared.
    fn declare_lost(&mut self, wire_seq: u32, lost: &mut Vec<(u32, Transmission)>) {
        let Some(rec) = self.records.remove(&wire_seq) else {
            return;
        };
        if rec.departed.is_none() {
            self.stats.expired_before_departure += 1;
        }
        if crate::transport::reliable_emitter::rto_probe_enabled() {
            eprintln!("RTOPROBE expired_ws ws={}", wire_seq);
        }
        let tx = rec.tx;
        self.tombstones.insert(wire_seq, tx);
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
        lost.push((wire_seq, tx));
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

    fn tx(n: u32) -> Transmission {
        Transmission {
            emit_us: n as u64 * 1000,
            wire_bytes: 1200,
            key: EmitKey::new(n, 0, 0, 0),
            depart_us: None,
        }
    }

    /// `tx(n)` as the ledger hands it back once it has departed at `us`.
    fn departed(n: u32, us: u64) -> Transmission {
        Transmission {
            depart_us: Some(us),
            ..tx(n)
        }
    }

    #[test]
    fn a_resolved_transmission_is_classified_once_and_not_expired() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(7, t0, tx(7));
        assert!(l.mark_departed(7, t0, 5));
        assert_eq!(l.resolve(7), Some(Resolution::Live(departed(7, 5))));
        assert_eq!(l.resolve(7), None, "a second resolve finds nothing");
        assert!(
            l.expire(t0 + Duration::from_secs(10), Duration::from_millis(1))
                .is_empty(),
            "an acknowledged transmission must never also be reported lost"
        );
    }

    #[test]
    fn an_unresolved_transmission_expires_as_lost_once() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(7, t0, tx(7));
        l.mark_departed(7, t0, 5);
        let lost = l.expire(t0 + Duration::from_millis(500), Duration::from_millis(100));
        assert_eq!(lost, vec![departed(7, 5)]);
        assert!(
            l.expire(t0 + Duration::from_secs(10), Duration::from_millis(1))
                .is_empty(),
            "expiry must not report the same transmission twice"
        );
    }

    /// A late acknowledgement is visible and does not retract the loss --
    /// but it does release the content.
    ///
    /// Previously this asserted `resolve` returned `None`, which made the
    /// acknowledgement countable while leaving its cache entry unreachable
    /// forever. Visibility and non-retraction were the point and are kept;
    /// the stranding was not, and is gone.
    #[test]
    fn an_acknowledgement_after_expiry_is_counted_and_releases_content() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(7, t0, tx(7));
        l.mark_departed(7, t0, 5);
        assert_eq!(
            l.expire(t0 + Duration::from_millis(500), Duration::from_millis(100))
                .len(),
            1
        );
        assert_eq!(
            l.resolve(7),
            Some(Resolution::Late(departed(7, 5))),
            "the content must still be releasable, or its cache entry is stranded"
        );
        assert_eq!(
            l.stats().acked_after_declared_lost,
            1,
            "a late acknowledgement is visible, not silent"
        );
        assert_eq!(
            l.stats().unknown_acks,
            0,
            "it is no longer an unknown acknowledgement -- we know exactly what it was"
        );
    }

    #[test]
    fn expiry_stops_at_the_first_young_record() {
        // Departure order is send order, so the walk must not scan the whole
        // ledger on every sweep.
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(1, t0, tx(1));
        l.record(2, t0, tx(2));
        l.mark_departed(1, t0, 1);
        l.mark_departed(2, t0 + Duration::from_millis(400), 2);
        let lost = l.expire(t0 + Duration::from_millis(500), Duration::from_millis(200));
        assert_eq!(lost, vec![departed(1, 1)], "only the old one");
        assert_eq!(l.len(), 1);
    }

    // The invariant the whole design rests on, and the one most likely to
    // break under an ordering nobody thought of: across any interleaving of
    // recording, resolving and expiring, a transmission is classified
    // **exactly once** — never both received and lost, never neither.
    //
    // Capacity is deliberately left larger than the operation count here.
    // The hard cap is a backstop against a pathological peer and drops
    // records *unclassified* by design; mixing it into this property would
    // assert something the cap explicitly does not promise. It has its own
    // test above.
    proptest::proptest! {
        #[test]
        fn every_transmission_is_classified_exactly_once(
            ops in proptest::collection::vec(
                (0u32..12, proptest::bool::ANY, 0u64..300), 1..200)
        ) {
            use std::collections::HashSet;
            let t0 = Instant::now();
            let horizon = Duration::from_millis(100);
            let mut l = TransmissionLedger::new(4096);
            let mut outstanding: HashSet<u32> = HashSet::new();
            let mut classified: HashSet<u32> = HashSet::new();
            // wire_seq is unique per transmission, so a fresh one each time.
            let mut next_ws: u32 = 0;
            // Maps the proptest slot back to whichever wire_seq is currently
            // outstanding for it, so "resolve something plausible" can name a
            // real record as well as a stale one.
            let mut slot_ws: std::collections::HashMap<u32, u32> = Default::default();
            let mut now = t0;

            for (slot, do_resolve, advance_us) in ops {
                now += Duration::from_micros(advance_us);
                if do_resolve {
                    if let Some(ws) = slot_ws.remove(&slot) {
                        if l.resolve(ws).is_some() {
                            proptest::prop_assert!(
                                outstanding.remove(&ws),
                                "resolved a transmission that was not outstanding"
                            );
                            proptest::prop_assert!(
                                classified.insert(ws),
                                "wire_seq {} classified twice", ws
                            );
                        }
                    }
                } else {
                    let ws = next_ws;
                    next_ws += 1;
                    l.record(ws, now, Transmission {
                        emit_us: ws as u64,
                        wire_bytes: 1200,
                        key: EmitKey::new(ws, 0, 0, 0),
                        depart_us: None,
                    });
                    l.mark_departed(ws, now, ws as u64);
                    outstanding.insert(ws);
                    slot_ws.insert(slot, ws);
                }
                for tx in l.expire(now, horizon) {
                    let ws = tx.emit_us as u32;
                    proptest::prop_assert!(
                        outstanding.remove(&ws),
                        "expired a transmission that was not outstanding"
                    );
                    proptest::prop_assert!(
                        classified.insert(ws),
                        "wire_seq {} classified twice", ws
                    );
                    slot_ws.retain(|_, v| *v != ws);
                }
            }

            // Drain whatever is still in flight; nothing may be left unknown.
            for tx in l.expire(now + horizon * 2, horizon) {
                let ws = tx.emit_us as u32;
                proptest::prop_assert!(outstanding.remove(&ws));
                proptest::prop_assert!(classified.insert(ws));
            }
            proptest::prop_assert!(
                outstanding.is_empty(),
                "{} transmissions were never classified", outstanding.len()
            );
            proptest::prop_assert_eq!(classified.len(), next_ws as usize);
        }
    }

    #[test]
    fn a_late_acknowledgement_still_resolves_after_expiry() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        let key = EmitKey::new(7, 1, 2, 0);
        l.record(
            99,
            t0,
            Transmission {
                emit_us: 10,
                wire_bytes: 500,
                key,
                depart_us: None,
            },
        );
        l.mark_departed(99, t0, 10);

        let lost = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        assert_eq!(lost.len(), 1, "the transmission is declared lost");

        match l.resolve(99) {
            Some(Resolution::Late(tx)) => assert_eq!(tx.key, key),
            other => panic!("a late ack must still resolve to its content, got {other:?}"),
        }
        assert_eq!(l.stats().acked_after_declared_lost, 1);
    }

    #[test]
    fn a_live_acknowledgement_resolves_as_live() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        let key = EmitKey::new(7, 1, 2, 0);
        l.record(
            99,
            t0,
            Transmission {
                emit_us: 10,
                wire_bytes: 500,
                key,
                depart_us: None,
            },
        );
        match l.resolve(99) {
            Some(Resolution::Live(tx)) => assert_eq!(tx.key, key),
            other => panic!("expected Live, got {other:?}"),
        }
        assert_eq!(l.stats().acked_after_declared_lost, 0);
    }

    #[test]
    fn a_tombstone_resolves_only_once() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(
            99,
            t0,
            Transmission {
                emit_us: 10,
                wire_bytes: 500,
                key: EmitKey::new(7, 1, 2, 0),
                depart_us: None,
            },
        );
        l.mark_departed(99, t0, 10);
        let _ = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        assert!(matches!(l.resolve(99), Some(Resolution::Late(_))));
        assert!(
            l.resolve(99).is_none(),
            "a duplicate ack must not resolve twice"
        );
        assert_eq!(
            l.stats().acked_after_declared_lost,
            1,
            "and must not double-count"
        );
    }

    #[test]
    fn tombstones_are_bounded() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8);
        for ws in 0..40u32 {
            l.record(
                ws,
                t0,
                Transmission {
                    emit_us: 0,
                    wire_bytes: 1,
                    key: EmitKey::new(ws, 0, 0, 0),
                    depart_us: None,
                },
            );
            l.mark_departed(ws, t0, 0);
            let _ = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        }
        assert!(
            l.tombstone_len() <= 8,
            "tombstones must be bounded by capacity; got {}",
            l.tombstone_len()
        );
        // The most recent expiry must still be resolvable -- eviction drops
        // the oldest, which is the one least likely to still be in flight.
        assert!(matches!(l.resolve(39), Some(Resolution::Late(_))));
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

    #[test]
    fn a_transmission_that_has_not_departed_is_never_declared_lost() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(1, t0, tx(1));
        l.record(2, t0, tx(2));
        let lost = l.expire(t0 + Duration::from_secs(3600), Duration::from_millis(1));
        assert!(lost.is_empty(), "queued transmissions are not on the clock");
        assert_eq!(l.stats().expired_before_departure, 0);
        assert_eq!(l.len(), 2, "and they are still outstanding");
    }

    #[test]
    fn the_loss_clock_starts_at_departure_not_at_recording() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(1, t0, tx(1));
        // Sat in a queue for 400 ms, then left.
        let left = t0 + Duration::from_millis(400);
        assert!(l.mark_departed(1, left, 400_000));
        let horizon = Duration::from_millis(200);
        assert!(
            l.expire(t0 + Duration::from_millis(500), horizon)
                .is_empty(),
            "100 ms on the wire is inside the 200 ms horizon, however long it queued"
        );
        let lost = l.expire(t0 + Duration::from_millis(601), horizon);
        assert_eq!(lost, vec![departed(1, 400_000)]);
        assert_eq!(l.stats().expired_before_departure, 0);
    }

    #[test]
    fn departure_is_stamped_once_and_only_for_a_known_record() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        assert!(!l.mark_departed(9, t0, 1), "unknown wire_seq");
        l.record(9, t0, tx(9));
        assert!(l.mark_departed(9, t0, 1));
        assert!(!l.mark_departed(9, t0 + Duration::from_secs(1), 2), "twice");
        assert_eq!(l.departed(9), Some(true));
        assert_eq!(l.stats().departures_stamped, 1);
        assert!(matches!(
            l.resolve(9),
            Some(Resolution::Live(Transmission {
                depart_us: Some(1),
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
            "recording order grew to {}",
            l.order.len()
        );
        assert!(
            l.departed_order.len() <= l.len() + 2 * cap + 64 + 1,
            "departure order grew to {}",
            l.departed_order.len()
        );
    }
}
