# Workspace test-crate split — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Move `ghostframe-lib`'s integration tests (`tests/e2e.rs`, `tests/e2e/`, `tests/loopback_h3.rs`) and `benches/*` into two new workspace crates (`ghostframe-e2e`, `ghostframe-bench`), removing 8 heavy dev-dependencies from `ghostframe-lib`'s compilation unit.

**Architecture:** Create two skeleton crates (Cargo.toml + empty `src/lib.rs`), move integration tests + benches into them, then trim `ghostframe-lib`'s `[dev-dependencies]` and `[[bench]]` entries. Each step is one commit, independently revertible.

**Tech Stack:** Cargo workspace. No code logic changes — only Cargo.toml edits and file moves (`git mv`).

**Branch:** master (user's established workflow throughout the cleanup session).

**Spec reference:** `docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md`.

---

## File Structure (post-implementation)

```
ghostframe/
├── Cargo.toml                              # workspace members += 2
├── ghostframe-lib/
│   ├── Cargo.toml                          # [dev-dependencies] -8, [[bench]] -3
│   ├── src/                                # unchanged
│   └── tests/
│       └── gpu_pipeline.rs                 # stays
├── ghostframe-test-pattern/                # unchanged
├── ghostframe-xdaemon/                     # unchanged
├── ghostframe-e2e/                         # NEW
│   ├── Cargo.toml
│   ├── src/lib.rs                          # empty placeholder
│   └── tests/
│       ├── e2e.rs
│       ├── e2e/
│       │   ├── helpers.rs
│       │   └── golden/
│       └── loopback_h3.rs
└── ghostframe-bench/                       # NEW
    ├── Cargo.toml
    ├── src/lib.rs                          # empty placeholder
    └── benches/
        ├── codec_callgrind.rs
        ├── codec_latency.rs
        ├── pipeline_throughput.rs
        └── fixtures/
```

---

## Task 1: Create skeleton crates

**Files:**
- Create: `ghostframe-e2e/Cargo.toml`
- Create: `ghostframe-e2e/src/lib.rs`
- Create: `ghostframe-bench/Cargo.toml`
- Create: `ghostframe-bench/src/lib.rs`
- Modify: `Cargo.toml` (workspace members)

**Suggested implementer model:** `haiku` (purely mechanical Cargo.toml authoring).

- [ ] **Step 1: Create the e2e crate skeleton.**

Create `ghostframe-e2e/Cargo.toml` with this exact content:

```toml
[package]
name = "ghostframe-e2e"
version = "0.1.0"
edition = "2021"

# Empty library — this crate exists to host integration tests in `tests/`.
# Heavy e2e dependencies (chromiumoxide, testcontainers, axum, tower-http,
# image, image-compare) live here instead of in ghostframe-lib so that
# `cargo test --lib --package ghostframe-lib` doesn't have to compile them.

[lib]
path = "src/lib.rs"

[dependencies]
# Use ghostframe-lib with `test-loss-injection` enabled so the e2e tests'
# GHOSTFRAME_OUTBOUND_LOSS_* / GHOSTFRAME_INBOUND_LOSS_* env vars take effect.
ghostframe-lib = { path = "../ghostframe-lib", features = ["test-loss-injection"] }
ghostframe-test-pattern = { path = "../ghostframe-test-pattern" }

[dev-dependencies]
# Versions copied verbatim from ghostframe-lib/Cargo.toml's [dev-dependencies].
tracing-subscriber = { workspace = true }
testcontainers = "0.27"
chromiumoxide = "0.9"
serde_json = "1"
anyhow = "1"
axum = "0.7"
tower-http = { version = "0.6", features = ["fs"] }
futures = "0.3"
url = "2"
image-compare = "0.4"
image = { version = "0.25", features = ["png"] }
# Async runtime + bytes for the integration tests themselves.
tokio = { workspace = true }
bytes = { workspace = true }
```

Create `ghostframe-e2e/src/lib.rs` with this exact content:

```rust
//! Empty library — this crate exists to host integration tests in `tests/`.
//!
//! Heavy e2e dependencies live in this crate's `[dev-dependencies]` so that
//! the lib's `cargo test --lib` doesn't have to compile them. See
//! `docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md`
//! for the rationale.
```

- [ ] **Step 2: Create the bench crate skeleton.**

Create `ghostframe-bench/Cargo.toml` with this exact content:

```toml
[package]
name = "ghostframe-bench"
version = "0.1.0"
edition = "2021"

# Empty library — this crate exists to host benchmarks in `benches/`.
# criterion and iai-callgrind live here instead of in ghostframe-lib so
# that `cargo test --lib --package ghostframe-lib` doesn't have to compile
# them.

[lib]
path = "src/lib.rs"

[features]
default = []
# Forwarded from ghostframe-lib: enables benches that require real VA-API /
# Vulkan (skipped on CI without GPU). The bench .rs files gate code on this
# feature name, so the crate must redeclare it for the cfg attributes to
# resolve.
gpu-bench = ["ghostframe-lib/gpu-bench"]
# Forwarded from ghostframe-lib: codecs not yet implemented. No-op for now;
# flip on as each codec lands in M3.
m3 = ["ghostframe-lib/m3"]

[dependencies]
ghostframe-lib = { path = "../ghostframe-lib" }
ghostframe-test-pattern = { path = "../ghostframe-test-pattern" }

[dev-dependencies]
# Versions copied verbatim from ghostframe-lib/Cargo.toml.
criterion = { version = "0.5", features = ["html_reports"] }
iai-callgrind = "0.14"

[[bench]]
name = "codec_latency"
harness = false

[[bench]]
name = "pipeline_throughput"
harness = false

[[bench]]
name = "codec_callgrind"
harness = false
```

Create `ghostframe-bench/src/lib.rs` with this exact content:

```rust
//! Empty library — this crate exists to host benchmarks in `benches/`.
//!
//! criterion and iai-callgrind live in this crate's `[dev-dependencies]` so
//! that the lib's `cargo test --lib` doesn't have to compile them. See
//! `docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md`
//! for the rationale.
```

- [ ] **Step 3: Add the new crates to the workspace.**

Read the current workspace `Cargo.toml`:

```bash
cat /home/cedric/work/ghostframe/Cargo.toml | head -10
```

Modify `/home/cedric/work/ghostframe/Cargo.toml` to add the two new members. The current `[workspace]` block looks like:

```toml
[workspace]
resolver = "2"
members = [
    "ghostframe-lib",
    "ghostframe-xdaemon",
    "ghostframe-test-pattern",
]
```

Replace with:

```toml
[workspace]
resolver = "2"
members = [
    "ghostframe-lib",
    "ghostframe-xdaemon",
    "ghostframe-test-pattern",
    "ghostframe-e2e",
    "ghostframe-bench",
]
```

- [ ] **Step 4: Build verification.**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --workspace -j 1 2>&1 | tail -10
```

Expected: clean build, no errors. The two new crates compile to nothing (empty `src/lib.rs`). Pre-existing 2 warnings in `io_bridge.rs` (about `inject_will_fire` and `phase_b_encode_payloads`) are still there but unchanged.

If errors surface they're likely typos in the Cargo.tomls — re-check against the exact content above.

- [ ] **Step 5: Stage and commit.**

```bash
cd /home/cedric/work/ghostframe
git add Cargo.toml \
        ghostframe-e2e/Cargo.toml ghostframe-e2e/src/lib.rs \
        ghostframe-bench/Cargo.toml ghostframe-bench/src/lib.rs
git status --short
```

Expected `git status --short` output:
```
M  Cargo.toml
A  ghostframe-bench/Cargo.toml
A  ghostframe-bench/src/lib.rs
A  ghostframe-e2e/Cargo.toml
A  ghostframe-e2e/src/lib.rs
```

Nothing else (no `.claude/` — that's gitignored, but verify nonetheless).

Commit:

```bash
git commit -m "$(cat <<'EOF'
chore(workspace): add ghostframe-e2e and ghostframe-bench skeletons

Two new workspace members, each with an empty src/lib.rs and a
Cargo.toml carrying the dev-dependencies that integration tests / benches
need. No files moved yet — that lands in the next two commits. The lib's
[dev-dependencies] still contain the heavy deps and will be trimmed once
the file moves are in place.

ghostframe-e2e enables ghostframe-lib's `test-loss-injection` feature so
the e2e tests' GHOSTFRAME_*_LOSS_* env vars still take effect after the
move.

ghostframe-bench forwards `gpu-bench` and `m3` features to ghostframe-lib
because the bench source files use `#[cfg(feature = "gpu-bench")]` and
those need to resolve in the new crate.

Part 1 of 4 of the workspace test-crate split.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Move e2e tests

**Files:**
- `git mv ghostframe-lib/tests/e2e.rs → ghostframe-e2e/tests/e2e.rs`
- `git mv ghostframe-lib/tests/e2e/ → ghostframe-e2e/tests/e2e/`
- `git mv ghostframe-lib/tests/loopback_h3.rs → ghostframe-e2e/tests/loopback_h3.rs`

**Suggested implementer model:** `haiku` (purely mechanical `git mv` operations).

- [ ] **Step 1: Create the destination directory.**

```bash
cd /home/cedric/work/ghostframe
mkdir -p ghostframe-e2e/tests
```

- [ ] **Step 2: Move e2e.rs and its helpers directory.**

```bash
cd /home/cedric/work/ghostframe
git mv ghostframe-lib/tests/e2e.rs ghostframe-e2e/tests/e2e.rs
git mv ghostframe-lib/tests/e2e ghostframe-e2e/tests/e2e
```

Verify both moved:

```bash
ls ghostframe-e2e/tests/    # should show: e2e/  e2e.rs
ls ghostframe-lib/tests/    # should show only: gpu_pipeline.rs  loopback_h3.rs (until next step)
```

- [ ] **Step 3: Move loopback_h3.rs.**

```bash
cd /home/cedric/work/ghostframe
git mv ghostframe-lib/tests/loopback_h3.rs ghostframe-e2e/tests/loopback_h3.rs
```

Verify:

```bash
ls ghostframe-lib/tests/    # should show only: gpu_pipeline.rs
ls ghostframe-e2e/tests/    # should show: e2e/  e2e.rs  loopback_h3.rs
```

- [ ] **Step 4: Compile-check the e2e crate.**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --tests --package ghostframe-e2e -j 1 2>&1 | tail -15
```

Expected: clean build. Common failure modes:

- "cannot find crate `ghostframe_lib`": ghostframe-e2e's `[dependencies]` is wrong; re-check Task 1 Step 1.
- "cannot find function/type `xxx` in crate `ghostframe_lib`": the test uses a `pub(crate)` item from ghostframe-lib. List the missing item, then EITHER promote it to `pub` in ghostframe-lib (separate concern — flag it as DONE_WITH_CONCERNS rather than doing it here) OR confirm with the user that it's safe to promote.
- "missing feature `test-loss-injection`": double-check the dep line in `ghostframe-e2e/Cargo.toml`.
- A dep version mismatch: `ghostframe-lib`'s `[dev-dependencies]` still has all the deps; the new crate has its own copies; they should agree on version, so if you see a version conflict, re-verify the version strings.

- [ ] **Step 5: Verify ghostframe-lib still builds (didn't accidentally orphan a reference).**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --tests --package ghostframe-lib -j 1 2>&1 | tail -10
```

Expected: clean build. (Lib's `[dev-dependencies]` still contain all the e2e deps at this point, so even though e2e.rs is gone, the lib's test compilation unit still compiles — it just no longer has any test that uses those deps. They'll be removed in Task 4.)

- [ ] **Step 6: Stage and commit.**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

Expected output (the `git mv` operations should show as renames; cargo lock may update):
```
R  ghostframe-lib/tests/e2e.rs -> ghostframe-e2e/tests/e2e.rs
R  ghostframe-lib/tests/e2e/golden/... -> ghostframe-e2e/tests/e2e/golden/...
R  ghostframe-lib/tests/e2e/helpers.rs -> ghostframe-e2e/tests/e2e/helpers.rs
R  ghostframe-lib/tests/loopback_h3.rs -> ghostframe-e2e/tests/loopback_h3.rs
 M Cargo.lock
```

If `Cargo.lock` shows changes, that's normal (the dep graph rearranged). Stage it too.

```bash
git add -A ghostframe-lib/tests/ ghostframe-e2e/tests/ Cargo.lock
git status --short
```

Verify nothing in `.claude/` is staged.

Commit:

```bash
git commit -m "$(cat <<'EOF'
chore(workspace): move e2e tests to ghostframe-e2e crate

Move tests/e2e.rs, tests/e2e/{helpers.rs,golden/}, and tests/loopback_h3.rs
out of ghostframe-lib and into the new ghostframe-e2e crate's tests/
directory. No content changes — pure file relocation via `git mv`. The
helpers.rs `mod` references are unchanged because the relative path within
tests/ is preserved.

ghostframe-lib/tests/ now contains only gpu_pipeline.rs (which uses only
the lib itself, no heavy deps).

Part 2 of 4. The lib's [dev-dependencies] still contain the heavy deps at
this point; they get trimmed in part 4 once the bench files are also
moved.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Move benches

**Files:**
- `git mv ghostframe-lib/benches/codec_callgrind.rs → ghostframe-bench/benches/codec_callgrind.rs`
- `git mv ghostframe-lib/benches/codec_latency.rs → ghostframe-bench/benches/codec_latency.rs`
- `git mv ghostframe-lib/benches/pipeline_throughput.rs → ghostframe-bench/benches/pipeline_throughput.rs`
- `git mv ghostframe-lib/benches/fixtures → ghostframe-bench/benches/fixtures`
- `git mv ghostframe-lib/benches/README.md → ghostframe-bench/benches/README.md` (if it exists)

**Suggested implementer model:** `haiku`.

- [ ] **Step 1: Create destination directory.**

```bash
cd /home/cedric/work/ghostframe
mkdir -p ghostframe-bench/benches
```

- [ ] **Step 2: Move all bench files.**

```bash
cd /home/cedric/work/ghostframe
git mv ghostframe-lib/benches/codec_callgrind.rs   ghostframe-bench/benches/codec_callgrind.rs
git mv ghostframe-lib/benches/codec_latency.rs     ghostframe-bench/benches/codec_latency.rs
git mv ghostframe-lib/benches/pipeline_throughput.rs ghostframe-bench/benches/pipeline_throughput.rs
git mv ghostframe-lib/benches/fixtures             ghostframe-bench/benches/fixtures
```

If `ghostframe-lib/benches/README.md` exists (per the earlier `ls`, it does), also move:

```bash
git mv ghostframe-lib/benches/README.md ghostframe-bench/benches/README.md
```

- [ ] **Step 3: Remove the now-empty ghostframe-lib/benches directory.**

```bash
cd /home/cedric/work/ghostframe
rmdir ghostframe-lib/benches 2>&1 || ls ghostframe-lib/benches
```

If `rmdir` fails because the directory is not empty, list what's left and decide whether to `git mv` it too. If it succeeds, no commit-time action needed (git treats the missing directory as expected from the moves).

- [ ] **Step 4: Compile-check the bench crate.**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --benches --package ghostframe-bench -j 1 2>&1 | tail -15
```

Expected: clean build. (Some benches are gated on `gpu-bench` and won't actually exercise much code without that feature; that's fine — the file should still compile.)

If you see `cannot find feature "gpu-bench" in this scope`, double-check that `ghostframe-bench/Cargo.toml` has the `[features]` section forwarding `gpu-bench` to `ghostframe-lib/gpu-bench`.

- [ ] **Step 5: Verify ghostframe-lib still builds.**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --tests --package ghostframe-lib -j 1 2>&1 | tail -10
```

Expected: clean. ghostframe-lib's Cargo.toml still has the `[[bench]]` entries pointing to files that no longer exist; this MAY cause a warning but should NOT fail the build because we're building `--tests`, not `--benches`. If it does fail, the next task (Task 4) will fix it; you can either skip ahead or note the issue.

- [ ] **Step 6: Stage and commit.**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

Expected (rename detection on the .rs files; the fixtures directory and README move too):
```
R  ghostframe-lib/benches/codec_callgrind.rs -> ghostframe-bench/benches/codec_callgrind.rs
R  ghostframe-lib/benches/codec_latency.rs -> ghostframe-bench/benches/codec_latency.rs
R  ghostframe-lib/benches/pipeline_throughput.rs -> ghostframe-bench/benches/pipeline_throughput.rs
R  ghostframe-lib/benches/README.md -> ghostframe-bench/benches/README.md
R  ghostframe-lib/benches/fixtures/... -> ghostframe-bench/benches/fixtures/...
 M Cargo.lock
```

Stage and commit:

```bash
git add -A ghostframe-lib/benches ghostframe-bench/benches Cargo.lock
git status --short

git commit -m "$(cat <<'EOF'
chore(workspace): move benches to ghostframe-bench crate

Move all of ghostframe-lib/benches/ (codec_callgrind.rs, codec_latency.rs,
pipeline_throughput.rs, fixtures/, README.md) into the new ghostframe-bench
crate's benches/ directory. No content changes — pure file relocation.

The `gpu-bench` and `m3` features that bench source files gate on are
forwarded from ghostframe-bench to ghostframe-lib via the [features]
section established in part 1, so the cfg attributes still resolve.

Part 3 of 4. The lib's [dev-dependencies] still contain criterion and
iai-callgrind at this point, plus the [[bench]] entries pointing at the
now-moved files; those get cleaned up in part 4.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Trim ghostframe-lib's `[dev-dependencies]` and `[[bench]]` entries

**Files:**
- Modify: `ghostframe-lib/Cargo.toml`

**Suggested implementer model:** `haiku`.

This is the payoff commit. The lib's compilation graph shrinks.

- [ ] **Step 1: Read current ghostframe-lib/Cargo.toml.**

```bash
cd /home/cedric/work/ghostframe
cat ghostframe-lib/Cargo.toml
```

Confirm the `[dev-dependencies]` section currently lists (this is its state at the start of Task 4):

```
tracing-subscriber, testcontainers, chromiumoxide, serde_json, anyhow, axum,
tower-http, futures, url, proptest, criterion, iai-callgrind, lz4_flex,
image-compare, image, ghostframe-test-pattern
```

…and that there are three `[[bench]]` blocks at the bottom for `codec_latency`, `pipeline_throughput`, `codec_callgrind`.

- [ ] **Step 2: Edit `ghostframe-lib/Cargo.toml` — replace the `[dev-dependencies]` block.**

Replace the current `[dev-dependencies]` block (lines ~41-57 of the current Cargo.toml) with this exact content:

```toml
[dev-dependencies]
tracing-subscriber = { workspace = true }
serde_json = "1"
anyhow = "1"
futures = "0.3"
url = "2"
proptest = "1"
lz4_flex = "0.11"
ghostframe-test-pattern = { path = "../ghostframe-test-pattern" }
```

Removed: `testcontainers`, `chromiumoxide`, `axum`, `tower-http`, `criterion`, `iai-callgrind`, `image-compare`, `image`.

Kept (still used by `ghostframe-lib/src/` `#[cfg(test)]` mods or by `tests/gpu_pipeline.rs`):
- `tracing-subscriber` — used by lib's tracing setup.
- `serde_json` — used by lib unit tests and gpu_pipeline.rs.
- `anyhow` — used widely.
- `futures` — used by tokio tests.
- `url` — used by lib tests.
- `proptest` — used by tile/tests_proptest.rs (in lib src).
- `lz4_flex` — used by lib code.
- `ghostframe-test-pattern` — used by gpu_pipeline.rs integration test.

- [ ] **Step 3: Edit `ghostframe-lib/Cargo.toml` — remove the three `[[bench]]` blocks.**

Delete these three blocks at the bottom of the file:

```toml
[[bench]]
name = "codec_latency"
harness = false

[[bench]]
name = "pipeline_throughput"
harness = false

[[bench]]
name = "codec_callgrind"
harness = false
```

After this edit, the bottom of `ghostframe-lib/Cargo.toml` should end with the last entry of `[dev-dependencies]`.

- [ ] **Step 4: Build verification — ghostframe-lib's test compile is now lighter.**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --tests --package ghostframe-lib -j 1 2>&1 | tail -10
```

Expected: clean build. The compilation graph no longer includes chromiumoxide, testcontainers, axum, tower-http, criterion, iai-callgrind, image, image-compare. Only 2 pre-existing warnings in io_bridge.rs.

If errors surface they're almost certainly:
- "cannot find crate XXX": something in `ghostframe-lib/src/` was actually using one of the removed dev-deps via `#[cfg(test)]`. List the import and re-add that one dep. Likeliest candidates: `image` (if any lib unit test reads PNGs), `lz4_flex` (already kept, but verify).
- "no bench target with name XXX": the `[[bench]]` blocks didn't all delete cleanly. Re-check.

- [ ] **Step 5: Build the rest of the workspace, confirm nothing broke.**

```bash
cd /home/cedric/work/ghostframe
systemd-run --user --scope -p MemoryMax=18G --quiet -- cargo build --workspace -j 1 2>&1 | tail -10
```

Expected: clean workspace build.

- [ ] **Step 6: Stage and commit.**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

Expected:
```
M  ghostframe-lib/Cargo.toml
 M Cargo.lock
```

Stage:

```bash
git add ghostframe-lib/Cargo.toml Cargo.lock
git status --short    # verify only those two
```

Commit:

```bash
git commit -m "$(cat <<'EOF'
chore(ghostframe-lib): drop 8 dev-deps now hosted by ghostframe-e2e and ghostframe-bench

Remove from ghostframe-lib/[dev-dependencies]:
  - chromiumoxide, testcontainers (e2e infrastructure)
  - axum, tower-http (loopback_h3 test infrastructure)
  - criterion, iai-callgrind (bench harnesses)
  - image, image-compare (e2e fixture readback / golden compare)

Remove the three [[bench]] entries (codec_latency, pipeline_throughput,
codec_callgrind) — those bench targets are now hosted by
ghostframe-bench.

ghostframe-lib's [dev-dependencies] is now 8 entries (tracing-subscriber,
serde_json, anyhow, futures, url, proptest, lz4_flex,
ghostframe-test-pattern) — all genuinely needed by lib src `#[cfg(test)]`
mods or by tests/gpu_pipeline.rs.

This is the payoff commit. `cargo build --tests --package ghostframe-lib`
no longer pulls the 8 heavy deps into its compilation unit. Measured peak
RSS impact lands in the next commit.

Part 4 of 4 of the workspace test-crate split.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 5: Measure post-split peak RSS (verification only — no code change)

**Files:** None modified.

**Suggested implementer model:** `haiku`.

- [ ] **Step 1: Re-measure `cargo build --tests --package ghostframe-lib`.**

```bash
cd /home/cedric/work/ghostframe
touch ghostframe-lib/src/lib.rs   # invalidate cache for ghostframe-lib only
python3 /tmp/measure_rss.py -- \
  systemd-run --user --scope -p MemoryMax=18G --quiet -- \
  cargo build --tests --package ghostframe-lib -j 1
```

(`/tmp/measure_rss.py` is the polling script created earlier in the session. If it's been removed, the implementer can recreate it from the spec doc's verification section, or fall back to `ps` watching: `( while sleep 0.5; do ps -e -o rss,comm | awk '/rustc/ {sum+=$1} END {print sum/1024 "MB"}'; done ) &` running in parallel.)

Record the output. Expected: `peak_total_rustc+cargo_MB` is materially less than 894 MB (the pre-split peak). Order-of-magnitude expectation: 400–600 MB.

If the peak is unchanged or higher, something went wrong — investigate before declaring done.

- [ ] **Step 2: Re-measure `cargo build --tests --package ghostframe-e2e`.**

```bash
cd /home/cedric/work/ghostframe
touch ghostframe-e2e/src/lib.rs
python3 /tmp/measure_rss.py -- \
  systemd-run --user --scope -p MemoryMax=18G --quiet -- \
  cargo build --tests --package ghostframe-e2e -j 1
```

Record the output. This number will be HIGHER than the lib's because it pulls all 8 evicted deps. That's expected and fine — what matters is that `cargo build --tests --package ghostframe-lib` (the routine command) no longer bears that cost.

- [ ] **Step 3: Document the result.**

Append a small note to the spec doc to record the actual measurement. Open `/home/cedric/work/ghostframe/docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md` and add a new section after "Done definition":

```markdown
## Post-split measurement (recorded after Task 5)

| Command | Pre-split peak RSS | Post-split peak RSS |
|---|---|---|
| `cargo build --tests --package ghostframe-lib` | 894 MB | <FILL_IN> MB |
| `cargo build --tests --package ghostframe-e2e` | n/a (didn't exist) | <FILL_IN> MB |

The lib's test-compile no longer pulls chromiumoxide, testcontainers, axum,
tower-http, criterion, iai-callgrind, image, image-compare.
```

Fill in `<FILL_IN>` with the actual numbers from Steps 1-2.

- [ ] **Step 4: Stage and commit.**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

Expected: `M  docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md`.

```bash
git add docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md
git commit -m "$(cat <<'EOF'
docs(spec): record post-split peak RSS measurements

After moving e2e tests and benches out of ghostframe-lib, the lib's
cargo build --tests peaks at <FILL_IN> MB (was 894 MB pre-split).
ghostframe-e2e's cargo build --tests peaks at <FILL_IN> MB; this is
isolated to its own rustc invocation so it doesn't penalise routine
lib-only test compilation.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

(Update the `<FILL_IN>` values in the commit message to match the spec doc.)

---

## Done definition

After all 5 tasks land:

- `ghostframe-e2e` and `ghostframe-bench` exist as workspace members.
- `ghostframe-lib/tests/` contains only `gpu_pipeline.rs`.
- `ghostframe-lib/benches/` does not exist.
- `ghostframe-lib/Cargo.toml` `[dev-dependencies]` has exactly 8 entries (tracing-subscriber, serde_json, anyhow, futures, url, proptest, lz4_flex, ghostframe-test-pattern).
- `ghostframe-lib/Cargo.toml` contains no `[[bench]]` entries.
- `cargo build --workspace` passes.
- `cargo build --tests --package ghostframe-lib -j 1` peaks at materially less RSS than 894 MB.
- The spec doc has been updated with the measured peak RSS.

---

## Risks recap (from spec)

1. **Feature flag inheritance.** Task 1 enables `test-loss-injection` on ghostframe-e2e's path dep to ghostframe-lib. If Task 2's build fails because some other feature is needed, add it to the same `features = [...]` list.
2. **`pub(crate)` access.** Integration tests are already a separate compilation unit; moving them to a new crate doesn't change `pub` visibility. If Task 2 surfaces a "function not found" error on a previously-accessible item, that's a sign the item was being accessed via the `tests/` directory's implicit access path that integration tests get within the same package. Promote the item to `pub` (with a `pub(crate)` → `pub` change in ghostframe-lib) OR ask the user.
3. **bench feature flags.** Task 1's `[features]` block on ghostframe-bench forwards `gpu-bench` and `m3` to ghostframe-lib. If Task 3's bench compile fails on a feature flag, add the forwarding here.
4. **`Cargo.lock` churn.** Expected on Tasks 1, 2, 3, 4. Always stage Cargo.lock along with the package edits.
5. **`.claude/` discipline.** `.gitignore` lists it so it shouldn't appear in `git status`, but verify before every commit anyway.

---

## Out of scope (do NOT do these as part of this plan)

- Promoting any `pub(crate)` items in `ghostframe-lib` to `pub` (separate decision; flag and ask if encountered).
- Reorganising test logic or adding new tests.
- Changing the workspace's other members (`ghostframe-xdaemon`, `ghostframe-test-pattern`, `ghostframe-web-client`, `ghostbridge`).
- Adding new workspace-level dependency pins.
- Touching `ghostframe-web-client` or `ghostbridge`.
- Documentation rewrites beyond what the spec note in Task 5 requires.
- Running e2e tests inside Docker — out of scope; merely compiling the e2e crate is sufficient verification for this plan.
