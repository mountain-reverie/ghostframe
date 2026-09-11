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
    loss_tracker::LossTracker,
    nack_batcher::{NackBatcher, NackEntry},
    palette_shadow::PaletteShadow,
    parity_decoder::ParityDecoder,
    DecodeErrorCode,
};
use ghostframe_protocol::ack::{AckBatch, AckEntry};
use ghostframe_protocol::protocol::{Codec, TileParityEnvelope};
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
/// or `undefined` on a malformed envelope — `serde_wasm_bindgen` maps
/// `Option::None` to `undefined`, matching every other export here.
///
/// The TS `parseAckEnvelopeForTest` it replaces *throws* instead. Returning
/// a value is deliberate: it lets a suite assert rejection without
/// `expect(...).toThrow`. No existing test depends on either behaviour —
/// every TS call site passes a well-formed envelope the batcher just
/// produced, so the malformed path is currently unexercised.
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

#[wasm_bindgen]
pub struct WasmPaletteShadow {
    inner: PaletteShadow,
}

#[wasm_bindgen]
impl WasmPaletteShadow {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmPaletteShadow {
        WasmPaletteShadow {
            inner: PaletteShadow::new(),
        }
    }

    pub fn has(&self, id: u8) -> bool {
        self.inner.has(id)
    }

    pub fn count(&self, id: u8) -> u8 {
        self.inner.count(id)
    }

    pub fn put(&mut self, id: u8, count: u8) {
        self.inner.put(id, count)
    }

    pub fn clear(&mut self) {
        self.inner.clear()
    }
}

impl Default for WasmPaletteShadow {
    fn default() -> Self {
        Self::new()
    }
}

/// Rust-only accessor; a later task's `prevalidatePalRle` export needs the
/// wrapped shadow. Deliberately not `#[wasm_bindgen]`.
#[allow(dead_code)]
impl WasmPaletteShadow {
    pub(crate) fn inner_ref(&self) -> &PaletteShadow {
        &self.inner
    }
}

#[wasm_bindgen]
pub struct WasmParityDecoder {
    inner: ParityDecoder,
}

#[wasm_bindgen]
impl WasmParityDecoder {
    #[wasm_bindgen(constructor)]
    pub fn new(window_capacity: usize) -> WasmParityDecoder {
        WasmParityDecoder {
            inner: ParityDecoder::new(window_capacity),
        }
    }

    #[wasm_bindgen(js_name = hasSource)]
    pub fn has_source(&self, wire_seq: u32) -> bool {
        self.inner.has_source(wire_seq)
    }

    /// Returns a recovered source datagram if this arrival unlocked a
    /// buffered parity, otherwise `undefined`.
    #[wasm_bindgen(js_name = recordSource)]
    pub fn record_source(&mut self, wire_seq: u32, bytes: &[u8]) -> Option<Vec<u8>> {
        self.inner.record_source(wire_seq, bytes)
    }

    /// Takes the raw envelope bytes and parses internally, mirroring the TS
    /// suite's `parseParityEnvelope` + `receiveParity` pairing.
    ///
    /// Returns `undefined` both for a malformed envelope and for a
    /// well-formed one that recovers nothing — the suite distinguishes those
    /// by also asserting on `hasSource`.
    #[wasm_bindgen(js_name = receiveParity)]
    pub fn receive_parity(&mut self, envelope_bytes: &[u8]) -> Option<Vec<u8>> {
        let env = TileParityEnvelope::decode(envelope_bytes).ok()?;
        self.inner.receive_parity(&env)
    }
}

#[wasm_bindgen]
pub struct WasmLossTracker {
    inner: LossTracker,
}

#[wasm_bindgen]
impl WasmLossTracker {
    #[wasm_bindgen(constructor)]
    pub fn new() -> WasmLossTracker {
        WasmLossTracker {
            inner: LossTracker::new(),
        }
    }

    #[wasm_bindgen(js_name = onDatagram)]
    pub fn on_datagram(&mut self, now_us: u64) {
        self.inner.on_datagram(now_us)
    }

    #[wasm_bindgen(js_name = onStaleTile)]
    pub fn on_stale_tile(&mut self, expected: usize, received: usize) {
        self.inner.on_stale_tile(expected, received)
    }

    #[wasm_bindgen(js_name = onFecRecovery)]
    pub fn on_fec_recovery(&mut self) {
        self.inner.on_fec_recovery()
    }

    #[wasm_bindgen(js_name = encodeFeedback)]
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.inner.encode_feedback(now_us)
    }
}

impl Default for WasmLossTracker {
    fn default() -> Self {
        Self::new()
    }
}
