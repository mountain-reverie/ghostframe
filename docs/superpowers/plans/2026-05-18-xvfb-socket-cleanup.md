# Xvfb Stale Socket Cleanup Precondition — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prevent cascading e2e suite failures caused by stale `/tmp/.X11-unix/X<N>` socket accumulation. Probe-then-unlink helper called at the top of `setup_e2e_inner` removes any stale socket in the 99..199 range that no live process is bound to.

**Architecture:** Pure-Rust helper using `std::os::unix::net::UnixStream::connect` as a liveness probe. Single call site in the shared setup-helper body. No new dependencies.

**Tech Stack:** Rust standard library (no crates added).

**Spec:** `docs/superpowers/specs/2026-05-18-xvfb-socket-cleanup-design.md`

---

## File Structure

- `ghostframe-lib/tests/e2e/helpers.rs` — new `pub fn cleanup_stale_xvfb_sockets()` (~25 lines).
- `ghostframe-lib/tests/e2e.rs` — one new line at the top of `setup_e2e_inner` calling the helper.

---

## Task 1: Add `cleanup_stale_xvfb_sockets` helper

**Files:**
- Modify: `ghostframe-lib/tests/e2e/helpers.rs` (append at end of file)

- [ ] **Step 1: Add the helper function at the end of helpers.rs**

Append exactly the following:

```rust
/// Pre-test hygiene: remove stale `/tmp/.X11-unix/X<N>` socket files and
/// matching `/tmp/.X<N>-lock` lock files for `N in 99..=199`, but skip any
/// socket that's currently backed by a live X server (connect probe returns
/// Ok). Without this, accumulated stale sockets from prior test runs cause
/// `spawn_xvfb` to fail with "no free X display number in 99..200" after
/// ~100 test invocations have leaked Xvfb sockets.
///
/// Called once at the top of `setup_e2e_inner` so it runs once per test.
pub fn cleanup_stale_xvfb_sockets() {
    use std::os::unix::net::UnixStream;
    use std::path::Path;
    let mut removed = 0;
    for n in 99..=199u32 {
        let socket = format!("/tmp/.X11-unix/X{n}");
        let lock = format!("/tmp/.X{n}-lock");
        if !Path::new(&socket).exists() && !Path::new(&lock).exists() {
            continue;
        }
        match UnixStream::connect(&socket) {
            Ok(_) => continue, // live X server bound; keep
            Err(e) if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) => {
                let _ = std::fs::remove_file(&socket);
                let _ = std::fs::remove_file(&lock);
                removed += 1;
            }
            Err(_) => continue,
        }
    }
    if removed > 0 {
        eprintln!("cleanup_stale_xvfb_sockets: removed {removed} stale entries");
    }
}
```

- [ ] **Step 2: Verify the test crate compiles**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds, no errors. (If you see "unused import" warnings from the `UnixStream` use being scoped to the function body, that's fine — Rust accepts that.)

- [ ] **Step 3: Smoke-test the helper directly**

Create 3 fake stale entries (no live X server behind them), then invoke the helper from a quick scratch test:

```bash
touch /tmp/.X11-unix/X100 /tmp/.X11-unix/X101 /tmp/.X100-lock
ls /tmp/.X11-unix/ | grep -E '^X(100|101)$' | wc -l
# Expected: 2
```

Then run any existing e2e test that uses `setup_e2e_inner`. The helper runs and removes them. Verify after:

```bash
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_solid_color -- --test-threads=1 2>&1 | tee /tmp/cleanup_smoke.log | tail -5
grep "cleanup_stale_xvfb_sockets: removed" /tmp/cleanup_smoke.log || echo "WARNING: cleanup didn't run yet — Task 2 wires the call site"
ls /tmp/.X11-unix/ | grep -E '^X(100|101)$' | wc -l
```

Expected before Task 2: the cleanup line does NOT appear (helper not yet called), and the fake files persist. This Step 3 is just sanity-validating that we set up the right preconditions; the actual cleanup verification happens after Task 2.

If you want to validate Task 1 in isolation (without Task 2's wiring), you can call the helper from a one-off Rust binary or invoke it via `cargo test` after writing a temporary test that calls it. But the easier path is to verify it as part of Task 2 below.

Clean up the fake sockets so they don't leak into Task 2:
```bash
rm -f /tmp/.X11-unix/X100 /tmp/.X11-unix/X101 /tmp/.X100-lock
```

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e/helpers.rs
git commit -m "$(cat <<'EOF'
feat(e2e-helpers): cleanup_stale_xvfb_sockets probe-then-unlink helper

Iterates display numbers 99..=199. For each: probes
`/tmp/.X11-unix/X<N>` via UnixStream::connect. Live server (Ok) → skip.
Stale (ECONNREFUSED) or missing (ENOENT) → unlink the socket and the
matching lock file. Unknown errors → conservatively skip. Single
eprintln when ≥1 entry removed.

Task 2 wires the call site at setup_e2e_inner.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Wire the helper into `setup_e2e_inner`

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs` (top of `setup_e2e_inner`)

- [ ] **Step 1: Locate `setup_e2e_inner`**

```bash
grep -n "async fn setup_e2e_inner" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

Note the line number (around line 247 as of commit `33247f8`).

- [ ] **Step 2: Read the function signature + first few lines**

```bash
sed -n "247,255p" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

Confirm the signature is:
```rust
async fn setup_e2e_inner(
    test_pattern_args: &str,
    extra_env: &[(&str, &str)],
    gpu: bool,
    webgpu: bool,
) -> Result<E2eSetup> {
    let hs_server_url = format!(...);
    ...
```

- [ ] **Step 3: Insert the cleanup call as the very first line of the body**

Add one line immediately after the opening `{` of `setup_e2e_inner`:

```rust
async fn setup_e2e_inner(
    test_pattern_args: &str,
    extra_env: &[(&str, &str)],
    gpu: bool,
    webgpu: bool,
) -> Result<E2eSetup> {
    helpers::cleanup_stale_xvfb_sockets();
    let hs_server_url = format!(...);
    ...
```

- [ ] **Step 4: Verify the test crate compiles**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds.

- [ ] **Step 5: Smoke-test end-to-end**

Seed fake stale sockets, run a single test, confirm both the cleanup eprintln fires AND the test passes:

```bash
touch /tmp/.X11-unix/X100 /tmp/.X11-unix/X101 /tmp/.X11-unix/X102 /tmp/.X100-lock
ls /tmp/.X11-unix/ | grep -cE '^X1(00|01|02)$'  # expect: 3
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_solid_color -- --test-threads=1 --nocapture 2>&1 | tee /tmp/cleanup_smoke.log | tail -10
grep "cleanup_stale_xvfb_sockets: removed" /tmp/cleanup_smoke.log
ls /tmp/.X11-unix/ | grep -cE '^X1(00|01|02)$'  # expect: 0
```

Expected:
- Log contains `cleanup_stale_xvfb_sockets: removed 3 stale entries`
- The 3 fake sockets are gone
- `e2e_solid_color` passes

If the cleanup count is less than 3, one of the fake sockets unexpectedly probed as live — investigate (likely Xorg running on the host has bound to one of those). The helper should skip live ones; that's the correct behavior.

- [ ] **Step 6: Live-server safety test (manual)**

Start a real Xvfb on display :150, verify the helper does NOT remove it:

```bash
which Xvfb && Xvfb :150 &
XVFB_PID=$!
sleep 1
ls /tmp/.X11-unix/X150
# Expected: file exists, owned by Xvfb's user
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_solid_color -- --test-threads=1 --nocapture 2>&1 | grep cleanup_stale_xvfb_sockets
# Expected: either no log line at all, OR a log line with a count that does NOT remove X150
ls /tmp/.X11-unix/X150
# Expected: file STILL exists
kill $XVFB_PID; rm -f /tmp/.X150-lock /tmp/.X11-unix/X150
```

If Xvfb isn't installed, skip this step and document it; the probe semantics are still correct.

- [ ] **Step 7: Run the full e2e suite to confirm no regressions**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/cleanup_full.log | tail -5
```

Expected: same count as pre-fix (22 passed, 0 failed, 3 ignored as of commit `f8019a0`). If any regression, the cleanup is touching something it shouldn't — investigate `cleanup_smoke.log` for unexpected removals.

- [ ] **Step 8: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "$(cat <<'EOF'
feat(e2e): call cleanup_stale_xvfb_sockets at setup_e2e_inner start

Eliminates the cascading "no free X display number in 99..200" failure
mode when /tmp/.X11-unix/ has accumulated stale Xvfb sockets from
prior test runs (observed during M3.2c regression on 2026-05-18).
Live X servers in the same range are skipped (probe returns Ok).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Notes

The plan has no contingency branch — the helper's behavior is fully deterministic and tested by Steps 5-6. If the smoke test surfaces unexpected behavior, fix the helper inline (it's <30 lines) and re-run.

Subagent-driven execution: dispatch each Task as one subagent invocation. Tasks are sequential — Task 2 depends on Task 1's helper existing.
