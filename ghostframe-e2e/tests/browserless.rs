//! Browserless e2e: real IoBridge + netsim + headless client, no browser,
//! no containers, no GPU. See
//! docs/superpowers/specs/2026-09-07-headless-client-netsim-design.md

use ghostframe_lib::transport::io_bridge::IoBridge;
use ghostframe_lib::transport::quic::QuicServer;
use tokio::net::UnixStream;

#[tokio::test]
async fn io_bridge_constructs_from_a_socketpair() {
    let (ours, _peer) = UnixStream::pair().expect("UnixStream::pair");
    let server = QuicServer::new().expect("QuicServer::new");
    let bridge = IoBridge::new_with_stream_for_test(ours, server);

    // The assertion that matters is structural: this crate can link and call
    // the constructor at all under `browserless-harness`. Real scene coverage
    // arrives with the harness in a later task.
    let hash = bridge.cert_hash_sha256();
    assert_eq!(hash.len(), 64, "cert hash must be 64 hex chars");
    assert!(
        hash.chars().all(|c| c.is_ascii_hexdigit()),
        "cert hash must be hex, got {hash:?}"
    );
}
