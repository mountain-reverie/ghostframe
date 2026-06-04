//! Per-scene aggregation for the M3.5 codec_report.

use std::collections::BTreeMap;

use ghostframe_e2e::harness::SceneResult;

use crate::cli::Cli;

pub struct ReportState {
    pub args: Cli,
    pub scenes: BTreeMap<String, SceneSummary>,
}

#[derive(Debug, Default)]
pub struct SceneSummary {
    pub server_interval_min_ns: u64,
    pub server_interval_p10_ns: u64,
    pub server_interval_median_ns: u64,
    pub client_interval_min_ms: f64,
    pub client_interval_p10_ms: f64,
    pub client_interval_median_ms: f64,
    pub joined_frame_count: usize,
    pub drop_rate_percent: f64,
    pub wire_mbps: f64,
    pub cpu_mean_percent: f64,
    pub cpu_peak_percent: f64,
    pub rss_max_mb: f64,
    pub vmhwm_max_mb: f64,
    pub codec_wire_bytes: BTreeMap<String, u64>,

    // M3.6c additions
    /// Total wall-clock µs spent in "h264" mode (sum across frames).
    pub h264_dwell_us: u64,
    /// Total wall-clock µs spent in "tile" mode.
    pub tile_dwell_us: u64,
    /// Count of `h264` ↔ `tile` mode flips in the server-telemetry timeline.
    pub mode_switch_count: u32,
    /// Count of mode.decision events per reason.
    /// Keys: "cost_comparison", "headroom_guard", "loss_override",
    /// "suspension", "hysteresis_clamp".
    pub mode_decision_counts: std::collections::BTreeMap<String, u32>,

    // M3.7a additions
    pub pixelperfect_count: u32,
    pub decode_error_count: u32,
    /// When this scene was part of a M3.7 sweep, the label of the swept
    /// value (e.g. "bias_10us" or "loss_5pct"). None for non-sweep runs.
    pub sweep_axis_label: Option<String>,
}

impl ReportState {
    pub fn new(args: &Cli) -> Self {
        Self { args: args.clone(), scenes: BTreeMap::new() }
    }

    pub fn record_scene(&mut self, name: &str, r: &SceneResult) {
        let server_intervals_ns: Vec<u64> = r.server_telemetry.iter()
            .filter(|st| st.capture_done_ns > 0 && st.last_send_ns >= st.capture_done_ns)
            .map(|st| st.last_send_ns - st.capture_done_ns)
            .collect();
        let client_intervals_ms: Vec<f64> = r.client_diagnostics.iter()
            .filter(|cd| cd.first_recv_ms > 0.0 && cd.raf_ms >= cd.first_recv_ms)
            .map(|cd| cd.raf_ms - cd.first_recv_ms)
            .collect();

        let total_frames = r.server_telemetry.len();
        let joined = client_intervals_ms.len();
        let drop_pct = if total_frames > 0 {
            (1.0 - joined as f64 / total_frames as f64) * 100.0
        } else {
            0.0
        };

        // CPU%: total ticks delta over scene duration windows.
        let clock_tick = {
            let raw = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            if raw > 0 { raw as f64 } else { 100.0 }
        };
        let (cpu_mean, cpu_peak) = compute_cpu(&r.proc_samples, clock_tick);
        let rss_max_kb = r.proc_samples.iter().map(|s| s.rss_kb).max().unwrap_or(0);
        let vmhwm_max_kb = r.proc_samples.iter().map(|s| s.vmhwm_kb).max().unwrap_or(0);

        let duration_secs = self.args.scene_duration.as_secs_f64().max(0.001);
        let wire_mbps = (r.wire_bytes_total as f64 * 8.0) / duration_secs / 1_000_000.0;

        // Per-codec wire bytes: distribute total_wire_bytes proportionally
        // across codecs in each frame's histogram (first-pass attribution).
        let mut codec_bytes: BTreeMap<String, u64> = BTreeMap::new();
        for st in &r.server_telemetry {
            let hist = &st.codec_histogram;
            let tile_total = (hist.solid + hist.palrle + hist.cdf53 + hist.raw) as u64
                + hist.h264 as u64;
            if tile_total == 0 {
                continue;
            }
            let bytes = st.total_wire_bytes as u64;
            *codec_bytes.entry("solid".into()).or_default() +=
                bytes * hist.solid as u64 / tile_total;
            *codec_bytes.entry("palrle".into()).or_default() +=
                bytes * hist.palrle as u64 / tile_total;
            *codec_bytes.entry("cdf53".into()).or_default() +=
                bytes * hist.cdf53 as u64 / tile_total;
            *codec_bytes.entry("raw".into()).or_default() +=
                bytes * hist.raw as u64 / tile_total;
            *codec_bytes.entry("h264".into()).or_default() +=
                bytes * hist.h264 as u64 / tile_total;
        }

        // M3.6c: mode-dwell timeline.
        // The server_telemetry list is ordered by frame_seq; consecutive
        // entries' capture_done_ns gaps are the dwell intervals.
        let mut h264_dwell_us: u64 = 0;
        let mut tile_dwell_us: u64 = 0;
        let mut mode_switch_count: u32 = 0;
        let mut prev_ts: Option<u64> = None;
        let mut prev_mode: Option<&str> = None;
        for st in &r.server_telemetry {
            let ts_us = st.capture_done_ns / 1_000;
            if let (Some(pt), Some(pm)) = (prev_ts, prev_mode) {
                let dwell = ts_us.saturating_sub(pt);
                match pm {
                    "h264" => h264_dwell_us += dwell,
                    "tile" => tile_dwell_us += dwell,
                    _ => {}
                }
                if pm != st.mode.as_str() {
                    mode_switch_count += 1;
                }
            }
            prev_ts = Some(ts_us);
            prev_mode = Some(st.mode.as_str());
        }

        // M3.6c: mode.decision events per reason.
        let mut mode_decision_counts: std::collections::BTreeMap<String, u32> =
            std::collections::BTreeMap::new();
        for d in &r.mode_decisions {
            *mode_decision_counts.entry(d.reason.clone()).or_default() += 1;
        }

        let pixelperfect_count = r.policy_metrics.pixelperfect_count;
        let decode_error_count = r.policy_metrics.decode_error_count;

        self.scenes.insert(
            name.into(),
            SceneSummary {
                server_interval_min_ns: percentile_u64(&server_intervals_ns, 0.0),
                server_interval_p10_ns: percentile_u64(&server_intervals_ns, 10.0),
                server_interval_median_ns: percentile_u64(&server_intervals_ns, 50.0),
                client_interval_min_ms: percentile_f64(&client_intervals_ms, 0.0),
                client_interval_p10_ms: percentile_f64(&client_intervals_ms, 10.0),
                client_interval_median_ms: percentile_f64(&client_intervals_ms, 50.0),
                joined_frame_count: joined,
                drop_rate_percent: drop_pct,
                wire_mbps,
                cpu_mean_percent: cpu_mean,
                cpu_peak_percent: cpu_peak,
                rss_max_mb: rss_max_kb as f64 / 1024.0,
                vmhwm_max_mb: vmhwm_max_kb as f64 / 1024.0,
                codec_wire_bytes: codec_bytes,
                h264_dwell_us,
                tile_dwell_us,
                mode_switch_count,
                mode_decision_counts,
                pixelperfect_count,
                decode_error_count,
                sweep_axis_label: None,
            },
        );
    }
}

fn percentile_u64(v: &[u64], pct: f64) -> u64 {
    if v.is_empty() {
        return 0;
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let idx = ((pct / 100.0) * (s.len() - 1) as f64).round() as usize;
    s[idx.min(s.len() - 1)]
}

fn percentile_f64(v: &[f64], pct: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((pct / 100.0) * (s.len() - 1) as f64).round() as usize;
    s[idx.min(s.len() - 1)]
}

fn compute_cpu(
    samples: &[ghostframe_e2e::harness::ProcSample],
    clk_tck: f64,
) -> (f64, f64) {
    if samples.len() < 2 {
        return (0.0, 0.0);
    }
    let mut pct_per_window = Vec::new();
    for w in samples.windows(2) {
        let dwall_ns = w[1].wall_ns.saturating_sub(w[0].wall_ns) as f64;
        if dwall_ns <= 0.0 {
            continue;
        }
        let dwall_s = dwall_ns / 1e9;
        let dticks = (w[1].utime_ticks + w[1].stime_ticks)
            .saturating_sub(w[0].utime_ticks + w[0].stime_ticks) as f64;
        let cpu_seconds = dticks / clk_tck;
        let percent = (cpu_seconds / dwall_s) * 100.0;
        pct_per_window.push(percent);
    }
    let mean = if pct_per_window.is_empty() {
        0.0
    } else {
        pct_per_window.iter().sum::<f64>() / pct_per_window.len() as f64
    };
    let peak = pct_per_window.iter().cloned().fold(0.0f64, f64::max);
    (mean, peak)
}
