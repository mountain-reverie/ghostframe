//! Bandwidth-estimator wrapper backed by a real GoogCC controller.
//!
//! # Design note — why not `str0m::bwe::Bwe`?
//!
//! str0m does expose a standalone `bwe_::Bwe` struct (a full GoogCC port)
//! internally, but it is declared `pub(crate)` and is NOT accessible from
//! outside the crate (checked against 0.21 and 0.23.1). The only public
//! `str0m::bwe` items are:
//!   - `Bitrate` — the bitrate newtype (re-exported from `str0m-proto`)
//!   - `Bwe<'a>` — a borrowed wrapper around an `Rtc` session
//!   - `BweKind`  — an event enum
//!
//! Constructing a standalone GCC estimator without an `Rtc` session is not
//! possible with str0m's public API. The standalone `goog_cc` crate backs
//! this wrapper instead — see `googcc::GoogCcDriver`.
//!
//! See `docs/superpowers/specs/2026-06-27-protocol-redesign-design.md`.

use std::time::{Duration, Instant};

mod ceiling;
mod googcc;
#[cfg(test)]
mod react_replay;
pub(crate) mod sliding_min;
mod timeline;

// ── Public types ─────────────────────────────────────────────────────────────

/// Tier-tagged ACK arrival record fed into the estimator. Built from the
/// `BweSample`s the io_bridge accumulates at ACK-receive time.
#[derive(Debug, Clone, Copy)]
pub struct AckArrival {
    /// Server-side send time in microseconds on the emitter's clock, from the
    /// transmission ledger, so a retransmitted pass reports when it actually
    /// went out rather than when it first did. Full width: the datagram
    /// header's `u32` stamp wraps every 71.6 minutes, this does not.
    pub server_emit_us: u64,
    /// Low 16 bits of the client's arrival time in ms.
    pub client_arrival_ms_lo16: u16,
    /// Wire size of the acknowledged datagram in bytes, summed over its
    /// fragments. GoogCC derives delivery rate from bytes; without this the
    /// controller sees arrivals but no volume and cannot form an estimate.
    /// This is datagram payload only and excludes UDP/IP framing (~48 bytes
    /// per packet on IPv6), so the estimate runs a few percent low on
    /// full-size datagrams — no correction factor is applied here.
    pub size_bytes: u32,
    /// Probe cluster this pass was tagged with at emit time (BWE Stage
    /// 2.4), carried through unchanged from `CacheEntry::probe` via
    /// `BweSample::probe`. `None` for ordinary traffic. `GoogCcDriver`
    /// converts this into the `SentPacket.pacing_info` goog_cc's
    /// `ProbeBitrateEstimator` requires — see `googcc::pacing_info_for`.
    pub probe: Option<crate::transport::reliable_emitter::cache::ProbeTag>,
}

/// Public observability snapshot returned by `BweWrapper::snapshot()`.
#[derive(Debug, Clone, Copy)]
pub struct BweSnapshot {
    /// Current bandwidth estimate, bits per second.
    pub bitrate_bps: u64,
    /// Total ACK-arrival samples fed into the estimator since startup.
    pub samples_seen: u64,
    /// Count of derived-RTT samples that landed far outside the plausible
    /// range for quinn's measured path RTT (see `GoogCcDriver::note_path_rtt`
    /// / `observe_derived_rtt`). Non-zero means the emit and arrival
    /// timestamps being differenced likely do not share an epoch — a real
    /// bug, not a slow link. Zero whenever no path RTT has been supplied yet.
    pub implausible_rtt_samples: u64,
    /// BWE Stage 2.2's pacing rate, bits per second — derived from the
    /// controller's most recent `NetworkControlUpdate::pacer_config`
    /// (`PacerConfig::data_rate()`). `None` until the controller has
    /// processed at least one feedback batch. Informational: admission
    /// (the model controller) reads `bitrate_bps`, and this is logged and
    /// reported by the harness. See `GoogCcDriver::absorb`'s doc comment for
    /// why the two fields diverge and `pad_window` (padding to hold a floor
    /// rate) is deliberately not surfaced here at all.
    pub pacer_rate_bps: Option<u64>,
    /// Countable bytes per token the admission bucket spends: how much of
    /// what is billed (parity, framing, control datagrams) goog_cc is told
    /// of. The estimate and `pacer_rate_bps` are rates of countable bytes, so
    /// the token rate that carries them is `rate / payload_fraction`. In
    /// `(0.5, 1.0]`; `1.0` until there is spend to measure it on.
    pub payload_fraction: f64,
    /// Counters for scenes and tests; none of them steers anything.
    pub stats: BweStats,
}

/// Whether goog_cc's loss-based estimator is holding the estimate down (or
/// ramping it back up), as opposed to the estimate following the delay-based
/// one. Mirrors goog_cc's `LossBasedState` so that crate's types stay inside
/// this module.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum LossState {
    /// No loss limit: the estimate is the delay-based one.
    #[default]
    DelayBased,
    Increasing,
    IncreasingWithPadding,
    Decreasing,
}

impl LossState {
    /// True in every state but `DelayBased`.
    pub fn is_limited(self) -> bool {
        self != LossState::DelayBased
    }

    /// Index into `BweStats::loss_state_feedbacks`.
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// What the estimator wrapper has done, in numbers a scene can hold it to.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BweStats {
    /// Probe clusters goog_cc has asked for since startup.
    pub probe_requests: u64,
    /// Target rate of the first four clusters it asked for, in request order
    /// (zero for a cluster not yet asked for). The first two are the startup
    /// ladder, `3 x` and `6 x` the start rate when nothing capped it.
    pub first_probe_targets_bps: [u64; 4],
    /// Readings of the estimate ceiling that had a full window of history.
    pub ceiling_reads: u64,
    /// Of those, readings taken while the sender was saturated, so a ceiling
    /// applied. Zero in an app-limited run proves the gate held.
    pub ceiling_saturated_reads: u64,
    /// Readings at which the ceiling was below the estimate and lowered what
    /// was published. Zero in a run that was meant to be bounded proves
    /// nothing was measured.
    pub ceiling_binding_reads: u64,
    /// Times goog_cc was handed the ceiling as its estimate because its own
    /// was above it.
    pub ceiling_reseeds_down: u64,
    /// Times it was handed the ceiling because its own was below it while the
    /// path showed no limit.
    pub ceiling_reseeds_up: u64,
    /// Lifts checked one feedback after they were made, and how many had
    /// fallen under 0.98 of the lifted value by then (goog_cc's loss-based
    /// estimator undoes a lift while it is in a loss-limited state).
    pub ceiling_lifts_checked: u64,
    pub ceiling_lifts_undone: u64,
    /// The same judged one second after the lift instead of one feedback
    /// after: a lift that is not under 0.98 of its value a second later held.
    /// (A later down-reseed or delay back-off also counts as undone here.)
    pub ceiling_lifts_checked_1s: u64,
    pub ceiling_lifts_undone_1s: u64,
    /// The loss-based state after the last feedback.
    pub loss_state: LossState,
    /// Feedbacks absorbed in each loss-based state, indexed by
    /// `LossState::index`. Their sum is the feedback count: a zero in the
    /// limited slots on a lossy run says the state was never reached.
    pub loss_state_feedbacks: [u64; 4],
    /// Lifts not made because the loss-based estimator was limiting.
    pub ceiling_lifts_blocked_loss: u64,
    /// Reseeds down made while the loss-based estimator was limiting.
    pub ceiling_reseeds_down_loss_limited: u64,
    /// Lifts not made because quinn's round trip said a queue was standing.
    pub ceiling_lifts_blocked_queue: u64,
    /// Reseeds down not made because the window held a gap in the
    /// acknowledgements longer than a round trip.
    pub ceiling_reseeds_skipped_gap: u64,
    /// The payload fraction last measured, in parts per million.
    pub payload_fraction_ppm: u64,
    /// The last acknowledged rate read, bits per second.
    pub ceiling_acked_bps: u64,
    /// The last ceiling applied (zero if none was), bits per second.
    pub ceiling_bps: u64,
}

/// A probe goog_cc has asked for (BWE Stage 2.4), surfaced from
/// `NetworkControlUpdate::probe_cluster_configs`. Rates and durations are
/// converted out of goog_cc's units at this boundary so nothing downstream
/// of `BweWrapper` needs the crate's types — see this module's doc comment
/// on why `goog_cc` types don't leak past here.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ProbeRequest {
    /// goog_cc's `ProbeClusterConfig::id`. Threaded through unchanged so
    /// the eventual `PacedPacketInfo::probe_cluster_id` matches what
    /// goog_cc itself assigned the cluster.
    pub id: i32,
    pub target_rate_bps: u64,
    pub duration: Duration,
    /// From the config's `target_probe_count`. The estimator discards a
    /// cluster with fewer acknowledged packets than this
    /// (`probe_bitrate_estimator.rs:137`).
    pub min_probes: i64,
    /// `target_rate x duration`, converted from bits to bytes. goog_cc
    /// offers no helper for this derivation — `PacedPacketInfo::new` just
    /// takes it as a plain `i64` argument — so it is derived here and
    /// pinned by a bench assertion (BWE Stage 2.4 Task 4) against a known
    /// config.
    pub min_bytes: i64,
}

// ── BweWrapper ────────────────────────────────────────────────────────────────

/// Bandwidth estimator wrapper. Delegates to a real `GoogCcDriver`
/// (`googcc::GoogCcDriver`), a standalone port of libwebrtc's GoogCC
/// congestion controller. Kept as a thin seam so the two `io_bridge`
/// call sites (`AckArrival` in, `BweSnapshot` out) never need to change
/// regardless of what backs the estimate.
pub struct BweWrapper {
    driver: googcc::GoogCcDriver,
}

impl BweWrapper {
    /// Initial bitrate seed. Sized for a typical broadband first-paint
    /// burst (2 Mbps); the estimator will adapt within a few hundred ms.
    pub const INITIAL_BPS: u64 = 2_000_000;

    /// `now` seeds the controller's clock base. Callers should pass the
    /// same clock source they'll later pass to `update` (`now_std()` in
    /// production) — seeding from a bare `Instant::now()` instead would mix
    /// one wall-clock stamp into an estimator whose every other input is
    /// the caller's (possibly virtual, paused-tokio) clock. Benign in
    /// practice since the wall clock only ever runs ahead of a freshly-
    /// paused virtual clock (the first window just over-reports elapsed
    /// time rather than under-reporting it), but there's no reason to
    /// leave a real wall-clock read inside the one estimator this
    /// paused-clock harness exists to unblock.
    pub fn new(initial_bps: u64, now: Instant) -> Self {
        Self {
            driver: googcc::GoogCcDriver::new(initial_bps, now),
        }
    }

    /// `new`, for the model controller: the estimator is not fed a transport
    /// capacity hint, so it does not probe on one either.
    pub fn new_without_hint(initial_bps: u64, now: Instant) -> Self {
        Self {
            driver: googcc::GoogCcDriver::with_network_state_probing(initial_bps, now, false),
        }
    }

    /// Continue the estimate from `bps` after the base round trip rose.
    pub fn restore_from(&mut self, now: Instant, bps: u64) {
        self.driver.restore_from(now, bps);
    }

    /// Continue the estimate from `bps` (an idle restart's cold start).
    pub fn restart_from(&mut self, now: Instant, bps: u64) {
        self.driver.restart_from(now, bps);
    }

    /// Feed a batch of ACK arrival records and update the estimate.
    ///
    /// `now` is the caller's wall-clock at the time these records were
    /// collected (typically `Instant::now()` at ACK-parse time).
    ///
    /// Returns the updated `BweSnapshot`.
    /// `losses` are transmissions RTO fired on, as `(emit_us, wire_bytes)`,
    /// from `ReliableTileEmitter::take_rto_losses`. They are the only loss
    /// signal the estimator receives; without them goog_cc's loss-based half
    /// never engages.
    pub fn update(
        &mut self,
        records: &[AckArrival],
        losses: &[(u64, usize)],
        now: Instant,
        data_in_flight_bytes: usize,
    ) -> BweSnapshot {
        self.driver
            .update(records, losses, now, data_in_flight_bytes)
    }

    /// Return the current observability snapshot without advancing the estimator.
    pub fn snapshot(&self) -> BweSnapshot {
        self.driver.snapshot()
    }

    /// Record quinn's measured path RTT as an independent plausibility bound
    /// on the estimator's derived RTT. See `GoogCcDriver::note_path_rtt`.
    pub fn note_path_rtt(&mut self, rtt: Duration) {
        self.driver.note_path_rtt(rtt);
    }

    /// Feed the transport's own BDP-derived rate as an independent ceiling
    /// for probe decisions. See `GoogCcDriver::set_transport_capacity_hint`
    /// for what this number is and is not.
    pub fn set_transport_capacity_hint(&mut self, bytes_per_us: f32, now: Instant) {
        self.driver.set_transport_capacity_hint(bytes_per_us, now);
    }

    /// Withdraw the capacity hint: the path is no longer being filled, so
    /// `cwnd / srtt` no longer describes it and must not cap a probe.
    pub fn withdraw_transport_capacity_hint(&mut self, now: Instant) {
        self.driver.withdraw_transport_capacity_hint(now);
    }

    /// Wire bytes of the transmissions recorded so far (cumulative): what
    /// goog_cc is told of what was sent, to set against the tokens spent.
    pub fn note_offered(&mut self, now: Instant, cumulative_bytes: f64) {
        self.driver.note_offered(now, cumulative_bytes);
    }

    /// The admission bucket's cumulative grant and spend, read at the end of
    /// an admission pass. The estimate ceiling uses them to tell a sender that
    /// was rate-limited from one with nothing to send.
    pub fn note_admission(&mut self, now: Instant, granted_bytes: f64, spent_bytes: f64) {
        self.driver.note_admission(now, granted_bytes, spent_bytes);
    }

    /// Return and clear the most recently requested probe cluster, if any
    /// (BWE Stage 2.4). Consumes it: a config is surfaced exactly once
    /// rather than re-triggering on every subsequent poll that happens to
    /// see it still stored. Only one request is ever held — a later config
    /// (from the same or a subsequent `update()`) replaces whatever was
    /// pending, since `IoBridge` runs at most one active probe window at a
    /// time.
    pub fn take_probe_request(&mut self) -> Option<ProbeRequest> {
        self.driver.take_probe_request()
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_starts_near_initial_bps() {
        let bwe = BweWrapper::new(BweWrapper::INITIAL_BPS, Instant::now());
        let snap = bwe.snapshot();
        // The initial seed should be reflected as the estimate before any
        // update. Loose tolerance in case the controller clamps the seed.
        assert!(
            snap.bitrate_bps >= BweWrapper::INITIAL_BPS / 2
                && snap.bitrate_bps <= BweWrapper::INITIAL_BPS * 2,
            "initial estimate {} not within 2× of seed {}",
            snap.bitrate_bps,
            BweWrapper::INITIAL_BPS,
        );
        assert_eq!(snap.samples_seen, 0);
    }

    /// `BweWrapper::snapshot()` must forward `pacer_rate_bps` unchanged from
    /// the driver — the wrapper is documented as a thin seam
    /// (`AckArrival` in, `BweSnapshot` out) and this field is exactly the
    /// kind of thing a careless refactor could drop while still compiling
    /// (both are `Option<u64>`-shaped, easy to leave zeroed).
    #[test]
    fn snapshot_forwards_pacer_rate_after_feedback() {
        let t0 = Instant::now();
        let mut bwe = BweWrapper::new(BweWrapper::INITIAL_BPS, t0);
        assert_eq!(bwe.snapshot().pacer_rate_bps, None);

        let records: Vec<AckArrival> = (0..12u32)
            .map(|i| {
                let emit_us = (i as u64) * 1_000;
                AckArrival {
                    server_emit_us: emit_us,
                    client_arrival_ms_lo16: ((emit_us / 1_000 + 15) & 0xFFFF) as u16,
                    size_bytes: 1200,
                    probe: None,
                }
            })
            .collect();
        let snap = bwe.update(
            &records,
            &[],
            t0 + Duration::from_millis(20),
            records.iter().map(|r| r.size_bytes as usize).sum(),
        );
        assert!(
            snap.pacer_rate_bps.is_some_and(|bps| bps > 0),
            "wrapper did not forward a positive pacer rate after feedback"
        );
    }

    #[test]
    fn update_counts_records_seen() {
        let mut bwe = BweWrapper::new(BweWrapper::INITIAL_BPS, Instant::now());
        let records = vec![
            AckArrival {
                server_emit_us: 100,
                client_arrival_ms_lo16: 110,
                size_bytes: 1200,
                probe: None,
            },
            AckArrival {
                server_emit_us: 120,
                client_arrival_ms_lo16: 135,
                size_bytes: 1200,
                probe: None,
            },
        ];
        let snap = bwe.update(
            &records,
            &[],
            Instant::now(),
            records.iter().map(|r| r.size_bytes as usize).sum(),
        );
        assert_eq!(snap.samples_seen, 2);
    }

    /// Guards the import paths. goog_cc's published docs reference
    /// `goog_cc::api::*`, which is a private module; the real re-exports are
    /// at the crate root. This test fails to compile if that changes.
    #[test]
    fn googcc_constructs_standalone() {
        use goog_cc::network_control::NetworkControllerConfig;
        use goog_cc::transport::TargetRateConstraints;
        use goog_cc::units::{DataRate, Timestamp};
        use goog_cc::{GoogCcConfig, GoogCcNetworkController};

        let t0 = Timestamp::from_millis(1_000);
        let cfg = NetworkControllerConfig {
            constraints: TargetRateConstraints {
                at_time: t0,
                min_data_rate: Some(DataRate::from_kilobits_per_sec(100)),
                max_data_rate: Some(DataRate::from_kilobits_per_sec(50_000)),
                starting_rate: Some(DataRate::from_kilobits_per_sec(1_000)),
            },
            ..Default::default()
        };
        let _ctl = GoogCcNetworkController::new(
            cfg,
            GoogCcConfig {
                feedback_only: false,
            },
        );
    }

    /// GoogCC derives rate from bytes, so a sample without a size cannot
    /// contribute to an estimate. This pins the field onto the seam.
    #[test]
    fn ack_arrival_carries_packet_size() {
        let a = AckArrival {
            server_emit_us: 10,
            client_arrival_ms_lo16: 25,
            size_bytes: 1200,
            probe: None,
        };
        assert_eq!(a.size_bytes, 1200);
    }

    /// The emit time must be server-side monotonic microseconds, not a
    /// 16-bit wrapped wire value. A retransmitted datagram keeps its original
    /// stamp in the cache, so reading bytes would report the first send and
    /// bury the RTO backoff inside the measured one-way delay.
    #[test]
    fn ack_arrival_emit_time_is_monotonic_micros() {
        let a = AckArrival {
            server_emit_us: 5_000_000,
            client_arrival_ms_lo16: 25,
            size_bytes: 1200,
            probe: None,
        };
        assert_eq!(a.server_emit_us, 5_000_000);
    }

    /// The seam must be backed by GoogCC, not the EWMA. Rising one-way delay
    /// at constant volume is the signal only a delay-gradient controller
    /// reacts to: a delivery-rate estimator sees steady bytes and holds.
    #[test]
    fn wrapper_backs_off_on_rising_delay() {
        let t0 = std::time::Instant::now();
        let mut w = BweWrapper::new(4_000_000, t0);

        let mut last = w.snapshot().bitrate_bps;
        for step in 0..400u32 {
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    // One-way delay grows 10 ms per step. Volume is constant.
                    let arrival_ms = emit_us / 1_000 + 15 + (step as u64) * 10;
                    AckArrival {
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                        size_bytes: 1200,
                        probe: None,
                    }
                })
                .collect();
            last = w
                .update(
                    &batch,
                    &[],
                    t0 + std::time::Duration::from_millis(20 * step as u64),
                    batch.iter().map(|r| r.size_bytes as usize).sum(),
                )
                .bitrate_bps;
        }

        assert!(
            last < 4_000_000,
            "estimate {last} never fell despite steadily rising one-way delay"
        );
    }

    /// Control for `wrapper_backs_off_on_rising_delay`. Same shape, flat
    /// delay: the estimate must RISE above the seed. Without this, a driver
    /// that simply decays toward zero would satisfy the back-off test.
    #[test]
    fn wrapper_rises_on_a_clean_link() {
        let t0 = std::time::Instant::now();
        let mut w = BweWrapper::new(4_000_000, t0);

        let mut last = w.snapshot().bitrate_bps;
        for step in 0..400u32 {
            let batch: Vec<AckArrival> = (0..12u32)
                .map(|i| {
                    let emit_us = ((step * 20 + i) as u64) * 1_000;
                    let arrival_ms = emit_us / 1_000 + 15;
                    AckArrival {
                        server_emit_us: emit_us,
                        client_arrival_ms_lo16: (arrival_ms & 0xFFFF) as u16,
                        size_bytes: 1200,
                        probe: None,
                    }
                })
                .collect();
            last = w
                .update(
                    &batch,
                    &[],
                    t0 + std::time::Duration::from_millis(20 * step as u64),
                    batch.iter().map(|r| r.size_bytes as usize).sum(),
                )
                .bitrate_bps;
        }

        assert!(
            last > 4_000_000,
            "estimate {last} did not rise on a clean link with constant delay"
        );
    }
}
