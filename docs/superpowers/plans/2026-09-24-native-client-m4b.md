# Native client M4b: display negotiation — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the client tell the server what it can display, and make the server honour it — correct resolution and correct scale, without restarting the session.

**Architecture:** The client sends its monitor maximum and scale once, and its window size on each debounced resize. The server applies both through RandR. A `DisplayController` trait keeps the negotiation logic testable without X and is the seam a future Wayland backend slots into, mirroring how `InputInjector` already works.

**Tech Stack:** Rust, x11rb (RandR), smithay-client-toolkit (`wl_output`), VESA CVT reduced blanking.

**Spec:** `docs/superpowers/specs/2026-09-24-native-client-m4b-design.md`

---

## Read this first

**The original Task 1 spike is DONE and it changed the design.** EDID injection
does not work on these connectors — `edid_override` is accepted, stored, and
never read, because the kernel consults it only when a driver's `get_modes`
returns zero modes, and both VKMS and amdgpu's `virtual_display` use
`drm_add_modes_noedid`. Spec §10 has the evidence. **There is no EDID, no root
helper, no debugfs, and no privileged test anywhere in this plan.** If you find
yourself reaching for any of them, re-read §10 first.

**Scale, not millimetres, is the DPI unit.** A future Wayland backend can set
mode and scale but *not* physical size (`wlr-output-management` states
`physical_size` "cannot be changed by clients"). Millimetres are carried
advisory-only. Nothing may read them to make a decision — spec §4.1, §7.

**Nothing above `DisplayController` may mention X, RandR, or millimetres as a
control input.** That rule is what keeps the Wayland port to one new impl.

**`cvt` is your oracle for timings.** `cvt <w> <h> 60 -r` prints the
authoritative reduced-blanking modeline. Generate your own vectors with it
rather than trusting numbers in this plan — including the ones below, which were
generated that way but should still be re-checked.

**Verified facts** (checked against source while writing this plan):

| fact | where |
|---|---|
| Feedback msg types 0x01–0x06 taken; 0x07/0x08 free | `client_caps.rs:15`, `decode_error.rs:20`, `input_inject.rs:14`, `ack.rs:59`, `feedback.rs:1` |
| HELLO is pushed at core construction into an outbox | `ghostframe-client-core/src/lib.rs:214-219` |
| `PollOutput::{Datagram, Stream}` is how the core emits | `ghostframe-client-core/src/event.rs:125` |
| `InputInjector` trait in lib, impl in xdaemon, mock in tests | `input_inject.rs:49`, `ghostframe-xdaemon/src/input_inject.rs:132` |
| Bridge holds `Option<Arc<dyn InputInjector>>` | `io_bridge.rs:803` |
| `GhostframeServer::new(config, ":443", lib_config, input_injector)` | `ghostframe-xdaemon/src/main.rs:186` |
| `randr::set_screen_size(conn, window, w, h, mm_w, mm_h)` exists | x11rb 0.13 `protocol/randr.rs:111` |
| x11rb has a `randr` feature | x11rb 0.13 `Cargo.toml:108` |
| CLI uses x11rb + smithay, **not** winit | `ghostframe-cli/Cargo.toml:22-25` |
| Production Xorg pins `Virtual 1920 1080` | `packaging/xorg-headless-amdgpu.conf` |

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `ghostframe-xdaemon/src/cvt.rs` | CVT-RB timing calculation | 1 |
| `ghostframe-lib/src/transport/display.rs` | Wire format + `DisplayController` trait | 2, 3 |
| `ghostframe-client-core/src/lib.rs` | Emit the two messages | 2 |
| `ghostframe-lib/src/transport/io_bridge.rs` | Dispatch + server-side debounce | 3 |
| `ghostframe-xdaemon/src/display.rs` | `XrandrDisplay`: modes + screen size | 4 |
| `ghostframe-cli/src/display_probe.rs` | Monitor enumeration (X11 + Wayland) | 5 |
| `packaging/xorg-headless-amdgpu.conf` | Raise the framebuffer ceiling | 6 |
| `ghostframe-e2e/tests/display_negotiation.rs` | End-to-end | 7 |

CVT lives as a **module in `ghostframe-xdaemon`**, not a new crate: only xdaemon
needs it, and a new workspace member would also need adding to the test-server
Dockerfile's per-crate manifest list, which fails the e2e image build when
forgotten. YAGNI.

---

## Task 1: CVT reduced-blanking timings

**Files:** Create `ghostframe-xdaemon/src/cvt.rs`; modify `ghostframe-xdaemon/src/main.rs` (add `mod cvt;`)

RandR's `CreateMode` needs a full modeline — dot clock, sync starts and ends,
totals — not just a resolution. This computes it.

- [ ] **Step 1: Write the failing tests**

```rust
// ghostframe-xdaemon/src/cvt.rs
#[cfg(test)]
mod tests {
    use super::*;

    // Generated with `cvt 1920 1080 60 -r`:
    //   Modeline "1920x1080R" 138.50 1920 1968 2000 2080 1080 1083 1088 1111
    #[test]
    fn matches_cvt_for_1920x1080() {
        let t = reduced_blanking(1920, 1080, 60);
        assert_eq!(t.pixel_clock_khz, 138_500);
        assert_eq!((t.h_sync_start, t.h_sync_end, t.h_total), (1968, 2000, 2080));
        assert_eq!((t.v_sync_start, t.v_sync_end, t.v_total), (1083, 1088, 1111));
    }

    // Generated with `cvt 2560 1440 60 -r`:
    //   Modeline "2560x1440R" 241.50 2560 2608 2640 2720 1440 1443 1448 1481
    #[test]
    fn matches_cvt_for_2560x1440() {
        let t = reduced_blanking(2560, 1440, 60);
        assert_eq!(t.pixel_clock_khz, 241_500);
        assert_eq!((t.h_sync_start, t.h_sync_end, t.h_total), (2608, 2640, 2720));
        assert_eq!((t.v_sync_start, t.v_sync_end, t.v_total), (1443, 1448, 1481));
    }
}
```

**Add at least two more of your own**, generated by running `cvt` yourself,
including one non-16:9 aspect — `cvt 1280 1024 60 -r` is a good one because
5:4 exercises the aspect-dependent path. Re-run `cvt` on the two above rather
than trusting them; a transcription error here is exactly the defect class this
project keeps hitting.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p ghostframe-xdaemon cvt
```

Expected: FAIL to compile — the function does not exist.

- [ ] **Step 3: Implement**

VESA CVT 1.2 reduced blanking (RB v1). Fixed parameters: horizontal blanking
160 px, h sync width 32 px, h front porch 48 px, v front porch 3 lines, minimum
v back porch 6 lines, minimum vertical blanking 460 µs. The pixel clock is
rounded **down** to a 0.25 MHz multiple.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub pixel_clock_khz: u32,
    pub h_active: u16,
    pub h_sync_start: u16,
    pub h_sync_end: u16,
    pub h_total: u16,
    pub v_active: u16,
    pub v_sync_start: u16,
    pub v_sync_end: u16,
    pub v_total: u16,
}
```

Work from the VESA algorithm, not these notes alone — they name the constants
but not the rounding order, and rounding is what the tests pin. **When your
numbers disagree with `cvt`, `cvt` is right.**

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-xdaemon cvt
cargo clippy -p ghostframe-xdaemon --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-xdaemon/src/cvt.rs ghostframe-xdaemon/src/main.rs
git commit -m "feat(xdaemon): CVT reduced-blanking timing calculation

RandR CreateMode needs a full modeline, not a resolution. Pinned against
the cvt(1) utility rather than remembered values."
```

---

## Task 2: `DisplayInfo` / `DisplayMode` wire format

**Files:**
- Create: `ghostframe-lib/src/transport/display.rs`
- Modify: `ghostframe-lib/src/transport/mod.rs`, `ghostframe-client-core/src/lib.rs`, `ghostframe-client-core/src/loss_tracker.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// ghostframe-lib/src/transport/display.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_info_round_trips() {
        let msg = DisplayInfoMsg {
            max_width: 2560, max_height: 1440,
            scale_milli: 1500,
            mm_width: 597, mm_height: 336,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf[0], DISPLAY_INFO_MSG_TYPE);
        assert_eq!(DisplayInfoMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn display_mode_round_trips() {
        let msg = DisplayModeMsg { width: 1280, height: 800 };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        assert_eq!(buf[0], DISPLAY_MODE_MSG_TYPE);
        assert_eq!(DisplayModeMsg::decode(&buf), Some(msg));
    }

    #[test]
    fn a_truncated_message_decodes_to_none_rather_than_panicking() {
        // The feedback dispatcher buffers partial stream reads, so a short
        // slice is normal traffic, not corruption.
        let msg = DisplayInfoMsg {
            max_width: 2560, max_height: 1440,
            scale_milli: 1000, mm_width: 597, mm_height: 336,
        };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        for n in 0..buf.len() {
            assert_eq!(DisplayInfoMsg::decode(&buf[..n]), None, "prefix of length {n}");
        }
    }

    #[test]
    fn the_two_types_do_not_collide_with_existing_messages() {
        for taken in [0x01u8, 0x03, 0x04, 0x05, 0x06] {
            assert_ne!(DISPLAY_INFO_MSG_TYPE, taken);
            assert_ne!(DISPLAY_MODE_MSG_TYPE, taken);
        }
        assert_ne!(DISPLAY_INFO_MSG_TYPE, DISPLAY_MODE_MSG_TYPE);
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p ghostframe-lib --lib display
```

- [ ] **Step 3: Implement**

```rust
//! DisplayInfo (0x07) and DisplayMode (0x08): client → server display
//! negotiation on the reliable bidi feedback stream.
//!
//! Wire formats, all big-endian:
//!   DisplayInfo  [0x07][max_w:u16][max_h:u16][scale_milli:u16][mm_w:u16][mm_h:u16]  11 bytes
//!   DisplayMode  [0x08][w:u16][h:u16]                                                5 bytes
//!
//! Separate from HELLO: HELLO is a one-shot capability advertisement and
//! eviction keys on it (M4a), while display state changes throughout a
//! session.
//!
//! **`scale_milli` is authoritative; `mm_*` is advisory.** A future Wayland
//! backend can set mode and scale but NOT physical size -- the
//! wlr-output-management protocol says physical_size "cannot be changed by
//! clients". Millimetres are carried for logs and for a backend that might
//! one day use them; nothing reads them to make a decision. See the M4b
//! design §4.1.
//!
//! Scale is thousandths: 1000 = 1.0, 1500 = 1.5.

pub const DISPLAY_INFO_MSG_TYPE: u8 = 0x07;
pub const DISPLAY_MODE_MSG_TYPE: u8 = 0x08;
pub const DISPLAY_INFO_SIZE: usize = 11;
pub const DISPLAY_MODE_SIZE: usize = 5;
```

Write both structs with `encode(&self, &mut Vec<u8>)` and
`decode(&[u8]) -> Option<Self>`, deriving `Debug, Clone, Copy, PartialEq, Eq`.
Mirror `client_caps.rs`'s shape — it is the established idiom for this stream.

Add `pub mod display;` to `ghostframe-lib/src/transport/mod.rs`.

- [ ] **Step 4: Emit from the client core**

`ghostframe-client-core` must **not** depend on `ghostframe-lib` (that is the
server crate; it would be a cycle). Put the encoders beside `encode_hello`
(`loss_tracker.rs:93`) and add byte-exact oracle tests in
`ghostframe-client-core/tests/` asserting they match what `ghostframe-lib`
decodes — `tests/oracle_feedback.rs` is the pattern
(`assert_eq!(encode_hello(true, false), vec![0x03, 0x01])`).

Extend `ClientConfig` (`lib.rs:105`):

```rust
/// What the client knows about its own display. Deliberately NOT named
/// `DisplayInfo` -- that name belongs to the wire message in
/// `ghostframe-lib`, and client-core cannot depend on the server crate, so
/// two same-named types in different crates would read as one type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientDisplay {
    pub max_width: u16,
    pub max_height: u16,
    /// Thousandths: 1000 = 1.0.
    pub scale_milli: u16,
    /// Advisory only -- see the module docs on why scale is authoritative.
    pub mm_width: u16,
    pub mm_height: u16,
}
```

Push `DisplayInfo` into the outbox at construction when `config.display` is
`Some`, right after HELLO. Add:

```rust
/// Queue a display-mode request. The server clamps it and reports what it
/// actually set through the existing frame-dimensions message -- callers
/// must render what arrives, not what they asked for.
pub fn request_display_mode(&mut self, width: u16, height: u16) {
    self.outbox.push_back(PollOutput::Stream(encode_display_mode(width, height)));
}
```

- [ ] **Step 5: Run the tests**

```bash
cargo test -p ghostframe-lib --lib display
cargo test -p ghostframe-client-core
```

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/transport/display.rs ghostframe-lib/src/transport/mod.rs ghostframe-client-core
git commit -m "feat(protocol): DisplayInfo and DisplayMode messages

Scale is authoritative and millimetres advisory: a Wayland backend can set
mode and scale but not physical size, so carrying mm as the control input
would not survive the port. Encoders live in client-core with byte-exact
oracle tests, since client-core cannot depend on the server crate."
```

---

## Task 3: `DisplayController` trait, dispatch, and debounce

**Files:** Modify `ghostframe-lib/src/transport/display.rs`, `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_display_mode_request_reaches_the_controller() {
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg { width: 1280, height: 800 }.encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs(), vec![(1280, 800, 1000)]);
}

#[tokio::test]
async fn rapid_resizes_collapse_to_one_change() {
    // A resize drag emits continuously. Applying every one would thrash X
    // for sizes the user never settled on.
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    for w in [1000u16, 1100, 1200, 1280] {
        let mut buf = Vec::new();
        DisplayModeMsg { width: w, height: 800 }.encode(&mut buf);
        bridge.dispatch_feedback_bytes(handle, &buf);
    }
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs().len(), 1, "only the final size should be applied");
    assert_eq!(ctl.outputs()[0].0, 1280);
}

#[tokio::test]
async fn a_width_is_aligned_down_to_the_cvt_granularity() {
    // CVT rounds h_active UP to a multiple of 8: `cvt 1283 817 60 -r` gives
    // a 1288-wide mode. Rendering 1288 into a 1283-wide window crops 5 px of
    // the remote desktop off-screen -- content the user cannot see or scroll
    // to. Aligning DOWN leaves a 3 px border instead, which is harmless.
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg { width: 1283, height: 817 }.encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs(), vec![(1280, 817, 1000)],
        "width must align down to a multiple of 8; height is unconstrained");
}

#[tokio::test]
async fn a_tiny_window_does_not_collapse_to_nothing() {
    // Aligning down must not produce a zero or absurd mode when someone
    // drags a window very small.
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg { width: 4, height: 3 }.encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    let (w, h, _) = ctl.outputs()[0];
    assert!(w >= MIN_MODE_WIDTH && h >= MIN_MODE_HEIGHT,
        "expected a floor, got {w}x{h}");
}

#[tokio::test]
async fn a_mode_above_the_ceiling_is_clamped_not_rejected() {
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::with_ceiling(4096, 2160));
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg { width: 8192, height: 4320 }.encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs(), vec![(4096, 2160, 1000)],
        "an oversized request must be clamped so the client still gets a picture");
}

#[tokio::test]
async fn the_scale_from_display_info_is_applied_to_later_modes() {
    // DisplayInfo carries the scale once; every subsequent mode change must
    // carry it through, or a resize would silently reset the user's DPI.
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut info = Vec::new();
    DisplayInfoMsg { max_width: 2560, max_height: 1440, scale_milli: 1500,
                     mm_width: 597, mm_height: 336 }.encode(&mut info);
    bridge.dispatch_feedback_bytes(handle, &info);

    let mut mode = Vec::new();
    DisplayModeMsg { width: 1280, height: 800 }.encode(&mut mode);
    bridge.dispatch_feedback_bytes(handle, &mode);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.outputs(), vec![(1280, 800, 1500)]);
}
```

`test_bridge_with_one_session` exists from M4a. `RecordingController` is yours;
reuse the M4a helpers rather than inventing new ones.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p ghostframe-lib --lib display_
```

- [ ] **Step 3: Define the trait**

```rust
/// Server-side display control. Implemented against X/RandR in
/// `ghostframe-xdaemon`; mocked in tests. Mirrors `InputInjector`
/// (`transport/input_inject.rs`), which exists for the same reason: keep
/// protocol logic testable without a running display server.
///
/// **Nothing above this trait may mention X, RandR, or millimetres.** A
/// future headless-Wayland backend implements the same two methods with
/// `set_custom_mode` and `set_scale`; that port should need no change above
/// this line. See the M4b design §5 and §7.
pub trait DisplayController: Send + Sync {
    /// The framebuffer ceiling. Requests above it are clamped, never rejected.
    fn ceiling(&self) -> (u16, u16);

    /// Apply a resolution and scale together -- they are one user-visible
    /// change, and applying them separately would show an intermediate
    /// state at the wrong size or the wrong font scale.
    fn set_output(&self, width: u16, height: u16, scale_milli: u16)
        -> Result<(), DisplayError>;
}
```

- [ ] **Step 4: Wire dispatch and debounce**

Add `display_controller: Option<Arc<dyn DisplayController>>` to `IoBridge`
beside `input_injector` (`io_bridge.rs:803`), and arms for both message types in
`dispatch_feedback_bytes`.

`DisplayInfo` stores the scale (and logs the advisory millimetres — that is the
only thing that reads them). `DisplayMode` stores the pending size and a
deadline; `set_output` fires from the existing timeout path once the deadline
passes, carrying the stored scale.

```rust
/// How long to wait after the last DisplayMode before applying it.
///
/// **A guess, not a measurement.** Roughly the pause a person makes on
/// releasing a window edge, and far longer than the 2.6-4.9 ms a resolution
/// change costs (M3 spec §10.7), so being wrong means latency on the last
/// resize rather than thrash. If it proves wrong, measure a real resize
/// drag rather than guessing again.
const DISPLAY_MODE_DEBOUNCE_US: u64 = 250_000;

/// Scale before any DisplayInfo arrives. 1.0 -- the server's current
/// behaviour, so a client that never negotiates sees no change.
const DEFAULT_SCALE_MILLI: u16 = 1000;

/// CVT rounds h_active UP to a multiple of 8 (verified: `cvt 1283 817 60 -r`
/// yields a 1288-wide mode). Align requested widths DOWN to this instead:
/// a mode wider than the client's window crops the remote desktop
/// off-screen, while a narrower one leaves a harmless border.
const MODE_WIDTH_GRANULARITY: u16 = 8;

/// Floor so that aligning down, or a client dragging a window to nothing,
/// cannot ask X for a degenerate mode.
const MIN_MODE_WIDTH: u16 = 320;
const MIN_MODE_HEIGHT: u16 = 240;
```

Find how `poll_timeout`/`on_timeout` deadlines are managed in this file and
follow it; do not add a second timing mechanism.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p ghostframe-lib --lib
```

- [ ] **Step 6: Prove the debounce test can fail**

Apply each request immediately instead of on the deadline, confirm
`rapid_resizes_collapse_to_one_change` fails, then revert. A test that cannot
fail is worse than no test.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/display.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(transport): display negotiation dispatch with server-side debounce

DisplayController mirrors InputInjector so negotiation is testable without
X, and is the seam a headless-Wayland backend slots into unchanged.
Debouncing is server-side: a client that spams resizes must not be able to
drive mode switches at will."
```

---

## Task 4: `XrandrDisplay` — the X backend

**Files:**
- Create: `ghostframe-xdaemon/src/display.rs`
- Modify: `ghostframe-xdaemon/src/main.rs`, `ghostframe-xdaemon/Cargo.toml`

- [ ] **Step 1: Add the RandR feature**

`x11rb` needs its `randr` feature (present in x11rb 0.13, `Cargo.toml:108`).

- [ ] **Step 2: Implement the trait**

```rust
/// `DisplayController` backed by X RandR.
///
/// Entirely unprivileged: RandR is an ordinary X client operation and the
/// daemon already holds the display. The earlier EDID design needed root
/// and did not work -- see the M4b design §10.
pub struct XrandrDisplay { /* connection, root window, output, crtc */ }
```

`ceiling()` reads the screen's maximum size from RandR's screen resources
rather than hardcoding 4096x2160 — the config can change and a stale constant
would clamp wrongly.

`set_output(w, h, scale_milli)`:

1. Compute the modeline with `crate::cvt::reduced_blanking(w, h, 60)`.
2. If no mode of that size exists on the output, `CreateMode` + `AddOutputMode`.
   **Check first** — X accumulates modes and a resize drag would otherwise
   create one per step.
3. `SetCrtcConfig` to switch to it.
4. `set_screen_size(conn, root, w, h, mm_w, mm_h)` where the millimetres are
   derived from the scale:

   ```rust
   // X has no scale concept; it reports a physical size and toolkits derive
   // DPI from it. Inverting the usual relation -- mm = px / (96 * scale) *
   // 25.4 -- makes a scale of 1.0 report exactly 96 DPI, and 1.5 report 144.
   fn mm_for_scale(px: u16, scale_milli: u16) -> u32 {
       let dpi = 96.0 * f64::from(scale_milli) / 1000.0;
       ((f64::from(px) / dpi) * 25.4).round() as u32
   }
   ```

   Unit-test `mm_for_scale` directly: scale 1000 on 1920 px must give 508 mm
   (1920/96 = 20 in = 508 mm), and scale 2000 must give half that.

**Order matters**: `SetScreenSize` must not shrink the screen below a CRTC still
using a larger mode, or X returns `BadMatch`. Set the CRTC first when growing
and the screen first when shrinking, or disable the CRTC across the change.
Whichever you choose, say why in a comment — the next person will hit `BadMatch`
and need to know the rule.

- [ ] **Step 3: Wire it into the server**

`ghostframe-xdaemon/src/main.rs:186`:

```rust
let server = match GhostframeServer::new(config, ":443", lib_config, input_injector).await {
```

Add the controller. At five positional parameters this is at the edge of
readable; if you introduce a small struct instead, do it as its own commit so
the mechanical change stays separable.

Construct `XrandrDisplay` after the X-server-ready wait, beside the
`XTestInjector` construction (`main.rs:157-178`), and follow its failure
handling: log and continue with `None` rather than aborting startup.

- [ ] **Step 4: Verify**

```bash
cargo test -p ghostframe-xdaemon
cargo clippy -p ghostframe-xdaemon --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-xdaemon
git commit -m "feat(xdaemon): RandR-backed DisplayController

Unprivileged throughout. Scale becomes a physical size for SetScreenSize,
since X reports mm and toolkits derive DPI from it; a Wayland backend will
pass scale straight to set_scale instead."
```

---

## Task 5: Client-side display enumeration

**Files:**
- Create: `ghostframe-cli/src/display_probe.rs`
- Modify: `ghostframe-cli/src/commands.rs`, `ghostframe-cli/Cargo.toml`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_scale_from_a_normal_monitor() {
        // 2560 px across 597 mm is about 109 DPI -> ~1.13x
        let s = scale_from_mm(2560, 597);
        assert!((1100..=1200).contains(&s), "expected ~1.13, got {s}");
    }

    #[test]
    fn zero_millimetres_falls_back_to_unity() {
        // Projectors and many TVs report 0. Deriving a scale from it would
        // produce a division by zero or an absurd DPI.
        assert_eq!(scale_from_mm(1920, 0), 1000);
    }

    #[test]
    fn an_implausible_dpi_falls_back_to_unity() {
        // 1920 px across 10 mm is not a display.
        assert_eq!(scale_from_mm(1920, 10), 1000);
    }

    #[test]
    fn a_standard_96_dpi_display_is_unity() {
        // 1920 px across 508 mm is exactly 96 DPI.
        assert_eq!(scale_from_mm(1920, 508), 1000);
    }
}
```

Check `derives_scale_from_a_normal_monitor`'s range yourself — it depends on the
rounding you pick. Pick one, make the test match, say which in a comment.

- [ ] **Step 2: Implement both backends**

- **Wayland**: `wl_output.scale` is the scale directly (integer); prefer
  `wp_fractional_scale_v1` where the compositor offers it.
  `wl_output.geometry` supplies the advisory millimetres.
  `smithay-client-toolkit` is already a dependency.
- **X11**: RandR `GetOutputInfo` gives `mm_width`/`mm_height`; derive the scale
  with `scale_from_mm`. Add the `randr` feature to `x11rb` in
  `ghostframe-cli/Cargo.toml` (currently `dri3, present, xfixes, allow-unsafe-code`).

Log which backend ran and whether the fallback fired — "the scale is wrong" is
otherwise very hard to diagnose from the server side.

- [ ] **Step 3: Send the messages**

Populate `ClientConfig.display` at connect. On window resize, call
`request_display_mode` — **do not debounce client-side**; the server does it
(Task 3), and duplicating it would make the effective delay the sum of both.

- [ ] **Step 4: Verify**

```bash
cargo test -p ghostframe-cli
cargo clippy -p ghostframe-cli --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-cli
git commit -m "feat(cli): probe the local display and negotiate it

Wayland reports scale directly; X11 derives it from RandR millimetres.
Implausible physical sizes (projectors and TVs often report zero) fall back
to 1.0 rather than propagating an absurd scale, and which path ran is
logged because wrong scale is otherwise undiagnosable from the server."
```

---

## Task 6: Raise the framebuffer ceiling

**Files:** Modify `packaging/xorg-headless-amdgpu.conf`, `packaging/install.sh`

- [ ] **Step 1: Raise it**

Change `Virtual 1920 1080` to `Virtual 4096 2160`, and add a comment explaining
that `Virtual` is fixed at X startup and is therefore the hard ceiling for every
client — that is not obvious and is the reason the line exists.

- [ ] **Step 2: Measure what it costs**

Spec risk 2 says measure rather than assume. A 4096x2160 32-bit framebuffer is
about 35 MB versus about 8 MB at 1920x1080. Confirm the real figure:

```bash
grep -i VmRSS /proc/$(pgrep -f "Xorg :1")/status
```

under each ceiling. Record both numbers in the commit message. If the delta is
far larger than ~27 MB, something else scales with the ceiling and that is worth
knowing before this ships to a small server.

- [ ] **Step 3: Check the upgrade path**

`install.sh` writes this file. An existing install that keeps the old ceiling
makes the feature look broken. Confirm the script replaces it rather than
skipping when present; if it skips, fix that and say so in the commit message.

- [ ] **Step 4: Validate and commit**

```bash
bash -n packaging/install.sh && echo "install.sh parses"
git add packaging
git commit -m "packaging: raise the framebuffer ceiling to 4096x2160"
```

---

## Task 7: End-to-end display negotiation

**Files:**
- Create: `ghostframe-e2e/tests/display_negotiation.rs`
- Modify: `.github/workflows/e2e.yml` (exemption comment)

No `--privileged`, no debugfs, no root — the pivot removed all of it.

- [ ] **Step 1: Write the test**

A client connects advertising 2560x1440 at scale 1.0, requests a 1280x800 mode,
and the test asserts the server actually changes resolution — observable through
the `Resized` event the client already receives.

Model it on `ghostframe-e2e/tests/eviction.rs` (M4a): same `setup_e2e_server`,
same sharing of the harness tsnet node via `attach_bridge`. Note `wait_for_frame`
discards non-frame events, so a waiter for `Resized` belongs in this test file —
the same reason `eviction.rs` has its own `wait_for_disconnect`.

Assert on the **new dimensions**, not merely that some event arrived: a resize
to the size it already was would otherwise pass.

- [ ] **Step 2: Rebuild the container and run**

```bash
just containers-build
TS_CONTROL_URL=http://127.0.0.1:18080 \
  cargo test -p ghostframe-e2e --test display_negotiation -- --nocapture --test-threads=1
```

Do not pipe either command — a pipe reports the last stage's status, and this
repo has had a failed build look green exactly that way.

- [ ] **Step 3: Keep it out of CI, visibly**

Extend the exemption comment in `e2e.yml` to name `display_negotiation.rs`
alongside `eviction.rs`, `native_client.rs`, `showcase.rs` and `h264.rs`:

```bash
grep -rn "test display_negotiation" .github/workflows/ ; echo "exit=$? (1 = correctly absent)"
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/e2e.yml'))" && echo "yaml ok"
```

- [ ] **Step 4: Commit**

```bash
git add ghostframe-e2e/tests/display_negotiation.rs .github/workflows/e2e.yml
git commit -m "test(e2e): a client negotiates its resolution end to end"
```

---

## Task 8: Make X11 capture follow a mode change

**Added after the e2e proved the milestone incomplete.** Negotiation works
end to end — the server logs `display mode applied width=1280 height=800`
and `set_output` returns `Ok` — but the client never sees a new size,
because the capture pipeline keeps producing frames at the startup
resolution forever.

**Files:** Modify `ghostframe-xdaemon/src/x11_capture.rs`

### What is already true (verified; do not re-do it)

- **The DRM backend already adapts.** `main.rs:372` calls
  `drm_capture::capture()` fresh each frame and takes `geom.width`/`geom.height`
  from the result. Only the X11 path is broken.
- **Clients are told automatically.** `io_bridge.rs::emit_frame_dimensions`
  compares against `last_emitted_dimensions` and retransmits on change. Once
  capture reports the new size, the frame-dimensions message and the client's
  `Resized` event follow with no extra work.
- **The bug is one cached pair.** `x11_capture.rs:111-112` reads
  `screen.width_in_pixels`/`height_in_pixels` **once** in `X11Capture::new()`
  into `self.width`/`self.height`, and allocates `target` from them. Every
  capture uses those values. There is no `RRScreenChangeNotify` subscription
  anywhere in the daemon (`grep -rn "RRScreenChange\|ScreenChangeNotify"
  ghostframe-xdaemon/src/` finds nothing).

- [ ] **Step 1: Write the failing test**

The capture path needs a live X server, so unit-test the part that does not:
extract the resize decision into a pure helper and test that.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_root_geometry_requires_a_resize() {
        assert_eq!(resize_needed((640, 480), (1280, 800)), Some((1280, 800)));
    }

    #[test]
    fn an_unchanged_geometry_does_not() {
        // This runs once per captured frame. Reallocating a
        // multi-megabyte buffer every frame because the check was sloppy
        // would be a worse bug than the one being fixed.
        assert_eq!(resize_needed((1280, 800), (1280, 800)), None);
    }

    #[test]
    fn a_zero_geometry_is_ignored() {
        // X can briefly report 0 during a mode transition. Reallocating to
        // a zero-sized buffer would panic or produce empty frames.
        assert_eq!(resize_needed((1280, 800), (0, 0)), None);
        assert_eq!(resize_needed((1280, 800), (1280, 0)), None);
    }
}
```

Run `cargo test -p ghostframe-xdaemon resize_needed` — expect a compile failure.

- [ ] **Step 2: Implement detection and reallocation**

At the top of the capture call, re-read the root geometry and resize if it
changed:

```rust
/// Decide whether the cached capture geometry must be rebuilt.
///
/// Called once per captured frame, so the unchanged case must be free.
/// A zero dimension is ignored rather than honoured: X can report one
/// briefly mid-transition, and resizing to it would yield empty frames.
fn resize_needed(current: (u16, u16), actual: (u16, u16)) -> Option<(u16, u16)> {
    if actual.0 == 0 || actual.1 == 0 || actual == current {
        return None;
    }
    Some(actual)
}
```

Use `self.conn.get_geometry(self.root)` for the actual size — the file already
uses `get_geometry` on child windows (`x11_capture.rs:218`), so the idiom and
error handling are established there. On a change: update `self.width`/
`self.height`, reallocate `self.target` to `w * h * 4`, and log at INFO with
both old and new sizes (this is a rare, user-visible event worth a line).

**Poll rather than subscribe to `RRScreenChangeNotify`.** One extra
`GetGeometry` round-trip per frame is negligible beside the `GetImage` the
capture already performs, and it cannot miss an event or need a second
connection. If profiling later shows the round-trip matters, the RandR
subscription is the upgrade — say so in a comment so the next reader knows
the choice was deliberate.

**Check whether the strategy needs re-picking.** `pick_strategy` runs once in
`new()`. Decide whether a resize can invalidate it (e.g. a compositor
appearing or a root pixmap being recreated) and say what you concluded — if it
can, re-pick; if not, write down why not.

- [ ] **Step 3: Verify the whole chain end to end**

```bash
just containers-build   # or: docker build --build-arg CARGO_JOBS=6 ...
TS_CONTROL_URL=http://127.0.0.1:18080   cargo test -p ghostframe-e2e --test display_negotiation -- --nocapture --test-threads=1
```

The first test must now pass: `matched=true` with `1280x800` among the
observed sizes.

The second test (`..._larger_than_the_containers_startup_size`) answers an
open question in the spec (§2.1): whether `Virtual`/`Modes` is a hard ceiling
or merely the startup size. **Either outcome is a valid finding — report it,
do not adjust the test to make it pass.** If it fails, record the answer in
spec §2.1 and mark that test `#[ignore]` with the finding as its reason.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-xdaemon/src/x11_capture.rs
git commit -m "fix(xdaemon): X11 capture follows a mode change

X11Capture cached screen.width_in_pixels at construction, so after a RandR
mode change it kept producing frames at the startup size forever and the
client was never told. The DRM backend already re-read geometry per frame;
only this path was affected.

Polls root geometry per capture rather than subscribing to
RRScreenChangeNotify: one GetGeometry is negligible beside the GetImage
already performed, and it cannot miss an event."
```

---

## Done means

- [ ] A client advertising 2560x1440 with a 1280x800 window is served 1280x800 **and the frames actually arrive at that size** (the e2e passes).
- [ ] Scale reaches X as a physical size, so 1.0 reports 96 DPI and 1.5 reports 144.
- [ ] A resize drag produces one mode change, and that test fails if the debounce is removed.
- [ ] A non-aligned window width yields a mode no wider than the window, never wider.
- [ ] CVT timings match `cvt(1)` exactly, on at least four resolutions including one non-16:9.
- [ ] Nothing above `DisplayController` mentions X, RandR, or millimetres as a control input.
- [ ] No root, no debugfs, no privileged container anywhere in the diff.
- [ ] `just ci-local` green, `just containers-build` succeeds.
- [ ] Every number that is a guess says so where it is written.
