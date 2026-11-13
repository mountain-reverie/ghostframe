//! Mounting the stats panel from JS.

use std::rc::Rc;

use ghostframe_web_ui::CoreBackend;
use wasm_bindgen::prelude::*;

use crate::core::WasmClientCore;

/// Draw the stats panel into `container`, wired to `core`. Call once, after
/// the core exists. An error means no panel; the session is unaffected, so
/// the caller logs it and carries on.
#[wasm_bindgen(js_name = mountStatsPanel)]
pub fn mount_stats_panel(
    container: web_sys::HtmlElement,
    core: &WasmClientCore,
) -> Result<(), JsValue> {
    let backend = Rc::new(CoreBackend::new(core.shared()));
    ghostframe_web_ui::mount_stats_panel(container, backend)
}
