#!/usr/bin/env bash
# The daemon that `just build-release` produces, and packaging/install.sh
# ships, must not be compiled with any of ghostframe-lib's test or diagnostic
# features. Cargo unifies features over every package in one build, so a
# workspace-wide `cargo build --release` used to compile the shipped daemon
# with ghostframe-e2e's `test-loss-injection` and `browserless-harness`
# (pulled in through ghostframe-bench's dependency on ghostframe-e2e).
# build-release therefore builds `-p ghostframe-server` only, and this guard
# checks that selection's resolved features, so a new dependency edge that
# drags a test feature back in fails here rather than in a release.
set -euo pipefail
cd "$(dirname "$0")/.."

forbidden='test-loss-injection|browserless-harness|cdf53-diag|allow-software-vulkan|gpu-bench'
resolved=$(cargo tree -p ghostframe-server -e normal,build,features -i ghostframe-lib \
    | grep -oE 'ghostframe-lib feature "[a-z0-9-]+"' | sort -u)
echo "$resolved"
if grep -Eq "\"($forbidden)\"" <<<"$resolved"; then
    echo "::error::the release daemon resolves a test-only ghostframe-lib feature (see ci/check-release-features.sh)"
    exit 1
fi
