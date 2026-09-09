//! Task 13: SocketPairPump framing tests.
//!
//! These tests exercise the production `encode_frame` / `parse_frame_rest`
//! wire format (see `ghostframe_lib::transport::ghostbridge`) over a real
//! `UnixStream` socketpair, without any netsim impairment logic involved.

use ghostframe_e2e::netsim::pump::SocketPairPump;
use ghostframe_lib::transport::ghostbridge::encode_frame;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn pump_round_trips_a_framed_datagram() {
    let (mut ours, peer) = tokio::net::UnixStream::pair().expect("pair");
    let mut pump = SocketPairPump::new(peer);
    let addr = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 5000);

    let frame = encode_frame(b"hello", &addr);
    ours.write_all(&frame).await.expect("write");
    // Close the write half after writing. Every test here does this so that a
    // framing regression fails fast with UnexpectedEof: with the write half
    // left open, a desynced `read_exact` blocks forever, turning a clean test
    // failure into a CI job timeout with no diagnostic.
    ours.shutdown().await.expect("shutdown");

    let got = pump.recv().await.expect("recv");
    assert_eq!(got.payload, b"hello");
    assert_eq!(got.addr, addr);
}

/// This is the test that actually catches an off-by-8 in the remainder
/// length: with a single frame, an over-read just hits EOF and produces a
/// misleading error. Writing two frames back-to-back means an over-read on
/// frame 1 eats into frame 2's header, so frame 2 either fails to parse or
/// parses with the wrong payload/addr.
#[tokio::test]
async fn pump_recv_handles_two_back_to_back_frames() {
    let (mut ours, peer) = tokio::net::UnixStream::pair().expect("pair");
    let mut pump = SocketPairPump::new(peer);

    let addr1 = SocketAddr::new(Ipv4Addr::new(100, 64, 0, 2).into(), 443);
    let addr2 = SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 9000);

    let frame1 = encode_frame(b"first-payload", &addr1);
    let frame2 = encode_frame(b"second-payload-distinct", &addr2);

    let mut both = Vec::new();
    both.extend_from_slice(&frame1);
    both.extend_from_slice(&frame2);
    ours.write_all(&both).await.expect("write");
    ours.shutdown().await.expect("shutdown");

    let got1 = pump.recv().await.expect("recv frame1");
    assert_eq!(got1.payload, b"first-payload");
    assert_eq!(got1.addr, addr1);

    let got2 = pump.recv().await.expect("recv frame2");
    assert_eq!(got2.payload, b"second-payload-distinct");
    assert_eq!(got2.addr, addr2);
}

#[tokio::test]
async fn pump_send_matches_encode_frame_byte_for_byte() {
    let (mut ours, peer) = tokio::net::UnixStream::pair().expect("pair");
    let mut pump = SocketPairPump::new(peer);
    let addr = SocketAddr::new(Ipv4Addr::new(198, 51, 100, 7).into(), 12345);
    let payload = b"outbound-payload";

    pump.send(payload, &addr).await.expect("send");

    let expected = encode_frame(payload, &addr);
    let mut raw = vec![0u8; expected.len()];
    ours.read_exact(&mut raw).await.expect("read raw bytes");

    assert_eq!(raw, expected);
}

#[tokio::test]
async fn pump_recv_rejects_absurd_total_len_without_allocating() {
    let (mut ours, peer) = tokio::net::UnixStream::pair().expect("pair");
    let mut pump = SocketPairPump::new(peer);

    // payload_len = 0, total_len = 0xFFFF_FFFF: passes the `total_len <
    // 8 + payload_len + 3` sanity check trivially (since it's huge), so a
    // naive implementation would proceed straight to
    // `vec![0u8; total_len - 8]`, a ~4 GiB allocation attempt.
    let mut header = Vec::with_capacity(8);
    header.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
    header.extend_from_slice(&0u32.to_be_bytes());
    ours.write_all(&header).await.expect("write header");
    ours.shutdown().await.expect("shutdown");

    let err = pump.recv().await.expect_err("must reject absurd total_len");
    assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
}

/// Guards against the size bound being satisfied by rejecting everything
/// large: a legitimate frame near the top of the valid UDP payload range
/// (65000 bytes, close to the 65507-byte practical UDP payload ceiling)
/// must still round-trip through `recv`.
#[tokio::test]
async fn pump_recv_accepts_a_legitimate_large_frame() {
    let (mut ours, peer) = tokio::net::UnixStream::pair().expect("pair");
    let mut pump = SocketPairPump::new(peer);
    let addr = SocketAddr::new(Ipv4Addr::new(100, 64, 0, 2).into(), 443);
    let payload = vec![0xABu8; 65000];

    let frame = encode_frame(&payload, &addr);
    ours.write_all(&frame).await.expect("write");
    ours.shutdown().await.expect("shutdown");

    let got = pump.recv().await.expect("recv large frame");
    assert_eq!(got.payload, payload);
    assert_eq!(got.addr, addr);
}
