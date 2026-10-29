# ghostframe-server X11 Rework Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Rename `ghostframe-xdaemon` to `ghostframe-server`, a rootless per-user binary with `install`, `uninstall`, `login`, `logout` and `capture` commands; reduce capture to one X11 method (MIT-SHM full-root grab with a `GetImage` fallback and the XFixes cursor blended in); make server-initiated RandR changes look to the window manager like `xrandr --output O --mode M`; delete DRM capture, the per-window composite path, the root installer and the `--drm-direct` e2e workaround.

**Architecture:** `main.rs` becomes clap dispatch into `commands/{capture,login,install}.rs`. Capture sits behind a `capture::Backend` trait (frames, damage, `InputInjector`, `DisplayController`), implemented by `capture::x11::X11Backend` over `capture/x11/{frames,damage,input,display,cvt}.rs`. A `state::StateLock` (`flock` on `<state_dir>/.lock`) serialises `capture`, `login` and `logout`. `install` writes an embedded systemd user unit and XDG autostart entry under `$HOME`, migrates the old units, and only reports root-owned leftovers. The e2e test patterns paint into Xorg through a new `ghostframe-test-pattern --x11-sink` instead of owning DRM master, so every e2e test exercises the production capture path. `ghostframe-lib` is untouched; its dma-buf ingest is kept unused (spec section 5).

**Tech Stack:** Rust 1.96.1 (pinned), x11rb 0.13.2 (`shm`, `randr`, `xfixes`, `damage`, `xtest`), clap 4.6.7 (workspace), libc (memfd/mmap/flock/gethostname/getpwuid), tempfile 3 (tests), ghostbridge/tsnet via `ghostframe_lib::transport::ghostbridge::GhostbridgeHandle`, Xvfb + xrandr for tests, Docker e2e harness.

**Spec:** `docs/superpowers/specs/2026-10-29-ghostframe-server-x11-design.md`.

---

## How this plan was checked

Every Rust, shell and template block below was cut from a scratch copy of
`feature/ghostframe-server-x11` (at `0ec05fb5`) where the tasks were applied
in order and, per task, `cargo clippy --all-targets` showed no warnings in the
touched crates and the unit tests passed (X11 tests on private Xvfb servers).
The e2e suite, `just ci-local` and the live steps were **not** run; they are
Tasks 15 and 16.

## Ground rules for every task

- **Never `git add -A` or `git add .`.** Each commit step names its paths; run `git status --short` first and stage only those.
- **Lint with `just lint`** (workspace clippy, the CI gate). `cargo clippy -p ghostframe-server -- -D warnings` fails on `ghostframe-lib` dead code under that package's feature subset; that failure predates this plan and is not yours.
- **X11 unit tests run under `xvfb-run`**, as CI does:
  `xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins`.
  The new tests start their own private Xvfb (`capture::x11::test_support::PrivateXvfb`) and never touch `$DISPLAY`; `xvfb-run` is for the pre-existing `damage.rs` tests.
- **Build the web bundle once before the first server build:** `just build-web` (ghostbridge embeds `ghostframe-web-client/dist/`).
- **GPU work is serialised.** Anything that runs `ghostframe-lib` tests or the e2e suite goes through `../gf-gpu-run` (the GPU lock wrapper next to the repository) with `RUST_TEST_THREADS=1`, and Docker work additionally under `flock -o ../.gf-heavy.lock`. `ghostframe-server`'s and `ghostframe-test-pattern`'s own tests touch no GPU and need no lock.
- **Notes directory** for logs this plan asks you to keep: `target/gf-x11-notes/` (git-ignored).
- **Commit trailer:** every commit message ends with these two lines:

  ```
  Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N
  ```

## File structure

Created:

| Path | Responsibility |
|---|---|
| `ghostframe-server/src/capture/mod.rs` | `Backend` trait: frames, damage, input injector, display controller |
| `ghostframe-server/src/capture/x11/mod.rs` | `X11Backend`, `wait_for_x11` |
| `ghostframe-server/src/capture/x11/frames.rs` | (from `x11_capture.rs`) MIT-SHM / `GetImage(root)` grab, cursor blend, geometry poll |
| `ghostframe-server/src/capture/x11/randr_events.rs` | Records RandR/root events; shared by tests and `randr_probe` |
| `ghostframe-server/src/capture/x11/test_support.rs` | `PrivateXvfb` for tests |
| `ghostframe-server/src/cli.rs` | clap `Cli`/`Command`, `MaxResolution` |
| `ghostframe-server/src/commands/{mod,capture,login,install}.rs` | One module per command |
| `ghostframe-server/src/state.rs` | `state_dir_seeded`, `StateLock` |
| `ghostframe-server/templates/ghostframe-server.service.in` | Embedded unit template |
| `ghostframe-server/templates/ghostframe-server.desktop` | Embedded autostart entry |
| `ghostframe-server/examples/randr_probe.rs` | RandR investigation tool (Task 1) |
| `ghostframe-server/tests/commands.rs` | CLI contract tests (replaces `tests/init_flag.rs`) |
| `ghostframe-test-pattern/src/x11_sink.rs` | `Scanout`: paint a BGRX buffer into root via (Shm)PutImage |
| `ghostframe-test-pattern/src/cycle.rs` | (from `drm_direct.rs`) mode-switch paint loop on the sink |
| `ghostframe-test-pattern/src/text_grid_sink.rs` | (from `text_grid_drm.rs`) pixel-exact text grid on the sink |

Moved (`git mv`): `ghostframe-xdaemon/` → `ghostframe-server/`; `src/{xdamage,input_inject,display,cvt}.rs` → `src/capture/x11/{damage,input,display,cvt}.rs`.

Deleted: `ghostframe-server/src/drm_capture.rs`, `tests/drm_gpu_pipeline.rs`, `tests/init_flag.rs`, `examples/writeback_probe.rs`; `ghostframe-test-pattern/src/{drm_direct,text_grid_drm}.rs` (by move); `packaging/` (all); `tests/packaging/mode_switch_test.sh`.

Modified outside the crates: `Cargo.toml`, `Cargo.lock`, `Justfile`, `.github/workflows/{ci,e2e}.yml`, `.github/workflows/_setup-system-deps/packages-build.txt`, `ci/check-{env-reads,release-binary,release-features,test-features}.sh`, `ci/skip-list.txt`, `tests/containers/test-server/{Dockerfile,entrypoint.sh,xorg-vkms.conf}`, the e2e harness and tests (`--drm-direct` → `--x11-sink`, process name), `README.md`, `DEVELOPERS.md`, `AGENTS.md`, `docs/ci.md`, `docs/m0-manual-smoke-test.md`, comments in `ghostframe-lib`/client crates. Not touched: `docs/superpowers/` (except this plan's notes and the spec's section 7 finding), `docs/reviews/`, `docs/specs/`, `docs/design/`.

## Decisions taken while planning (spec ambiguities)

1. **`capture` with `TS_AUTHKEY` but no state** joins non-interactively instead of exiting 2. The spec says "exits with code 2 ... if the state is not seeded"; the e2e image starts every container unseeded with `TS_AUTHKEY`, and an auth key is exactly the non-interactive path `login` also takes. Without a key, an unseeded `capture` exits 2 as specified.
2. **`Backend` exposes `Option<Arc<dyn InputInjector>>` and `Option<Arc<dyn DisplayController>>`.** The spec writes `Arc<dyn InputInjector>` and `Option<Box<dyn DisplayController>>`; `GhostframeServer::new` takes `Option<Arc<..>>` for both, and the spec says the library interfaces do not change.
3. **ghostbridge needs no Go change.** `gbridge_login_url` / `gbridge_logout` (`ghostbridge/main.go`) and `GhostbridgeHandle::login_url` / `logout` (`ghostframe-tsnet/src/lib.rs`) already exist and are used by the native client's `ghostframe login/logout`. `login` polls the URL and then calls `up()`, which blocks until the backend is Running; `logout` calls `Logout` and drops the handle (`gbridge_close`). Amended in Task 11 review: a `quiet` flag was added to `GhostbridgeConfig` and `gbridge_new` (tsnet `Logf`/`UserLogf` discarded) so `login`/`logout` can show the sign-in URL without backend noise; capture and the native client keep it off.
4. **`login --hostname/--control-url` are not persisted.** The unit does not set them, so `login` prints the exact `systemctl --user edit` line when they differ from what `capture` will use. Both default from the same place (`TS_HOSTNAME`, else `<host>-ghostframe`), so the default path needs nothing.
5. **The LightDM autologin drop-in is not a "leftover".** It is what step 7 asks for, so `install` reports it as present; only the getty autologin drop-in and `/etc/X11/ghostframe-headless.conf` are listed for removal.
6. **`xorg-vkms.conf` is kept.** It has consumers besides the DRM-direct path: `ghostframe-e2e/src/harness/e2e_setup.rs` (every `gpu: true` setup) and `ghostframe-e2e/src/harness/scene.rs` set `XORG_CONF=/etc/X11/xorg-vkms.conf`. Only the entrypoint's auto-detection, which no caller relies on, is removed. VKMS therefore stays a host requirement of the GPU e2e setups for now; switching them to the dummy driver is a follow-up with its own e2e run.
7. **Order differs from the suggested phasing:** the test-pattern X11 sink (Task 4) lands before DRM capture is deleted (Task 5) and before the `Backend` refactor (Task 6), so no throwaway DRM adapter is written for the trait, and the e2e suite keeps a working capture path at every commit.
8. **`GHOSTFRAME_DRM_MODE` keeps its name.** Tests and scenes set it; the entrypoint now applies it to Xorg with `xrandr` for `--x11-sink` (default `1024x768`, the VKMS preferred mode the DRM patterns painted at), so "tests change `--drm-direct` to the X11 sink and nothing else" holds.
9. **Tests that set `CAPTURE_FPS_DRM_DIRECT` now set `CAPTURE_FPS`** with the same value; `--x11-sink` scenes default to 30 fps (the old DRM-direct default), others to 2. Task 15 re-runs each of those tests.

---

### Task 1: RandR investigation tool, and the recording on Enlightenment

Spec section 4 makes this the first task: the mode-reuse and request-order hypotheses are unproven until the RandR event stream of `xrandr` and of `XrandrDisplay` have been recorded on Enlightenment. The tool is built against the **current** `display.rs`, so the recording shows today's behaviour.

What is already known (recorded on Xvfb while planning, `xrandr 1.5`): `xrandr --verbose --output screen --mode M` prints `crtc 0: disable`, then `screen 0: WxH ...`, then `crtc 0: M ...`, for a shrink **and** for a grow. Today's `XrandrDisplay` never disables the CRTC; on a shrink it switches the CRTC while the screen is still large, so a window manager sees `crtc 1000x700` under a `screen 1280x1024` it then has to reconcile. Evangeline's `xrandr --current` also shows `panning 1920x1080+0+0` on `HDMI-A-1`, which the recording must cover.

**Files:**
- Create: `ghostframe-xdaemon/src/randr_events.rs` (not declared in `main.rs` yet; only the example includes it)
- Create: `ghostframe-xdaemon/examples/randr_probe.rs`
- Modify (notes only, after the live run): this plan file, section "Task 1 findings" at the end

- [ ] **Step 1: Write the event recorder**

Create `ghostframe-xdaemon/src/randr_events.rs`:

````rust
//! Records the RandR notifications a window manager receives.
//!
//! A window manager learns about a mode change only through these events, so
//! "indistinguishable from `xrandr`" (ghostframe-server design, section 4) is
//! checked by recording them: once around `xrandr --output O --mode M`, once
//! around `XrandrDisplay::set_output`, and comparing.
//!
//! Shared by the `randr_probe` example (the live investigation on a real
//! desktop) and the display tests (on a private Xvfb).

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// One notification, reduced to the fields a window manager acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandrEvent {
    Screen {
        width: u16,
        height: u16,
        mm_width: u16,
        mm_height: u16,
    },
    Crtc {
        crtc: u32,
        mode: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    },
    Output {
        output: u32,
        crtc: u32,
        mode: u32,
    },
    OutputProperty {
        output: u32,
        atom: u32,
    },
    RootConfigure {
        width: u16,
        height: u16,
    },
}

/// A second X client that listens to root's RandR and structure events.
pub struct Recorder {
    conn: RustConnection,
}

impl Recorder {
    /// Connect to `display` (`None` = `$DISPLAY`) and subscribe.
    pub fn connect(display: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let root = conn.setup().roots[screen_num].root;
        // RandR 1.2+ notifications are only sent to a client that negotiated
        // the version first.
        conn.randr_query_version(1, 5)?.reply()?;
        conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE
                | randr::NotifyMask::OUTPUT_PROPERTY,
        )?
        .check()?;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        Ok(Self { conn })
    }

    /// Collect events until `quiet` passes without one, or `max` elapses.
    /// Each event carries its offset from the call.
    pub fn collect(
        &self,
        quiet: Duration,
        max: Duration,
    ) -> Result<Vec<(Duration, RandrEvent)>, Box<dyn std::error::Error>> {
        let start = Instant::now();
        let mut last = start;
        let mut out = Vec::new();
        loop {
            while let Some(ev) = self.conn.poll_for_event()? {
                last = Instant::now();
                if let Some(e) = classify(&ev) {
                    out.push((last.duration_since(start), e));
                }
            }
            let now = Instant::now();
            if now.duration_since(last) >= quiet || now.duration_since(start) >= max {
                return Ok(out);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The events this module records; everything else is `None`.
pub fn classify(ev: &Event) -> Option<RandrEvent> {
    match ev {
        Event::RandrScreenChangeNotify(e) => Some(RandrEvent::Screen {
            width: e.width,
            height: e.height,
            mm_width: e.mwidth,
            mm_height: e.mheight,
        }),
        Event::RandrNotify(e) if e.sub_code == randr::Notify::CRTC_CHANGE => {
            let c = e.u.as_cc();
            Some(RandrEvent::Crtc {
                crtc: c.crtc,
                mode: c.mode,
                x: c.x,
                y: c.y,
                width: c.width,
                height: c.height,
            })
        }
        Event::RandrNotify(e) if e.sub_code == randr::Notify::OUTPUT_CHANGE => {
            let o = e.u.as_oc();
            Some(RandrEvent::Output {
                output: o.output,
                crtc: o.crtc,
                mode: o.mode,
            })
        }
        Event::RandrNotify(e) if e.sub_code == randr::Notify::OUTPUT_PROPERTY => {
            let p = e.u.as_op();
            Some(RandrEvent::OutputProperty {
                output: p.output,
                atom: p.atom,
            })
        }
        Event::ConfigureNotify(e) => Some(RandrEvent::RootConfigure {
            width: e.width,
            height: e.height,
        }),
        _ => None,
    }
}

/// The event stream with ids and timing dropped, one line per event: the
/// form two recordings are compared in.
pub fn shape(events: &[(Duration, RandrEvent)]) -> Vec<String> {
    events
        .iter()
        .map(|(_, e)| match *e {
            RandrEvent::Screen { width, height, .. } => format!("screen {width}x{height}"),
            RandrEvent::Crtc { mode: 0, .. } => "crtc off".to_string(),
            RandrEvent::Crtc { width, height, .. } => format!("crtc {width}x{height}"),
            RandrEvent::Output { mode: 0, .. } => "output off".to_string(),
            RandrEvent::Output { .. } => "output on".to_string(),
            RandrEvent::OutputProperty { .. } => "output property".to_string(),
            RandrEvent::RootConfigure { width, height } => format!("root {width}x{height}"),
        })
        .collect()
}
````

- [ ] **Step 2: Write the probe example**

Create `ghostframe-xdaemon/examples/randr_probe.rs`:

````rust
//! RandR investigation tool for the ghostframe-server X11 rework (design
//! section 4: "record the RandR event stream for `xrandr` and for
//! `XrandrDisplay` and close the difference").
//!
//! ```text
//! randr_probe state            # screen, outputs, modes, CRTC, panning
//! randr_probe record SECS      # print every RandR/root event for SECS seconds
//! randr_probe apply W H        # XrandrDisplay::set_output(W, H, 1000)
//! ```
//!
//! `apply` CHANGES THE PHYSICAL MONITOR MODE of `$DISPLAY`. On a live desktop
//! run it only with the owner's go-ahead, and restore with
//! `xrandr --output <OUT> --mode <WxH>` afterwards.
//!
//! An example, not a `src/bin/*`, for the reason `cvt_sweep.rs` gives:
//! `cargo build -p` (the e2e image) does not build examples, while
//! `clippy --all-targets` still lints them. It includes the shipped modules
//! verbatim via `#[path]`, so it always exercises the code that ships.

#[path = "../src/cvt.rs"]
mod cvt;
#[path = "../src/display.rs"]
mod display;
#[path = "../src/randr_events.rs"]
mod randr_events;

use std::time::Duration;

use ghostframe_lib::transport::display::DisplayController;
use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::ConnectionExt as _;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["state"] => state(),
        ["record", secs] => record(secs.parse()?),
        ["apply", w, h] => apply(w.parse()?, h.parse()?),
        _ => {
            eprintln!("usage: randr_probe state | record SECS | apply W H");
            std::process::exit(2);
        }
    }
}

fn mode_name(res: &randr::GetScreenResourcesReply, id: randr::Mode) -> String {
    let mut off = 0usize;
    for m in &res.modes {
        let end = off + usize::from(m.name_len);
        if m.id == id {
            return String::from_utf8_lossy(&res.names[off..end]).into_owned();
        }
        off = end;
    }
    format!("#{id}")
}

fn state() -> Result<()> {
    let (conn, n) = x11rb::connect(None)?;
    let root = conn.setup().roots[n].root;
    conn.randr_query_version(1, 5)?.reply()?;
    let geom = conn.get_geometry(root)?.reply()?;
    let range = conn.randr_get_screen_size_range(root)?.reply()?;
    println!(
        "screen {}x{} (range {}x{}..{}x{})",
        geom.width,
        geom.height,
        range.min_width,
        range.min_height,
        range.max_width,
        range.max_height
    );
    let res = conn.randr_get_screen_resources(root)?.reply()?;
    for &output in &res.outputs {
        let info = conn
            .randr_get_output_info(output, res.config_timestamp)?
            .reply()?;
        println!(
            "output {} ({}) {:?} crtc={} {}x{}mm preferred={}",
            String::from_utf8_lossy(&info.name),
            output,
            info.connection,
            info.crtc,
            info.mm_width,
            info.mm_height,
            info.num_preferred
        );
        for (i, &m) in info.modes.iter().enumerate() {
            let mi = res.modes.iter().find(|x| x.id == m);
            if let Some(mi) = mi {
                let total = u64::from(mi.htotal) * u64::from(mi.vtotal);
                let hz = (u64::from(mi.dot_clock) * 100)
                    .checked_div(total)
                    .unwrap_or(0);
                println!(
                    "  mode {} ({m}) {}x{} {}.{:02}Hz{}",
                    mode_name(&res, m),
                    mi.width,
                    mi.height,
                    hz / 100,
                    hz % 100,
                    if i < usize::from(info.num_preferred) {
                        " preferred"
                    } else {
                        ""
                    }
                );
            }
        }
        if info.crtc != 0 {
            let c = conn
                .randr_get_crtc_info(info.crtc, res.config_timestamp)?
                .reply()?;
            println!(
                "  crtc {} mode {} ({}) {}x{}+{}+{} rotation {:?}",
                info.crtc,
                mode_name(&res, c.mode),
                c.mode,
                c.width,
                c.height,
                c.x,
                c.y,
                c.rotation
            );
            let p = conn.randr_get_panning(info.crtc)?.reply()?;
            println!(
                "  panning {}x{}+{}+{} track {}x{}+{}+{} border {},{},{},{}",
                p.width,
                p.height,
                p.left,
                p.top,
                p.track_width,
                p.track_height,
                p.track_left,
                p.track_top,
                p.border_left,
                p.border_top,
                p.border_right,
                p.border_bottom
            );
        }
    }
    Ok(())
}

fn record(secs: u64) -> Result<()> {
    let rec = randr_events::Recorder::connect(None)?;
    eprintln!("recording for {secs}s");
    let window = Duration::from_secs(secs);
    let events = rec.collect(window, window)?;
    for (at, e) in &events {
        println!("{:>8.1}ms {e:?}", at.as_secs_f64() * 1e3);
    }
    println!("--- shape");
    for line in randr_events::shape(&events) {
        println!("{line}");
    }
    Ok(())
}

fn apply(w: u16, h: u16) -> Result<()> {
    let ctl = display::XrandrDisplay::new(None).map_err(|e| e.to_string())?;
    ctl.set_output(w, h, 1000).map_err(|e| e.to_string())?;
    println!("set_output({w}, {h}, 1000) returned Ok");
    Ok(())
}
````

- [ ] **Step 3: Lint it**

Run: `just lint`
Expected: exit 0. (`clippy.toml` forbids `Instant::elapsed`; the recorder uses `duration_since` for that reason.)

- [ ] **Step 4: Smoke-test it on a throwaway Xvfb (no live display)**

```bash
cargo build -p ghostframe-xdaemon --example randr_probe
P=target/debug/examples/randr_probe
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" sh -c \
  "$P state; ($P record 2 &); sleep 0.5; $P apply 1000 700; sleep 2.5; $P state | head -1"
```

Expected: a `screen 1280x1024 ...` line, an `output screen ...` block with a `crtc ...` and a `panning 0x0+0+0 ...` line, then (after `set_output(1000, 700, 1000) returned Ok`) the recording; its `--- shape` section is exactly

```
screen 1280x1024
output on
root 1280x1024
screen 1280x1024
crtc 1000x700
root 1280x1024
screen 1000x700
root 1000x700
```

and the last line is `screen 1000x700 (range 1x1..1280x1024)`. That `crtc 1000x700` before any `screen 1000x700` is today's order.

- [ ] **Step 5: Commit the tool**

```bash
git status --short
git add ghostframe-xdaemon/src/randr_events.rs ghostframe-xdaemon/examples/randr_probe.rs
git commit -m "feat(xdaemon): randr_probe example records the RandR event stream

Spec section 4's first task: compare what a window manager sees for
\`xrandr --output O --mode M\` and for XrandrDisplay::set_output.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

- [ ] **Step 6: STOP — get the owner's go-ahead for the live run**

The next step **changes the physical monitor mode** of the live Enlightenment session on evangeline (`DISPLAY=:0`, user `cedric`) several times and stops the running `ghostframe-xdaemon.service` for a few minutes. Ask the owner explicitly, naming both effects, and wait for a yes. Do not run Step 7 on an implied or earlier approval.

- [ ] **Step 7: Record xrandr and XrandrDisplay on Enlightenment (owner-approved)**

```bash
export DISPLAY=:0 XAUTHORITY=$HOME/.Xauthority
N=target/gf-x11-notes; mkdir -p "$N"
P=target/debug/examples/randr_probe
systemctl --user stop ghostframe-xdaemon.service
$P state > "$N/state-before.txt"
OUT=$(xrandr --query | awk '/ connected/ {print $1; exit}')   # HDMI-A-1 on evangeline
grep -q ' 1280x720 ' "$N/state-before.txt" && echo "1280x720 offered" || echo "pick another listed mode below 1920x1080 and use it for SMALL"
SMALL=1280x720 BIG=1920x1080

# xrandr, shrink then grow (15 s windows: a revert by the WM shows up as extra events)
$P record 15 > "$N/xrandr-shrink.log" & sleep 1
xrandr --verbose --output "$OUT" --mode "$SMALL" > "$N/xrandr-shrink.verbose"; wait
$P state > "$N/state-after-xrandr-shrink.txt"
$P record 15 > "$N/xrandr-grow.log" & sleep 1
xrandr --verbose --output "$OUT" --mode "$BIG" > "$N/xrandr-grow.verbose"; wait
$P state > "$N/state-after-xrandr-grow.txt"

# XrandrDisplay (today's code), shrink then grow
$P record 15 > "$N/ours-shrink.log" & sleep 1
$P apply ${SMALL%x*} ${SMALL#*x}; wait
$P state > "$N/state-after-ours-shrink.txt"
$P record 15 > "$N/ours-grow.log" & sleep 1
$P apply ${BIG%x*} ${BIG#*x}; wait
$P state > "$N/state-after-ours-grow.txt"

# Restore and bring the service back
xrandr --output "$OUT" --mode "$BIG"
systemctl --user start ghostframe-xdaemon.service
```

While each 15 s window runs, ask the owner to watch the monitor and note whether Enlightenment re-lays out (shelf and maximised windows follow the new size) and whether the mode snaps back by itself.

Compare: the `--- shape` sections of `xrandr-*.log` against `ours-*.log`; the `screen` line and the `crtc`/`panning` lines of each `state-after-*.txt`; the `Screen { mm_width, mm_height }` values; the `crtc 0:` / `screen 0:` order in `xrandr-*.verbose`.

- [ ] **Step 8: Decide, and record the outcome**

Fill in the "Task 1 findings" section at the end of this file (paste the four shapes, the four final `screen` lines, the owner's observations) and pick the outcome:

| Outcome | Evidence | What Task 8 does |
|---|---|---|
| **A** (expected) | xrandr's shape starts `crtc off` (disable before `screen`), E keeps the new size; ours has no `crtc off` and/or reverts within 15 s | Task 8 as written (xrandr order + mode reuse) |
| **B** | ours has the same shape as xrandr's, E still reverts ours; the `Screen` events' `mm_width`/`mm_height` differ between the two | Task 8 **plus** Step 8b (keep the current DPI) |
| **C** | E reverts ours and the final state shows the old size with **no** `Crtc` event in our window, while `panning` in `state-before.txt` is non-zero | Task 8 **plus** Step 8c (reset panning) |
| **D** | none of the above, or Task 15's re-check on E (Task 16 step 3) still reverts | stop: report both recordings to the owner; the cause is outside the spec's two hypotheses and needs a spec change |

More than one of B and C can apply; apply each step whose evidence is present.

- [ ] **Step 9: Commit the findings**

```bash
git add docs/superpowers/plans/2026-10-29-ghostframe-server-x11.md
git commit -m "docs(plan): RandR recording on Enlightenment, outcome <A|B|C|D>

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

(Write the actual outcome letter(s) in the subject line.)

---

### Task 2: Confirm from an e2e log which ingest tier DRM-direct exercises

Spec section 7 claims dma-buf import is not exercised today (the test pattern holds DRM master, so writeback fails with EACCES and capture falls back to a CPU copy of the scanout). It must be confirmed from a log **before** `--drm-direct` is deleted. Needs the host's VKMS and the GPU.

**Files:**
- Modify: `docs/superpowers/specs/2026-10-29-ghostframe-server-x11-design.md` (section 7, the `*dma-buf import:*` bullet)

- [ ] **Step 1: Build the image from the current tree**

```bash
just build-web
flock -o ../.gf-heavy.lock just containers-build
```

Expected: both images build.

- [ ] **Step 2: Run one DRM-direct test while following the server log**

```bash
N=target/gf-x11-notes; mkdir -p "$N"
( until docker inspect ghostframe-server >/dev/null 2>&1; do sleep 0.5; done
  docker logs -f ghostframe-server > "$N/ingest-e2e.log" 2>&1 ) &
flock -o ../.gf-heavy.lock ../gf-gpu-run cargo test --test e2e -- e2e_cdf53_gradient_emission --exact --test-threads=1
wait
grep -c 'DRM capture available (zero-copy GPU path (PRIME DMA-BUF))' "$N/ingest-e2e.log"
grep -c 'DRM capture available (CPU-mmap path (modesetting FB))' "$N/ingest-e2e.log"
grep -c 'dma-buf GPU import refused' "$N/ingest-e2e.log"
```

The first two strings are logged once at startup by `main.rs` (`"DRM capture available ({})"`), the third by `ghostframe-lib/src/capture/gpu_pipeline/frame.rs` on the first failed import.

- [ ] **Step 3: Record the result in the spec**

Append one sentence to the `*dma-buf import:*` bullet in spec section 7, by outcome:

- CPU-mmap `>= 1`, zero-copy `0`: `Confirmed 2026-10-29 from an e2e_cdf53_gradient_emission log: capture took the CPU-mmap path; no dma-buf was submitted.`
- zero-copy `>= 1`, refused `>= 1`: `Checked 2026-10-29: writeback produced dma-bufs but every import was refused (CPU-mmap ingest), so no import succeeded in e2e.`
- zero-copy `>= 1`, refused `0`: `Refuted 2026-10-29: DRM-direct e2e did import dma-bufs; deleting it removes the only e2e coverage of import_dmabuf. Remaining coverage: ghostframe-lib/tests/gpu_pipeline.rs (dmabuf_real_*), local-only.` and tell the owner before continuing past Task 4.
- all three `0`: the container never reached capture. Re-run Step 2 once; if it is still `0`, stop and report.

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-10-29-ghostframe-server-x11-design.md
git commit -m "docs(spec): record which ingest tier DRM-direct e2e exercised

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

---

### Task 3: Rename the crate and binary to ghostframe-server

Mechanical: names only, no behaviour. `packaging/` and `tests/packaging/` are left alone (Task 13 deletes them, and they must keep naming the legacy unit `ghostframe-xdaemon.service`), as are `docs/superpowers/`, `docs/reviews/`, `docs/specs/` and `docs/design/` (historical).

**Files:**
- Move: `ghostframe-xdaemon/` → `ghostframe-server/`
- Modify: every tracked file outside those exclusions that names `ghostframe-xdaemon` or the bare word `xdaemon` (about 50; `git grep -l xdaemon` lists them), plus `Cargo.lock`

- [ ] **Step 1: Move the crate and rewrite the names**

```bash
git mv ghostframe-xdaemon ghostframe-server
files=$(git grep -l -E "xdaemon" -- ':!docs/superpowers' ':!docs/reviews' ':!docs/specs' ':!docs/design' ':!packaging' ':!tests/packaging' ':!Cargo.lock')
sed -i -e 's/ghostframe::xdaemon::/ghostframe::server::/g' \
       -e 's/resolve_xdaemon_host_pid/resolve_server_host_pid/g' \
       -e 's/xdaemon_host_pid/server_host_pid/g' \
       -e 's/ghostframe-xdaemon/ghostframe-server/g' \
       -e 's/ghostframe_xdaemon/ghostframe_server/g' \
       -e 's/\bxdaemon\b/ghostframe-server/g' $files
cargo fmt --all
```

The order matters: the tracing target `ghostframe::xdaemon::capture` becomes `ghostframe::server::capture` and the harness identifiers become `server_host_pid` before the generic word rule runs. `XdaemonConfig` (capital X) is untouched here; Task 9 renames it.

- [ ] **Step 2: Check nothing was missed or mangled**

```bash
git grep -n "xdaemon" -- ':!docs' ':!packaging' ':!tests/packaging' ':!Cargo.lock'
git grep -n "ghostframe-ghostframe"
```

Expected: both print nothing.

- [ ] **Step 3: Build, lint and test**

```bash
cargo build -p ghostframe-server
just lint
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins
cargo test -p ghostframe-server --test init_flag
ci/check-env-reads.sh && ci/check-release-features.sh
```

Expected: all pass; `Cargo.lock` now names `ghostframe-server`. (`tests/init_flag.rs` was renamed too and must still pass; Task 11 replaces it.)

- [ ] **Step 4: Commit**

In the same shell as Step 1 (`$files` is the list it rewrote; the move itself is already staged by `git mv`):

```bash
git status --short
git add Cargo.lock $files
git commit -m "refactor: rename ghostframe-xdaemon to ghostframe-server

Crate, binary, process name in the e2e harness, test-server image,
Justfile, CI and docs. No behaviour change.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

`git status --short` must show nothing left unstaged afterwards; if `cargo fmt` touched a file outside `$files`, it was unformatted before this task: restore it with `git checkout -- <file>` rather than committing it here.

---

### Task 4: Test patterns paint into Xorg (`--x11-sink`), e2e switches to it

Replaces the DRM-master paint path. The patterns already render into a BGRX buffer laid out like the DRM dumb buffer (`pitch`, `mode_w`, `mode_h`), so each module only swaps "map the dumb buffer, msync" for "map the sink frame, which draws itself into root when dropped". The server still has DRM capture at this point, so the entrypoint forces X11 capture for sink scenes (`GHOSTFRAME_X11_CAPTURE_ONLY=1`; Task 5 removes both).

**Files:**
- Create: `ghostframe-test-pattern/src/x11_sink.rs`
- Move+rewrite: `ghostframe-test-pattern/src/drm_direct.rs` → `src/cycle.rs`; `src/text_grid_drm.rs` → `src/text_grid_sink.rs`
- Modify: `ghostframe-test-pattern/src/{lib,main,gradient,lossless_golden,tile_pattern,subtle_drift,palrle_exact,scroll_text,solid_per_tile}.rs`, `ghostframe-test-pattern/Cargo.toml`
- Modify: `ghostframe-e2e/tests/{e2e,h264,native_client}.rs`, `ghostframe-e2e/src/harness/scroll_scene.rs`, `ghostframe-bench/benches/README.md`, `ghostframe-client-gpu/README.md`, `ci/skip-list.txt`, `docs/ci.md`
- Modify: `tests/containers/test-server/entrypoint.sh`

- [ ] **Step 1: Write the sink and its tests**

`ghostframe-test-pattern/Cargo.toml`: replace `x11rb = "0.13"` with `x11rb = { version = "0.13", features = ["shm"] }`. Leave `drm`/`drm-ffi` for Step 5.

Create `ghostframe-test-pattern/src/x11_sink.rs`:

````rust
//! `--x11-sink`: the patterns render into a BGRX buffer the size of the
//! root window, and this draws it into root.
//!
//! It replaces the DRM-master paint path (`--drm-direct`), which existed only
//! because Xorg on VKMS swallowed X drawing while it held DRM master and the
//! server captured from DRM. The server now captures over X11, so the pattern
//! goes where the capture reads it: the root window of `$DISPLAY`.
//!
//! Pixel format is unchanged from the DRM path: XRGB8888 little-endian,
//! `[B, G, R, X]` per pixel, `pitch = width * 4`. The pad byte is ignored by
//! a depth-24 root.
//!
//! The buffer is a MIT-SHM segment drawn with `ShmPutImage` when the server
//! offers MIT-SHM 1.2; otherwise a heap buffer drawn with `PutImage` in row
//! bands no larger than the server's maximum request (a 4096x2160 frame is
//! 35 MB, over the BIG-REQUESTS limit).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, Gcontext, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

/// A memfd-backed MIT-SHM segment the X server reads frames from.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

impl ShmSegment {
    fn attach(conn: &RustConnection, len: usize) -> io::Result<Self> {
        // SAFETY: memfd_create takes a NUL-terminated name and flags only.
        let raw = unsafe { libc::memfd_create(c"ghostframe-sink".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: ftruncate on a descriptor we own.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: maps `len` bytes of a memfd just sized to `len`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Built before the attach so an error below still unmaps.
        let mut me = Self {
            seg: 0,
            ptr: ptr.cast(),
            len,
        };
        me.seg = conn.generate_id().map_err(io::Error::other)?;
        // read_only: the server only reads what we draw.
        conn.shm_attach_fd(me.seg, fd, true)
            .map_err(io::Error::other)?
            .check()
            .map_err(io::Error::other)?;
        Ok(me)
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` maps exactly `len` bytes for the life of `self`, and
        // `&mut self` makes this the only live view.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` come from a successful mmap, unmapped once.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// The root window of `$DISPLAY`, as a paintable framebuffer.
pub struct Scanout {
    conn: RustConnection,
    root: Window,
    gc: Gcontext,
    depth: u8,
    shm: Option<ShmSegment>,
    buf: Vec<u8>,
    /// Bytes per row of the buffer (`mode_w * 4`).
    pub pitch: u32,
    /// Root width in pixels.
    pub mode_w: u32,
    /// Root height in pixels.
    pub mode_h: u32,
}

/// A writable view of the frame. Dropping it draws the frame into root.
pub struct Frame<'a> {
    scanout: &'a mut Scanout,
}

impl AsMut<[u8]> for Frame<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.scanout.pixels_mut()
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        // A test fixture that cannot draw must fail loudly: a silently stale
        // screen reads as a capture or codec bug in the e2e that uses it.
        if let Err(e) = self.scanout.present() {
            panic!("x11_sink: drawing the frame into root failed: {e}");
        }
    }
}

impl Scanout {
    /// Connect to `display` (`None` = `$DISPLAY`) and size the buffer to root.
    pub fn connect(display: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let screen = &conn.setup().roots[screen_num];
        let (root, depth) = (screen.root, screen.root_depth);
        let (w, h) = (
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        );
        let gc = conn.generate_id()?;
        conn.create_gc(gc, root, &CreateGCAux::new())?.check()?;
        let len = w as usize * h as usize * 4;
        let shm = if shm_1_2(&conn) {
            match ShmSegment::attach(&conn, len) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("x11_sink: MIT-SHM attach failed ({e}); using PutImage");
                    None
                }
            }
        } else {
            None
        };
        let buf = if shm.is_some() {
            Vec::new()
        } else {
            vec![0u8; len]
        };
        eprintln!(
            "x11_sink: root {w}x{h} depth {depth}, drawing with {}",
            if shm.is_some() {
                "ShmPutImage"
            } else {
                "PutImage"
            }
        );
        Ok(Self {
            conn,
            root,
            gc,
            depth,
            shm,
            buf,
            pitch: w * 4,
            mode_w: w,
            mode_h: h,
        })
    }

    /// Whether frames go through MIT-SHM.
    pub fn uses_shm(&self) -> bool {
        self.shm.is_some()
    }

    /// The frame to paint; it is drawn into root when the view drops.
    pub fn map(&mut self) -> Frame<'_> {
        Frame { scanout: self }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        match &mut self.shm {
            Some(s) => s.bytes_mut(),
            None => &mut self.buf,
        }
    }

    /// Draw the current buffer into root and wait until the server has read
    /// it, so the caller may overwrite the buffer at once.
    pub fn present(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (w, h) = (self.mode_w as u16, self.mode_h as u16);
        if let Some(s) = &self.shm {
            self.conn
                .shm_put_image(
                    self.root,
                    self.gc,
                    w,
                    h,
                    0,
                    0,
                    w,
                    h,
                    0,
                    0,
                    self.depth,
                    ImageFormat::Z_PIXMAP.into(),
                    false,
                    s.seg,
                    0,
                )?
                .check()?;
        } else {
            let row = self.pitch as usize;
            let max_rows = (self.conn.maximum_request_bytes().saturating_sub(64) / row).max(1);
            let mut y = 0usize;
            while y < usize::from(h) {
                let rows = max_rows.min(usize::from(h) - y);
                self.conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    self.root,
                    self.gc,
                    w,
                    rows as u16,
                    0,
                    y as i16,
                    0,
                    self.depth,
                    &self.buf[y * row..(y + rows) * row],
                )?;
                y += rows;
            }
        }
        // A round trip: once it returns, the server has consumed the buffer.
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }
}

/// MIT-SHM 1.2 (fd passing) is present.
fn shm_1_2(conn: &RustConnection) -> bool {
    conn.shm_query_version()
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    /// A private Xvfb, so a test can choose the screen size and extensions.
    struct PrivateXvfb {
        child: Child,
        display: String,
    }

    impl PrivateXvfb {
        fn spawn(args: &[&str]) -> Self {
            let mut child = Command::new("Xvfb")
                .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset"])
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn Xvfb");
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("piped stdout"))
                .read_line(&mut line)
                .expect("read Xvfb display number");
            let n: u32 = line.trim().parse().expect("Xvfb display number");
            Self {
                child,
                display: format!(":{n}"),
            }
        }
    }

    impl Drop for PrivateXvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn pixel(x: u32, y: u32) -> [u8; 3] {
        [(x * 3) as u8, (y * 3) as u8, ((x + y) * 2) as u8]
    }

    /// Paint a gradient through the sink and read root back with GetImage.
    fn round_trip(args: &[&str], want_shm: bool) {
        let x = PrivateXvfb::spawn(args);
        let mut s = Scanout::connect(Some(&x.display)).unwrap();
        assert_eq!(s.uses_shm(), want_shm);
        let (w, h) = (s.mode_w, s.mode_h);
        {
            let mut f = s.map();
            for (i, px) in f.as_mut().chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                let [b, g, r] = pixel(x, y);
                px.copy_from_slice(&[b, g, r, 0]);
            }
        }
        let (conn, n) = x11rb::connect(Some(&x.display)).unwrap();
        let root = conn.setup().roots[n].root;
        let got = conn
            .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, w as u16, h as u16, !0)
            .unwrap()
            .reply()
            .unwrap()
            .data;
        for (i, px) in got.chunks_exact(4).enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            assert_eq!(px[..3], pixel(x, y), "pixel ({x},{y})");
        }
    }

    #[test]
    fn shm_frames_land_in_root() {
        round_trip(&["-screen", "0", "320x200x24"], true);
    }

    /// 4096x2160x4 is 35 MB: over the request limit, so this also proves the
    /// banding.
    #[test]
    fn put_image_frames_land_in_root_without_mit_shm() {
        round_trip(
            &["-extension", "MIT-SHM", "-screen", "0", "4096x2160x24"],
            false,
        );
    }
}
````

In `ghostframe-test-pattern/src/lib.rs` add `pub mod x11_sink;` after `pub mod tile_pattern;`.

- [ ] **Step 2: Run the sink tests**

Run: `cargo test -p ghostframe-test-pattern x11_sink`
Expected: `shm_frames_land_in_root` and `put_image_frames_land_in_root_without_mit_shm` pass (the second allocates a 4096x2160 Xvfb, about 35 MB).

- [ ] **Step 3: Break it, watch it fail, restore**

1. In `shm_1_2`, change `>= (1, 2)` to `>= (9, 9)`. Run `cargo test -p ghostframe-test-pattern x11_sink`: `shm_frames_land_in_root` must fail on `assert_eq!(s.uses_shm(), want_shm)`. Change it back.
2. In `present`, in the `put_image` call, change `y as i16,` to `0,`. Run the same command: `put_image_frames_land_in_root_without_mit_shm` must fail on a `pixel (x,y)` assertion (every band lands at the top). Change it back.

- [ ] **Step 4: Port the mode-switch cycle and the text grid**

```bash
cd ghostframe-test-pattern/src
git mv drm_direct.rs cycle.rs
git mv text_grid_drm.rs text_grid_sink.rs
cd -
```

Replace the whole of `ghostframe-test-pattern/src/cycle.rs` with:

````rust
//! `--x11-sink` + `--mode-switch-cycle SECS` mode.
//!
//! Loops painting alternating static/motion content into the root window
//! through [`crate::x11_sink`]: a solid static half, then a motion half that
//! repaints every `motion_tick`.
//!
//! Pixel format: XRGB8888 little-endian, `[B, G, R, X]`.

use std::thread;
use std::time::{Duration, Instant};

use crate::x11_sink::Scanout;

/// Background colour for the static half — packed XRGB8888 little-endian, so
/// in memory bytes are `[B=0xA0, G=0x40, R=0x20, X=0x00]`. Matches the
/// X11-based mode_switch's intent (steel-blue-ish).
const STATIC_BG_PIXEL: u32 = 0x00_20_40_A0;
/// Default motion paint interval. MUST be < capture interval so every captured frame
/// sees a fresh paint and Vulkan SAD reports dirty tiles. With CAPTURE_FPS=30
/// (33 ms), 50 ms paints would leave every-other capture seeing no change
/// (empty dirty → classifier's `enter_streak` resets → never reaches sustain
/// → mode never flips to H264). 16 ms (~60 Hz paint) safely beats any
/// reasonable capture rate.
pub const MOTION_TICK: Duration = Duration::from_millis(16);
/// Band height matches `ghostframe-lib::tile::TILE_SIZE` (32 px) so every
/// motion-band exactly covers one tile row in the capture grid. Kept in sync
/// manually since `ghostframe-test-pattern` has no `ghostframe-lib`
/// dependency.
const BAND_H: u32 = 32;

/// What the motion half paints.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum Motion {
    /// One flat colour per tile row, recoloured every tick. Every tile is
    /// dirty every frame, and every tile is `Solid`: four bytes each. This
    /// is motion the tile codec is *right* to keep -- a whole screen of it
    /// costs less than one H.264 frame -- so nothing but a network override
    /// should move the classifier off TileCodec while it plays.
    #[default]
    Bands,
    /// A scrolling per-pixel gradient: every tile is dirty every frame and
    /// holds far more than 16 colours, so each classifies as `Cdf53`. A
    /// screenful of those per frame is what H.264 exists for, and the cost
    /// comparison is expected to enter it.
    Video,
}

/// Run the static/motion paint loop on the root window.
pub fn run(
    half_cycle: Duration,
    motion: Motion,
    motion_tick: Duration,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = Scanout::connect(None)?;
    // Pre-fill with the static background before the first capture sees any pixels.
    fill_solid(&mut scanout, STATIC_BG_PIXEL);
    eprintln!("cycle: root painted — entering paint loop");

    let cycle = half_cycle * 2;
    let mut motion_phase: u8 = 0;

    loop {
        let cycle_start = Instant::now();

        // Static half: paint the buffer once with the solid colour.
        fill_solid(&mut scanout, STATIC_BG_PIXEL);
        thread::sleep(half_cycle);

        // Motion half: rewrite tile-aligned bands of shifting colours every
        // `motion_tick` so the capture grid sees fresh dirty tiles every frame.
        let motion_deadline = cycle_start + cycle;
        while Instant::now() < motion_deadline {
            match motion {
                Motion::Bands => paint_motion(&mut scanout, motion_phase),
                Motion::Video => paint_video(&mut scanout, motion_phase),
            }
            motion_phase = motion_phase.wrapping_add(7);
            thread::sleep(motion_tick);
        }
    }
}

/// Fill the frame with a single packed-XRGB pixel value.
fn fill_solid(scanout: &mut Scanout, pixel: u32) {
    let row_pitch = scanout.pitch as usize;
    let row_active = (scanout.mode_w as usize) * 4;
    let height = scanout.mode_h as usize;
    let mut map = scanout.map();
    let bytes = map.as_mut();
    let pixel_bytes = pixel.to_le_bytes();
    for y in 0..height {
        let row_start = y * row_pitch;
        let row = &mut bytes[row_start..row_start + row_active];
        for chunk in row.chunks_exact_mut(4) {
            chunk.copy_from_slice(&pixel_bytes);
        }
    }
}

/// Colour of pixel `(x, y)` in the `Motion::Video` frame at `phase`, packed
/// XRGB8888. Red follows x and green follows y, so any 32x32 tile holds 1024
/// distinct colours; all three channels slide with `phase`, so no pixel
/// keeps its value from one frame to the next.
pub fn video_pixel(x: u32, y: u32, phase: u8) -> u32 {
    let p = phase as u32;
    let r = (x + p) & 0xFF;
    let g = (y + 2 * p) & 0xFF;
    let b = ((x + y) / 2 + 3 * p) & 0xFF;
    (r << 16) | (g << 8) | b
}

/// Paint the `Motion::Video` frame for `motion_phase`.
fn paint_video(scanout: &mut Scanout, motion_phase: u8) {
    let row_pitch = scanout.pitch as usize;
    let row_active = (scanout.mode_w as usize) * 4;
    let height = scanout.mode_h;
    let mut map = scanout.map();
    let bytes = map.as_mut();
    for y in 0..height {
        let row_start = (y as usize) * row_pitch;
        let row = &mut bytes[row_start..row_start + row_active];
        for (x, chunk) in row.chunks_exact_mut(4).enumerate() {
            chunk.copy_from_slice(&video_pixel(x as u32, y, motion_phase).to_le_bytes());
        }
    }
}

/// Paint horizontal bands of shifting colours, each `BAND_H` rows tall.
fn paint_motion(scanout: &mut Scanout, motion_phase: u8) {
    let row_pitch = scanout.pitch as usize;
    let row_active = (scanout.mode_w as usize) * 4;
    let height = scanout.mode_h;
    let mut map = scanout.map();
    let bytes = map.as_mut();
    let mut band_idx: u8 = 0;
    let mut y = 0u32;
    while y < height {
        let r = motion_phase.wrapping_mul(3).wrapping_add(band_idx);
        let g = motion_phase.wrapping_mul(5).wrapping_add(band_idx ^ 0x55);
        let b = motion_phase.wrapping_mul(7).wrapping_add(band_idx ^ 0xAA);
        // XRGB8888 little-endian: bytes [B, G, R, X]
        let pixel = ((r as u32) << 16) | ((g as u32) << 8) | (b as u32);
        let pixel_bytes = pixel.to_le_bytes();

        let band_end = (y + BAND_H).min(height);
        for row in y..band_end {
            let row_start = (row as usize) * row_pitch;
            let row_slice = &mut bytes[row_start..row_start + row_active];
            for chunk in row_slice.chunks_exact_mut(4) {
                chunk.copy_from_slice(&pixel_bytes);
            }
        }

        y += BAND_H;
        band_idx = band_idx.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::video_pixel;
    use std::collections::HashSet;

    /// The two properties the classifier is supposed to react to. Sixteen is
    /// the PalRle palette limit: at or under it a tile is cheap again and the
    /// pattern stops being a reason to enter H.264.
    #[test]
    fn video_motion_gives_every_tile_many_colours_and_changes_every_pixel() {
        for phase in [0u8, 7, 14, 249] {
            for (tx, ty) in [(0u32, 0u32), (31, 23), (59, 33)] {
                let colours: HashSet<u32> = (0..32)
                    .flat_map(|dy| (0..32).map(move |dx| (dx, dy)))
                    .map(|(dx, dy)| video_pixel(tx * 32 + dx, ty * 32 + dy, phase))
                    .collect();
                assert!(colours.len() > 16, "tile ({tx},{ty}) phase {phase}");
            }
            // The paint loop advances the phase by 7 per tick.
            let next = phase.wrapping_add(7);
            for (x, y) in [(0u32, 0u32), (1919, 1079), (640, 400)] {
                assert_ne!(video_pixel(x, y, phase), video_pixel(x, y, next));
            }
        }
    }
}
````

Replace the whole of `ghostframe-test-pattern/src/text_grid_sink.rs` with:

````rust
//! `--text-grid --x11-sink` mode.
//!
//! Paints `text_grid::TEXT` glyphs pixel-exactly into the root window
//! through [`crate::x11_sink`]. The X11 `--text-grid` path draws with a core
//! font instead, whose rendering does not match `font.rs` bit for bit; the
//! e2e tests that assert exact glyph pixels need this one.
//!
//! The text is static: it is painted once, in one frame, so a capture never
//! sees the background without the glyphs.

use std::thread;
use std::time::Duration;

use crate::font::{pixel_set, GLYPH_H, GLYPH_W};
use crate::text_grid::{char_origin, BG_PIXEL, FG_PIXEL, TEXT};
use crate::x11_sink::Scanout;

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = Scanout::connect(None)?;
    eprintln!(
        "text_grid_sink: scanout {}x{} active — painting once then entering keepalive loop",
        scanout.mode_w, scanout.mode_h
    );

    let pitch = scanout.pitch as usize;
    let width = scanout.mode_w;
    let height = scanout.mode_h;

    // ── One-time paint ──────────────────────────────────────────────────────
    {
        let mut map = scanout.map();
        let bytes = map.as_mut();

        // Background fill — pack BG_PIXEL (0x00141414) into every active pixel.
        let bg_bytes = BG_PIXEL.to_le_bytes();
        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let row = &mut bytes[row_start..row_start + row_active];
            for chunk in row.chunks_exact_mut(4) {
                chunk.copy_from_slice(&bg_bytes);
            }
        }

        // Foreground glyphs.
        let fg_bytes = FG_PIXEL.to_le_bytes();
        for (n, ch) in TEXT.chars().enumerate() {
            let (gx0, gy0) = char_origin(n);
            for py in 0..GLYPH_H {
                for px in 0..GLYPH_W {
                    if pixel_set(ch, px, py) {
                        let ax = (gx0 + px) as usize;
                        let ay = (gy0 + py) as usize;
                        if ax < width as usize && ay < height as usize {
                            let off = ay * pitch + ax * 4;
                            bytes[off..off + 4].copy_from_slice(&fg_bytes);
                        }
                    }
                }
            }
        }
        // `map` drops here, which draws the frame into root.
    }

    eprintln!("text_grid_sink: initial paint complete — entering keepalive loop");

    // The root window keeps what was drawn; nothing to refresh.
    loop {
        thread::sleep(Duration::from_secs(60));
    }
}
````

In `lib.rs`: `pub mod drm_direct;` → `pub mod cycle;`, `pub mod text_grid_drm;` → `pub mod text_grid_sink;`.

- [ ] **Step 5: Port the remaining patterns**

The seven modules share one shape, rewritten mechanically:

```bash
cd ghostframe-test-pattern/src
F="gradient.rs lossless_golden.rs tile_pattern.rs subtle_drift.rs palrle_exact.rs scroll_text.rs solid_per_tile.rs"
perl -0pi -e '
  s/scanout\s*\.card\s*\.map_dumb_buffer\(&mut scanout\.db\)\s*\.map_err\(\|e\| format!\("map_dumb_buffer[^"]*"\)\)\?;/scanout.map();/g;
  s/^\s*msync_buffer\([^\n]*\);\n//mg;
  s/^use drm::control::Device as ControlDevice;\n\n?//mg;
  s/use crate::drm_direct::\{msync_buffer, setup_dumb_scanout\};/use crate::x11_sink::Scanout;/g;
  s/setup_dumb_scanout\(card_path\)\?/Scanout::connect(None)?/g;
  s/pub fn run\(card_path: &str\)/pub fn run()/g;
  s/pub fn run\(card_path: &str, /pub fn run(/g;
  s/\n    card_path: &str,\n/\n/g;
  s/run_inner\(card_path, /run_inner(/g;
  s/tile_pattern::run\(card_path, /tile_pattern::run(/g;
  s/crate::drm_direct::DrmScanout/Scanout/g;
  s/--drm-direct/--x11-sink/g;
  s/text_grid_drm/text_grid_sink/g;
  s/drm_direct::MOTION_TICK/cycle::MOTION_TICK/g;
  s/`drm_direct\.rs`/`cycle.rs`/g' $F
perl -pi -e 's/DRM dumb buffer/frame buffer/g; s/the DRM scanout/the root window/g; s/DRM scanout/root-window frame/g; s/the mmap slice for the frame buffer/the frame buffer/g; s/dumb buffer/frame buffer/g' $F
cd -
```

Then these hand edits, each an exact replacement:

`subtle_drift.rs` — the frame borrows `scanout` mutably, so the initial paint must use the locals:

```rust
        tile_pattern::fill_with_tile_pattern(
            class_name,
            scanout.mode_w,
            scanout.mode_h,
            pitch,
            bytes,
        );
```
→
```rust
        tile_pattern::fill_with_tile_pattern(
            class_name,
            width as u32,
            height as u32,
            pitch,
            bytes,
        );
```

`subtle_drift.rs` — the comment above the drift loop. Replace
```rust
    //
    // It has to happen on the DRM path rather than via X11. The e2e server
    // captures from DRM, and with no compositor and a non-flipping driver,
    // X rendering after the initial modeset never reaches scanout: a mixed.rs
    // repaint fires, prints its marker, and the server captures 44 further
    // frames without ever seeing a dirty tile. Writing the frame buffer
    // directly is what the capture actually reads.
    // Drift loop: shift bytes left by 4 (one BGRX pixel) per row, wrapping
    // the leftmost pixel to the right edge. msync each frame.
```
with
```rust
    //
    // Drift loop: shift bytes left by 4 (one BGRX pixel) per row, wrapping
    // the leftmost pixel to the right edge; each frame is drawn into root
    // when `map` drops.
```
Its module doc line becomes `//! the root window, then every ...` through the generic rewrite; `grep -n "then every" subtle_drift.rs` confirms it.

`palrle_exact.rs` — the keep-alive loop:
```rust
    let keepalive = Duration::from_millis(33);
    loop {
        thread::sleep(keepalive);
        let mut map = scanout.map();
    }
```
→
```rust
    // The root window keeps what was drawn; nothing to refresh.
    loop {
        thread::sleep(Duration::from_secs(60));
    }
```
and (after the generic rewrite turned `dumb buffer` into `frame buffer`) `/// frame buffer and msyncs without writing — the pattern is static.` → `/// loop that only sleeps — the pattern is static.`

`gradient.rs` — Replace
```rust
        // `map` drops here, returning the buffer to the DRM master.
```
with
```rust
        // `map` drops here, which draws the frame into root.
```

`scroll_text.rs` (module doc) — Replace
```rust
//!    the final frame forever (a keep-alive `msync` only, no pixel writes).
```
with
```rust
//!    the final frame forever (re-drawn unchanged every 33 ms).
```

- [ ] **Step 6: Replace `--drm-direct` with `--x11-sink` in the CLI**

In `ghostframe-test-pattern/src/main.rs`:

1. The `subtle_drift_once` doc: replace
```rust
    /// With --drm-direct and --tile-pattern: paint, wait this many
    /// milliseconds, shift the bitmap ONCE, then leave the screen alone
    /// forever. Models a dialog opening on a settled desktop.
    ///
    /// Must be the DRM path: the e2e server captures from DRM, and with no
    /// compositor an X11 repaint after the initial modeset never reaches
    /// scanout, so the change is invisible to capture.
```
with
```rust
    /// With --x11-sink and --tile-pattern: paint, wait this many
    /// milliseconds, shift the bitmap ONCE, then leave the screen alone
    /// forever. Models a dialog opening on a settled desktop.
```
2. Replace the `drm_direct` and `drm_device` fields (from `    /// Run as a DRM master and paint directly to` through `    drm_device: String,`) with
```rust
    /// Render the pattern into a BGRX buffer the size of the root window and
    /// draw it into root with (Shm)PutImage, pixel-exact. Required by the
    /// patterns below that say so; with `--mode-switch-cycle` it selects the
    /// buffer-painted cycle instead of the X11-primitive one.
    #[arg(long)]
    x11_sink: bool,
```
3. Mechanical renames in the rest of the file:
```bash
perl -pi -e 's/ghostframe_test_pattern::drm_direct::/ghostframe_test_pattern::cycle::/g; s/--drm-direct/--x11-sink/g; s/args\.drm_direct/args.x11_sink/g; s/\(&args\.drm_device\)/()/g; s/text_grid_drm::run/text_grid_sink::run/g; s/tile_pattern::run\(&args\.drm_device, class_name\)/tile_pattern::run(class_name)/g' ghostframe-test-pattern/src/main.rs
```
4. Remove the remaining `&args.drm_device,` argument lines (in the `subtle_drift::run_once(`, `subtle_drift::run(`, `scroll_text::run(` and `cycle::run(` calls):
```bash
sed -i '/^ *&args\.drm_device,$/d' ghostframe-test-pattern/src/main.rs
grep -n drm_device ghostframe-test-pattern/src/main.rs   # must print nothing
```
5. Comments (each an exact replacement):
Replace
```rust
    /// paint to the DRM dumb-buffer scanout. Drives `e2e_solid_per_tile_pixels`.
```
with
```rust
    /// paint pixel-exactly into root. Drives `e2e_solid_per_tile_pixels`.
```
Replace
```rust
    /// palette. Combine with `--x11-sink` to paint to the DRM
    /// dumb-buffer scanout. Drives `e2e_palrle_exact_pixels`.
```
with
```rust
    /// palette. Combine with `--x11-sink` to paint pixel-exactly into
    /// root. Drives `e2e_palrle_exact_pixels`.
```
Replace
```rust
    /// Combine with `--x11-sink` to paint to the DRM dumb-buffer scanout.
```
with
```rust
    /// Combine with `--x11-sink` to paint pixel-exactly into root.
```
Every remaining `    // DRM-direct <name> mode` comment becomes `    // Sink <name> mode` (`sed -i 's|// DRM-direct \([a-z-]*\) mode|// Sink \1 mode|' ghostframe-test-pattern/src/main.rs`), then:
Replace
```rust
    // Cdf53 e2e tests. No X11 path; gradient is DRM-only.
```
with
```rust
    // Cdf53 e2e tests. Sink only.
```
Replace
```rust
    // repeated to cover the scanout. Drives M3.5 Layer B pure-scene benches.
```
with
```rust
    // repeated to cover the screen. Drives M3.5 Layer B pure-scene benches.
```
Replace
```rust
    // Sink text-grid mode: paint text glyphs directly to scanout,
    // bypassing Xorg. Required for E2E tests where Xorg-on-VKMS yields
    // >16 unique colours per tile and PalRle never wins classification.
    // See docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md.
```
with
```rust
    // Sink text-grid mode: paint `font.rs` glyphs pixel-exactly. The X11
    // core-font `--text-grid` yields > 16 unique colours per tile, so PalRle
    // never wins classification on it.
    // See docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md.
```
Replace
```rust
    // DRM-direct path: take DRM master and paint straight to scanout.
    // Must run before any X11 connect attempt — there is no X server in
    // this configuration.
```
with
```rust
    // Sink mode-switch cycle: the buffer-painted static/motion loop.
```

The finished dispatch reads (for checking):

````rust
    // Sink palrle-exact mode: paint four 32×32 PalRle test tiles
    // with known 2-color patterns. Drives `e2e_palrle_exact_pixels`
    // (M3.2c B1 follow-up).
    if args.x11_sink && args.palrle_exact {
        return ghostframe_test_pattern::palrle_exact::run();
    }

    // Sink solid-per-tile mode: paint fixed corner tiles + a small
    // central motion region so the classifier sees `unique_colors == 1`
    // per corner and picks `CodecState::Solid`. Drives
    // `e2e_solid_per_tile_pixels` (M3.2c W3/B3).
    if args.x11_sink && args.solid_per_tile {
        return ghostframe_test_pattern::solid_per_tile::run();
    }

    // Sink gradient mode: full-screen high-color gradient for M3.3a
    // Cdf53 e2e tests. Sink only.
    if args.x11_sink && args.gradient {
        return ghostframe_test_pattern::gradient::run();
    }

    // Sink lossless-golden mode: static codec-mixed frame
    // (Solid + PalRle + Cdf53 regions) for whole-frame strict pixel
    // compare. Drives `e2e_lossless_golden_png`.
    if args.x11_sink && args.lossless_golden {
        return ghostframe_test_pattern::lossless_golden::run();
    }

    // Sink tile-pattern mode: full-screen ContentClass fixture tile
    // repeated to cover the screen. Drives M3.5 Layer B pure-scene benches.
    // With --subtle-drift <ms>, shifts the bitmap 1 px/X per tick to generate
    // dirty events for the classifier (M3.7a bench).
    if args.x11_sink {
        if let Some(ref class_name) = args.tile_pattern {
            if let Some(delay_ms) = args.subtle_drift_once {
                return ghostframe_test_pattern::subtle_drift::run_once(class_name, delay_ms);
            }
            if let Some(drift_ms) = args.subtle_drift {
                return ghostframe_test_pattern::subtle_drift::run(class_name, drift_ms);
            }
            return ghostframe_test_pattern::tile_pattern::run(class_name);
        }
    }

    // Sink scrolling-text mode (text-under-motion bench scene).
    if args.x11_sink && args.scroll_text {
        return ghostframe_test_pattern::scroll_text::run(
            &ghostframe_test_pattern::scroll_text::ScrollConfig {
                rows_per_frame: args.scroll_rows_per_frame,
                scroll_secs: args.scroll_secs,
                fps: args.scroll_fps.max(1),
                go_file: args.scroll_go_file.clone(),
            },
        );
    }

    // Sink text-grid mode: paint `font.rs` glyphs pixel-exactly. The X11
    // core-font `--text-grid` yields > 16 unique colours per tile, so PalRle
    // never wins classification on it.
    // See docs/superpowers/specs/2026-05-15-m3.2c-verification-design.md.
    if args.x11_sink && args.text_grid {
        return ghostframe_test_pattern::text_grid_sink::run();
    }

    // Sink mode-switch cycle: the buffer-painted static/motion loop.
    if args.x11_sink {
        let secs = args
            .mode_switch_cycle
            .ok_or_else(|| "--x11-sink requires --mode-switch-cycle SECS".to_string())?;
        let half = std::time::Duration::from_secs(secs);
        return ghostframe_test_pattern::cycle::run(
            half,
            args.motion,
            args.motion_tick_ms.map_or(
                ghostframe_test_pattern::cycle::MOTION_TICK,
                std::time::Duration::from_millis,
            ),
        );
    }
````

6. `ghostframe-test-pattern/Cargo.toml`: delete the `drm = "0.15"` and `drm-ffi = "0.9"` lines.

- [ ] **Step 7: Build, lint, test the crate**

```bash
cargo fmt --all
git grep -n -i "drm\b\|drm_\|dumb buffer\|msync" -- ghostframe-test-pattern/src ':!ghostframe-test-pattern/src/x11_sink.rs'
cargo test -p ghostframe-test-pattern
just lint
```

Expected: the grep prints nothing; 15 tests pass (including `cycle::tests::video_motion_gives_every_tile_many_colours_and_changes_every_pixel`); lint clean.

- [ ] **Step 8: Switch the e2e tests and docs**

```bash
sed -i 's/--drm-direct/--x11-sink/g' \
  ghostframe-e2e/tests/e2e.rs ghostframe-e2e/tests/h264.rs ghostframe-e2e/tests/native_client.rs \
  ghostframe-e2e/src/harness/scroll_scene.rs ghostframe-bench/benches/README.md \
  ghostframe-client-gpu/README.md ci/skip-list.txt docs/ci.md
sed -i 's/CAPTURE_FPS_DRM_DIRECT/CAPTURE_FPS/g; s/the entrypoint default for drm-direct/the entrypoint default for --x11-sink/' ghostframe-e2e/tests/e2e.rs
cargo fmt --all
git grep -n "drm-direct\|CAPTURE_FPS_DRM_DIRECT" -- ':!docs/superpowers' ':!docs/specs' ':!docs/reviews' ':!tests/containers'
```

Expected: the grep prints nothing. (`ghostframe-lib/src/capture/gpu_pipeline/tests.rs` mentions `--drm-direct` in a historical comment about the DMA-BUF path; leave it, it is not in the sed list.) No test sets both `CAPTURE_FPS` and `CAPTURE_FPS_DRM_DIRECT`, so the rename creates no duplicate key.

- [ ] **Step 9: Rewrite the entrypoint**

Replace `tests/containers/test-server/entrypoint.sh` with:

````bash
#!/bin/bash
set -euo pipefail

until curl -fsS http://headscale:8080/health >/dev/null 2>&1; do
    echo "waiting for headscale..."
    sleep 1
done

export TS_HOSTNAME=${TS_HOSTNAME:-ghostframe-server}
export TS_STATE_DIR=${TS_STATE_DIR:-/tmp/ghostframe-ts}
export TS_CONTROL_URL=${TS_CONTROL_URL:-http://headscale:8080}
export DISPLAY=:99

# --x11-sink patterns repaint at up to 60 Hz, and the tests built on them were
# tuned at 30 fps capture (the old DRM-direct default). Everything else keeps
# 2 fps. A CAPTURE_FPS set by the test wins either way.
if [[ "${TEST_PATTERN:-}" == *--x11-sink* ]]; then
    export CAPTURE_FPS=${CAPTURE_FPS:-30}
else
    export CAPTURE_FPS=${CAPTURE_FPS:-2}
fi

# M3.7b: apply tc qdisc shaping if SHAPE_* env vars set.
# netem provides delay + loss; tbf provides bandwidth rate-limit.
# Failures log warnings and continue without shaping (better than
# crashing the bench; downstream detects via the resulting data shape).
if [[ -n "${SHAPE_BANDWIDTH_KBPS:-}" ]]; then
    DELAY_MS="${SHAPE_DELAY_MS:-0}"
    LOSS_PCT="${SHAPE_LOSS_PCT:-0}"
    echo "entrypoint: applying tc shaping — ${SHAPE_BANDWIDTH_KBPS}kbit, ${DELAY_MS}ms delay, ${LOSS_PCT}% loss"
    if tc qdisc add dev eth0 root handle 1: netem delay "${DELAY_MS}ms" loss "${LOSS_PCT}%" 2>&1; then
        if tc qdisc add dev eth0 parent 1:1 handle 10: tbf rate "${SHAPE_BANDWIDTH_KBPS}kbit" burst 32kbit latency 400ms 2>&1; then
            echo "entrypoint: tc shaping applied successfully"
        else
            echo "entrypoint: WARNING — tbf qdisc add failed; rolling back netem to run unshaped"
            tc qdisc del dev eth0 root 2>&1 || true
        fi
    else
        echo "entrypoint: WARNING — tc shaping unavailable (netem add failed; container needs --privileged or NET_ADMIN cap)"
    fi
fi

# If XORG_CONF is unset, auto-detect the right config:
#   - /etc/X11/xorg-vkms.conf if /dev/dri/card0 is accessible IN the
#     container AND the host sysfs shows a Writeback-1 connector on it.
#     Both conditions must be true: the writeback-connector sysfs entry
#     visible via the host-mounted /sys can be seen even when /dev/dri is
#     NOT bind-mounted into the container, so checking /sys alone causes
#     false positives when the host has vkms loaded but the test doesn't
#     use a GPU bind-mount (i.e. all non-GPU e2e tests).
#   - /etc/X11/xorg.conf (Driver "dummy") otherwise.
if [ -z "${XORG_CONF:-}" ]; then
    if [ -e /dev/dri/card0 ] && [ -e /sys/class/drm/card0-Writeback-1 ]; then
        XORG_CONF=/etc/X11/xorg-vkms.conf
        echo "entrypoint: detected VKMS card0 (writeback connector, /dev/dri/card0 present) — using xorg-vkms.conf"
    else
        XORG_CONF=/etc/X11/xorg.conf
        echo "entrypoint: no VKMS DRM device in container — using xorg.conf (dummy driver)"
    fi
fi

Xorg :99 -config "$XORG_CONF" &
for _ in $(seq 1 50); do
    xrandr --query >/dev/null 2>&1 && break
    sleep 0.2
done

# Switch the first connected output to WxH: one of its own modes when it has
# one (VKMS lists 1024x768, 1920x1080 and 4096x2160), else a new mode (the
# dummy driver accepts any timings).
set_screen_mode() {
    local mode=$1 w=${1%x*} h=${1#*x} out clock
    out=$(xrandr --query | awk '/ connected/ {print $1; exit}')
    if [[ -z "$out" ]]; then
        echo "entrypoint: no connected output on $DISPLAY" >&2
        return 1
    fi
    if ! xrandr --output "$out" --mode "$mode" 2>/dev/null; then
        clock=$(( (w + 160) * (h + 30) * 60 / 1000000 + 1 ))
        xrandr --newmode "gf_$mode" "$clock" \
            "$w" $((w + 48)) $((w + 80)) $((w + 160)) \
            "$h" $((h + 3)) $((h + 8)) $((h + 30))
        xrandr --addmode "$out" "gf_$mode"
        xrandr --output "$out" --mode "gf_$mode"
    fi
    echo "entrypoint: $out set to $mode for --x11-sink"
}

# --x11-sink draws a buffer the size of the root window, so the screen must
# have the size the test expects before the pattern starts. The variable keeps
# its DRM-direct name because tests and scenes set it; unset means 1024x768,
# the VKMS preferred mode the DRM-direct patterns painted at.
if [[ "${TEST_PATTERN:-}" == *--x11-sink* ]]; then
    set_screen_mode "${GHOSTFRAME_DRM_MODE:-1024x768}"
    # The DRM probe would read a VKMS scanout that X drawing does not reach.
    export GHOSTFRAME_X11_CAPTURE_ONLY=1
fi

# Paint root window — default is --solid-red, override with TEST_PATTERN env var
ghostframe-test-pattern ${TEST_PATTERN:---solid-red} &
sleep 1

# Start the server (captures X11 root, serves via WebTransport)
exec /usr/local/bin/ghostframe-server
````

Then: `bash -n tests/containers/test-server/entrypoint.sh` (expected: no output). The runtime image already has `xrandr` (`x11-xserver-utils`).

- [ ] **Step 10: Lint the workspace and commit**

```bash
just lint
git status --short
git add ghostframe-test-pattern/Cargo.toml ghostframe-test-pattern/src Cargo.lock \
  ghostframe-e2e/tests/e2e.rs ghostframe-e2e/tests/h264.rs ghostframe-e2e/tests/native_client.rs \
  ghostframe-e2e/src/harness/scroll_scene.rs ghostframe-bench/benches/README.md \
  ghostframe-client-gpu/README.md ci/skip-list.txt docs/ci.md tests/containers/test-server/entrypoint.sh
git commit -m "test(e2e): test patterns paint into Xorg through --x11-sink

--drm-direct made the test pattern DRM master because Xorg on VKMS
swallowed drawing; with X11 capture that workaround is unnecessary,
and every e2e test now exercises the production capture path. Tests
that set CAPTURE_FPS_DRM_DIRECT set CAPTURE_FPS; sink scenes default
to 30 fps as DRM-direct did.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

The e2e suite itself runs in Task 15.

---

### Task 5: Delete DRM capture and its knobs

**Files:**
- Delete: `ghostframe-server/src/drm_capture.rs`, `ghostframe-server/tests/drm_gpu_pipeline.rs`, `ghostframe-server/examples/writeback_probe.rs` (DRM-only; nothing references it)
- Modify: `ghostframe-server/Cargo.toml`, `ghostframe-server/src/main.rs`, `ghostframe-server/src/config.rs`, `tests/containers/test-server/entrypoint.sh`, `tests/containers/test-server/xorg-vkms.conf`

- [ ] **Step 1: Delete the files and the dependency**

```bash
git rm ghostframe-server/src/drm_capture.rs ghostframe-server/tests/drm_gpu_pipeline.rs ghostframe-server/examples/writeback_probe.rs
```

In `ghostframe-server/Cargo.toml` delete the line `drm = "0.15"` and replace the x11rb line with
`x11rb = { version = "0.13", features = ["damage", "xtest", "composite", "xfixes", "randr"] }`
(`dri3` was used by nothing; `composite` goes in Task 7 with its last user).

- [ ] **Step 2: `main.rs` captures with X11 only**

1. Delete `mod drm_capture;`.
2. `use ghostframe_lib::{FrameSubmission, GhostbridgeConfig, GhostframeServer};` → `use ghostframe_lib::{GhostbridgeConfig, GhostframeServer};`
3. Delete the `enum CaptureBackend { ... }` item and its doc comment (from `/// Capture backend selection.` through its closing `}`).
4. Replace
```rust
    // Skip in --init mode: that path only seeds the tsnet state and exits
    // without capturing anything, and install.sh runs --init before X is up.
    //
    // Also skip when GHOSTFRAME_NO_X11_WAIT is set — the DRM-direct e2e
    // path runs ghostframe-test-pattern as the DRM master with no Xorg
    // anywhere in the container, so blocking on a non-existent display :99
    // would just time out after 10 s and abort startup. The DRM-direct
    // entrypoint sets this env var; production never does.
    let skip_x11_wait = cfg.no_x11_wait;
    if !init_mode && !skip_x11_wait {
```
with
```rust
    // Skip in --init mode: that path only seeds the tsnet state and exits
    // without capturing anything, and install.sh runs --init before X is up.
    if !init_mode {
```
5. Delete everything from the line `    // Select capture backend. Default: try DRM first, fall back to X11. With` up to (not including) `    // Not armed yet: damage reporting is switched on and off with the capture`, and put there:
```rust
    let mut capture = x11_capture::X11Capture::new(&cfg)?;

```
6. In the loop, delete everything from `        let submission = match &mut backend {` up to (not including) `        let capture_done = std::time::Instant::now();`, and put there:
```rust
        let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;
        let submission = match capture.capture(timestamp_us) {
            Ok(mut frame) => {
                frame.damage_tiles = damage_tiles;
                Some(frame)
            }
            Err(e) => {
                tracing::warn!("X11 capture failed: {e}");
                None
            }
        };

```

- [ ] **Step 3: Remove `GHOSTFRAME_NO_X11_WAIT` and `GHOSTFRAME_X11_CAPTURE_ONLY`**

In `ghostframe-server/src/config.rs`:
- delete the two table rows `` //! | `GHOSTFRAME_NO_X11_WAIT` | ... `` and `` //! | `GHOSTFRAME_X11_CAPTURE_ONLY` | ... ``;
- delete the fields `    pub no_x11_wait: bool,` and `    pub x11_capture_only: bool,`;
- delete their two initialiser lines in `from_lookup` (`no_x11_wait: env_flag(...)`, `x11_capture_only: env_flag(...)`);
- in `fn defaults`: `assert!(!c.log_json && !c.no_x11_wait && !c.x11_capture_only);` → `assert!(!c.log_json);`
- in `fn flags_and_paths` the body becomes
```rust
        let c = cfg(&[
            ("GHOSTFRAME_CAPTURE_DUMP_DIR", ""),
            ("GHOSTFRAME_LOG_FORMAT", "json"),
        ]);
        assert!(c.log_json);
        assert!(c.capture_dump_dir.is_none());
```

- [ ] **Step 4: Entrypoint and the VKMS Xorg config**

In `tests/containers/test-server/entrypoint.sh`:
- delete the two lines (inside the `--x11-sink` block) `    # The DRM probe would read a VKMS scanout that X drawing does not reach.` and `    export GHOSTFRAME_X11_CAPTURE_ONLY=1`;
- replace the whole `# If XORG_CONF is unset, auto-detect the right config:` comment and its `if [ -z "${XORG_CONF:-}" ]; then ... fi` block with
```bash
# The dummy driver unless the harness names a config: its GPU setups pass
# XORG_CONF=/etc/X11/xorg-vkms.conf explicitly.
XORG_CONF=${XORG_CONF:-/etc/X11/xorg.conf}
echo "entrypoint: Xorg config $XORG_CONF"
```
The auto-detection only fired when `/dev/dri` was mounted without `XORG_CONF`, which no harness setup does (`e2e_setup.rs` and `scene.rs` set it whenever they mount `/dev/dri`).

In `tests/containers/test-server/xorg-vkms.conf`, replace
```
# do NOT want to grab). The Virtual-1 connector that VKMS exposes lets
# Xorg attach a framebuffer; drm_capture::capture_prime_fd() reads it
# back via PRIME DMA-BUF for the GPU pipeline.
```
with
```
# do NOT want to grab). The Virtual-1 connector that VKMS exposes lets
# Xorg attach a framebuffer. The server captures over X11 (root window),
# not from this framebuffer; the harness's GPU setups use this config so
# the container's Xorg runs on the VKMS device they bind-mount.
```

The finished entrypoint:

````bash
#!/bin/bash
set -euo pipefail

until curl -fsS http://headscale:8080/health >/dev/null 2>&1; do
    echo "waiting for headscale..."
    sleep 1
done

export TS_HOSTNAME=${TS_HOSTNAME:-ghostframe-server}
export TS_STATE_DIR=${TS_STATE_DIR:-/tmp/ghostframe-ts}
export TS_CONTROL_URL=${TS_CONTROL_URL:-http://headscale:8080}
export DISPLAY=:99

# --x11-sink patterns repaint at up to 60 Hz, and the tests built on them were
# tuned at 30 fps capture (the old DRM-direct default). Everything else keeps
# 2 fps. A CAPTURE_FPS set by the test wins either way.
if [[ "${TEST_PATTERN:-}" == *--x11-sink* ]]; then
    export CAPTURE_FPS=${CAPTURE_FPS:-30}
else
    export CAPTURE_FPS=${CAPTURE_FPS:-2}
fi

# M3.7b: apply tc qdisc shaping if SHAPE_* env vars set.
# netem provides delay + loss; tbf provides bandwidth rate-limit.
# Failures log warnings and continue without shaping (better than
# crashing the bench; downstream detects via the resulting data shape).
if [[ -n "${SHAPE_BANDWIDTH_KBPS:-}" ]]; then
    DELAY_MS="${SHAPE_DELAY_MS:-0}"
    LOSS_PCT="${SHAPE_LOSS_PCT:-0}"
    echo "entrypoint: applying tc shaping — ${SHAPE_BANDWIDTH_KBPS}kbit, ${DELAY_MS}ms delay, ${LOSS_PCT}% loss"
    if tc qdisc add dev eth0 root handle 1: netem delay "${DELAY_MS}ms" loss "${LOSS_PCT}%" 2>&1; then
        if tc qdisc add dev eth0 parent 1:1 handle 10: tbf rate "${SHAPE_BANDWIDTH_KBPS}kbit" burst 32kbit latency 400ms 2>&1; then
            echo "entrypoint: tc shaping applied successfully"
        else
            echo "entrypoint: WARNING — tbf qdisc add failed; rolling back netem to run unshaped"
            tc qdisc del dev eth0 root 2>&1 || true
        fi
    else
        echo "entrypoint: WARNING — tc shaping unavailable (netem add failed; container needs --privileged or NET_ADMIN cap)"
    fi
fi

# The dummy driver unless the harness names a config: its GPU setups pass
# XORG_CONF=/etc/X11/xorg-vkms.conf explicitly.
XORG_CONF=${XORG_CONF:-/etc/X11/xorg.conf}
echo "entrypoint: Xorg config $XORG_CONF"

Xorg :99 -config "$XORG_CONF" &
for _ in $(seq 1 50); do
    xrandr --query >/dev/null 2>&1 && break
    sleep 0.2
done

# Switch the first connected output to WxH: one of its own modes when it has
# one (VKMS lists 1024x768, 1920x1080 and 4096x2160), else a new mode (the
# dummy driver accepts any timings).
set_screen_mode() {
    local mode=$1 w=${1%x*} h=${1#*x} out clock
    out=$(xrandr --query | awk '/ connected/ {print $1; exit}')
    if [[ -z "$out" ]]; then
        echo "entrypoint: no connected output on $DISPLAY" >&2
        return 1
    fi
    if ! xrandr --output "$out" --mode "$mode" 2>/dev/null; then
        clock=$(( (w + 160) * (h + 30) * 60 / 1000000 + 1 ))
        xrandr --newmode "gf_$mode" "$clock" \
            "$w" $((w + 48)) $((w + 80)) $((w + 160)) \
            "$h" $((h + 3)) $((h + 8)) $((h + 30))
        xrandr --addmode "$out" "gf_$mode"
        xrandr --output "$out" --mode "gf_$mode"
    fi
    echo "entrypoint: $out set to $mode for --x11-sink"
}

# --x11-sink draws a buffer the size of the root window, so the screen must
# have the size the test expects before the pattern starts. The variable keeps
# its DRM-direct name because tests and scenes set it; unset means 1024x768,
# the VKMS preferred mode the DRM-direct patterns painted at.
if [[ "${TEST_PATTERN:-}" == *--x11-sink* ]]; then
    set_screen_mode "${GHOSTFRAME_DRM_MODE:-1024x768}"
fi

# Paint root window — default is --solid-red, override with TEST_PATTERN env var
ghostframe-test-pattern ${TEST_PATTERN:---solid-red} &
sleep 1

# Start the server (captures X11 root, serves via WebTransport)
exec /usr/local/bin/ghostframe-server capture
````

(except that its last line still reads `exec /usr/local/bin/ghostframe-server`, without ` capture`, until Task 11).

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all
bash -n tests/containers/test-server/entrypoint.sh
git grep -n "NO_X11_WAIT\|X11_CAPTURE_ONLY\|drm_capture\|CaptureBackend" -- ghostframe-server tests/containers ci .github Justfile
just lint
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins
cargo test -p ghostframe-server --test init_flag
git status --short
git add ghostframe-server/Cargo.toml ghostframe-server/src/main.rs ghostframe-server/src/config.rs Cargo.lock \
  tests/containers/test-server/entrypoint.sh tests/containers/test-server/xorg-vkms.conf
git commit -m "refactor(server): delete DRM capture

X11 is the one capture method. Removes drm_capture.rs, the drm
dependency, the DRM pipeline test, writeback_probe, and the
GHOSTFRAME_NO_X11_WAIT / GHOSTFRAME_X11_CAPTURE_ONLY knobs that only
existed to steer between DRM and X11.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

Expected: the grep prints nothing; lint clean; tests pass. The `git rm` of Step 1 is already staged.

---

### Task 6: `Backend` trait and the `capture/x11/` layout

A move plus one seam. No behaviour change: the per-window path is still there until Task 7.

**Files:**
- Create: `ghostframe-server/src/capture/mod.rs`, `ghostframe-server/src/capture/x11/mod.rs`
- Move: `src/x11_capture.rs` → `src/capture/x11/frames.rs`, `src/xdamage.rs` → `src/capture/x11/damage.rs`, `src/input_inject.rs` → `src/capture/x11/input.rs`, `src/display.rs` → `src/capture/x11/display.rs`, `src/cvt.rs` → `src/capture/x11/cvt.rs`, `src/randr_events.rs` → `src/capture/x11/randr_events.rs`
- Modify: `ghostframe-server/src/main.rs`, `ghostframe-server/src/capture/x11/display.rs`, `ghostframe-server/examples/{cvt_sweep,randr_probe}.rs`

- [ ] **Step 1: Move the files**

```bash
cd ghostframe-server
mkdir -p src/capture/x11
git mv src/x11_capture.rs src/capture/x11/frames.rs
git mv src/xdamage.rs src/capture/x11/damage.rs
git mv src/input_inject.rs src/capture/x11/input.rs
git mv src/display.rs src/capture/x11/display.rs
git mv src/cvt.rs src/capture/x11/cvt.rs
git mv src/randr_events.rs src/capture/x11/randr_events.rs
sed -i 's/^use crate::cvt;$/use super::cvt;/' src/capture/x11/display.rs
sed -i 's#"../src/cvt.rs"#"../src/capture/x11/cvt.rs"#' examples/cvt_sweep.rs examples/randr_probe.rs
sed -i 's#"../src/display.rs"#"../src/capture/x11/display.rs"#; s#"../src/randr_events.rs"#"../src/capture/x11/randr_events.rs"#' examples/randr_probe.rs
sed -i 's#before touching the arithmetic in `../src/cvt.rs`:#before touching the arithmetic in `../src/capture/x11/cvt.rs`:#' examples/cvt_sweep.rs
cd -
```

`display.rs` uses `super::cvt` so the same file compiles both as `capture::x11::display` and as the example's crate-root `display` module (where `super` is the crate root and the example declares `mod cvt`).

- [ ] **Step 2: The trait**

Create `ghostframe-server/src/capture/mod.rs`:

````rust
//! Capture backends.
//!
//! A backend bundles everything `GhostframeServer` consumes from the desktop
//! it serves: frames, an input injector and an optional display controller.
//! X11 is the only one today; a PipeWire/Wayland backend implements the same
//! trait later (design: `docs/superpowers/specs/2026-10-29-ghostframe-server-x11-design.md`).

pub mod x11;

use std::sync::Arc;

use ghostframe_lib::transport::display::DisplayController;
use ghostframe_lib::transport::input_inject::InputInjector;
use ghostframe_lib::FrameSubmission;

/// One desktop to serve.
pub trait Backend {
    /// Input injection, or `None` if the desktop offers none (frames still
    /// stream; input is dropped).
    fn input_injector(&self) -> Option<Arc<dyn InputInjector>>;

    /// Resolution control, or `None` (the session keeps its size).
    fn display_controller(&self) -> Option<Arc<dyn DisplayController>>;

    /// Grab one frame. `damage_tiles` is left `None`; the capture loop fills
    /// it from [`Backend::drain_damage`].
    fn capture(&mut self, timestamp_us: u32) -> std::io::Result<FrameSubmission>;

    /// Start damage reporting (a client connected). No-op without support.
    fn arm_damage(&mut self);

    /// Stop damage reporting (no client). No-op without support.
    fn disarm_damage(&mut self);

    /// Whether damage reporting is live: the one operator-visible sign that
    /// it has not silently failed.
    fn damage_armed(&self) -> bool;

    /// Tiles damaged since the last call, or `None` for "unknown: check
    /// every tile" (no damage support, or no event since the last call).
    fn drain_damage(&mut self) -> Option<Vec<(u32, u32)>>;
}
````

Create `ghostframe-server/src/capture/x11/mod.rs`:

````rust
//! The X11 backend: full-root frames, XDamage, XTest input and RandR.

pub mod cvt;
pub mod damage;
pub mod display;
pub mod frames;
pub mod input;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostframe_lib::transport::display::DisplayController;
use ghostframe_lib::transport::input_inject::InputInjector;
use ghostframe_lib::FrameSubmission;

use crate::config::XdaemonConfig;

/// The X11 desktop on `$DISPLAY`.
pub struct X11Backend {
    frames: frames::X11Capture,
    damage: Option<damage::XDamageMonitor>,
    input: Option<Arc<dyn InputInjector>>,
    display: Option<Arc<dyn DisplayController>>,
}

impl X11Backend {
    /// Connect every part. Frames are required; damage, input and display
    /// control are each optional and logged when missing, so a desktop that
    /// lacks one still streams.
    pub fn new(cfg: &XdaemonConfig) -> io::Result<Self> {
        let frames = frames::X11Capture::new(cfg)?;

        // Not armed yet: damage reporting is switched on and off with the
        // capture gate, so the X server does no damage bookkeeping for us
        // while no client is connected.
        let damage = damage::XDamageMonitor::new();
        if damage.is_some() {
            tracing::info!("XDamage available; armed only while a client is connected");
        } else {
            tracing::info!("XDamage not available, using full-frame dirty detection");
        }

        // If the XTEST extension isn't compiled into this X server, log and
        // continue: frames still stream, just no input.
        let input: Option<Arc<dyn InputInjector>> = match input::XTestInjector::new(
            cfg.display.as_deref(),
        ) {
            Ok(inj) => {
                tracing::info!("XTest input injector ready");
                Some(Arc::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "XTest injector unavailable; input forwarding disabled");
                None
            }
        };

        // RandR is an ordinary, unprivileged X client operation. If it is
        // unavailable (extension missing, no connected output), continue with
        // no display controller: a fixed resolution beats refusing to start.
        let display: Option<Arc<dyn DisplayController>> =
            match display::XrandrDisplay::new(cfg.attach_max_resolution.clone()) {
                Ok(ctl) => {
                    tracing::info!("XRandR display controller ready");
                    Some(Arc::new(ctl))
                }
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "XRandR display controller unavailable; resolution negotiation disabled"
                    );
                    None
                }
            };

        Ok(Self {
            frames,
            damage,
            input,
            display,
        })
    }
}

impl super::Backend for X11Backend {
    fn input_injector(&self) -> Option<Arc<dyn InputInjector>> {
        self.input.clone()
    }

    fn display_controller(&self) -> Option<Arc<dyn DisplayController>> {
        self.display.clone()
    }

    fn capture(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        self.frames.capture(timestamp_us)
    }

    fn arm_damage(&mut self) {
        if let Some(d) = self.damage.as_mut() {
            d.arm();
        }
    }

    fn disarm_damage(&mut self) {
        if let Some(d) = self.damage.as_mut() {
            d.disarm();
        }
    }

    fn damage_armed(&self) -> bool {
        self.damage.as_ref().is_some_and(|d| d.is_armed())
    }

    fn drain_damage(&mut self) -> Option<Vec<(u32, u32)>> {
        // An empty drain means no damage events since the last call, but the
        // first frame for a newly connected client must still be compared in
        // full, so empty maps to `None` ("check every tile").
        let tiles = self.damage.as_ref()?.drain_damage();
        if tiles.is_empty() {
            None
        } else {
            Some(tiles)
        }
    }
}

/// Block until `$DISPLAY` accepts an X11 connection, or the timeout expires.
///
/// `After=graphical-session.target` says nothing about whether the user's
/// session will accept a new client. Without this gate the server would bring
/// up tsnet (a ~5 s operation) against an X server it cannot talk to, and the
/// only symptom downstream would be "no frames".
pub fn wait_for_x11(timeout: Duration, display: Option<&str>) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut attempt: u32 = 0;
    let last_err = loop {
        attempt += 1;
        let err = match x11rb::connect(None) {
            Ok(_) => {
                if attempt > 1 {
                    tracing::info!(attempts = attempt, "X11 display ready");
                }
                return Ok(());
            }
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            break err;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let display = display.unwrap_or("(unset)");
    Err(format!(
        "X11 display {display} not ready after {timeout:?} ({attempt} attempt(s)); \
         last error: {last_err}"
    ))
}
````

(`wait_for_x11` moves here from `main.rs` verbatim except its doc comment, which no longer mentions headless mode. `randr_events.rs` is not declared yet; Task 8 declares it for tests.)

- [ ] **Step 3: `main.rs` drives the backend**

1. Replace the module list and imports at the top
```rust
mod config;
mod cvt;
mod display;
mod input_inject;
mod util;
mod x11_capture;
mod xdamage;

use std::path::Path;
use std::time::{Duration, Instant};
```
with
```rust
mod capture;
mod config;
mod util;

use std::path::Path;
use std::time::Duration;

use capture::Backend;
```
2. Delete the `wait_for_x11` function and its doc comment (from the line
```rust
/// Block until `$DISPLAY` accepts an X11 connection, or the timeout expires.
```
to the line before `#[tokio::main]`), and change its call to `capture::x11::wait_for_x11(Duration::from_secs(10), cfg.display.as_deref())`.
3. Delete everything from `    // X server is up; safe to open the XTest injector connection.` up to (not including) `    let lib_config = match ghostframe_lib::config::LibConfig::from_env() {`, and put there:
```rust
    // The X server is up: connect the backend (frames, damage, input,
    // display). Not in --init mode, which exits before serving anything.
    let mut backend = if init_mode {
        None
    } else {
        Some(capture::x11::X11Backend::new(&cfg)?)
    };
    let input_injector = backend.as_ref().and_then(|b| b.input_injector());
    let display_controller = backend.as_ref().and_then(|b| b.display_controller());

```
4. Delete everything from `    let mut capture = x11_capture::X11Capture::new(&cfg)?;` up to (not including) `    tracing::info!("Server ready, entering capture loop");` (this removes the XDamage construction, now inside `X11Backend::new`), and put there:
```rust
    let Some(backend) = backend.as_mut() else {
        unreachable!("the backend is only None in --init mode, which returned above");
    };

```
5. In the loop:
   - Replace
```rust
                if let Some(monitor) = xdamage_monitor.as_mut() {
                    monitor.disarm();
                }
```
with
```rust
                backend.disarm_damage();
```
   - Replace
```rust
            if let Some(monitor) = xdamage_monitor.as_mut() {
                monitor.arm();
            }
```
with
```rust
            backend.arm_damage();
```
   - `                xdamage_armed = xdamage_monitor.as_ref().is_some_and(|m| m.is_armed()),` → `                xdamage_armed = backend.damage_armed(),`
   - delete from `        // XDamage drain: Some(non-empty) = only check these tiles, None = check all.` up to (not including) `        let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;`, and put there:
```rust
        // Some(tiles) = only these changed; None = check every tile.
        let damage_tiles = backend.drain_damage();

```
   - `        let submission = match capture.capture(timestamp_us) {` → `        let submission = match backend.capture(timestamp_us) {`

- [ ] **Step 4: Build, lint, test**

```bash
cargo fmt --all
just lint
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins
cargo test -p ghostframe-server --test init_flag
cargo build -p ghostframe-server --examples
```

Expected: clean; the moved tests (`capture::x11::damage::...`, `capture::x11::display::...`, `capture::x11::frames::...`) all pass under their new paths.

- [ ] **Step 5: Commit**

```bash
git status --short
git add ghostframe-server/src ghostframe-server/examples
git commit -m "refactor(server): capture::Backend trait and capture/x11 layout

The backend bundles what GhostframeServer consumes: frames, damage,
the input injector and the display controller. A PipeWire backend
implements the same trait later.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

---

### Task 7: One frame method: MIT-SHM full-root grab, `GetImage` fallback, cursor blended

Deletes `PerWindowComposite`, `pick_strategy`, `root_already_composited`, `fraction_matching`, `blit_window`, `window_is_offscreen` and their tests, the x11rb `composite` feature (no other user: `git grep -n composite ghostframe-server/src` is empty after this task) and `GHOSTFRAME_FORCE_ROOT_GET_IMAGE`. `blit_cursor` and the resize tests carry over; `blit_cursor` had no test, so it gets two.

**Files:**
- Rewrite: `ghostframe-server/src/capture/x11/frames.rs`
- Create: `ghostframe-server/src/capture/x11/test_support.rs`
- Modify: `ghostframe-server/src/capture/x11/mod.rs`, `ghostframe-server/Cargo.toml`, `ghostframe-server/src/config.rs`, `ghostframe-server/src/util.rs`

- [ ] **Step 1: The private-Xvfb test helper**

Create `ghostframe-server/src/capture/x11/test_support.rs`:

````rust
//! A private Xvfb per test.
//!
//! Tests that paint root, resize the screen or run without MIT-SHM must not
//! share a server: CI runs the crate's tests in parallel under one
//! `xvfb-run`, and a developer's `$DISPLAY` is a live session. Each
//! `PrivateXvfb` picks a free display itself (`-displayfd`) and is killed on
//! drop.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

pub struct PrivateXvfb {
    child: Child,
    display: String,
}

impl PrivateXvfb {
    /// Start `Xvfb -displayfd 1 -nolisten tcp -noreset <args>` and wait until
    /// it reports its display number. `-noreset`: Xvfb otherwise resets, and
    /// clears root, whenever its last client disconnects.
    pub fn spawn(args: &[&str]) -> Self {
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset"])
            .args(args)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn Xvfb (CI installs it through _setup-system-deps)");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("read the display number Xvfb prints");
        let n: u32 = line
            .trim()
            .parse()
            .unwrap_or_else(|_| panic!("Xvfb printed {line:?}, not a display number"));
        Self {
            child,
            display: format!(":{n}"),
        }
    }

    /// `":N"`, for `x11rb::connect(Some(..))`.
    pub fn display(&self) -> &str {
        &self.display
    }
}

impl Drop for PrivateXvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
````

In `capture/x11/mod.rs`, after `pub mod input;` add:
```rust
#[cfg(test)]
pub(crate) mod test_support;
```

- [ ] **Step 2: Rewrite `frames.rs` (tests included)**

Replace the whole of `ghostframe-server/src/capture/x11/frames.rs` with:

````rust
//! X11 frames: a full grab of the root window, plus the cursor.
//!
//! One method, the one `x11vnc` and ffmpeg's `x11grab` use: `ShmGetImage`
//! of the whole root into a MIT-SHM segment, falling back to
//! `GetImage(root)` when MIT-SHM is absent or the segment cannot be attached
//! (a remote display cannot share memory, and fd passing fails over TCP).
//!
//! There used to be a second method, per-window `CompositeNameWindowPixmap`
//! capture, chosen once at startup by probing whether the compositor painted
//! into root. On Enlightenment at autologin the probe was inconclusive (no
//! redirected root child of at least 64x64 existed yet), the per-window path
//! was chosen for the life of the process, and it composited nothing: the
//! client saw only the cursor. `GetImage(root)` on the same display returned
//! the full desktop. Root is what every X compositor this server targets
//! paints, so root is what is captured.
//!
//! The root path never drew the cursor (X does not include it in an image).
//! It is blended here from `XFixesGetCursorImage`, at `(x - xhot, y - yhot)`.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::util::monotonic_now_ns;
use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::protocol::shm::{self, ConnectionExt as ShmExt};
use x11rb::protocol::xfixes::{ConnectionExt as XfixesExt, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};
use x11rb::rust_connection::RustConnection;

/// Which request fills a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabPath {
    /// `ShmGetImage` into a shared segment.
    Shm,
    /// `GetImage(root)`: the pixels travel over the X socket.
    GetImage,
}

/// A memfd-backed MIT-SHM segment the X server writes root pixels into.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

impl ShmSegment {
    /// Create a `len`-byte memfd, map it, and attach it to the server
    /// writable (`ShmGetImage` writes into it).
    fn attach(conn: &RustConnection, len: usize) -> io::Result<Self> {
        // SAFETY: memfd_create takes a NUL-terminated name and flags only.
        let raw = unsafe { libc::memfd_create(c"ghostframe-shm".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: ftruncate on a descriptor we own.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: maps `len` bytes of a memfd just sized to `len`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Built before the attach so an error below still unmaps.
        let mut me = Self {
            seg: 0,
            ptr: ptr.cast(),
            len,
        };
        me.seg = conn.generate_id().map_err(io::Error::other)?;
        // x11rb sends the fd and closes our copy; the mapping keeps the memory.
        conn.shm_attach_fd(me.seg, fd, false)
            .map_err(io::Error::other)?
            .check()
            .map_err(io::Error::other)?;
        Ok(me)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps exactly `len` bytes for the life of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` come from a successful mmap, unmapped once.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// Captures full-root frames from one X display.
pub struct X11Frames {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    /// The segment for the current root size; `None` means `GetImage`.
    shm: Option<ShmSegment>,
    /// False once MIT-SHM is known not to work on this connection, so a
    /// resize does not retry (and re-log) a failing attach every time.
    shm_usable: bool,
    /// One-shot raw-frame dump target (`GHOSTFRAME_DUMP_FIRST_FRAME`); taken on first use.
    dump_first_frame: Option<String>,
}

impl X11Frames {
    /// Connect to `display` (`None` = `$DISPLAY`).
    pub fn connect(display: Option<&str>, dump_first_frame: Option<String>) -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(display)
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);

        // XFixes needs its version negotiated before GetCursorImage. Without
        // it the frame is still correct, minus the cursor.
        match conn.xfixes_query_version(5, 0).map(|c| c.reply()) {
            Ok(Ok(_)) => {}
            _ => tracing::warn!("XFIXES unavailable; frames will not include the cursor"),
        }

        let shm_usable = shm_supported(&conn);
        let mut me = Self {
            conn,
            root,
            width,
            height,
            shm: None,
            shm_usable,
            dump_first_frame,
        };
        me.ensure_segment();
        tracing::info!(width, height, path = ?me.path(), "X11 capture connected to display");
        Ok(me)
    }

    /// How frames are grabbed right now.
    pub fn path(&self) -> GrabPath {
        if self.shm.is_some() {
            GrabPath::Shm
        } else {
            GrabPath::GetImage
        }
    }

    /// Make the segment match the root size, or drop to `GetImage` for good
    /// if MIT-SHM cannot be used.
    fn ensure_segment(&mut self) {
        if !self.shm_usable {
            return;
        }
        let len = usize::from(self.width) * usize::from(self.height) * 4;
        if self.shm.as_ref().is_some_and(|s| s.len == len) {
            return;
        }
        if let Some(old) = self.shm.take() {
            let _ = self.conn.shm_detach(old.seg);
        }
        match ShmSegment::attach(&self.conn, len) {
            Ok(s) => self.shm = Some(s),
            Err(e) => {
                tracing::warn!(error = %e, "MIT-SHM segment attach failed; using GetImage(root)");
                self.shm_usable = false;
            }
        }
    }

    /// Poll the root window's current geometry and, if it changed since the
    /// last capture, adopt it and resize the segment.
    ///
    /// Polled once per captured frame rather than driven by
    /// `RRScreenChangeNotify`: one `GetGeometry` round trip next to the grab
    /// is negligible, and unlike an event subscription it cannot miss a
    /// notification or need a second connection to listen on.
    fn refresh_geometry(&mut self) -> io::Result<()> {
        let geom = self
            .conn
            .get_geometry(self.root)
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;
        if let Some((w, h)) = resize_needed((self.width, self.height), (geom.width, geom.height)) {
            tracing::info!(
                old_width = self.width,
                old_height = self.height,
                new_width = w,
                new_height = h,
                "X11 root geometry changed; resizing capture target"
            );
            self.width = w;
            self.height = h;
            self.ensure_segment();
        }
        Ok(())
    }

    /// The root pixels, BGRX, `width * 4` per row, without the cursor.
    fn grab(&mut self) -> io::Result<Vec<u8>> {
        let (w, h) = (self.width, self.height);
        let want = usize::from(w) * usize::from(h) * 4;
        if let Some(seg) = &self.shm {
            let reply = self
                .conn
                .shm_get_image(
                    self.root,
                    0,
                    0,
                    w,
                    h,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    seg.seg,
                    0,
                )
                .map_err(io::Error::other)?
                .reply()
                .map_err(io::Error::other)?;
            if reply.size as usize != want {
                return Err(io::Error::other(format!(
                    "ShmGetImage returned {} bytes for {w}x{h}, expected {want} (root is not 32 bpp)",
                    reply.size
                )));
            }
            return Ok(seg.bytes()[..want].to_vec());
        }
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;
        Ok(reply.data)
    }

    /// Capture a single frame: root, then the cursor on top.
    pub fn capture(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        self.refresh_geometry()?;
        let mut pixels = self.grab()?;
        // Best effort: without XFixes the frame is correct minus the cursor.
        if let Ok(cookie) = self.conn.xfixes_get_cursor_image() {
            if let Ok(cursor) = cookie.reply() {
                blit_cursor(
                    &mut pixels,
                    i32::from(self.width),
                    i32::from(self.height),
                    &cursor,
                );
            }
        }
        let capture_done_ns = monotonic_now_ns();
        let width = u32::from(self.width);
        let height = u32::from(self.height);
        let frame = FrameSubmission {
            width,
            height,
            stride: width * 4,
            pixels,
            dmabuf_fd: None,
            timestamp_us,
            damage_tiles: None,
            capture_done_ns,
        };
        // One-shot diagnostic: dump the first frame to the path in
        // `GHOSTFRAME_DUMP_FIRST_FRAME`, to byte-compare against `xwd -root`.
        if let Some(path) = self.dump_first_frame.take() {
            let header = format!(
                "ghostframe-bgra width={} height={} stride={}\n",
                frame.width, frame.height, frame.stride
            );
            let _ = std::fs::write(format!("{path}.meta"), header.as_bytes());
            let _ = std::fs::write(&path, &frame.pixels);
            tracing::info!(
                path = %path,
                width = frame.width,
                height = frame.height,
                bytes = frame.pixels.len(),
                "GHOSTFRAME_DUMP_FIRST_FRAME: dumped raw BGRA to disk"
            );
        }
        Ok(frame)
    }
}

/// MIT-SHM 1.2 (`AttachFd`) is present on this connection.
fn shm_supported(conn: &RustConnection) -> bool {
    match conn.shm_query_version().map(|c| c.reply()) {
        Ok(Ok(v)) => (v.major_version, v.minor_version) >= (1, 2),
        _ => false,
    }
}

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

/// Alpha-blend the cursor onto the target framebuffer.
///
/// `XFixesGetCursorImage` returns 32-bit pre-multiplied ARGB pixels
/// (per spec); on little-endian platforms those bytes are BGRA in
/// memory, with the alpha channel ALREADY pre-multiplied into the
/// color channels. So the standard "over" composite simplifies to:
///
///   out = src + dst * (1 - src.a)
///
/// applied per channel.
fn blit_cursor(target: &mut [u8], target_w: i32, target_h: i32, cursor: &GetCursorImageReply) {
    let cx = (cursor.x as i32) - (cursor.xhot as i32);
    let cy = (cursor.y as i32) - (cursor.yhot as i32);
    let cw = cursor.width as i32;
    let ch = cursor.height as i32;

    let x0 = cx.max(0);
    let y0 = cy.max(0);
    let x1 = (cx + cw).min(target_w);
    let y1 = (cy + ch).min(target_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let src_off_x = (x0 - cx) as usize;
    let src_off_y = (y0 - cy) as usize;
    let copy_w = (x1 - x0) as usize;
    let copy_h = (y1 - y0) as usize;

    let target_stride = (target_w as usize) * 4;

    for row in 0..copy_h {
        for col in 0..copy_w {
            let src_idx = (src_off_y + row) * (cw as usize) + (src_off_x + col);
            if src_idx >= cursor.cursor_image.len() {
                continue;
            }
            let argb = cursor.cursor_image[src_idx];
            // ARGB unpacking; on little-endian X11 reports in
            // host byte order, so bits are A R G B from MSB → LSB.
            let a = (argb >> 24) & 0xff;
            let r = (argb >> 16) & 0xff;
            let g = (argb >> 8) & 0xff;
            let b = argb & 0xff;
            if a == 0 {
                continue; // fully transparent pixel — nothing to do
            }

            let dst_idx = ((y0 as usize) + row) * target_stride + ((x0 as usize) + col) * 4;
            let inv_a = 255 - a;
            // Target is BGRA. Source channels are already pre-multiplied
            // by alpha (XFixes guarantee), so we just add + scale dst.
            let db = target[dst_idx] as u32;
            let dg = target[dst_idx + 1] as u32;
            let dr = target[dst_idx + 2] as u32;
            target[dst_idx] = (b + (db * inv_a + 127) / 255).min(255) as u8;
            target[dst_idx + 1] = (g + (dg * inv_a + 127) / 255).min(255) as u8;
            target[dst_idx + 2] = (r + (dr * inv_a + 127) / 255).min(255) as u8;
            target[dst_idx + 3] = 0xff;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_changed_root_geometry_requires_a_resize() {
        assert_eq!(resize_needed((640, 480), (1280, 800)), Some((1280, 800)));
    }

    #[test]
    fn an_unchanged_geometry_does_not() {
        // This runs once per captured frame. Reallocating a multi-megabyte
        // buffer every frame because the check was sloppy would be a worse
        // bug than the one being fixed.
        assert_eq!(resize_needed((1280, 800), (1280, 800)), None);
    }

    #[test]
    fn a_zero_geometry_is_ignored() {
        // X can briefly report 0 during a mode transition. Resizing to it
        // would yield empty frames or panic on a zero-sized buffer.
        assert_eq!(resize_needed((1280, 800), (0, 0)), None);
        assert_eq!(resize_needed((1280, 800), (1280, 0)), None);
    }

    /// A 3x2 opaque white cursor with its hotspot at (1, 1), pointer at
    /// (5, 4): the image's top-left lands at (4, 3), nowhere else changes.
    #[test]
    fn blit_cursor_places_the_image_at_pointer_minus_hotspot() {
        let (w, h) = (10i32, 8i32);
        let mut target = vec![0u8; (w * h * 4) as usize];
        let cursor = GetCursorImageReply {
            x: 5,
            y: 4,
            width: 3,
            height: 2,
            xhot: 1,
            yhot: 1,
            cursor_image: vec![0xffff_ffff; 6],
            ..Default::default()
        };
        blit_cursor(&mut target, w, h, &cursor);
        for y in 0..h {
            for x in 0..w {
                let px = &target[((y * w + x) * 4) as usize..][..4];
                let inside = (4..7).contains(&x) && (3..5).contains(&y);
                let want: &[u8] = if inside { &[255; 4] } else { &[0; 4] };
                assert_eq!(px, want, "pixel ({x},{y})");
            }
        }
    }

    #[test]
    fn blit_cursor_blends_premultiplied_alpha() {
        // 50% black over white: out = 0 + 255 * (255 - 128) / 255 = 127.
        let mut target = vec![255u8; 4];
        let cursor = GetCursorImageReply {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            cursor_image: vec![0x8000_0000],
            ..Default::default()
        };
        blit_cursor(&mut target, 1, 1, &cursor);
        assert_eq!(target, [127, 127, 127, 255]);
    }
}

#[cfg(test)]
mod live_tests {
    //! Against a private Xvfb (`test_support::PrivateXvfb`), so painting
    //! root or disabling MIT-SHM never touches another test or a session.
    use super::*;
    use crate::capture::x11::test_support::PrivateXvfb;
    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, CreateGCAux};

    const W: u16 = 64;
    const H: u16 = 48;

    /// Paint a deterministic, non-uniform image into root.
    fn paint_root(display: &str) {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let screen = &conn.setup().roots[n];
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, screen.root, &CreateGCAux::new())
            .unwrap();
        let mut data = vec![0u8; usize::from(W) * usize::from(H) * 4];
        for (i, px) in data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % usize::from(W), i / usize::from(W));
            px.copy_from_slice(&[(x * 7) as u8, (y * 5) as u8, (x ^ y) as u8, 0]);
        }
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            gc,
            W,
            H,
            0,
            0,
            0,
            screen.root_depth,
            &data,
        )
        .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();
    }

    fn root_get_image(display: &str) -> Vec<u8> {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let s = &conn.setup().roots[n];
        conn.get_image(ImageFormat::Z_PIXMAP, s.root, 0, 0, W, H, !0)
            .unwrap()
            .reply()
            .unwrap()
            .data
    }

    #[test]
    fn shm_grab_is_byte_identical_to_get_image() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        paint_root(x.display());
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        assert_eq!(f.path(), GrabPath::Shm);
        let shm = f.grab().unwrap();
        assert!(shm.iter().any(|&b| b != 0), "root was not painted");
        assert_eq!(shm, root_get_image(x.display()));
    }

    #[test]
    fn falls_back_to_get_image_without_mit_shm() {
        let x = PrivateXvfb::spawn(&["-extension", "MIT-SHM", "-screen", "0", "64x48x24"]);
        paint_root(x.display());
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        assert_eq!(f.path(), GrabPath::GetImage);
        let frame = f.capture(0).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (64, 48, 256));
        assert_eq!(f.grab().unwrap(), root_get_image(x.display()));
    }

    /// The live cursor (a glyph from the server's built-in cursor font) is
    /// drawn into the frame, and only inside its rectangle at the hotspot.
    #[test]
    fn capture_draws_the_cursor_at_its_hotspot() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let (conn, n) = x11rb::connect(Some(x.display())).unwrap();
        let root = conn.setup().roots[n].root;
        let font = conn.generate_id().unwrap();
        conn.open_font(font, b"cursor").unwrap();
        let cursor = conn.generate_id().unwrap();
        // XC_crosshair (34) with its mask (35): white on black.
        conn.create_glyph_cursor(cursor, font, font, 34, 35, 0, 0, 0, 0xffff, 0xffff, 0xffff)
            .unwrap();
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().cursor(cursor))
            .unwrap();
        conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 30, 20)
            .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();

        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        let plain = f.grab().unwrap();
        let frame = f.capture(0).unwrap();
        let cur = f.conn.xfixes_get_cursor_image().unwrap().reply().unwrap();
        let (cx, cy) = (
            i32::from(cur.x) - i32::from(cur.xhot),
            i32::from(cur.y) - i32::from(cur.yhot),
        );
        let mut changed = 0;
        for (i, (a, b)) in plain
            .chunks_exact(4)
            .zip(frame.pixels.chunks_exact(4))
            .enumerate()
        {
            if a[..3] != b[..3] {
                changed += 1;
                let (px, py) = ((i % 64) as i32, (i / 64) as i32);
                assert!(
                    (cx..cx + i32::from(cur.width)).contains(&px)
                        && (cy..cy + i32::from(cur.height)).contains(&py),
                    "pixel ({px},{py}) changed outside the cursor at ({cx},{cy})"
                );
            }
        }
        assert!(changed > 0, "the cursor was not drawn");
    }
}
````

- [ ] **Step 3: Wire it and drop the dead knob and feature**

- `capture/x11/mod.rs`: `    frames: frames::X11Capture,` → `    frames: frames::X11Frames,`; `        let frames = frames::X11Capture::new(cfg)?;` → `        let frames = frames::X11Frames::connect(None, cfg.dump_first_frame.clone())?;`
- `ghostframe-server/Cargo.toml`: `features = ["damage", "xtest", "composite", "xfixes", "randr"]` → `features = ["damage", "xtest", "xfixes", "randr", "shm"]`
- `config.rs`: delete the row `` //! | `GHOSTFRAME_FORCE_ROOT_GET_IMAGE` | bypass compositor probe | off | ``, the field `    pub force_root_get_image: bool,`, its initialiser `            force_root_get_image: env_flag(get("GHOSTFRAME_FORCE_ROOT_GET_IMAGE").as_deref()),`, and `use crate::util::env_flag;` (and the blank line after it).
- `util.rs`: `env_flag` has no caller left. Delete the function (its doc comment `/// Interpret an environment value as a boolean flag: ...` through its closing `}`) and the test `fn env_flag_truth_table` with its `#[test]`.

- [ ] **Step 4: Run the tests**

Run: `xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins frames`
Expected: 8 pass: `a_changed_root_geometry_requires_a_resize`, `an_unchanged_geometry_does_not`, `a_zero_geometry_is_ignored`, `blit_cursor_places_the_image_at_pointer_minus_hotspot`, `blit_cursor_blends_premultiplied_alpha`, `shm_grab_is_byte_identical_to_get_image`, `falls_back_to_get_image_without_mit_shm`, `capture_draws_the_cursor_at_its_hotspot`.

- [ ] **Step 5: Break each new test, watch it fail, restore**

Run the Step 4 command after each change, then undo the change:
1. `shm_supported`: `>= (1, 2)` → `>= (9, 9)`. `shm_grab_is_byte_identical_to_get_image` fails on `assert_eq!(f.path(), GrabPath::Shm)`.
2. `grab`, GetImage branch: `Ok(reply.data)` → `Ok(vec![0; want])`. `falls_back_to_get_image_without_mit_shm` fails (bytes differ from `GetImage`); the SHM test still passes, showing the two paths are tested separately.
3. `blit_cursor`: `let cx = (cursor.x as i32) - (cursor.xhot as i32);` → `let cx = cursor.x as i32;`. `blit_cursor_places_the_image_at_pointer_minus_hotspot` fails at `pixel (4,3)`; `capture_draws_the_cursor_at_its_hotspot` fails with "changed outside the cursor".
4. `blit_cursor`: `let inv_a = 255 - a;` → `let inv_a = 0;`. `blit_cursor_blends_premultiplied_alpha` fails (`[0, 0, 0, 255]`).

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --all
git grep -n "composite\|FORCE_ROOT_GET_IMAGE\|PerWindowComposite\|pick_strategy\|env_flag" -- ghostframe-server
just lint
git add ghostframe-server/Cargo.toml Cargo.lock ghostframe-server/src/capture/x11/frames.rs \
  ghostframe-server/src/capture/x11/test_support.rs ghostframe-server/src/capture/x11/mod.rs \
  ghostframe-server/src/config.rs ghostframe-server/src/util.rs
git commit -m "feat(server): X11 frames are a MIT-SHM root grab with the cursor

One method, chosen per connection, never by a startup probe: ShmGetImage
of root, GetImage(root) when MIT-SHM is absent or cannot attach. The
XFixes cursor is blended at its hotspot; the root path never drew it.
Deletes the per-window composite path that captured only a cursor on
Enlightenment at autologin, and GHOSTFRAME_FORCE_ROOT_GET_IMAGE.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

Expected: the grep prints nothing.

---

### Task 8: RandR changes as `xrandr` makes them

Spec section 4: prefer an existing output mode (CVT only otherwise) and order the requests as `xrandr` does. The order is the one recorded on Xvfb: grab, disable the CRTC, set the screen size, set the CRTC, ungrab, for a grow and a shrink alike. A scale-only change (same mode, same size) only resizes the screen, so the monitor never blanks for nothing. If the disable step fails half way, the CRTC is put back. Apply Steps 8b/8c only if Task 1's outcome says so.

**Files:**
- Rewrite: `ghostframe-server/src/capture/x11/display.rs`
- Modify: `ghostframe-server/src/capture/x11/mod.rs`, `.github/workflows/_setup-system-deps/packages-build.txt`

- [ ] **Step 1: Make xrandr available to CI's tests**

In `.github/workflows/_setup-system-deps/packages-build.txt`, after the line `xvfb` add:
```
# xrandr: the RandR tests compare a server-initiated mode change with the
# event stream `xrandr --output O --mode M` produces on the same Xvfb.
x11-xserver-utils
```
(Editing this file also starts a fresh apt cache key, by design.) Locally: `command -v xrandr`.

- [ ] **Step 2: Declare the recorder for tests**

In `capture/x11/mod.rs`, directly above the two lines
```rust
#[cfg(test)]
pub(crate) mod test_support;
```
add:
```rust
#[cfg(test)]
pub(crate) mod randr_events;
```

- [ ] **Step 3: Rewrite `display.rs`**

Replace the whole of `ghostframe-server/src/capture/x11/display.rs` with the following. Against the old file: `new` delegates to a new `connect(display, max)` and negotiates RandR 1.5; `set_output` picks a mode with `pick_existing_mode`, creates one with `create_cvt_mode` (the old creation code, moved) only when none fits, and applies `apply_steps`; `ResizeStep`/`resize_steps` and their five tests are gone; unit tests for `apply_steps` and `pick_existing_mode` and a `live_tests` module are added; `ceiling`, `clamp_ceiling`, `mm_for_scale`, `find_mode_by_name` and their tests are unchanged.

````rust
//! `DisplayController` backed by X RandR.
//!
//! Implements `ghostframe_lib::transport::display::DisplayController`
//! (mirrors `input_inject::XTestInjector`, which implements `InputInjector`
//! for the same reason: keep the protocol/`IoBridge` layer testable without
//! a running display server). Everything X-specific -- RandR requests,
//! modelines, millimetres -- stays inside this module; the trait itself
//! must not gain an X concept. See the M4b design doc §5 and §7.

use std::sync::Mutex;

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::rust_connection::RustConnection;

use ghostframe_lib::transport::display::{DisplayController, DisplayError};

use super::cvt;

/// `DisplayController` backed by X RandR.
///
/// Entirely unprivileged: RandR is an ordinary X client operation and the
/// daemon already holds the display. An earlier design injected a synthetic
/// EDID through a root helper; a spike proved the kernel ignores
/// `edid_override` on these connectors, because it consults the override
/// only when a driver's `get_modes` returns zero modes and both VKMS and
/// amdgpu's `virtual_display` use `drm_add_modes_noedid`. See the M4b
/// design doc §10.
pub struct XrandrDisplay {
    conn: RustConnection,
    root: u32,
    output: randr::Output,
    crtc: randr::Crtc,
    /// Fallback for `ceiling()` if a live `GetScreenSizeRange` query ever
    /// fails. Seeded from a real query in `new()` and refreshed on every
    /// subsequent successful call -- never a hardcoded guess.
    ceiling_fallback: Mutex<(u16, u16)>,
    /// `GHOSTFRAME_ATTACH_MAX_RESOLUTION`, read once at construction (this
    /// crate's convention -- see `main.rs`). `Some("WIDTHxHEIGHT")` lowers
    /// what `ceiling()` reports; `None` (unset) leaves the hardware ceiling
    /// alone. Applied by `clamp_ceiling()` on every path through `ceiling()`.
    max_resolution: Option<String>,
}

impl XrandrDisplay {
    /// Connect to the local X server, confirm the RandR extension is
    /// present, and pick the first connected output with an available CRTC.
    ///
    /// Errors if no DISPLAY, RandR is missing, or there is no connected
    /// output -- the caller (ghostframe-server's `main`) logs and continues with no
    /// display controller, exactly like `XTestInjector::new`.
    pub fn new(max_resolution: Option<String>) -> anyhow::Result<Self> {
        Self::connect(None, max_resolution)
    }

    /// [`Self::new`] on an explicit display (`None` = `$DISPLAY`); the tests
    /// drive a private Xvfb through it.
    pub fn connect(display: Option<&str>, max_resolution: Option<String>) -> anyhow::Result<Self> {
        use anyhow::{anyhow, Context};

        let (conn, screen_num) =
            x11rb::connect(display).map_err(|e| anyhow!("x11rb::connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;

        let ext = conn
            .query_extension(b"RANDR")
            .context("query_extension(RANDR)")?
            .reply()
            .context("RANDR query_extension reply")?;
        if !ext.present {
            return Err(anyhow!("RandR extension not present on this Xorg build"));
        }
        // RandR 1.2+ requests need the version negotiated first.
        conn.randr_query_version(1, 5)
            .context("RandR QueryVersion request")?
            .reply()
            .context("RandR QueryVersion reply")?;

        let resources = conn
            .randr_get_screen_resources(root)
            .context("RandR GetScreenResources request")?
            .reply()
            .context("RandR GetScreenResources reply")?;
        if resources.outputs.is_empty() {
            return Err(anyhow!("RandR reports zero outputs"));
        }

        let mut chosen: Option<(randr::Output, randr::Crtc)> = None;
        for &output in &resources.outputs {
            let info = conn
                .randr_get_output_info(output, resources.config_timestamp)
                .context("RandR GetOutputInfo request")?
                .reply()
                .context("RandR GetOutputInfo reply")?;
            if info.connection != randr::Connection::CONNECTED {
                continue;
            }
            let crtc = if info.crtc != 0 {
                info.crtc
            } else if let Some(&c) = info.crtcs.first() {
                c
            } else {
                continue;
            };
            chosen = Some((output, crtc));
            break;
        }
        let (output, crtc) = chosen
            .ok_or_else(|| anyhow!("no connected RandR output with an available CRTC found"))?;

        let range = conn
            .randr_get_screen_size_range(root)
            .context("RandR GetScreenSizeRange request")?
            .reply()
            .context("RandR GetScreenSizeRange reply")?;

        tracing::info!(
            output,
            crtc,
            max_width = range.max_width,
            max_height = range.max_height,
            "XrandrDisplay connected"
        );

        tracing::info!(
            max_resolution = ?max_resolution,
            "display controller ceiling clamp"
        );

        Ok(Self {
            conn,
            root,
            output,
            crtc,
            ceiling_fallback: Mutex::new((range.max_width, range.max_height)),
            max_resolution,
        })
    }

    /// Live `GetScreenSizeRange` query, used by `ceiling()`. Split out so
    /// `ceiling()` (infallible per the trait) can fall back to the last
    /// known-good value on error rather than unwrapping.
    fn query_ceiling(&self) -> Result<(u16, u16), String> {
        let reply = self
            .conn
            .randr_get_screen_size_range(self.root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok((reply.max_width, reply.max_height))
    }
}

impl DisplayController for XrandrDisplay {
    fn ceiling(&self) -> (u16, u16) {
        let hardware = match self.query_ceiling() {
            Ok(val) => {
                *self.ceiling_fallback.lock().unwrap() = val;
                val
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "RandR GetScreenSizeRange failed; using last known ceiling"
                );
                *self.ceiling_fallback.lock().unwrap()
            }
        };
        clamp_ceiling(hardware, self.max_resolution.as_deref())
    }

    fn set_output(&self, width: u16, height: u16, scale_milli: u16) -> Result<(), DisplayError> {
        let resources = self
            .conn
            .randr_get_screen_resources(self.root)
            .map_err(|e| DisplayError(format!("GetScreenResources request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetScreenResources reply: {e}")))?;

        let output_info = self
            .conn
            .randr_get_output_info(self.output, resources.config_timestamp)
            .map_err(|e| DisplayError(format!("GetOutputInfo request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetOutputInfo reply: {e}")))?;

        // `xrandr --output O --mode WxH` selects one of the output's own
        // modes. Do the same whenever one fits, so the window manager sees
        // a mode it already knows; only a size no mode offers gets a CVT
        // mode of our own.
        let timing = cvt::reduced_blanking(width, height, 60);
        let (mode, mode_w, mode_h) = match pick_existing_mode(
            &output_info.modes,
            output_info.num_preferred,
            &resources.modes,
            (width, height),
            (timing.h_active, timing.v_active),
        ) {
            Some(m) => (m.id, m.width, m.height),
            None => (
                self.create_cvt_mode(&resources, &timing)?,
                timing.h_active,
                timing.v_active,
            ),
        };

        let crtc_info = self
            .conn
            .randr_get_crtc_info(self.crtc, resources.config_timestamp)
            .map_err(|e| DisplayError(format!("GetCrtcInfo request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetCrtcInfo reply: {e}")))?;

        let screen_geom = self
            .conn
            .get_geometry(self.root)
            .map_err(|e| DisplayError(format!("GetGeometry request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetGeometry reply: {e}")))?;

        let set_crtc = |mode: randr::Mode, outputs: &[randr::Output]| -> Result<(), DisplayError> {
            let reply = self
                .conn
                .randr_set_crtc_config(
                    self.crtc,
                    x11rb::CURRENT_TIME,
                    resources.config_timestamp,
                    0,
                    0,
                    mode,
                    randr::Rotation::ROTATE0,
                    outputs,
                )
                .map_err(|e| DisplayError(format!("SetCrtcConfig request: {e}")))?
                .reply()
                .map_err(|e| DisplayError(format!("SetCrtcConfig reply: {e}")))?;
            if reply.status != randr::SetConfig::SUCCESS {
                return Err(DisplayError(format!(
                    "SetCrtcConfig failed: status={:?}",
                    reply.status
                )));
            }
            Ok(())
        };

        let set_screen = |w: u16, h: u16| -> Result<(), DisplayError> {
            let mm_w = mm_for_scale(w, scale_milli);
            let mm_h = mm_for_scale(h, scale_milli);
            self.conn
                .randr_set_screen_size(self.root, w, h, mm_w, mm_h)
                .map_err(|e| DisplayError(format!("SetScreenSize request: {e}")))?
                .check()
                .map_err(|e| DisplayError(format!("SetScreenSize: {e}")))
        };

        let steps = apply_steps(
            (screen_geom.width, screen_geom.height),
            crtc_info.mode,
            mode,
            (mode_w, mode_h),
        );
        let mut grabbed = false;
        let mut disabled = false;
        let result = (|| -> Result<(), DisplayError> {
            for step in &steps {
                match *step {
                    RandrStep::Grab => {
                        self.conn
                            .grab_server()
                            .map_err(|e| DisplayError(format!("GrabServer: {e}")))?;
                        grabbed = true;
                    }
                    RandrStep::DisableCrtc => {
                        set_crtc(x11rb::NONE, &[])?;
                        disabled = true;
                    }
                    RandrStep::SetScreen(w, h) => set_screen(w, h)?,
                    RandrStep::SetCrtc => {
                        set_crtc(mode, &[self.output])?;
                        disabled = false;
                    }
                    RandrStep::Ungrab => {
                        self.conn
                            .ungrab_server()
                            .map_err(|e| DisplayError(format!("UngrabServer: {e}")))?;
                        grabbed = false;
                    }
                }
            }
            Ok(())
        })();
        if result.is_err() && disabled {
            // Never leave the output dark: put the CRTC back the way it was.
            let _ = self.conn.randr_set_crtc_config(
                self.crtc,
                x11rb::CURRENT_TIME,
                resources.config_timestamp,
                crtc_info.x,
                crtc_info.y,
                crtc_info.mode,
                crtc_info.rotation,
                &crtc_info.outputs,
            );
        }
        if grabbed {
            let _ = self.conn.ungrab_server();
        }
        let _ = self.conn.flush();
        result
    }
}

impl XrandrDisplay {
    /// Create (or find by name, after a BadName) and attach a CVT
    /// reduced-blanking mode for a size none of the output's modes offers.
    fn create_cvt_mode(
        &self,
        resources: &randr::GetScreenResourcesReply,
        timing: &cvt::Timing,
    ) -> Result<randr::Mode, DisplayError> {
        let (mode_w, mode_h) = (timing.h_active, timing.v_active);
        let name = format!("ghostframe_{mode_w}x{mode_h}");
        // CVT reduced blanking is +hsync -vsync (checked against `cvt -r`).
        let mode_info = randr::ModeInfo {
            id: 0,
            width: mode_w,
            height: mode_h,
            // ModeInfo.dot_clock is Hz; `Timing::pixel_clock_khz` is kHz.
            dot_clock: timing.pixel_clock_khz * 1000,
            hsync_start: timing.h_sync_start,
            hsync_end: timing.h_sync_end,
            htotal: timing.h_total,
            hskew: 0,
            vsync_start: timing.v_sync_start,
            vsync_end: timing.v_sync_end,
            vtotal: timing.v_total,
            name_len: name.len() as u16,
            mode_flags: randr::ModeFlag::HSYNC_POSITIVE | randr::ModeFlag::VSYNC_NEGATIVE,
        };
        let mode_id = match self
            .conn
            .randr_create_mode(self.root, mode_info, name.as_bytes())
            .map_err(|e| DisplayError(format!("CreateMode request: {e}")))?
            .reply()
        {
            Ok(created) => created.mode,
            // BadName: this size was created earlier and later detached from
            // every output. Reuse it by name rather than failing forever.
            Err(x11rb::errors::ReplyError::X11Error(ref e))
                if e.error_kind == x11rb::protocol::ErrorKind::Name =>
            {
                find_mode_by_name(resources, name.as_bytes()).ok_or_else(|| {
                    DisplayError(format!(
                        "CreateMode: BadName for {name:?} but no matching global mode found"
                    ))
                })?
            }
            Err(e) => return Err(DisplayError(format!("CreateMode reply: {e}"))),
        };
        self.conn
            .randr_add_output_mode(self.output, mode_id)
            .map_err(|e| DisplayError(format!("AddOutputMode request: {e}")))?
            .check()
            .map_err(|e| DisplayError(format!("AddOutputMode: {e}")))?;
        Ok(mode_id)
    }
}

/// One request of a mode change, in the order `xrandr(1)` issues them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RandrStep {
    Grab,
    DisableCrtc,
    SetScreen(u16, u16),
    SetCrtc,
    Ungrab,
}

/// The requests for a mode change, ordered as `xrandr --output O --mode M`
/// orders them. Recorded on Xvfb (2026-10-29, xrandr 1.5) for both a grow
/// and a shrink: `crtc 0: disable`, then `screen 0: WxH`, then
/// `crtc 0: <mode>`, all inside a server grab. Disabling first makes every
/// screen size legal, so there is no grow/shrink/mixed special case.
///
/// When the CRTC already shows the target mode and the screen already has
/// the target size, only the physical size can differ (a scale change), and
/// SetScreenSize alone carries it -- the output is not blanked for nothing.
fn apply_steps(
    screen: (u16, u16),
    current_mode: randr::Mode,
    target_mode: randr::Mode,
    target: (u16, u16),
) -> Vec<RandrStep> {
    if current_mode == target_mode && screen == target {
        return vec![RandrStep::SetScreen(target.0, target.1)];
    }
    let mut steps = vec![RandrStep::Grab];
    if current_mode != x11rb::NONE {
        steps.push(RandrStep::DisableCrtc);
    }
    steps.push(RandrStep::SetScreen(target.0, target.1));
    steps.push(RandrStep::SetCrtc);
    steps.push(RandrStep::Ungrab);
    steps
}

/// The output mode to use for a `want` request, if the output already has
/// one: an exact `want` size first, then the CVT-rounded `cvt` size. Among
/// equal sizes a preferred mode (the first `num_preferred` of
/// `output_modes`) wins, then the highest refresh rate.
fn pick_existing_mode(
    output_modes: &[randr::Mode],
    num_preferred: u16,
    all_modes: &[randr::ModeInfo],
    want: (u16, u16),
    cvt: (u16, u16),
) -> Option<randr::ModeInfo> {
    let refresh_mhz = |m: &randr::ModeInfo| -> u64 {
        let total = u64::from(m.htotal) * u64::from(m.vtotal);
        (u64::from(m.dot_clock) * 1000)
            .checked_div(total)
            .unwrap_or(0)
    };
    for size in [want, cvt] {
        let best = output_modes
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| {
                let info = all_modes.iter().find(|m| m.id == *id)?;
                ((info.width, info.height) == size).then_some((idx, *info))
            })
            .max_by_key(|(idx, info)| (*idx < usize::from(num_preferred), refresh_mhz(info)));
        if let Some((_, info)) = best {
            return Some(info);
        }
    }
    None
}

/// Look up a mode by name in a `GetScreenResourcesReply`. Names are packed
/// consecutively into `resources.names`, with each mode's slice given by
/// its own `name_len`, in the same order as `resources.modes` -- there is
/// no per-mode name field to compare directly.
fn find_mode_by_name(
    resources: &randr::GetScreenResourcesReply,
    name: &[u8],
) -> Option<randr::Mode> {
    let mut offset = 0usize;
    for m in &resources.modes {
        let end = offset + m.name_len as usize;
        if resources.names.get(offset..end) == Some(name) {
            return Some(m.id);
        }
        offset = end;
    }
    None
}

/// X has no scale concept: it reports a physical size and toolkits derive
/// DPI from it. Inverting the usual relation -- mm = px / (96 * scale) *
/// 25.4 -- makes scale 1.0 report exactly 96 DPI and 1.5 report 144.
fn mm_for_scale(px: u16, scale_milli: u16) -> u32 {
    // A client-supplied 0 would divide DPI to zero and blow this up to
    // u32::MAX via the saturating cast (no panic, no X error -- just a
    // screen reported as thousands of km wide). `IoBridge` doesn't validate
    // `scale_milli` before storing it, so this is the only guard.
    let scale_milli = scale_milli.max(1);
    let dpi = 96.0 * f64::from(scale_milli) / 1000.0;
    ((f64::from(px) / dpi) * 25.4).round() as u32
}

/// Lower a hardware ceiling to an operator-configured maximum.
///
/// `raw` is the value of `GHOSTFRAME_ATTACH_MAX_RESOLUTION`: `"WIDTHxHEIGHT"`
/// to clamp, `"none"` to disable, absent to disable.
///
/// **Only ever lowers.** A configured value above the hardware ceiling is
/// ignored rather than applied, because raising it would have the server ask
/// X for a mode it cannot allocate.
///
/// **Malformed input is ignored, not fatal.** A typo in a unit file must not
/// stop the daemon serving; it warns and keeps the hardware ceiling. Failing
/// closed here would turn a cosmetic mistake into an outage.
fn clamp_ceiling(hardware: (u16, u16), raw: Option<&str>) -> (u16, u16) {
    let Some(raw) = raw else {
        return hardware;
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return hardware;
    }
    let parsed = raw
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse::<u16>().ok()?, h.trim().parse::<u16>().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    match parsed {
        Some((w, h)) => (hardware.0.min(w), hardware.1.min(h)),
        None => {
            tracing::warn!(
                value = raw,
                "GHOSTFRAME_ATTACH_MAX_RESOLUTION is not WIDTHxHEIGHT or 'none'; ignoring"
            );
            hardware
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_for_scale_1x_1920_is_508mm() {
        // 1920 / 96 dpi = 20 in = 508 mm.
        assert_eq!(mm_for_scale(1920, 1000), 508);
    }

    #[test]
    fn mm_for_scale_2x_1920_is_half() {
        // Doubling the scale halves the reported physical size.
        assert_eq!(mm_for_scale(1920, 2000), 254);
    }

    #[test]
    fn mm_for_scale_1_5x_2560() {
        // 2560 / (96 * 1.5) = 2560 / 144 = 17.7778 in = 451.556 mm, rounds
        // to 452.
        assert_eq!(mm_for_scale(2560, 1500), 452);
    }

    #[test]
    fn mm_for_scale_zero_scale_does_not_blow_up() {
        // A malicious or buggy client sending scale_milli=0 must not
        // produce inf/NaN/u32::MAX -- it should behave as scale_milli=1.
        assert_eq!(mm_for_scale(1920, 0), mm_for_scale(1920, 1));
    }

    fn mode(id: u32, w: u16, h: u16, dot_clock: u32) -> randr::ModeInfo {
        randr::ModeInfo {
            id,
            width: w,
            height: h,
            dot_clock,
            htotal: 1000,
            vtotal: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn apply_steps_follow_xrandr_order() {
        // Recorded from `xrandr --verbose --output screen --mode M` on Xvfb,
        // for a shrink and for a grow alike: disable, screen, crtc.
        let all = vec![
            RandrStep::Grab,
            RandrStep::DisableCrtc,
            RandrStep::SetScreen(1280, 720),
            RandrStep::SetCrtc,
            RandrStep::Ungrab,
        ];
        assert_eq!(apply_steps((1920, 1080), 7, 9, (1280, 720)), all);
        let grow = apply_steps((1280, 720), 9, 7, (1920, 1080));
        assert_eq!(grow[1], RandrStep::DisableCrtc);
        assert_eq!(grow[2], RandrStep::SetScreen(1920, 1080));
    }

    #[test]
    fn apply_steps_skip_the_disable_for_a_disabled_crtc() {
        assert_eq!(
            apply_steps((1920, 1080), x11rb::NONE, 9, (1280, 720)),
            vec![
                RandrStep::Grab,
                RandrStep::SetScreen(1280, 720),
                RandrStep::SetCrtc,
                RandrStep::Ungrab,
            ]
        );
    }

    #[test]
    fn apply_steps_for_the_current_mode_only_resize_the_screen() {
        // A scale-only change: the output must not blank.
        assert_eq!(
            apply_steps((1920, 1080), 9, 9, (1920, 1080)),
            vec![RandrStep::SetScreen(1920, 1080)]
        );
    }

    #[test]
    fn pick_existing_mode_prefers_exact_then_cvt_size() {
        let all = [
            mode(1, 1920, 1080, 60_000_000),
            mode(2, 1368, 768, 60_000_000),
        ];
        // Exact size present.
        assert_eq!(
            pick_existing_mode(&[1, 2], 0, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            Some(1)
        );
        // 1366 is not offered; the CVT-rounded 1368 is.
        assert_eq!(
            pick_existing_mode(&[1, 2], 0, &all, (1366, 768), (1368, 768)).map(|m| m.id),
            Some(2)
        );
        // Neither: create a CVT mode.
        assert_eq!(
            pick_existing_mode(&[1, 2], 0, &all, (1000, 700), (1000, 700)).map(|m| m.id),
            None
        );
        // A global mode not on this output does not count.
        assert_eq!(
            pick_existing_mode(&[2], 0, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            None
        );
    }

    #[test]
    fn pick_existing_mode_prefers_preferred_then_refresh() {
        // 1000x1000 totals: dot_clock / 1e6 is the refresh rate in Hz.
        let all = [
            mode(1, 1920, 1080, 60_000_000),
            mode(2, 1920, 1080, 75_000_000),
            mode(3, 1920, 1080, 50_000_000),
        ];
        // No preferred mode: the highest refresh.
        assert_eq!(
            pick_existing_mode(&[1, 2, 3], 0, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            Some(2)
        );
        // The first mode is preferred: it wins over a faster one.
        assert_eq!(
            pick_existing_mode(&[3, 1, 2], 1, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            Some(3)
        );
    }

    #[test]
    fn unset_means_no_clamp() {
        assert_eq!(clamp_ceiling((16384, 16384), None), (16384, 16384));
    }

    #[test]
    fn a_wxh_value_lowers_the_ceiling() {
        assert_eq!(
            clamp_ceiling((16384, 16384), Some("1920x1080")),
            (1920, 1080)
        );
    }

    #[test]
    fn each_axis_clamps_independently() {
        // The five tests this started with all had either NO axis exceeding
        // or BOTH exceeding, so none of them could tell per-axis `min` apart
        // from whole-value rejection -- a review mutation to the latter left
        // every test passing. This is the case that straddles: width is
        // already under the clamp, height is over.
        assert_eq!(clamp_ceiling((1280, 2160), Some("1920x1080")), (1280, 1080));
        assert_eq!(clamp_ceiling((3840, 720), Some("1920x1080")), (1920, 720));
    }

    #[test]
    fn the_clamp_only_ever_lowers() {
        // A clamp larger than the hardware ceiling must not raise it -- that
        // would ask X for a mode it cannot allocate.
        assert_eq!(clamp_ceiling((1280, 720), Some("1920x1080")), (1280, 720));
    }

    #[test]
    fn none_disables_the_clamp() {
        // The documented escape hatch, for an operator who has decided the
        // recovery view no longer needs protecting.
        assert_eq!(clamp_ceiling((16384, 16384), Some("none")), (16384, 16384));
    }

    #[test]
    fn malformed_values_are_ignored_not_fatal() {
        // A typo in a unit file must not stop the daemon serving. Each of
        // these logs a warning and leaves the hardware ceiling in place.
        for bad in ["", "1920", "1920x", "x1080", "1920*1080", "abcxdef", "0x0"] {
            assert_eq!(
                clamp_ceiling((16384, 16384), Some(bad)),
                (16384, 16384),
                "input {bad:?} should have been ignored"
            );
        }
    }

    #[test]
    fn find_mode_by_name_locates_the_matching_entry() {
        let modes = vec![
            randr::ModeInfo {
                id: 100,
                name_len: 5,
                ..Default::default()
            },
            randr::ModeInfo {
                id: 200,
                name_len: 4,
                ..Default::default()
            },
        ];
        let resources = randr::GetScreenResourcesReply {
            modes,
            names: b"firstbbbb".to_vec(),
            ..Default::default()
        };
        assert_eq!(find_mode_by_name(&resources, b"first"), Some(100));
        assert_eq!(find_mode_by_name(&resources, b"bbbb"), Some(200));
        assert_eq!(find_mode_by_name(&resources, b"nope"), None);
    }
}

#[cfg(test)]
mod live_tests {
    //! Against a private Xvfb, whose single output is called `screen` and
    //! whose initial screen size is also its maximum.
    use super::*;
    use crate::capture::x11::randr_events::{shape, Recorder};
    use crate::capture::x11::test_support::PrivateXvfb;
    use std::process::Command;
    use std::time::Duration;

    fn xrandr(display: &str, args: &[&str]) {
        let status = Command::new("xrandr")
            .arg("-display")
            .arg(display)
            .args(args)
            .status()
            .expect("run xrandr (CI installs x11-xserver-utils)");
        assert!(status.success(), "xrandr {args:?} failed");
    }

    /// Give the output an 800x600 mode of its own, as a real output has.
    fn add_800x600(display: &str) {
        xrandr(
            display,
            &[
                "--newmode",
                "800x600_t",
                "40",
                "800",
                "840",
                "968",
                "1056",
                "600",
                "601",
                "605",
                "628",
            ],
        );
        xrandr(display, &["--addmode", "screen", "800x600_t"]);
    }

    fn mode_names(display: &str) -> Vec<String> {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[n].root;
        let res = conn
            .randr_get_screen_resources(root)
            .unwrap()
            .reply()
            .unwrap();
        let mut off = 0usize;
        res.modes
            .iter()
            .map(|m| {
                let end = off + usize::from(m.name_len);
                let name = String::from_utf8_lossy(&res.names[off..end]).into_owned();
                off = end;
                name
            })
            .collect()
    }

    /// The window manager's view of a server-initiated change must match
    /// `xrandr --output screen --mode M` exactly, for a shrink and a grow.
    #[test]
    fn set_output_emits_the_same_events_as_xrandr() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let rec = Recorder::connect(Some(x.display())).unwrap();
        let ctl = XrandrDisplay::connect(Some(x.display()), None).unwrap();
        let settle = || {
            rec.collect(Duration::from_millis(200), Duration::from_secs(3))
                .unwrap()
        };
        settle();
        for (name, other, (w, h)) in [
            ("800x600_t", "1280x1024", (800u16, 600u16)),
            ("1280x1024", "800x600_t", (1280, 1024)),
        ] {
            xrandr(x.display(), &["--output", "screen", "--mode", name]);
            let theirs = shape(&settle());
            xrandr(x.display(), &["--output", "screen", "--mode", other]);
            settle();
            ctl.set_output(w, h, 1000).unwrap();
            let ours = shape(&settle());
            assert!(!theirs.is_empty(), "xrandr produced no events");
            assert_eq!(ours, theirs, "to {w}x{h}");
        }
    }

    #[test]
    fn set_output_reuses_an_existing_mode_and_creates_cvt_only_otherwise() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = XrandrDisplay::connect(Some(x.display()), None).unwrap();
        let before = mode_names(x.display());
        ctl.set_output(800, 600, 1000).unwrap();
        assert_eq!(mode_names(x.display()), before, "800x600_t must be reused");
        ctl.set_output(1000, 700, 1000).unwrap();
        let after = mode_names(x.display());
        assert!(
            after.iter().any(|n| n == "ghostframe_1000x700"),
            "{after:?}"
        );
        let (conn, n) = x11rb::connect(Some(x.display())).unwrap();
        let g = conn
            .get_geometry(conn.setup().roots[n].root)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!((g.width, g.height), (1000, 700));
    }
}
````

- [ ] **Step 4: Run the tests**

Run: `xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins display`
Expected: all pass, among them `apply_steps_follow_xrandr_order`, `apply_steps_skip_the_disable_for_a_disabled_crtc`, `apply_steps_for_the_current_mode_only_resize_the_screen`, `pick_existing_mode_prefers_exact_then_cvt_size`, `pick_existing_mode_prefers_preferred_then_refresh`, `live_tests::set_output_emits_the_same_events_as_xrandr`, `live_tests::set_output_reuses_an_existing_mode_and_creates_cvt_only_otherwise`.

- [ ] **Step 5: Break each new test, watch it fail, restore**

Run the Step 4 command after each change, then undo it:
1. `apply_steps`: delete `        steps.push(RandrStep::DisableCrtc);` (keep the `if`'s braces empty). `apply_steps_follow_xrandr_order` fails, and `set_output_emits_the_same_events_as_xrandr` fails with `ours` lacking `"crtc off"` / `"output off"` (recorded while planning: the old order yields `["screen 1280x1024", "crtc 800x600", "root 1280x1024", "screen 800x600", "root 800x600"]` against xrandr's ten events).
2. `pick_existing_mode`: make the body `None`. `set_output_reuses_an_existing_mode_and_creates_cvt_only_otherwise` fails on "800x600_t must be reused" (a `ghostframe_800x600` mode appears), and both `pick_existing_mode_*` tests fail.
3. `pick_existing_mode`: `.max_by_key(|(idx, info)| (*idx < usize::from(num_preferred), refresh_mhz(info)))` → `.max_by_key(|(_, info)| refresh_mhz(info))`. `pick_existing_mode_prefers_preferred_then_refresh` fails (`Some(2)` instead of `Some(3)`).

- [ ] **Step 6 (only for Task 1 outcome B): keep the current DPI like xrandr**

In `set_output`, replace
```rust
        let set_screen = |w: u16, h: u16| -> Result<(), DisplayError> {
            let mm_w = mm_for_scale(w, scale_milli);
            let mm_h = mm_for_scale(h, scale_milli);
```
with
````rust
        // Task 1, outcome B: xrandr keeps the current DPI across a size
        // change. Only when the client asked for no scale of its own.
        let current_size = self
            .conn
            .randr_get_screen_info(self.root)
            .ok()
            .and_then(|c| c.reply().ok())
            .and_then(|i| i.sizes.get(usize::from(i.size_id)).copied());
        let set_screen = |w: u16, h: u16| -> Result<(), DisplayError> {
            let (mm_w, mm_h) = match current_size {
                Some(cur) if scale_milli == 1000 => mm_keeping_dpi((w, h), cur),
                _ => (mm_for_scale(w, scale_milli), mm_for_scale(h, scale_milli)),
            };
````
add above `/// X has no scale concept:`
````rust
/// `xrandr(1)`'s rule for the physical size: keep the current DPI, taken
/// from the current height (`dpi = 25.4 * height / mm_height`), truncating
/// as xrandr does.
fn mm_keeping_dpi(new: (u16, u16), current: randr::ScreenSize) -> (u32, u32) {
    if current.height == 0 || current.mheight == 0 {
        return (mm_for_scale(new.0, 1000), mm_for_scale(new.1, 1000));
    }
    let dpi = 25.4 * f64::from(current.height) / f64::from(current.mheight);
    (
        (25.4 * f64::from(new.0) / dpi) as u32,
        (25.4 * f64::from(new.1) / dpi) as u32,
    )
}
````
and in `mod tests`, directly above
```rust
    #[test]
    fn unset_means_no_clamp() {
```
add
````rust
    #[test]
    fn mm_keeping_dpi_matches_xrandr() {
        // evangeline's HDMI-A-1: 1920x1080 on 527x296 mm.
        let cur = randr::ScreenSize {
            width: 1920,
            height: 1080,
            mwidth: 527,
            mheight: 296,
        };
        assert_eq!(mm_keeping_dpi((1280, 720), cur), (350, 197));
        assert_eq!(mm_keeping_dpi((1920, 1080), cur), (526, 296));
    }
````
Run Step 4's command (expected: `mm_keeping_dpi_matches_xrandr` passes with the rest); break it by changing `current.height` to `current.width` in the `dpi` line (it fails), restore.

- [ ] **Step 7 (only for Task 1 outcome C): reset panning with the mode**

In `set_output`, replace
```rust
        let steps = apply_steps(
            (screen_geom.width, screen_geom.height),
```
with
````rust
        let panning = self
            .conn
            .randr_get_panning(self.crtc)
            .ok()
            .and_then(|c| c.reply().ok())
            .map(|p| (p.width, p.height));
        let steps = apply_steps(
            (screen_geom.width, screen_geom.height),
````
and replace the `RandrStep::SetCrtc => { ... }` arm with
````rust
                    RandrStep::SetCrtc => {
                        set_crtc(mode, &[self.output])?;
                        disabled = false;
                        // Task 1, outcome D: a panning area left at the old
                        // size makes the server keep the old screen size.
                        // Reset it to the mode, i.e. no panning.
                        if panning.is_some_and(|(w, h)| w != 0 || h != 0) {
                            self.conn
                                .randr_set_panning(
                                    self.crtc,
                                    x11rb::CURRENT_TIME,
                                    0,
                                    0,
                                    mode_w,
                                    mode_h,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                    0,
                                )
                                .map_err(|e| DisplayError(format!("SetPanning request: {e}")))?
                                .reply()
                                .map_err(|e| DisplayError(format!("SetPanning reply: {e}")))?;
                        }
                    }
````
Run Step 4's command (Xvfb has no panning, so the arm is not taken there; this step's proof is Task 16 step 3 on the real monitor).

- [ ] **Step 8: Lint and commit**

```bash
cargo fmt --all
just lint
cargo build -p ghostframe-server --examples
git add .github/workflows/_setup-system-deps/packages-build.txt \
  ghostframe-server/src/capture/x11/display.rs ghostframe-server/src/capture/x11/mod.rs
git commit -m "fix(server): RandR mode changes in xrandr's order, reusing output modes

A server-initiated change now looks to the window manager exactly like
\`xrandr --output O --mode M\`: an existing output mode when one fits,
and grab / disable CRTC / screen size / CRTC / ungrab. Tested by
recording both event streams on Xvfb.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

(Name Task 1's outcome letters in the message body when Step 6 or 7 was applied.)

---

### Task 9: Configuration: `ServerConfig`, `GHOSTFRAME_MAX_RESOLUTION`, default node name

**Files:**
- Rewrite: `ghostframe-server/src/config.rs`, `ghostframe-server/src/util.rs`
- Modify: `ghostframe-server/src/main.rs`, `ghostframe-server/src/capture/x11/mod.rs`, `ghostframe-server/src/capture/x11/display.rs`

- [ ] **Step 1: Write the failing tests first**

In `config.rs`'s `mod tests`, add before `fn state_dir_precedence_and_no_tmp_default`:

```rust
    #[test]
    fn hostname_defaults_to_the_host_name() {
        assert_eq!(cfg(&[]).ts_hostname, "testhost-ghostframe");
        assert_eq!(cfg(&[("TS_HOSTNAME", "")]).ts_hostname, "testhost-ghostframe");
        assert_eq!(cfg(&[("TS_HOSTNAME", "box")]).ts_hostname, "box");
        let nameless = ServerConfig::from_lookup(|_| None, None);
        assert_eq!(nameless.ts_hostname, "ghostframe-server");
    }

    #[test]
    fn max_resolution_is_read_from_its_new_name_only() {
        assert_eq!(
            cfg(&[("GHOSTFRAME_MAX_RESOLUTION", "2560x1440")])
                .max_resolution
                .as_deref(),
            Some("2560x1440")
        );
        assert_eq!(
            cfg(&[("GHOSTFRAME_ATTACH_MAX_RESOLUTION", "2560x1440")]).max_resolution,
            None
        );
    }
```

Run: `cargo test -p ghostframe-server --bins config`
Expected: compile errors (`ServerConfig` not found; `from_lookup` takes one argument; no field `max_resolution`).

- [ ] **Step 2: Implement**

Replace the whole of `ghostframe-server/src/config.rs` with:

````rust
//! Every environment variable `ghostframe-server` reads, parsed once.
//!
//! `ServerConfig::from_env()` is called once at the top of `main()` and the
//! result is passed down. This is the only file in the crate allowed to touch
//! `std::env` (CI greps for it), which keeps three things true:
//!
//! - a bad value is handled in one place, with a warning and a default,
//!   instead of an `expect` somewhere deep in startup;
//! - nothing mutates the process environment at runtime (`remove_var` is
//!   unsound in a multi-threaded process on glibc);
//! - this list is the complete, documented set.
//!
//! | Variable | Meaning | Default |
//! |---|---|---|
//! | `DISPLAY` | X display (diagnostics only; x11rb reads it itself) | unset |
//! | `GHOSTFRAME_LOG_FORMAT` | `json` for JSON logs | text |
//! | `TS_HOSTNAME` | tsnet node name | `<hostname>-ghostframe` (`ghostframe-server` if the host has no name) |
//! | `TS_STATE_DIR` | tsnet state directory (holds node keys) | `$XDG_STATE_HOME/ghostframe`, else `~/.local/state/ghostframe` |
//! | `XDG_STATE_HOME`, `HOME` | only to locate the default state directory | unset |
//! | `TS_AUTHKEY` | tsnet auth key: `login` without a browser | unset |
//! | `TS_CONTROL_URL` | tsnet control server | empty |
//! | `CAPTURE_FPS` | capture rate; invalid or 0 falls back | 30 |
//! | `GHOSTFRAME_CAPTURE_DUMP_DIR` | write each frame as PNG here (test builds only) | off |
//! | `GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY` | trace heartbeat every N frames | 5 |
//! | `GHOSTFRAME_DUMP_FIRST_FRAME` | dump first raw frame to this path (test builds only) | off |
//! | `GHOSTFRAME_MAX_RESOLUTION` | `WxH` resolution ceiling, or `none` | none (the unit `install` writes sets `1920x1080`) |

pub const DEFAULT_CAPTURE_FPS: u64 = 30;
const DEFAULT_HEARTBEAT_EVERY: u64 = 5;

#[derive(Debug, Clone)]
pub struct ServerConfig {
    pub display: Option<String>,
    pub log_json: bool,
    pub ts_hostname: String,
    /// Explicit `TS_STATE_DIR`, if set and non-empty. Use [`Self::state_dir`].
    ts_state_dir: Option<String>,
    xdg_state_home: Option<String>,
    home: Option<String>,
    pub ts_authkey: Option<String>,
    pub ts_control_url: String,
    /// Always >= 1.
    pub capture_fps: u64,
    pub capture_dump_dir: Option<std::path::PathBuf>,
    /// Always >= 1 (it is used as a modulus).
    pub heartbeat_every: u64,
    pub dump_first_frame: Option<String>,
    pub max_resolution: Option<String>,
}

impl ServerConfig {
    /// The tsnet state directory: `TS_STATE_DIR`, else
    /// `$XDG_STATE_HOME/ghostframe`, else `$HOME/.local/state/ghostframe`.
    /// Never a world-writable location: with none of the variables set there
    /// is no default, and the daemon refuses to start.
    pub fn state_dir(&self) -> Result<std::path::PathBuf, String> {
        use std::path::{Path, PathBuf};
        let abs = |what: &str, v: &str| {
            if Path::new(v).is_absolute() {
                Ok(PathBuf::from(v))
            } else {
                Err(format!("{what}={v:?} is not an absolute path"))
            }
        };
        if let Some(d) = &self.ts_state_dir {
            return abs("TS_STATE_DIR", d);
        }
        if let Some(x) = &self.xdg_state_home {
            return abs("XDG_STATE_HOME", x).map(|p| p.join("ghostframe"));
        }
        match &self.home {
            Some(h) => abs("HOME", h).map(|p| p.join(".local/state/ghostframe")),
            None => Err("no state directory: set TS_STATE_DIR, XDG_STATE_HOME or HOME".into()),
        }
    }

    /// Read the process environment. Call once, from `main()`.
    pub fn from_env() -> Self {
        Self::from_lookup(|k| std::env::var(k).ok(), crate::util::system_hostname())
    }

    /// Build from an arbitrary lookup and host name, so tests need no
    /// process-global state.
    pub fn from_lookup(
        get: impl Fn(&str) -> Option<String>,
        system_hostname: Option<String>,
    ) -> Self {
        // The two dump knobs write captured screen contents to a path the
        // environment names. They are honoured only in a test build; the
        // release daemon ignores them. ghostframe-server has no cargo features of its
        // own, so it follows the lib's `test-loss-injection`, which the e2e
        // image enables and `ci/check-release-features.sh` forbids in a
        // release build.
        let nonempty = |k: &str| get(k).filter(|s| !s.is_empty());
        let knob = |k: &str| {
            if ghostframe_lib::config::test_knobs_compiled() {
                get(k)
            } else {
                None
            }
        };
        Self {
            display: get("DISPLAY"),
            log_json: get("GHOSTFRAME_LOG_FORMAT").as_deref() == Some("json"),
            ts_hostname: nonempty("TS_HOSTNAME")
                .unwrap_or_else(|| default_hostname(system_hostname)),
            ts_state_dir: nonempty("TS_STATE_DIR"),
            xdg_state_home: nonempty("XDG_STATE_HOME"),
            home: nonempty("HOME"),
            ts_authkey: get("TS_AUTHKEY"),
            ts_control_url: get("TS_CONTROL_URL").unwrap_or_default(),
            capture_fps: parse_positive("CAPTURE_FPS", get("CAPTURE_FPS"), DEFAULT_CAPTURE_FPS),
            capture_dump_dir: knob("GHOSTFRAME_CAPTURE_DUMP_DIR")
                .filter(|s| !s.is_empty())
                .map(Into::into),
            heartbeat_every: parse_positive(
                "GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY",
                get("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY"),
                DEFAULT_HEARTBEAT_EVERY,
            ),
            dump_first_frame: knob("GHOSTFRAME_DUMP_FIRST_FRAME"),
            max_resolution: get("GHOSTFRAME_MAX_RESOLUTION"),
        }
    }
}

/// `<host>-ghostframe`, the name `login` and `capture` both register when
/// `TS_HOSTNAME` is unset; they must agree, or the node is renamed on every
/// start.
fn default_hostname(system_hostname: Option<String>) -> String {
    match system_hostname {
        Some(h) => format!("{h}-ghostframe"),
        None => "ghostframe-server".to_string(),
    }
}

/// Parse a strictly positive integer; anything else warns and yields `default`.
///
/// Logging is via `eprintln!` because this runs before the tracing subscriber
/// exists (the log format is itself configured here).
fn parse_positive(name: &str, raw: Option<String>, default: u64) -> u64 {
    let Some(raw) = raw else { return default };
    match raw.trim().parse::<u64>() {
        Ok(n) if n >= 1 => n,
        _ => {
            eprintln!("warning: {name}={raw:?} is not a positive integer; using {default}");
            default
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn cfg(pairs: &[(&str, &str)]) -> ServerConfig {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        ServerConfig::from_lookup(|k| m.get(k).cloned(), Some("testhost".into()))
    }

    #[test]
    fn defaults() {
        let c = cfg(&[]);
        assert_eq!(c.capture_fps, 30);
        assert_eq!(c.heartbeat_every, 5);
        assert!(!c.log_json);
        assert!(c.capture_dump_dir.is_none());
    }

    #[test]
    fn hostname_defaults_to_the_host_name() {
        assert_eq!(cfg(&[]).ts_hostname, "testhost-ghostframe");
        assert_eq!(
            cfg(&[("TS_HOSTNAME", "")]).ts_hostname,
            "testhost-ghostframe"
        );
        assert_eq!(cfg(&[("TS_HOSTNAME", "box")]).ts_hostname, "box");
        let nameless = ServerConfig::from_lookup(|_| None, None);
        assert_eq!(nameless.ts_hostname, "ghostframe-server");
    }

    #[test]
    fn max_resolution_is_read_from_its_new_name_only() {
        assert_eq!(
            cfg(&[("GHOSTFRAME_MAX_RESOLUTION", "2560x1440")])
                .max_resolution
                .as_deref(),
            Some("2560x1440")
        );
        assert_eq!(
            cfg(&[("GHOSTFRAME_ATTACH_MAX_RESOLUTION", "2560x1440")]).max_resolution,
            None
        );
    }

    #[test]
    fn state_dir_precedence_and_no_tmp_default() {
        use std::path::PathBuf;
        let p = |pairs: &[(&str, &str)]| cfg(pairs).state_dir();
        assert_eq!(
            p(&[
                ("TS_STATE_DIR", "/x/s"),
                ("XDG_STATE_HOME", "/xdg"),
                ("HOME", "/h")
            ]),
            Ok(PathBuf::from("/x/s"))
        );
        assert_eq!(
            p(&[("XDG_STATE_HOME", "/xdg"), ("HOME", "/h")]),
            Ok(PathBuf::from("/xdg/ghostframe"))
        );
        assert_eq!(
            p(&[("XDG_STATE_HOME", ""), ("HOME", "/h")]),
            Ok(PathBuf::from("/h/.local/state/ghostframe"))
        );
        assert!(p(&[]).is_err(), "no variables: fail closed, never /tmp");
        assert!(p(&[("XDG_STATE_HOME", "rel")]).is_err());
        assert!(p(&[("HOME", "rel")]).is_err());
    }

    /// `CAPTURE_FPS` used to `expect()` (non-numeric) and divide by zero (0).
    #[test]
    fn bad_capture_fps_falls_back_to_default() {
        for bad in ["0", "abc", "", "-5", "30.5"] {
            assert_eq!(cfg(&[("CAPTURE_FPS", bad)]).capture_fps, 30, "{bad:?}");
        }
        assert_eq!(cfg(&[("CAPTURE_FPS", "60")]).capture_fps, 60);
    }

    /// The heartbeat value is a modulus; 0 would panic in `is_multiple_of`-style use.
    #[test]
    fn zero_heartbeat_falls_back() {
        assert_eq!(
            cfg(&[("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY", "0")]).heartbeat_every,
            5
        );
        assert_eq!(
            cfg(&[("GHOSTFRAME_CAPTURE_HEARTBEAT_EVERY", "9")]).heartbeat_every,
            9
        );
    }

    #[test]
    fn flags_and_paths() {
        let c = cfg(&[
            ("GHOSTFRAME_CAPTURE_DUMP_DIR", ""),
            ("GHOSTFRAME_LOG_FORMAT", "json"),
        ]);
        assert!(c.log_json);
        assert!(c.capture_dump_dir.is_none());
    }

    /// The dump knobs write screen contents to disk, so a release build must
    /// ignore them. In this crate's own test run the lib is built without
    /// `test-loss-injection`, so this exercises the ignored branch; under the
    /// e2e image's feature set it exercises the honoured one. Either way the
    /// result must follow `test_knobs_compiled()`.
    #[test]
    fn dump_knobs_follow_the_test_knob_switch() {
        let c = cfg(&[
            ("GHOSTFRAME_CAPTURE_DUMP_DIR", "/var/dump"),
            ("GHOSTFRAME_DUMP_FIRST_FRAME", "/var/first"),
        ]);
        let on = ghostframe_lib::config::test_knobs_compiled();
        assert_eq!(c.capture_dump_dir.is_some(), on);
        assert_eq!(c.dump_first_frame.is_some(), on);
    }
}
````

Replace the whole of `ghostframe-server/src/util.rs` with (adds `system_hostname` and its test; everything else as left by Task 7):

````rust
//! Small helpers shared by the capture backends and `main`.

/// Read CLOCK_MONOTONIC and return the value as nanoseconds.
///
/// Stamps capture completion for the bench telemetry; both capture backends
/// must use the same clock for the numbers to be comparable.
pub fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe and writes only to the
    // caller-owned timespec. CLOCK_MONOTONIC is always available on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// This machine's host name, for the default tsnet node name.
pub fn system_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_hostname_matches_the_kernel() {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(system_hostname().as_deref(), Some(kernel.trim()));
    }

    #[test]
    fn monotonic_clock_is_nonzero_and_nondecreasing() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(a > 0);
        assert!(b >= a);
    }
}

/// Create (mode 0700) or validate the tsnet state directory, which holds the
/// node's private keys. Fails closed: an existing directory that is not a
/// directory, is owned by another user, or is accessible to group or others
/// is refused rather than repaired, since its contents may already have been
/// read. A symlink is refused too (`symlink_metadata`), so a planted link
/// cannot redirect the keys.
pub fn ensure_private_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| format!("cannot create state dir {}: {e}", path.display()))?;
        }
        Err(e) => return Err(format!("cannot stat state dir {}: {e}", path.display())),
        Ok(_) => {}
    }
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot stat state dir {}: {e}", path.display()))?;
    if !md.is_dir() {
        return Err(format!(
            "state dir {} is not a directory (symlinks are refused)",
            path.display()
        ));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if md.uid() != euid {
        return Err(format!(
            "state dir {} is owned by uid {}, not the current user (uid {euid}); refusing to start",
            path.display(),
            md.uid()
        ));
    }
    let mode = md.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "state dir {} has mode {mode:03o}, which lets other users read the tailnet node keys; \
             run `chmod 700 {}` and re-check who may have read it",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod state_dir_tests {
    use super::ensure_private_dir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn tmp(label: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gf-statedir-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn creates_missing_nested_dir_as_0700() {
        let root = tmp("create");
        let d = root.join("a/b/ghostframe");
        ensure_private_dir(&d).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        ensure_private_dir(&d).expect("idempotent on an existing 0700 dir");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_group_or_world_accessible() {
        let root = tmp("loose");
        for bad in [0o770, 0o750, 0o705, 0o755, 0o701] {
            let d = root.join(format!("m{bad:o}"));
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(bad)).unwrap();
            let e = ensure_private_dir(&d).expect_err(&format!("{bad:o} must be refused"));
            assert!(e.contains("chmod 700"), "{e}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_symlink_and_file() {
        let root = tmp("link");
        let real = root.join("real");
        ensure_private_dir(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
        let f = root.join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(ensure_private_dir(&f).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_dir_owned_by_another_user() {
        // /proc/1 is root-owned; when not root the ownership branch trips.
        if unsafe { libc::geteuid() } != 0 {
            let e = ensure_private_dir(std::path::Path::new("/proc/1")).unwrap_err();
            assert!(e.contains("owned by uid") || e.contains("mode"), "{e}");
        }
    }
}
````

Then:
- `main.rs`: `    let cfg = config::XdaemonConfig::from_env();` → `    let cfg = config::ServerConfig::from_env();`
- `capture/x11/mod.rs`: `use crate::config::XdaemonConfig;` → `use crate::config::ServerConfig;`; `pub fn new(cfg: &XdaemonConfig)` → `pub fn new(cfg: &ServerConfig)`; `display::XrandrDisplay::new(cfg.attach_max_resolution.clone())` → `display::XrandrDisplay::new(cfg.max_resolution.clone())`
- `capture/x11/display.rs`: `sed -i 's/GHOSTFRAME_ATTACH_MAX_RESOLUTION/GHOSTFRAME_MAX_RESOLUTION/g' ghostframe-server/src/capture/x11/display.rs` (a doc comment, the `clamp_ceiling` doc and its warning text)

- [ ] **Step 3: Run the tests**

Run: `xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins`
Expected: all pass, including `hostname_defaults_to_the_host_name`, `max_resolution_is_read_from_its_new_name_only`, `system_hostname_matches_the_kernel`.

- [ ] **Step 4: Break each new test, watch it fail, restore**

1. `default_hostname`: `format!("{h}-ghostframe")` → `format!("{h}")`. `hostname_defaults_to_the_host_name` fails.
2. `from_lookup`: `nonempty("TS_HOSTNAME")` → `get("TS_HOSTNAME")`. The `("TS_HOSTNAME", "")` assertion fails.
3. `from_lookup`: `get("GHOSTFRAME_MAX_RESOLUTION")` → `get("GHOSTFRAME_ATTACH_MAX_RESOLUTION")`. `max_resolution_is_read_from_its_new_name_only` fails.
4. `system_hostname`: `.trim().to_string()` → `.to_uppercase()`. `system_hostname_matches_the_kernel` fails (unless the host name is all capitals already; evangeline's is not).

- [ ] **Step 5: Lint and commit**

```bash
cargo fmt --all
ci/check-env-reads.sh
git grep -n "ATTACH_MAX_RESOLUTION\|XdaemonConfig\|attach_max_resolution" -- ghostframe-server tests ci .github Justfile
just lint
git add ghostframe-server/src/config.rs ghostframe-server/src/util.rs ghostframe-server/src/main.rs \
  ghostframe-server/src/capture/x11/mod.rs ghostframe-server/src/capture/x11/display.rs
git commit -m "feat(server): GHOSTFRAME_MAX_RESOLUTION; default node name <host>-ghostframe

GHOSTFRAME_ATTACH_MAX_RESOLUTION is renamed (there is only one mode
now). The tsnet node name defaults to <host>-ghostframe, the name the
old unit set with %H, so login and capture agree without a unit
setting it.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

Expected: `check-env-reads.sh` passes (`util::system_hostname` calls `gethostname`, not the environment); the grep prints nothing.

---

### Task 10: State-directory lock

**Files:**
- Create: `ghostframe-server/src/state.rs`
- Modify: `ghostframe-server/src/main.rs`, `ghostframe-server/Cargo.toml`

- [ ] **Step 1: Write the module and its tests**

In `ghostframe-server/Cargo.toml` append:
```toml

[dev-dependencies]
tempfile = "3"
```

Create `ghostframe-server/src/state.rs`:

````rust
//! The tsnet state directory: whether it is seeded, and the lock that keeps
//! two tsnet instances off one `tailscaled.state`.
//!
//! `capture`, `login` and `logout` each take an exclusive `flock` on
//! `<state_dir>/.lock` for as long as they use the directory. Nothing else
//! stopped a `login` from running against the state the service was using.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

/// The lock file's name inside the state directory.
pub const LOCK_FILE: &str = ".lock";

/// True once tsnet has written `tailscaled.state`, which it does after the
/// first successful join: the "has logged in" proxy.
pub fn state_dir_seeded(state_dir: &Path) -> bool {
    state_dir.join("tailscaled.state").exists()
}

#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock.
    Busy,
    /// The lock file could not be opened or locked.
    Io(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Busy => write!(
                f,
                "the ghostframe-server service is running; stop it first \
                 (systemctl --user stop ghostframe-server)"
            ),
            LockError::Io(e) => write!(f, "cannot lock the state directory: {e}"),
        }
    }
}

impl std::error::Error for LockError {}

/// Held for as long as this process uses the state directory; dropping it,
/// or exiting, releases it.
#[derive(Debug)]
pub struct StateLock {
    _file: File,
}

impl StateLock {
    /// Take the lock, or fail at once with [`LockError::Busy`].
    pub fn acquire(state_dir: &Path) -> Result<Self, LockError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(state_dir.join(LOCK_FILE))
            .map_err(LockError::Io)?;
        // SAFETY: flock on a descriptor this function owns; LOCK_NB makes it
        // fail instead of waiting.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let e = std::io::Error::last_os_error();
            return Err(if e.kind() == std::io::ErrorKind::WouldBlock {
                LockError::Busy
            } else {
                LockError::Io(e)
            });
        }
        Ok(Self { _file: file })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seeded_only_once_tailscaled_state_exists() {
        let d = tempfile::tempdir().unwrap();
        assert!(!state_dir_seeded(d.path()));
        assert!(!state_dir_seeded(&d.path().join("missing")));
        std::fs::write(d.path().join("tailscaled.state"), b"{}").unwrap();
        assert!(state_dir_seeded(d.path()));
    }

    #[test]
    fn a_second_holder_is_refused_until_the_first_drops() {
        let d = tempfile::tempdir().unwrap();
        let first = StateLock::acquire(d.path()).unwrap();
        // flock locks belong to the open file description, so a second open
        // in the same process conflicts exactly as another process would.
        let second = StateLock::acquire(d.path());
        assert!(matches!(second, Err(LockError::Busy)), "{second:?}");
        drop(first);
        StateLock::acquire(d.path()).expect("free again once dropped");
    }

    #[test]
    fn the_busy_message_says_to_stop_the_service() {
        let msg = LockError::Busy.to_string();
        assert!(msg.contains("service is running; stop it first"), "{msg}");
    }
}
````

Add `mod state;` to `main.rs`'s module list (between `mod config;` and `mod util;`).

- [ ] **Step 2: Run the tests**

Run: `cargo test -p ghostframe-server --bins state::`
Expected: `seeded_only_once_tailscaled_state_exists`, `a_second_holder_is_refused_until_the_first_drops`, `the_busy_message_says_to_stop_the_service` pass. (`main.rs` still has its own `state_dir_seeded`; Step 4 removes it, so expect a dead-code warning only until then.)

- [ ] **Step 3: Break it, watch it fail, restore**

In `StateLock::acquire`, `libc::LOCK_EX | libc::LOCK_NB` → `libc::LOCK_SH | libc::LOCK_NB`. `a_second_holder_is_refused_until_the_first_drops` fails (two shared locks coexist). Restore.

- [ ] **Step 4: The daemon takes the lock**

In `main.rs`:
1. Delete `fn state_dir_seeded` with its doc comment, and `use std::path::Path;`.
2. Replace `    let seeded = state_dir_seeded(&state_dir_path);` with
```rust
    // Held until the process exits: no second tsnet may write this state.
    let _state_lock = match state::StateLock::acquire(&state_dir_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let seeded = state::state_dir_seeded(&state_dir_path);
```
3. In `mod tests`, delete the three `state_dir_seeded_*` tests (now in `state.rs`), the `temp_dir` helper and the `use std::fs;` / `use std::path::PathBuf;` lines, leaving `use super::*;` and `capture_failures_trip_after_budget_and_reset_on_success`.

- [ ] **Step 5: Verify and commit**

```bash
cargo fmt --all
just lint
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins
cargo test -p ghostframe-server --test init_flag
git add ghostframe-server/Cargo.toml Cargo.lock ghostframe-server/src/state.rs ghostframe-server/src/main.rs
git commit -m "feat(server): an exclusive lock on the tsnet state directory

Two tsnet instances could write one tailscaled.state; nothing stopped
it. The daemon now holds flock(<state_dir>/.lock) for its lifetime and
refuses to start while another holder exists.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

---

### Task 11: Command dispatch: `capture`, `login`, `logout`

`main.rs` becomes dispatch; the daemon body moves to `commands/capture.rs` unchanged except: no `--init`, a Wayland-only refusal first, the "run `ghostframe-server login`" exit, and a `wait_for_x11` hint without headless mode. `login`/`logout` take the state lock. ghostbridge needs no change (see decision 3); its existing Go tests cover `loginURL`'s three outcomes.

**Files:**
- Create: `ghostframe-server/src/cli.rs`, `ghostframe-server/src/commands/{mod,capture,login}.rs`, `ghostframe-server/tests/commands.rs`
- Rewrite: `ghostframe-server/src/main.rs`
- Delete: `ghostframe-server/tests/init_flag.rs`
- Modify: `ghostframe-server/Cargo.toml`, `ghostframe-server/src/config.rs`, `.github/workflows/ci.yml`, `Justfile`, `tests/containers/test-server/entrypoint.sh`

- [ ] **Step 1: The CLI contract test (fails first)**

```bash
git rm ghostframe-server/tests/init_flag.rs
```

Create `ghostframe-server/tests/commands.rs`:

````rust
//! The `ghostframe-server` binary's command-line contract, run as a
//! subprocess with an empty environment. Every case here fails before tsnet
//! or X is touched, so it needs neither a tailnet nor a display.
//!
//! A tests/*.rs target runs in CI only if a workflow names it: see the
//! `--test commands` step in .github/workflows/ci.yml.

use std::fs::{File, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

fn run(args: &[&str], env: &[(&str, &Path)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ghostframe-server"));
    cmd.args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("spawn ghostframe-server")
}

/// A 0700 state directory, as the server insists on.
fn private_dir() -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    std::fs::set_permissions(d.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    d
}

/// Hold the state lock the way a running service would.
fn hold_lock(dir: &Path) -> File {
    let f = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join(".lock"))
        .unwrap();
    // SAFETY: flock on a descriptor this test owns.
    let rc = unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    assert_eq!(rc, 0, "test could not take the lock");
    f
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn capture_without_login_exits_2_and_says_to_log_in() {
    let state = private_dir();
    let out = run(&["capture"], &[("TS_STATE_DIR", state.path())]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("run `ghostframe-server login`"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn capture_refuses_a_wayland_only_session() {
    let state = private_dir();
    let out = run(
        &["capture"],
        &[
            ("TS_STATE_DIR", state.path()),
            ("WAYLAND_DISPLAY", Path::new("wayland-0")),
        ],
    );
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("Wayland sessions are not supported yet"),
        "{}",
        stderr(&out)
    );
}

#[test]
fn capture_refuses_a_world_accessible_state_dir() {
    let state = tempfile::tempdir().unwrap();
    std::fs::set_permissions(state.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = run(&["capture"], &[("TS_STATE_DIR", state.path())]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
    assert!(stderr(&out).contains("chmod 700"), "{}", stderr(&out));
}

#[test]
fn login_logout_and_capture_refuse_while_the_service_holds_the_lock() {
    let state = private_dir();
    std::fs::write(state.path().join("tailscaled.state"), b"{}").unwrap();
    let _held = hold_lock(state.path());
    for cmd in ["login", "logout", "capture"] {
        let out = run(&[cmd], &[("TS_STATE_DIR", state.path())]);
        assert_eq!(out.status.code(), Some(1), "{cmd}: {}", stderr(&out));
        assert!(
            stderr(&out).contains("the ghostframe-server service is running; stop it first"),
            "{cmd}: {}",
            stderr(&out)
        );
    }
    assert!(
        state.path().join("tailscaled.state").exists(),
        "a refused logout must not delete the state"
    );
}

#[test]
fn init_is_gone() {
    let out = run(&["--init"], &[]);
    assert_eq!(out.status.code(), Some(2), "{}", stderr(&out));
}
````

Run: `cargo test -p ghostframe-server --test commands`
Expected: FAIL — the binary has no subcommands yet (`capture` is ignored and the old startup path prints the `--init` message; `--init` is accepted).

- [ ] **Step 2: Config reads `WAYLAND_DISPLAY`**

In `config.rs`:
- after the `DISPLAY` table row add `` //! | `WAYLAND_DISPLAY` | only to refuse a Wayland-only session with a clear error | unset | ``
- after `    pub display: Option<String>,` add `    pub wayland_display: Option<String>,`
- Replace
```rust
            display: get("DISPLAY"),
```
with
```rust
            display: nonempty("DISPLAY"),
            wayland_display: nonempty("WAYLAND_DISPLAY"),
```

In `ghostframe-server/Cargo.toml`, after `anyhow = "1"` add `clap = { workspace = true }`.

- [ ] **Step 3: The command modules**

Create `ghostframe-server/src/cli.rs`:

````rust
//! The command line: `ghostframe-server <command>`.

#[derive(Debug, clap::Parser)]
#[command(
    name = "ghostframe-server",
    version,
    about = "Serve this user's X session to ghostframe clients over a Tailscale tailnet"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
pub enum Command {
    /// Join the tailnet: print a sign-in URL and wait until the node is
    /// authorised. With TS_AUTHKEY set, join without a browser.
    Login {
        /// Node name to register (default: TS_HOSTNAME, else `<host>-ghostframe`).
        #[arg(long)]
        hostname: Option<String>,
        /// Control server URL, e.g. a headscale instance (default: TS_CONTROL_URL, else Tailscale).
        #[arg(long, value_name = "URL")]
        control_url: Option<String>,
    },
    /// Leave the tailnet and delete the node's state.
    Logout,
    /// Run the server (what the systemd unit starts).
    Capture,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("ghostframe-server").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(parse(&["capture"]).unwrap(), Command::Capture);
        assert_eq!(parse(&["logout"]).unwrap(), Command::Logout);
        assert_eq!(
            parse(&["login"]).unwrap(),
            Command::Login {
                hostname: None,
                control_url: None
            }
        );
        assert_eq!(
            parse(&["login", "--hostname", "h", "--control-url", "http://hs"]).unwrap(),
            Command::Login {
                hostname: Some("h".into()),
                control_url: Some("http://hs".into())
            }
        );
    }

    #[test]
    fn a_command_is_required_and_init_is_gone() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--init"]).is_err());
    }
}
````

Create `ghostframe-server/src/commands/mod.rs`:

````rust
//! One module per `ghostframe-server` command.

pub mod capture;
pub mod login;
````

Create `ghostframe-server/src/commands/login.rs`:

````rust
//! `login` and `logout`: join the tailnet once, interactively or with
//! `TS_AUTHKEY`, and leave it again.
//!
//! Both hold the state-directory lock, so neither can run while the service
//! (`capture`) is using the same `tailscaled.state`.

use ghostframe_lib::transport::ghostbridge::GhostbridgeHandle;
use ghostframe_lib::GhostbridgeConfig;

use crate::config::ServerConfig;
use crate::state::{self, StateLock};

/// Bring the node up, print the sign-in URL (unless `TS_AUTHKEY` makes that
/// unnecessary) and return once the node is authorised.
pub fn login(
    cfg: &ServerConfig,
    hostname: Option<String>,
    control_url: Option<String>,
) -> Result<(), String> {
    let dir = cfg.state_dir()?;
    crate::util::ensure_private_dir(&dir)?;
    let _lock = StateLock::acquire(&dir).map_err(|e| e.to_string())?;

    let config = GhostbridgeConfig {
        hostname: hostname.clone().unwrap_or_else(|| cfg.ts_hostname.clone()),
        authkey: cfg.ts_authkey.clone().unwrap_or_default(),
        state_dir: dir.to_string_lossy().into_owned(),
        control_url: control_url
            .clone()
            .unwrap_or_else(|| cfg.ts_control_url.clone()),
    };
    let bridge = GhostbridgeHandle::connect(&config).map_err(|e| format!("tailnet: {e}"))?;

    if !config.authkey.is_empty() {
        // Non-interactive: the auth key authorises the node; Up returns once
        // it is Running.
        bridge.up().map_err(|e| format!("tailnet: {e}"))?;
        println!("logged in as {}", config.hostname);
    } else {
        // `login_url` returns once control publishes a URL, or `None` once
        // the node is Running (already authorised).
        match bridge.login_url().map_err(|e| format!("tailnet: {e}"))? {
            Some(url) => {
                println!(
                    "To authorise this node, open:\n\n    {url}\n\nWaiting for authorisation..."
                );
                bridge.up().map_err(|e| format!("tailnet: {e}"))?;
                println!("authorised as {}", config.hostname);
            }
            None => println!("already authorised as {}", config.hostname),
        }
    }

    // The service reads the same two settings from its environment, not from
    // anything `login` stores. Say so when they differ, or the next start
    // renames the node or talks to another control server.
    if hostname.is_some_and(|h| h != cfg.ts_hostname) {
        println!(
            "note: the service registers as {:?} unless its unit sets TS_HOSTNAME; \
             add `Environment=TS_HOSTNAME={}` with `systemctl --user edit ghostframe-server`",
            cfg.ts_hostname, config.hostname
        );
    }
    if control_url.is_some_and(|u| u != cfg.ts_control_url) {
        println!(
            "note: the service uses the default control server unless its unit sets \
             TS_CONTROL_URL; add `Environment=TS_CONTROL_URL={}` with \
             `systemctl --user edit ghostframe-server`",
            config.control_url
        );
    }
    Ok(())
}

/// Deregister the node from the control server, then delete the state
/// directory.
pub fn logout(cfg: &ServerConfig) -> Result<(), String> {
    let dir = cfg.state_dir()?;
    if !dir.exists() {
        println!(
            "not logged in ({} does not exist); nothing to do",
            dir.display()
        );
        return Ok(());
    }
    crate::util::ensure_private_dir(&dir)?;
    let lock = StateLock::acquire(&dir).map_err(|e| e.to_string())?;

    if state::state_dir_seeded(&dir) {
        let config = GhostbridgeConfig {
            hostname: cfg.ts_hostname.clone(),
            authkey: String::new(),
            state_dir: dir.to_string_lossy().into_owned(),
            control_url: cfg.ts_control_url.clone(),
        };
        let bridge = GhostbridgeHandle::connect(&config).map_err(|e| format!("tailnet: {e}"))?;
        // Log out BEFORE deleting state: the credentials logout needs live
        // there. If it fails, keep the state so the user can retry; deleting
        // it would strand the node in the tailnet's device list.
        bridge
            .logout()
            .map_err(|e| format!("tailnet logout failed, state kept for a retry: {e}"))?;
        // Dropping the handle closes the tsnet server.
        drop(bridge);
    }
    std::fs::remove_dir_all(&dir).map_err(|e| format!("removing {}: {e}", dir.display()))?;
    drop(lock);
    println!("logged out; removed {}", dir.display());
    Ok(())
}
````

Create `ghostframe-server/src/commands/capture.rs` (from `main.rs`: everything from `    tracing::info!("Server ready, entering capture loop");` to the end of the file, i.e. the loop, `CaptureFailures`, `dump_frame_as_png` and the remaining test, moves unchanged):

````rust
//! `capture`: the daemon. Waits for X, connects the backend, joins the
//! tailnet and streams frames while a client is connected.

use std::time::Duration;

use ghostframe_lib::{GhostbridgeConfig, GhostframeServer};

use crate::capture::Backend;
use crate::config::ServerConfig;
use crate::state;

/// Run until the server shuts down. Startup failures that need the operator
/// exit the process with code 2 and a message saying what to do.
pub async fn run(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Checked first: on a Wayland session every X11 step below would fail
    // with an error that names X, not the actual cause.
    if cfg.display.is_none() && cfg.wayland_display.is_some() {
        eprintln!(
            "error: Wayland sessions are not supported yet: ghostframe-server captures \
             X11, and this session has WAYLAND_DISPLAY set but no DISPLAY"
        );
        std::process::exit(2);
    }

    let state_dir_path = match cfg.state_dir().and_then(|p| {
        crate::util::ensure_private_dir(&p)?;
        Ok(p)
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let state_dir = state_dir_path.to_string_lossy().into_owned();
    // Held until the process exits: no second tsnet may write this state.
    let _state_lock = match state::StateLock::acquire(&state_dir_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    // An unseeded node can only join non-interactively, with TS_AUTHKEY (the
    // e2e image does exactly that). A person runs `login` once instead.
    let authkey = match (
        cfg.ts_authkey.clone().filter(|k| !k.is_empty()),
        state::state_dir_seeded(&state_dir_path),
    ) {
        (Some(k), _) => k,
        (None, true) => String::new(),
        (None, false) => {
            eprintln!(
                "error: this node has not joined a tailnet yet ({state_dir:?} holds no \
                 tailscaled.state).\n\
                 run `ghostframe-server login`, then `systemctl --user start ghostframe-server`"
            );
            std::process::exit(2);
        }
    };
    let capture_fps = cfg.capture_fps;

    let config = GhostbridgeConfig {
        hostname: cfg.ts_hostname.clone(),
        authkey,
        state_dir,
        control_url: cfg.ts_control_url.clone(),
    };

    if let Err(msg) =
        crate::capture::x11::wait_for_x11(Duration::from_secs(10), cfg.display.as_deref())
    {
        let dpy = cfg.display.as_deref().unwrap_or("(unset)");
        tracing::error!(
            "{msg}\n\
             The display is the user's own session, not ghostframe's to start. Confirm\n\
             this user can open it: `DISPLAY={dpy} xrandr --current`. The same failure\n\
             there means the X server refused the connection -- a wedged or exited\n\
             session, not a ghostframe misconfiguration."
        );
        std::process::exit(2);
    }

    // The X server is up: connect the backend (frames, damage, input, display).
    let mut backend = crate::capture::x11::X11Backend::new(&cfg)?;

    let lib_config = match ghostframe_lib::config::LibConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}; refusing to start");
            std::process::exit(2);
        }
    };
    tracing::info!(?lib_config, "effective LibConfig");

    tracing::info!("Connecting to Tailscale...");
    let server = match GhostframeServer::new(
        config,
        ":443",
        lib_config,
        backend.input_injector(),
        backend.display_controller(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            if let Some(ws) = e.downcast_ref::<ghostframe_lib::WebServerError>() {
                if matches!(ws, ghostframe_lib::WebServerError::HttpsCertsDisabled) {
                    tracing::error!(
                        "HTTPS Certificates are not enabled for this tailnet. \
                         Enable them at https://login.tailscale.com/admin/dns and restart."
                    );
                    std::process::exit(2);
                }
            }
            return Err(e);
        }
    };

    tracing::info!("Server ready, entering capture loop");

    let frame_interval = Duration::from_micros(1_000_000 / capture_fps);
    let mut frame_count = 0u64;
    // Tracks whether the loop is currently in idle (no-client) mode, so we
    // emit a single info log on each idle↔active flip. The IoBridge logs the
    // corresponding event on its side; this log shows the capture loop's view
    // (X11 GetImage suspended/resumed).
    // None = nothing logged yet, so the FIRST pass through the loop announces
    // whichever state it finds. Seeded `true` this stayed silent on the most
    // common startup path -- no client yet -- so the daemon logged "entering
    // capture loop" and then went permanently quiet at 0% CPU with no frames.
    // That is correct behaviour (capture is gated on a connected client) but it
    // reads exactly like a hang, and cost real time to tell apart from one.
    let mut was_idle: Option<bool> = None;

    // Optional capture-dump diagnostic: when set, write each captured frame
    // as a PNG to this directory (filename: frame_<count>.png). Useful for
    // visually verifying the server's view of the desktop alongside trace
    // logs.  Disabled when the env var is absent.
    let capture_dump_dir = cfg.capture_dump_dir.clone();
    if let Some(dir) = &capture_dump_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(?dir, error = %e, "could not create capture-dump dir; PNG dump disabled");
        } else {
            tracing::info!(?dir, "capture-dump enabled — writing frame PNGs");
        }
    }

    // Heartbeat cadence — emit one debug line every Nth iteration so we can
    // tell from logs whether the capture loop is alive, blocked on
    // submit_frame (= IoBridge backpressure), or has exited.
    let heartbeat_every = cfg.heartbeat_every;
    let mut capture_failures = CaptureFailures::default();

    loop {
        let loop_iter_start = std::time::Instant::now();
        // Skip the capture scrape entirely when nobody is connected. Without
        // this gate, X11 GetImage copies a full 1920x1080 framebuffer through
        // the X server every 33 ms regardless of demand.
        //
        // `wait_for_client` parks on the connect event itself. The earlier
        // version slept a fixed 500 ms and only re-read the gate on waking,
        // which put up to the whole interval between a client connecting and
        // the first frame being scraped -- for no benefit, since the gate is
        // already refreshed the moment a session's H3 handshake completes.
        if server.connected_session_count() == 0 {
            if was_idle != Some(true) {
                // Tear the damage subscription down with the gate. Draining
                // and discarding instead would leave the X server generating
                // one RAW_RECTANGLES event per damaged rectangle for output
                // nobody reads -- and would need a timer to do the
                // draining, which is the idle poll this loop no longer has.
                backend.disarm_damage();
                tracing::info!(
                    "no client connected; capture loop parked until one arrives \
                     (XDamage disarmed)"
                );
                was_idle = Some(true);
            }
            if let Err(e) = server.wait_for_client().await {
                // The IoBridge event loop is gone, so no client can ever
                // arrive. Previously this loop could only notice a dead
                // server via a failed `submit_frame` -- which it never
                // reaches while the gate is shut -- so it polled a dead
                // server at 2 Hz forever. Exits the same way the
                // `submit_frame` path does (warn + Ok), so the process exit
                // code is unchanged.
                tracing::warn!(frames = frame_count, "{e}; capture loop exiting");
                break;
            }
            // Re-check from the top: the client may already be gone again.
            continue;
        }
        if was_idle != Some(false) {
            backend.arm_damage();
            // `xdamage_armed` is the one place an operator can tell a working
            // damage path from a silently dead one: an unarmed monitor
            // reports no dirty tiles, every frame falls back to full-frame
            // comparison, and the output stays correct while costing more.
            tracing::info!(
                xdamage_armed = backend.damage_armed(),
                "client connected; capture loop resuming"
            );
            was_idle = Some(false);
        }

        let capture_start = std::time::Instant::now();

        // Some(tiles) = only these changed; None = check every tile.
        let damage_tiles = backend.drain_damage();

        let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;
        let submission = match backend.capture(timestamp_us) {
            Ok(mut frame) => {
                frame.damage_tiles = damage_tiles;
                Some(frame)
            }
            Err(e) => {
                tracing::warn!("X11 capture failed: {e}");
                None
            }
        };

        let capture_done = std::time::Instant::now();
        let capture_us = capture_done.duration_since(capture_start).as_micros() as u64;

        if let Some(frame) = submission {
            let frame_w = frame.width;
            let frame_h = frame.height;
            let frame_stride = frame.stride;
            let frame_has_dmabuf = frame.dmabuf_fd.is_some();
            let frame_pixels_len = frame.pixels.len();

            // Optional PNG dump of the captured frame so we can SEE what the
            // daemon is feeding into the encoder pipeline (gated on
            // GHOSTFRAME_CAPTURE_DUMP_DIR).  Runs BEFORE submit so we still
            // get a file even if submit blocks/fails.
            if let Some(dump_dir) = &capture_dump_dir {
                if !frame.pixels.is_empty() {
                    dump_frame_as_png(dump_dir, frame_count, &frame);
                }
            }

            let submit_start = std::time::Instant::now();
            let submit_res = server.submit_frame(frame).await;
            #[allow(
                clippy::disallowed_methods,
                reason = "ghostframe-server is a standalone capture-loop process with its own tokio runtime, never driven by the browserless harness's paused clock -- submit_us is real wall-clock latency telemetry for how long submit_frame() actually took"
            )]
            let submit_us = submit_start.elapsed().as_micros() as u64;
            if let Err(e) = submit_res {
                tracing::warn!(
                    frames = frame_count,
                    "submit_frame returned Err — channel closed, capture loop exiting: {e}"
                );
                break;
            }
            frame_count += 1;
            if frame_count == 1 {
                tracing::info!("first frame submitted");
            } else if frame_count.is_multiple_of(30) {
                // Bumped from 300 → 30 so e2e runs (~10-20s wall) see at
                // least one milestone.  Stays infrequent enough not to
                // dominate prod logs.
                tracing::info!(frames = frame_count, "capture running");
            }
            // Per-iter heartbeat — trace level so prod and the default e2e
            // log volume stay quiet (RUST_LOG now defaults to
            // `ghostframe=info`; the e2e harness sets `ghostframe=trace,debug`
            // which still hits trace for ghostframe targets when explicitly
            // debugging).
            // Captures the four timing components separately so we can spot
            // exactly which one stalls when the daemon goes silent.
            if frame_count.is_multiple_of(heartbeat_every) {
                tracing::trace!(
                    target: "ghostframe::server::capture",
                    frames = frame_count,
                    w = frame_w,
                    h = frame_h,
                    stride = frame_stride,
                    has_dmabuf = frame_has_dmabuf,
                    pixels_len = frame_pixels_len,
                    capture_us = capture_us,
                    submit_us = submit_us,
                    "[CAP] iter done"
                );
            }
            capture_failures.record_success();
        } else {
            tracing::trace!(
                target: "ghostframe::server::capture",
                frames = frame_count,
                capture_us = capture_us,
                "[CAP] capture returned None (no submission this iter)"
            );
            // A capture that fails every iteration used to warn 30x/s forever
            // while the process stayed up, so systemd (Restart=on-failure)
            // never got a failure to act on. Exit non-zero instead.
            if capture_failures.record_failure() {
                tracing::error!(
                    consecutive = capture_failures.consecutive,
                    "capture failed on every attempt; exiting so the service manager restarts us"
                );
                return Err(format!(
                    "capture failed {} consecutive times",
                    capture_failures.consecutive
                )
                .into());
            }
        }

        #[allow(
            clippy::disallowed_methods,
            reason = "ghostframe-server paces real screen capture against real wall-clock frame_interval on its own tokio runtime (not the browserless harness's paused one) -- this elapsed time directly controls how long tokio::time::sleep below actually sleeps"
        )]
        let elapsed = loop_iter_start.elapsed();
        if elapsed < frame_interval {
            tokio::time::sleep(frame_interval - elapsed).await;
        }
    }

    tracing::warn!(frames = frame_count, "capture loop exited");
    Ok(())
}

/// Consecutive failed captures tolerated before the daemon exits (about ten
/// seconds at the default 30 fps). Transient failures (a mode switch, a
/// momentary DRM master loss) clear well inside this.
const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = 300;

#[derive(Debug, Default)]
struct CaptureFailures {
    consecutive: u32,
}

impl CaptureFailures {
    fn record_success(&mut self) {
        self.consecutive = 0;
    }

    /// Returns true once the failure budget is exhausted.
    fn record_failure(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive >= MAX_CONSECUTIVE_CAPTURE_FAILURES
    }
}

/// Write a captured BGRA frame to `<dir>/frame_<count>.png`.  Best-effort:
/// logs and continues on any error so a failed dump never disturbs the live
/// capture path.  Gated by `GHOSTFRAME_CAPTURE_DUMP_DIR` env var.
fn dump_frame_as_png(
    dump_dir: &std::path::Path,
    count: u64,
    frame: &ghostframe_lib::server::FrameSubmission,
) {
    let path = dump_dir.join(format!("frame_{count:05}.png"));
    let w = frame.width;
    let h = frame.height;
    let stride = frame.stride as usize;
    let bgra = &frame.pixels;
    // Allocate tightly-packed RGBA8 (image crate accepts no stride param).
    let row_bytes = (w as usize).saturating_mul(4);
    if stride == 0 || bgra.len() < (h as usize).saturating_mul(stride.max(row_bytes)) {
        tracing::warn!(
            ?path,
            w,
            h,
            stride,
            len = bgra.len(),
            "pixel buffer too small for frame dims; skipping PNG dump"
        );
        return;
    }
    let mut rgba = vec![0u8; (h as usize).saturating_mul(row_bytes)];
    for y in 0..h as usize {
        let src_off = y.saturating_mul(stride);
        let dst_off = y.saturating_mul(row_bytes);
        for x in 0..w as usize {
            let s = src_off + x * 4;
            let d = dst_off + x * 4;
            // Wire format is BGRA; PNG expects RGBA. Swap B↔R and force
            // alpha=255 so the dump renders opaque even when X11's X-channel
            // happens to be zero.
            rgba[d] = bgra[s + 2];
            rgba[d + 1] = bgra[s + 1];
            rgba[d + 2] = bgra[s];
            rgba[d + 3] = 0xff;
        }
    }
    match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
        Ok(()) => {
            tracing::debug!(target: "ghostframe::server::capture", ?path, count, "[CAP-PNG] wrote frame")
        }
        Err(e) => tracing::warn!(?path, error = %e, "failed to write capture PNG"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_failures_trip_after_budget_and_reset_on_success() {
        let mut f = CaptureFailures::default();
        for _ in 0..MAX_CONSECUTIVE_CAPTURE_FAILURES - 1 {
            assert!(!f.record_failure());
        }
        // A success in the middle of a streak restarts the count.
        f.record_success();
        for _ in 0..MAX_CONSECUTIVE_CAPTURE_FAILURES - 1 {
            assert!(!f.record_failure());
        }
        assert!(f.record_failure(), "budget exhausted must trip");
    }
}
````

Replace the whole of `ghostframe-server/src/main.rs` with:

````rust
//! `ghostframe-server`: command dispatch only. Each command lives in
//! `commands/`.

mod capture;
mod cli;
mod commands;
mod config;
mod state;
mod util;

use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();
    let cfg = config::ServerConfig::from_env();
    // Default to INFO, not DEBUG. `ghostframe=debug` as a *default* means a
    // deployed daemon writes every DEBUG line forever: measured on a live
    // session at ~207 lines/second on a screen with nothing to send, six per
    // captured frame at ~29 fps (two INFO dispatch lines, two bench lines,
    // plus DEBUG scheduler.tick and dirty-tile-detection). That is formatting
    // and a journald write per line, and it is also what makes journald's
    // RateLimitBurst reachable -- a first-frame burst on top of this steady
    // rate blew the 10000/30s budget and silently discarded 23232 messages,
    // leaving a 20-second hole in the journal.
    //
    // DEBUG stays one `RUST_LOG=ghostframe=debug` away for anyone who wants
    // it; e2e sets `ghostframe=trace,debug` explicitly and is unaffected.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ghostframe=info".into());

    if cfg.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let result = match cli.command {
        Command::Capture => return commands::capture::run(cfg).await,
        Command::Login {
            hostname,
            control_url,
        } => commands::login::login(&cfg, hostname, control_url),
        Command::Logout => commands::login::logout(&cfg),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    Ok(())
}
````

- [ ] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-server --test commands
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins
(cd ghostbridge && go test -tags noweb -run 'TestLoginURL|TestLogout' ./...)
```

Expected: `commands`: 5 pass (`capture_without_login_exits_2_and_says_to_log_in`, `capture_refuses_a_wayland_only_session`, `capture_refuses_a_world_accessible_state_dir`, `login_logout_and_capture_refuse_while_the_service_holds_the_lock`, `init_is_gone`); unit tests pass including `cli::tests::*`; Go tests pass (`TestLoginURL*`, `TestLogoutRejectsUnknownHandle`).

- [ ] **Step 5: Break each new test, watch it fail, restore**

1. `commands/capture.rs`: in the Wayland check, `cfg.display.is_none() && cfg.wayland_display.is_some()` → `cfg.display.is_some() && cfg.wayland_display.is_some()`. `capture_refuses_a_wayland_only_session` fails (stderr says "has not joined a tailnet yet").
2. `commands/capture.rs`: in the `(None, false)` arm, `std::process::exit(2);` → `std::process::exit(1);`. `capture_without_login_exits_2_and_says_to_log_in` fails.
3. `state.rs`: in `LockError::Busy`'s message, `stop it first` → `stop it`. `login_logout_and_capture_refuse_while_the_service_holds_the_lock` fails for `login` (and the unit test `the_busy_message_says_to_stop_the_service`).
4. `main.rs`: `Command::Logout => commands::login::logout(&cfg),` → `Command::Logout => Ok(()),`. `login_logout_and_capture_refuse_while_the_service_holds_the_lock` fails for `logout` (exit 0).

Do not mutate the lock acquisition itself away in `login`/`logout`: with a seeded state dir the command would then start a real tsnet node from the test.

- [ ] **Step 6: CI, ci-local, entrypoint**

- `.github/workflows/ci.yml`, after the line `      - run: cargo test -p ghostframe-lib --test bwe_bench` add:
```yaml
      # ghostframe-server's command-line contract (exit codes, the state lock).
      - run: cargo test -p ghostframe-server --test commands
```
- `Justfile`, in `ci-local` after `    cargo test --workspace --bins` add `    cargo test -p ghostframe-server --test commands`.
- `tests/containers/test-server/entrypoint.sh`: `exec /usr/local/bin/ghostframe-server` → `exec /usr/local/bin/ghostframe-server capture`.

A tests/*.rs target runs nowhere in CI until a workflow names it; check: `grep -n "test commands" .github/workflows/ci.yml Justfile` prints both lines.

- [ ] **Step 7: Lint and commit**

```bash
cargo fmt --all
ci/check-env-reads.sh
just lint
git add ghostframe-server/Cargo.toml Cargo.lock ghostframe-server/src/cli.rs ghostframe-server/src/commands \
  ghostframe-server/src/main.rs ghostframe-server/src/config.rs ghostframe-server/tests/commands.rs \
  .github/workflows/ci.yml Justfile tests/containers/test-server/entrypoint.sh
git commit -m "feat(server): capture, login and logout commands

main.rs is dispatch. login prints the sign-in URL and waits until the
node is authorised (TS_AUTHKEY: no URL); logout deregisters the node,
then deletes the state. Both hold the state lock, so neither can run
while the service does. --init is gone; tests/commands.rs replaces
tests/init_flag.rs and is named in CI and ci-local.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

(The `git rm` of Step 1 is already staged.)

---

### Task 12: `install` and `uninstall`

Spec section 3.1. Everything is under `$HOME`; every path and every `systemctl` call is injected, so the tests run against a temporary home with a canary outside it (the canary carries a legacy unit's *name*, so a path that escaped the sandbox would delete it).

**Files:**
- Create: `ghostframe-server/templates/ghostframe-server.service.in`, `ghostframe-server/templates/ghostframe-server.desktop`, `ghostframe-server/src/commands/install.rs`
- Rewrite: `ghostframe-server/src/cli.rs`, `ghostframe-server/src/main.rs`, `ghostframe-server/src/util.rs`
- Modify: `ghostframe-server/src/commands/mod.rs`, `ghostframe-server/src/config.rs`, `ghostframe-server/tests/commands.rs`

- [ ] **Step 1: Templates**

Create `ghostframe-server/templates/ghostframe-server.service.in`:

````ini
# Written by `ghostframe-server install`, which overwrites it: put local
# changes in a drop-in (`systemctl --user edit ghostframe-server`).
#
# This service captures the X session of the user it runs as (DISPLAY=:0,
# your own desktop). A client admitted over the tailnet therefore gets that
# desktop: your files, your browser sessions, your sudo. Single-client
# eviction still applies, so two clients cannot both attach, but the tailnet
# is the only boundary.

[Unit]
Description=Ghostframe remote desktop server
After=graphical-session.target
PartOf=graphical-session.target
StartLimitIntervalSec=60
StartLimitBurst=3

[Service]
Type=simple
Environment=DISPLAY=:0
Environment=TS_STATE_DIR=%S/ghostframe
# Caps how large a remote client may make this session. The session is the
# physical display, which a KVM may be capturing as the machine's recovery
# view; an unclamped client could set a mode that view cannot show.
# `install --max-resolution none` leaves the variable out.
@MAX_RESOLUTION@
ExecStart=%h/.local/bin/ghostframe-server capture
Restart=on-failure
RestartSec=2
SyslogIdentifier=ghostframe-server

[Install]
WantedBy=graphical-session.target
````

Create `ghostframe-server/templates/ghostframe-server.desktop`:

````ini
[Desktop Entry]
Type=Application
Name=Ghostframe server
Comment=Starts ghostframe-server.service in this graphical session
# Written by `ghostframe-server install`. The unit's
# WantedBy=graphical-session.target is not enough on its own: many desktops
# (Enlightenment among them) never activate that target, so the service
# would be enabled and never start. XDG autostart is honoured by every
# desktop that honours anything, and `systemctl --user start` is idempotent.
Exec=systemctl --user start ghostframe-server.service
X-GNOME-Autostart-enabled=true
NoDisplay=true
````

The unit keeps the old attach unit's restart policy (`Restart=on-failure`, `RestartSec=2`, `StartLimitIntervalSec=60`, `StartLimitBurst=3`) and its warning comment, rewritten for a single mode.

- [ ] **Step 2: The install module, tests included**

Create `ghostframe-server/src/commands/install.rs`:

````rust
//! `install` and `uninstall`: a per-user install with no root.
//!
//! Everything written lives under the user's home: the binary in
//! `~/.local/bin`, the unit in `~/.config/systemd/user`, the autostart entry
//! in `~/.config/autostart`. `install` also migrates away from the old
//! `packaging/install.sh` layout, removing only files whose names it knows,
//! and reports root-owned leftovers in `/etc` without touching them.
//!
//! Nothing here reads the environment or derives a path on its own: every
//! location comes in through [`InstallPaths`] and [`InstallContext`], and
//! every `systemctl` call through [`Systemctl`], so the tests can run it all
//! against a temporary home (a getent-derived path once escaped a test
//! sandbox in this repository and deleted real files).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::cli::MaxResolution;

pub const BINARY_NAME: &str = "ghostframe-server";
pub const UNIT_NAME: &str = "ghostframe-server.service";
pub const AUTOSTART_NAME: &str = "ghostframe-server.desktop";

const UNIT_TEMPLATE: &str = include_str!("../../templates/ghostframe-server.service.in");
const AUTOSTART_TEMPLATE: &str = include_str!("../../templates/ghostframe-server.desktop");

/// User units the old installer wrote, by exact name. `ghostframe-xorg*.service`
/// is matched by [`is_legacy_unit`] as well.
const LEGACY_UNITS: &[&str] = &[
    "ghostframe-xdaemon.service",
    "ghostframe.target",
    "ghostframe-wm.service",
];
/// Drop-in directories of legacy units (the 2026-10-29 stopgap lives in one).
const LEGACY_DROPIN_DIRS: &[&str] = &["ghostframe-xdaemon.service.d"];
/// The old attach-mode autostart entry.
const LEGACY_AUTOSTART: &str = "ghostframe-attach.desktop";

/// Root-owned files the old installer wrote and nothing needs any more.
/// Listed with the command to remove them; never touched.
const ROOT_LEFTOVERS: &[&str] = &[
    "etc/systemd/system/getty@tty1.service.d/99-ghostframe-autologin.conf",
    "etc/X11/ghostframe-headless.conf",
];

fn is_legacy_unit(name: &str) -> bool {
    LEGACY_UNITS.contains(&name)
        || (name.starts_with("ghostframe-xorg") && name.ends_with(".service"))
}

/// Where `install` writes, derived from the home directory alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPaths {
    /// `~/.local/bin/ghostframe-server`; the unit's `%h/.local/bin/...`.
    pub bin: PathBuf,
    /// `~/.config/systemd/user`.
    pub unit_dir: PathBuf,
    /// `~/.config/autostart`.
    pub autostart_dir: PathBuf,
}

impl InstallPaths {
    pub fn new(home: &Path) -> Self {
        Self {
            bin: home.join(".local/bin").join(BINARY_NAME),
            unit_dir: home.join(".config/systemd/user"),
            autostart_dir: home.join(".config/autostart"),
        }
    }
}

/// Everything about the machine `install` needs, gathered by the caller.
pub struct InstallContext<'a> {
    pub paths: &'a InstallPaths,
    /// The running executable, copied to `paths.bin`.
    pub current_exe: &'a Path,
    /// The directory holding `etc/`: `/` in production.
    pub sysroot: &'a Path,
    /// Whether the tsnet state is seeded (decides start vs. "run login").
    pub state_seeded: bool,
    /// The user's login name, for the autologin check.
    pub username: &'a str,
    /// The effective uid; 0 is refused.
    pub euid: u32,
}

/// `systemctl --user`, behind a trait so tests can record the calls.
pub trait Systemctl {
    /// Run `systemctl --user <args>`; `Err` carries the reason.
    fn user(&mut self, args: &[&str]) -> Result<(), String>;
}

/// The real `systemctl`.
pub struct RealSystemctl;

impl Systemctl for RealSystemctl {
    fn user(&mut self, args: &[&str]) -> Result<(), String> {
        let out = std::process::Command::new("systemctl")
            .arg("--user")
            .args(args)
            .output()
            .map_err(|e| format!("cannot run systemctl: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(format!(
                "systemctl --user {} failed: {}",
                args.join(" "),
                String::from_utf8_lossy(&out.stderr).trim()
            ))
        }
    }
}

/// The unit file, with `GHOSTFRAME_MAX_RESOLUTION` set or left out.
pub fn render_unit(max: MaxResolution) -> String {
    let mut out = String::with_capacity(UNIT_TEMPLATE.len());
    for line in UNIT_TEMPLATE.lines() {
        if line == "@MAX_RESOLUTION@" {
            if let MaxResolution::Limit(w, h) = max {
                out.push_str(&format!("Environment=GHOSTFRAME_MAX_RESOLUTION={w}x{h}\n"));
            }
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The XDG autostart entry.
pub fn render_autostart() -> &'static str {
    AUTOSTART_TEMPLATE
}

/// `install`: see the design, section 3.1, for the steps.
pub fn install(
    ctx: &InstallContext,
    force: bool,
    max: MaxResolution,
    sys: &mut dyn Systemctl,
    out: &mut dyn Write,
) -> Result<(), String> {
    let w = |out: &mut dyn Write, s: String| {
        let _ = writeln!(out, "{s}");
    };
    // 1. Never as root: everything below belongs to the desktop user.
    if ctx.euid == 0 {
        return Err(
            "refusing to install as root: run `ghostframe-server install` as the \
                    desktop user whose session it should serve"
                .into(),
        );
    }

    // 2. The binary.
    match copy_binary(ctx.current_exe, &ctx.paths.bin, force)? {
        CopyOutcome::AlreadyRunningFromTarget => w(
            out,
            format!("binary: already running from {}", ctx.paths.bin.display()),
        ),
        CopyOutcome::Unchanged => w(
            out,
            format!("binary: {} is up to date", ctx.paths.bin.display()),
        ),
        CopyOutcome::Copied => w(
            out,
            format!("binary: installed {}", ctx.paths.bin.display()),
        ),
    }

    // 3. The unit.
    write_file(&ctx.paths.unit_dir.join(UNIT_NAME), &render_unit(max))?;
    w(
        out,
        format!(
            "unit: wrote {}",
            ctx.paths.unit_dir.join(UNIT_NAME).display()
        ),
    );

    // 4. Autostart: Enlightenment never activates graphical-session.target.
    write_file(
        &ctx.paths.autostart_dir.join(AUTOSTART_NAME),
        render_autostart(),
    )?;
    w(
        out,
        format!(
            "autostart: wrote {}",
            ctx.paths.autostart_dir.join(AUTOSTART_NAME).display()
        ),
    );

    // 5. Migrate away from the old installer.
    for removed in migrate(ctx.paths, sys)? {
        w(out, format!("migrate: removed {}", removed.display()));
    }
    for leftover in root_leftovers(ctx.sysroot) {
        w(
            out,
            format!(
                "leftover from the old root install: {leftover}\n  remove it with: sudo rm {leftover}"
            ),
        );
    }

    // 6. Enable, and start only if the node can join.
    sys.user(&["daemon-reload"])?;
    sys.user(&["enable", UNIT_NAME])?;
    if ctx.state_seeded {
        sys.user(&["restart", UNIT_NAME])?;
        w(out, format!("service: enabled and (re)started {UNIT_NAME}"));
    } else {
        w(
            out,
            format!(
                "service: enabled {UNIT_NAME}, not started: this node has not joined a tailnet.\n  \
                 run `ghostframe-server login`, then `systemctl --user start ghostframe-server`"
            ),
        );
    }

    // 7. Unattended boot needs the session to exist: report, never fail.
    if lightdm_autologin_user(ctx.sysroot).as_deref() == Some(ctx.username) {
        w(
            out,
            format!("autologin: LightDM logs {} in at boot", ctx.username),
        );
    } else {
        w(out, autologin_hint(ctx.username));
    }
    Ok(())
}

/// `uninstall`: remove what `install` wrote. The tailnet state stays; that is
/// `logout`'s to remove.
pub fn uninstall(
    paths: &InstallPaths,
    sys: &mut dyn Systemctl,
    out: &mut dyn Write,
) -> Result<(), String> {
    // Not installed, or the user manager is gone: removing the files below
    // is still right, so report and continue.
    if let Err(e) = sys.user(&["disable", "--now", UNIT_NAME]) {
        let _ = writeln!(out, "service: {e}");
    }
    for path in [
        paths.unit_dir.join(UNIT_NAME),
        paths.autostart_dir.join(AUTOSTART_NAME),
        paths.bin.clone(),
    ] {
        if remove_if_present(&path)? {
            let _ = writeln!(out, "removed {}", path.display());
        }
    }
    sys.user(&["daemon-reload"])?;
    let _ = writeln!(
        out,
        "the tailnet state is kept; `ghostframe-server logout` removes it"
    );
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
enum CopyOutcome {
    AlreadyRunningFromTarget,
    Unchanged,
    Copied,
}

/// Copy `exe` to `target`, refusing to replace a different binary without
/// `force`. Writes beside the target and renames over it, so a running copy
/// of the old binary is never written into.
fn copy_binary(exe: &Path, target: &Path, force: bool) -> Result<CopyOutcome, String> {
    if target.exists() {
        let same_file = match (exe.canonicalize(), target.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        };
        if same_file {
            return Ok(CopyOutcome::AlreadyRunningFromTarget);
        }
        let new = std::fs::read(exe).map_err(|e| format!("reading {}: {e}", exe.display()))?;
        let old =
            std::fs::read(target).map_err(|e| format!("reading {}: {e}", target.display()))?;
        if new == old {
            return Ok(CopyOutcome::Unchanged);
        }
        if !force {
            return Err(format!(
                "{} exists and differs from this binary; re-run with --force to replace it",
                target.display()
            ));
        }
    }
    let dir = target
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", target.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    let staging = target.with_extension("new");
    std::fs::copy(exe, &staging).map_err(|e| format!("copying to {}: {e}", staging.display()))?;
    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod {}: {e}", staging.display()))?;
    std::fs::rename(&staging, target)
        .map_err(|e| format!("renaming onto {}: {e}", target.display()))?;
    Ok(CopyOutcome::Copied)
}

fn write_file(path: &Path, contents: &str) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| format!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {e}", dir.display()))?;
    std::fs::write(path, contents).map_err(|e| format!("writing {}: {e}", path.display()))
}

/// Remove a file, symlink or directory if it exists; never follows a
/// symlink. Returns whether anything was there.
fn remove_if_present(path: &Path) -> Result<bool, String> {
    let md = match std::fs::symlink_metadata(path) {
        Ok(md) => md,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(format!("stat {}: {e}", path.display())),
    };
    let res = if md.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    res.map_err(|e| format!("removing {}: {e}", path.display()))?;
    Ok(true)
}

/// Stop, disable and delete the old installer's user units, their drop-ins,
/// the `*.wants/` links `enable` made for them, and the old autostart entry.
/// Only names in the legacy lists are touched.
fn migrate(paths: &InstallPaths, sys: &mut dyn Systemctl) -> Result<Vec<PathBuf>, String> {
    let mut removed = Vec::new();
    let entries: Vec<std::fs::DirEntry> = match std::fs::read_dir(&paths.unit_dir) {
        Ok(rd) => rd.filter_map(Result::ok).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(e) => return Err(format!("reading {}: {e}", paths.unit_dir.display())),
    };
    for entry in &entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_legacy_unit(&name) {
            // Best effort: a unit the user manager never loaded cannot be
            // disabled, and the file goes either way.
            let _ = sys.user(&["disable", "--now", &name]);
            if remove_if_present(&entry.path())? {
                removed.push(entry.path());
            }
        } else if LEGACY_DROPIN_DIRS.contains(&name.as_str()) {
            if remove_if_present(&entry.path())? {
                removed.push(entry.path());
            }
        } else if name.ends_with(".wants") && entry.path().is_dir() {
            for link in std::fs::read_dir(entry.path())
                .map_err(|e| format!("reading {}: {e}", entry.path().display()))?
                .filter_map(Result::ok)
            {
                let link_name = link.file_name().to_string_lossy().into_owned();
                if is_legacy_unit(&link_name) && remove_if_present(&link.path())? {
                    removed.push(link.path());
                }
            }
        }
    }
    let autostart = paths.autostart_dir.join(LEGACY_AUTOSTART);
    if remove_if_present(&autostart)? {
        removed.push(autostart);
    }
    removed.sort();
    Ok(removed)
}

/// The root-owned leftovers present under `sysroot`, as absolute paths.
fn root_leftovers(sysroot: &Path) -> Vec<String> {
    ROOT_LEFTOVERS
        .iter()
        .filter(|rel| sysroot.join(rel).exists())
        .map(|rel| format!("/{rel}"))
        .collect()
}

/// The user LightDM logs in automatically, if any: the last uncommented
/// `autologin-user=` in `etc/lightdm/lightdm.conf.d/*.conf` (sorted), then
/// `etc/lightdm/lightdm.conf`, which LightDM reads last.
fn lightdm_autologin_user(sysroot: &Path) -> Option<String> {
    let dir = sysroot.join("etc/lightdm");
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir.join("lightdm.conf.d"))
        .map(|rd| {
            rd.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "conf"))
                .collect()
        })
        .unwrap_or_default();
    files.sort();
    files.push(dir.join("lightdm.conf"));
    let mut user = None;
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else {
            continue;
        };
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            if let Some((k, v)) = line.split_once('=') {
                if k.trim() == "autologin-user" {
                    let v = v.trim();
                    user = (!v.is_empty()).then(|| v.to_string());
                }
            }
        }
    }
    user
}

/// The one root step an unattended boot needs.
fn autologin_hint(username: &str) -> String {
    format!(
        "autologin: none found for {username}. After a reboot the service starts only once \
         {username} logs in.\n  For an unattended boot, as root:\n    \
         sudo mkdir -p /etc/lightdm/lightdm.conf.d\n    \
         printf '[Seat:*]\\nautologin-user={username}\\nautologin-user-timeout=0\\n' | \
         sudo tee /etc/lightdm/lightdm.conf.d/99-ghostframe-autologin.conf"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records `systemctl --user` calls instead of making them.
    #[derive(Default)]
    struct Recorder {
        calls: Vec<String>,
    }

    impl Systemctl for Recorder {
        fn user(&mut self, args: &[&str]) -> Result<(), String> {
            self.calls.push(args.join(" "));
            Ok(())
        }
    }

    /// A sandbox: `root/home` is HOME, `root/sysroot` holds `etc/`, and a
    /// canary that carries a legacy unit's *name* sits outside both. Every
    /// test ends by checking the canary survived.
    struct Sandbox {
        root: tempfile::TempDir,
        paths: InstallPaths,
        exe: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let paths = InstallPaths::new(&root.path().join("home"));
            let exe = root.path().join("build/ghostframe-server");
            std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
            std::fs::write(&exe, b"binary v2").unwrap();
            let canary = root.path().join("canary/ghostframe-xdaemon.service");
            std::fs::create_dir_all(canary.parent().unwrap()).unwrap();
            std::fs::write(&canary, b"must survive").unwrap();
            Self { root, paths, exe }
        }

        fn sysroot(&self) -> PathBuf {
            self.root.path().join("sysroot")
        }

        fn ctx<'a>(&'a self, sysroot: &'a Path, seeded: bool) -> InstallContext<'a> {
            InstallContext {
                paths: &self.paths,
                current_exe: &self.exe,
                sysroot,
                state_seeded: seeded,
                username: "alice",
                euid: 1000,
            }
        }

        fn touch(&self, path: &Path) {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, b"x").unwrap();
        }

        fn run_install(&self, seeded: bool, force: bool) -> (Result<(), String>, Recorder, String) {
            let sysroot = self.sysroot();
            let mut sys = Recorder::default();
            let mut out = Vec::new();
            let r = install(
                &self.ctx(&sysroot, seeded),
                force,
                MaxResolution::Limit(1920, 1080),
                &mut sys,
                &mut out,
            );
            (r, sys, String::from_utf8(out).unwrap())
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let canary = self.root.path().join("canary/ghostframe-xdaemon.service");
            if !std::thread::panicking() {
                assert_eq!(std::fs::read(&canary).unwrap(), b"must survive");
            }
        }
    }

    #[test]
    fn unit_template_renders_the_limit_or_leaves_it_out() {
        let limited = render_unit(MaxResolution::Limit(2560, 1440));
        assert!(limited.contains("\nEnvironment=GHOSTFRAME_MAX_RESOLUTION=2560x1440\n"));
        assert!(limited.contains("\nExecStart=%h/.local/bin/ghostframe-server capture\n"));
        assert!(limited.contains("\nEnvironment=TS_STATE_DIR=%S/ghostframe\n"));
        assert!(limited.contains("\nEnvironment=DISPLAY=:0\n"));
        assert!(limited.contains("\nWantedBy=graphical-session.target\n"));
        assert!(!limited.contains('@'), "placeholder left in:\n{limited}");
        let open = render_unit(MaxResolution::Unlimited);
        assert!(!open.contains("GHOSTFRAME_MAX_RESOLUTION="));
        assert!(!open.contains('@'));
    }

    #[test]
    fn autostart_starts_the_service() {
        assert!(render_autostart()
            .contains("\nExec=systemctl --user start ghostframe-server.service\n"));
    }

    #[test]
    fn install_writes_binary_unit_and_autostart_and_does_not_start_unseeded() {
        let sb = Sandbox::new();
        let (r, sys, out) = sb.run_install(false, false);
        r.unwrap();
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"binary v2");
        let mode = std::fs::metadata(&sb.paths.bin)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        assert_eq!(
            std::fs::read_to_string(sb.paths.unit_dir.join(UNIT_NAME)).unwrap(),
            render_unit(MaxResolution::Limit(1920, 1080))
        );
        assert!(sb.paths.autostart_dir.join(AUTOSTART_NAME).exists());
        assert_eq!(
            sys.calls,
            ["daemon-reload", "enable ghostframe-server.service"]
        );
        assert!(out.contains("run `ghostframe-server login`"), "{out}");
    }

    #[test]
    fn install_restarts_the_service_when_seeded() {
        let sb = Sandbox::new();
        let (r, sys, _) = sb.run_install(true, false);
        r.unwrap();
        assert_eq!(
            sys.calls,
            [
                "daemon-reload",
                "enable ghostframe-server.service",
                "restart ghostframe-server.service"
            ]
        );
    }

    #[test]
    fn install_refuses_root() {
        let sb = Sandbox::new();
        let sysroot = sb.sysroot();
        let mut ctx = sb.ctx(&sysroot, false);
        ctx.euid = 0;
        let mut sys = Recorder::default();
        let e = install(
            &ctx,
            false,
            MaxResolution::Unlimited,
            &mut sys,
            &mut Vec::new(),
        )
        .unwrap_err();
        assert!(e.contains("refusing to install as root"), "{e}");
        assert!(!sb.paths.bin.exists() && sys.calls.is_empty());
    }

    #[test]
    fn a_different_installed_binary_needs_force() {
        let sb = Sandbox::new();
        sb.touch(&sb.paths.bin);
        let (r, _, _) = sb.run_install(false, false);
        assert!(r.unwrap_err().contains("--force"));
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"x");
        let (r, _, _) = sb.run_install(false, true);
        r.unwrap();
        assert_eq!(std::fs::read(&sb.paths.bin).unwrap(), b"binary v2");
    }

    #[test]
    fn running_from_the_target_skips_the_copy() {
        let sb = Sandbox::new();
        std::fs::create_dir_all(sb.paths.bin.parent().unwrap()).unwrap();
        std::fs::copy(&sb.exe, &sb.paths.bin).unwrap();
        assert_eq!(
            copy_binary(&sb.paths.bin, &sb.paths.bin, false).unwrap(),
            CopyOutcome::AlreadyRunningFromTarget
        );
        assert_eq!(
            copy_binary(&sb.exe, &sb.paths.bin, false).unwrap(),
            CopyOutcome::Unchanged
        );
    }

    #[test]
    fn migration_removes_exactly_the_legacy_names() {
        let sb = Sandbox::new();
        let units = &sb.paths.unit_dir;
        let legacy = [
            units.join("ghostframe-xdaemon.service"),
            units.join("ghostframe-xdaemon.service.d/force-root.conf"),
            units.join("ghostframe.target"),
            units.join("ghostframe-wm.service"),
            units.join("ghostframe-xorg.service"),
            units.join("ghostframe-xorg-vt7.service"),
            units.join("default.target.wants/ghostframe.target"),
            units.join("graphical-session.target.wants/ghostframe-xdaemon.service"),
            sb.paths.autostart_dir.join("ghostframe-attach.desktop"),
        ];
        let decoys = [
            units.join("ghostframe-xdaemon.service.bak"),
            units.join("ghostframe-xorg.timer"),
            units.join("my-ghostframe.target"),
            units.join("default.target.wants/pipewire.service"),
            units.join("other.service"),
            sb.paths
                .autostart_dir
                .join("ghostframe-attach.desktop.orig"),
        ];
        for p in legacy.iter().chain(&decoys) {
            sb.touch(p);
        }
        let (r, sys, _) = sb.run_install(false, false);
        r.unwrap();
        for p in &legacy {
            assert!(!p.exists(), "{} survived", p.display());
        }
        assert!(!units.join("ghostframe-xdaemon.service.d").exists());
        for p in &decoys {
            assert!(p.exists(), "{} was removed", p.display());
        }
        for unit in [
            "ghostframe-xdaemon.service",
            "ghostframe.target",
            "ghostframe-wm.service",
            "ghostframe-xorg.service",
        ] {
            assert!(
                sys.calls.contains(&format!("disable --now {unit}")),
                "{unit} not disabled: {:?}",
                sys.calls
            );
        }
    }

    #[test]
    fn root_leftovers_are_listed_and_left_alone() {
        let sb = Sandbox::new();
        let conf = sb.sysroot().join("etc/X11/ghostframe-headless.conf");
        sb.touch(&conf);
        let (r, _, out) = sb.run_install(false, false);
        r.unwrap();
        assert!(
            out.contains("sudo rm /etc/X11/ghostframe-headless.conf"),
            "{out}"
        );
        assert!(!out.contains("getty@tty1"), "{out}");
        assert!(conf.exists());
    }

    #[test]
    fn lightdm_autologin_is_detected_with_lightdm_conf_read_last() {
        let sb = Sandbox::new();
        let etc = sb.sysroot().join("etc/lightdm");
        assert_eq!(lightdm_autologin_user(&sb.sysroot()), None);
        std::fs::create_dir_all(etc.join("lightdm.conf.d")).unwrap();
        std::fs::write(
            etc.join("lightdm.conf.d/99-ghostframe-autologin.conf"),
            "[Seat:*]\nautologin-user=alice\n",
        )
        .unwrap();
        assert_eq!(
            lightdm_autologin_user(&sb.sysroot()).as_deref(),
            Some("alice")
        );
        // A commented line does not count; an uncommented empty value in the
        // main file, read last, wins and means nobody.
        std::fs::write(etc.join("lightdm.conf"), "#autologin-user=bob\n").unwrap();
        assert_eq!(
            lightdm_autologin_user(&sb.sysroot()).as_deref(),
            Some("alice")
        );
        std::fs::write(etc.join("lightdm.conf"), "[Seat:*]\nautologin-user=\n").unwrap();
        assert_eq!(lightdm_autologin_user(&sb.sysroot()), None);
    }

    #[test]
    fn install_prints_the_autologin_step_only_when_missing() {
        let sb = Sandbox::new();
        let (_, _, out) = sb.run_install(false, false);
        assert!(out.contains("autologin-user=alice"), "{out}");
        let etc = sb.sysroot().join("etc/lightdm/lightdm.conf.d");
        std::fs::create_dir_all(&etc).unwrap();
        std::fs::write(etc.join("50-x.conf"), "[Seat:*]\nautologin-user=alice\n").unwrap();
        let (_, _, out) = sb.run_install(false, false);
        assert!(out.contains("LightDM logs alice in at boot"), "{out}");
        assert!(!out.contains("sudo tee"), "{out}");
    }

    #[test]
    fn uninstall_removes_the_install_and_keeps_the_state() {
        let sb = Sandbox::new();
        sb.run_install(false, false).0.unwrap();
        let state = sb
            .root
            .path()
            .join("home/.local/state/ghostframe/tailscaled.state");
        sb.touch(&state);
        let mut sys = Recorder::default();
        uninstall(&sb.paths, &mut sys, &mut Vec::new()).unwrap();
        assert!(!sb.paths.bin.exists());
        assert!(!sb.paths.unit_dir.join(UNIT_NAME).exists());
        assert!(!sb.paths.autostart_dir.join(AUTOSTART_NAME).exists());
        assert!(state.exists());
        assert_eq!(
            sys.calls,
            ["disable --now ghostframe-server.service", "daemon-reload"]
        );
    }
}
````

`commands/mod.rs` becomes:

````rust
//! One module per `ghostframe-server` command.

pub mod capture;
pub mod install;
pub mod login;
````

- [ ] **Step 3: CLI, config, util, main**

Replace the whole of `ghostframe-server/src/cli.rs` with (adds `Install`, `Uninstall`, `MaxResolution`, `parse_max_resolution` and their tests):

````rust
//! The command line: `ghostframe-server <command>`.

#[derive(Debug, clap::Parser)]
#[command(
    name = "ghostframe-server",
    version,
    about = "Serve this user's X session to ghostframe clients over a Tailscale tailnet"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, PartialEq, Eq, clap::Subcommand)]
pub enum Command {
    /// Install this binary, its systemd user unit and an autostart entry
    /// for the current user, and remove what the old root installer left.
    Install {
        /// Replace a different ~/.local/bin/ghostframe-server.
        #[arg(long)]
        force: bool,
        /// Largest resolution a client may set, `WIDTHxHEIGHT`, or `none`.
        #[arg(long, value_name = "WxH|none", default_value = "1920x1080", value_parser = parse_max_resolution)]
        max_resolution: MaxResolution,
    },
    /// Remove what `install` wrote. Keeps the tailnet state (see `logout`).
    Uninstall,
    /// Join the tailnet: print a sign-in URL and wait until the node is
    /// authorised. With TS_AUTHKEY set, join without a browser.
    Login {
        /// Node name to register (default: TS_HOSTNAME, else `<host>-ghostframe`).
        #[arg(long)]
        hostname: Option<String>,
        /// Control server URL, e.g. a headscale instance (default: TS_CONTROL_URL, else Tailscale).
        #[arg(long, value_name = "URL")]
        control_url: Option<String>,
    },
    /// Leave the tailnet and delete the node's state.
    Logout,
    /// Run the server (what the systemd unit starts).
    Capture,
}

/// The ceiling `install` writes into the unit as `GHOSTFRAME_MAX_RESOLUTION`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxResolution {
    Limit(u16, u16),
    Unlimited,
}

/// `WIDTHxHEIGHT` (both non-zero) or `none`. Unlike the runtime parser in
/// `capture::x11::display`, which ignores a bad value rather than stop a
/// running service, `install` rejects one before writing anything.
pub fn parse_max_resolution(s: &str) -> Result<MaxResolution, String> {
    let t = s.trim();
    if t.eq_ignore_ascii_case("none") {
        return Ok(MaxResolution::Unlimited);
    }
    let bad = || format!("{s:?} is not WIDTHxHEIGHT or 'none'");
    let (w, h) = t.split_once(['x', 'X']).ok_or_else(bad)?;
    let w: u16 = w.trim().parse().map_err(|_| bad())?;
    let h: u16 = h.trim().parse().map_err(|_| bad())?;
    if w == 0 || h == 0 {
        return Err(bad());
    }
    Ok(MaxResolution::Limit(w, h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Command, clap::Error> {
        Cli::try_parse_from(std::iter::once("ghostframe-server").chain(args.iter().copied()))
            .map(|c| c.command)
    }

    #[test]
    fn parses_each_command() {
        assert_eq!(parse(&["capture"]).unwrap(), Command::Capture);
        assert_eq!(parse(&["logout"]).unwrap(), Command::Logout);
        assert_eq!(
            parse(&["login"]).unwrap(),
            Command::Login {
                hostname: None,
                control_url: None
            }
        );
        assert_eq!(
            parse(&["login", "--hostname", "h", "--control-url", "http://hs"]).unwrap(),
            Command::Login {
                hostname: Some("h".into()),
                control_url: Some("http://hs".into())
            }
        );
    }

    #[test]
    fn install_defaults_to_1080p_and_takes_none() {
        assert_eq!(
            parse(&["install"]).unwrap(),
            Command::Install {
                force: false,
                max_resolution: MaxResolution::Limit(1920, 1080)
            }
        );
        assert_eq!(
            parse(&["install", "--force", "--max-resolution", "none"]).unwrap(),
            Command::Install {
                force: true,
                max_resolution: MaxResolution::Unlimited
            }
        );
        assert_eq!(parse(&["uninstall"]).unwrap(), Command::Uninstall);
    }

    #[test]
    fn max_resolution_rejects_what_it_cannot_write() {
        assert_eq!(
            parse_max_resolution("2560X1440"),
            Ok(MaxResolution::Limit(2560, 1440))
        );
        for bad in [
            "",
            "1920",
            "1920x",
            "x1080",
            "0x1080",
            "1920x0",
            "abcxdef",
            "1920*1080",
        ] {
            assert!(parse_max_resolution(bad).is_err(), "{bad:?} accepted");
        }
        assert!(parse(&["install", "--max-resolution", "huge"]).is_err());
    }

    #[test]
    fn a_command_is_required_and_init_is_gone() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["--init"]).is_err());
    }
}
````

In `config.rs`, before the line
```rust
    /// Read the process environment. Call once, from `main()`.
```
add
```rust
    /// `$HOME`, if set and non-empty: where `install` puts everything.
    pub fn home(&self) -> Option<&str> {
        self.home.as_deref()
    }

```
and change the row `` //! | `XDG_STATE_HOME`, `HOME` | only to locate the default state directory | unset | `` to `` //! | `XDG_STATE_HOME`, `HOME` | the default state directory; `HOME` also roots `install` | unset | ``.

Replace the whole of `ghostframe-server/src/util.rs` with (adds `current_username`, `euid` and a test):

````rust
//! Small helpers shared by the capture backends and `main`.

/// Read CLOCK_MONOTONIC and return the value as nanoseconds.
///
/// Stamps capture completion for the bench telemetry; both capture backends
/// must use the same clock for the numbers to be comparable.
pub fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe and writes only to the
    // caller-owned timespec. CLOCK_MONOTONIC is always available on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// This machine's host name, for the default tsnet node name.
pub fn system_hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most `buf.len()` bytes into `buf`.
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// The effective user's login name, from the passwd database.
pub fn current_username() -> Option<String> {
    // SAFETY: getpwuid returns NULL or a pointer to static storage that stays
    // valid until the next getpw* call; the name is copied out before any.
    unsafe {
        let pw = libc::getpwuid(libc::geteuid());
        if pw.is_null() || (*pw).pw_name.is_null() {
            return None;
        }
        Some(
            std::ffi::CStr::from_ptr((*pw).pw_name)
                .to_string_lossy()
                .into_owned(),
        )
    }
}

/// The effective uid.
pub fn euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_username_is_the_owner_of_this_process() {
        let id = std::process::Command::new("id")
            .arg("-un")
            .output()
            .unwrap();
        let want = String::from_utf8(id.stdout).unwrap();
        assert_eq!(current_username().as_deref(), Some(want.trim()));
    }

    #[test]
    fn system_hostname_matches_the_kernel() {
        let kernel = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
        assert_eq!(system_hostname().as_deref(), Some(kernel.trim()));
    }

    #[test]
    fn monotonic_clock_is_nonzero_and_nondecreasing() {
        let a = monotonic_now_ns();
        let b = monotonic_now_ns();
        assert!(a > 0);
        assert!(b >= a);
    }
}

/// Create (mode 0700) or validate the tsnet state directory, which holds the
/// node's private keys. Fails closed: an existing directory that is not a
/// directory, is owned by another user, or is accessible to group or others
/// is refused rather than repaired, since its contents may already have been
/// read. A symlink is refused too (`symlink_metadata`), so a planted link
/// cannot redirect the keys.
pub fn ensure_private_dir(path: &std::path::Path) -> Result<(), String> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(path)
                .map_err(|e| format!("cannot create state dir {}: {e}", path.display()))?;
        }
        Err(e) => return Err(format!("cannot stat state dir {}: {e}", path.display())),
        Ok(_) => {}
    }
    let md = std::fs::symlink_metadata(path)
        .map_err(|e| format!("cannot stat state dir {}: {e}", path.display()))?;
    if !md.is_dir() {
        return Err(format!(
            "state dir {} is not a directory (symlinks are refused)",
            path.display()
        ));
    }
    // SAFETY: geteuid has no preconditions and cannot fail.
    let euid = unsafe { libc::geteuid() };
    if md.uid() != euid {
        return Err(format!(
            "state dir {} is owned by uid {}, not the current user (uid {euid}); refusing to start",
            path.display(),
            md.uid()
        ));
    }
    let mode = md.mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "state dir {} has mode {mode:03o}, which lets other users read the tailnet node keys; \
             run `chmod 700 {}` and re-check who may have read it",
            path.display(),
            path.display()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod state_dir_tests {
    use super::ensure_private_dir;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn tmp(label: &str) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("gf-statedir-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn creates_missing_nested_dir_as_0700() {
        let root = tmp("create");
        let d = root.join("a/b/ghostframe");
        ensure_private_dir(&d).unwrap();
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&d), 0o700);
        ensure_private_dir(&d).expect("idempotent on an existing 0700 dir");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_group_or_world_accessible() {
        let root = tmp("loose");
        for bad in [0o770, 0o750, 0o705, 0o755, 0o701] {
            let d = root.join(format!("m{bad:o}"));
            std::fs::create_dir(&d).unwrap();
            std::fs::set_permissions(&d, std::fs::Permissions::from_mode(bad)).unwrap();
            let e = ensure_private_dir(&d).expect_err(&format!("{bad:o} must be refused"));
            assert!(e.contains("chmod 700"), "{e}");
        }
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_symlink_and_file() {
        let root = tmp("link");
        let real = root.join("real");
        ensure_private_dir(&real).unwrap();
        let link = root.join("link");
        symlink(&real, &link).unwrap();
        assert!(ensure_private_dir(&link).is_err());
        let f = root.join("file");
        std::fs::write(&f, b"x").unwrap();
        assert!(ensure_private_dir(&f).is_err());
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn refuses_dir_owned_by_another_user() {
        // /proc/1 is root-owned; when not root the ownership branch trips.
        if unsafe { libc::geteuid() } != 0 {
            let e = ensure_private_dir(std::path::Path::new("/proc/1")).unwrap_err();
            assert!(e.contains("owned by uid") || e.contains("mode"), "{e}");
        }
    }
}
````

Replace the whole of `ghostframe-server/src/main.rs` with:

````rust
//! `ghostframe-server`: command dispatch only. Each command lives in
//! `commands/`.

mod capture;
mod cli;
mod commands;
mod config;
mod state;
mod util;

use clap::Parser;

use cli::{Cli, Command};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cli = Cli::parse();
    let cfg = config::ServerConfig::from_env();
    // Default to INFO, not DEBUG. `ghostframe=debug` as a *default* means a
    // deployed daemon writes every DEBUG line forever: measured on a live
    // session at ~207 lines/second on a screen with nothing to send, six per
    // captured frame at ~29 fps (two INFO dispatch lines, two bench lines,
    // plus DEBUG scheduler.tick and dirty-tile-detection). That is formatting
    // and a journald write per line, and it is also what makes journald's
    // RateLimitBurst reachable -- a first-frame burst on top of this steady
    // rate blew the 10000/30s budget and silently discarded 23232 messages,
    // leaving a 20-second hole in the journal.
    //
    // DEBUG stays one `RUST_LOG=ghostframe=debug` away for anyone who wants
    // it; e2e sets `ghostframe=trace,debug` explicitly and is unaffected.
    let env_filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ghostframe=info".into());

    if cfg.log_json {
        tracing_subscriber::fmt()
            .json()
            .with_env_filter(env_filter)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    }

    let result = match cli.command {
        Command::Capture => return commands::capture::run(cfg).await,
        Command::Login {
            hostname,
            control_url,
        } => commands::login::login(&cfg, hostname, control_url),
        Command::Logout => commands::login::logout(&cfg),
        Command::Install {
            force,
            max_resolution,
        } => install(&cfg, force, max_resolution),
        Command::Uninstall => uninstall(&cfg),
    };
    if let Err(e) = result {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
    Ok(())
}

fn install_paths(cfg: &config::ServerConfig) -> Result<commands::install::InstallPaths, String> {
    let home = cfg
        .home()
        .ok_or("HOME is not set; install writes under the user's home directory")?;
    Ok(commands::install::InstallPaths::new(std::path::Path::new(
        home,
    )))
}

fn install(
    cfg: &config::ServerConfig,
    force: bool,
    max_resolution: cli::MaxResolution,
) -> Result<(), String> {
    let paths = install_paths(cfg)?;
    let exe = std::env::current_exe().map_err(|e| format!("locating this binary: {e}"))?;
    let username = util::current_username().ok_or("this uid has no passwd entry")?;
    let state_seeded = cfg
        .state_dir()
        .map(|d| state::state_dir_seeded(&d))
        .unwrap_or(false);
    let ctx = commands::install::InstallContext {
        paths: &paths,
        current_exe: &exe,
        sysroot: std::path::Path::new("/"),
        state_seeded,
        username: &username,
        euid: util::euid(),
    };
    commands::install::install(
        &ctx,
        force,
        max_resolution,
        &mut commands::install::RealSystemctl,
        &mut std::io::stdout(),
    )
}

fn uninstall(cfg: &config::ServerConfig) -> Result<(), String> {
    commands::install::uninstall(
        &install_paths(cfg)?,
        &mut commands::install::RealSystemctl,
        &mut std::io::stdout(),
    )
}
````

Append to `ghostframe-server/tests/commands.rs`:

```rust

#[test]
fn help_lists_every_command() {
    let out = run(&["--help"], &[]);
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8_lossy(&out.stdout);
    for cmd in ["install", "uninstall", "login", "logout", "capture"] {
        assert!(text.contains(cmd), "{cmd} missing from:\n{text}");
    }
}
```

- [ ] **Step 4: Run the tests**

```bash
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test -p ghostframe-server --bins
cargo test -p ghostframe-server --test commands
```

Expected: all pass, including the 12 `commands::install::tests::*`, `cli::tests::install_defaults_to_1080p_and_takes_none`, `cli::tests::max_resolution_rejects_what_it_cannot_write`, `util::tests::current_username_is_the_owner_of_this_process`, and `help_lists_every_command`.

Never run `ghostframe-server install` from a test: it calls the real `systemctl --user`. Only `install::install`/`uninstall` with `Recorder` run in tests.

- [ ] **Step 5: Break each new test, watch it fail, restore**

Run Step 4's first command after each change, then undo it. Every one of these stays inside the sandbox; each run also re-checks the canary.
1. `is_legacy_unit`: replace the body with `name.starts_with("ghostframe")`. `migration_removes_exactly_the_legacy_names` fails on a decoy (`ghostframe-xdaemon.service.bak` or `ghostframe-xorg.timer` "was removed").
2. `render_unit`: delete the `if let MaxResolution::Limit(w, h) = max { ... }` statement (keep the `if line == ...` branch empty). `unit_template_renders_the_limit_or_leaves_it_out` fails.
3. `install`: `if ctx.euid == 0` → `if ctx.euid == 1`. `install_refuses_root` fails.
4. `install`: `if ctx.state_seeded` → `if !ctx.state_seeded`. Both `install_writes_binary_unit_and_autostart_and_does_not_start_unseeded` and `install_restarts_the_service_when_seeded` fail on the recorded calls.
5. `lightdm_autologin_user`: `files.push(dir.join("lightdm.conf"));` → `files.insert(0, dir.join("lightdm.conf"));` (main file read first). `lightdm_autologin_is_detected_with_lightdm_conf_read_last` fails on its last assertion.
6. `copy_binary`: delete the `if !force { return Err(...) }` block. `a_different_installed_binary_needs_force` fails.
7. `root_leftovers`: return `Vec::new()`. `root_leftovers_are_listed_and_left_alone` fails.

- [ ] **Step 6: Lint and commit**

```bash
cargo fmt --all
ci/check-env-reads.sh
just lint
git add ghostframe-server/templates ghostframe-server/src/commands/install.rs ghostframe-server/src/commands/mod.rs \
  ghostframe-server/src/cli.rs ghostframe-server/src/config.rs ghostframe-server/src/util.rs \
  ghostframe-server/src/main.rs ghostframe-server/tests/commands.rs
git commit -m "feat(server): rootless install and uninstall

install refuses root, copies itself to ~/.local/bin, writes an embedded
systemd user unit and an XDG autostart entry, migrates away from the
old xdaemon/headless units (known names only), lists root-owned
leftovers without touching them, enables the unit and (re)starts it if
the node is logged in, and prints the one root step for an unattended
boot. uninstall removes what install wrote and keeps the tailnet state.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

---

### Task 13: Delete `packaging/` and its test

`install` (Task 12) replaces `packaging/install.sh`, its templates and the headless Xorg configs.

**Files:**
- Delete: `packaging/` (all: `install.sh`, `systemd/*`, `xorg-headless-*.conf*`, `lightdm-autologin.conf.tmpl`, `ghostframe-attach-autostart.desktop.tmpl`), `tests/packaging/mode_switch_test.sh`
- Modify: `.github/workflows/ci.yml`, `Justfile`, `ci/check-test-features.sh`, `ci/check-release-features.sh`

- [ ] **Step 1: Delete**

```bash
git rm -r packaging tests/packaging
```

- [ ] **Step 2: Remove the references**

- `.github/workflows/ci.yml`: delete the whole `packaging:` job (from `  packaging:` through its `bash tests/packaging/mode_switch_test.sh` line and the blank line after it).
- `Justfile`: delete the `test-packaging` recipe with its five-line comment above it, and in `ci-local` the two lines `    @echo "=== packaging tests ==="` and `    just test-packaging`.
- `ci/check-test-features.sh`: `done < <(grep -rn -- "$feature" Justfile .github ci packaging 2>/dev/null || true)` → `done < <(grep -rn -- "$feature" Justfile .github ci 2>/dev/null || true)`
- `ci/check-release-features.sh` line 2: Replace
```bash
# The daemon that `just build-release` produces, and packaging/install.sh
```
with
```bash
# The server that `just build-release` produces, and `ghostframe-server install`
```

- [ ] **Step 3: Verify and commit**

```bash
git grep -n "packaging/\|test-packaging\|mode_switch_test" -- ':!docs/superpowers' ':!docs/reviews' ':!docs/specs' ':!docs/design' ':!README.md' ':!DEVELOPERS.md' ':!AGENTS.md'
ci/check-test-features.sh
ci/check-release-features.sh
git add .github/workflows/ci.yml Justfile ci/check-test-features.sh ci/check-release-features.sh
git commit -m "chore: delete packaging/ and its mode-switch test

ghostframe-server install writes the per-user unit and autostart entry
itself, and the headless deployment is gone.

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

Expected: the grep prints only `ghostframe-server/src/commands/install.rs` (its module doc names the old `packaging/install.sh` layout it migrates from). README, DEVELOPERS and AGENTS are Task 14.

---

### Task 14: Documentation

**Files:**
- Modify: `README.md`, `DEVELOPERS.md`, `AGENTS.md`, `docs/m0-manual-smoke-test.md`

- [ ] **Step 1: README**

1. Replace
```markdown
| **Server** | `ghostframe-server` — headless Xorg, capture, encode, tailnet. Serves the browser client itself. | [Install](#install) |
```
with
```markdown
| **Server** | `ghostframe-server` — captures your X11 desktop session, encodes, joins the tailnet. Serves the browser client itself. | [Install](#install) |
```
2. Replace
```markdown
A Linux-only remote desktop server: stream a headless Xorg session running
your favourite window manager to a browser, over QUIC on a Tailscale
tailnet.
```
with
```markdown
A Linux-only remote desktop server: stream your X11 desktop session to a
browser, over QUIC on a Tailscale tailnet.
```
3. In "Server prerequisites": replace the paragraph that begins `The install path is supported on **AMD GPUs` and ends `[DEVELOPERS.md](DEVELOPERS.md#wm-alternatives)).` with
```markdown
The server captures the X11 session of the user who installs it, whatever
window manager it runs. It needs a Vulkan device for analysis and encoding
(AMD with `amdgpu`/RADV is what it is developed on); VA-API is used for H.264
when present. No root access is needed to install or run it. Wayland sessions
are not supported yet.
```
   in the Ubuntu block replace the last two lines `    libvulkan1 mesa-vulkan-drivers vulkan-tools \` / `    xserver-xorg xserver-xorg-video-amdgpu enlightenment` with the single line `    libvulkan1 mesa-vulkan-drivers vulkan-tools`, and in the Arch block replace `    vulkan-icd-loader vulkan-tools \` / `    xorg-server xf86-video-amdgpu enlightenment` with `    vulkan-icd-loader vulkan-tools`; delete the `amdgpu virtual_display` paragraph (from `You also need an \`amdgpu\` kernel module configured to expose a virtual` through `Reboot after creating this file.`); replace the pre-auth key paragraph (from `A Tailscale account with a pre-auth key` through `lets anyone holding it register a device that will be admitted.`) with
```markdown
A Tailscale account. `ghostframe-server login` prints a sign-in URL; the
tailnet user who approves it owns the node, and every device of that user may
connect (see [Who can connect](#who-can-connect)). To join without a browser,
set `TS_AUTHKEY` to a **one-off** pre-auth key for that one `login`: a reusable
key that is still valid lets anyone holding it register an admitted device.
```
4. Replace everything from `## Install` up to (not including) `## First connection` with:
````markdown
## Install

As the desktop user whose session should be served — not root:

```bash
git clone https://github.com/mountain-reverie/ghostframe.git
cd ghostframe
just build-release
./target/release/ghostframe-server install    # binary, user unit, autostart
ghostframe-server login                       # prints a sign-in URL, waits for approval
systemctl --user start ghostframe-server
```

`install` copies the binary to `~/.local/bin/ghostframe-server`, writes
`~/.config/systemd/user/ghostframe-server.service` and
`~/.config/autostart/ghostframe-server.desktop` (some desktops, Enlightenment
among them, never activate `graphical-session.target`), and enables the unit.
From then on the server starts with your graphical session. It removes the
units of the old `packaging/install.sh` (attach or headless) and lists any
root-owned leftovers with the command to remove them; it never touches `/etc`.

### Security position

Any client admitted over the tailnet sees your desktop: your files, browser
sessions, saved passwords and sudo. Single-client eviction applies (one remote
client at a time). Access is limited to the tailnet user who owns the node plus
anyone in `GHOSTFRAME_ALLOWED_USERS` (see [Who can connect](#who-can-connect));
**everyone admitted gets full access to your session**, so keep the list short
and restrict the node in your tailnet ACLs as well.

### Max resolution

`install` caps what a remote client may set at **1920×1080**: the session is
the physical display, which a KVM or remote-management console may be
capturing as the machine's recovery view, and a mode it cannot show removes
that view exactly when it is needed. Change it with
`ghostframe-server install --force --max-resolution 2560x1440`, or
`--max-resolution none`. The value is `GHOSTFRAME_MAX_RESOLUTION` in the unit.

### Unattended boot

The session must exist to be captured. For a machine that should serve after
a reboot without anyone logging in, the display manager must log you in
automatically; `install` checks LightDM and prints the one root command when it
is not set up. Anyone who reaches the machine physically then finds a
logged-in desktop.

### Uninstall and logout

`ghostframe-server uninstall` removes the binary, unit and autostart entry and
keeps the tailnet state. `ghostframe-server logout` (with the service stopped:
`systemctl --user stop ghostframe-server`) removes the node from the tailnet
and deletes the state. `login`, `logout` and the service share one state
directory, `${XDG_STATE_HOME:-~/.local/state}/ghostframe` (mode 0700), and
refuse to run at the same time.

## Update

```bash
cd ghostframe
git pull
just build-release
./target/release/ghostframe-server install --force
```

`install --force` replaces the binary and restarts the service when the node
is logged in. Then hard-reload open browser tabs (Ctrl+Shift+R).
````
5. In "Adding users", Replace
```markdown
the daemon's
**user** unit (`<user>` is the account `install.sh` was run for; a drop-in
under `/etc/systemd/system/` has no effect on a user unit), then restart
the daemon:
```
with
```markdown
the server's **user** unit (a drop-in under
`/etc/systemd/system/` has no effect on a user unit), as the user it runs
as:
```
and the code block that follows (the `sudo mkdir ... ghostframe-server.service` lines) with
```bash
systemctl --user edit ghostframe-server
#   [Service]
#   Environment=GHOSTFRAME_ALLOWED_USERS=alice@example.com,tag:ops
systemctl --user restart ghostframe-server
```
6. In "First connection", Replace
```markdown
(the installer registers it as `<hostname>-ghostframe`); `<tailnet>` is
```
with
```markdown
(`login` and the service register it as `<hostname>-ghostframe`); `<tailnet>` is
```

Check: `grep -n "install.sh\|headless\|ghostframe.target\|machinectl\|virtual_display\|attach" README.md` prints nothing.

- [ ] **Step 2: DEVELOPERS.md**

1. Crate table: Replace
```markdown
| `ghostframe-server` | Standalone X11/DRM capture daemon that hosts a session over an embedded Tailscale node. |
```
with
```markdown
| `ghostframe-server` | The server binary: `install`/`login`/`capture` commands; captures the user's X11 session and hosts it over an embedded Tailscale node. |
```
2. In "Environment variables", Replace
```markdown
any `ghostframe-client-*` crate reads `std::env` anywhere else. Other crates
(ghostframe-server, test-pattern, the Go bridge) have not been converted yet and read
their variables at the point of use.
```
with
```markdown
any `ghostframe-client-*` crate reads `std::env` anywhere else. `ghostframe-server`
follows the same rule (`ghostframe-server/src/config.rs`). The test pattern and
the Go bridge read their variables at the point of use.
```
3. Delete the sections `## Alternative configurations` (with `### Intel iGPU`, `### NVIDIA (proprietary driver)`, `### Xorg + dummy driver (no display GPU)`, `### VKMS (virtual KMS kernel module)`) and `## WM alternatives`: everything from `## Alternative configurations` up to (not including) `## Contributing`. They describe the deleted headless install.
4. In "Running tests", after the paragraph ending `` `cargo test --test e2e`. `` add the paragraph
```markdown
The test patterns draw into the container's Xorg with `--x11-sink`, so the e2e
tests exercise the server's real capture path. The GPU setups still run that
Xorg on the host's VKMS device (`xorg-vkms.conf`).
```

- [ ] **Step 3: AGENTS.md**

1. Replace the deploy block
```bash
just build-release
sudo ./packaging/install.sh <user> --force     # --force overwrites the binary
sudo machinectl shell <user>@ /usr/bin/systemctl --user restart ghostframe.target
```
with
```bash
just build-release
./target/release/ghostframe-server install --force   # as the session's user; restarts the service if logged in
```
and the sentence before it stays `Deploying to a real session:`.
2. The "systemd units are *user* units" trap: Replace
```markdown
files but does **not** restart anything, and `install.sh` enables units without
restarting them.
```
with
```markdown
files but does **not** restart anything; `ghostframe-server install` restarts the
service only when the node is logged in.
```
3. In "Reading a live session", after its first code block add
```markdown
The service is `ghostframe-server.service` (`systemctl --user status
ghostframe-server`); `login`, `logout` and the service share a lock on the state
directory, so stop the service before `logout`.
```

- [ ] **Step 4: Smoke-test doc**

In `docs/m0-manual-smoke-test.md`, `./target/release/ghostframe-server` (in the section-1 code block) → `DISPLAY=:0 ./target/release/ghostframe-server capture`.

- [ ] **Step 5: Verify and commit**

```bash
ci/check-local-paths.sh
git grep -n "packaging/install.sh\|ghostframe-xdaemon\|--init" -- README.md DEVELOPERS.md AGENTS.md docs/m0-manual-smoke-test.md
git add README.md DEVELOPERS.md AGENTS.md docs/m0-manual-smoke-test.md
git commit -m "docs: rootless ghostframe-server install, login and capture

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

Expected: `check-local-paths.sh` passes; the grep prints nothing.

---

### Task 15: Verification gate

- [ ] **Step 1: The fast tier, exactly as CI runs it**

```bash
RUST_TEST_THREADS=1 ../gf-gpu-run just ci-local
xvfb-run -a -s "-screen 0 1280x1024x24 -noreset" cargo test --workspace --bins
```

Expected: `=== ci-local passed ===`, which includes the release-feature check against `-p ghostframe-server` and the software-Vulkan literal check on `target/release/ghostframe-server`; the second command (CI's `--bins` step, under Xvfb as in `ci.yml`) passes.

- [ ] **Step 2: The full e2e suite under the locks**

```bash
docker rm -f ghostframe-server headscale >/dev/null 2>&1 || true
flock -o ../.gf-heavy.lock ../gf-gpu-run just test-e2e 2>&1 | tee target/gf-x11-notes/e2e-full.log
echo "exit: ${PIPESTATUS[0]}"
```

Expected: `exit: 0`. (`tee` hides the exit status; read `PIPESTATUS`, never the pipe's.) `just test-e2e` rebuilds the web client and both images first, so a stale image cannot be tested. Known flakes (AGENTS.md: `full_frame_keyframe_interval`, a browserless `.expect("scene ran")`, `new_page` timeouts): note the failing test's name, re-run it alone once with `cargo test --test e2e -- <name> --exact --test-threads=1` under the same locks, and treat a second failure as real.

- [ ] **Step 3: Re-check every test whose capture rate changed meaning**

Spec section 7: tests that set `CAPTURE_FPS_DRM_DIRECT` are re-checked, not assumed. They now set `CAPTURE_FPS`. Run each alone and record pass/fail in `target/gf-x11-notes/fps-recheck.txt`:

```bash
for t in e2e_palrle_5pct_loss_chromium e2e_solid_per_tile_pixels e2e_palrle_exact_pixels \
         e2e_palrle_oob_index e2e_palrle_session_reset_chromium e2e_production_scale_with_loss_converges \
         e2e_saturated_link_starves_tiles_into_giving_up e2e_production_shaped_session_strands_no_tiles \
         e2e_text_clarity e2e_indices_raw_handshake e2e_lossless_golden_png; do
  docker rm -f ghostframe-server headscale >/dev/null 2>&1 || true
  if flock -o ../.gf-heavy.lock ../gf-gpu-run cargo test --test e2e -- "$t" --exact --test-threads=1 >/dev/null 2>&1
  then echo "$t pass"; else echo "$t FAIL"; fi
done | tee target/gf-x11-notes/fps-recheck.txt
```

Expected: every line `pass`. (`e2e_saturated_link_starves_tiles_into_giving_up` "fails on master by design" per project notes; compare with its result on `master` before calling it a regression.) A `FAIL` elsewhere: run it with `--nocapture`, compare the server log's `capture running` frame counts with the value the test asked for, and fix the test's expectation only if the capture rate, not the product, is the cause; otherwise it is a regression of this branch.

- [ ] **Step 4: Confirm the e2e capture path is X11**

```bash
grep -h "X11 capture connected to display" target/gf-x11-notes/e2e-full.log | sort | uniq -c | head
grep -c "DRM capture" target/gf-x11-notes/e2e-full.log
```

Expected: lines with `path=Shm` (the dummy and VKMS Xorg both offer MIT-SHM); `0` DRM lines. (The harness prints server logs on failure only; if the first grep finds nothing, run one sink test with `--nocapture` and `docker logs` it as in Task 2.)

No commit in this task unless Step 3 changed a test; then commit that test file alone with a message naming the test and the measured cause.

---

### Task 16: Live acceptance on evangeline (owner-run)

Spec section 9. **The owner runs these steps** as `cedric` on evangeline, without `sudo`; an agent prepares the build and records the results, and does not log the owner out or change the monitor mode itself.

- [ ] **Step 1 (owner): install and log in**

```bash
cd <ghostframe checkout>
just build-release
./target/release/ghostframe-server install
```

Expected output names, among others: `migrate: removed .../ghostframe-xdaemon.service`, `migrate: removed .../ghostframe-xdaemon.service.d` (the 2026-10-29 stopgap drop-in), `migrate: removed .../autostart/ghostframe-attach.desktop`, and either the LightDM line or the one root step. `systemctl --user list-unit-files 'ghostframe*'` lists only `ghostframe-server.service`.

The existing state (`~/.local/state/ghostframe`, seeded by the old unit) means `install` restarts the service at once; to also prove `login`, first `systemctl --user stop ghostframe-server && ghostframe-server logout`, then `ghostframe-server login` (opens through the sign-in URL) and `systemctl --user start ghostframe-server`.

- [ ] **Step 2 (owner): log out of the desktop and back in**

Expected: `systemctl --user status ghostframe-server` is active without a manual start, and a client sees the desktop **with a moving cursor** (the original bug: only the cursor moved). `journalctl --user -u ghostframe-server -b | grep "X11 capture connected"` shows `path=Shm`.

- [ ] **Step 3 (owner): a client resolution request**

From a client, request a resolution change (e.g. resize the native client window, or the browser client's display-mode request). Expected: Enlightenment re-lays out (shelf and maximised windows follow) and `xrandr --current` shows the new mode 10 s later, with no second `display mode applied` line in `journalctl --user -u ghostframe-server`. If it reverts, this is Task 1's outcome D: record it there and stop.

- [ ] **Step 4 (owner): logout is refused while the service runs**

```bash
ghostframe-server logout; echo "exit $?"
systemctl --user stop ghostframe-server
ghostframe-server logout; echo "exit $?"
```

Expected: the first prints `the ghostframe-server service is running; stop it first ...` and `exit 1`; the second prints `logged out; removed ...` and `exit 0`, and the node disappears from the tailnet admin console. Afterwards `ghostframe-server login` and `systemctl --user start ghostframe-server` to restore service.

- [ ] **Step 5: Record and commit**

Add a "Live acceptance" section at the end of this plan file with the date, the four results and any deviation, then:

```bash
git add docs/superpowers/plans/2026-10-29-ghostframe-server-x11.md
git commit -m "docs(plan): live acceptance of ghostframe-server on evangeline

Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01DGDCULokdqLykgiwofeg3N"
```

---

## Spec coverage

| Spec | Task |
|---|---|
| §1 root cause (per-window path chosen once, composited nothing) | 7 (path deleted, root grab only) |
| §2 non-goals (Wayland, headless X11, greeter, H.264 shearing, damage-driven encoding) | not implemented; `capture` refuses Wayland-only (11); `damage_tiles` stays filled but unused (6) |
| §3 rename + `main.rs` dispatch only | 3, 11 |
| §3 `install [--force] [--max-resolution WxH\|none]` | 12 |
| §3 `uninstall` keeps tailnet state | 12 |
| §3 `login [--hostname] [--control-url]`, `TS_AUTHKEY` | 11 (decision 4) |
| §3 `logout` deregisters then deletes state | 11 |
| §3 `capture` exit 2 + "run login"; Wayland error | 11 (decision 1) |
| §3 defaults: hostname `$(hostname)-ghostframe`, state dir chain, 0700 | 9, existing `ensure_private_dir` |
| §3.1 steps 1–7 (root refusal, copy, unit, autostart, migrate, enable/start, autologin hint) | 12 (decision 5) |
| §3.2 state lock on `capture`/`login`/`logout` | 10, 11 |
| §3.3 ghostbridge login/logout | 11 (decision 3: already exposed) |
| §4 layout `capture/x11/{frames,damage,input,display,cvt}.rs` behind `Backend` | 6 |
| §4 frames: SHM grab, recreate on resize, `GetImage` fallback, cursor at hotspot, geometry poll, CPU BGRA `stride = width*4` | 7 |
| §4 damage armed only with a client; input unchanged | 6 (moved) |
| §4 display: prefer existing mode, CVT otherwise, xrandr order; first task records E | 1, 8 |
| §4 startup wait for X 10 s | 6, 11 |
| §4 config renames/removals, env-read guard | 5, 7, 9 |
| §5 dma-buf import kept unused | nothing deleted in `ghostframe-lib`; 2 records the coverage fact |
| §6 deletions: `drm_capture.rs`, `drm`, `drm_gpu_pipeline.rs` | 5 |
| §6 `PerWindowComposite`, `pick_strategy`, `root_already_composited`, composite feature | 7 |
| §6 `--init` | 11 |
| §6 `packaging/`, `tests/packaging/mode_switch_test.sh` | 13 |
| §6 test-pattern `text_grid_drm.rs`, DRM paint loop, `--drm-direct` | 4 |
| §6 entrypoint DRM-direct branch, `CAPTURE_FPS_DRM_DIRECT`, VKMS config | 4, 5 (decision 6: config kept, detection dropped) |
| §7 X11 sink via ShmPutImage/PutImage; tests change flag only; FPS re-check; IngestTier from a log | 4, 15 (decisions 8, 9), 2 |
| §8 renames (harness, e2e Cargo note, Dockerfile, Justfile, workflows, ci scripts, docs, comments; not docs/superpowers or reviews) | 3, 14 |
| §9 unit tests (commands, templates, migration with temp HOME + canary, lock, SHM identity, fallback, cursor, RandR reuse + xrandr order, mutation checks) | 7, 8, 10, 11, 12 |
| §9 e2e after `containers-build` under the GPU lock; ci-local gate | 15 |
| §9 live acceptance 1–4 | 16 |

## Task 1 findings

Recorded 2026-10-29 on evangeline: X.Org 1.21.1.25 (amdgpu), Enlightenment
0.27.99.25357, output `HDMI-A-1` fed by a PiKVM (EDID vendor `LNX`, product
0x7771). Logs in `target/gf-x11-notes/` (not committed).

- **First attempt blocked: X session on a background VT.** From 13:35, seat0's
  active VT was tty1. Xorg logged `get vblank counter failed` every 500 ms, and
  *every* modeset failed (`xrandr: Configure crtc 0 failed`, with or without
  `--panning 0x0`). No events were sent. `loginctl activate <session>`
  (no root) brought it back. A server on an inactive session cannot change
  modes at all.
- `state-before.txt`: screen 1920x1080, CRTC on mode 91 (1920x1080 @ 50 Hz),
  panning 1920x1080+0+0. The output's **preferred** mode is 1280x720 @ 60 Hz
  (mode 90).
- `xrandr-shrink` (1280x720): `crtc off, output off, screen 1920x1080,
  crtc 1280x720, output on, …, screen 1280x720`. Final 1280x720. **Kept.**
- `xrandr-grow` (1920x1080): `crtc off, screen 1920x1080, crtc 1920x1080`, then
  **~1.7 s later another client set the CRTC back to mode 90 (1280x720) and
  shrank the screen to 1280x720**. xrandr's own trailing `RRSetPanning` failed
  with BadMatch because of that revert (rc=1). Final 1280x720. **Reverted.**
- `ours-grow` (XrandrDisplay, 1920x1080): `screen 1920x1080, crtc 1920x1080`
  (no CRTC disable), then the same revert to mode 90 ~1.7 s later. Final
  1280x720. **Reverted.** (`ours-shrink` not run: the screen was already at
  1280x720.)
- `xrandr --verbose` order: `crtc 0: disable`, `screen 0: WxH`, `crtc 0: mode`,
  confirmed.
- `Screen` mm: xrandr 341x192 (142.88 dpi), ours 508x286, E's revert 227x128 or
  338x190. The DPI differs, but it does not matter: both get reverted.
- **Cause:** Enlightenment's `e_randr2` in profile `standard` has
  `restore = 1` and a saved entry for this exact monitor id
  (`HDMI-A-1/<PiKVM EDID>`) at **1280x720 @ 60 Hz**. On any screen change,
  E re-applies its saved config for the connected monitor. The way the change
  is made does not matter. Shrinking to 1280x720 "worked" only because it
  matched E's saved mode. The 2026-10-29 attach symptom ("E stuck at
  1280x720") is the same mechanism.
- **Outcome: D.** It is neither event order (A), DPI (B) nor panning (C).
  Making our request look like `xrandr` would change nothing. Task 8 as
  written does not fix the revert. It needs an owner decision before it runs.
