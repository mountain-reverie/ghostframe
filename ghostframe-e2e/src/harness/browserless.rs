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

use std::collections::HashMap;
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
/// longer-duration scenes toward `drive_session`'s `MAX_ITERS` bail-out.
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

    Ok(BrowserlessResult {
        framebuffer,
        events,
        bytes_delivered,
        bytes_dropped,
        stale_generation_tiles,
        seed,
        bwe_estimate_bps: bwe_snapshot.bitrate_bps,
        implausible_rtt_samples: bwe_snapshot.implausible_rtt_samples,
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
    // hanging: `MAX_ITERS` guards against a pathological loop that somehow
    // keeps making "progress" without advancing virtual time, and
    // `overall_deadline` guards virtual time itself (derived from
    // `scene.duration`, the only time budget the scene declares).
    const MAX_ITERS: usize = 5_000;
    let overall_deadline = base + scene.duration;
    let mut iter: usize = 0;

    loop {
        iter += 1;
        if iter > MAX_ITERS {
            bail!(
                "seed {seed}: scene did not finish within {MAX_ITERS} iterations; \
                 last events observed: {events:?}"
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
            deliver_c2s(
                pump,
                &mut net_c2s,
                out.payload,
                client_addr,
                t,
                base,
                &mut bytes_delivered,
                &mut bytes_dropped,
            )
            .await
            .map_err(|e| anyhow!("seed {seed}: pump send failed at iteration {iter}: {e}"))?;
        }

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
                deliver_s2c(
                    &mut client,
                    &mut net_s2c,
                    pkt.payload,
                    server_addr,
                    t,
                    base,
                    &mut bytes_delivered,
                    &mut bytes_dropped,
                )
                .await;
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
        // Unpaced: the netsim's token bucket does the real capping, and
        // `IoBridge` clamps to quinn's actual send capacity
        // (`clamp_to_quinn_capacity`, io_bridge.rs:1154) so this cannot
        // overrun quinn's send buffer.
        budget_bytes: usize::MAX,
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
        budget_bytes: usize::MAX,
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

/// Sleep until virtual time reaches `at_us` since `base`, or return
/// immediately if that time has already passed.
async fn wait_until(base: TokioInstant, at_us: u64) {
    let target = base + Duration::from_micros(at_us);
    if TokioInstant::now() < target {
        tokio::time::sleep_until(target).await;
    }
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

/// Route one client-produced datagram through the client->server `NetSim`
/// and, if it survives, write it onto the socketpair pump for `IoBridge`
/// to consume. Corruption flips a bit in the payload itself, never in the
/// ghostbridge framing `pump.send` adds on top.
#[allow(clippy::too_many_arguments)]
async fn deliver_c2s(
    pump: &mut SocketPairPump,
    sim: &mut NetSim,
    payload: Vec<u8>,
    client_addr: SocketAddr,
    now_us_at_send: u64,
    base: TokioInstant,
    bytes_delivered: &mut u64,
    bytes_dropped: &mut u64,
) -> std::io::Result<()> {
    match sim.decide(payload.len(), now_us_at_send) {
        Verdict::Drop => {
            *bytes_dropped += payload.len() as u64;
        }
        Verdict::Deliver { at_us } => {
            wait_until(base, at_us).await;
            pump.send(&payload, &client_addr).await?;
            *bytes_delivered += payload.len() as u64;
        }
        Verdict::Duplicate { at_us, dup_at_us } => {
            wait_until(base, at_us).await;
            pump.send(&payload, &client_addr).await?;
            *bytes_delivered += payload.len() as u64;
            wait_until(base, dup_at_us).await;
            pump.send(&payload, &client_addr).await?;
            *bytes_delivered += payload.len() as u64;
        }
        Verdict::Corrupt { at_us, bit_index } => {
            let mut corrupted = payload;
            flip_bit(&mut corrupted, bit_index);
            wait_until(base, at_us).await;
            pump.send(&corrupted, &client_addr).await?;
            *bytes_delivered += corrupted.len() as u64;
        }
    }
    Ok(())
}

/// Route one server-produced datagram (already read off the socketpair
/// pump) through the server->client `NetSim` and, if it survives, feed it
/// into `ClientNet`. Corruption flips a bit in the payload itself, never
/// in the ghostbridge framing that already came off the wire.
#[allow(clippy::too_many_arguments)]
async fn deliver_s2c(
    client: &mut ClientNet,
    sim: &mut NetSim,
    payload: Vec<u8>,
    server_addr: SocketAddr,
    now_us_at_recv: u64,
    base: TokioInstant,
    bytes_delivered: &mut u64,
    bytes_dropped: &mut u64,
) {
    match sim.decide(payload.len(), now_us_at_recv) {
        Verdict::Drop => {
            *bytes_dropped += payload.len() as u64;
        }
        Verdict::Deliver { at_us } => {
            wait_until(base, at_us).await;
            let t = now_us(base);
            client.handle_udp(&payload, server_addr, t);
            *bytes_delivered += payload.len() as u64;
        }
        Verdict::Duplicate { at_us, dup_at_us } => {
            wait_until(base, at_us).await;
            let t = now_us(base);
            client.handle_udp(&payload, server_addr, t);
            *bytes_delivered += payload.len() as u64;
            wait_until(base, dup_at_us).await;
            let t2 = now_us(base);
            client.handle_udp(&payload, server_addr, t2);
            *bytes_delivered += payload.len() as u64;
        }
        Verdict::Corrupt { at_us, bit_index } => {
            let mut corrupted = payload;
            flip_bit(&mut corrupted, bit_index);
            wait_until(base, at_us).await;
            let t = now_us(base);
            client.handle_udp(&corrupted, server_addr, t);
            *bytes_delivered += corrupted.len() as u64;
        }
    }
}
