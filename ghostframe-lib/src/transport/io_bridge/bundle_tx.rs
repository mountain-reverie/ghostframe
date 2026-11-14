// SPDX-License-Identifier: AGPL-3.0-or-later
//! The rev 6 framing on the send side: every
//! datagram leaves inside a PLAIN bundle built by the per-session
//! [`Packer`](crate::transport::fec_tx::packer::Packer).
//!
//! How a record travels:
//!
//! 1. The emitter builds it (allocating its `wire_seq`, which is now a record
//!    id) and reports `(wire_seq, content, emit_us)`. `record_emitted_transmissions`
//!    parks that in `emitted_meta` instead of the ledger, which is keyed by
//!    bundle.
//! 2. The drain admits it (`record_len + 2` against the bucket) and hands it to
//!    `send_counted`, which takes its meta back out of `emitted_meta` and
//!    pushes it into the packer. The test hooks that model content loss (the
//!    env loss injector, the drop plan) have already run, per record.
//! 3. A sealed bundle is recorded in the ledger (one transmission, all its
//!    records) and queued in `pending_bundles`; `drive_pending_bundles` offers
//!    them to quinn oldest first. A refusal leaves the bundle at the head, and
//!    while one stands no further record is accepted from the emitter
//!    (`Rejected`), so nothing is lost and nothing is sent out of order. The
//!    bandwidth cap models the link and so applies here, to the bundle.
//! 4. A bundle accepted by quinn is billed (`note_bundle_enqueued`): records
//!    as their own class (`len + 2`), the frame header and the QUIC framing as
//!    `Control`, so `offered_total == quinn_bytes_enqueued` holds.

use super::*;
use crate::transport::fec_tx::packer::{BundleKind, Packer, RecordMeta, SealedBundle};
use crate::transport::reliable_emitter::emitter::EmittedDatagram;
use crate::transport::reliable_emitter::traits::SendOutcome;
use crate::transport::reliable_emitter::Content;
use crate::transport::transmission_ledger::Transmission;
use ghostframe_protocol::fec::frame::FEC_RECORD_LEN_PREFIX;
use smallvec::SmallVec;

/// `emitted_meta` is cleared when it passes this many entries: records that
/// were built and then purged from the emission queue are never consumed, and
/// a session that ends takes the rest with `release_departures`.
const EMITTED_META_CAP: usize = 1 << 17;

/// Unit tests only: stands in for the quinn connection a unit bridge lacks.
#[cfg(test)]
#[derive(Debug, Default)]
pub(crate) struct TestWire {
    /// Refuse every bundle (`Blocked`).
    pub(crate) blocked: bool,
    /// Refuse every bundle as `TooLarge` (the path shrank).
    pub(crate) too_large: bool,
    /// Accepted bytes, frame plus one byte of framing each.
    pub(crate) cum: u64,
}

/// What offering one datagram to the live session came to.
pub(super) enum Offer {
    /// No live session: the datagram counts as sent.
    NoSession,
    Taken {
        handle: ConnectionHandle,
        /// The session's cumulative accepted bytes including this one.
        cum: u64,
        framing: usize,
        /// quinn's queued bytes after it.
        queued: u64,
    },
    Refused(quinn_proto::SendDatagramError),
}

impl IoBridge {
    /// Offer `dg` (a sealed bundle) to the live session's quinn connection.
    pub(super) fn offer_to_live(&mut self, dg: &[u8]) -> Offer {
        #[cfg(test)]
        if self.live_send_handle().is_none() {
            if let Some(w) = self.test_wire.as_mut() {
                if w.too_large {
                    return Offer::Refused(quinn_proto::SendDatagramError::TooLarge);
                }
                if w.blocked {
                    return Offer::Refused(quinn_proto::SendDatagramError::Blocked(
                        bytes::Bytes::new(),
                    ));
                }
                w.cum += dg.len() as u64 + 1;
                return Offer::Taken {
                    handle: self.last_hello_from.unwrap_or(ConnectionHandle(0)),
                    cum: w.cum,
                    framing: 1,
                    queued: 0,
                };
            }
        }
        let send_capacity = self.server.datagram_send_buffer();
        let Some(handle) = self.live_send_handle() else {
            return Offer::NoSession;
        };
        let (Some(wt), Some(conn)) = (
            self.wt_sessions.get_mut(&handle),
            self.server.connections.get_mut(&handle),
        ) else {
            return Offer::NoSession;
        };
        match wt.send_datagram(conn, dg) {
            Ok(()) => Offer::Taken {
                handle,
                cum: wt.datagram_bytes_enqueued(),
                framing: wt.datagram_framing_len(),
                queued: send_capacity.saturating_sub(conn.datagrams().send_buffer_space()) as u64,
            },
            Err(e) => Offer::Refused(e),
        }
    }

    /// Wire bytes that are admitted but not yet in quinn's FIFO: the open
    /// bundle and the sealed ones quinn has not taken, each with its frame
    /// header and the QUIC framing it will carry. The depth gate counts them,
    /// or records admitted while a bundle fills would overshoot `D_target`;
    /// and it counts them as the FIFO will (`queued` in
    /// `note_bundle_enqueued`), or a short bundle's header would let the FIFO
    /// stand at `D_target` when the next one is accepted.
    pub(super) fn unsent_bundle_bytes(&self) -> usize {
        let framing = self.live_framing_len();
        let open = self.packer.open_bytes();
        let open = if open == 0 {
            0
        } else {
            open + ghostframe_protocol::fec::frame::FEC_PLAIN_HEADER_LEN + framing
        };
        open + self
            .pending_bundles
            .iter()
            .map(|b| b.bytes.len() + framing)
            .sum::<usize>()
    }

    /// Whether a send to the live session can happen at all.
    pub(super) fn framed_sink_present(&self) -> bool {
        #[cfg(test)]
        if self.test_wire.is_some() {
            return true;
        }
        self.live_send_handle().is_some()
    }

    /// The largest datagram (WebTransport framing excluded) quinn takes now.
    pub(super) fn live_max_dg(&mut self) -> Option<usize> {
        let framing = self.live_framing_len();
        self.live_max_datagram_size_raw()
            .map(|raw| raw.saturating_sub(framing))
    }

    /// The longest inner datagram for a path datagram of `max_dg` bytes: the
    /// packer's record limit.
    pub(super) fn inner_limit(&self, max_dg: usize) -> usize {
        Packer::record_limit(max_dg)
    }

    /// Move what the emitter built into `emitted_meta`.
    pub(super) fn park_emitted(&mut self, e: EmittedDatagram) {
        if self.emitted_meta.len() >= EMITTED_META_CAP {
            tracing::warn!(
                target: LOG_TARGET,
                entries = self.emitted_meta.len(),
                "emitted-record meta table full, clearing it"
            );
            self.emitted_meta.clear();
        }
        self.emitted_meta.insert(e.wire_seq, e);
    }

    /// A record has no ledger entry of its own, only its parked meta.
    pub(super) fn take_undeparted_record(&mut self, wire_seq: u32) -> Option<Transmission> {
        let e = self.emitted_meta.remove(&wire_seq)?;
        Some(Transmission::single(
            e.emit_us,
            e.wire_bytes,
            e.wire_seq,
            e.content,
            None,
            None,
        ))
    }

    /// A record consumed before it reached the wire by a harness hook (the
    /// loss injector, the drop plan). The record becomes a transmission of its own, keyed by a
    /// `bundle_seq` taken from the packer (the client sees the gap and counts
    /// a pre-FEC loss, which is what it is), and departs now without ever
    /// being offered to quinn: the loss path declares it lost and
    /// retransmits it. It bills nothing here; the drain already admitted the
    /// record.
    #[cfg(any(test, feature = "test-loss-injection"))]
    pub(super) fn consumed_before_wire(&mut self, wire_seq: Option<u32>, dg_len: usize) {
        let Some(ws) = wire_seq else { return };
        let now = now_std();
        self.record_emitted_transmissions(now);
        let Some(e) = self.emitted_meta.remove(&ws) else {
            return;
        };
        let bundle_seq = self.packer.alloc_seq();
        let wire_bytes = dg_len
            + ghostframe_protocol::fec::frame::FEC_PLAIN_HEADER_LEN
            + FEC_RECORD_LEN_PREFIX
            + self.live_framing_len();
        self.transmission_ledger.record(
            bundle_seq,
            now,
            Transmission::single(e.emit_us, wire_bytes, e.wire_seq, e.content, None, None),
        );
        self.stamp_departure(bundle_seq, now);
    }

    /// The framed half of `send_counted`. The record (or control datagram,
    /// `wire_seq == None`) is consumed: it is now in the open bundle or a
    /// pending one. `Rejected` only when an earlier bundle is still refused,
    /// in which case nothing was consumed.
    pub(super) fn send_counted_framed(
        &mut self,
        dg: &[u8],
        class: WireClass,
        wire_seq: Option<u32>,
    ) -> SendOutcome {
        if !self.drive_pending_bundles() {
            return SendOutcome::Rejected;
        }
        let now = now_std();
        let now_us = self.reliable_emitter.emit_us64(now);
        let max_dg = match self.live_max_dg() {
            Some(m) if self.framed_sink_present() => m,
            _ => {
                // No live session: consumed, as without framing.
                if let Some(ws) = wire_seq {
                    self.emitted_meta.remove(&ws);
                }
                return SendOutcome::Sent;
            }
        };
        // A path that shrank closes the open block at its current size.
        if self.packer.close_for_mtu(max_dg) {
            self.process_closed_blocks(Some(block_tx::CloseWhy::Mtu));
        }
        let meta = wire_seq.and_then(|ws| {
            if !self.emitted_meta.contains_key(&ws) {
                self.record_emitted_transmissions(now);
            }
            self.emitted_meta.remove(&ws)
        });
        // A record whose parked meta is gone (the table was cleared, or a hook
        // took it) is still a record: its header says what it carries, so it
        // is ledgered as that rather than billed as a content-less control
        // frame. A tile pass names no key in its datagram and cannot be.
        let meta = meta.or_else(|| {
            let ws = wire_seq?;
            let content = Self::content_of_datagram(dg, class)?;
            Some(EmittedDatagram {
                wire_seq: ws,
                content,
                emit_us: self.reliable_emitter.emit_us64(now),
                wire_bytes: dg.len(),
            })
        });
        let sealed = match meta {
            Some(e) => {
                let meta = RecordMeta {
                    wire_seq: e.wire_seq,
                    content: e.content,
                    emit_us: e.emit_us,
                    class,
                    is_pad: matches!(e.content, Content::Pad(_)),
                    is_probe: self.probe_tag_of(&e.content).is_some(),
                    len: dg.len(),
                    // First sends are protected; retransmissions and pads
                    // are PLAIN (plan decision 2).
                    protect: matches!(
                        class,
                        WireClass::Critical | WireClass::Refinement | WireClass::H264
                    ),
                };
                self.packer.push(dg, meta, max_dg, now_us)
            }
            // A control datagram (frame dimensions): a lone PLAIN frame.
            None => self.packer.lone(dg, max_dg, now_us),
        };
        self.queue_bundles(sealed);
        // Repairs of a block this record closed follow its last source.
        self.service_pending_repairs();
        self.drive_pending_bundles();
        SendOutcome::Sent
    }

    /// A pong, framed as a lone PLAIN bundle (never inside a bundle of
    /// records, so a ping's answer is not held behind a frame's tail). The live
    /// session's goes through its packer and the ordinary bundle path; any
    /// other connection's is framed by a throwaway packer and sent
    /// unbilled, as eviction notices to it are.
    pub(super) fn send_pong_framed(&mut self, handle: ConnectionHandle, pong: &[u8]) {
        if self.last_hello_from == Some(handle) && self.framed_sink_present() {
            self.send_counted_framed(pong, WireClass::Control, None);
            return;
        }
        self.send_scratch_lone(handle, pong);
    }

    /// Frame `dg` alone with a packer of its own (sequence 0) and offer it to
    /// `handle`'s connection. Returns whether quinn took it.
    pub(super) fn send_scratch_lone(&mut self, handle: ConnectionHandle, dg: &[u8]) -> bool {
        let (Some(wt), Some(conn)) = (
            self.wt_sessions.get_mut(&handle),
            self.server.connections.get_mut(&handle),
        ) else {
            return false;
        };
        let Some(max_dg) = conn
            .datagrams()
            .max_size()
            .map(|raw| raw.saturating_sub(wt.datagram_framing_len()))
            .filter(|&m| m > 0)
        else {
            return false;
        };
        let mut ok = false;
        for b in Packer::new().lone(dg, max_dg, 0) {
            ok = wt.send_datagram(conn, &b.bytes).is_ok();
        }
        ok
    }

    /// Record each sealed bundle in the ledger and queue it for quinn.
    pub(super) fn queue_bundles(&mut self, sealed: SmallVec<[SealedBundle; 2]>) {
        for b in sealed {
            self.record_bundle(&b);
            self.note_source_sealed(&b);
            self.pending_bundles.push_back(b);
        }
        self.process_closed_blocks(None);
    }

    /// One ledger transmission for the bundle, keyed by `bundle_seq`.
    pub(super) fn record_bundle(&mut self, b: &SealedBundle) {
        let now = now_std();
        let emit_us = if b.records.is_empty() {
            // A lone control frame: stamp it like any transmission.
            self.reliable_emitter.emit_us64(now)
        } else {
            b.emit_us
        };
        self.transmission_ledger.record(
            b.bundle_seq,
            now,
            Transmission {
                emit_us,
                wire_bytes: b.bytes.len(),
                records: b.records.iter().map(|r| (r.wire_seq, r.content)).collect(),
                depart_us: None,
                dep_idx: None,
            },
        );
    }

    /// Seal the open bundle and offer every pending one to quinn. True when
    /// none is left standing.
    pub(super) fn flush_bundles(&mut self) -> bool {
        let now_us = self.reliable_emitter.emit_us64(now_std());
        if let Some(b) = self.packer.flush(now_us) {
            self.queue_bundles(SmallVec::from_elem(b, 1));
        }
        self.drive_pending_bundles()
    }

    /// Offer pending bundles oldest first. False when quinn refused one: it
    /// stays at the head and is retried on the next call.
    pub(super) fn drive_pending_bundles(&mut self) -> bool {
        while let Some(b) = self.pending_bundles.pop_front() {
            if !self.send_one_bundle(&b) {
                self.pending_bundles.push_front(b);
                return false;
            }
        }
        true
    }

    /// True when the bundle is consumed (sent, dropped by the bandwidth cap,
    /// nobody to send to, or too large to ever send).
    fn send_one_bundle(&mut self, b: &SealedBundle) -> bool {
        #[cfg(test)]
        self.sent_frames_for_test.push(b.bytes.clone());
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(cap) = self.outbound_bandwidth_cap.as_mut() {
            if !cap.try_consume(b.bytes.len()) {
                // Consumed, and departed as far as the transport is
                // concerned: later acknowledgements will find the loss.
                self.stamp_departure(b.bundle_seq, now_std());
                return true;
            }
        }
        match self.offer_to_live(&b.bytes) {
            Offer::NoSession => {
                self.transmission_ledger.forget_undeparted(b.bundle_seq);
                true
            }
            Offer::Taken {
                handle,
                cum,
                framing,
                queued,
            } => {
                self.note_bundle_enqueued(handle, b, framing, cum, queued);
                true
            }
            Offer::Refused(quinn_proto::SendDatagramError::TooLarge) => {
                self.note_bundle_refused(b, &quinn_proto::SendDatagramError::TooLarge);
                // The path shrank under a bundle sealed for the old size. It
                // can never go, and it will never be acknowledged or judged,
                // so nothing would ever tell the owners of its content: each
                // record gets what an oversize datagram retired from the
                // emission queue gets (a tile re-encoded, an H.264 frame lost
                // to the manager, padding dropped).
                tracing::warn!(
                    target: LOG_TARGET,
                    bundle_seq = b.bundle_seq,
                    len = b.bytes.len(),
                    "dropping a sealed bundle the path no longer carries"
                );
                let tracked = self.transmission_ledger.forget_undeparted(b.bundle_seq);
                let contents = b.records.iter().map(|r| Some(r.content)).collect();
                self.retire_oversize_contents(contents, tracked, now_std());
                true
            }
            Offer::Refused(e) => {
                self.note_bundle_refused(b, &e);
                false
            }
        }
    }

    /// Count and log quinn's refusal of a bundle. A bundle that stays refused
    /// is one refusal, however often it is offered again: the count is what
    /// quinn's backpressure cost, not how often the loop looked.
    fn note_bundle_refused(&mut self, b: &SealedBundle, e: &quinn_proto::SendDatagramError) {
        if matches!(e, quinn_proto::SendDatagramError::TooLarge) {
            self.h264.stats.send_too_large += 1;
        }
        if self.last_refused_bundle == Some(b.bundle_seq) {
            return;
        }
        self.last_refused_bundle = Some(b.bundle_seq);
        self.datagram_send_errs = self.datagram_send_errs.saturating_add(1);
        if !self.datagram_send_err_first_logged {
            self.datagram_send_err_first_logged = true;
            tracing::warn!(
                target: LOG_TARGET,
                error = ?e,
                payload_len = b.bytes.len(),
                "first send_datagram Err — QUIC backpressure or limit; \
                 subsequent send failures will log at debug and a running \
                 total appears in the per-frame `dispatch returned` log"
            );
        } else {
            tracing::debug!(target: LOG_TARGET, error = ?e, "datagram send failed");
        }
    }

    /// Bill a bundle quinn just accepted. Its records were admitted one by one
    /// at `record_len + 2`; what is left of the frame (header, a lone control
    /// record) and the QUIC framing is charged here, ungated, and billed as
    /// `Control`.
    pub(super) fn note_bundle_enqueued(
        &mut self,
        handle: ConnectionHandle,
        b: &SealedBundle,
        framing: usize,
        cum: u64,
        queued: u64,
    ) {
        let wire = b.bytes.len() + framing;
        let records_wire: usize = b
            .records
            .iter()
            .map(|r| r.len + FEC_RECORD_LEN_PREFIX)
            .sum();
        let control_dg = b.bytes.len().saturating_sub(records_wire);
        let control_wire = control_dg + framing;
        let d_target = self.gate.map(|g| g.d_target as u64);
        let over = d_target.is_some_and(|d| queued.saturating_sub(wire as u64) >= d);
        if !b.records.is_empty() {
            if over {
                self.instr.admission.depth_violations += 1;
            }
            self.instr.admission.datagrams_admitted += b.records.len() as u64;
        } else if over {
            self.instr.admission.ungated_at_depth += 1;
        }
        // A REPAIR frame paid its whole length when it was admitted
        // (`AdmitClass::Critical`), so only the QUIC framing is charged here.
        let repair = b.kind == BundleKind::Repair;
        let soft_charge = if repair { framing } else { control_wire };
        self.admission.charge(soft_charge);
        self.instr.admission.soft_charged_bytes += soft_charge as u64;
        for r in &b.records {
            self.instr
                .offered
                .bill(r.class, r.len + FEC_RECORD_LEN_PREFIX, 0);
        }
        let (class, dg) = if repair {
            (WireClass::Repair, b.bytes.len())
        } else {
            (WireClass::Control, control_dg)
        };
        self.instr.offered.bill(class, dg, framing);
        self.instr.offered.bundles += 1;
        self.instr.fifo_peak_at_enqueue = self.instr.fifo_peak_at_enqueue.max(queued);
        // Short bundles back to back in a non-empty FIFO can share a QUIC
        // packet; `queued` includes this bundle, so the FIFO before it is
        // `queued - wire`.
        let short = self
            .live_max_dg()
            .is_some_and(|max_dg| b.bytes.len() + 64 < max_dg);
        let fifo_before = queued.saturating_sub(wire as u64);
        if short && self.last_enqueued_was_short && fifo_before > 0 {
            self.instr.admission.short_bundle_shared_risk += 1;
        }
        self.last_enqueued_was_short = short;
        self.track_enqueue(handle, Some(b.bundle_seq), cum);
    }
}
