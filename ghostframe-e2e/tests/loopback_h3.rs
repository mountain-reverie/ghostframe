//! In-memory loopback test: quinn-proto client <-> server + WebTransportServer.
//!
//! Verifies that:
//! 1. The QUIC handshake completes (Event::Connected on both sides).
//! 2. The client can open streams and write actual data (not just open empty streams).
//! 3. The WebTransportServer processes a full HTTP/3 handshake (SETTINGS + CONNECT)
//!    and reports `is_connected() == true`.
//!
//! Uses a sans-IO test harness modeled after quinn-proto's own internal test
//! utility: both endpoints live in the same process, datagrams are shuttled
//! through in-memory queues, and time is advanced explicitly.

use std::collections::{HashMap, VecDeque};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use bytes::{Bytes, BytesMut};
use quinn_proto::{
    ClientConfig, Connection, ConnectionEvent, ConnectionHandle, DatagramEvent, Dir, Endpoint,
    EndpointConfig, Event, Incoming, ServerConfig, StreamEvent, Transmit, TransportConfig,
};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use web_transport_proto::{ConnectRequest, Settings};

use ghostframe_lib::transport::webtransport::WebTransportServer;

// ---------------------------------------------------------------------------
// Test-only: TLS verifier that accepts any self-signed cert
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct AcceptAnyCert;

impl ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

// ---------------------------------------------------------------------------
// Sans-IO test endpoint, modeled after quinn-proto's own test harness
// ---------------------------------------------------------------------------

struct TestEndpoint {
    endpoint: Endpoint,
    addr: SocketAddr,
    outbound: VecDeque<(Transmit, Bytes)>,
    inbound: VecDeque<(Instant, Option<quinn_proto::EcnCodepoint>, BytesMut)>,
    connections: HashMap<ConnectionHandle, Connection>,
    conn_events: HashMap<ConnectionHandle, VecDeque<ConnectionEvent>>,
    timeout: Option<Instant>,
}

impl TestEndpoint {
    fn new(endpoint: Endpoint, addr: SocketAddr) -> Self {
        Self {
            endpoint,
            addr,
            outbound: VecDeque::new(),
            inbound: VecDeque::new(),
            connections: HashMap::new(),
            conn_events: HashMap::new(),
            timeout: None,
        }
    }

    /// Process all queued inbound datagrams. Connection events are queued for
    /// later dispatch in `drive_outgoing`.
    fn drive_incoming(&mut self, now: Instant, remote: SocketAddr) {
        let buf_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let mut buf = Vec::with_capacity(buf_size);

        while self.inbound.front().is_some_and(|x| x.0 <= now) {
            let (recv_time, ecn, packet) = self.inbound.pop_front().unwrap();
            match self
                .endpoint
                .handle(recv_time, remote, None, ecn, packet, &mut buf)
            {
                None => {}
                Some(DatagramEvent::NewConnection(incoming)) => {
                    self.accept(incoming, now, &mut buf);
                }
                Some(DatagramEvent::ConnectionEvent(ch, event)) => {
                    self.conn_events.entry(ch).or_default().push_back(event);
                }
                Some(DatagramEvent::Response(transmit)) => {
                    let size = transmit.size;
                    self.outbound.extend(split_transmit(transmit, &buf[..size]));
                    buf.clear();
                }
            }
        }
    }

    /// Dispatch queued connection events, poll endpoint events, drain transmits.
    /// Loops until there are no more endpoint events to process.
    fn drive_outgoing(&mut self, now: Instant) {
        let buf_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let mut buf = Vec::with_capacity(buf_size);

        loop {
            let mut endpoint_events: Vec<(ConnectionHandle, quinn_proto::EndpointEvent)> = vec![];

            for (ch, conn) in self.connections.iter_mut() {
                if self.timeout.is_some_and(|t| t <= now) {
                    self.timeout = None;
                    conn.handle_timeout(now);
                }

                // Dispatch all queued connection events.
                for (_, mut events) in self.conn_events.drain() {
                    for event in events.drain(..) {
                        conn.handle_event(event);
                    }
                }

                while let Some(event) = conn.poll_endpoint_events() {
                    endpoint_events.push((*ch, event));
                }

                while let Some(transmit) = conn.poll_transmit(now, 10, &mut buf) {
                    let size = transmit.size;
                    self.outbound.extend(split_transmit(transmit, &buf[..size]));
                    buf.clear();
                }

                self.timeout = conn.poll_timeout();
            }

            if endpoint_events.is_empty() {
                break;
            }

            for (ch, event) in endpoint_events {
                if let Some(conn_event) = self.endpoint.handle_event(ch, event) {
                    if let Some(conn) = self.connections.get_mut(&ch) {
                        conn.handle_event(conn_event);
                    }
                }
            }
        }
    }

    fn accept(&mut self, incoming: Incoming, now: Instant, buf: &mut Vec<u8>) {
        match self.endpoint.accept(incoming, now, buf, None) {
            Ok((ch, conn)) => {
                self.connections.insert(ch, conn);
            }
            Err(error) => {
                if let Some(transmit) = error.response {
                    let size = transmit.size;
                    self.outbound.extend(split_transmit(transmit, &buf[..size]));
                }
            }
        }
    }

    fn drive(&mut self, now: Instant, remote: SocketAddr) {
        self.drive_incoming(now, remote);
        self.drive_outgoing(now);
    }

    fn next_wakeup(&self) -> Option<Instant> {
        let next_inbound = self.inbound.front().map(|x| x.0);
        match (self.timeout, next_inbound) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (Some(a), _) => Some(a),
            (_, Some(b)) => Some(b),
            _ => None,
        }
    }
}

/// Split a GSO transmit into individual datagrams.
fn split_transmit(transmit: Transmit, buffer: &[u8]) -> Vec<(Transmit, Bytes)> {
    let mut buffer = Bytes::copy_from_slice(buffer);
    let segment_size = match transmit.segment_size {
        Some(s) => s,
        _ => return vec![(transmit, buffer)],
    };
    let mut out = Vec::new();
    while !buffer.is_empty() {
        let end = segment_size.min(buffer.len());
        let contents = buffer.split_to(end);
        out.push((
            Transmit {
                destination: transmit.destination,
                size: contents.len(),
                ecn: transmit.ecn,
                segment_size: None,
                src_ip: transmit.src_ip,
            },
            contents,
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// Pair: drives both endpoints in lockstep
// ---------------------------------------------------------------------------

struct Pair {
    client: TestEndpoint,
    server: TestEndpoint,
    time: Instant,
}

impl Pair {
    /// Drive one step: client -> server -> advance time.
    /// Returns true if there is more work to do.
    fn step(&mut self) -> bool {
        self.drive_client();
        self.drive_server();

        let has_inbound = !self.client.inbound.is_empty() || !self.server.inbound.is_empty();

        if !has_inbound {
            let client_t = self.client.next_wakeup();
            let server_t = self.server.next_wakeup();
            let next = match (client_t, server_t) {
                (Some(a), Some(b)) => a.min(b),
                (Some(a), _) => a,
                (_, Some(b)) => b,
                _ => return false,
            };
            // Cap time advance at 1s to avoid jumping to idle timeout.
            let max_advance = self.time + std::time::Duration::from_secs(1);
            if next > max_advance {
                return false;
            }
            self.time = self.time.max(next);
        }
        true
    }

    /// Drive until both sides have exchanged all pending data.
    fn drive(&mut self) {
        for _ in 0..100 {
            if !self.step() {
                break;
            }
        }
    }

    fn drive_client(&mut self) {
        self.client.drive(self.time, self.server.addr);
        for (transmit, buffer) in self.client.outbound.drain(..) {
            if self.server.addr == transmit.destination {
                self.server.inbound.push_back((
                    self.time,
                    transmit.ecn,
                    BytesMut::from(buffer.as_ref()),
                ));
            }
        }
    }

    fn drive_server(&mut self) {
        self.server.drive(self.time, self.client.addr);
        for (transmit, buffer) in self.server.outbound.drain(..) {
            if self.client.addr == transmit.destination {
                self.client.inbound.push_back((
                    self.time,
                    transmit.ecn,
                    BytesMut::from(buffer.as_ref()),
                ));
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The test
// ---------------------------------------------------------------------------

#[test]
fn loopback_h3_handshake() {
    // -- 1. Generate self-signed cert --
    let mut params =
        rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
    let now_utc = time::OffsetDateTime::now_utc();
    params.not_before = now_utc;
    params.not_after = now_utc + time::Duration::days(13);
    let key_pair = rcgen::KeyPair::generate().unwrap();
    let cert = params.self_signed(&key_pair).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = key_pair.serialize_der();

    let cert_chain: Vec<CertificateDer<'static>> = vec![CertificateDer::from(cert_der)];
    let private_key =
        rustls::pki_types::PrivateKeyDer::try_from(key_der).expect("invalid private key");

    // -- Server TLS --
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut server_tls = rustls::ServerConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(cert_chain, private_key)
        .unwrap();
    server_tls.alpn_protocols = vec![b"h3".to_vec()];

    let quic_server_tls =
        quinn_proto::crypto::rustls::QuicServerConfig::try_from(server_tls).unwrap();

    let mut server_transport = TransportConfig::default();
    server_transport.datagram_receive_buffer_size(Some(65536));
    server_transport.datagram_send_buffer_size(65536);

    let mut server_config = ServerConfig::with_crypto(Arc::new(quic_server_tls));
    server_config.transport = Arc::new(server_transport);

    let server_ep = Endpoint::new(
        Arc::new(EndpointConfig::default()),
        Some(Arc::new(server_config)),
        false,
        None,
    );

    // -- Client TLS --
    let mut client_tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();
    client_tls.alpn_protocols = vec![b"h3".to_vec()];

    let quic_client_tls =
        quinn_proto::crypto::rustls::QuicClientConfig::try_from(client_tls).unwrap();

    let mut client_transport = TransportConfig::default();
    client_transport.datagram_receive_buffer_size(Some(65536));
    client_transport.datagram_send_buffer_size(65536);

    let mut client_config = ClientConfig::new(Arc::new(quic_client_tls));
    client_config.transport_config(Arc::new(client_transport));

    let client_ep = Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None);

    // -- Addresses --
    let server_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 4443);
    let client_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000);

    let now = Instant::now();
    let mut pair = Pair {
        client: TestEndpoint::new(client_ep, client_addr),
        server: TestEndpoint::new(server_ep, server_addr),
        time: now,
    };

    // -- 2. Initiate client connection --
    let (client_ch, client_conn) = pair
        .client
        .endpoint
        .connect(now, client_config, server_addr, "localhost")
        .expect("client connect");
    pair.client.connections.insert(client_ch, client_conn);

    // -- 3. Drive the QUIC handshake to completion --
    pair.drive();

    assert_eq!(
        pair.server.connections.len(),
        1,
        "server should have exactly one connection"
    );
    let server_ch = *pair.server.connections.keys().next().unwrap();

    // Verify both sides fire Event::Connected.
    let mut client_connected = false;
    let mut server_connected = false;

    while let Some(event) = pair.client.connections.get_mut(&client_ch).unwrap().poll() {
        if matches!(event, Event::Connected) {
            client_connected = true;
        }
    }
    while let Some(event) = pair.server.connections.get_mut(&server_ch).unwrap().poll() {
        if matches!(event, Event::Connected) {
            server_connected = true;
        }
    }

    assert!(client_connected, "client must fire Event::Connected");
    assert!(server_connected, "server must fire Event::Connected");

    // -- 4. Initialize WebTransportServer: server sends its own SETTINGS --
    let mut wt = WebTransportServer::new();
    {
        let conn = pair.server.connections.get_mut(&server_ch).unwrap();
        wt.on_new_connection(conn);
    }

    // -- 5. Client opens streams and writes HTTP/3 frames --
    {
        let client_conn = pair.client.connections.get_mut(&client_ch).unwrap();

        // Uni stream: stream type 0x00 + SETTINGS frame (via web-transport-proto).
        let uni_sid = client_conn
            .streams()
            .open(Dir::Uni)
            .expect("open uni stream");
        let mut settings = Settings::default();
        settings.enable_webtransport(1);
        let mut settings_buf = BytesMut::new();
        settings.encode(&mut settings_buf);
        let written = client_conn
            .send_stream(uni_sid)
            .write(&settings_buf)
            .expect("write SETTINGS");
        assert!(
            written > 0,
            "client must write SETTINGS data, not just open an empty stream"
        );

        // Bidi stream: CONNECT request.
        let bidi_sid = client_conn
            .streams()
            .open(Dir::Bi)
            .expect("open bidi stream");
        let connect = ConnectRequest::new(
            url::Url::parse("https://localhost/.well-known/webtransport").unwrap(),
        );
        let mut connect_buf = BytesMut::new();
        connect.encode(&mut connect_buf).expect("encode CONNECT");
        let written = client_conn
            .send_stream(bidi_sid)
            .write(&connect_buf)
            .expect("write CONNECT");
        assert!(
            written > 0,
            "client must write CONNECT data, not just open an empty stream"
        );
    }

    // -- 6. Drive the stream data from client to server --
    pair.drive();

    // -- 7. Process server-side events through WebTransportServer --
    {
        let conn = pair.server.connections.get_mut(&server_ch).unwrap();
        while let Some(event) = conn.poll() {
            match event {
                Event::Stream(StreamEvent::Opened { dir }) => {
                    wt.on_stream_opened(conn, dir);
                }
                Event::Stream(StreamEvent::Readable { id }) => {
                    wt.on_stream_readable(conn, id);
                }
                _ => {}
            }
        }

        // Force-read known streams in case data arrived without a separate
        // Readable event.
        for sid in wt.all_known_stream_ids() {
            wt.on_stream_readable(conn, sid);
        }
    }

    // -- 8. Assertions --
    assert!(
        wt.is_connected(),
        "WebTransportServer must complete the handshake (SETTINGS + CONNECT -> 200)"
    );
}
