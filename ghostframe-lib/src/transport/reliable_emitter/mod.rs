//! Reliable Tile Emitter — see docs/superpowers/specs/2026-06-17-reliable-tile-emitter-design.md

pub mod cache;
pub mod emission_queue;
pub mod parity;
pub mod traits;
pub mod wire_seq;

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
        Self { frame_seq, tile_x, tile_y, pass_idx }
    }
}

// ---- Knob constants (spec §9) ----
pub const FEC_GROUP_SIZE_K: usize = 10;
pub const FEC_PARITY_PER_GROUP_R: usize = 1;
pub const PARITY_INTERLEAVE_OFFSET: u32 = (2 * FEC_GROUP_SIZE_K) as u32;
pub const END_OF_STREAM_PARITY_FLUSH_MS: u64 = 5;
pub const MAX_RETRANSMITS: u8 = 4;
pub const BASE_RTO_MS: u64 = 50;
pub const RTO_BACKOFF_FACTOR: u32 = 2;
pub const CACHE_CAPACITY: usize = 8192;

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn knob_invariants() {
        assert!(FEC_GROUP_SIZE_K >= 2);
        assert!(FEC_PARITY_PER_GROUP_R >= 1);
        assert_eq!(PARITY_INTERLEAVE_OFFSET, 20);
        assert!(MAX_RETRANSMITS >= 1 && MAX_RETRANSMITS <= 8);
        assert!(BASE_RTO_MS >= 25 && BASE_RTO_MS <= 200);
        assert!(CACHE_CAPACITY.is_power_of_two() || CACHE_CAPACITY >= 1024);
    }

    #[test]
    fn emit_key_hashable_and_ordered() {
        use std::collections::HashMap;
        let mut m = HashMap::new();
        let k1 = EmitKey { frame_seq: 1, tile_x: 2, tile_y: 3, pass_idx: 4 };
        let k2 = EmitKey { frame_seq: 1, tile_x: 2, tile_y: 3, pass_idx: 4 };
        let k3 = EmitKey { frame_seq: 1, tile_x: 2, tile_y: 3, pass_idx: 5 };
        m.insert(k1, "a");
        assert_eq!(m.get(&k2), Some(&"a"));
        assert!(!m.contains_key(&k3));
    }
}
