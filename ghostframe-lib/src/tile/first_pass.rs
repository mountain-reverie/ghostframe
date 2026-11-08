//! First-pass demand: what the tile codec's first pass costs on the wire.
//!
//! "First pass" is the critical tier: the only pass of a Solid or PalRle
//! tile, passes `0..=CRITICAL_PASS_MAX` of a CDF 5/3 tile (the bytes the
//! bridge counts as `bytes_emitted_critical`). The frame-mode classifier
//! compares its rate to the bandwidth estimate (design
//! `docs/superpowers/specs/2026-11-07-bandwidth-fit-mode-switch-design.md`).

use super::{CodecState, TileMetrics, UNIQUE_COLORS_UNKNOWN};
use std::collections::VecDeque;

/// The per-tile size a state is priced at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstPassKind {
    Solid,
    PalRle,
    Cdf53,
}

impl FirstPassKind {
    /// `None` for a state that sends nothing. An H264-classified tile is
    /// priced as CDF 5/3: that is what it would be sent as in tile mode.
    pub fn of(state: CodecState) -> Option<Self> {
        match state {
            CodecState::Skip | CodecState::PixelPerfect => None,
            CodecState::Solid => Some(Self::Solid),
            CodecState::PalRle { .. } => Some(Self::PalRle),
            CodecState::Cdf53 { .. } | CodecState::H264 { .. } => Some(Self::Cdf53),
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Solid => 0,
            Self::PalRle => 1,
            Self::Cdf53 => 2,
        }
    }
}

/// Wire bytes of a tile datagram before its payload: the datagram header plus
/// the tile header. Calibration observes `tile_wire_bytes_total`, which
/// counts datagram lengths including these.
const TILE_DATAGRAM_HEADER: usize =
    crate::transport::protocol::DATAGRAM_HEADER_SIZE + crate::transport::protocol::TILE_HEADER_SIZE;

/// Starting per-tile sizes (Solid, PalRle, Cdf53), wire bytes. Calibration
/// replaces them. Measured in an e2e run on 2026-11-07: a CDF 5/3 tile's
/// pass 0 payload averaged 26 B (all 14 passes about 570 B); 130 B leaves
/// margin for natural images until calibration replaces it. The old 1300 B
/// was the full-lossless upper bound and overstated first-pass demand 20-50x.
pub(crate) const PRIOR_BYTES: [f64; 3] = [
    (TILE_DATAGRAM_HEADER + 4) as f64,
    (TILE_DATAGRAM_HEADER + 100) as f64,
    (TILE_DATAGRAM_HEADER + 130) as f64,
];
/// EWMA weight of one calibration chunk.
pub(crate) const CALIBRATION_WEIGHT: f64 = 1.0 / 16.0;
/// A chunk this many pass-0 tiles big is one calibration sample, so bytes of
/// later passes that arrive without their tile are averaged in, not divided
/// by a decaying count.
pub(crate) const CALIBRATION_CHUNK_TILES: u32 = 16;

/// Per-kind first-pass bytes per tile, calibrated from what tile mode sends.
///
/// Emission batches accumulate into a pending chunk per kind. When a chunk
/// holds [`CALIBRATION_CHUNK_TILES`] pass-0 tiles, its bytes divided by its
/// tiles is one EWMA sample of `per_tile`. A run of batches with bytes but no
/// new tile therefore only grows the pending chunk; it never shrinks the
/// price. Every kind starts at its prior.
#[derive(Debug, Clone, Copy)]
pub struct FirstPassModel {
    per_tile: [f64; 3],
    pending_bytes: [u64; 3],
    pending_tiles: [u32; 3],
}

impl Default for FirstPassModel {
    fn default() -> Self {
        Self {
            per_tile: PRIOR_BYTES,
            pending_bytes: [0; 3],
            pending_tiles: [0; 3],
        }
    }
}

impl FirstPassModel {
    /// Bytes one tile in `state` costs on its first pass.
    pub fn state_bytes(&self, state: CodecState) -> f64 {
        FirstPassKind::of(state).map_or(0.0, |k| self.tile_bytes(k))
    }

    /// The calibrated (or prior) wire bytes of one first-pass tile of `kind`.
    pub fn tile_bytes_of(&self, kind: FirstPassKind) -> f64 {
        self.tile_bytes(kind)
    }

    fn tile_bytes(&self, kind: FirstPassKind) -> f64 {
        self.per_tile[kind.index()]
    }

    /// One emission batch's first-pass wire bytes of `kind` (no
    /// retransmissions) and how many of them were a tile's pass 0.
    pub fn observe(&mut self, kind: FirstPassKind, bytes: u64, tiles: u32) {
        let i = kind.index();
        self.pending_bytes[i] += bytes;
        self.pending_tiles[i] += tiles;
        if self.pending_tiles[i] >= CALIBRATION_CHUNK_TILES {
            let sample = self.pending_bytes[i] as f64 / f64::from(self.pending_tiles[i]);
            self.per_tile[i] += CALIBRATION_WEIGHT * (sample - self.per_tile[i]);
            self.pending_bytes[i] = 0;
            self.pending_tiles[i] = 0;
        }
    }
}

/// Length of the demand window, µs.
pub(crate) const DEMAND_WINDOW_US: u64 = 1_000_000;

/// First-pass bytes of each frame over the last [`DEMAND_WINDOW_US`].
/// At most one entry per frame, so it stays small (about 60 at 60 fps).
#[derive(Debug, Clone, Default)]
pub struct DemandWindow {
    frames: VecDeque<(u64, f64)>,
    sum: f64,
}

impl DemandWindow {
    /// Add a frame's bytes at `now_us`, drop frames a full window old, and
    /// return the demand in bit/s.
    pub fn push(&mut self, now_us: u64, bytes: f64) -> u64 {
        self.frames.push_back((now_us, bytes));
        self.sum += bytes;
        while let Some(&(t, b)) = self.frames.front() {
            if now_us.saturating_sub(t) < DEMAND_WINDOW_US {
                break;
            }
            self.frames.pop_front();
            self.sum -= b;
        }
        if self.frames.is_empty() {
            self.sum = 0.0;
        }
        (self.sum.max(0.0) * 8.0 * 1e6 / DEMAND_WINDOW_US as f64) as u64
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.sum = 0.0;
    }
}

/// The state a tile would be repainted in, from its last colour count: the
/// classifier's own colour rules (Rules 6-8 of `classify_tile`) without the
/// motion and idle rules, since a repaint sends every tile once.
pub fn repaint_state(metrics: &TileMetrics) -> CodecState {
    let uc = metrics.unique_colors;
    if uc != UNIQUE_COLORS_UNKNOWN && uc <= 1 {
        CodecState::Solid
    } else if uc != UNIQUE_COLORS_UNKNOWN && uc <= 16 {
        CodecState::PalRle { palette_id: 0 }
    } else {
        CodecState::Cdf53 {
            passes_sent: 0,
            present_passes: 0x3FFF,
        }
    }
}

#[cfg(test)]
#[path = "first_pass_tests.rs"]
mod tests;
