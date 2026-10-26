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
    Incoming, ServerConfig, Transmit, TransportConfig,
};

use crate::transport::peer_auth::{GateCheck, PeerGate, PeerIdentitySource, PeerLookup};

/// SHA-256 fingerprint of the self-signed certificate.
///
/// Printed by the xdaemon at startup (`CERT_HASH_SHA256=<hex>`) so the E2E
/// test and the browser client can pin the certificate via `certificateHashes`.
pub struct CertInfo {
    /// Hash of the certificate that was current when the server was built.
    /// A snapshot: the daemon rotates certificates in-process, so anything
    /// that outlives startup (publishing to clients) must read
    /// [`CertSchedule::hashes_hex`] from `schedule` instead.
    pub sha256_hex: String,
    /// The live certificate rotation state.
    pub schedule: Arc<CertSchedule>,
}

/// The TLS provider for every ghostframe QUIC endpoint: aws-lc-rs with the
/// hybrid post-quantum group X25519MLKEM768 **first** in the preference order,
/// then the classical groups so a peer that does not offer it (older
/// browsers) still connects.
///
/// The list is set explicitly rather than inherited from rustls's
/// `prefer-post-quantum` cargo feature. Note the server cannot *enforce* a
/// preference: rustls picks the first group in the **client's** list that the
/// server supports, so the hybrid is used exactly when the client offers it
/// first (Chrome >= 131, Firefox >= 132, and the native client do). What this
/// provider guarantees is that the server *can* do it. The `ring` provider has
/// no ML-KEM at all. Certificates stay ECDSA P-256 (browser pinning
/// requires it); this changes key exchange only.
///
/// `ghostframe-client-net` carries an identical copy (this crate is not a
/// dependency of the client); both have a test pinning the order.
pub fn pq_provider() -> rustls::crypto::CryptoProvider {
    use rustls::crypto::aws_lc_rs::{self, kx_group};
    let mut provider = aws_lc_rs::default_provider();
    provider.kx_groups = vec![
        kx_group::X25519MLKEM768,
        kx_group::X25519,
        kx_group::SECP256R1,
        kx_group::SECP384R1,
    ];
    provider
}

pub struct QuicServer {
    /// The quinn-proto endpoint state machine.
    pub(crate) endpoint: Endpoint,
    /// Active connections keyed by their handle.
    pub connections: HashMap<ConnectionHandle, Connection>,
    /// The datagram send buffer size this endpoint was configured with, so a
    /// reading of `send_buffer_space()` can be turned into queued bytes.
    datagram_send_buffer: usize,
    /// The TLS adapter the endpoint's `ServerConfig` was built from, kept so
    /// `use_model_controller` can build a second `ServerConfig` over it.
    quic_tls: Arc<quinn_proto::crypto::rustls::QuicServerConfig>,
    /// Certificate fingerprint for browser pinning.
    pub(crate) cert_info: CertInfo,
    /// Stray `ConnectionEvent`s for handles we no longer hold, counted rather
    /// than each logged. A client that goes away leaves packets in flight, and
    /// one departure produced 208 identical warnings in a single burst in the
    /// field. journald rate-limits a burst like that and drops a window of
    /// unrelated lines with it -- so the flood does not merely add noise, it
    /// destroys the log around the very event being diagnosed.
    unknown_conn_events: u64,
    /// Handles accepted since the bridge last asked. The bridge arms its
    /// never-HELLO deadline from this at accept time rather than on
    /// `Event::Connected`, so a half-open handshake is covered too.
    newly_accepted: Vec<ConnectionHandle>,
    /// Initials refused because the connection table was full.
    refused_connections: u64,
    /// Who may connect at all (see `transport::peer_auth`). Starts as
    /// [`PeerGate::deny_all`]: a server nobody has given an identity source
    /// admits no one.
    peer_gate: PeerGate,
    /// Initials held while their peer's identity is looked up, by peer IP.
    /// quinn-proto buffers any further packets for a held `Incoming` itself
    /// (bounded by `INCOMING_BUFFER_*` below).
    held_incoming: HashMap<IpAddr, Vec<Incoming>>,
    /// Initials dropped because too many were already held.
    held_overflow: u64,
    /// Initials refused because their source IP was at its connection cap.
    refused_per_source: u64,
}

/// Initials held across all peers while identity lookups are outstanding.
/// Past it a new Initial is dropped silently; the client retransmits it.
pub(crate) const MAX_HELD_INCOMING: usize = 32;
/// Initials held for one peer IP. A real client opens one connection.
pub(crate) const MAX_HELD_INCOMING_PER_PEER: usize = 4;
/// Bytes quinn-proto buffers for one held `Incoming`, and for all of them.
/// An Initial flight is a few KiB, even with a post-quantum key share.
/// quinn's defaults (10 MiB / 100 MiB) assume a server that holds Incomings
/// on purpose for a long time; this one holds them only for an identity
/// lookup, from peers it has not admitted yet. Nothing else uses these
/// buffers: before the gate, every Incoming was decided in the same call.
const INCOMING_BUFFER_PER_CONNECTION: u64 = 64 * 1024;
const INCOMING_BUFFER_TOTAL: u64 = 1024 * 1024;

/// Connections the table holds before new Initials are refused outright.
/// One client is live at a time (M4a); the slack covers a reconnect that
/// overlaps its predecessor's drain, and an operator probing.
pub(crate) const MAX_CONNECTIONS: usize = 16;
/// Live connections one source IP may hold. One client is live at a time
/// (M4a); the slack covers a reconnect overlapping its predecessor's drain
/// and a second tab. Without it one peer fills the whole table
/// ([`MAX_CONNECTIONS`]) and locks every other peer out. The count is derived
/// from the connection table, so a connection that closes (or is reaped)
/// frees its slot with no per-handle state to forget or alias when quinn
/// reuses the handle.
pub(crate) const MAX_CONNECTIONS_PER_SOURCE: usize = 4;

/// Past this many connections, an Initial must prove its source address with
/// a Retry round trip before a `Connection` is allocated for it. Below it the
/// handshake stays one round trip faster.
pub(crate) const RETRY_THRESHOLD: usize = 4;

/// What to do with a first-flight Initial.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Admission {
    Accept,
    Retry,
    Refuse,
}

/// Admission decision for a new Initial given the current table size.
pub(crate) fn admission(live: usize, validated: bool, may_retry: bool) -> Admission {
    if live >= MAX_CONNECTIONS {
        Admission::Refuse
    } else if live >= RETRY_THRESHOLD && !validated && may_retry {
        Admission::Retry
    } else {
        Admission::Accept
    }
}

/// Whether a source holding `live_from_source` connections may open another.
pub(crate) fn source_may_connect(live_from_source: usize) -> bool {
    live_from_source < MAX_CONNECTIONS_PER_SOURCE
}

/// Connections among `remotes` that come from `ip`.
pub(crate) fn count_from_source(remotes: impl Iterator<Item = SocketAddr>, ip: IpAddr) -> usize {
    remotes.filter(|a| a.ip() == ip).count()
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
/// # Since admission (D1 F1)
///
/// The clamp and `RTO_RETRANSMITS_PER_TICK` above are gone. Every datagram,
/// retransmissions and parity included, is billed to one bucket and admitted
/// only while quinn's queue holds less than about 10 ms of the sending rate
/// (`transport::admission`), so the queue stays far below this size and the
/// unclamped retransmit path this paragraph blamed no longer exists. The
/// size is now only an overflow guard; shrinking it to a few MB is the next
/// step (D1 Q3), and a `Blocked` is a bug signal.
///
/// `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES` overrides it, so the experiment
/// above can be repeated without a rebuild.
fn datagram_send_buffer_bytes() -> usize {
    // Read in `config`, never here: see that module's header on why a
    // call-site `env::var` races other tests in the same process.
    crate::config::datagram_send_buffer_bytes()
}

// -----------------------------------------------------------------------------
// Certificate lifetime and rotation.
// -----------------------------------------------------------------------------
//
// Browsers accept a `serverCertificateHashes` certificate only if it is valid
// for at most 14 days. A cert generated once at startup therefore guarantees
// an outage on day 13 of any daemon that stays up, and a client holding the
// hash it fetched before that edge cannot connect after it.
//
// The schedule keeps two certificates: `current` (served) and `next` (already
// generated, advertised but not yet served). The hash list published to
// clients is always `[current, next]`, so a client that fetched just before a
// rotation still holds a pin that matches what the server serves after it
// (the libp2p pattern). Each cert is served for `CERT_SERVE_PERIOD` and is
// valid for `CERT_VALIDITY`, leaving it a margin before expiry.

/// Wall clock used for certificate validity and rotation. Injectable so tests
/// move time without sleeping.
pub type CertClock = Arc<dyn Fn() -> time::OffsetDateTime + Send + Sync>;

/// Maximum span the W3C spec allows for a pinned cert is 14 days; stay under.
pub(crate) const CERT_VALIDITY: time::Duration = time::Duration::days(13);
/// How far `notBefore` precedes the moment a cert starts being served, so a
/// client whose clock runs behind does not see a not-yet-valid cert.
pub(crate) const CERT_BACKDATE: time::Duration = time::Duration::days(1);
/// How long one certificate is served before the successor takes over.
pub(crate) const CERT_SERVE_PERIOD: time::Duration = time::Duration::days(7);

fn system_clock() -> CertClock {
    Arc::new(time::OffsetDateTime::now_utc)
}

struct CertSlot {
    certified: Arc<rustls::sign::CertifiedKey>,
    hex: String,
    serve_from: time::OffsetDateTime,
    not_before: time::OffsetDateTime,
    not_after: time::OffsetDateTime,
}

struct ScheduleState {
    current: CertSlot,
    next: CertSlot,
}

/// Two-cert rotation state; doubles as the rustls certificate resolver.
pub struct CertSchedule {
    clock: CertClock,
    /// Bumped on every rotation, so a publisher wakes at once instead of
    /// polling (a monotonic sleep does not advance across a suspend).
    rotated: tokio::sync::watch::Sender<u64>,
    state: std::sync::Mutex<ScheduleState>,
}

impl std::fmt::Debug for CertSchedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertSchedule").finish_non_exhaustive()
    }
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

impl CertSchedule {
    pub fn new(clock: CertClock) -> Result<Self, BoxErr> {
        let now = clock();
        let current = Self::make_slot(now)?;
        let next = Self::make_slot(now + CERT_SERVE_PERIOD)?;
        Ok(Self {
            clock,
            rotated: tokio::sync::watch::channel(0).0,
            state: std::sync::Mutex::new(ScheduleState { current, next }),
        })
    }

    /// Generate a cert that starts being served at `serve_from`.
    fn make_slot(serve_from: time::OffsetDateTime) -> Result<CertSlot, BoxErr> {
        // SANs include both "localhost" (DNS) and "127.0.0.1" (IP) so that
        // Chromium's cert-hash pinning works when the E2E forwarder is bound
        // to 127.0.0.1.
        let mut params =
            rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()])?;
        params.not_before = serve_from - CERT_BACKDATE;
        params.not_after = params.not_before + CERT_VALIDITY;
        let key_pair = rcgen::KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;
        let der = cert.der().to_vec();
        let hex = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&der))
        };
        let key = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
            .map_err(|e| format!("invalid private key: {e}"))?;
        let signing_key = pq_provider().key_provider.load_private_key(key)?;
        let certified = rustls::sign::CertifiedKey::new(
            vec![rustls::pki_types::CertificateDer::from(der)],
            signing_key,
        );
        Ok(CertSlot {
            certified: Arc::new(certified),
            hex,
            serve_from,
            not_before: params.not_before,
            not_after: params.not_after,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScheduleState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Roll forward to `now`. Cheap when nothing is due.
    fn advance_locked(&self, st: &mut ScheduleState) {
        let now = (self.clock)();
        let stepped_back = now < st.current.not_before;
        if now < st.next.serve_from && !stepped_back {
            return;
        }
        // Asleep for more than a whole period (suspend, clock step forward),
        // or the clock stepped back before the served cert's notBefore (it
        // would be rejected as not yet valid): start over from now.
        let rebuilt = if stepped_back || now >= st.next.serve_from + CERT_SERVE_PERIOD {
            Self::make_slot(now).and_then(|c| Ok((c, Self::make_slot(now + CERT_SERVE_PERIOD)?)))
        } else {
            Self::make_slot(st.next.serve_from + CERT_SERVE_PERIOD).map(|n| {
                (
                    CertSlot {
                        certified: st.next.certified.clone(),
                        hex: st.next.hex.clone(),
                        serve_from: st.next.serve_from,
                        not_before: st.next.not_before,
                        not_after: st.next.not_after,
                    },
                    n,
                )
            })
        };
        match rebuilt {
            Ok((current, next)) => {
                tracing::info!(
                    current = %current.hex,
                    next = %next.hex,
                    "TLS certificate rotated"
                );
                st.current = current;
                st.next = next;
                self.rotated.send_modify(|g| *g += 1);
            }
            // Keep serving what we have; the next handshake retries.
            Err(e) => tracing::error!(error = %e, "TLS certificate rotation failed"),
        }
    }

    /// Hash of the cert being served now.
    pub fn current_hex(&self) -> String {
        let mut st = self.lock();
        self.advance_locked(&mut st);
        st.current.hex.clone()
    }

    /// Hashes to publish to clients: `[current, next]`.
    pub fn hashes_hex(&self) -> Vec<String> {
        let mut st = self.lock();
        self.advance_locked(&mut st);
        vec![st.current.hex.clone(), st.next.hex.clone()]
    }

    /// Subscribe to rotation notifications.
    pub fn subscribe_rotations(&self) -> tokio::sync::watch::Receiver<u64> {
        self.rotated.subscribe()
    }

    /// DER of the leaf `resolve` would present right now.
    pub fn serving_cert_der(&self) -> Vec<u8> {
        let mut st = self.lock();
        self.advance_locked(&mut st);
        st.current.certified.cert[0].as_ref().to_vec()
    }

    /// `(notBefore, notAfter)` of the cert being served now.
    pub fn current_validity(&self) -> (time::OffsetDateTime, time::OffsetDateTime) {
        let mut st = self.lock();
        self.advance_locked(&mut st);
        (st.current.not_before, st.current.not_after)
    }

    /// Time until the next rotation is due, so a publisher can sleep to it.
    pub fn until_next_rotation(&self) -> std::time::Duration {
        let st = self.lock();
        (st.next.serve_from - (self.clock)())
            .try_into()
            .unwrap_or(std::time::Duration::ZERO)
    }
}

impl rustls::server::ResolvesServerCert for CertSchedule {
    fn resolve(
        &self,
        _client_hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let mut st = self.lock();
        self.advance_locked(&mut st);
        Some(st.current.certified.clone())
    }
}

impl QuicServer {
    /// Construct a new `QuicServer` with a freshly-generated self-signed cert.
    ///
    /// Returns `Err` if TLS configuration fails (cert generation, key parsing,
    /// cipher-suite negotiation, etc.).
    pub fn new() -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::new_with_datagram_send_buffer(datagram_send_buffer_bytes())
    }

    /// `new`, with an explicit datagram send-buffer size.
    ///
    /// Exists so a test can shrink the buffer without touching
    /// `GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES`: that is process-global, and
    /// browserless scenes run in-process and in parallel, so an env var
    /// would race between scenes rather than configure one.
    pub fn new_with_datagram_send_buffer(
        datagram_send_buffer: usize,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::build(datagram_send_buffer, None, None)
    }

    /// `new_with_datagram_send_buffer`, with the QUIC endpoint's RNG pinned.
    ///
    /// The endpoint RNG picks connection IDs, the stateless-reset key and
    /// the point at which a connection skips a packet number. Entropy-seeded,
    /// two same-seed browserless scenes differ in the server's wire image, so
    /// the harness pins it, as it already does for the client.
    /// Harness-only: production must never run with a predictable RNG.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_rng_seed(
        datagram_send_buffer: usize,
        rng_seed: [u8; 32],
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::build(datagram_send_buffer, Some(rng_seed), None)
    }

    /// `new`, with the certificate clock injected, so a test can move the
    /// rotation schedule without sleeping. Harmless in production: it only
    /// decides when this process rotates its own certificate.
    pub fn new_with_cert_clock(
        clock: CertClock,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        Self::build(datagram_send_buffer_bytes(), None, Some(clock))
    }

    fn build(
        datagram_send_buffer: usize,
        rng_seed: Option<[u8; 32]>,
        clock: Option<CertClock>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // --- 1. Self-signed certs, rotated in-process (see `CertSchedule`) ---
        // The schedule is also the rustls certificate resolver, so the cert is
        // chosen per handshake from the clock and never goes stale on a
        // long-running daemon.
        let schedule = Arc::new(CertSchedule::new(clock.unwrap_or_else(system_clock))?);
        let sha256_hex = schedule.current_hex();

        // --- 3. Build rustls ServerConfig ---
        // rustls 0.23 with default-features=false: supply the crypto provider
        // explicitly; do NOT call install_default(). The provider is aws-lc-rs
        // with the hybrid X25519MLKEM768 group preferred (see `pq_provider`).
        let provider = Arc::new(pq_provider());
        let mut tls_config = rustls::ServerConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_cert_resolver(schedule.clone());
        // HTTP/3 ALPN token required by WebTransport.
        tls_config.alpn_protocols = vec![b"h3".to_vec()];

        // --- 4. Wrap in quinn-proto's rustls adapter ---
        let quic_tls = Arc::new(quinn_proto::crypto::rustls::QuicServerConfig::try_from(tls_config)?);

        // --- 5/6. Transport config (datagrams) and ServerConfig ---
        let server_config = Self::server_config(quic_tls.clone(), datagram_send_buffer, None);

        // --- 7. Endpoint ---
        let endpoint = Endpoint::new(
            Arc::new(EndpointConfig::default()),
            Some(Arc::new(server_config)),
            /* allow_mtud */ true,
            rng_seed,
        );

        Ok(Self {
            endpoint,
            connections: HashMap::new(),
            datagram_send_buffer,
            quic_tls,
            cert_info: CertInfo {
                sha256_hex,
                schedule,
            },
            unknown_conn_events: 0,
            newly_accepted: Vec::new(),
            refused_connections: 0,
            peer_gate: PeerGate::deny_all(),
            held_incoming: HashMap::new(),
            held_overflow: 0,
            refused_per_source: 0,
        })
    }

    /// The `ServerConfig`: datagrams on, and quinn's default Cubic unless a
    /// model cell is given (D1 design A), in which case the window is the
    /// model controller's.
    fn server_config(
        quic_tls: Arc<quinn_proto::crypto::rustls::QuicServerConfig>,
        datagram_send_buffer: usize,
        model: Option<Arc<crate::transport::model_controller::ModelCell>>,
    ) -> ServerConfig {
        let mut transport_config = TransportConfig::default();
        // `datagram_receive_buffer_size` takes `Option<usize>`.
        transport_config.datagram_receive_buffer_size(Some(65536));
        // `datagram_send_buffer_size` takes `usize` (not Option) in quinn-proto 0.11.
        transport_config.datagram_send_buffer_size(datagram_send_buffer);
        if let Some(cell) = model {
            transport_config.congestion_controller_factory(Arc::new(
                crate::transport::model_controller::ModelControllerFactory::new(cell),
            ));
        }

        let mut server_config = ServerConfig::with_crypto(quic_tls);
        server_config.transport = Arc::new(transport_config);
        server_config
            .incoming_buffer_size(INCOMING_BUFFER_PER_CONNECTION)
            .incoming_buffer_size_total(INCOMING_BUFFER_TOTAL)
            // A connection is admitted for the peer at its source address
            // (see `transport::peer_auth`), so it stays on that address. A
            // tailnet peer's address does not change under a live session:
            // WireGuard absorbs the underlying network moving.
            .migration(false);
        server_config
    }

    /// Make connections accepted from now on use the model controller (D1
    /// design A) over `cell`. A connection already open keeps the controller
    /// it was built with, so call this before the first handshake.
    pub fn use_model_controller(
        &mut self,
        cell: Arc<crate::transport::model_controller::ModelCell>,
    ) {
        let config = Self::server_config(self.quic_tls.clone(), self.datagram_send_buffer, Some(cell));
        self.endpoint.set_server_config(Some(Arc::new(config)));
    }

    /// Install who may connect. Until this is called the server refuses
    /// every new connection. `IoBridge::new` installs the tsnet WhoIs gate.
    pub fn set_peer_gate(&mut self, gate: PeerGate) {
        self.peer_gate = gate;
    }

    pub fn peer_gate(&self) -> &PeerGate {
        &self.peer_gate
    }

    /// The gate, for re-checking connected peers through it.
    pub fn peer_gate_mut(&mut self) -> &mut PeerGate {
        &mut self.peer_gate
    }

    /// Identity lookups the caller should start, with the source to run them
    /// against. Each IP is returned once until [`Self::resolve_peer`] is
    /// called for it.
    pub fn take_peer_lookups(&mut self) -> Vec<(IpAddr, u64, Arc<dyn PeerIdentitySource>)> {
        let ips = self.peer_gate.take_stamped_requests();
        match self.peer_gate.source() {
            Some(src) => ips
                .into_iter()
                .map(|(ip, stamp)| (ip, stamp, src.clone()))
                .collect(),
            // Unreachable: the gate only defers when it has a source.
            None => Vec::new(),
        }
    }

    /// Initials currently held awaiting a lookup.
    pub fn held_incoming_count(&self) -> usize {
        self.held_incoming.values().map(Vec::len).sum()
    }

    /// Apply a finished identity lookup for `ip`: admit or refuse every
    /// Initial held for it. Returns the datagrams to send (refusals, and
    /// any immediate accept-failure response) as `(destination, bytes)`.
    pub fn resolve_peer(
        &mut self,
        ip: IpAddr,
        lookup: &PeerLookup,
        now: Instant,
    ) -> Vec<(SocketAddr, Vec<u8>)> {
        let allowed = self.peer_gate.resolve(ip, lookup, now);
        let mut out = Vec::new();
        for incoming in self.held_incoming.remove(&ip).unwrap_or_default() {
            let mut buf = Vec::new();
            let transmit = if allowed {
                self.admit(incoming, now, &mut buf)
            } else {
                Some(self.endpoint.refuse(incoming, &mut buf))
            };
            if let Some(t) = transmit {
                buf.truncate(t.size);
                out.push((t.destination, buf));
            }
        }
        out
    }

    /// Live connections currently held from `ip`.
    fn connections_from(&self, ip: IpAddr) -> usize {
        count_from_source(self.connections.values().map(|c| c.remote_address()), ip)
    }

    /// Refuse an Initial from a source that is at its connection cap.
    fn refuse_source_at_cap(&mut self, incoming: Incoming, buf: &mut Vec<u8>) -> Transmit {
        self.refused_per_source = self.refused_per_source.saturating_add(1);
        if self.refused_per_source == 1 || self.refused_per_source.is_multiple_of(64) {
            tracing::warn!(
                peer = %incoming.remote_address().ip(),
                limit = MAX_CONNECTIONS_PER_SOURCE,
                total = self.refused_per_source,
                "source at its connection cap; refusing (rate-limited)"
            );
        }
        self.endpoint.refuse(incoming, buf)
    }

    /// Initials refused because their source was at its connection cap.
    pub fn refused_per_source(&self) -> u64 {
        self.refused_per_source
    }

    /// Whether one more Initial from `ip` may be held for a lookup.
    fn can_hold(&self, ip: IpAddr) -> bool {
        let for_peer = self.held_incoming.get(&ip).map_or(0, Vec::len);
        self.held_incoming_count() < MAX_HELD_INCOMING && for_peer < MAX_HELD_INCOMING_PER_PEER
    }

    fn note_held_overflow(&mut self, ip: IpAddr) {
        self.held_overflow = self.held_overflow.saturating_add(1);
        if self.held_overflow == 1 || self.held_overflow.is_multiple_of(64) {
            tracing::warn!(
                peer = %ip,
                held = self.held_incoming_count(),
                total = self.held_overflow,
                "too many connections awaiting an identity lookup; dropping (rate-limited)"
            );
        }
    }

    /// Initials dropped because no more could be held for a lookup.
    pub fn held_overflow(&self) -> u64 {
        self.held_overflow
    }

    /// Hold an Initial until its peer's lookup resolves. The gate only says
    /// `Pending` when [`Self::can_hold`] was true; the check is repeated so
    /// the bound holds even if that ever changes.
    fn hold(&mut self, ip: IpAddr, incoming: Incoming) {
        if !self.can_hold(ip) {
            self.note_held_overflow(ip);
            self.endpoint.ignore(incoming);
            return;
        }
        self.held_incoming.entry(ip).or_default().push(incoming);
    }

    /// Table-size admission, then accept. The peer is already admitted.
    fn admit(&mut self, incoming: Incoming, now: Instant, buf: &mut Vec<u8>) -> Option<Transmit> {
        let remote = incoming.remote_address();
        // Authoritative per-source cap: Initials held for a lookup were
        // counted when they arrived, before their siblings were admitted.
        if !source_may_connect(self.connections_from(remote.ip())) {
            return Some(self.refuse_source_at_cap(incoming, buf));
        }
        // Admission control. A spoofed-source Initial flood otherwise
        // allocates a full `Connection` (TLS state, buffers) per
        // packet, and a second ghostframe client is served by
        // eviction, not by room for many.
        match admission(
            self.connections.len(),
            incoming.remote_address_validated(),
            incoming.may_retry(),
        ) {
            Admission::Refuse => {
                self.refused_connections = self.refused_connections.saturating_add(1);
                if self.refused_connections == 1 || self.refused_connections.is_multiple_of(64) {
                    tracing::warn!(
                        %remote,
                        live = self.connections.len(),
                        total = self.refused_connections,
                        "connection table full; refusing (rate-limited)"
                    );
                }
                return Some(self.endpoint.refuse(incoming, buf));
            }
            Admission::Retry => match self.endpoint.retry(incoming, buf) {
                Ok(transmit) => return Some(transmit),
                // `may_retry` was checked by `admission`; nothing to
                // do but drop the packet.
                Err(e) => {
                    self.endpoint.ignore(e.into_incoming());
                    return None;
                }
            },
            Admission::Accept => {}
        }
        match self.endpoint.accept(incoming, now, buf, None) {
            Ok((handle, conn)) => {
                // INFO (not debug) so connection attempts are visible at
                // the default tracing filter — without this, a failed
                // browser handshake (cert SAN mismatch, ALPN denial,
                // etc.) is silent from the operator's perspective.
                tracing::info!(?handle, %remote, "new QUIC connection accepted");
                self.connections.insert(handle, conn);
                self.newly_accepted.push(handle);
                None
            }
            Err(err) => {
                tracing::warn!(%remote, cause=%err.cause, "connection accept failed");
                err.response
            }
        }
    }

    /// The configured datagram send buffer size, bytes.
    pub fn datagram_send_buffer(&self) -> usize {
        self.datagram_send_buffer
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
    /// - `NewConnection(Incoming)` — checked against the peer gate first:
    ///   refused, held for an identity lookup (see [`Self::resolve_peer`]),
    ///   or admitted, in which case a new `Connection` is inserted into
    ///   `self.connections`.
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
                // Who may connect at all, before anything is allocated for
                // the connection and long before it could send HELLO. A
                // refused peer never gets a `Connection`.
                let ip = incoming.remote_address().ip();
                // Cheapest check first: a source already at its cap gets no
                // identity lookup and no held Initial either.
                if !source_may_connect(self.connections_from(ip)) {
                    return Some(self.refuse_source_at_cap(incoming, buf));
                }
                let can_hold = self.can_hold(ip);
                match self.peer_gate.check(ip, now, can_hold) {
                    GateCheck::Allow => self.admit(incoming, now, buf),
                    GateCheck::Deny => Some(self.endpoint.refuse(incoming, buf)),
                    GateCheck::Pending => {
                        self.hold(ip, incoming);
                        None
                    }
                    GateCheck::Busy => {
                        self.note_held_overflow(ip);
                        self.endpoint.ignore(incoming);
                        None
                    }
                }
            }

            Some(DatagramEvent::ConnectionEvent(handle, event)) => {
                if let Some(conn) = self.connections.get_mut(&handle) {
                    conn.handle_event(event);
                } else {
                    // Log the first, then every 64th. The fact worth knowing is
                    // "this is happening and roughly how much", which one line
                    // per 64 conveys as well as 208 lines and without taking
                    // the surrounding log down with it.
                    self.unknown_conn_events += 1;
                    if self.unknown_conn_events == 1 || self.unknown_conn_events.is_multiple_of(64)
                    {
                        tracing::warn!(
                            ?handle,
                            total = self.unknown_conn_events,
                            "ConnectionEvent for unknown connection \
                             (stale packets from a departed client; rate-limited)"
                        );
                    }
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

    /// Handles accepted since the last call (oldest first).
    pub fn take_accepted(&mut self) -> Vec<ConnectionHandle> {
        std::mem::take(&mut self.newly_accepted)
    }

    /// Fire all connection timeouts whose deadline has passed, then prune
    /// connections that have fully drained.
    ///
    /// Returns the handles that were pruned. A connection that idles out or
    /// finishes closing is reaped here and never surfaces as
    /// `Event::ConnectionLost`, so the caller must run its per-handle
    /// cleanup for each returned handle.
    pub fn handle_timeout(&mut self, now: Instant) -> Vec<ConnectionHandle> {
        for conn in self.connections.values_mut() {
            if conn.poll_timeout().is_some_and(|t| t <= now) {
                conn.handle_timeout(now);
            }
        }
        // Prune drained connections.
        let mut reaped = Vec::new();
        self.connections.retain(|handle, conn| {
            if conn.is_drained() {
                tracing::debug!(?handle, "connection drained, removing");
                reaped.push(*handle);
                false
            } else {
                true
            }
        });
        // Drain endpoint events produced by handle_timeout on connections.
        self.drain_endpoint_events();
        reaped
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

    /// A clock the test moves by hand.
    fn manual_clock() -> (CertClock, Arc<std::sync::atomic::AtomicI64>) {
        use std::sync::atomic::{AtomicI64, Ordering};
        let base = time::OffsetDateTime::now_utc().unix_timestamp();
        let off = Arc::new(AtomicI64::new(0));
        let o = off.clone();
        let clock: CertClock = Arc::new(move || {
            time::OffsetDateTime::from_unix_timestamp(base + o.load(Ordering::SeqCst)).unwrap()
        });
        (clock, off)
    }

    const HOUR: i64 = 3600;
    const DAY: i64 = 24 * HOUR;

    #[test]
    fn served_cert_is_valid_at_every_moment_of_a_long_run() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock.clone()).unwrap();
        // 60 days in 30-minute steps: far past the old single 13-day cert.
        for step in 0..(60 * 48) {
            off.store(step * 1800, Ordering::SeqCst);
            let now = clock();
            let (nb, na) = sched.current_validity();
            assert!(
                nb <= now && now <= na,
                "served cert not valid at +{step} half-hours"
            );
            assert!(
                na - nb <= time::Duration::days(14),
                "browser rejects > 14 days"
            );
        }
    }

    #[test]
    fn not_before_is_backdated_for_client_clock_skew() {
        let (clock, _off) = manual_clock();
        let sched = CertSchedule::new(clock.clone()).unwrap();
        let (nb, _) = sched.current_validity();
        assert!(
            clock() - nb >= time::Duration::hours(23),
            "notBefore not backdated"
        );
    }

    #[test]
    fn served_window_sits_inside_validity_with_margin_for_current_and_next() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock.clone()).unwrap();
        // Current cert: at the very end of its serve period it must still
        // have days left, and the total span must stay under Chrome's 14.
        off.store(7 * DAY - 1, Ordering::SeqCst);
        let (nb, na) = sched.current_validity();
        assert!(
            na - clock() >= time::Duration::days(4),
            "no margin left at end of serve"
        );
        assert!(na - nb <= time::Duration::days(13));
        // Next cert (just promoted): valid already, whole serve period ahead
        // inside its window with margin.
        off.store(7 * DAY, Ordering::SeqCst);
        let (nb, na) = sched.current_validity();
        assert!(nb <= clock(), "promoted cert not yet valid");
        assert!(na - (clock() + CERT_SERVE_PERIOD) >= time::Duration::days(4));
        assert!(na - nb <= time::Duration::days(13));
    }

    #[test]
    fn advertised_pair_always_contains_the_hash_served_after_rotation() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock).unwrap();
        // A client fetches at t0 and connects at t0 + gap, for gaps spanning
        // a rotation. Its pinned list must contain what is served then.
        let mut t0 = 0;
        while t0 < 30 * DAY {
            off.store(t0, Ordering::SeqCst);
            let fetched = sched.hashes_hex();
            assert_eq!(fetched.len(), 2);
            assert_ne!(fetched[0], fetched[1]);
            for gap in [0, HOUR, 3 * DAY, 6 * DAY] {
                off.store(t0 + gap, Ordering::SeqCst);
                let served = sched.current_hex();
                assert!(
                    fetched.contains(&served),
                    "client fetched at +{t0}s, connects at +{gap}s later: served {served} not in {fetched:?}"
                );
            }
            t0 += 11 * HOUR;
        }
    }

    #[test]
    fn rotation_happens_and_a_long_sleep_does_not_serve_expired() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock.clone()).unwrap();
        let first = sched.current_hex();
        off.store(8 * DAY, Ordering::SeqCst);
        assert_ne!(
            sched.current_hex(),
            first,
            "no rotation after one serve period"
        );
        // Suspend for a month: must come back with a valid cert.
        off.store(40 * DAY, Ordering::SeqCst);
        let (nb, na) = sched.current_validity();
        assert!(nb <= clock() && clock() <= na);
    }

    #[test]
    fn quic_server_exposes_live_hashes() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let server = QuicServer::new_with_cert_clock(clock).unwrap();
        let before = server.cert_info().schedule.hashes_hex();
        off.store(8 * DAY, Ordering::SeqCst);
        let after = server.cert_info().schedule.hashes_hex();
        assert_eq!(after[0], before[1], "next became current");
    }

    #[test]
    fn presented_leaf_follows_rotation_and_matches_published_hash() {
        use sha2::{Digest, Sha256};
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock).unwrap();
        for day in [0, 8, 16, 45] {
            off.store(day * DAY, Ordering::SeqCst);
            let der = sched.serving_cert_der();
            assert_eq!(hex::encode(Sha256::digest(&der)), sched.hashes_hex()[0]);
        }
    }

    #[test]
    fn backward_clock_step_does_not_leave_a_not_yet_valid_cert() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock.clone()).unwrap();
        off.store(9 * DAY, Ordering::SeqCst);
        sched.current_hex();
        off.store(2 * DAY, Ordering::SeqCst);
        let (nb, na) = sched.current_validity();
        assert!(nb <= clock() && clock() <= na);
    }

    #[test]
    fn rotation_notifies_subscribers() {
        use std::sync::atomic::Ordering;
        let (clock, off) = manual_clock();
        let sched = CertSchedule::new(clock).unwrap();
        let rx = sched.subscribe_rotations();
        assert_eq!(*rx.borrow(), 0);
        off.store(8 * DAY, Ordering::SeqCst);
        sched.current_hex();
        assert_eq!(*rx.borrow(), 1);
    }

    #[test]
    fn provider_prefers_the_hybrid_post_quantum_group() {
        let p = pq_provider();
        assert_eq!(p.kx_groups[0].name(), rustls::NamedGroup::X25519MLKEM768);
        assert!(p
            .kx_groups
            .iter()
            .any(|g| g.name() == rustls::NamedGroup::X25519));
    }

    #[test]
    fn admission_refuses_at_the_cap_whatever_the_source() {
        for validated in [false, true] {
            for may_retry in [false, true] {
                assert_eq!(
                    admission(MAX_CONNECTIONS, validated, may_retry),
                    Admission::Refuse
                );
                assert_eq!(
                    admission(MAX_CONNECTIONS + 1000, validated, may_retry),
                    Admission::Refuse
                );
            }
        }
    }

    #[test]
    fn admission_demands_address_validation_under_load_only() {
        // Idle table: no extra round trip.
        assert_eq!(admission(0, false, true), Admission::Accept);
        assert_eq!(
            admission(RETRY_THRESHOLD - 1, false, true),
            Admission::Accept
        );
        // Loaded: an unvalidated source must Retry first.
        assert_eq!(admission(RETRY_THRESHOLD, false, true), Admission::Retry);
        assert_eq!(
            admission(MAX_CONNECTIONS - 1, false, true),
            Admission::Retry
        );
        // Already validated (it came back with a token): accepted.
        assert_eq!(admission(RETRY_THRESHOLD, true, true), Admission::Accept);
        // Cannot be retried (already retried once): accepted, not looped.
        assert_eq!(admission(RETRY_THRESHOLD, false, false), Admission::Accept);
    }

    #[test]
    fn source_cap_counts_only_that_source_and_a_closed_connection_frees_its_slot() {
        let a: IpAddr = "100.64.0.1".parse().unwrap();
        let b: IpAddr = "100.64.0.2".parse().unwrap();
        let mut table: Vec<SocketAddr> = (0..MAX_CONNECTIONS_PER_SOURCE as u16)
            .map(|i| SocketAddr::new(a, 40000 + i))
            .collect();
        table.push(SocketAddr::new(b, 40000));
        let n = count_from_source(table.iter().copied(), a);
        assert_eq!(n, MAX_CONNECTIONS_PER_SOURCE);
        assert!(!source_may_connect(n), "at the cap");
        // Another source is unaffected by it.
        assert!(source_may_connect(count_from_source(
            table.iter().copied(),
            b
        )));
        // A connection leaving the table (closed or reaped) frees a slot.
        table.remove(0);
        assert!(source_may_connect(count_from_source(
            table.iter().copied(),
            a
        )));
    }

    #[test]
    fn source_cap_is_below_the_table_cap() {
        const { assert!(MAX_CONNECTIONS_PER_SOURCE < MAX_CONNECTIONS) };
        const { assert!(MAX_CONNECTIONS_PER_SOURCE >= 2) }; // reconnect overlap
    }

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
