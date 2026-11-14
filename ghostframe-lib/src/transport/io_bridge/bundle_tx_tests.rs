// SPDX-License-Identifier: AGPL-3.0-or-later
//! The packer at `send_counted` (R6-c task C5).
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
    // One bundle is one datagram, however many records (and billing
    // entries) it carries.
    assert_eq!(b.instr.offered.bundles, b.sent_frames_for_test.len() as u64);
    assert!(
        b.instr.offered.datagrams.iter().sum::<u64>() > b.instr.offered.bundles,
        "premise: bundles hold several records"
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
async fn a_record_dropped_by_the_plan_is_a_lost_transmission_and_is_retransmitted() {
    use crate::transport::drop_plan::{DropPlan, DropRule};
    let mut b = framed_bridge();
    // Every test tile is at (0, 0); drop the second occurrence.
    b.drop_plan = Some(std::sync::Arc::new(std::sync::Mutex::new(DropPlan::new(
        vec![DropRule {
            tile_x: 0,
            tile_y: 0,
            occurrences: vec![1],
        }],
    ))));
    submit_tiles(&mut b, 15);
    send_all(&mut b);
    let sent: Vec<Vec<u8>> = b.sent_frames_for_test.clone();
    let seqs: Vec<u32> = sent.iter().map(|f| frame_seq(f)).collect();
    let in_bundles: usize = sent.iter().map(|f| frame_records(f).len()).sum();
    assert_eq!(in_bundles, 14, "premise: one record never reached a bundle");
    let gap: Vec<u32> = (0..=*seqs.iter().max().unwrap())
        .filter(|s| !seqs.contains(s))
        .collect();
    assert_eq!(gap.len(), 1, "one bundle_seq is the dropped record's");
    let dropped = gap[0];
    assert_eq!(
        b.transmission_ledger.departed(dropped),
        Some(true),
        "the dropped record is on the books and departed"
    );
    for s in &seqs {
        b.stamp_departure(*s, now_std());
    }
    // Everything after it is acknowledged: the packet threshold fires.
    let later: Vec<u32> = seqs.iter().copied().filter(|s| *s > dropped).collect();
    assert!(later.len() >= 3, "premise: enough later bundles {later:?}");
    ack_v4(&mut b, &later);
    b.expire_transmissions(now_std());
    assert!(
        b.instr.loss_retx_requested >= 1,
        "the dropped record is retransmitted"
    );
    let mut n = 0;
    b.reliable_emitter
        .tick_loss_budgeted(now_std(), usize::MAX, usize::MAX, &mut n);
    assert!(n >= 1);
}

#[tokio::test]
async fn forgetting_the_live_session_resets_the_packer() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 2);
    b.drain_reliable_emitter(now_std());
    assert!(b.sent_frames_for_test.is_empty(), "records are open");
    b.forget_session(H);
    assert!(b.packer.flush(0).is_none(), "the open bundle is gone");
    assert!(b.pending_bundles.is_empty());
}

#[tokio::test]
async fn record_limit_sizes_the_drain_and_the_frame_path() {
    let mut b = framed_bridge();
    let limit = ghostframe_lib_packer_limit(RAW - 1);
    assert_eq!(b.inner_limit(RAW - 1), limit);
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

/// A sealed bundle of `len` bytes and no records: only its length matters
/// to the short-bundle rule.
fn sized_bundle(len: usize) -> crate::transport::fec_tx::packer::SealedBundle {
    crate::transport::fec_tx::packer::SealedBundle {
        bundle_seq: 0,
        bytes: vec![0; len],
        records: Default::default(),
        emit_us: 0,
        kind: crate::transport::fec_tx::packer::BundleKind::Plain,
        block: None,
    }
}

/// quinn accepts `len` bytes (one byte of framing) on top of a FIFO of
/// `fifo` bytes; `fifo` is what it holds afterwards.
fn accept_bundle(b: &mut IoBridge, fifo: &mut u64, len: usize) {
    let sb = sized_bundle(len);
    *fifo += len as u64 + 1;
    b.note_bundle_enqueued(H, &sb, 1, *fifo, *fifo);
}

#[tokio::test]
async fn short_bundles_back_to_back_in_a_busy_fifo_are_counted() {
    // RAW is 1000, so a 300-byte bundle is short and a 990-byte one is not.
    let mut b = framed_bridge();
    let mut fifo = 0u64;
    accept_bundle(&mut b, &mut fifo, 300);
    assert_eq!(
        b.instr.admission.short_bundle_shared_risk, 0,
        "the FIFO was empty before the first"
    );
    accept_bundle(&mut b, &mut fifo, 300);
    assert_eq!(b.instr.admission.short_bundle_shared_risk, 1);

    // A full-size bundle between two short ones breaks the pair.
    let mut b = framed_bridge();
    let mut fifo = 0u64;
    accept_bundle(&mut b, &mut fifo, 300);
    accept_bundle(&mut b, &mut fifo, 990);
    accept_bundle(&mut b, &mut fifo, 300);
    assert_eq!(b.instr.admission.short_bundle_shared_risk, 0);
}

#[tokio::test]
async fn a_sealed_bundle_refused_as_too_large_gives_its_tiles_and_pad_back() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 2);
    let template = bytes::Bytes::from(vec![0u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter
        .submit_h264_pad(&template, probe, now_std());
    b.test_wire.as_mut().unwrap().too_large = true;
    b.drain_reliable_emitter(now_std());
    b.flush_bundles();

    assert_eq!(b.test_wire.as_ref().unwrap().cum, 0, "premise: none sent");
    assert!(b.pending_bundles.is_empty(), "the bundles are consumed");
    assert!(b.transmission_ledger.is_empty(), "forgotten, not judged");
    assert_eq!(b.h264.stats.oversize_tile, 2, "each tile record is retired");
    assert_eq!(b.h264.stats.oversize_pad, 1);
}

/// A standing refusal is one refusal of one bundle, however often the drain
/// and the pump's flush look at it again, and the drain takes no record from
/// the queue while a bundle waits.
#[tokio::test]
async fn a_standing_refusal_is_counted_once_and_stops_the_drain_before_the_next_record() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 12);
    b.test_wire.as_mut().unwrap().blocked = true;

    let stop = b.drain_reliable_emitter(now_std());
    assert!(matches!(stop, pacing::Stop::Rejected));
    assert_eq!(b.datagram_send_errs, 1, "premise: the refusal was counted");
    b.flush_bundles(); // seals the open bundle behind the refused one
    let waiting = b.pending_bundles.len();
    let queued = b.reliable_emitter.emission_queue_len();
    assert!(
        waiting > 0 && queued > 0,
        "premise: a bundle waits, work remains"
    );
    for _ in 0..4 {
        assert!(matches!(
            b.drain_reliable_emitter(now_std()),
            pacing::Stop::Rejected
        ));
        b.flush_bundles();
    }
    assert_eq!(b.datagram_send_errs, 1, "the same refusal, counted once");
    assert_eq!(b.pending_bundles.len(), waiting);
    assert_eq!(
        b.reliable_emitter.emission_queue_len(),
        queued,
        "no record is taken while a bundle waits"
    );

    // A refusal of the next bundle is a second one.
    b.test_wire.as_mut().unwrap().blocked = false;
    assert!(b.flush_bundles(), "the wire takes the waiting bundle");
    b.test_wire.as_mut().unwrap().blocked = true;
    b.drain_reliable_emitter(now_std());
    assert_eq!(b.datagram_send_errs, 2);
}

/// A record with a `wire_seq` but no parked meta (the meta table was cleared)
/// is ledgered as what its header says it is and billed as its class, not
/// sent as a content-less control frame.
#[tokio::test]
async fn a_record_without_parked_meta_is_ledgered_from_its_header() {
    use ghostframe_protocol::protocol::FrameHeader;
    let mut b = framed_bridge();
    let mut dg = Vec::new();
    FrameHeader {
        frame_seq: 5,
        frag_idx: 2,
        frag_total: 4,
        wire_seq: 77,
        timestamp_us: 0,
        flags: 0,
        slots: 0,
        ref_frame_seq: 0,
    }
    .encode(&mut dg);
    dg.resize(120, 0);
    assert!(b.emitted_meta.is_empty(), "premise: no parked meta");

    b.send_counted_framed(&dg, WireClass::H264, Some(77));
    assert!(b.flush_bundles());

    let id = frame_seq(&b.sent_frames_for_test[0]);
    let tx = b.transmission_ledger.peek(id).expect("ledgered");
    assert_eq!(
        tx.records.as_slice(),
        &[(
            77,
            Content::H264Frag {
                frame_seq: 5,
                frag_idx: 2
            }
        )]
    );
    assert!(
        b.instr.offered.of(WireClass::H264) >= 120,
        "billed as its class"
    );
}

/// A lone control bundle (frame dimensions, a pong) carries no record, but its
/// acknowledgement is still a delivery: one Critical sample, no probe.
#[tokio::test(start_paused = true)]
async fn an_acknowledged_lone_control_bundle_is_a_bwe_sample() {
    use crate::transport::io_bridge::bwe_glue::PassTier;
    let mut b = framed_bridge();
    b.emit_frame_dimensions(1, 0, 160, 96);
    b.flush_bundles();
    let id = frame_seq(&b.sent_frames_for_test[0]);
    assert!(
        b.transmission_ledger
            .peek(id)
            .expect("ledgered")
            .records
            .is_empty(),
        "premise: no record"
    );
    b.stamp_departure(id, now_std());
    ack_v4(&mut b, &[id]);

    assert_eq!(b.bwe_samples_buffer.len(), 1);
    let s = &b.bwe_samples_buffer[0];
    assert_eq!(s.tier, PassTier::Critical);
    assert!(s.probe.is_none());
    assert_eq!(s.size_bytes as usize, b.sent_frames_for_test[0].len());
}

/// A REPAIR frame of `payload_len` bytes as the bridge will build one in D4b:
/// a sealed bundle of kind `Repair` with no records.
fn repair_bundle(
    bundle_seq: u32,
    payload_len: usize,
) -> crate::transport::fec_tx::packer::SealedBundle {
    use ghostframe_protocol::fec::frame::RepairFrame;
    let payload = vec![0x5a; payload_len];
    let bytes = RepairFrame {
        rtt_hint: 0,
        bundle_seq,
        block_id: 7,
        k: 4,
        r: 2,
        repair_idx: 0,
        payload: &payload,
    }
    .encode();
    crate::transport::fec_tx::packer::SealedBundle {
        bundle_seq,
        bytes,
        records: Default::default(),
        emit_us: 0,
        kind: crate::transport::fec_tx::packer::BundleKind::Repair,
        block: None,
    }
}

/// D4a: a repair bundle is one ledger transmission with no records, billed
/// whole as `WireClass::Repair`, charged to the bucket for its framing only
/// (it paid its length at admission), acknowledged as one BWE sample, never
/// retransmitted on a verdict, and counted while queued.
#[tokio::test(start_paused = true)]
async fn a_repair_bundle_is_one_recordless_transmission_billed_as_repair() {
    let mut b = framed_bridge();
    b.admission.refill(now_std(), 1_000_000.0, 1_000);
    let seq = b.packer.alloc_seq();
    let rb = repair_bundle(seq, 200);
    let len = rb.bytes.len();
    let banked = b.admission.available();

    b.queue_bundles(smallvec::smallvec![rb]);
    assert_eq!(b.transmission_ledger.len(), 1, "one ledger transmission");
    assert!(b.transmission_ledger.peek(seq).unwrap().records.is_empty());
    assert_eq!(
        b.unsent_bundle_bytes(),
        len + b.live_framing_len(),
        "a queued repair counts toward the depth gate"
    );

    assert!(b.drive_pending_bundles(), "the wire accepts");
    assert!(b.pending_bundles.is_empty());
    let cum = b.test_wire.as_ref().unwrap().cum;
    assert!(cum > 0);
    assert_eq!(b.instr.offered.of(WireClass::Repair), len as u64);
    assert_eq!(
        b.instr.offered.of(WireClass::Control),
        0,
        "not double billed"
    );
    assert_eq!(b.instr.offered.total(), cum, "offered == quinn enqueued");
    assert_eq!(
        banked - b.admission.available(),
        b.live_framing_len(),
        "the bucket is charged the framing only"
    );
    assert_eq!(
        b.instr.admission.soft_charged_bytes,
        b.live_framing_len() as u64
    );

    b.stamp_departure(seq, now_std());
    ack_v4(&mut b, &[seq]);
    assert_eq!(b.bwe_samples_buffer.len(), 1, "one BWE sample");
    assert_eq!(b.bwe_samples_buffer[0].size_bytes as usize, len);
}

#[tokio::test(start_paused = true)]
async fn a_lost_repair_bundle_retransmits_nothing() {
    let mut b = framed_bridge();
    let seq = b.packer.alloc_seq();
    b.queue_bundles(smallvec::smallvec![repair_bundle(seq, 100)]);
    assert!(b.drive_pending_bundles());
    submit_tiles(&mut b, 15);
    send_all(&mut b);
    let tile_seqs: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .skip(1)
        .map(|f| frame_seq(f))
        .collect();
    b.stamp_departure(seq, now_std());
    for s in &tile_seqs {
        b.stamp_departure(*s, now_std());
    }
    // The tiles after it arrive; the repair is overtaken by K_PKT of them.
    ack_v4(&mut b, &tile_seqs);
    let before = b.reliable_emitter.emission_queue_len();

    b.expire_transmissions(now_std());

    assert_eq!(b.instr.losses_declared, 1, "premise: the verdict happened");
    assert_eq!(b.instr.loss_retx_requested, 0, "no record, no retransmit");
    let mut n = 0;
    b.reliable_emitter
        .tick_loss_budgeted(now_std(), usize::MAX, usize::MAX, &mut n);
    assert_eq!(n, 0);
    assert_eq!(b.reliable_emitter.emission_queue_len(), before);
}

const GOLDEN_FRAMES: usize = 12;
const GOLDEN_DIGEST: u64 = 0x7ca8_58bd_97c7_010f;

/// FNV-1a over every sent frame, length-prefixed. Bytes 12..16 of a record
/// are the datagram header's wall-clock `timestamp_us`; they are zeroed so the
/// digest does not depend on when the test ran.
fn frames_digest(frames: &[Vec<u8>]) -> (usize, u64) {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut eat = |bytes: &[u8]| {
        for &x in bytes {
            h ^= u64::from(x);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    };
    for f in frames {
        let mut f = f.clone();
        if let Frame::Plain(p) = parse(&f).expect("a frame") {
            let base = f.as_ptr() as usize;
            let spans: Vec<(usize, usize)> = records(p.records)
                .map(|r| {
                    let r = r.unwrap();
                    (r.as_ptr() as usize - base, r.len())
                })
                .collect();
            for (off, len) in spans {
                if len >= 16 {
                    f[off + 12..off + 16].fill(0);
                }
            }
        }
        eat(&(f.len() as u32).to_be_bytes());
        eat(&f);
    }
    (frames.len(), h)
}

/// Knob-off byte identity (R6-d D2 step 0). The scene is scripted and the
/// digest was recorded at the R6-c tip (d494b094), before any block code
/// existed. With the knob off every later step must reproduce it exactly.
#[tokio::test]
async fn knob_off_sent_frames_match_the_r6c_golden() {
    let mut b = framed_bridge();
    submit_tiles(&mut b, 10);
    let now = now_std();
    let template = bytes::Bytes::from(vec![7u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter.submit_h264_pad(&template, probe, now);
    submit_tiles(&mut b, 4);
    send_all(&mut b);
    b.emit_frame_dimensions(3, 0, 160, 96);
    b.send_pong_framed(H, &[0x05, 1, 2, 3]);
    submit_tiles(&mut b, 7);
    send_all(&mut b);
    let (n, digest) = frames_digest(&b.sent_frames_for_test);
    println!("GOLDEN frames={n} digest={digest:#018x}");
    assert_eq!((n, digest), (GOLDEN_FRAMES, GOLDEN_DIGEST));
}

/// Adaptive mode with nothing live is the knob off, byte for byte.
#[tokio::test]
async fn adaptive_with_no_live_params_matches_the_r6c_golden() {
    let mut b = framed_bridge();
    b.set_fec_mode_for_test(crate::config::FecMode::Adaptive);
    assert!(b.fec_machinery() && b.fec_live.is_none(), "premise");
    submit_tiles(&mut b, 10);
    let now = now_std();
    let template = bytes::Bytes::from(vec![7u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter.submit_h264_pad(&template, probe, now);
    submit_tiles(&mut b, 4);
    send_all(&mut b);
    b.emit_frame_dimensions(3, 0, 160, 96);
    b.send_pong_framed(H, &[0x05, 1, 2, 3]);
    submit_tiles(&mut b, 7);
    send_all(&mut b);
    let (n, digest) = frames_digest(&b.sent_frames_for_test);
    println!("GOLDEN frames={n} digest={digest:#018x}");
    assert_eq!((n, digest), (GOLDEN_FRAMES, GOLDEN_DIGEST));
}

/// Adaptive mode on a clean link (loss reports with no loss arrive, the
/// controller runs) is the knob off, byte for byte.
#[tokio::test]
async fn adaptive_after_clean_loss_reports_matches_the_r6c_golden() {
    let mut b = framed_bridge();
    b.set_fec_mode_for_test(crate::config::FecMode::Adaptive);
    assert!(b.fec_machinery() && b.fec_live.is_none(), "premise");
    for _ in 0..3 {
        let lr = ghostframe_protocol::fec::feedback::LossReport {
            received: 5000,
            ..Default::default()
        }
        .encode();
        b.dispatch_inbound_datagram(H, &lr);
    }
    assert_eq!(
        b.fec_adapt
            .as_ref()
            .expect("adaptive")
            .estimator()
            .reports_seen(),
        3,
        "premise: the controller saw the reports"
    );
    assert!(b.fec_live.is_none());
    submit_tiles(&mut b, 10);
    let now = now_std();
    let template = bytes::Bytes::from(vec![7u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter.submit_h264_pad(&template, probe, now);
    submit_tiles(&mut b, 4);
    send_all(&mut b);
    b.emit_frame_dimensions(3, 0, 160, 96);
    b.send_pong_framed(H, &[0x05, 1, 2, 3]);
    submit_tiles(&mut b, 7);
    send_all(&mut b);
    let (n, digest) = frames_digest(&b.sent_frames_for_test);
    println!("GOLDEN frames={n} digest={digest:#018x}");
    assert_eq!((n, digest), (GOLDEN_FRAMES, GOLDEN_DIGEST));
}
