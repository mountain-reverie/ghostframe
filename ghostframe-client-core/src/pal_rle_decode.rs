//! Validate + materialize a PalRLE payload into a 512 B indices buffer, and
//! a full pixel decode into a 4096-byte RGBA tile buffer.
//!
//! Port of `ghostframe-web-client/src/prevalidate.ts`. Error-code
//! granularity intentionally differs from
//! `ghostframe_protocol::codec::pal_rle::PalRleDecodeError`; this is a
//! fresh, independent port and must not be routed through that codec's
//! `decode_pal_rle`.

use crate::event::DecodeErrorCode;
use crate::palette_shadow::PaletteShadow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PalRleVariant {
    Bundled,
    Thin,
    IndicesRaw,
}

#[derive(Debug)]
pub struct PrevalidatedPalRle {
    pub variant: PalRleVariant,
    pub palette_id: u8,
    pub count: u8,
    /// 512 bytes, 2 pixels/byte, low nibble first.
    pub indices: Vec<u8>,
    /// `count * 4` BGRA bytes for Bundled; `None` otherwise.
    pub palette_upsert: Option<Vec<u8>>,
}

/// Port of `prevalidatePalRle` (prevalidate.ts). Validates + expands;
/// updates nothing (neither `shadow` nor any palette table).
pub fn prevalidate_pal_rle(
    payload: &[u8],
    shadow: &PaletteShadow,
) -> Result<PrevalidatedPalRle, DecodeErrorCode> {
    if payload.len() < 2 {
        return Err(DecodeErrorCode::PayloadTooShort);
    }
    let flags = payload[0];
    let palette_id = payload[1];
    let bundled = (flags & 0x01) != 0;
    let indices_raw = (flags & 0x02) != 0;

    if indices_raw {
        // [0x02][palette_id][512 B indices]
        if payload.len() < 2 + 512 {
            return Err(DecodeErrorCode::PayloadTooShort);
        }
        if !shadow.has(palette_id) {
            return Err(DecodeErrorCode::ThinUncachedPalette);
        }
        let indices = payload[2..2 + 512].to_vec();
        return Ok(PrevalidatedPalRle {
            variant: PalRleVariant::IndicesRaw,
            palette_id,
            count: shadow.count(palette_id),
            indices,
            palette_upsert: None,
        });
    }

    let mut cursor = 2usize;
    let count: u8;
    let mut palette_upsert: Option<Vec<u8>> = None;

    if bundled {
        if cursor >= payload.len() {
            return Err(DecodeErrorCode::BundledTruncated);
        }
        count = payload[cursor];
        cursor += 1;
        if !(1..=16).contains(&count) {
            return Err(DecodeErrorCode::CountOutOfRange);
        }
        let palette_bytes = count as usize * 4;
        if cursor + palette_bytes > payload.len() {
            return Err(DecodeErrorCode::BundledTruncated);
        }
        palette_upsert = Some(payload[cursor..cursor + palette_bytes].to_vec());
        cursor += palette_bytes;
    } else {
        if !shadow.has(palette_id) {
            return Err(DecodeErrorCode::ThinUncachedPalette);
        }
        count = shadow.count(palette_id);
    }

    let indices =
        expand_rle_to_indices(&payload[cursor..]).ok_or(DecodeErrorCode::PayloadTooShort)?;

    Ok(PrevalidatedPalRle {
        variant: if bundled {
            PalRleVariant::Bundled
        } else {
            PalRleVariant::Thin
        },
        palette_id,
        count,
        indices,
        palette_upsert,
    })
}

/// Expand nibble-packed RLE bytes `(idx << 4) | (run_len - 1)` into 512
/// bytes of 4-bit-packed indices (low nibble of byte 0 = pixel 0, high
/// nibble of byte 0 = pixel 1, etc.). Returns `None` if expansion doesn't
/// yield exactly 1024 pixels.
fn expand_rle_to_indices(rle_bytes: &[u8]) -> Option<Vec<u8>> {
    let mut indices = vec![0u8; 512];
    let mut pixel_idx: usize = 0;
    for &b in rle_bytes {
        let idx = (b >> 4) & 0x0F;
        let run_len = (b & 0x0F) + 1;
        for _ in 0..run_len {
            if pixel_idx >= 1024 {
                return None;
            }
            let byte_idx = pixel_idx >> 1;
            if pixel_idx & 1 == 0 {
                indices[byte_idx] = (indices[byte_idx] & 0xF0) | idx;
            } else {
                indices[byte_idx] = (indices[byte_idx] & 0x0F) | (idx << 4);
            }
            pixel_idx += 1;
        }
    }
    if pixel_idx != 1024 {
        return None;
    }
    Some(indices)
}

/// Full pixel decode: prevalidate, apply any bundled `palette_upsert` to
/// `palettes` and `shadow`, then expand validated indices + palette table
/// into a 4096-byte RGBA buffer (1024 pixels x 4 bytes). The palette table
/// stores BGRA; this swizzles BGRA -> RGBA when writing output, forcing the
/// alpha byte to 255.
pub fn decode_pal_rle_tile(
    payload: &[u8],
    shadow: &mut PaletteShadow,
    palettes: &mut [[[u8; 4]; 16]; 256],
) -> Result<Vec<u8>, DecodeErrorCode> {
    let validated = prevalidate_pal_rle(payload, shadow)?;

    if let Some(upsert) = &validated.palette_upsert {
        let slot = &mut palettes[validated.palette_id as usize];
        for i in 0..validated.count as usize {
            let b = upsert[i * 4];
            let g = upsert[i * 4 + 1];
            let r = upsert[i * 4 + 2];
            let a = upsert[i * 4 + 3];
            slot[i] = [b, g, r, a];
        }
        shadow.put(validated.palette_id, validated.count);
    }

    let palette_slot = &palettes[validated.palette_id as usize];
    let mut rgba = vec![0u8; 4096];
    for pixel_idx in 0..1024usize {
        let byte_idx = pixel_idx >> 1;
        let idx = if pixel_idx & 1 == 0 {
            validated.indices[byte_idx] & 0x0F
        } else {
            (validated.indices[byte_idx] >> 4) & 0x0F
        };
        let bgra = palette_slot[idx as usize];
        let out = pixel_idx * 4;
        rgba[out] = bgra[2]; // R
        rgba[out + 1] = bgra[1]; // G
        rgba[out + 2] = bgra[0]; // B
        rgba[out + 3] = 255; // A forced
    }

    Ok(rgba)
}
