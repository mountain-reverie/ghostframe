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
# 1. Clone and build from source.
git clone https://github.com/mountain-reverie/ghostframe.git
cd ghostframe
cargo build --release -p ghostframe-xdaemon
(cd ghostframe-web-client && npm install && npm run build)

# 2. Run the installer as root, targeting the user that should own the
#    headless session.
sudo ./packaging/install.sh <username>

# 3. Paste your Tailscale auth key when the script prompts, then reboot.
sudo reboot
```

After reboot, the configured user is automatically logged in on `tty1`,
Xorg comes up on display `:1`, Enlightenment starts inside that session,
and `ghostframe-xdaemon` joins the tailnet and starts capturing.

## First connection

On any device on the same tailnet (phone, laptop, another machine):

1. Find this host's tailnet name and the daemon's TLS cert hash. SSH into
   the host as the configured user and run:

   ```bash
   journalctl --user -u ghostframe-xdaemon -b | grep CERT_HASH_SHA256
   ```

   That prints a line like `CERT_HASH_SHA256=<64-hex>`.

2. Serve the web client on the host (or any other tailnet device):

   ```bash
   cd ghostframe/ghostframe-web-client/dist
   python3 -m http.server 8000 --bind 127.0.0.1
   ```

3. Open in Chrome / Chromium / Edge:

   ```
   http://127.0.0.1:8000/?host=<hostname>-ghostframe.<tailnet>.ts.net:4443&certHash=<64-hex>
   ```

You should see the Enlightenment desktop streaming into your browser.

## More

- Build, test, contribute: [DEVELOPERS.md](DEVELOPERS.md)
- Protocol and architecture: [docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md)
