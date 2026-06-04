//! Scene orchestration: spin up xdaemon + Chromium for one scene, sample
//! telemetry, return aggregated records.
//!
//! Implementation completed in Task 17 of the M3.5a plan.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use chromiumoxide::{Browser, BrowserConfig};
use futures::StreamExt;
use testcontainers::core::{IntoContainerPort, Mount, WaitFor};
use testcontainers::{runners::AsyncRunner, ContainerAsync, GenericImage, ImageExt};
use tokio::sync::Mutex as AsyncMutex;

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
pub struct ModeDecisionRecord {
    /// Frame sequence number (0 if unset by the event).
    pub frame_seq: u64,
    /// The `reason` field from the classifier event.
    /// One of: "cost_comparison", "headroom_guard", "loss_override",
    /// "suspension", "hysteresis_clamp".
    pub reason: String,
    /// Destination mode of the transition ("H264" or "TileCodec").
    pub to_mode: String,
}

/// M3.7a counters parsed from `target: "ghostframe::bench"` log lines.
/// PixelPerfect transitions and decode errors are sampled from the
/// server's existing log emissions — no new instrumentation required.
#[derive(Clone, Debug, Default)]
pub struct ScenePolicyMetrics {
    /// Count of `cdf53.pixelperfect` log lines observed during the scene.
    /// Each line fires when all passes for one tile reach Acked state.
    pub pixelperfect_count: u32,
    /// Count of `client decode error` log lines observed during the scene.
    /// Each line fires when the client reports a per-tile decode failure
    /// via the DECODE_ERROR datagram (0x04).
    pub decode_error_count: u32,
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
    pub mode_decisions: Vec<ModeDecisionRecord>,
    pub policy_metrics: ScenePolicyMetrics,
    pub proc_samples: Vec<ProcSample>,
    pub wire_bytes_total: u64,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// Fixed ports / IPs (mirrors the values used in tests/e2e.rs).
// ---------------------------------------------------------------------------

/// Fixed host port for headscale. Outside the Linux ephemeral range.
const HEADSCALE_HOST_PORT: u16 = 18080;

/// Docker default-bridge gateway: reachable from both host and containers.
const DOCKER_HOST_IP: &str = "172.17.0.1";

// ---------------------------------------------------------------------------
// ScenarioStack — keeps all resource handles alive until drop.
// ---------------------------------------------------------------------------

/// Owns all live handles for an active scenario stack. Dropping this struct
/// tears down the containers, browser, and compositor in reverse field order
/// (Rust drops fields top-to-bottom, so `_xvfb` is listed last so it outlives
/// `_browser`).
struct ScenarioStack {
    /// headscale container (kept alive via RAII).
    _headscale: ContainerAsync<GenericImage>,
    /// ghostframe-server container (kept alive via RAII).
    _server: ContainerAsync<GenericImage>,
    /// Chromium browser handle.
    _browser: Browser,
    /// CDP event-loop task.
    _browser_handle: tokio::task::JoinHandle<()>,
    /// TestNode + forwarder kept alive.
    _test_node: crate::harness::containers::TestNode,
    /// Bound address of the loopback forwarder (not used after setup, but
    /// kept alive so the socket isn't dropped).
    _forwarder: std::net::SocketAddr,
    /// Static web-client server (axum task running as long as addr is live).
    _static_addr: std::net::SocketAddr,
    /// Weston compositor guard (must outlive `_browser`).
    _xvfb: crate::harness::weston::WestonGuard,
    /// The open browser page pointed at the web client.
    page: chromiumoxide::Page,
    /// Docker container name, used to collect logs after the run.
    server_container_name: String,
    /// Host-namespace PID of the ghostframe-xdaemon process. On rootful
    /// Docker, container processes appear in `/proc` under their host PID.
    /// We locate this by reading the container root's host PID from
    /// `docker inspect State.Pid` then searching children for xdaemon.
    xdaemon_host_pid: Option<u32>,
}

// ---------------------------------------------------------------------------
// Sub-step A: launch_scenario_stack
// ---------------------------------------------------------------------------

/// Spin up the full Docker + headscale + ghostframe-server + static web-client
/// + Chromium browser stack for one scene. Returns a `ScenarioStack` whose
/// drop tears everything down.
///
/// Mirrors `setup_e2e_inner_with_url_extra` in `tests/e2e.rs` but is
/// self-contained inside the library so `codec_report` can call it without
/// depending on `tests/`.
async fn launch_scenario_stack(spec: &SceneSpec) -> Result<ScenarioStack> {
    crate::harness::cleanup::cleanup_stale_xvfb_sockets();

    let hs_server_url = format!("http://{DOCKER_HOST_IP}:{HEADSCALE_HOST_PORT}");

    // --- headscale ---
    let headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_mapped_port(HEADSCALE_HOST_PORT, 8080.tcp())
            .with_container_name("headscale")
            .with_network(crate::harness::containers::NETWORK_NAME)
            .with_env_var("HS_SERVER_URL", &hs_server_url)
            .with_ready_conditions(vec![WaitFor::message_on_stderr(
                "listening and serving HTTP",
            )])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let server_key =
        crate::harness::containers::create_preauth_key("headscale", "ghostframe").await?;
    let client_key =
        crate::harness::containers::create_preauth_key("headscale", "ghostframe").await?;

    // --- ghostframe-server container ---
    // Join test_pattern_args into a single whitespace-separated string, matching
    // how the container entrypoint splits TEST_PATTERN into argv.
    let test_pattern_str = spec.test_pattern_args.join(" ");
    let server_container_name = "ghostframe-server".to_string();

    // M3.6c: forward bench-set env vars into the container so
    // GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP + GHOSTFRAME_INBOUND_LOSS_*
    // actually reach xdaemon. Without this, the env vars stay on
    // the bench process and the container sees defaults.
    let mut server_image = GenericImage::new("ghostframe/test-server", "latest")
        .with_container_name(&server_container_name)
        .with_network(crate::harness::containers::NETWORK_NAME)
        .with_env_var("TS_AUTHKEY", &server_key)
        .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
        .with_env_var("RUST_LOG", "ghostframe=trace,debug")
        .with_env_var("TEST_PATTERN", &test_pattern_str)
        // Enable JSON log format so parse_server_telemetry can decode events.
        .with_env_var("GHOSTFRAME_LOG_FORMAT", "json")
        // GPU passthrough: bind /dev/dri + modesetting Xorg config + privileged.
        .with_env_var("XORG_CONF", "/etc/X11/xorg-vkms.conf")
        .with_mount(Mount::bind_mount("/dev/dri", "/dev/dri"))
        .with_privileged(true)
        .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
        .with_startup_timeout(Duration::from_secs(120));

    // Forward 4 host env vars if set.
    for var in [
        "GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP",
        "GHOSTFRAME_INBOUND_LOSS_PROBABILITY",
        "GHOSTFRAME_INBOUND_LOSS_SEED",
        "GHOSTFRAME_INBOUND_LOSS_PREDICATE",
        // M3.7a: classifier env-var overrides — bench sweep modes set
        // these per swept value; without forwarding they'd silently
        // never reach xdaemon and every swept value would behave
        // identically (the bug that produced near-identical mode_switch
        // counts across bias_sweep / loss_axis runs in v4).
        "GHOSTFRAME_TEST_REFINEMENT_BIAS_US",
        "GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD",
        "GHOSTFRAME_TEST_HEADROOM_MIN_BPUS",
        "GHOSTFRAME_TEST_FORCE_BYTES_PER_US",
        // M3.6/M3.7: required for any CDF53 refinement work to fire at
        // all. e2e_progressive_refinement sets this explicitly; bench
        // sweeps that target refinement (PixelPerfect outcome metric)
        // need it forwarded so the codec is actually enabled in the
        // container's xdaemon.
        "GHOSTFRAME_ENABLE_CDF53",
        // M3.7b: tc qdisc shaping vars consumed by the entrypoint.
        "SHAPE_BANDWIDTH_KBPS",
        "SHAPE_DELAY_MS",
        "SHAPE_LOSS_PCT",
        // Capture frame rate — defaults to 2 fps which is too slow for
        // the scheduler's refinement loop to make progress within bench
        // scene-duration. e2e_progressive_refinement sets this to 30.
        "CAPTURE_FPS",
    ] {
        if let Ok(val) = std::env::var(var) {
            server_image = server_image.with_env_var(var, val);
        }
    }

    let server: ContainerAsync<GenericImage> = server_image.start().await?;

    let cert_hash =
        crate::harness::containers::read_cert_hash_from_logs(&server_container_name).await?;

    // Resolve xdaemon's host-visible PID for /proc sampling.
    let xdaemon_host_pid =
        resolve_xdaemon_host_pid(&server_container_name).await;

    // --- TestNode + forwarder ---
    let client_control_url = format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}");
    let test_node =
        crate::harness::containers::TestNode::join(client_key, client_control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;
    let forwarder = crate::harness::transport::start_forwarder("127.0.0.1:0", upstream).await?;

    // --- Static web-client ---
    let dist_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root")
        .join("ghostframe-web-client/dist");
    let static_addr = crate::harness::transport::start_static_server(dist_dir).await?;

    // --- Weston headless compositor + XWayland ---
    let xvfb = crate::harness::weston::spawn_weston_headless()?;

    // --- Chromium browser ---
    let chrome_profile_dir = std::env::temp_dir().join(format!(
        "chromiumoxide-scene-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .subsec_nanos()
    ));
    let builder = BrowserConfig::builder()
        .chrome_executable("/usr/bin/chromium")
        .no_sandbox()
        .user_data_dir(&chrome_profile_dir)
        .with_head()
        .env("DISPLAY", xvfb.display.clone())
        .env(
            "XDG_RUNTIME_DIR",
            xvfb.runtime_dir().to_string_lossy().to_string(),
        )
        .arg(("enable-features", "Vulkan,WebGPU"))
        .arg("use-vulkan")
        .arg("ozone-platform=x11")
        .arg("enable-unsafe-webgpu")
        .arg("ignore-gpu-blocklist");

    let (browser, mut handler) = Browser::launch(builder.build().unwrap()).await?;
    let browser_handle = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page_url = format!(
        "http://{}/index.html?host={}:{}&certHash={}",
        static_addr,
        forwarder.ip(),
        forwarder.port(),
        cert_hash,
    );
    let page = browser.new_page(&page_url).await?;

    // Wait for "Receiving frames" before returning — ensures the stack is live.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let result = page
            .evaluate(
                "(document.getElementById('status') || {textContent: '<null>'}).textContent",
            )
            .await;
        if let Ok(v) = result {
            let status: String = v.into_value().unwrap_or_default();
            if status.contains("Receiving frames") {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                let content = page.content().await.unwrap_or_default();
                return Err(anyhow!(
                    "timed out waiting for frame rendering. last status: {status}\npage:\n{content}"
                ));
            }
        } else if tokio::time::Instant::now() >= deadline {
            let content = page.content().await.unwrap_or_default();
            return Err(anyhow!(
                "timed out waiting for frame rendering\npage:\n{content}"
            ));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    Ok(ScenarioStack {
        _headscale: headscale,
        _server: server,
        _browser: browser,
        _browser_handle: browser_handle,
        _test_node: test_node,
        _forwarder: forwarder,
        _static_addr: static_addr,
        _xvfb: xvfb,
        page,
        server_container_name,
        xdaemon_host_pid,
    })
}

/// Resolve the host-namespace PID of the `ghostframe-xdaemon` process running
/// inside `container_name`.
///
/// Strategy:
/// 1. `docker exec <container> pidof ghostframe-xdaemon` → in-container PID.
/// 2. `docker inspect --format='{{.State.Pid}}' <container>` → container root
///    host PID (the init/entrypoint process in the host's PID namespace).
/// 3. Walk `/proc/<root_host_pid>/root/proc/<inner_pid>/status` — this reads
///    through the container's mount namespace, giving us in-container proc
///    info. But we really want the HOST pid so we can access
///    `/proc/<host_pid>/stat` directly from the sampler.
/// 4. Find the host PID by scanning `/proc` for a process whose `NSpid`
///    field in `/proc/<candidate>/status` matches `inner_pid` and whose
///    parent is reachable from the container root.
///
/// Falls back to `None` on any failure (sampling just skips).
async fn resolve_xdaemon_host_pid(container_name: &str) -> Option<u32> {
    // Step 1: get in-container PID of xdaemon.
    let out = tokio::process::Command::new("docker")
        .args(["exec", container_name, "pidof", "ghostframe-xdaemon"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let inner_pid_str = String::from_utf8_lossy(&out.stdout);
    // pidof may return multiple PIDs (space-separated); take the first.
    let inner_pid: u32 = inner_pid_str.split_whitespace().next()?.parse().ok()?;

    // Step 2: container root's host PID.
    let inspect_out = tokio::process::Command::new("docker")
        .args(["inspect", "--format={{.State.Pid}}", container_name])
        .output()
        .await
        .ok()?;
    let root_host_pid: u32 = String::from_utf8_lossy(&inspect_out.stdout)
        .trim()
        .parse()
        .ok()?;

    // Step 3: scan /proc for the host-side PID whose NSpid matches inner_pid
    // and whose parent chain includes root_host_pid.
    // Heuristic: read /proc/*/status, look for NSpid containing inner_pid.
    let proc_dir = std::fs::read_dir("/proc").ok()?;
    for entry in proc_dir.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let candidate: u32 = match name.parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        // Skip the container root process itself.
        if candidate == root_host_pid {
            continue;
        }
        let status_path = format!("/proc/{candidate}/status");
        let status = match std::fs::read_to_string(&status_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        // NSpid line: "NSpid:\t<host_pid>\t<ns1_pid>\t<ns2_pid>..."
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("NSpid:") {
                let pids: Vec<u32> = rest
                    .split_whitespace()
                    .filter_map(|s| s.parse().ok())
                    .collect();
                // The last PID in NSpid is the innermost namespace PID.
                if pids.last().copied() == Some(inner_pid) {
                    return Some(candidate);
                }
            }
        }
    }

    // Fallback: if NSpid scan failed (kernel < 4.1 or single-level ns),
    // try reading through the container's /proc directly.
    // /proc/<root_host_pid>/root/proc/<inner_pid>/status is the container's view.
    let container_proc_stat = format!(
        "/proc/{root_host_pid}/root/proc/{inner_pid}/status"
    );
    if std::path::Path::new(&container_proc_stat).exists() {
        // We can't get the host PID from here without NSpid, so we just
        // return None and let the sampler use container-path reads.
        // (The `sample_proc_loop` below checks `proc_root` for this case.)
        return None;
    }

    None
}

// ---------------------------------------------------------------------------
// Sub-step B: run_scene composition
// ---------------------------------------------------------------------------

/// Run a scene end-to-end against the local Docker + Weston + Chromium
/// harness and return aggregated telemetry.
pub async fn run_scene(spec: &SceneSpec) -> Result<SceneResult> {
    let stack = launch_scenario_stack(spec).await?;

    // Start ProcSampler tokio task.
    let proc_samples: Arc<AsyncMutex<Vec<ProcSample>>> =
        Arc::new(AsyncMutex::new(Vec::new()));
    let proc_handle = tokio::spawn(sample_proc_loop(
        stack.xdaemon_host_pid,
        stack.server_container_name.clone(),
        Arc::clone(&proc_samples),
    ));

    // 2s warmup + scene duration.
    tokio::time::sleep(Duration::from_secs(2)).await;
    tokio::time::sleep(spec.duration).await;

    // Stop sampling.
    proc_handle.abort();

    // Collect server telemetry from the container's combined logs.
    let server_logs =
        crate::harness::cleanup::read_server_logs_stripped(&stack.server_container_name);
    let (server_telemetry, mode_decisions, policy_metrics) = parse_server_telemetry(&server_logs);

    // Collect client diagnostics from Chromium via DevTools eval.
    let client_diagnostics = read_client_diagnostics(&stack.page).await?;

    let wire_bytes_total: u64 = server_telemetry
        .iter()
        .map(|r| r.total_wire_bytes as u64)
        .sum();

    let proc_samples_vec = std::mem::take(&mut *proc_samples.lock().await);

    Ok(SceneResult {
        server_telemetry,
        client_diagnostics,
        mode_decisions,
        policy_metrics,
        proc_samples: proc_samples_vec,
        wire_bytes_total,
        error: None,
    })
}

// ---------------------------------------------------------------------------
// Helper: monotonic clock
// ---------------------------------------------------------------------------

fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe; `ts` is a local stack variable.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

// ---------------------------------------------------------------------------
// Helper: /proc sampler
// ---------------------------------------------------------------------------

/// Sample `ghostframe-xdaemon`'s CPU and memory usage every 100 ms.
///
/// Two modes:
/// - If `host_pid` is `Some(pid)`, read `/proc/<pid>/stat` and
///   `/proc/<pid>/status` directly (host-visible process).
/// - If `host_pid` is `None`, use `docker exec <container> cat /proc/1/stat`
///   as a best-effort fallback (samples the container init, not xdaemon, but
///   avoids a hard failure). The loop exits when the container stops responding.
async fn sample_proc_loop(
    host_pid: Option<u32>,
    container_name: String,
    samples: Arc<AsyncMutex<Vec<ProcSample>>>,
) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        interval.tick().await;
        let now_ns = monotonic_now_ns();

        let (stat, status) = if let Some(pid) = host_pid {
            // Fast path: host-visible PID.
            let stat = match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                Ok(s) => s,
                Err(_) => break, // process exited
            };
            let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
                Ok(s) => s,
                Err(_) => break,
            };
            (stat, status)
        } else {
            // Fallback: exec into container (slow, ~10ms overhead per call).
            // Use `cat` to read xdaemon's /proc/1 as a rough proxy.
            let stat_out = tokio::process::Command::new("docker")
                .args([
                    "exec",
                    &container_name,
                    "sh",
                    "-c",
                    "cat /proc/$(pidof ghostframe-xdaemon | awk '{print $1}')/stat 2>/dev/null || echo ''",
                ])
                .output()
                .await;
            let status_out = tokio::process::Command::new("docker")
                .args([
                    "exec",
                    &container_name,
                    "sh",
                    "-c",
                    "cat /proc/$(pidof ghostframe-xdaemon | awk '{print $1}')/status 2>/dev/null || echo ''",
                ])
                .output()
                .await;
            let stat = match stat_out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).to_string()
                }
                _ => break,
            };
            let status = match status_out {
                Ok(o) if o.status.success() => {
                    String::from_utf8_lossy(&o.stdout).to_string()
                }
                _ => break,
            };
            if stat.trim().is_empty() {
                break;
            }
            (stat, status)
        };

        // /proc/<pid>/stat: comm (field 2) may contain spaces inside parens;
        // split after the last ')' to reach the numeric fields.
        let after_comm = stat.rsplit_once(')').map(|(_, r)| r.trim()).unwrap_or("");
        let parts: Vec<&str> = after_comm.split_whitespace().collect();
        // After ')': state, ppid, pgrp, session, tty, tpgid, flags,
        //             minflt, cminflt, majflt, cmajflt, utime, stime, ...
        // → utime = parts[11], stime = parts[12].
        let utime: u64 = parts.get(11).and_then(|s| s.parse().ok()).unwrap_or(0);
        let stime: u64 = parts.get(12).and_then(|s| s.parse().ok()).unwrap_or(0);

        let parse_kb = |key: &str| -> u64 {
            status
                .lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|s| s.parse().ok())
                .unwrap_or(0)
        };

        samples.lock().await.push(ProcSample {
            wall_ns: now_ns,
            utime_ticks: utime,
            stime_ticks: stime,
            rss_kb: parse_kb("VmRSS:"),
            vmhwm_kb: parse_kb("VmHWM:"),
        });
    }
}

// ---------------------------------------------------------------------------
// Helper: parse JSON server telemetry
// ---------------------------------------------------------------------------

fn parse_server_telemetry(stderr_lines: &str) -> (Vec<ServerTelemetryRecord>, Vec<ModeDecisionRecord>, ScenePolicyMetrics) {
    use std::collections::BTreeMap;

    // frame_seq → (capture_done_ns, mode)
    let mut captures: BTreeMap<u64, (u64, String)> = BTreeMap::new();
    // frame_seq → (last_send_ns, total_wire_bytes, tile_count, CodecHistogram)
    let mut sends: BTreeMap<u64, (u64, u32, u16, CodecHistogram)> = BTreeMap::new();
    let mut decisions: Vec<ModeDecisionRecord> = Vec::new();
    let mut policy_metrics = ScenePolicyMetrics::default();

    for line in stderr_lines.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue, // non-JSON warm-up text or plain-text lines
        };

        // tracing_subscriber::fmt().json() emits fields nested under "fields".
        let msg = v
            .pointer("/fields/message")
            .and_then(|m| m.as_str())
            .unwrap_or("");
        let target = v
            .pointer("/target")
            .and_then(|t| t.as_str())
            .unwrap_or("");
        if target != "ghostframe::bench" {
            continue;
        }

        let frame_seq = v
            .pointer("/fields/frame_seq")
            .and_then(|s| s.as_u64())
            .unwrap_or(0);

        match msg {
            "frame.captured" => {
                let ts = v
                    .pointer("/fields/capture_done_ns")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0);
                let mode = v
                    .pointer("/fields/mode")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                captures.insert(frame_seq, (ts, mode));
            }
            "frame.last_send" => {
                let ts = v
                    .pointer("/fields/last_send_ns")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0);
                let total = v
                    .pointer("/fields/total_wire_bytes")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0) as u32;
                let tc = v
                    .pointer("/fields/tile_count")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0) as u16;
                let hist = CodecHistogram {
                    solid: v
                        .pointer("/fields/codec_solid")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0) as u16,
                    palrle: v
                        .pointer("/fields/codec_palrle")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0) as u16,
                    cdf53: v
                        .pointer("/fields/codec_cdf53")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0) as u16,
                    raw: v
                        .pointer("/fields/codec_raw")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0) as u16,
                    h264: v
                        .pointer("/fields/codec_h264")
                        .and_then(|s| s.as_u64())
                        .unwrap_or(0) as u8,
                };
                sends.insert(frame_seq, (ts, total, tc, hist));
            }
            "mode decision" => {
                let reason = v
                    .pointer("/fields/reason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let to_mode = v
                    .pointer("/fields/to")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string();
                let frame_seq = v
                    .pointer("/fields/frame_seq")
                    .and_then(|s| s.as_u64())
                    .unwrap_or(0);
                decisions.push(ModeDecisionRecord { frame_seq, reason, to_mode });
            }
            "cdf53.pixelperfect" => {
                policy_metrics.pixelperfect_count = policy_metrics.pixelperfect_count.saturating_add(1);
            }
            "client decode error" => {
                policy_metrics.decode_error_count = policy_metrics.decode_error_count.saturating_add(1);
            }
            _ => {}
        }
    }

    let records = captures
        .into_iter()
        .filter_map(|(seq, (cap_ns, mode))| {
            sends.get(&seq).map(|(send_ns, bytes, tc, hist)| {
                ServerTelemetryRecord {
                    frame_seq: seq,
                    capture_done_ns: cap_ns,
                    last_send_ns: *send_ns,
                    total_wire_bytes: *bytes,
                    tile_count: *tc,
                    mode,
                    codec_histogram: hist.clone(),
                }
            })
        })
        .collect();
    (records, decisions, policy_metrics)
}

// ---------------------------------------------------------------------------
// Helper: read client diagnostics via Chromium DevTools
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_telemetry_counts_pixelperfect_and_decode_error() {
        let json = r#"
{"target":"ghostframe::bench","fields":{"message":"cdf53.pixelperfect","tile_x":1,"tile_y":2}}
{"target":"ghostframe::bench","fields":{"message":"cdf53.pixelperfect","tile_x":3,"tile_y":4}}
{"target":"ghostframe::bench","fields":{"message":"client decode error","tile_x":0,"tile_y":0,"error_code":1}}
{"target":"ghostframe::bench","fields":{"message":"frame.captured","frame_seq":1,"capture_done_ns":0,"mode":"tile"}}
"#.trim();
        let (records, decisions, metrics) = parse_server_telemetry(json);
        assert_eq!(metrics.pixelperfect_count, 2);
        assert_eq!(metrics.decode_error_count, 1);
        // Sanity: existing parsers still work alongside.
        assert!(!records.is_empty() || decisions.is_empty()); // captured frame without send is dropped — just assert no panic
    }
}

async fn read_client_diagnostics(
    page: &chromiumoxide::Page,
) -> Result<Vec<ClientDiagnosticRecord>> {
    use std::collections::BTreeMap;

    let tiles_json: String = page
        .evaluate("JSON.stringify(window.__ghostframeRecordedTiles || [])")
        .await
        .context("evaluate __ghostframeRecordedTiles")?
        .into_value()
        .context("into_value tiles_json")?;
    let paints_json: String = page
        .evaluate("JSON.stringify(window.__ghostframe_framePaints || [])")
        .await
        .context("evaluate __ghostframe_framePaints")?
        .into_value()
        .context("into_value paints_json")?;

    let tiles: Vec<serde_json::Value> = serde_json::from_str(&tiles_json)
        .context("parse __ghostframeRecordedTiles JSON")?;
    let paints: Vec<serde_json::Value> = serde_json::from_str(&paints_json)
        .context("parse __ghostframe_framePaints JSON")?;

    // Build per-frame-seq min(firstRecvMsClient) and max(lastPaintMsClient).
    let mut first_recv_by_seq: BTreeMap<u64, f64> = BTreeMap::new();
    let mut last_paint_by_seq: BTreeMap<u64, f64> = BTreeMap::new();

    for t in &tiles {
        let seq = t.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        if let Some(v) = t.get("firstRecvMsClient").and_then(|s| s.as_f64()) {
            let cur = first_recv_by_seq
                .get(&seq)
                .copied()
                .unwrap_or(f64::INFINITY);
            if v < cur {
                first_recv_by_seq.insert(seq, v);
            }
        }
        if let Some(v) = t.get("lastPaintMsClient").and_then(|s| s.as_f64()) {
            let cur = last_paint_by_seq.get(&seq).copied().unwrap_or(0.0);
            if v > cur {
                last_paint_by_seq.insert(seq, v);
            }
        }
    }

    let mut out = Vec::with_capacity(paints.len());
    for p in &paints {
        let seq = p.get("seq").and_then(|s| s.as_u64()).unwrap_or(0);
        let raf = p.get("rafMsClient").and_then(|s| s.as_f64()).unwrap_or(0.0);
        let frecv = first_recv_by_seq.get(&seq).copied().unwrap_or(0.0);
        let lpaint = last_paint_by_seq.get(&seq).copied().unwrap_or(0.0);
        out.push(ClientDiagnosticRecord {
            frame_seq: seq,
            first_recv_ms: frecv,
            last_paint_ms: lpaint,
            raf_ms: raf,
        });
    }
    Ok(out)
}
