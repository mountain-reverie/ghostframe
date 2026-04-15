build:
    cargo build

build-release:
    cargo build --release

test-unit:
    cargo test --lib

test-e2e:
    cargo test --test e2e

containers-build:
    docker build -t ghostframe/test-server tests/containers/test-server/

lint:
    cargo clippy -- -D warnings

fmt-check:
    cargo fmt -- --check

fmt:
    cargo fmt
