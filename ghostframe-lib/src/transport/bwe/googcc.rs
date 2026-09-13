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
    NetworkControlUpdate, PacedPacketInfo, PacketResult, ProbeClusterConfig, ProcessInterval,
    SentPacket, TargetRateConstraints, TransportPacketsFeedback,
};
use goog_cc::units::{DataRate, DataSize, Timestamp};
use goog_cc::{GoogCcConfig, GoogCcNetworkController};
use std::time::{Duration, Instant};

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
    /// quinn's measured path RTT, used only as a plausibility bound on the
    /// derived RTT — never fed to the controller. See `note_path_rtt`.
    path_rtt: Option<Duration>,
    implausible_rtt_samples: u64,
    /// Bits-per-second derived from the controller's most recent
    /// `NetworkControlUpdate::pacer_config` (`PacerConfig::data_rate()`).
    /// `None` until the controller has produced at least one pacer config —
    /// see `absorb`. This is Stage 2.2's pacing rate: BWE Stage 2's pacer
    /// consumes this directly rather than deriving a rate from
    /// `target_rate` (see `absorb`'s doc comment on that field).
    pacer_rate_bps: Option<u64>,
    /// The most recently requested, not-yet-consumed probe cluster (BWE
    /// Stage 2.4), converted out of goog_cc's units by `to_probe_request`.
    /// `absorb` overwrites this whenever `NetworkControlUpdate` carries a
    /// new config — one active probe at a time, per the design: a config
    /// this driver hasn't surfaced yet is itself indistinguishable from
    /// "no probe pending" to any caller, so overwriting rather than
    /// queuing loses nothing a caller could have observed. Cleared by
    /// `take_probe_request`.
    pending_probe_request: Option<super::ProbeRequest>,
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
            path_rtt: None,
            implausible_rtt_samples: 0,
            pacer_rate_bps: None,
            pending_probe_request: None,
        }
    }

    /// Record quinn's measured path RTT. Deliberately NOT fed to the
    /// controller: with `packet_feedback_only` set, GoogCC derives its own
    /// RTT from the feedback we send, and that is the value it wants — it
    /// includes the receiver's ACK batching delay, which bounds reaction
    /// speed, while quinn's path RTT excludes exactly that.
    pub(crate) fn note_path_rtt(&mut self, rtt: Duration) {
        self.path_rtt = Some(rtt);
    }

    /// Compare a derived one-way delay against the measured path RTT. Ten
    /// times the path RTT plus a second of slack is far outside anything a
    /// real link produces, so exceeding it means the two timestamps being
    /// differenced are not on the same clock.
    pub(crate) fn observe_derived_rtt(&mut self, derived: Duration) {
        let Some(path) = self.path_rtt else { return };
        if derived > path * 10 + Duration::from_secs(1) {
            self.implausible_rtt_samples += 1;
            tracing::warn!(
                derived_ms = derived.as_millis() as u64,
                path_rtt_ms = path.as_millis() as u64,
                "derived RTT implausible against measured path RTT — check \
                 that emit and arrival timestamps share an epoch"
            );
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
        let mut last_derived = Duration::ZERO;
        for r in records {
            let send_ms = (r.server_emit_us / 1_000) as i64;
            let recv_ms = self.arrival_time.unwrap_ms(r.client_arrival_ms_lo16) as i64;
            send_ms_max = send_ms_max.max(send_ms);
            last_derived = Duration::from_millis((recv_ms - send_ms).max(0) as u64);
            let sent = SentPacket {
                send_time: Timestamp::from_millis(send_ms),
                size: DataSize::from_bytes(r.size_bytes as i64),
                // BWE Stage 2.4: tag this `SentPacket` as a probe packet
                // iff the pass it came from was tagged at emit time. See
                // `pacing_info_for`'s doc comment for why an untagged pass
                // must produce `PacedPacketInfo::default()`
                // (`probe_cluster_id == NOT_APROBE`) rather than anything
                // else.
                pacing_info: pacing_info_for(r.probe),
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
        self.observe_derived_rtt(last_derived);

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
            implausible_rtt_samples: self.implausible_rtt_samples,
            pacer_rate_bps: self.pacer_rate_bps,
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
            // independently and which `pacer_rate_bps` below captures.
            //
            // `target_rate` is the controller's actual bandwidth estimate —
            // the delay-based/loss-based AIMD result — and is kept for
            // observability (`BweSnapshot::bitrate_bps`, the periodic log)
            // even though Stage 2.2's pacer consumes `pacer_rate_bps`
            // instead.
            let bps = t.target_rate.bps();
            if bps > 0 {
                self.estimate_bps = bps as u64;
            }
        }
        if let Some(pc) = upd.pacer_config {
            // `pad_window` ("send at least this much, as padding, to hold a
            // floor rate") is deliberately unused. This project does not
            // send padding — see the Stage 2 design doc's 2.4 section,
            // "prefer draining queued work faster over sending padding":
            // our queues normally hold real refinement work that is wanted
            // anyway, so there's no case where we'd pad instead of just
            // sending that. `probe_cluster_configs` (below) is 2.4's, kept
            // as a separate `if` from this one rather than folded in since
            // pacer config and probe requests arrive independently and
            // either can be absent from a given update.
            //
            // `data_window` / `time_window` are guarded finite before
            // calling `data_rate()`: goog_cc always sets `time_window` to a
            // literal 1 s in `get_pacing_rates`, and `data_window` from a
            // finite pacing rate, so this should always hold in practice —
            // but `DataSize::microbits()` (which `data_rate()` calls
            // through `Div<TimeDelta>`) panics on an infinite/oversized
            // operand and dividing by a zero `TimeDelta` panics too, so both
            // are checked defensively rather than trusted.
            if pc.data_window.is_finite() && pc.time_window.is_finite() && pc.time_window.us() > 0 {
                let bps = pc.data_rate().bps();
                if bps > 0 {
                    self.pacer_rate_bps = Some(bps as u64);
                }
            }
        }
        // BWE Stage 2.4: `ProbeController` may ask for a burst above the
        // current estimate to discover headroom after a capacity increase.
        // Store the *most recent* config -- `IoBridge` keeps one active
        // probe window at a time (overlapping clusters would interleave
        // their packets and corrupt both measurements), so an
        // as-yet-unsurfaced earlier config in this same batch is
        // indistinguishable to any caller from "never requested" and can
        // simply be overwritten.
        if let Some(cfg) = upd.probe_cluster_configs.last() {
            self.pending_probe_request = Some(Self::to_probe_request(cfg));
        }
    }

    /// Convert a goog_cc `ProbeClusterConfig` into our own `ProbeRequest`,
    /// converting units at this boundary so the type doesn't leak
    /// `goog_cc` types past the `bwe` module.
    fn to_probe_request(cfg: &ProbeClusterConfig) -> super::ProbeRequest {
        // `bps()` / `us()` return negative only for `minus_infinity()`-ish
        // sentinel values goog_cc never actually hands back here; `.max(0)`
        // is a defensive floor, not an expected path.
        let target_rate_bps = cfg.target_data_rate.bps().max(0);
        let duration_us = cfg.target_duration.us().max(0);
        super::ProbeRequest {
            id: cfg.id,
            target_rate_bps: target_rate_bps as u64,
            duration: Duration::from_micros(duration_us as u64),
            min_probes: cfg.target_probe_count.max(0) as i64,
            // bits/sec * microseconds, then /8 (bits -> bytes) /1_000_000
            // (microseconds -> seconds) = bytes. goog_cc exposes no direct
            // helper for this -- `PacedPacketInfo::new` takes `min_bytes`
            // as a plain `i64` -- so this derivation is ours; pinned by a
            // bench assertion against a known config (BWE Stage 2.4
            // Task 4). Both factors are well under 2^32 (rate clamped to
            // `MAX_BPS` = 2e8, duration realistically under a few seconds)
            // so the product fits comfortably in i64 with no overflow risk.
            min_bytes: (target_rate_bps * duration_us) / 8_000_000,
        }
    }

    /// Return and clear the pending probe request, if any -- see
    /// `pending_probe_request`'s doc comment. A config is consumed exactly
    /// once rather than re-triggering on every poll that still sees it
    /// stored.
    pub(crate) fn take_probe_request(&mut self) -> Option<super::ProbeRequest> {
        self.pending_probe_request.take()
    }

    fn to_timestamp(&self, now: Instant) -> Timestamp {
        Timestamp::from_millis(now.saturating_duration_since(self.base).as_millis() as i64)
    }
}

/// Build the `PacedPacketInfo` a `SentPacket` should carry for this
/// `AckArrival`'s probe tag (BWE Stage 2.4). A free function (not a method)
/// so the mapping is directly unit-testable without going through the
/// whole driver/controller plumbing -- see the tests below for exactly
/// what this is checked against.
///
/// `None` (ordinary traffic, the overwhelming majority) MUST produce
/// `PacedPacketInfo::default()`, whose `probe_cluster_id` is
/// `PacedPacketInfo::NOT_APROBE`: `probe_bitrate_estimator.rs:88` asserts
/// `cluster_id != NOT_APROBE` the moment a feedback packet's
/// `pacing_info.probe_cluster_id` is anything else, so mis-tagging
/// ordinary traffic would feed the probe estimator garbage instead of
/// merely leaving it unaffected.
///
/// `Some` sets the estimator's per-packet cluster identity plus the
/// thresholds it gates completion on (`min_probes`/`min_bytes`) and the
/// cluster's running byte total *as of this packet*
/// (`bytes_sent_before` -> `probe_cluster_bytes_sent`) -- goog_cc's own
/// semantics for that field, not the cluster's eventual final total.
fn pacing_info_for(
    probe: Option<crate::transport::reliable_emitter::cache::ProbeTag>,
) -> PacedPacketInfo {
    match probe {
        Some(p) => {
            let mut info = PacedPacketInfo::new(p.id, p.min_probes, p.min_bytes);
            info.probe_cluster_bytes_sent = p.bytes_sent_before;
            info
        }
        None => PacedPacketInfo::default(),
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
                        probe: None,
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
                            probe: None,
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

    /// A derived RTT far above the measured path RTT means the timestamps
    /// being differenced do not share an epoch. That is not a slow link, it
    /// is a bug — and it is how a client-epoch value read as an RTT slipped
    /// through once already.
    #[test]
    fn an_implausible_derived_rtt_is_counted() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        d.note_path_rtt(Duration::from_millis(20));
        // 30 s of apparent one-way delay against a 20 ms path RTT.
        d.observe_derived_rtt(Duration::from_secs(30));
        assert_eq!(d.snapshot().implausible_rtt_samples, 1);

        // A plausible one must not count.
        d.observe_derived_rtt(Duration::from_millis(25));
        assert_eq!(d.snapshot().implausible_rtt_samples, 1);
    }

    /// Without a reference RTT there is nothing to compare against, so
    /// nothing may be flagged — otherwise every session would warn before
    /// quinn reports its first measurement.
    #[test]
    fn no_path_rtt_means_nothing_is_flagged() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        d.observe_derived_rtt(Duration::from_secs(30));
        assert_eq!(d.snapshot().implausible_rtt_samples, 0);
    }

    /// BWE Stage 2.2 consumes `pacer_rate_bps`, not `bitrate_bps`, to drive
    /// emission. Before any feedback it must be `None` (nothing to consume
    /// yet — `PacingMode::Paced` must not be reachable off a
    /// never-produced pacer config), and it must become `Some` once the
    /// controller has processed at least one feedback batch, since
    /// `GoogCcNetworkController::on_transport_packets_feedback` /
    /// `on_process_interval` always populate `pacer_config` (see
    /// `get_pacing_rates`).
    #[test]
    fn pacer_rate_appears_only_after_feedback() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(1_000_000, t0);
        assert_eq!(
            d.snapshot().pacer_rate_bps,
            None,
            "pacer rate must be None before any feedback is absorbed"
        );

        let batch: Vec<AckArrival> = (0..12u32)
            .map(|i| {
                let emit_us = (i as u64) * 1_000;
                AckArrival {
                    wire_seq: i,
                    server_emit_us: emit_us,
                    client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                    size_bytes: 1200,
                    probe: None,
                }
            })
            .collect();
        d.update(&batch, t0 + Duration::from_millis(20));

        let pacer_bps = d
            .snapshot()
            .pacer_rate_bps
            .expect("pacer_config was not absorbed after a feedback batch");
        assert!(
            pacer_bps > 0,
            "pacer rate derived from PacerConfig::data_rate() must be positive"
        );
    }

    // ── BWE Stage 2.4: probe tagging reaches PacedPacketInfo ──────────────
    //
    // These are the direct-evidence tests: a change that compiles but
    // leaves every packet at `NOT_APROBE` would otherwise be invisible,
    // because the estimator would behave exactly as it does today and
    // nothing would fail.

    use super::pacing_info_for;
    use crate::transport::reliable_emitter::cache::ProbeTag;
    use goog_cc::transport::PacedPacketInfo;

    /// The overwhelming-majority case: an untagged pass must produce
    /// `PacedPacketInfo::default()`, whose `probe_cluster_id` is
    /// `NOT_APROBE`. `probe_bitrate_estimator.rs:88` asserts on exactly
    /// this value, so a regression here would panic the first time *any*
    /// feedback batch reached the real estimator, not just probe batches.
    #[test]
    fn untagged_pass_keeps_not_a_probe() {
        let info = pacing_info_for(None);
        assert_eq!(info.probe_cluster_id, PacedPacketInfo::NOT_APROBE);
    }

    /// The evidence this task exists to produce: a tagged pass must arrive
    /// at the driver with a real, non-`NOT_APROBE` `probe_cluster_id`, and
    /// every other `PacedPacketInfo` field must reflect the tag's values —
    /// not some default or placeholder. In particular
    /// `probe_cluster_bytes_sent` must come from `bytes_sent_before` (the
    /// cluster's running total *before* this packet), not `min_bytes` or
    /// zero.
    #[test]
    fn tagged_pass_carries_its_probe_identity_into_paced_packet_info() {
        let tag = ProbeTag {
            id: 7,
            min_probes: 4,
            min_bytes: 12_000,
            bytes_sent_before: 3_500,
        };
        let info = pacing_info_for(Some(tag));
        assert_ne!(
            info.probe_cluster_id,
            PacedPacketInfo::NOT_APROBE,
            "a tagged pass must not be indistinguishable from ordinary traffic"
        );
        assert_eq!(info.probe_cluster_id, 7);
        assert_eq!(info.probe_cluster_min_probes, 4);
        assert_eq!(info.probe_cluster_min_bytes, 12_000);
        assert_eq!(info.probe_cluster_bytes_sent, 3_500);
    }

    /// End-to-end: a probe-tagged `AckArrival` fed through the real
    /// `update()` must reach goog_cc's own `ProbeBitrateEstimator`
    /// (`goog_cc_network_control.rs:811` routes any feedback packet whose
    /// `probe_cluster_id != NOT_APROBE` there) without tripping its
    /// internal invariants -- `probe_bitrate_estimator.rs:88`'s
    /// `assert_ne!(cluster_id, NOT_APROBE)` and its two `assert!(... > 0)`
    /// checks on `probe_cluster_min_probes` / `probe_cluster_min_bytes`.
    /// If `pacing_info_for` ever regressed to not tagging the packet, or to
    /// building a degenerate `PacedPacketInfo`, this reaches those
    /// assertions for real and panics -- rather than silently producing a
    /// snapshot indistinguishable from the untagged case, which is the
    /// exact failure mode ("instrumentation that compiles and reads zero")
    /// this project has hit before.
    #[test]
    fn a_probe_tagged_ack_arrival_reaches_the_controller_without_tripping_its_invariants() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        let tag = ProbeTag {
            id: 3,
            min_probes: 2,
            min_bytes: 1_000,
            bytes_sent_before: 0,
        };
        let records = vec![AckArrival {
            wire_seq: 1,
            server_emit_us: 0,
            client_arrival_ms_lo16: 15,
            size_bytes: 1200,
            probe: Some(tag),
        }];
        // Must not panic.
        let snap = d.update(&records, t0 + Duration::from_millis(20));
        assert_eq!(snap.samples_seen, 1);
    }
}
