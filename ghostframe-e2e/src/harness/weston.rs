use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};

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
///
/// Promoted to `pub` so `harness::mod` can re-export it.
pub fn parse_weston_xwayland_display(log: &str) -> Option<String> {
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
