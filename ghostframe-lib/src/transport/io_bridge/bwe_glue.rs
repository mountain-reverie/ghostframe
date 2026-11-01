//! Glue between the transport's observations and the bandwidth estimator /
//! adaptation context: quinn path stats, the transmission ledger, receiver
//! feedback and the probe window.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - the loss signal: `transmission_ledger` expiry (`expire_transmissions`,
//!   `loss_path`, `live_path_rtt`, `select_live_rtt`), `pending_losses`,
//!   `loss_window` and `adaptation_context.loss_rate` (`refresh_loss_rate`);
//! - path-stats sampling (`sample_all_path_stats`,
//!   `apply_path_stats_snapshot`), which writes `adaptation_context`
//!   (`bytes_per_us`, `smoothed_rtt_us`);
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
//! `apply_path_stats_snapshot` feeds the classifier's view of the path.
//!
//! Must not: compute a per-tick send budget or touch the scheduler or the
//! quinn send buffers.

use super::*;

/// Path RTT of the live (HELLO'd) session only. An unauthenticated or
/// evicted connection must not steer the BWE reference.
/// `None` when there is no live session or its connection is gone.
pub(super) fn select_live_rtt(
    live: Option<ConnectionHandle>,
    rtts: impl Iterator<Item = (ConnectionHandle, std::time::Duration)>,
) -> Option<std::time::Duration> {
    let live = live?;
    rtts.filter(|(h, _)| *h == live).map(|(_, r)| r).min()
}

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

/// Outstanding transmissions the ledger will hold. Sized by send rate x round
/// trip rather than session length — roughly 2000 packets/s at 20 Mbps against
/// a round trip measured in hundreds of milliseconds. The cap is a backstop
/// against a peer that stops acknowledging (and, with no horizon any more, the
/// only thing that retires the records of a dead tail the probe cannot find
/// content for), not the normal bound.
pub(super) const TRANSMISSION_LEDGER_CAPACITY: usize = 16_384;

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
    /// When the window opened, for its length at close.
    pub(super) opened_at: std::time::Instant,
    /// An admission pass inside the window ran out of work.
    pub(super) starved: bool,
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
            opened_at: now,
            starved: false,
        });
        // The window must carry the probe, or quinn clips the cluster.
        self.publish_model_window(false);
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
    pub(super) fn close_probe_window(&mut self, probe: ActiveProbe) {
        self.publish_model_window(false);
        let n = self.instr.probe_windows_closed as usize;
        if let Some(slot) = self.instr.probe_windows.get_mut(n) {
            *slot = crate::transport::instruments::ProbeWindow {
                target_bps: probe.target_rate_bps,
                duration_us: probe
                    .ends_at
                    .saturating_duration_since(probe.opened_at)
                    .as_micros() as u64,
                bytes_sent: probe.bytes_sent.max(0) as u64,
                packets_sent: probe.packets_sent.max(0) as u64,
                starved: probe.starved,
            };
        }
        self.instr.probe_windows_closed += 1;
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

    /// Declare lost what the acknowledgements received so far put out of
    /// contention (packet and time threshold, RFC 9002 section 6.1), queue one
    /// retransmission for each whose content is still current, and bring the
    /// loss rate up to date.
    ///
    /// This is the complete loss signal: it does not care whether the content
    /// was superseded, which is precisely what NACK- and RTO-derived signals
    /// could not see. It replaces the 3 x RTT horizon, which declared a loss
    /// for anything the clock called overdue, whether the path was dropping or
    /// only queueing.
    pub(super) fn expire_transmissions(&mut self, now: std::time::Instant) {
        let out = crate::transport::loss_path::detect(
            &mut self.transmission_ledger,
            &mut self.reliable_emitter,
            now,
        );
        self.instr.losses_declared += out.lost.len() as u64;
        self.instr.loss_retx_requested += out.retx_queued;
        for (wire_seq, tx) in out.lost {
            self.report_loss(wire_seq, tx);
        }
        self.refresh_loss_rate(now);
    }

    /// Feed one loss verdict to the estimators: the pending list (flushed to
    /// goog_cc with the next acknowledgement) and the sender-side loss window.
    fn report_loss(
        &mut self,
        wire_seq: u32,
        tx: crate::transport::transmission_ledger::Transmission,
    ) {
        // The loss is reported against the moment the datagram went out, the
        // same clock the acknowledgements' send times are on, not the moment
        // it was built.
        self.pending_losses.push(PendingLoss {
            wire_seq,
            emit_us: tx.depart_us.unwrap_or(tx.emit_us),
            wire_bytes: tx.wire_bytes,
        });
        self.loss_window.record(tx.emit_us as u32, true);
    }

    /// Declare `wire_seqs` lost as a verdict would, for a test that is about
    /// what a loss does afterwards, not about finding one.
    #[cfg(test)]
    pub(crate) fn declare_lost_for_test(&mut self, wire_seqs: &[u32]) {
        for ws in wire_seqs {
            if let Some(tx) = self.transmission_ledger.declare_lost_by_pto(*ws) {
                self.instr.losses_declared += 1;
                self.report_loss(*ws, tx);
            }
        }
    }

    /// Run the probe timeout if it is due (see `loss_path::probe`).
    pub(super) fn poll_pto(&mut self, now: std::time::Instant) {
        let Some(out) = crate::transport::loss_path::probe(
            &mut self.transmission_ledger,
            &mut self.reliable_emitter,
            now,
        ) else {
            return;
        };
        self.instr.pto_probes_requested += out.retx_queued;
        self.instr.losses_declared += out.lost.len() as u64;
        for (wire_seq, tx) in out.lost {
            self.report_loss(wire_seq, tx);
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
                content: key.into(),
                depart_us: None,
                dep_idx: None,
            },
        );
    }

    /// quinn's measured path RTT for the HELLO'd connection. Used to give the
    /// goog_cc estimator a plausibility reference.
    pub(super) fn live_path_rtt(&self) -> Option<std::time::Duration> {
        select_live_rtt(
            self.last_hello_from,
            self.server
                .connections
                .iter()
                .map(|(h, c)| (*h, c.stats().path.rtt)),
        )
    }

    /// Publish the classifier's view of the path from one smoothed RTT: the
    /// rate is the estimate (`model_rate_bytes_per_s`), which quinn's window
    /// is derived from, so `cwnd / srtt` would say nothing the estimate does
    /// not. Ignores a non-positive RTT.
    pub(crate) fn apply_path_stats_snapshot(&mut self, smoothed_rtt_us: f32) {
        if smoothed_rtt_us <= 0.0 {
            return;
        }
        let bytes_per_us = (self.model_rate_bytes_per_s() / 1e6) as f32;
        // The test override replaces the *classifier's* view of the headroom.
        #[cfg(any(test, feature = "test-loss-injection"))]
        let bytes_per_us = self.test_force_bytes_per_us.unwrap_or(bytes_per_us);
        self.adaptation_context.bytes_per_us = bytes_per_us;
        self.adaptation_context.smoothed_rtt_us = smoothed_rtt_us;
        self.adaptation_context.last_update_seq =
            self.adaptation_context.last_update_seq.wrapping_add(1);
        self.classifier
            .set_adaptation_context(self.adaptation_context);
    }

    /// Read the live smoothed RTT from the HELLO'd connection and feed it to
    /// `apply_path_stats_snapshot`. Called once per tick from `run()`.
    pub(super) fn sample_all_path_stats(&mut self) {
        // Only the HELLO'd session's path feeds the classifier: an
        // unauthenticated connection must not steer bandwidth estimates.
        let rtt_us = self
            .last_hello_from
            .and_then(|h| self.server.connections.get(&h))
            .map(|conn| conn.stats().path.rtt.as_secs_f32() * 1_000_000.0);
        if let Some(rtt_us) = rtt_us {
            self.apply_path_stats_snapshot(rtt_us);
        }
        self.model_note_persistent_congestion();
        self.update_transport_probe();
    }
}
