//! Glue between the transport's observations and the bandwidth estimator /
//! adaptation context: quinn path stats, the transmission ledger, receiver
//! feedback and the probe window.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - the loss signal: `transmission_ledger` expiry (`expire_transmissions`,
//!   `loss_horizon`, `live_path_rtt`, `select_live_rtt`), `pending_losses`,
//!   `loss_window` and `adaptation_context.loss_rate` (`refresh_loss_rate`);
//! - path-stats sampling (`sample_all_path_stats`,
//!   `apply_path_stats_snapshot`), which writes `adaptation_context`
//!   (`bytes_per_us`, `smoothed_rtt_us`) and pushes the cwnd/rtt capacity hint
//!   to the estimator (`CapacityHintWindow`, `capacity_hint_window`,
//!   `path_last_sent_at`);
//! - receiver-feedback suspension debounce (`ingest_feedback_for_adaptation`,
//!   `recent_suspension_flags`);
//! - the probe window lifecycle (`open_probe_window_for_burst`,
//!   `poll_probe_window`, `close_probe_window`, `active_probe`,
//!   `ActiveProbe`);
//! - the record types the estimator is fed with or that summarise ACK latency
//!   (`BweSample`, `TierLatencyStats`, `PassTier`) and the constants that size
//!   them;
//! - the harness-only estimator accessors `bwe_snapshot` and
//!   `bwe_publish_handle`.
//!
//! Not here, deliberately: the `BweWrapper` itself (`crate::transport::bwe`);
//! the event loop that drains `bwe_samples_buffer` into the estimator and
//! publishes its snapshot (`IoBridge::run`); the ACK decoding that fills
//! `bwe_samples_buffer` (`ack_dispatch.rs`); and the budget the estimate is
//! turned into (`pacing.rs`).
//!
//! Boundary with `pacing.rs`: pacing reads `adaptation_context.bytes_per_us`
//! (written by `apply_path_stats_snapshot` here) and the estimator snapshot,
//! and while a probe window is open `emit_via_scheduler` replaces the budget
//! with `ActiveProbe::target_rate_bps`. `drain_scheduler_into_quinn` (in
//! `io_bridge.rs`) advances `ActiveProbe::bytes_sent`/`packets_sent`.
//! `apply_path_stats_snapshot` straddles the two: it feeds both the classifier
//! and pacing input and the estimator's capacity hint, and stays here whole.
//!
//! Must not: compute a per-tick send budget or touch the scheduler or the
//! quinn send buffers.

use super::*;

/// Path RTT of the live (HELLO'd) session only. An unauthenticated or
/// evicted connection must not steer the loss horizon or the BWE reference.
/// `None` when there is no live session or its connection is gone.
pub(super) fn select_live_rtt(
    live: Option<ConnectionHandle>,
    rtts: impl Iterator<Item = (ConnectionHandle, std::time::Duration)>,
) -> Option<std::time::Duration> {
    let live = live?;
    rtts.filter(|(h, _)| *h == live).map(|(_, r)| r).min()
}

/// The capacity hint's window is `HINT_WINDOW_SRTTS` smoothed round trips,
/// never under `HINT_WINDOW_FLOOR`. It was the last 16 samples, and a sample
/// is taken per loop iteration, so the window was a count of wakeups, not a
/// span of time: whether it was full when the first probe ladder was issued
/// depended on how many wakeups the handshake had cost, and the hybrid
/// post-quantum handshake lost that race in 19 of 20 startup cells.
///
/// Four round trips span several windows of feedback and the swing of a
/// Cubic cycle's growth step; the floor keeps it from shrinking below the
/// path's own timing resolution (the 5 ms ACK flush plus a batch).
const HINT_WINDOW_SRTTS: u32 = 4;
const HINT_WINDOW_FLOOR: std::time::Duration = std::time::Duration::from_millis(100);

/// What the hint should do after a reading.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum HintAction {
    /// Hand this `bytes_per_us` to the estimator as the capacity ceiling.
    Feed(f32),
    /// Nothing in the window describes a busy path: tell the estimator the
    /// capacity is bounded only by its configured maximum.
    Unbounded,
    None,
}

/// What the estimator was last told.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum HintState {
    /// Nothing yet (no feedback has been heard).
    #[default]
    Silent,
    Bounded,
    Unbounded,
}

/// The `cwnd / srtt` readings that may cap a probe, and the rules for when.
///
/// `cwnd / srtt` is a rate only while `cwnd` is the limit. When the sender is
/// held back by anything else (the admission bucket, the application) quinn's
/// window is whatever it last grew to and says nothing about the path, so:
///
/// 1. only readings taken while quinn was the limit (its datagram queue still
///    holding data at the end of the turn: cwnd or its pacer, which is
///    `1.25 x cwnd / srtt`) enter the window;
/// 2. the window is a span of time, `max(4 x srtt, 100 ms)`;
/// 3. nothing is handed to the estimator before it has heard its first
///    feedback. The hint caps goog_cc's startup ladder; before any feedback
///    the reading is of a window nothing has used (12 000 bytes over quinn's
///    333 ms initial RTT is 288 kbit/s) and it collapsed the 6 / 12 Mbit
///    ladder to the 2 Mbit start rate;
/// 4. when the window is empty the estimator is told the capacity is
///    unbounded (up to its configured maximum), so a stale reading does not
///    cap probes for as long as the path stays unfilled.
#[derive(Debug, Default)]
pub(super) struct CapacityHintWindow {
    samples: std::collections::VecDeque<(std::time::Instant, f32)>,
    state: HintState,
}

impl CapacityHintWindow {
    pub(super) fn window(srtt: std::time::Duration) -> std::time::Duration {
        (srtt * HINT_WINDOW_SRTTS).max(HINT_WINDOW_FLOOR)
    }

    /// Take one reading at `now`.
    pub(super) fn observe(
        &mut self,
        now: std::time::Instant,
        srtt: std::time::Duration,
        bytes_per_us: f32,
        quinn_limited: bool,
        feedback_heard: bool,
    ) -> HintAction {
        let window = Self::window(srtt);
        while self
            .samples
            .front()
            .is_some_and(|(t, _)| now.saturating_duration_since(*t) > window)
        {
            self.samples.pop_front();
        }
        if quinn_limited {
            self.samples.push_back((now, bytes_per_us));
        }
        if !feedback_heard {
            return HintAction::None;
        }
        match self.samples.iter().map(|(_, v)| *v).reduce(f32::min) {
            Some(min) => {
                self.state = HintState::Bounded;
                HintAction::Feed(min)
            }
            None => self.unbounded(),
        }
    }

    /// The path is idle: forget every reading.
    pub(super) fn clear(&mut self, feedback_heard: bool) -> HintAction {
        self.samples.clear();
        if feedback_heard {
            self.unbounded()
        } else {
            HintAction::None
        }
    }

    fn unbounded(&mut self) -> HintAction {
        if std::mem::replace(&mut self.state, HintState::Unbounded) == HintState::Unbounded {
            HintAction::None
        } else {
            HintAction::Unbounded
        }
    }
}

/// How long without a packet on the path before `cwnd / rtt` stops being
/// treated as a live measurement. Comfortably longer than a 33 ms frame tick
/// so ordinary inter-frame gaps do not read as idle, short enough that a real
/// pause stops feeding the hint before AIMD can climb far on it.
const PATH_IDLE_AFTER: std::time::Duration = std::time::Duration::from_millis(250);

/// Cap on losses held waiting for an acknowledgement to report them with.
/// Comfortably more than a congested tick produces, and small enough that a
/// session whose acknowledgements never resume cannot accumulate without
/// bound.
pub(super) const MAX_PENDING_LOSSES: usize = 4096;

/// A transmission declared lost whose report has not yet gone to goog_cc. The
/// `wire_seq` is what lets a late acknowledgement withdraw exactly this loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PendingLoss {
    pub(super) wire_seq: u32,
    pub(super) emit_us: u64,
    pub(super) wire_bytes: usize,
}

/// Outstanding transmissions the ledger will hold. Sized by send rate x
/// horizon rather than session length — roughly 2000 packets/s at 20 Mbps
/// against a horizon measured in hundreds of milliseconds. The cap is a
/// backstop against a peer that stops acknowledging, not the normal bound.
pub(super) const TRANSMISSION_LEDGER_CAPACITY: usize = 16_384;

/// Loss horizon bounds. An unacknowledged transmission is declared lost after
/// `clamp(LOSS_HORIZON_RTTS x smoothed_rtt, FLOOR, CEIL)`.
///
/// Adaptive because the spread is too wide for one number: measured RTT on
/// this system runs from ~20 ms on a clean link to 147 ms through the
/// bufferbloated wifi bottleneck.
///
/// The floor covers the client's 5 ms ACK batching plus a batch interval,
/// which is RTT-independent and does not shrink on a fast link.
///
/// The ceiling bounds a perverse coupling: under bufferbloat RTT inflates
/// *because of* the congestion being detected, which would stretch the
/// horizon exactly when loss should be reported fastest.
pub(super) const LOSS_HORIZON_RTTS: u32 = 3;

pub(super) const LOSS_HORIZON_FLOOR: std::time::Duration = std::time::Duration::from_millis(60);

pub(super) const LOSS_HORIZON_CEIL: std::time::Duration = std::time::Duration::from_millis(600);

/// Visual-importance tier of a CDF53 pass.
/// - Critical: passes 0-3 — LL3 sub-band + first 3 bit-planes. Carry
///   most of the perceived image quality. NACKed aggressively, given
///   priority in the pacer.
/// - Refinement: passes 4-13 — higher-frequency detail. NACKed
///   conservatively, lower priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum PassTier {
    Critical,
    Refinement,
}

pub(super) fn pass_tier(pass_idx: u8) -> PassTier {
    if pass_idx <= crate::transport::reliable_emitter::emission_queue::CRITICAL_PASS_MAX {
        PassTier::Critical
    } else {
        PassTier::Refinement
    }
}

/// Upper bound (ms, exclusive) of each of the first 7 `TierLatencyStats`
/// buckets; the 8th bucket is the open-ended "500ms+" tail. Fixed rather
/// than adaptive: this is a baseline-measurement tool (BWE Stage 2.0), not
/// a tuned production percentile estimator, and tail latency is exactly
/// what pacing (Stage 2) is meant to shrink — a mean alone can stay flat
/// while p95 moves, or hide a regression.
const LATENCY_BUCKET_BOUNDS_MS: [u64; 7] = [5, 10, 20, 50, 100, 200, 500];

/// Per-tier ACK round-trip latency accumulator (BWE Stage 2.0 baseline
/// measurement). Populated at the `bwe_samples_buffer` drain site in
/// `run()` from `received_at - server_emit_us` — both timestamps are on
/// the server's own monotonic clock, so this is an emit-to-ACK-receipt
/// round trip, not a one-way delay. See that call site's comment for why
/// `client_arrival_ms_lo16` (the client-clock field) is not used here.
///
/// Cumulative since bridge construction, mirroring `bytes_emitted_critical`
/// / `bytes_emitted_refinement` — never reset, so the periodic log's values
/// are always "since startup" and a browserless harness reader gets a
/// complete-scene total regardless of when it samples.
#[derive(Debug, Default, Clone, Copy)]
pub struct TierLatencyStats {
    pub count: u64,
    pub sum_us: u64,
    pub max_us: u64,
    /// Counts for buckets `0-5, 5-10, 10-20, 20-50, 50-100, 100-200,
    /// 200-500, 500+` ms, in that order. See `LATENCY_BUCKET_BOUNDS_MS`.
    pub buckets: [u64; 8],
}

impl TierLatencyStats {
    pub(super) fn record(&mut self, latency_us: u64) {
        self.count += 1;
        self.sum_us = self.sum_us.saturating_add(latency_us);
        self.max_us = self.max_us.max(latency_us);
        let latency_ms = latency_us / 1000;
        let bucket = LATENCY_BUCKET_BOUNDS_MS
            .iter()
            .position(|&bound| latency_ms < bound)
            .unwrap_or(LATENCY_BUCKET_BOUNDS_MS.len());
        self.buckets[bucket] += 1;
    }

    /// Mean latency in microseconds, or 0 on an empty accumulator (rather
    /// than dividing by zero) — matching the "zero is a valid, checkable
    /// reading" convention the rest of this module's stats use.
    pub fn mean_us(&self) -> u64 {
        self.sum_us.checked_div(self.count).unwrap_or(0)
    }
}

/// Maximum number of pending BweSample entries the per-tick drain
/// will hold between drains. Sized to comfortably exceed one tick's
/// worth of ACK throughput on a 1920×1080 first-paint burst
/// (~24K passes, ACK batches of ~64 entries → ~376 batches).
pub(super) const BWE_SAMPLES_BUFFER_CAPACITY: usize = 4096;

/// The probe cluster currently being filled (BWE Stage 2.4), if any. One at
/// a time: a new request replaces this rather than queueing, because two
/// clusters' packets interleaving on the wire would corrupt both
/// measurements, and `ProbeController` does not expect concurrent clusters
/// anyway.
pub(super) struct ActiveProbe {
    pub(super) id: i32,
    /// Target rate for the burst; overrides the pacing budget for ticks
    /// inside the window (see `poll_probe_window`'s call sites) — sending
    /// above the current estimate is the entire point of a probe.
    pub(super) target_rate_bps: u64,
    /// Wall-clock end of the window, from the poll time the request was
    /// observed plus the config's `target_duration`.
    pub(super) ends_at: std::time::Instant,
    /// From the config's `target_probe_count` — see `ProbeRequest`.
    pub(super) min_probes: i64,
    /// From `target_data_rate x target_duration` — see `ProbeRequest`.
    pub(super) min_bytes: i64,
    /// Cumulative tagged bytes so far — goog_cc's `probe_cluster_bytes_sent`
    /// semantics. Advanced once per tagged datagram in
    /// `drain_scheduler_into_quinn`.
    pub(super) bytes_sent: i64,
    /// Cumulative tagged packets so far, for the min-probes check at close.
    pub(super) packets_sent: i64,
}

/// One-way-delay sample observed at ACK time. Used as input to the
/// per-tier latency histograms and the goog_cc estimator. All timestamps
/// are u16 wall-clock-ms wrapped; the consumer looks at *relative* deltas,
/// so clock skew is fine.
///
/// `tier` and `received_at` are consumed at the drain site (`run()`) to
/// accumulate `TierLatencyStats` (BWE Stage 2.0). `owd_ms_lo16` is still
/// staged for the Phase 2 controller's delay-gradient input and not read
/// anywhere yet; kept in the struct so the layout is stable across the
/// Phase 1 → 2 handoff, hence the remaining `allow(dead_code)`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub(super) struct BweSample {
    pub(super) tier: PassTier,
    /// Server-side send time in microseconds on the emitter's clock
    /// (`ReliableTileEmitter::time_base`), taken from the transmission ledger,
    /// so a retransmitted pass reports when it actually went out rather than
    /// when it first did. Full width: the datagram header's `u32` stamp wraps
    /// every 71.6 minutes, this does not.
    pub(super) server_emit_us: u64,
    /// When this transmission left quinn's FIFO for the wire, on the same
    /// clock as `server_emit_us` (the datagram's header carries the earlier
    /// moment it was built). This is the send time the estimator is given
    /// (F3): the delay signal measures the path, not the host's queue. The
    /// latency statistics above stay on `server_emit_us` on purpose, so they
    /// keep reporting what the user waited for. `server_emit_us` itself when
    /// the transmission was never stamped (a test, or an acknowledgement that
    /// beat the poll).
    pub(super) depart_us: u64,
    /// When the underlying `TileWork` became available to the scheduler
    /// (`CacheEntry::queued_at`, ultimately `TileWork::queued_at`), in
    /// microseconds since the bridge's BWE epoch — same clock and units as
    /// `server_emit_us`, computed the same way. Unlike `server_emit_us`,
    /// this does NOT change on retransmit: `queued_at` is copied through
    /// unchanged by `ReliableTileEmitter::submit_one` regardless of how
    /// many times a pass is resent, because the work became available
    /// exactly once. Feeds `queued_at -> ACK` (BWE Stage 2.1), the
    /// interval scheduler prioritisation actually shortens.
    pub(super) queued_since_epoch_us: u64,
    /// Low 16 bits of the client's receive time in ms, echoed back via
    /// the ACK envelope's `arrival_time_ms_lo16`.
    pub(super) client_arrival_ms_lo16: u16,
    /// Modular u16 subtraction of arrival - emit. *Delta-of-deltas* input
    /// to delay-gradient estimation; the absolute value is meaningless
    /// because of clock skew.
    pub(super) owd_ms_lo16: u16,
    /// Wire size of this datagram in bytes, summed over its fragments.
    pub(super) size_bytes: u32,
    /// Probe cluster this pass was tagged with at emit time (BWE Stage
    /// 2.4), read off `CacheEntry::probe` while the entry is still live —
    /// see the extraction site in `dispatch_ack_datagram`. Carried through
    /// unchanged to `AckArrival::probe` so `GoogCcDriver` can build the
    /// `PacedPacketInfo` goog_cc's probe estimator requires. `None` for
    /// the overwhelming majority of ordinary traffic.
    pub(super) probe: Option<crate::transport::reliable_emitter::cache::ProbeTag>,
    /// Wall-clock instant when the ACK arrived (for periodic-drain timing).
    pub(super) received_at: std::time::Instant,
}

impl BweSample {
    /// What the estimator is told of this acknowledgement. Its send time is
    /// when the transmission left quinn's FIFO (`depart_us`), not when it was
    /// built (`server_emit_us`): the delay signal and a probe cluster's send
    /// span should measure the path, not the host's queue.
    pub(super) fn ack_arrival(self) -> crate::transport::bwe::AckArrival {
        crate::transport::bwe::AckArrival {
            server_emit_us: self.depart_us,
            client_arrival_ms_lo16: self.client_arrival_ms_lo16,
            size_bytes: self.size_bytes,
            probe: self.probe,
        }
    }
}

impl IoBridge {
    /// BWE Stage 2.4: open a new probe window if goog_cc requested one
    /// since the last poll, then close the current window once its
    /// duration has elapsed. Called unconditionally from every `run()`
    /// iteration — not only when `bwe_samples_buffer` had something to
    /// drain — because a probe window on an otherwise-idle link (no
    /// further ACKs arriving) must still close on schedule rather than
    /// hang open forever waiting for the next sample.
    ///
    /// `take_probe_request()` only ever returns `Some` in the same tick
    /// `self.bwe.update()` ran (it's populated by `absorb`, which only
    /// runs from inside `update()`), so in practice the "open" half of
    /// this only fires from ticks that also drained `bwe_samples_buffer` —
    /// but polling unconditionally costs nothing and keeps the "close"
    /// half correct regardless.
    /// Claim a pending probe cluster at the instant a drain with real work
    /// is about to run, so the burst it emits lands *inside* the window.
    ///
    /// goog_cc issues its initial ladder on a timer, which for us fires at
    /// session start — before the first frame has been captured. Measured,
    /// those windows opened at t=0 ms and t=4 ms and closed having sent
    /// nothing at all (`packets_sent=0`, not an under-fill), because the
    /// scheduler queue is empty at every tick: tiles are enqueued and
    /// drained inside one call, so no timer-driven observer between ticks
    /// ever sees work waiting. A `drain_for_probe_window_open` helper used
    /// to paper over this by draining on open; it could not work for the
    /// same reason — the queue it drained was always empty — and was
    /// removed along with this change.
    ///
    /// Opening here inverts that. `emit_via_scheduler` already switches its
    /// budget to `probe.target_rate_bps` whenever a window is open (see its
    /// `pre_clamp_budget`), so claiming the request immediately before that
    /// budget is computed means this frame drains *at the probe's rate*
    /// rather than the pacer's — which is what a probe cluster is. No
    /// padding is involved: the bytes are real frame content that was going
    /// to be sent regardless.
    ///
    /// This matters most at startup, where the first frame is a full screen
    /// and therefore the largest natural burst a session ever produces —
    /// the best probe material available, and available exactly when a
    /// fast capacity estimate is worth the most.
    pub(super) fn open_probe_window_for_burst(&mut self, now: std::time::Instant) {
        // An open window is already collecting; never replace it mid-flight.
        if self.active_probe.is_some() {
            return;
        }
        let Some(req) = self.bwe.take_probe_request() else {
            return;
        };
        tracing::debug!(
            target: LOG_TARGET,
            probe_id = req.id,
            target_rate_bps = req.target_rate_bps,
            duration_ms = req.duration.as_millis() as u64,
            min_probes = req.min_probes,
            min_bytes = req.min_bytes,
            "bwe: probe cluster window opened on burst"
        );
        self.active_probe = Some(ActiveProbe {
            id: req.id,
            target_rate_bps: req.target_rate_bps,
            ends_at: now + req.duration,
            min_probes: req.min_probes,
            min_bytes: req.min_bytes,
            bytes_sent: 0,
            packets_sent: 0,
        });
    }

    /// Close the active probe window once its duration is up.
    ///
    /// Opening moved to `open_probe_window_for_burst` (see its doc comment):
    /// a window opened from this tick-driven poll could — and measurably did
    /// — open at a moment with no work to emit, then close having sent
    /// nothing. Requests therefore sit in `bwe`'s pending queue, bounded by
    /// `MAX_PENDING_PROBE_REQUESTS`, until a drain with real content claims
    /// one. A session that never emits never probes, which is correct: there
    /// is nothing to measure a link with.
    pub(super) fn poll_probe_window(&mut self, now: std::time::Instant) {
        let is_due = matches!(&self.active_probe, Some(p) if now >= p.ends_at);
        if is_due {
            let probe = self.active_probe.take().expect("checked Some above");
            self.close_probe_window(probe);
        }
    }

    /// Credit exactly one of `probes_completed` / `probes_abandoned` for a
    /// window that just closed (naturally, at `ends_at`, or early because a
    /// new request replaced it). See `probes_abandoned`'s doc comment for
    /// why under-filling is the expected outcome on an idle link, not a
    /// bug — no padding is ever sent to force completion.
    fn close_probe_window(&mut self, probe: ActiveProbe) {
        if probe.packets_sent >= probe.min_probes && probe.bytes_sent >= probe.min_bytes {
            self.probes_completed += 1;
            tracing::debug!(
                target: LOG_TARGET,
                probe_id = probe.id,
                packets_sent = probe.packets_sent,
                bytes_sent = probe.bytes_sent,
                "bwe: probe cluster window completed"
            );
        } else {
            self.probes_abandoned += 1;
            tracing::debug!(
                target: LOG_TARGET,
                probe_id = probe.id,
                packets_sent = probe.packets_sent,
                min_probes = probe.min_probes,
                bytes_sent = probe.bytes_sent,
                min_bytes = probe.min_bytes,
                "bwe: probe cluster window abandoned — under-filled (no \
                 padding is sent; an idle link legitimately under-fills, \
                 see BWE Stage 2.4 design's \"No padding\" section)"
            );
        }
    }

    /// Snapshot of the bandwidth estimator, for the browserless harness.
    ///
    /// Gated the same way as `now_std`: the harness needs to assert on the
    /// estimate, production has no reason to reach in.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn bwe_snapshot(&self) -> crate::transport::bwe::BweSnapshot {
        self.bwe.snapshot()
    }

    /// Clone of the `Arc` behind `bwe_publish`, for a caller that is about
    /// to move `self` into a `spawn_local`d task (as the browserless
    /// harness does with `IoBridge::run`) and needs a way to read the BWE
    /// estimate back out afterwards. See `bwe_publish`'s doc comment on the
    /// ownership problem this solves: `bwe_snapshot()` above takes `&self`,
    /// which is unreachable once the bridge is owned by a spawned task.
    /// Call this *before* the move; read the clone's contents *after*
    /// aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn bwe_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::transport::bwe::BweSnapshot>> {
        self.bwe_publish.clone()
    }

    /// Update the suspension debounce from `fb`, recompute
    /// `adaptation_context.suspended`, then push the new context to the
    /// classifier. Called from `dispatch_feedback_bytes`. Bumps
    /// `last_update_seq`.
    ///
    /// `fb`'s loss counts are not read here. They cannot see the loss of a
    /// tile that fit in one datagram, so `adaptation_context.loss_rate`
    /// comes from the transmission ledger instead: see `refresh_loss_rate`.
    pub(super) fn ingest_feedback_for_adaptation(
        &mut self,
        fb: crate::transport::feedback::ReceiverFeedback,
    ) {
        self.note_client_feedback(&fb);
        let suspended = fb.suspension_detected;
        self.recent_suspension_flags = [self.recent_suspension_flags[1], suspended];
        self.adaptation_context.suspended =
            self.recent_suspension_flags[0] || self.recent_suspension_flags[1];
        self.adaptation_context.last_update_seq =
            self.adaptation_context.last_update_seq.wrapping_add(1);
        self.classifier
            .set_adaptation_context(self.adaptation_context);
    }

    /// Withdraw the pending loss of every transmission that has since been
    /// acknowledged late, one pass over `pending_losses` per call.
    ///
    /// A loss declared in a turn with no live acknowledgement waits in
    /// `pending_losses` for the next one. If the "lost" datagram turns up in
    /// the meantime it did not get lost, and reporting it to goog_cc would
    /// feed the loss-based estimator a loss that never happened. A loss
    /// that has already been handed to goog_cc cannot be taken back, and
    /// stays reported. The retracted transmission is deliberately fed to the
    /// estimator as neither lost nor delivered: its arrival time reflects the
    /// stall, not the path.
    ///
    /// `late_acked` is cleared unconditionally, so it is bounded by one turn's
    /// acknowledgements however long nothing is flushed.
    pub(super) fn retract_late_acked_losses(&mut self) {
        if self.late_acked.is_empty() {
            return;
        }
        let before = self.pending_losses.len();
        let late = std::mem::take(&mut self.late_acked);
        self.pending_losses.retain(|l| !late.contains(&l.wire_seq));
        self.pending_losses_retracted += (before - self.pending_losses.len()) as u64;
    }

    /// Declare lost everything outstanding past the horizon, and bring the
    /// loss rate up to date.
    ///
    /// This is the complete loss signal: it does not care whether the
    /// content was superseded, which is precisely what NACK- and RTO-derived
    /// signals could not see.
    pub(super) fn expire_transmissions(&mut self, now: std::time::Instant) {
        let horizon = self.loss_horizon();
        let expired = self.transmission_ledger.expire_with_seq(now, horizon);
        if crate::transport::reliable_emitter::rto_probe_enabled() && !expired.is_empty() {
            eprintln!(
                "RTOPROBE expire n={} horizon_us={}",
                expired.len(),
                horizon.as_micros()
            );
        }
        self.instr.losses_declared += expired.len() as u64;
        for (wire_seq, tx) in expired {
            // The loss is reported against the moment the datagram went out,
            // the same clock the acknowledgements' send times are on, not the
            // moment it was built.
            self.pending_losses.push(PendingLoss {
                wire_seq,
                emit_us: tx.depart_us.unwrap_or(tx.emit_us),
                wire_bytes: tx.wire_bytes,
            });
            self.loss_window.record(tx.emit_us as u32, true);
        }
        self.refresh_loss_rate(now);
    }

    /// Re-read the sender-side loss rate and, if it moved, push it to the
    /// classifier. Called once per loop turn, so it also runs while nothing
    /// is being sent, which is what lets a held measurement lapse.
    fn refresh_loss_rate(&mut self, now: std::time::Instant) {
        let rate = self.loss_window.rate(self.reliable_emitter.emit_us(now));
        if rate == self.adaptation_context.loss_rate {
            return;
        }
        self.adaptation_context.loss_rate = rate;
        self.adaptation_context.last_update_seq =
            self.adaptation_context.last_update_seq.wrapping_add(1);
        self.classifier
            .set_adaptation_context(self.adaptation_context);
    }

    /// Compute `bytes_per_us` from a `(cwnd_bytes, smoothed_rtt_us)` snapshot,
    /// mirror into `adaptation_context`, bump `last_update_seq`, push to
    /// classifier. Pure function of the two inputs — exposed for unit tests
    /// so `sample_all_path_stats` can stay thin. `rtt <= 0` is a no-op.
    /// Seed the transmission ledger so a test can acknowledge content by
    /// naming a transmission, the way the wire now does.
    ///
    /// Production records these from `ReliableTileEmitter::take_transmissions`
    /// because `wire_seq` is allocated in there. A test that hand-builds an
    /// ACK has no emitter run behind it, so it declares the mapping directly.
    #[cfg(test)]
    pub(crate) fn seed_transmission_for_test(
        &mut self,
        wire_seq: u32,
        key: crate::transport::reliable_emitter::EmitKey,
    ) {
        self.transmission_ledger.record(
            wire_seq,
            now_std(),
            crate::transport::transmission_ledger::Transmission {
                emit_us: 0,
                wire_bytes: 1200,
                key,
                depart_us: None,
            },
        );
    }

    /// How long an unacknowledged transmission waits before being declared
    /// lost. See `LOSS_HORIZON_RTTS` and friends for why this adapts, and why
    /// it is bounded at both ends.
    ///
    /// Sourced from quinn's measured path RTT rather than the estimator's
    /// derived one: the derived value is downstream of the very feedback this
    /// horizon governs, and `GoogCcDriver::note_path_rtt` already exists
    /// because that derivation has been seen to go implausible.
    pub(super) fn live_path_rtt(&self) -> Option<std::time::Duration> {
        select_live_rtt(
            self.last_hello_from,
            self.server
                .connections
                .iter()
                .map(|(h, c)| (*h, c.stats().path.rtt)),
        )
    }

    pub(super) fn loss_horizon(&self) -> std::time::Duration {
        let rtt = self.live_path_rtt().unwrap_or(LOSS_HORIZON_FLOOR);
        (rtt * LOSS_HORIZON_RTTS).clamp(LOSS_HORIZON_FLOOR, LOSS_HORIZON_CEIL)
    }

    /// Hand the estimator what the hint window decided. See
    /// `CapacityHintWindow` for the rules.
    fn apply_hint_action(&mut self, action: HintAction) {
        match action {
            HintAction::Feed(v) => {
                self.instr.hint_feeds += 1;
                if self.bwe.snapshot().samples_seen == 0 {
                    self.instr.hint_feeds_before_feedback += 1;
                }
                self.bwe.set_transport_capacity_hint(v, now_std());
            }
            HintAction::Unbounded => {
                self.instr.hint_unbounded += 1;
                self.bwe.withdraw_transport_capacity_hint(now_std());
            }
            HintAction::None => {}
        }
    }

    /// Turn one `(cwnd, smoothed RTT)` sample into `bytes_per_us`, publish it
    /// to the classifier, and (while the path is being filled) to the
    /// estimator as a capacity hint. Ignores a non-positive RTT.
    pub(crate) fn apply_path_stats_snapshot(&mut self, cwnd_bytes: u64, smoothed_rtt_us: f32) {
        if smoothed_rtt_us <= 0.0 {
            return;
        }
        let bytes_per_us = (cwnd_bytes as f32) / smoothed_rtt_us;
        // Same number, second consumer: the classifier uses it to pick
        // codecs, and the BWE controller uses it as the independent ceiling
        // that lets it decide a probe is worth sending. See
        // `GoogCcDriver::set_transport_capacity_hint` for why a BDP-derived
        // rate is fed as a hint rather than trusted as a measurement.
        //
        // Only while we are actually filling the pipe. `cwnd / rtt` is a
        // rate only if `cwnd` is being used: on an idle link quinn holds
        // whatever window it last grew to while the RTT falls back to
        // baseline, so the ratio *rises* precisely when nothing is being
        // sent. Measured, feeding it unconditionally drove the estimate to
        // 22 Mbps on a 16 Mbps link across an 8 s idle gap, and that
        // overshoot then collapsed the estimate to goog_cc's 200 kbps floor
        // when traffic resumed and overran the link.
        //
        // Requiring in-flight bytes to be a real fraction of the window
        // keeps the hint tied to a window the path has actually carried.
        // Fed as a rolling minimum, not the instantaneous value. `cwnd / rtt`
        // is noisy in both directions: measured 42.6 Mbps on a 16 Mbps link
        // early in a session (a ballooned window over an inflated RTT),
        // settling to ~16.5 Mbps once the path steadied. Feeding the spike
        // hands goog_cc a licence to climb to a rate the link cannot carry.
        // A minimum over a short window discards the spikes and keeps the
        // conservative end, which is the right bias for something used as a
        // ceiling.
        // No traffic, no measurement, no hint. `cwnd / rtt` describes a path
        // only while packets are crossing it: on an idle link quinn holds the
        // window it last grew to while the RTT falls back to baseline, so the
        // ratio drifts *upward* precisely when nothing is being sent. Feeding
        // that hands goog_cc a licence to raise its estimate with nothing to
        // contradict it, and AIMD duly climbs at 8% per second for the whole
        // idle gap. Measured across an 8 s gap on a 16 Mbps link: the
        // estimate reached 22 Mbps, and the overshoot then collapsed it to
        // goog_cc's 200 kbps floor when traffic resumed and overran the link.
        //
        // `sent_packets` is quinn's own counter of packets actually put on
        // this path, so its delta is a direct "did we send anything since the
        // last sample" test — unlike `bytes_in_flight`, which counts the
        // retransmit cache and grows monotonically whether or not anything
        // is moving.
        // Judged over a window, not per sample: path stats are polled far
        // more often than packets are emitted, so almost every individual
        // sample sees no change even on a busy link. Gating on that directly
        // suppressed the hint entirely and gave back the whole convergence
        // improvement.
        let idle = match self.path_last_sent_at {
            Some(t) => now_std().saturating_duration_since(t) > PATH_IDLE_AFTER,
            None => true,
        };
        let action = if idle {
            // The classifier's `adaptation_context` below is deliberately
            // still updated: it wants the path's current state whether or not
            // the link is busy. Only the BWE hint, which licenses the
            // estimator to *raise* its estimate, is withheld.
            self.capacity_hint_window
                .clear(self.bwe.snapshot().samples_seen > 0)
        } else {
            // quinn is the limit when it still holds datagrams at the end of
            // the turn (cwnd, or its pacer at `1.25 x cwnd / srtt`).
            let quinn_limited = self.live_fifo_depth().unwrap_or(0) > 0;
            let feedback_heard = self.bwe.snapshot().samples_seen > 0;
            self.capacity_hint_window.observe(
                now_std(),
                std::time::Duration::from_secs_f64(f64::from(smoothed_rtt_us) / 1e6),
                bytes_per_us,
                quinn_limited,
                feedback_heard,
            )
        };
        self.apply_hint_action(action);
        self.adaptation_context.bytes_per_us = bytes_per_us;
        self.adaptation_context.smoothed_rtt_us = smoothed_rtt_us;
        self.adaptation_context.last_update_seq =
            self.adaptation_context.last_update_seq.wrapping_add(1);
        self.classifier
            .set_adaptation_context(self.adaptation_context);
    }

    /// Read live `path_stats` from every active connection, convert RTT
    /// (`Duration` → µs `f32`) and cwnd, and feed each to
    /// `apply_path_stats_snapshot`. Called once per tick from `run()`.
    /// With multiple connections, the last sampled snapshot wins — M3.6
    /// production is single-client; multi-client is M4+ scope.
    pub(super) fn sample_all_path_stats(&mut self) {
        // Collect first to satisfy the borrow checker: the immutable borrow of
        // `self.server.connections` must end before `apply_path_stats_snapshot`
        // takes `&mut self`.
        // Only the HELLO'd session's path feeds the classifier: an
        // unauthenticated connection must not steer bandwidth estimates.
        let snapshots: Vec<(u64, f32, u64)> = self
            .last_hello_from
            .and_then(|h| self.server.connections.get(&h))
            .map(|conn| {
                let stats = conn.stats();
                let rtt_us = stats.path.rtt.as_secs_f32() * 1_000_000.0;
                (stats.path.cwnd, rtt_us, stats.path.sent_packets)
            })
            .into_iter()
            .collect();
        for (cwnd, rtt_us, sent_packets) in snapshots {
            if sent_packets != self.path_sent_packets_last {
                self.path_sent_packets_last = sent_packets;
                self.path_last_sent_at = Some(now_std());
            }
            #[cfg(any(test, feature = "test-loss-injection"))]
            if let Some(forced) = self.test_force_bytes_per_us {
                // Override path: keep the live RTT (it's still real) but
                // replace bytes_per_us with the test value. apply_path_stats_snapshot
                // computes bytes_per_us = cwnd / rtt, so we synthesize a
                // cwnd that yields the desired forced value.
                let synth_cwnd = (forced * rtt_us) as u64;
                self.apply_path_stats_snapshot(synth_cwnd, rtt_us);
                continue;
            }
            self.apply_path_stats_snapshot(cwnd, rtt_us);
        }
        self.update_transport_probe();
    }
}

#[cfg(test)]
mod hint_window_tests {
    use super::*;
    use std::time::{Duration, Instant};

    const MS: Duration = Duration::from_millis(1);

    /// The window is a span of time. Sixteen readings taken within a few
    /// milliseconds are all inside it; readings older than the window are
    /// gone however few came after.
    #[test]
    fn the_window_is_a_span_of_time_not_a_count() {
        let t0 = Instant::now();
        let srtt = Duration::from_millis(20);
        let mut w = CapacityHintWindow::default();
        // Twenty readings in 2 ms: all kept (the old window kept sixteen).
        for i in 0..20u32 {
            w.observe(
                t0 + Duration::from_micros(100 * i as u64),
                srtt,
                5.0,
                true,
                true,
            );
        }
        assert_eq!(w.samples.len(), 20);
        // 4 x 20 ms = 80 ms is under the 100 ms floor, so the window is 100 ms:
        // a reading 101 ms later finds every earlier one expired.
        assert_eq!(CapacityHintWindow::window(srtt), 100 * MS);
        w.observe(t0 + 103 * MS, srtt, 7.0, true, true);
        assert_eq!(w.samples.len(), 1);
        // 4 round trips once that is longer than the floor.
        assert_eq!(
            CapacityHintWindow::window(Duration::from_millis(60)),
            240 * MS
        );
    }

    #[test]
    fn only_readings_taken_while_quinn_was_the_limit_count() {
        let t0 = Instant::now();
        let srtt = Duration::from_millis(30);
        let mut w = CapacityHintWindow::default();
        // A wildly high reading while the bucket was the limit: ignored, and
        // with nothing else to go on the estimator is told nothing caps it.
        assert_eq!(
            w.observe(t0, srtt, 90.0, false, true),
            HintAction::Unbounded
        );
        assert!(w.samples.is_empty());
        // Then a real one.
        assert_eq!(
            w.observe(t0 + MS, srtt, 3.0, true, true),
            HintAction::Feed(3.0)
        );
        // The minimum over the window is what is fed.
        assert_eq!(
            w.observe(t0 + 2 * MS, srtt, 4.0, true, true),
            HintAction::Feed(3.0)
        );
    }

    /// Nothing is fed before the estimator has heard feedback, however many
    /// readings came first; the first feedback then releases what was seen.
    #[test]
    fn nothing_is_fed_before_the_first_feedback_whatever_the_reading_count() {
        let t0 = Instant::now();
        let srtt = Duration::from_millis(40);
        for readings in [0u64, 1, 15, 16, 17, 64, 1_000] {
            let mut w = CapacityHintWindow::default();
            for i in 0..readings {
                let a = w.observe(t0 + Duration::from_micros(10 * i), srtt, 0.036, true, false);
                assert_eq!(a, HintAction::None, "{readings} readings, before feedback");
            }
            let after = w.observe(
                t0 + Duration::from_micros(10 * readings),
                srtt,
                0.036,
                true,
                true,
            );
            assert_eq!(after, HintAction::Feed(0.036), "{readings} readings");
        }
    }

    #[test]
    fn the_hint_is_unbounded_once_when_the_window_empties() {
        let t0 = Instant::now();
        let srtt = Duration::from_millis(20);
        let mut w = CapacityHintWindow::default();
        assert_eq!(w.observe(t0, srtt, 2.0, true, true), HintAction::Feed(2.0));
        // Not quinn-limited any more; the reading ages out after 100 ms.
        assert_eq!(
            w.observe(t0 + 50 * MS, srtt, 9.0, false, true),
            HintAction::Feed(2.0)
        );
        assert_eq!(
            w.observe(t0 + 150 * MS, srtt, 9.0, false, true),
            HintAction::Unbounded
        );
        assert_eq!(
            w.observe(t0 + 160 * MS, srtt, 9.0, false, true),
            HintAction::None,
            "told once, not every turn"
        );
        // An idle path releases a hint too, and once.
        assert_eq!(
            w.observe(t0 + 170 * MS, srtt, 2.0, true, true),
            HintAction::Feed(2.0)
        );
        assert_eq!(w.clear(true), HintAction::Unbounded);
        assert_eq!(w.clear(true), HintAction::None);
        // Before any feedback there is nothing to release.
        assert_eq!(CapacityHintWindow::default().clear(false), HintAction::None);
    }
}
