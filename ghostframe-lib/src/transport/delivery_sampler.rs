//! Per-transmission delivery-rate samples (D1 PR-6b): the glue between
//! ghostframe's departures and acknowledgements and the ported TQUIC
//! estimator in [`crate::transport::delivery_rate`].
//!
//! The estimator follows draft-cheng-iccrg-delivery-rate-estimation. Each
//! transmission is marked at departure with the connection's `delivered`,
//! `delivered_time`, `first_sent_time` and `is_app_limited` (the
//! [`RateMark`], kept on its ledger record), and each acknowledged
//! `wire_seq` turns that mark and its own send time into a rate. The send
//! time is the transmission's own departure stamp (F2), not that of the most
//! recent send: that is the difference from quinn's BBR sampler, whose
//! sample reads about half the delivery rate under ack-every-2.
//!
//! Hooks, and what they replace in TQUIC:
//!
//! - packet sent: [`DeliverySampler::on_departure`], called when F2 stamps a
//!   datagram as having left quinn's FIFO. `bytes_in_flight` is what the ledger
//!   holds as departed and unresolved.
//! - packet acked: [`DeliverySampler::on_acked`] for each live `wire_seq` of an
//!   acknowledgement batch, then [`DeliverySampler::end_ack`] once for the
//!   batch, which is the draft's "upon receiving ACK, fill in the rate
//!   sample". A late acknowledgement of a transmission already declared lost
//!   is not sampled.
//! - app-limited: [`DeliverySampler::note_no_backlog`], called when a
//!   departure left F1 admission with no work held back and quinn's FIFO
//!   empty. The draft's "no more data to send, not network-limited" test.
//!
//! This is observability only in PR-6b: nothing here is read by a sending,
//! pacing, loss or congestion decision. O(1) per departure and per
//! acknowledgement (the windowed maximum is a monotonic deque).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::transport::delivery_rate::{DeliveryRateEstimator, RateSamplePacketState, SentPacket};

/// How long a sample stays in the windowed maximum. A fixed observability
/// choice, not a controller parameter: long enough to span a few frame
/// bursts of a frame-paced sender, short enough to follow a capacity step in
/// a scene held for 20 s. BBR's own filter is ten round trips.
pub const MAX_FILTER_WINDOW: Duration = Duration::from_secs(1);

/// What a transmission carries from its departure to its acknowledgement.
/// Four fields of the draft (`delivered`, `delivered_time`,
/// `first_sent_time`, `is_app_limited`, inside `state`), plus the ordinal
/// the estimator uses to tell newer from older.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateMark {
    pub pkt_num: u64,
    pub state: RateSamplePacketState,
}

/// One rate sample, as [`DeliverySampler::end_ack`] returns it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateSample {
    /// `rs.delivery_rate`, bytes per second.
    pub bytes_per_sec: u64,
    pub is_app_limited: bool,
    pub rtt: Duration,
}

/// The sampler's readings, bits per second where a rate.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeliveryRateStats {
    /// Transmissions marked at departure.
    pub marked: u64,
    /// Departures that set the app-limited flag.
    pub app_limited_marks: u64,
    /// Acknowledged transmissions that fed a sample.
    pub acked: u64,
    /// Rate samples produced, and how many of them were app-limited.
    pub samples: u64,
    pub app_limited_samples: u64,
    /// The latest sample, bits per second, and whether it was app-limited.
    pub latest_bps: u64,
    pub latest_app_limited: bool,
    /// The windowed maximum ([`MAX_FILTER_WINDOW`]) as of the reading, bits
    /// per second. An app-limited sample enters it only when it exceeds the
    /// maximum, the draft's rule for `max_bw`.
    pub max_bps: u64,
}

/// Monotonic-deque windowed maximum: amortised O(1) per sample.
#[derive(Debug, Default)]
struct WindowedMax {
    /// Strictly decreasing in value, increasing in time.
    q: VecDeque<(Instant, u64)>,
}

impl WindowedMax {
    fn push(&mut self, at: Instant, v: u64) {
        while self.q.back().is_some_and(|&(_, b)| b <= v) {
            self.q.pop_back();
        }
        self.q.push_back((at, v));
        self.expire(at);
    }

    fn expire(&mut self, now: Instant) {
        while self
            .q
            .front()
            .is_some_and(|&(t, _)| now.saturating_duration_since(t) > MAX_FILTER_WINDOW)
        {
            self.q.pop_front();
        }
    }

    /// The maximum over the window ending at `now`, without mutating.
    fn get(&self, now: Instant) -> u64 {
        self.q
            .iter()
            .find(|&&(t, _)| now.saturating_duration_since(t) <= MAX_FILTER_WINDOW)
            .map_or(0, |&(_, v)| v)
    }
}

pub struct DeliverySampler {
    est: DeliveryRateEstimator,
    next_pkt: u64,
    /// Acknowledged transmissions folded into the estimator since the last
    /// [`Self::end_ack`].
    updated: u32,
    /// Departure time of the most recent send. Read only by the mutation
    /// that reproduces quinn BBR's sampling (see the unit tests).
    #[allow(dead_code)]
    last_sent_at: Option<Instant>,
    max: WindowedMax,
    stats: DeliveryRateStats,
}

impl DeliverySampler {
    pub fn new(now: Instant) -> Self {
        Self {
            est: DeliveryRateEstimator::new(now),
            next_pkt: 1,
            updated: 0,
            last_sent_at: None,
            max: WindowedMax::default(),
            stats: DeliveryRateStats::default(),
        }
    }

    /// Transmissions marked so far; lets a caller tell whether a batch of
    /// departures marked any.
    pub fn marked(&self) -> u64 {
        self.stats.marked
    }

    /// A transmission of `wire_bytes` left quinn's FIFO at `at` with
    /// `bytes_in_flight` already on the wire and unresolved. Returns the mark
    /// to keep on its ledger record.
    pub fn on_departure(
        &mut self,
        at: Instant,
        wire_bytes: usize,
        bytes_in_flight: u64,
    ) -> RateMark {
        let mut p = SentPacket {
            pkt_num: self.next_pkt,
            time_sent: at,
            time_acked: None,
            sent_size: wire_bytes,
            rate_sample_state: RateSamplePacketState::default(),
        };
        self.next_pkt += 1;
        self.est.on_packet_sent(&mut p, bytes_in_flight, 0);
        self.last_sent_at = Some(at);
        self.stats.marked += 1;
        self.stats.app_limited_marks += u64::from(p.rate_sample_state.is_app_limited);
        RateMark {
            pkt_num: p.pkt_num,
            state: p.rate_sample_state,
        }
    }

    /// Admission held no work and quinn's FIFO was empty after the
    /// departures just stamped: everything sent so far is app-limited, until
    /// something sent after this is acknowledged.
    pub fn note_no_backlog(&mut self) {
        self.est.set_app_limited(true);
    }

    /// A live transmission was acknowledged at `acked_at`. `mark` is its
    /// ledger mark, `departed_at` its own departure stamp.
    pub fn on_acked(
        &mut self,
        mark: &RateMark,
        departed_at: Instant,
        wire_bytes: usize,
        acked_at: Instant,
    ) {
        let mut p = SentPacket {
            pkt_num: mark.pkt_num,
            time_sent: departed_at,
            time_acked: Some(acked_at),
            sent_size: wire_bytes,
            rate_sample_state: mark.state,
        };
        // The transmission's own send time is the point of the exercise: a
        // sampler that uses the most recent send's here reads about half the
        // rate under ack-every-2.
        self.est.update_rate_sample(&mut p);
        self.updated += 1;
        self.stats.acked += 1;
    }

    /// End of one acknowledgement batch: generate the rate sample if the
    /// batch acknowledged anything. Returns it when it carries a new rate.
    pub fn end_ack(&mut self, now: Instant) -> Option<RateSample> {
        if std::mem::take(&mut self.updated) == 0 {
            return None;
        }
        self.est.generate_rate_sample();
        if self.est.sample_interval().is_zero() {
            return None;
        }
        let s = RateSample {
            bytes_per_sec: self.est.delivery_rate(),
            is_app_limited: self.est.is_sample_app_limited(),
            rtt: self.est.sample_rtt(),
        };
        let bps = s.bytes_per_sec.saturating_mul(8);
        self.stats.samples += 1;
        self.stats.app_limited_samples += u64::from(s.is_app_limited);
        self.stats.latest_bps = bps;
        self.stats.latest_app_limited = s.is_app_limited;
        if !s.is_app_limited || bps >= self.max.get(now) {
            self.max.push(now, bps);
        }
        Some(s)
    }

    /// The readings, with the windowed maximum as of `now`.
    pub fn stats(&self, now: Instant) -> DeliveryRateStats {
        DeliveryRateStats {
            max_bps: self.max.get(now),
            ..self.stats
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;

    /// Bottleneck rate: 1200 bytes per millisecond.
    const RATE_BPS: u64 = 1_200_000;
    const SIZE: usize = 1200;
    const PROP_US: u64 = 20_000;

    struct Run {
        /// `(departed_at_us, sample)` for every sample.
        samples: Vec<(u64, RateSample)>,
        first_ack_us: u64,
        /// The sampler's readings at the last acknowledgement.
        final_stats: DeliveryRateStats,
        /// quinn BBR's sample, for contrast (design D1 section 6, D): one
        /// packet's bytes over the interval between two acknowledgement
        /// frames. Saturated runs only.
        quinn_like: Vec<(u64, u64)>,
    }

    /// A sender over a single-queue bottleneck of `RATE_BPS`, an
    /// acknowledgement every `every` packets, scripted departures. In
    /// `Saturated` the sender keeps `window` packets in flight, sending at
    /// ack time. In `Sparse` it sends one packet per `gap_us` and says it has
    /// no backlog after each. Times are microseconds from `base`.
    enum Mode {
        Saturated { window: usize },
        Sparse { gap_us: u64 },
    }

    fn run(mode: Mode, every: usize, duration_us: u64) -> Run {
        let base = Instant::now();
        let at = |us: u64| base + Duration::from_micros(us);
        let mut s = DeliverySampler::new(base);
        let mut link_free = 0u64;
        let mut inflight = 0u64;
        // (ack_time, [(sent_us, mark, size)])
        let mut acks: BinaryHeap<Reverse<(u64, usize)>> = BinaryHeap::new();
        let mut batches: Vec<Vec<(u64, RateMark, usize)>> = Vec::new();
        let mut open: Vec<(u64, u64, RateMark, usize)> = Vec::new(); // (arrival, sent, mark, size)
        let mut out = Run {
            samples: Vec::new(),
            first_ack_us: u64::MAX,
            final_stats: DeliveryRateStats::default(),
            quinn_like: Vec::new(),
        };
        let mut prev_ack_us: Option<u64> = None;
        let send = |s: &mut DeliverySampler,
                    now: u64,
                    inflight: &mut u64,
                    link_free: &mut u64,
                    open: &mut Vec<(u64, u64, RateMark, usize)>,
                    acks: &mut BinaryHeap<Reverse<(u64, usize)>>,
                    batches: &mut Vec<Vec<(u64, RateMark, usize)>>| {
            let mark = s.on_departure(at(now), SIZE, *inflight);
            *inflight += SIZE as u64;
            let start = now.max(*link_free);
            *link_free = start + SIZE as u64 * 1_000_000 / RATE_BPS;
            open.push((*link_free + PROP_US, now, mark, SIZE));
            if open.len() == every {
                // The receiver acknowledges when the last of the batch arrives.
                let ack_at = open.iter().map(|o| o.0).max().unwrap() + PROP_US;
                batches.push(open.drain(..).map(|o| (o.1, o.2, o.3)).collect());
                acks.push(Reverse((ack_at, batches.len() - 1)));
            }
        };
        match mode {
            Mode::Saturated { window } => {
                for _ in 0..window {
                    send(
                        &mut s,
                        0,
                        &mut inflight,
                        &mut link_free,
                        &mut open,
                        &mut acks,
                        &mut batches,
                    );
                }
            }
            Mode::Sparse { gap_us } => {
                let mut t = 0;
                while t < duration_us {
                    // Drain acks due before this send.
                    while let Some(&Reverse((ta, bi))) = acks.peek() {
                        if ta > t {
                            break;
                        }
                        acks.pop();
                        for (sent, mark, size) in batches[bi].clone() {
                            s.on_acked(&mark, at(sent), size, at(ta));
                            inflight -= size as u64;
                        }
                        out.first_ack_us = out.first_ack_us.min(ta);
                        if let Some(r) = s.end_ack(at(ta)) {
                            out.samples.push((batches[bi][0].0, r));
                        }
                    }
                    send(
                        &mut s,
                        t,
                        &mut inflight,
                        &mut link_free,
                        &mut open,
                        &mut acks,
                        &mut batches,
                    );
                    s.note_no_backlog();
                    t += gap_us;
                }
                while let Some(Reverse((ta, bi))) = acks.pop() {
                    for (sent, mark, size) in batches[bi].clone() {
                        s.on_acked(&mark, at(sent), size, at(ta));
                    }
                    if let Some(r) = s.end_ack(at(ta)) {
                        out.samples.push((batches[bi][0].0, r));
                    }
                    out.final_stats = s.stats(at(ta));
                }
                return out;
            }
        }
        let Mode::Saturated { window } = mode else {
            unreachable!()
        };
        while let Some(Reverse((ta, bi))) = acks.pop() {
            if ta > duration_us {
                break;
            }
            for (sent, mark, size) in batches[bi].clone() {
                s.on_acked(&mark, at(sent), size, at(ta));
                inflight -= size as u64;
            }
            out.first_ack_us = out.first_ack_us.min(ta);
            if let Some(prev) = prev_ack_us.filter(|p| ta > *p) {
                out.quinn_like
                    .push((batches[bi][0].0, SIZE as u64 * 1_000_000 / (ta - prev)));
            }
            prev_ack_us = Some(ta);
            if let Some(r) = s.end_ack(at(ta)) {
                out.samples.push((batches[bi][0].0, r));
            }
            out.final_stats = s.stats(at(ta));
            while inflight < (window * SIZE) as u64 {
                send(
                    &mut s,
                    ta,
                    &mut inflight,
                    &mut link_free,
                    &mut open,
                    &mut acks,
                    &mut batches,
                );
            }
        }
        out
    }

    #[test]
    fn saturated_samples_equal_the_bottleneck_rate_exactly() {
        for every in [1usize, 2, 3] {
            // 60 packets in flight is above the 41-packet bandwidth-delay
            // product, so the link never idles.
            let r = run(Mode::Saturated { window: 60 }, every, 1_000_000);
            // Past the start-up transient: the initial burst's samples, then
            // the dip while the 19-packet standing queue the oversized
            // window built drains, are legitimately below the link rate.
            // Three round trips after the first acknowledgement the sender
            // is ack-clocked and every sample must be the link rate.
            let steady: Vec<_> = r
                .samples
                .iter()
                .filter(|(sent, _)| *sent >= r.first_ack_us + 3 * (2 * PROP_US + 1_000))
                .collect();
            assert!(
                steady.len() > 100,
                "ack-every-{every}: only {} steady samples",
                steady.len()
            );
            for (sent, s) in &r.samples {
                assert!(
                    s.bytes_per_sec <= RATE_BPS,
                    "ack-every-{every}: sample {} above the link ({sent} us)",
                    s.bytes_per_sec
                );
            }
            for (sent, s) in &steady {
                // The sample's round trip is the acknowledged transmission's
                // own: at least two propagation delays. A sampler that took
                // the most recent send time reads a round trip near zero.
                assert!(
                    s.rtt >= Duration::from_micros(2 * PROP_US),
                    "ack-every-{every}: rtt {:?} below the path's for the send at {sent} us",
                    s.rtt
                );
                assert!(
                    !s.is_app_limited,
                    "ack-every-{every}: saturated sample flagged"
                );
                assert_eq!(
                    s.bytes_per_sec, RATE_BPS,
                    "ack-every-{every}: sample for the send at {sent} us"
                );
            }
        }
    }

    /// The contrast the design relies on: on the same acknowledgements,
    /// quinn BBR's sample reads about half the rate under ack-every-2 while
    /// the per-transmission sample reads the link. Proves the vector above
    /// can tell the two apart.
    #[test]
    fn quinn_style_sampling_halves_under_ack_every_2_and_this_does_not() {
        for (every, quinn_expected) in [(1usize, RATE_BPS), (2, RATE_BPS / 2)] {
            let r = run(Mode::Saturated { window: 60 }, every, 1_000_000);
            let q: Vec<_> = r
                .quinn_like
                .iter()
                .filter(|(sent, _)| *sent >= r.first_ack_us + 3 * (2 * PROP_US + 1_000))
                .collect();
            assert!(
                q.len() > 100,
                "ack-every-{every}: {} quinn-style samples",
                q.len()
            );
            assert!(
                q.iter().all(|(_, v)| *v == quinn_expected),
                "ack-every-{every}: quinn-style samples {:?}",
                q.iter().map(|x| x.1).take(5).collect::<Vec<_>>()
            );
            assert_eq!(r.final_stats.latest_bps, RATE_BPS * 8);
        }
    }

    #[test]
    fn app_limited_samples_never_exceed_the_bottleneck_rate() {
        for every in [1usize, 2] {
            // One 1200 B packet per 10 ms: a tenth of the link.
            let r = run(Mode::Sparse { gap_us: 10_000 }, every, 1_000_000);
            let limited: Vec<_> = r.samples.iter().filter(|(_, s)| s.is_app_limited).collect();
            assert!(
                limited.len() > 30,
                "ack-every-{every}: only {} app-limited samples; the premise was not reached",
                limited.len()
            );
            for (sent, s) in &limited {
                assert!(
                    s.bytes_per_sec <= RATE_BPS,
                    "ack-every-{every}: app-limited sample {} above the link ({sent} us)",
                    s.bytes_per_sec
                );
            }
            // And they measure the sender, not the link: if one equalled the
            // link rate the flag would be hiding nothing.
            assert!(
                limited.iter().all(|(_, s)| s.bytes_per_sec < RATE_BPS / 2),
                "ack-every-{every}: an app-limited sample read near the link rate"
            );
        }
    }

    #[test]
    fn the_windowed_maximum_reads_the_link_and_ignores_smaller_app_limited_samples() {
        let r = run(Mode::Saturated { window: 60 }, 2, 1_000_000);
        assert_eq!(r.final_stats.max_bps, RATE_BPS * 8);
        assert_eq!(r.final_stats.latest_bps, RATE_BPS * 8);
        assert!(r.final_stats.samples > 100 && r.final_stats.app_limited_samples == 0);
        // A sparse sender: every sample is app-limited, the maximum is the
        // largest of them (they enter only when they raise it) and none is
        // the link rate.
        let r = run(Mode::Sparse { gap_us: 10_000 }, 2, 1_000_000);
        let biggest = r
            .samples
            .iter()
            .map(|(_, s)| s.bytes_per_sec * 8)
            .max()
            .unwrap();
        assert!(r.final_stats.app_limited_samples > 30);
        assert_eq!(r.final_stats.max_bps, biggest);
        assert!(r.final_stats.max_bps < RATE_BPS * 8 / 2);
    }

    #[test]
    fn windowed_max_follows_a_step_down_after_the_window() {
        let base = Instant::now();
        let mut m = WindowedMax::default();
        for i in 0..10u64 {
            m.push(base + Duration::from_millis(i * 100), 10_000_000);
        }
        for i in 10..40u64 {
            m.push(base + Duration::from_millis(i * 100), 2_000_000);
        }
        let end = base + Duration::from_millis(3_900);
        assert_eq!(m.get(end), 2_000_000, "the old high must have aged out");
        assert!(m.q.len() <= 12, "deque grew: {}", m.q.len());
    }
}
