//! Inbound datagram routing and the receive-side handling of ACKs and NACKs.
//!
//! Owns `dispatch_inbound_datagram` (the HELLO gate and the discriminator
//! match that sends a datagram to its handler, including the
//! `non_live_datagram_drops` and `unroutable_inbound_datagrams` counters),
//! `dispatch_ack_datagram` (decoding an ACK batch and applying it to the
//! retransmit cache, transmission ledger, fragment coverage, palette table and
//! the `bwe_samples_buffer`), `dispatch_tile_nack_datagram` (tile NACKs into
//! the reliable emitter) and the `handle_datagram_payload` ping reply. The
//! fields themselves stay on `IoBridge`.
//!
//! Must not: run the per-tick pacing or scheduler logic, or interpret
//! FEEDBACK-stream messages (`dispatch_feedback_bytes` stays in `io_bridge.rs`);
//! it records what the peer acknowledged and hands the samples on.

use super::*;

/// Dispatch a raw WebTransport datagram payload.
///
/// Returns `Some(response)` when a reply should be sent, or `None` for
/// unknown payloads (which the caller logs at trace level).
pub(crate) fn handle_datagram_payload(payload: &[u8]) -> Option<&'static [u8]> {
    if payload == PING_PAYLOAD {
        Some(PONG_PAYLOAD)
    } else {
        None
    }
}

/// One resolved ACK entry: what the transmission carried, the transmission,
/// the client arrival stamp, and whether it was already reported lost (so no
/// timing sample).
type ResolvedAck = (
    crate::transport::reliable_emitter::Content,
    crate::transport::transmission_ledger::Transmission,
    u16,
    bool,
);

impl IoBridge {
    /// Route a single inbound datagram into the appropriate handler.
    /// Currently dispatches ACK_BATCH_MSG_TYPE; future M3.x message types
    /// can be added here. Unknown discriminators are silently dropped
    /// (forward-compatible).
    pub(crate) fn dispatch_ack_datagram(&mut self, data: &[u8]) {
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(inj) = self.inbound_loss.as_mut() {
            if inj.should_drop(data) {
                return;
            }
        }
        if data.is_empty() {
            return;
        }
        tracing::debug!(
            target: "ghostframe::cdf53",
            first_byte = data[0],
            len = data.len(),
            "ack_datagram_received"
        );
        if data[0] == crate::transport::ack::ACK_BATCH_MSG_TYPE {
            if let Ok(batch) = crate::transport::ack::AckBatch::decode(data) {
                tracing::trace!(
                    target: "ghostframe::cdf53",
                    entry_count = batch.entries.len(),
                    "ack_batch_received"
                );
                // Route every ACKed key into the reliable-tile emitter as
                // well — it owns the retransmit cache and needs to drop the
                // matching CacheEntry so nothing sends the pass again. Runs alongside the scheduler/coverage path below,
                // which is not legacy: the coverage record outlives the
                // cache entry in several cases, and `apply_ack_coverage`
                // reads it for exactly those late ACKs (see the
                // `fragment_coverage` module docs).
                //
                // Phase 1 Task 5: we ALSO extract OWD samples here, before
                // on_ack removes the cache entries. The cache lookup below
                // recovers the server's emit time from `CacheEntry::last_sent_at`
                // (monotonic µs since `self.bwe_epoch`) and pairs it with the
                // client's echoed arrival_time_ms_lo16 (u16 wall-clock-ms
                // wrapped). Reading `last_sent_at` rather than the stamp
                // baked into the cached datagram bytes matters on retransmit:
                // `ReliableEmitter::tick` re-stamps the *copy* it re-sends but
                // updates `last_sent_at` on the cache entry itself, so this is
                // the only place that reflects the actual on-wire moment for a
                // retransmitted pass. The BWE consumer uses relative deltas
                // only, so clock skew is acceptable. Cache misses are silent
                // (already ACKed by an overlapping batch, or retired).
                let now_for_samples = now_std();
                // Entries name transmissions now, so the ledger supplies both
                // halves: the content identity every delivery consumer below
                // expects, and the exact send time of *that* transmission —
                // no longer inferred from a cache entry that a retransmission
                // may have re-stamped since.
                let resolved = self.resolve_ack_entries(&batch, now_for_samples);
                // Both Live and Late resolutions release their content --
                // that release is the entire point of this phase, since
                // nothing else ever clears a cache entry acknowledged after
                // its transmission was declared lost.
                let emit_keys: Vec<crate::transport::reliable_emitter::EmitKey> = resolved
                    .iter()
                    .filter_map(|(c, _, _, _)| c.tile())
                    .collect();
                // Samples come from the ledger record, not the cache entry.
                // `Transmission::emit_us` is the stamp *this* datagram
                // carried, so a retransmission no longer borrows the send time
                // of whichever transmission the cache happened to hold last —
                // the ambiguity that made a Karn-style filter necessary.
                //
                // `queued_at` and the probe tag still come from the cache,
                // which is content-scoped and may legitimately be gone if the
                // tile was superseded. Absent, the sample is still recorded:
                // its timing is what the estimator came for.
                // [RTO-PROBE] temporary instrumentation: emit -> acknowledgement
                // latency, measured on the same clock the probe timeout uses.
                self.log_ack_probes(&batch, &resolved, now_for_samples);
                self.record_ack_bwe_samples(&resolved, now_for_samples);
                self.reliable_emitter.on_ack(&emit_keys);
                self.apply_h264_acks(&resolved, now_for_samples);
                self.apply_ack_coverage(&resolved);
            } else {
                // The first byte already said `ACK_BATCH_MSG_TYPE` so a
                // decode failure here is a real protocol-level disagreement
                // (count out of range, truncated payload, etc.) rather than
                // a stray new message type. Surface it loudly — the
                // overlap-batch-rejection bug (commit message of this fix)
                // hid for weeks because the legacy code silently dropped
                // these, so every overlap-bearing batch was discarded
                // without any signal that ACK throughput had collapsed.
                tracing::warn!(
                    target: "ghostframe::cdf53",
                    first_byte = data[0],
                    len = data.len(),
                    "ack_batch_decode_failed"
                );
            }
        } else {
            // Unknown first byte — possibly a future protocol message
            // type. Forward-compat: log at debug so it's discoverable
            // under verbose tracing but doesn't spam normal operation.
            tracing::debug!(
                target: "ghostframe::cdf53",
                first_byte = data[0],
                len = data.len(),
                "ack_datagram_unknown_first_byte"
            );
        }
    }

    /// Resolve each ACK entry against the transmission ledger, updating the
    /// loss window. Entries that resolve to nothing are dropped.
    fn resolve_ack_entries(
        &mut self,
        batch: &crate::transport::ack::AckBatch,
        now_for_samples: std::time::Instant,
    ) -> Vec<ResolvedAck> {
        let min_rtt = self
            .reliable_emitter
            .rtt
            .min_rtt()
            .unwrap_or(std::time::Duration::ZERO);
        let resolved: Vec<ResolvedAck> = batch
            .entries
            .iter()
            .filter_map(|e| {
                use crate::transport::transmission_ledger::Resolution;
                // Read before `resolve_at` removes the record or tombstone.
                let rate = self.transmission_ledger.rate_mark(e.wire_seq);
                let r = match self
                    .transmission_ledger
                    .resolve_at(e.wire_seq, Some((now_for_samples, min_rtt)))
                {
                    Some(Resolution::Live(tx)) => {
                        // At dispatch, not when the samples are drained later
                        // in the turn: the probe timeout runs before the
                        // drain and must see this acknowledgement.
                        self.reliable_emitter.note_ack(now_for_samples);
                        if let Some((mark, departed)) = rate {
                            self.delivery_sampler.on_acked(
                                &mark,
                                departed,
                                tx.wire_bytes,
                                now_for_samples,
                            );
                        }
                        if let Some(k) = tx.content.tile() {
                            self.reliable_emitter.note_ack_origin(k, e.wire_seq);
                        }
                        self.loss_window.record(tx.emit_us as u32, false);
                        Some((tx.content, tx, e.arrival_time_ms_lo16, false))
                    }
                    Some(Resolution::Late { tx, fec_possible }) => {
                        self.reliable_emitter.note_ack(now_for_samples);
                        if let Some(k) = tx.content.tile() {
                            self.reliable_emitter.note_ack_origin(k, e.wire_seq);
                        }
                        let was_flushed = self.instr.flushed.take(e.wire_seq);
                        if fec_possible {
                            // May be the client acking a datagram it rebuilt
                            // from parity: a loss that happened. Not
                            // retracted, not counted as one goog_cc was wrongly
                            // told of.
                            self.instr.late_acks_fec_possible += 1;
                        } else {
                            // The network did deliver it, only late: its
                            // bytes count as delivered for the rate sample.
                            // (Not when it may be an FEC recovery: then the
                            // network did not.)
                            if let Some((mark, departed)) = rate {
                                self.delivery_sampler.on_acked(
                                    &mark,
                                    departed,
                                    tx.wire_bytes,
                                    now_for_samples,
                                );
                            }
                            self.loss_window.retract(tx.emit_us as u32);
                            // Observability: was goog_cc already told this was
                            // lost?
                            if was_flushed {
                                self.instr.late_acked_after_flush += 1;
                            }
                            // Withdrawn from `pending_losses` once per turn
                            // (see `retract_late_acked_losses`); only worth
                            // noting when something is waiting there.
                            if !self.pending_losses.is_empty() {
                                self.late_acked.insert(e.wire_seq);
                            }
                        }
                        if crate::transport::reliable_emitter::rto_probe_enabled() {
                            eprintln!(
                                "RTOPROBE late_ack ws={} age_us={}",
                                e.wire_seq,
                                self.reliable_emitter
                                    .emit_us(now_for_samples)
                                    .wrapping_sub(tx.emit_us as u32)
                            );
                        }
                        Some((tx.content, tx, e.arrival_time_ms_lo16, true))
                    }
                    None => None,
                };
                if r.is_none() && crate::transport::reliable_emitter::rto_probe_enabled() {
                    eprintln!("RTOPROBE resolve_miss ws={}", e.wire_seq);
                }
                r
            })
            .collect();
        // One rate sample per acknowledgement batch, as the draft has it.
        self.delivery_sampler.end_ack(now_for_samples);
        resolved
    }

    /// Env-gated ACK instrumentation (ACKORDER and RTOPROBE).
    fn log_ack_probes(
        &self,
        batch: &crate::transport::ack::AckBatch,
        resolved: &[ResolvedAck],
        now_for_samples: std::time::Instant,
    ) {
        if crate::transport::reliable_emitter::ack_order_probe_enabled() {
            // One line per batch, entries in arrival order. Kept
            // compact: a real session produces thousands. Gated
            // independently of the RTO probe -- these answer different
            // questions and are enabled separately.
            let seqs: Vec<String> = batch
                .entries
                .iter()
                .map(|e| e.wire_seq.to_string())
                .collect();
            tracing::info!(
                target: "ghostframe::ackorder",
                "ACKORDER {}",
                seqs.join(",")
            );
        }
        if crate::transport::reliable_emitter::rto_probe_enabled() {
            let ack_us = self.reliable_emitter.emit_us(now_for_samples) as i64;
            for (c, tx, _, _) in resolved.iter() {
                let Some(k) = c.tile() else { continue };
                let codec = self
                    .reliable_emitter
                    .cache
                    .get(&k)
                    .and_then(|e| e.datagram.get(18).copied())
                    .map(|b| b >> 1)
                    .unwrap_or(255);
                eprintln!(
                    "RTOPROBE ack codec={} pass={} latency_us={}",
                    codec,
                    k.pass_idx,
                    ack_us.wrapping_sub(tx.emit_us as u32 as i64) as i32 as i64
                );
            }
        }
    }

    /// Turn resolved ACKs into BWE samples. Must run before `on_ack`, which
    /// removes the cache entries the `queued_at` and probe reads need.
    fn record_ack_bwe_samples(
        &mut self,
        resolved: &[ResolvedAck],
        now_for_samples: std::time::Instant,
    ) {
        for (content, tx, arrival_lo16, late) in resolved.iter() {
            if *late {
                // Declared lost when it expired: either already reported to
                // the estimator (feeding its timing now would report the same
                // bytes twice) or withdrawn from `pending_losses` this turn
                // (see `retract_late_acked_losses`). Its arrival time reflects
                // the stall rather than the path, so it is not fed as delivered.
                // It is still an acknowledgement (the PTO backoff ended at
                // dispatch, in `resolve_ack_entries`), but its round trip is
                // not a sample: it includes the time the verdict beat it by,
                // and a recovered datagram's reflects the parity, not the
                // path. srtt, rttvar and the latest round trip stay as the
                // live acknowledgements made them.
                continue;
            }
            if self.bwe_samples_buffer.len() >= BWE_SAMPLES_BUFFER_CAPACITY {
                break;
            }
            let tile_key = content.tile();
            // `queued_at` and the probe tag come from whichever cache holds the
            // content: the tile cache, or the H.264 probe cache.
            let cache_entry = match content {
                crate::transport::reliable_emitter::Content::Tile(k) => {
                    self.reliable_emitter.cache.get(k)
                }
                crate::transport::reliable_emitter::Content::H264Frag {
                    frame_seq,
                    frag_idx,
                } => self.reliable_emitter.h264.get(*frame_seq, *frag_idx),
            };
            let server_emit_us = tx.emit_us;
            let queued_since_epoch_us = cache_entry
                .map(|entry| {
                    entry
                        .queued_at
                        .saturating_duration_since(self.bwe_epoch)
                        .as_micros() as u64
                })
                .unwrap_or(server_emit_us);
            // A frame fragment is first-send, latency-critical traffic.
            let tier = tile_key.map_or(PassTier::Critical, |k| pass_tier(k.pass_idx));
            let emit_lo16 = ((server_emit_us / 1000) & 0xFFFF) as u16;
            let owd_ms_lo16 = arrival_lo16.wrapping_sub(emit_lo16);
            let probe = cache_entry.and_then(|entry| entry.probe);
            self.bwe_samples_buffer.push(BweSample {
                tier,
                server_emit_us,
                depart_us: tx.depart_us.unwrap_or(server_emit_us),
                queued_since_epoch_us,
                client_arrival_ms_lo16: *arrival_lo16,
                owd_ms_lo16,
                size_bytes: tx.wire_bytes as u32,
                probe,
                received_at: now_for_samples,
            });
        }
    }

    /// Release fragment coverage, scheduler and palette state for each
    /// acknowledged tile-pass.
    fn apply_ack_coverage(&mut self, resolved: &[ResolvedAck]) {
        for (content, _, _, _) in resolved.iter() {
            let Some(emit_key) = content.tile() else {
                continue;
            };
            // Key matches how coverage is recorded at emit time:
            // (frame_seq, tile_x, tile_y, pass_idx), recovered from
            // the ledger since the entry named a transmission.
            let e = emit_key;
            let key = (e.frame_seq, e.tile_x, e.tile_y, e.pass_idx);
            let coverage = match self.fragment_coverage.take(key) {
                Some(c) => c,
                None => {
                    tracing::debug!(
                        target: "ghostframe::cdf53",
                        frame_seq = e.frame_seq,
                        tile_x = e.tile_x,
                        tile_y = e.tile_y,
                        pass_idx = e.pass_idx,
                        "ack_miss"
                    );
                    continue;
                }
            };
            for entry in coverage {
                // Mark the matching InFlight entry in
                // `priority_queue`/`refinement_queue` as Acked so
                // `drain_priority_queue` stops retrying it every
                // 2×RTT. The M3.3d `Scheduler::on_ack` deletion
                // (commit de37a83) dropped this transition; without
                // it Solid/Raw/PalRle work stays InFlight
                // forever and the entire grid re-emits every frame.
                self.scheduler.mark_acked(
                    entry.tile_x,
                    entry.tile_y,
                    entry.generation,
                    entry.pass_idx,
                );
                match entry.codec {
                    crate::transport::protocol::Codec::Cdf53 => {
                        tracing::debug!(
                            target: "ghostframe::cdf53",
                            tile_x = entry.tile_x,
                            tile_y = entry.tile_y,
                            gen = entry.generation,
                            pass_idx = entry.pass_idx,
                            "ack_cdf53_pass"
                        );
                        self.scheduler.record_cdf53_ack(
                            entry.tile_x,
                            entry.tile_y,
                            entry.generation,
                            entry.pass_idx,
                        );
                    }
                    crate::transport::protocol::Codec::PalRle => {
                        // Move the previously-on-ack PalRle palette
                        // bookkeeping here. Mirrors what
                        // Scheduler::on_ack used to do via the
                        // ResolvedTileWork.palette_id channel.
                        if let Some(pid) = entry.palette_id {
                            let pid_usize = pid as usize;
                            // Delivery is about the palette, so it is
                            // recorded for thin emissions too: a thin
                            // one arriving proves the receiver already
                            // had the palette.
                            if !self.palette_table.delivered.contains(pid) {
                                self.palette_table.delivered.insert(pid);
                            }
                            // The counter and the acquire are about
                            // this *emission* carrying a palette, and
                            // only a bundled emission does. Both are
                            // taken on the bundled path at encode time
                            // (`in_flight_carrying += 1` next to
                            // `acquire_or_allocate`), so undoing them
                            // for a thin emission takes down a count
                            // that was never put up and releases an
                            // acquire that never happened.
                            if entry.palette_bundled {
                                let cnt = self.palette_table.in_flight_carrying[pid_usize];
                                self.palette_table.in_flight_carrying[pid_usize] =
                                    cnt.saturating_sub(1);
                                if cnt == 0 {
                                    tracing::warn!(
                                        target: "palrle.alloc",
                                        palette_id = pid,
                                        "in_flight_carrying underflow — enqueue/ack pairing bug",
                                    );
                                }
                                if self.palette_table.ref_count[pid_usize] > 0 {
                                    self.palette_table.release(pid);
                                }
                            }
                        }
                    }
                    // mark_acked above is the only scheduler-state
                    // update these need. Listed rather than caught by
                    // `_`: this is the per-codec ACK-side state hook,
                    // and a new codec needing release/refcount work
                    // here would otherwise get none, silently -- the
                    // shape of the PalRle palette-leak bug.
                    //
                    // `Codec::H264` here is a tile-pass codec value, which no
                    // tile pass carries. H.264 frame fragments never reach
                    // this function: `apply_ack_coverage` skips them, and
                    // `apply_h264_acks` releases their probe-cache entries and
                    // feeds the frame tracker.
                    Codec::Solid | Codec::H264 | Codec::Skip => {}
                }
            }
        }
    }

    /// Route an inbound TILE_NACK envelope (discriminator `0x05`) into the
    /// reliable-tile emitter's `on_nack`. The wire-level NACK entry carries
    /// `(frame_seq, tile_x, tile_y, pass_idx)`, which is the `EmitKey` of the
    /// cached datagram for one tile-pass (a pass is exactly one datagram). Malformed envelopes are silently dropped
    /// (forward-compatible, mirrors `dispatch_ack_datagram`).
    pub(crate) fn dispatch_tile_nack_datagram(&mut self, data: &[u8]) {
        #[cfg(any(test, feature = "test-loss-injection"))]
        if let Some(inj) = self.inbound_loss.as_mut() {
            if inj.should_drop(data) {
                return;
            }
        }
        #[cfg(any(test, feature = "test-loss-injection"))]
        if self.ignore_tile_nacks {
            return;
        }
        use crate::transport::protocol::TileNackEnvelope;
        let Ok(env) = TileNackEnvelope::decode(data) else {
            return;
        };
        let entries: Vec<crate::transport::reliable_emitter::EmitKey> = env
            .entries
            .into_iter()
            .map(|e| {
                crate::transport::reliable_emitter::EmitKey::new(
                    e.frame_seq,
                    e.tile_x,
                    e.tile_y,
                    e.pass_idx,
                )
            })
            .collect();
        // P5c: split the misses by whether the pass is still waiting in the
        // scheduler (not yet sent: the client asked early) or is simply not
        // owed any more (acknowledged, superseded, retired).
        for k in &entries {
            if !self
                .reliable_emitter
                .holds_pass(k.tile_x, k.tile_y, k.pass_idx)
            {
                if self
                    .scheduler
                    .refinement_queue_holds_tile(k.tile_x, k.tile_y)
                {
                    self.instr.nack_miss_pending += 1;
                } else {
                    self.instr.nack_miss_not_owed += 1;
                }
            }
        }
        self.reliable_emitter.on_nack(&entries, now_std());
    }

    /// Handle an H.264 frame acknowledgement (datagram 0x0A). Phase A only
    /// counts it: nothing consumes frame acks until the H.264 ledger lands, so
    /// a malformed one is dropped and an accepted one is logged at debug.
    fn dispatch_frame_ack_datagram(&mut self, dg: &[u8]) {
        match crate::transport::protocol::FrameAck::decode(dg) {
            Ok(ack) => {
                self.frame_acks_received = self.frame_acks_received.saturating_add(1);
                tracing::debug!(
                    target: LOG_TARGET,
                    newest_seq = ack.newest_seq,
                    decoded_mask = format_args!("{:#018x}", ack.decoded_mask),
                    refused_mask = format_args!("{:#018x}", ack.refused_mask),
                    "frame ack received (ignored until the H.264 ledger lands)"
                );
            }
            Err(e) => {
                self.frame_acks_malformed = self.frame_acks_malformed.saturating_add(1);
                tracing::debug!(target: LOG_TARGET, error = %e, "malformed frame ack; dropped");
            }
        }
    }

    /// Route one inbound (non-ping) datagram from `from`.
    ///
    /// Only the HELLO'd session's datagrams are honoured: an ACK or NACK
    /// from anyone else would let an unauthenticated peer release the
    /// retransmit cache or trigger retransmissions. Everything from a
    /// non-live sender is dropped and counted.
    pub(crate) fn dispatch_inbound_datagram(&mut self, from: ConnectionHandle, dg: &[u8]) {
        if !self.is_live(from) {
            self.non_live_datagram_drops = self.non_live_datagram_drops.saturating_add(1);
            if self.non_live_datagram_drops == 1 || self.non_live_datagram_drops.is_multiple_of(64)
            {
                tracing::warn!(
                    target: LOG_TARGET,
                    ?from,
                    total = self.non_live_datagram_drops,
                    "datagram from a session that has not sent HELLO; dropped (rate-limited)"
                );
            }
            return;
        }
        // Route by discriminator. TileNack (`TILE_NACK_ENVELOPE`) routes into
        // the reliable-tile emitter; ACK batches go to `dispatch_ack_datagram`.
        // Other kinds (Hello, frame/tile fragments) are not dispatched on the
        // inbound server path.
        //
        // The kinds are listed rather than caught by `_ =>`: a catch-all
        // once fed every non-NACK kind into the ACK parser and hid a stale
        // `classify_inbound` mapping for months. Listing them forces a
        // decision about where a new inbound type goes.
        use crate::transport::protocol::InboundKind as K;
        match crate::transport::protocol::classify_inbound(dg) {
            K::TileNack => self.dispatch_tile_nack_datagram(dg),
            K::AckBatch => self.dispatch_ack_datagram(dg),
            K::FrameAck => self.dispatch_frame_ack_datagram(dg),
            // Superseded ACK revisions still reach the ACK decoder, which
            // rejects them by design -- that rejection is the intended
            // handling, not a fallthrough.
            K::AckBatchV1 => self.dispatch_ack_datagram(dg),
            K::Empty
            | K::Unknown
            | K::Hello
            | K::TileParity
            | K::FrameFragment
            | K::TileFragment => {
                self.unroutable_inbound_datagrams =
                    self.unroutable_inbound_datagrams.saturating_add(1);
            }
        }
    }
}
