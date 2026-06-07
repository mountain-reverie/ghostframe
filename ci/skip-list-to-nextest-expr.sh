#!/usr/bin/env bash
# ci/skip-list-to-nextest-expr.sh
#
# Reads ci/skip-list.txt and emits a cargo-nextest filter expression that
# excludes every listed test. Used by .github/workflows/nightly.yml.
#
# Usage:
#   $(./ci/skip-list-to-nextest-expr.sh)
#
# Output example:
#   not test(/e2e_cdf53_gradient_emission/) and not test(/e2e_mode_switch/)
#
# If the skip list is empty or contains only comments, emits `all()` (which
# nextest accepts as "match all tests").

set -euo pipefail

SKIP_FILE="${1:-ci/skip-list.txt}"

if [[ ! -r "$SKIP_FILE" ]]; then
    echo "error: cannot read $SKIP_FILE" >&2
    exit 1
fi

# Strip comments and blank lines; collect names into a bash array.
mapfile -t names < <(grep -Ev '^\s*(#|$)' "$SKIP_FILE")

if [[ ${#names[@]} -eq 0 ]]; then
    echo "all()"
    exit 0
fi

expr=""
for name in "${names[@]}"; do
    # Trim whitespace.
    name="${name#"${name%%[![:space:]]*}"}"
    name="${name%"${name##*[![:space:]]}"}"
    if [[ -z "$name" ]]; then
        continue
    fi
    if [[ -z "$expr" ]]; then
        expr="not test(/${name}/)"
    else
        expr="${expr} and not test(/${name}/)"
    fi
done

echo "$expr"
