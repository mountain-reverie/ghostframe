// SPDX-License-Identifier: MIT OR Apache-2.0
//! `VideoCodec` over the browser's WebCodecs `VideoDecoder`.

use std::rc::Rc;

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

use crate::app::decoder::VideoCodec;
use crate::app::js::{EncodedVideoChunk, VideoDecoder};
use crate::web::raw::{js_message, obj};

/// Called with the generation of the decoder that produced it and the
/// `VideoFrame` (whose `timestamp` is the chunk's `frame_seq`).
pub type OnOutput = Rc<dyn Fn(u32, JsValue)>;
/// Called with the generation of the decoder that failed and the message.
pub type OnError = Rc<dyn Fn(u32, String)>;

type Callbacks = (Closure<dyn FnMut(JsValue)>, Closure<dyn FnMut(JsValue)>);

pub struct WebCodec {
    decoder: Option<VideoDecoder>,
    on_output: OnOutput,
    on_error: OnError,
    /// Every generation's callbacks, kept until this codec is dropped: a late
    /// callback from a replaced decoder must find its closure alive (a dropped
    /// one throws, and the frame it carried would never be closed). Rebuilds
    /// happen only after a decode failure, so this stays small.
    callbacks: Vec<Callbacks>,
}

impl WebCodec {
    pub fn new(on_output: OnOutput, on_error: OnError) -> Self {
        WebCodec {
            decoder: None,
            on_output,
            on_error,
            callbacks: Vec::new(),
        }
    }
}

/// `frame.timestamp` as the `frame_seq` it carries.
pub fn frame_seq_of(frame: &JsValue) -> u32 {
    Reflect::get(frame, &"timestamp".into())
        .ok()
        .and_then(|t| t.as_f64())
        .map_or(0, |t| t as u32)
}

impl VideoCodec for WebCodec {
    fn open(&mut self, generation: u32) {
        let out = self.on_output.clone();
        let output = Closure::new(move |frame: JsValue| out(generation, frame));
        let err = self.on_error.clone();
        let error = Closure::new(move |e: JsValue| err(generation, js_message(&e)));
        let init = obj(&[
            ("output", output.as_ref().clone()),
            ("error", error.as_ref().clone()),
        ]);
        // A constructor that throws leaves no decoder: `is_closed` reports
        // closed, so the next key chunk tries again.
        self.decoder = VideoDecoder::new(&init).ok();
        self.callbacks.push((output, error));
    }

    fn configure(&mut self, codec: &str, coded_size: (u32, u32)) -> Result<(), String> {
        let d = self.decoder.as_ref().ok_or("no VideoDecoder")?;
        let config = obj(&[
            ("codec", codec.into()),
            ("codedWidth", coded_size.0.into()),
            ("codedHeight", coded_size.1.into()),
            ("optimizeForLatency", true.into()),
        ]);
        d.configure(&config).map_err(|e| js_message(&e))
    }

    fn decode(&mut self, key: bool, timestamp: u32, data: &[u8]) -> Result<(), String> {
        let d = self.decoder.as_ref().ok_or("no VideoDecoder")?;
        let init = obj(&[
            ("type", if key { "key" } else { "delta" }.into()),
            ("timestamp", timestamp.into()),
            ("data", Uint8Array::from(data).into()),
        ]);
        let chunk = EncodedVideoChunk::new(&init).map_err(|e| js_message(&e))?;
        d.decode(&chunk).map_err(|e| js_message(&e))
    }

    fn is_closed(&self) -> bool {
        self.decoder.as_ref().is_none_or(|d| d.state() == "closed")
    }

    fn close(&mut self) {
        if let Some(d) = &self.decoder {
            let _ = d.close();
        }
    }
}
