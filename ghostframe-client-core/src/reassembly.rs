//! Full per-datagram reassembly pipeline.
//!
//! Ports the datagram receive loop (`main.ts` ~1280-1421),
//! `handleSourceTileDatagram` (~1099-1276) and `finishAssembly` (~464-751)
//! from `ghostframe-web-client/src/main.ts`. The order of operations here
//! matches that reference exactly; see `.superpowers/sdd/task-11-report.md`
//! for the line-by-line mapping table.
//!
//! Safety: nothing in this path may panic on malformed or hostile input.
//! All header decodes are fallible and short-circuit; all indexing goes
//! through `get`/bounds checks; no `unwrap` touches attacker-controlled data.

use ghostframe_protocol::ack::AckEntry;
use ghostframe_protocol::protocol::{
    decode_tile_datagram, is_tile_datagram, Codec, TileParityEnvelope, DATAGRAM_HEADER_SIZE,
    FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y, TILE_DATAGRAM_FLAG, TILE_HEADER_SIZE,
    TILE_PARITY_ENVELOPE,
};

use crate::cdf53_coverage::apply_cdf53_arrival;
use crate::cdf53_prevalidate::prevalidate_cdf53;
use crate::event::{Event, PollOutput, TileKey};
use crate::pal_rle_decode::decode_pal_rle_tile;
use crate::{Assembly, ClientCore};

/// Minimum bytes that carry a full tile header stack.
const TILE_MIN: usize = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE; // 24

impl ClientCore {
    /// Feed one received datagram. Returns decode/render events.
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: u64) -> Vec<Event> {
        let mut events = Vec::new();

        // 1. Empty -> ignore.
        if bytes.is_empty() {
            return events;
        }

        // TILE_PARITY (0x04) envelope — routed before the ping/pong guard.
        // A tile datagram has bit 31 of the first u32 set (first byte
        // >= 0x80), so 0x04 can never collide with one, but we still guard.
        if bytes[0] == TILE_PARITY_ENVELOPE && !is_tile_datagram(bytes) {
            if let Ok(env) = TileParityEnvelope::decode(bytes) {
                if let Some(recovered) = self.parity_decoder.receive_parity(&env) {
                    self.handle_source_tile(&recovered, now_us, &mut events);
                }
            }
            return events;
        }

        // 2. Ping/pong (< 20 bytes) -> ignore; too-short tile (< 24) -> drop.
        if bytes.len() < 20 {
            return events;
        }
        if bytes.len() < TILE_MIN {
            return events;
        }

        // 3. Not a tile datagram -> H.264 full-frame reassembly path.
        if !is_tile_datagram(bytes) {
            self.handle_frame_datagram(bytes, now_us, &mut events);
            return events;
        }

        // 4. Tile datagram: feed the parity window (may replay a recovered
        //    source), then process this datagram.
        let wire_seq = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if let Some(replayed) = self.parity_decoder.record_source(wire_seq, bytes) {
            self.handle_source_tile(&replayed, now_us, &mut events);
        }
        self.handle_source_tile(bytes, now_us, &mut events);

        events
    }

    /// Core per-source-tile pipeline (`handleSourceTileDatagram`).
    fn handle_source_tile(&mut self, bytes: &[u8], now_us: u64, events: &mut Vec<Event>) {
        if bytes.len() < TILE_MIN {
            return;
        }
        let (dh, th, payload) = match decode_tile_datagram(bytes) {
            Ok(x) => x,
            Err(_) => return,
        };
        let frame_seq = dh.frame_seq & !TILE_DATAGRAM_FLAG;

        // loss_tracker.onDatagram (main.ts:1105).
        self.loss_tracker.on_datagram(now_us);

        let is_sentinel =
            th.tile_x == FRAME_DIMENSIONS_SENTINEL_X && th.tile_y == FRAME_DIMENSIONS_SENTINEL_Y;

        // ACK on receipt unless sentinel or Cdf53 (main.ts:1118-1145).
        if !is_sentinel && th.codec != Codec::Cdf53 {
            let entry = AckEntry {
                frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
                tile_x: th.tile_x,
                tile_y: th.tile_y,
                pass_idx: th.pass,
                arrival_time_ms_lo16: ((now_us / 1000) & 0xFFFF) as u16,
            };
            if let Some(dg) = self.ack_batcher.add(entry, now_us) {
                self.outbox.push_back(PollOutput::Datagram(dg));
            }
        }

        // Advance latest_frame_seq (main.ts:1156-1158).
        if frame_seq > self.latest_frame_seq {
            self.latest_frame_seq = frame_seq;
        }

        // Evict stale assemblies: frame_seq < latest_frame_seq - 2
        // (saturating to avoid underflow when latest < 2). main.ts:1160-1177.
        let threshold = self.latest_frame_seq.saturating_sub(2);
        let stale: Vec<TileKey> = self
            .assemblies
            .keys()
            .filter(|k| k.frame_seq < threshold)
            .copied()
            .collect();
        for k in stale {
            if let Some(asm) = self.assemblies.remove(&k) {
                if asm.received < asm.fragments.len() {
                    self.loss_tracker
                        .on_stale_tile(asm.fragments.len(), asm.received);
                }
            }
            self.fragment_parity.remove(&k);
        }

        let key = TileKey {
            frame_seq,
            tile_x: th.tile_x,
            tile_y: th.tile_y,
            pass_idx: th.pass,
        };

        // Legacy fragment-level parity (frag_idx >= frag_total). main.ts:1181-1209.
        if dh.frag_idx >= dh.frag_total {
            self.fragment_parity.store(key, payload);
            let want_recover = self
                .assemblies
                .get(&key)
                .map(|asm| !asm.fragments.is_empty() && asm.received + 1 == asm.fragments.len())
                .unwrap_or(false);
            if want_recover {
                let frags = &self.assemblies[&key].fragments;
                if let Some((idx, recovered)) = self.fragment_parity.try_recover(key, frags) {
                    if let Some(asm) = self.assemblies.get_mut(&key) {
                        if asm.fragments.get(idx).map(|f| f.is_none()).unwrap_or(false) {
                            asm.fragments[idx] = Some(recovered);
                            asm.received += 1;
                            self.loss_tracker.on_fec_recovery();
                            self.finish_assembly(key, now_us, events);
                        }
                    }
                }
            }
            return;
        }

        // Frame-dimensions sentinel (0xFF, 0xFF) with >= 8-byte payload.
        // main.ts:1211-1222. Sentinels are always single-fragment (Skip
        // codec, 8-byte payload), so they are handled inline here and never
        // reach the assembly/finish path.
        if is_sentinel {
            if payload.len() >= 8 {
                let width = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let height = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                events.push(Event::FrameDimensions { width, height });
            }
            return;
        }

        // Codec::Skip -> return with no side effects beyond ACK/eviction.
        // main.ts:1224-1226.
        if th.codec == Codec::Skip {
            return;
        }

        // Insert fragment (ignore duplicates). main.ts:1232-1255.
        let asm = self
            .assemblies
            .entry(key)
            .or_insert_with(|| Assembly::new(&th, frame_seq, dh.frag_total, now_us));
        let fi = dh.frag_idx as usize;
        if fi < asm.fragments.len() && asm.fragments[fi].is_none() {
            asm.fragments[fi] = Some(payload.to_vec());
            asm.received += 1;
        }
        let received = asm.received;
        let total = asm.fragments.len();

        // received == total - 1 -> attempt fragment-parity recovery.
        // main.ts:1257-1271.
        if total > 0 && received + 1 == total {
            let frags = &self.assemblies[&key].fragments;
            if let Some((idx, recovered)) = self.fragment_parity.try_recover(key, frags) {
                if let Some(asm) = self.assemblies.get_mut(&key) {
                    if asm.fragments.get(idx).map(|f| f.is_none()).unwrap_or(false) {
                        asm.fragments[idx] = Some(recovered);
                        asm.received += 1;
                        self.loss_tracker.on_fec_recovery();
                    }
                }
            }
        }

        // received == total -> finish. main.ts:1273-1275.
        let complete = self
            .assemblies
            .get(&key)
            .map(|a| !a.fragments.is_empty() && a.received == a.fragments.len())
            .unwrap_or(false);
        if complete {
            self.finish_assembly(key, now_us, events);
        }
    }

    /// Concatenate a completed assembly's fragments and dispatch by codec
    /// (`finishAssembly`).
    fn finish_assembly(&mut self, key: TileKey, now_us: u64, events: &mut Vec<Event>) {
        let asm = match self.assemblies.remove(&key) {
            Some(a) => a,
            None => return,
        };
        self.fragment_parity.remove(&key);

        let mut payload = Vec::new();
        for b in asm.fragments.iter().flatten() {
            payload.extend_from_slice(b);
        }

        let tx = asm.tile_x;
        let ty = asm.tile_y;
        let frame_seq = asm.frame_seq;

        // Sentinel guard (main.ts:497). In practice sentinels never reach
        // here (single-fragment, handled inline), but mirror the reference.
        if tx == FRAME_DIMENSIONS_SENTINEL_X && ty == FRAME_DIMENSIONS_SENTINEL_Y {
            if payload.len() >= 8 {
                let width = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
                let height = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
                events.push(Event::FrameDimensions { width, height });
            }
            return;
        }

        match asm.codec {
            Codec::Raw => {
                // Payload is BGRA; swizzle to RGBA (preserving alpha).
                let px = payload.len() / 4;
                let mut rgba = vec![0u8; px * 4];
                for i in 0..px {
                    let o = i * 4;
                    rgba[o] = payload[o + 2]; // R
                    rgba[o + 1] = payload[o + 1]; // G
                    rgba[o + 2] = payload[o]; // B
                    rgba[o + 3] = payload[o + 3]; // A
                }
                events.push(Event::TileReady {
                    frame_seq,
                    tile_x: tx,
                    tile_y: ty,
                    rgba,
                });
            }
            Codec::Solid => {
                // main.ts only paints Solid when the payload is exactly 4B.
                if payload.len() == 4 {
                    let (b, g, r, a) = (payload[0], payload[1], payload[2], payload[3]);
                    let _ = a; // alpha forced to 255 (mirrors GPU solid expand)
                    let mut rgba = vec![0u8; 4096];
                    for px in 0..1024 {
                        let o = px * 4;
                        rgba[o] = r;
                        rgba[o + 1] = g;
                        rgba[o + 2] = b;
                        rgba[o + 3] = 255;
                    }
                    events.push(Event::TileReady {
                        frame_seq,
                        tile_x: tx,
                        tile_y: ty,
                        rgba,
                    });
                }
            }
            Codec::PalRle => {
                match decode_pal_rle_tile(&payload, &mut self.palette_shadow, &mut self.palettes) {
                    Ok(rgba) => events.push(Event::TileReady {
                        frame_seq,
                        tile_x: tx,
                        tile_y: ty,
                        rgba,
                    }),
                    Err(code) => {
                        if let Some(msg) =
                            self.decode_error_batcher
                                .report(Codec::PalRle, tx, ty, code, now_us)
                        {
                            self.outbox.push_back(PollOutput::Stream(msg));
                        }
                        events.push(Event::DecodeError {
                            codec: Codec::PalRle,
                            tile_x: tx,
                            tile_y: ty,
                            code,
                        });
                    }
                }
            }
            Codec::Cdf53 => {
                let result = prevalidate_cdf53(&payload, asm.generation, asm.pass);
                let ok = result.is_ok();

                // Coverage bookkeeping + NACK decision (shared success/fail).
                let prev = self.cdf53_coverage.get(&(tx, ty)).copied();
                let outcome =
                    apply_cdf53_arrival(prev, asm.generation, asm.pass, frame_seq, now_us, ok);
                self.cdf53_coverage.insert((tx, ty), outcome.entry);
                for p in outcome.nack_passes {
                    // Coverage NACKs are routed through the debounced
                    // pending-NACK queue (main.ts `queuePassNack`,
                    // ~618-629), not straight to the NACK batcher: the
                    // debounce flush re-checks live coverage right before
                    // sending, in case the pass arrived validly in the
                    // meantime. Dispatch itself carries frag_idx = 0
                    // (main.ts:830-838).
                    self.queue_pass_nack(frame_seq, tx, ty, p, now_us);
                }

                match result {
                    Ok(pre) => {
                        let rgba = self.cdf53_tile_state.integrate(tx, ty, &pre);
                        events.push(Event::TileReady {
                            frame_seq,
                            tile_x: tx,
                            tile_y: ty,
                            rgba,
                        });
                        // Deferred ACK — fires only after prevalidation success.
                        let entry = AckEntry {
                            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
                            tile_x: tx,
                            tile_y: ty,
                            pass_idx: asm.pass,
                            arrival_time_ms_lo16: ((now_us / 1000) & 0xFFFF) as u16,
                        };
                        if let Some(dg) = self.ack_batcher.add(entry, now_us) {
                            self.outbox.push_back(PollOutput::Datagram(dg));
                        }
                    }
                    Err(code) => {
                        if let Some(msg) =
                            self.decode_error_batcher
                                .report(Codec::Cdf53, tx, ty, code, now_us)
                        {
                            self.outbox.push_back(PollOutput::Stream(msg));
                        }
                        events.push(Event::DecodeError {
                            codec: Codec::Cdf53,
                            tile_x: tx,
                            tile_y: ty,
                            code,
                        });
                    }
                }
            }
            // H264 tile codec + Skip: no per-tile RGBA path here.
            Codec::H264 | Codec::Skip => {}
        }
    }
}
