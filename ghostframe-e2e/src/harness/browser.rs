//! Browser-driver abstraction for the e2e harness.
//!
//! The two impls (`ChromiumSession` + `FirefoxSession`) wrap different
//! underlying protocols (CDP and WebDriver respectively), but the test
//! body never sees the difference. See
//! `docs/superpowers/specs/2026-06-12-firefox-e2e-design.md`.

use anyhow::Result;
use async_trait::async_trait;
use serde::de::DeserializeOwned;

/// What every e2e test needs from a browser session: navigate, run JS,
/// screenshot, tear down.
///
/// Implementors are responsible for their own lifecycle (browser launch
/// in `new(...)`, cleanup in `close(...)` and `Drop`).
#[async_trait]
pub trait BrowserSession: Send {
    /// Navigate the active page to `url`. Subsequent `evaluate` /
    /// `screenshot` calls operate on this page.
    async fn new_page(&mut self, url: &str) -> Result<()>;

    /// Run `script` as JS, deserialize the return value as JSON into
    /// `T`. Use `T = serde_json::Value` for ad-hoc shapes; `T = ()` for
    /// fire-and-forget. The script should be a single expression or an
    /// IIFE that returns a JSON-serialisable value.
    async fn evaluate<T: DeserializeOwned + Send + 'static>(
        &mut self,
        script: &str,
    ) -> Result<T>;

    /// PNG bytes of the current page's viewport. Both backends return
    /// PNG (not JPEG) so SSIM thresholds in `pixels.rs` stay comparable.
    async fn screenshot(&mut self) -> Result<Vec<u8>>;

    /// Orderly shutdown: close the browser, wait for the driver to
    /// exit. Best-effort cleanup also runs in `Drop`.
    async fn close(self) -> Result<()>;
}
