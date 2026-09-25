# Native client M4b: display capability negotiation and virtual EDID — design

**Status:** approved for planning
**Date:** 2026-09-24
**Predecessors:** [M1](2026-09-22-native-client-design.md), [M2](2026-09-23-native-client-m2-design.md), [M3](2026-09-23-native-client-m3-design.md), [M4a](2026-09-24-native-client-m4a-design.md)

M4a made "exactly one attached client" an invariant, so the server no longer
has to answer "whose screen?". M4b answers "how big, and at what DPI?" — the
gate for calling the native client done.

---

## 1. The problem

**The client cannot tell the server what it can display.** The server picks;
the client takes what arrives. Two consequences already measured:

- The production Xorg config (`packaging/xorg-headless-amdgpu.conf`) hardcodes
  a single 1920x1080 modeline with `Virtual 1920 1080`. Every client gets
  1080p regardless of its own display.
- `card0-Virtual-1` reports **no EDID at all** (the sysfs `edid` file is
  empty). X and Enlightenment therefore get default modes and a meaningless
  physical size, so DPI — and every font on the remote desktop — is wrong.

M3 measured a resolution change at 2.6-4.9 ms and pre-warms the decoder at the
client's *own screen size* as a stand-in for a negotiated ceiling (M3 spec
§10.8). That stand-in becomes real here.

### 1.1 What is NOT in scope

**Multi-monitor.** The initial spec sketches independent outputs with per-
monitor EDID (`docs/specs/ghostframe-initial-spec.md:1300`). That is a much
larger problem, nothing depends on it, and single-display negotiation is a
prerequisite for it regardless.

**The web client.** It will need this eventually, but the native client is
where physical monitor size is actually available; browsers do not expose it.

---

## 2. The framebuffer ceiling forces the shape

In X, the `Virtual` line in the `Display` subsection fixes the framebuffer
size at server start. RandR can switch modes *within* it but cannot grow past
it, so a 2560x1440 client cannot be served correctly however good the EDID is.

**Decision: start Xorg once with a generous ceiling (`Virtual 4096 2160`), then
negotiate within it.** The session survives every change, which is what makes
reconnecting from a different machine useful — the alternative, rewriting
`xorg.conf` and restarting X, destroys every running application and is
therefore unusable for the case the feature exists to serve.

Two costs, accepted: a framebuffer sized for the worst case regardless of the
attached client, and a hard clamp for clients above 4K. Both are recorded in
§8 rather than designed around.

---

## 3. What is negotiated

**EDID advertises the client's monitor maximum. The active mode follows the
client's window.**

These are different quantities and conflating them is the mistake to avoid.
The monitor maximum is the true ceiling and the source of physical size (and
therefore DPI); it changes only when the client moves to a different display.
The window size is what is actually on screen, and rendering anything larger
wastes encode bandwidth to produce pixels that get scaled away.

So the remote desktop is rendered at exactly the pixels being displayed, never
upscaled, with DPI derived from the real monitor.

---

## 4. Protocol

Two new messages on the existing reliable feedback stream, not an extension of
HELLO.

HELLO is documented as a one-shot 2-byte capability advertisement, and since
M4a, eviction keys on it. Overloading it with a mutable, variably-sized payload
would tangle two lifetimes: capabilities are fixed for a session, display state
changes throughout it.

| message | when | payload |
|---|---|---|
| `DisplayInfo` | once, after HELLO | monitor max w/h, physical mm w/h |
| `DisplayMode` | on debounced resize | requested w/h |

The server clamps both to the `Virtual` ceiling. It ignores a mode it cannot
satisfy and reports what it actually set. **The client renders what arrives,
not what it asked for** — the existing frame-dimensions message already carries
the authoritative size, so no new client-side machinery is needed.

Both messages must tolerate a client that never sends them: the server keeps
its current behaviour, exactly as it does for a client that never sends HELLO.

### 4.1 Where the client gets these numbers

The CLI does not use winit — it drives `smithay-client-toolkit` (Wayland) and
`x11rb` (X11) directly, and both expose physical size natively:

- **Wayland**: the `wl_output::geometry` event carries `physical_width` and
  `physical_height` in millimetres.
- **X11**: RandR's `GetOutputInfo` reply carries `mm_width` / `mm_height`. Note
  `x11rb` is currently built with `dri3, present, xfixes, allow-unsafe-code` —
  the **`randr` feature must be added**, and RandR is needed on this path
  anyway.

**Displays lie about this.** Projectors, many TVs, and some monitors report
zero or nonsense millimetres. Treat an implausible value (zero, or a computed
DPI outside roughly 30-400) as absent and fall back to 96 DPI derived from the
reported resolution, rather than propagating a bogus physical size into the
EDID where it would produce unusable font scaling. Log which path was taken —
"the DPI is wrong" is otherwise very hard to diagnose from the server side.

---

## 5. Privilege boundary

Mode-setting needs no privilege — RandR is an ordinary X client and the daemon
already holds the display. **EDID injection does**:
`/sys/kernel/debug/dri/<minor>/<connector>/edid_override` is root-only debugfs,
and `ghostframe-xdaemon` runs as an unprivileged user.

**A systemd socket-activated root service owns that one job.** It accepts a
small fixed-size struct (resolution + physical mm), validates the ranges,
**synthesises the EDID itself**, writes `edid_override`, and triggers the
hotplug. Socket permissions are the access control.

```
xdaemon (guest uid)              helper (root)
  |                                |
  |-- {w, h, mm_w, mm_h} --------->|  validate ranges
  |                                |  synthesise EDID  <-- here, not upstream
  |                                |  write edid_override
  |                                |  trigger_hotplug
  |<---------- ok / error ---------|
```

The daemon never sends bytes that reach the kernel verbatim, so a compromised
daemon cannot choose what the kernel's EDID parser sees. That is the whole
reason synthesis lives on the privileged side despite being more awkward to
test there.

### 5.1 Resolve the DRI minor by connector name, never by index

The helper must find `/sys/kernel/debug/dri/<minor>/` by matching the connector
name, not by assuming `dri/0`. Card enumeration order is not stable across
boots. On the development box `card0` is VKMS and `card1` is the real GPU; on
the production server with amdgpu `virtual_display` it is likely the reverse. A
hardcoded index works in one place and silently targets the wrong GPU in the
other.

---

## 6. EDID synthesis

A 128-byte EDID 1.4 base block: header, manufacturer and product identifiers,
basic display parameters, chromaticity, timing bitmaps, one preferred detailed
timing descriptor, extension count, and a checksum byte chosen so the block
sums to zero mod 256.

**Physical size must go in the detailed timing descriptor.** The basic-
parameters block stores image size in *centimetres*, too coarse for correct
DPI. Millimetre precision lives in the DTD, so that is the field that actually
fixes font scaling.

**The DTD needs real timings**, not just a resolution — pixel clock, blanking
intervals, sync offsets. Generate them with **CVT reduced blanking** (VESA CVT
1.2), which is what flat panels use. It is deterministic arithmetic, so it
tests exactly: a known resolution in, a known byte sequence out, checked
against published CVT-RB timings for standard modes.

### 6.1 The helper runs once per connect, not once per resize

EDID establishes the ceiling and the physical size. Individual modes are added
at runtime with `xrandr --newmode` / `--addmode`, which needs no privilege.

So a resize drag never touches root, never triggers a hotplug, and cannot storm
the kernel with reprobes. The privileged path is exercised once per session.

---

## 7. Failure handling

Every step degrades rather than fails:

- **Helper unreachable or injection fails** — keep the existing EDID, log once,
  continue with RandR only. Correct pixels, wrong DPI; the session still works.
- **Mode cannot be applied** — stay at the current mode and report what is
  actually set. The client is already built to render the authoritative size.
- **Client never sends `DisplayInfo`** — current behaviour, unchanged.

Debouncing is **server-side**. A client that spams resizes must not be able to
drive mode switches at will, and putting it in the server means every client
gets the behaviour rather than each reimplementing it.

**Start at 250 ms, and label it a guess where it is written.** It is roughly
the pause a person makes on releasing a window edge, and it is far longer than
the 2.6-4.9 ms a resolution change costs (M3 spec §10.7), so the cost of being
wrong is latency on the last resize rather than thrash. It is not a
measurement; if it proves wrong the fix is to measure a real resize drag, not
to guess again.

---

## 8. Testing

**A spike gates the design.** Before any implementation: on real hardware,
write `edid_override` on `card0-Virtual-1`, trigger the hotplug, and confirm a
*running* X server picks up the new modes and physical size **without a
restart**. If it does not, §2's "the session survives" premise collapses and the
design changes before code is written.

Then:

- **EDID synthesiser and CVT-RB calculator** — exact byte-level unit tests, no
  root and no hardware. This is the bulk of the logic and it is fully testable.
- **Helper protocol** — including rejection of out-of-range and malformed
  requests.
- **Privileged e2e container** — injects on `card0-Virtual-1` and asserts X
  sees the new mode list and physical size. This covers the kernel-facing step
  that would otherwise be a recorded gap.
- **Unprivileged e2e** — negotiation through to a RandR mode change.

### 8.1 Why the privileged container is safe here, and what it does touch

Verified on the development box: the live desktop runs on `card1-HDMI-A-2` (the
real AMD GPU), while the VKMS connector is `card0-Virtual-1`, and the host Xorg
is configured to ignore that driver entirely
(`/etc/X11/xorg.conf.d/99-ignore-vkms.conf`: `MatchDriver "vkms"` +
`Option "Ignore" "true"`). An EDID write and hotplug on `card0-Virtual-1` fires
a uevent that the desktop's X server has been told not to act on, for a GPU it
is not driving.

**But debugfs is global, not namespaced.** A privileged container writing
`/sys/kernel/debug/dri/*/` changes host kernel state. The blast radius is not
the desktop — it is anything else using VKMS, which in practice means a
concurrent e2e run. That is the same collision class as the VKMS master
conflict already recorded for the GPU e2e path, and the existing rule (do not
run two VKMS e2e suites at once) covers it.

---

## 9. Risks

1. **`edid_override` may require an X restart to take effect.** This is the
   design's load-bearing assumption and the reason §8's spike comes first.
2. **The e2e proves VKMS, not amdgpu.** Production uses amdgpu
   `virtual_display`; the test connector is VKMS. The mechanism is DRM core, so
   it should carry over, but "should" is doing work here — worth one manual
   check on the production server before the milestone is called done.
3. **`Virtual 4096 2160` costs framebuffer memory** on a server that may have
   little. Measure it rather than assuming it is free.
4. **Changing the production Xorg config affects existing installs.**
   `packaging/install.sh` writes that file; an upgrade path that silently keeps
   the old 1920x1080 ceiling would make the feature appear broken. The install
   script must replace it, and the change must be visible in the release notes.
5. **Physical size may be unavailable or wrong** on the client's display, and
   the DPI goal quietly degrades to a 96-DPI assumption. §4.1 makes that
   explicit and logged rather than silent, but it means "correct DPI" is
   best-effort, not guaranteed.
6. **CVT-RB is easy to get subtly wrong.** A miscalculated pixel clock produces
   an EDID that parses cleanly and drives nothing. The exact-byte tests against
   published timings are the guard.
