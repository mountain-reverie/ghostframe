// SPDX-License-Identifier: AGPL-3.0-or-later
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
//!
//! # The engage test follows the traffic volume (rev 7, FINDINGS section 16)
//!
//! Engaging asks one question: is the loss rate shown to be above the floor?
//! Under the null (rate = floor) the datagrams seen, `n`, would lose
//! `mu0 = n x floor` of them, Poisson. The estimator engages when
//! `P(X >= l | mu0) <= FEC_ENGAGE_ALPHA` (the exact upper tail, the same
//! one-sided 97.7 % as the former `z = 2`) and `l >= FEC_ADAPT_MIN_LOSSES`.
//! `mu0` is the volume term: with 800 datagrams in the window the bar is
//! 4 losses, with 5 000 it is 11, with 10 000 it is 18. The former bound
//! `(l - 2 sqrt(l)) / n >= floor` took its variance from the *observed*
//! count, which at a few losses overstates it (6 losses at 1 000 datagrams,
//! when the same 1 000 datagrams show 1 under the null); at 2 Mbit/s
//! (about 165 datagrams/s, a frame is about 5 of them, so a lost datagram is
//! a damaged frame) that was 0.8 s of 5 % loss on top of the report delay.
//!
//! The test reads its own accumulator pair, `lost_e` / `total_e`, that
//! forgets by time ([`FEC_ENGAGE_TAU_US`], 5 s) or by count
//! ([`FEC_ADAPT_WINDOW_DATAGRAMS`]) with no minimum mass: the window is
//! seconds of traffic, so at 165 datagrams/s it holds about 800 datagrams,
//! not the 5 000 the release side keeps (a fresh step is not diluted by 30 s
//! of clean history). Release reads the slow pair (`lost`, `total`) as
//! before: leaving r >= 1 must stay the cautious move.

use ghostframe_protocol::fec::feedback::LossReport;

use super::adapt_size::{
    r_cap, widest_block_sources, DeclinedTrim, Ladder, Notch, RSizer, SourceRate,
};
use super::block_table::CloseReason;
use crate::config::FecBlockParams;
use crate::transport::io_bridge::FecAdaptProbe;

/// Owner decision (rev 6 section 8 item 5): below 0.1 % loss the daemon sends
/// no repair (r = 0, PLAIN). It is a rate the loss must be shown to reach.
pub(crate) const FEC_LOSS_FLOOR: f64 = 0.001;
/// Estimator window in datagrams: the decay per report is `exp(-n / W)`. A
/// window of 1 000 flaps at 100 Mbit/s (under 0.1 s of traffic).
pub(crate) const FEC_ADAPT_WINDOW_DATAGRAMS: f64 = 10_000.0;
/// Estimator window in time: the time decay is `exp(-dt / tau)`, so a slow
/// link forgets, down to [`FEC_ADAPT_MIN_MASS`] (see there).
pub(crate) const FEC_ADAPT_TAU_US: u64 = 10_000_000;
/// Time constant of the engage window (its accumulators have no minimum
/// mass; see the module doc). Five seconds of traffic: 800 datagrams at
/// 2 Mbit/s H.264, where a fresh 5 % step shows 4 losses in about 0.5 s of
/// reports; above about 2 000 datagrams/s the count window is the shorter
/// and nothing changes.
pub(crate) const FEC_ENGAGE_TAU_US: u64 = 5_000_000;
/// Time decay may not pull the decayed `total` below this many datagrams
/// (count decay is not limited). Why: at `L = 0` the release bound is
/// `(Z_RELEASE + 1) / N`, which falls under the 0.001 floor only for
/// `N > 3 330`. A window of rate x tau holds fewer datagrams than that below
/// about 330 datagrams/s, so `N` settles under the bound and the state could
/// never release, even with no loss at all (measured: `N` about 1 840 at 180
/// datagrams/s, still engaged after 60 s). With the minimum the window at low
/// rates is about this many datagrams (a slower release, `MIN_MASS / rate`
/// seconds per e-fold); at high rates nothing changes. An idle link (`n = 0`)
/// stops forgetting once its total is down to the minimum: acceptable, since
/// no traffic means no repair cost, and the first report after the pause
/// brings its own counts.
pub(crate) const FEC_ADAPT_MIN_MASS: f64 = 5_000.0;
/// Release is not allowed sooner than this after the last state change.
pub(crate) const FEC_ADAPT_MIN_DWELL_US: u64 = 5_000_000;
/// Engaging needs at least this many (decayed) losses: a stray loss never does.
pub(crate) const FEC_ADAPT_MIN_LOSSES: f64 = 3.0;
/// Engage when the probability of seeing this many losses (or more) at the
/// floor rate is at most this: one-sided 97.7 %, the tail beyond z = 2. The
/// plan started at 1.64 (95 %); at 100 Mbit/s a stationary 0.1 % link then
/// changed state 5 times in a minute (a 10 000 datagram window is 1 s
/// there), so the two quantiles were widened (measured, 5 min runs, 18
/// seeds).
const FEC_ENGAGE_ALPHA: f64 = 0.0228;
/// Normal quantile of the release (upper) bound: one-sided 99 %. Wider than
/// the engage side on purpose: leaving r >= 1 is the cheap mistake to avoid
/// repeating, engaging is the one that protects content.
const Z_RELEASE: f64 = 2.33;
/// Run lengths the histogram buckets stand for: 1, 2, 3, 4, 5-8, 9-16,
/// 17-32, >32.
const RUN_REPRESENTATIVES: [f64; 8] = [1.0, 2.0, 3.0, 4.0, 6.5, 12.5, 24.5, 40.0];
/// Shortest and longest run each bucket can hold (the last has no bound).
const RUN_LOWER: [u32; 8] = [1, 2, 3, 4, 5, 9, 17, 33];
const RUN_UPPER: [Option<u32>; 8] = [
    Some(1),
    Some(2),
    Some(3),
    Some(4),
    Some(8),
    Some(16),
    Some(32),
    None,
];
/// Runs of at most this many datagrams are always left to FEC, however small
/// the block: on a slow link a block has one or two repair symbols, and an
/// ordinary short burst is the case FEC is for.
pub(crate) const FEC_UNREPAIRABLE_FLOOR: u32 = 4;
/// The longest loss run FEC could be asked to repair at a block of `k`
/// sources: a run straddling two blocks, each of which rebuilds `r_cap(k)` at
/// most, so `2 x r_cap(k)` (at most 64), and never under
/// [`FEC_UNREPAIRABLE_FLOOR`]. A longer run destroys at least one block past
/// anything its repair symbols could rebuild; it is for the resend path, and
/// must neither engage FEC nor raise its estimates.
///
/// (`2 x k`, two blocks of sources, was tried first and is far too loose: it
/// is 40 ms of datagrams, and an outage's gap is about `2 x R x (RTT + 5 ms)`
/// whatever the rate `R`, so below an RTT of 15 to 28 ms the outage was under
/// the limit and engaged FEC.)
pub(crate) fn repairable_run_limit(k: u32) -> u32 {
    (2 * r_cap(k)).max(FEC_UNREPAIRABLE_FLOOR)
}

/// How long the run limit stays at the highest value it reached, us. The
/// limit follows the block width, which follows the rate, and it steps at the
/// histogram's bucket edges (9 to 16 flips near 2.2 Mbit/s): a loss-driven
/// cut of the estimate must not move a run from one side of it to the other.
/// The rate it follows is already a 1 to 2 s mean, so the hold only has to
/// cover what the cut takes to reach a report: the run's forced close (1 s),
/// the report interval (0.25 s) and a round trip. 3 s does. It was 10 s, on a
/// peak that already lasted 5 to 10 s, and one burst held the limit up for 20
/// s; with a mean the longest a burst is felt is the 2 s of the mean and the
/// 3 s of the hold.
const RUN_LIMIT_HOLD_US: u64 = 3_000_000;

/// The run limit held at its highest for [`RUN_LIMIT_HOLD_US`].
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct HeldLimit {
    held: u32,
    at_us: u64,
}

impl HeldLimit {
    /// Fold in the limit `raw` now implies and return the one to use: the
    /// highest within the hold, `raw` once the hold has passed.
    pub(crate) fn hold(&mut self, raw: u32, now_us: u64) -> u32 {
        if raw >= self.held || now_us.saturating_sub(self.at_us) >= RUN_LIMIT_HOLD_US {
            self.held = raw;
            self.at_us = now_us;
        }
        self.held
    }
}

/// What a report's histogram says about runs FEC could not repair.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RunSplit {
    /// Upper bound of the losses that are in runs FEC could repair (or not
    /// shown to be longer): never below the true count of those.
    pub(crate) kept: f64,
    /// The histogram of the runs that stay (a left-out bucket reads 0).
    pub(crate) runs: [f64; 8],
    /// Runs left out.
    pub(crate) runs_unrepairable: u64,
    /// `lost - kept`: losses left out, at least what the left-out runs hold.
    pub(crate) losses_unrepairable: u64,
}

/// Split `lost` losses by the report's run histogram around `limit`.
///
/// A bucket is left out when its LOWER bound exceeds `limit`: every run in it
/// is longer than FEC could repair. The `>32` bucket is always left out: the
/// limit is at most 64 (`2 x r_cap`, `r_cap <= 32`), and a run of 33 to 64 is
/// at the edge of what two full blocks could split between them, while a
/// bucket with no upper bound cannot be told apart from an outage by its
/// count. (Deciding it by the report's total instead keeps a run that a
/// second long run in the same report hides: 70 + 70 at limit 42.)
///
/// The losses left out are not `count x representative` (the `>32`
/// representative, 40, can be far under the real run: 870 in a measured
/// outage); only `count x lower bound` is certain.
///
/// What is kept is bounded from above two ways, and the smaller is used:
/// `lost - (the left-out minimum)`, and what the kept runs can hold at most
/// (`count x upper`). So `kept = min(.., ..)`, clamped at 0: a report whose
/// only run is an outage keeps nothing. An upper bound on `kept` errs toward
/// protection. O(8).
pub(crate) fn split_runs(lost: u32, hist: &[u16; 8], limit: u32) -> RunSplit {
    let lost = u64::from(lost);
    let mut runs = [0.0f64; 8];
    let mut excl_min = 0u64;
    let mut runs_excl = 0u64;
    let mut kept_cap = 0u64;
    for i in 0..8 {
        let c = u64::from(hist[i]);
        if i == 7 || RUN_LOWER[i] > limit {
            runs_excl += c;
            excl_min += c * u64::from(RUN_LOWER[i]);
        } else {
            runs[i] = f64::from(hist[i]);
            kept_cap += c * u64::from(RUN_UPPER[i].unwrap_or(0));
        }
    }
    if runs_excl == 0 {
        return RunSplit {
            kept: lost as f64,
            runs,
            runs_unrepairable: 0,
            losses_unrepairable: 0,
        };
    }
    let kept = lost.saturating_sub(excl_min).min(kept_cap);
    RunSplit {
        kept: kept as f64,
        runs,
        runs_unrepairable: runs_excl,
        losses_unrepairable: lost - kept,
    }
}

/// `P(X >= l)` for `X ~ Poisson(mu)`, `l` real (linear between the integers
/// around it: the decayed count is fractional). At most `l + 1` terms.
fn poisson_upper_tail(l: f64, mu: f64) -> f64 {
    let k = l.floor() as usize;
    let f = l - k as f64;
    let (mut pmf, mut cdf) = ((-mu).exp(), 0.0);
    for j in 0..k {
        cdf += pmf;
        pmf *= mu / (j + 1) as f64;
    }
    (1.0 - cdf - f * pmf).max(0.0)
}

/// The engage test: `l` (decayed) losses over `n` datagrams show a rate above
/// the floor (module doc). A count far past the null's mean needs no sum.
fn engage_shown(l: f64, n: f64) -> bool {
    if l < FEC_ADAPT_MIN_LOSSES || n <= 0.0 {
        return false;
    }
    let mu0 = n * FEC_LOSS_FLOOR;
    l > mu0 + 12.0 * mu0.sqrt() + 30.0 || poisson_upper_tail(l, mu0) <= FEC_ENGAGE_ALPHA
}

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
    /// The engage window's pair (see the module doc): same inputs, shorter
    /// memory at low rates, no minimum mass.
    lost_e: f64,
    total_e: f64,
    /// Lost datagrams inside the histogram's runs, and the run count.
    run_lost: f64,
    runs: f64,
    state: FloorState,
    last_us: Option<u64>,
    last_change_us: Option<u64>,
    reports: u64,
    changes: u64,
    congestion_skips: u64,
    losses_excused: u64,
    /// Runs and losses left out as unrepairable (cumulative).
    runs_unrepairable: u64,
    losses_unrepairable: u64,
}

impl LossEstimator {
    pub(crate) fn new() -> Self {
        Self {
            lost: 0.0,
            total: 0.0,
            lost_e: 0.0,
            total_e: 0.0,
            run_lost: 0.0,
            runs: 0.0,
            state: FloorState::Clean,
            last_us: None,
            last_change_us: None,
            reports: 0,
            changes: 0,
            congestion_skips: 0,
            losses_excused: 0,
            runs_unrepairable: 0,
            losses_unrepairable: 0,
        }
    }

    /// Fold one LOSS_REPORT in. A `congested` interval adds its datagrams to
    /// `total` but its losses to nothing (plan decision 4).
    #[cfg(test)]
    pub(crate) fn on_report(&mut self, r: &LossReport, congested: bool, now_us: u64) {
        self.on_report_excusing(r, congested, 0, u32::MAX, now_us);
    }

    /// [`Self::on_report`] with `excused` of the report's losses charged to
    /// the sender, not the path (queue overflow, local discards; see
    /// `queue_loss`). They stay in `total` (the datagrams were sent) and are
    /// left out of `lost`; the run histogram is scaled by what is left, since
    /// which runs were the excused ones is not known. A report whose every
    /// loss is excused adds no runs at all.
    ///
    /// Runs longer than `limit` (see [`split_runs`]) are left out of `lost`,
    /// `lost_e`, `runs` and `run_lost` as well: FEC cannot repair them, the
    /// resend path or the acknowledged-reference recovery does, and they must
    /// neither engage FEC nor raise its estimates. Their datagrams stay in
    /// `total`. How the two compose: which of the losses were the excused
    /// ones is not known, so they are assumed to overlap with the left-out
    /// runs as much as they can (an overflow during an outage is the likely
    /// case), and the report keeps `min(kept, lost - excused)`: it drops the
    /// larger of the two exclusions, not their sum. Each rule alone excludes
    /// no more than its own evidence, and together they never exclude more
    /// than the larger, so the sum of two uncertain estimates cannot swallow
    /// a repairable loss.
    pub(crate) fn on_report_excusing(
        &mut self,
        r: &LossReport,
        congested: bool,
        excused: u16,
        limit: u32,
        now_us: u64,
    ) {
        let lost = f64::from(r.lost_pre_fec);
        let n = f64::from(r.received) + lost;
        self.decay(n, now_us);
        self.total += n;
        self.total_e += n;
        if congested {
            if r.lost_pre_fec > 0 {
                self.congestion_skips += 1;
            }
        } else {
            let excused = excused.min(r.lost_pre_fec);
            let split = split_runs(u32::from(r.lost_pre_fec), &r.loss_run_hist, limit);
            let kept = split.kept.min(f64::from(r.lost_pre_fec - excused));
            self.lost += kept;
            self.lost_e += kept;
            self.losses_excused += u64::from(excused);
            self.runs_unrepairable += split.runs_unrepairable;
            self.losses_unrepairable += split.losses_unrepairable;
            // The histogram of what is left: the runs under the limit, scaled
            // by the share of their losses that the excuse did not take.
            let share = if split.kept > 0.0 {
                kept / split.kept
            } else if lost > 0.0 {
                0.0
            } else {
                1.0
            };
            for (count, rep) in split.runs.iter().zip(RUN_REPRESENTATIVES) {
                self.runs += count * share;
                self.run_lost += count * rep * share;
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

    /// The decay factor for `n` datagrams over `dt` us: the count term, or the
    /// time term held up by the minimum mass, whichever forgets more.
    fn decay_factor(&self, n: f64, dt: u64) -> f64 {
        let d_count = (-n / FEC_ADAPT_WINDOW_DATAGRAMS).exp();
        let d_time = (-(dt as f64) / FEC_ADAPT_TAU_US as f64).exp();
        let d_floor = if self.total > 0.0 {
            (FEC_ADAPT_MIN_MASS / self.total).min(1.0)
        } else {
            1.0
        };
        d_count.min(d_time.max(d_floor))
    }

    fn decay(&mut self, n: f64, now_us: u64) {
        let dt = self.last_us.map_or(0, |l| now_us.saturating_sub(l));
        self.last_us = Some(now_us.max(self.last_us.unwrap_or(0)));
        let d = self.decay_factor(n, dt);
        self.lost *= d;
        self.total *= d;
        let d_e = (-n / FEC_ADAPT_WINDOW_DATAGRAMS)
            .exp()
            .min((-(dt as f64) / FEC_ENGAGE_TAU_US as f64).exp());
        self.lost_e *= d_e;
        self.total_e *= d_e;
        self.run_lost *= d;
        self.runs *= d;
    }

    fn evaluate(&mut self, now_us: u64) {
        let (l, n) = (self.lost, self.total);
        match self.state {
            FloorState::Clean => {
                if engage_shown(self.lost_e, self.total_e) {
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

    #[cfg(test)]
    pub(crate) fn lost(&self) -> f64 {
        self.lost
    }
    #[cfg(test)]
    pub(crate) fn total(&self) -> f64 {
        self.total
    }
    pub(crate) fn state(&self) -> FloorState {
        self.state
    }
    #[cfg(test)]
    pub(crate) fn reports_seen(&self) -> u64 {
        self.reports
    }
    pub(crate) fn state_changes(&self) -> u64 {
        self.changes
    }
    pub(crate) fn congestion_skips(&self) -> u64 {
        self.congestion_skips
    }
    /// Runs, and the losses in them, left out as longer than FEC can repair.
    pub(crate) fn unrepairable(&self) -> (u64, u64) {
        (self.runs_unrepairable, self.losses_unrepairable)
    }
    /// Losses charged to the sender and kept out of `lost`.
    #[cfg(test)]
    pub(crate) fn losses_excused(&self) -> u64 {
        self.losses_excused
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
    /// The run limit the last report was read under.
    last_limit: u32,
    limit_hold: HeldLimit,
    /// Source datagrams sealed per busy 20 ms, mean over the last 1 to 2 s.
    sent: SourceRate,
}

impl FecAdapt {
    pub(crate) fn new() -> Self {
        Self::with_frame_notch(true)
    }

    /// `frame = false` never enters the frame notch: the control arm of the
    /// frame-notch scenes (a test switch, not a configuration).
    pub(crate) fn with_frame_notch(frame: bool) -> Self {
        Self {
            est: LossEstimator::new(),
            ladder: Ladder::with_frame_notch(frame),
            sizer: RSizer::new(),
            trim: DeclinedTrim::new(),
            was_engaged: false,
            blocks_sized: 0,
            saturated: 0,
            changes: 0,
            last_applied: None,
            last_limit: 0,
            limit_hold: HeldLimit::default(),
            sent: SourceRate::default(),
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
    #[cfg(test)]
    pub(crate) fn on_report(&mut self, r: &LossReport, standing_queue: bool, now_us: u64) {
        self.est.on_report(r, standing_queue, now_us);
    }

    /// [`Self::on_report`], `excused` of the report's losses being the
    /// stream's own queue overflow or quinn's local discards (`queue_loss`).
    ///
    /// Runs longer than FEC could repair at the block size the rate implies
    /// are left out of the estimate ([`LossEstimator::on_report_excusing`]).
    pub(crate) fn on_report_excusing(
        &mut self,
        r: &LossReport,
        standing_queue: bool,
        excused: u16,
        rate_bps: u64,
        max_dg: usize,
        now_us: u64,
    ) {
        let limit = self.run_limit(rate_bps, max_dg, now_us);
        self.last_limit = limit;
        self.est
            .on_report_excusing(r, standing_queue, excused, limit, now_us);
    }

    /// A source datagram (a protected first send: not a pad, a
    /// retransmission or a control frame) was sealed at `now_us`: the sender's
    /// own rate, which bounds the block width the estimate alone does not (a
    /// desktop sending 5 Mbit/s under a held 100 Mbit/s estimate has a block of
    /// 26 sources, not 208). O(1).
    pub(crate) fn note_sent(&mut self, now_us: u64) {
        self.sent.note(now_us);
    }

    /// Source datagrams the rate tracker holds (tests).
    #[cfg(test)]
    pub(crate) fn sent_for_test(&self) -> u64 {
        self.sent.held_for_test()
    }

    /// A new session: the previous client's send rate and held limit are not
    /// the new one's. (The estimate itself is not reset here.)
    pub(crate) fn reset_session(&mut self) {
        self.sent = SourceRate::default();
        self.limit_hold = HeldLimit::default();
    }

    /// The longest loss run FEC could be asked to repair at this rate: twice
    /// the repair cap of the widest block the ladder can build (see
    /// [`widest_block_sources`]), not of the notch it is in. A burst widens
    /// the block, and measuring the burst needs the burst's runs in the
    /// estimate. The width follows the rate the stream really sends: at most
    /// the mean of source datagrams per busy 20 ms over the last 1 to 2 s.
    /// Held at its peak for [`RUN_LIMIT_HOLD_US`].
    fn run_limit(&mut self, rate_bps: u64, max_dg: usize, now_us: u64) -> u32 {
        let k = widest_block_sources(rate_bps, max_dg).min(self.sent.mean(now_us).max(1));
        self.limit_hold.hold(repairable_run_limit(k), now_us)
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
        fps: u32,
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
        let span = self
            .ladder
            .step(self.est.b_hat(), rate_bps, max_dg, fps, now_us);
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

    /// Rows the trim added to the last block's `r` (measurement).
    pub(crate) fn last_trim(&self) -> u8 {
        self.sizer.last_trim()
    }

    /// The rule's rows of the last block's `r`, before trim and pacing.
    pub(crate) fn last_base(&self) -> u8 {
        self.sizer.last_base()
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
            frame: self.ladder.notch() == Notch::Frame,
            notch_changes: self.ladder.notch_changes(),
            frame_entries: self.ladder.frame_entries(),
            r_at_k8: r_at(8),
            r_at_k64: r_at(64),
            changes: self.changes,
            floor_changes: self.est.state_changes(),
            congestion_skips: self.est.congestion_skips(),
            runs_unrepairable: self.est.unrepairable().0,
            losses_unrepairable: self.est.unrepairable().1,
            run_limit: u64::from(self.last_limit),
            trim_steps: self.trim.steps(),
            blocks_sized: self.blocks_sized,
            saturated: self.saturated,
            ..FecAdaptProbe::default()
        }
    }

    #[cfg(test)]
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
    fn the_poisson_tail_is_the_exact_one() {
        // P(X >= 3 | 1) = 1 - e^-1 (1 + 1 + 1/2); P(X >= 0) = 1.
        let exact = 1.0 - (-1.0f64).exp() * 2.5;
        assert!((poisson_upper_tail(3.0, 1.0) - exact).abs() < 1e-12);
        assert_eq!(poisson_upper_tail(0.0, 7.0), 1.0);
        // Between integers it is between their tails.
        let (a, b) = (poisson_upper_tail(3.0, 1.0), poisson_upper_tail(4.0, 1.0));
        let m = poisson_upper_tail(3.5, 1.0);
        assert!(b < m && m < a && (m - (a + b) / 2.0).abs() < 1e-12);
    }

    /// Fewest whole losses that engage a young estimator with `n` clean-ish
    /// datagrams, in one report.
    fn losses_to_engage(n: u16) -> u16 {
        (0..n)
            .find(|&l| {
                let mut e = LossEstimator::new();
                e.on_report(&rep(n - l, l), false, 0);
                e.state() == FloorState::Engaged
            })
            .unwrap()
    }

    /// The bar is a count of losses the traffic makes unlikely at the floor:
    /// it grows with the datagrams in the window (rev 7, section 16).
    #[test]
    fn the_engage_bar_follows_the_volume_of_traffic() {
        let bars: Vec<u16> = [200u16, 500, 900, 2_000, 5_000, 10_000]
            .iter()
            .map(|&n| losses_to_engage(n))
            .collect();
        assert!(bars.windows(2).all(|w| w[0] <= w[1]), "{bars:?}");
        // 2 Mbit/s's 800 to 900 datagrams: 4 losses, not the 6 the old
        // bound asked; the minimum of 3 still holds at any volume.
        assert_eq!(bars[2], 4, "{bars:?}");
        assert_eq!(bars[0], 3, "{bars:?}");
        assert!(bars[5] >= 15, "{bars:?}");
        // Three losses in 1 000 datagrams are 8 % likely at the floor.
        assert_eq!(losses_to_engage(1_000), 4);
    }

    /// A step after a long clean spell is not diluted by it: at 2 Mbit/s the
    /// slow window holds 5 000 datagrams (30 s), the engage window 800.
    #[test]
    fn a_late_step_at_a_low_rate_engages_as_fast_as_an_early_one() {
        let engage_after_step_s = |clean_s: u64| {
            let mut rng = Rng(0x5eed);
            let mut e = LossEstimator::new();
            let mut t = 0u64;
            // 40 datagrams per 250 ms report (160/s).
            while t < clean_s * 1_000_000 {
                e.on_report(&rep(40, 0), false, t);
                t += 250_000;
            }
            assert_eq!(e.state(), FloorState::Clean);
            let t0 = t;
            for _ in 0..40 {
                let lo = (0..40).filter(|_| rng.next() < 0.05).count() as u16;
                e.on_report(&rep(40 - lo, lo), false, t);
                if e.state() == FloorState::Engaged {
                    return (t - t0) as f64 / 1e6;
                }
                t += 250_000;
            }
            panic!("never engaged after {clean_s} s clean");
        };
        let (early, late) = (engage_after_step_s(5), engage_after_step_s(60));
        assert!(late <= 2.0, "late step took {late} s");
        assert!(late <= early + 0.75, "early {early} s, late {late} s");
    }

    /// Zero loss after a 5 % episode, at `dg_per_s` datagrams/s with reports
    /// every 250 ms or 128 datagrams: seconds until the state is Clean again
    /// (None within `limit_s`).
    fn release_time_s(dg_per_s: f64, limit_s: f64) -> Option<f64> {
        let mut e = LossEstimator::new();
        let per_report = (dg_per_s * 0.25).clamp(1.0, 128.0);
        let step_us = (per_report / dg_per_s * 1e6) as u64;
        let mut t = 0u64;
        // The episode: 5 % for 10 s.
        while t < 10_000_000 {
            let n = per_report as u16;
            e.on_report(
                &rep(
                    n - (n as f64 * 0.05).ceil() as u16,
                    (n as f64 * 0.05).ceil() as u16,
                ),
                false,
                t,
            );
            t += step_us;
        }
        assert_eq!(
            e.state(),
            FloorState::Engaged,
            "premise: engaged at {dg_per_s}/s"
        );
        let start = t;
        while ((t - start) as f64) < limit_s * 1e6 {
            e.on_report(&rep(per_report as u16, 0), false, t);
            if e.state() == FloorState::Clean {
                return Some((t - start) as f64 / 1e6);
            }
            t += step_us;
        }
        None
    }

    /// Release at zero loss is reachable at every rate, within a bound named
    /// from the minimum mass: ln(L0 / L_release) e-folds, each of about
    /// MIN_MASS / rate seconds at low rates (tau at high rates), plus the
    /// dwell. L0 is at most the episode's mass (5 % of 5 000) and the release
    /// needs L under about 1, so 6 e-folds bound it with margin.
    #[test]
    fn release_is_reachable_at_every_rate_within_a_bound_from_the_minimum_mass() {
        for rate in [50.0, 100.0, 180.0, 260.0, 1_000.0, 10_000.0] {
            let e_fold_s = (FEC_ADAPT_MIN_MASS / rate).max(FEC_ADAPT_TAU_US as f64 / 1e6);
            let bound = 6.0 * e_fold_s + FEC_ADAPT_MIN_DWELL_US as f64 / 1e6;
            let got = release_time_s(rate, bound * 1.5);
            println!("release at {rate}/s: {got:?} (bound {bound:.1} s)");
            let got = got.unwrap_or_else(|| panic!("{rate}/s: never released within {bound:.1} s"));
            assert!(
                got <= bound,
                "{rate}/s: released after {got:.1} s, bound {bound:.1} s"
            );
        }
    }

    /// At 10 000+ datagrams/s the count term dominates and the minimum mass
    /// never binds: the decay is bit-identical to the old formula.
    #[test]
    fn at_a_high_rate_the_decay_is_what_it_was_before_the_minimum_mass() {
        let (mut e, mut old_total) = (LossEstimator::new(), 0.0f64);
        let mut last = None::<u64>;
        let mut t = 0u64;
        for i in 0..4_000u32 {
            // 1 280 datagrams per 128 ms: 10 000/s.
            t += 128_000;
            let (rx, lo) = (1_260u16, if i % 7 == 0 { 20 } else { 0 });
            let n = f64::from(rx) + f64::from(lo);
            let dt = last.map_or(0, |l| t - l);
            last = Some(t);
            let exp = (n / FEC_ADAPT_WINDOW_DATAGRAMS).max(dt as f64 / FEC_ADAPT_TAU_US as f64);
            old_total = old_total * (-exp).exp() + n;
            e.on_report(&rep(rx, lo), false, t);
            assert_eq!(e.total().to_bits(), old_total.to_bits(), "report {i}");
        }
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
    fn excused_losses_count_in_total_but_not_in_lost() {
        let mut r = rep(900, 100);
        // 25 runs of 4.
        r.loss_run_hist[3] = 25;
        // Every loss excused: the datagrams are in `total`, nothing else.
        let mut a = LossEstimator::new();
        a.on_report_excusing(&r, false, 100, u32::MAX, 0);
        assert_eq!(a.lost(), 0.0);
        assert_eq!(a.total(), 1000.0);
        assert_eq!(a.state(), FloorState::Clean);
        assert_eq!(a.losses_excused(), 100);
        assert_eq!(a.b_hat(), 1.0, "no run of an all-excused report is kept");
        // The same report unexcused engages.
        let mut b = LossEstimator::new();
        b.on_report_excusing(&r, false, 0, u32::MAX, 0);
        assert_eq!(b.state(), FloorState::Engaged);
        // Part excused: the rest counts, the runs scale with it. 100 lost of
        // which 97 excused leaves 3: not enough to engage on its own.
        let mut c = LossEstimator::new();
        c.on_report_excusing(&r, false, 97, u32::MAX, 0);
        assert_eq!(c.lost(), 3.0);
        assert_eq!(c.state(), FloorState::Clean);
        // More excused than lost is clamped to what was lost.
        let mut d = LossEstimator::new();
        d.on_report_excusing(&r, false, 500, u32::MAX, 0);
        assert_eq!(d.lost(), 0.0);
        assert_eq!(d.losses_excused(), 100);
        // A congested report skips whole, and excuses nothing it did not need to.
        let mut e = LossEstimator::new();
        e.on_report_excusing(&r, true, 40, u32::MAX, 0);
        assert_eq!(e.lost(), 0.0);
        assert_eq!(e.losses_excused(), 0);
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
        let live = a.live_params(10_000_000, 1200, 30, 5_000_000);
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

    fn hist_report(received: u16, hist: [u16; 8], lost: u16) -> LossReport {
        LossReport {
            received,
            lost_pre_fec: lost,
            loss_run_hist: hist,
            ..Default::default()
        }
    }

    /// One outage: a single run of `gap` datagrams.
    fn outage(gap: u16, received: u16) -> LossReport {
        let mut h = [0u16; 8];
        h[match gap {
            1..=4 => gap as usize - 1,
            5..=8 => 4,
            9..=16 => 5,
            17..=32 => 6,
            _ => 7,
        }] = 1;
        hist_report(received, h, gap)
    }

    /// `rate_bps` of 1200 B datagrams from `from_us` to `to_us`, noted as the
    /// bridge does: the sender's peak per 20 ms window is what `run_limit`
    /// reads. Times only go forward, as in the bridge.
    fn sending(a: &mut FecAdapt, rate_bps: u64, from_us: u64, to_us: u64) {
        let gap_us = ((8.0 * 1200.0 * 1e6 / rate_bps as f64) as u64).max(1);
        let mut t = from_us;
        while t < to_us {
            a.note_sent(t);
            t += gap_us;
        }
    }

    #[test]
    fn the_run_limit_is_twice_the_repair_cap_of_the_block_with_a_floor() {
        for (k, want) in [
            (1u32, 4u32),
            (4, 4),
            (13, 8),
            (21, 12),
            (63, 32),
            (128, 64),
            (223, 64),
        ] {
            assert_eq!(repairable_run_limit(k), want, "k {k}");
            assert!(repairable_run_limit(k) <= 64);
        }
        // Through the estimate, the stream sending at it (1200 B): the sources
        // of a 20 ms window.
        for (rate, want) in [
            (2_000_000u64, 4u32), // k = 4
            (10_000_000, 12),     // k = 21
            (30_000_000, 32),     // k = 63
            (100_000_000, 64),    // k = 208
        ] {
            let mut a = FecAdapt::new();
            sending(&mut a, rate, 1_000_000, 2_000_000);
            assert_eq!(a.run_limit(rate, 1200, 2_000_000), want, "{rate} bit/s");
        }
    }

    #[test]
    fn a_stream_sending_less_than_the_estimate_is_limited_by_what_it_sends() {
        // 5 Mbit/s sent under a held 100 Mbit/s estimate: a block of about 10
        // sources, so a limit of 6 to 8, not the 64 the estimate alone gives.
        let mut a = FecAdapt::new();
        sending(&mut a, 5_000_000, 1_000_000, 2_000_000);
        let l = a.run_limit(100_000_000, 1200, 2_000_000);
        assert!((4..=8).contains(&l), "limit {l}");
        // The estimate below the stream still bounds it.
        let mut b = FecAdapt::new();
        sending(&mut b, 100_000_000, 1_000_000, 2_000_000);
        assert_eq!(b.run_limit(5_000_000, 1200, 2_000_000), l);
        // Nothing sent yet: the floor.
        let mut c = FecAdapt::new();
        assert_eq!(c.run_limit(100_000_000, 1200, 2_000_000), 4);
    }

    #[test]
    fn the_limit_holds_its_peak_for_the_hold() {
        let h = RUN_LIMIT_HOLD_US;
        let mut l = HeldLimit::default();
        assert_eq!(l.hold(12, 0), 12);
        // Falls (the estimate was cut) and wobbles: held.
        assert_eq!(l.hold(8, h / 10), 12);
        assert_eq!(l.hold(9, h / 2), 12);
        assert_eq!(l.hold(8, h - 1), 12);
        // Rises: follows at once, and restarts the hold.
        assert_eq!(l.hold(16, h), 16);
        assert_eq!(l.hold(8, 2 * h - 1), 16);
        // The hold passes: it falls to what the rate implies now.
        assert_eq!(l.hold(8, 2 * h), 8);
        assert_eq!(l.hold(8, 2 * h + 1), 8);
        // Long enough to cover a forced close, a report and a round trip.
        assert!(h >= 1_000_000 + 250_000 + 500_000);
        // ... and short enough not to carry a burst past the mean's own 2 s.
        assert!(h <= 5_000_000);
    }

    #[test]
    fn a_cut_of_the_rate_near_a_bucket_edge_does_not_reclassify_a_run() {
        // At 10 Mbit/s the limit is 12 (a run of 9-16 is kept), at 6 Mbit/s it
        // is 8 (it is left out). A loss-driven cut of the estimate right after
        // must not move the same run to the other side.
        let mut a = FecAdapt::new();
        sending(&mut a, 10_000_000, 1_000_000, 2_000_000);
        let before = a.run_limit(10_000_000, 1200, 2_000_000);
        let cut = a.run_limit(6_000_000, 1200, 2_500_000);
        assert_eq!(before, 12);
        assert_eq!(cut, before, "held");
        // Without the hold the same two calls differ.
        assert_ne!(repairable_run_limit(21), repairable_run_limit(13));
    }

    #[test]
    fn the_source_rate_is_the_mean_of_the_busy_windows() {
        let mut p = SourceRate::default();
        assert_eq!(p.mean(0), 0);
        // Windows of 30 and 10 datagrams alternate, with idle windows between:
        // the mean is over the windows that held any (20), not the peak (30).
        let w = SourceRate::WINDOW_US;
        for i in 0..20u64 {
            let at = 1_000_000 + i * 3 * w;
            let n = if i % 2 == 0 { 30 } else { 10 };
            for j in 0..n {
                p.note(at + j * 10);
            }
        }
        assert_eq!(p.mean(1_000_000 + 60 * w + 1), 20);
        // Silence of any length leaves it where it was.
        assert_eq!(p.mean(1_000_000 + 60 * w + 3_000_000), 20);
        assert_eq!(p.mean(u64::MAX / 2), 20);
    }

    #[test]
    fn one_burst_moves_the_mean_by_its_share_not_to_its_peak() {
        // 16 per 20 ms, steadily, for 2 s; then one window of 200.
        let mut p = SourceRate::default();
        let w = SourceRate::WINDOW_US;
        let mut t = 1_000_000;
        for _ in 0..100 {
            for j in 0..16 {
                p.note(t + j * 100);
            }
            t += w;
        }
        assert_eq!(p.mean(t), 16);
        for j in 0..200 {
            p.note(t + j * 10);
        }
        t += w;
        let m = p.mean(t + 1);
        assert!((16..=24).contains(&m), "mean {m}, not the peak 200");
    }

    #[test]
    fn an_outage_keeps_the_rate_and_the_burst_after_it_moves_it_by_its_share() {
        let mut p = SourceRate::default();
        let w = SourceRate::WINDOW_US;
        let mut t = 1_000_000;
        for _ in 0..100 {
            for j in 0..17 {
                p.note(t + j * 100);
            }
            t += w;
        }
        // Three seconds of silence, then the recovery burst fills one window.
        t += 3_000_000;
        for j in 0..40 {
            p.note(t + j * 10);
        }
        let m = p.mean(t + w + 1);
        assert!((17..=18).contains(&m), "mean {m}, not the burst's 40");
    }

    #[test]
    fn the_rate_forgets_by_busy_windows() {
        let mut p = SourceRate::default();
        let w = SourceRate::WINDOW_US;
        let mut t = 1_000_000;
        for n in [40u64, 10] {
            for _ in 0..2 * SourceRate::EPOCH_WINDOWS {
                for j in 0..n {
                    p.note(t + j * 10);
                }
                t += w;
            }
        }
        assert_eq!(p.mean(t + 1), 10, "two epochs of 10 replaced the 40");
    }

    /// A stream that is not perfectly paced: bursts of 5 to 27 datagrams per
    /// 20 ms window around a mean of 16 (781 datagrams/s, about 7.5 Mbit/s of
    /// 1200 B), with a rare burst of 150. Returns the limit it gives under a
    /// held 100 Mbit/s estimate.
    fn bursty_limit() -> u32 {
        let mut a = FecAdapt::new();
        let w = SourceRate::WINDOW_US;
        let mut t = 1_000_000u64;
        let mut x = 7u64;
        let end = t + 4_000_000;
        let mut i = 0;
        while t < end {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let n = if i % 97 == 50 {
                150
            } else {
                5 + (x >> 33) % 23
            };
            for j in 0..n {
                a.note_sent(t + j * 20);
            }
            t += w;
            i += 1;
        }
        a.run_limit(100_000_000, 1200, t)
    }

    #[test]
    fn a_bursty_application_limited_stream_gets_the_limit_of_its_mean() {
        // Mean about 17: k 17, r_cap 5, limit 10. A peak would read the burst
        // of 150 (limit 64); a 16 or 24 datagram outage must stay out.
        let l = bursty_limit();
        assert!((8..=12).contains(&l), "limit {l}");
    }

    #[test]
    fn a_session_reset_forgets_the_rate_and_the_held_limit() {
        let mut a = FecAdapt::new();
        sending(&mut a, 100_000_000, 1_000_000, 2_000_000);
        assert_eq!(a.run_limit(100_000_000, 1200, 2_000_000), 64);
        a.reset_session();
        // The new client has sent nothing yet: the floor, at once (not 64
        // held, not 64 from the old rate).
        assert_eq!(a.run_limit(100_000_000, 1200, 2_100_000), 4);
    }

    /// Datagrams of an outage: the stream's window in flight when the path
    /// goes, `2 x R x (RTT + 5 ms)` with `R` the datagram rate.
    fn outage_gap(rate_bps: u64, rtt_ms: u64) -> u16 {
        let dg_per_s = rate_bps as f64 / (8.0 * 1200.0);
        (2.0 * dg_per_s * (rtt_ms as f64 + 5.0) / 1000.0).ceil() as u16
    }

    #[test]
    fn an_outage_alone_does_not_engage_or_move_any_estimate_at_any_rate() {
        // The grid: the outage's gap follows from the rate and the RTT, as in
        // the measured scenes (2M 16-21, 10M ~70, 30M ~200, 100M ~870 at
        // 50 ms). Outages over the limit are the unrepairable ones: they must
        // leave the estimator where it was. (Under it, the run is a burst FEC
        // is for, and is read as one.)
        let mut checked = 0;
        for rate in [2_000_000u64, 10_000_000, 30_000_000, 100_000_000] {
            for rtt in [5u64, 10, 20, 50, 100] {
                let gap = outage_gap(rate, rtt);
                let mut a = FecAdapt::new();
                let mut t = 0u64;
                sending(&mut a, rate, 0, 750_000);
                let limit = a.run_limit(rate, 1200, 750_000);
                if u32::from(gap) <= limit {
                    // A short outage is a burst FEC can repair: not asserted.
                    continue;
                }
                checked += 1;
                for i in 0..40 {
                    t += 250_000;
                    sending(&mut a, rate, t - 250_000, t);
                    // The outage's report, then clean ones.
                    let r = if i == 3 { outage(gap, 20) } else { rep(400, 0) };
                    a.on_report_excusing(&r, false, 0, rate, 1200, t);
                }
                let what = format!("{rate} bit/s, {rtt} ms, gap {gap}, limit {limit}");
                let e = a.estimator();
                assert_eq!(e.state(), FloorState::Clean, "{what}");
                assert_eq!(e.state_changes(), 0, "{what}");
                assert_eq!(e.p_hat(), 0.0, "{what}");
                assert_eq!(e.b_hat(), 1.0, "{what}");
                assert_eq!(e.unrepairable(), (1, u64::from(gap)), "{what}");
                assert!(a.live_params(rate, 1200, 30, t).is_none(), "{what}");
                let p = a.probe();
                assert_eq!(
                    (p.runs_unrepairable, p.losses_unrepairable),
                    (1, u64::from(gap)),
                    "{what}"
                );
            }
        }
        // The grid is not vacuous: every cell is an outage over the limit.
        assert_eq!(checked, 20, "cells that reached the assertions");
    }

    #[test]
    fn the_same_outage_engages_without_the_limit() {
        // The control: the limit is what keeps the outage out, not the
        // thresholds. (Runs over 32 are left out at any limit, so the control
        // is the outages the histogram can place at or under 32.)
        for (gap, rate_recv) in [(9u16, 20u16), (16, 20), (21, 20), (32, 20)] {
            let mut e = LossEstimator::new();
            e.on_report_excusing(&outage(gap, rate_recv), false, 0, u32::MAX, 0);
            assert_eq!(e.state(), FloorState::Engaged, "gap {gap}");
            assert!(e.p_hat() > 0.2 && e.b_hat() > 1.5);
            let mut e = LossEstimator::new();
            e.on_report_excusing(&outage(gap, rate_recv), false, 0, 4, 0);
            assert_eq!(e.state(), FloorState::Clean, "gap {gap} at limit 4");
        }
    }

    #[test]
    fn a_report_mixing_short_and_long_runs_keeps_the_short_ones() {
        // 6 single losses, one run of 3, and one outage of 870, over 4000.
        let mut h = [0u16; 8];
        h[0] = 6;
        h[2] = 1;
        h[7] = 1;
        let r = hist_report(1000, h, 6 + 3 + 870);
        let mut e = LossEstimator::new();
        e.on_report_excusing(&r, false, 0, 4, 0);
        // kept = min(879 - 33, 6 x 1 + 1 x 3) = 9.
        assert_eq!(e.lost(), 9.0);
        assert_eq!(e.total(), 1879.0);
        assert_eq!(e.unrepairable(), (1, 870));
        // b_hat reads the 7 kept runs (6 x 1 + 1 x 3 over 7), not the outage.
        assert!((e.b_hat() - 9.0 / 7.0).abs() < 1e-9, "{}", e.b_hat());
        // It still engages on the short losses alone (9 of 1879 > the floor).
        assert_eq!(e.state(), FloorState::Engaged);
    }

    #[test]
    fn a_run_at_the_limit_counts_and_one_bucket_above_does_not() {
        // Limit 4: a run of 4 is repairable, the bucket 5-8 starts above it.
        let mut at = LossEstimator::new();
        at.on_report_excusing(&outage(4, 2000), false, 0, 4, 0);
        assert_eq!((at.lost(), at.unrepairable()), (4.0, (0, 0)));
        let mut above = LossEstimator::new();
        above.on_report_excusing(&outage(5, 2000), false, 0, 4, 0);
        assert_eq!((above.lost(), above.unrepairable()), (0.0, (1, 5)));
        // A bucket that starts at or under the limit is kept whole, even if
        // part of it is above: 5-8 at limit 6 or 8.
        for limit in [5, 6, 8] {
            let mut e = LossEstimator::new();
            e.on_report_excusing(&outage(7, 2000), false, 0, limit, 0);
            assert_eq!((e.lost(), e.unrepairable()), (7.0, (0, 0)), "limit {limit}");
        }
        // 9-16 starts above 8.
        let mut e = LossEstimator::new();
        e.on_report_excusing(&outage(12, 2000), false, 0, 8, 0);
        assert_eq!((e.lost(), e.unrepairable()), (0.0, (1, 12)));
    }

    #[test]
    fn the_bound_on_what_is_kept_never_goes_below_zero_or_past_the_losses() {
        // The excluded minimum can exceed the report's losses if a stale
        // histogram arrives: kept clamps at 0.
        let mut h = [0u16; 8];
        h[7] = 3;
        let s = split_runs(10, &h, 4);
        assert_eq!(
            (s.kept, s.runs_unrepairable, s.losses_unrepairable),
            (0.0, 3, 10)
        );
        // The >32 bucket is left out at any limit, so a report with two long
        // runs does not keep one of them (70 + 70 at limit 42 kept 42 losses
        // and engaged FEC).
        let mut h = [0u16; 8];
        h[7] = 2;
        for limit in [4, 32, 42, 64] {
            let s = split_runs(140, &h, limit);
            assert_eq!(
                (
                    s.kept,
                    s.runs_unrepairable,
                    s.losses_unrepairable,
                    s.runs[7]
                ),
                (0.0, 2, 140, 0.0),
                "limit {limit}"
            );
        }
        // A run of 17-32 beside a >32 run: the run of at most 32 is kept at a
        // limit that admits it, whatever the total says.
        let mut h = [0u16; 8];
        h[6] = 1;
        h[7] = 1;
        let s = split_runs(100, &h, 40);
        assert_eq!((s.kept, s.runs_unrepairable, s.runs[7]), (32.0, 1, 0.0));
        assert_eq!(s.losses_unrepairable, 68);
        // The left-out minimum is subtracted: a run of 5-8 and one of 9-16
        // over 14 losses, limit 8. The run of 9-16 holds at least 9, so at
        // most 5 are kept (the 5-8 bucket alone would allow 8).
        let mut h = [0u16; 8];
        h[4] = 1;
        h[5] = 1;
        let s = split_runs(14, &h, 8);
        assert_eq!(
            (s.kept, s.runs_unrepairable, s.losses_unrepairable),
            (5.0, 1, 9)
        );
        // Nothing left out: all losses stay, whatever the histogram says.
        let s = split_runs(50, &[0; 8], 4);
        assert_eq!(s.kept, 50.0);
    }

    #[test]
    fn excused_and_unrepairable_losses_are_dropped_once_not_added() {
        // 5 singles + an outage of 100, 80 of the losses excused as queue
        // overflow: kept = min(5, 105 - 80) = 5; not 5 - 80.
        let mut h = [0u16; 8];
        h[0] = 5;
        h[7] = 1;
        let r = hist_report(2000, h, 105);
        let mut e = LossEstimator::new();
        e.on_report_excusing(&r, false, 80, 4, 0);
        assert_eq!(e.lost(), 5.0);
        // 100 excused: the excuse alone leaves 5 as well; kept 5.
        let mut e = LossEstimator::new();
        e.on_report_excusing(&r, false, 103, 4, 0);
        assert_eq!(e.lost(), 2.0, "min(5, 105 - 103)");
        assert_eq!(e.losses_excused(), 103);
    }
}
