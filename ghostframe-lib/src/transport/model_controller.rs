//! quinn's congestion controller under D1 design A: a window derived from the
//! bandwidth estimate, not a controller of its own.
//!
//! goog_cc owns the send rate. The admission bucket paces to it
//! (`io_bridge::pacing`); this controller only bounds what may be in flight:
//!
//! ```text
//! W = max(4 x MTU, ceil(R_w x (min_rtt + max_ack_delay) x G))
//! ```
//!
//! `R_w` is the rate the window must be able to carry: the estimate, or a
//! probe cluster's rate while one is open. A probe more than about twice the
//! link also builds a queue that outgrows `G x R_w x RTT_eff` before its
//! window closes (the round trip it is held against is the minimum, not the
//! one the probe causes), so while a cluster is open the window also carries
//! the whole cluster (`probe_bytes`): a probe is never clipped by the window.
//! `G` (2 by default) is slack for the ack clock and for the
//! estimate rising between two updates; it is also what bounds the bottleneck
//! queue a wrong estimate can build, to about `(G x R_w / C - 1) x RTT_eff`.
//!
//! What the controller does not do: grow or shrink on acknowledgements (quinn's
//! `app_limited` flag stops mattering), or react to ordinary loss, which
//! goog_cc already learns from the transmission ledger and must not be told
//! twice. The one loss signal it acts on is persistent congestion, the
//! backstop for lost feedback: when every packet of a PTO-scaled span is lost
//! the estimator may be blind (it learns nothing from acknowledgements that
//! do not arrive), so the window drops to `4 x MTU` until the estimator next
//! hears feedback ([`ModelCell::publish`] with `fresh_feedback`).
//!
//! The estimate reaches the controller through a [`ModelCell`], a bundle of
//! atomics shared with `IoBridge`. quinn builds one controller per connection;
//! they all read the one cell, which is right for the single live session this
//! server serves.

use crate::config::ModelSettings;
use crate::transport::rtt_estimator::MAX_ACK_DELAY;
use quinn_proto::congestion::{Controller, ControllerFactory};
use quinn_proto::RttEstimator;
use std::any::Any;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Datagrams the window never falls below: enough for a handshake flight and
/// for the feedback that restores the estimate after a collapse.
const MIN_WINDOW_MTUS: u64 = 4;

/// The window before the connection has a round-trip sample, in datagrams.
/// quinn's own default is 12 (14 720 bytes); the hybrid post-quantum handshake
/// flight is several datagrams, and a smaller window would cost it round trips.
/// The design sizes this from the start rate and the handshake round trip, but
/// `Controller::initial_window` is asked before there is one.
const INITIAL_WINDOW_MTUS: u64 = 10;

/// The window for rate `rate_bytes_per_s` over an effective round trip
/// `rtt_eff`, with gain `gain` (to the nearest thousandth, never under 1),
/// never under `4 x mtu`. Pure; the controller and the scenes' bounds use the
/// same arithmetic.
pub fn model_window(rate_bytes_per_s: u64, rtt_eff: Duration, gain: f64, mtu: u64) -> u64 {
    // Integer arithmetic, rounded up: a float product of 0.035 s drifts a
    // byte either side of the exact figure.
    let rtt_us = rtt_eff.as_micros();
    let bdp = (u128::from(rate_bytes_per_s) * rtt_us * u128::from(gain_to_milli(gain)))
        .div_ceil(1_000_000_000);
    u64::try_from(bdp)
        .unwrap_or(u64::MAX)
        .max(MIN_WINDOW_MTUS * mtu)
}

/// What the controller counted, for scenes and the periodic log.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ModelStats {
    /// `is_persistent_congestion` events seen.
    pub persistent_congestion_events: u64,
    /// Largest window returned, bytes.
    pub window_max: u64,
}

/// The estimator's side of the window, shared with every controller.
#[derive(Debug)]
pub struct ModelCell {
    /// `R_w`, bytes per second of wire bytes (token rate).
    rate_bytes_per_s: AtomicU64,
    /// `G`, thousandths.
    gain_milli: AtomicU32,
    /// Bytes of the open probe cluster, wire bytes; zero with no cluster open.
    probe_bytes: AtomicU64,
    /// Bumped by every publish that follows estimator feedback. A collapse
    /// lasts until it moves.
    feedback_epoch: AtomicU64,
    persistent: AtomicU64,
    window_max: AtomicU64,
}

impl ModelCell {
    pub fn new(rate_bytes_per_s: u64, gain: f64) -> Arc<Self> {
        Arc::new(Self {
            rate_bytes_per_s: AtomicU64::new(rate_bytes_per_s),
            gain_milli: AtomicU32::new(gain_to_milli(gain)),
            probe_bytes: AtomicU64::new(0),
            feedback_epoch: AtomicU64::new(0),
            persistent: AtomicU64::new(0),
            window_max: AtomicU64::new(0),
        })
    }

    /// Write `R_w` and `G`. `fresh_feedback` is true when the estimator has
    /// just heard feedback (a persistent-congestion collapse ends there).
    pub fn publish(
        &self,
        rate_bytes_per_s: u64,
        gain: f64,
        probe_bytes: u64,
        fresh_feedback: bool,
    ) {
        self.probe_bytes.store(probe_bytes, Ordering::Relaxed);
        self.rate_bytes_per_s
            .store(rate_bytes_per_s, Ordering::Relaxed);
        self.gain_milli
            .store(gain_to_milli(gain), Ordering::Relaxed);
        if fresh_feedback {
            self.feedback_epoch.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn rate_bytes_per_s(&self) -> u64 {
        self.rate_bytes_per_s.load(Ordering::Relaxed)
    }

    pub fn probe_bytes(&self) -> u64 {
        self.probe_bytes.load(Ordering::Relaxed)
    }

    pub fn gain(&self) -> f64 {
        f64::from(self.gain_milli.load(Ordering::Relaxed)) / 1000.0
    }

    fn epoch(&self) -> u64 {
        self.feedback_epoch.load(Ordering::Relaxed)
    }

    pub fn stats(&self) -> ModelStats {
        ModelStats {
            persistent_congestion_events: self.persistent.load(Ordering::Relaxed),
            window_max: self.window_max.load(Ordering::Relaxed),
        }
    }
}

fn gain_to_milli(gain: f64) -> u32 {
    (gain * 1000.0).round().clamp(1000.0, f64::from(u32::MAX)) as u32
}

/// Builds a [`ModelController`] per connection, all on one [`ModelCell`].
#[derive(Debug, Clone)]
pub struct ModelControllerFactory {
    cell: Arc<ModelCell>,
}

impl ModelControllerFactory {
    pub fn new(cell: Arc<ModelCell>) -> Self {
        Self { cell }
    }
}

impl ControllerFactory for ModelControllerFactory {
    fn build(self: Arc<Self>, _now: std::time::Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(ModelController {
            cell: self.cell.clone(),
            mtu: u64::from(current_mtu),
            min_rtt: None,
            collapsed_at: None,
        })
    }
}

#[derive(Clone)]
pub struct ModelController {
    cell: Arc<ModelCell>,
    mtu: u64,
    /// quinn's minimum round trip, once it has a sample.
    min_rtt: Option<Duration>,
    /// The feedback epoch a persistent-congestion event collapsed the window
    /// in. The window stays at the floor while the cell is still in it.
    collapsed_at: Option<u64>,
}

impl ModelController {
    fn current_window(&self) -> u64 {
        if self.collapsed_at == Some(self.cell.epoch()) {
            return MIN_WINDOW_MTUS * self.mtu;
        }
        let Some(min_rtt) = self.min_rtt else {
            return self.initial_window();
        };
        model_window(
            self.cell.rate_bytes_per_s(),
            min_rtt + MAX_ACK_DELAY,
            self.cell.gain(),
            self.mtu,
        )
        .saturating_add(self.cell.probe_bytes())
    }
}

impl Controller for ModelController {
    fn on_ack(
        &mut self,
        _now: std::time::Instant,
        _sent: std::time::Instant,
        _bytes: u64,
        _app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.min_rtt = Some(rtt.min());
    }

    fn on_congestion_event(
        &mut self,
        _now: std::time::Instant,
        _sent: std::time::Instant,
        is_persistent_congestion: bool,
        _lost_bytes: u64,
    ) {
        if is_persistent_congestion {
            self.collapsed_at = Some(self.cell.epoch());
            self.cell.persistent.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.mtu = u64::from(new_mtu);
    }

    fn window(&self) -> u64 {
        let w = self.current_window();
        self.cell.window_max.fetch_max(w, Ordering::Relaxed);
        w
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        INITIAL_WINDOW_MTUS * self.mtu
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// The idle-restart policy of design section 6.6, as a pure state machine.
/// `IoBridge` feeds it the instant of every send and every estimator
/// feedback and applies what it says.
#[derive(Debug, Clone)]
pub(crate) struct RestartPolicy {
    settings: ModelSettings,
    last_send: Option<Instant>,
    /// A restart is open: the gain stays `restart_gain` until feedback.
    restarting: bool,
}

/// What a send after a gap asks of the glue.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Resume {
    /// Not a restart.
    No,
    /// An idle of at most `t_hold`: keep the estimate, send with the
    /// restart gain until the first feedback.
    Hold,
    /// An idle past `t_hold`: resume the estimate from `restart_fraction x`
    /// the held rate (never under the start rate), with the restart gain.
    Cold,
}

impl RestartPolicy {
    pub(crate) fn new(settings: ModelSettings) -> Self {
        Self {
            settings,
            last_send: None,
            restarting: false,
        }
    }

    /// A send is about to happen at `now`. Classifies the gap since the last
    /// one; the first send of a session is no restart.
    pub(crate) fn on_send(&mut self, now: Instant) -> Resume {
        let gap = self
            .last_send
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or_default();
        self.last_send = Some(now);
        if gap < self.settings.idle_after {
            return Resume::No;
        }
        self.restarting = true;
        if gap > self.settings.t_hold {
            Resume::Cold
        } else {
            Resume::Hold
        }
    }

    /// The estimator heard feedback: the restart is over.
    pub(crate) fn on_feedback(&mut self) {
        self.restarting = false;
    }

    /// The gain to publish now.
    pub(crate) fn gain(&self) -> f64 {
        if self.restarting {
            self.settings.restart_gain
        } else {
            self.settings.gain
        }
    }

    #[cfg(test)]
    pub(crate) fn restarting(&self) -> bool {
        self.restarting
    }

    /// The rate a cold restart resumes at, for a held rate `held_bps` and the
    /// session's start rate `start_bps`.
    pub(crate) fn cold_rate_bps(&self, held_bps: u64, start_bps: u64) -> u64 {
        (((held_bps as f64) * self.settings.restart_fraction) as u64).max(start_bps)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MTU: u64 = 1200;

    fn controller(cell: &Arc<ModelCell>) -> ModelController {
        ModelController {
            cell: cell.clone(),
            mtu: MTU,
            min_rtt: Some(Duration::from_millis(30)),
            collapsed_at: None,
        }
    }

    #[test]
    fn the_window_is_the_gain_times_rate_times_effective_rtt() {
        // 2 Mbit/s of wire bytes, 30 ms + 5 ms, G = 2: 250 kB/s x 35 ms x 2.
        let w = model_window(250_000, Duration::from_millis(35), 2.0, MTU);
        assert_eq!(w, 17_500);
        // The controller reads the same cell: 10x the rate, 10x the window.
        let cell = ModelCell::new(250_000, 2.0);
        let c = controller(&cell);
        assert_eq!(c.window(), 17_500);
        cell.publish(2_500_000, 2.0, 0, true);
        assert_eq!(c.window(), 175_000);
        // And the gain: the same rate at G = 1 is half.
        cell.publish(2_500_000, 1.0, 0, true);
        assert_eq!(c.window(), 87_500);
    }

    #[test]
    fn an_open_probe_adds_its_whole_cluster_to_the_window() {
        let cell = ModelCell::new(250_000, 2.0);
        let c = controller(&cell);
        assert_eq!(c.window(), 17_500);
        // A 36 Mbit/s cluster for 100 ms is 450 kB; the rate is the probe's.
        cell.publish(4_500_000, 2.0, 450_000, false);
        assert_eq!(c.window(), 315_000 + 450_000);
        // Closed, the cluster leaves the window.
        cell.publish(250_000, 2.0, 0, false);
        assert_eq!(c.window(), 17_500);
    }

    #[test]
    fn the_window_never_falls_below_four_datagrams() {
        let cell = ModelCell::new(1, 2.0);
        assert_eq!(controller(&cell).window(), 4 * MTU);
        cell.publish(0, 2.0, 0, false);
        assert_eq!(controller(&cell).window(), 4 * MTU);
        // A rate that would overflow saturates rather than wrapping.
        assert_eq!(
            model_window(u64::MAX, Duration::from_secs(1000), 64.0, MTU),
            u64::MAX
        );
    }

    #[test]
    fn before_a_round_trip_sample_the_window_is_the_initial_one() {
        let cell = ModelCell::new(25_000_000, 2.0);
        let mut c = controller(&cell);
        c.min_rtt = None;
        assert_eq!(
            c.window(),
            10 * MTU,
            "a fast start rate must not open the window"
        );
        assert_eq!(c.initial_window(), 10 * MTU);
    }

    /// Persistent congestion collapses the window; only a publish that follows
    /// feedback restores it. Mutation: ignore persistent congestion in
    /// `on_congestion_event` and the first assertion fails.
    #[test]
    fn persistent_congestion_collapses_the_window_until_the_next_feedback() {
        let cell = ModelCell::new(2_500_000, 2.0);
        let mut c = controller(&cell);
        let now = std::time::Instant::now();
        let before = c.window();
        assert!(before > 4 * MTU, "premise: the window is above the floor");

        // Ordinary loss does not move it: goog_cc has that signal already.
        c.on_congestion_event(now, now, false, 3 * MTU);
        assert_eq!(c.window(), before, "ordinary loss is not counted twice");
        assert_eq!(cell.stats().persistent_congestion_events, 0);

        c.on_congestion_event(now, now, true, MTU);
        assert_eq!(c.window(), 4 * MTU, "persistent congestion collapses it");
        assert_eq!(cell.stats().persistent_congestion_events, 1);

        // A new rate without feedback behind it does not end the collapse
        // (a probe opening, an idle restart) ...
        cell.publish(5_000_000, 2.0, 0, false);
        assert_eq!(c.window(), 4 * MTU);
        // ... the estimator hearing feedback does.
        cell.publish(2_000_000, 2.0, 0, true);
        assert!(c.window() > 4 * MTU, "feedback restores the window");
        assert_eq!(
            c.window(),
            2 * 35 * 2_000_000 / 1000,
            "to the estimate's window"
        );

        // A second collapse, later, is a new one.
        c.on_congestion_event(now, now, true, MTU);
        assert_eq!(c.window(), 4 * MTU);
        assert_eq!(cell.stats().persistent_congestion_events, 2);
    }

    #[test]
    fn a_collapse_on_one_connection_does_not_outlive_feedback_on_another() {
        // Controllers are per connection; the epoch is the cell's.
        let cell = ModelCell::new(2_500_000, 2.0);
        let mut a = controller(&cell);
        let b = controller(&cell);
        let now = std::time::Instant::now();
        a.on_congestion_event(now, now, true, 0);
        assert_eq!(a.window(), 4 * MTU);
        assert!(b.window() > 4 * MTU, "the other connection is untouched");
    }

    fn policy() -> (RestartPolicy, Instant) {
        (RestartPolicy::new(ModelSettings::default()), Instant::now())
    }

    #[test]
    fn the_restart_policy_classifies_the_gap_before_a_send() {
        let (mut p, t0) = policy();
        let ms = Duration::from_millis;
        assert_eq!(p.on_send(t0), Resume::No, "the first send is no restart");
        assert_eq!(p.on_send(t0 + ms(33)), Resume::No, "a frame interval");
        assert_eq!(p.gain(), 2.0);
        // 1 s of silence: held, restart gain until feedback.
        assert_eq!(p.on_send(t0 + ms(1033)), Resume::Hold);
        assert!(p.restarting());
        assert_eq!(p.gain(), 1.0);
        p.on_feedback();
        assert_eq!(p.gain(), 2.0);
        // Exactly t_hold is still held; past it is cold.
        assert_eq!(p.on_send(t0 + ms(3033)), Resume::Hold);
        p.on_feedback();
        assert_eq!(p.on_send(t0 + ms(3033 + 2001)), Resume::Cold);
        assert_eq!(p.gain(), 1.0);
        // A second send before feedback is not a new restart, but the gain stays.
        assert_eq!(p.on_send(t0 + ms(3033 + 2001 + 10)), Resume::No);
        assert_eq!(p.gain(), 1.0, "the restart gain lasts until feedback");
    }

    #[test]
    fn a_cold_restart_resumes_at_a_quarter_of_the_held_rate_never_under_the_start() {
        let (p, _) = policy();
        assert_eq!(p.cold_rate_bps(20_000_000, 2_000_000), 5_000_000);
        assert_eq!(
            p.cold_rate_bps(4_000_000, 2_000_000),
            2_000_000,
            "floor: the start rate"
        );
        assert_eq!(p.cold_rate_bps(0, 2_000_000), 2_000_000);
    }
}
