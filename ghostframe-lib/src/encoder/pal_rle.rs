//! PalRLE tile codec — palette-indexed nibble-packed run-length encoding.
//!
//! Server-side state: `PaletteTable` carries persistent 256-slot lifecycle
//! (ref counts, `delivered` BitSet, `in_flight_carrying` counters) under
//! the M3.2a single-client invariant. See
//! `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`.

/// Maximum colors in a PalRLE palette. Tiles with more unique colors fall
/// through the classifier to BC1/Cdf53.
pub const MAX_PALETTE_COUNT: usize = 16;

/// Persistent palette table capacity.
pub const PALETTE_TABLE_SLOTS: usize = 256;

/// Canonical palette entry. Colors are BGRA-ascending sorted (matching the
/// canonical sort appended to `tile_analysis.comp`).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PaletteEntry {
    pub colors: [[u8; 4]; MAX_PALETTE_COUNT],
    pub count: u8,
}

/// Per-slot lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    Empty,
    Held,
    FreeButCached,
}

/// Per-frame palette-table activity, surfaced via tracing.
#[derive(Debug, Default, Clone, Copy)]
pub struct FramePaletteStats {
    pub reused_or_allocated: u32,
    pub fell_back_to_raw: u32,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum PalRleDecodeError {
    #[error("payload too short: needed {needed} bytes at offset {offset}, got {got}")]
    Truncated { needed: usize, offset: usize, got: usize },

    #[error("thin payload references uncached palette id {0}")]
    UncachedPalette(u8),

    #[error("palette index {index} >= count {count}")]
    IndexOutOfRange { index: u8, count: u8 },

    #[error("decoded {decoded} pixels, expected 1024")]
    PixelCountMismatch { decoded: u32 },

    #[error("palette count {0} exceeds MAX_PALETTE_COUNT")]
    PaletteCountOutOfRange(u8),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn palette_entry_default_is_empty() {
        let p = PaletteEntry::default();
        assert_eq!(p.count, 0);
        assert_eq!(p.colors[0], [0, 0, 0, 0]);
    }

    #[test]
    fn slot_state_variants_distinct() {
        assert_ne!(SlotState::Empty, SlotState::Held);
        assert_ne!(SlotState::Held, SlotState::FreeButCached);
    }
}
