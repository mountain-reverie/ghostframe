//! Deterministic loss injection for testing — feature-gated behind
//! `test-loss-injection` and `cfg(test)`. Compiled out in release builds.
//!
//! Used by E2E tests to drop a known fraction of datagrams matching a
//! predicate (e.g. all tile datagrams, or only ACK_BATCH datagrams).

/// A deterministic packet-drop predicate keyed on the datagram contents
/// (typically the first byte) so e2e tests can drop only specific message
/// types without disturbing the rest of the data path.
pub type DropPredicate = fn(&[u8]) -> bool;

/// Splitmix64-style PRNG. Tiny, deterministic, zero dependencies.
struct SplitMix {
    state: u64,
}

impl SplitMix {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

pub struct LossInjector {
    drop_probability: f32,
    predicate: DropPredicate,
    rng: SplitMix,
}

impl LossInjector {
    pub fn new(drop_probability: f32, predicate: DropPredicate, seed: u64) -> Self {
        Self {
            drop_probability,
            predicate,
            rng: SplitMix::new(seed),
        }
    }

    pub fn should_drop(&mut self, datagram: &[u8]) -> bool {
        if !(self.predicate)(datagram) {
            return false;
        }
        self.rng.next_f32() < self.drop_probability
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_rng_drops_deterministically() {
        let mut a = LossInjector::new(0.5, |_| true, 42);
        let mut b = LossInjector::new(0.5, |_| true, 42);
        for _ in 0..20 {
            assert_eq!(a.should_drop(&[0x01]), b.should_drop(&[0x01]));
        }
    }

    #[test]
    fn predicate_filters_drops() {
        // Drop only datagrams whose first byte is 0x02 (ACKs).
        let mut inj = LossInjector::new(1.0, |dg| dg.first().copied() == Some(0x02), 0);
        assert!(inj.should_drop(&[0x02, 0, 0]));
        assert!(!inj.should_drop(&[0x01, 0, 0]));
    }

    #[test]
    fn zero_probability_never_drops() {
        let mut inj = LossInjector::new(0.0, |_| true, 0);
        for _ in 0..100 {
            assert!(!inj.should_drop(&[0xAA]));
        }
    }

    #[test]
    fn one_probability_drops_when_predicate_matches() {
        let mut inj = LossInjector::new(1.0, |_| true, 0);
        for _ in 0..100 {
            assert!(inj.should_drop(&[0xAA]));
        }
    }

    #[test]
    fn drops_at_approximately_configured_rate() {
        // Statistical sanity: drop probability 0.5 with 1000 trials should land near 500.
        let mut inj = LossInjector::new(0.5, |_| true, 12345);
        let drops = (0..1000).filter(|_| inj.should_drop(&[0xAA])).count();
        // Generous ±10% bound — splitmix64 is a fast PRNG, not cryptographic.
        assert!((400..=600).contains(&drops), "drops={drops} out of 1000");
    }
}
