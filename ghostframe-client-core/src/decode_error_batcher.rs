//! Rate-limited DECODE_ERROR emitter. Ports
//! `ghostframe-web-client/src/decode_error_batcher.ts`, but time is injected
//! (`now_us: u64`) instead of using `performance.now()`.
//!
//!   - per-(codec, tile_x, tile_y) cap: <= 1 emission per WINDOW_US
//!   - global cap: <= GLOBAL_CAP emissions per WINDOW_US (sliding window)
//!   - Entries above either cap are dropped silently.

use std::collections::{HashMap, VecDeque};

use ghostframe_protocol::protocol::Codec;

use crate::event::DecodeErrorCode;

/// Rate-limit window: 1000 ms in microseconds (mirrors `WINDOW_MS` in
/// decode_error_batcher.ts).
const WINDOW_US: u64 = 1_000_000;

/// Global cap on emissions per rolling window (mirrors `GLOBAL_CAP`).
const GLOBAL_CAP: usize = 32;

pub struct DecodeErrorBatcher {
    /// (codec, tile_x, tile_y) -> last-emit timestamp (µs).
    per_key: HashMap<(u8, u8, u8), u64>,
    /// Sliding window of emission timestamps (µs).
    recent_emits: VecDeque<u64>,
}

impl DecodeErrorBatcher {
    pub fn new() -> Self {
        DecodeErrorBatcher {
            per_key: HashMap::new(),
            recent_emits: VecDeque::new(),
        }
    }

    /// Returns the 5-byte stream message `[0x04, codec, tile_x, tile_y,
    /// code]` when allowed, `None` when rate-limited.
    pub fn report(
        &mut self,
        codec: Codec,
        tile_x: u8,
        tile_y: u8,
        code: DecodeErrorCode,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        // Trim sliding global window: drop entries at or before (t - WINDOW_US).
        while let Some(&front) = self.recent_emits.front() {
            if now_us >= WINDOW_US && front <= now_us - WINDOW_US {
                self.recent_emits.pop_front();
            } else {
                break;
            }
        }
        if self.recent_emits.len() >= GLOBAL_CAP {
            return None; // global cap exceeded — drop
        }

        let codec_byte = codec as u8;
        let key = (codec_byte, tile_x, tile_y);
        if let Some(&last) = self.per_key.get(&key) {
            if now_us >= last && now_us - last < WINDOW_US {
                return None; // per-key cap exceeded — drop
            }
        }

        self.per_key.insert(key, now_us);
        self.recent_emits.push_back(now_us);

        Some(vec![0x04, codec_byte, tile_x, tile_y, code as u8])
    }
}

impl Default for DecodeErrorBatcher {
    fn default() -> Self {
        Self::new()
    }
}
