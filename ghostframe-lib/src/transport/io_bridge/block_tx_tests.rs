//! The block send side with the knob on (R6-d task D4b). Same rig as
//! `bundle_tx_tests`: a unit bridge with a test wire, production code above it.

use super::*;
use crate::config::FecBlockParams;
use crate::transport::reliable_emitter::cache::ProbeTag;
use crate::transport::reliable_emitter::EmitKey;
use ghostframe_client_core::fec_rx::BlockStore;
use ghostframe_protocol::fec::frame::{parse, Frame};
use tokio::net::UnixStream;

const H: ConnectionHandle = ConnectionHandle(3);
const RAW: usize = 1000;
const TILE_LEN: usize = 300;

fn blocks_bridge(k_max: u8, t_ms: u8, r: u8) -> IoBridge {
    let (ours, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let mut b = IoBridge::new_with_stream_for_test(ours, server);
    b.wt_sessions.insert(H, WebTransportServer::default());
    b.last_hello_from = Some(H);
    b.test_wire = Some(bundle_tx::TestWire::default());
    b.forced_raw_datagram_size_for_test = Some(RAW);
    b.set_fec_blocks_for_test(Some(FecBlockParams { k_max, t_ms, r }));
    b
}

fn submit_tiles(b: &mut IoBridge, n: u32, probe: Option<ProbeTag>) {
    let now = now_std();
    for i in 0..n {
        let mut dg = vec![0u8; TILE_LEN];
        dg[0] = 0x80;
        dg[16..20].copy_from_slice(&i.to_be_bytes());
        b.reliable_emitter
            .submit_one(EmitKey::new(100 + i, 1, 2, 0), dg.into(), now, probe, now);
    }
}

fn kinds(frames: &[Vec<u8>]) -> Vec<&'static str> {
    frames
        .iter()
        .map(|f| match parse(f).expect("a frame") {
            Frame::Source(_) => "S",
            Frame::Repair(_) => "R",
            Frame::Plain(_) => "P",
        })
        .collect()
}

/// A frame of tiles through the drain and the Empty close, no admission gate.
fn send_and_close(b: &mut IoBridge) {
    b.drain_reliable_emitter(now_std());
    b.close_block_on_empty();
    b.service_pending_repairs();
}

#[tokio::test]
async fn tiles_leave_as_sources_and_an_empty_close_sends_repairs_the_real_block_store_decodes() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let frames = b.sent_frames_for_test.clone();
    assert_eq!(kinds(&frames), ["S", "S", "S", "S", "R", "R"]);
    assert_eq!(b.block_tx.closed_empty, 1);
    assert_eq!(b.block_tx.repairs_sent, 2);
    assert_eq!(b.block_table.open_blocks(), 1);
    // LAST is on the closing source only.
    let lasts: Vec<bool> = frames
        .iter()
        .filter_map(|f| match parse(f).unwrap() {
            Frame::Source(s) => Some(s.last),
            _ => None,
        })
        .collect();
    assert_eq!(lasts, [false, false, false, true]);

    let want: std::collections::BTreeSet<Vec<u8>> =
        b.sent_log_for_test.iter().map(|d| d.to_vec()).collect();
    assert_eq!(want.len(), 10, "premise: ten distinct records");
    for dropped in 0..4 {
        let mut store = BlockStore::new();
        let mut got = std::collections::BTreeSet::new();
        for (i, f) in frames.iter().enumerate() {
            if i == dropped {
                continue;
            }
            for r in store.on_datagram(f, 1_000).records {
                got.insert(r.bytes);
            }
        }
        assert_eq!(got, want, "source {dropped} dropped: every record rebuilt");
    }
}

#[tokio::test]
async fn a_pump_that_stops_on_tokens_does_not_close_and_the_block_closes_on_t_after() {
    let mut b = blocks_bridge(64, 5, 1);
    submit_tiles(&mut b, 3, None);
    b.forced_fifo_depth_for_test = Some(0);
    let now = now_std();
    b.admission.refill(now, 1e6, 1500);
    let have = b.admission.available();
    // Less than MIN_PULL_BYTES once the three records are paid for.
    assert!(b.admission.try_take(have - 3 * (TILE_LEN + 2) - 100));
    b.pump_admission();
    assert_eq!(b.block_tx.closed_empty, 0, "a Tokens stop never closes");
    assert_eq!(b.block_tx.repairs_sent, 0);
    assert_eq!(kinds(&b.sent_frames_for_test), ["S"]);
    // Tokens again, and the pump runs dry: it has no open bundle left to mark
    // LAST (every pump flushed), so the block closes on T.
    std::thread::sleep(std::time::Duration::from_millis(30));
    for _ in 0..3 {
        b.pump_admission();
    }
    assert_eq!(b.block_tx.closed_empty, 0);
    b.service_blocks(now_std());
    assert_eq!(b.block_tx.closed_time, 1);
    assert_eq!(kinds(&b.sent_frames_for_test), ["S", "R"]);
}

#[tokio::test]
async fn an_empty_close_whose_repair_gets_tokens_arms_a_wake_and_the_repair_leaves_when_it_fires() {
    let mut b = blocks_bridge(64, 5, 1);
    submit_tiles(&mut b, 3, None);
    b.forced_fifo_depth_for_test = Some(0);
    let now = now_std();
    b.admission.refill(now, 1e6, 1500);
    let have = b.admission.available();
    // 600 tokens after the records: above MIN_PULL_BYTES, below the repair.
    assert!(b.admission.try_take(have - 3 * (TILE_LEN + 2) - 600));
    b.pump_admission();
    assert_eq!(b.block_tx.closed_empty, 1, "the pump ran dry and closed");
    assert_eq!(b.pending_repairs.len(), 1, "the repair waits for tokens");
    assert_eq!(kinds(&b.sent_frames_for_test), ["S"]);
    let wake = b
        .admission_wake
        .expect("the Empty close's Tokens stop armed a wake");
    assert!(wake > now);
    assert_eq!(b.instr.budget.gate_tokens, 1);
    // The wake fires: the bucket has refilled.
    std::thread::sleep(
        wake.saturating_duration_since(now_std()) + std::time::Duration::from_millis(2),
    );
    b.pump_admission();
    assert!(b.pending_repairs.is_empty());
    assert_eq!(kinds(&b.sent_frames_for_test), ["S", "R"]);
}

#[tokio::test]
async fn a_refused_source_stays_ahead_of_its_repairs_and_a_queued_repair_counts_as_unsent() {
    let mut b = blocks_bridge(2, 5, 1);
    submit_tiles(&mut b, 4, None);
    b.drain_reliable_emitter(now_std());
    assert_eq!(
        kinds(&b.sent_frames_for_test),
        ["S"],
        "one sealed, one open"
    );
    b.test_wire.as_mut().unwrap().blocked = true;
    b.close_block_on_empty();
    // The second source closed the block at k_max and quinn refused it: the
    // repair is queued, not sealed, and not ahead of it.
    assert_eq!(b.pending_bundles.len(), 1);
    assert_eq!(b.pending_repairs.len(), 1);
    assert_eq!(b.service_pending_repairs(), pacing::Stop::Rejected);
    assert_eq!(b.pending_bundles.len(), 1);
    assert_eq!(b.pending_repairs.len(), 1);
    b.test_wire.as_mut().unwrap().blocked = false;
    assert_eq!(b.service_pending_repairs(), pacing::Stop::Empty);
    // `sent_frames_for_test` logs refused offers too: the repair is last.
    let k = kinds(&b.sent_frames_for_test);
    assert_eq!(k.last(), Some(&"R"));
    assert!(k[..k.len() - 1].iter().all(|x| *x == "S"), "{k:?}");

    // A repair standing in `pending_bundles` is counted by the depth gate.
    let mut b = blocks_bridge(2, 5, 1);
    submit_tiles(&mut b, 6, None);
    b.drain_reliable_emitter(now_std());
    b.test_wire.as_mut().unwrap().blocked = true;
    submit_tiles(&mut b, 0, None);
    // A new block's repair, queued behind a refusal.
    b.pending_repairs.push_back(block_tx::RepairJob {
        bundle_seq: 999,
        bytes: vec![0u8; 400],
    });
    let before = b.unsent_bundle_bytes();
    b.service_pending_repairs();
    assert_eq!(b.pending_bundles.len(), 1);
    assert!(b.unsent_bundle_bytes() >= before + 400);
}

#[tokio::test]
async fn the_t_close_is_a_wake_deadline_and_sends_repairs_without_last() {
    let mut b = blocks_bridge(64, 5, 1);
    submit_tiles(&mut b, 3, None);
    b.drain_reliable_emitter(now_std());
    assert!(b.flush_bundles());
    assert_eq!(kinds(&b.sent_frames_for_test), ["S"]);
    let now = now_std();
    let at = b.block_t_deadline().expect("an open block arms T");
    assert!(at <= now + std::time::Duration::from_millis(6));
    assert!(b.next_wake_deadline().expect("a wake") <= at + std::time::Duration::from_millis(1));
    b.service_blocks(now + std::time::Duration::from_millis(10));
    assert_eq!(b.block_tx.closed_time, 1);
    assert_eq!(kinds(&b.sent_frames_for_test), ["S", "R"]);
    match parse(&b.sent_frames_for_test[0]).unwrap() {
        Frame::Source(s) => assert!(!s.last, "T closes without LAST"),
        _ => unreachable!(),
    }
    assert!(
        b.block_t_deadline().is_none(),
        "nothing open, nothing armed"
    );
}

#[tokio::test]
async fn offered_bytes_equal_what_quinn_accepted_with_repairs() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    assert_eq!(b.block_tx.repairs_sent, 2);
    let cum = b.test_wire.as_ref().unwrap().cum;
    assert_eq!(b.instr.offered.total(), cum);
    assert!(b.instr.offered.of(WireClass::Repair) > 0);
}

#[tokio::test]
async fn retransmissions_pads_and_control_are_plain_and_a_probe_record_is_a_lone_source() {
    use ghostframe_protocol::protocol::FrameHeader;
    let mut b = blocks_bridge(64, 5, 1);
    let h264 = |ws: u32| {
        let mut dg = Vec::new();
        FrameHeader {
            frame_seq: 5,
            frag_idx: ws as u16,
            frag_total: 4,
            wire_seq: ws,
            timestamp_us: 0,
            flags: 0,
            slots: 0,
            ref_frame_seq: 0,
        }
        .encode(&mut dg);
        dg.resize(120, 0);
        dg
    };
    b.send_counted_framed(&h264(70), WireClass::LossRetransmit, Some(70));
    b.send_counted_framed(&h264(71), WireClass::NackRepair, Some(71));
    assert!(b.flush_bundles());
    let template = bytes::Bytes::from(vec![7u8; 100]);
    let probe = ProbeTag {
        id: 1,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    b.reliable_emitter
        .submit_h264_pad(&template, probe, now_std());
    b.drain_reliable_emitter(now_std());
    b.emit_frame_dimensions(3, 0, 160, 96);
    b.send_pong_framed(H, &[0x05, 1, 2, 3]);
    assert!(b.flush_bundles());
    assert!(
        kinds(&b.sent_frames_for_test).iter().all(|k| *k == "P"),
        "{:?}",
        kinds(&b.sent_frames_for_test)
    );
    assert_eq!(b.block_tx.repairs_sent, 0);
    assert_eq!(b.packer.take_closed().len(), 0);

    // A probe-tagged first send is a SOURCE alone in its bundle, and the
    // cluster's byte count uses the SOURCE header.
    let n = b.sent_frames_for_test.len();
    let tag = ProbeTag {
        id: 2,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    submit_tiles(&mut b, 1, Some(tag));
    b.drain_reliable_emitter(now_std());
    assert!(b.flush_bundles());
    let f = &b.sent_frames_for_test[n];
    assert!(matches!(parse(f).unwrap(), Frame::Source(_)));
    assert_eq!(
        f.len(),
        crate::transport::fec_tx::packer::Packer::lone_bundle_len(TILE_LEN, b.probe_bundle_kind())
    );
}

// ---- D5: the receive side ------------------------------------------------

use ghostframe_protocol::fec::feedback::{BundleAck, BundleAckBatch, RepairAsk, RepairRequest};
use ghostframe_protocol::fec::repair_status::{DeclineReason, StatusRecord};
use std::time::Duration;

fn ack_ids(b: &mut IoBridge, ids: &[u32]) {
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

fn ask(b: &mut IoBridge, a: RepairAsk) {
    b.dispatch_repair_request_datagram(&RepairRequest { entries: vec![a] }.encode());
}

/// `(block_id, source bundle_seqs, records per source)` of the first block.
fn first_block(frames: &[Vec<u8>]) -> (u16, Vec<u32>, Vec<usize>) {
    let mut id = 0;
    let (mut seqs, mut counts) = (Vec::new(), Vec::new());
    for f in frames {
        if let Frame::Source(s) = parse(f).unwrap() {
            id = s.block_id;
            seqs.push(s.bundle_seq);
            counts.push(ghostframe_protocol::fec::frame::records(s.records).count());
        }
    }
    (id, seqs, counts)
}

fn repair_rows(frames: &[Vec<u8>]) -> Vec<u8> {
    frames
        .iter()
        .filter_map(|f| match parse(f).unwrap() {
            Frame::Repair(r) => Some(r.repair_idx),
            _ => None,
        })
        .collect()
}

/// What the pump does with queued loss-path work: turn it into emissions,
/// then send them.
fn send_retransmits(b: &mut IoBridge) {
    let mut n = 0;
    b.reliable_emitter
        .tick_loss_budgeted(now_std(), usize::MAX, usize::MAX, &mut n);
    b.drain_reliable_emitter(now_std());
    b.flush_bundles();
}

fn converge_rtt(b: &mut IoBridge, ms: u64) {
    for _ in 0..40 {
        b.reliable_emitter.rtt.on_sample(Duration::from_millis(ms));
    }
}

#[tokio::test]
async fn a_repair_request_gets_fresh_rows_the_real_block_store_decodes() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let frames = b.sent_frames_for_test.clone();
    let (id, _, _) = first_block(&frames);
    let want: std::collections::BTreeSet<Vec<u8>> =
        b.sent_log_for_test.iter().map(|d| d.to_vec()).collect();
    // The client holds source 0 and both planned repairs (3 of k = 4).
    let mut store = BlockStore::new();
    let mut got = std::collections::BTreeSet::new();
    for i in [0usize, 4, 5] {
        for r in store.on_datagram(&frames[i], 1_000).records {
            got.insert(r.bytes);
        }
    }
    assert_ne!(got, want, "premise: not yet decodable");
    ask(
        &mut b,
        RepairAsk {
            block_id: id,
            have: 3,
            missing: 1,
            first_new_idx: 2,
        },
    );
    b.service_pending_repairs();
    let after = b.sent_frames_for_test.clone();
    assert_eq!(repair_rows(&after[6..]), [2], "one fresh row, idx 2");
    let Frame::Repair(r) = parse(&after[6]).unwrap() else {
        panic!("repair")
    };
    assert_eq!((r.k, r.r), (4, 2), "the block's own k and r");
    for rec in store.on_datagram(&after[6], 1_000).records {
        got.insert(rec.bytes);
    }
    assert_eq!(got, want, "the extra row completed the block");
    assert_eq!(b.block_tx.requests_served, 1);
    assert_eq!(b.block_tx.request_rows_sent, 1);
}

#[tokio::test]
async fn a_request_past_r_plus_4_rows_or_for_an_unknown_block_sends_nothing() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let (id, _, _) = first_block(&b.sent_frames_for_test.clone());
    ask(
        &mut b,
        RepairAsk {
            block_id: id,
            have: 1,
            missing: 20,
            first_new_idx: 0,
        },
    );
    b.service_pending_repairs();
    assert_eq!(b.block_tx.request_rows_sent, 6, "capped at r + 4");
    assert_eq!(b.block_table.stats().request_over_cap, 1);
    let sent = b.sent_frames_for_test.len();
    ask(
        &mut b,
        RepairAsk {
            block_id: id,
            have: 1,
            missing: 3,
            first_new_idx: 0,
        },
    );
    ask(
        &mut b,
        RepairAsk {
            block_id: id.wrapping_add(500),
            have: 1,
            missing: 3,
            first_new_idx: 0,
        },
    );
    b.service_pending_repairs();
    assert_eq!(b.sent_frames_for_test.len(), sent, "no more rows");
    assert_eq!(b.block_table.stats().request_unknown, 1);
}

#[tokio::test]
async fn declined_hands_off_every_unaccounted_source_whatever_the_bitmap_says() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let frames = b.sent_frames_for_test.clone();
    let (id, seqs, counts) = first_block(&frames);
    ack_ids(&mut b, &seqs[..1]);
    // An empty bitmap: the client did not know k, so it names nothing.
    b.handle_status_record(StatusRecord::Declined {
        block_id: id,
        reason: DeclineReason::Pruned,
        k: 4,
        missing: vec![],
    });
    let owed: usize = counts[1..].iter().sum();
    assert!(owed > 0, "premise");
    assert_eq!(b.block_tx.handoffs as usize, owed);
    assert_eq!(b.block_tx.handoff_queued as usize, owed);
    assert_eq!(b.block_table.open_blocks(), 0);
    // They leave as PLAIN frames through the ordinary drain.
    send_retransmits(&mut b);
    let plain = kinds(&b.sent_frames_for_test[6..])
        .iter()
        .filter(|k| **k == "P")
        .count();
    assert!(plain > 0, "hand-off retransmits are PLAIN frames");
    assert_eq!(b.reliable_emitter.stats.handoff_retransmits as usize, owed);
}

#[tokio::test]
async fn a_block_whose_symbols_all_vanish_hands_off_at_its_deadline() {
    let mut b = blocks_bridge(64, 5, 2);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let (_, _, counts) = first_block(&b.sent_frames_for_test.clone());
    let total: usize = counts.iter().sum();
    let t0 = now_std();
    b.service_blocks(t0 + Duration::from_millis(5));
    assert_eq!(b.block_tx.handoffs, 0, "before the deadline nothing moves");
    b.service_blocks(t0 + Duration::from_secs(2));
    assert!(b.block_tx.handoffs > 0, "the release is alive");
    assert_eq!(b.block_tx.handoffs as usize, total);
    assert_eq!(b.block_table.open_blocks(), 0);
    send_retransmits(&mut b);
    let n_plain = kinds(&b.sent_frames_for_test)
        .iter()
        .filter(|k| **k == "P")
        .count();
    assert!(n_plain > 0, "the tiles went out again, PLAIN");
}

#[tokio::test]
async fn t_lr_sends_one_repair_and_does_not_re_arm() {
    let mut b = blocks_bridge(64, 5, 2);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let t0 = now_std();
    b.service_blocks(t0 + Duration::from_millis(5));
    assert_eq!(b.block_table.stats().t_lr_fired, 0);
    b.service_blocks(t0 + Duration::from_millis(85));
    assert_eq!(b.block_table.stats().t_lr_fired, 1);
    assert_eq!(repair_rows(&b.sent_frames_for_test), [0, 1, 2]);
    assert_eq!(b.block_tx.handoffs, 0, "still inside the deadline");
    b.service_blocks(t0 + Duration::from_millis(90));
    assert_eq!(repair_rows(&b.sent_frames_for_test), [0, 1, 2], "once only");
}

#[tokio::test]
async fn declined_for_a_block_still_sending_closes_the_packers_open_block() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 5, None);
    b.drain_reliable_emitter(now_std());
    b.flush_bundles();
    let id = b.packer.open_block_id().expect("a block is open");
    b.handle_status_record(StatusRecord::Declined {
        block_id: id,
        reason: DeclineReason::Pruned,
        k: 0,
        missing: vec![],
    });
    assert_eq!(b.packer.open_block_id(), None, "no later source joins it");
    assert_eq!(b.block_tx.closed_declined, 1);
    assert!(b.block_tx.handoffs > 0);
    assert_eq!(b.block_tx.repairs_sent + b.pending_repairs.len() as u64, 0);
    // A new source starts a new block.
    submit_tiles(&mut b, 1, None);
    b.drain_reliable_emitter(now_std());
    assert_ne!(b.packer.open_block_id(), Some(id));
}

#[tokio::test]
async fn an_mtu_close_with_no_repairs_hands_off_at_once() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 6, None);
    b.drain_reliable_emitter(now_std());
    b.flush_bundles();
    b.forced_raw_datagram_size_for_test = Some(200);
    assert!(b.packer.close_now());
    b.process_closed_blocks(Some(block_tx::CloseWhy::Mtu));
    assert!(
        b.block_tx.repairs_dropped_mtu > 0,
        "premise: no repair fits"
    );
    assert!(b.block_tx.handoffs > 0, "nothing to wait for: handed off");
    assert_eq!(b.block_table.open_blocks(), 0);
}

// ---- D6: protected retransmit rules --------------------------------------

/// Tiles at column `x`, so a second batch never supersedes the first.
fn submit_tiles_at(b: &mut IoBridge, x: u8, n: u32) {
    let now = now_std();
    for i in 0..n {
        let mut dg = vec![0u8; TILE_LEN];
        dg[0] = 0x80;
        dg[16..20].copy_from_slice(&i.to_be_bytes());
        b.reliable_emitter
            .submit_one(EmitKey::new(500 + i, x, 7, 0), dg.into(), now, None, now);
    }
}

/// Every frame sent so far left for the wire (the ledger needs departures to
/// judge a loss and the PTO needs one to arm).
fn stamp_all(b: &mut IoBridge) {
    let seqs: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .map(|f| match parse(f).unwrap() {
            Frame::Source(s) => s.bundle_seq,
            Frame::Repair(r) => r.bundle_seq,
            Frame::Plain(p) => p.bundle_seq,
        })
        .collect();
    for q in seqs {
        b.stamp_departure(q, now_std());
    }
}

/// A closed block of 30 tiles in several sources; every source but the first
/// is acknowledged, so the first is lost by the packet threshold (`expire`)
/// and the block is neither complete nor deferred.
fn block_with_one_lost_source() -> (IoBridge, u16, Vec<u32>) {
    let mut b = blocks_bridge(64, 5, 2);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 30, None);
    send_and_close(&mut b);
    stamp_all(&mut b);
    let (id, seqs, _) = first_block(&b.sent_frames_for_test.clone());
    assert!(
        seqs.len() >= 6,
        "premise: several sources, got {}",
        seqs.len()
    );
    ack_ids(&mut b, &seqs[1..]);
    (b, id, seqs)
}

#[tokio::test]
async fn a_lost_protected_tile_source_is_not_retransmitted_by_the_loss_path() {
    let (mut b, _, seqs) = block_with_one_lost_source();
    b.expire_transmissions(now_std() + Duration::from_millis(500));
    assert!(
        b.instr.losses_declared >= 1,
        "premise: the source was declared lost"
    );
    assert_eq!(b.instr.loss_retx_requested, 0, "no loss-path retransmit");
    assert!(b.block_tx.protected_timer_suppressed > 0);
    assert_eq!(b.reliable_emitter.stats.handoff_retransmits, 0);
    // The client's RECOVERED releases it; nothing was ever resent.
    let before = b.sent_frames_for_test.len();
    b.handle_status_record(StatusRecord::Recovered(vec![seqs[0]]));
    send_retransmits(&mut b);
    assert_eq!(
        b.sent_frames_for_test.len(),
        before,
        "no retransmit ever went out"
    );
    assert_eq!(b.block_tx.handoffs, 0);
}

#[tokio::test]
async fn with_the_repairs_lost_too_the_deadline_hands_off_and_a_lost_handoff_is_retransmitted() {
    let (mut b, _, seqs) = block_with_one_lost_source();
    let t0 = now_std();
    b.expire_transmissions(t0 + Duration::from_millis(500));
    assert_eq!(b.instr.loss_retx_requested, 0);
    // The deadline: the unaccounted source goes to ARQ as a PLAIN hand-off.
    b.service_blocks(t0 + Duration::from_secs(2));
    assert!(b.block_tx.handoff_queued > 0, "the block handed off");
    let before = b.sent_frames_for_test.len();
    send_retransmits(&mut b);
    stamp_all(&mut b);
    let new = &b.sent_frames_for_test[before..];
    assert!(kinds(new).contains(&"P"), "it arrived as a PLAIN frame");
    let handoff_seq = match parse(
        new.iter()
            .find(|f| matches!(parse(f), Ok(Frame::Plain(_))))
            .unwrap(),
    )
    .unwrap()
    {
        Frame::Plain(p) => p.bundle_seq,
        _ => unreachable!(),
    };
    assert_ne!(handoff_seq, seqs[0]);
    assert_eq!(
        b.block_table.protected_block(handoff_seq),
        None,
        "no block, no protection"
    );
    // Later traffic is acknowledged past it: the hand-off is lost and the
    // ordinary loss path retransmits it, per transmission.
    submit_tiles_at(&mut b, 9, 30);
    let from = b.sent_frames_for_test.len();
    send_and_close(&mut b);
    stamp_all(&mut b);
    let later: Vec<u32> = b.sent_frames_for_test[from..]
        .iter()
        .filter_map(|f| match parse(f).unwrap() {
            Frame::Source(s) => Some(s.bundle_seq),
            _ => None,
        })
        .collect();
    assert!(later.len() >= 5, "premise: enough later sources");
    ack_ids(&mut b, &later);
    let before_req = b.instr.loss_retx_requested;
    b.expire_transmissions(t0 + Duration::from_secs(3));
    assert!(
        b.instr.loss_retx_requested > before_req,
        "a lost hand-off is retransmitted by the loss path"
    );
}

#[tokio::test]
async fn a_lost_repair_retransmits_nothing() {
    let mut b = blocks_bridge(64, 5, 2);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 30, None);
    send_and_close(&mut b);
    stamp_all(&mut b);
    let frames = b.sent_frames_for_test.clone();
    let (_, seqs, _) = first_block(&frames);
    let repair_seq = frames
        .iter()
        .find_map(|f| match parse(f).unwrap() {
            Frame::Repair(r) => Some(r.bundle_seq),
            _ => None,
        })
        .expect("a repair");
    // Everything but that one repair is acknowledged.
    let mut ids = seqs.clone();
    ids.extend(frames.iter().filter_map(|f| match parse(f).unwrap() {
        Frame::Repair(r) if r.bundle_seq != repair_seq => Some(r.bundle_seq),
        _ => None,
    }));
    // A newer bundle, acknowledged, to put the repair behind the threshold.
    submit_tiles_at(&mut b, 9, 30);
    let from = b.sent_frames_for_test.len();
    send_and_close(&mut b);
    stamp_all(&mut b);
    ids.extend(
        b.sent_frames_for_test[from..]
            .iter()
            .filter_map(|f| match parse(f).unwrap() {
                Frame::Source(s) => Some(s.bundle_seq),
                _ => None,
            }),
    );
    ack_ids(&mut b, &ids);
    let lost_before = b.instr.losses_declared;
    b.expire_transmissions(now_std() + Duration::from_millis(500));
    assert!(
        b.instr.losses_declared > lost_before,
        "premise: the repair was declared lost"
    );
    assert_eq!(b.instr.loss_retx_requested, 0, "and nothing was queued");
    assert_eq!(b.reliable_emitter.stats.handoff_retransmits, 0);
}

#[tokio::test]
async fn the_pto_answers_a_protected_candidate_with_a_repair_row_not_a_source() {
    let mut b = blocks_bridge(64, 5, 2);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 30, None);
    send_and_close(&mut b);
    stamp_all(&mut b);
    assert_eq!(repair_rows(&b.sent_frames_for_test), [0, 1]);
    let before = b.sent_frames_for_test.len();
    let t = now_std() + Duration::from_secs(2);
    b.poll_pto(t);
    assert_eq!(b.block_tx.pto_repair_rows, 1, "one block, one new row");
    assert_eq!(b.instr.pto_probes_requested, 0, "no source was probed");
    send_retransmits(&mut b);
    b.service_pending_repairs();
    b.flush_bundles();
    let new = b.sent_frames_for_test[before..].to_vec();
    assert_eq!(kinds(&new), ["R"], "a repair, and no source or plain frame");
    assert_eq!(repair_rows(&new), [2], "the next unused row");
    assert!(b.block_tx.protected_timer_suppressed > 0);
}

#[tokio::test]
async fn the_pto_skips_a_deferred_block_and_counts_its_rows_against_the_cap() {
    let mut b = blocks_bridge(64, 5, 2);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 30, None);
    send_and_close(&mut b);
    stamp_all(&mut b);
    let frames = b.sent_frames_for_test.clone();
    let (_, seqs, _) = first_block(&frames);
    let k = seqs.len();
    // k - 1 sources and one repair: held == k, one source missing: Deferred.
    let repair_seq = frames
        .iter()
        .find_map(|f| match parse(f).unwrap() {
            Frame::Repair(r) => Some(r.bundle_seq),
            _ => None,
        })
        .unwrap();
    let mut ids: Vec<u32> = seqs[1..].to_vec();
    ids.push(repair_seq);
    assert_eq!(ids.len(), k);
    ack_ids(&mut b, &ids);
    let before = b.sent_frames_for_test.len();
    b.poll_pto(now_std() + Duration::from_secs(2));
    assert_eq!(b.block_tx.pto_repair_rows, 0, "a deferred block is skipped");
    assert_eq!(
        b.instr.pto_probes_requested, 0,
        "and its source is not resent"
    );
    send_retransmits(&mut b);
    assert_eq!(b.sent_frames_for_test.len(), before);
}
