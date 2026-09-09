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

    // A verifier expecting SHA256(empty) should accept it; this is correct behavior.
    let verifier_empty = PinnedCertVerifier::new(empty_der_hash);
    assert!(verifier_empty
        .verify_server_cert(&cert, &[], &name, &[], UnixTime::now())
        .is_ok());
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

    // A verifier expecting the correct garbage hash should accept it
    let verifier_garbage = PinnedCertVerifier::new(garbage_hash);
    let cert_garbage = CertificateDer::from(vec![0xFFu8; 256]);
    assert!(verifier_garbage
        .verify_server_cert(&cert_garbage, &[], &name, &[], UnixTime::now())
        .is_ok());
}
