//! Reconstructs a monotonic millisecond timeline from the 16-bit timestamps
//! carried on the wire.
//!
//! `arrival_time_ms_lo16` (ACK envelope) and the server emit timestamp are
//! both low-16-bits of a millisecond clock, so they wrap every 65,536 ms.
//! The previous EWMA estimator tolerated that because it only compared coarse
//! differences; a delay-gradient controller does not — a wrap reads as a
//! 65-second backward jump and poisons the gradient signal.

/// Half a wrap period. A step larger than this in either direction is read as
/// a wrap rather than a genuine jump — the standard sequence-space
/// disambiguation. Correct as long as consecutive samples are less than ~32 s
/// apart, and ACK batches arrive far more often than that.
#[allow(dead_code)]
const HALF_PERIOD: i64 = 32_768;
#[allow(dead_code)]
const PERIOD: i64 = 65_536;

/// Per-series unwrapper. One instance per timestamp series — the emit series
/// and the arrival series come from different clocks and must NOT share one.
#[allow(dead_code)]
#[derive(Debug, Default)]
pub(crate) struct Lo16Timeline {
    last: Option<u64>,
}

impl Lo16Timeline {
    /// Map the next 16-bit sample onto the monotonic timeline.
    #[allow(dead_code)]
    pub(crate) fn unwrap_ms(&mut self, lo16: u16) -> u64 {
        let Some(last) = self.last else {
            // Anchor on the first value so early timestamps stay small and
            // readable in logs.
            self.last = Some(lo16 as u64);
            return lo16 as u64;
        };

        let base = (last as i64) & !(PERIOD - 1);
        let mut candidate = base | (lo16 as i64);
        // Choose the wrap-period offset landing nearest `last`.
        if candidate - (last as i64) > HALF_PERIOD {
            candidate -= PERIOD;
        } else if (last as i64) - candidate > HALF_PERIOD {
            candidate += PERIOD;
        }
        let value = candidate.max(0) as u64;
        self.last = Some(value);
        value
    }
}

#[cfg(test)]
mod tests {
    use super::Lo16Timeline;

    #[test]
    fn first_sample_anchors_without_jumping() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(5_000), 5_000);
    }

    #[test]
    fn monotonic_within_a_wrap_period() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(1_000), 1_000);
        assert_eq!(t.unwrap_ms(2_000), 2_000);
        assert_eq!(t.unwrap_ms(30_000), 30_000);
        // 30 s step: under HALF_PERIOD, so unambiguous.
        assert_eq!(t.unwrap_ms(60_000), 60_000);
    }

    #[test]
    fn carries_across_a_wrap() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(65_000), 65_000);
        // 65_000 + 1_000 wraps to 464 in u16 space.
        assert_eq!(t.unwrap_ms(464), 66_000);
        assert_eq!(t.unwrap_ms(1_464), 67_000);
    }

    #[test]
    fn small_backward_step_stays_backward_not_wrapped() {
        // Reordered ACK: 50 ms earlier must read as 50 ms earlier, not as
        // one wrap period later.
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(10_000), 10_000);
        assert_eq!(t.unwrap_ms(9_950), 9_950);
    }

    #[test]
    fn many_consecutive_wraps_stay_monotonic() {
        let mut t = Lo16Timeline::default();
        let mut expected: u64 = 0;
        for step in 0..1_000u64 {
            expected = step * 500;
            let lo16 = (expected % 65_536) as u16;
            assert_eq!(t.unwrap_ms(lo16), expected, "diverged at step {step}");
        }
        assert_eq!(expected, 499_500);
    }

    /// A gap larger than half a wrap period is genuinely ambiguous: in 16-bit
    /// space a 58 s forward step and a 7.5 s backward step are the same bits.
    /// Nearest-value disambiguation resolves it backward, which is the right
    /// call for our traffic — ACK batches arrive milliseconds apart, so a
    /// >32 s forward gap is far less likely than a reorder.
    ///
    /// This is a precondition, not a defect. If a caller ever needs to
    /// tolerate multi-minute silences, the wire field must widen; the
    /// unwrapper cannot recover information the 16 bits do not carry.
    #[test]
    fn a_gap_over_half_a_period_resolves_backward_by_design() {
        let mut t = Lo16Timeline::default();
        assert_eq!(t.unwrap_ms(2_000), 2_000);
        // 58 s forward is read as 7.5 s backward, then clamped at zero.
        assert_eq!(t.unwrap_ms(60_000), 0);
    }
}
