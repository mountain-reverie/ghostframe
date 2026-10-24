//! Transport measurements for the `cumulative emit` log line and the
//! browserless harness. MEASUREMENT ONLY: nothing here feeds a decision, the
//! bandwidth estimator or the classifier.
//!
//! It answers the questions the retransmission cascade left open: how deep
//! quinn's datagram send queue stands, whether the congestion window has been
//! cut, and what the client says it received, lost and rebuilt from parity.
//! Everything is read on the loop side (`sample_all_path_stats` already holds
//! the live connection); the frame worker never touches it.

use super::{IoBridge, WireClass};
use crate::transport::instruments::{
    AdmissionCounters, BudgetBinding, DepartureMirror, EcnObservation, FifoSeries,
    InstrumentSummary, OfferedBytes,
};
use quinn_proto::ConnectionHandle;

/// Cumulative and latest transport readings for the live session.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TransportProbe {
    /// quinn's congestion window, bytes (latest).
    pub cwnd: u64,
    /// Packets quinn declared lost on the path (cumulative).
    pub lost_packets: u64,
    /// Packets quinn sent on the path (cumulative).
    pub sent_packets: u64,
    /// Congestion events (window cuts) on the path (cumulative).
    pub congestion_events: u64,
    /// Bytes queued in quinn's datagram send buffer at the last reading:
    /// `capacity - send_buffer_space()`. Read once per loop turn AFTER the
    /// outbound drain, so it is what was left queued, not what was queued
    /// at the moment of the burst.
    pub queue_bytes: usize,
    /// Highest `queue_bytes` seen: the largest post-drain reading, not the
    /// true peak.
    pub max_queue_bytes: usize,
    /// Number of readings taken; zero means nothing below was ever measured.
    pub samples: u64,
    /// Sums of the client's `ReceiverFeedback` (cumulative). `client_recovered`
    /// is the datagrams it rebuilt from FEC parity.
    pub client_received: u64,
    pub client_recovered: u64,
    /// The D1 instruments (see [`crate::transport::instruments`]). Filled when
    /// the probe is published to the harness; zero in the live copy.
    pub instr: InstrumentSummary,
}

/// A datagram quinn just accepted, as `note_enqueued` is told of it.
#[derive(Debug, Clone, Copy)]
pub(super) struct Enqueued {
    pub(super) class: WireClass,
    /// It passed the admission gate (and was charged to the bucket before
    /// it got here) rather than bypassing it.
    pub(super) gated: bool,
    /// The datagram as handed to the WebTransport layer.
    pub(super) dg_len: usize,
    /// What that layer added.
    pub(super) framing_len: usize,
    /// The session's cumulative accepted bytes including this one.
    pub(super) cum_enqueued: u64,
    /// quinn's queued bytes after it.
    pub(super) queued: u64,
}

/// The mutable state behind [`InstrumentSummary`]. Measurement only: nothing
/// in the bridge reads any field of this to decide anything.
///
/// Scope. The offered-load accounting, the FIFO series, the departure mirror
/// and the quinn byte counters describe the LIVE session only: they are
/// reset when a different connection becomes live (`apply_hello`) and when a
/// session is forgotten, and only sends to the live session are billed.
/// The budget, ECN, and ledger counters belong to the bridge and run for its
/// whole life (the ledger is not per session).
#[derive(Debug, Default)]
pub(super) struct Instruments {
    pub(super) offered: OfferedBytes,
    pub(super) mirror: DepartureMirror,
    /// The session the mirror describes; a new session starts it afresh.
    mirror_handle: Option<ConnectionHandle>,
    pub(super) fifo: FifoSeries,
    /// Largest FIFO depth seen right after a datagram was accepted.
    pub(super) fifo_peak_at_enqueue: u64,
    pub(super) budget: BudgetBinding,
    pub(super) admission: AdmissionCounters,
    pub(super) ecn: EcnObservation,
    /// Session counter and queue depth at the last post-drain reading.
    quinn_bytes_enqueued: u64,
    quinn_bytes_queued: u64,
    /// The live session's accepted-byte counter when it became live, so the
    /// counters compared with `offered` cover the same sends.
    session_bytes_base: u64,
    /// Ledger expiries (declared lost) and how many were handed to goog_cc.
    pub(super) losses_declared: u64,
    pub(super) losses_flushed: u64,
    /// Declared lost but dropped from the pending list by its cap.
    pub(super) losses_overflowed: u64,
    pub(super) flushed: crate::transport::instruments::FlushedLosses,
    /// Late acknowledgements of losses goog_cc had already been told about.
    pub(super) late_acked_after_flush: u64,
}

impl Instruments {
    pub(super) fn mirror_handle(&self) -> Option<ConnectionHandle> {
        self.mirror_handle
    }

    /// Start the session-scoped instruments afresh. `bytes_base` is the new
    /// live session's accepted-byte counter at this moment.
    pub(super) fn reset_session_scope(&mut self, bytes_base: u64) {
        self.offered = OfferedBytes::default();
        self.mirror = DepartureMirror::default();
        self.mirror_handle = None;
        self.fifo = FifoSeries::default();
        self.fifo_peak_at_enqueue = 0;
        self.quinn_bytes_enqueued = 0;
        self.quinn_bytes_queued = 0;
        self.session_bytes_base = bytes_base;
    }
}

impl IoBridge {
    /// A different connection just became the live session.
    pub(super) fn reset_session_instruments(&mut self, handle: ConnectionHandle) {
        let base = self
            .wt_sessions
            .get(&handle)
            .map_or(0, |wt| wt.datagram_bytes_enqueued());
        self.instr.reset_session_scope(base);
    }

    /// Read the live session's quinn stats and send-queue depth.
    pub(super) fn update_transport_probe(&mut self) {
        let capacity = self.server.datagram_send_buffer();
        let Some(handle) = self.last_hello_from else {
            return;
        };
        let Some(conn) = self.server.connections.get_mut(&handle) else {
            return;
        };
        let stats = conn.stats();
        let space = conn.datagrams().send_buffer_space();
        let queued = capacity.saturating_sub(space) as u64;
        let enqueued = self
            .wt_sessions
            .get(&handle)
            .map_or(0, |wt| wt.datagram_bytes_enqueued());
        let now = super::now_std();
        self.instr.fifo.record(now, queued);
        self.instr.quinn_bytes_enqueued = enqueued.saturating_sub(self.instr.session_bytes_base);
        self.instr.quinn_bytes_queued = queued;
        let p = &mut self.transport_probe;
        p.cwnd = stats.path.cwnd;
        p.lost_packets = stats.path.lost_packets;
        p.sent_packets = stats.path.sent_packets;
        p.congestion_events = stats.path.congestion_events;
        p.queue_bytes = capacity.saturating_sub(space);
        p.max_queue_bytes = p.max_queue_bytes.max(p.queue_bytes);
        p.samples += 1;
    }

    /// Note a datagram quinn just accepted: bill its class and wire bytes,
    /// put it in the departure mirror and track the FIFO's peak depth.
    pub(super) fn note_enqueued(&mut self, handle: ConnectionHandle, e: Enqueued) {
        let Enqueued {
            class,
            gated,
            dg_len,
            framing_len,
            cum_enqueued,
            queued,
        } = e;
        let wire = dg_len + framing_len;
        if gated {
            // Read off quinn's own queue, not the gate's bookkeeping: a
            // datagram accepted while the FIFO already stood at `D_target`
            // means something bypassed the gate.
            if let Some(g) = self.gate {
                if queued.saturating_sub(wire as u64) >= g.d_target as u64 {
                    self.instr.admission.depth_violations += 1;
                }
            }
            self.instr.admission.datagrams_admitted += 1;
        } else {
            // H.264 frames and control datagrams have no queue to hold them
            // back, so they are billed to the bucket after the fact and the
            // classes that can wait pay for them.
            self.admission.charge(wire);
            self.instr.admission.soft_charged_bytes += wire as u64;
            if let Some(g) = self.gate {
                if queued.saturating_sub(wire as u64) >= g.d_target as u64 {
                    self.instr.admission.ungated_at_depth += 1;
                }
            }
        }
        let i = &mut self.instr;
        i.offered.bill(class, dg_len, framing_len);
        i.fifo_peak_at_enqueue = i.fifo_peak_at_enqueue.max(queued);
        if i.mirror_handle != Some(handle) {
            i.mirror.clear();
            i.mirror_handle = Some(handle);
        }
        i.mirror.on_enqueue(cum_enqueued, super::now_std());
    }

    /// After a `poll_transmit` batch, stamp the datagrams that left quinn's
    /// FIFO. Does nothing while the mirror is empty.
    pub(super) fn poll_departures(&mut self, now: std::time::Instant) {
        let Some(handle) = self.instr.mirror_handle else {
            return;
        };
        if self.instr.mirror.is_empty() {
            return;
        }
        let capacity = self.server.datagram_send_buffer();
        let (Some(conn), Some(wt)) = (
            self.server.connections.get_mut(&handle),
            self.wt_sessions.get(&handle),
        ) else {
            return;
        };
        let queued = capacity.saturating_sub(conn.datagrams().send_buffer_space()) as u64;
        self.instr
            .mirror
            .on_poll(wt.datagram_bytes_enqueued(), queued, now);
    }

    /// Snapshot every instrument.
    pub(super) fn instrument_summary(&self) -> InstrumentSummary {
        let i = &self.instr;
        // Mirror the session counter against what was still queued at the
        // last reading; both are as of the last `update_transport_probe`.
        let lag = &i.mirror.lag;
        let late_total = self.transmission_ledger.stats().acked_after_declared_lost;
        InstrumentSummary {
            window_us: i.fifo.window_us(),
            fifo_samples: i.fifo.samples(),
            fifo_depth_max: i.fifo.max(),
            fifo_depth_peak_at_enqueue: i.fifo_peak_at_enqueue,
            fifo_depth_mean: i.fifo.time_weighted_mean().round() as u64,
            fifo_depth_slope_last_quarter_bps: i.fifo.slope_last_quarter().round() as i64,
            quinn_limited_turns: i.fifo.nonempty(),
            quinn_limited_us: i.fifo.nonempty_us(),
            ledger_unknown_acks: self.transmission_ledger.stats().unknown_acks,
            ledger_tombstone_evictions: self.transmission_ledger.stats().tombstone_evictions,
            offered: i.offered,
            quinn_bytes_enqueued: i.quinn_bytes_enqueued,
            quinn_bytes_departed: i.quinn_bytes_enqueued.saturating_sub(i.quinn_bytes_queued),
            lag_samples: lag.count(),
            lag_p50_us: lag.quantile_us(0.5),
            lag_p95_us: lag.quantile_us(0.95),
            lag_max_us: lag.max_us(),
            mirror_len: i.mirror.len() as u64,
            mirror_overflowed: i.mirror.overflowed,
            budget: i.budget,
            admission: i.admission,
            ledger_losses_declared: i.losses_declared,
            ledger_losses_flushed: i.losses_flushed,
            ledger_losses_overflowed: i.losses_overflowed,
            late_acked_before_flush: self.pending_losses_retracted,
            late_acked_after_flush: i.late_acked_after_flush,
            late_acked_unreported: late_total
                .saturating_sub(self.pending_losses_retracted)
                .saturating_sub(i.late_acked_after_flush),
            ecn: i.ecn,
        }
    }

    /// Add one `ReceiverFeedback` to the client-side totals.
    pub(super) fn note_client_feedback(
        &mut self,
        fb: &crate::transport::feedback::ReceiverFeedback,
    ) {
        let p = &mut self.transport_probe;
        p.client_received += u64::from(fb.datagrams_received);
        p.client_recovered += u64::from(fb.datagrams_recovered_fec);
    }
}
