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
