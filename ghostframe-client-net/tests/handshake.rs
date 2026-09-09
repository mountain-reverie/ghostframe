use bytes::BytesMut;
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::quic::QuicServer;
use ghostframe_lib::transport::webtransport::WebTransportServer;
use quinn_proto::{Event, StreamEvent};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

/// The server's cert hash, as the client pins it.
fn pinned_hash(server: &QuicServer) -> [u8; 32] {
    let mut hash = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut hash).expect("cert hash hex");
    hash
}

/// Shuttle one round of datagrams between the client and a real
/// `QuicServer`, and fire any due server-side timeout. Returns whether
/// anything moved, so callers can tell when to stop looping.
///
/// `QuicServer`'s public surface is already what the I/O bridge drives
/// (`handle_datagram`, `poll_transmit`, `next_timeout`, `handle_timeout`,
/// `drain_endpoint_events`), so no test-only helpers are needed for the
/// QUIC layer itself. Shared by `pump` and `pump_with_wt` so the two tests
/// don't duplicate this body.
fn shuttle(client: &mut ClientNet, server: &mut QuicServer, now: Instant, now_us: u64) -> bool {
    let server_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443);
    let client_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000);
    let mut buf = Vec::with_capacity(2048);
    let mut moved = false;

    while let Some(out) = client.poll_transmit() {
        let resp = server.handle_datagram(
            now,
            client_addr,
            None,
            None,
            BytesMut::from(&out.payload[..]),
            &mut buf,
        );
        if let Some(t) = resp {
            client.handle_udp(&buf[..t.size], server_addr, now_us);
        }
        buf.clear();
        moved = true;
    }

    server.drain_endpoint_events();
    while let Some(t) = server.poll_transmit(now, 10, &mut buf) {
        client.handle_udp(&buf[..t.size], server_addr, now_us);
        buf.clear();
        moved = true;
    }

    if server.next_timeout().is_some_and(|d| d <= now) {
        server.handle_timeout(now);
        moved = true;
    }

    moved
}

/// Shuttle datagrams between the client and a real `QuicServer` until the
/// handshake settles or `max_steps` is exhausted.
fn pump(
    client: &mut ClientNet,
    server: &mut QuicServer,
    base: Instant,
    now_us: &mut u64,
    max_steps: usize,
) {
    for _ in 0..max_steps {
        let now = base + Duration::from_micros(*now_us);
        let moved = shuttle(client, server, now, *now_us);
        *now_us += 1_000;
        if !moved {
            break;
        }
    }
}

/// Same as `pump`, but also drives the server-side `WebTransportServer`
/// state machine: hands it the connection once it exists, feeds it stream
/// events as they're polled off the connection, and force-reads every known
/// stream each round as a backstop against data that arrived without a
/// separate `Readable` event (mirrors `ghostframe-e2e/tests/loopback_h3.rs:475-495`).
fn pump_with_wt(
    client: &mut ClientNet,
    server: &mut QuicServer,
    wt: &mut WebTransportServer,
    base: Instant,
    now_us: &mut u64,
    max_steps: usize,
) {
    let mut wt_started = false;

    for _ in 0..max_steps {
        let now = base + Duration::from_micros(*now_us);
        let mut moved = shuttle(client, server, now, *now_us);

        if !wt_started {
            if let Some(conn) = server.connections.values_mut().next() {
                wt.on_new_connection(conn);
                wt_started = true;
                moved = true;
            }
        }

        while let Some((handle, event)) = server.poll_events() {
            if let Some(conn) = server.connections.get_mut(&handle) {
                match event {
                    Event::Stream(StreamEvent::Opened { dir }) => {
                        wt.on_stream_opened(conn, dir);
                    }
                    Event::Stream(StreamEvent::Readable { id }) => {
                        wt.on_stream_readable(conn, id);
                    }
                    _ => {}
                }
            }
            moved = true;
        }

        // Force-read known streams in case data arrived without a separate
        // Readable event.
        if let Some(conn) = server.connections.values_mut().next() {
            for sid in wt.all_known_stream_ids() {
                wt.on_stream_readable(conn, sid);
            }
        }

        *now_us += 1_000;
        if !moved {
            break;
        }
    }
}

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
    let mut server = QuicServer::new().expect("QuicServer::new");
    let mut wt = ghostframe_lib::transport::webtransport::WebTransportServer::new();
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

    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 128);

    assert!(wt.is_connected(), "server must accept the CONNECT");
    assert!(
        client.take_events().contains(&ClientNetEvent::SessionReady),
        "client must report SessionReady"
    );
}
