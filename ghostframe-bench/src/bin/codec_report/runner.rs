//! Scene runner.

use std::time::Duration;

use ghostframe_e2e::harness::{FrameMode, SceneSpec};

use crate::aggregate::ReportState;

pub fn resolve_scenes(names: &[String], duration: Duration) -> Vec<SceneSpec> {
    let mut out = Vec::new();
    for name in names {
        let spec = match name.as_str() {
            "solid" => SceneSpec {
                name: "solid",
                test_pattern_args: vec!["--tile-pattern".into(), "solid".into()],
                duration,
                initial_mode: FrameMode::TileCodec,
            },
            "flat_ui" => SceneSpec {
                name: "flat_ui",
                test_pattern_args: vec!["--tile-pattern".into(), "flat_ui".into()],
                duration,
                initial_mode: FrameMode::TileCodec,
            },
            "text" => SceneSpec {
                name: "text",
                test_pattern_args: vec!["--tile-pattern".into(), "text".into()],
                duration,
                initial_mode: FrameMode::TileCodec,
            },
            "gradient" => SceneSpec {
                name: "gradient",
                test_pattern_args: vec!["--tile-pattern".into(), "gradient".into()],
                duration,
                initial_mode: FrameMode::TileCodec,
            },
            "photo" => SceneSpec {
                name: "photo",
                test_pattern_args: vec!["--tile-pattern".into(), "photo".into()],
                duration,
                initial_mode: FrameMode::TileCodec,
            },
            "motion" => SceneSpec {
                name: "motion",
                test_pattern_args: vec!["--tile-pattern".into(), "motion".into()],
                duration,
                initial_mode: FrameMode::H264,
            },
            "mode_switch" => SceneSpec {
                name: "mode_switch",
                test_pattern_args: vec!["--mode-switch-cycle".into(), "2".into()],
                duration,
                initial_mode: FrameMode::TileCodec,
            },
            other => {
                tracing::warn!(scene = %other, "unknown scene name; skipping");
                continue;
            }
        };
        out.push(spec);
    }
    out
}

pub async fn run_one_scene(spec: &SceneSpec, state: &mut ReportState) -> anyhow::Result<()> {
    let result = ghostframe_e2e::harness::run_scene(spec).await?;
    state.record_scene(spec.name, &result);
    Ok(())
}
