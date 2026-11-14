// SPDX-License-Identifier: AGPL-3.0-or-later
//! Unit-test shorthand for an ACK v4 batch.
//!
//! The ledger is keyed by a transmission id: a `bundle_seq` in production, and
//! whatever the test recorded it under in a unit test that records
//! transmissions by hand. `AckEntry::wire_seq` names that key, and
//! [`AckBatch::encode`] writes the batch as ACK v4 (0x08), the only ACK the
//! server decodes.

use super::{now_std, ConnectionHandle, IoBridge};
use ghostframe_protocol::fec::feedback::{BundleAck, BundleAckBatch};
use ghostframe_protocol::fec::frame::{parse, records, Frame};

/// The connection `attach_test_wire` makes live.
pub(crate) const LIVE: ConnectionHandle = ConnectionHandle(3);

/// quinn's datagram size for a bridge with a test wire: room for one
/// 1,200-byte fragment per bundle (two records never fit), so a bundle is a
/// transmission of one datagram, as a datagram was before the framing.
pub(crate) const RAW: usize = 1_400;

/// Make `b` send like the live server does: a live session whose connection
/// is the bridge's test wire (accepts every bundle), so records are packed into
/// PLAIN bundles and the ledger is keyed by bundle. A unit bridge has no quinn
/// connection; without this its sends are consumed with no ledger entry.
pub(crate) fn attach_test_wire(b: &mut IoBridge) {
    b.wt_sessions.insert(
        LIVE,
        crate::transport::webtransport::WebTransportServer::default(),
    );
    b.last_hello_from = Some(LIVE);
    b.test_wire = Some(super::bundle_tx::TestWire::default());
    b.forced_raw_datagram_size_for_test = Some(RAW);
}

/// The `bundle_seq` of the sent frame that carried the record `ws`. Seals the
/// open bundle first, so a record just sent is found.
pub(crate) fn bundle_of(b: &mut IoBridge, ws: u32) -> Option<u32> {
    b.flush_bundles();
    b.sent_frames_for_test.iter().find_map(|f| match parse(f) {
        Ok(Frame::Plain(p)) => records(p.records)
            .any(|r| {
                let r = r.expect("a record");
                u32::from_be_bytes(r[8..12].try_into().unwrap()) == ws
            })
            .then_some(p.bundle_seq),
        _ => None,
    })
}

/// The `bundle_seq`s of the sent frames that carried a fragment of the H.264
/// frame `frame_seq` (a record whose first bit is clear: a tile datagram starts
/// with it set).
pub(crate) fn bundles_of_h264_frame(b: &mut IoBridge, frame_seq: u32) -> Vec<u32> {
    b.flush_bundles();
    let mut out: Vec<u32> = b
        .sent_frames_for_test
        .iter()
        .filter_map(|f| match parse(f) {
            Ok(Frame::Plain(p)) => records(p.records)
                .any(|r| {
                    let r = r.expect("a record");
                    r.len() >= 22
                        && r[0] & 0x80 == 0
                        && u32::from_be_bytes(r[0..4].try_into().unwrap()) == frame_seq
                })
                .then_some(p.bundle_seq),
            _ => None,
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The bundles `ids` leave quinn's FIFO now.
pub(crate) fn depart_bundles(b: &mut IoBridge, ids: &[u32]) {
    let now = now_std();
    b.record_emitted_transmissions(now);
    // A millisecond ago, so an acknowledgement at once is a real interval to
    // the rate sampler (it divides by whole microseconds).
    let left = now
        .checked_sub(std::time::Duration::from_millis(1))
        .unwrap_or(now);
    for id in ids {
        b.stamp_departure(*id, left);
    }
}

/// The bundles that carried the records `wire_seqs` leave quinn's FIFO now.
pub(crate) fn depart_records(b: &mut IoBridge, wire_seqs: &[u32]) {
    let ids: Vec<u32> = wire_seqs
        .iter()
        .filter_map(|ws| bundle_of(b, *ws))
        .collect();
    depart_bundles(b, &ids);
}

/// Acknowledge (ACK v4) the bundles `ids`.
pub(crate) fn ack_bundles(b: &mut IoBridge, ids: &[u32]) {
    for chunk in ids.chunks(64) {
        let batch = AckBatch {
            entries: chunk
                .iter()
                .map(|id| AckEntry {
                    wire_seq: *id,
                    arrival_time_ms_lo16: 0,
                })
                .collect(),
        };
        b.dispatch_ack_v4_datagram(&batch.encode());
    }
}

/// Acknowledge (ACK v4) the bundles that carried the records `wire_seqs`.
pub(crate) fn ack_records(b: &mut IoBridge, wire_seqs: &[u32]) {
    let mut ids: Vec<u32> = wire_seqs
        .iter()
        .filter_map(|ws| bundle_of(b, *ws))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ack_bundles(b, &ids);
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct AckEntry {
    /// The ledger key of the acknowledged transmission.
    pub wire_seq: u32,
    pub arrival_time_ms_lo16: u16,
}

#[derive(Debug, Clone)]
pub(crate) struct AckBatch {
    pub entries: Vec<AckEntry>,
}

impl AckBatch {
    pub(crate) fn encode(&self) -> Vec<u8> {
        BundleAckBatch {
            entries: self
                .entries
                .iter()
                .map(|e| BundleAck {
                    bundle_seq: e.wire_seq,
                    arrival_time_ms_lo16: e.arrival_time_ms_lo16,
                })
                .collect(),
        }
        .encode()
    }
}
