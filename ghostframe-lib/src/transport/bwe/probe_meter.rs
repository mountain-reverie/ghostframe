// SPDX-License-Identifier: AGPL-3.0-or-later
//! Our own reading of what each probe cluster delivered.
//!
//! goog_cc measures a cluster in its `ProbeBitrateEstimator`, but the result
//! is private to the controller and is published only through its target,
//! where the loss-based controller can cap it in the same feedback. The
//! growth gate (`growth_gate.rs`) must know what the probe *measured*, so the
//! probe-tagged acknowledgements are measured here as well, by the same rule:
//!
//! goog_cc evaluates after every packet of a feedback (sorted by receive
//! time) and keeps the last result; here the acknowledgements only accumulate
//! (`on_ack`, in receive order) and `finish_feedback` reads each cluster
//! touched once, after all of its acknowledgements, keeping the last result,
//! so neither a short early prefix of a cluster nor the best of many prefixes
//! can be read high. The rule:
//!
//! * a cluster is read once at least 80 % of its `min_probes` packets and 80 %
//!   of its `min_bytes` are acknowledged;
//! * the send and the receive spans must each be positive and at most 1 s;
//! * the send rate leaves out the last-sent packet (its time is not in the
//!   span) and the receive rate the first-received one;
//! * receive over send above 2 is a clock artefact: no result;
//! * the result is the lower of the two, and when the receive rate is under
//!   0.9 of the send rate (the link was saturated) 0.95 of the receive rate;
//! * a cluster whose last arrival is more than 1 s old is forgotten.
//!
//! Pure and clock-free: times are the caller's, in microseconds.

use std::collections::HashMap;

use crate::transport::reliable_emitter::cache::ProbeTag;

/// Share of a cluster's `min_probes` and `min_bytes` that must be
/// acknowledged, in percent.
const MIN_SHARE_PCT: u64 = 80;
/// The longest send or receive span of a cluster, and how long one is kept
/// after its last arrival.
const MAX_SPAN_US: u64 = 1_000_000;
/// Receive over send above this is not a measurement.
const MAX_RATIO: f64 = 2.0;
/// Receive under this share of send means the link was saturated.
const SATURATED_RATIO: f64 = 0.9;
/// What is taken of the receive rate on a saturated link.
const TARGET_UTILISATION: f64 = 0.95;

#[derive(Debug, Clone, Copy)]
struct Cluster {
    count: i64,
    bytes: u64,
    first_send_us: u64,
    last_send_us: u64,
    last_send_size: u64,
    first_recv_us: u64,
    last_recv_us: u64,
    first_recv_size: u64,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ProbeMeter {
    clusters: HashMap<i32, Cluster>,
    /// Clusters acknowledged in this feedback, in the order of their last
    /// acknowledgement.
    touched: Vec<ProbeTag>,
}

impl ProbeMeter {
    /// Forget every cluster and any result not yet taken.
    pub(crate) fn reset(&mut self) {
        self.clusters.clear();
        self.touched.clear();
    }

    /// One acknowledged probe packet (callers feed a feedback's packets in
    /// receive order, then call `finish_feedback`): sent at `send_us`, received at
    /// `recv_us`, `size_bytes` on the wire. O(1) amortised: the map holds the
    /// clusters of the last second.
    pub(crate) fn on_ack(&mut self, tag: ProbeTag, send_us: u64, recv_us: u64, size_bytes: u32) {
        self.clusters
            .retain(|_, c| c.last_recv_us.saturating_add(MAX_SPAN_US) >= recv_us);
        let size = u64::from(size_bytes);
        let c = self.clusters.entry(tag.id).or_insert(Cluster {
            count: 0,
            bytes: 0,
            first_send_us: send_us,
            last_send_us: send_us,
            last_send_size: size,
            first_recv_us: recv_us,
            last_recv_us: recv_us,
            first_recv_size: size,
        });
        if c.count > 0 {
            if send_us < c.first_send_us {
                c.first_send_us = send_us;
            }
            if send_us > c.last_send_us {
                c.last_send_us = send_us;
                c.last_send_size = size;
            }
            if recv_us < c.first_recv_us {
                c.first_recv_us = recv_us;
                c.first_recv_size = size;
            }
            if recv_us > c.last_recv_us {
                c.last_recv_us = recv_us;
            }
        }
        c.count += 1;
        c.bytes += size;
        self.touched.retain(|t| t.id != tag.id);
        self.touched.push(tag);
    }

    /// End of a feedback: read each cluster it touched once, in the order of
    /// their last acknowledgement, and return the last valid result, bits a
    /// second.
    pub(crate) fn finish_feedback(&mut self) -> Option<u64> {
        let mut result = None;
        for tag in std::mem::take(&mut self.touched) {
            if let Some(bps) = self.clusters.get(&tag.id).and_then(|c| Self::read(c, tag)) {
                result = Some(bps);
            }
        }
        result
    }

    fn read(c: &Cluster, tag: ProbeTag) -> Option<u64> {
        let min_probes = (tag.min_probes as f64 * MIN_SHARE_PCT as f64 / 100.0) as i64;
        let min_bytes = tag.min_bytes.max(0) as f64 * MIN_SHARE_PCT as f64 / 100.0;
        if c.count < min_probes || (c.bytes as f64) < min_bytes {
            return None;
        }
        let send_span = c.last_send_us - c.first_send_us;
        let recv_span = c.last_recv_us - c.first_recv_us;
        if send_span == 0 || send_span > MAX_SPAN_US || recv_span == 0 || recv_span > MAX_SPAN_US {
            return None;
        }
        let send_bytes = c.bytes.checked_sub(c.last_send_size).filter(|&b| b > 0)?;
        let recv_bytes = c.bytes.checked_sub(c.first_recv_size).filter(|&b| b > 0)?;
        let send = send_bytes as f64 * 8e6 / send_span as f64;
        let recv = recv_bytes as f64 * 8e6 / recv_span as f64;
        if recv / send > MAX_RATIO {
            return None;
        }
        let rate = if recv < SATURATED_RATIO * send {
            TARGET_UTILISATION * recv
        } else {
            send.min(recv)
        };
        Some(rate as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tag(id: i32, probes: i64, bytes: i64) -> ProbeTag {
        ProbeTag {
            id,
            min_probes: probes,
            min_bytes: bytes,
            bytes_sent_before: 0,
        }
    }

    /// `n` packets of `size` bytes, sent every `send_gap_us` and received
    /// every `recv_gap_us` after a fixed 20 ms delay, from `t0_us`.
    fn feed(
        m: &mut ProbeMeter,
        t: ProbeTag,
        n: u64,
        size: u32,
        send_gap_us: u64,
        recv_gap_us: u64,
        t0_us: u64,
    ) {
        for i in 0..n {
            m.on_ack(
                t,
                t0_us + i * send_gap_us,
                t0_us + 20_000 + i * recv_gap_us,
                size,
            );
        }
    }

    #[test]
    fn a_clean_cluster_reads_the_lower_of_send_and_receive() {
        let mut m = ProbeMeter::default();
        // 10 x 1250 B, sent 1 ms apart, received 1 ms apart: the span is
        // 9 ms and 11 250 B count (the last-sent / first-received one is
        // left out): 11 250 * 8 / 0.009 s = 10_000_000 bit/s.
        feed(&mut m, tag(1, 10, 10_000), 10, 1250, 1_000, 1_000, 0);
        assert_eq!(m.finish_feedback(), Some(10_000_000));
        assert_eq!(m.finish_feedback(), None, "read once");
    }

    #[test]
    fn too_few_probes_reads_nothing() {
        let mut m = ProbeMeter::default();
        // min_probes 10 -> 8 needed; 7 sent. Bytes are plentiful.
        feed(&mut m, tag(1, 10, 1_000), 7, 1250, 1_000, 1_000, 0);
        assert_eq!(m.finish_feedback(), None);
        // The eighth makes it valid: the test is about the count alone.
        m.on_ack(tag(1, 10, 1_000), 7_000, 27_000, 1250);
        assert!(m.finish_feedback().is_some());
    }

    #[test]
    fn too_few_bytes_reads_nothing() {
        let mut m = ProbeMeter::default();
        // 10 packets of 1250 B = 12 500 B; min_bytes 20 000 -> 16 000 needed.
        feed(&mut m, tag(1, 10, 20_000), 10, 1250, 1_000, 1_000, 0);
        assert_eq!(m.finish_feedback(), None);
        // With min_bytes 15 000 -> 12 000 needed, the same packets are enough.
        let mut m = ProbeMeter::default();
        feed(&mut m, tag(1, 10, 15_000), 10, 1250, 1_000, 1_000, 0);
        assert!(m.finish_feedback().is_some());
    }

    #[test]
    fn receive_over_twice_send_reads_nothing() {
        let mut m = ProbeMeter::default();
        // Sent 1 ms apart (span 9 ms), received 0.4 ms apart (span 3.6 ms):
        // ratio 2.5.
        feed(&mut m, tag(1, 10, 10_000), 10, 1250, 1_000, 400, 0);
        assert_eq!(m.finish_feedback(), None);
    }

    #[test]
    fn a_saturated_link_reads_ninety_five_percent_of_the_receive_rate() {
        let mut m = ProbeMeter::default();
        // Sent 1 ms apart (send 11 250 B / 9 ms = 10 Mbit/s), received
        // 2 ms apart (11 250 B / 18 ms = 5 Mbit/s): 5 < 0.9 x 10, so
        // 0.95 x 5 Mbit/s = 4_750_000.
        feed(&mut m, tag(1, 10, 10_000), 10, 1250, 1_000, 2_000, 0);
        assert_eq!(m.finish_feedback(), Some(4_750_000));
    }

    #[test]
    fn a_receive_rate_just_under_the_send_rate_is_not_scaled() {
        let mut m = ProbeMeter::default();
        // Receive 9.5 Mbit/s of 10 (ratio 0.95 >= 0.9): the lower one, as is.
        // send span 9 ms, receive span 9.473 ms -> 11 250 B * 8 / 0.009473.
        feed(&mut m, tag(1, 10, 10_000), 10, 1250, 1_000, 1_053, 0);
        let r = m.finish_feedback().unwrap();
        // 11 250 * 8e6 / 9_477 = 9_496_... bit/s, no 0.95 applied.
        assert!((9_400_000..=9_600_000).contains(&r), "{r}");
    }

    #[test]
    fn a_span_over_one_second_reads_nothing() {
        let mut m = ProbeMeter::default();
        // 10 packets 200 ms apart: the cluster is complete at its eighth
        // packet, 1.4 s after the first.
        feed(&mut m, tag(1, 10, 10_000), 10, 1250, 200_000, 200_000, 0);
        assert_eq!(m.finish_feedback(), None);
    }

    #[test]
    fn an_old_cluster_is_forgotten() {
        let mut m = ProbeMeter::default();
        // 7 of the 10 packets of cluster 1 (not enough on their own).
        feed(&mut m, tag(1, 10, 10_000), 7, 1250, 1_000, 1_000, 0);
        assert_eq!(m.clusters.len(), 1);
        // A packet of another cluster 2 s later prunes it.
        m.on_ack(tag(2, 10, 10_000), 2_000_000, 2_020_000, 1250);
        assert_eq!(m.clusters.len(), 1);
        assert!(m.clusters.contains_key(&2));
        // Had cluster 1 been kept, its 8th packet would complete it; now it
        // starts over from one packet.
        m.on_ack(tag(1, 10, 10_000), 7_000, 2_030_000, 1250);
        assert_eq!(m.finish_feedback(), None);
    }

    #[test]
    fn the_best_result_since_the_last_take_wins() {
        let mut m = ProbeMeter::default();
        feed(&mut m, tag(1, 10, 10_000), 10, 1250, 1_000, 1_000, 0); // 10 M
        feed(&mut m, tag(2, 10, 10_000), 10, 1250, 1_000, 2_000, 100_000); // 4.75 M
                                                                           // The last cluster read in the feedback, not the highest.
        assert_eq!(m.finish_feedback(), Some(4_750_000));
    }

    /// The first eight of ten packets (the 80 % that make a cluster readable)
    /// arrive at the send rate, the last two after a stall: an evaluation
    /// after every ack would have read 10 Mbit/s at the eighth and kept it.
    #[test]
    fn a_cluster_is_read_whole_not_by_its_best_prefix() {
        let mut m = ProbeMeter::default();
        let t = tag(1, 10, 10_000);
        let recv = [0u64, 1, 2, 3, 4, 5, 6, 7, 40, 80];
        for (i, r) in recv.iter().enumerate() {
            m.on_ack(t, i as u64 * 1_000, 20_000 + r * 1_000, 1250);
        }
        // Whole: 11 250 B over 80 ms = 1.125 Mbit/s against 10 sent: saturated,
        // 0.95 x 1.125 Mbit/s.
        assert_eq!(m.finish_feedback(), Some(1_068_750));
        // A prefix read (the old behaviour) saw 10 Mbit/s after the eighth.
        let mut p = ProbeMeter::default();
        for (i, r) in recv.iter().enumerate().take(8) {
            p.on_ack(t, i as u64 * 1_000, 20_000 + r * 1_000, 1250);
        }
        assert_eq!(p.finish_feedback(), Some(10_000_000), "premise");
    }
}
