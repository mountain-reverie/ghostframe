#!/bin/bash
set -euo pipefail

until curl -fsS http://headscale:8080/health >/dev/null 2>&1; do
    echo "waiting for headscale..."
    sleep 1
done

export TS_HOSTNAME=${TS_HOSTNAME:-ghostframe-server}
export TS_STATE_DIR=${TS_STATE_DIR:-/tmp/ghostframe-ts}
export TS_CONTROL_URL=${TS_CONTROL_URL:-http://headscale:8080}

exec /usr/local/bin/ghostframe-xdaemon
