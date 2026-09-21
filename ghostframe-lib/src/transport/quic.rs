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

/// Bytes of un-ACKed datagrams quinn will hold before `send_datagram`
/// returns `Blocked`.
///
/// # Why not the 16 MB this used to be
///
/// A send buffer is a queue, and a queue on a slow link is latency. 16 MB
/// drained at the ~1 MB/s a real session gets is up to **16 seconds** of
/// datagrams committed before any backpressure appears. Measured at
/// production scale over 291 scheduler ticks: 6.71 MB peak buffered, i.e.
/// ~6.7 s of queueing before a byte reached the wire. Production reported
/// `queued_critical_latency_max_us=16,844,729` -- 16.8 s -- which is the
/// same effect on a fuller buffer and a slower link.
///
/// A refinement pass that arrives 6 seconds late is usually worthless: the
/// tile may have been superseded twice over, and the client has spent that
/// time NACKing for it.
///
/// # Why 16 MB was chosen, and why shrinking is safe now
///
/// It was a deliberate stopgap. A 2 MB cap predated the cdf53 refinement
/// queue, so a first-frame burst (2040 tiles x ~14 passes) saturated the
/// buffer and quinn *rejected the tail*, losing ~70% of tiles; the comment
/// that set 16 MB called a rate-paced scheduler the real fix.
///
/// That fix has since landed. Every drain is now clamped to
/// `QUINN_SEND_BUFFER_SAFETY_FRACTION * send_buffer_space()`
/// (`io_bridge.rs`), so the scheduler cannot over-pop into a full buffer and
/// cannot lose already-popped work to a rejection. With the clamp in place a
/// smaller buffer does not drop tiles -- it leaves them queued one layer up,
/// in the scheduler.
///
/// That is the right layer. The scheduler can supersede a stale generation,
/// reprioritise by pass, and drop work that no longer matters. quinn's
/// datagram queue is an opaque FIFO that can do none of those: a superseded
/// pass sitting in it is still transmitted, still ACKed, and still counted.
///
/// # Why the default is still 16 MB
///
/// Shrinking it to 256 KiB was tried and **measured**, and it does fix the
/// bufferbloat: peak buffered fell from 6.71 MB (~6.7 s) to 256 KB (~0.25 s).
/// It also broke delivery outright, at production scale with 1% loss:
///
/// | | 256 KiB | 16 MiB |
/// |---|---|---|
/// | emitted_cdf53 | 4,506 | 40,800 |
/// | send_datagram_errs_total | **15,944** | 0 |
/// | rto_fired | 14,301 | - |
/// | client coverage | complete=0, gave_up=2034 | complete=2040 |
///
/// The scheduler's `0.8 * send_buffer_space()` clamp covers scheduler
/// drains, but **not the retransmit path**: `RTO_RETRANSMITS_PER_TICK` is
/// documented as "sized to comfortably fit in quinn's datagram_send_buffer",
/// and that sizing assumed 16 MB. Against a small buffer the RTO sweep
/// overruns it, `Blocked` discards work the emitter already popped, the
/// missing passes provoke more RTO, and the buffer stays pinned full --
/// observed at `quinn_send_buffer_space=61` with `drained_count=0` and
/// 15,894 passes queued behind it.
///
/// So 16 MB is not a considered size, it is a workaround masking an
/// unclamped retransmit path. Shrinking it is the right direction and wants
/// that path clamped first; until then the large buffer is the safer
/// behaviour, and this stays a knob rather than a regression.
///
/// `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES` overrides it, so the experiment
/// above can be repeated without a rebuild.
fn datagram_send_buffer_bytes() -> usize {
    const DEFAULT: usize = 16 * 1024 * 1024;
    std::env::var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(DEFAULT)
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
        transport_config.datagram_send_buffer_size(datagram_send_buffer_bytes());

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

#[cfg(test)]
mod datagram_buffer_tests {
    use super::datagram_send_buffer_bytes;

    /// Serialises the three tests below, which mutate one process-global env
    /// var. Local rather than shared because no other test reads
    /// `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES`; a test that starts to must
    /// take this lock too, or the two will race under parallel `cargo test`.
    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn defaults_to_16_mib() {
        let _g = lock_env();
        std::env::remove_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES");
        assert_eq!(datagram_send_buffer_bytes(), 16 * 1024 * 1024);
    }

    #[test]
    fn env_override_is_honoured() {
        let _g = lock_env();
        std::env::set_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES", "1048576");
        assert_eq!(datagram_send_buffer_bytes(), 1024 * 1024);
        std::env::remove_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES");
    }

    #[test]
    fn nonsense_values_fall_back_rather_than_producing_a_zero_buffer() {
        // A zero-byte send buffer rejects every datagram, which would look
        // like total packet loss rather than a bad config value.
        let _g = lock_env();
        for bad in ["0", "-1", "lots", ""] {
            std::env::set_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES", bad);
            assert_eq!(
                datagram_send_buffer_bytes(),
                16 * 1024 * 1024,
                "{bad:?} should fall back to the default"
            );
        }
        std::env::remove_var("GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES");
    }
}
