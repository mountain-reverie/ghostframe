//! FEC v2 rev 6, R6-e E4: the span ladder and `r_for(k)`.
//!
//! Pure and sans-IO. The tail is an exact dynamic program over the block's
//! positions (i.i.d. or two-state Gilbert-Elliott, loss 1 in the bad state,
//! stationary start); `r_for` is memoised per quantised `(p, b)` and `k`, so a
//! call is a table lookup once warm. Checked against the Python oracle through
//! `testdata/adapt_oracle_vectors.csv` (`sim-rev6/adapt_oracle.py`).

use std::collections::HashMap;

use super::block_table::CloseReason;

/// Largest repair count per block.
pub(crate) const FEC_ADAPT_R_MAX: u8 = 32;
/// Target probability that a block loses more than `r` of its `k + r`.
pub(crate) const FEC_ADAPT_TARGET: f64 = 1e-3;
/// Burst estimate above which the wide notch is entered (hysteresis up).
pub(crate) const FEC_ADAPT_NOTCH_UP: f64 = 1.5;
/// Burst estimate below which the narrow notch is re-entered.
pub(crate) const FEC_ADAPT_NOTCH_DOWN: f64 = 1.25;
/// One notch change at most per this long.
pub(crate) const FEC_ADAPT_NOTCH_DWELL_US: u64 = 1_000_000;
/// Closed blocks the declined-share trim looks at.
pub(crate) const FEC_ADAPT_TRIM_WINDOW: usize = 256;
/// Burst estimate above which the frame notch is entered. A midpoint, **not a
/// measured crossing**: the simulation (FINDINGS section 12) measured the
/// per-block rule equal to the single block at mean burst 5 and ahead in every
/// seed at 8, and did not run 6 or 7. The harness (section 13) found the notch
/// ahead of the single block at bursts 8, 16 and 32 and level at 3, and did not
/// run 4 to 7 either. 6.0 sits between 5 and 8.
pub(crate) const FEC_FRAME_B_ENTER: f64 = 6.0;
/// Burst estimate below which the frame notch is left: 2 under the entry, the
/// same kind of midpoint, not a measured crossing.
pub(crate) const FEC_FRAME_B_EXIT: f64 = 4.0;
/// The frame notch is entered only when a frame, with `FRAME_RHO` of repair, no
/// longer fits one 255-symbol block (`fill > 1`), and left when it fits again
/// with a margin (`fill <= FRAME_FILL_EXIT`: a rate that wobbles around one
/// block does not flap the notch). Below that the narrow and wide notches are
/// what they were: the gain was measured at 150 and 200 Mbit/s (fill 1.6 to 3),
/// and at 100 Mbit/s the simulation found none.
const FRAME_FILL_ENTER: f64 = 1.0;
const FRAME_FILL_EXIT: f64 = 0.9;
/// The repair overhead the frame's fill is sized for: `frame_bundles x (1 +
/// rho) / 255`, the 255 being the GF(2^8) limit on `k + r`.
const FRAME_RHO: f64 = 0.2;
/// Below this burst estimate the loss is modelled as i.i.d.
const IID_BURST: f64 = 1.05;
/// Block duration cap of the ladder. Lower than the knob's `T_MS_MAX` (50) on
/// purpose: E1 found T = 20 loses nothing measurable against 25-33 ms and cuts
/// tile recovery p95, so the wider advertised range serves the frame notch,
/// not a retune of the narrow and wide notches.
const T_MAX_MS: u32 = 20;
const T_MIN_MS: u32 = 5;
const K_MIN: u32 = 64;
const K_MAX: u32 = 223;

/// Overhead cap of a block of `k` sources: `max(1, ceil(k / 4))`, for every
/// `k` (a lift for `k < 4` makes `r` non-monotone in `k`), bounded by 32 and
/// by `255 - k`.
pub(crate) fn r_cap(k: u32) -> u32 {
    k.div_ceil(4)
        .max(1)
        .min(u32::from(FEC_ADAPT_R_MAX))
        .min(255u32.saturating_sub(k).max(1))
}

/// P(more than `r` of `n` consecutive datagrams are lost). `b <= 1.05` is
/// i.i.d. with loss `p`; otherwise Gilbert-Elliott with mean bad run `b`.
/// Cost O(n * r).
pub(crate) fn tail_probability(n: usize, r: usize, p: f64, b: f64) -> f64 {
    if p <= 0.0 || n <= r {
        return 0.0;
    }
    let p = p.min(1.0);
    // dist[j] = P(j lost so far), j in 0..=r, and dist[r + 1] = P(more than r).
    let w = r + 2;
    if b <= IID_BURST {
        let mut d = vec![0.0f64; w];
        d[0] = 1.0;
        for _ in 0..n {
            let mut nd = vec![0.0f64; w];
            for (j, &x) in d.iter().enumerate() {
                nd[j] += x * (1.0 - p);
                nd[(j + 1).min(r + 1)] += x * p;
            }
            d = nd;
        }
        return d[r + 1];
    }
    let p_bg = 1.0 / b;
    let p_gb = (p * p_bg / (1.0 - p)).min(1.0);
    // g = good now, s = bad now; position 0 takes the stationary state.
    let mut g = vec![0.0f64; w];
    let mut s = vec![0.0f64; w];
    g[0] = 1.0 - p;
    s[1.min(r + 1)] = p;
    for _ in 1..n {
        let mut ng = vec![0.0f64; w];
        let mut ns = vec![0.0f64; w];
        for j in 0..w {
            let (gj, sj) = (g[j], s[j]);
            ng[j] += gj * (1.0 - p_gb) + sj * p_bg;
            ns[(j + 1).min(r + 1)] += gj * p_gb + sj * (1.0 - p_bg);
        }
        g = ng;
        s = ns;
    }
    g[r + 1] + s[r + 1]
}

/// The repair count of a block and whether the target was reachable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RDecision {
    pub(crate) r: u8,
    /// The 1e-3 target is not reachable inside the cap; `r` is the cap. A
    /// state of the link (bursty loss), not a controller failure.
    pub(crate) saturated: bool,
}

/// Smallest `r >= 1` with `tail(k + r, r) <= target` under the cap; the cap
/// (`saturated`) when none is. Uncached; see [`RSizer`].
pub(crate) fn r_for_target(k: u32, p: f64, b: f64, target: f64) -> RDecision {
    let cap = r_cap(k);
    for r in 1..=cap {
        if tail_probability((k + r) as usize, r as usize, p, b) <= target {
            return RDecision {
                r: r as u8,
                saturated: false,
            };
        }
    }
    RDecision {
        r: cap as u8,
        saturated: true,
    }
}

/// Memoised `r_for`. The memo is cleared when the quantised `(p, b)` moves
/// and evaluates at the `(p, b)` that opened the cell (0.5 % in `p`, 0.05 in
/// `b`), so a lookup is O(1) once warm.
#[derive(Clone, Debug)]
pub(crate) struct RSizer {
    key: Option<(i32, i32)>,
    p: f64,
    b: f64,
    memo: HashMap<u32, RDecision>,
    /// Per `k`: the `r` last handed out (0 = none) and when it last fell.
    paced: [(u8, u64); 256],
    /// Rows the trim added to the last `r_for` (after the ceiling).
    last_trim: u8,
    /// The rule's rows of the last `r_for`, before the trim.
    last_base: u8,
}

impl Default for RSizer {
    fn default() -> Self {
        Self {
            key: None,
            p: 0.0,
            b: 1.0,
            memo: HashMap::new(),
            paced: [(0, 0); 256],
            last_trim: 0,
            last_base: 0,
        }
    }
}

impl RSizer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Rows the declined-share trim added to the last `r_for` (measurement).
    pub(crate) fn last_trim(&self) -> u8 {
        self.last_trim
    }

    /// The rule's rows of the last `r_for`, before the trim (measurement).
    pub(crate) fn last_base(&self) -> u8 {
        self.last_base
    }

    /// Forget the step-limit memory (a fresh engagement starts from the
    /// untouched sizing, not from an older, higher `r`).
    pub(crate) fn reset_pacing(&mut self) {
        self.paced = [(0, 0); 256];
    }

    /// [`Self::r_for`] under the step limit (plan decision 6): for a given
    /// `k`, `r` may fall by at most 1 per [`FEC_ADAPT_NOTCH_DWELL_US`];
    /// nothing limits a rise. `saturated` is the untrimmed, unpaced decision.
    pub(crate) fn r_for_paced(
        &mut self,
        k: u32,
        p: f64,
        b: f64,
        trim: u8,
        now_us: u64,
    ) -> RDecision {
        let mut d = self.r_for(k, p, b, trim);
        let slot = &mut self.paced[k.clamp(1, 254) as usize];
        if slot.0 == 0 || d.r >= slot.0 {
            slot.0 = d.r;
        } else if now_us.saturating_sub(slot.1) >= FEC_ADAPT_NOTCH_DWELL_US {
            slot.0 -= 1;
            slot.1 = now_us;
        }
        d.r = slot.0;
        d
    }

    /// `r` for a block of `k` sources, plus `trim` (0 or 1, from
    /// [`DeclinedTrim`]), still bounded by 32 and `255 - k`. `saturated`
    /// describes the untrimmed decision.
    pub(crate) fn r_for(&mut self, k: u32, p: f64, b: f64, trim: u8) -> RDecision {
        let k = k.clamp(1, 254);
        let key = (
            (p.max(1e-9).ln() / 1.005f64.ln()).round() as i32,
            (b / 0.05).round() as i32,
        );
        if self.key != Some(key) {
            self.key = Some(key);
            self.p = p;
            self.b = b;
            self.memo.clear();
        }
        let (p0, b0) = (self.p, self.b);
        let mut d = *self
            .memo
            .entry(k)
            .or_insert_with(|| r_for_target(k, p0, b0, FEC_ADAPT_TARGET));
        let ceiling = u32::from(FEC_ADAPT_R_MAX).min(255 - k);
        let base = d.r;
        d.r = (u32::from(d.r) + u32::from(trim)).min(ceiling) as u8;
        self.last_trim = d.r.saturating_sub(base);
        self.last_base = base;
        d
    }
}

/// The notches of the span ladder (decision 6). No 10 ms notch. `Frame` is not
/// between the other two: it is entered from either and left to `Wide` (or
/// `Narrow` when the burst is gone). In it one block spans a frame: T is the
/// frame interval and the block advertises it (`T_adv`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Notch {
    Narrow,
    Wide,
    Frame,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Span {
    pub(crate) t_ms: u8,
    pub(crate) k_max: u8,
}

/// `T_base = min(max(5 ms, 6 slots), 20 ms)`; a slot is `max_dg` at the
/// source rate.
pub(crate) fn t_base_ms(rate_bps: u64, max_dg: usize) -> u8 {
    let slot_us = (max_dg as f64 * 8.0 * 1e6) / (rate_bps.max(1) as f64);
    let t_ms = (6.0 * slot_us / 1000.0)
        .ceil()
        .clamp(f64::from(T_MIN_MS), f64::from(T_MAX_MS));
    t_ms as u8
}

/// How many 255-symbol blocks a frame at `rate_bps` fills once `FRAME_RHO` of
/// repair is added: `frame_bundles x (1 + rho) / 255`.
fn frame_fill(rate_bps: u64, max_dg: usize, fps: u32) -> f64 {
    let frame_bundles = rate_bps as f64 / (8.0 * max_dg.max(1) as f64 * f64::from(fps.max(1)));
    frame_bundles * (1.0 + FRAME_RHO) / 255.0
}

/// The frame interval, ms, rounded up and held to the knob's `T` range.
fn frame_interval_ms(fps: u32) -> u8 {
    let ms = (1000.0 / f64::from(fps.max(1))).ceil();
    ms.clamp(1.0, f64::from(crate::config::FecBlockParams::T_MS_MAX)) as u8
}

#[derive(Clone, Debug)]
pub(crate) struct Ladder {
    notch: Notch,
    last_change_us: Option<u64>,
    /// `Frame` may be entered. Off is a test switch (the control arm of the
    /// frame-notch scenes): the ladder is then the two-notch ladder of E4.
    frame: bool,
    /// Notch changes, and entries into `Frame`, cumulative.
    notch_changes: u64,
    frame_entries: u64,
}

impl Ladder {
    pub(crate) fn new() -> Self {
        Self::with_frame_notch(true)
    }

    /// A ladder that may (`true`) or never (`false`) enter the frame notch.
    pub(crate) fn with_frame_notch(frame: bool) -> Self {
        Self {
            notch: Notch::Narrow,
            last_change_us: None,
            frame,
            notch_changes: 0,
            frame_entries: 0,
        }
    }

    pub(crate) fn notch(&self) -> Notch {
        self.notch
    }

    /// Notch changes so far.
    pub(crate) fn notch_changes(&self) -> u64 {
        self.notch_changes
    }

    /// Entries into `Frame` so far.
    pub(crate) fn frame_entries(&self) -> u64 {
        self.frame_entries
    }

    /// Update the notch from the burst estimate (hysteresis, one change per
    /// dwell) and return the span for the source rate.
    ///
    /// `Frame` is entered when a frame no longer fits one block and
    /// `b_hat > FEC_FRAME_B_ENTER`, and left when `b_hat < FEC_FRAME_B_EXIT` or
    /// the frame fits one block again.
    pub(crate) fn step(
        &mut self,
        b_hat: f64,
        rate_bps: u64,
        max_dg: usize,
        fps: u32,
        now_us: u64,
    ) -> Span {
        let dwelled = self
            .last_change_us
            .is_none_or(|c| now_us.saturating_sub(c) >= FEC_ADAPT_NOTCH_DWELL_US);
        let fill = frame_fill(rate_bps, max_dg, fps);
        if dwelled {
            let enter_frame = self.frame && b_hat > FEC_FRAME_B_ENTER && fill > FRAME_FILL_ENTER;
            let next = match self.notch {
                Notch::Frame if b_hat < FEC_FRAME_B_EXIT || fill <= FRAME_FILL_EXIT => {
                    if b_hat < FEC_ADAPT_NOTCH_DOWN {
                        Notch::Narrow
                    } else {
                        Notch::Wide
                    }
                }
                Notch::Frame => Notch::Frame,
                _ if enter_frame => Notch::Frame,
                Notch::Narrow if b_hat > FEC_ADAPT_NOTCH_UP => Notch::Wide,
                Notch::Wide if b_hat < FEC_ADAPT_NOTCH_DOWN => Notch::Narrow,
                Notch::Narrow => Notch::Narrow,
                Notch::Wide => Notch::Wide,
            };
            if next != self.notch {
                self.notch = next;
                self.last_change_us = Some(now_us);
                self.notch_changes += 1;
                self.frame_entries += u64::from(next == Notch::Frame);
            }
        }
        match self.notch {
            Notch::Narrow => Span {
                t_ms: t_base_ms(rate_bps, max_dg),
                k_max: K_MIN as u8,
            },
            Notch::Wide => {
                let per_20ms =
                    (rate_bps as f64 * f64::from(T_MAX_MS) / 1000.0) / (8.0 * max_dg.max(1) as f64);
                Span {
                    t_ms: T_MAX_MS as u8,
                    k_max: per_20ms.clamp(f64::from(K_MIN), f64::from(K_MAX)) as u8,
                }
            }
            Notch::Frame => Span {
                t_ms: frame_interval_ms(fps),
                k_max: (255 - u32::from(FEC_ADAPT_R_MAX)) as u8,
            },
        }
    }
}

/// The closed-loop trim: +1 to `r` when more than 1 % of the last 256 closed
/// blocks ended `Declined`, until the share is under 0.5 %. Only `Declined`
/// counts: a `HandedOff` is often a cap eviction and a `TimedOut` an alarm,
/// neither says `r` was too small. A window under 256 never acts.
#[derive(Clone, Debug)]
pub(crate) struct DeclinedTrim {
    ring: [bool; FEC_ADAPT_TRIM_WINDOW],
    next: usize,
    filled: usize,
    declined: usize,
    active: bool,
    steps: u64,
}

impl DeclinedTrim {
    pub(crate) fn new() -> Self {
        Self {
            ring: [false; FEC_ADAPT_TRIM_WINDOW],
            next: 0,
            filled: 0,
            declined: 0,
            active: false,
            steps: 0,
        }
    }

    pub(crate) fn on_close(&mut self, reason: CloseReason) {
        let d = reason == CloseReason::Declined;
        if self.filled == FEC_ADAPT_TRIM_WINDOW && self.ring[self.next] {
            self.declined -= 1;
        }
        self.ring[self.next] = d;
        self.declined += usize::from(d);
        self.next = (self.next + 1) % FEC_ADAPT_TRIM_WINDOW;
        self.filled = (self.filled + 1).min(FEC_ADAPT_TRIM_WINDOW);
        if self.filled == FEC_ADAPT_TRIM_WINDOW {
            let share = self.declined as f64 / FEC_ADAPT_TRIM_WINDOW as f64;
            if share > 0.01 {
                self.steps += u64::from(!self.active);
                self.active = true;
            } else if share < 0.005 {
                self.active = false;
            }
        }
    }

    /// Times the trim switched on (cumulative).
    pub(crate) fn steps(&self) -> u64 {
        self.steps
    }

    /// 0 or 1: what to add to `r`.
    pub(crate) fn trim(&self) -> u8 {
        u8::from(self.active)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VECTORS: &str = include_str!("testdata/adapt_oracle_vectors.csv");
    const EXPECTED: &str = include_str!("testdata/adapt_r_for_expected.csv");

    const PS: [f64; 6] = [0.001, 0.005, 0.01, 0.02, 0.05, 0.1];
    const BS: [f64; 4] = [1.0, 1.5, 3.0, 8.0];

    fn rows(s: &str) -> impl Iterator<Item = Vec<f64>> + '_ {
        s.lines()
            .skip(1)
            .map(|l| l.split(',').map(|x| x.parse::<f64>().unwrap()).collect())
    }

    #[test]
    fn tail_matches_every_oracle_row() {
        let mut seen = 0;
        let mut nonzero = 0;
        for v in rows(VECTORS) {
            let (n, r, p, b, want) = (v[0] as usize, v[1] as usize, v[2], v[3], v[4]);
            let got = tail_probability(n, r, p, b);
            seen += 1;
            if want > 0.0 {
                nonzero += 1;
                assert!(
                    ((got - want) / want).abs() <= 1e-9,
                    "n={n} r={r} p={p} b={b}: got {got:e} want {want:e}"
                );
            } else {
                assert_eq!(got, 0.0, "n={n} r={r}");
            }
        }
        assert_eq!(seen, 4608);
        assert!(nonzero > 3000, "oracle rows must be mostly non-trivial");
    }

    #[test]
    fn r_for_matches_the_oracle_table_and_saturation() {
        let (mut seen, mut sat, mut capped_met) = (0, 0, 0);
        for v in rows(EXPECTED) {
            let (k, p, b, r, s) = (v[0] as u32, v[1], v[2], v[3] as u8, v[4] == 1.0);
            let d = RSizer::new().r_for(k, p, b, 0);
            assert_eq!((d.r, d.saturated), (r, s), "k={k} p={p} b={b}");
            seen += 1;
            sat += usize::from(s);
            // Independent restatement from the DP: saturated iff the cap misses.
            let rr = u32::from(d.r);
            let miss = tail_probability((k + rr) as usize, rr as usize, p, b) > FEC_ADAPT_TARGET;
            assert_eq!(d.saturated, miss, "k={k} p={p} b={b}");
            if !miss && rr == r_cap(k) {
                capped_met += 1;
            }
            if !d.saturated && rr > 1 {
                let below = tail_probability((k + rr - 1) as usize, (rr - 1) as usize, p, b);
                assert!(below > FEC_ADAPT_TARGET, "r is the smallest");
            }
        }
        assert_eq!(seen, 264);
        assert!(
            sat > 50 && sat < seen,
            "both outcomes must occur, got {sat}"
        );
        let _ = capped_met;
    }

    #[test]
    fn findings_table_cells() {
        // FINDINGS.md section 8, k >= 4 (no k < 4 lift); '*' = saturated.
        let cases = [
            (10, 0.02, 1.0, 3, false),
            (64, 0.05, 1.0, 11, false),
            (223, 0.05, 1.0, 24, false),
            (4, 0.02, 1.0, 1, true),
            (10, 0.05, 1.0, 3, true),
            (64, 0.02, 3.0, 16, true),
            (128, 0.02, 3.0, 23, false),
            (223, 0.1, 8.0, 32, true),
        ];
        for (k, p, b, r, s) in cases {
            let d = RSizer::new().r_for(k, p, b, 0);
            assert_eq!((d.r, d.saturated), (r, s), "k={k} p={p} b={b}");
        }
    }

    #[test]
    fn r_is_monotone_in_k() {
        let mut steps = 0;
        for &p in &PS {
            for &b in &BS {
                let mut sizer = RSizer::new();
                let mut prev = 0;
                for k in 1..=223 {
                    let r = sizer.r_for(k, p, b, 0).r;
                    assert!(r >= prev, "p={p} b={b}: k={k} asks {r} below {prev}");
                    steps += usize::from(r > prev);
                    prev = r;
                }
            }
        }
        assert!(steps > 100, "r must actually grow with k");
    }

    #[test]
    fn r_bounds_and_monotone_in_p_b_and_target() {
        for k in [1, 3, 4, 10, 24, 64, 128, 223] {
            let mut prev_p = 0;
            for &p in &PS {
                let mut prev_b = 0;
                for &b in &BS {
                    let d = r_for_target(k, p, b, FEC_ADAPT_TARGET);
                    assert!(d.r >= 1 && u32::from(d.r) <= r_cap(k) && d.r <= FEC_ADAPT_R_MAX);
                    assert!(u32::from(d.r) + k <= 255);
                    assert!(d.r >= prev_b, "monotone in b: k={k} p={p} b={b}");
                    prev_b = d.r;
                    let tight = r_for_target(k, p, b, FEC_ADAPT_TARGET / 100.0);
                    assert!(tight.r >= d.r, "monotone in 1/target");
                }
                let d = r_for_target(k, p, 3.0, FEC_ADAPT_TARGET);
                assert!(d.r >= prev_p, "monotone in p: k={k} p={p}");
                prev_p = d.r;
            }
        }
    }

    #[test]
    fn trim_adds_one_and_stays_bounded() {
        let mut s = RSizer::new();
        let base = s.r_for(64, 0.05, 1.0, 0);
        let t = s.r_for(64, 0.05, 1.0, 1);
        assert_eq!(t.r, base.r + 1);
        assert_eq!(t.saturated, base.saturated);
        assert_eq!(s.memo.len(), 1, "one entry per k within a (p, b) cell");
        s.r_for(65, 0.05, 1.0, 0);
        assert_eq!(s.memo.len(), 2);
        assert_eq!(s.r_for(223, 0.1, 8.0, 1).r, 32);
        assert_eq!(s.memo.len(), 1, "a new (p, b) cell clears the memo");
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
    }

    #[test]
    fn the_ladder_obeys_the_dwell_in_every_notch() {
        let mut rng = Lcg(7);
        for rate in [500_000u64, 2_000_000, 30_000_000, 200_000_000] {
            let mut l = Ladder::new();
            let (mut now, mut last_change, mut changes, mut frames) = (0u64, None::<u64>, 0, 0);
            let mut prev = l.notch();
            for _ in 0..5000 {
                now += (rng.next() * 400_000.0) as u64;
                let b = 1.0 + rng.next() * 9.0;
                let s = l.step(b, rate, 1200, 30, now);
                if l.notch() == Notch::Frame {
                    assert!(s.t_ms <= crate::config::FecBlockParams::T_MS_MAX);
                } else {
                    assert!((5..=20).contains(&s.t_ms), "T {}", s.t_ms);
                }
                assert!((64..=223).contains(&s.k_max));
                assert!(u32::from(s.k_max) + u32::from(FEC_ADAPT_R_MAX) <= 255);
                match l.notch() {
                    Notch::Narrow => assert_eq!(s.k_max, 64),
                    Notch::Wide => assert_eq!(s.t_ms, 20),
                    Notch::Frame => assert_eq!((s.t_ms, s.k_max), (34, 223)),
                }
                frames += usize::from(l.notch() == Notch::Frame);
                if l.notch() != prev {
                    changes += 1;
                    if let Some(c) = last_change {
                        assert!(
                            now - c >= FEC_ADAPT_NOTCH_DWELL_US,
                            "two changes inside a dwell"
                        );
                    }
                    last_change = Some(now);
                    prev = l.notch();
                }
            }
            assert!(changes > 5, "the walk must cross the notch");
            if rate >= 100_000_000 {
                assert!(frames > 0, "{rate}: the walk must enter the frame notch");
            }
        }
    }

    /// The two-notch ladder as it was before the frame notch, frozen for the
    /// equivalence test below.
    struct OldLadder {
        notch: Notch,
        last_change_us: Option<u64>,
    }
    impl OldLadder {
        fn step(&mut self, b_hat: f64, rate_bps: u64, max_dg: usize, now_us: u64) -> (u8, u8) {
            let dwelled = self
                .last_change_us
                .is_none_or(|c| now_us.saturating_sub(c) >= FEC_ADAPT_NOTCH_DWELL_US);
            if dwelled {
                let next = match self.notch {
                    Notch::Narrow if b_hat > FEC_ADAPT_NOTCH_UP => Notch::Wide,
                    Notch::Wide if b_hat < FEC_ADAPT_NOTCH_DOWN => Notch::Narrow,
                    n => n,
                };
                if next != self.notch {
                    self.notch = next;
                    self.last_change_us = Some(now_us);
                }
            }
            match self.notch {
                Notch::Narrow => (t_base_ms(rate_bps, max_dg), K_MIN as u8),
                _ => {
                    let per_20ms = (rate_bps as f64 * f64::from(T_MAX_MS) / 1000.0)
                        / (8.0 * max_dg.max(1) as f64);
                    (
                        T_MAX_MS as u8,
                        per_20ms.clamp(f64::from(K_MIN), f64::from(K_MAX)) as u8,
                    )
                }
            }
        }
    }

    /// While a frame fits one block, whatever the burst, the ladder's whole
    /// output is the two-notch ladder's. Break check: enter `Frame` at any rate
    /// and this fails.
    #[test]
    fn while_a_frame_fits_one_block_the_ladder_is_the_one_before_the_frame_notch() {
        let mut rng = Lcg(11);
        let mut bursty = 0;
        // 100 Mbit/s at 30 fps, 1200 B: 347 bundles x 1.2 / 255 = 1.6 blocks, so
        // 60 Mbit/s (0.98) is the highest rate checked here.
        for rate in [500_000u64, 2_000_000, 10_000_000, 30_000_000, 60_000_000] {
            assert!(frame_fill(rate, 1200, 30) <= FRAME_FILL_ENTER);
            let mut new = Ladder::new();
            let mut old = OldLadder {
                notch: Notch::Narrow,
                last_change_us: None,
            };
            let mut now = 0u64;
            for _ in 0..4000 {
                now += (rng.next() * 600_000.0) as u64;
                let b = 1.0 + rng.next() * 11.0;
                bursty += usize::from(b > FEC_FRAME_B_ENTER);
                let got = new.step(b, rate, 1200, 30, now);
                assert_eq!(
                    (got.t_ms, got.k_max),
                    old.step(b, rate, 1200, now),
                    "rate {rate}"
                );
                assert_ne!(new.notch(), Notch::Frame);
            }
        }
        assert!(bursty > 1000, "premise: the walk was often above the entry");
    }

    /// Entry needs both conditions, exit either one's failure, and in between
    /// the notch holds.
    #[test]
    fn the_frame_notch_has_a_hysteresis_and_needs_a_frame_over_one_block() {
        let rate = 200_000_000;
        let mut l = Ladder::new();
        l.step(5.0, rate, 1200, 30, 0);
        assert_ne!(l.notch(), Notch::Frame, "5 is under the entry");
        let s = l.step(7.0, rate, 1200, 30, 2_000_000);
        assert_eq!(l.notch(), Notch::Frame);
        assert_eq!((s.t_ms, s.k_max), (34, 223));
        for (i, b) in [5.9, 4.1, 5.0, 6.5, 4.01].into_iter().enumerate() {
            l.step(b, rate, 1200, 30, 4_000_000 + i as u64 * 2_000_000);
            assert_eq!(l.notch(), Notch::Frame, "b = {b} is inside the band");
        }
        l.step(3.9, rate, 1200, 30, 20_000_000);
        assert_eq!(
            l.notch(),
            Notch::Wide,
            "under the exit, burst still above 1.25"
        );
        // The rate falling to one block leaves it too.
        let mut l = Ladder::new();
        l.step(7.0, rate, 1200, 30, 0);
        assert_eq!(l.notch(), Notch::Frame);
        l.step(7.0, 20_000_000, 1200, 30, 2_000_000);
        assert_eq!(l.notch(), Notch::Wide, "one block fits the frame again");
        l.step(7.0, 20_000_000, 1200, 30, 4_000_000);
        assert_eq!(
            l.notch(),
            Notch::Wide,
            "b is high but the frame fits one block"
        );
        // A switched-off ladder never enters.
        let mut off = Ladder::with_frame_notch(false);
        off.step(9.0, rate, 1200, 30, 0);
        off.step(9.0, rate, 1200, 30, 2_000_000);
        assert_eq!(off.notch(), Notch::Wide);
        // The span fits the advertised table: T + queue allowance <= 66 ms.
        assert!(
            u64::from(frame_interval_ms(30)) + 10 <= 66
                && u64::from(frame_interval_ms(1)) + 10 <= 66
        );
    }

    /// Random burst and rate walks: at most one change per dwell, `Frame` only
    /// entered above the entry, and a noisy burst estimate inside the band never
    /// leaves it. Break check: remove the hysteresis (exit at the entry) and the
    /// last assertion fails.
    #[test]
    fn frame_notch_changes_are_bounded_by_the_dwell_and_the_band() {
        let mut rng = Lcg(5);
        let mut l = Ladder::new();
        let (mut now, mut prev, mut last_change) = (0u64, l.notch(), None::<u64>);
        let mut entries = 0;
        for i in 0..20_000 {
            now += (rng.next() * 500_000.0) as u64;
            let b = 1.0 + rng.next() * 11.0;
            let rate = 20_000_000 + (rng.next() * 380_000_000.0) as u64 + (i % 7) * 1_000_000;
            l.step(b, rate, 1200, 30, now);
            if l.notch() != prev {
                if let Some(c) = last_change {
                    assert!(
                        now - c >= FEC_ADAPT_NOTCH_DWELL_US,
                        "two changes in a dwell"
                    );
                }
                last_change = Some(now);
                if l.notch() == Notch::Frame {
                    entries += 1;
                    assert!(b > FEC_FRAME_B_ENTER && frame_fill(rate, 1200, 30) > FRAME_FILL_ENTER);
                }
                prev = l.notch();
            }
        }
        assert!(entries > 20, "premise: {entries} entries");

        let rate = 200_000_000;
        let mut l = Ladder::new();
        l.step(8.0, rate, 1200, 30, 0);
        assert_eq!(l.notch(), Notch::Frame);
        let before = l.notch_changes();
        let mut t = 0;
        for _ in 0..2000 {
            t += 1_100_000;
            let b =
                FEC_FRAME_B_EXIT + 0.05 + rng.next() * (FEC_FRAME_B_ENTER - FEC_FRAME_B_EXIT - 0.1);
            l.step(b, rate, 1200, 30, t);
        }
        assert_eq!(
            l.notch_changes(),
            before,
            "inside the band: no change at all"
        );
        assert_eq!(l.notch(), Notch::Frame);
    }

    #[test]
    fn ladder_hysteresis_and_k_max_follow_rate() {
        let mut l = Ladder::new();
        assert_eq!(l.step(1.4, 10_000_000, 1200, 30, 0).k_max, 64);
        assert_eq!(l.notch(), Notch::Narrow);
        let s = l.step(1.6, 100_000_000, 1200, 30, 2_000_000);
        assert_eq!((l.notch(), s.t_ms, s.k_max), (Notch::Wide, 20, 208));
        l.step(1.3, 100_000_000, 1200, 30, 4_000_000);
        assert_eq!(l.notch(), Notch::Wide, "1.3 is inside the hysteresis band");
        l.step(1.2, 100_000_000, 1200, 30, 6_000_000);
        assert_eq!(l.notch(), Notch::Narrow);
        // T_base: 6 slots of 1200 B at 2 Mbit/s is 28.8 ms -> capped at 20.
        assert_eq!(t_base_ms(2_000_000, 1200), 20);
        assert_eq!(t_base_ms(1_000_000_000, 1200), 5);
        assert_eq!(t_base_ms(10_000_000, 1200), 6);
    }

    #[test]
    fn r_falls_by_at_most_one_per_dwell_and_rises_at_once() {
        let mut s = RSizer::new();
        let k = 64;
        let hi = s.r_for_paced(k, 0.05, 1.0, 0, 0).r;
        assert!(hi >= 8, "premise: 5 % sizes a large r, got {hi}");
        let lo = s.r_for(k, 0.002, 1.0, 0).r;
        assert!(lo + 3 < hi, "premise: 0.2 % sizes far lower ({lo})");
        let mut prev = hi;
        let mut falls = 0;
        // Ten dwells, polled every 100 ms: one step per dwell, never two.
        for i in 1..=100u64 {
            let t = i * 100_000;
            let r = s.r_for_paced(k, 0.002, 1.0, 0, t).r;
            assert!(r <= prev && prev - r <= 1, "t={t}: {prev} -> {r}");
            if r < prev {
                falls += 1;
                assert!(
                    t + 100_000 >= falls * FEC_ADAPT_NOTCH_DWELL_US,
                    "fall {falls} at {t} came sooner than one per dwell"
                );
            }
            prev = r;
        }
        assert!(
            falls >= 3 && prev >= lo,
            "it did fall ({falls}), toward {lo}"
        );
        // A rise is immediate.
        assert_eq!(s.r_for_paced(k, 0.05, 1.0, 0, 10_100_000).r, hi);
    }

    #[test]
    fn trim_counts_declined_only() {
        let mut t = DeclinedTrim::new();
        for _ in 0..255 {
            t.on_close(CloseReason::Declined);
        }
        assert_eq!(t.trim(), 0, "a window under 256 never acts");
        t.on_close(CloseReason::Declined);
        assert_eq!(t.trim(), 1);
        // Release below 0.5 %: 256 accounted closes push every Declined out.
        for i in 1..=255 {
            t.on_close(CloseReason::Accounted);
            // 256 - i declined left: held between the 1 % and 0.5 % bands.
            let want = u8::from(256 - i >= 2);
            assert_eq!(t.trim(), want, "after {i} accounted closes");
        }

        let mut t = DeclinedTrim::new();
        for i in 0..2000 {
            t.on_close(if i % 2 == 0 {
                CloseReason::HandedOff
            } else {
                CloseReason::TimedOut
            });
            assert_eq!(t.trim(), 0);
        }
        assert_eq!(t.filled, FEC_ADAPT_TRIM_WINDOW);
        // 1 % band: 3 of 256 declined engages, 2 does not.
        let mut t = DeclinedTrim::new();
        for i in 0..256 {
            t.on_close(if i < 2 {
                CloseReason::Declined
            } else {
                CloseReason::Accounted
            });
        }
        assert_eq!(t.trim(), 0);
        let mut t = DeclinedTrim::new();
        for i in 0..256 {
            t.on_close(if i < 3 {
                CloseReason::Declined
            } else {
                CloseReason::Accounted
            });
        }
        assert_eq!(t.trim(), 1);
    }
    /// `DeclinedLate` never raises the trim, and it fills the 256 window like
    /// any other non-decline: replacing every `DeclinedLate` by `Accounted`
    /// changes nothing the trim reports.
    #[test]
    fn trim_ignores_declined_late_but_it_fills_the_window() {
        let mut t = DeclinedTrim::new();
        for _ in 0..2000 {
            t.on_close(CloseReason::DeclinedLate);
            assert_eq!(t.trim(), 0);
        }
        assert_eq!((t.steps(), t.filled), (0, FEC_ADAPT_TRIM_WINDOW));

        // 3 Declined engage it; DeclinedLate closes then push them out of the
        // window exactly as Accounted ones do.
        let mut a = DeclinedTrim::new();
        let mut b = DeclinedTrim::new();
        for i in 0..256 {
            let d = if i < 3 {
                CloseReason::Declined
            } else {
                CloseReason::DeclinedLate
            };
            a.on_close(d);
            b.on_close(if i < 3 {
                CloseReason::Declined
            } else {
                CloseReason::Accounted
            });
        }
        assert_eq!(a.trim(), 1, "premise: three declines engaged it");
        for _ in 0..256 {
            a.on_close(CloseReason::DeclinedLate);
            b.on_close(CloseReason::Accounted);
            assert_eq!((a.trim(), a.steps()), (b.trim(), b.steps()));
        }
        assert_eq!(a.trim(), 0, "pushed out of the window");
    }
}
