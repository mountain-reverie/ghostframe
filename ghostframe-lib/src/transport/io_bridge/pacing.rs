//! The admission rate, the depth gate's inputs, and the pass that feeds quinn.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - the rate: the estimate, the probe override and `tick_budget_floor`,
//!   folded into one bytes-per-second figure by `admission_rate`;
//! - the admission pass `pump_admission`, which refills the bucket from that
//!   rate, serves the byte classes in strict priority against it and stops at
//!   the depth gate (the arithmetic is in `transport::admission`);
//! - `refill_after_drain`, which runs the pass again after quinn has emptied
//!   its queue, so that quinn is not left idle between turns;
//! - `emit_via_scheduler`, the frame-time entry into the pass;
//! - the stuck-entry sweep (`sweep_stuck_entries`, `STUCK_ENTRY_AGE`,
//!   `STUCK_SWEEP_EVERY_TICKS`);
//! - `priority_retry_after` and the harness setter `set_tick_budget_floor_bytes`.
//!
//! Boundary with `bwe_glue.rs`: this file consumes the estimator (the
//! `bwe.snapshot()` samples count and `pacer_rate_bps`) and the probe window
//! (`active_probe.target_rate_bps` replaces the rate while a window is open)
//! but does not manage the window. It hands the estimator one thing, at the
//! end of every pass: the bucket's cumulative grant and spend
//! (`bwe.note_admission`), which the estimate ceiling reads to tell a
//! rate-limited sender from one with nothing to send.
//! `emit_via_scheduler` straddles the two (it calls
//! `open_probe_window_for_burst`) and is left here whole.
//!
//! Must not: decode ACKs or feedback, update the loss signal, or decide session lifetime.

use super::*;
use crate::transport::admission::{depth_target, pull_in_priority, source_budget, AdmitClass};
use crate::transport::instruments::BudgetTerm;

// ---------------------------------------------------------------------------
// The admission rate
// ---------------------------------------------------------------------------
// The rate was a budget per scheduler tick, spent once per call, with a
// fixed 33 ms slice whatever the call interval, a multiplier that backed off
// when quinn refused a datagram (it never does: the send buffer is 16 MiB),
// and a give-back that billed retransmission at half price. It is now a
// rate: bytes per second, refilled by elapsed time into one bucket that every
// byte class is billed against. The numbers it is built from are unchanged.

/// The tick the rate is expressed over. The scheduler's own cadence is a 30 fps
/// capture; the rate is `per-tick bytes / tick`.
pub(crate) const SCHEDULER_TICK_INTERVAL_US: f64 = 33_333.0;

/// Rate floor, in datagrams per tick rather than bytes.
///
/// This was a flat 256 KiB, which is 7.86 MB/s at a 33.3 ms tick: the
/// bandwidth-derived term only overtook the floor above ~70 Mbit/s -- above
/// every link this system serves. The rate was therefore a constant, and slow
/// links were overdriven by up to ~12x. Measured cost of that on a saturating
/// 60x34 scene: 511,473 retransmit attempts and an emitter queue 389,053 deep
/// at 5 Mbit, and 1,213,785 retransmits on a 5 -> 50 Mbit step-up.
///
/// The floor's only legitimate job is to keep the rate above a single
/// datagram per tick, so a tile can always make progress even when the
/// estimate is pessimistic or not yet established. Everything above that
/// belongs to the estimate. See `docs/specs/blocked-path-redesign.md`.
const SCHEDULER_TICK_BUDGET_FLOOR_DATAGRAMS: usize = 1;

/// Datagram size assumed when no session is connected yet, so the floor is
/// defined before `compute_max_datagram_size` has a live connection to read.
const SCHEDULER_TICK_BUDGET_FLOOR_FALLBACK_BYTES: usize = 1_200;

/// Fewest tokens an admission pass will start pulling new work with: about a
/// quarter of a datagram. A pass that pulled with less would only build a
/// datagram it could not yet afford and hold it, with its timer running.
const MIN_PULL_BYTES: usize = 300;

/// The fixed part of a tile datagram's wire size beyond its payload: the
/// datagram header and the tile header. The WebTransport framing is added on
/// top, per session.
const TILE_DATAGRAM_HEADERS: usize =
    crate::transport::protocol::DATAGRAM_HEADER_SIZE + crate::transport::protocol::TILE_HEADER_SIZE;

/// Most passes one `pump_admission` call makes before it stops. Each pass
/// admits up to `D_target` and the bucket holds a few passes' worth, so this
/// is a backstop against a livelock, not a limit anything reaches.
const MAX_PASSES_PER_PUMP: usize = 64;

/// Most pump/drain rounds `refill_after_drain` makes in one turn. A round
/// spends tokens, and the bucket never holds more than a burst.
const MAX_REFILL_ROUNDS: usize = 256;

/// How long an entry may keep retransmitting before it is retired and its
/// content re-sent fresh instead.
///
/// The retransmit cache is unbounded and 0aa97de deliberately removed the
/// attempt cap, because a cap of 4 firings abandoned passes that were still
/// legitimately in flight. That was the right call, but it left no aggregate
/// bound at all: with backoff capped at `RTO_BACKOFF_MAX` (5 s), every entry
/// that never gets acknowledged costs one retransmit every 5 s forever.
/// Measured in the field on a daemon up two hours: 2697 pending entries
/// producing 549 retransmits/s, which is what `cache_pending / 5 s` predicts to
/// within 2%. That traffic exceeded the whole send budget, and since the
/// scheduler admits by cost (solid 0.05 us, palRLE 8 us, CDF 5/3 90 us) the
/// cheap codecs still fit while CDF 5/3 emitted nothing at all — the client
/// received flat-colour tiles and no detail, indefinitely.
///
/// 30 s is chosen to be unambiguous rather than tight. The cache legitimately
/// holds tens of thousands of entries during a first-paint burst over a
/// high-RTT link, so a size threshold would fire on healthy traffic; an age
/// threshold does not. Nothing that is still plausibly in flight is 30 s old,
/// and a payload that age is a stale snapshot regardless — re-encoding beats
/// retransmitting it.
const STUCK_ENTRY_AGE: std::time::Duration = std::time::Duration::from_secs(30);

/// Ticks between stuck-entry sweeps. The sweep scans the whole cache, which is
/// large exactly when it matters, so it does not belong on every tick.
const STUCK_SWEEP_EVERY_TICKS: u32 = 30;

/// The token rate that carries `countable_bps` of the bytes goog_cc counts,
/// given the payload fraction (countable bytes per token billed). The fraction
/// is clamped to `[0.5, 1.0]`: a reading outside it is a window that straddled
/// a burst of control traffic, and more than every token carrying payload is
/// not possible.
pub(super) fn countable_to_token_bps(countable_bps: u64, payload_fraction: f64) -> u64 {
    (countable_bps as f64 / payload_fraction.clamp(0.5, 1.0)) as u64
}

/// Why a drain of the emission queue stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    /// Everything built has been handed to quinn.
    Empty,
    /// quinn's FIFO already holds `D_target`.
    Depth,
    /// The bucket cannot afford the next datagram.
    Tokens,
    /// quinn refused a datagram: its buffer is full.
    Rejected,
}

/// What the depth gate is checked against, set by each admission pass.
#[derive(Debug, Clone, Copy)]
pub(super) struct GateParams {
    /// `D_target`, bytes.
    pub(super) d_target: usize,
}

/// The shortest wait the loop is woken for when the bucket is empty:
/// `max(1 ms, one datagram at the rate)`. A wake for less than a datagram's
/// worth of tokens finds the pass unable to send, sets the same wake again,
/// and spins the loop; at 1 ms the loop does at most 1,000 passes a second
/// however high the rate.
pub(super) fn token_wake_floor(mtu: usize, rate: f64) -> std::time::Duration {
    let datagram = if rate > 0.0 {
        std::time::Duration::try_from_secs_f64(mtu as f64 / rate).unwrap_or_default()
    } else {
        std::time::Duration::ZERO
    };
    datagram.max(std::time::Duration::from_millis(1))
}

/// The rate admission runs at and the term that set it.
pub(super) struct AdmissionRate {
    pub(super) bytes_per_sec: f64,
    pub(super) term: BudgetTerm,
}

/// What one admission pass did.
#[derive(Default)]
pub(super) struct PumpOutcome {
    pub(super) stats: FrameSendStats,
    /// Wire bytes and tile count the scheduler gave this pass.
    pub(super) scheduler_bytes: usize,
    pub(super) scheduler_count: usize,
    /// Datagrams quinn accepted during the pass, whatever their class.
    pub(super) admitted: usize,
}

impl IoBridge {
    /// Bytes quinn's datagram queue holds for the live session, or `None`
    /// with no live session. One subtraction (quinn `datagrams.rs`).
    pub(super) fn live_fifo_depth(&mut self) -> Option<usize> {
        #[cfg(test)]
        if let Some(d) = self.forced_fifo_depth_for_test {
            return Some(d);
        }
        let capacity = self.server.datagram_send_buffer();
        let handle = self.live_send_handle()?;
        let conn = self.server.connections.get_mut(&handle)?;
        Some(capacity.saturating_sub(conn.datagrams().send_buffer_space()))
    }

    /// The WebTransport framing the live session adds to every datagram.
    pub(super) fn live_framing_len(&self) -> usize {
        self.live_send_handle()
            .and_then(|h| self.wt_sessions.get(&h))
            .map_or(1, |wt| wt.datagram_framing_len())
    }

    /// One datagram on the wire, framing included: the floor of `D_target`
    /// and the size the bucket is sized for.
    fn wire_mtu(&self) -> usize {
        self.last_max_datagram_size
            .unwrap_or(SCHEDULER_TICK_BUDGET_FLOOR_FALLBACK_BYTES)
            + Self::WT_VARINT_OVERHEAD
    }

    /// The rate admission runs at, bytes per second, and which input set it
    /// (D1 design A, the model controller):
    ///
    /// ```text
    /// R = max( probe_target / f, E / f, one datagram per tick )
    /// ```
    ///
    /// * `E` is the estimate (goog_cc's loss-based target, bounded by the
    ///   estimate ceiling while the sender is saturated) and `probe_target` a
    ///   probe window's rate while one is open, zero otherwise.
    /// * `f` is the payload fraction (`BweSnapshot::payload_fraction`, 0.8 to
    ///   0.9 here): the countable share of the bytes the bucket bills. `E` and
    ///   `probe_target` are rates of the bytes goog_cc counts; the bucket
    ///   bills parity, framing and control datagrams too, so the token rate
    ///   that carries a countable rate is that rate over `f`.
    ///
    /// There is no `cwnd / srtt` term: quinn's window is derived from this
    /// same `R`, so it cannot disagree with it. The floor is the one thing
    /// under it, a datagram a tick, so a tile can always make progress.
    pub(super) fn admission_rate(&self) -> AdmissionRate {
        let tick_s = SCHEDULER_TICK_INTERVAL_US / 1_000_000.0;
        let snap = self.bwe.snapshot();
        let est = countable_to_token_bps(snap.bitrate_bps, snap.payload_fraction) as f64 / 8.0;
        let probe = self.active_probe.as_ref().map_or(0.0, |p| {
            countable_to_token_bps(p.target_rate_bps, snap.payload_fraction) as f64 / 8.0
        });
        let floor = self.tick_budget_floor() as f64 / tick_s;
        let term = if probe > est && probe >= floor {
            BudgetTerm::Probe
        } else if floor > est.max(probe) {
            BudgetTerm::Floor
        } else {
            // The estimate sets the rate (`Pacer`: goog_cc's).
            BudgetTerm::Pacer
        };
        AdmissionRate {
            bytes_per_sec: est.max(probe).max(floor),
            term,
        }
    }

    /// Whether admission is holding work it has not been able to send: a
    /// datagram built and waiting, a client request or a timer due, or tiles
    /// in the scheduler. A scheduler with no frame identity to tag its work
    /// with has nothing it can send.
    pub(super) fn admission_backlog(&self, now: std::time::Instant) -> bool {
        self.reliable_emitter.emission_queue_len() > 0
            || self.reliable_emitter.pending_nack_count() > 0
            || self.reliable_emitter.pending_loss_count() > 0
            || self.reliable_emitter.pto_due(now)
            || (self.last_drain_frame_context.is_some() && self.scheduler.has_pending_work())
    }

    /// One admission pass: refill the bucket, then serve the classes in
    /// strict priority until the depth gate, the bucket or the work runs out.
    ///
    /// The order inside a pass is the priority order: NACK repair, then the
    /// timer's retransmissions, then the scheduler (critical tier before
    /// refinement), each against what the ones before left. Parity is not
    /// pulled; it is built by the emitter as sources are sent and is charged
    /// when it is emitted, so `source_budget` leaves it a share.
    ///
    /// Work that cannot be admitted stays where it was built: in the
    /// scheduler (where a newer frame supersedes it), in the NACK list or in
    /// the timer wheel. Nothing is built that cannot be sent in the same
    /// pass, bar the datagram that crosses the bound, because a built
    /// datagram has a wire_seq and a running timer.
    pub(super) fn pump_admission(&mut self) -> PumpOutcome {
        let now = now_std();
        let mut out = PumpOutcome::default();
        self.model_before_admission(now);
        let rate = self.admission_rate();
        let mtu = self.wire_mtu();
        let framing = self.live_framing_len();
        self.admission.refill(now, rate.bytes_per_sec, mtu);
        let d_target = depth_target(rate.bytes_per_sec, mtu);
        self.gate = Some(GateParams { d_target });
        {
            let a = &mut self.instr.admission;
            a.pumps += 1;
            a.d_target_last = d_target as u64;
            a.mtu = mtu as u64;
            a.d_target_max = a.d_target_max.max(d_target as u64);
        }
        self.instr.budget.note_rate(rate.term);
        self.last_admission_term = rate.term;
        self.scheduler
            .set_item_overhead(TILE_DATAGRAM_HEADERS + framing);
        self.reliable_emitter.set_wire_overhead(framing);

        let admitted_before = self.instr.admission.datagrams_admitted;
        // What an earlier pass built and could not send goes first.
        let mut stop = self.drain_reliable_emitter(now);
        let mut passes = 0;
        while stop == Stop::Empty && passes < MAX_PASSES_PER_PUMP {
            passes += 1;
            let depth = self.live_fifo_depth().unwrap_or(0);
            if depth >= d_target {
                stop = Stop::Depth;
                break;
            }
            let tokens = self.admission.available();
            if tokens < MIN_PULL_BYTES {
                stop = Stop::Tokens;
                break;
            }
            // Room under the gate, at least one datagram so a refinement pass
            // (which the scheduler admits whole) is never starved by a small
            // remainder; tokens, less the parity the sources will owe, but
            // never less than a datagram when a datagram is affordable.
            let room = (d_target - depth).max(mtu);
            let budget = source_budget(tokens).max(tokens.min(mtu)).min(room);
            let pulled =
                pull_in_priority(
                    budget,
                    &AdmitClass::PRIORITY,
                    |class, remaining| match class {
                        AdmitClass::NackRepair => {
                            let mut n = 0;
                            self.reliable_emitter.tick_nacks_budgeted(
                                now,
                                usize::MAX,
                                remaining,
                                &mut n,
                            )
                        }
                        AdmitClass::Retransmit => {
                            let mut n = 0;
                            self.reliable_emitter.tick_loss_budgeted(
                                now,
                                usize::MAX,
                                remaining,
                                &mut n,
                            )
                        }
                        // The scheduler serves its critical tier before its
                        // refinement tier inside one call, so the two classes are
                        // one pull and `Refinement` has nothing of its own.
                        AdmitClass::Critical => match self.last_drain_frame_context {
                            Some(ctx) => {
                                let (stats, bytes, count) = self.drain_scheduler_into_quinn(
                                    ctx.seq,
                                    ctx.timestamp_us,
                                    ctx.max_frag,
                                    remaining,
                                );
                                out.stats.accumulate(&stats);
                                out.scheduler_bytes += bytes;
                                out.scheduler_count += count;
                                bytes + count * framing
                            }
                            None => 0,
                        },
                        AdmitClass::Refinement | AdmitClass::Parity => 0,
                    },
                );
            stop = self.drain_reliable_emitter(now);
            if pulled == 0 && stop == Stop::Empty {
                break;
            }
        }
        out.admitted = (self.instr.admission.datagrams_admitted - admitted_before) as usize;
        // A pass that ran out of work before the cluster was full: the frame
        // was smaller than the probe, and a short fill says nothing about the
        // rate.
        if stop == Stop::Empty {
            if let Some(p) = self.active_probe.as_mut() {
                p.starved |= p.bytes_sent < p.min_bytes;
            }
        }
        // What the bucket granted and what was spent of it, for the estimate
        // ceiling: it applies only to a sender that used its grant.
        if stop == Stop::Depth && self.admission_backlog(now) && self.model_window_limit_is_load() {
            let dropped = self.admission.forfeit_bank();
            self.model_note_gated_pass(dropped);
        }
        self.bwe.note_admission(
            now,
            self.admission.granted_total(),
            self.admission.spent_total() + self.model_forfeited(),
        );

        // What held the rest back, and when to look again.
        self.admission_wake = None;
        // Only a pass that stopped early has anything to account for.
        if stop != Stop::Empty && self.admission_backlog(now) {
            match stop {
                Stop::Depth => self.instr.budget.gate_depth += 1,
                Stop::Rejected => self.instr.budget.gate_rejected += 1,
                Stop::Tokens => {
                    self.instr.budget.gate_tokens += 1;
                    // The bucket refills by time alone; nothing else will
                    // wake the loop for it.
                    let need = if self.reliable_emitter.emission_queue_len() > 0 {
                        mtu
                    } else {
                        MIN_PULL_BYTES
                    };
                    let wait = self
                        .admission
                        .time_until(need, rate.bytes_per_sec)
                        .unwrap_or(std::time::Duration::from_millis(33))
                        .max(token_wake_floor(mtu, rate.bytes_per_sec));
                    self.admission_wake = Some(now + wait);
                }
                Stop::Empty => {}
            }
        }
        out
    }

    /// Run the admission pass again after quinn has written packets, and
    /// again after each of those drains, until it admits nothing.
    ///
    /// quinn empties its FIFO inside `drain_outbound`. A pass made only
    /// before that drain leaves quinn idle until the next turn, however much
    /// the bucket holds: the FIFO stands at `D_target` (10 ms of the rate)
    /// and one drain can write all of it. So the pass follows every drain.
    pub(super) async fn refill_after_drain(&mut self) -> io::Result<()> {
        for _ in 0..MAX_REFILL_ROUNDS {
            if self.pump_admission().admitted == 0 {
                break;
            }
            self.drain_outbound().await?;
        }
        self.note_turn_end_admission();
        Ok(())
    }

    /// Count the turn as backlogged, and as starved if quinn was left with
    /// nothing, when admission ended the turn holding work and the tokens to
    /// send it. `starved_turns` is the number refill-after-drain exists to
    /// keep at zero.
    fn note_turn_end_admission(&mut self) {
        let Some(depth) = self.live_fifo_depth() else {
            return;
        };
        if self.admission.available() < self.wire_mtu() {
            return;
        }
        if !self.admission_backlog(now_std()) {
            return;
        }
        self.instr.admission.backlog_turns += 1;
        if depth == 0 {
            self.instr.admission.starved_turns += 1;
        }
    }

    /// Retire cache entries that have been retransmitting for longer than
    /// anything in flight plausibly is, and re-dirty their tiles. A sweep of
    /// the whole cache, so every `STUCK_SWEEP_EVERY_TICKS` calls.
    pub(super) fn sweep_stuck_entries(&mut self) {
        let tick_now = now_std();
        self.stuck_sweep_counter = self.stuck_sweep_counter.wrapping_add(1);
        if !self
            .stuck_sweep_counter
            .is_multiple_of(STUCK_SWEEP_EVERY_TICKS)
        {
            return;
        }
        let retired = self
            .reliable_emitter
            .retire_stuck(tick_now, STUCK_ENTRY_AGE);
        if !retired.is_empty() {
            // Not a drop. Forcing the tiles dirty re-encodes them at a
            // current frame_seq and sends them again, so the client ends up
            // with newer content than the retired payload carried. This is
            // the same mechanism a screen change uses; we are only
            // asserting that a tile stuck this long counts as changed.
            //
            // Deliberately the existing whole-frame force rather than a
            // per-tile one: there is no per-tile re-dirty hook, and a
            // cache in this state is not a one-tile problem.
            tracing::warn!(
                target: LOG_TARGET,
                retired = retired.len(),
                pending_after = self.reliable_emitter.pending_cache_entries(),
                age_s = STUCK_ENTRY_AGE.as_secs(),
                "retired stuck retransmit entries and forced a re-dirty; \
                 each was costing a retransmit every RTO_BACKOFF_MAX forever. \
                 Sustained non-zero here means acknowledgements are not \
                 retiring content the normal way — check ledger_unknown_acks."
            );
            self.force_dirty_frames = self.force_dirty_frames.max(2);
            self.backend_apply(Control::InvalidateBaseline(2));
        }
    }

    /// Lower bound for the rate: one datagram at the live MTU,
    /// unless a test has pinned it. Sized in datagrams so it scales with the
    /// path rather than with a constant chosen for a link speed we do not
    /// serve -- see `SCHEDULER_TICK_BUDGET_FLOOR_DATAGRAMS`.
    pub(super) fn tick_budget_floor(&self) -> usize {
        if let Some(explicit) = self.tick_budget_floor_bytes {
            return explicit;
        }
        self.last_max_datagram_size
            .unwrap_or(SCHEDULER_TICK_BUDGET_FLOOR_FALLBACK_BYTES)
            .saturating_mul(SCHEDULER_TICK_BUDGET_FLOOR_DATAGRAMS)
    }

    /// How long the scheduler waits for a priority tile's acknowledgement
    /// before sending it again: the reliable emitter's first-attempt
    /// deadline (the probe timeout from the same RTT estimator), so the two
    /// retry mechanisms race the same clock.
    ///
    /// Not a function of QUIC RTT: the estimator measures application
    /// acknowledgements and is not seeded from the transport. See
    /// `Scheduler::set_retry_after`.
    pub(super) fn priority_retry_after(&self) -> std::time::Duration {
        self.reliable_emitter.pto()
    }

    /// The frame-time entry into the admission pass: tell it which frame the
    /// work it pulls belongs to, then run it.
    ///
    /// Both the capture path (`dispatch_dirty_tiles_via_scheduler`) and the
    /// harness injection path (`apply_injected_frame`) call this, so they
    /// cannot drift. Anything that must differ between the two callers
    /// belongs *before* this call, in how their work is produced and
    /// enqueued, never in how it is emitted.
    ///
    /// BWE burst discovery: a pending probe window is claimed *here*, at the
    /// one moment we know datagrams are about to be emitted. See
    /// `open_probe_window_for_burst`. While a window is open the rate is the
    /// probe's (see `admission_rate`).
    pub(super) fn emit_via_scheduler(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        max_frag: usize,
    ) -> (FrameSendStats, usize, usize) {
        self.open_probe_window_for_burst(now_std());
        // The scheduler re-sends an un-acknowledged priority tile once this
        // has elapsed. Set here, on the path every emission shares, so the
        // capture and injected paths cannot disagree about it.
        self.scheduler.set_retry_after(self.priority_retry_after());
        // Later passes, with no frame of their own, tag the work they pull
        // with this one's identity.
        self.last_drain_frame_context = Some(DrainFrameContext {
            seq,
            timestamp_us,
            max_frag,
        });
        let out = self.pump_admission();
        (out.stats, out.scheduler_bytes, out.scheduler_count)
    }

    /// Override the rate floor. See `tick_budget_floor_bytes`
    /// for why this is worth varying: the default sits above every link the
    /// system actually serves, so without lowering it a test cannot observe
    /// the bandwidth-derived budget binding at all.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_tick_budget_floor_bytes(&mut self, bytes: usize) {
        self.tick_budget_floor_bytes = Some(bytes);
    }
}

#[cfg(test)]
mod rate_tests {
    use super::*;

    #[test]
    fn a_countable_rate_is_carried_by_that_rate_over_the_payload_fraction() {
        // 8.4 Mbit of countable bytes at 0.84 countable per token billed
        // needs 10 Mbit of tokens.
        assert_eq!(countable_to_token_bps(8_400_000, 0.84), 10_000_000);
        // No overhead measured yet: the rate passes through.
        assert_eq!(countable_to_token_bps(8_400_000, 1.0), 8_400_000);
    }

    #[test]
    fn the_payload_fraction_is_clamped_to_what_is_possible() {
        // Above 1 would bill less than the payload; below 0.5 is a window of
        // control traffic, not the sender's overhead.
        assert_eq!(countable_to_token_bps(1_000_000, 1.7), 1_000_000);
        assert_eq!(countable_to_token_bps(1_000_000, 0.1), 2_000_000);
    }
}
