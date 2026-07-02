//! Integration tests for the H.264 full-frame reassembly path
//! (`frame_assembly.rs`, wired into `ClientCore::handle_datagram`'s
//! non-tile-datagram branch).

use ghostframe_client_core::{ClientConfig, ClientCore, Event};
use ghostframe_protocol::protocol::fragment_frame;

fn test_core() -> ClientCore {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
        },
        0,
    );
    while core.poll_transmit(0).is_some() {}
    core
}

#[test]
fn h264_frame_reassembles_to_needs_h264() {
    let mut core = test_core();
    let payload: Vec<u8> = (0..3000u32).map(|i| (i % 256) as u8).collect();
    let dgs = fragment_frame(9, 12_345, true, &payload, 1200);
    assert!(dgs.len() > 1, "payload should span multiple fragments");

    // Deliver out of order except the last one.
    let mut evs = Vec::new();
    for dg in &dgs[1..] {
        evs.extend(core.handle_datagram(dg, 0));
    }
    assert!(evs.is_empty(), "assembly incomplete without fragment 0");
    evs.extend(core.handle_datagram(&dgs[0], 0));

    match &evs[..] {
        [Event::NeedsH264 {
            frame_seq: 9,
            timestamp_us: 12_345,
            is_keyframe: true,
            payload: p,
        }] => {
            assert_eq!(p, &payload);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn h264_parity_fragments_are_dropped_not_counted() {
    let mut core = test_core();
    // handle_datagram requires >= DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE
    // (24) bytes before even reaching the tile-vs-frame branch, so keep
    // each fragment's on-wire datagram (14B FrameHeader + payload) >= 24B.
    let payload: Vec<u8> = (0..24u8).collect();
    let dgs = fragment_frame(3, 0, false, &payload, 12);
    assert_eq!(dgs.len(), 2); // 24 bytes / 12-byte fragments -> 2 frags

    // Synthesize a parity fragment (frag_idx >= frag_total) for this frame:
    // reuse the header bytes but bump frag_idx past frag_total.
    let mut parity = dgs[0].clone();
    parity[4] = 0x00;
    parity[5] = 0x02; // frag_idx = 2 == frag_total (2) -> parity, must be skipped
    let evs = core.handle_datagram(&parity, 0);
    assert!(evs.is_empty(), "parity fragment must not be counted or emit events");

    // Now deliver both real fragments; frame should still complete cleanly.
    let mut evs = core.handle_datagram(&dgs[0], 0);
    evs.extend(core.handle_datagram(&dgs[1], 0));
    match &evs[..] {
        [Event::NeedsH264 { frame_seq: 3, payload: p, .. }] => {
            assert_eq!(p, &payload);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn h264_stale_frame_dropped_after_eviction_window() {
    let mut core = test_core();
    // Frame 1: incomplete (only fragment 0 of 2). 20-byte payload / 10-byte
    // fragments -> each datagram is 14 + 10 = 24 bytes (at the gate floor).
    let payload1: Vec<u8> = (0..20u8).collect();
    let f1 = fragment_frame(1, 0, false, &payload1, 10);
    assert_eq!(f1.len(), 2);
    assert!(core.handle_datagram(&f1[0], 0).is_empty());

    // Frames 2, 3, 4 (single-fragment each) advance latest_full_frame_seq.
    for seq in 2..=4u32 {
        let d = fragment_frame(seq, 0, false, &[9u8; 10], 100);
        assert_eq!(d.len(), 1);
        let _ = core.handle_datagram(&d[0], 0);
    }

    // Frame 1's remaining fragment now arrives late; frame is stale
    // (1 < latest(4) - 2 = 2), dropped before insertion, so no event.
    let evs = core.handle_datagram(&f1[1], 0);
    assert!(evs.is_empty(), "stale frame-1 fragment must be dropped: {evs:?}");
}

#[test]
fn h264_malformed_datagram_does_not_panic() {
    let mut core = test_core();
    // frag_total = 0 in an otherwise well-formed, minimum-length
    // FrameHeader (top bit of byte 0 clear -> not a tile datagram; all
    // zero -> frag_idx = 0, frag_total = 0) must be dropped, not panic on
    // `vec![None; 0]` indexing.
    let zero_total = vec![0u8; 24];
    let evs = core.handle_datagram(&zero_total, 0);
    assert!(evs.is_empty());
}
