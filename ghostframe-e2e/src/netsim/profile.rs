//! Network impairment profiles: the specification of link behavior.

/// A complete network link profile: loss, burst behavior, duplication,
/// corruption, delay, jitter, reordering, and bandwidth caps.
#[derive(Debug, Clone)]
pub struct NetProfile {
    /// Independent per-datagram drop probability: [0, 1].
    pub loss: f64,

    /// Gilbert-Elliott burst state machine:
    /// probability of entering the bad (high-loss) state.
    pub burst_enter: f64,
    /// Probability of exiting the bad state.
    pub burst_exit: f64,
    /// Drop probability while in the bad state.
    pub burst_loss: f64,

    /// Independent per-datagram duplication probability: [0, 1].
    pub duplicate: f64,
    /// Independent per-datagram bit-flip probability: [0, 1].
    pub corrupt: f64,

    /// One-way propagation delay and jitter, in microseconds.
    pub delay_us: u64,
    pub jitter_us: u64,

    /// Reorder window: datagrams arriving within this time window of each other
    /// may be delivered out of order. 0 disables reordering.
    pub reorder_us: u64,

    /// Bandwidth cap timeline: piecewise constant function of time.
    pub cap: CapTimeline,
}

impl NetProfile {
    /// Construct an ideal (lossless, delay-free, uncapped) profile.
    pub fn perfect() -> Self {
        Self {
            loss: 0.0,
            burst_enter: 0.0,
            burst_exit: 1.0,
            burst_loss: 0.0,
            duplicate: 0.0,
            corrupt: 0.0,
            delay_us: 0,
            jitter_us: 0,
            reorder_us: 0,
            cap: CapTimeline::unlimited(),
        }
    }
}

/// Piecewise constant bandwidth cap as a function of elapsed time.
///
/// Each entry (at_us, bps) indicates that starting at time `at_us`, the cap
/// is `bps` bytes per second. Queries use the most recent entry at or before
/// the current time.
#[derive(Debug, Clone, Default)]
pub struct CapTimeline {
    /// Points in (at_us, bytes_per_second) order, typically sorted.
    pub points: Vec<(u64, u64)>,
}

impl CapTimeline {
    /// Construct an uncapped (infinite bandwidth) timeline.
    pub fn unlimited() -> Self {
        Self { points: Vec::new() }
    }

    /// Construct a constant cap.
    pub fn constant(bps: u64) -> Self {
        Self {
            points: vec![(0, bps)],
        }
    }

    /// Construct a step function: `first_bps` until time `at_us`, then `then_bps`.
    pub fn step(first_bps: u64, at_us: u64, then_bps: u64) -> Self {
        Self {
            points: vec![(0, first_bps), (at_us, then_bps)],
        }
    }

    /// Query the cap in effect at time `now_us`.
    /// Returns the bytes-per-second cap, or `u64::MAX` if unlimited.
    pub fn bps_at(&self, now_us: u64) -> u64 {
        self.points
            .iter()
            .rev()
            .find(|(at_us, _)| *at_us <= now_us)
            .map(|(_, bps)| *bps)
            .unwrap_or(u64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_is_ideal() {
        let p = NetProfile::perfect();
        assert_eq!(p.loss, 0.0);
        assert_eq!(p.delay_us, 0);
        assert_eq!(p.cap.bps_at(0), u64::MAX);
    }

    #[test]
    fn cap_timeline_unlimited() {
        let cap = CapTimeline::unlimited();
        assert_eq!(cap.bps_at(0), u64::MAX);
        assert_eq!(cap.bps_at(1_000_000), u64::MAX);
    }

    #[test]
    fn cap_timeline_constant() {
        let cap = CapTimeline::constant(1_000_000);
        assert_eq!(cap.bps_at(0), 1_000_000);
        assert_eq!(cap.bps_at(999_999), 1_000_000);
    }

    #[test]
    fn cap_timeline_step() {
        let cap = CapTimeline::step(1_000_000, 100_000, 2_000_000);
        assert_eq!(cap.bps_at(0), 1_000_000);
        assert_eq!(cap.bps_at(99_999), 1_000_000);
        assert_eq!(cap.bps_at(100_000), 2_000_000);
        assert_eq!(cap.bps_at(1_000_000), 2_000_000);
    }
}
