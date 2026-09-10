//! Task 15a: browserless netsim scene runner — transport bring-up only.
//!
//! Wires together a real `IoBridge` and a real `ghostframe_client_net::ClientNet`
//! across a `tokio::net::UnixStream` socketpair, with every datagram routed
//! through a [`crate::netsim::NetSim`] in each direction, under tokio's
//! virtual (paused) clock. This proves the WebTransport session can
//! establish over that whole path — the same wire framing, the same
//! `IoBridge` event loop, the same `ClientNet` sans-IO state machine that
//! production uses, just without tsnet or a browser.
//!
//! **Scope**: this module does NOT do tile injection, frame scripts, or
//! framebuffer assembly. `BrowserlessScene::frames` is accepted but unused
//! — see the `TODO(task-15b)` below for what will consume it. The public
//! types are shaped so that work slots in without changing this module's
//! wiring.
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

use std::net::{Ipv6Addr, SocketAddr};
use std::time::Duration;

use anyhow::{anyhow, bail};
use tokio::sync::mpsc;
use tokio::task::LocalSet;
use tokio::time::Instant as TokioInstant;

use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::io_bridge::{InjectedFrame, IoBridge};
use ghostframe_lib::transport::quic::QuicServer;

use crate::harness::framebuffer::FrameBuffer;
use crate::harness::scene_tiles::TileSpec;
use crate::netsim::{NetProfile, NetSim, SocketPairPump, Verdict};

/// One scene frame's worth of declared tile content, keyed by `(tile_x,
/// tile_y)`.
///
/// TODO(task-15b): this is unused by `run_browserless` today. Task 15b
/// will drain `BrowserlessScene::frames` through `scene_tiles::encode_tile`
/// into `InjectedFrame`s and send them over the `inject_tx` channel this
/// module already keeps alive.
pub struct FrameScript {
    pub tiles: Vec<((u8, u8), TileSpec)>,
}

/// A netsim scene to run browserlessly: a real `IoBridge` <-> `ClientNet`
/// session, no browser, no tsnet, driven under tokio's virtual clock.
pub struct BrowserlessScene {
    pub seed: u64,
    /// TODO(task-15b): accepted but not yet consumed. See `FrameScript`'s
    /// doc comment for what will drain this.
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
    /// Read from `FrameBuffer::stale_generation_tiles()` — this module does
    /// not maintain a second staleness definition.
    pub stale_generation_tiles: u32,
    pub seed: u64,
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

    // TODO(task-15b): `inject_tx` will carry `FrameScript`-derived
    // `InjectedFrame`s once tile injection lands. For now nothing is ever
    // sent on it; it is only kept alive (as `_inject_tx`) so the channel
    // does not close under `IoBridge`, which would surface as a spurious
    // `inject_rx = None` inside `IoBridge::run`'s select loop.
    let (inject_tx, inject_rx) = mpsc::channel::<InjectedFrame>(8);
    let _inject_tx = inject_tx;

    let mut bridge = IoBridge::new_with_injection_for_test(
        ours,
        server,
        inject_rx,
        scene.grid_cols,
        scene.grid_rows,
    );
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

    let outcome = drive_handshake(cfg, &mut pump, client_addr, server_addr, base, &scene).await;

    // Always abort, on every exit path — the bridge task must not outlive
    // this function, and `outcome` may be an `Err`.
    bridge_handle.abort();

    let (events, bytes_delivered, bytes_dropped) = outcome?;

    let framebuffer = FrameBuffer::new();
    let stale_generation_tiles = framebuffer.stale_generation_tiles();

    Ok(BrowserlessResult {
        framebuffer,
        events,
        bytes_delivered,
        bytes_dropped,
        stale_generation_tiles,
        seed,
    })
}

/// Drive the client<->server handshake to `SessionReady` (or fail loudly),
/// routing every datagram through a seeded `NetSim` per direction.
///
/// Returns the accumulated client events, bytes delivered, and bytes
/// dropped across both directions.
async fn drive_handshake(
    cfg: ClientNetConfig,
    pump: &mut SocketPairPump,
    client_addr: SocketAddr,
    server_addr: SocketAddr,
    base: TokioInstant,
    scene: &BrowserlessScene,
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

    // Two independent bounds so a stuck handshake fails loudly instead of
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
                "seed {seed}: handshake did not reach SessionReady within {MAX_ITERS} \
                 iterations; last events observed: {events:?}"
            );
        }
        if TokioInstant::now() >= overall_deadline {
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

        events.extend(client.take_events());
        if events.contains(&ClientNetEvent::SessionReady) {
            break;
        }

        // Fire an already-due ClientNet timeout without waiting on
        // anything, then re-check events/transmits from the top.
        if let Some(deadline_us) = client.poll_timeout() {
            if now_us(base) >= deadline_us {
                client.on_timeout(now_us(base));
                continue;
            }
        }

        // Wait for the next inbound frame from IoBridge, or the earliest
        // deadline (ClientNet's own timer, capped by the scene's overall
        // deadline), whichever comes first. Capping by `overall_deadline`
        // guarantees this select cannot itself hang past the scene's
        // budget even if ClientNet never arms a timer and IoBridge never
        // responds.
        let wake_at = match client.poll_timeout() {
            Some(d) => (base + Duration::from_micros(d)).min(overall_deadline),
            None => overall_deadline,
        };

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
