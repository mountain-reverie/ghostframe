//! Integration tests for the H.264 full-frame reassembly path
//! (`frame_assembly.rs`, wired into `ClientCore::handle_datagram`'s
//! non-tile-datagram branch).

use ghostframe_client_core::{ClientConfig, ClientCore, Event};
use ghostframe_protocol::protocol::{
    fragment_frame, fragment_frame_ref, FrameRefInfo, FRAME_FLAG_IDR, REF_FRAME_NONE,
};

fn test_core() -> ClientCore {
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
            ref_frame_seq: REF_FRAME_NONE,
            flags: FRAME_FLAG_IDR,
            slots: 0,
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
    // Frame datagrams need only the 22-byte FrameHeader (the 24-byte floor is
    // for tiles); these fragments are 22B + 12B payload each.
    let payload: Vec<u8> = (0..24u8).collect();
    let dgs = fragment_frame(3, 0, false, &payload, 12);
    assert_eq!(dgs.len(), 2); // 24 bytes / 12-byte fragments -> 2 frags

    // Synthesize a parity fragment (frag_idx >= frag_total) for this frame:
    // reuse the header bytes but bump frag_idx past frag_total.
    let mut parity = dgs[0].clone();
    parity[4] = 0x00;
    parity[5] = 0x02; // frag_idx = 2 == frag_total (2) -> parity, must be skipped
    let evs = core.handle_datagram(&parity, 0);
    assert!(
        evs.is_empty(),
        "parity fragment must not be counted or emit events"
    );

    // Now deliver both real fragments; frame should still complete cleanly.
    let mut evs = core.handle_datagram(&dgs[0], 0);
    evs.extend(core.handle_datagram(&dgs[1], 0));
    match &evs[..] {
        [Event::NeedsH264 {
            frame_seq: 3,
            payload: p,
            ..
        }] => {
            assert_eq!(p, &payload);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn h264_stale_frame_dropped_after_eviction_window() {
    let mut core = test_core();
    // Frame 1: incomplete (only fragment 0 of 2). 20-byte payload / 10-byte
    // fragments -> each datagram is 22 + 10 = 32 bytes.
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
    assert!(
        evs.is_empty(),
        "stale frame-1 fragment must be dropped: {evs:?}"
    );
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

#[test]
fn h264_v2_reference_info_reaches_the_event() {
    let mut core = test_core();
    let payload: Vec<u8> = (0..2500u32).map(|i| (i % 251) as u8).collect();
    let info = FrameRefInfo {
        flags: 0x04 | 0x08, // mark_lt | target_is_lt
        slots: 0b0111,
        ref_frame_seq: 41,
    };
    let dgs = fragment_frame_ref(42, 7, info, &payload, 1000);
    assert_eq!(dgs.len(), 3);
    let mut evs = Vec::new();
    for dg in &dgs {
        evs.extend(core.handle_datagram(dg, 0));
    }
    match &evs[..] {
        [Event::NeedsH264 {
            frame_seq: 42,
            is_keyframe: false,
            ref_frame_seq: 41,
            flags: 0x0C,
            slots: 0b0111,
            payload: p,
            ..
        }] => assert_eq!(p, &payload),
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn h264_final_fragment_with_zero_or_one_payload_bytes_completes_the_frame() {
    // An access unit whose length is a multiple of the fragment size + 0 or 1
    // leaves a last fragment of 22 or 23 wire bytes, below the 24-byte tile
    // floor. It must still complete the frame.
    for tail in [0usize, 1] {
        let mut core = test_core();
        let payload: Vec<u8> = (0..(100 + tail) as u32).map(|i| i as u8).collect();
        let dgs = fragment_frame(11, 0, false, &payload, 100);
        assert_eq!(dgs.len(), if tail == 0 { 1 } else { 2 });
        if tail == 1 {
            assert_eq!(
                dgs[1].len(),
                ghostframe_protocol::protocol::FRAME_HEADER_SIZE + 1
            );
            assert!(dgs[1].len() < 24, "premise: below the tile floor");
        }
        let mut evs = Vec::new();
        for dg in &dgs {
            evs.extend(core.handle_datagram(dg, 0));
        }
        match &evs[..] {
            [Event::NeedsH264 {
                frame_seq: 11,
                payload: p,
                ..
            }] => assert_eq!(p, &payload),
            other => panic!("tail {tail}: unexpected events: {other:?}"),
        }
    }
}

#[test]
fn h264_empty_access_unit_single_fragment_is_delivered() {
    // fragment_frame of an empty payload yields one 22-byte datagram.
    let mut core = test_core();
    let dgs = fragment_frame(12, 0, true, &[], 100);
    assert_eq!(
        dgs[0].len(),
        ghostframe_protocol::protocol::FRAME_HEADER_SIZE
    );
    let evs = core.handle_datagram(&dgs[0], 0);
    assert!(
        matches!(&evs[..], [Event::NeedsH264 { frame_seq: 12, payload, .. }] if payload.is_empty()),
        "{evs:?}"
    );
}
