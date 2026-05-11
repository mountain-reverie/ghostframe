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
#   - /etc/X11/xorg-vkms.conf if /dev/dri/card0 is a VKMS device
#     (presence-of-Writeback connector heuristic).
#   - /etc/X11/xorg.conf (Driver "dummy") otherwise.
if [ -z "${XORG_CONF:-}" ]; then
    if [ -e /sys/class/drm/card0-Writeback-1 ]; then
        XORG_CONF=/etc/X11/xorg-vkms.conf
        echo "entrypoint: detected VKMS card0 (writeback connector) — using xorg-vkms.conf"
    else
        XORG_CONF=/etc/X11/xorg.conf
        echo "entrypoint: no VKMS detected — using xorg.conf (dummy driver)"
    fi
fi

Xorg :99 -config "$XORG_CONF" &
sleep 2

# Paint root window — default is --solid-red, override with TEST_PATTERN env var
ghostframe-test-pattern ${TEST_PATTERN:---solid-red} &
sleep 1

# Start xdaemon (captures X11 root, serves via WebTransport)
exec /usr/local/bin/ghostframe-xdaemon
