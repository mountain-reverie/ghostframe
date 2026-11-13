//! FEC v2 rev 6, R6-e E3: the loss estimator and the clean-link floor.
//!
//! Pure and sans-IO: time and samples are arguments, nothing here reads a
//! clock or touches the bridge. Every call is O(1).
//!
//! Inputs, per LOSS_REPORT: `received`, `lost_pre_fec` and `loss_run_hist`
//! only. `residual_lost`, `repair_wasted`, `blocks_unrecoverable` and the
//! fields the client leaves 0 are deliberately not read (plan decision 3).
//!
//! Two decayed accumulators, `lost` and `total`, give `p_hat = lost / total`.
//! Attack is instant (counts add), release is the decay. A second pair over
//! the run histogram gives the mean burst length `b_hat`.
//!
//! The floor is statistical, not `p_hat >= floor` (plan decision 5): a bare
//! threshold flaps at a stationary rate near the line, and a lone loss in a
//! young estimate reads as a large rate.

use ghostframe_protocol::fec::feedback::LossReport;

use super::adapt_size::{DeclinedTrim, Ladder, RSizer};
use super::block_table::CloseReason;
use crate::config::FecBlockParams;
use crate::transport::io_bridge::FecAdaptProbe;

/// Owner decision (rev 6 section 8 item 5): below 0.1 % loss the daemon sends
/// no repair (r = 0, PLAIN). It is a rate the loss must be shown to reach.
pub(crate) const FEC_LOSS_FLOOR: f64 = 0.001;
/// Estimator window in datagrams: the decay per report is `exp(-n / W)`. A
/// window of 1 000 flaps at 100 Mbit/s (under 0.1 s of traffic).
pub(crate) const FEC_ADAPT_WINDOW_DATAGRAMS: f64 = 10_000.0;
/// Estimator window in time: the decay never falls below `exp(-dt / tau)`, so
/// a slow link still forgets.
pub(crate) const FEC_ADAPT_TAU_US: u64 = 10_000_000;
/// Release is not allowed sooner than this after the last state change.
pub(crate) const FEC_ADAPT_MIN_DWELL_US: u64 = 5_000_000;
/// Engaging needs at least this many (decayed) losses: a stray loss never does.
pub(crate) const FEC_ADAPT_MIN_LOSSES: f64 = 3.0;
/// Normal quantile of the engage (lower) bound: one-sided 97.7 %. The plan
/// started at 1.64 (95 %); at 100 Mbit/s a stationary 0.1 % link then changed
/// state 5 times in a minute (a 10 000 datagram window is 1 s there), so the
/// two quantiles were widened (measured, 5 min runs, 18 seeds).
const Z_ENGAGE: f64 = 2.0;
/// Normal quantile of the release (upper) bound: one-sided 99 %. Wider than
/// the engage side on purpose: leaving r >= 1 is the cheap mistake to avoid
/// repeating, engaging is the one that protects content.
const Z_RELEASE: f64 = 2.33;
/// Run lengths the histogram buckets stand for: 1, 2, 3, 4, 5-8, 9-16,
/// 17-32, >32.
const RUN_REPRESENTATIVES: [f64; 8] = [1.0, 2.0, 3.0, 4.0, 6.5, 12.5, 24.5, 40.0];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FloorState {
    /// Loss not shown to be at or above the floor: r = 0.
    Clean,
    /// Loss shown to be at or above the floor.
    Engaged,
}

#[derive(Clone, Debug)]
pub(crate) struct LossEstimator {
    lost: f64,
    total: f64,
    /// Lost datagrams inside the histogram's runs, and the run count.
    run_lost: f64,
    runs: f64,
    state: FloorState,
    last_us: Option<u64>,
    last_change_us: Option<u64>,
    reports: u64,
    changes: u64,
    congestion_skips: u64,
}

impl LossEstimator {
    pub(crate) fn new() -> Self {
        Self {
            lost: 0.0,
            total: 0.0,
            run_lost: 0.0,
            runs: 0.0,
            state: FloorState::Clean,
            last_us: None,
            last_change_us: None,
            reports: 0,
            changes: 0,
            congestion_skips: 0,
        }
    }

    /// Fold one LOSS_REPORT in. A `congested` interval adds its datagrams to
    /// `total` but its losses to nothing (plan decision 4).
    pub(crate) fn on_report(&mut self, r: &LossReport, congested: bool, now_us: u64) {
        let lost = f64::from(r.lost_pre_fec);
        let n = f64::from(r.received) + lost;
        self.decay(n, now_us);
        self.total += n;
        if congested {
            if r.lost_pre_fec > 0 {
                self.congestion_skips += 1;
            }
        } else {
            self.lost += lost;
            for (count, rep) in r.loss_run_hist.iter().zip(RUN_REPRESENTATIVES) {
                self.runs += f64::from(*count);
                self.run_lost += f64::from(*count) * rep;
            }
        }
        self.reports += 1;
        self.evaluate(now_us);
    }

    /// Time passes with no report: decay and re-check release.
    pub(crate) fn tick(&mut self, now_us: u64) {
        self.decay(0.0, now_us);
        self.evaluate(now_us);
    }

    fn decay(&mut self, n: f64, now_us: u64) {
        let dt = self.last_us.map_or(0, |l| now_us.saturating_sub(l));
        self.last_us = Some(now_us.max(self.last_us.unwrap_or(0)));
        let e = (n / FEC_ADAPT_WINDOW_DATAGRAMS).max(dt as f64 / FEC_ADAPT_TAU_US as f64);
        let d = (-e).exp();
        self.lost *= d;
        self.total *= d;
        self.run_lost *= d;
        self.runs *= d;
    }

    fn evaluate(&mut self, now_us: u64) {
        let (l, n) = (self.lost, self.total);
        match self.state {
            FloorState::Clean => {
                if l >= FEC_ADAPT_MIN_LOSSES
                    && n > 0.0
                    && (l - Z_ENGAGE * l.sqrt()) / n >= FEC_LOSS_FLOOR
                {
                    self.set(FloorState::Engaged, now_us);
                }
            }
            FloorState::Engaged => {
                let dwelled = self
                    .last_change_us
                    .is_none_or(|c| now_us.saturating_sub(c) >= FEC_ADAPT_MIN_DWELL_US);
                let upper = if n > 0.0 {
                    (l + Z_RELEASE * (l + 1.0).sqrt() + 1.0) / n
                } else {
                    f64::INFINITY
                };
                if dwelled && upper < FEC_LOSS_FLOOR {
                    self.set(FloorState::Clean, now_us);
                }
            }
        }
    }

    fn set(&mut self, s: FloorState, now_us: u64) {
        self.state = s;
        self.last_change_us = Some(now_us);
        self.changes += 1;
    }

    /// Loss rate over all datagrams (repairs included); 0 before any traffic.
    pub(crate) fn p_hat(&self) -> f64 {
        if self.total > 0.0 {
            self.lost / self.total
        } else {
            0.0
        }
    }

    /// Mean loss-run length; 1 (no burst evidence) when no run was seen.
    pub(crate) fn b_hat(&self) -> f64 {
        if self.runs > 0.0 {
            self.run_lost / self.runs
        } else {
            1.0
        }
    }

    pub(crate) fn lost(&self) -> f64 {
        self.lost
    }
    pub(crate) fn total(&self) -> f64 {
        self.total
    }
    pub(crate) fn state(&self) -> FloorState {
        self.state
    }
    pub(crate) fn reports_seen(&self) -> u64 {
        self.reports
    }
    pub(crate) fn state_changes(&self) -> u64 {
        self.changes
    }
    pub(crate) fn congestion_skips(&self) -> u64 {
        self.congestion_skips
    }
}

/// Time constant of the overhead EWMA: it moves slowly, so the encoder target
/// does not follow every block.
const RHO_TAU_US: f64 = 2_000_000.0;
/// No overhead is reported for this long after blocks went live: the first
/// samples cover a handful of blocks.
const RHO_WARMUP_US: u64 = 2_000_000;
/// A sample spans at least this long.
const RHO_SAMPLE_US: u64 = 250_000;

/// The planned repair overhead `rho`: an EWMA of repair wire bytes over source
/// wire bytes, read by the H.264 rate target (plan E6, decision 7). 0 while
/// blocks are not live and in the first [`RHO_WARMUP_US`] after they are.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Rho {
    ewma: Option<f64>,
    live_since_us: Option<u64>,
    last_us: u64,
    last_source: u64,
    last_repair: u64,
}

impl Rho {
    /// Fold in the cumulative byte counters at `now_us`; the overhead now.
    pub(crate) fn update(&mut self, now_us: u64, source: u64, repair: u64, live: bool) -> f64 {
        if !live {
            *self = Self::default();
            return 0.0;
        }
        let since = *self.live_since_us.get_or_insert_with(|| {
            self.last_us = now_us;
            self.last_source = source;
            self.last_repair = repair;
            now_us
        });
        let dt = now_us.saturating_sub(self.last_us);
        if dt >= RHO_SAMPLE_US && source > self.last_source {
            let x =
                (repair - self.last_repair.min(repair)) as f64 / (source - self.last_source) as f64;
            self.ewma = Some(match self.ewma {
                None => x,
                Some(e) => e + (1.0 - (-(dt as f64) / RHO_TAU_US).exp()) * (x - e),
            });
            self.last_us = now_us;
            self.last_source = source;
            self.last_repair = repair;
        }
        if now_us.saturating_sub(since) < RHO_WARMUP_US {
            return 0.0;
        }
        self.ewma.unwrap_or(0.0)
    }
}

/// The adaptive controller the bridge holds (plan E5): the estimator, the
/// span ladder, the `r` sizer and the declined-share trim, behind four calls.
/// Sans-IO; every call is O(1) amortised.
#[derive(Debug)]
pub(crate) struct FecAdapt {
    est: LossEstimator,
    ladder: Ladder,
    sizer: RSizer,
    trim: DeclinedTrim,
    was_engaged: bool,
    /// Blocks sized by `r_for_block`, and how many of them at the cap.
    blocks_sized: u64,
    saturated: u64,
    /// Changes of the live parameters the bridge applied (`note_applied`).
    changes: u64,
    last_applied: Option<FecBlockParams>,
}

impl FecAdapt {
    pub(crate) fn new() -> Self {
        Self {
            est: LossEstimator::new(),
            ladder: Ladder::new(),
            sizer: RSizer::new(),
            trim: DeclinedTrim::new(),
            was_engaged: false,
            blocks_sized: 0,
            saturated: 0,
            changes: 0,
            last_applied: None,
        }
    }

    /// Fold in a LOSS_REPORT. The interval is *congested* only when the delay
    /// signal says so: a standing queue holds (`bwe.standing_queue()`).
    ///
    /// quinn's `congestion_events` is deliberately not read, alone or
    /// otherwise: under Cubic it is loss by another name. Under random loss
    /// Cubic declares an event on almost every loss (measured: a rise on nearly
    /// every report at 5 % i.i.d., so gating on it never engaged), and on a
    /// policed, shallow-buffer link the loss looks random. A rise that
    /// coincides with the delay signal adds nothing, since the standing queue
    /// already gates it. Nor `is_limited()`: goog_cc is loss-limited under
    /// plain random loss too. On a policed link goog_cc's loss-based estimator
    /// lowers the rate and the encoder target pays for the repair overhead
    /// (E6), so engaging FEC there is acceptable.
    pub(crate) fn on_report(&mut self, r: &LossReport, standing_queue: bool, now_us: u64) {
        self.est.on_report(r, standing_queue, now_us);
    }

    /// Time passes: decay, re-check the release, and read the declined
    /// closes the table logged.
    pub(crate) fn tick(&mut self, now_us: u64, closes: impl IntoIterator<Item = CloseReason>) {
        for c in closes {
            self.trim.on_close(c);
        }
        self.est.tick(now_us);
    }

    /// The live parameters now: `None` while the floor is `Clean`. `r` in the
    /// result is a placeholder (1); each block's `r` comes from
    /// [`Self::r_for_block`] at its close, so a change of `r` alone never
    /// reopens a block.
    pub(crate) fn live_params(
        &mut self,
        rate_bps: u64,
        max_dg: usize,
        now_us: u64,
    ) -> Option<FecBlockParams> {
        let engaged = self.est.state() == FloorState::Engaged;
        if engaged && !self.was_engaged {
            self.sizer.reset_pacing();
        }
        self.was_engaged = engaged;
        if !engaged {
            return None;
        }
        let span = self.ladder.step(self.est.b_hat(), rate_bps, max_dg, now_us);
        Some(FecBlockParams {
            k_max: span.k_max,
            t_ms: span.t_ms,
            r: 1,
        })
    }

    /// `r` for a block that closed with `k` sources.
    pub(crate) fn r_for_block(&mut self, k: u32, now_us: u64) -> u8 {
        let (p, b, trim) = (self.est.p_hat(), self.est.b_hat(), self.trim.trim());
        let d = self.sizer.r_for_paced(k, p, b, trim, now_us);
        self.blocks_sized += 1;
        self.saturated += u64::from(d.saturated);
        d.r
    }

    /// Records the parameters the bridge now runs; a different value is one
    /// change (engage, release, or a new span).
    pub(crate) fn note_applied(&mut self, live: Option<FecBlockParams>) {
        if live != self.last_applied {
            self.last_applied = live;
            self.changes += 1;
        }
    }

    /// Counters and readings for the log line and the harness. `r` at k = 8
    /// and 64 is what a block of that size would get now (0 while `Clean`);
    /// it is read from a copy of the sizer, so asking changes nothing.
    pub(crate) fn probe(&self) -> FecAdaptProbe {
        let engaged = self.est.state() == FloorState::Engaged;
        let (p, b, trim) = (self.est.p_hat(), self.est.b_hat(), self.trim.trim());
        let mut sizer = self.sizer.clone();
        let mut r_at = |k: u32| {
            if engaged {
                sizer.r_for(k, p, b, trim).r
            } else {
                0
            }
        };
        FecAdaptProbe {
            engaged,
            p_hat_ppm: (p * 1e6).round() as u64,
            b_hat_x100: (b * 100.0).round() as u64,
            k_max: self.last_applied.map_or(0, |a| a.k_max),
            t_ms: self.last_applied.map_or(0, |a| a.t_ms),
            r_at_k8: r_at(8),
            r_at_k64: r_at(64),
            changes: self.changes,
            floor_changes: self.est.state_changes(),
            congestion_skips: self.est.congestion_skips(),
            trim_steps: self.trim.steps(),
            blocks_sized: self.blocks_sized,
            saturated: self.saturated,
        }
    }

    pub(crate) fn estimator(&self) -> &LossEstimator {
        &self.est
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rho_is_zero_until_live_for_two_seconds_then_follows_the_byte_ratio() {
        let mut r = Rho::default();
        let (mut src, mut rep) = (0u64, 0u64);
        let mut out = Vec::new();
        for i in 0..=40u64 {
            src += 100_000;
            rep += 25_000;
            out.push(r.update(i * 100_000, src, rep, true));
        }
        assert!(out[..20].iter().all(|&x| x == 0.0), "{:?}", &out[..20]);
        assert!((out[40] - 0.25).abs() < 1e-9, "{}", out[40]);
        // Not live: back to zero and forgotten.
        assert_eq!(r.update(5_000_000, src, rep, false), 0.0);
        assert_eq!(r.update(5_100_000, src + 1, rep, true), 0.0);
    }

    fn rep(received: u16, lost: u16) -> LossReport {
        LossReport {
            received,
            lost_pre_fec: lost,
            ..Default::default()
        }
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> f64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    /// Stationary i.i.d. loss `p` at `mbit` Mbit/s (1200-byte datagrams),
    /// reports every 250 ms or 128 datagrams. Returns state changes per
    /// minute (max over minutes), the estimator and whether it ever engaged.
    fn run_iid(p: f64, mbit: f64, seed: u64, minutes: u64) -> (u64, LossEstimator, bool) {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
        let mut e = LossEstimator::new();
        let dg_per_s = mbit * 1e6 / 8.0 / 1200.0;
        let us_per_dg = 1e6 / dg_per_s;
        let (mut t, mut rx, mut lo) = (0.0f64, 0u32, 0u32);
        let mut last_report = 0.0;
        let mut engaged = false;
        let (mut worst, mut base) = (0u64, 0u64);
        let end = (minutes * 60) as f64 * 1e6;
        let mut next_min = 60e6;
        while t < end {
            t += us_per_dg;
            if rng.next() < p {
                lo += 1;
            } else {
                rx += 1;
            }
            if rx + lo >= 128 || t - last_report >= 250_000.0 {
                e.on_report(&rep(rx as u16, lo as u16), false, t as u64);
                engaged |= e.state() == FloorState::Engaged;
                (rx, lo, last_report) = (0, 0, t);
            }
            if t >= next_min {
                worst = worst.max(e.state_changes() - base);
                base = e.state_changes();
                next_min += 60e6;
            }
        }
        (worst, e, engaged)
    }

    #[test]
    fn clean_link_never_engages() {
        let mut e = LossEstimator::new();
        for i in 0..(3600 * 4) {
            e.on_report(&rep(128, 0), false, i * 250_000);
        }
        assert!(e.reports_seen() > 10_000);
        assert!(e.total() > 0.0);
        assert_eq!(e.state(), FloorState::Clean);
        assert_eq!(e.state_changes(), 0);
        assert_eq!(e.p_hat(), 0.0);
    }

    #[test]
    fn a_lone_loss_never_engages_at_any_rate() {
        for n in [50u32, 100, 128, 500, 1000, 10_000, 100_000, 1_000_000] {
            // n datagrams in a fresh estimator, one lost, in 16-bit chunks.
            let mut e = LossEstimator::new();
            let mut left = n - 1;
            let mut first = true;
            let mut t = 0;
            while left > 0 || first {
                let chunk = left.min(60_000);
                e.on_report(&rep(chunk as u16, u16::from(first)), false, t);
                first = false;
                left -= chunk;
                t += 250_000;
            }
            assert!(e.reports_seen() >= 1 && e.total() > 0.0);
            assert!(e.lost() > 0.0, "the loss must have been counted");
            assert_eq!(e.state(), FloorState::Clean, "N = {n}");
        }
        // And two losses, still not enough.
        let mut e = LossEstimator::new();
        e.on_report(&rep(50, 2), false, 0);
        assert_eq!(e.state(), FloorState::Clean);
    }

    #[test]
    fn stationary_001_percent_never_engages_and_01_percent_is_calm() {
        for mbit in [2.0, 10.0, 100.0] {
            for seed in 1..=20u64 {
                let (_, e, engaged) = run_iid(0.0001, mbit, seed, 3);
                assert!(e.reports_seen() > 100 && e.lost() >= 0.0);
                assert!(!engaged, "0.01 % engaged at {mbit} Mbit/s, seed {seed}");
            }
        }
        let mut any_lost = false;
        for mbit in [2.0, 10.0, 100.0] {
            for seed in 1..=20u64 {
                let (worst, e, _) = run_iid(0.001, mbit, seed, 5);
                assert!(e.reports_seen() > 100);
                any_lost |= e.lost() > 0.0;
                assert!(
                    worst <= 2,
                    "0.1 % changed state {worst}x in a minute at {mbit} Mbit/s, seed {seed}"
                );
            }
        }
        assert!(any_lost);
    }

    #[test]
    fn a_step_to_5_percent_engages_at_once_and_converges() {
        let mut e = LossEstimator::new();
        for i in 0..40 {
            e.on_report(&rep(128, 0), false, i * 250_000);
        }
        assert_eq!(e.state(), FloorState::Clean);
        let t0 = 40 * 250_000;
        // After 10 s of clean traffic the lower bound needs ~20 losses: one
        // 1 000 datagram report at 5 % does it; a 128 datagram one (6 losses)
        // takes a few reports, which is the price of the confidence bound.
        e.on_report(&rep(950, 50), false, t0);
        assert!(e.reports_seen() == 41 && e.lost() >= 50.0 - 1e-9);
        assert_eq!(e.state(), FloorState::Engaged, "within one report");
        let mut rng = Rng(7);
        let mut t = t0;
        // 3 * tau of traffic at 10 Mbit/s cadence (128 dg per report, ~100 ms).
        while t < t0 + 3 * FEC_ADAPT_TAU_US {
            t += 100_000;
            let lo = (0..128).filter(|_| rng.next() < 0.05).count() as u16;
            e.on_report(&rep(128 - lo, lo), false, t);
        }
        assert!((e.p_hat() - 0.05).abs() < 0.01, "p_hat {}", e.p_hat());
        assert_eq!(e.state(), FloorState::Engaged);
    }

    #[test]
    fn release_waits_for_the_dwell() {
        let mut e = LossEstimator::new();
        e.on_report(&rep(100, 5), false, 0);
        assert_eq!(e.state(), FloorState::Engaged);
        // A huge clean report drowns the loss at once: the bound is below the
        // floor, but the dwell has not passed.
        e.on_report(&rep(60_000, 0), false, 1_000_000);
        e.on_report(&rep(60_000, 0), false, 2_000_000);
        assert!(e.total() > 50_000.0);
        assert_eq!(e.state(), FloorState::Engaged, "inside the dwell");
        e.tick(FEC_ADAPT_MIN_DWELL_US);
        assert_eq!(e.state(), FloorState::Clean, "after the dwell");
        assert_eq!(e.state_changes(), 2);
    }

    #[test]
    fn a_congested_interval_counts_datagrams_not_losses() {
        let mut a = LossEstimator::new();
        let mut b = LossEstimator::new();
        a.on_report(&rep(1000, 0), false, 0);
        b.on_report(&rep(1000, 0), false, 0);
        let mut r = rep(900, 100);
        r.loss_run_hist[0] = 100;
        a.on_report(&r, true, 250_000);
        b.on_report(&rep(1000, 0), false, 250_000);
        assert!(a.reports_seen() == 2);
        assert_eq!(a.lost(), 0.0);
        assert!((a.total() - b.total()).abs() < 1e-9);
        assert_eq!(a.state(), FloorState::Clean);
        assert_eq!(a.congestion_skips(), 1);
        assert_eq!(
            a.b_hat(),
            1.0,
            "a congested interval's runs are not counted"
        );
        // The same report uncongested engages.
        let mut c = LossEstimator::new();
        c.on_report(&r, false, 0);
        assert_eq!(c.state(), FloorState::Engaged);
    }

    #[test]
    fn b_hat_from_known_runs_is_within_bucket_rounding() {
        let mut e = LossEstimator::new();
        let mut r = rep(1000, 0);
        // 10 runs of 1, 10 runs of 3, 10 runs of 4: mean 8/3.
        r.loss_run_hist[0] = 10;
        r.loss_run_hist[2] = 10;
        r.loss_run_hist[3] = 10;
        r.lost_pre_fec = 80;
        e.on_report(&r, false, 0);
        assert!(e.reports_seen() == 1 && e.runs > 0.0);
        assert!((e.b_hat() - 8.0 / 3.0).abs() < 1e-9, "{}", e.b_hat());
        // A bucket spanning 5..=8 reads 6.5: error at most 1.5 on a run.
        let mut f = LossEstimator::new();
        let mut r = rep(1000, 0);
        r.loss_run_hist[4] = 4;
        f.on_report(&r, false, 0);
        assert!((f.b_hat() - 6.5).abs() <= 1.5);
        assert_eq!(LossEstimator::new().b_hat(), 1.0);
    }

    #[test]
    fn unread_fields_do_not_change_the_estimate() {
        let mut a = LossEstimator::new();
        let mut b = LossEstimator::new();
        for i in 0..50u64 {
            let mut r = rep(120, 8);
            r.loss_run_hist[1] = 4;
            a.on_report(&r, false, i * 250_000);
            r.residual_lost = 9999;
            r.repair_wasted = 7777;
            r.blocks_unrecoverable = 5555;
            r.recovered = 3333;
            r.repair_rx = 11;
            r.repair_used = 12;
            r.src_lost_pre_fec = 4444;
            r.h264_frames_lost = 2222;
            r.recovery_p95_ms = 99;
            r.reorder_max = 77;
            r.interval_ms = 250;
            b.on_report(&r, false, i * 250_000);
        }
        assert!(a.reports_seen() == 50 && a.lost() > 0.0);
        assert_eq!(a.lost(), b.lost());
        assert_eq!(a.total(), b.total());
        assert_eq!(a.b_hat(), b.b_hat());
        assert_eq!(a.state(), b.state());
    }

    #[test]
    fn probe_counts_sizing_saturation_and_changes() {
        let mut a = FecAdapt::new();
        let clean = a.probe();
        assert!(!clean.engaged);
        assert_eq!(
            (clean.r_at_k8, clean.r_at_k64, clean.blocks_sized),
            (0, 0, 0)
        );
        // Bursty 5 % loss (runs of 8): the cap is unreachable at k = 64.
        let mut r = rep(950, 50);
        r.loss_run_hist[4] = 7;
        for i in 0..20u64 {
            a.on_report(&r, false, i * 250_000);
        }
        a.tick(5_000_000, []);
        assert_eq!(a.estimator().reports_seen(), 20, "reports were seen");
        let live = a.live_params(10_000_000, 1200, 5_000_000);
        a.note_applied(live);
        a.note_applied(live);
        assert_eq!(a.r_for_block(64, 5_000_000), 16, "the 25 % overhead cap");
        let p = a.probe();
        assert!(p.engaged && p.p_hat_ppm > 0 && p.b_hat_x100 > 150);
        assert_eq!((p.blocks_sized, p.saturated, p.changes), (1, 1, 1));
        assert!(p.k_max > 0 && p.t_ms > 0 && p.r_at_k64 == 16 && p.r_at_k8 > 0);
        // A congested interval is counted.
        a.on_report(&r, true, 6_000_000);
        assert_eq!(a.probe().congestion_skips, 1);
    }
}
