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
fn valid_cdf53_payload() -> Vec<u8> {
    let mut p = Vec::new();
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
        // AckBatch (ACK_BATCH_MSG_TYPE) rides the Datagram channel as 0x04;
        // decode-error messages share the byte but ride the Stream channel.
        PollOutput::Datagram(buf) => assert_eq!(buf[0], 0x04, "expected AckBatch datagram"),
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
            PollOutput::Datagram(b) if b.first() == Some(&0x04) => saw_ack = true,
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
