//! Wire-accurate test fixtures for [`crate::renderer::Renderer`], built with
//! the real protocol encoders.
//!
//! Mirrors `ghostframe-client-core/tests/loopback.rs`'s `tile_bgra` /
//! `encode_tile_datagrams` closely -- both exist to prove the same thing:
//! that a set of datagrams built from the real, shipped encoders decodes
//! correctly, wherever they're consumed. Hand-assembling bytes here would
//! prove nothing about the wire format the two decoders actually have to
//! agree on.

use ghostframe_protocol::codec::cdf53;
use ghostframe_protocol::codec::pal_rle::{encode_pal_rle_payload, PaletteEntry};
use ghostframe_protocol::codec::solid::encode_solid;
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

const TILE_DIM: usize = 32;
const TILE_PIXELS: usize = TILE_DIM * TILE_DIM;
const TILE_BYTES: usize = TILE_PIXELS * 4;
const MTU_PAYLOAD: usize = 900;

#[allow(clippy::too_many_arguments)]
fn tile_datagrams(
    frame_seq: u32,
    x: u8,
    y: u8,
    codec: Codec,
    generation: u8,
    pass: u8,
    payload: &[u8],
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
        MTU_PAYLOAD,
    )
}

/// BGRA pixel buffer for a solid-colour 32x32 tile.
fn solid_bgra(bgra: [u8; 4]) -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
    for px in buf.chunks_exact_mut(4) {
        px.copy_from_slice(&bgra);
    }
    buf
}

/// BGRA pixel buffer for a smooth gradient, keyed by (tx, ty) so distinct
/// tiles get distinct content -- exercises the CDF 5/3 path with real
/// variation rather than a flat field.
fn cdf53_bgra(tx: u8, ty: u8) -> Vec<u8> {
    let mut buf = vec![0u8; TILE_BYTES];
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
    buf
}

/// Wire datagrams for a small mixed-codec capture: one Solid tile, one
/// PalRle tile with a bundled palette, and one Cdf53 tile delivered as
/// multiple sparse passes, placed at distinct coordinates in a 64x64 (2x2
/// tile) framebuffer.
///
/// Built with the real protocol encoders (`encode_solid`,
/// `encode_pal_rle_payload`, `cdf53::forward`/`encode_passes`) and
/// fragmented with the real `fragment_tile`, mirroring
/// `ghostframe-client-core/tests/loopback.rs`'s `encode_tile_datagrams`.
/// Hand-assembled bytes would prove nothing about the wire format.
pub fn mixed_codec_capture() -> Vec<Vec<u8>> {
    let frame_seq = 1u32;
    let mut datagrams: Vec<Vec<u8>> = Vec::new();

    // Tile (0, 0): Solid, BGRA fill.
    {
        let bgra = solid_bgra([0x11, 0x22, 0x33, 0xFF]);
        let payload = encode_solid(&bgra);
        datagrams.extend(tile_datagrams(
            frame_seq,
            0,
            0,
            Codec::Solid,
            1,
            0,
            &payload,
        ));
    }

    // Tile (1, 0): PalRle, 4-colour bundled palette.
    {
        let tx = 1u8;
        let ty = 0u8;
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
                let q = ((x / 16) + (y / 16) * 2) % 4_usize;
                let pixel_idx = y * TILE_DIM + x;
                let byte_idx = pixel_idx / 2;
                if pixel_idx.is_multiple_of(2) {
                    packed[byte_idx] = (packed[byte_idx] & 0xF0) | (q as u8);
                } else {
                    packed[byte_idx] = (packed[byte_idx] & 0x0F) | ((q as u8) << 4);
                }
            }
        }
        let palette_id = ((tx as u16 * 3 + ty as u16) % 256) as u8;
        let payload = encode_pal_rle_payload(&packed, &entry, palette_id, /* bundled */ true);
        datagrams.extend(tile_datagrams(
            frame_seq,
            tx,
            ty,
            Codec::PalRle,
            1,
            0,
            &payload,
        ));
    }

    // Tile (0, 1): Cdf53, delivered as multiple sparse passes.
    {
        let tx = 0u8;
        let ty = 1u8;
        let bgra = cdf53_bgra(tx, ty);
        let coeffs = cdf53::forward(&bgra);
        let passes = cdf53::encode_passes(&coeffs);
        for (pass_idx, payload) in passes.iter().enumerate() {
            datagrams.extend(tile_datagrams(
                frame_seq,
                tx,
                ty,
                Codec::Cdf53,
                1,
                pass_idx as u8,
                payload,
            ));
        }
    }

    datagrams
}
