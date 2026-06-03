//! `codec_report` — M3.5 Layer B bench binary. Drives the
//! ghostframe-e2e harness against the configured scenes and emits
//! a markdown report.
//!
//! Spec: docs/superpowers/specs/2026-06-01-m3.5-bench-publication-design.md.

mod bandwidth_matrix;
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
    if args.bandwidth_matrix {
        for point in bandwidth_matrix::BANDWIDTH_POINTS {
            std::env::set_var(
                "GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP",
                point.bytes_per_sec.to_string(),
            );
            for spec in &scenes {
                let labeled_name = format!("{}/{}", point.label, spec.name);
                tracing::info!(scene = %spec.name, bw = point.label, "running scene under bandwidth cap");
                let mut labeled_spec = spec.clone();
                // SAFETY: SceneSpec.name is &'static str. To label with a
                // dynamic prefix we leak the formatted string — it's a
                // bench binary, lives until process exit.
                labeled_spec.name = Box::leak(labeled_name.clone().into_boxed_str());
                runner::run_one_scene(&labeled_spec, &mut state).await?;
            }
            std::env::remove_var("GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP");
        }
    } else {
        for spec in &scenes {
            tracing::info!(name = %spec.name, "running scene");
            runner::run_one_scene(spec, &mut state).await?;
        }
    }

    let criterion_data = report::load_criterion(&args.reuse_criterion_json).ok();
    report::write(&args.output, &state, criterion_data.as_ref())?;
    tracing::info!(out = ?args.output, "report written");
    Ok(())
}
