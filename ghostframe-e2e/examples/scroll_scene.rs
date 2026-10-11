//! Text-under-motion bench scene driver. See
//! `ghostframe_e2e::harness::scroll_scene` for the metric definitions and
//! `ghostframe-bench/benches/README.md` for how to run it.
//!
//! ```text
//! # needs `just containers-build` first, and the host Docker lock
//! cargo run --release -p ghostframe-e2e --example scroll_scene -- \
//!     run --label scroll_10 --mbps 10 --rows 3 --n 3 --out /tmp/scroll.jsonl
//! cargo run --release -p ghostframe-e2e --example scroll_scene -- summarize /tmp/scroll.jsonl
//! ```

use std::io::Write;
use std::time::Duration;

use ghostframe_e2e::harness::scroll_scene::{median, run_scroll_run, ScrollRunSpec};
use ghostframe_test_pattern::scroll_text::render_scroll_frame;
use serde_json::Value;

fn arg<T: std::str::FromStr>(args: &[String], name: &str, default: T) -> T {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn summarize(files: &[String]) {
    use std::collections::BTreeMap;
    let mut by_label: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for f in files {
        for line in std::fs::read_to_string(f).unwrap_or_default().lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v.get("error").is_none() {
                    by_label
                        .entry(v["label"].as_str().unwrap_or("?").to_string())
                        .or_default()
                        .push(v);
                }
            }
        }
    }
    let metrics = [
        "frames_in_window",
        "wire_bytes_per_s_scroll",
        "h264_frame_frac",
        "h264_time_frac",
        "mode_flips",
        "ssim_during_median",
        "ssim_during_min",
        "lag_frames_median",
        "ssim_post1s",
        "recovery_exact_s",
        "recovery_ssim999_s",
        "wire_bytes_5s_after_stop",
        "pixelperfect_lines",
        "pre_scroll_ssim",
    ];
    for (label, runs) in &by_label {
        let recovered = runs.iter().filter(|r| r["recovered"] == true).count();
        let pre_exact = runs
            .iter()
            .filter(|r| r["pre_scroll_exact"] == true)
            .count();
        println!(
            "== {label}  n={}  recovered={recovered}/{}  pre_scroll_exact={pre_exact}/{}",
            runs.len(),
            runs.len(),
            runs.len()
        );
        for m in metrics {
            let mut v: Vec<f64> = runs.iter().filter_map(|r| r[m].as_f64()).collect();
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            if v.is_empty() {
                println!("  {m:<26} (none)");
            } else {
                println!(
                    "  {m:<26} min {:>14.4}  median {:>14.4}  max {:>14.4}  (n={})",
                    v[0],
                    median(&v).unwrap(),
                    v[v.len() - 1],
                    v.len()
                );
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("summarize") => {
            summarize(&args[1..]);
            Ok(())
        }
        Some("run") => {
            let label: String = arg(&args, "--label", "scroll".to_string());
            let mbps: f64 = arg(&args, "--mbps", 10.0);
            let n: u32 = arg(&args, "--n", 1);
            let out: String = arg(&args, "--out", "scroll_scene.jsonl".to_string());
            let spec = ScrollRunSpec {
                label,
                rows_per_frame: arg(&args, "--rows", 3),
                scroll_secs: arg(&args, "--secs", 10),
                fps: arg(&args, "--fps", 30),
                bandwidth_kbps: (mbps * 1000.0) as u32,
                delay_ms: arg(&args, "--delay-ms", 40),
                loss_pct: arg(&args, "--loss-pct", 0),
                width: arg(&args, "--width", 1920),
                height: arg(&args, "--height", 1080),
                recovery_cap: Duration::from_secs(arg(&args, "--recovery-cap-s", 15)),
                shot_interval: Duration::from_millis(arg(&args, "--shot-interval-ms", 600)),
            };
            let render = |w: u32, h: u32, o: u64| render_scroll_frame(w, h, o);
            for i in 0..n {
                // One retry per run: a Chromium CDP start-up timeout is a
                // harness flake, not a result. Both attempts are recorded.
                let mut attempt = 0;
                loop {
                    attempt += 1;
                    let res = run_scroll_run(&spec, &render).await;
                    let line = match res {
                        Ok(mut v) => {
                            v["run"] = i.into();
                            v["attempt"] = attempt.into();
                            eprintln!("run {i} attempt {attempt}: ok");
                            (true, v)
                        }
                        Err(e) => {
                            eprintln!("run {i} attempt {attempt}: FAILED: {e:#}");
                            (
                                false,
                                serde_json::json!({"label": spec.label, "run": i,
                                    "attempt": attempt, "error": format!("{e:#}")}),
                            )
                        }
                    };
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(&out)?;
                    writeln!(f, "{}", line.1)?;
                    if line.0 || attempt >= 2 {
                        break;
                    }
                }
            }
            Ok(())
        }
        _ => {
            eprintln!("usage: scroll_scene run [--label L --mbps M --rows R --n N --out F ...] | summarize FILE...");
            Ok(())
        }
    }
}
