build: build-web
    cargo build

build-release: build-web
    cargo build --release

# --bins is not redundant: it is the only thing that runs the tests in binary
# crates (ghostframe-xdaemon, codec_report). `--lib` skips them silently.
test-unit:
    cargo test --workspace --lib
    cargo test --workspace --bins

# --- Native client only -----------------------------------------------
#
# The `ghostframe` binary and the crates beneath it. None of this touches
# the server, the web client, Docker, or the e2e harness -- which matters
# on a machine that cannot afford to build the workspace. `ghostframe-tsnet`
# leaves its `web-embed` feature off here, so ghostbridge is built with
# `-tags noweb` and no npm/vite step is involved at all.
#
# `ghostframe-client-net` is in the build list but NOT the test/lint lists:
# its dev-dependencies include `ghostframe-lib`, so exercising its tests
# drags in the whole server (and, through it, the embedded SPA). Run
# `cargo test -p ghostframe-client-net` separately when you have the
# headroom for that.
client-build-crates := "-p ghostframe-cli -p ghostframe-client-native -p ghostframe-client-gpu -p ghostframe-client-h264 -p ghostframe-client-core -p ghostframe-client-net -p ghostframe-client-capi -p ghostframe-protocol -p ghostframe-tsnet"
client-test-crates := "-p ghostframe-cli -p ghostframe-client-native -p ghostframe-client-gpu -p ghostframe-client-h264 -p ghostframe-client-core -p ghostframe-client-capi -p ghostframe-protocol -p ghostframe-tsnet"

# Build the `ghostframe` native client (debug). No server, no web client.
build-client:
    cargo build {{client-build-crates}}

# Build the `ghostframe` native client (release).
build-client-release:
    cargo build --release {{client-build-crates}}

# `cargo test -p <crate>` silently drops test binaries that fail to compile
# and still reports 0 failed (see AGENTS.md), so the clippy --all-targets
# pass below is the real gate, not the test summary. Both run here.
#
# Clippy and unit-test the native client crates.
test-client:
    cargo clippy {{client-test-crates}} --all-targets -- -D warnings
    cargo test {{client-test-crates}}

# Clippy the native client crates.
lint-client:
    cargo clippy {{client-build-crates}} --all-targets -- -D warnings

# The GLES backend, for GPUs with no Vulkan driver (Mali Midgard and friends).
# Mutually exclusive with the default `vulkan` feature, so it needs its own
# pass -- a `--all-features` build would fail the compile_error! guard on
# purpose.
#
# CI can only *check* this (no runner has a Mali GPU), which is exactly why it
# must be checked: a feature nobody builds is already broken. On real hardware
# run `test-client-gles` instead.
check-gles:
    cargo clippy -p ghostframe-client-gpu --no-default-features --features gles,test-support --all-targets -- -D warnings

# The GLES backend's tests. Needs a GLES 3.1 device with
# EGL_MESA_image_dma_buf_export -- verified on Mali-T860/panfrost. Skips
# gpu_import (Vulkan-only; see that file's header).
test-client-gles:
    cargo test -p ghostframe-client-gpu --no-default-features --features gles,test-support

# Build, lint and test the native client -- the client-only `ci-local`.
ci-client: build-client
    @echo "=== fmt-check ==="
    just fmt-check
    @echo "=== clippy + unit tests (native client) ==="
    just test-client
    @echo "=== cbindgen header up-to-date ==="
    cargo check -p ghostframe-client-capi
    git diff --exit-code ghostframe-client-capi/include/ghostframe_client.h
    @echo "=== GLES backend still compiles ==="
    just check-gles
    @echo "=== ci-client passed ==="

# Run from a clean checkout: builds the web client SPA (vite) into
# ghostframe-web-client/dist/, which ghostbridge //go:embeds at compile
# time. A stale or missing dist/ now fails the ghostbridge build with a
# clear message rather than silently embedding nothing.
build-web:
    cd ghostframe-web-client && npm install && npm run build

test-e2e: build-web containers-build
    cargo test --test e2e

containers-build:
    cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern
    docker build -t ghostframe/test-server -f tests/containers/test-server/Dockerfile .
    docker build -t ghostframe/test-headscale -f tests/containers/headscale/Dockerfile tests/containers/headscale/

# Packaging tests. Pure bash, no toolchain, no root: sources
# packaging/install.sh and asserts its mode-switch cleanup against a throwaway
# unit dir. GHOSTFRAME_TEST_UNIT_DIR is mandatory and has no default on
# purpose -- the test deletes every file it names, and a default would put
# that rm one typo away from /etc/systemd.
test-packaging:
    GHOSTFRAME_TEST_UNIT_DIR="$(mktemp -d)" bash tests/packaging/mode_switch_test.sh

lint:
    cargo clippy --workspace --all-targets -- -D warnings

fmt-check:
    cargo fmt --all -- --check

fmt:
    cargo fmt --all

# Run the fast CI tier (everything in .github/workflows/ci.yml) locally,
# in the same order. Does NOT run e2e — use `just test-e2e` for that.
ci-local:
    @echo "=== fmt-check ==="
    just fmt-check
    @echo "=== clippy ==="
    cargo clippy --workspace --all-targets -- -D warnings
    @echo "=== unit tests ==="
    cargo test --workspace --lib
    cargo test --workspace --bins
    @echo "=== packaging tests ==="
    just test-packaging
    @echo "=== web client build ==="
    just build-web
    cd ghostframe-web-client && npx tsc --noEmit
    @echo "=== release build ==="
    cargo build --workspace --release --exclude ghostframe-e2e
    @echo "=== cbindgen headers up-to-date ==="
    cargo check -p ghostframe-lib
    git diff --exit-code ghostframe-lib/include/ghostframe.h
    cargo check -p ghostframe-client-capi
    git diff --exit-code ghostframe-client-capi/include/ghostframe_client.h
    @echo "=== go vet + build (both tags) ==="
    cd ghostbridge && go vet ./... && go build ./...
    # The native client's archive; untagged vet never sees web_dist_noweb.go.
    cd ghostbridge && go vet -tags noweb ./... && go build -tags noweb ./...
    @echo "=== ci-local passed ==="

firefox-bin := env_var_or_default('GHOSTFRAME_E2E_FIREFOX_BIN', '/usr/bin/firefox')

# Verify host has what the Firefox e2e path needs.
e2e-firefox-doctor:
    @command -v {{firefox-bin}} >/dev/null || { echo "missing: {{firefox-bin}}"; exit 1; }
    @command -v geckodriver >/dev/null || { echo "missing: geckodriver"; exit 1; }
    @command -v certutil >/dev/null || { echo "missing: certutil (install nss-tools / nss / libnss3-tools)"; exit 1; }
    @echo "firefox-e2e prereqs OK"

# Run only the Firefox slice of the e2e suite.
e2e-firefox: containers-build
    cargo test --test e2e -- _firefox
