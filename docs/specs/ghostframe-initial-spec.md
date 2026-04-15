# Ghostframe Protocol Specification

## A Modern Open-Source Remote Desktop Protocol for Linux

**Version:** 0.1-draft
**Language:** Rust
**Status:** Design Phase

---

## 1. Overview

Ghostframe is a Linux-only remote desktop protocol built around per-tile adaptive encoding, zero-copy GPU pipelines, QUIC transport over Tailscale, and a browser-based client.

The core insight: typical desktop content is ~80% text, ~15% static images, and ~5% motion. Encoding everything with a single video codec wastes bandwidth on static content and destroys text clarity. Ghostframe classifies each 32×32 tile independently and selects the optimal codec per tile per frame.

### Design Principles

- **GPU-first design** — the protocol assumes GPU hardware encoding (VA-API/NVENC) and Vulkan compute. Software fallbacks are not a priority; they may be added later for portability but are never the optimized path.
- **Compositor-agnostic core library** consumed by any compositor or standalone daemon
- **Zero-copy where hardware permits** — DMA-BUF from capture through encode
- **Adaptive per-tile encoding** — H.264 for motion, lossless for text, skip for unchanged
- **Progressive build-to-lossless** — coarse image immediately, refine to pixel-perfect when idle
- **Tailscale for identity and encryption** — no custom authentication
- **Browser-first client** via WebTransport + WebCodecs + WebGPU

### Security Model

**Ghostframe opens zero network ports.** The server binary does not call `bind()` on any kernel socket. All network I/O flows through an embedded Tailscale node running in userspace (via `libtailscale`). The server is unreachable from the local network, the public internet, and any network interface on the host. It exists only within the user's Tailscale mesh network (tailnet).

Consequences:
- **No firewall configuration required.** There are no ports to open or forward.
- **No port scanning exposure.** `nmap` against the host will never discover the server.
- **No authentication code in Ghostframe.** Tailscale handles identity (SSO-backed), device authorization, and per-connection access control via ACL tags.
- **Encryption is always on.** WireGuard encrypts all traffic. QUIC adds a second TLS 1.3 layer (required by the QUIC spec). Both are always active; neither can be disabled.
- **No certificate management.** The QUIC TLS layer uses an ephemeral self-signed certificate generated at startup. Its hash is served to browser clients over Tailscale's own HTTPS infrastructure (LetsEncrypt-backed). No CA, no cert renewal, no ACME.

---

## 2. Architecture

```
┌─────────────────────────────────────────────────────────┐
│                     Server Host                         │
│                                                         │
│  ┌──────────────┐     ┌─────────────────────────────┐   │
│  │  Compositor   │     │   ghostframe-lib (Rust)     │   │
│  │  (E, sway,   │────▶│                             │   │
│  │   custom)     │     │  ┌─────────┐ ┌──────────┐  │   │
│  │              │     │  │ Tile    │ │ Encoder  │  │   │
│  │  Provides:   │     │  │ Engine  │ │ Pipeline │  │   │
│  │  - DMA-BUF fd│     │  └────┬────┘ └─────┬────┘  │   │
│  │  - Damage    │     │       │             │       │   │
│  │    rects     │     │  ┌────▼─────────────▼────┐  │   │
│  └──────────────┘     │  │   QUIC/WebTransport   │  │   │
│                       │  │   (over Tailscale)     │  │   │
│  ┌──────────────┐     │  └───────────────────────┘  │   │
│  │ ghostframe-  │     └─────────────────────────────┘   │
│  │ xdaemon      │                                       │
│  │ (X11 fallback│──── DRM/KMS capture ─────┘            │
│  │  + XDamage)  │                                       │
│  └──────────────┘                                       │
└─────────────────────────────────────────────────────────┘
                           │
                      QUIC/UDP over
                      Tailscale WireGuard
                           │
┌─────────────────────────────────────────────────────────┐
│                     Client                              │
│                                                         │
│  ┌─────────────────────────────────────────────────┐    │
│  │  Browser (WebTransport + WebCodecs + WebGPU)    │    │
│  │  or                                             │    │
│  │  Native Client (multi-monitor, EDID injection)  │    │
│  └─────────────────────────────────────────────────┘    │
└─────────────────────────────────────────────────────────┘
```

---

## 3. Components

### 3.1 ghostframe-lib (Core Library, Rust)

The core library that any compositor can link against. It is the server-side protocol implementation.

**Input contract (provided by compositor or daemon):**

| Input | Type | Required | Description |
|-------|------|----------|-------------|
| `frame_fd` | `RawFd` (DMA-BUF) | Yes | File descriptor to the composited output framebuffer |
| `frame_format` | `FrameFormat` | Yes | Pixel format, modifier, width, height, stride, num_planes |
| `damage_rects` | `Vec<Rect>` | No | Regions that changed since last frame. If empty, library performs full-frame delta scan |
| `output_id` | `u32` | Yes | Which virtual output this frame belongs to (multi-monitor) |
| `timestamp_ns` | `u64` | Yes | Monotonic capture timestamp for A/V sync |

**Behavior when damage is not provided:**

When `damage_rects` is empty, the library imports the DMA-BUF into Vulkan, runs a compute shader that divides the frame into 32×32 tiles, and compares each tile against the previous frame using Sum of Absolute Differences (SAD). This produces a synthetic damage map. The compute cost is <0.5ms at 1080p.

**Behavior when damage is provided:**

The library maps the compositor's damage rects onto its 32×32 tile grid. Any tile overlapping a damage rect is marked dirty. No GPU tile comparison needed — the compositor's damage information is trusted.

**Public API (Rust):**

```rust
pub struct GhostframeServer {
    // opaque internals
}

pub struct FrameSubmission {
    pub fd: RawFd,               // DMA-BUF fd
    pub format: FrameFormat,
    pub damage: Option<Vec<Rect>>,
    pub output_id: u32,
    pub timestamp_ns: u64,
}

pub struct FrameFormat {
    pub width: u32,
    pub height: u32,
    pub fourcc: u32,             // DRM_FORMAT_*
    pub modifier: u64,           // DRM format modifier
    pub planes: Vec<PlaneInfo>,  // fd, offset, stride per plane
}

pub struct DisplayConfig {
    pub outputs: Vec<OutputConfig>,
}

pub struct OutputConfig {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub position: (i32, i32),     // position in virtual desktop
    pub physical_size_mm: (u32, u32),
}

impl GhostframeServer {
    /// Create a new server instance.
    /// Starts an embedded Tailscale node with the configured hostname.
    /// The server is only reachable from within the tailnet.
    /// No ports are opened on any physical or virtual network interface.
    pub fn new(config: ServerConfig) -> Result<Self>;

    /// Submit a new frame for encoding and transmission.
    /// Non-blocking. Returns immediately after queuing.
    pub fn submit_frame(&self, frame: FrameSubmission) -> Result<()>;

    /// Submit captured audio samples.
    /// PCM f32, 48kHz, interleaved stereo.
    /// Only needed when audio_backend is None (caller manages capture).
    /// When the library is configured with an audio backend, it captures
    /// internally and this call is ignored.
    pub fn submit_audio(&self, samples: &[f32], timestamp_ns: u64) -> Result<()>;

    /// Called when client requests a display configuration change.
    /// Returns the new layout requested by the client.
    /// The compositor should apply this and start submitting
    /// frames with the new resolution.
    pub fn poll_display_change(&self) -> Option<DisplayConfig>;

    /// Forward input events from client to the compositor.
    /// Returns an iterator of pending input events.
    pub fn poll_input(&self) -> impl Iterator<Item = InputEvent>;

    /// Shutdown.
    pub fn close(self);
}
```

**Public API (C, via `extern "C"`):**

The library exposes a C ABI for integration with C-based compositors (Enlightenment, custom Wayland compositors, etc.). The C API uses opaque handles and follows POSIX conventions (return 0 on success, -1 on error with errno).

```c
#ifndef GHOSTFRAME_H
#define GHOSTFRAME_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Opaque server handle */
typedef struct gf_server gf_server_t;

/* Frame format descriptor */
typedef struct {
    uint32_t width;
    uint32_t height;
    uint32_t fourcc;          /* DRM_FORMAT_* */
    uint64_t modifier;        /* DRM format modifier */
    uint32_t num_planes;
    struct {
        int      fd;          /* DMA-BUF fd for this plane */
        uint32_t offset;
        uint32_t stride;
    } planes[4];
} gf_frame_format_t;

/* Damage rectangle */
typedef struct {
    int32_t  x, y;
    uint32_t width, height;
} gf_rect_t;

/* Frame submission */
typedef struct {
    int                fd;             /* DMA-BUF fd (primary plane, or use format.planes) */
    gf_frame_format_t  format;
    const gf_rect_t   *damage_rects;   /* NULL = no damage info, library does full scan */
    uint32_t           damage_count;   /* 0 if damage_rects is NULL */
    uint32_t           output_id;
    uint64_t           timestamp_ns;   /* monotonic capture timestamp */
} gf_frame_t;

/* Output configuration */
typedef struct {
    uint32_t id;
    uint32_t width, height;
    uint32_t refresh_hz;
    int32_t  pos_x, pos_y;
    uint32_t phys_width_mm, phys_height_mm;
} gf_output_config_t;

/* Display layout (from client) */
typedef struct {
    const gf_output_config_t *outputs;
    uint32_t                  output_count;
} gf_display_config_t;

/* Input event types */
typedef enum {
    GF_INPUT_KEY_DOWN,
    GF_INPUT_KEY_UP,
    GF_INPUT_MOUSE_MOVE,       /* relative */
    GF_INPUT_MOUSE_MOVE_ABS,   /* absolute */
    GF_INPUT_MOUSE_BUTTON_DOWN,
    GF_INPUT_MOUSE_BUTTON_UP,
    GF_INPUT_MOUSE_SCROLL,
} gf_input_type_t;

typedef struct {
    gf_input_type_t type;
    uint64_t        timestamp_ns;
    union {
        struct { uint32_t keycode; } key;
        struct { int32_t dx, dy; } mouse_rel;
        struct { uint32_t x, y; } mouse_abs;
        struct { uint32_t button; } mouse_button;
        struct { int32_t dx, dy; } scroll;
    };
} gf_input_event_t;

/* Server configuration */
typedef struct {
    const char *tailscale_hostname; /* Tailnet hostname, e.g. "ghostframe-server" */
    const char *tailscale_authkey;  /* TS_AUTHKEY for headless auth, or NULL for interactive */
    const char *tailscale_state_dir;/* Persistent state directory, or NULL for default */
    uint32_t    quic_port;         /* Port within tailnet (default 4443). NOT a system port. */
    uint32_t    max_bitrate;       /* bits/sec, 0 = auto */
    enum {
        GF_AUDIO_NONE,         /* no audio capture */
        GF_AUDIO_ALSA_LOOPBACK,/* snd-aloop: for headless daemon */
        GF_AUDIO_PIPEWIRE,     /* PipeWire monitor: for compositor modules */
    }           audio_backend;
    const char *audio_device;  /* ALSA: e.g. "hw:Loopback,1,0", PW: sink monitor name. NULL = auto */
} gf_server_config_t;

/* Lifecycle */
gf_server_t *gf_server_new(const gf_server_config_t *config);
void         gf_server_destroy(gf_server_t *server);

/* Frame submission — non-blocking, queues internally.
 * The DMA-BUF fd is NOT consumed; caller retains ownership.
 * Returns 0 on success, -1 on error. */
int gf_server_submit_frame(gf_server_t *server, const gf_frame_t *frame);

/* Audio submission — PCM f32, 48kHz, interleaved stereo.
 * Only needed when audio_backend is GF_AUDIO_NONE (caller manages capture).
 * When audio_backend is GF_AUDIO_ALSA_LOOPBACK or GF_AUDIO_PIPEWIRE,
 * the library captures audio internally and this call is ignored.
 * Returns 0 on success, -1 on error. */
int gf_server_submit_audio(gf_server_t *server,
                           const float *samples,
                           uint32_t sample_count,
                           uint64_t timestamp_ns);

/* Poll for client-requested display layout change.
 * Returns 1 if a new config is available (written to *out), 0 if none.
 * The caller should apply the config and begin submitting frames
 * at the new resolution. The output pointers in *out are valid until
 * the next call to gf_server_poll_display_change. */
int gf_server_poll_display_change(gf_server_t *server,
                                  gf_display_config_t *out);

/* Poll for client input events.
 * Returns the number of events written to `events` (up to max_events).
 * Returns 0 if no events pending. */
int gf_server_poll_input(gf_server_t *server,
                         gf_input_event_t *events,
                         uint32_t max_events);

/* Get the server's ephemeral self-signed cert hash (SHA-256, 32 bytes).
 * Browser clients need this for WebTransport serverCertificateHashes.
 * The cert is generated at startup and used only within the tailnet.
 * Returns 0 on success, -1 if server not yet started. */
int gf_server_get_cert_hash(gf_server_t *server, uint8_t out[32]);

#ifdef __cplusplus
}
#endif

#endif /* GHOSTFRAME_H */
```

**Build output:** `libghostframe.so` / `libghostframe.a` + `ghostframe.h`. The Rust library is compiled with `crate-type = ["cdylib", "staticlib", "rlib"]` to support C dynamic linking, C static linking, and Rust crate dependency respectively. `libtailscale` (built from Go source via `go build -buildmode=c-archive`) is statically linked into the final binary, embedding the Go runtime (~15MB). The Go runtime handles only WireGuard crypto and Tailscale control plane; all hot-path video/audio processing is pure Rust.

### 3.2 ghostframe-xdaemon (X11 Headless Daemon, Rust)

A standalone daemon for **headless servers** running X11. It creates virtual displays, captures the screen, and feeds frames into ghostframe-lib. Not intended for capturing an existing user desktop session (that use case is served by compositor modules).

**Capture path:** DRM/KMS → `drmModeGetFB2()` → `drmPrimeHandleToFD()` → DMA-BUF fd

**Damage path:** XDamage extension on root window → coarse per-window damage rects

**Requires:** `CAP_SYS_ADMIN` for DRM framebuffer access. A small setcap helper binary provides the DMA-BUF fd via `SCM_RIGHTS` over a Unix socket if running unprivileged.

**Headless display setup (performed at startup):**
1. Load EVDI or VKMS kernel module for virtual DRM connectors
2. Load `snd-aloop` kernel module for audio loopback
3. Generate synthetic EDID from client-requested layout
4. Start Xorg with virtual outputs at requested resolutions
5. Start PipeWire (or PulseAudio) configured to output to `hw:Loopback,0`
6. Start Enlightenment (or configured WM) on the virtual display

**Flow:**

```
1. Load kernel modules (snd-aloop, EVDI/VKMS)
2. Wait for client connection (QUIC)
3. Receive DisplayLayout from client
4. Create virtual outputs matching client monitors
5. Start X11 + PipeWire (→ hw:Loopback,0) + window manager
6. Initialize ghostframe-lib with GF_AUDIO_ALSA_LOOPBACK
7. Open DRM device (/dev/dri/card0)
8. Subscribe to XDamage on root window
9. On each vsync or damage event:
   a. drmModeGetFB2() → get current framebuffer handle
   b. drmPrimeHandleToFD() → export as DMA-BUF
   c. Collect XDamage rects since last frame
   d. Call gf_server_submit_frame(server, &frame)
   (Audio is captured internally by the library from hw:Loopback,1,0)
10. On gf_server_poll_input() → inject events via XTest extension
11. On gf_server_poll_display_change() → xrandr to resize/add outputs
```

### 3.3 Enlightenment Module (C, Optional)

A loadable `.so` module for Enlightenment that hooks E's compositor internals for tighter integration than the X11 daemon.

**Hooks:**

- `e_comp_object_damage` → collects per-object damage rects from E's internal tiler (`Eina_Tiler`)
- `EVAS_CALLBACK_RENDER_POST` on `e_comp->evas` → fires after each composited frame
- GL FBO export via `eglExportDMABUFImageMESA()` → DMA-BUF fd of the composited output

**Advantage over xdaemon:** damage rects are per-object and sub-window granularity, versus XDamage's per-window coarseness. The GL FBO export also avoids the DRM/KMS capture path entirely. Audio is captured via PipeWire monitor source (`GF_AUDIO_PIPEWIRE`) since PipeWire is already running in the user's desktop session.

**IPC to ghostframe-lib:** Link `libghostframe.so` directly and call the C API (`gf_server_submit_frame`, etc.). Alternatively, communicate via shared memory ring buffer for damage rect lists + `SCM_RIGHTS` for DMA-BUF fd handoff over Unix socket to a separate ghostframe process.

---

## 4. Tile Engine

### 4.1 Tile Grid

The frame is divided into a grid of **32×32 pixel tiles**. At 1920×1080, this produces 60×34 = 2040 tiles (partial tiles at edges are padded).

Each tile carries per-frame metadata:

```rust
pub enum CodecState {
    Skip,
    H264 { frames_in_h264: u32 },
    BC1,
    PalettizedRLE { palette_id: u8 },
    Solid,
    CDF53 { passes_sent: u8, max_passes: u8 },
    PixelPerfect,
}
```

### 4.2 Two-Axis Tile Classification

Tile codec selection is driven by **two independent axes**, not frame rate alone:

- **Frequency:** How often the tile changes (exponential moving average over last 60 frames)
- **Magnitude:** How much the tile changes when it does (SAD normalized to tile area, plus unique color count)

This distinction is critical. A blinking cursor is high-frequency but low-magnitude — a few pixels toggling between two colors at 60Hz. A video playing is high-frequency AND high-magnitude — thousands of pixels changing to completely different values every frame. Scrolling text is medium-frequency, medium-magnitude. These all require different codecs.

```
                        Magnitude (SAD / pixel count)
                    Low              Medium            High
                ┌────────────┬─────────────────┬────────────────┐
         High   │ BC1 or     │ BC1             │ H.264          │
  Freq   (>15Hz)│ Palettized │                 │                │
                ├────────────┼─────────────────┼────────────────┤
         Med    │ Palettized │ BC1             │ BC1 or H.264   │
         (5-15) │            │                 │ (hysteresis)   │
                ├────────────┼─────────────────┼────────────────┤
         Low    │ Solid/     │ Palettized │ BC1            │
                │ Palettized │ RLE        │ or CDF 5/3     │
         (<5Hz) │ RLE        │ → refine   │ → refine       │
                ├────────────┼─────────────────┼────────────────┤
         Zero   │ Skip       │ Skip            │ Skip           │
         (0Hz)  │            │                 │                │
                └────────────┴─────────────────┴────────────────┘
```

When damage rects are provided, only tiles overlapping damage are evaluated. When not provided, all tiles are compared via GPU compute shader.

**Per-tile metrics (updated every frame):**

```rust
pub struct TileMetrics {
    /// Exponential moving average of change frequency.
    /// Alpha = 0.1. Updated: freq = freq * 0.9 + (changed ? 60.0 : 0.0) * 0.1
    pub change_freq_hz: f32,

    /// SAD (Sum of Absolute Differences) of the most recent change,
    /// normalized to [0.0, 1.0] per pixel.
    /// 0.0 = identical, 1.0 = every pixel maximally different.
    pub change_magnitude: f32,

    /// Approximate number of distinct colors in the tile.
    /// Computed via hash-based estimation in compute shader.
    pub unique_colors: u16,

    /// Edge density: fraction of pixels with high gradient.
    /// High = text/lines, low = flat/photographic.
    pub edge_density: f32,

    /// Consecutive frames this tile has been unchanged.
    pub idle_frames: u32,

    /// Current codec and refinement state.
    pub codec_state: CodecState,
}
```

**Classification rules (evaluated in order):**

| # | Condition | Codec | Rationale |
|---|-----------|-------|-----------|
| 1 | `idle_frames > 0` (unchanged) | **Skip** | Zero bandwidth |
| 2 | `change_freq > 15Hz` AND `change_magnitude > 0.3` sustained 10 frames | **H.264** | True motion content (video, scrolling, animation) |
| 3 | `change_freq > 15Hz` AND `change_magnitude ≤ 0.3` | **BC1** or **Palettized** | High-frequency but small changes: cursors, blinking icons, spinners. BC1 if >16 colors, palettized if ≤16. |
| 4 | `change_freq 5-15Hz` AND currently H.264 | **Keep H.264** | Hysteresis: don't exit H.264 until frequency drops further |
| 5 | `change_freq 5-15Hz` AND not H.264 | **BC1** | Medium activity, lossy but fast |
| 6 | `unique_colors ≤ 1` | **Solid fill** | Single color (4 bytes) |
| 7 | `unique_colors ≤ 16` AND palette available | **Palettized RLE** | Text, UI elements, flat fills. Uses shared palette table + run-length encoding. |
| 8 | Everything else (or palette table full) | **CDF 5/3 lossy** or **BC1** | General-purpose lossy, fast GPU encode |

**Magnitude thresholds:**

- **Low magnitude** (`change_magnitude ≤ 0.05`): Cursor blink, single-pixel changes, status indicator toggles
- **Medium magnitude** (`0.05 < change_magnitude ≤ 0.3`): Typing in a terminal (new characters), progress bars, small UI updates
- **High magnitude** (`change_magnitude > 0.3`): Video playback, page scrolling, window drag, full repaint

### 4.3 Hysteresis

Codec switching uses asymmetric thresholds to prevent rapid oscillation:

- **Enter H.264:** `change_freq > 15Hz` AND `change_magnitude > 0.3`, sustained for 10 frames
- **Exit H.264:** `change_freq < 5Hz` OR `change_magnitude < 0.1`, sustained for 30 frames
- **Deadband:** If currently H.264 and conditions are between enter/exit thresholds, stay H.264

A tile that was in H.264 mode and becomes idle transitions through BC1 first (immediate lossy snapshot), then enters the refinement chain. It does NOT jump directly to lossless.

### 4.4 Progressive Refinement (Build-to-Lossless via CDF 5/3)

When a tile becomes idle (`idle_frames > 30`) and spare bandwidth exists, it enters progressive wavelet refinement using the **CDF 5/3 (Le Gall) integer wavelet transform**. CDF 5/3 uses only integer additions, subtractions, and bit-shifts — the forward and inverse transforms are mathematically exact, guaranteeing pixel-perfect reconstruction when all bit-planes are delivered. This is the same wavelet JPEG 2000 uses for its lossless mode.

A single codec covers the entire quality range from coarse preview to exact pixel match, with no codec switching:

```
Pass 0        Pass 1       Pass 2      ...  Pass N
(coarse)     (+detail)   (+detail)       (lossless)
  ████          ▓▓▓▓        ░░░░            ....
  ~128B        +~128B      +~128B         +~128B
                                        total ~1-1.3KB
```

**Why CDF 5/3 instead of CDF 9/7:** CDF 9/7 has irrational filter coefficients — every forward+inverse transform introduces rounding error, making lossless reconstruction impossible regardless of how many precision bits are sent. CDF 5/3 achieves identical visual quality at typical remote desktop bitrates (the difference matters only at extreme compression ratios below 1 bpp for photographic content) while providing a clean path to pixel-perfect. The GPU compute shader cost is equivalent — same number of operations, simpler arithmetic.

**Generation-based retry on unreliable datagrams:**

All wavelet passes — including refinement — are sent as unreliable QUIC datagrams with application-level retry. No reliable streams, no head-of-line blocking. A generation counter on each tile enables instant cancellation when content changes.

```
Server state per tile:
┌─────────────────────────────────────┐
│ tile (12, 7)                        │
│ generation: 42                      │
│ coefficients: [i16; 1024]           │
│ max_bitplanes: 9                    │
│ passes_acked: [bool; 9]            │
│ last_sent_at: [Option<Instant>; 9] │
│ refinement_started_at: Instant      │
└─────────────────────────────────────┘
```

**Retry policy: never give up, let bandwidth govern pace.**

Refinement passes are retried indefinitely until either (a) the tile's generation changes (content updated, all old passes cancelled) or (b) the pass is successfully ACKed. There is no `max_retries` or `refinement_budget` timeout. The bandwidth adaptation mechanism naturally throttles retries during congestion — if the network can only sustain 5 passes/round, the scheduler sends 5, waits for ACKs, and retries failures in the next round. During sustained congestion, refinement may progress at 1 pass per tile per second or slower, but it never stops trying. When bandwidth recovers, it catches up.

**Bandwidth-aware backoff using app-level ACK rate:**

QUIC's congestion controller tracks network capacity (congestion window, packet loss) and correctly accounts for datagram traffic since datagrams ride in acknowledged QUIC packets. However, the refinement scheduler also tracks its own delivery rate from the ACK stream for finer-grained adaptation:

```
refinement_delivery_rate = acked_passes / sent_passes  (rolling 10-round window)

If delivery_rate < 0.5 for 10+ rounds:
  → refinement is being crowded out by higher-priority traffic
  → halve refinement_bandwidth_fraction temporarily
  → when delivery_rate recovers above 0.8, restore original fraction
```

This dual-signal approach uses QUIC congestion state as the ceiling and app-level ACK rate as the floor for refinement throughput.

**H.264 retry policy:**

H.264 frames are sent as unreliable datagrams (fragmented across multiple packets). When packets are lost, the server retries the lost fragments — but only until a **newer frame is submitted** for any tile covered by that H.264 region. Since H.264 is full-frame (or at minimum covers multiple tiles), a new frame completely supersedes the old one. Retry of old frame fragments stops immediately when a new frame enters the pipeline.

In practice, this means:
- At 60fps, each frame has ~16ms before the next frame arrives. Lost fragments are retried within that window if bandwidth permits.
- During cooldown (frame rate dropping, transition from H.264 to idle), the inter-frame gap grows and retries have more time to succeed. This is exactly when retries matter most — the last H.264 frame before a tile goes idle becomes the starting point for wavelet refinement, so delivering it completely produces a better base image.
- The retry bandwidth is governed by the same QUIC congestion window — retries compete with new frame data and only proceed when spare capacity exists.

```
Server H.264 frame state:
┌──────────────────────────────────────┐
│ frame_seq: 1047                      │
│ fragments: [sent, acked, sent, lost] │
│ superseded_by: None                  │
│                                      │
│ On new frame submitted:              │
│   superseded_by = Some(1048)         │
│   → stop retrying fragments of 1047 │
└──────────────────────────────────────┘
```

**Protocol flow:**

1. Tile becomes idle → server computes CDF 5/3 forward transform, stores integer coefficients, assigns `generation=N`.
2. Send pass 0 as unreliable datagram. If lost, it will be retried in the next refinement round — same as any other pass.
3. When the scheduler allocates bandwidth for this tile's next pass (see round-robin scheduling below), send it as a datagram. Start retry timer = `2 × RTT`.
4. Client receives pass → sends lightweight ACK: `tile(x,y) gen=N pass=K` (8 bytes on the control stream).
5. Server receives ACK → marks pass K as delivered, tile becomes eligible for pass K+1 in the next round.
6. No ACK within `2 × RTT` → pass re-enters the retry queue for the next round. No retry limit — the pass will be retried until ACKed or generation invalidated.
7. **Tile changes** → generation increments. All pending retries for old generations are cancelled instantly. Client drops any stale-generation passes that arrive late.

**Client-side logic:**

```rust
fn on_wavelet_pass(tile_x: u16, tile_y: u16, gen: u32, pass: u8, data: &[u8]) {
    let tile = &mut tiles[tile_x][tile_y];

    // Stale generation — ignore, don't ACK
    if gen < tile.current_gen { return; }

    // New generation — reset refinement state
    if gen > tile.current_gen {
        tile.current_gen = gen;
        tile.passes_received = [false; MAX_PASSES];
    }

    // Duplicate — re-ACK in case server missed it
    if tile.passes_received[pass] {
        send_ack(tile_x, tile_y, gen, pass);
        return;
    }

    // Out of order — can't apply pass 3 without pass 2
    // Don't ACK; server will retry earlier pass
    if pass > 0 && !tile.passes_received[pass - 1] { return; }

    // Apply bit-plane, update display
    tile.apply_bitplane(pass, data);
    tile.passes_received[pass] = true;
    send_ack(tile_x, tile_y, gen, pass);
}
```

**Round-robin refinement scheduling:**

Refinement does not focus on a single tile until it reaches lossless. Instead, the server maintains a **refinement queue** of all idle tiles sorted by idle duration (oldest first). Each scheduling round distributes passes across all eligible tiles:

```
Refinement scheduler (runs after all higher-priority traffic is sent):

1. Estimate available bandwidth for refinement:
   available_bytes = min(
     congestion_window - bytes_in_flight - reserved_for_higher_priority,
     congestion_window * refinement_bandwidth_fraction
   )

2. Collect all tiles needing work:
   pending_retry = tiles with unacked passes past retry_interval
   pending_new   = tiles where idle_frames > 30 AND passes_sent < max_bitplanes

3. Interleave retries and new passes (retries first within each round-robin cycle):
   work_queue = merge(pending_retry, pending_new) deduplicated by tile

4. Estimate bytes per pass ≈ (tile_coefficients / 8) + header ≈ 140 bytes
   passes_this_round = available_bytes / bytes_per_pass

5. Distribute round-robin across eligible tiles:
   for i in 0..passes_this_round:
       tile = work_queue[i % work_queue.len()]
       if tile has unacked pass pending retry:
           resend that pass
       else:
           send next new pass
```

This ensures:
- **All tiles progress together** — a screen with 500 idle tiles gets 1 pass each before any tile gets pass 2, so the entire display sharpens uniformly.
- **Retries don't starve new passes** — retries are interleaved with new passes in round-robin order, not prioritized above them.
- **Bandwidth-proportional throughput** — on a fast LAN, many passes per round; on a slow link, fewer passes, spread across tiles.
- **No tile is abandoned** — every tile with incomplete refinement eventually reaches lossless, as long as its content doesn't change.

**Priority ordering (lowest-priority traffic last):**

| Priority | Traffic | Notes |
|----------|---------|-------|
| 1 (highest) | Input events | Keyboard, mouse — latency-critical |
| 2 | Video (dirty tiles + H.264 retries) | New tile content and H.264 fragment retries (until superseded) |
| 3 | Audio | Opus packets |
| 4 | Cursor updates | Position + image |
| 5 | Extension channels | Custom protocol data |
| 6 (lowest) | **Refinement passes** | CDF 5/3 pass 0...N, retries, round-robin scheduled |

Refinement is sent **only with leftover bandwidth** after all higher-priority traffic. If the network is saturated with video frames, refinement pauses entirely and resumes when bandwidth frees up. H.264 fragment retries are priority 2 (same as new video) because a partially-delivered H.264 frame is useless without the missing fragments — but retries stop immediately when a new frame supersedes the old one.

**Configurable parameters:**

| Parameter | Default | Purpose |
|-----------|---------|---------|
| `refinement_retry_interval` | `2 × RTT` | Wait before retrying an unacked pass |
| `refinement_min_idle_frames` | 30 | Frames of stability before refinement begins |
| `refinement_bandwidth_fraction` | 0.2 | Maximum fraction of congestion window used for refinement (adapts down under congestion) |
| `h264_retry_enabled` | true | Whether to retry lost H.264 fragments |

**Why never-give-up works:** The combination of generation-based cancellation (content change = instant invalidation), round-robin scheduling (no single tile monopolizes bandwidth), bandwidth-proportional pacing (congestion window governs throughput), and app-level delivery tracking (backoff when delivery rate drops) means the system self-regulates without artificial timeouts. During heavy activity, refinement effectively pauses because all bandwidth goes to dirty tiles. During idle periods, refinement consumes available bandwidth and every tile eventually reaches pixel-perfect. The only thing that stops refinement for a tile is new content — which is exactly the right invalidation signal.

---

## 5. Encoding Pipeline

### 5.1 GPU Zero-Copy Path

```
DMA-BUF (from compositor/DRM)
    │
    ▼
Vulkan import (VK_EXT_external_memory_dma_buf)
    │
    ├──▶ Compute shader: tile SAD + classify (< 0.5ms @ 1080p)
    │       Output: tile dirty bitmap + per-tile metrics
    │       (Only when compositor damage not provided)
    │
    ├──▶ Compute shader: RGB→NV12 conversion (for H.264 tiles)
    │
    ▼
VA-API import (VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2)
  or NVENC import (NvEncRegisterResource with CUDA)
    │
    ▼
H.264 hardware encode (dirty region via ROI / per-tile QP)
    │
    ▼
NAL units → QUIC datagrams
```

For non-H.264 tiles (palettized RLE, BC1, CDF 5/3), the GPU compute shader produces the compressed output directly. For CDF 5/3, the Vulkan compute shader performs the forward integer wavelet transform and bit-plane packing entirely on-GPU — achieving <0.1ms for a full 1080p frame (per PyroWave benchmarks on RDNA4 with the closely related CDF 9/7; CDF 5/3 is computationally simpler). Each 32×32 tile is an independent wavelet block, enabling per-tile codec selection and packet-loss resilience. The quantized coefficients are read back to CPU for QUIC transmission.

### 5.2 H.264 Encoder Configuration

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Rate control | CBR | Predictable bandwidth |
| B-frames | None (`ip_period=1`) | Minimum latency |
| GOP | Infinite + intra-refresh | No IDR bitrate spikes |
| Intra refresh | Rolling row | Spread I-data across frames |
| Slices | 1 per frame | Simplicity |
| Latency preset | Ultra-low | VA: `VA_ENC_TUNING_LOW_LATENCY`; NVENC: `NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY` |
| VBV buffer size | `bitrate / fps` | Single-frame buffer |

**Reference frame invalidation (NVENC):** When client reports packet loss, encoder invalidates the lost reference frame and uses the last-acknowledged frame as reference. Avoids full IDR.

### 5.3 Non-H.264 Codecs

**Palettized RLE (shared palette table):**
For tiles with ≤16 unique colors. Encodes runs of pixels as packed bytes: high nibble = color index (0-15), low nibble = run length minus one (0-15, representing 1-16 consecutive pixels). Pixels are scanned in row-major order across the 32×32 tile.

```
Each byte: [ color_index : 4 bits ][ run_length_minus_1 : 4 bits ]

Example: 0x3F = color 3, run of 16 pixels
         0xA0 = color 10, run of 1 pixel
```

Sizes by content type:
- Solid-color tile: 1024÷16 = **64 bytes** (one run of 16 repeated 64 times)
- Typical text tile (white text on dark background): **100-200 bytes** (long background runs, short foreground runs)
- Worst case (checkerboard, every pixel different from neighbor): **1024 bytes** (run length 1 for every pixel)

Each tile carries a 1-byte `palette_id` referencing a shared palette table rather than embedding the palette per tile.

The server maintains a **shared palette table** of up to 256 palettes (u8 palette ID), each containing up to 16 RGBA colors. Palettes are synchronized to the client over the reliable control stream and cached. The lifecycle:

1. **Palette lookup:** When a tile is classified as palettized, the server extracts its unique colors and searches the table for a matching palette — one that contains all of the tile's colors (extra unused entries are fine).
2. **Palette creation:** If no match exists and a free slot is available (reference count = 0 or table not full), a new palette is created and sent to the client: `PaletteUpdate { id: u8, colors: [RGBA; 16], count: u8 }`.
3. **Palette exhaustion fallback:** If the table is full (all 256 palettes in use by other tiles), the tile falls back to CDF 5/3 lossy or BC1 rather than evicting a palette that other tiles depend on.
4. **Palette eviction:** When a palette's reference count drops to zero (all tiles using it have changed or been reclassified), the slot becomes free. Reference counts are updated whenever a tile changes codec or is reclassified.

On a typical desktop, the number of distinct palettes is far smaller than the number of palettized tiles — a dark-themed IDE uses ~5-8 palettes (editor, sidebar, terminal, tabs, status bar), and even a complex multi-application desktop rarely exceeds 50. Sharing palettes across hundreds of tiles saves significant bandwidth compared to per-tile embedded palettes.

**Solid Fill:**
For single-color tiles: emit just the color (4 bytes). Common for window backgrounds, margins.

**BC1 (DXT1) GPU Texture Compression:**
4 bits/pixel, 32×32 = 512 bytes per tile. Encode via GPU compute shader (<0.1ms). Client decodes with a trivial WebGPU compute shader. Good for photographic content that doesn't need H.264-level compression.

**Raw/Pixel-Perfect:**
Uncompressed tile data, optionally LZ4 compressed. Used as the final state after progressive refinement completes. ~4KB per tile before LZ4, ~1-3KB after for typical content.

**LZ4 compression is optional on all non-H.264 codecs.** Whether LZ4 helps depends on content and codec — palettized RLE output is already compact but may benefit from LZ4 on repetitive patterns, while BC1 and CDF 5/3 output may already be near-entropy and LZ4 adds CPU cost with minimal size reduction. The M3 codec benchmark suite measures each codec with and without LZ4 to determine the break-even point. The tile header carries a `lz4` flag so the client knows whether to decompress.

### 5.4 Audio Pipeline

Audio capture uses two different backends depending on the deployment model:

**Headless daemon (`snd-aloop` ALSA loopback):**

The kernel's ALSA loopback module provides a virtual sound card with two sides. Applications play to one side; the daemon reads raw PCM from the other. This avoids any dependency on a userspace sound server being correctly initialized in a headless environment.

```
modprobe snd-aloop pcm_substreams=1

Applications → PipeWire/PulseAudio → hw:Loopback,0 (playback)
                                          │
                              kernel loopback device
                                          │
Ghostframe daemon ← ALSA read ← hw:Loopback,1 (capture)
```

PipeWire (or PulseAudio) is still started in the headless session for application compatibility, configured to output to the loopback device. But the daemon's capture path goes through ALSA directly, independent of the sound server's lifecycle.

The daemon opens the loopback capture side via the `alsa` Rust crate: `pcm::PCM::open("hw:Loopback,1,0", Direction::Capture)`, configured for S32_LE or F32_LE, 48kHz, stereo. This is a blocking read that produces PCM samples ready for Opus encoding.

**Compositor module (PipeWire monitor source):**

When running as a compositor module in an existing desktop session, PipeWire is already running and managing audio. The module captures from the default sink's monitor source via the `pipewire` Rust crate. Create a stream with `MEDIA_TYPE=Audio`, `MEDIA_CATEGORY=Capture`, target `PW_KEY_TARGET_OBJECT` set to the sink's monitor node (e.g. `alsa_output.pci-0000_00_1f.3.analog-stereo.monitor`). Negotiate `SPA_AUDIO_FORMAT_F32` at 48kHz stereo. In the `process` callback, `stream.dequeue_buffer()` provides `f32` PCM samples.

**Encoding (both paths):**

Opus at 64-128 kbit/s, configured with `Application::LowDelay` (5ms frame size for minimum latency, 26.5ms default for better quality). Built-in Packet Loss Concealment handles dropped QUIC datagrams gracefully.

**Synchronization:**

Both capture paths produce monotonic timestamps alongside PCM data — ALSA via `snd_pcm_status_get_tstamp()`, PipeWire via `spa_meta_header.pts`. Video frames carry capture timestamps from the same clock. The client uses a jitter buffer to present audio and video aligned by timestamp. For remote desktop use, lip-sync accuracy of ±40ms is acceptable.

---

## 6. Network Protocol

### 6.1 Transport: QUIC over Embedded Tailscale

**Ghostframe never opens a port on any physical or virtual network interface.** All network communication occurs exclusively within the Tailscale mesh network, via an embedded userspace Tailscale node linked directly into the binary. No UDP socket is bound on `0.0.0.0`, no port is exposed on `eth0`, `wlan0`, or any LAN/WAN-facing interface. The server is invisible to port scanners, firewall-irrelevant, and unreachable from outside the user's tailnet.

This is achieved by embedding Tailscale as a library (`libtailscale`, a C library built from Go's `tsnet` package) and driving QUIC as a pure state machine (`quinn-proto`, the no-I/O QUIC protocol crate).

**Why not the high-level `quinn` crate:** `quinn` owns a kernel UDP socket and performs I/O directly via `recvmsg`/`sendmsg` syscalls. `libtailscale` provides a userspace network stack (gVisor netstack) — its file descriptors are virtual, not kernel sockets. The two are incompatible. `quinn-proto` performs no I/O at all; it is a deterministic state machine that accepts byte buffers as input and produces byte buffers as output. This allows clean integration with any I/O source, including libtailscale's userspace stack.

**Architecture:**

```
┌──────────────────────────────────────────────────────┐
│                ghostframe-lib process                │
│                                                      │
│  ┌──────────────────┐     ┌───────────────────────┐  │
│  │  libtailscale    │     │  quinn-proto          │  │
│  │  (Go runtime,    │◄───▶│  (QUIC state machine, │  │
│  │   WireGuard,     │     │   no I/O, pure Rust)  │  │
│  │   control plane, │     │                       │  │
│  │   NAT traversal, │     │  Streams + Datagrams  │  │
│  │   DERP relay)    │     │                       │  │
│  │                  │     └───────────────────────┘  │
│  │  UDP read/write  │                                │
│  │  via userspace   │     No kernel sockets.         │
│  │  netstack        │     No listening ports.        │
│  └──────────────────┘     No firewall exposure.      │
│                                                      │
└──────────────────────────────────────────────────────┘
         │
    Tailscale WireGuard tunnel
    (encrypted, authenticated, NAT-traversed)
         │
┌──────────────────────────────────────────────────────┐
│                  Client                              │
│  Browser: WebTransport to tailnet hostname           │
│  Native:  Same embedded libtailscale + quinn-proto   │
└──────────────────────────────────────────────────────┘
```

**I/O bridge event loop:**

The core transport loop bridges libtailscale's UDP read/write with quinn-proto's state machine:

```rust
loop {
    // Read raw UDP from Tailscale userspace stack (C FFI)
    let (bytes, src_addr) = tailscale_read_udp(ts_conn);
    let now = Instant::now();

    // Feed into quinn-proto — pure computation, no syscalls
    if let Some((handle, event)) = endpoint.handle(
        now, src_addr, None, None, bytes, &mut buf
    ) {
        // Process new connections, stream data, datagrams
    }

    // Drain outgoing packets from quinn-proto
    for (handle, conn) in &mut connections {
        while let Some(transmit) = conn.poll_transmit(now, max, &mut buf) {
            // Write raw UDP via Tailscale userspace stack (C FFI)
            tailscale_write_udp(ts_conn, &transmit.destination, &buf);
        }
    }
}
```

**Authentication and identity:** Tailscale authenticates both endpoints via its control plane. The server appears as a named node in the tailnet (e.g. `ghostframe-server.tailnet-name.ts.net`). ACL tags control which users/devices can connect. No TLS certificates, OAuth tokens, or passwords are managed by Ghostframe — Tailscale handles all identity.

**NAT traversal:** Tailscale establishes direct WireGuard tunnels through NAT via STUN and hard-NAT traversal techniques. When direct connection fails, traffic is relayed through Tailscale's DERP servers (encrypted end-to-end, Tailscale cannot decrypt). This is transparent to the QUIC layer.

**Browser client access:** For the WebTransport browser client, the server generates an ephemeral self-signed TLS certificate at startup. The cert's SHA-256 hash is made available via Tailscale's built-in HTTPS serving (using Tailscale-provisioned LetsEncrypt certificates). The browser connects to `https://ghostframe-server.ts.net/` to load the client page (which embeds the ephemeral cert hash), then opens a WebTransport session to the QUIC endpoint using `serverCertificateHashes`. All of this occurs within the tailnet — no public internet exposure.

**Service discovery:** Tailscale MagicDNS assigns a stable hostname and `100.x.y.z` IP to the embedded node. Clients connect by hostname. No mDNS, no STUN/TURN, no manual IP configuration.

### 6.2 Stream Multiplexing

| Channel | Transport | Content | Priority |
|---------|-----------|---------|----------|
| Input | QUIC unidirectional stream (reliable, client→server) | Keyboard, mouse, touch events | 1 (highest) |
| Video frames | QUIC datagrams (unreliable, app-level retry for H.264) | New/changed tiles + H.264 fragment retries (until superseded) | 2 |
| Audio | QUIC datagrams (unreliable) | Opus packets | 3 |
| Cursor | QUIC datagrams (unreliable) | Cursor image + position updates | 4 |
| Control | QUIC bidirectional stream (reliable) | Session setup, display config, codec negotiation, palette table updates | 4 |
| Extensions (reliable) | QUIC bidirectional stream per channel | Custom channel data, length-prefixed messages | 5 |
| Extensions (unreliable) | QUIC datagrams, multiplexed via `type=0xFF` | Custom channel data, fragmented + reassembled | 5 |
| Clipboard | QUIC bidirectional stream (reliable) | Copy/paste data | 5 |
| Receiver feedback | QUIC control stream (reliable, client→server) | Bandwidth estimation: OWD, loss, suspension detection (every 100ms) | 5 |
| Refinement | QUIC datagrams (unreliable, app-level retry) | CDF 5/3 wavelet bit-plane passes 1...N + ACKs | 6 (lowest) |

### 6.3 Custom Extension Channels

The protocol supports registering application-defined extension channels that piggyback on the QUIC connection. This allows compositors, native clients, or third-party modules to exchange arbitrary data without modifying the core protocol. The library handles all QUIC framing, fragmentation, and reassembly — callers send and receive opaque byte buffers of any size and never deal with MTU limits.

**Use cases:**

- Enlightenment native client syncing virtual desktop list, active workspace, window metadata for deep UI integration (e.g. automatically creating local virtual desktops matching the remote server)
- Compositor-specific features: window thumbnails, taskbar state, notification forwarding
- File transfer, USB redirection, or serial port tunneling by third-party modules
- Custom clipboard formats beyond plain text/images

**Channel registration (C API):**

```c
/* Delivery mode for an extension channel */
typedef enum {
    GF_CHANNEL_RELIABLE,    /* QUIC bidirectional stream — ordered, retransmitted */
    GF_CHANNEL_UNRELIABLE,  /* QUIC datagrams — unordered, no retransmission */
} gf_channel_mode_t;

/* Callback invoked when data arrives on a registered channel.
 * `data` and `len` contain the complete message (reassembled if fragmented).
 * The buffer is valid only for the duration of the callback.
 * Called from the transport thread — keep processing short or copy data. */
typedef void (*gf_channel_recv_cb)(const char *channel_name,
                                    const uint8_t *data,
                                    uint32_t len,
                                    void *user_data);

/* Callback invoked when the remote peer registers (or deregisters)
 * a matching channel. This allows feature negotiation:
 * if the remote side doesn't register "e.desktop-sync", the local
 * side knows not to send desktop sync messages. */
typedef void (*gf_channel_peer_cb)(const char *channel_name,
                                    int peer_registered, /* 1 = registered, 0 = deregistered */
                                    void *user_data);

/* Register a custom extension channel.
 * `channel_name` — unique identifier, namespaced by convention
 *   (e.g. "e.desktop-sync", "gf.file-transfer", "org.example.serial").
 *   Max 64 bytes, ASCII alphanumeric + dots + hyphens.
 * `mode` — reliable (QUIC stream) or unreliable (QUIC datagrams).
 * `recv_cb` — called when data arrives. NULL if send-only.
 * `peer_cb` — called when remote peer registers/deregisters. NULL to ignore.
 * Returns 0 on success, -1 if name already registered or invalid. */
int gf_server_register_channel(gf_server_t *server,
                                const char *channel_name,
                                gf_channel_mode_t mode,
                                gf_channel_recv_cb recv_cb,
                                gf_channel_peer_cb peer_cb,
                                void *user_data);

/* Send data on a registered extension channel.
 * The library handles fragmentation for messages exceeding MTU.
 * For reliable channels, delivery is guaranteed and ordered.
 * For unreliable channels, messages may be dropped but are never
 * partially delivered — the library reassembles fragments and
 * discards incomplete messages.
 * `data` is copied internally; the caller may free it after this call returns.
 * Returns 0 on success, -1 if channel not registered or peer not connected. */
int gf_server_channel_send(gf_server_t *server,
                            const char *channel_name,
                            const uint8_t *data,
                            uint32_t len);

/* Deregister a custom extension channel.
 * Notifies the remote peer via peer_cb if they registered a matching channel.
 * Pending outgoing data is flushed (reliable) or dropped (unreliable). */
int gf_server_deregister_channel(gf_server_t *server,
                                  const char *channel_name);
```

**Equivalent Rust API:**

```rust
impl GhostframeServer {
    /// Register a custom extension channel.
    /// Returns a ChannelHandle for sending data.
    pub fn register_channel(
        &self,
        name: &str,
        mode: ChannelMode,
        on_recv: impl Fn(&[u8]) + Send + Sync + 'static,
        on_peer: Option<impl Fn(bool) + Send + Sync + 'static>,
    ) -> Result<ChannelHandle>;
}

impl ChannelHandle {
    /// Send data on this channel. Any size — fragmentation is internal.
    pub fn send(&self, data: &[u8]) -> Result<()>;

    /// Check if the remote peer has registered a matching channel.
    pub fn peer_available(&self) -> bool;
}

pub enum ChannelMode {
    Reliable,
    Unreliable,
}
```

**Client-side registration** uses the same API — both server and client can register channels, and channel names are matched by string. A channel only becomes active when both sides have registered the same name. If only one side registers, `peer_available()` returns false and `send()` returns an error.

**Wire format for extension channels:**

Reliable channels open a dedicated QUIC bidirectional stream per channel. The stream carries length-prefixed messages:

```
┌──────────────┬─────────────────────┐
│ len (u32 LE) │ message payload     │
└──────────────┴─────────────────────┘
```

Unreliable channels multiplex on QUIC datagrams alongside video/audio, distinguished by a channel header:

```
┌─────────────┬──────────────┬───────┬───────┬─────────────────┐
│ type = 0xFF │ channel_id   │ msg_id│ frag  │ payload         │
│ (u8)        │ (u16)        │ (u16) │ idx/  │                 │
│             │              │       │ total │                 │
│             │              │       │ (u8/  │                 │
│             │              │       │  u8)  │                 │
└─────────────┴──────────────┴───────┴───────┴─────────────────┘
```

`channel_id` is assigned during registration handshake (maps to channel name). `msg_id` groups fragments of a single message. The receiver reassembles all fragments of a `msg_id` before delivering to the callback. If any fragment is lost, the entire message is discarded (no partial delivery).

**Bandwidth management:** Extension channels share bandwidth with video/audio. The library prioritizes core protocol traffic (input > video > audio > cursor) over extension channels. Extension data is sent only when spare bandwidth exists, unless the channel is marked with `GF_CHANNEL_PRIORITY_HIGH` (future API addition). Reliable channels participate in QUIC's flow control and congestion control automatically.

**Example: Enlightenment desktop sync channel:**

```c
// In the Enlightenment module (server side)
void on_desktop_peer(const char *name, int registered, void *data) {
    if (registered)
        send_full_desktop_state();  // initial sync
}

void init_desktop_sync(gf_server_t *server) {
    gf_server_register_channel(server, "e.desktop-sync",
        GF_CHANNEL_RELIABLE, NULL, on_desktop_peer, NULL);
}

void on_desktop_changed(int desktop_index, const char *name) {
    // Pack a small message: { "type": "switch", "index": 2, "name": "Code" }
    uint8_t buf[256];
    int len = pack_desktop_event(buf, desktop_index, name);
    gf_server_channel_send(server, "e.desktop-sync", buf, len);
}

// In the Enlightenment native client (client side)
void on_desktop_recv(const char *name, const uint8_t *data,
                      uint32_t len, void *user_data) {
    // Unpack and create/switch local virtual desktops to match remote
    DesktopEvent ev = unpack_desktop_event(data, len);
    if (ev.type == DESKTOP_SWITCH)
        e_desk_show(e_desk_at(ev.index));
    else if (ev.type == DESKTOP_LIST)
        sync_local_desktops(ev.desktops);
}
```

### 6.4 Video Datagram Format

Each QUIC datagram is MTU-limited (~1200 bytes). Large frames are fragmented:

```
┌─────────────────────────────────────────┐
│ Datagram Header (12 bytes)              │
│ ┌─────────────┬──────────┬────────────┐ │
│ │ frame_seq   │ frag_idx │ frag_total │ │
│ │ (u32)       │ (u16)    │ (u16)      │ │
│ ├─────────────┴──────────┴────────────┤ │
│ │ timestamp_us (u32)                  │ │
│ └─────────────────────────────────────┘ │
├─────────────────────────────────────────┤
│ Tile Header (per tile, 8 bytes)         │
│ ┌──────────┬──────────┬───────────────┐ │
│ │ tile_x   │ tile_y   │ codec (u7)    │ │
│ │ (u16)    │ (u16)    │ + lz4 (u1)    │ │
│ ├──────────┴──────────┴───────────────┤ │
│ │ generation (u8) │ payload_len (u16) │ │
│ └─────────────────────────────────────┘ │
├─────────────────────────────────────────┤
│ Tile Payload (variable)                 │
│ Codec-specific encoded data             │
│ For PalRLE: first byte = palette_id     │
│ (u8), remainder = packed nibble pairs   │
│ (4-bit color index + 4-bit run length)  │
└─────────────────────────────────────────┘
```

**Codec identifiers:**

| ID | Codec | Description |
|----|-------|-------------|
| 0 | Skip | Unchanged tile |
| 1 | H264 | NAL unit fragment (part of full-frame H.264 stream) |
| 2 | PalRLE | Palettized RLE (4-bit color index + 4-bit run length) |
| 3 | Solid | Single color fill |
| 4 | BC1 | GPU texture compressed |
| 5 | Raw | Pixel data (optionally LZ4-compressed) |
| 6 | CDF53 | Progressive CDF 5/3 integer wavelet pass |

### 6.5 Bandwidth Estimation and Adaptation

Bandwidth estimation uses two layers: QUIC's transport-level congestion controller provides the send-rate ceiling, and an application-level estimator provides a more responsive bandwidth signal tuned for WiFi networks and real-time streaming.

**Why two layers:** QUIC's congestion controller (BBR or CUBIC) manages the congestion window and pacing rate at the packet level. It correctly accounts for all traffic including datagrams. However, WiFi networks have characteristics that confuse transport-level estimators: variable PHY rates (10 Mbps to 1 Gbps on 802.11ax depending on signal quality), shared-medium contention, and brief "suspensions" (100-200ms gaps from channel scanning, power management, or beacon processing) that look like severe packet loss but aren't real congestion. BBRv2's ProbeBW-Cruise phase can lock the pacing rate too low for up to 60 rounds after a WiFi suspension, causing unnecessary quality drops that persist for seconds.

The application-level estimator observes actual delivery patterns and feeds the encoding pipeline directly — H.264 target bitrate, tile codec selection thresholds, and refinement scheduler pacing.

**Application-level bandwidth estimation (GCC-inspired delay-gradient):**

The receiver timestamps every arriving datagram. The sender tags each datagram with its send timestamp and sequence number. The receiver computes one-way delay variation:

```
one_way_delay_delta = (recv_time[n] - recv_time[n-1])
                    - (send_time[n] - send_time[n-1])

Positive delta → queue is building (congestion)
Negative delta → queue is draining (spare capacity)
Zero delta     → stable
```

A Kalman filter smooths the delay gradient into a bandwidth estimate:

```
State:     estimated_bandwidth, bandwidth_variance
Measurement: observed inter-arrival rate (bytes between datagrams / time between datagrams)
Prediction:  previous estimate ± variance
Update:      weight measurement against prediction based on variance

The filter naturally handles WiFi jitter — individual noisy samples
are smoothed out, while sustained trends (real congestion or real
capacity increase) are tracked within a few RTTs.
```

**WiFi suspension detector:**

WiFi suspensions produce a distinctive pattern: a gap of 100-200ms with no packet arrivals, followed by a burst of packets delivered at line rate (they were queued in the AP's buffer during the suspension). This is not congestion — the bandwidth hasn't changed, just the timing.

```
On each received datagram:
  recv_gap = recv_time[n] - recv_time[n-1]

  if recv_gap > SUSPENSION_THRESHOLD (100ms):
    // Mark next SUSPENSION_WINDOW (500ms) of samples as "recovery"
    // During recovery:
    //   - Don't reduce bandwidth estimate
    //   - Don't count burst arrivals as bandwidth increase
    //   - Feed Kalman filter with pre-suspension estimate as prior
    suspension_recovery_until = now + 500ms

  if now < suspension_recovery_until:
    // Use pre-suspension bandwidth estimate, ignore noisy samples
    skip Kalman update for this sample
```

Without this detector, a 150ms WiFi suspension causes the encoder to panic-downshift to minimum quality. With it, the system maintains the previous bitrate estimate through the glitch and resumes normally.

**Feeding the encoding pipeline:**

The application-level bandwidth estimate is used by three consumers:

| Consumer | How it uses bandwidth estimate |
|----------|-------------------------------|
| H.264 encoder | `target_bitrate = estimated_bandwidth × video_fraction (0.7)`. Updated every frame. Encoder responds within 1-2 frames via CBR rate control. |
| Tile codec selection | Lower bandwidth → shift classification thresholds: tiles that would be H.264 at high bandwidth use CDF 5/3 lossy instead. Magnitude threshold for H.264 entry increases. |
| Refinement scheduler | `available_for_refinement = estimated_bandwidth × refinement_fraction (0.2)`. Directly controls `passes_per_round`. |

**Congestion response (immediate actions):**

When the delay gradient turns sharply positive (queue building), or when QUIC reports packet loss:

1. Immediately halve H.264 target bitrate (takes effect next frame)
2. Increase CDF 5/3 quantization on non-text tiles (fewer bit-planes sent)
3. Pause refinement entirely (zero refinement passes until gradient stabilizes)
4. Drop Opus audio to lower bitrate (128→64 kbit/s)
5. Increase tile skip threshold — require higher magnitude to classify as "changed"

**Recovery (gradual):**

When the delay gradient returns to zero/negative and QUIC congestion window grows:

1. Ramp H.264 bitrate up by 10% per RTT (not instantly — avoids re-triggering congestion)
2. Restore tile classification thresholds over 500ms
3. Resume refinement at 50% of previous rate, increase to full over 2s
4. Restore Opus bitrate

**Receiver feedback message (lightweight, periodic):**

The client sends a compact feedback report every 100ms (or every 30 received datagrams, whichever comes first) on the control stream:

```rust
pub struct ReceiverFeedback {
    pub timestamp_ns: u64,          // client monotonic clock
    pub datagrams_received: u32,    // since last feedback
    pub datagrams_lost: u32,        // gaps detected in sequence numbers
    pub min_one_way_delay_us: u32,  // minimum OWD in this interval
    pub max_one_way_delay_us: u32,  // maximum OWD (detects jitter)
    pub suspension_detected: bool,  // client saw >100ms gap
}
```

The server uses this to cross-validate its own QUIC-level stats and to run the delay-gradient estimation from the receiver's perspective. The `suspension_detected` flag lets the server avoid reacting to WiFi glitches even before it observes the effects in its own send path.

---

## 7. Display Configuration (Multi-Monitor)

### 7.1 Client → Server Layout Negotiation

On connection, client sends a `DisplayLayout` message over the control stream:

```rust
pub struct DisplayLayout {
    pub monitors: Vec<MonitorInfo>,
}

pub struct MonitorInfo {
    pub width: u32,
    pub height: u32,
    pub refresh_hz: u32,
    pub scale_factor: f32,        // e.g. 2.0 for HiDPI
    pub position: (i32, i32),     // position in virtual desktop (px)
    pub physical_width_mm: u32,
    pub physical_height_mm: u32,
    pub name: String,             // e.g. "DELL U2723QE"
}
```

### 7.2 Server-Side Virtual Display Creation

The server daemon (or compositor module) applies the layout:

**X11 (ghostframe-xdaemon):**
1. Generate EDID blob per monitor (from `MonitorInfo` physical size, resolution, name)
2. For headless: use EVDI kernel module to create virtual DRM connectors, or VKMS
3. Write EDID via sysfs: `/sys/kernel/debug/dri/0/VIRTUAL-1/edid_override`
4. Trigger hotplug uevent
5. Xrandr to position outputs: `xrandr --output VIRTUAL-1 --mode 2560x1440 --pos 0x0`
6. Xrandr for multi-monitor: `xrandr --output VIRTUAL-2 --mode 2560x1440 --right-of VIRTUAL-1`

**Enlightenment module:**
- Same EDID/EVDI path, but E auto-detects hotplug and configures outputs
- Or use E's internal screen config API if available

**Wayland (future: sway/wlroots):**
- `swaymsg output HEADLESS-1 resolution 2560x1440 position 0 0`
- Or wlroots API for output creation/configuration

### 7.3 Dynamic Reconfiguration

When client resizes browser window or plugs/unplugs a monitor:
1. Client sends updated `DisplayLayout` over control stream
2. Server updates EDID + triggers hotplug (or calls compositor API)
3. Compositor reconfigures, begins producing frames at new resolution
4. Server sends `DisplayAck` to client confirming new layout
5. Client adjusts decode/render pipeline

---

## 8. Client Architecture

### 8.1 Browser Client (Primary)

| Layer | Technology | Purpose |
|-------|-----------|---------|
| Transport | WebTransport | QUIC datagrams + streams to server |
| H.264 decode | WebCodecs `VideoDecoder` | Hardware-accelerated decode, `optimizeForLatency: true` |
| Tile decode | WebAssembly (Rust→wasm32) + WebGPU compute | Palettized RLE, BC1 decode in WASM; CDF 5/3 inverse wavelet in WebGPU compute shader |
| Rendering | WebGPU | Composit decoded tiles into framebuffer; `importExternalTexture` for H.264 frames |
| Audio | WebCodecs `AudioDecoder` + AudioWorklet | Opus decode → low-latency playback |
| Input | Pointer Lock API + Keyboard Lock API | Mouse capture, system key capture (fullscreen) |
| Clipboard | Clipboard API | Copy/paste sync |

**Frame assembly:** The client maintains a framebuffer as a WebGPU texture. Each arriving tile updates its region. H.264 decoded `VideoFrame` objects are composited via `importExternalTexture`. Non-H.264 tiles are decoded in WASM and uploaded as texture sub-regions.

### 8.2 Native Client (Multi-Monitor)

Required for:
- True multi-monitor (independent outputs with correct EDID propagation)
- Physical monitor size detection (not available in browsers)
- System-level input capture without fullscreen requirement

Uses the same Rust `ghostframe-lib` decode logic compiled natively. Renders via `wgpu` (WebGPU on native Vulkan). Enumerates local monitors via OS APIs and generates `DisplayLayout` messages.

---

## 9. Testing Strategy

### 9.1 End-to-End Tests (Priority: Day One)

E2E tests validate the full pipeline from X server pixels to browser rendering. They run the real protocol stack over a real (self-hosted) Tailscale network, with no mocks. Any issues found in `playwright-rust` should be contributed upstream rather than worked around with a Node.js fallback.

**Infrastructure:**

- `testcontainers-rs` orchestrates Docker containers from within `cargo test`
- `headscale` (self-hosted Tailscale coordination server) runs in a container, issues pre-auth keys
- `playwright-rust` drives headless Chromium in the client container, takes screenshots, compares pixels

**Test topology:**

```
cargo test e2e::red_square

  testcontainers-rs creates Docker network "ghostframe-test":
  ┌──────────────────────────────────────────────────────────┐
  │                                                          │
  │  ┌─────────────┐                                         │
  │  │ Headscale   │  coordination server                    │
  │  │ container   │  issues 2 pre-auth keys                 │
  │  └──────┬──────┘                                         │
  │         │                                                │
  │  ┌──────▼──────┐         ┌────────────────────────────┐  │
  │  │ Server      │ tailnet │ Client                     │  │
  │  │ container   │◄───────►│ container                  │  │
  │  │             │         │                            │  │
  │  │ Xvfb :99    │         │ Chromium (headless)        │  │
  │  │ xdaemon     │         │ playwright-rust connects   │  │
  │  │ tailscale   │         │ via CDP                    │  │
  │  │             │         │                            │  │
  │  │ test-pattern│         │ screenshots + pixel assert │  │
  │  │ draws known │         │ SSIM against reference     │  │
  │  │ pixels      │         │                            │  │
  │  └─────────────┘         └────────────────────────────┘  │
  └──────────────────────────────────────────────────────────┘
```

**Test containers (built as part of the project):**

`ghostframe/test-server`:
- Ubuntu 24.04 + Xvfb + tailscale CLI
- `ghostframe-xdaemon` binary (built from source)
- `ghostframe-test-pattern` binary — small X11 app (`x11rb` crate) that draws known pixel patterns on command
- GPU passthrough via `--gpus all` (NVIDIA) or `--device /dev/dri` (Intel/AMD VA-API)
- Entrypoint: joins headscale via pre-auth key, starts Xvfb at 1920×1080, starts xdaemon with hardware encoding, signals readiness

`ghostframe/test-client`:
- Ubuntu 24.04 + Chromium + tailscale CLI
- Chromium launched with `--remote-debugging-port=9222` for CDP access
- Entrypoint: joins headscale via pre-auth key, starts Chromium, signals readiness

**Test pattern app (`ghostframe-test-pattern`):**

A small Rust binary using `x11rb` that draws deterministic pixel patterns on the X server. Each pattern is a subcommand:

| Pattern | What it draws | Tests |
|---------|---------------|-------|
| `--solid-red` | 200×200 red square at (100,100) | Basic pixel accuracy, tile classification (solid fill) |
| `--text-grid` | White monospace text on black background | Text clarity, palettized codec, build-to-lossless |
| `--checkerboard` | 1px alternating black/white | Worst-case for lossy codecs, verifies lossless path |
| `--spinner` | 60fps rotating blue rectangle at (700,500) | H.264 activation, codec hysteresis |
| `--gradient` | Smooth color gradient across full screen | BC1 banding detection, CDF 5/3 wavelet path |
| `--mixed` | All of the above simultaneously | Multi-codec tile classification in single frame |
| `--flash` | Alternates full red/blue at 1Hz | Codec transition timing, magnitude detection |

Each pattern writes exact expected pixel values to a JSON manifest alongside a reference PNG, enabling automated comparison.

**Example E2E test:**

```rust
use testcontainers::{core::WaitFor, runners::AsyncRunner, GenericImage, ImageExt};
use playwright::Playwright;

#[tokio::test]
async fn e2e_solid_red_square_renders_correctly() {
    let network = "ghostframe-e2e";

    // 1. Start Headscale
    let headscale = GenericImage::new("headscale/headscale", "0.26.0")
        .with_network(network)
        .with_container_name("headscale")
        .with_exposed_port(8080.tcp())
        .with_wait_for(WaitFor::message_on_stdout("Listening"))
        .start().await.unwrap();

    let server_key = headscale_create_key(&headscale, "testuser").await;
    let client_key = headscale_create_key(&headscale, "testuser").await;

    // 2. Start server: Xvfb + xdaemon + tailscale + GPU
    let server = GenericImage::new("ghostframe/test-server", "latest")
        .with_network(network)
        .with_env_var("TS_AUTHKEY", &server_key)
        .with_env_var("TS_LOGIN_SERVER", "http://headscale:8080")
        .with_privileged(true)       // GPU device access
        .with_mount("/dev/dri:/dev/dri")  // VA-API (Intel/AMD)
        .with_wait_for(WaitFor::message_on_stdout("ghostframe: ready"))
        .start().await.unwrap();

    // 3. Draw test pattern
    docker_exec(&server, "ghostframe-test-pattern --solid-red").await;
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 4. Start client: Chromium + tailscale
    let client = GenericImage::new("ghostframe/test-client", "latest")
        .with_network(network)
        .with_env_var("TS_AUTHKEY", &client_key)
        .with_env_var("TS_LOGIN_SERVER", "http://headscale:8080")
        .with_exposed_port(9222.tcp())
        .with_wait_for(WaitFor::message_on_stdout("tailscale: ready"))
        .start().await.unwrap();

    // 5. Playwright drives Chromium
    let pw = Playwright::initialize().await.unwrap();
    pw.prepare().unwrap();
    let browser = pw.chromium().connect_over_cdp(
        &format!("ws://{}:{}/",
            client.get_host().unwrap(),
            client.get_host_port_ipv4(9222).unwrap())
    ).await.unwrap();
    let context = browser.context_builder().build().await.unwrap();
    let page = context.new_page().await.unwrap();

    page.goto_builder("https://ghostframe-server.test/")
        .goto().await.unwrap();
    page.wait_for_selector("#ghostframe-canvas").await.unwrap();
    tokio::time::sleep(Duration::from_secs(1)).await;

    // 6. Screenshot and compare
    let screenshot_bytes = page.screenshot_builder()
        .full_page(true)
        .screenshot().await.unwrap();

    let actual = image::load_from_memory(&screenshot_bytes).unwrap();
    let expected = image::open("tests/fixtures/solid_red_expected.png").unwrap();

    // Pixel spot check: center of red square
    let pixel = actual.get_pixel(200, 200);
    assert!(pixel[0] > 240 && pixel[1] < 15 && pixel[2] < 15,
        "Expected red, got {:?}", pixel);

    // Full-frame SSIM
    let ssim = compute_ssim(&actual, &expected);
    assert!(ssim > 0.95, "SSIM {ssim:.4} below 0.95 threshold");
}
```

**E2E test scenarios:**

| Test | Server pattern | Browser assertion | Validates |
|------|----------------|-------------------|-----------|
| `e2e_solid_color` | `--solid-red` | Pixel at (200,200) is red ±10 | Basic pipeline, solid fill codec |
| `e2e_text_clarity` | `--text-grid` | SSIM > 0.99 after 5s idle (lossless refinement) | Palettized codec, build-to-lossless chain |
| `e2e_tile_skip` | `--solid-red`, wait 5s | Two screenshots 2s apart are identical | Skip codec, no visual drift |
| `e2e_h264_motion` | `--spinner` | Two screenshots 500ms apart differ in spinner region | H.264 tile activation |
| `e2e_codec_transition` | `--spinner`, then stop | Region refines from lossy to lossless over 5s | Hysteresis, BC1→palettized→perfect chain |
| `e2e_resolution_change` | Resize browser to 1280×720 | Server X display changes to 1280×720 | Display config propagation, xrandr |
| `e2e_multi_pattern` | `--mixed` | Each region classified correctly | Multi-codec classification in one frame |
| `e2e_audio` | `aplay test_tone.wav` | Web Audio API captures 440Hz tone | Audio pipeline: snd-aloop → Opus → browser |

**CI requirements:**
- Docker-in-Docker or privileged runners (for testcontainers)
- **GPU runner required.** E2E tests use VA-API hardware encoding by default — this is a GPU-first protocol and tests must validate the real hardware encode path. A self-hosted GitHub Actions runner with a GPU (Intel/AMD with VA-API or NVIDIA with NVENC) is expected. Software encoding fallback (`libx264`) may be added later for contributors without GPU access, but is not a priority.
- The GPU runner also validates Vulkan compute shaders (tile diff, BC1 encode) against real hardware rather than Lavapipe software emulation.

### 9.2 Unit Tests

- **Tile engine:** Property-based testing with `proptest` — random frame data, verify codec selection invariants (e.g. unchanged tile always classified as Skip, high-freq low-magnitude never triggers H.264)
- **Codec correctness:** Round-trip encode→decode for each codec, verify lossless codecs produce exact output
- **Protocol parsing:** Fuzz datagram/stream parsers with `cargo-fuzz`
- **Refinement chain:** Verify BC1→palettized→pixel-perfect transitions, verify any change resets state

### 9.3 Integration Tests

- **GPU compute:** Vulkan compute shader tests (tile diff, BC1 encode) run on real GPU hardware. Lavapipe software fallback may be added later for contributors without GPU access.
- **Hardware encoding:** VA-API and NVENC integration tests run on GPU runner — validate DMA-BUF import, encoder configuration, NAL output correctness.
- **Virtual display:** VKMS kernel module for DRM/KMS capture tests; Xvfb for X11 tests.
- **Network simulation:** `turmoil` crate for deterministic network simulation; `tc`/`netem` for real-network adaptive encoding tests.
- **quinn-proto state machine:** Feed synthetic UDP packets, verify QUIC handshake and datagram delivery without any network.

### 9.4 Benchmarks

- **Criterion.rs** for encode latency (p50/p95/p99) across resolutions
- **iai-callgrind** for deterministic instruction-count tracking in CI
- Track: tiles/sec, bytes/tile by codec, GPU utilization, frame-to-wire latency

---

## 10. Crate Dependencies (Preliminary)

### Server

| Crate | Purpose |
|-------|---------|
| `quinn-proto` | QUIC protocol state machine (no I/O) |
| `libtailscale` (C FFI) | Embedded Tailscale node — WireGuard, control plane, NAT traversal |
| `tokio` | Async runtime (for internal task scheduling, not socket I/O) |
| `ash` | Vulkan (compute shaders, DMA-BUF import) |
| `cros-libva` | VA-API H.264 encoding |
| `nvidia-video-codec-sdk` | NVENC H.264 encoding (optional) |
| `pipewire` | Audio capture from PipeWire monitor source (compositor module path) |
| `alsa` | Audio capture from snd-aloop loopback device (headless daemon path) |
| `opus` | Opus audio encoding |
| `drm` / `drm-ffi` | DRM/KMS access for xdaemon |
| `xcb` | X11 connection for xdaemon (XDamage, XTest, xrandr) |
| `lz4_flex` | Optional LZ4 compression for tile payloads |
| `proptest` | Property-based testing |
| `criterion` | Benchmarking |
| `cbindgen` | C header generation (build dependency) |
| `testcontainers` | Docker container orchestration for E2E tests (dev dependency) |
| `playwright` | Browser automation for E2E tests (dev dependency) |
| `image` | PNG loading/pixel comparison for E2E tests (dev dependency) |

### Client (WASM)

| Crate | Purpose |
|-------|---------|
| `wasm-bindgen` | JS ↔ WASM bridge |
| `web-sys` | WebTransport, WebCodecs, WebGPU bindings |
| `lz4_flex` | Optional LZ4 decompression (compiles to WASM) |

### Client (Native)

| Crate | Purpose |
|-------|---------|
| `quinn-proto` | QUIC protocol state machine (no I/O) |
| `libtailscale` (C FFI) | Embedded Tailscale node |
| `wgpu` | WebGPU rendering on native Vulkan |
| `opus` | Opus audio decoding |
| `cpal` | Cross-platform audio output |
| `winit` | Window management, input |

---

## 11. File Structure (Proposed)

```
ghostframe/
├── ghostframe-lib/          # Core library (Rust)
│   ├── src/
│   │   ├── lib.rs           # Public API (Rust)
│   │   ├── ffi.rs           # extern "C" wrapper functions
│   │   ├── tile_engine.rs   # Tile classification + hysteresis
│   │   ├── encoder/
│   │   │   ├── mod.rs
│   │   │   ├── h264_vaapi.rs
│   │   │   ├── h264_nvenc.rs
│   │   │   ├── pal_rle.rs        # Palettized RLE (shared palette + run-length)
│   │   │   ├── bc1.rs
│   │   │   ├── cdf53.rs         # CDF 5/3 integer wavelet (Vulkan compute shader driver)
│   │   │   └── solid.rs
│   │   ├── capture/
│   │   │   ├── dmabuf.rs    # DMA-BUF import + Vulkan compute
│   │   │   ├── tile_diff.comp.glsl  # Tile comparison shader
│   │   │   └── cdf53.comp.glsl     # CDF 5/3 integer wavelet encode/decode shaders
│   │   ├── transport/
│   │   │   ├── tailscale_ffi.rs  # libtailscale C FFI bindings
│   │   │   ├── quic_proto.rs     # quinn-proto state machine driver
│   │   │   ├── io_bridge.rs      # Event loop: libtailscale ↔ quinn-proto
│   │   │   ├── webtransport.rs   # WebTransport framing over quinn-proto
│   │   │   └── protocol.rs       # Datagram/stream format
│   │   ├── audio.rs         # PipeWire capture + Opus encode
│   │   ├── display.rs       # Display layout, EDID generation
│   │   └── input.rs         # Input event types
│   ├── Cargo.toml           # crate-type = ["cdylib", "staticlib", "rlib"]
│   ├── build.rs             # cbindgen to generate ghostframe.h
│   ├── include/
│   │   └── ghostframe.h     # Generated C header (committed for convenience)
│   └── benches/
│       └── encode_bench.rs
│
├── ghostframe-xdaemon/      # X11 standalone daemon
│   ├── src/
│   │   ├── main.rs
│   │   ├── drm_capture.rs   # DRM/KMS framebuffer capture
│   │   ├── xdamage.rs       # XDamage listener
│   │   ├── xinput.rs        # XTest input injection
│   │   └── xrandr.rs        # Output management
│   └── Cargo.toml
│
├── ghostframe-e-module/     # Enlightenment module (C)
│   ├── e_mod_ghostframe.c   # Module entry, damage hooks
│   ├── e_mod_ghostframe.h
│   └── meson.build          # Links against libghostframe.so
│
├── ghostframe-web-client/   # Browser client
│   ├── src/
│   │   ├── index.html
│   │   ├── main.ts          # Entry, WebTransport connection
│   │   ├── decoder.ts       # Tile reassembly, WebCodecs
│   │   ├── renderer.ts      # WebGPU compositing
│   │   ├── audio.ts         # AudioWorklet + Opus decode
│   │   └── input.ts         # Pointer Lock, Keyboard Lock
│   ├── wasm/                # Rust WASM module for tile decode
│   │   ├── src/lib.rs
│   │   └── Cargo.toml
│   └── package.json
│
├── ghostframe-native-client/ # Native client (multi-monitor)
│   ├── src/
│   │   ├── main.rs
│   │   ├── monitors.rs      # OS monitor enumeration
│   │   ├── edid.rs          # EDID generation from local monitors
│   │   └── render.rs        # wgpu rendering
│   └── Cargo.toml
│
├── tests/                   # E2E test infrastructure
│   ├── e2e/
│   │   ├── mod.rs           # Shared test helpers (container setup, headscale key gen)
│   │   ├── solid_color.rs   # E2E: solid red square test
│   │   ├── text_clarity.rs  # E2E: text lossless refinement
│   │   ├── motion.rs        # E2E: H.264 activation on spinner
│   │   ├── codec_transition.rs  # E2E: codec hysteresis
│   │   ├── resolution.rs    # E2E: dynamic resolution change
│   │   └── audio.rs         # E2E: audio pipeline
│   ├── fixtures/
│   │   ├── solid_red_expected.png
│   │   ├── text_grid_expected.png
│   │   └── ...              # Reference images per test pattern
│   └── containers/
│       ├── test-server/
│       │   ├── Dockerfile
│       │   └── entrypoint.sh
│       └── test-client/
│           ├── Dockerfile
│           └── entrypoint.sh
│
├── ghostframe-test-pattern/ # X11 test pattern generator
│   ├── src/
│   │   ├── main.rs          # CLI: --solid-red, --text-grid, --spinner, etc.
│   │   └── patterns.rs      # Drawing routines via x11rb
│   └── Cargo.toml
│
├── CLAUDE.md                # Context continuity for development
└── README.md
```

---

## 12. Milestones

### M1: Proof of Concept
- ghostframe-xdaemon creates headless X11 session with virtual display (EVDI/VKMS)
- Captures screen via DRM/KMS
- Single codec: H.264 only via VA-API
- QUIC transport via quinn-proto over embedded libtailscale (no open ports)
- Browser client: WebTransport + WebCodecs decode + canvas render
- Single monitor, fixed resolution
- C API (`libghostframe.so`) builds and links
- E2E test infrastructure operational: testcontainers + Headscale + playwright-rust
- `e2e_solid_color` test passes (red square drawn on Xvfb renders correctly in headless Chromium)

### M2: Tile Engine
- 32×32 tile grid with GPU compute shader diff
- Skip unchanged tiles
- Adaptive codec selection: H.264 + palettized RLE + solid fill
- Two-axis classification (frequency × magnitude)
- Hysteresis state machine
- XDamage integration (bypass GPU diff when available)

### M3: Full Codec Suite + Codec Benchmarking
- CDF 5/3 integer wavelet codec via Vulkan compute shaders (encode + decode, lossless-capable)
- BC1 GPU encode via compute shader
- Palettized codec
- Progressive CDF 5/3 refinement with generation-based retry and round-robin scheduling
- Optional LZ4 compression on all non-H.264 payloads
- **Codec benchmark suite:** Measure encode latency, decode latency, compressed size, and PSNR/SSIM for each codec (H.264, BC1, palettized RLE, CDF 5/3) across a standard set of tile content types:
  - Solid color, flat UI, text on background, photographic, gradient, high-motion video frame
  - Each codec benchmarked with and without LZ4 post-compression — measure size reduction vs CPU cost to determine per-codec break-even (e.g. LZ4 likely helps palettized RLE/raw but not BC1/CDF 5/3)
  - Results drive tile classification thresholds: if CDF 5/3 is faster AND smaller than BC1 for a given content class, update the classification rules to prefer CDF 5/3 for that class
  - If CDF 5/3 quality at comparable bitrate exceeds BC1 across all content types, consider dropping BC1 entirely and using CDF 5/3 as the universal lossy-to-lossless tile codec (simplifying the codec suite)
  - Benchmark progressive refinement: measure total bytes to lossless via CDF 5/3 bit-planes vs BC1→palettized→raw chain across content types
  - Publish benchmark results as part of the project documentation

### M4: Multi-Monitor + Display Config
- EDID generation from client layout
- EVDI virtual output creation
- xrandr dynamic reconfiguration
- Native client with monitor enumeration

### M5: Audio + Polish
- PipeWire audio capture
- Opus encode/decode
- A/V sync via shared timestamps
- Bandwidth adaptation
- Enlightenment module

### M6: Production Hardening
- NVENC support (in addition to VA-API)
- Clipboard sync
- File transfer
- Comprehensive test suite
- Performance profiling + optimization

---

## 13. Non-Goals (Explicit)

- **Operation without Tailscale** — Tailscale is not optional. There is no `--listen 0.0.0.0:4443` flag. The embedded Tailscale node is the only network interface.
- **Windows/macOS server support** — Linux only, by design
- **Capturing an existing user desktop session** (xdaemon) — The daemon targets headless servers only. Capturing a live user session is the domain of compositor modules (Enlightenment, sway, etc.)
- **3D/game streaming** — Optimized for productivity (text, code, UI), not game streaming
- **USB device redirection** — Out of scope
- **Printer redirection** — Out of scope
- **Multi-user session broker** — Single-user, single-session per server
- **Raspberry Pi 5 as server** — No hardware encoding. The protocol requires a GPU with VA-API or NVENC.
- **Software-only encoding as a supported configuration** — Software H.264/libx264 may be added later as a contributor convenience, but is not a design target. Performance and quality guarantees only apply to hardware encode paths.
