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
///
/// `tiles` names the tiles the test found wrong; their renderer state and
/// framebuffer pixel go to `{name}_tiles.json`.
pub async fn dump_failure_artifacts(
    page: &chromiumoxide::Page,
    server_container: &str,
    name: &str,
    tiles: &[(u32, u32)],
) {
    let dir = artifacts_dir();
    let probe = r#"(() => ({
        health: window.__ghostframeHealth ? window.__ghostframeHealth() : null,
        renderer: window.__ghostframeRendererState ? window.__ghostframeRendererState() : null,
        rafTicks: window.__ghostframeRafTicks ?? null,
        stats: window.__ghostframeStats ?? null,
        resizes: window.__ghostframeRecordedResizes ?? null,
        firstTiles: (window.__ghostframeRecordedTiles ?? []).slice(0, 12),
        recordedTileCount: (window.__ghostframeRecordedTiles ?? []).length,
        cdf53: window.__cdf53CoverageSummary ? window.__cdf53CoverageSummary() : null,
    }))()"#;
    // For each tile the test found wrong, and each tile the client was told
    // to paint solid red: what the renderer last did to it (codec, frame_seq,
    // paint tick, CDF 5/3 host state) next to the framebuffer pixel at its
    // centre. Read before the heal probe below, which repaints.
    let wanted: Vec<serde_json::Value> = tiles
        .iter()
        .map(|&(x, y)| serde_json::json!({ "tileX": x, "tileY": y, "what": "test" }))
        .collect();
    let readback = format!(
        r#"(async () => {{
        const h = window.__ghostframeHealth ? window.__ghostframeHealth() : null;
        const want = {wanted};
        const out = [];
        for (const m of want.concat(h ? h.marks : [])) {{
            const px = window.__readPixelRect ? await window.__readPixelRect(m.tileX * 32 + 16, m.tileY * 32 + 16, 1, 1) : null;
            const state = window.__ghostframeTile ? window.__ghostframeTile(m.tileX, m.tileY) : null;
            out.push({{ ...m, px: px ? Array.from(px).slice(0, 4) : null, state }});
        }}
        return out;
    }})()"#,
        wanted = serde_json::Value::Array(wanted)
    );
    let marks = match page.evaluate(readback).await {
        Ok(v) => v
            .into_value::<serde_json::Value>()
            .unwrap_or_else(|e| serde_json::json!({ "decode_error": e.to_string() })),
        Err(e) => serde_json::json!({ "evaluate_error": e.to_string() }),
    };
    let _ = std::fs::write(
        dir.join(format!("{name}_tiles.json")),
        serde_json::to_string_pretty(&marks).unwrap_or_default(),
    );
    // When the page was loaded with ?debug: re-lift every CDF 5/3 tile from
    // its accumulated coefficients and count the pixels that change. Non-zero
    // means the incremental (dirty-list) lift left tiles stale although the
    // coefficient state was complete.
    let heal = r#"(async () => {
        if (!window.__cdf53FullGridDiff || !window.__readPixelRect) return null;
        const before = await window.__readPixelRect(672, 0, 1, 1);
        const diff = await window.__cdf53FullGridDiff();
        const after = await window.__readPixelRect(672, 0, 1, 1);
        return { diff, before: Array.from(before).slice(0, 4), after: Array.from(after).slice(0, 4) };
    })()"#;
    let healed = match page.evaluate(heal).await {
        Ok(v) => v
            .into_value::<serde_json::Value>()
            .unwrap_or_else(|e| serde_json::json!({ "decode_error": e.to_string() })),
        Err(e) => serde_json::json!({ "evaluate_error": e.to_string() }),
    };
    let _ = std::fs::write(
        dir.join(format!("{name}_heal.json")),
        serde_json::to_string_pretty(&healed).unwrap_or_default(),
    );
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
    eprintln!(
        "failure artifacts written to {dir:?} ({name}_browser.json, {name}_tiles.json, {name}_server.log)"
    );
}
