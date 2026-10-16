mod common;

use common::{
    admitting_server, client_ip, connected_session, install_gate, owner_identity, pinned_hash, pump,
};
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::peer_auth::{PeerIdentity, PeerLookup, WhoIsError};
use ghostframe_lib::transport::quic::QuicServer;
use std::net::{Ipv6Addr, SocketAddr};
use std::time::Instant;

#[test]
fn quic_handshake_completes_against_the_real_server() {
    let mut server = admitting_server();
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
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

/// Both ends of a real handshake must have negotiated the hybrid
/// post-quantum group, not silently fallen back to classical X25519 (which is
/// what the `ring` provider did, with no error). Read from each side's own
/// TLS state, not inferred from the config.
#[test]
fn handshake_negotiates_x25519mlkem768_on_both_ends() {
    use rustls::NamedGroup;
    let mut server = admitting_server();
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");
    pump(&mut client, &mut server, base, &mut now_us, 64);
    assert!(client.is_connected(), "handshake must have completed");

    let group = |hd: Box<dyn std::any::Any>| {
        hd.downcast::<quinn_proto::crypto::rustls::HandshakeData>()
            .expect("rustls handshake data")
            .negotiated_key_exchange_group
    };
    assert_eq!(
        group(client.tls_handshake_data().expect("client handshake data")),
        NamedGroup::X25519MLKEM768,
        "client side"
    );
    let conn = server.connections.values().next().expect("server conn");
    let hd = conn
        .crypto_session()
        .handshake_data()
        .expect("server handshake data");
    let hd = hd
        .downcast::<quinn_proto::crypto::rustls::HandshakeData>()
        .expect("rustls handshake data");
    assert_eq!(
        hd.negotiated_key_exchange_group,
        NamedGroup::X25519MLKEM768,
        "server side"
    );
}

// ---- Tailnet peer admission (transport::peer_auth) ----
//
// These drive a real client handshake into the gate, so they see what a
// peer sees: refused before a `Connection` exists, or admitted.

fn client_for(server: &QuicServer) -> (ClientNet, Instant, u64) {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(server),
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
    };
    let mut client = ClientNet::new(cfg, 0).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), 0)
        .expect("connect");
    (client, Instant::now(), 0)
}

fn refused(events: &[ClientNetEvent]) -> bool {
    events.iter().any(|e| match e {
        ClientNetEvent::ConnectionLost { reason } => reason.contains("refused"),
        _ => false,
    })
}

fn other_user() -> PeerIdentity {
    PeerIdentity {
        user_id: 2,
        login_name: "mallory@test".into(),
        node_name: "intruder".into(),
        tags: vec![],
    }
}

#[test]
fn a_server_given_no_identity_source_refuses_everyone() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    let (mut client, base, mut now_us) = client_for(&server);
    pump(&mut client, &mut server, base, &mut now_us, 64);
    let events = client.take_events();
    assert!(!events.contains(&ClientNetEvent::Connected));
    assert!(refused(&events), "expected a refusal, got {events:?}");
    assert!(server.connections.is_empty());
}

#[test]
fn a_peer_of_another_user_is_refused_before_a_connection_exists() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    install_gate(&mut server, Ok(other_user()), true);
    let (mut client, base, mut now_us) = client_for(&server);
    pump(&mut client, &mut server, base, &mut now_us, 64);
    let events = client.take_events();
    assert!(!events.contains(&ClientNetEvent::Connected));
    assert!(refused(&events), "expected a refusal, got {events:?}");
    assert!(server.connections.is_empty());
    assert!(server.take_accepted().is_empty(), "nothing was accepted");
}

#[test]
fn a_peer_the_tailnet_does_not_know_is_refused() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    install_gate(&mut server, Err(WhoIsError::NotFound), true);
    let (mut client, base, mut now_us) = client_for(&server);
    pump(&mut client, &mut server, base, &mut now_us, 64);
    assert!(refused(&client.take_events()));
    assert!(server.connections.is_empty());
}

/// Drive a client into a server whose identity source is NOT immediate,
/// so its Initial is held, then resolve the lookup with `answer`.
/// Returns the client's events after resolution and the server.
fn held_then_resolved(answer: PeerLookup) -> (Vec<ClientNetEvent>, QuicServer) {
    let mut server = QuicServer::new().expect("QuicServer::new");
    // The source's own answer is never consulted here: the test resolves
    // the lookup by hand, as the I/O bridge does when its task finishes.
    install_gate(&mut server, Err(WhoIsError::Failed("unused".into())), false);
    let (mut client, base, mut now_us) = client_for(&server);
    pump(&mut client, &mut server, base, &mut now_us, 8);

    assert!(server.connections.is_empty(), "held, not accepted");
    assert_eq!(server.held_incoming_count(), 1);
    let lookups = server.take_peer_lookups();
    assert_eq!(lookups.len(), 1, "one lookup for the one peer");
    assert_eq!(lookups[0].0, client_ip());
    assert!(server.take_peer_lookups().is_empty(), "not asked twice");
    assert!(
        !client
            .take_events()
            .iter()
            .any(|e| matches!(e, ClientNetEvent::ConnectionLost { .. })),
        "a held client is not told anything yet"
    );

    let now = base + std::time::Duration::from_micros(now_us);
    for (_dst, bytes) in server.resolve_peer(client_ip(), &answer, now) {
        client.handle_udp(
            &bytes,
            SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443),
            now_us,
        );
    }
    assert_eq!(server.held_incoming_count(), 0);
    pump(&mut client, &mut server, base, &mut now_us, 64);
    (client.take_events(), server)
}

#[test]
fn a_held_connection_is_admitted_when_its_lookup_finds_the_owner() {
    let (events, server) = held_then_resolved(Ok(owner_identity()));
    assert!(
        events.contains(&ClientNetEvent::Connected),
        "expected Connected, got {events:?}"
    );
    assert_eq!(server.connections.len(), 1);
}

#[test]
fn a_held_connection_is_refused_when_its_lookup_finds_someone_else() {
    let (events, server) = held_then_resolved(Ok(other_user()));
    assert!(!events.contains(&ClientNetEvent::Connected));
    assert!(refused(&events), "expected a refusal, got {events:?}");
    assert!(server.connections.is_empty());
}

#[test]
fn a_held_connection_is_refused_when_its_lookup_fails() {
    let (events, server) = held_then_resolved(Err(WhoIsError::Failed("lookup timed out".into())));
    assert!(refused(&events), "expected a refusal, got {events:?}");
    assert!(server.connections.is_empty());
}

// While lookups are outstanding, held Initials are bounded: more clients
// from one peer than may be held are dropped, not queued.
#[test]
fn held_connections_are_bounded_per_peer() {
    let mut server = QuicServer::new().expect("QuicServer::new");
    install_gate(&mut server, Err(WhoIsError::Failed("unused".into())), false);
    let mut clients: Vec<_> = (0..10).map(|_| client_for(&server)).collect();
    for (client, base, now_us) in clients.iter_mut() {
        pump(client, &mut server, *base, now_us, 4);
    }
    let held = server.held_incoming_count();
    assert!(held > 0 && held < 10, "held {held} of 10");
    assert!(server.held_overflow() > 0, "the rest were dropped");
    assert_eq!(server.take_peer_lookups().len(), 1, "still one lookup");
    assert!(server.connections.is_empty());
}
