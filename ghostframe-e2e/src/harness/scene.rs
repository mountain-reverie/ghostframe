//! Scene orchestration: spin up xdaemon + Chromium for one scene, sample
//! telemetry, return aggregated records.
//!
//! Implementation completed in Task 17 of the M3.5a plan.

use std::time::Duration;

use anyhow::Result;

/// Frame-mode classifier output the scene starts in. Pre-classifier; the
/// actual mode emitted by the server is whatever the classifier decides
/// given the scene's content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameMode {
    H264,
    TileCodec,
}

#[derive(Clone, Debug)]
pub struct SceneSpec {
    pub name: &'static str,
    /// Args passed verbatim to `ghostframe-test-pattern` (e.g.
    /// `["--tile-pattern", "solid"]` or `["--mode-switch-cycle", "2s"]`).
    pub test_pattern_args: Vec<String>,
    /// Wall-clock duration the scene runs for (after a 2 s warmup ramp
    /// inside `run_scene`).
    pub duration: Duration,
    /// Hint for what the classifier should pick; not enforced.
    pub initial_mode: FrameMode,
}

#[derive(Clone, Debug)]
pub struct ServerTelemetryRecord {
    pub frame_seq: u64,
    pub capture_done_ns: u64,
    pub last_send_ns: u64,
    pub total_wire_bytes: u32,
    pub tile_count: u16,
    pub mode: String, // "h264" | "tile"
    pub codec_histogram: CodecHistogram,
}

#[derive(Clone, Debug, Default)]
pub struct CodecHistogram {
    pub solid: u16,
    pub palrle: u16,
    pub cdf53: u16,
    pub raw: u16,
    pub h264: u8,
}

#[derive(Clone, Debug)]
pub struct ClientDiagnosticRecord {
    pub frame_seq: u64,
    /// `performance.now()` at first-fragment arrival. DOMHighResTimeStamp,
    /// millisecond resolution (sub-ms precision). Not directly comparable
    /// to server-side `capture_done_ns`/`last_send_ns` (those are
    /// CLOCK_MONOTONIC nanoseconds) — use intervals on each side.
    pub first_recv_ms: f64,
    pub last_paint_ms: f64,
    pub raf_ms: f64,
}

#[derive(Clone, Debug)]
pub struct ProcSample {
    pub wall_ns: u64,
    pub utime_ticks: u64,
    pub stime_ticks: u64,
    pub rss_kb: u64,
    pub vmhwm_kb: u64,
}

#[derive(Debug, Default)]
pub struct SceneResult {
    pub server_telemetry: Vec<ServerTelemetryRecord>,
    pub client_diagnostics: Vec<ClientDiagnosticRecord>,
    pub proc_samples: Vec<ProcSample>,
    pub wire_bytes_total: u64,
    pub error: Option<String>,
}

/// Run a scene end-to-end against the local Docker + Weston + Chromium
/// harness and return aggregated telemetry. Implementation in Task 17.
pub async fn run_scene(_spec: &SceneSpec) -> Result<SceneResult> {
    unimplemented!("filled in Task 17 of M3.5a plan")
}
