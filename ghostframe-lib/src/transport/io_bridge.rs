//! Tokio async event loop bridging ghostbridge and quinn-proto.
//!
//! `IoBridge` wraps a ghostbridge socketpair fd (as a tokio `UnixStream`) and
//! drives a `QuicServer` state machine:
//!
//! ```text
//! select!:
//!   - read_exact(header) from UnixStream → parse frame → endpoint.handle → drain
//!   - sleep_until(next_timeout)           → handle_timeout               → drain
//! drain:
//!   - while let Some(t) = poll_transmit { write_all(encode_frame(t)) }
//!   - poll_events → log (Task 6/8 add real handling)
//! ```

use std::future::{pending, Future};
use std::io;
use std::net::SocketAddr;
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::pin::Pin;

use bytes::BytesMut;
use quinn_proto::{ConnectionHandle, Event, StreamEvent};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio::time::{sleep_until, Instant as TokioInstant};

use rayon::prelude::*;
use smallvec::smallvec;

use crate::capture::gpu_pipeline::GpuFrameProcessor;
use crate::encoder::h264_vaapi::FullFrameEncoder;
use crate::server::FrameSubmission;
use crate::tile::{DirtyTracker, TileGrid};
use crate::transport::fec;
use crate::transport::fec::fec_group_size;
use crate::transport::feedback::ReceiverFeedback;
use crate::transport::ghostbridge::{
    encode_frame, parse_frame_rest, GhostbridgeConfig, GhostbridgeHandle, MAX_FRAME_LEN,
};
use crate::transport::protocol::{
    build_frame_parity_datagram, fragment_frame, fragment_tile, max_fec_frame_fragment_payload,
    max_fragment_payload, Codec, FrameHeader, NackMessage, TileFragmentInputs, FRAME_HEADER_SIZE,
    PING_PAYLOAD, PONG_PAYLOAD, TILE_DATAGRAM_FLAG,
};
use crate::transport::quic::QuicServer;
use crate::transport::webtransport::WebTransportServer;
use ghostframe_protocol::eviction::{build_eviction_datagram, EvictionReason};

/// Tracing target the moved-out code keeps logging under: the child modules
/// would otherwise report `module_path!()` of the child.
const LOG_TARGET: &str = "ghostframe_lib::transport::io_bridge";

#[path = "io_bridge/display_mode.rs"]
mod display_mode;
use display_mode::{clamp_and_align_mode, PendingDisplayMode, DISPLAY_MODE_DEBOUNCE_US};
#[cfg(test)]
use display_mode::{MIN_MODE_HEIGHT, MIN_MODE_WIDTH};

#[path = "io_bridge/session.rs"]
mod session;
use session::{CloseDeadline, DEFAULT_SCALE_MILLI};
#[cfg(test)]
use session::{
    CLOSE_CODE_EVICTED, CLOSE_CODE_NO_HELLO, CLOSE_CODE_TAKEOVER_RATE_LIMITED, EVICT_CLOSE_GRACE,
    HELLO_DEADLINE, TAKEOVER_MIN_INTERVAL,
};

#[path = "io_bridge/ack_dispatch.rs"]
mod ack_dispatch;
pub(crate) use ack_dispatch::handle_datagram_payload;

#[path = "io_bridge/bwe_glue.rs"]
mod bwe_glue;
pub use bwe_glue::TierLatencyStats;
use bwe_glue::{
    pass_tier, ActiveProbe, BweSample, PassTier, BWE_SAMPLES_BUFFER_CAPACITY, MAX_PENDING_LOSSES,
    TRANSMISSION_LEDGER_CAPACITY,
};
#[cfg(test)]
use bwe_glue::{select_live_rtt, LOSS_HORIZON_CEIL, LOSS_HORIZON_FLOOR, LOSS_HORIZON_RTTS};

#[path = "io_bridge/pacing.rs"]
mod pacing;
pub(crate) use pacing::SCHEDULER_TICK_INTERVAL_US;
pub use pacing::{combine_pacing_budget, pacer_tick_budget_bytes, select_pacing_mode, PacingMode};
#[cfg(test)]
use pacing::{BWE_PACED_MODE_SAMPLE_THRESHOLD, QUINN_SEND_BUFFER_SAFETY_FRACTION};
use pacing::{
    SCHEDULER_BUDGET_BACKOFF, SCHEDULER_BUDGET_MULT_MAX, SCHEDULER_BUDGET_MULT_MIN,
    SCHEDULER_BUDGET_RAMP,
};

#[path = "io_bridge/frame.rs"]
mod frame;
#[cfg(test)]
use frame::{clamp_gpu_tile_count, monotonic_now_ns, populate_gpu_metrics, PalRleTileWorkPrep};

// ---------------------------------------------------------------------------
// Clock discipline
// ---------------------------------------------------------------------------
// This file has exactly two deliberate exceptions to "always call
// now_std()", each measuring something that must keep moving even while the
// browserless harness has tokio's clock paused. Everything else in the server
// loop goes through now_std():
//
//   1. The `process_frame` duration probe (`t0`/`dt_us`, ~line 1000 below) --
//      measures real CPU cost of encode work, not protocol/deadline time.
//   2. `monotonic_now_ns()` (below) -- reads `CLOCK_MONOTONIC` directly and
//      is paired *offline*, after the fact, with xdaemon's `capture_done_ns`
//      timestamp from a separate process. Neither process shares the other's
//      tokio runtime, so there is no virtual clock to route through here.
//
// Any new timing probe you add should be `now_std()` unless it is
// measuring real wall/CPU time for something outside tokio's control (in
// which case: name it here too, and gate the `.elapsed()` call behind
// `#[allow(clippy::disallowed_methods, reason = "...")]` with a
// site-specific justification -- see clippy.toml at the workspace root).
// ---------------------------------------------------------------------------

/// Current time as a `std::time::Instant`, sourced from tokio's clock.
///
/// Every deadline the bridge computes flows from here, so
/// `tokio::time::pause()` controls the whole server loop: the browserless
/// harness advances virtual time and a 60-second scene costs its event count,
/// not its duration. In production tokio's clock is the system clock, so this
/// is exactly `Instant::now()`.
///
/// **Never call `.elapsed()` or `Instant::now() - x` on a value from here**
/// — both read the wall clock and saturate to `0ns` against a virtual
/// instant. Compute durations with `now_std().duration_since(earlier)`
/// instead, where `earlier` also came from `now_std()`.
///
/// `pub` under `cfg(test)` / `browserless-harness` (mirroring
/// `new_with_stream_for_test`) so the harness in `ghostframe-e2e` can stamp
/// its own timestamps on tokio's clock; `pub(crate)` otherwise, since
/// production code only ever needs this from within the crate.
#[cfg(any(test, feature = "browserless-harness"))]
pub fn now_std() -> std::time::Instant {
    now_std_impl()
}

/// See the `pub` variant's doc comment above (the two are cfg-gated
/// alternatives of the same function, not a public/private pair with
/// different behavior).
#[cfg(not(any(test, feature = "browserless-harness")))]
pub(crate) fn now_std() -> std::time::Instant {
    now_std_impl()
}

fn now_std_impl() -> std::time::Instant {
    tokio::time::Instant::now().into_std()
}

/// Scratch buffer size for quinn-proto's `endpoint.handle` / `conn.poll_transmit`.
/// Sized above a 1500-byte MTU with headroom for ECN/padding; quinn-proto may
/// write a full datagram into this buffer in a single call.
const QUIC_SCRATCH: usize = 2048;

/// `DatagramSender` impl that forwards every emit to
/// `IoBridge::send_to_all_sessions` via a raw pointer.
///
/// SAFETY: This adapter is constructed inside `&mut self` methods on
/// `IoBridge` (`drain_scheduler_into_quinn`, `process_frame_gpu`) and
/// lives only for the duration of the enclosing call. The pointer
/// targets the same `IoBridge` whose `&mut self` borrow created it, and
/// `ReliableTileEmitter::drain` is single-threaded — there is no
/// re-entrant or concurrent access to the bridge while the adapter is
/// live. The pointer is never stored, never shared across threads, and
/// never outlives the stack frame.
///
/// The raw-pointer pattern is the cleanest way to thread
/// `send_to_all_sessions` (which needs `&mut self`) into
/// `emitter.drain` while the emitter itself is already borrowed
/// `&mut self.reliable_emitter`.
struct IoBridgeSenderAdapter {
    bridge: *mut IoBridge,
}

impl crate::transport::reliable_emitter::traits::DatagramSender for IoBridgeSenderAdapter {
    fn send(&mut self, dg: &[u8]) -> crate::transport::reliable_emitter::traits::SendOutcome {
        // SAFETY: see struct doc — the pointer is valid for the duration
        // of the surrounding `&mut self` call on `IoBridge`.
        unsafe { (*self.bridge).send_to_all_sessions(dg) }
    }
}

// Period (in frames) between cumulative-emit log lines. 60 frames ≈ 2 s
// at the daemon's 30 fps capture rate, which is the same cadence as
// the client's periodic `stats:` log — so the two lines line up
// naturally for side-by-side delta inspection.
const CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES: u32 = 60;

/// Inbound packets the event loop will take back to back before the frame
/// and injection arms get a turn regardless.
///
/// Inbound is polled ahead of frames (see the `select!` in `IoBridge::run`),
/// which is safe only if it cannot starve them in turn. A packet costs tens
/// of microseconds and a client's send rate is bounded by its own
/// acknowledgement traffic, so in practice this never binds; it is here so
/// that a peer sending as fast as it can gets a slow session, not a frozen
/// one.
const INBOUND_STREAK_LIMIT: u32 = 64;

// ---------------------------------------------------------------------------
// M3.5 bench telemetry: per-frame send-side stats
// ---------------------------------------------------------------------------

/// Per-codec send counters accumulated during a frame's tile dispatch.
/// Counts tiles (not fragments): a multi-fragment Cdf53 tile increments
/// `cdf53` by 1, regardless of the number of QUIC datagrams emitted.
#[derive(Default, Clone, Copy)]
struct CodecHistogram {
    solid: u16,
    palrle: u16,
    cdf53: u16,
    raw: u16,
    h264: u8,
}

/// Accumulator filled during `dispatch_dirty_tiles_via_scheduler` (and the
/// H264 branch of `process_frame_gpu`). Returned so callers can emit a single
/// `frame.last_send` event after the last datagram is handed to QUIC.
#[derive(Default)]
pub(crate) struct FrameSendStats {
    total_wire_bytes: u32,
    tile_count: u16,
    codec_histogram: CodecHistogram,
}

// ---------------------------------------------------------------------------

/// FEC parity defaults — enable when loss exceeds this threshold.
const FEC_ENABLE_THRESHOLD: f64 = 0.005;
/// FEC parity defaults — disable when loss drops below this threshold (hysteresis).
const FEC_DISABLE_THRESHOLD: f64 = 0.002;
/// Number of frames the server re-emits the frame-dimensions datagram after
/// any dimension change. At 5% packet loss, probability of all 10 being lost
/// is 0.05^10 ≈ 9.7e-14.
const FRAME_DIMENSIONS_RETRANSMITS: u8 = 10;

/// One scene frame's worth of pre-encoded tile work, injected by the
/// browserless harness in place of capture + classification.
///
/// Carries frame identity because enqueueing alone emits nothing: the arm
/// must also call `drain_scheduler_into_quinn`, which tags outbound
/// fragments with `seq` / `timestamp_us`.
#[derive(Debug, Clone)]
pub struct InjectedFrame {
    pub seq: u32,
    pub timestamp_us: u32,
    pub work: Vec<crate::transport::scheduler::TileWork>,
}

/// An arm of the `select!` in `IoBridge::run`. See `IoBridge::loop_arm_trace`.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LoopArm {
    Timeout,
    Inbound,
    Frame,
    Injected,
}

pub struct IoBridge {
    /// Keep the ghostbridge handle alive so the socketpair fd stays open.
    /// `None` only in the test-only constructor, which builds directly from a
    /// `UnixStream::pair()` and has no tsnet node to own.
    _handle: Option<GhostbridgeHandle>,
    stream: TokioUnixStream,
    server: QuicServer,
    /// The local address of the UDP listener; passed as `local_ip` to
    /// quinn-proto.  M0: we only bind a single port so this is constant.
    local_addr: SocketAddr,
    /// Per-connection WebTransport handshake state.
    wt_sessions: HashMap<ConnectionHandle, WebTransportServer>,
    /// Which session most recently sent HELLO. `None` until the first one
    /// arrives. Note this does not make `client_caps` per-session — those
    /// remain last-writer-wins; this field only names that writer, which is
    /// what a later M4a task needs to tell an incumbent from a newcomer.
    last_hello_from: Option<ConnectionHandle>,
    /// Connections that must be closed at a deadline: either a fresh
    /// connection that has not sent HELLO yet ([`HELLO_DEADLINE`]), or an
    /// evicted session given [`EVICT_CLOSE_GRACE`] to flush its notice.
    /// Swept by `sweep_close_deadlines`. Cleared per handle by
    /// `forget_session` (eviction re-inserts its entry afterwards).
    close_deadlines: HashMap<ConnectionHandle, CloseDeadline>,
    /// Feedback-stream buffers discarded because the sender had not been
    /// HELLO'd (lifetime count; rate-limited warn).
    non_live_feedback_drops: u64,
    /// Inbound datagrams dropped because the sender was not the live
    /// session (lifetime count; rate-limited warn).
    non_live_datagram_drops: u64,
    /// Tracks whether ANY client has previously been the live (HELLO'd)
    /// session on THIS `IoBridge` instance. Initial value `false`. Flips to
    /// `true` permanently at the first HELLO takeover. Used to skip the reset body on the
    /// FIRST connect (where dirty_tracker / classifier / scheduler /
    /// etc. are in their initial-startup state and the test suite
    /// implicitly depends on that state surviving session
    /// establishment). Reconnects fire the reset body normally.
    ///
    /// **Lifecycle assumption**: this field is scoped to a single
    /// `IoBridge` instance. The single-server-process model used in
    /// production never recycles an IoBridge — each server boot
    /// constructs a fresh one. If a future API ever exposes an
    /// IoBridge::reset() (e.g. for config reload), this field MUST
    /// be reset to `false` there.
    has_seen_prior_session: bool,
    /// Inbound channel of captured frames to be fragmented and sent as datagrams.
    frame_rx: Option<mpsc::Receiver<FrameSubmission>>,
    /// Pre-encoded tile work injected by the browserless harness, bypassing
    /// capture and classification. `None` in production.
    inject_rx: Option<mpsc::Receiver<InjectedFrame>>,
    /// Monotonically increasing frame sequence number (wrapping).
    frame_seq: u32,
    /// Per-tile dirty detection.
    dirty_tracker: DirtyTracker,
    /// Per-tile metrics fed to the classifier each frame.
    metrics_tracker: crate::tile::MetricsTracker,
    /// Cost-aware frame-mode + per-tile classifier.
    classifier: crate::tile::Classifier,
    /// Reference instant (sourced from `now_std()`, so it tracks tokio's
    /// clock) that `decide_frame_mode_at`'s `now_us` is measured from. The
    /// classifier itself must stay clock-agnostic — it only ever sees a
    /// caller-supplied `now_us` — so this is where the bridge's own paused-
    /// clock discipline gets threaded through to the frame-mode hysteresis
    /// dwell timers (`enter_sustain_micros` / `exit_sustain_micros`).
    /// Stamped once at construction; never reset.
    epoch: std::time::Instant,
    /// Reference instant for `BweSample::server_emit_us`. Arbitrary but
    /// stable for the session — the estimator uses deltas only.
    bwe_epoch: std::time::Instant,
    /// Last-emitted frame mode (carried across frames for hysteresis).
    frame_mode: crate::tile::FrameMode,
    /// Round-robin tile-work scheduler shared by both CPU and GPU emission paths.
    scheduler: crate::transport::scheduler::Scheduler,
    /// Cfg-gated outbound-datagram loss injector for e2e tests.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) outbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    /// Cfg-gated inbound-datagram loss injector for e2e tests.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) inbound_loss: Option<crate::transport::loss_injection::LossInjector>,
    /// Cfg-gated outbound bandwidth cap for e2e tests / bench scenes.
    /// `None` ⇒ no rate limiting (the `GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP`
    /// env var was unset at construction time). Production code path is
    /// unaffected.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) outbound_bandwidth_cap: Option<crate::transport::bandwidth_cap::BandwidthCap>,
    /// Cfg-gated test hook: when `Some(value)`, `sample_all_path_stats`
    /// overrides `bytes_per_us` with this value instead of computing
    /// from quinn `cwnd / rtt`. Bypasses the slow QUIC bandwidth-discovery
    /// path so e2e tests can drive the M3.6b headroom_guard override
    /// deterministically.
    /// Sourced from `GHOSTFRAME_TEST_FORCE_BYTES_PER_US` env var
    /// (parsed as f32). Unset / 0 / unparseable ⇒ None (no override).
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) test_force_bytes_per_us: Option<f32>,
    /// Cfg-gated one-shot OOB-PalRle injection coordinate for e2e tests.
    /// When set to `Some((x, y))`, the next PalRle payload encoded for the
    /// matching tile coordinate is replaced with a hand-built bundled payload
    /// containing an out-of-bounds palette index. Cleared after firing once.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) oob_inject_at: Option<(u32, u32)>,
    /// Cfg-gated test hook: when true, the new-session handler preserves the
    /// `palette_table.delivered` bitset across the session reset (it still
    /// resets in_flight_carrying / ref_count / etc.). Drives the
    /// e2e_decode_error_thin_uncached round-trip by simulating a client
    /// that lost palette shadow without the server noticing.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) skip_palette_session_reset: bool,
    /// Cfg-gated deterministic drop plan for e2e tests: drops the Nth
    /// occurrence of a datagram carrying a named tile. Consulted in
    /// `send_to_all_sessions`, immediately after `outbound_loss` — this is
    /// the plaintext seam, the last point before QUIC encrypts the
    /// datagram, which is why the check lives here and not at the UDP/QUIC
    /// layer (see `drop_plan`'s module doc). `Arc<Mutex<_>>` because the
    /// harness needs to read the fired-count back out after `self` has been
    /// moved wholesale into a `spawn_local`'d task; see `set_drop_plan`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(crate) drop_plan:
        Option<std::sync::Arc<std::sync::Mutex<crate::transport::drop_plan::DropPlan>>>,

    /// Lower bound applied to `base_budget_bytes`, defaulting to
    /// `SCHEDULER_TICK_BUDGET_FLOOR_BYTES`.
    ///
    /// A field rather than the bare constant because the constant is not a
    /// neutral safety net: 256 KiB per 33.3 ms tick is 7.86 MB/s, so the
    /// floor only stops dominating above ~70 Mbit/s. Every link this system
    /// has been measured on is below that, which means the bandwidth-derived
    /// term has never once bound and the budget has been a constant. Varying
    /// the floor is how a test reaches the other side of that knee.
    tick_budget_floor_bytes: Option<usize>,

    /// Most recent result of `compute_max_datagram_size`, so the budget
    /// floor can be expressed in datagrams rather than in bytes.
    last_max_datagram_size: Option<usize>,
    /// Remaining frames to force all-dirty after a new session connects.
    /// QUIC slow-start can only deliver a fraction of tiles in the first burst;
    /// forcing dirty for several frames lets the congestion window open.
    force_dirty_frames: u32,
    /// Ticks since startup, for `STUCK_SWEEP_EVERY_TICKS`.
    stuck_sweep_counter: u32,
    /// Byte allowance for RTO and NACK replay. See `retransmit_pacer`.
    retransmit_pacer: crate::transport::retransmit_pacer::RetransmitPacer,
    /// Which `select!` arm each `run()` iteration took, in order. The arm
    /// *order* is the behaviour under test and leaves no other trace: with
    /// every source ready the loop drains them all inside one poll.
    #[cfg(test)]
    loop_arm_trace: Vec<LoopArm>,
    /// When the current session was established, for session-duration
    /// reporting on disconnect. `None` between sessions.
    session_started_at: Option<std::time::Instant>,
    /// When the last session ended, so the next connect can report how long
    /// the server was serving nobody. `None` before the first disconnect.
    last_disconnect_at: Option<std::time::Instant>,
    /// When a HELLO last displaced a live incumbent; see `TAKEOVER_MIN_INTERVAL`.
    last_takeover_at: Option<std::time::Instant>,
    /// Persistent palette table for PalRLE codec emission. M3.2a single-client
    /// invariant — flat server-wide state.
    pub(crate) palette_table: crate::encoder::pal_rle::PaletteTable,
    /// Capabilities advertised by the connected client via HELLO. Defaults to
    /// all-disabled until the client sends a HELLO message.
    client_caps: crate::transport::client_caps::ClientCapabilities,
    /// FEC parity group size. 0 = disabled.
    fec_k: usize,
    /// Loss rate threshold to enable FEC (0.005 = 0.5%).
    fec_enable_threshold: f64,
    /// Loss rate threshold to disable FEC (hysteresis).
    fec_disable_threshold: f64,
    /// Live signal vector pushed to `classifier.set_adaptation_context()`
    /// after every `dispatch_feedback_bytes` or `sample_quic_path_stats`.
    /// M3.6a wires it; M3.6b's policy reads the non-bytes_per_us fields.
    pub(crate) adaptation_context: crate::tile::classifier::AdaptationContext,
    /// Last two windows' `suspension_detected` flag, OR'd into
    /// `adaptation_context.suspended` to debounce single-window flaps.
    pub(crate) recent_suspension_flags: [bool; 2],
    /// M3.3c idle-escalation: flat tile indices from the most recent per-frame
    /// sweep. Stashed here so Task 10's post-fence Phase B can read the list
    /// and update `already_escalated_this_gen` after the GPU work returns.
    cdf53_escalation_candidates_this_frame: Vec<u32>,
    /// Phase 1.5-B stranded-CDF53 recovery: flat tile indices of tiles in
    /// `CodecState::Cdf53` whose ACK bitmap (`Scheduler::cdf53_unacked_pass_mask`)
    /// shows passes still unacked AND the retransmit cache holds no
    /// entries for them (RTO has nothing to fire — cache must have been
    /// drained by `cancel_for_tile` or `clear`, since with Phase 1.5-A v2
    /// the client only ACKs successfully-prevalidated passes).
    /// Detected per-frame just before `set_escalation_candidates`, fed
    /// into the same GPU coefficient pipeline, and dispatched by Phase B
    /// as the "stranded" branch (re-enqueue only the unacked passes at
    /// the EXISTING generation — no `bump_generation`, no
    /// `cancel_for_tile`).
    cdf53_stranded_candidates_this_frame: Vec<u32>,
    /// GPU-accelerated dirty tracker (Vulkan compute SAD).
    gpu_frame_processor: Option<GpuFrameProcessor>,
    /// Full-frame H.264 encoder (VA-API zero-copy).
    full_frame_encoder: Option<FullFrameEncoder>,
    /// Recent frame fragments for NACK retransmission. Key: (frame_seq, frag_idx).
    recent_frame_fragments: HashMap<(u32, u16), Vec<u8>>,
    /// Per-tile-pass coverage map: (frame_seq, tile_x, tile_y, pass_idx) ->
    /// tile metadata for a work item. Populated at emit time; consumed by
    /// dispatch_ack_datagram on incoming ACKs. LRU-bounded by
    /// FRAGMENT_COVERAGE_CAPACITY. Independent of `recent_frame_fragments`
    /// (different lifetime semantics: retransmit window is RTT-bounded;
    /// coverage retention is until-ACKed-or-evicted).
    fragment_coverage: crate::transport::fragment_coverage::FragmentCoverageMap,
    /// Times `resume_scheduler_continuation` actually drained work, and the
    /// bytes it moved.
    ///
    /// Separate from the per-frame `scheduler.tick` line, which counts only
    /// frame-driven drains. Without these the continuation path is
    /// unobservable: it was dead for the whole life of the code (its only
    /// trigger, `Event::DatagramsUnblocked`, cannot fire while the capacity
    /// clamp prevents `Blocked`) and nothing showed that.
    /// Republished copy of `datagram_send_errs`, for a harness that needs to
    /// read it back after the bridge has moved into a spawned task.
    ///
    /// `SendDatagramError::Blocked` is the signal this whole area turns on:
    /// quinn only emits `DatagramsUnblocked` after one, and
    /// `send_to_all_sessions` currently drops the rejected datagram. A test
    /// asserting anything about that path must be able to prove Blocked
    /// actually happened, or it passes on a scene that never provoked it.
    #[cfg(any(test, feature = "browserless-harness"))]
    send_errs_publish: std::sync::Arc<std::sync::Mutex<u64>>,
    continuation_resumes: u64,
    continuation_drained_bytes: u64,
    /// Inbound datagrams whose `InboundKind` has no route on this path.
    ///
    /// The routing match that feeds this used to end in `_ =>
    /// dispatch_ack_datagram`, so every unrecognised, empty, or
    /// wrong-direction datagram was handed to the ACK parser. Counting them
    /// instead keeps the miscount visible: a non-zero value here means
    /// something is arriving that nothing claims, which is worth knowing and
    /// was previously indistinguishable from a malformed ACK.
    unroutable_inbound_datagrams: u64,
    /// Last (width, height) the server emitted as a frame-dimensions datagram.
    /// `None` means we've never emitted dimensions (first frame upcoming).
    last_emitted_dimensions: Option<(u32, u32)>,
    /// Frames remaining in the current retransmit window for the dimensions
    /// message. Each dimension change resets this to N=10. While > 0, every
    /// frame re-emits the dimensions datagram; loss tolerance is 0.05^10 ≈ 1e-13.
    dimensions_retransmits_left: u8,
    /// Count of `wt_sessions` entries whose handshake is currently up,
    /// published to `GhostframeServer` so the capture loop in xdaemon can skip
    /// capture work when no consumer exists. Refreshed inside
    /// `compute_max_datagram_size` and in the run loop after handling events.
    ///
    /// A `watch` channel rather than an atomic so a capture loop can *park*
    /// on the 0 → non-zero transition instead of polling it. Polling meant the
    /// gate was only re-read once per poll interval, which put the whole
    /// interval between a client connecting and the first frame being
    /// scraped. `refresh_connected_session_count` notifies only on an actual
    /// change, so the per-frame refresh does not wake parked waiters.
    ///
    /// Dropping this sender (i.e. the `IoBridge` event loop exiting) closes
    /// the channel, which is how `GhostframeServer::wait_for_client` reports
    /// that no client can ever arrive.
    connected_session_count: watch::Sender<usize>,
    /// `true` while no `wt_sessions` entry is connected. Flips on transitions
    /// so we can emit a single info-level log on each enter/leave instead of
    /// once per dropped frame. Initial `true` (we start with zero sessions).
    was_idle: bool,
    /// Production-only handle for browser-driven input injection (see
    /// `transport::input_inject`). `None` on the test paths whose
    /// `IoBridge` constructor doesn't take an injector — the
    /// `INPUT_MSG_TYPE` dispatch arm silently skips messages when this
    /// is `None`.
    pub(crate) input_injector:
        Option<std::sync::Arc<dyn crate::transport::input_inject::InputInjector>>,
    /// Production handle for server-side display control (X RandR; see
    /// `transport::display::DisplayController`). `None` until a later M4b
    /// task wires `XrandrDisplay` in -- the `DISPLAY_MODE_MSG_TYPE` dispatch
    /// arm silently drops requests when this is `None`, mirroring
    /// `input_injector`'s pattern.
    pub(crate) display_controller:
        Option<std::sync::Arc<dyn crate::transport::display::DisplayController>>,
    /// Scale most recently advertised by `DisplayInfoMsg`. Carried forward
    /// into every subsequent `set_output` call so a later resize doesn't
    /// silently reset the user's DPI. Defaults to `DEFAULT_SCALE_MILLI`
    /// (1.0) so a client that never sends `DisplayInfo` sees no change.
    display_scale_milli: u16,
    /// A `DisplayMode` request awaiting its debounce deadline. Overwritten
    /// (not queued) by every new `DisplayMode`, which is what collapses a
    /// resize drag's continuous stream of requests into a single applied
    /// change. `on_timeout` clears this once it fires.
    pending_display_mode: Option<PendingDisplayMode>,
    /// Accumulator for partial feedback-stream messages. QUIC stream reads
    /// can split a single browser-side write into multiple chunks; we
    /// buffer them here and only consume complete messages, leaving any
    /// trailing partial in place for the next call. Cleared on garbage
    /// (unknown msg_type or unknown INPUT sub-kind) so a corrupt byte
    /// can't wedge the dispatcher forever.
    ///
    /// Keyed by sender: one connection's partial message must never be
    /// completed by another connection's bytes. Removed in `forget_session`.
    feedback_recv_bufs: HashMap<ConnectionHandle, Vec<u8>>,
    /// Lifetime counter of `send_datagram` Errs (Blocked / TooLarge /
    /// Disabled / UnsupportedByPeer). The Blocked case fires whenever the
    /// QUIC send buffer is full — at session start while cwnd is small,
    /// or any time tile-burst emission outruns drain. Without surfacing
    /// these, server-side packet loss from QUIC backpressure is silent:
    /// the receiver-side `loss_rate` doesn't include datagrams that
    /// never left the host.
    datagram_send_errs: u64,
    /// Whether we've already logged a `warn!` for the first send-datagram
    /// error of the process's life. Subsequent errors increment the
    /// counter but log at `debug` to avoid 30 Hz warn-spam under
    /// sustained backpressure.
    datagram_send_err_first_logged: bool,
    /// AIMD multiplier on the per-tick scheduler budget. Initialised to
    /// 1.0; halved (down to `BUDGET_MIN_MULTIPLIER`) whenever a dispatch
    /// pass observes ≥1 `send_datagram` error (proof our budget exceeded
    /// quinn's actual drain); ramped back up by `BUDGET_RAMP_FACTOR` on
    /// every error-free dispatch (capped at 1.0). Classic TCP-style
    /// additive-increase / multiplicative-decrease, but applied to the
    /// scheduler budget rather than the cwnd. Lives on the bridge so it
    /// survives across `process_frame_gpu` invocations.
    tick_budget_multiplier: f64,
    /// Continuation state for the in-flight frame's scheduler tick.
    /// Populated at the end of `dispatch_dirty_tiles_via_scheduler` and
    /// consumed by the `Event::DatagramsUnblocked` handler so that as
    /// soon as quinn drains its send buffer we top it back up from the
    /// scheduler queues — without waiting for the next 33 ms capture
    /// tick. This is the "pull-driven" pacing piece on top of A+B-light:
    /// rather than running the full per-frame budget in one shot at
    /// frame-receive time, we hand quinn one chunk and let its
    /// completion event tell us when there's room for more. The
    /// `remaining_budget_bytes` field is the per-frame budget minus what
    /// we've already drained, so we never overshoot the AIMD-adjusted
    /// frame budget across all continuation invocations.
    scheduler_continuation: Option<SchedulerContinuation>,
    /// Recent `cwnd / smoothed_rtt` samples; the BWE capacity hint is their
    /// minimum. See `apply_path_stats_snapshot`.
    capacity_hint_window: std::collections::VecDeque<f32>,
    /// quinn's cumulative `sent_packets` at the previous path-stats sample,
    /// and the delta since. Zero delta means the link was idle over that
    /// interval, which is when `cwnd / rtt` stops meaning anything.
    path_sent_packets_last: u64,
    /// Losses awaiting a feedback report with at least one acknowledgement to
    /// travel in. See the drain site.
    pending_losses: Vec<(u32, usize)>,
    /// Per-transmission accounting. Deliberately not the retransmit cache:
    /// that one is content-scoped and emptied by supersession, which is what
    /// hid most losses from the estimator.
    transmission_ledger: crate::transport::transmission_ledger::TransmissionLedger,
    /// The ledger's verdicts as a rate: `adaptation_context.loss_rate`.
    loss_window: crate::transport::loss_window::LossWindow,
    /// When quinn's `sent_packets` last advanced. `None` before the first
    /// packet. Used to tell a busy link from an idle one — see
    /// `apply_path_stats_snapshot`.
    path_last_sent_at: Option<std::time::Instant>,
    /// Frame identity from the most recent `drain_scheduler_into_quinn`
    /// call, from any of its three call sites. Unlike
    /// `scheduler_continuation`, never cleared once set — it exists purely
    /// so BWE Stage 2.4b's probe-window-open drain
    /// (see `open_probe_window_for_burst`) has a valid `seq` /
    /// `timestamp_us` / `max_frag` to tag its datagrams with. `None` until
    /// the bridge's first drain; a probe window that opens before then
    /// (e.g. racing session start) has nothing valid to stamp and skips
    /// the immediate drain rather than fabricate a seq — see that
    /// method's doc comment.
    last_drain_frame_context: Option<DrainFrameContext>,
    /// Lifetime counters of datagrams handed to `send_datagram` per codec.
    /// Counted post-fragmentation, regardless of send success — the
    /// `datagram_send_errs` counter above tracks the failure subset. The
    /// purpose is to give us a server-side number directly comparable to
    /// the client's cumulative `rx{r:.. s:.. p:.. c:.. h:..}` stats so we
    /// can tell whether un-rendered tiles are server-side (never emitted),
    /// quinn-side (Blocked → counted in `datagram_send_errs`), or
    /// wire-side (emitted minus errs > client rx).
    cumulative_datagrams_emitted: CumulativeEmitCounters,
    /// Last frame seq at which we logged the cumulative-emit counters.
    /// Used to throttle the periodic `cumulative emit` log to once per
    /// `CUMULATIVE_EMIT_LOG_INTERVAL_FRAMES` frames (≈ 2 s at 30 fps).
    last_cumulative_emit_log_frame: u32,
    /// Per-frame unique-color histogram accumulator, gated by
    /// `GHOSTFRAME_DIAGNOSE_COLOR_HIST=1`. Buckets dirty-tile
    /// `unique_colors` values to answer "is the background really
    /// breaking the 16-color PalRle threshold per 32×32 tile, or is the
    /// classifier sending tiles to Cdf53 that should have been PalRle?".
    color_histogram_accumulator: UniqueColorHistogram,
    /// Per-frame bump counts, reset on every `cumulative emit` log.
    /// Drives the "did the server keep bumping generations after the
    /// initial burst?" question that the client's
    /// `cdf53-coverage: refined=0/14p` distribution surfaced — repeated
    /// `bump_generation` mid-refinement supersedes the prior
    /// generation's pending passes (`drain_*` queues `retain` Superseded
    /// entries on the next tick), so a tile that keeps getting marked
    /// dirty never finishes any single generation's 14-pass burst.
    /// Counters are total (across all tiles), separated by emission path
    /// so we can correlate with the scheduler-emission-policy site
    /// (`dispatch_dirty_tiles_via_scheduler` line 1007: priority-queue
    /// path, non-cdf53 codecs only; Phase B dirty-cdf53 line 2630;
    /// Phase B escalation line 2711).
    bump_count_accumulator: BumpCountAccumulator,
    /// Reliable tile emitter — owns wire_seq allocation, retransmit
    /// caching, FEC parity scheduling, and RTO bookkeeping. Fed by
    /// `drain_scheduler_into_quinn` via `submit_batch`; drained to the
    /// wire via the `IoBridgeSenderAdapter` trait impl wrapping
    /// `send_to_all_sessions`. Cancellation on bump_generation is wired
    /// by direct paired `cancel_for_tile` calls at every
    /// `scheduler.bump_generation*` site (see
    /// `dispatch_dirty_tiles_via_scheduler`, dirty-cdf53 Phase B, and
    /// escalation Phase B). Task 28 will retire the legacy
    /// `fragment_coverage` path that currently coexists with the new
    /// emitter pipeline.
    pub(crate) reliable_emitter: crate::transport::reliable_emitter::ReliableTileEmitter,
    /// Rolling buffer of one-way-delay samples derived from ACK
    /// envelopes (Phase 1 Task 5). Drained periodically into the Bwe
    /// estimator (Task 8) and into per-tier `TierLatencyStats` (BWE Stage
    /// 2.0). Bounded by `BWE_SAMPLES_BUFFER_CAPACITY`; under high ACK
    /// load excess samples are dropped (the consumer doesn't need
    /// every single sample for a useful estimate).
    bwe_samples_buffer: Vec<BweSample>,
    /// Bandwidth-estimator wrapper. Phase 1: runs in PASSIVE mode — we
    /// feed it ACK arrivals and read its estimate for the periodic log,
    /// but the estimate doesn't drive emission yet. Phase 2 will plug
    /// `bwe.snapshot().bitrate_bps` into the pacer.
    bwe: crate::transport::bwe::BweWrapper,
    /// The probe cluster window currently open (BWE Stage 2.4), if any. See
    /// `ActiveProbe`'s doc comment and `poll_probe_window`.
    active_probe: Option<ActiveProbe>,
    /// Cumulative count of probe windows that closed having met both
    /// `min_probes` and `min_bytes` (BWE Stage 2.4). Paired with
    /// `probes_abandoned` — exactly one of the two increments per closed
    /// window, in `poll_probe_window`'s `close_probe_window`.
    probes_completed: u64,
    /// Cumulative count of probe windows that closed *without* meeting
    /// both thresholds. Per the design's "No padding" section, this is the
    /// **expected** outcome on an idle link — the server never generates
    /// filler to complete a probe, so an under-filled window is discarded
    /// by goog_cc's own estimator (`probe_bitrate_estimator.rs:137`) and
    /// silently means nothing without this counter. A non-zero rate here
    /// alongside genuine encoder/scheduler backlog (not just idle gaps)
    /// would indicate probing is broken rather than merely unexercised.
    probes_abandoned: u64,
    /// Shared cell the browserless harness reads the final `BweSnapshot`
    /// from, after aborting the `run()` task.
    ///
    /// `run()` owns `self` for its whole lifetime once `spawn_local`d, so a
    /// plain `&self` accessor (`bwe_snapshot`, below) is unreachable from
    /// the harness once the bridge has been moved into the task — there is
    /// no way to call back into a value a spawned task owns. This cell is
    /// the workaround: the harness clones the `Arc` via
    /// `bwe_publish_handle()` *before* moving `bridge` into `spawn_local`,
    /// `run()`'s loop republishes into it on every iteration (see the
    /// `sample_all_path_stats`/BWE-drain block), and the harness reads the
    /// clone after `bridge_handle.abort()`. Cfg-gated identically to
    /// `bwe_snapshot` since both exist for the same caller.
    #[cfg(any(test, feature = "browserless-harness"))]
    bwe_publish: std::sync::Arc<std::sync::Mutex<crate::transport::bwe::BweSnapshot>>,
    /// Shared cell the browserless harness reads the final
    /// `reliable_emitter::EmitterStats` from, after aborting the `run()`
    /// task. Parallel to `bwe_publish` above and for the identical reason:
    /// `run()` owns `self` for its whole lifetime once `spawn_local`'d, so
    /// a plain `&self` accessor is unreachable from the harness once the
    /// bridge has moved into the task. The harness clones the `Arc` via
    /// `emitter_stats_publish_handle()` *before* moving `bridge` into
    /// `spawn_local`, `run()` republishes into it every iteration, and the
    /// harness reads the clone after `abort()`.
    ///
    /// Kept as a separate cell rather than folded into `BweSnapshot`:
    /// retransmit/NACK/RTO counts are a reliability-layer concept, not a
    /// bandwidth-estimator concept, and conflating the two would make
    /// "the BWE snapshot" mean two unrelated things.
    #[cfg(any(test, feature = "browserless-harness"))]
    emitter_stats_publish:
        std::sync::Arc<std::sync::Mutex<crate::transport::reliable_emitter::emitter::EmitterStats>>,
    /// Shared cell the browserless harness reads the final per-tier ACK
    /// latency stats from, after aborting the `run()` task. `(critical,
    /// refinement)`. Same cell/republish/read-after-abort pattern as
    /// `bwe_publish` and `emitter_stats_publish` above, and for the
    /// identical ownership reason — see `bwe_publish`'s doc comment.
    #[cfg(any(test, feature = "browserless-harness"))]
    latency_stats_publish: std::sync::Arc<std::sync::Mutex<(TierLatencyStats, TierLatencyStats)>>,
    /// Same publish-cell pattern as `latency_stats_publish`, for the
    /// `queued_at -> ACK` pair (BWE Stage 2.1) — `queued_at` is when the
    /// underlying `TileWork` became available to the scheduler, so this
    /// interval additionally captures scheduler queueing delay that
    /// `latency_stats_publish`'s `last_sent_at -> ACK` cannot see. Kept
    /// alongside, not instead of, `latency_stats_publish`: the latter is
    /// what would reveal a pacer overfilling the link and causing wire
    /// queueing.
    #[cfg(any(test, feature = "browserless-harness"))]
    queued_latency_stats_publish:
        std::sync::Arc<std::sync::Mutex<(TierLatencyStats, TierLatencyStats)>>,
    /// Shared cell the browserless harness reads the final probe-window
    /// counters from, after aborting the `run()` task: `(probes_completed,
    /// probes_abandoned)`. Same cell/republish/read-after-abort pattern as
    /// `bwe_publish` above and for the identical ownership reason — see
    /// that field's doc comment. Added for BWE Stage 2.4 Task 5's
    /// measurement: a scene run reporting zero of both means the probe
    /// window never opened, which the harness has no other way to see.
    #[cfg(any(test, feature = "browserless-harness"))]
    probe_stats_publish: std::sync::Arc<std::sync::Mutex<(u64, u64)>>,
    /// Cumulative bytes of CDF53 critical-tier (passes 0-3) datagrams
    /// emitted since startup. Counts every successful emit including
    /// retransmits, because the BWE-side rate computation should reflect
    /// actual wire utilization. Reset never; the per-second logger
    /// derives rates from the snapshot delta.
    bytes_emitted_critical: u64,
    /// Cumulative bytes of CDF53 refinement-tier (passes 4-13) datagrams
    /// emitted since startup. Same semantics as `bytes_emitted_critical`.
    bytes_emitted_refinement: u64,
    /// Per-tier ACK round-trip latency stats (BWE Stage 2.0 baseline
    /// measurement). See `TierLatencyStats`'s doc comment for the exact
    /// latency expression and the drain site in `run()` for where it's
    /// computed.
    critical_latency_stats: TierLatencyStats,
    /// Same as `critical_latency_stats`, for `PassTier::Refinement`.
    refinement_latency_stats: TierLatencyStats,
    /// `queued_at -> ACK` per-tier latency stats (BWE Stage 2.1). Same
    /// accumulation site (`run()`'s `bwe_samples_buffer` drain) as
    /// `critical_latency_stats`, but measured from `BweSample::queued_at`
    /// (via `CacheEntry::queued_at`, ultimately `TileWork::queued_at`)
    /// rather than `last_sent_at`. This is the interval scheduler
    /// prioritisation (pass-major drain, and later tier budgeting)
    /// actually shortens: `last_sent_at -> ACK` starts only when a pass
    /// leaves, so it cannot see queueing delay incurred before that.
    queued_critical_latency_stats: TierLatencyStats,
    /// Same as `queued_critical_latency_stats`, for `PassTier::Refinement`.
    queued_refinement_latency_stats: TierLatencyStats,
    /// Rolling ACK-latency tracker feeding the RTO policy. Recorded once
    /// per sample at the same drain site as the `*_latency_stats` fields
    /// above (`ack_latency_us`, before it's routed to a per-tier bucket);
    /// its p95 is pushed into `reliable_emitter.set_ack_deadline` once per
    /// drain. See `ack_latency::AckLatencyTracker`'s doc comment for why
    /// p95 over a small window rather than the cumulative mean/max the
    /// `TierLatencyStats` fields track — this is a control input, not a
    /// baseline measurement, and needs to track the *current* path rather
    /// than the whole session.
    ack_latency_tracker: crate::transport::ack_latency::AckLatencyTracker,
    /// Snapshot of (critical, refinement) byte counters at the previous
    /// periodic-log tick, used to derive per-window rates without
    /// polluting the cumulative counters.
    bytes_emitted_snapshot: (u64, u64),
    /// Wall-clock instant of the previous periodic-log tick. None until
    /// the first log fires.
    bwe_log_last_at: Option<std::time::Instant>,
    /// Diagnostic logging / one-shot dump toggles, parsed once at
    /// construction from the `LibConfig` the caller supplied. Ungated in
    /// every build — see `DiagnosticsConfig::from_lookup`'s doc comment for
    /// why these are live production toggles rather than test-only knobs.
    diagnostics: crate::config::DiagnosticsConfig,
}

/// Per-cumulative-log bump counters. Reset on log emission.
#[derive(Default, Clone, Copy)]
struct BumpCountAccumulator {
    /// `bump_generation_collecting` in `dispatch_dirty_tiles_via_scheduler`
    /// (priority-queue path, Solid/PalRle/Raw — Cdf53 tiles are skipped
    /// before reaching this site).
    priority_path: u32,
    /// `bump_generation` in dirty-cdf53 Phase B
    /// (`process_frame_gpu` line 2630). One per cdf53-classified dirty
    /// tile per frame.
    cdf53_dirty_path: u32,
    /// `bump_generation` in escalation Phase B
    /// (`process_frame_gpu` line 2711). One per
    /// idle-escalation-promoted tile per frame.
    cdf53_escalation_path: u32,
}

/// Cumulative count of datagrams the server has handed to
/// `wt.send_datagram(...)`, keyed by source codec. Per-codec rather than
/// total so we can correlate with the per-codec client `rx` counters.
#[derive(Default, Clone, Copy)]
struct CumulativeEmitCounters {
    raw: u64,
    solid: u64,
    palrle: u64,
    cdf53: u64,
    h264: u64,
}

/// Unique-color histogram over dirty tiles for one frame, reset on every
/// log emission. Buckets mirror the classifier's decision boundaries:
/// 1 → Solid eligible, 2–16 → PalRle eligible, 17 → Cdf53 (overflow).
#[derive(Default, Clone, Copy)]
struct UniqueColorHistogram {
    bucket_1: u32,
    bucket_2_4: u32,
    bucket_5_16: u32,
    bucket_overflow: u32,
    bucket_unknown: u32,
}

/// Frame identity (`seq` / `timestamp_us` / `max_frag`) captured from the
/// most recent `drain_scheduler_into_quinn` call, regardless of which of
/// its three call sites triggered it (per-frame dispatch, continuation
/// resume, or injected-scene drain). Independent of `SchedulerContinuation`
/// below, which additionally tracks the *remaining budget* of one specific
/// in-flight drain and is cleared once that budget is exhausted — this
/// field is never cleared; it always holds the last valid identity.
///
/// BWE probe-window bookkeeping (see `open_probe_window_for_burst`)
/// reuses this: `drain_scheduler_into_quinn` stamps `seq |
/// TILE_DATAGRAM_FLAG` into every datagram header, and the client keys tile
/// assembly / ACKs off that value, so a probe drain cannot invent a seq the
/// client has never seen. See `IoBridge::last_drain_frame_context`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct DrainFrameContext {
    seq: u32,
    timestamp_us: u32,
    max_frag: usize,
}

/// State the `Event::DatagramsUnblocked` handler needs to resume the
/// in-flight frame's scheduler drain. `seq` / `timestamp_us` /
/// `max_frag` come from the most recent `dispatch_dirty_tiles_via_scheduler`
/// call; re-emitted refinement work gets tagged with the current frame's
/// identity even though the underlying TileWork was queued earlier.
#[derive(Debug, Clone, Copy, PartialEq)]
struct SchedulerContinuation {
    seq: u32,
    timestamp_us: u32,
    max_frag: usize,
    remaining_budget_bytes: usize,
}

/// Per-frame inputs passed into `dispatch_dirty_tiles_via_scheduler`.
///
/// Borrows the BGRA pixel buffer and bundles the layout (`stride`) plus the
/// transport-level identity (`seq`, `timestamp_us`) so callers don't have to
/// spread four parallel arguments at every dispatch site.
pub(crate) struct TileDispatchFrame<'a> {
    pub pixels: &'a [u8],
    pub stride: u32,
    pub seq: u32,
    pub timestamp_us: u32,
}

/// Selects how `dispatch_dirty_tiles_via_scheduler` picks a codec per tile.
pub(crate) enum SchedulerEmissionPolicy {
    /// CPU path: every tile emits `Codec::Raw` regardless of classifier state.
    /// M3.1 D1 keeps the classifier sentinel-gated; this is the actual behavior.
    CpuRawOnly,
    /// GPU path: per-tile `CodecState` drives codec choice. `CodecState::Solid`
    /// → `encode_solid`; everything else → `Codec::Raw`.
    GpuClassifierDriven,
}

/// Validate a frame header and return how many bytes follow it.
///
/// Split out of `process_inbound` so the bounds can be tested directly:
/// driving them through `run()` cannot distinguish a rejected frame from an
/// accepted one, because either way the read that follows ends in
/// `UnexpectedEof` once the peer closes and the loop swallows it identically.
///
/// `total_len` counts the 8-byte header itself, so the remainder is
/// `total_len - 8`.
fn validate_frame_header(total_len: usize, payload_len: usize) -> io::Result<usize> {
    // Minimum valid frame: header (8) + payload + port (2) + NUL (1) = 11.
    if total_len < 8 + payload_len + 3 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too short",
        ));
    }

    // ...and an upper bound, because the check above only rejects frames that
    // are too *small*. Without it a corrupt `total_len` drives a ~4 GiB
    // allocation and then a `read_exact` for bytes that never arrive, wedging
    // the bridge task. See `MAX_FRAME_LEN`.
    if total_len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame total_len {total_len} exceeds MAX_FRAME_LEN {MAX_FRAME_LEN}"),
        ));
    }

    total_len
        .checked_sub(8)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "frame total_len underflow"))
}

/// The three constructor inputs that differ between the production and test
/// `IoBridge` literals. Deliberately has no `Default`: each caller spells all
/// three out. See [`IoBridge::base`].
struct BaseDiffs {
    handle: Option<GhostbridgeHandle>,
    local_addr: SocketAddr,
    gpu_frame_processor: Option<GpuFrameProcessor>,
}

impl IoBridge {
    /// The one place every `IoBridge` field gets its initial value.
    ///
    /// Production (`new`) and the test/harness constructor
    /// (`new_with_lib_config_for_test`) used to carry two complete copies of
    /// this literal, which is how a field added to one could silently be
    /// missing or different in the other. Exactly three values legitimately
    /// differ between them and they are passed in explicitly via
    /// [`BaseDiffs`]: the tsnet handle, the bound address, and the GPU frame
    /// processor. Everything else is identical by construction.
    fn base(
        stream: TokioUnixStream,
        server: QuicServer,
        lib_config: crate::config::LibConfig,
        diffs: BaseDiffs,
    ) -> Self {
        Self {
            _handle: diffs.handle,
            stream,
            server,
            local_addr: diffs.local_addr,
            wt_sessions: HashMap::new(),
            last_hello_from: None,
            close_deadlines: HashMap::new(),
            non_live_feedback_drops: 0,
            non_live_datagram_drops: 0,
            has_seen_prior_session: false,
            frame_rx: None,
            inject_rx: None,
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            metrics_tracker: crate::tile::MetricsTracker::new(0, 0),
            classifier: crate::tile::Classifier::new(lib_config.classifier),
            epoch: now_std(),
            bwe_epoch: now_std(),
            frame_mode: crate::tile::FrameMode::TileCodec,
            scheduler: crate::transport::scheduler::Scheduler::new(0, 0),
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_loss: lib_config.transport.outbound_loss,
            #[cfg(any(test, feature = "test-loss-injection"))]
            inbound_loss: lib_config.transport.inbound_loss,
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_bandwidth_cap: lib_config
                .transport
                .outbound_bandwidth_cap_bps
                .map(crate::transport::bandwidth_cap::BandwidthCap::new),
            #[cfg(any(test, feature = "test-loss-injection"))]
            test_force_bytes_per_us: lib_config.transport.test_force_bytes_per_us,
            #[cfg(any(test, feature = "test-loss-injection"))]
            oob_inject_at: lib_config.transport.oob_inject_at,
            #[cfg(any(test, feature = "test-loss-injection"))]
            skip_palette_session_reset: lib_config.transport.skip_palette_session_reset,
            #[cfg(any(test, feature = "test-loss-injection"))]
            drop_plan: None,
            tick_budget_floor_bytes: None,
            last_max_datagram_size: None,
            force_dirty_frames: 0,
            stuck_sweep_counter: 0,
            retransmit_pacer: crate::transport::retransmit_pacer::RetransmitPacer::new(),
            #[cfg(test)]
            loop_arm_trace: Vec::new(),
            session_started_at: None,
            last_disconnect_at: None,
            last_takeover_at: None,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            client_caps: crate::transport::client_caps::ClientCapabilities::default(),
            // `fec_k` parses unconditionally in every build (see
            // `TransportConfig::from_lookup`'s doc comment), so this reads
            // straight from the config with no `#[cfg]` needed.
            fec_k: lib_config.transport.fec_k.unwrap_or(0),
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
            adaptation_context: crate::tile::classifier::AdaptationContext::default(),
            recent_suspension_flags: [false, false],
            cdf53_escalation_candidates_this_frame: Vec::new(),
            cdf53_stranded_candidates_this_frame: Vec::new(),
            gpu_frame_processor: diffs.gpu_frame_processor,
            full_frame_encoder: None,
            recent_frame_fragments: HashMap::new(),
            fragment_coverage: crate::transport::fragment_coverage::FragmentCoverageMap::new(
                crate::transport::fragment_coverage::FRAGMENT_COVERAGE_CAPACITY,
            ),
            unroutable_inbound_datagrams: 0,
            #[cfg(any(test, feature = "browserless-harness"))]
            send_errs_publish: std::sync::Arc::new(std::sync::Mutex::new(0)),
            continuation_resumes: 0,
            continuation_drained_bytes: 0,
            last_emitted_dimensions: None,
            dimensions_retransmits_left: 0,
            connected_session_count: watch::Sender::new(0),
            was_idle: true,
            input_injector: None,
            display_controller: None,
            display_scale_milli: DEFAULT_SCALE_MILLI,
            pending_display_mode: None,
            feedback_recv_bufs: HashMap::new(),
            datagram_send_errs: 0,
            datagram_send_err_first_logged: false,
            tick_budget_multiplier: 1.0,
            scheduler_continuation: None,
            capacity_hint_window: std::collections::VecDeque::new(),
            path_sent_packets_last: 0,
            pending_losses: Vec::new(),
            transmission_ledger: crate::transport::transmission_ledger::TransmissionLedger::new(
                TRANSMISSION_LEDGER_CAPACITY,
            ),
            loss_window: crate::transport::loss_window::LossWindow::new(),
            path_last_sent_at: None,
            last_drain_frame_context: None,
            cumulative_datagrams_emitted: CumulativeEmitCounters::default(),
            last_cumulative_emit_log_frame: 0,
            color_histogram_accumulator: UniqueColorHistogram::default(),
            bump_count_accumulator: BumpCountAccumulator::default(),
            reliable_emitter: crate::transport::reliable_emitter::ReliableTileEmitter::new(
                now_std(),
            ),
            bwe_samples_buffer: Vec::with_capacity(BWE_SAMPLES_BUFFER_CAPACITY),
            bwe: crate::transport::bwe::BweWrapper::new(
                crate::transport::bwe::BweWrapper::INITIAL_BPS,
                now_std(),
            ),
            active_probe: None,
            probes_completed: 0,
            probes_abandoned: 0,
            // Throwaway estimator solely to obtain the same starting
            // `BweSnapshot` (`bwe.snapshot()` right above is unavailable
            // here — `self` doesn't exist yet inside this literal). Cheap:
            // `BweWrapper::new` just seeds a GoogCC controller's initial
            // state, and this field only exists under `cfg(test)` /
            // `browserless-harness` anyway.
            #[cfg(any(test, feature = "browserless-harness"))]
            bwe_publish: std::sync::Arc::new(std::sync::Mutex::new(
                crate::transport::bwe::BweWrapper::new(
                    crate::transport::bwe::BweWrapper::INITIAL_BPS,
                    now_std(),
                )
                .snapshot(),
            )),
            // Same cfg-gate and same reasoning as `bwe_publish` above; seeded
            // with `EmitterStats::default()` since there's no throwaway
            // instance needed here (unlike the BWE estimator, default stats
            // are simply all-zero).
            #[cfg(any(test, feature = "browserless-harness"))]
            emitter_stats_publish: std::sync::Arc::new(std::sync::Mutex::new(
                crate::transport::reliable_emitter::emitter::EmitterStats::default(),
            )),
            // Same cfg-gate and reasoning again, for the per-tier latency
            // stats — see `latency_stats_publish`'s doc comment.
            #[cfg(any(test, feature = "browserless-harness"))]
            latency_stats_publish: std::sync::Arc::new(std::sync::Mutex::new((
                TierLatencyStats::default(),
                TierLatencyStats::default(),
            ))),
            // Same cfg-gate and reasoning, for the `queued_at -> ACK`
            // per-tier latency stats (BWE Stage 2.1) — see
            // `queued_latency_stats_publish`'s doc comment.
            #[cfg(any(test, feature = "browserless-harness"))]
            queued_latency_stats_publish: std::sync::Arc::new(std::sync::Mutex::new((
                TierLatencyStats::default(),
                TierLatencyStats::default(),
            ))),
            // Same cfg-gate and reasoning, for the probe-window counters —
            // see `probe_stats_publish`'s doc comment. Both start at zero,
            // matching `probes_completed`/`probes_abandoned` above.
            #[cfg(any(test, feature = "browserless-harness"))]
            probe_stats_publish: std::sync::Arc::new(std::sync::Mutex::new((0u64, 0u64))),
            bytes_emitted_critical: 0,
            bytes_emitted_refinement: 0,
            critical_latency_stats: TierLatencyStats::default(),
            refinement_latency_stats: TierLatencyStats::default(),
            queued_critical_latency_stats: TierLatencyStats::default(),
            queued_refinement_latency_stats: TierLatencyStats::default(),
            ack_latency_tracker: crate::transport::ack_latency::AckLatencyTracker::new(),
            bytes_emitted_snapshot: (0, 0),
            bwe_log_last_at: None,
            diagnostics: lib_config.diagnostics,
        }
    }

    /// Create a new `IoBridge` by connecting to ghostbridge and opening a UDP
    /// listener on `listen_addr` (e.g. `":443"`).
    ///
    /// `lib_config` supplies every `GHOSTFRAME_*`-derived setting the bridge
    /// needs (transport fault-injection/pacing/FEC knobs, diagnostics
    /// toggles); the caller is expected to build it once via
    /// `crate::config::LibConfig::from_env()` at the executable boundary.
    /// Taken by value (not `&LibConfig`) because `TransportConfig` carries
    /// `LossInjector`s that must never be cloned — see `TransportConfig`'s
    /// doc comment.
    pub async fn new(
        ghostbridge_config: &GhostbridgeConfig,
        listen_addr: &str,
        lib_config: crate::config::LibConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let handle = GhostbridgeHandle::connect(ghostbridge_config)?;
        handle.up()?;
        let ips = handle.get_ips()?;
        tracing::info!(?ips, "ghostbridge node joined tailnet");

        // tsnet.ListenPacket rejects a bare ":port" address with "address
        // must be a valid IP". Bind instead to our first IPv4 tailnet IP on
        // the caller-supplied port.
        let port = parse_listen_port(listen_addr)?;
        let bind_ip = ips
            .iter()
            .find(|ip| ip.is_ipv4())
            .or_else(|| ips.first())
            .copied()
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AddrNotAvailable,
                    "tsnet node reported no tailnet IPs",
                )
            })?;
        let bind = format!("{bind_ip}:{port}");
        tracing::info!(%bind, "binding UDP listener on tailnet IP");

        let udp = handle.listen_udp(&bind)?;
        let raw_fd = udp.into_raw_fd();

        // Wrap the non-blocking ghostbridge fd as a tokio UnixStream.
        let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
        std_stream.set_nonblocking(true)?;
        let stream = TokioUnixStream::from_std(std_stream)?;

        let server = QuicServer::new()?;
        tracing::info!(cert_sha256 = %server.cert_info().sha256_hex, "QUIC server ready");

        // local_addr is the tailnet-rooted socket we bound; quinn-proto uses
        // its IP as the `local_ip` hint for inbound packets.
        let local_addr = SocketAddr::new(bind_ip, port);

        let gpu_frame_processor = GpuFrameProcessor::new(2048 * 2).ok();
        if gpu_frame_processor.is_some() {
            tracing::info!("GPU dirty tracker initialized (Vulkan compute SAD)");
        }

        // Warm rayon's global thread pool so the first PalRLE-heavy frame
        // doesn't pay thread-spin-up latency on the hot path (design Section 4).
        rayon::iter::IntoParallelIterator::into_par_iter(0..1u32).for_each(|_| {});

        let bridge = Self::base(
            stream,
            server,
            lib_config,
            BaseDiffs {
                handle: Some(handle),
                local_addr,
                gpu_frame_processor,
            },
        );
        Ok(bridge)
    }

    /// Create a new `IoBridge` wired to an inbound frame channel.
    ///
    /// Equivalent to [`new`] but attaches `frame_rx` so that captured frames
    /// are fragmented and sent as WebTransport datagrams to all connected peers.
    pub async fn new_with_frames(
        ghostbridge_config: &GhostbridgeConfig,
        listen_addr: &str,
        frame_rx: mpsc::Receiver<FrameSubmission>,
        lib_config: crate::config::LibConfig,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut bridge = Self::new(ghostbridge_config, listen_addr, lib_config).await?;
        bridge.frame_rx = Some(frame_rx);
        Ok(bridge)
    }

    /// Return the SHA-256 hex fingerprint of the server's self-signed cert.
    pub fn cert_hash_sha256(&self) -> &str {
        &self.server.cert_info().sha256_hex
    }

    /// Hand out a receiver on the connected-session counter so the capture
    /// loop (running on a different task) can skip the GetImage/DRM scrape when
    /// no consumer is attached, and park until one attaches. Updated by
    /// `compute_max_datagram_size` and the run loop. See the struct field doc
    /// for why this is a channel and not an atomic.
    pub fn connected_session_count_handle(&self) -> watch::Receiver<usize> {
        self.connected_session_count.subscribe()
    }

    /// Borrow the GhostbridgeHandle for callers that need to issue out-of-band
    /// FFI calls (e.g., starting the embedded web server on tsnet :80 + :443
    /// after the QUIC cert hash is known). Returns `None` when the IoBridge
    /// was constructed via the test-only path that has no real ghostbridge
    /// connection.
    pub fn ghostbridge(&self) -> Option<&crate::transport::ghostbridge::GhostbridgeHandle> {
        self._handle.as_ref()
    }

    /// Fragment a captured frame and send the resulting datagrams to all
    /// connected WebTransport sessions.
    /// Maximum bytes for the WebTransport VarInt session-ID prefix.
    /// A quarter-stream-id up to 63 encodes in 1 byte; we budget 2 for safety.
    const WT_VARINT_OVERHEAD: usize = 2;

    /// Compute the maximum datagram size from the smallest connected session.
    ///
    /// Side effect: calls `refresh_connected_session_count` so the capture
    /// gate's atomic stays current per frame.
    fn compute_max_datagram_size(&mut self) -> Option<usize> {
        self.refresh_connected_session_count();
        let mut min_size: Option<usize> = None;
        if let Some(handle) = self.live_send_handle() {
            if let Some(conn) = self.server.connections.get_mut(&handle) {
                if let Some(sz) = conn.datagrams().max_size() {
                    let usable = sz.saturating_sub(Self::WT_VARINT_OVERHEAD);
                    min_size = Some(match min_size {
                        Some(prev) => prev.min(usable),
                        None => usable,
                    });
                }
            }
        }
        let out = min_size.filter(|&sz| sz > 0);
        // Cached so `tick_budget_floor` can size itself in datagrams from
        // `&self`. Calling `compute_max_datagram_size` there instead would
        // re-run `refresh_connected_session_count` a second time per tick.
        self.last_max_datagram_size = out;
        out
    }

    /// Send a datagram to all connected WebTransport sessions.
    fn send_to_all_sessions(
        &mut self,
        dg: &[u8],
    ) -> crate::transport::reliable_emitter::traits::SendOutcome {
        use crate::transport::reliable_emitter::traits::SendOutcome;
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(cap) = self.outbound_bandwidth_cap.as_mut() {
            if !cap.try_consume(dg.len()) {
                // A harness-injected drop *consumes* the datagram. Reporting
                // `Rejected` here would make the emitter re-queue the same
                // bytes forever and the injected loss would never happen.
                return SendOutcome::Sent;
            }
        }
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(inj) = self.outbound_loss.as_mut() {
            if inj.should_drop(dg) {
                return SendOutcome::Sent; // injected loss: consumed, see above
            }
        }
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(plan) = self.drop_plan.as_ref() {
            if plan
                .lock()
                .expect("drop_plan mutex poisoned")
                .should_drop(dg)
            {
                return SendOutcome::Sent; // injected drop: consumed, see above
            }
        }
        // Every datagram goes to every connected session, so one refusal
        // means re-queue. With more than one session that re-sends to the
        // ones that accepted; the duplicate is wasted bandwidth rather than
        // corruption -- the client dedups on wire_seq and tile reassembly is
        // idempotent per frag_idx. Production runs a single session.
        let mut rejected = false;
        // Only the HELLO'd session receives stream data.
        if let Some(handle) = self.live_send_handle() {
            let handle = &handle;
            if let (Some(wt), Some(conn)) = (
                self.wt_sessions.get_mut(handle),
                self.server.connections.get_mut(handle),
            ) {
                if let Err(e) = wt.send_datagram(conn, dg) {
                    rejected = true;
                    self.datagram_send_errs = self.datagram_send_errs.saturating_add(1);
                    if !self.datagram_send_err_first_logged {
                        self.datagram_send_err_first_logged = true;
                        tracing::warn!(
                            ?handle,
                            error = ?e,
                            payload_len = dg.len(),
                            "first send_datagram Err — QUIC backpressure or limit; \
                             subsequent send failures will log at debug and a running \
                             total appears in the per-frame `dispatch returned` log"
                        );
                    } else {
                        tracing::debug!(?handle, error = ?e, "datagram send failed");
                    }
                }
            }
        }

        // No connected session counts as sent, not rejected: re-queueing a
        // datagram with nowhere to go would spin the drain forever.
        if rejected {
            SendOutcome::Rejected
        } else {
            SendOutcome::Sent
        }
    }

    /// Emit the frame-dimensions datagram on the first frame of each session and
    /// whenever dimensions change, retransmitting `FRAME_DIMENSIONS_RETRANSMITS`
    /// additional times to absorb datagram loss. Called by both
    /// `process_frame_cpu` and `process_frame_gpu`.
    fn emit_frame_dimensions(&mut self, seq: u32, timestamp_us: u32, width: u32, height: u32) {
        let dims_changed = self.last_emitted_dimensions != Some((width, height));
        if dims_changed {
            self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
            self.last_emitted_dimensions = Some((width, height));
        }
        if self.dimensions_retransmits_left > 0 {
            let dg = crate::transport::protocol::build_frame_dimensions_datagram(
                seq,
                timestamp_us,
                width,
                height,
            );
            self.send_to_all_sessions(&dg);
            self.dimensions_retransmits_left -= 1;
        }
    }

    /// Shared scheduler dispatch: grid-sync → RTT update → bump+encode+enqueue
    /// per dirty tile → tick → fragment+send. Called by both `process_frame_cpu`
    /// and `process_frame_gpu`'s `FrameMode::TileCodec` branch.
    ///
    /// Returns a `FrameSendStats` accumulating wire bytes and codec counts for
    /// all tiles sent this invocation. The caller uses this to emit
    /// `frame.last_send` after the final datagram is handed to QUIC.
    pub(crate) fn dispatch_dirty_tiles_via_scheduler(
        &mut self,
        dirty: &[(u32, u32)],
        grid: &crate::tile::TileGrid,
        frame: TileDispatchFrame<'_>,
        max_frag: usize,
        policy: SchedulerEmissionPolicy,
        mut palrle_payloads: Option<&mut std::collections::HashMap<(u32, u32), Vec<u8>>>,
    ) -> FrameSendStats {
        let TileDispatchFrame {
            pixels,
            stride,
            seq,
            timestamp_us,
        } = frame;
        // Grid sync — keep scheduler in lockstep with the dirty-detection grid.
        if self.scheduler.cols() != grid.cols || self.scheduler.rows() != grid.rows {
            self.scheduler.resize(grid.cols, grid.rows);
        }

        use crate::transport::scheduler::{TileWork, WorkState};

        for &(tile_x, tile_y) in dirty {
            // For GpuClassifierDriven, check whether this tile is handled by
            // Path B (Cdf53 refinement queue, enqueued above by Phase B) BEFORE
            // calling bump_generation_collecting. bump_generation supersedes all
            // queued work for the tile — so calling it on a Cdf53 tile would
            // invalidate the refinement passes Phase B just enqueued.
            if matches!(policy, SchedulerEmissionPolicy::GpuClassifierDriven) {
                use crate::tile::CodecState;
                let codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
                if matches!(codec_state, CodecState::Cdf53 { .. }) {
                    // Cdf53 state → Phase B handles emission via
                    // the refinement queue (generation already bumped by Phase B).
                    // Skip entirely: do NOT bump generation (would supersede Phase
                    // B's enqueued passes), do NOT enqueue to priority_queue.
                    continue;
                }
            }

            // Drop retransmit/cache state in the reliable emitter in lock-step
            // with scheduler supersession. Paired with every bump_generation*
            // call (replaces the Task 20 set_cancel_callback machinery, which
            // captured a raw pointer to self.reliable_emitter that dangled when
            // the bridge was moved out of `new`).
            self.reliable_emitter
                .cancel_for_tile(tile_x as u8, tile_y as u8);
            let (gen, superseded) = self
                .scheduler
                .bump_generation_collecting(tile_x as u8, tile_y as u8);
            self.bump_count_accumulator.priority_path =
                self.bump_count_accumulator.priority_path.saturating_add(1);
            // Drop any in-flight Cdf53 coverage for this tile. The bump
            // marked the matching refinement-queue work `Superseded`; the
            // already-emitted-but-not-yet-ACKed Cdf53 coverage must be
            // discarded in lockstep so `pending_refinement_snapshot` does
            // not keep reporting the cancelled passes as still pending until
            // LRU eviction. Non-Cdf53 coverage is preserved — see
            // `FragmentCoverageMap::drop_cdf53_for_tile` docs.
            self.fragment_coverage
                .drop_cdf53_for_tile(tile_x as u8, tile_y as u8);
            // New generation: tile is eligible for re-escalation.
            // Guard with a bounds check to stay safe when metrics_tracker has
            // not yet been sized (e.g. CpuRawOnly unit test path).
            if tile_x < self.metrics_tracker.cols() && tile_y < self.metrics_tracker.rows() {
                self.metrics_tracker
                    .get_mut(tile_x, tile_y)
                    .already_escalated_this_gen = false;
            }
            for s in superseded {
                if let Some(pid) = s.palette_id {
                    let pid_usize = pid as usize;
                    let cnt = self.palette_table.in_flight_carrying[pid_usize];
                    self.palette_table.in_flight_carrying[pid_usize] = cnt.saturating_sub(1);
                    // Release the acquire from Phase A. Supersession means the tile
                    // work is dropped without ever being ACKed.
                    if self.palette_table.ref_count[pid_usize] > 0 {
                        self.palette_table.release(pid);
                    }
                }
            }
            // Copied out only by the arms that send the pixels. Solid needs
            // one pixel and PalRle arrives already encoded, and on a
            // full-screen change this 4 KB copy per tile was most of what
            // the loop cost once the bookkeeping above stopped scanning.
            let extract_tile = || grid.extract_tile(pixels, stride, tile_x, tile_y);

            let (codec, payload) = match policy {
                SchedulerEmissionPolicy::CpuRawOnly => (Codec::Raw, extract_tile()),
                SchedulerEmissionPolicy::GpuClassifierDriven => {
                    use crate::tile::CodecState;
                    // Cdf53 tiles were already skipped above (before bump_generation);
                    // only non-Cdf53 tiles reach this point.
                    let codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
                    match codec_state {
                        CodecState::Solid => {
                            let solid = grid.first_pixel(pixels, stride, tile_x, tile_y);
                            (Codec::Solid, solid.to_vec())
                        }
                        CodecState::PalRle { .. } => {
                            let payload = if let Some(m) = palrle_payloads.as_mut() {
                                m.remove(&(tile_x, tile_y))
                            } else {
                                None
                            };
                            match payload {
                                Some(p) => (Codec::PalRle, p),
                                None => (Codec::Raw, extract_tile()), // table-full fallback or no GPU prep
                            }
                        }
                        // Raw is the encoder of last resort: these states
                        // either have no tile-local encoding of their own
                        // (H264 is a whole-frame path, Skip emits nothing)
                        // or are handled before this match. Listed so a new
                        // CodecState must choose its encoder rather than
                        // silently inheriting Raw, which is the most
                        // expensive option on the wire.
                        CodecState::H264 { .. }
                        | CodecState::Cdf53 { .. }
                        | CodecState::PixelPerfect
                        | CodecState::Skip => (Codec::Raw, extract_tile()),
                    }
                }
            };

            let now = now_std();
            self.scheduler.enqueue_at(
                TileWork {
                    tile_x: tile_x as u8,
                    tile_y: tile_y as u8,
                    generation: gen,
                    pass_idx: 0,
                    total_passes: 1,
                    codec,
                    payload,
                    queued_at: now,
                    last_sent_at: None,
                    state: WorkState::Pending,
                },
                now,
            );
        }

        // ---- A + B-light: feedback-modulated rate pacing ----
        //
        // (A, feedforward) Per-tick budget = measured QUIC bandwidth
        // (bytes/µs from path stats) × 33 ms frame interval × headroom.
        // The scheduler retains un-drained work in its queues, so a
        // 14 MB first-frame burst naturally spreads across ~70 frames
        // at typical tailnet speeds rather than dumping in one go.
        //
        // (B-light, feedback) `tick_budget_multiplier` shrinks on every
        // dispatch that observes a send_datagram Err (proof our
        // bytes_per_us estimate is too generous for quinn's actual drain)
        // and ramps back up on every error-free dispatch. Classic AIMD.
        //
        // CpuRawOnly tests bypass this with `usize::MAX` so unit fixtures
        // that pre-seed tiles still drain in one tick.
        let pre_dispatch_send_errs = self.datagram_send_errs;
        let base_budget_bytes = self.base_budget_bytes();
        // Clamp the AIMD-derived budget to what `quinn-proto` can actually
        // accept right now. `scheduler.tick` is destructive on the
        // refinement queue (popped work is removed, not marked `InFlight`),
        // and `send_datagram` returning `Err(Blocked)` discards the
        // already-popped TileWork irrecoverably. Pre-checking
        // `send_buffer_space()` — the only thing that triggers quinn's
        // Blocked (`outgoing_total + data.len() > datagram_send_buffer_size`
        // in quinn-proto/datagrams.rs) — prevents the scheduler from
        // popping more than quinn can absorb in this tick. Anything left
        // over stays in `refinement_queue` for the next `DatagramsUnblocked`
        // continuation or the next 33 ms capture tick.
        //
        // The 80 % safety fraction (`QUINN_SEND_BUFFER_SAFETY_FRACTION`)
        // leaves headroom for per-fragment wire overhead (WT quarter-
        // stream-id VarInt + tile-fragment header, ~30–60 B per datagram)
        // that `scheduler.tick` accounts as payload bytes only. That clamp,
        // the pacing combine, the drain, and the continuation bookkeeping
        // all live in `emit_via_scheduler`, shared with `apply_injected_
        // frame` — see its doc comment.
        //
        // CpuRawOnly bypasses all of that with `usize::MAX` so unit
        // fixtures that pre-seed tiles still drain in one tick.
        let (stats, drained_bytes, drained_count) = match policy {
            SchedulerEmissionPolicy::CpuRawOnly => {
                self.drain_scheduler_into_quinn(seq, timestamp_us, max_frag, usize::MAX)
            }
            _ => {
                let aimd_budget =
                    ((base_budget_bytes as f64) * self.tick_budget_multiplier) as usize;
                self.emit_via_scheduler(seq, timestamp_us, max_frag, aimd_budget)
            }
        };
        // ---- AIMD feedback: shrink the multiplier on any send error,
        //      ramp back up on a clean dispatch. CpuRawOnly bypasses the
        //      tracking — it ran with unbudgeted `usize::MAX` and unit
        //      fixtures don't go through real QUIC paths anyway.
        if !matches!(policy, SchedulerEmissionPolicy::CpuRawOnly) {
            let errs_this_dispatch = self
                .datagram_send_errs
                .saturating_sub(pre_dispatch_send_errs);
            let mult_before = self.tick_budget_multiplier;
            if errs_this_dispatch > 0 {
                self.tick_budget_multiplier = (self.tick_budget_multiplier
                    * SCHEDULER_BUDGET_BACKOFF)
                    .max(SCHEDULER_BUDGET_MULT_MIN);
            } else {
                self.tick_budget_multiplier = (self.tick_budget_multiplier * SCHEDULER_BUDGET_RAMP)
                    .min(SCHEDULER_BUDGET_MULT_MAX);
            }
            let quinn_space_diag = self.min_session_send_buffer_space();
            // `refinement_fraction` and the two queue depths are here
            // because their absence made a real defect unreadable: a drain
            // moving 5 KB against a 256 KB base budget looks like the budget
            // is irrelevant, when in fact refinement only ever gets
            // `base * refinement_fraction`, and that fraction halves toward
            // a 0.05 floor whenever delivery drops below 50%. Without these
            // fields the log shows the symptom (tiny drains) and hides both
            // the cause (the fraction) and the consequence (the backlog).
            tracing::debug!(
                quinn_send_buffer_space = quinn_space_diag,
                refinement_fraction = self.scheduler.current_refinement_fraction(),
                refinement_budget_bytes = self.scheduler.last_refinement_budget(),
                refinement_queue_len = self.scheduler.refinement_queue_len(),
                priority_queue_len = self.scheduler.queue_len(),
                drained_count = drained_count,
                drained_bytes = drained_bytes,
                base_budget_bytes = base_budget_bytes,
                bytes_per_us = self.adaptation_context.bytes_per_us,
                errs_this_dispatch = errs_this_dispatch,
                multiplier_before = mult_before,
                multiplier_after = self.tick_budget_multiplier,
                "scheduler.tick AIMD"
            );
        }

        stats
    }

    /// Drain the scheduler up to `budget_bytes`, fragment+send each
    /// returned `TileWork`, and return the per-codec / wire-byte stats
    /// alongside the total bytes drained and the number of TileWork
    /// items emitted. Shared between the per-frame dispatch entry point
    /// (`dispatch_dirty_tiles_via_scheduler`) and the
    /// `Event::DatagramsUnblocked` continuation handler — both want the
    /// same pull-from-scheduler / fragment / record-coverage / send
    /// loop, the only difference being the budget they pass in.
    ///
    /// `seq` / `timestamp_us` tag the outbound fragments with the
    /// *current* logical frame's identity; refinement work queued under
    /// an earlier frame still gets tagged with the current `seq` (which
    /// is what the client expects from a continuous stream).
    fn drain_scheduler_into_quinn(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        max_frag: usize,
        budget_bytes: usize,
    ) -> (FrameSendStats, usize, usize) {
        // Record this call's frame identity unconditionally, before any of
        // the budget/queue-dependent logic below, so a later probe-window
        // open always has the most recent valid `seq` to reuse — see
        // `DrainFrameContext` and `open_probe_window_for_burst`.
        self.last_drain_frame_context = Some(DrainFrameContext {
            seq,
            timestamp_us,
            max_frag,
        });
        let now = now_std();
        // The emitter owns recovery for whatever it still has cached; the
        // scheduler re-sends only what the emitter has let go of. See
        // `Scheduler::tick_at_with`.
        let emitter = &self.reliable_emitter;
        let drained = self
            .scheduler
            .tick_at_with(budget_bytes, now, &|x, y, p| emitter.holds_pass(x, y, p));
        let drained_count = drained.len();
        let mut stats = FrameSendStats::default();
        let mut total_wire_bytes_sent: usize = 0;
        // Build per-fragment (EmitKey, Bytes) items for the reliable
        // emitter. The emitter stamps wire_seq into bytes[8..12], caches
        // for retransmit, schedules RTO, and queues parity per FEC group.
        // The actual wire write happens in `reliable_emitter.drain(&mut
        // adapter, now)` below.
        let frame_seq_with_flag = seq | TILE_DATAGRAM_FLAG;
        let mut items: Vec<(
            crate::transport::reliable_emitter::EmitKey,
            bytes::Bytes,
            std::time::Instant,
            Option<crate::transport::reliable_emitter::cache::ProbeTag>,
        )> = Vec::with_capacity(drained.len());
        for work in drained {
            let datagrams = fragment_tile(
                &TileFragmentInputs {
                    frame_seq: frame_seq_with_flag,
                    tile_x: work.tile_x,
                    tile_y: work.tile_y,
                    codec: work.codec,
                    generation: work.generation,
                    pass: work.pass_idx,
                    timestamp_us,
                },
                &work.payload,
                max_frag,
            );
            // Record fragment coverage (legacy path — retained alongside
            // the new emitter pipeline for Task 25; Task 28 deletes).
            // Sub-option (a) per spec: only the FINAL fragment carries
            // the tile entry; intermediate fragments contribute nothing
            // (client can't usefully process the tile until assembly
            // completes anyway).
            let n = datagrams.len();
            // Declared outside the `if n > 0` block so that the per-tier
            // byte counters below (Phase 1 Task 6) can read it after the
            // block closes.
            let mut tile_wire_bytes_total: u32 = 0;
            if n > 0 {
                let palette_id = if work.codec == crate::transport::protocol::Codec::PalRle {
                    // PalRle payload byte 1 is the persistent palette slot id
                    // (byte 0 is the bundled-flag); guarded against malformed
                    // payloads via .get().
                    work.payload.get(1).copied()
                } else {
                    None
                };
                // Byte 0 bit 0 is the bundled flag (see `pal_rle::decode`).
                // Recorded so the acknowledgement path decrements
                // `in_flight_carrying` only for the emissions that
                // incremented it.
                let palette_bundled = work.codec == crate::transport::protocol::Codec::PalRle
                    && work.payload.first().is_some_and(|f| f & 0x01 != 0);
                let coverage: crate::transport::fragment_coverage::CoverageList =
                    smallvec![crate::transport::fragment_coverage::FragmentCoverage {
                        tile_x: work.tile_x,
                        tile_y: work.tile_y,
                        generation: work.generation,
                        pass_idx: work.pass_idx,
                        codec: work.codec,
                        palette_id,
                        palette_bundled,
                    }];
                let key = (frame_seq_with_flag, work.tile_x, work.tile_y, work.pass_idx);
                self.fragment_coverage.record(key, coverage);
                if work.pass_idx == 0 {
                    stats.tile_count = stats.tile_count.saturating_add(1);
                    match work.codec {
                        Codec::Solid => {
                            stats.codec_histogram.solid =
                                stats.codec_histogram.solid.saturating_add(1);
                        }
                        Codec::PalRle => {
                            stats.codec_histogram.palrle =
                                stats.codec_histogram.palrle.saturating_add(1);
                        }
                        Codec::Cdf53 => {
                            stats.codec_histogram.cdf53 =
                                stats.codec_histogram.cdf53.saturating_add(1);
                        }
                        Codec::Raw => {
                            stats.codec_histogram.raw = stats.codec_histogram.raw.saturating_add(1);
                        }
                        // No histogram bucket: H264 is counted on the
                        // whole-frame path, Skip emits no datagram. Listed so
                        // a new codec cannot go uncounted without someone
                        // deciding that it should.
                        Codec::H264 | Codec::Skip => {}
                    }
                }
                let tile_wire_bytes: u32 = datagrams.iter().map(|dg| dg.len() as u32).sum();
                tile_wire_bytes_total = tile_wire_bytes;
                stats.total_wire_bytes = stats.total_wire_bytes.saturating_add(tile_wire_bytes);
                total_wire_bytes_sent =
                    total_wire_bytes_sent.saturating_add(tile_wire_bytes as usize);
            }
            // Cumulative per-codec emit counter — bumped once per
            // datagram fragment so the cadence matches the client's
            // per-datagram `rx{...}` accounting. Counted before send so
            // the "emitted - send_errs" subtraction reflects actual
            // wire-attempts; loss between emit and client is then
            // `(emitted - send_errs) - client.rx`.
            let n_dg = datagrams.len() as u64;
            match work.codec {
                Codec::Solid => {
                    self.cumulative_datagrams_emitted.solid =
                        self.cumulative_datagrams_emitted.solid.saturating_add(n_dg);
                }
                Codec::PalRle => {
                    self.cumulative_datagrams_emitted.palrle = self
                        .cumulative_datagrams_emitted
                        .palrle
                        .saturating_add(n_dg);
                }
                Codec::Cdf53 => {
                    self.cumulative_datagrams_emitted.cdf53 =
                        self.cumulative_datagrams_emitted.cdf53.saturating_add(n_dg);
                    // Per-tier byte attribution (Phase 1 Task 6). Uses the
                    // real total wire bytes summed from each datagram fragment
                    // (headers included), set in `tile_wire_bytes_total` above.
                    // Counts every successful emit including retransmits so the
                    // BWE consumer's rate computation reflects actual wire
                    // utilization, not initial-emit accounting.
                    let bytes_this_tile = tile_wire_bytes_total as u64;
                    match pass_tier(work.pass_idx) {
                        PassTier::Critical => {
                            self.bytes_emitted_critical =
                                self.bytes_emitted_critical.saturating_add(bytes_this_tile);
                        }
                        PassTier::Refinement => {
                            self.bytes_emitted_refinement = self
                                .bytes_emitted_refinement
                                .saturating_add(bytes_this_tile);
                        }
                    }
                }
                Codec::Raw => {
                    self.cumulative_datagrams_emitted.raw =
                        self.cumulative_datagrams_emitted.raw.saturating_add(n_dg);
                }
                // Same reasoning as the histogram above: no per-codec
                // datagram counter for these, stated rather than defaulted.
                Codec::H264 | Codec::Skip => {}
            }
            // Hand every fragment to the reliable emitter under the
            // same EmitKey (the per-tile-pass identity used by ACK/NACK).
            // Use `frame_seq_with_flag` (= seq | TILE_DATAGRAM_FLAG) so
            // the cache key matches the wire frame_seq embedded in the
            // datagram header — that's what the client echoes back in
            // every ACK/NACK entry, and what `fragment_coverage` already
            // uses one line up.  Indexing the cache by raw `seq` made
            // every ACK lookup miss, so every entry sat in the cache
            // until RTO fired MAX_RETRANSMITS times and the entry
            // retired, which presents as PixelPerfect-never-fires on
            // an otherwise idle gradient.
            let key = crate::transport::reliable_emitter::EmitKey::new(
                frame_seq_with_flag,
                work.tile_x,
                work.tile_y,
                work.pass_idx,
            );
            for dg in datagrams {
                // BWE Stage 2.4: tag this datagram for the active probe
                // window, if any -- mirrors `queued_at`'s treatment above:
                // the tag travels with the pass from here through
                // `CacheEntry` to the ACK path. `bytes_sent_before` is the
                // cluster's running total *before* this datagram (goog_cc's
                // `probe_cluster_bytes_sent` semantics), so it must be read
                // before `bytes_sent` is advanced by this datagram's size.
                let dg_len = dg.len() as i64;
                let probe_tag = self.active_probe.as_mut().map(|p| {
                    let tag = crate::transport::reliable_emitter::cache::ProbeTag {
                        id: p.id,
                        min_probes: p.min_probes,
                        min_bytes: p.min_bytes,
                        bytes_sent_before: p.bytes_sent,
                    };
                    p.bytes_sent = p.bytes_sent.saturating_add(dg_len);
                    p.packets_sent = p.packets_sent.saturating_add(1);
                    tag
                });
                items.push((key, bytes::Bytes::from(dg), work.queued_at, probe_tag));
            }
        }
        // Submit the batch (stamps wire_seq, caches, schedules RTO + parity).
        self.reliable_emitter.submit_batch(items, now);
        // Drain everything queued to the wire via the adapter. The
        // adapter routes each datagram to `send_to_all_sessions` — the
        // same wire path as before this refactor.
        let bridge_ptr: *mut IoBridge = self as *mut IoBridge;
        let mut adapter = IoBridgeSenderAdapter { bridge: bridge_ptr };
        self.reliable_emitter.drain(&mut adapter, now);
        (stats, total_wire_bytes_sent, drained_count)
    }

    /// Enqueue an injected frame's tile work into the scheduler, then
    /// attempt to drain it to the wire. Used by both the `run()` select
    /// arm and the `drain_injection_for_test` test accessor, so the two
    /// paths cannot drift apart.
    ///
    /// Enqueuing alone would leave the work sitting in the scheduler
    /// forever: `drain_scheduler_into_quinn` is the only thing that turns
    /// queued `TileWork` into datagrams (see its two production callers,
    /// `dispatch_dirty_tiles_via_scheduler` and the `DatagramsUnblocked`
    /// continuation). If there's no connected session yet,
    /// `compute_max_datagram_size` returns `None` and the drain is
    /// skipped — the work stays queued for the next opportunity.
    ///
    /// Returns the number of tiles actually enqueued into the scheduler.
    /// This is NOT the number of tiles submitted in `inj.work`: tiles
    /// outside the fixed grid are skipped by the bounds check below, so
    /// the two counts diverge whenever a harness scene mis-sizes its
    /// grid. Callers that need to know work was dropped (as opposed to
    /// merely queued) must use this return value, not `inj.work.len()`.
    fn apply_injected_frame(&mut self, inj: InjectedFrame) -> usize {
        // The scheduler grid is normally sized by the real capture path
        // (`dispatch_dirty_tiles_via_scheduler`, keyed off the frame's
        // actual dimensions via `TileGrid`). Injected work bypasses that
        // path entirely, so on a bridge that only ever receives injected
        // frames the scheduler is still at its construction-time (0, 0)
        // and `Scheduler::enqueue`'s bounds `debug_assert` would fire for
        // any real tile coordinate. `new_with_injection_for_test` sizes
        // the grid once, up front, from the scene's known dimensions.
        //
        // Never resize here: `Scheduler::resize` clears the priority
        // queue, the refinement queue, and every CDF53 pass-ACK record,
        // so growing the grid mid-scene would silently discard in-flight
        // work — e.g. a multi-frame scene that rewrites tile (0,0) across
        // several frames would have its queued/refinement/ACK state wiped
        // the moment a later frame first touches a tile outside the
        // original bounding box. That failure mode is invisible at the
        // call site: tiles simply never converge, with no error anywhere.
        // Coordinates outside the fixed grid are a harness-author bug —
        // surface them loudly instead of growing around them.
        let cols = self.scheduler.cols();
        let rows = self.scheduler.rows();
        let mut enqueued_count = 0usize;
        // Accumulate out-of-grid tiles instead of logging per-tile: a
        // mis-sized harness grid injects whole frames of out-of-grid work
        // at scene rate (e.g. 60 tiles/frame x 30 fps), and a five-line
        // `error!` per tile would bury the one summary line that actually
        // matters under ~1800 identical records per second.
        let mut out_of_grid_skipped = 0usize;
        let mut first_out_of_grid: Option<(u8, u8)> = None;

        // Newer content for a tile must invalidate whatever is still queued
        // for it, exactly as the capture path's `bump_generation` does. Skip
        // this and two versions of the same tile sit in the queue at once;
        // whichever drains later is rendered last, and since
        // `drain_scheduler_into_quinn` stamps every tile it drains with the
        // *draining* frame's seq rather than the enqueuing frame's, the older
        // content can arrive labelled newer and win.
        //
        // Done once per distinct tile before the enqueue loop, not per work
        // item: a Cdf53 tile contributes 14 pass items for the same
        // coordinate, and superseding per item would drop this very frame's
        // earlier passes.
        let mut superseded: Vec<(u8, u8)> = Vec::new();
        for work in &inj.work {
            if work.tile_x as u32 >= cols || work.tile_y as u32 >= rows {
                continue;
            }
            let coord = (work.tile_x, work.tile_y);
            if !superseded.contains(&coord) {
                self.scheduler.supersede_pending_for_tile(coord.0, coord.1);
                superseded.push(coord);
            }
        }

        for work in inj.work {
            if work.tile_x as u32 >= cols || work.tile_y as u32 >= rows {
                out_of_grid_skipped += 1;
                first_out_of_grid.get_or_insert((work.tile_x, work.tile_y));
                continue;
            }
            // Route CDF53 passes through `refinement_queue` so they get
            // `drain_refinement_pass_major`'s pass-major drain, exactly as
            // the production capture path does via `enqueue_refinement_at`.
            // Everything else (Solid/PalRle/Raw single-pass work) keeps
            // going through `enqueue_at` -> `priority_queue` -> FIFO drain,
            // unchanged. `enqueue_refinement_work_at` pushes the harness's
            // already-formed `TileWork` as-is, preserving its `pass_idx`/
            // `total_passes` exactly rather than rebuilding them by
            // position — load-bearing when a scene submits a partial or
            // out-of-order set of passes. See
            // docs/specs/bwe-tier-latency-baseline.md's "Correction: the
            // harness never exercises pass-major ordering".
            if work.codec == crate::transport::protocol::Codec::Cdf53 {
                self.scheduler.enqueue_refinement_work_at(work, now_std());
            } else {
                self.scheduler.enqueue_at(work, now_std());
            }
            enqueued_count += 1;
        }
        if let Some((tile_x, tile_y)) = first_out_of_grid {
            tracing::error!(
                skipped = out_of_grid_skipped,
                first_tile_x = tile_x,
                first_tile_y = tile_y,
                grid_cols = cols,
                grid_rows = rows,
                "apply_injected_frame: tile coordinate(s) outside the \
                 scheduler grid fixed at construction — skipped (first \
                 offender shown, `skipped` is the count for this frame). \
                 Pass the scene's true grid_cols/grid_rows to \
                 new_with_injection_for_test instead of relying on \
                 growth-on-demand."
            );
        }
        // This gate is load-bearing for dimensions, not just for the
        // drain: `emit_frame_dimensions` below only runs inside it. With
        // no session connected yet, `compute_max_datagram_size()` returns
        // `None` and the whole block — including `emit_frame_dimensions`
        // — is skipped for that frame. Emitting dimensions unconditionally
        // here would burn all of `FRAME_DIMENSIONS_RETRANSMITS` into the
        // void with nobody connected to receive them; by the time a client
        // does connect, `dims_changed` in `emit_frame_dimensions` is false
        // (the dimensions haven't changed since the burned attempts) and
        // the retransmit counter is already exhausted, so the new client
        // would never learn the grid size. `fire_session_reset` re-arming
        // `dimensions_retransmits_left` (`:2468`) is what actually covers
        // reconnects — this gate just has to not waste the budget before
        // there is anyone to receive it.
        if let Some(max_frag) = self.compute_max_datagram_size() {
            // Tell the client the canvas / tile-grid size before draining
            // any tile work, mirroring process_frame_cpu (:2388) and
            // process_frame_gpu (:2500) — the injected path bypasses capture
            // entirely, so nothing else on this path would ever emit frame
            // dimensions, and a browserless client would receive tile
            // datagrams for a grid it was never told the size of.
            // `emit_frame_dimensions` only re-arms its retransmit counter
            // when (width, height) actually changes (`last_emitted_dimensions`,
            // :1241-1245), so calling it on every injected frame is safe even
            // though the harness's grid is fixed for the bridge's lifetime.
            // Width/height come from the scheduler grid fixed at construction
            // (`cols`/`rows` above) scaled by the same `crate::tile::TILE_SIZE`
            // (32px) edge the real capture path derives its grid from
            // (`tile::mod.rs`'s `TileGrid::new`).
            self.emit_frame_dimensions(
                inj.seq,
                inj.timestamp_us,
                cols * crate::tile::TILE_SIZE,
                rows * crate::tile::TILE_SIZE,
            );

            // BWE Stage 2.2/2.4 pacing, the quinn-capacity clamp, the drain
            // itself, and the post-drain continuation bookkeeping are all
            // shared with `dispatch_dirty_tiles_via_scheduler` via
            // `emit_via_scheduler` — see its doc comment.
            //
            // The base budget is `base_budget_bytes()` — the same
            // bytes_per_us-derived, floor-clamped number the capture path
            // starts from — rather than a harness-private constant.
            // `InjectedFrame` used to carry its own `budget_bytes` (always
            // `usize::MAX` in practice, i.e. "drain the whole queue every
            // injection"), which was deleted along with the field: an
            // always-unbounded budget meant the harness never queued the
            // way production does, which in turn meant a probe window
            // could never open against a non-empty queue. See
            // docs/superpowers/specs/2026-09-13-unified-emission-path-design.md.
            //
            // Deliberately *not* scaled by `tick_budget_multiplier`. That
            // AIMD ramp/backoff is adjusted at the dispatch call site from a
            // pre/post delta of `self.datagram_send_errs` computed around
            // that specific call (see `dispatch_dirty_tiles_via_scheduler`).
            // `send_datagram` errors are in fact recorded regardless of
            // which path triggered them, but nothing here tracks a
            // comparable delta for injected frames, and the AIMD loop is
            // calibrated for a roughly-periodic 33 ms real capture cadence —
            // the harness submits frames on whatever cadence a scene
            // script chooses, under virtual (`start_paused`) time, which is
            // not that cadence. Wiring the same ramp/backoff bookkeeping
            // into this call site would duplicate the AIMD accounting the
            // Task 1 extraction exists to avoid duplicating, in exchange for
            // a control loop whose tuning assumptions this path doesn't
            // meet. If a future scene needs to exercise AIMD backoff
            // specifically, that is a reason to add tracking here
            // deliberately, not a reason to have inherited it by accident.
            let base_budget_bytes = self.base_budget_bytes();
            self.emit_via_scheduler(inj.seq, inj.timestamp_us, max_frag, base_budget_bytes);
        }
        // Mirror the frame path's post-dispatch RTO sweep so injected
        // scenes exercise retransmission the same way real frames do.
        self.sweep_rto_retransmits();
        enqueued_count
    }

    /// Parse a concatenated FEEDBACK-stream byte buffer, dispatching by
    /// message-type byte. Supports:
    /// - `0x01` FEEDBACK_MSG_TYPE  (22 bytes)  — `ReceiverFeedback`
    /// - `0x03` HELLO_MSG_TYPE     (2 bytes)   — capability advertisement
    /// - `0x04` DECODE_ERROR_MSG_TYPE (5 bytes) — per-tile decode failure
    /// - `0x05` INPUT_MSG_TYPE     (variable) — pointer/wheel/key event
    ///
    /// Unknown message types abort parsing of the rest of the buffer
    /// (we can't safely advance past an unknown variable-length message).
    /// Future message types must extend this dispatcher.
    pub(crate) fn dispatch_feedback_bytes(&mut self, from: ConnectionHandle, data: &[u8]) {
        use crate::transport::client_caps::{HelloMsg, HELLO_MSG_TYPE, HELLO_SIZE};
        use crate::transport::decode_error::{
            DecodeErrorMsg, DECODE_ERROR_MSG_TYPE, DECODE_ERROR_SIZE,
        };
        use crate::transport::display::{
            DisplayInfoMsg, DisplayModeMsg, DISPLAY_INFO_MSG_TYPE, DISPLAY_INFO_SIZE,
            DISPLAY_MODE_MSG_TYPE, DISPLAY_MODE_SIZE,
        };
        use crate::transport::feedback::{FEEDBACK_MSG_TYPE, FEEDBACK_SIZE};

        // Append the new chunk to our accumulator. QUIC stream reads can
        // split a single browser-side write across multiple deliveries
        // (e.g. an 8-byte PointerButton arriving as 6 + 2 bytes), so we
        // can't dispatch directly off `data` — we need to walk a
        // contiguous view that includes any leftover tail from the
        // previous call.
        //
        // The accumulator is per sender: bytes from one connection must
        // never complete a partial message from another.
        let mut buf = self.feedback_recv_bufs.remove(&from).unwrap_or_default();
        buf.extend_from_slice(data);
        let mut offset = 0;
        let mut discard_tail = false;

        while offset < buf.len() {
            let msg_type = buf[offset];
            // Authorisation, re-evaluated per message: HELLO is the only
            // message a connection may send before it is the live session,
            // and a HELLO earlier in this same buffer can make it live for
            // what follows. Nothing else executes for a non-live sender.
            if msg_type != HELLO_MSG_TYPE && !self.is_live(from) {
                self.non_live_feedback_drops = self.non_live_feedback_drops.saturating_add(1);
                if self.non_live_feedback_drops == 1
                    || self.non_live_feedback_drops.is_multiple_of(64)
                {
                    tracing::warn!(
                        ?from,
                        msg_type,
                        total = self.non_live_feedback_drops,
                        "feedback-stream bytes from a session that has not sent HELLO; \
                         discarded (rate-limited)"
                    );
                }
                discard_tail = true;
                break;
            }
            match msg_type {
                FEEDBACK_MSG_TYPE => {
                    if buf.len() - offset < FEEDBACK_SIZE {
                        break;
                    }
                    if let Some(fb) = ReceiverFeedback::decode(&buf[offset..]) {
                        tracing::debug!(
                            received = fb.datagrams_received,
                            lost = fb.datagrams_lost,
                            recovered_fec = fb.datagrams_recovered_fec,
                            loss_rate = %format!("{:.2}%", fb.loss_rate() * 100.0),
                            "receiver feedback"
                        );
                        self.update_fec_from_feedback(&fb);
                        self.ingest_feedback_for_adaptation(fb);
                    }
                    offset += FEEDBACK_SIZE;
                }
                HELLO_MSG_TYPE => {
                    if buf.len() - offset < HELLO_SIZE {
                        break;
                    }
                    if let Some(msg) = HelloMsg::decode(&buf[offset..]) {
                        self.apply_hello(from, msg);
                    }
                    offset += HELLO_SIZE;
                }
                DECODE_ERROR_MSG_TYPE => {
                    if buf.len() - offset < DECODE_ERROR_SIZE {
                        break;
                    }
                    if let Some(msg) = DecodeErrorMsg::decode(&buf[offset..]) {
                        self.handle_decode_error(msg);
                    }
                    offset += DECODE_ERROR_SIZE;
                }
                DISPLAY_INFO_MSG_TYPE => {
                    if buf.len() - offset < DISPLAY_INFO_SIZE {
                        break;
                    }
                    if let Some(msg) = DisplayInfoMsg::decode(&buf[offset..]) {
                        // `mm_width`/`mm_height` are advisory-only (M4b design
                        // §4.1, §7): logging them here is the ONLY thing
                        // permitted to read them -- no decision below this
                        // line may branch on them. `scale_milli` is the
                        // authoritative DPI signal and is the only field
                        // carried into `display_scale_milli`.
                        tracing::debug!(
                            max_width = msg.max_width,
                            max_height = msg.max_height,
                            scale_milli = msg.scale_milli,
                            mm_width = msg.mm_width,
                            mm_height = msg.mm_height,
                            "display info"
                        );
                        self.display_scale_milli = msg.scale_milli;
                    }
                    offset += DISPLAY_INFO_SIZE;
                }
                DISPLAY_MODE_MSG_TYPE => {
                    if buf.len() - offset < DISPLAY_MODE_SIZE {
                        break;
                    }
                    if let Some(msg) = DisplayModeMsg::decode(&buf[offset..]) {
                        // Clamp/align/floor now (the decision of *what* to
                        // apply); only the *when* is deferred to the
                        // debounce deadline below. A controller-less bridge
                        // (X backend not wired in yet -- Task 4) has no
                        // ceiling to clamp against, so the request is
                        // dropped rather than guessed at.
                        if let Some(ctl) = self.display_controller.as_ref() {
                            let ceiling = ctl.ceiling();
                            let (w, h) = clamp_and_align_mode(msg.width, msg.height, ceiling);
                            let now_us = now_std().duration_since(self.epoch).as_micros() as u64;
                            tracing::debug!(
                                requested_w = msg.width,
                                requested_h = msg.height,
                                applied_w = w,
                                applied_h = h,
                                "display mode requested; debounced"
                            );
                            self.pending_display_mode = Some(PendingDisplayMode {
                                width: w,
                                height: h,
                                deadline_us: now_us + DISPLAY_MODE_DEBOUNCE_US,
                            });
                        }
                    }
                    offset += DISPLAY_MODE_SIZE;
                }
                crate::transport::input_inject::INPUT_MSG_TYPE => {
                    use crate::transport::input_inject::{apply_input, decode_input_msg};
                    // Need at least the kind byte to know the message size.
                    if buf.len() - offset < 2 {
                        break;
                    }
                    let sub_kind = buf[offset + 1];
                    let required = match sub_kind {
                        // PointerMove / Wheel / KeyDown / KeyUp: 6 bytes
                        0x01 | 0x03 | 0x04 | 0x05 => 6,
                        // PointerButton: 8 bytes
                        0x02 => 8,
                        _ => 0,
                    };
                    if required == 0 {
                        tracing::warn!(sub_kind, "unknown INPUT sub-kind; clearing feedback buf");
                        discard_tail = true;
                        break;
                    }
                    if buf.len() - offset < required {
                        // Tail is a complete header + partial payload; keep
                        // it and wait for the next stream-read to extend.
                        break;
                    }
                    if let Some((msg, consumed)) = decode_input_msg(&buf[offset..]) {
                        let injected = self.input_injector.is_some();
                        // `trace`, not `debug`: `?msg` carries the key codes, and at
                        // the default `ghostframe=debug` filter a debug line would
                        // write every keystroke into the journal. The input e2e
                        // reads this line from a trace-level container log.
                        tracing::trace!(?msg, injected, "input event dispatched");
                        if let Some(injector) = self.input_injector.as_ref() {
                            apply_input(injector.as_ref(), &msg);
                        }
                        offset += consumed;
                    } else {
                        // Pre-check guarantees this branch is unreachable;
                        // belt-and-braces in case decode_input_msg evolves.
                        tracing::warn!(sub_kind, "decode_input_msg failed after size check");
                        discard_tail = true;
                        break;
                    }
                }
                unknown => {
                    tracing::warn!(
                        msg_type = unknown,
                        "unknown feedback-stream message type; dropping rest of buffer"
                    );
                    discard_tail = true;
                    break;
                }
            }
        }

        // Retain any unconsumed tail for next call (partial-message case).
        // Garbage paths (`discard_tail = true`) intentionally drop the tail
        // so a corrupt byte doesn't wedge the dispatcher.
        if !discard_tail && offset < buf.len() {
            debug_assert!(
                buf.len() - offset < 64,
                "retained feedback tail must be a partial message, not {} bytes",
                buf.len() - offset
            );
            self.feedback_recv_bufs.insert(from, buf.split_off(offset));
        }
    }

    /// Fire any due timers driven by `now_us` (microseconds since
    /// `IoBridge::epoch` -- the same clock `dispatch_feedback_bytes` stamps
    /// `PendingDisplayMode::deadline_us` from, i.e. the same
    /// `now_std().duration_since(self.epoch)`
    /// pattern, so callers never need a second clock to drive this).
    ///
    /// Currently the only timer is the `DisplayMode` debounce: once
    /// `now_us` reaches the stored deadline, applies the pending mode via
    /// `display_controller.set_output` and clears it, so a resize drag's
    /// continuous stream of requests collapses into the one size the user
    /// actually settled on.
    /// When the event loop must next wake: the earlier of QUIC's own
    /// deadline and any pending `DisplayMode` debounce.
    ///
    /// **The display half is load-bearing, not belt-and-braces.** The loop
    /// used to wait on `server.next_timeout()` alone, which is `None` when
    /// no connection has a deadline pending. On a static desktop that is the
    /// normal state: no damage means no captured frames, an idle client
    /// sends nothing, and the loop blocks. A `DisplayMode` stored just
    /// before that happens would sit with its 250 ms deadline unexamined for
    /// as long as the session stayed quiet.
    ///
    /// The M4b end-to-end test caught exactly this: the server logged
    /// "display mode requested; debounced", then went completely silent for
    /// the remaining 10 s of the test while the resize never arrived. Every
    /// unit test passed throughout, because they call `on_timeout` directly
    /// and so never exercise the wakeup.
    fn next_wake_deadline(&mut self) -> Option<std::time::Instant> {
        let quic = self.server.next_timeout();
        let display = self
            .pending_display_mode
            .as_ref()
            .map(|p| self.epoch + std::time::Duration::from_micros(p.deadline_us));
        let close = self.close_deadlines.values().map(|d| d.at).min();
        [quic, display, close].into_iter().flatten().min()
    }

    /// React to a client-reported decode error. Most error codes are
    /// log-only in M3.2b; code 3 (ERR_THIN_UNCACHED_PALETTE) triggers
    /// `force_rebundle` so the next emission for that palette includes
    /// the palette block again.
    pub(crate) fn handle_decode_error(
        &mut self,
        msg: crate::transport::decode_error::DecodeErrorMsg,
    ) {
        use crate::tile::CodecState;
        use crate::transport::decode_error::ERR_THIN_UNCACHED_PALETTE;

        tracing::warn!(
            target: "ghostframe::bench",
            codec = msg.codec,
            tile_x = msg.tile_x,
            tile_y = msg.tile_y,
            error_code = msg.error_code,
            "client decode error"
        );

        if msg.error_code == ERR_THIN_UNCACHED_PALETTE {
            // Recover which palette_id was last emitted for this tile by
            // consulting metrics_tracker.codec_state.
            let m = self
                .metrics_tracker
                .get(msg.tile_x as u32, msg.tile_y as u32);
            if let CodecState::PalRle { palette_id } = m.codec_state {
                self.palette_table.force_rebundle(palette_id);
                tracing::info!(
                    palette_id,
                    "force_rebundle: next emission for palette will include bundled palette block"
                );
            }
        }
    }

    /// Run the event loop until the ghostbridge fd is closed (EOF).
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut header = [0u8; 8];
        // [BRIDGE-DIAG] per-select-arm counters so a periodic heartbeat
        // shows whether the loop is alive and which arm dominates.
        let mut diag_iters: u64 = 0;
        let mut diag_timeout_arm: u64 = 0;
        let mut diag_frame_arm: u64 = 0;
        let mut diag_inbound_arm: u64 = 0;
        let mut diag_last_heartbeat = now_std();
        // Consecutive inbound packets taken without a frame arm running.
        let mut inbound_streak: u32 = 0;

        loop {
            // Build a timer future that fires at the earliest QUIC timeout, or
            // a never-completing future if there are no active connections.
            let sleep_fut: Pin<Box<dyn Future<Output = ()> + Send>> =
                match self.next_wake_deadline() {
                    Some(deadline) => Box::pin(sleep_until(TokioInstant::from_std(deadline))),
                    None => Box::pin(pending::<()>()),
                };

            // After `INBOUND_STREAK_LIMIT` packets in a row the inbound arm
            // sits one round out, so a frame that has been waiting is taken.
            let frames_turn = inbound_streak >= INBOUND_STREAK_LIMIT;

            tokio::select! {
                biased;

                // 1. Earliest QUIC timeout fired.
                _ = sleep_fut => {
                    diag_timeout_arm += 1;
                    #[cfg(test)]
                    self.loop_arm_trace.push(LoopArm::Timeout);
                    let reaped = self.server.handle_timeout(now_std());
                    for handle in reaped {
                        self.on_connection_gone(handle, "reaped after drain");
                    }
                }

                // 2. Inbound framed UDP packet from ghostbridge.
                //
                // Ahead of the frame arm, and it has to be. This `select!`
                // is biased, so whichever of the two is listed first wins
                // whenever both are ready -- and a frame is *always* ready
                // once processing one takes longer than the capture
                // interval. Frames used to be listed first. Measured on a
                // 1920x1080 screen re-encoding every tile each frame (550 ms
                // of work against a 200 ms capture interval): over 18 s the
                // loop took 34 frames and 8 inbound packets, the server read
                // no acknowledgement of any kind, quinn fell back to PTO
                // probes on an exponential timer, and about 35 packets
                // reached the client in half a minute. The session was up,
                // the capture was running, and nothing was being delivered.
                //
                // Reading the network is cheap and is what lets everything
                // already sent make progress; encoding another frame is
                // expensive and, with the network unread, produces work that
                // cannot leave. So the cheap thing that unblocks goes first.
                read_res = self.stream.read_exact(&mut header), if !frames_turn => {
                    diag_inbound_arm += 1;
                    inbound_streak += 1;
                    #[cfg(test)]
                    self.loop_arm_trace.push(LoopArm::Inbound);
                    match read_res {
                        Ok(_) => {
                            if let Err(e) = self.process_inbound(&header).await {
                                tracing::warn!("inbound frame error: {e}");
                            }
                        }
                        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                            tracing::info!("ghostbridge fd closed — shutting down");
                            return Ok(());
                        }
                        Err(e) => return Err(e.into()),
                    }
                }

                // 3. Frame submission from capture thread.
                frame = async {
                    match self.frame_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    diag_frame_arm += 1;
                    inbound_streak = 0;
                    #[cfg(test)]
                    self.loop_arm_trace.push(LoopArm::Frame);
                    if let Some(frame) = frame {
                        self.process_frame(frame);
                    } else {
                        tracing::warn!("frame_rx closed; capture upstream dropped");
                    }
                }

                // 3b. Pre-encoded tile work injected by the browserless
                // harness, in place of capture + classification. `None` in
                // production (the field is always present so this branch
                // needs no `cfg`; a `None` receiver just costs a pending
                // future).
                injected = async {
                    match self.inject_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    inbound_streak = 0;
                    #[cfg(test)]
                    self.loop_arm_trace.push(LoopArm::Injected);
                    match injected {
                        Some(inj) => {
                            self.apply_injected_frame(inj);
                        }
                        None => {
                            self.inject_rx = None;
                        }
                    }
                }

                // 4. It was the frame arms' turn and neither had anything.
                // Always ready, and last, so it fires only then: without it
                // a round with inbound switched off and no frame pending
                // would wait on a frame that may not be coming.
                _ = std::future::ready(()), if frames_turn => {
                    inbound_streak = 0;
                }
            }

            // Arm/enforce close deadlines first, so a CONNECTION_CLOSE queued
            // here leaves in the drain_outbound just below.
            self.sweep_close_deadlines(now_std());

            // Always drain after either branch.
            //
            // Ordering is critical: drain_app_events may write new stream
            // data (e.g. HTTP/3 SETTINGS on Connected, or a 200 response
            // on CONNECT), so we must call drain_outbound *again* after
            // drain_app_events to flush any newly-queued transmits.
            // Without this second flush, the data sits in quinn-proto's
            // send buffer until the next loop iteration — which may never
            // come if the peer is waiting for that data (deadlock).
            self.drain_outbound().await?;
            self.server.drain_endpoint_events();
            self.drain_app_events();
            self.server.drain_endpoint_events();
            self.drain_outbound().await?;

            // M3.6a: sample QUIC path stats once per tick so the classifier's
            // bandwidth estimate stays current. No-op when no connections.
            self.sample_all_path_stats();

            // BWE Stage 2.4: open/close the probe cluster window. See
            // `poll_probe_window`'s doc comment for why this runs
            // unconditionally rather than only alongside the BWE drain
            // below.
            self.poll_probe_window(now_std());

            // M4b: fire the DisplayMode debounce timer, if one is pending.
            // Same epoch-relative clock the classifier's dwell timers use
            // (`self.epoch`; see `dispatch_feedback_bytes`'s DisplayMode
            // arm) -- not a second timing mechanism. Cheap no-op when
            // nothing is pending.
            let display_now_us = now_std().duration_since(self.epoch).as_micros() as u64;
            self.on_timeout(display_now_us);

            // Phase 1 Task 8: drain accumulated BWE samples (populated by
            // the ACK-receive path in Task 5) into the estimator. Cheap
            // on empty. The wire_seq we feed is a Phase 1 placeholder
            // (composed from emit/arrival timestamps); Phase 2 will plumb
            // the real cache-entry wire_seq through `BweSample` if the
            // estimator backend needs strict packet-sequence identity.
            // Everything the emitter put on the wire becomes an outstanding
            // record. `wire_seq` is allocated inside the emitter, so this is
            // the only place that knows which value each datagram carried.
            let ledger_now = now_std();
            for e in self.reliable_emitter.take_transmissions() {
                self.transmission_ledger.record(
                    e.wire_seq,
                    ledger_now,
                    crate::transport::transmission_ledger::Transmission {
                        emit_us: e.emit_us,
                        wire_bytes: e.wire_bytes,
                        key: e.key,
                    },
                );
            }
            self.expire_transmissions(ledger_now);
            // Reported only alongside an acknowledgement: goog_cc unwraps the
            // max receive time over a report's received packets, so a
            // loss-only report panics inside the library. A congested link can
            // spend whole ticks with nothing acknowledged, so they accumulate
            // here rather than being dropped.
            //
            // Bounded: if acknowledgements never resume the session is dead
            // and the backlog is worthless. Oldest go first — a stale loss
            // describes a send time the estimator has long since moved past.
            let overflow = self.pending_losses.len().saturating_sub(MAX_PENDING_LOSSES);
            if overflow > 0 {
                self.pending_losses.drain(..overflow);
            }
            if !self.bwe_samples_buffer.is_empty() {
                use crate::transport::bwe::AckArrival;
                let mut records: Vec<AckArrival> =
                    Vec::with_capacity(self.bwe_samples_buffer.len());
                // BWE Stage 2.0: accumulate per-tier ACK round-trip latency
                // while draining, instead of discarding `tier` and
                // `received_at` in a bare `.map()`. Both `received_at` and
                // `server_emit_us` are on the *server's* monotonic clock
                // (`self.bwe_epoch`), so this is an emit-to-ACK-receipt
                // round trip, not a one-way delay — deliberately: pacing
                // (Stage 2) reduces queueing delay for critical passes, and
                // this round trip is what surfaces that. Do NOT substitute
                // `client_arrival_ms_lo16` here — it's on the client's clock
                // with an unknown epoch, which is exactly what
                // `implausible_rtt_samples` (Stage 1) exists to detect.
                for s in self.bwe_samples_buffer.drain(..) {
                    let elapsed_since_epoch_us = s
                        .received_at
                        .saturating_duration_since(self.bwe_epoch)
                        .as_micros() as u64;
                    // `server_emit_us` comes from `ReliableTileEmitter::emit_us`,
                    // which is `u32` microseconds because that is what the
                    // datagram header carries at bytes [12..16]. It wraps every
                    // 2^32 us = 71.6 minutes, while `elapsed_since_epoch_us` is
                    // u64 and never does. Subtracting one from the other breaks
                    // at the first wrap: the u64 keeps climbing while the u32
                    // resets, so the difference becomes the elapsed session time
                    // instead of a round trip.
                    //
                    // Observed in production: a daemon up 81 minutes reported
                    // `critical_latency_mean_us=2448007853` -- 2448 seconds --
                    // having read a steady 19375 us for the preceding hour. The
                    // same corruption reaches `BweSample::server_emit_us`, so
                    // goog_cc's timing input is garbage past the first wrap.
                    //
                    // Do the subtraction in wrapping u32, which is correct
                    // across a wrap because the true delta is milliseconds.
                    let ack_latency_us = (elapsed_since_epoch_us as u32)
                        .wrapping_sub(s.server_emit_us as u32)
                        as u64;
                    match s.tier {
                        PassTier::Critical => self.critical_latency_stats.record(ack_latency_us),
                        PassTier::Refinement => {
                            self.refinement_latency_stats.record(ack_latency_us)
                        }
                    }
                    // Feed the RTO policy's rolling tracker with the same
                    // per-sample latency, regardless of tier -- the RTO
                    // applies uniformly to every emitted tile-pass, so its
                    // deadline should track the whole population rather
                    // than either tier alone. See `ack_latency_tracker`'s
                    // doc comment.
                    self.ack_latency_tracker.record(ack_latency_us);
                    // BWE Stage 2.1: `queued_at -> ACK`, alongside (not
                    // instead of) the `last_sent_at -> ACK` pair above.
                    // Starts earlier (when the work was queued rather than
                    // when it last left the wire), so this is always >=
                    // `ack_latency_us` for the same sample.
                    let queued_latency_us =
                        elapsed_since_epoch_us.saturating_sub(s.queued_since_epoch_us);
                    match s.tier {
                        PassTier::Critical => {
                            self.queued_critical_latency_stats.record(queued_latency_us)
                        }
                        PassTier::Refinement => self
                            .queued_refinement_latency_stats
                            .record(queued_latency_us),
                    }
                    records.push(AckArrival {
                        wire_seq: (((s.server_emit_us / 1000) as u32) << 16)
                            | (s.client_arrival_ms_lo16 as u32),
                        server_emit_us: s.server_emit_us,
                        client_arrival_ms_lo16: s.client_arrival_ms_lo16,
                        size_bytes: s.size_bytes,
                        probe: s.probe,
                    });
                }
                // Once per drain is enough: this is a cheap setter, and the
                // tracker only just received new samples above. Derives the
                // emitter's next first-attempt RTO deadline from the
                // rolling p95 -- see `rto::rto_for_attempt`'s doc comment
                // for the floor/ceiling this feeds into.
                self.reliable_emitter
                    .set_ack_deadline(self.ack_latency_tracker.p95());
                // quinn already measures path RTT for the scheduler; reuse it
                // as the reference for the estimator's plausibility check.
                if let Some(rtt) = self.live_path_rtt() {
                    self.bwe.note_path_rtt(rtt);
                }
                // Real in-flight bytes rather than a hardcoded zero, so
                // goog_cc's congestion-window pushback has something to act
                // on. Read before `update` so it reflects the state the
                // feedback describes.
                let in_flight = self.reliable_emitter.bytes_in_flight();
                let losses = std::mem::take(&mut self.pending_losses);
                self.bwe.update(&records, &losses, now_std(), in_flight);
            }

            // Republish into `bwe_publish` every iteration, not only when
            // `bwe_samples_buffer` had something to drain: the browserless
            // harness reads this after `bridge_handle.abort()`, at a point
            // in virtual time it does not control relative to this loop, so
            // the cell must always hold *a* current snapshot rather than
            // only being current on iterations that happened to update the
            // estimate. See `bwe_publish`'s doc comment.
            #[cfg(any(test, feature = "browserless-harness"))]
            {
                *self.bwe_publish.lock().unwrap() = self.bwe.snapshot();
                *self.emitter_stats_publish.lock().unwrap() = self.reliable_emitter.stats;
                #[cfg(any(test, feature = "browserless-harness"))]
                {
                    *self.send_errs_publish.lock().unwrap() = self.datagram_send_errs;
                }
                *self.latency_stats_publish.lock().unwrap() =
                    (self.critical_latency_stats, self.refinement_latency_stats);
                *self.queued_latency_stats_publish.lock().unwrap() = (
                    self.queued_critical_latency_stats,
                    self.queued_refinement_latency_stats,
                );
                *self.probe_stats_publish.lock().unwrap() =
                    (self.probes_completed, self.probes_abandoned);
            }

            // [BRIDGE-DIAG] heartbeat every 2s of virtual time (tokio's
            // clock, which is the wall clock in production but follows
            // `tokio::time::pause()` under the browserless harness) so we
            // can tell from the log whether the loop is making progress
            // when the emit pipeline goes silent. Comparing via
            // `now_std().duration_since(..)` rather than `.elapsed()`
            // matters here: `Instant::elapsed()` always reads the real
            // wall clock internally, so under a paused runtime it would
            // never see the stamp catch up and this heartbeat would go
            // silent forever.
            diag_iters += 1;
            if now_std().duration_since(diag_last_heartbeat) >= std::time::Duration::from_secs(2) {
                tracing::trace!(
                    target: "ghostframe::io_bridge::diag",
                    iters = diag_iters,
                    timeout = diag_timeout_arm,
                    frame = diag_frame_arm,
                    inbound = diag_inbound_arm,
                    "[BRIDGE-HB] select-arm counts (since start)"
                );
                diag_last_heartbeat = now_std();
            }
        }
    }

    /// Parse one inbound frame and feed it to quinn-proto.
    async fn process_inbound(&mut self, header: &[u8; 8]) -> io::Result<()> {
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;

        let rest_len = validate_frame_header(total_len, payload_len)?;
        let mut rest = vec![0u8; rest_len];
        self.stream.read_exact(&mut rest).await?;

        let packet = parse_frame_rest(&rest, payload_len)?;

        let mut buf: Vec<u8> = Vec::with_capacity(QUIC_SCRATCH);
        let data = BytesMut::from(packet.payload.as_slice());

        tracing::trace!(remote = %packet.addr, payload_len, "processing inbound datagram");
        if let Some(transmit) = self.server.handle_datagram(
            now_std(),
            packet.addr,
            Some(self.local_addr.ip()),
            None, // ECN not available from ghostbridge framing
            data,
            &mut buf,
        ) {
            // Immediate response (retry / version negotiation).
            let payload = &buf[..transmit.size];
            let frame = encode_frame(payload, &transmit.destination);
            self.stream.write_all(&frame).await?;
        }

        Ok(())
    }

    /// Write all pending outbound QUIC transmits back to ghostbridge.
    async fn drain_outbound(&mut self) -> io::Result<()> {
        let now = now_std();
        let mut buf: Vec<u8> = Vec::with_capacity(QUIC_SCRATCH);

        loop {
            buf.clear();
            match self.server.poll_transmit(now, 1, &mut buf) {
                None => break,
                Some(transmit) => {
                    let payload = &buf[..transmit.size];
                    tracing::trace!(size = transmit.size, dst = %transmit.destination, "outbound transmit");
                    let frame = encode_frame(payload, &transmit.destination);
                    self.stream.write_all(&frame).await?;
                }
            }
        }

        Ok(())
    }

    /// Drain all pending application-level QUIC events and drive the
    /// WebTransport handshake state machine.
    ///
    /// Events are polled and processed one at a time rather than collected
    /// up-front because processing an `Opened` event calls `accept()` +
    /// `read()`, which may cause quinn-proto to generate new `Readable`
    /// events that would be missed if we had already finished polling.
    fn drain_app_events(&mut self) {
        loop {
            let Some((handle, event)) = self.server.poll_events() else {
                break;
            };
            tracing::trace!(?handle, ?event, "app event");

            match event {
                Event::Connected => self.on_connection_established(handle),

                Event::Stream(StreamEvent::Opened { dir }) => {
                    // Accept all newly-opened peer streams for this direction.
                    if let (Some(wt), Some(conn)) = (
                        self.wt_sessions.get_mut(&handle),
                        self.server.connections.get_mut(&handle),
                    ) {
                        wt.on_stream_opened(conn, dir);
                    }
                }

                Event::Stream(StreamEvent::Readable { id }) => {
                    if let (Some(wt), Some(conn)) = (
                        self.wt_sessions.get_mut(&handle),
                        self.server.connections.get_mut(&handle),
                    ) {
                        wt.on_stream_readable(conn, id);
                    }
                }

                Event::DatagramReceived => {
                    // First pass: drain all available datagrams from the session,
                    // also responding to pings inline (since we already hold the
                    // wt+conn borrows). Defer ACK/etc. dispatch to a second pass
                    // because dispatch_ack_datagram takes `&mut self`.
                    let mut to_dispatch: Vec<Vec<u8>> = Vec::new();
                    if let Some(wt) = self.wt_sessions.get_mut(&handle) {
                        if let Some(conn) = self.server.connections.get_mut(&handle) {
                            while let Some(payload) = wt.recv_datagram(conn) {
                                if let Some(response) = handle_datagram_payload(&payload) {
                                    tracing::trace!(?handle, "ping received, sending pong");
                                    if let Err(e) = wt.send_datagram(conn, response) {
                                        tracing::warn!(?handle, error = ?e, "failed to send pong");
                                    }
                                } else {
                                    to_dispatch.push(payload);
                                }
                            }
                        }
                    }
                    for dg in to_dispatch {
                        self.dispatch_inbound_datagram(handle, &dg);
                    }
                }

                Event::ConnectionLost { reason } => {
                    self.on_connection_gone(handle, &reason.to_string());
                }

                Event::HandshakeDataReady => {
                    // First moment TLS hands us the client's ClientHello data —
                    // critical diagnostic for "browser fails to connect" reports:
                    // SNI tells us what hostname the browser thinks it's talking
                    // to (must match a cert SAN, even with serverCertificateHashes
                    // pinning), and ALPN confirms h3 negotiation.
                    if let Some(conn) = self.server.connections.get(&handle) {
                        if let Some(hd) = conn.crypto_session().handshake_data() {
                            if let Ok(hd) =
                                hd.downcast::<quinn_proto::crypto::rustls::HandshakeData>()
                            {
                                let sni = hd.server_name.as_deref().unwrap_or("(none)");
                                let alpn = hd
                                    .protocol
                                    .as_deref()
                                    .map(|p| String::from_utf8_lossy(p).into_owned())
                                    .unwrap_or_else(|| "(none)".into());
                                tracing::info!(
                                    ?handle,
                                    sni,
                                    alpn,
                                    "TLS handshake data: client SNI and ALPN"
                                );
                            }
                        }
                    }
                }

                Event::DatagramsUnblocked => {
                    // quinn just drained its send buffer to the wire —
                    // top it back up from the scheduler queues without
                    // waiting for the next 33 ms capture tick. This is
                    // the pull-driven pacing piece: scheduler.tick runs
                    // when quinn signals it has room, not when the
                    // next frame arrives.
                    self.resume_scheduler_continuation();
                    // Also fire the next batch of RTO-due retransmits.
                    // tick() is rate-limited to RTO_RETRANSMITS_PER_TICK
                    // entries so quinn's just-freed buffer absorbs them
                    // without re-Blocking. drain() flushes the resulting
                    // emission queue (source + parity) to the wire.
                    // Without this, the emitter would have to wait for
                    // the next 33 ms capture tick to push pending
                    // retransmits, adding up to ~33 ms latency to every
                    // recovered pass under sustained load.
                    self.sweep_rto_retransmits();
                }

                _ => {
                    // Everything else (DatagramsReceived handled above,
                    // etc.) — no action needed.
                }
            }
        }

        // Process any feedback data received on non-session bidi streams.
        // `iter_mut`, not `values_mut`: the handle identifies which client
        // sent each buffer, which eviction (and any future per-client state)
        // needs. Collecting first avoids holding a borrow of `wt_sessions`
        // across the `dispatch_feedback_bytes` calls, which take `&mut self`.
        //
        // Known race, not fixed here: this collects every session's
        // feedback into one batch before any of it is dispatched. If an
        // incumbent's re-HELLO and a genuinely new client's first HELLO
        // both land in the same batch, `wt_sessions` is a `HashMap`, so
        // which one is ordered last -- and therefore which `apply_hello`
        // runs second and evicts the other -- is a coin flip per pass; the
        // rare part is the simultaneous re-HELLO itself, not the ordering.
        // Not a wedge either way: both parties receive an eviction notice
        // (best-effort; see `evict_session`), so both disconnect knowing
        // why and either can reconnect. Narrow and self-recovering enough
        // that restructuring the batch isn't warranted; noted so a future
        // "sessions keep vanishing under load" report has a lead.
        let feedback_data: Vec<(ConnectionHandle, Vec<u8>)> = self
            .wt_sessions
            .iter_mut()
            .flat_map(|(&handle, wt)| wt.drain_feedback().into_iter().map(move |d| (handle, d)))
            .collect();
        for (handle, data) in &feedback_data {
            self.dispatch_feedback_bytes(*handle, data);
        }

        // Bootstrap-deadlock guard: refresh the connected-session count so a
        // freshly established WT session unblocks xdaemon's capture gate
        // BEFORE any frame submission (without this, the gate would stay at
        // 0 until a frame triggered `compute_max_datagram_size`, which can't
        // happen because the gate suppresses frames).
        self.refresh_connected_session_count();
    }

    /// Test-only constructor that accepts a pre-built stream and server,
    /// bypassing the real ghostbridge connection. No tsnet node is held, so
    /// `_handle` is `None` and no `gbridge_close` is called on Drop.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    ///
    /// Defaults to `LibConfig::default()` — every transport/diagnostics
    /// knob production-inert — so existing call sites are unaffected. Tests
    /// that need a specific config should call
    /// `new_with_lib_config_for_test` instead.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_stream_for_test(stream: TokioUnixStream, server: QuicServer) -> Self {
        Self::new_with_lib_config_for_test(stream, server, crate::config::LibConfig::default())
    }

    /// Test-only constructor taking a full `LibConfig`, for tests that need
    /// to set specific transport/diagnostics knobs without touching the
    /// process environment. `new_with_stream_for_test` is this with
    /// `LibConfig::default()`.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_lib_config_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        lib_config: crate::config::LibConfig,
    ) -> Self {
        // Warm rayon's global thread pool so the first PalRLE-heavy frame
        // doesn't pay thread-spin-up latency on the hot path (design Section 4).
        rayon::iter::IntoParallelIterator::into_par_iter(0..1u32).for_each(|_| {});

        let local_addr = "0.0.0.0:443".parse().unwrap();
        Self::base(
            stream,
            server,
            lib_config,
            BaseDiffs {
                handle: None,
                local_addr,
                gpu_frame_processor: None,
            },
        )
    }

    /// Frame-carrying variant of the test constructor, accepting a pre-built
    /// stream, server, and frame channel. Delegates to `new_with_stream_for_test`
    /// and adds frame injection support.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_frames_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        frame_rx: mpsc::Receiver<FrameSubmission>,
    ) -> Self {
        let mut bridge = Self::new_with_stream_for_test(stream, server);
        bridge.frame_rx = Some(frame_rx);
        bridge
    }

    /// Injection-carrying variant of the test constructor, accepting a
    /// pre-built stream, server, and injected-tile-work channel. Delegates
    /// to `new_with_stream_for_test` and adds tile-injection support for the
    /// browserless harness.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    /// `grid_cols`/`grid_rows` size the scheduler once, up front, from the
    /// scene's known dimensions — mirroring how the real capture path sizes
    /// it from the frame's `TileGrid` in `dispatch_dirty_tiles_via_scheduler`.
    /// See `apply_injected_frame` for why this must not change mid-scene.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_injection_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        inject_rx: mpsc::Receiver<InjectedFrame>,
        grid_cols: u32,
        grid_rows: u32,
    ) -> Self {
        let mut bridge = Self::new_with_stream_for_test(stream, server);
        bridge.scheduler.resize(grid_cols, grid_rows);
        bridge.inject_rx = Some(inject_rx);
        bridge
    }

    /// Install a deterministic drop plan, for a caller that is about to move
    /// `self` into a `spawn_local`'d task (as the browserless harness does
    /// with `IoBridge::run`) and needs a way to both drive the plan from
    /// inside `send_to_all_sessions` and read its fired-count back out
    /// afterwards. Unlike `bwe_publish_handle` and friends, the `Arc` here
    /// originates with the *caller* (it already holds the rules to enforce)
    /// rather than with the bridge, so this is a setter rather than a
    /// handle getter — but the reason an `Arc<Mutex<_>>` is needed at all is
    /// the same ownership problem: once `self` is owned by a spawned task, a
    /// plain `&self`/`&mut self` accessor is unreachable, so the caller must
    /// keep its own clone of the `Arc` from *before* the move and read the
    /// clone's contents *after* aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_drop_plan(
        &mut self,
        plan: std::sync::Arc<std::sync::Mutex<crate::transport::drop_plan::DropPlan>>,
    ) {
        self.drop_plan = Some(plan);
    }

    /// Clone of the `Arc` behind `emitter_stats_publish`, for a caller that
    /// is about to move `self` into a `spawn_local`'d task and needs a way
    /// to read the reliable-emitter counters back out afterwards. See
    /// `emitter_stats_publish`'s doc comment — same ownership problem as
    /// `bwe_publish_handle`, same fix. Call this *before* the move; read
    /// the clone's contents *after* aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn emitter_stats_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::transport::reliable_emitter::emitter::EmitterStats>>
    {
        self.emitter_stats_publish.clone()
    }

    /// Clone of the `Arc` behind `send_errs_publish` — cumulative
    /// `send_datagram` failures, overwhelmingly `Blocked`. Same
    /// ownership problem and fix as `emitter_stats_publish_handle`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn send_errs_publish_handle(&self) -> std::sync::Arc<std::sync::Mutex<u64>> {
        self.send_errs_publish.clone()
    }

    /// Clone of the `Arc` behind `latency_stats_publish` — `(critical,
    /// refinement)` — for a caller that is about to move `self` into a
    /// `spawn_local`'d task and needs a way to read the per-tier ACK
    /// latency stats back out afterwards. Same ownership problem and fix
    /// as `bwe_publish_handle` / `emitter_stats_publish_handle`. Call this
    /// *before* the move; read the clone's contents *after* aborting the
    /// task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn latency_stats_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<(TierLatencyStats, TierLatencyStats)>> {
        self.latency_stats_publish.clone()
    }

    /// Clone of the `Arc` behind `queued_latency_stats_publish` — `(critical,
    /// refinement)` `queued_at -> ACK` stats (BWE Stage 2.1). Same
    /// ownership problem and fix as `latency_stats_publish_handle`; call
    /// this *before* the move, read the clone's contents *after* aborting
    /// the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn queued_latency_stats_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<(TierLatencyStats, TierLatencyStats)>> {
        self.queued_latency_stats_publish.clone()
    }

    /// Clone of the `Arc` behind `probe_stats_publish` — `(probes_completed,
    /// probes_abandoned)` (BWE Stage 2.4). Same ownership problem and fix as
    /// `latency_stats_publish_handle`; call this *before* the move, read the
    /// clone's contents *after* aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn probe_stats_publish_handle(&self) -> std::sync::Arc<std::sync::Mutex<(u64, u64)>> {
        self.probe_stats_publish.clone()
    }

    /// Drive exactly one injected frame: enqueue its work, then attempt the
    /// drain. Returns the number of items `apply_injected_frame` actually
    /// enqueued into the scheduler — NOT the number of items submitted in
    /// the batch. Tiles outside the fixed grid are skipped and excluded
    /// from the count (see `apply_injected_frame`'s grid-bounds check), so
    /// a harness asserting on this value gets an accurate signal that work
    /// was dropped instead of a false "all delivered".
    ///
    /// This awaits the next message on `inject_rx`: on an open channel
    /// with nothing queued yet, the call blocks until a frame arrives (or
    /// the sender drops the channel) — it does **not** time out on its
    /// own. Under `tokio::time::pause()` (the reason the
    /// `browserless-harness` feature exists) that means a scene which
    /// never sends another frame hangs here indefinitely rather than
    /// returning 0. Returns 0 only when the channel is closed or was
    /// never set up (`inject_rx.is_none()`). Callers that want to poll
    /// "is a frame ready right now" instead of blocking should use
    /// `try_drain_injection_for_test`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub async fn drain_injection_for_test(&mut self) -> usize {
        let inj = match self.inject_rx.as_mut() {
            Some(rx) => match rx.recv().await {
                Some(f) => f,
                None => return 0,
            },
            None => return 0,
        };
        self.apply_injected_frame(inj)
    }

    /// Non-blocking counterpart to `drain_injection_for_test`. If a frame
    /// is already queued on `inject_rx`, drains and applies it exactly
    /// like the blocking version and returns the number of items
    /// enqueued (same submitted-vs-enqueued distinction applies). If
    /// nothing is queued right now — or the channel is closed or was
    /// never set up — returns 0 immediately instead of waiting.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn try_drain_injection_for_test(&mut self) -> usize {
        let inj = match self.inject_rx.as_mut() {
            Some(rx) => match rx.try_recv() {
                Ok(f) => f,
                Err(_) => return 0,
            },
            None => return 0,
        };
        self.apply_injected_frame(inj)
    }

    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn scheduler_peek_for_test(&self) -> Vec<crate::transport::scheduler::TileWork> {
        self.scheduler.peek_for_test()
    }

    /// Same as `scheduler_peek_for_test`, but for `refinement_queue` — lets
    /// a test confirm CDF53 injected work lands in the pass-major-drained
    /// queue rather than the FIFO one.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn scheduler_refinement_peek_for_test(&self) -> Vec<crate::transport::scheduler::TileWork> {
        self.scheduler.refinement_peek_for_test()
    }

    /// Update FEC parity state based on receiver feedback.
    /// Enables parity (K=4) when loss exceeds threshold, disables when it drops.
    fn update_fec_from_feedback(&mut self, fb: &ReceiverFeedback) {
        let loss = fb.loss_rate();
        if self.fec_k == 0 && loss >= self.fec_enable_threshold {
            self.fec_k = 4;
            tracing::info!(loss_rate = %format!("{:.2}%", loss * 100.0), "FEC enabled (K=4)");
        } else if self.fec_k > 0 && loss < self.fec_disable_threshold {
            self.fec_k = 0;
            tracing::info!(loss_rate = %format!("{:.2}%", loss * 100.0), "FEC disabled");
        }
    }
}

/// Parse the port from a `":<port>"` or `"<host>:<port>"` listen address.
fn parse_listen_port(listen_addr: &str) -> io::Result<u16> {
    listen_addr
        .rsplit(':')
        .next()
        .and_then(|s| s.parse::<u16>().ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid listen_addr: {listen_addr}"),
            )
        })
}

#[cfg(test)]
#[path = "io_bridge/tests.rs"]
mod tests;
