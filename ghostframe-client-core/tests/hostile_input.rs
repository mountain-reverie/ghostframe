//! A hostile or buggy server must not be able to drive the client into
//! unbounded allocation or out-of-grid work through the reassembly path.
//!
//! Each cap has two tests: one where the hostile input is refused, and one
//! where the largest legitimate input still goes through. The second kind is
//! what stops a cap from being tightened until it rejects real frames.
//! Every hostile test asserts on observable state ([`AssemblyStats`] and the
//! emitted events), so removing the guard it names makes it fail.

use ghostframe_client_core::{
    AssemblyStats, ClientConfig, ClientCore, Event, PollOutput, MAX_FRAME_DIMENSION,
    MAX_FRAME_FRAGMENTS, MAX_H264_ACCESS_UNIT_BYTES,
};
use ghostframe_protocol::protocol::{
    build_frame_dimensions_datagram, fragment_frame, fragment_tile, Codec, TileFragmentInputs,
    TILE_DATAGRAM_FLAG,
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

/// Fragments of a tile pass at `(x, y)`, `pass`, cut to `mtu_payload`.
fn tile_dgs(frame_seq: u32, x: u8, y: u8, pass: u8, payload: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: y,
            codec: Codec::Solid,
            generation: 1,
            pass,
            timestamp_us: 0,
        },
        payload,
        mtu,
    )
}

/// Overwrite the `frag_total` field of an already-built datagram, as a
/// hostile server would.
fn forge_frag_total(dg: &mut [u8], frag_total: u16) {
    dg[6..8].copy_from_slice(&frag_total.to_be_bytes());
}

fn forge_frag_idx(dg: &mut [u8], frag_idx: u16) {
    dg[4..6].copy_from_slice(&frag_idx.to_be_bytes());
}

fn stats(core: &ClientCore) -> AssemblyStats {
    core.assembly_stats()
}

// ---------------------------------------------------------------------------
// P0-3: frag_total
// ---------------------------------------------------------------------------

/// A single Solid tile datagram that is complete on arrival.
fn solid_dg(frame_seq: u32, x: u8, y: u8) -> Vec<u8> {
    tile_dgs(frame_seq, x, y, 0, &[1, 2, 3, 255], 64).remove(0)
}

fn tile_ready_count(evs: &[Event]) -> usize {
    evs.iter()
        .filter(|e| matches!(e, Event::TileReady { .. }))
        .count()
}

#[test]
fn a_tile_claiming_other_than_one_fragment_is_dropped_and_stores_nothing() {
    // A tile pass is one datagram (no payload exceeds MAX_TILE_PAYLOAD), so any
    // other fragment count is hostile or from a server that no longer exists.
    let mut core = test_core();
    // Positive control: the same datagram with frag_total 1 completes.
    assert_eq!(
        tile_ready_count(&core.handle_datagram(&solid_dg(1, 0, 0), 0)),
        1
    );
    for total in [0u16, 2, 3, 64, 65, u16::MAX] {
        let mut dg = solid_dg(2, 0, 0);
        forge_frag_total(&mut dg, total);
        let evs = core.handle_datagram(&dg, 0);
        assert!(
            evs.is_empty(),
            "frag_total {total} must be dropped: {evs:?}"
        );
        assert_eq!(stats(&core).tile_assemblies, 0, "frag_total {total}");
        assert_eq!(stats(&core).tile_assembly_bytes, 0, "frag_total {total}");
    }
}

#[test]
fn a_fan_out_of_multi_fragment_claims_with_a_pinned_frame_seq_stores_nothing() {
    // Pin frame_seq, vary the key, claim 65535 fragments.
    let mut core = test_core();
    let base = solid_dg(1, 0, 0);
    for i in 0..5000u32 {
        let mut dg = base.clone();
        dg[16] = (i % 200) as u8; // tile_x
        dg[17] = ((i / 200) % 200) as u8; // tile_y
        forge_frag_total(&mut dg, u16::MAX);
        core.handle_datagram(&dg, 0);
    }
    assert_eq!(stats(&core).tile_assemblies, 0);
}

#[test]
fn the_largest_cdf53_pass_is_one_datagram_and_completes() {
    // MAX_PASS_PAYLOAD (776 bytes): every plane byte is 0xFF, so each costs the
    // 2-byte escape; pass 0 also carries the 2-byte present_passes prefix.
    let mut core = test_core();
    let mut payload = vec![0x00, 0x01];
    for _ in 0..3 {
        let mut plane = Vec::new();
        for _ in 0..128 {
            plane.extend_from_slice(&[0x7F, 0xFF]);
        }
        payload.extend_from_slice(&(plane.len() as u16).to_be_bytes());
        payload.extend_from_slice(&plane);
    }
    assert_eq!(
        payload.len(),
        ghostframe_protocol::codec::cdf53::MAX_PASS_PAYLOAD
    );
    let dgs = fragment_tile(
        &TileFragmentInputs {
            frame_seq: 1 | TILE_DATAGRAM_FLAG,
            tile_x: 3,
            tile_y: 4,
            codec: Codec::Cdf53,
            generation: 1,
            pass: 0,
            timestamp_us: 0,
        },
        &payload,
        ghostframe_protocol::protocol::MAX_TILE_PAYLOAD,
    );
    assert_eq!(dgs.len(), 1, "it must not need fragmenting");
    let evs = core.handle_datagram(&dgs[0], 0);
    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::TileReady {
                tile_x: 3,
                tile_y: 4,
                ..
            }
        )),
        "the largest pass must complete: {evs:?}"
    );
}

/// True if the core queues any ACK datagram after `handle_datagram` calls.
fn acked(core: &mut ClientCore) -> bool {
    core.on_timeout(10_000_000);
    let mut any = false;
    while let Some(out) = core.poll_transmit(10_000_000) {
        if matches!(out, PollOutput::Datagram(_)) {
            any = true;
        }
    }
    any
}

#[test]
fn accepted_tile_is_acked_but_refused_ones_are_not() {
    // Positive control first, or "no ACK" below proves nothing.
    let mut core = test_core();
    core.handle_datagram(&tile_dgs(1, 0, 0, 0, &[1u8; 64], 64)[0], 0);
    assert!(acked(&mut core), "control: an accepted tile must be ACKed");

    // More than one fragment, off the grid: refused before the ACK, so the
    // server retransmits instead of counting them delivered.
    let mut core = test_core();
    let mut dg = tile_dgs(1, 0, 0, 0, &[1u8; 64], 64).remove(0);
    forge_frag_total(&mut dg, 2);
    core.handle_datagram(&dg, 0);
    assert!(!acked(&mut core), "multi-fragment tile ACKed");

    let mut core = test_core();
    dims_events(&mut core, 640, 480);
    core.handle_datagram(&tile_dgs(1, 30, 0, 0, &[1u8; 64], 64)[0], 0);
    assert!(!acked(&mut core), "off-grid tile ACKed");
}

#[test]
fn h264_frag_total_above_cap_allocates_nothing() {
    let mut core = test_core();
    let mut dg = fragment_frame(7, 0, true, &[1u8; 64], 64).remove(0);
    forge_frag_total(&mut dg, u16::MAX);
    core.handle_datagram(&dg, 0);
    assert_eq!(stats(&core).frame_assemblies, 0);
    forge_frag_total(&mut dg, MAX_FRAME_FRAGMENTS + 1);
    core.handle_datagram(&dg, 0);
    assert_eq!(stats(&core).frame_assemblies, 0);
}

#[test]
fn h264_frag_total_at_cap_is_accepted() {
    let mut core = test_core();
    let mut dg = fragment_frame(7, 0, true, &[1u8; 64], 64).remove(0);
    forge_frag_total(&mut dg, MAX_FRAME_FRAGMENTS);
    core.handle_datagram(&dg, 0);
    assert_eq!(stats(&core).frame_assemblies, 1);
}

// ---------------------------------------------------------------------------
// H.264 access-unit size
// ---------------------------------------------------------------------------

fn frame_fragment(frame_seq: u32, idx: u16, total: u16, payload_len: usize) -> Vec<u8> {
    let mut dg =
        fragment_frame(frame_seq, 0, false, &vec![0xAB; payload_len], payload_len).remove(0);
    forge_frag_idx(&mut dg, idx);
    forge_frag_total(&mut dg, total);
    dg
}

#[test]
fn h264_access_unit_over_the_byte_cap_is_abandoned() {
    let mut core = test_core();
    let chunk = 1024 * 1024;
    let n = MAX_H264_ACCESS_UNIT_BYTES / chunk + 1;
    let mut evs = Vec::new();
    for i in 0..n {
        evs.extend(core.handle_datagram(&frame_fragment(3, i as u16, n as u16, chunk), 0));
    }
    assert!(evs.is_empty(), "oversize access unit must not be delivered");
    assert_eq!(
        stats(&core).frame_assemblies,
        0,
        "and must not stay buffered"
    );
}

#[test]
fn h264_access_unit_exactly_at_the_cap_is_delivered() {
    let mut core = test_core();
    let chunk = 1024 * 1024;
    let n = MAX_H264_ACCESS_UNIT_BYTES / chunk;
    let mut evs = Vec::new();
    for i in 0..n {
        evs.extend(core.handle_datagram(&frame_fragment(3, i as u16, n as u16, chunk), 0));
    }
    match &evs[..] {
        [Event::NeedsH264 { payload, .. }] => assert_eq!(payload.len(), MAX_H264_ACCESS_UNIT_BYTES),
        other => panic!("expected one NeedsH264, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// P0-4: FrameDimensions
// ---------------------------------------------------------------------------

fn dims_events(core: &mut ClientCore, w: u32, h: u32) -> Vec<(u32, u32)> {
    core.handle_datagram(&build_frame_dimensions_datagram(1, 0, w, h), 0)
        .into_iter()
        .filter_map(|e| match e {
            Event::FrameDimensions { width, height } => Some((width, height)),
            _ => None,
        })
        .collect()
}

#[test]
fn oversize_or_zero_dimensions_are_dropped() {
    for (w, h) in [
        (u32::MAX, u32::MAX),
        (MAX_FRAME_DIMENSION + 1, 1080),
        (1920, MAX_FRAME_DIMENSION + 1),
        (0, 1080),
        (1920, 0),
    ] {
        let mut core = test_core();
        assert!(dims_events(&mut core, w, h).is_empty(), "{w}x{h} must drop");
    }
}

#[test]
fn legitimate_dimensions_pass_including_the_limit() {
    for (w, h) in [
        (1920, 1080),
        (3840, 2160),
        (7680, 2160), // dual 4K side by side
        (3840, 4320), // dual 4K stacked
        (MAX_FRAME_DIMENSION, MAX_FRAME_DIMENSION),
        (1, 1),
    ] {
        let mut core = test_core();
        assert_eq!(dims_events(&mut core, w, h), vec![(w, h)], "{w}x{h}");
    }
}

#[test]
fn a_dropped_dimensions_message_does_not_change_the_grid() {
    let mut core = test_core();
    assert_eq!(dims_events(&mut core, 640, 480).len(), 1);
    assert!(dims_events(&mut core, u32::MAX, u32::MAX).is_empty());
    // Grid is still 20x15: a tile at x=30 is refused.
    let dgs = tile_dgs(2, 30, 0, 0, &[1u8; 128], 64);
    core.handle_datagram(&dgs[0], 0);
    assert_eq!(stats(&core).tile_assemblies, 0);
}

// ---------------------------------------------------------------------------
// Tile coordinates against the grid
// ---------------------------------------------------------------------------

#[test]
fn tiles_outside_the_grid_are_refused_and_edge_tiles_are_not() {
    let mut core = test_core();
    // 650x490 -> ceil(650/32)=21 columns, ceil(490/32)=16 rows.
    assert_eq!(dims_events(&mut core, 650, 490).len(), 1);

    for (x, y) in [(21u8, 0u8), (0, 16), (200, 200), (253, 253)] {
        let evs = core.handle_datagram(&solid_dg(2, x, y), 0);
        assert_eq!(tile_ready_count(&evs), 0, "({x},{y}) is off the grid");
    }
    // The last row/column are partial tiles but legitimate.
    for (x, y) in [(20u8, 15u8), (0, 0), (20, 0), (0, 15)] {
        let evs = core.handle_datagram(&solid_dg(2, x, y), 0);
        assert_eq!(tile_ready_count(&evs), 1, "({x},{y}) is on the grid");
    }
}

#[test]
fn before_any_dimensions_tiles_are_not_refused() {
    // The dimensions datagram is unreliable; tiles that beat it must not be
    // treated as hostile.
    let mut core = test_core();
    let evs = core.handle_datagram(&solid_dg(2, 100, 60), 0);
    assert_eq!(tile_ready_count(&evs), 1);
}
