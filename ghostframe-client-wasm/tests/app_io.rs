// SPDX-License-Identifier: MIT OR Apache-2.0
//! The stream pump and the logged writer in a real browser (port of
//! `server_stream.test.ts`). Run with `just test-web-renderer`.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_wasm::app::bootstrap::bootstrap;
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

/// Installs stub `fetch` and `WebTransport` globals; restores the originals on
/// drop, so a failing test cannot leak its stubs into the next one.
struct Stubs;

impl Stubs {
    /// `answers_js` is a JS array literal of `{ ok, status, json }`.
    fn install(answers_js: &str) -> Stubs {
        let setup = Function::new_with_args(
            "answers",
            "globalThis.__orig = { fetch: globalThis.fetch, wt: globalThis.WebTransport }; \
             globalThis.__answers = answers; globalThis.__fetches = []; globalThis.__wts = []; \
             globalThis.fetch = async (url, init) => { \
               globalThis.__fetches.push([url, init && init.cache]); \
               const a = globalThis.__answers.shift(); \
               return { ok: a.ok, status: a.status, json: async () => a.json }; }; \
             globalThis.WebTransport = class { constructor(url, opts) { \
               globalThis.__wts.push([url, opts]); } };",
        );
        let answers = Function::new_no_args(&format!("return {answers_js};"))
            .call0(&JsValue::NULL)
            .unwrap();
        setup.call1(&JsValue::NULL, &answers).unwrap();
        Stubs
    }

    /// Evaluates a JS expression over the recorded `__fetches` / `__wts`.
    fn eval(&self, expr: &str) -> JsValue {
        Function::new_no_args(&format!("return {expr};"))
            .call0(&JsValue::NULL)
            .unwrap()
    }
}

impl Drop for Stubs {
    fn drop(&mut self) {
        let _ = Function::new_no_args(
            "const o = globalThis.__orig; if (!o) return; \
             globalThis.fetch = o.fetch; globalThis.WebTransport = o.wt; \
             delete globalThis.__orig; delete globalThis.__answers; \
             delete globalThis.__fetches; delete globalThis.__wts;",
        )
        .call0(&JsValue::NULL);
    }
}

const HASH_00: &str = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";

#[wasm_bindgen_test]
async fn bootstrap_opens_a_same_origin_low_latency_transport_with_the_pin() {
    let stubs = Stubs::install(&format!(
        "[{{ ok: true, status: 200, json: {{ certHash: '{HASH_00}' }} }}]"
    ));
    bootstrap().await.unwrap();
    let host = web_sys::window().unwrap().location().host().unwrap();
    assert_eq!(
        stubs.eval("__wts[0][0]").as_string().unwrap(),
        format!("https://{host}/")
    );
    assert_eq!(
        stubs
            .eval("__wts[0][1].congestionControl")
            .as_string()
            .unwrap(),
        "low-latency"
    );
    assert_eq!(
        stubs
            .eval("__wts[0][1].serverCertificateHashes.length")
            .as_f64(),
        Some(1.0)
    );
    assert_eq!(
        stubs
            .eval("__wts[0][1].serverCertificateHashes[0].algorithm")
            .as_string()
            .unwrap(),
        "sha-256"
    );
    let v = stubs.eval("Array.from(new Uint8Array(__wts[0][1].serverCertificateHashes[0].value))");
    let bytes: Vec<u8> = Array::from(&v)
        .iter()
        .map(|b| b.as_f64().unwrap() as u8)
        .collect();
    assert_eq!(
        (bytes.len(), bytes[0], bytes[1], bytes[31]),
        (32, 0x00, 0x11, 0xff)
    );
    assert_eq!(
        stubs.eval("__fetches[0][0]").as_string().unwrap(),
        "/config.json"
    );
    assert_eq!(
        stubs.eval("__fetches[0][1]").as_string().unwrap(),
        "no-store"
    );
}

#[wasm_bindgen_test]
async fn bootstrap_pins_every_listed_hash() {
    let (a, b) = ("aa".repeat(32), "bb".repeat(32));
    let stubs = Stubs::install(&format!(
        "[{{ ok: true, status: 200, json: {{ certHashes: ['{a}', '{b}'] }} }}]"
    ));
    bootstrap().await.unwrap();
    assert_eq!(
        stubs
            .eval("__wts[0][1].serverCertificateHashes.length")
            .as_f64(),
        Some(2.0)
    );
    let first = stubs.eval("new Uint8Array(__wts[0][1].serverCertificateHashes[0].value)[0]");
    let second = stubs.eval("new Uint8Array(__wts[0][1].serverCertificateHashes[1].value)[0]");
    assert_eq!(
        (first.as_f64(), second.as_f64()),
        (Some(170.0), Some(187.0))
    );
}

#[wasm_bindgen_test]
async fn bootstrap_fetches_afresh_on_every_call() {
    let (a, b) = ("11".repeat(32), "22".repeat(32));
    let stubs = Stubs::install(&format!(
        "[{{ ok: true, status: 200, json: {{ certHash: '{a}' }} }}, \
          {{ ok: true, status: 200, json: {{ certHash: '{b}' }} }}]"
    ));
    bootstrap().await.unwrap();
    bootstrap().await.unwrap();
    assert_eq!(stubs.eval("__fetches.length").as_f64(), Some(2.0));
    assert_eq!(
        stubs.eval("__fetches[0][1]").as_string().unwrap(),
        "no-store"
    );
    assert_eq!(
        stubs.eval("__fetches[1][1]").as_string().unwrap(),
        "no-store"
    );
    assert_eq!(
        stubs
            .eval("new Uint8Array(__wts[1][1].serverCertificateHashes[0].value)[0]")
            .as_f64(),
        Some(34.0)
    );
}

#[wasm_bindgen_test]
async fn bootstrap_names_http_errors() {
    let _stubs = Stubs::install("[{ ok: false, status: 500, json: {} }]");
    let err = bootstrap().await.err().expect("must fail");
    assert!(err.contains("/config.json returned HTTP 500"), "{err}");
}

#[wasm_bindgen_test]
async fn bootstrap_names_a_missing_cert_hash() {
    let _stubs = Stubs::install("[{ ok: true, status: 200, json: {} }]");
    let err = bootstrap().await.err().expect("must fail");
    assert!(err.contains("certHash"), "{err}");
}

// ---- page DOM (src/app/dom.rs) ----

mod page_dom {
    use ghostframe_client_wasm::app::dom::{
        pin_canvas, show_disconnected_overlay, PageLog, LOG_LINES,
    };
    use wasm_bindgen::JsCast;
    use wasm_bindgen_test::*;
    use web_sys::{Document, HtmlCanvasElement, HtmlElement};

    fn document() -> Document {
        web_sys::window().unwrap().document().unwrap()
    }

    /// A fresh container holding `#status` and `#log`; removed by `drop`.
    struct Fixture {
        root: HtmlElement,
    }

    impl Fixture {
        fn new() -> Self {
            let doc = document();
            let root: HtmlElement = doc.create_element("div").unwrap().unchecked_into();
            root.set_inner_html("<div id=\"status\"></div><div id=\"log\"></div>");
            doc.body().unwrap().append_child(&root).unwrap();
            Fixture { root }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.root.remove();
        }
    }

    #[wasm_bindgen_test]
    fn the_page_log_keeps_the_last_50_lines() {
        let _f = Fixture::new();
        let doc = document();
        let log = PageLog::find(&doc).unwrap();
        for i in 0..60 {
            log.line(&format!("line {i}"));
        }
        let el = doc.get_element_by_id("log").unwrap();
        assert_eq!(el.child_element_count(), LOG_LINES);
        assert_eq!(LOG_LINES, 50);
        assert_eq!(
            el.first_element_child().unwrap().text_content().unwrap(),
            "line 10"
        );
        assert_eq!(
            el.last_element_child().unwrap().text_content().unwrap(),
            "line 59"
        );
        log.status("hello");
        assert_eq!(
            doc.get_element_by_id("status")
                .unwrap()
                .text_content()
                .unwrap(),
            "hello"
        );
    }

    #[wasm_bindgen_test]
    fn the_overlay_is_shown_once_with_role_status() {
        let doc = document();
        show_disconnected_overlay(&doc, "first");
        show_disconnected_overlay(&doc, "second");
        let all = doc.query_selector_all("#gf-disconnected").unwrap();
        let el = doc.get_element_by_id("gf-disconnected").unwrap();
        el.remove();
        assert_eq!(all.length(), 1);
        assert_eq!(el.text_content().unwrap(), "first");
        assert_eq!(el.get_attribute("role").as_deref(), Some("status"));
    }

    #[wasm_bindgen_test]
    fn lossless_hides_the_chrome_and_pins_the_canvas() {
        let _f = Fixture::new();
        let doc = document();
        let log = PageLog::find(&doc).unwrap();
        log.hide();
        for id in ["status", "log"] {
            let el: HtmlElement = doc.get_element_by_id(id).unwrap().unchecked_into();
            assert_eq!(el.style().get_property_value("display").unwrap(), "none");
        }
        let canvas: HtmlCanvasElement = doc.create_element("canvas").unwrap().unchecked_into();
        pin_canvas(&canvas);
        let s = canvas.style();
        assert_eq!(s.get_property_value("position").unwrap(), "fixed");
        assert_eq!(s.get_property_value("top").unwrap(), "0px");
        assert_eq!(s.get_property_value("left").unwrap(), "0px");
    }
}
