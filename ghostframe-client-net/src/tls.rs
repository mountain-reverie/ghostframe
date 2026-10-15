//! Certificate pinning, mirroring the browser's `serverCertificateHashes`.
//!
//! **What is checked:** the end-entity certificate's DER must hash to exactly
//! the pinned SHA-256; the certificate's `notAfter` must not be in the past
//! (see [`NOT_AFTER_SKEW`]); and the TLS 1.2/1.3 handshake signatures are
//! verified for real, by delegating to the aws-lc-rs provider. That last part
//! is the security-meaningful difference from an "accept any certificate"
//! verifier, which asserts signature validity without checking it.
//!
//! **What is deliberately not checked:** the certificate chain, the server
//! name, and `notBefore`. Under pinning the certificate *is* the identity, so
//! a CA chain and an SNI match add nothing — this is the same model the
//! browser uses for `serverCertificateHashes`. `notBefore` is skipped because
//! it protects nothing here: the hash already fixes the exact certificate, so
//! "not yet valid" only ever means the client's clock is behind, whereas an
//! expired certificate means the pinned key's intended lifetime is over (a
//! leaked key must not be usable forever by a client that never restarts).
//!
//! **Expired pinned certificate:** refused with a TLS error, matching Chrome
//! (which also caps pinned certs at 14 days; the server issues 13-day certs,
//! `ghostframe-lib/src/transport/quic.rs`). A DER that matches the pin but
//! does not parse as X.509 is refused too — fail closed.
//!
//! The key-exchange provider is [`pq_provider`]: aws-lc-rs with the hybrid
//! X25519MLKEM768 group preferred.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// How far past `notAfter` the client's clock may be before the pinned
/// certificate is refused. Absorbs ordinary client clock drift (a fast clock
/// would otherwise reject a live certificate in its last minutes) without
/// meaningfully extending the 13-day lifetime.
pub const NOT_AFTER_SKEW: Duration = Duration::from_secs(5 * 60);

/// The TLS provider for the native client: aws-lc-rs with the hybrid
/// post-quantum group X25519MLKEM768 first, then the classical groups.
///
/// Same list as `ghostframe_lib::transport::quic::pq_provider` (the client does
/// not depend on the server crate). The client's order decides what is
/// negotiated: rustls servers pick the first group in the *client's* list that
/// they support, so this order, not the server's, is what makes the hybrid win.
/// Explicit, not the `prefer-post-quantum` cargo feature, so feature
/// unification cannot demote it.
///
/// On wasm32 (compile-only here) aws-lc-rs cannot be built, so this is ring:
/// no ML-KEM.
#[cfg(not(all(target_family = "wasm", target_os = "unknown")))]
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

/// wasm32 build: ring, classical groups only. See the native variant.
#[cfg(all(target_family = "wasm", target_os = "unknown"))]
pub fn pq_provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::ring::default_provider()
}

#[derive(Debug)]
pub struct PinnedCertVerifier {
    /// Any one of these hashes is accepted: the server advertises its
    /// current and next certificate, so a client that fetched just before a
    /// rotation still connects.
    expected: Vec<[u8; 32]>,
    provider: rustls::crypto::CryptoProvider,
}

impl PinnedCertVerifier {
    pub fn new(expected: [u8; 32]) -> Self {
        Self::with_hashes(vec![expected])
    }

    /// Accept a certificate matching any of `expected`.
    pub fn with_hashes(expected: Vec<[u8; 32]>) -> Self {
        Self {
            expected,
            provider: pq_provider(),
        }
    }
}

impl ServerCertVerifier for PinnedCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let actual: [u8; 32] = hasher.finalize().into();
        if !self.expected.contains(&actual) {
            return Err(TlsError::General("server certificate hash mismatch".into()));
        }
        let (_, cert) = x509_parser::parse_x509_certificate(end_entity.as_ref()).map_err(|e| {
            TlsError::General(format!("pinned certificate is not valid X.509: {e}"))
        })?;
        let not_after = cert.validity().not_after.timestamp();
        let limit = not_after.saturating_add(NOT_AFTER_SKEW.as_secs() as i64);
        if i64::try_from(now.as_secs()).unwrap_or(i64::MAX) > limit {
            return Err(TlsError::InvalidCertificate(
                rustls::CertificateError::ExpiredContext {
                    time: now,
                    not_after: UnixTime::since_unix_epoch(Duration::from_secs(
                        u64::try_from(not_after).unwrap_or(0),
                    )),
                },
            ));
        }
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}
