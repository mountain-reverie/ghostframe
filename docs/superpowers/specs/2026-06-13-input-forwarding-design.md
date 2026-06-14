# Input Forwarding (Browser → Server): Design

**Date:** 2026-06-13
**Status:** Design approved
**Predecessors:** `2026-06-12-firefox-e2e-design.md`, `2026-06-09-tailnet-served-web-client-design.md`. PR #19 closed the H.264 capability loop, leaving the daemon as a one-way capture-and-stream pipeline with the existing client→server FEEDBACK stream carrying only HELLO + DECODE_ERROR + ReceiverFeedback.

---

## Context

The daemon currently captures the guest's X session and streams frames to a browser, but the browser cannot drive the desktop — pointing the URL at the daemon shows enlightenment with no panel and no apps, because nothing in the guest session has anything to draw, and nothing the user does in the browser has any effect on the guest. Every input event (`pointermove`, `pointerdown`, `keydown`, ...) is consumed by the browser and dropped.

A grep for `keydown` / `keyup` / `pointerdown` / `pointermove` / `MouseEvent` / `KeyboardEvent` / `XTestFakeKey` / `XInput` across the workspace returns zero matches: the work hasn't been done. This spec covers the minimum scope that turns the daemon into an interactive remote desktop.

After this change, opening the daemon's URL in Chrome (or Firefox) and clicking on the canvas focuses it; subsequent mouse motion, button clicks, scroll, and keystrokes flow over WebTransport to the daemon, which injects them into the guest's X session via the XTEST extension. The user can run `xterm`, type into it, and see characters echo through the captured framebuffer.

---

## Decision Register

| # | Decision | Rationale |
|---|---|---|
| D1 | v1 scope: pointer move + button + wheel + keyboard down/up | Minimum useful remote-desktop loop; clipboard and touch each have design subtleties (permission model, multi-touch slot management) that benefit from dedicated follow-up specs |
| D2 | Transport: existing reliable bidi feedback stream | LAN-class tailnet RTTs make datagrams' latency advantage marginal; reusing the proven `dispatch_feedback_bytes` router avoids a second control plane; lost `keydown` over an unreliable channel would leave a stuck key |
| D3 | Keyboard encoding: X11 KeySyms over the wire | Survives layout mismatches between client and server (browser QWERTY 'a' produces guest 'a' regardless of guest XKB layout); aligns with what `xdotool` and `wlroots` use internally; JS-side keymap is ~60 lines |
| D4 | Per-event messages with a sub-kind byte | Keeps the top-level message-type space sparse for future evolution (clipboard, touch, file transfer); routes all input into one `INPUT_MSG_TYPE` arm of the existing dispatcher |
| D5 | Server-frame-pixel coordinates as `i16` on the wire | Browser already knows the framebuffer size from the dimensions datagram and does the canvas→FB scale before sending; server stays stateless on input geometry; `i16` lets `(-1, -1)` mean "pointer left the canvas" |
| D6 | `InputInjector` trait on the lib/daemon seam, `Option<Arc<dyn …>>` on `IoBridge` | Keeps X11 plumbing out of `ghostframe-lib` (cross-platform clean); test bridges pass `None` so the existing test fixtures don't need an X server |
| D7 | Synchronous x11rb calls from the tokio task (no `spawn_blocking`, no dedicated thread) | Each XTestFakeInput is sub-millisecond over the local X socket; the existing X11 capture path already runs synchronously in the same task; revisitable once we measure |
| D8 | Separate X11 connection for `XTestInjector` (distinct from capture + XDamage) | Prevents a `XTestFakeInput` write from contending with a capture-side `GetImage` reply on the same connection; mirrors how `XDamageMonitor` already owns its own connection |
| D9 | Hover-only pointer + click-to-focus keyboard, no `pointerLock` | Matches the user's mental model of a remote desktop pane; lets `Tab`/`Shift-Tab` escape into the browser UI; pointer-lock is a games / FPS pattern that hides the cursor |
| D10 | Single 30 Hz coalesce of `pointermove` via `requestAnimationFrame` on the client | Matches frame rate (no point sending position updates faster than we can render the resulting motion); keeps the bidi stream uncluttered; identical to how VNC clients sample |
| D11 | Modifier reconciliation deferred: trust the browser's per-event modifier flags | A stuck modifier (lost `keyup` on a reliable stream — shouldn't happen but in theory) is recoverable by the user pressing the same key again; a periodic "release everything" heartbeat is YAGNI for v1 |
| D12 | Threat model: tailnet membership = authorized to drive the desktop | Matches the framebuffer-capture threat model (anyone who can see the screen can already see your typing); per-session viewer-only scoping is a future capability bit |

---

## Architecture

### Topology

```
Browser canvas                              Server (ghostframe-xdaemon)
┌────────────────┐                          ┌──────────────────────────┐
│ pointerdown    │                          │  X server (:1)            │
│ pointermove    │                          │  enlightenment + apps     │
│ pointerup      │                          │  ▲                        │
│ wheel          │                          │  │ XTestFakeInput        │
│ keydown        │                          │  │ (XTEST extension)     │
│ keyup          │                          │  │                        │
└──────┬─────────┘                          │ ┌┴──────────────────────┐ │
       │ DOM listeners                       │ │ XTestInjector         │ │
       │ keymap.ts (KeyboardEvent.key →      │ │ (x11rb XTEST client)  │ │
       │            X11 KeySym)              │ └────────▲──────────────┘ │
       │ wire.ts   (RAF-coalesced moves,     │          │                │
       │            canvas → FB px scale)    │  apply_input(injector,   │
       ▼                                     │              &msg)        │
  encodeInput…(bytes)                        │          ▲                │
       │                                     │          │                │
       ▼                                     │  dispatch_feedback_bytes  │
  feedbackWriter.write(bytes) ────────────────▶ INPUT_MSG_TYPE (0x05)    │
       │ (reliable bidi stream — same        │          ▲                │
       │  stream HELLO + DECODE_ERROR        │          │                │
       │  + ReceiverFeedback use)            │  IoBridge::run loop polls │
       │                                     │  feedback streams,        │
       └────── WT bidi stream ───────────────┘  routes by first byte     │
                                             └──────────────────────────┘
```

### Wire format

One new top-level message type on the bidi feedback stream:

```
INPUT_MSG_TYPE = 0x05    (next after DECODE_ERROR_MSG_TYPE = 0x04)
```

Each event is one self-describing variable-size message. All multi-byte fields are big-endian, matching the existing FEEDBACK/HELLO/DECODE_ERROR conventions:

```
                                                          length
                                                          (bytes)
┌────────────────────────────────────────────────────────────────┐
│ pointer-move    [0x05][0x01]  x:i16   y:i16                    │  6
│ pointer-button  [0x05][0x02]  x:i16   y:i16  button:u8 down:u8 │  8
│ wheel           [0x05][0x03]  dx:i16  dy:i16                   │  6
│ keysym-down     [0x05][0x04]  keysym:u32                       │  6
│ keysym-up       [0x05][0x05]  keysym:u32                       │  6
└────────────────────────────────────────────────────────────────┘
```

- **Coordinates** are framebuffer pixels (`i16`). Browser scales canvas-pixel → FB-pixel before sending. `(-1, -1)` is the legal "pointer left the canvas" marker.
- **Buttons** are X11 numbers: `1=left, 2=middle, 3=right, 4=wheel-up, 5=wheel-down, 6=wheel-left, 7=wheel-right, 8=back, 9=forward`. `down: 1=press, 0=release`.
- **KeySyms** are X11 KeySym values (32-bit). `0x0061 = 'a'`, `0xff0d = Return`, `0xff52 = Up`, `0x00f1 = ñ`, `0x0100xxxx = high BMP`.
- **Sub-kind byte after the message-type byte** keeps the top-level dispatcher (`dispatch_feedback_bytes`) a single `match` on byte[0]. Input-specific decoding is one level down.
- **No batched form in v1.** At 30 Hz pointer + bursty keystrokes the peak rate is ~60 msg/s. A `[0x05][0xff][count:u16][...]` batched form is a forward-compatible extension if a profile ever flags per-event overhead.

### Browser side

Three new files under `ghostframe-web-client/src/input/`:

**`keymap.ts`** — pure JS table. Two parts:
- Named keys: ~50-entry record literal (`Enter → 0xff0d`, `Escape → 0xff1b`, `ArrowUp → 0xff52`, `F1..F12`, `Tab`, `Backspace`, `Delete`, `Home/End/PageUp/PageDown`, modifiers `Shift → Shift_L = 0xffe1` etc.). Generated by hand from the X11 keysymdef header; one-time write.
- Printable text fallback: if `event.key` is single-codepoint and not in the named map, the X11 keysym-from-Unicode rule applies — code points `0x20..0xff` map straight to that value, higher BMP codepoints OR with `0x01000000`. Covers ASCII, Latin-1 supplement, Unicode (ñ, 中, 🌳) without an explicit table.
- Returns `null` for `event.key === 'Dead'` (composing state) so we don't transmit a half-formed dead-key event; the resolved character will arrive in the next `keydown`.

**`encode.ts`** — five tiny functions returning `Uint8Array`, mirroring `feedback.ts`'s style for `encodeHello`/`encodeDecodeError`:

```ts
encodePointerMove(x, y): Uint8Array        // 6 bytes
encodePointerButton(x, y, button, down): Uint8Array   // 8 bytes
encodeWheel(dx, dy): Uint8Array            // 6 bytes
encodeKeyDown(keysym): Uint8Array          // 6 bytes
encodeKeyUp(keysym): Uint8Array            // 6 bytes
```

**`wire.ts`** — a single `attachInputCapture(canvas, feedbackWriter, getFrameDims)`:
1. Sets `canvas.tabIndex = 0` so it can receive keyboard focus.
2. Hooks `pointerdown`, `pointerup`, `pointermove`, `wheel`, `keydown`, `keyup` with `passive: false` so it can `preventDefault()` on canvas-bound events (stops browser scroll on wheel, stops Tab leaving the canvas via the keyboard).
3. Coalesces `pointermove` to 30 Hz via `requestAnimationFrame` — only the latest position is sent per RAF; intermediate moves are dropped. Pointer button events and key events are sent immediately.
4. Maps canvas-pixel → FB-pixel using `getFrameDims()` (`main.ts` already exposes the framebuffer size from the dimensions datagram).
5. On `pointerleave`, sends one `(-1, -1)` pointer-move marker so the server can hide its cursor.
6. All sends route through the existing `feedbackWriter` from `main.ts`.

**Focus model:** hover-only for pointer (canvas captures move/click/wheel only while pointer is over it), click-to-focus for keyboard (`tabindex=0` + `canvas.focus()` on `pointerdown`). Page `Tab`/`Shift-Tab` still work to escape into the browser UI.

### Wiring in `main.ts`

Two lines added near the existing `feedbackWriter` construction (around line 317 in current `main.ts`):

```ts
const { attachInputCapture } = await import('./input/wire');
attachInputCapture(canvasEl, feedbackWriter, () => renderer.framebuffer);
```

### Server side

**`ghostframe-xdaemon/Cargo.toml`** — add the `xtest` feature to the existing `x11rb` dep (already pulls `damage`):

```toml
x11rb = { version = "0.13", features = ["damage", "xtest"] }
```

No new top-level dependency.

**`ghostframe-lib/src/transport/input_inject.rs`** (new file) — trait + message decoder:

```rust
pub const INPUT_MSG_TYPE: u8 = 0x05;

pub enum InputMsg {
    PointerMove { x: i16, y: i16 },
    PointerButton { x: i16, y: i16, button: u8, down: bool },
    Wheel { dx: i16, dy: i16 },
    Key { keysym: u32, down: bool },
}

pub trait InputInjector: Send + Sync {
    fn pointer_move(&self, x: i16, y: i16);
    fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool);
    fn wheel(&self, dx: i16, dy: i16);
    fn key(&self, keysym: u32, down: bool);
}

pub fn decode_input_msg(data: &[u8]) -> Option<InputMsg> { /* … */ }
pub fn apply_input(injector: &dyn InputInjector, msg: &InputMsg) { /* switch */ }
```

**`IoBridge`** — new `input_injector: Option<Arc<dyn InputInjector>>` field. `Option` because the test-only `IoBridge` constructors pass `None`. Production wires it via `GhostframeServer::new(...)` with one extra parameter.

**`dispatch_feedback_bytes` extension** — five lines:

```rust
INPUT_MSG_TYPE => {
    if let Some(injector) = self.input_injector.as_ref() {
        if let Some(msg) = decode_input_msg(remaining) {
            apply_input(injector.as_ref(), &msg);
        }
    }
    // advance cursor past the variable-length input msg
}
```

**`ghostframe-xdaemon/src/input_inject.rs`** (new file, ~120 lines) — production `XTestInjector`:

- Opens its own `x11rb::connect(None)` (separate from the capture + XDamage connections).
- Checks the XTEST extension via `query_extension`; returns `Err` if absent.
- Six trait methods, each one X11 protocol call:
  - `pointer_move(x, y)` → `xtest_fake_input(MotionNotify, 0, 0, root, x, y, 0)` + `flush()`.
  - `pointer_button(x, y, button, down)` → `pointer_move(x, y)` first (so the click lands at the right place if a `pointermove` got coalesced away) then `xtest_fake_input(ButtonPress | ButtonRelease, button, 0, root, x, y, 0)` + `flush()`.
  - `wheel(dx, dy)` → `dy.abs()` clicks of button 4 (`dy < 0`) or 5 (`dy > 0`); `dx` similarly maps to buttons 6/7. Each click is a press+release pair.
  - `key(keysym, down)` → server-side `XkbKeysymToKeycode` (looked up once per process via `get_keyboard_mapping`, cached as a `HashMap<u32, u8>`), then `xtest_fake_input(KeyPress | KeyRelease, keycode, ...)` + `flush()`. Unknown keysym: warn once, skip.

**Wiring in `ghostframe-xdaemon/src/main.rs`** — construct `XTestInjector::new()` after `wait_for_x11` succeeds (the X server is guaranteed up at that point), wrap in `Arc`, pass to `GhostframeServer::new(...)`. If `XTestInjector::new()` returns `Err` (XTEST extension missing on the host's xorg-server build), the daemon logs and continues — frames still stream, just no input.

---

## Data flow

Single round trip per event:

```
1. User moves pointer over canvas
2. Browser fires pointermove (every ~16ms at 60fps refresh)
3. wire.ts buffers latest position; at next RAF tick, picks up the latest
4. wire.ts scales canvas coords → FB coords via getFrameDims()
5. wire.ts calls encodePointerMove(x_fb, y_fb) → Uint8Array(6)
6. wire.ts calls feedbackWriter.write(bytes)
7. WT bidi stream delivers reliably to the server in order
8. drain_feedback (existing path) pulls the bytes
9. dispatch_feedback_bytes sees byte[0] == 0x05, calls decode_input_msg
10. apply_input(injector, &InputMsg::PointerMove { x, y })
11. XTestInjector.pointer_move(x, y) — xtest_fake_input + flush
12. X server moves the virtual pointer
13. enlightenment + apps see the motion
14. Capture loop next frame includes the cursor at the new position
15. User sees the cursor caught up in the canvas
```

For pointer buttons, key events, and wheel: identical path, different sub-kind. The "buffer + RAF" stage at step 3 only applies to `pointermove`; all other events go straight to step 5.

---

## Error handling

- **`XTestInjector::new()` fails** (no XTEST ext, no DISPLAY): log + continue with `input_injector = None`. Frames still stream.
- **`decode_input_msg` returns `None`** (malformed wire bytes, or sub-kind byte not in {0x01..0x05}): log at debug and **abandon the rest of the current feedback chunk** — input messages are variable-length and we can't know the size of an unrecognized sub-kind, so resyncing inside the chunk is unsafe. The next outer `dispatch_feedback_bytes` call (next bidi-stream read batch) starts fresh. Fixed-size HELLO/DECODE_ERROR/FEEDBACK arms can still advance their cursors normally because their length is determined by `byte[0]` alone.
- **`keysym → keycode` lookup misses** (server's current X keymap has no entry for the keysym): warn once per missing keysym, skip the inject. Avoids spam if the user holds a key with no mapping.
- **X11 protocol error from `XTestFakeInput`** (very rare on local socket): log; the connection survives — x11rb doesn't auto-close on a single protocol error.
- **`feedbackWriter.write` rejects** (stream closed mid-session): the existing `console.warn` path in `main.ts` already handles it; no input-specific recovery needed.

---

## Testing

Three layers:

1. **Wire-format unit tests** (Rust + TS). `decode_input_msg` round-trip against fixtures for each of the five sub-kinds. Mirror `encodeHello` test style in `feedback.test.ts`. Catches byte-layout drift between client and server.
2. **InputInjector trait + mock**. A `MockInjector` in the lib's tests records each call; `apply_input` is exercised over each `InputMsg` variant. Verifies the dispatch path without an X server.
3. **e2e smoke (one new test, Chromium-only)**. `e2e_input_injection_chromium`: setup_e2e_webgpu, run `xterm` in the guest session, simulate `pointermove + keypress 'q'` via `setup.browser.evaluate(...)` injecting synthetic DOM events, then `xdotool getactivewindow getwindowname` server-side to confirm xterm received the keystroke. Loops back through the whole stack — browser → bidi stream → `dispatch_feedback_bytes` → `XTestInjector` → X server → xterm. Gated on the test-server image growing `xterm` + `xdotool` (skip-list entry until then).

---

## Components

### New
- `ghostframe-lib/src/transport/input_inject.rs` (~120 lines) — `INPUT_MSG_TYPE`, `InputMsg`, `InputInjector` trait, `decode_input_msg`, `apply_input`, unit tests.
- `ghostframe-xdaemon/src/input_inject.rs` (~120 lines) — `XTestInjector` struct + `InputInjector` impl + once-loaded keysym→keycode cache.
- `ghostframe-web-client/src/input/keymap.ts` (~60 lines) — `keyboardEventToKeysym(event: KeyboardEvent): number | null`.
- `ghostframe-web-client/src/input/encode.ts` (~50 lines) — five encoder functions.
- `ghostframe-web-client/src/input/wire.ts` (~120 lines) — `attachInputCapture(canvas, feedbackWriter, getFrameDims)`.
- `ghostframe-web-client/tests/input.test.ts` (~80 lines) — wire-format + keymap unit tests.

### Modified
- `ghostframe-lib/src/transport/io_bridge.rs` — add `input_injector: Option<Arc<dyn InputInjector>>` field; extend `dispatch_feedback_bytes` with the `INPUT_MSG_TYPE` arm.
- `ghostframe-lib/src/server.rs` — `GhostframeServer::new(...)` takes an optional `input_injector` and threads it to `IoBridge`.
- `ghostframe-xdaemon/src/main.rs` — construct `XTestInjector::new()` after `wait_for_x11` succeeds; pass to `GhostframeServer::new(...)`.
- `ghostframe-xdaemon/Cargo.toml` — `x11rb` features `["damage", "xtest"]`.
- `ghostframe-web-client/src/main.ts` — call `attachInputCapture(...)` near existing `feedbackWriter` setup.

---

## Out of scope

- **Clipboard sync.** Permission-model design has its own subtleties (browser asks "allow paste?"); covered by a follow-up spec.
- **Touch events.** Multi-touch slot management via XInput2; follow-up spec.
- **Modifier-state reconciliation.** No periodic "release everything" heartbeat in v1.
- **Pointer Lock / cursor capture.** Games / FPS pattern.
- **Server→browser cursor sync** (CSS-rendered cursor avoiding capture latency).
- **Per-session viewer-only sharing.** v1 trusts tailnet membership.
- **Input replay attack resistance.** Future work can layer per-session auth on the existing TLS + cert-hash pinning.
