//! Quinn-proto QUIC endpoint wrapper.
//!
//! Task 4: constructs a `quinn_proto::Endpoint` configured for WebTransport
//! (HTTP/3 ALPN, datagrams enabled), generates a self-signed cert with
//! `localhost` and `127.0.0.1` SANs, and exports the SHA-256 of the cert DER
//! for browser cert-hash pinning.  Method bodies are stubs filled by Task 5.

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
    /// The quinn-proto endpoint state machine.  Consumed by Task 5 (I/O bridge).
    #[allow(dead_code)]
    pub(crate) endpoint: Endpoint,
    /// Active connections keyed by their handle.  Populated by Task 5.
    #[allow(dead_code)]
    pub(crate) connections: HashMap<ConnectionHandle, Connection>,
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
        let cert =
            rcgen::generate_simple_self_signed(vec!["localhost".into(), "127.0.0.1".into()])?;
        // rcgen 0.14.x: DER bytes live on `cert.cert`; key is `signing_key`.
        let cert_der = cert.cert.der().to_vec();
        let key_der = cert.signing_key.serialize_der();

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
        transport_config.datagram_send_buffer_size(65536);

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
    // Method surface consumed by the I/O bridge (Task 5).
    // Bodies are intentionally left as `unimplemented!` stubs so that Task 5
    // only needs to fill them in without changing signatures.
    // -------------------------------------------------------------------------

    /// Feed an inbound UDP datagram into the endpoint state machine.
    ///
    /// `local_ip` is the destination IP of the received packet (may be `None`
    /// if the OS didn't report it); `ecn` carries the ECN codepoint.
    /// In quinn-proto 0.11 `Endpoint::handle` takes `Option<IpAddr>` (not
    /// `SocketAddr`) for `local_ip`, and owns the `data: BytesMut`.
    pub fn handle_datagram(
        &mut self,
        now: Instant,
        remote: SocketAddr,
        local_ip: Option<IpAddr>,
        ecn: Option<EcnCodepoint>,
        data: BytesMut,
        buf: &mut Vec<u8>,
    ) -> Option<DatagramEvent> {
        let _ = (now, remote, local_ip, ecn, data, buf);
        unimplemented!("Task 5")
    }

    /// Drain one pending outbound transmit from a connection.
    ///
    /// In quinn-proto 0.11 `Connection::poll_transmit` takes
    /// `(now, max_datagrams: usize, buf: &mut Vec<u8>)`.
    pub fn poll_transmit(
        &mut self,
        now: Instant,
        max_datagrams: usize,
        buf: &mut Vec<u8>,
    ) -> Option<Transmit> {
        let _ = (now, max_datagrams, buf);
        unimplemented!("Task 5")
    }

    /// Earliest deadline across all connections, for driving the tokio timer.
    pub fn next_timeout(&self) -> Option<Instant> {
        unimplemented!("Task 5")
    }

    /// Fire all connection timeouts whose deadline has passed.
    pub fn handle_timeout(&mut self, now: Instant) {
        let _ = now;
        unimplemented!("Task 5")
    }

    /// Drain one application-level event (new connection, stream data, datagram
    /// received, connection closed, …).
    pub fn poll_events(&mut self) -> Option<(ConnectionHandle, Event)> {
        unimplemented!("Task 5")
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
}
