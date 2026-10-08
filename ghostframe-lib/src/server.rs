//! High-level server API: `FrameSubmission` and `GhostframeServer`.
//!
//! `GhostframeServer` wraps an `IoBridge` event loop and exposes a
//! `submit_frame` channel for pushing captured frames into the pipeline.

use std::os::unix::io::OwnedFd;
use std::sync::Arc;
use tokio::sync::{mpsc, watch};

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

// ── ServerShutDown ───────────────────────────────────────────────────────────

/// The `IoBridge` event loop has exited, so the connected-session count will
/// never change again.
///
/// Returned by [`GhostframeServer::wait_for_client`]. Terminal: a caller
/// looping on it should break out, not retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ServerShutDown;

impl std::fmt::Display for ServerShutDown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ghostframe server event loop has shut down")
    }
}

impl std::error::Error for ServerShutDown {}

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
    /// Published by the `IoBridge` event loop; non-zero means at least one
    /// WebTransport session is currently connected. Read by the capture loop
    /// to avoid scraping frames nobody will consume, either by polling
    /// [`connected_session_count`] or by parking on [`wait_for_client`].
    ///
    /// [`connected_session_count`]: GhostframeServer::connected_session_count
    /// [`wait_for_client`]: GhostframeServer::wait_for_client
    connected_session_count: watch::Receiver<usize>,
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
    ///
    /// `lib_config` is passed straight through to `IoBridge`: this is the
    /// executable boundary. `GhostframeServer` itself never reads the
    /// process environment; callers (the `ghostframe-xdaemon` binary and the
    /// C FFI layer) build it once via `crate::config::LibConfig::from_env()`
    /// and hand it in here.
    pub async fn new(
        config: GhostbridgeConfig,
        listen_addr: &str,
        lib_config: crate::config::LibConfig,
        input_injector: Option<Arc<dyn crate::transport::input_inject::InputInjector>>,
        display_controller: Option<Arc<dyn crate::transport::display::DisplayController>>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (frame_tx, frame_rx) = mpsc::channel::<FrameSubmission>(2);

        let mut bridge =
            IoBridge::new_with_frames(&config, listen_addr, frame_rx, lib_config).await?;
        bridge.input_injector = input_injector;
        bridge.display_controller = display_controller;
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
    /// frame and on connect/disconnect events. Non-blocking; prefer
    /// [`wait_for_client`] over polling this in a sleep loop, which puts the
    /// whole poll interval between a client connecting and the first frame.
    ///
    /// [`wait_for_client`]: GhostframeServer::wait_for_client
    pub fn connected_session_count(&self) -> usize {
        *self.connected_session_count.borrow()
    }

    /// Resolve as soon as at least one WebTransport session is connected,
    /// returning how many are. Returns immediately if one already is.
    ///
    /// This is the gate a capture loop should park on while idle: it wakes on
    /// the connect event itself, so the first frame is scraped without the
    /// extra latency a poll interval would add. Only an actual change to the
    /// count wakes a waiter, so the `IoBridge`'s per-frame refresh does not.
    ///
    /// `Err(ServerShutDown)` means the `IoBridge` event loop has exited and
    /// no client can ever arrive — a terminal condition for the caller's
    /// loop, not something to retry.
    pub async fn wait_for_client(&self) -> Result<usize, ServerShutDown> {
        wait_for_nonzero(&self.connected_session_count).await
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

/// Resolve once `rx` holds a non-zero count, returning it.
///
/// Split out from [`GhostframeServer::wait_for_client`] so the parking
/// behaviour is unit-testable against a bare `watch` channel — constructing a
/// `GhostframeServer` needs a live tsnet node.
async fn wait_for_nonzero(rx: &watch::Receiver<usize>) -> Result<usize, ServerShutDown> {
    // Clone so we have an owned receiver whose version we may advance; the
    // caller's is never advanced, which keeps `connected_session_count()` a
    // plain non-blocking read. `borrow_and_update` reads the value and marks
    // that version seen in one step, so `changed()` below waits for something
    // strictly newer and a change landing between the two cannot be missed.
    let mut rx = rx.clone();
    loop {
        let connected = *rx.borrow_and_update();
        if connected > 0 {
            return Ok(connected);
        }
        rx.changed().await.map_err(|_| ServerShutDown)?;
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

    // ── wait_for_nonzero: the capture-loop idle gate ─────────────────────

    /// A client that is already connected must not cost a park at all —
    /// otherwise every capture iteration after the first would wait for the
    /// *next* connect event, which may never come.
    #[tokio::test]
    async fn wait_for_nonzero_returns_immediately_when_already_connected() {
        let (_tx, rx) = watch::channel(3usize);
        assert_eq!(wait_for_nonzero(&rx).await, Ok(3));
    }

    /// The point of the channel: the wait resolves on the publish itself.
    ///
    /// Driven by hand with a no-op waker rather than a runtime, so the two
    /// polls pin down exactly what a poll-interval implementation got wrong:
    /// pending while the count is 0, ready on the very next poll after it
    /// changes — no timer in between, nothing to tune.
    #[test]
    fn wait_for_nonzero_is_pending_until_the_count_changes() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let (tx, rx) = watch::channel(0usize);
        let mut fut = std::pin::pin!(wait_for_nonzero(&rx));
        let mut cx = Context::from_waker(Waker::noop());

        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
            "must park while no client is connected, not resolve on 0"
        );

        tx.send_replace(1);

        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(1))),
            "must be ready on the first poll after the count changes — any \
             delay here lands in full between a client connecting and the \
             first frame"
        );
    }

    /// A count that changes but stays 0 (e.g. a handshake that never
    /// completed) must not be mistaken for a client.
    #[tokio::test]
    async fn wait_for_nonzero_ignores_changes_that_stay_at_zero() {
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let (tx, rx) = watch::channel(1usize);
        // Drop to 0, then churn without ever reaching a connected client.
        tx.send_replace(0);
        let mut fut = std::pin::pin!(wait_for_nonzero(&rx));
        let mut cx = Context::from_waker(Waker::noop());
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Pending));

        tx.send_replace(0);
        assert!(
            matches!(fut.as_mut().poll(&mut cx), Poll::Pending),
            "0 -> 0 is not a client arriving"
        );

        tx.send_replace(2);
        assert!(matches!(fut.as_mut().poll(&mut cx), Poll::Ready(Ok(2))));
    }

    /// With the sender gone the count can never change again, so a waiter
    /// must be told rather than parked forever. This is what lets the capture
    /// loop exit when the IoBridge event loop dies — it cannot learn that
    /// from `submit_frame`, which it never reaches while the gate is shut.
    #[tokio::test]
    async fn wait_for_nonzero_reports_shutdown_when_the_sender_is_dropped() {
        let (tx, rx) = watch::channel(0usize);
        drop(tx);
        assert_eq!(wait_for_nonzero(&rx).await, Err(ServerShutDown));
    }

    /// A sender dropped *while* a waiter is parked must wake it, not leave it
    /// parked on a channel nobody can ever write to again.
    #[tokio::test]
    async fn wait_for_nonzero_wakes_on_shutdown_mid_park() {
        let (tx, rx) = watch::channel(0usize);
        let waiter = tokio::spawn(async move { wait_for_nonzero(&rx).await });
        tokio::task::yield_now().await;
        drop(tx);
        assert_eq!(
            waiter.await.expect("waiter task panicked"),
            Err(ServerShutDown)
        );
    }
}
