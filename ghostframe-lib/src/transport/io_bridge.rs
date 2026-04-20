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
use std::os::unix::io::FromRawFd;
use std::pin::Pin;
use std::time::Instant;

use bytes::BytesMut;
use quinn_proto::{ConnectionHandle, Event, StreamEvent};
use std::collections::HashMap;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::sync::mpsc;
use tokio::time::{sleep_until, Instant as TokioInstant};

use crate::encoder::h264_vaapi::H264VaapiEncoder;
use crate::server::FrameSubmission;
use crate::tile::{DirtyTracker, TileGrid};
use crate::transport::ghostbridge::{
    encode_frame, parse_frame_rest, GhostbridgeConfig, GhostbridgeHandle,
};
use crate::transport::protocol::{
    Codec, fragment_tile, max_fragment_payload, PING_PAYLOAD, PONG_PAYLOAD,
};
use crate::transport::quic::QuicServer;
use crate::transport::webtransport::WebTransportServer;

/// Scratch buffer size for quinn-proto's `endpoint.handle` / `conn.poll_transmit`.
/// Sized above a 1500-byte MTU with headroom for ECN/padding; quinn-proto may
/// write a full datagram into this buffer in a single call.
const QUIC_SCRATCH: usize = 2048;

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
    /// Per-tile H.264 encoders, keyed by (tile_x, tile_y). Lazily initialized.
    encoders: HashMap<(u32, u32), H264VaapiEncoder>,
    /// Whether H.264 encoding is available (checked once at startup).
    h264_available: bool,
}

impl IoBridge {
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

        let h264_available = H264VaapiEncoder::new().is_ok();

        Ok(Self {
            _handle: Some(handle),
            stream,
            server,
            local_addr,
            wt_sessions: HashMap::new(),
            frame_rx: None,
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            encoders: HashMap::new(),
            h264_available,
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

    fn process_frame(&mut self, frame: FrameSubmission) {
        let grid = TileGrid::new(frame.width, frame.height);
        self.frame_seq = self.frame_seq.wrapping_add(1);
        let seq = self.frame_seq;

        // Determine dirty tiles
        let dirty_tiles = match &frame.damage_tiles {
            Some(hints) => self.dirty_tracker.update_with_hints(
                &frame.pixels, frame.stride, frame.width, frame.height, hints,
            ),
            None => self.dirty_tracker.update(
                &frame.pixels, frame.stride, frame.width, frame.height,
            ),
        };

        if dirty_tiles.is_empty() {
            return;
        }

        // Determine the maximum fragment payload from the smallest connected
        // session's QUIC max datagram size.  We subtract the WebTransport
        // VarInt prefix and our own headers so the final QUIC DATAGRAM frame
        // fits within the path MTU.
        let max_frag = {
            let mut min_size: Option<usize> = None;
            for (handle, wt) in &self.wt_sessions {
                if !wt.is_connected() {
                    continue;
                }
                if let Some(conn) = self.server.connections.get_mut(handle) {
                    if let Some(sz) = conn.datagrams().max_size() {
                        let usable = max_fragment_payload(
                            sz.saturating_sub(Self::WT_VARINT_OVERHEAD),
                        );
                        min_size = Some(match min_size {
                            Some(prev) => prev.min(usable),
                            None => usable,
                        });
                    }
                }
            }
            match min_size {
                Some(0) | None => {
                    // No connected sessions or MTU too small — skip frame.
                    return;
                }
                Some(sz) => sz,
            }
        };

        // Encode and send only dirty tiles
        for (tile_x, tile_y) in &dirty_tiles {
            let tile_data = grid.extract_tile(&frame.pixels, frame.stride, *tile_x, *tile_y);

            let (codec, payload) = if self.h264_available {
                let encoder = self.encoders
                    .entry((*tile_x, *tile_y))
                    .or_insert_with(|| H264VaapiEncoder::new().unwrap());
                match encoder.encode(&tile_data) {
                    Ok(Some(encoded)) => (encoded.codec, encoded.payload),
                    Ok(None) => (Codec::Raw, tile_data),
                    Err(e) => {
                        tracing::warn!(tile_x, tile_y, error = %e, "H.264 encode failed, sending raw");
                        (Codec::Raw, tile_data)
                    }
                }
            } else {
                (Codec::Raw, tile_data)
            };

            let datagrams = fragment_tile(
                seq,
                *tile_x as u8,
                *tile_y as u8,
                codec,
                &payload,
                frame.timestamp_us,
                max_frag,
            );

            for dg in &datagrams {
                for (handle, wt) in &mut self.wt_sessions {
                    if !wt.is_connected() {
                        continue;
                    }
                    if let Some(conn) = self.server.connections.get_mut(handle) {
                        if let Err(e) = wt.send_datagram(conn, dg) {
                            tracing::trace!(?handle, tile_x, tile_y, error = ?e,
                                "datagram send failed (congestion/buffer full)");
                        }
                    }
                }
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
                        wt.on_stream_readable(conn, id);
                    }
                }

                Event::DatagramReceived => {
                    if let Some(wt) = self.wt_sessions.get_mut(&handle) {
                        if let Some(conn) = self.server.connections.get_mut(&handle) {
                            while let Some(payload) = wt.recv_datagram(conn) {
                                if let Some(response) = handle_datagram_payload(&payload) {
                                    tracing::debug!(?handle, "ping received, sending pong");
                                    if let Err(e) = wt.send_datagram(conn, response) {
                                        tracing::warn!(?handle, error = ?e, "failed to send pong");
                                    }
                                } else {
                                    tracing::trace!(
                                        ?handle,
                                        bytes = payload.len(),
                                        "WebTransport datagram received (unknown payload)"
                                    );
                                }
                            }
                        }
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
    }

    /// Test-only constructor that accepts a pre-built stream and server,
    /// bypassing the real ghostbridge connection. No tsnet node is held, so
    /// `_handle` is `None` and no `gbridge_close` is called on Drop.
    #[cfg(test)]
    pub(crate) fn new_with_stream_for_test(stream: TokioUnixStream, server: QuicServer) -> Self {
        IoBridge {
            _handle: None,
            stream,
            server,
            local_addr: "0.0.0.0:4443".parse().unwrap(),
            wt_sessions: HashMap::new(),
            frame_rx: None,
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            encoders: HashMap::new(),
            h264_available: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_frames_for_test(
        stream: TokioUnixStream, server: QuicServer,
        frame_rx: mpsc::Receiver<FrameSubmission>,
    ) -> Self {
        IoBridge {
            _handle: None,
            stream,
            server,
            local_addr: "0.0.0.0:4443".parse().unwrap(),
            wt_sessions: HashMap::new(),
            frame_rx: Some(frame_rx),
            frame_seq: 0,
            dirty_tracker: DirtyTracker::new(0, 0),
            encoders: HashMap::new(),
            h264_available: false,
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
            width: 64, height: 64, stride: 64 * 4,
            pixels: vec![0xFF; 64 * 64 * 4],
            timestamp_us: 1000,
            damage_tiles: None,
        };
        // No connected sessions, so datagrams go nowhere, but should not panic
        bridge.process_frame(frame);
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
            width: 32, height: 32, stride: 32 * 4,
            pixels: vec![0; 32 * 32 * 4],
            timestamp_us: 0,
            damage_tiles: None,
        };

        bridge.process_frame(make_frame());
        assert_eq!(bridge.frame_seq, 1);

        bridge.process_frame(make_frame());
        assert_eq!(bridge.frame_seq, 2);
    }
}
