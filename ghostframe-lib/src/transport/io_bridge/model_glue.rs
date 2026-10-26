//! The model controller's side of `IoBridge` (D1 design A): the rate the
//! window must carry, the idle-restart policy, and the persistent-congestion
//! notice.
//!
//! Owns, as logic over `IoBridge` fields that stay in `io_bridge.rs`:
//! - `ModelGlue`, the state that exists only under `GHOSTFRAME_CC=model`;
//! - `model_rate_bytes_per_s`, `R`: the estimator's rate in the bucket's
//!   units, which admission, the window and the classifier all read;
//! - `publish_model_window`, which hands `R_w` and `G` to quinn's controller
//!   through the shared cell (a few atomic stores, O(1));
//! - `model_before_admission`, the §6.6 restart policy;
//! - `model_after_feedback`, which ends a restart and restores a collapsed
//!   window;
//! - `model_probe`, the counters a scene reads.
//!
//! Must not: compute the per-pass admission rate (`pacing.rs` does, from
//! `model_rate_bytes_per_s`), decode feedback, or open or close probe windows
//! (`bwe_glue.rs`; it calls `publish_model_window` when it does).

use super::*;
use crate::transport::instruments::ModelProbe;
use crate::transport::model_controller::{ModelCell, RestartPolicy, Resume};

/// State that exists only when the model controller is installed.
pub(super) struct ModelGlue {
    pub(super) cell: Arc<ModelCell>,
    pub(super) restart: RestartPolicy,
    /// Persistent-congestion events already reported to the log.
    pub(super) persistent_seen: u64,
}

impl ModelGlue {
    pub(super) fn new(cell: Arc<ModelCell>, settings: crate::config::ModelSettings) -> Self {
        Self {
            cell,
            restart: RestartPolicy::new(settings),
            persistent_seen: 0,
        }
    }
}

impl IoBridge {
    /// `R`: what the estimator currently allows, as a rate of the bytes the
    /// bucket bills (bytes per second). An open probe window raises it to the
    /// probe's rate, so a probe is never clipped by the window or the bucket.
    ///
    /// The estimate and the probe target count payload; the bucket and the
    /// window count every byte on the wire, so both are divided by the payload
    /// fraction (`countable_to_token_bps`).
    pub(super) fn model_rate_bytes_per_s(&self) -> f64 {
        let snap = self.bwe.snapshot();
        let countable = match &self.active_probe {
            Some(p) => snap.bitrate_bps.max(p.target_rate_bps),
            None => snap.bitrate_bps,
        };
        pacing::countable_to_token_bps(countable, snap.payload_fraction) as f64 / 8.0
    }

    /// Write `R_w` and `G` to the cell quinn's controller reads. Called when
    /// the estimate moves (after each estimator update), when a probe window
    /// opens or closes, and when a restart begins.
    pub(super) fn publish_model_window(&mut self, fresh_feedback: bool) {
        let rate = self.model_rate_bytes_per_s();
        let Some(m) = self.model.as_mut() else { return };
        m.cell
            .publish(rate as u64, m.restart.gain(), fresh_feedback);
    }

    /// Run before an admission pass that has work to send: classify the gap
    /// since the last send and apply §6.6.
    ///
    /// * Up to `t_hold` of idle: keep the validated rate, send with
    ///   `restart_gain` until the first feedback.
    /// * Past `t_hold`: resume the estimate from `restart_fraction x` the held
    ///   rate (never under the start rate), same gain. goog_cc's delay
    ///   detector resets after 2 s without feedback and is blind for 21 groups,
    ///   which is exactly when the held estimate is most likely stale.
    ///
    /// A pass with nothing to send is not a send, so a gap is measured to the
    /// first pass that has something.
    pub(super) fn model_before_admission(&mut self, now: std::time::Instant) {
        if self.model.is_none() || !self.admission_backlog(now) {
            return;
        }
        self.model_restart_at(now);
    }

    /// `model_before_admission` once there is work: the policy and its effect.
    pub(super) fn model_restart_at(&mut self, now: std::time::Instant) {
        let held_bps = self.bwe.snapshot().bitrate_bps;
        let samples = self.bwe.snapshot().samples_seen;
        let Some(m) = self.model.as_mut() else { return };
        // Before the first feedback there is no validated rate to hold: the
        // startup ladder is the policy there.
        let resume = match m.restart.on_send(now) {
            Resume::No => return,
            _ if samples == 0 => {
                m.restart.on_feedback();
                return;
            }
            r @ (Resume::Hold | Resume::Cold) => r,
        };
        let cold_bps = m
            .restart
            .cold_rate_bps(held_bps, crate::transport::bwe::BweWrapper::INITIAL_BPS);
        match resume {
            Resume::Hold => {
                self.instr.model_restarts_hold += 1;
                tracing::debug!(target: LOG_TARGET, held_bps, "model: restart, holding the rate");
            }
            Resume::Cold => {
                self.instr.model_restarts_cold += 1;
                tracing::debug!(
                    target: LOG_TARGET, held_bps, cold_bps,
                    "model: cold restart, resuming from a fraction of the held rate"
                );
                // A held rate under the cold one is not raised.
                if cold_bps < held_bps {
                    self.bwe.restart_from(now, cold_bps);
                }
            }
            Resume::No => {}
        }
        self.publish_model_window(false);
    }

    /// The estimator heard feedback: a restart ends, a collapsed window is
    /// restored, and the new estimate reaches the window.
    pub(super) fn model_after_feedback(&mut self) {
        let Some(m) = self.model.as_mut() else { return };
        m.restart.on_feedback();
        self.publish_model_window(true);
    }

    /// Report persistent congestion the controller counted since the last
    /// look. The window has already collapsed; this is the log line and the
    /// place a future consumer (the estimator) would be told.
    pub(super) fn model_note_persistent_congestion(&mut self) {
        let Some(m) = self.model.as_mut() else { return };
        let n = m.cell.stats().persistent_congestion_events;
        if n > m.persistent_seen {
            tracing::warn!(
                target: LOG_TARGET,
                events = n - m.persistent_seen,
                "model: persistent congestion, window collapsed to 4 datagrams until feedback"
            );
            m.persistent_seen = n;
        }
    }

    pub(super) fn model_probe(&self) -> ModelProbe {
        match &self.model {
            None => ModelProbe::default(),
            Some(m) => ModelProbe {
                active: true,
                restarts_hold: self.instr.model_restarts_hold,
                restarts_cold: self.instr.model_restarts_cold,
                rate_bytes_per_s: m.cell.rate_bytes_per_s(),
                gain_milli: (m.cell.gain() * 1000.0).round() as u64,
                stats: m.cell.stats(),
            },
        }
    }

    /// The cell quinn's controller reads, for a harness that wants the
    /// controller's counters after the bridge has moved into its task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn model_cell_handle(&self) -> Option<Arc<ModelCell>> {
        self.model.as_ref().map(|m| m.cell.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ModelSettings;
    use crate::transport::instruments::BudgetTerm;
    use std::time::Duration;
    use tokio::net::UnixStream;

    async fn model_bridge() -> IoBridge {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);
        bridge.use_model_controller_for_test(ModelSettings::default());
        bridge
    }

    /// One feedback batch, so the estimator has heard something.
    fn feed(bridge: &mut IoBridge, at: std::time::Instant) {
        let records: Vec<crate::transport::bwe::AckArrival> = (0..12u32)
            .map(|i| crate::transport::bwe::AckArrival {
                server_emit_us: u64::from(i) * 1_000,
                client_arrival_ms_lo16: (i + 15) as u16,
                size_bytes: 1_200,
                probe: None,
            })
            .collect();
        bridge.bwe.update(&records, &[], at, 14_400);
        bridge.model_after_feedback();
    }

    /// The rate is the estimate, whatever quinn's window says. Under Cubic the
    /// same path reading (12 000 bytes over 333 ms, 288 kbit/s) sets the rate;
    /// here it must not. Mutation: drop the model branch of `admission_rate`
    /// and this reads 36 kB/s.
    #[tokio::test(start_paused = true)]
    async fn admission_runs_at_the_estimate_not_at_cwnd_over_srtt() {
        let mut bridge = model_bridge().await;
        // The floor under the rate: one datagram a tick (36 kB/s), well under
        // the estimate, so the floor cannot be what the assertion reads.
        bridge.set_tick_budget_floor_bytes(1_200);
        bridge.apply_path_stats_snapshot(12_000, 333_000.0);
        let snap = bridge.bwe.snapshot();
        assert_eq!(snap.bitrate_bps, crate::transport::bwe::BweWrapper::INITIAL_BPS);
        let rate = bridge.admission_rate();
        let expect = snap.bitrate_bps as f64 / 8.0 / snap.payload_fraction;
        assert!(
            (rate.bytes_per_sec - expect).abs() < 1.0,
            "rate {} B/s, estimate {} B/s",
            rate.bytes_per_sec,
            expect
        );
        assert_eq!(rate.term, BudgetTerm::Pacer);
        // The classifier reads the same figure, not cwnd / srtt.
        let bpu = bridge.adaptation_context.bytes_per_us as f64;
        assert!((bpu * 1e6 - expect).abs() < 1.0, "classifier {bpu} B/us");
        // Put the cwnd term back where the Cubic path reads it: the rate must
        // not follow it.
        bridge.adaptation_context.bytes_per_us = 12_000.0 / 333_000.0;
        let again = bridge.admission_rate().bytes_per_sec;
        assert!(
            (again - expect).abs() < 1.0,
            "rate {again} B/s followed the cwnd term, not the estimate {expect} B/s"
        );
        // And no hint was handed to the estimator.
        assert_eq!(bridge.instr.hint_feeds + bridge.instr.hint_unbounded, 0);
    }

    /// An open probe window raises `R_w` to the probe's rate, so quinn's window
    /// cannot clip the cluster; closing it returns the window to the estimate.
    #[tokio::test(start_paused = true)]
    async fn the_window_carries_a_probe_while_one_is_open() {
        let mut bridge = model_bridge().await;
        let t0 = now_std();
        feed(&mut bridge, t0 + Duration::from_millis(20));
        let cell = bridge.model_cell_handle().expect("model installed");
        let est = bridge.model_rate_bytes_per_s() as u64;
        assert_eq!(cell.rate_bytes_per_s(), est, "published after feedback");

        let probe_bps = 60_000_000u64;
        bridge.active_probe = Some(ActiveProbe {
            id: 7,
            target_rate_bps: probe_bps,
            ends_at: t0 + Duration::from_secs(1),
            min_probes: 1,
            min_bytes: 1,
            bytes_sent: 0,
            packets_sent: 0,
        });
        bridge.publish_model_window(false);
        let f = bridge.bwe.snapshot().payload_fraction;
        assert_eq!(
            cell.rate_bytes_per_s(),
            (probe_bps as f64 / f / 8.0) as u64,
            "R_w is the probe's rate while the window is open"
        );
        let probe = bridge.active_probe.take().expect("open");
        bridge.close_probe_window(probe);
        assert_eq!(cell.rate_bytes_per_s(), bridge.model_rate_bytes_per_s() as u64);
        assert!(cell.rate_bytes_per_s() < 1_000_000, "back to the estimate");
    }

    /// Section 6.6: an idle within `t_hold` keeps the rate and drops the gain to
    /// `restart_gain` until feedback; an idle past it resumes from a quarter of
    /// the held rate. Mutation: skip the reseed in the cold arm and the held
    /// 40 Mbit/s stays.
    #[tokio::test(start_paused = true)]
    async fn an_idle_restarts_at_the_held_rate_then_at_a_fraction_of_it() {
        let mut bridge = model_bridge().await;
        let t0 = now_std();
        feed(&mut bridge, t0 + Duration::from_millis(20));
        // A validated 40 Mbit/s: the estimator continues from it.
        bridge.bwe.restart_from(t0 + Duration::from_millis(30), 40_000_000);
        let held = bridge.bwe.snapshot().bitrate_bps;
        assert!(held > 30_000_000, "premise: a high held rate ({held})");
        let cell = bridge.model_cell_handle().expect("model");
        let sent = |b: &mut IoBridge, at| b.model_restart_at(at);

        // Steady sending: no restart.
        sent(&mut bridge, t0 + Duration::from_millis(40));
        sent(&mut bridge, t0 + Duration::from_millis(73));
        assert_eq!(bridge.model_probe().restarts_hold + bridge.model_probe().restarts_cold, 0);
        assert_eq!(cell.gain(), 2.0);

        // A 1 s idle: the rate is held, the gain drops.
        sent(&mut bridge, t0 + Duration::from_millis(1_073));
        let p = bridge.model_probe();
        assert_eq!((p.restarts_hold, p.restarts_cold), (1, 0));
        assert_eq!(bridge.bwe.snapshot().bitrate_bps, held, "held through a short idle");
        assert_eq!(cell.gain(), 1.0, "restart gain until feedback");
        feed(&mut bridge, t0 + Duration::from_millis(1_110));
        assert_eq!(cell.gain(), 2.0, "feedback ends the restart");

        // A 3 s idle: past t_hold, resume from a quarter.
        sent(&mut bridge, t0 + Duration::from_millis(4_200));
        let p = bridge.model_probe();
        assert_eq!((p.restarts_hold, p.restarts_cold), (1, 1));
        let resumed = bridge.bwe.snapshot().bitrate_bps;
        assert!(
            (resumed as f64) < 0.3 * held as f64 && resumed >= 2_000_000,
            "cold restart resumed at {resumed} from {held}"
        );
        assert_eq!(cell.gain(), 1.0);
    }

    /// Before the estimator has heard any feedback there is no validated rate
    /// to hold: an idle there is the startup, not a restart.
    #[tokio::test(start_paused = true)]
    async fn no_restart_before_the_first_feedback() {
        let mut bridge = model_bridge().await;
        let t0 = now_std();
        bridge.model_restart_at(t0);
        bridge.model_restart_at(t0 + Duration::from_secs(5));
        let p = bridge.model_probe();
        assert_eq!((p.restarts_hold, p.restarts_cold), (0, 0));
        assert_eq!(bridge.model_cell_handle().unwrap().gain(), 2.0);
    }
}
