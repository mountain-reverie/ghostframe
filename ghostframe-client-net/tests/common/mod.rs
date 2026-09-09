//! Shared test plumbing for `ghostframe-client-net`'s integration tests.
//!
//! Everything here drives a *real* `ghostframe_lib::transport::quic::QuicServer`
//! (and, where needed, a real `WebTransportServer`) against `ClientNet`
//! purely in-memory: `shuttle` copies bytes out of one side's outbound queue
//! straight into the other's `handle_*` call, with no socket anywhere. This
//! file has no `#[test]` of its own — cargo only turns a top-level
//! `tests/*.rs` file into its own binary, so `tests/common/mod.rs` is just a
//! module the real test binaries (`handshake.rs`, `datagram.rs`, ...) pull
//! in with `mod common;`.

use bytes::BytesMut;
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::quic::QuicServer;
use ghostframe_lib::transport::webtransport::WebTransportServer;
use quinn_proto::{Event, StreamEvent};
use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

/// The server's cert hash, as the client pins it.
pub fn pinned_hash(server: &QuicServer) -> [u8; 32] {
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
pub fn shuttle(client: &mut ClientNet, server: &mut QuicServer, now: Instant, now_us: u64) -> bool {
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
///
/// Only `handshake.rs`'s bare-QUIC test calls this directly (everything
/// that also needs the WebTransport layer goes through `pump_with_wt` /
/// `connected_session` instead), so it's dead code from the point of view
/// of any test binary that doesn't use it -- hence the `allow`.
#[allow(dead_code)]
pub fn pump(
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
pub fn pump_with_wt(
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

/// Run the QUIC + WebTransport handshake against a real server and hand
/// back the connected parts so a test can drive further traffic (datagrams,
/// feedback streams, ...) directly.
///
/// Shared by every test that needs a live session rather than just the bare
/// handshake, so `webtransport_session_becomes_ready` in `handshake.rs` and
/// `solid_tile_datagram_becomes_tile_ready` in `datagram.rs` don't each
/// reimplement session setup.
pub fn connected_session() -> (ClientNet, QuicServer, WebTransportServer, u64, Instant) {
    let mut server = QuicServer::new().expect("QuicServer::new");
    let mut wt = WebTransportServer::new();
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
    let events = client.take_events();
    assert!(
        events.contains(&ClientNetEvent::SessionReady),
        "client must report SessionReady"
    );

    (client, server, wt, now_us, base)
}
