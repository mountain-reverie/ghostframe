//! Transport measurements for the `cumulative emit` log line and the
//! browserless harness. MEASUREMENT ONLY: nothing here feeds a decision, the
//! bandwidth estimator or the classifier.
//!
//! It answers the questions the retransmission cascade left open: how deep
//! quinn's datagram send queue stands, whether the congestion window has been
//! cut, and what the client says it received, lost and rebuilt from parity.
//! Everything is read on the loop side (`sample_all_path_stats` already holds
//! the live connection); the frame worker never touches it.

use super::IoBridge;

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
    pub client_lost: u64,
    pub client_recovered: u64,
}

impl IoBridge {
    /// Read the live session's quinn stats and send-queue depth.
    pub(super) fn update_transport_probe(&mut self) {
        let capacity = self.server.datagram_send_buffer();
        let Some(conn) = self
            .last_hello_from
            .and_then(|h| self.server.connections.get_mut(&h))
        else {
            return;
        };
        let stats = conn.stats();
        let space = conn.datagrams().send_buffer_space();
        let p = &mut self.transport_probe;
        p.cwnd = stats.path.cwnd;
        p.lost_packets = stats.path.lost_packets;
        p.sent_packets = stats.path.sent_packets;
        p.congestion_events = stats.path.congestion_events;
        p.queue_bytes = capacity.saturating_sub(space);
        p.max_queue_bytes = p.max_queue_bytes.max(p.queue_bytes);
        p.samples += 1;
    }

    /// Add one `ReceiverFeedback` to the client-side totals.
    pub(super) fn note_client_feedback(
        &mut self,
        fb: &crate::transport::feedback::ReceiverFeedback,
    ) {
        let p = &mut self.transport_probe;
        p.client_received += u64::from(fb.datagrams_received);
        p.client_lost += u64::from(fb.datagrams_lost);
        p.client_recovered += u64::from(fb.datagrams_recovered_fec);
    }
}
