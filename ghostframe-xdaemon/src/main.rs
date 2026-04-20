mod drm_capture;
mod x11_capture;

use std::env;
use std::os::unix::io::AsRawFd;
use std::time::Duration;

use ghostframe_lib::capture::dmabuf::{readback_dmabuf, VulkanReadback};
use ghostframe_lib::{FrameSubmission, GhostbridgeConfig, GhostframeServer};

/// Capture backend selection.
enum CaptureBackend {
    /// DRM/KMS capture with Vulkan or mmap readback.
    Drm { vk: Option<Box<VulkanReadback>> },
    /// X11 GetImage fallback (for containers without DRM).
    X11 { capture: Box<x11_capture::X11Capture> },
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ghostframe=debug,info".into()),
        )
        .init();

    let authkey = env::var("TS_AUTHKEY").expect("TS_AUTHKEY must be set");
    let hostname = env::var("TS_HOSTNAME").unwrap_or_else(|_| "ghostframe-server".into());
    let state_dir = env::var("TS_STATE_DIR").unwrap_or_else(|_| "/tmp/ghostframe-ts".into());
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
    let server = GhostframeServer::new(config, ":4443").await?;

    // Machine-parseable line for the E2E test harness. Use println! (stdout)
    // rather than tracing so the format stays stable regardless of log config.
    println!("CERT_HASH_SHA256={}", server.cert_hash());

    // Select capture backend: try DRM first, fall back to X11.
    let backend = match drm_capture::capture_prime_fd() {
        Ok((fd, geom)) => {
            drop(fd);
            tracing::info!(
                width = geom.width,
                height = geom.height,
                "DRM capture available"
            );
            let vk = match VulkanReadback::new() {
                Ok(vk) => {
                    tracing::info!(device = %vk.device_name(), "Vulkan readback initialized");
                    Some(Box::new(vk))
                }
                Err(e) => {
                    tracing::warn!("Vulkan init failed, using mmap readback: {e}");
                    None
                }
            };
            CaptureBackend::Drm { vk }
        }
        Err(e) => {
            tracing::warn!("DRM capture unavailable: {e}");
            tracing::info!("Falling back to X11 capture");
            let capture = x11_capture::X11Capture::new()?;
            CaptureBackend::X11 { capture: Box::new(capture) }
        }
    };

    tracing::info!("Server ready, entering capture loop");

    let frame_interval = Duration::from_micros(1_000_000 / capture_fps);
    let mut frame_count = 0u64;

    loop {
        let capture_start = std::time::Instant::now();

        let submission = match &backend {
            CaptureBackend::Drm { vk } => match drm_capture::capture_prime_fd() {
                Ok((dmabuf_fd, geom)) => {
                    let pixels = if let Some(vk) = vk {
                        vk.readback(
                            dmabuf_fd.as_raw_fd(),
                            geom.width,
                            geom.height,
                            geom.stride,
                        )
                        .map_err(|e| std::io::Error::other(e.to_string()))?
                    } else {
                        readback_dmabuf(
                            dmabuf_fd.as_raw_fd(),
                            geom.width,
                            geom.height,
                            geom.stride,
                        )?
                    };
                    let timestamp_us =
                        (frame_count * frame_interval.as_micros() as u64) as u32;
                    Some(FrameSubmission {
                        width: geom.width,
                        height: geom.height,
                        stride: geom.stride,
                        pixels,
                        timestamp_us,
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
                    Ok(frame) => Some(frame),
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
