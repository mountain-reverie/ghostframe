#!/usr/bin/env bash
# No tracked file outside docs/ may name a developer's home directory. A path
# like /home/<user>/... in a test, script or README only works on one machine,
# leaks that machine's username and layout into a public repository, and
# usually points at a helper that is not in the repo at all. Refer to a
# repo-relative path, an environment variable, or describe the helper instead.
#
# docs/ is exempt: dated plans and reviews record what was run where, as
# history. CI's clippy job and `just ci-local` / `just ci-client` all run this
# script, so a stray path fails locally rather than only after a push.
set -euo pipefail
cd "$(dirname "$0")/.."

# git grep: tracked files only, so build output and untracked notes are never
# scanned. Exit 1 means "no match", the passing case.
hits=$(git grep -nIE '/(home|Users)/[A-Za-z0-9_][A-Za-z0-9_.-]*' -- \
    ':!docs/' ':!ci/check-local-paths.sh' || true)
if [[ -n "$hits" ]]; then
    echo "$hits"
    echo "::error::tracked files outside docs/ must not name a home directory (see ci/check-local-paths.sh)"
    exit 1
fi
