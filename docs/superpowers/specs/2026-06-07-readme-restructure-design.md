# README restructure + headless-Xorg install path

## Problem

The current `README.md` is developer-oriented: build, test, lint, contribute.
There is no documented path for an end user (or sysadmin) to install
ghostframe on a Linux box and have a headless Xorg session running a window
manager auto-start at boot for a specific user.

The repository ships no install artifacts at all — no `install.sh`, no
systemd units, no sample `xorg.conf` outside the e2e test-server container.
So "install ghostframe" today means "read the README, build it, then design
your own deployment from scratch".

## Goal

Split the documentation into two files:

- `README.md` becomes a short install-and-first-connection guide for someone
  who wants to deploy ghostframe on a Linux machine and stream a headless
  desktop session to their browser.
- `DEVELOPERS.md` (English spelling — the user's original "DEVELOPPERS.md"
  was the French form; English chosen for the actual filename) absorbs the
  current README's developer content plus alternative-configuration notes.

Ship the minimum in-tree artifacts the README needs to be short: an
`install.sh`, a sample `xorg.conf`, four systemd unit files, and one
`getty` autologin drop-in template.

## Decisions

### Architecture

The install path uses real-GPU headless Xorg + autologin + user-level
systemd units. Specifically:

| Concern | Choice | Rationale |
|---|---|---|
| Doc structure | README (install) + DEVELOPERS.md (dev/alt configs) | User asked for the split |
| Install artifacts | Ship in-tree under `packaging/` (option B from brainstorm) | Without artifacts the README would be a wall of manual steps |
| Boot model | Autologin → real PAM session → `systemd --user` units | Other options skip PAM and leave enlightenment without a proper session bus / seat / `XDG_SESSION_*` |
| Headless Xorg | Real GPU + `VIRTUAL` connector on `amdgpu` | Xorg dummy has no DRM → no hardware encode. VKMS dodges nothing because it's not on a GPU (capture still has to cross-import to the real GPU's encoder, which is the broken PRIME case). Real GPU + virtual connector keeps the local monitor untouched and the encode path on the same device |
| systemd topology | Three user units under a `ghostframe.target` | Per-component logs/restart, idiomatic; `WantedBy=default.target` so login auto-pulls it |
| `TS_AUTHKEY` handling | Required only at install time; seeds tsnet state dir via `--init`; never again | Cleanest no-secret architecture (host `tailscaled`, C2) is a much larger refactor that doesn't belong in a docs-restructure PR |
| WM in README | Enlightenment as the documented default, swap-via-one-line | User's framing was enlightenment-as-example; concrete README beats WM-agnostic prose; DEVELOPERS.md lists alternatives |
| Driver coverage in README | `amdgpu` only | We have not tested Intel/NVIDIA install paths; DEVELOPERS.md documents the deltas as snippets, not as supported configurations |

### Non-goals (explicitly out of scope)

- Switching off embedded `tsnet` to depend on host `tailscaled` (C2). This
  is architecturally cleaner and removes secrets entirely, but it's a
  separate milestone: rip out `ghostbridge`, rebind the QUIC listener to a
  host interface, rework the e2e harness, rewrite M0. Tracked as future
  work; this spec does not block on it.
- Distro packaging (`.deb`, PKGBUILD, AUR). Out of scope; the install path
  is a shell script.
- Intel and NVIDIA `xorg.conf` testing. We document the deltas but do not
  promise they work.
- VKMS as a primary configuration. Memory note
  `feedback_vkms_cross_gpu_dmabuf.md` documents the PRIME cross-device bug;
  the workaround (CPU-mmap) gives no win over Xorg dummy. VKMS gets a
  paragraph in DEVELOPERS.md saying "not useful today, revisit when the
  PRIME bug is fixed".
- Xorg + dummy driver as a documented fallback. Defer — if a user without
  a supported GPU asks, we add it later. Avoiding the temptation to ship
  configurations we haven't exercised.
- Multi-distro `install.sh`. The script runs on any distro whose packages
  are present; package installation is the README's "Prerequisites" step,
  with one box per distro (Arch, Ubuntu 24.04).

## Repository changes

### New: `packaging/`

```
packaging/
  install.sh
  xorg-headless-amdgpu.conf
  systemd/
    ghostframe.target
    ghostframe-xorg.service
    ghostframe-wm.service
    ghostframe-xdaemon.service
    getty-autologin.conf.tmpl
```

### Modified: `README.md`

Shrinks to roughly five sections, in this order:

1. **What is ghostframe** — one paragraph, links to the full spec.
2. **Prerequisites** — Ubuntu 24.04 `apt` line and Arch `pacman` line. Because
   we don't ship pre-built binaries today, this list covers both runtime
   (Xorg server, `xf86-video-amdgpu`, enlightenment, `vulkan-icd-loader`,
   ffmpeg runtime libs) *and* build-from-source dependencies (clang, libclang,
   the `-dev` packages for ffmpeg/x264/X/DRM, golang, Rust, Node 20+, npm).
   Developer-only tooling (`cbindgen`, Docker, `just`) is *not* in the README's
   prerequisites and lives in DEVELOPERS.md instead — the README's install
   path doesn't need them.
3. **Install** — three steps:
   1. `cargo build --release -p ghostframe-xdaemon && (cd ghostframe-web-client && npm install && npm run build)`
   2. `sudo ./packaging/install.sh <username>`
   3. Paste your `TS_AUTHKEY` when prompted, reboot.
4. **First connection** — explain where to find the cert hash printed at
   first boot (it's in `journalctl --user -u ghostframe-xdaemon` for
   `<username>`), and how to open the browser URL.
5. **More** — links to DEVELOPERS.md and `docs/specs/ghostframe-initial-spec.md`.

### Modified: `DEVELOPERS.md` (new file)

Absorbs from the current README:

- Repository layout table
- Toolchain prerequisites (Rust, cbindgen, Node 20+, Docker, just)
- Building (workspace + web client)
- Running unit tests and e2e tests
- Container rebuild caveat
- Lint and format
- Contributing

Adds new sections:

- **Alternative configurations** — Intel `Virtual_1`, NVIDIA
  `AllowHeadlessMode`, Xorg+dummy as a no-GPU fallback (one paragraph,
  not a full recipe), VKMS not-useful-today note.
- **WM alternatives** — one-line table mapping i3 / openbox / sway-X11 /
  fluxbox to the `ExecStart=` line they need in
  `ghostframe-wm.service`.
- **Manual smoke test** — keep the link to `docs/m0-manual-smoke-test.md`.

### Modified: `ghostframe-xdaemon/src/main.rs`

Two changes, both small:

1. `TS_AUTHKEY` becomes optional. Today: `env::var("TS_AUTHKEY").expect(…)`.
   New behaviour: if `TS_AUTHKEY` is absent and `TS_STATE_DIR` already
   contains a populated tsnet state, proceed with empty authkey (tsnet
   reuses saved auth). If absent AND state dir is empty, error with a
   message pointing at the `--init` flag and the install script.
2. Add `--init` flag. With `--init`: require `TS_AUTHKEY`, perform the
   tsnet join (which writes auth blob into `TS_STATE_DIR`), then exit
   `0` without entering the capture loop. Without `--init`: today's
   behaviour (with the optional-authkey relaxation above).

The exact tsnet "is this state dir already authenticated" check we'll
need to confirm against `ghostbridge` — it may already do the right thing
when authkey is empty and state is present. If so, the change is purely
in `main.rs` argument parsing.

## Unit file shapes

These are sketches, not the final text — exact values land in the
implementation plan.

### `ghostframe.target` (user)

```ini
[Unit]
Description=Ghostframe headless desktop session
Requires=ghostframe-xdaemon.service
After=ghostframe-xdaemon.service

[Install]
WantedBy=default.target
```

### `ghostframe-xorg.service` (user)

```ini
[Unit]
Description=Ghostframe headless Xorg server (:1)
Before=ghostframe-wm.service

[Service]
Type=simple
ExecStart=/usr/bin/Xorg :1 -config /etc/X11/ghostframe-headless.conf -noreset -nolisten tcp vt7
Restart=on-failure
```

### `ghostframe-wm.service` (user)

```ini
[Unit]
Description=Ghostframe window manager
Requires=ghostframe-xorg.service
After=ghostframe-xorg.service

[Service]
Type=simple
Environment=DISPLAY=:1
# Swap this line to change WM (see DEVELOPERS.md)
ExecStart=/usr/bin/enlightenment_start
Restart=on-failure
```

### `ghostframe-xdaemon.service` (user)

```ini
[Unit]
Description=Ghostframe capture + transport daemon
Requires=ghostframe-wm.service
After=ghostframe-wm.service

[Service]
Type=simple
Environment=DISPLAY=:1
Environment=TS_HOSTNAME=%H-ghostframe
Environment=TS_STATE_DIR=%h/.local/share/ghostframe/ts-state
ExecStart=/usr/local/bin/ghostframe-xdaemon
Restart=on-failure
```

No `TS_AUTHKEY` is ever set in any unit. The state dir, seeded by
`install.sh --init`, carries the auth.

### `getty-autologin.conf.tmpl` (system)

```ini
[Service]
ExecStart=
ExecStart=-/sbin/agetty -o '-p -- \\u' --autologin __USER__ --noclear %I $TERM
```

`install.sh` substitutes `__USER__`.

## `install.sh` behaviour

Invoked as `sudo ./packaging/install.sh <username>`. Refuses to run if not
root. Refuses to run if `<username>` does not exist.

Steps, in order, each printing a single-line progress message:

1. **Preflight**: verify `xorg-server`, `xf86-video-amdgpu`,
   `enlightenment_start`, and either a built `./target/release/ghostframe-xdaemon`
   or an existing `/usr/local/bin/ghostframe-xdaemon` are present. Exit
   with a remediation message if any check fails.
2. **Binary**: copy `./target/release/ghostframe-xdaemon` → `/usr/local/bin/`
   (mode 0755, root-owned). Skip if `/usr/local/bin/ghostframe-xdaemon`
   already exists and `--force` was not passed.
3. **Xorg config**: copy `packaging/xorg-headless-amdgpu.conf` →
   `/etc/X11/ghostframe-headless.conf` (mode 0644).
4. **State dir**: create `~<username>/.local/share/ghostframe/ts-state`
   (mode 0700, owned by `<username>`).
5. **User units**: copy the four user unit files into
   `~<username>/.config/systemd/user/`, owned by `<username>`. Create the
   directory if needed.
6. **getty autologin**: write
   `/etc/systemd/system/getty@tty1.service.d/ghostframe-autologin.conf`
   by substituting `__USER__` in the template. The drop-in is
   ghostframe-namespaced so it doesn't collide with other things in the
   directory.
7. **Tsnet seed**: if `stdin` is a TTY and `~<username>/.local/share/ghostframe/ts-state`
   is empty, prompt for `TS_AUTHKEY` (read without echo) and run
   `sudo -u <username> TS_AUTHKEY=… TS_STATE_DIR=… /usr/local/bin/ghostframe-xdaemon --init`.
   If non-interactive or state dir already populated, skip with an
   appropriate message ("run the above before first boot" / "state
   already present, skipping").
8. **Enable**: `systemctl daemon-reload`,
   `sudo -u <username> XDG_RUNTIME_DIR=/run/user/$(id -u <username>) systemctl --user enable ghostframe.target`,
   `systemctl enable getty@tty1`. The `XDG_RUNTIME_DIR` dance is needed
   because the target user's manager isn't running yet when we run
   `install.sh`; on the first boot after install, autologin starts it.
9. **Done message**: print the connection URL template the user should
   open from another device on their tailnet:
   `http://<tailnet-host>:<port>/?host=<tailnet-host>:4443&certHash=<cert-hash>`.
   Note that the certHash is only known after the daemon starts, so the
   "first boot" instructions explain how to retrieve it via
   `journalctl --user -u ghostframe-xdaemon -b`.

Failure modes the script handles:

- Missing prerequisite → exit 1, print which one and the install command for the user's distro
- Re-run (idempotent) → re-copies files but does not re-prompt for
  `TS_AUTHKEY` if state dir is populated. `--force` ignores existence checks for binary and Xorg config (but never re-seeds tsnet, to avoid surprise re-auth).
- Non-interactive (no TTY): completes steps 1–8 but skips step 7; prints
  the exact one-shot command to run later.

## Sample `xorg-headless-amdgpu.conf`

A single-screen Xorg config that:

- Binds the `amdgpu` driver to the GPU.
- Disables auto-add for connected outputs (`AutoAddGPU "off"`,
  `Option "Ignore" "true"` on non-virtual connectors) so the headless
  server cannot accidentally claim a physical monitor.
- Configures a single `Virtual` connector at 1920×1080 with a single
  Modeline (RandR can resize at runtime).
- Loads only the X modules needed for a Vulkan-capable session
  (`glx`, `dri3`).

Exact text lands in the implementation plan; this spec defines the
intent.

## Open questions deferred to implementation plan

1. Exact xorg.conf modeline / refresh rate.
2. Whether `install.sh` should also offer a `--uninstall` flow.
3. Whether to validate the supplied `TS_AUTHKEY` format before invoking
   `--init` (prefix `tskey-auth-`, length).
4. Where to print the certHash on first boot — `journalctl` is the
   default, but we may want a small helper (`ghostframe-status`) that
   wraps the parse. Probably out of scope.
5. The `ghostbridge` "is this state already authenticated" semantics — to
   be confirmed when implementing the `--init` flag.

## Acceptance criteria

- `README.md` is under ~80 lines and walks a user from clean Ubuntu /
  Arch install to a working browser connection in five numbered steps
  (3 install + 2 first-connection).
- `DEVELOPERS.md` exists and contains every section that was in the old
  README plus the new "alternative configurations" and "WM alternatives"
  sections.
- `packaging/install.sh` succeeds (exit 0) on a fresh Arch and a fresh
  Ubuntu 24.04 VM with the prerequisites pre-installed. After reboot,
  `systemctl --user status ghostframe-xdaemon` (as the configured user)
  is `active (running)` and the daemon's stdout contains a
  `CERT_HASH_SHA256=` line.
- `ghostframe-xdaemon` without `TS_AUTHKEY` but with a populated state
  dir joins the tailnet and runs the capture loop.
- `ghostframe-xdaemon --init` with `TS_AUTHKEY` seeds the state dir and
  exits 0; subsequent run without `TS_AUTHKEY` works.
- Existing tests (`cargo test --lib`, `just test-e2e`) continue to pass.
  This change does not alter the wire format or any encoder behaviour.

## Risks

- **Autologin + user-unit ordering races.** `pam_systemd` starts the user
  manager but `default.target` doesn't wait for graphical resources. If
  Xorg-as-user-unit starts before something it needs (a tty handover?
  seat assignment? VT permissions?), it'll fail in subtle ways. Mitigation:
  the user unit is `Type=simple` and `Restart=on-failure`; if the first
  attempt fails it'll retry once the seat is fully set up. We'll verify
  this on a real machine, not just in a VM.
- **`amdgpu` `VIRTUAL` connector availability.** This is a relatively new
  feature of the `amdgpu` kernel module / DDX. Older kernels may not
  expose it. The README states the minimum kernel version required
  (TBD during implementation — checked against the running mainline).
- **`Restart=on-failure` storms** if Xorg fails to come up at all (e.g.,
  user has no GPU access permissions). systemd's default rate-limiting
  will cool this down, but the log experience is noisy. Mitigation: keep
  it as-is; document `journalctl --user -u ghostframe-xorg -b` as the
  first thing to look at if the screen never comes up.
- **`getty@tty1` overlap with existing autologin.** If the user already
  has an `agetty.service.d/autologin.conf` (e.g. they were logging in
  another user automatically), we'll add a second drop-in. systemd merges
  drop-ins alphabetically; our `ghostframe-autologin.conf` will lose to
  earlier names. Mitigation: prefix our drop-in with `99-` so it always
  wins, and print a warning if any non-ghostframe drop-in already exists.
