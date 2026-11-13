// SPDX-License-Identifier: MIT OR Apache-2.0
//! The panel's window onto the core.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use ghostframe_client_core::{ClientCore, StatsView};

use crate::PanelBackend;

/// Microseconds on the page's `performance.now()` clock: the clock `main.ts`
/// gives every `now_us` it passes the core, so staleness compares like with
/// like.
fn now_us() -> u64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map(|p| (p.now() * 1000.0) as u64)
        .unwrap_or(0)
}

/// A [`PanelBackend`] over the core the wasm module shares with the panel.
///
/// The core sits in an `Rc<RefCell<..>>` because both the JS-facing
/// `WasmClientCore` and the panel's timer and click handlers own it. Those
/// callbacks run as separate browser tasks, never inside a `WasmClientCore`
/// call (no export calls back into JS), so a borrow here cannot overlap one
/// there. The `try_` forms exist anyway: a failed borrow must cost one
/// refresh, not trap the module.
pub struct CoreBackend {
    core: Rc<RefCell<ClientCore>>,
    /// The last view that was read, served if a borrow fails.
    last: RefCell<StatsView>,
    /// Subscribe requests that could not be queued; see `set_subscribed`.
    dropped: Cell<u32>,
}

impl CoreBackend {
    pub fn new(core: Rc<RefCell<ClientCore>>) -> Self {
        CoreBackend {
            core,
            last: RefCell::new(StatsView::default()),
            dropped: Cell::new(0),
        }
    }

    /// Subscribe requests lost to a busy core. Zero in practice.
    pub fn dropped_requests(&self) -> u32 {
        self.dropped.get()
    }
}

impl PanelBackend for CoreBackend {
    fn view(&self) -> StatsView {
        // Read the clock first: `performance.now()` is a JS call, and the
        // borrow below must not span one.
        let now = now_us();
        if let Ok(core) = self.core.try_borrow() {
            let view = core.stats().view(now);
            *self.last.borrow_mut() = view.clone();
            return view;
        }
        self.last.borrow().clone()
    }

    fn set_subscribed(&self, on: bool) {
        match self.core.try_borrow_mut() {
            Ok(mut core) => core.set_stats_subscribed(on),
            Err(_) => {
                self.dropped.set(self.dropped.get().saturating_add(1));
                web_sys::console::warn_1(
                    &"stats panel: core busy, subscribe request dropped".into(),
                );
            }
        }
    }
}
