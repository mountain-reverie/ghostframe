//! Pure metric extraction over a bandwidth-estimate time series, for the
//! nested-congestion-control measurement scenes
//! (`tests/browserless_nested_cc.rs`).
//!
//! Everything here takes the `(virtual_us, bps)` series that
//! `BrowserlessResult::bwe_estimate_samples` exposes and returns a number or
//! `None`. Nothing runs a scene, so each definition is unit-testable against
//! a synthetic series, and a later change (wire-departure timestamps fed to
//! the BWE) can be compared against the same definitions.
//!
//! # Conventions
//!
//! * Times are virtual microseconds on the series' own clock.
//! * The series is a sample-and-hold signal: a sample's value holds until
//!   the next sample. Samples are ~100 ms apart.
//! * A value is "within the band" of `target` when
//!   `|v - target| <= tol * target`, `tol` being [`BAND`] (0.25) by default.
//! * `None` means "the event never happened inside the observed window",
//!   never "zero". A scene must report `None` as non-convergence, not as 0.

/// The +-25% band used for every convergence definition.
pub const BAND: f64 = 0.25;

/// A reversal is a swing of more than this fraction (5%) against the last
/// extremum.
pub const REVERSAL_THRESHOLD: f64 = 0.05;

pub type Series = [(u64, u64)];

fn in_band(v: u64, target: f64, tol: f64) -> bool {
    (v as f64 - target).abs() <= tol * target
}

/// Samples with `from_us <= t < to_us`.
pub fn window(s: &Series, from_us: u64, to_us: u64) -> Vec<(u64, u64)> {
    s.iter()
        .copied()
        .filter(|(t, _)| *t >= from_us && *t < to_us)
        .collect()
}

/// True if the series is not constant: a dead estimator pinned at its seed
/// must not be able to report "converged".
pub fn estimate_moved(s: &Series) -> bool {
    match s.first() {
        Some((_, first)) => s.iter().any(|(_, v)| v != first),
        None => false,
    }
}

/// Time from `from_us` until the estimate first reaches `fraction * target`
/// (a one-sided "first reach", e.g. 50% of capacity). `None` if never.
pub fn time_to_first_reach(
    s: &Series,
    from_us: u64,
    target_bps: f64,
    fraction: f64,
) -> Option<u64> {
    s.iter()
        .find(|(t, v)| *t >= from_us && (*v as f64) >= target_bps * fraction)
        .map(|(t, _)| t - from_us)
}

/// Time from `from_us` until the estimate first enters the +-`tol` band
/// around `target_bps`, whether or not it stays. `None` if never.
pub fn time_to_first_enter(s: &Series, from_us: u64, target_bps: f64, tol: f64) -> Option<u64> {
    s.iter()
        .find(|(t, v)| *t >= from_us && in_band(*v, target_bps, tol))
        .map(|(t, _)| t - from_us)
}

/// Convergence time: the earliest sample time `t >= from_us` such that every
/// sample in `[t, end_us)` lies inside the band, **and** `[t, end_us)` spans at
/// least `min_hold_us`. Returns `t - from_us`.
///
/// The hold requirement is what stops a single in-band sample at the very end
/// of the series counting as convergence.
pub fn convergence_time(
    s: &Series,
    from_us: u64,
    end_us: u64,
    target_bps: f64,
    tol: f64,
    min_hold_us: u64,
) -> Option<u64> {
    let w = window(s, from_us, end_us);
    // Walk backwards to find the start of the maximal in-band suffix.
    let mut start: Option<usize> = None;
    for (i, (_, v)) in w.iter().enumerate().rev() {
        if in_band(*v, target_bps, tol) {
            start = Some(i);
        } else {
            break;
        }
    }
    let i = start?;
    let t = w[i].0;
    let last_t = w.last()?.0;
    if last_t.saturating_sub(t) < min_hold_us {
        return None;
    }
    Some(t - from_us)
}

/// Number of direction reversals larger than `threshold` (fraction of the
/// running extremum) within `[from_us, to_us)`. A zig-zag filter, so
/// sample-to-sample jitter smaller than the threshold never counts and a
/// monotone ramp counts zero.
pub fn direction_reversals(s: &Series, from_us: u64, to_us: u64, threshold: f64) -> u32 {
    let w = window(s, from_us, to_us);
    let mut it = w.iter().map(|(_, v)| *v as f64);
    let Some(first) = it.next() else { return 0 };
    // `pivot` is the last confirmed extremum, `extreme` the running one.
    let mut dir: i8 = 0; // +1 rising, -1 falling, 0 undecided
    let mut extreme = first;
    let mut reversals = 0;
    for v in it {
        match dir {
            0 => {
                if v > extreme * (1.0 + threshold) {
                    dir = 1;
                    extreme = v;
                } else if v < extreme * (1.0 - threshold) {
                    dir = -1;
                    extreme = v;
                } else {
                    // Still undecided; track nothing so slow drift cannot
                    // accumulate into a phantom swing.
                }
            }
            1 => {
                if v > extreme {
                    extreme = v;
                } else if v < extreme * (1.0 - threshold) {
                    dir = -1;
                    extreme = v;
                    reversals += 1;
                }
            }
            _ => {
                if v < extreme {
                    extreme = v;
                } else if v > extreme * (1.0 + threshold) {
                    dir = 1;
                    extreme = v;
                    reversals += 1;
                }
            }
        }
    }
    reversals
}

/// `(max - min) / mean` over `[from_us, to_us)`; `None` for an empty window
/// or zero mean.
pub fn peak_to_peak_over_mean(s: &Series, from_us: u64, to_us: u64) -> Option<f64> {
    let w = window(s, from_us, to_us);
    if w.is_empty() {
        return None;
    }
    let max = w.iter().map(|(_, v)| *v).max()? as f64;
    let min = w.iter().map(|(_, v)| *v).min()? as f64;
    let mean = w.iter().map(|(_, v)| *v as f64).sum::<f64>() / w.len() as f64;
    if mean <= 0.0 {
        return None;
    }
    Some((max - min) / mean)
}

/// Mean of the samples in `[from_us, to_us)`.
pub fn mean_bps(s: &Series, from_us: u64, to_us: u64) -> Option<f64> {
    let w = window(s, from_us, to_us);
    if w.is_empty() {
        return None;
    }
    Some(w.iter().map(|(_, v)| *v as f64).sum::<f64>() / w.len() as f64)
}

/// Median of the samples in `[from_us, to_us)`.
pub fn median_bps(s: &Series, from_us: u64, to_us: u64) -> Option<u64> {
    let mut v: Vec<u64> = window(s, from_us, to_us).iter().map(|(_, b)| *b).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[v.len() / 2])
}

/// Dip depth: `1 - min/reference` over `[from_us, to_us)`, clamped to 0 (a
/// window that never goes below the reference has no dip).
pub fn dip_depth(s: &Series, from_us: u64, to_us: u64, reference_bps: f64) -> Option<f64> {
    let w = window(s, from_us, to_us);
    let min = w.iter().map(|(_, v)| *v).min()? as f64;
    if reference_bps <= 0.0 {
        return None;
    }
    Some((1.0 - min / reference_bps).max(0.0))
}

/// Time after `resume_us` until the estimate is back within `tol` of
/// `reference_bps` (first entry, one-sided from below is not required: a
/// sample inside the band counts). `None` if never.
pub fn recovery_time(s: &Series, resume_us: u64, reference_bps: f64, tol: f64) -> Option<u64> {
    time_to_first_enter(s, resume_us, reference_bps, tol)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;
    const S: u64 = 1_000_000;

    /// 100 ms samples, `f(t_us)` -> bps.
    fn series(dur_us: u64, f: impl Fn(u64) -> f64) -> Vec<(u64, u64)> {
        (0..dur_us / (100 * MS))
            .map(|i| {
                let t = i * 100 * MS;
                (t, f(t).max(0.0) as u64)
            })
            .collect()
    }

    const C: f64 = 10_000_000.0;

    /// Exponential approach to C from 2 Mbps with time constant 2 s.
    fn ramp() -> Vec<(u64, u64)> {
        series(20 * S, |t| C - (C - 2e6) * (-(t as f64) / 2e6).exp())
    }

    #[test]
    fn clean_ramp_converges_at_the_analytic_time() {
        let s = ramp();
        // Enters the +-25% band when 10 - 8e^{-t/2} >= 7.5 -> t = 2*ln(8/2.5)
        // = 2.326 s; sample grid is 100 ms so expect 2.3-2.4 s.
        let t = convergence_time(&s, 0, 20 * S, C, BAND, 2 * S).expect("must converge");
        assert!((2_300 * MS..=2_500 * MS).contains(&t), "got {t}");
        // 50% of C (5 Mbps): 2*ln(8/5) = 0.94 s.
        let h = time_to_first_reach(&s, 0, C, 0.5).unwrap();
        assert!((900 * MS..=1_100 * MS).contains(&h), "got {h}");
        // A ramp has no reversals and tiny settled spread.
        assert_eq!(direction_reversals(&s, 0, 20 * S, REVERSAL_THRESHOLD), 0);
        assert!(peak_to_peak_over_mean(&s, 10 * S, 20 * S).unwrap() < 0.05);
    }

    #[test]
    fn a_series_that_never_converges_reports_none() {
        // Plateau at 40% of C.
        let s = series(20 * S, |t| (4e6_f64).min(1e6 + t as f64 * 3.0));
        assert_eq!(convergence_time(&s, 0, 20 * S, C, BAND, 2 * S), None);
        assert_eq!(time_to_first_enter(&s, 0, C, BAND), None);
        assert_eq!(time_to_first_reach(&s, 0, C, 0.5), None);
    }

    #[test]
    fn entering_the_band_and_leaving_is_not_convergence() {
        // In band 3-4 s only, otherwise at 3 Mbps.
        let s = series(
            10 * S,
            |t| if (3 * S..4 * S).contains(&t) { C } else { 3e6 },
        );
        assert_eq!(time_to_first_enter(&s, 0, C, BAND), Some(3 * S));
        assert_eq!(convergence_time(&s, 0, 10 * S, C, BAND, 2 * S), None);
    }

    #[test]
    fn a_lone_in_band_sample_at_the_end_does_not_converge() {
        let mut s = series(10 * S, |_| 3e6);
        s.last_mut().unwrap().1 = C as u64;
        assert_eq!(convergence_time(&s, 0, 10 * S, C, BAND, 2 * S), None);
    }

    #[test]
    fn convergence_requires_staying_not_just_the_first_entry() {
        // Enters at 2 s, dips out at 5 s for 1 s, back in from 6 s on.
        let s = series(15 * S, |t| {
            if t < 2 * S {
                3e6
            } else if (5 * S..6 * S).contains(&t) {
                4e6
            } else {
                C
            }
        });
        assert_eq!(time_to_first_enter(&s, 0, C, BAND), Some(2 * S));
        assert_eq!(convergence_time(&s, 0, 15 * S, C, BAND, 2 * S), Some(6 * S));
    }

    #[test]
    fn a_30_percent_oscillation_exceeds_the_band_and_counts_reversals() {
        // Square-ish +-30% about C every 1 s (0.5 s high, 0.5 s low).
        let s = series(20 * S, |t| {
            if (t / (500 * MS)).is_multiple_of(2) {
                1.3 * C
            } else {
                0.7 * C
            }
        });
        assert_eq!(convergence_time(&s, 0, 20 * S, C, BAND, 2 * S), None);
        let r = direction_reversals(&s, 5 * S, 15 * S, REVERSAL_THRESHOLD);
        assert!(r >= 17, "10 s of 1 s period is ~20 half-cycles, got {r}");
        let p = peak_to_peak_over_mean(&s, 5 * S, 15 * S).unwrap();
        assert!((p - 0.6).abs() < 0.05, "got {p}");
    }

    #[test]
    fn a_20_percent_oscillation_stays_in_band_but_still_reverses() {
        let s = series(20 * S, |t| {
            if (t / (500 * MS)).is_multiple_of(2) {
                1.1 * C
            } else {
                0.9 * C
            }
        });
        assert!(convergence_time(&s, 0, 20 * S, C, BAND, 2 * S).is_some());
        assert!(direction_reversals(&s, 5 * S, 15 * S, REVERSAL_THRESHOLD) >= 17);
    }

    #[test]
    fn jitter_below_the_reversal_threshold_is_ignored() {
        // +-2% alternate every sample.
        let s = series(10 * S, |t| {
            if (t / (100 * MS)).is_multiple_of(2) {
                1.02 * C
            } else {
                0.98 * C
            }
        });
        assert_eq!(direction_reversals(&s, 0, 10 * S, REVERSAL_THRESHOLD), 0);
    }

    #[test]
    fn a_dip_after_idle_is_measured_and_recovery_timed() {
        // At C until 10 s; decays to 40% of C by 14 s (idle), resumes at 16 s
        // and climbs back linearly over 3 s.
        let s = series(30 * S, |t| {
            if t < 10 * S {
                C
            } else if t < 16 * S {
                (C - 0.6 * C * ((t - 10 * S) as f64 / (4 * S) as f64).min(1.0)).max(0.4 * C)
            } else {
                (0.4 * C + 0.6 * C * ((t - 16 * S) as f64 / (3 * S) as f64)).min(C)
            }
        });
        let dip = dip_depth(&s, 10 * S, 30 * S, C).unwrap();
        assert!((dip - 0.6).abs() < 0.02, "got {dip}");
        let rec = recovery_time(&s, 16 * S, C, BAND).unwrap();
        // 7.5 Mbps = 0.4C + 0.6C*x -> x = 0.583 -> 1.75 s
        assert!((1_600 * MS..=1_900 * MS).contains(&rec), "got {rec}");
        // No dip case: a flat series at the reference has depth 0.
        let flat = series(5 * S, |_| C);
        assert_eq!(dip_depth(&flat, 0, 5 * S, C), Some(0.0));
    }

    #[test]
    fn a_constant_series_is_flagged_as_not_moved() {
        assert!(!estimate_moved(&series(5 * S, |_| 2e6)));
        assert!(estimate_moved(&ramp()));
        assert!(!estimate_moved(&[]));
    }

    #[test]
    fn empty_windows_report_none_not_zero() {
        assert_eq!(peak_to_peak_over_mean(&[], 0, S), None);
        assert_eq!(mean_bps(&[], 0, S), None);
        assert_eq!(median_bps(&[], 0, S), None);
        assert_eq!(dip_depth(&[], 0, S, C), None);
        assert_eq!(direction_reversals(&[], 0, S, 0.05), 0);
    }
}
