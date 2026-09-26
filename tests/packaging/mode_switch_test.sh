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

[[ $fail -eq 0 ]] && echo "PASS"
exit $fail
