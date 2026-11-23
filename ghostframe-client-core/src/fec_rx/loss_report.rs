// SPDX-License-Identifier: MIT OR Apache-2.0
//! Periodic LOSS_REPORT (0x07): pre-FEC loss from `bundle_seq` gaps (the
//! existing `WireLossTracker`) plus `BlockStats` deltas.
//!
//! Fields the client cannot measure yet are 0: `inter_arrival_us`,
//! `repair_late`, `repair_skipped_deadline`, `h264_frames_lost`,
//! `recovery_p95_ms`, `rx_buffer_drops`, `src_lost_pre_fec`.
//! `lost_pre_fec` counts the losses of settled runs only, so that it and
//! `loss_run_hist` agree within every report (the open run waits).
//! `recovery_allowance_ms` is the constant; `residual_lost` is the change of
//! `max(0, lost - recovered)` over the cumulative counters (new highs only),
//! so a block repaired while its run is still open (recovered ahead of lost)
//! is not a residual loss one report later, and a count that falls and rises
//! again is not summed twice.

use ghostframe_protocol::fec::feedback::LossReport;

use super::blocks::{BlockStats, FEC_RX_ALLOWANCE_US};
use crate::wire_loss::{WireLossStats, WireLossTracker};

pub const LOSS_REPORT_INTERVAL_US: u64 = 250_000;
pub const LOSS_REPORT_COUNT_TRIGGER: u64 = 128;
/// The longest a loss run stays open before it is closed (see
/// [`WireLossTracker::close_open_run`]): the slots of it still waiting in the
/// reorder window are settled with it, and the run reaches the server whole,
/// in one bucket at its true length. A run closes by itself when 65 datagrams
/// arrive after its end: at the 30 datagrams/s floor that is over 2 s. The
/// server holds a verdict at most 10 s for a report, and 2 s at the least;
/// this stays well inside both.
pub const OPEN_RUN_MAX_US: u64 = 1_000_000;
/// Nothing arrived for this long: no datagram of the reorder window is still in
/// flight, so everything through the newest is settled (a gap of 64 or less
/// followed by a few arrivals on a screen gone static opens no run and would
/// otherwise wait for traffic that may not come, until the server's verdict
/// expires).
pub const QUIET_SETTLE_US: u64 = 1_000_000;

fn sat(v: u64) -> u16 {
    v.min(u16::MAX as u64) as u16
}

pub struct LossReporter {
    wire: WireLossTracker,
    started: bool,
    last_report_us: u64,
    last_wire: WireLossStats,
    last_blocks: BlockStats,
    /// The highest `max(0, lost - recovered)` the cumulative counters have
    /// shown, so a report carries only what the count has newly passed.
    last_residual: u64,
    /// `received` as of the last poll, and when it last moved.
    seen_received: u64,
    last_arrival_us: u64,
    /// When the run now open was first seen open.
    open_since_us: Option<u64>,
    /// Runs closed as of the last poll: a change means the open run, if any,
    /// is a new one.
    seen_runs: u64,
    /// Lowest / highest bundle_seq seen in this interval.
    range: Option<(u32, u32)>,
    /// Highest bundle_seq seen ever, to measure reorder distance.
    newest: Option<u32>,
    reorder_max: u32,
}

impl LossReporter {
    pub fn new() -> Self {
        LossReporter {
            wire: WireLossTracker::new(),
            started: false,
            last_report_us: 0,
            last_wire: WireLossStats::default(),
            last_blocks: BlockStats::default(),
            last_residual: 0,
            seen_received: 0,
            last_arrival_us: 0,
            open_since_us: None,
            seen_runs: 0,
            range: None,
            newest: None,
            reorder_max: 0,
        }
    }

    pub fn on_arrival(&mut self, bundle_seq: u32) {
        self.wire.on_received(bundle_seq);
        self.range = Some(match self.range {
            None => (bundle_seq, bundle_seq),
            // Wrap-aware: `bundle_seq` is a u32 counter, and across its wrap a
            // plain min/max would report `last_bundle_seq` as the pre-wrap
            // value for ever after, so the server would never confirm a loss
            // past it.
            Some((lo, hi)) => (
                if (bundle_seq.wrapping_sub(lo) as i32) < 0 {
                    bundle_seq
                } else {
                    lo
                },
                if (bundle_seq.wrapping_sub(hi) as i32) > 0 {
                    bundle_seq
                } else {
                    hi
                },
            ),
        });
        match self.newest {
            Some(n) if (bundle_seq.wrapping_sub(n) as i32) < 0 => {
                self.reorder_max = self.reorder_max.max(n.wrapping_sub(bundle_seq));
            }
            _ => self.newest = Some(bundle_seq),
        }
    }

    pub fn on_rebuilt(&mut self, bundle_seq: u32) {
        self.wire.on_rebuilt(bundle_seq);
    }

    /// Cumulative wire statistics.
    pub fn wire_stats(&self) -> WireLossStats {
        self.wire.stats()
    }

    /// `now_us` is the caller's clock. Returns an encoded LOSS_REPORT when 250 ms
    /// have passed or 128 bundles were newly counted, and never before the first
    /// framed datagram.
    pub fn maybe_report(&mut self, now_us: u64, blocks: &BlockStats) -> Option<Vec<u8>> {
        if !self.started {
            // The first call (arrival or not) sets the interval's baseline.
            self.started = true;
            self.last_report_us = now_us;
        }
        let received = self.wire.stats().received;
        if received != self.seen_received {
            self.seen_received = received;
            self.last_arrival_us = now_us;
        }
        let runs: u64 = self.wire.stats().run_hist.iter().sum();
        if runs != self.seen_runs {
            self.seen_runs = runs;
            self.open_since_us = None;
        }
        // Nothing arrived for a while: settle through the newest, so a gap
        // that opened no run and the tail of one still waiting in the window
        // reach the server while its verdicts are alive.
        let mut closed = false;
        if self.wire.has_unsettled()
            && now_us.saturating_sub(self.last_arrival_us) >= QUIET_SETTLE_US
        {
            self.wire.settle_all();
            closed = true;
        } else if self.wire.open_run() == 0 {
            self.open_since_us = None;
        } else if now_us.saturating_sub(*self.open_since_us.get_or_insert(now_us))
            >= OPEN_RUN_MAX_US
        {
            // A run open for too long, with traffic still arriving slowly:
            // closed whole, at its true length, never in pieces.
            self.wire.close_open_run();
            closed = true;
        }
        if closed {
            self.open_since_us = None;
            // Nothing arrived since the last report on a static screen; the
            // report still has a `last_bundle_seq` to name: the newest.
            // Its range must reach the run just closed, even if this
            // interval's only arrivals were reordered ones below it.
            if let Some(n) = self.newest {
                self.range = Some(match self.range {
                    None => (n, n),
                    Some((lo, hi)) if (n.wrapping_sub(hi) as i32) > 0 => (lo, n),
                    Some(r) => r,
                });
            }
        }
        // Never report before the first framed datagram.
        self.range?;
        let w = self.wire.stats();
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let counted =
            d(w.received, self.last_wire.received) + d(w.lost_in_runs, self.last_wire.lost_in_runs);
        let elapsed = now_us.saturating_sub(self.last_report_us);
        if elapsed < LOSS_REPORT_INTERVAL_US && counted < LOSS_REPORT_COUNT_TRIGGER {
            return None;
        }
        let (first, last) = self.range.unwrap_or((0, 0));
        // The losses of closed runs (see `WireLossStats::lost_in_runs`), not
        // of the run still open: a report then carries a run's count and its
        // length together, so the server never sees a long run's losses one
        // report before it learns the run was long (or, for a run split over
        // two reports, losses with no length at all). What is in the open run
        // waits for the report after it closes; the sum is exact.
        let lost = d(w.lost_in_runs, self.last_wire.lost_in_runs);
        let recovered = d(blocks.rebuilt, self.last_blocks.rebuilt);
        let mut hist = [0u16; 8];
        for (i, h) in hist.iter_mut().enumerate() {
            *h = sat(d(w.run_hist[i], self.last_wire.run_hist[i]));
        }
        let residual_now = w.lost_in_runs.saturating_sub(blocks.rebuilt);
        let residual = d(residual_now, self.last_residual);
        // A high-water mark: a cumulative count that falls (a block rebuilt
        // after its run was reported) and rises again is not reported twice.
        let residual_high = self.last_residual.max(residual_now);
        let rep = LossReport {
            interval_ms: sat(elapsed / 1000),
            first_bundle_seq: first,
            last_bundle_seq: last,
            received: sat(d(w.received, self.last_wire.received)),
            lost_pre_fec: sat(lost),
            recovered: sat(recovered),
            residual_lost: sat(residual),
            repair_rx: sat(d(blocks.repairs_rx, self.last_blocks.repairs_rx)),
            repair_used: sat(d(blocks.repair_used, self.last_blocks.repair_used)),
            repair_wasted: sat(d(blocks.repair_wasted, self.last_blocks.repair_wasted)),
            loss_run_hist: hist,
            reorder_max: sat(self.reorder_max as u64),
            recovery_allowance_ms: sat(FEC_RX_ALLOWANCE_US / 1000),
            blocks_unrecoverable: sat(d(
                blocks.blocks_unrecoverable,
                self.last_blocks.blocks_unrecoverable,
            )),
            repair_requests_sent: sat(d(blocks.requests, self.last_blocks.requests)),
            ..Default::default()
        };
        self.last_report_us = now_us;
        self.last_wire = w;
        self.last_blocks = *blocks;
        self.last_residual = residual_high;
        self.range = None;
        self.reorder_max = 0;
        Some(rep.encode())
    }
}

impl Default for LossReporter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_counts_gaps_runs_and_rebuilds() {
        let mut r = LossReporter::new();
        let mut blocks = BlockStats::default();
        assert!(
            r.maybe_report(0, &blocks).is_none(),
            "nothing before the first datagram"
        );
        for s in 0..200u32 {
            if !(50..=52).contains(&s) && s != 120 {
                r.on_arrival(s);
            }
        }
        r.on_rebuilt(51);
        blocks.rebuilt = 1;
        blocks.repairs_rx = 3;
        let dg = r.maybe_report(250_000, &blocks).expect("report due");
        let rep = LossReport::decode(&dg).unwrap();
        assert_eq!(rep.lost_pre_fec, 4);
        assert_eq!(rep.received, 196);
        assert_eq!(rep.loss_run_hist[2], 1);
        assert_eq!(rep.loss_run_hist[0], 1);
        assert_eq!(rep.recovered, 1);
        assert_eq!(rep.repair_rx, 3);
        assert_eq!((rep.first_bundle_seq, rep.last_bundle_seq), (0, 199));
        assert_eq!(rep.interval_ms, 250);
        // Deltas: an immediate second call reports nothing.
        assert!(r.maybe_report(250_001, &blocks).is_none());
    }

    #[test]
    fn the_reported_range_survives_the_u32_wrap() {
        let mut r = LossReporter::new();
        let blocks = BlockStats::default();
        assert!(r.maybe_report(0, &blocks).is_none());
        // u32::MAX is never a bundle_seq (NO_ACK); the counter goes 0xFFFF_FFFE -> 0.
        // 0xFFFF_FFFD arrives late, after 1, so the low end moves backwards.
        for s in [u32::MAX - 3, u32::MAX - 1, 0, 1, u32::MAX - 2, 2] {
            r.on_arrival(s);
        }
        let dg = r.maybe_report(250_000, &blocks).expect("report due");
        let rep = LossReport::decode(&dg).unwrap();
        assert_eq!(
            (rep.first_bundle_seq, rep.last_bundle_seq),
            (u32::MAX - 3, 2),
            "first is the oldest and last the newest in wrapped order"
        );
    }

    /// Deliver `from..to` in order, skipping `lost`.
    fn feed(r: &mut LossReporter, from: u32, to: u32, lost: std::ops::Range<u32>) {
        for s in from..to {
            if !lost.contains(&s) {
                r.on_arrival(s);
            }
        }
    }

    /// A forced close in an interval whose only arrival was a reordered one:
    /// the report's range still reaches the run it closed, or the server could
    /// not confirm those verdicts.
    #[test]
    fn a_forced_close_reports_a_range_that_reaches_the_run() {
        let mut r = LossReporter::new();
        let b = BlockStats::default();
        for s in (0..10).chain(100..=150) {
            r.on_arrival(s);
        }
        r.maybe_report(0, &b);
        assert!(r.maybe_report(300_000, &b).is_some(), "premise: flushed");
        assert!(r.wire.open_run() > 0, "premise: a run is open");
        r.on_arrival(5);
        let dg = r
            .maybe_report(300_000 + OPEN_RUN_MAX_US, &b)
            .expect("the forced close reports");
        let rep = LossReport::decode(&dg).expect("decodes");
        assert_eq!(rep.lost_pre_fec, 90, "the whole run");
        assert_eq!(rep.last_bundle_seq, 150);
    }

    /// The age of an open run starts when that run opened, not when the one
    /// before it did.
    #[test]
    fn a_new_run_does_not_inherit_the_age_of_the_last_one() {
        let mut r = LossReporter::new();
        let b = BlockStats::default();
        for s in (0..10).chain(20..=80) {
            r.on_arrival(s);
        }
        r.maybe_report(0, &b);
        r.maybe_report(100_000, &b);
        assert!(r.wire.open_run() > 0, "premise: the first run is open");
        // It closes; a second one opens behind it.
        for s in (81..=90).chain(101..=160) {
            r.on_arrival(s);
        }
        r.maybe_report(200_000, &b);
        assert!(r.wire.open_run() > 0, "premise: the second run is open");
        r.on_arrival(161);
        r.maybe_report(100_000 + OPEN_RUN_MAX_US, &b);
        assert!(r.wire.open_run() > 0, "closed at the first run's age");
    }

    fn report(r: &mut LossReporter, at: u64) -> LossReport {
        let dg = r
            .maybe_report(at, &BlockStats::default())
            .expect("report due");
        LossReport::decode(&dg).unwrap()
    }

    #[test]
    fn a_runs_count_and_length_are_in_the_same_report() {
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        // 100..=139 lost (a run of 40); 0..170 sent: slots under 106 settled.
        feed(&mut r, 0, 170, 100..140);
        let a = report(&mut r, 250_000);
        // The 6 settled losses are in the open run: not reported yet, and not
        // in the histogram either.
        assert_eq!((a.lost_pre_fec, a.loss_run_hist), (0, [0; 8]));
        feed(&mut r, 170, 300, 0..0);
        let b = report(&mut r, 500_000);
        assert_eq!(b.lost_pre_fec, 40);
        assert_eq!(b.loss_run_hist[7], 1);
    }

    #[test]
    fn totals_are_exact_across_a_report_boundary_inside_a_long_run() {
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        // 100..=199 lost; 0..230 sent: 65 settled, the run open. A report
        // here carries none of it, and nothing it carries lacks its length.
        feed(&mut r, 0, 230, 100..200);
        let a = report(&mut r, 250_000);
        assert_eq!((a.lost_pre_fec, a.loss_run_hist), (0, [0; 8]));
        feed(&mut r, 230, 400, 0..0);
        let b = report(&mut r, 500_000);
        assert_eq!((b.lost_pre_fec, b.loss_run_hist[7]), (100, 1));
        assert_eq!(r.wire_stats().lost_pre_fec, 100);
        // The next report has nothing left over: no double count.
        feed(&mut r, 400, 600, 0..0);
        let c = report(&mut r, 750_000);
        assert_eq!((c.lost_pre_fec, c.loss_run_hist), (0, [0; 8]));
    }

    #[test]
    fn totals_are_exact_across_a_jump() {
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        feed(&mut r, 0, 100, 0..0);
        let a = report(&mut r, 250_000);
        assert_eq!(a.lost_pre_fec, 0);
        // A blackout of 1000 datagrams, then traffic resumes: the run is open
        // across the jump, so the report after it carries nothing of it.
        r.on_arrival(1100);
        let b = report(&mut r, 500_000);
        assert_eq!((b.lost_pre_fec, b.loss_run_hist), (0, [0; 8]));
        feed(&mut r, 1101, 1300, 0..0);
        let c = report(&mut r, 750_000);
        assert_eq!((c.lost_pre_fec, c.loss_run_hist[7]), (1000, 1));
        assert_eq!(r.wire_stats().lost_pre_fec, 1000);
    }

    /// Poll as the client's timer does from `from` to `to` (step `step`),
    /// collecting every report as `(time, report)`.
    fn poll(r: &mut LossReporter, from: u64, to: u64, step: u64) -> Vec<(u64, LossReport)> {
        let mut out = Vec::new();
        let mut t = from;
        while t <= to {
            if let Some(d) = r.maybe_report(t, &BlockStats::default()) {
                out.push((t, LossReport::decode(&d).unwrap()));
            }
            t += step;
        }
        out
    }

    #[test]
    fn an_open_run_is_reported_whole_within_a_bound_even_when_traffic_stops() {
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        // 100..=199 lost; arrivals up to 230, then the screen goes static.
        feed(&mut r, 0, 230, 100..200);
        let a = report(&mut r, 250_000);
        assert_eq!((a.lost_pre_fec, a.loss_run_hist), (0, [0; 8]));
        let got = poll(&mut r, 300_000, 3_000_000, 50_000);
        let (at, b) = got.first().expect("the open run was never reported");
        assert!(
            *at <= 250_000 + QUIET_SETTLE_US + LOSS_REPORT_INTERVAL_US + 50_000,
            "at {at}"
        );
        assert_eq!(
            (b.lost_pre_fec, b.loss_run_hist[7]),
            (100, 1),
            "the whole run, count and bucket together"
        );
        assert_eq!(b.last_bundle_seq, 229, "names the newest sequence it has");
        // Traffic returns: nothing is counted twice, no run is made of a tail.
        feed(&mut r, 230, 400, 0..0);
        let all: u64 = got.iter().map(|(_, g)| u64::from(g.lost_pre_fec)).sum();
        let c = poll(&mut r, 3_300_000, 3_300_000, 1);
        assert_eq!(
            all + c
                .iter()
                .map(|(_, g)| u64::from(g.lost_pre_fec))
                .sum::<u64>(),
            100
        );
        assert_eq!(r.wire_stats().lost_pre_fec, 100);
        assert_eq!(r.wire_stats().run_hist.iter().sum::<u64>(), 1);
    }

    #[test]
    fn a_gap_that_opened_no_run_is_reported_once_traffic_is_quiet() {
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        // 100..120 lost (20), 10 more arrivals, then silence: the gap is
        // inside the reorder window, so no slot of it ever settles by itself.
        feed(&mut r, 0, 130, 100..120);
        let a = report(&mut r, 250_000);
        assert_eq!((a.lost_pre_fec, a.loss_run_hist), (0, [0; 8]));
        let got = poll(&mut r, 300_000, 3_000_000, 50_000);
        let (at, b) = got.first().expect("the gap was never reported");
        assert!(
            *at <= 250_000 + QUIET_SETTLE_US + LOSS_REPORT_INTERVAL_US + 50_000,
            "at {at}: well inside the server's 10 s"
        );
        assert_eq!((b.lost_pre_fec, b.loss_run_hist[6]), (20, 1));
        assert_eq!(b.last_bundle_seq, 129);
        assert!(got.len() == 1, "reported once: {}", got.len());
    }

    /// An outage of `gap` datagrams, then `rate` datagrams a second. Returns
    /// every report in the following `secs` seconds, polled every 33 ms.
    fn trickle_after_outage(gap: u32, rate: u64, secs: u64) -> (LossReporter, Vec<LossReport>) {
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        feed(&mut r, 0, 100, 0..0);
        let mut seq = 100 + gap;
        let mut reports = Vec::new();
        let step = 1_000_000 / rate;
        let mut t = 250_000u64;
        let end = t + secs * 1_000_000;
        let mut next_dg = t;
        while t <= end {
            if t >= next_dg {
                r.on_arrival(seq);
                seq += 1;
                next_dg += step;
            }
            if let Some(d) = r.maybe_report(t, &BlockStats::default()) {
                reports.push(LossReport::decode(&d).unwrap());
            }
            t += 1_000;
        }
        (r, reports)
    }

    #[test]
    fn an_outage_followed_by_a_trickle_is_one_run_not_pieces() {
        // 344 lost, then 30 datagrams/s: 65 arrivals settle it only after 2 s,
        // so it is force-closed at 1 s. It used to be cut at 1 s and 2 s into
        // runs of 310, 31 and 3, and the 3 looked like a short burst.
        let (r, reports) = trickle_after_outage(344, 30, 5);
        let lost: u64 = reports.iter().map(|g| u64::from(g.lost_pre_fec)).sum();
        let hist = reports.iter().fold([0u64; 8], |mut h, g| {
            for (i, x) in g.loss_run_hist.iter().enumerate() {
                h[i] += u64::from(*x);
            }
            h
        });
        assert_eq!(lost, 344);
        assert_eq!(hist, [0, 0, 0, 0, 0, 0, 0, 1], "one run, in the >32 bucket");
        assert_eq!(r.wire_stats().run_hist.iter().sum::<u64>(), 1);
    }

    #[test]
    fn a_short_head_then_a_pause_is_one_run_of_its_true_length() {
        // Gap 70, one arrival, then a pause: 6 of the 70 are settled, 64 are
        // waiting. The forced close used to report a run of 6.
        let mut r = LossReporter::new();
        assert!(r.maybe_report(0, &BlockStats::default()).is_none());
        feed(&mut r, 0, 100, 0..0);
        r.on_arrival(170);
        let got = poll(&mut r, 250_000, 4_000_000, 33_000);
        let hist = got.iter().fold([0u64; 8], |mut h, (_, g)| {
            for (i, x) in g.loss_run_hist.iter().enumerate() {
                h[i] += u64::from(*x);
            }
            h
        });
        assert_eq!(hist, [0, 0, 0, 0, 0, 0, 0, 1]);
        assert_eq!(
            got.iter()
                .map(|(_, g)| u64::from(g.lost_pre_fec))
                .sum::<u64>(),
            70
        );
    }

    #[test]
    fn residual_lost_across_reports_is_the_change_of_the_cumulative_count() {
        let mut r = LossReporter::new();
        let mut blocks = BlockStats::default();
        assert!(r.maybe_report(0, &blocks).is_none());
        // 100..=139 lost, 170 sent: the run is open. A block inside it is
        // rebuilt (3 sources) before the run closes.
        feed(&mut r, 0, 170, 100..140);
        blocks.rebuilt = 3;
        let dg = r.maybe_report(250_000, &blocks).expect("due");
        let a = LossReport::decode(&dg).unwrap();
        assert_eq!((a.lost_pre_fec, a.recovered, a.residual_lost), (0, 3, 0));
        feed(&mut r, 170, 300, 0..0);
        let dg = r.maybe_report(500_000, &blocks).expect("due");
        let b = LossReport::decode(&dg).unwrap();
        assert_eq!((b.lost_pre_fec, b.recovered), (40, 0));
        assert_eq!(
            b.residual_lost, 37,
            "40 lost, 3 of them rebuilt in the report before"
        );
        // The sum of the residuals is the cumulative lost - rebuilt.
        assert_eq!(a.residual_lost + b.residual_lost, 40 - 3);
    }

    #[test]
    fn residual_lost_is_a_high_water_mark() {
        let mut r = LossReporter::new();
        let mut blocks = BlockStats::default();
        assert!(r.maybe_report(0, &blocks).is_none());
        let mut at = 0u64;
        r.on_arrival(0);
        let mut seq = 1u32;
        let mut residuals = Vec::new();
        // Cumulative residual 10, then 0 (rebuilt catches up), then 5.
        for (lost_total, rebuilt) in [(10u32, 0u64), (10, 10), (15, 10)] {
            let have = r.wire_stats().lost_in_runs as u32;
            // `lost_total - have` more datagrams lost, 200 arrive after them
            // so the run is closed.
            seq += lost_total - have;
            for _ in 0..200 {
                r.on_arrival(seq);
                seq += 1;
            }
            blocks.rebuilt = rebuilt;
            at += 300_000;
            let dg = r.maybe_report(at, &blocks).expect("due");
            residuals.push(LossReport::decode(&dg).unwrap().residual_lost);
        }
        assert_eq!(residuals, vec![10, 0, 0], "5 is under the mark of 10");
    }
}
