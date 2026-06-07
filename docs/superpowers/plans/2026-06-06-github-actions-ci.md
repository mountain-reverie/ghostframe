# GitHub Actions CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Land three GitHub Actions workflows that run lint, fmt, unit tests, release build, web-client build, FFI header check, Go vet, three Rust integration test crates, and the e2e suite (minus VKMS-gated tests) on every PR and push to `master`, plus a nightly retry-enabled sweep that opens an issue on failure.

**Architecture:** Three separate workflow files (`ci.yml`, `e2e.yml`, `nightly.yml`) under `.github/workflows/`. Fast and e2e tiers run in parallel on every PR. Nightly tier runs on a schedule with `cargo-nextest --retries 2`. The VKMS-gated test skip list lives in `ci/skip-list.txt` and is read by both `e2e.yml` (via `--skip` flags) and `nightly.yml` (via a tiny shell helper that emits a nextest filter expression).

**Tech Stack:** GitHub Actions, `Swatinem/rust-cache@v2`, `actions/setup-node@v4`, `actions/setup-go@v5`, `docker/setup-buildx-action@v3`, `docker/build-push-action@v5`, `cargo-nextest`, Bash 5, YAML.

**Reference spec:** [`docs/superpowers/specs/2026-06-06-github-actions-ci-design.md`](../specs/2026-06-06-github-actions-ci-design.md)

---

## Conventions used in this plan

- All paths are **absolute from the repo root** unless explicitly relative.
- Every commit message follows the existing style (`<type>(<scope>): <summary>`) and ends with the `Co-Authored-By` trailer.
- Workflow YAML uses 2-space indent (GitHub Actions convention; mixing tabs breaks the parser).
- The plan assumes the local machine has `actionlint` installed for workflow validation. If not, `brew install actionlint` / `apt install actionlint` / `go install github.com/rhysd/actionlint/cmd/actionlint@latest` works. The plan provides a graceful fallback (visual inspection) if `actionlint` is unavailable.

---

## Task 0: Create feature branch

**Files:**
- None (git only)

- [ ] **Step 0.1: Create the feature branch from `master`**

```bash
cd /home/cedric/work/ghostframe
git checkout master
git pull --ff-only
git checkout -b feature/github-actions-ci
git status
```

Expected: `On branch feature/github-actions-ci` with no uncommitted changes (other than the pre-existing untracked `ghostframe-bench/target/`).

- [ ] **Step 0.2: Verify the spec file is on this branch**

```bash
ls -la docs/superpowers/specs/2026-06-06-github-actions-ci-design.md
```

Expected: file exists. (It was committed to `master` before this branch was cut; the branch inherits it.)

---

## Phase 1: Skip-list, nextest helper, and `just ci-local`

This phase ships the infrastructure that the workflows will reuse. It's
tested locally (no GitHub Actions needed) and committed first so the
workflow tasks downstream can reference concrete files.

---

### Task 1: Create the VKMS-gated skip list

**Files:**
- Create: `ci/skip-list.txt`

The list is the **exact set of e2e tests whose body calls
`setup_e2e_webgpu_gpu(...)` or passes `--drm-direct` to the test pattern**.
These need the `vkms` kernel module on the host, which GitHub-hosted runners
do not have. Local enumeration:

```bash
awk '
  /^async fn e2e_[a-z_]+/ {
    name=$0; sub(/.*fn /, "", name); sub(/\(.*/, "", name);
    in_test=1; uses_gpu=0; next
  }
  in_test {
    if ($0 ~ /setup_e2e_webgpu_gpu|--drm-direct/) uses_gpu=1
    if ($0 ~ /^}$/) { if (uses_gpu) print name; in_test=0 }
  }
' ghostframe-e2e/tests/e2e.rs | sort -u
```

As of this plan the output is 25 names; the file below pins them all
explicitly. Substring-grouping (e.g. listing `cdf53` once instead of all 10
`e2e_cdf53_*` tests individually) is intentionally **not used** because an
explicit list survives renames better — a future `e2e_cdf53_foo` test that
doesn't actually need VKMS won't accidentally be skipped.

- [ ] **Step 1.1: Create `ci/` directory and the skip list**

```bash
mkdir -p ci
```

- [ ] **Step 1.2: Write `ci/skip-list.txt`**

```
# Exhaustive list of e2e tests that require host kernel VKMS (vkms module
# loaded with enable_writeback=1 + /dev/dri bind-mounted into the test-server
# container). GitHub-hosted runners cannot load kernel modules, so these tests
# are skipped on CI and run locally.
#
# This list is mirrored from `setup_e2e_webgpu_gpu(...)` / `--drm-direct`
# callers in ghostframe-e2e/tests/e2e.rs. Generate via:
#
#   awk '/^async fn e2e_[a-z_]+/{n=$0;sub(/.*fn /,"",n);sub(/\(.*/,"",n);t=1;g=0;next} t{if($0~/setup_e2e_webgpu_gpu|--drm-direct/)g=1;if($0~/^}$/){if(g)print n;t=0}}' ghostframe-e2e/tests/e2e.rs | sort -u
#
# When a new GPU-pipeline e2e test lands, add its name here in the same PR.
# When a test no longer needs VKMS, remove it here in the same PR.
#
# Format: one test name per line. `cargo test --skip <name>` uses substring
# matching, but we list full names for rename safety. `#` introduces a
# comment; blank lines are ignored.
e2e_ack_telemetry_no_waste
e2e_cdf53_bypass_integrate
e2e_cdf53_gradient_emission
e2e_cdf53_integrate_correctness
e2e_cdf53_inverse_gradient_tile
e2e_cdf53_live_tile_state
e2e_cdf53_live_tile_state_col18
e2e_cdf53_lossless_buildup
e2e_cdf53_mixed_codecs
e2e_cdf53_server_gpu_vs_cpu_diff
e2e_cdf53_tile_watcher
e2e_decode_error_thin_uncached
e2e_headroom_guard_forces_h264
e2e_indices_raw_handshake
e2e_loss_override_forces_h264
e2e_mode_switch
e2e_palette_eviction
e2e_palrle_5pct_loss
e2e_palrle_exact_pixels
e2e_palrle_oob_index
e2e_palrle_session_reset
e2e_progressive_refinement
e2e_refinement_cancel
e2e_solid_per_tile_pixels
e2e_text_clarity
```

- [ ] **Step 1.3: Verify the list is exactly what the grep produces**

```bash
awk '
  /^async fn e2e_[a-z_]+/ {
    name=$0; sub(/.*fn /, "", name); sub(/\(.*/, "", name);
    in_test=1; uses_gpu=0; next
  }
  in_test {
    if ($0 ~ /setup_e2e_webgpu_gpu|--drm-direct/) uses_gpu=1
    if ($0 ~ /^}$/) { if (uses_gpu) print name; in_test=0 }
  }
' ghostframe-e2e/tests/e2e.rs | sort -u | diff - <(grep -v '^#' ci/skip-list.txt | grep -v '^$')
```

Expected: no output (lists match).

---

### Task 2: Write the skip-list → nextest filter helper

**Files:**
- Create: `ci/skip-list-to-nextest-expr.sh`

`cargo-nextest` doesn't take `--skip <name>` directly; it takes an `-E`
filter expression. The helper reads `ci/skip-list.txt` and emits an
expression of the form `not test(/A/) and not test(/B/) and ...`. The
helper is itself testable from the command line.

- [ ] **Step 2.1: Write the helper script**

```bash
cat > ci/skip-list-to-nextest-expr.sh <<'EOF'
#!/usr/bin/env bash
# ci/skip-list-to-nextest-expr.sh
#
# Reads ci/skip-list.txt and emits a cargo-nextest filter expression that
# excludes every listed test. Used by .github/workflows/nightly.yml.
#
# Usage:
#   $(./ci/skip-list-to-nextest-expr.sh)
#
# Output example:
#   not test(/e2e_cdf53_gradient_emission/) and not test(/e2e_mode_switch/)
#
# If the skip list is empty or contains only comments, emits `all()` (which
# nextest accepts as "match all tests").

set -euo pipefail

SKIP_FILE="${1:-ci/skip-list.txt}"

if [[ ! -r "$SKIP_FILE" ]]; then
    echo "error: cannot read $SKIP_FILE" >&2
    exit 1
fi

# Strip comments and blank lines; collect names into a bash array.
mapfile -t names < <(grep -Ev '^\s*(#|$)' "$SKIP_FILE")

if [[ ${#names[@]} -eq 0 ]]; then
    echo "all()"
    exit 0
fi

expr=""
for name in "${names[@]}"; do
    # Trim whitespace.
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    if [[ -z "$name" ]]; then
        continue
    fi
    if [[ -z "$expr" ]]; then
        expr="not test(/${name}/)"
    else
        expr="${expr} and not test(/${name}/)"
    fi
done

echo "$expr"
EOF
chmod +x ci/skip-list-to-nextest-expr.sh
```

- [ ] **Step 2.2: Run the helper and inspect the output**

```bash
./ci/skip-list-to-nextest-expr.sh
```

Expected: a single line starting `not test(/e2e_ack_telemetry_no_waste/) and not test(/e2e_cdf53_bypass_integrate/) and ...` (25 conjuncts in alphabetical order).

- [ ] **Step 2.3: Sanity-test with an empty file**

```bash
echo '# only a comment' > /tmp/empty-skip-list.txt
./ci/skip-list-to-nextest-expr.sh /tmp/empty-skip-list.txt
rm /tmp/empty-skip-list.txt
```

Expected: `all()`

- [ ] **Step 2.4: Optional — run `shellcheck` if available**

```bash
if command -v shellcheck >/dev/null; then
    shellcheck ci/skip-list-to-nextest-expr.sh
else
    echo "shellcheck not installed; skipping"
fi
```

Expected: no warnings (or "skipping" if shellcheck is absent).

---

### Task 3: Add the `ci-local` target to the Justfile

**Files:**
- Modify: `Justfile`

`just ci-local` runs the fast tier locally in the same order CI does, so a
contributor can pre-flight a PR with one command. It does **not** run
e2e — that's `just test-e2e`.

- [ ] **Step 3.1: Read the current `Justfile`**

```bash
cat Justfile
```

Confirm it matches what was captured during brainstorming (build, build-release, test-unit, test-e2e, containers-build, lint, fmt-check, fmt).

- [ ] **Step 3.2: Append the `ci-local` recipe to the Justfile**

Open `Justfile` and add at the end:

```make

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
```

- [ ] **Step 3.3: Run `just ci-local` to verify the recipe works on the current tree**

```bash
just ci-local
```

Expected: all phases run cleanly and print `=== ci-local passed ===` at the end. If `tsc` is missing from `ghostframe-web-client/node_modules`, run `cd ghostframe-web-client && npm install` first. If the `cbindgen` step fails because the committed header is stale, that's an *existing* problem unrelated to this CI work — fix it in a separate commit before proceeding.

---

### Task 4: Commit Phase 1

- [ ] **Step 4.1: Stage and commit**

```bash
git add ci/skip-list.txt ci/skip-list-to-nextest-expr.sh Justfile
git status
git diff --staged
```

Expected: 3 files staged (2 new, 1 modified).

- [ ] **Step 4.2: Create the commit**

```bash
git commit -m "$(cat <<'EOF'
ci: skip list, nextest filter helper, just ci-local

Phase 1 of the GitHub Actions CI rollout: ship the
infrastructure pieces the workflow tasks will reuse.

- ci/skip-list.txt: 25 VKMS-gated e2e test names that
  GitHub-hosted runners cannot run.
- ci/skip-list-to-nextest-expr.sh: emits a nextest -E
  filter expression from the skip list for use in the
  nightly sweep.
- Justfile: add `just ci-local` mirroring the fast CI
  tier so contributors can pre-flight a PR locally.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

Expected: commit succeeds, `git log -1 --stat` shows 3 files changed.

---

## Phase 2: Composite action for system dependencies

`e2e.yml` and `nightly.yml` both need the same `apt-get install` step on
the runner before they can build `ghostframe-xdaemon` host-side. Putting
that list in a composite action means changes only happen in one place.

---

### Task 5: Write the `_setup-system-deps` composite action

**Files:**
- Create: `.github/workflows/_setup-system-deps/action.yml`

The package list mirrors what's needed to **build** ghostframe on a host
(not what's needed at runtime in the test-server container). Specifically:
the dev packages for ffmpeg + x264 + X11 + DRM + libclang, plus
`build-essential` and `pkg-config`. We do **not** install
`xserver-xorg-*`, `mesa-utils`, or `vulkan-tools` — those run inside the
container, not on the host runner.

- [ ] **Step 5.1: Create the action directory**

```bash
mkdir -p .github/workflows/_setup-system-deps
```

- [ ] **Step 5.2: Write the composite action**

```bash
cat > .github/workflows/_setup-system-deps/action.yml <<'EOF'
name: Setup ghostframe system dependencies
description: >
  Installs the apt packages required to build ghostframe on a runner
  (clang, ffmpeg dev libs, x264, X11 dev headers, DRM dev headers).
  Reused by e2e.yml and nightly.yml so the package list lives in one
  place. Production runtime libs live inside the test-server Docker
  image and are not installed here.

runs:
  using: composite
  steps:
    - name: apt-get update
      shell: bash
      run: sudo apt-get update -qq

    - name: install build deps
      shell: bash
      run: |
        sudo apt-get install -y --no-install-recommends \
          build-essential \
          pkg-config \
          clang \
          libclang-dev \
          libavcodec-dev \
          libavformat-dev \
          libavutil-dev \
          libswscale-dev \
          libavdevice-dev \
          libx264-dev \
          libx11-dev \
          libxext-dev \
          libxdamage-dev \
          libdrm-dev
EOF
```

- [ ] **Step 5.3: Lint the composite action**

```bash
if command -v actionlint >/dev/null; then
    actionlint .github/workflows/_setup-system-deps/action.yml
else
    echo "actionlint not installed; visually inspect the YAML"
    cat .github/workflows/_setup-system-deps/action.yml
fi
```

Expected: no errors (or visual confirmation that the YAML is valid).

---

### Task 6: Commit Phase 2

- [ ] **Step 6.1: Stage and commit**

```bash
git add .github/workflows/_setup-system-deps/action.yml
git commit -m "$(cat <<'EOF'
ci: composite action for system build deps

Phase 2 of CI rollout. Centralises the `apt-get install`
step that e2e.yml and nightly.yml both need so the package
list lives in one place. Mirrors the build-time deps of
the test-server Dockerfile; runtime deps (Xorg, mesa,
vulkan) stay inside the container.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 3: Fast tier (`ci.yml`)

Seven parallel jobs on `ubuntu-24.04`. Each runs one tool. All required
for merge.

---

### Task 7: Write `.github/workflows/ci.yml`

**Files:**
- Create: `.github/workflows/ci.yml`

Job names are pinned with explicit `name:` fields so renames cannot
silently break the branch-protection contract.

- [ ] **Step 7.1: Write the workflow file**

```bash
cat > .github/workflows/ci.yml <<'EOF'
name: ci

on:
  pull_request:
  push:
    branches: [master]

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

permissions:
  contents: read

jobs:
  fmt:
    name: fmt
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - run: cargo fmt --all -- --check

  clippy:
    name: clippy
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/workflows/_setup-system-deps
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: ci-fast
      - run: cargo clippy --workspace --all-targets -- -D warnings

  unit:
    name: unit
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/workflows/_setup-system-deps
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: ci-fast
      - run: cargo test --workspace --lib

  build-release:
    name: build-release
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/workflows/_setup-system-deps
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: ci-release
      - run: cargo build --workspace --release --exclude ghostframe-e2e

  web-client:
    name: web-client
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          cache: npm
          cache-dependency-path: ghostframe-web-client/package-lock.json
      - name: install + build
        working-directory: ghostframe-web-client
        run: |
          npm ci
          npm run build
          npx tsc --noEmit
      - name: upload dist artifact for e2e.yml
        uses: actions/upload-artifact@v4
        with:
          name: web-client-dist
          path: ghostframe-web-client/dist/
          retention-days: 7
          if-no-files-found: error

  cbindgen-check:
    name: cbindgen-check
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/workflows/_setup-system-deps
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: ci-fast
      - name: regenerate header via build.rs
        run: cargo check -p ghostframe-lib
      - name: verify committed header is up to date
        run: |
          if ! git diff --exit-code ghostframe-lib/include/ghostframe.h; then
            echo "::error::ghostframe-lib/include/ghostframe.h is out of date."
            echo "Run `cargo check -p ghostframe-lib` locally and commit the regenerated header."
            exit 1
          fi

  go-vet:
    name: go-vet
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
          cache-dependency-path: ghostbridge/go.sum
      - working-directory: ghostbridge
        run: |
          go vet ./...
          go build ./...
EOF
```

- [ ] **Step 7.2: Lint `ci.yml`**

```bash
if command -v actionlint >/dev/null; then
    actionlint .github/workflows/ci.yml
else
    echo "actionlint not installed; visually inspect the YAML"
fi
```

Expected: no errors. If `actionlint` reports `SC2086` or other shellcheck integration warnings inside the embedded `run:` blocks, those are advisory and may be ignored if the variables are known-safe (e.g. the cbindgen check's error message).

- [ ] **Step 7.3: Test the `actions/setup-go` version pin**

Confirm `ghostbridge/go.mod` declares a Go version compatible with `1.25`:

```bash
head -3 ghostbridge/go.mod
```

Expected: `go 1.25.5` or similar. If the major version differs (e.g. project bumps to `1.26`), update the `go-version: '1.25'` lines in `ci.yml` accordingly.

---

### Task 8: Commit Phase 3

- [ ] **Step 8.1: Stage and commit**

```bash
git add .github/workflows/ci.yml
git commit -m "$(cat <<'EOF'
ci: add ci.yml — fast tier (fmt/clippy/unit/release/web/cbindgen/go)

Phase 3. Seven parallel jobs on ubuntu-24.04. Triggers on
PRs and pushes to master. Cancel-in-progress per ref so a
force-push immediately abandons the prior run. Each job
isolated for clear logs. Cache keys split (ci-fast vs
ci-release) to avoid feature-flag thrash.

web-client uploads its dist/ as an artifact named
`web-client-dist` so e2e.yml can download it instead of
rebuilding.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 4: E2E tier (`e2e.yml`)

Three jobs: two small Rust integration tests + the main e2e suite (minus
the VKMS-gated subset).

---

### Task 9: Write `.github/workflows/e2e.yml`

**Files:**
- Create: `.github/workflows/e2e.yml`

- [ ] **Step 9.1: Write the workflow file**

```bash
cat > .github/workflows/e2e.yml <<'EOF'
name: e2e

on:
  pull_request:
  push:
    branches: [master]

concurrency:
  group: ${{ github.workflow }}-${{ github.ref }}
  cancel-in-progress: true

permissions:
  contents: read
  actions: read  # required to read web-client-dist artifact from ci.yml

jobs:
  loopback-h3:
    name: loopback-h3
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/workflows/_setup-system-deps
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: e2e
      - run: cargo test --test loopback_h3

  harness-smoke:
    name: harness-smoke
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - uses: ./.github/workflows/_setup-system-deps
      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: e2e
      - run: cargo test --test harness_smoke

  e2e:
    name: e2e
    runs-on: ubuntu-24.04
    timeout-minutes: 60
    steps:
      - uses: actions/checkout@v4

      - uses: ./.github/workflows/_setup-system-deps

      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'

      - uses: dtolnay/rust-toolchain@stable

      - uses: Swatinem/rust-cache@v2
        with:
          shared-key: e2e

      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          cache: npm
          cache-dependency-path: ghostframe-web-client/package-lock.json

      - name: Download web-client dist from ci.yml
        uses: actions/download-artifact@v4
        with:
          name: web-client-dist
          path: ghostframe-web-client/dist/
          # Look in the same workflow run; falls back to building locally
          # if the artifact isn't available (e.g. ci.yml failed early).
          github-token: ${{ github.token }}
          run-id: ${{ github.run_id }}
        continue-on-error: true

      - name: Fallback web-client build if artifact missing
        if: hashFiles('ghostframe-web-client/dist/**') == ''
        working-directory: ghostframe-web-client
        run: |
          echo "web-client-dist artifact not found, building locally"
          npm ci
          npm run build

      - uses: docker/setup-buildx-action@v3

      - name: Build host-side release binaries (for containers-build)
        run: |
          cargo build --release \
            -p ghostframe-xdaemon \
            -p ghostframe-test-pattern

      - name: Build test-headscale image
        uses: docker/build-push-action@v5
        with:
          context: tests/containers/headscale/
          file: tests/containers/headscale/Dockerfile
          tags: ghostframe/test-headscale:latest
          load: true
          cache-from: type=gha,scope=test-headscale
          cache-to: type=gha,mode=max,scope=test-headscale

      - name: Build test-server image
        uses: docker/build-push-action@v5
        with:
          context: .
          file: tests/containers/test-server/Dockerfile
          tags: ghostframe/test-server:latest
          load: true
          cache-from: type=gha,scope=test-server
          cache-to: type=gha,mode=max,scope=test-server

      - name: Build skip arguments from ci/skip-list.txt
        id: skip-args
        run: |
          args=""
          while IFS= read -r name; do
            # Strip leading/trailing whitespace
            name="${name#"${name%%[![:space:]]*}"}"
            name="${name%"${name##*[![:space:]]}"}"
            [[ -z "$name" || "$name" =~ ^# ]] && continue
            args="$args --skip $name"
          done < ci/skip-list.txt
          echo "args=$args" >> "$GITHUB_OUTPUT"

      - name: Run e2e tests (VKMS-gated subset skipped)
        run: |
          cargo test --test e2e -- \
            --test-threads=1 \
            ${{ steps.skip-args.outputs.args }}

      - name: Capture container logs on failure
        if: failure()
        run: |
          mkdir -p /tmp/ci-artifacts
          docker ps -a > /tmp/ci-artifacts/docker-ps.txt 2>&1 || true
          for c in ghostframe-server headscale; do
            if docker inspect "$c" >/dev/null 2>&1; then
              docker logs "$c" > "/tmp/ci-artifacts/${c}.stdout.log" 2>"/tmp/ci-artifacts/${c}.stderr.log" || true
            fi
          done

      - name: Upload diagnostic artifacts on failure
        if: failure()
        uses: actions/upload-artifact@v4
        with:
          name: e2e-failure-logs
          path: |
            /tmp/ci-artifacts/
            /tmp/ghostframe-*
          retention-days: 7
          if-no-files-found: ignore
EOF
```

- [ ] **Step 9.2: Lint `e2e.yml`**

```bash
if command -v actionlint >/dev/null; then
    actionlint .github/workflows/e2e.yml
fi
```

Expected: no errors.

- [ ] **Step 9.3: Verify the skip-args step matches what would actually run**

Simulate the inline Bash locally:

```bash
args=""
while IFS= read -r name; do
  name="${name#"${name%%[![:space:]]*}"}"
  name="${name%"${name##*[![:space:]]}"}"
  [[ -z "$name" || "$name" =~ ^# ]] && continue
  args="$args --skip $name"
done < ci/skip-list.txt
echo "$args" | head -c 200
echo "..."
```

Expected: starts ` --skip e2e_ack_telemetry_no_waste --skip e2e_cdf53_bypass_integrate ...`.

---

### Task 10: Commit Phase 4

- [ ] **Step 10.1: Stage and commit**

```bash
git add .github/workflows/e2e.yml
git commit -m "$(cat <<'EOF'
ci: add e2e.yml — loopback-h3, harness-smoke, full e2e

Phase 4. Three jobs on ubuntu-24.04. The e2e job
downloads ghostframe-web-client/dist/ from the
web-client artifact published by ci.yml (with a
fallback rebuild if missing), builds both container
images with buildx-gha caching scoped per image,
then runs `cargo test --test e2e -- --test-threads=1`
with the skip list applied via dynamically-built
--skip args. Captures container logs and /tmp/ghostframe-*
artifacts on failure for offline debugging.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 5: Nightly tier (`nightly.yml`)

Scheduled at 03:00 UTC + manual dispatch. Uses `cargo-nextest --retries 2`
and opens an issue on failure with dedup against existing open issues.

---

### Task 11: Write `.github/workflows/nightly.yml`

**Files:**
- Create: `.github/workflows/nightly.yml`

- [ ] **Step 11.1: Write the workflow file**

```bash
cat > .github/workflows/nightly.yml <<'EOF'
name: nightly

on:
  schedule:
    - cron: '0 3 * * *'  # 03:00 UTC daily
  workflow_dispatch:

permissions:
  contents: read
  issues: write  # to open/comment on the nightly-failure issue

jobs:
  e2e-sweep:
    name: e2e-sweep
    runs-on: ubuntu-24.04
    timeout-minutes: 120
    outputs:
      conclusion: ${{ steps.run-tests.outcome }}
    steps:
      - uses: actions/checkout@v4

      - uses: ./.github/workflows/_setup-system-deps

      - uses: actions/setup-go@v5
        with:
          go-version: '1.25'

      - uses: dtolnay/rust-toolchain@stable

      - uses: actions/setup-node@v4
        with:
          node-version: '20'

      - uses: taiki-e/install-action@v2
        with:
          tool: nextest

      - uses: docker/setup-buildx-action@v3

      - name: Build web client
        working-directory: ghostframe-web-client
        run: |
          npm ci
          npm run build

      - name: Build host-side release binaries
        run: |
          cargo build --release \
            -p ghostframe-xdaemon \
            -p ghostframe-test-pattern

      - name: Build test-headscale image
        run: |
          docker build \
            -t ghostframe/test-headscale:latest \
            tests/containers/headscale/

      - name: Build test-server image
        run: |
          docker build \
            -t ghostframe/test-server:latest \
            -f tests/containers/test-server/Dockerfile \
            .

      - name: Run e2e sweep with nextest retries
        id: run-tests
        run: |
          expr="$(./ci/skip-list-to-nextest-expr.sh)"
          echo "Filter expression: $expr"
          cargo nextest run \
            --test e2e \
            --no-fail-fast \
            --retries 2 \
            --test-threads 1 \
            -E "$expr"

      - name: Upload nextest report
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: nightly-nextest-report
          path: target/nextest/
          retention-days: 14
          if-no-files-found: ignore

  report-failure:
    name: report-failure
    needs: e2e-sweep
    if: failure() || needs.e2e-sweep.outputs.conclusion == 'failure'
    runs-on: ubuntu-24.04
    steps:
      - uses: actions/checkout@v4
      - name: Open or comment on nightly-failure issue
        env:
          GH_TOKEN: ${{ github.token }}
          RUN_URL: ${{ github.server_url }}/${{ github.repository }}/actions/runs/${{ github.run_id }}
        run: |
          set -euo pipefail
          title="Nightly e2e sweep failed"
          # Find an existing open issue with the same title.
          existing="$(gh issue list \
            --label nightly-failure \
            --state open \
            --search "$title in:title" \
            --json number \
            --jq '.[0].number // empty')"
          body="Nightly run failed.

          Run: $RUN_URL
          Commit: ${{ github.sha }}
          Triggered by: ${{ github.event_name }}"
          if [[ -n "$existing" ]]; then
            echo "Commenting on existing issue #$existing"
            gh issue comment "$existing" --body "$body"
          else
            echo "Opening new issue"
            gh issue create \
              --title "$title" \
              --label nightly-failure \
              --body "$body"
          fi
EOF
```

- [ ] **Step 11.2: Lint `nightly.yml`**

```bash
if command -v actionlint >/dev/null; then
    actionlint .github/workflows/nightly.yml
fi
```

Expected: no errors.

- [ ] **Step 11.3: Verify the `nightly-failure` label will be created automatically**

GitHub's REST API auto-creates labels when used in `gh issue create --label`,
provided the workflow's GITHUB_TOKEN has `issues: write` (which we set on
the job). No pre-creation step needed.

---

### Task 12: Commit Phase 5

- [ ] **Step 12.1: Stage and commit**

```bash
git add .github/workflows/nightly.yml
git commit -m "$(cat <<'EOF'
ci: add nightly.yml — retry-enabled e2e sweep + issue on failure

Phase 5. Scheduled 03:00 UTC + workflow_dispatch.
cargo-nextest run with --retries 2 over the same skip
list. No caching: catches what a fresh contributor
checkout would see. On failure, opens a "Nightly e2e
sweep failed" issue labeled nightly-failure (or comments
on the existing open one — dedup by title).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 6: Documentation

`docs/ci.md` and a one-line README update.

---

### Task 13: Write `docs/ci.md`

**Files:**
- Create: `docs/ci.md`

- [ ] **Step 13.1: Write the contributor doc**

```bash
cat > docs/ci.md <<'EOF'
# Continuous integration

Ghostframe runs three GitHub Actions workflows. This page documents what
each one does, how to reproduce a failure locally, and the triage flow for
nightly-sweep issues.

## Workflows

### `ci.yml` — fast tier

Runs on every pull request and every push to `master`. Seven parallel jobs:

| Job | What it runs | Local equivalent |
|---|---|---|
| `fmt` | `cargo fmt --all -- --check` | `just fmt-check` |
| `clippy` | `cargo clippy --workspace --all-targets -- -D warnings` | `just lint` |
| `unit` | `cargo test --workspace --lib` | `just test-unit` |
| `build-release` | `cargo build --workspace --release --exclude ghostframe-e2e` | `cargo build --release` |
| `web-client` | `npm ci && npm run build && npx tsc --noEmit` | `just web-client-build` |
| `cbindgen-check` | `cargo check -p ghostframe-lib` + `git diff --exit-code ghostframe-lib/include/ghostframe.h` | `cargo check -p ghostframe-lib && git status` |
| `go-vet` | `go vet ./... && go build ./...` in `ghostbridge/` | `cd ghostbridge && go vet ./... && go build ./...` |

All seven are required for merge.

To run the whole fast tier locally in one shot: `just ci-local`.

### `e2e.yml` — end-to-end tier

Runs on every pull request and every push to `master`. Three jobs:

| Job | What it runs |
|---|---|
| `loopback-h3` | `cargo test --test loopback_h3` (sans-IO QUIC/H3) |
| `harness-smoke` | `cargo test --test harness_smoke` |
| `e2e` | full `cargo test --test e2e` suite minus the VKMS-gated subset |

All three are required for merge.

#### Why some e2e tests are skipped

About 25 of the 41 e2e tests use `setup_e2e_webgpu_gpu(...)` or
`--drm-direct`, which require a host kernel with the `vkms` module loaded
(`enable_writeback=1`). GitHub-hosted runners do not allow loading kernel
modules, so those tests cannot run on CI.

The exact list lives in [`ci/skip-list.txt`](../ci/skip-list.txt). When a
new GPU-pipeline e2e test lands, add its name there in the same PR. When a
test no longer needs VKMS, remove it.

Developers on a machine with VKMS loaded run the full suite locally with
`just test-e2e`; the skip list is **not** consulted locally.

### `nightly.yml` — nightly sweep

Runs at 03:00 UTC daily and on `workflow_dispatch`. Same skip list as
`e2e.yml`, but uses `cargo-nextest run --retries 2` so flaky tests are
re-attempted before failing the run.

On failure, the workflow opens (or comments on) an issue labeled
`nightly-failure`. The nightly is **not required for merge** — it's a
post-merge canary.

#### Triaging a nightly-failure issue

1. Open the linked run; download the `nightly-nextest-report` artifact.
2. The report's `target/nextest/.../report.junit.xml` lists per-test
   attempts. A test that failed all three attempts is a real regression;
   one that passed on retry is a flake.
3. If the failure is in a test that's documented as flaky in
   `~/.claude/projects/-home-cedric-work-ghostframe/memory/feedback_e2e_test_isolation_flake.md`
   (or similar memory files), it's expected churn — comment on the issue
   and leave it open until the underlying flake is fixed.
4. If the failure is in a previously-stable test, it's a regression — open
   a PR that fixes it; close the issue with a reference to the fix commit.

## Reproducing CI failures locally

The CI runs `cargo` commands directly with no special environment beyond
the system packages installed by the
[`_setup-system-deps`](.github/workflows/_setup-system-deps/action.yml)
composite action. Mirroring locally:

```bash
# Install build deps (Ubuntu 24.04)
sudo apt-get install -y \
  build-essential pkg-config clang libclang-dev \
  libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
  libx264-dev \
  libx11-dev libxext-dev libxdamage-dev libdrm-dev

# Then mirror the fast tier
just ci-local

# Or mirror the e2e tier (requires Docker)
just test-e2e
```

For the e2e tier, the CI also passes `--skip` flags constructed from
`ci/skip-list.txt`. Locally, you typically want the full suite, so don't
pass `--skip` and just run `just test-e2e`.

## Configuring branch protection (one-time admin step)

The workflows produce status checks that should be marked required in the
repo's branch-protection rules for `master`. After the workflows have run
at least once (so GitHub knows the status-check names), apply:

```bash
gh api \
  --method PUT \
  -H "Accept: application/vnd.github+json" \
  repos/<OWNER>/<REPO>/branches/master/protection \
  -F required_status_checks[strict]=false \
  -F required_status_checks[contexts][]='fmt' \
  -F required_status_checks[contexts][]='clippy' \
  -F required_status_checks[contexts][]='unit' \
  -F required_status_checks[contexts][]='build-release' \
  -F required_status_checks[contexts][]='web-client' \
  -F required_status_checks[contexts][]='cbindgen-check' \
  -F required_status_checks[contexts][]='go-vet' \
  -F required_status_checks[contexts][]='loopback-h3' \
  -F required_status_checks[contexts][]='harness-smoke' \
  -F required_status_checks[contexts][]='e2e' \
  -F enforce_admins=false \
  -F required_pull_request_reviews= \
  -F restrictions=
```

(Replace `<OWNER>/<REPO>` with the actual repo path. `strict=false`
disables the "require branches to be up to date" rule that would force a
re-run of CI on every parallel merge — see the design spec for why we keep
this off.)
EOF
```

- [ ] **Step 13.2: Visually inspect the doc**

```bash
head -60 docs/ci.md
```

Expected: clean markdown, table renders, no placeholders.

---

### Task 14: Update README

**Files:**
- Modify: `README.md`

Add one line under the "Contributing" section pointing to `docs/ci.md`.

- [ ] **Step 14.1: Add the CI line to the contributing section**

Find the line in `README.md` that reads:

```
- pass `just lint` and `just fmt-check`,
```

Replace it with:

```
- pass `just lint`, `just fmt-check`, and the checks described in [docs/ci.md](docs/ci.md),
```

Use Edit to make this exact change.

- [ ] **Step 14.2: Verify the change**

```bash
grep -n 'docs/ci.md\|just lint' README.md
```

Expected: the modified line includes the doc link.

---

### Task 15: Commit Phase 6

- [ ] **Step 15.1: Stage and commit**

```bash
git add docs/ci.md README.md
git commit -m "$(cat <<'EOF'
docs(ci): contributor-facing CI guide

Phase 6. Explains the three workflows, why some e2e
tests are skipped, how to reproduce CI failures
locally, the nightly-issue triage flow, and the
one-time gh api command for branch protection.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Phase 7: Push, observe, iterate

CI workflows cannot be fully validated without running on GitHub. This
phase pushes the branch, opens a PR, and watches the first runs.

---

### Task 16: Push and open PR

- [ ] **Step 16.1: Verify the branch history**

```bash
git log --oneline master..HEAD
```

Expected: six commits (Phase 1 through Phase 6) in order.

- [ ] **Step 16.2: Push the branch**

```bash
git push -u origin feature/github-actions-ci
```

- [ ] **Step 16.3: Open the PR**

```bash
gh pr create --title "ci: GitHub Actions — fast + e2e + nightly tiers" --body "$(cat <<'EOF'
## Summary

Implements the CI rollout designed in
[docs/superpowers/specs/2026-06-06-github-actions-ci-design.md](docs/superpowers/specs/2026-06-06-github-actions-ci-design.md).

Three workflows under `.github/workflows/`:

- `ci.yml`: fmt, clippy, unit, build-release, web-client, cbindgen-check, go-vet (seven parallel jobs)
- `e2e.yml`: loopback-h3, harness-smoke, full e2e suite (minus the 25 VKMS-gated tests)
- `nightly.yml`: scheduled 03:00 UTC, `cargo nextest --retries 2`, opens issue on failure

Plus:
- `ci/skip-list.txt` + helper script
- `.github/workflows/_setup-system-deps/` composite action
- `just ci-local` Justfile target
- `docs/ci.md` contributor doc

Dependabot config ships in a separate follow-up PR (the workflows must
exist first so its PRs land on a branch where CI actually gates them).

## Test plan

- [ ] All seven `ci.yml` jobs pass.
- [ ] All three `e2e.yml` jobs pass.
- [ ] `just ci-local` passes on master.
- [ ] (Manual, post-merge) Branch protection configured per `docs/ci.md`.
EOF
)"
```

- [ ] **Step 16.4: Watch the run**

```bash
gh pr checks --watch
```

Expected: the seven ci.yml jobs and the three e2e.yml jobs all start.
Watching the run surfaces any actual failures.

---

### Task 17: Fix first-run failures

CI almost never goes green on the first push. Likely failure modes and
their fixes:

- **`apt-get` package name mismatch** (e.g. `libavdevice-dev` not available
  in the ubuntu-24.04 mirror): fix the composite action's package list,
  commit `ci: fix apt package name`, push.
- **`actions/setup-go` major version mismatch**: bump `go-version: '1.25'`
  to whatever `ghostbridge/go.mod` declares.
- **`actions/upload-artifact` "missing files"**: if `web-client` job
  produces a `dist/` at a slightly different path than expected, fix the
  `path:` field in the upload step.
- **`actions/download-artifact` permission denied across workflows**:
  ensure `permissions: actions: read` is set on the `e2e` job. (The plan
  already sets this.)
- **`Swatinem/rust-cache` cache hit but stale `target/`**: rare, but if it
  happens bump the `shared-key:` value (e.g. `ci-fast-v2`) on the affected
  job.
- **`cargo nextest install` fails** in `nightly.yml`: nightly doesn't run
  until the cron fires, so this won't surface in the PR run. Trigger it
  manually via `gh workflow run nightly.yml --ref feature/github-actions-ci`
  to test it.

- [ ] **Step 17.1: Watch the PR run output for failures**

```bash
gh pr checks
gh run list --workflow=ci.yml --limit 3
gh run list --workflow=e2e.yml --limit 3
```

- [ ] **Step 17.2: For each failure, drill into logs**

```bash
gh run view --log-failed
```

- [ ] **Step 17.3: For each fix, commit and push directly to the same branch**

```bash
git add <changed files>
git commit -m "ci: <one-line fix description>"
git push
```

Repeat until all checks are green. Once green, the PR is ready to merge.

- [ ] **Step 17.4: Trigger `nightly.yml` manually to validate**

```bash
gh workflow run nightly.yml --ref feature/github-actions-ci
gh run list --workflow=nightly.yml --limit 1
gh run watch
```

Expected: nightly run starts, runs the same e2e set, and completes (pass
or fail). If it passes, great. If it fails on retry, verify that the
issue is opened (`gh issue list --label nightly-failure`). If the failure
is real (regression), fix it in this PR; if it's a known flake, that's
expected behavior — close the issue manually after the PR merges.

---

### Task 18: Merge

- [ ] **Step 18.1: Once CI is green, merge the PR**

```bash
gh pr merge --merge --delete-branch
```

(Use `--merge`, not `--squash` or `--rebase`, to preserve the per-phase
commit history.)

- [ ] **Step 18.2: Update local master**

```bash
git checkout master
git pull --ff-only
git log --oneline -10
```

Expected: the six phase commits appear in `master` history.

- [ ] **Step 18.3: Apply branch protection**

Follow the `gh api` command in `docs/ci.md`, substituting the real
`<OWNER>/<REPO>`. This is a one-time admin step.

Verify:

```bash
gh api repos/<OWNER>/<REPO>/branches/master/protection
```

Expected: JSON output listing all ten required status checks.

---

## Phase 8: Dependabot (separate commit)

Dependabot ships as its own PR after CI is green and branch protection is
configured. This is so Dependabot's first batch of PRs lands on a branch
where CI actually gates them.

---

### Task 19: Create the Dependabot config branch

**Files:**
- None (git only)

- [ ] **Step 19.1: Create a fresh branch from updated master**

```bash
git checkout master
git pull --ff-only
git checkout -b feature/dependabot-config
```

---

### Task 20: Write `.github/dependabot.yml`

**Files:**
- Create: `.github/dependabot.yml`

- [ ] **Step 20.1: Write the config**

```bash
cat > .github/dependabot.yml <<'EOF'
version: 2
updates:
  - package-ecosystem: cargo
    directory: /
    schedule:
      interval: weekly
      day: monday
      time: '06:00'
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    labels: [dependencies, rust]
    groups:
      cargo-minor-patch:
        update-types: [minor, patch]

  - package-ecosystem: npm
    directory: /ghostframe-web-client
    schedule:
      interval: weekly
      day: monday
      time: '06:00'
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    labels: [dependencies, npm]
    groups:
      npm-minor-patch:
        update-types: [minor, patch]

  - package-ecosystem: gomod
    directory: /ghostbridge
    schedule:
      interval: weekly
      day: monday
      time: '06:00'
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    labels: [dependencies, go]
    groups:
      gomod-all:
        patterns: ['*']

  - package-ecosystem: github-actions
    directory: /
    schedule:
      interval: weekly
      day: monday
      time: '06:00'
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    labels: [dependencies, actions]
    groups:
      actions-all:
        patterns: ['*']

  - package-ecosystem: docker
    directory: /tests/containers/test-server
    schedule:
      interval: weekly
      day: monday
      time: '06:00'
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    labels: [dependencies, docker]

  - package-ecosystem: docker
    directory: /tests/containers/headscale
    schedule:
      interval: weekly
      day: monday
      time: '06:00'
      timezone: Etc/UTC
    open-pull-requests-limit: 5
    labels: [dependencies, docker]
EOF
```

- [ ] **Step 20.2: Validate the YAML**

```bash
if command -v actionlint >/dev/null; then
    # actionlint doesn't lint dependabot.yml, but yaml-lint or plain Python does
    python3 -c "import yaml; yaml.safe_load(open('.github/dependabot.yml'))"
fi
```

Expected: no output (parses cleanly).

---

### Task 21: Commit, push, open PR

- [ ] **Step 21.1: Commit**

```bash
git add .github/dependabot.yml
git commit -m "$(cat <<'EOF'
ci: enable Dependabot for cargo, npm, gomod, actions, docker

Phase 8 of the CI rollout (separate commit per design).
Weekly Monday 06:00 UTC schedule across five ecosystems.
Cargo and npm group minor + patch updates; go, actions
and docker group everything. Five PRs max per ecosystem.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

- [ ] **Step 21.2: Push and open PR**

```bash
git push -u origin feature/dependabot-config
gh pr create --title "ci: enable Dependabot" --body "$(cat <<'EOF'
## Summary

Adds `.github/dependabot.yml` covering five ecosystems:

- `cargo` (workspace root)
- `npm` (ghostframe-web-client)
- `gomod` (ghostbridge)
- `github-actions` (workflow pins)
- `docker` (test-server + headscale base images)

All ecosystems run weekly Monday 06:00 UTC. Cargo and npm
group minor + patch into one PR each; go/actions/docker
group everything.

## Test plan

- [ ] CI passes (gates this PR via the workflows merged in #<previous PR>).
- [ ] First Monday after merge, Dependabot opens at most ~5 PRs (one or two per ecosystem) and each one is gated by CI.
EOF
)"
```

- [ ] **Step 21.3: Watch CI**

```bash
gh pr checks --watch
```

Expected: all ten required checks pass (this PR only changes
`.github/dependabot.yml`, so e2e should hit warm caches and run quickly).

- [ ] **Step 21.4: Merge**

```bash
gh pr merge --merge --delete-branch
```

---

## Verification checklist

After both PRs are merged:

- [ ] Branch protection is configured: `gh api repos/<OWNER>/<REPO>/branches/master/protection` lists all ten required checks.
- [ ] `just ci-local` passes on master.
- [ ] `just test-e2e` passes on master (locally, with VKMS loaded).
- [ ] `gh workflow run nightly.yml` succeeds (manually triggered to confirm cron-side path).
- [ ] First Monday after merge: Dependabot opens at most five PRs across ecosystems and CI gates each one.

---

## Self-review notes

Spec coverage check:

- ✅ Three workflows (`ci.yml`, `e2e.yml`, `nightly.yml`) — Tasks 7, 9, 11
- ✅ Composite action for system deps — Task 5
- ✅ Skip list at `ci/skip-list.txt` — Task 1
- ✅ Nextest expression helper — Task 2
- ✅ `docs/ci.md` contributor doc with `gh api` snippet — Task 13
- ✅ README update — Task 14
- ✅ Justfile `ci-local` — Task 3
- ✅ Dependabot in separate commit — Tasks 19-21
- ✅ Caching strategy (rust-cache shared-keys, buildx-gha scopes) — embedded in Tasks 7, 9
- ✅ Concurrency control per ref with cancel-in-progress — embedded in workflow YAML
- ✅ Required vs informational checks — surfaced in Task 18 + Task 13
- ✅ Failure-mode artifact upload — Task 9 (`Upload diagnostic artifacts on failure` step)
- ✅ Nightly issue dedup — Task 11

Placeholder scan: no TBDs, TODOs, or unspecified commands. Every code
block is the literal content to write or run.

Type consistency: job names (`fmt`, `clippy`, `unit`, `build-release`,
`web-client`, `cbindgen-check`, `go-vet`, `loopback-h3`, `harness-smoke`,
`e2e`) are identical between the workflow YAML, the branch-protection
command, and the contributor doc.

Cross-task references: Task 9 (e2e.yml) downloads the artifact uploaded
by Task 7 (web-client job) — both name it `web-client-dist`. Task 11
(nightly) calls the helper from Task 2.
