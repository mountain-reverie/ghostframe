//! Generates a self-signed TLS cert + key for the e2e harness to inject
//! into ghostbridge via GHOSTFRAME_WEB_TLS_{CERT,KEY}_PEM env vars.
//! Also derives the cert's SPKI fingerprint so Chrome can be launched
//! with `--ignore-certificate-errors-spki-list=<spki>` — surgical
//! per-cert trust rather than a blanket disable.

use base64::Engine;
use rcgen::PublicKeyData;
use sha2::{Digest, Sha256};

pub struct E2eCert {
    pub cert_pem: String,
    pub key_pem: String,
    /// Padded standard base64 of SHA-256(SubjectPublicKeyInfo), formatted
    /// as Chrome's `--ignore-certificate-errors-spki-list` parser expects:
    /// `cert_verify_proc.cc → HashValue::FromString` requires exactly
    /// 44 chars for a SHA-256 hash. Unpadded (43 chars) silently fails
    /// the registration and every TLS handshake gets rejected with
    /// "remote error: tls: unknown certificate".
    pub spki_b64: String,
}

pub fn generate(sans: &[&str]) -> anyhow::Result<E2eCert> {
    let mut params =
        rcgen::CertificateParams::new(sans.iter().map(|s| s.to_string()).collect::<Vec<_>>())?;
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now;
    params.not_after = now + time::Duration::days(13);
    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let cert_pem = cert.pem();
    let key_pem = key_pair.serialize_pem();

    // SPKI fingerprint: hash the SubjectPublicKeyInfo DER, not the whole
    // cert. rcgen's PublicKeyData trait provides subject_public_key_info()
    // which returns the SPKI DER bytes (algorithm + bit-string). Padded
    // standard base64 — see field doc for why.
    let spki_der = key_pair.subject_public_key_info();
    let spki_b64 = base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&spki_der));

    Ok(E2eCert {
        cert_pem,
        key_pem,
        spki_b64,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_cert_with_valid_pem_and_32byte_spki() {
        let c = generate(&["localhost", "127.0.0.1"]).expect("generate");
        assert!(c.cert_pem.starts_with("-----BEGIN CERTIFICATE-----"));
        assert!(c.key_pem.contains("PRIVATE KEY"));
        // 44 chars = padded base64 of a 32-byte SHA-256 hash; Chrome's
        // --ignore-certificate-errors-spki-list parser checks this length.
        assert_eq!(c.spki_b64.len(), 44, "expected padded base64 (44 chars)");
        let spki = base64::engine::general_purpose::STANDARD
            .decode(&c.spki_b64)
            .expect("base64");
        assert_eq!(spki.len(), 32, "SHA-256 is 32 bytes");
    }
}
