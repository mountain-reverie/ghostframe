//! Integration tests for Task 12: timer-driven assembly-timeout NACK scan,
//! CDF53 tail sweep, periodic ReceiverFeedback emission, and the
//! debounced coverage-NACK queue.

use ghostframe_client_core::{ClientConfig, ClientCore, PollOutput};
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

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
/// Valid for passes 1..13, which carry no present_passes prefix.
fn valid_cdf53_payload() -> Vec<u8> {
    let mut p = Vec::new();
    for _ in 0..3 {
        p.extend_from_slice(&[0x00, 0x01, 0xFF]);
    }
    p
}

/// Same content as `valid_cdf53_payload`, but for pass 0: prefixed with a
/// 2-byte big-endian `present_passes` bitmap. The tail-sweep/debounce tests
/// in this file want the tile to claim all 14 passes so the "missing"
/// count they assert on (13 = passes 1..13) stays meaningful.
fn valid_cdf53_pass0_payload() -> Vec<u8> {
    let mut p = vec![0x3F, 0xFF]; // present_passes = 0x3FFF (all 14 passes).
    p.extend_from_slice(&valid_cdf53_payload());
    p
}

/// Drain all queued outputs, returning only raw `Datagram` payloads.
fn drain_datagrams(core: &mut ClientCore) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(o) = core.poll_transmit(0) {
        if let PollOutput::Datagram(b) = o {
            out.push(b);
        }
    }
    out
}

/// Drain all queued outputs, returning only raw `Stream` payloads.
fn drain_stream(core: &mut ClientCore) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while let Some(o) = core.poll_transmit(0) {
        if let PollOutput::Stream(b) = o {
            out.push(b);
        }
    }
    out
}

#[test]
fn on_timeout_clamps_feedback_burst_after_large_clock_gap() {
    let mut core = test_core();
    // Simulate a huge clock gap: on_timeout is next called 10s late, i.e.
    // 100 missed 100ms feedback intervals.
    core.on_timeout(10_000_000);
    let msgs = drain_stream(&mut core);
    let feedback_msgs: Vec<&Vec<u8>> = msgs.iter().filter(|m| m[0] == 0x01).collect();
    assert_eq!(
        feedback_msgs.len(),
        1,
        "expected exactly one feedback message, not a burst of missed intervals"
    );
    assert_eq!(feedback_msgs[0].len(), 18);
}

#[test]
fn feedback_emitted_every_100ms() {
    let mut core = test_core();
    core.on_timeout(100_000);
    let msgs = drain_stream(&mut core);
    assert_eq!(msgs.len(), 1, "expected one feedback message at 100ms");
    assert_eq!(msgs[0][0], 0x01);
    assert_eq!(msgs[0].len(), 18);

    core.on_timeout(200_000);
    let msgs = drain_stream(&mut core);
    assert_eq!(msgs.len(), 1, "expected one feedback message at 200ms");
    assert_eq!(msgs[0][0], 0x01);
    assert_eq!(msgs[0].len(), 18);
}

/// Feed `passes` of tile (2, 3), frame 1, all at t=0 (pass 0 carries the
/// all-14 bitmap), then let the arrival-time gap NACKs flush and drain them.
fn core_with_passes(passes: &[u8]) -> ClientCore {
    let mut core = test_core();
    for &p in passes {
        let payload = if p == 0 {
            valid_cdf53_pass0_payload()
        } else {
            valid_cdf53_payload()
        };
        let dgs = tile_datagrams(1, 2, 3, Codec::Cdf53, p, &payload, 1200);
        core.handle_datagram(&dgs[0], 0);
    }
    core.on_timeout(100_000);
    drain_datagrams(&mut core);
    drain_stream(&mut core);
    core
}

/// Run the clock from 1.5 s to `end_us` in 50 ms steps and return every
/// `pass_idx` NACKed in that window, in order, one per entry.
fn nacked_passes_until(core: &mut ClientCore, end_us: u64) -> Vec<u8> {
    nacked_entries_until(core, end_us)
        .into_iter()
        .map(|(_, pass)| pass)
        .collect()
}

/// As `nacked_passes_until`, as `(frame_seq, pass_idx)` pairs.
fn nacked_entries_until(core: &mut ClientCore, end_us: u64) -> Vec<(u32, u8)> {
    let mut out = Vec::new();
    let mut t = 1_500_000;
    while t <= end_us {
        core.on_timeout(t);
        for d in drain_datagrams(core).into_iter().filter(|d| d[0] == 0x05) {
            for i in 0..d[1] as usize {
                let e = 2 + 7 * i;
                let fs = u32::from_le_bytes([d[e], d[e + 1], d[e + 2], d[e + 3]]);
                out.push((fs, d[e + 6]));
            }
        }
        t += 50_000;
    }
    out
}

#[test]
fn tail_sweep_renacks_a_gap_that_a_higher_pass_overtook() {
    // Passes 0..4 and 6 arrived; 5 is the gap. 7..13 are above everything
    // received, so they may simply not have been sent.
    let mut core = core_with_passes(&[0, 1, 2, 3, 4, 6]);
    let nacked = nacked_passes_until(&mut core, 1_700_000);
    assert_eq!(nacked, vec![5], "exactly the overtaken pass is re-NACKed");
}

#[test]
fn tail_sweep_stays_silent_when_nothing_overtook_a_missing_pass() {
    // Passes 0..4 arrived and 5..13 were never sent (queued behind
    // bandwidth). Silence is not evidence: no NACK, however long it lasts,
    // and the tile does not burn its sweep budget either.
    let mut core = core_with_passes(&[0, 1, 2, 3, 4]);
    let nacked = nacked_passes_until(&mut core, 10_000_000);
    assert!(nacked.is_empty(), "silence NACKed passes {nacked:?}");
    let summary = core.cdf53_coverage_summary();
    assert_eq!((summary.partial, summary.gave_up), (1, 0));
    // Premise: the tile really is incomplete and past the grace period, so
    // the sweep reached the evidence check rather than skipping the tile.
    let tiles = core.cdf53_incomplete_tiles(8);
    assert_eq!(tiles.len(), 1);
    assert_eq!(tiles[0].2, 0b1_1111, "received passes 0..4");
    assert_eq!(tiles[0].4, 0, "no sweep attempt spent");
}

#[test]
fn tail_sweep_unknown_bitmap_asks_for_pass_zero_only_once_something_arrived() {
    // Pins that evidence is recorded while the bitmap is still unknown: pass
    // 0 lost, pass 3 arrived, so `present_passes` is `None`, pass 0 is the
    // one pass guaranteed to exist, and pass 3 overtook it. (Pass 0 is the
    // only candidate here, so this does not discriminate the evidence mask
    // itself; the unit tests on `gap_evidence_mask` do.)
    let mut core = test_core();
    let dgs = tile_datagrams(1, 2, 3, Codec::Cdf53, 3, &valid_cdf53_payload(), 1200);
    core.handle_datagram(&dgs[0], 0);
    core.on_timeout(100_000);
    drain_datagrams(&mut core);
    drain_stream(&mut core);
    let nacked = nacked_passes_until(&mut core, 1_700_000);
    assert_eq!(nacked, vec![0]);
}

#[test]
fn a_later_frame_replaces_the_entry_so_no_nack_names_the_older_frame() {
    // Frame 1 has a gap at pass 5 (0..4 and 6 arrived). Frame 2 of the same
    // tile then arrives with its pass 0. The older frame is superseded:
    // its pass 5 is content the server has replaced, so asking for it would
    // fetch a stale pass. The entry is the new frame's, with no evidence.
    let mut core = core_with_passes(&[0, 1, 2, 3, 4, 6]);
    let dgs = tile_datagrams(2, 2, 3, Codec::Cdf53, 0, &valid_cdf53_pass0_payload(), 1200);
    core.handle_datagram(&dgs[0], 200_000);
    core.on_timeout(300_000);
    drain_datagrams(&mut core);
    drain_stream(&mut core);
    let nacked = nacked_entries_until(&mut core, 5_000_000);
    assert!(
        nacked.iter().all(|(fs, _)| *fs != 1),
        "the superseded frame's gap was NACKed: {nacked:?}"
    );
    assert!(nacked.is_empty(), "silence NACKed: {nacked:?}");
}

#[test]
fn tail_sweep_keeps_asking_for_a_lost_gap_until_it_gives_up() {
    // Repair is not lost: a real gap is re-asked every sweep, up to the
    // budget (6), and then the tile stops.
    let mut core = core_with_passes(&[0, 1, 2, 3, 4, 6]);
    let nacked = nacked_passes_until(&mut core, 20_000_000);
    assert_eq!(nacked, vec![5; 6]);
}

#[test]
fn coverage_nack_debounce_suppresses_pass_that_arrives_in_window() {
    let mut core = test_core();
    // Pass 0 valid at t=0.
    let dgs0 = tile_datagrams(1, 5, 6, Codec::Cdf53, 0, &valid_cdf53_pass0_payload(), 1200);
    core.handle_datagram(&dgs0[0], 0);
    drain_datagrams(&mut core);
    drain_stream(&mut core);

    // Pass 2 arrives next (gap-detection: pass 1 missing -> queued for
    // debounced NACK).
    let dgs2 = tile_datagrams(1, 5, 6, Codec::Cdf53, 2, &valid_cdf53_payload(), 1200);
    core.handle_datagram(&dgs2[0], 1_000);
    // No NACK datagram before the debounce window closes.
    assert!(
        drain_datagrams(&mut core).into_iter().all(|d| d[0] != 0x05),
        "no NACK before debounce window"
    );

    // Pass 1 arrives validly within the 50ms debounce window.
    let dgs1 = tile_datagrams(1, 5, 6, Codec::Cdf53, 1, &valid_cdf53_payload(), 1200);
    core.handle_datagram(&dgs1[0], 10_000);
    drain_datagrams(&mut core);
    drain_stream(&mut core);

    // Debounce deadline fires; the pending NACK for pass 1 is re-checked
    // against live coverage and suppressed since it arrived.
    core.on_timeout(60_000);
    assert!(
        drain_datagrams(&mut core).into_iter().all(|d| d[0] != 0x05),
        "pass 1 arrived within debounce window; NACK must be suppressed"
    );
}

#[test]
fn coverage_nack_debounce_sends_pass_that_never_arrives() {
    let mut core = test_core();
    let dgs0 = tile_datagrams(1, 7, 8, Codec::Cdf53, 0, &valid_cdf53_pass0_payload(), 1200);
    core.handle_datagram(&dgs0[0], 0);
    drain_datagrams(&mut core);
    drain_stream(&mut core);

    // Pass 2 arrives; gap detection queues pass 1 for debounced NACK.
    let dgs2 = tile_datagrams(1, 7, 8, Codec::Cdf53, 2, &valid_cdf53_payload(), 1200);
    core.handle_datagram(&dgs2[0], 1_000);
    drain_datagrams(&mut core);
    drain_stream(&mut core);

    // Pass 1 never arrives. Debounce deadline (armed at t=1000+50ms) fires
    // -> NACK sent. Drive on_timeout forward past intervening deadlines
    // (e.g. the ack batcher's own 5ms flush) up to the debounce point.
    core.on_timeout(60_000);
    let nacks: Vec<Vec<u8>> = drain_datagrams(&mut core)
        .into_iter()
        .filter(|d| d[0] == 0x05)
        .collect();
    assert!(
        !nacks.is_empty(),
        "expected NACK for pass 1 which never arrived"
    );
}
