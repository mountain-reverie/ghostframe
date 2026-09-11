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

pub(crate) struct GoogCcDriver {
    ctl: SendCtl,
    /// Only the arrival series needs unwrapping: it crosses the wire as 16
    /// bits of the client's clock. `server_emit_us` is already monotonic
    /// server-side microseconds and is used directly.
    arrival_time: Lo16Timeline,
    base: Instant,
    estimate_bps: u64,
    samples_seen: u64,
}

/// Newtype carrying the `Send` assertion, so it covers exactly the one type
/// that needs it and `GoogCcDriver` derives `Send` normally. A future field
/// on the driver that is not `Send` will then be a compile error rather than
/// silently covered.
struct SendCtl(GoogCcNetworkController);

// SAFETY: `GoogCcNetworkController` is `!Send` only because it boxes
// `dyn AcknowledgedBitrateEstimatorInterface` without a `+ Send` bound. That
// trait is private to goog_cc (`lib.rs` has a private `use`, not `pub use`),
// the field is assigned at exactly two sites and both are
// `AcknowledgedBitrateEstimator::create`, and no public API accepts a boxed
// estimator — so no foreign, non-`Send` implementor can reach it. The crate
// has one dependency (`tracing`), zero `unsafe`, and no `Rc`, `RefCell`,
// `Cell`, `UnsafeCell`, raw pointers, `thread_local` or `static mut` anywhere
// in its source. The contents are uniquely owned plain data.
//
// This is a `Send` claim, not a `Sync` one: the value is moved between
// threads when the multi-threaded tokio runtime migrates the bridge task,
// and never shared. `Send` is exactly what that requires.
//
// The audit is pinned to one version by `goog_cc = "=0.1.4"` in Cargo.toml —
// a semver-compatible 0.1.5 could add an `Rc` field with no compile error and
// silently invalidate this.
unsafe impl Send for SendCtl {}

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
            ctl: SendCtl(GoogCcNetworkController::new(
                cfg,
                GoogCcConfig {
                    // We only ever feed `TransportPacketsFeedback` — there is
                    // no separate REMB channel and no independent RTT/loss
                    // report. `feedback_only: true` tells the controller to
                    // derive RTT and loss itself from that feedback (see
                    // `GoogCcNetworkController::on_transport_packets_feedback`'s
                    // `packet_feedback_only` branch); `false` disables that
                    // derivation entirely and starves the loss/RTT-based side
                    // of the estimator.
                    feedback_only: true,
                },
            )),
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

        // Feedback cannot logically precede the packets it reports on: the
        // controller computes `feedback_time - send_time` as an RTT bound,
        // which goes negative (and poisons the RTT/pushback logic) if the
        // caller's clock lags the last packet's send time within this same
        // batch. `now` is normally safely after every send in production
        // (acks can't arrive before the send that produced them), but a
        // batch's individual per-packet send stamps can still range a few
        // ms past the caller's sampled `now` — so floor feedback_time at the
        // batch's own latest send timestamp.
        //
        // Deliberately `send_ms` only — NOT `recv_ms`. `send_ms` is derived
        // from `server_emit_us`, measured from the server's `bwe_epoch`.
        // `recv_ms` is derived from `client_arrival_ms_lo16`, which in the
        // browser is `performance.now() & 0xFFFF` — a page-navigation epoch
        // wrapping every 65.5s — passed through `Lo16Timeline`, which
        // anchors on whatever it first sees. The offset between the two
        // epochs is arbitrary in [0, 65.5s) and `max()` across them is
        // meaningless: when the client series runs ahead, flooring on it
        // drags `feedback_time` into the client's epoch while `send_time`
        // stays in the server's, so goog_cc computes `feedback_rtt` as the
        // epoch offset instead of a real RTT. That trips `RttBasedBackoff`'s
        // 3s limit continuously, drives `LinkCapacityTracker::capacity_estimate_bps`
        // negative, and panics inside `DataRate::from_bits_per_sec_float`'s
        // `value >= 0.0` assertion. Everything else in the controller
        // consumes `receive_time` only as recv-minus-recv differences, so
        // the client epoch cancels naturally without needing to be floored
        // against anything here.
        let mut feedback_time = self.to_timestamp(now);
        // Safe to initialize from the first record's send_ms: `records` was
        // checked non-empty above.
        let mut send_ms_max = (records[0].server_emit_us / 1_000) as i64;
        let mut packet_feedbacks = Vec::with_capacity(records.len());
        for r in records {
            let send_ms = (r.server_emit_us / 1_000) as i64;
            let recv_ms = self.arrival_time.unwrap_ms(r.client_arrival_ms_lo16) as i64;
            send_ms_max = send_ms_max.max(send_ms);
            let sent = SentPacket {
                send_time: Timestamp::from_millis(send_ms),
                size: DataSize::from_bytes(r.size_bytes as i64),
                ..Default::default()
            };
            // The controller must see the send before the acknowledgement.
            self.ctl.0.on_sent_packet(sent);
            packet_feedbacks.push(PacketResult {
                sent_packet: sent,
                receive_time: Timestamp::from_millis(recv_ms),
                ..Default::default()
            });
        }
        feedback_time = feedback_time.max(Timestamp::from_millis(send_ms_max));
        self.samples_seen += records.len() as u64;

        let upd = self
            .ctl
            .0
            .on_transport_packets_feedback(TransportPacketsFeedback {
                feedback_time,
                data_in_flight: DataSize::from_bytes(0),
                packet_feedbacks,
                sendless_arrival_times: Vec::new(),
            });
        self.absorb(upd);

        let upd = self.ctl.0.on_process_interval(ProcessInterval {
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
            // `target_rate` (not `stable_target_rate`) is the value this
            // wrapper reports. `stable_target_rate` is
            // `min(link_capacity_estimate, pushback_target_rate)`, where the
            // capacity tracker starts near the acknowledged rate and climbs
            // back only slowly (a ~10 s time constant) after a delay-based
            // decrease — on a clean, already-settled link it under-reads
            // `target_rate` by roughly 2.5x. That field is libwebrtc's
            // *encoder* hint, chosen for resolution stability, not a value
            // any caller here consumes: this wrapper has no encoder-rate
            // caller and the pacer is driven by
            // `NetworkControlUpdate::pacer_config`, which goog_cc computes
            // independently.
            //
            // `target_rate` is the controller's actual bandwidth estimate —
            // the delay-based/loss-based AIMD result — and is what Stage 2's
            // pacer should be validated against once it's wired up (it
            // should consume `upd.pacer_config` directly rather than
            // deriving a rate from this field).
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

    /// The client's arrival clock is a completely independent epoch — in the
    /// browser it is `performance.now() & 0xFFFF`, anchored at page load. The
    /// driver must be immune to that offset. Flooring `feedback_time` with a
    /// client-epoch value instead makes goog_cc read the offset as an RTT,
    /// trips its 3 s RttBasedBackoff, drives the link-capacity estimate
    /// negative, and panics inside the bridge task.
    #[test]
    fn an_independently_epoched_client_clock_does_not_panic() {
        for offset_ms in [0u64, 3_000, 10_000, 40_000] {
            let t0 = Instant::now();
            let mut d = GoogCcDriver::new(4_000_000, t0);
            for step in 0..400u32 {
                let batch: Vec<AckArrival> = (0..12u32)
                    .map(|i| {
                        let emit_us = ((step * 20 + i) as u64) * 1_000;
                        // Constant 15 ms one-way delay; only the epoch differs.
                        let arrival_ms = emit_us / 1_000 + 15 + offset_ms;
                        AckArrival {
                            wire_seq: step * 12 + i,
                            server_emit_us: emit_us,
                            client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                            size_bytes: 1200,
                        }
                    })
                    .collect();
                d.update(&batch, t0 + Duration::from_millis(20 * step as u64));
            }
            let bps = d.snapshot().bitrate_bps;
            assert!(bps > 0, "offset {offset_ms} ms produced no estimate");
        }
    }

    /// `GoogCcDriver` must stay `Send`: `IoBridge` is spawned with
    /// `tokio::spawn` on the multi-threaded runtime.
    #[test]
    fn driver_is_send() {
        fn assert_send<T: Send>() {}
        assert_send::<GoogCcDriver>();
    }
}
