//! Integration tests for the full datagram reassembly pipeline
//! (`ClientCore::handle_datagram`). Datagrams are generated with the
//! server-side `fragment_tile` so the tests exercise the exact wire format
//! the client sees in production.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput};
use ghostframe_protocol::protocol::{
    build_frame_dimensions_datagram, fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG,
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
    // Drain the Hello stream message so tests observe a clean outbox.
    while core.poll_transmit(0).is_some() {}
    core
}

fn tile_datagrams(
    frame_seq: u32,
    x: u8,
    y: u8,
    codec: Codec,
    pass: u8,
    payload: &[u8],
    mtu_payload: usize,
) -> Vec<Vec<u8>> {
    fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: y,
            codec,
            generation: 1,
            pass,
            timestamp_us: 0,
        },
        payload,
        mtu_payload,
    )
}

/// A CDF53 pass payload whose three channels each RLE-decode to 128 zero
/// bytes: `[u16 BE len=1][0xFF]` per channel (0xFF => 128-byte zero run).
/// A valid **pass 0** payload: present_passes prefix (bit 0 set, naming
/// only pass 0 itself) followed by the 3-channel block.
fn valid_cdf53_payload() -> Vec<u8> {
    let mut p = vec![0x00, 0x01]; // present_passes = 0x0001
    for _ in 0..3 {
        p.extend_from_slice(&[0x00, 0x01, 0xFF]);
    }
    p
}

#[test]
fn solid_tile_roundtrip_single_fragment() {
    let mut core = test_core();
    // Solid payload is BGRA [0x11, 0x22, 0x33, 0xFF].
    let dgs = tile_datagrams(1, 3, 4, Codec::Solid, 0, &[0x11, 0x22, 0x33, 0xFF], 1200);
    assert_eq!(dgs.len(), 1);
    let evs = core.handle_datagram(&dgs[0], 1_000);
    match &evs[..] {
        [Event::TileReady {
            frame_seq: 1,
            tile_x: 3,
            tile_y: 4,
            rgba,
        }] => {
            assert_eq!(&rgba[..4], &[0x33, 0x22, 0x11, 255]); // BGRA -> RGBA
            assert_eq!(rgba.len(), 4096);
            // Every pixel is the same expanded color.
            assert_eq!(&rgba[4..8], &[0x33, 0x22, 0x11, 255]);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn multi_fragment_raw_tile_completes_out_of_order() {
    let mut core = test_core();
    // 4096-byte raw BGRA payload, distinct per byte so we can verify the
    // exact concatenation + swizzle.
    let payload: Vec<u8> = (0..4096).map(|i| (i % 256) as u8).collect();
    let dgs = tile_datagrams(5, 1, 2, Codec::Raw, 0, &payload, 1200);
    assert_eq!(dgs.len(), 4); // 4096 / 1200 -> 4 fragments

    // Deliver out of order: 3, 0, 2, 1.
    assert!(core.handle_datagram(&dgs[3], 100).is_empty());
    assert!(core.handle_datagram(&dgs[0], 101).is_empty());
    assert!(core.handle_datagram(&dgs[2], 102).is_empty());
    let evs = core.handle_datagram(&dgs[1], 103);

    match &evs[..] {
        [Event::TileReady {
            frame_seq: 5,
            tile_x: 1,
            tile_y: 2,
            rgba,
        }] => {
            assert_eq!(rgba.len(), 4096);
            // Verify swizzle for the first pixel: payload BGRA
            // [0,1,2,3] -> RGBA [2,1,0,3].
            assert_eq!(&rgba[..4], &[2, 1, 0, 3]);
            // Spot-check pixel 10: payload bytes 40..44 = [40,41,42,43].
            assert_eq!(&rgba[40..44], &[42, 41, 40, 43]);
        }
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn stale_assembly_evicted_at_threshold_2() {
    let mut core = test_core();
    // Frame 1: a 2-fragment raw tile, deliver only fragment 0 (incomplete).
    let payload: Vec<u8> = (0..2400).map(|i| (i % 256) as u8).collect();
    let f1 = tile_datagrams(1, 0, 0, Codec::Raw, 0, &payload, 1200);
    assert_eq!(f1.len(), 2);
    assert!(core.handle_datagram(&f1[0], 10).is_empty());

    // Frames 2, 3, 4: complete single-fragment solid tiles advance
    // latest_frame_seq. At frame 4, threshold = 4 - 2 = 2, so frame 1's
    // incomplete assembly (1 < 2) is evicted.
    for seq in 2..=4u32 {
        let d = tile_datagrams(seq, 9, 9, Codec::Solid, 0, &[1, 2, 3, 255], 1200);
        let _ = core.handle_datagram(&d[0], 10 + seq as u64);
    }

    // Now deliver frame 1's missing fragment late: the bucket is gone, so
    // no TileReady is produced.
    let evs = core.handle_datagram(&f1[1], 100);
    assert!(
        !evs.iter()
            .any(|e| matches!(e, Event::TileReady { frame_seq: 1, .. })),
        "evicted frame-1 assembly must not complete: {evs:?}"
    );

    // Feedback lost counter grew (2 expected fragments, 1 received -> 1 lost).
    let fb = core.encode_feedback(200);
    // ReceiverFeedback layout: [0]=0x06? we only assert datagrams_lost > 0.
    // datagrams_lost is at bytes [12..16] BE (after 8-byte ts + 4-byte recv).
    let lost = u32::from_be_bytes([fb[12], fb[13], fb[14], fb[15]]);
    assert!(lost >= 1, "expected lost >= 1, got {lost} (fb={fb:?})");
}

#[test]
fn sentinel_emits_frame_dimensions() {
    let mut core = test_core();
    let dg = build_frame_dimensions_datagram(7, 0, 1920, 1080);
    let evs = core.handle_datagram(&dg, 1_000);
    match &evs[..] {
        [Event::FrameDimensions {
            width: 1920,
            height: 1080,
        }] => {}
        other => panic!("unexpected events: {other:?}"),
    }
}

#[test]
fn an_eviction_datagram_becomes_an_event() {
    let mut core = test_core();
    let dg = ghostframe_protocol::eviction::build_eviction_datagram(
        ghostframe_protocol::eviction::EvictionReason::DisplacedByNewSession,
    );

    let events = core.handle_datagram(&dg, 1_000);

    assert!(
        events.iter().any(|e| matches!(
            e,
            Event::Evicted {
                reason: ghostframe_protocol::eviction::EvictionReason::DisplacedByNewSession
            }
        )),
        "an eviction datagram must surface as an event, not be silently \
         dropped as an unknown tile: got {events:?}"
    );
}

#[test]
fn an_eviction_datagram_is_not_acked_and_leaves_no_assembly_state() {
    let mut core = test_core();
    let dg = ghostframe_protocol::eviction::build_eviction_datagram(
        ghostframe_protocol::eviction::EvictionReason::DisplacedByNewSession,
    );

    let _ = core.handle_datagram(&dg, 1_000);

    // It must not be ACKed as though it were tile content: drive the ACK
    // batcher's own deadline and confirm nothing is emitted.
    if let Some(t) = core.poll_timeout() {
        let _ = core.on_timeout(t);
    }
    let mut saw_ack = false;
    while let Some(out) = core.poll_transmit(2_000) {
        if let PollOutput::Datagram(b) = out {
            if b.first() == Some(&ghostframe_protocol::ack::ACK_BATCH_MSG_TYPE) {
                saw_ack = true;
            }
        }
    }
    assert!(
        !saw_ack,
        "an eviction notice must not be ACKed as tile content"
    );

    // And it must not fall through into tile assembly as a Codec::Skip tile
    // at the eviction sentinel coordinates (254, 254).
    let evs = core.handle_datagram(&dg, 2_000);
    assert!(
        !evs.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "eviction sentinel must never reach tile assembly: {evs:?}"
    );
}

#[test]
fn cdf53_ack_deferred_until_prevalidation() {
    let mut core = test_core();

    // Valid CDF53 pass: prevalidation succeeds -> TileReady + deferred ACK.
    let dgs = tile_datagrams(1, 2, 3, Codec::Cdf53, 0, &valid_cdf53_payload(), 1200);
    assert_eq!(dgs.len(), 1);

    // No ACK datagram is emitted synchronously during handle_datagram (the
    // ACK batcher buffers a single entry until its deadline).
    let evs = core.handle_datagram(&dgs[0], 1_000);
    assert!(
        evs.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "valid cdf53 must produce TileReady: {evs:?}"
    );
    assert!(
        core.poll_transmit(1_000).is_none(),
        "no ACK should flush before the batcher deadline"
    );

    // Drive the ACK batcher deadline; the flushed ACK batch datagram (0x03)
    // now appears.
    let deadline = core.poll_timeout().expect("ack batcher deadline pending");
    let _ = core.on_timeout(deadline);
    let ack = core
        .poll_transmit(deadline)
        .expect("ACK datagram after deadline");
    match ack {
        // AckBatch (ACK_BATCH_MSG_TYPE) rides the Datagram channel as 0x06;
        // decode-error messages ride the Stream channel.
        PollOutput::Datagram(buf) => assert_eq!(
            buf[0],
            ghostframe_protocol::ack::ACK_BATCH_MSG_TYPE,
            "expected AckBatch datagram"
        ),
        other => panic!("expected ACK datagram, got {other:?}"),
    }
    // Drain any trailing outputs.
    while core.poll_transmit(deadline).is_some() {}

    // Corrupt CDF53 pass (truncated: only one channel present) ->
    // prevalidation fails: a Stream decode-error message is emitted and NO
    // ACK entry is queued for it.
    let corrupt = tile_datagrams(2, 4, 5, Codec::Cdf53, 0, &[0x00, 0x01, 0xFF], 1200);
    let evs = core.handle_datagram(&corrupt[0], 2_000);
    assert!(
        evs.iter().any(|e| matches!(e, Event::DecodeError { .. })),
        "corrupt cdf53 must produce DecodeError event: {evs:?}"
    );

    // The decode-error Stream message [0x04, codec, x, y, code] is queued.
    let mut saw_stream_error = false;
    let mut saw_ack = false;
    while let Some(out) = core.poll_transmit(2_000) {
        match out {
            PollOutput::Stream(b) if b.first() == Some(&0x04) => saw_stream_error = true,
            PollOutput::Datagram(b)
                if b.first() == Some(&ghostframe_protocol::ack::ACK_BATCH_MSG_TYPE) =>
            {
                saw_ack = true
            }
            _ => {}
        }
    }
    // Flush any pending batcher deadlines and re-check for a stray ACK
    // datagram (a NACK is 0x05, which is fine and not an ACK).
    if let Some(t) = core.poll_timeout() {
        let _ = core.on_timeout(t);
        while let Some(out) = core.poll_transmit(t) {
            if let PollOutput::Datagram(b) = out {
                if b.first() == Some(&0x04) {
                    saw_ack = true;
                }
            }
        }
    }
    assert!(saw_stream_error, "expected decode-error stream message");
    assert!(!saw_ack, "corrupt cdf53 must not produce an ACK");
}

// ── datagram_counts: the e2e gates' measurement ──────────────────────────────

/// `datagram_counts` must split datagrams the way `handle_datagram` dispatches
/// them, because five e2e tests decide pass/fail on those two numbers.
///
/// Both tile and frame datagrams are built by the **server's** `fragment_tile`
/// / `fragment_frame`, so the discriminator under test is the one production
/// actually sets. Asserting against a hand-written byte with bit 31 flipped
/// would pass just as happily if the classifier were reading the wrong bit.
#[test]
fn datagram_counts_split_tile_and_frame_as_the_dispatch_does() {
    use ghostframe_protocol::protocol::fragment_frame;

    let mut core = test_core();
    assert_eq!(
        core.datagram_counts(),
        (0, 0),
        "a fresh core has counted nothing"
    );

    // One tile, fragmented small enough to span several datagrams: the
    // counter is per datagram, not per tile.
    //
    // The payload is deliberately not a decodable pass. A datagram counts
    // because it arrived and was classified, not because its tile decoded --
    // otherwise the counts would silently under-report exactly on the lossy
    // and malformed sessions where they matter most. Moving the increment
    // below the decode would fail this.
    let tile = tile_datagrams(1, 0, 0, Codec::Cdf53, 0, &[0x5Au8; 200], 32);
    assert!(
        tile.len() > 1,
        "tile payload should span multiple datagrams"
    );
    for dg in &tile {
        core.handle_datagram(dg, 0);
    }
    assert_eq!(
        core.datagram_counts(),
        (tile.len() as u64, 0),
        "every tile datagram counts on the tile side, and none on the frame side"
    );

    let frame = fragment_frame(7, 0, true, &[0x41u8; 3000], 1200);
    assert!(
        frame.len() > 1,
        "frame payload should span multiple datagrams"
    );
    for dg in &frame {
        core.handle_datagram(dg, 0);
    }
    assert_eq!(
        core.datagram_counts(),
        (tile.len() as u64, frame.len() as u64),
        "frame datagrams must not be attributed to the tile side"
    );
}

/// Anything that is not a protocol tile/frame datagram must leave both
/// counters alone. A counter that drifts upward on ping/pong would make
/// `tile_seen` true on a session that never received a tile -- the exact
/// false pass these gates exist to prevent.
#[test]
fn datagram_counts_ignore_non_protocol_traffic() {
    use ghostframe_protocol::protocol::{TileParityEnvelope, TILE_PARITY_ENVELOPE};

    let mut core = test_core();

    core.handle_datagram(b"", 0);
    core.handle_datagram(b"pong", 0);
    // Long enough to pass the ping/pong guard, too short to carry a header.
    core.handle_datagram(&[0u8; 22], 0);
    assert_eq!(
        core.datagram_counts(),
        (0, 0),
        "empty, ping/pong and header-less datagrams are not protocol datagrams"
    );

    // A parity envelope carries recovered tile bytes but is not itself a tile
    // datagram. Counting it as one would inflate the tile side only on
    // FEC-enabled sessions, which is the hardest kind of skew to notice.
    let mut parity = Vec::new();
    TileParityEnvelope {
        group_first_wire_seq: 1,
        k: 2,
        parity_idx: 0,
        source_lens: vec![40, 40],
        parity_payload: vec![0u8; 40],
    }
    .encode(&mut parity);
    assert_eq!(parity[0], TILE_PARITY_ENVELOPE, "envelope discriminator");
    core.handle_datagram(&parity, 0);
    assert_eq!(
        core.datagram_counts(),
        (0, 0),
        "parity envelopes are counted on neither side"
    );
}

/// One Solid tile datagram for `(x, 0)` whose header carries `sent_us` as
/// the sender's emit stamp, the way the server's emitter writes it.
fn stamped_solid(frame_seq: u32, x: u8, sent_us: u32) -> Vec<u8> {
    let mut dg = tile_datagrams(frame_seq, x, 0, Codec::Solid, 0, &[1, 2, 3, 255], 1200)
        .pop()
        .expect("one fragment");
    dg[12..16].copy_from_slice(&sent_us.to_be_bytes());
    dg
}

fn suspension_reported(core: &mut ClientCore, now_us: u64) -> bool {
    ghostframe_protocol::feedback::ReceiverFeedback::decode(&core.encode_feedback(now_us))
        .expect("feedback decodes")
        .suspension_detected
}

/// The wiring from the datagram header to the suspension flag: the stamp
/// the core reads is the one at `[12..16]`, and a sentinel in between does
/// not stand in for a tile datagram.
///
/// A static screen, then one change nine seconds later, must not tell the
/// server the path was suspended -- it answers that by forcing H.264.
#[test]
fn content_after_an_idle_screen_is_not_reported_as_a_suspension() {
    let mut core = test_core();
    core.handle_datagram(&stamped_solid(1, 0, 1_000_000), 50_000);
    // The sender was quiet for 9.4 s; so, therefore, was the link.
    core.handle_datagram(&stamped_solid(2, 1, 10_400_000), 9_450_000);
    assert!(!suspension_reported(&mut core, 9_500_000));

    // A frame-dimensions sentinel arriving late carries the capture clock,
    // not an emit stamp, and must not be read as one.
    let sentinel = ghostframe_protocol::protocol::build_frame_dimensions_datagram(3, 7, 64, 64);
    core.handle_datagram(&sentinel, 12_000_000);
    assert!(!suspension_reported(&mut core, 12_050_000));

    // Whereas the path sitting on a datagram is still reported: sent 10 ms
    // after the last tile datagram, delivered 3 s after it.
    core.handle_datagram(&stamped_solid(4, 2, 10_410_000), 12_450_000);
    assert!(suspension_reported(&mut core, 12_500_000));
}
