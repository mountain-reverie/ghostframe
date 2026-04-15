# Ghostframe High-Level Design

**Purpose:** This document distills the Ghostframe specification into a structured high-level design. It identifies every subsystem, their responsibilities, boundaries, and inter-dependencies so that component-level and step-by-step design can proceed without ambiguity.

**Source:** `docs/specs/ghostframe-initial-spec.md` v0.1-draft

---

## 1. System Summary

Ghostframe is a Linux-only remote desktop protocol that streams a composited desktop to a remote client, using adaptive per-tile encoding (32×32 pixel tiles), zero-copy GPU pipelines, QUIC transport over an embedded Tailscale mesh, and a browser-based client. The core insight is that typical desktop content is ~80% text/UI, ~15% static images, and ~5% motion — each tile is independently classified and encoded with the optimal codec.

**Key invariant:** The server binary never calls `bind()` on any kernel socket. All network I/O flows through an embedded Tailscale node in userspace. The server is unreachable outside the tailnet.

---

## 2. Architecture Overview

```
┌──────────────────────────────────────────────────────────────────┐
│ Server Host                                                       │
│                                                                   │
│  ┌──────────────┐    ┌────────────────────────────────────────┐  │
│  │  Compositor   │    │        ghostframe-lib (Rust)           │  │
│  │  (Any)        │───▶│                                       │  │
│  │              │    │  Capture ──▶ Tile Engine ──▶ Encoder    │  │
│  │  Provides:   │    │     Pipeline        Pipeline            │  │
│  │  - DMA-BUF   │    │                                       │  │
│  │  - Damage    │    │            ┌─────────────┐             │  │
│  │    rects     │    │            │  Transport   │             │  │
│  └──────────────┘    │            │  (QUIC/      │             │  │
│                      │            │   WebTransport│             │  │
│  ┌──────────────┐    │            │   over        │             │  │
│  │  ghostframe- │    │            │   Tailscale)  │             │  │
│  │  xdaemon     │───▶│            └──────┬───────┘             │  │
│  │  (X11                │                   │                     │  │
│  │   headless)   │    │            ┌──────┴───────┐             │  │
│  └──────────────┘    │            │   Audio       │             │  │
│                      │            │   (PipeWire/  │             │  │
│                      │            │    ALSA)      │             │  │
│                      │            └──────────────┘             │  │
│                      └────────────────────────────────────────┘  │
└──────────────────────────────────────┬───────────────────────────┘
                                       │
                          QUIC/UDP over Tailscale WireGuard
                                       │
┌──────────────────────────────────────┬───────────────────────────┐
│  Browser Client                      │  Native Client            │
│  (WebTransport + WebCodecs + WebGPU) │  (libtailscale + quinn)   │
└──────────────────────────────────────┴───────────────────────────┘
```

---

## 3. Subsystem Decomposition

The system decomposes into **10 subsystems**, each with a clear boundary and responsibility. Subsystems marked with **(core lib)** are implemented inside `ghostframe-lib`; others are separate binaries/modules.

### 3.1 Capture Pipeline **(core lib)**
**Responsibility:** Import DMA-BUF framebuffers from the compositor, run GPU compute to detect changed tiles when damage rects are not provided, and produce a dirty tile set with per-tile metrics.

| Aspect | Detail |
|--------|--------|
| Input | `RawFd` (DMA-BUF), `FrameFormat`, optional `Vec<Rect>` damage, `output_id`, `timestamp_ns` |
| Output | Set of dirty 32×32 tiles with `TileMetrics` (change frequency, magnitude, unique colors, edge density) |
| GPU work | Vulkan compute shader: tile diff via SAD comparison (<0.5ms @ 1080p) |
| Key contract | If damage rects are provided, skip GPU diff entirely; trust compositor damage |

**External dependencies:** Vulkan (`ash`), DRM DMA-BUF import (`VK_EXT_external_memory_dma_buf`)

### 3.2 Tile Engine **(core lib)**
**Responsibility:** Classify each dirty tile into a codec based on two-axis metrics (frequency × magnitude), manage hysteresis state machines, track idle frames, and manage the progressive refinement queue.

| Aspect | Detail |
|--------|--------|
| Input | Per-tile `TileMetrics` from Capture Pipeline |
| Output | Per-tile `CodecState` decision (Skip / H.264 / BC1 / PalRLE / Solid / CDF53 / Raw) |
| Key behavior | Asymmetric hysteresis prevents rapid codec oscillation (enter H.264: 10 frames sustained; exit: 30 frames sustained) |
| Refinement queue | Sorted by idle duration; round-robin scheduling across all idle tiles needing wavelet passes |

**External dependencies:** None (pure CPU logic)

### 3.3 Encoder Pipeline **(core lib)**
**Responsibility:** Encode each tile's pixel data using the codec selected by the Tile Engine. Produce compressed tile payloads ready for QUIC serialization.

| Sub-encoder | GPU/CPU | Output size (32×32 tile) | When used |
|-------------|---------|--------------------------|-----------|
| H.264 (VA-API/NVENC) | GPU | Variable (CBR) | High-freq, high-magnitude motion |
| Palettized RLE | CPU | 64B (solid) – 1024B (worst) | ≤16 unique colors, text/UI |
| Solid Fill | CPU | 4B | Single-color tiles |
| BC1 (DXT1) | GPU compute | 512B | General lossy, photographic |
| CDF 5/3 wavelet | GPU compute | ~128B/pass, 1-1.3KB total | Progressive lossy→lossless refinement |
| Raw (+optional LZ4) | CPU | ~1-3KB after LZ4 | Final pixel-perfect state |

**Key design decision:** CDF 5/3 over CDF 9/7 — integer-only arithmetic enables exact lossless reconstruction. No rounding error accumulates.

**Shared palette table:** Up to 256 palettes of 16 RGBA colors each, referenced by 1-byte `palette_id` in PalRLE tiles. LRU eviction when table is full; fallback to CDF 5/3 or BC1.

**External dependencies:** VA-API (`cros-libva`), NVIDIA Video Codec SDK, Vulkan compute (`ash`), LZ4 (`lz4_flex`)

### 3.4 Transport **(core lib)**
**Responsibility:** Manage QUIC connections, WebTransport sessions, stream multiplexing, datagram fragmentation, priority scheduling, and the I/O bridge between `libtailscale` and `quinn-proto`.

| Sub-component | Role |
|---------------|------|
| `libtailscale` FFI | C FFI to embedded Go Tailscale node (WireGuard, control plane, NAT traversal, DERP relay) |
| `quinn-proto` driver | Drive the QUIC state machine with no I/O — accept byte buffers, produce byte buffers |
| I/O bridge event loop | Single loop: read UDP from libtailscale → feed quinn-proto → drain outgoing packets → write via libtailscale. No kernel sockets. |
| WebTransport framing | HTTP/3 + WebTransport handshake for browser clients; raw QUIC for native clients |
| Stream/dgram multiplexer | 8 priority levels (Input > Video > Audio > Cursor > Control > Extensions > Clipboard > Refinement) |
| Datagram fragmentation | Split payloads >MTU into fragments; reassemble on client; generation counters for invalidation |

**Key invariant:** The `GhostframeServer` binary never binds a kernel socket. All UDP I/O goes through the libtailscale userspace netstack.

**External dependencies:** `libtailscale` (C archive from Go build), `quinn-proto` (Rust crate), `tokio` (async runtime for task scheduling, not socket I/O)

### 3.5 Audio Pipeline **(core lib)**
**Responsibility:** Capture audio from either PipeWire monitor source (compositor module) or ALSA loopback device (headless daemon), encode to Opus, and transmit over QUIC datagrams.

| Path | Backend | Capture method |
|------|---------|---------------|
| Compositor module | PipeWire | Monitor source of default sink |
| Headless daemon | ALSA `snd-aloop` | Read from `hw:Loopback,1,0` |

Both paths produce monotonic timestamps for A/V sync. Opus encoding at 64-128 kbit/s, 5ms frame size for minimum latency.

**External dependencies:** `pipewire` (Rust crate), `alsa` (Rust crate), `opus` (Rust crate)

### 3.6 Bandwidth Estimation & Adaptation **(core lib)**
**Responsibility:** Estimate available bandwidth using dual signals (QUIC congestion controller + application-level GCC-inspired delay-gradient estimator), detect WiFi suspensions, and feed encoding parameters to three consumers: H.264 bitrate, tile classification thresholds, and refinement scheduler pacing.

| Consumer | Control signal |
|----------|---------------|
| H.264 encoder | `target_bitrate = estimated_bw × 0.7`, updated every frame |
| Tile classifier | Lower bandwidth → raise H.264 entry threshold, prefer lighter codecs |
| Refinement scheduler | `available_for_refinement = estimated_bw × 0.2`, controls passes/round |

**Congestion response:** Immediate halve H.264 bitrate, pause refinement, drop Opus to 64 kbit/s. **Recovery:** Ramp 10%/RTT.

**WiFi suspension detector:** Gaps >100ms followed by line-rate burst → mark 500ms recovery window, maintain pre-suspension bandwidth estimate, skip Kalman update.

**External dependencies:** None (pure algorithmic, uses data from Transport and receiver feedback)

### 3.7 Input Handling **(core lib)**
**Responsibility:** Receive keyboard and mouse events from the client over a dedicated QUIC unidirectional stream (highest priority), expose them via `poll_input()` for the compositor/daemon to inject.

| Event type | Fields |
|------------|--------|
| Key down/up | keycode, timestamp |
| Mouse move (relative) | dx, dy, timestamp |
| Mouse move (absolute) | x, y, timestamp |
| Mouse button down/up | button, timestamp |
| Mouse scroll | dx, dy, timestamp |

The X11 daemon injects via XTest extension. Enlightenment module injects via E's input API. Wayland compositor modules use their own input injection.

**External dependencies:** None (protocol parsing only)

### 3.8 Display Configuration **(core lib)**
**Responsibility:** Negotiate display layout between client and server. Client sends `DisplayLayout` with monitor geometry; server creates virtual outputs matching client monitors via EVDI/VKMS + EDID injection + xrandr (X11) or compositor API (Enlightenment/Wayland).

| Flow | Steps |
|------|-------|
| Initial connection | Client sends `DisplayLayout` → server creates virtual outputs → starts submitting frames |
| Dynamic reconfiguration | Client sends updated `DisplayLayout` → server updates EDID + hotplug → compositor reconfigures → `DisplayAck` to client |

**External dependencies:** DRM/KMS for EDID and EVDI/VKMS, `xcb` for xrandr (X11 daemon only)

### 3.9 ghostframe-xdaemon (Separate binary)
**Responsibility:** Standalone headless daemon for X11 servers. Creates virtual displays, captures screen via DRM/KMS, collects damage via XDamage, injects input via XTest.

**Startup sequence:** Load kernel modules (snd-aloop, EVDI/VKMS) → wait for client → receive DisplayLayout → create virtual outputs → start Xorg + PipeWire + WM → initialize ghostframe-lib → capture loop (DRM + XDamage → submit_frame).

**External dependencies:** `drm`/`drm-ffi`, `xcb` (XDamage, XTest, xrandr), `alsa` (loopback capture)

### 3.10 Client (Browser + Native)

**Browser client:** WebTransport (QUIC datagrams + streams) → WebCodecs VideoDecoder (H.264) → WebGPU compute shaders (BC1 decode, CDF 5/3 inverse wavelet) + WASM (PalRLE decode, Solid) → WebGPU framebuffer composition → canvas display. Audio via WebCodecs AudioDecoder + AudioWorklet. Input via Pointer Lock API + Keyboard Lock API.

**Native client:** Same Rust decode logic compiled natively, rendered via `wgpu` (WebGPU on Vulkan). Uses embedded libtailscale + quinn-proto for transport. Enumerates local monitors and generates `DisplayLayout`. Required for true multi-monitor with EDID injection.

---

## 4. Data Flow

### 4.1 Video Pipeline (Server → Client)

```
Compositor submits DMA-BUF fd
        │
        ▼
┌─ Capture Pipeline ─────────────────────────────────┐
│  Import DMA-BUF into Vulkan                         │
│  If damage rects provided:                          │
│    Map damage onto 32×32 tile grid → dirty set      │
│  Else:                                              │
│    GPU compute shader: SAD compare → dirty set      │
└─────────────────────────────────────────────────────┘
        │ dirty tiles + TileMetrics
        ▼
┌─ Tile Engine ───────────────────────────────────────┐
│  Two-axis classification (frequency × magnitude)    │
│  Hysteresis state machine per tile                  │
│  Output: per-tile CodecState                        │
│  If tile newly idle: queue for refinement           │
└─────────────────────────────────────────────────────┘
        │ codec per tile
        ▼
┌─ Encoder Pipeline ──────────────────────────────────┐
│  H.264 tiles → VA-API/NVENC encode (GPU)           │
│  Non-H.264 tiles → compute shader or CPU encode     │
│  CDF 5/3: forward wavelet + bit-plane pack (GPU)    │
│  PalRLE: palette lookup + nibble-pack (CPU)         │
│  Output: tile payloads with codec ID, generation    │
└─────────────────────────────────────────────────────┘
        │ encoded tiles
        ▼
┌─ Transport ─────────────────────────────────────────┐
│  Priority queue: Input > Video > Audio > Cursor >   │
│  Control > Extensions > Clipboard > Refinement       │
│  Fragment into QUIC datagrams (≤1200B each)          │
│  H.264 fragment retries until superseded by new frame│
│  Refinement passes: round-robin across idle tiles    │
└─────────────────────────────────────────────────────┘
        │ QUIC packets
        ▼
┌─ I/O Bridge ────────────────────────────────────────┐
│  quinn-proto state machine (no I/O, pure compute)   │
│  ↔ libtailscale userspace netstack (WireGuard)       │
│  ↔ Tailscale control plane (NAT traversal, DERP)    │
└─────────────────────────────────────────────────────┘
        │ encrypted UDP over tailnet
        ▼
┌─ Client (Browser) ──────────────────────────────────┐
│  WebTransport receive                                │
│  Reassemble datagrams → tile payloads                │
│  H.264 → WebCodecs VideoDecoder                     │
│  PalRLE/Solid → WASM decode                          │
│  BC1/CDF 5/3 → WebGPU compute shader decode          │
│  Composite into WebGPU framebuffer                    │
│  Send ACKs for refinement passes (control stream)    │
│  Send ReceiverFeedback every 100ms                   │
└──────────────────────────────────────────────────────┘
```

### 4.2 Audio Pipeline

```
PipeWire monitor source ──or── ALSA loopback (hw:Loopback,1,0)
        │                                        │
        ▼                                        ▼
   PCM f32 48kHz stereo                    PCM f32/S32 48kHz stereo
        │                                        │
        └──────────────┬─────────────────────────┘
                       ▼
              Opus encode (5ms frames, 64-128 kbit/s)
                       │
                       ▼
              QUIC datagrams (priority 3)
                       │
                       ▼
              Client: WebCodecs AudioDecoder → AudioWorklet → speakers
```

### 4.3 Input Pipeline (Client → Server)

```
Client browser: Pointer Lock + Keyboard Lock events
        │
        ▼
WebTransport unidirectional stream (priority 1, reliable)
        │
        ▼
Transport deserialization
        │
        ▼
ghostframe-lib: poll_input() → compositor/daemon
        │
        ├──▶ X11 daemon: XTest extension injection
        └──▶ Enlightenment module: E input API
        └──▶ Wayland compositor: remote input protocol
```

---

## 5. Key Design Decisions

| # | Decision | Rationale | Trade-off |
|---|----------|-----------|-----------|
| D1 | **GPU-first, no software encoding priority** | Linux desktop requires GPU for composition; software encode wastes CPU that the compositor needs | No Raspberry Pi / headless-ARM server support without VA-API |
| D2 | **Per-tile adaptive encoding (6 codecs)** | Desktop content is heterogeneous; one codec wastes bandwidth on text and destroys clarity on video | Complexity: 6 codepaths, classification engine, hysteresis |
| D3 | **CDF 5/3 over CDF 9/7 for progressive refinement** | CDF 5/3 uses integer arithmetic → exact lossless reconstruction. CDF 9/7 has irrational coefficients → inevitable rounding error | Slightly lower compression ratio for photographic content at extreme ratios (<1 bpp) |
| D4 | **No open ports — embedded Tailscale** | Zero attack surface, no firewall config, no cert management | Users must have a Tailscale account and network configured |
| D5 | **quinn-proto (no-I/O) over quinn (owns socket)** | libtailscale provides userspace netstack; quinn owns kernel sockets — fundamentally incompatible | More manual I/O bridge code, but zero architectural conflict |
| D6 | **Unreliable QUIC datagrams for video + app-level retry** | Head-of-line blocking is fatal for real-time video; datagrams allow dropping stale frames | Must implement fragmentation, reassembly, ACK protocol at app level |
| D7 | **Round-robin refinement with infinite retry** | All tiles converge to lossless together; no tile starves; generation counters invalidate stale work instantly | During heavy activity refinement pauses entirely — acceptable trade-off |
| D8 | **Browser-first client** | Zero install, universal access within tailnet | Limited to WebCodecs/WebGPU browser support; native client needed for full multi-monitor |
| D9 | **DMA-BUF zero-copy capture path** | Avoids GPU→CPU→GPU round-trips for encoding | Requires compositor or DRM/KMS access; fallback needs GPU diff shader |
| D10 | **Shared palette table (256 × 16 colors)** | Typical desktop uses only 5-8 distinct palettes; sharing across tiles saves bandwidth vs per-tile embedding | Must handle table exhaustion with fallback to CDF 5/3 or BC1 |

---

## 6. Cross-Cutting Concerns

### 6.1 Thread and Task Architecture

```
                                  tokio runtime
                            ┌─────────────────────┐
                            │  Task: I/O Bridge    │
                            │  (tailscale ↔ quinn) │
                            │                      │
                            │  Task: Frame Pipeline│
                            │  (capture → classify │
                            │   → encode → queue)   │
                            │                      │
                            │  Task: Transport TX  │
                            │  (priority scheduler,│
                            │   fragmentation,     │
                            │   refinement round-  │
                            │   robin)             │
                            │                      │
                            │  Task: Transport RX  │
                            │  (input deserialization│
                            │   feedback processing)│
                            │                      │
                            │  Task: Audio          │
                            │  (PipeWire/ALSA → Opus)│
                            └─────────────────────┘
                                │
                    ┌───────────┴───────────┐
                    │   GPU Work Queue       │
                    │   (Vulkan async compute│
                    │    + VA-API encoding)  │
                    └────────────────────────┘
```

**Synchronization model:** The public API (`submit_frame`, `submit_audio`, `poll_input`, `poll_display_change`) is non-blocking. The caller (compositor or daemon) drives frame submission from its own thread. Internally, `submit_frame` queues into a lock-free ring buffer consumed by the frame pipeline task. GPU work is submitted asynchronously; results are polled and forwarded to the transport task.

### 6.2 GPU Compute Pipeline

All GPU work runs on Vulkan compute queues (async, non-blocking from CPU perspective):

| Shader | Input | Output | When |
|--------|-------|--------|------|
| Tile diff (SAD) | Two DMA-BUF frames, tile grid | Dirty tile bitmap, per-tile SAD + unique color count + edge density | When compositor damage not provided |
| RGB → NV12 | Source frame region | NV12 luma+chroma surface | For H.264 tiles |
| BC1 encode | Source tile pixels | 512-byte BC1 block | For BC1-classified tiles |
| CDF 5/3 forward wavelet | Source tile pixels | Integer coefficients, bit-plane packed | For refinement-bound tiles |

VA-API/NVENC encoding is submitted via the same GPU — DMA-BUF fd is imported directly from the compositor's framebuffer into the encoder surface. Zero copy.

### 6.3 Generation-Based Invalidation

Every stateful per-tile mechanism uses a monotonically increasing **generation counter**:

- **Tile pixel data** — generation increments when tile content changes
- **Palette table entries** — referenced by tiles with their current generation; ref-counted
- **CDF 5/3 refinement passes** — tagged with tile generation; stale passes are discarded on arrival
- **H.264 frame fragments** — frame sequence number acts as generation; superseded frames stop retrying

This eliminates all forms of stale-data corruption without explicit cancellation messages.

### 6.4 Protocol Wire Format Summary

| Channel | QUIC Type | Priority | Reliability | Direction |
|---------|-----------|----------|-------------|-----------|
| Input | Unidirectional stream | 1 | Reliable | Client→Server |
| Video (dirty tiles + H.264) | Datagram | 2 | Unreliable + app retry | Server→Client |
| Audio | Datagram | 3 | Unreliable | Server→Client |
| Cursor | Datagram | 4 | Unreliable | Server→Client |
| Control | Bidirectional stream | 4 | Reliable | Both |
| Extensions (reliable) | Bidirectional stream | 5 | Reliable | Both |
| Extensions (unreliable) | Datagram | 5 | Unreliable + reassembly | Both |
| Clipboard | Bidirectional stream | 5 | Reliable | Both |
| Receiver feedback | Unidirectional stream | 5 | Reliable | Client→Server |
| Refinement (wavelet passes) | Datagram | 6 | Unreliable + app retry | Server→Client |

Datagram format: 12-byte header (frame_seq + frag_idx + frag_total + timestamp_us) + per-tile 8-byte header (tile_x + tile_y + codec + lz4 flag + generation + payload_len) + variable payload.

### 6.5 Security

- **Network:** WireGuard encryption (Tailscale) + QUIC TLS 1.3 (always on, cannot be disabled). No open ports. No authentication code in Ghostframe.
- **Certificate:** Ephemeral self-signed cert generated at startup. SHA-256 hash served via Tailscale HTTPS (LetsEncrypt-backed) to browser clients for `serverCertificateHashes`.
- **Input:** Pointer Lock + Keyboard Lock APIs (browser). System key capture only in fullscreen.
- **Codecs:** No JIT, no scripting, no eval. All decode is deterministic: GPU compute shaders (BC1, CDF 5/3 inverse), WebCodecs (H.264), WASM (PalRLE). No codec has write access beyond its tile region.

---

## 7. Component Dependency Map

```
                    ┌──────────────┐
                    │  Compositor   │ (external — provides DMA-BUF)
                    │  / xdaemon    │
                    └──────┬───────┘
                           │ FrameSubmission
                           ▼
┌────────┐    ┌────────────────────────────────────────┐
│  Audio │───▶│                                        │
│Source  │    │          ghostframe-lib                  │
│(PW/ALSA)│   │                                        │
└────────┘    │  ┌─────────┐  ┌──────────┐  ┌────────┐│
              │  │ Capture │─▶│  Tile     │─▶│Encoder ││
              │  │ Pipeline│  │  Engine   │  │Pipeline││
              │  └─────────┘  └──────────┘  └───┬────┘│
              │                                   │     │
              │  ┌─────────────────────────────────┘     │
              │  │ Encoded tiles                          │
              │  ▼                                       │
              │  ┌──────────┐  ┌───────────────────┐    │
              │  │Transport │◀─│Bandwidth          │    │
              │  │(QUIC/WT) │  │Estimation &      │    │
              │  └────┬─────┘  │Adaptation        │    │
              │       │        └───────────────────┘    │
              │       │                                  │
              │  ┌────▼─────┐                           │
              │  │  Input   │                           │
              │  │ Handler  │                           │
              │  └──────────┘                           │
              │  ┌──────────────────┐                    │
              │  │  Display Config  │                    │
              │  └──────────────────┘                    │
              └────────────────────────────────────────┘
                           │
                    libtailscale + quinn-proto
                           │
                    Tailscale WireGuard tunnel
                           │
              ┌────────────┴────────────┐
              │  Browser Client         │  Native Client
              │  (WT + WebCodecs +      │  (libtailscale + quinn +
              │   WebGPU + WASM)        │   wgpu + cpal)
              └─────────────────────────┘
```

### Crate Dependency Graph (Internal)

```
ghostframe-lib
  ├── quinn-proto        (QUIC state machine, no I/O)
  ├── libtailscale (C FFI) (WireGuard, control plane, NAT traversal)
  ├── ash               (Vulkan compute + DMA-BUF import)
  ├── cros-libva        (VA-API H.264 encode)
  ├── nvidia-video-codec-sdk (NVENC, optional)
  ├── pipewire          (Audio capture, compositor path)
  ├── alsa              (Audio capture, headless path)
  ├── opus              (Audio encode)
  ├── lz4_flex          (Optional tile compression)
  ├── drm / drm-ffi     (DRM/KMS, xdaemon path)
  ├── xcb               (XDamage, XTest, xrandr, xdaemon path)
  ├── tokio             (Async runtime for task scheduling)
  └── cbindgen          (Build: C header generation)

ghostframe-web-client (WASM)
  ├── wasm-bindgen
  ├── web-sys
  └── lz4_flex

ghostframe-native-client
  ├── ghostframe-lib
  ├── quinn-proto
  ├── libtailscale (C FFI)
  ├── wgpu
  ├── opus
  ├── cpal
  └── winit
```

---

## 8. Risk Areas and Mitigations

| # | Risk | Severity | Mitigation |
|---|------|----------|------------|
| R1 | **quinn-proto + libtailscale I/O bridge complexity** — Driving a no-I/O QUIC state machine through userspace UDP from a Go library is novel and has no reference implementation. | High | M1 scope limits this to basic QUIC + WebTransport handshake. Validate I/O bridge before building tile engine. Write a minimal ping-pong integration test first. |
| R2 | **GPU DMA-BUF lifetime across VA-API/NVENC/Vulkan** — DMA-BUF fd must remain valid from compositor capture through Vulkan import and VA-API encode. Premature close or incorrect synchronization corrupts frames. | High | Explicit lifetime contract in public API: caller retains fd ownership; library imports synchronously before return. Vulkan semaphore synchronization between compute and encode. Test with real GPU hardware from M1. |
| R3 | **WebGPU + WebCodecs browser compatibility** — WebGPU and WebCodecs are not universally available. Safari lacks WebCodecs; Firefox WebCodecs is behind flags. | Medium | Target Chromium-first. Feature-detect and show clear error messages. Native client is the fallback for full functionality. |
| R4 | **CDF 5/3 wavelet round-trip correctness** — Integer wavelet must produce exact lossless reconstruction. GPU compute shader floating-point concerns do not apply (integer arithmetic only), but shader dispatch and buffer alignment must be verified. | Medium | Criterion benchmarks + pixel-exact round-trip tests on real GPU hardware. Lavapipe for CI without GPU is acceptable for non-GPU tests only. |
| R5 | **H.264 intra-refresh with per-tile transmission** — Encoding with rolling intra-refresh but transmitting per-tile means the client must reassemble H.264 data into a coherent frame before decoding. Tile boundaries must align with macroblock boundaries. | Medium | Spec mandates 32×32 tiles which are exact multiples of H.264 macroblock size (16×16). Validate macroblock alignment in integration tests. |
| R6 | **EVDI/VKMS kernel module availability** — Virtual DRM connectors require either EVDI (out-of-tree) or VKMS (in-tree but limited). EVDI may not compile on all kernel versions. | Low | VKMS as in-tree fallback. Document kernel version requirements. Headless testing uses Xvfb + DRM render nodes as fallback. |
| R7 | **Palette table exhaustion** — 256 palettes of 16 colors each might overflow on extremely colorful desktops. | Low | Fallback to CDF 5/3 or BC1 is spec'd. LRU eviction clears stale palettes. Benchmark with worst-case desktops during M3. |

---

## 9. Phased Delivery Plan

This is a high-level sequencing for component-level design and implementation. Each milestone is designed to be independently testable.

### Phase 1: Transport Foundation (M1 subset)

**Goal:** Prove the quinn-proto + libtailscale I/O bridge works end-to-end before building anything else on top.

| Component | Deliverable |
|-----------|-------------|
| `ghostframe-lib/src/transport/tailscale_ffi.rs` | C FFI bindings to libtailscale |
| `ghostframe-lib/src/transport/quic_proto.rs` | Quinn-proto state machine driver |
| `ghostframe-lib/src/transport/io_bridge.rs` | I/O bridge event loop connecting the two |
| `ghostframe-lib/src/transport/webtransport.rs` | WebTransport handshake over quinn-proto |
| `ghostframe-lib/src/transport/protocol.rs` | Base datagram/stream format definitions |
| `ghostframe-xdaemon` (minimal) | Binary that embeds Tailscale, listens for QUIC connections |
| Browser client (minimal) | WebTransport connect + ping/pong |
| E2E test infra | testcontainers + headscale + playwright-rust setup |

**Test:** Connect browser to daemon over Tailscale, send/receive a ping. This validates the entire network stack.

### Phase 2: Single-Codec Video (M1)

**Goal:** Single codec (H.264) end-to-end video from DRM capture to browser display.

| Component | Deliverable |
|-----------|-------------|
| `ghostframe-lib/src/capture/dmabuf.rs` | Vulkan DMA-BUF import + compute shader for SAD (initially unused) |
| `ghostframe-lib/src/encoder/h264_vaapi.rs` | VA-API H.264 encode |
| `ghostframe-lib/src/display.rs` | Display layout negotiation |
| `ghostframe-lib/src/input.rs` | Input event types + forwarding |
| `ghostframe-xdaemon` | Full DRM/KMS capture + XDamage + XTest + xrandr |
| Browser client | WebCodecs H.264 decode + canvas render |
| `gf_server_submit_frame()` C API | Full C API compiles and links |

**Test:** `e2e_solid_color` — red square on Xvfb renders correctly in headless Chromium.

### Phase 3: Tile Engine (M2)

**Goal:** Per-tile adaptive encoding replaces brute-force H.264.

| Component | Deliverable |
|-----------|-------------|
| `ghostframe-lib/src/tile_engine.rs` | Two-axis classification, hysteresis state machine |
| `ghostframe-lib/src/encoder/pal_rle.rs` | Palettized RLE codec |
| `ghostframe-lib/src/encoder/solid.rs` | Solid fill codec |
| GPU compute shader | Tile diff (SAD comparison) when no damage rects |
| `ghostframe-lib/src/capture/dmabuf.rs` | SAD compute shader invocation path |
| XDamage bypass | When damage rects provided, skip GPU diff |

**Test:** `e2e_text_clarity` (palettized codec), `e2e_tile_skip` (skip codec), property-based testing.

### Phase 4: Full Codec Suite (M3)

**Goal:** All codecs operational, progressive refinement working, benchmark suite.

| Component | Deliverable |
|-----------|-------------|
| `ghostframe-lib/src/encoder/bc1.rs` | BC1 GPU compute shader encode |
| `ghostframe-lib/src/encoder/cdf53.rs` | CDF 5/3 forward/inverse wavelet shaders |
| Refinement scheduler | Round-robin, generation-based retry, bandwidth-aware pacing |
| Shared palette table | 256-palette LRU with reference counting |
| LZ4 optional compression | Per-codec cost/benefit measured |
| Codec benchmark suite | Criterion benchmarks for all codecs across content types |

**Test:** `e2e_codec_transition` (H.264 → BC1 → palettized → lossless chain), codec benchmarks pass.

### Phase 5: Multi-Monitor + Audio (M4 + M5)

| Component | Deliverable |
|-----------|-------------|
| `ghostframe-lib/src/display.rs` | EDID generation, EVDI output creation, xrandr reconfiguration |
| `ghostframe-lib/src/audio.rs` | PipeWire + ALSA capture paths |
| Opus encode/decode | 5ms frame, 64-128 kbit/s |
| A/V sync | Monotonic timestamp alignment, jitter buffer |
| Native client (initial) | Monitor enumeration, DisplayLayout generation |
| Bandwidth adaptation | GCC-inspired estimator, WiFi suspension detection, congestion response |

**Test:** `e2e_audio`, `e2e_resolution_change`.

### Phase 6: Production (M6)

| Component | Deliverable |
|-----------|-------------|
| NVENC support | `ghostframe-lib/src/encoder/h264_nvenc.rs` |
| Enlightenment module | `ghostframe-e-module` (C, links `libghostframe.so`) |
| Extension channels | Custom protocol channel registration API |
| Clipboard sync | Bidirectional clipboard over control stream |
| Hardening | Fuzz testing, performance profiling, GPU utilization optimization |

---

## 10. Downstream Component Design Checklist

Each subsystem listed in Section 3 requires a detailed component design document covering:

- [ ] **Data structures** — Rust structs, enums, constants with field-level documentation
- [ ] **Public API surface** — Rust `pub` API + C FFI function signatures
- [ ] **Thread/task model** — Which tokio tasks own what, lock-free queues vs mutexes
- [ ] **GPU pipeline details** — Vulkan shader code, buffer layouts, synchronization primitives
- [ ] **Error handling** — Error types, recovery strategies, failover paths
- [ ] **Persistent state** — What survives reconnection, what is discarded
- [ ] **Testing strategy** — Unit tests, integration tests, E2E tests, benchmarks for this component
- [ ] **Dependencies** — External crate versions, C library versions, kernel module versions
- [ ] **Configuration** — Runtime-configurable parameters with defaults (see spec for complete list)

Priority order for component design: **Transport → Capture Pipeline → Tile Engine → Encoder Pipeline → Audio → Bandwidth Estimation → Display Config → Client**. Transport is first because it is the highest-risk subsystem (R1) and everything depends on it. Client design can proceed in parallel once the wire format is stable.