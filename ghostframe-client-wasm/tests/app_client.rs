// SPDX-License-Identifier: MIT OR Apache-2.0
//! The client's main loop in a real browser, over fake streams: what the tick
//! does, in what order the hooks hear it, and which bytes leave. Run with
//! `just test-web-renderer`.
#![cfg(target_arch = "wasm32")]

use std::cell::RefCell;
use std::rc::Rc;

use ghostframe_client_wasm::app::client::{App, AppParts};
use ghostframe_client_wasm::app::dom::PageLog;
use ghostframe_client_wasm::app::hooks::HookCall;
use ghostframe_client_wasm::app::page::PageFlags;
use ghostframe_client_wasm::core::{client_config, new_client_core};
use ghostframe_client_wasm::web::renderer::WebRenderer;
use ghostframe_client_wasm::web::route::{Routed, SlimEvent};
use ghostframe_protocol::fec::feedback::{BundleAckBatch, FEC_ACK_V4_MSG_TYPE};
use ghostframe_protocol::feedback_stream::HELLO_MSG_TYPE;
use ghostframe_protocol::protocol::{
    build_frame_dimensions_datagram, fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG,
};
use js_sys::{Array, Function, Promise, Reflect, Uint8Array};
use wasm_bindgen::{JsCast, JsValue};
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;
use web_sys::{WritableStream, WritableStreamDefaultWriter};

wasm_bindgen_test_configure!(run_in_browser);

fn now_ms() -> f64 {
    web_sys::window().unwrap().performance().unwrap().now()
}

async fn sleep(ms: i32) {
    let p = Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(p).await.unwrap();
}

/// A writer whose every chunk is pushed, as a plain number array, into
/// `globalThis[name]`.
fn recording_writer(name: &str) -> WritableStreamDefaultWriter {
    let make = Function::new_with_args(
        "name",
        "globalThis[name] = []; return new WritableStream({ write(chunk) { \
           globalThis[name].push(Array.from(chunk)); } });",
    );
    let stream: WritableStream = make
        .call1(&JsValue::NULL, &JsValue::from_str(name))
        .unwrap()
        .unchecked_into();
    stream.get_writer().unwrap()
}

/// The chunks recorded so far.
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

fn div(document: &web_sys::Document, id: &str) -> web_sys::HtmlElement {
    let el = document.create_element("div").unwrap();
    el.set_id(id);
    document.body().unwrap().append_child(&el).unwrap();
    el.unchecked_into()
}

/// The sequence of hook calls as kinds, for failure messages and ordering.
fn kinds(calls: &[HookCall]) -> Vec<&'static str> {
    calls
        .iter()
        .map(|c| match c {
            HookCall::Events { .. } => "Events",
            HookCall::DatagramCounts { .. } => "DatagramCounts",
            HookCall::RafTick(_) => "RafTick",
            HookCall::BeforePaint { .. } => "BeforePaint",
            HookCall::AfterPaint { .. } => "AfterPaint",
            HookCall::Swallowed { .. } => "Swallowed",
            HookCall::RendererError { .. } => "RendererError",
            HookCall::ServerStream { .. } => "ServerStream",
        })
        .collect()
}

#[wasm_bindgen_test]
async fn a_tile_is_acknowledged_and_the_hooks_see_the_tick_in_order() {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas: web_sys::HtmlCanvasElement =
        document.create_element("canvas").unwrap().unchecked_into();
    document.body().unwrap().append_child(&canvas).unwrap();
    let renderer = WebRenderer::create(canvas, "auto".into())
        .await
        .unwrap_or_else(|e| panic!("WebRenderer::create: {e:?}"));
    let status = div(&document, "status");
    div(&document, "log");
    let log = Rc::new(PageLog::find(&document).unwrap());

    let now_us = (now_ms() * 1000.0).round() as u64;
    let core = Rc::new(RefCell::new(new_client_core(
        client_config(true, false, true),
        now_us,
    )));
    let app = App::new(AppParts {
        flags: PageFlags::from_params(&["1".into()], None),
        renderer: Rc::new(RefCell::new(renderer)),
        core,
        log,
        datagram_writer: recording_writer("__dg"),
        feedback_writer: Some(recording_writer("__fb")),
        repair_status: None,
        tick_us: Rc::new(RefCell::new(Vec::new())),
        hooks_enabled: true,
    });

    // The HELLO leaves first, on the feedback stream.
    app.borrow_mut().drain_transmit();
    sleep(0).await;
    let fb = recorded("__fb");
    assert_eq!(fb.first().and_then(|m| m.first()), Some(&HELLO_MSG_TYPE));

    // A 64x64 frame and one solid tile, as the server would send them.
    let dims = build_frame_dimensions_datagram(1, 0, 64, 64);
    let mut tile = fragment_tile(
        &TileFragmentInputs {
            frame_seq: 2 | TILE_DATAGRAM_FLAG,
            tile_x: 0,
            tile_y: 0,
            codec: Codec::Solid,
            generation: 0,
            pass: 0,
            timestamp_us: 0,
        },
        &[0, 0, 255, 255],
        1200,
    )
    .remove(0);
    tile[8..12].copy_from_slice(&4242u32.to_be_bytes());
    // Every datagram the core accepts is a rev 6 frame; the ACK names the
    // frame's bundle_seq, not the record's wire_seq.
    for (bundle, d) in [(9000, &dims), (9001, &tile)] {
        let framed = ghostframe_client_core::frame_plain(bundle, &[d.as_slice()]);
        app.borrow_mut().on_datagram(
            &Uint8Array::from(framed.as_slice()),
            now_ms() * 1000.0,
            false,
        );
    }

    // Exactly the borrow `start_client` takes.
    let calls = {
        let mut a = app.borrow_mut();
        a.tick();
        a.take_calls()
    };
    let seen = kinds(&calls);
    assert!(
        matches!(
            calls.as_slice(),
            [
                HookCall::RafTick(_),
                HookCall::Events { .. },
                HookCall::DatagramCounts { .. },
                ..,
                HookCall::AfterPaint { .. }
            ]
        ),
        "{seen:?}"
    );
    assert!(
        calls
            .iter()
            .any(|c| matches!(c, HookCall::BeforePaint { .. })),
        "{seen:?}"
    );
    let HookCall::Events { records, .. } = &calls[1] else {
        unreachable!()
    };
    assert!(
        records.iter().any(|r| matches!(
            r,
            Routed::Tile(SlimEvent::FrameDimensions { applied: true, .. })
        )),
        "{records:?}"
    );
    assert!(
        records
            .iter()
            .any(|r| matches!(r, Routed::Tile(SlimEvent::TilePayload { .. }))),
        "{records:?}"
    );
    let Some(HookCall::AfterPaint { drained, .. }) = calls.last() else {
        unreachable!()
    };
    assert_eq!(drained.solid, 1, "{seen:?}");
    assert_eq!(status.text_content().as_deref(), Some("Receiving frames"));

    // The ACK waits for its 5 ms deadline; the next tick sends it.
    sleep(30).await;
    {
        let mut a = app.borrow_mut();
        a.tick();
        a.take_calls();
    }
    sleep(0).await;
    let acked: Vec<u32> = recorded("__dg")
        .iter()
        .filter(|d| d.first() == Some(&FEC_ACK_V4_MSG_TYPE))
        .flat_map(|d| {
            BundleAckBatch::decode(d)
                .expect("a well-formed ACK batch")
                .entries
        })
        .map(|e| e.bundle_seq)
        .collect();
    assert!(acked.contains(&9001), "ACKed bundle_seqs: {acked:?}");
    // The record's wire_seq (4242) is not what an ACK names.
    assert!(!acked.contains(&4242), "ACKed bundle_seqs: {acked:?}");
}
