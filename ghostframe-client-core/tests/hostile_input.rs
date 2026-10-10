//! A hostile or buggy server must not be able to drive the client into
//! unbounded allocation or out-of-grid work through the reassembly path.
//!
//! Each cap has two tests: one where the hostile input is refused, and one
//! where the largest legitimate input still goes through. The second kind is
//! what stops a cap from being tightened until it rejects real frames.
//! Every hostile test asserts on observable state ([`AssemblyStats`] and the
//! emitted events), so removing the guard it names makes it fail.

use ghostframe_client_core::{
    AssemblyStats, ClientConfig, ClientCore, Event, PollOutput, MAX_ASSEMBLY_BYTES,
    MAX_FRAME_DIMENSION, MAX_FRAME_FRAGMENTS, MAX_H264_ACCESS_UNIT_BYTES, MAX_LIVE_ASSEMBLIES,
    MAX_TILE_FRAGMENTS,
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

/// Fragments of a Raw tile pass at `(x, y)`, `pass`, cut to `mtu_payload`.
fn tile_dgs(frame_seq: u32, x: u8, y: u8, pass: u8, payload: &[u8], mtu: usize) -> Vec<Vec<u8>> {
    fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: x,
            tile_y: y,
            codec: Codec::Raw,
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

#[test]
fn tile_frag_total_above_cap_allocates_nothing() {
    let mut core = test_core();
    let mut dg = tile_dgs(1, 0, 0, 0, &[7u8; 64], 64).remove(0);
    forge_frag_total(&mut dg, u16::MAX);
    core.handle_datagram(&dg, 0);
    assert_eq!(stats(&core).tile_assemblies, 0);
    assert_eq!(stats(&core).tile_assembly_bytes, 0);

    forge_frag_total(&mut dg, MAX_TILE_FRAGMENTS + 1);
    core.handle_datagram(&dg, 0);
    assert_eq!(stats(&core).tile_assemblies, 0);
}

#[test]
fn tile_frag_total_fan_out_with_pinned_frame_seq_is_bounded() {
    // The reported attack: pin frame_seq, vary the key, claim 65535 frags.
    let mut core = test_core();
    let base = tile_dgs(1, 0, 0, 0, &[7u8; 64], 64).remove(0);
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
fn tile_with_the_maximum_fragment_count_still_assembles() {
    let mut core = test_core();
    // A 4096-byte Raw tile cut into exactly MAX_TILE_FRAGMENTS fragments.
    let payload: Vec<u8> = (0..4096u32).map(|i| (i % 251) as u8).collect();
    let dgs = tile_dgs(1, 3, 4, 0, &payload, 64);
    assert_eq!(dgs.len(), MAX_TILE_FRAGMENTS as usize);
    let mut evs = Vec::new();
    for dg in &dgs {
        evs.extend(core.handle_datagram(dg, 0));
    }
    assert!(
        evs.iter().any(|e| matches!(
            e,
            Event::TileReady {
                tile_x: 3,
                tile_y: 4,
                ..
            }
        )),
        "a legitimate 64-fragment tile must complete: {evs:?}"
    );
    assert_eq!(stats(&core).tile_assemblies, 0);
}

#[test]
fn assembly_budget_bounds_a_valid_looking_fan_out() {
    // Every datagram is individually legal (frag_total at the cap); only the
    // sum is the attack. Pinned frame_seq, so the stale sweep never helps.
    let mut core = test_core();
    let base = tile_dgs(1, 0, 0, 0, &[7u8; 64], 64).remove(0);
    let mut sent = 0u32;
    'outer: for y in 0..=253u8 {
        for x in 0..=253u8 {
            for pass in 0..16u8 {
                let mut dg = base.clone();
                dg[16] = x;
                dg[17] = y;
                dg[19] = (dg[19] & 0xF0) | pass; // pass in the low nibble
                forge_frag_total(&mut dg, MAX_TILE_FRAGMENTS);
                core.handle_datagram(&dg, 0);
                sent += 1;
                if sent >= 80_000 {
                    break 'outer;
                }
            }
        }
    }
    let s = stats(&core);
    assert!(s.tile_assemblies > 1000, "test must actually fill: {s:?}");
    assert!(
        s.tile_assembly_bytes <= MAX_ASSEMBLY_BYTES,
        "budget exceeded: {s:?}"
    );
    assert!(
        s.tile_assemblies < sent as usize,
        "some datagrams must have been refused: {s:?}"
    );
    // Independent of the counter the guard compares: no fan-out of
    // frag_total=64 assemblies can exceed what the budget could pay for.
    assert!(s.tile_assemblies <= MAX_ASSEMBLY_BYTES / (512 + 64 * 40));
    assert!(s.tile_assemblies <= MAX_LIVE_ASSEMBLIES);
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

    // Over the fragment cap, off the grid: refused before the ACK, so the
    // server retransmits instead of counting them delivered.
    let mut core = test_core();
    let mut dg = tile_dgs(1, 0, 0, 0, &[1u8; 64], 64).remove(0);
    forge_frag_total(&mut dg, MAX_TILE_FRAGMENTS + 1);
    core.handle_datagram(&dg, 0);
    assert!(!acked(&mut core), "over-cap fragment ACKed");

    let mut core = test_core();
    dims_events(&mut core, 640, 480);
    core.handle_datagram(&tile_dgs(1, 30, 0, 0, &[1u8; 64], 64)[0], 0);
    assert!(!acked(&mut core), "off-grid tile ACKed");
}

#[test]
fn a_fragment_refused_for_budget_is_not_acked() {
    let mut core = test_core();
    let base = tile_dgs(1, 0, 0, 0, &[7u8; 64], 64).remove(0);
    let mut sent = 0u32;
    'outer: for y in 0..=253u8 {
        for x in 0..=253u8 {
            let mut dg = base.clone();
            dg[16] = x;
            dg[17] = y;
            forge_frag_total(&mut dg, MAX_TILE_FRAGMENTS);
            core.handle_datagram(&dg, 0);
            sent += 1;
            if core.assembly_stats().tile_assemblies >= MAX_LIVE_ASSEMBLIES || sent > 200_000 {
                break 'outer;
            }
        }
    }
    acked(&mut core); // drain
                      // Full to the count cap (or the byte cap): a new key is refused, unACKed.
    let s = stats(&core);
    assert!(s.tile_assemblies > 1000, "must be full: {s:?}");
    let mut dg = base.clone();
    dg[16] = 253;
    dg[17] = 253;
    dg[19] |= 0x0F; // a pass nobody used yet
    forge_frag_total(&mut dg, MAX_TILE_FRAGMENTS);
    let before = stats(&core);
    core.handle_datagram(&dg, 0);
    assert_eq!(
        stats(&core).tile_assemblies,
        before.tile_assemblies,
        "a full core must refuse a new assembly"
    );
    assert!(!acked(&mut core), "refused fragment was ACKed");
}

#[test]
fn budget_returns_to_zero_on_completion_and_on_stale_sweep() {
    let mut core = test_core();
    // Completion.
    for dg in tile_dgs(1, 2, 2, 0, &[5u8; 4096], 1176) {
        core.handle_datagram(&dg, 0);
    }
    assert_eq!(stats(&core).tile_assembly_bytes, 0);
    assert_eq!(stats(&core).tile_assemblies, 0);

    // Strand some, then let the frame window move past them.
    for i in 0..10u8 {
        let dgs = tile_dgs(2, i, 0, 0, &[5u8; 4096], 1176);
        core.handle_datagram(&dgs[0], 0);
    }
    assert!(stats(&core).tile_assembly_bytes > 0);
    core.handle_datagram(&tile_dgs(20, 0, 5, 0, &[5u8; 64], 64)[0], 0);
    let s = stats(&core);
    assert_eq!(s.tile_assemblies, 0, "{s:?}");
    assert_eq!(s.tile_assembly_bytes, 0, "{s:?}");
}

#[test]
fn a_full_4k_dual_screen_frame_of_one_short_tiles_fits_the_budget() {
    // Largest legitimate in-flight load: every tile of a 2 x 3840x2160
    // frame (~16k tiles) has 3 of its 4 fragments and is waiting on the 4th.
    let mut core = test_core();
    dims_events(&mut core, 7680, 2160);
    let (gw, gh) = (240u32, 68u32); // 7680x2160 side by side
    let payload = vec![9u8; 4096];
    let mut live = 0;
    for ty in 0..gh {
        for tx in 0..gw {
            let dgs = tile_dgs(5, tx as u8, ty as u8, 0, &payload, 1176);
            assert_eq!(dgs.len(), 4);
            for dg in &dgs[..3] {
                core.handle_datagram(dg, 0);
            }
            live += 1;
        }
    }
    assert_eq!(stats(&core).tile_assemblies, live);
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
        let dgs = tile_dgs(2, x, y, 0, &[1u8; 128], 64);
        core.handle_datagram(&dgs[0], 0);
        assert_eq!(stats(&core).tile_assemblies, 0, "({x},{y}) is off the grid");
    }
    // The last row/column are partial tiles but legitimate.
    for (x, y) in [(20u8, 15u8), (0, 0), (20, 0), (0, 15)] {
        let dgs = tile_dgs(2, x, y, 0, &[1u8; 128], 64);
        core.handle_datagram(&dgs[0], 0);
        assert_eq!(stats(&core).tile_assemblies, 1, "({x},{y}) is on the grid");
        for dg in &dgs[1..] {
            core.handle_datagram(dg, 0);
        }
        assert_eq!(stats(&core).tile_assemblies, 0, "({x},{y}) completes");
    }
}

#[test]
fn a_tile_that_falls_off_a_shrunk_grid_mid_assembly_is_not_delivered() {
    let mut core = test_core();
    dims_events(&mut core, 1280, 720);
    let dgs = tile_dgs(2, 30, 10, 0, &[1u8; 128], 64);
    assert_eq!(dgs.len(), 2);
    core.handle_datagram(&dgs[0], 0);
    dims_events(&mut core, 640, 480); // 20x15: tile (30,10) is gone
                                      // The stranded assembly is purged at once, with its budget.
    assert_eq!(stats(&core).tile_assemblies, 0);
    assert_eq!(stats(&core).tile_assembly_bytes, 0);
    let evs = core.handle_datagram(&dgs[1], 0);
    assert!(
        !evs.iter().any(|e| matches!(e, Event::TileReady { .. })),
        "tile outside the new grid must not reach a renderer: {evs:?}"
    );
}

#[test]
fn before_any_dimensions_tiles_are_not_refused() {
    // The dimensions datagram is unreliable; tiles that beat it must not be
    // treated as hostile.
    let mut core = test_core();
    let dgs = tile_dgs(2, 100, 60, 0, &[1u8; 128], 64);
    core.handle_datagram(&dgs[0], 0);
    assert_eq!(stats(&core).tile_assemblies, 1);
}

// ---------------------------------------------------------------------------
// Legacy fragment parity store
// ---------------------------------------------------------------------------

#[test]
fn fragment_parity_keys_are_bounded_and_swept_with_the_window() {
    let mut core = test_core();
    let base = tile_dgs(10, 0, 0, 0, &[7u8; 64], 64).remove(0);
    // A parity datagram (frag_idx >= frag_total) with a valid 3-byte header.
    let mut parity = base.clone();
    forge_frag_total(&mut parity, 4);
    forge_frag_idx(&mut parity, 4);
    for i in 0..9000u32 {
        let mut dg = parity.clone();
        dg[16] = (i % 200) as u8;
        dg[17] = ((i / 200) % 200) as u8;
        core.handle_datagram(&dg, 0);
    }
    let n = stats(&core).fragment_parity_keys;
    assert!(n > 100, "test must actually store parity: {n}");
    assert!(n <= ghostframe_client_core::fragment_parity::MAX_PARITY_KEYS);

    // Advancing the frame window sweeps parity that has no assembly.
    let newer = tile_dgs(50, 1, 1, 0, &[7u8; 64], 64).remove(0);
    core.handle_datagram(&newer, 0);
    assert_eq!(stats(&core).fragment_parity_keys, 0);
}
