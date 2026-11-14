// SPDX-License-Identifier: MIT OR Apache-2.0
//! Periodic LOSS_REPORT (0x07): pre-FEC loss from `bundle_seq` gaps (the
//! existing `WireLossTracker`) plus `BlockStats` deltas.
//!
//! Fields the client cannot measure yet are 0: `inter_arrival_us`,
//! `repair_late`, `repair_skipped_deadline`, `h264_frames_lost`,
//! `recovery_p95_ms`, `rx_buffer_drops`, `src_lost_pre_fec`.
//! `recovery_allowance_ms` is the constant; `residual_lost` is
//! `lost_pre_fec - recovered`, saturating at 0.

use ghostframe_protocol::fec::feedback::LossReport;

use super::blocks::{BlockStats, FEC_RX_ALLOWANCE_US};
use crate::wire_loss::{WireLossStats, WireLossTracker};

pub const LOSS_REPORT_INTERVAL_US: u64 = 250_000;
pub const LOSS_REPORT_COUNT_TRIGGER: u64 = 128;

fn sat(v: u64) -> u16 {
    v.min(u16::MAX as u64) as u16
}

pub struct LossReporter {
    wire: WireLossTracker,
    started: bool,
    last_report_us: u64,
    last_wire: WireLossStats,
    last_blocks: BlockStats,
    /// Lowest / highest bundle_seq seen in this interval.
    range: Option<(u32, u32)>,
    /// Highest bundle_seq seen ever, to measure reorder distance.
    newest: Option<u32>,
    reorder_max: u32,
}

impl LossReporter {
    pub fn new() -> Self {
        LossReporter {
            wire: WireLossTracker::new(),
            started: false,
            last_report_us: 0,
            last_wire: WireLossStats::default(),
            last_blocks: BlockStats::default(),
            range: None,
            newest: None,
            reorder_max: 0,
        }
    }

    pub fn on_arrival(&mut self, bundle_seq: u32) {
        self.wire.on_received(bundle_seq);
        self.range = Some(match self.range {
            None => (bundle_seq, bundle_seq),
            Some((lo, hi)) => (lo.min(bundle_seq), hi.max(bundle_seq)),
        });
        match self.newest {
            Some(n) if (bundle_seq.wrapping_sub(n) as i32) < 0 => {
                self.reorder_max = self.reorder_max.max(n.wrapping_sub(bundle_seq));
            }
            _ => self.newest = Some(bundle_seq),
        }
    }

    pub fn on_rebuilt(&mut self, bundle_seq: u32) {
        self.wire.on_rebuilt(bundle_seq);
    }

    /// Cumulative wire statistics.
    pub fn wire_stats(&self) -> WireLossStats {
        self.wire.stats()
    }

    /// `now_us` is the caller's clock. Returns an encoded LOSS_REPORT when 250 ms
    /// have passed or 128 bundles were newly counted, and never before the first
    /// framed datagram.
    pub fn maybe_report(&mut self, now_us: u64, blocks: &BlockStats) -> Option<Vec<u8>> {
        if !self.started {
            // The first call (arrival or not) sets the interval's baseline.
            self.started = true;
            self.last_report_us = now_us;
        }
        // Never report before the first framed datagram.
        self.range?;
        let w = self.wire.stats();
        let d = |a: u64, b: u64| a.saturating_sub(b);
        let counted =
            d(w.received, self.last_wire.received) + d(w.lost_pre_fec, self.last_wire.lost_pre_fec);
        let elapsed = now_us.saturating_sub(self.last_report_us);
        if elapsed < LOSS_REPORT_INTERVAL_US && counted < LOSS_REPORT_COUNT_TRIGGER {
            return None;
        }
        let (first, last) = self.range.unwrap_or((0, 0));
        let lost = d(w.lost_pre_fec, self.last_wire.lost_pre_fec);
        let recovered = d(blocks.rebuilt, self.last_blocks.rebuilt);
        let mut hist = [0u16; 8];
        for (i, h) in hist.iter_mut().enumerate() {
            *h = sat(d(w.run_hist[i], self.last_wire.run_hist[i]));
        }
        let rep = LossReport {
            interval_ms: sat(elapsed / 1000),
            first_bundle_seq: first,
            last_bundle_seq: last,
            received: sat(d(w.received, self.last_wire.received)),
            lost_pre_fec: sat(lost),
            recovered: sat(recovered),
            residual_lost: sat(lost.saturating_sub(recovered)),
            repair_rx: sat(d(blocks.repairs_rx, self.last_blocks.repairs_rx)),
            repair_used: sat(d(blocks.repair_used, self.last_blocks.repair_used)),
            repair_wasted: sat(d(blocks.repair_wasted, self.last_blocks.repair_wasted)),
            loss_run_hist: hist,
            reorder_max: sat(self.reorder_max as u64),
            recovery_allowance_ms: sat(FEC_RX_ALLOWANCE_US / 1000),
            blocks_unrecoverable: sat(d(
                blocks.blocks_unrecoverable,
                self.last_blocks.blocks_unrecoverable,
            )),
            repair_requests_sent: sat(d(blocks.requests, self.last_blocks.requests)),
            ..Default::default()
        };
        self.last_report_us = now_us;
        self.last_wire = w;
        self.last_blocks = *blocks;
        self.range = None;
        self.reorder_max = 0;
        Some(rep.encode())
    }
}

impl Default for LossReporter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_counts_gaps_runs_and_rebuilds() {
        let mut r = LossReporter::new();
        let mut blocks = BlockStats::default();
        assert!(
            r.maybe_report(0, &blocks).is_none(),
            "nothing before the first datagram"
        );
        for s in 0..200u32 {
            if !(50..=52).contains(&s) && s != 120 {
                r.on_arrival(s);
            }
        }
        r.on_rebuilt(51);
        blocks.rebuilt = 1;
        blocks.repairs_rx = 3;
        let dg = r.maybe_report(250_000, &blocks).expect("report due");
        let rep = LossReport::decode(&dg).unwrap();
        assert_eq!(rep.lost_pre_fec, 4);
        assert_eq!(rep.received, 196);
        assert_eq!(rep.loss_run_hist[2], 1);
        assert_eq!(rep.loss_run_hist[0], 1);
        assert_eq!(rep.recovered, 1);
        assert_eq!(rep.repair_rx, 3);
        assert_eq!((rep.first_bundle_seq, rep.last_bundle_seq), (0, 199));
        assert_eq!(rep.interval_ms, 250);
        // Deltas: an immediate second call reports nothing.
        assert!(r.maybe_report(250_001, &blocks).is_none());
    }
}
