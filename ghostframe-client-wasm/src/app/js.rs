// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser APIs web-sys 0.3.106 has only behind `web_sys_unstable_apis`
//! (WebTransport, WebCodecs), declared directly: the members this client
//! uses and nothing else. Same mechanism and reason as `web/raw.rs`.

use js_sys::{Object, Promise};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Object)]
    pub type WebTransport;
    #[wasm_bindgen(constructor, catch)]
    pub fn new(url: &str, options: &JsValue) -> Result<WebTransport, JsValue>;
    #[wasm_bindgen(method, getter)]
    pub fn ready(this: &WebTransport) -> Promise;
    /// Resolves with `{ closeCode, reason }`; rejects when the session failed.
    #[wasm_bindgen(method, getter)]
    pub fn closed(this: &WebTransport) -> Promise;
    #[wasm_bindgen(method, getter)]
    pub fn datagrams(this: &WebTransport) -> StreamPair;
    /// Resolves with a `{ readable, writable }` bidirectional stream.
    #[wasm_bindgen(method, js_name = createBidirectionalStream)]
    pub fn create_bidirectional_stream(this: &WebTransport) -> Promise;
}

#[wasm_bindgen]
extern "C" {
    /// `{ readable, writable }`: the datagram duplex stream or a bidi stream.
    #[wasm_bindgen(extends = Object)]
    pub type StreamPair;
    #[wasm_bindgen(method, getter)]
    pub fn readable(this: &StreamPair) -> web_sys::ReadableStream;
    #[wasm_bindgen(method, getter)]
    pub fn writable(this: &StreamPair) -> web_sys::WritableStream;
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Object)]
    pub type VideoDecoder;
    /// `init` is `{ output(frame), error(e) }`.
    #[wasm_bindgen(constructor, catch)]
    pub fn new(init: &JsValue) -> Result<VideoDecoder, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn configure(this: &VideoDecoder, config: &JsValue) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn decode(this: &VideoDecoder, chunk: &EncodedVideoChunk) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn close(this: &VideoDecoder) -> Result<(), JsValue>;
    /// `"unconfigured"`, `"configured"` or `"closed"`.
    #[wasm_bindgen(method, getter)]
    pub fn state(this: &VideoDecoder) -> String;
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Object)]
    pub type EncodedVideoChunk;
    /// `init` is `{ type: "key" | "delta", timestamp, data }`.
    #[wasm_bindgen(constructor, catch)]
    pub fn new(init: &JsValue) -> Result<EncodedVideoChunk, JsValue>;
}
