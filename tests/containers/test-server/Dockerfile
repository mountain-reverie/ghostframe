# Stage 1: Build ghostframe binaries inside the container so shared libraries
# match the runtime environment exactly.
FROM ubuntu:24.04 AS builder

RUN apt-get update && apt-get install -y --no-install-recommends \
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
    && rm -rf /var/lib/apt/lists/*

# Install Rust
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"
RUN cargo install cbindgen

WORKDIR /build
# Copy workspace files for dependency caching
COPY Cargo.toml Cargo.lock ./
COPY ghostframe-lib/Cargo.toml ghostframe-lib/Cargo.toml
COPY ghostframe-xdaemon/Cargo.toml ghostframe-xdaemon/Cargo.toml
COPY ghostframe-test-pattern/Cargo.toml ghostframe-test-pattern/Cargo.toml
# Workspace members not built here but required for cargo to resolve the
# workspace manifest. Empty src/lib.rs stubs are enough.
COPY ghostframe-e2e/Cargo.toml ghostframe-e2e/Cargo.toml
COPY ghostframe-e2e/src/lib.rs ghostframe-e2e/src/lib.rs
COPY ghostframe-bench/Cargo.toml ghostframe-bench/Cargo.toml
# Copy the full src/ tree, not just lib.rs: since the d404626 refactor the
# library declares `pub mod fixtures;` which expects fixtures/ on disk.
COPY ghostframe-bench/src/ ghostframe-bench/src/
# Cargo.toml declares [[bench]] targets; the .rs files must exist for the
# manifest to parse, even though they aren't compiled by `-p ghostframe-xdaemon`.
COPY ghostframe-bench/benches/ ghostframe-bench/benches/
COPY ghostbridge/ ghostbridge/

# Copy full source
COPY ghostframe-lib/ ghostframe-lib/
COPY ghostframe-xdaemon/ ghostframe-xdaemon/
COPY ghostframe-test-pattern/ ghostframe-test-pattern/

ENV CARGO_BUILD_JOBS=2
# ffmpeg-next overflows rustc's default 8 MiB stack on this codebase; bump
# to 16 MiB (the value rustc itself suggests when it crashes).
ENV RUST_MIN_STACK=16777216
# Enable the lib's `test-loss-injection` feature so e2e tests can drive
# deterministic packet/ACK drops via GHOSTFRAME_{OUTBOUND,INBOUND}_LOSS_*
# env vars. Also enable `cdf53-diag` so e2e tests can drive the GPU vs CPU
# cdf53 forward diff log (GHOSTFRAME_CDF53_DIFF_TILE=X,Y) and the
# GHOSTFRAME_CDF53_SKIP_L2_L3 / SKIP_L3 stage gates. Production builds
# (separate Dockerfile or release pipeline) do NOT enable these features.
RUN cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern \
    --features ghostframe-lib/test-loss-injection,ghostframe-lib/cdf53-diag

# Stage 2: Runtime image
FROM ubuntu:24.04

RUN apt-get update && apt-get install -y --no-install-recommends \
    ca-certificates \
    curl \
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

COPY --from=builder /build/target/release/ghostframe-xdaemon /usr/local/bin/ghostframe-xdaemon
COPY --from=builder /build/target/release/ghostframe-test-pattern /usr/local/bin/ghostframe-test-pattern
COPY tests/containers/test-server/entrypoint.sh /entrypoint.sh
COPY tests/containers/test-server/xorg.conf /etc/X11/xorg.conf
COPY tests/containers/test-server/xorg-odd.conf /etc/X11/xorg-odd.conf
COPY tests/containers/test-server/xorg-multi.conf /etc/X11/xorg-multi.conf
COPY tests/containers/test-server/xorg-vkms.conf /etc/X11/xorg-vkms.conf
RUN chmod +x /entrypoint.sh

ENTRYPOINT ["/entrypoint.sh"]
