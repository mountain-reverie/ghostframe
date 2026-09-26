# Attach mode Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let the daemon capture an existing X session instead of standing up a headless one, so a machine whose GPU also drives a local session stops fighting itself.

**Architecture:** A second deployment mode selected by `install.sh --mode attach|headless`. Attach installs one unit (`ghostframe-xdaemon` as the session user, `DISPLAY=:0`) instead of three, and lowers M4b's resolution ceiling so a remote client cannot resize the physical output out of the recovery view's reach.

**Tech Stack:** bash (install.sh), systemd user units, lightdm autologin, Rust (one env-var-driven ceiling clamp).

**Spec:** `docs/superpowers/specs/2026-09-25-attach-mode-design.md`

---

## Read this first

**Almost all of this is packaging, not Rust.** Exactly one task touches Rust: the ceiling clamp. Resist the urge to add code — the point of attach mode is to *remove* moving parts.

**The spec's §5 cleanup is load-bearing.** If both modes' units end up installed, both X servers run and you reproduce the exact contention attach mode exists to avoid — and it will look like a new bug. That task gets a real test.

**Verified against source while writing this plan** (confirm anything you depend on):

| fact | where |
|---|---|
| `fn ceiling(&self) -> (u16, u16)` queries RandR live, falls back to a `Mutex` | `ghostframe-xdaemon/src/display.rs` |
| `XrandrDisplay::new() -> anyhow::Result<Self>` | `display.rs:48` |
| Constructed in `main.rs` with `match display::XrandrDisplay::new()`, warn-and-continue on `Err` | `ghostframe-xdaemon/src/main.rs` |
| Env vars read as `env::var("X").unwrap_or_else(...)` | `main.rs:69`, `:99`, `:100` |
| Daemon unit sets `Environment=DISPLAY=:1` and `GHOSTFRAME_X11_CAPTURE_ONLY=1` | `packaging/systemd/ghostframe-xdaemon.service:13,25` |
| Daemon unit has `Requires=`/`After=`/`PartOf=ghostframe-wm.service` | same file, lines 3-7 |
| `install.sh` templates with `sed "s\|__USER__\|...\|g"` | `packaging/install.sh` |
| `install.sh` arg loop already handles `--force` and `--display-backend` | same file |
| Unit install loop covers `ghostframe.target`, `-wm`, `-xdaemon`; xorg is templated separately | same file |

**Operational rules** (this repo, this branch): stage explicit paths only — `git add -A` is forbidden. Never `git stash`. Use `cargo fmt -p <crate>`, not `--all`. Do not pipe a build or test through `tail`/`head` — the pipe reports the last stage's status and this repo has had a failed build look green that way.

---

## File structure

| File | Responsibility | Task |
|---|---|---|
| `ghostframe-xdaemon/src/display.rs` | Honour `GHOSTFRAME_ATTACH_MAX_RESOLUTION` in `ceiling()` | 1 |
| `packaging/systemd/ghostframe-xdaemon-attach.service.tmpl` | The single attach-mode unit | 2 |
| `packaging/install.sh` | `--mode` flag, attach install path, other-mode cleanup | 3, 4, 5 |
| `packaging/lightdm-autologin.conf.tmpl` | lightdm autologin drop-in | 4 |
| `tests/packaging/mode_switch_test.sh` | Assert mode switching removes the other mode's units | 5 |

---

## Task 1: Clamp the ceiling from an env var

The only Rust in this plan.

**Files:** Modify `ghostframe-xdaemon/src/display.rs`

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_means_no_clamp() {
        assert_eq!(clamp_ceiling((16384, 16384), None), (16384, 16384));
    }

    #[test]
    fn a_wxh_value_lowers_the_ceiling() {
        assert_eq!(clamp_ceiling((16384, 16384), Some("1920x1080")), (1920, 1080));
    }

    #[test]
    fn the_clamp_only_ever_lowers() {
        // A clamp larger than the hardware ceiling must not raise it -- that
        // would ask X for a mode it cannot allocate.
        assert_eq!(clamp_ceiling((1280, 720), Some("1920x1080")), (1280, 720));
    }

    #[test]
    fn none_disables_the_clamp() {
        // The documented escape hatch, for an operator who has decided the
        // recovery view no longer needs protecting.
        assert_eq!(clamp_ceiling((16384, 16384), Some("none")), (16384, 16384));
    }

    #[test]
    fn malformed_values_are_ignored_not_fatal() {
        // A typo in a unit file must not stop the daemon serving. Each of
        // these logs a warning and leaves the hardware ceiling in place.
        for bad in ["", "1920", "1920x", "x1080", "1920*1080", "abcxdef", "0x0"] {
            assert_eq!(
                clamp_ceiling((16384, 16384), Some(bad)),
                (16384, 16384),
                "input {bad:?} should have been ignored"
            );
        }
    }
}
```

- [ ] **Step 2: Run to verify failure**

```bash
cargo test -p ghostframe-xdaemon clamp_ceiling
```

Expected: FAIL to compile — `clamp_ceiling` does not exist.

- [ ] **Step 3: Implement the pure function**

```rust
/// Lower a hardware ceiling to an operator-configured maximum.
///
/// `raw` is the value of `GHOSTFRAME_ATTACH_MAX_RESOLUTION`: `"WIDTHxHEIGHT"`
/// to clamp, `"none"` to disable, absent to disable.
///
/// **Only ever lowers.** A configured value above the hardware ceiling is
/// ignored rather than applied, because raising it would have the server ask
/// X for a mode it cannot allocate.
///
/// **Malformed input is ignored, not fatal.** A typo in a unit file must not
/// stop the daemon serving; it warns and keeps the hardware ceiling. Failing
/// closed here would turn a cosmetic mistake into an outage.
fn clamp_ceiling(hardware: (u16, u16), raw: Option<&str>) -> (u16, u16) {
    let Some(raw) = raw else {
        return hardware;
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return hardware;
    }
    let parsed = raw
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse::<u16>().ok()?, h.trim().parse::<u16>().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    match parsed {
        Some((w, h)) => (hardware.0.min(w), hardware.1.min(h)),
        None => {
            tracing::warn!(
                value = raw,
                "GHOSTFRAME_ATTACH_MAX_RESOLUTION is not WIDTHxHEIGHT or 'none'; ignoring"
            );
            hardware
        }
    }
}
```

Check `split_once`'s char-slice pattern compiles on this toolchain; if not, split on `'x'` and handle `'X'` by lowercasing first.

- [ ] **Step 4: Wire it into `ceiling()`**

`ceiling()` currently returns the live RandR value or a `Mutex` fallback. Apply the clamp to **both** return paths — a clamp that only applies on the happy path would silently lift itself the moment a RandR query failed.

Read the env var **once** at construction in `XrandrDisplay::new()` and store it on the struct, rather than calling `env::var` inside `ceiling()`. `ceiling()` runs per `DisplayMode` request; re-reading the environment per call is wasted work, and this crate's convention (`main.rs:69`, `:99`) is to read env vars at startup. Log the effective value once at construction so the install is self-documenting:

```rust
tracing::info!(
    max_resolution = ?self.max_resolution,
    "display controller ceiling clamp"
);
```

- [ ] **Step 5: Run the tests and gates**

```bash
cargo test -p ghostframe-xdaemon
cargo fmt -p ghostframe-xdaemon
cargo clippy -p ghostframe-xdaemon --all-targets -- -D warnings
```

- [ ] **Step 6: Prove the clamp is load-bearing**

Mutate `clamp_ceiling` to return `hardware` unconditionally and confirm `a_wxh_value_lowers_the_ceiling` fails. Revert. A clamp that cannot fail its own test is not protecting anything.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-xdaemon/src/display.rs
git commit -m "feat(xdaemon): clamp the display ceiling from the environment

GHOSTFRAME_ATTACH_MAX_RESOLUTION lowers what ceiling() reports, so M4b's
existing clamp/align/floor pipeline applies unchanged -- no second place a
requested mode can be altered.

Only ever lowers: a value above the hardware ceiling is ignored rather than
applied, since raising it would ask X for a mode it cannot allocate.
Malformed input warns and keeps the hardware ceiling rather than refusing to
start, so a typo in a unit file is not an outage."
```

---

## Task 2: The attach-mode unit

**Files:** Create `packaging/systemd/ghostframe-xdaemon-attach.service.tmpl`

- [ ] **Step 1: Write the unit**

Start from `packaging/systemd/ghostframe-xdaemon.service` and make exactly these changes. Read that file first — it carries comments worth preserving.

- **Drop** `Requires=`, `After=` and `PartOf=ghostframe-wm.service`. There is no wm unit in this mode. Replace with `PartOf=graphical-session.target` and `After=graphical-session.target` so the daemon's lifetime follows the session, per spec §4.
- **Change** `Environment=DISPLAY=:1` to `Environment=DISPLAY=:0`.
- **Keep** `Environment=GHOSTFRAME_X11_CAPTURE_ONLY=1`, but **rewrite its comment**. The headless text says a DRM read would see the host's scanout "instead of the guest Xorg on `DISPLAY=:1`" — a content mismatch that cannot arise here, since in attach mode the captured session *is* the physical display. The variable stays set for a privilege reason instead (writeback needs DRM master, X holds it; the FB fallback needs master or `CAP_SYS_ADMIN`). See spec §2, which was corrected on this point.
- **Add** `Environment=GHOSTFRAME_ATTACH_MAX_RESOLUTION=__MAX_RESOLUTION__`.
- **Add** `WantedBy=graphical-session.target` in `[Install]` (not `ghostframe.target`; there is no target in this mode).

Include a header comment stating the security position from spec §6, in the file itself:

```
# ATTACH MODE. This daemon captures the session of the user it runs as --
# DISPLAY=:0, the operator's own desktop.
#
# A client that reaches the tailnet endpoint therefore gets that desktop:
# their files, their browser sessions, their sudo. Not a sandboxed guest
# session as in headless mode. M4a's single-client eviction still applies,
# so two clients cannot both attach, but the tailnet is the only boundary.
#
# If that is not what you want, use --mode headless.
```

- [ ] **Step 2: Verify it parses and substitutes**

```bash
sed 's|__MAX_RESOLUTION__|1920x1080|g' packaging/systemd/ghostframe-xdaemon-attach.service.tmpl > /tmp/attach-test.service
systemd-analyze verify --user /tmp/attach-test.service 2>&1 | head
grep -E "^(Environment|WantedBy|PartOf|After|ExecStart)" /tmp/attach-test.service
```

Expected: no syntax errors from `systemd-analyze` (it may warn about the binary not existing at that path — that is fine, unit *syntax* is what matters), and `DISPLAY=:0` present with no `ghostframe-wm` reference.

- [ ] **Step 3: Commit**

```bash
git add packaging/systemd/ghostframe-xdaemon-attach.service.tmpl
git commit -m "packaging: attach-mode daemon unit

One unit, no X server or WM of its own: DISPLAY=:0, lifetime tied to
graphical-session.target. Carries the security note from the design doc in
the file itself, so nobody deploys this mode without reading that clients
get the operator's own desktop."
```

---

## Task 3: The `--mode` flag

**Files:** Modify `packaging/install.sh`

- [ ] **Step 1: Add the flag to the arg loop**

`install.sh` already parses `--force` and `--display-backend amdgpu|vkms` with an `expect_*` pattern for the value form. Follow it exactly — both the space form (`--mode attach`) and the equals form (`--mode=attach`), rejecting a missing or invalid value with `die`.

```bash
mode="headless"
```

as the default, set alongside the existing `display_backend="amdgpu"`.

**Add `--max-resolution` in this task too**, not in Task 4 -- it is another value flag and belongs with the parsing work rather than split across tasks:

```bash
max_resolution="1920x1080"
```

Accept any `WIDTHxHEIGHT` or the literal `none`. Do **not** validate the geometry further here: `clamp_ceiling` (Task 1) already ignores malformed values at runtime without refusing to start, and duplicating the rule in bash would give two places to disagree. Reject only an empty value.

Also extend the usage comment block at the top of the file (the one `--help` prints via `sed -n '2,NNp'`). **Check the line range that `--help` prints and widen it if the block grows**, exactly as the `--display-backend` change had to — otherwise `--help` silently truncates.

- [ ] **Step 2: Test the parser before wiring anything to it**

Extract the arg-parsing block into a scratch script and exercise every case:

```bash
for args in "bob" "bob --mode attach" "bob --mode=attach" "bob --mode headless" \
            "bob --mode" "bob --mode bogus" "bob --force --mode attach"; do
  printf '%-34s -> ' "$args"; bash /tmp/argtest.sh $args 2>&1 | tail -1
done
```

Expected: default `headless`; both value forms accepted; `--mode` with no value and `--mode bogus` both `die` with a clear message.

- [ ] **Step 3: Commit**

```bash
git add packaging/install.sh
git commit -m "packaging: add install.sh --mode attach|headless

Defaults to headless, so existing installs and genuinely headless hosts are
unaffected. Parsing only; the attach path lands next."
```

---

## Task 4: The attach install path

**Files:** Modify `packaging/install.sh`; create `packaging/lightdm-autologin.conf.tmpl`

- [ ] **Step 1: Write the lightdm autologin template**

```
# Installed by ghostframe packaging/install.sh --mode attach.
#
# Attach mode captures an existing X session, so that session must exist after
# an unattended boot or the machine serves nothing. This autologins the session
# user to create it.
#
# Consequence, deliberate: an unattended boot leaves this machine logged in. On
# a host whose purpose is to serve that session remotely, that is the point. If
# it is not what you want, use --mode headless, which runs its own isolated
# session instead.
[Seat:*]
autologin-user=__USER__
autologin-user-timeout=0
```

Check the correct drop-in location on this distro — `/etc/lightdm/lightdm.conf.d/` if it exists, otherwise editing `[Seat:*]` in `lightdm.conf` directly. **Prefer a drop-in**; editing the main config means clobbering or merging an operator's own settings. If no drop-in dir exists, create it rather than editing `lightdm.conf`, and say so in the install output.

- [ ] **Step 2: Branch the install on `$mode`**

In attach mode:

1. Install `ghostframe-xdaemon-attach.service.tmpl` to the user unit dir as `ghostframe-xdaemon.service`, substituting `__MAX_RESOLUTION__` (a new `--max-resolution` flag, default `1920x1080`, accepting `none`).
2. Install the lightdm autologin drop-in, substituting `__USER__`.
3. **Skip** `ghostframe-xorg`, `ghostframe-wm`, `ghostframe.target`, the getty autologin drop-in, and the `Xwrapper.config` write entirely — spec §2.
4. Enable `ghostframe-xdaemon.service` for the user rather than `ghostframe.target`.
5. Print the §6 security notice **to stdout during install**, not only in the unit file.
6. Print the effective `GHOSTFRAME_ATTACH_MAX_RESOLUTION` **and name the variable** (spec risk 5). A user whose 1440p client gets 1080p must be able to find out why without reading the source.
7. If `/etc/X11/Xwrapper.config` exists and carries this project's header, print that the setting is no longer needed in attach mode and that the script is deliberately leaving it alone (spec §5). Do not remove it -- the script cannot know nothing else depends on it.

Keep the headless path byte-identical in behaviour. The cleanest structure is one `if [[ "$mode" == "attach" ]]` branch around steps 3-8 of the existing script rather than an `if` inside each step; decide and say which you chose and why.

**Verify the session user actually owns a graphical session** (spec §2) rather than accepting any username and failing later at connect time. `loginctl show-user "$target_user" -p Display` or checking for an active seat session is enough. Warn rather than `die` if autologin is being configured in the same run — the session may legitimately not exist yet.

- [ ] **Step 3: Validate**

```bash
bash -n packaging/install.sh && echo "parses"
bash packaging/install.sh --help | tail -20   # the usage block must not be truncated
```

- [ ] **Step 4: Commit**

```bash
git add packaging/install.sh packaging/lightdm-autologin.conf.tmpl
git commit -m "packaging: install the attach-mode session

One user unit, plus lightdm autologin so :0 exists after an unattended boot.
Skips the second X server, the WM, the getty autologin and the
Xwrapper.config loosening -- attach mode needs none of them, and that
loosening is a global grant of X-with-root-rights that existed solely so a
user unit could launch Xorg.

Prints the security notice during install: in this mode a client that
reaches the endpoint gets the operator's own desktop."
```

---

## Task 5: Mode switching must remove the other mode's units

The spec calls this load-bearing (§5, risk 1), so it gets a real test.

**Files:** Modify `packaging/install.sh`; create `tests/packaging/mode_switch_test.sh`

- [ ] **Step 1: Write the failing test**

```bash
#!/usr/bin/env bash
# Asserts that switching modes removes the other mode's units.
#
# Why this test exists: if both modes' units remain installed, both X servers
# run and contend for DRM mastership -- which is the exact failure attach mode
# was built to avoid, and it would present as a new bug rather than as a
# leftover file. The failure only appears on a *transition*, so nothing else
# would catch it.
set -uo pipefail
fail=0
check_absent() { [[ ! -e "$1" ]] || { echo "FAIL: $1 should have been removed"; fail=1; }; }
check_present() { [[ -e "$1" ]] || { echo "FAIL: $1 should exist"; fail=1; }; }

UNITS="${GHOSTFRAME_TEST_UNIT_DIR:?set to a throwaway unit dir}"

# Simulate a headless install having left its units behind.
mkdir -p "$UNITS"
touch "$UNITS/ghostframe-xorg.service" "$UNITS/ghostframe-wm.service" \
      "$UNITS/ghostframe.target" "$UNITS/ghostframe-xdaemon.service"

remove_other_mode_units attach "$UNITS"
check_absent  "$UNITS/ghostframe-xorg.service"
check_absent  "$UNITS/ghostframe-wm.service"
check_present "$UNITS/ghostframe-xdaemon.service"

# And the reverse direction.
touch "$UNITS/ghostframe-xorg.service" "$UNITS/ghostframe-wm.service"
remove_other_mode_units headless "$UNITS"
check_present "$UNITS/ghostframe-xorg.service"
check_present "$UNITS/ghostframe-wm.service"

[[ $fail -eq 0 ]] && echo "PASS"
exit $fail
```

The test calls `remove_other_mode_units <mode> <unit-dir>`, which does not exist yet. Source it from `install.sh` or factor it into a small sourceable file — **do not copy the logic into the test**, or the test verifies a copy rather than what ships.

- [ ] **Step 2: Run to verify failure**

```bash
GHOSTFRAME_TEST_UNIT_DIR=$(mktemp -d) bash tests/packaging/mode_switch_test.sh
```

Expected: fails — `remove_other_mode_units: command not found`.

- [ ] **Step 3: Implement**

```bash
# Remove the units belonging to the mode we are NOT installing.
#
# Load-bearing: if both modes' units remain, both X servers run and contend for
# DRM mastership -- the exact failure attach mode exists to avoid, and it
# presents as a new bug rather than as a leftover file.
#
# $2 (unit dir) is a parameter rather than a global so this is testable without
# root or a real user session -- see tests/packaging/mode_switch_test.sh.
remove_other_mode_units() {
  local target_mode="$1" unit_dir="$2" u
  case "$target_mode" in
    attach)
      for u in ghostframe-xorg.service ghostframe-wm.service ghostframe.target; do
        # `|| true` throughout: the unit may not exist, and the target user may
        # have no running session to talk to. Neither is an error -- we are
        # asserting an end state, not performing a transition.
        if [[ -e "$unit_dir/$u" ]]; then
          info "remove: $unit_dir/$u (not used in attach mode)"
          run_as_user systemctl --user stop "$u" 2>/dev/null || true
          run_as_user systemctl --user disable "$u" 2>/dev/null || true
          rm -f "$unit_dir/$u"
        fi
      done
      ;;
    headless)
      # Nothing to remove. The attach unit is installed under the same filename
      # (ghostframe-xdaemon.service) and is therefore overwritten in place by the
      # headless install. This branch exists so the symmetry is explicit --
      # without it a reader would reasonably assume it was unfinished.
      :
      ;;
    *) die "remove_other_mode_units: unknown mode '$target_mode'" ;;
  esac
}
```

`run_as_user` is whatever helper `install.sh` already uses to run `systemctl --user` as the target account -- find it and reuse it rather than inlining a `sudo -u ... XDG_RUNTIME_DIR=...` incantation. If none exists, add one; Task 4 needs it too for enabling the attach unit.

Call it from the install flow **before** installing the selected mode's units.

- [ ] **Step 4: Run and commit**

```bash
GHOSTFRAME_TEST_UNIT_DIR=$(mktemp -d) bash tests/packaging/mode_switch_test.sh
bash -n packaging/install.sh && echo "parses"
```

Expected: `PASS`.

```bash
git add packaging/install.sh tests/packaging/mode_switch_test.sh
git commit -m "packaging: remove the other mode's units when switching

If both modes' units stay installed, both X servers run and contend for DRM
mastership -- the exact failure attach mode exists to avoid, presenting as a
new bug rather than a leftover. The failure only appears on a transition, so
nothing else would catch it; hence a test."
```

---

## Task 6: Document the mode in the README

**Files:** Modify `README.md`

- [ ] **Step 1: Add an attach-mode section**

The README currently documents only the headless install, including the `options amdgpu virtual_display=<PCI_ID>,1` kernel requirement and a reboot. Add a section covering:

- when to choose attach over headless: ghostframe is the machine's primary workload and there is a local session whose GPU it would otherwise contend with;
- that it needs **no** kernel module option, no VKMS, and no reboot for a kernel parameter;
- the security position (§6), stated not implied;
- `GHOSTFRAME_ATTACH_MAX_RESOLUTION` and why it defaults to `1920x1080` — a remote client can otherwise resize the physical output beyond what a KVM can capture, removing the recovery view;
- the install command: `sudo ./packaging/install.sh <user> --mode attach`.

Match the README's existing voice. Read the surrounding sections first.

- [ ] **Step 2: Commit**

```bash
git add README.md
git commit -m "docs: document attach mode"
```

---

## Done means

- [ ] `GHOSTFRAME_ATTACH_MAX_RESOLUTION` lowers the ceiling, only ever lowers, accepts `none`, and ignores malformed input without refusing to start — with a test that fails when the clamp is removed.
- [ ] `install.sh --mode attach` installs one user unit plus lightdm autologin, and installs no second X server, WM, getty drop-in or `Xwrapper.config` loosening.
- [ ] Switching modes removes the other mode's units, proven by `tests/packaging/mode_switch_test.sh`.
- [ ] `install.sh --mode headless` is unchanged in behaviour.
- [ ] `--help` prints the full usage block, untruncated.
- [ ] The security position appears in the spec, the unit file, the install output and the README.
- [ ] The install output names `GHOSTFRAME_ATTACH_MAX_RESOLUTION` and its effective value, and says the `Xwrapper.config` setting is now unnecessary without removing it.
- [ ] `cargo test -p ghostframe-xdaemon`, `cargo fmt`, `cargo clippy -D warnings` all clean.
- [ ] What is untested is stated: capture from `:0`, autologin, lock-screen behaviour and whether the PiKVM holds its view across a restart all need the real machine.
