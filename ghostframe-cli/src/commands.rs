//! `login`, `logout`, and `connect` — the CLI's actual behaviour.

use std::path::PathBuf;

use ghostframe_client_native::{Client, ClientEvent, Config};
use ghostframe_tsnet::{GhostbridgeConfig, GhostbridgeHandle};

use crate::chord::{Chord, ChordAction};
use crate::cli::parse_prefix;
use crate::geometry::{map_pointer, Placement};
use crate::window::{self, Backend, WindowEvent};

/// Errors surfaced by the CLI's subcommands.
///
/// Every variant's `Display` is written for a terminal, not a log: `main`
/// prints it verbatim (`ghostframe: {e}`) and exits non-zero, with no
/// backtrace.
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    #[error("tailnet: {0}")]
    Bridge(#[from] ghostframe_tsnet::GhostbridgeError),
    #[error("client: {0}")]
    Client(#[from] ghostframe_client_native::ClientError),
    #[error("window: {0}")]
    Window(#[from] crate::window::WindowError),
    #[error("{0}")]
    Message(String),
}

/// `$XDG_STATE_HOME/ghostframe/tsnet`, falling back to
/// `$HOME/.local/state/ghostframe/tsnet`.
pub fn state_dir() -> Result<PathBuf, CommandError> {
    if let Ok(xdg) = std::env::var("XDG_STATE_HOME") {
        if !xdg.is_empty() {
            return Ok(PathBuf::from(xdg).join("ghostframe").join("tsnet"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| CommandError::Message("neither XDG_STATE_HOME nor HOME is set".to_string()))?;
    Ok(PathBuf::from(home)
        .join(".local")
        .join("state")
        .join("ghostframe")
        .join("tsnet"))
}

/// A sensible default node name: `ghostframe-<machine hostname>`, or a
/// fixed fallback if the machine's hostname can't be determined.
fn default_hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|s| format!("ghostframe-{s}"))
        .unwrap_or_else(|| "ghostframe-client".to_string())
}

fn print_ips(bridge: &GhostbridgeHandle) -> Result<(), CommandError> {
    let ips = bridge.get_ips()?;
    println!("tailnet IPs:");
    for ip in ips {
        println!("  {ip}");
    }
    Ok(())
}

pub fn login(
    authkey: Option<String>,
    login_server: Option<String>,
    hostname: Option<String>,
) -> Result<(), CommandError> {
    let dir = state_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| CommandError::Message(format!("creating state dir {}: {e}", dir.display())))?;

    let config = GhostbridgeConfig {
        hostname: hostname.unwrap_or_else(default_hostname),
        authkey: authkey.clone().unwrap_or_default(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: login_server.unwrap_or_default(),
    };

    let bridge = GhostbridgeHandle::connect(&config)?;

    if authkey.is_some() {
        bridge.up()?;
        println!("logged in.");
    } else {
        match bridge.login_url()? {
            Some(url) => {
                println!(
                    "To authorise this node, open:\n\n    {url}\n\nWaiting for authorisation..."
                );
                bridge.up()?;
                println!("authorised.");
            }
            None => {
                // `login_url` blocks until the node either gets a URL or
                // reaches Running -- `None` means the latter already
                // happened, so there is nothing to wait for.
                println!("already authorised.");
                print_ips(&bridge)?;
                return Ok(());
            }
        }
    }

    print_ips(&bridge)?;
    Ok(())
}

pub fn logout() -> Result<(), CommandError> {
    let dir = state_dir()?;
    if !dir.join("tailscaled.state").exists() {
        println!("not logged in; nothing to do.");
        return Ok(());
    }

    let config = GhostbridgeConfig {
        hostname: default_hostname(),
        authkey: String::new(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: String::new(),
    };
    let bridge = GhostbridgeHandle::connect(&config)?;

    // logout() BEFORE removing state: it needs the credentials in the
    // state dir to tell the control plane this node is leaving. Deleting
    // state first would strand the node in the tailnet's device list with
    // no way to reach it. And if logout() itself fails, leave the state
    // dir alone -- silently deleting it after a failed logout is the worst
    // outcome, since it discards the user's only means to retry.
    bridge.logout()?;

    std::fs::remove_dir_all(&dir)
        .map_err(|e| CommandError::Message(format!("removing state dir {}: {e}", dir.display())))?;
    println!("logged out.");
    Ok(())
}

/// One input event ready to forward to the remote, already decoded from a
/// `WindowEvent` and (for pointer events) mapped through the current
/// `Placement`. Kept as plain data, rather than calling `client.push_*`
/// directly, so [`route_window_event`] stays pure and testable without a
/// `Client`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputToSend {
    Key {
        keysym: u32,
        down: bool,
    },
    PointerMotion {
        x: i16,
        y: i16,
    },
    PointerButton {
        x: i16,
        y: i16,
        button: u8,
        down: bool,
    },
    Wheel {
        dx: i16,
        dy: i16,
    },
}

/// What the loop should do with one window event. Pure, so the routing is
/// testable without a display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoopAction {
    Forward(InputToSend),
    Quit,
    Minimize,
    Recompute,
    None,
}

/// Decide what a single backend [`WindowEvent`] means for the loop.
///
/// `chord` owns all modifier/prefix bookkeeping -- this function does not
/// re-check modifiers itself, it just dispatches on what `chord.on_key`
/// decides. `placement` maps pointer window-coordinates to remote
/// framebuffer coordinates; `map_pointer`'s clamping guarantee is what
/// keeps pointer events in the black surround well-defined.
pub fn route_window_event(
    ev: &WindowEvent,
    chord: &mut Chord,
    placement: &Placement,
) -> LoopAction {
    match *ev {
        WindowEvent::Key { keysym, down } => match chord.on_key(keysym, down) {
            ChordAction::Forward => LoopAction::Forward(InputToSend::Key { keysym, down }),
            ChordAction::Quit => LoopAction::Quit,
            ChordAction::Minimize => LoopAction::Minimize,
        },
        WindowEvent::PointerMotion { x, y } => {
            let (rx, ry) = map_pointer(placement, x, y);
            LoopAction::Forward(InputToSend::PointerMotion { x: rx, y: ry })
        }
        WindowEvent::PointerButton { x, y, button, down } => {
            let (rx, ry) = map_pointer(placement, x, y);
            LoopAction::Forward(InputToSend::PointerButton {
                x: rx,
                y: ry,
                button,
                down,
            })
        }
        WindowEvent::Wheel { dx, dy } => LoopAction::Forward(InputToSend::Wheel { dx, dy }),
        // The new output size lives in the event, but the loop already has
        // it (it owns `out_w`/`out_h`) by the time it calls this function --
        // returning `Recompute` just tells it to rebuild the `Placement`.
        WindowEvent::Resized { .. } => LoopAction::Recompute,
        WindowEvent::CloseRequested => LoopAction::Quit,
    }
}

pub fn connect(host: String, port: u16, chord_prefix: String) -> Result<(), CommandError> {
    // Argument validation before any I/O or state check: a typo'd
    // --chord-prefix should fail the same way whether or not the caller
    // happens to be logged in.
    let prefix = parse_prefix(&chord_prefix).map_err(CommandError::Message)?;
    let mut chord = Chord::new(prefix);

    let dir = state_dir()?;
    if !dir.join("tailscaled.state").exists() {
        return Err(CommandError::Message(
            "not logged in: run 'ghostframe login' first".to_string(),
        ));
    }

    // Open the window BEFORE constructing the `Client`, even though "connect,
    // then show a window" reads as the natural order. `Config::preferred_modifiers`
    // has to reach `Client::new`/`connect` so the render thread's export ring
    // allocates dmabufs the compositor can actually import (see
    // `Backend::preferred_dmabuf_modifiers`); the only place that list comes
    // from is a backend that is already open.
    let mut backend = window::open("ghostframe")?;
    let preferred_modifiers = backend.preferred_dmabuf_modifiers();

    let config = Config {
        hostname: default_hostname(),
        authkey: String::new(),
        state_dir: dir,
        supports_h264: true,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers,
        debug_map_frames: false,
    };

    let mut client = Client::new(config)?;
    client.connect(&host, port)?;
    println!("connected to {host}:{port}.");

    let result = run_window_loop(
        &mut client,
        backend.as_mut(),
        &mut chord,
        &mut |_| {},
        &mut || false,
    );

    // Always tear the session down, even if the loop errored -- best-effort,
    // since we don't want a teardown failure to hide the loop's own error
    // (which is almost always the more useful one to report).
    if let Err(e) = client.disconnect() {
        tracing::warn!("disconnect after window loop: {e}");
    }

    result
}

/// How long one `poll(2)` call waits for either fd before looping again.
/// Long enough to avoid spinning, short enough that the loop still notices
/// a `Ctrl-C`-adjacent shutdown promptly.
const POLL_TIMEOUT_MS: i32 = 100;

/// Drive the render/event loop until the user quits, the window is closed,
/// `should_quit` says to stop, or the connection drops. See the
/// module-level pseudocode in the M2 plan for the shape this mirrors.
///
/// `pub`, and parameterised by `on_frame_presented`/`should_quit`, so a test
/// can drive it directly against an already-connected `Client` and an
/// already-open `Backend` -- built the same way `connect` builds them, just
/// without `connect`'s login checks or its own tsnet node -- rather than
/// spawning the CLI binary as a subprocess. That matters here because a
/// second `tsnet.Server` per process is known not to converge a working
/// peer datapath (see `ghostframe-e2e/tests/native_client.rs`'s module
/// doc); a test wants to reuse a harness's existing node instead, which
/// only the library path allows. `connect` itself calls this with no-op
/// hooks (`&mut |_| {}`, `&mut || false`): production never quits early and
/// has no use for a frame counter.
///
/// `on_frame_presented` is called with the running count immediately after
/// each successful `backend.present`, which is the one place this loop
/// knows a frame genuinely reached the display server -- a test asserting
/// "a frame was presented" should assert on this, not merely on the
/// process/loop staying alive (that would pass even with a black window).
/// `should_quit` is polled once per iteration, before blocking in `poll`;
/// returning `true` ends the loop the same way `ChordAction::Quit` or
/// `WindowEvent::CloseRequested` would (`Ok(())`), without needing genuine
/// synthetic input -- which a headless Weston has no protocol to deliver in
/// this build (no `virtual-keyboard-unstable-v1` / `wlr-virtual-pointer`,
/// and the headless backend has no real input devices for a compositor-side
/// injection either). The prefix-chord -> quit *routing* itself is already
/// covered without a display, by `ghostframe-cli/tests/event_loop.rs`'s
/// `a_completed_quit_chord_yields_quit` and `close_requested_yields_quit`.
pub fn run_window_loop(
    client: &mut Client,
    backend: &mut dyn Backend,
    chord: &mut Chord,
    on_frame_presented: &mut dyn FnMut(u64),
    should_quit: &mut dyn FnMut() -> bool,
) -> Result<(), CommandError> {
    // The remote's screen size, learned from `ClientEvent::Resized`; `None`
    // until then, since there is nothing sensible to present or map pointer
    // coordinates against before it arrives (guarding here, rather than
    // defaulting to a made-up size, is what keeps the first frames from
    // landing at the wrong offset).
    let mut remote_size: Option<(u32, u32)> = None;
    let (mut out_w, mut out_h) = backend.output_size();
    // A degenerate 0x0 image placement until `remote_size` is known. Every
    // `map_pointer` call against it clamps to (0, 0) -- harmless, since
    // nothing has been presented yet either.
    let mut placement = Placement::centre(0, 0, out_w, out_h);

    let client_fd = client.event_fd();
    let backend_fd = backend.event_fd();

    let mut frames_presented: u64 = 0;

    loop {
        if should_quit() {
            return Ok(());
        }

        let mut fds = [
            libc::pollfd {
                fd: client_fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: backend_fd,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: `fds` points to two valid, initialised `pollfd` entries
        // for the duration of the call; `poll` only ever writes their
        // `revents` fields.
        let rc =
            unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, POLL_TIMEOUT_MS) };
        if rc < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(CommandError::Message(format!("poll: {err}")));
        }
        // A `0` or spurious wakeup just means neither fd had anything; the
        // drains below are no-ops in that case and the loop goes round again.

        while let Some(ev) = client.next_event() {
            match ev {
                ClientEvent::Connected => {
                    tracing::info!("session ready");
                }
                ClientEvent::Resized { width, height } => {
                    remote_size = Some((width, height));
                    placement = Placement::centre(width, height, out_w, out_h);
                }
                ClientEvent::FrameReady { .. } => {
                    if let Some(frame) = client.acquire_frame() {
                        // Release on EVERY path out of this block, success
                        // or failure: the export ring has only 3 buffers,
                        // and a leaked one stalls publishing after exactly
                        // 3 frames with no error anywhere -- the window
                        // just freezes.
                        let presented = remote_size.is_some();
                        let present_result = if presented {
                            backend.present(&frame, &placement)
                        } else {
                            Ok(())
                        };
                        client.release_frame(frame.frame_id);
                        present_result
                            .map_err(|e| CommandError::Message(format!("presenting frame: {e}")))?;
                        if presented {
                            frames_presented += 1;
                            on_frame_presented(frames_presented);
                        }
                    }
                }
                ClientEvent::Disconnected { reason, expected } => {
                    if expected {
                        // Exit 0: being displaced by another client is an
                        // expected outcome, not a failure. A non-zero code
                        // would make an ordinary hand-off look like a crash
                        // to any supervisor or script wrapping this binary.
                        tracing::info!(%reason, "session ended by the server");
                        println!("ghostframe: disconnected — {reason}");
                        return Ok(());
                    }
                    // A genuine transport failure (idle timeout, reset, TLS
                    // failure, ...) is not an ordinary hand-off and must
                    // not report success.
                    return Err(CommandError::Message(format!("disconnected: {reason}")));
                }
                ClientEvent::Error { message } => {
                    return Err(CommandError::Message(message));
                }
            }
        }

        for ev in backend.poll_events()? {
            if let WindowEvent::Resized { width, height } = ev {
                out_w = width;
                out_h = height;
            }
            match route_window_event(&ev, chord, &placement) {
                LoopAction::Forward(input) => match input {
                    InputToSend::Key { keysym, down } => client.push_key(keysym, down),
                    InputToSend::PointerMotion { x, y } => client.push_pointer_motion(x, y),
                    InputToSend::PointerButton { x, y, button, down } => {
                        client.push_pointer_button(x, y, button, down)
                    }
                    InputToSend::Wheel { dx, dy } => client.push_wheel(dx, dy),
                },
                LoopAction::Quit => return Ok(()),
                LoopAction::Minimize => backend.minimize()?,
                LoopAction::Recompute => {
                    if let Some((rw, rh)) = remote_size {
                        placement = Placement::centre(rw, rh, out_w, out_h);
                    }
                }
                LoopAction::None => {}
            }
        }
    }
}
