//! Client-side tile store for netsim scene runs.
//!
//! `FrameBuffer` accepts decoded tile payloads by codec and keeps the
//! resulting 4096-byte RGBA buffer per tile coordinate, so a scene runner
//! (`BrowserlessResult`, Task 15) can assert on the pixels it ends up with
//! without a real browser or WebGPU pipeline.
//!
//! Decode, generation-reset and swizzle logic for PalRle and Cdf53 already
//! exist in `ghostframe-client-core` (`pal_rle_decode::decode_pal_rle_tile`,
//! `cdf53_tile_state::Cdf53TileState::integrate`) and is not reimplemented
//! here. The only swizzle this module writes itself is Solid's BGRA ->
//! RGBA expansion, since `ghostframe_protocol::codec::solid::decode_solid`
//! only extracts the 4 raw bytes and leaves tiling to the caller.
//!
//! ## Staleness policy (this module's decision, not client-core's)
//!
//! `generation` is a 4-bit field (0..=15) that wraps, and nothing in this
//! codebase defines a wrap-aware ordering over it — `cdf53_coverage.rs` and
//! `Cdf53TileState` both only ever compare generations by equality. So
//! "older generation" has no well-defined meaning here, and this module
//! does not invent a `<` comparison for it.
//!
//! Instead, each tile tracks `current: u8` (the generation it is presently
//! accumulating) and `seen: u16` (a bitmask, one bit per generation value,
//! of every generation the tile has ever held). On `apply` with generation
//! `g`:
//!
//! - New tile: adopt `g` as current, mark it seen, store normally.
//! - `g == current`: store normally (this is the common progressive-pass
//!   case for Cdf53).
//! - `g != current` and bit `g` is already set in `seen`: `g` is a
//!   generation this tile has already moved past and come back to — a late
//!   or reordered datagram. Bump `stale_generation_tiles`, drop the
//!   payload without applying it, and return `Ok(())`.
//! - `g != current` and bit `g` is not set: a normal forward advance.
//!   Adopt `g` as current, mark it seen, store normally.
//!
//! Note this layers *on top of* `Cdf53TileState::integrate`, which would
//! happily reset its own accumulator for any `generation` that differs
//! from what it currently holds — it has no concept of "stale" at all.
//! Dropping a stale payload before it ever reaches `integrate` is this
//! module's policy, applied uniformly across all codecs; the two are not
//! in conflict, `FrameBuffer` is simply a policy layer client-core does
//! not need to know about.

use std::collections::HashMap;

use ghostframe_client_core::cdf53_prevalidate::{prevalidate_cdf53, PrevalidatedCdf53};
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_client_core::pal_rle_decode::decode_pal_rle_tile;
use ghostframe_client_core::palette_shadow::PaletteShadow;
use ghostframe_client_core::DecodeErrorCode;
use ghostframe_lib::transport::protocol::Codec;
use ghostframe_protocol::codec::solid::decode_solid;

/// Per-tile staleness bookkeeping (see module docs for the policy).
#[derive(Debug, Clone, Copy)]
struct GenerationTracker {
    current: u8,
    seen: u16,
}

impl GenerationTracker {
    fn new(g: u8) -> Self {
        GenerationTracker {
            current: g,
            seen: 1u16 << g,
        }
    }

    /// Returns `true` if `g` should be applied, `false` if it is stale and
    /// must be dropped. On a non-stale advance, updates `current`/`seen`.
    fn accept(&mut self, g: u8) -> bool {
        if g == self.current {
            return true;
        }
        if self.seen & (1u16 << g) != 0 {
            // Already held this generation before and moved on: late/
            // reordered datagram, do not adopt it as current.
            return false;
        }
        self.current = g;
        self.seen |= 1u16 << g;
        true
    }
}

struct TileEntry {
    rgba: Vec<u8>,
    tracker: GenerationTracker,
}

/// The result of decoding one payload, before staleness has been decided.
/// Cdf53 is kept as a `PrevalidatedCdf53` rather than RGBA because turning
/// it into RGBA requires `Cdf53TileState::integrate`, which must not run
/// for a payload that turns out to be stale.
enum Decoded {
    Rgba(Vec<u8>),
    Cdf53Pass(PrevalidatedCdf53),
}

/// Client-side tile store: accumulates decoded RGBA per tile coordinate
/// from received codec payloads, and reports stale (late/reordered)
/// generation arrivals. See module docs for the staleness policy.
pub struct FrameBuffer {
    tiles: HashMap<(u8, u8), TileEntry>,
    palette_shadow: PaletteShadow,
    palettes: Box<[[[u8; 4]; 16]; 256]>,
    cdf53_state: Cdf53TileState,
    stale_generation_tiles: u32,
}

impl FrameBuffer {
    pub fn new() -> Self {
        FrameBuffer {
            tiles: HashMap::new(),
            palette_shadow: PaletteShadow::new(),
            palettes: Box::new([[[0u8; 4]; 16]; 256]),
            cdf53_state: Cdf53TileState::new(),
            stale_generation_tiles: 0,
        }
    }

    /// Apply one received tile payload.
    pub fn apply(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        generation: u8,
        codec: Codec,
        pass_idx: u8,
        payload: &[u8],
    ) -> Result<(), DecodeErrorCode> {
        // Out-of-scope codecs: accepted, but never create/update a tile
        // entry. Skip carries no pixel data by definition; H264 and Raw
        // decode paths are not implemented by this harness module (H264
        // needs a real decoder, Raw is unused by netsim scenes so far).
        if matches!(codec, Codec::Skip | Codec::H264 | Codec::Raw) {
            return Ok(());
        }

        // Staleness is decided BEFORE decoding, and the order matters:
        // `decode_pal_rle_tile` writes any bundled palette upsert into
        // `self.palettes` / `self.palette_shadow` as a side effect. Decoding
        // a stale payload would therefore let a superseded generation's
        // palette overwrite the live one for that slot, silently corrupting
        // every later tile that references it without bundling — and netsim
        // exists to reorder and duplicate exactly this traffic.
        //
        // The cost is that a payload which is both malformed and stale
        // returns Ok(()) rather than Err: it is dropped unexamined, which is
        // what a real client would do with a datagram it has already
        // superseded.
        let accepted = match self.tiles.get_mut(&(tile_x, tile_y)) {
            None => true,
            Some(entry) => entry.tracker.accept(generation),
        };

        if !accepted {
            self.stale_generation_tiles += 1;
            return Ok(());
        }

        let decoded = match codec {
            Codec::Solid => {
                let bgra = decode_solid(payload).map_err(|_| DecodeErrorCode::PayloadTooShort)?;
                Decoded::Rgba(expand_solid_bgra_to_rgba(bgra))
            }
            Codec::PalRle => Decoded::Rgba(decode_pal_rle_tile(
                payload,
                &mut self.palette_shadow,
                &mut self.palettes,
            )?),
            Codec::Cdf53 => Decoded::Cdf53Pass(prevalidate_cdf53(payload, generation, pass_idx)?),
            Codec::Skip | Codec::H264 | Codec::Raw => unreachable!("handled above"),
        };

        let rgba = match decoded {
            Decoded::Rgba(v) => v,
            Decoded::Cdf53Pass(p) => self.cdf53_state.integrate(tile_x, tile_y, &p),
        };

        self.tiles
            .entry((tile_x, tile_y))
            .and_modify(|e| e.rgba = rgba.clone())
            .or_insert_with(|| TileEntry {
                rgba,
                tracker: GenerationTracker::new(generation),
            });

        Ok(())
    }

    /// The 4096-byte RGBA contents of a tile, if it has received anything.
    pub fn tile_rgba(&self, tile_x: u8, tile_y: u8) -> Option<&[u8]> {
        self.tiles.get(&(tile_x, tile_y)).map(|e| e.rgba.as_slice())
    }

    pub fn stale_generation_tiles(&self) -> u32 {
        self.stale_generation_tiles
    }
}

impl Default for FrameBuffer {
    fn default() -> Self {
        Self::new()
    }
}

/// Expand a 4-byte BGRA sample (as returned by `decode_solid`) to a full
/// 4096-byte RGBA tile buffer (32x32 pixels), swizzling BGRA -> RGBA. This
/// is the one swizzle this module writes itself; PalRle and Cdf53 both
/// already return RGBA from client-core.
fn expand_solid_bgra_to_rgba(bgra: [u8; 4]) -> Vec<u8> {
    let rgba_pixel = [bgra[2], bgra[1], bgra[0], bgra[3]];
    let mut out = Vec::with_capacity(4096);
    for _ in 0..1024 {
        out.extend_from_slice(&rgba_pixel);
    }
    out
}
