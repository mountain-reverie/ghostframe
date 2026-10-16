//! Per-tick send budgeting and the emission tail that spends it.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - the budget derivation: `base_budget_bytes` (`adaptation_context.bytes_per_us`
//!   x `SCHEDULER_TICK_INTERVAL_US` x `SCHEDULER_TICK_BUDGET_FRACTION`, bounded
//!   below by `tick_budget_floor` / `tick_budget_floor_bytes`),
//!   `paced_tick_budget_bytes`, and the goog_cc combination
//!   (`PacingMode`, `select_pacing_mode`, `combine_pacing_budget`,
//!   `pacer_tick_budget_bytes`, `BWE_PACED_MODE_SAMPLE_THRESHOLD`);
//! - the quinn send-buffer clamp (`min_session_send_buffer_space`,
//!   `clamp_to_quinn_capacity`, `QUINN_SEND_BUFFER_SAFETY_FRACTION`);
//! - the emission tail `emit_via_scheduler` and the budget carry-over it leaves
//!   behind (`scheduler_continuation`, `scheduler_continuation_after_drain`,
//!   `resume_scheduler_continuation`);
//! - the retransmit sweep and its budget share (`sweep_rto_retransmits`,
//!   `retransmit_pacer`, `RTO_RETRANSMITS_PER_TICK`, `STUCK_ENTRY_AGE`,
//!   `STUCK_SWEEP_EVERY_TICKS`, `RETRANSMIT_BUDGET_SHARE`);
//! - `priority_retry_after` and the harness setter `set_tick_budget_floor_bytes`;
//! - the scheduler tick constants, including the AIMD constants
//!   (`SCHEDULER_BUDGET_BACKOFF`, `_RAMP`, `_MULT_MIN`, `_MULT_MAX`) whose
//!   only use is the AIMD block inside `drain_scheduler_into_quinn`.
//!
//! Not here: `drain_scheduler_into_quinn` (and the AIMD multiplier update in
//! it), `dispatch_dirty_tiles_via_scheduler` and `apply_injected_frame`, which
//! take the AIMD-scaled budget from `base_budget_bytes` and hand it to
//! `emit_via_scheduler`; all of these stay in `io_bridge.rs`.
//!
//! Boundary with `bwe_glue.rs`: this file consumes the estimator (the
//! `bwe.snapshot()` samples count and `pacer_rate_bps`) and the probe window
//! (`active_probe.target_rate_bps` replaces the budget while a window is open)
//! but does not feed the estimator or manage the window. `emit_via_scheduler`
//! straddles the two (it reads the snapshot, applies the probe override and
//! calls `open_probe_window_for_burst`) and is left here whole.
//!
//! Must not: decode ACKs or feedback, update the loss signal or the capacity
//! hint, or decide session lifetime.

use super::*;

// ---------------------------------------------------------------------------
// Scheduler tick budgeting (A + B-light)
// ---------------------------------------------------------------------------
// `dispatch_dirty_tiles_via_scheduler` used to call `scheduler.tick(usize::MAX)`,
// asking the scheduler to drain *everything* and handing the entire dirty
// burst to QUIC in one go. With a first-frame cdf53 emission that runs
// ~14 MB (2040 tiles × 14 progressive passes × ~500 B mean), this
// catastrophically overran the datagram_send_buffer, with quinn either
// silently overwriting older queued datagrams (drop=true, pre-81e4954) or
// returning Blocked errors that dropped tiles on the floor (drop=false,
// post-81e4954).
//
// The scheduler already accepts a per-tick byte budget and explicitly
// keeps un-drained work in its queues for the next tick (see
// `drain_priority_queue` / `drain_refinement_pass_major`). We just have
// to use it. The budget tracks the measured QUIC bandwidth — feed-forward
// (A) — and modulates down on observed send errors — feedback (B-light).
//
// Frame interval: hard-coded for 30 FPS capture (the daemon default).
// At higher capture rates we'd budget conservatively (cap each tick to
// the 30-FPS slice), which is fine — un-drained work carries across.
pub(crate) const SCHEDULER_TICK_INTERVAL_US: f64 = 33_333.0;

// Leave 10 % headroom in the budget so we don't push right up to quinn's
// drain rate every tick (ACKs, feedback, NACK retransmits still need
// wire). Cheap enough.
const SCHEDULER_TICK_BUDGET_FRACTION: f64 = 0.90;

// Safety floor for the first frames before `adaptation_context.bytes_per_us`
// has been populated from path stats. 256 KB × 30 FPS = 7.5 MB/s — well
// under any realistic tailnet path so we won't hammer a slow link, but
// big enough that the visible-content first frame still lands quickly.
/// Budget floor, in datagrams rather than bytes.
///
/// This was a flat 256 KiB, which is 7.86 MB/s at a 33.3 ms tick. With
/// `SCHEDULER_TICK_BUDGET_FRACTION` that meant the bandwidth-derived term in
/// `base_budget_bytes` only overtook the floor above ~70 Mbit/s -- above every
/// link this system serves. The budget was therefore a constant, and slow
/// links were overdriven by up to ~12x. Measured cost of that on a saturating
/// 60x34 scene: 511,473 retransmit attempts and an emitter queue 389,053 deep
/// at 5 Mbit, and 1,213,785 retransmits on a 5 -> 50 Mbit step-up.
///
/// The floor's only legitimate job is to keep the budget above a single
/// datagram, so a tile can always make progress even when the estimate is
/// pessimistic or not yet established. Everything above that belongs to the
/// bandwidth term. See `docs/specs/blocked-path-redesign.md`.
const SCHEDULER_TICK_BUDGET_FLOOR_DATAGRAMS: usize = 1;

/// Datagram size assumed when no session is connected yet, so the floor is
/// defined before `compute_max_datagram_size` has a live connection to read.
const SCHEDULER_TICK_BUDGET_FLOOR_FALLBACK_BYTES: usize = 1_200;

// AIMD constants. Halving on error is conservative; 10 % ramp-up
// per clean frame recovers a 16× drop in ~30 frames (1 second at 30 FPS).
// Floor at 5 % so a sustained-loss link can't pin us to "send nothing".
pub(super) const SCHEDULER_BUDGET_BACKOFF: f64 = 0.5;

pub(super) const SCHEDULER_BUDGET_RAMP: f64 = 1.1;

pub(super) const SCHEDULER_BUDGET_MULT_MIN: f64 = 0.05;

pub(super) const SCHEDULER_BUDGET_MULT_MAX: f64 = 1.0;

// Fraction of quinn's reported `send_buffer_space()` we're willing to
// commit per tick. The remaining 20 % is headroom for per-fragment
// wire overhead (WT quarter-stream-id VarInt + tile-fragment header,
// 30–60 B per datagram) that the scheduler's payload-bytes budget
// can't see, so the actual on-wire footprint always exceeds the
// scheduler's accounting. Without this margin a tick that just fits
// in the payload budget still risks `SendDatagramError::Blocked` on
// the overhead bytes — and Blocked silently drops refinement work
// the scheduler already popped from its queue.
pub(super) const QUINN_SEND_BUFFER_SAFETY_FRACTION: f64 = 0.80;

// Maximum number of RTO-driven retransmits the ReliableTileEmitter is
// allowed to fire per `tick()` call. Sized to comfortably fit in quinn's
// `datagram_send_buffer` so a tick never overruns it. When more entries
// are due than this cap, the un-popped ones stay in the RTO heap and
// are picked up either by the next per-frame tick or by the
// `Event::DatagramsUnblocked` continuation — quinn signals available
// buffer space, we pop the next batch. Without this cap, sustained
// loss fills the cache with thousands of pending passes; one `tick()`
// floods quinn with retransmits, most are rejected with
// `SendDatagramError::Blocked`, the cache never drains, and the link
// settles into a self-sustained storm (~1.4 M retransmits / 3 minutes
// observed in production with no cap).
//
// Bounds *retransmissions*, not datagrams. Since every allocated `wire_seq`
// now joins an FEC group, a full sweep of 64 contiguous retransmits completes
// floor(64/10) = 6 groups, each scheduling a parity that the same drain may
// promote. So a sweep can hand quinn up to ~70 datagrams, not 64. That
// headroom is deliberate: the cap is a safety valve against a self-sustained
// storm, not a precise shaper, and tightening it to a datagram count would
// change what `max_retransmits` means without changing its name.
const RTO_RETRANSMITS_PER_TICK: usize = 64;

/// How long a cache entry may keep retransmitting before it is retired and its
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

/// Share of the link estimate retransmission may use while the scheduler
/// still has work queued. With nothing queued it may use all of it.
///
/// Half, because neither side can be shown to deserve more. A retransmission
/// repairs a pass the client is known or suspected to lack, and until it
/// lands the tile cannot finish; new work is what the user is waiting to
/// see, and under queueing delay most "lost" passes are merely late, so the
/// retransmission is waste. Starving either one stalls convergence -- the
/// first by never completing tiles, the second, as measured before this
/// existed, by never emitting them.
///
/// Not a tuned value. What matters is that the sum is bounded by the
/// estimate at all; see `retransmit_pacer`.
const RETRANSMIT_BUDGET_SHARE: f64 = 0.5;

/// Ticks between stuck-entry sweeps. The sweep scans the whole cache, which is
/// large exactly when it matters, so it does not belong on every tick.
const STUCK_SWEEP_EVERY_TICKS: u32 = 30;

// ── BWE Stage 2.2: PacingMode ────────────────────────────────────────────
//
// The estimate takes charge of the per-tick emission budget. Two call
// sites derive a starting budget before handing it to
// `clamp_to_quinn_capacity` — `dispatch_dirty_tiles_via_scheduler`'s
// AIMD-scaled `aimd_budget` and `apply_injected_frame`'s unscaled
// `base_budget_bytes()` — and both route through `combine_pacing_budget` so
// the two don't independently re-derive the same "should goog_cc bind
// here?" decision and drift apart.

/// Minimum `BweSnapshot::samples_seen` before `PacingMode::Paced` is
/// trusted over `PacingMode::PathStats`. Below this, `GoogCcDriver` is
/// still reporting values seeded at construction (`BweWrapper::INITIAL_BPS`
/// for `bitrate_bps`; no `pacer_config` at all yet for `pacer_rate_bps`) —
/// indistinguishable from a converged estimate without this check. Stage 1
/// added `samples_seen` for exactly this reason.
///
/// EMPIRICAL GUESS, not yet validated against the bench (flagged per this
/// project's precedent — see the M3.6/M3.7 constant-tuning work — for
/// retuning once real data exists): the browserless baseline scene
/// (`docs/specs/bwe-tier-latency-baseline.md`) produces roughly 500 ACK
/// samples over its 10 s duration, ~50/s pooled across 30 runs. 150 samples
/// is ~3 s of steady ACK arrivals at that rate — long enough to clear
/// goog_cc's own delay-based estimator warm-up (the trendline/inter-arrival
/// machinery wants on the order of a few dozen packet groups before its
/// slope estimate means anything) while still switching to `Paced` well
/// before a typical session's midpoint rather than at the very end of it.
///
/// Deliberately not `pub`: it would otherwise leak into `ghostframe.h`
/// (cbindgen scrapes every `pub const` in this crate for the C FFI surface)
/// alongside genuinely wire-relevant constants like `FEC_GROUP_SIZE_K` —
/// this is an internal tuning knob, matching every other AIMD/budget
/// constant in this file (`SCHEDULER_BUDGET_BACKOFF` and friends, none of
/// which are `pub` either).
pub(super) const BWE_PACED_MODE_SAMPLE_THRESHOLD: u64 = 150;

/// Which source governs the per-tick emission budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacingMode {
    /// Quinn path stats with AIMD — today's behaviour. The startup mode,
    /// and the fallback while the estimator has too few samples to trust
    /// (see `BWE_PACED_MODE_SAMPLE_THRESHOLD`).
    PathStats,
    /// goog_cc's `pacer_config` additionally governs the budget — see
    /// `combine_pacing_budget`. Does not replace the `PathStats`
    /// computation; the two are combined by taking the minimum.
    Paced,
}

/// Select `PacingMode` from the estimator's `samples_seen` counter.
/// `samples_seen` only grows for the life of a session, so in practice this
/// is a one-way switch (`PathStats` -> `Paced`), never back — matching the
/// design's framing of `PathStats` as "the startup mode."
pub fn select_pacing_mode(samples_seen: u64) -> PacingMode {
    if samples_seen >= BWE_PACED_MODE_SAMPLE_THRESHOLD {
        PacingMode::Paced
    } else {
        PacingMode::PathStats
    }
}

/// Bytes a `pacer_rate_bps` rate (bits per second, from
/// `BweSnapshot::pacer_rate_bps`) affords over one tick of `tick_interval_us`
/// microseconds. Pure arithmetic, factored out of `combine_pacing_budget` so
/// it's independently checkable and so `bwe_bench.rs` can pin the unit
/// conversion directly.
pub fn pacer_tick_budget_bytes(pacer_rate_bps: u64, tick_interval_us: f64) -> usize {
    (((pacer_rate_bps as f64) / 8.0) * tick_interval_us / 1_000_000.0) as usize
}

/// Combine the quinn-path-stats/AIMD-derived budget with goog_cc's
/// pacer-derived budget for the same tick, by taking the smaller —
/// `min`, not a replacement. They constrain different things: quinn's send
/// buffer is a hard local limit whose violation drops popped work; goog_cc's
/// pacer is a network estimate that can simply be wrong. Taking the minimum
/// means a wrong estimate costs throughput rather than dropping tiles.
///
/// `PacingMode::PathStats`, or `Paced` before the estimator has ever
/// produced a `pacer_config` (`pacer_rate_bps: None`), returns
/// `aimd_budget_bytes` unchanged — there is nothing from goog_cc to combine
/// with yet.
///
/// Callers must still pass the result through `clamp_to_quinn_capacity`
/// afterwards, last — this function has no knowledge of quinn's live
/// send-buffer state, and that clamp exists to prevent the scheduler
/// over-popping into quinn regardless of which mode produced the budget.
pub fn combine_pacing_budget(
    mode: PacingMode,
    aimd_budget_bytes: usize,
    pacer_rate_bps: Option<u64>,
    tick_interval_us: f64,
) -> usize {
    match (mode, pacer_rate_bps) {
        (PacingMode::Paced, Some(bps)) => {
            aimd_budget_bytes.min(pacer_tick_budget_bytes(bps, tick_interval_us))
        }
        _ => aimd_budget_bytes,
    }
}

impl IoBridge {
    /// Minimum `send_buffer_space()` across every currently-connected
    /// WebTransport session, or `usize::MAX` if no session is connected.
    ///
    /// Every datagram we emit goes to **every** connected client, so
    /// the slowest queue determines how much we can drain without
    /// `quinn-proto` returning `SendDatagramError::Blocked` —
    /// at which point the work has already been popped from
    /// `scheduler.refinement_queue` and is irrecoverably lost.
    ///
    /// The no-connected-session fallback is `usize::MAX` (uncapped)
    /// rather than `0`. Production never reaches this code with zero
    /// connected sessions — `process_frame_gpu` early-returns when
    /// `compute_max_datagram_size` returns `None`. Unit tests that
    /// drive `dispatch_dirty_tiles_via_scheduler` directly without a
    /// live quinn connection rely on the unlimited path so the
    /// scheduler's `tick` still runs.
    ///
    /// Called from `dispatch_dirty_tiles_via_scheduler` to clamp the
    /// per-tick AIMD budget down to what quinn can actually accept.
    pub(super) fn min_session_send_buffer_space(&mut self) -> usize {
        let mut min_space: Option<usize> = None;
        let handles: Vec<quinn_proto::ConnectionHandle> =
            self.live_send_handle().into_iter().collect();
        for handle in handles {
            if let Some(conn) = self.server.connections.get_mut(&handle) {
                let space = conn.datagrams().send_buffer_space();
                min_space = Some(match min_space {
                    Some(m) => m.min(space),
                    None => space,
                });
            }
        }
        min_space.unwrap_or(usize::MAX)
    }

    /// Clamp a drain budget to what quinn can actually absorb right now:
    /// `min_session_send_buffer_space()` scaled by
    /// `QUINN_SEND_BUFFER_SAFETY_FRACTION`, then `.min()`-ed against the
    /// caller-supplied budget. This one computation backs all three of the
    /// module's scheduler-drain budget sites — `dispatch_dirty_tiles_via_scheduler`'s
    /// AIMD-derived `tick_budget_bytes` (see the long comment on that
    /// variable, and the module-level history at `:145-152`),
    /// `resume_scheduler_continuation`'s per-continuation `effective_budget`,
    /// and `apply_injected_frame`'s injected-scene budget — because a
    /// `scheduler.tick(usize::MAX)` drain against a large first-frame CDF53
    /// burst once catastrophically overran `datagram_send_buffer`, with
    /// quinn either silently overwriting queued datagrams or returning
    /// `Blocked` and dropping the already-popped `TileWork` on the floor.
    /// `drain_refinement_pass_major` uses `queue.remove(idx)` (not
    /// `InFlight` retention), so an over-popped refinement pass has no
    /// scheduler-side retry — only the emitter's bounded RTO.
    ///
    /// `apply_injected_frame`'s only caller (the browserless harness) passes
    /// `budget: usize::MAX` — wave 1 relies entirely on the netsim's token
    /// bucket for pacing, and that bucket is consumed inside
    /// `send_to_all_sessions`, i.e. *below* quinn's `datagram_send_buffer`.
    /// It shapes what reaches the socket but cannot stop the scheduler from
    /// over-popping into quinn. Without this clamp, the injected path would
    /// reproduce the `:145-152` bug on every drain.
    ///
    /// Callers remain responsible for anything beyond the raw quinn-capacity
    /// clamp: `dispatch_dirty_tiles_via_scheduler` applies
    /// `tick_budget_multiplier` (AIMD) to its budget *before* calling this,
    /// and `apply_injected_frame` deliberately does not — that feedback loop
    /// (which reacts to *this bridge's own* `send_datagram` errors) is not
    /// exercised on the injected path. BWE Stage 2.2's `combine_pacing_budget`
    /// (goog_cc's pacer, an independent network estimate) is a different
    /// axis and both sites *do* apply it before calling this, so that the
    /// browserless harness — which only ever exercises the injected path —
    /// can actually observe `PacingMode::Paced`.
    ///
    /// A plain method (rather than inlined at each site) so unit tests can
    /// pin this arithmetic directly: `apply_injected_frame`'s own call site
    /// sits behind `compute_max_datagram_size()`, which requires a live,
    /// post-handshake `quinn_proto::Connection` to ever return `Some` —
    /// exactly the machinery the browserless harness (Task 15) exists to
    /// provide, and out of scope to fabricate here (see the doc comment on
    /// `injected_work_is_enqueued_and_drain_is_attempted` below for the same
    /// call).
    pub(super) fn clamp_to_quinn_capacity(&mut self, budget: usize) -> usize {
        let quinn_space = self.min_session_send_buffer_space();
        let quinn_cap = ((quinn_space as f64) * QUINN_SEND_BUFFER_SAFETY_FRACTION) as usize;
        budget.min(quinn_cap)
    }

    /// Fire any cached retransmits whose RTO deadline has elapsed, then
    /// drain the resulting emission queue (source + parity) to the wire.
    /// `tick()` is rate-limited to `RTO_RETRANSMITS_PER_TICK` entries per
    /// call so a burst of due retransmits can't re-Block quinn's just-freed
    /// buffer; `drain()` flushes whatever that produced. Cheap when nothing
    /// is due (no cache hits, no parity ready), so it is safe to call
    /// unconditionally from every place a drain just happened or quinn
    /// signaled room: the post-dispatch path in `process_frame_gpu`, the
    /// `Event::DatagramsUnblocked` handler, and `apply_injected_frame`'s
    /// post-drain mirror of the same behavior for injected scenes.
    ///
    /// The drain goes through [`IoBridge::drain_reliable_emitter`], which holds
    /// no borrow of the emitter across a send. (It used to hand the emitter a
    /// raw `*mut IoBridge`; see the commit that removed it.)
    pub(super) fn sweep_rto_retransmits(&mut self) {
        let tick_now = now_std();
        self.stuck_sweep_counter = self.stuck_sweep_counter.wrapping_add(1);
        if self
            .stuck_sweep_counter
            .is_multiple_of(STUCK_SWEEP_EVERY_TICKS)
        {
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
        // Retransmission draws on the same link estimate the scheduler
        // does. This is called per frame and per `DatagramsUnblocked`, so a
        // per-call bound alone is a bound per *event*; the pacer turns it
        // into one per unit of time.
        let share = if self.scheduler.queue_len() == 0 && self.scheduler.refinement_queue_len() == 0
        {
            1.0
        } else {
            RETRANSMIT_BUDGET_SHARE
        };
        self.retransmit_pacer.refill(
            tick_now,
            self.paced_tick_budget_bytes(),
            SCHEDULER_TICK_INTERVAL_US,
            share,
        );
        // And on quinn's room, for the scheduler's reason: a datagram quinn
        // refuses is re-queued, not lost, but queueing replay behind a full
        // buffer only adds to the standing delay that is making timers fire.
        let allowance = self.clamp_to_quinn_capacity(self.retransmit_pacer.available());
        let spent =
            self.reliable_emitter
                .tick_budgeted(tick_now, RTO_RETRANSMITS_PER_TICK, allowance);
        self.retransmit_pacer.spend(spent);
        self.drain_reliable_emitter(tick_now);
    }

    /// Build the continuation a scheduler drain should leave behind, given
    /// the (already-clamped) budget it drained against and how many bytes
    /// actually went out. `None` once the budget is fully spent, otherwise
    /// `Some` carrying the frame's own identity (`seq`/`timestamp_us`/
    /// `max_frag`) and the leftover budget, so
    /// `Event::DatagramsUnblocked` -> `resume_scheduler_continuation` can
    /// top the drain back up without waiting for the next capture tick or
    /// injected frame.
    ///
    /// Used by `emit_via_scheduler`, shared by both the capture and
    /// injected-harness paths.
    ///
    /// A free function (no `&self`) so it is directly unit-testable: unlike
    /// its callers (behind `compute_max_datagram_size()`, which needs a
    /// live post-handshake session), this is pure arithmetic over its
    /// arguments.
    pub(super) fn scheduler_continuation_after_drain(
        seq: u32,
        timestamp_us: u32,
        max_frag: usize,
        budget_bytes: usize,
        drained_bytes: usize,
    ) -> Option<SchedulerContinuation> {
        let remaining = budget_bytes.saturating_sub(drained_bytes);
        if remaining > 0 {
            Some(SchedulerContinuation {
                seq,
                timestamp_us,
                max_frag,
                remaining_budget_bytes: remaining,
            })
        } else {
            None
        }
    }

    /// Lower bound for `base_budget_bytes`: one datagram at the live MTU,
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

    /// Per-tick emission budget before any pacing override or AIMD
    /// modulation: measured QUIC bandwidth (`adaptation_context.bytes_per_us`)
    /// × the capture tick interval × a fixed headroom fraction, floored by
    /// `tick_budget_floor` so the first frames of a session (before
    /// `bytes_per_us` is populated from path stats) still emit promptly.
    ///
    /// Shared by `dispatch_dirty_tiles_via_scheduler` (which further scales
    /// this by `tick_budget_multiplier`, the AIMD ramp/backoff fed by
    /// observed `send_datagram` errors) and `apply_injected_frame` (which
    /// does not — see that call site's comment for why). Both callers must
    /// derive from the same production numbers rather than each computing
    /// their own idea of "the budget", which is exactly the split that
    /// generated Stage 2's defects.
    pub(super) fn base_budget_bytes(&self) -> usize {
        // Billing a fixed 33 ms slice per call looks wrong -- ticks are
        // frame-driven, so at 5 fps this grants 5 x 33 ms of bandwidth per
        // second of wall clock. Replacing it with elapsed-time billing was
        // implemented and measured, and changed nothing: the floor
        // (`tick_budget_floor`, then a flat 256 KB) dominated at every
        // realistic interval (33 ms computes ~30 KB, 250 ms ~225 KB), so this
        // expression's first term never decided the budget. (The floor is
        // now one datagram at the live MTU, so this term can bind.) Reverted
        // rather than kept, because unvalidated arithmetic in the
        // send path is a liability.
        (((self.adaptation_context.bytes_per_us as f64)
            * SCHEDULER_TICK_INTERVAL_US
            * SCHEDULER_TICK_BUDGET_FRACTION) as usize)
            .max(self.tick_budget_floor())
    }

    /// How long the scheduler waits for a priority tile's acknowledgement
    /// before sending it again: the reliable emitter's first-attempt
    /// deadline, so the two retry mechanisms race the same clock.
    ///
    /// Not a function of QUIC RTT. See `Scheduler::set_retry_after`.
    pub(super) fn priority_retry_after(&self) -> std::time::Duration {
        crate::transport::reliable_emitter::rto::rto_for_attempt(
            self.reliable_emitter.ack_deadline,
            0,
        )
    }

    /// The per-tick emission budget as the scheduler computes it outside a
    /// probe window: `base_budget_bytes` scaled by the AIMD multiplier, then
    /// bounded by goog_cc's pacer once that is trusted.
    ///
    /// For callers that are not a frame dispatch -- retransmission -- and so
    /// have no `aimd_budget` in hand, but must draw on the same estimate.
    /// Probe windows are deliberately not consulted: a probe exists to send
    /// *new* content above the estimate, and replaying old passes into it
    /// would spend the measurement on bytes the client may already hold.
    fn paced_tick_budget_bytes(&self) -> usize {
        let aimd_budget =
            ((self.base_budget_bytes() as f64) * self.tick_budget_multiplier) as usize;
        let bwe_snap = self.bwe.snapshot();
        combine_pacing_budget(
            select_pacing_mode(bwe_snap.samples_seen),
            aimd_budget,
            bwe_snap.pacer_rate_bps,
            SCHEDULER_TICK_INTERVAL_US,
        )
    }

    /// The one emission tail: probe override -> pacing combine -> quinn
    /// clamp -> drain -> continuation bookkeeping.
    ///
    /// Both the capture path (`dispatch_dirty_tiles_via_scheduler`) and the
    /// harness injection path (`apply_injected_frame`) call this, so they
    /// cannot drift — four BWE Stage 2 defects came from feeding what were
    /// once two independent copies of this logic different inputs. Anything
    /// that must differ between the two callers belongs *before* this call,
    /// in how their work is produced and enqueued, never in how it is
    /// emitted.
    ///
    /// Takes `base_budget_bytes` and nothing that identifies the caller. If
    /// this function ever needs to know who called it, the extraction has
    /// failed and the split has been rebuilt one level down.
    ///
    /// `SchedulerEmissionPolicy::CpuRawOnly` does not call this at all — it
    /// bypasses probe override, pacing combine, the quinn clamp, AND
    /// continuation bookkeeping outright with a direct `usize::MAX` drain
    /// (see the `match policy` in `dispatch_dirty_tiles_via_scheduler`).
    /// That is a bigger bypass than any budget value this function could be
    /// handed would produce, so it stays a caller-side branch rather than a
    /// parameter here.
    ///
    /// The AIMD `tick_budget_multiplier` is likewise applied by the caller,
    /// before this is called: it is fed by `send_datagram` errors observed
    /// on the dispatch path only, and the injected path has no equivalent
    /// feedback signal.
    pub(super) fn emit_via_scheduler(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        max_frag: usize,
        base_budget_bytes: usize,
    ) -> (FrameSendStats, usize, usize) {
        // BWE Stage 2.2: goog_cc's pacer additionally bounds the budget
        // once the estimator has enough samples to trust (`PacingMode`) —
        // `min(base_budget_bytes, googcc_budget)`, not a replacement.
        // BWE burst discovery: claim a pending probe window *here*, at the
        // one moment we know datagrams are about to be emitted. See
        // `open_probe_window_for_burst`.
        self.open_probe_window_for_burst(now_std());

        let bwe_snap = self.bwe.snapshot();
        let pacing_mode = select_pacing_mode(bwe_snap.samples_seen);
        // BWE Stage 2.4: a probe window overrides the pacing budget
        // outright rather than going through `combine_pacing_budget` —
        // probing exists to send *above* goog_cc's own current estimate,
        // so `min(base, googcc)` would defeat the point. `clamp_to_quinn_
        // capacity` still runs last (below) and still wins: a probe is not
        // worth dropping tiles for, and that clamp exists because
        // `scheduler.tick` is destructive.
        let pre_clamp_budget = match &self.active_probe {
            Some(probe) => {
                // A probe must send at *least* its target rate, but must
                // never send *less* than we otherwise would. Overriding the
                // pacing budget outright throttles emission whenever the
                // probe target sits below the current pacer rate — the pacer
                // runs at 2.5x the estimate while an early ladder rung is
                // only 2x, so that window is real. Measured on the
                // propagation-delay scene: delivered bytes fell 23% (747 kB
                // -> 577 kB) once network-state probing made windows
                // frequent enough for it to matter.
                //
                // Taking the max keeps the cluster's floor at the target
                // while leaving throughput alone. goog_cc computes a
                // cluster's send rate from what was actually sent, not from
                // what was requested, so exceeding the target costs nothing.
                let probe_budget =
                    pacer_tick_budget_bytes(probe.target_rate_bps, SCHEDULER_TICK_INTERVAL_US);
                let paced_budget = combine_pacing_budget(
                    pacing_mode,
                    base_budget_bytes,
                    bwe_snap.pacer_rate_bps,
                    SCHEDULER_TICK_INTERVAL_US,
                );
                probe_budget.max(paced_budget)
            }
            None => combine_pacing_budget(
                pacing_mode,
                base_budget_bytes,
                bwe_snap.pacer_rate_bps,
                SCHEDULER_TICK_INTERVAL_US,
            ),
        };
        // The scheduler re-sends an un-acknowledged priority tile once this
        // has elapsed. Set here, on the path every emission shares, so the
        // capture and injected paths cannot disagree about it.
        self.scheduler.set_retry_after(self.priority_retry_after());
        // Retransmission since the last tick came out of the same link, so
        // it comes out of this budget: the estimate bounds the sum, not each
        // sender separately. Bounded by retransmission's own share so that a
        // capture slower than the tick rate -- where several ticks' worth of
        // replay legitimately fits between two frames -- cannot zero the
        // scheduler out.
        let replayed = self.retransmit_pacer.take_spent_since_tick();
        let give_back = replayed.min((pre_clamp_budget as f64 * RETRANSMIT_BUDGET_SHARE) as usize);
        let pre_clamp_budget = pre_clamp_budget - give_back;
        // Never pop more from the scheduler than quinn can actually absorb
        // right now — see `clamp_to_quinn_capacity`'s doc comment for why
        // this is mandatory rather than defensive.
        let budget_bytes = self.clamp_to_quinn_capacity(pre_clamp_budget);
        let (stats, drained_bytes, drained_count) =
            self.drain_scheduler_into_quinn(seq, timestamp_us, max_frag, budget_bytes);
        // Cache the continuation context so `Event::DatagramsUnblocked` ->
        // `resume_scheduler_continuation` can resume draining the same
        // logical frame without waiting for the next capture tick / next
        // injected frame.
        self.scheduler_continuation = Self::scheduler_continuation_after_drain(
            seq,
            timestamp_us,
            max_frag,
            budget_bytes,
            drained_bytes,
        );
        (stats, drained_bytes, drained_count)
    }

    /// Resume the in-flight frame's scheduler drain. Called from the
    /// `Event::DatagramsUnblocked` event handler — quinn just transmitted
    /// the datagrams it had buffered and there's room for more. Pulls
    /// from the scheduler queues up to whatever's left of the current
    /// frame's AIMD-adjusted budget, fragments and sends, then either
    /// keeps the continuation alive with the new remaining budget or
    /// clears it once the per-frame budget is fully spent.
    ///
    /// AIMD is deliberately NOT adjusted here. A burst of Unblocked
    /// events at session start (slow-start cwnd ramp) used to collapse
    /// the multiplier to its floor (0.05) within one frame because each
    /// continuation halved on its own observed errors. With AIMD pinned
    /// to the per-frame dispatch boundary, the multiplier reflects the
    /// frame's *cumulative* error rate (direct drain + every continuation
    /// triggered before the next frame) via the
    /// `pre_dispatch_send_errs → datagram_send_errs` delta — exactly
    /// once per frame instead of once per quinn event.
    ///
    /// Cheap when nothing is queued (scheduler.tick returns empty), so
    /// no harm in firing it on every Unblocked event regardless of
    /// what was just unblocked.
    pub(super) fn resume_scheduler_continuation(&mut self) {
        let ctx = match self.scheduler_continuation {
            Some(c) => c,
            None => return,
        };
        if ctx.remaining_budget_bytes == 0 {
            self.scheduler_continuation = None;
            return;
        }
        // Mirror the dispatch-side clamp: never pop more from the
        // scheduler than quinn can absorb (see the long comment in
        // `dispatch_dirty_tiles_via_scheduler` for the Blocked-drops-
        // refinement-work rationale).
        let effective_budget = self.clamp_to_quinn_capacity(ctx.remaining_budget_bytes);
        if effective_budget == 0 {
            // Quinn is still full. Keep the continuation alive so the
            // next `DatagramsUnblocked` event fires this path again.
            return;
        }
        let (_stats, drained_bytes, drained_count) = self.drain_scheduler_into_quinn(
            ctx.seq,
            ctx.timestamp_us,
            ctx.max_frag,
            effective_budget,
        );
        let new_remaining = ctx.remaining_budget_bytes.saturating_sub(drained_bytes);
        if drained_count > 0 {
            self.continuation_resumes = self.continuation_resumes.saturating_add(1);
            self.continuation_drained_bytes = self
                .continuation_drained_bytes
                .saturating_add(drained_bytes as u64);
        }
        tracing::debug!(
            target: LOG_TARGET,
            seq = ctx.seq,
            drained_count = drained_count,
            drained_bytes = drained_bytes,
            remaining_before = ctx.remaining_budget_bytes,
            remaining_after = new_remaining,
            "scheduler continuation resumed"
        );
        // Clear the continuation when the budget is exhausted. We also
        // clear when drained_count == 0 AND drained_bytes == 0 (the
        // scheduler genuinely had nothing more to drain); a subsequent
        // dispatch will repopulate with fresh state. Don't clear on
        // drained_count==0 alone — a refinement-queue item whose payload
        // exceeds the remaining budget makes scheduler.tick return
        // empty even though work remains, and clearing would lose the
        // continuation hook for that next Unblocked event.
        self.scheduler_continuation = if new_remaining > 0 && drained_bytes > 0 {
            Some(SchedulerContinuation {
                remaining_budget_bytes: new_remaining,
                ..ctx
            })
        } else {
            None
        };
    }

    /// Override the `base_budget_bytes` floor. See `tick_budget_floor_bytes`
    /// for why this is worth varying: the default sits above every link the
    /// system actually serves, so without lowering it a test cannot observe
    /// the bandwidth-derived budget binding at all.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_tick_budget_floor_bytes(&mut self, bytes: usize) {
        self.tick_budget_floor_bytes = Some(bytes);
    }
}
