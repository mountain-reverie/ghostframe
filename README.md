# Ghostframe

A Linux-only remote desktop server: stream a headless Xorg session running
your favourite window manager to a browser, over QUIC on a Tailscale
tailnet. Per-tile adaptive encoding (H.264 for motion, palette/wavelet/
solid-fill for static content) keeps bandwidth low and text crisp.

There are two halves, and you do not need both:

| | What it is | Build |
| --- | --- | --- |
| **Server** | `ghostframe-xdaemon` — headless Xorg, capture, encode, tailnet. Serves the browser client itself. | [Install](#install) |
| **Native client** | `ghostframe` — a Linux desktop client. No browser, no server. | [Native client](#native-client) |

Any device on the tailnet can also just open the server's URL in Chrome —
that is the browser client, and it needs nothing installed. The native
client is the alternative to *that*, not to the server.

**If you only want the native client, skip straight to [Native
client](#native-client).** It builds a strict subset of the workspace:
no Xorg, no GPU driver setup, no Node, no Docker.

For the full design see
[docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).
For building, testing, and developer tooling see
[DEVELOPERS.md](DEVELOPERS.md).

## Server prerequisites

For the native client only, see [Native client](#native-client) instead —
its dependency list is much shorter.

Ghostframe today does not ship pre-built binaries — you build from source
during install. The prerequisites cover both the runtime stack (Xorg + GPU
driver + WM + Vulkan) and the build-from-source toolchain.

The install path is supported on **AMD GPUs with the `amdgpu` driver**.
Other GPUs are documented in [DEVELOPERS.md](DEVELOPERS.md#alternative-configurations)
but not part of the supported install. The default window manager is
**Enlightenment**; alternatives are one `ExecStart=` line away (see
[DEVELOPERS.md](DEVELOPERS.md#wm-alternatives)).

**Ubuntu 24.04:**

```bash
sudo apt-get install \
    build-essential rsync pkg-config clang libclang-dev golang-go \
    rustc cargo \
    nodejs npm \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
    libx264-dev libx11-dev libxext-dev libxdamage-dev libdrm-dev \
    libvulkan1 mesa-vulkan-drivers vulkan-tools \
    xserver-xorg xserver-xorg-video-amdgpu enlightenment
```

**Arch Linux:**

```bash
sudo pacman -S base-devel rsync clang go rust nodejs npm \
    ffmpeg x264 libx11 libxext libxdamage libdrm \
    vulkan-icd-loader vulkan-tools \
    xorg-server xf86-video-amdgpu enlightenment
```

Then, on either distribution:

- **Rust must be at least 1.74**, and the workspace pins an exact toolchain
  in `rust-toolchain.toml`. Install via [rustup](https://rustup.rs/) and it
  is honoured automatically; a distro `rustc` older than the pin will not
  build the tree.
- **`just`** runs every build target here: `cargo install just`, or your
  distribution's package (`pacman -S just`, `apt-get install just` on
  24.04+).
- **Go 1.25+.** `ghostbridge` declares `go 1.25.5`; an older `go` in `PATH`
  downloads the right toolchain on first build (Go's default
  `GOTOOLCHAIN=auto`), which needs network access and a few minutes.
- **`rsync` and `make`** are used by `ghostbridge/Makefile` to stage the
  browser bundle for embedding. Missing `rsync` fails the build with a bare
  `rsync: command not found` from inside a build script.

You also need an `amdgpu` kernel module configured to expose a virtual
display. Add `/etc/modprobe.d/amdgpu.conf`:

```
options amdgpu virtual_display=<PCI_ID>,1
```

Find `<PCI_ID>` with `lspci -D | grep VGA` (use the form `0000:03:00.0`).
Reboot after creating this file.

A Tailscale account with a reusable pre-auth key
(https://login.tailscale.com/admin/settings/keys) is required to register
the host on your tailnet. The install script will prompt for it.

## Install

```bash
# 1. Clone and build from source. `just build` runs the web client SPA
#    build first (vite) so ghostbridge can //go:embed it before the
#    daemon links.
git clone https://github.com/mountain-reverie/ghostframe.git
cd ghostframe
just build-release

# 2. Run the installer as root, targeting the user that should own the
#    headless session.
sudo ./packaging/install.sh <username>

# 3. Paste your Tailscale auth key when the script prompts, then reboot.
sudo reboot
```

After reboot, the configured user is automatically logged in on `tty1`,
Xorg comes up on display `:1`, Enlightenment starts inside that session,
and `ghostframe-xdaemon` joins the tailnet and starts capturing.

## Attach Mode

Attach mode captures the operator's existing X session instead of creating a
dedicated headless session. This is useful when ghostframe is the primary
workload and the machine would otherwise have a local desktop session
contending for the same GPU.

### When to Choose Attach Mode

The fundamental constraint is that a GPU has exactly one DRM master at a time.
When two X servers need to drive the same GPU — one running the local desktop,
one running ghostframe — they contend for this mastership through VT switching.
The local session's display is disrupted while ghostframe is active, and vice
versa.

Attach mode avoids this contention by running ghostframe in the *same* X
session as the local desktop, so there is no competition for the DRM master.
This makes sense when the remote session *is* the primary use of the machine.
It also makes the local output more useful rather than less: a monitor or KVM
attached to the host shows exactly what the remote client sees, rather than a
second, unrelated desktop.

### What It Does Not Require

Unlike headless mode, attach mode does not need:

- The `amdgpu virtual_display=` kernel module option or a reboot to install it
- VKMS (Virtual Kernel Mode Setting)
- A dedicated non-interactive user (it captures the existing operator's session)
- A getty autologin on `tty1`
- Loosening of `Xwrapper.config` to allow X servers to run as the current user

The operator's existing X session becomes the capture source, whichever window
manager it runs. The unit does hardcode `DISPLAY=:0`, though, so a host that puts
its graphical session somewhere else — a second seat, or a lightdm config that
moves it — needs that changed before the daemon finds anything to capture.

### Security Position

In attach mode, any client that connects to the tailnet endpoint sees the
operator's own desktop: their files, browser sessions, saved passwords, and
sudo privileges. There is no sandbox or separation from the local account.

Single-client eviction still applies — only one remote client can attach at a
time — but the tailnet is the only access boundary. **If you are choosing attach
mode, you should understand that anyone with tailnet access to the ghostframe
endpoint gets full access to that account's session.**

### Max Resolution

By default, attach mode caps the remote client to a maximum resolution of
**1920×1080**. The reason is that attach mode is capturing the physical display,
which may itself be captured by a KVM switch or remote management system as a
recovery view. If a remote client sets a video mode that the recovery path cannot
capture, you lose access at the moment you need it most.

To override this limit once you are confident recovery access is not needed:

```bash
sudo ./packaging/install.sh <user> --mode attach --max-resolution none
```

The flag writes `GHOSTFRAME_ATTACH_MAX_RESOLUTION` into the installed unit, and
the installer prints the value it used. If a client with a larger display than
1080p gets 1080p anyway, that variable is what to look for.

### Installation

Attach mode requires **lightdm** as the display manager. The installer will error
rather than silently skip if lightdm is not present, since silent failure after
reboot is worse than failing loudly.

If you do not already have lightdm installed:

**Ubuntu 24.04:**
```bash
sudo apt-get install lightdm
```

**Arch Linux:**
```bash
sudo pacman -S lightdm
```

Then install ghostframe in attach mode:

```bash
# 1. Clone and build from source (same as headless).
git clone https://github.com/mountain-reverie/ghostframe.git
cd ghostframe
just build-release

# 2. Run the installer in attach mode, targeting the user who owns the local session.
sudo ./packaging/install.sh <user> --mode attach

# 3. Optionally cap the maximum resolution (default is 1920×1080):
#    sudo ./packaging/install.sh <user> --mode attach --max-resolution 2560x1440

# 4. Paste your Tailscale auth key when prompted. Unlike headless mode,
#    no reboot is necessary — the changes take effect immediately.
```

After the install completes, `ghostframe-xdaemon` is installed as a user service
for the specified account. It will start automatically on the next login, or can
be started immediately if already logged in.

### Unattended Boot Behavior

In attach mode, the session must exist for ghostframe to capture it. On an
unattended boot, this means the specified user remains logged in automatically
(via lightdm). This is by design: a machine whose purpose is to serve that
session remotely has no competing need for a local logout. The consequence is
worth stating plainly, though: anyone who reaches the machine physically after a
boot finds a logged-in desktop, not a login prompt. If that is not acceptable,
use headless mode, which runs its own session under a separate account.

## Update

To pick up a new version, rebuild from source and re-run the installer
with `--force` so the binary and Xorg config are overwritten in place.
The tsnet state dir is preserved (step 6 of `install.sh` self-skips when
it's already seeded), so you do not need to re-paste your auth key.

```bash
# 1. Pull and rebuild.
cd ghostframe
git pull
just build-release

# 2. Reinstall over the existing files.
sudo ./packaging/install.sh <username> --force

# 3. Restart the session so Xorg and the daemon pick up the new binary.
#    A reboot also works.
sudo -u <username> XDG_RUNTIME_DIR=/run/user/$(id -u <username>) \
    systemctl --user restart ghostframe.target
```

Restarting `ghostframe.target` tears down Xorg, which will drop any
active browser sessions — they auto-reconnect once the daemon is back.

## First connection

On any device on the same tailnet, open

```
https://<hostname>-ghostframe.<tailnet>.ts.net/
```

in Chrome / Chromium / Edge. `<hostname>` is your machine's hostname
(the installer registers it as `<hostname>-ghostframe`); `<tailnet>` is
your tailnet's MagicDNS suffix.

The daemon serves the web client and its WebTransport cert hash
directly, so no manual setup is required on the client device. The
page uses a Tailscale-issued Let's Encrypt certificate — make sure
**HTTPS Certificates** are enabled at
<https://login.tailscale.com/admin/dns> before connecting for the
first time.

## Native client

`ghostframe` is a Linux desktop client: it joins the tailnet as its own
node, connects to a ghostframe server, and opens a single window (X11 or
Wayland) rendering the remote session. It decodes and composites on the
GPU via Vulkan, so it needs a working Vulkan driver but no Xorg
configuration, no `amdgpu` virtual display, and no window manager of its
own.

It is **independent of the server install above.** Nothing on this page's
install path is required to build or run it, and it does not need Node,
Docker, Xorg, or the browser bundle.

### Prerequisites

**Ubuntu 24.04:**

```bash
sudo apt-get install \
    build-essential pkg-config clang libclang-dev golang-go \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
    libx11-dev libxext-dev libxcb1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libdrm-dev \
    libvulkan-dev mesa-vulkan-drivers \
    vulkan-tools vainfo          # diagnostics, not build deps
```

**Arch Linux:**

```bash
sudo pacman -S base-devel clang go ffmpeg \
    libx11 libxext libxcb libxkbcommon libxkbcommon-x11 wayland libdrm \
    vulkan-icd-loader \
    vulkan-tools libva-utils     # diagnostics, not build deps
```

Plus a Vulkan ICD for your GPU (`vulkan-radeon`, `vulkan-intel`, or the
NVIDIA driver), Rust via [rustup](https://rustup.rs/) — the workspace pins
its toolchain in `rust-toolchain.toml` — and `just` (`cargo install just`).
Confirm Vulkan works before building: `vulkaninfo --summary` should list
your GPU.

**VA-API is optional but wanted.** At connect time the client probes
whether it can decode H.264 through VA-API on `/dev/dri/renderD128`, and
advertises that in its HELLO. Without it the session still works — the
server falls back to sending tile codecs — so a missing driver costs
bandwidth on video-heavy content, not the connection. Check with `vainfo`
(`libva-utils` / `vainfo`) for an `H264` *VLD* entry; installing
`libva-mesa-driver` or `intel-media-driver` is what usually supplies it.

Note what is **not** here: no `nodejs`/`npm`, no `rsync`, no
`xserver-xorg`, no window manager, no Docker. `clang`/`libclang-dev` is not
optional — the FFmpeg bindings generate their headers with bindgen at build
time.

### Build

```bash
git clone https://github.com/mountain-reverie/ghostframe.git
cd ghostframe
just build-client-release     # debug build: just build-client
```

The binary lands at `target/release/ghostframe`. Copy it wherever you like;
it has no install script and no runtime files beyond its own tailnet state.

`build-client` builds only the crates the client is made of, never the
server or the browser bundle. The mechanism is `ghostframe-tsnet`'s
`web-embed` Cargo feature: with it off, `ghostbridge` is compiled
`-tags noweb` and drops the `//go:embed` of the browser SPA, so no web
toolchain is involved. `ghostframe-lib` (the server) turns it back on. See
[DEVELOPERS.md](DEVELOPERS.md#building-only-the-native-client).

The first build compiles the Tailscale Go module (~4 min) and then the
wgpu/naga/FFmpeg stack (~29 min for a debug build on a 6-core ARM box with
4 GB RAM, `CARGO_BUILD_JOBS=3`). Budget half an hour and a few GB of disk;
incremental rebuilds are seconds. On a machine with little RAM, cap the
parallelism — peak memory is in linking, not compiling:

```bash
CARGO_BUILD_JOBS=2 just build-client-release
```

Go pulls its own toolchain on the first build if your `go` is older than
`ghostbridge`'s `go 1.25.5`, so the first run needs network access even
after `git clone`.

### Use

```bash
# 1. Join the tailnet. Prints a URL to authorise the node in your browser.
ghostframe login

#    Or non-interactively, with a pre-auth key:
ghostframe login --authkey tskey-auth-...

# 2. Connect to a ghostframe server by its tailnet hostname.
ghostframe connect <hostname>-ghostframe
```

`login` stores tailnet state in `$XDG_STATE_HOME/ghostframe/tsnet`
(default `~/.local/state/ghostframe/tsnet`) and prints the client's tailnet
IPs. It is a one-time step; `connect` reuses that state. `ghostframe
logout` leaves the tailnet and removes the directory.

`connect` takes `--port` (default 443) and `--chord-prefix`.

### Controls

Input goes to the remote session, so the client reserves a tmux-style
prefix chord for its own commands:

| Keys | Action |
| --- | --- |
| `Ctrl+Alt+B` then `d` | Disconnect and quit |
| `Ctrl+Alt+B` then `h` | Minimise the window |

Pass `--chord-prefix super-b` to use `Super+B` instead; those two are the
only accepted values, and an unrecognised one is rejected at startup rather
than silently defaulting. Every key including the prefix is still forwarded
to the remote — only the completing `d`/`h` is swallowed — so the remote
side may occasionally see a stray `b`.

### Tests

```bash
just test-client
```

Runs clippy (`--all-targets -D warnings`) and the unit tests for the
client crates only. The clippy pass is not decoration: `cargo test -p`
silently drops test binaries that fail to compile and still reports
`0 failed`, so clippy is the real gate.

## More

- Build, test, contribute: [DEVELOPERS.md](DEVELOPERS.md)
- Protocol and architecture: [docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md)
