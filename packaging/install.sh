#!/usr/bin/env bash
#
# packaging/install.sh — install ghostframe-xdaemon as a headless Xorg
# session for a specific user.
#
# Usage:  sudo ./packaging/install.sh <username> [--force]
#                                      [--display-backend amdgpu|vkms]
#                                      [--mode attach|headless]
#                                      [--max-resolution WIDTHxHEIGHT|none]
#
# --display-backend amdgpu (default): the GPU presents a virtual connector via
#   the `amdgpu virtual_display=` module option. Requires a machine with NO
#   local desktop -- a DRM device has one master at a time, so a Ghostframe X
#   server on the same GPU takes the display away from a local session.
# --display-backend vkms: run the session on VKMS, a separate virtual DRM
#   device. Safe alongside a local desktop; costs GPU acceleration inside the
#   Ghostframe session (VKMS has no render node). Requires the vkms module.
#
# --mode headless (default): installs ghostframe's own X server
#   (ghostframe-xorg), a window manager (ghostframe-wm), and the capture
#   daemon as three units, plus a sandboxed 'guest' user and a getty
#   autologin. Existing installs are unaffected by adding this flag.
# --mode attach: installs a single unit that captures the operator's own,
#   already-running X session instead. The remote client sees the operator's
#   real desktop, not a sandboxed guest session. Also configures lightdm to
#   autologin that session on boot -- attach mode has nothing to capture
#   otherwise. Requires lightdm as the display manager; install.sh dies with
#   a clear message if another one (gdm, sddm, ...) is detected instead.
#
# --max-resolution WIDTHxHEIGHT|none (default: 1920x1080): caps how large a
#   remote client may resize the captured session. Only meaningful in
#   --mode attach, where the session *is* the physical display -- which a KVM
#   may be capturing as the machine's recovery view, so an unclamped client
#   could set a mode that view cannot capture. Only checked here for being
#   non-empty: ghostframe-xdaemon/src/display.rs's clamp_ceiling already
#   ignores a malformed value at runtime without refusing to start, and
#   duplicating that rule in bash would just give it a second place to
#   disagree -- do not "helpfully" add stricter validation here.
#
# What it does:
#   1. Verifies required binaries are on the host (Xorg, amdgpu, enlightenment).
#   2. Installs /usr/local/bin/ghostframe-xdaemon from ./target/release/.
#   3. Installs /etc/X11/ghostframe-headless.conf from packaging/.
#   4. Ensures /etc/X11/Xwrapper.config lets non-console users launch X.
#      Writes the file only when absent. If it exists, the file is left
#      untouched and any missing keys are reported for manual merge.
#   5. Installs ~user/.config/systemd/user/{ghostframe*.{service,target}}
#   6. Installs /etc/systemd/system/getty@tty1.service.d/99-ghostframe-autologin.conf
#   7. (Interactive only) prompts for TS_AUTHKEY and seeds the tsnet state dir.
#   8. Enables ghostframe.target (user) and getty@tty1 (system).
#
# In --mode attach, steps 3, 4 and 6 are skipped entirely (no ghostframe-owned
# Xorg server, so no config and no Xwrapper.config loosening); step 5 installs
# one unit (ghostframe-xdaemon.service, attach flavor) instead of three; step 6
# installs a lightdm autologin drop-in instead of the getty one; and step 8
# enables ghostframe-xdaemon.service directly, not ghostframe.target or getty.
#
# Re-running is idempotent except for step 7, which is skipped if the state dir
# is already populated. Pass --force to overwrite the binary. The Xorg config
# (step 3) and the systemd units (step 6) are package-owned, not meant for
# local edits, and are always reinstalled on every run — so packaging
# changes (e.g. a framebuffer size bump) reach an existing install without
# needing --force.
# --force does NOT modify a pre-existing /etc/X11/Xwrapper.config — that file
# is shared with the host and is left alone unconditionally.

set -euo pipefail

die() { printf 'install.sh: %s\n' "$*" >&2; exit 1; }
info() { printf 'install.sh: %s\n' "$*"; }

# Run a command as $1 (a username) inside their systemd --user context ($2 =
# their uid, used to point XDG_RUNTIME_DIR at the right place). Factored out
# of the enable step (step 9, below) so remove_other_mode_units can reach the
# target user's session the exact same way instead of re-deriving it.
run_as_user() {
  local user="$1" uid="$2"; shift 2
  sudo -u "$user" XDG_RUNTIME_DIR="/run/user/$uid" "$@"
}

# Remove the units belonging to the mode we are NOT installing.
#
# Load-bearing: if both modes' units remain installed, both X servers run and
# contend for DRM mastership -- the exact failure attach mode exists to
# avoid, presenting as a new bug rather than as a leftover file. It only
# appears on a transition between modes, so nothing else would catch it.
#
# $2 (unit dir) is a parameter rather than a global so this is testable
# without root or a real user session -- see
# tests/packaging/mode_switch_test.sh. $target_user/$user_uid are read as
# globals rather than parameters: they are unset when this function is
# sourced for that test, so every use of them below is guarded to tolerate
# that (`${target_user:-}` and a plain existence check) rather than require it.
remove_other_mode_units() {
  local target_mode="$1" unit_dir="$2" u
  case "$target_mode" in
    attach)
      for u in ghostframe-xorg.service ghostframe-wm.service ghostframe.target; do
        if [[ -e "$unit_dir/$u" ]]; then
          info "remove: $unit_dir/$u (not used in attach mode)"
          # `|| true` throughout: the unit may already be stopped/disabled,
          # and the target user may have no running session to talk to (or,
          # under test, no $target_user at all). Neither is an error -- we
          # are asserting an end state, not performing a transition.
          if [[ -n "${target_user:-}" ]]; then
            run_as_user "${target_user:-}" "${user_uid:-}" systemctl --user stop "$u" 2>/dev/null || true
            run_as_user "${target_user:-}" "${user_uid:-}" systemctl --user disable "$u" 2>/dev/null || true
          fi
          rm -f "$unit_dir/$u"
        fi
      done
      ;;
    headless)
      # Nothing to remove. The attach unit is installed under the same
      # filename (ghostframe-xdaemon.service) and is therefore overwritten in
      # place by the headless install below. This branch exists so the
      # symmetry is explicit -- without it a reader would reasonably assume
      # it was unfinished.
      :
      ;;
    *) die "remove_other_mode_units: unknown mode '$target_mode'" ;;
  esac
}

# Allow this file to be sourced (instead of executed) so the functions above
# can be unit tested without root, a target user, or any of the top-level
# side effects below -- see tests/packaging/mode_switch_test.sh. When
# sourced, stop right here, before the root check, argument parsing, and
# every destructive step that follows.
if (return 0 2>/dev/null); then
  return
fi

for arg in "$@"; do
  case "$arg" in
    --help|-h)
      # Print the whole header comment block: every line after the shebang
      # until the first line that is not a comment.
      #
      # Deliberately NOT a hardcoded line range. This was `2,27p`, then
      # `2,36p`, and both went stale as flags were added -- the second time
      # silently dropping the entire "What it does:" list, with nothing to
      # notice. A range that has to be recounted by hand on every edit is the
      # wrong mechanism, not a number that keeps being got wrong.
      awk 'NR > 1 { if ($0 !~ /^#/) exit; print }' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
  esac
done

[[ ${EUID} -eq 0 ]] || die "must be run as root (try: sudo $0 $*)"

force=0
display_backend="amdgpu"
mode="headless"
max_resolution="1920x1080"
target_user=""
expect_backend=0
expect_mode=0
expect_max_resolution=0
for arg in "$@"; do
  if [[ $expect_backend -eq 1 ]]; then
    case "$arg" in
      amdgpu|vkms) display_backend="$arg" ;;
      *) die "--display-backend must be 'amdgpu' or 'vkms' (got '$arg')" ;;
    esac
    expect_backend=0
    continue
  fi
  if [[ $expect_mode -eq 1 ]]; then
    case "$arg" in
      attach|headless) mode="$arg" ;;
      *) die "--mode must be 'attach' or 'headless' (got '$arg')" ;;
    esac
    expect_mode=0
    continue
  fi
  if [[ $expect_max_resolution -eq 1 ]]; then
    [[ -n "$arg" ]] || die "--max-resolution requires a value: WIDTHxHEIGHT or none"
    max_resolution="$arg"
    expect_max_resolution=0
    continue
  fi
  case "$arg" in
    --force) force=1 ;;
    --display-backend) expect_backend=1 ;;
    --display-backend=*)
      case "${arg#*=}" in
        amdgpu|vkms) display_backend="${arg#*=}" ;;
        *) die "--display-backend must be 'amdgpu' or 'vkms' (got '${arg#*=}')" ;;
      esac
      ;;
    --mode) expect_mode=1 ;;
    --mode=*)
      case "${arg#*=}" in
        attach|headless) mode="${arg#*=}" ;;
        *) die "--mode must be 'attach' or 'headless' (got '${arg#*=}')" ;;
      esac
      ;;
    --max-resolution) expect_max_resolution=1 ;;
    --max-resolution=*)
      # Only checked for non-emptiness -- see the usage comment above for why
      # geometry validation belongs solely to clamp_ceiling at runtime, not
      # here.
      [[ -n "${arg#*=}" ]] || die "--max-resolution requires a value: WIDTHxHEIGHT or none"
      max_resolution="${arg#*=}"
      ;;
    --help|-h) ;;
    -*) die "unknown flag: $arg" ;;
    *)
      [[ -z "$target_user" ]] || die "only one username allowed (got '$target_user' and '$arg')"
      target_user="$arg"
      ;;
  esac
done

# A trailing bare `--display-backend` (or `--mode` / `--max-resolution`) would
# otherwise leave the default in place silently, which is the wrong outcome
# for a flag whose whole purpose is to pick a non-default.
[[ $expect_backend -eq 0 ]] || die "--display-backend requires a value: amdgpu or vkms"
[[ $expect_mode -eq 0 ]] || die "--mode requires a value: attach or headless"
[[ $expect_max_resolution -eq 0 ]] || die "--max-resolution requires a value: WIDTHxHEIGHT or none"

[[ -n "$target_user" ]] || die "username required (try: sudo $0 <username>)"
id "$target_user" >/dev/null 2>&1 || die "user '$target_user' does not exist"

user_uid=$(id -u "$target_user")
user_gid=$(id -g "$target_user")
user_home=$(getent passwd "$target_user" | cut -d: -f6)
[[ -d "$user_home" ]] || die "home directory '$user_home' for '$target_user' does not exist"

repo_root=$(cd "$(dirname "$0")/.." && pwd)
pkg_dir="$repo_root/packaging"
if [[ "$mode" == "headless" ]]; then
  [[ -f "$pkg_dir/xorg-headless-amdgpu.conf" ]] || die "missing $pkg_dir/xorg-headless-amdgpu.conf"
  [[ -f "$pkg_dir/xorg-headless-vkms.conf.tmpl" ]] || die "missing $pkg_dir/xorg-headless-vkms.conf.tmpl"
else
  [[ -f "$pkg_dir/systemd/ghostframe-xdaemon-attach.service.tmpl" ]] || die "missing $pkg_dir/systemd/ghostframe-xdaemon-attach.service.tmpl"
  [[ -f "$pkg_dir/lightdm-autologin.conf.tmpl" ]] || die "missing $pkg_dir/lightdm-autologin.conf.tmpl"
fi

# 1. Preflight.
info "preflight: checking required binaries..."
if [[ "$mode" == "headless" ]]; then
  need_bins=(Xorg enlightenment_start)
else
  # Attach mode captures the operator's already-running session: no
  # ghostframe-owned X server and no window manager to check for.
  need_bins=()
  info "attach mode: no Xorg/window-manager binaries required (captures the existing session)."
fi
for b in "${need_bins[@]}"; do
  command -v "$b" >/dev/null 2>&1 || die "missing '$b' on PATH — install it and retry"
done

if [[ "$mode" == "attach" ]]; then
  # $target_user must be the account whose desktop attach mode will capture.
  # Warn, don't die: if lightdm autologin (installed later in this same run)
  # hasn't taken effect yet -- e.g. this is a fresh install, pre-first-boot --
  # the session legitimately doesn't exist yet. The point is to catch
  # --mode attach pointed at the wrong account now, instead of failing
  # opaquely at connect time later.
  session_display=$(loginctl show-user "$target_user" -p Display --value 2>/dev/null || true)
  if [[ -z "$session_display" ]]; then
    info "warn: '$target_user' has no active graphical session right now (loginctl show-user -p Display is empty)."
    info "      If lightdm autologin hasn't run yet (fresh install, pre-reboot), this is expected -- ignore it."
    info "      Otherwise, double check '$target_user' is the account whose desktop should be captured."
  else
    info "ok: '$target_user' has an active graphical session (loginctl Display=$session_display)."
  fi
fi

# Group membership: 'video' for KMS framebuffer + DRI, 'render' for
# render-node ioctls. Without these, ghostframe-xdaemon's DRM capture path
# can't open /dev/dri/* and silently falls back to X11 GetImage — which is
# the CPU-copy slow path responsible for ~25% CPU at 30fps idle.
for grp in video render; do
  if ! getent group "$grp" >/dev/null 2>&1; then
    info "warn: group '$grp' does not exist on this host — skipping (DRM capture may fall back to X11)"
    continue
  fi
  if id -nG "$target_user" | tr ' ' '\n' | grep -qx "$grp"; then
    info "ok: $target_user already in group '$grp'"
  else
    info "usermod: adding $target_user to group '$grp'"
    usermod -aG "$grp" "$target_user"
  fi
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

if [[ "$mode" == "attach" ]]; then
  # 3/4. Attach mode has no ghostframe-owned X server -- it captures the
  # operator's own, already-running session -- so neither the headless Xorg
  # config nor the Xwrapper.config loosening (which exists solely so a user
  # unit can launch Xorg) is needed. Skip both entirely.
  info "mode attach: skipping Xorg config and Xwrapper.config (no ghostframe-owned X server in this mode)"

  xwrap_dst="/etc/X11/Xwrapper.config"
  if [[ -f "$xwrap_dst" ]] && grep -q "Installed by ghostframe packaging/install.sh" "$xwrap_dst"; then
    info "note: $xwrap_dst was installed by a previous ghostframe (headless) install."
    info "      It is not needed in attach mode -- leaving it in place untouched, since"
    info "      this script cannot know whether anything else on this host depends on it."
  fi
else

# 3. Xorg config.
#
# Always reinstalled, unconditionally — no --force needed. This file is
# package-owned, not a place for local edits, and step 6 (systemd units)
# already follows this policy below. Previously this step skipped when the
# file already existed, which meant an upgrade that changes this config
# (e.g. M4b's framebuffer size bump) silently kept the old file on any
# existing install and made the new feature look broken.
xorg_dst="/etc/X11/ghostframe-headless.conf"
info "install: $xorg_dst  (backend: $display_backend)"
if [[ "$display_backend" == "vkms" ]]; then
  # VKMS is a virtual device: it has no /dev/dri/by-path entry and DRM card
  # numbering is not stable across boots, so the kmsdev path cannot be
  # hardcoded in the config. Identify it by the device directory's name --
  # /sys/class/drm/cardN/device resolves to .../vkms for the virtual device
  # and to a PCI address for a real GPU.
  vkms_card=""
  for c in /dev/dri/card*; do
    [[ -c "$c" ]] || continue
    dev_dir=$(readlink -f "/sys/class/drm/$(basename "$c")/device" 2>/dev/null) || continue
    if [[ "$(basename "$dev_dir")" == "vkms" ]]; then
      vkms_card="$c"
      break
    fi
  done
  if [[ -z "$vkms_card" ]]; then
    die "--display-backend vkms: no VKMS DRM device found.
     Load the module first:
       echo vkms | sudo tee /etc/modules-load.d/vkms.conf
       echo 'options vkms enable_writeback=1' | sudo tee /etc/modprobe.d/vkms.conf
       sudo modprobe vkms
     Then confirm:  ls /sys/class/drm/ | grep Virtual"
  fi
  info "       VKMS detected at $vkms_card"
  sed "s|__KMSDEV__|$vkms_card|g" "$pkg_dir/xorg-headless-vkms.conf.tmpl" \
    | install -D -m 0644 -o root -g root /dev/stdin "$xorg_dst"
else
  install -D -m 0644 -o root -g root "$pkg_dir/xorg-headless-amdgpu.conf" "$xorg_dst"
fi

# 4. Xwrapper policy.
#
# Xorg.wrap's default policy is 'console-only': it refuses to launch X for a
# user that is not currently logged in on a console VT. ghostframe runs Xorg
# from a systemd --user service, so the target user is never a console user
# and every start attempt fails with:
#     /usr/lib/Xorg.wrap: Only console users are allowed to run the X server
# We need:
#   allowed_users=anybody     — permit non-console users.
#   needs_root_rights=yes     — the amdgpu Xorg config in step 3 needs DRM
#                               master + root, which Xorg.wrap drops by default
#                               once allowed_users=anybody is set.
xwrap_dst="/etc/X11/Xwrapper.config"
xwrap_required=("allowed_users=anybody" "needs_root_rights=yes")
xwrap_has_key() {
  # $1 = file, $2 = "key=value"
  local key="${2%%=*}" val="${2#*=}"
  grep -Eq "^[[:space:]]*${key}[[:space:]]*=[[:space:]]*${val}[[:space:]]*(#.*)?$" "$1"
}
if [[ ! -e "$xwrap_dst" ]]; then
  info "install: $xwrap_dst (allow non-console users to launch X)"
  install -D -m 0644 -o root -g root /dev/stdin "$xwrap_dst" <<EOF
# Installed by ghostframe packaging/install.sh.
#
# ghostframe runs Xorg from a systemd --user service. Without these settings
# Xorg.wrap refuses to start X for any non-console user and the headless
# session restart-loops until ghostframe.target collapses.
${xwrap_required[0]}
${xwrap_required[1]}
EOF
else
  xwrap_missing=()
  for line in "${xwrap_required[@]}"; do
    xwrap_has_key "$xwrap_dst" "$line" || xwrap_missing+=("$line")
  done
  if [[ ${#xwrap_missing[@]} -eq 0 ]]; then
    info "ok: $xwrap_dst already permits non-console users — skip"
  else
    info "warn: $xwrap_dst exists but is missing ghostframe-required setting(s)."
    info "      install.sh will NOT modify a pre-existing Xwrapper.config."
    info "      Without these, Xorg.wrap refuses to launch X for $target_user and"
    info "      ghostframe-xorg.service will restart-loop until the target collapses."
    info "      Add the following line(s) manually, then re-run this script:"
    for line in "${xwrap_missing[@]}"; do
      info "          $line"
    done
  fi
fi

fi # mode == headless (steps 3/4)

# 5. State dir.
state_dir="$user_home/.local/share/ghostframe/ts-state"
if [[ ! -d "$state_dir" ]]; then
  info "create: $state_dir"
  install -d -m 0700 -o "$user_uid" -g "$user_gid" "$state_dir"
fi

# 6. User units.
user_units_dir="$user_home/.config/systemd/user"
install -d -m 0755 -o "$user_uid" -g "$user_gid" "$user_units_dir"

# Remove whichever mode's units we are NOT installing, before installing the
# selected mode's units below -- see remove_other_mode_units's own comment
# for why this matters.
remove_other_mode_units "$mode" "$user_units_dir"

if [[ "$mode" == "attach" ]]; then
  xdaemon_unit_dst="$user_units_dir/ghostframe-xdaemon.service"
  info "install: $xdaemon_unit_dst  (attach mode)"
  sed "s|__MAX_RESOLUTION__|$max_resolution|g" "$pkg_dir/systemd/ghostframe-xdaemon-attach.service.tmpl" \
    | install -m 0644 -o "$user_uid" -g "$user_gid" /dev/stdin "$xdaemon_unit_dst"
  info "GHOSTFRAME_ATTACH_MAX_RESOLUTION=$max_resolution (set in $xdaemon_unit_dst)"
else
  for u in ghostframe.target ghostframe-xorg.service ghostframe-wm.service ghostframe-xdaemon.service; do
    info "install: $user_units_dir/$u"
    install -m 0644 -o "$user_uid" -g "$user_gid" "$pkg_dir/systemd/$u" "$user_units_dir/$u"
  done
fi

if [[ "$mode" == "attach" ]]; then
  # 7. lightdm autologin drop-in (replaces the getty autologin used in
  # headless mode). Attach mode captures an *existing* X session -- so that
  # session must exist after an unattended boot, or this machine serves
  # nothing. Autologin creates it.
  dm_unit=""
  if [[ -L /etc/systemd/system/display-manager.service ]]; then
    dm_unit=$(basename "$(readlink -f /etc/systemd/system/display-manager.service)")
  else
    for cand in lightdm gdm gdm3 sddm; do
      if systemctl is-active --quiet "$cand.service" 2>/dev/null \
        || systemctl is-enabled --quiet "$cand.service" 2>/dev/null; then
        dm_unit="$cand.service"
        break
      fi
    done
  fi
  case "$dm_unit" in
    lightdm.service) ;;
    "")
      die "attach mode: could not detect a display manager (checked the" \
          "display-manager.service alias and lightdm/gdm/gdm3/sddm units)." \
          "Attach mode currently only automates lightdm's autologin -- install" \
          "lightdm, or configure autologin for your display manager yourself" \
          "so :0 exists after an unattended boot, then re-run."
      ;;
    *)
      die "attach mode: detected display manager '$dm_unit', but attach mode" \
          "currently only automates lightdm's autologin. Configure autologin" \
          "for '$dm_unit' yourself so :0 exists after an unattended boot" \
          "(this is not optional -- without it the machine serves nothing" \
          "after a reboot), or switch the host to lightdm and re-run."
      ;;
  esac

  lightdm_dropin_dir="/etc/lightdm/lightdm.conf.d"
  if [[ -d "$lightdm_dropin_dir" ]]; then
    info "found: $lightdm_dropin_dir"
  else
    info "create: $lightdm_dropin_dir (did not exist; lightdm reads *.conf here automatically)"
    install -d -m 0755 "$lightdm_dropin_dir"
  fi

  # Warn (but don't abort) if another autologin drop-in exists.
  shopt -s nullglob
  existing=("$lightdm_dropin_dir"/*.conf)
  shopt -u nullglob
  for f in "${existing[@]}"; do
    base=$(basename "$f")
    [[ "$base" == "99-ghostframe-autologin.conf" ]] && continue
    if grep -q autologin "$f" 2>/dev/null; then
      info "warn: existing autologin config at $f — ours uses the 99- prefix and will win, but verify"
    fi
  done

  lightdm_dst="$lightdm_dropin_dir/99-ghostframe-autologin.conf"
  info "install: $lightdm_dst"
  sed "s|__USER__|$target_user|g" "$pkg_dir/lightdm-autologin.conf.tmpl" \
    | install -m 0644 -o root -g root /dev/stdin "$lightdm_dst"
else
  # 7. getty autologin drop-in.
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
  sed "s|__USER__|$target_user|g" "$pkg_dir/systemd/getty-autologin.conf.tmpl" \
    | install -m 0644 -o root -g root /dev/stdin "$drop_dst"
fi

# 8. Tsnet seed.
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

# 9. Enable.
info "systemctl daemon-reload"
systemctl daemon-reload

if [[ "$mode" == "attach" ]]; then
  enable_unit="ghostframe-xdaemon.service"
else
  enable_unit="ghostframe.target"
fi

info "enabling $enable_unit for user $target_user..."
# The target user's --user manager may not be running. Use `--global` to
# enable for the user, then their next login starts it. Equivalent to running
# `systemctl --user enable` inside that user's session.
{
  run_as_user "$target_user" "$user_uid" systemctl --user enable "$enable_unit" 2>/dev/null \
    || run_as_user "$target_user" "$user_uid" systemctl --user --no-block enable "$enable_unit"
} || {
  info "warn: 'systemctl --user enable $enable_unit' failed for user $target_user."
  info "      It should still autostart on first login/session via its own WantedBy=,"
  info "      but if it does not, run after logging in as that user:"
  info "          systemctl --user enable $enable_unit"
}

if [[ "$mode" == "headless" ]]; then
  info "enabling getty@tty1..."
  systemctl enable getty@tty1.service
fi

info ""
info "installation complete."
if [[ "$mode" == "attach" ]]; then
  info ""
  info "ATTACH MODE -- SECURITY: a client that reaches the tailnet endpoint gets"
  info "$target_user's own desktop -- their files, their browser sessions, their"
  info "sudo -- not a sandboxed guest session. The tailnet is the only boundary."
  info "GHOSTFRAME_ATTACH_MAX_RESOLUTION=$max_resolution (set in $user_units_dir/ghostframe-xdaemon.service)."
  info "To change it later, edit that unit's Environment= line and run:"
  info "  systemctl --user daemon-reload && systemctl --user restart ghostframe-xdaemon.service"
fi
if [[ "$seeded" -eq 1 ]]; then
  info "Reboot the machine. Then, on any tailnet device, open:"
  info "  https://$(hostname)-ghostframe.<tailnet>.ts.net/"
  info ""
  info "If the URL fails to load, enable HTTPS Certificates in your tailnet at"
  info "  https://login.tailscale.com/admin/dns"
  info "and refresh."
else
  info "Run the --init command above, then reboot."
fi
