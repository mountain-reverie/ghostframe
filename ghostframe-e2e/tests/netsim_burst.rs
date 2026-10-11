//! Statistical checks on the Gilbert-Elliott model in `netsim::burst`.
//!
//! Everything here drives `NetSim::decide` directly, which *is* seed-
//! deterministic (unlike browserless scenes end to end), so the assertions
//! are exact reproductions, not flaky statistics. Tolerances are still
//! chosen as if they were statistical (several standard errors) so a changed
//! rng draw order does not make them fail by luck.

use ghostframe_e2e::netsim::{
    BurstConfig, BurstDirection, GilbertElliott, NetProfile, NetSim, Verdict,
};
use std::collections::HashMap;

const N: usize = 2_000_000;

/// Drop mask for `n` datagrams through a profile.
fn mask(profile: NetProfile, seed: u64, n: usize) -> Vec<bool> {
    let mut sim = NetSim::new(profile, seed);
    (0..n)
        .map(|i| matches!(sim.decide(1200, i as u64 * 100), Verdict::Drop))
        .collect()
}

fn ge_profile(avg: f64, len: f64, bad: f64) -> NetProfile {
    let mut p = NetProfile::perfect();
    GilbertElliott::from_rate_and_burst(avg, len, bad)
        .expect("feasible")
        .apply_to(&mut p);
    p
}

/// (loss rate, mean loss-run length, number of runs)
fn stats(m: &[bool]) -> (f64, f64, usize) {
    let dropped = m.iter().filter(|d| **d).count();
    let mut runs = 0usize;
    let mut prev = false;
    for &d in m {
        if d && !prev {
            runs += 1;
        }
        prev = d;
    }
    let mean = if runs == 0 {
        0.0
    } else {
        dropped as f64 / runs as f64
    };
    (dropped as f64 / m.len() as f64, mean, runs)
}

#[test]
fn long_run_loss_rate_matches_configured_average() {
    for &(avg, len) in &[
        (0.01, 4.0),
        (0.02, 8.0),
        (0.05, 12.0),
        (0.10, 20.0),
        (0.03, 1.0),
    ] {
        let (rate, _, runs) = stats(&mask(ge_profile(avg, len, 1.0), 7, N));
        assert!(runs > 1000, "premise: only {runs} loss runs observed");
        assert!(
            (rate - avg).abs() < avg * 0.04,
            "avg {avg} len {len}: measured loss {rate:.5}"
        );
    }
}

#[test]
fn partial_loss_in_bad_state_still_hits_the_average() {
    // loss_bad < 1: the average is still the target, but loss runs are
    // shorter than the bad sojourn, so only the rate is asserted.
    let (rate, _, runs) = stats(&mask(ge_profile(0.02, 10.0, 0.5), 11, N));
    assert!(runs > 1000);
    assert!((rate - 0.02).abs() < 0.02 * 0.08, "measured {rate:.5}");
}

#[test]
fn mean_loss_run_length_matches_configured_burst() {
    for &(avg, len) in &[
        (0.05, 1.0),
        (0.05, 4.0),
        (0.05, 12.0),
        (0.02, 8.0),
        (0.1, 20.0),
    ] {
        let (_, mean, runs) = stats(&mask(ge_profile(avg, len, 1.0), 3, N));
        assert!(runs > 1000, "premise: only {runs} runs");
        assert!(
            (mean - len).abs() < len * 0.06,
            "avg {avg}: wanted mean run {len}, measured {mean:.3} over {runs} runs"
        );
    }
}

#[test]
fn same_average_different_burst_length_moves_the_measured_run_length() {
    let (r1, m1, _) = stats(&mask(ge_profile(0.05, 2.0, 1.0), 5, N));
    let (r2, m2, _) = stats(&mask(ge_profile(0.05, 16.0, 1.0), 5, N));
    assert!((r1 - r2).abs() < 0.005, "premise: rates {r1} vs {r2} equal");
    assert!(
        m2 > m1 * 4.0,
        "measurement must move with the knob: {m1:.2} vs {m2:.2}"
    );
}

#[test]
fn iid_run_length_is_short_compared_to_burst_at_equal_rate() {
    let iid = NetProfile {
        loss: 0.05,
        ..NetProfile::perfect()
    };
    let (_, m_iid, _) = stats(&mask(iid, 9, N));
    let (_, m_b, _) = stats(&mask(ge_profile(0.05, 8.0, 1.0), 9, N));
    // i.i.d. mean run is 1 / (1 - 0.05) = 1.0526.
    assert!((m_iid - 1.0526).abs() < 0.02, "iid mean run {m_iid}");
    assert!(m_b > 7.0, "burst mean run {m_b}");
}

#[test]
fn same_seed_same_sequence_different_seed_differs() {
    let p = ge_profile(0.05, 8.0, 1.0);
    let a = mask(p.clone(), 42, 50_000);
    let b = mask(p.clone(), 42, 50_000);
    let c = mask(p, 43, 50_000);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert!(a.iter().any(|d| *d), "premise: some drops");
}

#[test]
fn analytic_expectation_agrees_with_construction() {
    let m = GilbertElliott::from_rate_and_burst(0.04, 6.0, 0.8).unwrap();
    assert!((m.expected_loss() - 0.04).abs() < 1e-12);
    assert!((1.0 / m.p_bg - 6.0).abs() < 1e-12);
}

#[test]
fn infeasible_inputs_are_rejected() {
    let f = GilbertElliott::from_rate_and_burst;
    assert!(f(0.0, 4.0, 1.0).is_err());
    assert!(f(1.0, 4.0, 1.0).is_err());
    assert!(f(f64::NAN, 4.0, 1.0).is_err());
    assert!(f(0.05, 0.5, 1.0).is_err(), "burst < 1");
    assert!(f(0.6, 4.0, 0.5).is_err(), "avg > loss_bad");
    assert!(f(0.5, 4.0, 0.5).is_err(), "avg == loss_bad");
    assert!(f(0.05, 4.0, 0.0).is_err());
    assert!(f(0.05, 4.0, 1.5).is_err());
    // pi_B = 0.9, r = 1 => p = 9 > 1: bursts too short for that much loss.
    assert!(f(0.9, 1.0, 1.0).is_err(), "p_gb > 1");
    assert!(f(0.05, 4.0, 1.0).is_ok());
}

fn lookup(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let m: HashMap<String, String> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |k| m.get(k).cloned()
}

#[test]
fn env_unset_means_none() {
    for d in [BurstDirection::Outbound, BurstDirection::Inbound] {
        assert_eq!(BurstConfig::from_lookup(d, lookup(&[])).unwrap(), None);
    }
    // The other direction's variables must not leak across.
    let g = lookup(&[
        ("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02"),
        ("GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN", "8"),
    ]);
    assert_eq!(
        BurstConfig::from_lookup(BurstDirection::Inbound, &g).unwrap(),
        None
    );
    assert!(BurstConfig::from_lookup(BurstDirection::Outbound, &g)
        .unwrap()
        .is_some());
}

#[test]
fn env_parses_values_defaults_and_seed() {
    let g = lookup(&[
        ("GHOSTFRAME_INBOUND_BURST_LOSS_PROBABILITY", "0.02"),
        ("GHOSTFRAME_INBOUND_BURST_MEAN_LEN", "8"),
        ("GHOSTFRAME_INBOUND_BURST_SEED", "99"),
    ]);
    let c = BurstConfig::from_lookup(BurstDirection::Inbound, g)
        .unwrap()
        .unwrap();
    assert_eq!(c.seed, 99);
    assert_eq!(c.model.loss_bad, 1.0);
    assert!((c.model.expected_loss() - 0.02).abs() < 1e-12);
    assert!((1.0 / c.model.p_bg - 8.0).abs() < 1e-12);

    let g = lookup(&[
        ("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02"),
        ("GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN", "4"),
        ("GHOSTFRAME_OUTBOUND_BURST_LOSS_BAD", "0.5"),
    ]);
    let c = BurstConfig::from_lookup(BurstDirection::Outbound, g)
        .unwrap()
        .unwrap();
    assert_eq!(c.seed, 0);
    assert_eq!(c.model.loss_bad, 0.5);
}

#[test]
fn env_rejects_bad_values_instead_of_silently_disabling() {
    let base = |extra: &[(&'static str, &'static str)]| {
        let mut v = vec![
            ("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02"),
            ("GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN", "8"),
        ];
        for (k, val) in extra {
            v.retain(|(kk, _)| kk != k);
            v.push((k, val));
        }
        BurstConfig::from_lookup(BurstDirection::Outbound, lookup(&v))
    };
    assert!(base(&[]).is_ok());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "abc")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "1.5")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN", "0.5")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN", "x")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_BAD", "0.01")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_SEED", "-1")]).is_err());
    // Probability without a length is a configuration error, not "off".
    let only_p = lookup(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02")]);
    assert!(BurstConfig::from_lookup(BurstDirection::Outbound, only_p).is_err());
    // Sibling variables without the probability must not silently disable it.
    for k in [
        "GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN",
        "GHOSTFRAME_OUTBOUND_BURST_SEED",
        "GHOSTFRAME_OUTBOUND_BURST_LOSS_BAD",
    ] {
        let g = lookup(&[(k, "4")]);
        assert!(
            BurstConfig::from_lookup(BurstDirection::Outbound, g).is_err(),
            "{k} alone must be an error"
        );
    }
}
