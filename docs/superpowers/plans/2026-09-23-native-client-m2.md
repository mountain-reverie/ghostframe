# Native Client M2 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the M1 library usable — interactive tailnet login, a `ghostframe` CLI, and a fullscreen showcase window with chorded quit and minimize.

**Architecture:** Two new `ghostbridge` Go exports for interactive login and real logout, wrapped in `ghostframe-tsnet`. `ghostframe-cli` grows `login`/`logout`/`connect` plus a showcase window with a Wayland backend (SCTK) and an X11 backend (x11rb + DRI3), chosen at runtime. The window starts fullscreen and draws the remote image 1:1 and centred.

**Tech Stack:** Go (tailscale v1.94.1), Rust 1.96.1, `clap`, `smithay-client-toolkit` 0.21, `x11rb`, `xkbcommon`.

**Spec:** `docs/superpowers/specs/2026-09-23-native-client-m2-design.md`

**Branch:** `feature/native-client-m2` (already created; the spec commit is its first).

---

## Conventions used in this plan

Pure-logic code (the chord machine, coordinate mapping, CLI parsing) is written out
in full and is meant to be typed in as written. Mechanical protocol setup —
SCTK registry plumbing, DRI3 pixmap creation — gives the exact calls, types and
traps, and marks the body `todo_impl()`. That is a marker, not a function.

---

## Key facts, verified — do not re-derive

1. **`~/.cargo/bin` is NOT on the agent Bash tool's PATH.** Prefix commands with
   `export PATH="$HOME/.cargo/bin:$PATH"`.
2. **Never pipe a build or gate.** A pipe reports the pipe's exit status; redirect
   to a file and echo `$?`.
3. **Never `git add -A`.** Stage explicit paths.
4. **`just containers-build` after ANY server-side change**, and note today's
   rebuilds can fill the disk — `docker system prune -f` is pre-authorised.
5. **tailscale v1.94.1 API** (read from `~/go/pkg/mod/tailscale.com@v1.94.1/`):
   - `tsnet.Server.Up(ctx) -> (*ipnstate.Status, error)` blocks until Running. It
     does **not** return the auth URL.
   - The auth URL is on `ipnstate.Status.AuthURL`
     (`ipn/ipnstate/ipnstate.go:48`), reachable via
     `LocalClient().StatusWithoutPeers(ctx)` (`client/local/local.go:672`).
     tsnet's own `printAuthURLLoop` reads exactly this.
   - `local.Client.Logout(ctx) error` exists (`client/local/local.go:941`).
6. **`PublishedFrame`** carries everything the showcase needs to build a buffer:
   `frame_id, buffer_id, damage, width, height, modifier, fd, planes`.
7. **`Client`** exposes `new/attach_bridge/connect/disconnect/event_fd/next_event/
   acquire_frame/release_frame/push_key/push_pointer_motion/push_pointer_button/
   push_wheel`. `Config` has `hostname, authkey, state_dir, supports_h264,
   indices_raw, n_export_buffers, preferred_modifiers, debug_map_frames`.
8. **Wayland letterboxes for free.** xdg-shell requires a fullscreened surface
   smaller than the output to be centred and black-filled by the compositor. Do
   not add a background surface, subsurface, or viewporter. X11 must clear its own
   surround.
9. **This machine has no desktop session** (`loginctl` → `Type=tty`). The Wayland
   smoke test runs under the harness's `spawn_weston_headless`.

---

## File Structure

### `ghostbridge` (modified)

| File | Change |
|---|---|
| `main.go` | `gbridge_login_url`, `gbridge_logout` |
| `main_test.go` | tests for both (new file if absent) |

### `ghostframe-tsnet` (modified)

| File | Change |
|---|---|
| `src/lib.rs` | `GhostbridgeHandle::login_url()`, `::logout()` |

### `ghostframe-cli`

| File | Responsibility |
|---|---|
| `src/main.rs` | arg parsing, command dispatch, error→exit-code mapping |
| `src/commands.rs` | `login` / `logout` / `connect` bodies |
| `src/chord.rs` | the prefix chord state machine (pure logic) |
| `src/geometry.rs` | centring offset + coordinate mapping (pure logic) |
| `src/window/mod.rs` | `Backend` trait, runtime selection |
| `src/window/wayland.rs` | SCTK backend |
| `src/window/x11.rs` | x11rb + DRI3 backend |
| `tests/chord.rs` | chord table tests (**CI**) |
| `tests/geometry.rs` | mapping/clamping tests (**CI**) |

### `ghostframe-e2e` (modified)

| File | Change |
|---|---|
| `tests/showcase.rs` | Weston-headless smoke test (**not** named in CI) |

---

# Phase A — tailnet lifecycle

## Task 1: `gbridge_login_url` and `gbridge_logout`

**Files:** `ghostbridge/main.go`, `ghostbridge/main_test.go`

- [ ] **Step 1: Write the Go tests first**

In `ghostbridge/main_test.go` (create if absent; follow `web_server_test.go`'s style):

```go
// A login URL cannot be produced for a session that was never created.
func TestLoginURLRejectsUnknownHandle(t *testing.T) {
	var buf [512]C.char
	if rc := gbridge_login_url(C.int32_t(9999), &buf[0], C.size_t(len(buf))); rc >= 0 {
		t.Fatalf("expected failure for unknown handle, got %d", rc)
	}
}

func TestLogoutRejectsUnknownHandle(t *testing.T) {
	if rc := gbridge_logout(C.int32_t(9999)); rc >= 0 {
		t.Fatalf("expected failure for unknown handle, got %d", rc)
	}
}
```

Run: `cd ghostbridge && go test ./...` — expect compile failure (undefined functions).

- [ ] **Step 2: Implement both exports**

```go
//export gbridge_login_url
// Poll the backend until control provides an auth URL, then copy it into the
// caller's buffer.
//
// tsnet's Up() blocks until Running and never returns this URL; tsnet's own
// printAuthURLLoop reads it from StatusWithoutPeers().AuthURL, and so do we.
// An empty AuthURL means the node is already authorised -- that is success with
// an empty string, not an error, so a caller with seeded state is not forced
// through an interactive path it does not need.
func gbridge_login_url(sd C.int32_t, cBuf *C.char, cBufLen C.size_t) C.gbridge_status {
	// 1. lookup(sd); error if absent.
	// 2. lc, err := h.server.LocalClient()
	// 3. poll lc.StatusWithoutPeers(ctx) every ~250ms, up to ~60s, for a
	//    non-empty st.AuthURL OR st.BackendState == "Running".
	// 4. copy into cBuf (truncate safely, always NUL-terminate).
	todo_impl()
}

//export gbridge_logout
// Log the node out of the tailnet.
//
// This is NOT the same as deleting the state directory: that leaves the node
// registered and visible in the tailnet's device list with no way to reach it.
// The caller must log out BEFORE removing local state, because the credentials
// needed to log out live there.
func gbridge_logout(sd C.int32_t) C.gbridge_status {
	// lookup(sd) -> LocalClient() -> Logout(ctx)
	todo_impl()
}
```

Mirror the existing exports' status-code convention (see `gbridge_getips` for the
buffer-copy shape).

- [ ] **Step 3: Tests pass**

```bash
cd ghostframe/ghostbridge && go vet ./... && go test ./...
```

- [ ] **Step 4: Commit**

```bash
git add ghostbridge/main.go ghostbridge/main_test.go
git commit -m "feat(ghostbridge): interactive login URL and real logout

gbridge_new takes an authkey, which covers CI but not a person. login_url
polls StatusWithoutPeers().AuthURL -- the same source tsnet's own
printAuthURLLoop reads -- because Up() blocks until Running and never
returns the URL.

logout calls LocalClient().Logout rather than leaving the caller to delete
the state directory, which would strand the node registered in the
tailnet's device list with no way to reach it."
```

---

## Task 2: Rust wrappers

**Files:** `ghostframe-tsnet/src/lib.rs`

- [ ] **Step 1: Add the declarations and safe wrappers**

```rust
    /// Auth URL for interactive login, or `None` if already authorised.
    ///
    /// Blocks until control provides a URL or the node reaches Running, so a
    /// caller with seeded state gets `None` promptly rather than a timeout.
    pub fn login_url(&self) -> Result<Option<String>, GhostbridgeError> { todo_impl() }

    /// Log out of the tailnet.
    ///
    /// Call this BEFORE deleting the state directory: the credentials needed
    /// to log out live there, and removing state first leaves the node
    /// registered with no way to reach it.
    pub fn logout(&self) -> Result<(), GhostbridgeError> { todo_impl() }
```

Follow `get_ips`'s buffer handling exactly for `login_url`.

- [ ] **Step 2: Build and commit**

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cargo build -p ghostframe-tsnet
git add ghostframe-tsnet/src/lib.rs
git commit -m "feat(tsnet): wrap gbridge_login_url and gbridge_logout"
```

---

# Phase B — pure logic (runs in CI, no display)

## Task 3: The chord state machine

**Files:** `ghostframe-cli/src/chord.rs`, `ghostframe-cli/tests/chord.rs`

This is the piece whose failure mode is "a key I pressed vanished" — miserable
to diagnose interactively, trivial to pin with a table.

- [ ] **Step 1: Write the tests first**

`ghostframe-cli/tests/chord.rs`:

```rust
use ghostframe_cli::chord::{Chord, ChordAction, Prefix};

// X11 keysyms: these are what the wire carries and what xkbcommon yields.
const CTRL_L: u32 = 0xffe3;
const ALT_L: u32 = 0xffe9;
const SUPER_L: u32 = 0xffeb;
const B: u32 = 0x62;
const D: u32 = 0x64;
const H: u32 = 0x68;
const X: u32 = 0x78;

fn ctrl_alt_b() -> Chord { Chord::new(Prefix::CtrlAltB) }

#[test]
fn a_bare_key_is_forwarded_and_does_nothing() {
    let mut c = ctrl_alt_b();
    assert_eq!(c.on_key(X, true), ChordAction::Forward);
    assert_eq!(c.on_key(X, false), ChordAction::Forward);
}

#[test]
fn the_prefix_itself_is_still_forwarded() {
    // The remote must not lose the keystroke: we arm, and forward anyway.
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
}

#[test]
fn prefix_then_d_quits_and_swallows_only_the_d() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
    assert_eq!(c.on_key(D, true), ChordAction::Quit);
}

#[test]
fn prefix_then_h_minimizes() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(H, true), ChordAction::Minimize);
}

#[test]
fn an_unrelated_key_disarms_and_is_forwarded() {
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(X, true), ChordAction::Forward);
    // And the machine is disarmed: a later `d` must NOT quit.
    assert_eq!(c.on_key(D, true), ChordAction::Forward);
}

#[test]
fn b_without_the_modifiers_does_not_arm() {
    let mut c = ctrl_alt_b();
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
    assert_eq!(c.on_key(D, true), ChordAction::Forward);
}

#[test]
fn releasing_the_modifiers_before_the_second_key_still_completes() {
    // Users release Ctrl+Alt before pressing d. If that disarmed us, the
    // chord would be almost impossible to trigger.
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    c.on_key(ALT_L, false);
    c.on_key(CTRL_L, false);
    assert_eq!(c.on_key(D, true), ChordAction::Quit);
}

#[test]
fn key_releases_do_not_complete_the_chord() {
    // Only presses act; otherwise the release of `d` would fire a second time.
    let mut c = ctrl_alt_b();
    c.on_key(CTRL_L, true);
    c.on_key(ALT_L, true);
    c.on_key(B, true);
    assert_eq!(c.on_key(D, false), ChordAction::Forward);
}

#[test]
fn the_super_prefix_variant_works_the_same() {
    let mut c = Chord::new(Prefix::SuperB);
    c.on_key(SUPER_L, true);
    assert_eq!(c.on_key(B, true), ChordAction::Forward);
    assert_eq!(c.on_key(H, true), ChordAction::Minimize);
}
```

Run: `cargo test -p ghostframe-cli --test chord` — expect compile failure.

- [ ] **Step 2: Implement**

```rust
//! The prefix chord: `<prefix>` then `d` (quit) or `h` (minimize).
//!
//! Modelled on tmux's prefix. Every key, INCLUDING the prefix, is forwarded to
//! the remote; only the completing `d`/`h` is swallowed. That keeps input
//! latency and ordering untouched, at the cost of the remote occasionally
//! seeing a stray `b`.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Prefix { CtrlAltB, SuperB }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChordAction {
    /// Send it to the remote.
    Forward,
    /// Swallow it; quit.
    Quit,
    /// Swallow it; minimize.
    Minimize,
}

pub struct Chord { /* prefix, modifier state, armed flag */ }

impl Chord {
    pub fn new(prefix: Prefix) -> Self { todo_impl() }
    pub fn on_key(&mut self, keysym: u32, down: bool) -> ChordAction { todo_impl() }
}
```

Track modifier press/release so `armed` survives the user releasing Ctrl+Alt
before the second key — the test above pins this, and getting it wrong makes the
chord nearly untriggerable in practice.

- [ ] **Step 3: Pass, gate, commit**

```bash
cargo test -p ghostframe-cli --test chord
cargo clippy -p ghostframe-cli --all-targets -- -D warnings
cargo fmt --all -- --check
```

Commit subject: `feat(cli): prefix chord state machine`.

---

## Task 4: Geometry — centring and coordinate mapping

**Files:** `ghostframe-cli/src/geometry.rs`, `ghostframe-cli/tests/geometry.rs`

- [ ] **Step 1: Tests first**

```rust
use ghostframe_cli::geometry::{Placement, map_pointer};

#[test]
fn a_smaller_image_is_centred() {
    let p = Placement::centre(1920, 1080, 2560, 1440);
    assert_eq!((p.origin_x, p.origin_y), (320, 180));
}

#[test]
fn an_exact_match_has_no_offset() {
    let p = Placement::centre(1920, 1080, 1920, 1080);
    assert_eq!((p.origin_x, p.origin_y), (0, 0));
}

#[test]
fn an_odd_surplus_does_not_lose_a_pixel() {
    // 101 - 100 = 1, so one side gets the odd pixel. Whichever side it is,
    // origin + image must still fit inside the output.
    let p = Placement::centre(100, 100, 101, 101);
    assert!(p.origin_x + 100 <= 101);
    assert!(p.origin_y + 100 <= 101);
}

#[test]
fn pointer_inside_the_image_maps_by_offset() {
    let p = Placement::centre(1920, 1080, 2560, 1440);
    assert_eq!(map_pointer(&p, 320, 180), (0, 0));
    assert_eq!(map_pointer(&p, 1000, 700), (680, 520));
}

#[test]
fn pointer_in_the_black_surround_clamps_to_the_edge() {
    // The wire carries i16, so a negative coordinate is a REAL value the
    // server would act on -- clamping is not cosmetic.
    let p = Placement::centre(1920, 1080, 2560, 1440);
    assert_eq!(map_pointer(&p, 0, 0), (0, 0));
    assert_eq!(map_pointer(&p, 2559, 1439), (1919, 1079));
}

#[test]
fn an_image_larger_than_the_output_is_not_given_a_negative_origin() {
    // A remote bigger than the display: pin to 0 and crop rather than
    // producing an origin the mapping would then subtract into nonsense.
    let p = Placement::centre(2560, 1440, 1920, 1080);
    assert_eq!((p.origin_x, p.origin_y), (0, 0));
}
```

- [ ] **Step 2: Implement**

```rust
/// Where the remote image sits inside the window, and how big each is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placement {
    pub origin_x: i32,
    pub origin_y: i32,
    pub image_w: u32,
    pub image_h: u32,
    pub out_w: u32,
    pub out_h: u32,
}

impl Placement {
    /// Centre `image` inside `out`. Never yields a negative origin: an image
    /// larger than the output is pinned to 0 and cropped.
    pub fn centre(image_w: u32, image_h: u32, out_w: u32, out_h: u32) -> Self { todo_impl() }
}

/// Window coordinates -> remote framebuffer coordinates, clamped to the image.
pub fn map_pointer(p: &Placement, win_x: i32, win_y: i32) -> (i16, i16) { todo_impl() }
```

- [ ] **Step 3: Pass, gate, commit** — subject: `feat(cli): fullscreen centring and pointer mapping`.

---

# Phase C — the CLI

## Task 5: Arg parsing and command dispatch

**Files:** `ghostframe-cli/src/main.rs`, `ghostframe-cli/src/commands.rs`, `ghostframe-cli/Cargo.toml`

- [ ] **Step 1: Dependencies**

Add to `ghostframe-cli/Cargo.toml` (M1 deliberately left this crate dependency-free):

```toml
[dependencies]
ghostframe-client-native = { path = "../ghostframe-client-native" }
ghostframe-tsnet = { path = "../ghostframe-tsnet" }
clap = { version = "4", features = ["derive"] }
tracing = { workspace = true }
tracing-subscriber = { workspace = true }
```

- [ ] **Step 2: The CLI surface**

```rust
#[derive(Parser)]
#[command(name = "ghostframe", about = "ghostframe native client")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Join a tailnet.
    Login {
        /// Non-interactive auth key (CI). Without it, an auth URL is printed.
        #[arg(long)] authkey: Option<String>,
        /// Custom control plane.
        #[arg(long)] login_server: Option<String>,
        /// Node name to present.
        #[arg(long)] hostname: Option<String>,
    },
    /// Leave the tailnet and remove local state.
    Logout,
    /// Connect to a server and open a window.
    Connect {
        host: String,
        #[arg(long, default_value_t = 443)] port: u16,
        /// `ctrl-alt-b` (default) or `super-b`.
        #[arg(long, default_value = "ctrl-alt-b")] chord_prefix: String,
    },
}
```

- [ ] **Step 3: Errors are messages, not panics**

```rust
fn main() -> std::process::ExitCode {
    // A CLI that answers "you are not logged in" with a backtrace is
    // user-hostile, and this is the first thing a person touches.
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("ghostframe: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}
```

- [ ] **Step 4: `login` / `logout`**

State dir: `$XDG_STATE_HOME/ghostframe/tsnet`, falling back to
`$HOME/.local/state/ghostframe/tsnet`.

`login`: build a `GhostbridgeConfig`, `connect`, then — with no authkey — call
`login_url()` and print:

```
To authorise this node, open:

    <url>

Waiting for authorisation...
```

then `up()` to block until Running, and print the node's tailnet IPs from
`get_ips()`.

`logout`: **`logout()` first, then remove the state directory.** Removing state
first strands the node registered in the tailnet with no way to reach it. If
`logout()` fails, say so and do NOT delete state — the user can retry.

`connect`: if the state dir has no `tailscaled.state`, exit non-zero with
`not logged in: run 'ghostframe login' first` rather than dialling into a
connection that cannot succeed.

- [ ] **Step 5: Bridge the CLI string to `chord::Prefix`**

The flag is a string; the chord machine takes an enum. One function owns the
mapping, and it **rejects unknown values** rather than silently defaulting —
a typo'd prefix that quietly falls back leaves the user with a chord that
never fires and no indication why.

```rust
/// `"ctrl-alt-b"` / `"super-b"` -> `chord::Prefix`.
pub fn parse_prefix(s: &str) -> Result<chord::Prefix, String> {
    match s {
        "ctrl-alt-b" => Ok(chord::Prefix::CtrlAltB),
        "super-b" => Ok(chord::Prefix::SuperB),
        other => Err(format!(
            "unknown --chord-prefix {other:?}; expected ctrl-alt-b or super-b"
        )),
    }
}
```

- [ ] **Step 6: Unit-test parsing**

```rust
    #[test]
    fn connect_defaults_to_443_and_the_safe_prefix() {
        let c = Cli::parse_from(["ghostframe", "connect", "host"]);
        match c.cmd {
            Command::Connect { port, chord_prefix, .. } => {
                assert_eq!(port, 443);
                assert_eq!(chord_prefix, "ctrl-alt-b");
            }
            _ => panic!("wrong subcommand"),
        }
    }

    #[test]
    fn an_unknown_chord_prefix_is_rejected_at_parse_time() {
        // Better a clear parse error than a window whose chord silently
        // never fires.
        assert!(parse_prefix("meta-q").is_err());
    }
```

- [ ] **Step 7: Gate and commit** — subject: `feat(cli): login, logout and connect`.

---

# Phase D — the window

## Task 6: Backend trait and runtime selection

**Files:** `ghostframe-cli/src/window/mod.rs`

- [ ] **Step 1: Define the seam**

```rust
/// What the showcase needs from a display server.
pub trait Backend {
    /// Present `frame` centred per `placement`. The dmabuf fd and plane
    /// layout come straight from the library; cache any per-`buffer_id`
    /// import, since the library recycles a small fixed set.
    fn present(&mut self, frame: &PublishedFrame, placement: &Placement) -> Result<()>;
    /// Drain input and window events since the last call.
    fn poll_events(&mut self) -> Result<Vec<WindowEvent>>;
    fn minimize(&mut self) -> Result<()>;
    fn output_size(&self) -> (u32, u32);
}

pub enum WindowEvent {
    Key { keysym: u32, down: bool },
    PointerMotion { x: i32, y: i32 },
    PointerButton { x: i32, y: i32, button: u8, down: bool },
    Wheel { dx: i16, dy: i16 },
    Resized { width: u32, height: u32 },
    CloseRequested,
}

/// Wayland if `WAYLAND_DISPLAY` is set, else X11 if `DISPLAY` is, else error.
pub fn open(title: &str) -> Result<Box<dyn Backend>> { todo_impl() }
```

Emit a clear error naming both variables when neither is set — "no display" is
otherwise a confusing failure for a tool you just logged in with.

- [ ] **Step 2: Commit** — subject: `feat(cli): window backend seam`.

## Task 7: Wayland backend

**Files:** `ghostframe-cli/src/window/wayland.rs`

- [ ] **Step 1: Implement with SCTK 0.21**

Modules: `registry`, `shell::xdg::window`, `seat::keyboard` (feature
`xkbcommon`), `dmabuf`.

```rust
// Fullscreen BEFORE the first commit, so the compositor sizes us correctly
// from the start rather than mapping a window we then resize.
window.set_fullscreen(None);
```

**Do not composite a background.** xdg-shell requires a fullscreened surface
smaller than the output to be centred and black-filled by the compositor, which
is exactly the presentation we want. Adding a subsurface or viewporter would
duplicate work the compositor already does and introduce a second source of
truth for the offset.

Build one `wl_buffer` per `buffer_id` via `DmabufParams` (`add` per plane with
`frame.modifier`, then `create_immed`) and **cache it** — the library recycles a
small fixed set and re-importing per frame is pure waste.

Per frame: `attach`, one `damage_buffer` per reported rect, `commit`.

Feed `DmabufFeedback`'s preferred modifiers back into
`Config::preferred_modifiers` so the compositor gets a tiling it can import.

Keyboard: SCTK yields `xkeysym::Keysym`; `.raw()` is the X11 keysym the wire
carries, so no translation table.

- [ ] **Step 2: Gate and commit** — subject: `feat(cli): Wayland showcase backend`.

## Task 8: X11 backend

**Files:** `ghostframe-cli/src/window/x11.rs`

- [ ] **Step 1: Implement with x11rb**

- Fullscreen via `_NET_WM_STATE_FULLSCREEN` (`change_property` before map, or a
  client message after).
- `dri3::pixmap_from_buffers` with `frame.modifier` and the plane layout; cache
  one `Pixmap` per `buffer_id`.
- `present::pixmap` with the damage region, targeting the centred rectangle.
- **Clear the surround yourself** on configure and on resolution change — X11
  gives no letterboxing guarantee. Not per frame.
- Keycode → keysym via `xkbcommon::xkb::x11`.

- [ ] **Step 2: Gate and commit** — subject: `feat(cli): X11 showcase backend`.

## Task 9: Wire the window into `connect`

**Files:** `ghostframe-cli/src/commands.rs`

- [ ] **Step 1: The loop**

```
poll(client.event_fd(), backend fd, timeout)
  drain client.next_event():
      Resized  -> recompute Placement
      FrameReady -> acquire_frame, backend.present, release_frame
      Disconnected/Error -> print and exit non-zero
  drain backend.poll_events():
      Key -> chord.on_key(); Forward -> client.push_key();
             Quit -> break; Minimize -> backend.minimize()
      Pointer/Wheel -> map through Placement, then client.push_*
      Resized -> recompute Placement
      CloseRequested -> break
```

**Release every frame you acquire**, including on the error path — the ring has
three buffers, and leaking them stalls publishing after three frames with no
error message.

- [ ] **Step 2: Gate and commit** — subject: `feat(cli): drive the showcase window`.

---

# Phase E — verification

## Task 10: Weston-headless smoke test

**Files:** `ghostframe-e2e/tests/showcase.rs`

- [ ] **Step 1: The test**

Use `spawn_weston_headless()` and `setup_e2e_server` (`webgpu: false`, `gpu:
false`). Launch the `ghostframe` binary against the server with
`WAYLAND_DISPLAY` pointed at Weston, assert it reports a presented frame, then
drive the quit chord and assert a clean exit.

Do **not** name this target in any CI workflow — it needs Docker, a GPU and
Weston. Add a row to `ghostframe-client-gpu/README.md`'s table.

- [ ] **Step 2: Commit** — subject: `test(e2e): showcase smoke under Weston headless`.

## Task 11: Measure frame pacing, then decide on the fence

**Files:** measurement only; implementation conditional.

- [ ] **Step 1: Instrument**

Record published-frame intervals in the `connect` loop behind
`--log-frame-pacing`, and report p50/p99 over a 60-second 1080p session.

- [ ] **Step 2: Decide, and write the number down**

- **Clean at 60 Hz** → record the measurement in the spec and stop. M2 ends here.
- **Visible stalls** → implement the fence:
  `wgpu_hal::vulkan::Queue::add_signal_semaphore` on the blit submission in
  `ExportRing::publish`, exported with `vkGetSemaphoreFdKHR`, requiring
  `VK_KHR_external_semaphore_fd` on the device; populate
  `PublishedFrame`/`gf_frame.acquire_fence_fd`; have the Wayland backend pass it
  as the `wl_buffer`'s acquire fence.

Either way the measurement goes in the spec, so the next person inherits a number
rather than an opinion.

---

## Definition of done for M2

- [ ] `ghostframe login` prints an auth URL and completes against a real tailnet
- [ ] `ghostframe logout` removes the node from the tailnet device list
- [ ] `ghostframe connect <host>` opens fullscreen and shows the remote screen
- [ ] Chord: prefix+`d` quits, prefix+`h` minimizes, everything else forwards
- [ ] Wayland smoke test passes under Weston headless
- [ ] X11 backend manually verified, with the result written down
- [ ] Frame pacing measured; fence work done or explicitly deferred with the number
- [ ] `just ci-local` green; no `#[ignore]` in new CI-safe targets
