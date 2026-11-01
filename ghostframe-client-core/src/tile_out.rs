//! Where a decoded 32x32 RGBA tile is written.
//!
//! A tile is 32 rows of 128 bytes. [`TileOut`] is those rows at a caller-chosen
//! stride, so the same pixel writers fill a fresh `Vec` (stride 128: the web
//! client and `TileDelivery::Decoded`) or a slot inside a larger 2D surface
//! (stride = the surface's pitch: the native client's upload slabs) without
//! knowing which. [`TileSink`] is how [`crate::TileDecoder::decode_into`] asks
//! a consumer for such a slot.
//!
//! This crate knows nothing about GPUs; what a slot *is* stays behind the
//! trait's associated type.

use std::marker::PhantomData;
use std::ptr::NonNull;

use ghostframe_protocol::tile::TILE_SIZE;

use crate::event::Event;

/// Bytes in one tile row: 32 RGBA pixels.
pub const TILE_ROW_BYTES: usize = TILE_SIZE as usize * 4;
/// Bytes in a whole tile: 32 rows.
pub const TILE_BYTES: usize = TILE_ROW_BYTES * TILE_SIZE as usize;

/// A mutable view of one tile's 32 rows inside a larger buffer.
///
/// It holds a pointer and a stride, not a slice: a slice covering the tile's
/// 32 rows would also cover the 31 gaps between them, which in a slab belong
/// to neighbouring tiles that the GPU may be reading. Each row is handed out
/// as its own 128-byte `&mut`, so no reference ever spans bytes that are not
/// this tile's.
pub struct TileOut<'a> {
    base: NonNull<u8>,
    stride: usize,
    _borrow: PhantomData<&'a mut [u8]>,
}

impl<'a> TileOut<'a> {
    /// `buf` starts at the tile's first byte and must reach the end of its
    /// last row; `stride` is the distance between rows.
    ///
    /// Panics if the geometry cannot hold a tile: a short buffer here would be
    /// a write past the slot, so it is refused rather than clipped.
    #[inline]
    pub fn new(buf: &'a mut [u8], stride: usize) -> Self {
        assert!(stride >= TILE_ROW_BYTES, "stride narrower than a tile row");
        assert!(
            buf.len() >= stride * (TILE_SIZE as usize - 1) + TILE_ROW_BYTES,
            "buffer too short for 32 rows at stride {stride}"
        );
        TileOut {
            base: NonNull::from(buf).cast(),
            stride,
            _borrow: PhantomData,
        }
    }

    /// A tile at `base` in memory this value does not own, such as a slot in
    /// a mapped surface.
    ///
    /// # Safety
    /// For the lifetime `'a`: `base + y * stride .. + 128` must be valid for
    /// writes for every `y < 32`, `stride >= 128`, and nothing else may read
    /// or write those 128-byte row segments (the bytes between rows are not
    /// touched and may be in use by others).
    #[inline]
    pub unsafe fn from_raw(base: NonNull<u8>, stride: usize) -> Self {
        debug_assert!(stride >= TILE_ROW_BYTES);
        TileOut {
            base,
            stride,
            _borrow: PhantomData,
        }
    }

    /// Row `y` (0..32).
    #[inline]
    pub fn row_mut(&mut self, y: usize) -> &mut [u8; TILE_ROW_BYTES] {
        assert!(y < TILE_SIZE as usize, "tile row {y} out of range");
        // SAFETY: `new` checked the buffer holds 32 rows at `stride`, and
        // `from_raw`'s contract says the same of its memory, so the 128 bytes
        // at `y * stride` are in bounds and writable. `&mut self` makes this
        // the only reference to the row for the borrow's duration.
        unsafe {
            &mut *self
                .base
                .as_ptr()
                .add(y * self.stride)
                .cast::<[u8; TILE_ROW_BYTES]>()
        }
    }
}

/// A consumer's supply of tile slots, for [`crate::TileDecoder::decode_into`].
pub trait TileSink {
    /// What identifies a committed slot to the consumer.
    type Placed;

    /// A slot for the tile about to be written, or `None` when there is none
    /// (the decoder then produces an ordinary `Event::TileReady`). Calling it
    /// again before [`TileSink::commit`] returns the same slot.
    fn acquire(&mut self) -> Option<TileOut<'_>>;

    /// The slot last returned by `acquire` now holds the pixels of this tile.
    fn commit(&mut self, frame_seq: u32, tile_x: u8, tile_y: u8) -> Self::Placed;
}

/// A sink with no slots: every tile goes to a `Vec`.
pub struct NoSlots;

impl TileSink for NoSlots {
    type Placed = std::convert::Infallible;

    fn acquire(&mut self) -> Option<TileOut<'_>> {
        None
    }

    fn commit(&mut self, _: u32, _: u8, _: u8) -> Self::Placed {
        unreachable!("NoSlots never hands out a slot to commit")
    }
}

/// What [`crate::TileDecoder::decode_into`] produced for one input event.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded<P> {
    /// Any event, including a `TileReady` carrying its pixels in a `Vec`.
    Event(Event),
    /// A tile whose pixels were written into a slot the sink handed out.
    Placed {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        placed: P,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_land_at_their_stride() {
        let stride = TILE_ROW_BYTES * 3;
        let mut buf = vec![0u8; stride * 31 + TILE_ROW_BYTES];
        let mut out = TileOut::new(&mut buf, stride);
        for y in 0..TILE_SIZE as usize {
            out.row_mut(y).fill(y as u8 + 1);
        }
        for y in 0..TILE_SIZE as usize {
            assert!(buf[y * stride..][..TILE_ROW_BYTES]
                .iter()
                .all(|&b| b == y as u8 + 1));
            if y < 31 {
                // The gap between rows belongs to the neighbouring tiles.
                assert!(buf[y * stride + TILE_ROW_BYTES..(y + 1) * stride]
                    .iter()
                    .all(|&b| b == 0));
            }
        }
    }

    #[test]
    fn a_raw_tile_writes_only_its_own_row_segments() {
        let stride = TILE_ROW_BYTES * 4;
        let mut buf = vec![0xEEu8; stride * 32];
        let base = NonNull::new(buf[TILE_ROW_BYTES..].as_mut_ptr()).unwrap();
        // SAFETY: the tile's rows lie inside `buf` (one tile in, 32 strides)
        // and nothing else touches them while `out` lives.
        let mut out = unsafe { TileOut::from_raw(base, stride) };
        for y in 0..32 {
            out.row_mut(y).fill(y as u8);
        }
        for y in 0..32 {
            for (i, &b) in buf[y * stride..(y + 1) * stride].iter().enumerate() {
                let inside = (TILE_ROW_BYTES..2 * TILE_ROW_BYTES).contains(&i);
                assert_eq!(b, if inside { y as u8 } else { 0xEE }, "row {y} byte {i}");
            }
        }
    }

    #[test]
    #[should_panic(expected = "too short")]
    fn a_short_buffer_is_refused() {
        let mut buf = vec![0u8; TILE_BYTES - 1];
        let _ = TileOut::new(&mut buf, TILE_ROW_BYTES);
    }
}
