//! Tests for the `framebuffer` harness module: the client-side tile store
//! that Task 15's scene runner (`BrowserlessResult.framebuffer`) will use.
//!
//! Payloads are produced with `harness::scene_tiles::encode_tile` so these
//! tests exercise the same encoders the netsim scenes use, rather than
//! hand-rolled bytes that could quietly diverge from the real wire format.

use ghostframe_e2e::harness::framebuffer::FrameBuffer;
use ghostframe_e2e::harness::scene_tiles::{encode_tile, TileSpec};
use ghostframe_lib::transport::protocol::Codec;

/// Build a 32x32 BGRA tile filled with 4 distinct colors in 16x16 quadrants,
/// suitable for `TileSpec::PalRle` (well under the 16-color limit).
fn multi_colour_bgra() -> Vec<u8> {
    let colors = [
        [10u8, 20, 30, 255],
        [40, 50, 60, 255],
        [70, 80, 90, 255],
        [100, 110, 120, 255],
    ];
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32usize {
        for x in 0..32usize {
            let c = colors[(x / 16) + 2 * (y / 16)];
            bgra.extend_from_slice(&c);
        }
    }
    bgra
}

/// A gradient tile so CDF53 passes carry real, distinct bit-plane content
/// (a uniform tile can legitimately compress to near-identical passes).
fn gradient_bgra() -> Vec<u8> {
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = ((x * 8) % 256) as u8;
            let g = ((y * 8) % 256) as u8;
            let r = (((x + y) * 4) % 256) as u8;
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    bgra
}

#[test]
fn solid_swizzles_bgra_to_rgba_across_the_whole_tile() {
    let work = encode_tile(
        &TileSpec::Solid {
            bgra: [10, 20, 30, 255],
        },
        0,
        0,
        0,
    );
    assert_eq!(work.len(), 1);

    let mut fb = FrameBuffer::new();
    fb.apply(0, 0, 0, Codec::Solid, 0, &work[0].payload)
        .expect("valid solid payload");

    let rgba = fb.tile_rgba(0, 0).expect("tile stored");
    assert_eq!(rgba.len(), 4096);

    let expected = [30u8, 20, 10, 255]; // BGRA [10,20,30,255] -> RGBA
    assert_eq!(&rgba[0..4], &expected, "first pixel");
    assert_eq!(&rgba[512..516], &expected, "a middle pixel");
    assert_eq!(&rgba[4092..4096], &expected, "last pixel");
}

#[test]
fn pal_rle_round_trips_through_the_buffer() {
    let bgra = multi_colour_bgra();
    let work = encode_tile(
        &TileSpec::PalRle {
            bgra: bgra.clone(),
            palette_id: 3,
        },
        1,
        1,
        0,
    );
    assert_eq!(work.len(), 1);

    let mut fb = FrameBuffer::new();
    fb.apply(1, 1, 0, Codec::PalRle, 0, &work[0].payload)
        .expect("valid pal_rle payload");

    let rgba = fb.tile_rgba(1, 1).expect("tile stored");
    assert_eq!(rgba.len(), 4096);

    for pixel_idx in [0usize, 300, 600, 1023] {
        let src = pixel_idx * 4;
        let expected = [bgra[src + 2], bgra[src + 1], bgra[src], 255];
        let actual = &rgba[src..src + 4];
        assert_eq!(actual, expected, "pixel {pixel_idx} mismatch");
    }
}

#[test]
fn cdf53_accumulates_across_all_passes() {
    let bgra = gradient_bgra();
    let work = encode_tile(&TileSpec::Cdf53 { bgra }, 2, 2, 0);
    assert_eq!(work.len(), 14);

    let mut fb = FrameBuffer::new();

    fb.apply(2, 2, 0, Codec::Cdf53, work[0].pass_idx, &work[0].payload)
        .expect("pass 0 applies");
    let after_first = fb
        .tile_rgba(2, 2)
        .expect("tile stored after first pass")
        .to_vec();
    assert_eq!(after_first.len(), 4096);

    for w in &work[1..] {
        fb.apply(2, 2, 0, Codec::Cdf53, w.pass_idx, &w.payload)
            .expect("pass applies");
    }

    let after_last = fb.tile_rgba(2, 2).expect("tile stored after last pass");
    assert_eq!(after_last.len(), 4096);
    assert_ne!(
        after_first, after_last,
        "progressive refinement must change the reconstructed pixels \
         between the first and last pass"
    );
}

#[test]
fn stale_generation_fires_and_does_not_apply() {
    let work_gen0 = encode_tile(
        &TileSpec::Solid {
            bgra: [1, 1, 1, 255],
        },
        0,
        0,
        0,
    );
    let work_gen1 = encode_tile(
        &TileSpec::Solid {
            bgra: [2, 2, 2, 255],
        },
        0,
        0,
        1,
    );
    let work_gen0_again = encode_tile(
        &TileSpec::Solid {
            bgra: [3, 3, 3, 255],
        },
        0,
        0,
        0,
    );

    let mut fb = FrameBuffer::new();
    fb.apply(0, 0, 0, Codec::Solid, 0, &work_gen0[0].payload)
        .expect("gen 0 applies");
    fb.apply(0, 0, 1, Codec::Solid, 0, &work_gen1[0].payload)
        .expect("gen 1 applies");
    assert_eq!(fb.stale_generation_tiles(), 0);

    fb.apply(0, 0, 0, Codec::Solid, 0, &work_gen0_again[0].payload)
        .expect("stale apply returns Ok but is a no-op");
    assert_eq!(fb.stale_generation_tiles(), 1);

    // The stale gen-0 payload must have been dropped, not applied: the
    // tile should still hold gen 1's contents ([2,2,2,255] BGRA ->
    // [2,2,2,255] RGBA, since B==G==R here).
    let rgba = fb.tile_rgba(0, 0).expect("tile stored");
    assert_eq!(&rgba[0..4], &[2, 2, 2, 255]);
}

#[test]
fn normal_generation_advance_is_not_stale() {
    let mut fb = FrameBuffer::new();
    for gen in 0u8..=2 {
        let work = encode_tile(
            &TileSpec::Solid {
                bgra: [gen, gen, gen, 255],
            },
            0,
            0,
            gen,
        );
        fb.apply(0, 0, gen, Codec::Solid, 0, &work[0].payload)
            .expect("normal advance applies");
    }
    assert_eq!(fb.stale_generation_tiles(), 0);
}

#[test]
fn unknown_tile_returns_none() {
    let fb = FrameBuffer::new();
    assert!(fb.tile_rgba(5, 5).is_none());
}

#[test]
fn out_of_scope_codecs_are_accepted_but_not_stored() {
    let mut fb = FrameBuffer::new();
    fb.apply(0, 0, 0, Codec::H264, 0, &[])
        .expect("H264 is out of scope but must not error");
    assert!(
        fb.tile_rgba(0, 0).is_none(),
        "out-of-scope codec must not create a tile entry"
    );
}

/// A stale payload must be dropped **before** it is decoded.
///
/// `decode_pal_rle_tile` writes any bundled palette upsert into the buffer's
/// palette table as a side effect. If staleness were decided after decoding,
/// a late datagram from a superseded generation would overwrite the live
/// palette for its slot while its pixels were discarded — corrupting every
/// later tile that references that slot without bundling its own copy.
/// netsim reorders and duplicates datagrams by design, so this is traffic
/// the harness will actually produce.
///
/// The other tests here cannot catch it: they only ever observe the tile the
/// stale payload targeted, and that tile is correct either way. The damage
/// shows up on a *different* tile, later.
#[test]
fn a_stale_payload_does_not_corrupt_the_palette_table() {
    use ghostframe_protocol::codec::pal_rle::{encode_pal_rle_payload, PaletteEntry};

    const SLOT: u8 = 5;
    let colour_a = [10u8, 20, 30, 255];
    let colour_b = [90u8, 80, 70, 255];

    // All indices 0, so a tile is uniformly palette entry 0 — whichever
    // colour currently occupies that slot.
    let packed = [0u8; 512];

    let entry_for = |c: [u8; 4]| {
        let mut colors = [[0u8; 4]; 16];
        colors[0] = c;
        PaletteEntry { colors, count: 1 }
    };

    let bundled_a = encode_pal_rle_payload(&packed, &entry_for(colour_a), SLOT, true);
    let bundled_b = encode_pal_rle_payload(&packed, &entry_for(colour_b), SLOT, true);
    // Non-bundled: carries no palette, so it resolves against whatever the
    // buffer currently holds in SLOT. This is the probe.
    let unbundled = encode_pal_rle_payload(&packed, &entry_for(colour_a), SLOT, false);

    let mut fb = FrameBuffer::new();
    fb.apply(0, 0, 0, Codec::PalRle, 0, &bundled_a)
        .expect("gen 0");
    fb.apply(0, 0, 1, Codec::PalRle, 0, &bundled_b)
        .expect("gen 1");

    // Late datagram from the superseded generation 0.
    fb.apply(0, 0, 0, Codec::PalRle, 0, &bundled_a)
        .expect("stale gen 0");
    assert_eq!(
        fb.stale_generation_tiles(),
        1,
        "the late payload must count as stale"
    );

    // A different tile, current generation, resolving against SLOT.
    fb.apply(1, 0, 1, Codec::PalRle, 0, &unbundled)
        .expect("probe tile");

    let probe = fb.tile_rgba(1, 0).expect("probe tile stored");
    let expected_rgba = [colour_b[2], colour_b[1], colour_b[0], 255];
    assert_eq!(
        &probe[0..4],
        &expected_rgba,
        "SLOT must still hold generation 1's palette; if it holds the stale \
         generation 0 palette, the stale payload was decoded before being dropped"
    );
}

/// `apply_tile_ready` staleness is on `frame_seq`, a monotonically
/// increasing `u32` — unlike `apply`'s 4-bit `generation`, a plain `<`
/// comparison is correct here. A scene run cannot force reordering
/// deterministically, so this is exercised directly against the unit under
/// test instead of through `run_browserless`.
#[test]
fn apply_tile_ready_drops_a_strictly_older_frame_seq_and_keeps_the_newer_pixels() {
    let mut fb = FrameBuffer::new();

    let frame5 = vec![5u8; 4096];
    let frame3 = vec![3u8; 4096];
    let frame6 = vec![6u8; 4096];

    fb.apply_tile_ready(5, 0, 0, frame5.clone());
    assert_eq!(fb.stale_frame_tiles(), 0);
    assert_eq!(fb.tile_rgba(0, 0), Some(frame5.as_slice()));

    // Late/reordered arrival: frame_seq 3 < highest seen (5) — must be
    // dropped, not overwrite the stored pixels.
    fb.apply_tile_ready(3, 0, 0, frame3);
    assert_eq!(
        fb.stale_frame_tiles(),
        1,
        "frame_seq 3 arriving after frame_seq 5 must count as stale"
    );
    assert_eq!(
        fb.tile_rgba(0, 0),
        Some(frame5.as_slice()),
        "the stale frame_seq 3 payload must not have overwritten frame 5's pixels"
    );

    // A genuinely newer frame_seq must still apply, and must NOT be
    // counted as stale — this is what proves the counter isn't just
    // "always stale".
    fb.apply_tile_ready(6, 0, 0, frame6.clone());
    assert_eq!(
        fb.stale_frame_tiles(),
        1,
        "frame_seq 6 is newer than the stored frame_seq 5 and must not count as stale"
    );
    assert_eq!(fb.tile_rgba(0, 0), Some(frame6.as_slice()));
}
