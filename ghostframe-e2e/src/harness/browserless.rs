//! Task 15a/15b: browserless netsim scene runner.
//!
//! Wires together a real `IoBridge` and a real `ghostframe_client_net::ClientNet`
//! across a `tokio::net::UnixStream` socketpair, with every datagram routed
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
//! ClientNet <-> SocketPairPump <-> UnixStream::pair() <-> IoBridge <-> QuicServer
//!              (netsim in both directions)
//! ```
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
use ghostframe_lib::transport::io_bridge::{InjectedFrame, IoBridge};
use ghostframe_lib::transport::quic::QuicServer;

use crate::harness::framebuffer::FrameBuffer;
use crate::harness::scene_tiles::{encode_tile, TileSpec};
use crate::netsim::{NetProfile, NetSim, SocketPairPump, Verdict};

/// Two virtual milliseconds' worth of frame spacing between injected
/// frames, matching a 62.5 fps scene-authoring cadence. Chosen simply to
/// be a small, deterministic, non-zero gap — nothing downstream depends on
/// this value meaning "one frame at 60fps" precisely.
const FRAME_SPACING_US: u64 = 16_000;

/// Spacing between post-injection heartbeat ticks (see `drive_session`'s
/// doc comment on why heartbeats exist at all). Deliberately much coarser
/// than `FRAME_SPACING_US`: the sole purpose of a heartbeat is to reach
/// `IoBridge::apply_injected_frame`'s unconditional `sweep_rto_retransmits`
/// call often enough that a cached, unacked fragment's RTO deadline (floor
/// 25ms, see `reliable_emitter::rto::rto_for_attempt`) gets checked well
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

/// A netsim scene to run browserlessly: a real `IoBridge` <-> `ClientNet`
/// session, no browser, no tsnet, driven under tokio's virtual clock.
pub struct BrowserlessScene {
    pub seed: u64,
    /// Drained in order once the session reaches `SessionReady`, one
    /// `FrameScript` every `FRAME_SPACING_US` of virtual time. See the
    /// module docs.
    pub frames: Vec<FrameScript>,
    pub net: NetProfile,
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

/// Outcome of one `run_browserless` call.
pub struct BrowserlessResult {
    pub framebuffer: FrameBuffer,
    pub events: Vec<ClientNetEvent>,
    pub bytes_delivered: u64,
    pub bytes_dropped: u64,
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
pub async fn run_browserless(scene: BrowserlessScene) -> anyhow::Result<BrowserlessResult> {
    let local = LocalSet::new();
    local.run_until(run_inner(scene)).await
}

async fn run_inner(scene: BrowserlessScene) -> anyhow::Result<BrowserlessResult> {
    let seed = scene.seed;

    let (ours, peer) = tokio::net::UnixStream::pair()
        .map_err(|e| anyhow!("seed {seed}: UnixStream::pair failed: {e}"))?;

    let server =
        QuicServer::new().map_err(|e| anyhow!("seed {seed}: QuicServer::new failed: {e}"))?;

    // Cert hash MUST be taken before `server` moves into the bridge below.
    let mut server_cert_sha256 = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut server_cert_sha256)
        .map_err(|e| anyhow!("seed {seed}: cert hash hex decode failed: {e}"))?;

    // `inject_tx` carries `FrameScript`-derived `InjectedFrame`s to
    // `IoBridge`, once the session is ready — see `drive_session`.
    let (inject_tx, inject_rx) = mpsc::channel::<InjectedFrame>(8);

    let mut bridge = IoBridge::new_with_injection_for_test(
        ours,
        server,
        inject_rx,
        scene.grid_cols,
        scene.grid_rows,
    );
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
    // `IoBridge::run` is an infinite event loop that only returns on EOF or
    // error; it must be aborted explicitly (see below) rather than awaited.
    let bridge_handle = tokio::task::spawn_local(async move {
        let _ = bridge.run().await;
    });

    let mut pump = SocketPairPump::new(peer);
    let base = TokioInstant::now();
    let client_addr = client_addr();
    let server_addr = server_addr();

    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256,
        indices_raw_enabled: true,
        supports_h264: false,
    };

    let mut framebuffer = FrameBuffer::new();
    let outcome = drive_session(
        cfg,
        &mut pump,
        client_addr,
        server_addr,
        base,
        &scene,
        &inject_tx,
        &mut framebuffer,
    )
    .await;

    // Always abort, on every exit path — the bridge task must not outlive
    // this function, and `outcome` may be an `Err`. The last snapshot
    // `run()`'s loop published into `bwe_cell` before this abort is
    // therefore a final snapshot, not a live running read — there is no
    // point after the abort where `run()` could publish again.
    bridge_handle.abort();

    let (events, bytes_delivered, bytes_dropped) = outcome?;

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
    let (probes_completed, probes_abandoned) = *probe_stats_cell
        .lock()
        .expect("probe_stats_publish mutex poisoned");

    Ok(BrowserlessResult {
        framebuffer,
        events,
        bytes_delivered,
        bytes_dropped,
        stale_generation_tiles,
        seed,
        bwe_estimate_bps: bwe_snapshot.bitrate_bps,
        implausible_rtt_samples: bwe_snapshot.implausible_rtt_samples,
        bwe_samples_seen: bwe_snapshot.samples_seen,
        retransmit_attempts_total: emitter_stats.retransmit_attempts_total,
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
    pump: &mut SocketPairPump,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    base: TokioInstant,
    scene: &BrowserlessScene,
    inject_tx: &mpsc::Sender<InjectedFrame>,
    framebuffer: &mut FrameBuffer,
) -> anyhow::Result<(Vec<ClientNetEvent>, u64, u64)> {
    let seed = scene.seed;

    let mut client = ClientNet::new(cfg, now_us(base))
        .map_err(|e| anyhow!("seed {seed}: ClientNet::new failed: {e}"))?;
    client
        .connect(server_addr, now_us(base))
        .map_err(|e| anyhow!("seed {seed}: ClientNet::connect failed: {e}"))?;

    // Independent RNG streams per direction (see module docs on netsim.rs
    // for why the seeds must differ): scene.seed for client->server,
    // scene.seed XORed against a fixed constant for server->client.
    let mut net_c2s = NetSim::new(scene.net.clone(), seed);
    let mut net_s2c = NetSim::new(scene.net.clone(), seed ^ 0xA5A5_A5A5_A5A5_A5A5);

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

    let mut events: Vec<ClientNetEvent> = Vec::new();
    let mut bytes_delivered: u64 = 0;
    let mut bytes_dropped: u64 = 0;

    // Per-tile generation counters for injected frames: a coordinate's
    // generation is the number of times it has previously appeared in an
    // earlier `FrameScript`, wrapping mod 16 (the wire field is 4 bits) —
    // see `inject_frame`.
    let mut generations: HashMap<(u8, u8), u8> = HashMap::new();
    let mut session_ready = false;
    let mut next_frame_idx: usize = 0;
    // Set once the session is ready and the scene has at least one frame;
    // `Some(t)` means "the next scheduled injection (a real scene frame, or
    // once those run out, a heartbeat — see below) is due at virtual time
    // `t` (us since `base`)".
    let mut next_inject_at_us: Option<u64> = None;
    // `seq` for heartbeat `InjectedFrame`s sent after `scene.frames` is
    // exhausted (see below). Starts at `scene.frames.len()`, one past the
    // highest `seq` any real scene frame uses, so a heartbeat can never
    // collide with a real frame's wire `frame_seq`.
    let mut heartbeat_seq: u32 = scene.frames.len() as u32;

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
    let overall_deadline = base + scene.duration;
    let mut iter: usize = 0;

    // Virtual time at the last iteration that made progress, and how many
    // iterations have turned since.
    let mut last_progress_vt: u64 = 0;
    let mut iters_without_progress: usize = 0;

    loop {
        iter += 1;
        let vt_now = now_us(base);
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
                scene.frames.len(),
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

        // Drain everything ClientNet currently has queued to transmit,
        // routing each datagram through the client->server NetSim.
        while let Some(out) = client.poll_transmit() {
            let t = now_us(base);
            for item in rule(
                &mut net_c2s,
                Direction::C2s,
                out.payload,
                t,
                &mut bytes_dropped,
            ) {
                if item.at_us <= t {
                    // Due now: send it here, inside the drain loop, so each
                    // send yields to the runtime exactly where it did before
                    // arrivals could be queued at all. Batching these and
                    // flushing them together changes where the runtime goes
                    // idle, and under `start_paused` that changes when the
                    // virtual clock advances.
                    pump.send(&item.payload, &client_addr).await.map_err(|e| {
                        anyhow!("seed {seed}: pump send failed at iteration {iter}: {e}")
                    })?;
                    bytes_delivered += item.payload.len() as u64;
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
        flush_due(
            &mut in_flight,
            now_us(base),
            pump,
            &mut client,
            client_addr,
            server_addr,
            &mut bytes_delivered,
        )
        .await
        .map_err(|e| anyhow!("seed {seed}: pump send failed at iteration {iter}: {e}"))?;

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
            if scene.frames.is_empty() {
                // Nothing to inject: preserve task 15a's exact behavior of
                // returning the instant the session is ready.
                break;
            }
            next_inject_at_us = Some(now_us(base));
        }

        if session_ready {
            if let Some(due_at) = next_inject_at_us {
                if now_us(base) >= due_at {
                    let spacing_us;
                    if next_frame_idx < scene.frames.len() {
                        inject_frame(scene, next_frame_idx, &mut generations, inject_tx)
                            .await
                            .map_err(|e| {
                                anyhow!("seed {seed}: frame {next_frame_idx} injection failed: {e}")
                            })?;
                        next_frame_idx += 1;
                        spacing_us = FRAME_SPACING_US;
                    } else {
                        // `scene.frames` is exhausted, but `scene.duration`
                        // may still have plenty of virtual time left, and a
                        // datagram dropped on its one and only send attempt
                        // needs *something* to keep giving it a chance to
                        // land. `IoBridge::sweep_rto_retransmits` (the
                        // server-side RTO wheel that actually resends a
                        // dropped, unacked tile datagram) is only ever
                        // called from three places: a real capture frame's
                        // post-dispatch path, `apply_injected_frame`'s own
                        // post-drain mirror of that, and the
                        // `Event::DatagramsUnblocked` handler — see that
                        // function's doc comment. In production the first
                        // of those fires unconditionally on a fixed timer
                        // for as long as a client stays connected (the
                        // capture loop free-runs regardless of screen
                        // dirtiness), so the RTO wheel is always getting
                        // swept somewhere. This harness has no such
                        // free-running capture loop: once `scene.frames`
                        // runs out, nothing would otherwise ever call
                        // `apply_injected_frame` again, and `DatagramsUnblocked`
                        // does not fire on its own absent a previously
                        // blocked send. Sending an empty-work `InjectedFrame`
                        // on the coarser `HEARTBEAT_SPACING_US` cadence (see
                        // its doc comment for why heartbeats don't reuse
                        // `FRAME_SPACING_US`) has no tile content to
                        // enqueue, but still reaches
                        // `apply_injected_frame`'s unconditional
                        // `sweep_rto_retransmits()` call — this is the
                        // harness's substitute for "a client stays
                        // connected and capture keeps ticking", not a new
                        // behavior IoBridge doesn't already have.
                        inject_heartbeat(heartbeat_seq, inject_tx)
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
                }
            }
        }

        // Fire an already-due ClientNet timeout without waiting on
        // anything, then re-check events/transmits from the top.
        if let Some(deadline_us) = client.poll_timeout() {
            if now_us(base) >= deadline_us {
                client.on_timeout(now_us(base));
                continue;
            }
        }

        // Wait for the next inbound frame from IoBridge, the earliest
        // deadline (ClientNet's own timer or the next scheduled
        // injection), or the scene's overall deadline — whichever comes
        // first. Capping by `overall_deadline` guarantees this select
        // cannot itself hang past the scene's budget even if nothing else
        // ever wakes it.
        let mut wake_at = overall_deadline;
        if let Some(d) = client.poll_timeout() {
            wake_at = wake_at.min(base + Duration::from_micros(d));
        }
        // A datagram in flight is its own wake-up reason: nothing else
        // necessarily fires at the moment it lands.
        if let Some(next) = in_flight.peek() {
            wake_at = wake_at.min(base + Duration::from_micros(next.at_us));
        }
        if session_ready {
            // Unconditional once `session_ready`: unlike the injection
            // branch above, waking up for the next due time applies
            // whether that next injection is a real scene frame or a
            // heartbeat (see above) — heartbeats keep being scheduled for
            // the rest of `scene.duration`, not just until `scene.frames`
            // runs out.
            if let Some(due_at) = next_inject_at_us {
                wake_at = wake_at.min(base + Duration::from_micros(due_at));
            }
        }

        tokio::select! {
            biased;
            recv_res = pump.recv() => {
                let pkt = recv_res.map_err(|e| anyhow!(
                    "seed {seed}: socketpair pump recv failed at iteration {iter} \
                     (last events observed: {events:?}): {e}"
                ))?;
                let t = now_us(base);
                for item in rule(&mut net_s2c, Direction::S2c, pkt.payload, t, &mut bytes_dropped)
                {
                    if item.at_us <= t {
                        client.handle_udp(&item.payload, server_addr, t);
                        bytes_delivered += item.payload.len() as u64;
                    } else {
                        in_flight.push(InFlight {
                            seq: next_in_flight_seq,
                            ..item
                        });
                        next_in_flight_seq += 1;
                    }
                }
            }
            _ = tokio::time::sleep_until(wake_at) => {
                if now_us(base) >= client.poll_timeout().unwrap_or(u64::MAX) {
                    client.on_timeout(now_us(base));
                }
            }
        }
    }

    Ok((events, bytes_delivered, bytes_dropped))
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
    generations: &mut HashMap<(u8, u8), u8>,
    inject_tx: &mpsc::Sender<InjectedFrame>,
) -> anyhow::Result<()> {
    let seed = scene.seed;
    let script = &scene.frames[frame_idx];
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
        timestamp_us: (frame_idx as u64 * FRAME_SPACING_US) as u32,
        // No `budget_bytes` here: `IoBridge::apply_injected_frame` derives
        // its own per-tick budget the same way the production capture path
        // does (`IoBridge::base_budget_bytes`), rather than accepting one
        // from the caller. The netsim's token bucket still does the link
        // capping, and `IoBridge` still clamps to quinn's actual send
        // capacity (`clamp_to_quinn_capacity`) — this only removes the
        // harness's own unbounded "drain everything" budget that sat in
        // front of both.
        work,
    };

    inject_tx
        .send(frame)
        .await
        .map_err(|e| anyhow!("seed {seed}: inject_tx send failed at frame {frame_idx}: {e}"))
}

/// Send an empty-work `InjectedFrame` — no tiles, nothing new to encode —
/// purely to reach `IoBridge::apply_injected_frame`'s unconditional
/// post-drain `sweep_rto_retransmits()` call. See the doc comment at the
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
async fn inject_heartbeat(seq: u32, inject_tx: &mpsc::Sender<InjectedFrame>) -> anyhow::Result<()> {
    let frame = InjectedFrame {
        seq,
        timestamp_us: (seq as u64 * FRAME_SPACING_US) as u32,
        work: Vec::new(),
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
/// Called once per scene-loop iteration. `now` is the current virtual
/// time, and is also the timestamp handed to `ClientNet::handle_udp` —
/// the client sees the datagram as arriving when it actually arrives,
/// not when it was sent.
#[allow(clippy::too_many_arguments)]
async fn flush_due(
    in_flight: &mut BinaryHeap<InFlight>,
    now: u64,
    pump: &mut SocketPairPump,
    client: &mut ClientNet,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    bytes_delivered: &mut u64,
) -> std::io::Result<()> {
    while in_flight.peek().is_some_and(|f| f.at_us <= now) {
        let f = in_flight.pop().expect("peek just confirmed a due datagram");
        match f.dir {
            Direction::C2s => pump.send(&f.payload, &client_addr).await?,
            Direction::S2c => client.handle_udp(&f.payload, server_addr, now),
        }
        *bytes_delivered += f.payload.len() as u64;
    }
    Ok(())
}
