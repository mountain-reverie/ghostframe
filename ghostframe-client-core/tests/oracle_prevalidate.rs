//! Port of `ghostframe-web-client/tests/prevalidate.test.ts` (11 cases),
//! plus one RGBA pixel test for `decode_pal_rle_tile`.

use ghostframe_client_core::pal_rle_decode::{
    decode_pal_rle_tile, prevalidate_pal_rle, PalRleVariant,
};
use ghostframe_client_core::palette_shadow::PaletteShadow;
use ghostframe_client_core::DecodeErrorCode;

fn bundled_payload(palette_id: u8, count: u8, rle_bytes: &[u8]) -> Vec<u8> {
    let mut palette = Vec::new();
    for _ in 0..count {
        palette.extend_from_slice(&[0xFF, 0x00, 0x00, 0xFF]); // bgra red
    }
    let mut out = vec![0x01u8, palette_id, count];
    out.extend(palette);
    out.extend_from_slice(rle_bytes);
    out
}

fn thin_payload(palette_id: u8, rle_bytes: &[u8]) -> Vec<u8> {
    let mut out = vec![0x00u8, palette_id];
    out.extend_from_slice(rle_bytes);
    out
}

fn indices_raw_payload(palette_id: u8, indices512: &[u8]) -> Vec<u8> {
    let mut out = vec![0x02u8, palette_id];
    out.extend_from_slice(indices512);
    out
}

// --- error paths ---

#[test]
fn reports_payload_too_short() {
    let shadow = PaletteShadow::new();
    let r = prevalidate_pal_rle(&[0x00], &shadow);
    assert!(matches!(r, Err(DecodeErrorCode::PayloadTooShort)));
}

#[test]
fn reports_bundled_count_0() {
    let shadow = PaletteShadow::new();
    let r = prevalidate_pal_rle(&bundled_payload(5, 0, &[0x00]), &shadow);
    assert!(matches!(r, Err(DecodeErrorCode::CountOutOfRange)));
}

#[test]
fn reports_bundled_count_17() {
    let shadow = PaletteShadow::new();
    let payload = vec![0x01u8, 5, 17];
    let r = prevalidate_pal_rle(&payload, &shadow);
    assert!(matches!(r, Err(DecodeErrorCode::CountOutOfRange)));
}

#[test]
fn reports_bundled_truncated_palette() {
    let shadow = PaletteShadow::new();
    // count=2 means 8 bytes of palette expected; supply only 4.
    let payload = vec![0x01u8, 5, 2, 0xFF, 0, 0, 0xFF];
    let r = prevalidate_pal_rle(&payload, &shadow);
    assert!(matches!(r, Err(DecodeErrorCode::BundledTruncated)));
}

#[test]
fn reports_thin_uncached_palette() {
    let shadow = PaletteShadow::new(); // shadow empty
    let r = prevalidate_pal_rle(&thin_payload(99, &[0x0F]), &shadow);
    assert!(matches!(r, Err(DecodeErrorCode::ThinUncachedPalette)));
}

#[test]
fn reports_indices_raw_too_short() {
    let mut shadow = PaletteShadow::new();
    shadow.put(5, 1);
    let payload = {
        let mut v = vec![0x02u8, 5];
        v.extend(std::iter::repeat_n(0u8, 500)); // 502 bytes, not 514
        v
    };
    let r = prevalidate_pal_rle(&payload, &shadow);
    assert!(matches!(r, Err(DecodeErrorCode::PayloadTooShort)));
}

// --- success paths ---

#[test]
fn bundled_with_1_color_tile_expands_all_same_indices() {
    let shadow = PaletteShadow::new();
    let rle = vec![0x0Fu8; 64]; // (0 << 4) | (16-1) = 0x0F repeated
    let v = prevalidate_pal_rle(&bundled_payload(5, 1, &rle), &shadow).unwrap();
    assert_eq!(v.variant, PalRleVariant::Bundled);
    assert_eq!(v.palette_id, 5);
    assert_eq!(v.count, 1);
    assert_eq!(v.indices.len(), 512);
    for i in 0..512 {
        assert_eq!(v.indices[i], 0);
    }
}

#[test]
fn thin_against_pre_populated_shadow() {
    let mut shadow = PaletteShadow::new();
    shadow.put(7, 4);
    let rle = vec![0x0Fu8; 64];
    let v = prevalidate_pal_rle(&thin_payload(7, &rle), &shadow).unwrap();
    assert_eq!(v.variant, PalRleVariant::Thin);
    assert_eq!(v.palette_id, 7);
    assert_eq!(v.count, 4);
}

#[test]
fn indices_raw_passes_512_bytes_verbatim() {
    let mut shadow = PaletteShadow::new();
    shadow.put(9, 8);
    let mut indices = vec![0u8; 512];
    for (i, slot) in indices.iter_mut().enumerate() {
        *slot = (i as u8) & 0x77;
    }
    let v = prevalidate_pal_rle(&indices_raw_payload(9, &indices), &shadow).unwrap();
    assert_eq!(v.variant, PalRleVariant::IndicesRaw);
    assert_eq!(v.palette_id, 9);
    assert_eq!(v.indices, indices);
}

#[test]
fn bundled_returns_palette_upsert_with_the_colors() {
    let shadow = PaletteShadow::new();
    let rle = vec![0x0Fu8; 64];
    let v = prevalidate_pal_rle(&bundled_payload(11, 2, &rle), &shadow).unwrap();
    let upsert = v.palette_upsert.unwrap();
    assert_eq!(upsert.len(), 8); // 2 x 4 bytes
    assert_eq!(upsert[0], 0xFF); // B
}

// --- expandRleToIndices: adversarial patterns ---

#[test]
fn alternating_index_sequence_packs_correctly() {
    let shadow = PaletteShadow::new();
    // index 0 run-1, index 1 run-1, index 0 run-1, ... 1024 times -> 1024 RLE bytes
    let mut rle = Vec::with_capacity(1024);
    for i in 0..1024u32 {
        let idx = (i & 1) as u8;
        rle.push(idx << 4);
    }
    let v = prevalidate_pal_rle(&bundled_payload(3, 2, &rle), &shadow).unwrap();
    for i in 0..512 {
        assert_eq!(v.indices[i], 0x10);
    }
}

// --- decode_pal_rle_tile RGBA pixel test ---

#[test]
fn decode_pal_rle_tile_bundled_two_color_pixel0_matches_palette_color0_swizzled() {
    let mut shadow = PaletteShadow::new();
    let mut palettes = [[[0u8; 4]; 16]; 256];

    // Bundled payload with 2 colors: color0 = BGRA(0x11,0x22,0x33,0x44), color1 = BGRA(0x55,0x66,0x77,0x88).
    let mut payload = vec![0x01u8, 5, 2];
    payload.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // color 0 BGRA
    payload.extend_from_slice(&[0x55, 0x66, 0x77, 0x88]); // color 1 BGRA
                                                          // All 1024 pixels index 0.
    payload.extend(std::iter::repeat_n(0x0Fu8, 64));

    let rgba = decode_pal_rle_tile(&payload, &mut shadow, &mut palettes).unwrap();
    assert_eq!(rgba.len(), 4096);
    // pixel0 should be RGBA swizzled from BGRA(0x11,0x22,0x33,0x44) -> R=0x33,G=0x22,B=0x11,A=255
    assert_eq!(&rgba[0..4], &[0x33, 0x22, 0x11, 0xFF]);

    // shadow + palettes table should now be populated.
    assert!(shadow.has(5));
    assert_eq!(shadow.count(5), 2);
    assert_eq!(palettes[5][0], [0x11, 0x22, 0x33, 0x44]);
}
