// SPDX-License-Identifier: MIT OR Apache-2.0
//! The render thread's frame clock: when `flush` + `publish` may run.
//!
//! Applying an event to the `Renderer` is cheap; `flush` (a compute pass and a
//! submit, which ends a panfrost job batch), the export blit, `publish`'s
//! blocking `device.poll`, the host's present and the compositor's repaint are
//! not. Under trickle load -- one packet's worth of tiles at a time -- running
//! them once per drained batch means many cycles per display refresh, almost
//! all of which no one can see. So events are applied as they arrive and the
//! expensive half is paced to at most one per [`FrameClock::interval`], with
//! the damage accumulating in between (as EFL does between animator ticks).
//!
//! Pure on purpose: callers pass `now`, so the policy is testable without
//! sleeping and without reading a clock of its own.

use std::time::{Duration, Instant};

/// One display refresh at 60 Hz. The default for [`FrameClock::new`]; a
/// different panel rate only needs a different interval.
pub(crate) const DEFAULT_FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// What to do with a pending render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pace {
    /// Flush and publish now.
    Now,
    /// Keep applying events; flush and publish at (or after) this instant,
    /// even if nothing else arrives before then.
    WaitUntil(Instant),
}

pub(crate) struct FrameClock {
    interval: Duration,
    /// When the last flush + publish was attempted. `None` before the first.
    last: Option<Instant>,
}

impl FrameClock {
    pub(crate) fn new(interval: Duration) -> Self {
        FrameClock {
            interval,
            last: None,
        }
    }

    /// Decide for a render that is due at `now`.
    ///
    /// Inside the interval the answer is always "wait for its end", whether or
    /// not the previous frame has been taken: the host takes a frame by
    /// emptying the `published` slot, which sends the render thread no
    /// message, so the slot can only be re-read at a wake-up we schedule.
    ///
    /// `unacquired` is whether the previous publish is still in that slot at
    /// `now`. Publishing over it withdraws it, which discards the damage chain
    /// and makes the next present whole-frame. The host normally takes a frame
    /// within microseconds of `FrameReady`, so a frame still there after a
    /// whole interval means a busy host. Waiting one more interval for it costs
    /// that host latency it is already incurring and keeps the partial present
    /// path alive; waiting longer would let a stuck host pin the picture, so
    /// after two intervals the frame is withdrawn as before.
    ///
    /// A render due after the quiet period (one interval, or two for an
    /// unacquired frame) goes out at once: an isolated keystroke echo never
    /// waits. Otherwise the added latency is bounded by that same period.
    pub(crate) fn decide(&self, now: Instant, unacquired: bool) -> Pace {
        let Some(last) = self.last else {
            return Pace::Now;
        };
        let end = last + self.interval;
        if now < end {
            return Pace::WaitUntil(end);
        }
        let give_up = end + self.interval;
        if unacquired && now < give_up {
            Pace::WaitUntil(give_up)
        } else {
            Pace::Now
        }
    }

    /// Record a flush + publish attempt made at `now`. Attempts count even when
    /// the ring was starved, so a starved ring is not hammered per event.
    ///
    /// `woke_for` is the deadline the loop was waiting for, if any. An attempt
    /// that ran at or just after it is anchored to the deadline itself, not to
    /// `now`: the scheduling slack of one wake-up must not accumulate into the
    /// next interval, or the cadence drifts to `interval + slack` and steady
    /// content periodically misses a refresh. A deadline more than an interval
    /// stale, or one that has not been reached, is not trusted.
    pub(crate) fn attempted(&mut self, now: Instant, woke_for: Option<Instant>) {
        self.last = Some(match woke_for {
            Some(d) if now >= d && now.saturating_duration_since(d) < self.interval => d,
            Some(_) | None => now,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const I: Duration = Duration::from_millis(16);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    #[test]
    fn the_first_render_is_immediate() {
        let c = FrameClock::new(I);
        assert_eq!(c.decide(Instant::now(), false), Pace::Now);
    }

    #[test]
    fn a_render_within_the_interval_waits_for_its_end() {
        let base = Instant::now();
        let mut c = FrameClock::new(I);
        c.attempted(base, None);
        assert_eq!(c.decide(at(base, 3), false), Pace::WaitUntil(at(base, 16)));
        // The deadline does not slide with `now`.
        assert_eq!(c.decide(at(base, 15), false), Pace::WaitUntil(at(base, 16)));
    }

    #[test]
    fn a_render_after_a_quiet_interval_is_immediate() {
        let base = Instant::now();
        let mut c = FrameClock::new(I);
        c.attempted(base, None);
        assert_eq!(c.decide(at(base, 16), false), Pace::Now);
        assert_eq!(c.decide(at(base, 500), false), Pace::Now);
        assert_eq!(c.decide(at(base, 500), true), Pace::Now);
    }

    #[test]
    fn an_unacquired_frame_is_given_a_second_interval_then_withdrawn() {
        let base = Instant::now();
        let mut c = FrameClock::new(I);
        c.attempted(base, None);
        assert_eq!(c.decide(at(base, 16), true), Pace::WaitUntil(at(base, 32)));
        assert_eq!(c.decide(at(base, 20), true), Pace::WaitUntil(at(base, 32)));
        assert_eq!(c.decide(at(base, 32), true), Pace::Now);
        // Acquired in the meantime: back to one interval.
        assert_eq!(c.decide(at(base, 17), false), Pace::Now);
    }

    /// The host's acquire sends no message, so inside the first interval the
    /// clock must schedule a re-check at its end instead of committing to the
    /// longer unacquired hold: a frame taken a moment later must not cost the
    /// next event a second interval.
    #[test]
    fn inside_the_interval_the_wait_ends_at_one_interval_even_if_unacquired() {
        let base = Instant::now();
        let mut c = FrameClock::new(I);
        c.attempted(base, None);
        assert_eq!(c.decide(at(base, 1), true), Pace::WaitUntil(at(base, 16)));
        assert_eq!(c.decide(at(base, 1), false), Pace::WaitUntil(at(base, 16)));
    }

    /// A deadline wake that runs late must not push the next deadline out by
    /// its lateness.
    #[test]
    fn a_late_deadline_wake_does_not_shift_the_cadence() {
        let base = Instant::now();
        let mut c = FrameClock::new(I);
        c.attempted(base, None);
        // Woke for base+16 but ran at base+19.
        c.attempted(at(base, 19), Some(at(base, 16)));
        assert_eq!(c.decide(at(base, 20), false), Pace::WaitUntil(at(base, 32)));
        // Not a deadline wake (nothing was waited for): anchored to now.
        c.attempted(at(base, 100), None);
        assert_eq!(
            c.decide(at(base, 101), false),
            Pace::WaitUntil(at(base, 116))
        );
        // A stale deadline, or one not yet reached, is not trusted.
        c.attempted(at(base, 200), Some(at(base, 150)));
        assert_eq!(
            c.decide(at(base, 201), false),
            Pace::WaitUntil(at(base, 216))
        );
        c.attempted(at(base, 300), Some(at(base, 310)));
        assert_eq!(
            c.decide(at(base, 301), false),
            Pace::WaitUntil(at(base, 316))
        );
    }

    #[test]
    fn the_wait_never_exceeds_two_intervals() {
        let base = Instant::now();
        let mut c = FrameClock::new(I);
        for ms in [0u64, 7, 100] {
            c.attempted(at(base, ms), None);
            for off in 0..80 {
                let now = at(base, ms + off);
                for unacquired in [false, true] {
                    if let Pace::WaitUntil(t) = c.decide(now, unacquired) {
                        assert!(t.saturating_duration_since(now) <= I * 2);
                        assert!(t > now);
                    }
                }
            }
        }
    }
}
