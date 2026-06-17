//! Reliable Tile Emitter — see docs/superpowers/specs/2026-06-17-reliable-tile-emitter-design.md

pub mod traits;

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
}
