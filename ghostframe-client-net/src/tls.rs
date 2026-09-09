//! Certificate pinning, mirroring the browser's `serverCertificateHashes`.
//!
//! **What is checked:** the end-entity certificate's DER must hash to exactly
//! the pinned SHA-256, and the TLS 1.2/1.3 handshake signatures are verified
//! for real, by delegating to the ring provider. That second part is the
//! security-meaningful difference from an "accept any certificate" verifier,
//! which asserts signature validity without checking it.
//!
//! **What is deliberately not checked:** the certificate chain, the server
//! name, and the validity window. Under pinning the certificate *is* the
//! identity, so a CA chain and an SNI match add nothing — this is the same
//! model the browser uses for `serverCertificateHashes`.
//!
//! **One deviation from the browser, worth knowing before this crate becomes
//! the native client:** Chrome additionally requires the pinned certificate's
//! validity window to be under 14 days, and refuses an expired one. This
//! verifier ignores `now` entirely, so it would accept an expired certificate
//! the browser would reject. The server issues 13-day certs
//! (`ghostframe-lib/src/transport/quic.rs:55`), so the two agree in practice
//! today; they would diverge for a client left running across an expiry.

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct PinnedCertVerifier {
    expected: [u8; 32],
    provider: rustls::crypto::CryptoProvider,
}

impl PinnedCertVerifier {
    pub fn new(expected: [u8; 32]) -> Self {
        Self {
            expected,
            provider: rustls::crypto::ring::default_provider(),
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
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        let mut hasher = Sha256::new();
        hasher.update(end_entity.as_ref());
        let actual: [u8; 32] = hasher.finalize().into();
        if actual == self.expected {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General("server certificate hash mismatch".into()))
        }
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
