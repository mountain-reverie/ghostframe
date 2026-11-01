//! ## Catch-all match arms
//!
//! `clippy::wildcard_enum_match_arm` is on for non-test code in this crate.
//! A `_ =>` arm that silently absorbs unknown variants has twice hidden real
//! defects here: a wrong classifier that stood for three months, and inbound
//! routing that fed every non-NACK datagram to the ACK parser. Listing the
//! variants makes adding one a compile error at every site that must decide
//! about it.
//!
//! Wildcards that fail *loudly* (`other => panic!(...)` in a test) are fine,
//! which is why the lint is scoped to `not(test)`. A wildcard that is
//! genuinely required -- a foreign enum we do not control -- takes a local
//! `#[allow]` with a reason, so every exception is a decision on the record.
#![cfg_attr(not(test), warn(clippy::wildcard_enum_match_arm))]

pub mod ack_batcher;
pub mod cdf53_coverage;
pub mod cdf53_prevalidate;
pub mod cdf53_tile_state;
pub mod decode_error_batcher;
mod event;
mod frame_assembly;
pub mod input;
pub mod loss_tracker;
pub mod nack_batcher;
mod ordered_map;
pub mod pal_rle_decode;
pub mod palette_shadow;
pub mod parity_decoder;
mod reassembly;
pub mod wire_loss;
pub use cdf53_coverage::CoverageEntry;
pub use event::{DecodeErrorCode, Event, PollOutput, TileData, TileKey};
pub use ghostframe_protocol::feedback_stream::ResetReason;
pub use wire_loss::WireLossStats;

use std::collections::{HashMap, VecDeque};

use ack_batcher::AckBatcher;
use cdf53_tile_state::Cdf53TileState;
use decode_error_batcher::DecodeErrorBatcher;
use frame_assembly::FrameAssembly;
use loss_tracker::LossTracker;
use nack_batcher::{NackBatcher, NackEntry};
use palette_shadow::PaletteShadow;
use parity_decoder::ParityDecoder;

/// Wire-sequence FEC window size (mirrors `new ParityDecoder(40)` in main.ts).
const PARITY_WINDOW: usize = 40;

/// Coverage-NACK debounce window (main.ts:806, `NACK_DEBOUNCE_MS`).
const NACK_DEBOUNCE_US: u64 = 50_000;

/// Tail-fallback stall threshold (main.ts:807, `TAIL_FALLBACK_MS`).
const TAIL_FALLBACK_US: u64 = 1_500_000;

/// How many tail sweeps a tile gets before the client stops asking.
///
/// Each sweep re-requests every pass the tile is missing. Six of them spans
/// `6 * TAIL_SWEEP_INTERVAL_US` = 3 s of continued silence after the
/// `TAIL_FALLBACK_US` grace period, which is ample for a path that is merely
/// slow. Beyond that the passes are not coming, and continuing to ask costs
/// the server a lookup and a futile retransmission every 500 ms forever.
const MAX_TAIL_SWEEP_ATTEMPTS: u8 = 6;

/// Tail-sweep scan cadence (main.ts:808, `TAIL_SWEEP_INTERVAL_MS`).
const TAIL_SWEEP_INTERVAL_US: u64 = 500_000;

/// Periodic ReceiverFeedback cadence (main.ts:391-401, 100ms setInterval).
const FEEDBACK_INTERVAL_US: u64 = 100_000;

/// Largest frame dimension (either axis, in pixels) the core will pass on.
///
/// A `FrameDimensions` event is a network `u32` that drives texture and
/// buffer allocation in every renderer, so the core refuses anything larger
/// instead of trusting each renderer to. 8192 covers 4K, dual-screen layouts
/// (7680x2160 side by side, 3840x4320 stacked) and 8K, with headroom; it is
/// also the largest 2D texture most GPUs accept. Renderers add their own
/// clamp against this same value as belt and braces.
pub const MAX_FRAME_DIMENSION: u32 = 8192;

/// Most source fragments one H.264 access unit may claim.
///
/// [`MAX_H264_ACCESS_UNIT_BYTES`] at the smallest fragment worth sending
/// (~1 KiB) is 16384; the largest legitimate access unit (an 8K keyframe at
/// ~1.18 KiB per fragment) needs ~14k.
pub const MAX_FRAME_FRAGMENTS: u16 = 16_384;

/// Most payload bytes one H.264 access unit may buffer before it is dropped.
pub const MAX_H264_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;

/// Snapshot of reassembly occupancy, for diagnostics and tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AssemblyStats {
    pub frame_assemblies: usize,
}

/// One coverage NACK awaiting the debounce window (main.ts:815, `PendingNack`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingNack {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
}

/// Whether the core decodes tiles to pixels, or stops after validation and
/// hands the payload up.
///
/// The browser decodes on the GPU (`ghostframe-web-client/src/webgpu/`, ten
/// WGSL compute shaders), so it wants validated payloads, not RGBA. Native
/// and headless consumers want pixels. One core, two consumers — not two
/// implementations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TileDelivery {
    /// Decode to RGBA and emit `Event::TileReady`. The default, because it is
    /// what every existing consumer expects.
    #[default]
    Decoded,
    /// Stop after reassembly, parity recovery, prevalidation and generation
    /// checks; emit the undecoded bytes instead.
    Payload,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ClientConfig {
    pub indices_raw_enabled: bool,
    pub supports_h264: bool,
    /// The client sends the v2 H.264 feedback (reset requests; frame acks in
    /// later phases): advertised as HELLO bit 2. It does not gate H.264 --
    /// `supports_h264` does. Defaults to `false`; the web client does not set
    /// it yet.
    pub h264_acked_ref: bool,
    /// See [`TileDelivery`]. Defaults to `Decoded`.
    pub tile_delivery: TileDelivery,
    /// What the client knows about its own display. `None` if the client
    /// cannot determine it, in which case no DisplayInfo message is sent.
    pub display: Option<ClientDisplay>,
}

/// What the client knows about its own display. Deliberately NOT named
/// `DisplayInfo` -- that name belongs to the wire message in
/// `ghostframe-lib`, and client-core cannot depend on the server crate, so
/// two same-named types in different crates would read as one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientDisplay {
    pub max_width: u16,
    pub max_height: u16,
    /// Thousandths: 1000 = 1.0.
    pub scale_milli: u16,
    /// Advisory only -- see the module docs on why scale is authoritative.
    pub mm_width: u16,
    pub mm_height: u16,
}

pub struct ClientCore {
    pub(crate) outbox: VecDeque<PollOutput>,
    /// Frame size last accepted from the server, if any. Tile coordinates
    /// are validated against the grid it implies.
    pub(crate) frame_dims: Option<(u32, u32)>,
    pub(crate) parity_decoder: ParityDecoder,
    pub(crate) wire_loss: wire_loss::WireLossTracker,
    pub(crate) ack_batcher: AckBatcher,
    pub(crate) nack_batcher: NackBatcher,
    pub(crate) decode_error_batcher: DecodeErrorBatcher,
    pub(crate) loss_tracker: LossTracker,
    pub(crate) palette_shadow: PaletteShadow,
    /// BGRA palette tables, `[palette_id][slot] = [b, g, r, a]`.
    pub(crate) palettes: Box<[[[u8; 4]; 16]; 256]>,
    pub(crate) cdf53_coverage: HashMap<(u8, u8), CoverageEntry>,
    pub(crate) cdf53_tile_state: Cdf53TileState,
    /// Newest `frame_seq` label on a tile datagram that came off the wire
    /// (not one rebuilt from parity). The yardstick for judging a label
    /// plausible; see `cdf53_coverage::label_runs_ahead`.
    pub(crate) newest_received_label: Option<u32>,
    pub(crate) latest_frame_seq: u32,
    /// H.264 frame-level reassembly state (keyed by `frame_seq`).
    pub(crate) frame_assemblies: HashMap<u32, FrameAssembly>,
    pub(crate) latest_full_frame_seq: u32,
    /// Newest H.264 frame handed out as `NeedsH264`; a keyframe older than it
    /// is not delivered.
    pub(crate) last_delivered_frame_seq: Option<u32>,
    /// H.264 fragments rebuilt from XOR parity (cumulative).
    pub(crate) h264_fragments_recovered: u64,
    /// Coverage NACKs awaiting the debounce window before being handed to
    /// `nack_batcher` (main.ts `pendingNacks` / `queuePassNack`).
    /// Keyed `(pass_idx, tile_y, tile_x)`, so iteration order IS send order:
    /// pass-major, then by position. See `flush_pending_nacks`.
    pub(crate) pending_nacks: std::collections::BTreeMap<(u8, u8, u8), PendingNack>,
    pub(crate) pending_nack_deadline_us: Option<u64>,
    /// Next `now_us` at which the tail sweep runs (always armed).
    pub(crate) next_tail_sweep_us: u64,
    /// Next `now_us` at which periodic feedback is emitted (always armed).
    pub(crate) next_feedback_us: u64,
    /// See [`TileDelivery`]. Set once at construction from `ClientConfig`.
    pub(crate) tile_delivery: TileDelivery,
    /// Protocol datagrams accepted by `handle_datagram`, split the same way
    /// its dispatch splits them: tile datagrams (bit 31 of `frame_seq` set)
    /// and H.264 frame datagrams (bit 31 clear).
    ///
    /// Counted here rather than in the browser because the browser cannot
    /// classify them without duplicating that dispatch, and a second copy of
    /// a discriminator is exactly how one drifts. Parity and NACK envelopes
    /// are in neither count -- see [`ClientCore::datagram_counts`].
    pub(crate) datagrams_tile: u64,
    pub(crate) datagrams_frame: u64,
}

impl ClientCore {
    pub fn new(config: ClientConfig, now_us: u64) -> Self {
        let mut core = ClientCore {
            outbox: VecDeque::new(),
            frame_dims: None,
            parity_decoder: ParityDecoder::new(PARITY_WINDOW),
            wire_loss: wire_loss::WireLossTracker::new(),
            ack_batcher: AckBatcher::new(),
            nack_batcher: NackBatcher::new(),
            decode_error_batcher: DecodeErrorBatcher::new(),
            loss_tracker: LossTracker::new(),
            palette_shadow: PaletteShadow::new(),
            palettes: Box::new([[[0u8; 4]; 16]; 256]),
            cdf53_coverage: HashMap::new(),
            cdf53_tile_state: Cdf53TileState::new(),
            newest_received_label: None,
            latest_frame_seq: 0,
            frame_assemblies: HashMap::new(),
            latest_full_frame_seq: 0,
            last_delivered_frame_seq: None,
            h264_fragments_recovered: 0,
            pending_nacks: std::collections::BTreeMap::new(),
            pending_nack_deadline_us: None,
            next_tail_sweep_us: now_us + TAIL_SWEEP_INTERVAL_US,
            next_feedback_us: now_us + FEEDBACK_INTERVAL_US,
            tile_delivery: config.tile_delivery,
            datagrams_tile: 0,
            datagrams_frame: 0,
        };

        // Construct the Hello message [0x03, caps]
        // bit0 = indices_raw_enabled, bit1 = supports_h264,
        // bit2 = h264_acked_ref
        core.outbox
            .push_back(PollOutput::Stream(loss_tracker::encode_hello(
                config.indices_raw_enabled,
                config.supports_h264,
                config.h264_acked_ref,
            )));

        if let Some(display) = config.display {
            core.outbox
                .push_back(PollOutput::Stream(loss_tracker::encode_display_info(
                    display.max_width,
                    display.max_height,
                    display.scale_milli,
                    display.mm_width,
                    display.mm_height,
                )));
        }

        core
    }

    /// Queue a display-mode request. The server clamps it and reports what
    /// it actually set through the existing frame-dimensions message --
    /// callers must render what arrives, not what they asked for.
    pub fn request_display_mode(&mut self, width: u16, height: u16) {
        self.outbox
            .push_back(PollOutput::Stream(loss_tracker::encode_display_mode(
                width, height,
            )));
    }

    /// Ask the server for a fresh IDR (feedback stream 0x09). `frame_seq` is
    /// the frame the client was handling when it gave up. The server
    /// rate-limits these, so repeating one is harmless but wasteful.
    pub fn request_reset(&mut self, reason: ResetReason, frame_seq: u32) {
        self.outbox
            .push_back(PollOutput::Stream(loss_tracker::encode_reset_request(
                reason, frame_seq,
            )));
    }

    /// Drain one pending outbound message; call until None.
    pub fn poll_transmit(&mut self, _now_us: u64) -> Option<PollOutput> {
        self.outbox.pop_front()
    }

    /// Earliest deadline (µs) at which `on_timeout` must be called.
    ///
    /// = min(ack batcher, nack batcher, coverage-NACK debounce, next
    /// tail sweep, next feedback emit). The
    /// tail sweep and feedback deadlines are always armed (mirroring
    /// main.ts's always-running rAF loop / 100ms `setInterval`), so this
    /// is never `None` once the core has been constructed.
    pub fn poll_timeout(&self) -> Option<u64> {
        let mut deadline = self.next_tail_sweep_us.min(self.next_feedback_us);
        if let Some(a) = self.ack_batcher.poll_timeout() {
            deadline = deadline.min(a);
        }
        if let Some(n) = self.nack_batcher.poll_timeout() {
            deadline = deadline.min(n);
        }
        if let Some(d) = self.pending_nack_deadline_us {
            deadline = deadline.min(d);
        }
        Some(deadline)
    }

    /// H.264 fragments rebuilt from XOR parity since the core was created.
    pub fn h264_fragments_recovered(&self) -> u64 {
        self.h264_fragments_recovered
    }

    /// Pre-FEC loss, loss-run lengths and FEC recoveries measured from
    /// `wire_seq` gaps. Measurement only.
    pub fn wire_loss_stats(&self) -> WireLossStats {
        self.wire_loss.stats()
    }

    /// Reassembly occupancy, for diagnostics and tests.
    pub fn assembly_stats(&self) -> AssemblyStats {
        AssemblyStats {
            frame_assemblies: self.frame_assemblies.len(),
        }
    }

    /// Queue a coverage NACK for debounced dispatch (`queuePassNack`,
    /// main.ts:842-849). Deduplicated by (tile_x, tile_y, pass_idx); the
    /// debounce deadline is armed only when the queue transitions from
    /// empty to non-empty (matches `nackFlushTimer === null` guard).
    pub(crate) fn queue_pass_nack(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        now_us: u64,
    ) {
        let key = (pass_idx, tile_y, tile_x);
        if self.pending_nacks.contains_key(&key) {
            return;
        }
        self.pending_nacks.insert(
            key,
            PendingNack {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
            },
        );
        if self.pending_nack_deadline_us.is_none() {
            self.pending_nack_deadline_us = Some(now_us + NACK_DEBOUNCE_US);
        }
    }

    /// Flush the pending coverage-NACK queue (`flushPendingNacks`,
    /// main.ts:818-841): re-check the live coverage bitmap for each
    /// pending pass right before sending — a pass that arrived validly
    /// during the debounce window is dropped silently.
    fn flush_pending_nacks(&mut self, now_us: u64) {
        self.pending_nack_deadline_us = None;
        if self.pending_nacks.is_empty() {
            return;
        }
        // Pass-major: every pass 0 goes before any pass 1, and so on. The
        // server queues NACKs FIFO and drains them under its retransmit
        // budget, so the order sent here is the order recovered. Position
        // order (tile_y, tile_x, pass) would spend the budget on every pass
        // of the top-left tiles before pass 0 of the lower rows, the inverse
        // of the pass-major order refinement is emitted in. A `BTreeMap`
        // also makes the order independent of hashing, which decides which
        // NACKs share a datagram and so would make wire traffic unreproducible.
        let pending: Vec<PendingNack> = std::mem::take(&mut self.pending_nacks)
            .into_values()
            .collect();
        let mut queued = false;
        for p in pending {
            if let Some(cov) = self.cdf53_coverage.get(&(p.tile_x, p.tile_y)) {
                if cov.pass_mask & (1u16 << p.pass_idx) != 0 {
                    continue; // arrived validly during the debounce window.
                }
            }
            use ghostframe_protocol::protocol::TILE_DATAGRAM_FLAG;
            let entry = NackEntry {
                frame_seq: p.frame_seq | TILE_DATAGRAM_FLAG,
                tile_x: p.tile_x,
                tile_y: p.tile_y,
                pass_idx: p.pass_idx,
            };
            if let Some(dg) = self.nack_batcher.add(entry, now_us) {
                self.outbox.push_back(PollOutput::Datagram(dg));
            } else {
                queued = true;
            }
        }
        // The debounce window has already elapsed, so any entries the
        // batcher merely queued (didn't hit its own entry cap) must be
        // flushed immediately rather than waiting on its separate 5ms
        // flush timer — the debounce already provided the delay.
        if queued {
            if let Some(dg) = self.nack_batcher.flush() {
                self.outbox.push_back(PollOutput::Datagram(dg));
            }
        }
    }

    /// Tail-fallback sweep (main.ts:855-901): every `TAIL_SWEEP_INTERVAL_US`,
    /// any coverage entry that hasn't gained a new pass in
    /// `TAIL_FALLBACK_US` gets its missing passes re-queued for a
    /// (debounced) NACK, and those bits are cleared from `nacked_mask` so
    /// gap-detection can fire again if arrivals resume. `last_change_us`
    /// is reset on every sweep of a stalled tile, so it isn't re-swept on
    /// the very next tick.
    /// Snapshot of per-tile Cdf53 coverage, for diagnostics.
    ///
    /// Exposed because the browser can no longer derive this: the TypeScript
    /// decode path that maintained `window.__cdf53Coverage` was deleted at
    /// the wasm cutover, leaving the client's `cdf53-coverage` log line
    /// reporting zeros on sessions that had decoded thousands of tiles.
    ///
    /// `partial` is the stranding signal: tiles still owed a pass. `gave_up`
    /// counts tiles that exhausted `MAX_TAIL_SWEEP_ATTEMPTS` on a gap that was
    /// known (a higher pass arrived) and never repaired. A tile whose final
    /// pass was lost has no gap evidence, so it never asks and never counts as
    /// `gave_up`; only `partial` shows it.
    pub fn cdf53_coverage_summary(&self) -> crate::cdf53_coverage::Cdf53CoverageSummary {
        crate::cdf53_coverage::summarize(self.cdf53_coverage.values(), MAX_TAIL_SWEEP_ATTEMPTS)
    }

    /// Protocol datagrams accepted so far as `(tile, frame)`.
    ///
    /// The split is the one `handle_datagram` actually dispatches on, so the
    /// counts cannot disagree with the path a datagram took. Deliberately
    /// excluded: ping/pong (not protocol), datagrams too short to carry a
    /// header, and parity/NACK envelopes -- a parity datagram carries
    /// recovered tile bytes but is not itself a tile datagram, and counting
    /// it as one would inflate the tile side only on FEC-enabled sessions.
    ///
    /// This exists because `window.__ghostframeStats` was fed by the
    /// TypeScript receive loop that the wasm cutover deleted. `main.ts` kept
    /// `const stats = diag.stats` and never touched it again, so
    /// `tileDatagrams`/`frameDatagrams` read 0 on sessions moving thousands
    /// of datagrams, and the five e2e tests asserting on them could not pass.
    pub fn datagram_counts(&self) -> (u64, u64) {
        (self.datagrams_tile, self.datagrams_frame)
    }

    /// Coordinates of tiles still missing at least one present pass, with
    /// their received and expected masks, capped at `limit` so a fully
    /// stalled screen cannot produce a 2040-entry log line.
    pub fn cdf53_incomplete_tiles(&self, limit: usize) -> Vec<(u8, u8, u16, u16, u8)> {
        let mut out: Vec<(u8, u8, u16, u16, u8)> = self
            .cdf53_coverage
            .iter()
            .filter_map(|((x, y), e)| {
                let present = e.present_passes?;
                (e.pass_mask & present != present).then_some((
                    *x,
                    *y,
                    e.pass_mask,
                    present,
                    e.sweep_attempts,
                ))
            })
            .collect();
        // Most-stalled first, then by position so the list is stable.
        out.sort_by_key(|(x, y, mask, present, sweeps)| {
            (
                std::cmp::Reverse(*sweeps),
                (present & !mask).count_ones(),
                *y,
                *x,
            )
        });
        out.truncate(limit);
        out
    }

    fn tail_sweep(&mut self, now_us: u64) {
        if now_us < self.next_tail_sweep_us {
            return;
        }
        self.next_tail_sweep_us = now_us + TAIL_SWEEP_INTERVAL_US;

        // No sort: `queue_pass_nack` inserts into a BTreeMap keyed
        // (pass, y, x), which fixes the order the NACKs go out in.
        let keys: Vec<(u8, u8)> = self.cdf53_coverage.keys().copied().collect();
        for key in keys {
            let (frame_seq, missing) = {
                let entry = match self.cdf53_coverage.get(&key) {
                    Some(e) => e,
                    None => continue,
                };
                // Completeness is judged against the tile's OWN bitmap, not
                // a hardcoded "all 14" mask: sparse encoding means a tile's
                // real pass set is a subset of 14, sent as a bitmap in pass
                // 0's payload.
                //   - known (`present_passes` learned from pass 0): complete
                //     once `pass_mask == present_passes`; the missing set is
                //     `present_passes & !pass_mask`.
                //   - unknown (pass 0 hasn't arrived yet): the tile can't be
                //     complete, and the only pass worth re-asking for is
                //     pass 0 itself -- there's no way to know which of
                //     1..13 to expect without it.
                let missing = match entry.present_passes {
                    Some(present) => {
                        if entry.pass_mask == present {
                            continue; // complete
                        }
                        present & !entry.pass_mask
                    }
                    None => 1u16, // pass 0 unknown; ask only for pass 0.
                };
                if now_us.saturating_sub(entry.last_change_us) < TAIL_FALLBACK_US {
                    continue;
                }
                // Gap evidence (RC5): only a pass that a higher pass of this
                // generation has overtaken is known to be lost. Silence on
                // its own is not -- the server may not have sent it yet
                // (refinement queued behind bandwidth) -- and NACKing it
                // made every such pass a miss on the server, indistinguishable
                // from a real cache miss. The 1.5 s grace above stays as a
                // floor on how long a gap is left to the cheaper repair paths.
                //
                // Content superseded by a later frame needs no rule here:
                // a newer `frame_seq` replaces the whole entry (see
                // `classify_arrival`), so an older frame's gaps are dropped
                // with it and never asked for.
                let missing = entry.gap_evidence_mask(missing);
                if missing == 0 {
                    continue;
                }
                // Give up on a tile that has stopped making progress. The
                // sweep clears `nacked_mask` below so a request can repeat,
                // which on its own never terminates: a pass that will never
                // arrive is asked for every TAIL_SWEEP_INTERVAL_US for the
                // life of the session, and each request costs the server a
                // cache lookup and -- when it hits -- a retransmission of
                // something that is not going to help.
                //
                // Measured in production on a static screen with nothing to
                // send: 106,847 NACKs from the client against 94,834 server
                // retransmissions and 53,761 acknowledgements. Reproduced as
                // growth exactly linear in session duration.
                //
                // `sweep_attempts` resets whenever a pass lands, so a tile
                // still receiving data keeps its full budget; only a stalled
                // one runs out.
                if entry.sweep_attempts >= MAX_TAIL_SWEEP_ATTEMPTS {
                    continue;
                }
                (entry.frame_seq, missing)
            };
            for p in 0..14u8 {
                if missing & (1u16 << p) != 0 {
                    self.queue_pass_nack(frame_seq, key.0, key.1, p, now_us);
                }
            }
            if let Some(entry) = self.cdf53_coverage.get_mut(&key) {
                entry.nacked_mask &= !missing;
                entry.last_change_us = now_us;
                entry.sweep_attempts = entry.sweep_attempts.saturating_add(1);
            }
        }
    }

    /// Fire any batcher/timer deadlines that `now_us` has reached; flushed
    /// datagrams and feedback messages are queued to the outbox. Multiple
    /// deadlines may be due in a single call. Returns no render events
    /// (`handle_datagram` is the sole source of `Event`s).
    pub fn on_timeout(&mut self, now_us: u64) -> Vec<Event> {
        if let Some(dg) = self.ack_batcher.on_timeout(now_us) {
            self.outbox.push_back(PollOutput::Datagram(dg));
        }
        if let Some(dg) = self.nack_batcher.on_timeout(now_us) {
            self.outbox.push_back(PollOutput::Datagram(dg));
        }
        if let Some(deadline) = self.pending_nack_deadline_us {
            if now_us >= deadline {
                self.flush_pending_nacks(now_us);
            }
        }
        self.tail_sweep(now_us);
        if now_us >= self.next_feedback_us {
            let msg = self.loss_tracker.encode_feedback(now_us);
            self.outbox.push_back(PollOutput::Stream(msg));
            self.next_feedback_us = now_us + FEEDBACK_INTERVAL_US;
        }
        Vec::new()
    }

    /// Encode + queue-worthy `ReceiverFeedback` (used by the host to send
    /// periodic feedback). Returns the 18-byte wire message and resets the
    /// loss counters.
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.loss_tracker.encode_feedback(now_us)
    }
}

#[cfg(test)]
mod nack_order_tests {
    use super::*;

    /// Entries of every NACK datagram in the outbox, in send order.
    fn sent_nacks(core: &mut ClientCore) -> Vec<(u8, u8, u8)> {
        let mut out = Vec::new();
        while let Some(o) = core.poll_transmit(0) {
            if let PollOutput::Datagram(d) = o {
                if d.first() != Some(&0x05) {
                    continue;
                }
                for e in d[2..].chunks_exact(ghostframe_protocol::protocol::TILE_NACK_ENTRY_SIZE) {
                    // (pass_idx, tile_y, tile_x)
                    out.push((e[6], e[5], e[4]));
                }
            }
        }
        out
    }

    /// The server drains NACKs FIFO under a retransmit budget. With a budget
    /// smaller than the set, whatever is sent first is what gets recovered,
    /// so every pass-0 NACK must precede any pass-1 NACK however the NACKs
    /// were queued, and the order must not depend on insertion order.
    #[test]
    fn nacks_go_out_pass_major_so_a_small_budget_recovers_pass_zero_first() {
        let mut core = ClientCore::new(ClientConfig::default(), 0);
        while core.poll_transmit(0).is_some() {}
        // 4x3 tiles, 3 passes each, queued in an order that is neither
        // pass-major nor position order.
        let mut all = Vec::new();
        for y in 0..3u8 {
            for x in 0..4u8 {
                for p in 0..3u8 {
                    all.push((x, y, p));
                }
            }
        }
        all.reverse();
        all.rotate_left(5);
        for (x, y, p) in all {
            core.queue_pass_nack(1, x, y, p, 0);
        }
        core.on_timeout(NACK_DEBOUNCE_US);
        let sent = sent_nacks(&mut core);
        assert_eq!(sent.len(), 36, "premise: every queued NACK was sent");
        let first_pass1 = sent.iter().position(|e| e.0 == 1).expect("a pass-1 NACK");
        let last_pass0 = sent.iter().rposition(|e| e.0 == 0).expect("a pass-0 NACK");
        assert!(
            last_pass0 < first_pass1,
            "a pass-1 NACK was sent before the last pass-0 NACK: {sent:?}"
        );
        // A budget smaller than the set (12 pass-0 NACKs of 36) recovers all
        // of pass 0 and nothing of pass 1 or 2.
        let budgeted = &sent[..12];
        assert!(budgeted.iter().all(|e| e.0 == 0), "{budgeted:?}");
        // The whole order is the sorted one: deterministic, pass-major.
        let mut sorted = sent.clone();
        sorted.sort_unstable();
        assert_eq!(sent, sorted);
    }
}
