//! Tests for the netsim module: RNG determinism and loss rate accuracy.

use ghostframe_e2e::netsim::{DetRng, NetProfile, NetSim, Verdict};

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

#[test]
fn burst_loss_clusters_drops() {
    let profile = NetProfile {
        burst_enter: 0.01,
        burst_exit: 0.20,
        burst_loss: 0.90,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 7);
    let drops: Vec<bool> = (0..20_000)
        .map(|_| matches!(sim.decide(64, 0), Verdict::Drop))
        .collect();

    // A clustered process has a much higher P(drop | previous dropped) than
    // its unconditional drop rate.
    let total = drops.iter().filter(|d| **d).count() as f64;
    let pairs = drops.windows(2).filter(|w| w[0] && w[1]).count() as f64;
    let p_uncond = total / drops.len() as f64;
    let p_cond = pairs / total;
    assert!(
        p_cond > p_uncond * 3.0,
        "burst loss must cluster: P(drop|drop)={p_cond:.3} vs P(drop)={p_uncond:.3}"
    );
}

#[test]
fn delay_and_jitter_stay_within_bounds() {
    let profile = NetProfile {
        delay_us: 20_000,
        jitter_us: 5_000,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 3);
    for _ in 0..1_000 {
        match sim.decide(64, 100_000) {
            Verdict::Deliver { at_us } => {
                assert!((115_000..=125_000).contains(&at_us), "at_us={at_us}");
            }
            other => panic!("expected Deliver, got {other:?}"),
        }
    }
}

#[test]
fn duplication_fires_at_roughly_the_configured_rate_and_dup_at_us_is_ordered() {
    let profile = NetProfile {
        duplicate: 0.20,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 11);
    let n = 50_000;
    let mut dup_count = 0usize;
    for _ in 0..n {
        match sim.decide(64, 1_000) {
            Verdict::Duplicate { at_us, dup_at_us } => {
                dup_count += 1;
                assert!(
                    dup_at_us >= at_us,
                    "dup_at_us ({dup_at_us}) must be >= at_us ({at_us})"
                );
            }
            Verdict::Deliver { .. } => {}
            other => panic!("expected Deliver or Duplicate, got {other:?}"),
        }
    }
    let rate = dup_count as f64 / n as f64;
    assert!(
        (rate - 0.20).abs() < 0.01,
        "measured duplication rate {rate:.4} must be within 1% of 0.20"
    );
}

#[test]
fn corruption_fires_at_roughly_the_configured_rate_and_bit_index_in_range() {
    let profile = NetProfile {
        corrupt: 0.20,
        ..NetProfile::perfect()
    };
    let mut sim = NetSim::new(profile, 13);
    let n = 50_000;
    let len = 64usize;
    let mut corrupt_count = 0usize;
    for _ in 0..n {
        match sim.decide(len, 1_000) {
            Verdict::Corrupt { bit_index, .. } => {
                corrupt_count += 1;
                assert!(
                    bit_index < len * 8,
                    "bit_index ({bit_index}) must be < len*8 ({})",
                    len * 8
                );
            }
            Verdict::Deliver { .. } => {}
            other => panic!("expected Deliver or Corrupt, got {other:?}"),
        }
    }
    let rate = corrupt_count as f64 / n as f64;
    assert!(
        (rate - 0.20).abs() < 0.01,
        "measured corruption rate {rate:.4} must be within 1% of 0.20"
    );
}

#[test]
fn fixed_seed_full_profile_is_bit_for_bit_reproducible() {
    // Regression test: this is what protects every recorded seed in every
    // future failure report. If the rng draw order ever changes, this test
    // will still pass (it compares two fresh runs against each other, not
    // against a hardcoded golden value) — its job is only to catch runs
    // that are non-deterministic given identical inputs.
    fn run() -> Vec<Verdict> {
        let profile = NetProfile {
            loss: 0.05,
            burst_enter: 0.02,
            burst_exit: 0.30,
            burst_loss: 0.50,
            duplicate: 0.05,
            corrupt: 0.05,
            delay_us: 10_000,
            jitter_us: 2_000,
            reorder_us: 500,
            ..NetProfile::perfect()
        };
        let mut sim = NetSim::new(profile, 1234);
        (0..500)
            .map(|i| sim.decide(64 + (i % 200), 1_000 * i as u64))
            .collect()
    }

    let a = run();
    let b = run();
    assert_eq!(
        a, b,
        "identical seed + inputs must produce identical Verdicts"
    );

    // ...and the same values it produced when this constant was recorded.
    //
    // The comparison above cannot fail: `decide` is a pure function of
    // (seed, profile, inputs), so two runs in one build always agree even if
    // the algorithm changed. Only a golden catches the failure that actually
    // matters — swapping SplitMix64 for something else, or reordering the rng
    // draws — either of which silently invalidates every seed recorded in
    // every past failure report while every other test stays green.
    //
    // If this fails, do not re-record the constant to make it pass unless the
    // change to the generator or the draw order was deliberate. Re-recording
    // is declaring every previously-reported seed unreproducible.
    assert_eq!(
        digest(&a),
        GOLDEN_DIGEST,
        "netsim verdict stream changed for a fixed seed — the rng algorithm \
         or the draw order moved, which invalidates recorded seeds"
    );
}

/// Recorded from the sequence above. See the note in the test before changing.
const GOLDEN_DIGEST: u64 = 7_099_419_336_748_855_738;

/// Order-sensitive fold over the verdict stream. Deliberately hand-rolled and
/// stable: a `DefaultHasher` is explicitly not guaranteed stable across Rust
/// releases, which would turn this golden into a spurious failure on a
/// toolchain bump.
fn digest(verdicts: &[Verdict]) -> u64 {
    let mut acc: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        acc ^= v;
        acc = acc.wrapping_mul(0x1000_0000_01b3);
    };
    for v in verdicts {
        match v {
            Verdict::Drop => mix(1),
            Verdict::Deliver { at_us } => {
                mix(2);
                mix(*at_us);
            }
            Verdict::Duplicate { at_us, dup_at_us } => {
                mix(3);
                mix(*at_us);
                mix(*dup_at_us);
            }
            Verdict::Corrupt { at_us, bit_index } => {
                mix(4);
                mix(*at_us);
                mix(*bit_index as u64);
            }
        }
    }
    acc
}
