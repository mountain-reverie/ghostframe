//! R6-d D7: the H.264 content/transport split. With FEC blocks on, a ledger
//! verdict on a source of an open block does not reach the acked-reference
//! manager; only the block's content outcome (DECLINED, or its deadline) does.

use super::*;
use crate::config::FecBlockParams;
use crate::transport::reliable_emitter::Content;
use ghostframe_protocol::fec::frame::{parse, records, Frame};
use ghostframe_protocol::fec::repair_status::{DeclineReason, StatusRecord};
use std::time::Duration;
use tokio::net::UnixStream;

const H: ConnectionHandle = ConnectionHandle(3);
const RAW: usize = 1000;

fn meta() -> frame::FrameMeta {
    frame::FrameMeta {
        width: 64,
        height: 64,
        stride: 256,
        timestamp_us: 0,
        capture_done_ns: 0,
        unchanged: false,
    }
}

/// Plans frame `seq` with the manager and commits what the plan says (an
/// access unit of `len` bytes); returns the frame_seq.
fn send(b: &mut IoBridge, seq: u32, len: usize) -> u32 {
    use crate::encoder::h264::backend::{AuMeta, EncodedAu};
    use crate::encoder::h264::tracker::PicKind;
    let req = b
        .h264_refs_plan(seq, (64, 64), now_std(), false)
        .expect("premise: the manager planned the frame");
    let plan = req.plan;
    let au = EncodedAu {
        payload: vec![9; len],
        meta: AuMeta {
            idr: plan.kind == PicKind::Idr,
            header: Some(plan.slice_header(&crate::encoder::h264::syntax::StreamParams::writer())),
            ref_seq: plan.ref_frame_seq(),
            params: None,
            encode_us: 0,
        },
    };
    assert!(b.h264_refs_commit(&plan, au, &meta(), RAW - 100, now_std()));
    b.pump_admission();
    b.close_block_on_empty();
    b.service_pending_repairs();
    b.flush_bundles();
    seq
}

async fn h264_blocks_bridge(srtt_ms: u64) -> IoBridge {
    let (ours, _peer) = UnixStream::pair().expect("pair");
    let server = QuicServer::new().expect("server");
    let mut b = IoBridge::new_with_stream_for_test(ours, server);
    b.wt_sessions.insert(H, WebTransportServer::default());
    b.last_hello_from = Some(H);
    b.test_wire = Some(bundle_tx::TestWire::default());
    b.forced_raw_datagram_size_for_test = Some(RAW);
    b.set_fec_blocks_for_test(Some(FecBlockParams {
        k_max: 64,
        t_ms: 5,
        r: 2,
    }));
    b.client_caps.supports_h264 = true;
    b.client_caps.h264_acked_ref = true;
    for _ in 0..40 {
        b.reliable_emitter
            .rtt
            .on_sample(Duration::from_millis(srtt_ms));
    }
    b
}

/// `(bundle_seq, block_id, [(frame_seq, frag_idx)])` of every SOURCE sent so far.
type SourceView = (u32, u16, Vec<(u32, u16)>);

fn sources(b: &IoBridge) -> Vec<SourceView> {
    b.sent_frames_for_test
        .iter()
        .filter_map(|f| match parse(f).unwrap() {
            Frame::Source(s) => Some((
                s.bundle_seq,
                s.block_id,
                records(s.records)
                    .map(|r| r.unwrap())
                    .filter(|r| r.len() >= 22 && r[0] & 0x80 == 0)
                    .map(|r| {
                        (
                            u32::from_be_bytes(r[0..4].try_into().unwrap()),
                            u16::from_be_bytes(r[4..6].try_into().unwrap()),
                        )
                    })
                    .collect(),
            )),
            _ => None,
        })
        .collect()
}

fn stamp_all(b: &mut IoBridge) {
    b.record_emitted_transmissions(now_std());
    let ids: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .map(|f| match parse(f).unwrap() {
            Frame::Source(s) => s.bundle_seq,
            Frame::Repair(r) => r.bundle_seq,
            Frame::Plain(p) => p.bundle_seq,
        })
        .collect();
    for q in ids {
        b.stamp_departure(q, now_std());
    }
}

fn ack_ids(b: &mut IoBridge, ids: &[u32]) {
    crate::transport::io_bridge::test_ack::ack_bundles(b, ids);
}

/// An IDR (frame 1) acknowledged whole, then P frame 2 in several sources of
/// one block, all departed; every source of frame 2 but the first is
/// acknowledged. Returns the bridge, the block id, the first (lost) source's
/// bundle_seq and its fragment.
async fn p_frame_with_one_lost_source(srtt_ms: u64) -> (IoBridge, u16, u32, (u32, u16)) {
    let mut b = h264_blocks_bridge(srtt_ms).await;
    let idr_seq = send(&mut b, 0, 3000);
    stamp_all(&mut b);
    let idr: Vec<u32> = sources(&b).iter().map(|s| s.0).collect();
    ack_ids(&mut b, &idr);
    // The client decoded it: the next plan is a P frame from that anchor.
    b.h264_refs_frame_ack(idr_seq, 1, 0);
    let mark = sources(&b).len();
    let f2 = send(&mut b, 1, 9000);
    stamp_all(&mut b);
    let all = sources(&b);
    let p: Vec<_> = all[mark..].to_vec();
    assert_eq!(
        b.reliable_emitter.h264.is_idr(f2),
        Some(false),
        "premise: a P frame"
    );
    assert!(p.len() >= 6, "premise: several sources, got {}", p.len());
    ack_ids(&mut b, &p[1..].iter().map(|s| s.0).collect::<Vec<_>>());
    let lost = (p[0].0, p[0].2[0]);
    (b, p[0].1, lost.0, lost.1)
}

fn identity_holds(b: &IoBridge) {
    let s = b.h264.stats;
    let out = b
        .transmission_ledger
        .count_outstanding(|c| matches!(c, Content::H264Frag { .. })) as u64;
    assert_eq!(
        s.tx_recorded,
        s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten + out,
        "{s:?}"
    );
}

#[tokio::test]
async fn a_fragment_fec_rebuilds_never_reaches_the_manager_as_lost_even_at_srtt_150() {
    let (mut b, _blk, seq, _) = p_frame_with_one_lost_source(150).await;
    let t0 = now_std();
    b.expire_transmissions(t0 + Duration::from_millis(200));
    assert!(
        b.instr.losses_declared >= 1,
        "premise: a verdict was issued"
    );
    assert!(b.h264.stats.tx_judged_lost >= 1);
    assert!(
        b.h264.stats.verdicts_parked_by_fec >= 1,
        "the verdict was parked"
    );
    assert_eq!(b.h264.stats.frames_suspect, 0, "the manager was not told");
    assert_eq!(b.h264.refs_counters.verdicts, 0);
    // The client rebuilt it: RECOVERED. Then every deadline passes.
    b.handle_status_record(StatusRecord::Recovered(vec![seq]));
    b.service_blocks(t0 + Duration::from_secs(60));
    assert_eq!(b.block_tx.content_lost_h264, 0, "never reported lost");
    assert_eq!(b.h264.stats.frames_suspect, 0);
    assert_eq!(b.h264.stats.content_lost_deadline, 0);
    let st = b.h264.refs.as_ref().unwrap().mgr.stats();
    assert_eq!(st.recoveries, 0, "no recovery was planned");
    identity_holds(&b);
}

#[tokio::test]
async fn a_declined_fragment_reaches_the_manager_exactly_once_and_a_parked_verdict_keeps_its_queue()
{
    let (mut b, blk, seq, (frame, frag)) = p_frame_with_one_lost_source(20).await;
    let t0 = now_std();
    b.expire_transmissions(t0 + Duration::from_millis(500));
    assert!(b.h264.stats.verdicts_parked_by_fec >= 1, "premise: parked");
    assert_eq!(b.h264.stats.frames_suspect, 0);
    // A parked verdict has not purged or dropped the frame's fragments.
    assert!(
        b.reliable_emitter.h264.get(frame, frag).is_some(),
        "the cache still holds the fragment"
    );
    b.handle_status_record(StatusRecord::Declined {
        block_id: blk,
        reason: DeclineReason::Pruned,
        k: 0,
        missing: vec![],
    });
    assert_eq!(b.h264.stats.content_lost_decline, 1);
    assert_eq!(b.h264.stats.frames_suspect, 1, "its frame is suspect, once");
    assert!(b.reliable_emitter.h264.get(frame, frag).is_none());
    // A second DECLINED, the deadline and a late verdict add nothing.
    b.handle_status_record(StatusRecord::Declined {
        block_id: blk,
        reason: DeclineReason::Pruned,
        k: 0,
        missing: vec![],
    });
    b.service_blocks(t0 + Duration::from_secs(60));
    assert_eq!(b.block_tx.content_lost_h264, 1);
    assert_eq!(b.h264.stats.content_lost_decline, 1);
    assert_eq!(b.h264.stats.content_lost_deadline, 0);
    let _ = seq;
    identity_holds(&b);
}

#[tokio::test]
async fn a_late_verdict_after_the_deadline_does_not_report_a_second_time() {
    let (mut b, _blk, _seq, (frame, _)) = p_frame_with_one_lost_source(20).await;
    let t0 = now_std();
    // The deadline comes first (no verdict yet): the unaccounted source is lost.
    b.service_blocks(t0 + Duration::from_secs(60));
    assert_eq!(b.h264.stats.content_lost_deadline, 1);
    assert_eq!(b.h264.stats.frames_suspect, 1);
    // Make the frame look untouched, then let the ledger's verdict arrive late:
    // by_bundle retention says the outcome is taken, so the manager hears
    // nothing more.
    b.h264.tracker.get_mut(frame).unwrap().suspect = false;
    b.h264.stats.frames_suspect = 0;
    let parked = b.h264.stats.verdicts_parked_by_fec;
    b.expire_transmissions(t0 + Duration::from_secs(61));
    assert!(
        b.h264.stats.tx_judged_lost >= 1,
        "premise: the verdict came"
    );
    assert_eq!(b.h264.stats.frames_suspect, 0, "not reported again");
    assert_eq!(
        b.h264.stats.verdicts_parked_by_fec, parked,
        "Skip, not Park"
    );
    assert_eq!(b.h264.stats.content_lost_deadline, 1);
    identity_holds(&b);
}

#[tokio::test]
async fn note_raised_lost_runs_at_a_parked_verdict_and_a_frame_ack_clears_a_deadline_suspect() {
    let (mut b, _blk, _seq, (frame, _)) = p_frame_with_one_lost_source(20).await;
    let t0 = now_std();
    // Frame 2 was a raised frame.
    let st = b.h264.refs.as_mut().unwrap();
    st.raised_for_test(frame, t0);
    b.expire_transmissions(t0 + Duration::from_millis(500));
    assert_eq!(b.h264.refs_counters.raised_frame_lost, 1, "the guard ran");
    assert!(b.h264.raise_paused_until.is_some(), "the raise paused");
    assert_eq!(b.h264.stats.frames_suspect, 0, "the manager was not told");
    // The deadline makes it suspect; the client's FRAME_ACK clears it.
    b.service_blocks(t0 + Duration::from_secs(60));
    assert_eq!(b.h264.stats.frames_suspect, 1);
    b.h264_refs_frame_ack(frame, 1, 0);
    let st = b.h264.refs.as_ref().unwrap().mgr.stats();
    assert!(st.suspect_overridden >= 1, "the late ack overrode it");
}
