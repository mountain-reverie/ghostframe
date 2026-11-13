//! The browser client's DOM UI. Leptos lives here and nowhere else in the
//! workspace: swapping the framework means rewriting this crate.
//!
//! The video path (WebTransport, WebCodecs, WebGPU, input) stays
//! framework-free. This crate draws the stats panel beside it and talks to
//! the core only through [`PanelBackend`].

use ghostframe_client_core::StatsView;

pub mod backend;
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
