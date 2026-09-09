//! Network impairment simulator: deterministic loss, delay, corruption, and bandwidth modeling.
//!
//! This module provides a seeded, reproducible network simulator for e2e testing.
//! Every test carries a seed; every failure can be replayed exactly by rerunning
//! with the same seed.

pub mod profile;
pub mod rng;

pub use profile::{CapTimeline, NetProfile};
pub use rng::DetRng;

/// The fate of a single datagram as determined by the network simulator.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Deliver the datagram at the specified time `at_us` (microseconds).
    Deliver { at_us: u64 },

    /// Deliver the datagram at `at_us`, and send a duplicate at `dup_at_us`.
    /// Tasks 11-12 implement duplication and reordering.
    Duplicate { at_us: u64, dup_at_us: u64 },

    /// Deliver the datagram at `at_us` with bit `bit_index` flipped.
    /// Tasks 11-12 implement corruption.
    Corrupt { at_us: u64, bit_index: usize },

    /// Drop the datagram entirely.
    Drop,
}

/// The main network simulator: applies a NetProfile to each datagram,
/// tracking burst state, token bucket state, and reorder buffers.
pub struct NetSim {
    profile: NetProfile,
    rng: DetRng,

    // Gilbert-Elliott state.
    #[allow(dead_code)]
    in_burst: bool,

    // Token bucket state for bandwidth cap.
    #[allow(dead_code)]
    tokens: f64,
    #[allow(dead_code)]
    last_refill_us: u64,

    /// The seed used to construct this simulator; logged in test diagnostics.
    pub seed: u64,
}

impl NetSim {
    /// Construct a new simulator with a given profile and seed.
    pub fn new(profile: NetProfile, seed: u64) -> Self {
        Self {
            profile,
            rng: DetRng::new(seed),
            in_burst: false,
            tokens: 0.0,
            last_refill_us: 0,
            seed,
        }
    }

    /// Decide the fate of one datagram of `len` bytes offered at `now_us`.
    ///
    /// This task implements the loss roll and the immediate-delivery path.
    ///
    /// Tasks 11 and 12 extend this with:
    /// - Gilbert-Elliott burst state transitions and burst loss
    /// - Jitter (random delay within the profile window)
    /// - Reordering (buffering and out-of-order delivery)
    /// - Duplication (second delivery at random offset)
    /// - Corruption (single bit flip at random position)
    /// - Token bucket bandwidth cap (refill and consume tokens)
    pub fn decide(&mut self, _len: usize, now_us: u64) -> Verdict {
        // Roll independent loss.
        if self.rng.bernoulli(self.profile.loss) {
            return Verdict::Drop;
        }

        // Deliver immediately (Tasks 11-12 add delay/jitter).
        Verdict::Deliver { at_us: now_us }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_seeds_produce_identical_streams() {
        let mut a = DetRng::new(0xDEAD_BEEF);
        let mut b = DetRng::new(0xDEAD_BEEF);
        let xs: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();
        let ys: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();
        assert_eq!(xs, ys);

        let mut c = DetRng::new(0xDEAD_BEEE);
        let zs: Vec<u64> = (0..64).map(|_| c.next_u64()).collect();
        assert_ne!(xs, zs, "different seeds must diverge");
    }

    #[test]
    fn measured_loss_rate_matches_the_configuration() {
        let profile = NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        };
        let mut sim = NetSim::new(profile, 42);
        let n = 100_000;
        let dropped = (0..n)
            .filter(|_| matches!(sim.decide(64, 0), Verdict::Drop))
            .count();
        let rate = dropped as f64 / n as f64;
        assert!(
            (rate - 0.10).abs() < 0.005,
            "measured loss {rate:.4} must be within 0.5% of 0.10"
        );
    }
}
