//! `codec_report` CLI definition. See:
//! `docs/superpowers/specs/2026-06-01-m3.5-bench-publication-design.md` §Layer B.

use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

#[derive(Parser, Debug, Clone)]
#[command(name = "codec_report", about = "M3.5 Layer B bench: run scenes, emit markdown report.")]
pub struct Cli {
    /// Output markdown path. Default writes to the spec location.
    #[arg(long, default_value = "docs/specs/m3-codec-bench-results.md")]
    pub output: PathBuf,

    /// Per-scene wall-clock duration. Parsed as humantime-style (e.g. "10s", "30s", "2m").
    #[arg(long, default_value = "10s", value_parser = parse_duration)]
    pub scene_duration: Duration,

    /// Comma-separated scene names to run. Default = all seven.
    #[arg(long, default_value = "solid,flat_ui,text,gradient,photo,motion,mode_switch", value_delimiter = ',')]
    pub scenes: Vec<String>,

    /// GPU description; recorded in the report preamble.
    #[arg(long, default_value = "unknown")]
    pub gpu_name: String,

    /// Path to the criterion side-channel JSON file (from `cargo bench`).
    /// If absent, Layer A columns in the report are populated with "N/A".
    #[arg(long, default_value = "target/criterion/m3-codec-side-channel.json")]
    pub reuse_criterion_json: PathBuf,
}

fn parse_duration(s: &str) -> Result<Duration, String> {
    // Accept "Ns" / "Nms" / "Nm" suffix forms. Reject others.
    let s = s.trim();
    let split = s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split);
    let n: u64 = num_str.parse().map_err(|e: std::num::ParseIntError| e.to_string())?;
    match unit {
        "s" => Ok(Duration::from_secs(n)),
        "ms" => Ok(Duration::from_millis(n)),
        "m" => Ok(Duration::from_secs(n * 60)),
        other => Err(format!("unknown duration unit: {other:?} (accepted: s, ms, m)")),
    }
}
