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

use std::collections::HashMap;

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
    /// CDF 5/3 tiles whose newest passes are accumulated but not yet
    /// reconstructed. Empty whenever no batch is being decoded.
    pending: PendingCdf53,
    /// Reconstructions a batch skipped because a later pass of the same tile
    /// arrived before the earlier one was drawn.
    cdf53_coalesced: u64,
    /// Hand CDF 5/3 payloads on untouched, for a consumer that reconstructs
    /// them itself (the native client's GPU grid). This decoder then keeps no
    /// CDF 5/3 state, and the other codecs are unaffected.
    cdf53_passthrough: bool,
    /// `TileData::Cdf53Wire` passes dropped because their streams did not
    /// expand. Validation upstream makes that impossible; this says so if it
    /// is not.
    cdf53_wire_failed: u64,
}

/// A CDF 5/3 tile waiting for its one reconstruction in a batch.
struct PendingTile {
    /// The newest accumulated pass's sequence number, which the picture is
    /// labelled with, as the one-at-a-time decode would label it.
    frame_seq: u32,
    /// Generation of those passes, to notice a switch that would discard them.
    generation: u8,
}

#[derive(Default)]
struct PendingCdf53 {
    tiles: HashMap<(u8, u8), PendingTile>,
    /// Tiles in the order they became pending, so that flushing them is
    /// deterministic. May name a tile that was flushed since (skipped then),
    /// or twice if it went pending again.
    order: Vec<(u8, u8)>,
    /// `order` while it is being flushed, kept so neither is freed and
    /// reallocated for every batch.
    scratch: Vec<(u8, u8)>,
    /// Passes accumulated since the hook last got [`BatchStep::Tick`].
    since_tick: usize,
}

impl PendingCdf53 {
    fn clear(&mut self) {
        self.tiles.clear();
        self.order.clear();
        self.scratch.clear();
        self.since_tick = 0;
    }
}

/// Passes accumulated between two [`BatchStep::Tick`]s.
const TICK_EVERY: usize = 256;

/// What [`TileDecoder::decode_batch_into`] tells its callback.
pub enum BatchStep<P> {
    /// A result, as `decode_into` would give it.
    Out(Decoded<P>),
    /// A CDF 5/3 reconstruction (milliseconds) is about to run. The consumer
    /// can send what it has finished first.
    BeforeReconstruct,
    /// [`TICK_EVERY`] passes were accumulated without any output, which
    /// takes a while on a deep batch. Lets a consumer that holds output back
    /// notice it has waited long enough.
    Tick,
}

impl TileDecoder {
    pub fn new() -> Self {
        TileDecoder {
            palettes: Box::new([[[0u8; 4]; 16]; 256]),
            cdf53: Cdf53TileState::new(),
            pending: PendingCdf53::default(),
            cdf53_coalesced: 0,
            cdf53_passthrough: false,
            cdf53_wire_failed: 0,
        }
    }

    /// A decoder that forwards CDF 5/3 passes as `TileData::Cdf53` events
    /// (`Decoded::Event(Event::TilePayload)`), in order with everything else,
    /// instead of reconstructing them. A pass that arrives as
    /// `TileData::Cdf53Wire` is RLE-expanded here first, so the consumer only
    /// ever sees expanded planes. See [`TileDecoder::cdf53_passthrough`].
    pub fn with_cdf53_passthrough(mut self, on: bool) -> Self {
        self.cdf53_passthrough = on;
        self
    }

    /// How many `TileData::Cdf53Wire` passes were dropped because they would
    /// not expand. Zero unless something other than the core built them.
    pub fn cdf53_wire_failed(&self) -> u64 {
        self.cdf53_wire_failed
    }

    /// Expand a `Cdf53Wire` pass into the `Cdf53` event every later stage
    /// handles. `None` (counted in [`Self::cdf53_wire_failed`]; the caller
    /// reports it) if the streams do not expand, which a pass validated by
    /// the core cannot do.
    fn expand_wire(&mut self, ev: Event) -> Option<Event> {
        let Event::TilePayload {
            frame_seq,
            tile_x,
            tile_y,
            generation,
            data:
                TileData::Cdf53Wire {
                    pass_idx,
                    streams,
                    present_passes,
                    new_generation,
                },
        } = ev
        else {
            return Some(ev);
        };
        let mut bit_planes = vec![0u8; 384];
        let planes: &mut [u8; 384] = bit_planes.as_mut_slice().try_into().ok()?;
        if crate::cdf53_prevalidate::decode_cdf53_wire(&streams, planes).is_err() {
            self.cdf53_wire_failed += 1;
            return None;
        }
        Some(Event::TilePayload {
            frame_seq,
            tile_x,
            tile_y,
            generation,
            data: TileData::Cdf53 {
                pass_idx,
                bit_planes,
                present_passes,
                new_generation,
            },
        })
    }

    /// How many CDF 5/3 reconstructions [`Self::decode_batch_into`] has not
    /// had to do, cumulatively: passes folded into a tile that was already
    /// waiting for its picture.
    pub fn cdf53_coalesced(&self) -> u64 {
        self.cdf53_coalesced
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
        let mut result = None;
        self.decode_one(ev, sink, false, &mut |_, step| {
            if let BatchStep::Out(d) = step {
                result = Some(d);
            }
            true
        });
        result
    }

    /// Decode `events`, in order, as if each went through
    /// [`TileDecoder::decode_into`], except that a CDF 5/3 tile that several
    /// of them carry a pass for is reconstructed once, from its newest pass,
    /// rather than once per pass. For a consumer that is behind: the earlier
    /// pictures would only have been overwritten before they were seen.
    ///
    /// `emit` gets each result as it is made, with the sink back so that it
    /// can use it between tiles. What it gets is what `decode_into` would
    /// give, with two differences, both among tile payloads only: a pending
    /// CDF 5/3 tile's picture comes out when something must follow it (a
    /// later Solid or PalRle for the same tile, a generation switch, any
    /// event that is not a tile payload or a palette update) or at the end,
    /// rather than where its newest pass was; and so tiles may swap places
    /// with one another there. The render side composes tiles independently,
    /// so only the order within a tile, and against every other kind of
    /// event, is kept. After the last event nothing is pending.
    ///
    /// `emit` also hears [`BatchStep::BeforeReconstruct`] ahead of each
    /// reconstruction and [`BatchStep::Tick`] every [`TICK_EVERY`] passes
    /// accumulated, for a consumer that holds output back and must not let it
    /// age behind slow work. They are not results and change nothing.
    /// Pending tiles are never flushed because of either: that would undo the
    /// coalescing.
    ///
    /// `emit` returns whether to go on. On `false` the rest of the batch is
    /// dropped undecoded, along with anything pending: a consumer whose output
    /// has gone away must not keep filling slots for it. Decoder state then
    /// no longer follows the stream, so it is only for a consumer that is
    /// done.
    pub fn decode_batch_into<S: TileSink>(
        &mut self,
        events: impl IntoIterator<Item = Event>,
        sink: &mut S,
        mut emit: impl FnMut(&mut S, BatchStep<S::Placed>) -> bool,
    ) {
        for ev in events {
            if !self.decode_one(ev, sink, true, &mut emit) {
                self.pending.clear();
                return;
            }
        }
        if !self.flush_all(sink, &mut emit) {
            self.pending.clear();
        }
    }

    /// [`TileDecoder::decode_batch_into`] with a sink that never has a slot:
    /// the events `decode` would give, collected.
    pub fn decode_batch(&mut self, events: impl IntoIterator<Item = Event>) -> Vec<Event> {
        let mut out = Vec::new();
        self.decode_batch_into(events, &mut NoSlots, |_, step| {
            match step {
                BatchStep::Out(Decoded::Event(ev)) => out.push(ev),
                BatchStep::Out(Decoded::Placed { placed, .. }) => match placed {},
                BatchStep::BeforeReconstruct | BatchStep::Tick => {}
            }
            true
        });
        out
    }

    /// The one implementation behind `decode_into` (`coalesce` false: a CDF
    /// 5/3 pass is reconstructed on the spot) and `decode_batch_into` (true).
    fn decode_one<S: TileSink>(
        &mut self,
        ev: Event,
        sink: &mut S,
        coalesce: bool,
        emit: &mut impl FnMut(&mut S, BatchStep<S::Placed>) -> bool,
    ) -> bool {
        let Some(ev) = self.expand_wire(ev) else {
            return true;
        };
        if coalesce {
            // Anything that must come after a pending picture flushes it
            // first. A palette update does not: palettes only colour PalRle
            // tiles, which have their own per-tile rule.
            match &ev {
                Event::TilePayload {
                    tile_x,
                    tile_y,
                    data: TileData::Solid(_) | TileData::PalRle { .. },
                    ..
                } => {
                    if !self.flush_tile(*tile_x, *tile_y, sink, emit) {
                        return false;
                    }
                }
                Event::TilePayload {
                    tile_x,
                    tile_y,
                    generation,
                    data: TileData::Cdf53 { new_generation, .. },
                    ..
                } => {
                    // The passes waiting would be discarded by the switch, but
                    // their picture is what the one-at-a-time decode would
                    // have drawn before the new generation has any substance.
                    let switches = self
                        .pending
                        .tiles
                        .get(&(*tile_x, *tile_y))
                        .is_some_and(|p| *new_generation || p.generation != *generation);
                    if switches && !self.flush_tile(*tile_x, *tile_y, sink, emit) {
                        return false;
                    }
                }
                // Already expanded to `Cdf53` above.
                Event::TilePayload {
                    data: TileData::Cdf53Wire { .. },
                    ..
                }
                | Event::PaletteUpdated { .. } => {}
                Event::TileReady { .. }
                | Event::FrameDimensions { .. }
                | Event::NeedsH264 { .. }
                | Event::DecodeError { .. }
                | Event::Evicted { .. } => {
                    if !self.flush_all(sink, emit) {
                        return false;
                    }
                }
            }
        }
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
                    // `expand_wire` has already turned these into `Cdf53`.
                    TileData::Cdf53Wire { .. } => return true,
                    TileData::Cdf53 {
                        pass_idx,
                        bit_planes,
                        present_passes,
                        new_generation,
                    } if self.cdf53_passthrough => {
                        return emit(
                            sink,
                            BatchStep::Out(Decoded::Event(Event::TilePayload {
                                frame_seq,
                                tile_x,
                                tile_y,
                                generation,
                                data: TileData::Cdf53 {
                                    pass_idx,
                                    bit_planes,
                                    present_passes,
                                    new_generation,
                                },
                            })),
                        );
                    }
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
                        let entry = PrevalidatedCdf53 {
                            generation,
                            pass_idx,
                            bit_planes,
                            present_passes,
                        };
                        if !coalesce {
                            let Some(bgr) = self.cdf53.integrate_bgr(tile_x, tile_y, &entry) else {
                                return true;
                            };
                            return self.place(
                                sink,
                                frame_seq,
                                tile_x,
                                tile_y,
                                TilePixels::Cdf53(bgr),
                                emit,
                            );
                        }
                        self.pending.since_tick += 1;
                        if self.pending.since_tick >= TICK_EVERY {
                            self.pending.since_tick = 0;
                            if !emit(sink, BatchStep::Tick) {
                                return false;
                            }
                        }
                        if self.cdf53.accumulate(tile_x, tile_y, &entry) {
                            let tile = PendingTile {
                                frame_seq,
                                generation,
                            };
                            if self.pending.tiles.insert((tile_x, tile_y), tile).is_some() {
                                self.cdf53_coalesced += 1;
                            } else {
                                self.pending.order.push((tile_x, tile_y));
                            }
                        }
                        return true;
                    }
                };
                return self.place(sink, frame_seq, tile_x, tile_y, pixels, emit);
            }
            Event::PaletteUpdated { palette_id, colors } => {
                // Only the reported entries change; slots past `count` keep
                // what they held, as the inline decode always did.
                let slot = &mut self.palettes[palette_id as usize];
                for (dst, src) in slot.iter_mut().zip(colors) {
                    *dst = src;
                }
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
                return emit(
                    sink,
                    BatchStep::Out(Decoded::Event(Event::FrameDimensions { width, height })),
                );
            }
            // Listed rather than `_`, so a new event variant has to be
            // decided on here.
            other @ (Event::TileReady { .. }
            | Event::NeedsH264 { .. }
            | Event::DecodeError { .. }
            | Event::Evicted { .. }) => return emit(sink, BatchStep::Out(Decoded::Event(other))),
        }
        true
    }

    /// Write `pixels` to a slot from `sink` when it has one, to a fresh `Vec`
    /// otherwise, and emit the result.
    fn place<S: TileSink>(
        &mut self,
        sink: &mut S,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pixels: TilePixels,
        emit: &mut impl FnMut(&mut S, BatchStep<S::Placed>) -> bool,
    ) -> bool {
        // Asked for only now that there are pixels to write: a CDF 5/3 pass
        // with nothing new to show must not take a slot.
        if let Some(mut out) = sink.acquire() {
            pixels.write(&mut out);
            let placed = sink.commit(frame_seq, tile_x, tile_y);
            return emit(
                sink,
                BatchStep::Out(Decoded::Placed {
                    frame_seq,
                    tile_x,
                    tile_y,
                    placed,
                }),
            );
        }
        let mut rgba = vec![0u8; TILE_BYTES];
        pixels.write(&mut TileOut::new(&mut rgba, TILE_ROW_BYTES));
        emit(
            sink,
            BatchStep::Out(Decoded::Event(Event::TileReady {
                frame_seq,
                tile_x,
                tile_y,
                rgba,
            })),
        )
    }

    /// Reconstruct and emit one pending tile, if it is pending. Whether to go on.
    fn flush_tile<S: TileSink>(
        &mut self,
        tile_x: u8,
        tile_y: u8,
        sink: &mut S,
        emit: &mut impl FnMut(&mut S, BatchStep<S::Placed>) -> bool,
    ) -> bool {
        let Some(tile) = self.pending.tiles.remove(&(tile_x, tile_y)) else {
            return true;
        };
        if !emit(sink, BatchStep::BeforeReconstruct) {
            return false;
        }
        match self.cdf53.reconstruct(tile_x, tile_y) {
            Some(bgr) => self.place(
                sink,
                tile.frame_seq,
                tile_x,
                tile_y,
                TilePixels::Cdf53(bgr),
                emit,
            ),
            None => true,
        }
    }

    /// [`Self::flush_tile`] for every pending tile, oldest first.
    fn flush_all<S: TileSink>(
        &mut self,
        sink: &mut S,
        emit: &mut impl FnMut(&mut S, BatchStep<S::Placed>) -> bool,
    ) -> bool {
        // `order` is swapped out rather than taken, so its buffer survives.
        self.pending.scratch.clear();
        std::mem::swap(&mut self.pending.order, &mut self.pending.scratch);
        for i in 0..self.pending.scratch.len() {
            let (x, y) = self.pending.scratch[i];
            if !self.flush_tile(x, y, sink, emit) {
                return false;
            }
        }
        true
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

    // ---- Batch decoding ----

    use crate::cdf53_prevalidate::prevalidate_cdf53;
    use ghostframe_protocol::codec::cdf53::{encode_passes_sparse, forward};

    /// A 32x32 BGRA tile with a diagonal gradient shifted by `seed`, so
    /// different seeds give different tiles and every pass has content.
    fn gradient(seed: u8) -> Vec<u8> {
        (0..1024u32)
            .flat_map(|i| {
                let (x, y) = ((i % 32) as u8, (i / 32) as u8);
                [
                    x.wrapping_mul(7).wrapping_add(seed),
                    y.wrapping_mul(5) ^ seed,
                    x.wrapping_add(y).wrapping_mul(3),
                    255,
                ]
            })
            .collect()
    }

    /// The wire passes of a real CDF 5/3 tile as `TilePayload` events, passes
    /// in order, `frame_seq` counting up from `seq0`.
    fn cdf_events(x: u8, generation: u8, new_generation: bool, seed: u8, seq0: u32) -> Vec<Event> {
        let (_, passes) = encode_passes_sparse(&forward(&gradient(seed)));
        passes
            .into_iter()
            .enumerate()
            .map(|(i, (idx, payload))| {
                let pre = prevalidate_cdf53(&payload, generation, idx).unwrap();
                Event::TilePayload {
                    frame_seq: seq0 + i as u32,
                    tile_x: x,
                    tile_y: 0,
                    generation,
                    data: TileData::Cdf53 {
                        pass_idx: pre.pass_idx,
                        bit_planes: pre.bit_planes,
                        present_passes: pre.present_passes,
                        new_generation: new_generation && i == 0,
                    },
                }
            })
            .collect()
    }

    #[test]
    fn passthrough_forwards_cdf53_in_order_and_leaves_other_codecs_alone() {
        let solid = |seq| Event::TilePayload {
            frame_seq: seq,
            tile_x: 5,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([1, 2, 3, 255]),
        };
        let mut events = vec![solid(1)];
        events.extend(cdf_events(0, 1, true, 3, 10));
        events.push(solid(99));
        let want = events.clone();

        // One at a time and as a batch must both hand back the CDF 5/3 events
        // untouched and the Solid as pixels, in the order they came.
        for batched in [false, true] {
            let mut dec = TileDecoder::new().with_cdf53_passthrough(true);
            let out = if batched {
                dec.decode_batch(events.clone())
            } else {
                events
                    .clone()
                    .into_iter()
                    .filter_map(|e| dec.decode(e))
                    .collect()
            };
            assert_eq!(out.len(), want.len(), "batched {batched}");
            for (got, sent) in out.iter().zip(&want) {
                match sent {
                    Event::TilePayload {
                        data: TileData::Cdf53 { .. },
                        ..
                    } => assert_eq!(got, sent, "batched {batched}: not forwarded as it was"),
                    _ => assert!(
                        matches!(got, Event::TileReady { .. }),
                        "batched {batched}: a Solid must still decode: {got:?}"
                    ),
                }
            }
            assert_eq!(dec.cdf53_coalesced(), 0, "no CDF 5/3 state kept");
        }
    }

    /// What a tile output of either kind says: (frame_seq, tile_x, pixels).
    fn tile_of(ev: &Event) -> Option<(u32, u8, &[u8])> {
        match ev {
            Event::TileReady {
                frame_seq,
                tile_x,
                rgba,
                ..
            } => Some((*frame_seq, *tile_x, rgba)),
            _ => None,
        }
    }

    fn one_at_a_time(events: &[Event]) -> Vec<Event> {
        let (mut dec, _) = setup();
        events
            .iter()
            .filter_map(|e| dec.decode(e.clone()))
            .collect()
    }

    fn batched(events: &[Event], chunk: usize) -> (Vec<Event>, u64) {
        let (mut dec, _) = setup();
        let out = events
            .chunks(chunk)
            .flat_map(|c| dec.decode_batch(c.iter().cloned()))
            .collect();
        (out, dec.cdf53_coalesced())
    }

    /// The last picture of each tile, by tile column.
    fn finals(out: &[Event]) -> std::collections::BTreeMap<u8, Vec<u8>> {
        out.iter()
            .filter_map(tile_of)
            .map(|(_, x, px)| (x, px.to_vec()))
            .collect()
    }

    /// Round-robin the passes of several tiles, as a server's burst does.
    fn interleaved(tiles: Vec<Vec<Event>>) -> Vec<Event> {
        let longest = tiles.iter().map(Vec::len).max().unwrap();
        (0..longest)
            .flat_map(|i| tiles.iter().filter_map(move |t| t.get(i).cloned()))
            .collect()
    }

    #[test]
    fn batched_pictures_end_equal_to_one_at_a_time_for_any_splitting() {
        let events = interleaved(
            (0..4)
                .map(|x| cdf_events(x, 0, true, 30 * x, 100 * x as u32))
                .collect(),
        );
        let want = finals(&one_at_a_time(&events));
        assert_eq!(want.len(), 4);
        // Distinct, non-trivial tiles, or equality would be vacuous.
        assert!(want.values().all(|p| p.iter().any(|&b| b != p[0])));
        assert_ne!(want[&0], want[&1]);
        let reference_outputs = one_at_a_time(&events).len();
        for chunk in [1, 2, 3, 7, events.len()] {
            let (out, _) = batched(&events, chunk);
            assert_eq!(finals(&out), want, "chunk {chunk}");
            assert!(out.len() <= reference_outputs, "chunk {chunk}");
        }
    }

    #[test]
    fn a_batch_reconstructs_each_tile_once() {
        let events = cdf_events(0, 0, true, 9, 0);
        assert!(events.len() >= 4, "{} passes", events.len());
        let single = one_at_a_time(&events);
        assert!(single.len() >= 2, "one at a time draws several versions");
        let (batch, coalesced) = batched(&events, events.len());
        assert_eq!(batch.len(), 1);
        assert_eq!(finals(&batch), finals(&single));
        // Labelled with the newest pass, as the last one-at-a-time picture is.
        assert_eq!(
            tile_of(&batch[0]).unwrap().0,
            tile_of(single.last().unwrap()).unwrap().0
        );
        assert!(coalesced > 0);
        // One at a time, nothing is ever coalesced.
        assert_eq!(batched(&events, 1).1, 0);
    }

    #[test]
    fn a_solid_after_cdf_passes_comes_after_the_picture() {
        let mut events = cdf_events(0, 0, true, 9, 0);
        let solid_seq = 1000;
        events.push(Event::TilePayload {
            frame_seq: solid_seq,
            tile_x: 0,
            tile_y: 0,
            generation: 0,
            data: TileData::Solid([9, 8, 7, 255]),
        });
        // The passes alone draw something, or "the picture first" says nothing.
        assert!(!one_at_a_time(&events[..events.len() - 1]).is_empty());
        let single = one_at_a_time(&events);
        let (batch, _) = batched(&events, events.len());
        assert_eq!(batch.len(), 2, "the picture, then the solid");
        assert_eq!(tile_of(&batch[1]).unwrap().0, solid_seq);
        assert_ne!(tile_of(&batch[0]).unwrap().0, solid_seq);
        assert_eq!(finals(&batch), finals(&single));
        // And the other way: a solid for another tile does not flush it.
        let mut events = cdf_events(0, 0, true, 9, 0);
        events.insert(2, payload(5, TileData::Solid([1, 1, 1, 255])));
        let (batch, _) = batched(&events, events.len());
        assert_eq!(batch.len(), 2);
        assert_eq!(
            tile_of(&batch[0]).unwrap().1,
            5,
            "the solid is not held back"
        );
    }

    #[test]
    fn other_events_flush_pending_pictures_before_them() {
        let mut events = cdf_events(0, 0, true, 9, 0);
        let mid = events.len() / 2;
        events.insert(
            mid,
            Event::FrameDimensions {
                width: 640,
                height: 480,
            },
        );
        let (batch, _) = batched(&events, events.len());
        let at = batch
            .iter()
            .position(|e| matches!(e, Event::FrameDimensions { .. }))
            .unwrap();
        // Whatever the first half had to show is out before the marker.
        let first_half = one_at_a_time(&events[..mid]);
        assert!(!first_half.is_empty());
        assert_eq!(at, 1, "one picture precedes it");
        assert_eq!(
            tile_of(&batch[0]).unwrap().2,
            tile_of(first_half.last().unwrap()).unwrap().2
        );
        assert_eq!(finals(&batch), finals(&one_at_a_time(&events)));
        // A palette update, by contrast, does not cut a batch short.
        let mut events = cdf_events(0, 0, true, 9, 0);
        events.insert(
            mid,
            Event::PaletteUpdated {
                palette_id: 1,
                colors: vec![[1, 2, 3, 4]],
            },
        );
        assert_eq!(batched(&events, events.len()).0.len(), 1);
    }

    #[test]
    fn a_generation_switch_inside_a_batch_keeps_both_pictures_apart() {
        // (same number, new_generation flag): a reused number is told apart by
        // the flag; a different number by the number alone, flag clear.
        for (same_number, flagged) in [(true, true), (false, true), (false, false)] {
            let old = cdf_events(0, 0, true, 9, 0);
            let new = cdf_events(0, if same_number { 0 } else { 1 }, flagged, 77, 500);
            let events: Vec<Event> = old.iter().chain(&new).cloned().collect();
            let single = one_at_a_time(&events);
            let last_old = one_at_a_time(&old).pop().unwrap();
            let (batch, _) = batched(&events, events.len());
            assert_eq!(
                batch.len(),
                2,
                "same_number {same_number} flagged {flagged}"
            );
            assert_eq!(tile_of(&batch[0]).unwrap().2, tile_of(&last_old).unwrap().2);
            assert_eq!(finals(&batch), finals(&single));
            assert_ne!(tile_of(&batch[0]).unwrap().2, tile_of(&batch[1]).unwrap().2);
        }
    }

    #[test]
    fn batched_slot_pixels_equal_the_vec_pixels() {
        let events = interleaved(vec![
            cdf_events(0, 0, true, 3, 0),
            cdf_events(1, 0, true, 40, 100),
        ]);
        let (mut slot_dec, _) = setup();
        let mut sink = BandSink::new(8);
        let mut placed = Vec::new();
        slot_dec.decode_batch_into(events.clone(), &mut sink, |_, step| {
            match step {
                BatchStep::Out(Decoded::Placed {
                    tile_x, placed: p, ..
                }) => placed.push((tile_x, p)),
                BatchStep::Out(Decoded::Event(_)) => panic!("slots were free"),
                BatchStep::BeforeReconstruct | BatchStep::Tick => {}
            }
            true
        });
        assert_eq!(placed.len(), 2);
        let want = finals(&batched(&events, events.len()).0);
        for (x, slot) in placed {
            assert_eq!(sink.tile(slot), want[&x]);
        }
    }

    /// Three tiles' passes, interleaved.
    fn stop_events() -> Vec<Event> {
        interleaved(vec![
            cdf_events(0, 0, true, 3, 0),
            cdf_events(1, 0, true, 40, 100),
            cdf_events(2, 0, true, 80, 200),
        ])
    }

    #[test]
    fn running_out_of_slots_mid_batch_falls_back_in_order() {
        let events = interleaved(vec![
            cdf_events(0, 0, true, 3, 0),
            cdf_events(1, 0, true, 40, 100),
        ]);
        let (mut dec, _) = setup();
        let mut sink = BandSink::new(1);
        let mut out = Vec::new();
        dec.decode_batch_into(events.clone(), &mut sink, |_, step| {
            if let BatchStep::Out(d) = step {
                out.push(d);
            }
            true
        });
        assert_eq!(out.len(), 2);
        let want = finals(&batched(&events, events.len()).0);
        let Decoded::Placed { tile_x, placed, .. } = &out[0] else {
            panic!("the first tile gets the slot");
        };
        assert_eq!(sink.tile(*placed), want[tile_x]);
        let Decoded::Event(Event::TileReady { tile_x, rgba, .. }) = &out[1] else {
            panic!("the second falls back to a Vec");
        };
        assert_eq!(rgba, &want[tile_x]);
    }

    #[test]
    fn a_consumer_that_stops_the_batch_gets_no_further_slots() {
        let (mut dec, _) = setup();
        let mut sink = BandSink::new(8);
        let mut outs = 0;
        dec.decode_batch_into(stop_events(), &mut sink, |_, step| {
            if let BatchStep::Out(_) = step {
                outs += 1;
                return false;
            }
            true
        });
        assert_eq!(outs, 1);
        assert_eq!(sink.acquires, 1, "nothing was written after the stop");
        assert!(dec.pending.tiles.is_empty() && dec.pending.order.is_empty());
        // And the decoder is usable: a fresh stream draws as normal.
        let (batch, _) = (dec.decode_batch(cdf_events(5, 0, true, 9, 0)), 0);
        assert_eq!(batch.len(), 1);
    }

    #[test]
    fn the_hook_hears_each_reconstruction_before_its_picture_and_ticks_on_passes() {
        let events = stop_events();
        let (mut dec, _) = setup();
        let mut log = Vec::new();
        dec.decode_batch_into(events.clone(), &mut NoSlots, |_, step| {
            log.push(match step {
                BatchStep::Out(_) => 'o',
                BatchStep::BeforeReconstruct => 'r',
                BatchStep::Tick => 't',
            });
            true
        });
        assert_eq!(log.iter().collect::<String>(), "rororo");
        // A deep batch ticks every TICK_EVERY passes, and still draws each
        // tile once: ticking does not flush.
        let many: Vec<Event> = (0..TICK_EVERY * 2 + 3)
            .map(|i| cdf_events(0, 0, i == 0, 9, 0)[(i % 3).min(2)].clone())
            .collect();
        let (mut dec, _) = setup();
        let (mut ticks, mut outs) = (0, 0);
        dec.decode_batch_into(many, &mut NoSlots, |_, step| {
            match step {
                BatchStep::Tick => ticks += 1,
                BatchStep::Out(_) => outs += 1,
                BatchStep::BeforeReconstruct => {}
            }
            true
        });
        assert_eq!(ticks, 2);
        assert!(outs <= 1, "{outs}");
    }

    #[test]
    fn needs_h264_and_decode_errors_flush_pending_pictures_first() {
        use crate::event::DecodeErrorCode;
        use ghostframe_protocol::protocol::Codec;
        let other = [
            Event::NeedsH264 {
                frame_seq: 1,
                timestamp_us: 0,
                is_keyframe: true,
                ref_frame_seq: 0,
                flags: 0,
                slots: 0,
                payload: vec![],
            },
            Event::DecodeError {
                codec: Codec::Cdf53,
                tile_x: 0,
                tile_y: 0,
                code: DecodeErrorCode::Cdf53BadPass,
            },
        ];
        for marker in other {
            let mut events = cdf_events(0, 0, true, 9, 0);
            events.push(marker);
            let (batch, _) = batched(&events, events.len());
            assert_eq!(batch.len(), 2);
            assert!(tile_of(&batch[0]).is_some(), "the picture first");
            assert!(tile_of(&batch[1]).is_none());
        }
    }

    #[test]
    fn stopping_on_an_in_place_result_drops_the_rest_of_the_batch() {
        let (mut dec, _) = setup();
        let mut sink = BandSink::new(8);
        let mut events = cdf_events(0, 0, true, 3, 0);
        events.extend((1..4).map(|x| payload(x, TileData::Solid([x, 0, 0, 255]))));
        dec.decode_batch_into(events, &mut sink, |_, step| {
            !matches!(step, BatchStep::Out(_))
        });
        assert_eq!(sink.acquires, 1, "only the first solid was written");
        assert!(dec.pending.tiles.is_empty());
    }

    /// `cdf_events` with every pass as the wire variant the core emits in
    /// wire mode, built from the same payloads.
    fn wire_events(x: u8, generation: u8, new_generation: bool, seed: u8, seq0: u32) -> Vec<Event> {
        let (_, passes) = encode_passes_sparse(&forward(&gradient(seed)));
        passes
            .into_iter()
            .enumerate()
            .map(|(i, (idx, payload))| {
                let info = crate::cdf53_prevalidate::validate_cdf53(&payload, idx).unwrap();
                Event::TilePayload {
                    frame_seq: seq0 + i as u32,
                    tile_x: x,
                    tile_y: 0,
                    generation,
                    data: TileData::Cdf53Wire {
                        pass_idx: idx,
                        streams: payload[info.streams].to_vec(),
                        present_passes: info.present_passes,
                        new_generation: new_generation && i == 0,
                    },
                }
            })
            .collect()
    }

    /// Wire passes decode to exactly what the planes variant decodes to, in
    /// CPU mode (one at a time and batched) and in passthrough mode (the
    /// forwarded event is the planes variant).
    #[test]
    fn wire_passes_behave_as_plane_passes() {
        let planes: Vec<Event> = (0..3u8)
            .flat_map(|x| cdf_events(x, 1, true, 3 + x, 10))
            .collect();
        let wire: Vec<Event> = (0..3u8)
            .flat_map(|x| wire_events(x, 1, true, 3 + x, 10))
            .collect();
        assert!(wire.iter().all(|e| matches!(
            e,
            Event::TilePayload {
                data: TileData::Cdf53Wire { .. },
                ..
            }
        )));
        for passthrough in [false, true] {
            let new = || TileDecoder::new().with_cdf53_passthrough(passthrough);
            let one = |evs: &[Event]| -> Vec<Event> {
                let mut d = new();
                evs.iter().cloned().filter_map(|e| d.decode(e)).collect()
            };
            let got = one(&wire);
            assert!(!got.is_empty());
            assert_eq!(
                got,
                one(&planes),
                "one at a time, passthrough {passthrough}"
            );
            assert_eq!(
                new().decode_batch(wire.clone()),
                new().decode_batch(planes.clone()),
                "batched, passthrough {passthrough}"
            );
            if passthrough {
                assert!(got.iter().all(|e| matches!(
                    e,
                    Event::TilePayload {
                        data: TileData::Cdf53 { .. },
                        ..
                    }
                )));
            } else {
                assert!(got.iter().all(|e| tile_of(e).is_some()));
            }
        }
    }

    #[test]
    fn a_wire_pass_that_will_not_expand_is_dropped_and_counted() {
        for batched in [false, true] {
            let mut dec = TileDecoder::new();
            let bad = Event::TilePayload {
                frame_seq: 1,
                tile_x: 0,
                tile_y: 0,
                generation: 0,
                data: TileData::Cdf53Wire {
                    pass_idx: 1,
                    streams: vec![0, 1, 0xFE, 0, 1, 0xFF], // short, then cut
                    present_passes: None,
                    new_generation: true,
                },
            };
            let out = if batched {
                dec.decode_batch(vec![bad])
            } else {
                dec.decode(bad).into_iter().collect()
            };
            assert!(out.is_empty());
            assert_eq!(dec.cdf53_wire_failed(), 1);
        }
    }
}
