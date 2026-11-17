// SPDX-License-Identifier: MIT OR Apache-2.0
//! The page's e2e bookkeeping (`ghostframe-web-client/src/hooks.ts`), called
//! from Rust until PR 4 moves it here. Every entry point queues its calls and
//! dispatches them with no Rust borrow held: a hook may call back into
//! `DebugApi`. A hook that throws is logged once per method and skipped.

#[cfg(target_arch = "wasm32")]
use std::cell::RefCell;
#[cfg(target_arch = "wasm32")]
use std::collections::HashSet;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

#[cfg(target_arch = "wasm32")]
use crate::app::debug_api::DebugApi;
use crate::app::session::Drained;
#[cfg(target_arch = "wasm32")]
use crate::web::raw::{js_message, obj};
use crate::web::route::Routed;

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
extern "C" {
    pub type Hooks;
    #[wasm_bindgen(method, catch)]
    fn attach(this: &Hooks, api: DebugApi) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    fn events(this: &Hooks, records: JsValue, at_ms: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = datagramCounts)]
    fn datagram_counts(this: &Hooks, tile: u32, frame: u32) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = rafTick)]
    fn raf_tick(this: &Hooks, n: u32) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = beforePaint)]
    fn before_paint(this: &Hooks, at_ms: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = afterPaint)]
    fn after_paint(this: &Hooks, info: JsValue) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    fn swallowed(this: &Hooks, site: &str, message: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = rendererError)]
    fn renderer_error(this: &Hooks, tick: u32, message: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = serverStream)]
    fn server_stream(this: &Hooks, samples: u32, frames: u32) -> Result<(), JsValue>;
}

/// One call, queued while `App` is borrowed.
#[derive(Debug)]
pub enum HookCall {
    Events {
        at_ms: f64,
        records: Vec<Routed>,
    },
    DatagramCounts {
        tile: u32,
        frame: u32,
    },
    RafTick(u32),
    BeforePaint {
        at_ms: f64,
    },
    AfterPaint {
        drained: Drained,
        raf_ticks: u32,
        nack_sent: u32,
    },
    Swallowed {
        site: String,
        message: String,
    },
    RendererError {
        tick: u32,
        message: String,
    },
    ServerStream {
        samples: u32,
        frames: u32,
    },
}

#[cfg(target_arch = "wasm32")]
thread_local! {
    static WARNED: RefCell<HashSet<&'static str>> = RefCell::new(HashSet::new());
}

#[cfg(target_arch = "wasm32")]
fn check(method: &'static str, r: Result<(), JsValue>) {
    if let Err(e) = r {
        if WARNED.with(|w| w.borrow_mut().insert(method)) {
            web_sys::console::warn_1(&format!("hooks.{method} threw: {}", js_message(&e)).into());
        }
    }
}

/// `Routed` as the old `to_js(&route(..))` returned it: plain `to_value`
/// (`Routed` has no map fields, so maps-as-objects would be identical).
#[cfg(target_arch = "wasm32")]
fn records_js(records: &[Routed]) -> JsValue {
    serde_wasm_bindgen::to_value(records).unwrap_or(JsValue::NULL)
}

#[cfg(target_arch = "wasm32")]
pub fn attach(hooks: &Hooks, api: DebugApi) {
    check("attach", hooks.attach(api));
}

/// Run `calls` in order. Call with no `App` or renderer borrow held.
#[cfg(target_arch = "wasm32")]
pub fn dispatch(hooks: Option<&Hooks>, calls: Vec<HookCall>) {
    let Some(h) = hooks else { return };
    for c in calls {
        match c {
            HookCall::Events { at_ms, records } => {
                check("events", h.events(records_js(&records), at_ms))
            }
            HookCall::DatagramCounts { tile, frame } => {
                check("datagramCounts", h.datagram_counts(tile, frame))
            }
            HookCall::RafTick(n) => check("rafTick", h.raf_tick(n)),
            HookCall::BeforePaint { at_ms } => check("beforePaint", h.before_paint(at_ms)),
            HookCall::AfterPaint {
                drained,
                raf_ticks,
                nack_sent,
            } => {
                let info = obj(&[
                    ("solid", drained.solid.into()),
                    ("palrle", drained.palrle.into()),
                    ("cdf53", drained.cdf53.into()),
                    ("h264", drained.h264.into()),
                    ("rafTicks", raf_ticks.into()),
                    ("nackSent", nack_sent.into()),
                ]);
                check("afterPaint", h.after_paint(info));
            }
            HookCall::Swallowed { site, message } => {
                check("swallowed", h.swallowed(&site, &message))
            }
            HookCall::RendererError { tick, message } => {
                check("rendererError", h.renderer_error(tick, &message))
            }
            HookCall::ServerStream { samples, frames } => {
                check("serverStream", h.server_stream(samples, frames))
            }
        }
    }
}
