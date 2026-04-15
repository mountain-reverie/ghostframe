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
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::os::unix::io::FromRawFd;
use std::pin::Pin;
use std::time::Instant;

use bytes::BytesMut;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream as TokioUnixStream;
use tokio::time::{sleep_until, Instant as TokioInstant};

use crate::transport::ghostbridge::{
    encode_frame, parse_frame_rest, GhostbridgeConfig, GhostbridgeHandle,
};
use crate::transport::quic::QuicServer;

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

        let udp = handle.listen_udp(listen_addr)?;
        let raw_fd = udp.into_raw_fd();

        // Convert to std::os::unix::net::UnixStream → tokio::net::UnixStream.
        // ghostbridge already set O_NONBLOCK on this fd; set_nonblocking is a
        // no-op sanity call.
        let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
        std_stream.set_nonblocking(true)?;
        let stream = TokioUnixStream::from_std(std_stream)?;

        let server = QuicServer::new()?;
        tracing::info!(cert_sha256 = %server.cert_info().sha256_hex, "QUIC server ready");

        // Derive local_addr from listen_addr so that quinn-proto's local_ip
        // hint matches the port we actually bound. listen_addr is in the form
        // "[host]:port" or ":port"; for M0 the host is always "0.0.0.0".
        let port = parse_listen_port(listen_addr)?;
        let local_addr = SocketAddr::from((IpAddr::V4(Ipv4Addr::UNSPECIFIED), port));

        Ok(Self {
            _handle: Some(handle),
            stream,
            server,
            local_addr,
        })
    }

    /// Return the SHA-256 hex fingerprint of the server's self-signed cert.
    pub fn cert_hash_sha256(&self) -> &str {
        &self.server.cert_info().sha256_hex
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

                // 2. Inbound framed UDP packet from ghostbridge.
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
            self.drain_outbound().await?;
            self.server.drain_endpoint_events();
            self.drain_app_events();
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
                    let frame = encode_frame(payload, &transmit.destination);
                    self.stream.write_all(&frame).await?;
                }
            }
        }

        Ok(())
    }

    /// Log all pending application-level QUIC events.
    ///
    /// Task 6 / Task 8 will add real handling here (WebTransport handshake,
    /// ping/pong dispatch).
    fn drain_app_events(&mut self) {
        while let Some((handle, event)) = self.server.poll_events() {
            tracing::trace!(?handle, ?event, "app event");
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
}
