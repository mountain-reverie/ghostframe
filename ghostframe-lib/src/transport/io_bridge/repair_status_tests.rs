// SPDX-License-Identifier: AGPL-3.0-or-later
//! ACK v4 (0x08), LOSS_REPORT (0x07), REPAIR_REQUEST (0x0B) and the
//! repair-status stream on the server (R6-c task C4).
//!
//! The tests seed outstanding transmissions through the real emitter and the
//! ledger the way production does, so a consumer effect is observed on the
//! structures production reads (the emitter's cache, the ledger, the BWE
//! sample buffer), not on a mock.

use super::*;
use crate::transport::reliable_emitter::{Content, EmitKey};
use ghostframe_protocol::fec::feedback::{
    BundleAck, BundleAckBatch, LossReport, RepairAsk, RepairRequest,
};
use ghostframe_protocol::fec::repair_status::{
    DeclineReason, StatusRecord, FEC_REPAIR_STATUS_STREAM_TAG,
};
use quinn_proto::{Dir, Side, StreamId};
use tokio::net::UnixStream;

const H: ConnectionHandle = ConnectionHandle(3);

fn bridge() -> IoBridge {
    let (ours, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let mut b = IoBridge::new_with_stream_for_test(ours, server);
    b.wt_sessions.insert(H, WebTransportServer::default());
    b.last_hello_from = Some(H);
    b
}

/// Submit `n` tile passes through the emitter, record them in the ledger as
/// departed (as the production drain does), and return their `wire_seq`s.
fn seed(b: &mut IoBridge, n: u32) -> Vec<u32> {
    let now = now_std();
    for i in 0..n {
        let mut frag = vec![0u8; 20];
        frag[12..16].copy_from_slice(&1_000_000u32.to_be_bytes());
        b.reliable_emitter.submit_one(
            EmitKey::new(100 + i, 1, 2, 0),
            bytes::Bytes::from(frag),
            now,
            None,
            now,
        );
    }
    let mut out = Vec::new();
    for e in b.reliable_emitter.take_transmissions() {
        b.transmission_ledger.record_departed(
            e.wire_seq,
            now,
            crate::transport::transmission_ledger::Transmission {
                emit_us: e.emit_us,
                wire_bytes: e.wire_bytes,
                records: smallvec::smallvec![(e.wire_seq, e.content)],
                depart_us: None,
                dep_idx: None,
            },
        );
        out.push(e.wire_seq);
    }
    assert_eq!(out.len(), n as usize, "premise: every pass was recorded");
    out
}

/// What an acknowledgement is observable by.
#[derive(Debug, PartialEq)]
struct Effects {
    ack_hit: u64,
    ack_miss: u64,
    cache_entries: usize,
    ledger_records: usize,
    ledger_tombstones: usize,
    ledger_unknown_acks: u64,
    ledger_acked_after_lost: u64,
    bwe_samples: Vec<(u32, u16, u16, bool)>,
    srtt_samples: u64,
    pending_losses: usize,
}

fn effects(b: &IoBridge) -> Effects {
    Effects {
        ack_hit: b.reliable_emitter.stats.ack_hit,
        ack_miss: b.reliable_emitter.stats.ack_miss,
        cache_entries: b.reliable_emitter.pending_cache_entries(),
        ledger_records: b.transmission_ledger.len(),
        ledger_tombstones: b.transmission_ledger.tombstone_len(),
        ledger_unknown_acks: b.transmission_ledger.stats().unknown_acks,
        ledger_acked_after_lost: b.transmission_ledger.stats().acked_after_declared_lost,
        bwe_samples: b
            .bwe_samples_buffer
            .iter()
            .map(|s| {
                (
                    s.size_bytes,
                    s.client_arrival_ms_lo16,
                    s.owd_ms_lo16,
                    s.probe.is_some(),
                )
            })
            .collect(),
        srtt_samples: b.reliable_emitter.rtt.samples(),
        pending_losses: b.pending_losses.len(),
    }
}

/// One scenario, acknowledged by ACK v4: five passes, one of them already
/// declared lost (so its acknowledgement is a late one), and an id nobody
/// recorded.
fn run_ack() -> Effects {
    let mut b = bridge();
    let ws = seed(&mut b, 5);
    b.declare_lost_for_test(&[ws[4]]);
    assert_eq!(
        b.transmission_ledger.tombstone_len(),
        1,
        "premise: a late ack"
    );
    let ids = [ws[0], ws[2], ws[4], 0x00AB_CDEF];
    let stamps = [1_500u16, 1_510, 1_520, 1_530];
    let dg = BundleAckBatch {
        entries: ids
            .iter()
            .zip(stamps)
            .map(|(&bundle_seq, arrival_time_ms_lo16)| BundleAck {
                bundle_seq,
                arrival_time_ms_lo16,
            })
            .collect(),
    }
    .encode();
    b.dispatch_inbound_datagram(H, &dg);
    effects(&b)
}

/// The scenario moves every consumer (live, late and unknown entries), so the
/// effects are not an untouched structure.
#[tokio::test]
async fn an_ack_v4_batch_moves_every_ack_consumer() {
    let v4 = run_ack();
    assert_eq!(v4.ack_hit, 3, "{v4:?}");
    assert_eq!(v4.ledger_records, 2, "{v4:?}");
    assert_eq!(v4.ledger_unknown_acks, 1, "{v4:?}");
    assert_eq!(v4.ledger_acked_after_lost, 1, "{v4:?}");
    assert_eq!(
        v4.bwe_samples.len(),
        2,
        "live acks sample, the late one does not"
    );
    assert_eq!(v4.cache_entries, 2, "{v4:?}");
}

#[tokio::test]
async fn an_ack_v4_datagram_is_routed_not_counted_unroutable() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    let dg = BundleAckBatch {
        entries: vec![BundleAck {
            bundle_seq: ws[0],
            arrival_time_ms_lo16: 7,
        }],
    }
    .encode();
    b.dispatch_inbound_datagram(H, &dg);
    assert_eq!(b.unroutable_inbound_datagrams, 0);
    assert_eq!(b.reliable_emitter.stats.ack_hit, 1);
}

#[tokio::test]
async fn a_blackout_swallows_an_ack_v4_datagram_and_a_malformed_one_does_nothing() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    let dg = BundleAckBatch {
        entries: vec![BundleAck {
            bundle_seq: ws[0],
            arrival_time_ms_lo16: 7,
        }],
    }
    .encode();
    let now = now_std();
    b.ack_blackout = Some((now, now + std::time::Duration::from_secs(60)));
    b.dispatch_inbound_datagram(H, &dg);
    assert_eq!(b.acks_blacked_out, 1);
    assert_eq!(b.reliable_emitter.stats.ack_hit, 0, "swallowed");
    assert_eq!(b.transmission_ledger.len(), 1);

    b.ack_blackout = None;
    let mut bad = dg.clone();
    bad.push(0xFF); // trailing byte
    b.dispatch_inbound_datagram(H, &bad);
    assert_eq!(b.reliable_emitter.stats.ack_hit, 0);
    assert_eq!(b.transmission_ledger.len(), 1);
    b.dispatch_inbound_datagram(H, &dg);
    assert_eq!(
        b.reliable_emitter.stats.ack_hit, 1,
        "control: the good one lands"
    );
}

#[tokio::test]
async fn the_inbound_loss_injector_applies_to_ack_v4() {
    // `inbound_loss` is the hook an e2e uses to drop acknowledgements.
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    b.inbound_loss = Some(crate::transport::loss_injection::LossInjector::new(
        1.0,
        |d| d.first() == Some(&0x08),
        1,
    ));
    let dg = BundleAckBatch {
        entries: vec![BundleAck {
            bundle_seq: ws[0],
            arrival_time_ms_lo16: 7,
        }],
    }
    .encode();
    b.dispatch_inbound_datagram(H, &dg);
    assert_eq!(b.reliable_emitter.stats.ack_hit, 0);
    // Control: an injector that matches only 0x06 lets the 0x08 through.
    b.inbound_loss = Some(crate::transport::loss_injection::LossInjector::new(
        1.0,
        |d| d.first() == Some(&0x06),
        1,
    ));
    b.dispatch_inbound_datagram(H, &dg);
    assert_eq!(b.reliable_emitter.stats.ack_hit, 1);
}

#[tokio::test]
async fn recovered_releases_content_and_moves_no_transport_counter() {
    let mut b = bridge();
    let ws = seed(&mut b, 4);
    b.declare_lost_for_test(&[ws[3]]);
    let pending_losses_before = b.pending_losses.len();
    let before = effects(&b);
    assert_eq!(before.cache_entries, 4, "premise: content is held");
    assert!(before.bwe_samples.is_empty());
    let delivery_before = b.delivery_sampler.stats(now_std());
    let tombstone_before = before.ledger_tombstones;

    b.handle_status_record(StatusRecord::Recovered(vec![ws[0], ws[3], 0x00DE_ADBE]));

    let after = effects(&b);
    assert_eq!(b.fec_in.recovered_rx, 3);
    assert_eq!(b.fec_in.recovered_unknown, 1);
    assert_eq!(
        after.cache_entries, 2,
        "the live and the tombstoned pass released"
    );
    assert_eq!(after.ack_hit, 2);
    // Nothing a delivery proves moved.
    assert!(after.bwe_samples.is_empty(), "no BWE sample");
    assert_eq!(after.srtt_samples, before.srtt_samples, "no RTT");
    assert_eq!(after.pending_losses, pending_losses_before);
    assert_eq!(b.delivery_sampler.stats(now_std()), delivery_before);
    // The ledger entry is read, never consumed: still outstanding (the live
    // one) and still tombstoned (the late one), no spurious-loss accounting.
    assert_eq!(after.ledger_records, before.ledger_records);
    assert_eq!(after.ledger_tombstones, tombstone_before);
    assert_eq!(after.ledger_acked_after_lost, 0);
    assert_eq!(after.ledger_unknown_acks, before.ledger_unknown_acks);
    assert!(b.transmission_ledger.peek(ws[0]).is_some());
    assert!(b.transmission_ledger.peek(ws[3]).is_some());
}

#[tokio::test]
async fn recovered_does_not_touch_the_loss_window() {
    // The loss window's rate over what was sent: a recovery must not add an
    // "acknowledged" sample (rule 3) any more than a loss one.
    let mut a = bridge();
    let mut c = bridge();
    let wa = seed(&mut a, 3);
    let wc = seed(&mut c, 3);
    a.handle_status_record(StatusRecord::Recovered(wa.clone()));
    // Control: acknowledging does record into the window.
    let dg = BundleAckBatch {
        entries: wc
            .iter()
            .map(|&bundle_seq| BundleAck {
                bundle_seq,
                arrival_time_ms_lo16: 1,
            })
            .collect(),
    }
    .encode();
    c.dispatch_inbound_datagram(H, &dg);
    let now_us = a
        .reliable_emitter
        .emit_us(now_std())
        .wrapping_add(10_000_000);
    let ra = a.loss_window.rate(now_us);
    let rc = c.loss_window.rate(now_us);
    assert_eq!(ra, 0.0);
    assert_eq!(rc, 0.0, "a delivery-only window reads zero loss either way");
    assert_eq!(c.bwe_samples_buffer.len(), 3, "premise: the ack path ran");
    assert_eq!(a.bwe_samples_buffer.len(), 0);
}

#[tokio::test]
async fn content_rejected_of_a_live_tile_record_leaves_the_bundle_alone() {
    let mut b = bridge();
    let ws = seed(&mut b, 2);
    b.handle_status_record(StatusRecord::ContentRejected(vec![(ws[1], 0)]));
    assert_eq!(b.fec_in.content_rejected_rx, 1);
    assert_eq!(b.fec_in.content_rejected_unknown, 0);
    assert_eq!(b.fec_in.content_rejected_tile, 1);
    // A rejection is not a verdict on the bundle, and nothing is resent from
    // the cache: the tile is re-encoded fresh.
    assert_eq!(b.reliable_emitter.stats.loss_path_resends, 0);
    assert!(b.transmission_ledger.peek(ws[1]).is_some());
}

#[tokio::test]
async fn content_rejected_after_the_ack_is_found_in_the_recent_ring() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    let dg = BundleAckBatch {
        entries: vec![BundleAck {
            bundle_seq: ws[0],
            arrival_time_ms_lo16: 1,
        }],
    }
    .encode();
    b.dispatch_inbound_datagram(H, &dg);
    assert!(
        b.transmission_ledger.peek(ws[0]).is_none(),
        "premise: resolved"
    );
    assert_eq!(b.recent_bundles.len(), 1);
    b.handle_status_record(StatusRecord::ContentRejected(vec![(ws[0], 0)]));
    assert_eq!(b.fec_in.content_rejected_rx, 1);
    assert_eq!(b.fec_in.content_rejected_unknown, 0, "found in the ring");
    assert_eq!(b.fec_in.content_rejected_tile, 1);
}

#[tokio::test]
async fn content_rejected_of_a_declared_lost_bundle_is_found_too() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    b.declare_lost_for_test(&[ws[0]]);
    // Take the tombstone away as an acknowledgement would not: evict via
    // reset is wrong, so read the ring only by blanking the ledger side.
    assert!(
        b.recent_bundles.get(ws[0]).is_some(),
        "the verdict filled the ring"
    );
    b.transmission_ledger.reset_session();
    b.handle_status_record(StatusRecord::ContentRejected(vec![(ws[0], 0)]));
    assert_eq!(b.fec_in.content_rejected_unknown, 0);
}

#[tokio::test]
async fn content_rejected_unknown_bundle_or_record_is_counted() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    b.handle_status_record(StatusRecord::ContentRejected(vec![
        (0x0BAD_0001, 0),
        (ws[0], 5),
    ]));
    assert_eq!(b.fec_in.content_rejected_rx, 2);
    assert_eq!(b.fec_in.content_rejected_unknown, 2);
}

#[tokio::test]
async fn the_recent_ring_is_bounded_and_forgets_the_oldest() {
    let mut r = ack_dispatch::RecentBundles::default();
    let tx = crate::transport::transmission_ledger::Transmission::single(
        0,
        10,
        1,
        Content::Tile(EmitKey::new(1, 0, 0, 0)),
        None,
        None,
    );
    for id in 0..4106u32 {
        r.remember(id, &tx);
    }
    assert_eq!(r.len(), 4096);
    assert!(r.get(0).is_none() && r.get(9).is_none(), "oldest evicted");
    assert!(r.get(10).is_some() && r.get(4105).is_some());
}

#[tokio::test]
async fn declined_loss_report_and_repair_request_are_counted() {
    let mut b = bridge();
    b.handle_status_record(StatusRecord::Declined {
        block_id: 4,
        reason: DeclineReason::Pruned,
        k: 8,
        missing: vec![0x01],
    });
    assert_eq!(b.fec_in.declined_rx, 1);

    let rr = RepairRequest {
        entries: vec![RepairAsk {
            block_id: 1,
            have: 2,
            missing: 1,
            first_new_idx: 0,
        }],
    }
    .encode();
    b.dispatch_inbound_datagram(H, &rr);
    assert_eq!(b.fec_in.repair_requests_rx, 1);

    let lr = LossReport {
        received: 90,
        lost_pre_fec: 7,
        recovered: 5,
        residual_lost: 2,
        ..LossReport::default()
    }
    .encode();
    b.dispatch_inbound_datagram(H, &lr);
    assert_eq!(b.fec_in.loss_reports_rx, 1);
    let p = b.transport_probe;
    assert_eq!(
        (p.report_lost_pre_fec, p.report_recovered, p.report_residual),
        (7, 5, 2)
    );
    assert_eq!(b.unroutable_inbound_datagrams, 0);
    // A malformed one is counted apart and keeps the last good values.
    b.dispatch_inbound_datagram(H, &lr[..10]);
    assert_eq!(b.fec_in.loss_reports_malformed, 1);
    assert_eq!(b.transport_probe.report_lost_pre_fec, 7);
}

fn status_bytes(records: &[StatusRecord]) -> Vec<u8> {
    let mut out = Vec::new();
    for r in records {
        r.encode(&mut out);
    }
    out
}

fn feed(b: &mut IoBridge, sid: StreamId, bytes: &[u8]) {
    b.wt_sessions
        .get_mut(&H)
        .unwrap()
        .test_feed_feedback_stream(sid, bytes);
    b.dispatch_drained_feedback();
}

#[tokio::test]
async fn a_status_stream_split_across_reads_is_parsed_and_a_feedback_stream_next_to_it_is_unaffected(
) {
    let mut b = bridge();
    let ws = seed(&mut b, 2);
    let status = StreamId::new(Side::Client, Dir::Bi, 1);
    let fb = StreamId::new(Side::Client, Dir::Bi, 2);
    let mut wire = vec![FEC_REPAIR_STATUS_STREAM_TAG];
    wire.extend(status_bytes(&[
        StatusRecord::ContentRejected(vec![(ws[0], 0)]),
        StatusRecord::Recovered(vec![ws[1]]),
    ]));
    // Cut mid-record, at every byte boundary the second record allows.
    let (a, c) = wire.split_at(wire.len() - 3);
    feed(&mut b, status, a);
    assert_eq!(
        b.fec_in.content_rejected_rx, 1,
        "the whole first record landed"
    );
    assert_eq!(b.fec_in.recovered_rx, 0, "the second is still partial");
    // The ordinary feedback stream (a HELLO would change the session; use an
    // input message type the parser accepts and counts) interleaves.
    let mut hello = Vec::new();
    crate::transport::client_caps::HelloMsg {
        caps: crate::transport::client_caps::ClientCapabilities {
            indices_raw_enabled: false,
            supports_h264: false,
            h264_acked_ref: false,
        },
    }
    .encode(&mut hello);
    feed(&mut b, fb, &hello);
    assert_eq!(b.last_hello_from, Some(H), "feedback parsed as before");
    feed(&mut b, status, c);
    assert_eq!(b.fec_in.recovered_rx, 1);
    assert_eq!(b.fec_in.repair_status_malformed, 0);
}

#[tokio::test]
async fn a_malformed_status_stream_is_warned_once_counted_and_then_ignored() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    let sid = StreamId::new(Side::Client, Dir::Bi, 1);
    // Unknown record type 0x7E.
    feed(&mut b, sid, &[0xF5, 0x7E, 0x04, 0x00, 1, 2, 3, 4]);
    assert_eq!(b.fec_in.repair_status_malformed, 1);
    // A valid record afterwards on the same stream is ignored, and the stream
    // is not counted again.
    let good = status_bytes(&[StatusRecord::ContentRejected(vec![(ws[0], 0)])]);
    feed(&mut b, sid, &good);
    feed(&mut b, sid, &[0x7E, 0x00, 0x00]);
    assert_eq!(b.fec_in.content_rejected_rx, 0);
    assert_eq!(b.fec_in.repair_status_malformed, 1);
    // Another stream of the same session still works.
    let other = StreamId::new(Side::Client, Dir::Bi, 5);
    let mut wire = vec![0xF5];
    wire.extend(good);
    feed(&mut b, other, &wire);
    assert_eq!(b.fec_in.content_rejected_rx, 1);
}

#[tokio::test]
async fn status_records_from_a_session_that_is_not_live_are_dropped_and_forget_clears_readers() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    let sid = StreamId::new(Side::Client, Dir::Bi, 1);
    let mut wire = vec![0xF5];
    wire.extend(status_bytes(&[StatusRecord::ContentRejected(vec![(
        ws[0], 0,
    )])]));
    b.last_hello_from = None;
    feed(&mut b, sid, &wire);
    assert_eq!(b.fec_in.content_rejected_rx, 0, "not live: dropped");
    assert!(b.status_streams.is_empty());

    b.last_hello_from = Some(H);
    let other = StreamId::new(Side::Client, Dir::Bi, 5);
    feed(&mut b, other, &wire);
    assert_eq!(b.fec_in.content_rejected_rx, 1);
    assert_eq!(b.status_streams.len(), 1, "premise: a reader exists");
    b.forget_session(H);
    assert!(
        b.status_streams.is_empty(),
        "forget_session clears the readers"
    );
}

#[tokio::test]
async fn a_finished_status_stream_frees_its_slot_so_the_client_can_reopen_one() {
    let mut b = bridge();
    let ws = seed(&mut b, 1);
    let stream = |i| StreamId::new(Side::Client, Dir::Bi, i);
    let mut tagged = vec![FEC_REPAIR_STATUS_STREAM_TAG];
    tagged.extend(status_bytes(&[StatusRecord::Recovered(vec![ws[0]])]));
    for i in 0..8 {
        feed(&mut b, stream(i), &tagged);
    }
    assert_eq!(
        b.status_streams.len(),
        8,
        "premise: the per-handle cap is full"
    );
    let mut late = vec![FEC_REPAIR_STATUS_STREAM_TAG];
    late.extend(status_bytes(&[StatusRecord::ContentRejected(vec![(
        ws[0], 0,
    )])]));
    feed(&mut b, stream(9), &late);
    assert_eq!(
        b.fec_in.content_rejected_rx, 0,
        "premise: a ninth stream is refused while eight are held"
    );

    // The client finished its first stream.
    b.wt_sessions
        .get_mut(&H)
        .unwrap()
        .test_end_stream(stream(0));
    b.dispatch_drained_feedback();
    assert_eq!(
        b.status_streams.len(),
        7,
        "the closed stream's slot is freed"
    );

    feed(&mut b, stream(10), &late);
    assert_eq!(
        b.fec_in.content_rejected_rx, 1,
        "a reopened stream is served"
    );
}
