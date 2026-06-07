//! Diagnostic probe: launches the same Chromium binary the e2e tests use
//! and queries its codec support via the MediaCapabilities and WebCodecs
//! APIs. Prints results to stderr.
//!
//! Intentionally NOT `#[ignore]`'d: the e2e workflow runs this as its own
//! cargo invocation before the main e2e suite so the CI log records
//! what codecs the runner's Chromium actually supports. Run locally with:
//!     cargo test --test chromium_probe -- --nocapture

use anyhow::Result;
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use std::time::Duration;

#[tokio::test]
async fn chromium_codec_probe() -> Result<()> {
    let probe_html = r##"
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
      out.push(`${p.label}: supported=${r.supported} smooth=${r.smooth} pe=${r.powerEfficient}`);
    } catch (e) {
      out.push(`${p.label}: ERROR ${e}`);
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
    out.push("WebCodecs VideoDecoder: not exposed");
  }
  document.body.dataset.probeResult = out.join("\n");
  document.title = "DONE";
})();
</script>
</body></html>
"##;

    let mut builder = BrowserConfig::builder()
        .chrome_executable("/usr/bin/chromium")
        .no_sandbox()
        .arg("disable-gpu");
    builder = builder.new_headless_mode();
    let (mut browser, mut handler) = Browser::launch(builder.build().unwrap()).await?;

    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let data_url = format!(
        "data:text/html;base64,{}",
        base64_encode(probe_html.as_bytes())
    );

    let page = browser.new_page(data_url.as_str()).await?;

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let result: String = loop {
        if std::time::Instant::now() > deadline {
            anyhow::bail!("probe timed out waiting for document.title=DONE");
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

    eprintln!("=== Chromium codec probe results ===");
    for line in result.lines() {
        eprintln!("  {line}");
    }
    eprintln!("=== end probe results ===");

    let _ = browser.close().await;
    let _ = handle.await;
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
