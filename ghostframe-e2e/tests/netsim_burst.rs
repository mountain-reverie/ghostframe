//! Statistical checks on the Gilbert-Elliott model in `netsim::burst`.
//!
//! Everything here drives `NetSim::decide` directly, which *is* seed-
//! deterministic (unlike browserless scenes end to end), so the assertions
//! are exact reproductions, not flaky statistics. Tolerances are still
//! chosen as if they were statistical (several standard errors) so a changed
//! rng draw order does not make them fail by luck.

use ghostframe_e2e::netsim::{
    BurstConfig, BurstDirection, GilbertElliott, NetProfile, NetSim, Verdict, BURST_QUANTUM_US,
    REFERENCE_DATAGRAM_INTERVAL_US,
};
use std::collections::HashMap;

const N: usize = 2_000_000;

const Q: u64 = BURST_QUANTUM_US;
const REF: u64 = REFERENCE_DATAGRAM_INTERVAL_US;

/// Drop mask for `n` datagrams through a profile, one every `spacing_us`.
fn mask_at(profile: NetProfile, seed: u64, n: usize, spacing_us: u64) -> Vec<bool> {
    let mut sim = NetSim::new(profile, seed);
    (0..n)
        .map(|i| matches!(sim.decide(1200, i as u64 * spacing_us), Verdict::Drop))
        .collect()
}

/// At the reference send rate, which is what `len` is denominated in.
fn mask(profile: NetProfile, seed: u64, n: usize) -> Vec<bool> {
    mask_at(profile, seed, n, REF)
}

/// Time-based model (the default).
fn ge_profile(avg: f64, len: f64, bad: f64) -> NetProfile {
    let mut p = NetProfile::perfect();
    GilbertElliott::from_rate_and_burst(avg, len, bad)
        .expect("feasible")
        .apply_to(&mut p);
    p
}

/// The old per-datagram chain.
fn ge_profile_per_datagram(avg: f64, len: f64, bad: f64) -> NetProfile {
    let mut p = NetProfile::perfect();
    GilbertElliott::from_rate_and_burst_per_datagram(avg, len, bad)
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
fn mean_burst_duration_matches_configured_burst_in_time() {
    // Sampled once per quantum, a loss run of k datagrams is k quanta of
    // burst, so mean run * quantum is the mean burst duration, which the
    // configuration states as `len` reference intervals.
    for &(avg, len) in &[
        (0.05, 1.0),
        (0.05, 4.0),
        (0.05, 12.0),
        (0.02, 8.0),
        (0.1, 20.0),
    ] {
        let (_, mean, runs) = stats(&mask_at(ge_profile(avg, len, 1.0), 3, 6 * N, Q));
        assert!(runs > 1000, "premise: only {runs} runs");
        let want_us = len * REF as f64;
        let got_us = mean * Q as f64;
        assert!(
            (got_us - want_us).abs() < want_us * 0.06,
            "avg {avg}: wanted a {want_us} us burst, measured {got_us:.0} us over {runs} runs"
        );
    }
}

#[test]
fn per_datagram_model_still_has_its_mean_run_in_datagrams() {
    // The legacy chain, kept for fec_burst_analysis: run length is in
    // datagrams, with no time scale.
    for &(avg, len) in &[(0.05, 1.0), (0.05, 12.0), (0.1, 20.0)] {
        let (_, mean, runs) = stats(&mask(ge_profile_per_datagram(avg, len, 1.0), 3, N));
        assert!(runs > 1000);
        assert!(
            (mean - len).abs() < len * 0.06,
            "wanted mean run {len}, measured {mean:.3}"
        );
    }
}

/// The property the per-datagram chain got wrong. Hold the link and the
/// configured burst (8 reference intervals = 8 ms) fixed and change only how
/// often the sender sends.
///
/// * The long-run loss fraction is the configured average at every rate.
/// * Sending every 100 us (80x denser than the burst is long) a loss run is
///   about 80 datagrams; sending every 200 ms (25x longer than the burst)
///   consecutive datagrams are independent samples of the chain, so the mean
///   run is that of i.i.d. loss, 1 / (1 - avg), not 8 and not 80. A chain
///   that steps per datagram gives ~8 at both.
#[test]
fn burst_is_a_duration_not_a_datagram_count() {
    let avg = 0.05;
    let p = ge_profile(avg, 8.0, 1.0);
    let n = 4_000_000;
    let (rate_dense, run_dense, runs_d) = stats(&mask_at(p.clone(), 21, n, Q));
    let (rate_ref, run_ref, runs_r) = stats(&mask_at(p.clone(), 21, n, REF));
    let (rate_sparse, run_sparse, runs_s) = stats(&mask_at(p, 21, n, 200_000));
    assert!(runs_d > 500 && runs_r > 500 && runs_s > 500, "premise");
    for (name, r) in [
        ("dense", rate_dense),
        ("ref", rate_ref),
        ("sparse", rate_sparse),
    ] {
        assert!(
            (r - avg).abs() < avg * 0.08,
            "{name}: loss fraction {r:.4} vs {avg}"
        );
    }
    assert!(run_dense > 60.0, "dense mean run {run_dense:.1}");
    assert!(
        (6.0..12.0).contains(&run_ref),
        "reference-rate mean run {run_ref:.2}"
    );
    let iid_run = 1.0 / (1.0 - avg);
    assert!(
        (run_sparse - iid_run).abs() < 0.1,
        "sparse mean run {run_sparse:.3}: consecutive datagrams 200 ms apart must be \
         independent (i.i.d. {iid_run:.3}), not one burst"
    );
}

/// P(lost | previous datagram lost, `gap` of virtual time earlier) must be
/// the chain's own n-step law, pi + (1 - pi) l^n, with n = gap / quantum.
/// This pins the closed-form advance against the step-by-step definition.
#[test]
fn conditional_loss_follows_the_n_step_law_of_the_chain() {
    let (avg, len) = (0.05, 8.0);
    let m = GilbertElliott::from_rate_and_burst(avg, len, 1.0).unwrap();
    let pi = m.stationary_bad();
    let l = 1.0 - m.p_gb - m.p_bg;
    let p = ge_profile(avg, len, 1.0);
    for gap_us in [Q, 3 * Q, 10 * Q, 40 * Q, 120 * Q, 1_000 * Q] {
        let mk = mask_at(p.clone(), 17, N, gap_us);
        let (mut after_loss, mut both) = (0usize, 0usize);
        for w in mk.windows(2) {
            if w[0] {
                after_loss += 1;
                both += usize::from(w[1]);
            }
        }
        assert!(after_loss > 20_000, "premise: {after_loss} losses");
        let got = both as f64 / after_loss as f64;
        let want = pi + (1.0 - pi) * l.powi((gap_us / Q) as i32);
        assert!(
            (got - want).abs() < 0.02,
            "gap {gap_us} us: P(loss | loss) {got:.4}, chain says {want:.4}"
        );
    }
}

/// The time-based branch must consume exactly one rng draw per call, like the
/// per-datagram one, so that every later draw (jitter here) is where it was.
/// With no loss in either state the burst state cannot change a verdict, so
/// the Deliver times of a time-based profile and of `perfect()` plus the same
/// jitter must be identical; an extra (or missing) draw in the time branch
/// shifts the jitter stream and breaks it.
#[test]
fn the_time_branch_draws_exactly_once_per_call() {
    let mut time_p = NetProfile::perfect();
    time_p.burst_enter = 0.3;
    time_p.burst_exit = 0.2;
    time_p.burst_quantum_us = Q;
    time_p.burst_loss = 0.0;
    time_p.jitter_us = 800;
    let mut base = NetProfile::perfect();
    base.jitter_us = 800;
    // Same-quantum repeats (n = 0), small and huge gaps.
    let times: Vec<u64> = (0..5_000u64)
        .map(|i| [0, 0, 50, 100, 1_000, 123_456, 9_000_000_000][i as usize % 7] + i * 3 * Q)
        .collect();
    let run = |p: &NetProfile| {
        let mut sim = NetSim::new(p.clone(), 77);
        times
            .iter()
            .map(|&t| sim.decide(1200, t))
            .collect::<Vec<_>>()
    };
    let (a, b) = (run(&time_p), run(&base));
    assert!(
        a.iter().all(|v| matches!(v, Verdict::Deliver { .. })),
        "premise: no drops"
    );
    assert!(
        a.windows(2).any(|w| w[0] != w[1]),
        "premise: jitter actually varies"
    );
    let first = a.iter().zip(&b).position(|(x, y)| x != y);
    assert!(
        first.is_none(),
        "jitter stream shifted at call {first:?}: the time branch drew a different number of times"
    );
}

#[test]
fn irregular_gaps_are_deterministic() {
    // Same seed, same offered times => same verdicts; and the profile
    // being time-based must not change how many draws a call consumes, so
    // a Deliver's jitter after a long gap matches a run with the same gap
    // pattern (determinism), not a shifted stream.
    let mut p = ge_profile(0.05, 8.0, 1.0);
    p.jitter_us = 500;
    let times: Vec<u64> = (0..20_000u64)
        .map(|i| i * i % 7_919 * 137 + i * 90)
        .collect();
    let run = |seed| {
        let mut sim = NetSim::new(p.clone(), seed);
        let mut t = 0;
        times
            .iter()
            .map(|d| {
                t += d;
                sim.decide(1200, t)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(run(5), run(5));
    assert_ne!(run(5), run(6));
    assert!(run(5).iter().any(|v| matches!(v, Verdict::Drop)), "premise");
}

#[test]
fn iid_run_length_is_short_compared_to_burst_at_equal_rate() {
    let iid = NetProfile {
        loss: 0.05,
        ..NetProfile::perfect()
    };
    let (_, m_iid, _) = stats(&mask(iid, 9, N));
    let (_, m_b, _) = stats(&mask_at(ge_profile(0.05, 8.0, 1.0), 9, N, Q));
    // i.i.d. mean run is 1 / (1 - 0.05) = 1.0526.
    assert!((m_iid - 1.0526).abs() < 0.02, "iid mean run {m_iid}");
    assert!(m_b > 70.0, "burst mean run {m_b}");
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
    assert!((m.mean_burst_us().unwrap() - 6.0 * REF as f64).abs() < 1e-6);
    let d = GilbertElliott::from_rate_and_burst_per_datagram(0.04, 6.0, 0.8).unwrap();
    assert!((d.expected_loss() - 0.04).abs() < 1e-12);
    assert!((1.0 / d.p_bg - 6.0).abs() < 1e-12);
    assert_eq!(d.mean_burst_us(), None);
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
    // Per datagram: pi_B = 0.9, r = 1 => p = 9 > 1. In time: r = 1/10 per
    // quantum, pi_B = 0.95 => p = 1.9 > 1.
    assert!(
        GilbertElliott::from_rate_and_burst_per_datagram(0.9, 1.0, 1.0).is_err(),
        "p_gb > 1"
    );
    assert!(f(0.95, 1.0, 1.0).is_err(), "p_gb > 1");
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
        ("GHOSTFRAME_OUTBOUND_BURST_MEAN_MS", "8"),
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
        ("GHOSTFRAME_INBOUND_BURST_MEAN_MS", "8"),
        ("GHOSTFRAME_INBOUND_BURST_SEED", "99"),
    ]);
    let c = BurstConfig::from_lookup(BurstDirection::Inbound, g)
        .unwrap()
        .unwrap();
    assert_eq!(c.seed, 99);
    assert_eq!(c.model.loss_bad, 1.0);
    assert!((c.model.expected_loss() - 0.02).abs() < 1e-12);
    assert!((c.model.mean_burst_us().unwrap() - 8.0 * REF as f64).abs() < 1e-6);

    let g = lookup(&[
        ("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02"),
        ("GHOSTFRAME_OUTBOUND_BURST_MEAN_MS", "4"),
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
            ("GHOSTFRAME_OUTBOUND_BURST_MEAN_MS", "8"),
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
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_MEAN_MS", "0.05")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_MEAN_MS", "x")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_BAD", "0.01")]).is_err());
    assert!(base(&[("GHOSTFRAME_OUTBOUND_BURST_SEED", "-1")]).is_err());
    // The old datagram-denominated variable is an error, not reinterpreted.
    let leftover = lookup(&[
        ("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02"),
        ("GHOSTFRAME_OUTBOUND_BURST_MEAN_MS", "8"),
        ("GHOSTFRAME_OUTBOUND_BURST_MEAN_LEN", "8"),
    ]);
    let e = BurstConfig::from_lookup(BurstDirection::Outbound, leftover).unwrap_err();
    assert!(e.contains("MEAN_MS"), "{e}");
    // Probability without a length is a configuration error, not "off".
    let only_p = lookup(&[("GHOSTFRAME_OUTBOUND_BURST_LOSS_PROBABILITY", "0.02")]);
    assert!(BurstConfig::from_lookup(BurstDirection::Outbound, only_p).is_err());
    // Sibling variables without the probability must not silently disable it.
    for k in [
        "GHOSTFRAME_OUTBOUND_BURST_MEAN_MS",
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
