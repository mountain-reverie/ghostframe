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

// ---------------------------------------------------------------------------
// Chromium (CDP via chromiumoxide)
// ---------------------------------------------------------------------------

use anyhow::{anyhow, Context};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::ScreenshotParams;
use futures::StreamExt;

/// Launch parameters for Chromium. Mirrors the args the existing
/// `scene.rs` uses; lifted here so the trait impl owns them.
pub struct ChromiumLaunch {
    pub display: String,
    pub xdg_runtime_dir: String,
    pub spki_b64: String,
    pub user_data_dir: std::path::PathBuf,
}

pub struct ChromiumSession {
    browser: Browser,
    /// Consumes CDP event stream; aborted on close.
    _handler: tokio::task::JoinHandle<()>,
    page: Option<chromiumoxide::Page>,
}

impl ChromiumSession {
    pub async fn new(cfg: ChromiumLaunch) -> Result<Self> {
        let builder = BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .user_data_dir(&cfg.user_data_dir)
            .with_head()
            .env("DISPLAY", cfg.display)
            .env("XDG_RUNTIME_DIR", cfg.xdg_runtime_dir)
            .arg(("enable-features", "Vulkan,WebGPU"))
            .arg("use-vulkan")
            .arg("ozone-platform=x11")
            .arg("enable-unsafe-webgpu")
            .arg("ignore-gpu-blocklist")
            .arg((
                "ignore-certificate-errors-spki-list",
                cfg.spki_b64.as_str(),
            ));
        let (browser, mut handler) = Browser::launch(builder.build().map_err(|e| anyhow!(e))?)
            .await
            .context("chromiumoxide Browser::launch failed")?;
        let handler = tokio::spawn(async move { while handler.next().await.is_some() {} });
        Ok(Self {
            browser,
            _handler: handler,
            page: None,
        })
    }

    /// Escape hatch used by the transitional `E2eSetup` shim while older
    /// tests still poke `setup.page().evaluate(...)` directly. Removed once
    /// every test is converted to the trait (Task 9 onward).
    pub fn page(&self) -> Option<&chromiumoxide::Page> {
        self.page.as_ref()
    }
}

#[async_trait]
impl BrowserSession for ChromiumSession {
    async fn new_page(&mut self, url: &str) -> Result<()> {
        let page = self.browser.new_page(url).await.context("new_page")?;
        self.page = Some(page);
        Ok(())
    }

    async fn evaluate<T: DeserializeOwned + Send + 'static>(
        &mut self,
        script: &str,
    ) -> Result<T> {
        let page = self.page.as_ref().ok_or_else(|| anyhow!("no active page"))?;
        let v = page.evaluate(script).await.context("evaluate")?;
        v.into_value::<T>().context("deserialize evaluate result")
    }

    async fn screenshot(&mut self) -> Result<Vec<u8>> {
        let page = self.page.as_ref().ok_or_else(|| anyhow!("no active page"))?;
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .build();
        page.screenshot(params).await.context("screenshot")
    }

    async fn close(mut self) -> Result<()> {
        if let Some(page) = self.page.take() {
            let _ = page.close().await;
        }
        let _ = self.browser.close().await;
        Ok(())
    }
}
