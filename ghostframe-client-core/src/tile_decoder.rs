//! CPU pixel decode of validated tile payloads: `Event::TilePayload` in,
//! `Event::TileReady` out.
//!
//! Everything a tile does to the *protocol* -- prevalidation, palette
//! bookkeeping for later thin tiles, CDF 5/3 coverage, ACKs, NACKs, decode
//! errors -- happens in [`crate::ClientCore`], in either delivery mode. What
//! is left is pure pixel work, and this type does it. Splitting there lets a
//! native client run it on its own thread, so the thread that owns the
//! transport's timers never waits on an inverse wavelet transform.
//!
//! [`crate::TileDelivery::Decoded`] is this type applied inline to
//! [`crate::TileDelivery::Payload`]'s events, so the two cannot drift: a
//! consumer that runs it elsewhere gets exactly the events the core would
//! have emitted itself.

use ghostframe_protocol::tile::TILE_SIZE;

use crate::cdf53_prevalidate::PrevalidatedCdf53;
use crate::cdf53_tile_state::Cdf53TileState;
use crate::event::{Event, TileData};

/// Decodes the payload events of one [`crate::ClientCore`] in
/// [`crate::TileDelivery::Payload`] mode. Stateful (palettes, CDF 5/3 pass
/// accumulation), so it must see that core's events in order, all of them.
pub struct TileDecoder {
    /// BGRA palette tables, `[palette_id][slot] = [b, g, r, a]`, maintained
    /// from `Event::PaletteUpdated`.
    palettes: Box<[[[u8; 4]; 16]; 256]>,
    cdf53: Cdf53TileState,
}

impl TileDecoder {
    pub fn new() -> Self {
        TileDecoder {
            palettes: Box::new([[[0u8; 4]; 16]; 256]),
            cdf53: Cdf53TileState::new(),
        }
    }

    /// Map one event to what `TileDelivery::Decoded` emits in its place:
    /// a tile payload becomes its pixels (or nothing, while a CDF 5/3 tile
    /// has nothing new to show), a palette update is absorbed, and every
    /// other event passes through unchanged.
    pub fn decode(&mut self, ev: Event) -> Option<Event> {
        match ev {
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => {
                let rgba = match data {
                    TileData::Solid(bgra) => Some(solid_rgba(bgra)),
                    TileData::PalRle {
                        palette_id,
                        indices,
                        ..
                    } => Some(palrle_rgba(&self.palettes[palette_id as usize], &indices)),
                    TileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                        present_passes,
                        new_generation,
                    } => {
                        if new_generation {
                            // The integrator keys on the 4-bit generation
                            // alone, so it cannot see a new generation that
                            // reuses the number; the core can, and says so.
                            self.cdf53.forget_tile(tile_x, tile_y);
                        }
                        self.cdf53.integrate(
                            tile_x,
                            tile_y,
                            &PrevalidatedCdf53 {
                                generation,
                                pass_idx,
                                bit_planes,
                                present_passes,
                            },
                        )
                    }
                };
                rgba.map(|rgba| Event::TileReady {
                    frame_seq,
                    tile_x,
                    tile_y,
                    rgba,
                })
            }
            Event::PaletteUpdated { palette_id, colors } => {
                // Only the reported entries change; slots past `count` keep
                // what they held, as the inline decode always did.
                let slot = &mut self.palettes[palette_id as usize];
                for (dst, src) in slot.iter_mut().zip(colors) {
                    *dst = src;
                }
                None
            }
            Event::FrameDimensions { width, height } => {
                // The core drops per-tile state outside a new grid; drop the
                // accumulated planes too, so a shrink does not keep them for
                // good. Memory only: a tile that comes back after a regrow
                // has no coverage entry in the core, so its first pass
                // arrives with `new_generation` set and starts clean anyway.
                let (cols, rows) = (width.div_ceil(TILE_SIZE), height.div_ceil(TILE_SIZE));
                self.cdf53
                    .retain_tiles(|x, y| (x as u32) < cols && (y as u32) < rows);
                Some(Event::FrameDimensions { width, height })
            }
            // Listed rather than `_`, so a new event variant has to be
            // decided on here.
            other @ (Event::TileReady { .. }
            | Event::NeedsH264 { .. }
            | Event::DecodeError { .. }
            | Event::Evicted { .. }) => Some(other),
        }
    }
}

impl Default for TileDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// One BGRA quad expanded to a full RGBA tile, alpha forced to 255 (as the
/// GPU solid expand does).
fn solid_rgba(bgra: [u8; 4]) -> Vec<u8> {
    let px = [bgra[2], bgra[1], bgra[0], 255];
    px.repeat((TILE_SIZE * TILE_SIZE) as usize)
}

/// 512 bytes of 4-bit indices (low nibble first) through a BGRA palette to
/// RGBA, alpha forced to 255.
fn palrle_rgba(palette: &[[u8; 4]; 16], indices: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity((TILE_SIZE * TILE_SIZE * 4) as usize);
    for &pair in indices {
        for idx in [pair & 0x0F, pair >> 4] {
            let bgra = palette[idx as usize];
            rgba.extend_from_slice(&[bgra[2], bgra[1], bgra[0], 255]);
        }
    }
    rgba
}
