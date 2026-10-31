//! The estimate ceiling: a bound on the bandwidth estimate taken from what the
//! path was measured to deliver, and the measurements that bound is made of.
//!
//! # Why
//!
//! goog_cc's delay detector is what keeps its estimate honest, and the
//! estimate rides at its additive-increase limit, `1.5 x` the acknowledged
//! rate, until the detector fires. On a path whose queue hides its depth from
//! a delay gradient (CoDel drops the head of a standing queue, so the
//! gradient is a 1-8 ms sawtooth) the detector never fires, and the estimate
//! sits at that limit: measured on a 10 Mbit CoDel link, `1.26 x` the link,
//! never converging. Before the departure tracker (D1 PR-3) the same scene
//! read `0.94 x`, because send times were taken at enqueue and the probes
//! looked saturated; that was an accident of dishonest stamps, not a property
//! worth keeping.
//!
//! # What
//!
//! `ceiling = K x A`, where `A` is the acknowledged rate over a trailing
//! window, **in the units goog_cc counts**: the wire size of each
//! acknowledged transmission (`AckArrival::size_bytes`), the same bytes the
//! estimate is a rate of. It is applied only while the sender was *saturated*
//! (see below). Otherwise there is no ceiling.
//!
//! * **`K = 1 / beta`, `beta = 0.85`**, AIMD's multiplicative back-off
//!   (`DEFAULT_BACKOFF_FACTOR`, goog_cc `aimd_rate_control.rs`). On overuse
//!   goog_cc sets the estimate to `beta x` the acknowledged rate; `K` is the
//!   highest estimate from which one such step lands on the acknowledged rate
//!   itself. Anything above needs more than one back-off to come down to what
//!   the path delivers. It is also the headroom the estimate may grow in: a
//!   path with room is found `K`-fold per window, not capped.
//! * **Window `W = max(4 x srtt, 150 ms)`.** 150 ms is goog_cc's own steady
//!   acknowledged-rate window (`bitrate_estimator.rs`), so `A` is at least as
//!   smooth as the number being bounded; `4 x srtt` makes it span several
//!   round trips.
//! * **Saturated.** `A` is evidence about the path only if the sender was
//!   offering as much as it was allowed to. Over the same window, shifted back
//!   by one smoothed round trip (an acknowledgement describes a send that
//!   happened a round trip earlier), the sender must have spent at least
//!   `SATURATION = 0.9` of the tokens the admission bucket granted it. A
//!   sender with nothing to send (a static screen, cursor-sized updates) uses
//!   a small share of its grant, so `A` is its own send rate and says nothing
//!   about capacity: **no ceiling is applied**. Clamping the estimate to a
//!   rate the application chose would leave nothing to climb from the moment
//!   the screen changes. `0.9`: the bucket cannot be emptied more
//!   completely than one datagram's rounding per wake (a few percent at the
//!   rates served), and a sender that spends under nine tenths of its grant
//!   is not rate-limited.
//! * **History.** No ceiling until a full window (plus the shift) of both
//!   series exists, so startup is never bounded by a measurement of nothing.
//!   A gap in acknowledgements is not a lack of history: the counters hold.
//!
//! The reading also carries the **payload fraction**, countable bytes per
//! token spent: admission bills parity, framing and control datagrams that no
//! acknowledgement names, so a sender paced at the estimate offers goog_cc
//! `fraction x` it (0.82 to 0.86 measured). Admission divides the estimate by it. The
//! units matter for the ceiling too: without the division `K x A` fell under
//! the estimate whenever the fraction was under 0.85 (`K x fraction < 1`), and a sender paced at its
//! ceiling shrank it on every window.
//!
//! All state is cumulative series sampled at event times; every figure is a
//! difference of two lookups, so the work per event is a binary search.

use std::collections::VecDeque;

/// AIMD's multiplicative back-off, `DEFAULT_BACKOFF_FACTOR` in goog_cc's
/// `aimd_rate_control.rs`. Not exported by the crate, so mirrored here.
pub(crate) const AIMD_BACKOFF: f64 = 0.85;

/// Estimate headroom over the acknowledged rate: `1 / AIMD_BACKOFF`.
pub(crate) const HEADROOM: f64 = 1.0 / AIMD_BACKOFF;

/// Share of the granted tokens the sender must have spent for the
/// acknowledged rate to count as the path's answer.
pub(crate) const SATURATION: f64 = 0.9;

// A sender that spent at least `SATURATION` of its grant and had everything
// acknowledged has a ceiling `K x A = K x spent share x grant` at or above its
// grant exactly when the share is at least `AIMD_BACKOFF`. Below that, the
// ceiling would cut a path-limited-by-nothing sender under its own grant.
const _: () = assert!(SATURATION > AIMD_BACKOFF);

/// Spend over the window below which the payload fraction is not measured:
/// a handful of datagrams is all rounding.
const MIN_SPEND_FOR_FRACTION: f64 = 8_000.0;

/// The payload fraction is never read below this: parity alone is `1/11` and
/// framing a few percent, so a reading under it is a window that straddled a
/// burst of control traffic, not the sender's overhead.
const MIN_PAYLOAD_FRACTION: f64 = 0.5;

/// Floor of the rate window, goog_cc's steady acknowledged-rate window.
const WINDOW_FLOOR_US: u64 = 150_000;

/// Round trips the window spans.
const WINDOW_SRTTS: u64 = 4;

/// Extra history kept beyond a window and its shift, for a round trip that
/// moved between two reads.
const PRUNE_SLACK_US: u64 = 50_000;

/// History older than this is never needed (the longest window plus its
/// shift): bounds memory when the round trip is absurd.
const HISTORY_CAP_US: u64 = 10_000_000;

/// A monotone cumulative counter sampled at event times.
#[derive(Debug, Default)]
struct Series {
    points: VecDeque<(u64, f64)>,
}

impl Series {
    fn push(&mut self, at_us: u64, cumulative: f64) {
        // The counters are monotone, but one of them is a net figure: tokens
        // spent less the refunds of sends quinn refused, which can step down.
        // A step down is not a negative spend over the window, so the series
        // holds its value instead of falling.
        let cumulative = self
            .points
            .back()
            .map_or(cumulative, |last| cumulative.max(last.1));
        // Keep a single point per instant: the last one wins.
        match self.points.back_mut() {
            Some(last) if last.0 >= at_us => last.1 = cumulative,
            _ => self.points.push_back((at_us, cumulative)),
        }
    }

    /// The counter's value at `at_us`: the last sample at or before it.
    /// `None` when `at_us` precedes the first sample (no history that far).
    fn at(&self, at_us: u64) -> Option<f64> {
        let n = self.points.partition_point(|p| p.0 <= at_us);
        n.checked_sub(1).map(|i| self.points[i].1)
    }

    fn first_us(&self) -> Option<u64> {
        self.points.front().map(|p| p.0)
    }

    /// Drop samples that no lookup at or after `keep_from_us` can need,
    /// keeping the one at or before it.
    fn prune(&mut self, keep_from_us: u64) {
        while self.points.len() > 1 && self.points[1].0 <= keep_from_us {
            self.points.pop_front();
        }
    }
}

/// What the ceiling is made of at one moment, for scenes.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CeilingReading {
    /// Acknowledged rate over the window, bits per second.
    pub(crate) acked_bps: f64,
    /// Share of the granted tokens the sender spent over the shifted window.
    pub(crate) saturation: f64,
    /// `HEADROOM x acked_bps`, or `None` when the sender was not saturated.
    pub(crate) ceiling_bps: Option<f64>,
    /// Countable bytes offered over the shifted window, as a rate in bits per
    /// second: what the sender put on the books, to set `acked_bps` against.
    pub(crate) offered_bps: f64,
    /// Countable bytes (what goog_cc is told of) per token spent, over the
    /// shifted window: below 1 by the parity, framing and control bytes that
    /// admission bills and no acknowledgement names. `1.0` while there is
    /// too little spend to say.
    pub(crate) payload_fraction: f64,
}

#[derive(Debug, Default)]
pub(crate) struct EstimateCeiling {
    /// Cumulative acknowledged wire bytes.
    acked: Series,
    /// Cumulative tokens the admission bucket granted, bytes.
    granted: Series,
    /// Cumulative tokens spent, bytes.
    spent: Series,
    /// Cumulative wire bytes of the transmissions put on the books: what
    /// goog_cc counts of what was sent (`Transmission::wire_bytes`).
    offered: Series,
}

impl EstimateCeiling {
    /// Wire bytes acknowledged at `at_us` (a batch of acknowledgements).
    pub(crate) fn note_acked(&mut self, at_us: u64, cumulative_bytes: f64) {
        self.acked.push(at_us, cumulative_bytes);
    }

    /// Wire bytes of the transmissions recorded by `at_us`, cumulative.
    pub(crate) fn note_offered(&mut self, at_us: u64, cumulative_bytes: f64) {
        self.offered.push(at_us, cumulative_bytes);
    }

    /// The admission bucket's cumulative grant and spend at `at_us`.
    pub(crate) fn note_admission(&mut self, at_us: u64, granted: f64, spent: f64) {
        self.granted.push(at_us, granted);
        self.spent.push(at_us, spent);
    }

    /// The window the rate is taken over for a smoothed round trip.
    pub(crate) fn window_us(srtt_us: u64) -> u64 {
        (WINDOW_SRTTS * srtt_us).max(WINDOW_FLOOR_US)
    }

    /// The longest stretch without an acknowledgement inside the window that
    /// ends at `now_us` (the quiet before `now_us` counts). A stall or a
    /// blackout reads as a long gap; a path carrying less reads as small
    /// gaps with little in them.
    pub(crate) fn ack_gap_us(&self, now_us: u64, srtt_us: u64) -> u64 {
        let from = now_us.saturating_sub(Self::window_us(srtt_us));
        let mut prev = from;
        let mut gap = 0;
        for &(t, _) in self.acked.points.iter().filter(|p| p.0 > from) {
            gap = gap.max(t.saturating_sub(prev));
            prev = t;
        }
        gap.max(now_us.saturating_sub(prev))
    }

    /// The ceiling at `now_us`. `None` until there is a full window of
    /// history; `ceiling_bps` inside is `None` while the sender was not
    /// saturated.
    pub(crate) fn read(&mut self, now_us: u64, srtt_us: u64) -> Option<CeilingReading> {
        let w = Self::window_us(srtt_us.min(HISTORY_CAP_US / (WINDOW_SRTTS + 1)));
        let lag = srtt_us.min(HISTORY_CAP_US / (WINDOW_SRTTS + 1));
        // Prune with margin: the smoothed round trip moves between reads, and
        // a lookup made with a longer one must still find its sample.
        let keep = now_us.saturating_sub(3 * (w + lag) / 2 + PRUNE_SLACK_US);
        self.acked.prune(keep);
        self.granted.prune(keep);
        self.spent.prune(keep);
        self.offered.prune(keep);

        let acked_from = now_us.checked_sub(w)?;
        // The send side is read one round trip back.
        let sent_to = now_us.checked_sub(lag)?;
        let sent_from = sent_to.checked_sub(w)?;
        // A full window of both series must exist.
        if self.acked.first_us()? > acked_from || self.granted.first_us()? > sent_from {
            return None;
        }
        let acked = self.acked.at(now_us)? - self.acked.at(acked_from)?;
        let granted = self.granted.at(sent_to)? - self.granted.at(sent_from)?;
        let spent = self.spent.at(sent_to)? - self.spent.at(sent_from)?;
        let acked_bps = acked * 8.0 * 1e6 / w as f64;
        // No grant over the window (a zero rate) tells nothing.
        let saturation = if granted > 0.0 { spent / granted } else { 0.0 };
        // Countable bytes per token spent. Needs a few datagrams of spend to
        // mean anything; before that, no overhead is assumed.
        let offered = match (self.offered.at(sent_to), self.offered.at(sent_from)) {
            (Some(a), Some(b)) => (a - b).max(0.0),
            _ => 0.0,
        };
        let payload_fraction = if spent >= MIN_SPEND_FOR_FRACTION && offered > 0.0 {
            (offered / spent).clamp(MIN_PAYLOAD_FRACTION, 1.0)
        } else {
            1.0
        };
        Some(CeilingReading {
            acked_bps,
            saturation,
            ceiling_bps: (saturation >= SATURATION).then_some(HEADROOM * acked_bps),
            offered_bps: offered * 8.0 * 1e6 / w as f64,
            payload_fraction,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000;

    /// Drive `ceil` for `secs` at 1 ms steps: `grant_bps` granted, `spend`
    /// of the grant spent, `ack_bps` acknowledged (both in bits/s).
    fn drive(
        ceil: &mut EstimateCeiling,
        from_us: u64,
        secs: u64,
        grant_bps: f64,
        spend: f64,
        ack_bps: f64,
        cum: &mut (f64, f64, f64),
    ) {
        for step in 1..=(secs * 1000) {
            let t = from_us + step * MS;
            cum.0 += grant_bps / 8.0 / 1000.0;
            cum.1 += grant_bps / 8.0 / 1000.0 * spend;
            cum.2 += ack_bps / 8.0 / 1000.0;
            ceil.note_admission(t, cum.0, cum.1);
            ceil.note_acked(t, cum.2);
            // 82 % of what is spent is countable.
            ceil.note_offered(t, cum.1 * 0.82);
        }
    }

    #[test]
    fn headroom_is_the_inverse_of_the_aimd_back_off() {
        assert!((HEADROOM * AIMD_BACKOFF - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_saturated_sender_is_bounded_by_headroom_times_what_was_acknowledged() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        // Granted 12 Mbit, all spent, 8 Mbit acknowledged.
        drive(&mut c, 0, 3, 12e6, 1.0, 8e6, &mut cum);
        let r = c.read(3_000 * MS, 20 * MS).expect("a full window exists");
        assert!((r.acked_bps - 8e6).abs() < 8e4, "{r:?}");
        assert!(r.saturation > 0.99);
        let ceiling = r.ceiling_bps.expect("saturated");
        assert!((ceiling - HEADROOM * 8e6).abs() < 1e5, "{ceiling}");
    }

    /// The guard the owner asked for: a sender that spends a tenth of its grant
    /// (app-limited) acknowledges its own low rate, and that must not become a
    /// bound on the estimate.
    #[test]
    fn an_app_limited_sender_is_not_bounded_by_its_own_rate() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        drive(&mut c, 0, 10, 12e6, 0.1, 1.2e6, &mut cum);
        let r = c.read(10_000 * MS, 20 * MS).expect("a full window exists");
        assert!((r.acked_bps - 1.2e6).abs() < 2e4, "{r:?}");
        assert!(r.saturation < 0.2, "{r:?}");
        assert_eq!(
            r.ceiling_bps, None,
            "an idle sender's rate is not a ceiling"
        );
    }

    #[test]
    fn there_is_no_ceiling_before_a_full_window_exists() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        // 100 ms of history against a 150 ms floor.
        drive(&mut c, 0, 0, 12e6, 1.0, 8e6, &mut cum);
        for step in 1..=100u64 {
            let t = step * MS;
            cum.0 += 1500.0;
            cum.1 += 1500.0;
            cum.2 += 1000.0;
            c.note_admission(t, cum.0, cum.1);
            c.note_acked(t, cum.2);
        }
        assert!(c.read(100 * MS, 10 * MS).is_none());
    }

    /// The send side is read one round trip back: a sender that jumped to
    /// saturation a moment ago, whose acknowledgements have not returned,
    /// is not judged on a rate it only just started.
    #[test]
    fn the_send_side_is_read_a_round_trip_back() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        // 1 s idle, then 0.4 s saturated at 10 Mbit acknowledged one rtt late.
        drive(&mut c, 0, 1, 10e6, 0.05, 0.5e6, &mut cum);
        // Saturated for the last 150 ms only.
        for step in 1..=150u64 {
            let t = 1_000 * MS + step * MS;
            cum.0 += 10e6 / 8000.0;
            cum.1 += 10e6 / 8000.0;
            if step > 100 {
                cum.2 += 10e6 / 8000.0;
            }
            c.note_admission(t, cum.0, cum.1);
            c.note_acked(t, cum.2);
        }
        // With a 100 ms round trip the shifted send window is still mostly
        // the idle period, so the sender is not yet "saturated".
        let r = c.read(1_150 * MS, 100 * MS).expect("history");
        assert!(r.ceiling_bps.is_none(), "{r:?}");
    }

    #[test]
    fn memory_is_bounded_over_a_long_session() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        for chunk in 0..120u64 {
            drive(&mut c, chunk * 1000 * MS, 1, 8e6, 1.0, 8e6, &mut cum);
            let _ = c.read((chunk + 1) * 1000 * MS, 20 * MS);
        }
        assert!(c.acked.points.len() < 2_000, "{}", c.acked.points.len());
        assert!(c.granted.points.len() < 2_000);
    }

    /// A gap in acknowledgements (a stalled path, a long probe, a quiet
    /// moment) must not read as "no history": the counters did not move, they
    /// did not vanish.
    #[test]
    fn a_gap_in_acknowledgements_is_not_a_lack_of_history() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        drive(&mut c, 0, 2, 8e6, 1.0, 8e6, &mut cum);
        // 600 ms in which grants and spends continue and nothing is acked.
        for step in 1..=600u64 {
            let t = 2_000 * MS + step * MS;
            cum.0 += 8e6 / 8000.0;
            cum.1 += 8e6 / 8000.0;
            c.note_admission(t, cum.0, cum.1);
            c.note_offered(t, cum.1 * 0.82);
        }
        let r = c.read(2_600 * MS, 20 * MS);
        assert!(r.is_some(), "history vanished across the gap");
        assert!(r.unwrap().acked_bps < 1e5);
    }

    #[test]
    fn a_blackout_reads_as_no_delivery() {
        let mut c = EstimateCeiling::default();
        let mut cum = (0.0, 0.0, 0.0);
        drive(&mut c, 0, 2, 8e6, 1.0, 8e6, &mut cum);
        drive(&mut c, 2_000 * MS, 1, 8e6, 1.0, 0.0, &mut cum);
        let r = c.read(3_000 * MS, 20 * MS).unwrap();
        assert!(r.acked_bps < 1e5, "{r:?}");
        assert!(r.ceiling_bps.unwrap() < 1e5);
    }
}

#[cfg(test)]
mod invariant_tests {
    use super::*;

    const MS: u64 = 1_000;

    /// At every spend share a saturated sender can have (from `SATURATION`
    /// to all of it) with everything it offered acknowledged, the ceiling expressed as a
    /// token rate (the estimate over the payload fraction) is at or above the
    /// grant: the ceiling never cuts a sender that is delivering under its own
    /// grant. Below `AIMD_BACKOFF` of spend it would, which is why
    /// `SATURATION` is above it (and asserted to be, at compile time).
    #[test]
    fn a_saturated_sender_that_is_fully_acknowledged_is_never_cut_under_its_grant() {
        const FRACTION: f64 = 0.82;
        const GRANT_BPS: f64 = 12e6;
        for share in [0.90, 0.92, 0.95, 0.98, 1.0] {
            let mut c = EstimateCeiling::default();
            let (mut granted, mut spent, mut acked) = (0.0, 0.0, 0.0);
            for step in 1..=3_000u64 {
                let t = step * MS;
                granted += GRANT_BPS / 8000.0;
                spent += GRANT_BPS / 8000.0 * share;
                acked += GRANT_BPS / 8000.0 * share * FRACTION;
                c.note_admission(t, granted, spent);
                c.note_offered(t, acked);
                c.note_acked(t, acked);
            }
            let r = c.read(3_000 * MS, 20 * MS).expect("history");
            let ceiling = r.ceiling_bps.expect("saturated");
            assert!(
                ceiling / r.payload_fraction >= GRANT_BPS * 0.99,
                "share {share}: ceiling {ceiling} over fraction {} is under the grant",
                r.payload_fraction
            );
        }
    }

    #[test]
    fn a_stall_inside_the_window_reads_as_a_long_gap() {
        let mut c = EstimateCeiling::default();
        let mut cum = 0.0;
        for step in 1..=1_000u64 {
            cum += 1500.0;
            c.note_acked(step * MS, cum);
        }
        assert!(c.ack_gap_us(1_000 * MS, 20 * MS) <= 2 * MS);
        // 100 ms of silence, then the window is read.
        assert!(c.ack_gap_us(1_100 * MS, 20 * MS) >= 99 * MS);
    }

    #[test]
    fn a_refund_does_not_make_the_spend_series_fall() {
        let mut s = Series::default();
        s.push(1, 100.0);
        s.push(2, 90.0);
        assert_eq!(s.at(2), Some(100.0));
    }
}
