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
//! ## Two ingest paths, for two different sources (task 15b)
//!
//! `FrameBuffer` has two ways to receive a tile, and they are NOT
//! duplicates of each other — a future reader must not try to "unify"
//! them, because the two staleness fields they key off have genuinely
//! different ordering properties:
//!
//! - [`FrameBuffer::apply`] ingests a **raw wire payload** and decodes it
//!   itself (Solid/PalRle/Cdf53). It exists to build expected tiles from
//!   `TileSpec`s and to unit-test this module's own decode/swizzle logic
//!   directly, without a real `ClientNet`. Its staleness is on the 4-bit
//!   `generation` wire field, which wraps at 16 and has no well-defined
//!   `<` ordering — see the "Staleness policy" section below for the
//!   bitmask-based scheme this forces.
//! - [`FrameBuffer::apply_tile_ready`] ingests **already-decoded RGBA**,
//!   as surfaced by a real `ghostframe_client_core`/`ClientNet` session
//!   (`Event::TileReady`). This is what a browserless scene run
//!   (`BrowserlessResult`) actually drives, since the client core decodes
//!   tiles internally and the harness never sees payloads or codecs on
//!   the receive side. Its staleness is on `frame_seq`, a `u32` that is
//!   monotonically increasing across a scene — so unlike `generation`,
//!   plain `<` is a correct staleness test here, and no bitmask is
//!   needed.
//!
//! These are two layers with two mechanisms, not one duplicated in two
//! places: `apply` proves the decode logic is correct in isolation;
//! `apply_tile_ready` records what a real client actually rendered. A
//! scene run only ever uses one of the two.
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
    /// Populated only by `apply` (raw wire payload path). `None` for a
    /// tile that has only ever been touched by `apply_tile_ready`.
    tracker: Option<GenerationTracker>,
    /// Populated only by `apply_tile_ready` (decoded-RGBA path). `None`
    /// for a tile that has only ever been touched by `apply`.
    highest_frame_seq: Option<u32>,
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
    /// Staleness counter for the `apply_tile_ready` ingest path. Kept
    /// separate from `stale_generation_tiles` (the `apply` path's
    /// counter) since the two staleness definitions are unrelated — see
    /// the module docs.
    stale_frame_tiles: u32,
}

impl FrameBuffer {
    pub fn new() -> Self {
        FrameBuffer {
            tiles: HashMap::new(),
            palette_shadow: PaletteShadow::new(),
            palettes: Box::new([[[0u8; 4]; 16]; 256]),
            cdf53_state: Cdf53TileState::new(),
            stale_generation_tiles: 0,
            stale_frame_tiles: 0,
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
            // A tile already present from `apply_tile_ready` but never
            // touched by `apply` has no tracker yet: treat this as the
            // first `apply`-path arrival for it, not a staleness check.
            Some(entry) => match entry.tracker.as_mut() {
                Some(tracker) => tracker.accept(generation),
                None => true,
            },
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
            .and_modify(|e| {
                e.rgba = rgba.clone();
                if e.tracker.is_none() {
                    e.tracker = Some(GenerationTracker::new(generation));
                }
            })
            .or_insert_with(|| TileEntry {
                rgba,
                tracker: Some(GenerationTracker::new(generation)),
                highest_frame_seq: None,
            });

        Ok(())
    }

    /// Ingest a tile as actually decoded by a real `ClientNet`/client-core
    /// session (`ghostframe_client_core::Event::TileReady`). See the
    /// module docs for why this is a distinct ingest path from `apply`,
    /// with its own staleness definition.
    ///
    /// `frame_seq` is monotonically increasing across a scene, so unlike
    /// `generation` a plain `<` comparison is a correct staleness test
    /// here: a `TileReady` whose `frame_seq` is strictly less than the
    /// highest one already stored for this tile is stale — the counter
    /// returned by `stale_frame_tiles()` is bumped and the stored pixels
    /// are left untouched. An equal or greater `frame_seq` always applies
    /// (this covers a tile re-arriving for the same frame, e.g. a later
    /// Cdf53 refinement pass folded into the same `frame_seq`).
    pub fn apply_tile_ready(&mut self, frame_seq: u32, tile_x: u8, tile_y: u8, rgba: Vec<u8>) {
        match self.tiles.get_mut(&(tile_x, tile_y)) {
            None => {
                self.tiles.insert(
                    (tile_x, tile_y),
                    TileEntry {
                        rgba,
                        tracker: None,
                        highest_frame_seq: Some(frame_seq),
                    },
                );
            }
            Some(entry) => {
                let stale = matches!(entry.highest_frame_seq, Some(highest) if frame_seq < highest);
                if stale {
                    self.stale_frame_tiles += 1;
                    return;
                }
                entry.rgba = rgba;
                entry.highest_frame_seq = Some(frame_seq);
            }
        }
    }

    /// The 4096-byte RGBA contents of a tile, if it has received anything.
    pub fn tile_rgba(&self, tile_x: u8, tile_y: u8) -> Option<&[u8]> {
        self.tiles.get(&(tile_x, tile_y)).map(|e| e.rgba.as_slice())
    }

    pub fn stale_generation_tiles(&self) -> u32 {
        self.stale_generation_tiles
    }

    /// Staleness counter for the `apply_tile_ready` ingest path (see its
    /// doc comment). This is the counter `BrowserlessResult` actually
    /// uses, since a scene run only ever calls `apply_tile_ready`.
    pub fn stale_frame_tiles(&self) -> u32 {
        self.stale_frame_tiles
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
