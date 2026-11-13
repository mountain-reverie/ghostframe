//! Inbound FEC v2 rev 6 feedback that is not an acknowledgement: LOSS_REPORT
//! and REPAIR_REQUEST datagrams, and the repair-status stream's records
//! (RECOVERED, CONTENT_REJECTED, DECLINED). Plan decisions 7-9 of
//! `docs/superpowers/plans/2026-11-10-fec-v2-rev6-r6c.md`.
//!
//! No block is ever sent in R6-c, so RECOVERED and DECLINED cannot occur from a
//! correct client; they are implemented and unit-tested anyway.
//!
//! Must not: resolve a transmission in the ledger. Only an ACK does that
//! (`ack_dispatch`); a RECOVERED report releases content and nothing else.

use super::*;
use ghostframe_protocol::fec::feedback::{LossReport, RepairRequest};
use ghostframe_protocol::fec::repair_status::{StatusReader, StatusRecord};
use quinn_proto::StreamId;

/// Repair-status streams read per connection. A client opens one; the cap only
/// bounds a peer that opens many.
const MAX_STATUS_STREAMS_PER_HANDLE: usize = 8;

/// Counters for the rev 6 inbound feedback (cumulative, logged on the
/// `fec v2 inbound` line).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct FecInboundStats {
    /// CONTENT_REJECTED entries received, and the share that named a bundle
    /// or record this server no longer (or never) held.
    pub(super) content_rejected_rx: u64,
    pub(super) content_rejected_unknown: u64,
    /// The share handled as a tile re-encode and as an H.264 frame repair.
    pub(super) content_rejected_tile: u64,
    pub(super) content_rejected_h264: u64,
    /// RECOVERED ids received, and the share with no ledger record left.
    pub(super) recovered_rx: u64,
    pub(super) recovered_unknown: u64,
    pub(super) declined_rx: u64,
    pub(super) repair_requests_rx: u64,
    pub(super) repair_requests_malformed: u64,
    /// Streams whose bytes failed to parse (one count per stream).
    pub(super) repair_status_malformed: u64,
    pub(super) loss_reports_rx: u64,
    pub(super) loss_reports_malformed: u64,
}

/// One repair-status stream's reader. A failed stream stays in the map so its
/// later bytes are ignored instead of starting a fresh parse mid-record.
#[derive(Debug, Default)]
pub(super) struct StatusStream {
    reader: StatusReader,
    failed: bool,
}

impl IoBridge {
    /// Dispatch what every session's WebTransport layer has queued from its
    /// client-opened bidi streams: repair-status bytes to that stream's
    /// reader, everything else to the feedback parser as before.
    ///
    /// Collects every session's feedback before dispatching any of it
    /// (`iter_mut` on `wt_sessions` cannot be held across `&mut self` calls).
    /// Known race, not fixed here: if an incumbent's re-HELLO and a new
    /// client's first HELLO land in the same batch, which `apply_hello` runs
    /// second (and evicts the other) is a coin flip per pass, because
    /// `wt_sessions` is a `HashMap`. Both parties receive an eviction notice
    /// (best-effort; see `evict_session`), so it is not a wedge.
    pub(super) fn dispatch_drained_feedback(&mut self) {
        let feedback_data: Vec<(ConnectionHandle, StreamId, bool, Vec<u8>)> = self
            .wt_sessions
            .iter_mut()
            .flat_map(|(&handle, wt)| {
                wt.drain_feedback_routed()
                    .into_iter()
                    .map(move |(sid, status, d)| (handle, sid, status, d))
            })
            .collect();
        // Read after the bytes were taken: what a stream said before it ended
        // is still in `feedback_data`, and is dispatched before its reader goes.
        let ended: Vec<(ConnectionHandle, StreamId)> = self
            .wt_sessions
            .iter_mut()
            .flat_map(|(&handle, wt)| {
                wt.drain_ended_status_streams()
                    .into_iter()
                    .map(move |sid| (handle, sid))
            })
            .collect();
        for (handle, sid, status, data) in &feedback_data {
            if *status {
                self.dispatch_status_bytes(*handle, *sid, data);
            } else {
                self.dispatch_feedback_bytes(*handle, data);
            }
        }
        for key in ended {
            self.status_streams.remove(&key);
        }
    }

    /// A LOSS_REPORT (0x07): keep the last one's key fields for the log line
    /// and, in adaptive mode, hand it to the controller, which may change the
    /// live block parameters at once (`adapt_step`).
    pub(crate) fn dispatch_loss_report_datagram(&mut self, dg: &[u8]) {
        match LossReport::decode(dg) {
            Ok(r) => {
                self.fec_in.loss_reports_rx += 1;
                let p = &mut self.transport_probe;
                p.loss_reports += 1;
                p.report_lost_pre_fec = r.lost_pre_fec;
                p.report_lost_pre_fec_total += u64::from(r.lost_pre_fec);
                p.report_recovered = r.recovered;
                p.report_residual = r.residual_lost;
                if self.fec_adapt.is_some() {
                    let now_us = self.reliable_emitter.emit_us64(now_std());
                    let queue = self.bwe.standing_queue();
                    if let Some(a) = self.fec_adapt.as_mut() {
                        a.on_report(&r, queue, now_us);
                    }
                    self.adapt_step(now_us);
                }
            }
            Err(e) => {
                self.fec_in.loss_reports_malformed += 1;
                tracing::debug!(target: LOG_TARGET, error = %e, "malformed loss report; dropped");
            }
        }
    }

    /// A REPAIR_REQUEST (0x0B): counted, and each entry served from its
    /// block (fresh rows, capped at `r + 4`).
    pub(crate) fn dispatch_repair_request_datagram(&mut self, dg: &[u8]) {
        match RepairRequest::decode(dg) {
            Ok(req) => {
                self.fec_in.repair_requests_rx += 1;
                self.blocks_on_repair_request(&req.entries);
            }
            Err(e) => {
                self.fec_in.repair_requests_malformed += 1;
                tracing::debug!(target: LOG_TARGET, error = %e, "malformed repair request; dropped");
            }
        }
    }

    /// Bytes from a repair-status stream (`sid`) of `from`. Only the live
    /// session's records are honoured. A parse error ends the stream: warned
    /// once, counted, and its later bytes are ignored.
    pub(crate) fn dispatch_status_bytes(
        &mut self,
        from: ConnectionHandle,
        sid: StreamId,
        data: &[u8],
    ) {
        if !self.is_live(from) {
            self.non_live_feedback_drops = self.non_live_feedback_drops.saturating_add(1);
            return;
        }
        if !self.status_streams.contains_key(&(from, sid)) {
            let held = self
                .status_streams
                .keys()
                .filter(|(h, _)| *h == from)
                .count();
            if held >= MAX_STATUS_STREAMS_PER_HANDLE {
                return;
            }
        }
        let st = self.status_streams.entry((from, sid)).or_default();
        if st.failed {
            return;
        }
        match st.reader.push(data) {
            Ok(records) => {
                for r in records {
                    self.handle_status_record(r);
                }
            }
            Err(e) => {
                st.failed = true;
                self.fec_in.repair_status_malformed += 1;
                tracing::warn!(
                    target: LOG_TARGET,
                    ?from,
                    ?sid,
                    error = %e,
                    "malformed repair-status stream; ignoring the rest of it"
                );
            }
        }
    }

    pub(super) fn handle_status_record(&mut self, rec: StatusRecord) {
        match rec {
            StatusRecord::Recovered(ids) => self.on_recovered(&ids),
            StatusRecord::ContentRejected(list) => {
                for (id, idx) in list {
                    self.on_content_rejected(id, idx);
                }
            }
            StatusRecord::Declined {
                block_id,
                reason,
                k,
                missing,
            } => {
                self.fec_in.declined_rx += 1;
                tracing::debug!(target: LOG_TARGET, block_id, ?reason, "repair declined");
                self.blocks_on_declined(block_id, k, &missing);
            }
        }
    }

    /// CONTENT_REJECTED `(bundle_seq, record_idx)`: the client received the
    /// bundle and refused that record's content. Handled without the cache
    /// entry (spec 4.9.3): the bundle's ACK may already have released it, and
    /// re-sending cached bytes could fail prevalidation again. A tile is
    /// re-encoded fresh (the oversize path: cancel in flight, invalidate the
    /// baseline); an H.264 P fragment makes its frame suspect, as a loss
    /// verdict does; an IDR fragment starts a new epoch. Congestion state is
    /// untouched. The bundle is read from the ledger, else from the
    /// recent-bundle ring.
    fn on_content_rejected(&mut self, id: u32, idx: u8) {
        use crate::transport::reliable_emitter::Content;
        self.fec_in.content_rejected_rx += 1;
        let record = self
            .transmission_ledger
            .peek(id)
            .map(|t| t.records.get(usize::from(idx)).copied())
            .or_else(|| {
                self.recent_bundles
                    .get(id)
                    .map(|r| r.get(usize::from(idx)).copied())
            })
            .flatten();
        let Some((_, content)) = record else {
            self.fec_in.content_rejected_unknown += 1;
            return;
        };
        match content {
            Content::Tile(key) => {
                self.fec_in.content_rejected_tile += 1;
                self.cancel_in_flight_for_tile(key.tile_x, key.tile_y);
                self.ensure_baseline_invalidated();
            }
            Content::H264Frag { frame_seq, .. } => {
                if self.h264_refs_content_rejected(frame_seq, now_std()) {
                    self.fec_in.content_rejected_h264 += 1;
                } else {
                    self.fec_in.content_rejected_unknown += 1;
                }
            }
            // Padding carries nothing the picture needs.
            Content::Pad(_) => self.fec_in.content_rejected_unknown += 1,
        }
    }

    /// RECOVERED: the client rebuilt these bundles from repair symbols. Their
    /// content is delivered, so release it, but nothing was acknowledged: no
    /// RTT, BWE sample, loss-window entry, delivery-rate sample or
    /// `pending_losses` change (rev 5 section 4.9.1 rule 3), and the ledger
    /// record is only read, never resolved ([`TransmissionLedger::peek`]).
    /// The consumers run are the content-release ones: `on_ack`, the H.264
    /// content side ([`Self::release_h264_content`], without the transmission
    /// outcome counters) and `apply_ack_coverage`.
    fn on_recovered(&mut self, ids: &[u32]) {
        let now = now_std();
        let mut found = Vec::new();
        for &id in ids {
            self.fec_in.recovered_rx += 1;
            match self.transmission_ledger.peek(id) {
                Some(tx) => found.push((tx.clone(), 0u16, false)),
                None => self.fec_in.recovered_unknown += 1,
            }
        }
        let emit_keys: Vec<crate::transport::reliable_emitter::EmitKey> = found
            .iter()
            .flat_map(|(tx, _, _)| tx.records())
            .filter_map(|(_, c)| c.tile())
            .collect();
        self.reliable_emitter.on_ack(&emit_keys);
        let items: Vec<(crate::transport::reliable_emitter::Content, Option<bool>)> = found
            .iter()
            .flat_map(|(tx, _, _)| tx.records().map(|(_, c)| (c, None)))
            .collect();
        self.release_h264_content(&items, now);
        self.apply_ack_coverage(&found);
        self.blocks_on_recovered(ids);
    }
}
