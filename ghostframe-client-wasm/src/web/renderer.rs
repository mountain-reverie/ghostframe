// SPDX-License-Identifier: MIT OR Apache-2.0
//! `WebRenderer`: the page's renderer (spec 5.2).
//!
//! Owns the wgpu device on the browser's WebGPU backend, the canvas surface,
//! a `TileRenderer` (the native client's renderer), the H.264 escape and the
//! present blit. `paint_tick` is the rAF body: flush tiles, draw pending
//! pictures in ledger order, present; it does none of that when nothing
//! changed.

use std::sync::{Arc, Mutex};

use ghostframe_client_core::cdf53_coverage::frame_seq_is_newer;
use ghostframe_client_core::Event;
use ghostframe_render::TileRenderer;
use js_sys::{Array, Promise, Reflect, Uint8Array};
use serde::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, JsFuture};

use super::h264::H264Blit;
use super::logic::{pick_canvas_format, should_skip_paint, Cdf53Mode, PaintInputs};
use super::present::Present;
use super::raw;
use super::route::TileSink;

/// The tile shaders write `rgba8unorm` storage textures, so the framebuffer
/// cannot follow the canvas's preferred format.
const FRAMEBUFFER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// `requestAdapter` retries while Chrome's GPU process starts: 10 x 1 s, as
/// the TypeScript did.
const ADAPTER_RETRIES: u32 = 10;
/// Paint-cost samples kept (microseconds per non-skipped tick).
const PAINT_US_CAP: usize = 65_536;

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

fn console_info(msg: &str) {
    web_sys::console::info_1(&JsValue::from_str(msg));
}

fn console_warn(msg: &str) {
    web_sys::console::warn_1(&JsValue::from_str(msg));
}

async fn sleep_ms(ms: i32) {
    let promise = Promise::new(&mut |resolve, _reject| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

fn has_navigator_gpu() -> bool {
    web_sys::window().is_some_and(|w| {
        Reflect::get(&w.navigator(), &JsValue::from_str("gpu"))
            .is_ok_and(|g| !g.is_undefined() && !g.is_null())
    })
}

/// A picture waiting for the next tick: the JS `VideoFrame` and the
/// `frame_seq` its access unit carried.
struct PendingPicture {
    frame: JsValue,
    seq: u32,
}

/// What a tick did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PaintOutcome {
    /// A frame was presented (the only thing the page counts as painted).
    pub presented: bool,
    pub pictures_drawn: u32,
    pub pictures_dropped: u32,
}

#[wasm_bindgen]
pub struct WebRenderer {
    /// Kept for the renderer's lifetime: when the browser collects the
    /// instance, it loses every device made from it ("A valid external
    /// Instance reference no longer exists"), and later maps and submits fail.
    _instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    canvas: web_sys::HtmlCanvasElement,
    surface_format: wgpu::TextureFormat,
    present: Present,
    tiles: TileRenderer,
    fb_view: Option<wgpu::TextureView>,
    /// Names the framebuffer texture; bumped whenever a resize replaces it.
    fb_epoch: u64,
    h264: Option<H264Blit>,
    pending_h264: Vec<PendingPicture>,
    h264_dropped: u32,
    /// `None` until the first accepted `FrameDimensions`.
    size: Option<(u32, u32)>,
    dirty: bool,
    paint_ticks: u32,
    submits: u32,
    /// Uncaptured GPU errors (pushed from the device's callback).
    gpu_errors: Arc<Mutex<Vec<String>>>,
    /// Errors from this struct's own steps, drained by `takeErrors`.
    errors: Vec<String>,
    /// `[codec, tile_x, tile_y, code]` quadruples awaiting `takeDecodeErrors`.
    decode_errors: Vec<u32>,
    paint_us: Vec<u32>,
}

impl Drop for WebRenderer {
    /// wgpu's WebGPU backend frees nothing on drop: every buffer and texture
    /// lives until the page's garbage collector gets to it. Destroying the
    /// device frees them now, so a replaced renderer (or a test that made one)
    /// does not hold its framebuffers and CDF 5/3 storage for an unknown time.
    /// The device-lost callback ignores the `Destroyed` reason this causes.
    fn drop(&mut self) {
        self.device.destroy();
    }
}

impl WebRenderer {
    async fn create_inner(
        canvas: web_sys::HtmlCanvasElement,
        mode: Cdf53Mode,
    ) -> Result<Self, String> {
        // Design D2: WebGPU is required; its absence is fatal and named, so
        // main.ts can tell the user instead of showing a blank page.
        if !has_navigator_gpu() {
            return Err("WEBGPU_UNAVAILABLE: navigator.gpu is undefined".into());
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| format!("WEBGPU_UNAVAILABLE: canvas.getContext('webgpu'): {e}"))?;
        let mut adapter = None;
        for attempt in 0..=ADAPTER_RETRIES {
            let found = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    ..Default::default()
                })
                .await;
            match found {
                Ok(a) => {
                    adapter = Some(a);
                    break;
                }
                Err(e) if attempt == ADAPTER_RETRIES => {
                    return Err(format!("WEBGPU_UNAVAILABLE: requestAdapter: {e}"));
                }
                // Chrome's GPU process starts asynchronously; the first call
                // can come back empty before its Vulkan backend is ready.
                Err(_) => sleep_ms(1000).await,
            }
        }
        let adapter = adapter.ok_or("WEBGPU_UNAVAILABLE: no adapter")?;

        let limits = adapter.limits();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ghostframe-web"),
                // 8 is the WebGPU default; stated because the CDF 5/3 grid and
                // the PalRLE pipeline bind that many. The storage-buffer sizes
                // are raised to the adapter's so a 4K frame's plane store fits.
                required_limits: wgpu::Limits {
                    max_storage_buffers_per_shader_stage: 8,
                    max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                    max_buffer_size: limits.max_buffer_size,
                    ..wgpu::Limits::default()
                },
                ..Default::default()
            })
            .await
            .map_err(|e| format!("WEBGPU_UNAVAILABLE: requestDevice: {e}"))?;

        let gpu_errors: Arc<Mutex<Vec<String>>> = Arc::default();
        {
            let sink = gpu_errors.clone();
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                let msg = e.to_string();
                web_sys::console::error_1(&JsValue::from_str(&format!("[webgpu] {msg}")));
                if let Ok(mut v) = sink.lock() {
                    if v.len() < 64 {
                        v.push(msg);
                    }
                }
            }));
        }
        {
            // A lost device fails every later map and submit without an
            // uncaptured error: say so once, with the browser's reason.
            let sink = gpu_errors.clone();
            device.set_device_lost_callback(move |reason, msg| {
                if reason == wgpu::DeviceLostReason::Destroyed {
                    return;
                }
                let msg = format!("device lost ({reason:?}): {msg}");
                web_sys::console::error_1(&JsValue::from_str(&format!("[webgpu] {msg}")));
                if let Ok(mut v) = sink.lock() {
                    v.push(msg);
                }
            });
        }

        let caps = surface.get_capabilities(&adapter);
        let surface_format = pick_canvas_format(&caps.formats);
        let present = Present::new(&device, surface_format);

        let mut tiles = TileRenderer::new(&device, &queue, FRAMEBUFFER_FORMAT, 32, 32)
            .map_err(|e| format!("TileRenderer::new: {e}"))?;
        tiles.set_collect_decode_errors(true);

        // The CDF 5/3 grid, if this browser's WebGPU draws it correctly:
        // `try_enable_cdf53_gpu` runs the self-test (two known tiles, read
        // back and compared with the CPU decoder) and enables the grid only
        // if it passes; on `Err` passes decode on the CPU. It uses the
        // default cost model and tile cap (no calibration on the web: R3).
        match mode {
            Cdf53Mode::Cpu => console_info("CDF 5/3 decode on the CPU (?cdf53=cpu)"),
            Cdf53Mode::Auto => match tiles.try_enable_cdf53_gpu(&device, &queue).await {
                Ok(()) => console_info("CDF 5/3 decode on the GPU (self-test passed)"),
                Err(e) => console_warn(&format!(
                    "CDF 5/3 self-test failed; decoding on the CPU: {e}"
                )),
            },
        }

        let h264 = match H264Blit::probe(&device, &queue).await {
            Ok(h) => {
                console_info("[renderer] H264 pipeline available");
                Some(h)
            }
            Err(e) => {
                console_warn(&format!(
                    "[renderer] H264 pipeline unsupported on this browser; H.264 frames will be dropped. Reason: {e}"
                ));
                None
            }
        };

        Ok(WebRenderer {
            _instance: instance,
            adapter,
            device,
            queue,
            surface,
            canvas,
            surface_format,
            present,
            tiles,
            fb_view: None,
            fb_epoch: 0,
            h264,
            pending_h264: Vec::new(),
            h264_dropped: 0,
            size: None,
            dirty: false,
            paint_ticks: 0,
            submits: 0,
            gpu_errors,
            errors: Vec::new(),
            decode_errors: Vec::new(),
            paint_us: Vec::new(),
        })
    }

    /// Where CDF 5/3 tiles are reconstructed, from what the tile renderer
    /// actually enabled (not from what was asked for).
    fn cdf53_label(&self) -> &'static str {
        if self.tiles.cdf53_gpu_enabled() {
            "gpu"
        } else {
            "cpu"
        }
    }

    fn configure_surface(&mut self) -> Result<(), String> {
        let (w, h) = self.size.ok_or("framebuffer not sized yet")?;
        let mut config = self
            .surface
            .get_default_config(&self.adapter, w, h)
            .ok_or("the canvas surface is not supported by this adapter")?;
        config.format = self.surface_format;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        config.alpha_mode = wgpu::CompositeAlphaMode::Opaque;
        self.surface.configure(&self.device, &config);
        Ok(())
    }

    /// Resize the canvas, its surface and the framebuffer. A new picture: the
    /// tile renderer resets its paint ledger and keeps what fits.
    fn resize(&mut self, requested_w: u32, requested_h: u32) -> Result<(), String> {
        self.tiles
            .apply_event(
                &self.device,
                &self.queue,
                Event::FrameDimensions {
                    width: requested_w,
                    height: requested_h,
                },
            )
            .map_err(|e| e.to_string())?;
        // The tile renderer refuses an out-of-range size without an error and
        // keeps its framebuffer. Trust the framebuffer, not the wire's number.
        let fb = self.tiles.framebuffer();
        let (width, height) = (fb.width(), fb.height());
        if (width, height) != (requested_w, requested_h) {
            return Ok(());
        }
        // The same size again (the server repeats it): nothing to redo.
        if self.size == Some((width, height)) {
            return Ok(());
        }
        if self.canvas.width() != width || self.canvas.height() != height {
            // Changing the canvas size invalidates the surface; configure again.
            self.canvas.set_width(width);
            self.canvas.set_height(height);
        }
        // The tile renderer's framebuffer is already the new one: point the
        // blit at it before configuring the surface, so a failed configure
        // (retried by `present_frame` on every tick) never leaves a view of the old texture.
        self.size = Some((width, height));
        self.fb_view = Some(
            self.tiles
                .framebuffer()
                .create_view(&wgpu::TextureViewDescriptor::default()),
        );
        self.fb_epoch += 1;
        self.dirty = true;
        self.configure_surface()
    }

    /// Run one rAF's worth of painting. `Err` is only a present failure (the
    /// canvas could not give a texture); the tick is retried next time.
    pub fn paint_tick(&mut self, now_us: u64) -> Result<PaintOutcome, String> {
        let mut outcome = PaintOutcome::default();
        if self.size.is_none() {
            // Pictures pushed before the first FrameDimensions have nothing to
            // be drawn on: close them rather than hold decoder memory.
            for p in self.pending_h264.drain(..) {
                raw::close_frame(&p.frame);
                self.h264_dropped = self.h264_dropped.wrapping_add(1);
            }
            return Ok(outcome);
        }
        self.collect_decode_errors();
        let inputs = PaintInputs {
            dirty: self.dirty,
            h264_pending: self.pending_h264.len(),
            deferred_work: self.tiles.has_deferred_work(),
            errors_readback_pending: self.tiles.errors_readback_pending(),
        };
        if should_skip_paint(&inputs) {
            return Ok(outcome);
        }
        let started = now_ms();
        self.paint_ticks = self.paint_ticks.wrapping_add(1);
        let mut changed = self.dirty;

        // 1. Tiles. Everything the tile renderer submits is on the queue
        //    before the picture pass below is.
        //    When a picture that is not stale will be drawn this tick, the CDF
        //    5/3 tiles under it are held (as the native client does): the
        //    picture covers the screen, so drawing them first would only spend
        //    the flush's GPU budget, and `note_h264` below cancels them.
        //    A CDF 5/3 pass newer than the picture would make it stale, so
        //    nothing newer is ever held.
        let will_draw = self
            .pending_h264
            .iter()
            .any(|p| !self.tiles.h264_stale(p.seq));
        self.tiles.hold_cdf53(will_draw);
        let flushed = self.tiles.flush(&self.device, &self.queue, now_us);
        self.tiles.hold_cdf53(false);
        match flushed {
            Ok(report) => changed |= !report.damage.is_empty(),
            Err(e) => self.errors.push(format!("flush: {e}")),
        }
        // 2. Pictures, in ledger order, over what the flush drew.
        let (drawn, dropped) = self.draw_pending_pictures();
        outcome.pictures_drawn = drawn;
        outcome.pictures_dropped = dropped;
        // `note_h264` does not mark damage (and `mark_all_dirty` after a
        // picture would make the next flush report the whole screen and
        // present again for nothing), so a drawn picture is what says changed.
        changed |= drawn > 0;
        // 3. Present, only if the framebuffer changed.
        self.dirty = false;
        if changed {
            if let Err(e) = self.present_frame() {
                self.dirty = true; // retry next tick
                return Err(e);
            }
            outcome.presented = true;
            self.submits = self.submits.wrapping_add(1);
        }
        self.note_paint_us(((now_ms() - started) * 1000.0) as u32);
        Ok(outcome)
    }

    fn draw_pending_pictures(&mut self) -> (u32, u32) {
        if self.pending_h264.is_empty() {
            return (0, 0);
        }
        let pending = std::mem::take(&mut self.pending_h264);
        let mut kept: Vec<JsValue> = Vec::new();
        let mut kept_seqs: Vec<u32> = Vec::new();
        // The newest picture kept this tick: a second, older picture of the
        // same tick is judged against it as well as against the ledger.
        let mut newest_kept: Option<u32> = None;
        let mut dropped = 0u32;
        let mut drawn = 0u32;
        for p in &pending {
            // Older than a picture already drawn, or than any tile painted or
            // queued: it covers the whole screen, so drawing it would undo a
            // newer paint. It is closed with the rest, not drawn.
            if self.tiles.h264_stale(p.seq)
                || newest_kept.is_some_and(|n| frame_seq_is_newer(n, p.seq))
            {
                dropped += 1;
                continue;
            }
            newest_kept = Some(p.seq);
            kept.push(p.frame.clone());
            kept_seqs.push(p.seq);
        }
        if let (Some(h264), Some((w, h)), false) = (self.h264.as_ref(), self.size, kept.is_empty())
        {
            let view = self
                .tiles
                .framebuffer()
                .create_view(&wgpu::TextureViewDescriptor::default());
            match raw::raw_view(&view) {
                Ok(raw_view) => {
                    // A frame that failed to import is skipped and the others
                    // are still drawn: only those count, so a partial failure
                    // still presents what was drawn.
                    let (drawn_idx, failure) = h264.draw(&raw_view, w, h, &kept);
                    drawn = drawn_idx.len() as u32;
                    // Only a picture actually drawn goes in the ledger: noting
                    // it cancels the CDF 5/3 work held under it, which a
                    // picture that failed to import does not cover.
                    for i in drawn_idx {
                        self.tiles.note_h264(kept_seqs[i]);
                    }
                    if let Some(e) = failure {
                        self.errors.push(format!("h264: {e}"));
                    }
                }
                Err(e) => self.errors.push(format!("h264: {e}")),
            }
        }
        self.h264_dropped = self.h264_dropped.wrapping_add(dropped);
        for p in &pending {
            raw::close_frame(&p.frame);
        }
        (drawn, dropped)
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "wgpu::CurrentSurfaceTexture is a foreign enum; every other case means: try again next tick"
    )]
    fn present_frame(&mut self) -> Result<(), String> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.configure_surface()?;
                return Err("canvas surface was outdated; reconfigured".into());
            }
            other => {
                // Also the way back from a configure that failed in `resize`:
                // the canvas is unconfigured, so try again on every tick.
                let configured = self.configure_surface();
                return Err(match configured {
                    Ok(()) => format!("get_current_texture: {other:?}; reconfigured"),
                    Err(e) => format!("get_current_texture: {other:?}; configure: {e}"),
                });
            }
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let Some(fb_view) = self.fb_view.as_ref() else {
            return Err("no framebuffer view".into());
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-present"),
            });
        self.present
            .encode(&self.device, &mut encoder, fb_view, self.fb_epoch, &target);
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }

    fn collect_decode_errors(&mut self) {
        for e in self.tiles.take_decode_errors() {
            self.decode_errors.extend([
                u32::from(e.codec as u8),
                u32::from(e.tile_x),
                u32::from(e.tile_y),
                u32::from(e.code as u8),
            ]);
        }
    }

    fn note_paint_us(&mut self, us: u32) {
        self.paint_us.push(us);
        if self.paint_us.len() >= PAINT_US_CAP {
            self.paint_us.drain(..PAINT_US_CAP / 2);
        }
    }

    /// Drop pictures and forget what the old session painted.
    fn reset_session(&mut self) {
        for p in self.pending_h264.drain(..) {
            raw::close_frame(&p.frame);
            self.h264_dropped = self.h264_dropped.wrapping_add(1);
        }
        if let Err(e) = self.tiles.reset_session(&self.device, &self.queue) {
            self.errors.push(format!("session reset: {e}"));
        }
        self.dirty = true;
    }
}

impl TileSink for WebRenderer {
    fn apply(&mut self, ev: Event) -> Result<(), String> {
        if let Event::FrameDimensions { width, height } = ev {
            return self.resize(width, height);
        }
        self.dirty = true;
        self.tiles
            .apply_event(&self.device, &self.queue, ev)
            .map_err(|e| e.to_string())
    }
}

/// `debugState()`'s shape: the old renderer's keys (the page log and the
/// failure dumps print them) with the tile renderer's own state spliced in.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebState<'a, T: Serialize> {
    submit_count: u32,
    h264_queue: usize,
    framebuffer_dirty: bool,
    fb_width: u32,
    fb_height: u32,
    has_texture: bool,
    canvas_width: u32,
    canvas_height: u32,
    paint_tick: u32,
    /// Kept under its old key; true while a readback is in flight or untaken.
    errors_readback_in_flight: bool,
    /// The old `solidQueue` / `palRleQueue` keys, from the tile renderer's pending counts.
    solid_queue: usize,
    pal_rle_queue: usize,
    cdf53_path: &'a str,
    h264_supported: bool,
    h264_dropped: u32,
    #[serde(flatten)]
    tiles: T,
}

fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsValue> {
    v.serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
impl WebRenderer {
    /// Open the page's renderer on `canvas`. `cdf53` is the page URL's
    /// `cdf53` parameter: `""`/`"auto"` or `"cpu"` (see `Cdf53Mode`).
    /// Rejects with a string starting `WEBGPU_UNAVAILABLE` when the browser has
    /// no usable WebGPU.
    #[wasm_bindgen(js_name = create)]
    pub async fn create(
        canvas: web_sys::HtmlCanvasElement,
        cdf53: String,
    ) -> Result<WebRenderer, JsValue> {
        Self::create_inner(canvas, Cdf53Mode::parse(&cdf53))
            .await
            .map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(getter, js_name = h264Supported)]
    pub fn h264_supported(&self) -> bool {
        self.h264.is_some()
    }

    #[wasm_bindgen(getter, js_name = fbWidth)]
    pub fn fb_width(&self) -> u32 {
        self.size.map_or(0, |s| s.0)
    }

    #[wasm_bindgen(getter, js_name = fbHeight)]
    pub fn fb_height(&self) -> u32 {
        self.size.map_or(0, |s| s.1)
    }

    /// Frames presented so far (an idle tick adds none).
    #[wasm_bindgen(getter, js_name = submitCount)]
    pub fn submit_count(&self) -> u32 {
        self.submits
    }

    #[wasm_bindgen(getter, js_name = paintTickCount)]
    pub fn paint_tick_count(&self) -> u32 {
        self.paint_ticks
    }

    /// Whether CDF 5/3 work is waiting for a later tick (budget or throttle).
    pub fn has_deferred_work(&self) -> bool {
        self.tiles.has_deferred_work()
    }

    /// `"gpu"` or `"cpu"`: where CDF 5/3 tiles are reconstructed.
    #[wasm_bindgen(getter, js_name = cdf53Path)]
    pub fn cdf53_path(&self) -> String {
        self.cdf53_label().to_string()
    }

    /// Hand over a decoded `VideoFrame` (ownership: this renderer closes it)
    /// with the `frame_seq` of its access unit.
    #[wasm_bindgen(js_name = pushH264)]
    pub fn push_h264(&mut self, frame: JsValue, frame_seq: u32) {
        if self.h264.is_none() {
            raw::close_frame(&frame);
            self.h264_dropped = self.h264_dropped.wrapping_add(1);
            return;
        }
        // Not `dirty`: a picture the ledger drops changes nothing, so only a
        // drawn one makes the tick present (`paint_tick`).
        self.pending_h264.push(PendingPicture {
            frame,
            seq: frame_seq,
        });
    }

    /// One rAF tick; the number of frames presented (0 or 1). Errors are kept
    /// for `takeErrors`.
    #[wasm_bindgen(js_name = paintTick)]
    pub fn paint_tick_js(&mut self, now_us: u64) -> u32 {
        // A u64 crosses as a BigInt, exactly what `nowUs()` in main.ts returns
        // and what `onTimeout` takes: one clock type for every call.
        match self.paint_tick(now_us) {
            Ok(o) => u32::from(o.presented),
            Err(e) => {
                self.errors.push(e);
                0
            }
        }
    }

    /// The transport closed: drop pending pictures and forget the session's
    /// paint state. Call before closing the JS decoder.
    #[wasm_bindgen(js_name = onSessionReset)]
    pub fn on_session_reset(&mut self) {
        self.reset_session();
    }

    /// Errors since the last call: uncaptured GPU errors and failed steps.
    #[wasm_bindgen(js_name = takeErrors)]
    pub fn take_errors(&mut self) -> Array {
        let mut out = std::mem::take(&mut self.errors);
        if let Ok(mut g) = self.gpu_errors.lock() {
            out.append(&mut g);
        }
        out.iter().map(|s| JsValue::from_str(s)).collect()
    }

    /// `[codec, tile_x, tile_y, code, ...]` for tiles the GPU decode rejected.
    #[wasm_bindgen(js_name = takeDecodeErrors)]
    pub fn take_decode_errors(&mut self) -> Vec<u32> {
        self.collect_decode_errors();
        std::mem::take(&mut self.decode_errors)
    }

    /// `[count, p50, p99, max]` microseconds of the non-skipped ticks since the
    /// last call (the section 7 bench reads it through `__ghostframePaintUs`).
    #[wasm_bindgen(js_name = paintUsStats)]
    pub fn paint_us_stats(&mut self) -> Vec<f64> {
        let mut v = std::mem::take(&mut self.paint_us);
        v.sort_unstable();
        let at = |q: f64| {
            v.get(((v.len() as f64 - 1.0) * q).round() as usize)
                .copied()
                .map_or(0.0, f64::from)
        };
        vec![
            v.len() as f64,
            at(0.5),
            at(0.99),
            v.last().copied().map_or(0.0, f64::from),
        ]
    }

    /// The old `__ghostframeRendererState()`.
    #[wasm_bindgen(js_name = debugState)]
    pub fn debug_state(&self) -> Result<JsValue, JsValue> {
        let tile_state = self.tiles.debug_state();
        to_js(&WebState {
            submit_count: self.submits,
            h264_queue: self.pending_h264.len(),
            framebuffer_dirty: self.dirty,
            fb_width: self.fb_width(),
            fb_height: self.fb_height(),
            has_texture: self.size.is_some(),
            canvas_width: self.canvas.width(),
            canvas_height: self.canvas.height(),
            paint_tick: self.paint_ticks,
            errors_readback_in_flight: self.tiles.errors_readback_pending(),
            solid_queue: tile_state.pending_solid,
            pal_rle_queue: tile_state.pending_palrle,
            cdf53_path: self.cdf53_label(),
            h264_supported: self.h264.is_some(),
            h264_dropped: self.h264_dropped,
            tiles: tile_state,
        })
    }

    /// The old `__ghostframeTile(x, y)`: what the renderer last did to a tile,
    /// or `null` for a tile off the grid.
    #[wasm_bindgen(js_name = debugTile)]
    pub fn debug_tile(&self, tile_x: u32, tile_y: u32) -> Result<JsValue, JsValue> {
        match self.tiles.debug_tile(tile_x, tile_y) {
            Some(t) => to_js(&t),
            None => Ok(JsValue::NULL),
        }
    }

    /// `{ device, texture }`: the raw `GPUDevice` and the framebuffer's
    /// `GPUTexture`, for `window.__ghostframeRenderer` (the e2e suite reads
    /// `texture.width` and `texture.height`); `undefined` before the first size.
    #[wasm_bindgen(js_name = debugHandles)]
    pub fn debug_handles(&self) -> JsValue {
        if self.size.is_none() {
            return JsValue::UNDEFINED;
        }
        let device = self
            .device
            .as_webgpu()
            .map(|d| AsRef::<JsValue>::as_ref(d).clone());
        let texture = self
            .tiles
            .framebuffer()
            .as_webgpu()
            .map(|t| AsRef::<JsValue>::as_ref(t).clone());
        match (device, texture) {
            (Some(device), Some(texture)) => raw::obj(&[("device", device), ("texture", texture)]),
            _ => JsValue::UNDEFINED,
        }
    }

    /// The `w` x `h` rectangle at (`x`, `y`) of the framebuffer as RGBA bytes
    /// (a `Promise<Uint8Array>`). The copy is submitted now, after everything
    /// already submitted; the promise only waits for the bytes.
    #[wasm_bindgen(js_name = readPixelRect)]
    pub fn read_pixel_rect(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Promise, JsValue> {
        let (fw, fh) = self
            .size
            .ok_or_else(|| JsValue::from_str("framebuffer not sized yet"))?;
        if w == 0 || h == 0 {
            return Ok(Promise::resolve(&JsValue::from(
                Uint8Array::new_with_length(0),
            )));
        }
        if x.checked_add(w).is_none_or(|e| e > fw) || y.checked_add(h).is_none_or(|e| e > fh) {
            return Err(JsValue::from_str(&format!(
                "rect {w}x{h} at ({x},{y}) is outside the {fw}x{fh} framebuffer"
            )));
        }
        // The browser's reason for a failed map never reaches wgpu's error, and
        // a validation error would arrive as an event after the caller gave up:
        // catch both here so a failure says why.
        let oom = self.device.push_error_scope(wgpu::ErrorFilter::OutOfMemory);
        let validation = self.device.push_error_scope(wgpu::ErrorFilter::Validation);
        let pending = ghostframe_render::readback::begin_texture_read(
            &self.device,
            &self.queue,
            self.tiles.framebuffer(),
            x,
            y,
            w,
            h,
        );
        let validation = validation.pop();
        let oom = oom.pop();
        Ok(future_to_promise(async move {
            let caught = [validation.await, oom.await];
            let bytes = pending.finish().await.map_err(|e| {
                let why: Vec<String> = caught.iter().flatten().map(|c| c.to_string()).collect();
                JsValue::from_str(&format!("{e}; GPU errors in the readback: {why:?}"))
            })?;
            Ok(Uint8Array::from(bytes.as_slice()).into())
        }))
    }

    /// The old `__readGradientGolden()`; overwrites the framebuffer.
    #[wasm_bindgen(js_name = readGradientGolden)]
    pub fn read_gradient_golden(&self) -> Result<Promise, JsValue> {
        if self.size.is_none() {
            return Err(JsValue::from_str("framebuffer not sized yet"));
        }
        Ok(super::debug::read_gradient_golden(
            &self.device,
            &self.queue,
            self.tiles.framebuffer(),
        ))
    }
}
