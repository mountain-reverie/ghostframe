# Design: an all-Rust web client

**Status:** design, not implemented. **Date:** 2026-11-14 · **Base:** master at bcc59e03

## 1. Goal

Remove the remaining TypeScript from `ghostframe-web-client` while keeping
every feature, so the repository has one language on the client side (the
owner's decision of 2026-11-12; the stats panel was the first step). The
web renderer moves onto the native client's wgpu renderer, which becomes
shared code. Replacing ghostbridge (Go) with tailscale-rs is a separate,
later spec.

Success means:

- `ghostframe-web-client/` holds only `index.html`, CSS and the built `dist/`;
  there is no `package.json`, no Node toolchain in CI, Docker or
  DEVELOPERS.md.
- The web and native clients draw tiles with the same Rust renderer code
  (`ghostframe-render`).
- The Chromium e2e suite passes unchanged at every step: the e2e Rust code is
  not edited to accommodate the port (except deleting readers of globals that
  were already dead).
- Web paint time holds today's numbers at 1080p and 4K (bench, section 7).

## 2. What is there today

About 4,400 lines of TypeScript source and 3,000 of vitest tests (master at
bcc59e03). The wire protocol is already Rust (`WasmClientCore` in
`ghostframe-client-wasm`, the stats panel in `ghostframe-web-ui`). The
TypeScript is browser glue, the renderer and debug hooks:

| Area | Files | Lines |
|---|---|---|
| WebGPU renderer | `webgpu/*` (renderer, cdf53, palrle, solid, framebuffer, h264, init, paint_order, idle_skip, shader_module) | ~1,950 |
| Main loop, I/O | `main.ts` (WebTransport datagrams and streams, the rAF loop, ACK writer, overlays), `server_stream.ts` | ~870 |
| Debug hooks for e2e | `diagnostics.ts`, `cdf53_globals.ts`, `cdf53_debug.ts`, `stats_log.ts`, `health.ts` | ~1,060 |
| H.264 decode | `decoder.ts` (WebCodecs `VideoDecoder`) | ~220 |
| Input | `input/wire.ts` | ~170 |
| Small modules | `bootstrap`, `protocol_constants`, `frame_limits`, `close_message`, `disconnect_overlay`, `globals.d.ts` | ~280 |

The native client (`ghostframe-client-gpu`, wgpu 30) already renders the same
codecs from the same `ghostframe_client_core::Event`/`TileData`. Its
pipelines (`solid`, `palrle`, `rgba`, `cdf53_grid`, `cdf53_state`),
`framebuffer`, `paint_order`, `dirty` and `coalesce` use no threads, ffmpeg
or dmabuf; the export ring, dmabuf import/export, upload slabs and the
ffmpeg H.264 path are native-only. Its CDF 5/3 decode is a single-pass GPU
grid with a start-up self-test and a CPU fallback (`Cdf53TileState`); the
web's is a multi-pass inverse (`cdf53_integrate`, `cdf53_inverse_*`).

## 3. Decisions (owner, 2026-11-13/14)

1. **Share the renderer with native.** Extract the portable part of
   `ghostframe-client-gpu` into a crate both clients use; the web adopts the
   native CDF 5/3 grid.
2. **H.264 through a raw WebGPU escape, zero copy.** wgpu cannot import a
   `VideoFrame` as an external texture on the web, so one web-only pipeline
   uses web-sys directly on wgpu's own device (section 5.3).
3. **Piece by piece.** Five PRs, each shippable with e2e green (section 6).
4. **Build with the wasm-bindgen CLI and `just`**, no wasm-pack, no Trunk,
   no npm (section 5.6).

## 4. Crates after the move

| Crate | Role | Targets | Licence |
|---|---|---|---|
| `ghostframe-render` (new) | Portable tile renderer: framebuffer, solid, PalRLE, RGBA, CDF 5/3 grid and state, paint order, damage. `TileRenderer` applies `client_core::Event`s, records draws and submits on `flush()`. | native, wasm32 | MIT OR Apache-2.0 |
| `ghostframe-client-gpu` | Native wrapper: `Renderer` owns a `TileRenderer` and adds the export ring, dmabuf, upload slabs, the ffmpeg H.264 decode and NV12 blit. Public API unchanged. | native | MIT OR Apache-2.0 |
| `ghostframe-client-wasm` | The whole web client: `WebRenderer` over `TileRenderer`, the H.264 escape, WebCodecs, WebTransport I/O, the rAF loop, input, overlays, e2e hooks. | wasm32 | MIT OR Apache-2.0 |
| `ghostframe-web-ui` | Unchanged: Leptos, DOM UI only. | wasm32 | MIT OR Apache-2.0 |
| `ghostframe-web-client/` | `index.html`, CSS, built `dist/` (ghostbridge's `//go:embed all:dist` path is unchanged). | — | MIT OR Apache-2.0 |

Leptos stays confined to `ghostframe-web-ui` (memory of the 2026-11-12
decision): `ghostframe-render` and the video path never depend on it.

## 5. Design

### 5.1 `ghostframe-render`

Moved from `ghostframe-client-gpu` with history (`git mv`): `framebuffer.rs`,
`paint_order.rs`, `dirty.rs`, `coalesce.rs`, `pipelines/{solid, palrle, rgba,
cdf53_grid, cdf53_state}.rs`, and the part of `renderer.rs` that applies tile
events and flushes. The tests that cover them move along.

Constraints, so it builds for `wasm32-unknown-unknown`:

- Depends on `wgpu` (workspace version), `ghostframe-protocol`,
  `ghostframe-client-core`, `thiserror`, `tracing`. No `pollster`, `libc`,
  `ash`, `khronos-egl`, ffmpeg or `std::thread`.
- No `std::time::Instant`: every time it needs (the CDF 5/3 per-flush GPU
  budget, summaries) comes in as a parameter (`now_us: u64`) or through
  `web_time::Instant` behind the same cfg that quinn-proto uses (see the
  memory note on `web_time`).
- Device creation is the caller's: `TileRenderer::new(device, queue,
  format, size)`. wgpu's device request is async on the web and blocking
  (`pollster`) on native; both callers already have their own.
- The framebuffer format is a parameter: `Bgra8Unorm` for the native export,
  the canvas's preferred format on the web.

API (names indicative; the plan fixes them):

- `apply_event(&mut self, ev: Event) -> Result<(), GpuError>`: the tile
  codecs (Solid, PalRLE, CDF 5/3, RGBA), session reset, resize. H.264
  events are not handled here (each client has its own path), but the paint
  ledger's `note_h264(seq)` / `h264_stale(seq)` are exposed, so both clients
  apply the same supersede rules.
- `flush(&mut self, now_us) -> Result<FlushReport, GpuError>`: encode and
  submit; returns the damage and whether deferred CDF 5/3 work remains
  (carried over to the next flush, as native does today).
- `framebuffer(&self) -> &wgpu::Texture`.
- `debug_tile(x, y)` and `debug_state()` return serde structs (used by the
  e2e hooks, section 5.5).
- The CDF 5/3 self-test (`enable_cdf53_gpu`) and the CPU fallback move too.

`ghostframe-client-gpu::Renderer` keeps its public API and delegates.
`GHOSTFRAME_CDF53_DECODE=cpu` keeps working natively. The shaders stay in
`shaders/client/` and are included with `include_str!` from the new crate.

### 5.2 `WebRenderer` (in `ghostframe-client-wasm`)

- `async create(canvas)`: requests the adapter and device through wgpu's
  WebGPU backend (`wgpu` with the `webgpu` and `wgsl` features only, which
  should keep naga out of the wasm; PR 2 checks it with `cargo tree
  --target wasm32-unknown-unknown -i naga`). A missing WebGPU is fatal, as today
  (design D2). Configures the canvas surface in its preferred format. Runs
  the CDF 5/3 self-test; on failure it logs, as native does, and decodes on
  the CPU (`Cdf53TileState`).
- Events: once the main loop is in Rust (PR 3), the core's events go
  straight to the renderer inside wasm and tile data never crosses into JS.
  During PR 2 the TypeScript passes them through a wasm-bindgen method.
- `paint_tick(now_us)`: `flush()`, then the pending H.264 picture (5.3),
  then the present blit of the framebuffer to the canvas
  (`present_blit.wgsl`, kept as web-only). Skips encode and present when
  nothing changed, as `idle_skip.ts` does.
- `resize`, `on_session_reset`, `h264_supported`, and the debug reads of
  section 5.5.

### 5.3 H.264: the raw WebGPU escape

wgpu 30 exposes the web handles: `Device::as_webgpu()`,
`Queue::as_webgpu()`, `TextureView::as_webgpu()`. The escape (`web/h264.rs`,
`cfg(target_arch = "wasm32")`, raw web-sys) uses them, never wgpu internals:

- At creation: one raw `GPURenderPipeline` built from `h264_blit.wgsl`
  (`texture_external`), on `device.as_webgpu()`.
- Per decoded `VideoFrame`: the paint ledger decides staleness first
  (`h264_stale(seq)`; a stale frame is closed and dropped). Otherwise
  `importExternalTexture`, a raw render pass into the framebuffer's view
  (`framebuffer().create_view(..).as_webgpu()`), a raw submit on
  `queue.as_webgpu()`, `frame.close()`, then `note_h264(seq)`.
- Ordering: wgpu's WebGPU backend hands each `queue.submit` to the same
  `GPUQueue` immediately, so `flush()` → H.264 submit → present stays in
  order. A test asserts it (section 7).
- `probeH264` (`VideoDecoder.isConfigSupported`) moves with it.

### 5.4 Main loop and I/O (PR 3)

`#[wasm_bindgen(start)]` replaces `main.ts`:

- Startup: the `?e2e` and `?e2e=lossless` flags (the latter hides the page
  chrome and pins the canvas top-left), `WebRenderer::create`, `bootstrap`
  (`/config.json`, cert hash, same-origin `WebTransport`), the stats panel
  mount (a direct Rust call).
- WebTransport through web-sys' unstable API and `wasm-bindgen-futures`: one
  task reads datagrams into `ClientCore`, one pumps the feedback stream's
  return half (as `server_stream.ts` does); the ACK and NACK writers are
  unchanged in behaviour. Datagram bytes are copied once into wasm memory;
  no JS arrays, so the V8 `Array.shift` cliff cannot return.
- The rAF tick: a `Closure` over `Rc<RefCell<...>>` drains the core, runs
  `paint_tick`, re-arms. The hidden-tab case keeps a timer fallback, since
  the tick is the only drain.
- WebCodecs: `VideoDecoder` driven from Rust; `output` hands the
  `VideoFrame` to the escape with its `frame_seq`; an error maps to the
  existing 0x09 decoder-failure request; the per-chunk `frame_seq` tagging
  of `decoder.ts` (and its test) is kept.
- Input: `input/wire.ts` becomes Rust listeners; the keysym mapping moves
  with its tests; encoding uses `INPUT_MSG_TYPE` from `ghostframe-protocol`.
- Overlays: disconnect overlay and close message as plain web-sys DOM.
- What remains of `main.ts` is the module script tag in `index.html`
  calling `init()`.

### 5.5 E2E debug hooks (PR 4)

`debug_hooks.rs` installs, only under `?e2e`, every `window.__ghostframe*`
value the e2e suite reads, with the same name and JSON shape, through
`js_sys::Reflect`. The list is taken from the readers in `ghostframe-e2e`
and `ghostframe-bench` with grep, not from the TypeScript. Today's readers
include `__ghostframeRenderer.texture.{width,height}`,
`__ghostframeRendererState`, `__ghostframeTile`, `__ghostframeHealth`,
`__ghostframeStats`, `__ghostframeRafTicks`, `__ghostframeRecordedTiles`,
`__ghostframeRecordedCodecs`, `__ghostframeRecordedResizes`,
`__ghostframe_framePaints`, `__ghostframeStatsFrames` and
`__ghostframeStatsSamples`.

A global with no reader is not ported; it is listed in the PR. The
already-dead console diagnostics (`rx{...}` and `cdf53-coverage`, reading
`__tileCounts` / `__cdf53Coverage`) are deleted, and AGENTS.md's note about
them goes.

During PR 2, the renderer-backed hooks (`__ghostframeRenderer`,
`__ghostframeRendererState`, `__ghostframeTile`) are already served by
`WebRenderer`, since the TypeScript renderer they read is gone.

### 5.6 Build (PR 5)

`just build-web`:

1. `cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown
   --release` (the per-package `opt-level` overrides stay).
2. `wasm-bindgen --target web --out-dir <tmp>`. The recipe fails, naming
   both versions, if the installed CLI is not the `wasm-bindgen` version in
   `Cargo.lock`.
3. `wasm-opt -O` (binaryen, pinned version), as today: what wasm-pack
   0.15.0 ran by default (a same-commit build came within 68 bytes).
4. Copy `index.html`, CSS and the bindgen output into
   `ghostframe-web-client/dist/`.

`ci/check-wasm-size.sh` reads the new output. CI and the test-server
Dockerfile drop Node and npm and install `wasm-bindgen-cli` and `binaryen`
at pinned versions. `ci-local` drops `tsc` and vitest. DEVELOPERS.md and
AGENTS.md describe the new prerequisites (no Node).

Deleted: `package.json`, `package-lock.json`, `tsconfig*.json`,
`vite.config.ts`, `vitest.config.ts`, `scripts/protocol_stamp.*`, `src/`,
`tests/`.

## 6. PR series

Each PR is shippable: e2e green, `just ci-local` green, the daemon serves a
working client.

| PR | Scope | Gate |
|---|---|---|
| 1 | Extract `ghostframe-render`; native behaviour unchanged. CI adds `cargo check -p ghostframe-render --target wasm32-unknown-unknown`. | Native GPU tests, CDF 5/3 self-test and oracle tests, `cdf53_grid` panfrost shaderdb line unchanged, clippy. |
| 2 | Web renderer swap: TS calls `WebRenderer`; CDF 5/3 grid on web; H.264 escape; delete `webgpu/*.ts`, the web-only CDF 5/3 shaders, `cdf53_debug.ts` and the renderer-reading parts of `diagnostics.ts`. | Full Chromium e2e (lossless golden exact, CDF 5/3, PalRLE, H.264 local-only set, mode switch); bench (section 7); wasm size reported to the owner. |
| 3 | Main loop, WebTransport, rAF, WebCodecs, input, overlays, bootstrap in Rust. | Full e2e; the moved tests (section 8). |
| 4 | E2E hooks from Rust; delete dead globals and the TS diagnostics. | Full e2e unchanged; a `wasm-bindgen-test` asserting each hook exists under `?e2e`. |
| 5 | Remove npm: the build recipe, CI, Docker, docs; port or retire the remaining vitest. | `just ci-local`, CI, e2e; `git ls-files ghostframe-web-client` lists no `.ts`. |

## 7. Testing

- **Renderer equivalence (PR 2).** The lossless golden e2e is the exact
  gate for CDF 5/3 and PalRLE on Chrome's WebGPU. The native oracle tests
  stay the exact gate natively. A run with the self-test forced to fail must
  still pass the golden (CPU fallback), proving the fallback path on the
  web.
- **H.264 ordering.** A `wasm-bindgen-test` in headless Chrome paints a tile
  at seq N, an H.264 picture at N+1 and a tile at N+2, then reads the
  framebuffer back: the result must show N+2 over N+1 over N. Breaking the
  order (submitting the H.264 pass before `flush()`) must turn it red.
- **Performance.** Before merging PR 2, `ghostframe-bench`'s web paint time
  (`__ghostframe_framePaints`) at 1080p and 4K on this host, against master,
  three runs each. A regression is reported to the owner before merge, with
  numbers.
- **Wasm size.** wgpu's WebGPU backend will push the bundle past the
  512 KiB cap. PR 2 measures it (raw and gzip) and the owner decides on the
  cap with that number; the cap is not raised without that decision.
- **Habits.** As in AGENTS.md: break the code and watch each new test fail;
  every moved test is shown failing on a mutation in its new home.

## 8. Test migration

Each vitest file has one fate; none disappears without a replacement or a
written reason.

| vitest file | Fate | Where |
|---|---|---|
| `bootstrap` | browser | `wasm-bindgen-test` (PR 3) |
| `cdf53_dirty_inverse` | obsolete | the multi-pass inverse is deleted; the grid's damage tests in `ghostframe-render` cover resize repaint (PR 2) |
| `cdf53_globals` | browser | with the hooks (PR 4) |
| `cdf53_passes_processed` | moved | Rust unit test next to the port of `computePassesProcessed`, or obsolete if the grid has no equivalent (decided in PR 2's plan) |
| `close_message` | moved | Rust unit test (PR 3) |
| `datagram_batch` | moved | `ghostframe-client-wasm` / core unit test (PR 3) |
| `diagnostics` | browser | with the hooks (PR 4) |
| `eviction` | moved | core unit test (PR 3) |
| `frame_limits` | moved | Rust unit test (PR 3) |
| `h264_ack`, `h264_result`, `reset_request` | moved | core unit tests (PR 3); they test the wasm core, not TS |
| `h264_frame_seq` | browser | with the WebCodecs port (PR 3) |
| `health` | moved | Rust unit test (PR 4) |
| `input` | moved | Rust unit test of the keysym mapping (PR 3) |
| `protocol_constants`, `protocol_literals` | obsolete | they stop TS from retyping Rust constants; no TS is left |
| `renderer_errors_readback` | browser | `WebRenderer` (PR 2) |
| `renderer_idle_skip` | moved | Rust unit test (PR 2) |
| `renderer_paint_order` | moved | `ghostframe-render` unit tests on the shared ledger (PR 2) |
| `sanity`, `wasm_smoke` | obsolete | the build itself and the hook test replace them (PR 5) |
| `server_stream` | browser | (PR 3) |
| `solid_pack` | moved | `ghostframe-render` unit test, if not already covered natively (PR 2) |
| `tile_assembly` | moved | core unit test (PR 3) |

## 9. Risks

- **Wasm size** (section 7). Mitigation: `webgpu`-only wgpu features, no
  naga, per-package `opt-level`; the owner decides the cap.
- **CDF 5/3 grid on the web** is a shader change for web users. The golden
  e2e is exact; the self-test and CPU fallback cover a browser whose WebGPU
  fails it.
- **`as_webgpu` is a newer wgpu API.** If a wgpu upgrade changes it, only
  `web/h264.rs` is affected.
- **Hidden-tab and timing behaviour** of the rAF loop is subtle (the tick is
  the only drain). PR 3 keeps the timer fallback and its e2e.
- **The e2e suite only sees what the hooks report** (memory: e2e is blind to
  feedback-path bugs). PR 3's moved core tests check the feedback bytes
  directly.

## 10. Out of scope

- tailscale-rs replacing ghostbridge, and with it the Go `//go:embed`.
- Converging the native and web paths beyond the extraction (for example a
  shared H.264 path).
- Performance work beyond holding today's numbers.
