build:
    cargo build

build-release:
    cargo build --release

test-unit:
    cargo test --lib

test-e2e:
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
