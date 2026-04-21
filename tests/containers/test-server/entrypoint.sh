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

# Start X with dummy driver
Xorg :99 -config ${XORG_CONF:-/etc/X11/xorg.conf} &
sleep 2

# Paint root window — default is --solid-red, override with TEST_PATTERN env var
ghostframe-test-pattern ${TEST_PATTERN:---solid-red} &
sleep 1

# Start xdaemon (captures X11 root, serves via WebTransport)
exec /usr/local/bin/ghostframe-xdaemon
