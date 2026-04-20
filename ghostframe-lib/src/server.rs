//! High-level server API: `FrameSubmission` and `GhostframeServer`.
//!
//! `GhostframeServer` wraps an `IoBridge` event loop and exposes a
//! `submit_frame` channel for pushing captured frames into the pipeline.

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
    /// Capture timestamp in microseconds.
    pub timestamp_us: u32,
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
    _io_task: tokio::task::JoinHandle<()>,
}

impl GhostframeServer {
    /// Create a new server.
    ///
    /// - Connects to ghostbridge using `config`.
    /// - Binds the QUIC/WebTransport listener on `listen_addr` (e.g. `":4443"`).
    /// - Spawns the `IoBridge` event loop as a background tokio task.
    /// - Returns a `GhostframeServer` with a frame submission channel
    ///   (capacity 2).
    pub async fn new(
        config: GhostbridgeConfig,
        listen_addr: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (frame_tx, frame_rx) = mpsc::channel::<FrameSubmission>(2);

        let mut bridge = IoBridge::new_with_frames(&config, listen_addr, frame_rx).await?;
        let cert_hash = bridge.cert_hash_sha256().to_owned();

        let io_task = tokio::spawn(async move {
            if let Err(e) = bridge.run().await {
                tracing::error!(error = %e, "IoBridge event loop exited with error");
            }
        });

        Ok(Self {
            frame_tx,
            cert_hash,
            _io_task: io_task,
        })
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
            timestamp_us: 0,
        };
        assert_eq!(sub.width, 1920);
        assert_eq!(sub.pixels.len(), 1920 * 1080 * 4);
    }
}
