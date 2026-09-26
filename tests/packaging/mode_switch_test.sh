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

# A headless install has left its units behind; we are now switching to attach.
touch "$UNITS/ghostframe-xorg.service" "$UNITS/ghostframe-wm.service" \
      "$UNITS/ghostframe.target" "$UNITS/ghostframe-xdaemon.service"

remove_other_mode_units attach "$UNITS"
check_absent  "$UNITS/ghostframe-xorg.service"
check_absent  "$UNITS/ghostframe-wm.service"
check_absent  "$UNITS/ghostframe.target"
check_present "$UNITS/ghostframe-xdaemon.service"

# And the reverse: switching to headless must not delete the headless units.
touch "$UNITS/ghostframe-xorg.service" "$UNITS/ghostframe-wm.service" \
      "$UNITS/ghostframe.target"
remove_other_mode_units headless "$UNITS"
check_present "$UNITS/ghostframe-xorg.service"
check_present "$UNITS/ghostframe-wm.service"
check_present "$UNITS/ghostframe.target"
check_present "$UNITS/ghostframe-xdaemon.service"

# Calling it twice must be safe -- install.sh may be re-run at any time.
remove_other_mode_units attach "$UNITS"
remove_other_mode_units attach "$UNITS"
check_absent "$UNITS/ghostframe-xorg.service"

# Switching to headless must remove attach's lightdm autologin drop-in.
# Leaving it means lightdm autologins the operator AND the headless getty
# autologins guest: two graphical sessions on one GPU, the exact contention
# this cleanup exists to prevent.
dropin="$UNITS/99-ghostframe-autologin.conf"
printf '[Seat:*]\nautologin-user=%s\nautologin-user-timeout=0\n' "$USER" > "$dropin"
remove_other_mode_units headless "$UNITS" "$dropin"
check_absent "$dropin"

# ...and it must be safe when the drop-in is not there.
remove_other_mode_units headless "$UNITS" "$dropin"

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
remove_other_mode_units headless "$UNITS" "$dropin"
unset -f getent
check_absent "$TEST_PREV_UNIT"
check_absent "$dropin"

# Same-user switch must NOT delete that user's unit -- the headless install
# overwrites it in place, and removing it here would race that write.
touch "$TEST_PREV_UNIT"
printf '[Seat:*]\nautologin-user=%s\n' "$USER" > "$dropin"
target_user="$USER"
getent() { printf '%s:x:0:0::%s:/bin/sh\n' "$USER" "$TEST_PREV_HOME"; }
remove_other_mode_units headless "$UNITS" "$dropin"
unset -f getent
check_present "$TEST_PREV_UNIT"

# --- require_lightdm's main-conf override warning -------------------------
#
# lightdm reads lightdm.conf AFTER lightdm.conf.d, so an uncommented
# autologin-user= there beats our drop-in and the machine autologins nobody.
# The install would report success and then serve nothing after a reboot.
target_user="somebody"
# Force the lightdm.service branch so the check below is what we are testing.
readlink() { echo /usr/lib/systemd/system/lightdm.service; }
# shellcheck disable=SC2317
systemctl() { return 1; }

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
unset -f readlink systemctl

[[ $fail -eq 0 ]] && echo "PASS"
exit $fail
