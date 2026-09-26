# Ghostframe

A Linux-only remote desktop server: stream a headless Xorg session running
your favourite window manager to a browser, over QUIC on a Tailscale
tailnet. Per-tile adaptive encoding (H.264 for motion, palette/wavelet/
solid-fill for static content) keeps bandwidth low and text crisp.

For the full design see
[docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).
For building, testing, and developer tooling see
[DEVELOPERS.md](DEVELOPERS.md).

## Prerequisites

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
    build-essential pkg-config clang libclang-dev golang-go \
    rustc cargo \
    nodejs npm \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
    libx264-dev libx11-dev libxext-dev libxdamage-dev libdrm-dev \
    libvulkan1 mesa-vulkan-drivers vulkan-tools \
    xserver-xorg xserver-xorg-video-amdgpu enlightenment
```

If your distribution's `rustc` is older than 1.74, install Rust via
[rustup](https://rustup.rs/) instead.

**Arch Linux:**

```bash
sudo pacman -S base-devel clang go rust nodejs npm \
    ffmpeg x264 libx11 libxext libxdamage libdrm \
    vulkan-icd-loader vulkan-tools \
    xorg-server xf86-video-amdgpu enlightenment
```

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
This makes sense when the remote session *is* the primary use of the machine,
and any local GUI access is exceptional rather than concurrent.

### What It Does Not Require

Unlike headless mode, attach mode does not need:

- The `amdgpu virtual_display=` kernel module option or a reboot to install it
- VKMS (Virtual Kernel Mode Setting)
- A dedicated non-interactive user (it captures the existing operator's session)
- A getty autologin on `tty1`
- Loosening of `Xwrapper.config` to allow X servers to run as the current user

The operator's existing X session — whichever window manager they use, whichever
display it runs on — becomes the capture source.

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
session remotely has no competing need for a local logout. But be aware: if
physical access is lost or the machine is compromised, you cannot regain a
login prompt without remote access.

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

## More

- Build, test, contribute: [DEVELOPERS.md](DEVELOPERS.md)
- Protocol and architecture: [docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md)
