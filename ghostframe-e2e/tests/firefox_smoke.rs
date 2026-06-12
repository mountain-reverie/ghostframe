//! Smoke test for the FirefoxSession lifecycle.
//!
//! Verifies that we can launch Firefox via geckodriver, navigate to a data:
//! URL, evaluate JS, screenshot, and close cleanly. Skipped (via SKIP message)
//! when Firefox / geckodriver / certutil are missing on the host so dev boxes
//! without the Firefox path can still run the rest of the suite.

use ghostframe_e2e::harness::browser::{BrowserSession, FirefoxLaunch, FirefoxSession};

#[tokio::test(flavor = "multi_thread")]
async fn firefox_session_smoke() {
    for tool in ["firefox", "geckodriver", "certutil"] {
        if which::which(tool).is_err() {
            eprintln!("SKIP firefox_session_smoke: {tool} not on PATH");
            return;
        }
    }
    let cfg = FirefoxLaunch {
        display: std::env::var("DISPLAY").unwrap_or_default(),
        xdg_runtime_dir: std::env::var("XDG_RUNTIME_DIR").unwrap_or_default(),
        // Empty PEM exercises the smoke-test branch of install_cert (no-op).
        cert_pem: String::new(),
        firefox_bin: FirefoxLaunch::default_firefox_bin(),
    };
    let mut s = FirefoxSession::new(cfg).await.expect("FirefoxSession::new");
    s.new_page("data:text/html,<html><body><h1>hi</h1></body></html>")
        .await
        .expect("new_page");
    let h1: String = s
        .evaluate("document.querySelector('h1').textContent")
        .await
        .expect("evaluate");
    assert_eq!(h1, "hi");
    let png = s.screenshot().await.expect("screenshot");
    assert!(png.len() > 100, "screenshot returned suspiciously few bytes");
    s.close().await.expect("close");
}
