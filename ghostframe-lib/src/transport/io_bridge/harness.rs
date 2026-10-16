//! Test and browserless-harness entry points on `IoBridge`: the `*_for_test`
//! constructors, the drop-plan setter, the `*_publish_handle` accessors that
//! clone the `Arc`s a caller reads back after moving the bridge into a task,
//! the injection drains and the scheduler peeks.
//!
//! Every item here keeps its own `#[cfg(any(test, feature =
//! "browserless-harness"))]` attribute; the module itself is not gated, so
//! the file compiles to an empty `impl` in a production build. The
//! bandwidth-estimator accessors (`bwe_snapshot`, `bwe_publish_handle`, ...)
//! live in `bwe_glue.rs` and the `set_tick_budget_floor_bytes` setter in
//! `pacing.rs`.
//!
//! Not here: the `cfg`-gated struct fields these methods read and write, which
//! stay on `IoBridge` in `io_bridge.rs`.

use super::*;

impl IoBridge {
    /// Test-only constructor that accepts a pre-built stream and server,
    /// bypassing the real ghostbridge connection. No tsnet node is held, so
    /// `_handle` is `None` and no `gbridge_close` is called on Drop.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    ///
    /// Defaults to `LibConfig::default()` — every transport/diagnostics
    /// knob production-inert — so existing call sites are unaffected. Tests
    /// that need a specific config should call
    /// `new_with_lib_config_for_test` instead.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_stream_for_test(stream: TokioUnixStream, server: QuicServer) -> Self {
        Self::new_with_lib_config_for_test(stream, server, crate::config::LibConfig::default())
    }

    /// Test-only constructor taking a full `LibConfig`, for tests that need
    /// to set specific transport/diagnostics knobs without touching the
    /// process environment. `new_with_stream_for_test` is this with
    /// `LibConfig::default()`.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_lib_config_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        lib_config: crate::config::LibConfig,
    ) -> Self {
        // Warm rayon's global thread pool so the first PalRLE-heavy frame
        // doesn't pay thread-spin-up latency on the hot path (design Section 4).
        rayon::iter::IntoParallelIterator::into_par_iter(0..1u32).for_each(|_| {});

        let local_addr = "0.0.0.0:443".parse().unwrap();
        Self::base(
            stream,
            server,
            lib_config,
            BaseDiffs {
                handle: None,
                local_addr,
                frame_backend: None,
            },
        )
    }

    /// WS-C2 MEASUREMENT ONLY: install a real GPU frame processor on a test
    /// bridge so `process_frame` takes the production GPU path.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_gpu_frame_processor_for_measurement(&mut self, p: GpuFrameProcessor) {
        self.frame_backend = Some(Box::new(GpuBackend::new(p)));
    }

    /// Install a frame backend (a fake with controllable latency, or a
    /// recording one) in place of the GPU one.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_frame_backend(&mut self, backend: Box<dyn FrameBackend>) {
        self.frame_backend = Some(backend);
    }

    /// Frame-carrying variant of the test constructor, accepting a pre-built
    /// stream, server, and frame channel. Delegates to `new_with_stream_for_test`
    /// and adds frame injection support.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_frames_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        frame_rx: mpsc::Receiver<FrameSubmission>,
    ) -> Self {
        let mut bridge = Self::new_with_stream_for_test(stream, server);
        bridge.frame_rx = Some(frame_rx);
        bridge
    }

    /// Injection-carrying variant of the test constructor, accepting a
    /// pre-built stream, server, and injected-tile-work channel. Delegates
    /// to `new_with_stream_for_test` and adds tile-injection support for the
    /// browserless harness.
    /// Available under `cfg(test)` or the `browserless-harness` feature; never
    /// in a production build.
    /// `grid_cols`/`grid_rows` size the scheduler once, up front, from the
    /// scene's known dimensions — mirroring how the real capture path sizes
    /// it from the frame's `TileGrid` in `dispatch_dirty_tiles_via_scheduler`.
    /// See `apply_injected_frame` for why this must not change mid-scene.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_with_injection_for_test(
        stream: TokioUnixStream,
        server: QuicServer,
        inject_rx: mpsc::Receiver<InjectedFrame>,
        grid_cols: u32,
        grid_rows: u32,
    ) -> Self {
        let mut bridge = Self::new_with_stream_for_test(stream, server);
        bridge.scheduler.resize(grid_cols, grid_rows);
        bridge.inject_rx = Some(inject_rx);
        bridge
    }

    /// Install a deterministic drop plan, for a caller that is about to move
    /// `self` into a `spawn_local`'d task (as the browserless harness does
    /// with `IoBridge::run`) and needs a way to both drive the plan from
    /// inside `send_to_all_sessions` and read its fired-count back out
    /// afterwards. Unlike `bwe_publish_handle` and friends, the `Arc` here
    /// originates with the *caller* (it already holds the rules to enforce)
    /// rather than with the bridge, so this is a setter rather than a
    /// handle getter — but the reason an `Arc<Mutex<_>>` is needed at all is
    /// the same ownership problem: once `self` is owned by a spawned task, a
    /// plain `&self`/`&mut self` accessor is unreachable, so the caller must
    /// keep its own clone of the `Arc` from *before* the move and read the
    /// clone's contents *after* aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_drop_plan(
        &mut self,
        plan: std::sync::Arc<std::sync::Mutex<crate::transport::drop_plan::DropPlan>>,
    ) {
        self.drop_plan = Some(plan);
    }

    /// Record the `(start, end)` of every injected-frame `cost` hold. See
    /// `hold_log`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_hold_log(&mut self, log: HoldLog) {
        self.hold_log = Some(log);
    }

    /// Record, in `log`, the tokio-clock instant at which the inbound arm
    /// takes each packet off the socket. See `inbound_read_log`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_inbound_read_log(
        &mut self,
        log: std::sync::Arc<std::sync::Mutex<Vec<std::time::Instant>>>,
    ) {
        self.inbound_read_log = Some(log);
    }

    /// Clone of the `Arc` behind `emitter_stats_publish`, for a caller that
    /// is about to move `self` into a `spawn_local`'d task and needs a way
    /// to read the reliable-emitter counters back out afterwards. See
    /// `emitter_stats_publish`'s doc comment — same ownership problem as
    /// `bwe_publish_handle`, same fix. Call this *before* the move; read
    /// the clone's contents *after* aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn emitter_stats_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<crate::transport::reliable_emitter::emitter::EmitterStats>>
    {
        self.emitter_stats_publish.clone()
    }

    /// Clone of the `Arc` behind `send_errs_publish` — cumulative
    /// `send_datagram` failures, overwhelmingly `Blocked`. Same
    /// ownership problem and fix as `emitter_stats_publish_handle`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn send_errs_publish_handle(&self) -> std::sync::Arc<std::sync::Mutex<u64>> {
        self.send_errs_publish.clone()
    }

    /// Clone of the `Arc` behind `latency_stats_publish` — `(critical,
    /// refinement)` — for a caller that is about to move `self` into a
    /// `spawn_local`'d task and needs a way to read the per-tier ACK
    /// latency stats back out afterwards. Same ownership problem and fix
    /// as `bwe_publish_handle` / `emitter_stats_publish_handle`. Call this
    /// *before* the move; read the clone's contents *after* aborting the
    /// task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn latency_stats_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<(TierLatencyStats, TierLatencyStats)>> {
        self.latency_stats_publish.clone()
    }

    /// Clone of the `Arc` behind `queued_latency_stats_publish` — `(critical,
    /// refinement)` `queued_at -> ACK` stats (BWE Stage 2.1). Same
    /// ownership problem and fix as `latency_stats_publish_handle`; call
    /// this *before* the move, read the clone's contents *after* aborting
    /// the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn queued_latency_stats_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<(TierLatencyStats, TierLatencyStats)>> {
        self.queued_latency_stats_publish.clone()
    }

    /// Clone of the `Arc` behind `probe_stats_publish` — `(probes_completed,
    /// probes_abandoned)` (BWE Stage 2.4). Same ownership problem and fix as
    /// `latency_stats_publish_handle`; call this *before* the move, read the
    /// clone's contents *after* aborting the task.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn probe_stats_publish_handle(&self) -> std::sync::Arc<std::sync::Mutex<(u64, u64)>> {
        self.probe_stats_publish.clone()
    }

    /// Drive exactly one injected frame: enqueue its work, then attempt the
    /// drain. Returns the number of items `apply_injected_frame` actually
    /// enqueued into the scheduler — NOT the number of items submitted in
    /// the batch. Tiles outside the fixed grid are skipped and excluded
    /// from the count (see `apply_injected_frame`'s grid-bounds check), so
    /// a harness asserting on this value gets an accurate signal that work
    /// was dropped instead of a false "all delivered".
    ///
    /// This awaits the next message on `inject_rx`: on an open channel
    /// with nothing queued yet, the call blocks until a frame arrives (or
    /// the sender drops the channel) — it does **not** time out on its
    /// own. Under `tokio::time::pause()` (the reason the
    /// `browserless-harness` feature exists) that means a scene which
    /// never sends another frame hangs here indefinitely rather than
    /// returning 0. Returns 0 only when the channel is closed or was
    /// never set up (`inject_rx.is_none()`). Callers that want to poll
    /// "is a frame ready right now" instead of blocking should use
    /// `try_drain_injection_for_test`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub async fn drain_injection_for_test(&mut self) -> usize {
        let inj = match self.inject_rx.as_mut() {
            Some(rx) => match rx.recv().await {
                Some(f) => f,
                None => return 0,
            },
            None => return 0,
        };
        self.apply_injected_frame(inj)
    }

    /// Non-blocking counterpart to `drain_injection_for_test`. If a frame
    /// is already queued on `inject_rx`, drains and applies it exactly
    /// like the blocking version and returns the number of items
    /// enqueued (same submitted-vs-enqueued distinction applies). If
    /// nothing is queued right now — or the channel is closed or was
    /// never set up — returns 0 immediately instead of waiting.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn try_drain_injection_for_test(&mut self) -> usize {
        let inj = match self.inject_rx.as_mut() {
            Some(rx) => match rx.try_recv() {
                Ok(f) => f,
                Err(_) => return 0,
            },
            None => return 0,
        };
        self.apply_injected_frame(inj)
    }

    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn scheduler_peek_for_test(&self) -> Vec<crate::transport::scheduler::TileWork> {
        self.scheduler.peek_for_test()
    }

    /// Same as `scheduler_peek_for_test`, but for `refinement_queue` — lets
    /// a test confirm CDF53 injected work lands in the pass-major-drained
    /// queue rather than the FIFO one.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn scheduler_refinement_peek_for_test(&self) -> Vec<crate::transport::scheduler::TileWork> {
        self.scheduler.refinement_peek_for_test()
    }
}
