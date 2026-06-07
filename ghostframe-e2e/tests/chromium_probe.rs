//! Diagnostic probe: launches the same Chromium binary the e2e tests use
//! and queries its codec support via the MediaCapabilities and WebCodecs
//! APIs. Prints results to stderr.
//!
//! Runs the probe TWICE so the CI log captures both modes:
//!   1. headless=new (the simpler `--headless=new` Ozone backend)
//!   2. weston + xwayland + Vulkan (matches what the harness's
//!      `setup_e2e_webgpu*` configurations actually use — non-headless,
//!      with an X display backed by Weston + XWayland and Vulkan/WebGPU
//!      enabled). This is the configuration where the failing H.264 tests
//!      actually run, so this is the one whose WebCodecs answer matters.
//!
//! Intentionally NOT `#[ignore]`'d: the e2e workflow runs this as its own
//! cargo invocation before the main e2e suite so the CI log records what
//! codecs the runner's Chromium actually supports in each mode. Run
//! locally with: `cargo test --test chromium_probe -- --nocapture`.

use anyhow::Result;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use ghostframe_e2e::harness::{spawn_weston_headless, start_static_server};
use std::time::Duration;

const PROBE_HTML: &str = r##"
<!doctype html>
<html><head><title>chromium-codec-probe</title></head>
<body>
<script>
(async () => {
  const probes = [
    {label: "H264 baseline 720p", codecs: 'video/mp4; codecs="avc1.42E01E"'},
    {label: "H264 main 720p",     codecs: 'video/mp4; codecs="avc1.4D401E"'},
    {label: "H264 high 720p",     codecs: 'video/mp4; codecs="avc1.640028"'},
    {label: "VP8",                codecs: 'video/webm; codecs="vp8"'},
    {label: "VP9",                codecs: 'video/webm; codecs="vp09.00.10.08"'},
  ];
  const out = [];
  for (const p of probes) {
    try {
      const r = await navigator.mediaCapabilities.decodingInfo({
        type: "file",
        video: {contentType: p.codecs, width: 1280, height: 720,
                bitrate: 1000000, framerate: 30}
      });
      out.push(`MediaCapabilities ${p.label}: supported=${r.supported} smooth=${r.smooth} pe=${r.powerEfficient}`);
    } catch (e) {
      out.push(`MediaCapabilities ${p.label}: ERROR ${e}`);
    }
  }
  if (typeof VideoDecoder !== "undefined") {
    for (const codec of ["avc1.42E01E", "avc1.4D401E", "avc1.640028", "vp8", "vp09.00.10.08"]) {
      try {
        const r = await VideoDecoder.isConfigSupported({codec});
        out.push(`WebCodecs ${codec}: supported=${r.supported}`);
      } catch (e) {
        out.push(`WebCodecs ${codec}: ERROR ${e}`);
      }
    }
  } else {
    out.push("WebCodecs VideoDecoder: NOT EXPOSED on globalThis");
  }
  out.push(`userAgent: ${navigator.userAgent}`);
  document.body.dataset.probeResult = out.join("\n");
  document.title = "DONE";
})();
</script>
</body></html>
"##;

async fn run_probe(label: &str, builder: BrowserConfig, page_url: &str) -> Result<()> {
    eprintln!();
    eprintln!("=== Chromium probe: {label} ===");
    eprintln!("  page_url: {page_url}");
    let (mut browser, mut handler) = Browser::launch(builder).await?;
    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page(page_url).await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let result: String = loop {
        if std::time::Instant::now() > deadline {
            let _ = browser.close().await;
            let _ = handle.await;
            anyhow::bail!("probe '{label}' timed out waiting for document.title=DONE");
        }
        let title: String = page
            .evaluate("document.title")
            .await?
            .into_value()
            .unwrap_or_default();
        if title == "DONE" {
            break page
                .evaluate("document.body.dataset.probeResult")
                .await?
                .into_value()
                .unwrap_or_default();
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    };
    for line in result.lines() {
        eprintln!("  {line}");
    }
    eprintln!("=== end probe: {label} ===");

    let _ = browser.close().await;
    let _ = handle.await;
    Ok(())
}

#[tokio::test]
async fn chromium_codec_probe() -> Result<()> {
    // Stand up a tiny static HTTP server serving the probe HTML. Loading
    // via http://127.0.0.1:PORT/index.html gives a real (non-opaque)
    // origin — WebCodecs is gated on a secure context, and data: URLs
    // have opaque origins that historically disable it.
    let tmpdir = std::env::temp_dir().join(format!(
        "ghostframe-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    ));
    std::fs::create_dir_all(&tmpdir)?;
    std::fs::write(tmpdir.join("index.html"), PROBE_HTML)?;
    let static_addr = start_static_server(&tmpdir).await?;
    let http_url = format!("http://{}/index.html", static_addr);
    let data_url = format!(
        "data:text/html;base64,{}",
        base64_encode(PROBE_HTML.as_bytes())
    );

    // Mode 1: --headless=new + data: URL (worst case — opaque origin, no display)
    {
        let mut builder = BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .arg("disable-gpu");
        builder = builder.new_headless_mode();
        if let Err(e) = run_probe(
            "headless=new + data: URL",
            builder.build().unwrap(),
            data_url.as_str(),
        )
        .await
        {
            eprintln!("  headless+data probe error: {e}");
        }
    }

    // Mode 2: --headless=new + http://localhost — same headless mode but a
    // real origin. Isolates "is the failure about origin?" from "is it
    // about the headless backend?".
    {
        let mut builder = BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .arg("disable-gpu");
        builder = builder.new_headless_mode();
        if let Err(e) = run_probe(
            "headless=new + http://localhost",
            builder.build().unwrap(),
            http_url.as_str(),
        )
        .await
        {
            eprintln!("  headless+http probe error: {e}");
        }
    }

    // Mode 3: harness mode (with_head + Weston + Vulkan/WebGPU) + http://localhost
    // This is what the failing H.264 tests actually see.
    {
        let weston = spawn_weston_headless()?;
        let builder = BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .with_head()
            .env("DISPLAY", weston.display.clone())
            .env(
                "XDG_RUNTIME_DIR",
                weston.runtime_dir().to_string_lossy().to_string(),
            )
            .arg(("enable-features", "Vulkan,WebGPU"))
            .arg("use-vulkan")
            .arg("ozone-platform=x11")
            .arg("enable-unsafe-webgpu")
            .arg("ignore-gpu-blocklist");
        if let Err(e) = run_probe(
            "harness mode (weston + http://localhost)",
            builder.build().unwrap(),
            http_url.as_str(),
        )
        .await
        {
            eprintln!("  weston+http probe error: {e}");
        }
        drop(weston);
    }

    // Mode 4: harness mode + explicit --enable-blink-features=WebCodecs in
    // case Chrome's stable default has WebCodecs gated on a runtime flag.
    {
        let weston = spawn_weston_headless()?;
        let builder = BrowserConfig::builder()
            .chrome_executable("/usr/bin/chromium")
            .no_sandbox()
            .with_head()
            .env("DISPLAY", weston.display.clone())
            .env(
                "XDG_RUNTIME_DIR",
                weston.runtime_dir().to_string_lossy().to_string(),
            )
            .arg(("enable-features", "Vulkan,WebGPU,WebCodecs"))
            .arg("enable-blink-features=WebCodecs")
            .arg("use-vulkan")
            .arg("ozone-platform=x11")
            .arg("enable-unsafe-webgpu")
            .arg("ignore-gpu-blocklist");
        if let Err(e) = run_probe(
            "harness mode + enable-blink-features=WebCodecs",
            builder.build().unwrap(),
            http_url.as_str(),
        )
        .await
        {
            eprintln!("  weston+http+webcodecs-flag probe error: {e}");
        }
        drop(weston);
    }

    Ok(())
}

fn base64_encode(bytes: &[u8]) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    let mut chunks = bytes.chunks_exact(3);
    for chunk in &mut chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
        out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
        out.push(CHARS[((n >> 6) & 0x3f) as usize] as char);
        out.push(CHARS[(n & 0x3f) as usize] as char);
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let n = match rem.len() {
            1 => u32::from(rem[0]) << 16,
            2 => (u32::from(rem[0]) << 16) | (u32::from(rem[1]) << 8),
            _ => unreachable!(),
        };
        out.push(CHARS[((n >> 18) & 0x3f) as usize] as char);
        out.push(CHARS[((n >> 12) & 0x3f) as usize] as char);
        if rem.len() == 2 {
            out.push(CHARS[((n >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        out.push('=');
    }
    out
}
