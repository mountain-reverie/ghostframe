mod common;

use common::{connected_session, pinned_hash, pump};
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::quic::QuicServer;
use std::net::{Ipv6Addr, SocketAddr};
use std::time::Instant;

#[test]
fn quic_handshake_completes_against_the_real_server() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: false,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");

    pump(&mut client, &mut server, base, &mut now_us, 64);

    assert!(
        client.take_events().contains(&ClientNetEvent::Connected),
        "client must report Connected"
    );
}

#[test]
fn webtransport_session_becomes_ready() {
    // `connected_session` already asserts `SessionReady` fired and
    // `wt.is_connected()`; nothing further to check here beyond it not
    // panicking.
    let _ = connected_session();
}
