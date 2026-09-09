//! Deterministic random number generation for reproducible test impairment.
//!
//! This uses SplitMix64, a splitmix-based PRNG designed to produce bit-independent output
//! from a 64-bit seed. It is fast, has excellent statistical properties, and critically,
//! the algorithm is fixed and named: a seed is a complete reproduction recipe for a test
//! run, and the generator will not change (so seeds will not become stale or unstable).
//!
//! See https://xoshiro.di.unimi.it/splitmix64.c — this is the reference implementation.

/// A deterministic, seeded random number generator using SplitMix64.
#[derive(Debug, Clone)]
pub struct DetRng(u64);

impl DetRng {
    /// Construct a new generator from a 64-bit seed.
    pub fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Draw a uniformly random u64.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Draw a uniformly random f64 in [0, 1).
    pub fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Draw a Bernoulli(p) sample: true with probability p.
    pub fn bernoulli(&mut self, p: f64) -> bool {
        self.next_f64() < p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_deterministic() {
        let mut a = DetRng::new(0xDEAD_BEEF);
        let xs: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();

        let mut b = DetRng::new(0xDEAD_BEEF);
        let ys: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();

        assert_eq!(xs, ys, "identical seeds must produce identical streams");
    }

    #[test]
    fn rng_diverges_on_different_seeds() {
        let mut a = DetRng::new(0xDEAD_BEEF);
        let xs: Vec<u64> = (0..64).map(|_| a.next_u64()).collect();

        let mut b = DetRng::new(0xDEAD_BEEE);
        let ys: Vec<u64> = (0..64).map(|_| b.next_u64()).collect();

        assert_ne!(xs, ys, "different seeds must diverge");
    }

    #[test]
    fn f64_is_in_range() {
        let mut rng = DetRng::new(12345);
        for _ in 0..1000 {
            let x = rng.next_f64();
            assert!((0.0..1.0).contains(&x));
        }
    }

    #[test]
    fn bernoulli_matches_probability() {
        let mut rng = DetRng::new(42);
        let n = 10_000;
        let count = (0..n).filter(|_| rng.bernoulli(0.5)).count();
        let rate = count as f64 / n as f64;
        // Expect approximately 0.5; allow ±0.02 (20 standard errors at p=0.5)
        assert!(
            (rate - 0.5).abs() < 0.02,
            "bernoulli(0.5) rate {rate:.4} must be near 0.5"
        );
    }
}
