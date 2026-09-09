use bytes::BytesMut;
use ghostframe_client_net::{ClientNet, ClientNetConfig, ClientNetEvent};
use ghostframe_lib::transport::quic::QuicServer;
use std::net::{Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

/// The server's cert hash, as the client pins it.
fn pinned_hash(server: &QuicServer) -> [u8; 32] {
    let mut hash = [0u8; 32];
    hex::decode_to_slice(&server.cert_info().sha256_hex, &mut hash).expect("cert hash hex");
    hash
}

/// Shuttle datagrams between the client and a real `QuicServer` until the
/// handshake settles or `max_steps` is exhausted.
///
/// `QuicServer`'s public surface is already what the I/O bridge drives
/// (`handle_datagram`, `poll_transmit`, `next_timeout`, `handle_timeout`,
/// `drain_endpoint_events`), so no test-only helpers are needed.
fn pump(
    client: &mut ClientNet,
    server: &mut QuicServer,
    base: Instant,
    now_us: &mut u64,
    max_steps: usize,
) {
    let server_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443);
    let client_addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000);
    let mut buf = Vec::with_capacity(2048);

    for _ in 0..max_steps {
        let now = base + Duration::from_micros(*now_us);
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
                client.handle_udp(&buf[..t.size], server_addr, *now_us);
            }
            buf.clear();
            moved = true;
        }

        server.drain_endpoint_events();
        while let Some(t) = server.poll_transmit(now, 10, &mut buf) {
            client.handle_udp(&buf[..t.size], server_addr, *now_us);
            buf.clear();
            moved = true;
        }

        if server.next_timeout().is_some_and(|d| d <= now) {
            server.handle_timeout(now);
            moved = true;
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
