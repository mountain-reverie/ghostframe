//! `codec_report` — M3.5 Layer B bench binary. Drives the
//! ghostframe-e2e harness against the configured scenes and emits
//! a markdown report.
//!
//! Spec: docs/superpowers/specs/2026-06-01-m3.5-bench-publication-design.md.

mod bandwidth_matrix;
mod bias_sweep;
mod loss_axis;
mod shaping_matrix;
mod headroom_sweep;
mod cli;
mod runner;
mod aggregate;
mod report;

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "codec_report=info".into()),
        )
        .init();

    let args = cli::Cli::parse();
    tracing::info!(?args, "codec_report start");

    let scenes = runner::resolve_scenes(&args.scenes, args.scene_duration);
    let mut state = aggregate::ReportState::new(&args);
    if args.headroom_sweep {
        use ghostframe_e2e::harness::{FrameMode, SceneSpec};
        // CDF53 + CAPTURE_FPS needed for PixelPerfect signal (see M3.7a note).
        std::env::set_var("GHOSTFRAME_ENABLE_CDF53", "1");
        std::env::set_var("CAPTURE_FPS", "30");
        for shape in headroom_sweep::HEADROOM_SHAPING_POINTS {
            std::env::set_var("SHAPE_BANDWIDTH_KBPS", shape.bandwidth_kbps.to_string());
            std::env::set_var("SHAPE_DELAY_MS", shape.delay_ms.to_string());
            std::env::set_var("SHAPE_LOSS_PCT", shape.loss_pct.to_string());
            for hp in headroom_sweep::HEADROOM_VALUES_BPUS {
                std::env::set_var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS", hp.value_bpus.to_string());
                let labeled_name = format!("{}/{}", hp.label, shape.label);
                let spec = SceneSpec {
                    name: Box::leak(labeled_name.clone().into_boxed_str()),
                    test_pattern_args: headroom_sweep::DEFAULT_TEST_PATTERN_ARGS.iter().map(|s| s.to_string()).collect(),
                    duration: args.scene_duration,
                    initial_mode: FrameMode::TileCodec,
                };
                tracing::info!(headroom_bpus = hp.value_bpus, shape = shape.label, "headroom-sweep run");
                if let Err(e) = runner::run_one_scene(&spec, &mut state).await {
                    tracing::warn!(headroom_bpus = hp.value_bpus, shape = shape.label, error = %e, "headroom-sweep scene failed, continuing");
                }
                if let Some(summary) = state.scenes.get_mut(&labeled_name) {
                    summary.sweep_axis_label = Some(format!("{}/{}", hp.label, shape.label));
                }
                std::env::remove_var("GHOSTFRAME_TEST_HEADROOM_MIN_BPUS");
            }
            std::env::remove_var("SHAPE_BANDWIDTH_KBPS");
            std::env::remove_var("SHAPE_DELAY_MS");
            std::env::remove_var("SHAPE_LOSS_PCT");
        }
        std::env::remove_var("GHOSTFRAME_ENABLE_CDF53");
        std::env::remove_var("CAPTURE_FPS");
    } else if args.shaping_matrix {
        std::env::set_var("GHOSTFRAME_ENABLE_CDF53", "1");
        std::env::set_var("CAPTURE_FPS", "30");
        let scenes = runner::resolve_scenes(&args.scenes, args.scene_duration);
        for shape in shaping_matrix::SHAPING_POINTS {
            std::env::set_var("SHAPE_BANDWIDTH_KBPS", shape.bandwidth_kbps.to_string());
            std::env::set_var("SHAPE_DELAY_MS", shape.delay_ms.to_string());
            std::env::set_var("SHAPE_LOSS_PCT", shape.loss_pct.to_string());
            for spec in &scenes {
                let labeled_name = format!("{}/{}", shape.label, spec.name);
                let mut labeled_spec = spec.clone();
                labeled_spec.name = Box::leak(labeled_name.clone().into_boxed_str());
                tracing::info!(scene = spec.name, shape = shape.label, "shaping-matrix run");
                if let Err(e) = runner::run_one_scene(&labeled_spec, &mut state).await {
                    tracing::warn!(scene = spec.name, shape = shape.label, error = %e, "shaping-matrix scene failed, continuing");
                }
                if let Some(summary) = state.scenes.get_mut(&labeled_name) {
                    summary.sweep_axis_label = Some(format!("shape_{}", shape.label));
                }
            }
            std::env::remove_var("SHAPE_BANDWIDTH_KBPS");
            std::env::remove_var("SHAPE_DELAY_MS");
            std::env::remove_var("SHAPE_LOSS_PCT");
        }
        std::env::remove_var("GHOSTFRAME_ENABLE_CDF53");
        std::env::remove_var("CAPTURE_FPS");
    } else if args.bias_sweep {
        use ghostframe_e2e::harness::{FrameMode, SceneSpec};
        // Required for CDF53 refinement to fire at all — without this
        // env var the codec is disabled in xdaemon and the PixelPerfect
        // outcome metric stays at zero regardless of bias value.
        std::env::set_var("GHOSTFRAME_ENABLE_CDF53", "1");
        // 30 fps gives the refinement loop enough ticks per second to
        // complete all 14 passes for a tile within a 12-second static
        // half (matches e2e_progressive_refinement which observes 720+).
        std::env::set_var("CAPTURE_FPS", "30");
        for point in bias_sweep::BIAS_VALUES_US {
            std::env::set_var("GHOSTFRAME_TEST_REFINEMENT_BIAS_US", point.value_us.to_string());
            std::env::set_var(
                "GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP",
                bias_sweep::DEFAULT_BANDWIDTH_BYTES_PER_SEC.to_string(),
            );
            if bias_sweep::DEFAULT_LOSS_PROBABILITY > 0.0 {
                std::env::set_var(
                    "GHOSTFRAME_INBOUND_LOSS_PROBABILITY",
                    bias_sweep::DEFAULT_LOSS_PROBABILITY.to_string(),
                );
                std::env::set_var("GHOSTFRAME_INBOUND_LOSS_SEED", "42");
                std::env::set_var("GHOSTFRAME_INBOUND_LOSS_PREDICATE", "tile");
            }
            let labeled_name = format!("{}/{}", point.label, bias_sweep::DEFAULT_SCENE_NAME);
            let spec = SceneSpec {
                name: Box::leak(labeled_name.clone().into_boxed_str()),
                test_pattern_args: bias_sweep::DEFAULT_TEST_PATTERN_ARGS.iter().map(|s| s.to_string()).collect(),
                duration: args.scene_duration,
                initial_mode: FrameMode::TileCodec,
            };
            tracing::info!(bias_us = point.value_us, scene = bias_sweep::DEFAULT_SCENE_NAME, "bias-sweep run");
            if let Err(e) = runner::run_one_scene(&spec, &mut state).await {
                tracing::warn!(bias_us = point.value_us, error = %e, "bias-sweep scene failed, continuing");
            }
            if let Some(summary) = state.scenes.get_mut(&labeled_name) {
                summary.sweep_axis_label = Some(point.label.to_string());
            }
            std::env::remove_var("GHOSTFRAME_TEST_REFINEMENT_BIAS_US");
            std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PROBABILITY");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_SEED");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PREDICATE");
        }
        std::env::remove_var("GHOSTFRAME_ENABLE_CDF53");
        std::env::remove_var("CAPTURE_FPS");
    } else if args.loss_axis {
        use ghostframe_e2e::harness::{FrameMode, SceneSpec};
        // Required for CDF53 refinement (PixelPerfect column needs it).
        // The threshold ITSELF stays at production default (0.10) across
        // the sweep — we vary the inbound loss rate and observe where
        // the default threshold starts firing loss_override.
        std::env::set_var("GHOSTFRAME_ENABLE_CDF53", "1");
        std::env::set_var("CAPTURE_FPS", "30");
        for point in loss_axis::LOSS_VALUES {
            std::env::set_var(
                "GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP",
                loss_axis::DEFAULT_BANDWIDTH_BYTES_PER_SEC.to_string(),
            );
            std::env::set_var("GHOSTFRAME_INBOUND_LOSS_PROBABILITY", point.probability.to_string());
            std::env::set_var("GHOSTFRAME_INBOUND_LOSS_SEED", "42");
            std::env::set_var("GHOSTFRAME_INBOUND_LOSS_PREDICATE", "tile");
            let labeled_name = format!("{}/{}", point.label, loss_axis::DEFAULT_SCENE_NAME);
            let spec = SceneSpec {
                name: Box::leak(labeled_name.clone().into_boxed_str()),
                test_pattern_args: loss_axis::DEFAULT_TEST_PATTERN_ARGS.iter().map(|s| s.to_string()).collect(),
                duration: args.scene_duration,
                initial_mode: FrameMode::TileCodec,
            };
            tracing::info!(loss = point.probability, "loss-axis run");
            if let Err(e) = runner::run_one_scene(&spec, &mut state).await {
                tracing::warn!(loss = point.probability, error = %e, "loss-axis scene failed, continuing");
            }
            if let Some(summary) = state.scenes.get_mut(&labeled_name) {
                summary.sweep_axis_label = Some(point.label.to_string());
            }
            std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PROBABILITY");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_SEED");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PREDICATE");
        }
        std::env::remove_var("GHOSTFRAME_ENABLE_CDF53");
        std::env::remove_var("CAPTURE_FPS");
    } else if args.bandwidth_matrix {
        for point in bandwidth_matrix::BANDWIDTH_POINTS {
            std::env::set_var(
                "GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP",
                point.bytes_per_sec.to_string(),
            );
            if point.loss_probability > 0.0 {
                std::env::set_var(
                    "GHOSTFRAME_INBOUND_LOSS_PROBABILITY",
                    point.loss_probability.to_string(),
                );
                std::env::set_var("GHOSTFRAME_INBOUND_LOSS_SEED", "42");
                std::env::set_var("GHOSTFRAME_INBOUND_LOSS_PREDICATE", "tile");
            }
            for spec in &scenes {
                let labeled_name = format!("{}/{}", point.label, spec.name);
                tracing::info!(
                    scene = %spec.name, bw = point.label,
                    loss = point.loss_probability,
                    "running scene under bandwidth + loss"
                );
                let mut labeled_spec = spec.clone();
                // SAFETY: SceneSpec.name is &'static str. To label with a
                // dynamic prefix we leak the formatted string — it's a
                // bench binary, lives until process exit.
                labeled_spec.name = Box::leak(labeled_name.clone().into_boxed_str());
                runner::run_one_scene(&labeled_spec, &mut state).await?;
            }
            std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PROBABILITY");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_SEED");
            std::env::remove_var("GHOSTFRAME_INBOUND_LOSS_PREDICATE");
        }
    } else {
        for spec in &scenes {
            tracing::info!(name = %spec.name, "running scene");
            runner::run_one_scene(spec, &mut state).await?;
        }
    }

    let criterion_data = report::load_criterion(&args.reuse_criterion_json).ok();
    if let Some(parent) = args.output.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out_file = std::fs::File::create(&args.output)?;
    report::write(&mut out_file, &state, criterion_data.as_ref())?;
    tracing::info!(out = ?args.output, "report written");
    Ok(())
}
