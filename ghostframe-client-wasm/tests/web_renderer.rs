// SPDX-License-Identifier: MIT OR Apache-2.0
//! `WebRenderer` in a real browser with a real WebGPU device. Run with
//! `just test-web-renderer` (headless Chrome through wasm-pack).
#![cfg(target_arch = "wasm32")]

use ghostframe_client_core::{Event, TileData};
use ghostframe_client_wasm::web::renderer::WebRenderer;
use ghostframe_client_wasm::web::route::TileSink;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const RED: [u8; 4] = [0, 0, 255, 255]; // BGRA, as on the wire
#[allow(dead_code, reason = "used by the tests Task 11 adds")]
const GREEN: [u8; 4] = [0, 255, 0, 255];
#[allow(dead_code, reason = "used by the tests Task 11 adds")]
const BLUE: [u8; 4] = [255, 0, 0, 255];
#[allow(dead_code, reason = "used by the tests Task 11 adds")]
const YELLOW: [u8; 4] = [0, 255, 255, 255];

fn rgba(bgra: [u8; 4]) -> [u8; 4] {
    [bgra[2], bgra[1], bgra[0], bgra[3]]
}

async fn new_renderer(cdf53: &str) -> WebRenderer {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas: web_sys::HtmlCanvasElement =
        document.create_element("canvas").unwrap().unchecked_into();
    document.body().unwrap().append_child(&canvas).unwrap();
    WebRenderer::create(canvas, cdf53.to_string())
        .await
        .unwrap_or_else(|e| panic!("WebRenderer::create: {e:?}"))
}

fn dims(w: u32, h: u32) -> Event {
    Event::FrameDimensions {
        width: w,
        height: h,
    }
}

fn solid(frame_seq: u32, tile_x: u8, tile_y: u8, bgra: [u8; 4]) -> Event {
    Event::TilePayload {
        frame_seq,
        tile_x,
        tile_y,
        generation: 0,
        data: TileData::Solid(bgra),
    }
}

/// The framebuffer pixel at (x, y), RGBA.
async fn px(r: &WebRenderer, x: u32, y: u32) -> [u8; 4] {
    let bytes = JsFuture::from(r.read_pixel_rect(x, y, 1, 1).expect("read"))
        .await
        .expect("map");
    let v = Uint8Array::new(&bytes).to_vec();
    [v[0], v[1], v[2], v[3]]
}

fn near(got: [u8; 4], want_bgra: [u8; 4], tol: u8) -> bool {
    let want = rgba(want_bgra);
    got.iter()
        .zip(want.iter())
        .all(|(g, w)| g.abs_diff(*w) <= tol)
}

#[wasm_bindgen_test]
async fn a_solid_tile_reaches_the_framebuffer() {
    let mut r = new_renderer("auto").await;
    assert_eq!(r.fb_width(), 0, "no size until the first FrameDimensions");
    r.apply(dims(64, 64)).expect("resize");
    assert_eq!((r.fb_width(), r.fb_height()), (64, 64));
    r.apply(solid(10, 1, 1, RED)).expect("tile");
    let out = r.paint_tick(1_000).expect("paint");
    assert!(out.presented, "a tile arrived: the tick must present");
    assert!(near(px(&r, 40, 40).await, RED, 0), "tile (1,1) is red");
    assert_eq!(
        px(&r, 8, 8).await,
        [0, 0, 0, 0],
        "an untouched tile stays clear"
    );
}

#[wasm_bindgen_test]
async fn a_refused_frame_size_leaves_the_renderer_unchanged() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).expect("resize");
    r.apply(dims(0, 0)).expect("a refused size is not an error");
    r.apply(dims(u32::MAX, u32::MAX))
        .expect("nor is a huge one");
    assert_eq!((r.fb_width(), r.fb_height()), (64, 64));
    r.apply(solid(10, 1, 1, RED)).expect("tile");
    r.paint_tick(1_000).expect("paint");
    assert!(near(px(&r, 40, 40).await, RED, 0), "pixel read still works");
}

/// A real datagram stream: a core, a renderer, and a Solid tile datagram built
/// by the protocol crate. Proves `handleDatagramsInto` applies the tile inside
/// wasm and returns a slim record, not the payload.
#[wasm_bindgen_test]
async fn datagrams_reach_the_renderer_without_their_bytes_reaching_js() {
    use ghostframe_client_wasm::core::WasmClientCore;
    use ghostframe_protocol::protocol::{
        fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG,
    };

    let datagrams = fragment_tile(
        &TileFragmentInputs {
            frame_seq: 5 | TILE_DATAGRAM_FLAG,
            tile_x: 1,
            tile_y: 1,
            codec: Codec::Solid,
            generation: 1,
            pass: 0,
            timestamp_us: 0,
        },
        &RED, // a Solid payload is one BGRA quad
        1200,
    );
    let mut core = WasmClientCore::new(true, false, true, 0);
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    let concat: Vec<u8> = datagrams.iter().flatten().copied().collect();
    let lens: Vec<u32> = datagrams.iter().map(|d| d.len() as u32).collect();
    let arrivals = vec![1000.0; datagrams.len()];
    let out = core
        .handle_datagrams_into(&mut r, &concat, &lens, &arrivals)
        .expect("route");
    let out = js_sys::Array::from(&out);
    assert_eq!(out.length(), 1, "one tile, one record");
    let rec = out.get(0);
    let get = |k: &str| js_sys::Reflect::get(&rec, &k.into()).unwrap();
    assert_eq!(get("kind"), "TilePayload");
    assert_eq!(get("tile_x").as_f64(), Some(1.0));
    let data = js_sys::Reflect::get(&rec, &"data".into()).unwrap();
    assert_eq!(
        js_sys::Reflect::get(&data, &"codec".into()).unwrap(),
        "Solid"
    );
    assert_eq!(
        js_sys::Reflect::get(&data, &"len".into()).unwrap().as_f64(),
        Some(4.0)
    );
    r.paint_tick(2_000).unwrap();
    assert!(
        near(px(&r, 40, 40).await, RED, 0),
        "the tile was applied inside wasm"
    );
}
