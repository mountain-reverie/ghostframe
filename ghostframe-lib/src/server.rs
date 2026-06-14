//! High-level server API: `FrameSubmission` and `GhostframeServer`.
//!
//! `GhostframeServer` wraps an `IoBridge` event loop and exposes a
//! `submit_frame` channel for pushing captured frames into the pipeline.

use std::os::unix::io::OwnedFd;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;

use crate::transport::ghostbridge::GhostbridgeConfig;
use crate::transport::io_bridge::IoBridge;

// ── FrameSubmission ──────────────────────────────────────────────────────────

/// A single captured video frame ready for submission to the server.
pub struct FrameSubmission {
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Bytes per row (may include padding beyond `width * 4`).
    pub stride: u32,
    /// BGRA pixel data; length must equal `stride * height`.
    pub pixels: Vec<u8>,
    /// DMA-BUF file descriptor for zero-copy GPU access.
    /// When present, the GPU pipeline uses this directly instead of `pixels`.
    pub dmabuf_fd: Option<OwnedFd>,
    /// Capture timestamp in microseconds.
    pub timestamp_us: u32,
    /// Optional damage hints as tile coordinates. If `None`, all tiles are checked.
    pub damage_tiles: Option<Vec<(u32, u32)>>,
    /// Monotonic-clock timestamp (CLOCK_MONOTONIC nanoseconds) at the
    /// moment capture finished. Used by M3.5 Layer B bench to compute
    /// capture→send server-side latency interval.
    /// Capture sites that lack a precise read timestamp set this to 0;
    /// the bench filters records with capture_done_ns == 0.
    pub capture_done_ns: u64,
}

// ── GhostframeServer ─────────────────────────────────────────────────────────

/// Wraps an `IoBridge` event loop and provides a frame submission channel.
///
/// Construct with [`GhostframeServer::new`], then call [`submit_frame`] to
/// push frames into the pipeline.
///
/// [`submit_frame`]: GhostframeServer::submit_frame
pub struct GhostframeServer {
    frame_tx: mpsc::Sender<FrameSubmission>,
    cert_hash: String,
    /// Shared with the `IoBridge` event loop; non-zero means at least one
    /// WebTransport session is currently connected. Polled by the capture loop
    /// to avoid scraping frames nobody will consume.
    connected_session_count: Arc<AtomicUsize>,
    _io_task: tokio::task::JoinHandle<()>,
}

impl GhostframeServer {
    /// Create a new server.
    ///
    /// - Connects to ghostbridge using `config`.
    /// - Binds the QUIC/WebTransport listener on `listen_addr` (e.g. `":443"`).
    /// - Spawns the `IoBridge` event loop as a background tokio task.
    /// - Returns a `GhostframeServer` with a frame submission channel
    ///   (capacity 2).
    pub async fn new(
        config: GhostbridgeConfig,
        listen_addr: &str,
        input_injector: Option<Arc<dyn crate::transport::input_inject::InputInjector>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (frame_tx, frame_rx) = mpsc::channel::<FrameSubmission>(2);

        let mut bridge = IoBridge::new_with_frames(&config, listen_addr, frame_rx).await?;
        bridge.input_injector = input_injector;
        let cert_hash = bridge.cert_hash_sha256().to_owned();
        let connected_session_count = bridge.connected_session_count_handle();

        // Start the tsnet :443 (HTTPS) + :80 (redirect) listeners. Failures
        // are fatal: a misconfigured tailnet or a port bind error means the
        // first-connection URL will not work. Surface them at startup
        // rather than at user-connect time.
        //
        // `ghostbridge()` returns None only on the test-only IoBridge path
        // (no real tsnet node); production callers always have a handle.
        if let Some(bridge_handle) = bridge.ghostbridge() {
            bridge_handle.start_web_server(&cert_hash)?;
            // Stable readiness signal: emitted only after the :80 + :443
            // listeners are bound on tsnet. The e2e harness greps for this
            // line; production operators see it as confirmation the daemon
            // is reachable.
            tracing::info!("ghostbridge web server listening on :80 + :443");
        }

        let io_task = tokio::spawn(async move {
            if let Err(e) = bridge.run().await {
                tracing::error!(error = %e, "IoBridge event loop exited with error");
            }
        });

        Ok(Self {
            frame_tx,
            cert_hash,
            connected_session_count,
            _io_task: io_task,
        })
    }

    /// Number of WebTransport sessions currently connected to the server.
    ///
    /// `0` means the capture loop can safely skip capture work — nobody will
    /// consume the frame. Refreshed by the `IoBridge` event loop on every
    /// frame and on connect/disconnect events. `Relaxed` ordering: a stale
    /// read costs at most one extra frame captured.
    pub fn connected_session_count(&self) -> usize {
        self.connected_session_count.load(Ordering::Relaxed)
    }

    /// Submit a frame to the pipeline.
    ///
    /// Returns `Err` if the channel is closed (i.e. the server has shut down).
    /// Blocks (asynchronously) if the channel buffer is full until space is
    /// available.
    pub async fn submit_frame(
        &self,
        frame: FrameSubmission,
    ) -> Result<(), mpsc::error::SendError<FrameSubmission>> {
        self.frame_tx.send(frame).await
    }

    /// Return the SHA-256 hex fingerprint of the server's TLS certificate.
    ///
    /// Pass this to the WebTransport client as the `serverCertificateHashes`
    /// value.
    pub fn cert_hash(&self) -> &str {
        &self.cert_hash
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_submission_basic() {
        let sub = FrameSubmission {
            width: 1920,
            height: 1080,
            stride: 1920 * 4,
            pixels: vec![0u8; 1920 * 1080 * 4],
            dmabuf_fd: None,
            timestamp_us: 0,
            damage_tiles: None,
            capture_done_ns: 0,
        };
        assert_eq!(sub.width, 1920);
        assert_eq!(sub.pixels.len(), 1920 * 1080 * 4);
    }
}
