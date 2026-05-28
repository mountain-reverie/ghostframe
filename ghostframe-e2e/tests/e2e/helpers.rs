use std::net::SocketAddr;
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use axum::Router;
use ghostframe_lib::framing::{encode_frame, parse_frame_rest};
use ghostframe_lib::transport::ghostbridge::{GhostbridgeConfig, GhostbridgeHandle, UdpPacketConn};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UdpSocket, UnixStream as TokioUnixStream};
use tower_http::services::ServeDir;

pub const NETWORK_NAME: &str = "ghostframe-e2e";

/// Create a headscale preauth key for the given user. Idempotent — if the
/// user already exists the `users create` error is ignored.
///
/// Headscale 0.28 requires `--user <numeric_id>` (not name) for preauthkeys.
/// So we create the user by name, then look up its numeric ID via `users list`,
/// and pass that ID to `preauthkeys create`.
pub async fn create_preauth_key(container_name: &str, user: &str) -> Result<String> {
    // Idempotent: ignore "user already exists" error.
    let _ = tokio::process::Command::new("docker")
        .args(["exec", container_name, "headscale", "users", "create", user])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await?;

    // Look up the numeric user ID — headscale 0.28 requires it for preauthkeys.
    let list_out = tokio::process::Command::new("docker")
        .args([
            "exec",
            container_name,
            "headscale",
            "users",
            "list",
            "--output",
            "json",
        ])
        .output()
        .await
        .context("running headscale users list")?;
    let users: serde_json::Value = serde_json::from_slice(&list_out.stdout)?;
    let user_id = users
        .as_array()
        .and_then(|arr| arr.iter().find(|u| u["name"].as_str() == Some(user)))
        .and_then(|u| u["id"].as_u64())
        .ok_or_else(|| anyhow!("user {user} not found in headscale users list"))?;

    let out = tokio::process::Command::new("docker")
        .args([
            "exec",
            container_name,
            "headscale",
            "preauthkeys",
            "create",
            "--user",
            &user_id.to_string(),
            "--reusable",
            "--expiration",
            "1h",
            "--output",
            "json",
        ])
        .output()
        .await
        .context("running headscale preauthkeys create")?;
    if !out.status.success() {
        return Err(anyhow!(
            "preauthkeys create failed: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    v.get("key")
        .and_then(|k| k.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            anyhow!(
                "no `key` field in preauthkey JSON: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
}

/// Tail `docker logs -f <container>` until a line starting with
/// `CERT_HASH_SHA256=` is seen; returns the hash hex string. Timeout: 60s.
pub async fn read_cert_hash_from_logs(container_name: &str) -> Result<String> {
    let mut child = tokio::process::Command::new("docker")
        .args(["logs", "-f", container_name])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let line = tokio::time::timeout(remaining, lines.next_line()).await??;
        let Some(line) = line else { break };
        if let Some(rest) = line.strip_prefix("CERT_HASH_SHA256=") {
            let hash = rest.trim().to_string();
            child.kill().await.ok();
            return Ok(hash);
        }
    }
    Err(anyhow!("cert hash not seen in container logs within 60s"))
}

/// A tsnet node running inside the test process. Joins the test headscale
/// tailnet via ghostbridge so the test can dial the server container directly
/// over the tailnet.
pub struct TestNode {
    handle: GhostbridgeHandle,
}

impl TestNode {
    pub async fn join(authkey: String, control_url: String) -> Result<Self> {
        let handle = GhostbridgeHandle::connect(&GhostbridgeConfig {
            hostname: "e2e-test-client".into(),
            authkey,
            state_dir: format!("/tmp/ghostframe-e2e-client-{}", std::process::id()),
            control_url,
        })?;
        handle.up()?;
        Ok(Self { handle })
    }

    pub fn dial(&self, remote: &str) -> Result<UdpPacketConn> {
        Ok(self.handle.dial_udp(remote)?)
    }
}

/// Proxies between a local loopback UDP socket (what Chromium sees) and a
/// `UdpPacketConn` that forwards packets over tsnet to the server container.
///
/// Returns the bound `SocketAddr` that the browser should connect to.
pub async fn start_forwarder(local_bind: &str, upstream: UdpPacketConn) -> Result<SocketAddr> {
    let sock = UdpSocket::bind(local_bind).await?;
    let local = sock.local_addr()?;

    // Wrap the upstream UdpPacketConn fd in tokio for async I/O.
    let raw_fd = upstream.into_raw_fd();
    let std_stream = unsafe { std::os::unix::net::UnixStream::from_raw_fd(raw_fd) };
    std_stream.set_nonblocking(true)?;
    let mut upstream = TokioUnixStream::from_std(std_stream)?;

    tokio::spawn(async move {
        let mut buf = vec![0u8; 65535];
        let mut header = [0u8; 8];
        let mut client: Option<SocketAddr> = None;

        loop {
            tokio::select! {
                // Browser → tsnet
                res = sock.recv_from(&mut buf) => {
                    let Ok((n, from)) = res else { return };
                    client = Some(from);
                    let frame = encode_frame(&buf[..n], &from);
                    if upstream.write_all(&frame).await.is_err() { return; }
                }
                // tsnet → browser
                res = upstream.read_exact(&mut header) => {
                    if res.is_err() { return }
                    let total_len = u32::from_be_bytes(header[0..4].try_into().unwrap()) as usize;
                    let payload_len = u32::from_be_bytes(header[4..8].try_into().unwrap()) as usize;
                    let mut rest = vec![0u8; total_len - 8];
                    if upstream.read_exact(&mut rest).await.is_err() { return }
                    let Ok(pkt) = parse_frame_rest(&rest, payload_len) else { continue };
                    if let Some(dst) = client {
                        let _ = sock.send_to(&pkt.payload, dst).await;
                    }
                }
            }
        }
    });

    Ok(local)
}

/// Serve a directory over HTTP on 127.0.0.1:<random>. Returns the bound address.
/// Used so Chromium loads the page from a secure-context origin (http://127.0.0.1),
/// which is required for WebTransport. `file://` origins are not a secure context.
pub async fn start_static_server(dir: impl AsRef<Path>) -> Result<SocketAddr> {
    let app = Router::new().fallback_service(ServeDir::new(dir.as_ref()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(addr)
}

/// Run `docker exec [-e <env>...] <container> <args...>` against an
/// already-running container, and return `(stdout, stderr, exit_code)`.
///
/// Uses `tokio::process::Command` (no shell — args are passed directly to
/// `execve`). Pass environment variables as `("KEY", "value")` tuples —
/// useful for invoking X11 client tools that need `DISPLAY=:99`.
pub async fn docker_run_in_container(
    container: &str,
    env: &[(&str, &str)],
    args: &[&str],
) -> Result<(String, String, i32)> {
    let mut cmd = tokio::process::Command::new("docker");
    cmd.arg("exec");
    for (k, v) in env {
        cmd.arg("-e").arg(format!("{k}={v}"));
    }
    cmd.arg(container).args(args);
    let out = cmd.output().await.context("running docker exec")?;
    Ok((
        String::from_utf8_lossy(&out.stdout).to_string(),
        String::from_utf8_lossy(&out.stderr).to_string(),
        out.status.code().unwrap_or(-1),
    ))
}

// ---------------------------------------------------------------------------
// Weston headless test display (with XWayland)
// ---------------------------------------------------------------------------
//
// Replaces the previous private-Xvfb harness. Chromium's WebGPU on Linux
// requires a DRI3-capable display server to expose the host's Mesa Vulkan
// ICD to Dawn; Xvfb does not provide DRI3, so Dawn fell back to SwiftShader
// (CPU rasterized). SwiftShader's aggressive cleanup tripped a Dawn
// Instance-refcount race on page.reload(), reliably losing the new page's
// device — see `e2e_palrle_session_reset` notes.
//
// Weston-headless + xwayland gives us:
//   - A Wayland compositor backed by GBM on the real GPU (DRI3 works).
//   - An auto-spawned XWayland server providing a regular DISPLAY for
//     Chromium's `--ozone-platform=x11`, with system Mesa Vulkan available.
//   - Robust device lifecycle across reloads (matches production behavior).

use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::Instant;

/// Owns a Weston child process + its private XDG_RUNTIME_DIR. Drop kills
/// the compositor and removes the runtime dir. Captures both the Wayland
/// socket name and the XWayland-provided DISPLAY string so callers can
/// pass either to spawned Chromium processes.
pub struct WestonGuard {
    child: Child,
    runtime_dir: PathBuf,
    /// Wayland socket name relative to `runtime_dir` (e.g., `ghostframe-wl-0`).
    pub wayland_display: String,
    /// XWayland DISPLAY string (e.g., `:1`). Picked dynamically by Weston.
    pub display: String,
    log_path: PathBuf,
}

impl WestonGuard {
    pub fn runtime_dir(&self) -> &Path {
        &self.runtime_dir
    }
}

impl Drop for WestonGuard {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.runtime_dir);
        let _ = std::fs::remove_file(&self.log_path);
    }
}

/// Spawn a private Weston compositor (headless backend, GL renderer,
/// xwayland enabled). Returns once both the Wayland socket and the X
/// display socket appear, or after a 15 s deadline.
///
/// The compositor runs against the host's real GPU via Mesa GBM, exposing
/// a usable DRI3 path to Chromium. A unique runtime dir keeps multiple
/// concurrent test runs from clobbering each other (in practice the suite
/// uses `--test-threads=1`, but the isolation is cheap).
pub fn spawn_weston_headless() -> Result<WestonGuard> {
    use std::os::unix::fs::PermissionsExt;

    let pid = std::process::id();
    let runtime_dir = std::env::temp_dir().join(format!(
        "ghostframe-weston-{}-{}",
        pid,
        Instant::now().elapsed().as_nanos(),
    ));
    std::fs::create_dir_all(&runtime_dir).context("creating weston runtime dir")?;
    std::fs::set_permissions(&runtime_dir, std::fs::Permissions::from_mode(0o700))
        .context("setting weston runtime dir permissions")?;

    // Pick a unique Wayland socket name to avoid colliding with any host
    // compositor or earlier test runs that leaked sockets.
    let socket_name = format!("ghostframe-wl-{pid}");
    let log_path = std::env::temp_dir().join(format!("ghostframe-weston-{pid}.log"));
    let log_file = std::fs::File::create(&log_path)
        .context("creating weston log file")?;
    let log_stderr = log_file.try_clone().context("cloning weston log fd")?;

    // The headless backend renders offscreen but still drives Mesa GBM,
    // giving Chromium access to the real Vulkan adapter through XWayland.
    // `--idle-time=0` disables the inactivity timeout (default 5 minutes
    // would otherwise blank the display mid-test).
    let child = Command::new("weston")
        .arg("--backend=headless")
        .arg("--renderer=gl")
        .arg("--width=1920")
        .arg("--height=1080")
        .arg(format!("--socket={socket_name}"))
        .arg("--idle-time=0")
        .arg("--xwayland")
        .env("XDG_RUNTIME_DIR", &runtime_dir)
        .stdout(log_file)
        .stderr(log_stderr)
        .spawn()
        .context("spawning weston (is the weston package installed?)")?;

    // Wait for the Wayland socket to appear inside the runtime dir.
    let wayland_socket = runtime_dir.join(&socket_name);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !wayland_socket.exists() {
        if Instant::now() > deadline {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(anyhow!(
                "weston wayland socket {wayland_socket:?} did not appear within 15s. log:\n{log}"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    // Wait for Weston's xwayland module to log the X display number, then
    // verify the X socket itself is bound. Format we look for:
    //   xserver listening on display :N
    let display = loop {
        if Instant::now() > deadline {
            let log = std::fs::read_to_string(&log_path).unwrap_or_default();
            return Err(anyhow!(
                "weston xwayland did not start within 15s. log:\n{log}"
            ));
        }
        let log = std::fs::read_to_string(&log_path).unwrap_or_default();
        if let Some(d) = parse_weston_xwayland_display(&log) {
            break d;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let x_socket = format!("/tmp/.X11-unix/X{}", &display[1..]);
    while !Path::new(&x_socket).exists() {
        if Instant::now() > deadline {
            return Err(anyhow!(
                "weston xwayland announced DISPLAY={display} but socket {x_socket} never appeared"
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    Ok(WestonGuard {
        child,
        runtime_dir,
        wayland_display: socket_name,
        display,
        log_path,
    })
}

/// Parse the X display string (`:N`) from Weston's `--xwayland` log line.
/// Weston 15 logs `xserver listening on display :N`.
fn parse_weston_xwayland_display(log: &str) -> Option<String> {
    for line in log.lines() {
        if let Some(rest) = line.split_once("listening on display ") {
            let candidate = rest.1.split_whitespace().next()?;
            if candidate.starts_with(':') {
                return Some(candidate.to_string());
            }
        }
    }
    None
}

/// Capture the page's canvas as a PNG (via chromiumoxide's `screenshot`
/// helper, which wraps CDP `Page.captureScreenshot`) and return it as an
/// `image::RgbaImage`. Used by the SSIM golden tests.
pub async fn screenshot_canvas(page: &chromiumoxide::Page) -> Result<image::RgbaImage> {
    use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
    use chromiumoxide::page::ScreenshotParams;

    let params = ScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .build();
    let png_bytes = page
        .screenshot(params)
        .await
        .context("page.screenshot failed")?;
    let img = image::load_from_memory(&png_bytes)
        .context("decode screenshot PNG")?
        .to_rgba8();
    Ok(img)
}

/// Compare a captured `RgbaImage` against a checked-in golden PNG on disk.
///
/// Bless mode: setting `GHOSTFRAME_BLESS_GOLDENS=1` writes the captured
/// image to `golden_path` (creating the parent directory if needed) and
/// returns `Ok(())`. Normal mode: loads the golden, computes hybrid SSIM
/// via `image-compare::rgba_hybrid_compare`, and fails if score < threshold.
pub fn assert_ssim_against_golden(
    captured: &image::RgbaImage,
    golden_path: &str,
    threshold: f64,
) -> Result<()> {
    if std::env::var("GHOSTFRAME_BLESS_GOLDENS").is_ok() {
        if let Some(parent) = std::path::Path::new(golden_path).parent() {
            std::fs::create_dir_all(parent).context("create golden parent dir")?;
        }
        captured
            .save(golden_path)
            .context("write blessed golden")?;
        eprintln!("BLESSED golden: {}", golden_path);
        return Ok(());
    }
    let golden = image::open(golden_path)
        .with_context(|| format!("open golden {golden_path}"))?
        .to_rgba8();
    if golden.dimensions() != captured.dimensions() {
        return Err(anyhow!(
            "captured dimensions {:?} != golden {:?} ({})",
            captured.dimensions(),
            golden.dimensions(),
            golden_path,
        ));
    }
    let score = image_compare::rgba_hybrid_compare(captured, &golden)
        .context("ssim compare failed")?;
    if score.score < threshold {
        return Err(anyhow!(
            "SSIM {:.4} < threshold {:.4} against {}",
            score.score,
            threshold,
            golden_path,
        ));
    }
    Ok(())
}

/// Read `docker logs <container>` (stdout + stderr concatenated) and strip
/// ANSI escape sequences so substring assertions are stable across TTY /
/// non-TTY contexts. Used by tests that assert on tracing-subscriber output.
pub fn read_server_logs_stripped(container_name: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["logs", container_name])
        .output()
        .expect("running docker logs");
    let raw = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
    raw.chars().fold((String::new(), false), |(mut acc, in_esc), c| {
        if in_esc {
            (acc, c != 'm')
        } else if c == '\x1b' {
            (acc, true)
        } else {
            acc.push(c);
            (acc, false)
        }
    }).0
}

/// Pre-test hygiene: remove stale `/tmp/.X11-unix/X<N>` socket files and
/// matching `/tmp/.X<N>-lock` lock files for `N in 0..=20`, but skip any
/// socket that's currently backed by a live X server (connect probe returns
/// Ok), plus leftover `/tmp/ghostframe-weston-*` runtime dirs and log files.
/// Without this, accumulated stale sockets from prior test runs eventually
/// exhaust the low display numbers XWayland picks from (it starts at :1).
///
/// Called once at the top of `setup_e2e_inner` so it runs once per test.
///
/// **Serialization assumption**: this helper assumes the project's
/// `--test-threads=1` convention — only one test setup runs at a
/// time. The cleanup → `spawn_weston_headless` sequence within a SINGLE
/// test is safe because both are synchronous.
pub fn cleanup_stale_xvfb_sockets() {
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    let mut removed = 0;
    // XWayland-launched-by-Weston picks low display numbers (typically :1).
    // Old Xvfb-based runs used :99..:199 — sweep both ranges so this works
    // during the transition and stays robust afterwards.
    let ranges: &[std::ops::RangeInclusive<u32>] = &[0..=20, 99..=199];
    for range in ranges {
        for n in range.clone() {
            let socket = format!("/tmp/.X11-unix/X{n}");
            let lock = format!("/tmp/.X{n}-lock");
            if !Path::new(&socket).exists() && !Path::new(&lock).exists() {
                continue;
            }
            match UnixStream::connect(&socket) {
                Ok(_) => continue, // live X server bound; keep
                Err(e) if matches!(
                    e.kind(),
                    std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                ) => {
                    let _ = std::fs::remove_file(&socket);
                    let _ = std::fs::remove_file(&lock);
                    removed += 1;
                }
                Err(_) => continue,
            }
        }
    }
    // Sweep stale Weston runtime dirs + log files belonging to PIDs that
    // are no longer alive. Cheap: stat /proc/<pid>.
    if let Ok(entries) = std::fs::read_dir("/tmp") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let pid_str = if let Some(rest) = name.strip_prefix("ghostframe-weston-") {
                rest.split('-').next().or(Some(rest.trim_end_matches(".log")))
            } else { None };
            let Some(pid_str) = pid_str else { continue };
            let Ok(pid) = pid_str.parse::<u32>() else { continue };
            if Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
            removed += 1;
        }
    }
    if removed > 0 {
        eprintln!("cleanup_stale_xvfb_sockets: removed {removed} stale entries");
    }
}
