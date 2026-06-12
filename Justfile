build: build-web
    cargo build

build-release: build-web
    cargo build --release

test-unit:
    cargo test --lib

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
    @echo "=== web client build ==="
    just build-web
    cd ghostframe-web-client && npx tsc --noEmit
    @echo "=== release build ==="
    cargo build --workspace --release --exclude ghostframe-e2e
    @echo "=== cbindgen header up-to-date ==="
    cargo check -p ghostframe-lib
    git diff --exit-code ghostframe-lib/include/ghostframe.h
    @echo "=== go vet + build ==="
    cd ghostbridge && go vet ./... && go build ./...
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
