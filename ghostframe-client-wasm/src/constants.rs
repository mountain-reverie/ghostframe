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
    loss_tracker::{HELLO_MSG_TYPE, HELLO_SIZE},
    nack_batcher::{FLUSH_INTERVAL_US as NACK_FLUSH_INTERVAL_US, NACK_BATCH_MAX},
};
use ghostframe_protocol::ack::{
    ACK_BATCH_MSG_TYPE, ACK_ENTRY_SIZE, ACK_OVERLAP_COUNT, MAX_FRESH_ENTRIES_PER_BATCH,
};
use ghostframe_protocol::protocol::{TILE_NACK_ENVELOPE, TILE_PARITY_ENVELOPE};
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
