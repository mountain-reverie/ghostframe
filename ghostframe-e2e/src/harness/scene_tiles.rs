//! Declarative tile-spec encoders for netsim scene construction.
//!
//! Turns a small, hand-authored `TileSpec` into the exact `TileWork` items
//! the scheduler consumes (`ghostframe_lib::transport::scheduler::TileWork`),
//! so later netsim harness tasks can build test scenes without hand-crafting
//! wire payloads.

use ghostframe_lib::transport::io_bridge::now_std;
use ghostframe_lib::transport::protocol::Codec;
use ghostframe_lib::transport::scheduler::{TileWork, WorkState};

use ghostframe_protocol::codec::cdf53;
use ghostframe_protocol::codec::pal_rle::{
    encode_pal_rle_payload, PaletteEntry, MAX_PALETTE_COUNT,
};
use ghostframe_protocol::codec::solid::encode_solid;

/// One tile's declared content, in whichever encoding a netsim scene
/// author chooses for it.
#[derive(Debug, Clone)]
pub enum TileSpec {
    /// Uniform-color tile — encodes to a single 4-byte Solid payload.
    /// `encode_solid` only samples the first pixel, so no full 32x32
    /// buffer is needed here.
    Solid { bgra: [u8; 4] },
    /// Arbitrary-content tile — encodes to 14 progressive Cdf53 passes.
    /// `bgra` must be exactly 32*32*4 = 4096 bytes.
    Cdf53 { bgra: Vec<u8> },
    /// Palette tile — encodes to a single bundled PalRle payload (the
    /// palette travels inline, so no server-side palette-table state is
    /// needed for injected tiles). `bgra` must be exactly 32*32*4 = 4096
    /// bytes and carry at most `MAX_PALETTE_COUNT` (16) distinct colors.
    PalRle { bgra: Vec<u8>, palette_id: u8 },
}

/// Turn a `TileSpec` into the `TileWork` item(s) the scheduler consumes.
///
/// `queued_at` is stamped from `now_std()` (tokio's clock), never
/// `std::time::Instant::now()` — later netsim tasks run under
/// `#[tokio::test(start_paused = true)]`, and a wall-clock `Instant`
/// compared against a virtual one silently saturates to zero.
pub fn encode_tile(spec: &TileSpec, tile_x: u8, tile_y: u8, generation: u8) -> Vec<TileWork> {
    match spec {
        TileSpec::Solid { bgra } => {
            let payload = encode_solid(bgra).to_vec();
            vec![work_item(
                tile_x,
                tile_y,
                generation,
                0,
                1,
                Codec::Solid,
                payload,
            )]
        }
        TileSpec::Cdf53 { bgra } => {
            let coefficients = cdf53::forward(bgra);
            let passes = cdf53::encode_passes(&coefficients);
            let total_passes = passes.len() as u8;
            passes
                .into_iter()
                .enumerate()
                .map(|(pass_idx, payload)| {
                    work_item(
                        tile_x,
                        tile_y,
                        generation,
                        pass_idx as u8,
                        total_passes,
                        Codec::Cdf53,
                        payload,
                    )
                })
                .collect()
        }
        TileSpec::PalRle { bgra, palette_id } => {
            let (palette, packed_indices) = build_palette(bgra, tile_x, tile_y);
            let payload = encode_pal_rle_payload(&packed_indices, &palette, *palette_id, true);
            vec![work_item(
                tile_x,
                tile_y,
                generation,
                0,
                1,
                Codec::PalRle,
                payload,
            )]
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn work_item(
    tile_x: u8,
    tile_y: u8,
    generation: u8,
    pass_idx: u8,
    total_passes: u8,
    codec: Codec,
    payload: Vec<u8>,
) -> TileWork {
    TileWork {
        tile_x,
        tile_y,
        generation,
        pass_idx,
        total_passes,
        codec,
        payload,
        queued_at: now_std(),
        last_sent_at: None,
        state: WorkState::Pending,
    }
}

/// Build a bundled-PalRle `PaletteEntry` and its 512-byte nibble-packed
/// index buffer from a 32x32 BGRA tile. Colors are assigned palette
/// indices in first-appearance order. Panics if the tile carries more
/// than `MAX_PALETTE_COUNT` distinct colors — scenes must be authored
/// within the palette limit.
fn build_palette(bgra: &[u8], tile_x: u8, tile_y: u8) -> (PaletteEntry, [u8; 512]) {
    assert_eq!(
        bgra.len(),
        32 * 32 * 4,
        "PalRle tile ({tile_x}, {tile_y}) must be exactly 32x32 BGRA (4096 bytes)"
    );

    let mut colors: Vec<[u8; 4]> = Vec::new();
    let mut packed_indices = [0u8; 512];

    for (pixel_idx, chunk) in bgra.chunks_exact(4).enumerate() {
        let color: [u8; 4] = chunk.try_into().expect("chunk is 4 bytes");
        let idx = match colors.iter().position(|&c| c == color) {
            Some(existing) => existing,
            None => {
                colors.push(color);
                if colors.len() > MAX_PALETTE_COUNT {
                    panic!(
                        "PalRle tile ({tile_x}, {tile_y}) has {} distinct colors, \
                         exceeding the {MAX_PALETTE_COUNT}-color palette limit; \
                         scenes must be authored within the palette limit",
                        colors.len()
                    );
                }
                colors.len() - 1
            }
        };
        let byte_idx = pixel_idx >> 1;
        if pixel_idx & 1 == 0 {
            packed_indices[byte_idx] |= (idx as u8) & 0x0F;
        } else {
            packed_indices[byte_idx] |= ((idx as u8) & 0x0F) << 4;
        }
    }

    let mut palette = PaletteEntry {
        count: colors.len() as u8,
        ..PaletteEntry::default()
    };
    for (i, color) in colors.into_iter().enumerate() {
        palette.colors[i] = color;
    }

    (palette, packed_indices)
}
