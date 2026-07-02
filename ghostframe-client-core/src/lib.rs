mod event;
mod ordered_map;
mod reassembly;
pub mod ack_batcher;
pub mod cdf53_coverage;
pub mod cdf53_prevalidate;
pub mod cdf53_tile_state;
pub mod decode_error_batcher;
pub mod fragment_parity;
pub mod loss_tracker;
pub mod nack_batcher;
pub mod pal_rle_decode;
pub mod palette_shadow;
pub mod parity_decoder;
pub use cdf53_coverage::CoverageEntry;
pub use event::{DecodeErrorCode, Event, PollOutput, TileKey};

use std::collections::{HashMap, VecDeque};

use ghostframe_protocol::protocol::TileHeader;

use ack_batcher::AckBatcher;
use cdf53_tile_state::Cdf53TileState;
use decode_error_batcher::DecodeErrorBatcher;
use fragment_parity::FragmentParity;
use loss_tracker::LossTracker;
use nack_batcher::NackBatcher;
use palette_shadow::PaletteShadow;
use parity_decoder::ParityDecoder;

/// Wire-sequence FEC window size (mirrors `new ParityDecoder(40)` in main.ts).
const PARITY_WINDOW: usize = 40;

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
}

impl Assembly {
    fn new(header: &TileHeader, frame_seq: u32, frag_total: u16) -> Self {
        Assembly {
            tile_x: header.tile_x,
            tile_y: header.tile_y,
            codec: header.codec,
            generation: header.generation,
            pass: header.pass,
            frame_seq,
            fragments: vec![None; frag_total as usize],
            received: 0,
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
}

impl ClientCore {
    pub fn new(config: ClientConfig, _now_us: u64) -> Self {
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
        };

        // Construct the Hello message [0x03, caps]
        // bit0 = indices_raw_enabled, bit1 = supports_h264
        let mut caps = 0u8;
        if config.indices_raw_enabled {
            caps |= 0x01;
        }
        if config.supports_h264 {
            caps |= 0x02;
        }
        core.outbox.push_back(PollOutput::Stream(vec![0x03, caps]));

        core
    }

    /// Drain one pending outbound message; call until None.
    pub fn poll_transmit(&mut self, _now_us: u64) -> Option<PollOutput> {
        self.outbox.pop_front()
    }

    /// Earliest deadline (µs) at which `on_timeout` must be called, if any.
    ///
    /// Only the ACK and NACK batcher flush deadlines are wired here.
    /// Assembly-timeout scans and the NACK tail sweep are Task 12.
    pub fn poll_timeout(&self) -> Option<u64> {
        match (self.ack_batcher.poll_timeout(), self.nack_batcher.poll_timeout()) {
            (Some(a), Some(n)) => Some(a.min(n)),
            (Some(a), None) => Some(a),
            (None, Some(n)) => Some(n),
            (None, None) => None,
        }
    }

    /// Fire any batcher deadlines that `now_us` has reached; flushed
    /// datagrams are queued to the outbox. Returns no render events.
    pub fn on_timeout(&mut self, now_us: u64) -> Vec<Event> {
        if let Some(dg) = self.ack_batcher.on_timeout(now_us) {
            self.outbox.push_back(PollOutput::Datagram(dg));
        }
        if let Some(dg) = self.nack_batcher.on_timeout(now_us) {
            self.outbox.push_back(PollOutput::Datagram(dg));
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
