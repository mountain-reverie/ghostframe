#!/usr/bin/env bash
# A release ghostframe-server must not contain the env-var names of the test
# knobs that ghostframe-lib compiles out unless `test-loss-injection` is on.
# Those names exist in the binary as string literals only when the code that
# reads them was compiled in, so their absence proves the knob is not honoured.
#
# Usage: ci/check-release-binary.sh [path-to-binary]
#
# ghostframe-server's own dump knobs (GHOSTFRAME_CAPTURE_DUMP_DIR,
# GHOSTFRAME_DUMP_FIRST_FRAME) are gated at run time on the same feature, so
# their names stay in the binary and are covered by a unit test instead
# (config::tests::dump_knobs_follow_the_test_knob_switch).
set -euo pipefail
cd "$(dirname "$0")/.."

bin=${1:-target/release/ghostframe-server}
[[ -f "$bin" ]] || { echo "::error::$bin not found; build it first"; exit 1; }

knobs=(
    GHOSTFRAME_NO_RTO
    GHOSTFRAME_DUMP_FRAME
    GHOSTFRAME_DATAGRAM_SEND_BUFFER_BYTES
    GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY
    GHOSTFRAME_INBOUND_LOSS_PROBABILITY
    GHOSTFRAME_INJECT_OOB_PALRLE
    GHOSTFRAME_SKIP_PALETTE_SESSION_RESET
    GHOSTFRAME_OUTBOUND_BANDWIDTH_CAP
    GHOSTFRAME_TEST_FORCE_BYTES_PER_US
    'software Vulkan permitted (test build)'
)
status=0
for k in "${knobs[@]}"; do
    if grep -aqF -- "$k" "$bin"; then
        echo "::error::$bin contains '$k': a test-only knob is compiled into the release daemon"
        status=1
    fi
done
exit $status
