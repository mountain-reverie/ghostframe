#!/usr/bin/env bash
# `allow-software-vulkan` lets the daemon run on a software Vulkan device. It
# exists for the e2e test-server image only, and must reach no other build.
#
# A cargo feature named in ANY workspace member's Cargo.toml is turned on for
# every build that selects that member (resolver 2 unifies features across the
# packages of one invocation), which is exactly how it would leak into
# `just build-release`. So the only places it may be named are:
#   - ghostframe-lib/Cargo.toml, the `[features]` definition line;
#   - tests/containers/test-server/Dockerfile, its own `cargo build --features`.
# This guard fails on any other mention in a manifest, the Justfile, ci/ scripts
# or the workflows. ci-local additionally checks the release binary.
set -euo pipefail
cd "$(dirname "$0")/.."

status=0
feature='allow-software-vulkan'

# Manifests: only the definition line in ghostframe-lib/Cargo.toml.
while IFS= read -r hit; do
    file=${hit%%:*}; file=${file#./}
    rest=${hit#*:}
    line=${rest#*:}
    if [[ "$file" == "ghostframe-lib/Cargo.toml" && "$line" =~ ^${feature}[[:space:]]*=[[:space:]]*\[\] ]]; then
        continue
    fi
    # Comment lines in a manifest may name it.
    [[ "$line" =~ ^[[:space:]]*# ]] && continue
    echo "::error::$file:${rest%%:*}: '$feature' may only be defined in ghostframe-lib/Cargo.toml [features]; naming it elsewhere in a manifest unifies it into every build"
    status=1
done < <(grep -rn --include=Cargo.toml -- "$feature" . | grep -v '^./target/' || true)

# Build entry points other than the test-server Dockerfile.
while IFS= read -r hit; do
    case "$hit" in
        ci/check-test-features.sh:*) continue ;;
        # Names it only as a forbidden feature of the release daemon.
        ci/check-release-features.sh:*) continue ;;
    esac
    echo "::error::${hit%%:*}: '$feature' must only be passed by tests/containers/test-server/Dockerfile"
    status=1
done < <(grep -rn -- "$feature" Justfile .github ci packaging 2>/dev/null || true)

exit $status
