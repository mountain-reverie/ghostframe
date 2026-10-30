# syntax=docker/dockerfile:1
# Stage 1a: toolchain base shared by the planner and builder stages.
FROM ubuntu:25.10 AS base

# rsync is required by ghostbridge/Makefile's sync-dist target. It sits in
# this first apt layer, not in a RUN after the source COPYs: down there it was
# re-run (apt-get update and a mirror round trip) on every source change.
RUN apt-get update -o Acquire::AllowInsecureRepositories=true -o Acquire::Retries=3 && apt-get install -o Acquire::Retries=3 -y --allow-unauthenticated --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    pkg-config \
    golang-go \
    libavcodec-dev \
    libavformat-dev \
    libavutil-dev \
    libswscale-dev \
    libavdevice-dev \
    libclang-dev \
    clang \
    libx264-dev \
    libx11-dev \
    libxext-dev \
    libxdamage-dev \
    libdrm-dev \
    rsync \
    && rm -rf /var/lib/apt/lists/*

# Install Rust
# Download to a file, then run it: `curl | sh` reports sh's status, so a failed
# download (DNS down) yielded an empty script, a "successful" layer without
# cargo, and that layer was then served from the BuildKit cache.
#
# `--default-toolchain none`: the only toolchain this image needs is the one
# rust-toolchain.toml pins, installed in its own layer just below. Installing
# rustup's default (current stable, ~485 MB with docs) and then letting the
# pinned one auto-install inside the build RUN meant a second toolchain
# download on every build and a layer nothing used.
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --default-toolchain none --profile minimal \
    && rm /tmp/rustup-init.sh
ENV PATH="/root/.cargo/bin:${PATH}"

WORKDIR /build
# rust-toolchain.toml pins the compiler; copy it before any cargo invocation
# so the image builds with the same rustc as CI and local dev, and install it
# here so the download is a cached layer rather than part of the build RUN.
COPY rust-toolchain.toml ./
RUN rustup toolchain install

# cargo-chef splits "compile the dependencies" from "compile the workspace".
# Pinned exactly (and installed with --locked) so the layer is only rebuilt when
# this line changes. It is installed here, in the shared base, so the planner
# and builder stages below both start from the same cached layer.
RUN cargo install cargo-chef --version 0.1.78 --locked

# Stage 1b: the recipe. Copies the whole workspace (the context excludes
# target/, docs/, *.md) and reduces it to recipe.json: the manifests, the
# lockfile and the target layout, with no source code. This stage reruns on
# every source change, but what the builder takes from it is only recipe.json,
# whose content (and so whose cache key) changes only when a manifest,
# Cargo.lock or a target list does.
FROM base AS planner
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# Stage 1c: build ghostframe binaries inside the container so shared libraries
# match the runtime environment exactly.
FROM base AS builder
# Parallelism for the in-container release build. Default 6, which suits a
# 12-core workstation that other jobs share; CI runners (small, and sharing a
# host) can pass a smaller number. Override with:
#
#   docker build --build-arg CARGO_JOBS=2 -t ghostframe/test-server \
#     -f tests/containers/test-server/Dockerfile .
#
# Each rustc job carries the 16 MiB stack set below and ffmpeg-next's
# codegen is memory-hungry, so scale this against available RAM, not just
# core count.
ARG CARGO_JOBS=6
ENV CARGO_BUILD_JOBS=${CARGO_JOBS}
# ffmpeg-next overflows rustc's default 8 MiB stack on this codebase; bump
# to 16 MiB (the value rustc itself suggests when it crashes).
ENV RUST_MIN_STACK=16777216
# Enable the lib's `test-loss-injection` feature so e2e tests can drive
# deterministic packet/ACK drops via GHOSTFRAME_{OUTBOUND,INBOUND}_LOSS_*
# env vars. Also enable `cdf53-diag` so e2e tests can drive the GPU vs CPU
# cdf53 forward diff log (GHOSTFRAME_CDF53_DIFF_TILE=X,Y) and the
# GHOSTFRAME_CDF53_SKIP_L2_L3 / SKIP_L3 stage gates. Production builds
# (separate Dockerfile or release pipeline) do NOT enable these features.
# `allow-software-vulkan` is the same kind of flag: this image has no /dev/dri,
# so its daemon runs on llvmpipe, which production refuses. The grep below
# proves the feature really reached this binary; ci/check-test-features.sh and
# ci-local prove it reached no other.
#
# Dependencies first. `cargo chef cook` builds every third-party crate of this
# exact package and feature selection (so feature unification, and with it the
# set of dependency artifacts, matches the final build below) against stub
# workspace crates. The result is an ordinary image layer, which is the point:
# BuildKit never exports cache *mounts* to a remote cache, so the old
# mount-based target/ was invisible to CI's type=gha cache and every source
# change recompiled the whole dependency graph there (about 320 s of a 600 s
# build). A layer is exported, and is rebuilt only when recipe.json, the
# toolchain or the settings above change.
#
# Neither this step nor the final one mounts anything over /build/target, which
# would shadow the layer's artifacts. The registry and git checkouts therefore
# stay in the layer as well: cargo needs their sources to resolve the graph.
# `cargo fetch` completes the registry for every platform: ghostframe-lib's
# build.rs runs cbindgen, which runs `cargo metadata`, which downloads (and
# extracts) all of it, windows-only crates included, in the final step.
COPY --from=planner /build/recipe.json recipe.json
RUN cargo chef cook --release --recipe-path recipe.json \
        -p ghostframe-server -p ghostframe-test-pattern \
        --features ghostframe-lib/test-loss-injection,ghostframe-lib/cdf53-diag,ghostframe-lib/allow-software-vulkan \
    && cargo fetch --locked

# Warm Go's module and build caches for ghostbridge in a layer of their own.
# ghostframe-tsnet's build.rs (a stub during cook) runs `go build` on it in the
# step below; without this, that step downloads the module graph (~500 MB,
# tailscale and friends) and recompiles it on every source change, and the
# result lands in the layer that changes every time. `-tags noweb` skips the
# //go:embed (so no dist/ is needed) and shares every dependency package with
# the real archive build. This layer is rebuilt only when ghostbridge/ changes.
COPY ghostbridge/ ghostbridge/
RUN cd ghostbridge && go build -tags noweb -buildmode=c-archive -o /tmp/warm.a . \
    && rm -f /tmp/warm.a /tmp/warm.h

# Copy workspace files for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY ghostframe-lib/Cargo.toml ghostframe-lib/Cargo.toml
COPY ghostframe-server/Cargo.toml ghostframe-server/Cargo.toml
COPY ghostframe-test-pattern/Cargo.toml ghostframe-test-pattern/Cargo.toml
# ghostframe-lib depends on ghostframe-protocol, so its full source is needed.
COPY ghostframe-protocol/ ghostframe-protocol/
# ghostframe-tsnet likewise: ghostframe-lib links the ghostbridge archive
# through it, and its build.rs is what runs `make archive`.
COPY ghostframe-tsnet/ ghostframe-tsnet/
# Workspace member not built here; Cargo.toml + src/ satisfy manifest resolution.
COPY ghostframe-client-core/Cargo.toml ghostframe-client-core/Cargo.toml
COPY ghostframe-client-core/src/ ghostframe-client-core/src/
# Same again for ghostframe-client-net: not built here, but a workspace member,
# so cargo refuses to resolve the manifest without it.
COPY ghostframe-client-net/Cargo.toml ghostframe-client-net/Cargo.toml
COPY ghostframe-client-net/src/ ghostframe-client-net/src/
# Same again for the native-client crates: not built here, but workspace
# members, so cargo refuses to resolve the manifest without them.
COPY ghostframe-client-gpu/Cargo.toml ghostframe-client-gpu/Cargo.toml
COPY ghostframe-client-gpu/src/ ghostframe-client-gpu/src/
COPY ghostframe-client-native/Cargo.toml ghostframe-client-native/Cargo.toml
COPY ghostframe-client-native/src/ ghostframe-client-native/src/
COPY ghostframe-client-capi/Cargo.toml ghostframe-client-capi/Cargo.toml
COPY ghostframe-client-capi/src/ ghostframe-client-capi/src/
COPY ghostframe-client-capi/build.rs ghostframe-client-capi/build.rs
COPY ghostframe-client-capi/cbindgen.toml ghostframe-client-capi/cbindgen.toml
COPY ghostframe-cli/Cargo.toml ghostframe-cli/Cargo.toml
COPY ghostframe-cli/src/ ghostframe-cli/src/
COPY ghostframe-client-h264/Cargo.toml ghostframe-client-h264/Cargo.toml
COPY ghostframe-client-h264/src/ ghostframe-client-h264/src/
COPY ghostframe-client-wasm/Cargo.toml ghostframe-client-wasm/Cargo.toml
COPY ghostframe-client-wasm/src/ ghostframe-client-wasm/src/
COPY ghostframe-client-wasm/build.rs ghostframe-client-wasm/build.rs
# Workspace members not built here but required for cargo to resolve the
# workspace manifest.
COPY ghostframe-e2e/Cargo.toml ghostframe-e2e/Cargo.toml
# Copy the full src/ tree, not just lib.rs: since the M3.5a refactor the
# library declares `pub mod harness;` which expects harness/ on disk.
COPY ghostframe-e2e/src/ ghostframe-e2e/src/
COPY ghostframe-bench/Cargo.toml ghostframe-bench/Cargo.toml
# Copy the full src/ tree, not just lib.rs: since the d404626 refactor the
# library declares `pub mod fixtures;` which expects fixtures/ on disk.
COPY ghostframe-bench/src/ ghostframe-bench/src/
# Cargo.toml declares [[bench]] targets; the .rs files must exist for the
# manifest to parse, even though they aren't compiled by `-p ghostframe-server`.
COPY ghostframe-bench/benches/ ghostframe-bench/benches/

# Copy full source
COPY ghostframe-lib/ ghostframe-lib/
COPY ghostframe-server/ ghostframe-server/
COPY ghostframe-test-pattern/ ghostframe-test-pattern/

# ghostbridge //go:embeds ghostframe-web-client/dist/ at compile time.
# The Makefile rsyncs from here into ghostbridge/dist/ before `go build`
# runs (via ghostframe-lib's build.rs → make archive). The dist tree
# must be prebuilt on the host (CI's web-client job or `npm run build`);
# this Dockerfile does not install Node.
COPY ghostframe-web-client/dist/ ghostframe-web-client/dist/

# The workspace crates, on top of the cooked dependencies.
#
# The touch is still needed, for a different reason than it used to be: COPY
# preserves the context files' mtimes, which can be older than the stub crates
# that `cook` just compiled, and cargo's freshness check is mtime-based, so
# without it an old checkout would link the stubs. Touching every workspace
# file (not target/, not the registry) makes the real sources newer, so exactly
# the workspace crates rebuild and the registry dependencies do not.
#
# Same package and feature selection as the cook above, and the grep proves the
# `allow-software-vulkan` feature really reached this binary;
# ci/check-test-features.sh and ci-local prove it reached no other. The
# binaries stay in /build/target for the runtime stage to COPY.
RUN find /build -path /build/target -prune -o -type f -print0 | xargs -0 touch \
    && cargo build --release -p ghostframe-server -p ghostframe-test-pattern \
        --features ghostframe-lib/test-loss-injection,ghostframe-lib/cdf53-diag,ghostframe-lib/allow-software-vulkan \
    && grep -aq 'software Vulkan permitted (test build)' target/release/ghostframe-server

# Stage 2: Runtime image
FROM ubuntu:25.10

RUN apt-get update -o Acquire::AllowInsecureRepositories=true && apt-get install -y --allow-unauthenticated --no-install-recommends \
    ca-certificates \
    curl \
    iproute2 \
    xserver-xorg-core \
    xserver-xorg-video-dummy \
    xserver-xorg-video-modesetting \
    x11-xserver-utils \
    x11-utils \
    mesa-utils \
    libgl1-mesa-dri \
    libvulkan1 \
    mesa-vulkan-drivers \
    vulkan-tools \
    libavcodec60 \
    libavdevice60 \
    libavutil58 \
    libswscale7 \
    libswresample4 \
    libx264-164 \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/ghostframe-server /usr/local/bin/ghostframe-server
COPY --from=builder /build/target/release/ghostframe-test-pattern /usr/local/bin/ghostframe-test-pattern
COPY tests/containers/test-server/entrypoint.sh /entrypoint.sh
COPY tests/containers/test-server/xorg.conf /etc/X11/xorg.conf
COPY tests/containers/test-server/xorg-odd.conf /etc/X11/xorg-odd.conf
COPY tests/containers/test-server/xorg-multi.conf /etc/X11/xorg-multi.conf
COPY tests/containers/test-server/xorg-vkms.conf /etc/X11/xorg-vkms.conf
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
