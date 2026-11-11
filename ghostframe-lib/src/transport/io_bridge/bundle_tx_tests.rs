//! The packer at `send_counted` (`TransportConfig::fec_framing`, R6-c task C5).
//!
//! A unit bridge has no quinn connection, so the sends go to
//! `IoBridge::test_wire`, which accepts or refuses whole frames; everything
//! upstream of it (emitter, drain, packer, ledger, billing, ACK v4) is the
//! production code.

use super::*;
use crate::transport::reliable_emitter::cache::ProbeTag;
use crate::transport::reliable_emitter::{Content, EmitKey};
use ghostframe_protocol::fec::feedback::{BundleAck, BundleAckBatch};
use ghostframe_protocol::fec::frame::{parse, records, Frame};
use tokio::net::UnixStream;

const H: ConnectionHandle = ConnectionHandle(3);

/// Frames of 999 bytes of datagram: three 300-byte tile datagrams per bundle.
const RAW: usize = 1000;
const TILE_LEN: usize = 300;

fn framed_bridge() -> IoBridge {
    let (ours, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let mut b = IoBridge::new_with_stream_for_test(ours, server);
    b.wt_sessions.insert(H, WebTransportServer::default());
    b.last_hello_from = Some(H);
    b.set_fec_framing(true);
    b.test_wire = Some(bundle_tx::TestWire::default());
    b.forced_raw_datagram_size_for_test = Some(RAW);
    b
}

/// Queue `n` tile passes in the emitter, as a frame of tiles would.
fn submit_tiles(b: &mut IoBridge, n: u32) {
    let now = now_std();
    for i in 0..n {
        let mut dg = vec![0u8; TILE_LEN];
        dg[0] = 0x80; // a tile datagram
        dg[16..20].copy_from_slice(&i.to_be_bytes());
        b.reliable_emitter
            .submit_one(EmitKey::new(100 + i, 1, 2, 0), dg.into(), now, None, now);
    }
}

fn frame_records(f: &[u8]) -> Vec<Vec<u8>> {
    match parse(f).expect("a frame") {
        Frame::Plain(p) => records(p.records).map(|r| r.unwrap().to_vec()).collect(),
        other => panic!("not a PLAIN frame: {other:?}"),
    }
}

fn frame_seq(f: &[u8]) -> u32 {
    match parse(f).expect("a frame") {
        Frame::Plain(p) => p.bundle_seq,
        other => panic!("not a PLAIN frame: {other:?}"),
    }
}

fn wire_seq_of(record: &[u8]) -> u32 {
    u32::from_be_bytes(record[8..12].try_into().unwrap())
}

fn ack_v4(b: &mut IoBridge, ids: &[u32]) {
    let dg = BundleAckBatch {
        entries: ids
            .iter()
            .map(|&bundle_seq| BundleAck {
                bundle_seq,
                arrival_time_ms_lo16: 1_000,
            })
            .collect(),
    }
    .encode();
    b.dispatch_inbound_datagram(H, &dg);
}

fn send_all(b: &mut IoBridge) {
    b.drain_reliable_emitter(now_std());
    assert!(b.flush_bundles(), "premise: the wire accepts");
}

#[tokio::test]
async fn a_frame_of_tiles_leaves_as_fewer_larger_plain_bundles() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 10);
    send_all(&mut b);

    assert_eq!(b.sent_log_for_test.len(), 10, "records are logged pre-pack");
    let frames = &b.sent_frames_for_test;
    assert_eq!(frames.len(), 4, "3 + 3 + 3 + 1");
    let mut seen: Vec<u32> = Vec::new();
    for f in frames {
        assert!(f.len() < RAW, "a frame fits the datagram limit");
        for r in frame_records(f) {
            assert_eq!(r.len(), TILE_LEN);
            seen.push(wire_seq_of(&r));
        }
    }
    let mut want: Vec<u32> = b.sent_log_for_test.iter().map(|d| wire_seq_of(d)).collect();
    want.sort_unstable();
    seen.sort_unstable();
    assert_eq!(seen, want, "every record leaves exactly once");
    assert!(b.pending_bundles.is_empty());
    assert!(
        b.emitted_meta.is_empty(),
        "every record's meta was consumed"
    );
}

#[tokio::test]
async fn the_ledger_holds_one_transmission_per_bundle_and_an_ack_v4_releases_every_tile() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 10);
    send_all(&mut b);
    let seqs: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .map(|f| frame_seq(f))
        .collect();
    assert_eq!(seqs.len(), 4);

    assert_eq!(
        b.transmission_ledger.len(),
        4,
        "one per bundle, not per record"
    );
    for (i, s) in seqs.iter().enumerate() {
        let tx = b.transmission_ledger.peek(*s).expect("recorded at seal");
        let want = if i < 3 { 3 } else { 1 };
        assert_eq!(tx.records.len(), want);
        assert!(tx.records().all(|(_, c)| matches!(c, Content::Tile(_))));
        assert_eq!(tx.wire_bytes, b.sent_frames_for_test[i].len());
    }
    assert_eq!(b.reliable_emitter.pending_cache_entries(), 10);

    for s in &seqs {
        b.stamp_departure(*s, now_std());
    }
    ack_v4(&mut b, &seqs);

    assert_eq!(
        b.reliable_emitter.pending_cache_entries(),
        0,
        "every tile of every acknowledged bundle is released"
    );
    assert_eq!(b.transmission_ledger.len(), 0);
    assert_eq!(b.bwe_samples_buffer.len(), 4, "one BWE sample per bundle");
}

#[tokio::test]
async fn a_probe_pad_is_alone_in_its_bundle() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 2);
    let now = now_std();
    let template = bytes::Bytes::from(vec![0u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter.submit_h264_pad(&template, probe, now);
    submit_tiles(&mut b, 2);
    send_all(&mut b);

    let frames = &b.sent_frames_for_test;
    assert_eq!(frames.len(), 3, "2 tiles, the pad, 2 tiles");
    assert_eq!(frame_records(&frames[0]).len(), 2);
    let pad = frame_records(&frames[1]);
    assert_eq!(pad.len(), 1);
    assert_eq!(pad[0].len(), 100);
    assert_eq!(frame_records(&frames[2]).len(), 2);
    let s = frame_seq(&frames[1]);
    let tx = b.transmission_ledger.peek(s).expect("recorded");
    assert!(matches!(tx.records[0].1, Content::Pad(_)));
    assert_eq!(tx.records.len(), 1);
}

#[tokio::test]
async fn offered_bytes_equal_what_quinn_accepted_under_framing() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 10);
    let now = now_std();
    let template = bytes::Bytes::from(vec![0u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter.submit_h264_pad(&template, probe, now);
    send_all(&mut b);
    b.emit_frame_dimensions(1, 0, 160, 96);
    b.send_pong_framed(H, &[0x05, 1, 2, 3]);
    b.flush_bundles();

    let cum = b.test_wire.as_ref().unwrap().cum;
    assert!(cum > 0);
    assert_eq!(
        b.instr.offered.total(),
        cum,
        "records + 2, the frame header and the framing as Control"
    );
    // Every byte billed to the bucket is a byte offered: records at the
    // drain (not run under a gate here), the rest as soft charges.
    let recs: u64 = [WireClass::Critical, WireClass::H264, WireClass::Control]
        .iter()
        .map(|c| b.instr.offered.of(*c))
        .sum();
    assert!(recs > 0);
}

#[tokio::test]
async fn frame_dimensions_and_a_pong_leave_as_lone_plain_frames() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 2);
    b.drain_reliable_emitter(now_std());
    // The two tiles are still in the open bundle.
    assert!(b.sent_frames_for_test.is_empty());

    b.emit_frame_dimensions(7, 0, 160, 96);
    let dims = crate::transport::protocol::build_frame_dimensions_datagram(7, 0, 160, 96);
    let frames = b.sent_frames_for_test.clone();
    assert_eq!(
        frames.len(),
        2,
        "the open bundle, then the dimensions alone"
    );
    assert_eq!(frame_records(&frames[0]).len(), 2);
    assert_eq!(frame_records(&frames[1]), vec![dims.to_vec()]);

    let pong = [0x05u8, 9, 9, 9];
    b.send_pong_framed(H, &pong);
    let frames = b.sent_frames_for_test.clone();
    assert_eq!(frames.len(), 3);
    assert_eq!(frame_records(&frames[2]), vec![pong.to_vec()]);
    // Bundle sequence numbers are one run across all of them.
    let seqs: Vec<u32> = frames.iter().map(|f| frame_seq(f)).collect();
    assert_eq!(seqs, vec![seqs[0], seqs[0] + 1, seqs[0] + 2]);
}

#[tokio::test]
async fn a_blocked_send_loses_no_record() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 10);
    b.test_wire.as_mut().unwrap().blocked = true;

    let stop = b.drain_reliable_emitter(now_std());
    assert!(
        matches!(stop, pacing::Stop::Rejected),
        "a standing refusal stops the drain"
    );
    b.flush_bundles();
    assert!(!b.pending_bundles.is_empty(), "the refused bundle is kept");
    let held: usize = b
        .pending_bundles
        .iter()
        .map(|p| p.records.len())
        .sum::<usize>();
    assert!(held >= 3);
    assert_eq!(
        b.test_wire.as_ref().unwrap().cum,
        0,
        "nothing reached the wire"
    );

    b.test_wire.as_mut().unwrap().blocked = false;
    b.sent_frames_for_test.clear();
    // The retry goes first, then the rest of the queue.
    send_all(&mut b);
    b.drain_reliable_emitter(now_std());
    b.flush_bundles();

    let mut seen: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .flat_map(|f| frame_records(f))
        .map(|r| wire_seq_of(&r))
        .collect();
    seen.sort_unstable();
    let before = seen.len();
    seen.dedup();
    assert_eq!(before, seen.len(), "no record is sent twice");
    assert_eq!(seen.len(), 10, "and none is lost");
    // Bundles reached the wire in sequence order.
    let seqs: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .map(|f| frame_seq(f))
        .collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
}

#[tokio::test]
async fn a_lost_bundle_requests_a_retransmit_of_every_tile_in_it() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 15);
    send_all(&mut b);
    let seqs: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .map(|f| frame_seq(f))
        .collect();
    assert_eq!(seqs.len(), 5);
    for s in &seqs {
        b.stamp_departure(*s, now_std());
    }
    // Bundles 1..=4 arrive; bundle 0 is overtaken by K_PKT of them.
    ack_v4(&mut b, &seqs[1..]);
    assert_eq!(b.instr.loss_retx_requested, 0);

    b.expire_transmissions(now_std());

    assert_eq!(b.instr.losses_declared, 1, "one verdict, for the bundle");
    assert_eq!(
        b.instr.loss_retx_requested, 3,
        "one retransmit request per tile record of the lost bundle"
    );
    // And they are built: the tick turns the requests into datagrams.
    let mut n = 0;
    b.reliable_emitter
        .tick_loss_budgeted(now_std(), usize::MAX, usize::MAX, &mut n);
    assert_eq!(n, 3);
    assert_eq!(b.reliable_emitter.stats.loss_retransmits, 3);
}

#[tokio::test]
async fn the_bandwidth_cap_runs_on_bundles_and_the_drop_plan_on_records() {
    let mut b = framed_bridge();
    // A cap that admits nothing: every bundle is consumed, not sent.
    b.outbound_bandwidth_cap = Some(crate::transport::bandwidth_cap::BandwidthCap::new(1));
    submit_tiles(&mut b, 6);
    send_all(&mut b);
    assert_eq!(b.sent_frames_for_test.len(), 2, "2 bundles offered");
    assert_eq!(b.test_wire.as_ref().unwrap().cum, 0, "none reached quinn");
    assert_eq!(b.transmission_ledger.len(), 2);
    for s in b.sent_frames_for_test.iter().map(|f| frame_seq(f)) {
        assert_eq!(
            b.transmission_ledger.departed(s),
            Some(true),
            "a capped bundle departs and is lost"
        );
    }
}

#[tokio::test]
async fn forgetting_the_live_session_resets_the_packer() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 2);
    b.drain_reliable_emitter(now_std());
    assert!(b.sent_frames_for_test.is_empty(), "records are open");
    b.forget_session(H);
    assert!(b.packer.flush().is_none(), "the open bundle is gone");
    assert!(b.pending_bundles.is_empty());
}

#[tokio::test]
async fn record_limit_sizes_the_drain_and_the_frame_path_when_framing_is_on() {
    let mut b = framed_bridge();
    let limit = ghostframe_lib_packer_limit(RAW - 1);
    assert_eq!(b.inner_limit(RAW - 1), limit);
    b.set_fec_framing(false);
    assert_eq!(b.inner_limit(RAW - 1), RAW - 1);
    b.set_fec_framing(true);
    // A record just over the limit is retired, not packed.
    let now = now_std();
    let mut dg = vec![0u8; limit + 1];
    dg[0] = 0x80;
    b.reliable_emitter
        .submit_one(EmitKey::new(1, 0, 0, 0), dg.into(), now, None, now);
    b.drain_reliable_emitter(now);
    assert!(b.sent_frames_for_test.is_empty());
    assert_eq!(b.h264.stats.oversize_tile, 1);
    assert!(b.emitted_meta.is_empty());
}

fn ghostframe_lib_packer_limit(max_dg: usize) -> usize {
    crate::transport::fec_tx::packer::Packer::record_limit(max_dg)
}
