// SPDX-License-Identifier: MIT OR Apache-2.0
//! The repair-status stream in a real browser, over fake bidirectional
//! streams: the first chunk opens it, a failed write drops it, and the next
//! chunk opens a new one that starts with the tag. Run with
//! `just test-web-renderer`.
#![cfg(target_arch = "wasm32")]

use std::cell::Cell;
use std::rc::Rc;

use ghostframe_client_wasm::app::streams::RepairStatusStream;
use ghostframe_protocol::fec::repair_status::FEC_REPAIR_STATUS_STREAM_TAG as TAG;
use js_sys::{Array, Function, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

async fn sleep(ms: i32) {
    let p = Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(p).await.unwrap();
}

/// `{ readable, writable }` whose writable records every chunk into
/// `globalThis[name]` and, once `globalThis[name + "_fail"]` is true, rejects
/// every write.
fn fake_bidi(name: &str) -> Promise {
    let make = Function::new_with_args(
        "name",
        "globalThis[name] = []; \
         return Promise.resolve({ readable: new ReadableStream(), \
           writable: new WritableStream({ write(chunk) { \
             if (globalThis[name + '_fail']) throw new Error('reset'); \
             globalThis[name].push(Array.from(chunk)); } }) });",
    );
    make.call1(&JsValue::NULL, &JsValue::from_str(name))
        .unwrap()
        .unchecked_into()
}

fn recorded(name: &str) -> Vec<Vec<u8>> {
    let all = Reflect::get(&js_sys::global(), &JsValue::from_str(name)).unwrap();
    Array::from(&all)
        .iter()
        .map(|c| {
            Array::from(&c)
                .iter()
                .map(|b| b.as_f64().unwrap() as u8)
                .collect()
        })
        .collect()
}

#[wasm_bindgen_test]
async fn a_failed_write_reopens_the_stream_and_the_new_one_starts_with_the_tag() {
    let opened = Rc::new(Cell::new(0u32));
    let count = opened.clone();
    let stream = RepairStatusStream::new(Rc::new(move || {
        count.set(count.get() + 1);
        fake_bidi(&format!("__rs{}", count.get()))
    }));

    // Nothing is opened until the core has something to say.
    assert_eq!(opened.get(), 0);
    // The core's first chunk carries the tag; two chunks arrive before the
    // open resolves and must stay in order.
    stream.write(&[TAG, 1]);
    stream.write(&[2]);
    sleep(20).await;
    assert_eq!(opened.get(), 1);
    assert_eq!(recorded("__rs1"), vec![vec![TAG, 1], vec![2]]);

    // The stream dies: the failing write is lost; so is the one behind it.
    Reflect::set(&js_sys::global(), &"__rs1_fail".into(), &true.into()).unwrap();
    stream.write(&[3]);
    stream.write(&[4]);
    sleep(20).await;
    assert_eq!(opened.get(), 1, "a failure alone opens nothing");

    // The next chunk opens a fresh stream, tag first. The two failures above
    // must not count against it.
    stream.write(&[5]);
    stream.write(&[6]);
    sleep(20).await;
    assert_eq!(opened.get(), 2);
    assert_eq!(recorded("__rs2"), vec![vec![TAG], vec![5], vec![6]]);
}
