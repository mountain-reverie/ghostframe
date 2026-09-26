#!/usr/bin/env bash
#
# packaging/install.sh — install ghostframe-xdaemon for a specific user, either
# as a headless Xorg session of its own (--mode headless, the default) or
# attached to that user's existing session (--mode attach).
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
# In --mode attach, relative to the numbered list above:
#   - steps 3 and 4 are skipped entirely (no ghostframe-owned Xorg server, so
#     no config and no Xwrapper.config loosening);
#   - step 5 installs one unit (ghostframe-xdaemon.service, attach flavour)
#     instead of three;
#   - step 6 installs a lightdm autologin drop-in instead of the getty one --
#     replaced, not skipped;
#   - step 8 enables ghostframe-xdaemon.service directly, not ghostframe.target
#     or getty.
#
# Note the step comments in the code below number themselves differently (they
# count the tsnet state dir as 5), so "step N" here always means the list above.
#
# Re-running is idempotent except for step 7, which is skipped if the state dir
# is already populated. Pass --force to overwrite the binary. The Xorg config
# (step 3) and the systemd units (step 5) are package-owned, not meant for
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
# Attach mode needs lightdm, because lightdm's autologin is the only one it
# automates. This MUST run in preflight, not at the install step that uses it:
# the attach install overwrites ghostframe-xdaemon.service, so dying halfway
# through leaves a previously-working headless install with an attach unit
# (DISPLAY=:0, PartOf=graphical-session.target) and no headless stack behind it.
# It is a pure precondition; it belongs with the other probing.
# Echoes the active display manager's unit name, or nothing if none is found.
# Split out from require_lightdm so tests can stub it: otherwise the checks
# below can only be exercised on a host that already runs the display manager
# under test, which is how the first version of this passed locally (evangeline
# runs lightdm) and died on a CI runner that has no display manager at all.
detect_display_manager() {
  if [[ -L /etc/systemd/system/display-manager.service ]]; then
    basename "$(readlink -f /etc/systemd/system/display-manager.service)"
    return
  fi
  local cand
  for cand in lightdm gdm gdm3 sddm; do
    if systemctl is-active --quiet "$cand.service" 2>/dev/null \
      || systemctl is-enabled --quiet "$cand.service" 2>/dev/null; then
      echo "$cand.service"
      return
    fi
  done
}

require_lightdm() {
  local dm_unit
  dm_unit=$(detect_display_manager)
  case "$dm_unit" in
    lightdm.service) info "ok: display manager is lightdm." ;;
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

  # lightdm reads every lightdm.conf.d directory FIRST and lightdm.conf LAST,
  # so an uncommented autologin-user= in the main file silently beats our
  # drop-in. Measured: main conf with an empty `autologin-user=` wins, and the
  # machine autologins nobody -- the install reports success and then serves
  # nothing after a reboot, which is the very failure the check above exists to
  # prevent, reached through a different door. Warn rather than die: the
  # operator may want their own value to win, and we cannot tell.
  local main_conf="${1:-/etc/lightdm/lightdm.conf}"
  if [[ -f "$main_conf" ]] \
    && grep -qE '^[[:space:]]*autologin-user[[:space:]]*=' "$main_conf"; then
    info "warn: $main_conf sets autologin-user= and is read AFTER lightdm.conf.d,"
    info "      so it overrides the drop-in this script installs. Comment it out,"
    info "      or set it to '$target_user' there, or autologin will not work."
  fi
}

# remove_other_mode_units <mode> <unit_dir> [lightdm_dropin]
#
# The third argument exists so the packaging test can point the lightdm cleanup
# at a throwaway path instead of /etc.
# Stop, disable and delete $units (a space-separated list) from $dir, running
# the systemctl calls as $user. `|| true` throughout: a unit may already be
# stopped or disabled, and the user may have no running session to talk to.
# We are asserting an end state, not performing a transition.
purge_units() {
  local user="$1" uid="$2" dir="$3" why="$4" u
  shift 4
  for u in "$@"; do
    [[ -e "$dir/$u" ]] || continue
    info "remove: $dir/$u ($why)"
    if [[ -n "$user" ]]; then
      run_as_user "$user" "$uid" systemctl --user stop "$u" 2>/dev/null || true
      run_as_user "$user" "$uid" systemctl --user disable "$u" 2>/dev/null || true
    fi
    rm -f "$dir/$u"
  done
}

remove_other_mode_units() {
  local target_mode="$1" unit_dir="$2"
  local lightdm_dropin="${3:-/etc/lightdm/lightdm.conf.d/99-ghostframe-autologin.conf}"
  local getty_dropin="${4:-/etc/systemd/system/getty@tty1.service.d/99-ghostframe-autologin.conf}"
  case "$target_mode" in
    attach)
      purge_units "${target_user:-}" "${user_uid:-}" "$unit_dir" "not used in attach mode" \
        ghostframe-xorg.service ghostframe-wm.service ghostframe.target

      # The headless stack usually belongs to a DIFFERENT user -- `guest` is
      # the documented default -- in which case none of the above is in
      # $unit_dir at all and the entire headless stack keeps running: a second
      # daemon, a second tsnet node, and two X servers on one GPU, which is the
      # contention attach mode exists to avoid. The getty autologin drop-in
      # records who that user was, mirroring how the lightdm drop-in records
      # the attach user for the reverse switch.
      #
      # ghostframe-xdaemon.service is in this list and not the one above: for
      # the same user it is overwritten in place by the install that follows,
      # but a different user's copy would keep running.
      if [[ -f "$getty_dropin" ]]; then
        local prev_user prev_home prev_uid
        prev_user=$(sed -n 's/.*--autologin[[:space:]]\{1,\}\([^[:space:]]\{1,\}\).*/\1/p' \
          "$getty_dropin" | head -1)
        if [[ -n "$prev_user" && "$prev_user" != "${target_user:-}" ]]; then
          prev_home=$(getent passwd "$prev_user" 2>/dev/null | cut -d: -f6)
          prev_uid=$(id -u "$prev_user" 2>/dev/null || true)
          if [[ -n "$prev_home" ]]; then
            purge_units "$prev_user" "$prev_uid" "$prev_home/.config/systemd/user" \
              "headless unit for '$prev_user', unused in attach mode" \
              ghostframe-xorg.service ghostframe-wm.service ghostframe.target \
              ghostframe-xdaemon.service
          fi
        fi
        # Removing this leaves tty1 a NORMAL login getty rather than one that
        # autologins the headless account. getty@tty1 itself is deliberately
        # left enabled: a console login is useful and not ours to take away.
        info "remove: $getty_dropin (autologin is not used in attach mode)"
        rm -f "$getty_dropin"
      fi

      # A PREVIOUS ATTACH install may also have belonged to another user --
      # re-pointing attach mode from one account to another is a normal thing to
      # do, and the getty drop-in above cannot help because the first attach
      # install already deleted it. The lightdm drop-in records that user. We do
      # not remove the drop-in here: the install that follows overwrites it with
      # the new user.
      if [[ -f "$lightdm_dropin" ]]; then
        local prev_attach prev_attach_home
        prev_attach=$(sed -n 's/^[[:space:]]*autologin-user[[:space:]]*=[[:space:]]*//p' \
          "$lightdm_dropin" | head -1)
        if [[ -n "$prev_attach" && "$prev_attach" != "${target_user:-}" ]]; then
          prev_attach_home=$(getent passwd "$prev_attach" 2>/dev/null | cut -d: -f6)
          if [[ -n "$prev_attach_home" ]]; then
            purge_units "$prev_attach" "$(id -u "$prev_attach" 2>/dev/null || true)" \
              "$prev_attach_home/.config/systemd/user" \
              "attach unit for '$prev_attach', replaced by '${target_user:-}'" \
              ghostframe-xdaemon.service
          fi
        fi
      fi
      ;;
    headless)
      # The attach unit uses the same filename (ghostframe-xdaemon.service), so
      # a switch that keeps the same target user overwrites it in place and
      # there is nothing to do. Two things do NOT clean themselves up:
      #
      # 1. lightdm autologin. Attach mode installs it so :0 exists after an
      #    unattended boot. Headless mode has its own getty autologin for the
      #    guest account, so leaving this one behind means BOTH autologin --
      #    two graphical sessions contending for one GPU, which is precisely
      #    the failure this cleanup exists to prevent.
      #
      # 2. A switch that also changes the target user. The attach unit then
      #    stays in the *other* user's directory and keeps starting on their
      #    login. The lightdm drop-in records who that user was, which is how
      #    we find them.
      if [[ -f "$lightdm_dropin" ]]; then
        local prev_user prev_home prev_unit
        prev_user=$(sed -n 's/^[[:space:]]*autologin-user[[:space:]]*=[[:space:]]*//p' \
          "$lightdm_dropin" | head -1)
        if [[ -n "$prev_user" && "$prev_user" != "${target_user:-}" ]]; then
          prev_home=$(getent passwd "$prev_user" 2>/dev/null | cut -d: -f6)
          prev_unit="$prev_home/.config/systemd/user/ghostframe-xdaemon.service"
          if [[ -n "$prev_home" && -e "$prev_unit" ]]; then
            info "remove: $prev_unit (attach unit for '$prev_user', unused in headless mode)"
            rm -f "$prev_unit"
          fi
        fi
        info "remove: $lightdm_dropin (autologin is not used in headless mode)"
        rm -f "$lightdm_dropin"
      fi
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
  require_lightdm

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
  # Name the path before prompting. Without this the only symptom of a state dir
  # in the wrong place is being asked for a key you should not have needed, with
  # nothing on screen to say where the script looked -- and answering it
  # registers a SECOND node that then competes for the same hostname.
  info "no tailscaled.state found at:"
  info "  $state_dir"
  info "so this will register a NEW tailnet node named $(hostname)-ghostframe."
  info "If you moved an existing state dir here, stop and check that path first."
  info "(A nested ts-state/ts-state/ is the usual cause: 'mv src dst' puts src"
  info " INSIDE dst when dst already exists.)"
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
  # Name the concrete target rather than a bare "its own WantedBy=": the old
  # headless message said default.target, and dropping that made the warning
  # unactionable. The two modes genuinely differ, so branch.
  if [[ "$mode" == "attach" ]]; then
    info "      It should still autostart via WantedBy=graphical-session.target,"
  else
    info "      It should still autostart on first boot via WantedBy=default.target,"
  fi
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
