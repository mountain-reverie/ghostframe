# All-Rust Web Client PR 3: The Main Loop and I/O in Rust Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each task header carries a model tag: **[haiku]** is mechanical (the code is given whole; transcribe, run, commit), **[sonnet]** needs judgement (browser bindings, ownership across callbacks, TS rewiring). Opus reviews every task, per the owner's model split.

**Goal:** The browser client's startup, WebTransport I/O, rAF loop, WebCodecs H.264 decode, input capture and overlays run in Rust (`ghostframe-client-wasm/src/app/`). `main.ts` shrinks to a page entry that starts the Rust client. The TypeScript that did this work (`decoder.ts`, `bootstrap.ts`, `server_stream.ts`, `close_message.ts`, `disconnect_overlay.ts`, `input/wire.ts`, and most of `main.ts`) is deleted, and its vitest suites move to Rust. The e2e suite passes with its Rust code unchanged.

**Architecture:** `startClient(hooks)` (Rust, async) does what `main()` in `main.ts` did, in the same order. One `App` behind `Rc<RefCell<…>>` owns the core, the wire writers, the datagram batch, the H.264 decoder and the per-tick counters. The renderer lives in its own `Rc<RefCell<WebRenderer>>`. Every browser callback (the rAF tick, the datagram reader, the fallback timer, decoder output and error) borrows `App`, runs one step, releases the borrow, and only then calls the JS hooks the step queued.

The e2e bookkeeping (`window.__ghostframe*`, the recorders, `health`, the ~2 s stats line) stays TypeScript in this PR, in a new `src/hooks.ts`. Rust calls it through a small hooks interface and gives it a `DebugApi` handle for the renderer and core reads. PR 4 ports the hooks to Rust and deletes `hooks.ts`, `diagnostics.ts`, `cdf53_globals.ts`, `health.ts` and `stats_log.ts` (spec §6, PR 4 row).

The decisions stay plain Rust and are unit-tested on the host: page flags, cert pins, the datagram batch and its flush rule, the input mapping, the decoder state machine, and what each routed event does to the page. Only the browser shells are wasm32.

**Tech Stack:** Rust 1.96.1 (pinned), `wasm-bindgen` 0.2.129 (locked), `wasm-bindgen-futures` 0.4.79, `js-sys`, `web-sys` 0.3.106 (stable features only; WebTransport and WebCodecs through `extern "C"` bindings, as PR 2 did for the H.264 escape), `wasm-bindgen-test =0.3.79` driven by `wasm-pack` 0.15.0 in headless Chrome, and TypeScript plus vitest for what is left of `ghostframe-web-client`.

**Branch:** `feat/web-main-loop`, worktree `~/work/gf-web-mainloop`, off master `6e310e42` (PR 2, #318, merged). **Spec:** `docs/specs/all-rust-web-client-design.md`: section 5.4, the PR 3 rows of 6 and 8, and the hidden-tab and feedback-path risks of 9.

**Exit status.** A pipe reports the last command's status, and a masked build failure has shipped a stale `dist/` here before. Every gating command in this plan that is piped into `tail` or `grep` is followed by `echo "rc=${PIPESTATUS[0]}"`, and its step names the rc it expects. A step that does not mention rc expects `0`. Unpiped commands use `echo "rc=$?"`.

**Paths and locks.**
- The repository root is the worktree root.
- `~/work/gf-gpu-run` is the GPU guard.
- `just ci-local` is always run as `~/work/gf-gpu-run just ci-local`.
- `just test-e2e` takes `flock -o ~/work/.gf-heavy.lock` outside the GPU guard.
- **The owner may be running their own e2e on this machine.** Do not run `just test-e2e`, `just containers-build` or the browser tests (`wasm-pack test`, `just test-web-renderer`) locally without the controller's go-ahead (gate G3). Use CI for those until then.
- `TMPDIR=~/work/gf-tmp-webr` for every build (`/tmp` fills up).

## Findings that shaped this plan (verified 2026-11-15 on master 6e310e42)

- **The hooks bridge is what the spec's PR split implies.** PR 4's row says it deletes "the TS diagnostics", so they outlive PR 3. In `main.ts` the per-event recorders (`recordProtocolEvent`, `diag.recordTile`, `recordPaletteWrite`, `recordResize`, `__ghostframe_framePaints`, `health`, `statsLog`) are interleaved with the main loop. So PR 3 moves the loop and leaves those recorders behind one JS object that Rust calls.
  - **The order of calls is fixed.**
    - In each tick: `rafTick`, then `events` and `datagramCounts` for the datagram flush, then `events` for the timeout, then `beforePaint`, then `afterPaint`.
    - Outside a tick: `events` and `datagramCounts` after every flush.
    - `serverStream` after every chunk the feedback stream's return half delivers.
    - `swallowed` and `rendererError` wherever `main.ts` called `health.record`.
  - **Timestamps are passed in, not taken at dispatch:** `atMs` on `events` and `beforePaint`. So the bench's `firstRecvMsClient`/`rafMsClient` still mean "when the event was handled" and "before the paint".
- **Hooks are called with no Rust borrow held.** Two hook paths call back into Rust synchronously:
  - `events` re-publishes `window.__ghostframeRenderer = api.debugHandles()` on every applied `FrameDimensions`;
  - the stats line reads `__readPixel`.

  A `RefCell` double borrow is a panic, and a panic in wasm aborts the module. So every entry point queues its `HookCall`s, releases `App`, then dispatches. `DebugApi` uses `try_borrow` and turns a conflict into a JS error, so a future mistake costs one rejected call, not the session.
- **The spec's `#[wasm_bindgen(start)]` waits for PR 4.** `start()` already exists (it installs the panic hook) and stays. The client starts from `startClient(hooks)`, which `main.ts` calls, because the hooks have to be passed in. PR 4 removes the hooks and moves the call into `start`.
- **No `--cfg=web_sys_unstable_apis`.** In web-sys 0.3.106, `WebTransport*`, `VideoDecoder`, `EncodedVideoChunk` and `VideoFrame` are all behind it (checked: each `gen_*.rs` carries the cfg).
  - PR 2's decision (owner, 2026-11-14) was raw `extern "C"` bindings, not the build-wide flag, so `app/js.rs` declares the dozen members needed.
  - What is stable is used from web-sys directly: `ReadableStream`, `ReadableStreamDefaultReader`, `WritableStream`, `WritableStreamDefaultWriter`, `Response`, `RequestInit`, `RequestCache`, `Location`, `UrlSearchParams`, `PointerEvent`, `WheelEvent`, `KeyboardEvent`, `DomRect`, `Element`, `HtmlDivElement`, `CssStyleDeclaration`, `AddEventListenerOptions`, `Event`, `EventTarget`.
- **`ReadableStream::get_reader()` returns a `js_sys::Object` in 0.3.106; cast it with `unchecked_into`.** `WritableStream::get_writer()` returns `Result<WritableStreamDefaultWriter, JsValue>`. `RequestInit::set_cache` exists (the old builder is deprecated).
- **`WritableStreamDefaultWriter` keeps writes in order without awaiting.** `drainTransmit` awaited each stream write. It ran as a floating promise from every tick and flush, so two drains interleaved anyway: there was ordering but never backpressure. The Rust drain is synchronous. It calls `write` for each output in poll order and attaches one persistent `catch` closure, so stream order (HELLO first) is kept and no extra queue appears.
- **A WebCodecs `VideoDecoder` never calls `output` or `error` from inside `decode()`, `configure()` or `close()`, and `close()` does not invoke `error`** (WebCodecs §4.5: "Close VideoDecoder" with an `AbortError` skips the error callback).
  - So calling the decoder while `App` is borrowed is safe.
  - The decoder's callbacks run as their own tasks and borrow `App` themselves.
  - The vitest fake that answered from inside `decode()` modelled something the browser does not do. The Rust state machine is tested with outputs delivered after `decode` returns.
- **The decoder's callbacks are kept alive for the session.** In `decoder.ts`, a late callback from a replaced decoder is ignored by identity. In Rust, a dropped `Closure` that JS still calls throws, and the frame it carried leaks. So every generation's closures stay in a `Vec` until the session resets. Rebuilds only follow a decode failure, which the server answers with a paced IDR (2.2 s), so the `Vec` stays tiny.
- **`decoder.ts` kept a `latestFrame` that only `close()` closed.** The renderer owns every frame handed to `pushH264` and closes it (M3.2b: the tick is the sole closer), so that second close was a no-op on an already-closed frame. The port does not keep it.
- **`main.ts` installed `__ghostframeHealth`, the recorders and `health.record` with no `?e2e` gate.** Only the console ring, `__ghostframeStatsSamples`/`Frames`, `__ghostframePaintUs`, `__ghostframeTickUs`, the readbacks and the solid-colour tallies were gated. `hooks.ts` keeps exactly that split. Gating the rest is PR 4's business (spec §5.5: "only under `?e2e`").
- **No e2e covers the hidden tab.**
  - Searching `ghostframe-e2e` and `ghostframe-bench` for `hidden`, `visibility` or `Lifecycle` finds nothing.
  - Spec §9's "PR 3 keeps the timer fallback and its e2e" has no e2e to keep. The fallback rule becomes a pure function, `batch::flush_decision`, with host tests that fail if any branch flips. The stub that a real hidden tab would need is out of scope (listed in the PR body).
- **`e2e_input_forwarding_chromium` checks the whole input path end to end:** pointer move, down, up, wheel, Enter down and up, and the leave sentinel, in the server log. The keysym table and the wire encoding are already in `ghostframe-client-core::input`, with their own tests. The vitest `input.test.ts` re-tested them through `pkg-node`, and Task 4 adds to the core's tests whichever of its cases those lack.
- **The feedback bytes are the part e2e cannot see** (memory: e2e is blind to feedback-path bugs). Task 13's browser test drives a real `App`:
  - a real `WebRenderer` and `ClientCore`;
  - fake `WritableStream`s that record every write;
  - a solid tile with a real `wire_seq`.

  It asserts the HELLO leaves first on the feedback stream, a 0x06 ACK batch names the tile's `wire_seq`, and the hooks are called in the documented order.
- **A real H.264 decode in headless Chrome.** `ghostframe-client-h264/src/probe_clip.h264` is a 758-byte 64x64 High-profile IDR (SPS `67 64 00 0a`, so `avc1.64000a`). Task 10 decodes it through the real `VideoDecoder`. Whether CI's Chrome decodes H.264 in that mode is unproven, so that is gate G2.
- **The wasm cap is 800 KiB raw** (`ci/check-wasm-size.sh`). This PR adds bindings and logic and deletes two JS-facing exports (`handleDatagramsInto`, `onTimeoutInto`) and the six `input.rs` exports. Task 0 records the size before, and Task 15 the size after (gate G1).
- **`CLOSE_CODE_ACCESS_REVOKED` (0x6704) is `pub(super)` in AGPL `ghostframe-lib`**, which a permissive crate must never depend on (AGENTS.md, licensing rule 2). It stays a client-side constant, as `close_message.ts` had it. Moving it into `ghostframe-protocol` would touch the server and is out of scope.

## The hooks interface (Rust calls it; `src/hooks.ts` implements it)

```ts
interface Hooks {
  attach(api: DebugApi): void;                               // once, after WebRenderer.create succeeded
  events(records: HookRecord[], atMs: number): void;         // routed records, in order (see below)
  datagramCounts(tile: number, frame: number): void;         // after every datagram flush
  rafTick(n: number): void;                                  // start of every tick
  beforePaint(atMs: number): void;                           // after the tick's events, before paintTick
  afterPaint(info: AfterPaintInfo): void;                    // end of every tick
  swallowed(site: string, message: string): void;            // was health.record(site, e)
  rendererError(tick: number, message: string): void;        // was the takeErrors() loop
  serverStream(samples: number, frames: number): void;       // after every server-stream chunk
}
interface AfterPaintInfo { solid: number; palrle: number; cdf53: number; h264: number; rafTicks: number; nackSent: number }
```

`HookRecord` is the JSON `handleDatagramsInto` returned for `TilePayload` (slim), `PaletteUpdated`, `FrameDimensions` and `DecodeError`. It is serialised from the same `web::route::Routed` values, so the shapes match exactly. `NeedsH264`, `Evicted` and `TileReady` are handled in Rust and never sent: no recorder read them.

`DebugApi` (Rust, `#[wasm_bindgen]`) has:
- `debugState()`, `debugTile(x, y)`, `debugHandles()`;
- `paintUsStats()`, `tickUsStats()`;
- `readPixelRect(x, y, w, h)` and `readGradientGolden()`, which are promises;
- the getters `fbWidth`, `fbHeight` and `submitCount`;
- `cdf53Coverage()` and `cdf53IncompleteTiles(limit)`, which return `undefined` before the core exists;
- `log(msg)`, which appends to the page log.

The names are the ones `diagnostics.ts`'s `PixelReader` and `stats_log.ts`'s `StatsRenderer` and `StatsCore` expect, so those files do not change.

## Gates that stop the work

| Gate | Where | What the agent does |
|---|---|---|
| G0 | Task 0 | An API this plan names is missing or has another signature on master: stop, list the differences, wait for the controller. |
| G1 | Task 15 | The release wasm exceeds 800 KiB: stop and report raw and gzip sizes, before and after. The cap is the owner's decision. |
| G2 | Task 10, CI | `the_real_decoder_decodes_the_probe_clip` fails on CI because the runner's Chrome cannot decode H.264 in headless mode (the decoder errors or never answers), while it passes locally: stop and report. The owner decides between a workflow `--skip` (CI exempts itself, never `#[ignore]`) and another approach. |
| G3 | Task 15 | Local `just test-e2e` and the browser tests run only after the controller confirms the machine is free (the owner may be running their own e2e). Until then, CI is the gate. |

## File structure

New, in `ghostframe-client-wasm/src/app/` (crate licence MIT OR Apache-2.0, SPDX line first in every file):

| File | Target | Responsibility |
|---|---|---|
| `mod.rs` | all | module list; wasm32 gates |
| `page.rs` | host | `PageFlags` from the URL's `e2e` and `cdf53` parameters |
| `close.rs` | host | close-code message, eviction message |
| `certs.rs` | host | `/config.json` pin selection, hex decoding |
| `batch.rs` | host | `DatagramBatch` (one copy into wasm memory), `flush_decision` |
| `input_map.rs` | host | DOM button to X11, coordinate scaling, wheel ticks |
| `decoder.rs` | host | `FrameDecoder<C: VideoCodec>`: the `decoder.ts` state machine, SPS codec string |
| `session.rs` | host | `Session`: what each routed event does to the page (`Action`s), drain counts, NACK counting |
| `js.rs` | wasm32 | raw bindings: `WebTransport`, its datagram and bidi streams, `VideoDecoder`, `EncodedVideoChunk` |
| `streams.rs` | wasm32 | `pump_stream`, `LoggedWriter` |
| `bootstrap.rs` | wasm32 | fetch `/config.json`, open the same-origin `WebTransport` |
| `webcodecs.rs` | wasm32 | `WebCodec: VideoCodec` over the browser's `VideoDecoder` |
| `dom.rs` | wasm32 | `PageLog` (status and log divs), overlay, lossless page chrome, `page_flags()` |
| `hooks.rs` | wasm32 | `Hooks` extern type, `HookCall`, `dispatch` |
| `debug_api.rs` | wasm32 | `DebugApi` |
| `input.rs` | wasm32 | canvas listeners |
| `client.rs` | wasm32 | `App`, the tick, the flush, the reader loop, `startClient` |

Modified:
- `ghostframe-client-wasm/src/lib.rs` (`pub mod app;`; the `input` module goes).
- `src/core.rs`:
  - `client_config` and `feed_datagrams` are extracted;
  - the two `*Into` exports are deleted;
  - the moved core tests are added.
- `src/web/renderer.rs` (`create_inner` becomes `pub(crate)`, plus two Rust-side drains).
- `src/web/logic.rs` (`summarize_us`).
- `src/units.rs` (two converters become `pub(crate)`).
- `Cargo.toml` (web-sys features).
- `ghostframe-client-core/src/input.rs` (missing test cases only).
- `ghostframe-web-client/src/main.ts` (shrinks), new `src/hooks.ts`, `index.html` unchanged.

New browser tests:
- `ghostframe-client-wasm/tests/app_io.rs`: streams, bootstrap and DOM.
- `tests/app_decoder.rs`: the real `VideoDecoder`.
- `tests/app_client.rs`: a real `App` and the feedback bytes.

Deleted:
- TS: `decoder.ts`, `bootstrap.ts`, `server_stream.ts`, `close_message.ts`, `disconnect_overlay.ts`, `input/wire.ts`.
- vitest: `bootstrap`, `close_message`, `datagram_batch`, `eviction`, `h264_ack`, `h264_frame_seq`, `h264_result`, `input`, `reset_request`, `server_stream`, `tile_assembly`, and `helpers/wasm.ts`'s `buildEvictionDatagram` if nothing else uses it.

---

### Task 0: Baseline and contract check [haiku]

**Files:** none changed.

- [ ] **Step 1: Confirm the base.**

Run:
```bash
cd ~/work/gf-web-mainloop && git log --oneline -1 && git status --short | head
```
Expected: `6e310e42 ci(web): --use-vulkan=swiftshader ...` and no tracked changes. The only untracked file is this plan.

- [ ] **Step 2: Check every API this plan uses exists with the stated shape.** Run each `grep`. Each must print at least one line matching the comment.

```bash
cd ~/work/gf-web-mainloop/ghostframe-client-wasm
grep -n 'async fn create_inner(' src/web/renderer.rs                 # canvas: HtmlCanvasElement, mode: Cdf53Mode -> Result<Self, String>
grep -n 'pub fn paint_tick_js(&mut self, now_us: u64) -> u32' src/web/renderer.rs
grep -n 'pub fn push_h264(&mut self, frame: JsValue, frame_seq: u32)' src/web/renderer.rs
grep -n 'pub fn on_session_reset(&mut self)' src/web/renderer.rs
grep -n 'pub fn take_decode_errors(&mut self) -> Vec<u32>' src/web/renderer.rs
grep -n 'pub fn take_errors(&mut self) -> Array' src/web/renderer.rs
grep -n 'pub fn route<S: TileSink' src/web/route.rs
grep -n 'fn collect_datagram_events' src/core.rs
grep -n '^fn codec_from_u8\|^fn decode_error_code_from_u8' src/units.rs
grep -n 'pub fn mount_stats_panel' ../ghostframe-web-ui/src/lib.rs   # (container: HtmlElement, backend: Rc<dyn PanelBackend>)
grep -n 'pub enum H264Outcome' ../ghostframe-client-core/src/h264_gate.rs
grep -n 'pub fn report' ../ghostframe-client-core/src/decode_error_batcher.rs
grep -n 'pub fn build_frame_dimensions_datagram\|pub fn fragment_tile' ../ghostframe-protocol/src/protocol.rs
grep -n 'pub fn build_eviction_datagram' ../ghostframe-protocol/src/eviction.rs
grep -n 'pub const TILE_NACK_ENVELOPE' ../ghostframe-protocol/src/protocol.rs
```
If any is missing or differs, **G0: stop and report.**

- [ ] **Step 3: Record the baseline.**

Run:
```bash
cd ~/work/gf-web-mainloop && export TMPDIR=~/work/gf-tmp-webr
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"
just build-web > ~/work/gf-wml-build0.log 2>&1; echo "rc=$?"
ls -l ghostframe-web-client/dist/assets/*.wasm && gzip -9c ghostframe-web-client/dist/assets/*.wasm | wc -c
```
Expected: rc=0 for both. Write the pass count and the raw and gzip byte sizes into the task report: Task 15 compares against them.

No commit.

---

### Task 1: `app` module skeleton, page flags, close and eviction messages [haiku]

**Files:**
- Create: `ghostframe-client-wasm/src/app/mod.rs`, `src/app/page.rs`, `src/app/close.rs`
- Modify: `ghostframe-client-wasm/src/lib.rs` (add `pub mod app;` after `pub mod boundary;`)

- [ ] **Step 1: Write `src/app/mod.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client: startup, WebTransport I/O, the rAF loop, H.264 decode,
//! input and overlays (spec section 5.4). What decides things is plain Rust,
//! unit-tested on the host; what touches the browser is wasm32-only.

pub mod close;
pub mod page;
```

(Later tasks add their modules to this list.)

- [ ] **Step 2: Write `src/app/page.rs` with its tests**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the page URL asks for.

use crate::web::logic::Cdf53Mode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageFlags {
    /// `e2e` is present, with any value: the e2e suite drives this page.
    pub e2e: bool,
    /// One `e2e` value is `lossless` (`?e2e=1&e2e=lossless`): no page chrome,
    /// the canvas pinned top-left and no stats panel, so a screenshot holds
    /// only the canvas.
    pub lossless: bool,
    /// The `cdf53` parameter (`cpu` forces the CPU decoder; default `auto`).
    pub cdf53: Cdf53Mode,
}

impl PageFlags {
    /// `e2e_values` is `URLSearchParams.getAll("e2e")` (one empty string for a
    /// bare `?e2e`), `cdf53` is `URLSearchParams.get("cdf53")`.
    pub fn from_params(e2e_values: &[String], cdf53: Option<&str>) -> Self {
        PageFlags {
            e2e: !e2e_values.is_empty(),
            lossless: e2e_values.iter().any(|v| v == "lossless"),
            cdf53: Cdf53Mode::parse(cdf53.unwrap_or("auto")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_harness_url_is_e2e_and_lossless_when_asked() {
        // ghostframe-e2e: `?e2e=1&debug=1` plus `&e2e=lossless` for the golden.
        let f = PageFlags::from_params(&v(&["1", "lossless"]), None);
        assert!(f.e2e && f.lossless);
        assert_eq!(f.cdf53, Cdf53Mode::Auto);
        let f = PageFlags::from_params(&v(&["1"]), Some("cpu"));
        assert!(f.e2e && !f.lossless);
        assert_eq!(f.cdf53, Cdf53Mode::Cpu);
    }

    #[test]
    fn a_bare_e2e_counts_and_no_e2e_is_production() {
        assert!(PageFlags::from_params(&v(&[""]), None).e2e);
        let f = PageFlags::from_params(&[], None);
        assert!(!f.e2e && !f.lossless);
        assert!(!PageFlags::from_params(&v(&["losless"]), None).lossless, "a typo is not lossless");
    }
}
```

- [ ] **Step 3: Write `src/app/close.rs` with its tests** (port of `close_message.ts`, `close_message.test.ts` and the eviction wording from `main.ts`):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the user is told when the session ends.

use ghostframe_protocol::eviction::EvictionReason;

/// QUIC application close code the server uses for a session whose peer no
/// longer passes the tailnet access rule (`CLOSE_CODE_ACCESS_REVOKED` in
/// ghostframe-lib's io_bridge session; that crate is AGPL and is never a
/// dependency here, so the value is repeated, as `close_message.ts` did).
pub const CLOSE_CODE_ACCESS_REVOKED: u32 = 0x6704;

/// The overlay text for a transport that closed with `close_code`, or `None`
/// when the close needs nothing beyond the page log (takeover has its own
/// eviction overlay; other closes are not explained to the user).
pub fn close_overlay_message(close_code: Option<u32>) -> Option<&'static str> {
    (close_code == Some(CLOSE_CODE_ACCESS_REVOKED)).then_some("Access revoked")
}

/// The overlay text for the server's eviction notice; `reason` is
/// `EvictionReason::to_byte()`.
pub fn evicted_message(reason: u8) -> &'static str {
    if reason == EvictionReason::DisplacedByNewSession.to_byte() {
        "Another session took over this desktop."
    } else {
        "The server ended this session."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_revoked_is_the_only_explained_close() {
        assert_eq!(CLOSE_CODE_ACCESS_REVOKED, 26372);
        assert_eq!(close_overlay_message(Some(0x6704)), Some("Access revoked"));
        assert_eq!(close_overlay_message(Some(0x6701)), None);
        assert_eq!(close_overlay_message(Some(0)), None);
        assert_eq!(close_overlay_message(None), None);
    }

    #[test]
    fn eviction_names_a_takeover_and_nothing_else() {
        assert_eq!(
            evicted_message(EvictionReason::DisplacedByNewSession.to_byte()),
            "Another session took over this desktop."
        );
        assert_eq!(evicted_message(0xEE), "The server ended this session.");
    }
}
```

- [ ] **Step 4: Add `pub mod app;` to `src/lib.rs`** (alphabetical, before `pub mod boundary;`).

- [ ] **Step 5: Run the tests.**

Run: `cargo test -p ghostframe-client-wasm --lib app:: 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"`
Expected: `4 passed`, rc=0.

- [ ] **Step 6: Break each and watch it fail.**
  - In `close_overlay_message`, change `Some(CLOSE_CODE_ACCESS_REVOKED)` to `Some(0x6701)`.
  - In `PageFlags::from_params`, change `any(|v| v == "lossless")` to `any(|v| v.starts_with("loss"))`.

  Each must fail one named test. Revert both, rerun, and expect a pass.

- [ ] **Step 7: Commit.**

```bash
git add ghostframe-client-wasm/src/lib.rs ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/page.rs ghostframe-client-wasm/src/app/close.rs
git commit -m "feat(web): app module: page flags, close and eviction messages in Rust"
```

---

### Task 2: Certificate pins from `/config.json` [haiku]

**Files:** Create `ghostframe-client-wasm/src/app/certs.rs`; modify `src/app/mod.rs` (add `pub mod certs;`).

- [ ] **Step 1: Write `src/app/certs.rs`** (port of `bootstrap.ts`'s selection and `hexToBytes`, and of the selection half of `bootstrap.test.ts`):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Which certificate hashes to pin, from `/config.json`.

/// A hash as `/config.json` carries it: non-empty, even-length hex.
pub fn is_hex_hash(s: &str) -> bool {
    !s.is_empty() && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// `listed` is `certHashes` when it is an array (`None` per entry that is not
/// a string), `single` is `certHash` when it is a string. The full list wins
/// when it is non-empty and every entry is valid; an older server sends only
/// `certHash`. Every hash pinned: current first, then the successor.
pub fn pick_cert_hashes(
    listed: Option<&[Option<String>]>,
    single: Option<&str>,
) -> Result<Vec<String>, String> {
    if let Some(list) = listed {
        if !list.is_empty() && list.iter().all(|h| h.as_deref().is_some_and(is_hex_hash)) {
            return Ok(list.iter().flatten().cloned().collect());
        }
    }
    match single {
        Some(h) if is_hex_hash(h) => Ok(vec![h.to_string()]),
        _ => Err("bootstrap: /config.json missing certHash".to_string()),
    }
}

/// The bytes of a hash `is_hex_hash` accepted.
pub fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if !is_hex_hash(hex) {
        let head: String = hex.chars().take(32).collect();
        return Err(format!("bootstrap: invalid certHash hex: {head}…"));
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).map_err(|e| e.to_string()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn a_lone_cert_hash_is_pinned() {
        let h = "00112233445566778899aabbccddeeff".repeat(2);
        assert_eq!(pick_cert_hashes(None, Some(&h)), Ok(vec![h.clone()]));
        let b = hex_to_bytes(&h).unwrap();
        assert_eq!((b.len(), b[0], b[1], b[31]), (32, 0x00, 0x11, 0xff));
    }

    #[test]
    fn every_listed_hash_is_pinned_so_a_rotation_still_connects() {
        let (a, b) = ("aa".repeat(32), "bb".repeat(32));
        let list = [s(&a), s(&b)];
        assert_eq!(pick_cert_hashes(Some(&list), Some(&a)), Ok(vec![a, b]));
    }

    #[test]
    fn a_malformed_list_falls_back_to_cert_hash() {
        let a = "aa".repeat(32);
        assert_eq!(pick_cert_hashes(Some(&[s(&"zz".repeat(32))]), Some(&a)), Ok(vec![a.clone()]));
        assert_eq!(pick_cert_hashes(Some(&[None]), Some(&a)), Ok(vec![a.clone()]), "non-string entry");
        assert_eq!(pick_cert_hashes(Some(&[]), Some(&a)), Ok(vec![a]), "empty list");
    }

    #[test]
    fn no_usable_hash_is_an_error_that_names_cert_hash() {
        for single in [None, Some(""), Some("abc"), Some("zz")] {
            let e = pick_cert_hashes(None, single).unwrap_err();
            assert!(e.contains("certHash"), "{e}");
        }
        assert!(hex_to_bytes("abc").is_err());
    }
}
```

- [ ] **Step 2: Add `pub mod certs;` to `src/app/mod.rs`.** Run `cargo test -p ghostframe-client-wasm --lib app::certs 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"`. Expected: `4 passed`.

- [ ] **Step 3: Break and watch it fail.** Change `list.iter().all(` to `list.iter().any(`. `a_malformed_list_falls_back_to_cert_hash` must fail. Revert.

- [ ] **Step 4: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/certs.rs
git commit -m "feat(web): certificate pin selection from /config.json in Rust"
```

---

### Task 3: The datagram batch and its flush rule [haiku]

**Files:** Create `ghostframe-client-wasm/src/app/batch.rs`; modify `src/app/mod.rs` (`pub mod batch;`).

- [ ] **Step 1: Write `src/app/batch.rs`** (the batching half of `main.ts`'s PC-3 block, lines 700-820 on master):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inbound datagram batching (PC-3). The read loop only queues, with each
//! datagram's arrival instant (ACK timing and suspension detection read it per
//! datagram); the queue reaches the core in one call per tick. rAF is paused in
//! a hidden tab, so two fallbacks keep intake moving: a backlog threshold and a
//! timer. Neither fires in normal foreground use, where the tick runs every
//! ~16 ms.

/// Datagrams queued before a flush is forced.
pub const FLUSH_BACKLOG: usize = 32;
/// How long a datagram may wait for a tick before the timer flushes it.
pub const FLUSH_FALLBACK_MS: i32 = 50;
/// Shorter datagrams are the ping/pong text of M0, which predates the tile
/// protocol: never handed to the core.
pub const SHORT_DATAGRAM_LEN: usize = 20;

/// Datagrams back to back in one buffer, with their lengths and arrival
/// stamps: the `concat`/`lens`/`arrivals_us` form the core's batch call takes.
#[derive(Debug, Default)]
pub struct DatagramBatch {
    concat: Vec<u8>,
    lens: Vec<u32>,
    arrivals_us: Vec<f64>,
}

impl DatagramBatch {
    /// Queue one datagram of `len` bytes. `fill` writes it straight into the
    /// batch (the browser side copies from the JS `Uint8Array` here: one copy
    /// into wasm memory, no intermediate `Vec`).
    pub fn push_with(&mut self, len: usize, arrival_us: f64, fill: impl FnOnce(&mut [u8])) {
        let start = self.concat.len();
        self.concat.resize(start + len, 0);
        fill(&mut self.concat[start..]);
        self.lens.push(len as u32);
        self.arrivals_us.push(arrival_us);
    }

    pub fn len(&self) -> usize {
        self.lens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lens.is_empty()
    }

    /// `(concat, lens, arrivals_us)` for `core::feed_datagrams`.
    pub fn parts(&self) -> (&[u8], &[u32], &[f64]) {
        (&self.concat, &self.lens, &self.arrivals_us)
    }

    /// Empty the batch, keeping its capacity.
    pub fn clear(&mut self) {
        self.concat.clear();
        self.lens.clear();
        self.arrivals_us.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushDecision {
    /// Hand the batch to the core now.
    Now,
    /// Arm the fallback timer.
    ArmTimer,
    /// The timer is armed or the tick will come: nothing to do.
    Wait,
}

/// What to do after queueing a datagram. A hidden tab processes at once (rAF
/// is paused and timers are clamped to >= 1 s there), so ACKs stay prompt.
pub fn flush_decision(queued: usize, hidden: bool, timer_armed: bool) -> FlushDecision {
    if queued >= FLUSH_BACKLOG || hidden {
        FlushDecision::Now
    } else if !timer_armed {
        FlushDecision::ArmTimer
    } else {
        FlushDecision::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_batch_holds_datagrams_back_to_back_with_their_stamps() {
        let mut b = DatagramBatch::default();
        b.push_with(3, 10.5, |d| d.copy_from_slice(&[1, 2, 3]));
        b.push_with(2, 11.0, |d| d.copy_from_slice(&[4, 5]));
        assert_eq!(b.len(), 2);
        let (c, l, a) = b.parts();
        assert_eq!((c, l, a), (&[1, 2, 3, 4, 5][..], &[3, 2][..], &[10.5, 11.0][..]));
        b.clear();
        assert!(b.is_empty() && b.parts().0.is_empty());
    }

    #[test]
    fn a_backlog_or_a_hidden_tab_flushes_now() {
        assert_eq!(flush_decision(FLUSH_BACKLOG, false, true), FlushDecision::Now);
        assert_eq!(flush_decision(1, true, false), FlushDecision::Now);
        assert_eq!(flush_decision(1, true, true), FlushDecision::Now);
    }

    #[test]
    fn otherwise_the_first_datagram_arms_the_timer_and_the_rest_wait() {
        assert_eq!(flush_decision(1, false, false), FlushDecision::ArmTimer);
        assert_eq!(flush_decision(FLUSH_BACKLOG - 1, false, true), FlushDecision::Wait);
    }

    #[test]
    fn the_constants_are_the_ones_main_ts_used() {
        assert_eq!((FLUSH_BACKLOG, FLUSH_FALLBACK_MS, SHORT_DATAGRAM_LEN), (32, 50, 20));
    }
}
```

- [ ] **Step 2: Register the module, run, break.** Add `pub mod batch;`, then run `cargo test -p ghostframe-client-wasm --lib app::batch`. Expected: `4 passed`. Then try each mutation, which must fail a test:
  - drop `|| hidden`;
  - change `>=` to `>`;
  - change `!timer_armed` to `timer_armed`.

  Revert.

- [ ] **Step 3: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/batch.rs
git commit -m "feat(web): datagram batch and the hidden-tab flush rule in Rust"
```

---

### Task 4: Input mapping, and the keysym cases the core lacks [haiku]

**Files:**
- Create `ghostframe-client-wasm/src/app/input_map.rs`.
- Modify `src/app/mod.rs`.
- Modify `ghostframe-client-core/src/input.rs`, test module only.

- [ ] **Step 1: Write `src/app/input_map.rs`** (the pure half of `input/wire.ts`):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Browser input to the wire's terms: X11 button numbers and framebuffer
//! pixels. The encoding and the keysym table are `ghostframe-client-core::input`.

/// DOM `MouseEvent.button` to X11: DOM 0 left, 1 middle, 2 right, 3 back,
/// 4 forward; X11 1, 2, 3, 8, 9. Any other button is not sent.
pub fn dom_button_to_x11(button: i16) -> Option<u8> {
    match button {
        0 => Some(1),
        1 => Some(2),
        2 => Some(3),
        3 => Some(8),
        4 => Some(9),
        _ => None,
    }
}

/// Toward zero, then clamped to `i16` (NaN becomes 0, as it did crossing the
/// old JS boundary).
pub fn quantize(n: f64) -> i16 {
    n.trunc().clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
}

/// `getBoundingClientRect()` of the canvas.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CanvasRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// A pointer at (`client_x`, `client_y`) in framebuffer pixels; (0, 0) while
/// the canvas has no size or no frame has been announced.
pub fn canvas_to_fb(client_x: f64, client_y: f64, rect: CanvasRect, fb: (u32, u32)) -> (i16, i16) {
    if rect.width <= 0.0 || rect.height <= 0.0 || fb.0 == 0 || fb.1 == 0 {
        return (0, 0);
    }
    (
        quantize((client_x - rect.left) / rect.width * f64::from(fb.0)),
        quantize((client_y - rect.top) / rect.height * f64::from(fb.1)),
    )
}

/// One wheel tick per axis whatever the delta's unit; `None` when neither moved.
pub fn wheel_ticks(delta_x: f64, delta_y: f64) -> Option<(i16, i16)> {
    let sign = |d: f64| {
        if d > 0.0 {
            1
        } else if d < 0.0 {
            -1
        } else {
            0
        }
    };
    let (dx, dy) = (sign(delta_x), sign(delta_y));
    (dx != 0 || dy != 0).then_some((dx, dy))
}

/// The pointer-move the canvas sends when the pointer leaves it, so the
/// server can hide its cursor.
pub const POINTER_LEFT: (i16, i16) = (-1, -1);

#[cfg(test)]
mod tests {
    use super::*;

    const RECT: CanvasRect = CanvasRect { left: 10.0, top: 20.0, width: 960.0, height: 540.0 };

    #[test]
    fn buttons_map_to_x11_and_others_are_dropped() {
        let got: Vec<_> = (0..6).map(dom_button_to_x11).collect();
        assert_eq!(got, [Some(1), Some(2), Some(3), Some(8), Some(9), None]);
        assert_eq!(dom_button_to_x11(-1), None);
    }

    #[test]
    fn the_canvas_scales_to_the_framebuffer() {
        // A 1920x1080 framebuffer drawn at half size, offset (10, 20).
        assert_eq!(canvas_to_fb(10.0, 20.0, RECT, (1920, 1080)), (0, 0));
        assert_eq!(canvas_to_fb(490.0, 290.0, RECT, (1920, 1080)), (960, 540));
        assert_eq!(canvas_to_fb(10.9, 20.9, RECT, (1920, 1080)), (1, 1), "toward zero");
    }

    #[test]
    fn no_size_means_the_origin() {
        assert_eq!(canvas_to_fb(500.0, 300.0, RECT, (0, 0)), (0, 0));
        let flat = CanvasRect { width: 0.0, ..RECT };
        assert_eq!(canvas_to_fb(500.0, 300.0, flat, (1920, 1080)), (0, 0));
    }

    #[test]
    fn quantize_clamps_and_maps_nan_to_zero() {
        assert_eq!(quantize(-1.7), -1);
        assert_eq!(quantize(1e9), i16::MAX);
        assert_eq!(quantize(-1e9), i16::MIN);
        assert_eq!(quantize(f64::NAN), 0);
    }

    #[test]
    fn the_wheel_sends_signs_and_nothing_for_no_motion() {
        assert_eq!(wheel_ticks(0.0, 120.0), Some((0, 1)));
        assert_eq!(wheel_ticks(-3.0, -0.5), Some((-1, -1)));
        assert_eq!(wheel_ticks(0.0, -0.0), None);
    }
}
```

- [ ] **Step 2: Register the module, then run and break it.**

  Add `pub mod input_map;` and run `cargo test -p ghostframe-client-wasm --lib app::input_map`. Expected: `5 passed`.

  Then try each mutation below; each must fail a test. Revert after each.
  - Map `3 => Some(4)`.
  - Use `.round()` in place of `.trunc()`.
  - Drop the `|| dy != 0`.

- [ ] **Step 3: Move the keysym cases.**

  `ghostframe-web-client/tests/input.test.ts` asserts these 9 `keyToKeysym` cases:

  | Input | Expected |
  |---|---|
  | `"Dead"`, `"Unidentified"` | `None` |
  | `"Enter"` | `0xff0d` |
  | `"ArrowUp"` | `0xff52` |
  | `"F1"` | `0xffbe` |
  | `"a"` | `0x61` |
  | `"ñ"` | `0xf1` |
  | `"中"` | `0x0100_0000 \| 0x4e2d` |
  | `"NotARealKey"` | `None` |
  | `" "` | `0x20` |

  It also asserts these 8 encoder byte vectors:

  | Call | Bytes |
  |---|---|
  | `encode_pointer_move(400, -2)` | `[0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]` |
  | `encode_pointer_button(5, 10, 1, true)` | `[0x05, 0x02, 0, 5, 0, 10, 1, 1]` |
  | `encode_pointer_button(5, 10, 3, false)` | `[0x05, 0x02, 0, 5, 0, 10, 3, 0]` |
  | `encode_wheel(0, 1)` | `[0x05, 0x03, 0, 0, 0, 1]` |
  | `encode_key_down(0xff0d)` | `[0x05, 0x04, 0, 0, 0xff, 0x0d]` |
  | `encode_key_up(0x61)` | `[0x05, 0x05, 0, 0, 0, 0x61]` |
  | `encode_pointer_move(-1, -1)` | `[0x05, 0x01, 0xff, 0xff, 0xff, 0xff]` |
  | — | `INPUT_MSG_TYPE == 0x05` |

  Read `ghostframe-client-core/src/input.rs`'s test module (lines 170 onward). For **each case its tests do not already assert**, add it to the matching existing test, with the values exactly as listed. Do not duplicate a case already asserted. In the task report, list which cases you added and which were already covered.

- [ ] **Step 4: Run.** `cargo test -p ghostframe-client-core --lib input 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"`. Expected: rc=0.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/input_map.rs ghostframe-client-core/src/input.rs
git commit -m "feat(web): input mapping in Rust; keysym cases from input.test.ts move to the core"
```

---

### Task 5: The H.264 decoder state machine [sonnet]

**Files:** Create `ghostframe-client-wasm/src/app/decoder.rs`; modify `src/app/mod.rs` (`pub mod decoder;`).

This is `decoder.ts` without the browser. Behaviour must match it line for line. The tests are the vitest suites `h264_frame_seq.test.ts` and the `FullFrameDecoder`/SPS half of `h264_result.test.ts`, ported. Read both TS files on master before starting.

- [ ] **Step 1: Write `src/app/decoder.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The decisions of the WebCodecs `VideoDecoder` wrapper (port of
//! `decoder.ts`): what to tell the core about every access unit, when to
//! rebuild the decoder after a failure, and which codec string to configure.
//! `webcodecs.rs` is the browser half.
//!
//! The browser never calls `output` or `error` from inside `decode()`,
//! `configure()` or `close()`; both arrive as their own tasks, tagged with the
//! generation `open` was given, so a late callback from a replaced decoder is
//! recognised and ignored.

use std::collections::VecDeque;

use ghostframe_client_core::H264Outcome;

/// What a decoder is configured with before the first SPS says better.
pub const DEFAULT_CODEC: &str = "avc1.64002a";
/// The coded size the decoder is configured with (`main.ts` passed 1920x1080;
/// WebCodecs takes the real size from the stream).
pub const CODED_SIZE: (u32, u32) = (1920, 1080);

/// The WebCodecs codec string (`avc1.PPCCLL`) of an Annex-B access unit, read
/// from its SPS: profile_idc, the constraint flags and level_idc. The encoder
/// decides these (libva-direct writes High at level 4.2 or above by frame size
/// and rate, openh264 Main or Baseline), and a hardware decoder configured for
/// another profile may refuse the stream. `None` when the unit has no SPS.
pub fn codec_string_from_sps(au: &[u8]) -> Option<String> {
    let mut i = 0;
    while i + 6 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 && au[i + 3] & 0x1f == 7 {
            return Some(format!("avc1.{:02x}{:02x}{:02x}", au[i + 4], au[i + 5], au[i + 6]));
        }
        i += 1;
    }
    None
}

/// The warning `main.ts` printed once a decoder had answered 30 access units
/// in a row without a picture of their own.
pub fn not_output_warning(total: u64, streak: u64) -> Option<String> {
    (streak == 30).then(|| {
        format!(
            "H.264 decoder: no picture for 30 access units in a row ({total} in all); \
             output delay? every one is reported refused in the frame ack"
        )
    })
}

/// The browser's `VideoDecoder`, or a fake in the tests.
pub trait VideoCodec {
    /// Make a new decoder instance; its callbacks carry `generation`. The
    /// previous instance is closed or abandoned by then.
    fn open(&mut self, generation: u32);
    fn configure(&mut self, codec: &str, coded_size: (u32, u32)) -> Result<(), String>;
    /// Hand one chunk over; `timestamp` is its `frame_seq`. `Err` is the
    /// synchronous exception (`DataError`: a delta before any key chunk;
    /// `InvalidStateError`: closed).
    fn decode(&mut self, key: bool, timestamp: u32, data: &[u8]) -> Result<(), String>;
    fn is_closed(&self) -> bool;
    fn close(&mut self);
}

pub struct FrameDecoder<C: VideoCodec> {
    codec: C,
    generation: u32,
    /// Set by `close()`: the owner is done, never rebuild.
    disposed: bool,
    /// An async decode error closes the decoder for good, and the delta chunks
    /// that follow cannot be decoded without the reference they lost. Until
    /// the next key chunk, deltas are dropped; on it, a fresh decoder is built.
    awaiting_keyframe: bool,
    /// The frame_seq of the last chunk given to the decoder.
    last_frame_seq: u32,
    /// Chunks the decoder accepted and has not answered for, in decode order.
    /// An output for chunk N answers every earlier one too: WebCodecs outputs
    /// in decode order, so one that never came out never will.
    pending: VecDeque<u32>,
    /// The codec string the decoder is configured with.
    codec_string: String,
    /// The string in force before the SPS-derived one, to go back to once if
    /// the browser fails on the derived one before any picture came out.
    fallback: Option<String>,
    /// A derived string the browser refused: not tried again.
    refused: Option<String>,
    /// What the core must be told, in order: `(frame_seq, outcome)`.
    notes: Vec<(u32, H264Outcome)>,
    /// Console warnings for the shell to print.
    warnings: Vec<String>,
}

impl<C: VideoCodec> FrameDecoder<C> {
    /// Opens and configures the first decoder. An `Err` is a configure the
    /// browser threw on; the owner tries again with the next access unit, as
    /// `main.ts` did when `new FullFrameDecoder` threw.
    pub fn new(codec: C) -> Result<Self, String> {
        let mut d = FrameDecoder {
            codec,
            generation: 0,
            disposed: false,
            awaiting_keyframe: false,
            last_frame_seq: 0,
            pending: VecDeque::new(),
            codec_string: DEFAULT_CODEC.to_string(),
            fallback: None,
            refused: None,
            notes: Vec::new(),
            warnings: Vec::new(),
        };
        d.open()?;
        Ok(d)
    }

    fn open(&mut self) -> Result<(), String> {
        self.generation = self.generation.wrapping_add(1);
        self.codec.open(self.generation);
        self.codec.configure(&self.codec_string, CODED_SIZE)
    }

    /// The generation of the decoder in use.
    pub fn generation(&self) -> u32 {
        self.generation
    }

    pub fn codec_string(&self) -> &str {
        &self.codec_string
    }

    pub fn codec(&self) -> &C {
        &self.codec
    }

    pub fn codec_mut(&mut self) -> &mut C {
        &mut self.codec
    }

    /// The decoder of `generation` output a picture for `timestamp`. `false`
    /// for a replaced decoder's output: the caller closes the frame and draws
    /// nothing. `true`: the caller hands the frame to the renderer.
    pub fn on_output(&mut self, generation: u32, timestamp: u32) -> bool {
        if generation != self.generation {
            return false;
        }
        self.fallback = None; // the configured string works
        self.answer(timestamp);
        true
    }

    /// The decoder of `generation` failed (its error callback).
    pub fn on_error(&mut self, generation: u32) {
        if generation != self.generation {
            return;
        }
        self.awaiting_keyframe = true;
        // A string derived from the SPS that the browser would not decode: the
        // rebuild on the next key chunk uses the one before it, once.
        if let Some(before) = self.fallback.take() {
            self.refused = Some(std::mem::replace(&mut self.codec_string, before));
        }
        // Everything in flight before the chunk that failed produced nothing;
        // the failed chunk itself is the failure.
        let last = self.last_frame_seq;
        self.answer_all(H264Outcome::NotOutput, Some(last));
        self.notes.push((last, H264Outcome::DecodeError));
    }

    fn answer(&mut self, timestamp: u32) {
        let Some(i) = self.pending.iter().position(|&s| s == timestamp) else {
            return;
        };
        let earlier: Vec<u32> = self.pending.drain(..i).collect();
        for seq in earlier {
            self.notes.push((seq, H264Outcome::NotOutput));
        }
        self.pending.pop_front();
        self.notes.push((timestamp, H264Outcome::Decoded));
    }

    fn answer_all(&mut self, outcome: H264Outcome, except: Option<u32>) {
        for seq in std::mem::take(&mut self.pending) {
            if Some(seq) != except {
                self.notes.push((seq, outcome));
            }
        }
    }

    /// One access unit. A key chunk to a healthy decoder is just decoded
    /// (WebCodecs takes an IDR without a reset; rebuilding would discard the P
    /// frames in flight). The decoder is rebuilt only after a failure, on the
    /// key chunk that follows it. `Err` is the browser's synchronous rejection,
    /// already reported to the core as a decode error.
    pub fn decode(&mut self, data: &[u8], key: bool, frame_seq: u32) -> Result<(), String> {
        if self.disposed {
            return Ok(());
        }
        if self.awaiting_keyframe || self.codec.is_closed() {
            if !key {
                self.notes.push((frame_seq, H264Outcome::NotOutput));
                return Ok(());
            }
            if !self.codec.is_closed() {
                self.codec.close();
            }
            self.answer_all(H264Outcome::NotOutput, None);
            self.awaiting_keyframe = false;
            self.open()?;
        }
        if key {
            // The IDR says what the stream is. A key chunk is the place to
            // change: a configure() makes the next chunk start from a key chunk.
            if let Some(derived) = codec_string_from_sps(data) {
                if derived != self.codec_string && self.refused.as_deref() != Some(derived.as_str()) {
                    let before = std::mem::replace(&mut self.codec_string, derived);
                    match self.codec.configure(&self.codec_string, CODED_SIZE) {
                        Ok(()) => self.fallback = Some(before),
                        Err(e) => {
                            self.warnings.push(format!(
                                "H264 codec string {} refused; keeping {before} {e}",
                                self.codec_string
                            ));
                            self.refused = Some(std::mem::replace(&mut self.codec_string, before));
                            self.codec.configure(&self.codec_string, CODED_SIZE)?;
                        }
                    }
                }
            }
        }
        self.last_frame_seq = frame_seq;
        // Queued before the call, as decoder.ts did.
        self.pending.push_back(frame_seq);
        if let Err(e) = self.codec.decode(key, frame_seq, data) {
            // DataError: a delta while the decoder still wants a key chunk
            // (lost session-start IDR, or a key chunk the browser rejected).
            // The deltas that follow are dropped quietly.
            self.awaiting_keyframe = true;
            self.pending.pop_back();
            self.answer_all(H264Outcome::NotOutput, None);
            self.notes.push((frame_seq, H264Outcome::DecodeError));
            return Err(e);
        }
        Ok(())
    }

    /// What the core must hear (`ClientCore::h264_result`), oldest first.
    pub fn take_notes(&mut self) -> Vec<(u32, H264Outcome)> {
        std::mem::take(&mut self.notes)
    }

    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }

    pub fn close(&mut self) {
        self.disposed = true;
        if !self.codec.is_closed() {
            self.codec.close();
        }
    }
}
```

- [ ] **Step 2: Write the test module** in the same file.

  The fake is two structs: `FakeCodec`, which holds an `Rc<RefCell<FakeLog>>`, and `FakeLog`. `FakeLog` records:
  - `opens: u32`;
  - `configured: Vec<String>`, one entry per *successful* `configure`;
  - `decoded: Vec<(u32, bool)>`;
  - `closed: bool` (reset to `false` by `open`);
  - `need_key: bool` (set to `true` by `open`);
  - `refuse_codec: Option<String>`: `configure` returns `Err("unsupported")` for that string;
  - `reject_next_decode: bool`.

  `decode` returns `Err("InvalidStateError: closed")` when closed. It returns `Err("DataError: key frame required")` when `need_key && !key` or `reject_next_decode` (the latter clears the flag). Otherwise it clears `need_key` and records the chunk.

  One helper, `emit(d, ts)`, calls `d.on_output(d.generation(), ts)`.

  Port every case below; the source test is named in each test's doc line:
  1. `tags_each_output_with_its_own_seq_after_a_rejected_chunk` (h264_frame_seq #1). `decode(delta, 10)` is `Err`. `decode(key, 11)` then `emit(11)`; `decode(delta, 12)` then `emit(12)`. Notes: `[(10, DecodeError), (11, Decoded), (12, Decoded)]`. `opens == 2`.
  2. `rebuilds_after_an_async_error_on_the_next_key_chunk` (#2). Key 1 and `emit(1)`. Then set `closed = true` and `on_error(gen)`. Delta 2 is `Ok` and noted `NotOutput`, still `opens == 1`. Key 3 gives `opens == 2`. Delta 4. Emitting 3 and 4 gives `Decoded` for both.
  3. `does_not_rebuild_after_close` (#3). `close()`, then key 1: `opens == 1`, no notes.
  4. `a_late_error_from_a_replaced_decoder_is_ignored` (#4). Rebuild to generation 2, then `on_error(1)`: no new notes. Delta 6 is decoded.
  5. `after_a_rejected_delta_before_key_later_deltas_drop_quietly` (#5).
  6. `decoded_means_the_output_fired_for_its_own_timestamp` (h264_result "decoded means ...").
  7. `an_output_overtakes_the_chunks_before_it`: `[(10, NotOutput), (11, NotOutput), (12, Decoded)]`.
  8. `an_async_error_answers_in_flight_chunks_and_reports_the_last`: `[(1, NotOutput), (2, NotOutput), (3, DecodeError)]`, then delta 4 gives `(4, NotOutput)`.
  9. `a_chunk_the_browser_rejects_is_a_failure_not_a_pending_frame`. Set `reject_next_decode` after key 1. Delta 2 is `Err`. Notes `[(1, NotOutput), (2, DecodeError)]`.
  10. `an_idr_to_a_healthy_decoder_neither_rebuilds_nor_discards`. Keys 1 and 9, delta 2, `opens == 1`. Emitting 2 then 9 gives `[(1, NotOutput), (2, Decoded), (9, Decoded)]`.
  11. `after_a_failure_the_next_key_chunk_rebuilds`.
  12. `the_sps_gives_profile_constraints_and_level`: the three strings of the TS test (`avc1.64002a`, `avc1.4d401e`, the SPS followed by an IDR NAL).
  13. `no_codec_string_in_a_delta_or_a_truncated_sps`: the three `None` cases.
  14. `starts_as_high_4_2_and_follows_the_stream`. `configured` sequence `["avc1.64002a"]`, then the same after a High IDR, then `["avc1.64002a", "avc1.4d401f"]` after two Main IDRs. A delta changes nothing.
  15. `keeps_the_previous_string_when_configure_refuses`. `configured == ["avc1.64002a", "avc1.64002a"]` and one warning. A second Main IDR does not try `4d401f` again.
  16. `goes_back_once_when_the_derived_string_fails_before_any_picture`. A Main IDR, then `on_error(gen)` without `closed`, then a Main IDR again. The last configured string is `avc1.64002a`, `opens == 2`, and `codec_string() == "avc1.64002a"`.
  17. `the_not_output_warning_fires_once_at_30`. `not_output_warning(40, 29)` is `None`, `(40, 30)` is `Some`, `(41, 31)` is `None`.

- [ ] **Step 3: Run.** Add `pub mod decoder;`. Run `cargo test -p ghostframe-client-wasm --lib app::decoder 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"`. Expected: `17 passed`.

- [ ] **Step 4: Break and watch it fail.** Apply each mutation alone and name the tests that fail. Each must fail at least one. Revert all.
  - Drop the `if generation != self.generation { return; }` in `on_error`.
  - Replace `self.pending.pop_back()` with nothing.
  - In `answer`, drop the `for seq in earlier` loop.
  - Remove `self.refused.as_deref() != Some(...)`.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/decoder.rs
git commit -m "feat(web): the H.264 decoder state machine in Rust, with decoder.ts's tests"
```

---

### Task 6: Core entry points for the Rust client, and the core tests that move [sonnet]

**Files:**
- Modify `ghostframe-client-wasm/src/core.rs`.
- Modify `src/units.rs` (the two converters become `pub(crate)`).
- Delete these vitest files: `ghostframe-web-client/tests/datagram_batch.test.ts`, `eviction.test.ts`, `h264_ack.test.ts`, `reset_request.test.ts`, `tile_assembly.test.ts`, and the first `describe` of `h264_result.test.ts`. Delete `h264_result.test.ts` whole in Task 14, once `decoder.ts` is gone.

- [ ] **Step 1: Extract two functions and make the constructor use them.** In `core.rs`, add as free functions (pub, host-compiled):

```rust
/// The browser's `ClientConfig`. `tile_delivery_payload = true` gives
/// validated, undecoded payloads for the GPU (`TileDelivery::Payload`); false
/// would route tiles down the native Decoded path and hand back RGBA, which
/// looks like it works and is wrong.
pub fn client_config(
    indices_raw_enabled: bool,
    supports_h264: bool,
    tile_delivery_payload: bool,
) -> ClientConfig {
    ClientConfig {
        indices_raw_enabled,
        supports_h264,
        // The browser runs the same reference gate as the native client and
        // sends the v2 feedback (HELLO bit 2): frame acks (0x0A) and reset
        // requests (0x09). H.264 itself is gated by `supports_h264` alone.
        h264_acked_ref: true,
        tile_delivery: if tile_delivery_payload {
            TileDelivery::Payload
        } else {
            TileDelivery::Decoded
        },
        cdf53_wire: false,
        display: None,
    }
}

/// A core for the browser, with this build's version for the stats panel.
pub fn new_client_core(config: ClientConfig, now_us: u64) -> ClientCore {
    let mut core = ClientCore::new(config, now_us);
    core.set_client_version(crate::CLIENT_VERSION);
    core
}

/// Feed a batch of datagrams (`concat` back to back, `lens[i]` the i-th
/// length, `arrivals_us[i]` its arrival) and return the events in arrival
/// order. Malformed batches are processed up to the first inconsistency.
pub fn feed_datagrams(
    core: &mut ClientCore,
    concat: &[u8],
    lens: &[u32],
    arrivals_us: &[f64],
) -> Vec<ghostframe_client_core::Event> {
    // (body: the loop of today's `collect_datagram_events`, verbatim, over `core`)
}
```

  Then wire the existing code through them:
  - `WasmClientCore::new` becomes `Rc::new(RefCell::new(new_client_core(client_config(..), now_us)))`.
  - `collect_datagram_events` becomes `feed_datagrams(&mut self.inner.borrow_mut(), …)`.
  - The doc comments move with the code.

  Delete the `#[cfg(target_arch = "wasm32")] impl WasmClientCore` block (`handleDatagramsInto`, `onTimeoutInto`): after Task 14 nothing calls them.

  Add these `pub(crate)` helpers, which `DebugApi` (Task 12) shares with the exports:
  - `fn cdf53_coverage_js(core: &ClientCore) -> Result<JsValue, JsValue>`;
  - `fn cdf53_incomplete_js(core: &ClientCore, limit: usize) -> Result<JsValue, JsValue>`;
  - `fn datagram_counts(core: &ClientCore) -> (u32, u32)`: the saturating conversion of today's `datagramCounts`.

  Each existing export (`cdf53Coverage`, `cdf53IncompleteTiles`, `datagramCounts`) becomes a one-line call to its helper.

- [ ] **Step 2: Make `codec_from_u8` and `decode_error_code_from_u8` in `units.rs` `pub(crate)`.**

- [ ] **Step 3: Port the moved vitest suites as host unit tests** in a new `#[cfg(test)] mod moved_from_vitest` at the bottom of `core.rs`.

  Every test builds its core with `new_client_core(client_config(…), 0)`, which is what the Rust client runs. Each test's first doc line names the TS test it replaces. Build datagrams with the protocol's own builders:
  - `ghostframe_protocol::protocol::fragment_tile` with a `TileFragmentInputs` whose `frame_seq` has `TILE_DATAGRAM_FLAG` set;
  - `build_frame_dimensions_datagram`;
  - `ghostframe_protocol::eviction::build_eviction_datagram`.

  A `wire_seq` is the big-endian `u32` at datagram bytes 8..12 (`DatagramHeader::encode`: frame_seq 4, frag_idx 2, frag_total 2, wire_seq 4, timestamp 4). H.264 frame datagrams are built byte by byte as `h264_ack.test.ts`'s `frameDatagram` does. Port it as `fn frame_datagram(frame_seq, frag_idx, frag_total, wire_seq, idr, payload) -> Vec<u8>`.

  Port these tests:
  - `datagram_batch`:
    - `a_batch_yields_the_events_of_one_datagram_at_a_time`: a scene of 5 CDF 5/3 tiles of 2 passes each, `Decoded` delivery, reversed. `feed_datagrams` gives the same events, by `Debug` string, as `handle_datagram` one at a time, plus the same `datagram_counts`. The scene must produce at least one `TileReady`.
    - `a_lying_length_array_processes_the_consistent_prefix`: the three cases of the TS test.
  - `eviction`:
    - `the_eviction_notice_is_one_evicted_event_with_its_reason`;
    - `an_ordinary_tile_never_evicts`.
  - `h264_ack`:
    - `h264_fragments_are_acknowledged_by_wire_seq_and_no_ack_ones_are_not`;
    - `a_fragment_older_than_the_window_is_still_acknowledged`.

    Decode the 0x06 batches with `ghostframe_protocol::ack::AckBatch::decode`, not by hand.
  - `h264_result` (core half):
    - `the_hello_sets_0x04_next_to_0x02`;
    - `acks_a_decoded_frame_and_a_refused_one_in_the_refused_mask`, with the exact order `[0x06, 0x0a]` and the same masks;
    - `a_decode_error_is_one_paced_decoder_failure_request`;
    - `counts_access_units_with_no_picture_of_their_own`.

    The TS `ignores an outcome it does not know` tested the JS u8 mapping. Keep it as `h264_result_ignores_an_unknown_outcome` against `WasmClientCore::h264_result(…, 99, …)`, which is host-callable.
  - `reset_request`:
    - `request_reset_queues_a_0x09_decoder_failure`;
    - `request_reset_ignores_an_unknown_reason`.

    These go through `WasmClientCore::request_reset`, since that is the exported path. Read the output with `shared().borrow_mut().poll_transmit(0)` (the `pollTransmit` export returns a `JsValue`, which panics on the host).
  - `tile_assembly`: its three `it` cases, read from the TS file.

- [ ] **Step 4: Run.**

```bash
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```
Expected: rc=0, with the pass count up by the number of ported tests.

- [ ] **Step 5: Break the browser config and watch the moved tests notice.**
  - Set `h264_acked_ref: false` in `client_config`. At least `the_hello_sets_0x04_next_to_0x02` must fail.
  - Make `feed_datagrams` stamp every datagram with `arrivals_us[0]`. Whether `a_batch_yields…` catches it depends on the scene. Report whether it does. If it does not, add an assertion that would (for example on the ACK arrival times the core queues), and show it failing.

  Revert.

- [ ] **Step 6: Delete the five vitest files** listed under Files, and the `describe('the wasm core sends the v2 H.264 feedback'` block of `h264_result.test.ts`. Then check that nothing still imports them:

```bash
cd ghostframe-web-client && grep -rn 'buildEvictionDatagram\|datagram_batch\|tile_assembly' tests src; echo "rc=$?"
```
If `helpers/wasm.ts`'s `buildEvictionDatagram` is now unused, delete it. `fragmentTile` stays (`cdf53_globals.test.ts` uses it). Expected rc after the cleanup: 1 (no matches).

- [ ] **Step 7: Commit.**

```bash
git add ghostframe-client-wasm/src/core.rs ghostframe-client-wasm/src/units.rs ghostframe-web-client/tests
git commit -m "refactor(web): core entry points for the Rust client; core vitest suites move to Rust"
```

(`git add` of the `tests` directory stages the deletions; check `git status` first, and stage nothing else.)

---

### Task 7: What each routed event does to the page [sonnet]

**Files:** Create `ghostframe-client-wasm/src/app/session.rs`; modify `src/app/mod.rs` (`pub mod session;`).

This is `main.ts`'s `handleEvent` with the e2e bookkeeping split off. It stays here for each event:
- the status text and page log lines;
- the console error for an impossible `TileReady`;
- the overlay for `Evicted`;
- the H.264 hand-off;
- the per-tick drain counts.

Everything else about an event goes to the hooks, unchanged.

- [ ] **Step 1: Write `src/app/session.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! What the page does with each routed event (port of `main.ts`'s
//! `handleEvent`, minus the e2e recorders, which `hooks.ts` keeps until PR 4).

use ghostframe_protocol::protocol::TILE_NACK_ENVELOPE;

use crate::app::close::evicted_message;
use crate::boundary::WasmEvent;
use crate::web::route::{Routed, SlimEvent, SlimTileData};

/// Events applied since the last tick, per codec, for the page log's `Δdrain`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Drained {
    pub solid: u32,
    pub palrle: u32,
    pub cdf53: u32,
    pub h264: u32,
}

#[derive(Debug, PartialEq)]
pub enum Action {
    Status(&'static str),
    Log(String),
    ConsoleError(String),
    Overlay(&'static str),
    /// Hand this access unit to the H.264 decoder.
    DecodeH264 {
        frame_seq: u32,
        is_keyframe: bool,
        payload: Vec<u8>,
    },
}

#[derive(Debug, Default)]
pub struct Session {
    first_frame_seen: bool,
    drained: Drained,
}

fn hex_spaced(bytes: &[u8]) -> String {
    bytes.iter().take(16).map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

impl Session {
    /// One routed event; its actions go to `out`. The record comes back when
    /// the hooks want it (tiles, palettes, frame sizes, decode errors).
    /// `NeedsH264` (its payload is taken for the decoder), `Evicted` and
    /// `TileReady` do not: no recorder reads them.
    pub fn on_routed(&mut self, r: Routed, out: &mut Vec<Action>) -> Option<Routed> {
        match r {
            Routed::Tile(SlimEvent::TilePayload { tile_x, tile_y, ref data, .. }) => {
                let (sample, len) = match data {
                    SlimTileData::Solid { bytes, len } => {
                        self.drained.solid += 1;
                        (bytes, *len)
                    }
                    SlimTileData::PalRle { indices, len, .. } => {
                        self.drained.palrle += 1;
                        (indices, *len)
                    }
                    SlimTileData::Cdf53 { bit_planes, len, .. } => {
                        self.drained.cdf53 += 1;
                        (bit_planes, *len)
                    }
                };
                if !self.first_frame_seen {
                    self.first_frame_seen = true;
                    out.push(Action::Log(format!("First tile: ({tile_x},{tile_y}) {len}B")));
                    out.push(Action::Log(format!("First bytes: {}", hex_spaced(sample))));
                    out.push(Action::Status("Receiving frames"));
                }
                Some(r)
            }
            Routed::Tile(SlimEvent::FrameDimensions { applied: false, ref reason, .. }) => {
                let msg = format!("FrameDimensions rejected: {}", reason.as_deref().unwrap_or(""));
                out.push(Action::ConsoleError(msg.clone()));
                out.push(Action::Log(msg));
                Some(r)
            }
            Routed::Tile(SlimEvent::FrameDimensions { .. })
            | Routed::Tile(SlimEvent::PaletteUpdated { .. })
            | Routed::Passthrough(WasmEvent::DecodeError { .. }) => Some(r),
            Routed::Tile(SlimEvent::TileReady { frame_seq, tile_x, tile_y }) => {
                out.push(Action::ConsoleError(format!(
                    "Unexpected TileReady event (seq {frame_seq}, tile {tile_x},{tile_y}): the core \
                     was constructed with tile_delivery_payload=true, so Payload-mode reassembly \
                     should never emit this. No rendering path exists for it."
                )));
                None
            }
            Routed::Passthrough(WasmEvent::NeedsH264 { frame_seq, is_keyframe, payload, .. }) => {
                self.drained.h264 += 1;
                if !self.first_frame_seen {
                    self.first_frame_seen = true;
                    let key = if is_keyframe { "(keyframe)" } else { "" };
                    out.push(Action::Log(format!("First full frame: {}B {key}", payload.len())));
                    out.push(Action::Status("Receiving frames"));
                }
                out.push(Action::DecodeH264 { frame_seq, is_keyframe, payload });
                None
            }
            Routed::Passthrough(WasmEvent::Evicted { reason }) => {
                // Terminal: the server has dropped this session. No reconnect:
                // two clients that retry evict each other indefinitely.
                out.push(Action::Overlay(evicted_message(reason)));
                None
            }
            // `handle_datagram` never returns these through `route` (it maps
            // only NeedsH264, DecodeError and Evicted to Passthrough).
            Routed::Passthrough(other) => {
                out.push(Action::ConsoleError(format!("unexpected routed event: {other:?}")));
                None
            }
        }
    }

    /// A datagram shorter than `SHORT_DATAGRAM_LEN`: M0's ping/pong text.
    pub fn on_short_datagram(&mut self, bytes: &[u8], out: &mut Vec<Action>) {
        let text = String::from_utf8_lossy(bytes);
        out.push(Action::Log(format!("Received: {text} ({} bytes)", bytes.len())));
        if text == "pong" {
            out.push(Action::Status("Ping/Pong successful!"));
            out.push(Action::Log("M0 COMPLETE: Ping/Pong datagram round-trip verified!".into()));
        }
    }

    /// The drain counts since the last call.
    pub fn take_drained(&mut self) -> Drained {
        std::mem::take(&mut self.drained)
    }
}

/// NACK entries in an outbound datagram (a leading `TILE_NACK_ENVELOPE` and a
/// count byte), for the page log's `nack_sent`.
pub fn nacks_in(datagram: &[u8]) -> u32 {
    match datagram {
        [TILE_NACK_ENVELOPE, n, ..] => u32::from(*n),
        _ => 0,
    }
}
```

  If `WasmEvent` lacks `Debug`, the compiler says so. In that case, do not derive it. Replace `{other:?}` with a fixed string naming the variant set, and keep the match exhaustive: no `_ =>` arm, because the crate warns on `wildcard_enum_match_arm`. `Routed::Passthrough(other)` here is a binding, not a wildcard. If clippy still flags it, list the remaining `WasmEvent` variants explicitly instead.

- [ ] **Step 2: Write the test module.** Each test builds `Routed` values directly:

  1. `the_first_tile_logs_and_says_receiving_frames_once`:
     - A `Solid` record with a 4-byte sample and `len: 4` gives exactly `[Log("First tile: (3,4) 4B"), Log("First bytes: 00 00 ff ff"), Status("Receiving frames")]` and comes back.
     - A second tile gives no actions.
  2. `the_first_h264_frame_also_counts_as_receiving`: a `NeedsH264` record gives the `First full frame: 5B (keyframe)` log, the status and `DecodeH264` with the payload, and does not come back.
  3. `a_rejected_size_is_logged_and_still_recorded`.
  4. `eviction_shows_the_overlay_and_is_not_recorded`: reason 1 gives the takeover text.
  5. `tile_ready_is_an_error_not_a_render`.
  6. `drain_counts_per_codec_reset_on_take`: two Solid, one PalRle, one Cdf53 and one H.264 give `Drained { 2, 1, 1, 1 }`, then all zero.
  7. `pong_is_m0`: `b"pong"` gives three actions. `b"ping"` gives one.
  8. `nacks_are_counted_from_the_envelope`:
     - `[TILE_NACK_ENVELOPE, 3, …]` gives 3;
     - `[0x06, 3]` gives 0;
     - `[TILE_NACK_ENVELOPE]` gives 0;
     - use the constant, never a literal 0x05 (memory: load-bearing catch-all arms).

  The `Status("Receiving frames")` text is load-bearing: `ghostframe-e2e`'s setup polls `#status` for it. Assert it as an exact string.

- [ ] **Step 3: Run and break.** Add `pub mod session;` and run `cargo test -p ghostframe-client-wasm --lib app::session`. Expected: `8 passed`. Then try each mutation; each must fail a test:
  - drop `self.first_frame_seen = true;` from the tile arm;
  - match `[0x05, n, ..]`, then change `TILE_NACK_ENVELOPE`'s use to a wrong literal;
  - swap `drained.palrle` and `drained.cdf53`.

  Revert.

- [ ] **Step 4: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/session.rs
git commit -m "feat(web): page-side event handling in Rust (status, log, overlay, H.264 hand-off)"
```

---

### Task 8: Browser bindings, stream pump and writers [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/js.rs` and `src/app/streams.rs`, both wasm32-only.
- Create `tests/app_io.rs`.
- Modify `src/app/mod.rs` and `Cargo.toml`.

- [ ] **Step 1: web-sys features.** Add these to the `web-sys` `features` list of `[dependencies]`: `"AddEventListenerOptions"`, `"CssStyleDeclaration"`, `"DomRect"`, `"Element"`, `"Event"`, `"EventTarget"`, `"HtmlDivElement"`, `"KeyboardEvent"`, `"Location"`, `"MouseEvent"`, `"PointerEvent"`, `"ReadableStream"`, `"ReadableStreamDefaultReader"`, `"Request"`, `"RequestCache"`, `"RequestInit"`, `"Response"`, `"UrlSearchParams"`, `"WheelEvent"`, `"WritableStream"`, `"WritableStreamDefaultWriter"`. Add to the dev-dependency `web-sys` list what the tests below call: `"ReadableStream"`, `"WritableStream"`, `"Response"`. **None of them may need `web_sys_unstable_apis`.** If one does (the compiler says the item is missing), stop and report it.

- [ ] **Step 2: Write `src/app/js.rs`.** Use the raw bindings, the same mechanism as `web/raw.rs`, with one `extern "C"` block per type:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser APIs web-sys 0.3.106 has only behind `web_sys_unstable_apis`
//! (WebTransport, WebCodecs), declared directly: the members this client
//! uses and nothing else. Same mechanism and reason as `web/raw.rs`.

use js_sys::{Object, Promise};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Object)]
    pub type WebTransport;
    #[wasm_bindgen(constructor, catch)]
    pub fn new(url: &str, options: &JsValue) -> Result<WebTransport, JsValue>;
    #[wasm_bindgen(method, getter)]
    pub fn ready(this: &WebTransport) -> Promise;
    /// Resolves with `{ closeCode, reason }`; rejects when the session failed.
    #[wasm_bindgen(method, getter)]
    pub fn closed(this: &WebTransport) -> Promise;
    #[wasm_bindgen(method, getter)]
    pub fn datagrams(this: &WebTransport) -> StreamPair;
    /// Resolves with a `{ readable, writable }` bidirectional stream.
    #[wasm_bindgen(method, js_name = createBidirectionalStream)]
    pub fn create_bidirectional_stream(this: &WebTransport) -> Promise;
}

#[wasm_bindgen]
extern "C" {
    /// `{ readable, writable }`: the datagram duplex stream or a bidi stream.
    #[wasm_bindgen(extends = Object)]
    pub type StreamPair;
    #[wasm_bindgen(method, getter)]
    pub fn readable(this: &StreamPair) -> web_sys::ReadableStream;
    #[wasm_bindgen(method, getter)]
    pub fn writable(this: &StreamPair) -> web_sys::WritableStream;
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Object)]
    pub type VideoDecoder;
    /// `init` is `{ output(frame), error(e) }`.
    #[wasm_bindgen(constructor, catch)]
    pub fn new(init: &JsValue) -> Result<VideoDecoder, JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn configure(this: &VideoDecoder, config: &JsValue) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn decode(this: &VideoDecoder, chunk: &EncodedVideoChunk) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    pub fn close(this: &VideoDecoder) -> Result<(), JsValue>;
    /// `"unconfigured"`, `"configured"` or `"closed"`.
    #[wasm_bindgen(method, getter)]
    pub fn state(this: &VideoDecoder) -> String;
}

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(extends = Object)]
    pub type EncodedVideoChunk;
    /// `init` is `{ type: "key" | "delta", timestamp, data }`.
    #[wasm_bindgen(constructor, catch)]
    pub fn new(init: &JsValue) -> Result<EncodedVideoChunk, JsValue>;
}
```

- [ ] **Step 3: Write `src/app/streams.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Reading and writing the WebTransport streams.

use std::cell::Cell;
use std::rc::Rc;

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{ReadableStream, ReadableStreamDefaultReader, WritableStreamDefaultWriter};

use crate::web::raw::js_message;

/// Reads `readable` to its end and hands each non-empty chunk to `on_chunk`
/// (port of `server_stream.ts`). A chunk `on_chunk` refuses is warned about
/// and skipped, since the next one may be fine; an error ends the read with a
/// warning. Returns when the stream ends (a closing session ends it).
pub async fn pump_stream(
    readable: ReadableStream,
    mut on_chunk: impl FnMut(&[u8]) -> Result<(), String>,
    mut warn: impl FnMut(&str, &str),
) {
    let reader: ReadableStreamDefaultReader = readable.get_reader().unchecked_into();
    loop {
        let result = match JsFuture::from(reader.read()).await {
            Ok(r) => r,
            Err(e) => {
                warn("server stream ended with an error", &js_message(&e));
                break;
            }
        };
        if Reflect::get(&result, &"done".into()).ok().and_then(|d| d.as_bool()) == Some(true) {
            break;
        }
        let Ok(value) = Reflect::get(&result, &"value".into()) else { continue };
        let Some(chunk) = value.dyn_ref::<Uint8Array>() else { continue };
        if chunk.length() == 0 {
            continue;
        }
        if let Err(e) = on_chunk(&chunk.to_vec()) {
            warn("server stream: chunk handler failed; continuing", &e);
        }
    }
    reader.release_lock();
}

/// A stream writer whose failures are logged once, as `what: error`: a
/// feedback stream that closes mid-session would otherwise log every write.
/// Writes are not awaited: the writer queues them in call order.
pub struct LoggedWriter {
    writer: WritableStreamDefaultWriter,
    on_error: Closure<dyn FnMut(JsValue)>,
}

impl LoggedWriter {
    pub fn new(writer: WritableStreamDefaultWriter, what: &'static str) -> Self {
        let logged = Rc::new(Cell::new(false));
        let on_error = Closure::new(move |e: JsValue| {
            if !logged.replace(true) {
                web_sys::console::warn_1(&format!("{what}: {}", js_message(&e)).into());
            }
        });
        LoggedWriter { writer, on_error }
    }

    /// The same stream, with its own once-only failure line.
    pub fn labelled(&self, what: &'static str) -> Self {
        LoggedWriter::new(self.writer.clone(), what)
    }

    pub fn write(&self, bytes: &[u8]) {
        let promise = self.writer.write_with_chunk(&Uint8Array::from(bytes));
        let _ = promise.catch(&self.on_error);
    }
}
```

  `web/raw.rs` already has `js_message`. If it is not `pub`, make it `pub(crate)`. If `ReadableStream::get_reader`'s name or return type differs from the Findings, adapt it and say so in the task report.

- [ ] **Step 4: Register the modules.** In `src/app/mod.rs`, add:

```rust
#[cfg(target_arch = "wasm32")]
pub mod js;
#[cfg(target_arch = "wasm32")]
pub mod streams;
```

- [ ] **Step 5: Write the browser test `tests/app_io.rs`** (port of `server_stream.test.ts`). Use `#![cfg(target_arch = "wasm32")]` and `wasm_bindgen_test_configure!(run_in_browser)`.

  Build the streams with a JS helper:

```rust
fn stream_of(chunks: &[&[u8]], fail: bool) -> web_sys::ReadableStream {
    let arr = js_sys::Array::new();
    for c in chunks {
        arr.push(&js_sys::Uint8Array::from(*c));
    }
    let make = js_sys::Function::new_with_args(
        "chunks, fail",
        "let i = 0; return new ReadableStream({ pull(c) { \
           if (i < chunks.length) c.enqueue(chunks[i++]); \
           else if (fail) c.error(new Error('reset')); else c.close(); } });",
    );
    make.call2(&JsValue::NULL, &arr, &JsValue::from_bool(fail)).unwrap().unchecked_into()
}
```

  The three tests are:
  1. `the_pump_delivers_every_non_empty_chunk_in_order`: `[[1, 2], [], [3]]` gives `[[1, 2], [3]]`.
  2. `the_pump_keeps_reading_after_a_chunk_is_refused`: the chunk `[2]` is refused, so `seen == [1, 3]` and there is exactly one warning.
  3. `the_pump_ends_with_a_warning_when_the_stream_errors`: one warning.

  Add a fourth, `a_logged_writer_writes_in_order`. It builds `new WritableStream({ write(c) { globalThis.__w.push(Array.from(c)); } })` with a JS helper, writes `[1]` and `[2, 3]` through a `LoggedWriter`, and waits one macrotask (await a `Promise` resolved by `setTimeout(…, 0)`). `globalThis.__w` must equal `[[1], [2, 3]]`.

- [ ] **Step 6: Build for wasm32 and clippy.** The browser run itself is gated by G3, so it waits for CI or the controller.

```bash
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```
Expected: rc=0.

- [ ] **Step 7: Commit.**

```bash
git add ghostframe-client-wasm/Cargo.toml ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/js.rs ghostframe-client-wasm/src/app/streams.rs ghostframe-client-wasm/src/web/raw.rs ghostframe-client-wasm/tests/app_io.rs
git commit -m "feat(web): WebTransport and WebCodecs bindings, stream pump and logged writers"
```

---

### Task 9: Bootstrap: `/config.json` and the same-origin WebTransport [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/bootstrap.rs` (wasm32).
- Modify `src/app/mod.rs` and `tests/app_io.rs`.

- [ ] **Step 1: Write `src/app/bootstrap.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `bootstrap.ts`: fetch `/config.json` afresh, pin its certificate
//! hashes, open the same-origin WebTransport.

use js_sys::{Array, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use web_sys::{RequestCache, RequestInit, Response};

use crate::app::certs::{hex_to_bytes, pick_cert_hashes};
use crate::app::js::WebTransport;
use crate::web::raw::{js_message, obj};

/// Fetched on every call, never cached: the server rotates its certificate,
/// and a hash remembered from an earlier day pins a cert that is gone
/// (`no-store` also overrides any intermediary's heuristics).
pub async fn bootstrap() -> Result<WebTransport, String> {
    let window = web_sys::window().ok_or("bootstrap: no window")?;
    let init = RequestInit::new();
    init.set_cache(RequestCache::NoStore);
    let resp: Response = JsFuture::from(window.fetch_with_str_and_init("/config.json", &init))
        .await
        .map_err(|e| format!("bootstrap: fetch /config.json: {}", js_message(&e)))?
        // `unchecked_into`, not `dyn_into`: the tests stub `fetch` with a plain
        // object that has the members this reads (`ok`, `status`, `json`).
        .unchecked_into();
    if !resp.ok() {
        return Err(format!("bootstrap: /config.json returned HTTP {}", resp.status()));
    }
    let cfg = JsFuture::from(resp.json().map_err(|e| js_message(&e))?)
        .await
        .map_err(|e| format!("bootstrap: /config.json is not JSON: {}", js_message(&e)))?;
    let listed: Option<Vec<Option<String>>> = Reflect::get(&cfg, &"certHashes".into())
        .ok()
        .filter(Array::is_array)
        .map(|v| Array::from(&v).iter().map(|h| h.as_string()).collect());
    let single = Reflect::get(&cfg, &"certHash".into()).ok().and_then(|v| v.as_string());
    let hashes = pick_cert_hashes(listed.as_deref(), single.as_deref())?;

    let pins = Array::new();
    for h in &hashes {
        let bytes = hex_to_bytes(h)?;
        pins.push(&obj(&[
            ("algorithm", "sha-256".into()),
            ("value", Uint8Array::from(bytes.as_slice()).buffer().into()),
        ]));
    }
    let options = obj(&[
        // A hint to the browser's own congestion controller (its sending
        // direction: input, ACKs, NACKs): favour latency. A browser that does
        // not know the member ignores it.
        ("congestionControl", "low-latency".into()),
        ("serverCertificateHashes", pins.into()),
    ]);
    let host = window.location().host().map_err(|e| js_message(&e))?;
    WebTransport::new(&format!("https://{host}/"), &options).map_err(|e| js_message(&e))
}
```

  If `raw::obj` takes another argument type, use what it takes and keep the keys.

- [ ] **Step 2: Register the module.** Add `#[cfg(target_arch = "wasm32")] pub mod bootstrap;` to `src/app/mod.rs`.

- [ ] **Step 3: Write the browser tests.** These go in `tests/app_io.rs`, as a port of `bootstrap.test.ts`.

  - **Stubs.** Use one helper that installs a stub `globalThis.fetch` and a stub `globalThis.WebTransport`, then restores the originals with a guard's `Drop`.
    - The stub `fetch` records `[url, init.cache]` into `globalThis.__fetches`. It answers from `globalThis.__answers.shift()`, an array of `{ ok, status, json }` that the test sets.
    - The stub `WebTransport` is a class whose constructor records `[url, opts]` into `globalThis.__wts`.
    - Write the stubs as `js_sys::Function::new_with_args` bodies.
  - **Tests.** The five tests are:
    1. `bootstrap_opens_a_same_origin_low_latency_transport_with_the_pin`:
       - `url == format!("https://{}/", location.host)`;
       - `congestionControl == "low-latency"`;
       - one pin, `algorithm == "sha-256"`;
       - its `value` is 32 bytes, with `[0] == 0x00`, `[1] == 0x11` and `[31] == 0xff`;
       - `init.cache == "no-store"`.
    2. `bootstrap_pins_every_listed_hash`: two hashes, `aa…` then `bb…`.
    3. `bootstrap_fetches_afresh_on_every_call`: two calls, two fetches, each `no-store`, and the second pins `22…`.
    4. `bootstrap_names_http_errors`: `{ ok: false, status: 500 }` gives `Err` containing `/config.json returned HTTP 500`.
    5. `bootstrap_names_a_missing_cert_hash`: `{}` gives `Err` containing `certHash`.
  - **Order.** wasm-bindgen-test runs a file's tests one after another in one page, so the guard's restore is what keeps them independent. Each test must restore even on failure. Use the guard, not cleanup code at the end of the test.

- [ ] **Step 4: Run clippy on wasm32** as in Task 8, Step 6. Expected: rc=0.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/bootstrap.rs ghostframe-client-wasm/tests/app_io.rs ghostframe-client-wasm/src/web/raw.rs
git commit -m "feat(web): bootstrap (/config.json, pins, WebTransport) in Rust"
```

---

### Task 10: The WebCodecs shell [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/webcodecs.rs` (wasm32) and `tests/app_decoder.rs`.
- Modify `src/app/mod.rs`.

- [ ] **Step 1: Write `src/app/webcodecs.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! `VideoCodec` over the browser's WebCodecs `VideoDecoder`.

use std::rc::Rc;

use js_sys::{Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

use crate::app::decoder::VideoCodec;
use crate::app::js::{EncodedVideoChunk, VideoDecoder};
use crate::web::raw::{js_message, obj};

/// Called with the generation of the decoder that produced it and the
/// `VideoFrame` (whose `timestamp` is the chunk's `frame_seq`).
pub type OnOutput = Rc<dyn Fn(u32, JsValue)>;
/// Called with the generation of the decoder that failed and the message.
pub type OnError = Rc<dyn Fn(u32, String)>;

type Callbacks = (Closure<dyn FnMut(JsValue)>, Closure<dyn FnMut(JsValue)>);

pub struct WebCodec {
    decoder: Option<VideoDecoder>,
    on_output: OnOutput,
    on_error: OnError,
    /// Every generation's callbacks, kept until this codec is dropped: a late
    /// callback from a replaced decoder must find its closure alive (a dropped
    /// one throws, and the frame it carried would never be closed). Rebuilds
    /// happen only after a decode failure, so this stays small.
    callbacks: Vec<Callbacks>,
}

impl WebCodec {
    pub fn new(on_output: OnOutput, on_error: OnError) -> Self {
        WebCodec { decoder: None, on_output, on_error, callbacks: Vec::new() }
    }
}

/// `frame.timestamp` as the `frame_seq` it carries.
pub fn frame_seq_of(frame: &JsValue) -> u32 {
    Reflect::get(frame, &"timestamp".into())
        .ok()
        .and_then(|t| t.as_f64())
        .map_or(0, |t| t as u32)
}

impl VideoCodec for WebCodec {
    fn open(&mut self, generation: u32) {
        let out = self.on_output.clone();
        let output = Closure::new(move |frame: JsValue| out(generation, frame));
        let err = self.on_error.clone();
        let error = Closure::new(move |e: JsValue| err(generation, js_message(&e)));
        let init = obj(&[
            ("output", output.as_ref().clone()),
            ("error", error.as_ref().clone()),
        ]);
        // A constructor that throws leaves no decoder: `is_closed` reports
        // closed, so the next key chunk tries again.
        self.decoder = VideoDecoder::new(&init).ok();
        self.callbacks.push((output, error));
    }

    fn configure(&mut self, codec: &str, coded_size: (u32, u32)) -> Result<(), String> {
        let d = self.decoder.as_ref().ok_or("no VideoDecoder")?;
        let config = obj(&[
            ("codec", codec.into()),
            ("codedWidth", coded_size.0.into()),
            ("codedHeight", coded_size.1.into()),
            ("optimizeForLatency", true.into()),
        ]);
        d.configure(&config).map_err(|e| js_message(&e))
    }

    fn decode(&mut self, key: bool, timestamp: u32, data: &[u8]) -> Result<(), String> {
        let d = self.decoder.as_ref().ok_or("no VideoDecoder")?;
        let init = obj(&[
            ("type", if key { "key" } else { "delta" }.into()),
            ("timestamp", timestamp.into()),
            ("data", Uint8Array::from(data).into()),
        ]);
        let chunk = EncodedVideoChunk::new(&init).map_err(|e| js_message(&e))?;
        d.decode(&chunk).map_err(|e| js_message(&e))
    }

    fn is_closed(&self) -> bool {
        self.decoder.as_ref().is_none_or(|d| d.state() == "closed")
    }

    fn close(&mut self) {
        if let Some(d) = &self.decoder {
            let _ = d.close();
        }
    }
}
```

- [ ] **Step 2: Register the module.** Add `#[cfg(target_arch = "wasm32")] pub mod webcodecs;` to `src/app/mod.rs`.

- [ ] **Step 3: Write `tests/app_decoder.rs`** (browser). This test decodes the probe clip:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The real `VideoDecoder` through `FrameDecoder<WebCodec>`: the frame_seq of an
//! access unit comes back on its picture, and the core hears "decoded".
#![cfg(target_arch = "wasm32")]

const PROBE_CLIP: &[u8] = include_bytes!("../../ghostframe-client-h264/src/probe_clip.h264");
```

  The test, `the_real_decoder_decodes_the_probe_clip`, proceeds as follows:
  1. Build a `WebCodec`:
     - `on_output` pushes `(generation, frame_seq_of(&frame))` into an `Rc<RefCell<Vec<_>>>`, closes the frame with `crate::web::raw::close_frame` (or its public path), and resolves a JS `Promise` (keep its `resolve` function in an `Rc<RefCell<Option<js_sys::Function>>>`);
     - `on_error` records the message and resolves the same promise.
  2. Run `FrameDecoder::new(codec)`, then `decode(PROBE_CLIP, true, 77)`.
  3. Await the promise, raced against a 10 s `setTimeout` reject.
  4. Feed the output to `on_output(generation, 77)`.
  5. Assert:
     - no error was recorded (if one was, fail with its message);
     - the output's `frame_seq == 77`;
     - `codec_string() == "avc1.64000a"`, read from the clip's SPS;
     - `take_notes() == [(77, H264Outcome::Decoded)]`.

  A failure here on CI but not locally is **gate G2**.

- [ ] **Step 4: Run clippy on wasm32.** Expected: rc=0.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/webcodecs.rs ghostframe-client-wasm/tests/app_decoder.rs
git commit -m "feat(web): WebCodecs VideoDecoder driven from Rust"
```

---

### Task 11: Page DOM: status, log, overlay, lossless chrome, flags [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/dom.rs` (wasm32).
- Modify `src/app/mod.rs` and `tests/app_io.rs`.

- [ ] **Step 1: Write `src/app/dom.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The page itself: the status line, the log, the disconnect overlay, the
//! lossless page chrome and the URL flags.

use wasm_bindgen::JsCast;
use web_sys::{Document, HtmlCanvasElement, HtmlElement};

use crate::app::page::PageFlags;

/// Lines the page log keeps.
pub const LOG_LINES: u32 = 50;

/// `#status` and `#log` of `index.html`.
pub struct PageLog {
    document: Document,
    status: HtmlElement,
    log: HtmlElement,
}

impl PageLog {
    pub fn find(document: &Document) -> Result<Self, String> {
        let get = |id: &str| -> Result<HtmlElement, String> {
            document
                .get_element_by_id(id)
                .and_then(|e| e.dyn_into::<HtmlElement>().ok())
                .ok_or_else(|| format!("#{id} is missing from index.html"))
        };
        Ok(PageLog { document: document.clone(), status: get("status")?, log: get("log")? })
    }

    pub fn status(&self, text: &str) {
        self.status.set_text_content(Some(text));
    }

    /// Append one line, keeping the last `LOG_LINES`.
    pub fn line(&self, text: &str) {
        if let Ok(div) = self.document.create_element("div") {
            div.set_text_content(Some(text));
            let _ = self.log.append_child(&div);
        }
        while self.log.child_element_count() > LOG_LINES {
            match self.log.first_child() {
                Some(first) => {
                    let _ = self.log.remove_child(&first);
                }
                None => break,
            }
        }
    }

    /// `?e2e=lossless`: hide the status and log (they still work; only the
    /// screenshot must not show them).
    pub fn hide(&self) {
        let _ = self.status.style().set_property("display", "none");
        let _ = self.log.style().set_property("display", "none");
    }
}

/// `?e2e=lossless`: pin the canvas top-left so a CDP screenshot holds only it.
pub fn pin_canvas(canvas: &HtmlCanvasElement) {
    let style = canvas.style();
    let _ = style.set_property("position", "fixed");
    let _ = style.set_property("top", "0");
    let _ = style.set_property("left", "0");
}

/// Terminal disconnect overlay. Deliberately no reconnect button: reloading
/// is the reconnect, and a button invites two clients evicting each other.
/// Shown once; a second call keeps the first message.
pub fn show_disconnected_overlay(document: &Document, message: &str) {
    if document.get_element_by_id("gf-disconnected").is_some() {
        return;
    }
    let Ok(el) = document.create_element("div") else { return };
    el.set_id("gf-disconnected");
    let _ = el.set_attribute("role", "status");
    el.set_text_content(Some(message));
    let _ = el.set_attribute(
        "style",
        "position:fixed;inset:0;display:flex;align-items:center;justify-content:center;\
         background:rgba(0,0,0,0.82);color:#fff;font:16px system-ui,sans-serif;\
         z-index:9999;text-align:center;padding:2rem",
    );
    if let Some(body) = document.body() {
        let _ = body.append_child(&el);
    }
}

/// The flags of the page URL.
pub fn page_flags(window: &web_sys::Window) -> PageFlags {
    let search = window.location().search().unwrap_or_default();
    let Ok(params) = web_sys::UrlSearchParams::new_with_str(&search) else {
        return PageFlags::from_params(&[], None);
    };
    let e2e: Vec<String> = params.get_all("e2e").iter().filter_map(|v| v.as_string()).collect();
    PageFlags::from_params(&e2e, params.get("cdf53").as_deref())
}
```

  If a web-sys method name differs in 0.3.106 (`style()` needs `HtmlElement`, `child_element_count` needs `Element`), adapt it and report the change.

- [ ] **Step 2: Register the module.** Add `#[cfg(target_arch = "wasm32")] pub mod dom;` to `src/app/mod.rs`.

- [ ] **Step 3: Add the browser tests to `tests/app_io.rs`.** Each test builds its own `#status` and `#log` in a fresh container and removes them at the end:
  1. `the_page_log_keeps_the_last_50_lines`: write 60 lines; 50 children remain, and the first is `line 10`.
  2. `the_overlay_is_shown_once_with_role_status`: two calls give one `#gf-disconnected`, with the first message and `role="status"`.
  3. `lossless_hides_the_chrome_and_pins_the_canvas`.

- [ ] **Step 4: Run clippy on wasm32.** Expected: rc=0.

- [ ] **Step 5: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/dom.rs ghostframe-client-wasm/tests/app_io.rs
git commit -m "feat(web): page status, log, overlay and URL flags in Rust"
```

---

### Task 12: The hooks interface and `DebugApi` [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/hooks.rs` and `src/app/debug_api.rs` (wasm32).
- Modify `src/app/mod.rs`, `src/web/renderer.rs` and `src/web/logic.rs`.

- [ ] **Step 1: Add `summarize_us` to `src/web/logic.rs`** (host-tested). It is the body of today's `paint_us_stats`, made free:

```rust
/// `[count, p50, p99, max]` of `samples` (microseconds), for the bench's
/// `__ghostframePaintUs` and `__ghostframeTickUs`; zeros when empty.
pub fn summarize_us(mut samples: Vec<u32>) -> Vec<f64> {
    samples.sort_unstable();
    let at = |q: f64| {
        samples
            .get(((samples.len() as f64 - 1.0) * q).round() as usize)
            .copied()
            .map_or(0.0, f64::from)
    };
    vec![samples.len() as f64, at(0.5), at(0.99), samples.last().copied().map_or(0.0, f64::from)]
}
```

  Add a test: `[]` gives `[0, 0, 0, 0]`. `[5, 1, 3]` gives `[3, 3, 5, 5]`, where p99 rounds to index 2. Then make `WebRenderer::paint_us_stats` call it.

- [ ] **Step 2: Add the Rust-side drains in `src/web/renderer.rs`.**
  - Change `async fn create_inner` to `pub(crate) async fn create_inner`.
  - Add `pub(crate) fn take_error_strings(&mut self) -> Vec<String>`: the body of `take_errors` without the `Array` map. `take_errors` then maps it.

  `take_decode_errors` already returns `Vec<u32>` and stays as it is.

- [ ] **Step 3: Write `src/app/hooks.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The page's e2e bookkeeping (`ghostframe-web-client/src/hooks.ts`), called
//! from Rust until PR 4 moves it here. Every entry point queues its calls and
//! dispatches them with no Rust borrow held: a hook may call back into
//! `DebugApi`. A hook that throws is logged once per method and skipped.

use std::cell::RefCell;
use std::collections::HashSet;

use serde::Serialize;
use wasm_bindgen::prelude::*;

use crate::app::debug_api::DebugApi;
use crate::app::session::Drained;
use crate::web::raw::{js_message, obj};
use crate::web::route::Routed;

#[wasm_bindgen]
extern "C" {
    pub type Hooks;
    #[wasm_bindgen(method, catch)]
    fn attach(this: &Hooks, api: DebugApi) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    fn events(this: &Hooks, records: JsValue, at_ms: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = datagramCounts)]
    fn datagram_counts(this: &Hooks, tile: u32, frame: u32) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = rafTick)]
    fn raf_tick(this: &Hooks, n: u32) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = beforePaint)]
    fn before_paint(this: &Hooks, at_ms: f64) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = afterPaint)]
    fn after_paint(this: &Hooks, info: JsValue) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch)]
    fn swallowed(this: &Hooks, site: &str, message: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = rendererError)]
    fn renderer_error(this: &Hooks, tick: u32, message: &str) -> Result<(), JsValue>;
    #[wasm_bindgen(method, catch, js_name = serverStream)]
    fn server_stream(this: &Hooks, samples: u32, frames: u32) -> Result<(), JsValue>;
}

/// One call, queued while `App` is borrowed.
#[derive(Debug)]
pub enum HookCall {
    Events { at_ms: f64, records: Vec<Routed> },
    DatagramCounts { tile: u32, frame: u32 },
    RafTick(u32),
    BeforePaint { at_ms: f64 },
    AfterPaint { drained: Drained, raf_ticks: u32, nack_sent: u32 },
    Swallowed { site: String, message: String },
    RendererError { tick: u32, message: String },
    ServerStream { samples: u32, frames: u32 },
}

thread_local! {
    static WARNED: RefCell<HashSet<&'static str>> = RefCell::new(HashSet::new());
}

fn check(method: &'static str, r: Result<(), JsValue>) {
    if let Err(e) = r {
        if WARNED.with(|w| w.borrow_mut().insert(method)) {
            web_sys::console::warn_1(&format!("hooks.{method} threw: {}", js_message(&e)).into());
        }
    }
}

/// `Routed` as `handleDatagramsInto` returned it (maps as plain objects).
fn records_js(records: &[Routed]) -> JsValue {
    records
        .serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .unwrap_or(JsValue::NULL)
}

pub fn attach(hooks: &Hooks, api: DebugApi) {
    check("attach", hooks.attach(api));
}

/// Run `calls` in order. Call with no `App` or renderer borrow held.
pub fn dispatch(hooks: Option<&Hooks>, calls: Vec<HookCall>) {
    let Some(h) = hooks else { return };
    for c in calls {
        match c {
            HookCall::Events { at_ms, records } => check("events", h.events(records_js(&records), at_ms)),
            HookCall::DatagramCounts { tile, frame } => check("datagramCounts", h.datagram_counts(tile, frame)),
            HookCall::RafTick(n) => check("rafTick", h.raf_tick(n)),
            HookCall::BeforePaint { at_ms } => check("beforePaint", h.before_paint(at_ms)),
            HookCall::AfterPaint { drained, raf_ticks, nack_sent } => {
                let info = obj(&[
                    ("solid", drained.solid.into()),
                    ("palrle", drained.palrle.into()),
                    ("cdf53", drained.cdf53.into()),
                    ("h264", drained.h264.into()),
                    ("rafTicks", raf_ticks.into()),
                    ("nackSent", nack_sent.into()),
                ]);
                check("afterPaint", h.after_paint(info));
            }
            HookCall::Swallowed { site, message } => check("swallowed", h.swallowed(&site, &message)),
            HookCall::RendererError { tick, message } => check("rendererError", h.renderer_error(tick, &message)),
            HookCall::ServerStream { samples, frames } => check("serverStream", h.server_stream(samples, frames)),
        }
    }
}
```

  `records_js` must produce the same JSON as the deleted `to_js(&route(...))` in `core.rs`. That used `serde_wasm_bindgen::to_value`, which serialises maps as `Map`. `Routed` has no map fields, so either serializer gives the same objects; check this and say so in the report. If the old one was plain `to_value`, use plain `to_value` here as well.

- [ ] **Step 4: Write `src/app/debug_api.rs`**:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! What `hooks.ts` reads from Rust: the renderer's debug state and readbacks,
//! the core's coverage and counts, the page log. Names match what
//! `diagnostics.ts` (`PixelReader`) and `stats_log.ts` (`StatsRenderer`,
//! `StatsCore`) call. Every method uses `try_borrow`: a call made while the
//! client holds a borrow is a rejected call, never a panic (a panic aborts the
//! module).

use std::cell::{OnceCell, RefCell};
use std::rc::Rc;

use ghostframe_client_core::ClientCore;
use js_sys::Promise;
use wasm_bindgen::prelude::*;

use crate::app::dom::PageLog;
use crate::web::logic::summarize_us;
use crate::web::renderer::WebRenderer;

#[wasm_bindgen]
pub struct DebugApi {
    pub(crate) renderer: Rc<RefCell<WebRenderer>>,
    pub(crate) core: Rc<OnceCell<Rc<RefCell<ClientCore>>>>,
    pub(crate) log: Rc<PageLog>,
    pub(crate) tick_us: Rc<RefCell<Vec<u32>>>,
}

fn busy() -> JsValue {
    JsValue::from_str("ghostframe: renderer or core busy (a hook called back during a step)")
}
```

  The `#[wasm_bindgen] impl DebugApi` has these methods. Each one-liner forwards through `self.renderer.try_borrow()` (or `try_borrow_mut()` for `paintUsStats`), mapping the borrow error to `busy()`:
  - `debugState() -> Result<JsValue, JsValue>`;
  - `debugTile(x: u32, y: u32) -> Result<JsValue, JsValue>`;
  - `debugHandles() -> JsValue` (`UNDEFINED` when busy);
  - `paintUsStats() -> Vec<f64>`;
  - `readPixelRect(x, y, w, h) -> Result<Promise, JsValue>`;
  - `readGradientGolden() -> Result<Promise, JsValue>`;
  - the getters `fbWidth`, `fbHeight` and `submitCount` (`u32`, `0` when busy).

  The rest are not renderer forwards:
  - `tickUsStats() -> Vec<f64>` is `summarize_us(std::mem::take(&mut self.tick_us.borrow_mut()))`.
  - `cdf53Coverage()` and `cdf53IncompleteTiles(limit: usize)` return `Result<JsValue, JsValue>`. Before the core exists (`self.core.get()` is `None`), they return `Ok(JsValue::UNDEFINED)`. After that, they call Task 6's `crate::core::cdf53_coverage_js` and `cdf53_incomplete_js` through `try_borrow`.
  - `log(msg: &str)` is `self.log.line(msg)`.

  Use the JS names given here (`#[wasm_bindgen(js_name = …)]`), because `hooks.ts` and the unchanged `diagnostics.ts` and `stats_log.ts` call them by those names.

- [ ] **Step 5: Register the modules.** Add `#[cfg(target_arch = "wasm32")] pub mod hooks;` and `#[cfg(target_arch = "wasm32")] pub mod debug_api;` to `src/app/mod.rs`.

- [ ] **Step 6: Run the host tests, then clippy for both targets.**

```bash
cargo test -p ghostframe-client-wasm --lib 2>&1 | grep 'test result'; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```
Expected: rc=0 for each.

- [ ] **Step 7: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/hooks.rs ghostframe-client-wasm/src/app/debug_api.rs ghostframe-client-wasm/src/web/renderer.rs ghostframe-client-wasm/src/web/logic.rs
git commit -m "feat(web): hooks interface and DebugApi for the TS e2e bookkeeping"
```

---

### Task 13: The client: `App`, the tick, the flush, the reader loop, input, `startClient` [sonnet]

**Files:**
- Create `ghostframe-client-wasm/src/app/client.rs`, `src/app/input.rs` (both wasm32) and `tests/app_client.rs`.
- Modify `src/app/mod.rs`.

This task ports `main()` itself. Read `ghostframe-web-client/src/main.ts` on master whole before starting. Every step it took, in its order, has a place below. Its comments are the specification; move the ones that still apply next to the code that now does the work.

- [ ] **Step 1: Write `src/app/input.rs`** (port of `attachInputCapture`).

  `pub fn attach_input(canvas: &HtmlCanvasElement, send: Rc<LoggedWriter>, renderer: Rc<RefCell<WebRenderer>>)` does the following:
  - **Focus:** `canvas.set_tab_index(0)`.
  - **Pointer move:** the `pointermove` listener computes `canvas_to_fb(e.client_x(), e.client_y(), rect, dims)`.
    - `rect` comes from `get_bounding_client_rect()`.
    - `dims` is `(fb_width, fb_height)` read through `renderer.try_borrow()`, or `(0, 0)` when busy.
    - It stores the result in an `Rc<Cell<Option<(i16, i16)>>>`. If no frame is queued, it calls `request_animation_frame` with one persistent flush closure, which sends `encode_pointer_move` for the stored value and clears it. This is the 30 Hz coalescing.
  - **Pointer down:** call `canvas.focus()`. If `dom_button_to_x11` maps the button, send `encode_pointer_button(x, y, btn, true)`. Call `prevent_default`, but only when the button is mapped, as TS did.
  - **Pointer up:** the same with `down = false`, and no focus.
  - **Pointer leave:** send `encode_pointer_move(POINTER_LEFT.0, POINTER_LEFT.1)`.
  - **Wheel:** add it with `AddEventListenerOptions` `passive(false)`. Send `encode_wheel(dx, dy)` if `wheel_ticks` returns `Some`. Always call `prevent_default`.
  - **Keys:** `keydown` and `keyup` send `encode_key_down` or `encode_key_up` for `key_to_keysym(&e.key())` when it is `Some`, and then call `prevent_default`.
  - **Closures:** every listener closure lives for the page, so call `Closure::forget` and add a comment saying why.

  The encoders are `ghostframe_client_core::input::*`.

- [ ] **Step 2: Write `src/app/client.rs`.** The required structure follows. Fields and method names are fixed, because the browser test drives them. Bodies follow `main.ts`.

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser client (port of `main.ts`'s `main()`).

/// Everything one session's steps need. Borrowed by exactly one browser task
/// at a time (the rAF tick, the datagram reader, the fallback timer, a decoder
/// callback); hooks are dispatched after the borrow is released.
pub struct App {
    pub(crate) flags: PageFlags,
    pub(crate) renderer: Rc<RefCell<WebRenderer>>,
    pub(crate) core: Rc<RefCell<ClientCore>>,
    pub(crate) log: Rc<PageLog>,
    session: Session,
    /// ACK and NACK datagrams.
    datagrams: LoggedWriter,
    /// The feedback stream: HELLO, receiver feedback, decode errors, resets.
    /// `None` when the bidi stream could not be opened.
    feedback: Option<LoggedWriter>,
    decode_error_feedback: Option<LoggedWriter>,
    batch: DatagramBatch,
    flush_timer: Option<i32>,
    decoder: Option<FrameDecoder<WebCodec>>,
    decode_errors: DecodeErrorBatcher,
    nack_sent: u32,
    raf_ticks: u32,
    in_tick: bool,
    out_of_tick_us: f64,
    tick_us: Rc<RefCell<Vec<u32>>>,
    hooks_enabled: bool,
    calls: Vec<HookCall>,
    me: Weak<RefCell<App>>,
}

/// What `App::new` takes (the browser test builds one with fake streams).
pub struct AppParts {
    pub flags: PageFlags,
    pub renderer: Rc<RefCell<WebRenderer>>,
    pub core: Rc<RefCell<ClientCore>>,
    pub log: Rc<PageLog>,
    pub datagram_writer: WritableStreamDefaultWriter,
    pub feedback_writer: Option<WritableStreamDefaultWriter>,
    pub tick_us: Rc<RefCell<Vec<u32>>>,
    pub hooks_enabled: bool,
}

impl App {
    /// `Rc::new_cyclic`, so decoder callbacks can find the app.
    pub fn new(parts: AppParts) -> Rc<RefCell<App>>;
    /// Drain every pending outbound buffer to its wire (the HELLO first, at
    /// startup, before any input listener exists).
    pub fn drain_transmit(&mut self);
    /// One datagram off the WebTransport reader. `arrival_us` is
    /// `performance.now() * 1000` at the read.
    pub fn on_datagram(&mut self, value: &Uint8Array, arrival_us: f64, hidden: bool);
    /// Hand the batch to the core (the tick, the backlog, the timer, a hidden tab).
    pub fn flush_datagrams(&mut self);
    /// One animation frame.
    pub fn tick(&mut self);
    /// A decoder output (see `WebCodec`).
    pub fn on_decoder_output(&mut self, generation: u32, frame: JsValue);
    pub fn on_decoder_error(&mut self, generation: u32, message: String);
    /// The transport closed: renderer first (it closes the VideoFrames it owns),
    /// then the decoder.
    pub fn on_session_reset(&mut self);
    /// Hook calls queued since the last take; dispatch them after the borrow ends.
    pub fn take_calls(&mut self) -> Vec<HookCall>;
}
```

  Bodies, as `main.ts` did them:
  - **The clock.** `now_us()` is `(performance.now() * 1000.0).round() as u64` and `now_ms()` is `performance.now()`. Put one private helper pair in `client.rs`.
  - **`drain_transmit`.** Loop `self.core.borrow_mut().poll_transmit(now_us())`:
    - `PollOutput::Datagram(b)`: add `nacks_in(&b)` to `nack_sent`, then `self.datagrams.write(&b)`.
    - `PollOutput::Stream(b)`: write to `self.feedback` if it is `Some`.

    The `match` lists both variants and has no wildcard.
  - **`on_datagram`.**
    - Length 0 is ignored.
    - Below `SHORT_DATAGRAM_LEN`, use `session.on_short_datagram(&value.to_vec(), …)`, then apply the actions.
    - Otherwise call `batch.push_with(len, arrival_us, |dst| value.copy_to(dst))`, then act on `flush_decision(batch.len(), hidden, flush_timer.is_some())`:
      - `Now` calls `flush_datagrams()`;
      - `ArmTimer` arms `FLUSH_FALLBACK_MS` with the persistent timer closure (Step 3) and keeps the handle;
      - `Wait` does nothing.
  - **`flush_datagrams`.** In order:
    1. Clear and take the timer.
    2. Return early if the batch is empty.
    3. `let events = { let (c, l, a) = self.batch.parts(); feed_datagrams(&mut self.core.borrow_mut(), c, l, a) }`.
    4. `batch.clear()`.
    5. `let routed = route(events, &mut *self.renderer.borrow_mut())`. The borrow ends before the next line.
    6. `self.handle_routed(routed)`.
    7. Push `HookCall::DatagramCounts` from `core::datagram_counts`.
    8. `drain_transmit()`.
    9. If `!in_tick`, add the elapsed µs to `out_of_tick_us`.
  - **`handle_routed`** (private). For each record:
    - call `session.on_routed(r, &mut actions)`;
    - push the returned record to a `Vec<Routed>` when `hooks_enabled`;
    - run each action: `Status` and `Log` go to `PageLog`, `ConsoleError` to `console::error_1`, `Overlay` to `show_disconnected_overlay`, and `DecodeH264` to `decode_h264` (below).

    At the end, push `HookCall::Events { at_ms: now_ms(), records }` when it is non-empty.
  - **`decode_h264`** (private).
    1. Create the decoder lazily.
       - Its `WebCodec` callbacks capture `self.me.clone()`, upgrade it, take `try_borrow_mut()`, call `on_decoder_output` or `on_decoder_error`, take the calls, drop the borrow, and then `dispatch` them.
       - Dispatch needs the hooks object, so keep it in a `thread_local! { static HOOKS: RefCell<Option<Hooks>> }` set once by `start_client`. This is the one global, so document it.
       - On `try_borrow_mut` failure, close the frame and `console::error`. That cannot happen per the Findings, but must not panic.
       - If `FrameDecoder::new` fails, push `Swallowed { site: "handleEvent:NeedsH264", … }` and leave the decoder `None`.
    2. Call `decoder.decode(&payload, key, seq)`. An `Err` is `Swallowed` with the same site.
    3. Call `apply_decoder_notes`:
       - each note becomes `core.h264_result(seq, outcome, now_us())`, followed by `not_output_warning(core.h264_not_output_stats())`, which goes to `console::warn` if `Some`;
       - each warning goes to `console::warn`.

    No `drain_transmit` here: the frame ack must leave behind the ACK batch, which the tick flushes, as the `main.ts` comment says.
  - **`on_decoder_output`.** Read `frame_seq_of(&frame)`.
    - If the decoder exists and `on_output(generation, seq)` returns `true`, call `self.renderer.borrow_mut().push_h264(frame, seq)`.
    - Otherwise call `close_frame(&frame)`.

    Then run `apply_decoder_notes`.
  - **`on_decoder_error`.** `console::error("Full-frame H264 decode error:", msg)`, then `decoder.on_error(generation)` and `apply_decoder_notes`.
  - **`tick`.** In order:
    1. `raf_ticks += 1` and `t0 = now_ms()`; set `in_tick`.
    2. Push `RafTick`.
    3. `flush_datagrams()`.
    4. `let routed = route(self.core.borrow_mut().on_timeout(now_us()), &mut *self.renderer.borrow_mut())`, then `handle_routed`.
    5. `drain_transmit()`.
    6. Push `BeforePaint { at_ms: now_ms() }`.
    7. `let painted = self.renderer.borrow_mut().paint_tick_js(now_us())`. It keeps its own errors for `take_error_strings`, as `main.ts`'s `renderer.paintTick` did.
    8. Push one `RendererError { tick: paint_tick_count, message }` per `take_error_strings()`.
    9. For each `[codec, x, y, code]` of `take_decode_errors()`, map with `units::codec_from_u8` and `decode_error_code_from_u8`, skipping unknown values. When `DecodeErrorBatcher::report(…, now_us())` returns bytes, write them through `decode_error_feedback`.
    10. If `painted > 0`, call `core.record_paints(painted)`.
    11. Push `AfterPaint { drained: session.take_drained(), raf_ticks, nack_sent }`.
    12. Clear `in_tick`.
    13. If `flags.e2e && painted > 0`, push `round((now_ms() - t0) * 1000.0 + out_of_tick_us)` to `tick_us`.
    14. `out_of_tick_us = 0`.
  - **`on_session_reset`.** `renderer.on_session_reset()`. If there is a decoder, `take` it and `close()` it.

- [ ] **Step 3: Write `start_client` in `client.rs`.** This is `main()`, in its order:

```rust
/// Start the client on this page. `hooks` is `hooks.ts`'s object (PR 4 moves
/// the hooks into Rust and the call into `#[wasm_bindgen(start)]`). Resolves
/// when the datagram stream ends; an error is shown on the page, not thrown.
#[wasm_bindgen(js_name = startClient)]
pub async fn start_client(hooks: JsValue) -> Result<(), JsValue> {
    let hooks: Option<Hooks> = (!hooks.is_undefined() && !hooks.is_null()).then(|| hooks.unchecked_into());
    // ... find window, document, PageLog, #canvas; run `run(…)`; on Err(msg):
    //     log.line(&format!("Error: {msg}")); log.status(&format!("Error: {msg}"));
    Ok(())
}
```

  `run` does what `main.ts` did, in this order:
  1. `flags = page_flags(&window)`.
  2. If `flags.lossless`, call `log.hide()` and `pin_canvas(&canvas)`.
  3. `log.line(&format!("Connecting to {origin}..."))`.
  4. Call `WebRenderer::create_inner(canvas.clone(), flags.cdf53).await`.
     - An `Err` starting with `WEBGPU_UNAVAILABLE` sets the status to `WebGPU not available in this browser.`, logs the message, and returns `Ok(())`.
     - Any other `Err` is returned.
  5. Wrap the renderer in `Rc<RefCell<_>>`. Build the `DebugApi`, with `core` an empty `OnceCell`, `log`, and the shared `tick_us`. Call `hooks::attach`. Store the hooks in the `HOOKS` thread-local.
  6. `let transport = bootstrap().await?`.
  7. Spawn the `closed` handler (`spawn_local`).
     - On resolve, with info `{ closeCode, reason }`:
       - log `Transport closed: code={closeCode} reason={reason}`, printing JS `undefined` for missing values as TS did;
       - call `app.on_session_reset()`;
       - if `close_overlay_message(Some(closeCode))` is `Some(msg)`, show the overlay.
     - On reject: log `Transport closed with error: {e}`, then `on_session_reset()`.

     The handler waits for the `App` to exist: create it before this future can resolve, or hand the future an `Rc<OnceCell<Rc<RefCell<App>>>>`. Either way, it never borrows a missing app.
  8. Await `transport.ready()`. Log `Connected!`; set the status to `Connected`.
  9. Open the feedback stream: `JsFuture::from(transport.create_bidirectional_stream()).await`. An `Err` gives `console::warn("Could not open feedback stream")` and `None`. Take the writer from `StreamPair::writable().get_writer()`.
  10. Take the datagram writer from `transport.datagrams().writable().get_writer()`.
  11. Create the core: `core = Rc::new(RefCell::new(new_client_core(client_config(true, renderer.h264_supported(), true), now_us())))`, then `api_core.set(core.clone())`.
  12. Build the `App`. The writer labels are:
      - datagrams: `"ACK/NACK datagram write failed"`;
      - feedback: `"feedback stream write failed"`;
      - `decode_error_feedback`: `feedback.labelled("decode-error feedback write failed")`.
  13. Call `app.borrow_mut().drain_transmit()`. This is the HELLO, before any input listener, as the `main.ts` comment requires.
  14. Unless `flags.lossless`, mount the stats panel with `ghostframe_web_ui::mount_stats_panel(host, Rc::new(CoreBackend::new(core.clone())))`. `host` is `#stats-panel`. If the host is missing or the mount fails, `console::warn("stats panel failed to mount; continuing without it", e)`.
  15. If the bidi stream exists, `spawn_local(pump_stream(readable, …))`:
      - the chunk handler calls `core.borrow_mut().handle_server_stream(bytes, now_us())`;
      - under `flags.e2e`, it then dispatches `ServerStream { samples, frames }`, read from `core.stats()` as `core.rs`'s `statsSamplesAccepted` and `statsFramesReceived` do.
  16. If the feedback writer exists, `attach_input(&canvas, Rc::new(feedback.labelled("input send failed")), renderer.clone())`.
  17. Start the rAF loop with the standard self-rescheduling `Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>`. Each frame:
      - `let calls = { let mut a = app.borrow_mut(); a.tick(); a.take_calls() }`;
      - `dispatch`;
      - re-arm.
  18. Create the fallback timer closure once, as a persistent `Closure<dyn FnMut()>` in the `App`: `{ let calls = { let mut a = app.borrow_mut(); a.flush_datagrams(); a.take_calls() }; dispatch(...) }`.
  19. Run the datagram reader loop.
      - Read `transport.datagrams().readable()`.
      - Each `read` is awaited.
      - `done` ends the loop. An `Err` returns it, and the page shows `Error: …` as `main.ts` did.
      - For each value: `let calls = { let mut a = app.borrow_mut(); a.on_datagram(&value, now_ms() * 1000.0, document.hidden()); a.take_calls() }`, then `dispatch`.

  Every `app.borrow_mut()` above is held only for the synchronous step, never across an `.await`. In the task report, show where each one ends.

- [ ] **Step 4: Register the modules.** Add `#[cfg(target_arch = "wasm32")] pub mod client;` and `#[cfg(target_arch = "wasm32")] pub mod input;` to `src/app/mod.rs`.

- [ ] **Step 5: Write `tests/app_client.rs`, the feedback-bytes test.** One browser test, `a_tile_is_acknowledged_and_the_hooks_see_the_tick_in_order`, does the following:
  - Create a canvas and a `WebRenderer` (`create_inner` is `pub(crate)`). Use the public path `WebRenderer::create(canvas, "auto".into())`, which the existing browser tests use.
  - Build `#status` and `#log` divs and a `PageLog`.
  - Make two recording `WritableStream`s, one for datagrams and one for feedback, built with a `js_sys::Function` helper that pushes `Array.from(chunk)` into `globalThis.__dg` and `globalThis.__fb`. Take their writers.
  - Build the core with `new_client_core(client_config(true, false, true), now_us())`.
  - Build `App::new(AppParts { hooks_enabled: true, flags: PageFlags::from_params(&["1".into()], None), … })`.
  - Call `drain_transmit()`. After one macrotask, `__fb[0][0]` must be `0x03` (the HELLO message type, `ghostframe_protocol::feedback_stream::HELLO_MSG_TYPE`).
  - Feed `build_frame_dimensions_datagram(1, 0, 64, 64)` and one solid tile. The tile is `fragment_tile` with `frame_seq: 2 | TILE_DATAGRAM_FLAG`, at (0, 0), `Codec::Solid`, and payload `[0, 0, 255, 255]`. Its `wire_seq` is stamped as big-endian `4242` at bytes 8..12. Feed both through `on_datagram(…, now_ms() * 1000.0, false)`.
  - Call `tick()` and take the calls.
    - The `HookCall` kinds must start `RafTick`, `Events`, `DatagramCounts`, contain a `BeforePaint`, and end with `AfterPaint`. The first `Events` must hold a `FrameDimensions { applied: true, .. }` and a `TilePayload`.
    - The `AfterPaint`'s `drained.solid` must be 1.
    - The `#status` text must be `Receiving frames`.
  - Wait 30 ms with a `setTimeout` promise. Call `tick()` again and wait one macrotask.
    - Decode every `__dg` entry that starts with `ACK_BATCH_MSG_TYPE` with `AckBatch::decode`. Together they must name `wire_seq` 4242. Read `AckBatch`'s accessors from `ghostframe-protocol/src/ack.rs`; do not decode the bytes by hand.
  - The test must take the `App` borrow exactly as `start_client` does: `{ let mut a = app.borrow_mut(); a.tick(); a.take_calls() }`.

  Make `HookCall` derive `Debug` so the test can print the sequence on failure. Compare kinds with `matches!`, not by string.

- [ ] **Step 6: Run clippy on both targets and the host tests,** as in Task 12, Step 6. Expected: rc=0 for each.

- [ ] **Step 7: Show the test is load-bearing.** This can only run in a browser, so it waits for CI or G3. Write down two mutations for Task 15 to run:
  - delete the `drain_transmit()` call at the end of `tick`;
  - make `drain_transmit` skip `PollOutput::Datagram`.

  `a_tile_is_acknowledged…` must fail on each.

- [ ] **Step 8: Commit.**

```bash
git add ghostframe-client-wasm/src/app/mod.rs ghostframe-client-wasm/src/app/client.rs ghostframe-client-wasm/src/app/input.rs ghostframe-client-wasm/tests/app_client.rs
git commit -m "feat(web): the client main loop in Rust: startup, WebTransport, rAF tick, input"
```

---

### Task 14: TypeScript side: `hooks.ts`, a small `main.ts`, deletions [sonnet]

**Files:**
- Create `ghostframe-web-client/src/hooks.ts`.
- Rewrite `src/main.ts`.
- Delete `src/decoder.ts`, `src/bootstrap.ts`, `src/server_stream.ts`, `src/close_message.ts`, `src/disconnect_overlay.ts`, `src/input/wire.ts`.
- Delete `tests/bootstrap.test.ts`, `tests/close_message.test.ts`, `tests/h264_frame_seq.test.ts`, `tests/h264_result.test.ts`, `tests/input.test.ts`, `tests/server_stream.test.ts`.
- Delete `ghostframe-client-wasm/src/input.rs` and its `pub mod input;` in `lib.rs`. After this task nothing calls its exports; check with grep in Step 4.

- [ ] **Step 1: Write `src/hooks.ts`.** Move the bookkeeping of `main.ts` into it:
  - from `handleEvent`, the `recordProtocolEvent`, `health`, `diag.record*` and `recordPaletteWrite` calls;
  - the `firstRecvMs` / `noteFrameTileReceived` / `pendingFramePaintRaf` block;
  - the `__ghostframe*` installs.

  Keep each verbatim, comments included. The shape is:

```ts
// SPDX-License-Identifier: MIT OR Apache-2.0
// The page's e2e bookkeeping, called by the Rust client
// (ghostframe-client-wasm/src/app/hooks.rs) in a fixed order: per tick
// rafTick, events+datagramCounts (flush), events (timeout), beforePaint,
// afterPaint; per flush outside a tick events+datagramCounts. PR 4 ports this
// file to Rust and deletes it.
import { initDiagnostics, recordPaletteWrite, type DiagnosticsHandle } from './diagnostics.js';
import { recordProtocolEvent, codecDiscriminant, type Cdf53ErrorCodes } from './cdf53_globals.js';
import { createStatsLogger } from './stats_log.js';
import { health } from './health.js';
import { errorCodes, type DebugApi } from '../pkg-web/ghostframe_client_wasm.js';

export function createHooks(diagnostics: boolean) {
  // ... state: api, diag, statsLog, nackSent, fbW, fbH, firstRecvMs, latestTileFrameSeq, pendingFramePaintRaf
  return {
    attach(api: DebugApi): void { /* the window.__* installs main.ts did after WebRenderer.create, initDiagnostics({ reader: api, readbacks: diagnostics }), __cdf53CoverageSummary, createStatsLogger({ renderer: api, core: api, log: (m) => api.log(m), getNackSent }) */ },
    events(records: HookRecord[], atMs: number): void { /* per record: recordProtocolEvent(window, r, codes, atMs); then the TilePayload / PaletteUpdated / FrameDimensions branches of handleEvent's bookkeeping, using atMs where main.ts called performance.now() */ },
    datagramCounts(tile: number, frame: number): void { /* diag.stats.tileDatagrams = tile; diag.stats.frameDatagrams = frame */ },
    rafTick(n: number): void { /* diag.recordRafTick(n) */ },
    beforePaint(atMs: number): void { /* the pendingFramePaintRaf flush, rafMsClient = atMs */ },
    afterPaint(info: { solid: number; palrle: number; cdf53: number; h264: number; rafTicks: number; nackSent: number }): void { /* nackSent = info.nackSent; statsLog.onTick({...}, info.rafTicks) */ },
    swallowed(site: string, message: string): void { health.record(site, message); },
    rendererError(tick: number, message: string): void { health.record('renderer', `tick ${tick}: ${message}`); health.tally(`gpu:${message.slice(0, 80)}`); },
    serverStream(samples: number, frames: number): void { /* if (diagnostics) window.__ghostframeStatsSamples/Frames = ... */ },
  };
}
```

  Port each piece with these details:
  - **`attach`** installs what `main.ts` installed after `WebRenderer.create`:
    - `__ghostframeHealth`, `__ghostframeRendererState` and `__ghostframeTile`;
    - under `diagnostics`, `__ghostframePaintUs` and `__ghostframeTickUs` (now `api.tickUsStats()`), and zeroed `__ghostframeStatsSamples`/`Frames`;
    - `initDiagnostics({ reader: api, readbacks: diagnostics })`;
    - `__cdf53CoverageSummary`;
    - the stats logger.
  - **`events`.** Copy these branches of `handleEvent`:
    - `TilePayload`:
      - `health.record('renderer.apply', …)` on `error`;
      - `solid-red` marks and, under `diagnostics`, the solid tallies;
      - `noteFrameTileReceived(ev.frame_seq, atMs)` and `diag.recordTile`, with `lastPaintMsClient: atMs`.
    - `PaletteUpdated`: `health.record('renderer.palette', …)` on `error`, and `recordPaletteWrite`.
    - `FrameDimensions` when `applied`: `diag.recordResize`, the `fbW`/`fbH` update, and `window.__ghostframeRenderer = api.debugHandles()`.
    - `DecodeError`: only the `recordProtocolEvent` call at the top.
  - **Types.** `HookRecord` is `main.ts`'s `RoutedEvent` restricted to those four kinds.
  - **Unchanged files.** `diagnostics.ts`, `cdf53_globals.ts`, `health.ts`, `stats_log.ts`, `protocol_constants.ts` and `globals.d.ts` stay as they are, except `globals.d.ts` comments that name `main.ts` as the installer: point them at `hooks.ts`.

- [ ] **Step 2: Rewrite `src/main.ts`**:

```ts
// SPDX-License-Identifier: MIT OR Apache-2.0
// Page entry: starts the Rust client (ghostframe-client-wasm/src/app) with the
// e2e bookkeeping hooks (hooks.ts). PR 4 moves the hooks into Rust; PR 5
// deletes this file with the npm build.
import init, { startClient } from '../pkg-web/ghostframe_client_wasm.js';
import * as wasmExports from '../pkg-web/ghostframe_client_wasm.js';
import { initProtocol } from './protocol_constants.js';
import { installConsoleRing } from './health.js';
import { createHooks } from './hooks.js';

async function main() {
  // Diagnostics that cost per-call work (console mirroring, per-tile tallies)
  // run only for the e2e suite (`?e2e` on the page URL).
  const diagnostics = new URL(window.location.href).searchParams.has('e2e');
  if (diagnostics) installConsoleRing();
  // wasm-bindgen `--target web`: initialise before any export is called.
  await init();
  // The hooks' recorders read wire constants from Rust; TS must not retype them.
  initProtocol(wasmExports);
  // Errors after this point are shown on the page by the client itself.
  await startClient(createHooks(diagnostics));
}

main().catch((e) => {
  const msg = e instanceof Error ? e.message : String(e);
  const status = document.getElementById('status');
  if (status) status.textContent = `Error: ${msg}`;
  console.error('ghostframe client failed to start', e);
});
```

- [ ] **Step 3: Delete the files listed under Files.** Use `git rm`.

- [ ] **Step 4: Check nothing still uses what was deleted.**

```bash
cd ~/work/gf-web-mainloop
grep -rn "decoder\.js\|bootstrap\.js\|server_stream\|close_message\|disconnect_overlay\|input/wire\|FullFrameDecoder\|attachInputCapture\|encodePointerMove\|keyToKeysym\|handleDatagramsInto\|onTimeoutInto\|WasmDecodeErrorBatcher" ghostframe-web-client/src ghostframe-web-client/tests ghostframe-client-wasm/src; echo "rc=$?"
```
Expected: rc=1 (no matches).
- If `WasmDecodeErrorBatcher` is still exported, check whether any test uses it. If none does, delete it from `units.rs`.
- If a vitest suite still imports `encodePointerMove`/`keyToKeysym`, that suite was missed in Step 3; delete it if it is one of the ported ones, or report it.

Then look for live references in docs and code comments:
```bash
grep -rn "main\.ts\|decoder\.ts\|wire\.ts\|bootstrap\.ts\|server_stream\.ts" --include=*.rs --include=*.ts --include=*.md . | grep -v '^./docs/superpowers\|^./docs/specs\|^./docs/reviews\|^./target'
```
In each hit that describes the current code, point the comment at the Rust module that does the work now. Examples are `core.rs` ("Called by `main.ts`"), `renderer.rs` ("so main.ts can tell the user"), DEVELOPERS.md and AGENTS.md. Leave history in `docs/specs`, `docs/superpowers` and `docs/reviews` alone.

- [ ] **Step 5: Build the web client and typecheck it.**

```bash
export TMPDIR=~/work/gf-tmp-webr
just build-web > ~/work/gf-wml-build1.log 2>&1; echo "rc=$?"
cd ghostframe-web-client && npx tsc --noEmit && npx tsc --noEmit -p tsconfig.test.json; echo "rc=$?"
npm test 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```
Expected: rc=0 for each. The vitest suites left are `cdf53_globals`, `diagnostics`, `health`, `protocol_constants`, `protocol_literals`, `sanity` and `wasm_smoke`, and all pass. If `protocol_literals.test.ts` lists the deleted files by name, remove those entries.

- [ ] **Step 6: Commit.**

```bash
git status --short
git add ghostframe-web-client/src ghostframe-web-client/tests ghostframe-client-wasm/src/lib.rs ghostframe-client-wasm/src/units.rs
git add -u ghostframe-client-wasm/src/input.rs   # the deletion
# plus each comment/doc file Step 4 changed, by name
git commit -m "refactor(web): main.ts starts the Rust client; delete the TS main loop, decoder, bootstrap, input and overlays"
```

---

### Task 15: Gates, and the PR [sonnet]

**Files:** none new, except the PR body (not committed).

- [ ] **Step 1: The local gate.**

```bash
cd ~/work/gf-web-mainloop && export TMPDIR=~/work/gf-tmp-webr
~/work/gf-gpu-run just ci-local > ~/work/gf-wml-ci.log 2>&1; echo "rc=$?"
```
`ci-local` runs `just test-web-renderer`, which is a browser test, so **G3** applies: ask the controller first. If the go-ahead is not given, run what `ci-local` runs minus `test-web-renderer`, recipe by recipe from the `Justfile` (`grep -n '^ci-local' -A40 Justfile`), and say in the report that the browser tests are left to CI.

Expected: rc=0. On failure, quote the failing section of `~/work/gf-wml-ci.log`.

- [ ] **Step 2: Wasm size (G1).**

```bash
ls -l ghostframe-web-client/dist/assets/*.wasm && gzip -9c ghostframe-web-client/dist/assets/*.wasm | wc -c
ci/check-wasm-size.sh; echo "rc=$?"
```
Report raw and gzip sizes against Task 0's numbers. If rc≠0, stop at **G1**.

- [ ] **Step 3: Push and open the PR; CI is the browser gate.**

```bash
git push -u origin feat/web-main-loop   # SSH_AUTH_SOCK=/run/user/1000/ssh-agent.socket, outside the sandbox
gh pr create --base master --head feat/web-main-loop --title "web: main loop, WebTransport, WebCodecs, input and overlays in Rust (all-Rust web client, PR 3)" --body-file ~/work/gf-wml-prbody.md
```

The PR body covers:
- what moved where (the File structure table);
- the hooks bridge and why it exists until PR 4;
- every deleted TS file and vitest suite and where each test went (the spec §8 rows for PR 3);
- the hidden-tab fallback being host-tested only, since no e2e exists;
- the wasm sizes;
- the behaviour notes:
  - stream writes are no longer awaited;
  - input write failures are logged once;
  - tick timing now excludes hook work, so the bench's tick numbers may drop;
- what is left for PR 4 and PR 5.

End the body with the attribution line. Then link the PR, and watch it.

- [ ] **Step 4: Read CI.** Every job must pass. Watch these in particular:
  - `client-core` / `web-renderer`: the browser tests `app_io`, `app_decoder` and `app_client` run there;
  - `web-client`, for the wasm size and vitest;
  - `e2e`.

  If `the_real_decoder_decodes_the_probe_clip` fails only on CI, stop at **G2** with the job log lines. On any other failure, debug it (superpowers:systematic-debugging). Do not skip it.

- [ ] **Step 5: Mutations in the browser.** With CI green, or once G3 allows a local browser run, apply Task 13 Step 7's two mutations one at a time on a scratch commit. Push each (or run `wasm-pack test --headless --chrome ghostframe-client-wasm -- --test app_client` locally under G3). Show that `a_tile_is_acknowledged…` fails on each. Then revert and force-push back to the reviewed head; the owner OK'd force-pushes for a branch under review.

  Report the failing assertions.

- [ ] **Step 6: The full local e2e (G3).** Only with the controller's go-ahead:

```bash
just containers-build > ~/work/gf-wml-cb.log 2>&1; echo "rc=$?"
flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run just test-e2e > ~/work/gf-wml-e2e.log 2>&1; echo "rc=$?"
```
`just test-e2e` covers the VKMS set and the H.264 local-only set, among them `e2e_h264_forced_solid_red`, `e2e_h264_motion`, `e2e_mode_switch`, `e2e_input_forwarding_chromium`, `e2e_lossless_golden_png` and `e2e_polled_globals_are_live`. Expected: rc=0. A failure is compared against master on the same machine before it is called a regression; known flakes are in AGENTS.md. **Capture the failing test name before any rerun.**

---

### Task 16: Final review [opus]

**Files:** none changed by the reviewer.

Review the whole branch, `6e310e42..HEAD`, against this plan and spec section 5.4. Check in particular:

- **Behaviour parity with `main.ts`.** Go step by step through master's `main.ts`. Every behaviour it had must have a home now: in Rust, or in `hooks.ts`. That includes:
  - the status texts and log lines;
  - every `window.__*` it installed, under the same gate;
  - the order of HELLO, panel, server stream, input and rAF;
  - the hidden-tab flush;
  - the ping/pong path;
  - the overlays.
- **No `RefCell` borrow is held across an `.await` or a hook dispatch.** Check every `borrow_mut()` in `app/client.rs`.
- **Every test that moved can fail.** Check the mutation steps the implementers reported.
- **No `_ =>` wildcard was added to non-test code** (the crate's `wildcard_enum_match_arm` rule).
- **The SPDX line is on every new file, and no `/home/<user>` path appears anywhere.**
- **The wasm32 clippy job and the host tests pass.** Run them:
  - `cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings`;
  - `cargo test -p ghostframe-client-wasm --lib`;
  - `cargo clippy --workspace --all-targets -- -D warnings`.

  The browser tests run in CI (gate G3).

## Self-review against the spec

- **§5.4 startup:**
  - flags: Task 1 and Task 11;
  - lossless chrome: Tasks 11 and 13;
  - `WebRenderer::create`: Task 13;
  - bootstrap: Tasks 2 and 9;
  - stats panel mount as a direct Rust call: Task 13.
- **§5.4 WebTransport:**
  - one task reads datagrams into the core, one pumps the feedback stream: Tasks 8 and 13;
  - the ACK and NACK writers behave unchanged: `drain_transmit`, with the ordering finding;
  - one copy into wasm memory, no JS arrays: `DatagramBatch::push_with`, Task 3.
- **§5.4 rAF:** a `Closure` over `Rc<RefCell<…>>`, with the hidden-tab timer fallback kept: Tasks 3 and 13.
- **§5.4 WebCodecs:**
  - `VideoDecoder` driven from Rust, `output` to the escape with its `frame_seq`: Tasks 5, 10 and 13;
  - an error maps to 0x09 through the core's gate: `H264Outcome::DecodeError`, Task 5;
  - the `frame_seq` tagging and its test: Task 5 tests 1-5, Task 10.
- **§5.4 input:** Rust listeners: Task 13; mapping and keysym tests: Task 4; `INPUT_MSG_TYPE` comes in through the core's encoders.
- **§5.4 overlays:** plain web-sys DOM: Task 11.
- **§5.4 "what remains of main.ts":** deviates, on purpose. `main.ts` also passes the hooks until PR 4 (Findings).
- **§8, PR 3 rows:**
  - `bootstrap`: Task 9 (browser) and Task 2 (host);
  - `close_message`: Task 1;
  - `datagram_batch`, `eviction`, `h264_ack`, `h264_result`, `reset_request`, `tile_assembly`: Task 6, plus Task 5 for `h264_result`'s decoder half;
  - `h264_frame_seq`: Task 5 (host) and Task 10 (browser);
  - `input`: Task 4;
  - `server_stream`: Task 8;
  - `frame_limits` already moved in PR 2.
- **§9:**
  - hidden tab: Task 3 host tests; there is no e2e to keep, as stated;
  - feedback-path blindness: Task 13's `app_client` test checks the HELLO and the ACK bytes.
- **§6 PR 3 gate:** full e2e (Task 15 Step 6, G3) and the moved tests (Tasks 1-10, 13).
