//! Which of a batch's tile updates may still be painted.
//!
//! [`Renderer::flush`](crate::renderer::Renderer::flush) draws a batch in a
//! fixed codec order, whatever server frame each entry came from. When two
//! frames land in one batch, an older stateless draw (Raw, Solid, PalRle,
//! `TileReady`) could sit over a newer one from another codec. The ledger
//! records the newest `frame_seq` seen per tile; a stateless entry strictly
//! older than that is superseded and dropped, which loses nothing because it
//! only overwrites pixels.
//!
//! CDF 5/3 entries are *never* dropped and are not asked about: they
//! integrate into per-tile coefficient state, and refinement passes of one
//! tile re-use a single `frame_seq` (see `dirty.rs`). They do count towards
//! the newest `frame_seq`, so an older stateless draw under a newer pass is
//! dropped, and the flush draws the lift first so a newer stateless draw lands
//! on top of an older pass. Equal sequence numbers never supersede.
//!
//! Comparisons are RFC 1982 serial arithmetic, so a `u32` wrap is safe.

use std::collections::HashMap;

/// True when `a` is strictly newer than `b`.
pub(crate) fn seq_newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

#[derive(Debug, Default)]
pub(crate) struct SeqLedger {
    newest: HashMap<(u8, u8), u32>,
}

impl SeqLedger {
    /// Record an entry for `(tx, ty)` from frame `seq`.
    pub(crate) fn note(&mut self, tx: u8, ty: u8, seq: u32) {
        self.newest
            .entry((tx, ty))
            .and_modify(|cur| {
                if seq_newer(seq, *cur) {
                    *cur = seq;
                }
            })
            .or_insert(seq);
    }

    /// Whether a newer entry for the tile exists, making a stateless entry
    /// from `seq` pointless.
    pub(crate) fn superseded(&self, tx: u8, ty: u8, seq: u32) -> bool {
        self.newest
            .get(&(tx, ty))
            .is_some_and(|&newest| seq_newer(newest, seq))
    }

    pub(crate) fn clear(&mut self) {
        self.newest.clear();
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
    fn clear_forgets() {
        let mut l = SeqLedger::default();
        l.note(0, 0, 5);
        l.clear();
        assert!(!l.superseded(0, 0, 1));
    }
}
