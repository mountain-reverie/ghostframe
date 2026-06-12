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

/// Display backend the browser attaches to.
pub enum ChromiumDisplayMode {
    /// WebGPU mode: real X11 display via Weston/XWayland (Vulkan + WebGPU).
    Headed {
        display: String,
        xdg_runtime_dir: String,
        enable_webgpu: bool,
    },
    /// Headless mode (chromiumoxide's `new_headless_mode()`). No WebGPU.
    HeadlessNew,
}

/// Launch parameters for Chromium. Mirrors the args the existing
/// `scene.rs` uses; lifted here so the trait impl owns them.
pub struct ChromiumLaunch {
    pub mode: ChromiumDisplayMode,
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
        let base = BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .user_data_dir(&cfg.user_data_dir);

        let builder = match cfg.mode {
            ChromiumDisplayMode::Headed {
                display,
                xdg_runtime_dir,
                enable_webgpu,
            } => {
                let mut b = base
                    .with_head()
                    .env("DISPLAY", display)
                    .env("XDG_RUNTIME_DIR", xdg_runtime_dir);
                if enable_webgpu {
                    // With XWayland's DRI3 backing, Mesa Vulkan presentation works.
                    // The Wayland ozone platform is incompatible with `--use-vulkan`
                    // in Chromium 147+, so attach via `--ozone-platform=x11` instead.
                    b = b
                        .arg(("enable-features", "Vulkan,WebGPU"))
                        .arg("use-vulkan")
                        .arg("ozone-platform=x11")
                        .arg("enable-unsafe-webgpu")
                        .arg("ignore-gpu-blocklist");
                }
                b.arg((
                    "ignore-certificate-errors-spki-list",
                    cfg.spki_b64.as_str(),
                ))
            }
            ChromiumDisplayMode::HeadlessNew => base
                .new_headless_mode()
                .arg((
                    "ignore-certificate-errors-spki-list",
                    cfg.spki_b64.as_str(),
                )),
        };

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

// ---------------------------------------------------------------------------
// Firefox (WebDriver via fantoccini + geckodriver)
// ---------------------------------------------------------------------------

use std::path::PathBuf;

/// Launch parameters for Firefox. SPKI isn't used (certutil installs the
/// cert into the profile NSS DB instead).
pub struct FirefoxLaunch {
    pub display: String,
    pub xdg_runtime_dir: String,
    pub cert_pem: String,
    pub firefox_bin: PathBuf,
}

impl FirefoxLaunch {
    /// Pick the Firefox binary the e2e harness should run. Env-var override
    /// `GHOSTFRAME_E2E_FIREFOX_BIN` wins; otherwise `/usr/bin/firefox`.
    pub fn default_firefox_bin() -> PathBuf {
        std::env::var("GHOSTFRAME_E2E_FIREFOX_BIN")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("/usr/bin/firefox"))
    }
}

pub struct FirefoxSession {
    // Geckodriver + fantoccini client added in Tasks 7-8. For now only the
    // profile dir lives in the struct so it's drop-cleaned at end of test.
    _profile_dir: tempfile::TempDir,
}

impl FirefoxSession {
    /// Build a fresh profile directory + user.js with the prefs that the
    /// e2e harness needs (WebGPU on, HTTPS-only off). Returns the owned
    /// TempDir so the caller can pass its path to certutil and geckodriver.
    pub(crate) fn build_profile() -> Result<tempfile::TempDir> {
        let dir = tempfile::Builder::new()
            .prefix("ghostframe-fx-")
            .tempdir()
            .context("create Firefox profile tempdir")?;
        // Note: dom.security.https_only_mode = false because the harness
        // serves a static cert via tsnet's normally-LE-backed path; HTTPS-only
        // mode would warn-block before we get a chance to navigate.
        // marionette.port = 0 lets geckodriver pick its own (we only care
        // about the WebDriver port which we pass explicitly).
        let user_js = r#"user_pref("dom.webgpu.enabled", true);
user_pref("network.webtransport.enabled", true);
user_pref("dom.security.https_only_mode", false);
user_pref("marionette.port", 0);
"#;
        std::fs::write(dir.path().join("user.js"), user_js)
            .context("write user.js")?;
        Ok(dir)
    }

    /// Run `certutil -A` to import `cert_pem` into the profile's NSS DB so
    /// Firefox trusts the static e2e cert.
    ///
    /// Hard-fails if `certutil` isn't on PATH — silently skipped tests are
    /// the failure mode this whole iteration cycle just experienced.
    /// Empty `cert_pem` is treated as a no-op (smoke-test path, Task 8).
    pub(crate) fn install_cert(
        profile_dir: &std::path::Path,
        cert_pem: &str,
    ) -> Result<()> {
        if cert_pem.is_empty() {
            return Ok(());
        }
        if which::which("certutil").is_err() {
            return Err(anyhow!(
                "FirefoxSession::new: certutil(1) is required for the Firefox e2e path — \
                 install nss-tools (Debian) / nss (Arch) / nss-tools (Fedora) and retry, \
                 or skip the Firefox tests with --skip _firefox"
            ));
        }
        let cert_path = profile_dir.join("import.pem");
        std::fs::write(&cert_path, cert_pem).context("write import.pem")?;
        let out = std::process::Command::new("certutil")
            .arg("-A")
            .arg("-n").arg("ghostframe-e2e")
            .arg("-t").arg("C,,")
            .arg("-i").arg(&cert_path)
            .arg("-d").arg(format!("sql:{}", profile_dir.display()))
            .output()
            .context("spawn certutil")?;
        if !out.status.success() {
            return Err(anyhow!(
                "certutil -A failed: stdout={} stderr={}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr),
            ));
        }
        // Remove the imported PEM so it's not lying around in the profile dir.
        std::fs::remove_file(&cert_path).ok();
        Ok(())
    }

    /// Bind a localhost listener on port 0, read the assigned port, drop
    /// the listener. Race window between drop and geckodriver bind is
    /// acceptable for e2e — kernel won't recycle in that window under
    /// normal load.
    pub(crate) fn pick_free_port() -> Result<u16> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .context("bind 127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    }

    /// Spawn `geckodriver --port <port> --binary <firefox_bin>` as a tokio
    /// child process. stdout/stderr are inherited so the geckodriver log
    /// appears in test output under `cargo test --nocapture`. The child has
    /// `kill_on_drop` set so unexpected panics don't leak the driver.
    ///
    /// Hard-fails if `geckodriver` isn't on PATH — silently skipped tests
    /// are the failure mode this whole iteration cycle just experienced.
    pub(crate) fn spawn_geckodriver(
        port: u16,
        firefox_bin: &std::path::Path,
        display: &str,
        xdg_runtime_dir: &str,
    ) -> Result<tokio::process::Child> {
        if which::which("geckodriver").is_err() {
            return Err(anyhow!(
                "FirefoxSession::new: geckodriver is required for the Firefox e2e path — \
                 install from https://github.com/mozilla/geckodriver/releases and retry, \
                 or skip the Firefox tests with --skip _firefox"
            ));
        }
        let child = tokio::process::Command::new("geckodriver")
            .arg("--port").arg(port.to_string())
            .arg("--binary").arg(firefox_bin)
            .env("DISPLAY", display)
            .env("XDG_RUNTIME_DIR", xdg_runtime_dir)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn geckodriver")?;
        Ok(child)
    }

    /// Poll `http://127.0.0.1:<port>/status` until geckodriver reports
    /// `value.ready: true`, or `timeout` elapses. WebDriver's `/status`
    /// is a GET that returns `{ value: { ready: bool, message: str } }`.
    pub(crate) async fn wait_for_geckodriver(
        port: u16,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let deadline = tokio::time::Instant::now() + timeout;
        let url = format!("http://127.0.0.1:{port}/status");
        loop {
            if let Ok(resp) = reqwest::get(&url).await {
                if let Ok(v) = resp.json::<serde_json::Value>().await {
                    if v.pointer("/value/ready").and_then(|x| x.as_bool()) == Some(true) {
                        return Ok(());
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(anyhow!(
                    "geckodriver did not become ready within {:?} (port {port})",
                    timeout
                ));
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
}
