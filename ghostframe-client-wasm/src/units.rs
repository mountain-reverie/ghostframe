//! Thin per-unit exports, existing so the pre-cutover vitest suites can be
//! retargeted at Rust without being rewritten as integration tests. Those
//! suites encode the *old* implementation's real behaviour, which is what
//! lets them detect divergence that tests written alongside the new code
//! cannot.
//!
//! Nothing in `main.ts` should use these — it drives `WasmClientCore`.

use ghostframe_client_core::{
    ack_batcher::AckBatcher,
    decode_error_batcher::DecodeErrorBatcher,
    nack_batcher::{NackBatcher, NackEntry},
    DecodeErrorCode,
};
use ghostframe_protocol::ack::{AckBatch, AckEntry};
use ghostframe_protocol::protocol::Codec;
use wasm_bindgen::prelude::*;

use crate::boundary::WasmAckEntry;

// `Option<Vec<u8>>` crosses the boundary as `Uint8Array | undefined`, so the
// batchers' natural return type needs no adaptation.

/// `None` for an unrecognised discriminant. Callers return `undefined`
/// rather than panicking: a panic aborts the whole wasm module.
///
/// `Codec::from_u8` (ghostframe-protocol/src/protocol.rs:68) returns
/// `Result<Codec, ProtocolError>`; the error carries no information the
/// caller acts on, so it is discarded here.
fn codec_from_u8(v: u8) -> Option<Codec> {
    Codec::from_u8(v).ok()
}

fn decode_error_code_from_u8(v: u8) -> Option<DecodeErrorCode> {
    use DecodeErrorCode::*;
    Some(match v {
        1 => PayloadTooShort,
        2 => CountOutOfRange,
        3 => ThinUncachedPalette,
        4 => BundledTruncated,
        5 => IndexOob,
        6 => RleOvershoot,
        7 => RleUndershoot,
        8 => Cdf53BadPass,
        9 => Cdf53Truncated,
        10 => Cdf53RleLength,
        _ => return None,
    })
}

#[wasm_bindgen]
pub struct WasmAckBatcher {
    inner: AckBatcher,
}

#[wasm_bindgen]
impl WasmAckBatcher {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmAckBatcher {
        WasmAckBatcher {
            inner: AckBatcher::new(),
        }
    }

    /// Returns the encoded datagram when the fresh-entry cap forces an
    /// immediate flush, otherwise `undefined`.
    pub fn add(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        arrival_time_ms_lo16: u16,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        self.inner.add(
            AckEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                arrival_time_ms_lo16,
            },
            now_us,
        )
    }

    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.poll_timeout()
    }

    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>> {
        self.inner.on_timeout(now_us)
    }

    pub fn flush(&mut self) -> Option<Vec<u8>> {
        self.inner.flush()
    }
}

impl Default for WasmAckBatcher {
    fn default() -> Self {
        Self::new()
    }
}

/// Decodes an ACK envelope — the replacement for the TS suite's
/// `parseAckEnvelopeForTest`. Returns an array of entries with named fields,
/// or `null` on a malformed envelope. A malformed envelope is a **value**,
/// not an exception, so a suite can assert rejection without
/// `expect(...).toThrow`.
#[wasm_bindgen(js_name = parseAckEnvelope)]
pub fn parse_ack_envelope(bytes: &[u8]) -> Result<JsValue, JsValue> {
    let entries: Option<Vec<WasmAckEntry>> = AckBatch::decode(bytes)
        .ok()
        .map(|b| b.entries.iter().map(WasmAckEntry::from).collect());
    serde_wasm_bindgen::to_value(&entries).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
pub struct WasmNackBatcher {
    inner: NackBatcher,
}

#[wasm_bindgen]
impl WasmNackBatcher {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmNackBatcher {
        WasmNackBatcher {
            inner: NackBatcher::new(),
        }
    }

    pub fn add(
        &mut self,
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        pass_idx: u8,
        frag_idx: u8,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        self.inner.add(
            NackEntry {
                frame_seq,
                tile_x,
                tile_y,
                pass_idx,
                frag_idx,
            },
            now_us,
        )
    }

    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.poll_timeout()
    }

    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Option<Vec<u8>> {
        self.inner.on_timeout(now_us)
    }
}

impl Default for WasmNackBatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen]
pub struct WasmDecodeErrorBatcher {
    inner: DecodeErrorBatcher,
}

#[wasm_bindgen]
impl WasmDecodeErrorBatcher {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmDecodeErrorBatcher {
        WasmDecodeErrorBatcher {
            inner: DecodeErrorBatcher::new(),
        }
    }

    /// Returns the 5-byte stream message `[0x04, codec, tile_x, tile_y,
    /// code]` when allowed, `undefined` when rate-limited (per-key: <=1 per
    /// 1000 ms; global: <=32 per rolling 1000 ms).
    ///
    /// `codec` and `code` cross as their `repr(u8)` discriminants. An
    /// unrecognised value returns `undefined` rather than panicking — this
    /// is a wire-adjacent export and must not abort the module.
    pub fn report(
        &mut self,
        codec: u8,
        tile_x: u8,
        tile_y: u8,
        code: u8,
        now_us: u64,
    ) -> Option<Vec<u8>> {
        let codec = codec_from_u8(codec)?;
        let code = decode_error_code_from_u8(code)?;
        self.inner.report(codec, tile_x, tile_y, code, now_us)
    }
}

impl Default for WasmDecodeErrorBatcher {
    fn default() -> Self {
        Self::new()
    }
}
