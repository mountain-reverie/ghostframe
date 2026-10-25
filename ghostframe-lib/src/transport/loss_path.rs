//! The loss path of D1 F4 part 2, as a function of the two things it joins:
//! the transmission ledger (what is outstanding, in departure order) and the
//! reliable emitter (what content is still current, and the probe timer).
//!
//! Kept apart from `IoBridge` so the scripted-path tests drive exactly the code
//! production runs.

use crate::transport::reliable_emitter::{ReliableTileEmitter, RetxCause, RetxRequest};
use crate::transport::rtt_estimator::RttEstimator;
use crate::transport::transmission_ledger::{Transmission, TransmissionLedger};
use std::time::{Duration, Instant};

/// Transmissions a probe sends.
pub(crate) const PROBE_COUNT: usize = 2;
/// Records looked at from the back of the departure order when probing.
pub(crate) const PROBE_SCAN: usize = 32;

/// RFC 9002 section 6.1.2: a transmission that left more than
/// `9/8 x max(srtt, latest_rtt)` before the newest acknowledged departure is
/// lost. The estimator is the emitter's own (departure-based, application
/// acknowledgements), not quinn's: the acknowledgement being waited for is the
/// application's.
pub(crate) fn time_threshold(rtt: &RttEstimator) -> Duration {
    rtt.srtt().max(rtt.latest_rtt()) * 9 / 8
}

/// What a pass of the loss path decided.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    /// Transmissions declared lost, to be reported to the estimators.
    pub lost: Vec<(u32, Transmission)>,
    /// Retransmissions queued on the emitter.
    pub retx_queued: u64,
}

/// Declare lost what the acknowledgements so far put out of contention (packet
/// and time threshold), and queue one retransmission for each whose content is
/// still current.
pub(crate) fn detect(
    ledger: &mut TransmissionLedger,
    emitter: &mut ReliableTileEmitter,
    now: Instant,
) -> Outcome {
    let thr = time_threshold(&emitter.rtt);
    let mut out = Outcome::default();
    for (ws, tx, _reason) in ledger.detect_losses_at(now, thr) {
        if emitter.request_retransmit(tx.key, ws, RetxCause::Loss) == RetxRequest::Queued {
            out.retx_queued += 1;
        }
        out.lost.push((ws, tx));
    }
    out
}

/// The probe timeout as the tail probe. One timer per connection, not per
/// pass: when it expires, the newest one or two outstanding transmissions are
/// sent again, never the whole set. That draws an acknowledgement, and the
/// acknowledgement lets threshold loss find whatever else was lost.
///
/// A transmission whose content is gone (acknowledged through another
/// transmission, or superseded) has nothing to send and is declared lost on the
/// spot, so a dead tail cannot occupy the probe forever. One whose pass has a
/// newer transmission in flight is skipped: that one is what the next
/// acknowledgement is waiting for.
///
/// The timer restarts at every expiry even when nothing was probed, so a timer
/// with no candidates does not spin. Returns `None` while it is not due.
pub(crate) fn probe(
    ledger: &mut TransmissionLedger,
    emitter: &mut ReliableTileEmitter,
    now: Instant,
) -> Option<Outcome> {
    if !emitter.pto_due(now) {
        return None;
    }
    emitter.note_pto_fire(now);
    let mut out = Outcome::default();
    // Transmissions whose content is gone are declared lost and leave the
    // ledger, so they do not count against the scan: look again from the new
    // back until a pass over `PROBE_SCAN` records removes nothing or two
    // probes are queued. Each removal is O(1) and happens once.
    'rounds: loop {
        let candidates = ledger.newest_outstanding(PROBE_SCAN, PROBE_SCAN);
        if candidates.is_empty() {
            break;
        }
        let mut removed = false;
        for (ws, tx) in candidates {
            if out.retx_queued as usize >= PROBE_COUNT {
                break 'rounds;
            }
            match emitter.request_retransmit(tx.key, ws, RetxCause::Probe) {
                RetxRequest::Queued => out.retx_queued += 1,
                RetxRequest::ContentGone => {
                    if let Some(tx) = ledger.declare_lost_by_pto(ws) {
                        out.lost.push((ws, tx));
                        removed = true;
                    }
                }
                RetxRequest::NewerInFlight
                | RetxRequest::AlreadyQueued
                | RetxRequest::Deferred
                | RetxRequest::Disabled => {}
            }
        }
        if !removed {
            break;
        }
    }
    // Passes a declined verdict left to the probe: their newest transmission is
    // already tombstoned, so the scan above cannot see them.
    for (key, ws) in emitter.take_deferred(PROBE_COUNT) {
        if emitter.request_retransmit(key, ws, RetxCause::Probe) == RetxRequest::Queued {
            out.retx_queued += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::reliable_emitter::EmitKey;
    use crate::transport::transmission_ledger::Transmission;
    use bytes::Bytes;

    fn pass(i: u32) -> Bytes {
        let mut v = vec![0u8; 25];
        v[0..4].copy_from_slice(&(0x8000_0000 | i).to_be_bytes());
        v[6..8].copy_from_slice(&1u16.to_be_bytes());
        v[20..24].copy_from_slice(&1u32.to_be_bytes());
        Bytes::from(v)
    }

    /// `n` passes sent and departed at `t0`, in the emitter and the ledger.
    fn sent(n: u32, t0: Instant) -> (ReliableTileEmitter, TransmissionLedger, Vec<(EmitKey, u32)>) {
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let mut l = TransmissionLedger::new(4096);
        let mut sent = Vec::new();
        for i in 0..n {
            let key = EmitKey::new(i, 0, 0, 0);
            e.submit_one(key, pass(i), t0, None, t0);
            for em in e.take_transmissions() {
                l.record(
                    em.wire_seq,
                    t0,
                    Transmission {
                        emit_us: 0,
                        wire_bytes: em.wire_bytes,
                        key,
                        depart_us: None,
                        dep_idx: None,
                    },
                );
                l.mark_departed(em.wire_seq, t0, 0);
                e.on_departed(em.wire_seq, t0);
                sent.push((key, em.wire_seq));
            }
        }
        (e, l, sent)
    }

    /// Records whose content is gone are removed as the probe meets them and do
    /// not use up its scan: a live pass behind 100 of them is still probed.
    #[test]
    fn a_dead_tail_longer_than_the_scan_does_not_hide_the_live_pass_behind_it() {
        let t0 = Instant::now();
        let (mut e, mut l, sent) = sent(104, t0);
        // The newest 100 were acknowledged through another transmission or
        // superseded: their cache entries are gone, the ledger records stay.
        for (key, _) in &sent[4..] {
            e.on_ack(&[*key]);
        }
        let due = e.pto_deadline().expect("running");
        let out = probe(&mut l, &mut e, due).expect("due");
        assert_eq!(out.lost.len(), 100, "the dead tail is declared lost");
        assert_eq!(out.retx_queued, 2, "and the live passes behind it probed");
        const { assert!(100 > PROBE_SCAN) };
    }

    /// A probe that queues nothing still restarts the timer, backed off: it
    /// neither spins nor repeats at the same interval.
    #[test]
    fn a_probe_that_queues_nothing_still_backs_off() {
        let t0 = Instant::now();
        let (mut e, mut l, sent) = sent(2, t0);
        // Both passes have a newer transmission in flight (a NACK repair each),
        // so the probe has nothing to send.
        for (key, ws) in &sent {
            e.on_nack(&[*key], t0);
            let _ = ws;
        }
        let mut n = 0;
        e.tick_nacks_budgeted(t0, usize::MAX, usize::MAX, &mut n);
        let first = e.pto_deadline().expect("running");
        let out = probe(&mut l, &mut e, first).expect("due");
        assert_eq!(out.retx_queued, 0, "premise: nothing queued");
        let second = e.pto_deadline().expect("restarted");
        assert!(second > first, "the timer restarted");
        let interval1 = second.duration_since(first);
        let out = probe(&mut l, &mut e, second).expect("due again");
        assert_eq!(out.retx_queued, 0);
        let third = e.pto_deadline().unwrap();
        assert!(
            third.duration_since(second) > interval1,
            "and backs off: {:?} then {:?}",
            interval1,
            third.duration_since(second)
        );
    }
}

#[cfg(test)]
mod deferred_tests {
    use super::*;
    use crate::transport::reliable_emitter::emitter::LOSS_RETX_MAX_ATTEMPTS;
    use crate::transport::reliable_emitter::EmitKey;
    use crate::transport::transmission_ledger::Transmission;
    use bytes::Bytes;

    /// A pass lost four times running: the verdict on its last transmission is
    /// declined (it has been resent `LOSS_RETX_MAX_ATTEMPTS` times) and that
    /// transmission is tombstoned, so the ledger shows nothing outstanding for
    /// it. The probe must still find the pass, or it waits for the 30 s
    /// backstop (the seed 0x22 burst scene left 24 passes owed that way).
    #[test]
    fn a_pass_whose_verdict_was_declined_is_still_found_by_the_probe() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let mut l = TransmissionLedger::new(256);
        let key = EmitKey::new(1, 0, 0, 0);
        let mut v = vec![0u8; 25];
        v[0..4].copy_from_slice(&(0x8000_0000u32 | 1).to_be_bytes());
        v[6..8].copy_from_slice(&1u16.to_be_bytes());
        e.submit_one(key, Bytes::from(v), t0, None, t0);
        let book = |e: &mut ReliableTileEmitter, l: &mut TransmissionLedger| {
            for em in e.take_transmissions() {
                l.record(
                    em.wire_seq,
                    t0,
                    Transmission {
                        emit_us: 0,
                        wire_bytes: em.wire_bytes,
                        key,
                        depart_us: None,
                        dep_idx: None,
                    },
                );
                l.mark_departed(em.wire_seq, t0, 0);
                e.on_departed(em.wire_seq, t0);
            }
        };
        book(&mut e, &mut l);
        // Resent by verdicts until the limit, each transmission then declared
        // lost in turn.
        for _ in 0..LOSS_RETX_MAX_ATTEMPTS {
            let ws = e.cache.get(&key).unwrap().latest_ws;
            assert_eq!(l.declare_lost_by_pto(ws).map(|t| t.key), Some(key));
            assert_eq!(
                e.request_retransmit(key, ws, RetxCause::Loss),
                RetxRequest::Queued
            );
            e.tick(t0, usize::MAX);
            book(&mut e, &mut l);
        }
        let ws = e.cache.get(&key).unwrap().latest_ws;
        l.declare_lost_by_pto(ws).expect("outstanding");
        assert_eq!(
            e.request_retransmit(key, ws, RetxCause::Loss),
            RetxRequest::Deferred
        );
        assert_eq!(
            l.newest_outstanding(8, 8).len(),
            0,
            "premise: nothing outstanding"
        );

        let due = e.pto_deadline().expect("running");
        let out = probe(&mut l, &mut e, due).expect("due");
        assert_eq!(out.retx_queued, 1, "the probe resends the deferred pass");
        let mut n = 0;
        e.tick_loss_budgeted(due, usize::MAX, usize::MAX, &mut n);
        assert_eq!(e.stats.pto_probe_retransmits, 1);
    }
}
