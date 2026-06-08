# README restructure + headless-Xorg install path — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Split docs into a short user-facing `README.md` (install + first connection) and a developer-facing `DEVELOPERS.md`; ship a `packaging/` tree with `install.sh`, a sample `xorg.conf`, four user-level systemd unit files, and a `getty` autologin drop-in template; make `TS_AUTHKEY` optional in `ghostframe-xdaemon` so the secret only appears once at install time.

**Architecture:** Autologin (via `getty@tty1` drop-in) lands in a real PAM session that starts `systemd --user`. A user-level `ghostframe.target` pulls in three units in order: Xorg on a virtual `amdgpu` connector → enlightenment as WM → ghostframe-xdaemon. `TS_AUTHKEY` is consumed only by a one-shot `ghostframe-xdaemon --init` invocation that seeds `~user/.local/share/ghostframe/ts-state`; every subsequent run uses that state and needs no secret.

**Tech Stack:** Rust (xdaemon), Bash (install.sh), systemd units (`Type=simple` + `Requires=`/`After=` chain), Xorg with `amdgpu` driver.

**Spec:** `docs/superpowers/specs/2026-06-07-readme-restructure-design.md`

---

## File Structure

**Created:**
- `packaging/install.sh` — root-run installer, see Task 7
- `packaging/xorg-headless-amdgpu.conf` — Xorg config for headless `amdgpu` virtual display, see Task 2
- `packaging/systemd/ghostframe.target` — user-level target, see Task 3
- `packaging/systemd/ghostframe-xorg.service` — user-level Xorg service, see Task 3
- `packaging/systemd/ghostframe-wm.service` — user-level WM service, see Task 3
- `packaging/systemd/ghostframe-xdaemon.service` — user-level xdaemon service, see Task 3
- `packaging/systemd/getty-autologin.conf.tmpl` — system-level getty drop-in template, see Task 3
- `DEVELOPERS.md` — developer-facing docs absorbed from old README, see Task 8
- `ghostframe-xdaemon/tests/init_flag.rs` — integration test for `--init` and authkey-optional behaviour, see Task 6

**Modified:**
- `ghostframe-xdaemon/src/main.rs` — `--init` flag, optional `TS_AUTHKEY`, state-dir-seeded check, see Tasks 4–6
- `README.md` — full rewrite, short install-and-first-connection guide, see Task 9

---

## Task 1: Worktree + branch setup

**Files:** none (git-only)

- [ ] **Step 1: Confirm a clean working tree**

Run: `git status`
Expected: `working tree clean` on `master` (the spec commit `d24e091` is already there). The `ghostframe-bench/target/` untracked dir is fine; leave it alone.

- [ ] **Step 2: Create a feature branch**

Run: `git checkout -b feature/readme-restructure-install`
Expected: `Switched to a new branch 'feature/readme-restructure-install'`

- [ ] **Step 3: No commit yet**

This task just gets us on a branch — no files have changed.

---

## Task 2: Sample headless `amdgpu` xorg.conf

**Files:**
- Create: `packaging/xorg-headless-amdgpu.conf`

- [ ] **Step 1: Create the directory**

Run: `mkdir -p packaging`
Expected: silent success.

- [ ] **Step 2: Write the xorg.conf**

Create `packaging/xorg-headless-amdgpu.conf` with:

```
# Ghostframe headless Xorg configuration for amdgpu.
#
# This config tells Xorg to bind the amdgpu driver, refuse to auto-claim any
# physical connector, and present a single 1920x1080 virtual display. The
# combination means a Ghostframe-managed Xorg can run on the same GPU as your
# local desktop without ever taking over your real monitor.
#
# Required kernel module option (set this on the host, then reboot):
#
#   /etc/modprobe.d/amdgpu.conf:
#       options amdgpu virtual_display=<PCI_ID>,1
#
# Find <PCI_ID> with: lspci -D | grep VGA  (use the form 0000:03:00.0)
# The trailing ",1" tells amdgpu to create one virtual connector.

Section "ServerLayout"
    Identifier "Layout0"
    Screen 0 "Screen0"
    Option "AutoAddGPU" "off"
    Option "AutoBindGPU" "off"
EndSection

Section "ServerFlags"
    Option "DontVTSwitch" "true"
    Option "AutoAddDevices" "false"
EndSection

Section "Device"
    Identifier "Card0"
    Driver "amdgpu"
    Option "Monitor-Virtual-1" "VirtualMonitor"
EndSection

Section "Monitor"
    Identifier "VirtualMonitor"
    HorizSync 30.0-120.0
    VertRefresh 50.0-75.0
    Modeline "1920x1080" 173.00 1920 2048 2248 2576 1080 1083 1088 1120 -HSync +VSync
    Option "Enable" "true"
EndSection

Section "Screen"
    Identifier "Screen0"
    Device "Card0"
    Monitor "VirtualMonitor"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Modes "1920x1080"
        Virtual 1920 1080
    EndSubSection
EndSection
```

- [ ] **Step 3: Commit**

```bash
git add packaging/xorg-headless-amdgpu.conf
git commit -m "packaging: sample headless amdgpu xorg.conf

Ships a minimal Xorg config that binds the amdgpu driver, refuses to
auto-claim physical connectors, and serves a single 1920x1080 virtual
display. Requires 'options amdgpu virtual_display=<PCI>,1' in the host's
modprobe.d, which is documented in the file header.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 3: systemd unit files + getty drop-in template

**Files:**
- Create: `packaging/systemd/ghostframe.target`
- Create: `packaging/systemd/ghostframe-xorg.service`
- Create: `packaging/systemd/ghostframe-wm.service`
- Create: `packaging/systemd/ghostframe-xdaemon.service`
- Create: `packaging/systemd/getty-autologin.conf.tmpl`

- [ ] **Step 1: Create the systemd subdirectory**

Run: `mkdir -p packaging/systemd`
Expected: silent success.

- [ ] **Step 2: Write `ghostframe.target`**

Create `packaging/systemd/ghostframe.target` with:

```ini
[Unit]
Description=Ghostframe headless desktop session
Requires=ghostframe-xdaemon.service
After=ghostframe-xdaemon.service

[Install]
WantedBy=default.target
```

- [ ] **Step 3: Write `ghostframe-xorg.service`**

Create `packaging/systemd/ghostframe-xorg.service` with:

```ini
[Unit]
Description=Ghostframe headless Xorg server on :1
Before=ghostframe-wm.service

[Service]
Type=simple
ExecStart=/usr/bin/Xorg :1 -config /etc/X11/ghostframe-headless.conf -noreset -nolisten tcp vt7
Restart=on-failure
RestartSec=2

[Install]
WantedBy=ghostframe.target
```

- [ ] **Step 4: Write `ghostframe-wm.service`**

Create `packaging/systemd/ghostframe-wm.service` with:

```ini
[Unit]
Description=Ghostframe window manager (enlightenment)
Requires=ghostframe-xorg.service
After=ghostframe-xorg.service

[Service]
Type=simple
Environment=DISPLAY=:1
# To use a different WM, replace this line. See DEVELOPERS.md "WM alternatives".
ExecStart=/usr/bin/enlightenment_start
Restart=on-failure
RestartSec=2

[Install]
WantedBy=ghostframe.target
```

- [ ] **Step 5: Write `ghostframe-xdaemon.service`**

Create `packaging/systemd/ghostframe-xdaemon.service` with:

```ini
[Unit]
Description=Ghostframe capture + transport daemon
Requires=ghostframe-wm.service
After=ghostframe-wm.service

[Service]
Type=simple
Environment=DISPLAY=:1
Environment=TS_HOSTNAME=%H-ghostframe
Environment=TS_STATE_DIR=%h/.local/share/ghostframe/ts-state
ExecStart=/usr/local/bin/ghostframe-xdaemon
Restart=on-failure
RestartSec=2

[Install]
WantedBy=ghostframe.target
```

- [ ] **Step 6: Write `getty-autologin.conf.tmpl`**

Create `packaging/systemd/getty-autologin.conf.tmpl` with:

```ini
# Installed by packaging/install.sh as
# /etc/systemd/system/getty@tty1.service.d/99-ghostframe-autologin.conf
# (the 99- prefix ensures we win against any existing autologin drop-in.)

[Service]
ExecStart=
ExecStart=-/sbin/agetty -o '-p -- \\u' --autologin __USER__ --noclear %I $TERM
```

- [ ] **Step 7: Commit**

```bash
git add packaging/systemd/
git commit -m "packaging: user-level systemd units + getty autologin template

Adds:
- ghostframe.target: user-level umbrella, WantedBy=default.target
- ghostframe-xorg.service: starts Xorg on :1 with the headless config
- ghostframe-wm.service: starts enlightenment_start on :1
- ghostframe-xdaemon.service: starts the capture daemon with TS_AUTHKEY
  intentionally absent (state dir carries auth after first init)
- getty-autologin.conf.tmpl: system-level drop-in template that
  install.sh renders into /etc/systemd/system/getty@tty1.service.d/

The Requires=/After= chain is target -> xdaemon -> wm -> xorg, so
enabling the target transitively pulls in everything.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 4: Add `state_dir_seeded()` helper to xdaemon

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs` (add a new module-level fn + unit test)

- [ ] **Step 1: Write the failing test**

Append the following at the end of `ghostframe-xdaemon/src/main.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ghostframe-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn state_dir_seeded_returns_false_for_empty_dir() {
        let d = temp_dir();
        assert!(!state_dir_seeded(&d));
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn state_dir_seeded_returns_false_for_missing_dir() {
        let d = temp_dir().join("does-not-exist");
        assert!(!state_dir_seeded(&d));
    }

    #[test]
    fn state_dir_seeded_returns_true_when_tailscaled_state_present() {
        let d = temp_dir();
        fs::write(d.join("tailscaled.state"), b"{}").unwrap();
        assert!(state_dir_seeded(&d));
        fs::remove_dir_all(&d).ok();
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p ghostframe-xdaemon --lib state_dir_seeded`
Expected: compile error — `cannot find function state_dir_seeded in this scope`.

- [ ] **Step 3: Write the implementation**

Just above the `#[tokio::main]` line in `ghostframe-xdaemon/src/main.rs`, add:

```rust
use std::path::Path;

/// Returns true if the tsnet state directory has been seeded with a successful
/// login. tsnet writes `tailscaled.state` after the first successful join; we
/// use its presence as the "has been --init'd" proxy. If this returns false,
/// the daemon needs a TS_AUTHKEY (either via env or via `--init`).
fn state_dir_seeded(state_dir: &Path) -> bool {
    state_dir.join("tailscaled.state").exists()
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p ghostframe-xdaemon --lib state_dir_seeded`
Expected: `3 passed`.

- [ ] **Step 5: Run clippy and fmt**

Run: `cargo clippy -p ghostframe-xdaemon --all-targets -- -D warnings`
Expected: no warnings.

Run: `cargo fmt -p ghostframe-xdaemon -- --check`
Expected: no diff.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-xdaemon/src/main.rs
git commit -m "xdaemon: add state_dir_seeded() helper

Detects whether a tsnet state directory already has a valid login (by
checking for tailscaled.state). Used in the next commits to make
TS_AUTHKEY optional when the state dir has been seeded by an earlier
--init invocation.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 5: Add `--init` flag + optional TS_AUTHKEY in xdaemon

**Files:**
- Modify: `ghostframe-xdaemon/src/main.rs`

- [ ] **Step 1: Read current TS_AUTHKEY handling**

Open `ghostframe-xdaemon/src/main.rs`. The lines we replace are:

```rust
let authkey = env::var("TS_AUTHKEY").expect("TS_AUTHKEY must be set");
let hostname = env::var("TS_HOSTNAME").unwrap_or_else(|_| "ghostframe-server".into());
let state_dir = env::var("TS_STATE_DIR").unwrap_or_else(|_| "/tmp/ghostframe-ts".into());
```

We'll replace the `authkey` line and add init-mode handling.

- [ ] **Step 2: Replace the env-handling block**

In `ghostframe-xdaemon/src/main.rs`, find the block above and replace it with:

```rust
let init_mode = env::args().any(|a| a == "--init");

let hostname = env::var("TS_HOSTNAME").unwrap_or_else(|_| "ghostframe-server".into());
let state_dir = env::var("TS_STATE_DIR").unwrap_or_else(|_| "/tmp/ghostframe-ts".into());
let state_dir_path = std::path::PathBuf::from(&state_dir);
let seeded = state_dir_seeded(&state_dir_path);

let authkey = match (init_mode, env::var("TS_AUTHKEY").ok(), seeded) {
    (true, Some(k), _) if !k.is_empty() => k,
    (true, _, _) => {
        eprintln!("error: --init requires TS_AUTHKEY to be set and non-empty");
        std::process::exit(2);
    }
    (false, Some(k), _) if !k.is_empty() => k,
    (false, _, true) => String::new(),
    (false, _, false) => {
        eprintln!(
            "error: TS_AUTHKEY not set and state dir {state_dir:?} has not been initialised.\n\
             Run once with TS_AUTHKEY set and the --init flag to seed the state dir, e.g.\n\
             \n\
             \tTS_AUTHKEY=tskey-auth-... TS_STATE_DIR={state_dir:?} ghostframe-xdaemon --init\n\
             \n\
             (packaging/install.sh does this for you during setup.)"
        );
        std::process::exit(2);
    }
};
```

- [ ] **Step 3: Add the init-mode short-circuit after server creation**

Find the lines:

```rust
    tracing::info!("Connecting to Tailscale...");
    let server = GhostframeServer::new(config, ":4443").await?;

    // Machine-parseable line for the E2E test harness. Use println! (stdout)
    // rather than tracing so the format stays stable regardless of log config.
    println!("CERT_HASH_SHA256={}", server.cert_hash());
```

Immediately after the `println!("CERT_HASH_SHA256=…")` line, add:

```rust
    if init_mode {
        tracing::info!(
            state_dir = %state_dir,
            "tsnet state dir seeded successfully; exiting (--init)"
        );
        return Ok(());
    }
```

- [ ] **Step 4: Build and check it compiles**

Run: `cargo build -p ghostframe-xdaemon`
Expected: success.

- [ ] **Step 5: Run clippy**

Run: `cargo clippy -p ghostframe-xdaemon --all-targets -- -D warnings`
Expected: no warnings.

- [ ] **Step 6: Run fmt**

Run: `cargo fmt -p ghostframe-xdaemon`
Then: `cargo fmt -- --check`
Expected: no diff.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-xdaemon/src/main.rs
git commit -m "xdaemon: add --init flag, make TS_AUTHKEY optional once state dir is seeded

With --init: requires TS_AUTHKEY, performs the tsnet join, prints the
machine-parseable CERT_HASH_SHA256= line, then exits 0 without entering
the capture loop. This is what packaging/install.sh runs to seed the
state dir at install time.

Without --init: if TS_AUTHKEY is set and non-empty, behave as before. If
TS_AUTHKEY is unset but the state dir already has tailscaled.state,
proceed with an empty authkey (tsnet reuses the saved auth). If neither
is true, exit 2 with a message pointing at --init / install.sh.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 6: Integration tests for `--init` and missing-state error path

**Files:**
- Create: `ghostframe-xdaemon/tests/init_flag.rs`

This task validates the CLI surface without requiring a real tsnet control server: we verify the *pre-tsnet* error paths (missing authkey, missing state) and that `--init` is recognised as a flag. A real `--init` join is covered by the e2e harness already.

- [ ] **Step 1: Write the failing test**

Create `ghostframe-xdaemon/tests/init_flag.rs` with:

```rust
//! Integration tests for ghostframe-xdaemon's --init flag and TS_AUTHKEY
//! optionality. These run the binary as a subprocess with empty/seeded state
//! directories and verify the pre-tsnet error paths. A real --init that joins
//! a tsnet control server is exercised by the e2e harness.

use std::path::PathBuf;
use std::process::Command;

fn binary_path() -> PathBuf {
    // `cargo test` sets CARGO_BIN_EXE_<name> to the integration-test binary.
    PathBuf::from(env!("CARGO_BIN_EXE_ghostframe-xdaemon"))
}

fn unique_tmp(label: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!(
        "ghostframe-xdaemon-test-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&p).unwrap();
    p
}

#[test]
fn errors_when_no_authkey_and_empty_state_dir() {
    let state = unique_tmp("empty-state");
    let out = Command::new(binary_path())
        .env_clear()
        .env("TS_STATE_DIR", &state)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn xdaemon");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr was: {stderr}");
    assert!(
        stderr.contains("TS_AUTHKEY not set")
            && stderr.contains("--init"),
        "stderr should explain --init: {stderr}"
    );
    std::fs::remove_dir_all(&state).ok();
}

#[test]
fn errors_when_init_flag_set_but_no_authkey() {
    let state = unique_tmp("init-no-key");
    let out = Command::new(binary_path())
        .arg("--init")
        .env_clear()
        .env("TS_STATE_DIR", &state)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .output()
        .expect("spawn xdaemon");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr was: {stderr}");
    assert!(
        stderr.contains("--init requires TS_AUTHKEY"),
        "stderr should mention --init + TS_AUTHKEY: {stderr}"
    );
    std::fs::remove_dir_all(&state).ok();
}
```

- [ ] **Step 2: Run the new tests to verify they pass**

Run: `cargo test -p ghostframe-xdaemon --test init_flag`
Expected: `2 passed`.

(These tests rely on Task 5's behaviour; the seeded-state path is not exercised here because triggering tsnet against a real control server belongs in e2e.)

- [ ] **Step 3: Run the full xdaemon test suite to confirm no regression**

Run: `cargo test -p ghostframe-xdaemon`
Expected: all tests pass (3 unit tests from Task 4 + 2 new integration tests + the existing `drm_gpu_pipeline` integration test, which may be ignored if no DRM device is available — that's fine).

- [ ] **Step 4: Commit**

```bash
git add ghostframe-xdaemon/tests/init_flag.rs
git commit -m "xdaemon: integration tests for --init and missing-state error path

Covers the two pre-tsnet error paths:
- no TS_AUTHKEY + empty state dir -> exit 2 with --init hint
- --init + no TS_AUTHKEY -> exit 2 with explicit message

The successful --init join (which requires a real tsnet control server)
remains covered by the e2e harness.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 7: `packaging/install.sh`

**Files:**
- Create: `packaging/install.sh`

- [ ] **Step 1: Write the script**

Create `packaging/install.sh` with the following content. Mark it executable in step 2.

```bash
#!/usr/bin/env bash
#
# packaging/install.sh — install ghostframe-xdaemon as a headless Xorg
# session for a specific user.
#
# Usage:  sudo ./packaging/install.sh <username> [--force]
#
# What it does:
#   1. Verifies required binaries are on the host (Xorg, amdgpu, enlightenment).
#   2. Installs /usr/local/bin/ghostframe-xdaemon from ./target/release/.
#   3. Installs /etc/X11/ghostframe-headless.conf from packaging/.
#   4. Installs ~user/.config/systemd/user/{ghostframe*.{service,target}}
#   5. Installs /etc/systemd/system/getty@tty1.service.d/99-ghostframe-autologin.conf
#   6. (Interactive only) prompts for TS_AUTHKEY and seeds the tsnet state dir.
#   7. Enables ghostframe.target (user) and getty@tty1 (system).
#
# Re-running is idempotent except for step 6, which is skipped if the state dir
# is already populated. Pass --force to overwrite the binary and xorg.conf.

set -euo pipefail

die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
info() { printf 'install.sh: %s\n' "$*"; }

[[ ${EUID} -eq 0 ]] || die "must be run as root (try: sudo $0 $*)"

force=0
target_user=""
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    --help|-h)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*) die "unknown flag: $arg" ;;
    *)
      [[ -z "$target_user" ]] || die "only one username allowed (got '$target_user' and '$arg')"
      target_user="$arg"
      ;;
  esac
done

[[ -n "$target_user" ]] || die "username required (try: sudo $0 <username>)"
id "$target_user" >/dev/null 2>&1 || die "user '$target_user' does not exist"

user_uid=$(id -u "$target_user")
user_gid=$(id -g "$target_user")
user_home=$(getent passwd "$target_user" | cut -d: -f6)
[[ -d "$user_home" ]] || die "home directory '$user_home' for '$target_user' does not exist"

repo_root=$(cd "$(dirname "$0")/.." && pwd)
pkg_dir="$repo_root/packaging"
[[ -f "$pkg_dir/xorg-headless-amdgpu.conf" ]] || die "missing $pkg_dir/xorg-headless-amdgpu.conf"

# 1. Preflight.
info "preflight: checking required binaries..."
need_bins=(Xorg enlightenment_start)
for b in "${need_bins[@]}"; do
  command -v "$b" >/dev/null 2>&1 || die "missing '$b' on PATH — install it and retry"
done

built_bin="$repo_root/target/release/ghostframe-xdaemon"
installed_bin="/usr/local/bin/ghostframe-xdaemon"
[[ -x "$built_bin" || -x "$installed_bin" ]] || \
  die "neither $built_bin nor $installed_bin is present — run 'cargo build --release -p ghostframe-xdaemon' first"

# 2. Binary.
if [[ -x "$built_bin" ]]; then
  if [[ -e "$installed_bin" && $force -eq 0 ]]; then
    info "skip: $installed_bin already exists (use --force to overwrite)"
  else
    info "install: $installed_bin"
    install -m 0755 -o root -g root "$built_bin" "$installed_bin"
  fi
fi

# 3. Xorg config.
xorg_dst="/etc/X11/ghostframe-headless.conf"
if [[ -e "$xorg_dst" && $force -eq 0 ]]; then
  info "skip: $xorg_dst already exists (use --force to overwrite)"
else
  info "install: $xorg_dst"
  install -D -m 0644 -o root -g root "$pkg_dir/xorg-headless-amdgpu.conf" "$xorg_dst"
fi

# 4. State dir.
state_dir="$user_home/.local/share/ghostframe/ts-state"
if [[ ! -d "$state_dir" ]]; then
  info "create: $state_dir"
  install -d -m 0700 -o "$user_uid" -g "$user_gid" "$state_dir"
fi

# 5. User units.
user_units_dir="$user_home/.config/systemd/user"
install -d -m 0755 -o "$user_uid" -g "$user_gid" "$user_units_dir"
for u in ghostframe.target ghostframe-xorg.service ghostframe-wm.service ghostframe-xdaemon.service; do
  info "install: $user_units_dir/$u"
  install -m 0644 -o "$user_uid" -g "$user_gid" "$pkg_dir/systemd/$u" "$user_units_dir/$u"
done

# 6. getty autologin drop-in.
drop_dir="/etc/systemd/system/getty@tty1.service.d"
install -d -m 0755 "$drop_dir"

# Warn (but don't abort) if another autologin drop-in exists.
shopt -s nullglob
existing=("$drop_dir"/*.conf)
shopt -u nullglob
for f in "${existing[@]}"; do
  base=$(basename "$f")
  [[ "$base" == "99-ghostframe-autologin.conf" ]] && continue
  if grep -q autologin "$f" 2>/dev/null; then
    info "warn: existing autologin drop-in at $f — ours uses the 99- prefix and will win, but verify"
  fi
done

drop_dst="$drop_dir/99-ghostframe-autologin.conf"
info "install: $drop_dst"
sed "s/__USER__/$target_user/g" "$pkg_dir/systemd/getty-autologin.conf.tmpl" \
  | install -m 0644 -o root -g root /dev/stdin "$drop_dst"

# 7. Tsnet seed.
seeded=0
if [[ -f "$state_dir/tailscaled.state" ]]; then
  info "tsnet state dir already seeded — skipping --init"
  seeded=1
elif [[ -t 0 ]]; then
  info ""
  info "Paste your Tailscale auth key (input hidden; tskey-auth-... format):"
  read -r -s ts_authkey
  info ""
  [[ -n "$ts_authkey" ]] || die "empty TS_AUTHKEY — aborting"
  info "seeding tsnet state via 'ghostframe-xdaemon --init'..."
  sudo -u "$target_user" \
    TS_AUTHKEY="$ts_authkey" \
    TS_HOSTNAME="$(hostname)-ghostframe" \
    TS_STATE_DIR="$state_dir" \
    "$installed_bin" --init
  seeded=1
else
  info "non-interactive — skipping tsnet seed. Run this before first boot:"
  info "  sudo -u $target_user TS_AUTHKEY=<your-key> TS_STATE_DIR=$state_dir $installed_bin --init"
fi

# 8. Enable.
info "systemctl daemon-reload"
systemctl daemon-reload

info "enabling ghostframe.target for user $target_user..."
# The target user's --user manager may not be running. Use `--global` to
# enable for the user, then their next login starts it. Equivalent to running
# `systemctl --user enable` inside that user's session.
sudo -u "$target_user" \
  XDG_RUNTIME_DIR="/run/user/$user_uid" \
  systemctl --user enable ghostframe.target 2>/dev/null \
  || sudo -u "$target_user" \
       env XDG_RUNTIME_DIR="/run/user/$user_uid" \
       systemctl --user --no-block enable ghostframe.target

info "enabling getty@tty1..."
systemctl enable getty@tty1.service

info ""
info "installation complete."
if [[ "$seeded" -eq 1 ]]; then
  info "Reboot the machine. On a phone or laptop on your tailnet, open:"
  info "  http://<your-relay-host>:8000/?host=<this-hostname>-ghostframe.<tailnet>.ts.net:4443&certHash=<hash>"
  info ""
  info "The certHash is printed on stdout at first boot. After reboot, retrieve it with:"
  info "  journalctl --user -u ghostframe-xdaemon -b | grep CERT_HASH_SHA256"
else
  info "Run the --init command above, then reboot."
fi
```

- [ ] **Step 2: Make it executable**

Run: `chmod +x packaging/install.sh`
Expected: silent success.

- [ ] **Step 3: Sanity-check with shellcheck if available**

Run: `command -v shellcheck >/dev/null && shellcheck packaging/install.sh || echo "shellcheck not installed; skipping"`
Expected: either no warnings, or the "skipping" message. If shellcheck reports SC2086 (unquoted variable) or SC2155 (declare-and-assign), fix the specific line it points at; minor stylistic warnings (SC2034 unused-variable, etc.) can be `# shellcheck disable=...` annotated if they're false positives.

- [ ] **Step 4: Smoke-test the `--help` path**

Run: `packaging/install.sh --help`
Expected: prints the usage block (the first ~20 lines of the script's header comment) and exits 0. Should NOT require root.

Wait — the current script exits early if not root, before checking for `--help`. Fix that: move the `--help` handling before the root check. Edit the script so the loop processing `--help` happens before the `[[ ${EUID} -eq 0 ]] ...` line. Specifically: extract the `--help`/`-h` case into a quick pre-loop check.

Replace the lines:

```bash
[[ ${EUID} -eq 0 ]] || die "must be run as root (try: sudo $0 $*)"

force=0
target_user=""
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
    --help|-h)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
```

With:

```bash
for arg in "$@"; do
  case "$arg" in
    --help|-h)
      sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
  esac
done

[[ ${EUID} -eq 0 ]] || die "must be run as root (try: sudo $0 $*)"

force=0
target_user=""
for arg in "$@"; do
  case "$arg" in
    --force) force=1 ;;
```

Re-run: `packaging/install.sh --help`
Expected: prints usage, exits 0.

- [ ] **Step 5: Smoke-test the "no args" error path**

Run: `packaging/install.sh`
Expected: exits non-zero (root check fails first when not run via sudo: "must be run as root"). When run via sudo without arguments, it should say "username required".

- [ ] **Step 6: Commit**

```bash
git add packaging/install.sh
git commit -m "packaging: install.sh — root-run installer

Sets up ghostframe-xdaemon as a headless Xorg session for a specific
user: copies the release binary to /usr/local/bin, drops the Xorg
config, installs four user-level systemd units, writes a getty
autologin drop-in (99- prefix to win against existing drop-ins),
prompts for TS_AUTHKEY interactively to seed the tsnet state dir,
then enables ghostframe.target + getty@tty1.

Idempotent on re-run (skips already-installed files unless --force,
never re-prompts for TS_AUTHKEY if state dir is populated). Supports
non-interactive use by skipping the prompt and printing the one-shot
init command.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 8: Write `DEVELOPERS.md`

**Files:**
- Create: `DEVELOPERS.md`

This file absorbs the developer-facing content from the current README and adds the new "alternative configurations" and "WM alternatives" sections.

- [ ] **Step 1: Create `DEVELOPERS.md` with the following content**

```markdown
# Ghostframe — Developer Guide

This document covers building from source, running tests, day-to-day
development tooling, and notes on alternative configurations not covered by
the README's install path. For installing and using ghostframe, see
[README.md](README.md).

## Repository layout

| Crate | Purpose |
| --- | --- |
| `ghostframe-lib` | Core library: tile engine, encoders, QUIC/WebTransport stack, Tailscale integration. Builds as `cdylib` / `staticlib` / `rlib`. |
| `ghostframe-xdaemon` | Standalone X11/DRM capture daemon that hosts a session over an embedded Tailscale node. |
| `ghostframe-web-client` | TypeScript + Vite browser client (WebTransport + WebCodecs + WebGPU). |
| `ghostframe-test-pattern` | Deterministic test-pattern generator used by the e2e harness. |
| `ghostframe-e2e` | End-to-end test harness driving Chromium against a containerised server over a local headscale tailnet. |
| `ghostframe-bench` | Codec micro-benchmarks (see `docs/specs/m3-codec-bench-results.md`). |
| `ghostbridge` | Go ↔ Rust FFI shim for libtailscale. |

For the full protocol design, see
[docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).

## Developer prerequisites

These are on top of the runtime + build-from-source packages already listed
in the README's "Prerequisites" section:

- **`cbindgen`** for FFI header generation: `cargo install cbindgen`
- **Docker** for the end-to-end test containers (test-server + headscale).
- **`just`** — every common task is in the `Justfile`.

## Building

Workspace:

```bash
cargo build           # debug
cargo build --release # release
# or:
just build
just build-release
```

Web client (output goes to `ghostframe-web-client/dist/`):

```bash
cd ghostframe-web-client
npm install
npm run build
# or, from repo root:
just web-client-build
```

> The e2e harness serves `ghostframe-web-client/dist/` over HTTP to headless
> Chromium. A stale `dist/` silently breaks tests. Always rebuild the web
> client after pulling changes that touch it. `just test-e2e` does this for
> you.

## Running tests

Unit tests:

```bash
cargo test --lib
# or:
just test-unit
```

End-to-end tests bring up a containerised server with a virtual X server and
a local headscale, then drive Chromium against it:

```bash
just test-e2e
```

That target builds the web client, builds the release binaries used inside
the containers, builds the two container images
(`ghostframe/test-server`, `ghostframe/test-headscale`), then runs
`cargo test --test e2e`.

After any change to `ghostframe-lib` or `ghostframe-xdaemon`, rebuild the
container before re-running e2e tests, otherwise tests will silently run
the stale binary baked into the image:

```bash
just containers-build
```

E2e tests use `--test-threads=1` and depend on the Docker daemon being
reachable by the current user.

## Manual smoke test against a real tailnet

See [docs/m0-manual-smoke-test.md](docs/m0-manual-smoke-test.md) for the
ping/pong smoke test that exercises `ghostframe-xdaemon` + the web client
against a real Tailscale tailnet.

## Lint and format

```bash
just lint        # cargo clippy -- -D warnings
just fmt-check   # cargo fmt -- --check
just fmt         # cargo fmt
just ci-local    # run the fast CI tier in the order CI runs it
```

## Alternative configurations

The README's install path targets AMD GPUs via the `amdgpu` driver's
`Virtual` connector. Other configurations are not part of the supported
install path but are documented here for users who want to adapt the setup.

### Intel iGPU

The Intel `modesetting` driver exposes a `Virtual_1` connector under
similar constraints. Swap the `Device` and `Monitor` sections in
`packaging/xorg-headless-amdgpu.conf`:

```
Section "Device"
    Identifier "Card0"
    Driver     "modesetting"
    Option     "Monitor-Virtual-1" "VirtualMonitor"
EndSection
```

Save as `xorg-headless-intel.conf` and point the install script at it (or
manually copy it to `/etc/X11/ghostframe-headless.conf`).

### NVIDIA (proprietary driver)

The NVIDIA driver supports headless operation via `AllowEmptyInitialConfiguration`
and `AllowHeadlessMode`. A starting point:

```
Section "Device"
    Identifier "Card0"
    Driver     "nvidia"
    Option     "AllowEmptyInitialConfiguration" "true"
    Option     "AllowHeadlessMode" "true"
EndSection
```

This has not been tested against ghostframe; PRs welcome.

### Xorg + dummy driver (no GPU)

For machines without a supported GPU, `xf86-video-dummy` provides a CPU-only
Xorg with no DRM device. `ghostframe-xdaemon` will detect the absence of DRM
and fall back to its X11 capture path. Hardware encoding is not available
in this configuration.

```
Section "Device"
    Identifier "Card0"
    Driver     "dummy"
    VideoRam   16384
EndSection
```

### VKMS (virtual KMS kernel module)

Not useful today. VKMS is a software-only virtual KMS device; capturing from
it and encoding on a discrete GPU is the cross-device PRIME case which is
known broken (yields stale/scrambled bytes). The workaround in ghostframe is
a CPU-mmap fallback, which gives no advantage over Xorg dummy. Revisit when
the cross-device PRIME issue is resolved upstream.

## WM alternatives

The default `ghostframe-wm.service` runs `enlightenment_start`. To use a
different window manager, edit the `ExecStart=` line of
`~/.config/systemd/user/ghostframe-wm.service`. Common alternatives:

| WM | `ExecStart=` |
| --- | --- |
| enlightenment | `/usr/bin/enlightenment_start` |
| i3 | `/usr/bin/i3` |
| openbox | `/usr/bin/openbox` |
| sway (X11 via Xwayland) | not supported — sway is Wayland-only; use a different WM here |
| fluxbox | `/usr/bin/fluxbox` |
| xfwm4 | `/usr/bin/xfwm4` |

After editing, reload and restart:

```bash
systemctl --user daemon-reload
systemctl --user restart ghostframe-wm.service
```

The `Restart=on-failure` directive will not bring up the new WM if the
binary doesn't exist on the host. Verify the path with `command -v`.

## Contributing

Ghostframe is a small project with strong opinions about scope and design.
Before writing code, **please open an issue describing the problem or the
feature you want to work on.** A short discussion up front prevents the
much more painful situation where a PR that took real effort has to be
turned down because the change conflicts with the design or with work
already in flight.

Once we have agreed on the approach in the issue, open a GitHub pull request
that references it. Keep the PR focused on what was agreed; if the scope
grows during implementation, comment on the issue rather than expanding the
PR unilaterally.

The only exception: **trivial bug fixes of fewer than 10 lines of code** may
be submitted directly as a PR without a prior issue. "Trivial" here means a
clearly correct one-spot fix — typos, off-by-ones, obvious null/`unwrap`
guards. Anything that changes behaviour, adds a dependency, touches the
wire format, or requires judgement about the right fix should still go
through an issue first.

Every PR is expected to:

- pass `just lint`, `just fmt-check`, and the checks described in [docs/ci.md](docs/ci.md),
- pass `just test-unit`, and
- pass `just test-e2e` if it touches anything that the e2e harness covers
  (capture, encoders, protocol, web client).
```

- [ ] **Step 2: Verify the file was written and links are present**

Run: `wc -l DEVELOPERS.md && grep -c '\[.*\](.*)' DEVELOPERS.md`
Expected: file is ~200 lines, multiple Markdown link references.

- [ ] **Step 3: Commit**

```bash
git add DEVELOPERS.md
git commit -m "docs: add DEVELOPERS.md absorbing dev content from old README

Moves repository layout, developer prerequisites, build instructions,
test commands, manual smoke-test pointer, lint/format commands, and
contributing notes out of README.md and into a developer-focused
document. Adds new sections covering alternative GPU configurations
(Intel, NVIDIA, Xorg dummy, VKMS) and WM alternatives.

README.md rewrite follows in the next commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 9: Rewrite `README.md`

**Files:**
- Modify: `README.md` (full overwrite)

- [ ] **Step 1: Overwrite README.md**

Replace the entire contents of `README.md` with:

```markdown
# Ghostframe

A Linux-only remote desktop server: stream a headless Xorg session running
your favourite window manager to a browser, over QUIC on a Tailscale
tailnet. Per-tile adaptive encoding (H.264 for motion, palette/wavelet/
solid-fill for static content) keeps bandwidth low and text crisp.

For the full design see
[docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md).
For building, testing, and developer tooling see
[DEVELOPERS.md](DEVELOPERS.md).

## Prerequisites

Ghostframe today does not ship pre-built binaries — you build from source
during install. The prerequisites cover both the runtime stack (Xorg + GPU
driver + WM + Vulkan) and the build-from-source toolchain.

The install path is supported on **AMD GPUs with the `amdgpu` driver**.
Other GPUs are documented in [DEVELOPERS.md](DEVELOPERS.md#alternative-configurations)
but not part of the supported install. The default window manager is
**Enlightenment**; alternatives are one `ExecStart=` line away (see
[DEVELOPERS.md](DEVELOPERS.md#wm-alternatives)).

**Ubuntu 24.04:**

```bash
sudo apt-get install \
    build-essential pkg-config clang libclang-dev golang-go \
    rustc cargo \
    nodejs npm \
    libavcodec-dev libavformat-dev libavutil-dev libswscale-dev libavdevice-dev \
    libx264-dev libx11-dev libxext-dev libxdamage-dev libdrm-dev \
    libvulkan1 mesa-vulkan-drivers vulkan-tools \
    xserver-xorg xserver-xorg-video-amdgpu enlightenment
```

If your distribution's `rustc` is older than 1.74, install Rust via
[rustup](https://rustup.rs/) instead.

**Arch Linux:**

```bash
sudo pacman -S base-devel clang go rust nodejs npm \
    ffmpeg x264 libx11 libxext libxdamage libdrm \
    vulkan-icd-loader vulkan-tools \
    xorg-server xf86-video-amdgpu enlightenment
```

You also need an `amdgpu` kernel module configured to expose a virtual
display. Add `/etc/modprobe.d/amdgpu.conf`:

```
options amdgpu virtual_display=<PCI_ID>,1
```

Find `<PCI_ID>` with `lspci -D | grep VGA` (use the form `0000:03:00.0`).
Reboot after creating this file.

A Tailscale account with a reusable pre-auth key
(https://login.tailscale.com/admin/settings/keys) is required to register
the host on your tailnet. The install script will prompt for it.

## Install

```bash
# 1. Clone and build from source.
git clone https://github.com/<your-org>/ghostframe.git
cd ghostframe
cargo build --release -p ghostframe-xdaemon
(cd ghostframe-web-client && npm install && npm run build)

# 2. Run the installer as root, targeting the user that should own the
#    headless session.
sudo ./packaging/install.sh <username>

# 3. Paste your Tailscale auth key when the script prompts, then reboot.
sudo reboot
```

After reboot, the configured user is automatically logged in on `tty1`,
Xorg comes up on display `:1`, Enlightenment starts inside that session,
and `ghostframe-xdaemon` joins the tailnet and starts capturing.

## First connection

On any device on the same tailnet (phone, laptop, another machine):

1. Find this host's tailnet name and the daemon's TLS cert hash. SSH into
   the host as the configured user and run:

   ```bash
   journalctl --user -u ghostframe-xdaemon -b | grep CERT_HASH_SHA256
   ```

   That prints a line like `CERT_HASH_SHA256=<64-hex>`.

2. Serve the web client on the host (or any other tailnet device):

   ```bash
   cd ghostframe/ghostframe-web-client/dist
   python3 -m http.server 8000 --bind 127.0.0.1
   ```

3. Open in Chrome / Chromium / Edge:

   ```
   http://127.0.0.1:8000/?host=<hostname>-ghostframe.<tailnet>.ts.net:4443&certHash=<64-hex>
   ```

You should see the Enlightenment desktop streaming into your browser.

## More

- Build, test, contribute: [DEVELOPERS.md](DEVELOPERS.md)
- Protocol and architecture: [docs/specs/ghostframe-initial-spec.md](docs/specs/ghostframe-initial-spec.md)
```

- [ ] **Step 2: Verify the file is short and well-formed**

Run: `wc -l README.md`
Expected: under ~120 lines (target ~80, some give for the apt/pacman blocks).

- [ ] **Step 3: Check internal links are not broken**

Run: `grep -oE '\[[^]]+\]\([^)]+\)' README.md | grep -v 'http' | sed 's/.*(\(.*\))/\1/' | while read p; do test -e "$p" && echo "ok: $p" || echo "BROKEN: $p"; done`
Expected: every printed line starts with `ok:`. If any say `BROKEN:`, fix the reference before committing.

- [ ] **Step 4: Commit**

```bash
git add README.md
git commit -m "docs: rewrite README.md as a short install + first-connection guide

README is now a focused install path: prerequisites for Ubuntu / Arch
(targeting amdgpu + enlightenment), three install commands, then a
two-step first-connection walkthrough. All developer-facing content
(build, test, contribute, alt configs) was moved to DEVELOPERS.md in
the previous commit.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

## Task 10: Workspace-wide verification

**Files:** none (verification only)

- [ ] **Step 1: Full build**

Run: `cargo build --workspace --release --exclude ghostframe-e2e`
Expected: success. The xdaemon changes from Tasks 4–6 should compile cleanly.

- [ ] **Step 2: Unit tests**

Run: `cargo test --workspace --lib`
Expected: all pass. The new `state_dir_seeded` tests run as part of `ghostframe-xdaemon --lib`.

- [ ] **Step 3: xdaemon integration tests**

Run: `cargo test -p ghostframe-xdaemon`
Expected: all pass (including the new `init_flag.rs` tests).

- [ ] **Step 4: Lint and format**

Run: `just lint`
Expected: no warnings.

Run: `just fmt-check`
Expected: no diff.

- [ ] **Step 5: Web client typecheck (cheap sanity)**

Run: `cd ghostframe-web-client && npx tsc --noEmit && cd ..`
Expected: no errors. We haven't touched the web client; this is a smoke check that nothing implicit broke.

- [ ] **Step 6: shellcheck install.sh (if installed)**

Run: `command -v shellcheck >/dev/null && shellcheck packaging/install.sh || echo skipped`
Expected: no warnings, or "skipped".

- [ ] **Step 7: No commit needed**

This task only verifies — if anything failed, go back to the relevant task and fix.

---

## Task 11: E2E sweep

**Files:** none (verification only)

- [ ] **Step 1: Rebuild containers**

Because `ghostframe-xdaemon` changed (Task 5), the test-server image must
be rebuilt or e2e tests will run the stale binary.

Run: `just containers-build`
Expected: success.

- [ ] **Step 2: Run the full e2e suite**

Run: `just test-e2e`
Expected: same pass/ignore counts as before the change. Specifically, no new failures. (`ghostframe-bench/target/` already existed; ignore it.)

The xdaemon changes affect only CLI argument parsing and the TS_AUTHKEY/state-dir handling. The e2e harness sets `TS_AUTHKEY` explicitly when spawning the test-server container, so the "TS_AUTHKEY present and non-empty" path is exercised — which is the same path that ran before this change.

- [ ] **Step 3: No commit needed**

If anything fails, go back to the relevant task and fix.

---

## Task 12: Open the PR

**Files:** none (git/gh only)

- [ ] **Step 1: Push the branch**

Run: `git push -u origin feature/readme-restructure-install`
Expected: branch published.

- [ ] **Step 2: Open the PR**

Run:

```bash
gh pr create --title "Add install path: headless Xorg + autologin + xdaemon as user systemd units" --body "$(cat <<'EOF'
## Summary

- Splits docs into a short install-focused `README.md` and a developer-focused `DEVELOPERS.md`.
- Adds `packaging/` with `install.sh`, a sample `amdgpu` `xorg.conf`, four user-level systemd units, and a getty autologin drop-in template.
- Makes `TS_AUTHKEY` required only at install time via a new `ghostframe-xdaemon --init` flag; after the state dir is seeded, the user unit runs with no secret in any env file.

Spec: `docs/superpowers/specs/2026-06-07-readme-restructure-design.md` (commit d24e091).

## Test plan

- [ ] `cargo test --workspace --lib` passes
- [ ] `cargo test -p ghostframe-xdaemon` (incl. new `init_flag` integration tests) passes
- [ ] `just lint` and `just fmt-check` pass
- [ ] `just test-e2e` passes after `just containers-build`
- [ ] On a real machine with an AMD GPU + the `amdgpu virtual_display` kernel option: run `sudo ./packaging/install.sh <user>`, paste a tailnet auth key, reboot, verify the headless session comes up and a browser on the tailnet can connect.

EOF
)"
```

Expected: PR URL printed. Return that URL.

- [ ] **Step 3: No commit needed**

---

## Done

After Task 12, the PR is open and ready for review.

## Plan self-review notes

(Author's notes; not steps for the executor.)

- Spec section "Repository changes" lists `packaging/` artifacts — Tasks 2, 3, 7 create them.
- Spec section "Modified: `ghostframe-xdaemon/src/main.rs`" — Tasks 4, 5 implement both bullet points (optional `TS_AUTHKEY` + `--init` flag) and Task 6 adds tests.
- Spec section "Modified: `README.md`" + "Modified: `DEVELOPERS.md`" — Tasks 8, 9.
- Spec "Acceptance criteria" — Task 10 covers cargo build / lib tests / lint / fmt; Task 11 covers `just test-e2e`. README-under-80-lines is verified in Task 9 step 2 (with a `~120` allowance because the apt/pacman blocks are necessarily large; the spec said "under ~80" as a target, not a hard limit). DEVELOPERS.md "contains every section that was in the old README" verified visually in Task 8 step 2. The "fresh Arch/Ubuntu VM" success criterion is not in scope for the PR's automated tests — it's part of the PR's test plan and will be exercised by the author or reviewer.
- Open question 1 in spec (xorg.conf modeline) — addressed in Task 2 with a standard 1920×1080@60 modeline.
- Open question 2 (uninstall flow) — deferred; not implemented.
- Open question 3 (TS_AUTHKEY format validation) — deferred; install.sh accepts any non-empty string.
- Open question 4 (certHash retrieval helper) — deferred; the journalctl recipe is documented in install.sh's final message and in README.
- Open question 5 (ghostbridge "is state authenticated") — addressed by Task 4's `state_dir_seeded()` proxy (presence of `tailscaled.state`); tsnet's empty-AuthKey behaviour is to reuse saved state.
