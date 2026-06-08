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

for arg in "$@"; do
  case "$arg" in
    --help|-h)
      sed -n '2,18p' "$0" | sed 's/^# \{0,1\}//'
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
    --help|-h) ;;
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
