# Xvfb Stale Socket Cleanup Precondition — Design

**Date**: 2026-05-18
**Milestone**: e2e infrastructure (host-state hygiene)
**Predecessor**: discovered during M3.2c W5 closure verification — see `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md` "Host-state gotcha" note.

## Background

During the M3.2c near-complete suite run on 2026-05-18, the full e2e suite hit a cascading failure: 18 of 22 tests failed with `"no free X display number in 99..200"`. Root cause: `/tmp/.X11-unix/` had accumulated 102 stale Xvfb socket files (X99..X199 all present) from many test iterations earlier in the day. Each test calls `helpers::spawn_xvfb` → `find_free_display` which iterates 99..199 looking for a socket-free display number. With all numbers taken, the function errors and the test panics during setup.

The host's `XvfbGuard::Drop` impl kills the Xvfb child and removes the auth file, but does not remove the `/tmp/.X11-unix/X<N>` socket. The socket usually disappears when the X server exits cleanly, but if it crashes or is SIGKILLed (e.g., by an aggressive cargo test rerun, by docker cleanup mid-Xvfb-startup, or by the user interrupting), the socket lingers.

Manual mitigation today: `for n in $(seq 99 200); do rm -f /tmp/.X11-unix/X$n /tmp/.X$n-lock; done`. Should be automatic.

## Goal

Prevent test suite failures caused by accumulated stale Xvfb socket files in `/tmp/.X11-unix/`, without risking removal of a socket a live X server is bound to.

## Approach

New helper `cleanup_stale_xvfb_sockets()` in `ghostframe-lib/tests/e2e/helpers.rs`. Called once at the top of `setup_e2e_inner` (the shared body of all `setup_e2e_*` variants), before `spawn_xvfb`. Uses a probe-then-unlink strategy:

For each `N` in `99..=199`:
1. Skip if neither `/tmp/.X11-unix/X<N>` nor `/tmp/.X<N>-lock` exists.
2. Attempt `UnixStream::connect("/tmp/.X11-unix/X<N>")`.
3. If `Ok(_)`: live X server bound. Drop the connection and continue without touching the file.
4. If `Err(ConnectionRefused)` or `Err(NotFound)`: stale socket (file exists but no listener). Remove the socket file AND the matching lock file.
5. If other `Err(_)`: conservatively skip (don't risk).

The connect probe is the canonical Linux test for "is this Unix socket file backed by a listening process?". If a process holds the socket open via `bind() + listen()`, `connect()` succeeds. If the file exists but no process is listening (the stale case), `connect()` returns `ECONNREFUSED`. There is no race window long enough to matter for test infrastructure — a fresh Xvfb takes >1 ms to bind, well after our probe-then-unlink completes.

## Implementation

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

Call site at the top of `setup_e2e_inner` (around line 247 of e2e.rs):
```rust
async fn setup_e2e_inner(
    test_pattern_args: &str,
    extra_env: &[(&str, &str)],
    gpu: bool,
    webgpu: bool,
) -> Result<E2eSetup> {
    helpers::cleanup_stale_xvfb_sockets();
    // ... existing body
}
```

## Files Touched

- `ghostframe-lib/tests/e2e/helpers.rs` — new `pub fn cleanup_stale_xvfb_sockets()` (~25 lines).
- `ghostframe-lib/tests/e2e.rs` — one call at the top of `setup_e2e_inner` (~1 line).

## Testing

- **Manual smoke test**: create 5 fake stale sockets (`touch /tmp/.X11-unix/X{100..104}`), run any single e2e test, verify `cleanup_stale_xvfb_sockets: removed 5 stale entries` appears in test output and the test proceeds normally.
- **Live-server safety test**: while an Xvfb is running on `:99` (e.g., `Xvfb :99 &`), verify the helper does NOT remove `/tmp/.X11-unix/X99` (probe returns `Ok`). Tear down Xvfb with `kill $! && rm /tmp/.X99-lock` afterward.
- **Suite-level**: regression confirmed by running the full e2e suite after a previously-stale state. Pre-fix: 18 failures. Post-fix: 0 failures expected.

No unit test in `mod tests` — the function's behavior is filesystem-side-effect and is best validated by the suite-level regression. A unit test would either need to fake out the FS (complexity, mock-heavy) or actually create real sockets in /tmp (test-environment pollution).

## Out of Scope

- Display 0 cleanup. The host's main X session uses :0; never touch.
- Display 1..98 cleanup. Outside the test range.
- Locking / concurrent-test hardening. `--test-threads=1` is the project convention.
- Auto-cleanup of `/tmp/ghostframe-*` test-pattern directories. Separate concern, not blocking.
- A systemd / cron-style scheduled global cleanup. The on-test-startup approach is sufficient.

## Risk

- **Race between probe and unlink**: theoretically a third party could `bind()` to a probed-stale socket in the microsecond between our connect-failed and remove_file calls. In practice: impossible — fresh Xvfb takes >1 ms to bind, and the test suite runs `--test-threads=1`. Not worth defending against.
- **EACCES on remove_file**: a socket owned by another user that we can't unlink. The `let _ = ` discard silently ignores. Test proceeds; subsequent `spawn_xvfb` may still fail if all 99..199 are occupied by other users' sockets — but that's a multi-user-test-host scenario we don't support.
- **Probe latency on a hung X server**: `connect()` to a half-broken X server could block momentarily. The std `UnixStream::connect` is synchronous with no timeout. In practice: real X servers either accept immediately or refuse immediately; a hung state would be a rare host pathology. If observed, switch to `connect_timeout(100ms)` (requires `std::os::unix::net::UnixStream::connect_addr` + non-blocking setup, more code).

## Pointers

- Plan to be created next via writing-plans skill.
- Memory cross-reference: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md` "Host-state gotcha" note.
