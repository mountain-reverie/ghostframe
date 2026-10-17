//! The thread that runs a [`FrameBackend`], so a frame's GPU fence wait, H.264
//! encode and CDF 5/3 pass encoding no longer hold the transport event loop
//! (WS-C2, PS-2).
//!
//! # Shape
//!
//! One dedicated OS thread owns the backend (and so the Vulkan processor and
//! the hardware encoder) for its whole life. The loop talks to it through two
//! channels and has at most one frame in flight:
//!
//! ```text
//! loop                         worker thread
//! Analyse{frame,..}      -->   backend.analyse            (GPU submit + fence)
//!                        <--   Analysed | AnalyseFailed
//! (classify, decide)
//! Finish{H264|Tile|Discard} --> backend.finish_*          (encode / readback)
//!                        <--   H264Done | TileDone | Discarded
//! (commit, dispatch)
//! ```
//!
//! The worker keeps the frame between the two calls (readback and the NV12
//! encode need it) and hands its pixels back with `TileDone` when the tile
//! path reads from them. Controls (`invalidate_baseline`, `request_keyframe`)
//! ride along with the next message and are applied, in order, before it runs.
//!
//! A dedicated thread, not `spawn_blocking`: the Vulkan queue, command buffer
//! and fence and the VA-API context each have exactly one user, there is no
//! take-and-put-back window for the processor, and a fence wait stuck at
//! `u64::MAX` cannot hang the runtime's shutdown.
//!
//! # Hardware exclusion
//!
//! One bridge cannot use its encoder concurrently with itself. Two bridges in
//! one process (an embedder that replaces its server while the old worker is
//! mid-encode) could, and concurrent users of the VA-API encoder fault the
//! GPU on the host this runs on, so every call into a backend holds
//! [`FRAME_HW_LOCK`], which is process-wide. Uncontended with one bridge.
//!
//! # Failure
//!
//! A panic in the worker is fatal. The thread ends, the result channel
//! closes, and the loop's result arm returns an error from `IoBridge::run`,
//! the same blast radius as a panic in the old synchronous `process_frame`.
//! There is no silent fallback to the CPU path: it would hide a broken
//! backend behind degraded output.
//!
//! # Shutdown
//!
//! Dropping the [`FramePipeline`] closes the request channel; the worker
//! finishes the call it is in, drops the backend on its own thread (the Vulkan
//! device wait and the encoder free happen there) and signals. The drop waits
//! for that signal for at most [`SHUTDOWN_JOIN`], then gives up and leaves the
//! thread, so a hung fence cannot hang process exit.

use std::sync::mpsc as std_mpsc;
use std::sync::Mutex;
use std::time::Duration;

use tokio::sync::mpsc;

use super::frame_backend::{
    Analysed, Control, EscalationSlot, FrameBackend, H264Out, PixelSource, TileOut, TileRequest,
};
use crate::server::FrameSubmission;

/// Serialises every backend call in the process. See the module docs.
static FRAME_HW_LOCK: Mutex<()> = Mutex::new(());

/// How long the loop waits for a CDF 5/3 forward before failing the batch.
const FORWARD_TIMEOUT: Duration = Duration::from_secs(5);

/// How long dropping a [`FramePipeline`] waits for the worker to finish.
pub(super) const SHUTDOWN_JOIN: Duration = Duration::from_millis(1500);

/// What the loop wants done after it has classified a frame.
pub(super) enum FinishAction {
    H264 {
        force_keyframe: bool,
    },
    Tile {
        cols: u32,
        rows: u32,
        slots: Vec<EscalationSlot>,
        fallback: Vec<(u32, u32)>,
    },
    Discard,
}

pub(super) enum ToWorker {
    Analyse {
        seq: u32,
        frame: FrameSubmission,
        escalation: Vec<u32>,
        want_diag: bool,
        controls: Vec<Control>,
    },
    Finish {
        seq: u32,
        action: FinishAction,
        controls: Vec<Control>,
    },
    /// `FrameBackend::forward_cdf53_tiles`, for tiles the loop needs encoded
    /// while committing a frame. Answered on `reply`. The loop is single
    /// threaded and waits for the answer, so no frame step is outstanding.
    Forward {
        tiles: Vec<Vec<u8>>,
        reply: std_mpsc::Sender<Result<Vec<i16>, String>>,
    },
}

pub(super) enum FromWorker {
    /// The GPU stage succeeded.
    Analysed {
        seq: u32,
        analysed: Box<Analysed>,
        worker_us: u64,
    },
    /// The GPU stage failed. There is no software path, so the loop drops the
    /// frame.
    AnalyseFailed {
        seq: u32,
        error: String,
    },
    H264Done {
        seq: u32,
        out: Option<H264Out>,
        worker_us: u64,
    },
    TileDone {
        seq: u32,
        out: Box<TileOut>,
        /// The frame's own pixels, when `out.pixels` is `PixelSource::Frame`.
        frame_pixels: Vec<u8>,
        worker_us: u64,
    },
    Discarded {
        seq: u32,
    },
}

/// The loop's handle on the worker thread.
pub(super) struct FramePipeline {
    tx: Option<std_mpsc::Sender<ToWorker>>,
    pub(super) rx: mpsc::Receiver<FromWorker>,
    exited: std_mpsc::Receiver<()>,
    handle: Option<std::thread::JoinHandle<()>>,
    /// The panic message, if the worker died of one. Set before the result
    /// channel closes.
    failure: std::sync::Arc<Mutex<Option<String>>>,
    /// Blocking `Forward` requests made, for the trace and the tests: with
    /// the worker pre-encoding the fallback tiles it can know about, the common
    /// full-screen case makes none.
    forward_requests: std::sync::atomic::AtomicU64,
}

impl FramePipeline {
    pub(super) fn new(backend: Box<dyn FrameBackend>) -> Self {
        let (tx, worker_rx) = std_mpsc::channel::<ToWorker>();
        let (result_tx, rx) = mpsc::channel::<FromWorker>(2);
        let (exited_tx, exited) = std_mpsc::channel::<()>();
        let failure = std::sync::Arc::new(Mutex::new(None));
        let worker_failure = failure.clone();
        let handle = std::thread::Builder::new()
            .name("gf-frame-worker".into())
            .spawn(move || {
                let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_worker(backend, worker_rx, &result_tx)
                }));
                if let Err(payload) = ran {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "unknown panic".into());
                    *worker_failure.lock().unwrap_or_else(|e| e.into_inner()) = Some(msg);
                }
                // The result channel closes here, after the failure is recorded.
                drop(result_tx);
                let _ = exited_tx.send(());
            })
            .expect("spawn the frame worker thread");
        Self {
            tx: Some(tx),
            rx,
            exited,
            handle: Some(handle),
            failure,
            forward_requests: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// How many blocking `Forward` requests the loop has made.
    pub(super) fn forward_requests(&self) -> u64 {
        self.forward_requests
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Why the worker died, if it panicked.
    pub(super) fn failure(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// `false` when the worker has gone (it panicked or was told to stop).
    pub(super) fn send(&self, msg: ToWorker) -> bool {
        self.tx.as_ref().is_some_and(|tx| tx.send(msg).is_ok())
    }

    /// Forward-transform `tiles` on the worker and wait for the coefficients.
    ///
    /// Called from the loop while it commits a frame, for the CDF 5/3 fallback
    /// tiles the worker did not already encode with the frame's finish (a PalRle
    /// tile that lost its palette at commit; the always-fallback states are
    /// encoded by the worker in `FinishAction::Tile`, with no wait). The loop waits: the worker is idle
    /// then (the frame's own finish has returned and no other step can be
    /// outstanding), the call is one GPU round trip, and the commit cannot
    /// proceed without the result. No session event can be handled meanwhile,
    /// so a frame's epoch cannot change under it. Bounded, so a stuck GPU
    /// fails the batch (its tiles are retried) instead of freezing the loop.
    pub(super) fn forward_cdf53_tiles(&self, tiles: &[&[u8]]) -> Result<Vec<i16>, String> {
        self.forward_requests
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (reply, answer) = std_mpsc::channel();
        let sent = self.send(ToWorker::Forward {
            tiles: tiles.iter().map(|t| t.to_vec()).collect(),
            reply,
        });
        if !sent {
            return Err("the frame worker has stopped".into());
        }
        match answer.recv_timeout(FORWARD_TIMEOUT) {
            Ok(result) => result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                Err("the frame worker did not answer a CDF 5/3 forward in time".into())
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                Err("the frame worker stopped during a CDF 5/3 forward".into())
            }
        }
    }
}

impl Drop for FramePipeline {
    fn drop(&mut self) {
        self.tx = None;
        match self.exited.recv_timeout(SHUTDOWN_JOIN) {
            // Exited (cleanly, or by panic: the channel is then disconnected).
            Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                if let Some(h) = self.handle.take() {
                    let _ = h.join();
                }
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                tracing::error!(
                    target: "ghostframe_lib::transport::io_bridge",
                    timeout_ms = SHUTDOWN_JOIN.as_millis() as u64,
                    "frame worker did not stop in time (a GPU or encoder call is stuck); \
                     leaving the thread behind"
                );
            }
        }
    }
}

/// How long to wait for the hardware lock before saying so. A backend call
/// stuck inside the driver (a fence wait with no timeout) holds the lock
/// forever, and a replacement bridge in the same process would otherwise go
/// silent behind it.
const LOCK_WARN_EVERY: Duration = Duration::from_secs(5);

fn locked<T>(f: impl FnOnce() -> T) -> T {
    let started = std::time::Instant::now();
    let mut next_warning = LOCK_WARN_EVERY;
    // A panic while holding the lock poisons it; the worker that panicked is
    // already fatal, and another bridge's worker must still be able to run.
    let _guard = loop {
        match FRAME_HW_LOCK.try_lock() {
            Ok(g) => break g,
            Err(std::sync::TryLockError::Poisoned(e)) => break e.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                std::thread::sleep(Duration::from_millis(2));
                #[allow(
                    clippy::disallowed_methods,
                    reason = "real time spent waiting on a lock, for a log line"
                )]
                let waited = started.elapsed();
                if waited >= next_warning {
                    tracing::warn!(
                        target: "ghostframe_lib::transport::io_bridge",
                        waited_s = waited.as_secs(),
                        "frame worker is waiting for the process-wide hardware lock; \
                         another frame worker's GPU or encoder call has not returned"
                    );
                    next_warning += LOCK_WARN_EVERY;
                }
            }
        }
    };
    // The window the loop is NOT held for, for the measurement scenes.
    tracing::trace!(target: "ghostframe::io_bridge::diag", "[BRIDGE] worker call ENTER");
    let t0 = std::time::Instant::now();
    let out = f();
    tracing::trace!(
        target: "ghostframe::io_bridge::diag",
        dt_us = elapsed_us(t0),
        "[BRIDGE] worker call EXIT"
    );
    out
}

/// Real wall time, deliberately not `now_std()`: this is what the call cost the
/// worker, and the worker stamps nothing protocol-visible.
#[allow(
    clippy::disallowed_methods,
    reason = "measures real elapsed time for the stage trace; never a protocol deadline"
)]
fn elapsed_us(t0: std::time::Instant) -> u64 {
    t0.elapsed().as_micros() as u64
}

fn run_worker(
    mut backend: Box<dyn FrameBackend>,
    rx: std_mpsc::Receiver<ToWorker>,
    tx: &mpsc::Sender<FromWorker>,
) {
    // The frame of the call in flight, kept between `Analyse` and `Finish`.
    let mut held: Option<FrameSubmission> = None;
    while let Ok(msg) = rx.recv() {
        match msg {
            ToWorker::Analyse {
                seq,
                frame,
                escalation,
                want_diag,
                controls,
            } => {
                held = None;
                let t0 = std::time::Instant::now();
                let result = locked(|| {
                    for c in controls {
                        backend.apply(c);
                    }
                    backend.analyse(&frame, &escalation, want_diag)
                });
                let reply = match result {
                    Ok(analysed) => {
                        held = Some(frame);
                        FromWorker::Analysed {
                            seq,
                            analysed: Box::new(analysed),
                            worker_us: elapsed_us(t0),
                        }
                    }
                    Err(error) => FromWorker::AnalyseFailed { seq, error },
                };
                if tx.blocking_send(reply).is_err() {
                    return;
                }
            }
            ToWorker::Forward { tiles, reply } => {
                let refs: Vec<&[u8]> = tiles.iter().map(|t| t.as_slice()).collect();
                let result = locked(|| backend.forward_cdf53_tiles(&refs));
                let _ = reply.send(result);
            }
            ToWorker::Finish {
                seq,
                action,
                controls,
            } => {
                // A Finish without a held frame is a protocol bug in the loop.
                let mut frame = held
                    .take()
                    .expect("frame worker: Finish without a frame in flight");
                let t0 = std::time::Instant::now();
                let reply = locked(|| {
                    for c in controls {
                        backend.apply(c);
                    }
                    match action {
                        FinishAction::H264 { force_keyframe } => FromWorker::H264Done {
                            seq,
                            out: backend.finish_h264(&frame, force_keyframe),
                            worker_us: elapsed_us(t0),
                        },
                        FinishAction::Tile {
                            cols,
                            rows,
                            slots,
                            fallback,
                        } => {
                            let out = backend.finish_tile(
                                &frame,
                                &TileRequest {
                                    cols,
                                    rows,
                                    escalation: &slots,
                                    fallback: &fallback,
                                },
                            );
                            let frame_pixels = if matches!(out.pixels, Ok(PixelSource::Frame)) {
                                std::mem::take(&mut frame.pixels)
                            } else {
                                Vec::new()
                            };
                            FromWorker::TileDone {
                                seq,
                                out: Box::new(out),
                                frame_pixels,
                                worker_us: elapsed_us(t0),
                            }
                        }
                        FinishAction::Discard => {
                            backend.finish_discard();
                            FromWorker::Discarded { seq }
                        }
                    }
                });
                if tx.blocking_send(reply).is_err() {
                    return;
                }
            }
        }
    }
}
