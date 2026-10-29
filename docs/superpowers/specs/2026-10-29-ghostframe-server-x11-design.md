# ghostframe-server — X11 capture rework and rootless install — design

**Status:** approved for planning
**Date:** 2026-10-29
**Related:** [attach mode](2026-09-25-attach-mode-design.md), [M4b](2026-09-24-native-client-m4b-design.md) (display negotiation)

`ghostframe-xdaemon` becomes `ghostframe-server`: one binary with `install`,
`uninstall`, `login`, `logout` and `capture` commands, installed and run as the
desktop user without root. Capture is reduced to one X11 method. DRM capture,
the per-window composite path and the root-only headless deployment are deleted.
A PipeWire/Wayland backend is a later iteration behind the same seam.

---

## 1. Why

**The attach install on evangeline showed only a moving cursor.** Root cause
(diagnosed 2026-10-29, from the live journal and a manual `xwd -root`):

- `x11_capture::pick_strategy` probes once, at startup, whether Enlightenment
  paints the composited scene into root. The probe (`root_already_composited`)
  needs a redirected root child at least 64x64. At autologin there is none:
  root's 17 children are Enlightenment's 1x1 `e_layer_win` anchors and helpers.
- The inconclusive probe (`None`) falls through to `PerWindowComposite`, and the
  choice is never re-evaluated. In production that path composited nothing:
  `children=16 composited=0 skipped_bad_match=11`. The frame was empty; the
  XFixes cursor blended on top was the only moving content.
- `GetImage(root)` on the same display returns the full live desktop.
  Forcing it (`GHOSTFRAME_FORCE_ROOT_GET_IMAGE=1`, a user drop-in installed as a
  stopgap) fixed the session; the user confirmed it.

**Why not PipeWire for this iteration.** PipeWire carries a stream someone else
produces; on X11 nobody produces one. Every xdg-desktop-portal ScreenCast
backend (GNOME, KDE, wlroots) sources pixels from a Wayland compositor; the KDE
portal fails `CreateSession` on X11; Enlightenment has no portal backend at all;
GNOME 50 removed its X11 session. Feeding PipeWire from an X11 grab would keep
the X11 fragility and add a hop. So X11 stays an X11 grab, done the simplest way
that works (the x11vnc / ffmpeg `x11grab` method), and PipeWire arrives with the
Wayland backend.

**Why rootless.** Attach mode captures the user's own session. Nothing in that
path needs root once DRM capture is gone: on evangeline `/dev/dri/renderD128` is
`crw-rw-rw-`, so Vulkan and VA-API need no group membership. The only
root-requiring piece of the old install was LightDM autologin, which `install`
now reports instead of writing.

## 2. Non-goals

- **Wayland / PipeWire / portal capture.** Next iteration, as its own spec. That
  backend owns headless sessions (owned compositor, virtual outputs), libei
  input and restore tokens.
- **Headless X11.** Deleted, not ported (section 6). Headless returns
  Wayland-only.
- **The greeter.** No unprivileged process can show or drive LightDM, GDM or
  SDDM. Out of scope permanently for a user install.
- **The H.264 shearing artifacts at 1920x1080.** Observed during this diagnosis
  only during TileCodec→H264 bursts, cleared by the lossless handoff repaint, no
  loss or NACKs: an encoder or decoder bug, not capture. Separate follow-up
  session.
- **Damage-driven encoding.** `FrameSubmission::damage_tiles` is ignored by the
  library today; this work does not wire it up.

## 3. Commands

The crate and binary are renamed `ghostframe-server` (`ghostframe-xdaemon/` →
`ghostframe-server/`). `main.rs` becomes command dispatch only.

| Command | Behaviour |
|---|---|
| `install [--force] [--max-resolution WxH\|none]` | See 3.1. |
| `uninstall` | Disable and remove the unit, the autostart file and `~/.local/bin/ghostframe-server`. Keeps the tailnet state (owned by `logout`). |
| `login [--hostname H] [--control-url U]` | Bring the tsnet node up, print the sign-in URL, wait until the node is authorized, exit 0. With `TS_AUTHKEY` set, authorize non-interactively (no URL). Replaces `--init`. |
| `logout` | Deregister the node from the control server and delete the state directory. |
| `capture` | The daemon (section 4). Exits with code 2 and "run `ghostframe-server login`" if the state is not seeded. If `WAYLAND_DISPLAY` is set and `DISPLAY` is not, exits with a clear "Wayland sessions are not supported yet" error. |

Defaults are unchanged: hostname `$(hostname)-ghostframe`, state directory
`$TS_STATE_DIR`, else `$XDG_STATE_HOME/ghostframe`, else
`~/.local/state/ghostframe`, created 0700 and refused if looser (existing
`ensure_private_dir`).

### 3.1 `install`

1. Refuse to run as root (uid 0).
2. Copy the running executable to `~/.local/bin/ghostframe-server`. If the
   target exists and differs, refuse without `--force`. If the running
   executable already is the target, skip the copy.
3. Write `~/.config/systemd/user/ghostframe-server.service`, from a template
   embedded in the binary:
   - `ExecStart=%h/.local/bin/ghostframe-server capture`
   - `After=graphical-session.target`, `PartOf=graphical-session.target`,
     `WantedBy=graphical-session.target`
   - `Environment=TS_STATE_DIR=%S/ghostframe`, `DISPLAY=:0`, and
     `GHOSTFRAME_MAX_RESOLUTION` from `--max-resolution` (default `1920x1080`;
     `none` omits it)
   - the restart policy and the attach-mode warning comment of today's
     `ghostframe-xdaemon-attach.service.tmpl`
4. Write `~/.config/autostart/ghostframe-server.desktop`
   (`Exec=systemctl --user start ghostframe-server.service`). This is needed
   because Enlightenment does not activate `graphical-session.target`.
5. **Migrate:** stop and disable `ghostframe-xdaemon.service`, then remove:
   - `ghostframe-xdaemon.service` and its `.service.d/` drop-ins (including the
     2026-10-29 stopgap);
   - the headless units if present (`ghostframe.target`, `ghostframe-wm.service`,
     `ghostframe-xorg*.service`);
   - `~/.config/autostart/ghostframe-attach.desktop`.

   Only files whose names it knows are removed. Root-owned leftovers in `/etc`
   (getty or LightDM autologin, `/etc/X11/ghostframe-headless.conf`) are listed
   with the command to remove them, never touched.
6. `systemctl --user daemon-reload`, then `enable` the unit. If the state is
   seeded, also (re)start it, so the new binary replaces a running one. If it is
   unseeded, do not start it; print "run `ghostframe-server login`, then
   `systemctl --user start ghostframe-server`".
7. If no LightDM autologin for this user is detected, print the one root step
   needed for unattended boot. This is informational and does not fail.

### 3.2 State-directory lock

`capture`, `login` and `logout` take an exclusive `flock` on
`<state_dir>/.lock`. A second holder fails immediately: "the ghostframe-server
service is running; stop it first". This prevents two tsnet instances writing
one `tailscaled.state`, which nothing prevents today.

### 3.3 ghostbridge

- `login`: the existing `loginURL` helper (`ghostbridge/main.go`) reads
  `Status.AuthURL`. Expose it through the bridge FFI and poll until the backend
  state is `Running`.
- `logout`: new bridge call. Use the local client's `Logout`, then close the
  server.

## 4. X11 capture backend

Layout: `ghostframe-server/src/capture/x11/{frames,damage,input,display,cvt}.rs`
behind a `Backend` trait. The trait bundles what `GhostframeServer` already
consumes:

- a frame source producing `FrameSubmission`;
- `Arc<dyn InputInjector>`;
- `Option<Box<dyn DisplayController>>`.

The future `capture/pipewire/` implements the same trait. The library-facing
interfaces do not change.

**Frames.**
- One method: full-root grab via MIT-SHM (`ShmGetImage` into a shared segment
  sized to the root, recreated on resize). If MIT-SHM is absent or attaching the
  segment fails (remote display), fall back to `GetImage(root)`.
- Blend the XFixes cursor image at `(x - xhot, y - yhot)` with the existing
  `blit_cursor`. The root path does not do this today.
- Poll root geometry every frame, as today (`refresh_geometry`).
- Output: CPU `FrameSubmission`, BGRA, `stride = width * 4`, `dmabuf_fd: None`.

**Damage.** `XDamageMonitor`, unchanged: armed only while a client is connected,
used to skip idle frames.

**Input.** `XTestInjector`, unchanged.

**Display.** `XrandrDisplay` keeps the `DisplayController` contract.

*Requirement:* a server-initiated mode change must be indistinguishable, to the
window manager, from `xrandr --output <out> --mode <WxH>`.

*Evidence that it is not today* (2026-10-28 journal): 2 s after our 1920x1080
change, root went back to 1280x720 with no matching "display mode applied" line,
so a third party reverted it. We then re-applied, and Enlightenment kept laying
out at 1280x720. Enlightenment follows a manual `xrandr` change correctly.

*Changes:*
- Prefer an existing output mode that matches WxH. Create a CVT mode only when
  none does.
- Order the RandR requests (screen size, CRTC config) as `xrandr` does for grow
  and for shrink.
- *First implementation task:* on Enlightenment, record the RandR event stream
  for `xrandr` and for `XrandrDisplay` and close the difference. Both the
  ordering and the mode-reuse hypotheses are unproven until that recording
  exists.

**Startup.** Wait for X up to 10 s, as today. Then build the backend and hand it
to `GhostframeServer`.

**Configuration** stays environment-only through `config.rs` (the CI env-read
guard still applies). Renamed or removed variables:

- `GHOSTFRAME_ATTACH_MAX_RESOLUTION` → `GHOSTFRAME_MAX_RESOLUTION`
- `GHOSTFRAME_X11_CAPTURE_ONLY`, `GHOSTFRAME_FORCE_ROOT_GET_IMAGE`,
  `GHOSTFRAME_NO_X11_WAIT`: removed. There is no other path to choose, and the
  DRM-direct container no longer exists.

## 5. Library

No interface changes. **The dma-buf import path is kept, unused.** This covers
`FrameSubmission::dmabuf_fd`, `import_dmabuf`, `read_dmabuf_tight`, and the
`DmabufBuffer` / `CpuMmap` ingest tiers. After this change no producer exists.
The Wayland/PipeWire backend will need dma-buf ingest, and is expected to extend
it to DRM format modifiers and image import. This is an accepted leftover, not
an oversight.

## 6. Deletions

- `ghostframe-xdaemon/src/drm_capture.rs`, the `drm` dependency,
  `tests/drm_gpu_pipeline.rs`.
- In `x11_capture.rs`: `PerWindowComposite`, `pick_strategy`,
  `root_already_composited`, and the Composite feature of x11rb if nothing else
  uses it.
- The `--init` flag (replaced by `login`).
- All of `packaging/`: `install.sh`, `systemd/*`, `xorg-headless-*.conf*`,
  `lightdm-autologin.conf.tmpl`, `ghostframe-attach-autostart.desktop.tmpl`.
  Their replacements are the templates embedded for `install`.
- `tests/packaging/mode_switch_test.sh`.
- `ghostframe-test-pattern`: `text_grid_drm.rs`, the DRM-master paint loop and
  the `--drm-direct` flag (section 7).
- `tests/containers/test-server/entrypoint.sh`: the DRM-direct branch and
  `CAPTURE_FPS_DRM_DIRECT`. Drop the VKMS Xorg config (`xorg-vkms.conf` and its
  detection) unless planning finds another consumer.

## 7. e2e: test patterns paint into Xorg

About 15 e2e tests use `--drm-direct`: text-grid, solid-per-tile, palrle-exact,
gradient, tile-pattern photo/drift, the H.264 tests, the scroll scene and the
native-client gradient. It was added in `6fb034ab` because Xorg on VKMS
swallowed drawing while it held DRM master. It was a workaround, not a coverage
choice.

**Change.**
- Give `ghostframe-test-pattern` an X11 sink. The patterns already render into
  a BGRA buffer; the sink draws that buffer into the root window of Xorg `:99`
  (dummy driver) via `ShmPutImage`, falling back to `PutImage`. The DRM sink
  goes.
- Tests change `--drm-direct` to the X11 sink and nothing else.
- Each test that sets `CAPTURE_FPS_DRM_DIRECT=2` is re-checked under the new path
  rather than assumed to carry over.

**Coverage.**
- *Lost:* DRM capture, which is deleted anyway.
- *Gained:* every e2e test exercises the production capture path. Today none
  does.
- *dma-buf import:* not exercised today. Confirmed 2026-10-29 from an
  `e2e_cdf53_gradient_emission` log: the container finds no DRM writeback
  connector on `/dev/dri/card0`, so capture takes the CPU-mmap path over the
  modesetting framebuffer and no dma-buf is ever submitted (the DRM-master
  conflict assumed at design time is never reached).

**Host requirements (amended during planning).** VKMS stays a host requirement
for now: every `gpu: true` e2e setup (`e2e_setup.rs`, `scene.rs`) still starts
Xorg with `xorg-vkms.conf`. Only the entrypoint's VKMS auto-detection goes.
Moving those setups to the dummy driver is a follow-up with its own e2e run.

## 8. Renames outside the crate

- Process and binary name in:
  - the e2e harness (`scene.rs` `pidof`, `display_negotiation.rs`, `e2e.rs` log
    matches);
  - `ghostframe-e2e/Cargo.toml` build note;
  - the test-server `Dockerfile` (`-p`, manifest COPY, install path, literal
    check);
  - `Justfile` (`build-release`, the release literal check);
  - `.github/workflows/{ci,e2e}.yml`;
  - `ci/check-release-features.sh`, `ci/check-release-binary.sh`,
    `ci/check-env-reads.sh`.
- `README.md`, `DEVELOPERS.md` and `AGENTS.md`, including AGENTS.md's deploy
  recipe: `install` replaces `sudo ./packaging/install.sh`.
- Comments in `ghostframe-lib`, `ghostframe-cli` and the client crates that name
  the daemon.
- **Not renamed:** `docs/superpowers/` plans and specs (historical; that
  directory is slated for deletion after the waves), `docs/reviews/`.

## 9. Testing

**Unit tests** (CI; X11 tests under `xvfb-run`):
- *Commands:* argument parsing; rendering of the unit and autostart templates;
  migration removes exactly the known legacy names.
  - Install and migration tests run against a temporary `$HOME` with a canary
    file outside it. Every path derivation is stubbed (see the 2026-09
    test-deleted-real-files incident).
- *State lock:* `login` and `logout` fail while another process holds it.
- *Frames:*
  - the SHM grab is byte-identical to `GetImage(root)` on Xvfb;
  - the fallback is taken when SHM is unavailable;
  - the cursor lands at the hotspot.
  - The existing `blit_cursor` and XDamage tests carry over.
- *RandR:* mode selection reuses an existing mode and creates CVT only
  otherwise; the request order matches a recorded `xrandr` sequence on Xvfb.
- *Mutation check:* every new test is shown to fail with the code it covers
  broken (AGENTS.md).

**e2e:**
- Full suite after `just containers-build`, under the GPU lock.
- Confirm the dma-buf coverage claim (section 7) from a log first.

**Gate:** `just ci-local`, including the release-feature check against
`-p ghostframe-server`.

**Live acceptance on evangeline**, as `cedric`, no `sudo`:
1. `ghostframe-server install` removes the xdaemon units and the stopgap
   drop-in; `ghostframe-server login` completes through the sign-in URL.
2. Log out of the desktop and back in. The service starts by itself, and a
   client sees the desktop with a cursor.
3. A client resolution request: Enlightenment re-lays out to the new size, and
   no revert happens within 10 s.
4. `logout` fails while the service runs. After `systemctl --user stop`, it
   succeeds and the node is gone from the tailnet.
