// SPDX-License-Identifier: MIT OR Apache-2.0
//! The e2e `window` globals (port of `hooks.ts`, `cdf53_globals.ts`,
//! `diagnostics.ts` and `health.ts`'s console ring), installed only under
//! `?e2e`. The logs are real JS arrays Rust appends to, one small object per
//! event, so a poll reads `.length` for free; the counters stay in the
//! `Recorder` and are served by getters.
//!
//! Every closure installed here lives as long as the page (`Closure::forget`):
//! `window` holds it, and a closure freed while the page can still call it
//! throws "closure invoked after being dropped". Every borrow is a `try_borrow`:
//! a conflict gives a thrown error or a rejected promise naming the global,
//! never a panic (a panic aborts the module).

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use ghostframe_client_core::ClientCore;
use js_sys::{Array, Function, Promise, Reflect, Uint8Array};
use serde::Serialize;
use wasm_bindgen::closure::WasmClosure;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, JsFuture};

use crate::app::capped::{CappedCount, MAX_RECORDED_ENTRIES};
use crate::app::recorder::{LogEntry, Recorder};
use crate::web::logic::summarize_us;
use crate::web::raw::obj;
use crate::web::renderer::WebRenderer;

// The logs.
const RECORDED_TILES: &str = "__ghostframeRecordedTiles";
const RECORDED_CODECS: &str = "__ghostframeRecordedCodecs";
const RECORDED_FLAGS: &str = "__ghostframeRecordedFlags";
const RECORDED_RESIZES: &str = "__ghostframeRecordedResizes";
const FRAME_PAINTS: &str = "__ghostframe_framePaints";
const PALETTE_WRITES: &str = "__h2_clientPaletteWrites";
const TILE_PUSH_LOG: &str = "__h5_tilePushLog";
// Getters.
const STATS: &str = "__ghostframeStats";
const RAF_TICKS: &str = "__ghostframeRafTicks";
const STATS_SAMPLES: &str = "__ghostframeStatsSamples";
const STATS_FRAMES: &str = "__ghostframeStatsFrames";
const CDF53_DISPATCH_SEEN: &str = "__cdf53DispatchSeen";
const CDF53_PUSHED_TO_QUEUE: &str = "__cdf53PushedToQueue";
const RENDERER: &str = "__ghostframeRenderer";
// Functions.
const HEALTH: &str = "__ghostframeHealth";
const RENDERER_STATE: &str = "__ghostframeRendererState";
const TILE: &str = "__ghostframeTile";
const PAINT_US: &str = "__ghostframePaintUs";
const TICK_US: &str = "__ghostframeTickUs";
const CDF53_COVERAGE_SUMMARY: &str = "__cdf53CoverageSummary";
const READ_PIXEL: &str = "__readPixel";
const READ_PIXEL_RECT: &str = "__readPixelRect";
const READ_GRADIENT_GOLDEN: &str = "__readGradientGolden";

/// Every global `install` defines on `window`.
pub const E2E_GLOBALS: &[&str] = &[
    RECORDED_TILES,
    RECORDED_CODECS,
    RECORDED_FLAGS,
    RECORDED_RESIZES,
    FRAME_PAINTS,
    PALETTE_WRITES,
    TILE_PUSH_LOG,
    STATS,
    RAF_TICKS,
    STATS_SAMPLES,
    STATS_FRAMES,
    CDF53_DISPATCH_SEEN,
    CDF53_PUSHED_TO_QUEUE,
    RENDERER,
    HEALTH,
    RENDERER_STATE,
    TILE,
    PAINT_US,
    TICK_US,
    CDF53_COVERAGE_SUMMARY,
    READ_PIXEL,
    READ_PIXEL_RECT,
    READ_GRADIENT_GOLDEN,
];

/// What the globals read.
pub struct DebugHandles {
    pub renderer: Rc<RefCell<WebRenderer>>,
    pub core: Rc<OnceCell<Rc<RefCell<ClientCore>>>>,
    pub recorder: Rc<RefCell<Recorder>>,
    pub tick_us: Rc<RefCell<Vec<u32>>>,
}

/// One `window` log: a JS array and the Rust-side count that decides when to
/// trim it (`length` is never read back).
pub struct JsLog {
    array: Array,
    count: RefCell<CappedCount>,
}

impl JsLog {
    pub fn new() -> Self {
        Self::with_cap(MAX_RECORDED_ENTRIES)
    }

    /// A log with a small cap (the browser test trims at twice it).
    pub fn with_cap(cap: usize) -> Self {
        Self {
            array: Array::new(),
            count: RefCell::new(CappedCount::new(cap)),
        }
    }

    pub fn array(&self) -> &Array {
        &self.array
    }

    /// Append `v`; at twice the cap, drop the oldest entries down to the cap.
    pub fn push(&self, v: &JsValue) {
        self.array.push(v);
        let trim = self.count.borrow_mut().push();
        if let Some(n) = trim {
            self.array.splice_many(0, n as u32, &[]);
        }
    }
}

impl Default for JsLog {
    fn default() -> Self {
        Self::new()
    }
}

/// The seven logs `append` feeds.
pub struct Logs {
    tiles: JsLog,
    codecs: JsLog,
    flags: JsLog,
    resizes: JsLog,
    frame_paints: JsLog,
    palette_writes: JsLog,
    tile_push: JsLog,
}

impl Logs {
    fn new() -> Self {
        Self {
            tiles: JsLog::new(),
            codecs: JsLog::new(),
            flags: JsLog::new(),
            resizes: JsLog::new(),
            frame_paints: JsLog::new(),
            palette_writes: JsLog::new(),
            tile_push: JsLog::new(),
        }
    }
}

/// A plain object, maps as objects (the health snapshot has maps).
fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsValue> {
    v.serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

/// Push each entry to its log.
pub fn append(logs: &Logs, entries: Vec<LogEntry>) {
    for e in entries {
        let (log, v) = match &e {
            LogEntry::Codec(c) => (&logs.codecs, Ok(JsValue::from(*c))),
            LogEntry::Flag(f) => (&logs.flags, Ok(JsValue::from(*f))),
            LogEntry::Tile(t) => (&logs.tiles, to_js(t)),
            LogEntry::Resize(r) => (&logs.resizes, to_js(r)),
            LogEntry::FramePaint(p) => (&logs.frame_paints, to_js(p)),
            LogEntry::PaletteWrite(p) => (&logs.palette_writes, to_js(p)),
            LogEntry::TilePush(p) => (&logs.tile_push, to_js(p)),
        };
        // A flat struct always serialises; skipping is the safe answer if not.
        if let Ok(v) = v {
            log.push(&v);
        }
    }
}

fn busy(name: &str) -> JsValue {
    js_sys::Error::new(&format!("{name}: state is busy (a borrow is held)")).into()
}

fn rejected(name: &str) -> Promise {
    Promise::reject(&busy(name))
}

/// `window[name] = closure`, kept alive for the page.
fn set_fn<T: ?Sized + WasmClosure>(window: &web_sys::Window, name: &str, f: Closure<T>) {
    let _ = Reflect::set(window, &JsValue::from_str(name), f.as_ref());
    // `window` holds it for the page's life; dropping it would make a later
    // call throw.
    f.forget();
}

/// `Object.defineProperty(window, name, { get, configurable: true })`.
fn set_getter(
    window: &web_sys::Window,
    name: &'static str,
    get: impl Fn() -> Result<JsValue, JsValue> + 'static,
) {
    let f = Closure::<dyn Fn() -> Result<JsValue, JsValue>>::new(get);
    let desc = obj(&[("get", f.as_ref().clone()), ("configurable", true.into())]);
    let _ = Reflect::define_property(window, &JsValue::from_str(name), desc.unchecked_ref());
    // The property holds it for the page's life.
    f.forget();
}

fn set_array(window: &web_sys::Window, name: &str, log: &JsLog) {
    let _ = Reflect::set(window, &JsValue::from_str(name), log.array());
}

fn secs(v: Vec<f64>) -> JsValue {
    v.into_iter()
        .map(JsValue::from_f64)
        .collect::<Array>()
        .into()
}

/// A `Promise<Uint8Array>` as a `Promise<number[]>`.
fn bytes_as_numbers(p: Promise) -> Promise {
    future_to_promise(async move {
        let v = JsFuture::from(p).await?;
        let b = Uint8Array::new(&v).to_vec();
        Ok(b.into_iter()
            .map(|x| JsValue::from_f64(f64::from(x)))
            .collect::<Array>()
            .into())
    })
}

/// Define every e2e global on `window`. Call once, before bootstrap.
pub fn install(window: &web_sys::Window, h: DebugHandles) -> Logs {
    let logs = Logs::new();
    set_array(window, RECORDED_TILES, &logs.tiles);
    set_array(window, RECORDED_CODECS, &logs.codecs);
    set_array(window, RECORDED_FLAGS, &logs.flags);
    set_array(window, RECORDED_RESIZES, &logs.resizes);
    set_array(window, FRAME_PAINTS, &logs.frame_paints);
    set_array(window, PALETTE_WRITES, &logs.palette_writes);
    set_array(window, TILE_PUSH_LOG, &logs.tile_push);

    // Getters over the recorder.
    let rec = h.recorder.clone();
    let read = move |name: &'static str, f: fn(&Recorder) -> JsValue| {
        let rec = rec.clone();
        move || -> Result<JsValue, JsValue> {
            rec.try_borrow().map(|r| f(&r)).map_err(|_| busy(name))
        }
    };
    set_getter(
        window,
        STATS,
        read(STATS, |r| {
            obj(&[
                ("tileDatagrams", r.tile_datagrams.into()),
                ("frameDatagrams", r.frame_datagrams.into()),
            ])
        }),
    );
    set_getter(window, RAF_TICKS, read(RAF_TICKS, |r| r.raf_ticks.into()));
    set_getter(
        window,
        STATS_SAMPLES,
        read(STATS_SAMPLES, |r| r.stats_samples.into()),
    );
    set_getter(
        window,
        STATS_FRAMES,
        read(STATS_FRAMES, |r| r.stats_frames.into()),
    );
    set_getter(
        window,
        CDF53_DISPATCH_SEEN,
        read(CDF53_DISPATCH_SEEN, |r| r.cdf53_dispatch_seen.into()),
    );
    set_getter(
        window,
        CDF53_PUSHED_TO_QUEUE,
        read(CDF53_PUSHED_TO_QUEUE, |r| r.cdf53_pushed_to_queue.into()),
    );
    let renderer = h.renderer.clone();
    set_getter(window, RENDERER, move || {
        // `undefined` while busy: a poll before the first size reads the same.
        Ok(renderer
            .try_borrow()
            .map_or(JsValue::UNDEFINED, |r| r.debug_handles()))
    });

    // Functions.
    let rec = h.recorder.clone();
    set_fn(
        window,
        HEALTH,
        Closure::<dyn Fn() -> Result<JsValue, JsValue>>::new(move || {
            let r = rec.try_borrow().map_err(|_| busy(HEALTH))?;
            to_js(&r.health.snapshot())
        }),
    );
    let renderer = h.renderer.clone();
    set_fn(
        window,
        RENDERER_STATE,
        Closure::<dyn Fn() -> Result<JsValue, JsValue>>::new(move || {
            renderer
                .try_borrow()
                .map_err(|_| busy(RENDERER_STATE))?
                .debug_state()
        }),
    );
    let renderer = h.renderer.clone();
    set_fn(
        window,
        TILE,
        Closure::<dyn Fn(f64, f64) -> Result<JsValue, JsValue>>::new(move |x, y| {
            renderer
                .try_borrow()
                .map_err(|_| busy(TILE))?
                .debug_tile(x as u32, y as u32)
        }),
    );
    let renderer = h.renderer.clone();
    set_fn(
        window,
        PAINT_US,
        Closure::<dyn Fn() -> Result<JsValue, JsValue>>::new(move || {
            let mut r = renderer.try_borrow_mut().map_err(|_| busy(PAINT_US))?;
            Ok(secs(r.paint_us_stats()))
        }),
    );
    let tick_us = h.tick_us.clone();
    set_fn(
        window,
        TICK_US,
        Closure::<dyn Fn() -> Result<JsValue, JsValue>>::new(move || {
            let mut v = tick_us.try_borrow_mut().map_err(|_| busy(TICK_US))?;
            Ok(secs(summarize_us(std::mem::take(&mut *v))))
        }),
    );
    let core = h.core.clone();
    set_fn(
        window,
        CDF53_COVERAGE_SUMMARY,
        Closure::<dyn Fn() -> Result<JsValue, JsValue>>::new(move || {
            let Some(core) = core.get() else {
                return Ok(JsValue::UNDEFINED);
            };
            let core = core
                .try_borrow()
                .map_err(|_| busy(CDF53_COVERAGE_SUMMARY))?;
            Ok(obj(&[
                ("summary", crate::core::cdf53_coverage_js(&core)?),
                ("incomplete", crate::core::cdf53_incomplete_js(&core, 16)?),
            ]))
        }),
    );
    let renderer = h.renderer.clone();
    set_fn(
        window,
        READ_PIXEL,
        Closure::<dyn Fn(f64, f64) -> Promise>::new(move |x, y| {
            let Ok(r) = renderer.try_borrow() else {
                return rejected(READ_PIXEL);
            };
            match r.read_pixel_rect(x as u32, y as u32, 1, 1) {
                Ok(p) => bytes_as_numbers(p),
                Err(e) => Promise::reject(&e),
            }
        }),
    );
    let renderer = h.renderer.clone();
    set_fn(
        window,
        READ_PIXEL_RECT,
        Closure::<dyn Fn(f64, f64, f64, f64) -> Promise>::new(move |x, y, w, hh| {
            let (w, hh) = (w as u32, hh as u32);
            if w == 0 || hh == 0 {
                let empty: Promise = Promise::resolve(&JsValue::from(Array::new()));
                return empty;
            }
            let Ok(r) = renderer.try_borrow() else {
                return rejected(READ_PIXEL_RECT);
            };
            match r.read_pixel_rect(x as u32, y as u32, w, hh) {
                Ok(p) => bytes_as_numbers(p),
                Err(e) => Promise::reject(&e),
            }
        }),
    );
    let renderer = h.renderer;
    set_fn(
        window,
        READ_GRADIENT_GOLDEN,
        Closure::<dyn Fn() -> Promise>::new(move || {
            let Ok(r) = renderer.try_borrow() else {
                return rejected(READ_GRADIENT_GOLDEN);
            };
            r.read_gradient_golden()
                .unwrap_or_else(|e| Promise::reject(&e))
        }),
    );
    logs
}

/// Mirror `console.log/info/warn/error` lines into the recorder's health ring
/// (`health.ts` did this), keeping the page's own behaviour. A Rust closure
/// cannot take JS rest arguments, so the wrapper is this small JS function; it
/// is the only script text outside `index.html`.
pub fn install_console_ring(recorder: &Rc<RefCell<Recorder>>) {
    let global = js_sys::global();
    let Ok(console) = Reflect::get(&global, &JsValue::from_str("console")) else {
        return;
    };
    if !console.is_object() {
        return;
    }
    let wrap = Function::new_with_args(
        "orig, note",
        "return function (...a) { \
           try { note(a.map(String).join(' ')) } catch (_) {} \
           return orig.apply(this, a) }",
    );
    for level in ["log", "info", "warn", "error"] {
        let Ok(orig) = Reflect::get(&console, &JsValue::from_str(level)) else {
            continue;
        };
        if !orig.is_function() {
            continue;
        }
        let rec = recorder.clone();
        let note = Closure::<dyn Fn(String)>::new(move |line: String| {
            // A conflict means a hook is mid-borrow: drop the line.
            if let Ok(mut r) = rec.try_borrow_mut() {
                r.health.note(&format!("{level}: {line}"));
            }
        });
        let Ok(wrapped) = wrap.call2(&JsValue::NULL, &orig, note.as_ref()) else {
            continue;
        };
        let _ = Reflect::set(&console, &JsValue::from_str(level), &wrapped);
        // `console` holds the wrapper, which holds `note`, for the page's life.
        note.forget();
    }
}
