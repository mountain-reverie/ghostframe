# Native client M4b: display negotiation and virtual EDID — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the client tell the server what it can display, and make the server honour it — correct resolution and correct DPI, without restarting the session.

**Architecture:** The client sends its monitor maximum plus physical size once, and its window size on each debounced resize. The server applies the mode through RandR (unprivileged) and installs a synthetic EDID through a socket-activated root helper (once per connect). A `DisplayController` trait keeps the negotiation logic testable without X, mirroring how `InputInjector` already works.

**Tech Stack:** Rust, x11rb (RandR), smithay-client-toolkit (`wl_output`), systemd socket activation, VESA EDID 1.4 + CVT reduced blanking.

**Spec:** `docs/superpowers/specs/2026-09-24-native-client-m4b-design.md`

---

## Read this first

**Task 1 is a gate, not a warm-up.** The whole design assumes `edid_override` takes effect on a *running* X server. If it needs a restart, §2 of the spec collapses and Tasks 2, 5, 6, 8 and 9 change shape. Do not start them until Task 1 reports.

**Tasks 3, 4 and 7 survive either outcome.** Negotiation, the controller trait, and client-side enumeration are needed even in a RandR-only world. If Task 1 fails, those three still ship and the EDID half becomes a separate decision.

**`cvt` is your oracle for timings.** `cvt <w> <h> 60 -r` prints the authoritative CVT reduced-blanking modeline. Use it to generate test vectors rather than trusting any number in this plan — including the ones below, which were generated that way but should still be re-checked.

**Verified facts you can build on** (checked against source while writing this plan, but confirm anything you depend on):

| fact | where |
|---|---|
| Feedback msg types 0x01–0x06 are taken; 0x07/0x08 are free | `client_caps.rs:15`, `decode_error.rs:20`, `input_inject.rs:14`, `ack.rs:59`, `feedback.rs:1` |
| HELLO is pushed at core construction into an outbox | `ghostframe-client-core/src/lib.rs:214-219` |
| `PollOutput::{Datagram, Stream}` is how the core emits | `ghostframe-client-core/src/event.rs:125` |
| `InputInjector` trait in lib, impl in xdaemon, mock in tests | `input_inject.rs:49`, `ghostframe-xdaemon/src/input_inject.rs:132` |
| The bridge holds `Option<Arc<dyn InputInjector>>` | `io_bridge.rs:803` |
| `GhostframeServer::new(config, ":443", lib_config, input_injector)` | `ghostframe-xdaemon/src/main.rs:186` |
| x11rb has a `randr` feature and `protocol/randr.rs` | x11rb 0.13 `Cargo.toml:108` |
| The CLI uses x11rb + smithay, **not** winit | `ghostframe-cli/Cargo.toml:22-25` |
| Production Xorg pins `Virtual 1920 1080` | `packaging/xorg-headless-amdgpu.conf` |

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `ghostframe-edid/src/cvt.rs` | CVT-RB timing calculation | 2 |
| `ghostframe-edid/src/lib.rs` | EDID 1.4 base block assembly | 2 |
| `ghostframe-lib/src/transport/display.rs` | `DisplayInfo`/`DisplayMode` wire format + `DisplayController` trait | 3, 4 |
| `ghostframe-client-core/src/lib.rs` | Emit the two messages | 3 |
| `ghostframe-lib/src/transport/io_bridge.rs` | Dispatch + server-side debounce | 4 |
| `ghostframe-edid-helper/src/main.rs` | Root helper: validate, synthesise, write, hotplug | 5 |
| `ghostframe-xdaemon/src/display.rs` | `XrandrDisplay`: RandR + helper client | 6 |
| `ghostframe-cli/src/display_probe.rs` | Monitor enumeration (X11 + Wayland) | 7 |
| `packaging/` | Xorg ceiling, helper unit + socket, install | 8 |
| `ghostframe-e2e/tests/display_negotiation.rs` | Privileged e2e | 9 |

---

## Task 1: Spike — does `edid_override` work on a live X server?

**Throwaway. Write no production code.** Report findings; the spike is deleted afterwards.

**Files:** none committed except the findings write-up.

- [ ] **Step 1: Find the debugfs path by connector name**

```bash
for d in /sys/kernel/debug/dri/*/; do
  [ -d "$d/Virtual-1" ] && echo "found: $d/Virtual-1"
done
```

Needs root. Note which DRI minor `card0-Virtual-1` maps to, and **whether that minor matches the card number** — the spec (§5.1) asserts it may not, and this is where that gets confirmed or corrected.

- [ ] **Step 2: Confirm the files the design depends on exist**

```bash
sudo ls -la /sys/kernel/debug/dri/<minor>/Virtual-1/
```

Expected: `edid_override` and `trigger_hotplug`. **If either is missing, stop and report** — the design depends on both.

- [ ] **Step 3: Capture the before state**

```bash
cat /sys/class/drm/card0-Virtual-1/modes
cat /sys/class/drm/card0-Virtual-1/edid | wc -c   # expect 0 today
```

- [ ] **Step 4: Start an X server on that connector and record what it sees**

Use the e2e container's VKMS config as a starting point (`tests/containers/test-server/xorg-vkms.conf`). Record `xrandr --query` output and the reported physical size (`xrandr` prints `<N>mm x <M>mm`).

- [ ] **Step 5: Inject an EDID and trigger the hotplug**

Generate a known-good EDID for a resolution *not* currently in the mode list. Any valid 128-byte 1.4 block works; a real monitor's EDID copied from `card1-HDMI-A-2` is acceptable for the spike — the point is the mechanism, not the content.

```bash
sudo sh -c 'cat known.edid > /sys/kernel/debug/dri/<minor>/Virtual-1/edid_override'
sudo sh -c 'echo 1 > /sys/kernel/debug/dri/<minor>/Virtual-1/trigger_hotplug'
```

- [ ] **Step 6: Answer the gating question**

Without restarting X:

1. Does `cat /sys/class/drm/card0-Virtual-1/edid` now return the blob?
2. Does `xrandr --query` show the new modes?
3. Does `xrandr` report the physical size from the injected EDID?

**Record all three as yes/no with the actual output.** Question 2 is the gate. If X does not pick up the modes without a restart, say so plainly and stop — do not look for workarounds in this task.

- [ ] **Step 7: Check that it is reversible**

```bash
sudo sh -c 'echo 1 > /sys/kernel/debug/dri/<minor>/Virtual-1/edid_override'   # or truncate
sudo sh -c 'echo 1 > /sys/kernel/debug/dri/<minor>/Virtual-1/trigger_hotplug'
```

Confirm the connector returns to its prior state. A mechanism that cannot be undone is one a failed test leaves behind for the next run.

- [ ] **Step 8: Write up and commit the findings**

Append a `## 10. Spike result` section to
`docs/superpowers/specs/2026-09-24-native-client-m4b-design.md` with the three
answers, the DRI-minor mapping, and anything surprising. Commit that alone.

```bash
git add docs/superpowers/specs/2026-09-24-native-client-m4b-design.md
git commit -m "spike(m4b): record whether edid_override works on a live X server"
```

---

## Task 2: `ghostframe-edid` — CVT-RB timings and the EDID block

**Depends on Task 1 passing.**

**Files:**
- Create: `ghostframe-edid/Cargo.toml`, `ghostframe-edid/src/lib.rs`, `ghostframe-edid/src/cvt.rs`

This crate has **no dependencies** — it is linked into a root helper, so every
dependency is attack surface. Keep it `#![forbid(unsafe_code)]`.

**Add it to the workspace members in the root `Cargo.toml`, and to
`tests/containers/test-server/Dockerfile`'s per-crate manifest COPY list** — a
new workspace member that the Dockerfile does not know about fails the e2e
image build before any test runs.

- [ ] **Step 1: Write the failing CVT-RB test**

```rust
// ghostframe-edid/src/cvt.rs
#[cfg(test)]
mod tests {
    use super::*;

    // Generated with `cvt 1920 1080 60 -r`:
    //   Modeline "1920x1080R" 138.50 1920 1968 2000 2080 1080 1083 1088 1111
    #[test]
    fn matches_cvt_for_1920x1080() {
        let t = cvt_reduced_blanking(1920, 1080, 60);
        assert_eq!(t.pixel_clock_khz, 138_500);
        assert_eq!(t.h_total, 2080);
        assert_eq!(t.h_sync_start, 1968);
        assert_eq!(t.h_sync_end, 2000);
        assert_eq!(t.v_total, 1111);
        assert_eq!(t.v_sync_start, 1083);
        assert_eq!(t.v_sync_end, 1088);
    }

    // Generated with `cvt 2560 1440 60 -r`:
    //   Modeline "2560x1440R" 241.50 2560 2608 2640 2720 1440 1443 1448 1481
    #[test]
    fn matches_cvt_for_2560x1440() {
        let t = cvt_reduced_blanking(2560, 1440, 60);
        assert_eq!(t.pixel_clock_khz, 241_500);
        assert_eq!(t.h_total, 2720);
        assert_eq!(t.v_total, 1481);
    }
}
```

**Add at least two more cases of your own**, generated by running
`cvt <w> <h> 60 -r` yourself — including one non-16:9 aspect (try `cvt 1280 1024 60 -r`).
Do not trust the two vectors above without re-running `cvt`; they were generated
that way but a transcription error is exactly the defect class this project keeps hitting.

- [ ] **Step 2: Run it to verify it fails**

```bash
cargo test -p ghostframe-edid cvt
```

Expected: FAIL to compile — the function does not exist.

- [ ] **Step 3: Implement CVT reduced blanking**

Implement VESA CVT 1.2 reduced-blanking (RB v1). The fixed parameters are:
horizontal blanking 160 px, h sync width 32 px, h front porch 48 px, v front
porch 3 lines, minimum v back porch 6 lines, and a minimum vertical blanking
time of 460 µs. The pixel clock is rounded **down** to a 0.25 MHz multiple.

Return a struct; name the fields as the test uses them:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub pixel_clock_khz: u32,
    pub h_active: u32,
    pub h_sync_start: u32,
    pub h_sync_end: u32,
    pub h_total: u32,
    pub v_active: u32,
    pub v_sync_start: u32,
    pub v_sync_end: u32,
    pub v_total: u32,
}
```

Work from the VESA algorithm, not from these notes alone — the notes name the
constants but not the rounding order, and the rounding is what the test pins.
When your numbers disagree with `cvt`, `cvt` is right.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-edid cvt
```

Expected: all pass.

- [ ] **Step 5: Write the failing EDID-block tests**

```rust
// ghostframe-edid/src/lib.rs
#[cfg(test)]
mod tests {
    use super::*;

    fn block() -> [u8; 128] {
        build_edid(&EdidRequest { width: 1920, height: 1080, mm_width: 597, mm_height: 336 })
    }

    #[test]
    fn starts_with_the_edid_header() {
        assert_eq!(&block()[0..8], &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
    }

    #[test]
    fn checksum_makes_the_block_sum_to_zero() {
        let sum = block().iter().fold(0u8, |a, b| a.wrapping_add(*b));
        assert_eq!(sum, 0, "EDID byte 127 must make the block sum to 0 mod 256");
    }

    #[test]
    fn declares_edid_1_4() {
        let b = block();
        assert_eq!((b[18], b[19]), (1, 4));
    }

    #[test]
    fn manufacturer_id_is_packed_five_bit_letters() {
        // "GFR" = G(7), F(6), R(18) -> (7<<10)|(6<<5)|18 = 7378 = 0x1CD2,
        // stored big-endian. This is the one field where a plausible-looking
        // byte pair is silently wrong, so it gets its own test.
        assert_eq!(&block()[8..10], &[0x1C, 0xD2]);
    }

    #[test]
    fn detailed_timing_carries_physical_size_in_millimetres() {
        // The DTD, not the basic-parameters block, is what fixes DPI: bytes
        // 21/22 store size in *centimetres*, too coarse to be useful.
        // DTD starts at byte 54; mm fields are at DTD offsets 12, 13, 14.
        let b = block();
        let dtd = &b[54..72];
        let mm_w = (u32::from(dtd[14] >> 4) << 8) | u32::from(dtd[12]);
        let mm_h = (u32::from(dtd[14] & 0x0F) << 8) | u32::from(dtd[13]);
        assert_eq!((mm_w, mm_h), (597, 336));
    }

    #[test]
    fn detailed_timing_matches_cvt() {
        // The DTD must carry the CVT-RB timing, not just the resolution --
        // a wrong pixel clock produces an EDID that parses cleanly and
        // drives nothing.
        let b = block();
        let dtd = &b[54..72];
        let pclk_10khz = u32::from(dtd[0]) | (u32::from(dtd[1]) << 8);
        assert_eq!(pclk_10khz, 13_850, "138.50 MHz in 10 kHz units");
        let h_active = (u32::from(dtd[4] >> 4) << 8) | u32::from(dtd[2]);
        assert_eq!(h_active, 1920);
        let h_blank = (u32::from(dtd[4] & 0x0F) << 8) | u32::from(dtd[3]);
        assert_eq!(h_blank, 160, "2080 total - 1920 active");
    }
}
```

- [ ] **Step 6: Run to verify they fail, then implement**

```bash
cargo test -p ghostframe-edid
```

Expected: FAIL to compile.

The EDID 1.4 base block layout, for reference while implementing:

| offset | size | field |
|---|---|---|
| 0 | 8 | header `00 FF FF FF FF FF FF 00` |
| 8 | 2 | manufacturer ID, packed 5-bit letters, big-endian |
| 10 | 2 | product code, little-endian |
| 12 | 4 | serial, little-endian |
| 16 | 1 | week of manufacture |
| 17 | 1 | year − 1990 |
| 18 | 2 | version (1), revision (4) |
| 20 | 1 | video input params (bit 7 = digital) |
| 21 | 2 | max image size, **centimetres** |
| 23 | 1 | gamma, `(gamma × 100) − 100` |
| 24 | 1 | feature support |
| 25 | 10 | chromaticity |
| 35 | 3 | established timings |
| 38 | 16 | standard timings (`01 01` = unused) |
| 54 | 18 | descriptor 1 — preferred DTD |
| 72 | 18 | descriptor 2 |
| 90 | 18 | descriptor 3 |
| 108 | 18 | descriptor 4 |
| 126 | 1 | extension count |
| 127 | 1 | checksum |

DTD internal layout: pixel clock (10 kHz units, LE) at 0–1; h active low / h
blank low / split-upper at 2–4; v active low / v blank low / split-upper at
5–7; h front porch, h sync width, v porch+sync nibbles, upper-bits byte at
8–11; mm width low, mm height low, mm split-upper at 12–14; borders at 15–16;
flags at 17.

Use descriptors 2–4 for a display name (`0xFC`) and range limits (`0xFD`), or
leave them as unused padding (`0x10`) — either is valid; say which you chose
and why in a comment.

**`EdidRequest` lives in this crate and is the type every later task uses**:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdidRequest {
    pub width: u16,
    pub height: u16,
    pub mm_width: u16,
    pub mm_height: u16,
}
```

Task 4's `DisplayController::install_edid` takes it, and Task 5's helper
receives it over the socket, so `ghostframe-lib` gains a dependency on
`ghostframe-edid`. That is acceptable precisely because this crate has no
dependencies of its own — check that holds before adding anything to it.

- [ ] **Step 7: Verify against a real parser**

```bash
cargo test -p ghostframe-edid   # all green first
```

Then dump a generated block to a file and check it with an independent tool:

```bash
# edid-decode is in the `edid-decode` package on most distros
edid-decode generated.edid
```

Expected: no errors, the resolution and physical size reported correctly. **If
`edid-decode` is unavailable, say so and skip** rather than claiming
verification you did not do — the unit tests are the gate, this is the
cross-check.

- [ ] **Step 8: Commit**

```bash
git add ghostframe-edid Cargo.toml tests/containers/test-server/Dockerfile
git commit -m "feat(edid): CVT-RB timings and EDID 1.4 block synthesis

No dependencies and forbid(unsafe_code): this crate is linked into a root
helper, so every dependency is attack surface. Timings are pinned against
the cvt(1) utility; physical size goes in the detailed timing descriptor
because the basic-parameters block stores only centimetres."
```

---

## Task 3: `DisplayInfo` / `DisplayMode` wire format

Survives a failed Task 1 — negotiation is needed either way.

**Files:**
- Create: `ghostframe-lib/src/transport/display.rs`
- Modify: `ghostframe-lib/src/transport/mod.rs`, `ghostframe-client-core/src/lib.rs`

- [ ] **Step 1: Write the failing tests**

```rust
// ghostframe-lib/src/transport/display.rs
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_info_round_trips() {
        let msg = DisplayInfoMsg { max_width: 2560, max_height: 1440, mm_width: 597, mm_height: 336 };
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
        let msg = DisplayInfoMsg { max_width: 2560, max_height: 1440, mm_width: 597, mm_height: 336 };
        let mut buf = Vec::new();
        msg.encode(&mut buf);
        for n in 0..buf.len() {
            assert_eq!(DisplayInfoMsg::decode(&buf[..n]), None, "prefix of length {n}");
        }
    }

    #[test]
    fn the_two_types_do_not_collide_with_existing_messages() {
        // 0x01 feedback, 0x03 hello, 0x04 decode-error, 0x05 input, 0x06 ack.
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

Expected: FAIL to compile.

- [ ] **Step 3: Implement**

```rust
//! DisplayInfo (0x07) and DisplayMode (0x08): client → server display
//! negotiation on the reliable bidi feedback stream.
//!
//! Wire formats, all big-endian:
//!   DisplayInfo  [0x07][max_w:u16][max_h:u16][mm_w:u16][mm_h:u16]   9 bytes
//!   DisplayMode  [0x08][w:u16][h:u16]                               5 bytes
//!
//! Separate messages rather than an extension of HELLO: HELLO is a one-shot
//! capability advertisement and eviction keys on it (M4a), while display
//! state changes throughout a session. u16 is sufficient -- the server
//! clamps to the framebuffer ceiling regardless, and no display this decade
//! exceeds 65535 px in a dimension.

pub const DISPLAY_INFO_MSG_TYPE: u8 = 0x07;
pub const DISPLAY_MODE_MSG_TYPE: u8 = 0x08;
pub const DISPLAY_INFO_SIZE: usize = 9;
pub const DISPLAY_MODE_SIZE: usize = 5;
```

Write `DisplayInfoMsg` and `DisplayModeMsg` with `encode(&self, &mut Vec<u8>)`
and `decode(&[u8]) -> Option<Self>`, deriving
`Debug, Clone, Copy, PartialEq, Eq`. Mirror `client_caps.rs`'s shape exactly —
it is the established idiom for this stream.

Add `pub mod display;` to `ghostframe-lib/src/transport/mod.rs`.

- [ ] **Step 4: Emit from the client core**

`ghostframe-client-core/src/lib.rs:214-219` pushes HELLO into the outbox at
construction. Add the same for `DisplayInfo` when the config carries display
data, and a method for `DisplayMode`:

```rust
/// Queue a display-mode request. The server clamps it and reports what it
/// actually set through the existing frame-dimensions message -- callers
/// must render what arrives, not what they asked for.
pub fn request_display_mode(&mut self, width: u16, height: u16) {
    self.outbox.push_back(PollOutput::Stream(encode_display_mode(width, height)));
}
```

`ghostframe-client-core` must not depend on `ghostframe-lib` (that would be a
cycle — lib is the server). Put the encoders in `ghostframe-client-core` beside
`encode_hello` (`loss_tracker.rs:93`), and add an oracle test in
`ghostframe-client-core/tests/` asserting the exact bytes match what
`ghostframe-lib`'s decoder expects — `tests/oracle_feedback.rs` is the pattern
(`assert_eq!(encode_hello(true, false), vec![0x03, 0x01])`).

Extend `ClientConfig` (`lib.rs:105`) with `display: Option<ClientDisplay>` so a
client that cannot determine its display simply omits it:

```rust
/// What the client knows about its own monitor. Deliberately NOT named
/// `DisplayInfo` -- that name belongs to the wire message in
/// `ghostframe-lib`, and client-core cannot depend on the server crate, so
/// two same-named types in different crates would read as one type and
/// confuse every later reader.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClientDisplay {
    pub max_width: u16,
    pub max_height: u16,
    pub mm_width: u16,
    pub mm_height: u16,
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

Separate from HELLO: HELLO is one-shot and eviction keys on it, while
display state changes throughout a session. Encoders live in client-core
with byte-exact oracle tests, since client-core cannot depend on the
server crate."
```

---

## Task 4: `DisplayController` trait, dispatch, and debounce

Survives a failed Task 1.

**Files:**
- Modify: `ghostframe-lib/src/transport/display.rs`, `ghostframe-lib/src/transport/io_bridge.rs`

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

    assert_eq!(ctl.modes(), vec![(1280, 800)]);
}

#[tokio::test]
async fn rapid_resizes_collapse_to_one_mode_change() {
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

    assert_eq!(ctl.modes(), vec![(1280, 800)], "only the final size should be applied");
}

#[tokio::test]
async fn a_mode_above_the_ceiling_is_clamped_not_rejected() {
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    ctl.set_ceiling(4096, 2160);
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayModeMsg { width: 8192, height: 4320 }.encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.on_timeout(debounce_elapsed_us());

    assert_eq!(ctl.modes(), vec![(4096, 2160)],
        "an oversized request must be clamped so the client still gets a picture");
}

#[tokio::test]
async fn display_info_installs_the_edid_once_per_session() {
    let (mut bridge, handle) = test_bridge_with_one_session().await;
    let ctl = Arc::new(RecordingController::default());
    bridge.display_controller = Some(ctl.clone());

    let mut buf = Vec::new();
    DisplayInfoMsg { max_width: 2560, max_height: 1440, mm_width: 597, mm_height: 336 }
        .encode(&mut buf);
    bridge.dispatch_feedback_bytes(handle, &buf);
    bridge.dispatch_feedback_bytes(handle, &buf);

    assert_eq!(ctl.edids().len(), 1,
        "a repeated DisplayInfo must not re-trigger a privileged hotplug");
}
```

`test_bridge_with_one_session` already exists from M4a; `RecordingController`
is yours to write. Reuse the M4a helpers rather than inventing new ones.

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p ghostframe-lib --lib display_
```

- [ ] **Step 3: Define the trait**

```rust
/// Server-side display control. Implemented against X/RandR in
/// `ghostframe-xdaemon`; mocked in tests. Mirrors `InputInjector`
/// (`transport/input_inject.rs`), which exists for the same reason: keep
/// the protocol logic testable without a running X server.
pub trait DisplayController: Send + Sync {
    /// The framebuffer ceiling fixed at X startup (the `Virtual` line).
    /// Requests above it are clamped, never rejected.
    fn ceiling(&self) -> (u16, u16);

    /// Apply a mode, adding it to the output first if X does not have it.
    fn set_mode(&self, width: u16, height: u16) -> Result<(), DisplayError>;

    /// Install a synthetic EDID. Privileged (goes through the root helper)
    /// and called at most once per session -- see the design doc §6.1.
    fn install_edid(&self, info: EdidRequest) -> Result<(), DisplayError>;
}
```

- [ ] **Step 4: Wire dispatch and debounce**

Add `display_controller: Option<Arc<dyn DisplayController>>` to `IoBridge`
beside `input_injector` (`io_bridge.rs:803`), and arms for both message types
in `dispatch_feedback_bytes`.

`DisplayInfo` calls `install_edid` immediately, guarded so a repeat within the
session is a no-op — it triggers a kernel hotplug, and clients may resend.

`DisplayMode` stores the pending size and a deadline; `set_mode` fires from the
existing timeout path once the deadline passes.

```rust
/// How long to wait after the last DisplayMode before applying it.
///
/// **A guess, not a measurement.** Roughly the pause a person makes on
/// releasing a window edge, and far longer than the 2.6-4.9 ms a resolution
/// change costs (M3 spec §10.7), so being wrong means latency on the last
/// resize rather than thrash. If it proves wrong, measure a real resize
/// drag rather than guessing again.
const DISPLAY_MODE_DEBOUNCE_US: u64 = 250_000;
```

Find how `poll_timeout`/`on_timeout` deadlines are managed in this file and
follow it — do not add a second timing mechanism.

- [ ] **Step 5: Run the tests**

```bash
cargo test -p ghostframe-lib --lib
```

- [ ] **Step 6: Prove the debounce test can fail**

Apply each request immediately instead of on the deadline, and confirm
`rapid_resizes_collapse_to_one_mode_change` fails. Revert.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/display.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(transport): display negotiation dispatch with server-side debounce

DisplayController mirrors InputInjector so negotiation is testable without
X. Debouncing is server-side: a client that spams resizes must not be able
to drive mode switches at will, and every client gets the behaviour rather
than each reimplementing it."
```

---

## Task 5: The privileged EDID helper

**Depends on Task 1 passing.**

**Files:**
- Create: `ghostframe-edid-helper/Cargo.toml`, `ghostframe-edid-helper/src/main.rs`

Dependencies: `ghostframe-edid` and nothing else beyond `libc` if unavoidable.
Read the systemd-passed socket from `LISTEN_FDS` directly (fd 3) rather than
adding a socket-activation crate — it is about twenty lines and this binary
runs as root.

**Add to the workspace members and the test-server Dockerfile manifest list.**

- [ ] **Step 1: Write the failing validation tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_plausible_request() {
        assert!(validate(&EdidRequest { width: 2560, height: 1440, mm_width: 597, mm_height: 336 }).is_ok());
    }

    #[test]
    fn rejects_zero_dimensions() {
        assert!(validate(&EdidRequest { width: 0, height: 1080, mm_width: 597, mm_height: 336 }).is_err());
    }

    #[test]
    fn rejects_absurd_dimensions() {
        // Root is about to hand these to the kernel's EDID parser. A
        // resolution no display can have is a sign the daemon is
        // compromised or confused; refuse rather than synthesise.
        assert!(validate(&EdidRequest { width: 60000, height: 40000, mm_width: 597, mm_height: 336 }).is_err());
    }

    #[test]
    fn rejects_an_implausible_dpi() {
        // 2560 px across 10 mm is not a display. Accepting it produces an
        // EDID that makes the remote desktop unusable.
        assert!(validate(&EdidRequest { width: 2560, height: 1440, mm_width: 10, mm_height: 6 }).is_err());
    }

    #[test]
    fn a_short_request_is_rejected_without_panicking() {
        assert!(parse_request(&[0u8; 3]).is_none());
    }
}
```

- [ ] **Step 2: Run to verify failure, then implement**

The helper:

1. Takes the listening socket from fd 3 (`LISTEN_FDS`), or binds a path given
   on the command line when run outside systemd — the e2e in Task 9 needs that
   second path.

   ```rust
   /// systemd passes listening sockets starting at fd 3 (SD_LISTEN_FDS_START).
   /// Checking LISTEN_PID matters: the variables are inherited across exec,
   /// so a child could otherwise adopt a socket that was never meant for it.
   fn socket_from_systemd() -> Option<UnixListener> {
       if std::env::var("LISTEN_PID").ok()? != std::process::id().to_string() {
           return None;
       }
       if std::env::var("LISTEN_FDS").ok()? != "1" {
           return None;
       }
       // SAFETY: systemd guarantees fd 3 is an open listening socket when
       // LISTEN_PID matches our pid and LISTEN_FDS is 1.
       Some(unsafe { UnixListener::from_raw_fd(3) })
   }
   ```
2. Accepts one connection, reads a fixed-size request, validates it.
3. Calls `ghostframe_edid::build_edid`.
4. Resolves the DRI minor **by connector name** (spec §5.1 — never assume
   `dri/0`; Task 1 will have confirmed the real mapping).
5. Writes `edid_override`, then `trigger_hotplug`.
6. Replies ok or a numeric error, and exits.

Bound the read and reject anything longer than one request. Log every accepted
request with its values — this is the privileged step and its audit trail is
the only record.

- [ ] **Step 3: Run the tests**

```bash
cargo test -p ghostframe-edid-helper
```

- [ ] **Step 4: Commit**

```bash
git add ghostframe-edid-helper Cargo.toml tests/containers/test-server/Dockerfile
git commit -m "feat(helper): socket-activated root EDID installer

Synthesises the EDID itself rather than accepting bytes: a compromised
daemon must not be able to choose what the kernel's EDID parser sees.
Validates ranges and DPI before synthesising, and resolves the DRI minor
by connector name because card enumeration order is not stable."
```

---

## Task 6: `XrandrDisplay` — the real controller

**Depends on Task 1 passing** (for `install_edid`; the RandR half stands alone).

**Files:**
- Create: `ghostframe-xdaemon/src/display.rs`
- Modify: `ghostframe-xdaemon/src/main.rs`, `ghostframe-xdaemon/Cargo.toml`

- [ ] **Step 1: Add the RandR feature**

`x11rb` needs its `randr` feature (verified present in x11rb 0.13,
`Cargo.toml:108`). Add it wherever x11rb is declared for this crate.

- [ ] **Step 2: Implement `DisplayController` for X**

```rust
/// `DisplayController` backed by X RandR, with EDID installation delegated
/// to the root helper over its Unix socket.
///
/// `set_mode` is unprivileged: RandR is an ordinary X client operation and
/// the daemon already holds the display. Only `install_edid` crosses the
/// privilege boundary, and only once per session.
pub struct XrandrDisplay { /* connection, output, helper socket path */ }
```

`ceiling()` reads the screen's maximum size from RandR's screen resources
rather than hardcoding 4096x2160 — the config could change and a stale
constant would clamp wrongly.

`set_mode` must add the mode to the output if X does not already have it
(`CreateMode` + `AddOutputMode`), then `SetCrtcConfig`. Check whether a mode of
that size already exists before creating a duplicate; X accumulates them.

`install_edid` connects to the helper socket, writes the request, reads the
reply. **On any failure — socket missing, helper error, timeout — log once and
return `Ok`-equivalent degraded state**, per spec §7: wrong DPI is survivable,
a dead session is not.

- [ ] **Step 3: Wire it into the server**

`ghostframe-xdaemon/src/main.rs:186` currently reads:

```rust
let server = match GhostframeServer::new(config, ":443", lib_config, input_injector).await {
```

Add the controller. At five positional parameters this is at the edge of
readable; if you introduce a small struct instead, do it as its own commit so
the mechanical change is separable from the feature.

Construct `XrandrDisplay` after the X-server-ready wait, beside the
`XTestInjector` construction (`main.rs:157-178`), and follow its failure
handling: log and continue with `None` rather than aborting startup.

- [ ] **Step 4: Verify it builds and the daemon still starts**

```bash
cargo build -p ghostframe-xdaemon
cargo test -p ghostframe-xdaemon
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-xdaemon
git commit -m "feat(xdaemon): RandR-backed DisplayController

set_mode is unprivileged -- RandR is an ordinary X client operation. Only
install_edid crosses to the root helper, once per session. Both degrade to
a logged warning rather than failing startup, matching how the XTest
injector already behaves."
```

---

## Task 7: Client-side display enumeration

Survives a failed Task 1.

**Files:**
- Create: `ghostframe-cli/src/display_probe.rs`
- Modify: `ghostframe-cli/src/commands.rs`, `ghostframe-cli/Cargo.toml`

- [ ] **Step 1: Write the failing tests for the sanity filter**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_normal_monitor() {
        // 2560x1440 at 597x336 mm is about 109 DPI.
        assert_eq!(sanitise(2560, 1440, 597, 336), Some((597, 336)));
    }

    #[test]
    fn rejects_zero_millimetres() {
        // Projectors and many TVs report 0. Propagating it produces an EDID
        // with a nonsensical physical size and unusable font scaling.
        assert_eq!(sanitise(1920, 1080, 0, 0), None);
    }

    #[test]
    fn rejects_an_implausible_dpi() {
        assert_eq!(sanitise(1920, 1080, 10, 6), None);
    }

    #[test]
    fn fallback_is_96_dpi() {
        // 1920 px at 96 DPI = 20 in = 508 mm.
        let (w, h) = fallback_mm(1920, 1080);
        assert_eq!(w, 508);
        assert_eq!(h, 286);
    }
}
```

Check the arithmetic in `fallback_is_96_dpi` yourself — 1080 px at 96 DPI is
11.25 in = 285.75 mm, and whether that rounds to 285 or 286 depends on the
rounding you choose. Pick one, make the test match, and say which in a comment.

- [ ] **Step 2: Implement enumeration for both backends**

- **Wayland**: `wl_output::geometry` carries `physical_width` / `physical_height`
  in millimetres. `smithay-client-toolkit` is already a dependency.
- **X11**: RandR `GetOutputInfo` carries `mm_width` / `mm_height`. Add the
  `randr` feature to `x11rb` in `ghostframe-cli/Cargo.toml` (currently
  `dri3, present, xfixes, allow-unsafe-code`).

Return the monitor's full resolution and sanitised millimetres. Log which path
was taken and whether the fallback fired — "the DPI is wrong" is otherwise very
hard to diagnose from the server side.

- [ ] **Step 3: Send the messages**

Populate `ClientConfig.display` at connect. On window resize, call
`request_display_mode` — but **do not debounce client-side**; the server does
it (Task 4), and duplicating it would make the effective delay the sum of both.

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-cli
cargo clippy -p ghostframe-cli --all-targets -- -D warnings
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-cli
git commit -m "feat(cli): probe the local display and negotiate it

Wayland via wl_output geometry, X11 via RandR GetOutputInfo. Implausible
physical sizes (projectors and TVs often report zero) fall back to 96 DPI
rather than propagating a bogus size into the EDID, and which path ran is
logged because wrong DPI is otherwise undiagnosable from the server."
```

---

## Task 8: Packaging — the ceiling and the helper unit

**Depends on Task 1 passing.**

**Files:**
- Modify: `packaging/xorg-headless-amdgpu.conf`, `packaging/install.sh`
- Create: `packaging/systemd/ghostframe-edid-helper.service`, `packaging/systemd/ghostframe-edid-helper.socket`

- [ ] **Step 1: Raise the framebuffer ceiling**

In `packaging/xorg-headless-amdgpu.conf`, change `Virtual 1920 1080` to
`Virtual 4096 2160` and add modelines the negotiation can select, with a
comment explaining that `Virtual` is fixed at X startup and is therefore the
hard ceiling for every client.

- [ ] **Step 2: Add the socket and service units**

The socket restricted to the ghostframe user (`SocketUser=`/`SocketMode=`), the
service `Type=oneshot` running as root with the tightest sandboxing that still
permits debugfs writes. Start from `ProtectSystem=strict`, `PrivateTmp=yes`,
`NoNewPrivileges=yes` and relax only what breaks — and note in a comment which
directives had to be relaxed and why, since the next reader will wonder.

- [ ] **Step 3: Install them**

Extend `install.sh` to install the binary, the unit, and the socket, and to
enable the socket. Follow the existing install patterns in that script.

**The Xorg config is an upgrade hazard**: an existing install keeps the old
1920x1080 ceiling, making the feature look broken. The script already replaces
that file — confirm it does, and if it skips when present, fix that and say so
in the commit message.

- [ ] **Step 4: Measure what the bigger ceiling costs**

Spec risk 3 says to measure this rather than assume it is free. A 4096x2160
32-bit framebuffer is about 35 MB versus about 8 MB at 1920x1080. Confirm the
real figure rather than trusting that arithmetic — start X with each ceiling and
compare:

```bash
# with the server running under each config
grep -i "VmRSS" /proc/$(pgrep -f "Xorg :1")/status
```

Record both numbers in the commit message. If the delta is far larger than
~27 MB, something else is scaling with the ceiling and that is worth knowing
before this ships to a small server.

- [ ] **Step 5: Validate**

```bash
bash -n packaging/install.sh && echo "install.sh parses"
systemd-analyze verify packaging/systemd/ghostframe-edid-helper.service 2>&1 | head
```

`systemd-analyze verify` may warn about absent binaries on a dev box; unit
syntax errors are what matter.

- [ ] **Step 6: Commit**

```bash
git add packaging
git commit -m "packaging: raise the framebuffer ceiling and install the EDID helper

Virtual is fixed at X startup, so 1920x1080 capped every client regardless
of EDID. 4096x2160 costs framebuffer memory but lets the session survive
every resolution change, which restarting X would not."
```

---

## Task 9: End-to-end display negotiation

**Depends on Task 1 passing.**

**Files:**
- Create: `ghostframe-e2e/tests/display_negotiation.rs`
- Modify: `.github/workflows/e2e.yml` (exemption comment), the e2e container setup

- [ ] **Step 1: Write the unprivileged half first**

A client connects advertising 2560x1440, requests a 1280x800 mode, and the test
asserts the server changes resolution — observable through the existing
frame-dimensions message the client already receives. No root needed.

Model it on `ghostframe-e2e/tests/eviction.rs` (M4a): same `setup_e2e_server`,
same `attach_bridge` sharing of the harness tsnet node, and note that
`wait_for_frame` discards non-frame events, so a waiter for `Resized` belongs
in this test file.

- [ ] **Step 2: Add the privileged half**

Run the e2e container with the access the helper needs (`--privileged` plus a
`/sys/kernel/debug` mount), inject on `card0-Virtual-1`, and assert X inside the
container sees the new mode list and physical size.

**Record in the test's module doc what this touches.** debugfs is global, not
namespaced: this changes host kernel state for the VKMS connector. It is safe
on the development box because the desktop runs on `card1-HDMI-A-2` and the
host Xorg ignores the vkms driver
(`/etc/X11/xorg.conf.d/99-ignore-vkms.conf`) — but a concurrent VKMS e2e run
would collide, the same class as the VKMS master conflict already recorded.

**Restore the connector afterwards** whether the test passes or fails; Task 1
step 7 establishes how. A test that leaves an EDID installed poisons the next
run.

**Say in the module doc what this test does not prove.** It exercises VKMS;
production uses amdgpu `virtual_display`. The mechanism is DRM core so it should
carry over, but "should" is doing work — spec risk 2 calls for one manual check
on the production server before the milestone is called done, and this comment
is where the next reader will learn that is still outstanding.

- [ ] **Step 3: Rebuild the container and run**

```bash
just containers-build
TS_CONTROL_URL=http://127.0.0.1:18080 \
  cargo test -p ghostframe-e2e --test display_negotiation -- --nocapture --test-threads=1
```

Do not pipe either command — a pipe reports the last stage's status, and this
repo has had a failed build look green exactly that way.

- [ ] **Step 4: Keep it out of CI, visibly**

Extend the exemption comment in `e2e.yml` to name `display_negotiation.rs`
alongside `eviction.rs`, `native_client.rs`, `showcase.rs` and `h264.rs`, then:

```bash
grep -rn "test display_negotiation" .github/workflows/ ; echo "exit=$? (1 = correctly absent)"
python3 -c "import yaml; yaml.safe_load(open('.github/workflows/e2e.yml'))" && echo "yaml ok"
```

- [ ] **Step 5: Commit**

```bash
git add ghostframe-e2e/tests/display_negotiation.rs .github/workflows/e2e.yml
git commit -m "test(e2e): display negotiation end to end, including EDID injection"
```

---

## Done means

- [ ] Task 1's three answers are written into the spec, whatever they were.
- [ ] A client advertising 2560x1440 and a 1280x800 window is served 1280x800.
- [ ] The injected EDID gives X the client's physical size, so DPI is right.
- [ ] A resize drag produces one mode change, not many — and that test fails if the debounce is removed.
- [ ] The helper refuses zero, absurd, and implausible-DPI requests, with tests.
- [ ] `just ci-local` green, `npm test` green, `just containers-build` succeeds.
- [ ] The e2e restores the connector even when it fails.
- [ ] Every number that is a guess says so where it is written.
