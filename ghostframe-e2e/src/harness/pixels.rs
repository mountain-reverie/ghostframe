use anyhow::{anyhow, Context, Result};

/// Decode a PNG byte stream (from either `BrowserSession::screenshot` impl)
/// into an `image::RgbaImage` for SSIM comparison.
pub fn decode_screenshot(png_bytes: &[u8]) -> Result<image::RgbaImage> {
    let img = image::load_from_memory(png_bytes)
        .context("decode screenshot PNG")?
        .to_rgba8();
    Ok(img)
}

/// Compare a captured `RgbaImage` against a checked-in golden PNG on disk.
///
/// Bless mode: setting `GHOSTFRAME_BLESS_GOLDENS=1` writes the captured
/// image to `golden_path` and returns `Ok(())`. Normal mode computes hybrid
/// SSIM via `image-compare::rgba_hybrid_compare` and fails if score < threshold.
pub fn assert_ssim_against_golden(
    captured: &image::RgbaImage,
    golden_path: &str,
    threshold: f64,
) -> Result<()> {
    if std::env::var("GHOSTFRAME_BLESS_GOLDENS").is_ok() {
        if let Some(parent) = std::path::Path::new(golden_path).parent() {
            std::fs::create_dir_all(parent).context("create golden parent dir")?;
        }
        captured.save(golden_path).context("write blessed golden")?;
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
    let score =
        image_compare::rgba_hybrid_compare(captured, &golden).context("ssim compare failed")?;
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
