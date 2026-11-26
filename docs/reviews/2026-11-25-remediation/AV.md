# AV: server availability and input injection

Verified against `origin/master` at `e85fb005` (worktree
`ghostframe-review-2026-11-25`). Line numbers are for that tree. Every finding
below was re-read in the source. None has been fixed since the original
review, though several line numbers have moved.

Threat model, stated once: every actor here is an **admitted** peer, meaning
one the tailnet owner allowlist accepts (`transport::peer_auth`). Non-live
connections are already gated: `dispatch_feedback_bytes` drops every
non-HELLO byte from a non-live sender (`io_bridge.rs:2020-2037`), and
`dispatch_inbound_datagram` does the same for datagrams
(`ack_dispatch.rs:754-`). These findings therefore matter more for robustness
against a buggy or malicious *own* client than as a security boundary. AV-7's
Go half is the exception: any tailnet node can reach it, including one that
is not allowed in.

---

## AV-1: client tile coordinates index the metrics grid unchecked (High)

**Status at e85fb005: CONFIRMED, and worse than reported.**

- `ghostframe-lib/src/transport/io_bridge.rs:2381-2392`,
  `recover_from_decode_error`, calls
  `self.metrics_tracker.get(msg.tile_x as u32, msg.tile_y as u32)`.
- `ghostframe-lib/src/tile/metrics_tracker.rs:51-68`: `get` indexes
  `self.metrics[self.idx(..)]`, and `idx` has only a `debug_assert!` on
  bounds.
- The grid starts empty: `io_bridge.rs:1520` builds
  `MetricsTracker::new(0, 0)`, and the grid is sized on the first analysed
  frame (`frame.rs:456`, `frame.rs:1451`). Until then **any** code-3
  DECODE_ERROR panics, even with in-range coordinates (`metrics[0]` on an
  empty Vec). This is not only the `tile_y=255` case.
- In a release build, the Vec bounds check panics when `tile_y*cols + tile_x
  >= len`. When the coordinates are out of grid but the flat index is below
  `len` (for example `tile_x=255, tile_y=0` on a 120-column 4K grid), nothing
  panics: the error **silently aliases another tile**, and the wrong palette
  is rebundled.
- Entry point: `dispatch_feedback_bytes` → `DECODE_ERROR_MSG_TYPE` arm
  (`io_bridge.rs:2064-2073`) → `handle_decode_error` (`io_bridge.rs:2295-2320`).
  Only `error_code == ERR_THIN_UNCACHED_PALETTE` (3) reaches the index.
- Deferred replay: `handle_decode_error` pushes onto `deferred_decode_errors`
  (`io_bridge.rs:2315`, field at `:820`) while the tile is in flight.
  `on_frame_worker_message` replays them into the same unchecked
  `recover_from_decode_error` (`frame.rs:912-916`). The Vec is **unbounded**
  (one push per 5-byte message while a frame is in flight). It is also not
  cleared by `fire_session_reset` or `forget_session` (`session.rs:150-222`,
  `:408-430`), so a departed client's error replays into the next session.
- `tile_is_in_flight` (`frame.rs:921-927`) is a linear `dirty_xy.contains`
  for each message: up to 8160 compares per message at 4K. A 64 KiB chunk of
  DECODE_ERRORs costs about 10^8 compares on the loop.
- Other client-coordinate sites, all checked and **safe**:
  - TILE_NACK (`ack_dispatch.rs:659-712`) goes only to hash and Option
    lookups: `block_table.tile_protected` (`fec_tx/block_table.rs:955`, a
    map), `reliable_emitter.holds_pass` / `on_nack` (`lookup_content`, a
    map), and `scheduler.refinement_queue_holds_tile` (`scheduler.rs:738`,
    via `slots.handle`, whose `index()` at `scheduler/slots.rs:78-83` returns
    `Option`).
  - ACK entries name a `wire_seq` and are resolved through the ledger. The
    coordinates come from server records (`apply_ack_coverage`,
    `ack_dispatch.rs:542-`).
  - The reset request (0x09) has no coordinates, and DisplayMode is clamped.
  - DisplayInfo `scale_milli=0` is guarded in `mm_for_scale`
    (`display.rs:1101-1107`).
  - The other `metrics_tracker.get` callers in `emit.rs` and `frame.rs` take
    server-generated coordinates.

**Root cause.** A wire-supplied `u8` pair is treated as a trusted grid
coordinate, and the only bounds check is a debug assertion.

**Design (chosen).** Validate at the trust boundary and make the accessor
total:

- Add `MetricsTracker::try_get(&self, x, y) -> Option<&TileMetrics>`, which
  returns `None` when `x >= cols || y >= rows`.
- In `handle_decode_error`, drop an out-of-grid error **before** any deferral.
  It is counted (`decode_errors_out_of_grid`) and logged under the AV-7 rate
  limit.
- `recover_from_decode_error` uses `try_get`, so a replay after a grid change
  is also safe.
- Turn `deferred_decode_errors` into a set of `(u8, u8)`. This deduplicates
  it and bounds it by the grid size. Clear it on session reset and on
  live-session forget.

Rejected alternatives:

- Replace `debug_assert!` with `assert!` in `idx`. That still panics, and the
  panic is the bug.
- Clamp the coordinates into the grid. That silently acts on a tile the
  client did not name, which is the aliasing bug made official.

**Files:** `ghostframe-lib/src/tile/metrics_tracker.rs`,
`ghostframe-lib/src/transport/io_bridge.rs`,
`ghostframe-lib/src/transport/io_bridge/frame.rs`,
`ghostframe-lib/src/transport/io_bridge/session.rs`,
`ghostframe-lib/src/transport/io_bridge/tests.rs`.

**Steps**

1. `metrics_tracker.rs`: add `pub fn try_get(&self, tile_x: u32, tile_y: u32)
   -> Option<&TileMetrics>` (bounds check first, then
   `self.metrics.get(idx)`). Leave `get`, `get_mut` and `idx` unchanged.
   Their callers pass server coordinates.
2. `io_bridge.rs`: add a `decode_errors_out_of_grid: u64` field next to
   `non_live_feedback_drops` (`:544`) and initialise it to 0 in the
   constructor near `:1503`.
3. `handle_decode_error` (`:2295`): after the existing warn (which AV-7 will
   rate-limit), first check
   `(msg.tile_x as u32) >= self.metrics_tracker.cols() || (msg.tile_y as u32)
   >= self.metrics_tracker.rows()`. If it is out of grid, increment the
   counter, emit a `debug!` (with `tile_x`, `tile_y`, `cols`, `rows`, and the
   total), and `return`. Do this for every error code, not only 3, so the
   counter is meaningful.
4. Change `deferred_decode_errors: Vec<DecodeErrorMsg>` to
   `deferred_decode_errors: std::collections::BTreeSet<(u8, u8)>` and insert
   `(msg.tile_x, msg.tile_y)`. Change `recover_from_decode_error` to take
   `(tile_x: u8, tile_y: u8)`, and change the replay loop at `frame.rs:913`
   to iterate the set. The test at `tests.rs:1648/1655` uses
   `.len()`/`.is_empty()` and still compiles.
5. `recover_from_decode_error`: `let Some(m) = self.metrics_tracker.try_get(x
   as u32, y as u32) else { self.decode_errors_out_of_grid += 1; return; };`.
   This branch is reachable when the grid changed between the deferral and
   the replay.
6. `session.rs`: in `fire_session_reset`, after `self.metrics_tracker.reset()`
   (`:159-160`), call `self.deferred_decode_errors.clear()`. In
   `forget_session`, inside the existing `if self.last_hello_from ==
   Some(handle)` block (`:409-412`), do the same.
7. Optional, same PR: add `decode_errors_out_of_grid` to the cumulative emit
   line, if the instruments struct makes that a one-field change. Otherwise
   leave it on the bridge and read it in tests.

**Tests** (unit, `ghostframe-lib/src/transport/io_bridge/tests.rs`; the
fixtures `make_bridge_for_test`, `make_live`, and the existing test at
`:2825-2840`, which shows how to seed a PalRle tile):

- `decode_error_before_the_first_frame_is_dropped_not_a_panic`. Use a fresh
  bridge (0x0 grid) with `make_live(h)`. Call
  `dispatch_feedback_bytes(h, &[0x04, 2, 0, 0, 3])`. Assert no panic and
  `decode_errors_out_of_grid == 1`.
- `decode_error_never_panics_and_never_aliases_for_any_coordinate`. This is
  an oracle sweep, not a sample.
  - Set up `metrics_tracker.resize(64, 8)` and mark **every** in-grid tile
    `PalRle { palette_id: 5 }`.
  - For every `(x, y)` in `0..=255 × 0..=255`: re-insert 5 into
    `palette_table.delivered`, call `handle_decode_error` with code 3, then
    assert the property: `delivered.contains(5)` was cleared **iff** `x < 64
    && y < 8`.
  - Finally assert `decode_errors_out_of_grid == 65536 - 512`.
  - This catches both the panic (debug) and the aliasing (`(255, 0)` →
    flat 255 → tile (63, 3)).
- `deferred_decode_errors_are_deduplicated_and_cleared_by_a_session_reset`.
  Put tile (1, 1) in flight (as at `:1630`) and send the same error 1000
  times. Assert `deferred_decode_errors.len() == 1`. Then call
  `fire_session_reset` and assert the set is empty.
- **Mutation check:**
  - Delete the step-3 grid check. The first two tests fail: the 0x0 test
    panics at `metrics_tracker.rs:61` (`debug_assert`), and the sweep fails
    on the first out-of-grid coordinate.
  - Revert step 5 to `get` and delete step 3. The sweep still fails. With
    step 3 alone it passes, which proves step 3 is the load-bearing part.
  - Revert step 4 to a Vec. The dedup test fails with `len == 1000`.

**Gates:** `just lint`, `just test-unit`, `gf-gpu-run just ci-local` before
the push. No container rebuild is needed: the e2e tests that grep `client
decode error` (`e2e.rs:1234`, `:3729`) keep working because the warn line
stays.

**Risks.** The bench parses `client decode error` lines
(`ghostframe-e2e/src/harness/scene.rs:788`) for `decode_error_count`. This
finding leaves that line in place. AV-7 must keep it (see there).

**Size:** S.

---

## AV-2: a dead IoBridge exits 0, so systemd does not restart the daemon (High; pairs with AV-1)

**Status at e85fb005: CONFIRMED.**

- `ghostframe-lib/src/server.rs:169-173` spawns the IoBridge task, and only
  an `Err` from `bridge.run()` is logged. The `JoinHandle` is stored as
  `_io_task` (`:89`) and never awaited, so a panic is visible only through
  tokio's default panic print.
- When the task ends (panic, Err, or ghostbridge EOF, which `run` returns as
  `Ok(())` at `io_bridge.rs:2430-2436`), the bridge is dropped. That closes
  `frame_rx` and the `connected_session_count` watch sender.
- `ghostframe-server/src/commands/capture.rs`:
  - `:263-271`: `wait_for_client` returns `Err(ServerShutDown)`, which warns
    and `break`s. The comment even says "the process exit code is
    unchanged".
  - `:369-376`: a `submit_frame` Err warns and `break`s.
  - `:450-451`: `warn!("capture loop exited"); Ok(())`.
- `main.rs:66`: `Command::Capture => return commands::capture::run(cfg).await`.
  `Ok` → exit 0.
- Unit: `Restart=on-failure` (`templates/ghostframe-server.service.in`), so
  exit 0 is **not** restarted.
- Combined with AV-1, one 5-byte message from the live client takes the
  daemon down until someone restarts it by hand.
- Related hang: the backpressure loop (`capture.rs:304-316`) waits on
  `pool.wait_free()` forever with a warning every 5 s. If the bridge dies
  while a frame slot is held by something that outlives it, the loop never
  reaches either `break`. Slots are released on Drop (`frame_pool.rs:242`,
  `:331`), so this is likely fine, but it is unverified.

**Root cause.** "The server is gone" is treated as a normal end of the
loop, although nothing but a fault can cause it: shutdown is by SIGTERM,
and there is no in-process signal handling (`util.rs:62` only ignores
SIGHUP).

**Design (chosen).**

1. Make clean termination of `capture::run` unrepresentable. The loop
   `break`s with a reason value, and `run` returns
   `Result<std::convert::Infallible, Box<dyn Error + Send + Sync>>`. Returning
   `Ok(())` then becomes a compile error, which is the strongest test
   available for code that needs a tailnet to reach.
2. `GhostframeServer` supervises its own IoBridge task. A second small task
   awaits the `JoinHandle` and publishes an `IoExit` (`Eof` /
   `Error(String)` / `Panicked(String)`) on a `tokio::sync::watch`. Add
   `pub async fn closed(&self) -> IoExit`. `capture.rs` awaits it with a
   short timeout to name the cause in its error, and `select!`s on it inside
   the `wait_free` wait so a leaked slot cannot strand a dead daemon.

Rejected alternatives:

- `panic = "abort"` in the release profile. It is workspace-wide and also
  changes the FFI library (`ffi.rs`) and the native client. It turns a panic
  in *any* spawned task into a process abort, and it still leaves the
  EOF/Err paths at exit 0.
- A global panic hook that calls `process::exit(1)`. It kills the process
  from inside a panicking worker without running destructors, so AV-4's
  release-on-drop never runs. It also only covers panics, not EOF/Err.

**Files:** `ghostframe-lib/src/server.rs`,
`ghostframe-server/src/commands/capture.rs`, `ghostframe-server/src/main.rs`.
Optionally `ghostframe-lib/src/ffi.rs`, only if it pattern-matches
`ServerShutDown` (it does not today; it only checks `is_ok()` at `:151`).

**Steps**

1. `server.rs`: add the enum (sketch, names are free):
   ```rust
   // sketch
   #[derive(Clone, Debug, PartialEq, Eq)]
   pub enum IoExit { Eof, Error(String), Panicked(String) }
   ```
   Add a field `io_exit: watch::Receiver<Option<IoExit>>`.
2. Change the spawn at `:169` so the IoBridge task returns the `run()`
   result unchanged:
   `let io_task = tokio::spawn(async move { bridge.run().await.map_err(|e|
   e.to_string()) });`. Then spawn a supervisor through a free function
   `async fn supervise_io(task: JoinHandle<Result<(), String>>, tx:
   watch::Sender<Option<IoExit>>)`. It maps `Ok(Ok(()))` → `Eof`,
   `Ok(Err(e))` → `Error(e)`, and `Err(j) if j.is_panic()` → `Panicked(..)`
   (downcast the payload to `&str`/`String`, else `"<non-string panic>"`).
   It `error!`-logs every non-`Eof` outcome, warn-logs `Eof`, and sends.
   Keep the supervisor's `JoinHandle` in the existing `_io_task` slot. The
   `Err` log now in the spawned closure moves into `supervise_io`.
3. Add `pub async fn closed(&self) -> IoExit`, which clones the receiver and
   waits until the value is `Some`. If the sender is dropped without a
   value, return `IoExit::Error("supervisor gone".into())`.
4. `capture.rs`: define `enum LoopEnd { ServerGone(&'static str) }` (the
   `&'static str` names which await saw it). Change `loop { .. }` to
   `let end = loop { .. };`, and change both `break;` (`:270`, `:375`) to
   `break LoopEnd::ServerGone("wait_for_client")` /
   `break LoopEnd::ServerGone("submit_frame")`.
5. After the loop:
   `let why = tokio::time::timeout(Duration::from_secs(1),
   server.closed()).await;`. Then
   `return Err(format!("the server event loop ended ({where}): {why:?};
   exiting non-zero so the service manager restarts us").into())`. Remove
   `Ok(())` and change the signature to
   `Result<std::convert::Infallible, Box<dyn std::error::Error + Send +
   Sync>>`. The early `return Err(...)` at `:432-436` already fits.
6. Backpressure wait (`:304-316`): replace the `timeout(.., pool.wait_free())`
   loop body with a `tokio::select!` over `pool.wait_free()`,
   `server.closed()` (on which you `break LoopEnd::ServerGone("frame
   slot")`), and a 5 s sleep that keeps the existing warning.
7. `main.rs:66`: `Command::Capture => return commands::capture::run(cfg).await.map(|n| match n {}),`.
8. Update the comment at `capture.rs:263-268` ("exit code is unchanged") to
   say the opposite.

**Tests**

- The compile-time property: `run` returns `Result<Infallible, _>`. The
  mutation check is to add `Ok(())` back at the end of `run`. It must fail to
  compile with a type mismatch. State this in the doc comment on `run`.
- Unit, `ghostframe-lib/src/server.rs` `mod tests`:
  - `supervise_io_reports_a_panic`: `tokio::spawn(async { panic!("boom") })`
    → `supervise_io` → the receiver sees `Some(IoExit::Panicked(s))` with
    `s.contains("boom")`.
  - `supervise_io_reports_eof_and_error`: the same with `Ok(())` and
    `Err("x".into())`.
  - Mutation: map every outcome to `Eof` in `supervise_io`. The panic and
    error tests fail.
- Unit, `server.rs`: `closed_resolves_when_the_supervisor_publishes`. Drive
  the `watch` by hand, with the same no-op waker pattern as
  `wait_for_nonzero_is_pending_until_the_count_changes` (`:345`): it is
  pending before the send and ready after it.
- Not testable without a tailnet: the full `capture` → exit-1 path. Say so
  in the PR body. `tests/commands.rs` cannot get past `GhostframeServer::new`
  (it points `TS_CONTROL_URL` at `127.0.0.1:1`). A live check for the owner:
  apply AV-1's reverse (a debug build) or `kill -STOP` ghostbridge, then
  confirm `systemctl --user status ghostframe-server` shows a restart.

**Gates:** `just lint`, `just test-unit`, `cargo test -p ghostframe-server
--test commands`, and `gf-gpu-run just ci-local`.

**Restart policy (owner decision D6, 2026-11-25).** The operator is usually
remote, so a unit in `failed` locks them out. The unit must never give up
while the session exists, and must back off instead. Part of this PR:

9. `templates/ghostframe-server.service.in`:
   - `[Unit]`: replace `StartLimitIntervalSec=60` and `StartLimitBurst=3`
     with `StartLimitIntervalSec=0`.
   - `[Service]`: keep `Restart=on-failure`, `RestartPreventExitStatus=2` and
     `RestartSec=2`, and add `RestartSteps=6` and `RestartMaxDelaySec=60s`.
     The delay grows from 2 s to 60 s over six restarts and resets once a
     start stays up.
   - Add a comment saying why (remote operator; exit 2 is the only permanent
     failure).
10. systemd older than 254 does not know the two `Restart*` keys. It should
    log "Unknown key name ... ignoring" and keep a fixed 2 s delay with no
    start limit. That is acceptable, but it is **UNVERIFIED**: check it once
    with `systemd-analyze verify` on a 252 system (Debian 12) before relying
    on it.

**Tests for the policy.** Unit test in `install.rs`,
`unit_template_never_stops_restarting_and_backs_off`, on the rendered unit:
- it contains `StartLimitIntervalSec=0`, `RestartSteps=6`,
  `RestartMaxDelaySec=60s`, `Restart=on-failure` and
  `RestartPreventExitStatus=2`;
- it contains no `StartLimitBurst`.

Mutation: put `StartLimitBurst=3` back, and the test fails. A live check for
the owner: run `systemctl --user kill -s SEGV ghostframe-server` four times
and read the growing delays from the journal timestamps.

**Size:** M.

---

## AV-3: input injection blocks the IoBridge loop; wheel magnitude is uncapped (Medium)

**Status at e85fb005: CONFIRMED, with all three sub-parts.**

- `ghostframe-server/src/capture/x11/input.rs`:
  - `:111-129` `fake_input`: every event sends `xtest_fake_input`, then
    calls `cookie.check()`, a **synchronous round trip** (x11rb 0.13.2
    `VoidCookie::check` → `check_for_error`), then `flush()`.
  - `:144-155` `pointer_button`: two round trips (a move, then the button).
  - `:157-183` `wheel`: `click_pair(4, dy.unsigned_abs())` runs `|dy|`
    press/release pairs. `dy = i16::MIN` gives 32768 pairs = **65536 round
    trips**, plus up to 65536 more for `dx`, all on the event loop.
- It is called synchronously from `dispatch_feedback_bytes` → INPUT arm →
  `apply_input` (`ghostframe-lib/src/transport/io_bridge.rs:2155-2192`;
  `input_inject.rs:139-149`).
- Native client Wayland: `ghostframe-cli/src/window/wayland.rs:678-688`
  sends `axis_delta(horizontal.absolute)` (`:696-698`), which is **pixels**
  (sctk 0.21.1 `AxisScroll::absolute`: "measured in pixels"). One mouse
  notch is about 10-15 px, so it becomes 10-15 X wheel clicks. The X11
  backend (`window/x11.rs:134-137`) and the web client
  (`ghostframe-client-wasm/src/app/input_map.rs:45-58`, `wheel_ticks`) send
  ±1.
- RandR on the loop: `display_mode.rs:96` `ctl.set_output(..)` runs from
  `on_timeout`. `XrandrDisplay::set_output` (`display.rs:423-`) does several
  synchronous requests (`randr_get_screen_resources(..).reply()`, ...) and
  may `grab_server` (`:534`). It is bounded by the 250 ms debounce, which
  restarts on every request, so a flood never applies. Separately,
  **`ctl.ceiling()` runs on every DisplayMode message** (`io_bridge.rs:2124`
  → `display.rs:406-421`, `query_ceiling` = a synchronous
  `GetScreenSizeRange` round trip), with no debounce.

**Root cause.** The injector was written as "one protocol call per event,
check every error" with no queue between the network loop and the X
connection, and it trusts the wire's wheel magnitude.

**Design (chosen).**

1. **Clamp at the library boundary.** `apply_input` clamps wheel `dx`/`dy`
   to `±MAX_WHEEL_NOTCHES_PER_EVENT` (`pub(crate) const`, 16; not `pub`,
   because of cbindgen). Every backend is then protected, and the clamp can
   be unit-tested with the existing `MockInjector` (`input_inject.rs:268-303`).
2. **The XTest injector becomes a handle to a worker thread.**
   `XTestInjector` keeps its type and its `InputInjector` impl (so
   `X11Backend`, `capture/x11/mod.rs:110-122`, is unchanged), but holds only
   a `std::sync::mpsc::SyncSender<Cmd>` (capacity 4096). The worker thread
   owns the `RustConnection`. For each batch it:
   - blocks on `recv()`, then drains with `try_recv()` up to 256 commands;
   - issues each request **without** `check()` (it drops the `VoidCookie`,
     so x11rb 0.13.2 delivers any error as an `Event::Error`);
   - calls `flush()` once;
   - drains `poll_for_event()`. `Event::Error` gives a rate-limited warn.
     `Event::MappingNotify` invalidates the keysym cache (that is AV-6).
3. Trait calls use `try_send`. On `Full`, a pointer move is dropped and
   counted (the next move supersedes it). Any other command is dropped and
   counted with a rate-limited warn: by then the X server has stopped
   reading. AV-4 releases what a dropped up-event would have left held.
4. **DisplayMode:** query the ceiling once, when the debounced mode is
   applied, not per message. Store the raw request in `PendingDisplayMode`
   and call `clamp_and_align_mode(.., ctl.ceiling())` in `on_timeout` just
   before `set_output`.
5. **Wayland client:** convert axis events to notches. Accumulate
   `value120` per axis and emit `value120 / 120` with the remainder kept.
   Fall back to `discrete` when `value120 == 0`. For continuous sources
   (both zero) accumulate `absolute` pixels and emit one notch per
   `PX_PER_NOTCH = 10.0`.

Rejected alternatives:

- Only removing `cookie.check()` and keeping injection on the loop. A
  stalled X server still blocks the loop in `flush()` once the socket buffer
  fills, and the X server is not something ghostframe controls (an inactive
  VT, see AGENTS.md).
- `tokio::task::spawn_blocking` per event. That loses ordering between
  events (a key-up could overtake its key-down), and it takes a blocking-pool
  thread per event under a flood.
- Moving `set_output` itself off the loop is deferred. It is rate-bounded by
  the debounce, and moving it means making the `DisplayController` contract
  asynchronous (the read-back worker and the generation ticket in
  `display.rs:423-432` assume a synchronous caller). This is listed as a
  follow-up, not done here.

**Files:**

- `ghostframe-lib/src/transport/input_inject.rs`: the clamp.
- `ghostframe-server/src/capture/x11/input.rs`: the worker (rewrite).
- `ghostframe-lib/src/transport/io_bridge.rs`: the DisplayMode arm,
  `:2112-2140`, and the `PendingDisplayMode` struct.
- `ghostframe-lib/src/transport/io_bridge/display_mode.rs`: `on_timeout`.
- `ghostframe-cli/src/window/wayland.rs`.

**Steps**

1. `input_inject.rs`: `pub(crate) const MAX_WHEEL_NOTCHES_PER_EVENT: i16 =
   16;`. In `apply_input`'s `Wheel` arm:
   `injector.wheel(dx.clamp(-MAX, MAX), dy.clamp(-MAX, MAX))`. Document the
   unit (notches, ±1 per detent) on `InputMsg::Wheel`.
2. `input.rs`:
   - Define `enum Cmd { Move(i16, i16), Button { x: i16, y: i16, button: u8,
     down: bool }, Wheel(i16, i16), Key(u32, bool) }`.
   - `XTestInjector::new` connects and checks XTEST exactly as today
     (`:36-63`). It then spawns a `std::thread::Builder::new().name("xtest-inject".into())`
     worker that owns `conn`, `root`, a `HashMap<u32, u8>` keysym cache (no
     Mutex), and the held-state of AV-4 step 6.
   - It returns `Self { tx, dropped_moves: AtomicU64, dropped_other:
     AtomicU64, done_rx: Mutex<Receiver<()>> }`.
3. The worker loop, as in design point 2. Inside it, `Wheel` is clamped
   again to `MAX` (defence in depth; the lib constant is `pub(crate)`, so
   define a local `const MAX_WHEEL_NOTCHES: u16 = 16` with a comment that
   names the lib constant). `Button` still sends a move first. Requests use
   `self.conn.xtest_fake_input(event, detail, x11rb::CURRENT_TIME, root, x,
   y, 0)`, exactly the call at `:115`, and the cookie is simply dropped.
4. Trait impl: each method `try_send`s. On `TrySendError::Full(Cmd::Move(..))`
   it bumps `dropped_moves`. On any other `Full` it bumps `dropped_other` and
   warns on the first and every 64th, as `io_bridge.rs:2028-2030` does. On
   `Disconnected` it warns once (the worker died: the X connection broke).
5. `Drop for XTestInjector`: drop `tx` (move it into an `Option` and `take()`
   it), then `done_rx.recv_timeout(Duration::from_millis(500))`. **Never
   join unbounded**: a wedged X server must not hang shutdown. The worker
   sends on `done_tx` after its final release flush (AV-4).
6. DisplayMode: in the `DISPLAY_MODE_MSG_TYPE` arm, store
   `PendingDisplayMode { width: msg.width, height: msg.height, deadline_us }`
   unclamped, without calling `ctl.ceiling()`, and keep the
   `display_controller.is_some()` guard. In `on_timeout`, after `take()`,
   compute `let (w, h) = clamp_and_align_mode(width, height, ctl.ceiling());`
   and pass `w, h` to `set_output`. Move the "display mode requested;
   debounced" debug fields to log the raw request, and log the applied
   values in the existing "display mode applied" info line.
7. `wayland.rs`:
   - Add `struct WheelAccum { v120: [i32; 2], px: [f64; 2] }` to `AppData`
     and a pure `fn notches(acc: &mut WheelAccum, axis: usize, s:
     &AxisScroll) -> i16`. It prefers `value120`, then `discrete * 120`, then
     `absolute` (as px / `PX_PER_NOTCH`). It keeps the remainder, and resets
     the axis accumulator when `s.stop` is true. Its output is clamped to
     `±16`.
   - Replace `axis_delta(..)` at `:684-685` and skip the push when both are
     0. Delete `axis_delta` and its test (`:827-831`), or keep it if
     something else uses it (grep).

**Tests**

- Unit, `input_inject.rs` tests: `wheel_is_clamped_to_the_notch_cap`. Feed
  `apply_input(&m, Wheel { dx: i16::MIN, dy: i16::MAX })` and assert
  `calls == [Wheel(-16, 16)]`. Also sweep `dy` over every `i16` and assert
  `|recorded| <= 16 && sign(recorded) == sign(dy)` (a property, not a
  number). Mutation: remove the clamp, and the test fails on the first
  out-of-range value.
- Unit, `ghostframe-server` (`--bins`, under xvfb as CI runs it,
  `ci.yml:161`; use `test_support::PrivateXvfb`):
  - `injector_calls_return_without_waiting_for_the_x_server`. Spawn a
    private Xvfb and create the injector. Send `SIGSTOP` to the Xvfb pid
    (PrivateXvfb must expose it, so check `test_support.rs:13-45`; add a
    `pid()` accessor if it is missing). Issue 10 000 `pointer_move` and 100
    `key` calls, assert they all return (the test finishes) within a
    generous 5 s bound, then `SIGCONT`. This is the discriminating property:
    with the old `check()` the first call blocks forever.
    - Use `Instant` with `#[allow(clippy::disallowed_methods, reason =
      "...")]`, as `capture.rs:364` does.
    - Mutation: restore `cookie.check()` in the worker's request path. That
      only blocks the worker, so the test must target the **trait call**:
      restoring synchronous injection (calling `fake_input` from the trait
      method) must make it hang and hit the bound.
  - `wheel_of_minus_32768_sends_at_most_the_cap`. A second connection
    selects `BUTTON_PRESS` on the root of an empty Xvfb. Call
    `injector.wheel(0, i16::MIN)` and drop the injector (Drop waits for the
    flush). Count `ButtonPress` events with detail 4: assert `1 <= n <= 16`.
    Mutation: remove the worker clamp, and `n` becomes 32768.
- Unit, `wayland.rs` tests: `notches_from_value120_keep_the_remainder`. Three
  events of `value120 = 40` give `0, 0, 1`. `pixel_scroll_becomes_notches`:
  25 px then 5 px give `2, 1`. `stop_resets_the_remainder`. Mutation: return
  `absolute.round()` and every test fails.
- Unit, io_bridge `tests.rs`: `display_mode_requests_query_the_ceiling_once_per_apply`.
  Use the existing `RecordingController` (`tests.rs:4593-`), adding a
  `ceiling_calls: AtomicUsize` if it lacks one. Dispatch 50 DisplayMode
  messages, then fire `on_timeout` past the deadline. Assert
  `ceiling_calls == 1` and that `set_output` received the clamped size.
  Mutation: put `ctl.ceiling()` back in the dispatch arm, and the count is
  51.
- e2e, existing: `e2e_input_forwarding_chromium` / `_firefox`
  (`e2e.rs:4662-4751`) must stay green. They assert the `input event
  dispatched` trace lines, which are unchanged.

**Gates:** `just lint`, `just test-unit` (run the `ghostframe-server` part
under `xvfb-run -a`, as CI does), `just test-client` (wayland.rs),
`just containers-build`, then `just test-e2e` filtered to
`e2e_input_forwarding`, and `gf-gpu-run just ci-local`.

**Risks / owner decisions.**

- Dropping a non-move command under a full queue is a deliberate loss. The
  alternative (blocking) reintroduces the bug.
- `PX_PER_NOTCH = 10.0` is the libinput/mutter wheel convention. Check it on
  a real touchpad: scroll speed on continuous sources will change.
- The SIGSTOP test needs `libc::kill` in a test (`libc` is already a
  ghostframe-server dependency: `util.rs:62`).

**Size:** M (the worker is most of it).

---

## AV-4: held keys and buttons are never released when a session ends (Medium)

**Status at e85fb005: CONFIRMED.**

- Neither `InputInjector` (`input_inject.rs:65-70`) nor anything in
  `session.rs` tracks or releases input. `forget_session`
  (`session.rs:408-430`) clears transport and session maps only.
- Every session end goes through `forget_session`:
  - `on_connection_gone` (`:391`), which `revoke_session` uses (`:685`);
  - `evict_session` (`:623`);
  - `sweep_close_deadlines` (`:744`);
  - `on_connection_established` (`:477`, for a fresh handle).
- **Subtlety an implementer will get wrong:** on a takeover,
  `apply_hello` sets `self.last_hello_from = Some(from)` (`:287`) **before**
  `evict_session(incumbent)` (`:316-318`). So inside the incumbent's
  `forget_session`, `last_hello_from == Some(handle)` is false. A release
  gated on "was this handle live" inside `forget_session` alone misses the
  takeover case.
- Effect: a client that dies with Ctrl/Shift/Super or button 1 down leaves
  it held on the operator's real X session (attach mode). XTest presses are
  not released when the XTest client disconnects either, so process exit
  does not fix it.

**Root cause.** Input is applied statelessly. Nobody owns "what this session
currently holds down".

**Design (chosen).** There are two layers.

- **Library:** a `HeldInput` tracker in `input_inject.rs`. IoBridge `note`s
  each event it applies, and `release_all`s through the injector whenever the
  live session changes: in `forget_session` when the handle is live, **and**
  in `apply_hello` on a takeover, before `last_hello_from` changes. This
  works for any backend and is unit-testable with `MockInjector`.
- **Server:** the AV-3 worker also tracks the X **keycodes** and buttons it
  pressed, and releases them when its channel disconnects (the process is
  exiting or the bridge was dropped). That covers AV-2's death path, where
  no session-end hook runs.

Rejected alternatives:

- Only the server-side keycode tracker with a new `release_all()` trait
  method. That works, but it puts session policy in the backend and needs a
  real X server to test the session logic.
- Releasing every keycode 8..=255 on session end ("release everything").
  That sends about 250 KeyRelease events to the operator's desktop each
  time, and it would release keys the operator is physically holding.

**Files:** `ghostframe-lib/src/transport/input_inject.rs`,
`ghostframe-lib/src/transport/io_bridge.rs` (field, dispatch arm),
`ghostframe-lib/src/transport/io_bridge/session.rs`,
`ghostframe-server/src/capture/x11/input.rs` (worker; same PR as AV-3 or
after it), and the tests.

**Steps**

1. `input_inject.rs`, sketch:
   ```rust
   // sketch
   #[derive(Default)]
   pub(crate) struct HeldInput {
       keys: std::collections::BTreeSet<u32>, // keysyms currently down
       buttons: [bool; 256],
       last_xy: (i16, i16),
   }
   impl HeldInput {
       pub(crate) fn note(&mut self, msg: &InputMsg) { /* KeyDown insert (cap 256, ignore beyond),
           KeyUp remove, PointerButton set/clear, PointerMove/Button update last_xy */ }
       pub(crate) fn release_all(&mut self, injector: &dyn InputInjector) { /* KeyUp each key,
           then pointer_button(last_xy, b, false) for each held b; then clear */ }
       pub(crate) fn is_empty(&self) -> bool { .. }
   }
   ```
   Keys are released before buttons (a modifier released before the
   button-up avoids a Ctrl-click when the button is released).
2. `io_bridge.rs`: add a `held_input: crate::transport::input_inject::HeldInput`
   field (`Default` in the constructor). In the INPUT arm (`:2174-2183`),
   after `apply_input(injector.as_ref(), &msg)`, call
   `self.held_input.note(&msg)`. Note only when an injector exists, so there
   is nothing to release without one.
3. Add `fn release_held_input(&mut self)` in `session.rs`. If
   `!self.held_input.is_empty()` and `self.input_injector` is `Some`, call
   `release_all`, then `info!` the count released (target `LOG_TARGET`,
   once per session end, not per key; never log the keysyms, for the reason
   at `io_bridge.rs:2176-2179`).
4. `forget_session` (`session.rs:408`): first line,
   `if self.last_hello_from == Some(handle) { self.release_held_input(); }`.
5. `apply_hello`: just before `self.last_hello_from = Some(from);` (`:287`),
   add `if incumbent.is_some_and(|i| i != from) { self.release_held_input(); }`.
   That point is after the rate-limit early return, so a refused takeover
   releases nothing.
6. Server worker (with AV-3): track `held_keycodes: [bool; 256]` and
   `held_buttons: [bool; 256]` as requests are issued. When `recv()` returns
   `Err(RecvError)`, send release requests for each, `flush()`, then
   `done_tx.send(())`.

**Tests**

- Unit, `input_inject.rs`, `held_input_release_leaves_nothing_down`. This is
  a property over a scripted random sequence: use a fixed-seed LCG, since
  proptest is fine if already a dev-dep (check `ghostframe-lib/Cargo.toml`).
  Feed 1000 random Key/Button down/up events to both `MockInjector` (via
  `apply_input`) and `HeldInput::note`, then call `release_all`. Replay
  `calls()` into a net-state map and assert every key and button is up, and
  that **no up was sent for something not down** (no spurious releases).
- Unit, io_bridge `tests.rs` (fixtures `test_bridge_with_sessions`, `hello`,
  and the injector pattern at `:3293-3320`):
  - `live_session_loss_releases_held_input`. Make A live via `apply_hello`,
    dispatch KeyDown `0xffe3` (Control_L) and button 1 down, call
    `on_connection_gone(A, "test")`, and assert the recorder ends with
    `Key(0xffe3,false)` and `PointerButton(_,_,1,false)`.
  - `takeover_releases_the_incumbents_held_input`. A is live and holds
    Shift. `apply_hello(B)` (the first takeover is not rate-limited). Assert
    Shift was released. **This test is load-bearing for the ordering
    subtlety.**
  - `non_live_connection_loss_releases_nothing`. A is live and holds a key.
    `on_connection_gone(C)` for a never-HELLO'd C must record no
    calls.
  - `refused_takeover_releases_nothing`. Reuse the setup of
    `a_second_takeover_inside_the_interval_is_refused_and_the_newcomer_closed`
    (`:5342`): the incumbent's held key must stay down.
- Unit, `ghostframe-server` under xvfb:
  `dropping_the_injector_releases_pressed_keys`. Inject KeyDown of a mapped
  keysym (`0x61`), drop the injector (Drop waits on `done_rx`). On a second
  connection, `query_keymap()` must show that keycode up.
- **Mutation checks:**
  - Delete step 5: only the takeover test fails, proving it is separately
    load-bearing.
  - Delete step 4: the loss test fails.
  - Delete step 6: the xvfb test fails, with the keycode bit still set.

**Gates:** `just lint`, `just test-unit` (xvfb for `--bins`), and
`gf-gpu-run just ci-local`. The e2e input tests must stay green.

**Risks.** A modifier the *operator* physically holds while a remote session
ends is not affected: only keysyms the remote pressed are released. Releasing
a keysym the remote pressed but the operator is also holding releases the
operator's too. That is unavoidable at the XTest level, and acceptable.

**Size:** M (S without the server half).

---

## AV-5: a slow X server is a permanent failure; DISPLAY is hard-coded to :0 (Medium)

**Status at e85fb005: CONFIRMED.**

- `ghostframe-server/src/commands/capture.rs:84-97`: a
  `wait_for_x11(Duration::from_secs(10), ..)` failure → `std::process::exit(2)`.
- `templates/ghostframe-server.service.in`: `RestartPreventExitStatus=2`, so
  it is never retried.
- This contradicts `backend_connect_exit_code` in the same file (`:15-26`):
  "a refused connection, an X server still starting may clear up, so it
  stays a plain failure the unit retries". The unit test at `:564-573`
  asserts exactly that rule for the backend connect, but the wait path
  bypasses it.
- `wait_for_x11` itself is at `capture/x11/mod.rs:286-310`.
- The template has `Environment=DISPLAY=:0`. A unit `Environment=` overrides
  the user manager's imported `DISPLAY`, so a session on `:1` (a second
  seat, or a DM that starts at `:1`) is never captured. It times out, exits
  2, and stays dead.
- The test at `install.rs:1208` pins `"\nEnvironment=DISPLAY=:0\n"`.
- `capture.rs:30-32` documents the hard-coding.

**Root cause.** "X is not up yet" was put in the same bucket as "not logged
in", and the unit was written for a single-display assumption.

**Design (chosen).**

1. A wait timeout is retryable: exit 1, through a named function next to
   `backend_connect_exit_code` so that the rule lives in one place. Raise the
   wait to 30 s. With AV-2's restart policy (no start limit, backoff from 2 s
   to 60 s), the unit keeps retrying for as long as the session is up
   (`PartOf=` stops it when the session ends). The 30 s only sets how long
   one attempt waits before it exits and is restarted.
2. `install` writes the installing session's `DISPLAY`. A new `@DISPLAY@`
   placeholder renders `Environment=DISPLAY=<value>`. The value comes from
   `--display <:N[.S]>`, else the installing shell's `DISPLAY` (AGENTS.md
   already requires installing from the user's desktop session), else `:0`
   with a printed warning. Only local displays (`:N` or `:N.S`) are
   accepted. The detached re-exec passes `--display` explicitly, as it
   already does for `--max-resolution` (`install.rs:192-205`, `:414-424`),
   because a transient unit's environment is the manager's, not the
   shell's.

Rejected alternatives:

- Wait for X forever in-process. The unit then reads `active` while serving
  nothing, which is exactly the "looks like a hang" trap described at
  `capture.rs:210-218`.
- Drop `Environment=DISPLAY` and rely on
  `systemctl --user import-environment`. Whether the DM or the session
  imports `DISPLAY` varies by distro. When it is missing, `DISPLAY` is unset
  and the daemon fails with "(unset)".

**Files:** `ghostframe-server/src/commands/capture.rs`,
`ghostframe-server/src/capture/x11/mod.rs` (doc only),
`ghostframe-server/templates/ghostframe-server.service.in`,
`ghostframe-server/src/commands/install.rs`, `ghostframe-server/src/cli.rs`,
`ghostframe-server/src/main.rs` (the install plumbing at `:78-81`),
`ghostframe-server/src/config.rs` (only if a wait-timeout knob is added;
the env-read guard `ci/check-env-reads.sh` requires env reads there), and
`ghostframe-server/tests/commands.rs`.

**Steps**

1. `capture.rs`: add `fn x11_wait_exit_code() -> i32 { 1 }`, with a doc
   comment that ties it to `backend_connect_exit_code`'s rule. Use it at
   `:96`. Change the timeout to a `const X11_WAIT: Duration =
   Duration::from_secs(30);`.
2. **No public variable (owner decision D6, 2026-11-25).** The wait is made
   short for tests through a **hidden** argument on `capture`:
   - `cli.rs`: `Command::Capture` becomes a struct variant (sketch, against
     the clap derive at `cli.rs:57-58`):
     `Capture { #[arg(long, hide = true, value_name = "MS")] x11_wait_ms: Option<u64> }`.
   - The unit's `ExecStart` never passes it.
   - `capture::run` takes the resolved `Duration`. `None` means
     `X11_WAIT` (30 s). Clamp to at least 100 ms.
   - Update every `Command::Capture` pattern: `main.rs:52, 57, 66, 192, 269`
     and `cli.rs:98`. Patterns become `Command::Capture { .. }`.
   - It is not an environment variable, so `config.rs` and
     `ci/check-env-reads.sh` are untouched.
   - Add a `cli.rs` test asserting that `capture --help` output does not
     mention the flag, so it stays hidden.
3. Template: replace `Environment=DISPLAY=:0` with a line `@DISPLAY@`, and
   update the header comment "(DISPLAY=:0, your own desktop)" to "(the
   DISPLAY `install` was run from)".
4. `install.rs`: change `render_unit(max: MaxResolution)` to
   `render_unit(max: MaxResolution, display: &str)`. Add `fn
   is_local_display(s: &str) -> bool` (`:` then digits, optionally `.`
   digits) and `fn resolve_display(requested: Option<&str>, env:
   Option<&str>) -> (String, Option<String /* warning */>)`. Extend
   `install_args` / `detach_install_args` with `--display`. Read any
   existing unit's `DISPLAY` for a re-install (mirroring
   `max_resolution_of_unit`, `:438`) so `install --force` from an SSH shell
   keeps the installed value.
5. `cli.rs`: `#[arg(long, value_name = ":N", value_parser = parse_display)]
   display: Option<String>` on `Install`.
6. Update `capture.rs:30-32`'s comment.

**Tests**

- Unit, `capture.rs` tests: extend
  `only_an_unsupported_session_is_exit_2_not_a_transient_failure` with
  `assert_eq!(x11_wait_exit_code(), 1)`. Weak on its own, but it is the rule
  as code.
- Integration, `tests/commands.rs`:
  `capture_against_a_display_nobody_serves_exits_1_so_the_unit_retries`.
  Use `TS_AUTHKEY=tskey-unused`, a private state dir, `DISPLAY=:<unused>`
  (pick a display number by checking `/tmp/.X11-unix/X<n>` is absent), and
  `capture --x11-wait-ms 500`. Assert `status.code() == Some(1)` and that
  stderr names the display. This is the twin of
  `capture_on_a_16_bit_root_exits_2_so_the_unit_does_not_restart_loop`
  (`:283`). Mutation: put `exit(2)` back, and the test fails with `Some(2)`.
- Unit, `install.rs`:
  - Rewrite `unit_template_renders_the_limit_or_leaves_it_out` to pass `":1"`
    and assert `"\nEnvironment=DISPLAY=:1\n"`.
  - `resolve_display_prefers_flag_then_env_then_default`.
  - `remote_displays_are_refused`: `host:0`, `:abc` and the empty string are
    refused.
  - `detached_install_passes_the_display_explicitly`.
  - Mutation: hard-code `:0` in `render_unit`, and the `:1` assertion fails.

**Gates:** `just lint`, `just test-unit`, `cargo test -p ghostframe-server
--test commands`, and `gf-gpu-run just ci-local`. `ci/check-env-reads.sh` is
part of ci-local.

**Risks / owner decisions.**

- (a) Decided (D6): no public variable; the hidden `--x11-wait-ms` flag above.
- (b) An existing install keeps `:0` until it is re-installed.
- (c) The attach-mode docs, and AGENTS.md's deploy section, should mention
  that `install` records the display.

**Size:** S for the exit code and wait. M with the DISPLAY templating. Split
them into separate PRs.

---

## AV-6: the keysym cache is never invalidated; a miss refetches the whole keymap (Low)

**Status at e85fb005: CONFIRMED.**

- `ghostframe-server/src/capture/x11/input.rs:65-103`: a cache hit returns
  forever, with no `MappingNotify` handling, so a layout switch
  (`setxkbmap`, the GNOME input-source switcher) leaves stale keycodes.
- Every miss, including a keysym that is not in the map at all, sends a
  synchronous `get_keyboard_mapping(min, count).reply()` for the full
  keymap and rebuilds the cache. Each event then warns "no keycode for
  keysym" (`:186-192`).
- A client that sends KeyDown for unmapped keysyms causes a full keymap
  round trip plus a warn line **per event**, on the loop.
- Nobody reads events on this connection, so the `MappingNotify` events the
  server sends to every client accumulate in x11rb's queue.

**Design (chosen).** Folded into the AV-3 worker:

- On `Event::MappingNotify` from `poll_for_event`, clear the positive cache
  and a new negative cache. MappingNotify is delivered to all clients
  without selection.
- A miss refetches at most once per `MappingNotify` epoch: a keysym found
  absent goes into `misses: HashSet<u32>` (capped at 4096, cleared on
  MappingNotify), and later misses for it skip the fetch.
- The warn uses the rate limit of AV-7.

Rejected alternative: refetching on every miss but with a time-based
throttle. It is still stale after a layout switch, until the throttle lapses.

Out of scope: binding unmapped keysyms to a spare keycode, as xdotool does.
That is a feature, not a fix.

**Steps** (in the worker of AV-3):

1. Move the lookup into a worker-owned `struct Keymap { map: HashMap<u32,
   u8>, misses: HashSet<u32>, loaded: bool }`, with `fn lookup(&mut self,
   conn, keysym) -> Option<u8>`. If `!loaded`, fetch and set `loaded`. A
   hit returns. A miss inserts into `misses` and returns `None`. A keysym
   already in `misses` returns `None` with no fetch.
2. `fn invalidate(&mut self)`: `loaded = false`, clear both collections.
   Call it on `Event::MappingNotify(_)`.
3. Warn the first time and every 64th unmapped keysym (a counter), at debug
   otherwise.

**Tests** (ghostframe-server, under xvfb):

- `a_remapped_keysym_follows_the_mapping`. On a second connection,
  `change_keyboard_mapping` to map keysym S (pick an unused one, for example
  `0x1008ff2a`, after checking it is unmapped) to spare keycode A. Inject
  `key(S, true)` and `key(S, false)`. Then remap S to keycode B and A to
  NoSymbol, and wait for the injector to see the notify. Give the worker a
  `#[cfg(test)]` hook, or simply retry for up to 1 s. Inject `key(S, true)`
  and check `query_keymap()` shows B down and A up. Mutation: remove the
  `MappingNotify` arm, and A is pressed.
- `an_unmapped_keysym_fetches_the_keymap_once`. Needs a fetch counter
  exposed under `#[cfg(test)]`. 100 misses of the same unmapped keysym give
  `fetches == 1`. Mutation: remove the `misses` check, and `fetches == 100`.

**Gates:** as for AV-3. **Size:** S on top of AV-3.

---

## AV-7: journal flooding (Low)

**Status at e85fb005: CONFIRMED.**

- Go, `ghostbridge/web_server.go`:
  - `:157-164` `loggingListener.Accept`: `log.Printf("ghostbridge: %s accept
    from %s", ..)` on **every** TCP accept, on both `:443` and `:80`
    (wrapped at `:260-263`, production only).
  - That is before the TLS handshake and before `requireAuthorizedPeer`,
    which runs in the HTTP handler (`:166-172`). So **any tailnet node**,
    including one the access policy refuses, can write one line per TCP
    connect.
  - `:236-245`: `http.Server.ErrorLog` writes every "TLS handshake error"
    unthrottled (half-open handshakes from a scanner, each a line).
  - The existing limiter is `rateLimitedLog` (`ghostbridge/identity.go:185-211`,
    tested at `main_test.go:323`).
- Rust, live client only (the non-live paths are already rate-limited at
  `io_bridge.rs:2027-2037`, `ack_dispatch.rs:757-`, and `quic.rs:675/699/745/855`):
  - `handle_decode_error` warns per message (`io_bridge.rs:2300-2308`,
    target `ghostframe::bench`);
  - unknown INPUT sub-kind (`:2165`);
  - "decode_input_msg failed after size check" (`:2186`);
  - unknown feedback message type (`:2194-2197`);
  - "no keycode for keysym" (`input.rs:186-192`);
  - the X11 error warn (`input.rs:119`).
- AGENTS.md's memory note "journald rate-limit blinds diagnosis" is the
  consequence: a burst triggers journald suppression and hides the lines
  that matter.

**Design (chosen).**

- Go: rate-limit both sites with `rateLimitedLog`. Extract
  `func (l *rateLimitedLog) allow() (ok bool, suppressed int)` from `printf`
  so an `io.Writer` adapter can share it. Add `type rateLimitedWriter struct
  { lim *rateLimitedLog; w io.Writer }` for `ErrorLog`. Use one limiter per
  listener for accepts and one for the TLS error log, each at 10 s.
- Rust: use the house pattern `n == 1 || n.is_multiple_of(64)` with a
  counter per site. **The decode-error line must keep its message text at
  every occurrence**, because the bench counts it
  (`ghostframe-e2e/src/harness/scene.rs:788` → `decode_error_count`,
  surfaced at `scroll_scene.rs:536`). So emit it at `warn!` when the limiter
  allows and at `debug!` otherwise, with the same message and fields. The
  bench and e2e run at `ghostframe=trace,debug` and still count every one.

Rejected alternatives:

- Delete the Go accept log. It was added to tell "never connected" from
  "failed inside TLS" (comment at `:147-151`). The first line in each window
  keeps that signal.
- A `tracing` filter or a global rate-limit layer. That is invisible at the
  call sites, and it would also throttle lines the bench counts.

**Files:** `ghostbridge/identity.go`, `ghostbridge/web_server.go`,
`ghostbridge/main_test.go` (or `web_server_test.go`),
`ghostframe-lib/src/transport/io_bridge.rs`, and
`ghostframe-server/src/capture/x11/input.rs` (in the AV-3 worker).

**Steps**

1. `identity.go`: refactor `printf` to call a new `allow()` (same locking;
   it returns whether to print and how many were suppressed, and resets
   both). Keep `printf`'s behaviour byte-identical so
   `TestRateLimitedLogSuppressesWithinInterval` still passes.
2. `web_server.go`:
   - Add a `lim *rateLimitedLog` field to `loggingListener`, and in `Accept`
     use `l.lim.printf("ghostbridge: %s accept from %s", ..)`.
   - Construct both listeners with their own
     `&rateLimitedLog{interval: 10 * time.Second}`.
   - Add `rateLimitedWriter.Write(p []byte) (int, error)`: if allowed, write
     `p` (append " (N similar suppressed)" before the trailing newline when
     N > 0). Always return `len(p), nil`.
   - Build `tlsErrLog := log.New(&rateLimitedWriter{lim: ..., w:
     log.Writer()}, "ghostbridge: :443 http: ", log.LstdFlags)`.
3. `io_bridge.rs`:
   - Add the counters `decode_errors_received`,
     `unknown_input_subkinds` and `unknown_feedback_types` (`u64`, next to
     `:544`).
   - In `handle_decode_error`, bump the counter and pick `warn!` or
     `debug!` with the identical message `"client decode error"`, target
     `"ghostframe::bench"`, plus `total = n`.
   - Apply the same at `:2165`, `:2186` and `:2194` (warn, adding `total`).
4. `input.rs` worker: count and rate-limit the unmapped-keysym and X11-error
   warns (see AV-3 and AV-6).

**Tests**

- Go, `TestLoggingListenerRateLimitsAccepts`. Use a fake `net.Listener`
  whose `Accept` returns `net.Pipe()` ends, wrap it, call `Accept` 100
  times, and capture `log.SetOutput(&buf)` (restore it in `t.Cleanup`).
  Assert exactly 1 line containing "accept from". Mutation: revert to
  `log.Printf` and you get 100 lines.
- Go, `TestRateLimitedWriterSuppressesAndReports`. Two writes inside the
  interval give 1 line. Then set `lim.last` back past the interval: the
  third write's line contains "(1 similar suppressed)".
- Rust, io_bridge `tests.rs`, `decode_error_warns_are_rate_limited_but_every_one_is_logged`.
  - Use the `CapturingWriter` pattern already in this file
    (`tests.rs:2947-2975`) with a subscriber at `debug`. Send 200
    out-of-grid DECODE_ERRORs (they return early, so the test is cheap and
    independent of AV-1's palette state).
  - Assert 200 lines contain `client decode error` (the bench contract) and
    exactly 4 are `WARN` (n = 1, 64, 128, 192).
  - Mutation: make every one `warn!` and the WARN count is 200. Drop the
    `debug!` branch and the total is 4. **Both directions are guarded.**

**Gates:** `just lint`, `just test-unit`, `gf-gpu-run just ci-local` (it runs
`go vet` + `go test` for both tags; Justfile `:316-318`). Run a browserless
scroll scene once to confirm `decode_errors` is unchanged on master's
fixtures. A Go change also changes the embedded ghostbridge, so
`just containers-build` comes before any e2e.

**Size:** S.

---

## AV-8: a connect, HELLO, close loop forces a full session reset each time (Low)

**Status at e85fb005: CONFIRMED.**

- `session.rs:56-62` `TAKEOVER_MIN_INTERVAL` (1 s) is consulted only when
  `incumbent.is_some_and(|i| i != from)` (`apply_hello`, `:263-283`).
- After the live session leaves, `forget_session` clears `last_hello_from`
  (`:427-429`), so the next HELLO is not a takeover, by design. The test
  `reconnect_after_the_incumbent_left_is_not_rate_limited` (`tests.rs:5371-5383`)
  pins that.
- Because `has_seen_prior_session` is true, every such HELLO runs
  `fire_session_reset` (`:325-331`). That resets the dirty tracker, metrics,
  classifier, scheduler and palette, requests an IDR, and sets
  `force_dirty_frames = 20` plus `InvalidateBaseline(20)`: about 20 frames of
  full-screen encode each time.
- The web client has no auto-reconnect (`ghostframe-client-wasm/src/app/session.rs:141`),
  so a legitimate burst is a human reloading the page.

**Root cause.** The rate limit is keyed on *displacement*, but the cost is
paid on every *session start*.

**Design (chosen).** Generalise the limiter to every HELLO that would run
`fire_session_reset`, using a small token bucket: `SESSION_START_BURST = 4`
starts, refilled at 1 per `SESSION_START_REFILL = 2 s`. The takeover
interval stays as it is, an additional rule for displacement. A HELLO that
finds the bucket empty closes the newcomer with a new
`CLOSE_CODE_SESSION_START_RATE_LIMITED = 0x6705`, warn-logged at the AV-7
rate. The burst keeps a few reloads in a row working.
`reconnect_after_the_incumbent_left_is_not_rate_limited` keeps passing
unchanged, because it does one reconnect.

Rejected alternatives:

- Applying `TAKEOVER_MIN_INTERVAL` to reconnects too. That breaks a reload
  that lands within 1 s of the old session's loss, which the existing test
  forbids on purpose.
- Making the reset cheaper (a smaller cushion on a fast reconnect). That
  changes correctness-relevant recovery (the slow-start cushion comment at
  `session.rs:187-205`) to solve a resource problem.

**Files:** `ghostframe-lib/src/transport/io_bridge/session.rs`,
`ghostframe-lib/src/transport/io_bridge.rs` (a bucket field next to
`last_takeover_at`), and `ghostframe-lib/src/transport/io_bridge/tests.rs`.

**Steps**

1. `session.rs`: add the constants and a `pub(super) struct StartBucket {
   tokens: f32, at: Option<Instant> }` with `fn take(&mut self, now) ->
   bool` (refill by elapsed / REFILL, cap BURST, then take 1 if ≥ 1). Write
   it as a pure struct so it is unit-testable with explicit `Instant`s.
2. `apply_hello`: after the takeover check, and only when
   `incumbent != Some(from) && self.has_seen_prior_session` (exactly the
   condition under which `fire_session_reset` will run), check `if
   !self.session_starts.take(now) { insert CloseDeadline { at: now, code:
   CLOSE_CODE_SESSION_START_RATE_LIMITED }; warn; return; }`. This must come
   **before** `self.last_hello_from = Some(from)` and before AV-4's release.
   Use `now_std()` for the clock, as the takeover check does (`:265`).
3. Add a `CLOSE_CODE_SESSION_START_RATE_LIMITED =>
   b"session start rate limit"` arm to the reason match in
   `sweep_close_deadlines` (`:731-737`).

**Tests** (unit, io_bridge `tests.rs` and `session.rs`)

- `start_bucket_allows_the_burst_then_refills`: a pure struct test with
  synthetic `Instant`s.
- `a_reconnect_storm_is_cut_off_after_the_burst`. Use handles `0..10` via
  `test_bridge_with_sessions`. The first `apply_hello(h0)` is the first
  ever. Then for `h in 1..10` call `forget_session(prev)` and
  `apply_hello(h)`. Assert the session resets fired == 4 (count the
  `session_epoch` increments, or add a `#[cfg(test)]` counter in
  `fire_session_reset`), and that every later handle got
  `CLOSE_CODE_SESSION_START_RATE_LIMITED`. Mutation: always return `true`
  from `take`, and resets == 9.
- Keep `reconnect_after_the_incumbent_left_is_not_rate_limited` and
  `a_second_takeover_inside_the_interval_is_refused_and_the_newcomer_closed`
  green, unchanged.
- e2e: no new test, but run the reload-based e2e set (`just test-e2e`) once,
  because e2e scenes reconnect.

**Gates:** `just lint`, `just test-unit`, `just containers-build`, then
`just test-e2e`, and `gf-gpu-run just ci-local`.

**Risks / owner decisions.** The burst and refill values are a guess. A
browserless or e2e scene that reconnects more than 4 times in quick
succession would now fail. Grep `ghostframe-e2e` for repeated HELLOs before
merging. The current e2e tree shows no reconnect-storm scenes.

**Size:** S.

---

## AV-9: there is no view-only role (Low; a design note, not a fix)

**Status at e85fb005: CONFIRMED as described.**

- Every admitted peer that sends HELLO becomes the live session
  (`apply_hello`).
- The live session's INPUT messages are injected unconditionally
  (`io_bridge.rs:2155-2192`). The only gate is `is_live`.
- Any admitted peer can take over (subject to `TAKEOVER_MIN_INTERVAL`).
- The unit header (`ghostframe-server.service.in:4-8`) already warns that an
  admitted client "gets that desktop: your files, your browser sessions,
  your sudo".

**Owner decision, not a remediation item.** The options are:

- (a) Keep "admitted = full control" and say so in DEVELOPERS.md's "Tailnet
  peer access" section.
- (b) Add a per-identity role to the allowlist policy (`peer_auth`). A
  view-only peer would be served frames but its INPUT and DisplayMode would
  be dropped, and its HELLO would not displace a controlling session. This
  needs a wire signal so the client can show "view only".
- (c) Require a confirmation on the operator's desktop for a takeover.

(b) composes with the existing "policy in Rust, fail closed" rule (see
`project_tailnet_owner_allowlist`). If chosen, it is its own design doc. It
touches `peer_auth`, `apply_hello`, the INPUT and DISPLAY_MODE arms, and the
HELLO/eviction protocol. **Size if chosen:** L.

---

## Suggested PRs for AV

Order matters for the first two: AV-2 must land with or right after AV-1, so
that the next AV-1-class panic restarts the daemon instead of killing it.

1. **`fix/av-decode-error-grid-bounds`**: AV-1.
   - Files: `metrics_tracker.rs`, `io_bridge.rs` (`handle_decode_error`,
     `recover_from_decode_error`, a field), `io_bridge/frame.rs` (the replay
     loop), `io_bridge/session.rs` (two `clear()` lines), `io_bridge/tests.rs`.
   - **Touches `io_bridge.rs` and `session.rs`.** Land it first: it is the
     smallest and most urgent.
2. **`fix/av-capture-exit-nonzero`**: AV-2.
   - Files: `ghostframe-lib/src/server.rs`, `ghostframe-server/src/commands/capture.rs`,
     `ghostframe-server/src/main.rs`, plus the restart policy (D6) in
     `templates/ghostframe-server.service.in` and its `install.rs` test.
   - No io_bridge contention. It can be developed in parallel with PR 1 and
     merged right after it.
3. **`fix/av-x11-wait-retryable`**: AV-5 part 1 (the exit code, the 30 s
   wait, and the hidden `capture --x11-wait-ms` test flag; no new variable).
   - Files: `capture.rs`, `cli.rs`, `main.rs`, `tests/commands.rs`.
   - It **conflicts textually with PR 2 in `capture.rs`**. Rebase it onto
     PR 2, or fold the two into one PR.
4. **`fix/av-xtest-worker`**: AV-3 (the injector worker, the lib wheel
   clamp, and the DisplayMode ceiling-at-apply), AV-6 (the keymap in the
   worker) and AV-7's `input.rs` warns.
   - Files: `ghostframe-server/src/capture/x11/input.rs`, `input_inject.rs`,
     `io_bridge.rs` (the DISPLAY_MODE arm only), `io_bridge/display_mode.rs`,
     `io_bridge/tests.rs`.
   - **Touches `io_bridge.rs`.** Rebase after PR 1.
5. **`fix/av-release-held-input`**: AV-4.
   - Files: `input_inject.rs` (`HeldInput`), `io_bridge.rs` (a field, one
     line in the INPUT arm), `io_bridge/session.rs` (`forget_session`,
     `apply_hello`), `input.rs` (the worker's release on disconnect),
     `io_bridge/tests.rs`.
   - **Depends on PR 4**, since the server half lives in the worker. It
     contends with PR 4 on `input_inject.rs` and `input.rs`, and with PR 6
     on **`session.rs` `apply_hello`**: both insert code before
     `self.last_hello_from = Some(from)`. Land PR 5 before PR 6. PR 6's
     bucket check goes *above* PR 5's release, so a refused start releases
     nothing.
6. **`fix/av-session-start-bucket`**: AV-8.
   - Files: `io_bridge/session.rs`, `io_bridge.rs` (a field),
     `io_bridge/tests.rs`.
   - **`session.rs` contention with PRs 1 and 5.** Land it last of the
     three.
7. **`fix/av-log-rate-limits`**: AV-7 (the Go half and the Rust
   `io_bridge.rs` warns).
   - Files: `ghostbridge/identity.go`, `ghostbridge/web_server.go`, Go tests,
     `io_bridge.rs` (`handle_decode_error`'s warn and the dispatch warns).
   - **It touches the same `handle_decode_error` lines as PR 1.** Land it
     after PR 1. The Go half is independent and could be a separate PR,
     `fix/av-ghostbridge-log-rate-limit`.
8. **`feat/av-install-records-display`**: AV-5 part 2 (`@DISPLAY@`,
   `--display`).
   - Files: the template, `install.rs`, `cli.rs`, `main.rs` (the install
     args).
   - Independent of everything above. It needs owner sign-off on the
     approach.
9. AV-9 has no PR. It is an owner decision, and if (b) is chosen, a design
   doc first.

`io_bridge.rs` is touched by PRs 1, 4, 5, 6 and 7 (different functions in
each). `session.rs` is touched by PRs 1, 5 and 6. Serialise them in the
order 1 → 4 → 5 → 6 → 7, or give one implementer the io_bridge PRs in
sequence. Parallel agents on these two files will produce semantic merge
conflicts (see `feedback_rebase_merge_semantic_conflicts`). Each PR runs
`gf-gpu-run just ci-local` before it is pushed. PRs 4, 5 and 8 also need
`just containers-build`, then the input e2e. PR 6 needs the full
`just test-e2e`.
