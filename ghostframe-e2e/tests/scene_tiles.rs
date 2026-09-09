//! Tests for the scene_tiles harness module: TileSpec -> TileWork encoders.

use ghostframe_e2e::harness::scene_tiles::{encode_tile, TileSpec};
use ghostframe_lib::transport::protocol::Codec;

fn solid_bgra(b: u8, g: u8, r: u8) -> Vec<u8> {
    let mut px = Vec::with_capacity(32 * 32 * 4);
    for _ in 0..(32 * 32) {
        px.extend_from_slice(&[b, g, r, 255]);
    }
    px
}

#[test]
fn solid_encodes_to_one_work_item_of_four_bytes() {
    let work = encode_tile(
        &TileSpec::Solid {
            bgra: [10, 20, 30, 255],
        },
        3,
        4,
        0,
    );
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].codec, Codec::Solid);
    assert_eq!(work[0].payload, vec![10, 20, 30, 255]);
    assert_eq!((work[0].tile_x, work[0].tile_y), (3, 4));
}

#[test]
fn cdf53_encodes_to_fourteen_passes() {
    let work = encode_tile(
        &TileSpec::Cdf53 {
            bgra: solid_bgra(10, 20, 30),
        },
        0,
        0,
        0,
    );
    assert_eq!(work.len(), 14, "CDF53 emits 14 progressive passes");
    assert_eq!(work[0].total_passes, 14);
    for (i, w) in work.iter().enumerate() {
        assert_eq!(w.pass_idx as usize, i);
        assert_eq!(w.codec, Codec::Cdf53);
    }
}

/// A perfectly uniform tile can legitimately compress to near-identical
/// (or even byte-identical, for high bit-planes that are all-zero) pass
/// payloads, so it would not catch an encoder that emits 14 copies of an
/// empty/placeholder payload. Use a non-uniform gradient tile instead so
/// each pass plausibly carries distinct bit-plane content, and assert that
/// (a) every payload is non-empty and (b) not all 14 payloads collapse to
/// the same bytes.
#[test]
fn cdf53_passes_carry_real_distinct_data() {
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = ((x * 8) % 256) as u8;
            let g = ((y * 8) % 256) as u8;
            let r = (((x + y) * 4) % 256) as u8;
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    let work = encode_tile(&TileSpec::Cdf53 { bgra }, 1, 2, 0);
    assert_eq!(work.len(), 14);
    for w in &work {
        assert!(
            !w.payload.is_empty(),
            "every pass payload must be non-empty"
        );
    }
    let all_identical = work.iter().all(|w| w.payload == work[0].payload);
    assert!(
        !all_identical,
        "a gradient tile's 14 passes must not all collapse to the same bytes"
    );
}

/// The plan never exercises the PalRle path at all; without this test the
/// path would ship untested and look complete.
#[test]
fn pal_rle_encodes_to_one_bundled_work_item() {
    // 4 distinct colors, laid out in a simple 2x2 block pattern repeated
    // across the tile.
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
    let work = encode_tile(
        &TileSpec::PalRle {
            bgra,
            palette_id: 7,
        },
        5,
        6,
        0,
    );
    assert_eq!(work.len(), 1);
    assert_eq!(work[0].codec, Codec::PalRle);
    assert_eq!(work[0].payload[0], 0x01, "bundled flag must be set");
    assert_eq!(work[0].payload[1], 7, "palette_id must round-trip");
}

/// An encoder that produces plausible-but-malformed payloads would pass
/// every test above and fail mysteriously in every later scene test.
/// Decode what we encoded and compare against the input.
#[test]
fn round_trip_solid_and_pal_rle() {
    // Solid round-trip via the protocol decoder.
    let work = encode_tile(
        &TileSpec::Solid {
            bgra: [1, 2, 3, 255],
        },
        0,
        0,
        0,
    );
    let decoded = ghostframe_protocol::codec::solid::decode_solid(&work[0].payload)
        .expect("valid solid payload");
    assert_eq!(decoded, [1, 2, 3, 255]);

    // PalRle round-trip via the client-core decoder.
    let colors = [[10u8, 20, 30, 255], [40, 50, 60, 255], [70, 80, 90, 255]];
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32usize {
        for x in 0..32usize {
            let c = colors[(x + y) % 3];
            bgra.extend_from_slice(&c);
        }
    }
    let work = encode_tile(
        &TileSpec::PalRle {
            bgra: bgra.clone(),
            palette_id: 9,
        },
        2,
        3,
        0,
    );
    let mut shadow = ghostframe_client_core::palette_shadow::PaletteShadow::new();
    let mut palettes = [[[0u8; 4]; 16]; 256];
    let decoded_rgba = ghostframe_client_core::pal_rle_decode::decode_pal_rle_tile(
        &work[0].payload,
        &mut shadow,
        &mut palettes,
    )
    .expect("valid pal_rle payload");

    // decode_pal_rle_tile returns RGBA (swapped from our BGRA input), with
    // alpha forced to 255.
    for pixel_idx in 0..1024usize {
        let src = pixel_idx * 4;
        let expected = [
            bgra[src + 2], // R
            bgra[src + 1], // G
            bgra[src],     // B
            255,           // A forced
        ];
        let actual = &decoded_rgba[src..src + 4];
        assert_eq!(actual, expected, "pixel {pixel_idx} mismatch");
    }
}

/// The plan requires that tiles with more than 16 distinct colours panic
/// at encode time, naming the offending count and tile coordinate, rather
/// than a scene silently authoring a malformed palette.
#[test]
#[should_panic(expected = "17 distinct colors")]
fn pal_rle_panics_over_the_palette_limit() {
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    // 17 distinct colors, one per pixel for the first 17 pixels, the rest
    // filler reusing color 0.
    for i in 0..(32 * 32) {
        if i < 17 {
            bgra.extend_from_slice(&[i as u8, i as u8, i as u8, 255]);
        } else {
            bgra.extend_from_slice(&[0, 0, 0, 255]);
        }
    }
    let _ = encode_tile(
        &TileSpec::PalRle {
            bgra,
            palette_id: 0,
        },
        8,
        9,
        0,
    );
}

/// Guards the `now_std()` stamping in `encode_tile`.
///
/// The plan for this task said to fill `queued_at` with `Instant::now()`.
/// That reads the wall clock, while every later netsim task runs under
/// `#[tokio::test(start_paused = true)]` on tokio's virtual clock. The two
/// do not merely disagree — `Instant::duration_since` *saturates to zero*
/// rather than erroring, so a wall-clock `queued_at` produces silently
/// wrong retry and staleness timing that no assertion downstream would
/// catch.
///
/// Advancing the virtual clock an hour separates the two: `now_std()`
/// follows it, `Instant::now()` does not.
#[tokio::test(start_paused = true)]
async fn queued_at_is_stamped_from_the_virtual_clock() {
    use ghostframe_lib::transport::io_bridge::now_std;
    use std::time::Duration;

    tokio::time::advance(Duration::from_secs(3600)).await;

    let work = encode_tile(
        &TileSpec::Solid {
            bgra: [1, 2, 3, 255],
        },
        0,
        0,
        0,
    );
    let skew = now_std().duration_since(work[0].queued_at);
    assert!(
        skew < Duration::from_millis(10),
        "queued_at must be stamped from the virtual clock, but it lags \
         now_std() by {skew:?} — it was almost certainly taken from \
         std::time::Instant::now()"
    );
}
