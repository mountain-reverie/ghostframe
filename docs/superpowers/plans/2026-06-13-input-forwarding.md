# Input Forwarding Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Browser-to-server input forwarding — pointer move/button/wheel + keyboard down/up — so the guest desktop becomes interactive when viewed through the daemon's URL.

**Architecture:** New `INPUT_MSG_TYPE = 0x05` on the existing reliable feedback bidi stream, with five sub-kinds. KeySym-on-the-wire encoding (browser maps `KeyboardEvent.key` to X11 KeySym JS-side). `InputInjector` trait on the lib/daemon seam keeps `ghostframe-lib` X11-free; `ghostframe-xdaemon` owns the production `XTestInjector` using `x11rb`'s `xtest` feature. Sync calls from the tokio task — local X socket round-trips are sub-millisecond.

**Tech Stack:** Rust 2021 + tokio + `x11rb` 0.13 (already a dep; new `xtest` feature). TypeScript + DOM `pointerevent` / `keyboardevent` / `wheelevent` APIs. Reuses the existing WebTransport bidi feedback stream and `dispatch_feedback_bytes` router in `ghostframe-lib`.

**Spec:** `docs/superpowers/specs/2026-06-13-input-forwarding-design.md`

---

## File map

### New
- `ghostframe-lib/src/transport/input_inject.rs` — `INPUT_MSG_TYPE`, `InputMsg`, `InputInjector` trait, `decode_input_msg`, `apply_input`, unit tests.
- `ghostframe-xdaemon/src/input_inject.rs` — `XTestInjector` struct + `InputInjector` impl + keysym→keycode cache.
- `ghostframe-web-client/src/input/keymap.ts` — `keyboardEventToKeysym` (named-key table + Unicode rule).
- `ghostframe-web-client/src/input/encode.ts` — 5 wire encoders.
- `ghostframe-web-client/src/input/wire.ts` — `attachInputCapture` (DOM listeners + RAF coalesce + canvas→FB scale + dispatch to `feedbackWriter`).
- `ghostframe-web-client/tests/input.test.ts` — wire-format + keymap unit tests.

### Modified
- `ghostframe-lib/src/transport/io_bridge.rs` — add `input_injector: Option<Arc<dyn InputInjector>>` field; extend `dispatch_feedback_bytes` with the `INPUT_MSG_TYPE` arm.
- `ghostframe-lib/src/transport/mod.rs` — re-export `input_inject`.
- `ghostframe-lib/src/server.rs` — `GhostframeServer::new` takes `input_injector: Option<Arc<dyn InputInjector>>`, threads to `IoBridge`.
- `ghostframe-xdaemon/src/main.rs` — construct `XTestInjector::new()` after `wait_for_x11` and pass to `GhostframeServer::new`.
- `ghostframe-xdaemon/Cargo.toml` — `x11rb` features: `["damage", "xtest"]`.
- `ghostframe-xdaemon/src/lib.rs` (or add `mod input_inject;` to `main.rs` if no `lib.rs` exists).
- `ghostframe-web-client/src/main.ts` — call `attachInputCapture(...)` near the existing `feedbackWriter` setup.

---

## Task 1: lib `input_inject.rs` — constants, types, trait

**Files:**
- Create: `ghostframe-lib/src/transport/input_inject.rs`
- Modify: `ghostframe-lib/src/transport/mod.rs`

- [ ] **Step 1: Create the module skeleton**

Create `ghostframe-lib/src/transport/input_inject.rs` with:

```rust
//! Browser → server input forwarding.
//!
//! New `INPUT_MSG_TYPE = 0x05` on the bidi feedback stream. Sub-kind byte
//! after the message-type selects pointer-move / pointer-button / wheel /
//! key-down / key-up. KeySyms (not scan codes) on the wire so layout
//! mismatches between browser host and guest desktop don't silently
//! corrupt characters.
//!
//! See `docs/superpowers/specs/2026-06-13-input-forwarding-design.md`.

/// Top-level feedback message type for input events. Routed by the first
/// byte in `IoBridge::dispatch_feedback_bytes`. Sub-kind byte at offset 1
/// selects the specific event (see `decode_input_msg`).
pub const INPUT_MSG_TYPE: u8 = 0x05;

const SUB_POINTER_MOVE: u8 = 0x01;
const SUB_POINTER_BUTTON: u8 = 0x02;
const SUB_WHEEL: u8 = 0x03;
const SUB_KEY_DOWN: u8 = 0x04;
const SUB_KEY_UP: u8 = 0x05;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputMsg {
    PointerMove { x: i16, y: i16 },
    PointerButton { x: i16, y: i16, button: u8, down: bool },
    Wheel { dx: i16, dy: i16 },
    KeyDown { keysym: u32 },
    KeyUp { keysym: u32 },
}

/// Abstract handle the I/O layer uses to inject events into a windowing
/// system. Production impl lives in `ghostframe-xdaemon` (X11 via
/// `x11rb`'s `xtest`); test bridges pass `None` and skip injection.
pub trait InputInjector: Send + Sync {
    fn pointer_move(&self, x: i16, y: i16);
    fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool);
    fn wheel(&self, dx: i16, dy: i16);
    fn key(&self, keysym: u32, down: bool);
}
```

- [ ] **Step 2: Register the module**

Open `ghostframe-lib/src/transport/mod.rs` and add `pub mod input_inject;` in alphabetical order with the other `pub mod ...` lines. (Verify the file's existing style by reading it first; the project keeps these alphabetical.)

- [ ] **Step 3: Verify it compiles**

```bash
cd /home/cedric/work/ghostframe
cargo check -p ghostframe-lib 2>&1 | tail -5
```

Expected: `Finished` with no errors. The trait has no impl yet, that's fine.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/input_inject.rs ghostframe-lib/src/transport/mod.rs
git commit -m "feat(transport): input_inject module skeleton — INPUT_MSG_TYPE, InputMsg, trait"
```

Footer: `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`.

---

## Task 2: lib `decode_input_msg` + round-trip tests

**Files:**
- Modify: `ghostframe-lib/src/transport/input_inject.rs`

- [ ] **Step 1: Write the failing tests first**

Append to `ghostframe-lib/src/transport/input_inject.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_pointer_move() {
        // [0x05][0x01][x:i16 BE][y:i16 BE]
        let bytes = [0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]; // x=400, y=-2
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::PointerMove { x: 400, y: -2 });
    }

    #[test]
    fn decode_pointer_button() {
        // [0x05][0x02][x:i16][y:i16][button:u8][down:u8]
        let bytes = [0x05, 0x02, 0x00, 0x05, 0x00, 0x0a, 0x01, 0x01];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 8);
        assert_eq!(
            msg,
            InputMsg::PointerButton { x: 5, y: 10, button: 1, down: true }
        );
    }

    #[test]
    fn decode_pointer_button_up() {
        let bytes = [0x05, 0x02, 0x00, 0x05, 0x00, 0x0a, 0x03, 0x00];
        let (msg, _) = decode_input_msg(&bytes).unwrap();
        assert_eq!(
            msg,
            InputMsg::PointerButton { x: 5, y: 10, button: 3, down: false }
        );
    }

    #[test]
    fn decode_wheel() {
        // [0x05][0x03][dx:i16][dy:i16]; dy=+1 (down)
        let bytes = [0x05, 0x03, 0x00, 0x00, 0x00, 0x01];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::Wheel { dx: 0, dy: 1 });
    }

    #[test]
    fn decode_key_down() {
        // [0x05][0x04][keysym:u32 BE]; 'a' = 0x00000061
        let bytes = [0x05, 0x04, 0x00, 0x00, 0x00, 0x61];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::KeyDown { keysym: 0x61 });
    }

    #[test]
    fn decode_key_up() {
        // Return key = XK_Return = 0xff0d
        let bytes = [0x05, 0x05, 0x00, 0x00, 0xff, 0x0d];
        let (msg, consumed) = decode_input_msg(&bytes).unwrap();
        assert_eq!(consumed, 6);
        assert_eq!(msg, InputMsg::KeyUp { keysym: 0xff0d });
    }

    #[test]
    fn decode_rejects_wrong_msg_type() {
        let bytes = [0x04, 0x01, 0, 0, 0, 0]; // 0x04 = DECODE_ERROR, not us
        assert_eq!(decode_input_msg(&bytes), None);
    }

    #[test]
    fn decode_rejects_unknown_subkind() {
        let bytes = [0x05, 0xfe, 0, 0, 0, 0];
        assert_eq!(decode_input_msg(&bytes), None);
    }

    #[test]
    fn decode_rejects_short_buffer() {
        // pointer-move needs 6 bytes
        assert_eq!(decode_input_msg(&[0x05, 0x01, 0, 0]), None);
        // empty
        assert_eq!(decode_input_msg(&[]), None);
        // just the type byte
        assert_eq!(decode_input_msg(&[0x05]), None);
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p ghostframe-lib --lib transport::input_inject 2>&1 | tail -15
```

Expected: FAIL with `cannot find function 'decode_input_msg'` in scope.

- [ ] **Step 3: Implement `decode_input_msg`**

Add ABOVE the `#[cfg(test)]` block (after the trait):

```rust
/// Returns `Some((msg, consumed_bytes))` on success, `None` on:
///   - empty input
///   - byte[0] != INPUT_MSG_TYPE
///   - unknown sub-kind (byte[1] not in {0x01..0x05})
///   - short buffer (insufficient bytes for the sub-kind)
///
/// All multi-byte fields are big-endian, matching FEEDBACK/HELLO/DECODE_ERROR.
pub fn decode_input_msg(data: &[u8]) -> Option<(InputMsg, usize)> {
    if data.len() < 2 || data[0] != INPUT_MSG_TYPE {
        return None;
    }
    match data[1] {
        SUB_POINTER_MOVE => {
            if data.len() < 6 {
                return None;
            }
            let x = i16::from_be_bytes([data[2], data[3]]);
            let y = i16::from_be_bytes([data[4], data[5]]);
            Some((InputMsg::PointerMove { x, y }, 6))
        }
        SUB_POINTER_BUTTON => {
            if data.len() < 8 {
                return None;
            }
            let x = i16::from_be_bytes([data[2], data[3]]);
            let y = i16::from_be_bytes([data[4], data[5]]);
            let button = data[6];
            let down = data[7] != 0;
            Some((
                InputMsg::PointerButton { x, y, button, down },
                8,
            ))
        }
        SUB_WHEEL => {
            if data.len() < 6 {
                return None;
            }
            let dx = i16::from_be_bytes([data[2], data[3]]);
            let dy = i16::from_be_bytes([data[4], data[5]]);
            Some((InputMsg::Wheel { dx, dy }, 6))
        }
        SUB_KEY_DOWN => {
            if data.len() < 6 {
                return None;
            }
            let keysym =
                u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
            Some((InputMsg::KeyDown { keysym }, 6))
        }
        SUB_KEY_UP => {
            if data.len() < 6 {
                return None;
            }
            let keysym =
                u32::from_be_bytes([data[2], data[3], data[4], data[5]]);
            Some((InputMsg::KeyUp { keysym }, 6))
        }
        _ => None,
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

```bash
cargo test -p ghostframe-lib --lib transport::input_inject 2>&1 | tail -10
```

Expected: `test result: ok. 9 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/input_inject.rs
git commit -m "feat(transport): decode_input_msg + 9 round-trip tests"
```

---

## Task 3: lib `apply_input` + MockInjector dispatcher tests

**Files:**
- Modify: `ghostframe-lib/src/transport/input_inject.rs`

- [ ] **Step 1: Write the failing tests**

Add to the `#[cfg(test)] mod tests` block (above the closing brace):

```rust
    /// Records every trait call as a typed event. Used to verify that
    /// `apply_input` routes each `InputMsg` variant to the right method
    /// with the right arguments.
    struct MockInjector {
        calls: std::sync::Mutex<Vec<MockCall>>,
    }
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum MockCall {
        PointerMove(i16, i16),
        PointerButton(i16, i16, u8, bool),
        Wheel(i16, i16),
        Key(u32, bool),
    }
    impl MockInjector {
        fn new() -> Self {
            Self { calls: std::sync::Mutex::new(Vec::new()) }
        }
        fn calls(&self) -> Vec<MockCall> {
            self.calls.lock().unwrap().clone()
        }
    }
    impl InputInjector for MockInjector {
        fn pointer_move(&self, x: i16, y: i16) {
            self.calls.lock().unwrap().push(MockCall::PointerMove(x, y));
        }
        fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool) {
            self.calls
                .lock()
                .unwrap()
                .push(MockCall::PointerButton(x, y, button, down));
        }
        fn wheel(&self, dx: i16, dy: i16) {
            self.calls.lock().unwrap().push(MockCall::Wheel(dx, dy));
        }
        fn key(&self, keysym: u32, down: bool) {
            self.calls.lock().unwrap().push(MockCall::Key(keysym, down));
        }
    }

    #[test]
    fn apply_pointer_move_calls_pointer_move() {
        let m = MockInjector::new();
        apply_input(&m, &InputMsg::PointerMove { x: 17, y: 42 });
        assert_eq!(m.calls(), vec![MockCall::PointerMove(17, 42)]);
    }

    #[test]
    fn apply_pointer_button_calls_pointer_button() {
        let m = MockInjector::new();
        apply_input(
            &m,
            &InputMsg::PointerButton { x: 1, y: 2, button: 3, down: true },
        );
        assert_eq!(
            m.calls(),
            vec![MockCall::PointerButton(1, 2, 3, true)]
        );
    }

    #[test]
    fn apply_wheel_calls_wheel() {
        let m = MockInjector::new();
        apply_input(&m, &InputMsg::Wheel { dx: 0, dy: -3 });
        assert_eq!(m.calls(), vec![MockCall::Wheel(0, -3)]);
    }

    #[test]
    fn apply_key_down_calls_key_with_down_true() {
        let m = MockInjector::new();
        apply_input(&m, &InputMsg::KeyDown { keysym: 0xff0d });
        assert_eq!(m.calls(), vec![MockCall::Key(0xff0d, true)]);
    }

    #[test]
    fn apply_key_up_calls_key_with_down_false() {
        let m = MockInjector::new();
        apply_input(&m, &InputMsg::KeyUp { keysym: 0xff0d });
        assert_eq!(m.calls(), vec![MockCall::Key(0xff0d, false)]);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cargo test -p ghostframe-lib --lib transport::input_inject 2>&1 | tail -10
```

Expected: FAIL with `cannot find function 'apply_input'`.

- [ ] **Step 3: Implement `apply_input`**

Add right after `decode_input_msg` (still above the `#[cfg(test)]` block):

```rust
/// Routes an `InputMsg` to the matching `InputInjector` trait method.
/// Kept separate from `decode_input_msg` so the parser and the
/// dispatcher can be tested independently.
pub fn apply_input(injector: &dyn InputInjector, msg: &InputMsg) {
    match *msg {
        InputMsg::PointerMove { x, y } => injector.pointer_move(x, y),
        InputMsg::PointerButton { x, y, button, down } => {
            injector.pointer_button(x, y, button, down)
        }
        InputMsg::Wheel { dx, dy } => injector.wheel(dx, dy),
        InputMsg::KeyDown { keysym } => injector.key(keysym, true),
        InputMsg::KeyUp { keysym } => injector.key(keysym, false),
    }
}
```

- [ ] **Step 4: Run tests**

```bash
cargo test -p ghostframe-lib --lib transport::input_inject 2>&1 | tail -10
```

Expected: `test result: ok. 14 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/input_inject.rs
git commit -m "feat(transport): apply_input dispatcher + MockInjector tests"
```

---

## Task 4: Wire `input_injector` into `IoBridge` + dispatch arm

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

- [ ] **Step 1: Add the field**

Find the `pub struct IoBridge { ... }` declaration (around line 130-265 — the struct is large). Add this field at the end of the struct, just before the closing brace:

```rust
    /// Production-only handle for browser-driven input injection (see
    /// `transport::input_inject`). `None` on the test paths whose
    /// `IoBridge` constructor doesn't take an injector — the
    /// `INPUT_MSG_TYPE` dispatch arm silently skips messages when this
    /// is `None`.
    input_injector: Option<std::sync::Arc<dyn crate::transport::input_inject::InputInjector>>,
```

- [ ] **Step 2: Initialize the field in every constructor**

`IoBridge` has three constructors:
1. The production `new(...)` (around line 426).
2. `new_with_stream_for_test` (around line 2616-ish — search for `#[cfg(test)]`).
3. `new_with_frames_for_test` (around line 2670-ish).

In all three, where the struct literal lists every field, append:

```rust
            input_injector: None,
```

For #1 (`new`), the field is `None` because the production path needs an out-of-band injector handle that gets plugged via `new_with_input_injector(...)` — see Task 5.

- [ ] **Step 3: Extend `dispatch_feedback_bytes`**

Find `pub(crate) fn dispatch_feedback_bytes` (around io_bridge.rs:1052). Read the existing match block — it handles `FEEDBACK_MSG_TYPE`, `HELLO_MSG_TYPE`, `DECODE_ERROR_MSG_TYPE`. Add a new arm BEFORE the catch-all (look for the final wildcard `_ =>`):

```rust
                crate::transport::input_inject::INPUT_MSG_TYPE => {
                    use crate::transport::input_inject::{apply_input, decode_input_msg};
                    match decode_input_msg(&data[cursor..]) {
                        Some((msg, consumed)) => {
                            if let Some(injector) = self.input_injector.as_ref() {
                                apply_input(injector.as_ref(), &msg);
                            }
                            cursor += consumed;
                        }
                        None => {
                            // Variable-length and we can't know the size of
                            // an unrecognized sub-kind. Bail on this chunk;
                            // the next stream-read will start fresh.
                            tracing::debug!(
                                cursor,
                                remaining = data.len() - cursor,
                                "input_msg decode failed; abandoning chunk"
                            );
                            break;
                        }
                    }
                }
```

Read the existing arms carefully to match the project's exact style for cursor advancement; the snippet above mirrors the HELLO/DECODE_ERROR pattern (cursor += length).

- [ ] **Step 4: Update the rustdoc for `dispatch_feedback_bytes`**

Find the doc comment listing message types (around io_bridge.rs:1044-1047). Add one line so the documented set matches the implemented set:

```rust
    /// - `0x01` FEEDBACK_MSG_TYPE  (22 bytes)  — `ReceiverFeedback`
    /// - `0x03` HELLO_MSG_TYPE     (2 bytes)   — capability advertisement
    /// - `0x04` DECODE_ERROR_MSG_TYPE (5 bytes) — per-tile decode failure
    /// - `0x05` INPUT_MSG_TYPE     (variable) — pointer/wheel/key event
```

- [ ] **Step 5: Add a wire-level dispatch test**

Find the `#[cfg(test)] mod tests` block in io_bridge.rs and add:

```rust
    #[tokio::test]
    async fn dispatch_feedback_bytes_routes_input_to_injector() {
        use crate::transport::input_inject::InputInjector;
        use std::sync::{Arc, Mutex};

        struct CountingInjector {
            moves: Mutex<Vec<(i16, i16)>>,
        }
        impl InputInjector for CountingInjector {
            fn pointer_move(&self, x: i16, y: i16) {
                self.moves.lock().unwrap().push((x, y));
            }
            fn pointer_button(&self, _: i16, _: i16, _: u8, _: bool) {}
            fn wheel(&self, _: i16, _: i16) {}
            fn key(&self, _: u32, _: bool) {}
        }

        let injector = Arc::new(CountingInjector { moves: Mutex::new(Vec::new()) });
        let mut bridge = make_bridge_for_test().await;
        bridge.input_injector = Some(injector.clone() as Arc<dyn InputInjector>);

        // pointer-move (0x05 0x01 x=100 y=200) — 100 = 0x0064, 200 = 0x00c8
        bridge.dispatch_feedback_bytes(&[0x05, 0x01, 0x00, 0x64, 0x00, 0xc8]);

        assert_eq!(injector.moves.lock().unwrap().clone(), vec![(100, 200)]);
    }

    #[tokio::test]
    async fn dispatch_feedback_bytes_silently_skips_input_when_no_injector() {
        // input_injector is None on the test bridge — must not panic.
        let mut bridge = make_bridge_for_test().await;
        bridge.dispatch_feedback_bytes(&[0x05, 0x01, 0x00, 0x64, 0x00, 0xc8]);
    }
```

- [ ] **Step 6: Run lib tests**

```bash
cargo test -p ghostframe-lib --lib transport 2>&1 | tail -8
```

Expected: all green, including the two new dispatcher tests.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(io_bridge): INPUT_MSG_TYPE dispatch arm + Option<Arc<dyn InputInjector>> field"
```

---

## Task 5: Thread `input_injector` through `GhostframeServer::new`

**Files:**
- Modify: `ghostframe-lib/src/server.rs`
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (add a constructor variant)

- [ ] **Step 1: Add a constructor variant on `IoBridge`**

Read the existing `IoBridge::new(...)` signature in `ghostframe-lib/src/transport/io_bridge.rs` (around line 426). Add a sibling method just below it:

```rust
    /// Same as `new`, but with an explicit `input_injector` for the
    /// production server path. The plain `new` keeps callers that don't
    /// care about input working unchanged.
    pub async fn new_with_input_injector(
        ghostbridge_config: &GhostbridgeConfig,
        listen_addr: &str,
        input_injector: Option<
            std::sync::Arc<dyn crate::transport::input_inject::InputInjector>,
        >,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let mut b = Self::new(ghostbridge_config, listen_addr).await?;
        b.input_injector = input_injector;
        Ok(b)
    }
```

- [ ] **Step 2: Extend `GhostframeServer::new`**

In `ghostframe-lib/src/server.rs`, read the existing `pub async fn new(...)` signature. Add a third parameter and thread it to `IoBridge`. The current signature is roughly:

```rust
pub async fn new(
    config: GhostbridgeConfig,
    listen_addr: &str,
) -> Result<Self, ...>
```

Change to:

```rust
pub async fn new(
    config: GhostbridgeConfig,
    listen_addr: &str,
    input_injector: Option<
        std::sync::Arc<dyn crate::transport::input_inject::InputInjector>,
    >,
) -> Result<Self, ...>
```

Inside the function body, find where `IoBridge::new` (or `new_with_frames`) is currently called. The existing code likely does:

```rust
let mut bridge = IoBridge::new_with_frames(&config, listen_addr, frame_rx).await?;
```

Update the construction to set the injector immediately after, then continue with the existing wiring:

```rust
let mut bridge = IoBridge::new_with_frames(&config, listen_addr, frame_rx).await?;
bridge.input_injector = input_injector;
```

(Direct field set rather than another `new_with_*` variant — `IoBridge` is `pub(crate)` to the lib's own modules, so this works inside `server.rs`. Verify by reading the field declaration from Task 4 Step 1 — if it's `pub(crate)` or just `pub`, the assignment compiles. If it's bare-private, change the field's visibility to `pub(crate)` in `io_bridge.rs`.)

- [ ] **Step 3: Update existing call sites**

The compile will fail at every call site of `GhostframeServer::new`. Find them:

```bash
grep -rn 'GhostframeServer::new' --include='*.rs' /home/cedric/work/ghostframe
```

For each, pass `None` for the new parameter (these are tests / harness / the bench codec_report — none want X11 injection). Example mechanical fix:

```rust
// before
GhostframeServer::new(config, ":443").await?
// after
GhostframeServer::new(config, ":443", None).await?
```

- [ ] **Step 4: Build the workspace**

```bash
cargo build -p ghostframe-lib -p ghostframe-xdaemon -p ghostframe-bench -p ghostframe-e2e 2>&1 | tail -8
```

Expected: clean.

- [ ] **Step 5: Run the existing lib tests**

```bash
cargo test -p ghostframe-lib --lib 2>&1 | tail -3
```

Expected: all green.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/server.rs ghostframe-lib/src/transport/io_bridge.rs
git status # confirm any other modified files (bench, e2e) are also staged
git add -A # if there are extra files
git commit -m "feat(server): GhostframeServer::new takes an Option<Arc<dyn InputInjector>>"
```

---

## Task 6: Enable `x11rb` xtest feature

**Files:**
- Modify: `ghostframe-xdaemon/Cargo.toml`

- [ ] **Step 1: Read the current dep line**

```bash
grep -n 'x11rb' ghostframe-xdaemon/Cargo.toml
```

- [ ] **Step 2: Add the xtest feature**

The current dep is something like `x11rb = "0.13"` or `x11rb = { version = "0.13", features = ["damage"] }`. Make sure both `damage` and `xtest` are listed:

```toml
x11rb = { version = "0.13", features = ["damage", "xtest"] }
```

- [ ] **Step 3: Verify it resolves**

```bash
cd /home/cedric/work/ghostframe
cargo check -p ghostframe-xdaemon 2>&1 | tail -5
```

Expected: clean. The xtest feature shouldn't pull any new top-level dep.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-xdaemon/Cargo.toml Cargo.lock
git commit -m "deps(xdaemon): x11rb xtest feature for input injection"
```

---

## Task 7: Implement `XTestInjector` in xdaemon

**Files:**
- Create: `ghostframe-xdaemon/src/input_inject.rs`
- Modify: `ghostframe-xdaemon/src/main.rs` (just add `mod input_inject;`)

- [ ] **Step 1: Create the file with the bare struct + constructor**

Create `ghostframe-xdaemon/src/input_inject.rs`:

```rust
//! X11 input injection via XTest extension.
//!
//! Implements `ghostframe_lib::transport::input_inject::InputInjector`
//! against the local X server (DISPLAY=:1 by default). One protocol
//! call per event over a dedicated x11rb connection — separate from
//! the capture + XDamage connections so an `XTestFakeInput` write
//! can't deadlock against a capture-side `GetImage` reply.
//!
//! Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

use anyhow::{anyhow, Context, Result};
use std::collections::HashMap;
use std::sync::Mutex;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::protocol::xtest::ConnectionExt as XTestExt;
use x11rb::rust_connection::RustConnection;

use ghostframe_lib::transport::input_inject::InputInjector;

pub struct XTestInjector {
    conn: RustConnection,
    root: u32,
    /// Lazily-built `keysym → keycode` lookup, populated on first miss
    /// via `get_keyboard_mapping`. Behind a Mutex because `key()` is
    /// `&self` (the trait promises `Send + Sync`).
    keysym_to_keycode: Mutex<HashMap<u32, u8>>,
}

impl XTestInjector {
    /// Connect to the local X server and confirm the XTEST extension is
    /// present. Errors if no DISPLAY or XTEST is missing — the caller
    /// (xdaemon's main) logs and continues with no injector.
    pub fn new() -> Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| anyhow!("x11rb::connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;

        // Confirm XTEST extension is available.
        let ext = conn
            .query_extension(b"XTEST")
            .context("query_extension(XTEST)")?
            .reply()
            .context("XTEST query_extension reply")?;
        if !ext.present {
            return Err(anyhow!(
                "XTEST extension not present on this Xorg build"
            ));
        }

        Ok(Self {
            conn,
            root,
            keysym_to_keycode: Mutex::new(HashMap::new()),
        })
    }
}
```

- [ ] **Step 2: Register the module**

Open `ghostframe-xdaemon/src/main.rs` and add `mod input_inject;` near the top with the other `mod` lines (alphabetical).

- [ ] **Step 3: Verify it compiles**

```bash
cargo check -p ghostframe-xdaemon 2>&1 | tail -5
```

Expected: clean (with a "struct never used" warning — fine for now).

- [ ] **Step 4: Implement `InputInjector` for `XTestInjector`**

Append to `ghostframe-xdaemon/src/input_inject.rs`:

```rust
impl XTestInjector {
    /// Look up a keysym → keycode mapping via the X server's current
    /// keymap. Caches successful lookups. Returns `None` if no keycode
    /// in the server's table corresponds to this keysym.
    fn keysym_to_keycode(&self, keysym: u32) -> Option<u8> {
        // Fast path: cache hit.
        if let Some(kc) = self.keysym_to_keycode.lock().unwrap().get(&keysym) {
            return Some(*kc);
        }

        // Pull the full keymap on first miss. Servers typically have
        // ~256 keycodes × ~6 keysyms-per-keycode = small.
        let setup = self.conn.setup();
        let min_keycode = setup.min_keycode;
        let max_keycode = setup.max_keycode;
        let count = (max_keycode - min_keycode) + 1;
        let map = self
            .conn
            .get_keyboard_mapping(min_keycode, count)
            .ok()?
            .reply()
            .ok()?;
        let per = map.keysyms_per_keycode as usize;
        if per == 0 {
            return None;
        }

        // Rebuild the cache from scratch — cheap and avoids partial-miss
        // questions.
        let mut cache = self.keysym_to_keycode.lock().unwrap();
        cache.clear();
        for (i, syms) in map.keysyms.chunks(per).enumerate() {
            let kc = min_keycode + i as u8;
            for &sym in syms {
                if sym != 0 {
                    cache.entry(sym).or_insert(kc);
                }
            }
        }
        cache.get(&keysym).copied()
    }
}

impl InputInjector for XTestInjector {
    fn pointer_move(&self, x: i16, y: i16) {
        let _ = self
            .conn
            .xtest_fake_input(
                x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
                0,
                x11rb::CURRENT_TIME,
                self.root,
                x,
                y,
                0,
            )
            .and_then(|cookie| {
                cookie.check()?;
                self.conn.flush().map_err(Into::into)
            });
    }

    fn pointer_button(&self, x: i16, y: i16, button: u8, down: bool) {
        // Move first so the click lands where the user expects, in case
        // a `pointermove` got coalesced away client-side.
        self.pointer_move(x, y);
        let event = if down {
            x11rb::protocol::xproto::BUTTON_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::BUTTON_RELEASE_EVENT
        };
        let _ = self
            .conn
            .xtest_fake_input(
                event,
                button,
                x11rb::CURRENT_TIME,
                self.root,
                x,
                y,
                0,
            )
            .and_then(|cookie| {
                cookie.check()?;
                self.conn.flush().map_err(Into::into)
            });
    }

    fn wheel(&self, dx: i16, dy: i16) {
        // X11 wheel = button 4 (up) / 5 (down) / 6 (left) / 7 (right);
        // one press+release pair per tick.
        let click_pair = |button: u8, count: u16| {
            for _ in 0..count {
                let _ = self.conn.xtest_fake_input(
                    x11rb::protocol::xproto::BUTTON_PRESS_EVENT,
                    button,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                );
                let _ = self.conn.xtest_fake_input(
                    x11rb::protocol::xproto::BUTTON_RELEASE_EVENT,
                    button,
                    x11rb::CURRENT_TIME,
                    self.root,
                    0,
                    0,
                    0,
                );
            }
            let _ = self.conn.flush();
        };
        if dy < 0 {
            click_pair(4, dy.unsigned_abs());
        } else if dy > 0 {
            click_pair(5, dy as u16);
        }
        if dx < 0 {
            click_pair(6, dx.unsigned_abs());
        } else if dx > 0 {
            click_pair(7, dx as u16);
        }
    }

    fn key(&self, keysym: u32, down: bool) {
        let Some(keycode) = self.keysym_to_keycode(keysym) else {
            tracing::warn!(
                keysym = format!("0x{:08x}", keysym),
                "no keycode for keysym; skipping key event"
            );
            return;
        };
        let event = if down {
            x11rb::protocol::xproto::KEY_PRESS_EVENT
        } else {
            x11rb::protocol::xproto::KEY_RELEASE_EVENT
        };
        let _ = self
            .conn
            .xtest_fake_input(
                event,
                keycode,
                x11rb::CURRENT_TIME,
                self.root,
                0,
                0,
                0,
            )
            .and_then(|cookie| {
                cookie.check()?;
                self.conn.flush().map_err(Into::into)
            });
    }
}
```

- [ ] **Step 5: Verify it compiles**

```bash
cargo build -p ghostframe-xdaemon 2>&1 | tail -8
```

Expected: clean. Some `Result` / `unused` warnings are fine.

Note: x11rb's exact import paths can vary slightly between minor versions. If `MOTION_NOTIFY_EVENT` etc. don't resolve from `x11rb::protocol::xproto`, check the actual names with:

```bash
grep -rn 'pub const MOTION_NOTIFY' ~/.cargo/registry/src/index.crates.io-*/x11rb-protocol-*/src/protocol/xproto.rs | head -3
```

and adjust the use paths. Similarly for `xtest_fake_input` — the trait may be `x11rb::protocol::xtest::ConnectionExt`.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-xdaemon/src/input_inject.rs ghostframe-xdaemon/src/main.rs
git commit -m "feat(xdaemon): XTestInjector — X11 input injection via XTEST extension"
```

---

## Task 8: Wire `XTestInjector` into the daemon's startup

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs`

- [ ] **Step 1: Construct after `wait_for_x11`**

Read the current `ghostframe-xdaemon/src/main.rs`. Find where `wait_for_x11(...)` is awaited successfully and where `GhostframeServer::new(config, ":443").await` is called (probably back-to-back).

Insert immediately after the X11 wait, before the server construction:

```rust
    // X server is up; safe to open the XTest injector connection. If the
    // XTEST extension isn't compiled into this xorg-server build, log
    // and continue — frames still stream, just no input.
    let input_injector: Option<
        std::sync::Arc<dyn ghostframe_lib::transport::input_inject::InputInjector>,
    > = match crate::input_inject::XTestInjector::new() {
        Ok(inj) => {
            tracing::info!("XTest input injector ready");
            Some(std::sync::Arc::new(inj))
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "XTest injector unavailable; input forwarding disabled"
            );
            None
        }
    };
```

- [ ] **Step 2: Pass it to `GhostframeServer::new`**

Change the existing call:

```rust
let server = match GhostframeServer::new(config, ":443").await { ... }
```

to:

```rust
let server = match GhostframeServer::new(config, ":443", input_injector).await { ... }
```

- [ ] **Step 3: Build**

```bash
cargo build --release -p ghostframe-xdaemon 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Smoke-run the binary against a running X**

(Optional, only if you have a guest Xorg already running). Just verify the binary doesn't panic at startup:

```bash
RUST_LOG=info DISPLAY=:1 ./target/release/ghostframe-xdaemon --help 2>&1 | head -5
```

(`--help` doesn't trigger real startup; the injector path only fires on real run. The point of this step is just to confirm the binary still links.)

- [ ] **Step 5: Commit**

```bash
git add ghostframe-xdaemon/src/main.rs
git commit -m "feat(xdaemon): construct XTestInjector after wait_for_x11, thread to server"
```

---

## Task 9: web client `keymap.ts` + unit tests

**Files:**
- Create: `ghostframe-web-client/src/input/keymap.ts`
- Create: `ghostframe-web-client/tests/input.test.ts`

- [ ] **Step 1: Create the keymap module**

Create `ghostframe-web-client/src/input/keymap.ts`:

```typescript
// X11 KeySym translation for browser KeyboardEvent.key values.
//
// Two paths:
//   1. Named keys (Enter, ArrowUp, F-keys, modifiers, ...) - lookup table.
//   2. Printable single-codepoint text - X11 keysym-from-Unicode rule:
//      U+0020..U+00FF maps directly; higher BMP ORs with 0x01000000.
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

// Selected from /usr/include/X11/keysymdef.h. Names match
// KeyboardEvent.key, NOT KeyboardEvent.code.
const NAMED_KEYSYMS: Record<string, number> = {
  // Whitespace / control
  Backspace: 0xff08,
  Tab: 0xff09,
  Enter: 0xff0d,
  Escape: 0xff1b,
  Delete: 0xffff,

  // Navigation
  Home: 0xff50,
  End: 0xff57,
  PageUp: 0xff55,
  PageDown: 0xff56,
  ArrowLeft: 0xff51,
  ArrowUp: 0xff52,
  ArrowRight: 0xff53,
  ArrowDown: 0xff54,

  // Editing
  Insert: 0xff63,

  // Function keys
  F1: 0xffbe,
  F2: 0xffbf,
  F3: 0xffc0,
  F4: 0xffc1,
  F5: 0xffc2,
  F6: 0xffc3,
  F7: 0xffc4,
  F8: 0xffc5,
  F9: 0xffc6,
  F10: 0xffc7,
  F11: 0xffc8,
  F12: 0xffc9,

  // Modifiers - map to the *_L variants. KeyboardEvent.key reports
  // "Shift" for both shifts; the server doesn't usually care, but if
  // it does we'd need to look at event.location.
  Shift: 0xffe1,
  Control: 0xffe3,
  Alt: 0xffe9,
  Meta: 0xffeb,
  AltGraph: 0xfe03,
  CapsLock: 0xffe5,
  NumLock: 0xff7f,
  ScrollLock: 0xff14,

  // Misc
  PrintScreen: 0xff61,
  Pause: 0xff13,
  ContextMenu: 0xff67,
  Space: 0x0020, // KeyboardEvent.key for space is " ", but be defensive
};

/** Return the X11 KeySym for a DOM KeyboardEvent, or null if it should
 * not be sent (e.g. dead-key composing state). */
export function keyboardEventToKeysym(event: KeyboardEvent): number | null {
  const k = event.key;

  // Dead keys: the browser will fire a follow-up event with the resolved
  // character. Don't send anything yet.
  if (k === 'Dead' || k === 'Unidentified') return null;

  // Named keys take priority.
  if (k in NAMED_KEYSYMS) return NAMED_KEYSYMS[k];

  // Single character: Unicode → X11 keysym.
  if (k.length === 1) {
    const cp = k.codePointAt(0)!;
    if (cp >= 0x20 && cp <= 0xff) return cp;
    return cp | 0x01000000;
  }

  // Unknown multi-char name we don't have in the table.
  return null;
}
```

- [ ] **Step 2: Create the unit tests**

Create `ghostframe-web-client/tests/input.test.ts`:

```typescript
import { describe, it, expect } from 'vitest';
import { keyboardEventToKeysym } from '../src/input/keymap.js';

function ev(key: string): KeyboardEvent {
  return { key } as unknown as KeyboardEvent;
}

describe('keyboardEventToKeysym', () => {
  it('returns null for dead keys', () => {
    expect(keyboardEventToKeysym(ev('Dead'))).toBeNull();
    expect(keyboardEventToKeysym(ev('Unidentified'))).toBeNull();
  });

  it('maps Enter to XK_Return (0xff0d)', () => {
    expect(keyboardEventToKeysym(ev('Enter'))).toBe(0xff0d);
  });

  it('maps ArrowUp to XK_Up (0xff52)', () => {
    expect(keyboardEventToKeysym(ev('ArrowUp'))).toBe(0xff52);
  });

  it('maps F1 to XK_F1 (0xffbe)', () => {
    expect(keyboardEventToKeysym(ev('F1'))).toBe(0xffbe);
  });

  it("maps printable 'a' to 0x61", () => {
    expect(keyboardEventToKeysym(ev('a'))).toBe(0x61);
  });

  it("maps Latin-1 'ñ' to 0xf1", () => {
    expect(keyboardEventToKeysym(ev('ñ'))).toBe(0xf1);
  });

  it("maps higher BMP '中' to 0x01000000 | codepoint", () => {
    expect(keyboardEventToKeysym(ev('中'))).toBe(0x01000000 | 0x4e2d);
  });

  it('returns null for unknown multi-char names', () => {
    expect(keyboardEventToKeysym(ev('NotARealKey'))).toBeNull();
  });

  it('handles space (KeyboardEvent.key = " ")', () => {
    expect(keyboardEventToKeysym(ev(' '))).toBe(0x20);
  });
});
```

- [ ] **Step 3: Run the tests**

```bash
cd ghostframe-web-client
npm test 2>&1 | tail -8
```

Expected: all green, including the 9 new keymap tests.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/input/keymap.ts ghostframe-web-client/tests/input.test.ts
git commit -m "feat(web-client): input keymap.ts — KeyboardEvent.key → X11 KeySym"
```

---

## Task 10: web client `encode.ts` + tests

**Files:**
- Create: `ghostframe-web-client/src/input/encode.ts`
- Modify: `ghostframe-web-client/tests/input.test.ts`

- [ ] **Step 1: Write failing tests first**

Append to `ghostframe-web-client/tests/input.test.ts`:

```typescript
import {
  encodePointerMove,
  encodePointerButton,
  encodeWheel,
  encodeKeyDown,
  encodeKeyUp,
  INPUT_MSG_TYPE,
} from '../src/input/encode.js';

describe('encodeInput*', () => {
  it('encodePointerMove writes [0x05, 0x01, x:i16, y:i16] (6 bytes)', () => {
    const buf = encodePointerMove(400, -2);
    expect(buf).toEqual(new Uint8Array([0x05, 0x01, 0x01, 0x90, 0xff, 0xfe]));
  });

  it('encodePointerButton writes [0x05, 0x02, x:i16, y:i16, btn, down]', () => {
    const buf = encodePointerButton(5, 10, 1, true);
    expect(buf).toEqual(new Uint8Array([0x05, 0x02, 0, 5, 0, 10, 1, 1]));
  });

  it('encodePointerButton encodes down=false correctly', () => {
    const buf = encodePointerButton(5, 10, 3, false);
    expect(buf).toEqual(new Uint8Array([0x05, 0x02, 0, 5, 0, 10, 3, 0]));
  });

  it('encodeWheel writes [0x05, 0x03, dx:i16, dy:i16]', () => {
    const buf = encodeWheel(0, 1);
    expect(buf).toEqual(new Uint8Array([0x05, 0x03, 0, 0, 0, 1]));
  });

  it('encodeKeyDown writes [0x05, 0x04, keysym:u32 BE]', () => {
    const buf = encodeKeyDown(0xff0d);
    expect(buf).toEqual(new Uint8Array([0x05, 0x04, 0, 0, 0xff, 0x0d]));
  });

  it('encodeKeyUp writes [0x05, 0x05, keysym:u32 BE]', () => {
    const buf = encodeKeyUp(0x61);
    expect(buf).toEqual(new Uint8Array([0x05, 0x05, 0, 0, 0, 0x61]));
  });

  it('encodePointerMove encodes negative y correctly', () => {
    const buf = encodePointerMove(-1, -1);
    expect(buf).toEqual(new Uint8Array([0x05, 0x01, 0xff, 0xff, 0xff, 0xff]));
  });

  it('INPUT_MSG_TYPE is 0x05', () => {
    expect(INPUT_MSG_TYPE).toBe(0x05);
  });
});
```

- [ ] **Step 2: Run tests to verify they fail**

```bash
cd ghostframe-web-client
npm test -- input 2>&1 | tail -8
```

Expected: FAIL — cannot resolve `../src/input/encode.js`.

- [ ] **Step 3: Implement the encoders**

Create `ghostframe-web-client/src/input/encode.ts`:

```typescript
// Wire encoders for INPUT_MSG_TYPE (0x05).
//
// Layout (big-endian, matching FEEDBACK/HELLO/DECODE_ERROR):
//   pointer-move    [0x05][0x01][x:i16][y:i16]                    6 bytes
//   pointer-button  [0x05][0x02][x:i16][y:i16][button:u8][down:u8] 8 bytes
//   wheel           [0x05][0x03][dx:i16][dy:i16]                  6 bytes
//   key-down        [0x05][0x04][keysym:u32]                      6 bytes
//   key-up          [0x05][0x05][keysym:u32]                      6 bytes
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

export const INPUT_MSG_TYPE = 0x05;

const SUB_POINTER_MOVE = 0x01;
const SUB_POINTER_BUTTON = 0x02;
const SUB_WHEEL = 0x03;
const SUB_KEY_DOWN = 0x04;
const SUB_KEY_UP = 0x05;

export function encodePointerMove(x: number, y: number): Uint8Array {
  const buf = new Uint8Array(6);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = SUB_POINTER_MOVE;
  view.setInt16(2, x, false);
  view.setInt16(4, y, false);
  return buf;
}

export function encodePointerButton(
  x: number,
  y: number,
  button: number,
  down: boolean,
): Uint8Array {
  const buf = new Uint8Array(8);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = SUB_POINTER_BUTTON;
  view.setInt16(2, x, false);
  view.setInt16(4, y, false);
  buf[6] = button & 0xff;
  buf[7] = down ? 1 : 0;
  return buf;
}

export function encodeWheel(dx: number, dy: number): Uint8Array {
  const buf = new Uint8Array(6);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = SUB_WHEEL;
  view.setInt16(2, dx, false);
  view.setInt16(4, dy, false);
  return buf;
}

export function encodeKeyDown(keysym: number): Uint8Array {
  return encodeKey(SUB_KEY_DOWN, keysym);
}

export function encodeKeyUp(keysym: number): Uint8Array {
  return encodeKey(SUB_KEY_UP, keysym);
}

function encodeKey(sub: number, keysym: number): Uint8Array {
  const buf = new Uint8Array(6);
  const view = new DataView(buf.buffer);
  buf[0] = INPUT_MSG_TYPE;
  buf[1] = sub;
  view.setUint32(2, keysym >>> 0, false);
  return buf;
}
```

- [ ] **Step 4: Run tests**

```bash
cd ghostframe-web-client
npm test 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/input/encode.ts ghostframe-web-client/tests/input.test.ts
git commit -m "feat(web-client): input encode.ts — five wire encoders + 8 round-trip tests"
```

---

## Task 11: web client `wire.ts` — DOM capture + RAF coalesce + dispatch

**Files:**
- Create: `ghostframe-web-client/src/input/wire.ts`

- [ ] **Step 1: Create the file**

Create `ghostframe-web-client/src/input/wire.ts`:

```typescript
// Browser-side input capture wiring.
//
// `attachInputCapture(canvas, feedbackWriter, getFrameDims)`:
//   - sets tabIndex on the canvas so it can hold keyboard focus
//   - hooks pointer / wheel / keyboard events
//   - coalesces pointermove to 30 Hz via requestAnimationFrame
//   - scales canvas-pixel coords → framebuffer-pixel coords using
//     getFrameDims() (provided by main.ts, sourced from the dimensions
//     datagram)
//   - sends each event over the existing feedback bidi stream
//
// Spec: docs/superpowers/specs/2026-06-13-input-forwarding-design.md

import {
  encodePointerMove,
  encodePointerButton,
  encodeWheel,
  encodeKeyDown,
  encodeKeyUp,
} from './encode.js';
import { keyboardEventToKeysym } from './keymap.js';

/** Caller-provided lookup for the current framebuffer dimensions. The
 * web-client framebuffer module exposes `framebuffer.width/height`. */
export type FrameDimsGetter = () => { width: number; height: number };

/** Browser DOM button index → X11 button number.
 *   DOM 0=left,1=middle,2=right,3=back,4=forward
 *   X11 1=left,2=middle,3=right,8=back,9=forward */
function domButtonToX11(b: number): number | null {
  switch (b) {
    case 0:
      return 1;
    case 1:
      return 2;
    case 2:
      return 3;
    case 3:
      return 8;
    case 4:
      return 9;
    default:
      return null;
  }
}

/** Round-half-toward-zero for fb-pixel quantization. */
function quantize(n: number): number {
  return Math.max(-32768, Math.min(32767, Math.trunc(n)));
}

export function attachInputCapture(
  canvas: HTMLCanvasElement,
  feedbackWriter: WritableStreamDefaultWriter<Uint8Array>,
  getFrameDims: FrameDimsGetter,
): void {
  // Allow keyboard focus.
  canvas.tabIndex = 0;

  // Pending coalesced pointermove (in FB coords).
  let pending: { x: number; y: number } | null = null;
  let rafQueued = false;

  function flushPending(): void {
    rafQueued = false;
    if (!pending) return;
    const { x, y } = pending;
    pending = null;
    void send(encodePointerMove(x, y));
  }

  function schedulePointerMove(x: number, y: number): void {
    pending = { x, y };
    if (!rafQueued) {
      rafQueued = true;
      requestAnimationFrame(flushPending);
    }
  }

  async function send(bytes: Uint8Array): Promise<void> {
    try {
      await feedbackWriter.write(bytes);
    } catch (e) {
      // Feedback stream closed mid-session — same posture as main.ts's
      // HELLO write failure. Don't spam: log once per writer.
      console.warn('input send failed:', e);
    }
  }

  function canvasToFb(clientX: number, clientY: number): { x: number; y: number } {
    const rect = canvas.getBoundingClientRect();
    const cx = clientX - rect.left;
    const cy = clientY - rect.top;
    const cw = rect.width;
    const ch = rect.height;
    const dims = getFrameDims();
    if (cw <= 0 || ch <= 0 || dims.width <= 0 || dims.height <= 0) {
      return { x: 0, y: 0 };
    }
    return {
      x: quantize((cx / cw) * dims.width),
      y: quantize((cy / ch) * dims.height),
    };
  }

  canvas.addEventListener('pointermove', (e) => {
    const { x, y } = canvasToFb(e.clientX, e.clientY);
    schedulePointerMove(x, y);
  });

  canvas.addEventListener('pointerdown', (e) => {
    canvas.focus();
    const btn = domButtonToX11(e.button);
    if (btn === null) return;
    const { x, y } = canvasToFb(e.clientX, e.clientY);
    void send(encodePointerButton(x, y, btn, true));
    e.preventDefault();
  });

  canvas.addEventListener('pointerup', (e) => {
    const btn = domButtonToX11(e.button);
    if (btn === null) return;
    const { x, y } = canvasToFb(e.clientX, e.clientY);
    void send(encodePointerButton(x, y, btn, false));
    e.preventDefault();
  });

  canvas.addEventListener('pointerleave', () => {
    // Marker so the server can hide its cursor while the user is in the
    // browser UI.
    void send(encodePointerMove(-1, -1));
  });

  canvas.addEventListener(
    'wheel',
    (e) => {
      // Per-tick deltas: clamp to ±N and send. Browsers report deltaY in
      // various units depending on deltaMode; we treat any non-zero as
      // one wheel tick for v1.
      const dx = e.deltaX > 0 ? 1 : e.deltaX < 0 ? -1 : 0;
      const dy = e.deltaY > 0 ? 1 : e.deltaY < 0 ? -1 : 0;
      if (dx || dy) {
        void send(encodeWheel(dx, dy));
      }
      e.preventDefault();
    },
    { passive: false },
  );

  canvas.addEventListener('keydown', (e) => {
    const sym = keyboardEventToKeysym(e);
    if (sym !== null) {
      void send(encodeKeyDown(sym));
      e.preventDefault();
    }
  });

  canvas.addEventListener('keyup', (e) => {
    const sym = keyboardEventToKeysym(e);
    if (sym !== null) {
      void send(encodeKeyUp(sym));
      e.preventDefault();
    }
  });
}
```

- [ ] **Step 2: Verify the build**

```bash
cd ghostframe-web-client
npm run build 2>&1 | tail -5
```

Expected: clean. No new dependencies.

- [ ] **Step 3: Run all tests**

```bash
npm test 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/input/wire.ts
git commit -m "feat(web-client): attachInputCapture — DOM listeners + RAF coalesce + dispatch"
```

---

## Task 12: web client `main.ts` — wire up `attachInputCapture`

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`

- [ ] **Step 1: Find the wiring point**

Read `ghostframe-web-client/src/main.ts` around line 317 (where `feedbackWriter` is constructed and the HELLO is sent). The new lines go right AFTER the HELLO write.

- [ ] **Step 2: Wire it up**

Add a new import near the top of `main.ts` (alphabetical with other `./input/*` imports if any, otherwise just below the existing `./feedback` import):

```typescript
import { attachInputCapture } from './input/wire';
```

Then, right after the HELLO `feedbackWriter.write(...)` block (the one that ends with `} catch (e) { console.warn('HELLO write failed:', e); }`), add:

```typescript
  // Browser → server input forwarding. Hooks pointer / wheel / keyboard
  // on the canvas and routes events through the same feedback writer the
  // HELLO above just used. See docs/superpowers/specs/2026-06-13-
  // input-forwarding-design.md.
  if (feedbackWriter) {
    attachInputCapture(canvasEl, feedbackWriter, () => ({
      width: renderer.framebuffer.width,
      height: renderer.framebuffer.height,
    }));
  }
```

(Confirm the variable name `canvasEl` matches what's in scope at that point — main.ts already has a canvas reference from earlier in the function; use whatever name it uses. `renderer.framebuffer` is the `Framebuffer` object exposed by `renderer.ts` and the `width`/`height` fields are already public.)

- [ ] **Step 3: Build the web client**

```bash
cd ghostframe-web-client
npm run build 2>&1 | tail -5
```

Expected: clean.

- [ ] **Step 4: Run tests one more time**

```bash
npm test 2>&1 | tail -5
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-web-client/src/main.ts
git commit -m "feat(web-client): wire attachInputCapture in main.ts"
```

---

## Task 13: end-to-end deploy + manual smoke test

**Files:** No source changes. This task validates the full stack works.

- [ ] **Step 1: Rebuild the Go archive and Rust release binary**

The web client dist is `//go:embed`-ed into `libghostbridge.a`, which is linked into the daemon binary. Both need to be rebuilt:

```bash
cd /home/cedric/work/ghostframe
cd ghostbridge && make archive 2>&1 | tail -3
cd ..
cargo build --release -p ghostframe-xdaemon 2>&1 | tail -3
```

Expected: clean, fresh artifact timestamps.

- [ ] **Step 2: Redeploy**

```bash
sudo ./packaging/install.sh guest --force
sudo systemctl --machine=guest@.host --user restart ghostframe.target
```

Wait ~10 s for the daemon to come up.

- [ ] **Step 3: Open the URL in a browser and verify by journal**

Open the daemon's URL in Chrome (or Firefox). The page should load and start showing the (likely blank) guest desktop.

In another terminal, follow the journal:

```bash
journalctl _UID=1001 -f
```

Then:

1. **Click on the canvas** — should see `XTest input injector ready` once at startup and no error messages.
2. **In the guest session, start xterm** — e.g. via `sudo machinectl shell guest@.host` then `DISPLAY=:1 xterm &` if you want a visible app to verify against.
3. **In the browser, click into the canvas** and **type a few keys**.

Expected behaviour:
- The browser cursor over the canvas drives the guest cursor (visible if you make enlightenment show one).
- Keystrokes show up in xterm.

- [ ] **Step 4: Confirm no input-injection errors in the journal**

```bash
journalctl _UID=1001 --since '5 min ago' | grep -iE 'xtest|inject|keysym' | head
```

Expected: at most an `XTest input injector ready` line. Any `no keycode for keysym` warns are fine if you pressed an exotic key; bad if they fire for every key.

- [ ] **Step 5: Commit a note**

If everything works, mark the manual smoke complete:

```bash
echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) input forwarding manual smoke: PASS" \
  >> docs/superpowers/specs/2026-06-13-input-forwarding-design.md
git add docs/superpowers/specs/2026-06-13-input-forwarding-design.md
git commit -m "docs(specs): mark input-forwarding manual smoke as passed"
```

(Optional but useful — records that the design landed end-to-end.)

---

## Plan self-review

**Spec coverage:**

- D1 (v1 scope: pointer/button/wheel/key) → Tasks 1–12 cover exactly that surface.
- D2 (existing reliable bidi stream) → Task 4 (`INPUT_MSG_TYPE` arm in `dispatch_feedback_bytes`), Task 11 (`feedbackWriter.write`).
- D3 (KeySyms on the wire) → Task 9 (`keymap.ts`), Task 10 (encoders take `keysym: number`).
- D4 (per-event with sub-kind byte) → Task 2 encodes the layout, Task 10 mirrors it.
- D5 (FB-pixel `i16` coords) → Task 11 (`canvasToFb` + `quantize`).
- D6 (`Option<Arc<dyn InputInjector>>`) → Task 4 (field), Task 5 (server.rs threading).
- D7 (sync calls from tokio task) → Task 7 (no `spawn_blocking`).
- D8 (separate X11 connection) → Task 7 (`XTestInjector::new` calls its own `x11rb::connect`).
- D9 (hover-only + click-to-focus + no pointer-lock) → Task 11 (focus on pointerdown, no requestPointerLock).
- D10 (30 Hz RAF coalesce) → Task 11 (`schedulePointerMove` + `requestAnimationFrame`).
- D11 (no modifier reconciliation) → not added in any task.
- D12 (tailnet trust model) → no code; documented in spec.

All Architecture / Components / Data flow / Error handling / Testing sections trace to tasks.

**Placeholder scan:** All code blocks contain real code. Two delegating notes ("Confirm `canvasEl` matches what's in scope" in Task 12, "Verify the field's visibility" in Task 5) hand over a small lookup rather than a guess — the lookup is concrete (one grep / one read of an adjacent line).

**Type consistency:**
- `INPUT_MSG_TYPE: u8 = 0x05` (Rust Task 1) ↔ `INPUT_MSG_TYPE = 0x05` (TS Task 10) — match.
- `InputInjector` method signatures (`pointer_move(&self, x: i16, y: i16)` etc., Task 1) ↔ used identically in Task 3 / Task 4 / Task 7. Match.
- `decode_input_msg` returns `(InputMsg, usize)` (Task 2) ↔ Task 4 destructures `(msg, consumed)`. Match.
- `attachInputCapture(canvas, feedbackWriter, getFrameDims)` (Task 11 signature) ↔ Task 12 call site uses the same shape. Match.
- `FrameDimsGetter = () => { width: number; height: number }` (Task 11) ↔ Task 12 supplies `() => ({ width, height })`. Match.

**Sub-kind constants** are `0x01..0x05` in both Rust (`SUB_POINTER_MOVE` … `SUB_KEY_UP` in Task 1/2) and TS (Task 10). Consistent.
