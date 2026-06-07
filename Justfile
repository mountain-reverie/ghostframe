build:
    cargo build

build-release:
    cargo build --release

test-unit:
    cargo test --lib

# The e2e harness serves ghostframe-web-client/dist/ over HTTP to Chromium.
# A stale dist/ silently breaks tests (old ACK wire format → 0 PixelPerfect
# transitions in lossless-buildup, etc.) so always rebuild before running.
web-client-build:
    cd ghostframe-web-client && npm install && npm run build

test-e2e: web-client-build containers-build
    cargo test --test e2e

containers-build:
    cargo build --release -p ghostframe-xdaemon -p ghostframe-test-pattern
    docker build -t ghostframe/test-server -f tests/containers/test-server/Dockerfile .
    docker build -t ghostframe/test-headscale -f tests/containers/headscale/Dockerfile tests/containers/headscale/

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
    @echo "=== release build ==="
    cargo build --workspace --release --exclude ghostframe-e2e
    @echo "=== web client build ==="
    just web-client-build
    cd ghostframe-web-client && npx tsc --noEmit
    @echo "=== cbindgen header up-to-date ==="
    cargo check -p ghostframe-lib
    git diff --exit-code ghostframe-lib/include/ghostframe.h
    @echo "=== go vet + build ==="
    cd ghostbridge && go vet ./... && go build ./...
    @echo "=== ci-local passed ==="
