// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client's DOM UI. Leptos lives here and nowhere else in the
//! workspace: swapping the framework means rewriting this crate.
//!
//! The video path (WebTransport, WebCodecs, WebGPU, input) stays
//! framework-free. This crate draws the stats panel beside it and talks to
//! the core only through [`PanelBackend`].

use std::rc::Rc;

use ghostframe_client_core::StatsView;
use leptos::prelude::*;
use wasm_bindgen::JsValue;

pub mod backend;
pub mod components;
pub mod format;
pub mod spark;
mod storage;

pub use backend::CoreBackend;

/// What the panel needs from the session, and all it may touch.
pub trait PanelBackend {
    /// Everything to draw, as of now.
    fn view(&self) -> StatsView;
    /// The panel opened (`true`) or closed (`false`): start or stop the
    /// server's samples.
    fn set_subscribed(&self, on: bool);
}

/// Mount the stats panel into `container`. The panel lives until the page
/// does.
///
/// An error here must never break the stream: the caller logs it and carries
/// on without a panel.
pub fn mount_stats_panel(
    container: web_sys::HtmlElement,
    backend: Rc<dyn PanelBackend>,
) -> Result<(), JsValue> {
    let handle = leptos::mount::mount_to(container, move || {
        view! { <components::StatsPanel backend=backend /> }
    });
    handle.forget();
    Ok(())
}
