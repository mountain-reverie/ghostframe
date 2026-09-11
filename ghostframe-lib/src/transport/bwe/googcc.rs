//! Drives `goog_cc::GoogCcNetworkController` from our ACK-arrival samples.
//!
//! The controller runs send-side, so no wire-format change is needed: the
//! server's own send times plus the receiver arrival times echoed in the ACK
//! envelope are sufficient. A constant clock offset between the two cancels in
//! GoogCC's `recv_delta - send_delta`.

use super::timeline::Lo16Timeline;
use super::{AckArrival, BweSnapshot};
use goog_cc::network_control::{NetworkControllerConfig, NetworkControllerInterface};
use goog_cc::transport::{
    NetworkControlUpdate, PacketResult, ProcessInterval, SentPacket, TargetRateConstraints,
    TransportPacketsFeedback,
};
use goog_cc::units::{DataRate, DataSize, Timestamp};
use goog_cc::{GoogCcConfig, GoogCcNetworkController};
use std::time::Instant;

/// Floor and ceiling handed to the controller. The floor keeps a badly
/// congested link usable rather than collapsing to nothing; the ceiling stops
/// a probe overshoot proposing an absurd rate on a fast LAN.
const MIN_BPS: i64 = 200_000;
const MAX_BPS: i64 = 200_000_000;

// The driver has no production caller yet — that lands in a later task that
// wires it in behind the public seam. Remove this allow once that lands.
#[allow(dead_code)]
pub(crate) struct GoogCcDriver {
    ctl: GoogCcNetworkController,
    /// Only the arrival series needs unwrapping: it crosses the wire as 16
    /// bits of the client's clock. `server_emit_us` is already monotonic
    /// server-side microseconds and is used directly.
    arrival_time: Lo16Timeline,
    base: Instant,
    estimate_bps: u64,
    samples_seen: u64,
}

#[allow(dead_code)]
impl GoogCcDriver {
    pub(crate) fn new(initial_bps: u64, now: Instant) -> Self {
        let at_time = Timestamp::from_millis(0);
        let cfg = NetworkControllerConfig {
            constraints: TargetRateConstraints {
                at_time,
                min_data_rate: Some(DataRate::from_bits_per_sec(MIN_BPS)),
                max_data_rate: Some(DataRate::from_bits_per_sec(MAX_BPS)),
                starting_rate: Some(DataRate::from_bits_per_sec(
                    (initial_bps as i64).clamp(MIN_BPS, MAX_BPS),
                )),
            },
            ..Default::default()
        };
        Self {
            ctl: GoogCcNetworkController::new(
                cfg,
                GoogCcConfig {
                    feedback_only: false,
                },
            ),
            arrival_time: Lo16Timeline::default(),
            base: now,
            estimate_bps: initial_bps,
            samples_seen: 0,
        }
    }

    /// Feed one ACK batch and advance the controller.
    pub(crate) fn update(&mut self, records: &[AckArrival], now: Instant) -> BweSnapshot {
        if records.is_empty() {
            return self.snapshot();
        }

        let feedback_time = self.to_timestamp(now);
        let mut packet_feedbacks = Vec::with_capacity(records.len());
        for r in records {
            let send_ms = (r.server_emit_us / 1_000) as i64;
            let recv_ms = self.arrival_time.unwrap_ms(r.client_arrival_ms_lo16) as i64;
            let sent = SentPacket {
                send_time: Timestamp::from_millis(send_ms),
                size: DataSize::from_bytes(r.size_bytes as i64),
                ..Default::default()
            };
            // The controller must see the send before the acknowledgement.
            self.ctl.on_sent_packet(sent);
            packet_feedbacks.push(PacketResult {
                sent_packet: sent,
                receive_time: Timestamp::from_millis(recv_ms),
                ..Default::default()
            });
        }
        self.samples_seen += records.len() as u64;

        let upd = self
            .ctl
            .on_transport_packets_feedback(TransportPacketsFeedback {
                feedback_time,
                data_in_flight: DataSize::from_bytes(0),
                packet_feedbacks,
                sendless_arrival_times: Vec::new(),
            });
        self.absorb(upd);

        let upd = self.ctl.on_process_interval(ProcessInterval {
            at_time: feedback_time,
            ..Default::default()
        });
        self.absorb(upd);

        self.snapshot()
    }

    pub(crate) fn snapshot(&self) -> BweSnapshot {
        BweSnapshot {
            bitrate_bps: self.estimate_bps,
            samples_seen: self.samples_seen,
        }
    }

    fn absorb(&mut self, upd: NetworkControlUpdate) {
        if let Some(t) = upd.target_rate {
            let bps = t.target_rate.bps();
            if bps > 0 {
                self.estimate_bps = bps as u64;
            }
        }
    }

    fn to_timestamp(&self, now: Instant) -> Timestamp {
        Timestamp::from_millis(now.saturating_duration_since(self.base).as_millis() as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::GoogCcDriver;
    use crate::transport::bwe::AckArrival;
    use std::time::{Duration, Instant};

    /// Feeding a steadily-delivered stream must move the estimate off its
    /// starting rate. Asserting only `> 0` would pass on a driver that never
    /// reaches the controller at all, since the starting rate is non-zero.
    #[test]
    fn steady_delivery_moves_the_estimate() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(1_000_000, t0);

        for step in 0..400u32 {
            // 12 x 1200 B every 20 ms, sends spaced 1 ms apart.
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    AckArrival {
                        wire_seq: step * 12 + i,
                        server_emit_us: emit_us,
                        // 15 ms one-way delay, constant.
                        client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                        size_bytes: 1200,
                    }
                })
                .collect();
            d.update(&batch, t0 + Duration::from_millis(20 * step as u64));
        }

        let snap = d.snapshot();
        assert_eq!(snap.samples_seen, 400 * 12);
        assert!(
            snap.bitrate_bps > 0,
            "controller produced no estimate after 400 batches"
        );
        assert_ne!(
            snap.bitrate_bps, 1_000_000,
            "estimate never moved off the starting rate — the controller's \
             updates are not being absorbed"
        );
    }
}
