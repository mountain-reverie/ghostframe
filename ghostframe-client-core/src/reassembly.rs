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
    decode_tile_datagram, is_tile_datagram, Codec, TileHeader, TileParityEnvelope,
    DATAGRAM_HEADER_SIZE, FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y,
    TILE_DATAGRAM_FLAG, TILE_HEADER_SIZE, TILE_PARITY_ENVELOPE,
};

use crate::cdf53_coverage::apply_cdf53_arrival;
use crate::cdf53_prevalidate::{prevalidate_cdf53, validate_cdf53};
use crate::event::{Event, PollOutput, TileData};
use crate::pal_rle_decode::prevalidate_pal_rle;
use crate::{ClientCore, MAX_FRAME_DIMENSION};
use ghostframe_protocol::tile::TILE_SIZE;

/// A CDF 5/3 pass that passed validation, in the form it will be emitted.
enum Cdf53Pass {
    Planes {
        bit_planes: Vec<u8>,
        present_passes: Option<u16>,
    },
    Wire {
        streams: Vec<u8>,
        present_passes: Option<u16>,
    },
}

impl Cdf53Pass {
    fn present_passes(&self) -> Option<u16> {
        match self {
            Cdf53Pass::Planes { present_passes, .. } | Cdf53Pass::Wire { present_passes, .. } => {
                *present_passes
            }
        }
    }
}

/// Minimum bytes that carry a full tile header stack.
const TILE_MIN: usize = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE; // 24

/// How a source tile datagram came to be in hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Provenance {
    /// It came off the wire, just now.
    Received,
    /// It was reconstructed from FEC parity and the rest of its group.
    Rebuilt,
}

impl ClientCore {
    /// Whether tile `(tile_x, tile_y)` lies inside the grid of the last
    /// accepted frame size. With no size yet there is no grid to check
    /// against, so the tile is let through (the coordinate space is still
    /// bounded: `u8`, minus the two sentinel values).
    pub(crate) fn tile_in_grid(&self, tile_x: u8, tile_y: u8) -> bool {
        match self.frame_dims {
            Some((w, h)) => {
                (tile_x as u32) < w.div_ceil(TILE_SIZE) && (tile_y as u32) < h.div_ceil(TILE_SIZE)
            }
            None => true,
        }
    }

    /// Drop everything keyed to tiles that a new grid no longer contains.
    fn purge_outside_grid(&mut self) {
        let dims = self.frame_dims;
        let inside = |x: u8, y: u8| match dims {
            Some((w, h)) => {
                (x as u32) < w.div_ceil(TILE_SIZE) && (y as u32) < h.div_ceil(TILE_SIZE)
            }
            None => true,
        };
        self.cdf53_coverage.retain(|&(x, y), _| inside(x, y));
        self.pending_nacks.retain(|&(_, y, x), _| inside(x, y));
    }

    /// Parse and vet a frame-dimensions payload. Anything outside
    /// `1..=MAX_FRAME_DIMENSION` per axis is dropped: the value is a network
    /// `u32` that renderers turn into texture allocations.
    fn accept_frame_dimensions(&mut self, payload: &[u8], events: &mut Vec<Event>) {
        if payload.len() < 8 {
            return;
        }
        let width = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        let height = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        if !(1..=MAX_FRAME_DIMENSION).contains(&width)
            || !(1..=MAX_FRAME_DIMENSION).contains(&height)
        {
            return;
        }
        let changed = self.frame_dims != Some((width, height));
        self.frame_dims = Some((width, height));
        if changed {
            self.purge_outside_grid();
        }
        events.push(Event::FrameDimensions { width, height });
    }

    /// Feed one received datagram. Returns decode/render events.
    ///
    /// Tiles always come out of the protocol handling as payloads; in
    /// `TileDelivery::Decoded` mode they are then decoded to pixels here,
    /// by the same [`crate::TileDecoder`] a consumer can run elsewhere.
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: u64) -> Vec<Event> {
        let events = self.handle_datagram_payloads(bytes, now_us);
        match self.decoder.as_mut() {
            Some(decoder) => events
                .into_iter()
                .filter_map(|e| decoder.decode(e))
                .collect(),
            None => events,
        }
    }

    fn handle_datagram_payloads(&mut self, bytes: &[u8], now_us: u64) -> Vec<Event> {
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
                    self.handle_source_tile(&recovered, now_us, Provenance::Rebuilt, &mut events);
                }
            }
            return events;
        }

        // 2. Ping/pong (< 20 bytes) -> ignore.
        if bytes.len() < 20 {
            return events;
        }

        // 3. Not a tile datagram -> H.264 full-frame reassembly path. Its floor
        //    is the frame header, not the tile stack: the last fragment of an
        //    access unit can carry 0 or 1 payload bytes (22 or 23 bytes on the
        //    wire), and dropping it would stall the frame forever.
        if !is_tile_datagram(bytes) {
            if bytes.len() < ghostframe_protocol::protocol::FRAME_HEADER_SIZE {
                return events;
            }
            // Counted on the branch that classifies it, not alongside it, so
            // the count cannot disagree with the path taken.
            self.datagrams_frame = self.datagrams_frame.saturating_add(1);
            self.handle_frame_datagram(bytes, now_us, &mut events);
            return events;
        }

        // Too-short tile (< 24) -> drop.
        if bytes.len() < TILE_MIN {
            return events;
        }

        // 4. Tile datagram: feed the parity window (may replay a recovered
        //    source), then process this datagram.
        self.datagrams_tile = self.datagrams_tile.saturating_add(1);
        let wire_seq = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        if let Some(replayed) = self.parity_decoder.record_source(wire_seq, bytes) {
            self.handle_source_tile(&replayed, now_us, Provenance::Rebuilt, &mut events);
        }
        self.handle_source_tile(bytes, now_us, Provenance::Received, &mut events);

        events
    }

    /// Core per-source-tile pipeline (`handleSourceTileDatagram`).
    fn handle_source_tile(
        &mut self,
        bytes: &[u8],
        now_us: u64,
        provenance: Provenance,
        events: &mut Vec<Event>,
    ) {
        if bytes.len() < TILE_MIN {
            return;
        }
        let (dh, th, payload) = match decode_tile_datagram(bytes) {
            Ok(x) => x,
            Err(_) => return,
        };
        let frame_seq = dh.frame_seq & !TILE_DATAGRAM_FLAG;

        let is_frame_dims_sentinel =
            th.tile_x == FRAME_DIMENSIONS_SENTINEL_X && th.tile_y == FRAME_DIMENSIONS_SENTINEL_Y;
        // Eviction notices use their own sentinel coordinates (0xFE, 0xFE),
        // distinct from the frame-dimensions sentinel (0xFF, 0xFF) -- see
        // `ghostframe_protocol::eviction`. Folded into `is_sentinel` so an
        // eviction notice is neither ACKed as tile content (below) nor
        // treated as a `Codec::Skip` tile at (254, 254) that falls through
        // into assembly.
        let is_eviction_sentinel = th.tile_x == ghostframe_protocol::eviction::EVICTION_SENTINEL_X
            && th.tile_y == ghostframe_protocol::eviction::EVICTION_SENTINEL_Y;
        let is_sentinel = is_frame_dims_sentinel || is_eviction_sentinel;

        // Sentinels are sent around the emitter and carry the capture
        // clock in `timestamp_us`, not the emit stamp tile datagrams do, so
        // their spacing means nothing next to a tile datagram's.
        //
        // A datagram rebuilt from parity carries a real emit stamp but has
        // no arrival to set against it: `now_us` is when the last piece of
        // its group turned up, which can be a whole quiet spell after it
        // was sent.
        if is_sentinel || provenance == Provenance::Rebuilt {
            self.loss_tracker.on_datagram(now_us);
        } else {
            self.loss_tracker
                .on_datagram_sent_at(now_us, dh.timestamp_us);
        }

        // Wire-level loss accounting (measurement only). A rebuilt datagram
        // that had really been lost is what `datagrams_recovered_fec` in the
        // receiver feedback has always promised and never counted on this path.
        if !is_sentinel {
            match provenance {
                Provenance::Received => self.wire_loss.on_received(dh.wire_seq),
                Provenance::Rebuilt => {
                    if self.wire_loss.on_rebuilt(dh.wire_seq) {
                        self.loss_tracker.on_fec_recovery();
                    }
                }
            }
        }

        // Validate what the wire claims before it can size anything. These
        // drop before the ACK on purpose: a tile refused here (for example
        // one from a larger frame whose dimensions datagram has not arrived
        // yet) is then retransmitted rather than counted delivered.
        //
        // A tile pass is one datagram (no payload exceeds `MAX_TILE_PAYLOAD`),
        // so a tile claiming any other fragment count, or any index but the
        // first, is hostile or from a server that no longer exists: refused,
        // never stored.
        if !is_sentinel
            && (dh.frag_total != 1 || dh.frag_idx != 0 || !self.tile_in_grid(th.tile_x, th.tile_y))
        {
            return;
        }

        // ACK on receipt unless sentinel or Cdf53 (main.ts:1118-1145).
        if !is_sentinel && th.codec != Codec::Cdf53 {
            // Names the transmission, not the content. The server maps it
            // back to the tile-pass through its ledger, so every delivery
            // consumer downstream is unchanged.
            //
            // A FEC-recovered replay reaches here carrying the *original*
            // `wire_seq`, so acknowledging it reports a transmission that did
            // not actually arrive — the parity did. That slightly
            // under-reports loss, bounded by the FEC recovery rate, and is
            // taken deliberately: the alternative leaves recovered content
            // unacknowledged, so the server retransmits data the client
            // already has. Exact accounting here would mean acknowledging
            // parity envelopes in their own right, which is a larger change
            // than it is worth today.
            let entry = AckEntry {
                wire_seq: dh.wire_seq,
                arrival_time_ms_lo16: ((now_us / 1000) & 0xFFFF) as u16,
            };
            if let Some(dg) = self.ack_batcher.add(entry, now_us) {
                self.outbox.push_back(PollOutput::Datagram(dg));
            }
        }

        // The newest label the wire has delivered. Only datagrams QUIC
        // authenticated count: a rebuilt one is derived from them and is what
        // this is the yardstick for. See `label_runs_ahead`.
        if !is_sentinel && provenance == Provenance::Received {
            let advance = match self.newest_received_label {
                None => true,
                Some(newest) => crate::cdf53_coverage::frame_seq_is_newer(frame_seq, newest),
            };
            if advance {
                self.newest_received_label = Some(frame_seq);
            }
        }

        // Advance latest_frame_seq (main.ts:1156-1158).
        if frame_seq > self.latest_frame_seq {
            self.latest_frame_seq = frame_seq;
        }

        // Frame-dimensions sentinel (0xFF, 0xFF) with >= 8-byte payload, and
        // eviction sentinel (0xFE, 0xFE) with a 1-byte payload. Both are
        // always single-datagram and handled inline.
        if is_sentinel {
            if is_frame_dims_sentinel {
                self.accept_frame_dimensions(payload, events);
            } else if is_eviction_sentinel {
                if let Some(reason) = ghostframe_protocol::eviction::parse_eviction(bytes) {
                    events.push(Event::Evicted { reason });
                }
            }
            return;
        }

        // Codec::Skip -> return with no side effects beyond ACK/eviction.
        // main.ts:1224-1226.
        if th.codec == Codec::Skip {
            return;
        }

        self.deliver_tile(frame_seq, &th, dh.wire_seq, payload, now_us, events);
    }

    /// Decode one tile pass and hand it up, dispatching by codec.
    fn deliver_tile(
        &mut self,
        frame_seq: u32,
        th: &TileHeader,
        wire_seq: u32,
        payload: &[u8],
        now_us: u64,
        events: &mut Vec<Event>,
    ) {
        let tx = th.tile_x;
        let ty = th.tile_y;

        // Every codec stops at its payload here; the pixel decode, if this
        // core does it at all, runs on the result (see `handle_datagram`).
        match th.codec {
            Codec::Solid => {
                let Ok(quad) = <[u8; 4]>::try_from(payload) else {
                    // Wire-derived: a malformed Solid payload must not panic.
                    // main.ts only ever painted Solid when the payload was
                    // exactly 4 bytes; mirror that by dropping the tile
                    // silently rather than crashing.
                    return;
                };
                events.push(Event::TilePayload {
                    frame_seq,
                    tile_x: tx,
                    tile_y: ty,
                    generation: th.generation,
                    data: TileData::Solid(quad),
                });
            }
            Codec::PalRle => {
                match prevalidate_pal_rle(payload, &self.palette_shadow) {
                    Ok(validated) => {
                        // Apply the upsert exactly as decode_pal_rle_tile does.
                        // It is protocol state, and palrle_decode.wgsl decodes
                        // against this table — skipping it leaves the shader on
                        // a stale palette, which shows as wrong colours.
                        if let Some(upsert) = &validated.palette_upsert {
                            let reported: Vec<[u8; 4]> = upsert
                                .chunks_exact(4)
                                .take(validated.count as usize)
                                .map(|c| [c[0], c[1], c[2], c[3]])
                                .collect();
                            self.palette_shadow
                                .put(validated.palette_id, validated.count);
                            events.push(Event::PaletteUpdated {
                                palette_id: validated.palette_id,
                                colors: reported,
                            });
                        }
                        events.push(Event::TilePayload {
                            frame_seq,
                            tile_x: tx,
                            tile_y: ty,
                            generation: th.generation,
                            data: TileData::PalRle {
                                palette_id: validated.palette_id,
                                count: validated.count,
                                indices: validated.indices,
                                wire_flags: validated.wire_flags,
                            },
                        });
                    }
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
                // A pass from a generation older than the one this tile is
                // already accumulating is stale -- the server superseded that
                // content -- and must not reach the integrator. Integrating it
                // mixes coefficients from two different pictures, which is
                // exactly what left a tile rendering wrong forever in the
                // supersede reproduction: its sequence was
                // `gen0 p0..p13, gen1 p0, gen0 p12, gen1 p1..p13`, and that
                // one late `gen0 p12` corrupted the generation-1 image.
                //
                // Checked here rather than only in `apply_cdf53_arrival`
                // because coverage bookkeeping and rendering are separate
                // consumers; fixing the bookkeeping alone stopped the
                // spurious NACKs and left the pixels wrong.
                //
                // A label implausibly far ahead of everything received off
                // the wire is dropped before it can be believed, and a held
                // entry that has aged out is treated as absent. See
                // `label_runs_ahead` and `label_expired`.
                if crate::cdf53_coverage::label_runs_ahead(frame_seq, self.newest_received_label) {
                    return;
                }
                let prev = self.cdf53_coverage.get(&(tx, ty)).copied().filter(|e| {
                    !crate::cdf53_coverage::label_expired(e.frame_seq, self.newest_received_label)
                });
                if let Some(entry) = prev {
                    if crate::cdf53_coverage::arrival_is_stale(
                        entry.generation,
                        entry.frame_seq,
                        th.generation,
                        frame_seq,
                    ) {
                        return;
                    }
                }

                // In wire mode (Payload delivery only) the expansion is left
                // to the consumer: validate, and keep the framed streams.
                let wire = self.delivers_cdf53_wire();
                let result: Result<Cdf53Pass, _> = if wire {
                    validate_cdf53(payload, th.pass).map(|info| Cdf53Pass::Wire {
                        streams: payload[info.streams].to_vec(),
                        present_passes: info.present_passes,
                    })
                } else {
                    prevalidate_cdf53(payload, th.generation, th.pass).map(|pre| {
                        Cdf53Pass::Planes {
                            bit_planes: pre.bit_planes,
                            present_passes: pre.present_passes,
                        }
                    })
                };
                let ok = result.is_ok();
                let present_passes = result.as_ref().ok().and_then(Cdf53Pass::present_passes);
                let outcome = apply_cdf53_arrival(
                    prev,
                    th.generation,
                    th.pass,
                    frame_seq,
                    now_us,
                    ok,
                    present_passes,
                );
                if outcome.committed {
                    self.cdf53_coverage.insert((tx, ty), outcome.entry);
                }

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
                        // No accumulation here: the pixel decode (CPU
                        // `TileDecoder` or cdf53_integrate.wgsl) does it.
                        //
                        // `new_generation`: the pixel decode keys its
                        // accumulation on the 4-bit generation alone, so it
                        // cannot see a new generation that reuses the number.
                        let new_generation = outcome.new_generation;
                        let data = match pre {
                            Cdf53Pass::Planes {
                                bit_planes,
                                present_passes,
                            } => TileData::Cdf53 {
                                pass_idx: th.pass,
                                bit_planes,
                                present_passes,
                                new_generation,
                            },
                            Cdf53Pass::Wire {
                                streams,
                                present_passes,
                            } => TileData::Cdf53Wire {
                                pass_idx: th.pass,
                                streams,
                                present_passes,
                                new_generation,
                            },
                        };
                        events.push(Event::TilePayload {
                            frame_seq,
                            tile_x: tx,
                            tile_y: ty,
                            generation: th.generation,
                            data,
                        });
                        // Deferred ACK — fires only after prevalidation success.
                        // Deliberately outside the match: it is protocol, not
                        // decoding, and must happen in both modes.
                        //
                        // The pass is one datagram, so one entry names the
                        // transmission it arrived on. A pass rebuilt from
                        // FEC parity carries the original `wire_seq`; see the
                        // note on the ACK-on-receipt path above.
                        let entry = AckEntry {
                            wire_seq,
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
