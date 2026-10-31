//! Test and browserless-harness entry points on `IoBridge`: the `*_for_test`
//! constructors, the drop-plan setter, the `*_publish_handle` accessors that
//! clone the `Arc`s a caller reads back after moving the bridge into a task,
//! the injection drains and the scheduler peeks.
//!
//! Every item here keeps its own `#[cfg(any(test, feature =
//! "browserless-harness"))]` attribute; the module itself is not gated, so
//! the file compiles to an empty `impl` in a production build. The one
//! exception is `hold_injected_frame`, which `IoBridge::run`'s injected arm
//! calls unconditionally (that arm is compiled in every build, and dead in
//! production, where `inject_rx` is `None`). The
//! bandwidth-estimator accessors (`bwe_snapshot`, `bwe_publish_handle`, ...)
//! live in `bwe_glue.rs` and the `set_tick_budget_floor_bytes` setter in
//! `pacing.rs`.
//!
//! Not here: the `cfg`-gated struct fields these methods read and write, which
//! stay on `IoBridge` in `io_bridge.rs`.

use super::*;

/// Upper bound on the turns one `step_until_idle` may take. A busy scene
/// needs tens; this only exists to turn a livelock into an error.
#[cfg(any(test, feature = "browserless-harness"))]
const MAX_TURNS_PER_STEP: usize = 1_000_000;

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
        // No tailnet here, so no WhoIs. The gate still runs the real rule,
        // against an in-memory source that reports every peer as this
        // node's owner. Only these test constructors install it.
        let mut server = server;
        server.set_peer_gate(crate::transport::peer_auth::PeerGate::single_owner_for_tests());
        Self::base(
            stream.into(),
            server,
            lib_config,
            BaseDiffs {
                handle: None,
                local_addr,
                frame_backend: None,
            },
        )
    }

    /// Rebuild the model controller (D1 design A) with these settings. Call
    /// before the first connection; harness only, because production reads
    /// them through `LibConfig`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn use_model_controller_for_test(&mut self, settings: crate::config::ModelSettings) {
        self.install_model_controller(settings);
    }

    /// Replace the QUIC peer gate, for a test that needs a specific policy
    /// or a background identity source.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_peer_gate_for_test(&mut self, gate: crate::transport::peer_auth::PeerGate) {
        self.server.set_peer_gate(gate);
    }

    /// WS-C2 MEASUREMENT ONLY: install a real GPU frame processor on a test
    /// bridge so `process_frame` takes the production GPU path.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_gpu_frame_processor_for_measurement(&mut self, p: GpuFrameProcessor) {
        self.frame_pipeline = Some(FramePipeline::new(Box::new(GpuBackend::new(p))));
    }

    /// Install a frame backend (a fake with controllable latency, or a
    /// recording one) in place of the GPU one.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_frame_backend(&mut self, backend: Box<dyn FrameBackend>) {
        self.frame_pipeline = Some(FramePipeline::new(backend));
    }

    /// Hold an injected frame for its `cost`: it is applied when the loop's
    /// select reaches the deadline (arm 3c), and the loop serves the network
    /// meanwhile. Stands in for the worker thread's GPU and encode time in
    /// browserless scenes, where CPU time is invisible to a paused clock.
    /// Unreachable in production (`inject_rx` is `None`).
    pub(super) fn hold_injected_frame(&mut self, inj: InjectedFrame, cost: std::time::Duration) {
        let started = now_std();
        self.held = Some(HeldFrame {
            started,
            until: started + cost,
            frame: inj,
        });
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

    /// Constructor for a bridge the caller steps by hand: injection channel
    /// as `new_with_injection_for_test`, but reading and writing an in-memory
    /// pipe instead of a socketpair, and answering peer lookups in place.
    /// Drive it with [`step_until_idle`](Self::step_until_idle), never `run`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn new_stepped_for_test(
        stream: MemStream,
        server: QuicServer,
        inject_rx: mpsc::Receiver<InjectedFrame>,
        grid_cols: u32,
        grid_rows: u32,
    ) -> Self {
        // `new_with_lib_config_for_test` wants a socket; build on a throwaway
        // pair and swap the real stream in before anything is read.
        let (dummy, _peer) = TokioUnixStream::pair().expect("socketpair for a placeholder stream");
        let mut bridge =
            Self::new_with_injection_for_test(dummy, server, inject_rx, grid_cols, grid_rows);
        bridge.stream = BridgeStream::Mem(stream);
        bridge.inline_peer_lookups = true;
        bridge
    }

    /// Turn the event loop until it has nothing ready, then return.
    ///
    /// This is `run()`'s own body ([`turn`](Self::turn)), polled by hand
    /// instead of by a spawned task: when it returns, every source the loop
    /// selects over (inbound bytes, injected frames, a held frame, the next
    /// QUIC timer) is not ready *at the current instant*, and the caller owns
    /// the clock. Nothing here awaits anything that can complete later, so
    /// the number and order of turns is a function of the inputs alone.
    ///
    /// The dropped, pending turn is safe to drop: with whole frames pushed
    /// into the pipe the inbound `read_exact` either completes in one poll or
    /// has consumed nothing, and every other arm is rebuilt each turn.
    ///
    /// Returns the number of turns that completed, or an error from the loop.
    /// A [`TurnEnd::Eof`] ends the stepping early.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn step_until_idle(&mut self) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::task::{Context, Poll, Wake};

        struct Flag(AtomicBool);
        impl Wake for Flag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = std::task::Waker::from(flag.clone());
        let mut cx = Context::from_waker(&waker);
        let mut turns = 0usize;
        loop {
            // A loop that never goes idle (a timer that stays due, say) would
            // otherwise hang the caller's thread; production would spin too,
            // but a harness must say so.
            if turns >= MAX_TURNS_PER_STEP {
                return Err(format!(
                    "the bridge took {MAX_TURNS_PER_STEP} turns without going idle"
                )
                .into());
            }
            flag.0.store(false, Ordering::SeqCst);
            // Unconstrained: tokio's cooperative budget would make a long
            // enough run of ready channel reads report Pending, which here
            // would end the step early for no reason related to the scene.
            let mut fut = Box::pin(tokio::task::unconstrained(self.turn()));
            let polled = fut.as_mut().poll(&mut cx);
            drop(fut);
            match polled {
                Poll::Ready(Ok(TurnEnd::Continue)) => turns += 1,
                Poll::Ready(Ok(TurnEnd::Eof)) => return Ok(turns),
                Poll::Ready(Err(e)) => return Err(e),
                // Pending after the select picked an arm means a handler (or
                // `post_turn`) awaited something that was not ready. Dropping
                // the turn here, as the arms below do, would silently throw
                // away the rest of that handler: a half-read frame body, a
                // half-applied step. Say so instead.
                Poll::Pending if self.arm_picked => {
                    return Err(
                        "the bridge's turn went Pending inside a handler (after its \
                                select! picked an arm): stepping would drop the rest of the \
                                handler. A handler must not await anything that is not \
                                ready, or this harness cannot step it"
                            .into(),
                    );
                }
                // Woken while polled means "poll me again" (a yield), not
                // "waiting on something external".
                Poll::Pending if flag.0.load(Ordering::SeqCst) => {}
                Poll::Pending => return Ok(turns),
            }
        }
    }

    /// The earliest instant at which the loop has something to do without any
    /// new input: the QUIC timer, a display-mode debounce, a close deadline or
    /// a held frame. The stepping caller sleeps its clock to the minimum of
    /// this and its own deadlines.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn next_wake_for_harness(&mut self) -> Option<std::time::Instant> {
        // Not part of `next_wake_deadline`: that feeds the loop's timer arm,
        // which sits ahead of the held-frame arm and would starve it.
        let held = self.held.as_ref().map(|h| h.until);
        [self.next_wake_deadline(), held]
            .into_iter()
            .flatten()
            .min()
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

    /// Clone of the `Arc` behind `transport_probe_publish`: quinn path stats,
    /// send-queue depth and the client's feedback totals. Same pattern as
    /// `emitter_stats_publish_handle`.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn transport_probe_publish_handle(
        &self,
    ) -> std::sync::Arc<std::sync::Mutex<super::TransportProbe>> {
        self.transport_probe_publish.clone()
    }

    /// Drop every client tile NACK at the door (see `ignore_tile_nacks`).
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_ignore_tile_nacks(&mut self, ignore: bool) {
        self.ignore_tile_nacks = ignore;
    }

    /// Opt this bridge's emitter in to tile XOR parity (off by default; see
    /// `TransportConfig::tile_fec`). Call before the first datagram is built.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub fn set_tile_fec(&mut self, on: bool) {
        self.reliable_emitter.set_tile_fec(on);
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

#[cfg(test)]
mod stepping_tests {
    use super::*;
    use crate::transport::ghostbridge::encode_frame;
    use crate::transport::io_bridge::mem_stream_pair;

    fn stepped() -> (IoBridge, MemStream) {
        let (ours, theirs) = mem_stream_pair();
        let (_tx, rx) = mpsc::channel(1);
        let server = QuicServer::new().expect("server");
        (
            IoBridge::new_stepped_for_test(ours, server, rx, 4, 4),
            theirs,
        )
    }

    /// A handler that awaits something not ready must be an error: the
    /// stepper drops a pending turn, which throws away the rest of the
    /// handler. Here the frame header is in the pipe and its body is not, so
    /// `process_inbound`'s body `read_exact` goes Pending after the select
    /// picked the inbound arm.
    #[tokio::test]
    async fn a_handler_that_goes_pending_mid_turn_is_an_error_not_a_dropped_half_turn() {
        let (mut bridge, theirs) = stepped();
        let frame = encode_frame(&[1, 2, 3], &"127.0.0.1:9".parse().unwrap());
        theirs.push(&frame[..8]);
        let err = bridge
            .step_until_idle()
            .expect_err("a Pending inside a handler must not be swallowed");
        assert!(
            err.to_string().contains("Pending inside a handler"),
            "{err}"
        );
    }

    /// Control, so the test above cannot pass by erroring on anything: the
    /// same frame, whole, steps cleanly, and so does an empty pipe.
    #[tokio::test]
    async fn a_whole_frame_and_an_idle_pipe_step_cleanly() {
        let (mut bridge, theirs) = stepped();
        assert!(bridge.step_until_idle().is_ok(), "idle pipe");
        theirs.push(&encode_frame(&[1, 2, 3], &"127.0.0.1:9".parse().unwrap()));
        let turns = bridge.step_until_idle().expect("whole frame");
        assert!(turns >= 1, "premise: the frame was taken ({turns} turns)");
    }
}
