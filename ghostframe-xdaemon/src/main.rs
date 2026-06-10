mod drm_capture;
mod x11_capture;
mod xdamage;

use std::env;
use std::path::Path;
use std::time::Duration;

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

    tracing::info!("Connecting to Tailscale...");
    let server = GhostframeServer::new(config, ":443").await?;

    // Machine-parseable line for the E2E test harness. Use println! (stdout)
    // rather than tracing so the format stays stable regardless of log config.
    println!("CERT_HASH_SHA256={}", server.cert_hash());

    if init_mode {
        tracing::info!(
            state_dir = %state_dir_path.display(),
            "tsnet state dir seeded successfully; exiting (--init)"
        );
        return Ok(());
    }

    // Select capture backend: try DRM first, fall back to X11.
    let backend = match drm_capture::capture() {
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
    };

    let xdamage_monitor = xdamage::XDamageMonitor::new();
    if xdamage_monitor.is_some() {
        tracing::info!("XDamage monitoring active");
    } else {
        tracing::info!("XDamage not available, using full-frame dirty detection");
    }

    tracing::info!("Server ready, entering capture loop");

    let frame_interval = Duration::from_micros(1_000_000 / capture_fps);
    let mut frame_count = 0u64;

    loop {
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

        let submission = match &backend {
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

        if let Some(frame) = submission {
            if let Err(e) = server.submit_frame(frame).await {
                tracing::warn!("frame submission failed: {e}");
                break;
            }
            frame_count += 1;
            if frame_count == 1 {
                tracing::info!("first frame submitted");
            } else if frame_count.is_multiple_of(300) {
                tracing::info!(frames = frame_count, "capture running");
            }
        }

        let elapsed = capture_start.elapsed();
        if elapsed < frame_interval {
            tokio::time::sleep(frame_interval - elapsed).await;
        }
    }

    Ok(())
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
