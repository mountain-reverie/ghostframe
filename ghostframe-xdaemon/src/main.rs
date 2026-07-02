mod drm_capture;
mod input_inject;
mod x11_capture;
mod xdamage;

use std::env;
use std::path::Path;
use std::time::{Duration, Instant};

use ghostframe_lib::{FrameSubmission, GhostbridgeConfig, GhostframeServer};

/// Capture backend selection.
enum CaptureBackend {
    /// DRM/KMS capture — passes DMA-BUF fd directly for zero-copy GPU pipeline.
    Drm,
    /// X11 GetImage fallback (for containers without DRM).
    X11 {
        capture: Box<x11_capture::X11Capture>,
    },
}

/// Returns true if the tsnet state directory has been seeded with a successful
/// login. tsnet writes `tailscaled.state` after the first successful join; we
/// use its presence as the "has been --init'd" proxy. If this returns false,
/// the daemon needs a TS_AUTHKEY (either via env or via `--init`).
fn state_dir_seeded(state_dir: &Path) -> bool {
    state_dir.join("tailscaled.state").exists()
}

/// Block until `$DISPLAY` accepts an X11 connection, or the timeout expires.
///
/// systemd's `After=ghostframe-wm.service` only guarantees enlightenment_start
/// was launched, not that Xorg finished initialising. Without this gate the
/// daemon would happily bring up tsnet (a ~5s operation) against a missing or
/// crashed X server, and the only symptom downstream would be "no frames" —
/// the actual failure (Xwrapper denial, vt conflict, etc.) lives only in
/// ghostframe-xorg.service's journal.
fn wait_for_x11(timeout: Duration) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut attempt: u32 = 0;
    let last_err = loop {
        attempt += 1;
        let err = match x11rb::connect(None) {
            Ok(_) => {
                if attempt > 1 {
                    tracing::info!(attempts = attempt, "X11 display ready");
                }
                return Ok(());
            }
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            break err;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let display = env::var("DISPLAY").unwrap_or_else(|_| "(unset)".into());
    Err(format!(
        "X11 display {display} not ready after {:?} ({attempt} attempt(s)); \
         last error: {last_err}",
        timeout
    ))
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let log_format = std::env::var("GHOSTFRAME_LOG_FORMAT").unwrap_or_else(|_| "text".into());
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ghostframe=debug,info".into());

    match log_format.as_str() {
        "json" => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .init();
        }
        _ => {
            tracing_subscriber::fmt().with_env_filter(env_filter).init();
        }
    }

    let init_mode = env::args().any(|a| a == "--init");

    let hostname = env::var("TS_HOSTNAME").unwrap_or_else(|_| "ghostframe-server".into());
    let state_dir = env::var("TS_STATE_DIR").unwrap_or_else(|_| "/tmp/ghostframe-ts".into());
    let state_dir_path = std::path::PathBuf::from(&state_dir);
    let seeded = state_dir_seeded(&state_dir_path);

    let authkey = match (init_mode, env::var("TS_AUTHKEY").ok(), seeded) {
        (true, Some(k), _) if !k.is_empty() => k,
        (true, _, _) => {
            eprintln!("error: --init requires TS_AUTHKEY to be set and non-empty");
            std::process::exit(2);
        }
        (false, Some(k), _) if !k.is_empty() => k,
        (false, _, true) => String::new(),
        (false, _, false) => {
            eprintln!(
                "error: TS_AUTHKEY not set and state dir {state_dir:?} has not been initialised.\n\
                 Run once with TS_AUTHKEY set and the --init flag to seed the state dir, e.g.\n\
                 \n\
                 \tTS_AUTHKEY=tskey-auth-... TS_STATE_DIR={state_dir:?} ghostframe-xdaemon --init\n\
                 \n\
                 (packaging/install.sh does this for you during setup.)"
            );
            std::process::exit(2);
        }
    };
    let control_url = env::var("TS_CONTROL_URL").unwrap_or_default();
    let capture_fps: u64 = env::var("CAPTURE_FPS")
        .unwrap_or_else(|_| "30".into())
        .parse()
        .expect("CAPTURE_FPS must be a positive integer");

    let config = GhostbridgeConfig {
        hostname,
        authkey,
        state_dir,
        control_url,
    };

    // Skip in --init mode: that path only seeds the tsnet state and exits
    // without capturing anything, and install.sh runs --init before X is up.
    //
    // Also skip when GHOSTFRAME_NO_X11_WAIT is set — the DRM-direct e2e
    // path runs ghostframe-test-pattern as the DRM master with no Xorg
    // anywhere in the container, so blocking on a non-existent display :99
    // would just time out after 10 s and abort startup. The DRM-direct
    // entrypoint sets this env var; production never does.
    let skip_x11_wait = std::env::var("GHOSTFRAME_NO_X11_WAIT")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some();
    if !init_mode && !skip_x11_wait {
        if let Err(msg) = wait_for_x11(Duration::from_secs(10)) {
            tracing::error!(
                "{msg}\nCheck the upstream service: \
                 `systemctl --user status ghostframe-xorg.service`"
            );
            std::process::exit(2);
        }
    }

    // X server is up; safe to open the XTest injector connection. If the
    // XTEST extension isn't compiled into this xorg-server build, log
    // and continue — frames still stream, just no input.
    let input_injector: Option<
        std::sync::Arc<dyn ghostframe_lib::transport::input_inject::InputInjector>,
    > = if !init_mode {
        match input_inject::XTestInjector::new() {
            Ok(inj) => {
                tracing::info!("XTest input injector ready");
                Some(std::sync::Arc::new(inj))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "XTest injector unavailable; input forwarding disabled"
                );
                None
            }
        }
    } else {
        // --init mode exits before serving traffic; no point opening
        // the injector connection.
        None
    };

    tracing::info!("Connecting to Tailscale...");
    let server = match GhostframeServer::new(config, ":443", input_injector).await {
        Ok(s) => s,
        Err(e) => {
            if let Some(ws) = e.downcast_ref::<ghostframe_lib::WebServerError>() {
                if matches!(ws, ghostframe_lib::WebServerError::HttpsCertsDisabled) {
                    tracing::error!(
                        "HTTPS Certificates are not enabled for this tailnet. \
                         Enable them at https://login.tailscale.com/admin/dns and restart."
                    );
                    std::process::exit(2);
                }
            }
            return Err(e);
        }
    };

    if init_mode {
        tracing::info!(
            state_dir = %state_dir_path.display(),
            "tsnet state dir seeded successfully; exiting (--init)"
        );
        return Ok(());
    }

    // Select capture backend. Default: try DRM first, fall back to X11. With
    // GHOSTFRAME_X11_CAPTURE_ONLY=1, skip the DRM probe entirely — the
    // modesetting-FB fallback inside `drm_capture::capture()` would read
    // /dev/dri/card0's active scanout (the host's real display) rather than
    // the guest Xorg on :1, producing solid-black or host-screen captures
    // for the guest-mode install. See packaging/systemd/
    // ghostframe-xdaemon.service which sets the env var by default.
    let force_x11 = std::env::var("GHOSTFRAME_X11_CAPTURE_ONLY")
        .ok()
        .filter(|v| !v.is_empty() && v != "0")
        .is_some();
    let mut backend = if force_x11 {
        tracing::info!("GHOSTFRAME_X11_CAPTURE_ONLY set; skipping DRM probe");
        let capture = x11_capture::X11Capture::new()?;
        CaptureBackend::X11 {
            capture: Box::new(capture),
        }
    } else {
        match drm_capture::capture() {
            Ok(result) => {
                let (path_label, w, h) = match &result {
                    drm_capture::CaptureResult::Prime(_, g, _) => {
                        ("zero-copy GPU path (PRIME DMA-BUF)", g.width, g.height)
                    }
                    drm_capture::CaptureResult::Pixels(_, g, _) => {
                        ("CPU-mmap path (modesetting FB)", g.width, g.height)
                    }
                };
                drop(result);
                tracing::info!(
                    width = w,
                    height = h,
                    "DRM capture available ({})",
                    path_label
                );
                CaptureBackend::Drm
            }
            Err(e) => {
                tracing::warn!("DRM capture unavailable: {e}");
                tracing::info!("Falling back to X11 capture");
                let capture = x11_capture::X11Capture::new()?;
                CaptureBackend::X11 {
                    capture: Box::new(capture),
                }
            }
        }
    };

    let xdamage_monitor = xdamage::XDamageMonitor::new();
    if xdamage_monitor.is_some() {
        tracing::info!("XDamage monitoring active");
    } else {
        tracing::info!("XDamage not available, using full-frame dirty detection");
    }

    tracing::info!("Server ready, entering capture loop");

    let frame_interval = Duration::from_micros(1_000_000 / capture_fps);
    // Sleep granularity while idle. 500 ms keeps the daemon responsive (≤ that
    // much delay before the first frame after a client connects) without
    // burning ~25% CPU on X11 GetImage scrapes nobody consumes.
    let idle_poll_interval = Duration::from_millis(500);
    let mut frame_count = 0u64;
    // Tracks whether the loop is currently in idle (no-client) mode, so we
    // emit a single info log on each idle↔active flip instead of every
    // iteration. The IoBridge logs the corresponding event on its side; this
    // log shows the capture loop's view (X11 GetImage suspended/resumed).
    let mut was_idle = true;

    // Optional capture-dump diagnostic: when set, write each captured frame
    // as a PNG to this directory (filename: frame_<count>.png). Useful for
    // visually verifying the server's view of the desktop alongside trace
    // logs.  Disabled when the env var is absent.
    let capture_dump_dir = env::var("GHOSTFRAME_CAPTURE_DUMP_DIR")
        .ok()
        .filter(|s| !s.is_empty())
        .map(std::path::PathBuf::from);
    if let Some(dir) = &capture_dump_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(?dir, error = %e, "could not create capture-dump dir; PNG dump disabled");
        } else {
            tracing::info!(?dir, "capture-dump enabled — writing frame PNGs");
        }
    }

    // Heartbeat cadence — emit one debug line every Nth iteration so we can
    // tell from logs whether the capture loop is alive, blocked on
    // submit_frame (= IoBridge backpressure), or has exited.
    let heartbeat_every: u64 = env::var("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);

    loop {
        let loop_iter_start = std::time::Instant::now();
        // Skip the capture scrape entirely when nobody is connected. Without
        // this gate, X11 GetImage copies a full 1920x1080 framebuffer through
        // the X server every 33 ms regardless of demand.
        if server.connected_session_count() == 0 {
            if !was_idle {
                tracing::info!("no client connected; capture loop entering idle poll");
                was_idle = true;
            }
            tokio::time::sleep(idle_poll_interval).await;
            continue;
        }
        if was_idle {
            tracing::info!("client connected; capture loop resuming");
            was_idle = false;
        }

        let capture_start = std::time::Instant::now();

        // XDamage drain: Some(non-empty) = only check these tiles, None = check all.
        // An empty drain means no damage events since last call — but we still need
        // to fall back to full-frame comparison (None) so the dirty tracker can detect
        // the first frame for a newly connected client.
        let damage_tiles = xdamage_monitor.as_ref().and_then(|m| {
            let tiles = m.drain_damage();
            if tiles.is_empty() {
                None
            } else {
                Some(tiles)
            }
        });

        let submission = match &mut backend {
            CaptureBackend::Drm => match drm_capture::capture() {
                Ok(drm_capture::CaptureResult::Prime(dmabuf_fd, geom, capture_done_ns)) => {
                    let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;
                    Some(FrameSubmission {
                        width: geom.width,
                        height: geom.height,
                        stride: geom.stride,
                        pixels: Vec::new(),
                        dmabuf_fd: Some(dmabuf_fd),
                        timestamp_us,
                        damage_tiles: damage_tiles.clone(),
                        capture_done_ns,
                    })
                }
                Ok(drm_capture::CaptureResult::Pixels(pixels, geom, capture_done_ns)) => {
                    let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;
                    Some(FrameSubmission {
                        width: geom.width,
                        height: geom.height,
                        stride: geom.stride,
                        pixels,
                        dmabuf_fd: None,
                        timestamp_us,
                        damage_tiles: damage_tiles.clone(),
                        capture_done_ns,
                    })
                }
                Err(e) => {
                    tracing::warn!("DRM capture failed: {e}");
                    None
                }
            },
            CaptureBackend::X11 { capture } => {
                let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;
                match capture.capture(timestamp_us) {
                    Ok(mut frame) => {
                        frame.damage_tiles = damage_tiles.clone();
                        Some(frame)
                    }
                    Err(e) => {
                        tracing::warn!("X11 capture failed: {e}");
                        None
                    }
                }
            }
        };

        let capture_done = std::time::Instant::now();
        let capture_us = capture_done.duration_since(capture_start).as_micros() as u64;

        if let Some(frame) = submission {
            let frame_w = frame.width;
            let frame_h = frame.height;
            let frame_stride = frame.stride;
            let frame_has_dmabuf = frame.dmabuf_fd.is_some();
            let frame_pixels_len = frame.pixels.len();

            // Optional PNG dump of the captured frame so we can SEE what the
            // daemon is feeding into the encoder pipeline (gated on
            // GHOSTFRAME_CAPTURE_DUMP_DIR).  Runs BEFORE submit so we still
            // get a file even if submit blocks/fails.
            if let Some(dump_dir) = &capture_dump_dir {
                if !frame.pixels.is_empty() {
                    dump_frame_as_png(dump_dir, frame_count, &frame);
                }
            }

            let submit_start = std::time::Instant::now();
            let submit_res = server.submit_frame(frame).await;
            let submit_us = submit_start.elapsed().as_micros() as u64;
            if let Err(e) = submit_res {
                tracing::warn!(
                    frames = frame_count,
                    "submit_frame returned Err — channel closed, capture loop exiting: {e}"
                );
                break;
            }
            frame_count += 1;
            if frame_count == 1 {
                tracing::info!("first frame submitted");
            } else if frame_count.is_multiple_of(30) {
                // Bumped from 300 → 30 so e2e runs (~10-20s wall) see at
                // least one milestone.  Stays infrequent enough not to
                // dominate prod logs.
                tracing::info!(frames = frame_count, "capture running");
            }
            // Per-iter heartbeat — trace level so prod and the default e2e
            // log volume stay quiet (RUST_LOG defaults `ghostframe=debug,info`
            // and the e2e harness sets `ghostframe=trace,debug` which still
            // hits trace for ghostframe targets when explicitly debugging).
            // Captures the four timing components separately so we can spot
            // exactly which one stalls when the daemon goes silent.
            if frame_count.is_multiple_of(heartbeat_every) {
                tracing::trace!(
                    target: "ghostframe::xdaemon::capture",
                    frames = frame_count,
                    w = frame_w,
                    h = frame_h,
                    stride = frame_stride,
                    has_dmabuf = frame_has_dmabuf,
                    pixels_len = frame_pixels_len,
                    capture_us = capture_us,
                    submit_us = submit_us,
                    "[CAP] iter done"
                );
            }
        } else {
            tracing::trace!(
                target: "ghostframe::xdaemon::capture",
                frames = frame_count,
                capture_us = capture_us,
                "[CAP] capture returned None (no submission this iter)"
            );
        }

        let elapsed = loop_iter_start.elapsed();
        if elapsed < frame_interval {
            tokio::time::sleep(frame_interval - elapsed).await;
        }
    }

    tracing::warn!(frames = frame_count, "capture loop exited");
    Ok(())
}

/// Write a captured BGRA frame to `<dir>/frame_<count>.png`.  Best-effort:
/// logs and continues on any error so a failed dump never disturbs the live
/// capture path.  Gated by `GHOSTFRAME_CAPTURE_DUMP_DIR` env var.
fn dump_frame_as_png(
    dump_dir: &std::path::Path,
    count: u64,
    frame: &ghostframe_lib::server::FrameSubmission,
) {
    let path = dump_dir.join(format!("frame_{count:05}.png"));
    let w = frame.width;
    let h = frame.height;
    let stride = frame.stride as usize;
    let bgra = &frame.pixels;
    // Allocate tightly-packed RGBA8 (image crate accepts no stride param).
    let row_bytes = (w as usize).saturating_mul(4);
    if stride == 0 || bgra.len() < (h as usize).saturating_mul(stride.max(row_bytes)) {
        tracing::warn!(
            ?path,
            w,
            h,
            stride,
            len = bgra.len(),
            "pixel buffer too small for frame dims; skipping PNG dump"
        );
        return;
    }
    let mut rgba = vec![0u8; (h as usize).saturating_mul(row_bytes)];
    for y in 0..h as usize {
        let src_off = y.saturating_mul(stride);
        let dst_off = y.saturating_mul(row_bytes);
        for x in 0..w as usize {
            let s = src_off + x * 4;
            let d = dst_off + x * 4;
            // Wire format is BGRA; PNG expects RGBA. Swap B↔R and force
            // alpha=255 so the dump renders opaque even when X11's X-channel
            // happens to be zero.
            rgba[d] = bgra[s + 2];
            rgba[d + 1] = bgra[s + 1];
            rgba[d + 2] = bgra[s];
            rgba[d + 3] = 0xff;
        }
    }
    match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
        Ok(()) => {
            tracing::debug!(target: "ghostframe::xdaemon::capture", ?path, count, "[CAP-PNG] wrote frame")
        }
        Err(e) => tracing::warn!(?path, error = %e, "failed to write capture PNG"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ghostframe-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn state_dir_seeded_returns_false_for_empty_dir() {
        let d = temp_dir();
        assert!(!state_dir_seeded(&d));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn state_dir_seeded_returns_false_for_missing_dir() {
        let d = temp_dir().join("does-not-exist");
        assert!(!state_dir_seeded(&d));
    }

    #[test]
    fn state_dir_seeded_returns_true_when_tailscaled_state_present() {
        let d = temp_dir();
        fs::write(d.join("tailscaled.state"), b"{}").unwrap();
        assert!(state_dir_seeded(&d));
        fs::remove_dir_all(&d).ok();
    }
}
