use ghostframe_client_net::tls::PinnedCertVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use sha2::{Digest, Sha256};

fn self_signed() -> Vec<u8> {
    let params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let key = rcgen::KeyPair::generate().unwrap();
    params.self_signed(&key).unwrap().der().to_vec()
}

#[test]
fn accepts_the_pinned_cert_and_rejects_others() {
    let der = self_signed();
    let mut hasher = Sha256::new();
    hasher.update(&der);
    let hash: [u8; 32] = hasher.finalize().into();

    let verifier = PinnedCertVerifier::new(hash);
    let cert = CertificateDer::from(der.clone());
    let name = ServerName::try_from("localhost").unwrap();
    assert!(verifier
        .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
        .is_ok());

    let other = CertificateDer::from(self_signed());
    assert!(verifier
        .verify_server_cert(&other, &[], &name, &[], UnixTime::now())
        .is_err());
}

#[test]
fn rejects_empty_der() {
    // Edge case: empty DER payload should not match any reasonable expected hash.
    // SHA256(empty) = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    let empty_der_hash: [u8; 32] = sha2::Sha256::digest(b"").as_slice().try_into().unwrap();

    // An unrelated expected hash should reject empty DER
    let other_hash = [0x11u8; 32];
    let verifier = PinnedCertVerifier::new(other_hash);
    let cert = CertificateDer::from(Vec::<u8>::new());
    let name = ServerName::try_from("localhost").unwrap();

    assert!(verifier
        .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
        .is_err());

    // A pin that matches the empty DER is still refused: it is not X.509,
    // and the verifier now has to read `notAfter` out of it. Fail closed.
    let verifier_empty = PinnedCertVerifier::new(empty_der_hash);
    assert!(verifier_empty
        .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
        .is_err());
}

#[test]
fn rejects_garbage_der() {
    // Garbage DER should not match an all-zeros expected hash.
    // (all-zeros is what default ClientNetConfig would pass if uninitialized)
    let garbage = vec![0xFFu8; 256];
    let mut hasher = Sha256::new();
    hasher.update(&garbage);
    let garbage_hash: [u8; 32] = hasher.finalize().into();

    let all_zeros_hash = [0u8; 32];
    let verifier = PinnedCertVerifier::new(all_zeros_hash);
    let cert = CertificateDer::from(garbage);
    let name = ServerName::try_from("localhost").unwrap();

    assert!(verifier
        .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
        .is_err());

    // Even with the matching pin, bytes that are not X.509 are refused.
    let verifier_garbage = PinnedCertVerifier::new(garbage_hash);
    let cert_garbage = CertificateDer::from(vec![0xFFu8; 256]);
    assert!(verifier_garbage
        .verify_server_cert(&cert_garbage, &[], &name, &[], UnixTime::now())
        .is_err());
}

/// A self-signed cert whose validity ends `days_left` days from now
/// (negative = already expired), plus its pin.
fn cert_expiring_in(days_left: i64) -> (Vec<u8>, [u8; 32]) {
    let mut params = rcgen::CertificateParams::new(vec!["localhost".into()]).unwrap();
    let now = time::OffsetDateTime::now_utc();
    params.not_before = now - time::Duration::days(20);
    params.not_after = now + time::Duration::days(days_left);
    let key = rcgen::KeyPair::generate().unwrap();
    let der = params.self_signed(&key).unwrap().der().to_vec();
    let hash: [u8; 32] = Sha256::digest(&der).into();
    (der, hash)
}

fn verify(der: &[u8], hash: [u8; 32], now: UnixTime) -> Result<(), rustls::Error> {
    let name = ServerName::try_from("localhost").unwrap();
    PinnedCertVerifier::new(hash)
        .verify_server_cert(&CertificateDer::from(der.to_vec()), &[], &name, &[], now)
        .map(|_| ())
}

#[test]
fn refuses_an_expired_pinned_cert_even_with_a_matching_hash() {
    let (der, hash) = cert_expiring_in(-1);
    let err = verify(&der, hash, UnixTime::now()).unwrap_err();
    assert!(
        matches!(
            err,
            rustls::Error::InvalidCertificate(rustls::CertificateError::ExpiredContext { .. })
        ),
        "expected an expiry error, got {err:?}"
    );
}

#[test]
fn accepts_a_pinned_cert_inside_its_validity_window() {
    let (der, hash) = cert_expiring_in(13);
    assert!(verify(&der, hash, UnixTime::now()).is_ok());
}

#[test]
fn tolerates_clock_skew_up_to_the_documented_bound_and_no_further() {
    use ghostframe_client_net::tls::NOT_AFTER_SKEW;
    let (der, hash) = cert_expiring_in(1);
    let not_after = {
        let (_, c) = x509_parser::parse_x509_certificate(&der).unwrap();
        c.validity().not_after.timestamp() as u64
    };
    let at = |secs: u64| UnixTime::since_unix_epoch(std::time::Duration::from_secs(secs));
    // Just inside the skew window: still accepted.
    assert!(verify(&der, hash, at(not_after + NOT_AFTER_SKEW.as_secs() - 1)).is_ok());
    // Just outside: refused.
    assert!(verify(&der, hash, at(not_after + NOT_AFTER_SKEW.as_secs() + 1)).is_err());
}

#[test]
fn a_hash_mismatch_is_still_refused_when_the_cert_is_valid() {
    let (der, _) = cert_expiring_in(13);
    assert!(verify(&der, [0u8; 32], UnixTime::now()).is_err());
}

#[test]
fn provider_prefers_the_hybrid_post_quantum_group() {
    use rustls::NamedGroup;
    let p = ghostframe_client_net::tls::pq_provider();
    assert_eq!(p.kx_groups[0].name(), NamedGroup::X25519MLKEM768);
    // A classical fallback must remain for peers without the hybrid.
    assert!(p.kx_groups.iter().any(|g| g.name() == NamedGroup::X25519));
}
