// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading and writing the WebTransport streams.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{ReadableStream, ReadableStreamDefaultReader, WritableStreamDefaultWriter};

use crate::app::js::StreamPair;
use crate::app::repair_status::{RepairStatusPipe, Step};
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

/// Opens a bidirectional stream (the session's `createBidirectionalStream`);
/// a test hands in one that makes fake streams.
pub type OpenStream = Rc<dyn Fn() -> js_sys::Promise>;

struct RepairInner {
    pipe: RepairStatusPipe,
    writer: Option<WritableStreamDefaultWriter>,
    open: OpenStream,
    warned_open: bool,
    warned_write: bool,
}

/// The client-opened repair-status stream (FEC v2 rev 6): `PollOutput::
/// RepairStatus` chunks go here, never to the feedback stream. Opened on the
/// first chunk; after a failed open or write it is reopened by the next chunk
/// and the tag is written first (see `RepairStatusPipe`). Chunks reach the
/// stream in the order they were handed in.
#[derive(Clone)]
pub struct RepairStatusStream(Rc<RefCell<RepairInner>>);

impl RepairStatusStream {
    pub fn new(open: OpenStream) -> Self {
        RepairStatusStream(Rc::new(RefCell::new(RepairInner {
            pipe: RepairStatusPipe::default(),
            writer: None,
            open,
            warned_open: false,
            warned_write: false,
        })))
    }

    pub fn write(&self, bytes: &[u8]) {
        let step = self.0.borrow_mut().pipe.push(bytes.to_vec());
        self.run(step);
    }

    fn run(&self, step: Step) {
        match step {
            Step::Nothing => {}
            Step::Open(generation) => {
                let this = self.clone();
                let opening = (self.0.borrow().open)();
                spawn_local(async move {
                    let opened = JsFuture::from(opening)
                        .await
                        .map_err(|e| js_message(&e))
                        .and_then(|b| {
                            b.unchecked_into::<StreamPair>()
                                .writable()
                                .get_writer()
                                .map_err(|e| js_message(&e))
                        });
                    let step = {
                        let mut i = this.0.borrow_mut();
                        match opened {
                            Ok(w) => {
                                i.writer = Some(w);
                                i.pipe.opened(generation)
                            }
                            Err(e) => {
                                i.pipe.open_failed(generation);
                                i.writer = None;
                                if !std::mem::replace(&mut i.warned_open, true) {
                                    web_sys::console::warn_1(
                                        &format!("repair-status stream unavailable: {e}").into(),
                                    );
                                }
                                Step::Nothing
                            }
                        }
                    };
                    this.run(step);
                });
            }
            Step::Write(generation, chunks) => {
                let Some(writer) = self.0.borrow().writer.clone() else {
                    return;
                };
                for chunk in chunks {
                    // Called now, so the writer queues the chunks in order.
                    let promise = writer.write_with_chunk(&Uint8Array::from(chunk.as_slice()));
                    let this = self.clone();
                    spawn_local(async move {
                        if let Err(e) = JsFuture::from(promise).await {
                            let mut i = this.0.borrow_mut();
                            i.pipe.write_failed(generation);
                            if !std::mem::replace(&mut i.warned_write, true) {
                                web_sys::console::warn_1(
                                    &format!(
                                        "repair-status stream write failed: {}",
                                        js_message(&e)
                                    )
                                    .into(),
                                );
                            }
                        }
                    });
                }
            }
        }
    }
}
