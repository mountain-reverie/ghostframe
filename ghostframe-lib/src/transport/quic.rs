//! Quinn-proto QUIC endpoint wrapper.
//!
//! Task 4: constructs a `quinn_proto::Endpoint` configured for WebTransport
//! (HTTP/3 ALPN, datagrams enabled), generates a self-signed cert with
//! `localhost` and `127.0.0.1` SANs, and exports the SHA-256 of the cert DER
//! for browser cert-hash pinning.  Method bodies filled in Task 5.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Instant;

use bytes::BytesMut;
use quinn_proto::{
    Connection, ConnectionHandle, DatagramEvent, EcnCodepoint, Endpoint, EndpointConfig, Event,
    ServerConfig, Transmit, TransportConfig,
};

/// SHA-256 fingerprint of the self-signed certificate.
///
/// Printed by the xdaemon at startup (`CERT_HASH_SHA256=<hex>`) so the E2E
/// test and the browser client can pin the certificate via `certificateHashes`.
pub struct CertInfo {
    pub sha256_hex: String,
}

pub struct QuicServer {
    /// The quinn-proto endpoint state machine.
    pub(crate) endpoint: Endpoint,
    /// Active connections keyed by their handle.
    pub connections: HashMap<ConnectionHandle, Connection>,
    /// Certificate fingerprint for browser pinning.
    pub(crate) cert_info: CertInfo,
}

impl QuicServer {
    /// Construct a new `QuicServer` with a freshly-generated self-signed cert.
    ///
    /// Returns `Err` if TLS configuration fails (cert generation, key parsing,
    /// cipher-suite negotiation, etc.).
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // --- 1. Generate self-signed cert ---
        // SANs include both "localhost" (DNS) and "127.0.0.1" (IP) so that
        // Chromium's cert-hash pinning works when the E2E forwarder is bound to
        // 127.0.0.1 (Task 9).
        //
        // W3C WebTransport spec requires serverCertificateHashes certs to have a
        // validity period of at most 14 days. `generate_simple_self_signed` uses
        // the rcgen default (~1 year), which Chrome rejects with
        // CERTIFICATE_VERIFY_FAILED.
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?;
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::days(13);
        let key_pair = rcgen::KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;
        let cert_der = cert.der().to_vec();
        let key_der = key_pair.serialize_der();

        // --- 2. SHA-256 fingerprint for browser certificateHashes ---
        let sha256_hex = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&cert_der))
        };

        // --- 3. Build rustls ServerConfig ---
        // rustls 0.23 with default-features=false: supply the crypto provider
        // explicitly; do NOT call install_default().
        let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>> =
            vec![rustls::pki_types::CertificateDer::from(cert_der)];
        let private_key = rustls::pki_types::PrivateKeyDer::try_from(key_der)
            .map_err(|e| format!("invalid private key: {e}"))?;

        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let mut tls_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(cert_chain, private_key)?;
        // HTTP/3 ALPN token required by WebTransport.
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        // --- 4. Wrap in quinn-proto's rustls adapter ---
        let quic_tls = quinn_proto::crypto::rustls::QuicServerConfig::try_from(tls_config)?;

        // --- 5. Transport config: enable datagrams ---
        let mut transport_config = TransportConfig::default();
        // `datagram_receive_buffer_size` takes `Option<usize>`.
        transport_config.datagram_receive_buffer_size(Some(65536));
        // `datagram_send_buffer_size` takes `usize` (not Option) in quinn-proto 0.11.
        //
        // Sized for the worst-case first-frame cdf53 burst: 2040 dirty tiles ×
        // 14 progressive passes × ~500 B mean payload ≈ 14 MB. The previous
        // 2 MB cap was set for a raw-codec frame and pre-dated the cdf53
        // refinement queue, so cdf53 emissions saturated the buffer at
        // session start and the tail of the burst was rejected (visible as
        // a ~70 % drop in tiles reaching the client even after we flipped
        // quinn-proto's silent-drop-oldest behaviour off in `send_datagram`).
        //
        // 16 MB worth of pending datagrams is well below any realistic
        // memory ceiling, the cost is only paid when emission outruns
        // drain, and it buys us enough headroom to absorb the entire first
        // frame's cdf53 burst until cwnd ramps. A proper rate-paced
        // scheduler is the longer-term fix — this just gets cdf53
        // first-frame quality usable in the meantime.
        transport_config.datagram_send_buffer_size(16 * 1024 * 1024);

        // --- 6. ServerConfig ---
        let mut server_config = ServerConfig::with_crypto(Arc::new(quic_tls));
        server_config.transport = Arc::new(transport_config);

        // --- 7. Endpoint ---
        let endpoint = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            Some(Arc::new(server_config)),
            /* allow_mtud */ true,
            /* rng_seed */ None,
        );

        Ok(Self {
            endpoint,
            connections: HashMap::new(),
            cert_info: CertInfo { sha256_hex },
        })
    }

    /// Return the certificate fingerprint for browser pinning.
    pub fn cert_info(&self) -> &CertInfo {
        &self.cert_info
    }

    // -------------------------------------------------------------------------
    // Method surface consumed by the I/O bridge.
    // -------------------------------------------------------------------------

    /// Feed an inbound UDP datagram into the endpoint state machine.
    ///
    /// Handles all three `DatagramEvent` variants internally:
    /// - `NewConnection(Incoming)` — auto-accepted; new `Connection` inserted
    ///   into `self.connections`.
    /// - `ConnectionEvent(handle, event)` — dispatched to the right connection.
    /// - `Response(Transmit)` — returned to the caller so the I/O bridge can
    ///   write the response bytes (already in `buf`) back to ghostbridge.
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
        buf: &mut Vec<u8>,
    ) -> Option<Transmit> {
        match self.endpoint.handle(now, remote, local_ip, ecn, data, buf) {
            None => None,

            Some(DatagramEvent::NewConnection(incoming)) => {
                match self.endpoint.accept(incoming, now, buf, None) {
                    Ok((handle, conn)) => {
                        // INFO (not debug) so connection attempts are visible at
                        // the default tracing filter — without this, a failed
                        // browser handshake (cert SAN mismatch, ALPN denial,
                        // etc.) is silent from the operator's perspective.
                        tracing::info!(?handle, %remote, "new QUIC connection accepted");
                        self.connections.insert(handle, conn);
                        None
                    }
                    Err(err) => {
                        tracing::warn!(%remote, cause=%err.cause, "connection accept failed");
                        err.response
                    }
                }
            }

            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(conn) = self.connections.get_mut(&handle) {
                    conn.handle_event(event);
                } else {
                    tracing::warn!(?handle, "ConnectionEvent for unknown connection");
                }
                None
            }

            Some(DatagramEvent::Response(transmit)) => Some(transmit),
        }
    }

    /// Drain one pending outbound transmit from any connection.
    ///
    /// The caller is responsible for clearing `buf` between successive calls so
    /// each call writes a fresh datagram into the shared scratch buffer.
    ///
    /// Returns `None` when all connections have been drained.
    pub fn poll_transmit(
        &mut self,
        now: Instant,
        max_datagrams: usize,
        buf: &mut Vec<u8>,
    ) -> Option<Transmit> {
        for conn in self.connections.values_mut() {
            if let Some(t) = conn.poll_transmit(now, max_datagrams, buf) {
                return Some(t);
            }
        }
        None
    }

    /// Earliest deadline across all connections, for driving the tokio timer.
    pub fn next_timeout(&mut self) -> Option<Instant> {
        self.connections
            .values_mut()
            .filter_map(|conn| conn.poll_timeout())
            .min()
    }

    /// Fire all connection timeouts whose deadline has passed, then prune
    /// connections that have fully drained.
    pub fn handle_timeout(&mut self, now: Instant) {
        for conn in self.connections.values_mut() {
            if conn.poll_timeout().is_some_and(|t| t <= now) {
                conn.handle_timeout(now);
            }
        }
        // Prune drained connections.
        self.connections.retain(|handle, conn| {
            if conn.is_drained() {
                tracing::debug!(?handle, "connection drained, removing");
                false
            } else {
                true
            }
        });
        // Drain endpoint events produced by handle_timeout on connections.
        self.drain_endpoint_events();
    }

    /// Drain one application-level event from any connection.
    ///
    /// Also removes connections that emit `Event::ConnectionLost`.
    pub fn poll_events(&mut self) -> Option<(ConnectionHandle, Event)> {
        let handles: Vec<ConnectionHandle> = self.connections.keys().copied().collect();
        for handle in handles {
            if let Some(conn) = self.connections.get_mut(&handle) {
                if let Some(event) = conn.poll() {
                    tracing::trace!(?handle, ?event, "poll_events: event found");
                    let lost = matches!(event, Event::ConnectionLost { .. });
                    if lost {
                        self.connections.remove(&handle);
                    }
                    return Some((handle, event));
                }
            }
        }
        None
    }

    /// Drain `EndpointEvent`s from every connection and feed them back into the
    /// endpoint.  Any resulting `ConnectionEvent`s are fed back to the
    /// corresponding connection.
    pub fn drain_endpoint_events(&mut self) {
        let handles: Vec<ConnectionHandle> = self.connections.keys().copied().collect();
        for handle in handles {
            while let Some(ep_event) = self
                .connections
                .get_mut(&handle)
                .and_then(|conn| conn.poll_endpoint_events())
            {
                if let Some(conn_event) = self.endpoint.handle_event(handle, ep_event) {
                    if let Some(conn) = self.connections.get_mut(&handle) {
                        conn.handle_event(conn_event);
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn can_create_endpoint() {
        let server = QuicServer::new().expect("endpoint creation should succeed");
        // SHA-256 hex string is always exactly 64 lowercase hex characters.
        let hash = &server.cert_info().sha256_hex;
        assert_eq!(hash.len(), 64);
        assert!(
            hash.chars().all(|c| c.is_ascii_hexdigit()),
            "cert hash should be pure hex, got: {hash}"
        );
    }

    #[test]
    fn dump_cert_for_inspection() {
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).unwrap();
        let now = time::OffsetDateTime::now_utc();
        params.not_before = now;
        params.not_after = now + time::Duration::days(13);
        let key_pair = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key_pair).unwrap();
        std::fs::write("/tmp/test_cert.der", cert.der()).unwrap();
        println!("Cert written to /tmp/test_cert.der");
    }
}
