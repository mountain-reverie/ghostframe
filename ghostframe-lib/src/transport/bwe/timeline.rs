//! Reconstructs a monotonic microsecond timeline from the 32-bit timestamps
//! carried on the wire.
//!
//! `arrival_time_us_lo32` (ACK envelope) and the server emit timestamp are
//! both low-32-bits of a microsecond clock, so they wrap every 2^32 us
//! (~71.6 minutes). The previous EWMA estimator tolerated wrapping because it
//! only compared coarse differences; a delay-gradient controller does not —
//! a wrap reads as a 71.6-minute backward jump and poisons the gradient
//! signal.
//!
//! Backward steps are also expected in normal operation, not just at the
//! wrap boundary: an ACK batch is unwrapped as a fresh base plus a set of
//! overlap entries replayed from previous batches, and overlap entries are
//! older than the fresh base they follow. The caller unwraps each overlap
//! entry against the same series *after* the newer fresh base, so a step
//! backwards is routine and must be read as "earlier", not misread as having
//! wrapped forward by a full period.

/// Half a wrap period. A step larger than this in either direction is read as
/// a wrap rather than a genuine jump — the standard sequence-space
/// disambiguation. Correct as long as consecutive samples are less than
/// ~35.8 minutes apart, and ACK batches (plus their overlap entries) arrive
/// far more often than that.
const HALF_PERIOD: i64 = 1 << 31;
const PERIOD: i64 = 1 << 32;

/// Per-series unwrapper. One instance per timestamp series — the emit series
/// and the arrival series come from different clocks and must NOT share one.
/// State is kept unclamped (may be negative) so that a backward step near the
/// anchor does not corrupt subsequent deltas; only the returned value is floored
/// at zero.
#[derive(Debug, Default)]
pub(crate) struct Lo32Timeline {
    last: Option<i64>,
}

impl Lo32Timeline {
    /// Map the next 32-bit microsecond sample onto the monotonic timeline.
    ///
    /// Overlap entries from an ACK batch are older than the fresh entries
    /// unwrapped just before them, so a backward step here is expected
    /// behaviour, not a sign of a lost wrap.
    pub(crate) fn unwrap_us(&mut self, lo32: u32) -> u64 {
        let Some(last) = self.last else {
            // Anchor on the first value so early timestamps stay small and
            // readable in logs.
            self.last = Some(lo32 as i64);
            return lo32 as u64;
        };

        let base = last & !(PERIOD - 1);
        let mut candidate = base | (lo32 as i64);
        // Choose the wrap-period offset landing nearest `last`.
        if candidate - last > HALF_PERIOD {
            candidate -= PERIOD;
        } else if last - candidate > HALF_PERIOD {
            candidate += PERIOD;
        }
        let value = candidate.max(0) as u64;
        self.last = Some(candidate);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::{Lo32Timeline, HALF_PERIOD, PERIOD};

    #[test]
    fn first_sample_anchors_without_jumping() {
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(5_000), 5_000);
    }

    #[test]
    fn monotonic_within_a_wrap_period() {
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(1_000_000), 1_000_000);
        assert_eq!(t.unwrap_us(2_000_000), 2_000_000);
        assert_eq!(t.unwrap_us(1_000_000_000), 1_000_000_000);
        // ~1000 s step: under HALF_PERIOD (~1073 s), so unambiguous.
        assert_eq!(t.unwrap_us(2_000_000_000), 2_000_000_000);
    }

    #[test]
    fn carries_across_a_wrap() {
        let mut t = Lo32Timeline::default();
        let near_top = u32::MAX as u64 - 1_000;
        assert_eq!(t.unwrap_us(near_top as u32), near_top);
        // near_top + 2_000 wraps past the top of the 32-bit space.
        assert_eq!(t.unwrap_us(999), near_top + 2_000);
        assert_eq!(t.unwrap_us(1_999), near_top + 3_000);
    }

    #[test]
    fn small_backward_step_stays_backward_not_wrapped() {
        // Reordered ACK: 50 us earlier must read as 50 us earlier, not as
        // one wrap period later.
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(10_000), 10_000);
        assert_eq!(t.unwrap_us(9_950), 9_950);
    }

    #[test]
    fn many_consecutive_wraps_stay_monotonic() {
        let mut t = Lo32Timeline::default();
        let mut expected: u64 = 0;
        for step in 0..1_000u64 {
            expected = step * 500_000_000;
            let lo32 = (expected % (1u64 << 32)) as u32;
            assert_eq!(t.unwrap_us(lo32), expected, "diverged at step {step}");
        }
        assert_eq!(expected, 499_500_000_000);
    }

    /// A gap larger than half a wrap period is genuinely ambiguous: in
    /// 32-bit space a forward step just over 35.8 minutes and a backward
    /// step just under 35.8 minutes are the same bits. Nearest-value
    /// disambiguation resolves it backward, which is the right call for our
    /// traffic — ACK batches arrive far more often than every 35.8 minutes,
    /// so a forward gap that large is far less likely than a reorder.
    ///
    /// This is a precondition, not a defect. If a caller ever needs to
    /// tolerate multi-hour silences, the wire field must widen; the
    /// unwrapper cannot recover information the 32 bits do not carry.
    #[test]
    fn a_gap_over_half_a_period_resolves_backward_by_design() {
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(2_000), 2_000);
        // ~2.2 billion us forward (over half the ~71.6-minute period) is
        // read as backward instead, then clamped at zero.
        assert_eq!(t.unwrap_us(2_200_000_000), 0);
    }

    /// The zero floor applies to the returned value only; the timeline must
    /// remember the true (possibly negative) position. Storing the floored
    /// value silently moves the reference point, which changes how a later
    /// sample near the half-period boundary is disambiguated.
    #[test]
    fn the_zero_floor_does_not_corrupt_stored_state() {
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(100), 100);
        // 136us before the anchor: floors to 0 on the way out, but the
        // timeline must remember -36.
        assert_eq!(t.unwrap_us((PERIOD as u32).wrapping_sub(36)), 0);
        // Near the half-period boundary measured from -36: the nearest
        // candidate is -(HALF_PERIOD + 14), which floors to 0. Had the
        // floored 0 been stored instead, the nearest candidate would be
        // +(HALF_PERIOD - 14) and this would return a multi-minute phantom
        // jump forward.
        assert_eq!(t.unwrap_us((HALF_PERIOD - 14) as u32), 0);
    }

    #[test]
    fn microsecond_timeline_unwraps_across_a_32_bit_wrap() {
        let mut t = Lo32Timeline::default();
        let near_top = u32::MAX as u64 - 1_000;
        assert_eq!(t.unwrap_us(near_top as u32), near_top);
        // 2,000us later, having wrapped past the top of the 32-bit space.
        assert_eq!(t.unwrap_us(999), near_top + 2_000);
    }

    #[test]
    fn an_overlap_entry_behind_the_base_stays_behind_it() {
        // Overlap entries are older than the fresh base and reach the
        // unwrapper after it. A backward step must read as earlier, not as a
        // 71-minute jump forward.
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(5_000), 5_000);
        assert_eq!(t.unwrap_us(4_000), 4_000);
    }

    #[test]
    fn a_backward_step_across_the_wrap_reads_as_earlier() {
        // The hard case: the base sits just above the wrap, the overlap entry
        // just below it. The naive reading is "71 minutes later".
        let mut t = Lo32Timeline::default();
        assert_eq!(t.unwrap_us(1_000), 1_000);
        let before_wrap = u32::MAX; // 1,001us earlier in sequence space
        let got = t.unwrap_us(before_wrap);
        assert!(got < 1_000, "expected a value before 1_000us, got {got}");
    }

    #[test]
    fn a_long_ascending_series_stays_monotonic_across_several_wraps() {
        let mut t = Lo32Timeline::default();
        let mut last = t.unwrap_us(0);
        // 5 wraps' worth, stepping just under half a period so each step is
        // unambiguously forward.
        for i in 1..=10u64 {
            let raw = (i.wrapping_mul(2_000_000_000)) as u32;
            let got = t.unwrap_us(raw);
            assert!(got > last, "step {i} went backwards: {last} -> {got}");
            last = got;
        }
    }
}
