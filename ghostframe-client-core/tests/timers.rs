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
fn valid_cdf53_payload() -> Vec<u8> {
    let mut p = Vec::new();
    for _ in 0..3 {
        p.extend_from_slice(&[0x00, 0x01, 0xFF]);
    }
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
fn assembly_timeout_nacks_missing_fragments_once() {
    let mut core = test_core();
    let dgs = tile_datagrams(1, 0, 0, Codec::Raw, 0, &vec![7u8; 4096], 1200); // 4 frags
    assert_eq!(dgs.len(), 4);
    core.handle_datagram(&dgs[0], 0);

    // Advance past the 30ms scan deadline.
    let deadline = core.poll_timeout().unwrap();
    core.on_timeout(deadline.max(31_000));
    let nack = drain_datagrams(&mut core)
        .into_iter()
        .find(|d| d[0] == 0x05)
        .expect("nack sent");
    // NACK envelope: [0]=0x05, [1]=entry count.
    assert_eq!(nack[1], 3); // frags 1,2,3 missing

    // Second scan: no duplicate NACKs (dedup via nacked_frag_idxs).
    core.on_timeout(70_000);
    assert!(drain_datagrams(&mut core).into_iter().all(|d| d[0] != 0x05));
}

#[test]
fn assembly_timeout_scan_deadline_retires_once_fully_nacked() {
    let mut core = test_core();
    let dgs = tile_datagrams(1, 0, 0, Codec::Raw, 0, &vec![7u8; 4096], 1200); // 4 frags
    assert_eq!(dgs.len(), 4);
    core.handle_datagram(&dgs[0], 0);

    // Advance past the 30ms scan deadline: the scan NACKs all 3 missing
    // fragments, so every entry in `nacked_frag_idxs` now covers every
    // still-missing fragment index.
    let deadline = core.poll_timeout().unwrap();
    let now = deadline.max(31_000);
    core.on_timeout(now);
    let nack = drain_datagrams(&mut core)
        .into_iter()
        .find(|d| d[0] == 0x05)
        .expect("nack sent");
    assert_eq!(nack[1], 3);

    // Regression: a fully-NACKed assembly must stop arming the assembly
    // scan deadline in `poll_timeout`. Before the fix, `partial_since_us +
    // 30ms` (already in the past relative to `now`) stayed the min-fold
    // forever, so a "sleep until poll_timeout" driver would busy-loop.
    let next = core.poll_timeout().unwrap();
    assert!(
        next > now,
        "poll_timeout must not return a deadline <= now once every missing \
         fragment has been NACKed (got {next}, now was {now})"
    );
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
    assert_eq!(feedback_msgs[0].len(), 22);
}

#[test]
fn feedback_emitted_every_100ms() {
    let mut core = test_core();
    core.on_timeout(100_000);
    let msgs = drain_stream(&mut core);
    assert_eq!(msgs.len(), 1, "expected one feedback message at 100ms");
    assert_eq!(msgs[0][0], 0x01);
    assert_eq!(msgs[0].len(), 22);

    core.on_timeout(200_000);
    let msgs = drain_stream(&mut core);
    assert_eq!(msgs.len(), 1, "expected one feedback message at 200ms");
    assert_eq!(msgs[0][0], 0x01);
    assert_eq!(msgs[0].len(), 22);
}

#[test]
fn tail_sweep_renacks_stalled_cdf53_tile_after_1500ms() {
    let mut core = test_core();
    // One valid pass 0 at t=0 -> coverage entry created, pass_mask bit 0 set.
    let dgs = tile_datagrams(1, 2, 3, Codec::Cdf53, 0, &valid_cdf53_payload(), 1200);
    core.handle_datagram(&dgs[0], 0);
    // Drain the ACK/decode outputs from the initial arrival.
    drain_datagrams(&mut core);
    drain_stream(&mut core);

    // Sweep at 1500ms+ should NACK passes 1..13 (missing).
    core.on_timeout(1_500_000);
    // Debounced: the NACKs land in nack_batcher only after the debounce
    // window (50ms) elapses (or the sweep + debounce fire in later calls).
    core.on_timeout(1_550_000);

    let nacks: Vec<Vec<u8>> = drain_datagrams(&mut core)
        .into_iter()
        .filter(|d| d[0] == 0x05)
        .collect();
    assert!(
        !nacks.is_empty(),
        "expected re-NACK datagram(s) after tail sweep"
    );
    // Total NACK entry count across all datagrams should cover passes 1..13
    // (13 entries).
    let total: u32 = nacks.iter().map(|d| d[1] as u32).sum();
    assert_eq!(total, 13, "expected 13 missing passes re-NACKed");
}

#[test]
fn coverage_nack_debounce_suppresses_pass_that_arrives_in_window() {
    let mut core = test_core();
    // Pass 0 valid at t=0.
    let dgs0 = tile_datagrams(1, 5, 6, Codec::Cdf53, 0, &valid_cdf53_payload(), 1200);
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
    let dgs0 = tile_datagrams(1, 7, 8, Codec::Cdf53, 0, &valid_cdf53_payload(), 1200);
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
