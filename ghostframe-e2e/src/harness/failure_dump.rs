//! What a failing pixel test needs to say where the picture went wrong.
//!
//! A pixel assertion reports that the screen is wrong, never where a tile
//! that the core had already acknowledged was lost on its way to the canvas.
//! This writes, next to the failure's PNGs: the page's swallowed-exception
//! counts and console ring, the renderer's queue state, and the whole server
//! log (the failure printout keeps only a 400-line tail).

use std::path::PathBuf;

/// Directory failure artifacts go to (`target/e2e-artifacts`).
pub fn artifacts_dir() -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("e2e-artifacts");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Persist browser and server state for the failing test `name`. Best effort:
/// a diagnostic must never turn one failure into another.
pub async fn dump_failure_artifacts(page: &chromiumoxide::Page, server_container: &str, name: &str) {
    let dir = artifacts_dir();
    let probe = r#"(() => ({
        health: window.__ghostframeHealth ? window.__ghostframeHealth() : null,
        renderer: window.__ghostframeRendererState ? window.__ghostframeRendererState() : null,
        rafTicks: window.__ghostframeRafTicks ?? null,
        stats: window.__ghostframeStats ?? null,
        cdf53: window.__cdf53CoverageSummary ? window.__cdf53CoverageSummary() : null,
    }))()"#;
    let browser = match page.evaluate(probe).await {
        Ok(v) => v
            .into_value::<serde_json::Value>()
            .unwrap_or_else(|e| serde_json::json!({ "decode_error": e.to_string() })),
        Err(e) => serde_json::json!({ "evaluate_error": e.to_string() }),
    };
    let _ = std::fs::write(
        dir.join(format!("{name}_browser.json")),
        serde_json::to_string_pretty(&browser).unwrap_or_default(),
    );
    let logs = crate::harness::cleanup::read_server_logs_stripped(server_container);
    let _ = std::fs::write(dir.join(format!("{name}_server.log")), logs);
    eprintln!("failure artifacts written to {dir:?} ({name}_browser.json, {name}_server.log)");
}
