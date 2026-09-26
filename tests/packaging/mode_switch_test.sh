#!/usr/bin/env bash
# Asserts that switching modes removes the other mode's units.
#
# Why this test exists: if both modes' units remain installed, both X servers
# run and contend for DRM mastership -- the exact failure attach mode was built
# to avoid, presenting as a new bug rather than as a leftover file. It only
# appears on a transition, so nothing else would catch it.
set -uo pipefail

# remove_other_mode_units (and its run_as_user helper) live in install.sh
# itself -- source the real file rather than copying its logic here, or this
# test would verify a copy instead of what ships. install.sh guards its own
# top-level code (root check, arg parsing, the install steps) behind a
# `(return 0 2>/dev/null); then return; fi` check so sourcing it only defines
# functions and does not execute any of that.
repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
# shellcheck source=/dev/null
source "$repo_root/packaging/install.sh"

fail=0
check_absent()  { [[ ! -e "$1" ]] || { echo "FAIL: $1 should have been removed"; fail=1; }; }
check_present() { [[   -e "$1" ]] || { echo "FAIL: $1 should exist"; fail=1; }; }

UNITS="${GHOSTFRAME_TEST_UNIT_DIR:?set to a throwaway unit dir}"
mkdir -p "$UNITS"

dropin="$UNITS/99-ghostframe-autologin.conf"
getty_dropin="$UNITS/99-ghostframe-getty.conf"

# ALWAYS call through this, never remove_other_mode_units directly: its 3rd and
# 4th arguments default to real paths under /etc, and an earlier version of this
# test omitted them and tried to delete this machine's actual getty drop-in. It
# survived only because the test does not run as root -- install.sh does.
switch_to() { remove_other_mode_units "$1" "$UNITS" "$dropin" "$getty_dropin"; }

# A headless install has left its units behind; we are now switching to attach.
touch "$UNITS/ghostframe-xorg.service" "$UNITS/ghostframe-wm.service" \
      "$UNITS/ghostframe.target" "$UNITS/ghostframe-xdaemon.service"

switch_to attach
check_absent  "$UNITS/ghostframe-xorg.service"
check_absent  "$UNITS/ghostframe-wm.service"
check_absent  "$UNITS/ghostframe.target"
check_present "$UNITS/ghostframe-xdaemon.service"

# And the reverse: switching to headless must not delete the headless units.
touch "$UNITS/ghostframe-xorg.service" "$UNITS/ghostframe-wm.service" \
      "$UNITS/ghostframe.target"
switch_to headless
check_present "$UNITS/ghostframe-xorg.service"
check_present "$UNITS/ghostframe-wm.service"
check_present "$UNITS/ghostframe.target"
check_present "$UNITS/ghostframe-xdaemon.service"

# Calling it twice must be safe -- install.sh may be re-run at any time.
switch_to attach
switch_to attach
check_absent "$UNITS/ghostframe-xorg.service"

# Switching to headless must remove attach's lightdm autologin drop-in.
# Leaving it means lightdm autologins the operator AND the headless getty
# autologins guest: two graphical sessions on one GPU, the exact contention
# this cleanup exists to prevent.
printf '[Seat:*]\nautologin-user=%s\nautologin-user-timeout=0\n' "$USER" > "$dropin"
switch_to headless
check_absent "$dropin"

# ...and it must be safe when the drop-in is not there.
switch_to headless

# A switch that also CHANGES the target user does not overwrite the attach unit
# in place -- it lands in the new user's directory while the old one keeps
# starting on the previous user's login. The drop-in records who that was.
# TEST_PREV_* and not prev_*: remove_other_mode_units declares `local prev_home`
# and `local prev_unit`, which would shadow these inside the stub below.
TEST_PREV_HOME="$UNITS/prevhome"
TEST_PREV_UNIT="$TEST_PREV_HOME/.config/systemd/user/ghostframe-xdaemon.service"
mkdir -p "$(dirname "$TEST_PREV_UNIT")"
touch "$TEST_PREV_UNIT"
printf '[Seat:*]\nautologin-user=%s\n' "$USER" > "$dropin"
# The cleanup resolves the previous user's home with getent. Stub it so the test
# does not depend on this machine's passwd database, and set target_user to
# someone else so $USER counts as the *previous* user.
target_user="definitely-not-$USER"
getent() { printf '%s:x:0:0::%s:/bin/sh\n' "$USER" "$TEST_PREV_HOME"; }
switch_to headless
unset -f getent
check_absent "$TEST_PREV_UNIT"
check_absent "$dropin"

# Same-user switch must NOT delete that user's unit -- the headless install
# overwrites it in place, and removing it here would race that write.
touch "$TEST_PREV_UNIT"
printf '[Seat:*]\nautologin-user=%s\n' "$USER" > "$dropin"
target_user="$USER"
getent() { printf '%s:x:0:0::%s:/bin/sh\n' "$USER" "$TEST_PREV_HOME"; }
switch_to headless
unset -f getent
check_present "$TEST_PREV_UNIT"

# --- headless -> attach, where the headless stack belongs to ANOTHER user ----
#
# This is the real-world case: headless installs under `guest`, attach installs
# under the operator's own account. The target user's unit dir is then EMPTY,
# so a cleanup that only looks there leaves guest's whole stack enabled and
# running -- a second daemon, a second tsnet node, and two X servers on one GPU.
# The getty autologin drop-in records who that user was.
GUEST_HOME="$UNITS/guesthome"
GUEST_DIR="$GUEST_HOME/.config/systemd/user"
mkdir -p "$GUEST_DIR"
for u in ghostframe-xorg.service ghostframe-wm.service ghostframe.target \
         ghostframe-xdaemon.service; do
  touch "$GUEST_DIR/$u"
done
printf '[Service]\nExecStart=\nExecStart=-/sbin/agetty --autologin %s --noclear %%I $TERM\n' \
  "$USER" > "$getty_dropin"

target_user="definitely-not-$USER"
user_uid=""
getent() { printf '%s:x:0:0::%s:/bin/sh\n' "$USER" "$GUEST_HOME"; }
switch_to attach
unset -f getent

for u in ghostframe-xorg.service ghostframe-wm.service ghostframe.target \
         ghostframe-xdaemon.service; do
  check_absent "$GUEST_DIR/$u"
done
check_absent "$getty_dropin"

# Same user: the xdaemon unit is overwritten in place by the install that
# follows, so removing it here would delete what is about to be rewritten.
mkdir -p "$GUEST_DIR"
touch "$GUEST_DIR/ghostframe-xdaemon.service"
printf '[Service]\nExecStart=-/sbin/agetty --autologin %s --noclear %%I $TERM\n' \
  "$USER" > "$getty_dropin"
target_user="$USER"
getent() { printf '%s:x:0:0::%s:/bin/sh\n' "$USER" "$GUEST_HOME"; }
switch_to attach
unset -f getent
check_present "$GUEST_DIR/ghostframe-xdaemon.service"

# --- require_lightdm's main-conf override warning -------------------------
#
# lightdm reads lightdm.conf AFTER lightdm.conf.d, so an uncommented
# autologin-user= there beats our drop-in and the machine autologins nobody.
# The install would report success and then serve nothing after a reboot.
target_user="somebody"
# Stub detection rather than the primitives it uses. The first version of this
# test overrode `readlink`/`systemctl`, which only works on a host that already
# has /etc/systemd/system/display-manager.service -- so it passed here and died
# on a CI runner with no display manager at all.
detect_display_manager() { echo "lightdm.service"; }

# The die paths first: they are the reason require_lightdm exists, and running
# them needs a subshell because die exits.
# `want` and not "$1": inside the nested function, $1 would be that function's
# own first argument (there is none), not dm_exit's.
dm_exit() {
  local want="$1"
  ( detect_display_manager() { echo "$want"; }; require_lightdm /dev/null >/dev/null 2>&1 )
  echo $?
}
[[ "$(dm_exit lightdm.service)" == 0 ]] || { echo "FAIL: lightdm must be accepted"; fail=1; }
[[ "$(dm_exit gdm.service)"     != 0 ]] || { echo "FAIL: gdm must be rejected"; fail=1; }
[[ "$(dm_exit sddm.service)"    != 0 ]] || { echo "FAIL: sddm must be rejected"; fail=1; }
[[ "$(dm_exit '')"              != 0 ]] || { echo "FAIL: no display manager must be rejected"; fail=1; }

main_conf="$UNITS/lightdm.conf"
warns() { require_lightdm "$main_conf" 2>&1 | grep -c "overrides the drop-in"; }

printf '[Seat:*]\n#autologin-user=someone\n' > "$main_conf"
[[ "$(warns)" == 0 ]] || { echo "FAIL: a commented autologin-user= must not warn"; fail=1; }

printf '[Seat:*]\nautologin-user=someone\n' > "$main_conf"
[[ "$(warns)" == 1 ]] || { echo "FAIL: an uncommented autologin-user= must warn"; fail=1; }

# Empty value still overrides -- that is the measured failure, not a no-op.
printf '[Seat:*]\nautologin-user=\n' > "$main_conf"
[[ "$(warns)" == 1 ]] || { echo "FAIL: an empty autologin-user= still overrides, must warn"; fail=1; }

rm -f "$main_conf"
[[ "$(warns)" == 0 ]] || { echo "FAIL: absent lightdm.conf must not warn"; fail=1; }
unset -f detect_display_manager

[[ $fail -eq 0 ]] && echo "PASS"
exit $fail
