// SPDX-License-Identifier: AGPL-3.0-or-later
//! Drives `goog_cc::GoogCcNetworkController` from our ACK-arrival samples.
//!
//! The controller runs send-side, so no wire-format change is needed: the
//! server's own send times plus the receiver arrival times echoed in the ACK
//! envelope are sufficient. A constant clock offset between the two cancels in
//! GoogCC's `recv_delta - send_delta`.
//!
//! # D1: the growth gate
//!
//! While the sender is not saturated (`ceiling.rs`: it spent under
//! `SATURATION` of its grant), the **published** estimate is rate-limited to
//! `UNPROBED_GROWTH_PER_S` (3 % of itself a second) above the highest rate
//! *proved* in the last `PROVED_WINDOW_US` (10 s), unless a probe result lets
//! a rise through; decreases pass at once, and rises up to a proved rate are
//! free (`growth_gate.rs`). A rate is proved by a probe result the gate
//! accepted, by being published while saturated (bounded by the ceiling), or by what a probe cluster
//! measured (`probe_meter.rs`, our own reading of the same acknowledgements)
//! even when goog_cc published less: its loss controller can cap the target
//! in the feedback that carries the result. goog_cc's own upward
//! drift between probes is therefore published only as fast as that.
//!
//! History of the rule, measured: probe-only (the first D1) removed the
//! overshoot (1.27 x to 1.01 x the link) but made recovery after a cut
//! probe-only and regressed p8e, the refs scenes and the step-up (median
//! 1.1 s to 2.3 s); free recovery up to a proved rate (D1') fixed those but the
//! step-up median went to 5.7 s and the refs twin found the headroom in 5 of 24
//! runs (13 needed): a hard hold above the proved rate also stops the ordinary
//! seeds' own 2 %/s drift, which is how they find part of their headroom. The
//! limit lets that through and stops the 8 %/s of the overshoot seeds.
//! What is published is what
//! `snapshot` returns: admission, the H.264 rate target, the classifier, the
//! base-rise check and the spike hold all read it. goog_cc's raw target is
//! read only by the lift-survival counters (a lift is made saturated, where
//! the gate is off, and what they measure is goog_cc's loss-based estimator).
//!
//! Why. In the H.264 step-up scene (2 Mbit/s link, step at 6 s) about one
//! seed in ten (0x6706, 0x6728, 0x672e) has goog_cc's start-up probe land
//! just above its link-capacity bound; the AIMD then grows the estimate by
//! about 8 %/s to its cap of 1.5 x the acknowledged rate (2.5 - 2.9 Mbit/s)
//! with nothing to refute it, because the stream sends at 0.8 x the estimate
//! and stays at or under the link (queue about 20 ms) until it finally queues,
//! about 5.5 s later. libwebrtc's `no_bitrate_increase_in_alr` does nothing:
//! its ALR detector budgets 80 % of the estimate, which a 0.8 x stream spends
//! as fast as it refills, so it never reports application-limited (measured:
//! identical estimate). Normal seeds drift about 0.04 Mbit/s per second
//! between probes. Headroom is still found by probes: goog_cc's own
//! (start-up ladder, repeated initial probing, ALR probing) and the
//! application's (`h264_start_app_probe`), each packet of which reaches this
//! module as `AckArrival::probe`.
//!
//! Only what a probe cluster measured proves a rate, not the mere presence of
//! probe-tagged acknowledgements: raw may drift well above the link while a
//! cluster is still partial. The meter reads each cluster once per feedback
//! and its result is proved before the gate steps in the same feedback, so a
//! rise up to what the probe measured is free and a rise above it is
//! rate-limited. Only a feedback earns unprobed growth credit.
//! No ceiling reading yet means the gate is off, and proves nothing (see
//! `step_gate`).

use super::ceiling::{CeilingReading, EstimateCeiling, SATURATION};
use super::growth_gate::{GateOutcome, GrowthGate, Saturation};
use super::probe_meter::ProbeMeter;
use super::timeline::Lo16Timeline;
use super::{AckArrival, BweSnapshot};
use goog_cc::experiments::FieldTrials;
use goog_cc::network_control::{NetworkControllerConfig, NetworkControllerInterface};
use goog_cc::transport::{
    NetworkAvailability, NetworkControlUpdate, PacedPacketInfo, PacketResult, ProbeClusterConfig,
    ProcessInterval, SentPacket, StreamsConfig, TargetRateConstraints, TransportPacketsFeedback,
};
use goog_cc::units::{DataRate, DataSize, TimeDelta, Timestamp};
use goog_cc::{GoogCcConfig, GoogCcNetworkController, RobustThroughputEstimatorSettings};
use std::time::{Duration, Instant};

/// The acknowledged-rate window goog_cc reads its decrease and probing from,
/// set by time.
///
/// goog_cc's `RobustThroughputEstimator` keeps the smallest window holding at
/// least `window_packets` packets and the last `window_duration`, then caps it
/// at `max_window_packets` (500) however short that makes it. The defaults
/// assume packets of a video stream's size, and the cap meant a length of time
/// only as long as the packets stayed small: with one record per datagram
/// (about 120 bytes) it held the window to about 50 ms at 10 Mbit/s, while an
/// MTU-sized bundle of 7 or 8 records is one packet and the same cap lets the
/// window span about a second. After a stall (a send window that opened again
/// after a rise of the round trip) that second still held the stalled flow:
/// the delay detector read `0.85 x` an acknowledged rate of 3 Mbit/s on a
/// 10 Mbit/s link and the estimate fell to it (p8e: back at 0.6 x the link after
/// 11 s, where it took 2.3 s).
///
/// A window of the last 300 ms does not depend on the packet size. It is as
/// long as the one-record-per-datagram window was at the rates where that
/// mattered (50 ms there, but p_b_population's H.264 streams, a few hundred
/// 1200-byte packets a second, need more than 100 ms of them: at 100 ms the
/// step-up on its 20 ms row found the headroom on 8 of 24 timelines, at 300 ms
/// on 16).
///
/// FIXME(fec-v2): on a sparse stream the 300 ms window slows the estimate's
/// climb after the link rises: in `openh264_scene_6b` the coded rate reached
/// only about 1.45 x its level 10 s after a 0.8 -> 3 Mbit/s rise, against 2 x
/// with a 1 s window (700 ms passes too, 400-500 ms fails a seed). Accepted
/// for PR #319 (owner, 2026-11-16); the scene's rise was moved from 5 s to
/// 4 s to give the climb time. Revisit: a window between 300 and 700 ms that
/// still passes p8e, or one that scales with the stream's packet rate.
fn acked_rate_window() -> RobustThroughputEstimatorSettings {
    RobustThroughputEstimatorSettings {
        window_duration: TimeDelta::from_millis(300),
        ..RobustThroughputEstimatorSettings::default()
    }
}

/// Floor and ceiling handed to the controller. The floor keeps a badly
/// congested link usable rather than collapsing to nothing; the ceiling stops
/// a probe overshoot proposing an absurd rate on a fast LAN.
const MIN_BPS: i64 = 200_000;
const MAX_BPS: i64 = 200_000_000;

/// How far the estimate may sit from the ceiling before goog_cc is told to
/// move. Empirical: a hysteresis against re-seeding on the rounding of the
/// windowed rate (one acknowledgement batch moves it by a few percent), kept
/// under goog_cc's `PROBE_UNCERTAINTY` (5 %), the margin to which one of its
/// probe results is known.
const FOLLOW_TOLERANCE: f64 = 0.02;

/// The estimate is lifted only while the path has no standing queue: quinn's
/// smoothed RTT within this factor of its minimum over `MIN_RTT_SPAN`.
///
/// With a filling queue `A >= 0.85 x offered` keeps holding up to `K x C`
/// (the queue absorbs the excess), so lifting there walks the estimate to
/// `K x capacity` and keeps it there, and the delay detector's back-off (to
/// `0.85 x A`) is undone a window later. The owner's bound is a quarter of
/// the base RTT; it is also what CoDel's own 5 ms target adds to a 20 ms
/// path, so the standing queue CoDel allows does not block the lift, and a
/// drop-tail buffer that has filled (100 ms) does. quinn's `PathStats`
/// carries only the smoothed RTT, not its minimum, so the minimum is the
/// least smoothed RTT seen in the span (a slight over-estimate of the true
/// minimum, which errs towards lifting).
const QUEUE_RTT_RATIO: f64 = 1.25;

/// A gap in the acknowledgements is a stall when it is longer than this
/// fraction of the reading window (and the round trip).
const GAP_WINDOW_FRACTION: u64 = 4;

/// ...and by at least this much: transient self-queueing (a burst of our own
/// datagrams ahead of an acknowledged one) and jitter in quinn's round trip
/// samples move its smoothed round trip by a few ms with no queue standing on
/// the path. At a round trip of 4 ms the ratio alone calls that wander a queue
/// (a quarter of 4 ms is 1 ms):
/// measured on a 1 ms one-way link, 1 to 3 timelines in 96 collapsed and 80 -
/// 87 % of a CBR stream's readings were saturated; with it none collapse and
/// 74 - 80 % are. Swept 0, 2, 3, 4, 5, 6 ms against the step-down scene at 1
/// and 2 ms one-way: backpressure skips are 130 at 0 - 3 ms and 91 - 104 from 4
/// (master 105 and 121); the reaction time is bimodal at 0.4 or 1.2 s there at
/// every value.
const QUEUE_MIN_EXCESS_US: u64 = 5_000;

/// Span of the minimum round trip: twice goog_cc's `MIN_TIME_BETWEEN_ALR_PROBES`
/// (5 s), the longest it waits to re-measure a path, so a route change ages out
/// of it on the same scale as everything else the estimator forgets.
const MIN_RTT_SPAN_US: u64 = 10_000_000;

/// A lift counts as undone when the estimate one feedback later is under this
/// share of the value handed to goog_cc (2 % below it: the `FOLLOW_TOLERANCE`
/// around the ceiling is the noise a lift is measured against).
const LIFT_SURVIVAL: f64 = 0.98;

/// `AIMD_BACKOFF` as a float for the delivery test.
const AIMD_BACKOFF_F: f64 = super::ceiling::AIMD_BACKOFF;

/// goog_cc's loss-based estimator (v2), with one change to its defaults.
///
/// `not_increase_if_inherent_loss_less_than_average_loss` refuses every
/// increase of the loss-limited estimate while the average reported loss is
/// above the estimator's own *inherent loss* (the loss it believes the channel
/// has whatever the rate). That estimate is a one-step Newton update that
/// lags the average and is capped at `inherent_loss_upper_bound_offset`
/// (0.05) plus `75 kbit/s / bandwidth`, so on a path with steady random loss
/// the average can sit above it for long stretches and the estimate, once
/// down, climbs back only on the observations where it does not. Measured:
/// H.264 over 1 % i.i.d. loss at 100 Mbit/s averaged 57 Mbit/s with the guard
/// on, against 95 without it, and at 5 % 39 against 94. Off,
/// steady loss is what the model explains as inherent, and loss that rises
/// with the sending rate is still caught where the model looks for it: the
/// likelihood adds `(rate - bandwidth) / rate` to the loss probability of an
/// observation sent above the candidate bandwidth, the preference for a higher
/// bandwidth turns into one for a lower at 15 % loss
/// (`loss_threshold_of_high_bandwidth_preference`), and the instant upper
/// bound `75 kbit/s / (loss - 0.05)` caps the estimate outright. A 25 ms
/// tail-drop buffer behind a 15 to 5 Mbit/s step lost 17 % of the stream with
/// goog_cc loss-blind and about 2 % with this (see
/// `browserless_nested_cc::s4_*`). Raising the two 0.05 offsets to 0.10 on top
/// bought nothing at 1 % and little at 5 % and made the tail-drop scene lose
/// 3 % and settle later; `use_byte_loss_rate` made 5 % worse. Both were left
/// at the defaults.
fn loss_based_bwe_v2() -> goog_cc::LossBasedBweV2Config {
    goog_cc::LossBasedBweV2Config {
        not_increase_if_inherent_loss_less_than_average_loss: false,
        ..Default::default()
    }
}

/// goog_cc's `pacing_factor`: the pacer rate is this times the loss-based
/// target. 1.0: the pacer rate is the estimate.
///
/// libwebrtc's default is 2.5, its allowance for draining an encoder's frames
/// ahead of the rate it is asked to hold. Here it put the pacer rate at 2.5 x
/// the estimate, out of step with the rate admission is held to; at 1.0 the
/// pacer rate is the estimate.
const PACING_FACTOR: f64 = 1.0;

/// Unwraps `u32` transmission ids (`wire_seq`, wrapping by design) onto an
/// `i64` line that increases with them, anchored on the newest seen.
#[derive(Debug, Default, Clone, Copy)]
struct SeqUnwrap {
    anchor: Option<i64>,
}

impl SeqUnwrap {
    /// `seq` unwrapped (within 2^31 of the anchor), moving the anchor up to it.
    fn advance(&mut self, seq: u32) -> i64 {
        let u = match self.anchor {
            None => i64::from(seq),
            Some(a) => a + i64::from(seq.wrapping_sub(a as u32) as i32),
        };
        if self.anchor.is_none_or(|a| u > a) {
            self.anchor = Some(u);
        }
        u
    }
}

pub(crate) struct GoogCcDriver {
    ctl: SendCtl,
    /// Only the arrival series needs unwrapping: it crosses the wire as 16
    /// bits of the client's clock. `server_emit_us` is already monotonic
    /// server-side microseconds and is used directly.
    arrival_time: Lo16Timeline,
    /// Unwraps transmission ids into goog_cc's packet sequence numbers.
    seq: SeqUnwrap,
    base: Instant,
    /// goog_cc's raw target rate. What is published is `gate.published`.
    estimate_bps: u64,
    /// The growth gate (D1) and what it last published.
    gate: GrowthGate,
    /// Our own reading of each probe cluster's delivered rate, proved to the
    /// gate whatever goog_cc published.
    probe_meter: ProbeMeter,
    published_bps: u64,
    /// What the last ceiling reading that existed said. `Unknown` before any
    /// reading: no history, no restriction (see `step_gate`).
    saturation: Saturation,
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
    /// Requested, not-yet-consumed probe clusters (BWE Stage 2.4), converted
    /// out of goog_cc's units by `to_probe_request` and surfaced one at a
    /// time by `take_probe_request`.
    ///
    /// This used to hold only the most recent config, on the reasoning that a
    /// config never surfaced is indistinguishable from one never requested.
    /// That is true of any single config and false of the set: goog_cc's
    /// exponential probing asks for a *ladder*, and the first request of a
    /// session was measured here as a pair at 6 Mbps and 12 Mbps. Keeping the
    /// last discarded the lower rung — the one more likely to be answerable
    /// on a slow link. Bounded by `MAX_PENDING_PROBE_REQUESTS`.
    pending_probe_requests: std::collections::VecDeque<super::ProbeRequest>,
    stats: crate::transport::bwe::BweStats,
    /// The acknowledged-rate ceiling (`ceiling.rs`) and the figures it is
    /// made of. `active_ceiling_bps` is `Some` only while the sender was
    /// saturated; it bounds what `snapshot` reports.
    ceiling: EstimateCeiling,
    acked_cum_bytes: f64,
    active_ceiling_bps: Option<u64>,
    /// Countable bytes per token spent, last measured (see `BweSnapshot`).
    payload_fraction: f64,
    /// When goog_cc was last given an estimate (`reseed`).
    last_reseed_us: Option<u64>,
    /// The ceiling is not read before this instant (`restore_from`).
    ceiling_hold_until_us: u64,
    /// Least smoothed RTT seen over `MIN_RTT_SPAN_US`, microseconds.
    min_rtt: super::sliding_min::SlidingMin<u64, u64>,
    /// The raw estimate a lift handed goog_cc, until the next feedback has
    /// been absorbed and compared with it.
    pending_lift_bps: Option<u64>,
    /// Lifts to judge a second after they were made: `(due_us, lifted_bps)`.
    /// goog_cc's loss-based estimator only moves on a completed observation
    /// (at least 250 ms of sends), so "one feedback later" can be inside one
    /// observation and says little about whether a lift held.
    lifts_to_judge: std::collections::VecDeque<(u64, u64)>,
}

/// How long after a lift it is judged held or not.
const LIFT_HOLD_US: u64 = 1_000_000;
/// Bound on `lifts_to_judge` (lifts are at most one per window).
const MAX_LIFTS_TO_JUDGE: usize = 64;

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
    /// Most probe requests held at once. goog_cc asks for a ladder of at
    /// most a few clusters; anything beyond this is a backlog the session
    /// will never work through while the rates still describe the link.
    const MAX_PENDING_PROBE_REQUESTS: usize = 4;

    /// Do not read the estimate ceiling until `hold` after `now`. A probe
    /// burst spends the whole grant for a moment while its acknowledgements
    /// are still a round trip (plus the queue it built) away, so the ceiling
    /// would read a saturated sender with almost nothing acknowledged and
    /// clamp the estimate to its floor.
    pub(crate) fn hold_ceiling(&mut self, now: Instant, hold: Duration) {
        let until = self.micros_since_base(now) + hold.as_micros() as u64;
        self.ceiling_hold_until_us = self.ceiling_hold_until_us.max(until);
    }

    /// Whether the ceiling is held off at `now` (see [`Self::hold_ceiling`]).
    #[cfg(test)]
    pub(crate) fn ceiling_held(&self, now: Instant) -> bool {
        self.micros_since_base(now) < self.ceiling_hold_until_us
    }

    /// goog_cc is never given a `NetworkStateEstimate` (the model controller
    /// feeds no capacity hint), so `ProbeController::time_for_network_state_probe`
    /// returns false at its first check: every network-state path needs one.
    /// The defaults would keep it quiet anyway: `network_state_interval` is
    /// infinite and `est_lower_than_network_ratio` is 0 (its 3 s interval is
    /// unreachable). Headroom is looked for by the t=0 ladder, the repeated
    /// initial probes and ALR probing.
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
            // Both of these default to `None`, which leaves the
            // `ProbeController` issuing exactly one ladder of probes at
            // t=0 and then never probing again for the life of the
            // session. Measured before setting them, on a 12 s
            // continuous-traffic scene: two windows opened, both at
            // t=0 ms, and nothing else ever opened.
            stream_based_config: StreamsConfig {
                at_time,
                // Without this, `enable_periodic_alr_probing` is never
                // called and stays at its `false` default, so
                // `ProbeController::time_for_alr_probe` returns `false`
                // unconditionally. A tile-mode screen-sharing source is
                // application-limited almost all of the time (it sends what
                // the screen produced, not what the link could carry), and
                // goog_cc's own ALR detector sees that; ALR probing is then
                // what re-measures capacity after start-up (measured: 5.5
                // Mbps estimated on a link that stepped up to 20 Mbps
                // mid-scene, without it). The fork's ALR detector budgets
                // 80 % of the estimate (`alr_bandwidth_usage_percent`),
                // lets unused budget build up over a 500 ms window and starts
                // the region above 40 % full. A rate-capped H.264 stream held
                // at 0.8 x its estimate refills the budget exactly as fast as
                // it spends it, so it is not reliably seen as
                // application-limited, and for that stream ghostframe starts
                // probe clusters of its own (`h264_rate`: `h264_app_probe`),
                // which this estimator measures like any other because it
                // takes the cluster id from the packets.
                requests_alr_probing: Some(true),
                // The pacer rate is the estimate, not libwebrtc's `2.5 x`.
                // See `PACING_FACTOR`.
                pacing_factor: Some(PACING_FACTOR),
                // The t=0 ladder is issued before the first frame has been
                // captured, so the scheduler queue is empty and the
                // windows close having emitted nothing (`packets_sent=0`,
                // not an under-fill). This re-probes every 1 s for the
                // first 5 s, by which point there is real traffic for a
                // window to measure.
                enable_repeated_initial_probing: Some(true),
                ..Default::default()
            },
            field_trials: FieldTrials {
                robust_throughput_estimator_settings: acked_rate_window(),
                loss_based_bwe_v2: loss_based_bwe_v2(),
                ..FieldTrials::default()
            },
        };
        let mut ctl = GoogCcNetworkController::new(
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
        );
        // BWE Stage 2.4: `ProbeController` starts in `State::Init` and can
        // only ever leave it — for exponential (or any later) probing — if
        // `network_available` has been set `true` via `on_network_availability`
        // first (`probe_controller.rs`'s `set_bitrates`/`on_network_availability`
        // both gate on it). Nothing else in this driver ever calls it: we have
        // no separate "network up/down" signal, and a live `IoBridge` session
        // implies an available network for as long as it exists. Without this
        // call `ProbeController` is stuck in `Init` forever and
        // `probe_cluster_configs` is permanently empty — discovered while
        // writing Task 4's bench, which could not otherwise drive a real
        // probe request out of the controller at all. `start_bitrate` is
        // still zero at this point (only set later via `set_bitrates`, which
        // `on_process_interval` triggers from `initial_config` on the first
        // `update()` call), so this call itself never yields a probe cluster
        // — it only flips the flag that lets the first real one through.
        let _ = ctl.on_network_availability(NetworkAvailability {
            at_time,
            network_available: true,
        });
        Self {
            ctl: SendCtl(ctl),
            arrival_time: Lo16Timeline::default(),
            base: now,
            estimate_bps: initial_bps,
            gate: GrowthGate::default(),
            probe_meter: ProbeMeter::default(),
            published_bps: initial_bps,
            saturation: Saturation::Unknown,
            samples_seen: 0,
            path_rtt: None,
            implausible_rtt_samples: 0,
            seq: SeqUnwrap::default(),
            pacer_rate_bps: None,
            pending_probe_requests: std::collections::VecDeque::new(),
            stats: crate::transport::bwe::BweStats::default(),
            ceiling: EstimateCeiling::default(),
            acked_cum_bytes: 0.0,
            active_ceiling_bps: None,
            payload_fraction: 1.0,
            last_reseed_us: None,
            ceiling_hold_until_us: 0,
            min_rtt: Default::default(),
            pending_lift_bps: None,
            lifts_to_judge: std::collections::VecDeque::new(),
        }
    }

    /// Wire bytes of the transmissions recorded so far, cumulative.
    pub(crate) fn note_offered(&mut self, now: Instant, cumulative_bytes: f64) {
        let now_us = self.micros_since_base(now);
        self.ceiling.note_offered(now_us, cumulative_bytes);
    }

    /// The admission bucket's cumulative grant and spend, as of the end of an
    /// admission pass.
    pub(crate) fn note_admission(&mut self, now: Instant, granted: f64, spent: f64) {
        let now_us = self.micros_since_base(now);
        self.ceiling.note_admission(now_us, granted, spent);
        self.refresh_ceiling(now);
    }

    fn micros_since_base(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.base).as_micros() as u64
    }

    /// Re-read the ceiling. Called whenever either of its inputs moved.
    fn refresh_ceiling(&mut self, now: Instant) {
        let now_us = self.micros_since_base(now);
        let srtt_us = self.path_rtt.map_or(0, |d| d.as_micros() as u64);
        let reading: Option<CeilingReading> = if now_us < self.ceiling_hold_until_us {
            None
        } else {
            self.ceiling.read(now_us, srtt_us)
        };
        // The gate reads the same reading: saturated, it is off.
        if let Some(r) = reading {
            self.saturation = if r.saturation >= SATURATION {
                Saturation::Yes {
                    // What is published while saturated is bounded by this
                    // (`bounded`), so it is what the gate proves.
                    ceiling_bps: r.ceiling_bps.map(|c| (c as u64).max(MIN_BPS as u64)),
                }
            } else {
                Saturation::No
            };
            self.step_gate(now_us, false);
        }
        let standing_queue = self.standing_queue();
        // A window holding a stall in its acknowledgements (`ack_gap_is_stall`:
        // a blackout, a pacer trickling out one big frame) measures the stall,
        // not the path, unless the round trip shows the path slowed: a sender
        // that resumes at a stale estimate after an idle builds a queue, and
        // the low rate of the window is then the cut it needs. With no queue
        // the stall has no other cause than itself, and the bound last read
        // from a window without one stays. Taken at face value it published 5
        // to 10 times too little, and the sender, paced to that and spending
        // all of it, was then its own evidence: each window read what the
        // last had allowed. `standing_queue` is true for up to ten seconds
        // after the base round trip rose and under WiFi jitter: there this is
        // the old behaviour, and the fix gives those paths nothing.
        let held = reading.is_some_and(|r| {
            r.ceiling_bps.is_some() && !standing_queue && self.ack_gap_is_stall(now_us, srtt_us)
        });
        if held {
            self.stats.ceiling_reads_held += 1;
        } else {
            self.active_ceiling_bps = reading.and_then(|r| r.ceiling_bps).map(|c| c as u64);
        }
        if let Some(r) = reading {
            self.payload_fraction = r.payload_fraction;
            self.stats.payload_fraction_ppm = (r.payload_fraction * 1e6) as u64;
            self.stats.ceiling_reads += 1;
            self.stats.ceiling_acked_bps = r.acked_bps as u64;
            self.stats.ceiling_saturated_reads += r.ceiling_bps.is_some() as u64;
        }
        self.stats.ceiling_bps = self.active_ceiling_bps.unwrap_or(0);
        if self
            .active_ceiling_bps
            .is_some_and(|c| c < self.published_bps)
        {
            self.stats.ceiling_binding_reads += 1;
        }
        if let Some(r) = reading.filter(|_| !held) {
            self.follow_ceiling(now, now_us, srtt_us, r, standing_queue);
        }
    }

    /// The window holds a stretch without acknowledgements that is a stall and
    /// not the cadence of the traffic: longer than the round trip and longer
    /// than a quarter of the window (37 ms at the window's 150 ms floor). A gap
    /// under that still dilutes the window's rate, by at most a quarter of it.
    /// The floor is what keeps a low-latency path honest: the client flushes
    /// its acknowledgements every 5 ms and a CBR stream leaves most of a frame
    /// interval (33 ms at 30 fps) idle, so against a round trip alone (2 ms)
    /// every window holds a "gap" and the ceiling would never be read.
    fn ack_gap_is_stall(&self, now_us: u64, srtt_us: u64) -> bool {
        let floor = EstimateCeiling::window_us(srtt_us) / GAP_WINDOW_FRACTION;
        self.ceiling.ack_gap_us(now_us, srtt_us) > srtt_us.max(floor)
    }

    /// quinn's round trip is well above its minimum: a queue is standing.
    pub(crate) fn standing_queue(&self) -> bool {
        match (self.path_rtt, self.min_rtt.min()) {
            (Some(rtt), Some(min)) => {
                let rtt = rtt.as_micros() as u64;
                rtt as f64 > QUEUE_RTT_RATIO * min as f64 && rtt >= min + QUEUE_MIN_EXCESS_US
            }
            _ => false,
        }
    }

    /// Keep goog_cc's own estimate level with the ceiling: hand it the ceiling
    /// as its new estimate when it is above, and, while the path is showing no
    /// limit, when it is below. At most once per window.
    ///
    /// *Above.* Left alone, goog_cc's estimate keeps running at its additive
    /// limit and the published one is held at the ceiling by `bounded`; its
    /// probe controller would then reason from an estimate nobody sends at.
    /// Handing it the ceiling is also the re-measurement the estimate asks
    /// for: goog_cc continues from what the path was seen to carry, and its
    /// delay detector takes it from there.
    ///
    /// *Below.* With the pacer at the estimate, goog_cc's AIMD grows by one
    /// packet per round trip once it believes it is near capacity, which it
    /// does after any overuse: nothing in what it sees says the path has room,
    /// because the sender never offers more than the estimate allows. The ceiling is the evidence it lacks: the sender used
    /// what it was granted (`saturated`), the path delivered at least
    /// `AIMD_BACKOFF` of what was offered (it is not dropping or queueing the
    /// excess), and the estimate is under `K x` what was delivered. Raising it
    /// to that bound grows it by `K` per window while the path has room. The
    /// delay detector is untouched and still lowers the estimate when the
    /// path does not (to `AIMD_BACKOFF` of the acknowledged rate); the next
    /// lift is a window later, no sooner.
    fn follow_ceiling(
        &mut self,
        now: Instant,
        now_us: u64,
        srtt_us: u64,
        r: CeilingReading,
        standing_queue: bool,
    ) {
        let Some(ceiling) = r.ceiling_bps else { return };
        let window_us = EstimateCeiling::window_us(srtt_us);
        let cooled = self
            .last_reseed_us
            .is_none_or(|t| now_us.saturating_sub(t) >= window_us);
        if !cooled || r.acked_bps <= 0.0 {
            return;
        }
        // The published value: saturated (the only time there is a ceiling) the
        // gate is off and it is goog_cc's own.
        let est = self.published_bps as f64;
        let ceiling = ceiling.max(MIN_BPS as f64);
        if est > ceiling * (1.0 + FOLLOW_TOLERANCE) {
            // A gap in the acknowledgements (a stall, a blackout) makes the
            // window's rate a measurement of the gap, not of the path: the
            // bound is still published (`bounded`), but goog_cc is not told
            // to continue from it.
            if self.ack_gap_is_stall(now_us, srtt_us) {
                self.stats.ceiling_reseeds_skipped_gap += 1;
                return;
            }
            self.reseed(now, now_us, ceiling);
            self.stats.ceiling_reseeds_down += 1;
            if self.loss_state().is_limited() {
                self.stats.ceiling_reseeds_down_loss_limited += 1;
            }
        } else if est < ceiling * (1.0 - FOLLOW_TOLERANCE)
            && r.acked_bps >= AIMD_BACKOFF_F * r.offered_bps
        {
            if standing_queue {
                self.stats.ceiling_lifts_blocked_queue += 1;
                return;
            }
            self.reseed(now, now_us, ceiling);
            self.pending_lift_bps = Some(ceiling as u64);
            if self.lifts_to_judge.len() < MAX_LIFTS_TO_JUDGE {
                self.lifts_to_judge
                    .push_back((now_us + LIFT_HOLD_US, ceiling as u64));
            }
            self.stats.ceiling_reseeds_up += 1;
        }
    }

    /// goog_cc's loss-based state, in this module's terms.
    fn loss_state(&self) -> super::LossState {
        use goog_cc::LossBasedState as G;
        match self.ctl.0.loss_based_state() {
            G::DelayBasedEstimate => super::LossState::DelayBased,
            G::Increasing => super::LossState::Increasing,
            G::IncreaseUsingPadding => super::LossState::IncreasingWithPadding,
            G::Decreasing => super::LossState::Decreasing,
        }
    }

    /// Tell goog_cc to continue from `bps`: the one API it has for an
    /// estimate the application knows better (`TargetRateConstraints`).
    fn reseed(&mut self, now: Instant, now_us: u64, bps: f64) {
        let at = self.to_timestamp(now);
        let rate = (bps as i64).clamp(MIN_BPS, MAX_BPS);
        let upd = self
            .ctl
            .0
            .on_target_rate_constraints(TargetRateConstraints {
                at_time: at,
                min_data_rate: Some(DataRate::from_bits_per_sec(MIN_BPS)),
                max_data_rate: Some(DataRate::from_bits_per_sec(MAX_BPS)),
                starting_rate: Some(DataRate::from_bits_per_sec(rate)),
            });
        self.absorb(upd);
        self.step_gate(now_us, false);
        self.last_reseed_us = Some(now_us);
    }

    /// Resume the estimate from `bps`: the idle-restart policy's cold start
    /// (D1 section 6.6). The same API the ceiling follower uses; the held
    /// estimate is stale, and the evidence the ceiling needs to lift it again
    /// is what the restarted flow produces.
    pub(crate) fn restart_from(&mut self, now: Instant, bps: u64) {
        let now_us = self.micros_since_base(now);
        self.gate.reset();
        self.probe_meter.reset();
        self.reseed(now, now_us, bps as f64);
    }

    /// Continue the estimate from `bps` after the base round trip rose, and
    /// keep the ceiling from reading until its acknowledgement window holds
    /// nothing from before: a window (`window_us`) after the restore, plus the
    /// round trip the first acknowledgements of the faster flow take to come
    /// back. Before that the window averages the throttled flow with the one
    /// the restored estimate admits, and reads the ramp as the path's limit.
    pub(crate) fn restore_from(&mut self, now: Instant, bps: u64) {
        let now_us = self.micros_since_base(now);
        let srtt_us = self.path_rtt.map_or(0, |d| d.as_micros() as u64);
        self.ceiling_hold_until_us = now_us + EstimateCeiling::window_us(srtt_us) + srtt_us;
        self.gate
            .restore(now_us, (bps as i64).clamp(MIN_BPS, MAX_BPS) as u64);
        self.reseed(now, now_us, bps as f64);
    }

    /// Run the growth gate (D1) on goog_cc's current target. `count` is true
    /// for the step made on a feedback, the only one that is counted and the
    /// only one that earns unprobed growth credit.
    ///
    /// Not saturated is the gate's case. Before any ceiling reading exists
    /// (`Saturation::Unknown`) it is off, as the ceiling is: with no history
    /// there is no evidence either way and start-up is not restricted by a
    /// rule that needs some; but nothing is proved either, the value is only
    /// anchored. While the ceiling is held off (a probe burst, a restore) the
    /// last reading's answer stands.
    fn step_gate(&mut self, now_us: u64, count: bool) {
        let (published, outcome) =
            self.gate
                .step(now_us, self.estimate_bps, self.saturation, count);
        self.published_bps = published;
        if !count {
            return;
        }
        match outcome {
            GateOutcome::Limited => {
                self.stats.growth_held_feedbacks += 1;
                let pct = self.estimate_bps.saturating_mul(100) / published.max(1);
                self.stats.growth_held_max_ratio_pct =
                    self.stats.growth_held_max_ratio_pct.max(pct);
            }
            GateOutcome::Recovered => self.stats.growth_free_recoveries += 1,
            GateOutcome::Passed => {}
        }
    }

    /// `bps` bounded by the ceiling while one applies (never below the
    /// controller's own floor: a stalled link is the loss path's to report).
    fn bounded(&self, bps: u64) -> u64 {
        match self.active_ceiling_bps {
            Some(c) => bps.min(c.max(MIN_BPS as u64)),
            None => bps,
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
    pub(crate) fn update(
        &mut self,
        records: &[AckArrival],
        losses: &[(u32, u64, usize)],
        now: Instant,
        data_in_flight_bytes: usize,
    ) -> BweSnapshot {
        // Records, not losses, gate this. `on_transport_packets_feedback`
        // unwraps `received_with_send_info().max()`, so a report carrying
        // only losses panics inside goog_cc. Callers therefore hold losses
        // back until an acknowledgement is available to accompany them.
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
        // Safe to index: `records` is checked non-empty above.
        let mut send_ms_max = (records[0].server_emit_us / 1_000) as i64;
        let mut packet_feedbacks = Vec::with_capacity(records.len() + losses.len());
        let mut last_derived = Duration::ZERO;
        let mut probe_acks = Vec::new();
        for r in records {
            let send_ms = (r.server_emit_us / 1_000) as i64;
            let recv_ms = self.arrival_time.unwrap_ms(r.client_arrival_ms_lo16) as i64;
            if let Some(tag) = r.probe {
                probe_acks.push((
                    tag,
                    r.server_emit_us,
                    (recv_ms.max(0) as u64) * 1_000,
                    r.size_bytes,
                ));
            }
            send_ms_max = send_ms_max.max(send_ms);
            last_derived = Duration::from_millis((recv_ms - send_ms).max(0) as u64);
            let sent = SentPacket {
                sequence_number: self.seq.advance(r.wire_seq),
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
        // Sequence numbers: goog_cc's loss-based estimator (v2) keys the
        // packets lost within an observation by `sequence_number` and drops a
        // key when a packet of that number is received. Left at the default 0,
        // every loss inserted key 0 and the next received packet removed it,
        // so it counted at most one loss per observation and usually none.
        // Each transmission carries its own id (a retransmission is a new
        // transmission), unwrapped so it stays increasing across the u32 wrap.
        //
        // Losses, reported as feedback entries with no receive time.
        // `PacketResult::is_received` is `!receive_time.is_plus_infinity()`
        // and goog_cc's `lost_packets()` filters on exactly that, so an
        // ACK-only feedback stream reports zero losses however much traffic
        // is being dropped — the loss-based estimator never engages at all.
        // Measured before this: a link dropping from 16 to 4 Mbps under
        // ~750 kB of loss left the estimate pinned at its pre-drop 16.4 Mbps
        // in 5 runs out of 6, a sustained 4x overestimate of a degraded
        // link.
        for &(wire_seq, emit_us, size_bytes) in losses {
            let send_ms = (emit_us / 1_000) as i64;
            send_ms_max = send_ms_max.max(send_ms);
            let sent = SentPacket {
                sequence_number: self.seq.advance(wire_seq),
                send_time: Timestamp::from_millis(send_ms),
                size: DataSize::from_bytes(size_bytes as i64),
                // Never a probe: a retransmission belongs to no cluster
                // (see the emitter's `entry.probe = None`), and the
                // transmission this stands in for was lost, so it measured
                // nothing.
                pacing_info: PacedPacketInfo::default(),
                ..Default::default()
            };
            self.ctl.0.on_sent_packet(sent);
            packet_feedbacks.push(PacketResult {
                sent_packet: sent,
                receive_time: Timestamp::plus_infinity(),
                ..Default::default()
            });
        }
        // goog_cc walks this vector in order; keep sends monotonic so a
        // loss interleaved with acknowledgements lands where it belongs.
        packet_feedbacks.sort_by_key(|f| (f.sent_packet.send_time, f.sent_packet.sequence_number));

        feedback_time = feedback_time.max(Timestamp::from_millis(send_ms_max));
        self.samples_seen += records.len() as u64;
        self.observe_derived_rtt(last_derived);

        let upd = self
            .ctl
            .0
            .on_transport_packets_feedback(TransportPacketsFeedback {
                feedback_time,
                // Real outstanding bytes, from the retransmit cache: an
                // entry lives there from emission until its ACK. goog_cc's
                // congestion-window pushback controller reads this, and fed
                // a hardcoded zero -- as it was until 2026-09-14 -- it has
                // nothing to push back against, so that half of the
                // controller never engages.
                data_in_flight: DataSize::from_bytes(data_in_flight_bytes as i64),
                packet_feedbacks,
                sendless_arrival_times: Vec::new(),
            });
        self.absorb(upd);

        let upd = self.ctl.0.on_process_interval(ProcessInterval {
            at_time: feedback_time,
            ..Default::default()
        });
        self.absorb(upd);

        // What the probe clusters acknowledged in this feedback measured, read
        // as goog_cc reads them: the acknowledgements in receive order, each
        // cluster once after all of its acknowledgements, the last result
        // kept. Proved before the gate steps.
        let now_us = self.micros_since_base(now);
        probe_acks.sort_by_key(|&(_, _, recv_us, _)| recv_us);
        for (tag, send_us, recv_us, size) in probe_acks {
            self.probe_meter.on_ack(tag, send_us, recv_us, size);
        }
        if let Some(rate) = self.probe_meter.finish_feedback() {
            self.gate.note_probe_result(now_us, rate);
            self.stats.growth_probe_proofs += 1;
            self.stats.growth_probe_proved_max_bps =
                self.stats.growth_probe_proved_max_bps.max(rate);
        }
        self.step_gate(now_us, true);

        // What was acknowledged is counted as goog_cc counts it: the wire size
        // of each acknowledged transmission, the bytes the estimate is a rate
        // of.
        self.acked_cum_bytes += records.iter().map(|r| r.size_bytes as f64).sum::<f64>();
        self.ceiling.note_acked(now_us, self.acked_cum_bytes);
        if let Some(rtt) = self.path_rtt {
            self.min_rtt.push(now_us, rtt.as_micros() as u64);
            self.min_rtt
                .expire_before(now_us.saturating_sub(MIN_RTT_SPAN_US));
        }
        // Did the last lift survive one feedback? With goog_cc as pinned
        // before its fork reseeded the loss-based estimator (v2) on a
        // constraints call, about 80 % did not: v2 capped the target back to
        // its old value at the next feedback. Counted, not acted on.
        if let Some(lifted) = self.pending_lift_bps.take() {
            self.stats.ceiling_lifts_checked += 1;
            if (self.estimate_bps as f64) < LIFT_SURVIVAL * lifted as f64 {
                self.stats.ceiling_lifts_undone += 1;
            }
        }
        while let Some(&(due, lifted)) = self.lifts_to_judge.front() {
            if due > now_us {
                break;
            }
            self.lifts_to_judge.pop_front();
            self.stats.ceiling_lifts_checked_1s += 1;
            if (self.estimate_bps as f64) < LIFT_SURVIVAL * lifted as f64 {
                self.stats.ceiling_lifts_undone_1s += 1;
            }
        }
        let state = self.loss_state();
        self.stats.loss_state = state;
        self.stats.loss_state_feedbacks[state.index()] += 1;
        self.refresh_ceiling(now);

        self.snapshot()
    }

    /// The estimate and pacer rate, bounded by the ceiling while one applies.
    pub(crate) fn snapshot(&self) -> BweSnapshot {
        BweSnapshot {
            bitrate_bps: self.bounded(self.published_bps),
            samples_seen: self.samples_seen,
            implausible_rtt_samples: self.implausible_rtt_samples,
            // Held in the proportion the estimate is.
            pacer_rate_bps: self.pacer_rate_bps.map(|p| {
                let held = (p as u128 * self.published_bps as u128
                    / self.estimate_bps.max(1) as u128) as u64;
                self.bounded(held.min(p))
            }),
            payload_fraction: self.payload_fraction,
            stats: self.stats,
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
        for cfg in &upd.probe_cluster_configs {
            // Every requested cluster is queued, not just the last. goog_cc's
            // exponential probing asks for a *ladder* -- the first request of
            // a session is a pair, measured here as 6 Mbps then 12 Mbps -- and
            // keeping only the last discarded the lower rung, which is the one
            // more likely to be answerable on a slow link.
            //
            // Bounded so a burst of requests cannot queue probes the session
            // will still be working through long after they stopped
            // describing the link. Oldest goes first: a stale probe rate is
            // worth less than a fresh one.
            if self.pending_probe_requests.len() == Self::MAX_PENDING_PROBE_REQUESTS {
                self.pending_probe_requests.pop_front();
            }
            let req = Self::to_probe_request(cfg);
            if let Some(slot) = self
                .stats
                .first_probe_targets_bps
                .get_mut(self.stats.probe_requests as usize)
            {
                *slot = req.target_rate_bps;
            }
            self.stats.probe_requests += 1;
            self.pending_probe_requests.push_back(req);
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
        // goog_cc sizes a probe cluster for a continuous pacer, which
        // spreads the cluster's packets across the window so that
        // `last_send - first_send` is a real interval it can divide bytes
        // by. Ours is frame-quantised: a tick hands a whole frame's
        // datagrams to quinn in one instant, so every packet emitted in one
        // tick carries the same `emit_us`. A window shorter than the tick
        // interval can therefore only ever contain a single burst, and
        // goog_cc rejected every such cluster with `send interval: 0 us`.
        //
        // Flooring the window at `MIN_PROBE_TICKS` frame intervals gives it
        // room for that many distinct send timestamps. `min_bytes` below is
        // derived from this same widened duration, so the target rate still
        // has to be *sustained* across the whole window — widening the
        // window makes a cluster harder to complete, not easier.
        const MIN_PROBE_TICKS: f64 = 3.0;
        let min_duration_us =
            (super::super::io_bridge::SCHEDULER_TICK_INTERVAL_US * MIN_PROBE_TICKS) as i64;
        let duration_us = cfg.target_duration.us().max(0).max(min_duration_us);
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
        self.pending_probe_requests.pop_front()
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
                        wire_seq: 0,
                        server_emit_us: emit_us,
                        // 15 ms one-way delay, constant.
                        client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                        size_bytes: 1200,
                        probe: None,
                    }
                })
                .collect();
            d.update(
                &batch,
                &[],
                t0 + Duration::from_millis(20 * step as u64),
                batch.iter().map(|r| r.size_bytes as usize).sum(),
            );
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

    /// goog_cc's loss-based estimator (v2) keys the packets lost inside an
    /// observation by sequence number and forgets a key when a packet of that
    /// number is received. With every packet numbered 0, a loss followed by
    /// received packets cancelled itself, so a path dropping 25 % of what it
    /// was sent looked loss-free and the estimator never limited the rate.
    /// Same sends and same arrivals twice, with and without the lost packets.
    fn drive_with_loss(lost_per_batch: u32) -> (crate::transport::bwe::BweStats, u64) {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(3_000_000, t0);
        let mut next_seq = 0u32;
        for step in 0..1000u32 {
            let mut batch = Vec::new();
            let mut losses = Vec::new();
            for i in 0..12u32 {
                let emit_us = ((step * 20 + i) as u64) * 1_000;
                let wire_seq = next_seq;
                next_seq += 1;
                // The lost packets sit early in the batch, so received packets
                // follow them in send order.
                if i < lost_per_batch {
                    losses.push((wire_seq, emit_us, 1200usize));
                } else {
                    batch.push(AckArrival {
                        wire_seq,
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                        size_bytes: 1200,
                        probe: None,
                    });
                }
            }
            d.update(
                &batch,
                &losses,
                t0 + Duration::from_millis(20 * step as u64 + 30),
                14_400,
            );
        }
        (d.snapshot().stats, d.snapshot().bitrate_bps)
    }

    /// Steady random loss must not freeze the loss-limited estimate: with the
    /// guard on, H.264 over 1 % i.i.d. loss at 100 Mbit/s stayed at 57 Mbit/s
    /// (browserless_fec_adapt control_law_sweep, cell H264/iid1/100M).
    #[test]
    fn the_inherent_loss_guard_that_froze_the_estimate_on_steady_loss_is_off() {
        let c = super::loss_based_bwe_v2();
        assert!(c.enabled, "the loss-based half is on");
        assert!(!c.not_increase_if_inherent_loss_less_than_average_loss);
    }

    #[test]
    fn losses_followed_by_received_packets_limit_the_estimate() {
        let (clean, clean_bps) = drive_with_loss(0);
        let (lossy, lossy_bps) = drive_with_loss(3);
        let limited =
            |s: &crate::transport::bwe::BweStats| s.loss_state_feedbacks[1..].iter().sum::<u64>();
        assert_eq!(limited(&clean), 0, "no loss, no loss limit: {clean:?}");
        assert!(
            limited(&lossy) > 100,
            "25 % loss never reached the loss-based estimator: {lossy:?}"
        );
        assert!(
            lossy_bps < clean_bps,
            "loss did not lower the estimate: {lossy_bps} vs {clean_bps}"
        );
    }

    #[test]
    fn transmission_ids_unwrap_across_the_u32_wrap() {
        let mut u = super::SeqUnwrap::default();
        let a = u.advance(u32::MAX - 2);
        let b = u.advance(u32::MAX - 1);
        let c = u.advance(0); // NO_ACK (u32::MAX) is skipped by the allocator
        let d = u.advance(1);
        assert!(a < b && b < c && c < d, "{a} {b} {c} {d}");
        // A late straggler from before the wrap keeps its place and does not
        // move the anchor back.
        assert_eq!(u.advance(u32::MAX - 1), b);
        assert_eq!(u.advance(2), d + 1);
    }

    /// Drive `d` for `secs` with a clean path delivering `ACKED_BPS`, and an
    /// admission bucket that was granted `GRANT_BPS` and spent `spend` of it.
    /// What was offered is what was acknowledged (delivery is complete).
    const ACKED_BPS: f64 = 12.0 * 1200.0 * 8.0 / 0.020;

    fn drive_clean(d: &mut GoogCcDriver, t0: Instant, secs: u64, spend: f64) {
        drive_clean_then_silence(d, t0, secs, spend, 0, u64::MAX);
    }

    /// `drive_clean`, then `silence_ms` in which the bucket keeps granting and
    /// spending and nothing is acknowledged (a stall).
    fn drive_clean_then_silence(
        d: &mut GoogCcDriver,
        t0: Instant,
        secs: u64,
        spend: f64,
        silence_ms: u64,
        queue_after_ms: u64,
    ) -> crate::transport::bwe::BweStats {
        const GRANT_BPS: f64 = 9.0e6;
        d.note_path_rtt(Duration::from_millis(30));
        let (mut granted, mut spent, mut offered) = (0.0f64, 0.0f64, 0.0f64);
        for step in 0..(secs * 50) as u32 {
            let now = t0 + Duration::from_millis(20 * step as u64);
            // From `queue_after_ms` the smoothed round trip is double its
            // minimum: a queue is standing.
            if 20 * step as u64 == queue_after_ms {
                d.note_path_rtt(Duration::from_millis(60));
            }
            granted += GRANT_BPS / 8.0 * 0.020;
            spent += GRANT_BPS / 8.0 * 0.020 * spend;
            offered += 12.0 * 1200.0;
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    AckArrival {
                        wire_seq: 0,
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                        size_bytes: 1200,
                        probe: None,
                    }
                })
                .collect();
            d.note_offered(now, offered);
            d.note_admission(now, granted, spent);
            d.update(&batch, &[], now, 14_400);
        }
        let end = t0 + Duration::from_millis(secs * 1000);
        // The first 40 ms of silence are still inside a round trip's grace
        // (srtt 30 ms): the snapshot is taken once the gap is longer.
        let mut before_silence = d.snapshot().stats;
        for ms in 1..=silence_ms {
            let now = end + Duration::from_millis(ms);
            granted += GRANT_BPS / 8.0 * 0.001;
            spent += GRANT_BPS / 8.0 * 0.001 * spend;
            d.note_offered(now, offered);
            d.note_admission(now, granted, spent);
            if ms == 40 {
                before_silence = d.snapshot().stats;
            }
            if ms == 100 {
                // One late batch: the window holds some delivery (so a rate
                // is read) across a stretch of silence on either side.
                let step = (secs * 50) as u32;
                let batch: Vec<AckArrival> = (0..12u32)
                    .map(|i| {
                        let emit_us = ((step * 20 + i) as u64) * 1_000;
                        AckArrival {
                            wire_seq: 0,
                            server_emit_us: emit_us,
                            client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                            size_bytes: 1200,
                            probe: None,
                        }
                    })
                    .collect();
                d.update(&batch, &[], now, 14_400);
            }
        }
        before_silence
    }

    /// A saturated sender on a path whose round trip is shorter than the
    /// acknowledgement cadence: `rtt_ms` against batches every `period_ms`.
    /// Every window holds a stretch longer than twice the round trip without
    /// an acknowledgement, and none of them is a stall: the bound must be
    /// published and the estimate lifted as on any other path.
    fn drive_low_rtt(rtt_ms: u64, period_ms: u64) -> GoogCcDriver {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        d.note_path_rtt(Duration::from_millis(rtt_ms));
        let (mut granted, mut spent, mut offered) = (0.0f64, 0.0f64, 0.0f64);
        let per_batch = (period_ms * 6) as u32; // 6 packets a millisecond
        for step in 0..(12_000 / period_ms) as u32 {
            let now = t0 + Duration::from_millis(period_ms * step as u64);
            granted += 9.0e6 / 8.0 * period_ms as f64 / 1000.0;
            spent += 9.0e6 / 8.0 * period_ms as f64 / 1000.0;
            offered += per_batch as f64 * 1200.0 * 0.8;
            let batch: Vec<AckArrival> = (0..per_batch)
                .map(|i| {
                    let emit_us = (period_ms * step as u64) * 1_000 + u64::from(i) * 100;
                    AckArrival {
                        wire_seq: 0,
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: ((emit_us / 1_000 + rtt_ms / 2) & 0xFFFF) as u16,
                        size_bytes: 960,
                        probe: None,
                    }
                })
                .collect();
            d.note_offered(now, offered);
            d.note_admission(now, granted, spent);
            d.update(&batch, &[], now, 14_400);
        }
        d
    }

    /// Transient self-queueing and round trip jitter move quinn's smoothed
    /// round trip by a few ms with no queue on the path: on a 4 ms path that
    /// is not one, however large a share of the base it is.
    #[test]
    fn a_queue_needs_to_stand_a_few_ms_over_the_minimum() {
        let mut d = drive_low_rtt(4, 5);
        assert!(!d.standing_queue(), "premise: no queue at the base");
        d.note_path_rtt(Duration::from_millis(8));
        assert!(!d.standing_queue(), "2x the base but 4 ms over it: jitter");
        d.note_path_rtt(Duration::from_millis(10));
        assert!(d.standing_queue(), "6 ms over the base is a queue");
    }

    /// Mutation: measure the gap against the round trip alone and both cases
    /// hold every reading (4738 of 4738 at 2 ms / 5 ms) and lift nothing.
    #[test]
    fn a_path_faster_than_its_acknowledgement_cadence_still_gets_a_ceiling() {
        for (rtt_ms, period_ms) in [(2, 5), (4, 10)] {
            let st = drive_low_rtt(rtt_ms, period_ms).snapshot().stats;
            assert!(
                st.ceiling_saturated_reads > 100,
                "{rtt_ms}/{period_ms} ms: premise: readings were saturated ({st:?})"
            );
            assert_eq!(
                st.ceiling_reads_held, 0,
                "{rtt_ms}/{period_ms} ms: readings were set aside as stalls ({st:?})"
            );
            assert!(
                st.ceiling_bps > 0,
                "{rtt_ms}/{period_ms} ms: no bound published"
            );
            assert!(
                st.ceiling_reseeds_up > 0,
                "{rtt_ms}/{period_ms} ms: the estimate was never lifted ({st:?})"
            );
        }
    }

    /// While the sender uses what it is granted, the published estimate and
    /// pacer rate stay within `HEADROOM x` what the path acknowledged, whatever
    /// goog_cc's own estimate is doing.
    #[test]
    fn a_saturated_sender_is_held_to_headroom_times_the_acknowledged_rate() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        drive_clean(&mut d, t0, 12, 1.0);
        let snap = d.snapshot();
        let bound = crate::transport::bwe::ceiling::HEADROOM * ACKED_BPS;
        assert!(
            snap.stats.ceiling_saturated_reads > 0,
            "premise: the ceiling was never applied ({:?})",
            snap.stats
        );
        // The window holds seven or eight 20 ms batches, so the acknowledged
        // rate it reads is the true one to within one batch (about 13 %).
        let ceiling = snap.stats.ceiling_bps as f64;
        assert!(
            (0.85 * bound..=1.15 * bound).contains(&ceiling),
            "the ceiling {ceiling} is not K x the acknowledged rate {bound}"
        );
        assert!(
            snap.bitrate_bps as f64 <= ceiling,
            "published {} above the ceiling {ceiling}",
            snap.bitrate_bps
        );
        assert!(
            snap.pacer_rate_bps.unwrap() as f64 <= ceiling,
            "pacer {:?} above the ceiling {ceiling}",
            snap.pacer_rate_bps
        );
        // Premise for the above meaning something: goog_cc's own estimate
        // would have been higher (the ceiling lowered what is published).
        assert!(
            snap.stats.ceiling_binding_reads > 0 || snap.stats.ceiling_reseeds_down > 0,
            "the ceiling never bound: {:?}",
            snap.stats
        );
    }

    /// The lift is not made while a queue stands: with quinn's round trip at
    /// twice its minimum, the lift count stops where a path with no queue goes
    /// on lifting, and the refusals are counted.
    #[test]
    fn the_estimate_is_not_lifted_under_a_standing_queue() {
        let t0 = Instant::now();
        let mut free = GoogCcDriver::new(4_000_000, t0);
        let free_stats = drive_clean_then_silence(&mut free, t0, 12, 1.0, 0, u64::MAX);
        assert!(free_stats.ceiling_reseeds_up > 5, "premise: lifts happen");
        assert_eq!(free_stats.ceiling_lifts_blocked_queue, 0);
        let mut queued = GoogCcDriver::new(4_000_000, t0);
        let q = drive_clean_then_silence(&mut queued, t0, 12, 1.0, 0, 6_000);
        assert!(
            q.ceiling_lifts_blocked_queue > 0,
            "no lift was refused: {q:?}"
        );
        assert!(
            q.ceiling_reseeds_up < free_stats.ceiling_reseeds_up,
            "lifts went on under a standing queue: {} against {}",
            q.ceiling_reseeds_up,
            free_stats.ceiling_reseeds_up
        );
    }

    /// A stall inside the window, no queue standing: its rate is a measurement
    /// of the gap, not of the path. The bound last taken from a full window
    /// stays, and goog_cc is not told to continue from the gap's rate.
    #[test]
    fn a_gap_in_the_acknowledgements_keeps_the_last_bound_and_is_not_pushed_into_goog_cc() {
        let t0 = Instant::now();
        let mut steady = GoogCcDriver::new(4_000_000, t0);
        drive_clean(&mut steady, t0, 8, 1.0);
        let steady_bound = steady.snapshot().stats.ceiling_bps;
        assert!(steady_bound > 0, "premise: a bound applies");

        let mut d = GoogCcDriver::new(4_000_000, t0);
        // 8 s of steady delivery, then 200 ms with one late batch (srtt 30 ms).
        let before = drive_clean_then_silence(&mut d, t0, 8, 1.0, 200, u64::MAX);
        assert!(before.ceiling_reseeds_down > 0, "premise: reseeds happen");
        let after = d.snapshot();
        assert!(
            after.stats.ceiling_reads_held > before.ceiling_reads_held,
            "premise: the gap was met ({:?})",
            after.stats
        );
        assert_eq!(
            after.stats.ceiling_reseeds_down, before.ceiling_reseeds_down,
            "goog_cc was handed a bound read across a stall"
        );
        // The window across the stall holds one batch in eight: read at face
        // value it is a bound of about an eighth of the path's.
        assert!(
            after.stats.ceiling_bps as f64 >= 0.5 * steady_bound as f64,
            "the bound fell to {} across a stall, from {steady_bound}",
            after.stats.ceiling_bps
        );
        assert!(
            after.bitrate_bps as f64 <= after.stats.ceiling_bps as f64,
            "the bound is still published"
        );
    }

    /// The same stall with the round trip doubled (a queue stands, as after a
    /// resume at a stale estimate): the low rate of the window is then the
    /// path's answer and is published, though goog_cc is still not handed it.
    #[test]
    fn a_gap_with_a_queue_standing_is_published_but_not_pushed_into_goog_cc() {
        let t0 = Instant::now();
        let mut steady = GoogCcDriver::new(4_000_000, t0);
        drive_clean(&mut steady, t0, 8, 1.0);
        let steady_bound = steady.snapshot().stats.ceiling_bps;

        let mut d = GoogCcDriver::new(4_000_000, t0);
        let before = drive_clean_then_silence(&mut d, t0, 8, 1.0, 200, 7_000);
        let after = d.snapshot();
        assert_eq!(
            after.stats.ceiling_reseeds_down, before.ceiling_reseeds_down,
            "goog_cc was handed a bound read across a stall"
        );
        assert_eq!(
            after.stats.ceiling_reads_held, 0,
            "a gap with a queue is not set aside"
        );
        assert!(
            (after.stats.ceiling_bps as f64) < 0.5 * steady_bound as f64,
            "the bound {} did not follow the window across the stall (steady {steady_bound})",
            after.stats.ceiling_bps
        );
    }

    /// The same path with a sender that spent a fifth of its grant (nothing to
    /// send): the acknowledged rate is its own, and bounding by it would
    /// pin the estimate there. The ceiling must not apply, and the estimate
    /// must be free to sit where goog_cc puts it, above `HEADROOM x acked`.
    #[test]
    fn an_app_limited_sender_is_not_bounded_by_what_it_chose_to_send() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        drive_clean(&mut d, t0, 12, 0.2);
        let snap = d.snapshot();
        assert!(
            snap.stats.ceiling_reads > 0,
            "premise: no reading was taken"
        );
        assert_eq!(
            snap.stats.ceiling_saturated_reads, 0,
            "a ceiling was applied to a sender that was not rate-limited"
        );
        assert_eq!(snap.stats.ceiling_bps, 0, "a bound was published");
        assert_eq!(snap.stats.ceiling_binding_reads, 0);
        // goog_cc is free to sit above `HEADROOM x acked`: the ceiling does
        // not bind it.
        assert!(
            d.estimate_bps as f64 > crate::transport::bwe::ceiling::HEADROOM * ACKED_BPS * 1.05,
            "goog_cc's target {} is held down at its own rate {}",
            d.estimate_bps,
            ACKED_BPS
        );
        // What is published is the growth gate's (D1) to say: with no probe,
        // at most 3 % a second from the start over the 12 s driven (and
        // under goog_cc's own target, which is the point of the limit).
        // (4.1: until the first ceiling reading exists the gate is off.)
        let bound = 4_100_000.0 * (0.03f64 * 12.0).exp();
        assert!(
            (snap.bitrate_bps as f64) <= bound && snap.bitrate_bps < d.estimate_bps,
            "D1: rate-limited, got {} (bound {bound:.0}, raw {})",
            snap.bitrate_bps,
            d.estimate_bps
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
                            wire_seq: 0,
                            server_emit_us: emit_us,
                            client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                            size_bytes: 1200,
                            probe: None,
                        }
                    })
                    .collect();
                d.update(
                    &batch,
                    &[],
                    t0 + Duration::from_millis(20 * step as u64),
                    batch.iter().map(|r| r.size_bytes as usize).sum(),
                );
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

    /// `pacer_rate_bps` is informational (admission reads `bitrate_bps`).
    /// Before any feedback it must be `None` (nothing absorbed yet), and it
    /// must become `Some` once the
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
                    wire_seq: 0,
                    server_emit_us: emit_us,
                    client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                    size_bytes: 1200,
                    probe: None,
                }
            })
            .collect();
        d.update(
            &batch,
            &[],
            t0 + Duration::from_millis(20),
            batch.iter().map(|r| r.size_bytes as usize).sum(),
        );

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
            wire_seq: 0,
            server_emit_us: 0,
            client_arrival_ms_lo16: 15,
            size_bytes: 1200,
            probe: Some(tag),
        }];
        // Must not panic.
        let snap = d.update(
            &records,
            &[],
            t0 + Duration::from_millis(20),
            records.iter().map(|r| r.size_bytes as usize).sum(),
        );
        assert_eq!(snap.samples_seen, 1);
    }

    /// D1: an unsaturated sender's published estimate grows no faster than
    /// 3 % a second (compounded) above the last proved value, however fast
    /// goog_cc's own target drifts up.
    ///
    /// 12 x 1200 B per 20 ms acknowledged on a clean path (5.76 Mbit/s) from
    /// a 4 Mbit/s start, the grant 9 Mbit/s and half of it spent: not
    /// saturated. The first second's packets carry a probe tag; the rest none.
    #[test]
    fn an_unsaturated_estimate_grows_at_most_three_percent_a_second_above_what_a_probe_proved() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        d.note_path_rtt(Duration::from_millis(30));
        let tag = ProbeTag {
            id: 1,
            // A cluster of the whole first second (600 packets, 720 kB): it
            // reads the path's 5.76 Mbit/s. A short one would read a burst
            // (12 packets 1 ms apart is 9.6 Mbit/s) and prove more than the
            // stream carries.
            min_probes: 600,
            min_bytes: 720_000,
            bytes_sent_before: 0,
        };
        const PROBED_STEPS: u32 = 50;
        // Judge from well after the last probe-tagged feedback.
        const SETTLED_STEP: u32 = PROBED_STEPS + 25;
        let (mut granted, mut spent, mut offered) = (0.0f64, 0.0f64, 0.0f64);
        // Up to the settled step the published values are the probes' (and the
        // start-up's): the proved rates. `at_proved` is the last step the
        // published value was at or under the highest of them: a climb above
        // it starts from there.
        let mut proved_max = 0u64;
        let mut at_proved = SETTLED_STEP;
        let mut raw_outran = 0u32;
        let mut limited_rise = 0u32;
        let mut prev = 0u64;
        for step in 0..(30 * 50) as u32 {
            let now = t0 + Duration::from_millis(20 * step as u64);
            granted += 9.0e6 / 8.0 * 0.020;
            spent += 9.0e6 / 8.0 * 0.020 * 0.5;
            offered += 12.0 * 1200.0;
            let probe = (step < PROBED_STEPS).then_some(tag);
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    AckArrival {
                        wire_seq: 0,
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                        size_bytes: 1200,
                        probe,
                    }
                })
                .collect();
            d.note_offered(now, offered);
            d.note_admission(now, granted, spent);
            let snap = d.update(&batch, &[], now, 14_400);
            if step <= SETTLED_STEP {
                // What the probes measured is proved too, published or not.
                proved_max = proved_max
                    .max(snap.bitrate_bps)
                    .max(d.snapshot().stats.growth_probe_proved_max_bps);
            } else {
                let t_s = (step - at_proved) as f64 * 0.020;
                // Free up to what was proved, 3 %/s compounded from there.
                let bound = proved_max as f64 * (0.03 * t_s).exp();
                assert!(
                    snap.bitrate_bps as f64 <= bound,
                    "step {step}: published {} over bound {bound:.0}",
                    snap.bitrate_bps
                );
                raw_outran += (d.estimate_bps as f64 > bound) as u32;
                if snap.bitrate_bps <= proved_max {
                    at_proved = step;
                }
                limited_rise += (snap.bitrate_bps > prev && snap.bitrate_bps > proved_max) as u32;
            }
            prev = snap.bitrate_bps;
        }
        let st = d.snapshot().stats;
        assert_eq!(
            st.ceiling_saturated_reads, 0,
            "premise: the sender was saturated ({st:?})"
        );
        // The limit was exercised: goog_cc's target went over what was
        // allowed, and the published value kept climbing at the limit.
        assert!(
            raw_outran > 0 && st.growth_held_feedbacks > 100 && st.growth_held_max_ratio_pct > 100,
            "premise: goog_cc never outran the limit ({raw_outran}, {st:?})"
        );
        assert!(
            limited_rise > 10,
            "premise: the published value never climbed above what was proved ({limited_rise})"
        );
    }

    /// A probe cluster acknowledged at a high rate is proved to the gate by
    /// the driver's own meter, whatever goog_cc then publishes.
    #[test]
    fn an_acknowledged_probe_cluster_is_proved_by_the_driver() {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(1_000_000, t0);
        let tag = ProbeTag {
            id: 9,
            min_probes: 10,
            min_bytes: 10_000,
            bytes_sent_before: 0,
        };
        // 12 x 1200 B, sent and received 1 ms apart (ms resolution on the
        // wire): 11 x 1200 B over 11 ms = 9_600_000 bit/s.
        let batch: Vec<AckArrival> = (0..12u32)
            .map(|i| AckArrival {
                wire_seq: 0,
                server_emit_us: 1_000 * i as u64,
                client_arrival_ms_lo16: (15 + i) as u16,
                size_bytes: 1200,
                probe: Some(tag),
            })
            .collect();
        assert_eq!(d.snapshot().stats.growth_probe_proofs, 0, "premise");
        d.update(&batch, &[], t0 + Duration::from_millis(40), 14_400);
        let st = d.snapshot().stats;
        assert_eq!(st.growth_probe_proofs, 1, "{st:?}");
        assert_eq!(st.growth_probe_proved_max_bps, 9_600_000);
        assert_eq!(d.gate.proved_max_bps(40_000), Some(9_600_000));
    }

    /// Drive a long, application-limited stream (one 1200 B packet per 20 ms
    /// of virtual time, far under the estimate) and log every probe request
    /// with the virtual time at which it surfaced. Optionally stall the real
    /// clock between steps.
    fn alr_probe_log(stall: bool) -> Vec<(u64, i32, u64)> {
        let t0 = Instant::now();
        let mut d = GoogCcDriver::new(4_000_000, t0);
        let mut log = Vec::new();
        for step in 0..1_500u64 {
            if stall && step % 100 == 0 {
                std::thread::sleep(Duration::from_millis(400));
            }
            let emit_us = step * 20_000;
            let rec = AckArrival {
                wire_seq: 0,
                server_emit_us: emit_us,
                client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                size_bytes: 1200,
                probe: None,
            };
            d.update(&[rec], &[], t0 + Duration::from_millis(20 * step), 1200);
            while let Some(p) = d.take_probe_request() {
                log.push((20 * step, p.id, p.target_rate_bps));
            }
        }
        log
    }

    /// goog_cc stamped the start of an application-limited region with the
    /// wall clock and compared it with the caller's (virtual) time, so a
    /// real-time stall moved the periodic ALR probes. The probe schedule must
    /// be a function of the fed timestamps alone.
    #[test]
    fn alr_probing_is_independent_of_the_wall_clock() {
        let quiet = alr_probe_log(false);
        let stalled = alr_probe_log(true);
        // Non-vacuous: the startup ladder alone is a few requests; periodic
        // ALR probing (every few virtual seconds) adds requests well after.
        assert!(
            quiet.iter().any(|&(t_ms, _, _)| t_ms > 6_000),
            "no ALR probe surfaced after startup, so the scenario never reached \
             periodic ALR probing: {quiet:?}"
        );
        assert_eq!(quiet, stalled);
    }
}
