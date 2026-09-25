#!/usr/bin/env bash
# M4b Task 1 spike: does edid_override on a VKMS virtual connector actually
# take effect, and is it reversible?
#
# Run with:  sudo bash docs/superpowers/plans/m4b-spike.sh
#
# SAFETY
#   - Touches ONLY card0-Virtual-1, and refuses to run unless that connector
#     belongs to a vkms/faux driver. Your desktop is on card1-HDMI-A-2 (a real
#     amdgpu output) and is never touched.
#   - Restores the connector to its original state at the end, including on
#     failure (trap on EXIT).
#   - Reads, never writes, the real monitor's EDID -- it is only used as a
#     known-good blob to inject.

set -uo pipefail

CONN=card0-Virtual-1
SRC_EDID=/sys/class/drm/card1-HDMI-A-2/edid
BLOB=/tmp/m4b-spike-known.edid

say() { printf '\n=== %s ===\n' "$*"; }
ans() { printf '  %-58s %s\n' "$1" "$2"; }

[[ $EUID -eq 0 ]] || { echo "must run as root: sudo bash $0"; exit 1; }

# ---------------------------------------------------------------- safety ----
say "Safety check: confirm $CONN is a virtual connector, not your display"
CARD=${CONN%%-*}
DRV=$(basename "$(readlink -f "/sys/class/drm/$CARD/device/driver")" 2>/dev/null)
ans "driver behind $CARD" "$DRV"
case "$DRV" in
  vkms|faux_driver) ;;
  *) echo "REFUSING: $CARD is driven by '$DRV', not vkms. Aborting."; exit 1 ;;
esac
ans "your desktop connector (untouched)" "card1-HDMI-A-2"

# ------------------------------------------------------- locate debugfs ----
say "Step 1: resolve the DRI minor BY CONNECTOR NAME (never assume dri/0)"
MINOR=""
for d in /sys/kernel/debug/dri/*/; do
  if [[ -d "$d/Virtual-1" ]]; then MINOR="$d"; break; fi
done
if [[ -z "$MINOR" ]]; then
  echo "FAIL: no debugfs dir contains a Virtual-1 connector."
  echo "      Is debugfs mounted?  mount | grep debugfs"
  exit 1
fi
ans "debugfs path" "$MINOR/Virtual-1"
ans "minor number" "$(basename "$MINOR")"
ans "sysfs card number" "${CARD#card}"
if [[ "$(basename "$MINOR")" != "${CARD#card}" ]]; then
  ans "MINOR != CARD NUMBER" "CONFIRMED -- spec 5.1 is right to forbid hardcoding"
else
  ans "minor == card number here" "(but still resolve by name; not stable across boots)"
fi

# ------------------------------------------------- required debugfs files --
say "Step 2: the two files the design depends on"
for f in edid_override trigger_hotplug; do
  if [[ -e "$MINOR/Virtual-1/$f" ]]; then ans "$f" "present"
  else ans "$f" "MISSING -- design depends on it, stop here"; exit 1; fi
done

# ------------------------------------------------------------- before -----
say "Step 3: state BEFORE injection"
BEFORE_MODES=$(cat "/sys/class/drm/$CONN/modes" 2>/dev/null | tr '\n' ' ')
BEFORE_EDID_BYTES=$(wc -c < "/sys/class/drm/$CONN/edid" 2>/dev/null || echo 0)
ans "modes" "${BEFORE_MODES:-<none>}"
ans "edid size (bytes)" "$BEFORE_EDID_BYTES"

# restore on any exit from here on
restore() {
  say "Restoring $CONN to its original state"
  printf '1' > "$MINOR/Virtual-1/edid_override" 2>/dev/null
  echo 1 > "$MINOR/Virtual-1/trigger_hotplug" 2>/dev/null
  sleep 1
  local m b
  m=$(cat "/sys/class/drm/$CONN/modes" 2>/dev/null | tr '\n' ' ')
  b=$(wc -c < "/sys/class/drm/$CONN/edid" 2>/dev/null || echo 0)
  ans "modes after restore" "${m:-<none>}"
  ans "edid size after restore" "$b"
  if [[ "$b" == "$BEFORE_EDID_BYTES" ]]; then
    ans "REVERSIBLE" "YES -- back to the original state"
  else
    ans "REVERSIBLE" "NO -- edid size $b != original $BEFORE_EDID_BYTES (INVESTIGATE)"
  fi
  rm -f "$BLOB"
}
trap restore EXIT

# ------------------------------------------------------------- inject -----
say "Step 4: inject a known-good EDID (copied from your real monitor)"
if [[ ! -r "$SRC_EDID" ]] || [[ $(wc -c < "$SRC_EDID") -lt 128 ]]; then
  echo "FAIL: cannot read a usable source EDID at $SRC_EDID"; exit 1
fi
head -c 128 "$SRC_EDID" > "$BLOB"
ans "blob size" "$(wc -c < "$BLOB") bytes (base block only)"
ans "blob header ok" "$(head -c 8 "$BLOB" | xxd -p)"

cat "$BLOB" > "$MINOR/Virtual-1/edid_override" || { echo "FAIL: write to edid_override failed"; exit 1; }
ans "edid_override write" "accepted"
echo 1 > "$MINOR/Virtual-1/trigger_hotplug" || { echo "FAIL: trigger_hotplug failed"; exit 1; }
ans "trigger_hotplug" "accepted"
sleep 1

# -------------------------------------------------------------- after -----
say "Step 5: state AFTER injection -- THE GATING ANSWERS"
AFTER_MODES=$(cat "/sys/class/drm/$CONN/modes" 2>/dev/null | tr '\n' ' ')
AFTER_EDID_BYTES=$(wc -c < "/sys/class/drm/$CONN/edid" 2>/dev/null || echo 0)

if [[ "$AFTER_EDID_BYTES" -ge 128 ]]; then
  ans "Q1: kernel now exposes the injected EDID?" "YES ($AFTER_EDID_BYTES bytes)"
else
  ans "Q1: kernel now exposes the injected EDID?" "NO (still $AFTER_EDID_BYTES bytes) <-- BLOCKER"
fi

if [[ "$AFTER_MODES" != "$BEFORE_MODES" ]]; then
  ans "Q2: connector mode list changed?" "YES"
  printf '       before: %s\n' "${BEFORE_MODES:-<none>}"
  printf '       after:  %s\n' "${AFTER_MODES:-<none>}"
else
  ans "Q2: connector mode list changed?" "NO -- modes identical <-- INVESTIGATE"
fi

say "Step 6: is an X server currently on this connector?"
if command -v xrandr >/dev/null && [[ -n "${DISPLAY:-}" ]]; then
  ans "DISPLAY" "${DISPLAY}"
  ans "note" "your desktop is on card1; it should NOT have changed"
else
  ans "no X on VKMS right now" "expected -- the X-side half is covered by the e2e container"
fi

say "DONE -- copy this whole output back"
