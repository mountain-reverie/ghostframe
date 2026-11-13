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
async fn a_pump_that_stops_on_tokens_does_not_close_and_the_next_dry_pump_closes_the_block() {
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
    // LAST (every pump flushed), but the block is open and the emitter has
    // nothing left, so the dry pump closes it (Empty, not T).
    std::thread::sleep(std::time::Duration::from_millis(30));
    for _ in 0..3 {
        b.pump_admission();
    }
    assert_eq!(b.block_tx.closed_empty, 1);
    b.service_blocks(now_std());
    assert_eq!(b.block_tx.closed_time, 0);
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
        block_id: 999,
        k: 2,
        r: 1,
        row: 0,
        payload: vec![0u8; 400 - ghostframe_protocol::fec::frame::FEC_REPAIR_HEADER_LEN],
        probe: None,
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
async fn a_request_from_a_client_that_does_not_know_k_is_served_to_k() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    b.drain_reliable_emitter(now_std());
    assert!(b.flush_bundles());
    // The T close: no LAST on any source.
    b.service_blocks(now_std() + Duration::from_millis(10));
    assert_eq!(b.block_tx.closed_time, 1);
    let frames = b.sent_frames_for_test.clone();
    assert_eq!(kinds(&frames), ["S", "S", "S", "S", "R", "R"]);
    let (id, _, _) = first_block(&frames);
    let want: std::collections::BTreeSet<Vec<u8>> =
        b.sent_log_for_test.iter().map(|d| d.to_vec()).collect();
    // The two trailing sources and both repairs are lost: the client holds
    // two sources, sees neither LAST nor a repair, so it asks for one row.
    let mut store = BlockStore::new();
    let mut got = std::collections::BTreeSet::new();
    for i in [0usize, 1] {
        for r in store.on_datagram(&frames[i], 1_000).records {
            got.insert(r.bytes);
        }
    }
    assert_ne!(got, want, "premise: not yet decodable");
    ask(
        &mut b,
        RepairAsk {
            block_id: id,
            have: 2,
            missing: 1,
            first_new_idx: 0,
        },
    );
    b.service_pending_repairs();
    let after = b.sent_frames_for_test.clone();
    assert_eq!(repair_rows(&after[6..]), [2, 3], "k - have = 2 rows");
    for f in &after[6..] {
        for rec in store.on_datagram(f, 1_000).records {
            got.insert(rec.bytes);
        }
    }
    assert_eq!(got, want, "one request round decoded the block");
}

/// A repair dropped before it reaches quinn (MTU shrink, no session) leaves
/// no hole in the bundle_seq space: the client counts a hole as lost.
#[tokio::test]
async fn a_dropped_repair_leaves_no_bundle_seq_gap() {
    for no_sink in [false, true] {
        let mut b = blocks_bridge(64, 5, 2);
        submit_tiles(&mut b, 10, None);
        b.drain_reliable_emitter(now_std());
        b.close_block_on_empty();
        assert_eq!(b.pending_repairs.len(), 2, "premise: both repairs queued");
        let (_, seqs, _) = first_block(&b.sent_frames_for_test.clone());
        let last_source = *seqs.iter().max().unwrap();
        if no_sink {
            b.wt_sessions.clear();
            b.test_wire = None;
        } else {
            b.forced_raw_datagram_size_for_test = Some(250);
        }
        b.service_pending_repairs();
        assert!(b.pending_repairs.is_empty());
        assert_eq!(b.block_tx.repairs_sent, 0, "premise: nothing was sent");
        assert_eq!(
            b.packer.alloc_seq(),
            last_source + 1,
            "no bundle_seq was spent on a repair that never left (no_sink: {no_sink})"
        );
    }
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

// ---- E2: machinery versus live parameters ----------------------------------

use crate::config::FecMode;

fn adaptive_bridge() -> IoBridge {
    let mut b = blocks_bridge(64, 5, 2);
    b.set_fec_mode_for_test(FecMode::Adaptive);
    assert!(b.fec_machinery() && b.fec_live.is_none(), "premise");
    b
}

/// Tokens for a pump, no queue-depth gate.
fn open_gate(b: &mut IoBridge) {
    b.forced_fifo_depth_for_test = Some(0);
    b.admission.refill(now_std(), 1e7, 100_000);
}

#[tokio::test]
async fn setting_live_none_while_a_block_is_open_still_sends_that_blocks_repairs() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    b.drain_reliable_emitter(now_std());
    assert!(
        b.packer.open_block_id().is_some(),
        "premise: a block is open"
    );
    b.apply_fec_live(None);
    b.service_pending_repairs();
    let k = kinds(&b.sent_frames_for_test);
    assert_eq!(k.iter().filter(|x| **x == "R").count(), 2, "{k:?}");
    assert_eq!(b.block_tx.repairs_sent, 2);
    assert!(b.fec_live.is_none() && b.fec_machinery());
    // The next frame is PLAIN.
    let n = b.sent_frames_for_test.len();
    submit_tiles(&mut b, 3, None);
    b.drain_reliable_emitter(now_std());
    b.flush_bundles();
    assert!(kinds(&b.sent_frames_for_test[n..])
        .iter()
        .all(|x| *x == "P"));
}

#[tokio::test]
async fn with_live_none_and_machinery_on_a_probe_record_is_plain_and_a_pump_never_closes() {
    let mut b = adaptive_bridge();
    // The cluster's byte count (emit.rs and frame.rs read this) is the PLAIN
    // bundle's while nothing is live, the SOURCE's once something is.
    use crate::transport::fec_tx::packer::BundleKind;
    assert_eq!(b.probe_bundle_kind(), BundleKind::Plain);
    b.apply_fec_live(Some(FecBlockParams {
        k_max: 64,
        t_ms: 5,
        r: 2,
    }));
    assert_eq!(b.probe_bundle_kind(), BundleKind::Source);
    b.apply_fec_live(None);
    assert_eq!(b.probe_bundle_kind(), BundleKind::Plain);
    let tag = ProbeTag {
        id: 2,
        min_probes: 1,
        min_bytes: 1,
        bytes_sent_before: 0,
    };
    submit_tiles(&mut b, 1, Some(tag));
    open_gate(&mut b);
    for _ in 0..3 {
        b.pump_admission();
    }
    assert_eq!(kinds(&b.sent_frames_for_test), ["P"]);
    assert_eq!(
        b.sent_frames_for_test[0].len(),
        crate::transport::fec_tx::packer::Packer::lone_bundle_len(
            TILE_LEN,
            crate::transport::fec_tx::packer::BundleKind::Plain
        ),
        "the byte count of a probe cluster is the PLAIN one"
    );
    assert_eq!(b.block_tx.closed_empty, 0, "a pump never closes a block");
    assert_eq!(b.block_table.open_blocks(), 0);
}

#[tokio::test]
async fn a_repair_queued_before_the_switch_to_none_leaves_and_old_acks_are_still_handled() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    b.drain_reliable_emitter(now_std());
    b.close_block_on_empty(); // repairs queued, not yet sent
    assert!(!b.pending_repairs.is_empty(), "premise");
    b.fec_live = None;
    b.packer.set_blocks(None);
    open_gate(&mut b);
    b.pump_admission();
    assert!(b.pending_repairs.is_empty(), "the repair left");
    assert_eq!(repair_rows(&b.sent_frames_for_test).len(), 2);
    // A late ACK of every source still closes the block.
    let (_, seqs, _) = first_block(&b.sent_frames_for_test.clone());
    assert_eq!(b.block_table.open_blocks(), 1, "premise");
    ack_ids(&mut b, &seqs);
    assert_eq!(b.block_table.open_blocks(), 0, "the ACK hook ran");
}

#[tokio::test]
async fn a_declined_for_an_old_block_is_handled_with_live_none() {
    let mut b = blocks_bridge(64, 5, 2);
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let (id, _, counts) = first_block(&b.sent_frames_for_test.clone());
    b.apply_fec_live(None);
    b.handle_status_record(StatusRecord::Declined {
        block_id: id,
        reason: DeclineReason::Pruned,
        k: 4,
        missing: vec![],
    });
    let owed: usize = counts.iter().sum();
    assert!(owed > 0, "premise");
    assert_eq!(b.block_tx.handoffs as usize, owed);
    assert_eq!(b.block_table.open_blocks(), 0);
}

/// What the pump tail does after a frame, minus the admission gate.
fn pump_tail(b: &mut IoBridge) {
    b.drain_reliable_emitter(now_std());
    if b.fec_live.is_some() {
        b.close_block_on_empty();
    } else {
        b.flush_bundles();
    }
    b.service_pending_repairs();
}

#[tokio::test]
async fn adaptive_with_no_live_params_sends_the_bytes_the_knob_off_sends() {
    let scene = |b: &mut IoBridge| {
        let tag = ProbeTag {
            id: 3,
            min_probes: 1,
            min_bytes: 1,
            bytes_sent_before: 0,
        };
        submit_tiles(b, 10, Some(tag));
        pump_tail(b);
        submit_tiles(b, 7, None);
        pump_tail(b);
    };
    let mut off = blocks_bridge(64, 5, 2);
    off.set_fec_mode_for_test(FecMode::Off);
    let mut ada = adaptive_bridge();
    scene(&mut off);
    scene(&mut ada);
    // Bytes 12..16 of a record are a wall-clock timestamp.
    let mask = |frames: &[Vec<u8>]| -> Vec<Vec<u8>> {
        frames
            .iter()
            .map(|f| {
                let mut f = f.clone();
                if let Frame::Plain(p) = parse(&f).expect("a frame") {
                    let base = f.as_ptr() as usize;
                    let spans: Vec<(usize, usize)> =
                        ghostframe_protocol::fec::frame::records(p.records)
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
                f
            })
            .collect()
    };
    let (a, b) = (
        mask(&off.sent_frames_for_test),
        mask(&ada.sent_frames_for_test),
    );
    let (a, b) = (&a, &b);
    assert!(a.len() >= 4, "premise: frames were sent, got {}", a.len());
    assert!(kinds(a).iter().all(|k| *k == "P"));
    assert_eq!(
        a, b,
        "adaptive with no live params is byte-identical to off"
    );
}

#[tokio::test]
async fn fixed_mode_is_machinery_and_live_at_once() {
    let b = blocks_bridge(64, 5, 2);
    assert!(b.fec_machinery());
    assert_eq!(b.fec_live, b.fec_mode.initial_live());
    assert!(b.fec_live.is_some());
}

// ---- E5: the adaptive controller wired into the bridge ----

fn send_report(b: &mut IoBridge, received: u16, lost: u16, hist: [u16; 8]) {
    use ghostframe_protocol::fec::feedback::LossReport;
    let lr = LossReport {
        received,
        lost_pre_fec: lost,
        loss_run_hist: hist,
        ..LossReport::default()
    }
    .encode();
    b.dispatch_inbound_datagram(H, &lr);
}

fn count(frames: &[Vec<u8>], kind: &str) -> usize {
    kinds(frames).iter().filter(|k| **k == kind).count()
}

#[tokio::test]
async fn a_report_above_the_floor_makes_the_next_block_carry_r_for_k_repairs_the_store_decodes() {
    let mut b = adaptive_bridge();
    send_report(&mut b, 950, 50, [50, 0, 0, 0, 0, 0, 0, 0]);
    assert_eq!(b.fec_adapt.as_ref().unwrap().estimator().reports_seen(), 1);
    assert!(b.fec_live.is_some(), "engaged by a 5 % report");
    submit_tiles(&mut b, 40, None);
    send_and_close(&mut b);
    let frames = b.sent_frames_for_test.clone();
    let k = count(&frames, "S");
    assert!(k > 4, "premise: a real block, k = {k}");
    let est = b.fec_adapt.as_ref().unwrap().estimator();
    let want = crate::transport::fec_tx::adapt_size::RSizer::new()
        .r_for(k as u32, est.p_hat(), est.b_hat(), 0)
        .r;
    assert!(want >= 1);
    assert_eq!(count(&frames, "R"), usize::from(want), "r_for(k) repairs");
    let want_set: std::collections::BTreeSet<Vec<u8>> =
        b.sent_log_for_test.iter().map(|d| d.to_vec()).collect();
    let mut store = BlockStore::new();
    let mut got = std::collections::BTreeSet::new();
    for (i, f) in frames.iter().enumerate() {
        if i == 0 {
            continue;
        }
        for r in store.on_datagram(f, 1_000).records {
            got.insert(r.bytes);
        }
    }
    assert_eq!(got, want_set, "source 0 dropped: every record rebuilt");
}

#[tokio::test]
async fn a_report_below_the_floor_leaves_every_frame_plain() {
    let mut b = adaptive_bridge();
    send_report(&mut b, 1000, 0, [0; 8]);
    assert_eq!(b.fec_adapt.as_ref().unwrap().estimator().reports_seen(), 1);
    assert!(b.fec_live.is_none());
    submit_tiles(&mut b, 10, None);
    send_and_close(&mut b);
    let k = kinds(&b.sent_frames_for_test);
    assert!(!k.is_empty() && k.iter().all(|x| *x == "P"), "{k:?}");
}

#[tokio::test]
async fn a_change_applied_mid_block_loses_no_repair() {
    let mut b = adaptive_bridge();
    send_report(&mut b, 950, 50, [50, 0, 0, 0, 0, 0, 0, 0]);
    assert!(b.fec_live.is_some(), "premise: engaged");
    submit_tiles(&mut b, 10, None);
    b.drain_reliable_emitter(now_std());
    assert!(
        b.packer.open_block_id().is_some(),
        "premise: a block is open"
    );
    assert_eq!(
        count(&b.sent_frames_for_test, "R"),
        0,
        "premise: no repair yet"
    );
    // The loss clears and the dwell passes: the floor releases to None while
    // the block is open (the harshest change).
    send_report(&mut b, 60_000, 0, [0; 8]);
    send_report(&mut b, 60_000, 0, [0; 8]);
    let now_us = b.reliable_emitter.emit_us64(now_std());
    b.adapt_step(now_us + 5_100_000);
    assert!(b.fec_live.is_none(), "released");
    b.service_pending_repairs();
    assert!(
        count(&b.sent_frames_for_test, "R") >= 1,
        "the open block closed under its old parameters and its repair left"
    );
}

#[tokio::test]
async fn adapt_tick_releases_on_a_quiet_link_after_the_dwell() {
    let mut b = adaptive_bridge();
    send_report(&mut b, 100, 5, [5, 0, 0, 0, 0, 0, 0, 0]);
    assert!(b.fec_live.is_some(), "premise: engaged");
    send_report(&mut b, 60_000, 0, [0; 8]);
    send_report(&mut b, 60_000, 0, [0; 8]);
    assert!(b.fec_live.is_some(), "inside the dwell the loss stays");
    let now_us = b.reliable_emitter.emit_us64(now_std());
    b.adapt_step(now_us + 1_000_000);
    assert!(b.fec_live.is_some(), "still inside the dwell");
    b.adapt_step(now_us + 5_100_000);
    assert!(b.fec_live.is_none(), "released with no report");
    assert_eq!(b.packer.open_block_id(), None);
}

#[tokio::test]
async fn five_percent_loss_engages_even_with_congestion_events_rising_and_a_standing_queue_gates_it(
) {
    // A rise of quinn's congestion events alone is not congestion.
    let mut a = adaptive_bridge();
    for _ in 0..4 {
        send_report(&mut a, 1000, 0, [0; 8]);
        a.transport_probe.congestion_events += 3;
    }
    a.transport_probe.congestion_events += 3;
    send_report(&mut a, 950, 50, [50, 0, 0, 0, 0, 0, 0, 0]);
    let ea = a.fec_adapt.as_ref().unwrap().estimator();
    assert_eq!(ea.reports_seen(), 5);
    assert_eq!(ea.congestion_skips(), 0);
    assert!(a.fec_live.is_some(), "CE rising alone: engaged");

    // The delay signal gates the same report.
    let mut g = crate::transport::fec_tx::adapt::FecAdapt::new();
    let lr = ghostframe_protocol::fec::feedback::LossReport {
        received: 950,
        lost_pre_fec: 50,
        ..Default::default()
    };
    g.on_report(&lr, true, 1);
    assert_eq!(g.estimator().reports_seen(), 1);
    assert_eq!(g.estimator().congestion_skips(), 1, "standing queue gates");
    assert_eq!(
        g.estimator().state(),
        crate::transport::fec_tx::adapt::FloorState::Clean
    );
}

#[tokio::test]
async fn declined_blocks_move_the_trim_and_nothing_else_does() {
    use crate::transport::fec_tx::block_table::CloseReason;
    let mut b = adaptive_bridge();
    send_report(&mut b, 950, 50, [50, 0, 0, 0, 0, 0, 0, 0]);
    let a = b.fec_adapt.as_mut().unwrap();
    let before = a.r_for_block(64, 0);
    a.tick(1, std::iter::repeat_n(CloseReason::Accounted, 250));
    a.tick(
        2,
        [CloseReason::HandedOff, CloseReason::TimedOut]
            .into_iter()
            .chain([CloseReason::Accounted; 4]),
    );
    assert_eq!(
        a.r_for_block(64, 2),
        before,
        "HandedOff and TimedOut never trim"
    );
    a.tick(3, std::iter::repeat_n(CloseReason::Declined, 10));
    assert_eq!(a.r_for_block(64, 3), before + 1, "10/256 Declined trims");
}

// ---- padding as repair, part 1 (R6-e E8): the tag by bundle ----------------

fn pad_tag() -> ProbeTag {
    ProbeTag {
        id: 42,
        min_probes: 5,
        min_bytes: 6000,
        bytes_sent_before: 1234,
    }
}

/// A closed block (k sources, r = 1 planned repair, all sent), and a job for
/// one fresh pad row of it, sent through `service_pending_repairs`. Returns
/// the job's length, the `bundle_seq` it left under, and the bridge.
fn sent_pad_repair(probe: Option<ProbeTag>) -> (IoBridge, usize, u32) {
    let mut b = blocks_bridge(64, 5, 1);
    converge_rtt(&mut b, 20);
    submit_tiles(&mut b, 3, None);
    send_and_close(&mut b);
    let (id, _, _) = first_block(&b.sent_frames_for_test.clone());
    let (k, r) = b.block_table.params(id).expect("closed block");
    let rows = b.block_table.reserve_pad_rows(id, 1);
    assert_eq!(rows.len(), 1, "premise: a pad row");
    let job = block_tx::RepairJob {
        block_id: id,
        k,
        r,
        row: rows[0],
        payload: vec![0x33; 250],
        probe,
    };
    let len = job.len();
    let before = b.sent_frames_for_test.len();
    b.pending_repairs.push_back(job);
    assert_eq!(b.service_pending_repairs(), pacing::Stop::Empty);
    assert_eq!(b.sent_frames_for_test.len(), before + 1, "premise: it left");
    let Frame::Repair(rf) = parse(b.sent_frames_for_test.last().unwrap()).unwrap() else {
        panic!("a repair frame")
    };
    let seq = rf.bundle_seq;
    b.stamp_departure(seq, now_std());
    (b, len, seq)
}

#[tokio::test(start_paused = true)]
async fn a_tagged_pad_repairs_ack_is_a_tagged_bwe_sample_of_its_frame_length() {
    let (mut b, len, seq) = sent_pad_repair(Some(pad_tag()));
    assert_eq!(b.pad_repair_tags.len(), 1, "premise: the tag was recorded");
    assert!(
        b.transmission_ledger.peek(seq).unwrap().records.is_empty(),
        "premise: a repair bundle has no record to find a tag by"
    );
    b.bwe_samples_buffer.clear();
    ack_ids(&mut b, &[seq]);
    assert_eq!(b.bwe_samples_buffer.len(), 1, "premise: the ack path ran");
    let s = &b.bwe_samples_buffer[0];
    assert_eq!(s.probe, Some(pad_tag()));
    assert_eq!(s.size_bytes as usize, len);
    assert_eq!(b.pad_repair_tags.len(), 0, "the entry is released");
}

#[tokio::test(start_paused = true)]
async fn an_untagged_repairs_ack_is_unchanged() {
    let (mut b, len, seq) = sent_pad_repair(None);
    assert_eq!(b.pad_repair_tags.len(), 0, "no tag, no entry");
    b.bwe_samples_buffer.clear();
    ack_ids(&mut b, &[seq]);
    assert_eq!(b.bwe_samples_buffer.len(), 1, "premise: the ack path ran");
    assert_eq!(b.bwe_samples_buffer[0].probe, None);
    assert_eq!(b.bwe_samples_buffer[0].size_bytes as usize, len);
}

#[tokio::test(start_paused = true)]
async fn a_lost_tagged_pad_repair_pauses_the_raise_once_and_frees_its_entry() {
    let (mut b, _, seq) = sent_pad_repair(Some(pad_tag()));
    assert_eq!(b.pad_repair_tags.len(), 1, "premise");
    assert!(b.h264.raise_paused_until.is_none(), "premise: no pause yet");
    let tx = b.transmission_ledger.peek(seq).unwrap().clone();
    let now = now_std();
    b.note_h264_verdicts(&[(seq, tx.clone())], now);
    assert_eq!(b.h264.stats.pads_lost, 1);
    assert!(b.h264.raise_paused_until.is_some_and(|t| t > now));
    assert_eq!(b.pad_repair_tags.len(), 0, "the entry is dropped");
    // A second verdict on the same bundle changes nothing.
    b.note_h264_verdicts(&[(seq, tx)], now);
    assert_eq!(b.h264.stats.pads_lost, 1, "once");
}

#[tokio::test(start_paused = true)]
async fn a_lost_untagged_repair_does_not_pause_the_raise() {
    let (mut b, _, seq) = sent_pad_repair(None);
    let tx = b.transmission_ledger.peek(seq).unwrap().clone();
    b.note_h264_verdicts(&[(seq, tx)], now_std());
    assert_eq!(b.h264.stats.pads_lost, 0);
    assert!(b.h264.raise_paused_until.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_late_ack_of_a_tagged_pad_repair_frees_its_entry_and_samples_nothing() {
    let (mut b, _, seq) = sent_pad_repair(Some(pad_tag()));
    // Everything else sent is acknowledged, so the pad repair is lost by the
    // time threshold.
    stamp_all(&mut b);
    let others: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .map(|f| match parse(f).unwrap() {
            Frame::Source(s) => s.bundle_seq,
            Frame::Repair(r) => r.bundle_seq,
            Frame::Plain(p) => p.bundle_seq,
        })
        .filter(|&q| q != seq)
        .collect();
    ack_ids(&mut b, &others);
    b.expire_transmissions(now_std() + Duration::from_millis(500));
    assert!(b.instr.losses_declared >= 1, "premise: declared lost");
    assert_eq!(
        b.pad_repair_tags.len(),
        0,
        "premise: the verdict took the tag"
    );
    // Verdict routing is `note_h264_verdicts`' job and took the tag; put it
    // back to see the late ACK's own handling.
    b.pad_repair_tags.insert(seq, pad_tag());
    b.bwe_samples_buffer.clear();
    ack_ids(&mut b, &[seq]);
    assert_eq!(b.pad_repair_tags.len(), 0, "released");
    assert!(b.bwe_samples_buffer.is_empty(), "a late ack is no sample");
}

#[test]
fn the_pad_repair_tag_map_is_bounded_and_drops_the_oldest() {
    use crate::transport::reliable_emitter::h264::PAD_RING_CAP;
    let mut m = block_tx::PadRepairTags::default();
    for i in 0..(PAD_RING_CAP as u32 + 50) {
        m.insert(i, pad_tag());
        assert!(m.len() <= PAD_RING_CAP, "bounded at every step");
    }
    assert_eq!(
        m.len(),
        PAD_RING_CAP,
        "full, not empty: the cap was reached"
    );
    assert!(m.take(0).is_none(), "the oldest went");
    assert!(m.take(49).is_none(), "the 50 oldest went");
    assert!(m.take(50).is_some(), "the next oldest stayed");
    assert!(m.take(50).is_none(), "released once");
    assert_eq!(m.len(), PAD_RING_CAP - 1);
    // Releases keep room: a new insert evicts nothing.
    m.insert(10_000, pad_tag());
    assert!(m.take(51).is_some());
}
