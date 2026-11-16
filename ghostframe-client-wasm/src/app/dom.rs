// SPDX-License-Identifier: MIT OR Apache-2.0
//! The page itself: the status line, the log, the disconnect overlay, the
//! lossless page chrome and the URL flags.

use wasm_bindgen::JsCast;
use web_sys::{Document, HtmlCanvasElement, HtmlElement};

use crate::app::page::PageFlags;

/// Lines the page log keeps.
pub const LOG_LINES: u32 = 50;

/// `#status` and `#log` of `index.html`.
pub struct PageLog {
    document: Document,
    status: HtmlElement,
    log: HtmlElement,
}

impl PageLog {
    pub fn find(document: &Document) -> Result<Self, String> {
        let get = |id: &str| -> Result<HtmlElement, String> {
            document
                .get_element_by_id(id)
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                .ok_or_else(|| format!("#{id} is missing from index.html"))
        };
        Ok(PageLog {
            document: document.clone(),
            status: get("status")?,
            log: get("log")?,
        })
    }

    pub fn status(&self, text: &str) {
        self.status.set_text_content(Some(text));
    }

    /// Append one line, keeping the last `LOG_LINES`.
    pub fn line(&self, text: &str) {
        if let Ok(div) = self.document.create_element("div") {
            div.set_text_content(Some(text));
            let _ = self.log.append_child(&div);
        }
        while self.log.child_element_count() > LOG_LINES {
            match self.log.first_child() {
                Some(first) => {
                    let _ = self.log.remove_child(&first);
                }
                None => break,
            }
        }
    }

    /// `?e2e=lossless`: hide the status and log (they still work; only the
    /// screenshot must not show them).
    pub fn hide(&self) {
        let _ = self.status.style().set_property("display", "none");
        let _ = self.log.style().set_property("display", "none");
    }
}

/// `?e2e=lossless`: pin the canvas top-left so a CDP screenshot holds only it.
pub fn pin_canvas(canvas: &HtmlCanvasElement) {
    let style = canvas.style();
    let _ = style.set_property("position", "fixed");
    let _ = style.set_property("top", "0");
    let _ = style.set_property("left", "0");
}

/// Terminal disconnect overlay. Deliberately no reconnect button: reloading
/// is the reconnect, and a button invites two clients evicting each other.
/// Shown once; a second call keeps the first message.
pub fn show_disconnected_overlay(document: &Document, message: &str) {
    if document.get_element_by_id("gf-disconnected").is_some() {
        return;
    }
    let Ok(el) = document.create_element("div") else {
        return;
    };
    el.set_id("gf-disconnected");
    let _ = el.set_attribute("role", "status");
    el.set_text_content(Some(message));
    let _ = el.set_attribute(
        "style",
        "position:fixed;inset:0;display:flex;align-items:center;justify-content:center;\
         background:rgba(0,0,0,0.82);color:#fff;font:16px system-ui,sans-serif;\
         z-index:9999;text-align:center;padding:2rem",
    );
    if let Some(body) = document.body() {
        let _ = body.append_child(&el);
    }
}

/// The flags of the page URL.
pub fn page_flags(window: &web_sys::Window) -> PageFlags {
    let search = window.location().search().unwrap_or_default();
    let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else {
        return PageFlags::from_params(&[], None);
    };
    let e2e: Vec<String> = params
        .get_all("e2e")
        .iter()
        .filter_map(|v| v.as_string())
        .collect();
    PageFlags::from_params(&e2e, params.get("cdf53").as_deref())
}
