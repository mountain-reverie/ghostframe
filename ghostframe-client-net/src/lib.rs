//! Sans-IO QUIC + WebTransport client session wrapping `ClientCore`.
//!
//! There is deliberately no dial API and no socket code in this crate: the
//! embedder supplies the byte pump. In production that pump is ghostbridge's
//! `dial_udp` through tsnet, so a client can never escape the tailnet; in
//! tests it is the netsim.

mod clock;
mod endpoint;
mod event;
mod handshake;
pub mod tls;

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use quinn_proto::TransportConfig;

use clock::Instant;
use endpoint::ClientEndpoint;
use handshake::{ConnectOutcome, WebTransportHandshake};

pub use event::{ClientNetError, ClientNetEvent, UdpOut};

#[derive(Debug, Clone)]
pub struct ClientNetConfig {
    /// SNI name presented in the TLS handshake.
    pub server_name: String,
    /// The only certificate this client will accept, by SHA-256 of its DER.
    pub server_cert_sha256: [u8; 32],
    pub indices_raw_enabled: bool,
    pub supports_h264: bool,
}

pub struct ClientNet {
    config: ClientNetConfig,
    connected: bool,
    endpoint: ClientEndpoint,
    handshake: WebTransportHandshake,
    events: Vec<ClientNetEvent>,
    /// Wall-clock anchor captured once at construction. Every other piece of
    /// timing in this crate is injected microseconds (`now_us`); this is the
    /// sole `Instant::now()` call, needed only because quinn-proto's API is
    /// expressed in `Instant`, not because the crate tracks real time itself.
    /// See the "Wasm" note on `ClientNet::new` for why this is safe to build
    /// on `wasm32-unknown-unknown`.
    base: Instant,
}

impl ClientNet {
    /// `now_us` is unused today (there is nothing to schedule before
    /// `connect`), but is accepted for symmetry with the rest of this
    /// crate's injected-time API and so a future revision can seed
    /// `base`-relative state without changing the signature.
    ///
    /// Wasm note: bare `std::time::Instant::now()` panics on
    /// `wasm32-unknown-unknown`. `Instant` here is `crate::clock::Instant`,
    /// which is `std::time::Instant` on every target except wasm, where it
    /// is `web_time::Instant` (backed by `Performance.now()` via
    /// `wasm-bindgen`) — the same conditional type quinn-proto itself picks
    /// internally, which is why this crate has to mirror it rather than pick
    /// its own. This is the only place in the crate that calls `::now()`;
    /// every other method takes `now_us: u64` and derives an `Instant` from
    /// `base`, so the wasm/native split is confined to `clock.rs` and this
    /// one call site.
    pub fn new(config: ClientNetConfig, _now_us: u64) -> Result<Self, ClientNetError> {
        Ok(Self {
            config,
            connected: false,
            endpoint: ClientEndpoint::new(),
            handshake: WebTransportHandshake::new(),
            events: Vec::new(),
            base: Instant::now(),
        })
    }

    fn instant(&self, now_us: u64) -> Instant {
        self.base + Duration::from_micros(now_us)
    }

    /// Name the peer for quinn-proto's state machine and start the QUIC
    /// handshake. This does not open a socket — it only tells quinn-proto
    /// which `SocketAddr` outbound datagrams should be addressed to; the
    /// embedder's byte pump (tsnet's `dial_udp` in production, the netsim in
    /// tests) is what actually moves bytes.
    pub fn connect(&mut self, remote: SocketAddr, now_us: u64) -> Result<(), ClientNetError> {
        let now = self.instant(now_us);

        let verifier = Arc::new(tls::PinnedCertVerifier::new(self.config.server_cert_sha256));
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .map_err(|e| ClientNetError::Tls(e.to_string()))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth();
        client_tls.alpn_protocols = vec![b"h3".to_vec()];

        let quic_client_tls = quinn_proto::crypto::rustls::QuicClientConfig::try_from(client_tls)
            .map_err(|e| ClientNetError::Tls(e.to_string()))?;

        let mut transport = TransportConfig::default();
        transport.datagram_receive_buffer_size(Some(65536));
        transport.datagram_send_buffer_size(65536);

        let mut client_config = quinn_proto::ClientConfig::new(Arc::new(quic_client_tls));
        client_config.transport_config(Arc::new(transport));

        self.endpoint
            .connect(now, client_config, remote, &self.config.server_name)
            .map_err(|e| ClientNetError::Connect(e.to_string()))?;

        // Flush the Initial packet produced by `connect` into the outbound
        // queue so the first call to `poll_transmit` has something to give
        // the byte pump.
        self.endpoint.drive_outgoing(now);
        self.drain_connection_events();

        Ok(())
    }

    /// Feed one inbound datagram (received by the embedder's byte pump) into
    /// the QUIC state machine.
    pub fn handle_udp(&mut self, data: &[u8], remote: SocketAddr, now_us: u64) {
        let now = self.instant(now_us);
        self.endpoint
            .drive_incoming(now, remote, None, BytesMut::from(data));
        self.endpoint.drive_outgoing(now);
        self.drain_connection_events();
    }

    /// Poll `quinn_proto::Event`s off the active connection and translate
    /// the ones this crate currently understands into `ClientNetEvent`s.
    ///
    /// `Connected` kicks off the WebTransport handshake (opens the SETTINGS
    /// and CONNECT streams); `Stream(StreamEvent::Readable)` on the session
    /// stream drives it forward until the CONNECT response decodes.
    /// Datagram events are Task 8's concern and are left unhandled here.
    fn drain_connection_events(&mut self) {
        let Some(conn) = self.endpoint.connection() else {
            return;
        };
        while let Some(event) = conn.poll() {
            match event {
                quinn_proto::Event::Connected => {
                    self.connected = true;
                    self.events.push(ClientNetEvent::Connected);
                    if let Err(e) = self.handshake.start(conn, &self.config.server_name) {
                        self.events.push(ClientNetEvent::ConnectionLost {
                            reason: e.to_string(),
                        });
                    }
                }
                quinn_proto::Event::Stream(quinn_proto::StreamEvent::Readable { id }) => {
                    if self.handshake.session_stream() == Some(id) {
                        match self.handshake.on_readable(conn, id) {
                            ConnectOutcome::Pending => {}
                            ConnectOutcome::Accepted => {
                                self.events.push(ClientNetEvent::SessionReady);
                            }
                            ConnectOutcome::Rejected(reason) => {
                                self.events.push(ClientNetEvent::ConnectionLost { reason });
                            }
                        }
                    }
                }
                quinn_proto::Event::ConnectionLost { reason } => {
                    self.connected = false;
                    self.events.push(ClientNetEvent::ConnectionLost {
                        reason: reason.to_string(),
                    });
                }
                _ => {}
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Drain and return every event accumulated since the last call. Events
    /// can be produced by a timer expiry inside `drive_outgoing` as well as
    /// by an inbound datagram, so this single drain point (rather than a
    /// return value on `handle_udp`) is the only way callers are guaranteed
    /// not to miss one.
    pub fn take_events(&mut self) -> Vec<ClientNetEvent> {
        std::mem::take(&mut self.events)
    }

    pub fn poll_transmit(&mut self) -> Option<UdpOut> {
        let (transmit, bytes) = self.endpoint.pop_outbound()?;
        Some(UdpOut {
            payload: bytes.to_vec(),
            destination: transmit.destination,
        })
    }

    pub fn server_name(&self) -> &str {
        &self.config.server_name
    }
}
