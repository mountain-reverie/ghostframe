//! `codec_report` — M3.5 Layer B bench binary. Drives the
//! ghostframe-e2e harness against the configured scenes and emits
//! a markdown report.
//!
//! Spec: docs/superpowers/specs/2026-06-01-m3.5-bench-publication-design.md.

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
    for spec in &scenes {
        tracing::info!(name = %spec.name, "running scene");
        runner::run_one_scene(spec, &mut state).await?;
    }

    let criterion_data = report::load_criterion(&args.reuse_criterion_json).ok();
    report::write(&args.output, &state, criterion_data.as_ref())?;
    tracing::info!(out = ?args.output, "report written");
    Ok(())
}
