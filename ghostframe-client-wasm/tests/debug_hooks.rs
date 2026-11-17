// SPDX-License-Identifier: MIT OR Apache-2.0
//! The `?e2e` window globals in a real browser. `install` mutates the page's
//! `window` and `console`, so these tests share one page: each compares
//! deltas, never absolute lengths. Run with `just test-web-renderer`.
#![cfg(target_arch = "wasm32")]

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use ghostframe_client_wasm::app::debug_hooks::{
    append, install, install_console_ring, DebugHandles, JsLog, Logs, E2E_GLOBALS,
};
use ghostframe_client_wasm::app::recorder::{LogEntry, Recorder};
use ghostframe_client_wasm::core::{client_config, new_client_core};
use ghostframe_client_wasm::web::renderer::WebRenderer;
use js_sys::{Array, Function, Promise, Reflect};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

fn window() -> web_sys::Window {
    web_sys::window().unwrap()
}

fn get(name: &str) -> JsValue {
    Reflect::get(&window(), &JsValue::from_str(name)).unwrap()
}

fn call0(name: &str) -> JsValue {
    get(name)
        .unchecked_into::<Function>()
        .call0(&JsValue::NULL)
        .unwrap()
}

async fn setup() -> (Logs, Rc<RefCell<Recorder>>) {
    let document = window().document().unwrap();
    let canvas: web_sys::HtmlCanvasElement =
        document.create_element("canvas").unwrap().unchecked_into();
    document.body().unwrap().append_child(&canvas).unwrap();
    let renderer = WebRenderer::create(canvas, "auto".into())
        .await
        .unwrap_or_else(|e| panic!("WebRenderer::create: {e:?}"));
    let core = Rc::new(RefCell::new(new_client_core(
        client_config(true, false, true),
        0,
    )));
    let cell = Rc::new(OnceCell::new());
    let _ = cell.set(core);
    let recorder = Rc::new(RefCell::new(Recorder::new(true)));
    let logs = install(
        &window(),
        DebugHandles {
            renderer: Rc::new(RefCell::new(renderer)),
            core: cell,
            recorder: recorder.clone(),
            tick_us: Rc::new(RefCell::new(Vec::new())),
        },
    );
    (logs, recorder)
}

fn len(name: &str) -> u32 {
    get(name).unchecked_into::<Array>().length()
}

#[wasm_bindgen_test]
async fn every_e2e_global_exists_with_its_shape() {
    let _ = setup().await;
    for name in E2E_GLOBALS {
        assert!(
            Reflect::has(&window(), &JsValue::from_str(name)).unwrap(),
            "{name} is not defined"
        );
    }
    for log in [
        "__ghostframeRecordedTiles",
        "__ghostframeRecordedCodecs",
        "__ghostframeRecordedFlags",
        "__ghostframeRecordedResizes",
        "__ghostframe_framePaints",
        "__h2_clientPaletteWrites",
        "__h5_tilePushLog",
    ] {
        assert!(Array::is_array(&get(log)), "{log} is not an array");
    }
    let stats = get("__ghostframeStats");
    for f in ["tileDatagrams", "frameDatagrams"] {
        let v = Reflect::get(&stats, &JsValue::from_str(f)).unwrap();
        assert!(v.as_f64().is_some(), "__ghostframeStats.{f} = {v:?}");
    }
    let health = call0("__ghostframeHealth");
    assert!(
        Reflect::get(&health, &"swallowed".into())
            .unwrap()
            .as_f64()
            .is_some(),
        "health has no numeric swallowed"
    );
    let rect = get("__readPixelRect")
        .unchecked_into::<Function>()
        .call4(&JsValue::NULL, &0.into(), &0.into(), &0.into(), &0.into())
        .unwrap();
    let px = JsFuture::from(rect.unchecked_into::<Promise>())
        .await
        .unwrap();
    assert_eq!(px.unchecked_into::<Array>().length(), 0);
}

#[wasm_bindgen_test]
async fn appends_reach_the_window_arrays_and_trim_at_twice_the_cap() {
    let (logs, _) = setup().await;
    let before = len("__ghostframeRecordedCodecs");
    append(&logs, vec![LogEntry::Codec(1), LogEntry::Codec(3)]);
    let arr: Array = get("__ghostframeRecordedCodecs").unchecked_into();
    assert_eq!(arr.length(), before + 2);
    assert_eq!(arr.get(before).as_f64(), Some(1.0));
    assert_eq!(arr.get(before + 1).as_f64(), Some(3.0));

    let log = JsLog::with_cap(4);
    for i in 0..7 {
        log.push(&JsValue::from(i));
    }
    assert_eq!(log.array().length(), 7);
    log.push(&JsValue::from(7));
    assert_eq!(log.array().length(), 4);
    let kept: Vec<f64> = log.array().iter().filter_map(|v| v.as_f64()).collect();
    assert_eq!(kept, vec![4.0, 5.0, 6.0, 7.0]);
}

#[wasm_bindgen_test]
async fn the_console_ring_mirrors_warn_and_error() {
    let (_, recorder) = setup().await;
    install_console_ring(&recorder);
    web_sys::console::warn_1(&"x".into());
    web_sys::console::error_1(&"y".into());
    let health = call0("__ghostframeHealth");
    let console: Array = Reflect::get(&health, &"console".into())
        .unwrap()
        .unchecked_into();
    let lines: Vec<String> = console.iter().filter_map(|v| v.as_string()).collect();
    assert_eq!(&lines[lines.len() - 2..], ["warn: x", "error: y"]);
}
