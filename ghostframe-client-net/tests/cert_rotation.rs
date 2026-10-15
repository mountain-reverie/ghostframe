//! Real TLS handshakes against a `QuicServer` whose certificate clock is
//! injected, across a rotation.
//!
//! Unit tests on `CertSchedule` cannot see whether the endpoint actually
//! presents the scheduled certificate: a server built with a single fixed
//! cert passes all of them. Only a handshake does.

#[allow(dead_code)] // shared helpers; this binary uses only some
mod common;

use common::pump;
use ghostframe_client_net::{ClientNet, ClientNetConfig};
use ghostframe_lib::transport::quic::{CertClock, QuicServer};
use std::net::{Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::Instant;

const DAY: i64 = 24 * 3600;

fn hash32(hex_str: &str) -> [u8; 32] {
    let mut h = [0u8; 32];
    hex::decode_to_slice(hex_str, &mut h).expect("hex");
    h
}

/// Does a client pinning exactly `pins` complete a handshake?
fn connects(server: &mut QuicServer, pins: Vec<[u8; 32]>) -> bool {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pins[0],
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us)
        .expect("ClientNet::new")
        .with_extra_cert_pins(pins[1..].to_vec());
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");
    pump(&mut client, server, base, &mut now_us, 64);
    client.is_connected()
}

#[test]
fn a_client_fetched_before_rotation_still_connects_after_it() {
    let base = time::OffsetDateTime::now_utc().unix_timestamp();
    let off = Arc::new(AtomicI64::new(0));
    let o = off.clone();
    let clock: CertClock = Arc::new(move || {
        time::OffsetDateTime::from_unix_timestamp(base + o.load(Ordering::SeqCst)).unwrap()
    });
    let mut server = QuicServer::new_with_cert_clock(clock).expect("server");

    // The client fetched [current, next] now.
    let fetched = server.cert_info().schedule.hashes_hex();
    let (a, b) = (hash32(&fetched[0]), hash32(&fetched[1]));

    // Before rotation the server presents A: pinning only B must fail
    // (proves the handshake really checks the presented cert).
    assert!(
        connects(&mut server, vec![a]),
        "pinned current, pre-rotation"
    );
    assert!(!connects(&mut server, vec![b]), "B is not presented yet");

    // Eight days later the server presents B. The client still holds the
    // pair it fetched; it must connect, and pinning only A must now fail.
    off.store(8 * DAY, Ordering::SeqCst);
    assert!(
        connects(&mut server, vec![a, b]),
        "client holding [A, B] must connect after rotation"
    );
    assert!(connects(&mut server, vec![b]), "the TLS layer presents B");
    assert!(!connects(&mut server, vec![a]), "A is no longer presented");
}

#[test]
fn extra_pins_reach_the_verifier_set() {
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: [1; 32],
        indices_raw_enabled: true,
        supports_h264: false,
        display: None,
    };
    let client = ClientNet::new(cfg, 0)
        .expect("ClientNet::new")
        .with_extra_cert_pins(vec![[2; 32]]);
    assert_eq!(client.cert_pins(), vec![[1; 32], [2; 32]]);
}
