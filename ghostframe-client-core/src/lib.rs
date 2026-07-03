pub mod ack_batcher;
pub mod cdf53_coverage;
pub mod cdf53_prevalidate;
pub mod cdf53_tile_state;
pub mod decode_error_batcher;
mod event;
pub mod fragment_parity;
mod frame_assembly;
pub mod input;
pub mod loss_tracker;
pub mod nack_batcher;
mod ordered_map;
pub mod pal_rle_decode;
pub mod palette_shadow;
pub mod parity_decoder;
mod reassembly;
pub use cdf53_coverage::CoverageEntry;
pub use event::{DecodeErrorCode, Event, PollOutput, TileKey};

use std::collections::{HashMap, HashSet, VecDeque};

use ghostframe_protocol::protocol::TileHeader;

use ack_batcher::AckBatcher;
use cdf53_tile_state::Cdf53TileState;
use decode_error_batcher::DecodeErrorBatcher;
use fragment_parity::FragmentParity;
use frame_assembly::FrameAssembly;
use loss_tracker::LossTracker;
use nack_batcher::{NackBatcher, NackEntry};
use palette_shadow::PaletteShadow;
use parity_decoder::ParityDecoder;

/// Wire-sequence FEC window size (mirrors `new ParityDecoder(40)` in main.ts).
const PARITY_WINDOW: usize = 40;

/// Reliable-tile-emitter: NACK any fragment still missing this long after
/// the assembly's first fragment arrived (main.ts:756, `ASSEMBLY_TIMEOUT_MS`).
const ASSEMBLY_TIMEOUT_US: u64 = 30_000;

/// Coverage-NACK debounce window (main.ts:806, `NACK_DEBOUNCE_MS`).
const NACK_DEBOUNCE_US: u64 = 50_000;

/// Tail-fallback stall threshold (main.ts:807, `TAIL_FALLBACK_MS`).
const TAIL_FALLBACK_US: u64 = 1_500_000;

/// Tail-sweep scan cadence (main.ts:808, `TAIL_SWEEP_INTERVAL_MS`).
const TAIL_SWEEP_INTERVAL_US: u64 = 500_000;

/// Periodic ReceiverFeedback cadence (main.ts:391-401, 100ms setInterval).
const FEEDBACK_INTERVAL_US: u64 = 100_000;

/// All 14 CDF53 passes present (main.ts:809, `FULL_PASS_MASK`).
const FULL_PASS_MASK: u16 = (1 << 14) - 1;

/// One coverage NACK awaiting the debounce window (main.ts:815, `PendingNack`).
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingNack {
    pub frame_seq: u32,
    pub tile_x: u8,
    pub tile_y: u8,
    pub pass_idx: u8,
}

#[derive(Debug, Clone, Copy)]
pub struct ClientConfig {
    pub indices_raw_enabled: bool,
    pub supports_h264: bool,
}

/// One in-progress tile-pass assembly (fragments keyed by `TileKey`).
pub(crate) struct Assembly {
    pub tile_x: u8,
    pub tile_y: u8,
    pub codec: ghostframe_protocol::protocol::Codec,
    pub generation: u8,
    pub pass: u8,
    pub frame_seq: u32,
    pub fragments: Vec<Option<Vec<u8>>>,
    pub received: usize,
    /// `now_us` at which this assembly was created (first fragment
    /// arrival); the assembly-timeout scan measures elapsed time from here.
    pub partial_since_us: u64,
    /// Fragment indices already NACKed by the assembly-timeout scan, so a
    /// missing fragment is only ever NACKed once per assembly.
    pub nacked_frag_idxs: HashSet<usize>,
}

impl Assembly {
    fn new(header: &TileHeader, frame_seq: u32, frag_total: u16, now_us: u64) -> Self {
        Assembly {
            tile_x: header.tile_x,
            tile_y: header.tile_y,
            codec: header.codec,
            generation: header.generation,
            pass: header.pass,
            frame_seq,
            fragments: vec![None; frag_total as usize],
            received: 0,
            partial_since_us: now_us,
            nacked_frag_idxs: HashSet::new(),
        }
    }
}

pub struct ClientCore {
    pub(crate) outbox: VecDeque<PollOutput>,
    pub(crate) assemblies: HashMap<TileKey, Assembly>,
    pub(crate) parity_decoder: ParityDecoder,
    pub(crate) fragment_parity: FragmentParity,
    pub(crate) ack_batcher: AckBatcher,
    pub(crate) nack_batcher: NackBatcher,
    pub(crate) decode_error_batcher: DecodeErrorBatcher,
    pub(crate) loss_tracker: LossTracker,
    pub(crate) palette_shadow: PaletteShadow,
    /// BGRA palette tables, `[palette_id][slot] = [b, g, r, a]`.
    pub(crate) palettes: Box<[[[u8; 4]; 16]; 256]>,
    pub(crate) cdf53_coverage: HashMap<(u8, u8), CoverageEntry>,
    pub(crate) cdf53_tile_state: Cdf53TileState,
    pub(crate) latest_frame_seq: u32,
    /// H.264 frame-level reassembly state (keyed by `frame_seq`).
    pub(crate) frame_assemblies: HashMap<u32, FrameAssembly>,
    pub(crate) latest_full_frame_seq: u32,
    /// Coverage NACKs awaiting the debounce window before being handed to
    /// `nack_batcher` (main.ts `pendingNacks` / `queuePassNack`).
    pub(crate) pending_nacks: HashMap<(u8, u8, u8), PendingNack>,
    pub(crate) pending_nack_deadline_us: Option<u64>,
    /// Next `now_us` at which the tail sweep runs (always armed).
    pub(crate) next_tail_sweep_us: u64,
    /// Next `now_us` at which periodic feedback is emitted (always armed).
    pub(crate) next_feedback_us: u64,
}

impl ClientCore {
    pub fn new(config: ClientConfig, now_us: u64) -> Self {
        let mut core = ClientCore {
            outbox: VecDeque::new(),
            assemblies: HashMap::new(),
            parity_decoder: ParityDecoder::new(PARITY_WINDOW),
            fragment_parity: FragmentParity::new(),
            ack_batcher: AckBatcher::new(),
            nack_batcher: NackBatcher::new(),
            decode_error_batcher: DecodeErrorBatcher::new(),
            loss_tracker: LossTracker::new(),
            palette_shadow: PaletteShadow::new(),
            palettes: Box::new([[[0u8; 4]; 16]; 256]),
            cdf53_coverage: HashMap::new(),
            cdf53_tile_state: Cdf53TileState::new(),
            latest_frame_seq: 0,
            frame_assemblies: HashMap::new(),
            latest_full_frame_seq: 0,
            pending_nacks: HashMap::new(),
            pending_nack_deadline_us: None,
            next_tail_sweep_us: now_us + TAIL_SWEEP_INTERVAL_US,
            next_feedback_us: now_us + FEEDBACK_INTERVAL_US,
        };

        // Construct the Hello message [0x03, caps]
        // bit0 = indices_raw_enabled, bit1 = supports_h264
        core.outbox
            .push_back(PollOutput::Stream(loss_tracker::encode_hello(
                config.indices_raw_enabled,
                config.supports_h264,
            )));

        core
    }

    /// Drain one pending outbound message; call until None.
    pub fn poll_transmit(&mut self, _now_us: u64) -> Option<PollOutput> {
        self.outbox.pop_front()
    }

    /// Earliest deadline (µs) at which `on_timeout` must be called.
    ///
    /// = min(ack batcher, nack batcher, coverage-NACK debounce, next
    /// assembly-timeout scan, next tail sweep, next feedback emit). The
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
        if let Some(s) = self.next_assembly_scan_us() {
            deadline = deadline.min(s);
        }
        Some(deadline)
    }

    /// Earliest `partial_since_us + ASSEMBLY_TIMEOUT_US` across incomplete
    /// assemblies, if any are pending.
    fn next_assembly_scan_us(&self) -> Option<u64> {
        self.assemblies
            .values()
            .filter(|a| a.received < a.fragments.len())
            .filter(|a| {
                // If every still-missing fragment has already been NACKed by
                // a prior scan, this assembly has nothing left to contribute
                // to the scan deadline; leaving it in the min-fold would pin
                // poll_timeout at a perpetually-past instant (a host "sleep
                // until poll_timeout" driver would busy-loop).
                a.fragments
                    .iter()
                    .enumerate()
                    .any(|(i, f)| f.is_none() && !a.nacked_frag_idxs.contains(&i))
            })
            .map(|a| a.partial_since_us + ASSEMBLY_TIMEOUT_US)
            .min()
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
        let key = (tile_x, tile_y, pass_idx);
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
        let pending: Vec<PendingNack> = self.pending_nacks.drain().map(|(_, v)| v).collect();
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
                frag_idx: 0,
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

    /// Assembly-timeout scan (`scanForAssemblyTimeouts`, main.ts:756-769):
    /// any assembly whose first fragment arrived `>= ASSEMBLY_TIMEOUT_US`
    /// ago gets every still-missing fragment NACKed once (deduped per
    /// assembly via `nacked_frag_idxs`).
    fn scan_assembly_timeouts(&mut self, now_us: u64) {
        let keys: Vec<TileKey> = self.assemblies.keys().copied().collect();
        let mut queued = false;
        for key in keys {
            let (frame_seq, tile_x, tile_y, pass_idx, missing): (u32, u8, u8, u8, Vec<usize>) = {
                let asm = match self.assemblies.get(&key) {
                    Some(a) => a,
                    None => continue,
                };
                if asm.received >= asm.fragments.len() {
                    continue;
                }
                if now_us.saturating_sub(asm.partial_since_us) < ASSEMBLY_TIMEOUT_US {
                    continue;
                }
                let missing: Vec<usize> = asm
                    .fragments
                    .iter()
                    .enumerate()
                    .filter(|(i, f)| f.is_none() && !asm.nacked_frag_idxs.contains(i))
                    .map(|(i, _)| i)
                    .collect();
                (asm.frame_seq, asm.tile_x, asm.tile_y, key.pass_idx, missing)
            };
            if missing.is_empty() {
                continue;
            }
            use ghostframe_protocol::protocol::TILE_DATAGRAM_FLAG;
            for frag_idx in &missing {
                let entry = NackEntry {
                    frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
                    tile_x,
                    tile_y,
                    pass_idx,
                    frag_idx: *frag_idx as u8,
                };
                if let Some(dg) = self.nack_batcher.add(entry, now_us) {
                    self.outbox.push_back(PollOutput::Datagram(dg));
                } else {
                    queued = true;
                }
            }
            if let Some(asm) = self.assemblies.get_mut(&key) {
                for frag_idx in missing {
                    asm.nacked_frag_idxs.insert(frag_idx);
                }
            }
        }
        // The assembly has already sat past ASSEMBLY_TIMEOUT_US, so flush
        // immediately rather than waiting on the batcher's own 5ms timer.
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
    fn tail_sweep(&mut self, now_us: u64) {
        if now_us < self.next_tail_sweep_us {
            return;
        }
        self.next_tail_sweep_us = now_us + TAIL_SWEEP_INTERVAL_US;

        let keys: Vec<(u8, u8)> = self.cdf53_coverage.keys().copied().collect();
        for key in keys {
            let (frame_seq, missing) = {
                let entry = match self.cdf53_coverage.get(&key) {
                    Some(e) => e,
                    None => continue,
                };
                if entry.pass_mask == FULL_PASS_MASK {
                    continue;
                }
                if now_us.saturating_sub(entry.last_change_us) < TAIL_FALLBACK_US {
                    continue;
                }
                (entry.frame_seq, !entry.pass_mask & FULL_PASS_MASK)
            };
            for p in 0..14u8 {
                if missing & (1u16 << p) != 0 {
                    self.queue_pass_nack(frame_seq, key.0, key.1, p, now_us);
                }
            }
            if let Some(entry) = self.cdf53_coverage.get_mut(&key) {
                entry.nacked_mask &= !missing;
                entry.last_change_us = now_us;
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
        self.scan_assembly_timeouts(now_us);
        self.tail_sweep(now_us);
        if now_us >= self.next_feedback_us {
            let msg = self.loss_tracker.encode_feedback(now_us);
            self.outbox.push_back(PollOutput::Stream(msg));
            self.next_feedback_us = now_us + FEEDBACK_INTERVAL_US;
        }
        Vec::new()
    }

    /// Encode + queue-worthy `ReceiverFeedback` (used by the host to send
    /// periodic feedback). Returns the 22-byte wire message and resets the
    /// loss counters.
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.loss_tracker.encode_feedback(now_us)
    }
}
