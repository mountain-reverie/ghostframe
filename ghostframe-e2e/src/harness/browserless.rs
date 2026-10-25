//! Task 15a/15b: browserless netsim scene runner.
//!
//! Wires together a real `IoBridge` and a real `ghostframe_client_net::ClientNet`
//! across an in-memory pipe, with every datagram routed
//! through a [`crate::netsim::NetSim`] in each direction, under tokio's
//! virtual (paused) clock. This proves the WebTransport session can
//! establish over that whole path — the same wire framing, the same
//! `IoBridge` event loop, the same `ClientNet` sans-IO state machine that
//! production uses, just without tsnet or a browser.
//!
//! Once the session reaches `SessionReady`, `run_browserless` drains
//! `BrowserlessScene::frames` in order, encoding each `FrameScript`'s tiles
//! with `scene_tiles::encode_tile` and sending them to `IoBridge` as
//! `InjectedFrame`s over the injection channel `new_with_injection_for_test`
//! already wires up. The client core decodes tiles internally — the runner
//! never sees payloads or codecs on the receive side, only
//! `Event::TileReady`'s already-decoded RGBA, which is fed into
//! `FrameBuffer::apply_tile_ready` (see that module's docs for why this is
//! a separate ingest path from `FrameBuffer::apply`).
//!
//! ## Wiring
//!
//! ```text
//! ClientNet <-> BridgeLink <-> mem_stream_pair() <-> IoBridge <-> QuicServer
//!              (netsim in both directions)
//! ```
//!
//! ## Two modes
//!
//! Stepped (default) and concurrent (opt-in through
//! `RunOptions::mode`) see different failure classes; the doc comment on
//! [`HarnessMode`] lists what each can and cannot see. Everything below
//! describes the stepped mode unless it says otherwise.
//!
//! ## Reproducibility
//!
//! A scene is a pure function of its seed. `IoBridge` is not spawned: the
//! scene loop puts bytes in the pipe, calls `IoBridge::step_until_idle` (the
//! production `run()` body, `turn`, polled by hand until nothing is ready),
//! reads what came out, and only then lets the paused clock move, to the
//! earliest of the client's timer, the next datagram in flight, the next
//! injection and the bridge's own next deadline. No kernel socket, reactor,
//! blocking thread or task scheduler sits between the two halves, so nothing
//! outside the seed decides what happens next; `browserless_determinism`
//! holds that to exact equality.
//!
//! `IoBridge` owns the WebTransport layer itself
//! (`wt_sessions: HashMap<ConnectionHandle, WebTransportServer>`), so this
//! module does not construct a `WebTransportServer` — only `ClientNet` on
//! the far side.
//!
//! ## Clock
//!
//! There is a single virtual-time source: `tokio::time::Instant`. `IoBridge`
//! already derives its own `now_std()` from `tokio::time::Instant::now()`
//! (see `io_bridge::now_std_impl`), so driving `ClientNet` and the netsim
//! scheduling off the same clock keeps both sides of the socketpair on one
//! consistent timeline under `#[tokio::test(start_paused = true)]`.

use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tokio::time::Instant as TokioInstant;

use ghostframe_client_core::Event as CoreEvent;
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::ghostbridge::encode_frame;
use ghostframe_lib::transport::io_bridge::{mem_stream_pair, InjectedFrame, IoBridge, MemStream};
use ghostframe_lib::transport::quic::QuicServer;

use crate::harness::framebuffer::FrameBuffer;
use crate::harness::scene_tiles::{encode_tile, TileSpec};
use crate::netsim::pump::parse_frames;
use crate::netsim::{NetProfile, NetSim, SocketPairPump, Verdict};

/// Default spacing between injected frames.
///
/// This was 16 ms, documented as "a small, deterministic, non-zero gap"
/// that nothing downstream depended on. That turned out to be false.
/// Production dispatches every `SCHEDULER_TICK_INTERVAL_US` (33.3 ms), and
/// against a ~15 ms probe window the difference decides whether a window
/// contains an emission at all: the 8x8 CDF53 probe scene completes ten
/// clusters in ten seeds at 16 ms and **zero** at 33.3 ms, with or without
/// `drain_for_probe_window_open`. Every probe-completion figure this project
/// published before 2026-09-13 came from the faster regime. See
/// `docs/specs/bwe-probe-emission-timing.md`.
///
/// Re-exported rather than redeclared so a scripted scene and a generated
/// `LoadProfile` cannot drift to different defaults.
pub use crate::harness::load_profile::PRODUCTION_CADENCE_US as DEFAULT_CADENCE_US;

/// Spacing between post-injection heartbeat ticks (see `drive_session`'s
/// doc comment on why heartbeats exist at all). Deliberately much coarser
/// than `FRAME_SPACING_US`: the sole purpose of a heartbeat is to reach
/// `IoBridge::apply_injected_frame`'s unconditional admission pass
/// (`pump_admission`, which also fires due retransmissions) often enough that a cached, unacked fragment's RTO deadline (a
/// probe timeout, see `reliable_emitter::rto::pto_for_attempt`) gets checked well
/// before `scene.duration` runs out — not to reproduce a particular real
/// capture framerate. Ticking at `FRAME_SPACING_US` (16ms) would work too,
/// but multiplies the outer event loop's iteration count by
/// `scene.duration / FRAME_SPACING_US` for the entire remainder of every
/// scene once its declared frames run out, which measurably pushed
/// longer-duration scenes toward `drive_session`'s no-progress bail-out.
/// 100ms comfortably clears the RTO floor with room for multiple backoff
/// attempts, while keeping that iteration multiplier small.
const HEARTBEAT_SPACING_US: u64 = 100_000;

/// One scene frame's worth of declared tile content, keyed by `(tile_x,
/// tile_y)`.
pub struct FrameScript {
    pub tiles: Vec<((u8, u8), TileSpec)>,
}

/// Where a scene's frames come from.
pub enum SceneLoad {
    /// An explicit list, drained in order. Right for small deterministic
    /// scenes that assert on specific pixels.
    Script(Vec<FrameScript>),
    /// Generated across the scene's whole duration, so the scene never runs
    /// out of frames and never falls back to empty heartbeats. Right for
    /// anything measuring bandwidth, pacing or probing, where a scene that
    /// goes quiet after a short burst is not modelling production at all.
    ///
    /// The heartbeat path below exists so the admission pass keeps being
    /// run once a script is exhausted. A profile generates
    /// `duration / cadence_us` frames while injection only begins at
    /// `SessionReady`, so its last frame is scheduled past the scene's own
    /// deadline and `next_frame_idx < frames.len()` holds throughout —
    /// heartbeats never fire here. The path stays for scripted scenes,
    /// which very much do still need it.
    Profile(crate::harness::load_profile::LoadProfile),
}

/// A netsim scene to run browserlessly: a real `IoBridge` <-> `ClientNet`
/// session, no browser, no tsnet, driven under tokio's virtual clock.
pub struct BrowserlessScene {
    pub seed: u64,
    /// Override quinn's datagram send-buffer size for this scene.
    ///
    /// `None` keeps the production default (16 MiB). A small value is how a
    /// scene provokes `SendDatagramError::Blocked`, which is the only thing
    /// that makes quinn emit `DatagramsUnblocked` and the only condition
    /// under which `send_to_all_sessions`'s drop-on-error path runs.
    ///
    /// Passed to `QuicServer::new_with_datagram_send_buffer` rather than set
    /// via `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES`: that env var is
    /// process-global and scenes run in-process, so it would race between
    /// concurrent scenes instead of configuring one.
    pub datagram_send_buffer_bytes: Option<usize>,

    /// Override `IoBridge`'s `base_budget_bytes` floor, in bytes per
    /// 33.3 ms tick. `None` keeps the production default (256 KiB), which
    /// is equivalent to 7.86 MB/s and therefore dominates the
    /// bandwidth-derived term on any link below ~70 Mbit/s. Lower it to let
    /// the bandwidth term actually bind.
    pub tick_budget_floor_bytes: Option<usize>,
    /// Resolved once at the top of `drive_session`, then drained in order
    /// after `SessionReady`, one `FrameScript` every `FRAME_SPACING_US` of
    /// virtual time. See the module docs.
    pub load: SceneLoad,
    /// Interval between injected frames. Use [`DEFAULT_CADENCE_US`] unless
    /// the scene specifically needs another cadence; a `SceneLoad::Profile`
    /// carries its own and overrides this.
    pub cadence_us: u64,
    pub net: NetProfile,
    /// Deterministic drops, enforced inside the server before QUIC
    /// encryption — `IoBridge::send_to_all_sessions`, beside `outbound_loss`.
    ///
    /// This does **not** compose with `net`: the datagram is discarded before
    /// it reaches the simulated link, so `net`'s loss, cap and delay never see
    /// it and `BrowserlessResult::bytes_dropped` never counts it. Assert on
    /// `BrowserlessResult::drops_fired` instead.
    ///
    /// It has to be enforced there because tile identity is invisible once
    /// QUIC has encrypted the datagram — see
    /// `ghostframe_lib::transport::drop_plan`'s module doc for the
    /// measurement that established this. Empty by default, so existing
    /// scenes are bit-identical.
    pub drops: crate::netsim::DropPlan,
    pub duration: Duration,
    /// Grid dimensions, fixed for the whole scene.
    ///
    /// `Scheduler::resize` (`scheduler.rs:122-129`) clears `priority_queue`,
    /// `refinement_queue` AND `cdf53_passes_acked`, so growing the grid
    /// mid-scene would silently discard in-flight work and present as
    /// "tiles never converged" with no visible cause. Keeping this fixed
    /// for the whole scene sidesteps that entirely.
    pub grid_cols: u32,
    pub grid_rows: u32,
}

/// Optional measurement knobs for [`run_browserless_with_options`]. The
/// default changes nothing, so every existing scene is bit-identical.
#[derive(Clone, Copy, Default)]
pub struct RunOptions {
    /// Hold the server's event loop this long (virtual time) for every real
    /// scene frame, inline, before it is applied. Models `process_frame`
    /// running on the loop: see `InjectedFrame::cost`.
    ///
    /// The hold is applied in `IoBridge::run`'s injected arm, not inside
    /// `process_frame`: a later change that moves `process_frame` off the loop
    /// does not move this sleep by itself, so a test that merely stops
    /// applying it proves nothing about production.
    ///
    /// Keep it well below the scene's cadence. With the hold longer than the
    /// frame interval the bounded inject channel (8) fills, the driver blocks
    /// in `inject_tx.send(..).await` outside any select, the bridge blocks
    /// writing server -> client datagrams nobody is reading, and nothing is
    /// left to advance the paused clock. `inject_frame` now bounds that send
    /// so it fails instead of hanging.
    pub frame_cost: Option<Duration>,
    /// How the server's event loop is driven. See [`HarnessMode`].
    pub mode: HarnessMode,
    /// The client offers no hybrid post-quantum group, so the handshake
    /// negotiates classical X25519 (`false`, the default, is what the
    /// product offers: X25519MLKEM768 first). The two handshakes cost the
    /// server a different number of loop iterations before the first
    /// feedback, which the bandwidth-estimator startup scenes compare.
    pub classical_kx: bool,
}

/// How a scene drives the server's `IoBridge`.
///
/// Two modes exist because they see different failure classes, and neither
/// sees the other's.
///
/// * [`Stepped`](Self::Stepped), the default. The bridge is never spawned:
///   the scene loop pushes bytes into an in-memory pipe, steps
///   `IoBridge::step_until_idle` (the production loop body, polled by hand)
///   to idle, reads what came out, and only then moves the paused clock.
///   A scene is a pure function of its seed (`browserless_determinism`), so
///   a counter that moves between two runs is a code change, not noise.
///   It CAN see: logic in the loop body (scheduling, pacing, recovery,
///   congestion control, codec selection) and every counter derived from it.
///   It CANNOT see: anything that needs the client and the server to run at
///   the same time. The harness writes and the bridge reads at the same
///   virtual instant, so inbound service delay is 0 by construction; the pipe
///   has no capacity, so there is no backpressure on either side; and the
///   bridge always runs to idle before the client moves, so a long blocking
///   section in the loop delays nothing the scene can observe. Event-loop
///   starvation, head-of-line blocking in a handler and reactor wake-up
///   behaviour are all invisible.
/// * [`Concurrent`](Self::Concurrent), opt-in. `IoBridge::run` is spawned over
///   a real socketpair and the scene talks to it through
///   [`crate::netsim::SocketPairPump`], the pre-stepping harness path. Real reactor, real
///   socket buffers, the bridge and the client interleave as tasks.
///   It CAN see: starvation of one arm of the select by another, a handler
///   that holds the loop (inline sleep, a blocking section), socket
///   backpressure, and anything measured by `inbound_service_delays`.
///   It CANNOT give: seed reproducibility. The task schedule and the reactor
///   decide what happens next, so same-seed runs differ by a few percent in
///   byte totals and the BWE trace, and a pass or fail is only a statement
///   about thresholds with slack.
///   It also adds latency production does not have. A datagram crossing the
///   socketpair wakes its reader through the reactor, and a reactor wake
///   does not stop tokio's paused clock from auto-advancing to the next
///   timer before the woken task runs (only a remote `unpark` does). So a
///   hop can cost a jump of virtual time: on the 5 s rich scene of
///   `browserless_determinism` (10 ms link) the server reads a client
///   datagram a mean 5 ms late, p90 10 ms, up to ~50 ms; the handshake's
///   first reply leaves at 100 ms instead of 10 ms; emit-to-ACK latency is
///   45 ms against Stepped's 27.5 ms. The inflated RTT shrinks what the
///   scene carries: about 1200 server datagrams against Stepped's 2091, and
///   Stepped over a 15 ms link lands back near 1400. Read Concurrent's
///   volumes as biased low and its latencies as biased high. Stepped serves
///   a datagram at the instant it arrives, as the production loop does when
///   idle, and is the reference for those numbers.
///   In short: Concurrent adds about 5 ms per socket hop. tokio's paused
///   clock auto-advances to the next timer on an I/O wake (tokio 1.52,
///   `runtime/time/mod.rs` ~263-275), so it inflates RTT, and numbers
///   recorded on master with the concurrent harness are inflated by it.
///
/// Stepped has its own, smaller bias. It makes per-turn CPU cost free (the
/// virtual clock does not move while the bridge computes, unless
/// `RunOptions::frame_cost` charges for it), it does not model the
/// ghostbridge/WireGuard userspace hop, and it drains a big backlog
/// instantly. In production the idle service delay is microseconds, so the
/// bias is small, but it hides CPU-bound behaviour unless `frame_cost` is set.
/// Re-baseline thresholds on Stepped (D1 PR-0).
///
/// Use `Stepped` for everything that asserts on what the server decides, and
/// `Concurrent` only for a test whose subject is the loop's own concurrency.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub enum HarnessMode {
    #[default]
    Stepped,
    Concurrent,
}

/// Outcome of one `run_browserless` call.
pub struct BrowserlessResult {
    /// `(virtual_us_written, delay_us)` for every client -> server packet:
    /// the time between the harness writing it to the socketpair and the
    /// server's inbound arm taking it off. Near zero while the loop is idle;
    /// a frame that holds the loop makes it at least the remaining hold.
    pub inbound_service_delays: Vec<(u64, u64)>,
    /// `(start_us, end_us)` virtual-time windows during which the server's
    /// loop actually sat in an injected frame's `cost` hold, logged by the
    /// bridge itself. Empty without a cost.
    pub frame_cost_windows: Vec<(u64, u64)>,
    pub framebuffer: FrameBuffer,
    pub events: Vec<ClientNetEvent>,
    pub bytes_delivered: u64,
    /// Server -> client bytes delivered. `bytes_delivered` sums both
    /// directions, which makes it useless for reasoning about what the
    /// server's emission budget is doing — the client's ACK stream is in
    /// there too.
    pub bytes_delivered_s2c: u64,
    /// Client -> server bytes delivered (ACKs, NACKs, input).
    pub bytes_delivered_c2s: u64,
    /// Cumulative `send_datagram` failures on the server, overwhelmingly
    /// `SendDatagramError::Blocked`.
    ///
    /// A scene claiming anything about the Blocked path must assert this is
    /// non-zero first: with the production 16 MiB buffer it is always 0, so
    /// such a scene would otherwise pass without ever creating the case.
    pub send_datagram_errs: u64,
    /// High-water depth of the emitter's emission queue.
    ///
    /// The measurement that decides whether step B (scheduler backpressure
    /// on emitter-queue depth) is needed: a peak that scales with the work
    /// enqueued means the backlog merely moved out of quinn's send buffer
    /// into ours.
    pub emission_queue_peak: usize,
    /// Server -> client datagrams delivered.
    ///
    /// Distinct from `bytes_delivered_s2c` and not derivable from it. A test
    /// reasoning about *concurrency* -- how many datagrams a link could have
    /// carried in flight at once -- must count datagrams, because bytes
    /// conflate "fewer datagrams" with "smaller datagrams". Switching the
    /// Cdf53 encoder to skip empty bit-planes shrank payloads enough to push
    /// `a_link_with_propagation_delay_carries_datagrams_concurrently` under a
    /// byte floor it had been clearing, with concurrency entirely unchanged.
    pub datagrams_delivered_s2c: u64,
    pub bytes_dropped: u64,
    /// Server -> client datagrams the netsim ruled on, and how many of those
    /// it dropped (count, not bytes; any cause: random/burst loss, cap,
    /// bottleneck). `s2c_datagrams_dropped / s2c_datagrams_offered` is the
    /// realised loss rate of the run.
    pub s2c_datagrams_offered: u64,
    pub s2c_datagrams_dropped: u64,
    /// Number of maximal runs of consecutive s2c drops, and the longest one.
    /// `s2c_datagrams_dropped / s2c_loss_runs` is the mean loss-run length
    /// ([`BrowserlessResult::s2c_mean_loss_run`]). Runs are over the order
    /// the netsim decided, which is the order the server transmitted.
    pub s2c_loss_runs: u64,
    /// Hash of every datagram the netsim ruled on, both directions, in order:
    /// direction, offer instant, length, and the verdict's arrival instants
    /// and lengths. Stepped mode makes it a function of the seed.
    pub wire_digest: u64,
    /// Datagrams hashed into `wire_digest`; a premise that it is not hashing
    /// nothing.
    pub wire_datagrams: u64,
    /// Bounded record of the datagrams behind `wire_digest`, for locating a
    /// divergence between two runs.
    pub wire_trace: WireTrace,
    pub s2c_loss_run_max: u64,
    /// Read from `FrameBuffer::stale_frame_tiles()` — a scene run only
    /// ever ingests via `FrameBuffer::apply_tile_ready` (real decoded
    /// `TileReady` events), never `FrameBuffer::apply`, so the `frame_seq`
    /// staleness counter is the one that means something here. This
    /// module does not maintain a second staleness definition of its own.
    pub stale_generation_tiles: u32,
    pub seed: u64,
    /// Server-side bandwidth estimate at the end of the scene, bits per
    /// second. Zero if the controller never produced one.
    pub bwe_estimate_bps: u64,
    /// goog_cc's pacer rate at scene end, bits per second. This, not
    /// `bwe_estimate_bps`, is what `combine_pacing_budget` actually limits
    /// emission with, so a scene whose wire rate exceeds the estimate should
    /// be read against this before concluding the estimate is being ignored.
    pub pacer_rate_bps: Option<u64>,
    /// `(virtual_us, bitrate_bps)` sampled at most every 100 ms of virtual
    /// time while the scene runs.
    ///
    /// `bwe_estimate_bps` alone cannot distinguish an estimator that tracked
    /// a changing link from one that happened to finish near the right
    /// number. A step-up scene needs to see both sides of the step, so this
    /// records the series the final value is drawn from.
    pub bwe_estimate_samples: Vec<(u64, u64)>,
    /// Count of derived RTTs implausible against quinn's measured path RTT.
    /// Non-zero means emit and arrival timestamps are not on one clock.
    pub implausible_rtt_samples: u64,
    /// ACK-arrival samples the estimator actually consumed during the scene.
    /// Zero means the production path never delivered any, which the estimate
    /// alone cannot reveal — an unfed estimator simply reports its seed.
    pub bwe_samples_seen: u64,
    /// Total retransmit attempts the server's `ReliableTileEmitter` made
    /// (RTO-driven + NACK-driven combined), read from
    /// `IoBridge::emitter_stats_publish_handle()` after the bridge task is
    /// aborted — same cell/republish/read-after-abort pattern as
    /// `bwe_estimate_bps` above (see that field's construction site for the
    /// mechanics). A scene asserting this is non-zero is claiming the
    /// retransmit path was actually exercised; **zero is ambiguous** — it
    /// means either "no loss needed a retransmit" (a genuinely quiet path,
    /// e.g. FEC absorbed every loss) or "the emitter was never fed at all".
    /// Cross-check against `bytes_delivered`/`nack_hit`/`nack_miss` before
    /// treating a zero as proof of a quiet path.
    pub retransmit_attempts_total: u64,
    /// NACKs the emitter matched against a still-cached fragment and
    /// actually retransmitted. Zero alongside a non-zero
    /// `retransmit_attempts_total` means every retransmit in this scene was
    /// RTO-driven rather than NACK-driven.
    pub nack_hit: u64,
    /// NACKs the emitter received for a fragment no longer in its
    /// retransmit cache (already ack'd, evicted, or never sent under that
    /// key). Non-zero is not itself a bug, but a large count relative to
    /// `nack_hit` suggests the client is NACKing stale state.
    pub nack_miss: u64,
    /// Count of RTO deadlines that fired (the emitter's own retransmit
    /// timer, independent of any client NACK). Zero does not imply the
    /// path was unfed — a scene can retransmit purely via `nack_hit`.
    pub rto_fired: u64,
    /// Number of ACKed critical-tier (CDF53 passes 0-3) samples that fed
    /// `critical_latency_mean_us`/`critical_latency_max_us`/
    /// `critical_latency_buckets`. Read from
    /// `IoBridge::latency_stats_publish_handle()` after the bridge task is
    /// aborted — same cell/republish/read-after-abort pattern as
    /// `bwe_estimate_bps` (BWE Stage 2.0 baseline measurement). Zero here
    /// means passes 0-3 were never ACKed in this scene — a pacer built on
    /// top of this baseline would have nothing to optimise for.
    pub critical_latency_count: u64,
    /// Mean of `(received_at - server_emit_us)` in microseconds across
    /// `critical_latency_count` samples — both timestamps are on the
    /// server's own clock, so this is an emit-to-ACK-receipt round trip,
    /// not a one-way delay. Zero when `critical_latency_count` is zero.
    pub critical_latency_mean_us: u64,
    /// Max of the same per-sample latency, over the same population.
    pub critical_latency_max_us: u64,
    /// Coarse latency histogram over the same population, bucket bounds
    /// (ms, exclusive upper bound) `[5, 10, 20, 50, 100, 200, 500]` plus an
    /// open-ended "500+" tail as the 8th bucket. See
    /// `ghostframe_lib::transport::io_bridge::TierLatencyStats`.
    pub critical_latency_buckets: [u64; 8],
    /// Same as `critical_latency_count`, for refinement-tier (CDF53 passes
    /// 4-13) samples.
    pub refinement_latency_count: u64,
    /// Same as `critical_latency_mean_us`, for the refinement tier.
    pub refinement_latency_mean_us: u64,
    /// Same as `critical_latency_max_us`, for the refinement tier.
    pub refinement_latency_max_us: u64,
    /// Same as `critical_latency_buckets`, for the refinement tier.
    pub refinement_latency_buckets: [u64; 8],
    /// Same population as `critical_latency_count`, but measured
    /// `queued_at -> ACK` instead of `last_sent_at -> ACK` (BWE Stage
    /// 2.1) — `queued_at` is when the underlying `TileWork` became
    /// available to the scheduler, so this additionally captures
    /// scheduler queueing delay that `critical_latency_count`'s interval
    /// cannot see (it starts only when a pass last left the wire). Read
    /// from `IoBridge::queued_latency_stats_publish_handle()`, same
    /// cell/republish/read-after-abort pattern as `critical_latency_count`.
    pub queued_critical_latency_count: u64,
    /// Mean of `(received_at - queued_at)` in microseconds across
    /// `queued_critical_latency_count` samples. Always >= the
    /// corresponding `critical_latency_mean_us` for the same population,
    /// since it starts earlier. Zero when the count is zero.
    pub queued_critical_latency_mean_us: u64,
    /// Max of the same per-sample latency, over the same population.
    pub queued_critical_latency_max_us: u64,
    /// Same bucket scheme as `critical_latency_buckets`, over the
    /// `queued_at -> ACK` population.
    pub queued_critical_latency_buckets: [u64; 8],
    /// Same as `queued_critical_latency_count`, for refinement-tier
    /// (CDF53 passes 4-13) samples.
    pub queued_refinement_latency_count: u64,
    /// Same as `queued_critical_latency_mean_us`, for the refinement tier.
    pub queued_refinement_latency_mean_us: u64,
    /// Same as `queued_critical_latency_max_us`, for the refinement tier.
    pub queued_refinement_latency_max_us: u64,
    /// Same as `queued_critical_latency_buckets`, for the refinement tier.
    pub queued_refinement_latency_buckets: [u64; 8],
    /// Cumulative count of probe windows (BWE Stage 2.4) that closed having
    /// met both `min_probes` and `min_bytes`, read from
    /// `IoBridge::probe_stats_publish_handle()` after the bridge task is
    /// aborted — same cell/republish/read-after-abort pattern as
    /// `bwe_estimate_bps`. **Zero here alongside a zero
    /// `probes_abandoned` means the probe window never opened at all** —
    /// goog_cc's `ProbeController` never requested a cluster during this
    /// scene — which is different from, and should not be read as, "probing
    /// ran and found nothing to report".
    pub probes_completed: u64,
    /// Cumulative count of probe windows that closed *without* meeting both
    /// thresholds. Per the design's "No padding" section this is the
    /// **expected** outcome on an idle link, not a bug — see
    /// `probes_completed`'s doc comment for the zero/zero case.
    pub probes_abandoned: u64,
    /// Datagrams each `DropRule` in `BrowserlessScene::drops` actually dropped,
    /// parallel to the rules given to `DropPlan::new`. A scene asserting on the
    /// *effect* of an injected drop must first assert the matching entry is
    /// non-zero: a rule naming a tile the scene never sends is silent, and
    /// without that check the effect assertion passes whether or not anything
    /// was ever dropped. Distinct from `bytes_dropped`, which counts *netsim*
    /// drops across both directions and cannot separate the two causes.
    pub drops_fired: Vec<u32>,
    /// Of `retransmit_attempts_total`, those a NACK asked for.
    pub retransmit_by_nack: u64,
    /// RTO fires whose pass was then acknowledged by a transmission sent
    /// before the first retransmission (the timer fired for data still on its
    /// way). `rto_fired - rto_spurious - rto_useful` fired for passes not
    /// acknowledged by the end of the scene.
    pub rto_spurious: u64,
    /// RTO fires whose pass was first acknowledged through the original (see
    /// `EmitterStats::rto_original_first`). The share of `rto_fired` that
    /// this is, is the spurious-timeout rate; `rto_spurious` above reverts to
    /// useful once the retransmission is acknowledged too, which it nearly
    /// always is.
    pub rto_original_first: u64,
    /// RTO fires whose pass was repaired by the retransmission.
    pub rto_useful: u64,
    /// quinn path stats, send-queue depth and the client's feedback totals,
    /// from `IoBridge::transport_probe_publish_handle()`. `transport.samples
    /// == 0` means the probe never read a live connection.
    pub transport: ghostframe_lib::transport::io_bridge::TransportProbe,
    /// The client's own pre-FEC loss, loss-run histogram and recoveries.
    pub client_wire_loss: ghostframe_client_core::WireLossStats,
    /// `(datagram frames quinn received, frames read by the application)` on
    /// the client; the difference is receive-buffer drops.
    pub client_rx_datagrams: Option<(u64, u64)>,
}

impl BrowserlessResult {
    /// Mean length of an s2c loss run; 0.0 when nothing was dropped.
    pub fn s2c_mean_loss_run(&self) -> f64 {
        if self.s2c_loss_runs == 0 {
            0.0
        } else {
            self.s2c_datagrams_dropped as f64 / self.s2c_loss_runs as f64
        }
    }
}

/// Running tally of server -> client netsim verdicts.
#[derive(Default)]
struct S2cLoss {
    /// Digest of every datagram the netsim ruled on, in both directions. See
    /// [`WireDigest`]. Lives here only because this struct is already
    /// threaded through the scene loop.
    wire: WireDigest,
    offered: u64,
    dropped: u64,
    runs: u64,
    run_max: u64,
    cur_run: u64,
}

/// Running hash of the scene's wire traffic: for every datagram offered to the
/// netsim, its direction, the instant it was offered, its length, and what the
/// netsim did with it (the `at_us` and length of each copy, or a drop marker).
/// Two same-seed runs that agree on every counter can still differ in which
/// datagram went out when and how big; this is what catches that.
///
/// Lengths, not bytes: the QUIC payloads are encrypted under keys that rustls
/// draws from the OS (the TLS ephemeral keys and the server's certificate are
/// not seedable), so the ciphertext differs run to run by construction while
/// everything the endpoints decide stays identical. Hashing the bytes was
/// tried first and diverged on every run for exactly that reason.
#[derive(Default)]
struct WireDigest {
    hasher: std::collections::hash_map::DefaultHasher,
    datagrams: u64,
    trace: WireTrace,
    /// Verdict hash of the datagram being ruled (set by `offered`, folded
    /// into its record by `ruled`).
    window: std::collections::hash_map::DefaultHasher,
}

/// Datagrams whose raw record is kept for debugging a digest mismatch.
pub const WIRE_TRACE_RECORDS: usize = 4096;
/// Datagrams per window hash; the window hashes cover the whole run.
pub const WIRE_TRACE_WINDOW: usize = 64;
/// Cap on window hashes kept (64 * 65536 = 4M datagrams, 512 KiB).
const WIRE_TRACE_MAX_WINDOWS: usize = 65536;

/// One datagram offered to the netsim: direction (0 = c2s, 1 = s2c), offer
/// instant, length, and a hash of the verdict (arrival instants and lengths).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WireRecord {
    pub dir: u8,
    pub t_us: u64,
    pub len: usize,
    pub verdict: u64,
}

/// Bounded evidence behind `wire_digest`: the first [`WIRE_TRACE_RECORDS`]
/// records raw, plus a hash per [`WIRE_TRACE_WINDOW`] datagrams for the rest.
/// Memory is bounded (~100 KiB of records, at most 512 KiB of windows).
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct WireTrace {
    pub records: Vec<WireRecord>,
    pub windows: Vec<u64>,
}

impl WireTrace {
    /// Text form, one line per record then one per window, for a child
    /// process to hand to its parent.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        for r in &self.records {
            out.push_str(&format!("R {} {} {} {}\n", r.dir, r.t_us, r.len, r.verdict));
        }
        for w in &self.windows {
            out.push_str(&format!("W {w}\n"));
        }
        out
    }

    pub fn from_text(text: &str) -> Self {
        let mut t = WireTrace::default();
        for l in text.lines() {
            let f: Vec<&str> = l.split(' ').collect();
            match f.as_slice() {
                ["R", d, at, len, v] => t.records.push(WireRecord {
                    dir: d.parse().unwrap(),
                    t_us: at.parse().unwrap(),
                    len: len.parse().unwrap(),
                    verdict: v.parse().unwrap(),
                }),
                ["W", w] => t.windows.push(w.parse().unwrap()),
                _ => {}
            }
        }
        t
    }

    /// Describe the first divergence between two traces, with the records
    /// around it; `None` if they agree on everything kept.
    pub fn first_divergence(&self, other: &WireTrace) -> Option<String> {
        let fmt = |r: Option<&WireRecord>| match r {
            Some(r) => format!(
                "{} t={}us len={} verdict={:016x}",
                if r.dir == 0 { "c2s" } else { "s2c" },
                r.t_us,
                r.len,
                r.verdict
            ),
            None => "<none>".to_string(),
        };
        let n = self.records.len().max(other.records.len());
        let idx = (0..n).find(|&i| self.records.get(i) != other.records.get(i));
        if let Some(i) = idx {
            let lo = i.saturating_sub(3);
            let mut out = format!(
                "first divergent datagram: index {i}, a = [{}], b = [{}]\nrecords around it (a | b):\n",
                fmt(self.records.get(i)),
                fmt(other.records.get(i))
            );
            for j in lo..(i + 4).min(n) {
                out.push_str(&format!(
                    "  #{j}{} {} | {}\n",
                    if j == i { " <--" } else { "" },
                    fmt(self.records.get(j)),
                    fmt(other.records.get(j))
                ));
            }
            return Some(out);
        }
        let w = self.windows.len().max(other.windows.len());
        (0..w)
            .find(|&i| self.windows.get(i) != other.windows.get(i))
            .map(|i| {
                format!(
                    "first divergent window: datagrams {}..{} (past the {} raw records kept)",
                    i * WIRE_TRACE_WINDOW,
                    (i + 1) * WIRE_TRACE_WINDOW,
                    WIRE_TRACE_RECORDS
                )
            })
    }
}

impl WireDigest {
    fn offered(&mut self, dir: Direction, now_us: u64, payload: &[u8]) {
        use std::hash::Hasher;
        self.datagrams += 1;
        self.hasher.write_u8(dir as u8);
        self.hasher.write_u64(now_us);
        self.hasher.write_usize(payload.len());
        if self.trace.records.len() < WIRE_TRACE_RECORDS {
            self.trace.records.push(WireRecord {
                dir: dir as u8,
                t_us: now_us,
                len: payload.len(),
                verdict: 0,
            });
        }
        self.window.write_u8(dir as u8);
        self.window.write_u64(now_us);
        self.window.write_usize(payload.len());
    }
    fn ruled(&mut self, items: &[InFlight]) {
        use std::hash::Hasher;
        self.hasher.write_usize(items.len());
        let mut v = std::collections::hash_map::DefaultHasher::new();
        v.write_usize(items.len());
        for i in items {
            self.hasher.write_u64(i.at_us);
            self.hasher.write_usize(i.payload.len());
            v.write_u64(i.at_us);
            v.write_usize(i.payload.len());
        }
        let v = v.finish();
        let idx = self.datagrams as usize - 1;
        if let Some(r) = self.trace.records.get_mut(idx) {
            r.verdict = v;
        }
        self.window.write_u64(v);
        if (self.datagrams as usize).is_multiple_of(WIRE_TRACE_WINDOW) {
            self.close_window();
        }
    }
    fn close_window(&mut self) {
        use std::hash::Hasher;
        if self.trace.windows.len() < WIRE_TRACE_MAX_WINDOWS {
            self.trace.windows.push(self.window.finish());
        }
        self.window = Default::default();
    }
    fn finish(&self) -> u64 {
        use std::hash::Hasher;
        self.hasher.finish()
    }
    /// The trace, with the final partial window closed.
    fn into_trace(mut self) -> WireTrace {
        if !(self.datagrams as usize).is_multiple_of(WIRE_TRACE_WINDOW) {
            self.close_window();
        }
        self.trace
    }
}

impl S2cLoss {
    fn record(&mut self, dropped: bool) {
        self.offered += 1;
        if dropped {
            self.dropped += 1;
            if self.cur_run == 0 {
                self.runs += 1;
            }
            self.cur_run += 1;
            self.run_max = self.run_max.max(self.cur_run);
        } else {
            self.cur_run = 0;
        }
    }
}

/// Per-direction link overrides for [`run_browserless_with_links`].
///
/// `BrowserlessScene::net` applies the same profile to both directions. A
/// burst-loss scene wants loss on one direction only, so this replaces a
/// direction's profile (and XORs `*_seed_salt` into its `NetSim` seed).
/// `None` keeps `scene.net`. Not a `BrowserlessScene` field so existing
/// struct-literal scenes are untouched.
#[derive(Clone, Default)]
pub struct LinkOverrides {
    pub c2s: Option<NetProfile>,
    pub s2c: Option<NetProfile>,
    pub c2s_seed_salt: u64,
    pub s2c_seed_salt: u64,
}

impl LinkOverrides {
    /// Burst-loss models from the `GHOSTFRAME_<DIR>_BURST_*` environment
    /// variables (see `netsim::burst`), applied to a copy of `scene_net`, REPLACING its i.i.d. `loss` on that direction. OUTBOUND is
    /// server -> client, INBOUND client -> server. Empty when no burst
    /// variable is set. Panics on malformed values: a typo must not silently
    /// run a clean link.
    pub fn from_burst_env(scene_net: &NetProfile) -> Self {
        let mut o = Self::default();
        for (dir, is_s2c) in [
            (crate::netsim::BurstDirection::Outbound, true),
            (crate::netsim::BurstDirection::Inbound, false),
        ] {
            let cfg = crate::netsim::BurstConfig::from_env(dir)
                .unwrap_or_else(|e| panic!("bad burst-loss environment: {e}"));
            if let Some(cfg) = cfg {
                let mut p = scene_net.clone();
                cfg.model.apply_to(&mut p);
                if is_s2c {
                    o.s2c = Some(p);
                    o.s2c_seed_salt = cfg.seed;
                } else {
                    o.c2s = Some(p);
                    o.c2s_seed_salt = cfg.seed;
                }
            }
        }
        o
    }
}

/// The address the harness uses to identify the client, baked into every
/// ghostbridge frame carrying client->server traffic. `IoBridge` echoes
/// this straight back as the destination of server->client traffic, so it
/// never needs to mean anything beyond "the same value both ends agree on".
fn client_addr() -> SocketAddr {
    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000)
}

/// The address `ClientNet::connect` dials, and the address every
/// server->client datagram is attributed to when handed to
/// `ClientNet::handle_udp`.
fn server_addr() -> SocketAddr {
    SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443)
}

/// Run one browserless netsim scene: establish a real WebTransport session
/// between a real `IoBridge` and a real `ClientNet` across a Unix
/// socketpair, with both directions passing through a seeded `NetSim`.
///
/// Bounded by both `scene.duration` (virtual time) and an internal
/// iteration cap — this never blocks forever. Every failure path names
/// `scene.seed`, since the seed is what makes a netsim failure
/// reproducible.
///
/// Honours the `GHOSTFRAME_{OUTBOUND,INBOUND}_BURST_*` variables (off unless
/// set); see [`LinkOverrides::from_burst_env`].
pub async fn run_browserless(scene: BrowserlessScene) -> anyhow::Result<BrowserlessResult> {
    let links = LinkOverrides::from_burst_env(&scene.net);
    run_browserless_with_links(scene, links).await
}

/// [`run_browserless`] with explicit per-direction link overrides and no
/// environment lookup (scenes run in-process, so the env is process-global).
pub async fn run_browserless_with_links(
    scene: BrowserlessScene,
    links: LinkOverrides,
) -> anyhow::Result<BrowserlessResult> {
    run_browserless_with_options(scene, links, RunOptions::default()).await
}

/// [`run_browserless_with_links`] with measurement options.
pub async fn run_browserless_with_options(
    scene: BrowserlessScene,
    links: LinkOverrides,
    opts: RunOptions,
) -> anyhow::Result<BrowserlessResult> {
    let local = LocalSet::new();
    local.run_until(run_inner(scene, links, opts)).await
}

async fn run_inner(
    mut scene: BrowserlessScene,
    links: LinkOverrides,
    opts: RunOptions,
) -> anyhow::Result<BrowserlessResult> {
    let seed = scene.seed;

    let stepped = opts.mode == HarnessMode::Stepped;
    // Pin the server endpoint's RNG from the scene seed, as `drive_session`
    // does for the client. The client seed uses the low bytes, this one a
    // distinct pattern so the two endpoints do not share a stream.
    let mut server_rng_seed = [0u8; 32];
    server_rng_seed[..8].copy_from_slice(&(seed ^ 0x5E4E_5E4E_5E4E_5E4E).to_le_bytes());
    let server = QuicServer::new_with_rng_seed(
        scene
            .datagram_send_buffer_bytes
            .unwrap_or_else(ghostframe_lib::config::datagram_send_buffer_bytes),
        server_rng_seed,
    )
    .map_err(|e| anyhow!("seed {seed}: QuicServer::new failed: {e}"))?;

    // Cert hash MUST be taken before `server` moves into the bridge below.
    let mut server_cert_sha256 = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut server_cert_sha256)
        .map_err(|e| anyhow!("seed {seed}: cert hash hex decode failed: {e}"))?;

    // `inject_tx` carries `FrameScript`-derived `InjectedFrame`s to
    // `IoBridge`, once the session is ready — see `drive_session`.
    let (inject_tx, inject_rx) = mpsc::channel::<InjectedFrame>(8);

    // Stepped: an in-memory pipe the scene loop steps. Concurrent: a real
    // socketpair and a spawned `run()`.
    let (mut bridge, link_end) = if stepped {
        let (ours, peer) = mem_stream_pair();
        (
            IoBridge::new_stepped_for_test(
                ours,
                server,
                inject_rx,
                scene.grid_cols,
                scene.grid_rows,
            ),
            LinkEnd::Mem(peer),
        )
    } else {
        let (ours, peer) = tokio::net::UnixStream::pair()
            .map_err(|e| anyhow!("seed {seed}: UnixStream::pair failed: {e}"))?;
        (
            IoBridge::new_with_injection_for_test(
                ours,
                server,
                inject_rx,
                scene.grid_cols,
                scene.grid_rows,
            ),
            LinkEnd::Pump(SocketPairPump::new(peer)),
        )
    };
    // `bridge` is moved wholesale into the spawned task below, so a plain
    // `&self` accessor on it (`IoBridge::bwe_snapshot`) is unreachable from
    // this function afterwards — there is no way to call back into a value
    // a spawned task owns. Clone the `Arc` behind `IoBridge::bwe_publish`
    // *before* the move; `run()`'s loop republishes into it on every
    // iteration, and this function reads the clone's contents after
    // `bridge_handle.abort()`. See `bwe_publish`'s doc comment in
    // `io_bridge.rs` for the full picture — this is option (a) from that
    // comment, chosen because it doesn't disturb the existing `spawn_local`
    // + abort shape the bring-up tests already depend on.
    let bwe_cell = bridge.bwe_publish_handle();
    // Observes the server-side HELLO'd-session count; see `drive_session`.
    let server_live_rx = bridge.connected_session_count_handle();
    // Same reasoning, same pattern, for the reliable emitter's retransmit
    // counters — see `emitter_stats_publish`'s doc comment in `io_bridge.rs`.
    let emitter_stats_cell = bridge.emitter_stats_publish_handle();
    // Same reasoning, same pattern, for the per-tier ACK latency stats
    // (BWE Stage 2.0 baseline measurement) — see `latency_stats_publish`'s
    // doc comment in `io_bridge.rs`.
    let latency_stats_cell = bridge.latency_stats_publish_handle();
    // Same reasoning, same pattern, for the `queued_at -> ACK` per-tier
    // latency stats (BWE Stage 2.1) — see
    // `queued_latency_stats_publish`'s doc comment in `io_bridge.rs`.
    let queued_latency_stats_cell = bridge.queued_latency_stats_publish_handle();
    // Same reasoning, same pattern, for the probe-window counters (BWE
    // Stage 2.4) — see `probe_stats_publish`'s doc comment in
    // `io_bridge.rs`.
    let probe_stats_cell = bridge.probe_stats_publish_handle();
    let transport_probe_cell = bridge.transport_probe_publish_handle();
    let send_errs_cell = bridge.send_errs_publish_handle();
    // Same ownership problem, opposite direction: this `Arc` originates
    // here (it already holds `scene.drops`'s rules), and `set_drop_plan`
    // installs it into the bridge so `send_to_all_sessions` can consult it
    // at the plaintext seam — see `drop_plan`'s doc comment in
    // `io_bridge.rs` for why that seam, not the QUIC layer. `mem::take`
    // leaves an empty (default) plan behind in `scene`, which is fine: the
    // scene has nothing further to do with it.
    let drop_plan_cell =
        std::sync::Arc::new(std::sync::Mutex::new(std::mem::take(&mut scene.drops)));
    bridge.set_drop_plan(drop_plan_cell.clone());
    if let Some(floor) = scene.tick_budget_floor_bytes {
        bridge.set_tick_budget_floor_bytes(floor);
    }
    let read_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    bridge.set_inbound_read_log(read_log.clone());
    let hold_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    bridge.set_hold_log(hold_log.clone());
    let write_log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let link_end = match link_end {
        LinkEnd::Pump(p) => LinkEnd::Pump(p.with_send_log(write_log.clone())),
        mem => mem,
    };
    let mut link = BridgeLink {
        end: link_end,
        send_log: write_log.clone(),
    };
    // Concurrent mode: `IoBridge::run` is an infinite loop that only returns
    // on EOF or error, so it is aborted explicitly below rather than awaited.
    // Stepped mode keeps the bridge here and lends it to `drive_session`.
    let mut bridge_slot = None;
    let mut bridge_handle = None;
    if stepped {
        bridge_slot = Some(bridge);
    } else {
        bridge_handle = Some(tokio::task::spawn_local(async move {
            let _ = bridge.run().await;
        }));
    }
    let base = TokioInstant::now();
    let client_addr = client_addr();
    let server_addr = server_addr();

    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256,
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
    };

    let mut framebuffer = FrameBuffer::new();
    let mut s2c_loss = S2cLoss::default();
    let mut bwe_estimate_samples: Vec<(u64, u64)> = Vec::new();
    let outcome = drive_session(
        cfg,
        bridge_slot.as_mut(),
        &mut link,
        client_addr,
        server_addr,
        base,
        &scene,
        &inject_tx,
        &mut framebuffer,
        &bwe_cell,
        &mut bwe_estimate_samples,
        &server_live_rx,
        &links,
        &mut s2c_loss,
        opts,
    )
    .await;

    // Always abort a spawned bridge, on every exit path: the task must not
    // outlive this function, and `outcome` may be an `Err`. A stepped bridge
    // has no task to stop; either way what it published on its last turn is
    // the final snapshot.
    if let Some(h) = bridge_handle {
        h.abort();
    }

    let (
        events,
        bytes_delivered,
        bytes_delivered_s2c,
        bytes_delivered_c2s,
        datagrams_delivered_s2c,
        bytes_dropped,
        client_probe,
    ) = outcome?;

    let stale_generation_tiles = framebuffer.stale_frame_tiles();
    let bwe_snapshot = *bwe_cell.lock().expect("bwe_publish mutex poisoned");
    let emitter_stats = *emitter_stats_cell
        .lock()
        .expect("emitter_stats_publish mutex poisoned");
    let (critical_latency, refinement_latency) = *latency_stats_cell
        .lock()
        .expect("latency_stats_publish mutex poisoned");
    let (queued_critical_latency, queued_refinement_latency) = *queued_latency_stats_cell
        .lock()
        .expect("queued_latency_stats_publish mutex poisoned");
    let transport = *transport_probe_cell
        .lock()
        .expect("transport_probe_publish mutex poisoned");
    let send_datagram_errs = *send_errs_cell.lock().expect("send_errs mutex poisoned");
    let (probes_completed, probes_abandoned) = *probe_stats_cell
        .lock()
        .expect("probe_stats_publish mutex poisoned");
    let drops_fired = drop_plan_cell
        .lock()
        .expect("drop_plan mutex poisoned")
        .drops()
        .to_vec();

    // Pair each write with the read of the same packet by position: the
    // socketpair is a FIFO and the inbound arm takes one packet per turn.
    let writes = write_log.lock().expect("write_log poisoned").clone();
    let reads = read_log.lock().expect("read_log poisoned").clone();
    // Packets written but not yet read when the scene ended are the tail;
    // a read without a logged write, or a long unread run, would mean the
    // positional pairing has drifted, which must be loud, not a shifted series.
    anyhow::ensure!(
        reads.len() <= writes.len() && writes.len() - reads.len() <= 64,
        "seed {seed}: inbound pairing drifted: {} writes vs {} reads",
        writes.len(),
        reads.len()
    );
    let frame_cost_windows: Vec<(u64, u64)> = hold_log
        .lock()
        .expect("hold_log poisoned")
        .iter()
        .map(|(a, b)| {
            (
                a.saturating_duration_since(base.into_std()).as_micros() as u64,
                b.saturating_duration_since(base.into_std()).as_micros() as u64,
            )
        })
        .collect();
    let inbound_service_delays: Vec<(u64, u64)> = writes
        .iter()
        .zip(reads.iter())
        .map(|(w, r)| {
            (
                w.saturating_duration_since(base.into_std()).as_micros() as u64,
                r.saturating_duration_since(*w).as_micros() as u64,
            )
        })
        .collect();

    Ok(BrowserlessResult {
        inbound_service_delays,
        frame_cost_windows,
        send_datagram_errs,
        emission_queue_peak: emitter_stats.emission_queue_peak,
        framebuffer,
        events,
        bytes_delivered,
        bytes_delivered_s2c,
        bytes_delivered_c2s,
        datagrams_delivered_s2c,
        bytes_dropped,
        s2c_datagrams_offered: s2c_loss.offered,
        s2c_datagrams_dropped: s2c_loss.dropped,
        s2c_loss_runs: s2c_loss.runs,
        wire_digest: s2c_loss.wire.finish(),
        wire_datagrams: s2c_loss.wire.datagrams,
        wire_trace: std::mem::take(&mut s2c_loss.wire).into_trace(),
        s2c_loss_run_max: s2c_loss.run_max,
        stale_generation_tiles,
        seed,
        bwe_estimate_bps: bwe_snapshot.bitrate_bps,
        pacer_rate_bps: bwe_snapshot.pacer_rate_bps,
        bwe_estimate_samples,
        implausible_rtt_samples: bwe_snapshot.implausible_rtt_samples,
        bwe_samples_seen: bwe_snapshot.samples_seen,
        retransmit_attempts_total: emitter_stats.retransmit_attempts_total,
        retransmit_by_nack: emitter_stats.retransmit_by_nack,
        rto_spurious: emitter_stats.rto_spurious,
        rto_original_first: emitter_stats.rto_original_first,
        rto_useful: emitter_stats.rto_useful,
        transport,
        client_wire_loss: client_probe.wire_loss,
        client_rx_datagrams: client_probe.rx_datagrams,
        nack_hit: emitter_stats.nack_hit,
        nack_miss: emitter_stats.nack_miss,
        rto_fired: emitter_stats.rto_fired,
        critical_latency_count: critical_latency.count,
        critical_latency_mean_us: critical_latency.mean_us(),
        critical_latency_max_us: critical_latency.max_us,
        critical_latency_buckets: critical_latency.buckets,
        refinement_latency_count: refinement_latency.count,
        refinement_latency_mean_us: refinement_latency.mean_us(),
        refinement_latency_max_us: refinement_latency.max_us,
        refinement_latency_buckets: refinement_latency.buckets,
        queued_critical_latency_count: queued_critical_latency.count,
        queued_critical_latency_mean_us: queued_critical_latency.mean_us(),
        queued_critical_latency_max_us: queued_critical_latency.max_us,
        queued_critical_latency_buckets: queued_critical_latency.buckets,
        queued_refinement_latency_count: queued_refinement_latency.count,
        queued_refinement_latency_mean_us: queued_refinement_latency.mean_us(),
        queued_refinement_latency_max_us: queued_refinement_latency.max_us,
        queued_refinement_latency_buckets: queued_refinement_latency.buckets,
        probes_completed,
        probes_abandoned,
        drops_fired,
    })
}

/// Drive the client<->server session to `SessionReady` and then, if the
/// scene declares any `frames`, inject them in order at `FRAME_SPACING_US`
/// intervals of virtual time — routing every datagram through a seeded
/// `NetSim` per direction throughout, and feeding every decoded
/// `Event::TileReady` into `framebuffer`.
///
/// A scene with no frames returns the instant `SessionReady` is observed,
/// exactly as task 15a's handshake-only behavior did. A scene with frames
/// keeps running the same event loop — sending injected frames on schedule
/// and draining inbound datagrams/events — until `scene.duration` elapses,
/// since that is the scene's only declared time budget and there is no
/// other natural "done" signal (a `TileReady` for the last tile the scene
/// touched does not by itself mean every in-flight retransmission has
/// settled).
///
/// Returns the accumulated client events, bytes delivered, and bytes
/// dropped across both directions.
#[allow(clippy::too_many_arguments)]
async fn drive_session(
    cfg: ClientNetConfig,
    mut bridge: Option<&mut IoBridge>,
    link: &mut BridgeLink,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    base: TokioInstant,
    scene: &BrowserlessScene,
    inject_tx: &mpsc::Sender<InjectedFrame>,
    framebuffer: &mut FrameBuffer,
    bwe_cell: &std::sync::Arc<std::sync::Mutex<ghostframe_lib::transport::bwe::BweSnapshot>>,
    bwe_samples: &mut Vec<(u64, u64)>,
    server_live_rx: &tokio::sync::watch::Receiver<usize>,
    links: &LinkOverrides,
    s2c_loss: &mut S2cLoss,
    opts: RunOptions,
) -> anyhow::Result<(Vec<ClientNetEvent>, u64, u64, u64, u64, u64, ClientProbe)> {
    let seed = scene.seed;

    // Pin the client endpoint's RNG to the scene seed. Left to entropy,
    // quinn-proto skips one application-space packet number drawn from
    // `0..64`; for one of those 64 values the server's ACK of the client's
    // early 1-RTT packets gains a gap range (+2 bytes), so two identical
    // scenes differed in `bytes_delivered_s2c` about 1 run in 60.
    let mut rng_seed = [0u8; 32];
    rng_seed[..8].copy_from_slice(&seed.to_le_bytes());
    let mut client = ClientNet::with_rng_seed(cfg, now_us(base), rng_seed)
        .map_err(|e| anyhow!("seed {seed}: ClientNet::with_rng_seed failed: {e}"))?;
    if opts.classical_kx {
        client = client.with_classical_key_exchange();
    }
    client
        .connect(server_addr, now_us(base))
        .map_err(|e| anyhow!("seed {seed}: ClientNet::connect failed: {e}"))?;

    // Independent RNG streams per direction (see module docs on netsim.rs
    // for why the seeds must differ): scene.seed for client->server,
    // scene.seed XORed against a fixed constant for server->client.
    let mut net_c2s = NetSim::new(
        links.c2s.clone().unwrap_or_else(|| scene.net.clone()),
        seed ^ links.c2s_seed_salt,
    );
    let mut net_s2c = NetSim::new(
        links.s2c.clone().unwrap_or_else(|| scene.net.clone()),
        seed ^ 0xA5A5_A5A5_A5A5_A5A5 ^ links.s2c_seed_salt,
    );

    // Datagrams `NetSim` has already ruled on whose propagation delay has
    // not yet elapsed. Delivery happens from the scene loop below, never
    // inline at the point of sending: awaiting a datagram's arrival where
    // it is sent serialises the link to one datagram in flight at a time.
    // That is invisible at `delay_us: 0` (the await returns immediately)
    // and wedges any busy scene at a realistic RTT, because the inner
    // transmit drain sits outside both the no-progress guard and
    // `overall_deadline`.
    let mut in_flight: BinaryHeap<InFlight> = BinaryHeap::new();
    let mut next_in_flight_seq: u64 = 0;

    // Resolved once here so the loop below never cares which kind of load
    // the scene declared; a generated profile is just a longer script.
    // Borrowed, never cloned: a generated profile can run to tens of
    // thousands of tiles, and a scripted scene's frames are already owned by
    // the caller's `BrowserlessScene`.
    // A generated profile owns its cadence; a scripted scene declares one.
    let cadence_us = match &scene.load {
        SceneLoad::Profile(p) => p.cadence_us,
        SceneLoad::Script(_) => scene.cadence_us,
    };

    let generated: Vec<FrameScript>;
    let frames: &[FrameScript] = match &scene.load {
        SceneLoad::Script(f) => f,
        SceneLoad::Profile(p) => {
            generated = p.frames_for(scene.duration, scene.grid_cols as u8, scene.grid_rows as u8);
            &generated
        }
    };

    let mut events: Vec<ClientNetEvent> = Vec::new();
    let mut bytes_delivered: u64 = 0;
    let mut bytes_delivered_s2c: u64 = 0;
    let mut bytes_delivered_c2s: u64 = 0;
    let mut datagrams_delivered_s2c: u64 = 0;
    let mut bytes_dropped: u64 = 0;

    // Per-tile generation counters for injected frames: a coordinate's
    // generation is the number of times it has previously appeared in an
    // earlier `FrameScript`, wrapping mod 16 (the wire field is 4 bits) —
    // see `inject_frame`.
    let mut generations: HashMap<(u8, u8), u8> = HashMap::new();
    let mut session_ready = false;
    // Virtual time at which the client first reported `SessionReady`.
    let mut session_ready_at_us: u64 = 0;
    let mut next_frame_idx: usize = 0;
    // Set once the session is ready and the scene has at least one frame;
    // `Some(t)` means "the next scheduled injection (a real scene frame, or
    // once those run out, a heartbeat — see below) is due at virtual time
    // `t` (us since `base`)".
    let mut next_inject_at_us: Option<u64> = None;
    // `seq` for heartbeat `InjectedFrame`s sent after the resolved frame
    // list is exhausted (see below). Starts at `frames.len()`, one past the
    // highest `seq` any real scene frame uses, so a heartbeat can never
    // collide with a real frame's wire `frame_seq`.
    let mut heartbeat_seq: u32 = frames.len() as u32;

    // Two independent bounds so a stuck scene fails loudly instead of
    // hanging: the no-progress guard catches a stuck loop, and
    // `overall_deadline` guards
    // virtual time itself (derived from `scene.duration`, the only time
    // budget the scene declares).
    //
    // The guard is on *virtual-time progress*, not raw iteration count.
    //
    // It used to be a flat `MAX_ITERS = 5_000`, and that number could not
    // distinguish the two situations it was asked to catch. A genuinely
    // stuck loop and a merely busy one both reach it; only the second is
    // healthy. Under `start_paused` tokio advances the clock only when every
    // task goes idle, so how many iterations a virtual second costs depends
    // on how often the client's timers fire — and that varies hugely with
    // the connection's state. Measured on the demand-starved probe scene:
    // ~28 ms of virtual time per iteration when quinn is pacing on its
    // ACK-delay timer (the whole 10 s scene in ~350 iterations), but ~500 us
    // per iteration when it falls into fine-grained loss-detection timers —
    // a 56x swing, entirely legitimate, which blew the flat budget and
    // failed the scene with a bail rather than an assertion.
    //
    // Iterations that advance the clock are progress, however many of them
    // there are: `overall_deadline` already bounds the scene in the units it
    // actually declares. What must never happen is the loop turning without
    // the clock moving at all, so that is what this counts.
    const MAX_ITERS_WITHOUT_PROGRESS: usize = 5_000;

    let stepped = bridge.is_some();
    // Consecutive concurrent-mode iterations whose `select!` was won by the
    // pump; see `MAX_CONSECUTIVE_PUMP_SERVES`.
    let mut consecutive_pump_serves: usize = 0;

    // One server -> client packet through the netsim to the client (or into
    // the in-flight heap). A macro because it touches a dozen locals and is
    // called from both modes' receive paths.
    macro_rules! handle_s2c {
        ($pkt:expr) => {{
            let pkt = $pkt;
            let t = now_us(base);
            let items = rule(
                &mut net_s2c,
                Direction::S2c,
                pkt.payload,
                t,
                &mut bytes_dropped,
                &mut s2c_loss.wire,
            );
            // `rule` returns nothing exactly for `Verdict::Drop`.
            s2c_loss.record(items.is_empty());
            for item in items {
                if item.at_us <= t {
                    // Its own arrival instant, for the same reason as in
                    // `flush_due`. Equal to `t` at zero delay, earlier than it
                    // whenever the link models any.
                    client.handle_udp(&item.payload, server_addr, item.at_us);
                    bytes_delivered += item.payload.len() as u64;
                    bytes_delivered_s2c += item.payload.len() as u64;
                    datagrams_delivered_s2c += 1;
                } else {
                    in_flight.push(InFlight {
                        seq: next_in_flight_seq,
                        ..item
                    });
                    next_in_flight_seq += 1;
                }
            }
        }};
    }

    let overall_deadline = base + scene.duration;
    let mut iter: usize = 0;

    // Next virtual time at which to record a bandwidth-estimate sample.
    let mut next_bwe_sample_us: u64 = 0;

    // Virtual time at the last iteration that made progress, and how many
    // iterations have turned since.
    let mut last_progress_vt: u64 = 0;
    let mut iters_without_progress: usize = 0;

    loop {
        iter += 1;
        let vt_now = now_us(base);
        if vt_now >= next_bwe_sample_us {
            // `run()` republishes into this cell every iteration of its own
            // loop, so reading it here is a live sample rather than the
            // post-abort final one `run_browserless` takes.
            let bps = bwe_cell
                .lock()
                .expect("bwe_publish mutex poisoned")
                .bitrate_bps;
            bwe_samples.push((vt_now, bps));
            next_bwe_sample_us = vt_now + 100_000;
        }
        if vt_now > last_progress_vt {
            last_progress_vt = vt_now;
            iters_without_progress = 0;
        } else {
            iters_without_progress += 1;
        }
        if iters_without_progress > MAX_ITERS_WITHOUT_PROGRESS {
            bail!(
                "seed {seed}: scene made no virtual-time progress for \
                 {MAX_ITERS_WITHOUT_PROGRESS} consecutive iterations (total \
                 iterations {iter}); progress: virtual_elapsed={}us of {}us, \
                 events={}, frames_injected={next_frame_idx}/{}, \
                 session_ready={session_ready}, \
                 bytes_delivered={bytes_delivered}, bytes_dropped={bytes_dropped}.\n\
                 \n\
                 Unlike the flat iteration budget this replaces, reaching here \
                 does mean the loop is stuck: every iteration that moves the \
                 clock resets the counter, so a merely busy scene — however \
                 many iterations a virtual second costs it — cannot trip this. \
                 The clock is not moving, so look for something awaited that \
                 never becomes ready, or a `wake_at` that keeps landing at or \
                 before the current instant so `sleep_until` returns without \
                 advancing anything.",
                now_us(base),
                (overall_deadline - base).as_micros(),
                events.len(),
                frames.len(),
            );
        }
        if TokioInstant::now() >= overall_deadline {
            if session_ready {
                // Normal termination: the scene's declared time budget is
                // spent. This is expected once frames have been injected,
                // not a failure.
                break;
            }
            bail!(
                "seed {seed}: scene duration {:?} elapsed after {iter} iterations without \
                 SessionReady; last events observed: {events:?}",
                scene.duration
            );
        }

        // Set whenever something happened that can enable more work at this
        // same instant (a datagram moved, a timer fired, a frame went in): the
        // loop then goes round again before it lets the clock move.
        let mut moved = false;

        // Drain everything ClientNet currently has queued to transmit,
        // routing each datagram through the client->server NetSim.
        while let Some(out) = client.poll_transmit() {
            moved = true;
            let t = now_us(base);
            for item in rule(
                &mut net_c2s,
                Direction::C2s,
                out.payload.to_vec(),
                t,
                &mut bytes_dropped,
                &mut s2c_loss.wire,
            ) {
                if item.at_us <= t {
                    // Due now: hand it to the bridge's pipe. Nothing awaits
                    // here, so where the clock may move is decided in one
                    // place, at the bottom of this loop.
                    link.send(&item.payload, &client_addr).await.map_err(|e| {
                        anyhow!("seed {seed}: bridge send failed at iteration {iter}: {e}")
                    })?;
                    bytes_delivered += item.payload.len() as u64;
                    bytes_delivered_c2s += item.payload.len() as u64;
                } else {
                    in_flight.push(InFlight {
                        seq: next_in_flight_seq,
                        ..item
                    });
                    next_in_flight_seq += 1;
                }
            }
        }

        // The single point where datagrams actually arrive, in both
        // directions, once their scheduled time has come.
        moved |= flush_due(
            &mut in_flight,
            now_us(base),
            link,
            &mut client,
            client_addr,
            server_addr,
            &mut bytes_delivered,
            &mut bytes_delivered_s2c,
            &mut bytes_delivered_c2s,
            &mut datagrams_delivered_s2c,
        )
        .await
        .map_err(|e| anyhow!("seed {seed}: bridge send failed at iteration {iter}: {e}"))?;

        let new_events = client.take_events();
        for ev in &new_events {
            if let ClientNetEvent::Core(CoreEvent::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba,
            }) = ev
            {
                framebuffer.apply_tile_ready(*frame_seq, *tile_x, *tile_y, rgba.clone());
            }
        }
        events.extend(new_events);

        if !session_ready && events.contains(&ClientNetEvent::SessionReady) {
            session_ready = true;
            session_ready_at_us = now_us(base);
            if frames.is_empty() {
                // Nothing to inject: preserve task 15a's exact behavior of
                // returning the instant the session is ready.
                break;
            }
        }

        // Injection starts only once the server has a live (HELLO'd)
        // session. The client reports `SessionReady` before its HELLO has
        // crossed the netsim, and the server only honours the HELLO'd
        // session: injecting earlier would have `apply_injected_frame` see
        // no live session and drop the frame.
        //
        // The watch is written by the bridge when it processes the HELLO. It
        // used to run on its own task, so the iteration at which this loop
        // first SAW the count was not deterministic, and using the moment of
        // observation as the injection time made two same-seed runs inject at
        // different virtual instants. The bridge is stepped from this loop
        // now, so the observation point is reproducible, but the injection
        // instant stays fixed at `SessionReady + HELLO_GRACE_US` and the watch
        // only gates whether that instant has been reached in a state where
        // injection can succeed: the scene's timing does not depend on how
        // many turns the HELLO took.
        const HELLO_GRACE_US: u64 = 50_000;
        if session_ready
            && next_inject_at_us.is_none()
            && now_us(base) >= session_ready_at_us + HELLO_GRACE_US
            && *server_live_rx.borrow() >= 1
        {
            next_inject_at_us = Some(session_ready_at_us + HELLO_GRACE_US);
        }

        if session_ready {
            if let Some(due_at) = next_inject_at_us {
                if now_us(base) >= due_at {
                    let spacing_us;
                    if next_frame_idx < frames.len() {
                        inject_frame(
                            scene,
                            next_frame_idx,
                            &frames[next_frame_idx],
                            cadence_us,
                            &mut generations,
                            inject_tx,
                            opts.frame_cost,
                        )
                        .await
                        .map_err(|e| {
                            anyhow!("seed {seed}: frame {next_frame_idx} injection failed: {e}")
                        })?;
                        next_frame_idx += 1;
                        spacing_us = cadence_us;
                    } else {
                        // the frame list is exhausted, but `scene.duration`
                        // may still have plenty of virtual time left, and a
                        // datagram dropped on its one and only send attempt
                        // needs *something* to keep giving it a chance to
                        // land. `IoBridge::pump_admission` (which serves the
                        // server-side RTO wheel that actually resends a
                        // dropped, unacked tile datagram) runs after every
                        // loop turn, but a turn needs an event to happen at
                        // all, and nothing wakes the loop for an RTO
                        // deadline. In production the capture loop
                        // free-runs for as long as a client stays connected
                        // (regardless of screen dirtiness), so there is
                        // always a turn soon. This harness has no such
                        // free-running capture loop: once the frame list
                        // runs out, nothing would otherwise ever call
                        // `apply_injected_frame` again. Sending an
                        // empty-work `InjectedFrame` on the coarser
                        // `HEARTBEAT_SPACING_US` cadence (see its doc
                        // comment for why heartbeats don't reuse
                        // `FRAME_SPACING_US`) has no tile content to
                        // enqueue, but still reaches `apply_injected_frame`'s
                        // admission pass -- this is the harness's substitute
                        // for "a client stays connected and capture keeps
                        // ticking", not a new behavior IoBridge doesn't
                        // already have.
                        inject_heartbeat(heartbeat_seq, cadence_us, inject_tx)
                            .await
                            .map_err(|e| {
                                anyhow!(
                                    "seed {seed}: heartbeat {heartbeat_seq} injection failed: {e}"
                                )
                            })?;
                        heartbeat_seq += 1;
                        spacing_us = HEARTBEAT_SPACING_US;
                    }
                    next_inject_at_us = Some(due_at + spacing_us);
                    moved = true;
                }
            }
        }

        // Stepped: one turn of the server's loop for everything now in its
        // pipe and its channel, run to the point where it has nothing ready.
        // This is the production loop body, stepped here rather than spawned,
        // so what it does at this instant is a function of what was put in
        // front of it. Concurrent: the bridge is its own task and has been
        // running since it was spawned; there is nothing to step.
        if let Some(bridge) = bridge.as_deref_mut() {
            bridge
                .step_until_idle()
                .map_err(|e| anyhow!("seed {seed}: bridge failed at iteration {iter}: {e}"))?;
        }

        // Everything the bridge wrote, through the server->client netsim.
        let stepped_out = if stepped {
            link.recv_all().map_err(|e| {
                anyhow!("seed {seed}: bridge output unreadable at iteration {iter}: {e}")
            })?
        } else {
            Vec::new()
        };
        for pkt in stepped_out {
            moved = true;
            handle_s2c!(pkt);
        }

        // Fire an already-due ClientNet timeout, then look again from the top.
        let mut timer_fired = false;
        if let Some(deadline_us) = client.poll_timeout() {
            if now_us(base) >= deadline_us {
                client.on_timeout(now_us(base));
                moved = true;
                timer_fired = true;
            }
        }
        // Concurrent mode keeps the pre-stepping loop shape exactly: only a
        // fired timer restarts the iteration, because every other path goes
        // through the select below, which is where the runtime gets to run
        // the bridge task.
        if (stepped && moved) || timer_fired {
            continue;
        }

        // Nothing can happen at this instant. Sleep the clock to the earliest
        // moment anything in the scene needs attention: ClientNet's timer, the
        // next datagram in flight, the next scheduled injection, the bridge's
        // own next deadline (QUIC timer, frame hold), or the scene's end.
        let mut wake_at = overall_deadline;
        if let Some(d) = client.poll_timeout() {
            wake_at = wake_at.min(base + Duration::from_micros(d));
        }
        if let Some(next) = in_flight.peek() {
            wake_at = wake_at.min(base + Duration::from_micros(next.at_us));
        }
        if session_ready {
            // Unconditional once `session_ready`: heartbeats keep being
            // scheduled for the rest of `scene.duration`, not just until the
            // frame list runs out.
            if let Some(due_at) = next_inject_at_us {
                wake_at = wake_at.min(base + Duration::from_micros(due_at));
            }
        }
        if let Some(bridge) = bridge.as_deref_mut() {
            if let Some(at) = bridge.next_wake_for_harness() {
                wake_at = wake_at.min(TokioInstant::from_std(at));
            }
            tokio::time::sleep_until(wake_at).await;
            continue;
        }

        // Concurrent mode from here: wait for the bridge task's next packet
        // or the clock, whichever is first.
        //
        // Let the clock move before serving the pump again. The `select!`
        // below is biased, so a pump with a standing backlog would starve its
        // timer arm, and only the timer arm advances virtual time.
        // `MAX_CONSECUTIVE_PUMP_SERVES` bounds that: 512 sits above the
        // deepest run observed on a healthy sweep (192), far below the 5000
        // non-advancing iterations that trip the progress guard. Sleeping to
        // `wake_at` is what the loop would do anyway once the pump ran dry.
        const MAX_CONSECUTIVE_PUMP_SERVES: usize = 512;
        if consecutive_pump_serves >= MAX_CONSECUTIVE_PUMP_SERVES {
            consecutive_pump_serves = 0;
            tokio::time::sleep_until(wake_at).await;
            continue;
        }
        tokio::select! {
            biased;
            recv_res = link.recv_one() => {
                consecutive_pump_serves += 1;
                let pkt = recv_res.map_err(|e| anyhow!(
                    "seed {seed}: socketpair pump recv failed at iteration {iter} \
                     (last events observed: {events:?}): {e}"
                ))?;
                handle_s2c!(pkt);
            }
            _ = tokio::time::sleep_until(wake_at) => {
                consecutive_pump_serves = 0;
                if now_us(base) >= client.poll_timeout().unwrap_or(u64::MAX) {
                    client.on_timeout(now_us(base));
                }
            }
        }
    }

    let client_probe = ClientProbe {
        wire_loss: client.wire_loss_stats(),
        rx_datagrams: client.datagram_rx_counts(),
    };
    Ok((
        events,
        bytes_delivered,
        bytes_delivered_s2c,
        bytes_delivered_c2s,
        datagrams_delivered_s2c,
        bytes_dropped,
        client_probe,
    ))
}

/// What the client measured about its own receive path, read when the scene
/// ends. Measurement only.
#[derive(Debug, Clone, Copy, Default)]
pub struct ClientProbe {
    /// Pre-FEC loss, loss-run histogram and FEC recoveries from `wire_seq` gaps.
    pub wire_loss: ghostframe_client_core::WireLossStats,
    /// `(datagram frames quinn received, frames read by the application)`.
    /// The difference is what quinn's receive buffer dropped.
    pub rx_datagrams: Option<(u64, u64)>,
}

/// Encode and inject one `FrameScript`'s tiles as an `InjectedFrame`.
///
/// `generations` tracks, per tile coordinate, the generation to use next —
/// incremented (wrapping mod 16, since `generation` is a 4-bit wire field)
/// every time that specific coordinate is encoded, so a tile re-declared
/// across up to 16 frames gets a genuinely advancing generation. A scene
/// that rewrites any single tile coordinate more than 16 times will wrap
/// and reuse a generation value; no scene in this harness does that today.
///
/// A tile coordinate outside the scene's fixed `grid_cols`/`grid_rows` is a
/// scene-authoring bug, not a runtime condition to route around: the grid
/// is sized once at construction (see `BrowserlessScene::grid_cols`'s doc
/// comment on why `Scheduler::resize` is never called mid-scene), so this
/// fails loudly naming the offending coordinate instead of silently
/// dropping the tile the way `IoBridge::apply_injected_frame` does for a
/// mis-sized harness grid.
async fn inject_frame(
    scene: &BrowserlessScene,
    frame_idx: usize,
    script: &FrameScript,
    cadence_us: u64,
    generations: &mut HashMap<(u8, u8), u8>,
    inject_tx: &mpsc::Sender<InjectedFrame>,
    cost: Option<Duration>,
) -> anyhow::Result<()> {
    let seed = scene.seed;
    let mut work = Vec::new();

    for ((tile_x, tile_y), spec) in &script.tiles {
        if (*tile_x as u32) >= scene.grid_cols || (*tile_y as u32) >= scene.grid_rows {
            bail!(
                "seed {seed}: scene frame {frame_idx} declares tile ({tile_x}, {tile_y}) \
                 outside the {}x{} grid",
                scene.grid_cols,
                scene.grid_rows
            );
        }

        let counter = generations.entry((*tile_x, *tile_y)).or_insert(0u8);
        let generation = *counter;
        *counter = (*counter + 1) % 16;

        work.extend(encode_tile(spec, *tile_x, *tile_y, generation));
    }

    let frame = InjectedFrame {
        seq: frame_idx as u32,
        timestamp_us: (frame_idx as u64 * cadence_us) as u32,
        // No `budget_bytes` here: `IoBridge::apply_injected_frame` derives
        // its own per-tick budget the same way the production capture path
        // does (`IoBridge::base_budget_bytes`), rather than accepting one
        // from the caller. The netsim's token bucket still does the link
        // capping, and `IoBridge` still holds quinn's queue to its depth
        // gate (`admission`) — this only removes the harness's own
        // unbounded "drain everything" budget that sat in front of both.
        work,
        cost,
    };

    // Bounded: a send that cannot complete (the bridge is not draining the
    // channel) must fail the scene, not park the paused runtime forever.
    tokio::time::timeout(Duration::from_secs(30), inject_tx.send(frame))
        .await
        .map_err(|_| {
            anyhow!(
                "seed {seed}: inject_tx send blocked 30 s at frame {frame_idx}: the injection \
                 channel is full and the bridge is not draining it (frame cost longer than the \
                 cadence?)"
            )
        })?
        .map_err(|e| anyhow!("seed {seed}: inject_tx send failed at frame {frame_idx}: {e}"))
}

/// Send an empty-work `InjectedFrame` — no tiles, nothing new to encode —
/// purely to reach `IoBridge::apply_injected_frame`'s unconditional
/// admission pass (`pump_admission`). See the doc comment at the
/// `drive_session` call site for why this is needed once `scene.frames` is
/// exhausted but `scene.duration` still has virtual time left: without
/// something to keep calling into `IoBridge`, a datagram dropped on its one
/// and only send attempt is never retried, since nothing else in this
/// harness (no free-running capture loop, unlike production) would ever
/// call `apply_injected_frame` again.
///
/// `seq` must be distinct from every real scene frame's `seq` (the caller
/// guarantees this by starting `heartbeat_seq` at `scene.frames.len()`) —
/// otherwise this would collide with a real frame's wire `frame_seq` in
/// `IoBridge`'s per-tile-pass ACK/NACK/coverage bookkeeping, which is keyed
/// by `frame_seq`.
async fn inject_heartbeat(
    seq: u32,
    cadence_us: u64,
    inject_tx: &mpsc::Sender<InjectedFrame>,
) -> anyhow::Result<()> {
    let frame = InjectedFrame {
        seq,
        timestamp_us: (seq as u64 * cadence_us) as u32,
        work: Vec::new(),
        cost: None,
    };
    inject_tx
        .send(frame)
        .await
        .map_err(|e| anyhow!("heartbeat {seq}: inject_tx send failed: {e}"))
}

/// Current virtual time, in microseconds since `base`.
fn now_us(base: TokioInstant) -> u64 {
    TokioInstant::now()
        .saturating_duration_since(base)
        .as_micros() as u64
}

/// Flip bit `bit_index` (as `NetSim::decide` numbers them: bit 0 is the
/// LSB of byte 0) in `payload`, in place. A `bit_index` past the end of
/// `payload` is a no-op rather than a panic — `payload` here is always the
/// exact buffer the corrupt verdict was computed against, so this only
/// guards against a future caller passing a mismatched buffer.
fn flip_bit(payload: &mut [u8], bit_index: usize) {
    let byte_idx = bit_index / 8;
    if let Some(byte) = payload.get_mut(byte_idx) {
        *byte ^= 1 << (bit_index % 8);
    }
}

/// Which way a queued datagram is travelling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Direction {
    /// Client -> server: written onto the socketpair pump for `IoBridge`.
    C2s,
    /// Server -> client: fed into `ClientNet::handle_udp`.
    S2c,
}

/// A datagram `NetSim` has ruled on, waiting for its delivery time.
///
/// Ordered by `(at_us, seq)` and reversed, so the `BinaryHeap` holding
/// these — a max-heap — yields the *earliest* arrival first. `seq` is a
/// monotonic per-scene counter that breaks ties between datagrams sharing
/// an `at_us`, keeping their relative order the one they were queued in
/// rather than an arbitrary heap order.
#[derive(Clone, PartialEq, Eq, Debug)]
struct InFlight {
    at_us: u64,
    seq: u64,
    dir: Direction,
    payload: Vec<u8>,
}

impl Ord for InFlight {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .at_us
            .cmp(&self.at_us)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for InFlight {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Run one datagram through `sim` and return whatever survives, as zero,
/// one, or two arrivals with the times the verdict assigns them.
///
/// Deliberately decides nothing about *when* delivery happens: the caller
/// delivers an already-due arrival immediately and queues a future one. That
/// split matters. An earlier version pushed every arrival onto the queue and
/// flushed them together at the top of the loop, which at `delay_us: 0` —
/// where every `at_us` equals the moment it was ruled on — moved every send
/// out of the drain loop and into a batch. Under `start_paused` the virtual
/// clock advances only when the runtime goes idle, so relocating those yields
/// changed when time advanced: the demand-starved scene intermittently burned
/// all 5,000 iterations of the old flat budget in ~1.2 virtual seconds
/// where it needs ~350 for
/// the full 10. Keeping the due path free of the queue keeps a zero-delay
/// scene behaving exactly as it did before propagation delay was modelled.
fn rule(
    sim: &mut NetSim,
    dir: Direction,
    payload: Vec<u8>,
    now_us_at_send: u64,
    bytes_dropped: &mut u64,
    wire: &mut WireDigest,
) -> Vec<InFlight> {
    wire.offered(dir, now_us_at_send, &payload);
    let items = rule_inner(sim, dir, payload, now_us_at_send, bytes_dropped);
    wire.ruled(&items);
    items
}

fn rule_inner(
    sim: &mut NetSim,
    dir: Direction,
    payload: Vec<u8>,
    now_us_at_send: u64,
    bytes_dropped: &mut u64,
) -> Vec<InFlight> {
    // `seq` is a placeholder here; the caller assigns a real one if and when
    // it queues the arrival, so queued arrivals stay ordered by insertion.
    let at = |at_us: u64, payload: Vec<u8>| InFlight {
        at_us,
        seq: 0,
        dir,
        payload,
    };

    match sim.decide(payload.len(), now_us_at_send) {
        Verdict::Drop => {
            *bytes_dropped += payload.len() as u64;
            Vec::new()
        }
        Verdict::Deliver { at_us } => vec![at(at_us, payload)],
        Verdict::Duplicate { at_us, dup_at_us } => {
            vec![at(at_us, payload.clone()), at(dup_at_us, payload)]
        }
        Verdict::Corrupt { at_us, bit_index } => {
            // Corruption flips a bit in the payload itself, never in the
            // ghostbridge framing `pump.send` adds on top.
            let mut corrupted = payload;
            flip_bit(&mut corrupted, bit_index);
            vec![at(at_us, corrupted)]
        }
    }
}

/// Deliver every queued datagram whose arrival time has come, in arrival
/// order across both directions.
///
/// Called once per scene-loop iteration. `now` selects which datagrams are
/// due; each one is then handed to `ClientNet::handle_udp` stamped with its
/// *own* `at_us`, not with `now`. Those differ whenever more than one
/// datagram comes due in the same pass, which is most of the time — and
/// stamping them all with `now` would erase the arrival spread the
/// bottleneck model exists to produce.
#[allow(clippy::too_many_arguments)]
async fn flush_due(
    in_flight: &mut BinaryHeap<InFlight>,
    now: u64,
    link: &mut BridgeLink,
    client: &mut ClientNet,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    bytes_delivered: &mut u64,
    bytes_delivered_s2c: &mut u64,
    bytes_delivered_c2s: &mut u64,
    datagrams_delivered_s2c: &mut u64,
) -> std::io::Result<bool> {
    let mut any = false;
    while in_flight.peek().is_some_and(|f| f.at_us <= now) {
        any = true;
        let f = in_flight.pop().expect("peek just confirmed a due datagram");
        match f.dir {
            Direction::C2s => link.send(&f.payload, &client_addr).await?,
            // Stamp each datagram with *its own* arrival time, not the
            // loop's. `flush_due` drains everything already due in one pass,
            // so using `now` collapsed a whole batch's arrival spread to a
            // single instant -- discarding exactly the per-datagram
            // serialisation delay the bottleneck model computes into
            // `at_us`. Invisible at millisecond resolution; at microsecond
            // resolution it made receive intervals ~5x shorter than the
            // matching send intervals, and goog_cc rejected every probe
            // cluster as `receive/send ratio too high`.
            Direction::S2c => client.handle_udp(&f.payload, server_addr, f.at_us),
        }
        *bytes_delivered += f.payload.len() as u64;
        match f.dir {
            Direction::C2s => *bytes_delivered_c2s += f.payload.len() as u64,
            Direction::S2c => {
                *bytes_delivered_s2c += f.payload.len() as u64;
                *datagrams_delivered_s2c += 1;
            }
        }
    }
    Ok(any)
}

/// The harness's end of the link to the bridge. Either the in-memory pipe the
/// stepped harness uses, or the real socketpair of the concurrent one; both
/// speak ghostbridge's real framing so the bridge's reader sees exactly the
/// bytes it would from a socket.
enum LinkEnd {
    Mem(MemStream),
    Pump(SocketPairPump),
}

struct BridgeLink {
    end: LinkEnd,
    /// Tokio-clock instant of every packet handed to the bridge; paired with
    /// the bridge's own read log for the inbound-service-delay metric.
    send_log: std::sync::Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
}

impl BridgeLink {
    /// Frame `payload` and hand it to the bridge. In stepped mode one whole
    /// frame is pushed and nothing awaits, so the bridge never sees half of
    /// one and the clock cannot move here; in concurrent mode this is a
    /// socket write that can yield.
    async fn send(&mut self, payload: &[u8], addr: &SocketAddr) -> std::io::Result<()> {
        match &mut self.end {
            LinkEnd::Mem(m) => {
                self.send_log
                    .lock()
                    .expect("send_log mutex poisoned")
                    .push(TokioInstant::now().into_std());
                m.push(&encode_frame(payload, addr));
                Ok(())
            }
            // The pump logs its own writes (`with_send_log`).
            LinkEnd::Pump(p) => p.send(payload, addr).await,
        }
    }

    /// Everything the bridge has written since the last call, parsed.
    /// Stepped mode only.
    fn recv_all(
        &mut self,
    ) -> std::io::Result<Vec<ghostframe_lib::transport::ghostbridge::UdpPacket>> {
        match &mut self.end {
            LinkEnd::Mem(m) => parse_frames(&m.take_all()),
            LinkEnd::Pump(_) => unreachable!("recv_all is stepped-mode only"),
        }
    }

    /// The next packet the bridge task writes. Concurrent mode only.
    async fn recv_one(
        &mut self,
    ) -> std::io::Result<ghostframe_lib::transport::ghostbridge::UdpPacket> {
        match &mut self.end {
            LinkEnd::Pump(p) => p.recv().await,
            LinkEnd::Mem(_) => unreachable!("recv_one is concurrent-mode only"),
        }
    }
}

#[cfg(test)]
mod wire_trace_tests {
    use super::*;

    fn trace(n: usize, perturb: Option<(usize, usize)>) -> WireTrace {
        let mut w = WireDigest::default();
        for i in 0..n {
            let len = match perturb {
                Some((at, to)) if at == i => to,
                _ => 100 + i % 7,
            };
            let dir = if i % 3 == 0 {
                Direction::C2s
            } else {
                Direction::S2c
            };
            w.offered(dir, i as u64 * 1000, &vec![0u8; len]);
            w.ruled(&[]);
        }
        w.into_trace()
    }

    #[test]
    fn a_perturbed_datagram_is_named_by_index() {
        let a = trace(300, None);
        assert!(a.first_divergence(&trace(300, None)).is_none());
        let b = trace(300, Some((137, 999)));
        let report = a.first_divergence(&b).expect("must diverge");
        assert!(report.contains("index 137"), "{report}");
        assert!(report.contains("t=137000us"), "{report}");
        assert!(report.contains("len=999"), "{report}");
        // Survives the file round trip the child process uses.
        let b2 = WireTrace::from_text(&b.to_text());
        assert_eq!(b2, b);
    }

    #[test]
    fn a_divergence_past_the_raw_records_names_its_window() {
        let n = WIRE_TRACE_RECORDS + 500;
        let at = WIRE_TRACE_RECORDS + 130;
        let report = trace(n, None)
            .first_divergence(&trace(n, Some((at, 5))))
            .expect("must diverge");
        let w = at / WIRE_TRACE_WINDOW * WIRE_TRACE_WINDOW;
        assert!(report.contains(&format!("datagrams {w}..")), "{report}");
    }

    #[test]
    fn memory_stays_bounded() {
        let t = trace(WIRE_TRACE_RECORDS * 3, None);
        assert_eq!(t.records.len(), WIRE_TRACE_RECORDS);
        assert_eq!(t.windows.len(), WIRE_TRACE_RECORDS * 3 / WIRE_TRACE_WINDOW);
    }
}
