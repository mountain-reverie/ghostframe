//! Smoke test for `ghostframe_e2e::harness::run_scene`.
//!
//! Locks the public API shape and verifies a short solid-tile scene
//! produces non-empty server_telemetry and proc_samples. Does NOT
//! verify the produced numbers (that's the M3.5b bench's job) —
//! only proves the pipeline runs end-to-end.

use std::time::Duration;
use ghostframe_e2e::harness::{run_scene, FrameMode, SceneSpec};

#[tokio::test]
#[ignore = "requires Docker + GPU; run with `cargo test --test e2e -- --ignored harness_smoke`"]
async fn harness_smoke_solid_1s() {
    let spec = SceneSpec {
        name: "smoke_solid",
        test_pattern_args: vec!["--tile-pattern".into(), "solid".into()],
        duration: Duration::from_secs(1),
        initial_mode: FrameMode::TileCodec,
    };
    let result = run_scene(&spec).await.expect("run_scene");
    assert!(!result.server_telemetry.is_empty(), "no server telemetry recorded");
    assert!(!result.proc_samples.is_empty(), "no proc samples recorded");
    // Client diagnostics may be empty if Chromium painted 0 frames in 1s; allow that.
    assert!(result.wire_bytes_total > 0, "no wire bytes recorded");
    assert!(result.error.is_none(), "scene reported error: {:?}", result.error);
}
