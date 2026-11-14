# All-Rust Web Client PR 2: The Web Renderer Swap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Each task header carries a model tag: **[haiku]** is mechanical (file moves, deletions, wiring, copy-paste with a given body), **[sonnet]** needs judgement (new Rust, API drift against wgpu 30, TS rewiring). Opus reviews every task, per the owner's model split.

**Goal:** The browser draws tiles with `ghostframe-render`'s `TileRenderer` (the native client's renderer) on wgpu's WebGPU backend, owned by a new `WebRenderer` in `ghostframe-client-wasm`. H.264 pictures reach the framebuffer through one raw-WebGPU escape (`web/h264.rs`) on wgpu's own device. The TypeScript renderer (`webgpu/*.ts`), the web-only multi-pass CDF 5/3 shaders and the renderer-reading debug code are deleted. The Chromium e2e suite passes with the e2e Rust code unchanged except for three owner-approved changes: a URL-suffix hook in the harness (Task 14 Step 1), the deletion of seven tests of the old shaders' internals (Task 14 Step 2) and one `#[ignore]` bench test (Task 18).

**Architecture:** `WasmClientCore` hands its tile events to `WebRenderer` inside wasm (`core.handleDatagramsInto(renderer, ...)` and `core.onTimeoutInto(renderer, ...)`), so tile bytes never cross into JS; TS gets back a slim bookkeeping event per tile (codec, coordinates, full length and the first 16 bytes) for the e2e recorders. `WebRenderer.paintTick(now)` runs `TileRenderer::flush`, then the pending H.264 pictures (raw WebGPU, ordered by the paint ledger), then the present blit, and skips all three when nothing changed. PalRLE shader errors are read back by a blocking wait on native, which a wasm32 main thread cannot do, so Task 3 gives them a non-blocking form in `ghostframe-render`; the CDF 5/3 start-up self-test is already awaitable in PR 1 (`TileRenderer::try_enable_cdf53_gpu`).

**Tech Stack:** Rust 1.96.1 (pinned), `wgpu =30.0.1` with `webgpu` + `wgsl` on wasm32 only, `wasm-bindgen` 0.2.129 (locked) and `wasm-bindgen-futures` 0.4.79, `js-sys`, `web-sys` 0.3.106 (stable features only), `wasm-bindgen-test =0.3.79` driven by `wasm-pack` 0.15.0 in headless Chrome, TypeScript and vitest for what is left of `ghostframe-web-client`.

**Branch:** `feat/web-renderer-wgpu`, in a worktree off master **after PR 1 (`ghostframe-render`) has merged**. **Spec:** `docs/specs/all-rust-web-client-design.md` sections 5.2, 5.3, the renderer hooks of 5.5, 7 and the PR 2 rows of 6 and 8.

**Exit status.** A pipe reports the last command's status, and a masked build failure has shipped a stale `dist/` here before. Every gating command in this plan that is piped into `tail`/`grep` is followed by `echo "rc=${PIPESTATUS[0]}"` and its step names the rc it expects; a step that does not mention rc expects `0`. Unpiped commands use `echo "rc=$?"`.

**Paths.** The repository root is the worktree root. `~/work/gf-gpu-run` is the GPU guard; commands that touch the GPU (native GPU tests, `just test-e2e`, bench) run as `~/work/gf-gpu-run <command>`, and Docker ones take `flock -o ~/work/.gf-heavy.lock` outside it (see the wrapper's header). `just ci-local` is always `~/work/gf-gpu-run just ci-local`.

## Contract assumed from PR 1 (reconciler: check every line)

PR 1 creates crate `ghostframe-render` (MIT OR Apache-2.0, builds for native and `wasm32-unknown-unknown`). This plan uses exactly the following. A line marked **(extra)** is NOT in the agreed contract: it is something PR 2 needs, and either PR 1 already has it (then the matching PR 2 task shrinks to nothing) or a PR 2 task adds it.

| # | Item | Used by |
|---|---|---|
| C1 | `pub struct TileRenderer`; `TileRenderer::new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, width: u32, height: u32) -> Result<Self, GpuError>`. **`format` must be `Rgba8Unorm`** (`GpuError::UnsupportedFormat` otherwise): the decode shaders write `rgba8unorm` storage textures. So `WebRenderer` passes `Rgba8Unorm` and present-blits to the canvas format with `present_blit.wgsl` (confirmed: that is what Tasks 6 and 9 do). | Task 9 |
| C2 | **Device and queue are passed on every call, never stored**: `apply_event(&mut self, device: &wgpu::Device, queue: &wgpu::Queue, ev: ghostframe_client_core::Event) -> Result<(), GpuError>` for `TilePayload` (Solid, PalRle, Cdf53), `PaletteUpdated` and `FrameDimensions` (flushes what is pending, then resizes; keeps what fits; resets the ledger). `NeedsH264` is not handled. `WebRenderer` keeps its own clones and passes them. | Task 9 |
| C3 | `note_h264(&mut self, seq: u32)` records the picture in the ledger and cancels pending CDF 5/3 draws it covers, **but does not mark damage** (`mark_all_dirty()` does); `h264_stale(&self, seq: u32) -> bool`; `hold_cdf53(bool)` holds CDF 5/3 tiles pending for a flush; `WebRenderer::paint_tick` uses it while a non-stale picture is about to be drawn (Task 9). | Tasks 5, 9 |
| C4 | `flush(&mut self, device, queue, now_us: u64) -> Result<FlushReport, GpuError>`; `now_us` is only recorded (`FlushReport::at_us`, `DebugState::last_flush_us`). `FlushReport { recorded: bool, submits: u32, damage: Vec<coalesce::Rect>, deferred_work: bool, at_us: u64 }`; `damage` is the pixel rectangles dirtied since the last flush, empty when nothing but a picture changed. `has_deferred_work(&self) -> bool`. | Task 9 |
| C5 | `framebuffer(&self) -> &wgpu::Texture` (usage includes `STORAGE_BINDING`, `TEXTURE_BINDING`, `RENDER_ATTACHMENT`, `COPY_SRC`, `COPY_DST`); `fb(&self) -> &Framebuffer`. | Tasks 8, 9 |
| C6 | `debug_tile(&self, x: u32, y: u32) -> Option<DebugTile>` (`None` off the grid) and `debug_state(&self) -> DebugState`; both `serde::Serialize`, `camelCase`. `DebugTile { tileX, tileY, newestFrameSeq, cdf53 }`; `DebugState { width, height, cols, rows, flushes, lastFlushUs, pendingSolid, pendingPalrle, pendingRgba, cdf53Gpu, cdf53Pending, newestTileSeq, newestH264Seq }`. The old renderer's JSON differed; Task 9 maps it (see "JSON shapes" below). | Task 9 |
| C7 | CDF 5/3: **`pub async fn try_enable_cdf53_gpu(&mut self, device: &wgpu::Device, queue: &wgpu::Queue) -> Result<(), String>`** runs the self-test and, on success, enables the grid with the default cost model and the default per-flush tile cap; on `Err` nothing is enabled and passes decode on the CPU (`Cdf53TileState`). `cdf53_gpu_enabled(&self) -> bool`. `probe`/`Probed` are native-only and stay in `ghostframe-client-gpu`; the portable `cdf53_grid::{self_test, probe_grid, ErrorScopes}` and `enable_cdf53_gpu_with(device, grid, us_per_tile_k14: Option<f64>, max_tiles)` exist if the default cost model needs tuning (Task 18). | Task 9 |
| C8 | `reset_session(&mut self, device, queue) -> Result<(), GpuError>`: no session-reset *event*; clears pending draws, the ledger (H.264 floor included), the CDF 5/3 state on both sides and **zeroes the PalRLE palette atlas**; marks everything dirty. | Task 9 |
| C9 | **Native-only in PR 1** (`cfg(not(target_arch = "wasm32"))`): `set_collect_decode_errors(bool)`, `take_decode_errors() -> Vec<GpuDecodeError>`, `GpuDecodeError { codec: Codec, tile_x: u8, tile_y: u8, code: DecodeErrorCode }`, and `PalRlePipeline::take_errors`, because the readback blocks on the device. **Task 3 un-gates the first three on wasm and adds the non-blocking readback behind them.** | Task 3, 9 |
| C10 | `GpuError: std::fmt::Display` and has a `Device(String)` variant. `readback::read_texture_rgba8(device, queue, &texture, w, h)` (async, whole texture), `readback::block_on_device` (native driver) and `Framebuffer::read_rgba` exist; they read the whole texture and submit when first polled, so Task 2 adds a rectangle read that submits at once. | Task 2 |
| C11 | Layout: `ghostframe-render/src/{framebuffer,paint_order,dirty,coalesce,submit,readback,tile_renderer}.rs`, `src/pipelines/{solid,palrle,rgba,cdf53_grid,cdf53_state}.rs`. **`pipelines/cdf53_passes.rs` and its test stay in `ghostframe-client-gpu`.** | Tasks 3, 5 |
| C12 | wasm-safety: PR 1's `cargo check --target wasm32-unknown-unknown` and `ci/check-render-portable.sh` hold it (no `Instant`, no blocking wait, no `pollster` reachable on wasm32). The one blocking path left is native-gated `PalRlePipeline::take_errors`; Task 3 adds the wasm replacement. | Task 3 |

**JSON shapes the e2e suite reads, and where PR 1's structs differ.** `window.__ghostframeRenderer.texture.{width,height}` (asserted; raw `GPUTexture` from `as_webgpu`, unaffected). `__ghostframeRendererState()` and `__ghostframeTile(x, y)` are only dumped to failure artifacts (`failure_dump.rs` writes them into `*_browser.json` and `*_tiles.json`; nothing parses a field, checked with grep), so PR 1's shapes are accepted and extended, not mimicked: `debugState()` keeps the old keys that still mean something (`submitCount`, `h264Queue`, `framebufferDirty`, `fbWidth`, `fbHeight`, `hasTexture`, `canvasWidth`, `canvasHeight`, `paintTick`, `errorsReadbackInFlight`, `solidQueue` = `pendingSolid`, `palRleQueue` = `pendingPalrle`) and splices PR 1's `DebugState` in; the old `cdf53Queue` has no equivalent (`cdf53Pending` is a bool). `debugTile()` is PR 1's `DebugTile` (`newestFrameSeq` replaces `frameSeq`; the old `tick` and `codec` have no counterpart) or `null` off the grid.

## Findings that shaped this plan (verified 2026-11-14 on master bcc59e03)

- **`--cfg=web_sys_unstable_apis` is NOT used anywhere in this repository** (`grep -rn unstable_apis` over the tree, `.github/`, the Justfile, Dockerfile and `package.json` finds nothing; there is no `.cargo/config.toml`). The brief assumed it was. This plan does not introduce it: web-sys 0.3.106 gates `GpuExternalTexture`, `GpuDevice::import_external_texture` and `VideoFrame` behind it, and turning it on is a build-wide RUSTFLAGS change that would touch `wasm-pack` in npm, CI, the Dockerfile and every contributor's shell. Instead `web/raw.rs` declares the dozen JS methods the H.264 escape needs with `#[wasm_bindgen] extern "C"` blocks (the same mechanism web-sys uses), on objects obtained from wgpu's `as_webgpu()` handles. If the owner prefers web-sys unstable types, only `web/raw.rs` changes (owner decision 4).
- **wgpu 30.0.1 with `default-features = false, features = ["wgsl", "webgpu"]` compiles for `wasm32-unknown-unknown` without the unstable cfg and without naga.** Checked in a scratch crate: `cargo tree --target wasm32-unknown-unknown -i naga` prints `warning: nothing to print`, and `Device::as_webgpu`, `Queue::as_webgpu`, `TextureView::as_webgpu`, `Instance::new(InstanceDescriptor::new_without_display_handle())` and `request_adapter`/`request_device` type-check. The `webgpu` feature's `naga?/wgsl-out` is a weak dependency; naga would arrive only through `naga-ir`. (In the workspace, `ghostframe-client-gpu` has a `naga` **dev**-dependency; it is host-only and never in the wasm32 graph. Task 1 re-verifies in the real workspace.)
- **wgpu's webgpu types are its own vendored bindings** (`wgpu::webgpu::{GpuDevice, GpuQueue, GpuTexture, GpuTextureView, GpuBuffer}`), not `web_sys` types. They convert to `&JsValue` through `AsRef<JsValue>`, which is how `web/raw.rs` gets at the underlying JS objects.
- **One native path still blocks, and a wasm32 main thread cannot block**: `PalRlePipeline::take_errors` (a `device.poll(wait_indefinitely)` and an `mpsc::recv`), which PR 1 left native-only together with `set_collect_decode_errors`/`take_decode_errors`/`GpuDecodeError`. Task 3 adds the non-blocking form. PR 1 already made the CDF 5/3 self-test and the texture readback awaitable. The e2e test `e2e_palrle_oob_index` needs the decode-error feedback to work on the web (`error_code=5` in the server log), so dropping the PalRLE error readback is not an option.
- **`note_h264` does not mark damage** (PR 1), so a tick whose only change is a drawn picture has an empty `FlushReport::damage`: `paint_tick` presents on `pictures_drawn > 0` itself, and must not call `mark_all_dirty()` after drawing (the next flush would then report the whole screen and present a second time for nothing).
- **A picture the ledger finds stale is dropped, not drawn** (`SeqLedger::h264_stale`: older than a newer picture or than any tile already noted, which includes tiles queued earlier in the same tick). And an *older* tile queued with a newer picture is dropped by the ledger at flush time whichever order the two are submitted in (`superseded` is evaluated in `flush`, against whatever `note_h264` has recorded). So neither "N+2 over N+1 over N inside one tick" nor "late older tile under a newer picture" can show the submit order. What does: **a tile and a picture with the same `frame_seq` in one tick** (equal never supersedes, so both are drawn): the documented order, tiles flushed first and the picture after, makes the picture win; a picture submitted before `flush()` makes the tile win. Task 11's test walks "tile N, picture N+1, tile N+2, late picture N+1" across ticks (the cross-tick ledger behaviour) and ends with that same-tick equal-seq case, which is the one the breakage turns red.
- **`WasmClientCore::handleDatagrams` returns events to JS today** (`serde_wasm_bindgen`, tile payloads copied into `Uint8Array`s). The new `handleDatagramsInto` and `onTimeoutInto` keep the old methods untouched (vitest drives them) and add the in-wasm route.
- **The debug hooks tied to the deleted shaders have e2e tests** (all VKMS-gated and in `ci/skip-list.txt`, so they run only locally): `e2e_cdf53_bypass_integrate` (`__cdf53TestInverse`), `e2e_cdf53_integrate_correctness` (`__cdf53TestIntegrate`), `e2e_cdf53_live_tile_state` and `_col18` (`__cdf53DumpTileState`, both `#[ignore]`), `e2e_cdf53_tile_watcher` (`__cdf53GetTileWatcher`), `e2e_cdf53_inverse_gradient_tile` (`__cdf53TestInverse`), `e2e_cdf53_dirty_inverse_matches_full_grid` (`__cdf53FullGridDiff`). They test internals (integrate and inverse buffers) that do not exist in the grid. **Owner decision: delete them (Task 14 Step 2).** `failure_dump.rs` guards `__cdf53FullGridDiff` and `__cdf53Probe` with an existence check, so their absence is harmless there.
- **Headless Chrome may not give WebGPU.** The e2e harness comment says `--headless=new` (chromiumoxide `new_headless_mode`) "doesn't support WebGPU adapters", which is why the e2e runs headed under Weston. `wasm-pack test --headless --chrome` is the same mode. Task 1 is a spike that answers it before anything else is built, with a fallback ladder (SwiftShader flags, then headed Chrome under `xvfb-run`) and a stop rule.
- **The dockerised e2e image does not compile `ghostframe-client-wasm`** (it copies the crate's `Cargo.toml`, `src/` and `build.rs` only so the workspace resolves), so `include_str!("../../../shaders/...")` in `web/*.rs` cannot break it. A new `ghostframe-client-wasm/tests/` or `webdriver.json` is not copied and not needed there.
- **`ghostframe-client-wasm` has a host build** (`cargo test --workspace --lib`, clippy `--workspace --all-targets`). Everything that needs `wgpu::webgpu` or `web_sys` GPU handles is `#[cfg(target_arch = "wasm32")]`; the router, the idle-skip rule and the frame-size limits are plain Rust with host unit tests (CI runs those without a browser). CI's workspace clippy never sees the wasm32 code, so Task 15 adds a wasm32 clippy job.
- **Bench metric.** `__ghostframe_framePaints` stamps `rafMsClient` at rAF entry and `codec_report` reads `raf_ms - first_recv_ms`; it is a coarse upper bound that moves with main-thread occupancy, not a paint-cost measurement (the `docs/specs/m3-codec-bench-results.md` caveats say so). Task 18 adds a direct one: `WebRenderer` times each non-skipped `paintTick` with `performance.now()`, published as `window.__ghostframePaintUs()` under `?e2e`, and the same 12 lines are patched (not committed) into a master worktree around `encodeAndPresentFrame` for the baseline.
- **`codec_report` scenes fill the frame with `--tile-pattern` and cannot be set to 4K; the production-scale e2e scenes can** (`--x11-sink` with `GHOSTFRAME_DRM_MODE`: `e2e_production_shaped_session_strands_no_tiles` is 1080p, `e2e_a_4k_screen_runs_on_the_gpu_path_and_converges` is 4096x2160). Task 18 therefore adds one `#[ignore]` e2e test that runs the same scene shapes and prints the paint-cost JSON.

## Gates that stop the work

G1 (delete the seven old-shader e2e tests), G1b (the `GHOSTFRAME_E2E_URL_QUERY_EXTRA` hook) and G1c (the `#[ignore]` bench test) were resolved by the owner: all three are in, with no stop. The other owner answers are folded in below: raw `extern "C"` bindings and no `web_sys_unstable_apis` flag; an `Rgba8Unorm` framebuffer blitted to the canvas; `?cdf53=cpu` standing in for the spec's forced self-test failure; the bench is three runs each at 1080p and 4096x2160 on one screen.

| Gate | Where | What the agent does |
|---|---|---|
| G0 | Task 0 | Contract check against PR 1 fails: stop, list the diffs, wait for the reconciler. |
| G2 | Task 16 | **Wasm size over 512 KiB: STOP. Report the numbers; do not edit `ci/check-wasm-size.sh`, do not add a size-reducing profile, wait for the owner's decision on the cap.** `just ci-local` is expected to fail at exactly that step until then. |
| G3 | Task 18 | Every regression against master at 1080p or 4K is reported with its numbers; what counts as too much is the owner's call (open question 2), not this plan's. |

## File map

| File | Action | Responsibility |
|---|---|---|
| `ghostframe-client-wasm/Cargo.toml` | Modify | `ghostframe-render`, `js-sys`, more `web-sys` features; wasm32-only `wgpu` (`webgpu`) and `wasm-bindgen-futures`; dev-dep `wasm-bindgen-test`. |
| `ghostframe-client-wasm/webdriver.json` | Create | Chrome flags for a WebGPU adapter in the browser tests. |
| `ghostframe-client-wasm/src/lib.rs` | Modify | `pub mod web;`. |
| `ghostframe-client-wasm/src/web/mod.rs` | Create | Module list; the wasm32-only ones are cfg-gated. |
| `ghostframe-client-wasm/src/web/limits.rs` | Create | `check_frame_dimensions` (port of `frame_limits.ts`), host-tested. |
| `ghostframe-client-wasm/src/web/logic.rs` | Create | `PaintInputs`/`should_skip_paint` (port of `idle_skip.ts`), `pick_canvas_format`, `Cdf53Mode`, host-tested. |
| `ghostframe-client-wasm/src/web/route.rs` | Create | `TileSink`, `route`, the slim events JS gets; host-tested with a fake sink. |
| `ghostframe-client-wasm/src/web/raw.rs` | Create | Hand-declared raw WebGPU/WebCodecs bindings (wasm32). |
| `ghostframe-client-wasm/src/web/h264.rs` | Create | The H.264 escape: raw pipeline, `importExternalTexture`, raw pass, raw submit (wasm32). |
| `ghostframe-client-wasm/src/web/present.rs` | Create | The present blit pipeline (wasm32). |
| `ghostframe-client-wasm/src/web/renderer.rs` | Create | `WebRenderer` (wasm32). |
| `ghostframe-client-wasm/src/web/debug.rs` | Create | `readGradientGolden` (port of `__readGradientGolden`) (wasm32). |
| `ghostframe-client-wasm/src/core.rs` | Modify | `handleDatagramsInto`, `onTimeoutInto`; `handle_datagrams` shares its collector. |
| `ghostframe-client-wasm/src/boundary.rs` | Modify | `expand_wire_planes` becomes `pub(crate)`. |
| `ghostframe-client-wasm/tests/webgpu_smoke.rs` | Create | Spike: the browser hands wgpu an adapter. |
| `ghostframe-client-wasm/tests/web_renderer.rs` | Create | Browser tests: ordering, idle skip, errors readback, CDF 5/3 GPU against CPU, session reset, golden gradient. |
| `ghostframe-render/src/errors_readback.rs` | Create | The non-blocking PalRLE error state machine (Task 3). |
| `ghostframe-render/src/readback.rs` | Modify | Rectangle readback that submits at once (Task 2). |
| `ghostframe-render/src/pipelines/palrle.rs`, `src/tile_renderer.rs`, `src/lib.rs` | Modify | Non-blocking PalRLE error readback; decode-error API on wasm (Task 3). |
| `ghostframe-render/src/paint_order.rs`, `src/pipelines/solid.rs` | Modify | Ports of the vitest cases (Task 5). |
| `ghostframe-client-gpu/src/pipelines/cdf53_passes.rs`, `tests/cdf53_passes.rs` | Delete | The native twin of the deleted multi-pass `computePassesProcessed`; nothing calls it (Task 5). |
| `ghostframe-client-gpu/tests/readback_rect.rs` | Create | GPU test of the rectangle readback (Task 2). |
| `ghostframe-web-client/src/main.ts`, `diagnostics.ts`, `stats_log.ts`, `globals.d.ts` | Modify | Use `WebRenderer`; slim the renderer-reading parts. |
| `ghostframe-web-client/src/webgpu/*`, `cdf53_debug.ts`, `frame_limits.ts` | Delete | Replaced by Rust. |
| `ghostframe-web-client/tests/{renderer_paint_order,renderer_errors_readback,renderer_idle_skip,solid_pack,cdf53_dirty_inverse,cdf53_passes_processed,frame_limits}.test.ts` | Delete | Each has its replacement (table in Task 13). |
| `shaders/client/cdf53_integrate.wgsl`, `cdf53_inverse_l{1,1_pass2,2,3}.wgsl` | Delete | Web-only; nothing native uses them. |
| `ghostframe-e2e/src/harness/e2e_setup.rs` | Modify | `GHOSTFRAME_E2E_URL_QUERY_EXTRA` appended to the page URL (Task 14). |
| `ghostframe-e2e/tests/e2e.rs` | Modify | Delete the seven tests (owner-approved); add `e2e_bench_web_paint_us` (`#[ignore]`) (Task 18). |
| `Justfile`, `.github/workflows/client-core.yml`, `AGENTS.md`, `DEVELOPERS.md`, `docs/specs/wasm-bundle-baseline.md` | Modify | Recipes, CI job, docs, size record. |

## Owner decisions (answered) and what is still open

Answered, and built in: (1) delete the seven old-shader e2e tests in this PR, the PR body listing each with what now covers it (Task 14 Step 2 has the table; Task 19 puts it in the body); (2) the `GHOSTFRAME_E2E_URL_QUERY_EXTRA` hook is allowed; (3) the `#[ignore]` `e2e_bench_web_paint_us` test is allowed; (4) hand-declared `#[wasm_bindgen]` externs for the raw WebGPU calls, no build-wide `--cfg=web_sys_unstable_apis`; (5) the bench is three runs each at 1080p and at 4096x2160 on one screen (no multi-output scene); (6) `Rgba8Unorm` framebuffer plus the present blit to the canvas format, and `?cdf53=cpu` in place of the spec's forced self-test failure.

Still open:

1. **Wasm size cap** (Gate G2): the owner decides with Task 16's numbers.
2. **Bench threshold** (Gate G3). The spec says a regression is reported with numbers before merge but names no limit. Task 18 reports every cell where the branch is slower, on both the spec's `__ghostframe_framePaints` metric and the whole-rAF-tick cost; which of those, and how much, blocks the merge is the owner's to say.
3. **Firefox.** `_firefox` e2e tests fail locally on any branch today (memory note) and CI never runs them. The WebRenderer builds on Firefox's WebGPU unverified; the H.264 probe is built to fail soft there (`texture_external`), as `renderer.ts` did.

## Risks (also in the final report)

- **R1 PR 1 drift.** The contract above is assumed, not read. Task 0 checks it mechanically; Tasks 3 to 5 edit PR 1's files by anchor text and may need renaming.
- **R2 SwiftShader or headless WebGPU may not be available** in the browser tests locally or on the CI runner (Task 1 decides; CI's first run is the proof).
- **R3 CDF 5/3 grid cost model on the web.** `try_enable_cdf53_gpu` uses the default cost model (fitted on a Mali-T860, so conservative) and the default per-flush tile cap, with no calibration. On a desktop GPU that can defer work and slow convergence at 4K. The 4K e2e scene and the bench catch it; the lever is `probe_grid(..)` then `enable_cdf53_gpu_with(device, grid, Some(us_per_tile), 60_000)` with a figure measured in the browser (Task 18).
- **R4 Behaviour changes** (listed in the PR description): PalRLE decode errors now carry real tile coordinates (the TS sent `0xFF,0xFF`); `Δdrain` in the page log counts events applied per window instead of queue entries consumed; the canvas format is `rgba8unorm` when offered (as before), else the first non-sRGB format; **at an equal `frame_seq` the H.264 picture now draws over the tile** (native order: tiles flushed first, then pictures), where the TypeScript drew H.264 under the tiles.
- **R5 Wasm size** (Gate G2).
- **R6 Re-entrancy.** `handleDatagramsInto` holds `&mut WebRenderer`; wgpu calls JS synchronously and no JS calls back into wasm during them, so the wasm-bindgen borrow flag is safe. The only async exports (`readPixelRect`, `readGradientGolden`) copy what they need and return a `Promise` from a `'static` future, so no borrow is held across an await while `paintTick` runs.

---

## Task 0 [haiku]: Worktree and contract check

**Files:** none changed.

- [ ] **Step 1: Create the worktree off master with PR 1 merged.**

```bash
cd ~/work/ghostframe
git fetch origin
git log --oneline origin/master | head -20      # PR 1's merge must be visible: look for "ghostframe-render"
git worktree add ~/work/gf-web-renderer -b feat/web-renderer-wgpu origin/master
cd ~/work/gf-web-renderer
ls ghostframe-render/src ghostframe-render/tests
```

Expected: `ghostframe-render/` exists. If it does not, STOP (G0): PR 1 has not merged; this plan stacks on it, so either wait or branch from PR 1's branch.

- [ ] **Step 2: Mechanically check each contract line.**

```bash
cd ~/work/gf-web-renderer
cat > /tmp/gf-wr2-contract.sh <<'EOF'
#!/usr/bin/env bash
set -u
r=ghostframe-render/src
fail=0
chk() { if grep -rqE "$2" $r; then echo "ok   $1"; else echo "MISS $1  /$2/"; fail=1; fi; }
chk C1a 'pub struct TileRenderer'
chk C1b 'pub fn new\('   # the signature is multi-line after rustfmt: read it by eye (C1 in the table)
chk C2  'pub fn apply_event'
chk C3a 'pub fn note_h264'
chk C3b 'pub fn h264_stale'
chk C4  'pub struct FlushReport'
chk C4b 'deferred_work'
chk C5  'pub fn framebuffer\(&self\)'
chk C6a 'pub fn debug_tile'
chk C6b 'pub fn debug_state'
chk C7a 'pub async fn try_enable_cdf53_gpu'
chk C7b 'pub fn cdf53_gpu_enabled'
chk C7c 'pub fn has_deferred_work'
chk C8  'pub fn reset_session'
chk C9a 'pub fn set_collect_decode_errors'
chk C9b 'pub fn take_decode_errors'
chk C9c 'pub struct GpuDecodeError'   # native-gated in PR 1; Task 3 removes the gate
chk C10b 'pub fn block_on_device'
chk C10 'Device\(String\)'
echo "--- C3 semantic: note_h264 cancels pending CDF 5/3 draws and does not mark damage"
grep -n -A6 'pub fn note_h264' -r $r | head -30
echo "--- C12 wasm-safety: places PR 1 must have made non-blocking or cfg-gated"
grep -rn 'Instant::now\|PollType::wait_indefinitely\|PollType::Wait\|mpsc::\|pollster' $r | grep -v '^.*//' || true
exit $fail
EOF
chmod +x /tmp/gf-wr2-contract.sh && /tmp/gf-wr2-contract.sh; echo "rc=$?"
```

Expected: every `ok`; `rc=0`. Every `MISS` line is either (a) a rename (note PR 1's name and use it everywhere this plan says the contract name; keep a list for the PR description) or (b) a gated or missing item, which the named task adds. The last block prints the blocking calls still reachable; only the native-gated PalRLE `take_errors` should remain, and Task 3 replaces it on wasm. `MISS` on C1 to C8 or C10 is G0: stop. (C9 is expected to be native-gated; Task 3 fixes that.)

- [ ] **Step 3: Baseline the three things this PR must not worsen.**

```bash
cd ~/work/gf-web-renderer
cargo build -p ghostframe-render --target wasm32-unknown-unknown 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
ls -l ghostframe-web-client/dist/assets/*.wasm 2>/dev/null || echo "no dist yet (built in Task 16)"
git log -1 --format=%h > /tmp/gf-wr2-base-rev && cat /tmp/gf-wr2-base-rev
```

Expected: the wasm32 check of `ghostframe-render` succeeds (PR 1's CI gate); record the revision. No commit in this task.

## Task 1 [sonnet]: Wire wgpu into the wasm crate, and the WebGPU-in-headless-Chrome spike

**Files:**
- Modify: `ghostframe-client-wasm/Cargo.toml`
- Create: `ghostframe-client-wasm/webdriver.json`, `ghostframe-client-wasm/tests/webgpu_smoke.rs`

This is the spike for risk R2 and the proof of the dependency claims in "Findings". Do it before writing any renderer code.

- [ ] **Step 1: Dependencies.** Replace the `[dependencies]` block of `ghostframe-client-wasm/Cargo.toml` with the following and add the two new tables below it (keep `[package]` and `[lib]` as they are):

```toml
[dependencies]
ghostframe-client-core = { path = "../ghostframe-client-core" }
ghostframe-protocol = { path = "../ghostframe-protocol" }
ghostframe-render = { path = "../ghostframe-render" }
ghostframe-web-ui = { path = "../ghostframe-web-ui" }
wasm-bindgen = "0.2"
js-sys = "0.3"
serde = { version = "1", features = ["derive"] }
serde_bytes = "0.11"
serde-wasm-bindgen = "0.6"
console_error_panic_hook = "0.1"
web-sys = { version = "0.3", features = [
    "HtmlElement",
    "HtmlCanvasElement",
    "Window",
    "Document",
    "Navigator",
    "Performance",
    "console",
] }

# `web/logic.rs` (compiled on every target, host unit tests included) names
# `wgpu::TextureFormat`, so wgpu is a plain dependency; with the workspace's
# `default-features = false` it has no backend on the host.
wgpu = { workspace = true }

# WebGPU exists only in the browser. wgpu's `webgpu` feature is the whole
# backend (no wgpu-core, no naga): `cargo tree --target wasm32-unknown-unknown
# -i naga` must stay empty (checked below and in CI). Feature unification adds
# `webgpu` to the plain entry above on wasm32 only.
[target.'cfg(target_arch = "wasm32")'.dependencies]
wgpu = { workspace = true, features = ["webgpu"] }
wasm-bindgen-futures = "0.4"

[dev-dependencies]
wasm-bindgen-test = "=0.3.79"
# What the browser tests call on the DOM; stated here rather than borrowed from
# feature unification with the normal dependency (it would stop working the day
# a feature is trimmed there).
web-sys = { version = "0.3", features = [
    "Document", "Element", "Node", "HtmlElement", "HtmlCanvasElement", "Window", "console",
] }
```

- [ ] **Step 2: The lockfile may only gain packages.**

```bash
cd ~/work/gf-web-renderer
cargo metadata --format-version 1 > /dev/null; echo "rc=$?"
git diff --stat Cargo.lock
git diff Cargo.lock | grep '^-[^-]' | head      # removed or changed lines
```

Expected: `rc=0`; the lockfile diff adds `[[package]]` entries and a few `dependencies` lines of `ghostframe-client-wasm`; **no `-version` line of an existing package** (`wasm-bindgen` stays 0.2.129, `web-sys` 0.3.106, `js-sys` 0.3.106). A moved version: `cargo update -p <crate> --precise <old>` back, and say so in the PR.

- [ ] **Step 3: naga is absent from the wasm32 graph.**

```bash
cd ~/work/gf-web-renderer
cargo tree -p ghostframe-client-wasm --target wasm32-unknown-unknown -i naga 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
cargo tree -p ghostframe-client-wasm --target wasm32-unknown-unknown -i wgpu-core 2>&1 | tail -3
```

Expected: the first two print `warning: nothing to print` or `error: package ID specification ... did not match`, either meaning "not in the graph"; **if naga or wgpu-core is listed, STOP** and find which crate enabled it (`cargo tree -e features -i naga`), since the size budget (Gate G2) assumes they are absent. `ghostframe-render` must not depend on `naga` outside `[dev-dependencies]`.

- [ ] **Step 4: The spike test.** Create `ghostframe-client-wasm/tests/webgpu_smoke.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Spike: does this browser hand wgpu a WebGPU adapter in the browser tests?
//! Run with `wasm-pack test --headless --chrome ghostframe-client-wasm --test webgpu_smoke`.
#![cfg(target_arch = "wasm32")]

use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

#[wasm_bindgen_test]
async fn the_browser_hands_wgpu_an_adapter_and_a_device() {
    let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
    let adapter = instance
        .request_adapter(&wgpu::RequestAdapterOptions::default())
        .await
        .expect("no WebGPU adapter: see Task 1 of the PR 2 plan for the flag ladder");
    let info = adapter.get_info();
    web_sys::console::log_1(&format!("adapter: {} ({:?})", info.name, info.backend).into());
    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor::default())
        .await
        .expect("request_device");
    assert!(device.as_webgpu().is_some(), "not the browser WebGPU backend");
    assert!(queue.as_webgpu().is_some());
}
```

- [ ] **Step 5: Flags, first attempt.** Create `ghostframe-client-wasm/webdriver.json` (wasm-bindgen-test-runner reads it from the crate directory):

```json
{
  "goog:chromeOptions": {
    "args": [
      "--no-sandbox",
      "--enable-unsafe-webgpu",
      "--enable-features=Vulkan,WebGPU",
      "--use-angle=swiftshader",
      "--use-webgpu-adapter=swiftshader",
      "--disable-vulkan-surface",
      "--ignore-gpu-blocklist"
    ]
  }
}
```

```bash
cd ~/work/gf-web-renderer
wasm-pack test --headless --chrome ghostframe-client-wasm --test webgpu_smoke 2>&1 | tail -25; echo "rc=${PIPESTATUS[0]}"
```

Expected on success: `test result: ok. 1 passed` and a console line `adapter: ... SwiftShader ...`; `rc=0`. SwiftShader is software, so this step touches no hardware GPU.

- [ ] **Step 6: If it fails with "no WebGPU adapter", climb the ladder, one rung at a time, re-running Step 5's command:**
  1. wasm-bindgen-test-runner adds `--headless` and may add `--disable-gpu`; check with `RUST_LOG=debug wasm-pack test ... 2>&1 | grep -i "disable-gpu\|args"`. If `--disable-gpu` is present, add `"--enable-gpu"` after it in `webdriver.json`, or drop `--use-angle=swiftshader`.
  2. Replace `--headless=new` semantics: run headed under Xvfb. `xvfb-run -a wasm-pack test --chrome ghostframe-client-wasm --test webgpu_smoke` (no `--headless`). Xvfb is installed here and is what CI's X11 tests use.
  3. Use the host GPU instead of SwiftShader: drop the two `swiftshader` flags and run as `~/work/gf-gpu-run xvfb-run -a wasm-pack test --chrome ...` (this touches the GPU; follow the wrapper's rules).
  4. If none yields an adapter: **STOP.** Report the three outputs. The browser tests of Tasks 11 would then have to run inside the e2e harness (headed Weston), which is a different plan; the host unit tests of Tasks 5 to 7 and 10 and the rest of the work are unaffected, and the e2e gates of Task 17 still cover behaviour.

Record the flag set that worked (and whether it needs Xvfb) in `webdriver.json` and in the Justfile recipe of Task 15.

- [ ] **Step 7: Break it, watch it fail.** Temporarily change the test to `request_adapter(&wgpu::RequestAdapterOptions { force_fallback_adapter: true, ..Default::default() })` with SwiftShader flags removed from `webdriver.json`, or simply point `chromeOptions.args` at `"--disable-webgpu"`; the test must fail with the `no WebGPU adapter` message, proving it asserts a real adapter. Revert.

- [ ] **Step 8: Commit.**

```bash
cd ~/work/gf-web-renderer
git status --short
git add ghostframe-client-wasm/Cargo.toml ghostframe-client-wasm/webdriver.json ghostframe-client-wasm/tests/webgpu_smoke.rs Cargo.lock
git commit -m "build(web): wgpu's WebGPU backend in the wasm crate, and a browser adapter smoke test

wgpu =30.0.1 with webgpu+wgsl on wasm32 only; naga and wgpu-core are not
in the wasm32 graph. webdriver.json carries the Chrome flags that give the
browser tests a WebGPU adapter.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 2 [sonnet]: A rectangle readback that submits at once

PR 1's `readback::read_texture_rgba8(device, queue, &texture, w, h)` reads a whole texture and submits its copy when the future is first polled. `WebRenderer` needs two things it does not give: a rectangle (`__readPixel` reads one pixel), and a copy that is **submitted when the call is made**, ordered after everything already on the queue, with only the wait deferred (an `async` body would be polled on a later microtask, after the next `paintTick` may have changed the framebuffer). This task splits PR 1's function in two; `read_texture_rgba8` stays, as a wrapper. PR 1's private one-shot channel is reused; nothing else is added.

**Files:**
- Modify: `ghostframe-render/src/readback.rs`
- Create: `ghostframe-client-gpu/tests/readback_rect.rs` (GPU test; the render crate has no device helper of its own, the native `WgpuContext` is the device source for every PR 1 GPU test)

- [ ] **Step 1: Split `read_texture_rgba8`.** In `ghostframe-render/src/readback.rs` replace `read_texture_rgba8` with the following (the body is PR 1's, re-cut; `oneshot`, `Receiver` and `GpuError` are the file's own):

```rust
/// Rows of `padded` (pitch `padded_row` bytes) cut to `width_px` RGBA pixels
/// each, in one tight buffer. A readback's rows are padded to
/// `COPY_BYTES_PER_ROW_ALIGNMENT`; skipping this looks right at the start of
/// each row and drifts further out of alignment with every row after it.
pub(crate) fn tight_rows(padded: &[u8], width_px: u32, height_px: u32, padded_row: u32) -> Vec<u8> {
    let row = width_px as usize * 4;
    let mut tight = Vec::with_capacity(row * height_px as usize);
    for y in 0..height_px as usize {
        let start = y * padded_row as usize;
        tight.extend_from_slice(&padded[start..start + row]);
    }
    tight
}

/// A texture-to-buffer copy that has been **submitted**; [`PendingRead::finish`]
/// waits for the bytes. Splitting the two lets a caller order the copy after
/// everything already submitted and await it later.
pub struct PendingRead {
    staging: wgpu::Buffer,
    mapped: Receiver<Result<(), wgpu::BufferAsyncError>>,
    width: u32,
    height: u32,
    padded_row: u32,
}

/// Copy the `w` x `h` rectangle at (`x`, `y`) of a 4-byte-per-texel `texture`
/// (it needs `COPY_SRC`) into a staging buffer, submit the copy through
/// [`crate::submit::submit`] and start mapping the buffer.
pub fn begin_texture_read(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> PendingRead {
    let padded_row = (w * 4).div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT)
        * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("ghostframe-texture-readback"),
        size: u64::from(padded_row) * u64::from(h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("ghostframe-texture-readback"),
    });
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d { x, y, z: 0 },
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &staging,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row),
                rows_per_image: Some(h),
            },
        },
        wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
    );
    crate::submit::submit(queue, encoder);
    let (done, mapped) = oneshot();
    staging.slice(..).map_async(wgpu::MapMode::Read, done);
    PendingRead {
        staging,
        mapped,
        width: w,
        height: h,
        padded_row,
    }
}

impl PendingRead {
    /// The rectangle as `w * h * 4` tight RGBA bytes. Needs no device: the
    /// map callback is delivered by the browser's event loop, or on native by
    /// whoever polls the device ([`block_on_device`]).
    pub async fn finish(self) -> Result<Vec<u8>, GpuError> {
        self.mapped
            .await
            .map_err(|e| GpuError::Device(format!("map readback staging buffer: {e}")))?;
        let padded = self
            .staging
            .slice(..)
            .get_mapped_range()
            .map_err(|e| GpuError::Device(format!("readback get_mapped_range: {e}")))?;
        let tight = tight_rows(&padded, self.width, self.height, self.padded_row);
        drop(padded);
        self.staging.unmap();
        Ok(tight)
    }
}

/// Copy the whole `width` x `height` `texture` into tight rows,
/// `width * height * 4` bytes. Submits one encoder of its own.
pub async fn read_texture_rgba8(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, GpuError> {
    begin_texture_read(device, queue, texture, 0, 0, width, height)
        .finish()
        .await
}
```

and add to the file's test module (create `#[cfg(test)] mod tests` if PR 1 has none):

```rust
#[cfg(test)]
mod tests {
    use super::tight_rows;

    #[test]
    fn tight_rows_drops_exactly_the_padding() {
        // 3 px wide, 2 rows, pitch 16: row 0 = 1.., row 1 = 101.., padding 0xEE.
        let mut raw = vec![0xEE; 32];
        raw[..12].copy_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
        raw[16..28].copy_from_slice(&[101, 102, 103, 104, 105, 106, 107, 108, 109, 110, 111, 112]);
        let out = tight_rows(&raw, 3, 2, 16);
        assert_eq!(out.len(), 24);
        assert!(!out.contains(&0xEE), "padding leaked into the result");
        assert_eq!(&out[12..16], &[101, 102, 103, 104]);
    }
}
```

- [ ] **Step 2: The GPU test (native, local; "no GPU is a failure, not a skip").** `ghostframe-client-gpu/tests/readback_rect.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! `begin_texture_read` against a real device: a rectangle at an offset, and
//! a copy that is ordered at the call, not at the first poll. Needs a GPU; not
//! named in any CI workflow.

use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_render::readback::{begin_texture_read, block_on_device};

fn texture(ctx: &WgpuContext, w: u32, h: u32) -> wgpu::Texture {
    ctx.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    })
}

fn write(ctx: &WgpuContext, t: &wgpu::Texture, w: u32, h: u32, pixels: &[u8]) {
    ctx.queue.write_texture(
        t.as_image_copy(),
        pixels,
        wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
        wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
    );
}

#[test]
fn a_rectangle_reads_back_exactly_and_the_padding_is_gone() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let (w, h) = (70u32, 5u32); // 280 bytes a row: not a multiple of 256
    let t = texture(&ctx, w, h);
    let pixels: Vec<u8> = (0..(w * h * 4)).map(|i| (i % 251) as u8).collect();
    write(&ctx, &t, w, h, &pixels);
    let whole = block_on_device(&ctx.device, begin_texture_read(&ctx.device, &ctx.queue, &t, 0, 0, w, h).finish())
        .expect("read");
    assert_eq!(whole, pixels, "the whole texture");
    let rect = block_on_device(&ctx.device, begin_texture_read(&ctx.device, &ctx.queue, &t, 3, 2, 10, 2).finish())
        .expect("read rect");
    let want: Vec<u8> = (2..4)
        .flat_map(|y| pixels[((y * w + 3) * 4) as usize..((y * w + 13) * 4) as usize].to_vec())
        .collect();
    assert_eq!(rect, want, "an offset rectangle");
}

#[test]
fn the_copy_is_ordered_at_the_call_not_at_the_first_poll() {
    let ctx = WgpuContext::new().expect("wgpu context");
    let t = texture(&ctx, 4, 1);
    write(&ctx, &t, 4, 1, &[1; 16]);
    let before = begin_texture_read(&ctx.device, &ctx.queue, &t, 0, 0, 4, 1);
    write(&ctx, &t, 4, 1, &[2; 16]); // after the call, before anything is awaited
    let after = begin_texture_read(&ctx.device, &ctx.queue, &t, 0, 0, 4, 1);
    let got_before = block_on_device(&ctx.device, before.finish()).unwrap();
    let got_after = block_on_device(&ctx.device, after.finish()).unwrap();
    assert_eq!(got_before, vec![1; 16], "a copy submitted before the second write sees the first");
    assert_eq!(got_after, vec![2; 16]);
}
```

(`ctx.device`/`ctx.queue` are public fields of `WgpuContext`, as `ghostframe-client-native` uses them; if PR 1's native tests use accessors, use those.)

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-render --lib readback 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
~/work/gf-gpu-run cargo test -p ghostframe-client-gpu --test readback_rect 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
```

Expected: 1 and 2 tests pass. **Break each once, then revert:** (a) in `tight_rows` use `y * row` as the stride: the host test fails; (b) in `begin_texture_read` replace `origin: Origin3d { x, y, z: 0 }` with `{ x: 0, y: 0, z: 0 }`: the offset-rectangle assertion fails; (c) move `crate::submit::submit(queue, encoder);` into `finish` (copy at first poll): `the_copy_is_ordered_at_the_call` fails (`before` reads 2s). That last one is the property `WebRenderer::read_pixel_rect` relies on.

- [ ] **Step 3: Gates and commit.**

```bash
cd ~/work/gf-web-renderer
cargo clippy -p ghostframe-render -p ghostframe-client-gpu --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
cargo check -p ghostframe-render --target wasm32-unknown-unknown 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
git add ghostframe-render/src/readback.rs ghostframe-client-gpu/tests/readback_rect.rs
git commit -m "feat(render): begin_texture_read, a rectangle readback that submits at the call

read_texture_rgba8 is now a wrapper; the web renderer orders its pixel reads
after everything already submitted and awaits them later.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 3 [sonnet]: PalRLE decode errors without blocking

PR 1 left the PalRLE error readback, and with it `set_collect_decode_errors`, `take_decode_errors` and `GpuDecodeError`, native-only (`finish_flush` calls `PalRlePipeline::take_errors`, which blocks on the device). On wasm32 a blocking wait is a hang or a panic, and the e2e suite needs the decode-error feedback on the web. The browser needs the old TypeScript behaviour: copy the errors buffer into a staging buffer only when no readback holds it, map it asynchronously, report what it finds one tick later. This is the logic `renderer_errors_readback.test.ts` pins ("a paint tick must never be rejected because the previous tick's readback still holds `errorsStaging`").

**Files:**
- Create: `ghostframe-render/src/errors_readback.rs`
- Modify: `ghostframe-render/src/lib.rs` (un-gate the `GpuDecodeError` re-export), `ghostframe-render/src/pipelines/palrle.rs`, `ghostframe-render/src/tile_renderer.rs`

- [ ] **Step 1: The state machine first (pure, host-tested).** Create `ghostframe-render/src/errors_readback.rs`:

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The non-blocking PalRLE errors readback: one staging buffer, at most one
//! readback holding it, results kept until taken.
//!
//! The rule it exists for (from the TypeScript renderer it replaces): WebGPU
//! rejects a `queue.submit` that uses a buffer which is not unmapped. In the
//! TypeScript the errors copy rode in the tick's one encoder, so the rejection
//! took the tick's Solid draws, CDF 5/3 work and present with it. Here the copy
//! is a submit of its own, so what is lost is that batch's errors plus an
//! uncaptured validation error; either way the copy happens only if
//! [`ErrorsReadback::try_begin`] says the staging buffer is free, and the
//! buffer is unmapped before [`ErrorsReadback::complete`] publishes.

use std::sync::{Arc, Mutex, MutexGuard};

/// One finished readback: `(tile_x, tile_y, code)` for every non-zero slot.
pub type Found = Vec<(u8, u8, u32)>;

enum State {
    Idle,
    /// A copy was submitted and `map_async` is pending or mapped.
    InFlight(Vec<(u8, u8)>),
    /// The staging buffer is unmapped again; the result waits for `take`.
    Ready(Result<Found, String>),
}

#[derive(Clone)]
pub struct ErrorsReadback {
    state: Arc<Mutex<State>>,
}

impl Default for ErrorsReadback {
    fn default() -> Self {
        Self::new()
    }
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ErrorsReadback {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Idle)),
        }
    }

    /// Claim the staging buffer for a readback of the batch `tiles` (slot `i`
    /// belongs to `tiles[i]`). `false` when a readback is in flight **or its
    /// result has not been taken**: the caller must not copy into the staging
    /// buffer, and the batch's errors go unreported (as they did before).
    pub fn try_begin(&self, tiles: Vec<(u8, u8)>) -> bool {
        let mut s = lock(&self.state);
        match *s {
            State::Idle => {
                *s = State::InFlight(tiles);
                true
            }
            State::InFlight(_) | State::Ready(_) => false,
        }
    }

    /// Whether a readback holds the staging buffer **or has a result nobody
    /// has taken yet**. The idle skip must treat both as work: once the map
    /// callback has run, the result sits in `Ready` and only a `take` (from
    /// `TileRenderer::take_decode_errors`) reports it; a screen that stopped
    /// changing would otherwise never report its error.
    pub fn pending(&self) -> bool {
        matches!(*lock(&self.state), State::InFlight(_) | State::Ready(_))
    }

    /// Called by the map callback **after** the staging buffer is unmapped.
    /// `words` are the first 32-bit word of each error slot, in batch order.
    pub fn complete(&self, words: Result<Vec<u32>, String>) {
        let mut s = lock(&self.state);
        let tiles = match std::mem::replace(&mut *s, State::Idle) {
            State::InFlight(t) => t,
            // A completion with nothing in flight is a bug in the caller; keep
            // whatever is there rather than invent a result.
            other => {
                *s = other;
                return;
            }
        };
        *s = State::Ready(words.map(|w| {
            w.iter()
                .zip(tiles.iter())
                .filter(|(code, _)| **code != 0)
                .map(|(code, (x, y))| (*x, *y, *code))
                .collect()
        }));
    }

    /// The finished readback, if any; frees the staging buffer for the next.
    pub fn take(&self) -> Option<Result<Found, String>> {
        let mut s = lock(&self.state);
        match std::mem::replace(&mut *s, State::Idle) {
            State::Ready(r) => Some(r),
            other => {
                *s = other;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_readback_cannot_start_while_one_holds_the_staging_buffer() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(0, 0)]));
        assert!(rb.pending());
        assert!(!rb.try_begin(vec![(1, 1)]), "staging buffer is held");
    }

    #[test]
    fn a_finished_result_is_kept_until_taken_and_blocks_the_next_begin() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(2, 3), (4, 5)]));
        rb.complete(Ok(vec![0, 5]));
        assert!(rb.pending(), "a result nobody has taken is still pending: the idle skip must not hide it");
        assert!(!rb.try_begin(vec![(9, 9)]), "an untaken result must not be overwritten");
        assert_eq!(rb.take(), Some(Ok(vec![(4, 5, 5)])));
        assert_eq!(rb.take(), None);
        assert!(!rb.pending(), "taking ends it");
        assert!(rb.try_begin(vec![(9, 9)]), "taking frees the buffer");
    }

    #[test]
    fn zero_slots_and_slots_past_the_batch_are_ignored() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(1, 1)]));
        rb.complete(Ok(vec![0, 7, 7, 7]));
        assert_eq!(rb.take(), Some(Ok(vec![])));
    }

    #[test]
    fn a_failed_map_is_reported_and_frees_the_buffer() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(0, 0)]));
        rb.complete(Err("device lost".into()));
        assert_eq!(rb.take(), Some(Err("device lost".to_string())));
        assert!(rb.try_begin(vec![(0, 0)]));
    }

    #[test]
    fn a_stray_completion_changes_nothing() {
        let rb = ErrorsReadback::new();
        rb.complete(Ok(vec![1]));
        assert_eq!(rb.take(), None);
        assert!(rb.try_begin(vec![(0, 0)]));
    }
}
```

Add `pub mod errors_readback;` to `ghostframe-render/src/lib.rs`.

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-render --lib errors_readback 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
```

Expected: 5 pass. Break: make `pending` return `matches!(.., State::InFlight(_))` only (the old `in_flight`): `a_finished_result_is_kept_...` fails on its `pending` assertion; make `try_begin` return `true` in the `InFlight` arm; `a_second_readback_cannot_start...` fails. Make it return true in the `Ready` arm; the second test fails. Revert both.

- [ ] **Step 2: The GPU half in `palrle.rs`.** Next to `take_errors` add (use PR 1's names for `errors_buffer`/`errors_staging`/`ERROR_STRIDE`; the staging buffer is created with `COPY_DST | MAP_READ` already):

```rust
    /// The non-blocking counterpart of [`PalRlePipeline::take_errors`], for
    /// targets that cannot wait on the device. Copies the first
    /// `tiles.len()` error slots into the staging buffer and starts the map;
    /// the result is published to `rb` when the map completes. Does nothing
    /// when `rb` says a readback still holds the staging buffer.
    pub fn start_errors_readback(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        tiles: Vec<(u8, u8)>,
        rb: &crate::errors_readback::ErrorsReadback,
    ) {
        let (Some(errors_buffer), Some(staging)) = (self.errors_buffer.as_ref(), self.errors_staging.as_ref())
        else {
            return;
        };
        let read_bytes = tiles.len() as u64 * ERROR_STRIDE;
        if read_bytes == 0 || !rb.try_begin(tiles) {
            return;
        }
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("ghostframe-palrle-errors-readback"),
        });
        encoder.copy_buffer_to_buffer(errors_buffer, 0, staging, 0, read_bytes);
        queue.submit(std::iter::once(encoder.finish()));

        let staging = staging.clone();
        let for_map = staging.clone();
        let rb = rb.clone();
        for_map.slice(0..read_bytes).map_async(wgpu::MapMode::Read, move |r| {
            let words = match r {
                Ok(()) => {
                    let words = {
                        let view = staging.slice(0..read_bytes).get_mapped_range();
                        view.map(|v| {
                            v.chunks_exact(ERROR_STRIDE as usize)
                                .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                                .collect::<Vec<u32>>()
                        })
                        .map_err(|e| format!("get_mapped_range: {e}"))
                    };
                    // Unmap BEFORE publishing: `complete` is what lets the
                    // next tick copy into this buffer again.
                    staging.unmap();
                    words
                }
                Err(e) => Err(format!("map: {e}")),
            };
            rb.complete(words);
        });
    }
```

If PR 1's `ERROR_STRIDE` is not a `u64` or the `get_mapped_range` result is not a `Result`, mirror the shapes in `take_errors` above it.

- [ ] **Step 3: The `TileRenderer` side.** PR 1's `tile_renderer.rs` and `lib.rs` gate the decode-error machinery with `#[cfg(not(target_arch = "wasm32"))]`. Remove every one of those attributes, so the machinery exists on all targets, and keep the gate only on the one thing that blocks:

1. `ghostframe-render/src/lib.rs`: merge `GpuDecodeError` into the unconditional `pub use tile_renderer::{...}` list (delete the separate `#[cfg(not(target_arch = "wasm32"))] pub use tile_renderer::GpuDecodeError;`).
2. `tile_renderer.rs`: delete the attribute above `MAX_PENDING_DECODE_ERRORS`, `GpuDecodeError` (and its doc), `decode_error_code`, the fields `decode_errors`, `collect_decode_errors` and `palrle_tiles`, their three initialisers in `new`, `set_collect_decode_errors` and `take_decode_errors`, and the attribute above `self.palrle_tiles = ...` in `record`. Add a field `errors_rb: crate::errors_readback::ErrorsReadback` (initialised with `ErrorsReadback::new()`).
3. Replace the body of `finish_flush` from `#[cfg(not(target_arch = "wasm32"))] {` to the `let _ = (device, queue);` line with:

```rust
        let tiles = std::mem::take(&mut self.palrle_tiles);
        if !tiles.is_empty() && self.collect_decode_errors {
            // Native waits for the device; the browser cannot, so it copies the
            // error slots into a staging buffer only if no readback holds it
            // and collects the result on a later flush (`errors_rb`).
            #[cfg(not(target_arch = "wasm32"))]
            {
                let errors = self.palrle.take_errors(device, queue)?;
                let found: Vec<(u8, u8, u32)> = errors
                    .into_iter()
                    .filter_map(|(i, code)| tiles.get(i as usize).map(|&(x, y)| (x, y, code)))
                    .collect();
                self.push_decode_errors(found);
            }
            #[cfg(target_arch = "wasm32")]
            self.palrle
                .start_errors_readback(device, queue, tiles, &self.errors_rb);
        }
        // Readbacks that finished since the last flush (none natively).
        self.collect_finished_errors();
```

(keep the `self.flushes = self.flushes.saturating_add(1); Ok(())` that follows), and add these two methods to the `impl`:

```rust
    /// Record error slots as [`GpuDecodeError`]s, bounded like the native path
    /// always was: a hostile stream of bad tiles must not grow the list
    /// without limit between two drains.
    fn push_decode_errors(&mut self, found: impl IntoIterator<Item = (u8, u8, u32)>) {
        for (tile_x, tile_y, code) in found {
            let Some(code) = decode_error_code(code) else {
                tracing::warn!(code, tile_x, tile_y, "unknown GPU palrle error code");
                continue;
            };
            if self.decode_errors.len() < MAX_PENDING_DECODE_ERRORS {
                self.decode_errors.push(GpuDecodeError {
                    codec: Codec::PalRle,
                    tile_x,
                    tile_y,
                    code,
                });
            }
        }
    }

    /// Move a finished non-blocking readback (if any) into the decode-error
    /// list. Called by `finish_flush` and by [`TileRenderer::take_decode_errors`]:
    /// the second matters on a static screen, where no flush follows the map
    /// callback and the result would otherwise sit in `errors_rb` for ever.
    fn collect_finished_errors(&mut self) {
        if let Some(done) = self.errors_rb.take() {
            match done {
                Ok(found) => self.push_decode_errors(found),
                Err(e) => tracing::warn!(error = %e, "PalRLE errors readback failed"),
            }
        }
    }

    /// Whether a PalRLE errors readback is in flight or finished and not yet
    /// taken. A caller's idle skip must not skip a tick while this is true.
    pub fn errors_readback_pending(&self) -> bool {
        self.errors_rb.pending()
    }
```

and replace the body of `take_decode_errors` with `self.collect_finished_errors(); std::mem::take(&mut self.decode_errors)` (a result that arrived between flushes is reported by the next call, flush or no flush), and in the docs of `set_collect_decode_errors`/`take_decode_errors` replace "Native only: the readback blocks on the device" with "On wasm32 the readback is asynchronous: errors arrive on a later flush".

- [ ] **Step 4: The native behaviour is unchanged.**

```bash
cd ~/work/gf-web-renderer
cargo check -p ghostframe-render --target wasm32-unknown-unknown 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
~/work/gf-gpu-run cargo test -p ghostframe-render 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-render -p ghostframe-client-gpu --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```

Expected: all green (the native PalRLE error tests in `gpu_pipelines.rs`, PR 1's `tile_renderer` tests and the renderer tests still use the blocking path). Also run `ci/check-render-portable.sh` (PR 1's wasm guard): it must now find `GpuDecodeError` on wasm32 and still no blocking call. The wasm32 half is exercised by Task 11's browser test, which is where the "break it" for the copy guard lives.

- [ ] **Step 5: Commit.**

```bash
cd ~/work/gf-web-renderer
git add ghostframe-render/src/errors_readback.rs ghostframe-render/src/lib.rs ghostframe-render/src/pipelines/palrle.rs ghostframe-render/src/tile_renderer.rs
git commit -m "feat(render): PalRLE decode errors on targets that cannot block

A staging buffer held by one readback is never copied into by the next tick
(WebGPU rejects a submit using a mapped buffer), and results wait for take(). Native keeps
the blocking path.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 4: withdrawn (PR 1 provides it)

This task was a non-blocking CDF 5/3 probe (`probe_async`, an async self-test and a clock-driven calibration). PR 1 ships the portable half: `TileRenderer::try_enable_cdf53_gpu(device, queue).await` runs the self-test (`cdf53_grid::self_test`, async, in error scopes) and enables the grid on success, and `probe`/`Probed`/calibration stay native in `ghostframe-client-gpu`. `WebRenderer` calls `try_enable_cdf53_gpu` (Task 9); nothing is added here. Task numbers are kept so cross-references stay valid.

## Task 5 [haiku]: Port the vitest cases that belong to the shared crate

Spec section 8: `renderer_paint_order` -> `ghostframe-render` unit tests on the shared ledger; `solid_pack` -> a unit test "if not already covered natively"; `cdf53_passes_processed` -> **obsolete**: the vitest pinned `computePassesProcessed` of the multi-pass inverse, which the grid replaced (it keeps its own pass bookkeeping in `Cdf53GridState`, covered by its 35 tests). The native `passes_processed` in `ghostframe-client-gpu/src/pipelines/cdf53_passes.rs` is a twin of that deleted function that nothing in production calls (checked with grep: only its own test), so this task retires it instead of growing its tests. The within-tick `planPaint` of the TS renderer has no Rust twin: its job is done by the ledger and `Cdf53GridState` (cancelled and refused passes), already covered by `cdf53_state.rs`'s 35 tests and `tests/gpu_paint_order.rs`. This table is the written reason per case.

**Files:**
- Modify: `ghostframe-render/src/paint_order.rs` (tests), `ghostframe-render/src/pipelines/solid.rs` (extract `pack_instances` + test), `ghostframe-client-gpu/src/pipelines/cdf53_passes.rs`, `ghostframe-client-gpu/src/pipelines/mod.rs` and `ghostframe-client-gpu/tests/cdf53_passes.rs` (delete / un-declare: they stayed in `ghostframe-client-gpu` in PR 1)

Mapping of `tests/renderer_paint_order.test.ts` (first column: the `it(...)`):

| TS case | Rust home |
|---|---|
| older Solid does not cover a newer CDF53 lift; newer Solid still covers an older lift; arrival order does not matter | `cdf53_state.rs` `a_newer_stateless_tile_cancels_a_pending_draw_and_older_passes`, `an_older_or_equal_stateless_tile_does_not_cancel` (already) |
| older/newer H.264 frame vs CDF53 lift | `paint_order.rs` ledger: `an_h264_picture_supersedes_older_tiles_...` (already) + new tests below; `cdf53_state.rs` `supersede_all_cancels_everything_pending_and_refuses_older_passes`, `a_late_picture_does_not_cancel_the_pending_tiles_of_a_later_frame` (already) |
| older Solid vs newer PalRle, and the reverse | `paint_order.rs` `older_is_superseded_equal_and_newer_are_not` (already; codec-independent) + `tests/gpu_paint_order.rs` (GPU) |
| older Solid under a newer H.264 frame | `paint_order.rs` (already) |
| H.264 frames that cannot be drawn do not suppress tiles | n/a: the ledger is only told about pictures that were drawn (`note_h264` after the draw); asserted by Task 11's browser test |
| keeps h264Queue and h264Seqs aligned when the CDF53 upload throws | n/a: no parallel arrays in `WebRenderer` (pictures carry their seq in one struct) |
| never drops a CDF53 pass, even one a newer frame supersedes | `cdf53_state.rs` `a_new_generation_is_never_refused_as_stale`, `a_stateless_record_ends_once_a_newer_pass_is_accepted` (already) |
| across ticks: older Solid/PalRle/CDF53 do not cover newer; equal seq painted; other tiles unaffected | `paint_order.rs` (already) + `tests/gpu_paint_order.rs::an_older_stateless_tile_in_a_later_flush_does_not_cover_a_newer_one` |
| a late H.264 frame does not repaint over newer tiles; is closed not leaked | `paint_order.rs::an_h264_picture_supersedes...` (stale) + Task 11 browser test (closed) |
| a newer H.264 frame covers older tiles; after a picture older tiles/passes/pictures are dropped | `paint_order.rs` (already) |
| an older frame queued with a newer one in the same tick is not drawn over it | **new** `older_picture_after_newer_picture...` |
| compares frame_seq as serial numbers across a wrap | `paint_order.rs::newest_wins_regardless_of_note_order_and_across_a_wrap` (already) + **new** `picture_staleness_follows_the_seq_wrap` |
| a framebuffer resize forgets the ledger; a session reset forgets it | `clear_forgets` (already) + `gpu_paint_order.rs::the_ledger_is_forgotten_when_the_framebuffer_is_resized` + Task 11 (session reset) |
| two entries for a tile in one tick leave the newest | `paint_order.rs::newest_wins_regardless_of_note_order...` (already) |
| `planPaint` serial arithmetic / pre-wrap drop / older under newer PalRle / other tiles | same as above |

- [ ] **Step 1: New ledger tests.** Append to the `tests` module of `ghostframe-render/src/paint_order.rs`:

```rust
    #[test]
    fn older_picture_after_newer_picture_is_stale_and_an_equal_one_is_not() {
        let mut l = SeqLedger::default();
        l.note_h264(11);
        assert!(l.h264_stale(10), "an older picture queued after a newer one");
        assert!(!l.h264_stale(11), "equal never supersedes");
        assert!(!l.h264_stale(12));
    }

    #[test]
    fn picture_staleness_follows_the_seq_wrap() {
        let mut l = SeqLedger::default();
        l.note_h264(1); // 1 is newer than 0x7FFF_FFFF: the 31-bit wire counter wrapped
        assert!(l.h264_stale(0x7FFF_FFFF));
        assert!(!l.h264_stale(2));
        let mut l = SeqLedger::default();
        l.note(0, 0, 1);
        assert!(l.h264_stale(0x7FFF_FFF0), "a tile after the wrap makes a pre-wrap picture stale");
    }

    #[test]
    fn a_stateless_entry_equal_to_a_picture_is_kept_and_an_older_one_dropped() {
        let mut l = SeqLedger::default();
        l.note_h264(11);
        assert!(!l.superseded(5, 5, 11));
        assert!(l.superseded(5, 5, 10));
    }

    #[test]
    fn one_newer_tile_anywhere_makes_every_older_picture_stale() {
        let mut l = SeqLedger::default();
        l.note(200, 100, 50);
        for seq in [1, 49] {
            assert!(l.h264_stale(seq), "{seq}");
        }
        assert!(!l.h264_stale(50));
    }
```

- [ ] **Step 2: Solid packing.** In `ghostframe-render/src/pipelines/solid.rs` extract the loop in `SolidPipeline::encode` that builds `data` into a free function and call it (no behaviour change):

```rust
/// The instance buffer for `tiles`: per tile `tile_x: u32, tile_y: u32,
/// color_packed: u32`, little-endian. The colour is the wire's BGRA bytes
/// unchanged: the shader's `unpack4x8unorm` reads them as a little-endian u32
/// whose LSB-first byte order is exactly BGRA, then swizzles to RGBA.
pub(crate) fn pack_instances(tiles: &[(u8, u8, [u8; 4])]) -> Vec<u8> {
    let mut data = Vec::with_capacity(tiles.len() * INSTANCE_STRIDE as usize);
    for (tile_x, tile_y, bgra) in tiles {
        data.extend_from_slice(&(*tile_x as u32).to_le_bytes());
        data.extend_from_slice(&(*tile_y as u32).to_le_bytes());
        data.extend_from_slice(bgra);
    }
    data
}
```

In `encode`, replace the `let mut data = ...; for ... {}` with `let data = pack_instances(tiles);`. Append a test module (port of `solid_pack.test.ts`, which asserted B at the LSB of the u32):

```rust
#[cfg(test)]
mod tests {
    use super::pack_instances;

    fn colour_word(bgra: [u8; 4]) -> u32 {
        let d = pack_instances(&[(3, 4, bgra)]);
        assert_eq!(d.len(), 12);
        assert_eq!(&d[..4], &3u32.to_le_bytes());
        assert_eq!(&d[4..8], &4u32.to_le_bytes());
        u32::from_le_bytes([d[8], d[9], d[10], d[11]])
    }

    #[test]
    fn packs_bgra_with_blue_in_the_low_byte() {
        // The vitest's three vectors: [B, G, R, A] -> u32 with B at the LSB.
        assert_eq!(colour_word([0xFF, 0x00, 0x00, 0xFF]), 0xFF00_00FF);
        assert_eq!(colour_word([0x00, 0xFF, 0x00, 0xFF]), 0xFF00_FF00);
        assert_eq!(colour_word([0x00, 0x00, 0xFF, 0xFF]), 0xFFFF_0000);
    }
}
```

- [ ] **Step 3: Retire `cdf53_passes.rs` and its tests.**

```bash
cd ~/work/gf-web-renderer
grep -rn "cdf53_passes\|passes_processed\|FULL_PASS_MASK\|MAX_PASSES" --include=*.rs --include=*.toml --include=*.sh --include=*.yml . 2>/dev/null | grep -v "^./target\|^./ghostframe-client-gpu/src/pipelines/cdf53_passes.rs\|^./ghostframe-client-gpu/tests/cdf53_passes.rs"
```

Expected: only the `pub mod cdf53_passes;` line in `ghostframe-client-gpu/src/pipelines/mod.rs`. Any other hit is a production user: stop, keep the file, and say so in the PR. Then:

```bash
git rm ghostframe-client-gpu/src/pipelines/cdf53_passes.rs ghostframe-client-gpu/tests/cdf53_passes.rs
sed -i '/^pub mod cdf53_passes;$/d' ghostframe-client-gpu/src/pipelines/mod.rs
cargo check -p ghostframe-client-gpu --all-targets 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
```

Expected `rc=0`. The written reason for the vitest row is in the PR description (Task 13's table): "obsolete: the grid replaced the multi-pass inverse".

- [ ] **Step 4: Run, break, commit.**

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-render --lib -- paint_order pipelines::solid 2>&1 | tail -12; echo "rc=${PIPESTATUS[0]}"
```

Expected: all pass (no GPU needed for these). Break each family once and watch it fail, then revert: (a) in `seq_newer`, return `a > b`: `picture_staleness_follows_the_seq_wrap` fails; (b) in `pack_instances` write `bgra` reversed: `packs_bgra_with_blue_in_the_low_byte` fails.

```bash
cd ~/work/gf-web-renderer
cargo clippy -p ghostframe-render -p ghostframe-client-gpu --all-targets -- -D warnings 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
git add ghostframe-render/src/paint_order.rs ghostframe-render/src/pipelines/solid.rs ghostframe-client-gpu/src/pipelines/mod.rs
git status --short | head   # the two cdf53_passes deletions are already staged by git rm
git commit -m "test(render): port the ledger and Solid packing vitest cases; retire cdf53_passes

paint-order ledger (pictures vs tiles, wrap) and Solid instance packing as host
tests. passes_processed mirrored the deleted multi-pass inverse and nothing
calls it, so it goes with its tests; the table of what covers each remaining
TypeScript case is in the PR description.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 6 [sonnet]: The decisions that need no GPU: frame limits, idle skip, canvas format

Plain Rust with host unit tests, so CI runs them without a browser. `check_frame_dimensions` is the port of `frame_limits.ts` (the spec moves that file's tests in PR 3; it moves now, because the router of Task 7 needs the check and the TypeScript no longer will). `should_skip_paint` is the port of `idle_skip.ts` and `renderer_idle_skip.test.ts`.

**Files:**
- Create: `ghostframe-client-wasm/src/web/mod.rs`, `src/web/limits.rs`, `src/web/logic.rs`
- Modify: `ghostframe-client-wasm/src/lib.rs` (add `pub mod web;` after `pub mod units;`)

- [ ] **Step 1: `src/web/mod.rs`.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The browser renderer. The parts that decide things (`limits`, `logic`,
//! `route`) are plain Rust and unit-tested on the host; everything that
//! touches WebGPU or the DOM is wasm32-only.

pub mod limits;
pub mod logic;
pub mod route;
```

Add the module to the crate and give `route` a file to compile until Task 7 fills it:

```bash
cd ~/work/gf-web-renderer
sed -i 's/^pub mod units;$/pub mod units;\npub mod web;/' ghostframe-client-wasm/src/lib.rs
grep -n '^pub mod' ghostframe-client-wasm/src/lib.rs
printf '// SPDX-License-Identifier: MIT OR Apache-2.0\n//! Filled in by Task 7.\n' > ghostframe-client-wasm/src/web/route.rs
```

- [ ] **Step 2: `src/web/limits.rs`.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `ghostframe-web-client/src/frame_limits.ts`.
//!
//! A `FrameDimensions` event drives a framebuffer allocation proportional to
//! width x height. The authoritative per-axis bound is the core's
//! `MAX_FRAME_DIMENSION` (read from there, not retyped); this adds the tile
//! count bound the old WebGPU dispatch had (4 workgroups a tile against a
//! 65535 dispatch limit => 16383 tiles).
//!
//! The TypeScript also rejected non-integers, negatives and NaN; a `u32`
//! cannot hold those, so those vitest cases have no Rust twin by construction.

use ghostframe_client_core::MAX_FRAME_DIMENSION;
use ghostframe_protocol::tile::TILE_SIZE;

pub const MAX_TILES: u32 = 16383;

/// `Ok` when a server-announced size is safe to allocate; otherwise why not.
pub fn check_frame_dimensions(width: u32, height: u32) -> Result<(), String> {
    if width == 0 || height == 0 {
        return Err(format!("empty size {width}x{height}"));
    }
    if width > MAX_FRAME_DIMENSION || height > MAX_FRAME_DIMENSION {
        return Err(format!(
            "size {width}x{height} exceeds the {MAX_FRAME_DIMENSION}px limit"
        ));
    }
    let tiles = width.div_ceil(TILE_SIZE) * height.div_ceil(TILE_SIZE);
    if tiles > MAX_TILES {
        return Err(format!(
            "size {width}x{height} needs more than {MAX_TILES} tiles"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_and_boundary_sizes() {
        for (w, h) in [
            (640, 480),
            (1920, 1080),
            (3840, 2160),
            (MAX_FRAME_DIMENSION, 1),
            (1, MAX_FRAME_DIMENSION),
        ] {
            assert!(check_frame_dimensions(w, h).is_ok(), "{w}x{h}");
        }
    }

    #[test]
    fn rejects_oversize_and_empty_sizes_and_too_many_tiles() {
        for (w, h) in [
            (MAX_FRAME_DIMENSION + 1, 480),
            (640, MAX_FRAME_DIMENSION + 1),
            (u32::MAX, u32::MAX),
            (0, 480),
            (640, 0),
            // fits per axis, needs more than 16383 tiles
            (MAX_FRAME_DIMENSION, 4320),
        ] {
            assert!(check_frame_dimensions(w, h).is_err(), "{w}x{h}");
        }
    }

    #[test]
    fn the_limit_covers_what_the_u8_tile_grid_can_address_and_not_much_more() {
        // Tile coordinates are u8 with two values reserved as sentinels.
        let addressable = (254 + 1) * TILE_SIZE;
        assert!(MAX_FRAME_DIMENSION >= addressable - TILE_SIZE);
        assert!(MAX_FRAME_DIMENSION <= addressable + TILE_SIZE * 8);
    }
}
```

- [ ] **Step 3: `src/web/logic.rs`.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `idle_skip.ts`, plus the two small choices `WebRenderer::create`
//! makes: which canvas format to present in and how the CDF 5/3 decode path is
//! chosen. Plain Rust: no browser needed to test them.

/// What `paint_tick` knows when it decides whether to touch the GPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PaintInputs {
    /// A tile event, a resize, a session reset or a picture arrived since the
    /// last present.
    pub dirty: bool,
    /// H.264 pictures waiting to be drawn or dropped.
    pub h264_pending: usize,
    /// CDF 5/3 work carried over from the last flush (budget or throttle).
    pub deferred_work: bool,
    /// A PalRLE errors readback is in flight, or finished and not yet taken
    /// (`TileRenderer::errors_readback_pending`). The TypeScript rule was
    /// "in flight" only; a finished result must count too or a static screen
    /// never reports it.
    pub errors_readback_pending: bool,
}

/// True when the tick can return without encoding, submitting or presenting
/// anything: the canvas would show what it already shows.
pub fn should_skip_paint(i: &PaintInputs) -> bool {
    !(i.dirty || i.h264_pending > 0 || i.deferred_work || i.errors_readback_pending)
}

/// How CDF 5/3 tiles are decoded, from the page URL's `cdf53` parameter
/// (read by `main.ts`, so this crate never touches the environment).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cdf53Mode {
    /// The GPU grid if `TileRenderer::try_enable_cdf53_gpu`'s self-test passes, else the CPU decoder.
    Auto,
    /// The CPU decoder, without trying the grid (native: `GHOSTFRAME_CDF53_DECODE=cpu`). The
    /// e2e run that proves the fallback path on the web. (A mode that ran the self-test and
    /// then pretended it failed would need a way to disable an enabled grid, which
    /// `TileRenderer` does not have; the real failure branch is covered by PR 1's
    /// `the_self_test_keeps_a_grid_that_draws_nothing_off`.)
    Cpu,
}

impl Cdf53Mode {
    pub fn parse(s: &str) -> Self {
        match s {
            "cpu" => Cdf53Mode::Cpu,
            _ => Cdf53Mode::Auto,
        }
    }
}

/// The canvas format to present in. The page used `rgba8unorm` before (so the
/// copy from the framebuffer is byte-for-byte), and still prefers it; a canvas
/// that does not offer it gets the first non-sRGB format (an sRGB target would
/// re-encode the pixels), else the first one.
pub fn pick_canvas_format(offered: &[wgpu::TextureFormat]) -> wgpu::TextureFormat {
    use wgpu::TextureFormat::Rgba8Unorm;
    if offered.contains(&Rgba8Unorm) {
        return Rgba8Unorm;
    }
    offered
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .or_else(|| offered.first().copied())
        .unwrap_or(Rgba8Unorm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use wgpu::TextureFormat::{Bgra8Unorm, Bgra8UnormSrgb, Rgba8Unorm};

    #[test]
    fn skips_when_nothing_is_pending() {
        assert!(should_skip_paint(&PaintInputs::default()));
    }

    #[test]
    fn does_not_skip_for_any_one_reason() {
        for (name, i) in [
            ("dirty", PaintInputs { dirty: true, ..Default::default() }),
            ("pictures", PaintInputs { h264_pending: 1, ..Default::default() }),
            ("deferred", PaintInputs { deferred_work: true, ..Default::default() }),
            (
                "readback",
                PaintInputs { errors_readback_pending: true, ..Default::default() },
            ),
        ] {
            assert!(!should_skip_paint(&i), "{name}");
        }
    }

    #[test]
    fn the_cdf53_mode_defaults_to_auto_and_names_the_one_override() {
        assert_eq!(Cdf53Mode::parse(""), Cdf53Mode::Auto);
        assert_eq!(Cdf53Mode::parse("auto"), Cdf53Mode::Auto);
        assert_eq!(Cdf53Mode::parse("cpu"), Cdf53Mode::Cpu);
        assert_eq!(Cdf53Mode::parse("fail"), Cdf53Mode::Auto, "the removed spelling must not pick an override");
        assert_eq!(Cdf53Mode::parse("gpu?"), Cdf53Mode::Auto, "a typo must not silently pick an override");
    }

    #[test]
    fn the_canvas_format_prefers_rgba8unorm_then_avoids_srgb() {
        assert_eq!(pick_canvas_format(&[Bgra8Unorm, Rgba8Unorm]), Rgba8Unorm);
        assert_eq!(pick_canvas_format(&[Bgra8UnormSrgb, Bgra8Unorm]), Bgra8Unorm);
        assert_eq!(pick_canvas_format(&[Bgra8UnormSrgb]), Bgra8UnormSrgb);
        assert_eq!(pick_canvas_format(&[]), Rgba8Unorm);
    }
}
```

- [ ] **Step 4: Run, break, commit.**

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-client-wasm --lib web:: 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

Expected: 7 tests pass. This is a host build: `web/logic.rs` names `wgpu::TextureFormat`, which compiles because `wgpu` is a plain dependency of the crate (Task 1); if it fails with E0433, that entry is missing. Break: in `should_skip_paint` drop `|| i.errors_readback_pending`: `does_not_skip_for_any_one_reason` names `readback`; in `check_frame_dimensions` delete the tile-count `if`: the `(8192, 4320)` case fails; in `pick_canvas_format` remove the `Rgba8Unorm` early return: the first assertion fails. Revert each.

```bash
cd ~/work/gf-web-renderer
git add ghostframe-client-wasm/src/web/mod.rs ghostframe-client-wasm/src/web/limits.rs ghostframe-client-wasm/src/web/logic.rs ghostframe-client-wasm/src/web/route.rs ghostframe-client-wasm/src/lib.rs
git commit -m "feat(web): frame limits, idle-skip rule and canvas format as host-tested Rust

Ports of frame_limits.ts and idle_skip.ts with their vitest cases.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 7 [sonnet]: The event router

`route(events, sink)` applies tile work to a `TileSink` (the renderer) and returns what JS still needs: per tile a slim bookkeeping record (codec, coordinates, full length, first 16 bytes, the PalRLE wire flags), the palette colours (the e2e fingerprint log needs them), the dimension verdict, and `NeedsH264`/`DecodeError`/`Evicted` untouched. One event failing must not discard the rest of its batch (the core has acknowledged those tiles; this was `handleSafely`'s rule), so a failure is carried in the record's `error`.

**Files:**
- Create: `ghostframe-client-wasm/src/web/route.rs`
- Modify: `ghostframe-client-wasm/src/web/mod.rs` (already lists `route`), `ghostframe-client-wasm/src/boundary.rs` (make `expand_wire_planes` `pub(crate)`), `ghostframe-client-wasm/Cargo.toml` (`serde_json = "1"` under `[dev-dependencies]`)

- [ ] **Step 1: Tests first.** Create `ghostframe-client-wasm/src/web/route.rs` with the test module below plus the implementation of Step 2 (write both, then run; Step 3 breaks it). The test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Records what it was given; fails the tiles it is told to.
    #[derive(Default)]
    struct FakeSink {
        seen: Vec<Event>,
        fail_tile: Option<(u8, u8)>,
        fail_resize: bool,
    }

    impl TileSink for FakeSink {
        fn apply(&mut self, ev: Event) -> Result<(), String> {
            let fails = match &ev {
                Event::TilePayload { tile_x, tile_y, .. } => self.fail_tile == Some((*tile_x, *tile_y)),
                Event::FrameDimensions { .. } => self.fail_resize,
                _ => false,
            };
            self.seen.push(ev);
            if fails {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
    }

    fn palrle(frame_seq: u32, tx: u8, ty: u8) -> Event {
        Event::TilePayload {
            frame_seq,
            tile_x: tx,
            tile_y: ty,
            generation: 0,
            data: TileData::PalRle {
                palette_id: 3,
                count: 2,
                indices: (0..512u32).map(|i| i as u8).collect(),
                wire_flags: 0b11,
            },
        }
    }

    #[test]
    fn a_tile_reaches_the_sink_whole_and_js_gets_a_sample_and_the_full_length() {
        let mut sink = FakeSink::default();
        let out = route(vec![palrle(9, 1, 2)], &mut sink);
        match &sink.seen[0] {
            Event::TilePayload { data: TileData::PalRle { indices, .. }, .. } => {
                assert_eq!(indices.len(), 512, "the sink must get every byte");
            }
            other => panic!("{other:?}"),
        }
        match &out[..] {
            [Routed::Tile(SlimEvent::TilePayload {
                frame_seq: 9,
                tile_x: 1,
                tile_y: 2,
                data: SlimTileData::PalRle { palette_id: 3, count: 2, wire_flags: 0b11, indices, len },
                error: None,
                ..
            })] => {
                assert_eq!(*len, 512);
                assert_eq!(indices.len(), SAMPLE_LEN);
                assert_eq!(indices[..4], [0, 1, 2, 3]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn solid_is_carried_whole_and_cdf53_is_sampled_with_its_length() {
        let mut sink = FakeSink::default();
        let solid = Event::TilePayload {
            frame_seq: 1, tile_x: 0, tile_y: 0, generation: 0,
            data: TileData::Solid([1, 2, 3, 4]),
        };
        let cdf = Event::TilePayload {
            frame_seq: 1, tile_x: 1, tile_y: 0, generation: 2,
            data: TileData::Cdf53 {
                pass_idx: 4, bit_planes: vec![7; 384], present_passes: Some(0x3FFF), new_generation: false,
            },
        };
        let out = route(vec![solid, cdf], &mut sink);
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::TilePayload { data: SlimTileData::Solid { bytes, len: 4 }, .. }) if bytes == &[1, 2, 3, 4]
        ));
        assert!(matches!(
            &out[1],
            Routed::Tile(SlimEvent::TilePayload {
                data: SlimTileData::Cdf53 { pass_idx: 4, bit_planes, present_passes: Some(0x3FFF), len: 384 }, ..
            }) if bit_planes.len() == SAMPLE_LEN
        ));
    }

    #[test]
    fn order_is_kept_and_one_failing_tile_does_not_stop_the_batch() {
        let mut sink = FakeSink { fail_tile: Some((1, 1)), ..Default::default() };
        let out = route(vec![palrle(1, 0, 0), palrle(1, 1, 1), palrle(1, 2, 2)], &mut sink);
        assert_eq!(sink.seen.len(), 3, "the tile after the failing one was still applied");
        let errors: Vec<bool> = out
            .iter()
            .map(|r| matches!(r, Routed::Tile(SlimEvent::TilePayload { error: Some(_), .. })))
            .collect();
        assert_eq!(errors, [false, true, false]);
    }

    #[test]
    fn a_bad_frame_size_is_reported_and_not_applied_and_a_good_one_is() {
        let mut sink = FakeSink::default();
        let out = route(
            vec![
                Event::FrameDimensions { width: 0, height: 480 },
                Event::FrameDimensions { width: 64, height: 64 },
            ],
            &mut sink,
        );
        assert_eq!(sink.seen.len(), 1, "only the valid size reaches the renderer");
        assert!(matches!(&out[0], Routed::Tile(SlimEvent::FrameDimensions { applied: false, reason: Some(_), .. })));
        assert!(matches!(&out[1], Routed::Tile(SlimEvent::FrameDimensions { width: 64, height: 64, applied: true, reason: None })));
    }

    #[test]
    fn a_resize_the_renderer_refuses_is_reported_with_its_reason() {
        let mut sink = FakeSink { fail_resize: true, ..Default::default() };
        let out = route(vec![Event::FrameDimensions { width: 64, height: 64 }], &mut sink);
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::FrameDimensions { applied: false, reason: Some(r), .. }) if r == "boom"
        ));
    }

    #[test]
    fn palette_updates_are_applied_and_reported_with_their_colours() {
        let mut sink = FakeSink::default();
        let out = route(
            vec![Event::PaletteUpdated { palette_id: 7, colors: vec![[1, 2, 3, 4], [5, 6, 7, 8]] }],
            &mut sink,
        );
        assert!(matches!(&sink.seen[0], Event::PaletteUpdated { palette_id: 7, colors } if colors.len() == 2));
        assert!(matches!(
            &out[0],
            Routed::Tile(SlimEvent::PaletteUpdated { palette_id: 7, colors, error: None }) if colors[1] == [5, 6, 7, 8]
        ));
    }

    #[test]
    fn h264_decode_errors_and_evictions_go_to_js_untouched() {
        let mut sink = FakeSink::default();
        let h264 = Event::NeedsH264 {
            frame_seq: 5, timestamp_us: 1, is_keyframe: true, ref_frame_seq: 0xFFFF_FFFF,
            flags: 0, slots: 0, reset_decoder: true, payload: vec![9; 10],
        };
        let out = route(vec![h264], &mut sink);
        assert!(sink.seen.is_empty(), "H.264 is the escape's job, not the tile renderer's");
        assert!(matches!(&out[0], Routed::Passthrough(WasmEvent::NeedsH264 { payload, .. }) if payload.len() == 10));
    }

    #[test]
    fn a_tile_ready_is_reported_and_not_applied() {
        // The core is built with tile_delivery_payload = true, so this cannot
        // happen; main.ts logs it as an error rather than render it.
        let mut sink = FakeSink::default();
        let out = route(
            vec![Event::TileReady { frame_seq: 1, tile_x: 0, tile_y: 0, rgba: vec![0; 4096] }],
            &mut sink,
        );
        assert!(sink.seen.is_empty());
        assert!(matches!(&out[0], Routed::Tile(SlimEvent::TileReady { frame_seq: 1, .. })));
    }

    #[test]
    fn an_unexpandable_cdf53_wire_pass_is_dropped_like_the_native_renderer_does() {
        let mut sink = FakeSink::default();
        let wire = Event::TilePayload {
            frame_seq: 1, tile_x: 0, tile_y: 0, generation: 0,
            data: TileData::Cdf53Wire { pass_idx: 0, streams: vec![1, 2, 3], present_passes: None, new_generation: false },
        };
        let out = route(vec![wire], &mut sink);
        assert!(out.is_empty());
        assert!(sink.seen.is_empty());
    }

    #[test]
    fn the_serialised_shape_is_what_main_ts_reads() {
        let mut sink = FakeSink::default();
        let out = route(
            vec![
                palrle(9, 1, 2),
                Event::FrameDimensions { width: 64, height: 64 },
                Event::NeedsH264 {
                    frame_seq: 5, timestamp_us: 1, is_keyframe: false, ref_frame_seq: 4,
                    flags: 0, slots: 0, reset_decoder: false, payload: vec![1, 2, 3],
                },
            ],
            &mut sink,
        );
        let v = serde_json::to_value(&out).expect("serialises");
        assert_eq!(v[0]["kind"], "TilePayload");
        assert_eq!(v[0]["frame_seq"], 9);
        assert_eq!(v[0]["data"]["codec"], "PalRle");
        assert_eq!(v[0]["data"]["len"], 512);
        assert_eq!(v[0]["data"]["wire_flags"], 3);
        assert_eq!(v[0]["data"]["indices"].as_array().map(Vec::len), Some(16));
        assert!(v[0].get("error").is_none(), "no error key unless there is one");
        assert_eq!(v[1]["kind"], "FrameDimensions");
        assert_eq!(v[1]["applied"], true);
        assert_eq!(v[2]["kind"], "NeedsH264");
        assert_eq!(v[2]["payload"].as_array().map(Vec::len), Some(3));
    }
}
```

- [ ] **Step 2: The implementation** (above the test module in the same file):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Where the core's events go in the browser: tile work into the renderer
//! (inside wasm, so tile bytes never cross into JS), the rest to JS.
//!
//! JS still keeps the e2e bookkeeping (`__ghostframeRecordedTiles`, the
//! `__h5_tilePushLog`, ...), which needs a tile's codec, coordinates and size
//! but not its bytes. Each tile therefore comes back as a *slim* record: the
//! full payload length and at most [`SAMPLE_LEN`] leading bytes (enough for
//! the hex prefix `cdf53_globals.ts` logs and for `solid-red` marking).

use ghostframe_client_core::{Event, TileData};
use serde::Serialize;

use crate::boundary::{expand_wire_planes, WasmEvent};
use crate::web::limits::check_frame_dimensions;

/// Leading payload bytes a slim record carries.
pub const SAMPLE_LEN: usize = 16;

/// What applies tile events: the renderer, or a fake in the tests.
pub trait TileSink {
    /// Apply one event (tile payload, palette update or frame dimensions).
    fn apply(&mut self, ev: Event) -> Result<(), String>;
}

/// A tile payload as JS sees it: field names match `WasmTileData`, the byte
/// fields hold at most [`SAMPLE_LEN`] bytes, and `len` is the real length.
#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "codec")]
pub enum SlimTileData {
    Solid {
        #[serde(with = "serde_bytes")]
        bytes: Vec<u8>,
        len: usize,
    },
    PalRle {
        palette_id: u8,
        count: u8,
        wire_flags: u8,
        #[serde(with = "serde_bytes")]
        indices: Vec<u8>,
        len: usize,
    },
    Cdf53 {
        pass_idx: u8,
        #[serde(with = "serde_bytes")]
        bit_planes: Vec<u8>,
        present_passes: Option<u16>,
        len: usize,
    },
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "kind")]
pub enum SlimEvent {
    TilePayload {
        frame_seq: u32,
        tile_x: u8,
        tile_y: u8,
        generation: u8,
        data: SlimTileData,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    PaletteUpdated {
        palette_id: u8,
        colors: Vec<[u8; 4]>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    FrameDimensions {
        width: u32,
        height: u32,
        applied: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    /// Cannot happen (the core delivers payloads); reported, never rendered.
    TileReady { frame_seq: u32, tile_x: u8, tile_y: u8 },
}

/// One entry of the array JS gets back.
#[derive(Debug, Serialize, PartialEq)]
#[serde(untagged)]
pub enum Routed {
    Tile(SlimEvent),
    /// `NeedsH264`, `DecodeError`, `Evicted`: exactly what `handleDatagrams`
    /// returned before (the H.264 payload has to reach the JS decoder).
    Passthrough(WasmEvent),
}

fn sample(bytes: &[u8]) -> Vec<u8> {
    bytes[..bytes.len().min(SAMPLE_LEN)].to_vec()
}

fn slim_of(data: &TileData) -> Option<SlimTileData> {
    Some(match data {
        TileData::Solid(bgra) => SlimTileData::Solid { bytes: bgra.to_vec(), len: bgra.len() },
        TileData::PalRle { palette_id, count, indices, wire_flags } => SlimTileData::PalRle {
            palette_id: *palette_id,
            count: *count,
            wire_flags: *wire_flags,
            indices: sample(indices),
            len: indices.len(),
        },
        TileData::Cdf53 { pass_idx, bit_planes, present_passes, new_generation: _ } => SlimTileData::Cdf53 {
            pass_idx: *pass_idx,
            bit_planes: sample(bit_planes),
            present_passes: *present_passes,
            len: bit_planes.len(),
        },
        // Expanded to `Cdf53` before this is called.
        TileData::Cdf53Wire { .. } => return None,
    })
}

/// Apply the tile work in `events` to `sink`, in order, and return the records
/// JS needs, in the same order. Never stops early: a failing event is carried
/// in its own record.
pub fn route<S: TileSink + ?Sized>(events: Vec<Event>, sink: &mut S) -> Vec<Routed> {
    let mut out = Vec::with_capacity(events.len());
    for ev in events {
        match ev {
            Event::TilePayload { frame_seq, tile_x, tile_y, generation, data } => {
                let data = if let TileData::Cdf53Wire { pass_idx, streams, present_passes, new_generation } = data {
                    // Only produced with `cdf53_wire` on, which the browser never
                    // sets; expanded (or dropped) as the native renderer does.
                    let Some(bit_planes) = expand_wire_planes(&streams) else {
                        continue;
                    };
                    TileData::Cdf53 { pass_idx, bit_planes, present_passes, new_generation }
                } else {
                    data
                };
                let Some(slim) = slim_of(&data) else {
                    continue;
                };
                let error = sink
                    .apply(Event::TilePayload { frame_seq, tile_x, tile_y, generation, data })
                    .err();
                out.push(Routed::Tile(SlimEvent::TilePayload {
                    frame_seq,
                    tile_x,
                    tile_y,
                    generation,
                    data: slim,
                    error,
                }));
            }
            Event::PaletteUpdated { palette_id, colors } => {
                let error = sink
                    .apply(Event::PaletteUpdated { palette_id, colors: colors.clone() })
                    .err();
                out.push(Routed::Tile(SlimEvent::PaletteUpdated { palette_id, colors, error }));
            }
            Event::FrameDimensions { width, height } => {
                let verdict = check_frame_dimensions(width, height)
                    .and_then(|()| sink.apply(Event::FrameDimensions { width, height }));
                out.push(Routed::Tile(SlimEvent::FrameDimensions {
                    width,
                    height,
                    applied: verdict.is_ok(),
                    reason: verdict.err(),
                }));
            }
            Event::TileReady { frame_seq, tile_x, tile_y, rgba: _ } => {
                out.push(Routed::Tile(SlimEvent::TileReady { frame_seq, tile_x, tile_y }));
            }
            ev @ (Event::NeedsH264 { .. } | Event::DecodeError { .. } | Event::Evicted { .. }) => {
                if let Some(w) = WasmEvent::from_event(ev) {
                    out.push(Routed::Passthrough(w));
                }
            }
        }
    }
    out
}
```

In `ghostframe-client-wasm/src/boundary.rs` change `fn expand_wire_planes` to `pub(crate) fn expand_wire_planes`. Add to `Cargo.toml` `[dev-dependencies]`: `serde_json = "1"`.

- [ ] **Step 3: Run, then break each behaviour once.**

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-client-wasm --lib web::route 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
```

Expected: 9 tests pass, clippy clean (every `Event` and `TileData` variant is named: `wildcard_enum_match_arm` is on in this crate). Break and revert: (a) make `sample` return the whole slice: the first two tests fail; (b) delete the `.err()` capture (use `let _ =` and `error: None`): `order_is_kept...` fails; (c) skip `sink.apply` for `PaletteUpdated`: its test fails; (d) call `sink.apply` for `NeedsH264` too: `h264_decode_errors...` fails; (e) remove `check_frame_dimensions` from the `FrameDimensions` arm: the `a_bad_frame_size...` test fails.

- [ ] **Step 4: Commit.**

```bash
cd ~/work/gf-web-renderer
git add ghostframe-client-wasm/src/web/route.rs ghostframe-client-wasm/src/boundary.rs ghostframe-client-wasm/Cargo.toml Cargo.lock
git commit -m "feat(web): route core events to the renderer inside wasm

Tile payloads, palettes and frame sizes are applied to a TileSink; JS gets a
slim record per tile (full length, 16-byte sample) and the H.264 access units.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 8 [sonnet]: The raw WebGPU escape for H.264 (`web/raw.rs`, `web/h264.rs`)

wgpu cannot import a `VideoFrame` as an external texture on the web. The escape builds one `GPURenderPipeline` from `h264_blit.wgsl` (`texture_external`) on `device.as_webgpu()`, and per decoded frame does `importExternalTexture`, a raw render pass into the framebuffer's view (`create_view(..).as_webgpu()`), and a raw `GPUQueue.submit`, then the caller closes the frame. It uses only wgpu's public `as_webgpu()` handles, never wgpu internals; if a wgpu upgrade changes them, only these two files change.

The raw calls are declared here with `#[wasm_bindgen] extern "C"`, so no `--cfg=web_sys_unstable_apis` is needed (Findings; owner decision 4).

**Files:**
- Create: `ghostframe-client-wasm/src/web/raw.rs`, `ghostframe-client-wasm/src/web/h264.rs`
- Modify: `ghostframe-client-wasm/src/web/mod.rs`

- [ ] **Step 1: `src/web/raw.rs`.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Hand-declared bindings for the raw WebGPU and WebCodecs calls the H.264
//! escape makes. The objects they are called on are the very ones wgpu uses
//! (`Device::as_webgpu`, `Queue::as_webgpu`, `TextureView::as_webgpu`).
//!
//! web-sys has these behind `--cfg=web_sys_unstable_apis`; declaring the dozen
//! methods here keeps that flag out of every build of this repository.
//! Methods that can throw on bad input are `catch`; the rest report problems
//! through the device's error scopes, as WebGPU does.

use js_sys::{Array, Object, Promise, Reflect};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// A `GPUDevice`.
    #[wasm_bindgen(extends = Object)]
    pub type RawDevice;
    #[wasm_bindgen(method, catch, js_name = createSampler)]
    pub fn create_sampler(this: &RawDevice, descriptor: &JsValue) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, catch, js_name = createShaderModule)]
    pub fn create_shader_module(this: &RawDevice, descriptor: &JsValue) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, catch, js_name = createRenderPipeline)]
    pub fn create_render_pipeline(
        this: &RawDevice,
        descriptor: &JsValue,
    ) -> Result<RawPipeline, JsValue>;
    /// Throws for a closed or foreign `VideoFrame`.
    #[wasm_bindgen(method, catch, js_name = importExternalTexture)]
    pub fn import_external_texture(
        this: &RawDevice,
        descriptor: &JsValue,
    ) -> Result<JsValue, JsValue>;
    #[wasm_bindgen(method, js_name = createBindGroup)]
    pub fn create_bind_group(this: &RawDevice, descriptor: &JsValue) -> JsValue;
    #[wasm_bindgen(method, js_name = createCommandEncoder)]
    pub fn create_command_encoder(this: &RawDevice) -> RawEncoder;
    #[wasm_bindgen(method, js_name = pushErrorScope)]
    pub fn push_error_scope(this: &RawDevice, filter: &str);
    #[wasm_bindgen(method, js_name = popErrorScope)]
    pub fn pop_error_scope(this: &RawDevice) -> Promise;

    /// A `GPUQueue`.
    #[wasm_bindgen(extends = Object)]
    pub type RawQueue;
    #[wasm_bindgen(method)]
    pub fn submit(this: &RawQueue, command_buffers: &Array);

    /// A `GPURenderPipeline`.
    #[wasm_bindgen(extends = Object)]
    pub type RawPipeline;
    #[wasm_bindgen(method, js_name = getBindGroupLayout)]
    pub fn get_bind_group_layout(this: &RawPipeline, index: u32) -> JsValue;

    /// A `GPUCommandEncoder`.
    #[wasm_bindgen(extends = Object)]
    pub type RawEncoder;
    #[wasm_bindgen(method, js_name = beginRenderPass)]
    pub fn begin_render_pass(this: &RawEncoder, descriptor: &JsValue) -> RawPass;
    #[wasm_bindgen(method)]
    pub fn finish(this: &RawEncoder) -> JsValue;

    /// A `GPURenderPassEncoder`.
    #[wasm_bindgen(extends = Object)]
    pub type RawPass;
    #[wasm_bindgen(method, js_name = setPipeline)]
    pub fn set_pipeline(this: &RawPass, pipeline: &RawPipeline);
    #[wasm_bindgen(method, js_name = setBindGroup)]
    pub fn set_bind_group(this: &RawPass, index: u32, bind_group: &JsValue);
    #[wasm_bindgen(method, js_name = setViewport)]
    pub fn set_viewport(
        this: &RawPass,
        x: f32,
        y: f32,
        width: f32,
        height: f32,
        min_depth: f32,
        max_depth: f32,
    );
    #[wasm_bindgen(method)]
    pub fn draw(this: &RawPass, vertex_count: u32);
    #[wasm_bindgen(method)]
    pub fn end(this: &RawPass);
}

/// The `GPUDevice` behind a wgpu device, or why there is none.
pub fn raw_device(device: &wgpu::Device) -> Result<RawDevice, String> {
    let handle = device
        .as_webgpu()
        .ok_or("the wgpu device is not the browser WebGPU backend")?;
    let js: &JsValue = handle.as_ref();
    Ok(js.clone().unchecked_into())
}

pub fn raw_queue(queue: &wgpu::Queue) -> Result<RawQueue, String> {
    let handle = queue
        .as_webgpu()
        .ok_or("the wgpu queue is not the browser WebGPU backend")?;
    let js: &JsValue = handle.as_ref();
    Ok(js.clone().unchecked_into())
}

/// The `GPUTextureView` behind a wgpu view.
pub fn raw_view(view: &wgpu::TextureView) -> Result<JsValue, String> {
    let handle = view
        .as_webgpu()
        .ok_or("the wgpu texture view is not the browser WebGPU backend")?;
    let js: &JsValue = handle.as_ref();
    Ok(js.clone())
}

/// A plain JS object from `(key, value)` pairs.
pub fn obj(entries: &[(&str, JsValue)]) -> JsValue {
    let o = Object::new();
    for (k, v) in entries {
        // Setting a property on a fresh plain object cannot fail.
        let _ = Reflect::set(&o, &JsValue::from_str(k), v);
    }
    o.into()
}

pub fn array(items: &[JsValue]) -> JsValue {
    items.iter().cloned().collect::<Array>().into()
}

pub fn num(n: u32) -> JsValue {
    JsValue::from_f64(f64::from(n))
}

/// `VideoFrame.close()`; closing twice is harmless, and a value that is not a
/// frame is ignored (a stale close must never trap the module).
pub fn close_frame(frame: &JsValue) {
    let close = Reflect::get(frame, &JsValue::from_str("close"))
        .ok()
        .and_then(|f| f.dyn_into::<js_sys::Function>().ok());
    if let Some(close) = close {
        let _ = close.call0(frame);
    }
}

/// The `message` of a JS error (or the value's string form).
pub fn js_message(e: &JsValue) -> String {
    Reflect::get(e, &JsValue::from_str("message"))
        .ok()
        .and_then(|m| m.as_string())
        .or_else(|| e.as_string())
        .unwrap_or_else(|| format!("{e:?}"))
}
```

- [ ] **Step 2: `src/web/h264.rs`.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The H.264 escape (spec 5.3): a decoded `VideoFrame` onto the framebuffer
//! with no copy through wasm memory.
//!
//! Ordering is the caller's job (`WebRenderer::paint_tick`): the paint ledger
//! decides whether a picture is stale (`TileRenderer::h264_stale`), this only
//! draws the ones it is handed. wgpu's WebGPU backend hands each
//! `queue.submit` to the same `GPUQueue` immediately, so a raw submit made
//! after `TileRenderer::flush` lands after the flush's work.

use js_sys::Array;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::JsFuture;

use super::raw::{self, RawDevice, RawPipeline, RawQueue};

const H264_WGSL: &str = include_str!("../../../shaders/client/h264_blit.wgsl");

pub struct H264Blit {
    device: RawDevice,
    queue: RawQueue,
    pipeline: RawPipeline,
    sampler: JsValue,
}

impl H264Blit {
    /// Build the pipeline inside a `validation` error scope, as `probeH264`
    /// did: a browser without `texture_external` (Firefox) fails here, softly,
    /// and `Err` means "H.264 is unsupported in this browser", not a bug.
    pub async fn probe(device: &wgpu::Device, queue: &wgpu::Queue) -> Result<H264Blit, String> {
        let raw_device = raw::raw_device(device)?;
        let raw_queue = raw::raw_queue(queue)?;
        raw_device.push_error_scope("validation");
        let built = Self::build(&raw_device);
        let scope = JsFuture::from(raw_device.pop_error_scope())
            .await
            .map_err(|e| raw::js_message(&e))?;
        let (pipeline, sampler) =
            built.map_err(|e| format!("pipeline construction threw: {}", raw::js_message(&e)))?;
        if !scope.is_null() && !scope.is_undefined() {
            return Err(format!("validation error: {}", raw::js_message(&scope)));
        }
        Ok(H264Blit {
            device: raw_device,
            queue: raw_queue,
            pipeline,
            sampler,
        })
    }

    fn build(device: &RawDevice) -> Result<(RawPipeline, JsValue), JsValue> {
        let sampler = device.create_sampler(&raw::obj(&[
            ("magFilter", "linear".into()),
            ("minFilter", "linear".into()),
            ("addressModeU", "clamp-to-edge".into()),
            ("addressModeV", "clamp-to-edge".into()),
        ]))?;
        let module = device.create_shader_module(&raw::obj(&[
            ("label", "h264_blit".into()),
            ("code", H264_WGSL.into()),
        ]))?;
        let pipeline = device.create_render_pipeline(&raw::obj(&[
            ("layout", "auto".into()),
            (
                "vertex",
                raw::obj(&[("module", module.clone()), ("entryPoint", "vs_main".into())]),
            ),
            (
                "fragment",
                raw::obj(&[
                    ("module", module),
                    ("entryPoint", "fs_main".into()),
                    (
                        "targets",
                        raw::array(&[raw::obj(&[("format", "rgba8unorm".into())])]),
                    ),
                ]),
            ),
            (
                "primitive",
                raw::obj(&[("topology", "triangle-list".into())]),
            ),
        ]))?;
        Ok((pipeline, sampler))
    }

    /// Draw every frame of `frames` full-screen into `framebuffer_view` (the
    /// raw `GPUTextureView` of the tile renderer's framebuffer; `loadOp: load`,
    /// so tiles stay where the picture does not cover them) in one pass and
    /// one submit. The frames must be open; closing them is the caller's.
    /// A frame that cannot be imported is skipped and named in the error;
    /// the others are still drawn.
    pub fn draw(
        &self,
        framebuffer_view: &JsValue,
        width: u32,
        height: u32,
        frames: &[JsValue],
    ) -> Result<(), String> {
        let encoder = self.device.create_command_encoder();
        let attachment = raw::obj(&[
            ("view", framebuffer_view.clone()),
            ("loadOp", "load".into()),
            ("storeOp", "store".into()),
        ]);
        let pass = encoder.begin_render_pass(&raw::obj(&[(
            "colorAttachments",
            raw::array(&[attachment]),
        )]));
        let mut failure = None;
        for frame in frames {
            let external = match self
                .device
                .import_external_texture(&raw::obj(&[("source", frame.clone())]))
            {
                Ok(e) => e,
                Err(e) => {
                    failure = Some(format!("importExternalTexture: {}", raw::js_message(&e)));
                    continue;
                }
            };
            let bind_group = self.device.create_bind_group(&raw::obj(&[
                ("layout", self.pipeline.get_bind_group_layout(0)),
                (
                    "entries",
                    raw::array(&[
                        raw::obj(&[("binding", raw::num(0)), ("resource", external)]),
                        raw::obj(&[
                            ("binding", raw::num(1)),
                            ("resource", self.sampler.clone()),
                        ]),
                    ]),
                ),
            ]));
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group);
            pass.set_viewport(0.0, 0.0, width as f32, height as f32, 0.0, 1.0);
            pass.draw(6);
        }
        pass.end();
        self.queue.submit(&Array::of1(&encoder.finish()));
        failure.map_or(Ok(()), Err)
    }
}
```

- [ ] **Step 3: Module list and compile.** Append to `src/web/mod.rs`:

```rust
#[cfg(target_arch = "wasm32")]
pub mod h264;
#[cfg(target_arch = "wasm32")]
pub mod raw;
```

```bash
cd ~/work/gf-web-renderer
cargo check -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown -- -D warnings 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
```

Expected `rc=0`. If `AsRef<JsValue>` is not implemented for one of the wgpu handle types, use `JsValue::from(handle.clone())` instead (every wasm-bindgen type converts); that is the only likely drift.

The behaviour of this escape is proven by Task 11's ordering test and by the e2e H.264 scenes; there is nothing to unit-test without a browser.

- [ ] **Step 4: Commit.**

```bash
cd ~/work/gf-web-renderer
git add ghostframe-client-wasm/src/web/raw.rs ghostframe-client-wasm/src/web/h264.rs ghostframe-client-wasm/src/web/mod.rs
git commit -m "feat(web): the raw WebGPU escape for H.264 pictures

One GPURenderPipeline from h264_blit.wgsl on wgpu's own device; per frame
importExternalTexture, a raw pass into the framebuffer view, a raw submit.
Declared with extern \"C\" so no unstable web-sys cfg is needed.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 9 [sonnet]: `WebRenderer`

The renderer the page owns: device, canvas surface, `TileRenderer`, the H.264 escape, the present blit and the debug reads. Test first, with the simplest end-to-end case; Task 11 adds the rest.

**Files:**
- Create: `ghostframe-client-wasm/src/web/present.rs`, `src/web/renderer.rs`, `src/web/debug.rs`, `ghostframe-client-wasm/tests/web_renderer.rs`
- Modify: `ghostframe-client-wasm/src/web/mod.rs`

- [ ] **Step 1: The failing browser test.** Create `ghostframe-client-wasm/tests/web_renderer.rs` (Task 11 extends it):

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! `WebRenderer` in a real browser with a real WebGPU device. Run with
//! `just test-web-renderer` (headless Chrome through wasm-pack).
#![cfg(target_arch = "wasm32")]

use ghostframe_client_core::{Event, TileData};
use ghostframe_client_wasm::web::renderer::WebRenderer;
use ghostframe_client_wasm::web::route::TileSink;
use js_sys::Uint8Array;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::JsFuture;
use wasm_bindgen_test::*;

wasm_bindgen_test_configure!(run_in_browser);

const RED: [u8; 4] = [0, 0, 255, 255]; // BGRA, as on the wire
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [255, 0, 0, 255];
const YELLOW: [u8; 4] = [0, 255, 255, 255];

fn rgba(bgra: [u8; 4]) -> [u8; 4] {
    [bgra[2], bgra[1], bgra[0], bgra[3]]
}

async fn new_renderer(cdf53: &str) -> WebRenderer {
    let document = web_sys::window().unwrap().document().unwrap();
    let canvas: web_sys::HtmlCanvasElement = document.create_element("canvas").unwrap().unchecked_into();
    document.body().unwrap().append_child(&canvas).unwrap();
    WebRenderer::create(canvas, cdf53.to_string())
        .await
        .unwrap_or_else(|e| panic!("WebRenderer::create: {e:?}"))
}

fn dims(w: u32, h: u32) -> Event {
    Event::FrameDimensions { width: w, height: h }
}

fn solid(frame_seq: u32, tile_x: u8, tile_y: u8, bgra: [u8; 4]) -> Event {
    Event::TilePayload {
        frame_seq,
        tile_x,
        tile_y,
        generation: 0,
        data: TileData::Solid(bgra),
    }
}

/// The framebuffer pixel at (x, y), RGBA.
async fn px(r: &WebRenderer, x: u32, y: u32) -> [u8; 4] {
    let bytes = JsFuture::from(r.read_pixel_rect(x, y, 1, 1).expect("read"))
        .await
        .expect("map");
    let v = Uint8Array::new(&bytes).to_vec();
    [v[0], v[1], v[2], v[3]]
}

fn near(got: [u8; 4], want_bgra: [u8; 4], tol: u8) -> bool {
    let want = rgba(want_bgra);
    got.iter().zip(want.iter()).all(|(g, w)| g.abs_diff(*w) <= tol)
}

#[wasm_bindgen_test]
async fn a_solid_tile_reaches_the_framebuffer() {
    let mut r = new_renderer("auto").await;
    assert_eq!(r.fb_width(), 0, "no size until the first FrameDimensions");
    r.apply(dims(64, 64)).expect("resize");
    assert_eq!((r.fb_width(), r.fb_height()), (64, 64));
    r.apply(solid(10, 1, 1, RED)).expect("tile");
    let out = r.paint_tick(1_000).expect("paint");
    assert!(out.presented, "a tile arrived: the tick must present");
    assert!(near(px(&r, 40, 40).await, RED, 0), "tile (1,1) is red");
    assert_eq!(px(&r, 8, 8).await, [0, 0, 0, 0], "an untouched tile stays clear");
}
```

(`r.apply` is `TileSink::apply`; the test file imports the trait. `WebRenderer`'s Rust-only methods used here: `create`, `fb_width`, `fb_height`, `paint_tick`, `read_pixel_rect`.)

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-client-wasm --target wasm32-unknown-unknown --no-run 2>&1 | tail -5
```

Expected now: a compile error, `unresolved import ghostframe_client_wasm::web::renderer`. (That is the failing state.)

- [ ] **Step 2: `src/web/present.rs`.**

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! The present blit: the framebuffer to the canvas's current texture, nearest
//! filtered, so a 1:1 canvas shows the framebuffer byte for byte.

const PRESENT_WGSL: &str = include_str!("../../../shaders/client/present_blit.wgsl");

pub struct Present {
    pipeline: wgpu::RenderPipeline,
    sampler: wgpu::Sampler,
    /// Bound to one framebuffer texture; `epoch` names which.
    bind: Option<(u64, wgpu::BindGroup)>,
}

impl Present {
    pub fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("ghostframe-present-blit"),
            source: wgpu::ShaderSource::Wgsl(PRESENT_WGSL.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("ghostframe-present-blit"),
            layout: None,
            vertex: wgpu::VertexState {
                module: &module,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &module,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target,
                    blend: None,
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("ghostframe-present-sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        Present {
            pipeline,
            sampler,
            bind: None,
        }
    }

    /// Record the blit of `framebuffer` (texture number `epoch`) into `target`.
    pub fn encode(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        framebuffer: &wgpu::TextureView,
        epoch: u64,
        target: &wgpu::TextureView,
    ) {
        if self.bind.as_ref().map(|(e, _)| *e) != Some(epoch) {
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("ghostframe-present-bind"),
                layout: &self.pipeline.get_bind_group_layout(0),
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: wgpu::BindingResource::TextureView(framebuffer),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            self.bind = Some((epoch, bind_group));
        }
        let Some((_, bind_group)) = self.bind.as_ref() else {
            return;
        };
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("ghostframe-present"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.0,
                        g: 0.0,
                        b: 0.0,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.draw(0..6, 0..1);
    }
}
```

- [ ] **Step 3: `src/web/debug.rs`** (the port of `__readGradientGolden`; needed by `e2e_readpixel_correctness`).

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! Port of `window.__readGradientGolden` (the old `diagnostics.ts`): write a
//! known gradient into the framebuffer with a compute shader, read 16 points
//! back, report which differ. It exists so `e2e_readpixel_correctness` can
//! trust the pixel readbacks before any pixel assertion is trusted. It
//! overwrites the framebuffer; test use only.

use ghostframe_render::readback::begin_texture_read;
use js_sys::Promise;
use serde::Serialize;
use wasm_bindgen::JsValue;
use wasm_bindgen_futures::future_to_promise;

const GRADIENT_WGSL: &str = include_str!("../../../shaders/client/debug_gradient.wgsl");

#[derive(Serialize)]
struct Mismatch {
    x: u32,
    y: u32,
    got: Vec<u8>,
    want: Vec<u8>,
}

#[derive(Serialize)]
struct Golden {
    ok: bool,
    mismatches: Vec<Mismatch>,
}

fn sample_points(w: u32, h: u32) -> Vec<(u32, u32)> {
    [
        (0, 0),
        (1, 0),
        (255, 0),
        (256, 0),
        (0, 1),
        (0, 255),
        (0, 256),
        (100, 200),
        (16, 16),
        (31, 31),
        (32, 32),
        (63, 63),
        (w / 2, h / 2),
        (w - 1, h - 1),
        (255.min(w - 1), 255.min(h - 1)),
        (256.min(w - 1), 256.min(h - 1)),
    ]
    .into_iter()
    .filter(|&(x, y)| x < w && y < h)
    .collect()
}

/// Dispatch the gradient into `framebuffer` now; the returned promise
/// resolves to `{ ok, mismatches: [{x, y, got, want}] }`.
pub fn read_gradient_golden(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    framebuffer: &wgpu::Texture,
) -> Promise {
    let (w, h) = (framebuffer.width(), framebuffer.height());
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("debug_gradient"),
        source: wgpu::ShaderSource::Wgsl(GRADIENT_WGSL.into()),
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("debug_gradient"),
        layout: None,
        module: &module,
        entry_point: Some("debug_gradient"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let view = framebuffer.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("debug_gradient"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: wgpu::BindingResource::TextureView(&view),
        }],
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("debug_gradient"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
            label: Some("debug_gradient"),
            timestamp_writes: None,
        });
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.dispatch_workgroups(w.div_ceil(8), h.div_ceil(8), 1);
    }
    queue.submit(std::iter::once(encoder.finish()));

    // Every readback copy is submitted now (ordered after the dispatch); the
    // awaits only collect the bytes, so nothing here borrows the renderer.
    let reads: Vec<_> = sample_points(w, h)
        .into_iter()
        .map(|(x, y)| (x, y, begin_texture_read(device, queue, framebuffer, x, y, 1, 1)))
        .collect();
    future_to_promise(async move {
        let mut mismatches = Vec::new();
        for (x, y, read) in reads {
            let got = read
                .finish()
                .await
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            let want = vec![(x & 0xFF) as u8, (y & 0xFF) as u8, 0, 255];
            if got != want {
                mismatches.push(Mismatch { x, y, got, want });
            }
        }
        let report = Golden {
            ok: mismatches.is_empty(),
            mismatches,
        };
        serde_wasm_bindgen::to_value(&report).map_err(|e| JsValue::from_str(&e.to_string()))
    })
}
```

- [ ] **Step 4: `src/web/renderer.rs`.** This is the long one; it is written as one file. Read the wgpu 30.0.1 sources in `~/.cargo/registry/src/*/wgpu-30.0.1/src/api/` when a signature differs; the shape of every call is the native client's (`ghostframe-client-gpu/src/pipelines/solid.rs` and `wgpu_ctx.rs`).

```rust
// SPDX-License-Identifier: MIT OR Apache-2.0
//! `WebRenderer`: the page's renderer (spec 5.2).
//!
//! Owns the wgpu device on the browser's WebGPU backend, the canvas surface,
//! a `TileRenderer` (the native client's renderer), the H.264 escape and the
//! present blit. `paint_tick` is the rAF body: flush tiles, draw pending
//! pictures in ledger order, present; it does none of that when nothing
//! changed.

use std::sync::{Arc, Mutex};

use ghostframe_client_core::Event;
use ghostframe_render::TileRenderer;
use js_sys::{Array, Promise, Reflect, Uint8Array};
use serde::Serialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{future_to_promise, JsFuture};

use super::h264::H264Blit;
use super::logic::{pick_canvas_format, should_skip_paint, Cdf53Mode, PaintInputs};
use super::present::Present;
use super::raw;
use super::route::TileSink;

/// The tile shaders write `rgba8unorm` storage textures, so the framebuffer
/// cannot follow the canvas's preferred format.
const FRAMEBUFFER_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;
/// `requestAdapter` retries while Chrome's GPU process starts: 10 x 1 s, as
/// the TypeScript did.
const ADAPTER_RETRIES: u32 = 10;
/// Paint-cost samples kept (microseconds per non-skipped tick).
const PAINT_US_CAP: usize = 65_536;

fn now_ms() -> f64 {
    web_sys::window()
        .and_then(|w| w.performance())
        .map_or(0.0, |p| p.now())
}

fn console_info(msg: &str) {
    web_sys::console::info_1(&JsValue::from_str(msg));
}

fn console_warn(msg: &str) {
    web_sys::console::warn_1(&JsValue::from_str(msg));
}

async fn sleep_ms(ms: i32) {
    let promise = Promise::new(&mut |resolve, _reject| {
        if let Some(w) = web_sys::window() {
            let _ = w.set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
        }
    });
    let _ = JsFuture::from(promise).await;
}

fn has_navigator_gpu() -> bool {
    web_sys::window().is_some_and(|w| {
        Reflect::get(&w.navigator(), &JsValue::from_str("gpu"))
            .is_ok_and(|g| !g.is_undefined() && !g.is_null())
    })
}

/// A picture waiting for the next tick: the JS `VideoFrame` and the
/// `frame_seq` its access unit carried.
struct PendingPicture {
    frame: JsValue,
    seq: u32,
}

/// What a tick did.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PaintOutcome {
    /// A frame was presented (the only thing the page counts as painted).
    pub presented: bool,
    pub pictures_drawn: u32,
    pub pictures_dropped: u32,
}

#[wasm_bindgen]
pub struct WebRenderer {
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    canvas: web_sys::HtmlCanvasElement,
    surface_format: wgpu::TextureFormat,
    present: Present,
    tiles: TileRenderer,
    fb_view: Option<wgpu::TextureView>,
    /// Names the framebuffer texture; bumped whenever a resize replaces it.
    fb_epoch: u64,
    h264: Option<H264Blit>,
    pending_h264: Vec<PendingPicture>,
    h264_dropped: u32,
    /// `None` until the first accepted `FrameDimensions`.
    size: Option<(u32, u32)>,
    dirty: bool,
    paint_ticks: u32,
    submits: u32,
    /// Uncaptured GPU errors (pushed from the device's callback).
    gpu_errors: Arc<Mutex<Vec<String>>>,
    /// Errors from this struct's own steps, drained by `takeErrors`.
    errors: Vec<String>,
    /// `[codec, tile_x, tile_y, code]` quadruples awaiting `takeDecodeErrors`.
    decode_errors: Vec<u32>,
    paint_us: Vec<u32>,
}

impl WebRenderer {
    async fn create_inner(canvas: web_sys::HtmlCanvasElement, mode: Cdf53Mode) -> Result<Self, String> {
        // Design D2: WebGPU is required; its absence is fatal and named, so
        // main.ts can tell the user instead of showing a blank page.
        if !has_navigator_gpu() {
            return Err("WEBGPU_UNAVAILABLE: navigator.gpu is undefined".into());
        }
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| format!("WEBGPU_UNAVAILABLE: canvas.getContext('webgpu'): {e}"))?;
        let mut adapter = None;
        for attempt in 0..=ADAPTER_RETRIES {
            let found = instance
                .request_adapter(&wgpu::RequestAdapterOptions {
                    power_preference: wgpu::PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    ..Default::default()
                })
                .await;
            match found {
                Ok(a) => {
                    adapter = Some(a);
                    break;
                }
                Err(e) if attempt == ADAPTER_RETRIES => {
                    return Err(format!("WEBGPU_UNAVAILABLE: requestAdapter: {e}"));
                }
                // Chrome's GPU process starts asynchronously; the first call
                // can come back empty before its Vulkan backend is ready.
                Err(_) => sleep_ms(1000).await,
            }
        }
        let adapter = adapter.ok_or("WEBGPU_UNAVAILABLE: no adapter")?;

        let limits = adapter.limits();
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("ghostframe-web"),
                // 8 is the WebGPU default; stated because the CDF 5/3 grid and
                // the PalRLE pipeline bind that many. The storage-buffer sizes
                // are raised to the adapter's so a 4K frame's plane store fits.
                required_limits: wgpu::Limits {
                    max_storage_buffers_per_shader_stage: 8,
                    max_storage_buffer_binding_size: limits.max_storage_buffer_binding_size,
                    max_buffer_size: limits.max_buffer_size,
                    ..wgpu::Limits::default()
                },
                ..Default::default()
            })
            .await
            .map_err(|e| format!("WEBGPU_UNAVAILABLE: requestDevice: {e}"))?;

        let gpu_errors: Arc<Mutex<Vec<String>>> = Arc::default();
        {
            let sink = gpu_errors.clone();
            device.on_uncaptured_error(Arc::new(move |e: wgpu::Error| {
                let msg = e.to_string();
                web_sys::console::error_1(&JsValue::from_str(&format!("[webgpu] {msg}")));
                if let Ok(mut v) = sink.lock() {
                    if v.len() < 64 {
                        v.push(msg);
                    }
                }
            }));
        }

        let caps = surface.get_capabilities(&adapter);
        let surface_format = pick_canvas_format(&caps.formats);
        let present = Present::new(&device, surface_format);

        let mut tiles = TileRenderer::new(&device, &queue, FRAMEBUFFER_FORMAT, 32, 32)
            .map_err(|e| format!("TileRenderer::new: {e}"))?;
        tiles.set_collect_decode_errors(true);

        // The CDF 5/3 grid, if this browser's WebGPU draws it correctly:
        // `try_enable_cdf53_gpu` runs the self-test (two known tiles, read
        // back and compared with the CPU decoder) and enables the grid only
        // if it passes; on `Err` passes decode on the CPU. It uses the
        // default cost model and tile cap (no calibration on the web: R3).
        match mode {
            Cdf53Mode::Cpu => console_info("CDF 5/3 decode on the CPU (?cdf53=cpu)"),
            Cdf53Mode::Auto => match tiles.try_enable_cdf53_gpu(&device, &queue).await {
                Ok(()) => console_info("CDF 5/3 decode on the GPU (self-test passed)"),
                Err(e) => console_warn(&format!("CDF 5/3 self-test failed; decoding on the CPU: {e}")),
            },
        }

        let h264 = match H264Blit::probe(&device, &queue).await {
            Ok(h) => {
                console_info("[renderer] H264 pipeline available");
                Some(h)
            }
            Err(e) => {
                console_warn(&format!(
                    "[renderer] H264 pipeline unsupported on this browser; H.264 frames will be dropped. Reason: {e}"
                ));
                None
            }
        };

        Ok(WebRenderer {
            adapter,
            device,
            queue,
            surface,
            canvas,
            surface_format,
            present,
            tiles,
            fb_view: None,
            fb_epoch: 0,
            h264,
            pending_h264: Vec::new(),
            h264_dropped: 0,
            size: None,
            dirty: false,
            paint_ticks: 0,
            submits: 0,
            gpu_errors,
            errors: Vec::new(),
            decode_errors: Vec::new(),
            paint_us: Vec::new(),
        })
    }

    /// Where CDF 5/3 tiles are reconstructed, from what the tile renderer
    /// actually enabled (not from what was asked for).
    fn cdf53_label(&self) -> &'static str {
        if self.tiles.cdf53_gpu_enabled() {
            "gpu"
        } else {
            "cpu"
        }
    }

    fn configure_surface(&mut self) -> Result<(), String> {
        let (w, h) = self.size.ok_or("framebuffer not sized yet")?;
        let mut config = self
            .surface
            .get_default_config(&self.adapter, w, h)
            .ok_or("the canvas surface is not supported by this adapter")?;
        config.format = self.surface_format;
        config.usage = wgpu::TextureUsages::RENDER_ATTACHMENT;
        config.alpha_mode = wgpu::CompositeAlphaMode::Opaque;
        self.surface.configure(&self.device, &config);
        Ok(())
    }

    /// Resize the canvas, its surface and the framebuffer. A new picture: the
    /// tile renderer resets its paint ledger and keeps what fits.
    fn resize(&mut self, width: u32, height: u32) -> Result<(), String> {
        self.tiles
            .apply_event(&self.device, &self.queue, Event::FrameDimensions { width, height })
            .map_err(|e| e.to_string())?;
        if self.canvas.width() != width || self.canvas.height() != height {
            // Changing the canvas size invalidates the surface; configure again.
            self.canvas.set_width(width);
            self.canvas.set_height(height);
        }
        self.size = Some((width, height));
        self.configure_surface()?;
        self.fb_view = Some(
            self.tiles
                .framebuffer()
                .create_view(&wgpu::TextureViewDescriptor::default()),
        );
        self.fb_epoch += 1;
        self.dirty = true;
        Ok(())
    }

    /// Run one rAF's worth of painting. `Err` is only a present failure (the
    /// canvas could not give a texture); the tick is retried next time.
    pub fn paint_tick(&mut self, now_us: u64) -> Result<PaintOutcome, String> {
        let mut outcome = PaintOutcome::default();
        if self.size.is_none() {
            // Pictures pushed before the first FrameDimensions have nothing to
            // be drawn on: close them rather than hold decoder memory.
            for p in self.pending_h264.drain(..) {
                raw::close_frame(&p.frame);
                self.h264_dropped = self.h264_dropped.wrapping_add(1);
            }
            return Ok(outcome);
        }
        self.collect_decode_errors();
        let inputs = PaintInputs {
            dirty: self.dirty,
            h264_pending: self.pending_h264.len(),
            deferred_work: self.tiles.has_deferred_work(),
            errors_readback_pending: self.tiles.errors_readback_pending(),
        };
        if should_skip_paint(&inputs) {
            return Ok(outcome);
        }
        let started = now_ms();
        self.paint_ticks = self.paint_ticks.wrapping_add(1);
        let mut changed = self.dirty;

        // 1. Tiles. Everything the tile renderer submits is on the queue
        //    before the picture pass below is.
        //    When a picture that is not stale will be drawn this tick, the CDF
        //    5/3 tiles under it are held (as the native client does): the
        //    picture covers the screen, so drawing them first would only spend
        //    the flush's GPU budget, and `note_h264` below cancels them.
        //    A CDF 5/3 pass newer than the picture would make it stale, so
        //    nothing newer is ever held.
        let will_draw = self.pending_h264.iter().any(|p| !self.tiles.h264_stale(p.seq));
        self.tiles.hold_cdf53(will_draw);
        let flushed = self.tiles.flush(&self.device, &self.queue, now_us);
        self.tiles.hold_cdf53(false);
        match flushed {
            Ok(report) => changed |= !report.damage.is_empty(),
            Err(e) => self.errors.push(format!("flush: {e}")),
        }
        // 2. Pictures, in ledger order, over what the flush drew.
        let (drawn, dropped) = self.draw_pending_pictures();
        outcome.pictures_drawn = drawn;
        outcome.pictures_dropped = dropped;
        // `note_h264` does not mark damage (and `mark_all_dirty` after a
        // picture would make the next flush report the whole screen and
        // present again for nothing), so a drawn picture is what says changed.
        changed |= drawn > 0;
        // 3. Present, only if the framebuffer changed.
        self.dirty = false;
        if changed {
            if let Err(e) = self.present_frame() {
                self.dirty = true; // retry next tick
                return Err(e);
            }
            outcome.presented = true;
            self.submits = self.submits.wrapping_add(1);
        }
        self.note_paint_us(((now_ms() - started) * 1000.0) as u32);
        Ok(outcome)
    }

    fn draw_pending_pictures(&mut self) -> (u32, u32) {
        if self.pending_h264.is_empty() {
            return (0, 0);
        }
        let pending = std::mem::take(&mut self.pending_h264);
        let mut kept: Vec<JsValue> = Vec::new();
        let mut dropped = 0u32;
        for p in &pending {
            // Older than a picture already drawn, or than any tile painted or
            // queued: it covers the whole screen, so drawing it would undo a
            // newer paint. It is closed with the rest, not drawn.
            if self.tiles.h264_stale(p.seq) {
                dropped += 1;
                continue;
            }
            // Noted before the draw is encoded so a second, older picture of
            // this tick is judged against it; also cancels older CDF 5/3 work.
            self.tiles.note_h264(p.seq);
            kept.push(p.frame.clone());
        }
        if let (Some(h264), Some((w, h)), false) = (self.h264.as_ref(), self.size, kept.is_empty()) {
            let view = self
                .tiles
                .framebuffer()
                .create_view(&wgpu::TextureViewDescriptor::default());
            match raw::raw_view(&view) {
                Ok(raw_view) => {
                    if let Err(e) = h264.draw(&raw_view, w, h, &kept) {
                        self.errors.push(format!("h264: {e}"));
                    }
                }
                Err(e) => self.errors.push(format!("h264: {e}")),
            }
        }
        self.h264_dropped = self.h264_dropped.wrapping_add(dropped);
        for p in &pending {
            raw::close_frame(&p.frame);
        }
        (kept.len() as u32, dropped)
    }

    #[allow(
        clippy::wildcard_enum_match_arm,
        reason = "wgpu::CurrentSurfaceTexture is a foreign enum; every other case means: try again next tick"
    )]
    fn present_frame(&mut self) -> Result<(), String> {
        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                self.configure_surface()?;
                return Err("canvas surface was outdated; reconfigured".into());
            }
            other => return Err(format!("get_current_texture: {other:?}")),
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let Some(fb_view) = self.fb_view.as_ref() else {
            return Err("no framebuffer view".into());
        };
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("ghostframe-present"),
            });
        self.present
            .encode(&self.device, &mut encoder, fb_view, self.fb_epoch, &target);
        self.queue.submit(std::iter::once(encoder.finish()));
        self.queue.present(frame);
        Ok(())
    }

    fn collect_decode_errors(&mut self) {
        for e in self.tiles.take_decode_errors() {
            self.decode_errors.extend([
                u32::from(e.codec as u8),
                u32::from(e.tile_x),
                u32::from(e.tile_y),
                u32::from(e.code as u8),
            ]);
        }
    }

    fn note_paint_us(&mut self, us: u32) {
        self.paint_us.push(us);
        if self.paint_us.len() >= PAINT_US_CAP {
            self.paint_us.drain(..PAINT_US_CAP / 2);
        }
    }

    /// Drop pictures and forget what the old session painted.
    fn reset_session(&mut self) {
        for p in self.pending_h264.drain(..) {
            raw::close_frame(&p.frame);
        }
        if let Err(e) = self.tiles.reset_session(&self.device, &self.queue) {
            self.errors.push(format!("session reset: {e}"));
        }
        self.dirty = true;
    }
}

impl TileSink for WebRenderer {
    fn apply(&mut self, ev: Event) -> Result<(), String> {
        if let Event::FrameDimensions { width, height } = ev {
            return self.resize(width, height);
        }
        self.dirty = true;
        self.tiles
            .apply_event(&self.device, &self.queue, ev)
            .map_err(|e| e.to_string())
    }
}

/// `debugState()`'s shape: the old renderer's keys (the page log and the
/// failure dumps print them) with the tile renderer's own state spliced in.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct WebState<'a, T: Serialize> {
    submit_count: u32,
    h264_queue: usize,
    framebuffer_dirty: bool,
    fb_width: u32,
    fb_height: u32,
    has_texture: bool,
    canvas_width: u32,
    canvas_height: u32,
    paint_tick: u32,
    /// Kept under its old key; true while a readback is in flight or untaken.
    errors_readback_in_flight: bool,
    /// The old `solidQueue` / `palRleQueue` keys, from the tile renderer's pending counts.
    solid_queue: usize,
    pal_rle_queue: usize,
    cdf53_path: &'a str,
    h264_supported: bool,
    h264_dropped: u32,
    #[serde(flatten)]
    tiles: T,
}

fn to_js<T: Serialize>(v: &T) -> Result<JsValue, JsValue> {
    v.serialize(&serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true))
        .map_err(|e| JsValue::from_str(&e.to_string()))
}

#[wasm_bindgen]
impl WebRenderer {
    /// Open the page's renderer on `canvas`. `cdf53` is the page URL's
    /// `cdf53` parameter: `""`/`"auto"` or `"cpu"` (see `Cdf53Mode`).
    /// Rejects with a string starting `WEBGPU_UNAVAILABLE` when the browser has
    /// no usable WebGPU.
    #[wasm_bindgen(js_name = create)]
    pub async fn create(canvas: web_sys::HtmlCanvasElement, cdf53: String) -> Result<WebRenderer, JsValue> {
        Self::create_inner(canvas, Cdf53Mode::parse(&cdf53))
            .await
            .map_err(|e| JsValue::from_str(&e))
    }

    #[wasm_bindgen(getter, js_name = h264Supported)]
    pub fn h264_supported(&self) -> bool {
        self.h264.is_some()
    }

    #[wasm_bindgen(getter, js_name = fbWidth)]
    pub fn fb_width(&self) -> u32 {
        self.size.map_or(0, |s| s.0)
    }

    #[wasm_bindgen(getter, js_name = fbHeight)]
    pub fn fb_height(&self) -> u32 {
        self.size.map_or(0, |s| s.1)
    }

    /// Frames presented so far (an idle tick adds none).
    #[wasm_bindgen(getter, js_name = submitCount)]
    pub fn submit_count(&self) -> u32 {
        self.submits
    }

    #[wasm_bindgen(getter, js_name = paintTickCount)]
    pub fn paint_tick_count(&self) -> u32 {
        self.paint_ticks
    }

    /// `"gpu"` or `"cpu"`: where CDF 5/3 tiles are reconstructed.
    #[wasm_bindgen(getter, js_name = cdf53Path)]
    pub fn cdf53_path(&self) -> String {
        self.cdf53_label().to_string()
    }

    /// Hand over a decoded `VideoFrame` (ownership: this renderer closes it)
    /// with the `frame_seq` of its access unit.
    #[wasm_bindgen(js_name = pushH264)]
    pub fn push_h264(&mut self, frame: JsValue, frame_seq: u32) {
        if self.h264.is_none() {
            raw::close_frame(&frame);
            self.h264_dropped = self.h264_dropped.wrapping_add(1);
            return;
        }
        // Not `dirty`: a picture the ledger drops changes nothing, so only a
        // drawn one makes the tick present (`paint_tick`).
        self.pending_h264.push(PendingPicture { frame, seq: frame_seq });
    }

    /// One rAF tick; the number of frames presented (0 or 1). Errors are kept
    /// for `takeErrors`.
    #[wasm_bindgen(js_name = paintTick)]
    pub fn paint_tick_js(&mut self, now_us: u64) -> u32 {
        // A u64 crosses as a BigInt, exactly what `nowUs()` in main.ts returns
        // and what `onTimeoutInto` takes: one clock type for every call.
        match self.paint_tick(now_us) {
            Ok(o) => u32::from(o.presented),
            Err(e) => {
                self.errors.push(e);
                0
            }
        }
    }

    /// The transport closed: drop pending pictures and forget the session's
    /// paint state. Call before closing the JS decoder.
    #[wasm_bindgen(js_name = onSessionReset)]
    pub fn on_session_reset(&mut self) {
        self.reset_session();
    }

    /// Errors since the last call: uncaptured GPU errors and failed steps.
    #[wasm_bindgen(js_name = takeErrors)]
    pub fn take_errors(&mut self) -> Array {
        let mut out = std::mem::take(&mut self.errors);
        if let Ok(mut g) = self.gpu_errors.lock() {
            out.append(&mut g);
        }
        out.iter().map(|s| JsValue::from_str(s)).collect()
    }

    /// `[codec, tile_x, tile_y, code, ...]` for tiles the GPU decode rejected.
    #[wasm_bindgen(js_name = takeDecodeErrors)]
    pub fn take_decode_errors(&mut self) -> Vec<u32> {
        self.collect_decode_errors();
        std::mem::take(&mut self.decode_errors)
    }

    /// `[count, p50, p99, max]` microseconds of the non-skipped ticks since the
    /// last call (the section 7 bench reads it through `__ghostframePaintUs`).
    #[wasm_bindgen(js_name = paintUsStats)]
    pub fn paint_us_stats(&mut self) -> Vec<f64> {
        let mut v = std::mem::take(&mut self.paint_us);
        v.sort_unstable();
        let at = |q: f64| v.get(((v.len() as f64 - 1.0) * q).round() as usize).copied().map_or(0.0, f64::from);
        vec![v.len() as f64, at(0.5), at(0.99), v.last().copied().map_or(0.0, f64::from)]
    }

    /// The old `__ghostframeRendererState()`.
    #[wasm_bindgen(js_name = debugState)]
    pub fn debug_state(&self) -> Result<JsValue, JsValue> {
        let tile_state = self.tiles.debug_state();
        to_js(&WebState {
            submit_count: self.submits,
            h264_queue: self.pending_h264.len(),
            framebuffer_dirty: self.dirty,
            fb_width: self.fb_width(),
            fb_height: self.fb_height(),
            has_texture: self.size.is_some(),
            canvas_width: self.canvas.width(),
            canvas_height: self.canvas.height(),
            paint_tick: self.paint_ticks,
            errors_readback_in_flight: self.tiles.errors_readback_pending(),
            solid_queue: tile_state.pending_solid,
            pal_rle_queue: tile_state.pending_palrle,
            cdf53_path: self.cdf53_label(),
            h264_supported: self.h264.is_some(),
            h264_dropped: self.h264_dropped,
            tiles: tile_state,
        })
    }

    /// The old `__ghostframeTile(x, y)`: what the renderer last did to a tile,
    /// or `null` for a tile off the grid.
    #[wasm_bindgen(js_name = debugTile)]
    pub fn debug_tile(&self, tile_x: u32, tile_y: u32) -> Result<JsValue, JsValue> {
        match self.tiles.debug_tile(tile_x, tile_y) {
            Some(t) => to_js(&t),
            None => Ok(JsValue::NULL),
        }
    }

    /// `{ device, texture }`: the raw `GPUDevice` and the framebuffer's
    /// `GPUTexture`, for `window.__ghostframeRenderer` (the e2e suite reads
    /// `texture.width` and `texture.height`); `undefined` before the first size.
    #[wasm_bindgen(js_name = debugHandles)]
    pub fn debug_handles(&self) -> JsValue {
        if self.size.is_none() {
            return JsValue::UNDEFINED;
        }
        let device = self.device.as_webgpu().map(|d| AsRef::<JsValue>::as_ref(d).clone());
        let texture = self
            .tiles
            .framebuffer()
            .as_webgpu()
            .map(|t| AsRef::<JsValue>::as_ref(t).clone());
        match (device, texture) {
            (Some(device), Some(texture)) => raw::obj(&[("device", device), ("texture", texture)]),
            _ => JsValue::UNDEFINED,
        }
    }

    /// The `w` x `h` rectangle at (`x`, `y`) of the framebuffer as RGBA bytes
    /// (a `Promise<Uint8Array>`). The copy is submitted now, after everything
    /// already submitted; the promise only waits for the bytes.
    #[wasm_bindgen(js_name = readPixelRect)]
    pub fn read_pixel_rect(&self, x: u32, y: u32, w: u32, h: u32) -> Result<Promise, JsValue> {
        let (fw, fh) = self
            .size
            .ok_or_else(|| JsValue::from_str("framebuffer not sized yet"))?;
        if w == 0 || h == 0 {
            return Ok(Promise::resolve(&Uint8Array::new_with_length(0)));
        }
        if x.checked_add(w).is_none_or(|e| e > fw) || y.checked_add(h).is_none_or(|e| e > fh) {
            return Err(JsValue::from_str(&format!(
                "rect {w}x{h} at ({x},{y}) is outside the {fw}x{fh} framebuffer"
            )));
        }
        let pending = ghostframe_render::readback::begin_texture_read(
            &self.device,
            &self.queue,
            self.tiles.framebuffer(),
            x,
            y,
            w,
            h,
        );
        Ok(future_to_promise(async move {
            let bytes = pending
                .finish()
                .await
                .map_err(|e| JsValue::from_str(&e.to_string()))?;
            Ok(Uint8Array::from(bytes.as_slice()).into())
        }))
    }

    /// The old `__readGradientGolden()`; overwrites the framebuffer.
    #[wasm_bindgen(js_name = readGradientGolden)]
    pub fn read_gradient_golden(&self) -> Result<Promise, JsValue> {
        if self.size.is_none() {
            return Err(JsValue::from_str("framebuffer not sized yet"));
        }
        Ok(super::debug::read_gradient_golden(
            &self.device,
            &self.queue,
            self.tiles.framebuffer(),
        ))
    }
}
```

`WebRenderer::apply` is reached through `TileSink` (the test imports the trait). If a wgpu name differs, the likely candidates are: `wgpu::Error` for the uncaptured handler's argument (`wgpu::Error` is `Display`; if the handler type is `Fn(Error)` it may be `wgpu::Error` by value), `Surface::get_default_config` returning `Option`, `Queue::present`, `Adapter::limits`.

- [ ] **Step 5: Module list; compile; run the first test.**

```bash
cd ~/work/gf-web-renderer
cat >> ghostframe-client-wasm/src/web/mod.rs <<'EOF'
#[cfg(target_arch = "wasm32")]
pub mod debug;
#[cfg(target_arch = "wasm32")]
pub mod present;
#[cfg(target_arch = "wasm32")]
pub mod renderer;
EOF
cargo check -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets 2>&1 | tail -30; echo "rc=${PIPESTATUS[0]}"
wasm-pack test --headless --chrome ghostframe-client-wasm --test web_renderer 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

(Use the browser invocation Task 1 settled on.) Expected: `test a_solid_tile_reaches_the_framebuffer ... ok`, `rc=0`. Everything in the framebuffer test is real: `px(&r, 8, 8) == [0,0,0,0]` proves the readback does not just return whatever it was handed.

- [ ] **Step 6: Break it.** In `paint_tick`, replace the `self.tiles.flush(...)` call with a no-op: the test fails at `px(&r, 40, 40)` (the tile was never drawn). In `read_pixel_rect`, change `x, y` in the `begin_texture_read` call to `0, 0`: the red assertion fails (tile (1,1) is read at the origin). Revert both.

- [ ] **Step 7: Lint and commit.**

```bash
cd ~/work/gf-web-renderer
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --all-targets -- -D warnings 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
git add ghostframe-client-wasm/src/web ghostframe-client-wasm/tests/web_renderer.rs
git commit -m "feat(web): WebRenderer over the shared TileRenderer

Device, canvas surface, CDF 5/3 grid with self-test and CPU fallback, the
H.264 escape, the present blit, idle skip, and the debug reads the e2e suite
uses (state, tile, handles, pixel rectangles, gradient golden).

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 10 [sonnet]: `WasmClientCore` hands tile events to the renderer inside wasm

**Files:**
- Modify: `ghostframe-client-wasm/src/core.rs`

`handleDatagrams` and `onTimeout` stay exactly as they are (vitest drives them). The two new methods exist on wasm32 only, because they name `WebRenderer`.

- [ ] **Step 1: Share the datagram loop.** In `core.rs`, move the body of `handle_datagrams` between `let mut core = self.inner.borrow_mut();` and `drop(core);` into a private method of a plain `impl WasmClientCore` block (the one that holds `shared()`):

```rust
    /// Feed a batch of datagrams to the core; the events, in arrival order.
    /// The loop `handleDatagrams` always ran (see its doc for the batch format).
    fn collect_datagram_events(
        &mut self,
        concat: &[u8],
        lens: &[u32],
        arrivals_us: &[f64],
    ) -> Vec<ghostframe_client_core::Event> {
        let mut core = self.inner.borrow_mut();
        let mut events = Vec::new();
        let mut offset = 0usize;
        for (len, at) in lens.iter().zip(arrivals_us.iter()) {
            let len = *len as usize;
            let Some(end) = offset.checked_add(len) else {
                break;
            };
            let Some(datagram) = concat.get(offset..end) else {
                break;
            };
            offset = end;
            // `as u64` saturates and maps NaN to 0, so a hostile or buggy
            // caller cannot wrap.
            events.extend(core.handle_datagram(datagram, *at as u64));
        }
        events
    }
```

and make `handle_datagrams` call it:

```rust
    pub fn handle_datagrams(
        &mut self,
        concat: &[u8],
        lens: &[u32],
        arrivals_us: &[f64],
    ) -> Result<JsValue, JsValue> {
        let events = self.collect_datagram_events(concat, lens, arrivals_us);
        let mirrored: Vec<WasmEvent> = events
            .into_iter()
            .filter_map(WasmEvent::from_event)
            .collect();
        to_js(&mirrored)
    }
```

- [ ] **Step 2: The two new exports.** Add a second `#[wasm_bindgen] impl` block (a type may have several):

```rust
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
impl WasmClientCore {
    /// [`WasmClientCore::handle_datagrams`] with the tile work applied to
    /// `renderer` inside wasm: tile payloads, palettes and frame sizes never
    /// cross into JS. Returns one record per event, in order (see
    /// `web::route`): a slim record per tile (codec, coordinates, full
    /// length, first 16 bytes), `FrameDimensions` with `applied`/`reason`, and
    /// `NeedsH264`/`DecodeError`/`Evicted` as before. A failing event is
    /// carried in its record's `error`; the rest of the batch is still applied.
    #[wasm_bindgen(js_name = handleDatagramsInto)]
    pub fn handle_datagrams_into(
        &mut self,
        renderer: &mut crate::web::renderer::WebRenderer,
        concat: &[u8],
        lens: &[u32],
        arrivals_us: &[f64],
    ) -> Result<JsValue, JsValue> {
        let events = self.collect_datagram_events(concat, lens, arrivals_us);
        to_js(&crate::web::route::route(events, renderer))
    }

    /// [`WasmClientCore::on_timeout`] with the same routing.
    #[wasm_bindgen(js_name = onTimeoutInto)]
    pub fn on_timeout_into(
        &mut self,
        renderer: &mut crate::web::renderer::WebRenderer,
        now_us: u64,
    ) -> Result<JsValue, JsValue> {
        let events = self.inner.borrow_mut().on_timeout(now_us);
        to_js(&crate::web::route::route(events, renderer))
    }
}
```

- [ ] **Step 3: Compile and run the existing suites** (nothing may change for the old methods).

```bash
cd ~/work/gf-web-renderer
cargo check -p ghostframe-client-wasm --target wasm32-unknown-unknown 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
cargo test -p ghostframe-client-wasm --lib 2>&1 | tail -6; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
```

Expected all `rc=0`. The vitest suite is run in Task 17 (`npm test`), where `tile_assembly`, `eviction` and `datagram_batch` exercise `handleDatagrams` through the refactored loop.

- [ ] **Step 4: A browser test of the route into a real renderer.** Append to `tests/web_renderer.rs` (the datagram builder is the protocol crate's own `fragment_tile`, the same one `tests/helpers/wasm.ts::fragmentTile` mirrors, so the bytes are what the server sends):

```rust
/// A real datagram stream: a core, a renderer, and a Solid tile datagram built
/// by the protocol crate. Proves `handleDatagramsInto` applies the tile inside
/// wasm and returns a slim record, not the payload.
#[wasm_bindgen_test]
async fn datagrams_reach_the_renderer_without_their_bytes_reaching_js() {
    use ghostframe_client_wasm::core::WasmClientCore;
    use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

    let datagrams = fragment_tile(
        &TileFragmentInputs {
            frame_seq: 5 | TILE_DATAGRAM_FLAG,
            tile_x: 1,
            tile_y: 1,
            codec: Codec::Solid,
            generation: 1,
            pass: 0,
            timestamp_us: 0,
        },
        &RED, // a Solid payload is one BGRA quad
        1200,
    );
    let mut core = WasmClientCore::new(true, false, true, 0);
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    let concat: Vec<u8> = datagrams.iter().flatten().copied().collect();
    let lens: Vec<u32> = datagrams.iter().map(|d| d.len() as u32).collect();
    let arrivals = vec![1000.0; datagrams.len()];
    let out = core
        .handle_datagrams_into(&mut r, &concat, &lens, &arrivals)
        .expect("route");
    let out = js_sys::Array::from(&out);
    assert_eq!(out.length(), 1, "one tile, one record");
    let rec = out.get(0);
    let get = |k: &str| js_sys::Reflect::get(&rec, &k.into()).unwrap();
    assert_eq!(get("kind"), "TilePayload");
    assert_eq!(get("tile_x").as_f64(), Some(1.0));
    let data = js_sys::Reflect::get(&rec, &"data".into()).unwrap();
    assert_eq!(js_sys::Reflect::get(&data, &"codec".into()).unwrap(), "Solid");
    assert_eq!(js_sys::Reflect::get(&data, &"len".into()).unwrap().as_f64(), Some(4.0));
    r.paint_tick(2_000).unwrap();
    assert!(near(px(&r, 40, 40).await, RED, 0), "the tile was applied inside wasm");
}
```

```bash
cd ~/work/gf-web-renderer
wasm-pack test --headless --chrome ghostframe-client-wasm --test web_renderer 2>&1 | tail -10; echo "rc=${PIPESTATUS[0]}"
```

(`WasmClientCore` and its `handle_datagrams_into` must be reachable from the integration test: `pub mod core;` already is. The core's reassembly may hold a tile until its stale-assembly threshold; a single-fragment Solid tile is delivered on arrival.)

Break it: make `collect_datagram_events` return an empty `Vec`; the test fails at `out.length() == 1`. Revert.

- [ ] **Step 5: Commit.**

```bash
cd ~/work/gf-web-renderer
git add ghostframe-client-wasm/src/core.rs ghostframe-client-wasm/tests/web_renderer.rs
git commit -m "feat(web): WasmClientCore applies tile events to the renderer inside wasm

handleDatagramsInto and onTimeoutInto; the old methods are unchanged and
share the datagram loop.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 11 [sonnet]: Browser tests for what only a browser can show

These are the tests the spec's section 7 and section 8 ask for in headless Chrome: H.264 ordering (with the breakage that must turn it red), the errors-readback hazard (`renderer_errors_readback`), idle skip against a real GPU, CDF 5/3 GPU against CPU (the proof of the fallback path), session reset, and the gradient golden. Append them to `ghostframe-client-wasm/tests/web_renderer.rs`; the helpers from Task 9 (`new_renderer`, `dims`, `solid`, `px`, `near`, colours) are reused. Run each as it is written.

**Files:**
- Modify: `ghostframe-client-wasm/tests/web_renderer.rs`

- [ ] **Step 1: Helpers.** Add below the Task 9 helpers:

```rust
use ghostframe_client_wasm::web::renderer::PaintOutcome;

const MAGENTA: [u8; 4] = [255, 0, 255, 255];

async fn sleep(ms: i32) {
    let p = js_sys::Promise::new(&mut |resolve, _| {
        web_sys::window()
            .unwrap()
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms)
            .unwrap();
    });
    JsFuture::from(p).await.unwrap();
}

/// A real `VideoFrame` of one colour (RGB), made from an `OffscreenCanvas`, so
/// the H.264 escape imports exactly what WebCodecs' decoder would hand it.
fn video_frame(rgb: [u8; 3], w: u32, h: u32) -> wasm_bindgen::JsValue {
    let make = js_sys::Function::new_with_args(
        "r,g,b,w,h",
        "const c = new OffscreenCanvas(w, h); const x = c.getContext('2d'); \
         x.fillStyle = `rgb(${r},${g},${b})`; x.fillRect(0, 0, w, h); \
         return new VideoFrame(c, { timestamp: 0 });",
    );
    let args = js_sys::Array::of5(&rgb[0].into(), &rgb[1].into(), &rgb[2].into(), &w.into(), &h.into());
    make.apply(&wasm_bindgen::JsValue::NULL, &args)
        .expect("VideoFrame from a canvas")
}

/// `VideoFrame.close()` leaves `codedWidth` at 0.
fn is_closed(frame: &wasm_bindgen::JsValue) -> bool {
    js_sys::Reflect::get(frame, &"codedWidth".into())
        .ok()
        .and_then(|v| v.as_f64())
        == Some(0.0)
}

fn tick(r: &mut WebRenderer, n: u64) -> PaintOutcome {
    r.paint_tick(n * 1_000).expect("paint tick")
}
```

- [ ] **Step 2: The ordering test.** Append:

```rust
/// Spec section 7, "H.264 ordering": tile N, picture N+1, tile N+2, then read
/// the framebuffer back. A picture the ledger finds stale is dropped, so the
/// order the GPU runs two overlapping draws in is only visible when neither is
/// stale: the last tick draws a tile and a picture of the SAME frame_seq,
/// which the contract (flush, then pictures, then present) says leaves the
/// picture on top.
#[wasm_bindgen_test]
async fn tiles_and_pictures_are_ordered_by_frame_seq_and_the_queue() {
    let mut r = new_renderer("auto").await;
    assert!(r.h264_supported(), "Chromium has texture_external: the picture tests need it");
    r.apply(dims(64, 64)).unwrap();

    // Tick 1, tile N = 10: every tile red.
    for (x, y) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
        r.apply(solid(10, x, y, RED)).unwrap();
    }
    tick(&mut r, 1);
    assert!(near(px(&r, 8, 8).await, RED, 0) && near(px(&r, 40, 40).await, RED, 0));

    // Tick 2, picture N+1 = 11 (green): over tile N everywhere.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 11);
    let out = tick(&mut r, 2);
    assert_eq!((out.pictures_drawn, out.pictures_dropped, out.presented), (1, 0, true));
    assert!(near(px(&r, 8, 8).await, GREEN, 3), "picture N+1 over tile N: {:?}", px(&r, 8, 8).await);
    assert!(near(px(&r, 40, 40).await, GREEN, 3));
    // `note_h264` marks no damage, so the tick presented because a picture was
    // DRAWN; nothing may be left over to present again.
    assert!(!tick(&mut r, 21).presented, "a drawn picture must not leave damage behind");

    // Tick 3, tile N+2 = 12 (yellow) on tile (0,0): over the picture; the rest stays the picture.
    r.apply(solid(12, 0, 0, YELLOW)).unwrap();
    tick(&mut r, 3);
    assert!(near(px(&r, 8, 8).await, YELLOW, 0), "tile N+2 over picture N+1");
    assert!(near(px(&r, 40, 40).await, GREEN, 3), "picture N+1 over tile N where N+2 did not draw");

    // Tick 4, a late picture 11 (red): tile 12 is newer, so it is stale: closed, not drawn, not presented.
    let late = video_frame([255, 0, 0], 64, 64);
    let presents = r.submit_count();
    r.push_h264(late.clone(), 11);
    let out = tick(&mut r, 4);
    assert_eq!((out.pictures_drawn, out.pictures_dropped, out.presented), (0, 1, false));
    assert_eq!(r.submit_count(), presents, "a stale picture must not cost a present");
    assert!(is_closed(&late), "a stale picture must be closed, not leaked");
    assert!(near(px(&r, 8, 8).await, YELLOW, 0) && near(px(&r, 40, 40).await, GREEN, 3));

    // Tick 5, the queue order itself: tile 13 (blue) on tile (1,0) and picture 13
    // (magenta) in one tick. Equal frame_seq supersedes nothing, so both draw;
    // tiles are flushed first, so the picture ends on top.
    r.apply(solid(13, 1, 0, BLUE)).unwrap();
    r.push_h264(video_frame([255, 0, 255], 64, 64), 13);
    let out = tick(&mut r, 5);
    assert_eq!(out.pictures_drawn, 1);
    assert!(
        near(px(&r, 40, 8).await, MAGENTA, 3),
        "the picture must be submitted after the tile flush: got {:?}",
        px(&r, 40, 8).await
    );
    assert!(near(px(&r, 8, 8).await, MAGENTA, 3));
}
```

```bash
cd ~/work/gf-web-renderer
wasm-pack test --headless --chrome ghostframe-client-wasm --test web_renderer 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"
```

Expected: passes. If the picture comes out a few levels off the colour you asked for, the tolerance of 3 is the knob; a wholly different colour is a colour-space problem in `importExternalTexture` (report it, do not widen the tolerance past 8).

**Break it twice, per the spec and AGENTS.md.**
 (a) In `WebRenderer::paint_tick` (Task 9) move `let (drawn, dropped) = self.draw_pending_pictures();` above the `match self.tiles.flush(...)`: tick 5 fails with `the picture must be submitted after the tile flush: got [0, 0, 255, 255]`-style (blue). Revert.
 (b) In `draw_pending_pictures` delete the `if self.tiles.h264_stale(p.seq) { ... continue; }` block: tick 4 fails (`pictures_dropped` is 0 and the picture is drawn over the tiles). Revert.
 (c) In `paint_tick` change `changed |= drawn > 0;` to `changed |= true;`: tick 4 fails on `presented`/`submit_count`. Revert.
 (d) In `draw_pending_pictures` call `self.tiles.mark_all_dirty()` after drawing: the extra idle tick after tick 2 fails (`a drawn picture must not leave damage behind`), because the next flush reports the whole screen. Revert. And delete `changed |= drawn > 0;` instead: tick 2 fails on `presented`, since a picture-only tick has an empty `FlushReport::damage`. Revert.

- [ ] **Step 3: Idle skip.**

```rust
#[wasm_bindgen_test]
async fn an_idle_tick_does_not_touch_the_gpu_and_an_event_wakes_it() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    r.apply(solid(1, 0, 0, RED)).unwrap();
    assert!(tick(&mut r, 1).presented);
    let before = (r.submit_count(), r.paint_tick_count());
    for n in 2..12 {
        assert!(!tick(&mut r, n).presented, "tick {n} had nothing to paint");
    }
    assert_eq!(
        (r.submit_count(), r.paint_tick_count()),
        before,
        "ten idle ticks must neither present nor count as painted ticks"
    );
    r.apply(solid(2, 1, 0, GREEN)).unwrap();
    assert!(tick(&mut r, 20).presented);
    assert_eq!(r.submit_count(), before.0 + 1);
}
```

Break: in `should_skip_paint` return `false` always: the loop of idle ticks fails. Revert.

- [ ] **Step 4: PalRLE errors are read back, never provoke a browser error, and still arrive on a screen that has stopped changing** (the port of `renderer_errors_readback.test.ts`: the rule is "a copy must never go into a staging buffer an earlier readback still holds"; the second test is what `e2e_palrle_oob_index` needs).

```rust
fn bad_palrle(frame_seq: u32, x: u8, y: u8) -> Event {
    // Two palette colours, indices 15: out of range, which the shader reports.
    Event::TilePayload {
        frame_seq,
        tile_x: x,
        tile_y: y,
        generation: 0,
        data: TileData::PalRle { palette_id: 0, count: 2, indices: vec![0xFF; 512], wire_flags: 0 },
    }
}

#[wasm_bindgen_test]
async fn palrle_errors_are_read_back_without_a_browser_error() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(128, 128)).unwrap();
    r.apply(Event::PaletteUpdated { palette_id: 0, colors: vec![RED, GREEN] }).unwrap();
    let mut found: Vec<u32> = Vec::new();
    let mut last = GREEN;
    // A PalRLE batch on every tick keeps the staging buffer contended. The
    // errors copy is a submit of its own (`start_errors_readback`), and a copy
    // into a buffer that is still pending or mapped is a validation error the
    // browser reports (and that batch's errors are lost): `take_errors()` below
    // is the detector. (The TypeScript carried the copy in the tick's one
    // encoder, so there the whole tick was rejected; here the Solid tile is
    // only a sanity check that painting carried on.)
    for n in 0..60u32 {
        r.apply(bad_palrle(2 + n, 0, 0)).unwrap();
        last = if n % 2 == 0 { GREEN } else { BLUE };
        r.apply(solid(2 + n, 3, 3, last)).unwrap();
        tick(&mut r, u64::from(n) + 1);
        sleep(4).await; // let the map callbacks run
        found.extend(r.take_decode_errors());
    }
    assert!(
        found.chunks(4).any(|c| c[1] == 0 && c[2] == 0 && c[3] == 5),
        "no IndexOob (5) for tile (0,0) in {found:?}"
    );
    assert!(found.chunks(4).all(|c| c[0] == 2), "every report names the PalRle codec: {found:?}");
    let errors = r.take_errors();
    assert_eq!(errors.length(), 0, "the browser rejected or reported something: {errors:?}");
    assert!(near(px(&r, 3 * 32 + 16, 3 * 32 + 16).await, last, 0), "the last Solid tile was painted");
}
```

(`Codec::PalRle` discriminant 2: `ghostframe_protocol::protocol::Codec::PalRle as u8`; use that in the assertion if the numeric literal disagrees.)

Break it (this is the vitest's rule, proven here): in `ghostframe-render`'s `PalRlePipeline::start_errors_readback` (Task 3) comment out the `|| !rb.try_begin(tiles)` guard so it copies every tick. Re-run: the test fails with a non-empty `take_errors()` naming a buffer used in a submit while mapped. Revert.

```rust
#[wasm_bindgen_test]
async fn a_palrle_error_on_a_static_screen_is_still_reported() {
    // The shape of e2e_palrle_oob_index (CAPTURE_FPS=2, a scene that never
    // changes): one bad tile, then only idle ticks. The map callback finishes
    // between ticks; nothing flushes afterwards, so the result must be
    // collected by `take_decode_errors` itself and the idle skip must not hide
    // it.
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    r.apply(Event::PaletteUpdated { palette_id: 0, colors: vec![RED, GREEN] }).unwrap();
    r.apply(bad_palrle(1, 0, 0)).unwrap();
    tick(&mut r, 1);
    let mut found: Vec<u32> = Vec::new();
    for n in 2..300u64 {
        sleep(10).await;
        let out = tick(&mut r, n); // idle: nothing was applied since tick 1
        assert_eq!(out.pictures_drawn, 0);
        found.extend(r.take_decode_errors());
        if !found.is_empty() {
            break;
        }
    }
    assert!(
        found.chunks(4).any(|c| c[1] == 0 && c[2] == 0 && c[3] == 5),
        "IndexOob (5) for tile (0,0) never arrived on a static screen: {found:?}"
    );
}
```

Break it (the bug this test exists for): in `ErrorsReadback::pending` count only `State::InFlight(_)` (the old `in_flight`) **and** delete the `self.collect_finished_errors();` line from `TileRenderer::take_decode_errors`: the finished result is never taken, the idle ticks skip, and the test fails after 300 ticks with `never arrived`. Revert both.

- [ ] **Step 5: CDF 5/3 on the GPU is exactly the CPU's, and the forced paths take the CPU.** This is the browser proof behind the spec's "fallback must still pass the golden" (the forced-failure spelling of the spec is realised as `?cdf53=cpu`: the same CPU path, without running a self-test that would then have to be disabled). Append:

```rust
use ghostframe_client_core::cdf53_prevalidate::{prevalidate_cdf53, PrevalidatedCdf53};
use ghostframe_client_core::cdf53_tile_state::Cdf53TileState;
use ghostframe_protocol::codec::cdf53 as codec;

fn gradient_bgra(seed: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity(4096);
    for y in 0..32u32 {
        for x in 0..32u32 {
            v.extend_from_slice(&[
                ((x * 6 + seed * 11) & 0xFF) as u8,
                ((y * 7 + seed) & 0xFF) as u8,
                (128 + x + y) as u8,
                0xFF,
            ]);
        }
    }
    v
}

fn passes(seed: u32) -> Vec<PrevalidatedCdf53> {
    let coeffs = codec::forward(&gradient_bgra(seed));
    let (_, sparse) = codec::encode_passes_sparse(&coeffs);
    sparse
        .iter()
        .map(|(p, payload)| prevalidate_cdf53(payload, 1, *p).expect("valid pass"))
        .collect()
}

fn pass_event(p: &PrevalidatedCdf53, x: u8, y: u8, seq: u32, first: bool) -> Event {
    Event::TilePayload {
        frame_seq: seq,
        tile_x: x,
        tile_y: y,
        generation: p.generation,
        data: TileData::Cdf53 {
            pass_idx: p.pass_idx,
            bit_planes: p.bit_planes.clone(),
            present_passes: p.present_passes,
            new_generation: first,
        },
    }
}

async fn paint_cdf53(mode: &str) -> (WebRenderer, Vec<u8>) {
    let mut r = new_renderer(mode).await;
    r.apply(dims(64, 64)).unwrap();
    for (i, (x, y)) in [(0u8, 0u8), (1, 1)].into_iter().enumerate() {
        for (k, p) in passes(i as u32 + 1).iter().enumerate() {
            r.apply(pass_event(p, x, y, 1, k == 0)).unwrap();
        }
    }
    // The grid defers work past its per-flush budget: tick until it is done.
    for n in 1..=200u64 {
        let _ = tick(&mut r, n);
        if !r.has_deferred_work() {
            break;
        }
        sleep(2).await;
    }
    tick(&mut r, 300);
    let bytes = JsFuture::from(r.read_pixel_rect(0, 0, 64, 64).unwrap()).await.unwrap();
    let v = Uint8Array::new(&bytes).to_vec();
    (r, v)
}

#[wasm_bindgen_test]
async fn cdf53_on_the_gpu_matches_the_cpu_decoder_exactly_and_the_forced_path_uses_the_cpu() {
    let (gpu, gpu_px) = paint_cdf53("auto").await;
    assert_eq!(gpu.cdf53_path(), "gpu", "this browser must pass the CDF 5/3 self-test in `try_enable_cdf53_gpu` (else only the fallback is tested)");
    let (cpu, cpu_px) = paint_cdf53("cpu").await;
    assert_eq!(cpu.cdf53_path(), "cpu");

    // The CPU decoder in `ghostframe-client-core` is the reference for what the
    // screen must show; lossless passes of a known picture reproduce it exactly.
    let mut want = vec![0u8; 64 * 64 * 4];
    for (i, (tx, ty)) in [(0usize, 0usize), (1, 1)].into_iter().enumerate() {
        let mut st = Cdf53TileState::new();
        let mut shown = None;
        for p in &passes(i as u32 + 1) {
            if let Some(rgba) = st.integrate(0, 0, p) {
                shown = Some(rgba);
            }
        }
        let shown = shown.expect("the CPU decoder drew the tile");
        for y in 0..32 {
            let o = ((ty * 32 + y) * 64 + tx * 32) * 4;
            want[o..o + 128].copy_from_slice(&shown[y * 128..y * 128 + 128]);
        }
    }
    assert_eq!(cpu_px, want, "the CPU path against the CPU reference");
    assert_eq!(gpu_px, want, "the GPU grid against the CPU reference: lossless means exact");
}
```

This needs one more accessor on `WebRenderer` (Task 9): add

```rust
    /// Whether CDF 5/3 work is waiting for a later tick (budget or throttle).
    pub fn has_deferred_work(&self) -> bool {
        self.tiles.has_deferred_work()
    }
```
to the plain `impl WebRenderer` block. (`Cdf53TileState::integrate` takes the tile coordinates it keys state on; native `cdf53_renderer.rs::cpu_picture` shows the exact call, and its `new()`/`integrate(0, 0, p)` shape is what is used above.)

Break it: in `ghostframe-render`'s grid reconstruct path (or simply in the test) change one expected pixel, e.g. `want[0] ^= 1`: both assertions fail, proving the comparison reads real pixels. Revert. Then break the product: in `create_inner` call `try_enable_cdf53_gpu` also for `Cdf53Mode::Cpu`: the `"cpu"` path assertion fails. Revert. (The failure branch of the self-test, a grid that draws nothing staying off, is PR 1's `the_self_test_keeps_a_grid_that_draws_nothing_off`; the web cannot force it without a way to disable an enabled grid, so there is no `?cdf53=fail`.)

- [ ] **Step 5b: CDF 5/3 work under a picture is held and cancelled, not left pending.** Append (uses `passes`, `pass_event`, `video_frame` from above):

```rust
#[wasm_bindgen_test]
async fn cdf53_work_under_a_picture_is_cancelled_and_not_left_pending() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    for (k, p) in passes(1).iter().enumerate() {
        r.apply(pass_event(p, 0, 0, 5, k == 0)).unwrap();
    }
    // The picture (seq 6) is newer than every pass (seq 5): not stale, so the
    // tick holds the passes, draws the picture and `note_h264` cancels them.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 6);
    let out = tick(&mut r, 1);
    assert_eq!(out.pictures_drawn, 1);
    assert!(near(px(&r, 8, 8).await, GREEN, 3), "the picture covers the CDF 5/3 tile");
    assert!(!r.has_deferred_work(), "held passes were cancelled by the picture, not left waiting");
    assert!(!tick(&mut r, 2).presented, "and nothing is left to draw");
    let state = r.debug_state().unwrap();
    assert_eq!(js_sys::Reflect::get(&state, &"cdf53Pending".into()).unwrap(), false);
}
```

Break it: delete the `self.tiles.note_h264(p.seq);` line in `draw_pending_pictures`: the held passes stay pending, `has_deferred_work()` is true, and the test fails. Revert. (That the hold itself saves GPU budget is PR 1's `hold_cdf53` behaviour, covered by its own tests; here it is the interaction that is pinned.)

- [ ] **Step 6: Session reset, handles and the gradient golden.**

```rust
#[wasm_bindgen_test]
async fn a_session_reset_closes_pending_pictures_and_forgets_the_ledger() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(64, 64)).unwrap();
    r.apply(solid(10, 0, 0, RED)).unwrap();
    tick(&mut r, 1);
    // Picture 3 is older than tile 10: stale, dropped.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 3);
    assert_eq!(tick(&mut r, 2).pictures_dropped, 1);
    // A picture still pending when the transport closes is closed by the reset.
    let pending = video_frame([0, 255, 0], 64, 64);
    r.push_h264(pending.clone(), 3);
    r.on_session_reset();
    assert!(is_closed(&pending), "a session reset must close pending pictures");
    // The new session restarts its sequence numbers: picture 3 is drawn now.
    r.push_h264(video_frame([0, 255, 0], 64, 64), 3);
    assert_eq!(tick(&mut r, 3).pictures_drawn, 1, "the ledger must be forgotten");
}

#[wasm_bindgen_test]
async fn the_debug_handles_expose_the_framebuffer_texture_the_e2e_reads() {
    let mut r = new_renderer("auto").await;
    assert!(r.debug_handles().is_undefined(), "no texture before the first size");
    r.apply(dims(96, 64)).unwrap();
    let h = r.debug_handles();
    let texture = js_sys::Reflect::get(&h, &"texture".into()).unwrap();
    let w = js_sys::Reflect::get(&texture, &"width".into()).unwrap().as_f64();
    let hh = js_sys::Reflect::get(&texture, &"height".into()).unwrap().as_f64();
    assert_eq!((w, hh), (Some(96.0), Some(64.0)));
    assert!(!js_sys::Reflect::get(&h, &"device".into()).unwrap().is_undefined());
    let state = r.debug_state().unwrap();
    // The old renderer's keys, and PR 1's `DebugState` spliced in (camelCase).
    for key in [
        "submitCount", "fbWidth", "fbHeight", "hasTexture", "paintTick", "canvasWidth",
        "solidQueue", "palRleQueue", "flushes", "cdf53Gpu", "newestTileSeq",
    ] {
        assert!(!js_sys::Reflect::get(&state, &key.into()).unwrap().is_undefined(), "debugState().{key}");
    }
    r.apply(solid(7, 1, 1, RED)).unwrap();
    let tile = r.debug_tile(1, 1).unwrap();
    assert_eq!(js_sys::Reflect::get(&tile, &"newestFrameSeq".into()).unwrap().as_f64(), Some(7.0));
    assert!(r.debug_tile(200, 200).unwrap().is_null(), "off the grid is null, as before");
}

#[wasm_bindgen_test]
async fn the_gradient_golden_agrees_with_the_pixel_readback() {
    let mut r = new_renderer("auto").await;
    r.apply(dims(512, 300)).unwrap();
    let report = JsFuture::from(r.read_gradient_golden().unwrap()).await.unwrap();
    let ok = js_sys::Reflect::get(&report, &"ok".into()).unwrap();
    assert_eq!(ok, true, "mismatches: {:?}", js_sys::Reflect::get(&report, &"mismatches".into()));
}
```

Break: in `a_session_reset...` comment out `self.tiles.reset_session(...)` in `reset_session`: the final `pictures_drawn == 1` fails. In the golden, change `want` in `debug.rs` to `(y & 0xFF) as u8, (x & 0xFF) as u8`: the golden test fails. Revert both.

- [ ] **Step 7: Run all, lint, commit.**

```bash
cd ~/work/gf-web-renderer
wasm-pack test --headless --chrome ghostframe-client-wasm --test web_renderer 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings 2>&1 | tail -10; echo "rc=${PIPESTATUS[0]}"
git add ghostframe-client-wasm/tests/web_renderer.rs ghostframe-client-wasm/src/web/renderer.rs
git commit -m "test(web): browser tests for ordering, idle skip, errors readback, CDF 5/3 and reset

Each was shown failing on a mutation of the code it covers.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

Expected: 11 tests pass (`a_solid_tile...`, `datagrams_reach...`, ordering, idle, errors, static-screen errors, cdf53, held cdf53, reset, handles, golden).

## Task 12 [sonnet]: `main.ts` and the diagnostics use `WebRenderer`

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`, `src/diagnostics.ts`, `src/stats_log.ts`, `src/globals.d.ts`, `tests/diagnostics.test.ts`

Nothing of the TypeScript renderer is deleted yet (Task 13); this task stops using it.

- [ ] **Step 1: `main.ts` imports.** Replace the import block's first lines and add `WebRenderer` to the wasm import.

Old:
```ts
import { FullFrameDecoder, H264_DECODE_ERROR } from './decoder.js';
import { WebGpuRenderer } from './webgpu/renderer.js';
import { WebGpuUnavailableError } from './webgpu/init.js';
import { attachInputCapture } from './input/wire';
import { checkFrameDimensions } from './frame_limits.js';
import { createStatsLogger } from './stats_log.js';
import { installCdf53Debug } from './cdf53_debug.js';
import { initDiagnostics } from './diagnostics.js';
```
New:
```ts
import { FullFrameDecoder, H264_DECODE_ERROR } from './decoder.js';
import { attachInputCapture } from './input/wire';
import { createStatsLogger } from './stats_log.js';
import { initDiagnostics, recordPaletteWrite } from './diagnostics.js';
```
and
```ts
import init, {
  WasmClientCore,
  mountStatsPanel,
```
becomes
```ts
import init, {
  WasmClientCore,
  WebRenderer,
  mountStatsPanel,
```

- [ ] **Step 2: Renderer creation.** Replace the region from the comment `// WebGPU init — fatal if unavailable per design D2.` through the closing `});` of the `initDiagnostics({...})` call with:

```ts
  // WebGPU init — fatal if unavailable per design D2. `?cdf53=cpu` forces
  // the CPU CDF 5/3 path (the e2e run that proves the fallback); anything
  // else lets the grid's self-test decide.
  let renderer: WebRenderer;
  try {
    renderer = await WebRenderer.create(canvasEl, url.searchParams.get('cdf53') ?? 'auto');
    // Test/diagnostic hooks: swallowed-exception counts and renderer state.
    window.__ghostframeHealth = () => health.snapshot();
    window.__ghostframeRendererState = () => renderer.debugState();
    window.__ghostframeTile = (x: number, y: number) => renderer.debugTile(x, y);
    // Time per painted tick, for the section 7 bench: [count, p50, p99, max] in
    // microseconds since the last call.
    if (url.searchParams.has('e2e')) {
      window.__ghostframePaintUs = () => Array.from(renderer.paintUsStats());
    }
  } catch (e) {
    if (String(e).startsWith('WEBGPU_UNAVAILABLE')) {
      statusEl.textContent = 'WebGPU not available in this browser.';
      log(String(e));
      return;
    }
    throw e;
  }

  // Wire all test/e2e diagnostic globals onto `window` via diagnostics.ts.
  const diag = initDiagnostics({
    canvas: canvasEl,
    reader: renderer,
    // Pixel readbacks are for the e2e suite only (`?e2e` on the page URL).
    readbacks: url.searchParams.has('e2e'),
  });
```

- [ ] **Step 3: Input capture** reads the framebuffer size from the renderer's getters. Old: `width: renderer.framebuffer.width,\n      height: renderer.framebuffer.height,` New: `width: renderer.fbWidth,\n      height: renderer.fbHeight,`.

- [ ] **Step 4: Event types.** Replace the block from the comment `// Serde mirror of \`ghostframe-client-wasm/src/boundary.rs\`'s \`WasmEvent\`,` through the line `| { kind: 'Evicted'; reason: number };` with:

```ts
  // What `core.handleDatagramsInto` / `core.onTimeoutInto` return: one record
  // per event, in order (`ghostframe-client-wasm/src/web/route.rs`). Tile
  // bytes never reach JS: the byte fields of a tile hold at most the first 16
  // bytes and `len` is the real payload length. `error` is set when the
  // renderer refused the event (the rest of the batch was still applied).
  type SlimTileData =
    | { codec: 'Solid'; bytes: Uint8Array; len: number }
    | { codec: 'PalRle'; palette_id: number; count: number; wire_flags: number; indices: Uint8Array; len: number }
    | { codec: 'Cdf53'; pass_idx: number; bit_planes: Uint8Array; present_passes?: number | null; len: number };

  type RoutedEvent =
    | {
        kind: 'TilePayload';
        frame_seq: number;
        tile_x: number;
        tile_y: number;
        generation: number;
        data: SlimTileData;
        error?: string;
      }
    | { kind: 'PaletteUpdated'; palette_id: number; colors: [number, number, number, number][]; error?: string }
    | { kind: 'FrameDimensions'; width: number; height: number; applied: boolean; reason?: string }
    | { kind: 'TileReady'; frame_seq: number; tile_x: number; tile_y: number }
    | {
        kind: 'NeedsH264';
        frame_seq: number;
        timestamp_us: number;
        is_keyframe: boolean;
        ref_frame_seq: number;
        flags: number;
        slots: number;
        reset_decoder: boolean;
        payload: Uint8Array;
      }
    | { kind: 'DecodeError'; codec: number; tile_x: number; tile_y: number; code: number }
    | { kind: 'Evicted'; reason: number };

  // The framebuffer size the server last announced (applied), for the recorders.
  let fbW = 0;
  let fbH = 0;
  // Events applied since the last tick, per codec, for the page log's Δdrain.
  const drained = { solid: 0, palrle: 0, cdf53: 0, h264: 0 };
```

Rename `WasmEvent` to `RoutedEvent` in the signatures `function handleEvent(ev: WasmEvent)`, `function handleSafely(ev: WasmEvent)`, and fix the comment above `handleEvent`: it still describes `TileReady`; keep its text.

- [ ] **Step 5: The `TilePayload`, `PaletteUpdated` and `FrameDimensions` cases** of `handleEvent`. Replace the whole `case 'TilePayload': { ... }`, `case 'PaletteUpdated': { ... }` and `case 'FrameDimensions': { ... }` blocks with:

```ts
      case 'TilePayload': {
        const d = ev.data;
        if (ev.error) {
          // The renderer refused it; the core has already acknowledged the tile.
          health.record(
            'renderer.apply',
            `tile (${ev.tile_x},${ev.tile_y}) seq ${ev.frame_seq}: ${ev.error}`,
          );
        }
        const sampleBytes: Uint8Array =
          d.codec === 'Solid' ? d.bytes : d.codec === 'PalRle' ? d.indices : d.bit_planes;
        if (d.codec === 'Solid') {
          drained.solid++;
          if (d.bytes[2] > 0x80) health.mark(ev.tile_x, ev.tile_y, ev.frame_seq, 'solid-red');
          if (diagnostics) {
            health.tally(`solid:${Array.from(d.bytes).map((b) => b.toString(16).padStart(2, '0')).join('')}`);
          }
        } else if (d.codec === 'PalRle') {
          drained.palrle++;
        } else {
          drained.cdf53++;
        }

        const tileNowMs = performance.now();
        const firstRecvMsClient = noteFrameTileReceived(ev.frame_seq, tileNowMs);

        // Re-derive the `__ghostframeRecorded*` test globals from the event.
        // `palRleFlag` is the wire flags byte the server encoded, not the
        // palette id: `e2e_indices_raw_handshake` tests its bit 1.
        diag.recordTile({
          seq: ev.frame_seq,
          tileX: ev.tile_x,
          tileY: ev.tile_y,
          codec: codecDiscriminant(d.codec),
          payloadLen: d.len,
          fbWidth: fbW,
          fbHeight: fbH,
          palRleFlag: d.codec === 'PalRle' ? d.wire_flags : undefined,
          firstRecvMsClient,
          lastPaintMsClient: tileNowMs,
        });

        if (!firstTileRendered) {
          firstTileRendered = true;
          const sample = Array.from(sampleBytes.slice(0, 16))
            .map(b => b.toString(16).padStart(2, '0'))
            .join(' ');
          log(`First tile: (${ev.tile_x},${ev.tile_y}) ${d.len}B`);
          log(`First bytes: ${sample}`);
          statusEl.textContent = 'Receiving frames';
        }
        break;
      }

      case 'PaletteUpdated': {
        // Applied to the atlas inside wasm (before the tiles that use it, in
        // wire order). JS only keeps the e2e fingerprint log of the writes.
        if (ev.error) health.record('renderer.palette', `palette ${ev.palette_id}: ${ev.error}`);
        const bgra = new Uint8Array(ev.colors.length * 4);
        ev.colors.forEach((c, i) => bgra.set(c, i * 4));
        recordPaletteWrite(ev.palette_id, bgra);
        break;
      }

      case 'FrameDimensions': {
        if (!ev.applied) {
          // Keep the current framebuffer rather than allocate for a size the
          // server has no business asking for (or one the renderer refused).
          console.error(`FrameDimensions rejected: ${ev.reason}`);
          log(`FrameDimensions rejected: ${ev.reason}`);
          break;
        }
        // `seq` is diagnostic-only (fifo-logged to
        // window.__ghostframeRecordedResizes, never asserted on); 0 is a placeholder.
        diag.recordResize({ seq: 0, oldW: fbW, oldH: fbH, newW: ev.width, newH: ev.height, trigger: 'sentinel' });
        fbW = ev.width;
        fbH = ev.height;
        // The framebuffer texture was replaced: re-publish the raw handles
        // (`texture.width`/`.height` are read by the e2e suite).
        window.__ghostframeRenderer = renderer.debugHandles();
        frameDimensionsKnown = true;
        break;
      }
```

In the `NeedsH264` case nothing changes except the counter: after `fullFrameDecoder.decode(ev.payload, ev.is_keyframe, ev.frame_seq);` add `drained.h264++;`. In the `TileReady` case keep the console error as it is. Delete the now-unused `import { checkFrameDimensions }` (done in Step 1); `frameDimensionsKnown` stays (it is assigned, never read, as before).

- [ ] **Step 6: The datagram batch and the rAF tick.**

In `flushDatagrams`, old: `for (const ev of core.handleDatagrams(concat, lens, arrivals) as WasmEvent[]) handleSafely(ev);` new:

```ts
    for (const ev of core.handleDatagramsInto(renderer, concat, lens, arrivals) as RoutedEvent[]) {
      handleSafely(ev);
    }
```

In `tick()`, old: `for (const ev of core.onTimeout(nowUs())) handleSafely(ev);` new: `for (const ev of core.onTimeoutInto(renderer, nowUs()) as RoutedEvent[]) handleSafely(ev);`

Replace from the comment `// Snapshot queue depths BEFORE drain so we can log how much each rAF` through the line `statsLog.onTick(beforeDrain, __rafTicks);` with:

```ts
    // Paint: flush the renderer's tiles, draw pending H.264 pictures, present;
    // all of it is skipped when nothing changed. A throw here must not stop
    // the rAF loop: it would freeze the client for the rest of the session.
    let painted = 0;
    try {
      painted = renderer.paintTick(nowUs());
    } catch (e) {
      health.record('paintTick', e);
      console.error('paintTick failed; continuing', e);
    }
    // Renderer-side problems since the last tick (uncaptured GPU errors, a
    // failed flush or present), as the old 'uncapturederror' listener recorded.
    for (const msg of renderer.takeErrors() as string[]) {
      health.record('renderer', `tick ${renderer.paintTickCount}: ${msg}`);
      health.tally(`gpu:${msg.slice(0, 80)}`);
    }
    // Tiles the GPU decode rejected (PalRLE, read back asynchronously): the
    // renderer reports [codec, tile_x, tile_y, code] quadruples.
    const de = renderer.takeDecodeErrors();
    for (let i = 0; i + 3 < de.length; i += 4) reportDecodeError(de[i], de[i + 1], de[i + 2], de[i + 3]);

    // Frames actually presented this tick (an idle tick adds none), for the
    // stats panel's painted-fps row.
    if (painted > 0) core.recordPaints(painted);

    // Drain accounting + the ~2 s stats line live in stats_log.ts.
    statsLog.onTick({ ...drained }, __rafTicks);
    drained.solid = drained.palrle = drained.cdf53 = drained.h264 = 0;
```

and delete the line `// Frames actually submitted this tick ...` block that used `renderer.submitCount - submitsBefore` (it is inside the region replaced above; check no `submitsBefore` remains).

- [ ] **Step 7: `diagnostics.ts`.** Replace the file with the following (the FIFO helper, recorders and counters are unchanged; the three GPU readbacks became thin wrappers over `WebRenderer`, the gradient pipeline moved to Rust, and the palette-write log moved here from `palrle.ts`):

```ts
// SPDX-License-Identifier: MIT OR Apache-2.0
/**
 * diagnostics.ts — test and e2e instrumentation surface.
 *
 * Owns the `window.__ghostframe*` recorders and the `window.__read*` pixel
 * readbacks, so that main.ts is free of scattered `(window as any)` casts.
 * The readbacks are one-line wrappers over `WebRenderer`: the GPU work lives
 * in Rust (`ghostframe-client-wasm/src/web`). The `typeof window !==
 * "undefined"` guards keep the module safe under Vitest/Node.
 */

import { protocol } from './protocol_constants.js';

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/** Shape returned by initDiagnostics. */
export interface DiagnosticsHandle {
  /** Mutable stats counters — main.ts increments these directly. */
  stats: { tileDatagrams: number; frameDatagrams: number };

  /** Record a framebuffer resize. */
  recordResize(params: {
    seq: number;
    oldW: number; oldH: number;
    newW: number; newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }): void;

  /** Record an applied tile. */
  recordTile(params: {
    seq: number;
    tileX: number; tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number; fbHeight: number;
    /** The PalRLE payload flags byte; only pushed when given. */
    palRleFlag?: number;
    // M3.5 bench instrumentation — optional, default undefined.
    // performance.now() values; DOMHighResTimeStamp, millisecond resolution.
    /** performance.now() at first fragment receipt for this tile. */
    firstRecvMsClient?: number;
    /** performance.now() when the tile was handed to the renderer. */
    lastPaintMsClient?: number;
  }): void;

  /** Update the rAF tick counter on window. */
  recordRafTick(n: number): void;

  // M3.5 bench instrumentation: called from rAF callback after a frame's
  // last tile has been painted.
  recordFramePainted(params: {
    seq: number;
    /** performance.now() inside the rAF callback. */
    rafMsClient: number;
  }): void;
}

/** The slice of `WebRenderer` the readbacks need. */
export interface PixelReader {
  /** RGBA bytes of a framebuffer rectangle. */
  readPixelRect(x: number, y: number, w: number, h: number): Promise<Uint8Array>;
  /** Writes a known gradient into the framebuffer and reports mismatches. */
  readGradientGolden(): Promise<{
    ok: boolean;
    mismatches: Array<{ x: number; y: number; got: number[]; want: number[] }>;
  }>;
}

// ---------------------------------------------------------------------------
// FIFO cap
// ---------------------------------------------------------------------------

// Cap test-instrumentation recorder arrays to bound memory + CDP
// serialisation cost in long-running tests. Sized for the busiest e2e
// (e2e_cdf53_mixed_codecs at 1920x1080: ~80 dirty tiles × 30 fps × ~5 s
// settle ≈ 12 000 records, plus headroom). FIFO drop keeps the most
// recent observations when exceeded.
export const MAX_RECORDED_ENTRIES = 32768;

/**
 * Append to a log that keeps its most recent `cap` entries.
 *
 * The log is trimmed back to `cap` only once it reaches twice that, so a
 * reader sees between `cap` and `2 * cap` entries, newest last.
 *
 * It used to be `push` then `shift`, one out for each one in. Dropping the
 * head of a 32768-element array makes V8 move the other 32767: 64 us a
 * call, measured, against 0.5 us at the 4096 this cap had until it was
 * raised. Every tile is recorded in up to four of these logs, so once a
 * session had drawn its first ~30,000 tiles each further one cost a quarter
 * of a millisecond of bookkeeping -- on a screen redrawing 23,000 tiles a
 * second, the datagram loop fell two seconds behind and stopped
 * acknowledging, and the server read a loss-free link as dead.
 */
export function appendCapped<T>(arr: T[], item: T, cap: number = MAX_RECORDED_ENTRIES): void {
  arr.push(item);
  if (arr.length >= 2 * cap) arr.splice(0, arr.length - cap);
}

function fifoAppend<T>(arr: T[], item: T): void {
  appendCapped(arr, item);
}

/**
 * Mirror every palette atlas write into a test-readable global so the e2e
 * test can dump it post-run and compare against the server's write_bytes log
 * to detect (id, palette) drift. The fingerprint is the one the server logs
 * for the same palette, computed by the Rust function itself so the two
 * sides cannot drift. (The atlas itself is written inside wasm.)
 */
export function recordPaletteWrite(paletteId: number, bgra: Uint8Array): void {
  if (typeof window === 'undefined') return;
  const acc = protocol().paletteFingerprint(bgra);
  const fpHex = (acc >>> 0).toString(16).padStart(8, '0');
  const c0 = bgra.length >= 4
    ? Array.from(bgra.slice(0, 4)).map(b => b.toString(16).padStart(2, '0')).join('')
    : '';
  if (!window.__h2_clientPaletteWrites) window.__h2_clientPaletteWrites = [];
  appendCapped(window.__h2_clientPaletteWrites, {
    id: paletteId,
    len: bgra.byteLength,
    fp: fpHex,
    c0,
    ts: performance.now(),
  });
}

// ---------------------------------------------------------------------------
// Initialiser
// ---------------------------------------------------------------------------

/**
 * Wire the diagnostic globals onto `window` and return a handle main.ts uses
 * for per-event recording.
 *
 * @param reader     The renderer, for the pixel readbacks.
 * @param canvas     Kept for the handle's callers; unused by the readbacks.
 * @param readbacks  Install `__readPixel`, `__readPixelRect` and
 *                   `__readGradientGolden`. Off unless the page URL has
 *                   `?e2e`: they exist for the e2e suite, and a production
 *                   session should not carry code that can map the
 *                   framebuffer to the CPU.
 */
export function initDiagnostics({
  reader,
  readbacks = false,
}: {
  canvas?: HTMLCanvasElement;
  reader?: PixelReader;
  readbacks?: boolean;
}): DiagnosticsHandle {
  // ---- stats counter -------------------------------------------------------
  const stats: { tileDatagrams: number; frameDatagrams: number } = {
    tileDatagrams: 0,
    frameDatagrams: 0,
  };

  if (typeof window !== 'undefined') {
    window.__ghostframeStats = stats;
  }

  // ---- pixel readbacks (e2e only) ------------------------------------------
  if (readbacks && reader && typeof window !== 'undefined') {
    // [R, G, B, A], the framebuffer texture's own bytes.
    window.__readPixel = async (x: number, y: number): Promise<number[]> =>
      Array.from(await reader.readPixelRect(x, y, 1, 1));
    window.__readPixelRect = async (
      x: number, y: number, w: number, h: number,
    ): Promise<number[]> => (w <= 0 || h <= 0 ? [] : Array.from(await reader.readPixelRect(x, y, w, h)));
    // Debug-only: overwrite the framebuffer with a known gradient and read it
    // back, so e2e_readpixel_correctness can verify the readbacks before any
    // pixel assertion is trusted.
    window.__readGradientGolden = () => reader.readGradientGolden();
  }

  // ---- DiagnosticsHandle implementation ------------------------------------

  function recordResize(params: {
    seq: number;
    oldW: number; oldH: number;
    newW: number; newH: number;
    trigger: 'sentinel' | 'fallback-expand';
  }): void {
    if (typeof window === 'undefined') return;
    if (!window.__ghostframeRecordedResizes) {
      window.__ghostframeRecordedResizes = [];
    }
    fifoAppend(window.__ghostframeRecordedResizes, params);
  }

  function recordTile(params: {
    seq: number;
    tileX: number; tileY: number;
    codec: number;
    payloadLen: number;
    fbWidth: number; fbHeight: number;
    palRleFlag?: number;
    firstRecvMsClient?: number;
    lastPaintMsClient?: number;
  }): void {
    if (typeof window === 'undefined') return;
    if (!window.__ghostframeRecordedCodecs) window.__ghostframeRecordedCodecs = [];
    if (!window.__ghostframeRecordedTiles) window.__ghostframeRecordedTiles = [];
    if (!window.__ghostframeRecordedFlags) window.__ghostframeRecordedFlags = [];

    fifoAppend(window.__ghostframeRecordedCodecs, params.codec);
    if (params.palRleFlag !== undefined) {
      fifoAppend(window.__ghostframeRecordedFlags, params.palRleFlag);
    }
    fifoAppend(window.__ghostframeRecordedTiles, {
      seq: params.seq,
      tileX: params.tileX,
      tileY: params.tileY,
      codec: params.codec,
      payloadLen: params.payloadLen,
      fbWidth: params.fbWidth,
      fbHeight: params.fbHeight,
      firstRecvMsClient: params.firstRecvMsClient,
      lastPaintMsClient: params.lastPaintMsClient,
    });
  }

  // ---- frame paint FIFO (M3.5 bench) -----------------------------------
  const framePaints: Array<{ seq: number; rafMsClient: number }> = [];
  if (typeof window !== 'undefined') {
    window.__ghostframe_framePaints = framePaints;
  }

  function recordFramePainted(params: { seq: number; rafMsClient: number }): void {
    fifoAppend(framePaints, params);
  }

  function recordRafTick(n: number): void {
    if (typeof window !== 'undefined') {
      window.__ghostframeRafTicks = n;
    }
  }

  return { stats, recordResize, recordTile, recordRafTick, recordFramePainted };
}
```

The old `palrle.ts` pushed to `__h2_clientPaletteWrites` with a manual `buf.length >= 2 * 32768` splice; `appendCapped` is the same rule.

- [ ] **Step 8: `stats_log.ts`.** Three edits. (1) Replace the import `import type { WebGpuRenderer } from './webgpu/renderer.js';` with:

```ts
/** The slice of `WebRenderer` this logger reads. */
export interface StatsRenderer {
  readonly fbWidth: number;
  readonly fbHeight: number;
  readonly submitCount: number;
}
```
(2) In `StatsLoggerDeps` change `renderer: WebGpuRenderer;` to `renderer: StatsRenderer;`. (3) Replace `const fb = renderer.framebuffer;` with `const fb = { width: renderer.fbWidth, height: renderer.fbHeight };`. Update the file's header comment line about `Δdrain` to read "`Δdrain`   events applied to the renderer per window, per codec (counted by main.ts)", and the `onTick` doc to "`applied` is the events applied since the last tick". `renderer.submitCount` already exists as a getter. Rename the first `onTick` parameter's doc only; the type `QueueDepths` stays.

- [ ] **Step 9: `globals.d.ts`.** Make these changes: change `__ghostframeRenderer: { device: GPUDevice; texture: GPUTexture } | undefined;` to `__ghostframeRenderer: { device: unknown; texture: { width: number; height: number } } | undefined;`; add `__ghostframePaintUs?: () => number[];` next to `__ghostframeRendererState`; delete the whole block `// ----- ?debug only (cdf53_debug.ts); absent in a normal session -----` (`__cdf53FullGridDiff` through `__cdf53Probe`); update the comment above `__ghostframeRendererState` to "`WebRenderer.debugState()`".

- [ ] **Step 10: `tests/diagnostics.test.ts`.** Delete the `fakeGetRenderer` function (lines 22 to 24) and change the three calls to `initDiagnostics({ canvas: fakeCanvas() })`:

```bash
cd ~/work/gf-web-renderer/ghostframe-web-client
sed -i '/^function fakeGetRenderer() {$/,/^}$/d' tests/diagnostics.test.ts
sed -i 's/initDiagnostics({ canvas: fakeCanvas(), getRenderer: fakeGetRenderer })/initDiagnostics({ canvas: fakeCanvas() })/' tests/diagnostics.test.ts
grep -n "fakeGetRenderer\|initDiagnostics" tests/diagnostics.test.ts
```

Expected: no `fakeGetRenderer` left; three `initDiagnostics({ canvas: fakeCanvas() })` calls.

- [ ] **Step 11: Typecheck now (the deletions come next).**

```bash
cd ~/work/gf-web-renderer/ghostframe-web-client
npm run build:wasm 2>&1 | tail -5; echo "rc=${PIPESTATUS[0]}"
npx tsc --noEmit 2>&1 | head -30; echo "rc=${PIPESTATUS[0]}"
```

Expected: the wasm builds (this is the first `wasm-pack build` with wgpu: it will take minutes) and `tsc` reports errors only from files Task 13 deletes (`src/webgpu/*`, `src/cdf53_debug.ts`) and nothing from `main.ts`, `diagnostics.ts`, `stats_log.ts`. Any error in those three is yours to fix now. Do not commit yet: the tree does not build until Task 13.

## Task 13 [haiku]: Delete the TypeScript renderer, the web-only shaders and the superseded tests

Run this task only after Tasks 1 to 12 are green. The e2e-test deletions are Task 14, not here.

**Files (delete):** `ghostframe-web-client/src/webgpu/` (all of it), `src/cdf53_debug.ts`, `src/frame_limits.ts`, `tests/renderer_paint_order.test.ts`, `tests/renderer_errors_readback.test.ts`, `tests/renderer_idle_skip.test.ts`, `tests/solid_pack.test.ts`, `tests/cdf53_dirty_inverse.test.ts`, `tests/cdf53_passes_processed.test.ts`, `tests/frame_limits.test.ts`, `shaders/client/cdf53_integrate.wgsl`, `shaders/client/cdf53_inverse_l1.wgsl`, `shaders/client/cdf53_inverse_l1_pass2.wgsl`, `shaders/client/cdf53_inverse_l2.wgsl`, `shaders/client/cdf53_inverse_l3.wgsl`.

- [ ] **Step 1: Prove nothing native uses the shaders.**

```bash
cd ~/work/gf-web-renderer
grep -rn "cdf53_integrate\|cdf53_inverse_l" --include=*.rs --include=*.ts --include=*.toml --include=*.sh --include=*.yml --include=Justfile --include=Dockerfile --include=*.py . 2>/dev/null | grep -v "^./target\|node_modules\|^./docs/"
```

Expected: only comments (in `ghostframe-client-core/tests/oracle_gpu_sparse.rs`, `tests/tile_delivery.rs`, `src/reassembly.rs`, and `ghostframe-render`/`client-gpu`'s `wgpu_ctx.rs` if it carried the "web client's cdf53_integrate.wgsl binds 7" remark) and the TS files being deleted. A match in code (an `include_str!`, a test listing shader names) is a stop: it is not web-only.

- [ ] **Step 2: Delete.**

```bash
cd ~/work/gf-web-renderer
git rm -r ghostframe-web-client/src/webgpu
git rm ghostframe-web-client/src/cdf53_debug.ts ghostframe-web-client/src/frame_limits.ts
git rm ghostframe-web-client/tests/renderer_paint_order.test.ts ghostframe-web-client/tests/renderer_errors_readback.test.ts \
       ghostframe-web-client/tests/renderer_idle_skip.test.ts ghostframe-web-client/tests/solid_pack.test.ts \
       ghostframe-web-client/tests/cdf53_dirty_inverse.test.ts ghostframe-web-client/tests/cdf53_passes_processed.test.ts \
       ghostframe-web-client/tests/frame_limits.test.ts
git rm shaders/client/cdf53_integrate.wgsl shaders/client/cdf53_inverse_l1.wgsl shaders/client/cdf53_inverse_l1_pass2.wgsl \
       shaders/client/cdf53_inverse_l2.wgsl shaders/client/cdf53_inverse_l3.wgsl
git status --short | head -40
```

- [ ] **Step 3: Fix the stale comments that named the deleted files.** In `ghostframe-client-core/tests/oracle_gpu_sparse.rs` (the doc lines naming `webgpu/cdf53.ts + cdf53_inverse_*.wgsl` and `cdf53_integrate.wgsl`), `tests/tile_delivery.rs` (line mentioning `cdf53_integrate.wgsl`), `src/reassembly.rs` (line mentioning `cdf53_integrate.wgsl`) and the remark in `ghostframe-client-gpu/src/wgpu_ctx.rs` ("the web client's cdf53_integrate.wgsl binds 7"; it stays in client-gpu with the device setup): rewrite each to name `shaders/client/cdf53_reconstruct.wgsl` (the grid) and, where the text says "the browser's GPU path", note that the browser now runs the same grid through `ghostframe-render`. Comments only; no code. Check with `git diff --stat` that only `//` lines changed in those files.

**Replacement table** (copy into the PR description; the file map row above points here). Every deleted vitest file has a named cover and each cover was shown failing on a mutation in the task named:

| Deleted vitest file | What covers it now |
|---|---|
| `renderer_paint_order` | Ledger tests in `ghostframe-render/src/paint_order.rs` (Task 5); `cdf53_state.rs` tests for cancelled and refused passes; PR 1's `tile_renderer` unit tests; the browser ordering test (Task 11 Step 2) |
| `renderer_errors_readback` | `errors_readback.rs` state-machine tests (Task 3); browser tests `palrle_errors_are_read_back_without_a_browser_error` and `a_palrle_error_on_a_static_screen_is_still_reported` (Task 11 Step 4) |
| `renderer_idle_skip` | `web/logic.rs` tests (Task 6); browser test `an_idle_tick_does_not_touch_the_gpu...` (Task 11 Step 3) |
| `solid_pack` | `pack_instances` test in `ghostframe-render/src/pipelines/solid.rs` (Task 5) |
| `cdf53_dirty_inverse` | **Obsolete**: it pinned the dirty-list inverse of the deleted multi-pass shaders. Resize repaint on the grid is PR 1's `a_resize_forgets_the_picture_and_damages_all_of_it` (a `tile_renderer` unit test) plus `ghostframe-client-gpu/tests/cdf53_grid.rs`; GPU against CPU exactness is Task 11 Step 5 |
| `cdf53_passes_processed` | **Obsolete**: the grid replaced the multi-pass inverse; its native twin was retired (Task 5) |
| `frame_limits` | `web/limits.rs` tests (Task 6) |

- [ ] **Step 4: Verify the web client still builds and the remaining vitest passes.**

```bash
cd ~/work/gf-web-renderer/ghostframe-web-client
npx tsc --noEmit 2>&1 | head -20; echo "rc=${PIPESTATUS[0]}"
npm test 2>&1 | tail -25; echo "rc=${PIPESTATUS[0]}"
```

Expected: `tsc` clean; vitest reports the remaining files passing (the seven deleted files are gone from the count). `tests/wasm_smoke.test.ts` and `tests/h264_*` exercise the nodejs wasm build, which now links wgpu's web glue: if loading `pkg-node` throws on a missing `GPU*` global, the glue is evaluating something at import time; report the first error, do not stub it.

Break it: the cross-checks that make the deletions safe are the replacement tests, already shown failing in Tasks 5 to 7 and 11; nothing to break here.

- [ ] **Step 5: Commit the TypeScript switch and the deletions together (one buildable commit).**

```bash
cd ~/work/gf-web-renderer
git add ghostframe-web-client/src/main.ts ghostframe-web-client/src/diagnostics.ts ghostframe-web-client/src/stats_log.ts \
        ghostframe-web-client/src/globals.d.ts ghostframe-web-client/tests/diagnostics.test.ts \
        ghostframe-client-core/tests/oracle_gpu_sparse.rs ghostframe-client-core/tests/tile_delivery.rs ghostframe-client-core/src/reassembly.rs \
        ghostframe-client-gpu/src/wgpu_ctx.rs
git status --short | grep -v '^D ' | head
git commit -m "feat(web): the page renders through WebRenderer; delete the TypeScript renderer

main.ts hands the core's tile events to the renderer inside wasm
(handleDatagramsInto/onTimeoutInto) and paints with paintTick. webgpu/*.ts,
cdf53_debug.ts, frame_limits.ts, the web-only multi-pass CDF 5/3 shaders and
the seven vitest files whose cases moved to Rust are deleted.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

`ghostframe-client-gpu/src/wgpu_ctx.rs` is in the list because Step 3 edits its comment (if `git status` shows it unchanged, drop it from the command); never `git add -A`.

## Task 14 [sonnet]: E2E harness changes, and the seven tests of deleted internals

Two separate things, both approved by the owner: a harness hook, and the deletion of seven tests.

**Files:**
- Modify: `ghostframe-e2e/src/harness/e2e_setup.rs`
- Modify: `ghostframe-e2e/tests/e2e.rs`, `ci/skip-list.txt`

- [ ] **Step 1: `GHOSTFRAME_E2E_URL_QUERY_EXTRA` (owner-approved).** In `ghostframe-e2e/src/harness/e2e_setup.rs` replace the `let page_url = { ... };` block (the one that builds `https://127.0.0.1:{}/?e2e=1&debug=1{sep}{suffix}`) with:

```rust
    let page_url = {
        // GHOSTFRAME_E2E_URL_QUERY_EXTRA (for example "&cdf53=cpu") is appended
        // after the caller's own suffix, so a run can change how the page
        // behaves without editing the test (the same idea as
        // GHOSTFRAME_SCENE_PAGE_QUERY in scene.rs).
        let from_env = std::env::var("GHOSTFRAME_E2E_URL_QUERY_EXTRA").unwrap_or_default();
        let query = url_query(spec.url_query_extra, &from_env);
        let sep = if query.is_empty() { "" } else { "&" };
        format!(
            "https://127.0.0.1:{}/?e2e=1&debug=1{sep}{query}",
            forwarder.port()
        )
    };
```

and add, next to the function, with its tests:

```rust
/// The caller's suffix and the environment's, joined by `&` with their leading
/// `&` removed and empty parts dropped.
fn url_query(caller: &str, from_env: &str) -> String {
    [caller, from_env]
        .iter()
        .map(|s| s.strip_prefix('&').unwrap_or(s))
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("&")
}

#[cfg(test)]
mod url_query_tests {
    use super::url_query;

    #[test]
    fn joins_and_strips_leading_ampersands() {
        assert_eq!(url_query("", ""), "");
        assert_eq!(url_query("&e2e=lossless", ""), "e2e=lossless");
        assert_eq!(url_query("", "&cdf53=cpu"), "cdf53=cpu");
        assert_eq!(url_query("&e2e=lossless", "&cdf53=cpu"), "e2e=lossless&cdf53=cpu");
        assert_eq!(url_query("a=1", "b=2"), "a=1&b=2");
    }
}
```

Also fix the comment above the block: `debug=1` is no longer read by the client (the CDF 5/3 GPU debug hooks went with the multi-pass shaders); it stays so saved URLs and logs do not change.

```bash
cd ~/work/gf-web-renderer
cargo test -p ghostframe-e2e --lib url_query 2>&1 | tail -6; echo "rc=${PIPESTATUS[0]}"
```

Expected: 1 passed. Break: in `url_query` remove the `strip_prefix`: the `&&` case fails. Revert.

- [ ] **Step 2: Delete the seven tests (owner-approved).** Delete these tests from `ghostframe-e2e/tests/e2e.rs`: `e2e_cdf53_bypass_integrate`, `e2e_cdf53_integrate_correctness`, `e2e_cdf53_live_tile_state_col18`, `e2e_cdf53_live_tile_state`, `e2e_cdf53_tile_watcher`, `e2e_cdf53_inverse_gradient_tile`, `e2e_cdf53_dirty_inverse_matches_full_grid` (and the helpers only they used). Each drove a hook that read the multi-pass shaders' coefficient, sign and integrate buffers or ran their inverse over the whole grid; none of those buffers exist in the grid. Do not stub the hooks to keep a test alive. Keep the table below: it goes into the PR body verbatim, because each deletion must name what now covers it.

| Deleted test | What it drove | What covers it now |
|---|---|---|
| `e2e_cdf53_bypass_integrate` | `__cdf53TestInverse`: the multi-pass inverse on fixture coefficients | The native grid oracle `ghostframe-client-gpu/tests/cdf53_grid.rs` (grid output against the CPU decoder), the grid's start-up self-test (`try_enable_cdf53_gpu`), and `e2e_lossless_golden_png` |
| `e2e_cdf53_integrate_correctness` | `__cdf53TestIntegrate`: `cdf53_integrate.wgsl`'s coefficient and sign buffers | The same oracle and self-test plus `cdf53_state.rs`'s pass bookkeeping tests (the grid keeps planes, not integrated coefficients); the lossless golden |
| `e2e_cdf53_live_tile_state` (`#[ignore]`) | `__cdf53DumpTileState`: a tile's GPU coefficient state under live load | PR 1's `DebugTile.cdf53` host-side state and `ghostframe-client-gpu/tests/cdf53_renderer.rs`; the lossless golden |
| `e2e_cdf53_live_tile_state_col18` (`#[ignore]`) | the same, one column | the same |
| `e2e_cdf53_tile_watcher` | `__cdf53GetTileWatcher`: JS-to-GPU upload capture for one tile | `cdf53_state.rs` (a tile is listed once per flush at its newest prefix, a duplicated pass uploads once) and `cdf53_renderer.rs`; the lossless golden |
| `e2e_cdf53_inverse_gradient_tile` | `__cdf53TestInverse` on one gradient tile | The grid oracle against the CPU decoder and the self-test; the lossless golden's gradient region |
| `e2e_cdf53_dirty_inverse_matches_full_grid` | `__cdf53FullGridDiff`: dirty-list inverse against a full-grid inverse | The grid has no dirty-list/full-grid pair; resize and damage are PR 1's `a_resize_forgets_the_picture_and_damages_all_of_it`, and a gradient building up to exact lossless is `e2e_lossless_golden_png` and `e2e_cdf53_lossless_buildup_chromium`; the browser test of Task 11 Step 5 checks GPU against CPU exactly |

```bash
cd ~/work/gf-web-renderer
python3 - <<'EOF'
import re
p = 'ghostframe-e2e/tests/e2e.rs'
names = [
    'e2e_cdf53_bypass_integrate', 'e2e_cdf53_integrate_correctness',
    'e2e_cdf53_live_tile_state_col18', 'e2e_cdf53_live_tile_state',
    'e2e_cdf53_tile_watcher', 'e2e_cdf53_inverse_gradient_tile',
    'e2e_cdf53_dirty_inverse_matches_full_grid',
]
lines = open(p).read().split('\n')
for name in names:
    start = next((i for i, l in enumerate(lines) if re.match(rf'async fn {name}\(', l)), None)
    assert start is not None, f'{name} not found (already deleted?)'
    top = start
    # attributes and doc comments directly above
    while top > 0 and (lines[top - 1].startswith('#[') or lines[top - 1].startswith('///')):
        top -= 1
    end = next(i for i in range(start, len(lines)) if lines[i] == '}')
    # drop one blank line after the item
    if end + 1 < len(lines) and lines[end + 1] == '':
        end += 1
    del lines[top:end + 1]
open(p, 'w').write('\n'.join(lines))
print('deleted', len(names))
EOF
for n in e2e_cdf53_bypass_integrate e2e_cdf53_integrate_correctness e2e_cdf53_live_tile_state_col18 e2e_cdf53_live_tile_state e2e_cdf53_tile_watcher e2e_cdf53_inverse_gradient_tile e2e_cdf53_dirty_inverse_matches_full_grid; do
  sed -i "/^$n\$/d" ci/skip-list.txt
done
grep -c "__cdf53TestInverse\|__cdf53TestIntegrate\|__cdf53DumpTileState\|__cdf53GetTileWatcher\|__cdf53FullGridDiff" ghostframe-e2e/tests/e2e.rs
cargo clippy -p ghostframe-e2e --all-targets -- -D warnings 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
```

Expected: `deleted 7`, a `0` from the grep, clippy clean. If clippy reports dead code (fixtures such as `CDF53_FIXTURE_JSON` used only by the deleted tests, or a helper), delete those too and re-run. `failure_dump.rs` keeps its guarded `__cdf53FullGridDiff` read (`if (!window.__cdf53FullGridDiff ...) return null`); PR 4 removes it with the other readers.

`ci/skip-list.txt` lists names for `cargo test --skip`; confirm `git diff ci/skip-list.txt` shows exactly the seven lines gone, and that the skip-list consistency check the repo runs (`grep -n skip-list .github/workflows/e2e.yml ci/*.sh`) still passes.

- [ ] **Step 3: Commit.**

```bash
cd ~/work/gf-web-renderer
git add ghostframe-e2e/src/harness/e2e_setup.rs
git commit -m "test(e2e): GHOSTFRAME_E2E_URL_QUERY_EXTRA appends to the page URL

So a run can ask the page for ?cdf53=cpu without editing a test.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
git add ghostframe-e2e/tests/e2e.rs ci/skip-list.txt
git commit -m "test(e2e): delete the seven tests of the old multi-pass CDF 5/3 internals

They drove hooks over the integrate and inverse buffers of the web-only
shaders, which the shared grid does not have. Coverage: ghostframe-client-gpu's
cdf53_grid and cdf53_renderer tests, PR 1's tile_renderer unit tests, the lossless golden e2e, and the browser
test that checks the GPU grid against the CPU decoder exactly.

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 15 [haiku]: Recipes, CI, docs

**Files:**
- Modify: `Justfile`, `.github/workflows/client-core.yml`, `AGENTS.md`, `DEVELOPERS.md`

- [ ] **Step 1: Justfile.** Next to `test-web-ui` add (use the browser invocation Task 1 settled on; if it needs Xvfb, write `xvfb-run -a wasm-pack test --chrome ghostframe-client-wasm`):

```make
# The browser renderer: the host tests of its decisions (router, idle skip,
# frame limits) and its browser tests (WebGPU in Chrome; every test file of
# the crate, so a new tests/*.rs is picked up without being named). Needs
# wasm-pack 0.15.0 and Chrome or Chromium.
test-web-renderer:
    cargo test -p ghostframe-client-wasm --lib
    wasm-pack test --headless --chrome ghostframe-client-wasm

# Clippy for the wasm32-only code, which the workspace clippy never compiles.
lint-web:
    cargo clippy -p ghostframe-client-wasm -p ghostframe-web-ui --target wasm32-unknown-unknown --all-targets -- -D warnings
    @out="$(cargo tree -p ghostframe-client-wasm --target wasm32-unknown-unknown -i naga 2>&1 || true)"; echo "$out"; if echo "$out" | grep -q '^naga v'; then echo "naga is in the wasm32 graph" >&2; exit 1; fi
```

In `ci-local`, after `just test-web-ui` add:

```make
    @echo "=== web renderer (clippy wasm32, host tests, headless Chrome) ==="
    just lint-web
    just test-web-renderer
```

and update the `ci-local` comment ("Also covers client-core.yml's stats panel job ...") to name both the stats panel and the web renderer jobs.

- [ ] **Step 2: CI.** Append a job to `.github/workflows/client-core.yml` after `web-ui` (same indentation; the existing jobs show the pattern):

```yaml
  web-renderer:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: dtolnay/rust-toolchain@stable
        with: { targets: wasm32-unknown-unknown }
      - name: Cache wasm-pack
        id: cache-wasm-pack
        uses: actions/cache@v6
        with:
          path: ~/.cargo/bin/wasm-pack
          key: wasm-pack-0.15.0-${{ runner.os }}
      - name: Install wasm-pack
        if: steps.cache-wasm-pack.outputs.cache-hit != 'true'
        run: cargo install wasm-pack --version 0.15.0 --locked
      - name: Clippy (wasm32) and no naga in the wasm graph
        run: |
          cargo clippy -p ghostframe-client-wasm --target wasm32-unknown-unknown --all-targets -- -D warnings
          out="$(cargo tree -p ghostframe-client-wasm --target wasm32-unknown-unknown -i naga 2>&1 || true)"
          echo "$out"
          if echo "$out" | grep -q '^naga v'; then echo "::error::naga is in the wasm32 graph"; exit 1; fi
      - name: Host tests (router, idle skip, frame limits)
        run: cargo test -p ghostframe-client-wasm --lib
      # Every test file of the crate runs (no --test list to forget): the
      # WebRenderer needs a WebGPU adapter, which webdriver.json asks Chrome
      # to provide in software.
      - name: WebRenderer browser tests
        run: wasm-pack test --headless --chrome ghostframe-client-wasm
```

(If Task 1 needed Xvfb, the last step is `xvfb-run -a wasm-pack test --chrome ghostframe-client-wasm`.) Also add `-p ghostframe-render` to the existing `cargo build ... --target wasm32-unknown-unknown` line of the `test` job if PR 1 did not.

- [ ] **Step 3: Docs.**

`DEVELOPERS.md`: change the `ghostframe-client-wasm` row of the clients table to: "The browser client's Rust side: the JS boundary (`WasmClientCore`) and its renderer (`WebRenderer`: wgpu's WebGPU backend over `ghostframe-render`, plus the raw WebGPU path that draws decoded H.264 `VideoFrame`s). `just test-web-renderer` runs its tests, including WebGPU in headless Chrome." Add `just test-web-renderer` and `just lint-web` to the list of recipes where DEVELOPERS.md names `just test-web-ui`.

`AGENTS.md`, in the paragraph beginning "Client-side diagnostics are printed by the browser console.", change "`Δdrain` and `nack_sent` are live." to: "`Δdrain` (events applied to the renderer in the window, per codec: counted by `main.ts` from the core's slim records) and `nack_sent` are live." and append after that paragraph:

```
The browser renderer is Rust (`WebRenderer`, `ghostframe-client-wasm/src/web/`);
the page URL steers it: `?cdf53=cpu` decodes CDF 5/3 on the CPU without trying
the grid (the e2e run that proves the fallback:
`GHOSTFRAME_E2E_URL_QUERY_EXTRA='&cdf53=cpu' cargo test --test e2e
e2e_lossless_golden_png -- --exact`). Under `?e2e`,
`window.__ghostframePaintUs()` returns `[count, p50, p99, max]` microseconds of
the painted ticks since the last call. The console line `CDF 5/3 decode on the
GPU (self-test passed ...)` or `... failed; decoding on the CPU` says which path
a session took.
```

- [ ] **Step 4: Run what changed.** If Task 1 settled on the host-GPU rung (no SwiftShader flags), `just test-web-renderer` and `just ci-local` touch the GPU and run as `~/work/gf-gpu-run just test-web-renderer`; on the SwiftShader rungs the wrapper is optional.

```bash
cd ~/work/gf-web-renderer
just lint-web 2>&1 | tail -8; echo "rc=${PIPESTATUS[0]}"
just test-web-renderer 2>&1 | tail -15; echo "rc=${PIPESTATUS[0]}"   # ~/work/gf-gpu-run just test-web-renderer on the host-GPU rung
ci/check-local-paths.sh; echo "rc=$?"
ci/check-env-reads.sh; echo "rc=$?"
ci/check-licenses.py; echo "rc=$?"
```

Expected all `rc=0`. Then break the CI-side guard once: add `naga = "30"` temporarily to `ghostframe-client-wasm`'s wasm32 dependencies and run `just lint-web`: it fails with "naga is in the wasm32 graph". Revert.

- [ ] **Step 5: Commit.**

```bash
cd ~/work/gf-web-renderer
git add Justfile .github/workflows/client-core.yml AGENTS.md DEVELOPERS.md
git commit -m "ci: the web renderer's lint, host tests and browser tests

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 16 [sonnet]: Build, measure the wasm, and STOP for the owner (Gate G2)

The spec expects wgpu's WebGPU backend to push the bundle past the 512 KiB cap. This task measures; it does not fix, and it does not raise the cap.

- [ ] **Step 1: Baseline from master.**

```bash
cd ~/work/ghostframe && git fetch origin
git worktree add ~/work/gf-wr2-base origin/master
cd ~/work/gf-wr2-base/ghostframe-web-client && npm install > /dev/null 2>&1; npm run build 2>&1 | tail -4; echo "rc=${PIPESTATUS[0]}"
f=$(ls dist/assets/*.wasm); echo "BASE raw=$(stat -c %s $f) gzip=$(gzip -c $f | wc -c)"
```

(Run `just build-web` from the worktree root instead if you prefer; it is `npm install && npm run build` in that directory.)

- [ ] **Step 2: This branch.**

```bash
cd ~/work/gf-web-renderer
just build-web 2>&1 | tail -6; echo "rc=${PIPESTATUS[0]}"
f=$(ls ghostframe-web-client/dist/assets/*.wasm); echo "BRANCH raw=$(stat -c %s $f) gzip=$(gzip -c $f | wc -c)"
ci/check-wasm-size.sh; echo "rc=$?"
strings -n 8 ghostframe-web-client/dist/assets/*.wasm | grep -ci "naga" 
```

Expected: `ci/check-wasm-size.sh` fails (`rc=1`) naming the size; the `naga` string count is small (a few mentions in panic messages at most; a large count means naga is linked: stop and find it). Record BASE and BRANCH raw and gzip bytes.

- [ ] **Step 3: What would shrink it, measured and thrown away (for the owner's decision; commit none of it).** Each in a scratch out-dir:

```bash
cd ~/work/gf-web-renderer
out=~/work/gf-wr2-size; mkdir -p $out
build() { # build <label> <extra cargo args...>
  label=$1; shift
  wasm-pack build ghostframe-client-wasm --target web --release --out-dir $out/$label -- "$@" 2>&1 | tail -2
  f=$out/$label/ghostframe_client_wasm_bg.wasm
  echo "$label raw=$(stat -c %s $f) gzip=$(gzip -c $f | wc -c)"
}
build default
build wgpu-s --config 'profile.release.package.wgpu.opt-level="s"'
build wgpu-and-render-s --config 'profile.release.package.wgpu.opt-level="s"' --config 'profile.release.package.ghostframe-render.opt-level="s"'
build wgpu-types-s --config 'profile.release.package.wgpu.opt-level="s"' --config 'profile.release.package.wgpu-types.opt-level="s"' --config 'profile.release.package.ghostframe-render.opt-level="s"'
# wasm-opt -Oz on the default build, with wasm-pack's own binaryen:
wo=$(ls ~/.cache/.wasm-pack/wasm-opt-*/bin/wasm-opt 2>/dev/null | head -1); echo "wasm-opt: ${wo:-not cached}"
[ -n "$wo" ] && { $wo -Oz -all $out/default/ghostframe_client_wasm_bg.wasm -o $out/default-Oz.wasm; echo "default+Oz raw=$(stat -c %s $out/default-Oz.wasm) gzip=$(gzip -c $out/default-Oz.wasm | wc -c)"; }
# What is big (twiggy is optional):
command -v twiggy >/dev/null && twiggy top -n 25 $out/default/ghostframe_client_wasm_bg.wasm
```

Notes: `opt-level = "s"` on the render path costs decode/flush CPU time, so any lever that touches `ghostframe-render` or `ghostframe-client-core` must be re-benched (Task 18) before the owner picks it; the existing size-optimised packages are the Leptos stack only, by the owner's 2026-11-13 decision. These numbers are advice, not a change.

- [ ] **Step 4: Record and STOP.** Add a section to `docs/specs/wasm-bundle-baseline.md`:

```markdown
## 2026-11-14: PR 2, the web renderer on wgpu

`ghostframe-client-wasm` now links wgpu's WebGPU backend, `ghostframe-render`,
and the CDF 5/3 grid (the TypeScript renderer is gone from the JS bundle).
Measured with `just build-web` on the branch and on master at <base rev>.

| Build | wasm raw | wasm gzip | JS bundle raw | JS bundle gzip |
|---|---:|---:|---:|---:|
| master | <n> | <n> | <n> | <n> |
| PR 2 | <n> | <n> | <n> | <n> |
| PR 2, wgpu `opt-level = "s"` | <n> | <n> | | |
| PR 2, wgpu + ghostframe-render `"s"` | <n> | <n> | | |
| PR 2, default + `wasm-opt -Oz` | <n> | <n> | | |

The 512 KiB (524,288 byte) cap in `ci/check-wasm-size.sh` is NOT changed by this
PR. Owner decision pending: <decision>.
```

Fill every `<n>` from Steps 1 to 3 (the JS bundle row is `ls -l dist/assets/*.js`). Do not edit `ci/check-wasm-size.sh` or `ci/test-check-wasm-size.sh`.

**STOP.** Report to the owner: the table, the largest contributors if `twiggy` ran, and the choices (raise the cap to N; keep 512 KiB and pick a lever; split the renderer into a lazily loaded second module, which is a larger change this plan does not cover). Until the owner answers, leave the PR as a draft, expect `just ci-local` to fail at the `wasm bundle under 512 KiB` step, and continue only with Tasks 17 and 18, which do not depend on the cap. Commit the baseline section with `<decision>` left as "pending the owner" only if it is the honest state:

```bash
cd ~/work/gf-web-renderer
git add docs/specs/wasm-bundle-baseline.md
git commit -m "docs: wasm bundle size with the wgpu web renderer (cap decision pending)

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

## Task 17 [sonnet]: The gates

Run in this order; each command's expected result is stated. GPU-touching ones go through `~/work/gf-gpu-run`; Docker ones take the heavy lock outside it. Capture the name of any failing test **before** re-running it (AGENTS.md: a stall flake and a regression look alike once the name is lost).

- [ ] **Step 1: `just ci-local`, expecting exactly one failure.**

```bash
cd ~/work/gf-web-renderer
~/work/gf-gpu-run just ci-local > ~/work/gf-wr2-ci-local.log 2>&1; echo "rc=$?"
grep -n '^=== \|::error::\|error:' ~/work/gf-wr2-ci-local.log | tail -40
```

Expected: `rc` non-zero, every `=== ... ===` section up to and including `web renderer (clippy wasm32, host tests, headless Chrome)` and `web client build` passes, and the only error is `::error::... is N bytes, over the 524288 byte (512 KiB) limit` at the `wasm bundle under 512 KiB` step (if the owner has already decided and the cap changed: no error at all). Everything after that step in `ci-local` did not run; run the remainder by hand:

```bash
cd ~/work/gf-web-renderer
cargo build --workspace --release --exclude ghostframe-e2e 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
cargo build --release -p ghostframe-server 2>&1 | tail -3; echo "rc=${PIPESTATUS[0]}"
ci/check-release-features.sh; echo "rc=$?"
ci/check-release-binary.sh; echo "rc=$?"
cargo check -p ghostframe-lib && git diff --exit-code ghostframe-lib/include/ghostframe.h; echo "rc=$?"
cargo check -p ghostframe-client-capi && git diff --exit-code ghostframe-client-capi/include/ghostframe_client.h; echo "rc=$?"
(cd ghostbridge && go vet ./... && go build ./... && go test ./... && go vet -tags noweb ./... && go build -tags noweb ./... && go test -tags noweb ./...); echo "rc=$?"
```

Expected all `rc=0`. Also `just ci-client` (the native client's gate: `ghostframe-render` extraction must not have moved a native behaviour) with `rc=0`.

- [ ] **Step 2: The native GPU tests of the shared crate** (they moved with PR 1 and Tasks 2 to 5 added to them):

```bash
cd ~/work/gf-web-renderer
~/work/gf-gpu-run cargo test -p ghostframe-render -p ghostframe-client-gpu 2>&1 | tail -20; echo "rc=${PIPESTATUS[0]}"
```

Expected `rc=0`. The `cdf53_grid` shaderdb line is unchanged because no shader changed.

- [ ] **Step 3: Build the images.**

```bash
cd ~/work/gf-web-renderer
flock -o ~/work/.gf-heavy.lock just containers-build 2>&1 | tail -6; echo "rc=${PIPESTATUS[0]}"
```

(`just test-e2e` runs `build-web containers-build` itself; this step is so a failure here is attributable. After any `ghostframe-lib`/`ghostframe-server` change it is mandatory; this PR changes neither, but the image embeds `dist/`.) If the disk fills, `docker system prune -f` is pre-authorised.

- [ ] **Step 4: The full Chromium e2e.**

```bash
cd ~/work/gf-web-renderer
flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run just test-e2e > ~/work/gf-wr2-e2e.log 2>&1; echo "rc=$?"
grep -n '^test .* \(FAILED\|failed\)\|^failures:\|test result:' ~/work/gf-wr2-e2e.log | tail -20
```

Expected: every test passes, with the known baseline exceptions of AGENTS.md and the memory notes (`_firefox` tests fail locally on every branch; `e2e_*_saturated_link*` fails on master by design). For each failure: **re-run it alone, then on master** (the `~/work/gf-wr2-base` worktree after `just build-web` and `containers-build` from there; tag collision: build the images from the worktree you are running) and record the pair. A test that fails here and passes on master is a regression of this PR. The ones this PR can change, which must pass: `e2e_lossless_golden_png` (exact), `e2e_readpixel_correctness`, `e2e_polled_globals_are_live`, `e2e_solid_per_tile_pixels`, `e2e_solid_color_chromium`, `e2e_palrle_exact_pixels`, `e2e_palrle_oob_index` (server log has `error_code=5`: the async errors readback end to end), `e2e_palette_eviction`, `e2e_palrle_session_reset` (chromium), `e2e_indices_raw_handshake`, `e2e_cdf53_lossless_buildup_chromium`, `e2e_cdf53_lossless_under_congestion_chromium`, `e2e_cdf53_mixed_codecs`, `e2e_cdf53_gradient_emission`, `e2e_progressive_refinement_chromium`, `e2e_mode_switch_chromium`, `e2e_h264_forced_solid_red`, `e2e_headroom_guard_forces_h264`, `e2e_loss_override_forces_h264`, `e2e_a_4k_screen_runs_on_the_gpu_path_and_converges`, `e2e_text_clarity`, `e2e_stats_panel_subscribe_samples_unsubscribe`, and the `e2e_production_*` scenes. (`e2e_h264_openh264_motion` is local-only and needs `libopenh264`; run it if the library is installed.)

- [ ] **Step 5: The golden through the CPU fallback, and proof that the knob is wired.** (Uses the `GHOSTFRAME_E2E_URL_QUERY_EXTRA` hook of Task 14 Step 1.)

```bash
cd ~/work/gf-web-renderer
for q in '&cdf53=cpu'; do
  GHOSTFRAME_E2E_URL_QUERY_EXTRA="$q" flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run \
    cargo test --test e2e e2e_lossless_golden_png -- --exact --test-threads=1 --nocapture 2>&1 | tee ~/work/gf-wr2-golden-$(echo $q | tr -dc a-z0-9).log | grep -n "page_url\|converged\|test result"
  echo "rc=${PIPESTATUS[0]}"
done
```

Expected: `page_url: https://127.0.0.1:.../?e2e=1&debug=1&e2e=lossless&cdf53=cpu` `converged after N polls`, `test result: ok`, `rc=0`. (`tee` is the last pipe stage's concern here; the `PIPESTATUS[0]` is the cargo run's status.)

**Break it** (once, to prove the CPU path is what these two runs covered): temporarily corrupt the CPU decoder, `rgba[0] ^= 1;` just before `Cdf53TileState::integrate` returns its picture (`ghostframe-client-core/src/cdf53_tile_state.rs`; only builds that decode on the CPU see it), rebuild (`just build-web`, `containers-build` is not needed because the page is embedded: it **is**, rebuild both), and run the golden twice: with no knob it must PASS (the GPU grid is untouched), with `&cdf53=cpu` it must FAIL on a pixel mismatch. Revert and rebuild. If the knob run passes with the corrupted decoder, the knob is not wired to the CPU path: fix `main.ts`'s URL read or `Cdf53Mode::parse`.

- [ ] **Step 6: Record** the results (pass/fail table, the master pairs for anything red) in the PR description. No commit.

## Task 18 [sonnet]: The paint-cost bench against master (section 7; Gate G3)

The spec: web paint time at 1080p and 4K on this host (one screen; owner decision: three runs each at 1920x1080 and 4096x2160), against master, naming `__ghostframe_framePaints`; a regression is reported with numbers before merge. That global is a coarse rAF-entry stamp (Findings), and timing `paintTick` alone would flatter the branch: on master the tiles are applied to the renderer's queues in `handleEvent` and *uploaded and drawn inside* `encodeAndPresentFrame`; on the branch they are applied inside `handleDatagramsInto`, before `paintTick` runs. So this task reports, on both sides, three things:

1. **The spec's metric**: `__ghostframe_framePaints` (`raf_ms - first_recv_ms`, p50 and p99), through `codec_report` at the 1080p-class `--tile-pattern` scenes, and computed from the same two globals inside the bench test at 4096x2160, which `codec_report` cannot reach.
2. **The whole rAF tick**: `window.__ghostframeTickUs()`, the main-thread CPU time of each rAF tick that presented a frame (datagram routing, event handling, apply, paint), plus datagram flushes made outside a tick. Identical TypeScript on both sides, so the move of tile application cancels. This is the comparable cost number.
3. **The paint step alone** (`__ghostframePaintUs()`): informational, labelled "not comparable alone".

Every cell where the branch is slower is reported with its numbers; whether any of them blocks the merge is the owner's call (open question 2).

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`, `src/globals.d.ts` (the tick timing; committed on the branch, patched into the base)
- Modify: `ghostframe-e2e/tests/e2e.rs` (one `#[ignore]` test, owner-approved)

- [ ] **Step 1: The tick timing in `main.ts` (not gated; it is TypeScript under `?e2e`).** Four edits, the same text on the branch and, later, on the base. (a) Before `function tick() {` insert:

```ts
  // Bench (?e2e only): main-thread CPU time of each rAF tick that presented a
  // frame -- the whole tick: datagram routing, event handling, apply, paint --
  // plus datagram flushes made outside a tick (backlog or timer). The same
  // lines run on master and on the branch, because the branch moves tile
  // application from the paint step into the datagram step: only the sum
  // compares.
  const tickUsSamples: number[] = [];
  let outOfTickUs = 0;
  let inTick = false;
  if (diagnostics) {
    window.__ghostframeTickUs = () => {
      const v = tickUsSamples.splice(0).sort((a, b) => a - b);
      const at = (q: number) => (v.length ? v[Math.round((v.length - 1) * q)] : 0);
      return [v.length, at(0.5), at(0.99), v.length ? v[v.length - 1] : 0];
    };
  }
```

(b) First lines of `tick()`, after `__rafTicks++;`: `const tickStartMs = performance.now(); inTick = true;`. (c) Replace the end of `tick()`, the text `    requestAnimationFrame(tick);\n  }\n  requestAnimationFrame(tick);`, with:

```ts
    inTick = false;
    if (diagnostics && painted > 0) {
      tickUsSamples.push(Math.round((performance.now() - tickStartMs) * 1000 + outOfTickUs));
    }
    outOfTickUs = 0;
    requestAnimationFrame(tick);
  }
  requestAnimationFrame(tick);
```

(d) Rename `function flushDatagrams(): void {` to `function flushDatagramsInner(): void {` and add above it:

```ts
  function flushDatagrams(): void {
    const t0 = performance.now();
    try {
      flushDatagramsInner();
    } finally {
      if (!inTick) outOfTickUs += (performance.now() - t0) * 1000;
    }
  }
```

(`painted` is the number of frames presented this tick on both sides: the branch's `let painted`, master's `const painted = renderer.submitCount - submitsBefore`.) Add `__ghostframeTickUs?: () => number[];` next to `__ghostframePaintUs` in `globals.d.ts`.

```bash
cd ~/work/gf-web-renderer/ghostframe-web-client && npx tsc --noEmit; echo "rc=$?"
cd ~/work/gf-web-renderer && git add ghostframe-web-client/src/main.ts ghostframe-web-client/src/globals.d.ts
git commit -m "feat(web): e2e-only whole-tick timing (window.__ghostframeTickUs)

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

- [ ] **Step 2: The measurement test (owner-approved).** Append to `ghostframe-e2e/tests/e2e.rs` (next to `e2e_a_4k_screen_runs_on_the_gpu_path_and_converges`, whose server setup it reuses):

```rust
/// Section 7 bench of the all-Rust web client spec. On demand, once per label
/// and mode:
///
///   GF_BENCH_MODE=4096x2160 GF_BENCH_SECS=30 \
///     cargo test --test e2e e2e_bench_web_paint_us -- --ignored --exact --nocapture
///
/// Prints (1) the spec's `__ghostframe_framePaints` latency, `raf - first
/// receipt` per frame, from the two globals `codec_report` reads (so it works
/// at any screen size), (2) the CPU time of the whole rAF tick
/// (`__ghostframeTickUs`) and (3) the paint step alone (`__ghostframePaintUs`),
/// each as [count, p50, p99, max]. The run includes the start-up burst, a
/// whole-screen CDF 5/3 repaint.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "bench: run on demand, see the doc comment"]
async fn e2e_bench_web_paint_us() -> Result<()> {
    let mode = std::env::var("GF_BENCH_MODE").unwrap_or_else(|_| "1920x1080".to_string());
    let secs: u64 = std::env::var("GF_BENCH_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let setup = setup_e2e_webgpu_gpu_with_env(
        "--gradient --x11-sink",
        &[
            ("GHOSTFRAME_ENABLE_CDF53", "1"),
            ("GHOSTFRAME_FORCE_TILECODEC", "1"),
            ("GHOSTFRAME_DRM_MODE", &mode),
            ("CAPTURE_FPS", "30"),
        ],
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(secs)).await;

    let stats = |name: &'static str| {
        let js = format!("window.{name} ? window.{name}() : null");
        let page = setup.page();
        async move { page.evaluate(js).await?.into_value::<Vec<f64>>().map_err(anyhow::Error::from) }
    };
    let tick = stats("__ghostframeTickUs").await?;
    let paint = stats("__ghostframePaintUs").await?;

    // The spec's metric, computed the way scene.rs's read_client_diagnostics does.
    let tiles_json: String = setup
        .page()
        .evaluate("JSON.stringify(window.__ghostframeRecordedTiles || [])")
        .await?
        .into_value()?;
    let paints_json: String = setup
        .page()
        .evaluate("JSON.stringify(window.__ghostframe_framePaints || [])")
        .await?
        .into_value()?;
    let tiles: Vec<serde_json::Value> = serde_json::from_str(&tiles_json)?;
    let paints: Vec<serde_json::Value> = serde_json::from_str(&paints_json)?;
    let mut first_recv = std::collections::BTreeMap::<u64, f64>::new();
    for t in &tiles {
        let (Some(seq), Some(v)) = (t["seq"].as_u64(), t["firstRecvMsClient"].as_f64()) else { continue };
        let e = first_recv.entry(seq).or_insert(f64::INFINITY);
        if v < *e {
            *e = v;
        }
    }
    let mut lat: Vec<f64> = paints
        .iter()
        .filter_map(|p| {
            let (seq, raf) = (p["seq"].as_u64()?, p["rafMsClient"].as_f64()?);
            let f = *first_recv.get(&seq)?;
            (f > 0.0 && raf >= f).then_some((raf - f) * 1000.0)
        })
        .collect();
    lat.sort_by(|a, b| a.total_cmp(b));
    let at = |q: f64| lat.get(((lat.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0.0);
    eprintln!("BENCH mode={mode} framePaints_us count={} p50={} p99={}", lat.len(), at(0.5), at(0.99));
    eprintln!("BENCH mode={mode} tick_us count={} p50={} p99={} max={}", tick[0], tick[1], tick[2], tick[3]);
    eprintln!("BENCH mode={mode} paint_step_us count={} p50={} p99={} max={}", paint[0], paint[1], paint[2], paint[3]);
    assert!(tick[0] > 0.0 && !lat.is_empty(), "the bench observed nothing: tick {tick:?}, {} frame latencies", lat.len());
    Ok(())
}
```

```bash
cd ~/work/gf-web-renderer
cargo clippy -p ghostframe-e2e --all-targets -- -D warnings 2>&1 | tail -4; echo "rc=${PIPESTATUS[0]}"
git log -1 --format=%h
git add ghostframe-e2e/tests/e2e.rs
git commit -m "test(e2e): on-demand bench of the web client's frame and tick cost

Co-Authored-By: Claude Sonnet 5.5 <noreply@anthropic.com>"
```

- [ ] **Step 3: The baseline: master plus the same measurements in TypeScript, never committed.**

```bash
cd ~/work/gf-wr2-base
git cherry-pick --no-commit $(cd ~/work/gf-web-renderer && git log -1 --format=%H -- ghostframe-web-client/src/main.ts) 2>&1 | tail -3
git status --short
```

(That commit is the tick-timing one of Step 1, which applies to master's `main.ts` except that master's `tick()` ends with `statsLog.onTick(beforeDrain, __rafTicks);`; if the cherry-pick conflicts, apply edits (a) to (d) of Step 1 by hand.) Add the test function by hand from Step 2 (or `git cherry-pick --no-commit` the gated commit if it exists). Then add the paint-step timing to master's `main.ts`: declare `const paintUsSamples: number[] = [];` before `function tick()` and, around the existing call,

```ts
    const submitsBefore = renderer.submitCount;
    const paintStartMs = performance.now();
    try {
      renderer.encodeAndPresentFrame((codec, tx, ty, code) => {
        reportDecodeError(codec, tx, ty, code);
      });
    } catch (e) {
      health.record('encodeAndPresentFrame', e);
      console.error('encodeAndPresentFrame failed; continuing', e);
    }
    if (renderer.submitCount > submitsBefore) {
      paintUsSamples.push(Math.round((performance.now() - paintStartMs) * 1000));
    }
```

(the existing `submitsBefore` declaration above the `try` stays; do not declare it twice), and, once, in `main()` after the diagnostics are initialised:

```ts
  if (url.searchParams.has('e2e')) {
    window.__ghostframePaintUs = () => {
      const v = paintUsSamples.splice(0).sort((a, b) => a - b);
      const at = (q: number) => (v.length ? v[Math.round((v.length - 1) * q)] : 0);
      return [v.length, at(0.5), at(0.99), v.length ? v[v.length - 1] : 0];
    };
  }
```

plus `__ghostframePaintUs?: () => number[];` in `globals.d.ts`.

- [ ] **Step 4: Run it: base first (one image tag at a time), then the branch.** Per label: the three e2e bench runs per size, and three `codec_report` runs for the spec's metric at its 1080p-class scenes.

```bash
out=~/work/gf-wr2-bench; mkdir -p $out
run() { # run <label> <worktree>
  label=$1; wt=$2
  ( cd $wt && just build-web > /dev/null 2>&1 && flock -o ~/work/.gf-heavy.lock just containers-build > /dev/null 2>&1 ) || { echo "build $label failed"; return 1; }
  for mode in 1920x1080 4096x2160; do
    for n in 1 2 3; do
      ( cd $wt && GF_BENCH_MODE=$mode GF_BENCH_SECS=30 flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run \
          cargo test --test e2e e2e_bench_web_paint_us -- --ignored --exact --nocapture 2>&1 ) > $out/$label-$mode-$n.log
      grep -h "^BENCH" $out/$label-$mode-$n.log || echo "$label $mode run $n: NO RESULT (see the log)"
    done
  done
  for n in 1 2 3; do
    ( cd $wt && flock -o ~/work/.gf-heavy.lock ~/work/gf-gpu-run \
        cargo run --release -p ghostframe-bench --bin codec_report -- --scenes photo,motion,mode_switch \
        --scene-duration 20s --output $out/$label-report-$n.md 2>&1 ) > $out/$label-report-$n.log || echo "$label codec_report run $n failed"
  done
}
run base ~/work/gf-wr2-base
run branch ~/work/gf-web-renderer
```

Expected: six `BENCH` triples per label. A run with no result line is a failed run: re-run it, do not drop it. The three runs of one cell should agree within noise; if they scatter by more than 2x, add three more. The `codec_report` markdown's section 2 holds the per-scene `client p50/p99` (that is `__ghostframe_framePaints`).

- [ ] **Step 5: Compare and report; decide nothing.** Tables, median of the three runs per cell, microseconds:

| mode | label | framePaints p50 | framePaints p99 | tick p50 | tick p99 | tick max | paint step p50 | paint step p99 |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 1920x1080 | base | | | | | | | |
| 1920x1080 | branch | | | | | | | |
| 4096x2160 | base | | | | | | | |
| 4096x2160 | branch | | | | | | | |

plus the `codec_report` `client p50/p99` per scene, base against branch. **Report every cell in which the branch is slower, with its numbers and the three raw runs, to the owner before merge (Gate G3); the plan sets no threshold.** If the owner asks for tuning, the levers in order are: the grid's cost model and tile cap (`WebRenderer` uses `try_enable_cdf53_gpu`'s defaults: swap it for `cdf53_grid::probe_grid(..)` plus `enable_cdf53_gpu_with(device, grid, Some(us_per_tile), 60_000)` with a per-tile figure measured in the browser, which is what native Vulkan does), then the number of `queue.submit`s per tick (the present is its own submit; folding it into `flush`'s encoder is a `ghostframe-render` change). Do not tune on a single run. The paint-step column is not comparable alone: master's includes tile upload and draw, the branch's does not include the apply.

Clean up: `cd ~/work/gf-wr2-base && git checkout -- . && git status --short` (nothing left).

## Task 19 [haiku]: Open the PR as a draft

- [ ] **Step 1: Final local check.**

```bash
cd ~/work/gf-web-renderer
git status --short
git log --oneline origin/master..HEAD
git diff --stat origin/master..HEAD | tail -5
just fmt-check; echo "rc=$?"
ci/check-local-paths.sh; echo "rc=$?"
```

Expected: a clean tree, one commit per task, `rc=0`. `just fmt` if rustfmt objects, and commit the formatting alone.

- [ ] **Step 2: Push and open a draft** (the ssh agent is outside the sandbox: `dangerouslyDisableSandbox` with `SSH_AUTH_SOCK=/run/user/1000/ssh-agent.socket`, per the memory note). The PR body ends with the attribution line given for pull requests.

```bash
cd ~/work/gf-web-renderer
git push -u origin feat/web-renderer-wgpu
gh pr create --draft --base master --title "feat(web): all-Rust web client PR 2: the web renderer on wgpu" --body-file ~/work/gf-wr2-pr-body.md
```

The body (`~/work/gf-wr2-pr-body.md`, written from this plan, not committed) lists: what changed (the file map), the contract lines PR 1 had to be adapted to, the behaviour changes (R4), the table mapping each vitest case to its Rust home (Task 5), the e2e results and master pairs (Task 17), the golden through `?cdf53=cpu`, the bench table (Task 18), the wasm size table with **the cap decision requested** (Task 16), the e2e harness changes, and the seven deleted e2e tests, each listed with what now covers it (copy the table of Task 14 Step 2), and the globals this PR does not port because they have no reader (`__ghostframeRecordedFlags` is read; none dropped in PR 2). Last line: `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.

- [ ] **Step 3: Keep it a draft** until the owner has decided the wasm cap (open question 1) and has seen the bench report (open question 2).

## Self-review: spec coverage

| Spec item | Where |
|---|---|
| 5.2 `async create(canvas)`, adapter retry, device, surface in its preferred format, self-test and CPU fallback | Task 9 (`create_inner`, `try_enable_cdf53_gpu` from PR 1), Task 6 (`pick_canvas_format`, `Cdf53Mode`) |
| 5.2 events from TS through one wasm-bindgen call, tile bytes never in JS | Tasks 7, 10 (`handleDatagramsInto`, `onTimeoutInto`) |
| 5.2 `paint_tick` with idle skip, flush then pictures then present | Tasks 6, 9, test in Task 11 |
| 5.2 `resize`, `on_session_reset`, `h264_supported`, debug reads | Task 9 |
| 5.3 raw pipeline from `h264_blit.wgsl`, `importExternalTexture`, raw pass into the framebuffer view via `as_webgpu`, raw submit, close, `note_h264`, stale frames dropped, probe | Tasks 8, 9 |
| 5.5 renderer-backed hooks `__ghostframeRenderer`, `__ghostframeRendererState`, `__ghostframeTile` (plus `__readPixel*`, `__readGradientGolden`, `__h2_clientPaletteWrites` which the renderer's deletion orphaned) | Tasks 9, 12 |
| 6 PR 2 scope: TS calls `WebRenderer`; grid on web; H.264 escape; delete `webgpu/*.ts`, web-only shaders, `cdf53_debug.ts`, renderer-reading `diagnostics.ts` | Tasks 9 to 13 |
| 6 PR 2 gate: full Chromium e2e, golden exact, CDF 5/3, PalRLE, H.264, mode switch | Task 17 |
| 6 PR 2 gate: bench | Task 18 |
| 6 PR 2 gate: wasm size reported | Task 16 (STOP) |
| 7 renderer equivalence: golden exact; golden through the CPU fallback (the "forced failure" is `?cdf53=cpu`, see Task 11 Step 5) | Task 17 Step 5, Task 11 Step 5 |
| 7 H.264 ordering test and its breakage | Task 11 Step 2 |
| 7 performance against master, 1080p and 4K, three runs | Task 18 |
| 7 wasm size, owner decides the cap | Task 16 |
| 7 habits: every moved test shown failing on a mutation in its new home | "Break it" step in Tasks 2 to 7, 9, 11, 14, 15 |
| 8 PR 2 rows: `cdf53_dirty_inverse` obsolete, `cdf53_passes_processed` obsolete (native twin retired), `renderer_errors_readback` browser, `renderer_idle_skip` moved, `renderer_paint_order` moved, `solid_pack` moved | Tasks 5, 6, 11, 13 (`frame_limits` moved here too, because the router needs it) |

Task count: 19 live tasks (Tasks 0 to 19; Task 4 was withdrawn when PR 1 shipped the async self-test). Gates that remain: G0 (Task 0), G2 (Task 16), G3 (Task 18). G1, G1b and G1c were answered yes.
