# Ghostframe Implementation Plan: M0–M3

**Date:** 2026-04-11
**Status:** Design approved
**Approach:** Vertical slice — build the thinnest end-to-end pipe first, then widen it

---

## Context

- **Source spec:** `docs/specs/ghostframe-initial-spec.md`
- **High-level design:** `docs/design/high-level-design.md`
- **GPU hardware:** AMD/Intel with VA-API
- **Working style:** Solo, 4–8 hour sessions
- **Client target:** Browser-first (WebTransport + WebCodecs + WebGPU)
- **Build approach:** Vertical slice A — each milestone produces something testable end-to-end, then widens

## The Vertical Slice Ladder

| Milestone | What you see in the browser | What's proved |
|-----------|----------------------------|---------------|
| **M0: QUIC Ping** | Browser console logs "pong" received over WebTransport | The I/O bridge (libtailscale ↔ quinn-proto) works. Highest risk (R1) validated. |
| **M1: Raw Bytes** | Browser receives raw pixel bytes, draws them on canvas | Datagram fragmentation/reassembly works. Frame sequencing works. DRM capture to pixels. |
| **M2: H.264 + Skip** | Motion looks decent via VA-API H.264; static areas waste zero bandwidth | End-to-end GPU encode, tile diff, skip unchanged tiles, XDamage integration. |
| **M3: Adaptive Codecs + Refinement** | Text is crisp (palettized), colors are solid (4 bytes), idle regions sharpen to pixel-perfect over seconds | Two-axis classification, hysteresis, palette tables, CDF 5/3 progressive refinement, round-robin scheduling. |

Key property: every milestone is testable end-to-end. M0 is reachable in ~2 sessions. M2 is the spec's M1. M3 encompasses the spec's M2 and M3.

---

## Workspace Structure

```
ghostframe/
├── Cargo.toml                  # Workspace root
├── Justfile                    # Build + test commands
├── ghostframe-lib/             # Core library (cdylib + staticlib + rlib)
│   └── src/
│       ├── lib.rs              # Pub API (GhostframeServer, submit_frame, etc.)
│       ├── ffi.rs              # extern "C" wrapper (M1+)
│       ├── transport/
│       │   ├── mod.rs
│       │   ├── tailscale_ffi.rs   # C FFI to libtailscale
│       │   ├── quic.rs            # quinn-proto state machine driver
│       │   ├── io_bridge.rs       # Event loop: tailscale ↔ quinn-proto
│       │   ├── webtransport.rs    # HTTP/3 + WT handshake
│       │   ├── protocol.rs        # Datagram/stream format definitions
│       │   └── bandwidth.rs       # GCC-inspired estimator (M4+)
│       ├── capture/
│       │   ├── dmabuf.rs          # Vulkan DMA-BUF import + SAD compute
│       │   └── shaders/
│       ├── tile/
│       │   └── mod.rs             # Two-axis classification, hysteresis
│       ├── encoder/
│       │   ├── h264_vaapi.rs
│       │   ├── pal_rle.rs
│       │   ├── bc1.rs
│       │   ├── cdf53.rs
│       │   └── solid.rs
│       ├── audio.rs               # M5+
│       ├── display.rs            # Display layout negotiation
│       └── input.rs              # Input event types + forwarding
├── ghostframe-xdaemon/         # X11 headless daemon
│   └── src/
│       ├── main.rs
│       ├── drm_capture.rs
│       ├── xdamage.rs
│       ├── xinput.rs
│       └── xrandr.rs
├── ghostframe-web-client/      # Browser client
│   ├── src/
│   │   ├── main.ts             # WebTransport connection
│   │   ├── decoder.ts          # Tile reassembly, WebCodecs
│   │   ├── renderer.ts         # Canvas/WebGPU compositing
│   │   └── input.ts             # Pointer Lock + Keyboard Lock
│   └── wasm/                   # Rust WASM for tile decode
│       └── src/lib.rs
├── ghostframe-native-client/   # Native client (post-M3)
├── ghostframe-test-pattern/   # X11 test pattern app
├── tests/
│   ├── e2e/
│   │   ├── mod.rs              # Shared helpers (container setup, headscale)
│   │   ├── ping.rs             # M0: QUIC ping test
│   │   ├── raw_frame.rs        # M1: raw pixel round-trip
│   │   ├── solid_color.rs      # M2: solid fill + skip
│   │   ├── h264_motion.rs      # M2: H.264 tiles
│   │   ├── codec_transition.rs # M3: hysteresis state machine
│   │   ├── text_clarity.rs     # M3: palettized + refinement
│   │   └── multi_pattern.rs    # M3: multi-codec classification
│   ├── fixtures/               # Reference images for E2E
│   └── containers/
│       ├── test-server/
│       │   ├── Dockerfile
│       │   └── entrypoint.sh
│       ├── test-client/
│       │   ├── Dockerfile
│       │   └── entrypoint.sh
│       └── headscale/
│           ├── Dockerfile (or use upstream image)
│           └── config/
└── docs/
    ├── specs/
    ├── design/
    └── superpowers/specs/
```

---

## M0: QUIC Ping + Automated E2E Test

**Goal:** Prove that a browser can talk to our server over Tailscale through quinn-proto driven by libtailscale's userspace netstack.

### Server deliverables

- `ghostframe-lib/src/transport/tailscale_ffi.rs` — C FFI bindings to libtailscale: `ts_new`, `ts_listen`, `ts_read_udp`, `ts_write_udp`, `ts_close`. Embedded Tailscale node authenticates via `TS_AUTHKEY`, listens inside tailnet, provides UDP read/write through userspace netstack.
- `ghostframe-lib/src/transport/quic.rs` — Quinn-proto state machine driver. `Endpoint::handle()` accepts incoming bytes; `Connection::poll_transmit()` produces outgoing bytes.
- `ghostframe-lib/src/transport/io_bridge.rs` — The event loop that bridges the two: read from libtailscale → feed quinn-proto → drain outgoing from quinn-proto → write via libtailscale. No kernel sockets.
- `ghostframe-lib/src/transport/webtransport.rs` — HTTP/3 + WebTransport handshake over quinn-proto.
- `ghostframe-lib/src/transport/protocol.rs` — Minimal: ping/pong datagram format (4-byte payload).
- `ghostframe-xdaemon/src/main.rs` — Calls `gf_server_new()` (starts Tailscale node + QUIC listener), then sleeps. No capture, no video.
- `Cargo.toml` workspace with `ghostframe-lib` and `ghostframe-xdaemon` crates.
- `build.rs` in `ghostframe-lib` that builds libtailscale from Go source (`go build -buildmode=c-archive`) and links against it.

### Client deliverables

- `ghostframe-web-client/src/main.ts` — Opens WebTransport session to `https://<tailnet-hostname>.ts.net/`, sends "ping" datagram, receives "pong", logs success.
- `ghostframe-web-client/package.json` + build setup (Vite or similar).

### Test infrastructure

- `tests/containers/test-server/Dockerfile` — Ubuntu + Tailscale CLI + ghostframe-xdaemon binary.
- `tests/containers/test-client/Dockerfile` — Ubuntu + Chromium + Tailscale CLI + remote debugging.
- `tests/containers/headscale/` — headscale configuration (one user, one policy, pre-auth key generation).
- `Justfile` with: `just build`, `just test-unit`, `just test-e2e`, `just containers-build`.
- `tests/e2e/ping.rs` — Automated E2E test:
  1. `testcontainers-rs` spins up Docker network: headscale, test-server, test-client.
  2. Server container authenticates to headscale, starts daemon.
  3. Client container authenticates, launches Chromium with remote debugging.
  4. `playwright-rust` connects via CDP, navigates to WebTransport URL.
  5. JavaScript sends 4-byte "ping" datagram.
  6. Server echoes "pong".
  7. Playwright asserts "pong" was received.
  8. Containers torn down.

### What M0 explicitly does NOT build

No GPU code, no DMA-BUF, no Vulkan, no VA-API, no WebCodecs, no canvas rendering, no tile engine, no protocols beyond ping/pong.

### M0 completion gate

The automated `tests/e2e/ping.rs` test passes. This proves: Tailscale auth works, QUIC handshake works, WebTransport framing works, datagram round-trip works, and the I/O bridge is functional.

---

## M1: Raw Bytes

**Goal:** Capture a real frame via DRM/KMS, break it into 32×32 tiles, ship raw pixels over QUIC datagrams, and paint them on a browser canvas.

### Server deliverables

- `ghostframe-lib/src/capture/dmabuf.rs` — Vulkan DMA-BUF import. Reads pixel data back to CPU (no compute shaders yet — `vkMapMemory`). Creates `FrameSubmission` structs from the DMA-BUF.
- `ghostframe-lib/src/transport/protocol.rs` — Full datagram format: 12-byte header (frame_seq, frag_idx, frag_total, timestamp_us) + per-tile 8-byte header (tile_x, tile_y, codec=u7+lz4=u1, generation, payload_len) + variable payload. Fragmentation for payloads >MTU. Only codec 5 (Raw) is implemented.
- `GhostframeServer::new()`, `submit_frame()` — Real Rust API. `gf_server_submit_frame()` C FFI function.
- `ghostframe-xdaemon/src/drm_capture.rs` — Open `/dev/dri/card0`, `drmModeGetFB2()` → `drmPrimeHandleToFD()` → DMA-BUF fd, per-frame capture loop calling `gf_server_submit_frame()`.
- `ghostframe-xdaemon/src/main.rs` — Full capture loop: startup → wait for client → capture DRM → submit frame → repeat.
- `ghostframe-test-pattern/` — X11 test pattern app (`x11rb` crate) that draws deterministic pixel patterns on Xvfb. `--solid-red` draws a 200×200 red square. Used by E2E tests to generate known visual content.

### Client deliverables

- `ghostframe-web-client/src/main.ts` — WebTransport datagram reading, fragment reassembly, tile identification.
- `ghostframe-web-client/src/decoder.ts` — Receives tile data, dispatches by codec ID. Only codec 5 (Raw) implemented — uncompressed pixels.
- `ghostframe-web-client/src/renderer.ts` — Canvas 2D with `putImageData()` per tile. No WebGPU yet.

### E2E test (`tests/e2e/raw_frame.rs`)

1. Same testcontainers + headscale setup from M0.
2. Server: `ghostframe-test-pattern --solid-red` draws a red square on Xvfb.
3. xdaemon captures and sends raw tile data.
4. Client: Playwright screenshot, assert pixel at center of red square is red ±10.

Requires: GPU runner with `/dev/dri` access (VA-API). Test is marked `#[ignore]` in CI without GPU.

### M1 completion gate

`tests/e2e/raw_frame.rs` passes. Real pixels from DRM framebuffer appear correctly in the browser, transported over QUIC datagrams with fragmentation.

---

## M2: H.264 + Skip

**Goal:** Hardware-encode H.264 via VA-API, decode in browser via WebCodecs, and skip sending unchanged tiles.

### Server deliverables

- `ghostframe-lib/src/encoder/h264_vaapi.rs` — VA-API H.264 encoding. DMA-BUF import into VA surface, encode with: CBR, no B-frames, infinite GOP + rolling intra-refresh, ultra-low latency preset, VBV buffer = bitrate/fps. NAL units → datagrams.
- `ghostframe-lib/src/capture/dmabuf.rs` — Vulkan compute shader for tile diff (SAD comparison). When no damage rects provided, GPU diff shader runs; when damage rects provided, they're mapped directly to the tile grid.
- `ghostframe-lib/src/tile/mod.rs` — Initial: tile grid management, dirty tile detection, Skip codec. No adaptive classification — tiles are "dirty" or "skip". Every dirty tile gets H.264 encoded.
- `ghostframe-xdaemon/src/xdamage.rs` — XDamage integration. Collects damage rects from root window, maps to tile grid.

### Critical integration: Vulkan → VA-API zero-copy

Both Vulkan (tile diff) and VA-API (H.264 encode) import the same DMA-BUF. Zero copies between them. Synchronization via Linux kernel DMA-BUF fencing (implicit sync).

### Client deliverables

- `ghostframe-web-client/src/decoder.ts` — WebCodecs `VideoDecoder` for H.264 NAL units. Decoded `VideoFrame` objects composited into canvas via `drawImage()`.
- `ghostframe-web-client/src/renderer.ts` — Composites H.264 decoded regions alongside previously-drawn tile pixels. Skip tiles persist from last frame.

### E2E tests

| Test | Validates |
|------|-----------|
| `e2e_solid_color` | Red square renders. Skip tiles for static areas. |
| `e2e_tile_skip` | Two screenshots 2s apart are identical outside changed region. |
| `e2e_h264_motion` | Spinner region changes between screenshots 500ms apart. |
| `e2e_codec_transition` | Spinner stops, H.264 tiles transition to Skip. |

### M2 completion gate

All four M2 E2E tests pass on GPU runner. H.264 hardware encode validates the zero-copy DMA-BUF pipeline. Skip tiles validate dirty tile detection works.

---

## M3: Adaptive Codecs + Progressive Refinement

**Goal:** Full tile classification, all codecs, the build-to-lossless refinement engine.

### Server deliverables

- `ghostframe-lib/src/tile/mod.rs` — Full two-axis classification (frequency × magnitude). Per-tile `TileMetrics`: change_freq_hz, change_magnitude, unique_colors, edge_density, idle_frames, codec_state. Hysteresis with asymmetric thresholds.
- `ghostframe-lib/src/encoder/pal_rle.rs` — Palettized RLE. Shared palette table (256 × 16 RGBA), LRU eviction, nibble-packed encoding (4-bit color index + 4-bit run length).
- `ghostframe-lib/src/encoder/solid.rs` — Solid fill, 4 bytes per tile.
- `ghostframe-lib/src/encoder/bc1.rs` — BC1 (DXT1) GPU texture compression via Vulkan compute shader. 512 bytes per 32×32 tile.
- `ghostframe-lib/src/encoder/cdf53.rs` — CDF 5/3 integer wavelet forward transform via Vulkan compute shader. Bit-plane extraction, progressive pass generation, per-tile generation tracking.
- **Refinement scheduler** — Round-robin across idle tiles sorted by idle duration. Bandwidth-aware pacing (refinement bandwidth fraction = 0.2). Generation-based infinite retry. ACK-driven pass advancement.
- `ghostframe-lib/src/transport/protocol.rs` — ACK messages on control stream (`tile(x,y) gen=N pass=K`, 8 bytes). Refinement datagram format. Priority scheduler (8 priority levels).

### Client deliverables

- `ghostframe-web-client/wasm/src/lib.rs` — Rust→WASM for PalRLE decode and Solid fill.
- `ghostframe-web-client/src/decoder.ts` — ACK stream: tracks received wavelet passes per tile per generation, sends 8-byte ACKs on control stream. Skips out-of-order and stale-generation passes.
- `ghostframe-web-client/src/renderer.ts` — PalRLE and Solid tiles render via WASM → `putImageData()`. BC1 and CDF 5/3 inverse temporarily rendered as Raw fallback (WebGPU compute decode arrives post-M3).

### Codec benchmark suite

- Criterion.rs benchmarks for each codec across content types: solid color, flat UI, text on background, photographic, gradient, high-motion video frame.
- Each codec measured with and without LZ4 post-compression.
- Results determine: (a) per-codec LZ4 cost/benefit break-even, (b) whether CDF 5/3 should replace BC1 entirely for the lossy-to-lossless path.
- Benchmarks run on real GPU hardware.

### E2E tests

| Test | Validates |
|------|-----------|
| `e2e_solid_color` | Solid fill codec, minimal bandwidth |
| `e2e_text_clarity` | Palettized RLE + CDF 5/3 refinement to lossless. SSIM >0.99 after 5s idle. |
| `e2e_tile_skip` | Static regions produce zero traffic |
| `e2e_h264_motion` | High-frequency motion activates H.264 |
| `e2e_codec_transition` | Spinner stops → H.264 hysteresis → BC1 → palettized → refinement to lossless |
| `e2e_multi_pattern` | Multiple content types on screen, each tile classified correctly |
| `e2e_progressive_refinement` | SSIM monotonically increases at 1s, 3s, 5s intervals on idle tiles |

### M3 completion gate

All seven E2E tests pass. Codec benchmarks are published. The two-axis classification engine, hysteresis, palette table, and progressive refinement are validated end-to-end.

---

## Post-M3 (Not Detailed Here)

The following are tracked as milestones but not decomposed into session-length tasks in this plan. M1–M3 learnings will shape their design:

- **M4:** Multi-monitor + dynamic display reconfiguration (EDID, EVDI, xrandr)
- **M5:** Audio pipeline + bandwidth adaptation + final polish
- **M6:** Production hardening (NVENC, Enlightenment module, clipboard, fuzzing, profiling)

---

## Key Decisions

| # | Decision | Rationale |
|---|----------|-----------|
| D1 | M0 is QUIC ping, not GPU capture | Validate highest risk (I/O bridge) in 2 sessions before building GPU pipelines |
| D2 | M1 sends raw pixels, no encoding | Proves transport + capture + fragmentation without GPU encode complexity |
| D3 | M2 classifies tiles as dirty/skip only | Simplest state machine that proves skip bandwidth savings and H.264 encode path |
| D4 | M3 adds full adaptive classification | The tile engine's complexity (two-axis, hysteresis, palette table, refinement) is isolated in one milestone |
| D5 | Browser client from M0 | Server and client evolve together; protocol issues surface early |
| D6 | VA-API first, NVENC later | Matches available AMD/Intel GPU hardware |
| D7 | Canvas 2D rendering until M3, WebGPU decode later | Canvas 2D is sufficient for raw pixels and H.264; WebGPU compute shaders for BC1/CDF 5/3 inverse can be added incrementally |
| D8 | Automated E2E from M0 | Testing infrastructure is part of the vertical slice, not bolted on later |