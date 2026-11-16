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
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [255, 0, 0, 255];
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
/// by the protocol crate. Tests the path the Rust client calls directly,
/// `feed_datagrams` then `web::route::route`: the tile is applied inside wasm
/// and the record is slim, not the payload.
#[wasm_bindgen_test]
async fn datagrams_reach_the_renderer_without_their_bytes_reaching_js() {
    use ghostframe_client_wasm::core::{client_config, feed_datagrams, new_client_core};
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
    let mut core = new_client_core(client_config(true, false, true), 0);
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    let concat: Vec<u8> = datagrams.iter().flatten().copied().collect();
    let lens: Vec<u32> = datagrams.iter().map(|d| d.len() as u32).collect();
    let arrivals = vec![1000.0; datagrams.len()];
    let events = feed_datagrams(&mut core, &concat, &lens, &arrivals);
    let out =
        serde_wasm_bindgen::to_value(&ghostframe_client_wasm::web::route::route(events, &mut r))
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
use ghostframe_client_wasm::web::renderer::PaintOutcome;

const MAGENTA: [u8; 4] = [255, 0, 255, 255];

async fn sleep(ms: i32) {
    let p = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(p).await.unwrap();
}

/// A real `VideoFrame` of one colour (RGB), made from an `OffscreenCanvas`, so
/// the H.264 escape imports exactly what WebCodecs' decoder would hand it.
fn video_frame(rgb: [u8; 3], w: u32, h: u32) -> wasm_bindgen::JsValue {
    let make = js_sys::Function::new_with_args(
        "r,g,b,w,h",
        "const c = new OffscreenCanvas(w, h); const x = c.getContext('2d'); \
         x.fillStyle = `rgb(${r},${g},${b})`; x.fillRect(0, 0, w, h); \
         return new VideoFrame(c, { timestamp: 0 });",
    );
    let args = js_sys::Array::of5(
        &rgb[0].into(),
        &rgb[1].into(),
        &rgb[2].into(),
        &w.into(),
        &h.into(),
    );
    make.apply(&wasm_bindgen::JsValue::NULL, &args)
        .expect("VideoFrame from a canvas")
}

/// `VideoFrame.close()` leaves `codedWidth` at 0.
fn is_closed(frame: &wasm_bindgen::JsValue) -> bool {
    js_sys::Reflect::get(frame, &"codedWidth".into())
        .ok()
        .and_then(|v| v.as_f64())
        == Some(0.0)
}

fn tick(r: &mut WebRenderer, n: u64) -> PaintOutcome {
    r.paint_tick(n * 1_000).expect("paint tick")
}

/// Spec section 7, "H.264 ordering": tile N, picture N+1, tile N+2, then read
/// the framebuffer back. A picture the ledger finds stale is dropped, so the
/// order the GPU runs two overlapping draws in is only visible when neither is
/// stale: the last tick draws a tile and a picture of the SAME frame_seq,
/// which the contract (flush, then pictures, then present) says leaves the
/// picture on top.
#[wasm_bindgen_test]
async fn tiles_and_pictures_are_ordered_by_frame_seq_and_the_queue() {
    let mut r = new_renderer("auto").await;
    assert!(
        r.h264_supported(),
        "Chromium has texture_external: the picture tests need it"
    );
    r.apply(dims(64, 64)).unwrap();

    // Tick 1, tile N = 10: every tile red.
    for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        r.apply(solid(10, x, y, RED)).unwrap();
    }
    tick(&mut r, 1);
    assert!(near(px(&r, 8, 8).await, RED, 0) && near(px(&r, 40, 40).await, RED, 0));

    // Tick 2, picture N+1 = 11 (green): over tile N everywhere.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 11);
    let out = tick(&mut r, 2);
    assert_eq!(
        (out.pictures_drawn, out.pictures_dropped, out.presented),
        (1, 0, true)
    );
    assert!(
        near(px(&r, 8, 8).await, GREEN, 3),
        "picture N+1 over tile N: {:?}",
        px(&r, 8, 8).await
    );
    assert!(near(px(&r, 40, 40).await, GREEN, 3));
    // `note_h264` marks no damage, so the tick presented because a picture was
    // DRAWN; nothing may be left over to present again.
    assert!(!tick(&mut r, 20).presented, "an idle tick presents nothing");
    // An idle tick skips before the flush, so it cannot show leftover damage;
    // a tick that is not skipped (a stale picture is pending) and flushes can.
    r.push_h264(video_frame([255, 0, 0], 64, 64), 10);
    let out = tick(&mut r, 21);
    assert_eq!((out.pictures_drawn, out.pictures_dropped), (0, 1));
    assert!(
        !out.presented,
        "a drawn picture must not leave damage behind"
    );

    // Tick 3, tile N+2 = 12 (yellow) on tile (0,0): over the picture; the rest stays the picture.
    r.apply(solid(12, 0, 0, YELLOW)).unwrap();
    tick(&mut r, 3);
    assert!(
        near(px(&r, 8, 8).await, YELLOW, 0),
        "tile N+2 over picture N+1"
    );
    assert!(
        near(px(&r, 40, 40).await, GREEN, 3),
        "picture N+1 over tile N where N+2 did not draw"
    );

    // Tick 4, a late picture 11 (red): tile 12 is newer, so it is stale: closed, not drawn, not presented.
    let late = video_frame([255, 0, 0], 64, 64);
    let presents = r.submit_count();
    r.push_h264(late.clone(), 11);
    let out = tick(&mut r, 4);
    assert_eq!(
        (out.pictures_drawn, out.pictures_dropped, out.presented),
        (0, 1, false)
    );
    assert_eq!(
        r.submit_count(),
        presents,
        "a stale picture must not cost a present"
    );
    assert!(
        is_closed(&late),
        "a stale picture must be closed, not leaked"
    );
    assert!(near(px(&r, 8, 8).await, YELLOW, 0) && near(px(&r, 40, 40).await, GREEN, 3));

    // Tick 5, the queue order itself: tile 13 (blue) on tile (1,0) and picture 13
    // (magenta) in one tick. Equal frame_seq supersedes nothing, so both draw;
    // tiles are flushed first, so the picture ends on top.
    r.apply(solid(13, 1, 0, BLUE)).unwrap();
    r.push_h264(video_frame([255, 0, 255], 64, 64), 13);
    let out = tick(&mut r, 5);
    assert_eq!(out.pictures_drawn, 1);
    assert!(
        near(px(&r, 40, 8).await, MAGENTA, 3),
        "the picture must be submitted after the tile flush: got {:?}",
        px(&r, 40, 8).await
    );
    assert!(near(px(&r, 8, 8).await, MAGENTA, 3));
}

#[wasm_bindgen_test]
async fn an_idle_tick_does_not_touch_the_gpu_and_an_event_wakes_it() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    r.apply(solid(1, 0, 0, RED)).unwrap();
    assert!(tick(&mut r, 1).presented);
    let before = (r.submit_count(), r.paint_tick_count());
    for n in 2..12 {
        assert!(!tick(&mut r, n).presented, "tick {n} had nothing to paint");
    }
    assert_eq!(
        (r.submit_count(), r.paint_tick_count()),
        before,
        "ten idle ticks must neither present nor count as painted ticks"
    );
    r.apply(solid(2, 1, 0, GREEN)).unwrap();
    assert!(tick(&mut r, 20).presented);
    assert_eq!(r.submit_count(), before.0 + 1);
}

fn bad_palrle(frame_seq: u32, x: u8, y: u8) -> Event {
    // Two palette colours, indices 15: out of range, which the shader reports.
    Event::TilePayload {
        frame_seq,
        tile_x: x,
        tile_y: y,
        generation: 0,
        data: TileData::PalRle {
            palette_id: 0,
            count: 2,
            indices: vec![0xFF; 512],
            wire_flags: 0,
        },
    }
}

#[wasm_bindgen_test]
async fn palrle_errors_are_read_back_without_a_browser_error() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(128, 128)).unwrap();
    r.apply(Event::PaletteUpdated {
        palette_id: 0,
        colors: vec![RED, GREEN],
    })
    .unwrap();
    let mut found: Vec<u32> = Vec::new();
    let mut last = GREEN;
    // A PalRLE batch on every tick keeps the staging buffer contended. The
    // errors copy is a submit of its own (`start_errors_readback`), and a copy
    // into a buffer that is still pending or mapped is a validation error the
    // browser reports (and that batch's errors are lost): `take_errors()` below
    // is the detector. (The TypeScript carried the copy in the tick's one
    // encoder, so there the whole tick was rejected; here the Solid tile is
    // only a sanity check that painting carried on.)
    for n in 0..60u32 {
        r.apply(bad_palrle(2 + n, 0, 0)).unwrap();
        last = if n % 2 == 0 { GREEN } else { BLUE };
        r.apply(solid(2 + n, 3, 3, last)).unwrap();
        tick(&mut r, u64::from(n) + 1);
        sleep(4).await; // let the map callbacks run
        found.extend(r.take_decode_errors());
    }
    assert!(
        found.chunks(4).any(|c| c[1] == 0 && c[2] == 0 && c[3] == 5),
        "no IndexOob (5) for tile (0,0) in {found:?}"
    );
    assert!(
        found.chunks(4).all(|c| c[0] == 2),
        "every report names the PalRle codec: {found:?}"
    );
    let errors = r.take_errors();
    assert_eq!(
        errors.length(),
        0,
        "the browser rejected or reported something: {errors:?}"
    );
    assert!(
        near(px(&r, 3 * 32 + 16, 3 * 32 + 16).await, last, 0),
        "the last Solid tile was painted"
    );
}

#[wasm_bindgen_test]
async fn a_palrle_error_on_a_static_screen_is_still_reported() {
    // The shape of e2e_palrle_oob_index (CAPTURE_FPS=2, a scene that never
    // changes): one bad tile, then only idle ticks. The map callback finishes
    // between ticks; nothing flushes afterwards, so the result must be
    // collected by `take_decode_errors` itself and the idle skip must not hide
    // it.
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    r.apply(Event::PaletteUpdated {
        palette_id: 0,
        colors: vec![RED, GREEN],
    })
    .unwrap();
    r.apply(bad_palrle(1, 0, 0)).unwrap();
    tick(&mut r, 1);
    let mut found: Vec<u32> = Vec::new();
    for n in 2..300u64 {
        sleep(10).await;
        let out = tick(&mut r, n); // idle: nothing was applied since tick 1
        assert_eq!(out.pictures_drawn, 0);
        found.extend(r.take_decode_errors());
        if !found.is_empty() {
            break;
        }
    }
    assert!(
        found.chunks(4).any(|c| c[1] == 0 && c[2] == 0 && c[3] == 5),
        "IndexOob (5) for tile (0,0) never arrived on a static screen: {found:?}"
    );
}

use ghostframe_client_core::cdf53_prevalidate::{prevalidate_cdf53, PrevalidatedCdf53};
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_protocol::codec::cdf53 as codec;

fn gradient_bgra(seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(4096);
    for y in 0..32u32 {
        for x in 0..32u32 {
            v.extend_from_slice(&[
                ((x * 6 + seed * 11) & 0xFF) as u8,
                ((y * 7 + seed) & 0xFF) as u8,
                (128 + x + y) as u8,
                0xFF,
            ]);
        }
    }
    v
}

fn passes(seed: u32) -> Vec<PrevalidatedCdf53> {
    let coeffs = codec::forward(&gradient_bgra(seed));
    let (_, sparse) = codec::encode_passes_sparse(&coeffs);
    sparse
        .iter()
        .map(|(p, payload)| prevalidate_cdf53(payload, 1, *p).expect("valid pass"))
        .collect()
}

fn pass_event(p: &PrevalidatedCdf53, x: u8, y: u8, seq: u32, first: bool) -> Event {
    Event::TilePayload {
        frame_seq: seq,
        tile_x: x,
        tile_y: y,
        generation: p.generation,
        data: TileData::Cdf53 {
            pass_idx: p.pass_idx,
            bit_planes: p.bit_planes.clone(),
            present_passes: p.present_passes,
            new_generation: first,
        },
    }
}

async fn paint_cdf53(mode: &str) -> (WebRenderer, Vec<u8>) {
    let mut r = new_renderer(mode).await;
    r.apply(dims(64, 64)).unwrap();
    for (i, (x, y)) in [(0u8, 0u8), (1, 1)].into_iter().enumerate() {
        for (k, p) in passes(i as u32 + 1).iter().enumerate() {
            r.apply(pass_event(p, x, y, 1, k == 0)).unwrap();
        }
    }
    // The grid defers work past its per-flush budget: tick until it is done.
    for n in 1..=200u64 {
        let _ = tick(&mut r, n);
        if !r.has_deferred_work() {
            break;
        }
        sleep(2).await;
    }
    tick(&mut r, 300);
    let bytes = JsFuture::from(r.read_pixel_rect(0, 0, 64, 64).unwrap())
        .await
        .unwrap();
    let v = Uint8Array::new(&bytes).to_vec();
    (r, v)
}

#[wasm_bindgen_test]
async fn cdf53_on_the_gpu_matches_the_cpu_decoder_exactly_and_the_forced_path_uses_the_cpu() {
    let (gpu, gpu_px) = paint_cdf53("auto").await;
    assert_eq!(gpu.cdf53_path(), "gpu", "this browser must pass the CDF 5/3 self-test in `try_enable_cdf53_gpu` (else only the fallback is tested)");
    let (cpu, cpu_px) = paint_cdf53("cpu").await;
    assert_eq!(cpu.cdf53_path(), "cpu");

    // The CPU decoder in `ghostframe-client-core` is the reference for what the
    // screen must show; lossless passes of a known picture reproduce it exactly.
    let mut want = vec![0u8; 64 * 64 * 4];
    for (i, (tx, ty)) in [(0usize, 0usize), (1, 1)].into_iter().enumerate() {
        let mut st = Cdf53TileState::new();
        let mut shown = None;
        for p in &passes(i as u32 + 1) {
            if let Some(rgba) = st.integrate(0, 0, p) {
                shown = Some(rgba);
            }
        }
        let shown = shown.expect("the CPU decoder drew the tile");
        for y in 0..32 {
            let o = ((ty * 32 + y) * 64 + tx * 32) * 4;
            want[o..o + 128].copy_from_slice(&shown[y * 128..y * 128 + 128]);
        }
    }
    assert_eq!(cpu_px, want, "the CPU path against the CPU reference");
    assert_eq!(
        gpu_px, want,
        "the GPU grid against the CPU reference: lossless means exact"
    );
}

#[wasm_bindgen_test]
async fn cdf53_work_under_a_picture_is_cancelled_and_not_left_pending() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    for (k, p) in passes(1).iter().enumerate() {
        r.apply(pass_event(p, 0, 0, 5, k == 0)).unwrap();
    }
    // The picture (seq 6) is newer than every pass (seq 5): not stale, so the
    // tick holds the passes, draws the picture and `note_h264` cancels them.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 6);
    let out = tick(&mut r, 1);
    assert_eq!(out.pictures_drawn, 1);
    assert!(
        near(px(&r, 8, 8).await, GREEN, 3),
        "the picture covers the CDF 5/3 tile"
    );
    assert!(
        !r.has_deferred_work(),
        "held passes were cancelled by the picture, not left waiting"
    );
    assert!(!tick(&mut r, 2).presented, "and nothing is left to draw");
    let state = r.debug_state().unwrap();
    assert_eq!(
        js_sys::Reflect::get(&state, &"cdf53Pending".into()).unwrap(),
        false
    );
}

/// A picture that cannot be imported (a closed `VideoFrame`: importExternalTexture
/// throws) is not drawn, is reported, and covers nothing: the CDF 5/3 work held
/// under it must not be cancelled. Next to one that imports, only the second
/// counts, and the tick still presents it.
#[wasm_bindgen_test]
async fn a_picture_that_fails_to_import_is_reported_and_cancels_nothing() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    for (k, p) in passes(1).iter().enumerate() {
        r.apply(pass_event(p, 0, 0, 5, k == 0)).unwrap();
    }
    let closed = video_frame([0, 255, 0], 64, 64);
    js_sys::Reflect::get(&closed, &"close".into())
        .unwrap()
        .unchecked_into::<js_sys::Function>()
        .call0(&closed)
        .unwrap();
    r.push_h264(closed, 6);
    let out = tick(&mut r, 1);
    assert_eq!(out.pictures_drawn, 0, "a closed frame cannot be drawn");
    let errors = r.take_errors();
    assert!(
        errors.iter().any(|e| e
            .as_string()
            .unwrap_or_default()
            .contains("importExternalTexture")),
        "the import failure is reported: {errors:?}"
    );
    assert!(
        r.has_deferred_work(),
        "the held CDF 5/3 passes are still pending: no picture covers them"
    );
    for n in 2..=200u64 {
        let _ = tick(&mut r, n);
        if !r.has_deferred_work() {
            break;
        }
        sleep(2).await;
    }
    assert!(!r.has_deferred_work(), "the held passes were drawn later");
    assert!(
        !near(px(&r, 8, 8).await, GREEN, 3),
        "the failed picture never reached the framebuffer"
    );

    // One closed frame next to one that imports: one drawn, and it presents.
    let closed = video_frame([255, 0, 0], 64, 64);
    js_sys::Reflect::get(&closed, &"close".into())
        .unwrap()
        .unchecked_into::<js_sys::Function>()
        .call0(&closed)
        .unwrap();
    r.push_h264(closed, 7);
    r.push_h264(video_frame([0, 255, 0], 64, 64), 8);
    let out = tick(&mut r, 300);
    assert_eq!(
        (out.pictures_drawn, out.pictures_dropped, out.presented),
        (1, 0, true)
    );
    assert!(near(px(&r, 8, 8).await, GREEN, 3));
    assert!(
        r.take_errors().length() > 0,
        "the skipped import is reported"
    );
}

#[wasm_bindgen_test]
async fn a_session_reset_closes_pending_pictures_and_forgets_the_ledger() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    r.apply(solid(10, 0, 0, RED)).unwrap();
    tick(&mut r, 1);
    // Picture 3 is older than tile 10: stale, dropped.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 3);
    assert_eq!(tick(&mut r, 2).pictures_dropped, 1);
    // A picture still pending when the transport closes is closed by the reset.
    let pending = video_frame([0, 255, 0], 64, 64);
    r.push_h264(pending.clone(), 3);
    r.on_session_reset();
    assert!(
        is_closed(&pending),
        "a session reset must close pending pictures"
    );
    // The new session restarts its sequence numbers: picture 3 is drawn now.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 3);
    assert_eq!(
        tick(&mut r, 3).pictures_drawn,
        1,
        "the ledger must be forgotten"
    );
}

#[wasm_bindgen_test]
async fn the_debug_handles_expose_the_framebuffer_texture_the_e2e_reads() {
    let mut r = new_renderer("auto").await;
    assert!(
        r.debug_handles().is_undefined(),
        "no texture before the first size"
    );
    r.apply(dims(96, 64)).unwrap();
    let h = r.debug_handles();
    let texture = js_sys::Reflect::get(&h, &"texture".into()).unwrap();
    let w = js_sys::Reflect::get(&texture, &"width".into())
        .unwrap()
        .as_f64();
    let hh = js_sys::Reflect::get(&texture, &"height".into())
        .unwrap()
        .as_f64();
    assert_eq!((w, hh), (Some(96.0), Some(64.0)));
    assert!(!js_sys::Reflect::get(&h, &"device".into())
        .unwrap()
        .is_undefined());
    // `newestTileSeq` is an `Option`: absent until a tile has been noted.
    r.apply(solid(7, 1, 1, RED)).unwrap();
    let state = r.debug_state().unwrap();
    // The old renderer's keys, and PR 1's `DebugState` spliced in (camelCase).
    for key in [
        "submitCount",
        "fbWidth",
        "fbHeight",
        "hasTexture",
        "paintTick",
        "canvasWidth",
        "solidQueue",
        "palRleQueue",
        "flushes",
        "cdf53Gpu",
        "newestTileSeq",
    ] {
        assert!(
            !js_sys::Reflect::get(&state, &key.into())
                .unwrap()
                .is_undefined(),
            "debugState().{key}"
        );
    }
    let tile = r.debug_tile(1, 1).unwrap();
    assert_eq!(
        js_sys::Reflect::get(&tile, &"newestFrameSeq".into())
            .unwrap()
            .as_f64(),
        Some(7.0)
    );
    assert!(
        r.debug_tile(200, 200).unwrap().is_null(),
        "off the grid is null, as before"
    );
}

#[wasm_bindgen_test]
async fn the_gradient_golden_agrees_with_the_pixel_readback() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(512, 300)).unwrap();
    let report = JsFuture::from(r.read_gradient_golden().unwrap())
        .await
        .unwrap();
    let ok = js_sys::Reflect::get(&report, &"ok".into()).unwrap();
    assert_eq!(
        ok,
        true,
        "mismatches: {:?}",
        js_sys::Reflect::get(&report, &"mismatches".into())
    );
}
