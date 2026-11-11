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
