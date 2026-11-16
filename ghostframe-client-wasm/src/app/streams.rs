// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading and writing the WebTransport streams.

use std::cell::Cell;
use std::rc::Rc;

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{ReadableStream, ReadableStreamDefaultReader, WritableStreamDefaultWriter};

use crate::web::raw::js_message;

/// Reads `readable` to its end and hands each non-empty chunk to `on_chunk`
/// (port of `server_stream.ts`). A chunk `on_chunk` refuses is warned about
/// and skipped, since the next one may be fine; an error ends the read with a
/// warning. Returns when the stream ends (a closing session ends it).
pub async fn pump_stream(
    readable: ReadableStream,
    mut on_chunk: impl FnMut(&[u8]) -> Result<(), String>,
    mut warn: impl FnMut(&str, &str),
) {
    let reader: ReadableStreamDefaultReader = readable.get_reader().unchecked_into();
    loop {
        let result = match JsFuture::from(reader.read()).await {
            Ok(r) => r,
            Err(e) => {
                warn("server stream ended with an error", &js_message(&e));
                break;
            }
        };
        if Reflect::get(&result, &"done".into())
            .ok()
            .and_then(|d| d.as_bool())
            == Some(true)
        {
            break;
        }
        let Ok(value) = Reflect::get(&result, &"value".into()) else {
            continue;
        };
        let Some(chunk) = value.dyn_ref::<Uint8Array>() else {
            continue;
        };
        if chunk.length() == 0 {
            continue;
        }
        if let Err(e) = on_chunk(&chunk.to_vec()) {
            warn("server stream: chunk handler failed; continuing", &e);
        }
    }
    reader.release_lock();
}

/// A stream writer whose failures are logged once, as `what: error`: a
/// feedback stream that closes mid-session would otherwise log every write.
/// Writes are not awaited: the writer queues them in call order.
pub struct LoggedWriter {
    writer: WritableStreamDefaultWriter,
    on_error: Closure<dyn FnMut(JsValue)>,
}

impl LoggedWriter {
    pub fn new(writer: WritableStreamDefaultWriter, what: &'static str) -> Self {
        let logged = Rc::new(Cell::new(false));
        let on_error = Closure::new(move |e: JsValue| {
            if !logged.replace(true) {
                web_sys::console::warn_1(&format!("{what}: {}", js_message(&e)).into());
            }
        });
        LoggedWriter { writer, on_error }
    }

    /// The same stream, with its own once-only failure line.
    pub fn labelled(&self, what: &'static str) -> Self {
        LoggedWriter::new(self.writer.clone(), what)
    }

    pub fn write(&self, bytes: &[u8]) {
        let promise = self.writer.write_with_chunk(&Uint8Array::from(bytes));
        let _ = promise.catch(&self.on_error);
    }
}
