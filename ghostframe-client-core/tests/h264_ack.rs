// SPDX-License-Identifier: MIT OR Apache-2.0
//! Transport acknowledgement of H.264 frame fragments, and the IDR window
//! exemption (`frame_assembly.rs`).
//!
//! The server records every bundle it sends in its transmission ledger under
//! its `bundle_seq` and judges it lost unless the client acknowledges that
//! `bundle_seq` (ACK v4). These tests pin what the client owes: one
//! acknowledgement per received bundle, whatever its records hold (a record it
//! cannot use is reported CONTENT_REJECTED instead); and that a late IDR still
//! completes.

use ghostframe_client_core::{frame_plain, ClientConfig, ClientCore, Event, PollOutput};
use ghostframe_protocol::fec::feedback::BundleAckBatch;
use ghostframe_protocol::fec::repair_status::{
    StatusReader, StatusRecord, FEC_REPAIR_STATUS_STREAM_TAG,
};
use ghostframe_protocol::protocol::{fragment_frame, NO_ACK};

fn core() -> ClientCore {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            ..Default::default()
        },
        0,
    );
    while core.poll_transmit(0).is_some() {}
    core
}

/// A frame datagram stamped with `seq` as its record id and carried alone in
/// the PLAIN bundle `seq`.
fn framed(dg: Vec<u8>, seq: u32) -> Vec<u8> {
    frame_plain(seq, &[&stamp(dg, seq)])
}

/// Stamp a `wire_seq` into a frame datagram the way the server's emitter does.
fn stamp(mut dg: Vec<u8>, ws: u32) -> Vec<u8> {
    dg[8..12].copy_from_slice(&ws.to_be_bytes());
    dg
}

/// What the core sent once the batch timer has run: the bundle_seqs
/// acknowledged (ACK v4) and the CONTENT_REJECTED `(bundle_seq, record_idx)`
/// pairs.
fn sent(core: &mut ClientCore, now_us: u64) -> (Vec<u32>, Vec<(u32, u8)>) {
    core.on_timeout(now_us);
    let mut acked = Vec::new();
    let mut status = Vec::new();
    while let Some(o) = core.poll_transmit(now_us) {
        match o {
            PollOutput::Datagram(d) if d.first() == Some(&0x08) => acked.extend(
                BundleAckBatch::decode(&d)
                    .unwrap()
                    .entries
                    .iter()
                    .map(|e| e.bundle_seq),
            ),
            PollOutput::RepairStatus(b) => status.extend(b),
            _ => {}
        }
    }
    acked.sort_unstable();
    acked.dedup();
    let mut rejected = Vec::new();
    if !status.is_empty() {
        assert_eq!(status[0], FEC_REPAIR_STATUS_STREAM_TAG);
        for r in StatusReader::default().push(&status[1..]).unwrap() {
            if let StatusRecord::ContentRejected(v) = r {
                rejected.extend(v);
            }
        }
    }
    (acked, rejected)
}

fn acked(core: &mut ClientCore, now_us: u64) -> Vec<u32> {
    sent(core, now_us).0
}

#[test]
fn every_source_fragment_is_acknowledged_by_its_bundle_seq() {
    let mut c = core();
    let dgs = fragment_frame(9, 1, false, &[7u8; 3000], 1200);
    assert_eq!(dgs.len(), 3);
    for (i, dg) in dgs.into_iter().enumerate() {
        c.handle_datagram(&framed(dg, 100 + i as u32), 1_000);
    }
    assert_eq!(acked(&mut c, 10_000), vec![100, 101, 102]);
    // The loss reporter saw the three bundles as received, so the sequence it
    // follows has no phantom gap where they were.
    assert_eq!(c.wire_loss_stats().received, 3);
}

#[test]
fn a_fragment_marked_no_ack_is_acknowledged_with_its_bundle() {
    // The record id is no part of the acknowledgement any more: the client
    // acknowledges what arrived, the bundle, and the server resolves it.
    let mut c = core();
    for (i, dg) in fragment_frame(9, 1, false, &[7u8; 3000], 1200)
        .into_iter()
        .enumerate()
    {
        assert_eq!(u32::from_be_bytes(dg[8..12].try_into().unwrap()), NO_ACK);
        c.handle_datagram(&frame_plain(300 + i as u32, &[&dg]), 1_000);
    }
    let (acked, rejected) = sent(&mut c, 10_000);
    assert_eq!(acked, vec![300, 301, 302]);
    assert!(rejected.is_empty());
}

#[test]
fn malformed_fragments_are_content_rejected_in_an_acknowledged_bundle() {
    let mut c = core();
    // A fragment index at or past the total: a malformed fragment, not a
    // source fragment.
    let mut past_end = stamp(fragment_frame(9, 1, false, &[7u8; 10], 1200).remove(0), 500);
    past_end[4..6].copy_from_slice(&1u16.to_be_bytes());
    c.handle_datagram(&frame_plain(500, &[&past_end]), 1_000);
    // A fragment claiming more fragments than any access unit has is refused
    // like an invalid tile.
    let mut bad = stamp(fragment_frame(9, 1, false, &[7u8; 10], 1200).remove(0), 501);
    bad[6..8].copy_from_slice(&20_000u16.to_be_bytes());
    c.handle_datagram(&frame_plain(501, &[&bad]), 1_000);
    // The bundles arrived, so they are acknowledged; the records are refused.
    let (acked, rejected) = sent(&mut c, 10_000);
    assert_eq!(acked, vec![500, 501]);
    assert_eq!(rejected, vec![(500, 0), (501, 0)]);
}

#[test]
fn a_duplicate_fragment_is_acknowledged_again_under_its_own_bundle_seq() {
    let mut c = core();
    let dg = fragment_frame(9, 1, false, &[7u8; 10], 1200).remove(0);
    c.handle_datagram(&framed(dg.clone(), 40), 1_000);
    c.handle_datagram(&framed(dg, 41), 2_000);
    assert_eq!(acked(&mut c, 10_000), vec![40, 41]);
}

#[test]
fn a_fragment_older_than_the_window_is_still_acknowledged() {
    let mut c = core();
    // Frame 50 moves the window; a P fragment of frame 10 is then dropped as
    // stale, but it did arrive.
    for dg in fragment_frame(50, 1, false, &[1u8; 10], 1200) {
        c.handle_datagram(&framed(dg, 1), 1_000);
    }
    let old = fragment_frame(10, 1, false, &[2u8; 10], 1200).remove(0);
    let evs = c.handle_datagram(&framed(old, 2), 2_000);
    assert!(evs.is_empty(), "stale: dropped");
    assert_eq!(acked(&mut c, 10_000), vec![1, 2]);
}

/// An IDR whose fragments were repaired after newer frame sequences had been
/// seen still completes (within the exemption), where a P frame does not.
/// Newer frames here are incomplete ones: a keyframe older than a frame
/// already delivered is dropped (see the next test).
#[test]
fn a_late_idr_completes_but_a_late_p_frame_does_not() {
    let payload = [9u8; 2500];
    for (is_idr, completes) in [(true, true), (false, false)] {
        let mut c = core();
        let dgs = fragment_frame(10, 1, is_idr, &payload, 1200);
        assert_eq!(dgs.len(), 3);
        // Fragment 0 arrives, then the window moves on to frame 40 (whose
        // other fragment has not arrived).
        c.handle_datagram(&framed(dgs[0].clone(), 1), 1_000);
        let newer = fragment_frame(40, 1, false, &[1u8; 2500], 1200);
        c.handle_datagram(&framed(newer[0].clone(), 2), 2_000);
        // The rest of frame 10 arrives afterwards (a resend).
        let mut evs = c.handle_datagram(&framed(dgs[1].clone(), 3), 50_000);
        evs.extend(c.handle_datagram(&framed(dgs[2].clone(), 4), 51_000));
        let done = evs.iter().any(|e| {
            matches!(e, Event::NeedsH264 { frame_seq: 10, payload: p, .. } if p.len() == payload.len())
        });
        assert_eq!(done, completes, "idr={is_idr}: {evs:?}");
    }
}

/// A keyframe that completes after a newer frame was delivered is not
/// delivered: the decoder would jump back.
#[test]
fn a_keyframe_older_than_a_delivered_frame_is_not_delivered() {
    let mut c = core();
    let idr = fragment_frame(10, 1, true, &[9u8; 2500], 1200);
    c.handle_datagram(&framed(idr[0].clone(), 1), 1_000);
    // Frame 11 completes and is delivered.
    let p = fragment_frame(11, 1, false, &[1u8; 10], 1200);
    let evs = c.handle_datagram(&framed(p[0].clone(), 2), 2_000);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Event::NeedsH264 { frame_seq: 11, .. })));
    let mut evs = c.handle_datagram(&framed(idr[1].clone(), 3), 3_000);
    evs.extend(c.handle_datagram(&framed(idr[2].clone(), 4), 4_000));
    assert!(
        !evs.iter().any(|e| matches!(e, Event::NeedsH264 { .. })),
        "{evs:?}"
    );
    assert_eq!(c.assembly_stats().frame_assemblies, 0, "and it is gone");
}

/// A hostile stream of one IDR fragment per fresh frame_seq, each claiming
/// 16 K fragments, keeps the assembly map to the window plus the one newest
/// IDR, whether the sequence numbers climb with the window or sit behind it.
#[test]
fn a_stream_of_idr_fragments_with_fresh_frame_numbers_keeps_assemblies_bounded() {
    let mut c = core();
    let hostile = |seq: u32| {
        let mut dg = stamp(fragment_frame(seq, 1, true, &[0u8; 8], 1200).remove(0), seq);
        dg[6..8].copy_from_slice(&16_384u16.to_be_bytes());
        dg
    };
    let mut peak = 0;
    for seq in 1000..1400u32 {
        c.handle_datagram(
            &ghostframe_client_core::frame_one(hostile(seq)),
            1_000 + u64::from(seq),
        );
        peak = peak.max(c.assembly_stats().frame_assemblies);
    }
    // Behind the window: ascending stale sequence numbers.
    for seq in 10..300u32 {
        c.handle_datagram(
            &ghostframe_client_core::frame_one(hostile(seq)),
            500_000 + u64::from(seq),
        );
        peak = peak.max(c.assembly_stats().frame_assemblies);
    }
    assert!(peak >= 2, "premise: assemblies were created ({peak})");
    assert!(peak <= 4, "window (3) + the newest IDR: peak {peak}");
}

#[test]
fn an_idr_fragment_that_starts_late_assembly_is_taken_and_expires() {
    let mut c = core();
    for dg in fragment_frame(40, 1, false, &[1u8; 10], 1200) {
        c.handle_datagram(&framed(dg, 2), 1_000);
    }
    // The first fragment of an old IDR arrives only now (the original was lost).
    let dgs = fragment_frame(10, 1, true, &[9u8; 2500], 1200);
    c.handle_datagram(&framed(dgs[0].clone(), 5), 100_000);
    assert_eq!(c.assembly_stats().frame_assemblies, 1);
    // Past the exemption, any newer frame datagram evicts it.
    for dg in fragment_frame(41, 1, false, &[1u8; 10], 1200) {
        c.handle_datagram(&framed(dg, 6), 100_000 + 2_500_000);
    }
    assert_eq!(
        c.assembly_stats().frame_assemblies,
        0,
        "the exemption is bounded in time"
    );
}
