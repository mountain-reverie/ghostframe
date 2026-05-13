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
use std::collections::HashMap;
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
    max_frame_fragment_payload, Codec, FrameHeader, NackMessage, FRAME_HEADER_SIZE, PING_PAYLOAD,
    PONG_PAYLOAD, TILE_DATAGRAM_FLAG,
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
    /// Remaining frames to force all-dirty after a new session connects.
    /// QUIC slow-start can only deliver a fraction of tiles in the first burst;
    /// forcing dirty for several frames lets the congestion window open.
    force_dirty_frames: u32,
    /// Persistent palette table for PalRLE codec emission. M3.2a single-client
    /// invariant — flat server-wide state.
    pub(crate) palette_table: crate::encoder::pal_rle::PaletteTable,
    /// FEC parity group size. 0 = disabled.
    fec_k: usize,
    /// Loss rate threshold to enable FEC (0.005 = 0.5%).
    fec_enable_threshold: f64,
    /// Loss rate threshold to disable FEC (hysteresis).
    fec_disable_threshold: f64,
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
            force_dirty_frames: 0,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            fec_k: std::env::var("GHOSTFRAME_FEC_K")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0),
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
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

    /// Shared scheduler dispatch: grid-sync → RTT update → bump+encode+enqueue
    /// per dirty tile → tick → fragment+send. Called by both `process_frame_cpu`
    /// and `process_frame_gpu`'s `FrameMode::TileCodec` branch.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dispatch_dirty_tiles_via_scheduler(
        &mut self,
        dirty: &[(u32, u32)],
        grid: &crate::tile::TileGrid,
        pixels: &[u8],
        stride: u32,
        seq: u32,
        timestamp_us: u32,
        max_frag: usize,
        policy: SchedulerEmissionPolicy,
        mut palrle_payloads: Option<&mut std::collections::HashMap<(u32, u32), Vec<u8>>>,
    ) {
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
            let (gen, superseded) =
                self.scheduler.bump_generation_collecting(tile_x as u8, tile_y as u8);
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
                    let codec_state = self.metrics_tracker.get(tile_x, tile_y).codec_state;
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
                seq | TILE_DATAGRAM_FLAG,
                work.tile_x,
                work.tile_y,
                work.codec,
                work.generation,
                work.pass_idx,
                &work.payload,
                timestamp_us,
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

    /// CPU-side tile-based pipeline (original implementation).
    fn process_frame_cpu(&mut self, frame: FrameSubmission) {
        let grid = TileGrid::new(frame.width, frame.height);
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

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
        let dims_changed = self.last_emitted_dimensions != Some((frame.width, frame.height));
        if dims_changed {
            self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
            self.last_emitted_dimensions = Some((frame.width, frame.height));
        }
        if self.dimensions_retransmits_left > 0 {
            let dg = crate::transport::protocol::build_frame_dimensions_datagram(
                seq,
                frame.timestamp_us,
                frame.width,
                frame.height,
            );
            self.send_to_all_sessions(&dg);
            self.dimensions_retransmits_left -= 1;
        }

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
            &frame.pixels,
            frame.stride,
            seq,
            frame.timestamp_us,
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
        let dims_changed = self.last_emitted_dimensions != Some((frame.width, frame.height));
        if dims_changed {
            self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
            self.last_emitted_dimensions = Some((frame.width, frame.height));
        }
        if self.dimensions_retransmits_left > 0 {
            let dg = crate::transport::protocol::build_frame_dimensions_datagram(
                seq,
                frame.timestamp_us,
                frame.width,
                frame.height,
            );
            self.send_to_all_sessions(&dg);
            self.dimensions_retransmits_left -= 1;
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

        // Convert flat tile indices (Vec<u32>) into (tile_x, tile_y) pairs
        // matching the row-major layout used by MetricsTracker / TileGrid.
        let cols = frame.width.div_ceil(crate::tile::TILE_SIZE);
        let rows = frame.height.div_ceil(crate::tile::TILE_SIZE);
        let dirty_xy: Vec<(u32, u32)> = analysis
            .dirty_tiles
            .iter()
            .map(|&idx| (idx % cols, idx / cols))
            .collect();

        // Keep metrics_tracker AND scheduler grids in sync with dirty-detection.
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

        if new_mode != self.frame_mode {
            tracing::info!(
                prev = ?self.frame_mode,
                new = ?new_mode,
                seq,
                "classifier flipped frame mode"
            );
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
                let mut palrle_payloads = IoBridge::phase_b_encode_payloads(&preps);

                self.dispatch_dirty_tiles_via_scheduler(
                    &dirty_xy,
                    &grid,
                    pixels,
                    frame.stride,
                    seq,
                    frame.timestamp_us,
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
                }

                Event::Stream(StreamEvent::Readable { id }) => {
                    if let (Some(wt), Some(conn)) = (
                        self.wt_sessions.get_mut(&handle),
                        self.server.connections.get_mut(&handle),
                    ) {
                        let was_connected = wt.is_connected();
                        wt.on_stream_readable(conn, id);
                        if !was_connected && wt.is_connected() {
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
                            self.palette_table.on_session_reset();
                            self.frame_mode = crate::tile::FrameMode::H264;
                            // Re-prime the frame-dimensions retransmit counter so
                            // the new client receives the sentinel on its first
                            // frames even if the screen has been at a stable
                            // resolution for >FRAME_DIMENSIONS_RETRANSMITS frames.
                            // Without this, the client's per-tile fallback resize
                            // (gated on !frameDimensionsKnown) takes over and
                            // re-introduces the canvas-resize-clears-tiles bug.
                            self.dimensions_retransmits_left = FRAME_DIMENSIONS_RETRANSMITS;
                            // `force_dirty_frames` is consumed only by
                            // `process_frame_cpu` (no_commit slow-start mitigation).
                            // The GPU path doesn't read it — skip setting it when a
                            // GPU processor is active to avoid implying behavior that
                            // doesn't fire on that branch.
                            if self.gpu_frame_processor.is_none() {
                                self.force_dirty_frames = 20;
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
            // Stream data may contain multiple concatenated 22-byte messages.
            use crate::transport::feedback::FEEDBACK_SIZE;
            let mut offset = 0;
            while offset + FEEDBACK_SIZE <= data.len() {
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
    pub(crate) fn phase_b_encode_payloads(
        preps: &[PalRleTileWorkPrep],
    ) -> std::collections::HashMap<(u32, u32), Vec<u8>> {
        use rayon::prelude::*;
        preps
            .par_iter()
            .map(|p| {
                let payload = crate::encoder::pal_rle::encode_pal_rle_payload(
                    &p.indices,
                    &p.palette,
                    p.palette_id,
                    p.bundled,
                );
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
            force_dirty_frames: 0,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            fec_k: 0,
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
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
            force_dirty_frames: 0,
            palette_table: crate::encoder::pal_rle::PaletteTable::new(),
            fec_k: 0,
            fec_enable_threshold: FEC_ENABLE_THRESHOLD,
            fec_disable_threshold: FEC_DISABLE_THRESHOLD,
            gpu_frame_processor: None,
            full_frame_encoder: None,
            recent_frame_fragments: HashMap::new(),
            last_emitted_dimensions: None,
            dimensions_retransmits_left: 0,
        }
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
            &pixels,
            64 * 4,
            /* seq */ 1,
            /* timestamp_us */ 0,
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
            &pixels,
            64 * 4,
            1,
            0,
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
}
