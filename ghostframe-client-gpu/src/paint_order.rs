//! Which of a batch's tile updates may still be painted.
//!
//! [`Renderer::flush`](crate::renderer::Renderer::flush) draws a batch in a
//! fixed codec order, whatever server frame each entry came from. When two
//! frames land in one batch, an older stateless draw (Solid, PalRle,
//! `TileReady`) could sit over a newer one from another codec. The ledger
//! records the newest `frame_seq` seen per tile; a stateless entry strictly
//! older than that is superseded and dropped, which loses nothing because it
//! only overwrites pixels.
//!
//! CDF 5/3 entries are not asked about either, but they are not simply
//! "never dropped" any more. A pass feeds `Cdf53GridState`, which may hold it
//! back for a later flush (GPU budget), refuse it (older than a newer
//! stateless tile or H.264 frame already drawn over the tile) or cancel it
//! (superseded while waiting). A CDF 5/3 tile counts towards the newest
//! `frame_seq` **only in the flush that lists it for drawing**, so a pass that
//! is held back or draws nothing cannot make an older stateless draw
//! disappear; once listed, an older stateless draw under it is dropped, and the
//! grid is recorded first so that a newer stateless draw lands on top. Refinement
//! passes of one tile re-use a single `frame_seq` (see `dirty.rs`). Equal
//! sequence numbers never supersede.
//!
//! The ledger outlives a flush (see [`SeqLedger`]), so the same rule holds
//! across flushes, and an H.264 picture is ordered against the tiles too.
//!
//! Comparisons are RFC 1982 serial arithmetic, so a `u32` wrap is safe.

/// True when `a` is strictly newer than `b`.
pub(crate) fn seq_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

/// Tile coordinates are `u8` on the wire, so a flat table of this many entries
/// holds every tile with no hashing.
const TABLE: usize = 256 * 256;

/// The newest `frame_seq` painted (or about to be, in the batch being built)
/// per tile, kept for the life of the picture: it is **not** cleared by a
/// flush, so a late stateless entry or H.264 picture cannot repaint over a
/// newer paint from an earlier flush. Cleared when the framebuffer is
/// resized. One array lookup per tile; the table is allocated by the first
/// note, so a client that never paints costs nothing.
#[derive(Debug, Default)]
pub(crate) struct SeqLedger {
    newest: Vec<Option<u32>>,
    /// Newest `frame_seq` of a tile entry noted anywhere on the screen.
    max_tile: Option<u32>,
    /// Newest H.264 picture blitted. It covers every tile, so it is one value
    /// here rather than a write to every tile.
    h264: Option<u32>,
}

fn newest_of(cur: Option<u32>, seq: u32) -> Option<u32> {
    Some(match cur {
        Some(c) if !seq_newer(seq, c) => c,
        _ => seq,
    })
}

impl SeqLedger {
    /// Record an entry for `(tx, ty)` from frame `seq`.
    pub(crate) fn note(&mut self, tx: u8, ty: u8, seq: u32) {
        if self.newest.is_empty() {
            self.newest = vec![None; TABLE];
        }
        let slot = &mut self.newest[ty as usize * 256 + tx as usize];
        *slot = newest_of(*slot, seq);
        self.max_tile = newest_of(self.max_tile, seq);
    }

    /// Record an H.264 picture from `seq` drawn over the whole framebuffer.
    pub(crate) fn note_h264(&mut self, seq: u32) {
        self.h264 = newest_of(self.h264, seq);
    }

    /// Whether a newer entry for the tile (or a newer H.264 picture) exists,
    /// making a stateless entry from `seq` pointless.
    pub(crate) fn superseded(&self, tx: u8, ty: u8, seq: u32) -> bool {
        self.h264.is_some_and(|h| seq_newer(h, seq))
            || self
                .newest
                .get(ty as usize * 256 + tx as usize)
                .copied()
                .flatten()
                .is_some_and(|newest| seq_newer(newest, seq))
    }

    /// Whether an H.264 picture from `seq` is older than something already
    /// painted: a newer picture, or a tile from a later frame. Such a picture
    /// covers the whole screen, so drawing it would undo the newer paint.
    pub(crate) fn h264_stale(&self, seq: u32) -> bool {
        self.h264.is_some_and(|h| seq_newer(h, seq))
            || self.max_tile.is_some_and(|t| seq_newer(t, seq))
    }

    pub(crate) fn clear(&mut self) {
        self.newest.fill(None);
        self.max_tile = None;
        self.h264 = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_is_superseded_equal_and_newer_are_not() {
        let mut l = SeqLedger::default();
        l.note(1, 1, 10);
        assert!(l.superseded(1, 1, 9));
        assert!(!l.superseded(1, 1, 10));
        assert!(!l.superseded(1, 1, 11));
        assert!(!l.superseded(2, 1, 9), "other tile unaffected");
    }

    #[test]
    fn newest_wins_regardless_of_note_order_and_across_a_wrap() {
        let mut l = SeqLedger::default();
        l.note(0, 0, u32::MAX);
        l.note(0, 0, 2);
        l.note(0, 0, u32::MAX - 1);
        assert!(l.superseded(0, 0, u32::MAX));
        assert!(!l.superseded(0, 0, 2));
    }

    #[test]
    fn an_h264_picture_supersedes_older_tiles_and_older_pictures_are_stale() {
        let mut l = SeqLedger::default();
        l.note(3, 4, 10);
        assert!(l.h264_stale(9), "a tile from a later frame is on screen");
        assert!(!l.h264_stale(10), "equal never supersedes");
        assert!(!l.h264_stale(11));
        l.note_h264(11);
        assert!(l.superseded(0, 0, 10), "any tile: the picture covers it");
        assert!(!l.superseded(0, 0, 11));
        assert!(l.h264_stale(10), "an older picture than the one drawn");
    }

    #[test]
    fn the_table_spans_every_u8_tile() {
        let mut l = SeqLedger::default();
        l.note(255, 255, 7);
        assert!(l.superseded(255, 255, 6));
        assert!(!l.superseded(255, 254, 6));
        assert!(!l.superseded(254, 255, 6));
    }

    #[test]
    fn clear_forgets() {
        let mut l = SeqLedger::default();
        l.note(0, 0, 5);
        l.note_h264(5);
        l.clear();
        assert!(!l.superseded(0, 0, 1));
        assert!(!l.h264_stale(1));
    }
}
