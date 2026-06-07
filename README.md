# Ghostframe

A Linux-only remote desktop protocol built around per-tile adaptive encoding,
zero-copy GPU pipelines, QUIC transport over Tailscale, and a browser-based
client.

The core insight: typical desktop content is ~80% text, ~15% static images, and
~5% motion. Encoding everything with a single video codec wastes bandwidth on
static content and destroys text clarity. Ghostframe classifies each 32×32 tile
independently and selects the optimal codec per tile per frame (H.264 for
motion, PalRLE / CDF 5/3 wavelet / solid-fill for static content, skip for
unchanged).

For the full protocol design, see [docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).

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

## Prerequisites

Ghostframe targets Linux. Development has been done on recent Arch / Ubuntu
24.04. Other distributions should work as long as equivalent packages are
available.

### System packages

On Ubuntu 24.04 (matches what the test-server container installs):

```bash
sudo apt-get install \
    build-essential pkg-config clang libclang-dev \
    golang-go \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
    libx264-dev \
    libx11-dev libxext-dev libxdamage-dev \
    libdrm-dev \
    libvulkan1 mesa-vulkan-drivers vulkan-tools
```

On Arch (rough equivalents):

```bash
sudo pacman -S base-devel clang go ffmpeg x264 libx11 libxext libxdamage libdrm \
    vulkan-icd-loader vulkan-tools
```

### Toolchains

- **Rust** stable, edition 2021. Install via [rustup](https://rustup.rs/).
- **`cbindgen`** for the FFI header generation:
  `cargo install cbindgen`
- **Node.js** 20+ and **npm** for the web client.
- **Docker** for the end-to-end test containers (test-server + headscale).
- **`just`** (optional but recommended) — every common task is in the `Justfile`.

A Tailscale account with a reusable pre-auth key is only required if you want
to run a manual smoke test against a real tailnet. The e2e harness uses an
embedded headscale and needs no Tailscale account.

## Building

The whole workspace:

```bash
cargo build           # debug
cargo build --release # release
# or:
just build
just build-release
```

The web client (output goes to `ghostframe-web-client/dist/`):

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

## Running

### Unit tests

```bash
cargo test --lib
# or:
just test-unit
```

### End-to-end tests

The e2e harness brings up a containerised server with a virtual X server and
a local headscale, then drives Chromium against it.

```bash
just test-e2e
```

That target builds the web client, builds the release binaries used inside the
containers, builds the two container images (`ghostframe/test-server`,
`ghostframe/test-headscale`), then runs `cargo test --test e2e`.

After any change to `ghostframe-lib` or `ghostframe-xdaemon`, rebuild the
container before re-running e2e tests, otherwise tests will silently run the
stale binary baked into the image:

```bash
just containers-build
```

E2e tests use `--test-threads=1` and depend on the Docker daemon being
reachable by the current user.

### Manual smoke test against a real tailnet

See [docs/m0-manual-smoke-test.md](docs/m0-manual-smoke-test.md) for the
ping/pong smoke test that exercises `ghostframe-xdaemon` + the web client
against a real Tailscale tailnet.

### Lint and format

```bash
just lint        # cargo clippy -- -D warnings
just fmt-check   # cargo fmt -- --check
just fmt         # cargo fmt
```

## Contributing

Ghostframe is a small project with strong opinions about scope and design.
Before writing code, **please open an issue describing the problem or the
feature you want to work on.** A short discussion up front prevents the much
more painful situation where a PR that took real effort has to be turned down
because the change conflicts with the design or with work already in flight.

Once we have agreed on the approach in the issue, open a GitHub pull request
that references it. Keep the PR focused on what was agreed; if the scope grows
during implementation, comment on the issue rather than expanding the PR
unilaterally.

The only exception: **trivial bug fixes of fewer than 10 lines of code** may
be submitted directly as a PR without a prior issue. "Trivial" here means a
clearly correct one-spot fix — typos, off-by-ones, obvious null/`unwrap`
guards. Anything that changes behaviour, adds a dependency, touches the wire
format, or requires judgement about the right fix should still go through an
issue first.

Every PR is expected to:

- pass `just lint` and `just fmt-check`,
- pass `just test-unit`, and
- pass `just test-e2e` if it touches anything that the e2e harness covers
  (capture, encoders, protocol, web client).
