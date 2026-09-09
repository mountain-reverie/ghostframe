//! Sans-IO quinn-proto endpoint driver, narrowed to the client role.
//!
//! Modeled on `TestEndpoint` in `ghostframe-e2e/tests/loopback_h3.rs:78-207`,
//! which drives a generic (client-or-server) endpoint against a
//! `HashMap<ConnectionHandle, Connection>`. A client only ever has one
//! connection — the one it dials — so that map collapses to
//! `Option<(ConnectionHandle, Connection)>` and the per-connection loops in
//! `drive_outgoing` lose their outer iteration.

use std::net::SocketAddr;

use crate::clock::Instant;
use bytes::{Bytes, BytesMut};
use quinn_proto::{
    ClientConfig, Connection, ConnectionEvent, ConnectionHandle, DatagramEvent, Endpoint,
    EndpointConfig, Transmit,
};
use std::collections::VecDeque;
use std::sync::Arc;

/// Sans-IO client endpoint: exactly one connection, driven by injected
/// `Instant`s and datagrams handed in by the caller.
pub(crate) struct ClientEndpoint {
    endpoint: Endpoint,
    outbound: VecDeque<(Transmit, Bytes)>,
    conn: Option<(ConnectionHandle, Connection)>,
    conn_events: VecDeque<ConnectionEvent>,
    timeout: Option<Instant>,
}

impl ClientEndpoint {
    pub(crate) fn new() -> Self {
        // Client endpoints never accept incoming connections, so there is no
        // `ServerConfig`.
        let endpoint = Endpoint::new(Arc::new(EndpointConfig::default()), None, false, None);
        Self {
            endpoint,
            outbound: VecDeque::new(),
            conn: None,
            conn_events: VecDeque::new(),
            timeout: None,
        }
    }

    pub(crate) fn connect(
        &mut self,
        now: Instant,
        config: ClientConfig,
        remote: SocketAddr,
        server_name: &str,
    ) -> Result<(), quinn_proto::ConnectError> {
        let (ch, conn) = self.endpoint.connect(now, config, remote, server_name)?;
        self.conn = Some((ch, conn));
        Ok(())
    }

    /// Feed one inbound datagram into the endpoint state machine.
    ///
    /// `DatagramEvent::NewConnection` cannot occur for a client endpoint: it
    /// is only produced when a `ServerConfig` is installed (see
    /// `quinn_proto::Endpoint::handle`), and this endpoint is built with
    /// `Endpoint::new(.., None, ..)`. Treat it as a bug rather than silently
    /// discarding the incoming: if quinn-proto ever surfaces it here, our
    /// assumption about client-only wiring has broken.
    pub(crate) fn drive_incoming(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        ecn: Option<quinn_proto::EcnCodepoint>,
        packet: BytesMut,
    ) {
        let buf_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let mut buf = Vec::with_capacity(buf_size);

        match self
            .endpoint
            .handle(now, remote, None, ecn, packet, &mut buf)
        {
            None => {}
            Some(DatagramEvent::NewConnection(_incoming)) => {
                // Unreachable today: quinn-proto only produces this when a
                // ServerConfig is installed, and `new()` passes `None`. Loud in
                // debug so a future change that installs one is caught at once;
                // dropped rather than panicking in release, because this is the
                // datagram path of a crate that becomes the native client, and
                // `ClientCore`'s contract next door is that hostile input never
                // panics. An inbound packet must not be able to kill the client.
                debug_assert!(
                    false,
                    "client-only ClientEndpoint received DatagramEvent::NewConnection; \
                     no ServerConfig is installed, so quinn-proto should never produce this"
                );
                tracing::error!("ignoring unexpected NewConnection on a client-only endpoint");
            }
            Some(DatagramEvent::ConnectionEvent(ch, event)) => {
                if self.conn.as_ref().is_some_and(|(c, _)| *c == ch) {
                    self.conn_events.push_back(event);
                } else {
                    tracing::warn!(?ch, "ConnectionEvent for unknown connection");
                }
            }
            Some(DatagramEvent::Response(transmit)) => {
                let size = transmit.size;
                self.outbound.extend(split_transmit(transmit, &buf[..size]));
            }
        }
    }

    /// Dispatch queued connection events, poll endpoint events, drain transmits.
    /// Loops until there are no more endpoint events to process.
    pub(crate) fn drive_outgoing(&mut self, now: Instant) {
        let buf_size = self.endpoint.config().get_max_udp_payload_size() as usize;
        let mut buf = Vec::with_capacity(buf_size);

        loop {
            let mut endpoint_events: Vec<(ConnectionHandle, quinn_proto::EndpointEvent)> = vec![];

            if let Some((ch, conn)) = self.conn.as_mut() {
                if self.timeout.is_some_and(|t| t <= now) {
                    self.timeout = None;
                    conn.handle_timeout(now);
                }

                for event in self.conn_events.drain(..) {
                    conn.handle_event(event);
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
                    if let Some((c, conn)) = self.conn.as_mut() {
                        if *c == ch {
                            conn.handle_event(conn_event);
                        }
                    }
                }
            }
        }
    }

    /// Earliest deadline this endpoint needs to be woken at, independent of
    /// any inbound datagram — mirrors `QuicServer::next_timeout`
    /// (`ghostframe-lib/src/transport/quic.rs`). Not wired into `ClientNet`'s
    /// public API yet: today `drive_outgoing` only fires a due timeout when
    /// it is invoked from `handle_udp`, which is sufficient for the
    /// handshake this task drives to completion. A production I/O bridge
    /// that must retransmit without waiting on the peer will need to poll
    /// this and call back in on expiry, the same way the pump in
    /// `tests/handshake.rs` does for the server side.
    #[allow(dead_code)]
    pub(crate) fn next_wakeup(&self) -> Option<Instant> {
        self.timeout
    }

    pub(crate) fn pop_outbound(&mut self) -> Option<(Transmit, Bytes)> {
        self.outbound.pop_front()
    }

    pub(crate) fn connection(&mut self) -> Option<&mut Connection> {
        self.conn.as_mut().map(|(_, conn)| conn)
    }
}

/// Split a GSO transmit into individual datagrams. Copied verbatim from
/// `loopback_h3.rs:209-231` — role-independent.
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
