//! A scripted bottleneck against the emitter's retransmission timer (D1 F4).
//!
//! The bottleneck queues without loss or reordering: every datagram is
//! delivered, in departure order, after the base round trip plus a queueing
//! delay that ramps from nothing to a second. The emitter sees only what the
//! real one sees: a departure stamp for each datagram, and an acknowledgement
//! that feeds both the cache and the RTT estimator.

use super::emitter::ReliableTileEmitter;
use super::EmitKey;
use bytes::Bytes;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

fn source(seq: u32) -> Bytes {
    let mut v = vec![0u8; 25];
    v[0..4].copy_from_slice(&(0x8000_0000 | seq).to_be_bytes());
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v[20..24].copy_from_slice(&1u32.to_be_bytes());
    Bytes::from(v)
}

/// What a run of the script produced.
#[derive(Debug, Clone, Copy)]
pub(super) struct Run {
    pub sent: u64,
    pub acked: u64,
    pub fires: u64,
    pub spurious: u64,
    pub deferred: u64,
    pub lost: u64,
    pub first_fire_after_departure: Option<Duration>,
    pub max_rtt: Duration,
    pub max_queue: Duration,
    pub max_pto: Duration,
}

/// The scripted path: one datagram every `send_every` into a FIFO bottleneck
/// that serves one in `service_before` until `switch_at` and one in
/// `service_after` from then on, then `base_rtt` of propagation. A datagram
/// that arrives while the queue is non-empty waits its turn, so the queue grows
/// at `service_before.. / ..` whenever the offered rate exceeds the service
/// rate, and everything is delivered in departure order.
#[derive(Debug, Clone, Copy)]
pub(super) struct Script {
    pub send_every: Duration,
    pub base_rtt: Duration,
    pub service_before: Duration,
    pub service_after: Duration,
    pub switch_at: Duration,
    /// Sending stops at the first datagram that queues this long.
    pub stop_at_queue_delay: Duration,
    /// Also drop every n-th datagram (never delivered, takes no service).
    pub drop_every: Option<u32>,
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
        }
    }
}

pub(super) fn run_script(sc: Script) -> Run {
    let t0 = Instant::now();
    let mut e = ReliableTileEmitter::new(t0);
    // Before the first acknowledgement the deadline stands on the handshake
    // RTT, as it does in the bridge.
    e.rtt.seed(sc.base_rtt);
    let step = Duration::from_millis(1);

    // (ack_due, key, departed)
    let mut in_flight: VecDeque<(Duration, EmitKey, Duration)> = VecDeque::new();
    let mut run = Run {
        sent: 0,
        acked: 0,
        fires: 0,
        spurious: 0,
        deferred: 0,
        lost: 0,
        first_fire_after_departure: None,
        max_rtt: Duration::ZERO,
        max_queue: Duration::ZERO,
        max_pto: Duration::ZERO,
    };
    let mut t = Duration::ZERO;
    let mut next_send = Duration::ZERO;
    let mut sending = true;
    let mut server_free = Duration::ZERO;
    let mut seq = 0u32;
    let mut t_stop = Duration::ZERO;
    loop {
        let now = t0 + t;
        if sending && t >= next_send {
            let key = EmitKey::new(seq, 0, 0, 0);
            e.submit_one(key, source(seq), now, None, now);
            let ws = e.take_transmissions()[0].wire_seq;
            // The host's FIFO is bounded by F1 and empty here: departure is now.
            e.on_departed(ws, now);
            if sc.drop_every.is_some_and(|n| seq % n == n - 1) {
                run.lost += 1;
            } else {
                let service = if t < sc.switch_at {
                    sc.service_before
                } else {
                    sc.service_after
                };
                let start = server_free.max(t);
                server_free = start + service;
                let queued = start - t;
                run.max_queue = run.max_queue.max(queued);
                in_flight.push_back((server_free + sc.base_rtt, key, t));
                if queued >= sc.stop_at_queue_delay {
                    sending = false;
                    t_stop = t;
                }
            }
            seq += 1;
            run.sent += 1;
            next_send += sc.send_every;
        }
        while in_flight.front().is_some_and(|(due, _, _)| *due <= t) {
            let (_, key, departed) = in_flight.pop_front().unwrap();
            let rtt = t - departed;
            run.max_rtt = run.max_rtt.max(rtt);
            e.on_ack(&[key]);
            e.rtt.on_sample(rtt);
            e.note_ack(departed.as_micros() as u64, now);
            run.acked += 1;
        }
        let before = e.stats.rto_fired;
        e.tick_budgeted(now, usize::MAX, usize::MAX);
        if run.first_fire_after_departure.is_none() && e.stats.rto_fired > before {
            run.first_fire_after_departure = Some(t);
        }
        run.max_pto = run.max_pto.max(e.pto());
        if !sending
            && in_flight.is_empty()
            && (run.lost == 0 || t > t_stop + Duration::from_secs(8))
        {
            break;
        }
        assert!(
            t < Duration::from_secs(600),
            "script never finished: {run:?}"
        );
        t += step;
    }
    run.fires = e.stats.rto_fired;
    run.spurious = e.stats.rto_spurious;
    run.deferred = e.stats.rto_deferred_by_ack;
    run
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Duration = Duration::from_millis(1);

    /// F4's structural claim, at the emitter: a bottleneck that queues without
    /// loss or reordering, up to a second of delay, produces no retransmission
    /// timer fire, however fast the queue grows (0.25 to 3 seconds of delay per
    /// second). The delay a datagram sees is always larger than the estimate
    /// built from the acknowledgements that have returned so far, by the
    /// growth over one round trip; a deadline that only adds a variance term to
    /// `srtt` is passed by every datagram in a fast enough ramp. What stops it
    /// firing is that acknowledgements keep arriving in order.
    ///
    /// Premises: every datagram was sent and acknowledged, the queue reached
    /// the peak, the deadline followed it, and the hold-back (`deferred`) was
    /// exercised. The fires assertion comes first so a failure shows them.
    #[test]
    fn an_in_order_queue_ramping_to_a_second_never_fires_a_timer() {
        for service_ms in [40u64, 25, 20, 12] {
            let sc = Script::capacity_drop(Duration::from_millis(service_ms));
            let r = run_script(sc);
            let growth = service_ms as f64 / 10.0 - 1.0;
            let name = format!("queue grows {growth:.2} s/s");
            assert_eq!(r.fires, 0, "{name}: timer fired on a lossless queue: {r:?}");
            assert!(r.sent > 200 && r.acked == r.sent, "{name}: {r:?}");
            assert!(
                r.max_queue >= sc.stop_at_queue_delay,
                "{name}: premise: {r:?}"
            );
            assert!(
                r.max_pto >= sc.stop_at_queue_delay,
                "{name}: premise: the deadline followed the queue: {r:?}"
            );
            assert!(
                r.deferred > 0,
                "{name}: premise: the hold-back engaged: {r:?}"
            );
        }
    }

    /// The hold-back must not blind the timer to real loss: with every tenth
    /// datagram dropped by the same queue, later departures are acknowledged
    /// past the lost ones, so their own deadlines stand and they fire. At
    /// least one fire per lost pass, the first soon after the first loss.
    #[test]
    fn a_pass_the_queue_dropped_still_fires_while_later_ones_are_acknowledged() {
        let mut sc = Script::capacity_drop(25 * MS);
        sc.drop_every = Some(10);
        let r = run_script(sc);
        assert!(r.lost >= 20, "premise: datagrams were dropped: {r:?}");
        assert!(r.fires >= r.lost, "every lost pass fires: {r:?}");
        // The first loss is the tenth datagram, sent at 90 ms. Its deadline is
        // the handshake-seeded one (40 + 4 x 20 + 5 = 125 ms) until samples
        // arrive: well under a second.
        let first = r.first_fire_after_departure.expect("a fire");
        assert!(first < 500 * MS, "{first:?}");
    }

    /// And a path that goes silent (the last datagram is lost and nothing else
    /// is left to be acknowledged) still fires, one deadline after the last
    /// acknowledgement.
    #[test]
    fn a_silent_path_fires_one_deadline_after_the_last_acknowledgement() {
        let t0 = Instant::now();
        let mut e = ReliableTileEmitter::new(t0);
        e.rtt.on_sample(40 * MS);
        let pto = e.pto();
        let k0 = EmitKey::new(0, 0, 0, 0);
        let k1 = EmitKey::new(1, 0, 0, 0);
        for (i, k) in [k0, k1].into_iter().enumerate() {
            e.submit_one(k, source(i as u32), t0, None, t0);
            let ws = e.take_transmissions()[0].wire_seq;
            e.on_departed(ws, t0);
        }
        // k0 is acknowledged half a deadline in. k1 never is.
        let t_ack = t0 + pto / 2;
        e.on_ack(&[k0]);
        e.note_ack(0, t_ack);
        // Its own deadline has passed, but the path was delivering until
        // t_ack: not yet.
        e.tick_budgeted(t0 + pto, usize::MAX, usize::MAX);
        assert_eq!(e.stats.rto_fired, 0);
        assert_eq!(e.stats.rto_deferred_by_ack, 1, "premise: it was held back");
        e.tick_budgeted(t_ack + pto - MS, usize::MAX, usize::MAX);
        assert_eq!(e.stats.rto_fired, 0);
        e.tick_budgeted(t_ack + pto, usize::MAX, usize::MAX);
        assert_eq!(e.stats.rto_fired, 1);
    }
}
