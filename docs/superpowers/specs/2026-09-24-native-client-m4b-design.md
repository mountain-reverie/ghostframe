# Native client M4b: display capability negotiation — design

**Status:** approved for planning (revised after the Task 1 spike)
**Date:** 2026-09-24
**Predecessors:** [M1](2026-09-22-native-client-design.md), [M2](2026-09-23-native-client-m2-design.md), [M3](2026-09-23-native-client-m3-design.md), [M4a](2026-09-24-native-client-m4a-design.md)

M4a made "exactly one attached client" an invariant, so the server no longer
has to answer "whose screen?". M4b answers "how big, and at what scale?" — the
gate for calling the native client done.

> **This design was revised after implementation began.** It originally
> specified synthetic EDID injection through a privileged helper. The Task 1
> spike proved that cannot work on these connectors; §10 records the evidence.
> The result is a simpler design with **no privileged code at all**. The EDID
> approach is documented rather than deleted, because it is the obvious idea
> and the next person to have it deserves the measurement.

---

## 1. The problem

**The client cannot tell the server what it can display.** The server picks;
the client takes what arrives.

- The production Xorg config (`packaging/xorg-headless-amdgpu.conf`) hardcodes a
  single 1920x1080 modeline with `Virtual 1920 1080`. Every client gets 1080p
  regardless of its own display.
- Nothing conveys the client's scale, so the remote desktop's font sizing is
  whatever the server happens to default to.

M3 measured a resolution change at 2.6-4.9 ms and pre-warms the decoder at the
client's *own screen size* as a stand-in for a negotiated ceiling (M3 spec
§10.8). That stand-in becomes real here.

### 1.1 What is NOT in scope

**Multi-monitor.** The initial spec sketches independent outputs
(`docs/specs/ghostframe-initial-spec.md:1300`). Much larger, nothing depends on
it, and single-display negotiation is a prerequisite regardless.

**The web client.** It will need this eventually; the native client is where the
display properties are actually available.

**A Wayland backend.** Out of scope to *build*, but explicitly in scope to *not
preclude* — see §5 and §7. That constraint is what selected the protocol's
units.

---

## 2. The framebuffer ceiling forces the shape

In X, the `Virtual` line fixes the framebuffer size at server start. RandR can
switch modes *within* it but cannot grow past it, so a 2560x1440 client cannot
be served correctly however the modes are described.

**Decision: start Xorg once with a generous ceiling (`Virtual 4096 2160`), then
negotiate within it.** The session survives every change, which is what makes
reconnecting from a different machine useful — the alternative, rewriting
`xorg.conf` and restarting X, destroys every running application.

Two costs, accepted: a framebuffer sized for the worst case, and a hard clamp
above 4K. Both recorded in §11.

### 2.1 Correction: `Virtual` may not be a hard ceiling after all

Measured during Task 4 on a live X server:

```
Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384
```

`GetScreenSizeRange` reports the **driver's** capability, not the `Virtual`
line. Two consequences:

1. **`ceiling()` clamps nothing in production.** Task 3's clamp logic is
   correct and its tests pass against a mock ceiling, but with a real ceiling
   of 16384x16384 no realistic client request is ever clamped. The clamp is a
   guard against absurdity, not the mechanism that keeps requests in range.
2. **RandR 1.2 drivers can grow the screen past the startup size** via the
   driver's resize hook, so `Virtual` may be the *initial* size rather than a
   ceiling. If so, §2's premise is wrong — though its conclusion (start
   generous) is still harmless and still removes a dependency on the driver
   reallocating.

**What protects us either way**: a request the driver cannot satisfy fails at
`SetScreenSize`/`SetCrtcConfig`, `set_output` returns `Err`, the failure is
logged, and the session keeps its current mode (§6). Safe by failure, not by
prediction.

**ANSWERED (2026-09-25, `ghostframe-e2e/tests/display_negotiation.rs`)**: RandR
**can** grow past the startup size. A container configured for 640x480 was
driven to 2560x1440 and the frames arrived at that size:

```
[result] matched=true sizes observed: [(640, 480), (2560, 1440)]
FINDING: RandR grew the framebuffer past the container's startup size
```

So §2's premise — that `Virtual` is a hard ceiling fixed at server start — is
**wrong**. RandR 1.2 drivers reallocate through their resize hook. §2's
*conclusion* (start generous) still stands as prudence: it avoids depending on
that reallocation succeeding under memory pressure, and costs ~27 MB. But it is
not required for correctness, and a deployment that cannot spare the
framebuffer may leave `Virtual` small.

---

## 3. What is negotiated

**The client's monitor maximum and scale are sent once. The active mode follows
the client's window.**

The monitor maximum is the true ceiling; it changes only when the client moves
to a different display. The window size is what is actually on screen, and
rendering anything larger wastes encode bandwidth producing pixels that get
scaled away. So the remote desktop is rendered at exactly the pixels being
displayed, never upscaled.

---

## 4. Protocol

Two new messages on the existing reliable feedback stream, not an extension of
HELLO. HELLO is a one-shot 2-byte capability advertisement and eviction keys on
it (M4a); display state changes throughout a session, so overloading HELLO would
tangle two lifetimes.

| message | when | payload |
|---|---|---|
| `DisplayInfo` | once, after HELLO | max w/h, **scale**, physical mm w/h (advisory) |
| `DisplayMode` | on debounced resize | requested w/h |

The server clamps both to the `Virtual` ceiling, ignores a mode it cannot
satisfy, and reports what it actually set. **The client renders what arrives,
not what it asked for** — the existing frame-dimensions message already carries
the authoritative size.

Both messages must tolerate a client that never sends them: the server keeps its
current behaviour, exactly as for a client that never sends HELLO.

### 4.1 Scale is the DPI unit, not millimetres

This is the one decision the future Wayland backend dictates, and it is worth
stating plainly because millimetres look like the more fundamental choice.

`wlr-output-management-unstable-v1` — the protocol a Wayland backend would use
— lets a configuring client set `mode`, `custom_mode`, `position`, `transform`,
`scale`, and `adaptive_sync`. It does **not** let it set physical size: the
`physical_size` event is sent once per head, and the protocol states *"These
cannot be changed by clients"* and *"does not change over the lifetime"*.

So millimetres are unsettable on Wayland while scale is settable on both. X can
derive the millimetres `RRSetScreenSize` wants from scale plus resolution; the
reverse derivation — guessing a scale from millimetres — is a policy decision
compositors normally make, and clients already know their real scale.

Physical millimetres are still carried, **advisory only**: useful in logs when
someone asks why the scale is what it is, and available if a future backend can
use them. Nothing reads them to make a decision.

Scale is carried as a fixed-point integer in thousandths (1000 = 1.0,
1500 = 1.5). Wayland's own `set_scale` takes a `wl_fixed` (24.8); thousandths
convert cleanly and read better in a log line.

### 4.2 Where the client gets these numbers

The CLI drives `smithay-client-toolkit` (Wayland) and `x11rb` (X11) directly,
not winit, and both expose what is needed:

- **Wayland**: `wl_output.scale`, plus `wl_output.geometry` for advisory
  millimetres. Fractional scale via `wp_fractional_scale_v1` where present.
- **X11**: RandR `GetOutputInfo` gives `mm_width`/`mm_height`; scale is derived
  from those plus the mode. `x11rb` needs its **`randr` feature added**
  (currently `dri3, present, xfixes, allow-unsafe-code`), which this design
  requires anyway.

**Displays lie about physical size.** Projectors, many TVs, and some monitors
report zero or nonsense millimetres. On X11, where scale is *derived* from them,
an implausible value (zero, or a computed DPI outside roughly 30-400) must be
treated as absent and fall back to scale 1.0. Log which path was taken — "the
scale is wrong" is otherwise very hard to diagnose from the server.

---

## 5. The `DisplayController` seam

One trait, mirroring `InputInjector` (`transport/input_inject.rs`), which exists
for the same reason: keep protocol logic testable without a running display
server.

```rust
pub trait DisplayController: Send + Sync {
    /// The framebuffer ceiling. Requests above it are clamped, never rejected.
    fn ceiling(&self) -> (u16, u16);

    /// Apply a resolution and scale together -- they are one user-visible
    /// change and applying them separately would show an intermediate state.
    fn set_output(&self, width: u16, height: u16, scale_milli: u16)
        -> Result<(), DisplayError>;
}
```

**This trait is the whole Wayland story.** Nothing above it is X-specific:

| | X11 backend (built now) | Wayland backend (later) |
|---|---|---|
| resolution | `CreateMode` + `AddOutputMode` + `SetCrtcConfig` | `set_custom_mode(w, h, 0)` |
| scale | `SetScreenSize(w, h, mm_from_scale)` | `set_scale(scale)` |
| privilege | none — ordinary X client | none — ordinary Wayland client |

A headless Wayland compositor creates outputs in software, so there is no DRM
connector in that world at all. An EDID-based design would have had nothing to
inject into; a mode-and-scale design ports directly.

---

## 6. Failure handling

Every step degrades rather than fails:

- **Mode cannot be applied** — stay at the current mode and report what is
  actually set. The client already renders the authoritative size.
- **Scale cannot be applied** — apply the resolution anyway. Wrong font size is
  survivable; a dead session is not.
- **Client never sends `DisplayInfo`** — current behaviour, unchanged.

Debouncing is **server-side**. A client that spams resizes must not be able to
drive mode switches at will, and putting it in the server means every client
gets the behaviour rather than each reimplementing it.

**Start at 250 ms and label it a guess where it is written.** Roughly the pause
a person makes on releasing a window edge, and far longer than the 2.6-4.9 ms a
resolution change costs (M3 spec §10.7), so the cost of being wrong is latency
on the last resize rather than thrash. If it proves wrong, measure a real resize
drag rather than guessing again.

---

## 7. Not precluding Wayland

Two rules that cost nothing now and would be expensive to retrofit:

1. **Nothing above `DisplayController` may mention X, RandR, or millimetres as
   a control input.** The wire protocol, the dispatch, and the debounce are
   display-server-agnostic.
2. **Scale is authoritative; millimetres are advisory.** §4.1. A backend that
   cannot set physical size is then fully supported rather than partially.

---

## 8. Testing

**No privileged tests, no root, no debugfs.** The pivot removed all of it.

- **CVT-RB timing calculation** — exact unit tests. `CreateMode` needs a full
  modeline (dot clock, sync starts/ends, totals), so this arithmetic is still
  required. Pinned against the `cvt(1)` utility, which is authoritative and
  installed.
- **Negotiation, clamping, debounce** — unit tests against a mock
  `DisplayController`, no display server needed.
- **Scale derivation and the implausible-value fallback** — unit tests.
- **End-to-end** — a client advertises 2560x1440, requests 1280x800, and the
  test asserts the server changes resolution, observable through the existing
  frame-dimensions message. Ordinary e2e container; no `--privileged`.

---

## 9. What this does not cover

**Whether Enlightenment and its toolkits honour a mid-session screen-size
change.** `RRSetScreenSize` updates what X reports, but an application that read
DPI at startup keeps its old value until restarted. This is a property of the
applications, not of this design, and it is the most likely source of a "the
scale didn't change" report. Worth one manual check once the path works, and
recorded here so the report is recognised rather than debugged from scratch.

---

## 10. Spike result: why there is no EDID here

Run on the development box, 2026-09-24, against `card0-Virtual-1` (VKMS).

| question | answer |
|---|---|
| `edid_override` exists in debugfs? | **yes** |
| `trigger_hotplug` exists in debugfs? | **no** — absent on this kernel |
| Is there another reprobe trigger? | yes — sysfs `status`, writable, accepts `detect` |
| Write to `edid_override` accepted? | **yes** (after fixing the blob, see below) |
| Kernel then exposes the injected EDID? | **NO** — still 0 bytes |
| Connector mode list changed? | **NO** — identical 34 modes |
| Reversible? | yes, cleanly |

**Diagnosis.** The kernel consults `edid_override` only as a *fallback*. In
`drm_helper_probe_get_modes` (`drm_probe_helper.c`),
`drm_edid_override_connector_update` runs **only when the driver's `get_modes`
returns zero modes**. VKMS and amdgpu's `virtual_display` both implement
`get_modes` with `drm_add_modes_noedid`, which returns a canned list — the 34
modes up to 4096x2160 observed above. The override is accepted, stored, and
never read. That is also why these connectors report a 0-byte EDID at rest:
they never read EDID at all.

**This is a driver property, not a configuration mistake, and production is the
same.** amdgpu's virtual display uses `drm_add_modes_noedid` too, so the
original design would have failed identically on the real server — after the
helper, the systemd units, and the privileged container had been built.

**A second finding, kept because it will resurface.** The first injection
attempt failed with `EINVAL`. Cause: a 128-byte slice of a 256-byte monitor EDID
still declares one extension block in byte 126, and `drm_edid_override_set`
rejects a write unless `128 * (1 + extensions) <= len`. Any future EDID work
must emit a blob whose declared extension count matches its actual length.

**What would be required to make EDID work**: EVDI, an out-of-tree kernel module
whose connectors genuinely read EDID. Rejected — it adds a DKMS prerequisite on
top of the amdgpu `virtual_display` option already required, reintroduces DRM
master conflicts, and buys nothing a Wayland backend could use, since
`physical_size` is not client-settable there either (§4.1).

---

## 11. Risks

1. **Applications cache DPI at startup** (§9), so a mid-session scale change may
   not visibly take effect until they restart. Most likely source of a false
   "it doesn't work" report.
2. **`Virtual 4096 2160` costs framebuffer memory** on a server that may have
   little. Measure it rather than assuming it is free.
3. **Changing the production Xorg config affects existing installs.**
   `packaging/install.sh` writes that file; an upgrade that silently kept the old
   1920x1080 ceiling would make the feature appear broken.
4. **CVT-RB is easy to get subtly wrong.** A miscalculated pixel clock yields a
   mode X accepts and cannot drive. The exact tests against `cvt(1)` are the
   guard.
5. **Scale derivation on X11 depends on values displays get wrong** (§4.2). The
   fallback makes it survivable, not correct.
