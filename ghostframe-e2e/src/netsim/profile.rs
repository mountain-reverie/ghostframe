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
    ///
    /// On its own this is a token bucket that **drops** whatever it cannot
    /// afford. That models a lossy link, not a congested one. Set
    /// [`bottleneck`](Self::bottleneck) to model congestion properly.
    pub cap: CapTimeline,

    /// Bottleneck buffer in front of [`cap`](Self::cap), if any.
    ///
    /// `None` keeps the historical token-bucket behaviour: excess traffic is
    /// dropped outright and one-way delay never changes. `Some` makes the
    /// link queue first and drop only when the buffer is full, which is what
    /// a real congested path does — and is the difference between exercising
    /// a congestion controller and not.
    pub bottleneck: Option<Bottleneck>,
}

/// A bottleneck buffer: excess traffic waits instead of vanishing.
///
/// A real congested link has a fixed serialisation rate and a buffer in
/// front of it. Traffic arriving faster than the link drains queues, and
/// queuing delay grows as the buffer fills; only a *full* buffer drops
/// (tail drop). Loss is the tail event, not the primary signal — on real
/// paths you get tens to hundreds of milliseconds of delay growth as
/// advance warning before a single packet is lost.
///
/// That ordering is why goog_cc, BBR and every other modern controller
/// estimates from delay first. Against a bottleneck that drops without ever
/// queueing, the delay-based half of the controller is inert and only the
/// loss-based fallback runs — which, measured on this harness, pins the
/// estimate to its `MIN_BPS` floor regardless of the actual link rate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bottleneck {
    /// Buffer depth as milliseconds of drain time at the current link rate.
    ///
    /// Expressed in time rather than bytes because that is how buffering is
    /// actually discussed ("300 ms of bufferbloat"), and because it then
    /// tracks a [`CapTimeline`] step automatically instead of silently
    /// becoming a different buffer when the rate changes.
    pub depth_ms: u64,

    /// Active queue management. `None` is a plain tail-drop FIFO.
    pub aqm: Option<CoDel>,
}

/// CoDel (RFC 8289), simplified: drop from the head of a standing queue so
/// delay stays near `target_us`, rather than letting the buffer fill.
///
/// Modern CPE (fq_codel, cake) runs this, which is why a well-provisioned
/// fibre or ethernet path shows low latency under load while consumer WiFi
/// and LTE — typically no AQM, deep buffers — show hundreds of milliseconds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoDel {
    /// Standing-queue delay to hold. RFC 8289 default is 5 ms.
    pub target_us: u64,
    /// How long delay must stay above target before dropping starts, and the
    /// base for the drop-rate schedule. RFC 8289 default is 100 ms.
    pub interval_us: u64,
}

impl CoDel {
    /// RFC 8289 defaults: 5 ms target, 100 ms interval.
    pub fn rfc_defaults() -> Self {
        Self {
            target_us: 5_000,
            interval_us: 100_000,
        }
    }
}

impl Bottleneck {
    /// Fibre or ethernet behind CPE running fq_codel/cake: a shallow buffer
    /// with AQM holding the standing queue near 5 ms.
    pub fn fibre_aqm() -> Self {
        Self {
            depth_ms: 50,
            aqm: Some(CoDel::rfc_defaults()),
        }
    }

    /// Consumer WiFi: a moderate buffer, no AQM. Queuing delay is real and
    /// visible but bounded; L2 retries add jitter this does not model.
    pub fn wifi() -> Self {
        Self {
            depth_ms: 200,
            aqm: None,
        }
    }

    /// LTE/5G: a deep buffer, no AQM. The classic bufferbloat case, where
    /// delay climbs into the hundreds of milliseconds and loss barely
    /// appears at all.
    pub fn lte_bufferbloat() -> Self {
        Self {
            depth_ms: 600,
            aqm: None,
        }
    }
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
            bottleneck: None,
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
