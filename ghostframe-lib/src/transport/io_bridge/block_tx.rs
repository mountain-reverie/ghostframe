//! The server's block send side (R6-d D4b): feeds sealed sources and closed
//! blocks into the [`BlockTable`], encodes repairs at close, and admits them
//! through the same gate as any record.
//!
//! The receive side (D5) feeds the same table: arrivals, RECOVERED, DECLINED,
//! REPAIR_REQUEST and the deadline release all end in
//! [`IoBridge::execute_block_actions`]. A lost source still also retransmits
//! through the loss path, which is redundant and correct until D6.

use super::*;
use crate::transport::fec_tx::block_table::{
    Action, AlarmKind, BlockId, ClosedBlock as TableBlock, Rtt, FEC_TX_MAX_BLOCKS,
};
use crate::transport::fec_tx::packer::{BundleKind, SealedBundle};
use crate::transport::reliable_emitter::{RetxCause, RetxRequest};
use ghostframe_protocol::fec::frame::{RepairFrame, FEC_REPAIR_HEADER_LEN};

/// Why the packer closed a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseWhy {
    KMax,
    Time,
    Empty,
    Mtu,
    /// The client gave up on the block while it was still taking sources.
    Declined,
}

/// A repair row waiting for admission. Its `bundle_seq` is allocated only
/// when it is handed to quinn: one allocated and never sent would be a
/// permanent gap the client counts as lost.
#[derive(Debug)]
pub(super) struct RepairJob {
    pub(super) block_id: BlockId,
    pub(super) k: u8,
    pub(super) r: u8,
    pub(super) row: u8,
    pub(super) payload: Vec<u8>,
}

impl RepairJob {
    /// Bytes of the REPAIR frame it will become.
    pub(super) fn len(&self) -> usize {
        FEC_REPAIR_HEADER_LEN + self.payload.len()
    }
}

/// Buckets of the block-size histogram (measurement only): sizes 1 to 16
/// one each, then 17-24, 25-32, 33-48, 49-64, 65-96, 97-128, 129-192, 193+.
pub(crate) const K_BUCKETS: usize = 24;

fn k_bucket(k: u8) -> usize {
    match k {
        0..=16 => usize::from(k.max(1)) - 1,
        17..=24 => 16,
        25..=32 => 17,
        33..=48 => 18,
        49..=64 => 19,
        65..=96 => 20,
        97..=128 => 21,
        129..=192 => 22,
        _ => 23,
    }
}

/// The sizes `[lo, hi]` of bucket `i` of `InstrumentSummary::block_k_buckets`.
pub fn block_k_bucket_range(i: usize) -> (usize, usize) {
    const EDGES: [(usize, usize); 8] = [
        (17, 24),
        (25, 32),
        (33, 48),
        (49, 64),
        (65, 96),
        (97, 128),
        (129, 192),
        (193, 255),
    ];
    if i < 16 {
        (i + 1, i + 1)
    } else {
        EDGES[(i - 16).min(7)]
    }
}

/// Counters for the `cumulative emit` line.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct BlockTxStats {
    pub(super) closed_kmax: u64,
    pub(super) closed_time: u64,
    pub(super) closed_empty: u64,
    pub(super) closed_mtu: u64,
    pub(super) k_sum: u64,
    /// Blocks by size, in the buckets of `block_k_bucket_range`.
    pub(super) k_buckets: [u32; K_BUCKETS],
    pub(super) repairs_sent: u64,
    pub(super) repair_bytes: u64,
    /// Wire bytes of SOURCE bundles sealed: the base of the repair overhead.
    pub(super) source_bytes: u64,
    /// The planned repair overhead the H.264 rate target pays for.
    pub(super) rho: crate::transport::fec_tx::adapt::Rho,
    pub(super) repairs_queued_peak: u64,
    pub(super) repairs_dropped_mtu: u64,
    pub(super) encode_failed: u64,
    pub(super) closed_declined: u64,
    /// `ContentLostH264` actions executed: each tells the manager (or, for an
    /// IDR fragment, the emitter) once; never a ledger verdict.
    pub(super) content_lost_h264: u64,
    /// Tile records handed to ARQ, and the share the emitter queued.
    pub(super) handoffs: u64,
    pub(super) handoff_queued: u64,
    /// REPAIR_REQUEST entries that produced rows, and the rows.
    pub(super) requests_served: u64,
    pub(super) request_rows_sent: u64,
    pub(super) declined_handled: u64,
    /// Repair rows the table asked for whose block had no symbols left.
    pub(super) repair_rows_no_block: u64,
    pub(super) alarms: u64,
    /// Records the loss path declined to retransmit because their block
    /// still owes the content outcome.
    pub(super) protected_timer_suppressed: u64,
    /// PTO turns that named a block, and the repair rows the table gave.
    pub(super) pto_repair_asks: u64,
    pub(super) pto_repair_rows: u64,
    /// TILE_NACKs honoured for a pass whose protected source is in flight.
    pub(super) nack_on_protected: u64,
}

impl IoBridge {
    /// Block machinery on (`fec_mode`), as opposed to blocks being live now
    /// (`fec_live`): blocks and repairs sent earlier stay outstanding while
    /// the live parameters are `None`, so the table and ACK hooks follow this.
    pub(super) fn fec_machinery(&self) -> bool {
        self.fec_mode.machinery()
    }

    /// Switches the live block parameters at a block boundary: the open block
    /// closes under the parameters it was built with (so `process_closed_blocks`
    /// still reads its `r`), then `params` take over. Setting `None` first
    /// would drop that block's repairs.
    pub(super) fn apply_fec_live(&mut self, params: Option<crate::config::FecBlockParams>) {
        if self.fec_live == params {
            return;
        }
        if self.fec_machinery() {
            self.close_block_on_empty();
            self.process_closed_blocks(Some(CloseWhy::Empty));
        }
        self.fec_live = params;
        self.packer.set_blocks(params);
    }

    /// One controller step (adaptive mode; a no-op otherwise): read the
    /// table's closes, let the estimate decay and re-check the floor, then
    /// move the live parameters to what the controller wants, at a block
    /// boundary. Runs on every LOSS_REPORT and once per loop turn from
    /// `service_blocks` (the release path when no report arrives). It needs no
    /// wake deadline of its own: any loop turn runs it, and the dwell it waits
    /// on is checked then.
    ///
    /// A new span that differs from the live one by at most 1 ms and 12.5 % of
    /// `k_max` is not applied: the source-rate-derived span would otherwise
    /// close the open block on every wobble of the bandwidth estimate.
    pub(super) fn adapt_step(&mut self, now_us: u64) {
        if self.fec_adapt.is_none() {
            return;
        }
        let closes = self.block_table.take_close_log();
        let rate = self.bwe.snapshot().bitrate_bps;
        let max_dg = self.live_max_dg().unwrap_or(1200);
        let live = self.fec_live;
        let Some(a) = self.fec_adapt.as_mut() else {
            return;
        };
        a.tick(now_us, closes);
        let mut want = a.live_params(rate, max_dg, now_us);
        if let (Some(w), Some(cur)) = (want, live) {
            if w.t_ms.abs_diff(cur.t_ms) <= 1
                && u16::from(w.k_max.abs_diff(cur.k_max)) * 8 <= u16::from(cur.k_max)
            {
                want = live;
            }
        }
        self.apply_fec_live(want);
        let live = self.fec_live;
        if let Some(a) = self.fec_adapt.as_mut() {
            a.note_applied(live);
        }
    }

    /// Bundle kind of a probe-tagged first send: a lone SOURCE only while
    /// blocks are live (follows `fec_live`, else the cluster's byte count
    /// drifts by 4 bytes per packet).
    pub(super) fn probe_bundle_kind(&self) -> BundleKind {
        if self.fec_live.is_some() {
            BundleKind::Source
        } else {
            BundleKind::Plain
        }
    }

    /// Registers a sealed SOURCE with the table.
    pub(super) fn note_source_sealed(&mut self, b: &SealedBundle) {
        let Some((block_id, idx)) = b.block else {
            return;
        };
        self.block_tx.source_bytes += b.bytes.len() as u64;
        let records = b.records.iter().map(|r| (r.wire_seq, r.content)).collect();
        let actions = self
            .block_table
            .on_source_sealed(block_id, idx, b.bundle_seq, records);
        self.execute_block_actions(actions);
    }

    /// Takes the blocks the packer closed, encodes their repairs into
    /// `pending_repairs` and hands each to the table. `forced` names the
    /// cause when the caller knows it; otherwise a block of `k_max` sources
    /// closed on size and any other on Empty.
    pub(super) fn process_closed_blocks(&mut self, forced: Option<CloseWhy>) {
        let closed = self.packer.take_closed();
        if closed.is_empty() {
            return;
        }
        let Some(params) = self.fec_live else {
            return;
        };
        let now = now_std();
        let max_dg = self.live_max_dg();
        for c in closed {
            let k = c.k;
            let why = if self.block_table.closed_reason(c.block_id).is_some() {
                CloseWhy::Declined
            } else {
                forced.unwrap_or(if usize::from(k) >= usize::from(params.k_max) {
                    CloseWhy::KMax
                } else {
                    CloseWhy::Empty
                })
            };
            match why {
                CloseWhy::KMax => self.block_tx.closed_kmax += 1,
                CloseWhy::Time => self.block_tx.closed_time += 1,
                CloseWhy::Empty => self.block_tx.closed_empty += 1,
                CloseWhy::Mtu => self.block_tx.closed_mtu += 1,
                CloseWhy::Declined => self.block_tx.closed_declined += 1,
            }
            if self.block_table.closed_reason(c.block_id).is_some() {
                // Declined while it was still sending: nothing to repair.
                continue;
            }
            self.block_tx.k_sum += u64::from(k);
            self.block_tx.k_buckets[k_bucket(k)] += 1;
            let r = match self.fec_adapt.as_mut() {
                Some(a) => a.r_for_block(u32::from(k), self.reliable_emitter.emit_us64(now)),
                None => params.r,
            }
            .min(255 - k);
            let fits = max_dg.is_some_and(|m| c.symbol_len() + FEC_REPAIR_HEADER_LEN <= m);
            let mut repairs_queued = 0;
            if r > 0 && !fits {
                self.block_tx.repairs_dropped_mtu += u64::from(r);
            } else if r > 0 {
                let refs: Vec<&[u8]> = c.symbols.iter().map(Vec::as_slice).collect();
                let rows: Vec<u8> = (0..r).collect();
                match ghostframe_protocol::fec::codec::encode_repairs(&refs, &rows) {
                    Ok(payloads) => {
                        for (row, payload) in payloads.into_iter().enumerate() {
                            self.pending_repairs.push_back(RepairJob {
                                block_id: c.block_id,
                                k,
                                r,
                                row: row as u8,
                                payload,
                            });
                            repairs_queued += 1;
                        }
                        self.block_tx.repairs_queued_peak = self
                            .block_tx
                            .repairs_queued_peak
                            .max(self.pending_repairs.len() as u64);
                    }
                    Err(e) => {
                        self.block_tx.encode_failed += 1;
                        tracing::warn!(target: LOG_TARGET, error = %e, "FEC repair encode failed");
                    }
                }
            }
            let actions = self.block_table.on_closed(
                TableBlock {
                    block_id: c.block_id,
                    k,
                    r,
                    symbols: c.symbols,
                    repair_bundles: Vec::new(),
                    repairs_queued,
                    last_sent: now,
                },
                now,
            );
            let evicted = self.block_table.evict_over_cap();
            self.execute_block_actions(actions);
            self.execute_block_actions(evicted);
        }
        debug_assert!(self.block_table.open_blocks() <= FEC_TX_MAX_BLOCKS);
    }

    /// Runs what the table decided: encodes repair rows (one call per block),
    /// hands tile content to ARQ as PLAIN `Handoff` retransmits, tells the
    /// H.264 manager, logs alarms.
    pub(super) fn execute_block_actions(&mut self, actions: Vec<Action>) {
        if actions.is_empty() {
            return;
        }
        let mut rows: Vec<(BlockId, Vec<u8>)> = Vec::new();
        for a in actions {
            match a {
                Action::Repair { block_id, row } => {
                    match rows.iter_mut().find(|(id, _)| *id == block_id) {
                        Some((_, r)) => r.push(row),
                        None => rows.push((block_id, vec![row])),
                    }
                }
                Action::HandOffTile { records, .. } => {
                    for (ws, content) in records {
                        self.block_tx.handoffs += 1;
                        let r = self.reliable_emitter.request_retransmit(
                            content,
                            ws,
                            RetxCause::Handoff,
                        );
                        if r == RetxRequest::Queued {
                            self.block_tx.handoff_queued += 1;
                        }
                    }
                }
                Action::ContentLostH264 {
                    frame_seq,
                    frag_idx,
                    wire_seq,
                    cause,
                } => {
                    self.block_tx.content_lost_h264 += 1;
                    self.h264_content_lost(frame_seq, frag_idx, wire_seq, cause, now_std());
                }
                Action::Alarm { block_id, kind } => {
                    self.block_tx.alarms += 1;
                    match kind {
                        AlarmKind::TDef => tracing::warn!(
                            target: LOG_TARGET,
                            block_id,
                            "FEC block deferred past T_def; content handed off"
                        ),
                        AlarmKind::DeferralTimeout => tracing::debug!(
                            target: LOG_TARGET,
                            block_id,
                            "FEC block deferred too long (H.264); content lost to the manager"
                        ),
                    }
                }
            }
        }
        for (id, r) in rows {
            self.queue_repair_rows(id, &r);
        }
    }

    /// Encodes `rows` of block `id` from the table's symbols in one call and
    /// queues each as a repair job (admitted by `service_pending_repairs`).
    fn queue_repair_rows(&mut self, id: BlockId, rows: &[u8]) {
        let (Some((k, r)), Some(symbols)) =
            (self.block_table.params(id), self.block_table.symbols(id))
        else {
            self.block_tx.repair_rows_no_block += rows.len() as u64;
            return;
        };
        let refs: Vec<&[u8]> = symbols.iter().map(Vec::as_slice).collect();
        let payloads = match ghostframe_protocol::fec::codec::encode_repairs(&refs, rows) {
            Ok(p) => p,
            Err(e) => {
                self.block_tx.encode_failed += 1;
                tracing::warn!(target: LOG_TARGET, error = %e, "FEC repair encode failed");
                return;
            }
        };
        for (&row, payload) in rows.iter().zip(payloads) {
            self.pending_repairs.push_back(RepairJob {
                block_id: id,
                k,
                r,
                row,
                payload,
            });
        }
        self.block_tx.repairs_queued_peak = self
            .block_tx
            .repairs_queued_peak
            .max(self.pending_repairs.len() as u64);
    }

    /// The path numbers the table's deadlines are built from.
    fn block_rtt(&self) -> Rtt {
        let e = &self.reliable_emitter.rtt;
        Rtt {
            srtt: e.srtt(),
            rttvar: e.rttvar(),
            max_ack_delay: e.max_ack_delay(),
        }
    }

    /// ACK arrivals for the blocks (every entry, live or late).
    pub(super) fn blocks_on_arrivals(&mut self, entries: &[(u32, u16)]) {
        if !self.fec_machinery() {
            return;
        }
        let now = now_std();
        for &(id, _) in entries {
            let a = self.block_table.on_arrival(id, now);
            self.execute_block_actions(a);
        }
    }

    /// RECOVERED named these bundles: sources the client rebuilt.
    pub(super) fn blocks_on_recovered(&mut self, ids: &[u32]) {
        if !self.fec_machinery() {
            return;
        }
        let now = now_std();
        for &id in ids {
            let a = self.block_table.on_recovered(id, now);
            self.execute_block_actions(a);
        }
    }

    /// DECLINED: every unaccounted source is handed off (the bitmap is only
    /// a cross-check). A block still taking sources closes in the packer so
    /// no later source joins it.
    pub(super) fn blocks_on_declined(&mut self, block_id: u16, k: u8, missing: &[u8]) {
        if !self.fec_machinery() {
            return;
        }
        let now = now_std();
        let was_open = self.block_table.is_open(block_id);
        let a = self.block_table.on_declined(block_id, k, missing, now);
        if was_open {
            self.block_tx.declined_handled += 1;
        }
        self.execute_block_actions(a);
        if self.packer.open_block_id() == Some(block_id) && self.packer.close_now() {
            self.process_closed_blocks(Some(CloseWhy::Declined));
        }
    }

    /// REPAIR_REQUEST entries: fresh rows, capped at `r + 4` per block.
    pub(super) fn blocks_on_repair_request(
        &mut self,
        asks: &[ghostframe_protocol::fec::feedback::RepairAsk],
    ) {
        if !self.fec_machinery() {
            return;
        }
        let now = now_std();
        for &ask in asks {
            let a = self.block_table.on_request(ask, now);
            let rows = a
                .iter()
                .filter(|x| matches!(x, Action::Repair { .. }))
                .count() as u64;
            if rows > 0 {
                self.block_tx.requests_served += 1;
                self.block_tx.request_rows_sent += rows;
            }
            self.execute_block_actions(a);
        }
    }

    /// Admits queued repairs, oldest first, as `Critical` bytes, and seals
    /// each into `pending_bundles` behind everything already there. Returns
    /// why it stopped: `Depth` or `Tokens` leave the job queued.
    pub(super) fn service_pending_repairs(&mut self) -> pacing::Stop {
        use crate::transport::admission::{admit_one, Admit, AdmitClass};
        while let Some(len) = self.pending_repairs.front().map(RepairJob::len) {
            let max_dg = match self.live_max_dg() {
                Some(m) if self.framed_sink_present() => m,
                _ => {
                    // Nobody to send to: the block is the old session's.
                    self.pending_repairs.clear();
                    return pacing::Stop::Empty;
                }
            };
            if len > max_dg {
                self.pending_repairs.pop_front();
                self.block_tx.repairs_dropped_mtu += 1;
                continue;
            }
            // Sources quinn refused stay ahead of the repairs.
            if !self.drive_pending_bundles() {
                return pacing::Stop::Rejected;
            }
            if let Some(gate) = self.gate {
                if let Some(depth) = self.live_fifo_depth() {
                    let depth = depth + self.unsent_bundle_bytes();
                    match admit_one(
                        &mut self.admission,
                        AdmitClass::Critical,
                        len,
                        depth,
                        gate.d_target,
                    ) {
                        Admit::Go => self.instr.admission.charged_bytes += len as u64,
                        Admit::Depth => return pacing::Stop::Depth,
                        Admit::Tokens => return pacing::Stop::Tokens,
                    }
                }
            }
            let job = self.pending_repairs.pop_front().expect("front exists");
            // Allocated here, when it is certain to be sealed and offered.
            let bundle_seq = self.packer.alloc_seq();
            let bytes = RepairFrame {
                bundle_seq,
                block_id: job.block_id,
                k: job.k,
                r: job.r,
                repair_idx: job.row,
                payload: &job.payload,
            }
            .encode();
            self.block_table
                .register_repair(job.block_id, job.row, bundle_seq);
            self.block_tx.repairs_sent += 1;
            self.block_tx.repair_bytes += bytes.len() as u64;
            let sealed = SealedBundle {
                bundle_seq,
                bytes,
                records: Default::default(),
                emit_us: 0,
                kind: BundleKind::Repair,
                block: None,
            };
            self.record_bundle(&sealed);
            // A repair row is billed tokens above and is in the ledger, so
            // goog_cc is told of it as sent and counts it when acknowledged.
            // It is offered too, or `payload_fraction` (offered / spent) falls
            // to about `1 / (1 + rho)`: the pacer then grants `est / f` and the
            // ceiling reads a saturated sender as unsaturated. Every repair row
            // (block close, REPAIR_REQUEST, PTO, T_lr) is sent from here.
            self.offered_wire_bytes += sealed.bytes.len() as f64;
            self.bwe.note_offered(now_std(), self.offered_wire_bytes);
            self.pending_bundles.push_back(sealed);
            if !self.drive_pending_bundles() {
                return pacing::Stop::Rejected;
            }
        }
        pacing::Stop::Empty
    }

    /// The Empty close at the tail of a pump: the last open bundle is sealed
    /// as LAST and its block closes; with no open bundle, an open block
    /// (its sources sealed in earlier pumps) closes without LAST.
    pub(super) fn close_block_on_empty(&mut self) {
        let now_us = self.reliable_emitter.emit_us64(now_std());
        if let Some(b) = self.packer.flush_closing(now_us) {
            self.queue_bundles(smallvec::SmallVec::from_elem(b, 1));
        } else if self.packer.close_now() {
            // Every source of the block was sealed in an earlier pump: no
            // bundle left to carry LAST, but the emitter has nothing more, so
            // the block closes now (rev 6 4 rule 3), not on T.
            self.process_closed_blocks(Some(CloseWhy::Empty));
        }
        self.drive_pending_bundles();
    }

    /// The T close, and a retry of repairs that were refused earlier. Run
    /// once per loop turn.
    pub(super) fn service_blocks(&mut self, now: std::time::Instant) {
        if !self.fec_machinery() {
            return;
        }
        let now_us = self.reliable_emitter.emit_us64(now);
        self.adapt_step(now_us);
        if self.packer.close_if_due(now_us) {
            self.process_closed_blocks(Some(CloseWhy::Time));
        }
        let rtt = self.block_rtt();
        self.block_table.set_rtt(rtt);
        if self.block_table.next_deadline().is_some_and(|d| d <= now) {
            let a = self.block_table.on_timer(now, rtt);
            self.execute_block_actions(a);
        }
        self.service_pending_repairs();
    }

    /// The earliest block deadline (`T_lr`, release, alarms), for
    /// `next_wake_deadline`.
    pub(super) fn block_release_deadline(&mut self) -> Option<std::time::Instant> {
        self.block_table.next_deadline()
    }

    /// When the open block's T elapses, for `next_wake_deadline`.
    pub(super) fn block_t_deadline(&self) -> Option<std::time::Instant> {
        let at_us = self.packer.t_close_at_us()?;
        let now = now_std();
        let left = at_us.saturating_sub(self.reliable_emitter.emit_us64(now));
        Some(now + std::time::Duration::from_micros(left))
    }
}
