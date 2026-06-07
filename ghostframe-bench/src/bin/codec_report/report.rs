//! Markdown report writer.

use std::io::Write;
use std::path::Path;

use crate::aggregate::{ReportState, SceneSummary};

pub fn load_criterion(path: &Path) -> anyhow::Result<serde_json::Value> {
    let text = std::fs::read_to_string(path)?;
    let val: serde_json::Value = serde_json::from_str(&text)?;
    Ok(val)
}

pub fn write(
    f: &mut dyn Write,
    state: &ReportState,
    criterion: Option<&serde_json::Value>,
) -> anyhow::Result<()> {
    // ---- Section 1: Preamble ----
    let git_rev = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    let kernel = std::process::Command::new("uname")
        .arg("-r")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default();
    writeln!(f, "# M3 Codec Bench Results")?;
    writeln!(f)?;
    writeln!(f, "**Date:** {}", chrono::Utc::now().format("%Y-%m-%d"))?;
    writeln!(f, "**Git rev:** `{}`", git_rev.trim())?;
    writeln!(f, "**GPU:** {}", state.args.gpu_name)?;
    writeln!(f, "**Kernel:** `{}`", kernel.trim())?;
    writeln!(f, "**Scene duration:** {:?}", state.args.scene_duration)?;
    writeln!(
        f,
        "**dssim-core version:** 3 (see Cargo.lock for exact pin)"
    )?;
    writeln!(f, "**Constants version:** TODO(M3.5b) — set to `pre-M3.5b` or `post-M3.5b` per Step 5 of the M3.5b plan.")?;
    writeln!(f)?;

    // ---- Section 2: End-to-end latency per scene ----
    writeln!(f, "## 2. End-to-end latency per scene")?;
    writeln!(f)?;
    writeln!(f, "| Scene | server p10 (ms) | client p10 (ms) | sum p10 (ms) | server min (ms) | client min (ms) | server median (ms) | client median (ms) | frames | drop % |")?;
    writeln!(f, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|")?;
    for (name, s) in &state.scenes {
        writeln!(
            f,
            "| `{}` | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {:.2} | {} | {:.1} |",
            name,
            s.server_interval_p10_ns as f64 / 1e6,
            s.client_interval_p10_ms,
            s.server_interval_p10_ns as f64 / 1e6 + s.client_interval_p10_ms,
            s.server_interval_min_ns as f64 / 1e6,
            s.client_interval_min_ms,
            s.server_interval_median_ns as f64 / 1e6,
            s.client_interval_median_ms,
            s.joined_frame_count,
            s.drop_rate_percent,
        )?;
    }
    writeln!(f)?;

    // ---- Section 3: Wire bandwidth + server CPU/RSS per scene ----
    writeln!(f, "## 3. Wire bandwidth + server CPU/RSS per scene")?;
    writeln!(f)?;
    writeln!(
        f,
        "| Scene | egress Mbps | CPU mean % | CPU peak % | RSS max MB | VmHWM max MB |"
    )?;
    writeln!(f, "|---|---:|---:|---:|---:|---:|")?;
    for (name, s) in &state.scenes {
        writeln!(
            f,
            "| `{}` | {:.2} | {:.1} | {:.1} | {:.1} | {:.1} |",
            name, s.wire_mbps, s.cpu_mean_percent, s.cpu_peak_percent, s.rss_max_mb, s.vmhwm_max_mb
        )?;
    }
    writeln!(f)?;

    let classes = ["solid", "flat_ui", "text", "gradient", "photo", "motion"];
    let codecs = ["solid", "pal_rle", "cdf53"];

    // ---- Section 4: Per-codec micro-bench latency (µs) ----
    // Latency lives in criterion's HTML report (not the side-channel JSON);
    // M3.5b stitches it in. M3.5a stub: TODO cells.
    writeln!(f, "## 4. Per-codec micro-bench latency (µs)")?;
    writeln!(f)?;
    writeln!(f, "| Codec | LZ4 | {} |", classes.join(" | "))?;
    writeln!(
        f,
        "|---|:-:|{}|",
        classes.iter().map(|_| "---:").collect::<Vec<_>>().join("|")
    )?;
    for codec in &codecs {
        for lz4 in &[false, true] {
            write!(f, "| `{}` | {} ", codec, if *lz4 { "yes" } else { "no" })?;
            for _class in &classes {
                // Cells filled by M3.5b from criterion's estimates.json files.
                write!(f, "| TODO(M3.5b) ")?;
            }
            writeln!(f, "|")?;
        }
    }
    writeln!(f)?;
    if criterion.is_none() {
        writeln!(f, "_Side-channel JSON not provided. Run with `--reuse-criterion-json target/criterion/m3-codec-side-channel.json` for compressed-size + SSIM data below._")?;
        writeln!(f)?;
    }

    // ---- Section 5: Per-codec compressed size + LZ4 break-even ----
    writeln!(
        f,
        "## 5. Per-codec compressed size (bytes/tile) + LZ4 break-even"
    )?;
    writeln!(f)?;
    writeln!(f, "| Codec | LZ4 | {} |", classes.join(" | "))?;
    writeln!(
        f,
        "|---|:-:|{}|",
        classes.iter().map(|_| "---:").collect::<Vec<_>>().join("|")
    )?;
    for codec in &codecs {
        for lz4 in &[false, true] {
            write!(f, "| `{}` | {} ", codec, if *lz4 { "yes" } else { "no" })?;
            for class in &classes {
                let v = criterion.and_then(|c| {
                    c.get("compressed_bytes")?
                        .as_array()?
                        .iter()
                        .find_map(|entry| {
                            let ec = entry.get("codec")?.as_str()?;
                            let ecl = entry.get("class")?.as_str()?;
                            let elz = entry.get("lz4")?.as_bool()?;
                            if ec == *codec && ecl == *class && elz == *lz4 {
                                entry.get("bytes")?.as_u64()
                            } else {
                                None
                            }
                        })
                });
                match v {
                    Some(n) => write!(f, "| {} ", n)?,
                    None => write!(f, "| TODO(M3.5b) ")?,
                }
            }
            writeln!(f, "|")?;
        }
    }
    writeln!(f)?;
    writeln!(f, "**LZ4 break-even verdict per cell:** TODO(M3.5b) — annotate \"wins / loses / tie (within 5 %)\" by comparing the no-lz4 vs lz4 row per (codec, class).")?;
    writeln!(f)?;

    // ---- Section 6: Cdf53 SSIM vs passes per class ----
    writeln!(f, "## 6. Cdf53 SSIM vs passes per class")?;
    writeln!(f)?;
    writeln!(
        f,
        "| Class | K=1 SSIM | K=2 | K=3 | K=4 | K=5 | K=6 | K=7 | K=8 | K=9 | bytes-to-lossless |"
    )?;
    writeln!(f, "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")?;
    for class in &classes {
        write!(f, "| `{}` ", class)?;
        for k in 1..=9u8 {
            let v = criterion.and_then(|c| {
                c.get("ssim")?.as_array()?.iter().find_map(|e| {
                    let ec = e.get("codec")?.as_str()?;
                    let ecl = e.get("class")?.as_str()?;
                    let ek = e
                        .get("passes_kept")
                        .and_then(|p| p.as_u64())
                        .map(|n| n as u8);
                    if ec == "cdf53_per_pass" && ecl == *class && ek == Some(k) {
                        e.get("ssim")?.as_f64()
                    } else {
                        None
                    }
                })
            });
            match v {
                Some(s) => write!(f, "| {:.4} ", s)?,
                None => write!(f, "| TODO(M3.5b) ")?,
            }
        }
        let btl = criterion.and_then(|c| {
            c.get("bytes_to_lossless")?
                .as_array()?
                .iter()
                .find_map(|e| {
                    if e.get("class")?.as_str()? == *class {
                        e.get("bytes_total")?.as_u64()
                    } else {
                        None
                    }
                })
        });
        match btl {
            Some(n) => writeln!(f, "| {} |", n)?,
            None => writeln!(f, "| TODO(M3.5b) |")?,
        }
    }
    writeln!(f)?;

    // ---- Section 7: BC1 removed ----
    writeln!(f, "## 7. BC1 (removed)")?;
    writeln!(f)?;
    writeln!(f, "_BC1 was dropped after M3.5b's gap analysis showed CDF53 dominating across all measured cells. See `docs/specs/m3-codec-bench-results.md` §7 for the historical analysis; this section is retained only so report numbering stays stable._")?;
    writeln!(f)?;

    // ---- Section 8: Lossless-strategy recommendation ----
    writeln!(f, "## 8. Lossless-strategy recommendation")?;
    writeln!(f)?;
    writeln!(
        f,
        "_TODO(M3.5b) — narrative + proposed L1/L2/L3 values per spec §M3.5b Step 2._"
    )?;
    writeln!(f)?;

    // ---- Section 9: M3.6 Dynamic Policy — mode dwell × bw × scene ----
    writeln!(f, "## 9. Mode dwell × bandwidth × scene (M3.6)")?;
    writeln!(f)?;
    // Bandwidth-matrix runs label scenes "<bw>/<scene>". Other sweep modes
    // (shaping_matrix, headroom_sweep) also use '/' in scene names, so we
    // can't infer matrix mode from key shape alone — gate explicitly on
    // the CLI flag.
    let has_matrix = state.args.bandwidth_matrix;
    if !has_matrix {
        writeln!(
            f,
            "_Single-bandwidth run; matrix bench was not requested via `--bandwidth-matrix`._"
        )?;
        writeln!(f)?;
        writeln!(f, "| Scene | H264 (s) | TileCodec (s) | Mode switches |")?;
        writeln!(f, "|---|---:|---:|---:|")?;
        for (name, summary) in &state.scenes {
            writeln!(
                f,
                "| `{}` | {:.2} | {:.2} | {} |",
                name,
                summary.h264_dwell_us as f64 / 1e6,
                summary.tile_dwell_us as f64 / 1e6,
                summary.mode_switch_count,
            )?;
        }
    } else {
        // Matrix mode: group by scene-base, columns are bandwidth caps.
        // Discover scene-bases and bw labels from the keys.
        let mut scene_bases: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        let mut bw_labels: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
        for key in state.scenes.keys() {
            if let Some((bw, scene)) = key.split_once('/') {
                bw_labels.insert(bw);
                scene_bases.insert(scene);
            }
        }
        // Header row.
        write!(f, "| Scene ")?;
        for bw in &bw_labels {
            write!(f, "| {} (H264s / Tiles / switches) ", bw)?;
        }
        writeln!(f, "|")?;
        write!(f, "|---")?;
        for _ in &bw_labels {
            write!(f, "|---")?;
        }
        writeln!(f, "|")?;
        // Body.
        for scene in &scene_bases {
            write!(f, "| `{}` ", scene)?;
            for bw in &bw_labels {
                let key = format!("{}/{}", bw, scene);
                match state.scenes.get(&key) {
                    Some(s) => {
                        let thrash = if s.mode_switch_count > 5 {
                            " [thrash]"
                        } else {
                            ""
                        };
                        write!(
                            f,
                            "| {:.2} / {:.2} / {}{} ",
                            s.h264_dwell_us as f64 / 1e6,
                            s.tile_dwell_us as f64 / 1e6,
                            s.mode_switch_count,
                            thrash,
                        )?;
                    }
                    None => write!(f, "| _absent_ ")?,
                }
            }
            writeln!(f, "|")?;
        }
    }
    writeln!(f)?;

    // ---- Section 10: Override-trigger frequency × bw ----
    writeln!(f, "## 10. Override-trigger frequency (M3.6)")?;
    writeln!(f)?;
    if !has_matrix {
        // Single-run: sum across scenes; one row.
        let mut totals: std::collections::BTreeMap<String, u32> = std::collections::BTreeMap::new();
        for s in state.scenes.values() {
            for (k, v) in &s.mode_decision_counts {
                *totals.entry(k.clone()).or_default() += v;
            }
        }
        writeln!(f, "| Reason | Count |")?;
        writeln!(f, "|---|---:|")?;
        for reason in &[
            "cost_comparison",
            "headroom_guard",
            "loss_override",
            "suspension",
            "hysteresis_clamp",
        ] {
            writeln!(
                f,
                "| `{}` | {} |",
                reason,
                totals.get(*reason).copied().unwrap_or(0)
            )?;
        }
    } else {
        // Matrix: rows are bandwidth caps; columns are reasons.
        let mut by_bw: std::collections::BTreeMap<&str, std::collections::BTreeMap<String, u32>> =
            std::collections::BTreeMap::new();
        for (key, s) in &state.scenes {
            if let Some((bw, _)) = key.split_once('/') {
                let entry = by_bw.entry(bw).or_default();
                for (k, v) in &s.mode_decision_counts {
                    *entry.entry(k.clone()).or_default() += v;
                }
            }
        }
        writeln!(f, "| Cap | cost_comparison | headroom_guard | loss_override | suspension | hysteresis_clamp |")?;
        writeln!(f, "|---|---:|---:|---:|---:|---:|")?;
        for (bw, counts) in &by_bw {
            let g = |k: &str| counts.get(k).copied().unwrap_or(0);
            writeln!(
                f,
                "| {} | {} | {} | {} | {} | {} |",
                bw,
                g("cost_comparison"),
                g("headroom_guard"),
                g("loss_override"),
                g("suspension"),
                g("hysteresis_clamp"),
            )?;
        }
    }
    writeln!(f)?;

    // ---- Section 11: M3.7a Bias sweep ----
    {
        let mut rows: Vec<(&String, &SceneSummary)> = state
            .scenes
            .iter()
            .filter(|(_, s)| {
                s.sweep_axis_label
                    .as_deref()
                    .map(|l| l.starts_with("bias_"))
                    .unwrap_or(false)
            })
            .collect();
        rows.sort_by_key(|(name, _)| name.as_str());

        if !rows.is_empty() {
            writeln!(f, "## 11. Bias sweep (REFINEMENT_BIAS_PER_TILE_US) — M3.7a")?;
            writeln!(f)?;
            writeln!(
                f,
                "| Bias µs | PixelPerfect | Mode switches | Capture→paint p50 ms | Drop % |"
            )?;
            writeln!(f, "|---|---:|---:|---:|---:|")?;
            for (_, s) in &rows {
                let bias_label = s.sweep_axis_label.as_deref().unwrap_or("?");
                let bias_us: f32 = bias_label
                    .strip_prefix("bias_")
                    .and_then(|s| s.strip_suffix("us"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(f32::NAN); // surfaces label-format drift in the report
                writeln!(
                    f,
                    "| {:.1} | {} | {} | {:.2} | {:.1} |",
                    bias_us,
                    s.pixelperfect_count,
                    s.mode_switch_count,
                    s.client_interval_median_ms,
                    s.drop_rate_percent,
                )?;
            }
            writeln!(f)?;
            writeln!(f, "_Verdict rule: pick the bias with highest PixelPerfect/switches ratio whose latency p50 isn't > 10% above the lowest-swept value._")?;
            writeln!(f)?;
        }
    }

    // ---- Section 12: M3.7a Loss axis ----
    {
        let mut rows: Vec<(&String, &SceneSummary)> = state
            .scenes
            .iter()
            .filter(|(_, s)| {
                s.sweep_axis_label
                    .as_deref()
                    .map(|l| l.starts_with("loss_"))
                    .unwrap_or(false)
            })
            .collect();
        rows.sort_by_key(|(name, _)| name.as_str());

        if !rows.is_empty() {
            writeln!(f, "## 12. Loss axis (LOSS_OVERRIDE_THRESHOLD) — M3.7a")?;
            writeln!(f)?;
            writeln!(
                f,
                "| Loss % | Decode errors | Override fires | Mode switches | PixelPerfect |"
            )?;
            writeln!(f, "|---:|---:|---:|---:|---:|")?;
            for (_, s) in &rows {
                let label = s.sweep_axis_label.as_deref().unwrap_or("?");
                let loss_pct: u32 = label
                    .strip_prefix("loss_")
                    .and_then(|s| s.strip_suffix("pct"))
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(u32::MAX); // surfaces label-format drift in the report
                let loss_pct_cell = if loss_pct == u32::MAX {
                    "PARSE_ERR".to_string()
                } else {
                    format!("{}", loss_pct)
                };
                let override_count = s
                    .mode_decision_counts
                    .get("loss_override")
                    .copied()
                    .unwrap_or(0);
                writeln!(
                    f,
                    "| {} | {} | {} | {} | {} |",
                    loss_pct_cell,
                    s.decode_error_count,
                    override_count,
                    s.mode_switch_count,
                    s.pixelperfect_count,
                )?;
            }
            writeln!(f)?;
            writeln!(f, "_Verdict rule: threshold goes at the loss rate where decode_error_count first jumps; if no jump, keep 0.10._")?;
            writeln!(f)?;
        }
    }

    // ---- Section 13: M3.7b Shaping matrix (realistic-cwnd mode dwell) ----
    {
        let mut rows: Vec<(&String, &SceneSummary)> = state
            .scenes
            .iter()
            .filter(|(_, s)| {
                s.sweep_axis_label
                    .as_deref()
                    .map(|l| l.starts_with("shape_"))
                    .unwrap_or(false)
            })
            .collect();
        rows.sort_by_key(|(a, _)| *a);

        if !rows.is_empty() {
            writeln!(
                f,
                "## 13. Shaping matrix — realistic-cwnd mode dwell (M3.7b)"
            )?;
            writeln!(f)?;
            writeln!(
                f,
                "| Scene/Shape | H264 (s) | TileCodec (s) | Mode switches | PixelPerfect |"
            )?;
            writeln!(f, "|---|---:|---:|---:|---:|")?;
            for (name, s) in &rows {
                writeln!(
                    f,
                    "| `{}` | {:.2} | {:.2} | {} | {} |",
                    name,
                    s.h264_dwell_us as f64 / 1e6,
                    s.tile_dwell_us as f64 / 1e6,
                    s.mode_switch_count,
                    s.pixelperfect_count,
                )?;
            }
            writeln!(f)?;
            writeln!(f, "_Replaces M3.6c Table 9 for realistic-cwnd analyses. tc-shaped via netem+tbf inside the container._")?;
            writeln!(f)?;
        }
    }

    // ---- Section 14: M3.7b Headroom sweep (HEADROOM_MIN_BYTES_PER_US) ----
    {
        let mut rows: Vec<(&String, &SceneSummary)> = state
            .scenes
            .iter()
            .filter(|(_, s)| {
                s.sweep_axis_label
                    .as_deref()
                    .map(|l| l.starts_with("headroom_"))
                    .unwrap_or(false)
            })
            .collect();
        rows.sort_by_key(|(a, _)| *a);

        if !rows.is_empty() {
            writeln!(
                f,
                "## 14. Headroom sweep (HEADROOM_MIN_BYTES_PER_US) — M3.7b"
            )?;
            writeln!(f)?;
            writeln!(f, "| Threshold B/µs | Shape | Capture→paint median ms | Headroom fires | Mode switches | PixelPerfect |")?;
            writeln!(f, "|---:|---|---:|---:|---:|---:|")?;
            for (_name, s) in &rows {
                let label = s.sweep_axis_label.as_deref().unwrap_or("?");
                let (hr_label, shape_label) = label.split_once('/').unwrap_or((label, ""));
                let hr_value: f32 = hr_label
                    .strip_prefix("headroom_")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(f32::NAN); // surfaces label-format drift in the report
                let headroom_fires = s
                    .mode_decision_counts
                    .get("headroom_guard")
                    .copied()
                    .unwrap_or(0);
                writeln!(
                    f,
                    "| {:.2} | {} | {:.2} | {} | {} | {} |",
                    hr_value,
                    shape_label,
                    s.client_interval_median_ms,
                    headroom_fires,
                    s.mode_switch_count,
                    s.pixelperfect_count,
                )?;
            }
            writeln!(f)?;
            writeln!(f, "_Verdict rule: pick the lowest threshold whose latency stays acceptable across all 3 shaping points._")?;
            writeln!(f)?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::aggregate::{ReportState, SceneSummary};
    use crate::cli::Cli;
    use clap::Parser;

    fn make_test_state(matrix: bool) -> ReportState {
        let mut argv = vec!["codec_report"];
        if matrix {
            argv.push("--bandwidth-matrix");
        }
        let args = Cli::parse_from(argv);
        let mut state = ReportState::new(&args);
        if matrix {
            // Simulate bandwidth-matrix run with 2 bw caps × 2 scenes.
            for bw in &["10Mbps", "30Mbps"] {
                for scene in &["solid", "motion"] {
                    let key = format!("{}/{}", bw, scene);
                    let mut s = SceneSummary {
                        h264_dwell_us: 1_500_000,
                        tile_dwell_us: 2_000_000,
                        mode_switch_count: if *bw == "10Mbps" { 7 } else { 1 },
                        ..Default::default()
                    };
                    *s.mode_decision_counts
                        .entry("cost_comparison".into())
                        .or_default() += 3;
                    *s.mode_decision_counts
                        .entry("headroom_guard".into())
                        .or_default() += 1;
                    state.scenes.insert(key, s);
                }
            }
        } else {
            let mut s = SceneSummary {
                h264_dwell_us: 3_000_000,
                tile_dwell_us: 7_000_000,
                mode_switch_count: 2,
                ..Default::default()
            };
            *s.mode_decision_counts
                .entry("cost_comparison".into())
                .or_default() += 5;
            state.scenes.insert("solid".into(), s);
        }
        state
    }

    #[test]
    fn sections_9_10_single_bw() {
        let state = make_test_state(false);
        let mut buf = Vec::new();
        write(&mut buf, &state, None).expect("write should succeed");
        let text = String::from_utf8(buf).expect("valid utf8");
        assert!(
            text.contains("## 9. Mode dwell × bandwidth × scene"),
            "section 9 header"
        );
        assert!(
            text.contains("## 10. Override-trigger frequency"),
            "section 10 header"
        );
        assert!(text.contains("_Single-bandwidth run"), "single-bw note");
        assert!(
            text.contains("H264 (s) | TileCodec (s) | Mode switches"),
            "table 9 headers"
        );
        assert!(text.contains("| Reason | Count |"), "table 10 headers");
        // h264_dwell_us = 3_000_000 → 3.00 s
        assert!(text.contains("3.00"), "h264 dwell seconds present");
        // cost_comparison count = 5
        assert!(
            text.contains("| `cost_comparison` | 5 |"),
            "cost_comparison count"
        );
    }

    #[test]
    fn sections_9_10_matrix_bw() {
        let state = make_test_state(true);
        let mut buf = Vec::new();
        write(&mut buf, &state, None).expect("write should succeed");
        let text = String::from_utf8(buf).expect("valid utf8");
        assert!(
            text.contains("## 9. Mode dwell × bandwidth × scene"),
            "section 9 header"
        );
        assert!(
            text.contains("## 10. Override-trigger frequency"),
            "section 10 header"
        );
        // matrix headers should NOT have single-bw note
        assert!(
            !text.contains("_Single-bandwidth run"),
            "no single-bw note in matrix mode"
        );
        // 10Mbps / solid has mode_switch_count=7 > 5 → [thrash]
        assert!(text.contains("[thrash]"), "[thrash] annotation present");
        // Table 10 matrix: bw rows
        assert!(text.contains("cost_comparison"), "cost_comparison column");
    }

    #[test]
    fn write_emits_sections_11_and_12_when_sweep_labels_present() {
        use std::collections::BTreeMap;
        let args = Cli::parse_from(["codec_report"]);
        let mut state = ReportState::new(&args);
        // Inject one scene per sweep family.
        let bias_summary = SceneSummary {
            sweep_axis_label: Some("bias_10us".into()),
            pixelperfect_count: 42,
            mode_switch_count: 3,
            ..Default::default()
        };
        state.scenes.insert("bias_10us/text".into(), bias_summary);
        let mut loss_summary = SceneSummary {
            sweep_axis_label: Some("loss_5pct".into()),
            decode_error_count: 7,
            ..Default::default()
        };
        let mut counts = BTreeMap::new();
        counts.insert("loss_override".into(), 2);
        loss_summary.mode_decision_counts = counts;
        state
            .scenes
            .insert("loss_5pct/mode_switch".into(), loss_summary);

        let mut buf = Vec::new();
        write(&mut buf, &state, None).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("## 11. Bias sweep"), "section 11 header");
        assert!(text.contains("## 12. Loss axis"), "section 12 header");
        assert!(text.contains("| 10.0 |"), "bias row formatted");
        assert!(text.contains("| 5 |"), "loss row formatted");
    }

    #[test]
    fn write_emits_sections_13_and_14_when_sweep_labels_present() {
        use std::collections::BTreeMap;
        let cli = crate::cli::Cli {
            output: "/tmp/dummy.md".into(),
            scene_duration: std::time::Duration::from_secs(1),
            scenes: vec!["dummy".into()],
            gpu_name: "test".into(),
            reuse_criterion_json: "/tmp/dummy.json".into(),
            bandwidth_matrix: false,
            bias_sweep: false,
            loss_axis: false,
            shaping_matrix: false,
            headroom_sweep: false,
        };
        let mut state = ReportState::new(&cli);
        // Shaping matrix scene.
        let shape_summary = SceneSummary {
            sweep_axis_label: Some("shape_10mbps_dsl".into()),
            h264_dwell_us: 3_000_000,
            tile_dwell_us: 7_000_000,
            mode_switch_count: 5,
            pixelperfect_count: 100,
            ..Default::default()
        };
        state.scenes.insert("10mbps_dsl/text".into(), shape_summary);
        // Headroom sweep scene.
        let mut counts = BTreeMap::new();
        counts.insert("headroom_guard".into(), 3);
        let hr_summary = SceneSummary {
            sweep_axis_label: Some("headroom_0.50/1mbps".into()),
            mode_switch_count: 8,
            pixelperfect_count: 50,
            mode_decision_counts: counts,
            ..Default::default()
        };
        state
            .scenes
            .insert("headroom_0.50/1mbps".into(), hr_summary);

        let mut buf = Vec::new();
        write(&mut buf, &state, None).unwrap();
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("## 13. Shaping matrix"), "section 13 header");
        assert!(text.contains("## 14. Headroom sweep"), "section 14 header");
        assert!(text.contains("`10mbps_dsl/text`"), "shape row");
        assert!(text.contains("| 0.50 |"), "headroom row threshold");
    }
}
