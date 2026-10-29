#!/bin/bash
set -euo pipefail

until curl -fsS http://headscale:8080/health >/dev/null 2>&1; do
    echo "waiting for headscale..."
    sleep 1
done

export TS_HOSTNAME=${TS_HOSTNAME:-ghostframe-server}
export TS_STATE_DIR=${TS_STATE_DIR:-/tmp/ghostframe-ts}
export TS_CONTROL_URL=${TS_CONTROL_URL:-http://headscale:8080}
export DISPLAY=:99

# --x11-sink patterns repaint at up to 60 Hz, and the tests built on them were
# tuned at 30 fps capture (the old DRM-direct default). Everything else keeps
# 2 fps. A CAPTURE_FPS set by the test wins either way.
if [[ "${TEST_PATTERN:-}" == *--x11-sink* ]]; then
    export CAPTURE_FPS=${CAPTURE_FPS:-30}
else
    export CAPTURE_FPS=${CAPTURE_FPS:-2}
fi

# M3.7b: apply tc qdisc shaping if SHAPE_* env vars set.
# netem provides delay + loss; tbf provides bandwidth rate-limit.
# Failures log warnings and continue without shaping (better than
# crashing the bench; downstream detects via the resulting data shape).
if [[ -n "${SHAPE_BANDWIDTH_KBPS:-}" ]]; then
    DELAY_MS="${SHAPE_DELAY_MS:-0}"
    LOSS_PCT="${SHAPE_LOSS_PCT:-0}"
    echo "entrypoint: applying tc shaping — ${SHAPE_BANDWIDTH_KBPS}kbit, ${DELAY_MS}ms delay, ${LOSS_PCT}% loss"
    if tc qdisc add dev eth0 root handle 1: netem delay "${DELAY_MS}ms" loss "${LOSS_PCT}%" 2>&1; then
        if tc qdisc add dev eth0 parent 1:1 handle 10: tbf rate "${SHAPE_BANDWIDTH_KBPS}kbit" burst 32kbit latency 400ms 2>&1; then
            echo "entrypoint: tc shaping applied successfully"
        else
            echo "entrypoint: WARNING — tbf qdisc add failed; rolling back netem to run unshaped"
            tc qdisc del dev eth0 root 2>&1 || true
        fi
    else
        echo "entrypoint: WARNING — tc shaping unavailable (netem add failed; container needs --privileged or NET_ADMIN cap)"
    fi
fi

# If XORG_CONF is unset, auto-detect the right config:
#   - /etc/X11/xorg-vkms.conf if /dev/dri/card0 is accessible IN the
#     container AND the host sysfs shows a Writeback-1 connector on it.
#     Both conditions must be true: the writeback-connector sysfs entry
#     visible via the host-mounted /sys can be seen even when /dev/dri is
#     NOT bind-mounted into the container, so checking /sys alone causes
#     false positives when the host has vkms loaded but the test doesn't
#     use a GPU bind-mount (i.e. all non-GPU e2e tests).
#   - /etc/X11/xorg.conf (Driver "dummy") otherwise.
if [ -z "${XORG_CONF:-}" ]; then
    if [ -e /dev/dri/card0 ] && [ -e /sys/class/drm/card0-Writeback-1 ]; then
        XORG_CONF=/etc/X11/xorg-vkms.conf
        echo "entrypoint: detected VKMS card0 (writeback connector, /dev/dri/card0 present) — using xorg-vkms.conf"
    else
        XORG_CONF=/etc/X11/xorg.conf
        echo "entrypoint: no VKMS DRM device in container — using xorg.conf (dummy driver)"
    fi
fi

Xorg :99 -config "$XORG_CONF" &
for _ in $(seq 1 50); do
    xrandr --query >/dev/null 2>&1 && break
    sleep 0.2
done

# Switch the first connected output to WxH: one of its own modes when it has
# one (VKMS lists 1024x768, 1920x1080 and 4096x2160), else a new mode (the
# dummy driver accepts any timings).
set_screen_mode() {
    local mode=$1 w=${1%x*} h=${1#*x} out clock
    out=$(xrandr --query | awk '/ connected/ {print $1; exit}')
    if [[ -z "$out" ]]; then
        echo "entrypoint: no connected output on $DISPLAY" >&2
        return 1
    fi
    if ! xrandr --output "$out" --mode "$mode" 2>/dev/null; then
        clock=$(( (w + 160) * (h + 30) * 60 / 1000000 + 1 ))
        xrandr --newmode "gf_$mode" "$clock" \
            "$w" $((w + 48)) $((w + 80)) $((w + 160)) \
            "$h" $((h + 3)) $((h + 8)) $((h + 30))
        xrandr --addmode "$out" "gf_$mode"
        xrandr --output "$out" --mode "gf_$mode"
    fi
    echo "entrypoint: $out set to $mode for --x11-sink"
}

# --x11-sink draws a buffer the size of the root window, so the screen must
# have the size the test expects before the pattern starts. The variable keeps
# its DRM-direct name because tests and scenes set it; unset means 1024x768,
# the VKMS preferred mode the DRM-direct patterns painted at.
if [[ "${TEST_PATTERN:-}" == *--x11-sink* ]]; then
    set_screen_mode "${GHOSTFRAME_DRM_MODE:-1024x768}"
    # The DRM probe would read a VKMS scanout that X drawing does not reach.
    export GHOSTFRAME_X11_CAPTURE_ONLY=1
fi

# Paint root window — default is --solid-red, override with TEST_PATTERN env var
ghostframe-test-pattern ${TEST_PATTERN:---solid-red} &
sleep 1

# Start the server (captures X11 root, serves via WebTransport)
exec /usr/local/bin/ghostframe-server
