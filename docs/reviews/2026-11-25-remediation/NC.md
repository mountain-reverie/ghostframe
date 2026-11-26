# Group NC: native client runtime, C API, CLI, client-side hygiene

Re-verified at `e85fb005` in the worktree `ghostframe-review-2026-11-25`. Every
line number below is from that tree. Nothing was built or run. "CONFIRMED" means
confirmed by reading the code; where the runtime effect still needs a GPU to
prove, the finding says so.

**What changed since the original review, and how it affects this group:**

- `ghostframe-render` is new. It holds the portable `TileRenderer`,
  `Framebuffer`, `cdf53_grid`, `coalesce` and `dirty`. `ghostframe-client-gpu`
  re-exports `coalesce`, `dirty` and `framebuffer` from it
  (`ghostframe-client-gpu/src/lib.rs:119`). The native-only parts stay in
  client-gpu: `Renderer`, `ExportRing`, `WgpuContext`, the upload pool and the
  H.264 import. So the framebuffer `debug_*` helpers now live in
  `ghostframe-render/src/framebuffer.rs`.
- `ghostframe-web-client/` is now only `index.html`, which confirms the claim.
  The browser client is Rust: `ghostframe-client-wasm` plus the Leptos crate
  `ghostframe-web-ui` (DEVELOPERS.md:29-31). `ghostframe-client-wasm/src/units.rs`
  and `constants.rs` no longer exist.
- The web and native clients now share one CDF 5/3 GPU path, so NC-9 is
  stale (details there).

**A new High finding, folded into NC-2:** the native device is opened with
`max_texture_dimension_2d = 2048`. That limit comes from
`wgpu::Limits::downlevel_defaults()`, which `wgpu-types` 30.0.1 sets to 2048
(`limits.rs:560`); `with_adapter_buffer_limits` raises only the buffer limits.
As a result, any remote picture wider or taller than 2048 px (2560x1440, 4K)
fails texture validation. The default uncaptured-error handler panics on that,
so the render thread dies: the NC-1 frozen picture, triggered by an ordinary
screen size.

---

## NC-1: a dead decode or render thread freezes the picture while the net thread keeps acknowledging (High)

**Status at e85fb005: CONFIRMED.**

- None of the three threads is guarded against panics. They are spawned with a
  bare `std::thread::Builder::spawn` at `ghostframe-client-native/src/lib.rs:430`
  (render), `:468` (decode) and `:477` (net).
- `shutdown_threads` throws away every join result (`let _ = h.join()`,
  lib.rs:534-542).
- When the decode channel closes, the net thread logs a warning for every event
  and carries on (`net_thread.rs:477-482`, "decode thread channel closed;
  dropping core event"). It keeps running quinn, and with it the datagram
  acknowledgements, which `ClientCore` produces on receipt on that same thread.
- When the render thread goes away, the decode thread sets `render_gone` and
  drains silently forever (`decode_thread.rs:140-145`, `:188-192`).
- The render thread can never notice that the decode thread died. It holds its
  own `self_tx` (render_thread.rs:557, cloned into the H.264 notify at `:182`),
  so the `Err(_) => break` on `recv` (`:626`, `:631`) is unreachable while the
  thread is alive.
- A GPU flush failure is reported once, and every later flush fails silently
  (`render_thread.rs:816-830`, the `flush_error_reported` latch).
- **Additional silent paths, not in the original finding.** The net thread
  returns without any host event when `epoll_create1`, `timerfd_create` or
  `epoll_ctl` fail (`net_thread.rs:352-379`), when `epoll_wait` fails (`:403-406`)
  and when the tsnet UDP read fails (`:415-419`). The host is never told and
  waits forever.
- **The panic sources that exist today:**
  - `ring.rs:729`: `.expect("poll after export publish")`, reached on device
    loss.
  - `ghostframe-render/src/framebuffer.rs:274`, `:324` and `:345`: `expect`
    inside `debug_fill`, `debug_fill_tile` and `debug_read`. These are on
    production paths: `ring.rs:320` runs the export self-test, and
    `renderer.rs:655` runs the frame dump.
  - wgpu's default uncaptured-error handler (NC-2).

**Root cause:** the threads have no supervision. "Exited" is not distinguished
from "exited abnormally", and no thread's exit is turned into a host-visible,
terminal event. The CLI already treats `ClientEvent::Error` as fatal
(`ghostframe-cli/src/commands.rs:498-500`, `:692-694`), so an event is all that
is missing for a clean exit with a message.

**Design (chosen): a supervised spawn plus a once-only terminal event.**

1. New module `ghostframe-client-native/src/supervise.rs`.
   `spawn_supervised(name, ctx: Arc<SessionCtl>, f: impl FnOnce() -> Result<(), ThreadFailure>)`
   wraps `f` in `std::panic::catch_unwind(AssertUnwindSafe(f))`. On a panic
   (the payload downcast to `&str` or `String`) or on `Err`, and **only if
   shutdown was not requested**, it:
   - pushes `ClientEvent::Error { message: "<thread> failed: <why>" }`;
   - calls `SessionCtl::end(reason)`, which pushes a single
     `ClientEvent::Disconnected { expected: false }` per session;
   - asks the net thread to close the QUIC connection, by setting
     `SessionCtl::fatal` and writing the wake eventfd.
2. `SessionCtl` (one per `connect`) holds:
   - `shutting_down: AtomicBool`, set by `shutdown_threads` before it sends
     anything;
   - `terminal_sent: AtomicBool`, so the net thread's `ConnectionLost`, an
     eviction and a supervisor failure produce one `Disconnected` between them,
     never two;
   - `fatal: Mutex<Option<String>>`;
   - the wake fd, as a `WakeFd` (NC-8).
3. The net thread reads `ctl.fatal` on every wake. If it is set, it leaves
   `'outer` and calls the existing `close_and_flush` with
   `CLOSE_CODE_CLIENT_GONE` (no new wire code) and a reason such as
   `b"client render failed"`, so the server forgets the session at once.
   Everything the net thread currently does with `tracing::error!` + `return`
   or `break 'outer` becomes `return Err(ThreadFailure::Net(..))`.
4. The net thread treats a closed `decode_tx` as fatal: it leaves the loop with
   `ThreadFailure::DecodeGone` instead of warning per event.
5. The decode thread: `render_gone` makes `run` return
   `Err(ThreadFailure::RenderGone)` instead of draining. The supervisor reports
   it only when shutdown was not requested. Normal teardown already lets the
   decode thread see the render thread gone, because `RenderMsg::Shutdown` is
   sent before the joins.
6. The render thread:
   - `run` returns `Result<(), ThreadFailure>`;
   - a flush error whose cause is device loss (NC-2's `GpuError::DeviceLost`)
     returns `Err` instead of latching `flush_error_reported`;
   - other flush errors keep the report-once behaviour.
7. `shutdown_threads` sets `shutting_down` first. It then joins each handle,
   and an `Err(payload)` from `join` is logged with `tracing::error!` (no event:
   the host is tearing down).

**Rejected:**

- **`panic = "abort"`.** The library is linked into arbitrary hosts, and
  aborting the embedder's process is worse than a frozen picture.
- **A watchdog heartbeat between the threads.** It catches wedges as well as
  deaths, but the render thread legitimately blocks in `recv` for as long as
  the screen is idle, so a heartbeat would need a timer on every thread. It
  could be added later; deaths are the observed problem.
- **Restarting a dead render thread.** The GPU state and the in-flight export
  buffers held by the host make that unsafe. The terminal event lets the host
  reconnect.

**Files:**

- `ghostframe-client-native/src/{lib.rs, supervise.rs (new), net_thread.rs, decode_thread.rs, render_thread.rs, event.rs}`

**Steps:**

1. Add `supervise.rs` with `ThreadFailure` (a `thiserror` enum with `Panic(String)`,
   `Net(String)`, `DecodeGone`, `RenderGone`, `Gpu(String)`), `SessionCtl` and
   `spawn_supervised`.
2. Add `EventQueue::push_terminal(&self, ev, sent: &AtomicBool)`, or keep the
   once-only logic in `SessionCtl::end`.
3. Change `net_thread::run` to return `Result<(), ThreadFailure>` and add
   `ctl: Arc<SessionCtl>` to `NetThreadArgs`. Route `ConnectionLost` through
   `ctl.end`. Check `ctl.fatal` when the wake fd fires.
4. Extract the `take_events` loop (`net_thread.rs:460-485`) into
   `fn route_net_events(events, queue, decode_tx, ctl) -> ControlFlow<ThreadFailure>`
   so it can be tested without a network.
5. Change `decode_thread::run` and `run_batched` to return `Result`.
6. Change `render_thread::run` to return `Result` (signature via NC-8's
   `RenderThreadArgs`).
7. In `connect`, spawn all three through `spawn_supervised`. Reset `SessionCtl`
   per connect.
8. In `shutdown_threads`, set `shutting_down`, log join panics, and keep the
   order.

**Sketch (unverified signatures, new code):**

```rust
// sketch
pub(crate) fn spawn_supervised<F>(name: &str, ctl: Arc<SessionCtl>, f: F)
    -> std::io::Result<std::thread::JoinHandle<()>>
where F: FnOnce() -> Result<(), ThreadFailure> + Send + 'static
{
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        let why = match r { Ok(Ok(())) => return, Ok(Err(e)) => e.to_string(),
                            Err(p) => panic_message(&*p) };
        ctl.fail(std::thread::current().name().unwrap_or("?"), why);
    })
}
```

**Tests (no GPU needed for any of them):**

- `supervise::tests::panic_becomes_error_then_one_unexpected_disconnect`: a
  closure that panics. The queue then holds exactly `Error` followed by
  `Disconnected { expected: false }`, `ctl.fatal` is `Some`, and the wake fd is
  readable.
- `supervise::tests::err_return_is_reported_like_a_panic`.
- `supervise::tests::nothing_is_reported_after_shutdown_was_requested`: set
  `shutting_down`, then panic. The queue is empty.
- `supervise::tests::at_most_one_terminal_event`: `ctl.end` from two threads
  plus a failure gives exactly one `Disconnected` (property, over 100
  iterations).
- `net_thread::tests::closed_decode_channel_is_fatal`: call
  `route_net_events` with a `Core(TileReady)` and a dropped receiver. Expect
  `Break(DecodeGone)`.
- `decode_thread::tests::render_gone_ends_the_thread_with_an_error`: call
  `run_batched` with `slots: None` (no GPU), drop the render receiver, send one
  event. The function returns `Err(RenderGone)` within one second (use a done
  channel with `recv_timeout`, never an unbounded join).
- The existing `net_thread` socketpair tests (`net_thread.rs:600-810`) grow one
  case: close the read side, and `run` returns `Err(Net(..))` and pushes a
  `Disconnected`.

**Mutation checks:**

- Replace `catch_unwind` with a direct call. The panic test must fail (the
  thread dies and the queue stays empty).
- Restore the `warn!` in place of the `Break`. The decode-channel test fails.
- Remove the `shutting_down` check. The shutdown test fails.

**Gates:**

- `just test-client`, run as `gf-gpu-run just test-client`: the crate's other
  tests open devices, and AGENTS.md and memory say to serialise them.
- `just ci-client`.

**Risks and owner decisions:**

- Whether a fatal client failure deserves a distinct close code. The proposal
  reuses `CLOSE_CODE_CLIENT_GONE` and changes only the reason bytes, which
  avoids a protocol change. A new code would be group WP's.
- `AssertUnwindSafe` over the thread state is sound, because nothing is reused
  after an unwind.

**Size:** M.

---

## NC-2: no wgpu error or device-lost handling on the native path; device limits never checked (High)

**Status at e85fb005: CONFIRMED, and worse than reported.**

- `ghostframe-client-gpu/src/wgpu_ctx.rs` installs neither `on_uncaptured_error`
  nor `set_device_lost_callback`; a grep finds them only in
  `ghostframe-client-wasm/src/web/renderer.rs:193` and `:207`. wgpu's default
  uncaptured handler panics.
- **New:** both device opens ask for `max_texture_dimension_2d = 2048`.
  - Vulkan: `wgpu_ctx.rs:205-212`. GLES: `:434-444`.
  - The value comes from `wgpu::Limits::downlevel_defaults()`, which is 2048 in
    wgpu-types 30.0.1. `with_adapter_buffer_limits` (`:127-133`) copies only
    `max_storage_buffer_binding_size` and `max_buffer_size` from the adapter.
  - wgpu validates against the requested limits, so a `Framebuffer::new` larger
    than 2048 is a validation error, which then panics the render thread.
  - `dimensions_valid` (`ghostframe-render/src/tile_renderer.rs:40-43`) checks
    only `1..=MAX_FRAME_DIMENSION` (8192, `ghostframe-client-core/src/lib.rs:83`).
    It is called by `Renderer::new` (`renderer.rs:240`) and
    `apply_frame_dimensions` (`renderer.rs:366`), and never compared with
    `device.limits()`.
  - The only limit check that exists is the upload pool's (`upload_pool.rs:294`).
  - No native GPU test uses a frame wider than 2048. **Verify first** with the
    GPU test below; the reading of the code is unambiguous.
- `ring.rs:729`: `.expect("poll after export publish")`.

**Root cause:** the device is configured for portability (shaders are checked
against the downlevel limits) without separating the texture-size limits from
the shader limits, and the error model is wgpu's default, which panics.

**Design (chosen):**

1. **Limits.** Rename `with_adapter_buffer_limits` to `with_adapter_size_limits`
   and also copy `max_texture_dimension_2d` from the adapter (and
   `max_texture_dimension_1d`, harmlessly). Shader-relevant limits, such as
   workgroup sizes and storage buffers per stage, stay downlevel, so the
   portability guarantee the comment describes is kept.
2. **Size check against the device.** Add
   `WgpuContext::max_frame_dimension(&self) -> u32`, which is
   `min(MAX_FRAME_DIMENSION, device.limits().max_texture_dimension_2d)`.
   - `Renderer::new` returns `GpuError::FrameTooLarge { width, height, max }`.
   - `apply_frame_dimensions` returns that error instead of `Ok(())`. The
     render thread already turns an `apply_event` error into
     `ClientEvent::Error` (render_thread.rs:214-218), which the CLI treats as
     fatal with a readable message.
   - Do not silently keep the old size. A frozen, wrong-size picture is what
     this group exists to remove.
3. **Telling the server (optional, in the same PR).** In `connect`, once
   `gpu_ready` has arrived, clamp `ClientDisplay::max_width` and `max_height` to
   the device's maximum before building `ClientNetConfig`. This means building
   `ClientNet` after the render thread reports, which is a reorder within
   `connect` (lib.rs:369-378 moves below `:464`). That way, a negotiated session
   never asks for a size the GPU cannot hold. Attach mode, which captures the
   operator's screen whatever its size, still relies on step 2.
4. **Error handler.** In `WgpuContext::new` (both backends), install
   `device.on_uncaptured_error`. It logs at `error`, increments
   `uncaptured_errors: AtomicU64` and stores the first 16 messages in a
   `Mutex<Vec<String>>`, the same shape as the wasm handler. Never panic.
5. **Device loss.** Install `device.set_device_lost_callback(|reason, msg| ..)`.
   It sets `lost: Mutex<Option<String>>` and ignores `DeviceLostReason::Destroyed`,
   as wasm does. Add `WgpuContext::lost(&self) -> Option<String>`.
6. **No panic in the publish path.** `ExportRing::wait_for_gpu` returns
   `Result<(), GpuError>` and maps a poll error to `GpuError::DeviceLost(..)`.
   The callers in `publish` propagate it. `Renderer::flush_and_publish` already
   returns `Result`.
7. **The render thread.** After every `flush_and_publish`, if `ctx.lost()` is
   set or the error is `DeviceLost`, return `Err(ThreadFailure::Gpu)`, which is
   terminal through NC-1. Once a second (in the existing summary block), if
   `uncaptured_errors` grew, push one `ClientEvent::Error` with the first
   message; this replaces the panic.
8. **`expect`s on production paths.** `debug_fill` and `debug_read` are covered
   in NC-7. Their production callers move to the `Result` variants:
   `ring.rs:320` (self-test failure means "no compute export") and
   `renderer.rs:655` (a failed dump is logged and skipped).

**Rejected:**

- **Requesting `adapter.limits()` wholesale.** That loses the shader-limit
  guarantee in the comment at `wgpu_ctx.rs:118-126`, and the
  `shader_validation.rs` cross-check depends on it.
- **Treating every uncaptured error as fatal.** Validation errors from a
  diagnostic path should not end a session. Only device loss is terminal.
- **Error scopes around every submit.** The render crate already uses them for
  self-tests (`errors_readback.rs`); doing it per frame costs a round trip.

**Files:**

- `ghostframe-client-gpu/src/{wgpu_ctx.rs, ring.rs, renderer.rs, lib.rs (GpuError variants)}`
- `ghostframe-render/src/lib.rs` (the `GpuError` mapping, if `FrameTooLarge`
  goes there)
- `ghostframe-client-native/src/{render_thread.rs, lib.rs}`

**Tests:**

- No GPU:
  `wgpu_ctx::tests::size_limits_follow_the_adapter_and_shader_limits_do_not`.
  Pure: feed `with_adapter_size_limits(downlevel, &adapter_with_16384)` and
  assert that the 2D texture limit is 16384 and that
  `max_compute_invocations_per_workgroup` and
  `max_storage_buffers_per_shader_stage` are unchanged.
- No GPU: `renderer`'s pure helper `frame_fits(w, h, device_max)` is tested
  with a sweep over w and h in {1, max-1, max, max+1, MAX_FRAME_DIMENSION+1}
  against an oracle `w <= min(8192, max) && h <= ...`. Sweep it rather than
  sampling (memory: "test vectors must discriminate").
- GPU (`ghostframe-client-gpu/tests/gpu_export.rs` or a new
  `tests/gpu_limits.rs`, under `gf-gpu-run`):
  - `a_2560x1440_renderer_builds_and_publishes`, plus a `3840x2160` case. These
    fail today if the reading above is right.
  - `device_limit_is_at_least_4096_on_this_adapter`: skip when the adapter
    reports less.
- GPU, test-support:
  `device_loss_makes_flush_return_device_lost_not_panic`, using
  `device.destroy()`. It does not trip the lost callback, because the callback
  ignores `Destroyed`, but it does make `poll` and submit fail. Assert that
  `flush_and_publish` returns `Err` and that the test thread did not panic.

**Mutation checks:**

- Revert the limits helper. The 2560 GPU test panics or fails, and the pure
  limits test fails.
- Restore the `expect` at `ring.rs:729`. The destroy test panics.

**Gates:**

- `gf-gpu-run just test-client`.
- `just check-gles`, because the GLES `new` changes too.
- `just ci-client`.

**Risks and owner decisions:**

- Raising the 2D limit to the adapter's maximum on panfrost and other Mali
  parts: the adapter reports what it supports, so this is safe by construction,
  but run it once on the Pinebook.
- Step 3 changes what HELLO's display info carries (group WP or owner).

**Size:** M (the limits fix alone is S and should land first).

---

## NC-3: unbounded queues between the threads and to the host (Medium)

**Status at e85fb005: CONFIRMED.**

- Unbounded `mpsc::channel` for net to decode (`lib.rs:466`), decode to render
  (`lib.rs:390`, shared with the host's `Release` and `Redraw`) and host to net
  (`lib.rs:389`).
- The host event queue is an unbounded `VecDeque` (`event.rs:96`, `:141`).
- `FrameReady` is pushed for every publish (`render_thread.rs:855`), and none
  is coalesced.

**Where acknowledgements come from:** transport ACKs (`wire_seq`) and NACK
coverage are produced by `ClientCore` inside `ClientNet::handle_udp` and
`on_timeout`, on the net thread, on receipt and **before** decode
(`decode_thread.rs:1-9` says so). So a tile that has reached the decode channel
has already been acknowledged, and the server will not resend it. **Dropping
anything on the net-to-decode or decode-to-render hop is silent permanent
corruption**, until the tile changes or a refinement pass covers it. H.264
results go back through `NetCommand::H264Result`, so H.264 is gated on decode,
but tiles are not.

**Deadlock analysis for bounded channels.** The threads form a cycle:

- the net thread sends to decode;
- decode sends to render;
- render sends to the net thread (`result_sink` sends
  `NetCommand::H264Result`, lib.rs:408-428);
- render also sends to itself (`self_tx`, from the H.264 decoder's notify
  thread).

A blocking `sync_channel` on any edge of the cycle can deadlock: render blocks
on a full command channel, and the net thread blocks on a full decode channel
that decode cannot empty because render does not read. A bounded net-to-decode
channel that *blocks* also stalls the acknowledgement clock. That makes a client
CPU stall look like path congestion to the server's ledger and goog_cc, which
is the known open issue "client stalls read as path loss" (memory,
`project_h264_flips_worm_can`). So **no blocking bounded send on any of the
three thread edges.**

**Design (chosen): bounded where dropping is lossless, measured everywhere
else, and a fail-loud ceiling.**

1. **Host event queue.**
   - Coalesce `FrameReady`. `acquire_frame` always returns the latest published
     frame (lib.rs:579-584), and `withdraw_unacquired` has already released
     any frame the host did not take, so an older queued `FrameReady` names a
     frame that no longer exists. In `push`, when the event is `FrameReady`,
     remove any queued `FrameReady` first (an O(n) scan of a queue that is
     normally fewer than 4 long). Keep its position relative to `Resized`: the
     new one goes to the back.
   - Deduplicate consecutive identical `Error` messages.
   - Hard-cap the queue at 1024. Beyond the cap, drop the oldest non-terminal
     event and count it (`dropped_events`). `Disconnected` is never dropped.
2. **Net to decode, and decode to render:** stay unbounded (lossless, no
   deadlock), and add depth telemetry.
   - A shared `Arc<AtomicUsize>` per edge, incremented by the sender and
     decremented by the receiver, plus an approximate byte counter for
     `TilePayload` and `H264` payloads (`payload.len()`).
   - The render thread's existing 1 s `tracing::debug!` summary and the decode
     thread's `Stats::tick` log `depth`, `depth_max` and `bytes`.
3. **Fail-loud ceiling.** If the net-to-decode backlog exceeds 256 MiB (several
   seconds of 8K CDF 5/3), the net thread ends the session through NC-1's
   `ctl.fail("decode backlog exceeded …")`: an explicit, logged disconnect
   rather than an OOM kill. The ceiling is a constant, `pub(crate)`.
4. **Host to net (input):** unchanged. It is human-rate.

**Rejected:**

- **Bounded `sync_channel` everywhere.** It deadlocks (above) and stalls the
  acknowledgements.
- **Drop-oldest on the tile edges.** The tiles were already acknowledged, so
  dropping is silent corruption.
- **Asking the server to slow down.** There is no client-to-server "I am
  behind" message today. A protocol addition like that belongs to groups WP and
  CC, and is noted as a follow-up.

**Files:**

- `ghostframe-client-native/src/{event.rs, lib.rs, net_thread.rs, decode_thread.rs, render_thread.rs}`

**Tests (no GPU):**

- `event::tests::frame_ready_is_coalesced_to_the_latest`: push FR1, Resized,
  FR2, FR3. Popping gives Resized then FR3, and the eventfd is not readable
  afterwards.
- `event::tests::cap_never_drops_disconnected`: push 5000 `FrameReady` and
  `Error` events and one `Disconnected` in the middle. The length stays at or
  below 1024 and `Disconnected` is still present (property, over random
  sequences, with proptest, which is already a dev-dependency in client-gpu;
  add it to client-native's dev-dependencies).
- `event::tests::fd_readable_iff_nonempty_after_coalescing`: the existing
  invariant from the doc comment, re-checked after coalescing removals.
- `net_thread::tests::backlog_ceiling_is_fatal`: `route_net_events` with a fake
  depth counter set over the ceiling returns `Break`.

**Mutation checks:**

- Remove the coalescing scan. The coalescing test fails.
- Make the cap drop from the front unconditionally. The `Disconnected` property
  fails.

**Gates:** `gf-gpu-run just test-client`; `just ci-client`.

**Risks:**

- Any host that counted `FrameReady` events as a frame counter would see fewer
  of them. No production users, but document it in the `ClientEvent` doc and
  in `gf_event`'s.

**Size:** M.

---

## NC-4: the render thread leaks when `connect` fails part-way (Medium, low probability)

**Status at e85fb005: CONFIRMED, narrower than reported.**

- After the render thread has reported `GpuReady` (lib.rs:453-464), the only
  remaining failure points in `connect` are the decode spawn (`:468-473`) and
  the net spawn (`:477-491`). These fail on thread limits (EAGAIN).
- On either, `connect` returns while the render thread blocks in `rx.recv()`
  forever, because it holds its own `self_tx` (`:429`, `:443`). It keeps the
  GPU device, the upload pool and `published`. `render_handle` is dropped,
  which detaches the thread.
- The `Ok(Err(e))` branch (`:458`) does not leak: `run` has already returned
  (`render_thread.rs:560-566`).
- Failures before `:430` (bootstrap, dial, `ClientNet`) happen before any
  thread exists.

**Design (chosen):** an RAII rollback guard in `connect`.
`struct StartedThreads { render: Option<(Sender<RenderMsg>, JoinHandle<()>)>, decode: Option<JoinHandle<()>> }`
gets a `Drop` that sends `RenderMsg::Shutdown`, drops its sender and joins the
handles. `connect` creates it right after the render spawn, fills it as threads
start, and calls `disarm()` (which moves the parts into `self`) only on success.
With NC-1, `SessionCtl::shutting_down` is set in that `Drop`, so the
supervisor stays quiet.

**Rejected:**

- **A weak sender for the H.264 notify.** `std::sync::mpsc` has no weak
  sender, and a crossbeam migration is out of scope.
- **Spawning decode and net before render.** GPU readiness has to be known
  first for `slot_allocator` and `cdf53_on_gpu`.

**Files:** `ghostframe-client-native/src/lib.rs`.

**Steps:**

1. Add the guard type (private).
2. Wire it into `connect` between the render spawn and the final assignments.
3. Change `shutdown_threads` to share the guard's join helper.

**Tests (no GPU):**

- `lib::tests::rollback_guard_stops_a_self_feeding_thread`. A fake thread
  models the render loop: it holds a clone of its own sender and loops on
  `recv()` until `Shutdown`. Dropping the guard must make the thread exit;
  assert this through a done channel with `recv_timeout(2s)`. Do not use a bare
  join, which would hang on a regression instead of failing.
- To make the guard generic enough to test, parameterise it over the message
  type with a `shutdown_msg` constructor (sketch:
  `StartedThreads<M>{ tx: Sender<M>, stop: fn() -> M, .. }`).

**Mutation check:** remove the `Shutdown` send in `Drop`. The test times out and
fails.

**Gates:** `just test-client`.

**Size:** S.

---

## NC-5: gaps in the C API (Medium)

**Status at e85fb005, item by item:**

| Item | Status | Evidence |
|---|---|---|
| `gf_frame` has no DRM fourcc | CONFIRMED | Header `gf_frame` has `drm_modifier` and no format. `PublishedFrame` (`ring.rs:91-113`) has no format either. Export is `B8G8R8A8_UNORM` (Vulkan, `export.rs:56`) or `Bgra8Unorm` (GLES, `export_gles.rs:102`), both `DRM_FORMAT_ARGB8888` (`export_gles.rs:157`). The CLI hardcodes it (`ghostframe-cli/src/window/wayland.rs:87`). |
| An exact `struct_size` rule makes later additions breaking | CONFIRMED | `capi/src/lib.rs:20-30` (doc), `:196-198`, `:358-360`, `:511-513` (`!=`) |
| No version macros | CONFIRMED | None in the header. |
| No `gf_version()` | **STALE** | `gf_version` exists (`capi/src/lib.rs:729`; header `:417`), and `tests/abi.rs:33` covers it. |
| No visibility macro | CONFIRMED, but not needed on Linux | `cdylib` exports only `#[no_mangle] pub extern` symbols. |
| `gf_result_str(uint32_t)` while functions return `enum gf_result` | CONFIRMED, **deliberate** | Its doc (header `:419-429`) explains this: an unknown value must not be UB. |
| C enums as struct fields | CONFIRMED | `enum gf_event_type kind`, `enum gf_handle_type handle_type`. The Rust types are `#[repr(C)]` (`types.rs:14`, `:36`), so their width is "C int", which is implementation-defined (for example `-fshort-enums`). |
| Control URL only through the `TS_CONTROL_URL` env var inside the library | CONFIRMED | `client-native/src/lib.rs:328`. Note that tsnet itself already falls back to `TS_CONTROL_URL` when `ControlURL` is empty (`tailscale.com@v1.104.2/tsnet/tsnet.go:770-775`), so the library's read is redundant. |
| **New:** `gf_client_config` has no display info | CONFIRMED | `capi/src/lib.rs:253` hardcodes `display: None`, so a C embedder cannot negotiate display mode. |

**Design (chosen).** There are no production users and compatibility is not
required (memory, `project_gpu_first_no_compat`), so all the breaking changes go
into **one** ABI revision now, which then becomes the baseline that later
revisions extend.

1. **Fixed-width enums.** Change `gf_result`, `gf_event_type` and
   `gf_handle_type` to `#[repr(u32)]`. cbindgen 0.29 emits a `repr(u32)` enum in
   C as an `enum` of constants plus `typedef uint32_t <name>;`, so the struct
   fields become 32 bits by type. (Check the generated header text in the PR:
   the gate is the diff.) `gf_result_str` keeps taking `u32`, which now *is*
   the `gf_result` typedef in C, so the mismatch disappears.
2. **Fourcc.**
   - Add `pub drm_format: u32` to `PublishedFrame`. The value is set by the
     ring from a per-backend `pub(crate) const EXPORT_DRM_FORMAT` in
     `export.rs` and `export_gles.rs`.
   - Add `uint32_t drm_format` to `gf_frame`, after `drm_modifier`.
   - Make the CLI's Wayland and X11 backends use `frame.drm_format` instead of
     their constant, keeping the constant only as a debug assertion in tests.
   - Mind the cbindgen trap from AGENTS.md: the constant must be `pub(crate)`.
3. **The `struct_size` rule becomes minimum-plus-prefix.** Each struct gets a
   `const GF_<T>_SIZE_V1: usize`, equal to `size_of` of this revision
   (`pub(crate)`, so it is not exported). The rule is
   `claimed >= SIZE_V1 && claimed <= u32::MAX`.
   - In-structs (`gf_client_config`): copy `min(claimed, size_of)` bytes into a
     `MaybeUninit::<gf_client_config>::zeroed()` with
     `ptr::copy_nonoverlapping`. Then validate as today. Zero is a valid default
     for every field (null pointer, false, 0).
   - Out-structs (`gf_event`, `gf_frame`): build a local, write `struct_size =
     min(claimed, size_of)`, and copy that many bytes out.
   - Today every `claimed` must equal `SIZE_V1`, so the behaviour change is
     only that a larger caller struct (from a newer header) is accepted, and
     only the known prefix is written. Rewrite the doc at `capi/src/lib.rs:20-30`.
4. **Version macros.** In `build.rs`, call
   `.with_after_include(format!("#define GF_VERSION_MAJOR {}\n#define GF_VERSION_MINOR {}\n#define GF_VERSION_PATCH {}", ..))`
   from `CARGO_PKG_VERSION_{MAJOR,MINOR,PATCH}` (`cbindgen::Builder::with_after_include`
   exists in 0.29.2, `builder.rs:65`). The header then changes on a version
   bump, which is intended. Add `GF_ABI_VERSION 1` as a hand-maintained
   `pub const GF_ABI_VERSION: u32 = 1;`. This is the one deliberate exported
   constant, and the header diff gate keeps it honest.
5. **Control URL.**
   - Add `pub control_url: Option<String>` to `Config` (`client-native/src/lib.rs:83`)
     and `const char *control_url` (nullable, where null means tsnet's default)
     to `gf_client_config`.
   - `connect` passes `self.config.control_url.clone().unwrap_or_default()`.
   - Delete `lib.rs:314-328` and the `ghostframe-client-native/src/lib.rs`
     exemption in `ci/check-env-reads.sh` (lines 37-46). tsnet's own env
     fallback keeps the e2e harness working without change. Confirm by
     grepping how `ghostframe-e2e` sets `TS_CONTROL_URL` for the native client.
6. **Display.** Add `struct gf_display { uint16_t max_width, max_height, scale_milli, mm_width, mm_height; }`
   plus `const gf_display *display` (nullable) to `gf_client_config`, mapped to
   `ClientDisplay`.
7. **Visibility macro: rejected.** Without a Windows target it adds nothing,
   since the Rust cdylib already exports only `#[no_mangle]` symbols. Revisit
   with a Windows port (`GF_HANDLE_WIN32_NT` is reserved).
8. **`gf_result_str` taking `enum gf_result`: rejected.** The current `u32`
   parameter is the safe choice, and step 1 makes the types agree anyway.

**Files:**

- `ghostframe-client-capi/{src/lib.rs, src/types.rs, build.rs, include/ghostframe_client.h (regenerated), tests/abi.rs}`
- `ghostframe-client-native/src/lib.rs` (`Config`)
- `ghostframe-client-gpu/src/{ring.rs, export.rs, export_gles.rs}`
- `ghostframe-cli/src/window/{wayland.rs, x11.rs}`
- `ghostframe-cli/src/commands.rs` (the `Config` literal)
- `ci/check-env-reads.sh`
- Every `Config { .. }` literal in tests and e2e. Find them with
  `git grep -n "debug_map_frames:"`.

**Tests (no GPU, in `ghostframe-client-capi/tests/abi.rs`):**

- `struct_size_larger_than_v1_is_accepted_and_only_the_prefix_written`: a
  `#[repr(C)]` wrapper struct `{ gf_event ev; u8 canary[64] }` filled with 0xAA.
  `gf_client_next_event` with `struct_size = size_of(wrapper)` and an event
  pushed returns `GF_OK`, and the canary is still 0xAA. A test-only way to push
  an event exists? Check `make_client()`; otherwise use `GF_AGAIN` plus the
  struct-size acceptance alone.
- `struct_size_smaller_than_v1_is_rejected`: keep the existing test, renamed.
- `config_prefix_copy_zero_fills`: pass a config claiming exactly `SIZE_V1`.
  It succeeds, and the new trailing field reads as its default.
- `enum_fields_are_four_bytes`: `size_of::<gf_event_type>() == 4` and
  `offset_of!(gf_event, width) == 8`, pinning the layout.
- `version_macros_match_gf_version`: read
  `include/ghostframe_client.h` as text, parse the `GF_VERSION_*` defines, and
  compare them with `gf_version()`.
- `frame_format_is_argb8888`, a GPU test in client-gpu's `gpu_export.rs`: a
  published frame's `drm_format == 0x34325241`.
- C compile check: in `ghostframe-client-capi/tests/`, add a `cc`-free check
  through `build.rs`? Simpler: extend `ci-client` with
  `cc -std=c99 -Wall -Werror -fsyntax-only -x c ghostframe-client-capi/include/ghostframe_client.h`,
  plus `-fshort-enums`, to show the layout no longer depends on it. **Owner
  decision:** whether a C compiler is an acceptable CI dependency (it already
  is, through Go's cgo).

**Mutation checks:**

- Revert to `!=` for `struct_size`. The "larger is accepted" test fails.
- Write `size_of` bytes instead of `claimed`. Not caught by the canary test,
  because a larger caller struct is still big enough. Add a case where `claimed`
  is `SIZE_V1` and the wrapper's canary starts right at `SIZE_V1`.

**Gates:**

- `just ci-client` (includes `git diff --exit-code ghostframe-client-capi/include/ghostframe_client.h`,
  Justfile:154). Regenerate the header by building the crate and commit it.
- `just ci-local` before merge (Justfile:316), because the e2e and server
  crates construct `Config`.

**Risks:** downstream C test programs, if any, in `ghostframe-e2e` must be
updated. Check `git grep -n gf_client_config`.

**Size:** M.

---

## NC-6: GPU debug knobs read from the environment inside library constructors; dump paths chosen by the environment (Medium/Low)

**Status at e85fb005: CONFIRMED.** The rule in `ci/check-env-reads.sh` holds: the
only reader in the client crates is `ghostframe-client-gpu/src/config.rs`
(`:53`, `:85`, `:123`, `:164`). But the `from_env()` calls sit inside library
constructors:

- `wgpu_ctx.rs:221` (`PublishConfig`, device open);
- `ring.rs:231` (`PublishConfig`, export copy path);
- `renderer.rs:298` (`DumpConfig`);
- `client-native/src/render_thread.rs:506` (`Cdf53Decode`), `:573`
  (`UploadConfig`) and `:606` (`DumpConfig`).

So any process that links the C library writes PPM files to whatever path
`GHOSTFRAME_CLIENT_DUMP_FRAME` or `GHOSTFRAME_CLIENT_DUMP_EXPORT` names. The CLI
turns its `--dump-dir` flag into `std::env::set_var` calls
(`ghostframe-cli/src/commands.rs:314-329`), because there is no other channel
into the library. That is unsound once any thread exists, and `unsafe` in
edition 2024; the CLI is edition 2021. Also, `ghostframe-render/src` is not
covered by `check-env-reads.sh`, although it has no env reads today.

**Design (chosen): parse in the embedders, pass a value down.**

1. Add `pub struct GpuOptions { pub publish: PublishConfig, pub upload: UploadConfig, pub cdf53: Cdf53Decode, pub dump: DumpConfig }`
   in `ghostframe-client-gpu/src/config.rs`.
   - `Default` means every knob off: no dumps, GPU CDF 5/3 "auto", non-blocking
     publish.
   - `GpuOptions::from_env()` stays here, the only reader, so the CI rule is
     unchanged.
2. Constructors take what they need:
   - `WgpuContext::new(&PublishConfig)`;
   - `ExportRing::new(.., publish: &PublishConfig)`;
   - `Renderer::new(.., dump: DumpConfig)`, or a setter
     `Renderer::set_dump(DumpConfig)`, which keeps the already long argument
     list from growing.
   - Remove every `from_env()` call outside `config.rs`, its tests and the
     embedders.
3. `client-native::Config` gains `pub gpu: GpuOptions`. The render thread takes
   it through NC-8's `RenderThreadArgs` and stops calling `from_env`.
4. Embedders:
   - `ghostframe-cli` calls `GpuOptions::from_env()` and then overrides `dump`
     from `--dump-dir`. Delete the `set_var` block.
   - `ghostframe-client-capi` calls `GpuOptions { dump: DumpConfig::default(), ..GpuOptions::from_env() }`.
     The diagnostic knobs keep working for a C embedder (they are documented
     in AGENTS.md as operator tools), but a shipped library never writes files
     to a path taken from the environment. A future `gf_client_config` field
     can enable dumps explicitly.
5. Extend `ci/check-env-reads.sh` to cover `ghostframe-render/src`, where
   nothing may read the environment, and to forbid `env::set_var` and
   `remove_var` in `ghostframe-cli/src` and every `ghostframe-client-*/src`
   (mirroring the server rule at `:58-64`).

**Rejected:**

- **A process-global `OnceLock` config set by the embedder.** It is hidden
  global state again, and tests could not run two configurations in one
  process.
- **Keeping the environment for dumps in the C API.** It is a file write to an
  environment-chosen path from a library linked into arbitrary hosts.

**Files:**

- `ghostframe-client-gpu/src/{config.rs, wgpu_ctx.rs, ring.rs, renderer.rs}`
- `ghostframe-client-native/src/{lib.rs, render_thread.rs}`
- `ghostframe-cli/src/commands.rs`
- `ghostframe-client-capi/src/lib.rs`
- `ci/check-env-reads.sh`
- The GPU tests that call `WgpuContext::new()` (`git grep -n "WgpuContext::new()"`)
  switch to `WgpuContext::new(&Default::default())`, or keep a
  `WgpuContext::new_default()` shorthand.

**Tests:**

- No GPU: `config::tests::defaults_turn_every_knob_off`.
- No GPU: `config::tests::capi_profile_never_dumps`, a pure function
  `capi_gpu_options(lookup)` asserting that `dump.frame_prefix` is `None` even
  when the lookup sets `GHOSTFRAME_CLIENT_DUMP_FRAME`.
- CLI: `commands::tests::dump_dir_sets_gpu_options_not_env`: build the `Config`
  through an extracted `fn connect_config(opts, env_lookup) -> Config`, then
  assert that `config.gpu.dump.frame_prefix == Some(dir/frame)` and that
  `std::env::var("GHOSTFRAME_CLIENT_DUMP_FRAME")` is unset.
- CI: the extended `check-env-reads.sh` is itself the test. Prove it by
  temporarily adding a `std::env::set_var` to the CLI in a scratch commit and
  watching the script fail.

**Mutation check:** re-add a `from_env()` in `ring.rs`. Nothing catches that
except a grep, so add a `check-env-reads.sh` stanza: `from_env()` call sites in
client-gpu are allowed only in `config.rs`.

**Gates:**

- `just ci-client` (runs `check-env-reads.sh`).
- `gf-gpu-run just test-client`.
- `just check-gles`.

**Size:** M.

---

## NC-7: test-only code shipped in libraries (Low/Medium)

**Status at e85fb005:**

- `ghostframe-client-wasm/src/units.rs` and `constants.rs`, and their
  unconditional `#[wasm_bindgen]` exports: **STALE.** Neither file exists. The
  only `#[wasm_bindgen]` exports are `start` (`lib.rs:38`) and `startClient`
  (`app/client.rs:722`). There is no vitest; the suites moved to Rust
  (`client-wasm/src/core.rs:113-118`, `mod moved_from_vitest`).
- `ghostframe-client-gpu`'s `pub mod testdata` is ungated: **CONFIRMED**
  (`ghostframe-client-gpu/src/lib.rs:102`; the `#[cfg(test)]` at `:100` belongs
  to `shader_validation`). It contains a `#[allow(clippy::too_many_arguments)]`
  helper. Its users are only `tests/gpu_flush_submits.rs`, `tests/gpu_oracle.rs`
  and `tests/upload_bench.rs`.
- The framebuffer `debug_*` helpers with `expect`s in the public API:
  **CONFIRMED, moved.** They are now in `ghostframe-render/src/framebuffer.rs:249`
  (`debug_fill`), `:281` (`debug_fill_tile`) and `:343` (`debug_read`), with
  `expect`s at `:274`, `:324` and `:345`. They are **not** test-only: `ring.rs:320`
  (export self-test) and `ghostframe-client-gpu/src/renderer.rs:655` (frame dump)
  call them in production.

**Design (chosen):**

1. Gate `testdata` as `#[cfg(any(test, feature = "test-support"))]`, the same
   pattern as `nv12_reference` at `lib.rs:82-83`. client-gpu's
   `[dev-dependencies]` must enable its own `test-support` for the integration
   tests. Check whether it already does (Cargo.toml:93 defines the feature),
   and do not make it a default feature (see the comment at Cargo.toml:85-87).
2. In `ghostframe-render`, add fallible `pub fn fill(&mut self, ..) -> Result<(), GpuError>`
   and `pub fn read_blocking(&self, ..) -> Result<Vec<u8>, GpuError>` (native
   only). The production callers use them: on `ring.rs:320` an error means "the
   kernel is not trusted", so it returns `false`; on `renderer.rs:655` it logs
   and skips the dump. Keep `debug_fill`, `debug_fill_tile` and `debug_read` as
   thin `expect` wrappers under `#[cfg(any(test, feature = "test-support"))]`
   for the many GPU tests (`git grep -n debug_fill` lists 25 test call sites).
   `ghostframe-render` gets a `test-support` feature, enabled by client-gpu's
   own `test-support` and by its dev-dependency.
3. Rejected: deleting the `debug_*` names. That churns about 25 test call sites
   for no gain.

**Files:**

- `ghostframe-client-gpu/{src/lib.rs, Cargo.toml, src/ring.rs, src/renderer.rs}`
- `ghostframe-render/{src/framebuffer.rs, Cargo.toml}`
- Possibly `ghostframe-client-native/Cargo.toml` (its dev-dependency on
  client-gpu).

**Tests:**

- The compile gate is the test: `cargo clippy` in `just test-client`, with
  `--all-targets`, proves the integration tests still find `testdata` (AGENTS.md:
  a test target that fails to compile vanishes from `cargo test`).
- GPU: `ring` creation still selects compute export on the dev box (existing
  `gpu_export` tests).
- Mutation check: make `fill` return `Err` unconditionally. The ring must fall
  back to the blit (`uses_compute_copy() == false`) rather than panic, covered
  by a new GPU unit test in `ring.rs` that uses a `test-support` fault switch on
  `Framebuffer`. If that switch is too invasive, mark it PLAUSIBLE and rely on
  review.

**Gates:**

- `gf-gpu-run just test-client`.
- `just check-web-renderer` or its equivalent (Justfile:126-ish, "The portable
  tile renderer must keep building for the browser"), because
  `ghostframe-render` changes.

**Size:** S.

---

## NC-8: duplication and idiom (Low)

**Status at e85fb005:**

- **The `DecodeErrorCode` mapping is written twice: CONFIRMED.**
  `ghostframe-render/src/tile_renderer.rs:217-231` (`decode_error_code(u32)`)
  and `ghostframe-client-wasm/src/app/client.rs:44-59`
  (`decode_error_code_from_u8`). The enum lives in
  `ghostframe-client-core/src/event.rs:14-25`, not in `ghostframe-protocol`.
  **Coordination:** if group WP's `TryFrom` work covers only
  `ghostframe-protocol`, NC adds `impl TryFrom<u8> for DecodeErrorCode` in
  client-core. Otherwise NC only migrates the two call sites. Agree this before
  PR NC-G.
- **The unsafe eventfd write is copied: CONFIRMED,** six signal-writes:
  - `client-native/src/event.rs:151-157`;
  - `lib.rs:419-425`, `:523-529`, `:671-677` and `:703-709`;
  - the drain at `net_thread.rs` (`drain_eventfd`) and the read in
    `event.rs:177-183`.

  (`net_thread.rs:169` and `:543` are frame writes, not eventfd.)
- **`render_thread::run` takes 11 positional arguments: CONFIRMED**
  (`render_thread.rs:546-559`), and `handle_core_event` takes 12 (`:137-150`).
- **`#[allow(clippy::too_many_arguments)]` in the client crates: CONFIRMED,
  10:** client-gpu `export.rs` ×5, `testdata.rs` ×1; client-native
  `render_thread.rs` ×2; `ghostframe-render` `cdf53_state.rs` ×1 and
  `tile_renderer.rs` ×1.
- **No `#[non_exhaustive]` on `ClientEvent`, `GpuError` and `ClientError`:
  CONFIRMED, but see the design.**
- **Stale references to `prevalidate.ts`: CONFIRMED,**
  `ghostframe-client-core/src/pal_rle_decode.rs:5` and `:41`. 38 `.rs` lines
  still cite `main.ts`, `*.ts:` or vitest. Many are useful provenance ("port of
  X"); only those that point readers at a file to open are worth fixing.
- **`ghostframe-client-net` built for wasm in CI while no wasm crate depends on
  it: CONFIRMED.** `.github/workflows/client-core.yml:21` builds `-p
  ghostframe-client-net --target wasm32-unknown-unknown`. Neither
  `ghostframe-client-wasm/Cargo.toml` nor `ghostframe-web-ui/Cargo.toml` names
  it. client-net keeps wasm `cfg` branches (`clock.rs:17-19`, `tls.rs:50-65`)
  that only this CI line compiles.

**Design (chosen):**

1. `pub(crate) struct WakeFd(OwnedFd)` in a new `ghostframe-client-native/src/wake.rs`,
   with `new() -> io::Result<Self>` (eventfd `NONBLOCK|CLOEXEC`), `signal(&self)`,
   `drain(&self)` and `as_raw_fd`. `EventQueue` uses it internally, and
   `Client.wake_fd` becomes `Arc<WakeFd>`. That leaves one `unsafe` for each
   syscall.
2. `pub(crate) struct RenderThreadArgs { .. }`, like `NetThreadArgs`, for `run`.
   `handle_core_event` takes a `&mut RenderState` context struct (`ctx`,
   `renderer`, `queue`, `published`, `export cfg`, `self_tx`, `pool`,
   `cdf53_gpu`) plus the event. Both `allow`s in client-native go.
3. `export.rs` ×5: review case by case. Many are `unsafe fn` FFI helpers where
   a struct would only rename the problem; convert those that pass the same
   five values around (an `ExportParams` struct). The others keep their `allow`
   with a `reason = "..."`, the repo's pattern (`ring.rs:722-725`).
4. `#[non_exhaustive]`:
   - **add** it to `ClientError` and both `GpuError`s. They are error types;
     consumers display them and match a few variants.
   - **reject** it for `ClientEvent`. Its consumers are inside the workspace
     (capi `lib.rs:374`, the CLI), and the repo deliberately wants a compile
     error at every match when a variant is added (`net_thread.rs:270-275`,
     memory `feedback_load_bearing_catch_all`). `#[non_exhaustive]` would force
     a `_ =>` arm in capi and the CLI, which is the hazard.
   - Check that `GpuError`'s cross-crate `From` (`client-gpu/src/lib.rs:235-245`)
     matches `ghostframe_render::GpuError` exhaustively. With
     `#[non_exhaustive]` it would need a wildcard, so add the attribute only to
     the client-gpu one, or only to the outermost type. **Decision: add it to
     `ClientError` only.**
5. `DecodeErrorCode`: `impl TryFrom<u8>`, plus `TryFrom<u32>` through `u8`
   (owned by group WP or by NC, as above). `tile_renderer.rs` and
   `app/client.rs` call `DecodeErrorCode::try_from(code).ok()`. Delete both
   local functions.
6. Rewrite the `prevalidate.ts` references as "historically ported from the
   TypeScript client's `prevalidate.ts` (removed)". Leave other provenance
   comments alone.
7. **Owner decision on `ghostframe-client-net` and wasm.** Either (a) drop it
   from the wasm CI build line and delete its wasm `cfg` branches (recommended:
   dead code that a CI line keeps alive), or (b) keep it as a deliberate option
   for a future WebTransport-in-Rust client, and say so in the workflow comment
   at `client-core.yml:12-17`.

**Files:**

- `ghostframe-client-native/src/{wake.rs (new), event.rs, lib.rs, net_thread.rs, render_thread.rs}`
- `ghostframe-client-gpu/src/export.rs`
- `ghostframe-client-core/src/{event.rs, pal_rle_decode.rs}`
- `ghostframe-render/src/tile_renderer.rs`
- `ghostframe-client-wasm/src/app/client.rs`
- `.github/workflows/client-core.yml`
- `ghostframe-client-net/src/{clock.rs, tls.rs}` (only with option a)

**Tests:**

- `wake::tests::signal_makes_readable_drain_clears` (no GPU; `poll` with a
  0 timeout).
- `DecodeErrorCode`: `try_from_u8_round_trips_every_variant_and_rejects_the_rest`,
  sweeping 0..=255 against `code as u8` for every variant. List the variants
  with an exhaustive `match` in the test so a new variant fails to compile.
  Never compare against literals (memory, `feedback_load_bearing_catch_all`).
- The existing `queue.rs` integration test covers `EventQueue` after the
  `WakeFd` switch.

**Mutation check:** make `TryFrom` map 10 to `None`. The sweep fails.

**Gates:**

- `just ci-client`.
- The wasm build: `cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown`,
  through its Justfile recipe, because `app/client.rs` changes.
- `just test-web-renderer` is optional.

**Size:** M (mechanical).

---

## NC-9: the web and native CDF 5/3 GPU paths differ, and only native has the oracle self-test (Low)

**Status at e85fb005: STALE.**

- `shaders/client/` now holds only `cdf53_planes.wgsl` and
  `cdf53_reconstruct.wgsl`. `cdf53_inverse_l{1,2,3}` and `cdf53_integrate` are
  gone, and no `.rs` file references them.
- Both clients drive the same grid in `ghostframe-render/src/pipelines/cdf53_grid.rs`:
  - The web side calls `TileRenderer::try_enable_cdf53_gpu`
    (`ghostframe-render/src/tile_renderer.rs:878`) from
    `ghostframe-client-wasm/src/web/renderer.rs:226-239`. Its comment says the
    self-test draws two known tiles, reads them back and compares them with the
    CPU decoder.
  - The native side calls `cdf53_grid::probe` (`render_thread.rs:505-529`).

**Remaining gap (optional, S):** the native self-test runs `cdf53_grid::probe`
directly, and the web one goes through `TileRenderer::try_enable_cdf53_gpu`
over `probe_grid` (`cdf53_grid.rs:615`). Confirm in review that both reach the
same CPU-oracle comparison and differ only in calibration ("no calibration on
the web: R3"). If they do, no work is needed. Otherwise, route native through
the same `probe_grid` entry point.

**Size:** none, or S.

---

## NC-10: CLI state directory, settings and terminal hygiene (Medium)

**Status at e85fb005: CONFIRMED** on every point.

- **State directory with default permissions:**
  `ghostframe-cli/src/commands.rs:92-93`, a bare
  `std::fs::create_dir_all(&dir)` (umask, typically 0755). The directory holds
  the node keys (`tailscaled.state`). `connect` and `logout` never check it.
- **A relative `XDG_STATE_HOME` is accepted:** `commands.rs:35-38`. The XDG
  base-directory specification says relative paths must be ignored.
- **No state lock:** nothing in `ghostframe-cli` takes `flock`. Two `connect`s,
  or a `login` during a `connect`, run two tsnet nodes on one state directory.
  The server has `StateLock` (`ghostframe-server/src/state.rs:230-261`) and
  `ensure_private_dir` (`ghostframe-server/src/util.rs:110-149`).
- **`login`, `connect` and `logout` disagree on control URL and hostname:**
  - `login` uses `--login-server` and `--hostname` (`commands.rs:95-103`).
  - `connect` uses `default_hostname()` and `TS_CONTROL_URL` from the
    environment, read inside the library (`commands.rs:366`;
    `client-native/src/lib.rs:328`).
  - `logout` uses `default_hostname()` and an empty control URL
    (`commands.rs:142-150`).
  - tsnet re-applies both on every start: `prefs.Hostname = s.hostname` and
    `prefs.ControlURL = s.getControlURL()` go into `lb.Start(UpdatePrefs)`
    (`tsnet.go:982-993`). So after `login --login-server https://hs.example
    --hostname foo`, a plain `connect` renames the node to
    `ghostframe-<host>`, and without `TS_CONTROL_URL` points it at the default
    Tailscale control plane. `logout` does the same and may tell the wrong
    control server.
- **`read_authkey_stdin` leaves echo off on Ctrl-C:** `cli.rs:118-143`. Echo is
  cleared, then `read_line` blocks. SIGINT has its default action during
  `login` (`install_quit_signals` runs only in `connect`, `commands.rs:386`), so
  the process dies before `tcsetattr` restores the terminal.

**Licensing:** `ghostframe-server` is AGPL-3.0-or-later (`util.rs:1`), while the
CLI and `ghostframe-tsnet` are MIT OR Apache-2.0. `ci/check-licenses.py` rule 2
forbids a permissive crate depending on an AGPL crate. So the CLI cannot call
the server's helpers. **Chosen:** move `ensure_private_dir` and `StateLock` (with
`LockError`) into `ghostframe-tsnet`, under a new module
`ghostframe-tsnet/src/state_dir.rs` (MIT OR Apache-2.0). `ghostframe-server`
re-imports them from there; an AGPL crate may depend on a permissive one. This
relicenses about 90 lines of server code as permissive. **Owner decision:**
Cedric is the copyright holder, so it is his call. The alternative is a
clean-room CLI copy, which is rejected because it duplicates security-relevant
code. The per-file SPDX header must change with the move (licence-check rule
4).

**Design (chosen):**

1. **State directory.**
   - `state_dir()` ignores a relative `XDG_STATE_HOME` (falls through to
     `HOME`) and also rejects a relative `HOME` with a message.
   - Every subcommand calls `ensure_private_dir(&dir)`. For `connect` and
     `logout` that means validate-only: if the directory is missing, report
     "not logged in", as today.
2. **Lock.** `login`, `connect` and `logout` take `StateLock::acquire(&dir)`
   and hold it for the command's life. A busy lock prints "another ghostframe
   (login/connect) is using <dir>". The server's `LockError::Busy` text names
   systemd, so give the tsnet version a neutral message and keep the server's
   wording in the server.
3. **Persisted login settings.**
   - `login` writes `<state_dir>/ghostframe-login.json`,
     `{ "control_url": "...", "hostname": "..." }`, mode 0600, written to a
     temporary file and renamed, and only after `up()` succeeds.
   - `connect` and `logout` read it and pass both to tsnet: `connect` through
     NC-5's `Config::control_url` and `hostname`; `logout` through
     `GhostbridgeConfig`.
   - A missing file (a state directory from before this change) falls back to
     today's defaults, with an `info` line.
   - `TS_CONTROL_URL` set in the environment still wins, through tsnet's own
     fallback, only when the file has no `control_url`.
   - `connect` gains no `--login-server` flag. There is one source of truth.
   - Rejected: requiring the same flags on every subcommand. That is
     error-prone, and the mismatch is exactly the bug.
4. **Terminal.** A `TermiosGuard` restores the saved termios in `Drop`. For
   signals, during the prompt only, install SIGINT, SIGTERM and SIGHUP
   handlers (`sigaction`, `SA_RESETHAND`) that call `tcsetattr(0, TCSANOW, &SAVED)`
   (async-signal-safe per POSIX) from a `static` holding the saved termios,
   and then `raise` the same signal so the default action and exit status are
   kept. Restore the previous handlers when the guard drops.
   - Rejected: the `rpassword` crate. It restores on drop but not on a signal,
     so it has the same bug.

**Files:**

- `ghostframe-tsnet/src/{lib.rs, state_dir.rs (new)}`
- `ghostframe-server/src/{util.rs, state.rs}` (re-exports and call sites)
- `ghostframe-cli/src/{commands.rs, cli.rs}`
- `ghostframe-cli/tests/cli.rs`

**Tests (no GPU, no network):**

- `state_dir_ignores_relative_xdg_state_home`: refactor `state_dir` to
  `state_dir_from(lookup)`, so no environment mutation is needed. Memory says
  environment tests race unless they use the shared lock, so avoid them.
- tsnet `state_dir::tests`: move the server's existing tests for
  `ensure_private_dir` (mode 0755 refused, symlink refused, other owner
  refused, created as 0700) and `StateLock` (a second acquire is `Busy`) with
  the code. Check `git grep -n ensure_private_dir ghostframe-server/src` for
  their location.
- `login_settings_round_trip_and_mode_0600`: write and read in a tempdir, and
  assert the file mode is `0o600`.
- `connect_uses_persisted_control_url_and_hostname`: extract
  `fn connect_config(dir, settings, opts) -> Config` (shared with NC-6's
  extraction) and assert both fields.
- `termios_restored_on_sigint`: run the prompt in a child process on a pty
  (`libc::openpty`), send SIGINT, then read `tcgetattr` on the master or slave.
  `ECHO` must be set. Mark it `#[ignore]`? No: memory says CI exempts itself in
  the workflow, never through `#[ignore]`. It needs only a pty, which CI
  runners have.

**Mutation checks:**

- Drop the relative-path check. The XDG test fails.
- Remove the signal handler. The pty test fails, with echo still off.
- Make `connect` ignore the settings file. The config test fails.

**Gates:**

- `just ci-client` (licence check, env-read guard).
- `just ci-local`, because `ghostframe-server` imports change. Run it under
  `gf-gpu-run` (memory: `ci-local` touches VA-API).

**Risks:**

- Existing developer state directories have mode 0755 and would now be refused
  ("refused rather than repaired", as on the server). The error message tells
  the user to `chmod 700`, and that is the intended fail-closed behaviour.
- **Owner decisions:** the relicensing move, and whether `connect` should also
  accept `--hostname` to override the persisted one (recommend no).

**Size:** M.

---

## Suggested PRs for NC

The PRs touch `ghostframe-client-native/src/render_thread.rs` and `lib.rs`
repeatedly, so they are **sequential**, not parallel. Merge order:

| # | Branch | Findings | Owns (files) | Depends on | Size |
|---|---|---|---|---|---|
| NC-A | `fix/native-texture-limits` | NC-2 steps 1-2 only (adapter 2D texture limit, `FrameTooLarge` error, GPU test at 2560 and 4K) | `client-gpu/src/wgpu_ctx.rs`, `renderer.rs`, `lib.rs` (`GpuError`) | none | S |
| NC-B | `fix/native-thread-supervision` | NC-1, NC-4, plus NC-8's `WakeFd` and `RenderThreadArgs` (they are the scaffolding NC-1 needs) | `client-native/src/{supervise.rs, wake.rs, lib.rs, net_thread.rs, decode_thread.rs, render_thread.rs, event.rs}` | NC-A | M |
| NC-C | `fix/native-gpu-errors` | NC-2 steps 3-8 (uncaptured-error and device-lost handlers, `ring.rs` `expect`, `DeviceLost` made terminal), NC-7 framebuffer `fill` and `read_blocking` | `client-gpu/src/{wgpu_ctx.rs, ring.rs, renderer.rs}`, `ghostframe-render/src/framebuffer.rs`, `client-native/src/render_thread.rs` | NC-B | M |
| NC-D | `fix/native-event-queue-bounds` | NC-3 | `client-native/src/{event.rs, net_thread.rs, decode_thread.rs, render_thread.rs}` | NC-B | M |
| NC-E | `refactor/client-gpu-options` | NC-6, NC-5 step 5 (`Config::control_url`, delete the `TS_CONTROL_URL` read and its env-guard exemption) | `client-gpu/src/config.rs` + constructors, `client-native/src/lib.rs` (`Config`), `ghostframe-cli/src/commands.rs` (`Config` literal, `set_var` removal), `ci/check-env-reads.sh` | NC-C | M |
| NC-F | `feat/capi-abi-v1` | NC-5 (the rest) | `ghostframe-client-capi/**`, `client-gpu/src/{ring.rs, export.rs, export_gles.rs}` (`drm_format`), `ghostframe-cli/src/window/{wayland.rs, x11.rs}` | NC-E | M |
| NC-G | `fix/cli-state-dir` | NC-10 | `ghostframe-tsnet/src/state_dir.rs`, `ghostframe-server/src/{util.rs, state.rs}`, `ghostframe-cli/src/{commands.rs, cli.rs}` | NC-E (uses `Config::control_url`); owner OK on the relicence | M |
| NC-H | `chore/client-hygiene` | NC-7 (`testdata` gate), NC-8 (the rest: `DecodeErrorCode` call sites, `export.rs` `allow`s, `#[non_exhaustive]` on `ClientError`, `prevalidate.ts` references, client-net wasm CI line per the owner's choice), NC-9 confirmation | `client-gpu/src/lib.rs` + Cargo.toml, `client-core/src/{event.rs, pal_rle_decode.rs}`, `ghostframe-render/src/tile_renderer.rs`, `client-wasm/src/app/client.rs`, `.github/workflows/client-core.yml` | WP's `TryFrom` decision; any time after NC-C | S-M |

**Cross-group touch points:**

- `ghostframe-client-core/src/event.rs` (`DecodeErrorCode`) and the HELLO
  display clamp (NC-2 step 3, optional) touch group WP.
- `ghostframe-server/src/{util.rs, state.rs}` (NC-G) touches whichever group
  owns server state.
- `.github/workflows/client-core.yml` (NC-H) touches CI ownership.
- Every NC PR runs `gf-gpu-run just ci-client`. NC-E, NC-F and NC-G also run
  `gf-gpu-run just ci-local`, because `Config` or server code changes.
- GPU tests are always serialised through `gf-gpu-run`, never run in parallel
  (memory: parallel GPU load reset amdgpu).
