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
    cargo clippy -- -D warnings

fmt-check:
    cargo fmt -- --check

fmt:
    cargo fmt
