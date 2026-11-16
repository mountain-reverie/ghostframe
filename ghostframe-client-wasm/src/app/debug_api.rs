// SPDX-License-Identifier: MIT OR Apache-2.0
//! What `hooks.ts` reads from Rust: the renderer's debug state and readbacks,
//! the core's coverage and counts, the page log. Names match what
//! `diagnostics.ts` (`PixelReader`) and `stats_log.ts` (`StatsRenderer`,
//! `StatsCore`) call. Every method uses `try_borrow`: a call made while the
//! client holds a borrow is a rejected call, never a panic (a panic aborts the
//! module).

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use ghostframe_client_core::ClientCore;
use js_sys::Promise;
use wasm_bindgen::prelude::*;

use crate::app::dom::PageLog;
use crate::web::logic::summarize_us;
use crate::web::renderer::WebRenderer;

#[wasm_bindgen]
pub struct DebugApi {
    pub(crate) renderer: Rc<RefCell<WebRenderer>>,
    pub(crate) core: Rc<OnceCell<Rc<RefCell<ClientCore>>>>,
    pub(crate) log: Rc<PageLog>,
    pub(crate) tick_us: Rc<RefCell<Vec<u32>>>,
}

fn busy() -> JsValue {
    JsValue::from_str("ghostframe: renderer or core busy (a hook called back during a step)")
}

#[wasm_bindgen]
impl DebugApi {
    #[wasm_bindgen(js_name = debugState)]
    pub fn debug_state(&self) -> Result<JsValue, JsValue> {
        self.renderer
            .try_borrow()
            .map_err(|_| busy())?
            .debug_state()
    }

    #[wasm_bindgen(js_name = debugTile)]
    pub fn debug_tile(&self, x: u32, y: u32) -> Result<JsValue, JsValue> {
        self.renderer
            .try_borrow()
            .map_err(|_| busy())?
            .debug_tile(x, y)
    }

    #[wasm_bindgen(js_name = debugHandles)]
    pub fn debug_handles(&self) -> JsValue {
        self.renderer
            .try_borrow()
            .map_or(JsValue::UNDEFINED, |r| r.debug_handles())
    }

    #[wasm_bindgen(js_name = paintUsStats)]
    pub fn paint_us_stats(&self) -> Vec<f64> {
        self.renderer
            .try_borrow_mut()
            .map_or_else(|_| Vec::new(), |mut r| r.paint_us_stats())
    }

    #[wasm_bindgen(js_name = tickUsStats)]
    pub fn tick_us_stats(&self) -> Vec<f64> {
        let taken = self
            .tick_us
            .try_borrow_mut()
            .map(|mut v| std::mem::take(&mut *v))
            .unwrap_or_default();
        summarize_us(taken)
    }

    #[wasm_bindgen(js_name = readPixelRect)]
    pub fn read_pixel_rect(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Promise, JsValue> {
        self.renderer
            .try_borrow()
            .map_err(|_| busy())?
            .read_pixel_rect(x, y, w, h)
    }

    #[wasm_bindgen(js_name = readGradientGolden)]
    pub fn read_gradient_golden(&self) -> Result<Promise, JsValue> {
        self.renderer
            .try_borrow()
            .map_err(|_| busy())?
            .read_gradient_golden()
    }

    #[wasm_bindgen(getter, js_name = fbWidth)]
    pub fn fb_width(&self) -> u32 {
        self.renderer.try_borrow().map_or(0, |r| r.fb_width())
    }

    #[wasm_bindgen(getter, js_name = fbHeight)]
    pub fn fb_height(&self) -> u32 {
        self.renderer.try_borrow().map_or(0, |r| r.fb_height())
    }

    #[wasm_bindgen(getter, js_name = submitCount)]
    pub fn submit_count(&self) -> u32 {
        self.renderer.try_borrow().map_or(0, |r| r.submit_count())
    }

    #[wasm_bindgen(js_name = cdf53Coverage)]
    pub fn cdf53_coverage(&self) -> Result<JsValue, JsValue> {
        let Some(core) = self.core.get() else {
            return Ok(JsValue::UNDEFINED);
        };
        let core = core.try_borrow().map_err(|_| busy())?;
        crate::core::cdf53_coverage_js(&core)
    }

    #[wasm_bindgen(js_name = cdf53IncompleteTiles)]
    pub fn cdf53_incomplete_tiles(&self, limit: usize) -> Result<JsValue, JsValue> {
        let Some(core) = self.core.get() else {
            return Ok(JsValue::UNDEFINED);
        };
        let core = core.try_borrow().map_err(|_| busy())?;
        crate::core::cdf53_incomplete_js(&core, limit)
    }

    pub fn log(&self, msg: &str) {
        self.log.line(msg);
    }
}
