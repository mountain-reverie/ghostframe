//! The session boundary. `main.ts` drives this at cutover.
//!
//! Every export returns a value JS can inspect; none may `unwrap` on
//! wire-derived data. A panic here aborts the whole module — every later
//! call traps — so one malformed datagram would end the session rather than
//! drop a frame.

use ghostframe_client_core::{ClientConfig, ClientCore, TileDelivery};
use wasm_bindgen::prelude::*;

use crate::boundary::{WasmEvent, WasmPollOutput};

/// Serialises `T` for JS, mapping serialisation failure to a `JsValue` error
/// rather than panicking.
fn to_js<T: serde::Serialize>(value: &T) -> Result<JsValue, JsValue> {
    serde_wasm_bindgen::to_value(value).map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
pub struct WasmClientCore {
    inner: ClientCore,
}

#[wasm_bindgen]
impl WasmClientCore {
    /// `tile_delivery_payload = true` gives the browser undecoded, validated
    /// payloads for the GPU (`TileDelivery::Payload`). Native consumers pass
    /// `false` and keep `Decoded`.
    #[wasm_bindgen(constructor)]
    pub fn new(
        indices_raw_enabled: bool,
        supports_h264: bool,
        tile_delivery_payload: bool,
        now_us: u64,
    ) -> WasmClientCore {
        let config = ClientConfig {
            indices_raw_enabled,
            supports_h264,
            tile_delivery: if tile_delivery_payload {
                TileDelivery::Payload
            } else {
                TileDelivery::Decoded
            },
        };
        WasmClientCore {
            inner: ClientCore::new(config, now_us),
        }
    }

    /// Feed one inbound datagram; returns the resulting events as an array.
    ///
    /// Inbound is datagrams-only: the browser's bidi stream carries outbound
    /// feedback, and the server sends tiles, dimensions, palettes and H.264
    /// access units over `transport.datagrams`.
    #[wasm_bindgen(js_name = handleDatagram)]
    pub fn handle_datagram(&mut self, bytes: &[u8], now_us: u64) -> Result<JsValue, JsValue> {
        let events = self.inner.handle_datagram(bytes, now_us);
        let mirrored: Vec<WasmEvent> = events.iter().map(WasmEvent::from).collect();
        to_js(&mirrored)
    }

    /// Fire due timers (ACK/NACK flush, assembly timeout, tail sweep,
    /// periodic feedback); returns the resulting events.
    #[wasm_bindgen(js_name = onTimeout)]
    pub fn on_timeout(&mut self, now_us: u64) -> Result<JsValue, JsValue> {
        let events = self.inner.on_timeout(now_us);
        let mirrored: Vec<WasmEvent> = events.iter().map(WasmEvent::from).collect();
        to_js(&mirrored)
    }

    /// Drain one pending outbound buffer; returns `undefined` when empty.
    /// Call until it returns `undefined`.
    #[wasm_bindgen(js_name = pollTransmit)]
    pub fn poll_transmit(&mut self, now_us: u64) -> Result<JsValue, JsValue> {
        match self.inner.poll_transmit(now_us) {
            Some(out) => to_js(&WasmPollOutput::from(out)),
            None => Ok(JsValue::UNDEFINED),
        }
    }

    /// Earliest µs deadline at which `onTimeout` must be called.
    ///
    /// Returns `undefined` only if the core has no armed deadline. In
    /// practice the tail-sweep and feedback deadlines are always armed, so
    /// this is always a number — but JS must not assume that.
    #[wasm_bindgen(js_name = pollTimeout)]
    pub fn poll_timeout(&self) -> Option<u64> {
        self.inner.poll_timeout()
    }

    /// Encode a receiver-feedback report for the bidi stream.
    #[wasm_bindgen(js_name = encodeFeedback)]
    pub fn encode_feedback(&mut self, now_us: u64) -> Vec<u8> {
        self.inner.encode_feedback(now_us)
    }
}
