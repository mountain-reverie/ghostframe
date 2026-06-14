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
/// Kind of pointer event to dispatch via `dispatch_pointer_event`.
#[derive(Debug, Clone, Copy)]
pub enum PointerEventKind {
    Move,
    Down,
    Up,
    Leave,
}

impl PointerEventKind {
    fn as_dom_type(self) -> &'static str {
        match self {
            PointerEventKind::Move => "pointermove",
            PointerEventKind::Down => "pointerdown",
            PointerEventKind::Up => "pointerup",
            PointerEventKind::Leave => "pointerleave",
        }
    }
}

/// Kind of keyboard event to dispatch via `dispatch_keyboard_event`.
#[derive(Debug, Clone, Copy)]
pub enum KeyEventKind {
    Down,
    Up,
}

impl KeyEventKind {
    fn as_dom_type(self) -> &'static str {
        match self {
            KeyEventKind::Down => "keydown",
            KeyEventKind::Up => "keyup",
        }
    }
}

#[async_trait]
pub trait BrowserSession: Send {
    /// Navigate the active page to `url`. Subsequent `evaluate` /
    /// `screenshot` calls operate on this page.
    async fn new_page(&mut self, url: &str) -> Result<()>;

    /// Run `script` as JS, deserialize the return value as JSON into
    /// `T`. Use `T = serde_json::Value` for ad-hoc shapes; `T = ()` for
    /// fire-and-forget. The script should be a single expression or an
    /// IIFE that returns a JSON-serialisable value.
    async fn evaluate<T: DeserializeOwned + Send + 'static>(&mut self, script: &str) -> Result<T>;

    /// PNG bytes of the current page's viewport. Both backends return
    /// PNG (not JPEG) so SSIM thresholds in `pixels.rs` stay comparable.
    async fn screenshot(&mut self) -> Result<Vec<u8>>;

    /// Orderly shutdown: close the browser, wait for the driver to
    /// exit. Best-effort cleanup also runs in `Drop`.
    async fn close(self) -> Result<()>;

    /// Dispatch a synthetic `PointerEvent` on the element matched by
    /// `selector` at element-local coords `(x, y)`. `button` is the DOM
    /// button index (0=left, 1=middle, 2=right). Ignored for `Move` /
    /// `Leave`.
    ///
    /// Synthetic events have `isTrusted=false`. The web-client's
    /// `attachInputCapture` listeners do not gate on `isTrusted`, so
    /// they fire and round-trip the input through our wire encoder
    /// exactly as a real user click would.
    async fn dispatch_pointer_event(
        &mut self,
        selector: &str,
        kind: PointerEventKind,
        x: i32,
        y: i32,
        button: u8,
    ) -> Result<()> {
        let script = build_pointer_dispatch_js(selector, kind, x, y, button);
        let ok: bool = self.evaluate(&script).await?;
        if !ok {
            return Err(anyhow::anyhow!(
                "dispatch_pointer_event: element {selector:?} not found"
            ));
        }
        Ok(())
    }

    /// Dispatch a synthetic `WheelEvent` on the element matched by
    /// `selector` at element-local coords `(x, y)`. `dx`/`dy` go on
    /// `deltaX`/`deltaY` directly.
    async fn dispatch_wheel_event(
        &mut self,
        selector: &str,
        x: i32,
        y: i32,
        dx: i32,
        dy: i32,
    ) -> Result<()> {
        let script = build_wheel_dispatch_js(selector, x, y, dx, dy);
        let ok: bool = self.evaluate(&script).await?;
        if !ok {
            return Err(anyhow::anyhow!(
                "dispatch_wheel_event: element {selector:?} not found"
            ));
        }
        Ok(())
    }

    /// Dispatch a synthetic `KeyboardEvent` on the element matched by
    /// `selector`. `key` is the DOM `KeyboardEvent.key` string
    /// ("Enter", "ArrowUp", "a", etc.) — what `keymap.ts` parses.
    async fn dispatch_keyboard_event(
        &mut self,
        selector: &str,
        kind: KeyEventKind,
        key: &str,
    ) -> Result<()> {
        let script = build_keyboard_dispatch_js(selector, kind, key);
        let ok: bool = self.evaluate(&script).await?;
        if !ok {
            return Err(anyhow::anyhow!(
                "dispatch_keyboard_event: element {selector:?} not found"
            ));
        }
        Ok(())
    }
}

/// Encode `s` as a JS string literal — safe to interpolate inline.
fn js_string(s: &str) -> String {
    serde_json::to_string(s).expect("string is always JSON-serializable")
}

fn build_pointer_dispatch_js(
    selector: &str,
    kind: PointerEventKind,
    x: i32,
    y: i32,
    button: u8,
) -> String {
    let dom_type = kind.as_dom_type();
    // `buttons` is the bitmask of buttons CURRENTLY held. For pointerdown
    // we set the bit for `button`; for everything else we leave it 0.
    let buttons: u32 = match kind {
        PointerEventKind::Down => 1u32 << button,
        _ => 0,
    };
    format!(
        "(() => {{\n  const el = document.querySelector({sel});\n  if (!el) return false;\n  const rect = el.getBoundingClientRect();\n  el.dispatchEvent(new PointerEvent({ty}, {{\n    clientX: rect.left + {x},\n    clientY: rect.top + {y},\n    button: {button},\n    buttons: {buttons},\n    isPrimary: true,\n    bubbles: true,\n  }}));\n  return true;\n}})()",
        sel = js_string(selector),
        ty = js_string(dom_type),
        x = x,
        y = y,
        button = button,
        buttons = buttons,
    )
}

fn build_wheel_dispatch_js(selector: &str, x: i32, y: i32, dx: i32, dy: i32) -> String {
    format!(
        "(() => {{\n  const el = document.querySelector({sel});\n  if (!el) return false;\n  const rect = el.getBoundingClientRect();\n  el.dispatchEvent(new WheelEvent('wheel', {{\n    clientX: rect.left + {x},\n    clientY: rect.top + {y},\n    deltaX: {dx},\n    deltaY: {dy},\n    bubbles: true,\n  }}));\n  return true;\n}})()",
        sel = js_string(selector),
        x = x,
        y = y,
        dx = dx,
        dy = dy,
    )
}

fn build_keyboard_dispatch_js(selector: &str, kind: KeyEventKind, key: &str) -> String {
    let dom_type = kind.as_dom_type();
    format!(
        "(() => {{\n  const el = document.querySelector({sel});\n  if (!el) return false;\n  el.dispatchEvent(new KeyboardEvent({ty}, {{\n    key: {key},\n    bubbles: true,\n  }}));\n  return true;\n}})()",
        sel = js_string(selector),
        ty = js_string(dom_type),
        key = js_string(key),
    )
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
                b.arg(("ignore-certificate-errors-spki-list", cfg.spki_b64.as_str()))
            }
            ChromiumDisplayMode::HeadlessNew => base
                .new_headless_mode()
                .arg(("ignore-certificate-errors-spki-list", cfg.spki_b64.as_str())),
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
    /// Open a fresh CDP tab and replace `self.page`. NOTE: this is a
    /// subtle semantic asymmetry with `FirefoxSession::new_page` (which
    /// navigates the *current* tab via WebDriver `goto`). Both reach the
    /// requested URL, but CDP `new_page` allocates a new tab while
    /// WebDriver `goto` reuses the existing one. Currently affects only
    /// `e2e_palrle_session_reset`, whose body uses `new_page(page_url)`
    /// in place of CDP-specific `Page.reload()` — both reach the same
    /// `on_session_reset` server path, so the assertion holds on both.
    /// If a future test depends on `Page.reload()`'s specific
    /// cache-invalidation semantics, add a `reload()` method to the
    /// trait rather than relying on this asymmetric `new_page`.
    async fn new_page(&mut self, url: &str) -> Result<()> {
        let page = self.browser.new_page(url).await.context("new_page")?;
        self.page = Some(page);
        Ok(())
    }

    async fn evaluate<T: DeserializeOwned + Send + 'static>(&mut self, script: &str) -> Result<T> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("no active page"))?;
        let v = page.evaluate(script).await.context("evaluate")?;
        v.into_value::<T>().context("deserialize evaluate result")
    }

    async fn screenshot(&mut self) -> Result<Vec<u8>> {
        let page = self
            .page
            .as_ref()
            .ok_or_else(|| anyhow!("no active page"))?;
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
    _profile_dir: tempfile::TempDir,
    _geckodriver: tokio::process::Child,
    client: fantoccini::Client,
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
        std::fs::write(dir.path().join("user.js"), user_js).context("write user.js")?;
        Ok(dir)
    }

    /// Run `certutil -A` to import `cert_pem` into the profile's NSS DB so
    /// Firefox trusts the static e2e cert.
    ///
    /// Hard-fails if `certutil` isn't on PATH — silently skipped tests are
    /// the failure mode this whole iteration cycle just experienced.
    /// Empty `cert_pem` is treated as a no-op (smoke-test path, Task 8).
    pub(crate) fn install_cert(profile_dir: &std::path::Path, cert_pem: &str) -> Result<()> {
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
            .arg("-n")
            .arg("ghostframe-e2e")
            .arg("-t")
            .arg("C,,")
            .arg("-i")
            .arg(&cert_path)
            .arg("-d")
            .arg(format!("sql:{}", profile_dir.display()))
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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").context("bind 127.0.0.1:0")?;
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
            .arg("--port")
            .arg(port.to_string())
            .arg("--binary")
            .arg(firefox_bin)
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

    /// Install the rustls default crypto provider exactly once per process.
    /// fantoccini's `rustls-tls` feature pulls `hyper-rustls`, which is built
    /// against `rustls 0.23` — that version requires an explicit
    /// `CryptoProvider` to be registered before any TLS handshake. Without
    /// this the first FirefoxSession::new panics with
    /// "Could not automatically determine the process-level CryptoProvider".
    fn install_rustls_provider_once() {
        use std::sync::Once;
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            // Ignore the Err — if a provider was already installed (e.g. by
            // a peer crate in the workspace) we just keep that one.
            let _ = rustls::crypto::ring::default_provider().install_default();
        });
    }

    /// Construct a `FirefoxSession`: build a profile, install the TLS cert,
    /// pick a free port, spawn geckodriver, wait for it to be ready, then
    /// connect fantoccini.
    pub async fn new(cfg: FirefoxLaunch) -> Result<Self> {
        Self::install_rustls_provider_once();
        let profile_dir = Self::build_profile()?;
        Self::install_cert(profile_dir.path(), &cfg.cert_pem)?;
        let port = Self::pick_free_port()?;
        let mut child =
            Self::spawn_geckodriver(port, &cfg.firefox_bin, &cfg.display, &cfg.xdg_runtime_dir)?;
        if let Err(e) = Self::wait_for_geckodriver(port, std::time::Duration::from_secs(10)).await {
            // best-effort kill so a half-up geckodriver doesn't leak past the failure
            let _ = child.kill().await;
            return Err(e);
        }
        let caps = serde_json::json!({
            "moz:firefoxOptions": {
                "binary": cfg.firefox_bin.to_string_lossy(),
                "args": ["-profile", profile_dir.path().to_string_lossy()],
            },
            "acceptInsecureCerts": true,
        });
        let cap_map: serde_json::Map<String, serde_json::Value> = caps
            .as_object()
            .ok_or_else(|| anyhow!("FirefoxSession::new: capabilities JSON is not an object"))?
            .clone();
        // rustls() (not native()) — fantoccini's native-tls feature pulls
        // openssl-sys into the workspace and breaks the test-server Docker
        // build whose base image has no libssl-dev. The rustls builder is
        // gated on the rustls-tls feature in this crate's Cargo.toml.
        let client = fantoccini::ClientBuilder::rustls()
            .context("fantoccini rustls connector init")?
            .capabilities(cap_map)
            .connect(&format!("http://127.0.0.1:{port}"))
            .await
            .context("fantoccini connect to geckodriver")?;

        // Bump the script timeout from marionette's 30 s default. The
        // pixel-scan tests (e2e_solid_color_body, e2e_edge_tiles_body) loop
        // `await window.__readPixel(x, y)` 300+ times, and each
        // GPU-staging-buffer mapAsync on Firefox Nightly's software WebGPU
        // takes ~100 ms in CI — easily exceeding 30 s. When marionette
        // times out, geckodriver surfaces it as "Failed to decode response
        // from marionette" rather than a clean timeout error, which is
        // both opaque and easy to misdiagnose.
        let _ = client
            .update_timeouts(fantoccini::wd::TimeoutConfiguration::new(
                Some(std::time::Duration::from_secs(120)), // script
                Some(std::time::Duration::from_secs(60)),  // page_load
                Some(std::time::Duration::from_secs(0)),   // implicit
            ))
            .await;

        Ok(Self {
            _profile_dir: profile_dir,
            _geckodriver: child,
            client,
        })
    }
}

#[async_trait]
impl BrowserSession for FirefoxSession {
    async fn new_page(&mut self, url: &str) -> Result<()> {
        self.client.goto(url).await.context("fantoccini goto")?;
        Ok(())
    }

    async fn evaluate<T: DeserializeOwned + Send + 'static>(&mut self, script: &str) -> Result<T> {
        // Use Execute Async Script (W3C WebDriver §17.4) — `executeScript`
        // does NOT await returned Promises before serializing (unlike CDP's
        // Page.evaluate), so wrapping a returning-Promise IIFE in a sync
        // executeScript yields a `{}` Promise object that marionette can't
        // decode ("Failed to decode response from marionette"). Every
        // ghostframe e2e script is either an IIFE returning a value, an
        // async IIFE returning a Promise, or a synchronous expression.
        // `Promise.resolve(...).then(cb, err)` covers all three: the sync
        // case wraps the value in a resolved Promise and the async case
        // chains the existing Promise. Errors get sent back with an
        // `__err` marker so deserialization sees a structured failure
        // instead of timing out.
        let wrapped = format!(
            "const __cb = arguments[arguments.length - 1];\n\
             (async () => {{\n\
                 try {{ return await ({script}); }}\n\
                 catch (e) {{ return {{ __err: String(e && e.stack || e) }}; }}\n\
             }})().then(__cb);"
        );
        let v = self
            .client
            .execute_async(&wrapped, vec![])
            .await
            .context("fantoccini execute_async")?;
        if let Some(obj) = v.as_object() {
            if let Some(err) = obj.get("__err").and_then(|x| x.as_str()) {
                return Err(anyhow!("script threw: {err}"));
            }
        }
        serde_json::from_value(v).context("deserialize execute_async result")
    }

    async fn screenshot(&mut self) -> Result<Vec<u8>> {
        self.client
            .screenshot()
            .await
            .context("fantoccini screenshot")
    }

    async fn close(self) -> Result<()> {
        let _ = self.client.close().await;
        // _geckodriver has kill_on_drop so Drop tidies the child process.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Input dispatch smoke runner (shared between firefox_smoke and chromium_smoke)
// ---------------------------------------------------------------------------

/// Self-contained smoke that verifies the four input-dispatch trait
/// methods land synthetic DOM events on a target element with the
/// expected fields. The page records every received event into
/// `window.__events`; we read it back via `evaluate` and assert.
///
/// Backend-agnostic — both `ChromiumSession` and `FirefoxSession` go
/// through the same default-impl path that wraps `evaluate(...)`.
pub async fn run_input_dispatch_smoke<S: BrowserSession>(session: &mut S) -> Result<()> {
    // 400×300 canvas, listeners on every event type we dispatch. The
    // event log holds `{type, x, y, button, deltaX, deltaY, key}` per
    // event — fields irrelevant to a given type are 0/empty.
    let page_html = r##"<!doctype html>
<html><head><meta charset="utf-8"></head><body style="margin:0">
<canvas id="c" tabindex="0" width="400" height="300" style="background:#000"></canvas>
<script>
window.__events = [];
const c = document.getElementById('c');
const r0 = c.getBoundingClientRect();
for (const t of ['pointermove','pointerdown','pointerup','pointerleave','wheel','keydown','keyup']) {
  c.addEventListener(t, (e) => {
    const r = c.getBoundingClientRect();
    window.__events.push({
      type: e.type,
      x: Math.round((e.clientX || 0) - r.left),
      y: Math.round((e.clientY || 0) - r.top),
      button: e.button == null ? -1 : e.button,
      deltaX: e.deltaX || 0,
      deltaY: e.deltaY || 0,
      key: e.key || '',
    });
  });
}
</script></body></html>"##;
    let data_url = format!(
        "data:text/html;base64,{}",
        BASE64_STANDARD.encode(page_html.as_bytes())
    );
    session.new_page(&data_url).await.context("new_page")?;

    session
        .dispatch_pointer_event("#c", PointerEventKind::Move, 100, 50, 0)
        .await
        .context("dispatch pointermove")?;
    session
        .dispatch_pointer_event("#c", PointerEventKind::Down, 100, 50, 0)
        .await
        .context("dispatch pointerdown")?;
    session
        .dispatch_pointer_event("#c", PointerEventKind::Up, 100, 50, 0)
        .await
        .context("dispatch pointerup")?;
    session
        .dispatch_wheel_event("#c", 200, 100, 0, 1)
        .await
        .context("dispatch wheel")?;
    session
        .dispatch_keyboard_event("#c", KeyEventKind::Down, "Enter")
        .await
        .context("dispatch keydown Enter")?;
    session
        .dispatch_keyboard_event("#c", KeyEventKind::Up, "Enter")
        .await
        .context("dispatch keyup Enter")?;
    session
        .dispatch_pointer_event("#c", PointerEventKind::Leave, 0, 0, 0)
        .await
        .context("dispatch pointerleave")?;

    // Read the log back as JSON. Using `JSON.stringify` keeps the
    // payload a single string — sidesteps backend differences in how
    // arrays-of-objects round-trip through the WebDriver/CDP layers.
    let log_json: String = session
        .evaluate("JSON.stringify(window.__events)")
        .await
        .context("read window.__events")?;
    let events: Vec<serde_json::Value> =
        serde_json::from_str(&log_json).context("parse __events JSON")?;

    if events.len() != 7 {
        return Err(anyhow::anyhow!(
            "expected 7 events, got {}: {}",
            events.len(),
            log_json
        ));
    }

    let check = |i: usize, ty: &str, want_x: i64, want_y: i64| -> Result<()> {
        let e = &events[i];
        let got_ty = e.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let got_x = e.get("x").and_then(|v| v.as_i64()).unwrap_or(-1);
        let got_y = e.get("y").and_then(|v| v.as_i64()).unwrap_or(-1);
        if got_ty != ty || got_x != want_x || got_y != want_y {
            return Err(anyhow::anyhow!(
                "event[{i}]: want type={ty} x={want_x} y={want_y}, got type={got_ty} x={got_x} y={got_y}"
            ));
        }
        Ok(())
    };
    check(0, "pointermove", 100, 50)?;
    check(1, "pointerdown", 100, 50)?;
    check(2, "pointerup", 100, 50)?;
    // Wheel deltas live on a different field; verify type + deltas.
    {
        let e = &events[3];
        if e.get("type").and_then(|v| v.as_str()) != Some("wheel") {
            return Err(anyhow::anyhow!("event[3] type: {e:?}"));
        }
        let dy = e.get("deltaY").and_then(|v| v.as_f64()).unwrap_or(0.0);
        if (dy - 1.0).abs() > 1e-6 {
            return Err(anyhow::anyhow!("event[3] deltaY: got {dy}"));
        }
    }
    // Keyboard: verify type + key string. (xy on synthetic KeyboardEvent
    // is implementation-defined, so we don't pin it.)
    {
        let e = &events[4];
        if e.get("type").and_then(|v| v.as_str()) != Some("keydown") {
            return Err(anyhow::anyhow!("event[4] type: {e:?}"));
        }
        if e.get("key").and_then(|v| v.as_str()) != Some("Enter") {
            return Err(anyhow::anyhow!("event[4] key: {e:?}"));
        }
    }
    {
        let e = &events[5];
        if e.get("type").and_then(|v| v.as_str()) != Some("keyup") {
            return Err(anyhow::anyhow!("event[5] type: {e:?}"));
        }
    }
    {
        let e = &events[6];
        if e.get("type").and_then(|v| v.as_str()) != Some("pointerleave") {
            return Err(anyhow::anyhow!("event[6] type: {e:?}"));
        }
    }

    Ok(())
}

// data:base64 encoding — pulled in via base64 crate since it's already
// in the workspace tree (chromium_probe.rs uses a hand-rolled version,
// but for new code prefer the crate).
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine as _;
