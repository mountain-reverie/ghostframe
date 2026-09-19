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
    pub emit_us: u32,
    pub wire_bytes: usize,
    /// Content identity, for the consumers that still think in tile-passes.
    pub key: EmitKey,
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
}

pub struct TransmissionLedger {
    records: HashMap<u32, (Instant, Transmission)>,
    /// Insertion order, so expiry and capacity eviction both walk oldest
    /// first without sorting.
    order: VecDeque<u32>,
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
    pub fn record(&mut self, wire_seq: u32, at: Instant, tx: Transmission) {
        if self.records.insert(wire_seq, (at, tx)).is_some() {
            self.stats.capacity_evictions += 1;
            self.order.retain(|w| *w != wire_seq);
        }
        self.order.push_back(wire_seq);
        while self.records.len() > self.capacity {
            match self.order.pop_front() {
                Some(oldest) => {
                    if self.records.remove(&oldest).is_some() {
                        self.stats.capacity_evictions += 1;
                    }
                }
                None => break,
            }
        }
    }

    /// Classify a transmission as received. Returns `None` if it is not
    /// outstanding and has no tombstone — already resolved, or the
    /// acknowledgement is unknown to this ledger entirely.
    ///
    /// Not retracting an expiry is deliberate: goog_cc has no retraction, and
    /// an acknowledgement arriving after the horizon is genuinely a late
    /// arrival rather than evidence the loss report was wrong. The tombstone
    /// still lets the content be released — see `Resolution::Late`.
    pub fn resolve(&mut self, wire_seq: u32) -> Option<Resolution> {
        if let Some((_, tx)) = self.records.remove(&wire_seq) {
            return Some(Resolution::Live(tx));
        }
        if let Some(tx) = self.tombstones.remove(&wire_seq) {
            self.stats.acked_after_declared_lost += 1;
            return Some(Resolution::Late(tx));
        }
        self.stats.unknown_acks += 1;
        None
    }

    /// Classify everything outstanding longer than `horizon` as lost.
    pub fn expire(&mut self, now: Instant, horizon: std::time::Duration) -> Vec<Transmission> {
        let mut lost = Vec::new();
        while let Some(front) = self.order.front().copied() {
            match self.records.get(&front) {
                // Already resolved; drop the stale order entry and continue.
                None => {
                    self.order.pop_front();
                }
                Some((at, _)) => {
                    if now.saturating_duration_since(*at) < horizon {
                        // Insertion order is send order, so nothing behind
                        // this is older.
                        break;
                    }
                    self.order.pop_front();
                    if let Some((_, tx)) = self.records.remove(&front) {
                        if crate::transport::reliable_emitter::rto_probe_enabled() {
                            eprintln!("RTOPROBE expired_ws ws={}", front);
                        }
                        self.tombstones.insert(front, tx.clone());
                        self.tombstone_order.push_back(front);
                        while self.tombstones.len() > self.capacity {
                            if let Some(oldest) = self.tombstone_order.pop_front() {
                                self.tombstones.remove(&oldest);
                            } else {
                                break;
                            }
                        }
                        lost.push(tx);
                    }
                }
            }
        }
        lost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tx(n: u32) -> Transmission {
        Transmission {
            emit_us: n * 1000,
            wire_bytes: 1200,
            key: EmitKey::new(n, 0, 0, 0),
        }
    }

    #[test]
    fn a_resolved_transmission_is_classified_once_and_not_expired() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(7, t0, tx(7));
        assert_eq!(l.resolve(7), Some(Resolution::Live(tx(7))));
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
        let lost = l.expire(t0 + Duration::from_millis(500), Duration::from_millis(100));
        assert_eq!(lost, vec![tx(7)]);
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
        assert_eq!(
            l.expire(t0 + Duration::from_millis(500), Duration::from_millis(100))
                .len(),
            1
        );
        assert_eq!(
            l.resolve(7),
            Some(Resolution::Late(tx(7))),
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
        // Insertion order is send order, so the walk must not scan the whole
        // ledger on every sweep.
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(64);
        l.record(1, t0, tx(1));
        l.record(2, t0 + Duration::from_millis(400), tx(2));
        let lost = l.expire(t0 + Duration::from_millis(500), Duration::from_millis(200));
        assert_eq!(lost, vec![tx(1)], "only the old one");
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
                        emit_us: ws,
                        wire_bytes: 1200,
                        key: EmitKey::new(ws, 0, 0, 0),
                    });
                    outstanding.insert(ws);
                    slot_ws.insert(slot, ws);
                }
                for tx in l.expire(now, horizon) {
                    let ws = tx.emit_us;
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
                let ws = tx.emit_us;
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
        l.record(99, t0, Transmission { emit_us: 10, wire_bytes: 500, key });

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
        l.record(99, t0, Transmission { emit_us: 10, wire_bytes: 500, key });
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
        l.record(99, t0, Transmission { emit_us: 10, wire_bytes: 500, key: EmitKey::new(7, 1, 2, 0) });
        let _ = l.expire(t0 + Duration::from_millis(300), Duration::from_millis(100));
        assert!(matches!(l.resolve(99), Some(Resolution::Late(_))));
        assert!(l.resolve(99).is_none(), "a duplicate ack must not resolve twice");
        assert_eq!(l.stats().acked_after_declared_lost, 1, "and must not double-count");
    }

    #[test]
    fn tombstones_are_bounded() {
        let t0 = Instant::now();
        let mut l = TransmissionLedger::new(8);
        for ws in 0..40u32 {
            l.record(ws, t0, Transmission { emit_us: 0, wire_bytes: 1, key: EmitKey::new(ws, 0, 0, 0) });
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
}
