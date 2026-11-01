//! Transport acknowledgement of H.264 frame fragments, and the IDR window
//! exemption (`frame_assembly.rs`).
//!
//! The server records every H.264 fragment it sends in its transmission
//! ledger under a real `wire_seq` and judges it lost unless the client
//! acknowledges that `wire_seq`. These tests pin what the client owes: one
//! acknowledgement per received source fragment, none for `NO_ACK`, parity or
//! anything malformed; and that a late IDR still completes.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput};
use ghostframe_protocol::ack::AckBatch;
use ghostframe_protocol::protocol::{build_frame_parity_datagram, fragment_frame, NO_ACK};

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

/// Stamp a `wire_seq` into a frame datagram the way the server's emitter does.
fn stamp(mut dg: Vec<u8>, ws: u32) -> Vec<u8> {
    dg[8..12].copy_from_slice(&ws.to_be_bytes());
    dg
}

/// The wire_seqs acknowledged so far, once the batch timer has run.
fn acked(core: &mut ClientCore, now_us: u64) -> Vec<u32> {
    core.on_timeout(now_us);
    let mut out = Vec::new();
    while let Some(o) = core.poll_transmit(now_us) {
        if let PollOutput::Datagram(d) = o {
            if d.first() == Some(&0x06) {
                out.extend(
                    AckBatch::decode(&d)
                        .unwrap()
                        .entries
                        .iter()
                        .map(|e| e.wire_seq),
                );
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

#[test]
fn every_tracked_source_fragment_is_acknowledged_by_its_wire_seq() {
    let mut c = core();
    let dgs = fragment_frame(9, 1, false, &[7u8; 3000], 1200);
    assert_eq!(dgs.len(), 3);
    for (i, dg) in dgs.into_iter().enumerate() {
        c.handle_datagram(&stamp(dg, 100 + i as u32), 1_000);
    }
    assert_eq!(acked(&mut c, 10_000), vec![100, 101, 102]);
    // The wire-level loss accounting saw them as received, so the tile
    // sequence the same counter follows has no phantom gap where they were.
    assert_eq!(c.wire_loss_stats().received, 3);
}

#[test]
fn a_fragment_marked_no_ack_is_not_acknowledged() {
    let mut c = core();
    for dg in fragment_frame(9, 1, false, &[7u8; 3000], 1200) {
        assert_eq!(u32::from_be_bytes(dg[8..12].try_into().unwrap()), NO_ACK);
        c.handle_datagram(&dg, 1_000);
    }
    assert!(acked(&mut c, 10_000).is_empty());
}

#[test]
fn parity_and_malformed_fragments_are_not_acknowledged() {
    let mut c = core();
    let parity = stamp(build_frame_parity_datagram(9, 1, false, 3, &[1u8; 20]), 500);
    c.handle_datagram(&parity, 1_000);
    // A fragment claiming more fragments than any access unit has is refused
    // before the acknowledgement, like an invalid tile.
    let mut bad = stamp(fragment_frame(9, 1, false, &[7u8; 10], 1200).remove(0), 501);
    bad[6..8].copy_from_slice(&20_000u16.to_be_bytes());
    c.handle_datagram(&bad, 1_000);
    assert!(acked(&mut c, 10_000).is_empty());
}

#[test]
fn a_duplicate_fragment_is_acknowledged_again_under_its_own_wire_seq() {
    let mut c = core();
    let dg = fragment_frame(9, 1, false, &[7u8; 10], 1200).remove(0);
    c.handle_datagram(&stamp(dg.clone(), 40), 1_000);
    c.handle_datagram(&stamp(dg, 41), 2_000);
    assert_eq!(acked(&mut c, 10_000), vec![40, 41]);
}

#[test]
fn a_fragment_older_than_the_window_is_still_acknowledged() {
    let mut c = core();
    // Frame 50 moves the window; a P fragment of frame 10 is then dropped as
    // stale, but it did arrive.
    for dg in fragment_frame(50, 1, false, &[1u8; 10], 1200) {
        c.handle_datagram(&stamp(dg, 1), 1_000);
    }
    let old = fragment_frame(10, 1, false, &[2u8; 10], 1200).remove(0);
    let evs = c.handle_datagram(&stamp(old, 2), 2_000);
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
        c.handle_datagram(&stamp(dgs[0].clone(), 1), 1_000);
        let newer = fragment_frame(40, 1, false, &[1u8; 2500], 1200);
        c.handle_datagram(&stamp(newer[0].clone(), 2), 2_000);
        // The rest of frame 10 arrives afterwards (a resend).
        let mut evs = c.handle_datagram(&stamp(dgs[1].clone(), 3), 50_000);
        evs.extend(c.handle_datagram(&stamp(dgs[2].clone(), 4), 51_000));
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
    c.handle_datagram(&stamp(idr[0].clone(), 1), 1_000);
    // Frame 11 completes and is delivered.
    let p = fragment_frame(11, 1, false, &[1u8; 10], 1200);
    let evs = c.handle_datagram(&stamp(p[0].clone(), 2), 2_000);
    assert!(evs
        .iter()
        .any(|e| matches!(e, Event::NeedsH264 { frame_seq: 11, .. })));
    let mut evs = c.handle_datagram(&stamp(idr[1].clone(), 3), 3_000);
    evs.extend(c.handle_datagram(&stamp(idr[2].clone(), 4), 4_000));
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
        c.handle_datagram(&hostile(seq), 1_000 + u64::from(seq));
        peak = peak.max(c.assembly_stats().frame_assemblies);
    }
    // Behind the window: ascending stale sequence numbers.
    for seq in 10..300u32 {
        c.handle_datagram(&hostile(seq), 500_000 + u64::from(seq));
        peak = peak.max(c.assembly_stats().frame_assemblies);
    }
    assert!(peak >= 2, "premise: assemblies were created ({peak})");
    assert!(peak <= 4, "window (3) + the newest IDR: peak {peak}");
}

#[test]
fn an_idr_fragment_that_starts_late_assembly_is_taken_and_expires() {
    let mut c = core();
    for dg in fragment_frame(40, 1, false, &[1u8; 10], 1200) {
        c.handle_datagram(&stamp(dg, 2), 1_000);
    }
    // The first fragment of an old IDR arrives only now (the original was lost).
    let dgs = fragment_frame(10, 1, true, &[9u8; 2500], 1200);
    c.handle_datagram(&stamp(dgs[0].clone(), 5), 100_000);
    assert_eq!(c.assembly_stats().frame_assemblies, 1);
    // Past the exemption, any newer frame datagram evicts it.
    for dg in fragment_frame(41, 1, false, &[1u8; 10], 1200) {
        c.handle_datagram(&stamp(dg, 6), 100_000 + 2_500_000);
    }
    assert_eq!(
        c.assembly_stats().frame_assemblies,
        0,
        "the exemption is bounded in time"
    );
}
