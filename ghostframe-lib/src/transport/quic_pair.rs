//! A real quinn-proto client joined back to back with the production
//! `QuicServer`, no socket and a clock the test moves. Shared by the
//! departure-tracker tests and the stats-service tests.

use bytes::BytesMut;
use quinn_proto::{
    ClientConfig, Connection, ConnectionHandle, DatagramEvent, Endpoint, EndpointConfig,
};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::transport::quic::QuicServer;

/// A server (the production `QuicServer`) and a client endpoint joined
/// back to back with no socket, no harness and a clock the test moves.
pub(crate) struct Pair {
    pub(crate) server: QuicServer,
    pub(crate) server_handle: ConnectionHandle,
    pub(crate) client_ep: Endpoint,
    pub(crate) client: Connection,
    pub(crate) now: Instant,
    pub(crate) saddr: SocketAddr,
    pub(crate) caddr: SocketAddr,
    pub(crate) cap: usize,
}

impl Pair {
    pub(crate) fn new(cap: usize) -> Self {
        let mut server = QuicServer::new_with_datagram_send_buffer(cap).expect("server");
        server.set_peer_gate(crate::transport::peer_auth::PeerGate::single_owner_for_tests());
        let mut roots = rustls::RootCertStore::empty();
        roots
            .add(rustls::pki_types::CertificateDer::from(
                server.cert_info().schedule.serving_cert_der(),
            ))
            .expect("trust the server's own certificate");
        let mut tls = rustls::ClientConfig::builder_with_provider(Arc::new(
            crate::transport::quic::pq_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
        tls.alpn_protocols = vec![b"h3".to_vec()];
        let quic = quinn_proto::crypto::rustls::QuicClientConfig::try_from(tls).unwrap();
        let client_config = ClientConfig::new(Arc::new(quic));
        let mut client_ep = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            None,
            true,
            Some([7u8; 32]),
        );
        let saddr: SocketAddr = "127.0.0.1:4433".parse().unwrap();
        let caddr: SocketAddr = "127.0.0.1:50000".parse().unwrap();
        let now = Instant::now();
        let (_, client) = client_ep
            .connect(now, client_config, saddr, "localhost")
            .expect("connect");
        let mut p = Self {
            server,
            server_handle: ConnectionHandle(0),
            client_ep,
            client,
            now,
            saddr,
            caddr,
            cap,
        };
        for _ in 0..200 {
            p.step_all(Duration::from_millis(2));
            if let Some(h) = p.server.take_accepted().first() {
                p.server_handle = *h;
            }
            if p.server
                .connections
                .get_mut(&p.server_handle)
                .is_some_and(|c| !c.is_handshaking() && c.datagrams().max_size().is_some())
                && !p.client.is_handshaking()
            {
                // Let the handshake's tail (ACKs, HANDSHAKE_DONE) settle.
                for _ in 0..10 {
                    p.step_all(Duration::from_millis(2));
                }
                return p;
            }
        }
        panic!("handshake did not complete");
    }

    pub(crate) fn server_conn(&mut self) -> &mut Connection {
        self.server
            .connections
            .get_mut(&self.server_handle)
            .unwrap()
    }

    /// Advance the clock, run timers, and move every packet both ways
    /// until quiet. Drains the server's FIFO as far as quinn lets it.
    pub(crate) fn step_all(&mut self, dt: Duration) {
        self.now += dt;
        let now = self.now;
        self.server.handle_timeout(now);
        if self.client.poll_timeout().is_some_and(|x| x <= now) {
            self.client.handle_timeout(now);
        }
        for _ in 0..64 {
            let mut moved = false;
            let mut buf = Vec::new();
            while let Some(tx) = self.client.poll_transmit(now, 1, &mut buf) {
                moved = true;
                let mut resp = Vec::new();
                let pkt = BytesMut::from(&buf[..tx.size]);
                if let Some(r) = self
                    .server
                    .handle_datagram(now, self.caddr, None, tx.ecn, pkt, &mut resp)
                {
                    let mut out = BytesMut::from(&resp[..r.size]);
                    self.client_receive(now, &mut out);
                }
                buf.clear();
            }
            self.server.drain_endpoint_events();
            let mut buf = Vec::new();
            while let Some(tx) = self.server.poll_transmit(now, 1, &mut buf) {
                moved = true;
                let mut pkt = BytesMut::from(&buf[..tx.size]);
                self.client_receive(now, &mut pkt);
                buf.clear();
            }
            self.drain_client_endpoint();
            if !moved {
                break;
            }
        }
    }

    pub(crate) fn client_receive(&mut self, now: Instant, pkt: &mut BytesMut) {
        let mut resp = Vec::new();
        if let Some(DatagramEvent::ConnectionEvent(_, ev)) =
            self.client_ep
                .handle(now, self.saddr, None, None, std::mem::take(pkt), &mut resp)
        {
            self.client.handle_event(ev);
        }
    }

    pub(crate) fn drain_client_endpoint(&mut self) {
        while let Some(ev) = self.client.poll_endpoint_events() {
            // The handle is irrelevant to a one-connection endpoint.
            if let Some(ce) = self.client_ep.handle_event(ConnectionHandle(0), ev) {
                self.client.handle_event(ce);
            }
        }
        while self.client.poll().is_some() {}
    }

    /// One `drain_outbound`: poll the server until quinn has nothing more
    /// to send now. Returns the raw packets so the caller can choose to
    /// deliver them.
    pub(crate) fn drain_server(&mut self) -> Vec<Vec<u8>> {
        let now = self.now;
        let mut out = Vec::new();
        let mut buf = Vec::new();
        while let Some(tx) = self.server_conn().poll_transmit(now, 1, &mut buf) {
            out.push(buf[..tx.size].to_vec());
            buf.clear();
        }
        out
    }

    pub(crate) fn deliver_to_client(&mut self, pkts: Vec<Vec<u8>>) {
        let now = self.now;
        for p in pkts {
            let mut b = BytesMut::from(&p[..]);
            self.client_receive(now, &mut b);
        }
        self.drain_client_endpoint();
    }

    pub(crate) fn queued(&mut self) -> u64 {
        let cap = self.cap;
        (cap - self.server_conn().datagrams().send_buffer_space()) as u64
    }
}
