#!/bin/bash
set -euo pipefail

until curl -fsS http://headscale:8080/health >/dev/null 2>&1; do
    echo "waiting for headscale..."
    sleep 1
done

export TS_HOSTNAME=${TS_HOSTNAME:-ghostframe-server}
export TS_STATE_DIR=${TS_STATE_DIR:-/tmp/ghostframe-ts}
export TS_CONTROL_URL=${TS_CONTROL_URL:-http://headscale:8080}
export DRM_DEVICE=${DRM_DEVICE:-/dev/dri/card0}
export CAPTURE_FPS=${CAPTURE_FPS:-2}
export DISPLAY=:99

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
            echo "entrypoint: WARNING — tbf qdisc add failed; netem still active"
        fi
    else
        echo "entrypoint: WARNING — tc shaping unavailable (netem add failed; container needs --privileged or NET_ADMIN cap)"
    fi
fi

# If TEST_PATTERN uses DRM-direct mode, skip Xorg entirely — test-pattern
# will be the DRM master itself and ghostframe-xdaemon's drm_capture will
# read the FB the test-pattern attaches.
if [[ "${TEST_PATTERN:-}" == *--drm-direct* ]]; then
    echo "entrypoint: TEST_PATTERN is DRM-direct, skipping Xorg"
    # Override the line-13 default of 2 fps: the DRM-direct path is
    # GPU-light (no Xorg compositor in the loop) and needs a higher capture
    # rate so the classifier-flip e2e windows see enough H.264 datagrams
    # during motion phases. Caller can override via CAPTURE_FPS_DRM_DIRECT
    # for ad-hoc experiments.
    export CAPTURE_FPS=${CAPTURE_FPS_DRM_DIRECT:-30}
    ghostframe-test-pattern ${TEST_PATTERN} &
    sleep 1
    exec /usr/local/bin/ghostframe-xdaemon
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
sleep 2

# Paint root window — default is --solid-red, override with TEST_PATTERN env var
ghostframe-test-pattern ${TEST_PATTERN:---solid-red} &
sleep 1

# Start xdaemon (captures X11 root, serves via WebTransport)
exec /usr/local/bin/ghostframe-xdaemon
