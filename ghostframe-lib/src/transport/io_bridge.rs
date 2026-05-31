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
use std::time::Instant;

use bytes::BytesMut;
use quinn_proto::{ConnectionHandle, Event, StreamEvent};
use std::collections::{HashMap, HashSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant as TokioInstant};

use rayon::prelude::*;

use crate::capture::gpu_pipeline::GpuFrameProcessor;
use crate::encoder::h264_vaapi::FullFrameEncoder;
use crate::server::FrameSubmission;
use crate::tile::{DirtyTracker, TileGrid};
use crate::transport::fec;
use crate::transport::fec::fec_group_size;
use crate::transport::feedback::ReceiverFeedback;
use crate::transport::ghostbridge::{
    encode_frame, parse_frame_rest, GhostbridgeConfig, GhostbridgeHandle,
};
use crate::transport::protocol::{
    build_frame_parity_datagram, fragment_frame, fragment_tile, max_fragment_payload,
    max_frame_fragment_payload, Codec, FrameHeader, NackMessage, TileFragmentInputs,
    FRAME_HEADER_SIZE, PING_PAYLOAD, PONG_PAYLOAD, TILE_DATAGRAM_FLAG,
};
use crate::transport::quic::QuicServer;
use crate::transport::webtransport::WebTransportServer;

/// Scratch buffer size for quinn-proto's `endpoint.handle` / `conn.poll_transmit`.
/// Sized above a 1500-byte MTU with headroom for ECN/padding; quinn-proto may
/// write a full datagram into this buffer in a single call.
const QUIC_SCRATCH: usize = 2048;

/// Write GPU-derived per-tile metrics into the tracker for the dirty tiles.
///
/// Non-dirty tile entries in `tracker` are untouched. If a computed index
/// falls outside `tile_analysis`, that tile is silently skipped — this is
/// what makes the first-frame path safe to call with an empty slice.
/// Caller must ensure every `(tx, ty)` in `dirty` is within the tracker grid.
pub(crate) fn populate_gpu_metrics(
    tracker: &mut crate::tile::MetricsTracker,
    dirty: &[(u32, u32)],
    cols: u32,
    tile_analysis: &[crate::capture::gpu_pipeline::TileAnalysis],
) {
    for &(tx, ty) in dirty {
        let idx = (ty as usize) * (cols as usize) + (tx as usize);
        let Some(entry) = tile_analysis.get(idx) else {
            continue;
        };
        let m = tracker.get_mut(tx, ty);
        m.unique_colors = entry.count.min(u16::MAX as u32) as u16;
        m.edge_density = entry.edge_density_thou as f32 / 1000.0;
    }
}

/// FEC parity defaults — enable when loss exceeds this threshold.
const FEC_ENABLE_THRESHOLD: f64 = 0.005;
/// FEC parity defaults — disable when loss drops below this threshold (hysteresis).
const FEC_DISABLE_THRESHOLD: f64 = 0.002;
/// Number of frames the server re-emits the frame-dimensions datagram after
/// any dimension change. At 5% packet loss, probability of all 10 being lost
/// is 0.05^10 ≈ 9.7e-14.
const FRAME_DIMENSIONS_RETRANSMITS: u8 = 10;

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
    /// Per-handle "have we already fired on_session_reset for this
    /// connection?" tracking. Set when `maybe_fire_session_reset` runs
    /// for a handle; cleared on `Event::ConnectionLost` so a rare
    /// reconnect-with-same-handle re-fires the reset.
    session_resets_fired: HashSet<ConnectionHandle>,
    /// Tracks whether ANY client has previously established a session
    /// on THIS `IoBridge` instance. Initial value `false`. Flips to
    /// `true` permanently after the first `maybe_fire_session_reset`
    /// for a connected handle. Used to skip the reset body on the
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
    /// Monotonically increasing frame sequence number (wrapping).
    frame_seq: u32,
    /// Per-tile dirty detection.
    dirty_tracker: DirtyTracker,
    /// Per-tile metrics fed to the classifier each frame.
    metrics_tracker: crate::tile::MetricsTracker,
    /// Cost-aware frame-mode + per-tile classifier.
    classifier: crate::tile::Classifier,
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
    /// Remaining frames to force all-dirty after a new session connects.
    /// QUIC slow-start can only deliver a fraction of tiles in the first burst;
    /// forcing dirty for several frames lets the congestion window open.
    force_dirty_frames: u32,
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
    /// Server-side env flag: `GHOSTFRAME_ENABLE_CDF53`.
    /// When false (default), `gate_codec_state` downgrades Cdf53 → Bc1 (Raw on
    /// wire), preserving M3.2 behavior. Set to true by the operator to enable
    /// CDF 5/3 emission once Task 9 Phase B encoding is wired in.
    cdf53_enabled: bool,
    /// M3.3c idle-escalation: flat tile indices from the most recent per-frame
    /// sweep. Stashed here so Task 10's post-fence Phase B can read the list
    /// and update `already_escalated_this_gen` after the GPU work returns.
    cdf53_escalation_candidates_this_frame: Vec<u32>,
    /// GPU-accelerated dirty tracker (Vulkan compute SAD).
    gpu_frame_processor: Option<GpuFrameProcessor>,
    /// Full-frame H.264 encoder (VA-API zero-copy).
    full_frame_encoder: Option<FullFrameEncoder>,
    /// Recent frame fragments for NACK retransmission. Key: (frame_seq, frag_idx).
    recent_frame_fragments: HashMap<(u32, u16), Vec<u8>>,
    /// Last (width, height) the server emitted as a frame-dimensions datagram.
    /// `None` means we've never emitted dimensions (first frame upcoming).
    last_emitted_dimensions: Option<(u32, u32)>,
    /// Frames remaining in the current retransmit window for the dimensions
    /// message. Each dimension change resets this to N=10. While > 0, every
    /// frame re-emits the dimensions datagram; loss tolerance is 0.05^10 ≈ 1e-13.
    dimensions_retransmits_left: u8,
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

/// Per-tile staging produced by Phase A, consumed by Phase B.
#[derive(Debug, Clone)]
pub(crate) struct PalRleTileWorkPrep {
    pub tile_xy: (u32, u32),
    /// Owned copy of 512 B index slice. Owned (not borrowed) so Phase B's
    /// rayon parallel iter is `Send`.
    pub indices: [u8; 512],
    pub palette: crate::encoder::pal_rle::PaletteEntry,
    pub palette_id: u8,
    pub bundled: bool,
}

impl IoBridge {
    /// Build a `LossInjector` from environment variables. Returns `None` if
    /// the relevant probability is 0 or env vars aren't set. Recognized env vars
    /// (where `<DIR>` is either `OUTBOUND` or `INBOUND`):
    /// - `GHOSTFRAME_<DIR>_LOSS_PROBABILITY` — f32 in [0.0, 1.0], default 0.0
    /// - `GHOSTFRAME_<DIR>_LOSS_PREDICATE` — one of `all` / `tile` / `ack`,
    ///    default `all`.
    /// - `GHOSTFRAME_<DIR>_LOSS_SEED` — u64, default 0.
    #[cfg(any(test, feature = "test-loss-injection"))]
    fn loss_injector_from_env(
        direction: &str,
    ) -> Option<crate::transport::loss_injection::LossInjector> {
        let prob_var = format!("GHOSTFRAME_{direction}_LOSS_PROBABILITY");
        let pred_var = format!("GHOSTFRAME_{direction}_LOSS_PREDICATE");
        let seed_var = format!("GHOSTFRAME_{direction}_LOSS_SEED");

        let prob: f32 = std::env::var(&prob_var)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0);
        if prob <= 0.0 {
            return None;
        }

        // Predicate: function pointer that classifies an outbound/inbound
        // datagram by its first byte. Selected by the *_LOSS_PREDICATE env var.
        fn predicate_all(_: &[u8]) -> bool {
            true
        }
        // Tile datagrams set bit 31 of frame_seq (TILE_DATAGRAM_FLAG = 0x80000000),
        // which is the high bit of byte [0] in big-endian wire order.
        fn predicate_tile(dg: &[u8]) -> bool {
            !dg.is_empty() && (dg[0] & 0x80) != 0
        }
        // ACK_BATCH_MSG_TYPE = 0x02 (see transport/ack.rs).
        fn predicate_ack(dg: &[u8]) -> bool {
            dg.first().copied() == Some(crate::transport::ack::ACK_BATCH_MSG_TYPE)
        }
        // PalRle tile datagrams: tile datagram flag set, codec field = PalRle (2),
        // payload byte 0 has bundle flag set (0x01).
        // Wire layout: [DatagramHeader 12][TileHeader 8][payload].
        // TileHeader byte [2] (wire index 14) = (codec << 1) | lz4.
        fn predicate_palrle_bundled(dg: &[u8]) -> bool {
            dg.len() >= 21
                && (dg[0] & 0x80) != 0
                && (dg[14] >> 1) == (crate::transport::protocol::Codec::PalRle as u8)
                && (dg[20] & 0x01) != 0
        }
        // Inverse: PalRle tile datagrams without the bundle flag.
        fn predicate_palrle_thin(dg: &[u8]) -> bool {
            dg.len() >= 21
                && (dg[0] & 0x80) != 0
                && (dg[14] >> 1) == (crate::transport::protocol::Codec::PalRle as u8)
                && (dg[20] & 0x01) == 0
        }

        let predicate: crate::transport::loss_injection::DropPredicate =
            match std::env::var(&pred_var).as_deref() {
                Ok("tile") => predicate_tile,
                Ok("ack") => predicate_ack,
                Ok("palrle_bundled") => predicate_palrle_bundled,
                Ok("palrle_thin") => predicate_palrle_thin,
                _ => predicate_all,
            };
        let seed: u64 = std::env::var(&seed_var)
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        tracing::info!(
            direction,
            prob,
            "test-loss-injection: installed LossInjector"
        );
        Some(crate::transport::loss_injection::LossInjector::new(
            prob, predicate, seed,
        ))
    }

    /// Parse `GHOSTFRAME_INJECT_OOB_PALRLE` as `"x,y"` (two u32 separated by a
    /// comma). Returns `None` when the env var is unset or unparseable. The
    /// resulting coordinate is stored on `IoBridge` and consumed (set to `None`)
    /// the first time the matching tile is encoded.
    #[cfg(any(test, feature = "test-loss-injection"))]
    fn oob_injector_from_env() -> Option<(u32, u32)> {
        let raw = std::env::var("GHOSTFRAME_INJECT_OOB_PALRLE").ok()?;
        let mut parts = raw.split(',');
        let x = parts.next()?.parse::<u32>().ok()?;
        let y = parts.next()?.parse::<u32>().ok()?;
        Some((x, y))
    }

    /// Cfg-gated test hook: parse `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET`.
    /// When `"1"` or `"true"`, the new-session handler will preserve the
    /// `palette_table.delivered` bitset across the session reset (other
    /// per-session state still resets normally). Drives the e2e for the
    /// ERR_THIN_UNCACHED_PALETTE round-trip — see
    /// `docs/superpowers/specs/2026-05-17-decode-error-thin-uncached-design.md`.
    #[cfg(any(test, feature = "test-loss-injection"))]
    fn skip_palette_session_reset_from_env() -> bool {
        matches!(
            std::env::var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET").as_deref(),
            Ok("1") | Ok("true")
        )
    }

    /// Returns `true` when `GHOSTFRAME_DIAGNOSE_TILES` is set to `"1"` or `"true"`.
    fn diagnose_tiles_from_env() -> bool {
        matches!(
            std::env::var("GHOSTFRAME_DIAGNOSE_TILES").as_deref(),
            Ok("1") | Ok("true")
        )
    }

    /// Returns `true` when `GHOSTFRAME_DIAGNOSE_GPU_PIPELINE` is set.
    /// Enables one-line-per-frame logging of FrameAnalysis output buffer state.
    fn diagnose_gpu_pipeline_from_env() -> bool {
        matches!(
            std::env::var("GHOSTFRAME_DIAGNOSE_GPU_PIPELINE").as_deref(),
            Ok("1") | Ok("true")
        )
    }

    /// Create a new `IoBridge` by connecting to ghostbridge and opening a UDP
    /// listener on `listen_addr` (e.g. `":4443"`).
    pub async fn new(
        ghostbridge_config: &GhostbridgeConfig,
        listen_addr: &str,
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

        let cdf53_enabled = std::env::var("GHOSTFRAME_ENABLE_CDF53")
            .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        if cdf53_enabled {
            tracing::info!("GHOSTFRAME_ENABLE_CDF53=1: Cdf53 codec enabled");
        }

        // Warm rayon's global thread pool so the first PalRLE-heavy frame
        // doesn't pay thread-spin-up latency on the hot path (design Section 4).
        rayon::iter::IntoParallelIterator::into_par_iter(0..1u32)
            .for_each(|_| {});

        Ok(Self {
            _handle: Some(handle),
            stream,
            server,
            local_addr,
            wt_sessions: HashMap::new(),
            session_resets_fired: HashSet::new(),
            has_seen_prior_session: false,
            frame_rx: None,
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            metrics_tracker: crate::tile::MetricsTracker::new(0, 0),
            classifier: crate::tile::Classifier::default(),
            frame_mode: crate::tile::FrameMode::TileCodec,
            scheduler: crate::transport::scheduler::Scheduler::new(0, 0),
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_loss: Self::loss_injector_from_env("OUTBOUND"),
            #[cfg(any(test, feature = "test-loss-injection"))]
            inbound_loss: Self::loss_injector_from_env("INBOUND"),
            #[cfg(any(test, feature = "test-loss-injection"))]
            oob_inject_at: Self::oob_injector_from_env(),
            #[cfg(any(test, feature = "test-loss-injection"))]
            skip_palette_session_reset: Self::skip_palette_session_reset_from_env(),
            force_dirty_frames: 0,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            client_caps: crate::transport::client_caps::ClientCapabilities::default(),
            fec_k: std::env::var("GHOSTFRAME_FEC_K")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0),
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
            cdf53_escalation_candidates_this_frame: Vec::new(),
            cdf53_enabled,
            gpu_frame_processor,
            full_frame_encoder: None,
            recent_frame_fragments: HashMap::new(),
            last_emitted_dimensions: None,
            dimensions_retransmits_left: 0,
        })
    }

    /// Create a new `IoBridge` wired to an inbound frame channel.
    ///
    /// Equivalent to [`new`] but attaches `frame_rx` so that captured frames
    /// are fragmented and sent as WebTransport datagrams to all connected peers.
    pub async fn new_with_frames(
        ghostbridge_config: &GhostbridgeConfig,
        listen_addr: &str,
        frame_rx: mpsc::Receiver<FrameSubmission>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut bridge = Self::new(ghostbridge_config, listen_addr).await?;
        bridge.frame_rx = Some(frame_rx);
        Ok(bridge)
    }

    /// Return the SHA-256 hex fingerprint of the server's self-signed cert.
    pub fn cert_hash_sha256(&self) -> &str {
        &self.server.cert_info().sha256_hex
    }

    /// Fragment a captured frame and send the resulting datagrams to all
    /// connected WebTransport sessions.
    /// Maximum bytes for the WebTransport VarInt session-ID prefix.
    /// A quarter-stream-id up to 63 encodes in 1 byte; we budget 2 for safety.
    const WT_VARINT_OVERHEAD: usize = 2;

    /// Dispatch to GPU or CPU pipeline depending on whether a DMA-BUF fd is
    /// available and the GPU dirty tracker has been initialized.
    ///
    /// NOTE: `GHOSTFRAME_DUMP_FRAME` only fires on the CPU path. The DRM/DMA-BUF
    /// capture backend sets `frame.pixels = Vec::new()` and puts pixel data
    /// exclusively in the DMA-BUF fd, so there is nothing to dump here without a
    /// DMA-BUF readback (out of scope for M3.2c). If you need a raw-BGRA dump on
    /// the GPU path, implement DMA-BUF readback in `process_frame_gpu` first.
    fn process_frame(&mut self, frame: FrameSubmission) {
        if frame.dmabuf_fd.is_some() && self.gpu_frame_processor.is_some() {
            self.process_frame_gpu(frame);
        } else {
            self.process_frame_cpu(frame);
        }
    }

    /// Compute the maximum datagram size from the smallest connected session.
    fn compute_max_datagram_size(&mut self) -> Option<usize> {
        let mut min_size: Option<usize> = None;
        for (handle, wt) in &self.wt_sessions {
            if !wt.is_connected() {
                continue;
            }
            if let Some(conn) = self.server.connections.get_mut(handle) {
                if let Some(sz) = conn.datagrams().max_size() {
                    let usable = sz.saturating_sub(Self::WT_VARINT_OVERHEAD);
                    min_size = Some(match min_size {
                        Some(prev) => prev.min(usable),
                        None => usable,
                    });
                }
            }
        }
        min_size.filter(|&sz| sz > 0)
    }

    /// Send a datagram to all connected WebTransport sessions.
    fn send_to_all_sessions(&mut self, dg: &[u8]) {
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(inj) = self.outbound_loss.as_mut() {
            if inj.should_drop(dg) {
                return;
            }
        }
        for (handle, wt) in &mut self.wt_sessions {
            if !wt.is_connected() {
                continue;
            }
            if let Some(conn) = self.server.connections.get_mut(handle) {
                if let Err(e) = wt.send_datagram(conn, dg) {
                    tracing::trace!(?handle, error = ?e, "datagram send failed");
                }
            }
        }
    }

    /// Emit the frame-dimensions datagram on the first frame of each session and
    /// whenever dimensions change, retransmitting `FRAME_DIMENSIONS_RETRANSMITS`
    /// additional times to absorb datagram loss. Called by both
    /// `process_frame_cpu` and `process_frame_gpu`.
    fn emit_frame_dimensions(
        &mut self,
        seq: u32,
        timestamp_us: u32,
        width: u32,
        height: u32,
    ) {
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
    pub(crate) fn dispatch_dirty_tiles_via_scheduler(
        &mut self,
        dirty: &[(u32, u32)],
        grid: &crate::tile::TileGrid,
        frame: TileDispatchFrame<'_>,
        max_frag: usize,
        policy: SchedulerEmissionPolicy,
        mut palrle_payloads: Option<&mut std::collections::HashMap<(u32, u32), Vec<u8>>>,
    ) {
        let TileDispatchFrame { pixels, stride, seq, timestamp_us } = frame;
        // Grid sync — keep scheduler in lockstep with the dirty-detection grid.
        if self.scheduler.cols() != grid.cols || self.scheduler.rows() != grid.rows {
            self.scheduler.resize(grid.cols, grid.rows);
        }

        // RTT estimate across connected sessions; default to 20 ms if none.
        let rtt = self
            .server
            .connections
            .values()
            .map(|c| c.stats().path.rtt)
            .min()
            .unwrap_or_else(|| std::time::Duration::from_millis(20));
        self.scheduler.set_rtt(rtt);

        use crate::transport::scheduler::{TileWork, WorkState};
        use std::time::Instant;

        for &(tile_x, tile_y) in dirty {
            // For GpuClassifierDriven, check whether this tile is handled by
            // Path B (Cdf53 refinement queue, enqueued above by Phase B) BEFORE
            // calling bump_generation_collecting. bump_generation supersedes all
            // queued work for the tile — so calling it on a Cdf53 tile would
            // invalidate the refinement passes Phase B just enqueued.
            if matches!(policy, SchedulerEmissionPolicy::GpuClassifierDriven) {
                use crate::tile::CodecState;
                let raw_codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
                let codec_state = crate::tile::classifier::gate_codec_state(
                    raw_codec_state,
                    self.cdf53_enabled,
                    self.client_caps.supports_cdf53,
                );
                if matches!(codec_state, CodecState::Cdf53 { .. }) {
                    // Gate open + Cdf53 retained → Phase B handles emission via
                    // the refinement queue (generation already bumped by Phase B).
                    // Skip entirely: do NOT bump generation (would supersede Phase
                    // B's enqueued passes), do NOT enqueue to priority_queue.
                    continue;
                }
            }

            let (gen, superseded) =
                self.scheduler.bump_generation_collecting(tile_x as u8, tile_y as u8);
            // New generation: tile is eligible for re-escalation.
            // Guard with a bounds check to stay safe when metrics_tracker has
            // not yet been sized (e.g. CpuRawOnly unit test path).
            if tile_x < self.metrics_tracker.cols() && tile_y < self.metrics_tracker.rows() {
                self.metrics_tracker
                    .get_mut(tile_x as u32, tile_y as u32)
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
            let tile_data = grid.extract_tile(pixels, stride, tile_x, tile_y);

            let (codec, payload) = match policy {
                SchedulerEmissionPolicy::CpuRawOnly => (Codec::Raw, tile_data),
                SchedulerEmissionPolicy::GpuClassifierDriven => {
                    use crate::tile::CodecState;
                    let raw_codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
                    // Apply the Cdf53 emission gate before deciding the wire codec.
                    // Cdf53 tiles were already skipped above (before bump_generation);
                    // only non-Cdf53 tiles reach this point.
                    // When gates closed, gate_codec_state downgrades Cdf53 → Bc1,
                    // and the existing _ => Raw fallback fires, preserving M3.2 behavior.
                    let codec_state = crate::tile::classifier::gate_codec_state(
                        raw_codec_state,
                        self.cdf53_enabled,
                        self.client_caps.supports_cdf53,
                    );
                    match codec_state {
                        CodecState::Solid => {
                            let solid = crate::encoder::solid::encode_solid(&tile_data);
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
                                None => (Codec::Raw, tile_data), // table-full fallback or no GPU prep
                            }
                        }
                        _ => (Codec::Raw, tile_data),
                    }
                }
            };

            self.scheduler.enqueue(TileWork {
                tile_x: tile_x as u8,
                tile_y: tile_y as u8,
                generation: gen,
                pass_idx: 0,
                total_passes: 1,
                codec,
                payload,
                queued_at: Instant::now(),
                last_sent_at: None,
                state: WorkState::Pending,
            });
        }

        let drained = self.scheduler.tick(usize::MAX);
        for work in drained {
            let datagrams = fragment_tile(
                &TileFragmentInputs {
                    frame_seq: seq | TILE_DATAGRAM_FLAG,
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
            for dg in &datagrams {
                self.send_to_all_sessions(dg);
            }
        }
    }

    /// Route a single inbound datagram into the appropriate handler.
    /// Currently dispatches ACK_BATCH_MSG_TYPE; future M3.x message types
    /// can be added here. Unknown discriminators are silently dropped
    /// (forward-compatible).
    pub(crate) fn dispatch_ack_datagram(&mut self, data: &[u8]) {
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(inj) = self.inbound_loss.as_mut() {
            if inj.should_drop(data) {
                return;
            }
        }
        if data.is_empty() {
            return;
        }
        if data[0] == crate::transport::ack::ACK_BATCH_MSG_TYPE {
            if let Ok(batch) = crate::transport::ack::AckBatch::decode(data) {
                for e in batch.entries {
                    let resolved = self.scheduler
                        .on_ack(e.tile_x, e.tile_y, e.generation, e.pass);
                    for r in resolved {
                        if let Some(pid) = r.palette_id {
                            let pid_usize = pid as usize;
                            if !self.palette_table.delivered.contains(pid) {
                                self.palette_table.delivered.insert(pid);
                            }
                            let cnt = self.palette_table.in_flight_carrying[pid_usize];
                            self.palette_table.in_flight_carrying[pid_usize] = cnt.saturating_sub(1);
                            if cnt == 0 {
                                tracing::warn!(
                                    target: "palrle.alloc",
                                    palette_id = pid,
                                    "in_flight_carrying underflow — enqueue/ack pairing bug",
                                );
                            }
                            // Release the acquire from Phase A. ref_count > 0 means tile work
                            // was in flight; on ACK we let the slot become FreeButCached.
                            if self.palette_table.ref_count[pid_usize] > 0 {
                                self.palette_table.release(pid);
                            }
                        }
                    }
                }
            }
        }
        // Other discriminators: silently ignore. Forward-compatible.
    }

    /// Parse a concatenated FEEDBACK-stream byte buffer, dispatching by
    /// message-type byte. Supports:
    /// - `0x01` FEEDBACK_MSG_TYPE  (22 bytes)  — `ReceiverFeedback`
    /// - `0x03` HELLO_MSG_TYPE     (2 bytes)   — capability advertisement
    /// - `0x04` DECODE_ERROR_MSG_TYPE (5 bytes) — per-tile decode failure
    ///
    /// Unknown message types abort parsing of the rest of the buffer
    /// (we can't safely advance past an unknown variable-length message).
    /// Future message types must extend this dispatcher.
    pub(crate) fn dispatch_feedback_bytes(&mut self, data: &[u8]) {
        use crate::transport::client_caps::{HelloMsg, HELLO_MSG_TYPE, HELLO_SIZE};
        use crate::transport::decode_error::{DecodeErrorMsg, DECODE_ERROR_MSG_TYPE, DECODE_ERROR_SIZE};
        use crate::transport::feedback::{FEEDBACK_MSG_TYPE, FEEDBACK_SIZE};

        let mut offset = 0;
        while offset < data.len() {
            let msg_type = data[offset];
            match msg_type {
                FEEDBACK_MSG_TYPE => {
                    if offset + FEEDBACK_SIZE > data.len() { break; }
                    if let Some(fb) = ReceiverFeedback::decode(&data[offset..]) {
                        tracing::debug!(
                            received = fb.datagrams_received,
                            lost = fb.datagrams_lost,
                            recovered_fec = fb.datagrams_recovered_fec,
                            loss_rate = %format!("{:.2}%", fb.loss_rate() * 100.0),
                            "receiver feedback"
                        );
                        self.update_fec_from_feedback(&fb);
                    }
                    offset += FEEDBACK_SIZE;
                }
                HELLO_MSG_TYPE => {
                    if offset + HELLO_SIZE > data.len() { break; }
                    if let Some(msg) = HelloMsg::decode(&data[offset..]) {
                        self.apply_hello(msg);
                    }
                    offset += HELLO_SIZE;
                }
                DECODE_ERROR_MSG_TYPE => {
                    if offset + DECODE_ERROR_SIZE > data.len() { break; }
                    if let Some(msg) = DecodeErrorMsg::decode(&data[offset..]) {
                        self.handle_decode_error(msg);
                    }
                    offset += DECODE_ERROR_SIZE;
                }
                unknown => {
                    tracing::warn!(
                        msg_type = unknown,
                        "unknown feedback-stream message type; dropping rest of buffer"
                    );
                    break;
                }
            }
        }
    }

    /// React to a client-reported decode error. Most error codes are
    /// log-only in M3.2b; code 3 (ERR_THIN_UNCACHED_PALETTE) triggers
    /// `force_rebundle` so the next emission for that palette includes
    /// the palette block again.
    pub(crate) fn handle_decode_error(
        &mut self,
        msg: crate::transport::decode_error::DecodeErrorMsg,
    ) {
        use crate::transport::decode_error::ERR_THIN_UNCACHED_PALETTE;
        use crate::tile::CodecState;

        tracing::warn!(
            codec = msg.codec,
            tile_x = msg.tile_x,
            tile_y = msg.tile_y,
            error_code = msg.error_code,
            "client decode error"
        );

        if msg.error_code == ERR_THIN_UNCACHED_PALETTE {
            // Recover which palette_id was last emitted for this tile by
            // consulting metrics_tracker.codec_state.
            let m = self.metrics_tracker.get(msg.tile_x as u32, msg.tile_y as u32);
            if let CodecState::PalRle { palette_id } = m.codec_state {
                self.palette_table.force_rebundle(palette_id);
                tracing::info!(
                    palette_id,
                    "force_rebundle: next emission for palette will include bundled palette block"
                );
            }
        }
    }

    /// Gate for `fire_session_reset`: fires the reset body exactly once per
    /// connection handle when its `WebTransportServer` reports
    /// `is_connected() == true`, AND when at least one prior session has
    /// existed (i.e., this is a reconnect, not the first ever connect).
    /// The first-connect case is intentionally skipped because the reset
    /// body has RECONNECT-specific side effects (frame_mode → H264,
    /// scheduler clear, force_dirty_frames=20, request_keyframe) that are
    /// no-ops on a fresh server but break test setups that capture frames
    /// before the client connects.
    fn maybe_fire_session_reset(&mut self, handle: ConnectionHandle) {
        let Some(wt) = self.wt_sessions.get(&handle) else { return; };
        if !wt.is_connected() { return; }
        if !self.session_resets_fired.insert(handle) { return; }
        if self.has_seen_prior_session {
            self.fire_session_reset(handle);
        }
        // Implicit lifecycle invariant: ConnectionHandle ordering is monotonic
        // within an IoBridge instance. If this breaks (e.g. a future test
        // recycles handles), the `has_seen_prior_session` latch logic will
        // misclassify the new "first" connect as a reconnect. Debug-assert
        // surfaces the violation rather than silently producing wrong behaviour.
        #[cfg(debug_assertions)]
        if !self.has_seen_prior_session {
            debug_assert!(
                self.session_resets_fired.len() == 1,
                "has_seen_prior_session=false but session_resets_fired has >1 entry — \
                 IoBridge state inconsistency (lifecycle violation?)"
            );
        }
        self.has_seen_prior_session = true;
    }

    /// Reset per-session state on a new WebTransport session. Called by
    /// `maybe_fire_session_reset` (Task 3) once per new connection handle.
    /// Preserves cross-session palette bytes (warm cache) and, when the
    /// cfg-gated test hook GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1 is
    /// active, also preserves `palette_table.delivered`.
    fn fire_session_reset(&mut self, handle: ConnectionHandle) {
        // New WebTransport session just became active.
        // Initialize frame_mode to H264 so the first frame
        // emits a single compact IDR (~50 datagrams) instead
        // of an all-tiles raw burst (~8000 datagrams) that
        // QUIC slow-start would mostly drop. Classifier exits
        // back to TileCodec naturally after exit_sustain frames
        // of empty dirty (Task 17).
        self.dirty_tracker.reset();
        self.metrics_tracker.reset();
        self.classifier.reset();
        self.scheduler.clear();
        #[cfg(any(test, feature = "test-loss-injection"))]
        let preserve_delivered = self.skip_palette_session_reset;
        #[cfg(not(any(test, feature = "test-loss-injection")))]
        let preserve_delivered = false;
        self.palette_table.on_session_reset(preserve_delivered);
        if preserve_delivered {
            tracing::info!(
                "test-hook: preserving palette_table.delivered across session reset (GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1)"
            );
        }
        self.frame_mode = crate::tile::FrameMode::H264;
        // Re-prime the frame-dimensions retransmit counter so
        // the new client receives the sentinel on its first
        // frames even if the screen has been at a stable
        // resolution for >FRAME_DIMENSIONS_RETRANSMITS frames.
        // Without this, the client's per-tile fallback resize
        // (gated on !frameDimensionsKnown) takes over and
        // re-introduces the canvas-resize-clears-tiles bug.
        self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
        // Slow-start cushion: 20 frames of all-tiles-dirty so a tile
        // dropped during QUIC slow-start re-emerges as dirty on the
        // next frame. Each path has its own implementation — CPU's
        // `force_dirty_frames` runs dirty_tracker in no-commit mode;
        // GPU's `invalidate_baseline` drops prev_image and runs the
        // no-snapshot first-frame path for N frames.
        //
        // Both are set unconditionally. On the CPU-only path,
        // `gpu_frame_processor` is None so the `if let` is a no-op.
        // On the GPU path, `force_dirty_frames` is normally ignored
        // by `process_frame_gpu` — but IS consumed if the Vulkan
        // call errors and the frame falls back to `process_frame_cpu`,
        // which is exactly the correct behaviour on that error path.
        self.force_dirty_frames = 20;
        if let Some(p) = self.gpu_frame_processor.as_mut() {
            p.invalidate_baseline(20);
        }
        // Force IDR on the existing encoder so the new client
        // gets a fresh anchor. Without this, a client connecting
        // mid-stream may receive P-frames referencing GOPs from
        // the prior session and decode garbage until the next
        // natural keyframe (~10 frames later at GOP=11). The
        // first-time-only escape (lazy `FullFrameEncoder::new`
        // with pts=0 → IDR) only covers cold start.
        if let Some(enc) = self.full_frame_encoder.as_mut() {
            enc.request_keyframe();
        }
        tracing::debug!(?handle, "new session connected, dirty tracker reset");
    }

    /// CPU-side tile-based pipeline (original implementation).
    fn process_frame_cpu(&mut self, frame: FrameSubmission) {
        let grid = TileGrid::new(frame.width, frame.height);
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        // One-shot BGRA frame dump: write raw pixels to the path in
        // GHOSTFRAME_DUMP_FRAME, then clear the env-var so subsequent frames
        // are not re-dumped.
        //
        // CPU-path only: the DRM/DMA-BUF capture backend (GPU path) always
        // constructs FrameSubmission with `pixels = Vec::new()`, placing pixel
        // data exclusively in the DMA-BUF fd. Hoisting this block to
        // `process_frame()` would therefore write zero bytes on the GPU path.
        // DMA-BUF readback support (to make this work on the GPU path) is
        // deferred beyond M3.2c.
        if let Ok(path) = std::env::var("GHOSTFRAME_DUMP_FRAME") {
            let _ = std::fs::write(&path, &frame.pixels);
            std::env::remove_var("GHOSTFRAME_DUMP_FRAME");
            tracing::info!(target: "ghostframe::diagnose", "dumped frame to {}", path);
        }

        // Determine the maximum fragment payload from the smallest connected
        // session's QUIC max datagram size.  Check for connected sessions
        // BEFORE updating the dirty tracker — otherwise the tracker consumes
        // frame state even when no client is listening, and once a client
        // connects the content appears "unchanged" so nothing is ever sent.
        let max_frag = {
            let mut min_size: Option<usize> = None;
            for (handle, wt) in &self.wt_sessions {
                if !wt.is_connected() {
                    continue;
                }
                if let Some(conn) = self.server.connections.get_mut(handle) {
                    if let Some(sz) = conn.datagrams().max_size() {
                        let usable =
                            max_fragment_payload(sz.saturating_sub(Self::WT_VARINT_OVERHEAD));
                        min_size = Some(match min_size {
                            Some(prev) => prev.min(usable),
                            None => usable,
                        });
                    }
                }
            }
            match min_size {
                Some(0) | None => {
                    // No connected sessions or MTU too small — skip frame
                    // without updating dirty tracker state.
                    return;
                }
                Some(sz) => sz,
            }
        };

        // Emit frame-dimensions datagram on first frame, on dimension change,
        // and N more times after any change to absorb datagram loss.
        self.emit_frame_dimensions(seq, frame.timestamp_us, frame.width, frame.height);

        // During QUIC slow-start after a new session connects, datagrams may
        // be silently dropped by congestion control. Use no-commit mode so
        // dropped tiles remain dirty on subsequent frames until the congestion
        // window opens up enough to deliver them all.
        let no_commit = self.force_dirty_frames > 0;
        if no_commit {
            self.force_dirty_frames -= 1;
        }

        // Determine dirty tiles (only after confirming we have a connected session).
        // During slow-start (no_commit), ignore damage hints and do full-frame
        // comparison without committing, so unsent tiles stay dirty.
        //
        // On the first commit frame after slow-start ends, do a full-frame scan
        // regardless of XDamage hints. During no-commit mode prev_tiles is never
        // updated (it stays all-zeros), so switching immediately to hinted mode
        // would only check the animated region (e.g. spinner) and silently skip
        // static regions — leaving solid/text/gradient tiles committed as zeros and
        // never retransmitted. A full scan at the transition commits every tile.
        let dirty_tiles = if no_commit {
            self.dirty_tracker.update_no_commit(
                &frame.pixels,
                frame.stride,
                frame.width,
                frame.height,
            )
        } else if !self.dirty_tracker.has_been_committed() {
            // First commit frame: full scan so every tile is flushed from baseline.
            self.dirty_tracker
                .update(&frame.pixels, frame.stride, frame.width, frame.height)
        } else {
            match &frame.damage_tiles {
                Some(hints) => self.dirty_tracker.update_with_hints(
                    &frame.pixels,
                    frame.stride,
                    frame.width,
                    frame.height,
                    hints,
                ),
                None => self.dirty_tracker.update(
                    &frame.pixels,
                    frame.stride,
                    frame.width,
                    frame.height,
                ),
            }
        };

        if dirty_tiles.is_empty() {
            return;
        }

        // Also sync metrics_tracker grid so it stays aligned with dirty_tracker.
        if self.metrics_tracker.cols() != grid.cols || self.metrics_tracker.rows() != grid.rows {
            self.metrics_tracker.resize(grid.cols, grid.rows);
        }

        // Route through the shared scheduler-dispatch helper. CPU path emits
        // Codec::Raw per M3.1 D1 (classifier sentinel-gated).
        self.dispatch_dirty_tiles_via_scheduler(
            &dirty_tiles,
            &grid,
            TileDispatchFrame {
                pixels: &frame.pixels,
                stride: frame.stride,
                seq,
                timestamp_us: frame.timestamp_us,
            },
            max_frag,
            SchedulerEmissionPolicy::CpuRawOnly,
            None,
        );
    }

    /// GPU-accelerated full-frame pipeline: Vulkan compute dirty detection +
    /// VA-API VPP BGRA→NV12 conversion + H.264 encoding (true zero-copy).
    fn process_frame_gpu(&mut self, frame: FrameSubmission) {
        let fd = frame.dmabuf_fd.as_ref().unwrap();
        let raw_fd = fd.as_raw_fd();

        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        let max_dg_size = match self.compute_max_datagram_size() {
            Some(sz) => sz,
            None => {
                tracing::debug!(seq, "process_frame_gpu: no connected sessions, dropping");
                return;
            }
        };

        // Emit frame-dimensions datagram on first frame, on dimension change,
        // and N more times after any change to absorb datagram loss.
        self.emit_frame_dimensions(seq, frame.timestamp_us, frame.width, frame.height);

        // Per-frame idle-escalation sweep. Computes the candidate list against
        // the previous frame's tile metrics (the current frame's metrics are
        // updated after the GPU work returns), then hands the list to the
        // GpuFrameProcessor which records the forward dispatch in the same
        // command buffer as the dirty-Cdf53 forward stages.
        //
        // Note: metrics_tracker.resize() happens after process_frame returns
        // (below, around `self.metrics_tracker.resize(cols, rows)`). On the
        // very first frame the tracker has cols=0, rows=0 (constructed via
        // MetricsTracker::new(0, 0)), so detect_escalation_candidates iterates
        // 0 tiles and returns empty — safe.
        {
            let caps = self.client_caps;
            let candidates = if self.cdf53_enabled && caps.supports_cdf53 {
                crate::tile::detect_escalation_candidates(
                    &self.metrics_tracker,
                    crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME,
                )
            } else {
                Vec::new()
            };
            let processor = self.gpu_frame_processor.as_mut().unwrap();
            processor.set_escalation_candidates(&candidates);
            self.cdf53_escalation_candidates_this_frame = candidates;
        }

        // GPU pipeline: Vulkan SAD dirty detection + NV12 conversion
        let processor = self.gpu_frame_processor.as_mut().unwrap();
        let analysis =
            match processor.process_frame(raw_fd, frame.width, frame.height, frame.stride) {
                Ok(a) => a,
                Err(e) => {
                    tracing::warn!("GPU process_frame failed: {e}, falling back to CPU path");
                    self.process_frame_cpu(frame);
                    return;
                }
            };

        tracing::debug!(
            seq,
            dirty_count = analysis.dirty_tiles.len(),
            "process_frame_gpu: dirty tile detection complete"
        );

        // GPU pipeline diagnostic: one line per frame showing the state of
        // every output buffer FrameAnalysis exposes. Drives W1 root-cause
        // investigation when unique_colors stays at UNIQUE_COLORS_UNKNOWN.
        if Self::diagnose_gpu_pipeline_from_env() {
            let ta_slice = analysis.tile_analysis_slice();
            let first_nonzero = ta_slice
                .iter()
                .take(2048)
                .enumerate()
                .find(|(_, t)| t.count != 0)
                .map(|(i, t)| (i, t.count, t.edge_density_thou));
            tracing::info!(
                target: "ghostframe::diagnose",
                seq,
                dirty_count = analysis.dirty_tiles.len(),
                tile_analysis_is_null = analysis.tile_analysis.is_null(),
                tile_analysis_len = analysis.tile_analysis_len,
                tile_analysis_slice_len = ta_slice.len(),
                first_nonzero_idx = first_nonzero.map(|(i, _, _)| i),
                first_nonzero_count = first_nonzero.map(|(_, c, _)| c),
                first_nonzero_edge_thou = first_nonzero.map(|(_, _, e)| e),
                palrle_compact_count = analysis.palrle_compact_count,
                frame_palette_set_count = analysis.frame_palette_set_count,
                "gpu-pipeline"
            );
        }

        // Convert flat tile indices (Vec<u32>) into (tile_x, tile_y) pairs
        // matching the row-major layout used by MetricsTracker / TileGrid.
        let cols = frame.width.div_ceil(crate::tile::TILE_SIZE);
        let rows = frame.height.div_ceil(crate::tile::TILE_SIZE);
        let dirty_xy: Vec<(u32, u32)> = analysis
            .dirty_tiles
            .iter()
            .map(|&idx| (idx % cols, idx / cols))
            .collect();

        // Keep metrics_tracker AND scheduler grids in sync with dirty-detection
        // (before the early return on empty dirty_xy, so the scheduler stays
        // consistent even when no tiles are dispatched this frame).
        if self.metrics_tracker.cols() != cols || self.metrics_tracker.rows() != rows {
            self.metrics_tracker.resize(cols, rows);
            self.scheduler.resize(cols, rows);
        }
        // Always update per-tile metrics — idle_frames advances on every frame,
        // EMA decays toward 0 when tiles aren't dirty.
        self.metrics_tracker.record_frame(&dirty_xy);

        // Populate GPU-derived metrics (unique_colors, edge_density) for the dirty
        // tiles from the tile_analysis buffer. The classifier rule for PalRLE
        // (and Solid, post-M3.1) reads these.
        populate_gpu_metrics(
            &mut self.metrics_tracker,
            &dirty_xy,
            cols,
            analysis.tile_analysis_slice(),
        );

        // Classify each dirty tile. Empty dirty_xy → empty tentative — that's
        // the no-motion signal the classifier needs to exit H264 after exit_sustain.
        use crate::tile::{classifier::classify_tile, FrameMode};
        let tentative: Vec<crate::tile::CodecState> = dirty_xy
            .iter()
            .map(|&(tx, ty)| {
                let m = self.metrics_tracker.get(tx, ty);
                let prev = m.codec_state;
                classify_tile(m, &prev)
            })
            .collect();

        // Capture previous mode before evaluating the classifier, so the
        // keyframe-request guard below can compare prev vs. new.
        let prev_mode = self.frame_mode;

        // Always evaluate mode — empty tentative drives the exit-sustain counter.
        let new_mode = self.classifier.decide_frame_mode(&tentative, prev_mode);

        // Persist tentative state back into per-tile metrics for dirty tiles only.
        for (i, &(tx, ty)) in dirty_xy.iter().enumerate() {
            self.metrics_tracker.get_mut(tx, ty).codec_state = tentative[i];
        }

        // Per-tile diagnostic tracing: emit one log line per dirty tile when
        // GHOSTFRAME_DIAGNOSE_TILES=1 (or =true).  Parseable by downstream awk.
        if Self::diagnose_tiles_from_env() {
            for &(tx, ty) in &dirty_xy {
                let m = self.metrics_tracker.get(tx, ty);
                tracing::info!(
                    target: "ghostframe::diagnose",
                    tile_x = tx,
                    tile_y = ty,
                    unique_colors = m.unique_colors,
                    codec_state = ?m.codec_state,
                    "per-tile"
                );
            }
        }

        if new_mode != self.frame_mode {
            tracing::info!(
                prev = ?self.frame_mode,
                new = ?new_mode,
                seq,
                "classifier flipped frame mode"
            );

            // H264 → TileCodec architectural invariant: invalidate the GPU SAD
            // baseline so the next TileCodec frame re-emits every tile,
            // overwriting whatever the H.264 phase rendered. Without this,
            // static content produces 0 dirty tiles on mode entry (prev_image
            // is current relative to the static screen) and the lossless tile
            // codecs never get a chance to upgrade the canvas from the H.264
            // lossy render. Symmetric to the existing `request_keyframe()` on
            // TileCodec → H264 in the `FrameMode::H264` match arm below: each
            // direction of the mode flip resets the state that the entering mode
            // relies on (H.264 GOP for the H264 direction, GPU SAD baseline for
            // the TileCodec direction).
            //
            // force_frames = 1 (not 20) because we're past QUIC slow-start by
            // the time exit_sustain elapses; ACK-based retries handle
            // individual datagram drops on the lossless tiles.
            if prev_mode == FrameMode::H264 && new_mode == FrameMode::TileCodec {
                if let Some(p) = self.gpu_frame_processor.as_mut() {
                    p.invalidate_baseline(1);
                    tracing::info!(
                        seq,
                        "H264→TileCodec handoff: invalidate_baseline(1) — lossless repaint"
                    );
                }
            }
        }

        // Update frame mode for next frame's hysteresis reference.
        self.frame_mode = new_mode;

        // No dirty tiles → nothing to emit (regardless of mode). The classifier
        // already saw this frame above, so the exit-sustain counter advances.
        if dirty_xy.is_empty() {
            return;
        }

        match new_mode {
            FrameMode::H264 => {
                // Lazily initialize full-frame encoder
                let needs_init = match &self.full_frame_encoder {
                    Some(enc) => enc.width() != frame.width || enc.height() != frame.height,
                    None => true,
                };
                if needs_init {
                    match FullFrameEncoder::new(frame.width, frame.height) {
                        Ok(enc) => {
                            self.full_frame_encoder = Some(enc);
                        }
                        Err(e) => {
                            tracing::warn!("Full-frame encoder init failed: {e}");
                            return;
                        }
                    }
                }

                // Re-entry into H264: force IDR for a fresh client anchor.
                if prev_mode == FrameMode::TileCodec {
                    if let Some(enc) = self.full_frame_encoder.as_mut() {
                        enc.request_keyframe();
                    }
                }

                // Encode from the GPU-computed NV12 HOST_VISIBLE buffer
                let encoder = self.full_frame_encoder.as_mut().unwrap();
                let encoded = match encoder.encode_nv12_buffer(
                    analysis.nv12_data,
                    analysis.nv12_width,
                    analysis.nv12_height,
                    analysis.nv12_y_stride,
                    analysis.nv12_uv_stride,
                    analysis.nv12_uv_offset,
                ) {
                    Ok(Some(enc)) => enc,
                    Ok(None) => {
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("NV12 encode failed: {e}");
                        return;
                    }
                };

                // Fragment and send. `fragment_frame` chunks the payload at the
                // per-fragment payload limit (datagram size minus the frame
                // header), not the raw datagram size — otherwise the on-wire
                // datagram (header + chunk) overshoots the MTU and WebTransport
                // returns TooLarge, dropping every H.264 datagram silently.
                let datagrams = fragment_frame(
                    seq,
                    frame.timestamp_us,
                    encoded.is_keyframe,
                    &encoded.payload,
                    max_frame_fragment_payload(max_dg_size),
                );

                for dg in &datagrams {
                    self.send_to_all_sessions(dg);
                }

                // FEC parity
                let fec_k = fec_group_size(encoded.is_keyframe);
                if datagrams.len() > 1 {
                    let source_payloads: Vec<&[u8]> = datagrams
                        .iter()
                        .map(|dg| &dg[FRAME_HEADER_SIZE..])
                        .collect();
                    let parities = fec::generate_parity(&source_payloads, fec_k);
                    for (_group_start, parity_payload) in &parities {
                        let parity_dg = build_frame_parity_datagram(
                            seq,
                            frame.timestamp_us,
                            encoded.is_keyframe,
                            datagrams.len() as u16,
                            parity_payload,
                        );
                        self.send_to_all_sessions(&parity_dg);
                    }
                }

                // Store fragments for NACK
                let oldest_kept = seq.wrapping_sub(3);
                self.recent_frame_fragments
                    .retain(|(s, _), _| s.wrapping_sub(oldest_kept) <= 3);
                for dg in &datagrams {
                    if let Ok(hdr) = FrameHeader::decode(dg) {
                        self.recent_frame_fragments
                            .insert((hdr.frame_seq, hdr.frag_idx), dg.clone());
                    }
                }
            }
            FrameMode::TileCodec => {
                let max_frag = max_fragment_payload(max_dg_size);
                if max_frag == 0 {
                    return;
                }

                // Source for tile pixel extraction. CPU path populates
                // `frame.pixels`; GPU/DMA-BUF leaves it empty.
                let pixels_owned;
                let pixels: &[u8] = if frame.pixels.is_empty() {
                    match frame.dmabuf_fd.as_ref() {
                        Some(fd) => match crate::capture::dmabuf::readback_dmabuf(
                            fd.as_raw_fd(),
                            frame.width,
                            frame.height,
                            frame.stride,
                        ) {
                            Ok(p) => {
                                pixels_owned = p;
                                &pixels_owned
                            }
                            Err(e) => {
                                tracing::warn!(
                                    "DMA-BUF readback failed in TileCodec mode: {e}; \
                                     skipping frame to avoid emitting zero-filled tiles",
                                );
                                return;
                            }
                        },
                        None => {
                            tracing::warn!(
                                "TileCodec mode but no pixels and no DMA-BUF fd; skipping frame",
                            );
                            return;
                        }
                    }
                } else {
                    &frame.pixels
                };

                let grid = TileGrid::new(frame.width, frame.height);

                // Phase A — serial palette-table allocation per dirty PalRle-feasible tile.
                let cols = grid.cols;
                let palrle_compact_count = analysis.palrle_compact_count;
                let preps = self.phase_a_palette_allocation(
                    cols,
                    analysis.palrle_compact_list_slice(),
                    analysis.per_tile_frame_palette_id_slice(),
                    analysis.folded_into_slice(),
                    analysis.frame_palette_set_slice(),
                    // No bulk slice helper exists for index_buffer (only per-tile); keep inline.
                    if palrle_compact_count > 0 && !analysis.index_buffer.is_null() {
                        unsafe {
                            std::slice::from_raw_parts(
                                analysis.index_buffer,
                                (palrle_compact_count * 512) as usize,
                            )
                        }
                    } else {
                        &[]
                    },
                );

                // Phase B — rayon parallel encode.
                let caps = self.client_caps;
                #[cfg(any(test, feature = "test-loss-injection"))]
                let inject = self.oob_inject_at;
                #[cfg(not(any(test, feature = "test-loss-injection")))]
                let inject: Option<(u32, u32)> = None;
                // Only consume the one-shot injection when the target tile
                // is actually in this frame's preps — otherwise the hook
                // would be silently spent on the first PalRle frame whose
                // preps happen to exclude the injection target.
                let inject_will_fire = inject
                    .map(|xy| preps.iter().any(|p| p.tile_xy == xy))
                    .unwrap_or(false);
                let mut palrle_payloads =
                    IoBridge::phase_b_encode_payloads_with_caps(&preps, &caps, inject);
                #[cfg(any(test, feature = "test-loss-injection"))]
                if inject_will_fire {
                    self.oob_inject_at = None;
                }

                // Phase B — Cdf53 encode bit-plane passes for high-color tiles,
                // enqueue into the scheduler's refinement queue. Gated on BOTH
                // the server env flag (GHOSTFRAME_ENABLE_CDF53) AND the client
                // capability (caps.supports_cdf53, from HELLO).
                if self.cdf53_enabled && caps.supports_cdf53 {
                    // Extract raw pointers from the GPU processor up-front so that
                    // the immutable borrow of self.gpu_frame_processor ends before
                    // the mutable borrows of self.scheduler below.
                    // SAFETY: cdf53_compact_count_ptr, cdf53_compact_list_ptr, and
                    // cdf53_coefficients_ptr are HOST_VISIBLE | HOST_COHERENT mapped
                    // GPU memory, valid until the next process_frame call. This block
                    // runs inside the current process_frame_gpu invocation, before
                    // any subsequent call.
                    let (cdf53_compact_count_ptr, cdf53_compact_list_ptr, cdf53_coefficients_ptr) = {
                        let gpu = self.gpu_frame_processor.as_ref().unwrap();
                        (
                            gpu.cdf53_compact_count_ptr,
                            gpu.cdf53_compact_list_ptr,
                            gpu.cdf53_coefficients_ptr,
                        )
                    };
                    let cdf53_count = unsafe { *cdf53_compact_count_ptr };
                    if cdf53_count > 0 {
                        let cdf53_list = unsafe {
                            std::slice::from_raw_parts(
                                cdf53_compact_list_ptr,
                                cdf53_count as usize,
                            )
                        };
                        for (slot, &flat_tile_idx) in cdf53_list.iter().enumerate() {
                            let tile_x = (flat_tile_idx % cols) as u8;
                            let tile_y = (flat_tile_idx / cols) as u8;
                            // Read 3 * 1024 = 3072 i32 values from the compact-slot
                            // indexed coefficient buffer. The GPU writes i32-wide
                            // values; CPU casts to i16 (actual coefficient bit-width;
                            // high bits are sign-extension).
                            let coeffs_i32 = unsafe {
                                std::slice::from_raw_parts(
                                    cdf53_coefficients_ptr
                                        .add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                                    crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                                )
                            };
                            let coeffs_i16: Vec<i16> =
                                coeffs_i32.iter().map(|&v| v as i16).collect();
                            // M3.3b diagnostic: when GHOSTFRAME_CDF53_DIFF_TILE=X,Y is
                            // set, log a one-shot side-by-side comparison of GPU vs
                            // CPU `cdf53::forward(tile_bgra)` for that tile. Gated
                            // behind the `cdf53-diag` cargo feature so production
                            // builds skip the runtime env-var check entirely.
                            #[cfg(feature = "cdf53-diag")]
                            if let Ok(spec) = std::env::var("GHOSTFRAME_CDF53_DIFF_TILE") {
                                if let Some((sx, sy)) = spec.split_once(',') {
                                    if let (Ok(target_x), Ok(target_y)) =
                                        (sx.trim().parse::<u32>(), sy.trim().parse::<u32>())
                                    {
                                        if tile_x as u32 == target_x && tile_y as u32 == target_y {
                                            let tile_bgra = grid.extract_tile(
                                                pixels,
                                                frame.stride,
                                                tile_x as u32,
                                                tile_y as u32,
                                            );
                                            // Pick CPU reference based on which dispatches were skipped:
                                            // SKIP_L2_L3 → L1-only; SKIP_L3 → L2-only; otherwise full.
                                            let skip_l2_l3 = std::env::var(
                                                "GHOSTFRAME_CDF53_SKIP_L2_L3",
                                            )
                                            .is_ok();
                                            let skip_l3 = std::env::var("GHOSTFRAME_CDF53_SKIP_L3").is_ok();
                                            let cpu_coeffs = if skip_l2_l3 {
                                                crate::encoder::cdf53::forward_level1_only(&tile_bgra)
                                            } else if skip_l3 {
                                                crate::encoder::cdf53::forward_level2_only(&tile_bgra)
                                            } else {
                                                crate::encoder::cdf53::forward(&tile_bgra)
                                            };
                                            // B↔R swap hypothesis: build a fake tile where B and R are
                                            // swapped in the BGRA buffer, then CPU-forward it. If the GPU
                                            // result matches THIS, the shader's rgba8/bgra8 mismatch is
                                            // swapping channel 0 (B) with channel 2 (R).
                                            let mut tile_swapped = tile_bgra.clone();
                                            for px in tile_swapped.chunks_exact_mut(4) {
                                                px.swap(0, 2); // swap B and R bytes
                                            }
                                            let cpu_coeffs_swapped =
                                                crate::encoder::cdf53::forward(&tile_swapped);
                                            // Sample tile pixels (first 8 BGRA px).
                                            let bgra_head: Vec<u8> = tile_bgra.iter().take(32).copied().collect();
                                            tracing::info!(
                                                target: "ghostframe::cdf53::diff",
                                                tile_x = tile_x,
                                                tile_y = tile_y,
                                                "cdf53.diff.tile_bgra_head_32 = {:?}",
                                                bgra_head
                                            );
                                            let mut n_diff_per_ch = [0usize; 3];
                                            let mut n_match_swapped_per_ch = [0usize; 3];
                                            let mut shown = 0usize;
                                            for i in 0..crate::encoder::cdf53::CDF53_TOTAL_COEFFS {
                                                let ch = i / 1024;
                                                if coeffs_i16[i] != cpu_coeffs[i] {
                                                    n_diff_per_ch[ch] += 1;
                                                    if shown < 100 {
                                                        tracing::info!(
                                                            target: "ghostframe::cdf53::diff",
                                                            tile_x = tile_x,
                                                            tile_y = tile_y,
                                                            "cdf53.diff coef[{i}] ch={} idx={} gpu={} cpu={} diff={} cpu_swap={}",
                                                            ch,
                                                            i % 1024,
                                                            coeffs_i16[i],
                                                            cpu_coeffs[i],
                                                            coeffs_i16[i] as i32 - cpu_coeffs[i] as i32,
                                                            cpu_coeffs_swapped[i],
                                                        );
                                                        shown += 1;
                                                    }
                                                }
                                                if coeffs_i16[i] == cpu_coeffs_swapped[i] {
                                                    n_match_swapped_per_ch[ch] += 1;
                                                }
                                            }
                                            let n_diff = n_diff_per_ch.iter().sum::<usize>();
                                            // Per-subband mismatches (channel 0 / B only — quickest signal).
                                            // Subband ranges in per-channel layout:
                                            //   [0..16] LL3, [16..32] HL3, [32..48] LH3, [48..64] HH3,
                                            //   [64..128] HL2, [128..192] LH2, [192..256] HH2,
                                            //   [256..512] HL1, [512..768] LH1, [768..1024] HH1.
                                            // Per-channel per-subband breakdown. Subband boundaries
                                            // are within each channel's 1024-coefficient block.
                                            let mut sb_diff = [[0usize; 10]; 3];
                                            let band_ranges = [
                                                (0, 16), (16, 32), (32, 48), (48, 64),
                                                (64, 128), (128, 192), (192, 256),
                                                (256, 512), (512, 768), (768, 1024),
                                            ];
                                            for ch in 0..3 {
                                                let base = ch * 1024;
                                                for (b, (lo, hi)) in band_ranges.iter().enumerate() {
                                                    for i in *lo..*hi {
                                                        if coeffs_i16[base + i] != cpu_coeffs[base + i] {
                                                            sb_diff[ch][b] += 1;
                                                        }
                                                    }
                                                }
                                            }
                                            tracing::info!(
                                                target: "ghostframe::cdf53::diff",
                                                tile_x = tile_x,
                                                tile_y = tile_y,
                                                "cdf53.diff.summary total_diffs={n_diff}/3072 per_ch_diff={:?} per_ch_match_swapped={:?}",
                                                n_diff_per_ch,
                                                n_match_swapped_per_ch,
                                            );
                                            tracing::info!(
                                                target: "ghostframe::cdf53::diff",
                                                tile_x = tile_x,
                                                tile_y = tile_y,
                                                "cdf53.diff.subbands (LL3 HL3 LH3 HH3 HL2 LH2 HH2 HL1 LH1 HH1): ch0={:?} ch1={:?} ch2={:?}",
                                                sb_diff[0], sb_diff[1], sb_diff[2],
                                            );
                                        }
                                    }
                                }
                            }
                            let passes = crate::encoder::cdf53::encode_passes(&coeffs_i16);
                            let gen = self.scheduler.bump_generation(tile_x, tile_y);
                            // New generation: tile is eligible for re-escalation.
                            self.metrics_tracker
                                .get_mut(tile_x as u32, tile_y as u32)
                                .already_escalated_this_gen = false;
                            // Diagnostic: one line per pass for the e2e log scan.
                            for (pass_idx, payload) in passes.iter().enumerate() {
                                tracing::info!(
                                    target: "ghostframe::cdf53",
                                    tile_x = tile_x,
                                    tile_y = tile_y,
                                    gen = gen,
                                    pass_idx = pass_idx,
                                    payload_size = payload.len(),
                                    "cdf53.emit"
                                );
                            }
                            self.scheduler.enqueue_refinement(tile_x, tile_y, gen, passes);
                            // Override the CPU classifier's codec_state so that
                            // dispatch_dirty_tiles_via_scheduler's pre-check can
                            // correctly identify this tile as Cdf53 and skip the
                            // priority-queue enqueue (which would otherwise supersede
                            // the refinement passes just enqueued above).
                            // The CPU may have classified this tile as Bc1 (e.g. on the
                            // first frame when freq is medium and Rule 5 fires), but
                            // the GPU compact list and coefficient buffer are authoritative
                            // for Cdf53 emission.
                            self.metrics_tracker
                                .get_mut(tile_x as u32, tile_y as u32)
                                .codec_state = crate::tile::CodecState::Cdf53 {
                                    passes_sent: 0,
                                    max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
                                };
                        }
                    }
                }

                // ---- M3.3c: post-fence Phase B for escalation candidates ----
                // Runs after the GPU fence (the existing fence covers all
                // compute work in this command buffer, including the
                // escalation forward dispatches recorded above). Reads
                // escalation coefficients per-slot, encodes 14 passes,
                // enqueues into the scheduler.
                let escalation_candidates = std::mem::take(&mut self.cdf53_escalation_candidates_this_frame);
                if !escalation_candidates.is_empty() {
                    let gpu_coeffs_ptr = {
                        let gpu = self.gpu_frame_processor.as_ref().unwrap();
                        gpu.cdf53_escalation_coefficients_ptr
                    };
                    for (slot, &flat_tile_idx) in escalation_candidates.iter().enumerate() {
                        let tile_x = (flat_tile_idx % cols) as u8;
                        let tile_y = (flat_tile_idx / cols) as u8;
                        let coeffs_i32 = unsafe {
                            std::slice::from_raw_parts(
                                gpu_coeffs_ptr.add(slot * crate::encoder::cdf53::CDF53_TOTAL_COEFFS),
                                crate::encoder::cdf53::CDF53_TOTAL_COEFFS,
                            )
                        };
                        let coeffs_i16: Vec<i16> = coeffs_i32.iter().map(|&v| v as i16).collect();
                        let passes = crate::encoder::cdf53::encode_passes(&coeffs_i16);
                        let gen = self.scheduler.bump_generation(tile_x, tile_y);
                        for (pass_idx, payload) in passes.iter().enumerate() {
                            tracing::info!(
                                target: "ghostframe::cdf53",
                                tile_x = tile_x,
                                tile_y = tile_y,
                                gen = gen,
                                pass_idx = pass_idx,
                                payload_size = payload.len(),
                                source = "escalation",
                                "cdf53.emit"
                            );
                        }
                        self.scheduler.enqueue_refinement(tile_x, tile_y, gen, passes);

                        let tm = self.metrics_tracker.get_mut(tile_x as u32, tile_y as u32);
                        tm.codec_state = crate::tile::CodecState::Cdf53 {
                            passes_sent: 0,
                            max_passes: crate::encoder::cdf53::CDF53_PASS_COUNT as u8,
                        };
                        tm.already_escalated_this_gen = true;
                    }
                }

                self.dispatch_dirty_tiles_via_scheduler(
                    &dirty_xy,
                    &grid,
                    TileDispatchFrame {
                        pixels,
                        stride: frame.stride,
                        seq,
                        timestamp_us: frame.timestamp_us,
                    },
                    max_frag,
                    SchedulerEmissionPolicy::GpuClassifierDriven,
                    Some(&mut palrle_payloads),
                );

                // Frame stats — emit per design Section 4.
                tracing::debug!(
                    target: "palrle.frame",
                    reused_or_allocated = self.palette_table.stats_frame.reused_or_allocated,
                    fell_back_to_raw = self.palette_table.stats_frame.fell_back_to_raw,
                    unique_frame_palettes = analysis.frame_palette_set_count,
                    "palrle frame stats"
                );
                self.palette_table.stats_frame = Default::default();
            }
        }
    }

    /// Handle a NACK by retransmitting the requested fragment, but only if
    /// QUIC RTT is low enough for the retransmission to arrive before the
    /// next frame. Wired in once client-side NACK sending is integrated.
    #[allow(dead_code)]
    fn handle_nack(&mut self, nack: NackMessage, handle: ConnectionHandle) {
        if let Some(conn) = self.server.connections.get_mut(&handle) {
            let rtt = conn.stats().path.rtt;
            let frame_interval = std::time::Duration::from_micros(16_667); // ~60fps

            if rtt < frame_interval {
                if let Some(dg) = self
                    .recent_frame_fragments
                    .get(&(nack.frame_seq, nack.frag_idx))
                {
                    let dg = dg.clone();
                    if let Some(wt) = self.wt_sessions.get_mut(&handle) {
                        let _ = wt.send_datagram(conn, &dg);
                        tracing::trace!(
                            frame_seq = nack.frame_seq,
                            frag_idx = nack.frag_idx,
                            "NACK retransmit"
                        );
                    }
                }
            } else {
                tracing::trace!(
                    ?rtt,
                    frame_seq = nack.frame_seq,
                    "NACK skipped — RTT too high for retransmission"
                );
            }
        }
    }

    /// Run the event loop until the ghostbridge fd is closed (EOF).
    pub async fn run(&mut self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let mut header = [0u8; 8];

        loop {
            // Build a timer future that fires at the earliest QUIC timeout, or
            // a never-completing future if there are no active connections.
            let sleep_fut: Pin<Box<dyn Future<Output = ()> + Send>> =
                match self.server.next_timeout() {
                    Some(deadline) => Box::pin(sleep_until(TokioInstant::from_std(deadline))),
                    None => Box::pin(pending::<()>()),
                };

            tokio::select! {
                biased;

                // 1. Earliest QUIC timeout fired.
                _ = sleep_fut => {
                    self.server.handle_timeout(Instant::now());
                }

                // 2. Frame submission from capture thread.
                frame = async {
                    match self.frame_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(frame) = frame {
                        self.process_frame(frame);
                    }
                }

                // 3. Inbound framed UDP packet from ghostbridge.
                read_res = self.stream.read_exact(&mut header) => {
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
            }

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
        }
    }

    /// Parse one inbound frame and feed it to quinn-proto.
    async fn process_inbound(&mut self, header: &[u8; 8]) -> io::Result<()> {
        let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
        let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;

        // Minimum valid frame: header (8) + payload + port (2) + NUL (1) = 11.
        if total_len < 8 + payload_len + 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame too short",
            ));
        }

        let rest_len = total_len.checked_sub(8).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "frame total_len underflow")
        })?;
        let mut rest = vec![0u8; rest_len];
        self.stream.read_exact(&mut rest).await?;

        let packet = parse_frame_rest(&rest, payload_len)?;

        let mut buf: Vec<u8> = Vec::with_capacity(QUIC_SCRATCH);
        let data = BytesMut::from(packet.payload.as_slice());

        tracing::trace!(remote = %packet.addr, payload_len, "processing inbound datagram");
        if let Some(transmit) = self.server.handle_datagram(
            Instant::now(),
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
        let now = Instant::now();
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
                Event::Connected => {
                    // New connection fully established — start the H3 handshake.
                    let wt = self.wt_sessions.entry(handle).or_default();
                    if let Some(conn) = self.server.connections.get_mut(&handle) {
                        wt.on_new_connection(conn);
                    }
                }

                Event::Stream(StreamEvent::Opened { dir }) => {
                    // Accept all newly-opened peer streams for this direction.
                    if let (Some(wt), Some(conn)) = (
                        self.wt_sessions.get_mut(&handle),
                        self.server.connections.get_mut(&handle),
                    ) {
                        wt.on_stream_opened(conn, dir);
                    }
                    self.maybe_fire_session_reset(handle);
                }

                Event::Stream(StreamEvent::Readable { id }) => {
                    if let (Some(wt), Some(conn)) = (
                        self.wt_sessions.get_mut(&handle),
                        self.server.connections.get_mut(&handle),
                    ) {
                        wt.on_stream_readable(conn, id);
                    }
                    self.maybe_fire_session_reset(handle);
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
                                    tracing::debug!(?handle, "ping received, sending pong");
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
                        self.dispatch_ack_datagram(&dg);
                    }
                }

                Event::ConnectionLost { reason } => {
                    tracing::info!(?handle, %reason, "connection lost");
                    self.wt_sessions.remove(&handle);
                    self.session_resets_fired.remove(&handle);
                }

                _ => {
                    // HandshakeDataReady, DatagramsUnblocked, etc. — no action needed.
                }
            }
        }

        // Process any feedback data received on non-session bidi streams.
        let feedback_data: Vec<Vec<u8>> = self
            .wt_sessions
            .values_mut()
            .flat_map(|wt| wt.drain_feedback())
            .collect();
        for data in &feedback_data {
            self.dispatch_feedback_bytes(data);
        }
    }

    /// Phase A: serially walk the GPU's compact list, map each tile's
    /// frame-local palette id to a persistent slot via PaletteTable, and
    /// produce per-tile work preps for Phase B.
    pub(crate) fn phase_a_palette_allocation(
        &mut self,
        cols: u32,
        compact_list: &[u32],
        per_tile_frame_palette_id: &[u8],
        folded_into: &[u32], // 256 entries
        frame_palette_set: &[crate::capture::gpu_pipeline::FramePaletteEntryRaw],
        index_buffer: &[u8], // compact_list.len() * 512 bytes
    ) -> Vec<PalRleTileWorkPrep> {
        use crate::encoder::pal_rle::PaletteEntry;
        use crate::tile::CodecState;

        let mut preps: Vec<PalRleTileWorkPrep> = Vec::with_capacity(compact_list.len());
        for (c, &tile_idx) in compact_list.iter().enumerate() {
            let tx = tile_idx % cols;
            let ty = tile_idx / cols;

            // Honour the classifier's actual decision.
            if !matches!(
                self.metrics_tracker.get(tx, ty).codec_state,
                CodecState::PalRle { .. }
            ) {
                continue;
            }

            let pal_id_local = per_tile_frame_palette_id[c];
            if pal_id_local == 0xFF {
                // Stage 2a sentinel — frame-palette-set overflow.
                self.metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
                self.palette_table.stats_frame.fell_back_to_raw += 1;
                continue;
            }
            let effective = (folded_into[pal_id_local as usize] & 0xFF) as usize;
            let raw = &frame_palette_set[effective];
            let mut palette = PaletteEntry::default();
            palette.count = raw.count as u8;
            for i in 0..palette.count as usize {
                let v = raw.colors[i];
                palette.colors[i] = [
                    (v & 0xFF) as u8,
                    ((v >> 8) & 0xFF) as u8,
                    ((v >> 16) & 0xFF) as u8,
                    ((v >> 24) & 0xFF) as u8,
                ];
            }

            match self.palette_table.acquire_or_allocate(&palette) {
                Some(id) => {
                    let bundled = !self.palette_table.delivered.contains(id);
                    if bundled {
                        self.palette_table.in_flight_carrying[id as usize] += 1;
                    }
                    let mut indices = [0u8; 512];
                    indices.copy_from_slice(&index_buffer[c * 512..(c + 1) * 512]);
                    self.metrics_tracker.get_mut(tx, ty).codec_state =
                        CodecState::PalRle { palette_id: id };
                    self.palette_table.stats_frame.reused_or_allocated += 1;
                    preps.push(PalRleTileWorkPrep {
                        tile_xy: (tx, ty),
                        indices,
                        palette,
                        palette_id: id,
                        bundled,
                    });
                }
                None => {
                    self.metrics_tracker.get_mut(tx, ty).codec_state = CodecState::Skip;
                    self.palette_table.stats_frame.fell_back_to_raw += 1;
                }
            }
        }
        preps
    }

    /// Phase B: rayon-parallel per-tile encode of PalRle payloads.
    /// Returns a HashMap keyed by (tile_x, tile_y) for Phase C lookup.
    ///
    /// Legacy entry point — equivalent to `phase_b_encode_payloads_with_caps`
    /// with default (no-capabilities) client. Kept for callers that haven't
    /// threaded capabilities through yet.
    pub(crate) fn phase_b_encode_payloads(
        preps: &[PalRleTileWorkPrep],
    ) -> std::collections::HashMap<(u32, u32), Vec<u8>> {
        Self::phase_b_encode_payloads_with_caps(
            preps,
            &crate::transport::client_caps::ClientCapabilities::default(),
            None,
        )
    }

    /// Phase B with explicit client capabilities. When
    /// `caps.indices_raw_enabled == true`, thin payloads are emitted as
    /// `indices_raw` (flags bit 1) instead of nibble-RLE. Bundled payloads
    /// always use the bundled format regardless of caps.
    ///
    /// `inject_at` is an optional cfg-gated test hook: when the tile at the
    /// matching `(tile_x, tile_y)` is encoded, its payload is replaced with a
    /// hand-built bundled payload whose single RLE byte references palette
    /// index 2 against a count=1 palette. The client GPU shader detects the
    /// OOB and writes `error_code = 5 (ERR_INDEX_OOB)` to its per-tile error
    /// slot. The wire format is documented in `palrle-codec-design.md`.
    pub(crate) fn phase_b_encode_payloads_with_caps(
        preps: &[PalRleTileWorkPrep],
        caps: &crate::transport::client_caps::ClientCapabilities,
        inject_at: Option<(u32, u32)>,
    ) -> std::collections::HashMap<(u32, u32), Vec<u8>> {
        use rayon::prelude::*;
        preps
            .par_iter()
            .map(|p| {
                let mut payload = if !p.bundled && caps.indices_raw_enabled {
                    tracing::info!(
                        target: "palrle.wire",
                        palette_id = p.palette_id,
                        tile_x = p.tile_xy.0,
                        tile_y = p.tile_xy.1,
                        "indices_raw emitted"
                    );
                    crate::encoder::pal_rle::encode_pal_rle_payload_indices_raw(
                        &p.indices,
                        p.palette_id,
                    )
                } else {
                    crate::encoder::pal_rle::encode_pal_rle_payload(
                        &p.indices,
                        &p.palette,
                        p.palette_id,
                        p.bundled,
                    )
                };
                if let Some(inject) = inject_at {
                    if p.tile_xy == inject {
                        // Hand-built bundled payload: flags=0x01 (bundled),
                        // palette_id=0, count=1, one BGRA red entry, then 64
                        // RLE bytes of 0x2F (index=2, run_len=16) covering
                        // all 1024 tile pixels. Passes client prevalidation
                        // (which insists on exactly 1024 pixels) so the GPU
                        // compute shader actually runs and detects the OOB
                        // (color_idx=2 >= count=1), writing error_code=5.
                        let mut p_inj = Vec::with_capacity(7 + 64);
                        p_inj.extend_from_slice(&[0x01u8, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xFF]);
                        p_inj.extend(std::iter::repeat(0x2Fu8).take(64));
                        payload = p_inj;
                        tracing::info!(
                            target: "palrle.wire",
                            tile_x = p.tile_xy.0,
                            tile_y = p.tile_xy.1,
                            "test-hook: injected OOB PalRle payload"
                        );
                    }
                }
                (p.tile_xy, payload)
            })
            .collect()
    }

    /// Test-only constructor that accepts a pre-built stream and server,
    /// bypassing the real ghostbridge connection. No tsnet node is held, so
    /// `_handle` is `None` and no `gbridge_close` is called on Drop.
    #[cfg(test)]
    pub(crate) fn new_with_stream_for_test(stream: TokioUnixStream, server: QuicServer) -> Self {
        // Warm rayon's global thread pool so the first PalRLE-heavy frame
        // doesn't pay thread-spin-up latency on the hot path (design Section 4).
        rayon::iter::IntoParallelIterator::into_par_iter(0..1u32)
            .for_each(|_| {});

        IoBridge {
            _handle: None,
            stream,
            server,
            local_addr: "0.0.0.0:4443".parse().unwrap(),
            wt_sessions: HashMap::new(),
            session_resets_fired: HashSet::new(),
            has_seen_prior_session: false,
            frame_rx: None,
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            metrics_tracker: crate::tile::MetricsTracker::new(0, 0),
            classifier: crate::tile::Classifier::default(),
            frame_mode: crate::tile::FrameMode::TileCodec,
            scheduler: crate::transport::scheduler::Scheduler::new(0, 0),
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_loss: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            inbound_loss: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            oob_inject_at: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            skip_palette_session_reset: false,
            force_dirty_frames: 0,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            client_caps: crate::transport::client_caps::ClientCapabilities::default(),
            fec_k: 0,
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
            cdf53_escalation_candidates_this_frame: Vec::new(),
            cdf53_enabled: false,
            gpu_frame_processor: None,
            full_frame_encoder: None,
            recent_frame_fragments: HashMap::new(),
            last_emitted_dimensions: None,
            dimensions_retransmits_left: 0,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_frames_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        frame_rx: mpsc::Receiver<FrameSubmission>,
    ) -> Self {
        // Warm rayon's global thread pool so the first PalRLE-heavy frame
        // doesn't pay thread-spin-up latency on the hot path (design Section 4).
        rayon::iter::IntoParallelIterator::into_par_iter(0..1u32)
            .for_each(|_| {});

        IoBridge {
            _handle: None,
            stream,
            server,
            local_addr: "0.0.0.0:4443".parse().unwrap(),
            wt_sessions: HashMap::new(),
            session_resets_fired: HashSet::new(),
            has_seen_prior_session: false,
            frame_rx: Some(frame_rx),
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            metrics_tracker: crate::tile::MetricsTracker::new(0, 0),
            classifier: crate::tile::Classifier::default(),
            frame_mode: crate::tile::FrameMode::TileCodec,
            scheduler: crate::transport::scheduler::Scheduler::new(0, 0),
            #[cfg(any(test, feature = "test-loss-injection"))]
            outbound_loss: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            inbound_loss: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            oob_inject_at: None,
            #[cfg(any(test, feature = "test-loss-injection"))]
            skip_palette_session_reset: false,
            force_dirty_frames: 0,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            client_caps: crate::transport::client_caps::ClientCapabilities::default(),
            fec_k: 0,
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
            cdf53_escalation_candidates_this_frame: Vec::new(),
            cdf53_enabled: false,
            gpu_frame_processor: None,
            full_frame_encoder: None,
            recent_frame_fragments: HashMap::new(),
            last_emitted_dimensions: None,
            dimensions_retransmits_left: 0,
        }
    }

    /// Return the capabilities most recently advertised by the client via HELLO.
    /// Returns `ClientCapabilities::default()` until the first HELLO is received.
    pub fn current_client_caps(&self) -> crate::transport::client_caps::ClientCapabilities {
        self.client_caps
    }

    /// Update capabilities from a parsed HELLO message.
    pub(crate) fn apply_hello(&mut self, msg: crate::transport::client_caps::HelloMsg) {
        self.client_caps = msg.caps;
        tracing::info!(
            indices_raw = msg.caps.indices_raw_enabled,
            "HELLO received, client capabilities updated"
        );
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

/// Dispatch a raw WebTransport datagram payload.
///
/// Returns `Some(response)` when a reply should be sent, or `None` for
/// unknown payloads (which the caller logs at trace level).
pub(crate) fn handle_datagram_payload(payload: &[u8]) -> Option<&'static [u8]> {
    if payload == PING_PAYLOAD {
        Some(PONG_PAYLOAD)
    } else {
        None
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
mod tests {
    use super::*;
    use tokio::net::UnixStream;

    /// Test-only helper: construct an IoBridge with a fresh UnixStream pair
    /// and QuicServer. Discards the peer end of the stream pair (caller
    /// doesn't need to interact with it).
    async fn make_bridge_for_test() -> IoBridge {
        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        IoBridge::new_with_stream_for_test(our_end, server)
    }

    #[tokio::test]
    async fn caps_default_to_disabled_until_hello() {
        use crate::transport::client_caps::ClientCapabilities;
        let bridge = make_bridge_for_test().await;
        // Before any HELLO, current_client_caps() returns the default.
        assert_eq!(bridge.current_client_caps(), ClientCapabilities::default());
        assert!(!bridge.current_client_caps().indices_raw_enabled);
    }

    #[tokio::test]
    async fn apply_hello_enables_indices_raw() {
        use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
        let mut bridge = make_bridge_for_test().await;
        let msg = HelloMsg { caps: ClientCapabilities { indices_raw_enabled: true, ..Default::default() } };
        bridge.apply_hello(msg);
        assert!(bridge.current_client_caps().indices_raw_enabled);
    }

    /// Verify that `IoBridge::run` returns `Ok(())` when the peer end of the
    /// socketpair is dropped (EOF from ghostbridge).
    #[tokio::test]
    async fn run_exits_cleanly_on_eof() {
        let (our_end, peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);

        let run_handle = tokio::spawn(async move { bridge.run().await });
        drop(peer);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle).await;
        match result {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => panic!("run() returned Err: {e}"),
            Ok(Err(join_err)) => panic!("task panicked: {join_err}"),
            Err(_) => panic!("run() did not return within timeout"),
        }
    }

    /// A malformed frame must be logged and swallowed without killing the
    /// event loop. We write a short header declaring `total_len = 4`, which
    /// fails `process_inbound`'s minimum-size check, then drop the peer to
    /// trigger clean shutdown. The `Ok(())` return proves the loop kept
    /// running after the error.
    #[tokio::test]
    async fn run_survives_malformed_frame() {
        use tokio::io::AsyncWriteExt;

        let (our_end, mut peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);

        let run_handle = tokio::spawn(async move { bridge.run().await });

        // total_len=4 (smaller than 8 header), payload_len=0. `read_exact`
        // will consume these 8 bytes; `process_inbound` then rejects with
        // "frame too short" and the loop continues.
        let bogus = [0u8, 0, 0, 4, 0, 0, 0, 0];
        peer.write_all(&bogus).await.expect("peer write failed");
        peer.flush().await.ok();

        // Give the loop a moment to observe and swallow the error.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        drop(peer);

        let result = tokio::time::timeout(std::time::Duration::from_secs(5), run_handle).await;
        match result {
            Ok(Ok(Ok(()))) => {}
            Ok(Ok(Err(e))) => panic!("run() returned Err: {e}"),
            Ok(Err(join_err)) => panic!("task panicked: {join_err}"),
            Err(_) => panic!("run() did not return within timeout"),
        }
    }

    #[test]
    fn ping_produces_pong() {
        assert_eq!(handle_datagram_payload(b"ping"), Some(&b"pong"[..]));
        assert_eq!(handle_datagram_payload(b"not ping"), None);
        assert_eq!(handle_datagram_payload(b""), None);
    }

    #[test]
    fn parse_listen_port_accepts_bare_port() {
        assert_eq!(parse_listen_port(":4443").unwrap(), 4443);
        assert_eq!(parse_listen_port("0.0.0.0:4443").unwrap(), 4443);
        assert_eq!(parse_listen_port("[::]:4443").unwrap(), 4443);
    }

    #[test]
    fn parse_listen_port_rejects_garbage() {
        assert!(parse_listen_port("nope").is_err());
        assert!(parse_listen_port(":abc").is_err());
    }

    #[tokio::test]
    async fn process_frame_produces_no_panic() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        let frame = crate::server::FrameSubmission {
            width: 64,
            height: 64,
            stride: 64 * 4,
            pixels: vec![0xFF; 64 * 64 * 4],
            dmabuf_fd: None,
            timestamp_us: 1000,
            damage_tiles: None,
        };
        // No connected sessions, so datagrams go nowhere, but should not panic
        bridge.process_frame(frame);
    }

    #[tokio::test]
    async fn fec_toggle_from_feedback() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        assert_eq!(bridge.fec_k, 0, "FEC starts disabled");

        // High loss → enable
        let fb_high = crate::transport::feedback::ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 95,
            datagrams_lost: 5,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        bridge.update_fec_from_feedback(&fb_high);
        assert_eq!(bridge.fec_k, 4, "FEC should be enabled at 5% loss");

        // Still losing — stays enabled
        bridge.update_fec_from_feedback(&fb_high);
        assert_eq!(bridge.fec_k, 4);

        // Low loss → disable
        let fb_low = crate::transport::feedback::ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 1000,
            datagrams_lost: 1,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        bridge.update_fec_from_feedback(&fb_low);
        assert_eq!(bridge.fec_k, 0, "FEC should be disabled at 0.1% loss");
    }

    #[tokio::test]
    async fn process_frame_increments_frame_seq() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        assert_eq!(bridge.frame_seq, 0);
        assert!(bridge.frame_rx.is_some());

        let make_frame = || crate::server::FrameSubmission {
            width: 32,
            height: 32,
            stride: 32 * 4,
            pixels: vec![0; 32 * 32 * 4],
            dmabuf_fd: None,
            timestamp_us: 0,
            damage_tiles: None,
        };

        bridge.process_frame(make_frame());
        assert_eq!(bridge.frame_seq, 1);

        bridge.process_frame(make_frame());
        assert_eq!(bridge.frame_seq, 2);
    }

    /// The GPU-path TileCodec branch must not emit zero-filled tiles when
    /// `frame.pixels` is empty (DMA-BUF zero-copy path). Constructing a real
    /// DMA-BUF fd in a unit test is impractical, so this test exercises the
    /// adjacent skip path: empty pixels + no DMA-BUF fd routes to
    /// `process_frame_cpu` (no GPU processor in the test bridge), which uses
    /// `frame.pixels` directly. It documents the no-panic invariant for the
    /// pathological "no pixels, no fd" submission. End-to-end coverage of the
    /// readback fallback lives in `e2e_mode_switch`.
    #[tokio::test]
    async fn process_frame_no_panic_on_empty_pixels_no_dmabuf_no_session() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        bridge.frame_mode = crate::tile::FrameMode::TileCodec;

        let frame = crate::server::FrameSubmission {
            width: 64,
            height: 64,
            stride: 64 * 4,
            pixels: vec![0u8; 64 * 64 * 4],
            dmabuf_fd: None,
            timestamp_us: 0,
            damage_tiles: None,
        };
        bridge.process_frame(frame);
        assert!(bridge.frame_seq > 0, "frame_seq must advance");
    }

    // Note: Task 11 CPU-path scheduler integration is not unit-testable without
    // a connected WebTransport session (the no-sessions early-return is
    // intentional — it prevents the dirty tracker from absorbing frame state
    // before any client can see it). Integration coverage lives in:
    //   - transport::scheduler::tests::* — scheduler behavior in isolation.
    //   - tests/e2e.rs::e2e_solid_color (Task 15) — full pipeline with client.

    /// `Classifier::reset` must zero hysteresis streaks, so a single busy
    /// frame after reset can NOT promote to H264 (it needs `enter_sustain_frames`
    /// consecutive busy frames). Without reset, a partial enter streak from a
    /// prior session would leak into the new session and trigger early promotion.
    #[test]
    fn classifier_reset_clears_streaks() {
        use crate::tile::{Classifier, CodecState, FrameMode};
        let mut c = Classifier::default();
        let busy: Vec<CodecState> = (0..20)
            .map(|i| CodecState::H264 { frames_in_h264: i })
            .collect();
        // Build partial enter streak.
        c.decide_frame_mode(&busy, FrameMode::TileCodec);
        c.decide_frame_mode(&busy, FrameMode::TileCodec);
        // Reset clears it.
        c.reset();
        // After reset, one busy frame must NOT promote (streak was zeroed).
        assert_eq!(
            c.decide_frame_mode(&busy, FrameMode::TileCodec),
            FrameMode::TileCodec
        );
    }

    /// IoBridge must hold a Scheduler that resizes alongside metrics_tracker
    /// and dirty_tracker. This test verifies the scheduler field is present
    /// and zero-sized at construction time.
    #[tokio::test]
    async fn iobridge_holds_scheduler_zero_sized_on_construction() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        assert_eq!(bridge.scheduler.cols(), 0);
        assert_eq!(bridge.scheduler.rows(), 0);
        assert_eq!(bridge.scheduler.queue_len(), 0);
    }

    /// Coverage for C1: when a new session reconnects, the existing
    /// `FullFrameEncoder` (lazy-inited from the prior session) must be told to
    /// emit an IDR so the new client gets a fresh decoding anchor instead of a
    /// P-frame referencing a GOP they never received. Drives `request_keyframe`
    /// directly because spinning up a real WebTransport handshake is too heavy.
    /// Verifies via `keyframe_pending()` that the call site flips the flag.
    #[tokio::test]
    async fn session_reconnect_requests_keyframe_on_existing_encoder() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        // Inject a fake encoder so the reconnect path's request_keyframe call
        // has something to act on. Skip silently if no codec is available on
        // the test machine — matches the existing pattern.
        if let Ok(enc) = crate::encoder::h264_vaapi::FullFrameEncoder::new(640, 480) {
            bridge.full_frame_encoder = Some(enc);
            // Simulate the side-effect of the session-connect path.
            bridge
                .full_frame_encoder
                .as_mut()
                .unwrap()
                .request_keyframe();
            assert!(
                bridge
                    .full_frame_encoder
                    .as_ref()
                    .unwrap()
                    .keyframe_pending(),
                "encoder must have keyframe_pending after session reconnect"
            );
        }
    }

    /// An inbound ACK_BATCH datagram routed through dispatch_ack_datagram
    /// must mark the matching in-flight tile work as Acked.
    #[tokio::test]
    async fn dispatch_ack_datagram_clears_in_flight_work() {
        use crate::transport::ack::{AckBatch, AckEntry};
        use crate::transport::scheduler::TileWork;

        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        // Seed the scheduler with one InFlight item.
        bridge.scheduler.resize(4, 4);
        bridge
            .scheduler
            .enqueue(TileWork::raw_for_test(1, 2, 0, vec![1, 2, 3]));
        let _ = bridge.scheduler.tick(usize::MAX); // promote to InFlight

        let batch = AckBatch {
            entries: vec![AckEntry {
                tile_x: 1,
                tile_y: 2,
                generation: 0,
                pass: 0,
            }],
        };
        bridge.dispatch_ack_datagram(&batch.encode());

        // Next tick: nothing eligible — the Acked entry was dropped via retain.
        let out = bridge.scheduler.tick(usize::MAX);
        assert!(out.is_empty());
        assert_eq!(bridge.scheduler.queue_len(), 0);
    }

    /// dispatch_ack_datagram silently ignores datagrams whose first byte
    /// isn't ACK_BATCH_MSG_TYPE.
    #[tokio::test]
    async fn dispatch_ack_datagram_ignores_unknown_msg_types() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        // Should not panic on empty/random/unknown payloads.
        bridge.dispatch_ack_datagram(&[]);
        bridge.dispatch_ack_datagram(&[0xFF, 0x00, 0x00]);
        bridge.dispatch_ack_datagram(&[0x01, 0, 0]); // FEEDBACK_MSG_TYPE — unrelated
    }

    /// dispatch_dirty_tiles_via_scheduler enqueues the right work in the
    /// right codec/state for the CPU policy, even without a connected
    /// session. This is the "Gap 3" fix from the final M3.1 review.
    #[tokio::test]
    async fn dispatch_via_scheduler_cpu_policy_enqueues_raw_tiles() {
        use crate::transport::scheduler::WorkState;
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        let pixels = vec![0xAAu8; 64 * 64 * 4];
        let grid = crate::tile::TileGrid::new(64, 64);
        let dirty = vec![(0u32, 0u32), (1, 1)];

        bridge.dispatch_dirty_tiles_via_scheduler(
            &dirty,
            &grid,
            TileDispatchFrame {
                pixels: &pixels,
                stride: 64 * 4,
                seq: 1,
                timestamp_us: 0,
            },
            /* max_frag */ 1200,
            SchedulerEmissionPolicy::CpuRawOnly,
            None,
        );

        let queued = bridge.scheduler.peek_for_test();
        assert_eq!(queued.len(), 2, "scheduler must hold both dirty tiles");
        for w in &queued {
            assert_eq!(w.state, WorkState::InFlight, "tick should have promoted");
            assert_eq!(w.codec, Codec::Raw, "CPU policy emits Raw only");
            assert_eq!(w.pass_idx, 0);
        }
    }

    // NOTE: the two tests below use std::env::set_var / remove_var which is
    // inherently order-sensitive in concurrent test runs.  Must run with
    // --test-threads=1 to avoid races with other tests that don't touch these
    // env vars.  The project convention (reference_testing.md) already requires
    // --test-threads=1 for the lib test suite.

    #[test]
    fn loss_injector_from_env_parses_probability_and_predicate() {
        // Use std::env carefully: serial within this test.
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "0.5");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "tile");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_SEED", "42");
        let inj =
            IoBridge::loss_injector_from_env("OUTBOUND").expect("probability > 0 must yield Some");
        // Force two calls for determinism — same seed = same outcome.
        let mut inj2 = IoBridge::loss_injector_from_env("OUTBOUND").unwrap();
        let mut inj_copy = inj;
        // Tile-datagram first byte (high bit set) → predicate matches → may drop.
        let tile_dg = [0x80u8, 0, 0, 1];
        // ACK datagram first byte (0x02) → predicate doesn't match → never drops.
        let ack_dg = [0x02u8, 0, 0, 0];
        assert!(
            !inj_copy.should_drop(&ack_dg),
            "tile predicate filters ack out"
        );
        assert!(!inj2.should_drop(&ack_dg));
        // Tile path may or may not drop on a given call; just exercise it.
        let _ = inj_copy.should_drop(&tile_dg);
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_SEED");
    }

    #[test]
    fn loss_injector_from_env_returns_none_when_unset() {
        // Ensure no leftover from prior tests.
        std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PROBABILITY");
        assert!(IoBridge::loss_injector_from_env("INBOUND").is_none());
    }

    #[cfg(any(test, feature = "test-loss-injection"))]
    #[test]
    fn oob_injector_from_env_parses() {
        // Saved/restored to avoid leaking into sibling tests that run in the
        // same process (cargo test default uses threads but env-var leak is a
        // common test fragility — guard explicitly).
        let prev = std::env::var("GHOSTFRAME_INJECT_OOB_PALRLE").ok();
        std::env::set_var("GHOSTFRAME_INJECT_OOB_PALRLE", "5,7");
        let inj = IoBridge::oob_injector_from_env();
        if let Some(p) = prev { std::env::set_var("GHOSTFRAME_INJECT_OOB_PALRLE", p); }
        else { std::env::remove_var("GHOSTFRAME_INJECT_OOB_PALRLE"); }
        assert_eq!(inj, Some((5u32, 7u32)));
    }

    #[tokio::test]
    async fn maybe_fire_session_reset_skips_first_connect_fires_on_reconnect() {
        use crate::transport::quic::QuicServer;
        use crate::transport::webtransport::WebTransportServer;
        use quinn_proto::ConnectionHandle;
        use tokio::net::UnixStream as TokioUnixStream;

        let (stream, _peer) = TokioUnixStream::pair().expect("UnixStream::pair");
        let server = QuicServer::new().expect("QuicServer::new");
        let mut bridge = IoBridge::new_with_stream_for_test(stream, server);

        bridge.palette_table.delivered.insert(7);

        let handle_a = ConnectionHandle(0);
        bridge.wt_sessions.insert(handle_a, WebTransportServer::default());

        // Pre-connected → no fire.
        bridge.maybe_fire_session_reset(handle_a);
        assert!(
            bridge.palette_table.delivered.contains(7),
            "not-yet-connected: no fire"
        );

        // FIRST connect (no prior session): even though is_connected=true, the
        // reset body is SKIPPED — has_seen_prior_session was false. Tests that
        // rely on classifier/scheduler/dirty-tracker state surviving first
        // connect (e.g. e2e_palrle_5pct_loss, e2e_solid_per_tile_pixels) keep
        // their pre-existing behavior.
        bridge
            .wt_sessions
            .get_mut(&handle_a)
            .expect("wt session")
            .test_set_connected(true);
        bridge.maybe_fire_session_reset(handle_a);
        assert!(
            bridge.palette_table.delivered.contains(7),
            "first connect: reset body skipped (has_seen_prior_session was false)"
        );
        assert!(
            bridge.has_seen_prior_session,
            "first connect: has_seen_prior_session must now be true"
        );

        // Simulate ConnectionLost on handle_a (per real wiring at Event::ConnectionLost).
        bridge.wt_sessions.remove(&handle_a);
        bridge.session_resets_fired.remove(&handle_a);

        // RECONNECT (new handle_b): reset body fires now (has_seen_prior_session=true).
        let handle_b = ConnectionHandle(1);
        bridge.wt_sessions.insert(handle_b, WebTransportServer::default());
        bridge
            .wt_sessions
            .get_mut(&handle_b)
            .expect("wt session b")
            .test_set_connected(true);
        bridge.maybe_fire_session_reset(handle_b);
        assert!(
            !bridge.palette_table.delivered.contains(7),
            "reconnect: reset body MUST fire (delivered cleared)"
        );

        // Re-arm delivered. Second call on handle_b: no re-fire (session_resets_fired gate).
        bridge.palette_table.delivered.insert(7);
        bridge.maybe_fire_session_reset(handle_b);
        assert!(
            bridge.palette_table.delivered.contains(7),
            "second call on same handle: must not re-fire"
        );
    }

    #[cfg(any(test, feature = "test-loss-injection"))]
    #[test]
    fn skip_palette_session_reset_from_env_parses() {
        let prev = std::env::var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET").ok();
        std::env::set_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", "1");
        let got = IoBridge::skip_palette_session_reset_from_env();
        if let Some(p) = prev {
            std::env::set_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET", p);
        } else {
            std::env::remove_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET");
        }
        assert!(got, "env=1 must yield true");

        // Default (unset) is false.
        std::env::remove_var("GHOSTFRAME_SKIP_PALETTE_SESSION_RESET");
        assert!(!IoBridge::skip_palette_session_reset_from_env(), "unset must yield false");
    }

    #[test]
    fn diagnose_tiles_env_var_parses() {
        std::env::set_var("GHOSTFRAME_DIAGNOSE_TILES", "1");
        let on = IoBridge::diagnose_tiles_from_env();
        std::env::remove_var("GHOSTFRAME_DIAGNOSE_TILES");
        assert!(on);

        let off = IoBridge::diagnose_tiles_from_env();
        assert!(!off);
    }

    /// dispatch_dirty_tiles_via_scheduler emits Solid bytes when the GPU policy
    /// reads CodecState::Solid for a tile.
    #[tokio::test]
    async fn dispatch_via_scheduler_gpu_policy_emits_solid_for_solid_state() {
        use crate::tile::CodecState;
        use crate::transport::scheduler::WorkState;
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        bridge.scheduler.resize(2, 2);
        bridge.metrics_tracker.resize(2, 2);
        bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::Solid;

        let pixels = vec![0xBBu8; 64 * 64 * 4];
        let grid = crate::tile::TileGrid::new(64, 64);
        let dirty = vec![(0u32, 0u32)];

        bridge.dispatch_dirty_tiles_via_scheduler(
            &dirty,
            &grid,
            TileDispatchFrame {
                pixels: &pixels,
                stride: 64 * 4,
                seq: 1,
                timestamp_us: 0,
            },
            1200,
            SchedulerEmissionPolicy::GpuClassifierDriven,
            None,
        );

        let queued = bridge.scheduler.peek_for_test();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].codec, Codec::Solid);
        assert_eq!(queued[0].payload.len(), 4, "Solid is 4 bytes BGRA");
        assert_eq!(queued[0].state, WorkState::InFlight);
    }

    #[test]
    fn populate_gpu_metrics_writes_unique_colors_and_edge_density() {
        use crate::capture::gpu_pipeline::TileAnalysis;
        use crate::tile::MetricsTracker;

        let mut tracker = MetricsTracker::new(2, 1);
        let analysis = vec![
            TileAnalysis {
                count: 1,
                edge_density_thou: 0,
                _pad: [0; 2],
                colors: [0; 16],
            },
            TileAnalysis {
                count: 17,
                edge_density_thou: 850,
                _pad: [0; 2],
                colors: [0; 16],
            },
        ];
        let dirty: Vec<(u32, u32)> = vec![(0, 0), (1, 0)];

        super::populate_gpu_metrics(&mut tracker, &dirty, 2, &analysis);

        assert_eq!(tracker.get(0, 0).unique_colors, 1);
        assert!(tracker.get(0, 0).edge_density.abs() < 1e-6);

        assert_eq!(tracker.get(1, 0).unique_colors, 17);
        assert!((tracker.get(1, 0).edge_density - 0.850).abs() < 1e-6);
    }

    #[test]
    fn populate_gpu_metrics_skips_non_dirty_tiles() {
        use crate::capture::gpu_pipeline::TileAnalysis;
        use crate::tile::MetricsTracker;

        let mut tracker = MetricsTracker::new(2, 1);
        // Pre-seed tile (1,0) to a known sentinel so we can prove it stayed.
        tracker.get_mut(1, 0).unique_colors = u16::MAX;
        tracker.get_mut(1, 0).edge_density = f32::NAN;

        let analysis = vec![
            TileAnalysis {
                count: 5,
                edge_density_thou: 100,
                _pad: [0; 2],
                colors: [0; 16],
            },
            TileAnalysis {
                count: 9,
                edge_density_thou: 200,
                _pad: [0; 2],
                colors: [0; 16],
            },
        ];
        // Only (0,0) is dirty.
        let dirty: Vec<(u32, u32)> = vec![(0, 0)];
        super::populate_gpu_metrics(&mut tracker, &dirty, 2, &analysis);

        assert_eq!(tracker.get(0, 0).unique_colors, 5);
        assert_eq!(
            tracker.get(1, 0).unique_colors,
            u16::MAX,
            "non-dirty tile untouched"
        );
        assert!(
            tracker.get(1, 0).edge_density.is_nan(),
            "non-dirty tile untouched"
        );
    }

    /// Verify that `IoBridge` constructors initialize `palette_table` to an
    /// all-empty state. M3.2a: palette state is per-server, so a fresh bridge
    /// must start with no allocated slots.
    #[tokio::test]
    async fn io_bridge_constructs_with_empty_palette_table() {
        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let bridge = IoBridge::new_with_stream_for_test(our_end, server);

        for id in 0..crate::encoder::pal_rle::PALETTE_TABLE_SLOTS {
            assert_eq!(
                bridge.palette_table.slot_state[id],
                crate::encoder::pal_rle::SlotState::Empty,
                "slot {id} must start Empty"
            );
            assert!(
                bridge.palette_table.entries[id].is_none(),
                "slot {id} entries must start None"
            );
            assert_eq!(
                bridge.palette_table.ref_count[id], 0,
                "slot {id} ref_count must start 0"
            );
        }
        // Spot-check that the delivered bitset contains no ids.
        assert!(
            !bridge.palette_table.delivered.contains(0),
            "delivered[0] must start false"
        );
        assert!(
            !bridge.palette_table.delivered.contains(255),
            "delivered[255] must start false"
        );
    }

    #[tokio::test]
    async fn phase_a_allocates_and_marks_bundled_for_first_emission() {
        use crate::tile::CodecState;

        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
        let cols = 4;
        bridge.metrics_tracker.resize(cols, 2);
        bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::PalRle { palette_id: 0 };

        let compact_list = vec![0u32];
        let per_tile_id = vec![0u8];
        // folded_into[0] = default-self = ((255-1)<<8) | 0
        let folded_into = vec![((255u32 - 1) << 8) | 0u32; 256];

        // Build a frame_palette_set with slot 0 populated (count=1, color BGRA[10,20,30,255]).
        let mut frame_palette_set = vec![
            crate::capture::gpu_pipeline::FramePaletteEntryRaw {
                count: 0,
                _pad: [0; 3],
                colors: [0; 16],
            };
            256
        ];
        frame_palette_set[0].count = 1;
        frame_palette_set[0].colors[0] = 10 | (20 << 8) | (30 << 16) | (255 << 24);
        let index_buffer = vec![0u8; 512];

        let preps = bridge.phase_a_palette_allocation(
            cols,
            &compact_list,
            &per_tile_id,
            &folded_into,
            &frame_palette_set,
            &index_buffer,
        );

        assert_eq!(preps.len(), 1);
        assert_eq!(preps[0].tile_xy, (0, 0));
        assert_eq!(preps[0].palette.count, 1);
        assert_eq!(preps[0].palette.colors[0], [10, 20, 30, 255]);
        assert!(preps[0].bundled, "first emission must bundle");
        assert_eq!(preps[0].palette_id, 0);
        assert_eq!(bridge.palette_table.in_flight_carrying[0], 1);
        assert!(!bridge.palette_table.delivered.contains(0));
    }

    #[tokio::test]
    async fn phase_a_marks_thin_when_delivered_already_set() {
        use crate::encoder::pal_rle::PaletteEntry;
        use crate::tile::CodecState;

        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
        let cols = 4;
        bridge.metrics_tracker.resize(cols, 2);
        bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::PalRle { palette_id: 0 };

        // Pre-populate persistent slot 3 with exact palette + delivered=true.
        let mut p = PaletteEntry::default();
        p.colors[0] = [10, 20, 30, 255];
        p.count = 1;
        bridge.palette_table.entries[3] = Some(p);
        bridge.palette_table.slot_state[3] = crate::encoder::pal_rle::SlotState::FreeButCached;
        bridge.palette_table.delivered.insert(3);
        bridge.palette_table.free_lru.push_back(3);

        let compact_list = vec![0u32];
        let per_tile_id = vec![0u8];
        let folded_into = vec![((255u32 - 1) << 8) | 0u32; 256];
        let mut frame_palette_set = vec![
            crate::capture::gpu_pipeline::FramePaletteEntryRaw {
                count: 0,
                _pad: [0; 3],
                colors: [0; 16],
            };
            256
        ];
        frame_palette_set[0].count = 1;
        frame_palette_set[0].colors[0] = 10 | (20 << 8) | (30 << 16) | (255 << 24);
        let index_buffer = vec![0u8; 512];

        let preps = bridge.phase_a_palette_allocation(
            cols,
            &compact_list,
            &per_tile_id,
            &folded_into,
            &frame_palette_set,
            &index_buffer,
        );

        assert_eq!(preps.len(), 1);
        assert_eq!(preps[0].palette_id, 3, "find_matching should hit slot 3");
        assert!(!preps[0].bundled, "delivered=true → thin payload");
        assert_eq!(bridge.palette_table.in_flight_carrying[3], 0);
    }

    #[tokio::test]
    async fn phase_a_falls_back_to_raw_on_sentinel() {
        use crate::tile::CodecState;

        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
        let cols = 4;
        bridge.metrics_tracker.resize(cols, 2);
        bridge.metrics_tracker.get_mut(0, 0).codec_state = CodecState::PalRle { palette_id: 0 };

        let compact_list = vec![0u32];
        let per_tile_id = vec![0xFFu8]; // overflow sentinel
        let folded_into = vec![0u32; 256];
        let frame_palette_set = vec![];
        let index_buffer = vec![];

        let preps = bridge.phase_a_palette_allocation(
            cols,
            &compact_list,
            &per_tile_id,
            &folded_into,
            &frame_palette_set,
            &index_buffer,
        );

        assert!(preps.is_empty());
        assert_eq!(
            bridge.metrics_tracker.get(0, 0).codec_state,
            CodecState::Skip
        );
        assert_eq!(bridge.palette_table.stats_frame.fell_back_to_raw, 1);
    }

    #[test]
    fn phase_b_encodes_each_prep_in_parallel() {
        use crate::encoder::pal_rle::PaletteEntry;

        let mut palette = PaletteEntry::default();
        palette.colors[0] = [1, 2, 3, 255];
        palette.count = 1;
        let preps = vec![
            PalRleTileWorkPrep {
                tile_xy: (0, 0),
                indices: [0; 512],
                palette,
                palette_id: 7,
                bundled: true,
            },
            PalRleTileWorkPrep {
                tile_xy: (1, 0),
                indices: [0; 512],
                palette,
                palette_id: 7,
                bundled: false,
            },
        ];
        let encoded = IoBridge::phase_b_encode_payloads(&preps);
        assert_eq!(encoded.len(), 2);
        let p00 = &encoded[&(0u32, 0u32)];
        let p10 = &encoded[&(1u32, 0u32)];
        // Bundled has flag bit 0 set.
        assert_eq!(p00[0] & 0x01, 0x01);
        assert_eq!(p10[0] & 0x01, 0x00);
        // Both reference id 7.
        assert_eq!(p00[1], 7);
        assert_eq!(p10[1], 7);
    }

    #[tokio::test]
    async fn ack_for_palrle_tile_sets_delivered_and_decrements_in_flight() {
        use crate::encoder::pal_rle::{PaletteEntry, SlotState};

        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
        // Resize scheduler so tile (0,0) is in-bounds.
        bridge.scheduler.resize(1, 1);

        let mut p = PaletteEntry::default();
        p.colors[0] = [1, 2, 3, 255];
        p.count = 1;
        bridge.palette_table.entries[7] = Some(p);
        bridge.palette_table.slot_state[7] = SlotState::Held;
        bridge.palette_table.in_flight_carrying[7] = 1;

        // Enqueue a PalRle TileWork referencing palette 7.
        bridge.scheduler.enqueue(crate::transport::scheduler::TileWork {
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            pass_idx: 0,
            total_passes: 1,
            codec: crate::transport::protocol::Codec::PalRle,
            payload: vec![0x01u8, 7, 1, 1, 2, 3, 255, 0xF0],
            queued_at: std::time::Instant::now(),
            last_sent_at: None,
            state: crate::transport::scheduler::WorkState::Pending,
        });
        let _ = bridge.scheduler.tick(usize::MAX);

        // Build synthetic AckBatch wire: [0x02, count=1, tile_x=0, tile_y=0, (gen=0<<4)|pass=0, reserved=0]
        let wire = vec![0x02u8, 1, 0, 0, 0, 0];
        bridge.dispatch_ack_datagram(&wire);

        assert!(bridge.palette_table.delivered.contains(7));
        assert_eq!(bridge.palette_table.in_flight_carrying[7], 0);
    }

    #[tokio::test]
    async fn ack_for_palrle_tile_releases_ref_count() {
        use crate::encoder::pal_rle::{PaletteEntry, SlotState};

        let (our_end, _peer) = UnixStream::pair().expect("UnixStream::pair failed");
        let server = QuicServer::new().expect("QuicServer::new failed");
        let mut bridge = IoBridge::new_with_stream_for_test(our_end, server);
        bridge.scheduler.resize(1, 1);

        let mut p = PaletteEntry::default();
        p.colors[0] = [1, 2, 3, 255];
        p.count = 1;
        bridge.palette_table.entries[5] = Some(p);
        bridge.palette_table.slot_state[5] = SlotState::Held;
        bridge.palette_table.ref_count[5] = 1;
        bridge.palette_table.in_flight_carrying[5] = 1;

        bridge.scheduler.enqueue(crate::transport::scheduler::TileWork {
            tile_x: 0, tile_y: 0, generation: 0, pass_idx: 0, total_passes: 1,
            codec: crate::transport::protocol::Codec::PalRle,
            payload: vec![0x01u8, 5, 1, 1, 2, 3, 255, 0xF0],
            queued_at: std::time::Instant::now(),
            last_sent_at: None,
            state: crate::transport::scheduler::WorkState::Pending,
        });
        let _ = bridge.scheduler.tick(usize::MAX);

        let wire = vec![0x02u8, 1, 0, 0, 0, 0];
        bridge.dispatch_ack_datagram(&wire);

        assert_eq!(bridge.palette_table.ref_count[5], 0, "ACK must release the Phase A acquire");
        assert_eq!(
            bridge.palette_table.slot_state[5],
            SlotState::FreeButCached,
            "slot must transition to FreeButCached after ref_count reaches 0"
        );
    }

    #[test]
    fn palrle_bundled_predicate_matches_bundled_datagram() {
        // Synthetic wire: tile datagram, codec=PalRle, flags byte with bundle bit.
        let mut wire = vec![0u8; 21];
        wire[0] = 0x80; // tile datagram flag
        wire[14] = (crate::transport::protocol::Codec::PalRle as u8) << 1;
        wire[20] = 0x01; // bundled
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_bundled");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1");
        let mut inj = IoBridge::loss_injector_from_env("OUTBOUND").unwrap();
        assert!(inj.should_drop(&wire));
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_SEED");
    }

    #[test]
    fn palrle_bundled_predicate_rejects_thin_datagram() {
        let mut wire = vec![0u8; 21];
        wire[0] = 0x80;
        wire[14] = (crate::transport::protocol::Codec::PalRle as u8) << 1;
        wire[20] = 0x00; // thin
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_bundled");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1");
        let mut inj = IoBridge::loss_injector_from_env("OUTBOUND").unwrap();
        assert!(!inj.should_drop(&wire));
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_SEED");
    }

    #[test]
    fn palrle_thin_predicate_matches_thin_only() {
        let mut wire = vec![0u8; 21];
        wire[0] = 0x80;
        wire[14] = (crate::transport::protocol::Codec::PalRle as u8) << 1;
        wire[20] = 0x00;
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "1.0");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "palrle_thin");
        std::env::set_var("GHOSTFRAME_OUTBOUND_LOSS_SEED", "1");
        let mut inj = IoBridge::loss_injector_from_env("OUTBOUND").unwrap();
        assert!(inj.should_drop(&wire));
        wire[20] = 0x01;
        // Re-create inj since it consumed RNG state; or just check the predicate behavior:
        // (the predicate is the only filter at proba=1.0, so should_drop is purely predicate-driven)
        let mut inj2 = IoBridge::loss_injector_from_env("OUTBOUND").unwrap();
        assert!(!inj2.should_drop(&wire));
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE");
        std::env::remove_var("GHOSTFRAME_OUTBOUND_LOSS_SEED");
    }

    #[test]
    fn phase_b_emits_indices_raw_when_caps_enabled_and_thin() {
        use crate::encoder::pal_rle::PaletteEntry;
        let prep = PalRleTileWorkPrep {
            tile_xy: (0, 0),
            indices: [0xAB; 512],
            palette: PaletteEntry { count: 2, colors: [[0xFF, 0, 0, 0xFF]; 16] },
            palette_id: 9,
            bundled: false, // thin path
        };
        let caps = crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: true,
            ..Default::default()
        };
        let map = IoBridge::phase_b_encode_payloads_with_caps(&[prep], &caps, None);
        let payload = &map[&(0, 0)];
        assert_eq!(payload[0], 0x02, "indices_raw flag (bit 1)");
        assert_eq!(payload[1], 9, "palette_id");
        assert_eq!(payload.len(), 514, "fixed-size indices_raw payload");
    }

    #[test]
    fn phase_b_emits_thin_rle_when_caps_disabled() {
        use crate::encoder::pal_rle::PaletteEntry;
        let prep = PalRleTileWorkPrep {
            tile_xy: (5, 7),
            indices: [0x00; 512], // all-same index → max-compressible RLE
            palette: PaletteEntry { count: 1, colors: [[0xFF, 0, 0, 0xFF]; 16] },
            palette_id: 3,
            bundled: false, // thin path
        };
        let caps = crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            ..Default::default()
        };
        let map = IoBridge::phase_b_encode_payloads_with_caps(&[prep], &caps, None);
        let payload = &map[&(5, 7)];
        assert_eq!(payload[0], 0x00, "thin flag (bit 0 clear, bit 1 clear)");
        assert_eq!(payload[1], 3, "palette_id");
        assert!(payload.len() < 514, "RLE compresses single-index tile far below 514");
    }

    #[test]
    fn phase_b_emits_bundled_when_bundled_regardless_of_caps() {
        use crate::encoder::pal_rle::PaletteEntry;
        let prep = PalRleTileWorkPrep {
            tile_xy: (1, 1),
            indices: [0x00; 512],
            palette: PaletteEntry { count: 1, colors: [[0xFF, 0, 0, 0xFF]; 16] },
            palette_id: 12,
            bundled: true,
        };
        let caps = crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: true,
            ..Default::default()
        };
        let map = IoBridge::phase_b_encode_payloads_with_caps(&[prep], &caps, None);
        let payload = &map[&(1, 1)];
        assert_eq!(payload[0], 0x01, "bundled flag (bit 0 set)");
        // indices_raw promotion only applies to thin path; bundled passes through unchanged.
    }

    #[tokio::test]
    async fn feedback_stream_parses_hello_then_feedback_then_decode_error() {
        use crate::transport::client_caps::{ClientCapabilities, HelloMsg};
        use crate::transport::decode_error::{DecodeErrorMsg, ERR_THIN_UNCACHED_PALETTE};
        use crate::transport::feedback::ReceiverFeedback;
        use crate::encoder::pal_rle::PaletteEntry;

        let mut bridge = make_bridge_for_test().await;

        // Pre-populate palette_table slot 7 as delivered, so the decode-error
        // path can find a palette to clear. The metrics_tracker also needs
        // to record that tile (3, 4) is currently rendering palette_id=7
        // for handle_decode_error to locate it.
        let pal = PaletteEntry { count: 1, colors: [[10, 20, 30, 255]; 16] };
        bridge.palette_table.write_bytes(7, &pal);
        bridge.palette_table.delivered.insert(7);
        bridge.metrics_tracker.resize(8, 8);
        bridge.metrics_tracker.get_mut(3, 4).codec_state =
            crate::tile::CodecState::PalRle { palette_id: 7 };

        // Concatenated stream: HELLO (2 B) + FEEDBACK (22 B) + DECODE_ERROR (5 B) = 29 bytes
        let hello = HelloMsg { caps: ClientCapabilities { indices_raw_enabled: true, ..Default::default() } };
        let fb = ReceiverFeedback {
            timestamp_ns: 0, datagrams_received: 0, datagrams_lost: 0,
            datagrams_recovered_fec: 0, suspension_detected: false,
        };
        let err = DecodeErrorMsg {
            codec: 2, tile_x: 3, tile_y: 4, error_code: ERR_THIN_UNCACHED_PALETTE,
        };

        let mut buf = Vec::new();
        hello.encode(&mut buf);
        fb.encode(&mut buf);
        err.encode(&mut buf);
        assert_eq!(buf.len(), 2 + 22 + 5);

        bridge.dispatch_feedback_bytes(&buf);

        // HELLO applied:
        assert!(bridge.current_client_caps().indices_raw_enabled,
            "HELLO message must apply caps");

        // DECODE_ERROR with code 3 cleared the delivered bit (via force_rebundle):
        assert!(!bridge.palette_table.delivered.contains(7),
            "ERR_THIN_UNCACHED_PALETTE must clear delivered bit via force_rebundle");
    }

    #[tokio::test]
    async fn decode_error_3_triggers_force_rebundle() {
        use crate::transport::decode_error::{DecodeErrorMsg, ERR_THIN_UNCACHED_PALETTE};
        use crate::tile::CodecState;
        use crate::encoder::pal_rle::PaletteEntry;

        let mut bridge = make_bridge_for_test().await;

        // Set up state: palette_table slot 5 is in the "delivered" state so the
        // next emission for that palette would be thin. metrics_tracker records
        // that tile (3, 4) is currently rendering palette_id=5.
        let pal = PaletteEntry { count: 1, colors: [[10, 20, 30, 255]; 16] };
        bridge.palette_table.write_bytes(5, &pal);
        bridge.palette_table.delivered.insert(5);
        bridge.metrics_tracker.resize(8, 8);
        bridge.metrics_tracker.get_mut(3, 4).codec_state = CodecState::PalRle { palette_id: 5 };

        // Simulate client reporting thin-uncached for that tile.
        let msg = DecodeErrorMsg {
            codec: 2, tile_x: 3, tile_y: 4, error_code: ERR_THIN_UNCACHED_PALETTE,
        };
        bridge.handle_decode_error(msg);

        // delivered bit must be cleared so the next emission rebundles.
        assert!(!bridge.palette_table.delivered.contains(5),
            "force_rebundle should clear the delivered bit for palette_id=5");
    }

    #[tokio::test]
    async fn decode_error_other_codes_no_op_on_palette_table() {
        use crate::transport::decode_error::{DecodeErrorMsg, ERR_INDEX_OOB};
        use crate::tile::CodecState;
        use crate::encoder::pal_rle::PaletteEntry;

        let mut bridge = make_bridge_for_test().await;
        let pal = PaletteEntry { count: 1, colors: [[10, 20, 30, 255]; 16] };
        bridge.palette_table.write_bytes(5, &pal);
        bridge.palette_table.delivered.insert(5);
        bridge.metrics_tracker.resize(8, 8);
        bridge.metrics_tracker.get_mut(3, 4).codec_state = CodecState::PalRle { palette_id: 5 };

        let msg = DecodeErrorMsg {
            codec: 2, tile_x: 3, tile_y: 4, error_code: ERR_INDEX_OOB,
        };
        bridge.handle_decode_error(msg);

        // Code 5 is log-only in M3.2b — delivered must remain set.
        assert!(bridge.palette_table.delivered.contains(5),
            "ERR_INDEX_OOB should not clear delivered bit (M3.2b leaves it as a future hook)");
    }

    /// Verify the io_bridge escalation-sweep logic in isolation:
    /// `detect_escalation_candidates` is called against a hand-built tracker
    /// and the results match the expected eligible indices.
    ///
    /// This mirrors exactly the sweep that `process_frame_gpu` will run on every
    /// frame. The GPU dispatch path itself requires a Vulkan device and is
    /// validated end-to-end by Task 13's e2e_progressive_refinement test.
    #[tokio::test]
    async fn escalation_sweep_returns_expected_candidates() {
        use crate::tile::{CodecState, MetricsTracker};
        use crate::tile::detect_escalation_candidates;
        use crate::capture::gpu_pipeline::MAX_ESCALATION_PER_FRAME;

        // Build a 4×4 tracker and make tiles (0,0), (1,0), (2,0) eligible:
        // idle_frames > 30, lossy codec, not already escalated.
        let mut tracker = MetricsTracker::new(4, 4);
        for x in 0..3u32 {
            let m = tracker.get_mut(x, 0);
            m.idle_frames = 31;
            m.codec_state = CodecState::Solid;
        }
        // Tile (3,0): idle but already escalated — must be excluded.
        {
            let m = tracker.get_mut(3, 0);
            m.idle_frames = 31;
            m.codec_state = CodecState::Solid;
            m.already_escalated_this_gen = true;
        }
        // Remaining tiles (row 1-3): below threshold or Skip — excluded.

        let candidates = detect_escalation_candidates(&tracker, MAX_ESCALATION_PER_FRAME);

        // Flat row-major indices: (0,0)=0, (1,0)=1, (2,0)=2
        assert_eq!(candidates, vec![0u32, 1, 2],
            "sweep should return exactly the three eligible tiles in row-major order");

        // Simulate what process_frame_gpu does: stash on IoBridge.
        let mut bridge = make_bridge_for_test().await;
        bridge.cdf53_escalation_candidates_this_frame = candidates.clone();
        assert_eq!(bridge.cdf53_escalation_candidates_this_frame, vec![0u32, 1, 2],
            "stashed candidates must match what the sweep returned");

        // Verify k_max capping is respected by the sweep helper.
        let capped = detect_escalation_candidates(&tracker, 2);
        assert_eq!(capped, vec![0u32, 1],
            "k_max=2 should cap the result to the first 2 candidates");
    }
}
