//! A scripted bottleneck against the whole loss path (D1 F4 part 2): the
//! emitter, the transmission ledger and `loss_path`, the code production runs.
//!
//! The path is a FIFO server that delivers in order, with an optional capacity
//! drop (a queue that builds without losing anything) and optional drops. The
//! sender sees what the real one sees: a departure for each datagram, and
//! acknowledgements that feed the ledger, the cache and the RTT estimator.

use super::emitter::ReliableTileEmitter;
use super::EmitKey;
use crate::transport::loss_path;
use crate::transport::transmission_ledger::{Resolution, Transmission, TransmissionLedger};
use bytes::Bytes;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

pub(super) fn source(seq: u32) -> Bytes {
    let mut v = vec![0u8; 25];
    v[0..4].copy_from_slice(&(0x8000_0000 | seq).to_be_bytes());
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v[20..24].copy_from_slice(&1u32.to_be_bytes());
    Bytes::from(v)
}

/// What a run of the script produced.
#[derive(Debug, Clone, Default)]
pub(super) struct Run {
    pub sent: u64,
    /// Distinct passes whose content was acknowledged.
    pub passes_acked: u64,
    pub dropped: u64,
    /// Transmissions declared lost (all reasons).
    pub verdicts: u64,
    pub verdicts_packet: u64,
    pub verdicts_time: u64,
    pub verdicts_pto: u64,
    pub spurious_verdicts: u64,
    pub loss_retx: u64,
    pub probe_retx: u64,
    pub pto_fires: u64,
    pub k_pkt: u64,
    /// Time of the first retransmission after the first drop, from the drop.
    pub first_repair_after_drop: Option<Duration>,
    /// When the last pass was acknowledged, from the start.
    pub finished_at: Duration,
    pub max_rtt: Duration,
    pub max_queue: Duration,
    pub max_pto: Duration,
}

/// The scripted path: one datagram every `send_every` into a FIFO bottleneck
/// that serves one in `service_before` until `switch_at` and one in
/// `service_after` from then on, then `base_rtt` of propagation. A datagram that
/// arrives while the queue is non-empty waits its turn, so the queue grows at
/// `service / send_every - 1` seconds per second whenever the offered rate
/// exceeds the service rate, and everything is delivered in departure order.
#[derive(Debug, Clone, Copy)]
pub(super) struct Script {
    pub send_every: Duration,
    pub base_rtt: Duration,
    pub service_before: Duration,
    pub service_after: Duration,
    pub switch_at: Duration,
    /// Sending stops at the first datagram that queues this long.
    pub stop_at_queue_delay: Duration,
    /// Also drop every n-th original (never delivered, takes no service).
    pub drop_every: Option<u32>,
    /// Stop sending after this many originals, if set (and drop the last
    /// `drop_last` of them).
    pub max_sends: Option<u32>,
    pub drop_last: u32,
    /// Give the run this long to finish after the last send.
    pub settle: Duration,
}

impl Script {
    /// 100 datagrams/s into a 200/s bottleneck that drops to `service_after`
    /// at 2 s: the queue then grows at `service_after / send_every - 1`
    /// seconds of delay per second.
    pub(super) fn capacity_drop(service_after: Duration) -> Self {
        Self {
            send_every: Duration::from_millis(10),
            base_rtt: Duration::from_millis(40),
            service_before: Duration::from_millis(5),
            service_after,
            switch_at: Duration::from_secs(2),
            stop_at_queue_delay: Duration::from_secs(1),
            drop_every: None,
            max_sends: None,
            drop_last: 0,
            settle: Duration::from_secs(8),
        }
    }

    /// A clean 5 ms-service path (no queue at 100/s), `n` sends.
    pub(super) fn steady(n: u32) -> Self {
        Self {
            switch_at: Duration::from_secs(3600),
            stop_at_queue_delay: Duration::from_secs(3600),
            max_sends: Some(n),
            ..Self::capacity_drop(Duration::from_millis(5))
        }
    }
}

pub(super) fn run_script(sc: Script) -> Run {
    let t0 = Instant::now();
    let mut e = ReliableTileEmitter::new(t0);
    let mut ledger = TransmissionLedger::new(16_384);
    // A session that has carried one acknowledgement already: the first
    // sample is the path's own round trip.
    e.rtt.on_sample(sc.base_rtt);
    let step = Duration::from_millis(1);

    // (ack_due, wire_seq, departed)
    let mut in_flight: VecDeque<(Duration, u32, Duration)> = VecDeque::new();
    let mut run = Run::default();
    let mut passes_acked = std::collections::HashSet::new();
    let mut t = Duration::ZERO;
    let mut next_send = Duration::ZERO;
    let mut sending = true;
    let mut server_free = Duration::ZERO;
    let mut seq = 0u32;
    let mut t_stop = Duration::ZERO;
    let mut first_drop_at: Option<Duration> = None;
    let mut originals_total = 0u32;

    // Put every transmission the emitter built since the last call on the wire.
    let put_on_wire = |e: &mut ReliableTileEmitter,
                       ledger: &mut TransmissionLedger,
                       in_flight: &mut VecDeque<(Duration, u32, Duration)>,
                       server_free: &mut Duration,
                       run: &mut Run,
                       t: Duration,
                       original: bool,
                       drop_it: bool| {
        let now = t0 + t;
        for em in e.take_transmissions() {
            ledger.record(
                em.wire_seq,
                now,
                Transmission {
                    emit_us: em.emit_us,
                    wire_bytes: em.wire_bytes,
                    key: em.key,
                    depart_us: None,
                    dep_idx: None,
                },
            );
            ledger.mark_departed(em.wire_seq, now, t.as_micros() as u64);
            e.on_departed(em.wire_seq, now);
            if original && drop_it {
                run.dropped += 1;
                continue;
            }
            let service = if t < sc.switch_at {
                sc.service_before
            } else {
                sc.service_after
            };
            let start = (*server_free).max(t);
            *server_free = start + service;
            run.max_queue = run.max_queue.max(start - t);
            in_flight.push_back((*server_free + sc.base_rtt, em.wire_seq, t));
        }
    };

    loop {
        let now = t0 + t;
        if sending && t >= next_send {
            let key = EmitKey::new(seq, 0, 0, 0);
            e.submit_one(key, source(seq), now, None, now);
            let drop_it = sc.drop_every.is_some_and(|n| seq % n == n - 1)
                || sc
                    .max_sends
                    .is_some_and(|m| seq >= m.saturating_sub(sc.drop_last));
            if drop_it && first_drop_at.is_none() {
                first_drop_at = Some(t);
            }
            put_on_wire(
                &mut e,
                &mut ledger,
                &mut in_flight,
                &mut server_free,
                &mut run,
                t,
                true,
                drop_it,
            );
            seq += 1;
            originals_total += 1;
            run.sent += 1;
            next_send += sc.send_every;
            if sc.max_sends.is_some_and(|m| originals_total >= m) {
                sending = false;
                t_stop = t;
            } else if let Some(&(_, _, _)) = in_flight.back() {
                let queued = server_free.saturating_sub(t + sc.service_after);
                if queued >= sc.stop_at_queue_delay {
                    sending = false;
                    t_stop = t;
                }
            }
        }
        while in_flight.front().is_some_and(|(due, _, _)| *due <= t) {
            let (_, ws, departed) = in_flight.pop_front().unwrap();
            match ledger.resolve(ws) {
                Some(Resolution::Live(tx)) => {
                    let rtt = t - departed;
                    run.max_rtt = run.max_rtt.max(rtt);
                    e.note_ack_origin(tx.key, ws);
                    e.on_ack(&[tx.key]);
                    e.rtt.on_sample(rtt);
                    e.note_ack(now);
                    passes_acked.insert(tx.key);
                    run.finished_at = t;
                }
                Some(Resolution::Late(tx)) => {
                    e.note_ack_origin(tx.key, ws);
                    e.on_ack(&[tx.key]);
                    e.rtt.on_sample(t - departed);
                    e.note_ack(now);
                    passes_acked.insert(tx.key);
                    run.finished_at = t;
                }
                None => {}
            }
        }
        // The loss path, exactly as the bridge runs it each turn.
        let out = loss_path::detect(&mut ledger, &mut e);
        let _ = out;
        let _ = loss_path::probe(&mut ledger, &mut e, now);
        let before = e.stats.rto_fired;
        e.tick_budgeted(now, usize::MAX, usize::MAX);
        if e.stats.rto_fired > before && run.first_repair_after_drop.is_none() {
            run.first_repair_after_drop = first_drop_at.map(|d| t.saturating_sub(d));
        }
        put_on_wire(
            &mut e,
            &mut ledger,
            &mut in_flight,
            &mut server_free,
            &mut run,
            t,
            false,
            false,
        );
        run.max_pto = run.max_pto.max(e.pto());
        let done = !sending && in_flight.is_empty() && e.pending_cache_entries() == 0;
        if done || (!sending && t > t_stop + sc.settle) {
            break;
        }
        assert!(
            t < Duration::from_secs(600),
            "script never finished: {run:?}"
        );
        t += step;
    }
    let ls = ledger.stats();
    run.verdicts = ls.lost_packet_threshold + ls.lost_time_threshold + ls.lost_pto;
    run.verdicts_packet = ls.lost_packet_threshold;
    run.verdicts_time = ls.lost_time_threshold;
    run.verdicts_pto = ls.lost_pto;
    run.spurious_verdicts = ls.spurious_losses;
    run.k_pkt = ls.k_pkt;
    run.loss_retx = e.stats.loss_retransmits;
    run.probe_retx = e.stats.pto_probe_retransmits;
    run.pto_fires = e.stats.pto_fires;
    run.passes_acked = passes_acked.len() as u64;
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    /// F4's structural claim, now both halves: a bottleneck that queues
    /// without loss or reordering, up to a second of delay, however fast the
    /// queue grows (0.2 to 3 seconds of delay per second), produces no
    /// verdict and no probe fire. Acknowledgements keep arriving in departure
    /// order, so nothing outstanding departed `K_PKT` places before the newest
    /// acknowledged departure, and every acknowledgement restarts the timer.
    ///
    /// Premises: every pass was acknowledged, the queue reached the peak, the
    /// deadline followed it, and it ran long enough for a fixed-horizon
    /// detector to fire (the mutation below: a 3 x RTT horizon declares dozens
    /// of losses on these queues).
    #[test]
    fn an_in_order_queue_ramping_to_a_second_gives_no_verdict_and_no_probe() {
        for service_ms in [40u64, 25, 20, 12] {
            let sc = Script::capacity_drop(Duration::from_millis(service_ms));
            let r = run_script(sc);
            let name = format!("service {service_ms} ms");
            assert_eq!(
                r.verdicts, 0,
                "{name}: loss verdicts on a lossless queue: {r:?}"
            );
            assert_eq!(
                r.pto_fires, 0,
                "{name}: probe fired on a lossless queue: {r:?}"
            );
            assert_eq!(r.loss_retx + r.probe_retx, 0, "{name}: {r:?}");
            assert_eq!(r.passes_acked, r.sent, "{name}: {r:?}");
            assert!(
                r.max_queue >= sc.stop_at_queue_delay,
                "{name}: premise: {r:?}"
            );
            assert!(
                r.max_pto >= sc.stop_at_queue_delay,
                "{name}: premise: the deadline followed the queue: {r:?}"
            );
        }
    }

    /// Real loss in the same in-order stream: every tenth datagram dropped. The
    /// later departures' acknowledgements find each hole after three more
    /// (packet threshold), one retransmission per hole, no probe needed, and
    /// every pass ends up acknowledged.
    #[test]
    fn a_dropped_datagram_is_repaired_once_by_threshold_loss() {
        let mut sc = Script::steady(400);
        sc.drop_every = Some(10);
        let r = run_script(sc);
        assert_eq!(r.dropped, 40, "premise: {r:?}");
        assert_eq!(r.passes_acked, 400, "{r:?}");
        assert_eq!(r.verdicts, 40, "one verdict per hole, no more: {r:?}");
        // The 400th datagram is itself a hole, with nothing after it to be
        // judged against: the probe repairs that one.
        assert_eq!(
            r.loss_retx + r.probe_retx,
            40,
            "one retransmission per hole: {r:?}"
        );
        assert_eq!(r.probe_retx, 1, "{r:?}");
        assert_eq!(r.pto_fires, 1, "{r:?}");
        assert_eq!(r.spurious_verdicts, 0, "{r:?}");
        // 3 later departures at 10 ms plus the round trip: a few tens of ms.
        let first = r.first_repair_after_drop.expect("a repair");
        assert!(first < Duration::from_millis(200), "{first:?}");
    }

    /// PR-9 depends on this: a lost final pass with nothing after it has no
    /// later acknowledgement to be judged against, so only the probe timeout
    /// can repair it. It does, within a few PTOs, and so does a final pass of
    /// several lost datagrams (the probe sends the newest two, the
    /// acknowledgements make the rest threshold-lost or the next probe's).
    #[test]
    fn a_lost_final_pass_is_always_repaired_by_the_probe() {
        for tail in [1u32, 2, 3, 5] {
            let mut sc = Script::steady(30);
            sc.drop_last = tail;
            let r = run_script(sc);
            let name = format!("last {tail} lost");
            assert_eq!(r.dropped as u32, tail, "{name}: premise {r:?}");
            assert_eq!(r.passes_acked, 30, "{name}: every pass repaired: {r:?}");
            assert!(r.pto_fires >= 1, "{name}: the probe did it: {r:?}");
            assert!(r.probe_retx >= 1, "{name}: {r:?}");
            // Each probe sends the newest two: a tail of `tail` needs at most
            // ceil(tail / 2) probes plus the threshold path for the rest.
            assert!(
                r.finished_at < Duration::from_secs(3),
                "{name}: took {:?}: {r:?}",
                r.finished_at
            );
        }
    }

    /// A probe never resends the whole set: with a hundred outstanding it sends
    /// the newest two.
    #[test]
    fn a_probe_sends_the_newest_two_never_the_whole_set() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        let mut ledger = TransmissionLedger::new(1024);
        e.rtt.on_sample(Duration::from_millis(40));
        for i in 0..100u32 {
            let key = EmitKey::new(i, 0, 0, 0);
            e.submit_one(key, source(i), t0, None, t0);
            for em in e.take_transmissions() {
                ledger.record(
                    em.wire_seq,
                    t0,
                    Transmission {
                        emit_us: 0,
                        wire_bytes: em.wire_bytes,
                        key: em.key,
                        depart_us: None,
                        dep_idx: None,
                    },
                );
                ledger.mark_departed(em.wire_seq, t0, 0);
                e.on_departed(em.wire_seq, t0);
            }
        }
        let due = e.pto_deadline().expect("running");
        assert!(loss_path::probe(&mut ledger, &mut e, due - Duration::from_millis(1)).is_none());
        let out = loss_path::probe(&mut ledger, &mut e, due).expect("due");
        assert_eq!(out.retx_queued, 2);
        let mut n = 0;
        e.tick_loss_budgeted(due, usize::MAX, usize::MAX, &mut n);
        assert_eq!(n, 2, "two retransmissions, not a hundred");
        assert_eq!(e.stats.pto_probe_retransmits, 2);
        // And they are the newest two.
        let keys: Vec<u32> = e
            .take_transmissions()
            .iter()
            .map(|t| t.key.frame_seq)
            .collect();
        assert_eq!(keys, vec![99, 98]);
        // The timer restarted, backed off.
        assert!(e.pto_deadline().unwrap() > due + e.pto());
    }
}
