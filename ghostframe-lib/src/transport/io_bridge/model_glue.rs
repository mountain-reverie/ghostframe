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
