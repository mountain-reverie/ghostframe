//! Network impairment simulator: deterministic loss, delay, corruption, and bandwidth modeling.
//!
//! This module provides a seeded network simulator for e2e testing. Determinism
//! holds at the `NetSim`/`decide` layer only: for a fixed seed, the sequence of
//! rng draws and the verdict computed from each is exactly reproducible bit for
//! bit (see `fixed_seed_full_profile_is_bit_for_bit_reproducible` and its
//! `GOLDEN_DIGEST`), which is what the `NetSim` unit tests rely on.
//!
//! It does **not** hold for browserless scenes end to end. `run_browserless`
//! spawns `IoBridge::run` as a concurrent task on a `LocalSet`, and under
//! `tokio::time::pause()` the clock is virtual but *how far that task
//! progresses between the driver's polls* is decided by real (non-seeded) task
//! scheduling. That shifts the order and timing in which datagrams arrive at
//! the netsim from run to run, which changes which rng draws land on which
//! datagram even though the rng sequence itself is unchanged. Two identical
//! scenes (same seed, same binary, same process) have been observed to yield
//! different delivered/dropped byte counts, e.g. `(215794, 104743)` vs.
//! `(181462, 95299)` (measured 2026-09-10) — so a scene failure cannot be
//! replayed exactly by rerunning with the same seed.
//!
//! Consequences for scene assertions: prefer checking exact rendered pixels
//! (deterministic given what was actually delivered) over raw byte/datagram
//! counters, and when a byte-count assertion is unavoidable give it
//! order-of-magnitude margins rather than tight bounds.

pub mod profile;
pub mod pump;
pub mod rng;

pub use profile::{CapTimeline, NetProfile};
pub use pump::SocketPairPump;
pub use rng::DetRng;

/// The fate of a single datagram as determined by the network simulator.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Deliver the datagram at the specified time `at_us` (microseconds).
    Deliver { at_us: u64 },

    /// Deliver the datagram at `at_us`, and send a duplicate at `dup_at_us`.
    Duplicate { at_us: u64, dup_at_us: u64 },

    /// Deliver the datagram at `at_us` with bit `bit_index` flipped.
    Corrupt { at_us: u64, bit_index: usize },

    /// Drop the datagram entirely.
    Drop,
}

/// The main network simulator: applies a NetProfile to each datagram,
/// tracking burst state, token bucket state, and reorder buffers.
pub struct NetSim {
    profile: NetProfile,
    rng: DetRng,

    /// Gilbert-Elliott burst state: true while the link is in the bad
    /// (high-loss) state. Advanced once per `decide` call.
    in_burst: bool,

    /// Token-bucket state for the bandwidth cap: bytes currently available
    /// to spend, and the time of the last refill.
    tokens: f64,
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
    /// Applies, in order: Gilbert-Elliott burst-state advance, loss, delay +
    /// jitter, reorder, duplication, corruption, and finally the
    /// token-bucket bandwidth cap, which can convert any of the above
    /// outcomes into a drop.
    ///
    /// # RNG draw order
    ///
    /// The sequence of draws below is a fixed part of every seed's
    /// reproduction recipe: reordering, adding, or removing a draw changes
    /// the stream every later call sees, invalidating every previously
    /// recorded seed. Per call, in order:
    ///
    /// 1. Burst-state transition roll (always drawn).
    /// 2. Loss roll, using the just-updated burst state (always drawn; a
    ///    drop returns immediately, so no further draws happen this call).
    /// 3. Jitter fraction (always drawn, even when `jitter_us == 0` — a
    ///    profile field being zero must not shift what a later step draws).
    /// 4. Reorder fraction (always drawn, even when `reorder_us == 0`, for
    ///    the same reason).
    /// 5. Duplicate roll (always drawn).
    /// 6. Corrupt roll (always drawn).
    /// 7. Duplicate-offset draw, *only* if step 5 fired, or corrupt-bit-index
    ///    draw, *only* if step 6 fired (duplicate takes precedence if both
    ///    fire, since `Verdict` has no combined variant). These two are the
    ///    only outcome-dependent draws: their values are meaningless unless
    ///    the corresponding roll succeeded.
    ///
    /// The token-bucket bandwidth cap draws nothing: refill and spend are
    /// pure arithmetic on `tokens`/`last_refill_us`, so the cap is applied
    /// last, after every draw above, and a profile merely *having* a cap
    /// (or not) never shifts what any draw above produces.
    pub fn decide(&mut self, len: usize, now_us: u64) -> Verdict {
        // Token-bucket refill. No rng involved, so where this runs relative
        // to the draws below cannot affect the rng stream; it lives up front
        // for readability. An unlimited cap (bps == u64::MAX) short-circuits
        // entirely, skipping f64 arithmetic on an effectively-infinite rate
        // and keeping the uncapped path bit-identical to a build with no
        // bucket at all.
        let bps = self.profile.cap.bps_at(now_us);
        if bps != u64::MAX {
            let elapsed_s = now_us.saturating_sub(self.last_refill_us) as f64 / 1_000_000.0;
            let burst_capacity = bps as f64 / 10.0; // 100 ms of buffering
            self.tokens = (self.tokens + elapsed_s * bps as f64).min(burst_capacity);
        }
        self.last_refill_us = now_us;

        // 1. Advance Gilbert-Elliott burst state.
        if self.in_burst {
            if self.rng.bernoulli(self.profile.burst_exit) {
                self.in_burst = false;
            }
        } else if self.rng.bernoulli(self.profile.burst_enter) {
            self.in_burst = true;
        }

        // 2. Loss roll, using the updated burst state.
        let loss_p = if self.in_burst {
            self.profile.burst_loss
        } else {
            self.profile.loss
        };
        if self.rng.bernoulli(loss_p) {
            // Chosen: a loss-dropped datagram does NOT spend bucket tokens.
            // It models a link-level error (bad radio, corrupted frame)
            // rather than congestion at our own shaper, so it never reached
            // the point where it would have occupied capped capacity.
            return Verdict::Drop;
        }

        // 3. Delay + jitter: at_us = now_us + delay_us + jitter, jitter
        // uniform in [-jitter_us, +jitter_us], clamped so at_us >= now_us.
        let jitter_frac = self.rng.next_f64(); // [0, 1)
        let jitter = ((jitter_frac * 2.0 - 1.0) * self.profile.jitter_us as f64).round() as i64;
        let delayed = now_us as i128 + self.profile.delay_us as i128 + jitter as i128;
        let mut at_us = delayed.max(now_us as i128) as u64;

        // 4. Reorder: add a uniform [0, reorder_us) offset, letting a later
        // datagram overtake an earlier one.
        let reorder_frac = self.rng.next_f64(); // [0, 1)
        let reorder_offset = (reorder_frac * self.profile.reorder_us as f64) as u64;
        at_us += reorder_offset;

        // 5 & 6. Duplicate / corrupt rolls (both always drawn).
        let want_duplicate = self.rng.bernoulli(self.profile.duplicate);
        let want_corrupt = self.rng.bernoulli(self.profile.corrupt);

        let verdict = if want_duplicate {
            // 7. Duplicate offset: the duplicate arrives at or after the
            // original, within a small independent window.
            let dup_frac = self.rng.next_f64();
            let dup_window_us = self.profile.jitter_us.max(1_000) as f64;
            let dup_offset = (dup_frac * dup_window_us) as u64;
            Verdict::Duplicate {
                at_us,
                dup_at_us: at_us + dup_offset,
            }
        } else if want_corrupt {
            // 7. Corrupt bit index, uniform over the datagram's bits.
            let bit_frac = self.rng.next_f64();
            let bit_count = len.saturating_mul(8);
            let bit_index =
                ((bit_frac * bit_count as f64) as usize).min(bit_count.saturating_sub(1));
            Verdict::Corrupt { at_us, bit_index }
        } else {
            Verdict::Deliver { at_us }
        };

        // Token-bucket bandwidth cap: applied last, after every rng draw
        // above, so the cap's presence never shifts the draw sequence (see
        // "RNG draw order"). An unlimited cap lets every verdict through
        // unchanged.
        if bps == u64::MAX {
            return verdict;
        }
        match verdict {
            Verdict::Duplicate { at_us, dup_at_us } => {
                let full_cost = 2.0 * len as f64;
                if self.tokens >= full_cost {
                    self.tokens -= full_cost;
                    Verdict::Duplicate { at_us, dup_at_us }
                } else if self.tokens >= len as f64 {
                    // Can't afford both copies but can afford one: downgrade
                    // to a single delivery rather than dropping a datagram
                    // we could otherwise have sent.
                    self.tokens -= len as f64;
                    Verdict::Deliver { at_us }
                } else {
                    Verdict::Drop
                }
            }
            other => {
                let cost = len as f64;
                if self.tokens >= cost {
                    self.tokens -= cost;
                    other
                } else {
                    Verdict::Drop
                }
            }
        }
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
