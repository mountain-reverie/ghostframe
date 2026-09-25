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
say "Step 2: the files the design depends on"
# trigger_hotplug does NOT exist on this connector (found on the first spike
# run). The reprobe trigger is the sysfs `status` file instead: writing
# "detect" clears any forced state and re-runs connector probing, which
# re-reads the EDID honouring the override and then emits a hotplug event.
# Only edid_override needs debugfs; the trigger is plain sysfs.
if [[ -e "$MINOR/Virtual-1/edid_override" ]]; then ans "edid_override (debugfs)" "present"
else ans "edid_override (debugfs)" "MISSING -- design depends on it, stop here"; exit 1; fi
if [[ -e "$MINOR/Virtual-1/trigger_hotplug" ]]; then ans "trigger_hotplug (debugfs)" "present (unused; status=detect is the trigger)"
else ans "trigger_hotplug (debugfs)" "absent -- expected on this kernel, using status=detect"; fi
STATUS_F="/sys/class/drm/$CONN/status"
if [[ -w "$STATUS_F" ]]; then ans "status (sysfs, the trigger)" "writable"
else ans "status (sysfs, the trigger)" "NOT writable -- no reprobe mechanism, stop"; exit 1; fi

# ------------------------------------------------------------- before -----
say "Step 3: state BEFORE injection"
BEFORE_MODES=$(cat "/sys/class/drm/$CONN/modes" 2>/dev/null | tr '\n' ' ')
BEFORE_EDID_BYTES=$(wc -c < "/sys/class/drm/$CONN/edid" 2>/dev/null || echo 0)
ans "modes" "${BEFORE_MODES:-<none>}"
ans "edid size (bytes)" "$BEFORE_EDID_BYTES"

# restore on any exit from here on
restore() {
  say "Restoring $CONN to its original state"
  # Try the documented resets in order; verify after, do not assume.
  : > "$MINOR/Virtual-1/edid_override" 2>/dev/null          # zero-length write
  printf 'reset' > "$MINOR/Virtual-1/edid_override" 2>/dev/null
  echo detect > "$STATUS_F" 2>/dev/null
  sleep 1
  local m b
  m=$(cat "/sys/class/drm/$CONN/modes" 2>/dev/null | tr '\n' ' ')
  b=$(wc -c < "/sys/class/drm/$CONN/edid" 2>/dev/null || echo 0)
  ans "modes after restore" "${m:-<none>}"
  ans "edid size after restore" "$b"
  if [[ "$b" == "$BEFORE_EDID_BYTES" ]]; then
    ans "REVERSIBLE" "YES -- back to the original state"
  else
    ans "REVERSIBLE" "NO -- edid size $b != original $BEFORE_EDID_BYTES"
    echo
    echo "  !! The connector still has an injected EDID. To clear it fully:"
    echo "       sudo modprobe -r vkms && sudo modprobe vkms"
    echo "     (safe for your desktop -- it is on card1/amdgpu -- but it will"
    echo "      disturb any VKMS-backed e2e run.)  A reboot also clears it."
  fi
  rm -f "$BLOB"
}
trap restore EXIT

# ------------------------------------------------------------- inject -----
say "Step 4: inject a known-good EDID (copied from your real monitor)"
if [[ ! -r "$SRC_EDID" ]] || [[ $(wc -c < "$SRC_EDID") -lt 128 ]]; then
  echo "FAIL: cannot read a usable source EDID at $SRC_EDID"; exit 1
fi
# Take the base block and make it SELF-CONSISTENT: byte 126 is the extension
# count, and the kernel's drm_edid_override_set rejects the write with EINVAL
# unless 128*(1+extensions) <= len. A raw `head -c 128` of a 256-byte monitor
# EDID still claims 1 extension, which is why the first attempt failed.
# Zeroing it and recomputing the byte-127 checksum yields a valid standalone
# base block -- the same shape ghostframe-edid will synthesise.
python3 - "$SRC_EDID" "$BLOB" <<'PYEOF'
import sys
src, dst = sys.argv[1], sys.argv[2]
b = bytearray(open(src, 'rb').read()[:128])
b[126] = 0                                   # no extension blocks follow
b[127] = 0
b[127] = (-sum(b)) & 0xFF                    # block must sum to 0 mod 256
assert sum(b) % 256 == 0
open(dst, 'wb').write(bytes(b))
print(f"  built a self-consistent 128-byte base block (extensions=0, checksum=0x{b[127]:02X})")
PYEOF
ans "blob size" "$(wc -c < "$BLOB") bytes (base block, extensions=0)"
ans "blob header ok" "$(head -c 8 "$BLOB" | xxd -p)"
ans "blob checksum valid" "$(python3 -c "
import sys
b=open('$BLOB','rb').read()
print('yes' if sum(b)%256==0 else 'NO')")"

if ! cat "$BLOB" > "$MINOR/Virtual-1/edid_override" 2>/dev/null; then
  echo "  FAIL: write to edid_override rejected (EINVAL means the kernel"
  echo "        considers the blob malformed: it validates len >= 128 and"
  echo "        128*(1+extensions) <= len). Retrying with the full source EDID"
  echo "        including its extension block:"
  if cat "$SRC_EDID" > "$MINOR/Virtual-1/edid_override" 2>/dev/null; then
    ans "full-size injection" "accepted (base+extension)"
  else
    echo "  FAIL: both forms rejected -- report this, it blocks the design."
    exit 1
  fi
fi
ans "edid_override write" "accepted"
echo detect > "$STATUS_F" || { echo "FAIL: write 'detect' to status failed"; exit 1; }
ans "status=detect (reprobe trigger)" "accepted"
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
