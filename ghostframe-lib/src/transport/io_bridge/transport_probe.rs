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
    AdmissionCounters, BudgetBinding, EcnObservation, FifoSeries, InstrumentSummary, OfferedBytes,
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
    /// The transmission it carries (its `wire_seq`), when the ledger tracks it.
    pub(super) wire_seq: Option<u32>,
    /// For a tile parity datagram, the first `wire_seq` of the group it covers.
    /// `None` for everything else, H.264 frame parity included.
    pub(super) parity_group: Option<u32>,
    /// The session's cumulative accepted bytes including this one.
    pub(super) cum_enqueued: u64,
    /// quinn's queued bytes after it.
    pub(super) queued: u64,
}

/// The mutable state behind [`InstrumentSummary`]. Measurement only: nothing
/// in the bridge reads any field of this to decide anything.
///
/// Scope. The offered-load accounting, the FIFO series and the quinn byte
/// counters describe the LIVE session only (the departure tracker, which is
/// not an instrument, follows the same rule on the bridge): they are
/// reset when a different connection becomes live (`apply_hello`) and when a
/// session is forgotten, and only sends to the live session are billed.
/// The budget, ECN, and ledger counters belong to the bridge and run for its
/// whole life (the ledger is not per session).
#[derive(Debug, Default)]
pub(super) struct Instruments {
    pub(super) offered: OfferedBytes,
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
    pub(super) loss_retx_requested: u64,
    /// NACKs for a pass not in the retransmit cache, split by whether the
    /// scheduler still holds the tile's work (`pending`: not sent yet) or the
    /// pass is no longer owed.
    pub(super) nack_miss_pending: u64,
    pub(super) nack_miss_not_owed: u64,
    pub(super) late_acks_fec_possible: u64,
    pub(super) pto_probes_requested: u64,
    pub(super) losses_flushed: u64,
    /// Declared lost but dropped from the pending list by its cap.
    pub(super) losses_overflowed: u64,
    pub(super) flushed: crate::transport::instruments::FlushedLosses,
    /// Late acknowledgements of losses goog_cc had already been told about.
    pub(super) late_acked_after_flush: u64,
    /// Capacity-hint feeds, and how many came before any feedback.
    pub(super) hint_feeds: u64,
    pub(super) hint_feeds_before_feedback: u64,
    pub(super) hint_unbounded: u64,
}

impl Instruments {
    /// Start the session-scoped instruments afresh. `bytes_base` is the new
    /// live session's accepted-byte counter at this moment.
    pub(super) fn reset_session_scope(&mut self, bytes_base: u64) {
        self.offered = OfferedBytes::default();
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
        // The previous live session's queue is gone with it.
        self.release_departures();
    }

    /// The session the departure tracker describes is over (or another one
    /// became live): nothing will ever stamp what it still holds, so forget
    /// those transmissions rather than leave them on the books. They are
    /// neither lost nor delivered, they were never sent to anyone who counts,
    /// so the estimator hears nothing about them. Quinn reuses handles, so a
    /// stale tracker would alias the next connection.
    pub(super) fn release_departures(&mut self) {
        self.departures = Default::default();
        self.departures_handle = None;
        // The whole ledger, not just what never left: records of the old
        // session would otherwise be declared lost into the next connection's
        // estimator, and its adapted threshold would carry over.
        self.transmission_ledger.reset_session();
        // And what was waiting to be reported about it: a loss of the old
        // path handed to the new connection's estimator at its first
        // acknowledgement, and the late-ack marks that name the old session's
        // wire_seqs.
        self.pending_losses.clear();
        self.late_acked.clear();
        self.instr.flushed = Default::default();
        // The sampler's `delivered` and ordinals belong to the session that
        // is gone, and the ledger reset above dropped the marks it issued:
        // one place resets both.
        self.delivery_sampler =
            crate::transport::delivery_sampler::DeliverySampler::new(super::now_std());
    }

    /// Move what the emitter put on the wire since the last call into the
    /// ledger. A transmission has to be on the books before it can be stamped
    /// as departed, so this runs ahead of every departure poll as well as at
    /// the end of the turn.
    pub(super) fn record_emitted_transmissions(&mut self, now: std::time::Instant) {
        let mut recorded = false;
        for e in self.reliable_emitter.take_transmissions() {
            self.offered_wire_bytes += e.wire_bytes as f64;
            recorded = true;
            self.transmission_ledger.record(
                e.wire_seq,
                now,
                crate::transport::transmission_ledger::Transmission {
                    emit_us: e.emit_us,
                    wire_bytes: e.wire_bytes,
                    key: e.key,
                    depart_us: None,
                    dep_idx: None,
                },
            );
        }
        if recorded {
            self.bwe.note_offered(now, self.offered_wire_bytes);
        }
    }

    fn pacer_limited_now(&self) -> bool {
        pacer_limited_term(self.last_admission_term)
    }

    /// `wire_seq` left for the wire at `at`: give its ledger record a departure
    /// ledger record and the timer on its cache entry.
    pub(super) fn stamp_departure(&mut self, wire_seq: u32, at: std::time::Instant) {
        let pacer_limited = self.pacer_limited_now();
        stamp_departure_in(
            &mut self.transmission_ledger,
            &mut self.reliable_emitter,
            &mut self.delivery_sampler,
            pacer_limited,
            wire_seq,
            at,
        );
    }

    /// A datagram that was consumed before quinn saw it (a harness-injected
    /// drop) has left the host as far as the transport is concerned: it is on
    /// the wire and lost. It departs now, so later acknowledgements can find the
    /// loss rather than never.
    pub(super) fn departed_without_quinn(&mut self, wire_seq: Option<u32>) {
        let Some(ws) = wire_seq else { return };
        let now = super::now_std();
        self.record_emitted_transmissions(now);
        self.stamp_departure(ws, now);
    }

    /// Mirror a datagram quinn just accepted into the departure tracker.
    /// `cum_enqueued` is the session's cumulative accepted byte count
    /// including it, `wire_seq` the transmission it carries if the ledger
    /// tracks it. Every datagram quinn accepts for the live session goes
    /// through here, whether or not it names a transmission: the byte
    /// arithmetic needs the whole FIFO.
    pub(super) fn track_enqueue(
        &mut self,
        handle: ConnectionHandle,
        wire_seq: Option<u32>,
        cum_enqueued: u64,
    ) {
        match self.departures_handle {
            Some(h) if h == handle => {}
            other => {
                // A different connection than the tracker describes. Quinn
                // reuses handles from a slab, so the old state must not
                // alias it. With no tracker yet (`None`) there is nothing of
                // a previous session to release: everything on the books is
                // this session's, and the ledger holds transmissions queued
                // for it that have not been sent, which must stay.
                if other.is_some() {
                    self.release_departures();
                }
                self.departures_handle = Some(handle);
            }
        }
        let now = super::now_std();
        if let Some(pushed_out) = self.departures.on_enqueue(wire_seq, cum_enqueued, now) {
            // The backstop cap; see `TRACKER_CAP`.
            self.stamp_departure(pushed_out, now);
        }
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
    /// put it in the departure tracker and track the FIFO's peak depth.
    pub(super) fn note_enqueued(&mut self, handle: ConnectionHandle, e: Enqueued) {
        let Enqueued {
            class,
            gated,
            dg_len,
            framing_len,
            wire_seq,
            parity_group,
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
        self.track_enqueue(handle, wire_seq, cum_enqueued);
        // The ledger wants to know when a group's parity leaves (see
        // `Resolution::Late::fec_possible`).
        if let Some(first) = parity_group {
            self.departures.tag_last_as_parity(first);
        }
    }

    /// After a `poll_transmit` batch, stamp the datagrams that left quinn's
    /// FIFO: start the ledger's loss clock and the emitter's retransmission
    /// timer for each transmission among them, at `now`, the instant given to
    /// `poll_transmit`. Does nothing while the tracker is empty.
    pub(super) fn poll_departures(&mut self, now: std::time::Instant) {
        let Some(handle) = self.departures_handle else {
            return;
        };
        if self.departures.is_empty() {
            return;
        }
        // Everything stamped below was built before it was sent, but the
        // ledger only hears of it at the end of a turn.
        self.record_emitted_transmissions(now);
        let capacity = self.server.datagram_send_buffer();
        let (Some(conn), Some(wt)) = (
            self.server.connections.get_mut(&handle),
            self.wt_sessions.get(&handle),
        ) else {
            return;
        };
        let queued = capacity.saturating_sub(conn.datagrams().send_buffer_space()) as u64;
        let ledger = &mut self.transmission_ledger;
        let emitter = &mut self.reliable_emitter;
        let sampler = &mut self.delivery_sampler;
        let pacer_limited = pacer_limited_term(self.last_admission_term);
        let marked_before = sampler.marked();
        self.departures
            .on_poll(wt.datagram_bytes_enqueued(), queued, now, |ws, _| {
                if let Some(ws) = ws {
                    stamp_departure_in(ledger, emitter, sampler, pacer_limited, ws, now);
                }
            });
        let retain =
            (self.reliable_emitter.rtt.srtt() * 2).max(crate::transport::loss_window::SETTLE);
        for group_first in self.departures.take_departed_parity() {
            self.transmission_ledger
                .note_parity_departed(group_first, now, retain);
        }
        // App-limited, in the delivery-rate draft's sense: these departures
        // left nothing behind in admission and nothing queued in quinn, so
        // the sender, not the path, ran out of work. `queued` is the FIFO
        // after the batch; admission's own backlog is the F1 signal.
        if self.delivery_sampler.marked() != marked_before
            && queued == 0
            && !self.admission_backlog(now)
        {
            self.delivery_sampler.note_no_backlog();
        }
    }

    /// Snapshot every instrument.
    pub(super) fn instrument_summary(&self) -> InstrumentSummary {
        let i = &self.instr;
        // Mirror the session counter against what was still queued at the
        // last reading; both are as of the last `update_transport_probe`.
        let lag = &self.departures.lag;
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
            ledger_expired_before_departure: self
                .transmission_ledger
                .stats()
                .expired_before_departure,
            ledger_departures_stamped: self.transmission_ledger.stats().departures_stamped,
            ledger_evicted_before_departure: self
                .transmission_ledger
                .stats()
                .evicted_before_departure,
            loss_by_packet_threshold: self.transmission_ledger.stats().lost_packet_threshold,
            loss_by_time_threshold: self.transmission_ledger.stats().lost_time_threshold,
            loss_by_pto: self.transmission_ledger.stats().lost_pto,
            ledger_spurious_losses: self.transmission_ledger.stats().spurious_losses,
            k_pkt: self.transmission_ledger.stats().k_pkt,
            k_pkt_raises: self.transmission_ledger.stats().k_pkt_raises,
            loss_retx_requested: i.loss_retx_requested,
            nack_miss_pending: i.nack_miss_pending,
            nack_miss_not_owed: i.nack_miss_not_owed,
            late_acks_fec_possible: i.late_acks_fec_possible,
            pto_probes_requested: i.pto_probes_requested,
            pto_fires: self.reliable_emitter.stats.pto_fires,
            loss_retransmits: self.reliable_emitter.stats.loss_retransmits,
            pto_probe_retransmits: self.reliable_emitter.stats.pto_probe_retransmits,
            verdict_content_gone: self.reliable_emitter.stats.verdict_content_gone,
            verdict_newer_in_flight: self.reliable_emitter.stats.verdict_newer_in_flight,
            verdict_already_queued: self.reliable_emitter.stats.verdict_already_queued,
            verdict_deferred_to_pto: self.reliable_emitter.stats.verdict_deferred_to_pto,
            spurious_loss: self.reliable_emitter.stats.spurious_loss,
            spurious_pto: self.reliable_emitter.stats.spurious_pto,
            ledger_unknown_acks: self.transmission_ledger.stats().unknown_acks,
            ledger_tombstone_evictions: self.transmission_ledger.stats().tombstone_evictions,
            ledger_lost_packet_threshold: self.transmission_ledger.stats().lost_packet_threshold,
            ledger_lost_time_threshold: self.transmission_ledger.stats().lost_time_threshold,
            offered: i.offered,
            quinn_bytes_enqueued: i.quinn_bytes_enqueued,
            quinn_bytes_departed: i.quinn_bytes_enqueued.saturating_sub(i.quinn_bytes_queued),
            lag_samples: lag.count(),
            lag_p50_us: lag.quantile_us(0.5),
            lag_p95_us: lag.quantile_us(0.95),
            lag_max_us: lag.max_us(),
            mirror_len: self.departures.len() as u64,
            mirror_overflowed: self.departures.overflowed,
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
            generation_replacements: self.reliable_emitter.stats.generation_replacements,
            loss_path_resends_superseded: self.reliable_emitter.stats.loss_path_resends_superseded,
            hint_feeds: i.hint_feeds,
            hint_feeds_before_feedback: i.hint_feeds_before_feedback,
            hint_unbounded: i.hint_unbounded,
            bwe: self.bwe.snapshot().stats,
            delivery: self.delivery_sampler.stats(super::now_std()),
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

/// Whether the admission rate was set by goog_cc's pacer or the per-tick
/// floor rather than by the window or a probe: a delivery-rate sample over
/// such departures is capped by that term, not by the path.
fn pacer_limited_term(t: crate::transport::instruments::BudgetTerm) -> bool {
    use crate::transport::instruments::BudgetTerm;
    matches!(t, BudgetTerm::Pacer | BudgetTerm::Floor)
}

/// [`IoBridge::stamp_departure`] on the two fields it touches, for a caller
/// that holds a borrow of another one (the departure tracker's callback).
fn stamp_departure_in(
    ledger: &mut crate::transport::transmission_ledger::TransmissionLedger,
    emitter: &mut crate::transport::reliable_emitter::ReliableTileEmitter,
    sampler: &mut crate::transport::delivery_sampler::DeliverySampler,
    pacer_limited: bool,
    wire_seq: u32,
    at: std::time::Instant,
) {
    if ledger.mark_departed(wire_seq, at, emitter.emit_us64(at)) {
        ledger.mark_rate(wire_seq, |bytes, in_flight, dep_idx| {
            sampler.on_departure(at, bytes, in_flight, pacer_limited, dep_idx)
        });
    }
    emitter.on_departed(wire_seq, at);
}
