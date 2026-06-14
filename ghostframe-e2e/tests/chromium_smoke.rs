//! Smoke test for the ChromiumSession lifecycle + input-dispatch trait
//! methods. Mirrors `firefox_smoke.rs` so each backend has its own
//! self-contained verification of the new `BrowserSession::dispatch_*`
//! API.

use ghostframe_e2e::harness::browser::{
    BrowserSession, ChromiumDisplayMode, ChromiumLaunch, ChromiumSession,
};
use ghostframe_e2e::harness::run_input_dispatch_smoke;

#[tokio::test(flavor = "multi_thread")]
async fn chromium_input_dispatch_smoke() {
    if which::which("chromium").is_err() && !std::path::Path::new("/usr/bin/chromium").exists() {
        eprintln!("SKIP chromium_input_dispatch_smoke: chromium not on PATH");
        return;
    }
    let user_data_dir = tempfile::Builder::new()
        .prefix("ghostframe-cr-smoke-")
        .tempdir()
        .expect("user_data_dir");
    let cfg = ChromiumLaunch {
        mode: ChromiumDisplayMode::HeadlessNew,
        spki_b64: String::new(),
        user_data_dir: user_data_dir.path().to_path_buf(),
    };
    let mut s = ChromiumSession::new(cfg)
        .await
        .expect("ChromiumSession::new");
    run_input_dispatch_smoke(&mut s)
        .await
        .expect("run_input_dispatch_smoke");
    s.close().await.expect("close");
}
