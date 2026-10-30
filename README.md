# Ghostframe

A Linux-only remote desktop server: stream your X11 desktop session to a
browser, over QUIC on a Tailscale tailnet. Per-tile adaptive encoding (H.264 for motion, palette/wavelet/
solid-fill for static content) keeps bandwidth low and text crisp.

There are two halves, and you do not need both:

| | What it is | Build |
| --- | --- | --- |
| **Server** | `ghostframe-server` — captures your X11 desktop session, encodes, joins the tailnet. Serves the browser client itself. | [Install](#install) |
| **Native client** | `ghostframe` — a Linux desktop client. No browser, no server. | [Native client](#native-client) |

Any device on the tailnet can also just open the server's URL in Chrome —
that is the browser client, and it needs nothing installed. The native
client is the alternative to *that*, not to the server.

**If you only want the native client, skip straight to [Native
client](#native-client).** It builds a strict subset of the workspace:
no X session to capture, no GPU driver setup, no Node, no Docker.

For the full design see
[docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).
For building, testing, and developer tooling see
[DEVELOPERS.md](DEVELOPERS.md).

## Server prerequisites

For the native client only, see [Native client](#native-client) instead —
its dependency list is much shorter.

Ghostframe today does not ship pre-built binaries — you build from source
during install. The prerequisites cover both the runtime stack (Vulkan) and
the build-from-source toolchain.

The server captures the X11 session of the user who installs it, whatever
window manager it runs. It needs a Vulkan device for analysis and encoding
(AMD with `amdgpu`/RADV is what it is developed on); VA-API is used for H.264
when present. No root access is needed to install or run it. Wayland sessions
are not supported yet.

**Ubuntu 24.04:**

```bash
sudo apt-get install \
    build-essential rsync pkg-config clang libclang-dev golang-go \
    rustc cargo \
    nodejs npm \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
    libx264-dev libx11-dev libxext-dev libxdamage-dev libdrm-dev \
    libvulkan1 mesa-vulkan-drivers vulkan-tools
```

**Arch Linux:**

```bash
sudo pacman -S base-devel rsync clang go rust nodejs npm \
    ffmpeg x264 libx11 libxext libxdamage libdrm \
    vulkan-icd-loader vulkan-tools
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

A Tailscale account. `ghostframe-server login` prints a sign-in URL; the
tailnet user who approves it owns the node, and every device of that user may
connect (see [Who can connect](#who-can-connect)). To join without a browser,
set `TS_AUTHKEY` to a **one-off** pre-auth key for that one `login`: a reusable
key that is still valid lets anyone holding it register an admitted device.

## Install

As the desktop user whose session should be served — not root:

```bash
git clone https://github.com/mountain-reverie/ghostframe.git
cd ghostframe
just build-release
./target/release/ghostframe-server install    # binary, user unit, autostart
./target/release/ghostframe-server login      # prints a sign-in URL, waits for approval
systemctl --user start ghostframe-server
```

`install` copies the binary to `~/.local/bin/ghostframe-server`, writes
`~/.config/systemd/user/ghostframe-server.service` and
`~/.config/autostart/ghostframe-server.desktop` (some desktops, Enlightenment
among them, never activate `graphical-session.target`), and enables the unit.
From then on the server starts with your graphical session. It starts the
service itself when the node has already joined a tailnet (including a
registration left by the old `ghostframe-xdaemon`, which it adopts); otherwise
it tells you to run `login`. A bare `ghostframe-server` prints the help.

`install` must be run from your desktop session: it refuses root, needs the
user systemd manager to be reachable, and needs `HOME` to match your passwd
home. It also replaces the old `packaging/install.sh` install (attach or
headless): it stops and removes those user units, and lists any root-owned
leftovers with the command to remove them; it never touches `/etc`. When run
from inside the old headless session, stopping that session would kill it, so
it re-runs itself as the transient unit `ghostframe-install`; follow it with
`journalctl --user -u ghostframe-install -f`. `login` and `capture` refuse to
run while the old `ghostframe-xdaemon` unit is installed or running.

Joined means the file `.joined` exists in the state directory, written by
`login` or by a `capture` that joined with `TS_AUTHKEY`. The service never
prints a sign-in URL: started without that marker and without `TS_AUTHKEY`, it
exits with status 2 (which the unit does not restart on) and says to run
`login`.

### Security position

Any client admitted over the tailnet sees your desktop: your files, browser
sessions, saved passwords and sudo. Single-client eviction applies (one remote
client at a time). Access is limited to the tailnet user who owns the node plus
anyone in `GHOSTFRAME_ALLOWED_USERS` (see [Who can connect](#who-can-connect));
**everyone admitted gets full access to your session**, so keep the list short
and restrict the node in your tailnet ACLs as well.

### Max resolution

`install` caps what a remote client may set at **1920×1080** (on a first
install; a reinstall without `--max-resolution` keeps whatever the installed
unit says, `none` included, and prints it): the session is
the physical display, which a KVM or remote-management console may be
capturing as the machine's recovery view, and a mode it cannot show removes
that view exactly when it is needed. Change it with
`ghostframe-server install --force --max-resolution 2560x1440`, or
`--max-resolution none`. The value is `GHOSTFRAME_MAX_RESOLUTION` in the unit.
(The old `GHOSTFRAME_ATTACH_MAX_RESOLUTION` is no longer read.)

### Mode changes and Enlightenment

A client's resolution request is applied with RandR, reusing an existing
output mode, and the server reads the result back. If the desktop overrides
it, the server logs that and does not fight it. An unpatched Enlightenment
(its `e_randr2` restores its saved per-monitor mode) reverts an external mode
change about 1-2 s later, so on an unpatched Enlightenment a client's resolution
change does not stick; a patch is in progress upstream.
`GHOSTFRAME_RANDR_SETTLE_MS` (default 5000; `0` turns the read-back off) sets
how long the server waits before it checks.

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
and deletes the state; it deletes only the files tsnet is known to write and
refuses if it finds anything else in the directory. Pass the same `--hostname`
and `--control-url` that `login` was given, if any. `login`, `logout` and the
service share one state directory,
`${XDG_STATE_HOME:-~/.local/state}/ghostframe` (mode 0700; `TS_STATE_DIR`
overrides it and is held to the same check), and refuse to run at the same
time.

## Update

```bash
cd ghostframe
git pull
just build-release
./target/release/ghostframe-server install --force
```

`install --force` replaces the binary and restarts the service when the node
has joined. Then hard-reload open browser tabs (Ctrl+Shift+R).

## First connection

On any device on the same tailnet, open

```
https://<hostname>-ghostframe.<tailnet>.ts.net/
```

in Chrome / Chromium / Edge. `<hostname>` is your machine's hostname
(`login` and the service register it as `<hostname>-ghostframe`); `<tailnet>` is
your tailnet's MagicDNS suffix.

The daemon serves the web client and its WebTransport cert hash
directly, so no manual setup is required on the client device. The
page uses a Tailscale-issued Let's Encrypt certificate — make sure
**HTTPS Certificates** are enabled at
<https://login.tailscale.com/admin/dns> before connecting for the
first time.

## Who can connect

Being on the tailnet is not enough. By default the daemon admits only
devices owned by **the tailnet user who owns the ghostframe node**: the
user who logged it in, or the user who created the auth key it was
registered with. That is every device of that user — including any node
someone else registers with a key that user created, so do not leave a
reusable key lying around. Every other peer is refused, both by the QUIC listener (before any
session exists) and by the web server, which answers `403` and never hands
out the certificate hash. Refusals are logged at INFO, once a minute per
peer, with the peer's address, login name and the reason:

```bash
journalctl _UID=$(id -u <user>) --no-pager | grep 'refused tailnet peer'
```

The owner is matched by Tailscale's stable user ID, not by name.

The check is made when a connection opens, and **repeated for every
connected session** every 60 seconds. A peer that no longer passes (its
device was removed or expired, re-tagged, or moved to another user) has its
session closed with the reason `access revoked`; the same rule and the same
lookup are used as at connect time. Set the interval, in whole seconds from 1
to 86400, with `GHOSTFRAME_ACCESS_RECHECK_SECS`; `0` or any other value
(`5m`, say) refuses startup with an error naming the variable and the value,
so a typo never switches re-checking off nor quietly runs another interval. A session can therefore outlive a revocation by up
to one interval, plus up to one short retry (5 s) if the tailnet did not
answer. Fail closed: a peer the tailnet no longer knows is closed at once, and
a lookup that fails twice in a row also closes the session; one failure alone
does not.

What is *not* re-read while running: the node's own owner and
`GHOSTFRAME_ALLOWED_USERS` are read once at startup. After changing who owns
or tags the node, or the list, restart the daemon.

### Adding users

`GHOSTFRAME_ALLOWED_USERS` adds users and tags to the owner — it never
replaces the owner, and there is no value that admits everyone. It is a
comma-separated list of tailnet login names and ACL tags:

```
GHOSTFRAME_ALLOWED_USERS=alice@example.com,bob@github,tag:ops
```

Login names are matched exactly, as the refusal log prints them. A tag
admits devices carrying it; a tagged device is identified by its tags only,
never by the user behind it. Set it with a drop-in for the server's
**user** unit (a drop-in under
`/etc/systemd/system/` has no effect on a user unit), as the user it runs
as:

```bash
systemctl --user edit ghostframe-server
#   [Service]
#   Environment=GHOSTFRAME_ALLOWED_USERS=alice@example.com,tag:ops
systemctl --user restart ghostframe-server
```

The startup log names the owner and the list in effect. A malformed entry
is ignored with a warning.

### Tagged nodes have no owner

If the node was registered with a **tagged** auth key, Tailscale gives it
no owning user. The daemon then admits only what `GHOSTFRAME_ALLOWED_USERS`
lists — with an empty list, nobody — and says so with a warning at
startup. Either register the node with an untagged key, or list the users
and tags that may connect. For several ghostframe hosts this is the
tighter setup: register them with one tagged key and list yourself, so a
leaked key admits nothing.

### Browser pages: the Origin check

Who may connect is decided per machine, so by itself it cannot tell the
ghostframe web client from any other page open in an allowed user's
browser. The daemon therefore also checks the `Origin` of every WebTransport
session request and answers `403` unless it is `https://<this node's MagicDNS
name>` (port 443). `null`, other hosts, `http://` and other ports are refused.
A request with **no** `Origin` is accepted: browsers always send one and a
page cannot remove it, while the native client is not a browser and sends none.
That case is covered by the access policy above, not by this check.

To serve the client under another name, list the extra origins in
`GHOSTFRAME_WEB_ORIGINS` (comma-separated `scheme://host[:port]`, port `*`
for any). Malformed entries are ignored with a warning; nothing admits every
origin.

### Restrict the node in your tailnet policy too

The daemon's check is the access boundary, but nothing else on the tailnet
needs to reach the node at all. A policy grant limiting it to its users and
its three ports keeps everyone else from even probing it (`ghostframe` here
is a [host alias](https://tailscale.com/kb/1337/acl-syntax#hosts) for the
node's tailnet IP):

```json
{
  "hosts": { "ghostframe": "100.101.102.103" },
  "grants": [
    {
      "src": ["alice@example.com"],
      "dst": ["ghostframe"],
      "ip":  ["udp:443", "tcp:443", "tcp:80"]
    }
  ]
}
```

UDP 443 carries the session (QUIC); TCP 443 serves the browser client and
TCP 80 redirects to it and serves `/config.json` to the native client.

## Native client

`ghostframe` is a Linux desktop client: it joins the tailnet as its own
node, connects to a ghostframe server, and opens a single window (X11 or
Wayland) rendering the remote session. It decodes and composites on the
GPU via Vulkan, so it needs a working Vulkan driver but no Xorg
configuration and no window manager of its
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

#    Or non-interactively, with a pre-auth key read from stdin (never argv,
#    where other local users can see it in /proc) or from a 0600 file:
ghostframe login --authkey-stdin < key.txt
ghostframe login --authkey-file ~/.config/ghostframe/authkey

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
