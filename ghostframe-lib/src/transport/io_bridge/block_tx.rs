//! The server's block send side (R6-d D4b): feeds sealed sources and closed
//! blocks into the [`BlockTable`], encodes repairs at close, and admits them
//! through the same gate as any record.
//!
//! Receive handling (arrivals, RECOVERED, DECLINED, REPAIR_REQUEST, deadline
//! release) is D5, so the table's actions are counted and dropped here: a lost
//! source still retransmits through the loss path, which is redundant and
//! correct until D6.

use super::*;
use crate::transport::fec_tx::block_table::{ClosedBlock as TableBlock, FEC_TX_MAX_BLOCKS};
use crate::transport::fec_tx::packer::{BundleKind, SealedBundle};
use ghostframe_protocol::fec::frame::{RepairFrame, FEC_REPAIR_HEADER_LEN};

/// Why the packer closed a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CloseWhy {
    KMax,
    Time,
    Empty,
    Mtu,
}

/// An encoded REPAIR frame waiting for admission. Its `bundle_seq` was
/// allocated when its block closed.
#[derive(Debug)]
pub(super) struct RepairJob {
    pub(super) bundle_seq: u32,
    pub(super) bytes: Vec<u8>,
}

/// Counters for the `cumulative emit` line.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct BlockTxStats {
    pub(super) closed_kmax: u64,
    pub(super) closed_time: u64,
    pub(super) closed_empty: u64,
    pub(super) closed_mtu: u64,
    pub(super) k_sum: u64,
    pub(super) repairs_sent: u64,
    pub(super) repair_bytes: u64,
    pub(super) repairs_queued_peak: u64,
    pub(super) repairs_dropped_mtu: u64,
    pub(super) encode_failed: u64,
    /// Table actions D4b does not execute (hand-offs, manager notices).
    pub(super) actions_dropped: u64,
}

impl IoBridge {
    /// Bundle kind of a probe-tagged first send: a lone SOURCE with blocks on.
    pub(super) fn probe_bundle_kind(&self) -> BundleKind {
        if self.fec_blocks.is_some() {
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
        let records = b.records.iter().map(|r| (r.wire_seq, r.content)).collect();
        let actions = self
            .block_table
            .on_source_sealed(block_id, idx, b.bundle_seq, records);
        self.block_tx.actions_dropped += actions.len() as u64;
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
        let Some(params) = self.fec_blocks else {
            return;
        };
        let now = now_std();
        let max_dg = self.live_max_dg();
        for c in closed {
            let k = c.k;
            let why = forced.unwrap_or(if usize::from(k) >= usize::from(params.k_max) {
                CloseWhy::KMax
            } else {
                CloseWhy::Empty
            });
            match why {
                CloseWhy::KMax => self.block_tx.closed_kmax += 1,
                CloseWhy::Time => self.block_tx.closed_time += 1,
                CloseWhy::Empty => self.block_tx.closed_empty += 1,
                CloseWhy::Mtu => self.block_tx.closed_mtu += 1,
            }
            self.block_tx.k_sum += u64::from(k);
            let r = params.r.min(255 - k);
            let fits = max_dg.is_some_and(|m| c.symbol_len() + FEC_REPAIR_HEADER_LEN <= m);
            let mut repair_bundles = Vec::new();
            if r > 0 && !fits {
                self.block_tx.repairs_dropped_mtu += u64::from(r);
            } else if r > 0 {
                let refs: Vec<&[u8]> = c.symbols.iter().map(Vec::as_slice).collect();
                let rows: Vec<u8> = (0..r).collect();
                match ghostframe_protocol::fec::codec::encode_repairs(&refs, &rows) {
                    Ok(payloads) => {
                        for (row, payload) in payloads.iter().enumerate() {
                            let bundle_seq = self.packer.alloc_seq();
                            let bytes = RepairFrame {
                                bundle_seq,
                                block_id: c.block_id,
                                k,
                                r,
                                repair_idx: row as u8,
                                payload,
                            }
                            .encode();
                            self.pending_repairs
                                .push_back(RepairJob { bundle_seq, bytes });
                            repair_bundles.push(bundle_seq);
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
                    repair_bundles,
                    last_sent: now,
                },
                now,
            );
            let evicted = self.block_table.evict_over_cap();
            self.block_tx.actions_dropped += (actions.len() + evicted.len()) as u64;
        }
        debug_assert!(self.block_table.open_blocks() <= FEC_TX_MAX_BLOCKS);
    }

    /// Admits queued repairs, oldest first, as `Critical` bytes, and seals
    /// each into `pending_bundles` behind everything already there. Returns
    /// why it stopped: `Depth` or `Tokens` leave the job queued.
    pub(super) fn service_pending_repairs(&mut self) -> pacing::Stop {
        use crate::transport::admission::{admit_one, Admit, AdmitClass};
        while let Some(len) = self.pending_repairs.front().map(|j| j.bytes.len()) {
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
            self.block_tx.repairs_sent += 1;
            self.block_tx.repair_bytes += job.bytes.len() as u64;
            let sealed = SealedBundle {
                bundle_seq: job.bundle_seq,
                bytes: job.bytes,
                records: Default::default(),
                emit_us: 0,
                kind: BundleKind::Repair,
                block: None,
            };
            self.record_bundle(&sealed);
            self.pending_bundles.push_back(sealed);
            if !self.drive_pending_bundles() {
                return pacing::Stop::Rejected;
            }
        }
        pacing::Stop::Empty
    }

    /// The Empty close at the tail of a pump: the last open bundle is sealed
    /// as LAST and its block closes.
    pub(super) fn close_block_on_empty(&mut self) {
        let now_us = self.reliable_emitter.emit_us64(now_std());
        if let Some(b) = self.packer.flush_closing(now_us) {
            self.queue_bundles(smallvec::SmallVec::from_elem(b, 1));
        }
        self.drive_pending_bundles();
    }

    /// The T close, and a retry of repairs that were refused earlier. Run
    /// once per loop turn.
    pub(super) fn service_blocks(&mut self, now: std::time::Instant) {
        if self.fec_blocks.is_none() {
            return;
        }
        let now_us = self.reliable_emitter.emit_us64(now);
        if self.packer.close_if_due(now_us) {
            self.process_closed_blocks(Some(CloseWhy::Time));
        }
        self.service_pending_repairs();
    }

    /// When the open block's T elapses, for `next_wake_deadline`.
    pub(super) fn block_t_deadline(&self) -> Option<std::time::Instant> {
        let at_us = self.packer.t_close_at_us()?;
        let now = now_std();
        let left = at_us.saturating_sub(self.reliable_emitter.emit_us64(now));
        Some(now + std::time::Duration::from_micros(left))
    }
}
