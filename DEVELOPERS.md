# Ghostframe — Developer Guide

This document covers building from source, running tests, day-to-day
development tooling, and notes on alternative configurations not covered by
the README's install path. For installing and using ghostframe, see
[README.md](README.md).

## Repository layout

| Crate | Purpose |
| --- | --- |
| `ghostframe-lib` | Core library: tile engine, encoders, QUIC/WebTransport stack, Tailscale integration. Builds as `cdylib` / `staticlib` / `rlib`. |
| `ghostframe-xdaemon` | Standalone X11/DRM capture daemon that hosts a session over an embedded Tailscale node. |
| `ghostframe-web-client` | TypeScript + Vite browser client (WebTransport + WebCodecs + WebGPU). |
| `ghostframe-test-pattern` | Deterministic test-pattern generator used by the e2e harness. |
| `ghostframe-e2e` | End-to-end test harness driving Chromium against a containerised server over a local headscale tailnet. |
| `ghostframe-bench` | Codec micro-benchmarks (see `docs/specs/m3-codec-bench-results.md`). |
| `ghostbridge` | Go ↔ Rust FFI shim for libtailscale. |

For the full protocol design, see
[docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).

## Developer prerequisites

These are on top of the runtime + build-from-source packages already listed
in the README's "Prerequisites" section:

- **`cbindgen`** for FFI header generation: `cargo install cbindgen`
- **Docker** for the end-to-end test containers (test-server + headscale).
- **`just`** — every common task is in the `Justfile`.

## Building

Workspace:

```bash
cargo build           # debug
cargo build --release # release
# or:
just build
just build-release
```

Web client (output goes to `ghostframe-web-client/dist/`):

```bash
cd ghostframe-web-client
npm install
npm run build
# or, from repo root:
just web-client-build
```

> The e2e harness serves `ghostframe-web-client/dist/` over HTTP to headless
> Chromium. A stale `dist/` silently breaks tests. Always rebuild the web
> client after pulling changes that touch it. `just test-e2e` does this for
> you.

## Running tests

Unit tests:

```bash
cargo test --lib
# or:
just test-unit
```

End-to-end tests bring up a containerised server with a virtual X server and
a local headscale, then drive Chromium against it:

```bash
just test-e2e
```

That target builds the web client, builds the release binaries used inside
the containers, builds the two container images
(`ghostframe/test-server`, `ghostframe/test-headscale`), then runs
`cargo test --test e2e`.

After any change to `ghostframe-lib` or `ghostframe-xdaemon`, rebuild the
container before re-running e2e tests, otherwise tests will silently run
the stale binary baked into the image:

```bash
just containers-build
```

E2e tests use `--test-threads=1` and depend on the Docker daemon being
reachable by the current user.

## Manual smoke test against a real tailnet

See [docs/m0-manual-smoke-test.md](docs/m0-manual-smoke-test.md) for the
ping/pong smoke test that exercises `ghostframe-xdaemon` + the web client
against a real Tailscale tailnet.

## Lint and format

```bash
just lint        # cargo clippy -- -D warnings
just fmt-check   # cargo fmt -- --check
just fmt         # cargo fmt
just ci-local    # run the fast CI tier in the order CI runs it
```

## Alternative configurations

The README's install path targets AMD GPUs via the `amdgpu` driver's
`Virtual` connector. Other configurations are not part of the supported
install path but are documented here for users who want to adapt the setup.

### Intel iGPU

The Intel `modesetting` driver exposes a `Virtual_1` connector under
similar constraints. Swap the `Device` and `Monitor` sections in
`packaging/xorg-headless-amdgpu.conf`:

```
Section "Device"
    Identifier "Card0"
    Driver     "modesetting"
    Option     "Monitor-Virtual-1" "VirtualMonitor"
EndSection
```

Save as `xorg-headless-intel.conf` and point the install script at it (or
manually copy it to `/etc/X11/ghostframe-headless.conf`).

### NVIDIA (proprietary driver)

The NVIDIA driver supports headless operation via `AllowEmptyInitialConfiguration`
and `AllowHeadlessMode`. A starting point:

```
Section "Device"
    Identifier "Card0"
    Driver     "nvidia"
    Option     "AllowEmptyInitialConfiguration" "true"
    Option     "AllowHeadlessMode" "true"
EndSection
```

This has not been tested against ghostframe; PRs welcome.

### Xorg + dummy driver (no GPU)

For machines without a supported GPU, `xf86-video-dummy` provides a CPU-only
Xorg with no DRM device. `ghostframe-xdaemon` will detect the absence of DRM
and fall back to its X11 capture path. Hardware encoding is not available
in this configuration.

```
Section "Device"
    Identifier "Card0"
    Driver     "dummy"
    VideoRam   16384
EndSection
```

### VKMS (virtual KMS kernel module)

Not useful today. VKMS is a software-only virtual KMS device; capturing from
it and encoding on a discrete GPU is the cross-device PRIME case which is
known broken (yields stale/scrambled bytes). The workaround in ghostframe is
a CPU-mmap fallback, which gives no advantage over Xorg dummy. Revisit when
the cross-device PRIME issue is resolved upstream.

## WM alternatives

The default `ghostframe-wm.service` runs `enlightenment_start`. To use a
different window manager, edit the `ExecStart=` line of
`~/.config/systemd/user/ghostframe-wm.service`. Common alternatives:

| WM | `ExecStart=` |
| --- | --- |
| enlightenment | `/usr/bin/enlightenment_start` |
| i3 | `/usr/bin/i3` |
| openbox | `/usr/bin/openbox` |
| sway (X11 via Xwayland) | not supported — sway is Wayland-only; use a different WM here |
| fluxbox | `/usr/bin/fluxbox` |
| xfwm4 | `/usr/bin/xfwm4` |

After editing, reload and restart:

```bash
systemctl --user daemon-reload
systemctl --user restart ghostframe-wm.service
```

The `Restart=on-failure` directive will not bring up the new WM if the
binary doesn't exist on the host. Verify the path with `command -v`.

## Contributing

Ghostframe is a small project with strong opinions about scope and design.
Before writing code, **please open an issue describing the problem or the
feature you want to work on.** A short discussion up front prevents the
much more painful situation where a PR that took real effort has to be
turned down because the change conflicts with the design or with work
already in flight.

Once we have agreed on the approach in the issue, open a GitHub pull request
that references it. Keep the PR focused on what was agreed; if the scope
grows during implementation, comment on the issue rather than expanding the
PR unilaterally.

The only exception: **trivial bug fixes of fewer than 10 lines of code** may
be submitted directly as a PR without a prior issue. "Trivial" here means a
clearly correct one-spot fix — typos, off-by-ones, obvious null/`unwrap`
guards. Anything that changes behaviour, adds a dependency, touches the
wire format, or requires judgement about the right fix should still go
through an issue first.

Every PR is expected to:

- pass `just lint`, `just fmt-check`, and the checks described in [docs/ci.md](docs/ci.md),
- pass `just test-unit`, and
- pass `just test-e2e` if it touches anything that the e2e harness covers
  (capture, encoders, protocol, web client).
