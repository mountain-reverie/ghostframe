use anyhow::{anyhow, Context, Result};

/// Capture the page's canvas as a PNG (via chromiumoxide's `screenshot`
/// helper, which wraps CDP `Page.captureScreenshot`) and return it as an
/// `image::RgbaImage`. Used by the SSIM golden tests.
pub async fn screenshot_canvas(page: &chromiumoxide::Page) -> Result<image::RgbaImage> {
    use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
    use chromiumoxide::page::ScreenshotParams;

    let params = ScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .build();
    let png_bytes = page
        .screenshot(params)
        .await
        .context("page.screenshot failed")?;
    let img = image::load_from_memory(&png_bytes)
        .context("decode screenshot PNG")?
        .to_rgba8();
    Ok(img)
}

/// Compare a captured `RgbaImage` against a checked-in golden PNG on disk.
///
/// Bless mode: setting `GHOSTFRAME_BLESS_GOLDENS=1` writes the captured
/// image to `golden_path` (creating the parent directory if needed) and
/// returns `Ok(())`. Normal mode: loads the golden, computes hybrid SSIM
/// via `image-compare::rgba_hybrid_compare`, and fails if score < threshold.
pub fn assert_ssim_against_golden(
    captured: &image::RgbaImage,
    golden_path: &str,
    threshold: f64,
) -> Result<()> {
    if std::env::var("GHOSTFRAME_BLESS_GOLDENS").is_ok() {
        if let Some(parent) = std::path::Path::new(golden_path).parent() {
            std::fs::create_dir_all(parent).context("create golden parent dir")?;
        }
        captured
            .save(golden_path)
            .context("write blessed golden")?;
        eprintln!("BLESSED golden: {}", golden_path);
        return Ok(());
    }
    let golden = image::open(golden_path)
        .with_context(|| format!("open golden {golden_path}"))?
        .to_rgba8();
    if golden.dimensions() != captured.dimensions() {
        return Err(anyhow!(
            "captured dimensions {:?} != golden {:?} ({})",
            captured.dimensions(),
            golden.dimensions(),
            golden_path,
        ));
    }
    let score = image_compare::rgba_hybrid_compare(captured, &golden)
        .context("ssim compare failed")?;
    if score.score < threshold {
        return Err(anyhow!(
            "SSIM {:.4} < threshold {:.4} against {}",
            score.score,
            threshold,
            golden_path,
        ));
    }
    Ok(())
}
