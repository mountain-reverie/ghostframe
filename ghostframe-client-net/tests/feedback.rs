// SPDX-License-Identifier: MIT OR Apache-2.0
mod common;

use common::{connected_session, pump_with_wt};
use ghostframe_client_net::ClientNet;

/// The Hello queued by `ClientCore::new` at construction must survive the
/// gap between "queued" and "a feedback stream exists to carry it" — that
/// gap is exactly what `pending_stream_out` exists to bridge. Once the
/// WebTransport session is ready, `ClientNet` opens a bidi stream and
/// flushes it there; the server routes data on any bidi stream other than
/// the session stream into its feedback queue.
#[test]
fn hello_reaches_the_server_on_the_feedback_stream() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let feedback = wt.drain_feedback();
    assert!(
        feedback.iter().any(|m| m.first() == Some(&0x03)),
        "server must receive the Hello message (type 0x03), got {feedback:?}"
    );
}

/// Input produced before the session is ready -- the pointer crossing the
/// window as it appears, the release of the Enter that launched the client --
/// must not reach the stream ahead of the Hello. The server reads the first
/// non-Hello message from a session that is not live yet as grounds to
/// discard the rest of that read, Hello included, and then closes the
/// connection at the HELLO deadline: a black window and no input, 20 s later
/// "no HELLO before deadline".
#[test]
fn input_sent_before_the_session_is_ready_does_not_precede_the_hello() {
    use common::{admitting_server, pinned_hash};
    use ghostframe_client_net::{ClientNet, ClientNetConfig};
    use ghostframe_lib::transport::webtransport::WebTransportServer;
    use std::net::{Ipv6Addr, SocketAddr};
    use std::time::Instant;

    let mut server = admitting_server();
    let mut wt = WebTransportServer::new();
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
    // A pointer move, as the CLI encodes it, before any handshake step.
    client.send_input(vec![0x05, 0x01, 0x00, 0x10, 0x00, 0x20], now_us);

    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 128);
    assert!(wt.is_connected(), "server must accept the CONNECT");

    let first: Vec<u8> = wt.drain_feedback().into_iter().flatten().collect();
    assert_eq!(
        first.first(),
        Some(&0x03),
        "the first byte on the feedback stream must be the Hello's type, got {first:02x?}"
    );
}

/// The control for the test above: input sent once the session is ready does
/// arrive, so dropping early input is not hiding input altogether.
#[test]
fn input_sent_after_the_session_is_ready_reaches_the_server() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    let _ = wt.drain_feedback();

    let motion = vec![0x05, 0x01, 0x00, 0x10, 0x00, 0x20];
    client.send_input(motion.clone(), now_us);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let got: Vec<u8> = wt.drain_feedback().into_iter().flatten().collect();
    assert!(
        got.windows(motion.len()).any(|w| w == motion.as_slice()),
        "input after SessionReady must reach the server, got {got:02x?}"
    );
}

/// A reset request reaches the server on the feedback stream, byte for byte,
/// once the session is ready (and after the Hello).
#[test]
fn a_reset_request_reaches_the_server_on_the_feedback_stream() {
    use ghostframe_client_core::ResetReason;
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    let _ = wt.drain_feedback();

    client.request_reset(ResetReason::DecoderFailure, 0x0102_0304, now_us);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let got: Vec<u8> = wt.drain_feedback().into_iter().flatten().collect();
    let want = [0x09, 0x01, 0x04, 0x03, 0x02, 0x01];
    assert!(
        got.windows(want.len()).any(|w| w == want),
        "reset request must reach the server, got {got:02x?}"
    );
}

/// A client that asks for H.264 also advertises the v2 feedback: Hello caps
/// 0x02 | 0x04 (and 0x01 for indices_raw).
#[test]
fn an_h264_client_advertises_both_h264_hello_bits() {
    use common::{admitting_server, pinned_hash};
    use ghostframe_client_net::{ClientNet, ClientNetConfig};
    use ghostframe_lib::transport::webtransport::WebTransportServer;
    use std::net::{Ipv6Addr, SocketAddr};
    use std::time::Instant;

    let mut server = admitting_server();
    let mut wt = WebTransportServer::new();
    let cfg = ClientNetConfig {
        server_name: "localhost".into(),
        server_cert_sha256: pinned_hash(&server),
        indices_raw_enabled: true,
        supports_h264: true,
        display: None,
    };
    let base = Instant::now();
    let mut now_us = 0u64;
    let mut client = ClientNet::new(cfg, now_us).expect("ClientNet::new");
    client
        .connect(SocketAddr::new(Ipv6Addr::LOCALHOST.into(), 443), now_us)
        .expect("connect");
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 128);

    let got: Vec<u8> = wt.drain_feedback().into_iter().flatten().collect();
    assert_eq!(&got[..2], &[0x03, 0x07], "Hello caps, got {got:02x?}");
}

use ghostframe_lib::transport::webtransport::StatsWrite;
use ghostframe_protocol::stats::{
    MetricDescriptor, MetricGroup, MetricKind, MetricValue, ServerFrameReader, StatsSample,
    StatsSchema, SERVER_MSG_STATS_SAMPLE,
};

/// What the server writes on the feedback stream's return half reaches the
/// client's reader, on the real quinn pair (the wire path of the stats stream).
#[test]
fn a_stats_frame_written_by_the_server_reaches_the_client() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    assert!(
        wt.note_stats_subscribe(),
        "the Hello arrived on the feedback stream, so it is known"
    );

    let schema = text_schema();
    let frame = StatsSample {
        schema_id: schema.schema_id,
        timestamp_us: 77,
        values: vec![MetricValue::Text("tile".into())],
    }
    .encode_frame();
    let conn = server.connections.values_mut().next().expect("connection");
    assert_eq!(wt.write_stats_frame(conn, &frame), StatsWrite::Written);
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);

    let got = client.read_feedback_return(now_us);
    assert_eq!(got, frame, "the bytes arrive unchanged, on the return half");
    assert!(
        client.read_feedback_return(now_us).is_empty(),
        "a read drains: the same bytes are not returned twice"
    );
}

fn text_schema() -> StatsSchema {
    StatsSchema {
        schema_id: 1,
        server_version: "t".into(),
        host_name: "h".into(),
        metrics: vec![MetricDescriptor {
            id: 1,
            name: "n".into(),
            unit: "1".into(),
            kind: MetricKind::Text,
            group: MetricGroup::Other,
            label: "l".into(),
        }],
    }
}

/// With the client not reading, the stream's flow-control window fills. The
/// server must then refuse whole frames, never leave one torn: what the
/// client eventually reads decodes cleanly, in order, and contains exactly the
/// frames the server reported as written.
#[test]
fn a_full_window_refuses_whole_frames_and_never_tears_one() {
    let (mut client, mut server, mut wt, mut now_us, base) = connected_session();
    pump_with_wt(&mut client, &mut server, &mut wt, base, &mut now_us, 32);
    assert!(wt.note_stats_subscribe());
    let schema = text_schema();

    let make = |i: u64| {
        StatsSample {
            schema_id: schema.schema_id,
            timestamp_us: i,
            values: vec![MetricValue::Text("x".repeat(200))],
        }
        .encode_frame()
    };

    let mut written: Vec<u64> = Vec::new();
    let mut blocked = false;
    for i in 0..200_000u64 {
        let conn = server.connections.values_mut().next().expect("connection");
        match wt.write_stats_frame(conn, &make(i)) {
            StatsWrite::Written => written.push(i),
            StatsWrite::Blocked => {
                blocked = true;
                break;
            }
            other => panic!("frame {i}: {other:?}"),
        }
    }
    assert!(
        blocked,
        "premise: 200k x ~205 bytes overruns quinn's 1.25 MB default stream window"
    );
    assert!(
        wt.stats_tail_len() > 0 || !written.is_empty(),
        "premise: something was written before the block"
    );

    // The client drains; credit returns; the owed tail is flushed by the next
    // write, then one more frame goes after it.
    let mut reader = ServerFrameReader::default();
    let mut seen: Vec<u64> = Vec::new();
    let drain = |client: &mut ClientNet,
                 server: &mut _,
                 wt: &mut _,
                 now_us: &mut u64,
                 reader: &mut ServerFrameReader,
                 seen: &mut Vec<u64>| {
        for _ in 0..400 {
            pump_with_wt(client, server, wt, base, now_us, 8);
            reader.push(&client.read_feedback_return(*now_us));
            while let Some(f) = reader.next_frame() {
                let (ty, payload) = f.expect("no torn or oversize frame");
                assert_eq!(ty, SERVER_MSG_STATS_SAMPLE);
                let s = StatsSample::decode_payload(&payload, &text_schema())
                    .expect("every frame that arrives is whole");
                seen.push(s.timestamp_us);
            }
        }
    };
    drain(
        &mut client,
        &mut server,
        &mut wt,
        &mut now_us,
        &mut reader,
        &mut seen,
    );

    let last = u64::MAX;
    let conn = server.connections.values_mut().next().expect("connection");
    assert_eq!(
        wt.write_stats_frame(conn, &make(last)),
        StatsWrite::Written,
        "credit is back: the tail flushes and the new frame follows"
    );
    written.push(last);
    drain(
        &mut client,
        &mut server,
        &mut wt,
        &mut now_us,
        &mut reader,
        &mut seen,
    );

    assert_eq!(wt.stats_tail_len(), 0, "nothing is left owed");
    assert_eq!(reader.buffered(), 0, "no half frame is left in the reader");
    assert_eq!(
        seen, written,
        "exactly the frames reported Written, in order, none torn, none refused ones"
    );
}
