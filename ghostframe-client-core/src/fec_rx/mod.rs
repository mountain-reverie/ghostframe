//! Client side of the rev 6 framing (sans-IO).
//!
//! `blocks` unbundles SOURCE / REPAIR / PLAIN frames, keeps the coded symbols
//! of each block, rebuilds lost sources with the Reed-Solomon codec and decides
//! what to do when a block's recovery window ends.

mod blocks;
pub use blocks::*;

mod ack_v4;
mod loss_report;
pub use ack_v4::AckV4Batcher;
pub use loss_report::{LossReporter, LOSS_REPORT_COUNT_TRIGGER, LOSS_REPORT_INTERVAL_US};

use std::collections::VecDeque;

use ghostframe_protocol::fec::feedback::{
    BundleAck, RepairAsk, RepairRequest, FEC_MAX_REPAIR_REQUEST_ENTRIES,
};
use ghostframe_protocol::fec::repair_status::{
    StatusRecord, FEC_REPAIR_STATUS_STREAM_TAG, FEC_STATUS_MAX_RECORD_BODY,
};

/// RECOVERED ids per record: 4 bytes each, body at most 1020.
const MAX_RECOVERED_PER_RECORD: usize = FEC_STATUS_MAX_RECORD_BODY / 4;

/// The one object `ClientCore` holds for the rev 6 framing: block store, ACK v4
/// batcher, loss reporter, and the two output queues (datagrams, and bytes for
/// the repair-status stream).
pub struct FecRx {
    blocks: BlockStore,
    acks: AckV4Batcher,
    loss: LossReporter,
    datagrams: VecDeque<Vec<u8>>,
    status: Vec<u8>,
    tag_sent: bool,
    /// Set by the first framed datagram; nothing is reported before it.
    started: bool,
    next_report_us: u64,
}

impl FecRx {
    pub fn new() -> Self {
        FecRx {
            blocks: BlockStore::new(),
            acks: AckV4Batcher::new(),
            loss: LossReporter::new(),
            datagrams: VecDeque::new(),
            status: Vec::new(),
            tag_sent: false,
            started: false,
            next_report_us: 0,
        }
    }

    /// Absorb one datagram; returns the records to dispatch. Queues the arrival
    /// ACK and RECOVERED records. A rebuilt source is never an ACK arrival.
    pub fn on_datagram(&mut self, dg: &[u8], now_us: u64) -> Vec<RxRecord> {
        let out = self.blocks.on_datagram(dg, now_us);
        if let Some(seq) = out.arrival {
            self.started = true;
            let ack = BundleAck {
                bundle_seq: seq,
                arrival_time_ms_lo16: (now_us / 1000) as u16,
            };
            if let Some(d) = self.acks.add(ack, now_us) {
                self.datagrams.push_back(d);
            }
            self.loss.on_arrival(seq);
        }
        for &seq in &out.rebuilt {
            self.loss.on_rebuilt(seq);
        }
        self.queue_recovered(&out.rebuilt);
        self.maybe_report(now_us);
        out.records
    }

    /// The client found a record it cannot use (CDF 5/3 prevalidation).
    pub fn content_rejected(&mut self, bundle_seq: u32, record_idx: u8) {
        self.push_status(&StatusRecord::ContentRejected(vec![(
            bundle_seq, record_idx,
        )]));
    }

    pub fn on_timeout(&mut self, now_us: u64) {
        if let Some(d) = self.acks.on_timeout(now_us) {
            self.datagrams.push_back(d);
        }
        let mut asks: Vec<RepairAsk> = Vec::new();
        for action in self.blocks.on_timeout(now_us) {
            match action {
                CloseAction::Request(ask) => asks.push(ask),
                CloseAction::Decline {
                    block_id,
                    reason,
                    k,
                    missing,
                } => {
                    let n = usize::from(k).div_ceil(8).min(missing.len());
                    self.push_status(&StatusRecord::Declined {
                        block_id,
                        reason,
                        k,
                        missing: missing[..n].to_vec(),
                    });
                }
            }
        }
        for chunk in asks.chunks(FEC_MAX_REPAIR_REQUEST_ENTRIES) {
            self.datagrams.push_back(
                RepairRequest {
                    entries: chunk.to_vec(),
                }
                .encode(),
            );
        }
        self.maybe_report(now_us);
    }

    /// Earliest of the ACK flush, the block windows and the next LOSS_REPORT.
    pub fn poll_timeout(&self) -> Option<u64> {
        let report = self.started.then_some(self.next_report_us);
        [self.acks.poll_timeout(), self.blocks.poll_timeout(), report]
            .into_iter()
            .flatten()
            .min()
    }

    /// ACK v4, LOSS_REPORT and REPAIR_REQUEST datagrams, in queue order.
    pub fn take_datagram(&mut self) -> Option<Vec<u8>> {
        self.datagrams.pop_front()
    }

    /// Bytes for the repair-status stream. The first `Some` starts with
    /// `FEC_REPAIR_STATUS_STREAM_TAG`.
    pub fn take_status_bytes(&mut self) -> Option<Vec<u8>> {
        if self.status.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.status))
    }

    pub fn stats(&self) -> BlockStats {
        self.blocks.stats()
    }

    fn maybe_report(&mut self, now_us: u64) {
        if !self.started {
            return;
        }
        if let Some(d) = self.loss.maybe_report(now_us, &self.blocks.stats()) {
            self.datagrams.push_back(d);
            self.next_report_us = now_us + LOSS_REPORT_INTERVAL_US;
        } else if self.next_report_us == 0 {
            self.next_report_us = now_us + LOSS_REPORT_INTERVAL_US;
        }
    }

    fn queue_recovered(&mut self, ids: &[u32]) {
        for chunk in ids.chunks(MAX_RECOVERED_PER_RECORD) {
            self.push_status(&StatusRecord::Recovered(chunk.to_vec()));
        }
    }

    fn push_status(&mut self, rec: &StatusRecord) {
        // The tag is the first byte ever queued, so it precedes the first record
        // in the first `take_status_bytes` that returns Some.
        if !self.tag_sent {
            self.tag_sent = true;
            self.status.push(FEC_REPAIR_STATUS_STREAM_TAG);
        }
        rec.encode(&mut self.status);
    }
}

impl Default for FecRx {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_protocol::fec::codec::encode_repairs;
    use ghostframe_protocol::fec::feedback::{BundleAckBatch, LossReport};
    use ghostframe_protocol::fec::frame::{push_record, RepairFrame, SourceFrame};
    use ghostframe_protocol::fec::repair_status::{DeclineReason, StatusReader};

    const T: u64 = 1_000_000;
    const WINDOW: u64 = FEC_RX_DEFAULT_T_ADV_US + FEC_RX_ALLOWANCE_US;

    fn recs(items: &[&[u8]]) -> Vec<u8> {
        let mut v = Vec::new();
        for i in items {
            push_record(&mut v, i);
        }
        v
    }

    fn block(
        block_id: u16,
        base: u32,
        payloads: &[Vec<u8>],
        r: u8,
    ) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
        let k = payloads.len();
        let (mut srcs, mut syms) = (Vec::new(), Vec::new());
        for (i, p) in payloads.iter().enumerate() {
            let f = SourceFrame {
                last: i + 1 == k,
                bundle_seq: base + i as u32,
                block_id,
                idx: i as u8,
                records: p,
            };
            syms.push(f.coded_symbol());
            srcs.push(f.encode());
        }
        let refs: Vec<&[u8]> = syms.iter().map(|s| s.as_slice()).collect();
        let rows: Vec<u8> = (0..r).collect();
        let reps = encode_repairs(&refs, &rows).unwrap();
        let reps = reps
            .iter()
            .enumerate()
            .map(|(i, p)| {
                RepairFrame {
                    bundle_seq: base + 100 + i as u32,
                    block_id,
                    k: k as u8,
                    r,
                    repair_idx: i as u8,
                    payload: p,
                }
                .encode()
            })
            .collect();
        (srcs, reps)
    }

    fn drain_datagrams(f: &mut FecRx) -> Vec<Vec<u8>> {
        std::iter::from_fn(|| f.take_datagram()).collect()
    }

    #[test]
    fn rebuilt_sources_are_recovered_never_acked() {
        let payloads: Vec<Vec<u8>> = (0..10u8)
            .map(|i| recs(&[&vec![i; 1 + usize::from(i) * 7]]))
            .collect();
        let (srcs, reps) = block(11, 1000, &payloads, 3);
        let lost = [2usize, 5, 9];
        let mut f = FecRx::new();
        assert!(f.poll_timeout().is_none());
        assert!(f.take_datagram().is_none() && f.take_status_bytes().is_none());

        let mut arrived: Vec<u32> = Vec::new();
        let mut dispatched = 0;
        for (i, d) in srcs.iter().enumerate() {
            if !lost.contains(&i) {
                dispatched += f.on_datagram(d, T).len();
                arrived.push(1000 + i as u32);
            }
        }
        for (i, d) in reps.iter().enumerate() {
            let recs = f.on_datagram(d, T + 100);
            dispatched += recs.len();
            arrived.push(1100 + i as u32);
        }
        assert_eq!(dispatched, 10, "7 originals + 3 rebuilt, one record each");
        assert_eq!(f.stats().rebuilt, 3);

        f.on_timeout(T + 100 + 5_000);
        let mut acked: Vec<u32> = Vec::new();
        for d in drain_datagrams(&mut f) {
            if let Ok(b) = BundleAckBatch::decode(&d) {
                acked.extend(b.entries.iter().map(|e| e.bundle_seq));
            }
        }
        acked.sort_unstable();
        acked.dedup();
        arrived.sort_unstable();
        assert_eq!(acked, arrived);
        assert!(acked.iter().all(|s| ![1002, 1005, 1009].contains(s)));

        let bytes = f.take_status_bytes().expect("status bytes");
        assert_eq!(bytes[0], FEC_REPAIR_STATUS_STREAM_TAG);
        let recs = StatusReader::default().push(&bytes[1..]).unwrap();
        assert_eq!(recs, vec![StatusRecord::Recovered(vec![1002, 1005, 1009])]);
        assert!(f.take_status_bytes().is_none());

        // A later status record does not repeat the tag.
        f.content_rejected(1000, 0);
        let bytes = f.take_status_bytes().unwrap();
        assert_eq!(
            StatusReader::default().push(&bytes).unwrap(),
            vec![StatusRecord::ContentRejected(vec![(1000, 0)])]
        );
    }

    #[test]
    fn short_block_requests_once_then_declines_once() {
        let payloads: Vec<Vec<u8>> = (0..10).map(|i| recs(&[&[i; 8]])).collect();
        let (srcs, reps) = block(9, 0, &payloads, 3);
        let mut f = FecRx::new();
        for i in [0usize, 1, 3, 4, 5, 6, 7, 8] {
            f.on_datagram(&srcs[i], T);
        }
        let _ = drain_datagrams(&mut f);
        assert!(f.poll_timeout().is_some());

        f.on_timeout(T + WINDOW);
        let reqs: Vec<RepairRequest> = drain_datagrams(&mut f)
            .iter()
            .filter_map(|d| RepairRequest::decode(d).ok())
            .collect();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].entries.len(), 1);
        assert_eq!(
            (reqs[0].entries[0].block_id, reqs[0].entries[0].have),
            (9, 8)
        );
        assert!(f.take_status_bytes().is_none());

        // One repair arrives: k = 10, held 9, still one short.
        f.on_datagram(&reps[1], T + WINDOW + 10);
        f.on_timeout(T + 2 * WINDOW);
        assert!(drain_datagrams(&mut f)
            .iter()
            .all(|d| RepairRequest::decode(d).is_err()));
        let bytes = f.take_status_bytes().expect("declined");
        assert_eq!(bytes[0], FEC_REPAIR_STATUS_STREAM_TAG);
        let recs = StatusReader::default().push(&bytes[1..]).unwrap();
        match &recs[..] {
            [StatusRecord::Declined {
                block_id,
                reason,
                k,
                missing,
            }] => {
                assert_eq!((*block_id, *reason, *k), (9, DeclineReason::Pruned, 10));
                assert_eq!(missing, &vec![0b100, 0b10]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(f.stats().requests, 1);
        assert_eq!(f.stats().declined, 1);
    }

    #[test]
    fn loss_report_waits_for_a_datagram_then_every_250ms() {
        let mut f = FecRx::new();
        f.on_timeout(T);
        assert!(f.take_datagram().is_none());
        let dg = SourceFrame {
            last: true,
            bundle_seq: 1,
            block_id: 1,
            idx: 0,
            records: &recs(&[b"x"]),
        }
        .encode();
        f.on_datagram(&dg, T);
        f.on_timeout(T + 250_000);
        let got = drain_datagrams(&mut f);
        assert!(got.iter().any(|d| LossReport::decode(d).is_ok()));
    }
}
