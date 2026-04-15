build:
    cargo build

build-release:
    cargo build --release

test-unit:
    cargo test --lib

test-e2e:
    cargo test --test e2e

containers-build:
    cargo build --release -p ghostframe-xdaemon
    docker build -t ghostframe/test-server -f tests/containers/test-server/Dockerfile .

lint:
    cargo clippy -- -D warnings

fmt-check:
    cargo fmt -- --check

fmt:
    cargo fmt
