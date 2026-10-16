//! Protocol constants, re-exported so the retargeted vitest suites read
//! them from Rust rather than hardcoding them.
//!
//! Hardcoding would leave the suites green while a value drifted on the
//! Rust side, which is the exact failure these suites exist to catch.
//!
//! `wasm-bindgen` cannot export a `const`, so each is a getter.

use ghostframe_client_core::{
    ack_batcher::FLUSH_INTERVAL_US as ACK_FLUSH_INTERVAL_US,
    decode_error_batcher::{DECODE_ERROR_MSG_TYPE, DECODE_ERROR_SIZE},
    input::INPUT_MSG_TYPE,
    loss_tracker::{HELLO_MSG_TYPE, HELLO_SIZE},
    nack_batcher::{FLUSH_INTERVAL_US as NACK_FLUSH_INTERVAL_US, NACK_BATCH_MAX},
};
use ghostframe_protocol::ack::{
    ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT, MAX_FRESH_ENTRIES_PER_BATCH,
};
use ghostframe_protocol::codec::cdf53::CDF53_PASS_COUNT;
use ghostframe_protocol::codec::pal_rle::palette_fingerprint_bgra;
use ghostframe_protocol::eviction::{EvictionReason, EVICTION_SENTINEL_X, EVICTION_SENTINEL_Y};
use ghostframe_protocol::feedback_stream::{
    DISPLAY_INFO_MSG_TYPE, DISPLAY_MODE_MSG_TYPE, FEEDBACK_MSG_TYPE, HELLO_CAPS_RESERVED_MASK,
    HELLO_CAP_INDICES_RAW, HELLO_CAP_SUPPORTS_H264,
};
use ghostframe_protocol::protocol::{
    Codec, DATAGRAM_HEADER_SIZE, FRAME_DIMENSIONS_SENTINEL_X, FRAME_DIMENSIONS_SENTINEL_Y,
    FRAME_HEADER_SIZE, TILE_DATAGRAM_FLAG, TILE_HEADER_SIZE, TILE_NACK_ENVELOPE,
    TILE_PARITY_ENVELOPE,
};
use ghostframe_protocol::tile::{TILE_BYTES, TILE_SIZE};
use wasm_bindgen::prelude::*;

macro_rules! export_const {
    ($js:literal, $fn_name:ident, $ty:ty, $value:expr) => {
        #[wasm_bindgen(js_name = $js)]
        pub fn $fn_name() -> $ty {
            $value
        }
    };
}

export_const!(
    "ackBatchMsgType",
    ack_batch_msg_type,
    u8,
    ACK_BATCH_MSG_TYPE
);
export_const!("ackEntrySize", ack_entry_size, usize, ACK_ENTRY_SIZE);
export_const!(
    "ackOverlapCount",
    ack_overlap_count,
    usize,
    ACK_OVERLAP_COUNT
);
export_const!(
    "maxAckEntries",
    max_ack_entries,
    usize,
    MAX_FRESH_ENTRIES_PER_BATCH
);
export_const!(
    "ackFlushIntervalMs",
    ack_flush_interval_ms,
    u64,
    ACK_FLUSH_INTERVAL_US / 1000
);

export_const!(
    "tileNackEnvelope",
    tile_nack_envelope,
    u8,
    TILE_NACK_ENVELOPE
);
export_const!(
    "nackBatchFlushMs",
    nack_batch_flush_ms,
    u64,
    NACK_FLUSH_INTERVAL_US / 1000
);
export_const!("nackBatchMax", nack_batch_max, usize, NACK_BATCH_MAX);

export_const!(
    "tileParityEnvelope",
    tile_parity_envelope,
    u8,
    TILE_PARITY_ENVELOPE
);

export_const!("inputMsgType", input_msg_type, u8, INPUT_MSG_TYPE);
export_const!("helloMsgType", hello_msg_type, u8, HELLO_MSG_TYPE);
export_const!("helloSize", hello_size, usize, HELLO_SIZE);
export_const!(
    "decodeErrorMsgType",
    decode_error_msg_type,
    u8,
    DECODE_ERROR_MSG_TYPE
);
export_const!(
    "decodeErrorSize",
    decode_error_size,
    usize,
    DECODE_ERROR_SIZE
);

use ghostframe_client_core::DecodeErrorCode;
use serde::Serialize;

/// The `DecodeErrorCode` discriminants, mirroring the TS `ERR_*` constants
/// in `feedback.ts`. Exported as one object so a drifting discriminant
/// shows up in a single assertion rather than ten.
#[derive(Serialize)]
#[allow(non_snake_case)]
struct ErrorCodes {
    ERR_PAYLOAD_TOO_SHORT: u8,
    ERR_COUNT_OUT_OF_RANGE: u8,
    ERR_THIN_UNCACHED_PALETTE: u8,
    ERR_BUNDLED_TRUNCATED: u8,
    ERR_INDEX_OOB: u8,
    ERR_RLE_OVERSHOOT: u8,
    ERR_RLE_UNDERSHOOT: u8,
    ERR_CDF53_BAD_PASS: u8,
    ERR_CDF53_TRUNCATED: u8,
    ERR_CDF53_RLE_LENGTH: u8,
}

#[wasm_bindgen(js_name = errorCodes)]
pub fn error_codes() -> Result<JsValue, JsValue> {
    let codes = ErrorCodes {
        ERR_PAYLOAD_TOO_SHORT: DecodeErrorCode::PayloadTooShort as u8,
        ERR_COUNT_OUT_OF_RANGE: DecodeErrorCode::CountOutOfRange as u8,
        ERR_THIN_UNCACHED_PALETTE: DecodeErrorCode::ThinUncachedPalette as u8,
        ERR_BUNDLED_TRUNCATED: DecodeErrorCode::BundledTruncated as u8,
        ERR_INDEX_OOB: DecodeErrorCode::IndexOob as u8,
        ERR_RLE_OVERSHOOT: DecodeErrorCode::RleOvershoot as u8,
        ERR_RLE_UNDERSHOOT: DecodeErrorCode::RleUndershoot as u8,
        ERR_CDF53_BAD_PASS: DecodeErrorCode::Cdf53BadPass as u8,
        ERR_CDF53_TRUNCATED: DecodeErrorCode::Cdf53Truncated as u8,
        ERR_CDF53_RLE_LENGTH: DecodeErrorCode::Cdf53RleLength as u8,
    };
    serde_wasm_bindgen::to_value(&codes).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// The `PalRleVariant` discriminants, matching the mapping already used by
/// `WasmPrevalidatedPalRle` in `boundary.rs` (Bundled=0, Thin=1,
/// IndicesRaw=2). Must agree with that mapping — a mismatch here would make
/// every `prevalidate` variant assertion wrong in the same direction.
#[derive(Serialize)]
#[allow(non_snake_case)]
struct PalRleVariants {
    Bundled: u8,
    Thin: u8,
    IndicesRaw: u8,
}

#[wasm_bindgen(js_name = palRleVariants)]
pub fn pal_rle_variants() -> Result<JsValue, JsValue> {
    let variants = PalRleVariants {
        Bundled: 0,
        Thin: 1,
        IndicesRaw: 2,
    };
    serde_wasm_bindgen::to_value(&variants).map_err(|e| JsValue::from_str(&e.to_string()))
}

// ---- Feedback-stream message types the TS client does not own ----------
export_const!("feedbackMsgType", feedback_msg_type, u8, FEEDBACK_MSG_TYPE);
export_const!(
    "displayInfoMsgType",
    display_info_msg_type,
    u8,
    DISPLAY_INFO_MSG_TYPE
);
export_const!(
    "displayModeMsgType",
    display_mode_msg_type,
    u8,
    DISPLAY_MODE_MSG_TYPE
);

// ---- HELLO capability bits ---------------------------------------------
export_const!(
    "helloCapIndicesRaw",
    hello_cap_indices_raw,
    u8,
    HELLO_CAP_INDICES_RAW
);
export_const!(
    "helloCapSupportsH264",
    hello_cap_supports_h264,
    u8,
    HELLO_CAP_SUPPORTS_H264
);
export_const!(
    "helloCapsReservedMask",
    hello_caps_reserved_mask,
    u8,
    HELLO_CAPS_RESERVED_MASK
);

// ---- Datagram geometry -------------------------------------------------
export_const!(
    "datagramHeaderSize",
    datagram_header_size,
    usize,
    DATAGRAM_HEADER_SIZE
);
export_const!("tileHeaderSize", tile_header_size, usize, TILE_HEADER_SIZE);
export_const!(
    "frameHeaderSize",
    frame_header_size,
    usize,
    FRAME_HEADER_SIZE
);
export_const!(
    "tileDatagramFlag",
    tile_datagram_flag,
    u32,
    TILE_DATAGRAM_FLAG
);
export_const!("tileSize", tile_size, u32, TILE_SIZE);
export_const!("tileBytes", tile_bytes, usize, TILE_BYTES);
export_const!("cdf53PassCount", cdf53_pass_count, usize, CDF53_PASS_COUNT);

// ---- Sentinels and eviction -------------------------------------------
export_const!(
    "frameDimensionsSentinelX",
    frame_dimensions_sentinel_x,
    u8,
    FRAME_DIMENSIONS_SENTINEL_X
);
export_const!(
    "frameDimensionsSentinelY",
    frame_dimensions_sentinel_y,
    u8,
    FRAME_DIMENSIONS_SENTINEL_Y
);
export_const!(
    "evictionSentinelX",
    eviction_sentinel_x,
    u8,
    EVICTION_SENTINEL_X
);
export_const!(
    "evictionSentinelY",
    eviction_sentinel_y,
    u8,
    EVICTION_SENTINEL_Y
);
export_const!(
    "evictionReasonDisplaced",
    eviction_reason_displaced,
    u8,
    EvictionReason::DisplacedByNewSession.to_byte()
);

/// The `Codec` discriminants (Skip=0 .. Cdf53=5; 4 is unassigned), keyed by name.
#[derive(Serialize)]
#[allow(non_snake_case)]
struct Codecs {
    Skip: u8,
    H264: u8,
    PalRle: u8,
    Solid: u8,
    Cdf53: u8,
}

#[wasm_bindgen(js_name = codecs)]
pub fn codecs() -> Result<JsValue, JsValue> {
    let c = Codecs {
        Skip: Codec::Skip as u8,
        H264: Codec::H264 as u8,
        PalRle: Codec::PalRle as u8,
        Solid: Codec::Solid as u8,
        Cdf53: Codec::Cdf53 as u8,
    };
    serde_wasm_bindgen::to_value(&c).map_err(|e| JsValue::from_str(&e.to_string()))
}

/// The palette fingerprint the server logs for the same palette, over a
/// block of `count * 4` BGRA bytes. The web client's atlas-write
/// diagnostic calls this instead of keeping a TS copy that can drift.
#[wasm_bindgen(js_name = paletteFingerprint)]
pub fn palette_fingerprint(bgra: &[u8]) -> u32 {
    palette_fingerprint_bgra(bgra)
}
