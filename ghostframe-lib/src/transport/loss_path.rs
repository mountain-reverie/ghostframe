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

/// Transmissions a probe sends. The same number bounds the deferred passes
/// resent in one turn, but those are a separate allowance: a probe is the
/// connection's one timer, a deferred pass has its own.
pub(crate) const PROBE_COUNT: usize = 2;
/// Records looked at from the back of the departure order when probing.
pub(crate) const PROBE_SCAN: usize = 32;
/// Rounds of `PROBE_SCAN` records a probe looks through while records keep
/// being removed (content gone): 1,024 records at most.
pub(crate) const PROBE_MAX_ROUNDS: usize = 32;

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
/// still current. Also resends the passes earlier verdicts declined whose own
/// backed-off deadline has come (at most `PROBE_COUNT` a turn), so they do not
/// wait for the connection's probe timer, which acknowledgements keep from
/// expiring.
pub(crate) fn detect(
    ledger: &mut TransmissionLedger,
    emitter: &mut ReliableTileEmitter,
    now: Instant,
) -> Outcome {
    let thr = time_threshold(&emitter.rtt);
    let mut out = Outcome::default();
    for (ws, tx, _reason) in ledger.detect_losses_at(now, thr) {
        if emitter.request_retransmit(tx.content, ws, RetxCause::Loss) == RetxRequest::Queued {
            out.retx_queued += 1;
        }
        out.lost.push((ws, tx));
    }
    for (key, ws) in emitter.take_due_deferred(now, PROBE_COUNT) {
        if emitter.request_retransmit(key, ws, RetxCause::Probe) == RetxRequest::Queued {
            out.retx_queued += 1;
        }
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
    // ledger, so they do not count against the scan: look again until a round
    // removes nothing or two probes are queued. A positional cursor into the
    // ledger's departure order keeps a round from reading an entry twice, and
    // `PROBE_MAX_ROUNDS` bounds the whole.
    let mut cursor = 0usize;
    'rounds: for _ in 0..PROBE_MAX_ROUNDS {
        let candidates = ledger.scan_outstanding(&mut cursor, PROBE_SCAN, PROBE_SCAN);
        if candidates.is_empty() {
            break;
        }
        let mut removed = false;
        for (ws, tx) in candidates {
            if out.retx_queued as usize >= PROBE_COUNT {
                break 'rounds;
            }
            match emitter.request_retransmit(tx.content, ws, RetxCause::Probe) {
                RetxRequest::Queued => {
                    out.retx_queued += 1;
                }
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
                        content: key.into(),
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
    use crate::transport::reliable_emitter::pto::pto_for_attempt;
    use crate::transport::reliable_emitter::EmitKey;
    use crate::transport::transmission_ledger::Transmission;
    use bytes::Bytes;

    fn pass(i: u32) -> Bytes {
        let mut v = vec![0u8; 25];
        v[0..4].copy_from_slice(&(0x8000_0000u32 | i).to_be_bytes());
        v[6..8].copy_from_slice(&1u16.to_be_bytes());
        Bytes::from(v)
    }

    /// Put what the emitter sent on the books, departed at `at`.
    fn book(e: &mut ReliableTileEmitter, l: &mut TransmissionLedger, at: Instant) {
        for em in e.take_transmissions() {
            l.record(
                em.wire_seq,
                at,
                Transmission {
                    emit_us: 0,
                    wire_bytes: em.wire_bytes,
                    content: em.content,
                    depart_us: None,
                    dep_idx: None,
                },
            );
            l.mark_departed(em.wire_seq, at, 0);
            e.on_departed(em.wire_seq, at);
        }
    }

    /// `key` sent at `t`, then lost and resent by verdicts `lost_times` times,
    /// each resend a second apart. Returns the clock after the last.
    fn lost_n_times(
        e: &mut ReliableTileEmitter,
        l: &mut TransmissionLedger,
        key: EmitKey,
        lost_times: u8,
        t0: Instant,
    ) -> Instant {
        let mut t = t0;
        e.submit_one(key, pass(key.frame_seq), t, None, t);
        book(e, l, t);
        for _ in 0..lost_times {
            let ws = e.cache.get(&key).unwrap().latest_ws;
            assert_eq!(
                l.declare_lost_by_pto(ws).and_then(|x| x.content.tile()),
                Some(key)
            );
            assert_eq!(
                e.request_retransmit(key, ws, RetxCause::Loss),
                RetxRequest::Queued
            );
            t += Duration::from_secs(1);
            e.tick(t, usize::MAX);
            book(e, l, t);
        }
        t
    }

    /// A pass lost four times running: the verdict on its last transmission is
    /// declined (it has been resent `LOSS_RETX_MAX_ATTEMPTS` times) and that
    /// transmission is tombstoned, so the ledger shows nothing outstanding for
    /// it. It is resent at its own backed-off deadline, found by `detect`, while
    /// acknowledgements of other passes keep arriving, so the connection's probe
    /// timer never expires. Before this it waited for that timer (or the 30 s
    /// backstop): the seed 0x22 burst scene left 24 passes owed that way.
    #[test]
    fn a_deferred_pass_is_resent_at_its_own_deadline_while_acknowledgements_flow() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let mut l = TransmissionLedger::new(256);
        let key = EmitKey::new(1, 0, 0, 0);
        let mut t = lost_n_times(&mut e, &mut l, key, LOSS_RETX_MAX_ATTEMPTS, t0);
        let ws = e.cache.get(&key).unwrap().latest_ws;
        l.declare_lost_by_pto(ws).expect("outstanding");
        assert_eq!(
            e.request_retransmit(key, ws, RetxCause::Loss),
            RetxRequest::Deferred
        );
        assert_eq!(
            l.newest_outstanding(8, 8).len(),
            0,
            "premise: nothing outstanding for the probe to find"
        );
        let sent_at = e.cache.get(&key).unwrap().last_sent_at;
        let first_interval = pto_for_attempt(&e.rtt, LOSS_RETX_MAX_ATTEMPTS);
        let due = e.next_deferred_due().expect("deferred");
        assert_eq!(due, sent_at + first_interval);

        // Acknowledgements of other traffic keep arriving every 10 ms: the
        // connection's probe timer, which restarts at each, never expires.
        let mut probe_fired = false;
        let mut queued_at = None;
        while t < due + Duration::from_millis(50) {
            t += Duration::from_millis(10);
            e.note_ack(t);
            if e.pto_due(t) {
                probe_fired = true;
            }
            let out = detect(&mut l, &mut e, t);
            if out.retx_queued > 0 && queued_at.is_none() {
                queued_at = Some(t);
            }
        }
        assert!(
            !probe_fired,
            "premise: acknowledgements kept the probe quiet"
        );
        let queued_at = queued_at.expect("the deferred pass was resent");
        assert!(
            queued_at >= due && queued_at < due + Duration::from_millis(20),
            "at its deadline ({due:?}), not before or long after: {queued_at:?}"
        );
        // Sent, and lost again: the next deferral backs off.
        e.tick(queued_at, usize::MAX);
        book(&mut e, &mut l, queued_at);
        let ws = e.cache.get(&key).unwrap().latest_ws;
        l.declare_lost_by_pto(ws).expect("outstanding");
        assert_eq!(
            e.request_retransmit(key, ws, RetxCause::Loss),
            RetxRequest::Deferred
        );
        let second_interval = e.next_deferred_due().unwrap() - queued_at;
        assert!(
            second_interval > first_interval,
            "{first_interval:?} then {second_interval:?}"
        );
    }

    /// A client asking for a pass again says nothing about the path losing it:
    /// NACK repairs do not use up the loss path's allowance.
    #[test]
    fn nack_repairs_do_not_count_toward_giving_up_on_verdicts() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let mut l = TransmissionLedger::new(256);
        let key = EmitKey::new(1, 0, 0, 0);
        e.submit_one(key, pass(1), t0, None, t0);
        book(&mut e, &mut l, t0);
        for _ in 0..LOSS_RETX_MAX_ATTEMPTS + 2 {
            e.on_nack(&[key], t0);
            let mut n = 0;
            e.tick_nacks_budgeted(t0, usize::MAX, usize::MAX, &mut n);
            book(&mut e, &mut l, t0);
        }
        assert!(e.cache.get(&key).unwrap().attempts > LOSS_RETX_MAX_ATTEMPTS);
        let ws = e.cache.get(&key).unwrap().latest_ws;
        assert_eq!(
            e.request_retransmit(key, ws, RetxCause::Loss),
            RetxRequest::Queued,
            "five NACK repairs, no loss-path resend: a verdict still resends"
        );
    }

    /// The entry names the transmission judged. If anything resends the pass
    /// first, the entry is moot and dropped.
    #[test]
    fn a_deferred_entry_is_dropped_when_the_pass_has_been_resent_or_acknowledged() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(Duration::from_millis(40));
        let mut l = TransmissionLedger::new(256);
        let defer = |e: &mut ReliableTileEmitter, l: &mut TransmissionLedger, k: EmitKey| {
            let t = lost_n_times(e, l, k, LOSS_RETX_MAX_ATTEMPTS, t0);
            let ws = e.cache.get(&k).unwrap().latest_ws;
            l.declare_lost_by_pto(ws).unwrap();
            assert_eq!(
                e.request_retransmit(k, ws, RetxCause::Loss),
                RetxRequest::Deferred
            );
            t
        };
        let a = EmitKey::new(1, 0, 0, 0);
        let b = EmitKey::new(2, 1, 0, 0);
        let c = EmitKey::new(3, 2, 0, 0);
        let t = defer(&mut e, &mut l, a);
        defer(&mut e, &mut l, b);
        defer(&mut e, &mut l, c);
        assert_eq!(e.deferred_len(), 3);
        // a is repaired on the client's request, b is acknowledged, c waits.
        e.on_nack(&[a], t);
        let mut n = 0;
        e.tick_nacks_budgeted(t, usize::MAX, usize::MAX, &mut n);
        e.on_ack(&[b]);
        let far = t + Duration::from_secs(60);
        let due = e.take_due_deferred(far, 8);
        assert_eq!(
            due.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            vec![c],
            "a was resent since the verdict, b is acknowledged"
        );
        assert_eq!(
            e.deferred_len(),
            0,
            "the moot ones were dropped as they surfaced"
        );
    }
}
