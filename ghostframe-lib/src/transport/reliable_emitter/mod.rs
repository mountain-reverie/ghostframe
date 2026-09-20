//! Reliable Tile Emitter — see docs/superpowers/specs/2026-06-17-reliable-tile-emitter-design.md

pub mod cache;
pub mod emission_queue;
pub mod emitter;
pub mod parity;
pub mod rto;
pub mod traits;
pub mod wire_seq;

#[cfg(test)]
mod sim;

#[cfg(test)]
mod proptest_invariants;

pub use emitter::ReliableTileEmitter;

/// Logical identity of a tile-pass — the unit ACKed, NACKed, RTO'd, and
/// cancelled by bump_generation. Matches M3.3d's ACK key bit-for-bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EmitKey {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
}

impl EmitKey {
    pub fn new(frame_seq: u32, tile_x: u8, tile_y: u8, pass_idx: u8) -> Self {
        Self {
            frame_seq,
            tile_x,
            tile_y,
            pass_idx,
        }
    }
}

// ---- Knob constants (spec §9) ----
pub const FEC_GROUP_SIZE_K: usize = 10;
pub const FEC_PARITY_PER_GROUP_R: usize = 1;
pub const PARITY_INTERLEAVE_OFFSET: u32 = (2 * FEC_GROUP_SIZE_K) as u32;
pub const END_OF_STREAM_PARITY_FLUSH_MS: u64 = 5;
pub const RTO_BACKOFF_FACTOR: u32 = 2;
// Sized for the first-paint burst: at 1920×1080 with 32×32 tiles the
// worst case is ~2040 dirty tiles × 14 cdf53 passes ≈ 28 K tile-passes
// submitted before any ACK can return. The original 8 K cap was a guess
// that fit a typical *post-paint* working set but undersized the burst —
// at 48 % wire loss on evangeline the LRU evicted ~16 K entries before
// they could retry, so half the first-paint burst saw exactly one
// emission attempt with no retransmit coverage. Bumped to 32 K so the
// whole first-paint burst stays cached for indefinite retransmits.
// Memory cost: ~500 B/entry × 32 K = ~16 MB per session, well below
// any realistic ceiling.
pub const CACHE_CAPACITY: usize = 32768;

/// Gate for the acknowledgement-ordering measurement.
///
/// Set `GHOSTFRAME_ACK_ORDER_PROBE=1` to log the `wire_seq` of every
/// acknowledgement entry in arrival order. Replaying an RFC 9002
/// packet-threshold rule against that order tells us whether ordering-based
/// loss inference is usable on a given path: the rule declares a
/// transmission lost once 3 later ones are acknowledged, so it is only safe
/// where reordering stays shallow *in sequence-number terms*.
///
/// Measured on the browserless netsim: 0% false declarations with no
/// reordering, but 32% at a 2 ms reorder window, with inversions 140 deep.
/// The sender is bursty -- hundreds of datagrams within microseconds -- so a
/// small time window spans a large sequence range. This probe exists to find
/// out what a real path does, since that result cannot be assumed.
pub(crate) fn ack_order_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GHOSTFRAME_ACK_ORDER_PROBE").is_ok_and(|v| v == "1"))
}

/// [RTO-PROBE] temporary: gate for the retransmission-storm measurement.
/// Set `GHOSTFRAME_RTO_PROBE=1` to emit one line per RTO fire and per
/// acknowledgement, for offline histogramming. Remove with the probe.
pub(crate) fn rto_probe_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("GHOSTFRAME_RTO_PROBE").is_ok_and(|v| v == "1"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn knob_invariants() {
        const {
            assert!(FEC_GROUP_SIZE_K >= 2);
            assert!(FEC_PARITY_PER_GROUP_R >= 1);
            assert!(PARITY_INTERLEAVE_OFFSET == 20);
            assert!(CACHE_CAPACITY.is_power_of_two() || CACHE_CAPACITY >= 1024);
        }
    }

    #[test]
    fn emit_key_hashable_and_ordered() {
        use std::collections::HashMap;
        let mut m = HashMap::new();
        let k1 = EmitKey {
            frame_seq: 1,
            tile_x: 2,
            tile_y: 3,
            pass_idx: 4,
        };
        let k2 = EmitKey {
            frame_seq: 1,
            tile_x: 2,
            tile_y: 3,
            pass_idx: 4,
        };
        let k3 = EmitKey {
            frame_seq: 1,
            tile_x: 2,
            tile_y: 3,
            pass_idx: 5,
        };
        m.insert(k1, "a");
        assert_eq!(m.get(&k2), Some(&"a"));
        assert!(!m.contains_key(&k3));
    }
}
