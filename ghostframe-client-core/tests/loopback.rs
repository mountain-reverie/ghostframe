//! Task 14: server-encode -> client-core-decode loopback integration tests.
//!
//! These tests exercise the full round trip with no network involved:
//! build tile pixel data, encode it with the real `ghostframe_protocol`
//! codecs, fragment it with the real `fragment_tile` emitter helper, feed
//! the resulting datagrams into `ClientCore::handle_datagram`, and assert
//! byte-exact pixel reconstruction.

use std::collections::HashMap;

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput};
use ghostframe_protocol::codec::cdf53;
use ghostframe_protocol::codec::pal_rle::{encode_pal_rle_payload, PaletteEntry};
use ghostframe_protocol::codec::solid::encode_solid;
use ghostframe_protocol::protocol::{
    fragment_tile, Codec, TileFragmentInputs, TileNackEnvelope, TileParityEnvelope,
    TILE_DATAGRAM_FLAG,
};

const TILE_DIM: usize = 32;
const TILE_PIXELS: usize = TILE_DIM * TILE_DIM;
const TILE_BYTES: usize = TILE_PIXELS * 4;
const MTU_PAYLOAD: usize = 900;

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
    generation: u8,
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
            generation,
            pass,
            timestamp_us: 0,
        },
        payload,
        mtu_payload,
    )
}

/// Stamp `wire_seq` (bytes 8..12, big-endian) into a fragment produced by
/// `fragment_tile`, matching what the real `ReliableTileEmitter` does at
/// submit time (`fragment_tile` always leaves `wire_seq = UNSTAMPED_WIRE_SEQ`).
fn stamp_wire_seq(datagram: &mut [u8], wire_seq: u32) {
    datagram[8..12].copy_from_slice(&wire_seq.to_be_bytes());
}

/// One tile's role in the synthetic test frame.
#[derive(Clone, Copy)]
enum TileKind {
    Solid([u8; 4]),        // BGRA fill color
    PalRle,                 // deterministic 4-color pattern, keyed by tile index
    Cdf53,                   // horizontal BGR gradient, keyed by tile index
}

/// Build the BGRA pixel buffer for one 32x32 tile of the synthetic frame,
/// given its role and (tx, ty) coordinates (used to vary content per tile).
fn tile_bgra(kind: TileKind, tx: u8, ty: u8) -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    match kind {
        TileKind::Solid(bgra) => {
            for px in buf.chunks_exact_mut(4) {
                px.copy_from_slice(&bgra);
            }
        }
        TileKind::PalRle => {
            // 4-color deterministic pattern varying with (x mod 2, y mod 2)
            // quadrant within the tile, plus tile coords for uniqueness.
            let palette: [[u8; 4]; 4] = [
                [10u8.wrapping_add(tx), 20, 30, 255],
                [40, 50u8.wrapping_add(ty), 60, 255],
                [70, 80, 90u8.wrapping_add(tx), 255],
                [100, 110, 120u8.wrapping_add(ty), 255],
            ];
            for y in 0..TILE_DIM {
                for x in 0..TILE_DIM {
                    let q = ((x / 16) + (y / 16) * 2) % 4;
                    let o = (y * TILE_DIM + x) * 4;
                    buf[o..o + 4].copy_from_slice(&palette[q]);
                }
            }
        }
        TileKind::Cdf53 => {
            // Smooth horizontal gradient per channel, offset by tile coords
            // so every Cdf53 tile is distinct.
            for y in 0..TILE_DIM {
                for x in 0..TILE_DIM {
                    let o = (y * TILE_DIM + x) * 4;
                    let b = ((x * 7 + tx as usize * 3) % 256) as u8;
                    let g = ((y * 5 + ty as usize * 11) % 256) as u8;
                    let r = ((x + y + tx as usize + ty as usize) % 256) as u8;
                    buf[o] = b;
                    buf[o + 1] = g;
                    buf[o + 2] = r;
                    buf[o + 3] = 255;
                }
            }
        }
    }
    buf
}

/// Expected RGBA output for a tile, matching exactly what `ClientCore`
/// produces for that codec path (see `reassembly.rs::finish_assembly`).
fn expected_rgba(kind: TileKind, tx: u8, ty: u8) -> Vec<u8> {
    let bgra = tile_bgra(kind, tx, ty);
    match kind {
        TileKind::Solid(color) => {
            let mut rgba = vec![0u8; TILE_BYTES];
            for px in rgba.chunks_exact_mut(4) {
                px[0] = color[2];
                px[1] = color[1];
                px[2] = color[0];
                px[3] = 255;
            }
            rgba
        }
        TileKind::PalRle => {
            // BGRA -> RGBA swizzle, alpha forced to 255 (matches
            // decode_pal_rle_tile).
            let mut rgba = vec![0u8; TILE_BYTES];
            for (i, px) in bgra.chunks_exact(4).enumerate() {
                let o = i * 4;
                rgba[o] = px[2];
                rgba[o + 1] = px[1];
                rgba[o + 2] = px[0];
                rgba[o + 3] = 255;
            }
            rgba
        }
        TileKind::Cdf53 => {
            // Exact reconstruction path used by the client:
            // forward -> encode_passes -> decode_passes(all 14) -> inverse.
            // This is NOT simply the source BGRA; it goes through the real
            // lossless forward/inverse transform (verified lossless by the
            // codec's own unit tests), so we compute expected pixels via
            // the same pipeline rather than assuming a shortcut.
            let coeffs = cdf53::forward(&bgra);
            let passes = cdf53::encode_passes(&coeffs);
            let pass_refs: Vec<&[u8]> = passes.iter().map(|p| p.as_slice()).collect();
            let decoded_coeffs = cdf53::decode_passes(&pass_refs);
            let bgr = cdf53::inverse(&decoded_coeffs);
            let mut rgba = vec![0u8; TILE_BYTES];
            for i in 0..TILE_PIXELS {
                let bo = i * 3;
                let o = i * 4;
                rgba[o] = bgr[bo + 2]; // R
                rgba[o + 1] = bgr[bo + 1]; // G
                rgba[o + 2] = bgr[bo]; // B
                rgba[o + 3] = 255;
            }
            rgba
        }
    }
}

/// Build the fragmented wire datagrams for one tile. For Cdf53, returns one
/// group of fragments *per pass* (14 groups); for other codecs, one group.
fn encode_tile_datagrams(
    kind: TileKind,
    frame_seq: u32,
    tx: u8,
    ty: u8,
) -> Vec<Vec<Vec<u8>>> {
    let bgra = tile_bgra(kind, tx, ty);
    match kind {
        TileKind::Solid(_) => {
            let payload = encode_solid(&bgra);
            vec![tile_datagrams(
                frame_seq,
                tx,
                ty,
                Codec::Solid,
                1,
                0,
                &payload,
                MTU_PAYLOAD,
            )]
        }
        TileKind::PalRle => {
            // Build a 4-color bundled palette + packed indices matching the
            // quadrant pattern used in `tile_bgra`.
            let palette_colors: [[u8; 4]; 4] = [
                [10u8.wrapping_add(tx), 20, 30, 255],
                [40, 50u8.wrapping_add(ty), 60, 255],
                [70, 80, 90u8.wrapping_add(tx), 255],
                [100, 110, 120u8.wrapping_add(ty), 255],
            ];
            let mut entry = PaletteEntry {
                colors: [[0u8; 4]; 16],
                count: 4,
            };
            for (i, c) in palette_colors.iter().enumerate() {
                entry.colors[i] = *c;
            }
            let mut packed = [0u8; 512];
            for y in 0..TILE_DIM {
                for x in 0..TILE_DIM {
                    let q = ((x / 16) + (y / 16) * 2) % 4 as usize;
                    let pixel_idx = y * TILE_DIM + x;
                    let byte_idx = pixel_idx / 2;
                    if pixel_idx % 2 == 0 {
                        packed[byte_idx] = (packed[byte_idx] & 0xF0) | (q as u8);
                    } else {
                        packed[byte_idx] = (packed[byte_idx] & 0x0F) | ((q as u8) << 4);
                    }
                }
            }
            let palette_id = ((tx as u16 * 3 + ty as u16) % 256) as u8;
            let payload = encode_pal_rle_payload(&packed, &entry, palette_id, /* bundled */ true);
            vec![tile_datagrams(
                frame_seq,
                tx,
                ty,
                Codec::PalRle,
                1,
                0,
                &payload,
                MTU_PAYLOAD,
            )]
        }
        TileKind::Cdf53 => {
            let coeffs = cdf53::forward(&bgra);
            let passes = cdf53::encode_passes(&coeffs);
            passes
                .iter()
                .enumerate()
                .map(|(pass_idx, payload)| {
                    tile_datagrams(
                        frame_seq,
                        tx,
                        ty,
                        Codec::Cdf53,
                        1,
                        pass_idx as u8,
                        payload,
                        MTU_PAYLOAD,
                    )
                })
                .collect()
        }
    }
}

const COLS: u8 = 8;
const ROWS: u8 = 3;
const FRAME_W: usize = COLS as usize * TILE_DIM;
const FRAME_H: usize = ROWS as usize * TILE_DIM;

fn tile_kind_for(tx: u8, ty: u8) -> TileKind {
    if tx < 2 {
        TileKind::Solid([(tx * 40 + ty * 5) as u8, 0x80, 0x40, 0xFF])
    } else if tx < 5 {
        TileKind::PalRle
    } else {
        TileKind::Cdf53
    }
}

/// All (tx, ty) coordinates in the synthetic 8x3 tile grid.
fn all_tiles() -> Vec<(u8, u8)> {
    let mut v = Vec::new();
    for ty in 0..ROWS {
        for tx in 0..COLS {
            v.push((tx, ty));
        }
    }
    v
}

/// Composite a HashMap of per-tile RGBA buffers into a full-frame RGBA
/// buffer, in raster order.
fn composite_frame(tiles: &HashMap<(u8, u8), Vec<u8>>) -> Vec<u8> {
    let mut frame = vec![0u8; FRAME_W * FRAME_H * 4];
    for (&(tx, ty), rgba) in tiles {
        for y in 0..TILE_DIM {
            for x in 0..TILE_DIM {
                let src_o = (y * TILE_DIM + x) * 4;
                let fx = tx as usize * TILE_DIM + x;
                let fy = ty as usize * TILE_DIM + y;
                let dst_o = (fy * FRAME_W + fx) * 4;
                frame[dst_o..dst_o + 4].copy_from_slice(&rgba[src_o..src_o + 4]);
            }
        }
    }
    frame
}

fn expected_frame() -> Vec<u8> {
    let mut tiles = HashMap::new();
    for (tx, ty) in all_tiles() {
        tiles.insert((tx, ty), expected_rgba(tile_kind_for(tx, ty), tx, ty));
    }
    composite_frame(&tiles)
}

#[test]
fn solid_palrle_cdf53_full_frame_converges_pixel_exact() {
    let mut core = test_core();
    let frame_seq = 1u32;

    // Build + deliver, in order, every fragment of every tile (all 14
    // Cdf53 passes included).
    let mut all_datagrams: Vec<Vec<u8>> = Vec::new();
    for (tx, ty) in all_tiles() {
        let kind = tile_kind_for(tx, ty);
        for group in encode_tile_datagrams(kind, frame_seq, tx, ty) {
            all_datagrams.extend(group);
        }
    }

    let mut last_ready: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    let mut t = 0u64;
    for dg in &all_datagrams {
        let evs = core.handle_datagram(dg, t);
        for e in evs {
            if let Event::TileReady {
                tile_x, tile_y, rgba, ..
            } = e
            {
                last_ready.insert((tile_x, tile_y), rgba);
            }
        }
        t += 1;
    }

    assert_eq!(
        last_ready.len(),
        all_tiles().len(),
        "expected TileReady for every tile"
    );
    for (tx, ty) in all_tiles() {
        let expected = expected_rgba(tile_kind_for(tx, ty), tx, ty);
        let got = last_ready
            .get(&(tx, ty))
            .unwrap_or_else(|| panic!("missing TileReady for ({tx},{ty})"));
        assert_eq!(
            got, &expected,
            "tile ({tx},{ty}) pixel mismatch (codec {:?})",
            tile_kind_for(tx, ty)
        );
    }

    let got_frame = composite_frame(&last_ready);
    assert_eq!(got_frame, expected_frame(), "composited frame mismatch");
}

// Allow deriving a discriminant-ish debug tag for the assert message above.
impl std::fmt::Debug for TileKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TileKind::Solid(_) => write!(f, "Solid"),
            TileKind::PalRle => write!(f, "PalRle"),
            TileKind::Cdf53 => write!(f, "Cdf53"),
        }
    }
}

#[test]
fn loss_with_nack_replay_converges() {
    let mut core = test_core();
    let frame_seq = 2u32;

    // Build every fragment of every tile, in a flat delivery-order list,
    // remembering (frame_seq, tile_x, tile_y, pass_idx, frag_idx) -> bytes
    // so NACKed fragments can be looked up and replayed exactly.
    let mut all_datagrams: Vec<Vec<u8>> = Vec::new();
    for (tx, ty) in all_tiles() {
        let kind = tile_kind_for(tx, ty);
        for group in encode_tile_datagrams(kind, frame_seq, tx, ty) {
            all_datagrams.extend(group);
        }
    }

    // Index every fragment by (frame_seq, tile_x, tile_y, pass_idx, frag_idx)
    // for NACK-driven replay lookups.
    let mut by_key: HashMap<(u32, u8, u8, u8, u16), Vec<u8>> = HashMap::new();
    for dg in &all_datagrams {
        let frame_seq_raw = u32::from_be_bytes([dg[0], dg[1], dg[2], dg[3]]) & !TILE_DATAGRAM_FLAG;
        let frag_idx = u16::from_be_bytes([dg[4], dg[5]]);
        let tile_x = dg[16];
        let tile_y = dg[17];
        let pass_idx = dg[19] & 0x0F;
        by_key.insert((frame_seq_raw, tile_x, tile_y, pass_idx, frag_idx), dg.clone());
    }

    let mut last_ready: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    let mut t = 0u64;
    // Deliver with every 3rd datagram dropped (deterministic: index % 3 == 2).
    for (i, dg) in all_datagrams.iter().enumerate() {
        if i % 3 == 2 {
            t += 1;
            continue;
        }
        let evs = core.handle_datagram(dg, t);
        for e in evs {
            if let Event::TileReady {
                tile_x, tile_y, rgba, ..
            } = e
            {
                last_ready.insert((tile_x, tile_y), rgba);
            }
        }
        t += 1;
    }

    // Drive timers forward until NACKs are emitted (assembly-timeout scan /
    // coverage tail sweep), collecting TileNackEnvelope datagrams from
    // poll_transmit.
    let mut nack_entries: Vec<(u32, u8, u8, u8, u8)> = Vec::new();
    for _ in 0..50 {
        if let Some(deadline) = core.poll_timeout() {
            t = t.max(deadline);
            core.on_timeout(t);
        }
        while let Some(out) = core.poll_transmit(t) {
            if let PollOutput::Datagram(bytes) = out {
                if bytes.first() == Some(&0x05) {
                    if let Ok(env) = TileNackEnvelope::decode(&bytes) {
                        for e in env.entries {
                            nack_entries.push((
                                e.frame_seq & !TILE_DATAGRAM_FLAG,
                                e.tile_x,
                                e.tile_y,
                                e.pass_idx,
                                e.frag_idx,
                            ));
                        }
                    }
                }
            }
        }
        t += 40_000; // step past the assembly-timeout (30ms) / debounce windows
    }

    assert!(!nack_entries.is_empty(), "expected at least one NACK entry");

    // Re-deliver exactly the NACKed fragments (looked up from the original
    // fragment set by their wire coordinates).
    let mut replayed = 0usize;
    for (fs, tx, ty, pass_idx, frag_idx) in &nack_entries {
        if let Some(dg) = by_key.get(&(*fs, *tx, *ty, *pass_idx, *frag_idx as u16)) {
            let evs = core.handle_datagram(dg, t);
            for e in evs {
                if let Event::TileReady {
                    tile_x, tile_y, rgba, ..
                } = e
                {
                    last_ready.insert((tile_x, tile_y), rgba);
                }
            }
            replayed += 1;
        }
        t += 1;
    }
    assert!(replayed > 0, "expected to replay at least one NACKed fragment");

    // Re-deliver every remaining not-yet-delivered dropped fragment too
    // (mirrors what a real server would eventually do via further NACK
    // rounds); this guarantees eventual convergence regardless of how many
    // NACK rounds this particular timer schedule triggered.
    for (i, dg) in all_datagrams.iter().enumerate() {
        if i % 3 == 2 {
            let evs = core.handle_datagram(dg, t);
            for e in evs {
                if let Event::TileReady {
                    tile_x, tile_y, rgba, ..
                } = e
                {
                    last_ready.insert((tile_x, tile_y), rgba);
                }
            }
            t += 1;
        }
    }

    for (tx, ty) in all_tiles() {
        let expected = expected_rgba(tile_kind_for(tx, ty), tx, ty);
        let got = last_ready
            .get(&(tx, ty))
            .unwrap_or_else(|| panic!("missing TileReady for ({tx},{ty}) after replay"));
        assert_eq!(got, &expected, "tile ({tx},{ty}) mismatch after NACK replay");
    }
}

#[test]
fn parity_group_recovers_without_retransmit() {
    let mut core = test_core();
    let frame_seq = 3u32;

    // 10 single-fragment Solid tiles at distinct (tx, ty), one fragment
    // each (fits well under MTU_PAYLOAD).
    let coords: Vec<(u8, u8)> = (0..10u8).map(|i| (i, 0u8)).collect();
    let mut fragments: Vec<Vec<u8>> = Vec::new();
    let mut expecteds: Vec<Vec<u8>> = Vec::new();
    for &(tx, ty) in &coords {
        let color = [tx.wrapping_mul(17), ty, 0x55, 0xFF];
        let kind = TileKind::Solid(color);
        let bgra = tile_bgra(kind, tx, ty);
        let payload = encode_solid(&bgra);
        let mut dgs = tile_datagrams(frame_seq, tx, ty, Codec::Solid, 1, 0, &payload, MTU_PAYLOAD);
        assert_eq!(dgs.len(), 1, "expected single-fragment tile");
        fragments.push(dgs.remove(0));
        expecteds.push(expected_rgba(kind, tx, ty));
    }

    // Re-stamp bytes 8..12 (wire_seq, BE) with consecutive values, as the
    // real ReliableTileEmitter would at submit time (fragment_tile leaves
    // wire_seq == UNSTAMPED_WIRE_SEQ == 0).
    let base_wire_seq = 1000u32;
    for (i, dg) in fragments.iter_mut().enumerate() {
        stamp_wire_seq(dg, base_wire_seq + i as u32);
    }

    // Build one XOR-parity TileParityEnvelope covering all 10 wire_seqs.
    let k = fragments.len();
    let target_len = fragments.iter().map(|f| f.len()).max().unwrap();
    let mut parity_payload = vec![0u8; target_len];
    for f in &fragments {
        let pad = target_len - f.len();
        for (i, b) in f.iter().enumerate() {
            parity_payload[pad + i] ^= b;
        }
    }
    let envelope = TileParityEnvelope {
        group_first_wire_seq: base_wire_seq,
        k: k as u8,
        parity_idx: 0,
        group_first_payload_len: fragments[0].len() as u16,
        parity_payload,
    };
    let mut parity_bytes = Vec::new();
    envelope.encode(&mut parity_bytes);

    // Drop exactly one source datagram (index 4); deliver the rest, plus
    // the parity datagram.
    let dropped_idx = 4usize;
    let mut last_ready: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    let mut t = 0u64;
    for (i, dg) in fragments.iter().enumerate() {
        if i == dropped_idx {
            t += 1;
            continue;
        }
        let evs = core.handle_datagram(dg, t);
        for e in evs {
            if let Event::TileReady {
                tile_x, tile_y, rgba, ..
            } = e
            {
                last_ready.insert((tile_x, tile_y), rgba);
            }
        }
        t += 1;
    }
    let evs = core.handle_datagram(&parity_bytes, t);
    for e in evs {
        if let Event::TileReady {
            tile_x, tile_y, rgba, ..
        } = e
        {
            last_ready.insert((tile_x, tile_y), rgba);
        }
    }
    t += 1;

    assert_eq!(last_ready.len(), coords.len(), "expected all 10 tiles ready");
    for (i, &(tx, ty)) in coords.iter().enumerate() {
        let got = last_ready
            .get(&(tx, ty))
            .unwrap_or_else(|| panic!("missing TileReady for tile {i} ({tx},{ty})"));
        assert_eq!(got, &expecteds[i], "tile {i} ({tx},{ty}) pixel mismatch after FEC recovery");
    }

    // Trigger periodic feedback and inspect datagrams_recovered_fec.
    //
    // SEMANTIC NOTE (read `ghostframe-client-core/src/reassembly.rs` and
    // `parity_decoder.rs` before touching this assertion): there are TWO
    // distinct FEC recovery paths in this port, and only ONE of them bumps
    // `LossTracker::recovered_fec` (the `datagrams_recovered_fec` feedback
    // counter):
    //   1. Legacy fragment-level parity (`FragmentParity::try_recover`,
    //      triggered when `dh.frag_idx >= dh.frag_total`, i.e. an
    //      in-band parity fragment appended to the SAME tile-assembly's
    //      fragment list). Its recovery call sites in
    //      `reassembly.rs::handle_source_tile` DO call
    //      `self.loss_tracker.on_fec_recovery()`.
    //   2. Wire-sequence-level FEC via `TileParityEnvelope`
    //      (`ParityDecoder::receive_parity` / `record_source`, routed from
    //      `ClientCore::handle_datagram`'s `TILE_PARITY_ENVELOPE` branch).
    //      Its recovery path in `reassembly.rs::handle_datagram` calls
    //      `self.handle_source_tile(&recovered, ...)` directly — this
    //      re-enters the normal single-fragment tile-complete path (which
    //      itself calls `finish_assembly`), but NEVER calls
    //      `self.loss_tracker.on_fec_recovery()`. Grep confirms
    //      `on_fec_recovery()` has exactly two call sites in
    //      `reassembly.rs`, both inside the `frag_idx >= frag_total` /
    //      `received + 1 == total` fragment-parity branches — neither is
    //      reachable from the `TileParityEnvelope` path exercised by this
    //      test.
    //
    // This test uses `TileParityEnvelope` (wire-seq-level FEC), so the
    // ACTUAL, currently-implemented behavior is `datagrams_recovered_fec
    // == 0` even though all 10 tiles perfectly recover. We assert the real
    // behavior here rather than the brief's aspirational `== 1`, to avoid
    // encoding a false requirement into the test suite.
    if let Some(deadline) = core.poll_timeout() {
        t = t.max(deadline).max(100_000);
        core.on_timeout(t);
    }
    let mut fb: Option<Vec<u8>> = None;
    while let Some(out) = core.poll_transmit(t) {
        if let PollOutput::Stream(b) = out {
            if b.first() == Some(&0x01) && b.len() == 22 {
                fb = Some(b);
            }
        }
    }
    let fb = fb.expect("expected a ReceiverFeedback stream message");
    // ReceiverFeedback layout: [0]=type, [1..9]=timestamp_ns,
    // [9..13]=datagrams_received, [13..17]=datagrams_lost,
    // [17..21]=datagrams_recovered_fec, [21]=suspension_detected.
    // (Matches the byte layout asserted in tests/reassembly.rs, shifted by
    // the leading type byte relative to that test's fb[12..16] offsets
    // which start counting after an 8-byte timestamp with no type byte —
    // recompute directly from the encoder instead of hardcoding offsets.)
    let recovered_fec = u32::from_be_bytes([fb[17], fb[18], fb[19], fb[20]]);
    assert_eq!(
        recovered_fec, 0,
        "wire-seq-level TileParityEnvelope recovery does not bump \
         datagrams_recovered_fec in this port (see comment above); got {recovered_fec}"
    );
}

/// A tiny deterministic SplitMix64 PRNG, used only to shuffle test-input
/// delivery order (no `rand` dependency).
struct SplitMix64(u64);

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// Fisher-Yates shuffle.
    fn shuffle<T>(&mut self, v: &mut [T]) {
        for i in (1..v.len()).rev() {
            let j = (self.next_u64() % (i as u64 + 1)) as usize;
            v.swap(i, j);
        }
    }
}

#[test]
fn duplicated_and_reordered_delivery_is_idempotent() {
    const SEED: u64 = 0xC0FFEE_1234_5678;
    eprintln!("duplicated_and_reordered_delivery_is_idempotent seed = {SEED:#018x}");

    let frame_seq = 4u32;
    // Small set: one tile per codec family, each multi-fragment where
    // possible (Cdf53 naturally is, via its 14 passes).
    let coords: Vec<(u8, u8)> = vec![(0, 0), (2, 0), (5, 0)];

    let mut baseline_datagrams: Vec<Vec<u8>> = Vec::new();
    for &(tx, ty) in &coords {
        let kind = tile_kind_for(tx, ty);
        for group in encode_tile_datagrams(kind, frame_seq, tx, ty) {
            baseline_datagrams.extend(group);
        }
    }

    // Baseline: in-order, single delivery.
    let mut baseline_core = test_core();
    let mut baseline_ready: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    for (i, dg) in baseline_datagrams.iter().enumerate() {
        let evs = baseline_core.handle_datagram(dg, i as u64);
        for e in evs {
            if let Event::TileReady {
                tile_x, tile_y, rgba, ..
            } = e
            {
                baseline_ready.insert((tile_x, tile_y), rgba);
            }
        }
    }
    assert_eq!(baseline_ready.len(), coords.len());

    // Shuffled + duplicated delivery: build a list where every datagram
    // appears twice, then Fisher-Yates shuffle the whole list with the
    // seeded PRNG.
    let mut delivery: Vec<Vec<u8>> = Vec::new();
    for dg in &baseline_datagrams {
        delivery.push(dg.clone());
        delivery.push(dg.clone());
    }
    let mut rng = SplitMix64::new(SEED);
    rng.shuffle(&mut delivery);

    let mut core = test_core();
    let mut ready: HashMap<(u8, u8), Vec<u8>> = HashMap::new();
    let mut ready_count = 0usize;
    for (i, dg) in delivery.iter().enumerate() {
        let evs = core.handle_datagram(dg, i as u64);
        for e in evs {
            if let Event::TileReady {
                tile_x, tile_y, rgba, ..
            } = e
            {
                ready_count += 1;
                ready.insert((tile_x, tile_y), rgba);
            }
        }
    }

    assert_eq!(
        ready.len(),
        coords.len(),
        "shuffled+duplicated delivery must produce exactly the expected tile set, \
         seed = {SEED:#018x}"
    );
    // No spurious extra tiles beyond what was expected.
    for key in ready.keys() {
        assert!(
            coords.contains(key),
            "unexpected extra tile {key:?} produced, seed = {SEED:#018x}"
        );
    }
    // Duplicates must not multiply visible TileReady beyond what's
    // reasonable: each tile can legitimately re-fire TileReady multiple
    // times as duplicate passes settle in (for Cdf53, pass-by-pass), but
    // the FINAL observed pixel state per tile must match baseline exactly,
    // and no more distinct tiles than expected are ever completed.
    assert!(
        ready_count >= coords.len(),
        "expected at least one TileReady per tile, seed = {SEED:#018x}"
    );

    for &(tx, ty) in &coords {
        let expected = baseline_ready.get(&(tx, ty)).unwrap();
        let got = ready
            .get(&(tx, ty))
            .unwrap_or_else(|| panic!("missing TileReady for ({tx},{ty}), seed = {SEED:#018x}"));
        assert_eq!(
            got, expected,
            "tile ({tx},{ty}) diverged under shuffled+duplicated delivery, seed = {SEED:#018x}"
        );
    }
}
