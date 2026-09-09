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
