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
use crate::cdf53_tile_state::{write_bgr_rows, Cdf53TileState};
use crate::event::{Event, TileData};
use crate::tile_out::{Decoded, NoSlots, TileOut, TileSink, TILE_BYTES, TILE_ROW_BYTES};

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
        match self.decode_into(ev, &mut NoSlots)? {
            Decoded::Event(ev) => Some(ev),
            Decoded::Placed { placed, .. } => match placed {},
        }
    }

    /// [`TileDecoder::decode`], except that a tile's pixels go into a slot
    /// from `sink` when it has one, and come back as
    /// [`Decoded::Placed`] instead of an `Event::TileReady`. When the sink has
    /// none the result is exactly what `decode` gives. `decode` is this
    /// function with a sink that never has a slot, so the two cannot differ in
    /// what they write.
    pub fn decode_into<S: TileSink>(
        &mut self,
        ev: Event,
        sink: &mut S,
    ) -> Option<Decoded<S::Placed>> {
        match ev {
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data,
            } => {
                let pixels = match data {
                    TileData::Solid(bgra) => TilePixels::Solid(bgra),
                    TileData::PalRle {
                        palette_id,
                        indices,
                        ..
                    } => TilePixels::PalRle {
                        palette: self.palettes[palette_id as usize],
                        indices,
                    },
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
                        TilePixels::Cdf53(self.cdf53.integrate_bgr(
                            tile_x,
                            tile_y,
                            &PrevalidatedCdf53 {
                                generation,
                                pass_idx,
                                bit_planes,
                                present_passes,
                            },
                        )?)
                    }
                };
                // Asked for only now that there are pixels to write: a CDF 5/3
                // pass with nothing new to show must not take a slot.
                if let Some(mut out) = sink.acquire() {
                    pixels.write(&mut out);
                    let placed = sink.commit(frame_seq, tile_x, tile_y);
                    return Some(Decoded::Placed {
                        frame_seq,
                        tile_x,
                        tile_y,
                        placed,
                    });
                }
                let mut rgba = vec![0u8; TILE_BYTES];
                pixels.write(&mut TileOut::new(&mut rgba, TILE_ROW_BYTES));
                Some(Decoded::Event(Event::TileReady {
                    frame_seq,
                    tile_x,
                    tile_y,
                    rgba,
                }))
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
                Some(Decoded::Event(Event::FrameDimensions { width, height }))
            }
            // Listed rather than `_`, so a new event variant has to be
            // decided on here.
            other @ (Event::TileReady { .. }
            | Event::NeedsH264 { .. }
            | Event::DecodeError { .. }
            | Event::Evicted { .. }) => Some(Decoded::Event(other)),
        }
    }
}

impl Default for TileDecoder {
    fn default() -> Self {
        Self::new()
    }
}

/// What a tile looks like, before it is put anywhere.
enum TilePixels {
    /// One BGRA quad, alpha forced to 255 (as the GPU solid expand does).
    Solid([u8; 4]),
    /// 512 bytes of 4-bit indices (low nibble first) through a BGRA palette,
    /// alpha forced to 255. The palette is copied: it can change before the
    /// pixels are written.
    PalRle {
        palette: [[u8; 4]; 16],
        indices: Vec<u8>,
    },
    /// 1024 BGR triples from the inverse wavelet, alpha 255.
    Cdf53(Vec<u8>),
}

impl TilePixels {
    fn write(&self, out: &mut TileOut<'_>) {
        match self {
            TilePixels::Solid(bgra) => {
                let px = [bgra[2], bgra[1], bgra[0], 255];
                let mut row = [0u8; TILE_ROW_BYTES];
                for dst in row.chunks_exact_mut(4) {
                    dst.copy_from_slice(&px);
                }
                for y in 0..TILE_SIZE as usize {
                    *out.row_mut(y) = row;
                }
            }
            TilePixels::PalRle { palette, indices } => {
                // 16 index bytes make one 32-pixel row. A short `indices`
                // (prevalidation guarantees 512) leaves its rows black rather
                // than reading past the end.
                for y in 0..TILE_SIZE as usize {
                    let mut row = [0u8; TILE_ROW_BYTES];
                    let src = indices.get(y * 16..(y + 1) * 16).unwrap_or(&[]);
                    for (px, &pair) in row.chunks_exact_mut(8).zip(src) {
                        let (lo, hi) = (
                            palette[(pair & 0x0F) as usize],
                            palette[(pair >> 4) as usize],
                        );
                        px[..4].copy_from_slice(&[lo[2], lo[1], lo[0], 255]);
                        px[4..].copy_from_slice(&[hi[2], hi[1], hi[0], 255]);
                    }
                    *out.row_mut(y) = row;
                }
            }
            TilePixels::Cdf53(bgr) => write_bgr_rows(bgr, out),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink over a flat buffer holding `slots` tiles laid out two tiles per
    /// row band (stride = 2 tiles), counting what the decoder asks of it.
    struct BandSink {
        buf: Vec<u8>,
        slots: usize,
        next: usize,
        acquires: usize,
    }

    impl BandSink {
        const STRIDE: usize = TILE_ROW_BYTES * 2;

        fn new(slots: usize) -> Self {
            BandSink {
                buf: vec![0xEE; Self::STRIDE * 32 * slots.div_ceil(2)],
                slots,
                next: 0,
                acquires: 0,
            }
        }

        fn origin(slot: usize) -> usize {
            (slot / 2) * Self::STRIDE * 32 + (slot % 2) * TILE_ROW_BYTES
        }

        /// The slot's pixels as a tight 4096-byte tile.
        fn tile(&self, slot: usize) -> Vec<u8> {
            let at = Self::origin(slot);
            (0..32)
                .flat_map(|y| self.buf[at + y * Self::STRIDE..][..TILE_ROW_BYTES].to_vec())
                .collect()
        }
    }

    impl TileSink for BandSink {
        type Placed = usize;

        fn acquire(&mut self) -> Option<TileOut<'_>> {
            self.acquires += 1;
            if self.next >= self.slots {
                return None;
            }
            let at = Self::origin(self.next);
            Some(TileOut::new(&mut self.buf[at..], Self::STRIDE))
        }

        fn commit(&mut self, _: u32, _: u8, _: u8) -> usize {
            self.next += 1;
            self.next - 1
        }
    }

    fn payload(x: u8, data: TileData) -> Event {
        Event::TilePayload {
            frame_seq: 7,
            tile_x: x,
            tile_y: 0,
            generation: 0,
            data,
        }
    }

    fn palrle(fill: u8) -> TileData {
        TileData::PalRle {
            palette_id: 3,
            count: 16,
            indices: (0..512).map(|i| (i as u8).wrapping_mul(7) ^ fill).collect(),
            wire_flags: 0,
        }
    }

    fn setup() -> (TileDecoder, TileDecoder) {
        let update = Event::PaletteUpdated {
            palette_id: 3,
            colors: (0..16u8).map(|i| [i, i * 3, i * 5, 0x80]).collect(),
        };
        let (mut a, mut b) = (TileDecoder::new(), TileDecoder::new());
        assert!(a.decode(update.clone()).is_none());
        assert!(b.decode(update).is_none());
        (a, b)
    }

    #[test]
    fn slot_pixels_equal_the_vec_pixels_for_every_codec() {
        let (mut vec_dec, mut slot_dec) = setup();
        let mut sink = BandSink::new(8);
        let cases = [
            TileData::Solid([1, 2, 3, 4]),
            palrle(0x5A),
            // Every pass absent: complete at pass 0, an all-zero tile.
            TileData::Cdf53 {
                pass_idx: 0,
                bit_planes: vec![0; 384],
                present_passes: Some(0),
                new_generation: true,
            },
        ];
        for (i, data) in cases.into_iter().enumerate() {
            let want = vec_dec.decode(payload(i as u8, data.clone())).unwrap();
            let Event::TileReady { rgba: want, .. } = want else {
                panic!("expected TileReady");
            };
            let got = slot_dec
                .decode_into(payload(i as u8, data), &mut sink)
                .unwrap();
            let Decoded::Placed {
                frame_seq,
                tile_x,
                tile_y,
                placed,
            } = got
            else {
                panic!("expected a placed tile");
            };
            assert_eq!((frame_seq, tile_x, tile_y, placed), (7, i as u8, 0, i));
            assert_eq!(sink.tile(placed), want, "case {i}");
            // Not all one value, or this would pass for a fill bug too.
            assert!(want.iter().any(|&b| b != want[0]) || i != 1);
        }
    }

    #[test]
    fn an_exhausted_sink_falls_back_to_a_vec_in_order() {
        let (mut dec, mut vec_dec) = setup();
        let mut sink = BandSink::new(1);
        let a = dec.decode_into(payload(0, palrle(1)), &mut sink).unwrap();
        let b = dec.decode_into(payload(1, palrle(2)), &mut sink).unwrap();
        assert!(matches!(a, Decoded::Placed { placed: 0, .. }));
        let Decoded::Event(Event::TileReady { rgba, .. }) = b else {
            panic!("second tile must fall back to a Vec");
        };
        let Some(Event::TileReady { rgba: want, .. }) = vec_dec.decode(payload(1, palrle(2)))
        else {
            panic!()
        };
        assert_eq!(rgba, want);
    }

    #[test]
    fn a_pass_with_nothing_new_takes_no_slot() {
        let (mut dec, _) = setup();
        let mut sink = BandSink::new(4);
        let pass0 = || {
            payload(
                0,
                TileData::Cdf53 {
                    pass_idx: 0,
                    bit_planes: vec![0; 384],
                    present_passes: Some(0),
                    new_generation: false,
                },
            )
        };
        assert!(dec.decode_into(pass0(), &mut sink).is_some());
        assert_eq!((sink.acquires, sink.next), (1, 1));
        // The same pass again changes nothing on screen.
        assert!(dec.decode_into(pass0(), &mut sink).is_none());
        assert_eq!((sink.acquires, sink.next), (1, 1), "no slot for no picture");
    }
}
