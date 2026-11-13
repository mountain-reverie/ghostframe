#!/usr/bin/env bash
# Self-test of ci/check-wasm-size.sh: under, at and over the limit, no wasm at
# all, and a second oversize wasm next to a small one. Run by `just ci-local`
# and the web-client CI job, so the guard cannot rot into one that never fires.
set -uo pipefail
cd "$(dirname "$0")/.."

guard=ci/check-wasm-size.sh
limit=524288
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
fail=0

# expect <name> <expected exit 0|1> <dir>
expect() {
    local name=$1 want=$2 dir=$3 got
    "$guard" "$dir" >/dev/null 2>&1
    got=$?
    if [[ $got -ne $want ]]; then
        echo "FAIL: $name: expected exit $want, got $got"
        fail=1
    else
        echo "ok:   $name"
    fi
}

mk() { # mk <dir> <file> <bytes>
    mkdir -p "$1"
    truncate -s "$3" "$1/$2"
}

mk "$tmp/under" a.wasm 100000
expect "100 KB passes" 0 "$tmp/under"

mk "$tmp/at" a.wasm "$limit"
expect "exactly the limit passes" 0 "$tmp/at"

mk "$tmp/over1" a.wasm $((limit + 1))
expect "one byte over fails" 1 "$tmp/over1"

mk "$tmp/over" a.wasm 600000
expect "600 KB fails" 1 "$tmp/over"

mkdir -p "$tmp/empty"
expect "no wasm at all fails (stale or missing dist)" 1 "$tmp/empty"

mk "$tmp/js_only" index.js 1000
expect "only a .js file fails (no wasm found)" 1 "$tmp/js_only"

mk "$tmp/two" small.wasm 1000
mk "$tmp/two" big.wasm 600000
expect "a small wasm does not hide a big one" 1 "$tmp/two"

exit $fail
