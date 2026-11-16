// SPDX-License-Identifier: MIT OR Apache-2.0
//! The stream pump and the logged writer in a real browser (port of
//! `server_stream.test.ts`). Run with `just test-web-renderer`.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_wasm::app::streams::{pump_stream, LoggedWriter};
use js_sys::{Array, Function, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

fn stream_of(chunks: &[&[u8]], fail: bool) -> web_sys::ReadableStream {
    let arr = Array::new();
    for c in chunks {
        arr.push(&Uint8Array::from(*c));
    }
    let make = Function::new_with_args(
        "chunks, fail",
        "let i = 0; return new ReadableStream({ pull(c) { \
           if (i < chunks.length) c.enqueue(chunks[i++]); \
           else if (fail) c.error(new Error('reset')); else c.close(); } });",
    );
    make.call2(&JsValue::NULL, &arr, &JsValue::from_bool(fail))
        .unwrap()
        .unchecked_into()
}

/// Pumps `readable`, refusing every chunk whose first byte is in `refuse`.
async fn run(readable: web_sys::ReadableStream, refuse: &[u8]) -> (Vec<Vec<u8>>, usize) {
    let seen = Rc::new(RefCell::new(Vec::new()));
    let warnings = Rc::new(RefCell::new(0usize));
    let (s, w) = (seen.clone(), warnings.clone());
    pump_stream(
        readable,
        move |c| {
            if refuse.contains(&c[0]) {
                return Err("refused".into());
            }
            s.borrow_mut().push(c.to_vec());
            Ok(())
        },
        move |_, _| *w.borrow_mut() += 1,
    )
    .await;
    let seen = seen.borrow().clone();
    let warnings = *warnings.borrow();
    (seen, warnings)
}

#[wasm_bindgen_test]
async fn the_pump_delivers_every_non_empty_chunk_in_order() {
    let (seen, warnings) = run(stream_of(&[&[1, 2], &[], &[3]], false), &[]).await;
    assert_eq!(seen, vec![vec![1, 2], vec![3]]);
    assert_eq!(warnings, 0);
}

#[wasm_bindgen_test]
async fn the_pump_keeps_reading_after_a_chunk_is_refused() {
    let (seen, warnings) = run(stream_of(&[&[1], &[2], &[3]], false), &[2]).await;
    assert_eq!(seen, vec![vec![1], vec![3]]);
    assert_eq!(warnings, 1);
}

#[wasm_bindgen_test]
async fn the_pump_ends_with_a_warning_when_the_stream_errors() {
    let (seen, warnings) = run(stream_of(&[&[1]], true), &[]).await;
    assert_eq!(seen, vec![vec![1]]);
    assert_eq!(warnings, 1);
}

#[wasm_bindgen_test]
async fn a_logged_writer_writes_in_order() {
    let make = Function::new_no_args(
        "globalThis.__w = []; return new WritableStream({ write(c) { globalThis.__w.push(Array.from(c)); } });",
    );
    let writable: web_sys::WritableStream = make.call0(&JsValue::NULL).unwrap().unchecked_into();
    let writer = LoggedWriter::new(writable.get_writer().unwrap(), "test writer");
    writer.write(&[1]);
    writer.write(&[2, 3]);
    let tick = Function::new_no_args("return new Promise(r => setTimeout(r, 0));");
    JsFuture::from(
        tick.call0(&JsValue::NULL)
            .unwrap()
            .unchecked_into::<js_sys::Promise>(),
    )
    .await
    .unwrap();
    let got = Reflect::get(&js_sys::global(), &"__w".into()).unwrap();
    let json = js_sys::JSON::stringify(&got).unwrap().as_string().unwrap();
    assert_eq!(json, "[[1],[2,3]]");
}
