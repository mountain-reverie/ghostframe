#!/usr/bin/env bash
# The web client's wasm must stay under 512 KiB (524,288 bytes, raw).
#
# docs/specs/stats-panel-design.md section 5: adding Leptos to the wasm module
# is the biggest single size risk of the web client, and the wasm is what every
# browser downloads before the first frame. Today's wasm is about 168 KB. The
# limit leaves room for the panel and fails the build when a dependency or a
# generic blows it up; raise it deliberately, in a commit that says why, and
# record the new number in docs/specs/wasm-bundle-baseline.md.
#
# Usage: ci/check-wasm-size.sh [assets-dir]
#   assets-dir defaults to ghostframe-web-client/dist/assets (the vite output,
#   i.e. what ghostbridge embeds). It must already be built: `just build-web`.
#
# Fails closed: a directory with no .wasm file is an error, never a pass, so a
# missing or stale dist/ cannot read as "small enough". Every .wasm in the
# directory is checked, so a second module cannot hide behind a small one.
set -euo pipefail
cd "$(dirname "$0")/.."

assets="${1:-ghostframe-web-client/dist/assets}"
max_bytes=524288

shopt -s nullglob
files=("$assets"/*.wasm)
if [[ ${#files[@]} -eq 0 ]]; then
    echo "::error::no .wasm file in $assets (build the web client first: just build-web)"
    exit 1
fi

status=0
for f in "${files[@]}"; do
    size=$(stat -c %s "$f")
    gz=$(gzip -c "$f" | wc -c)
    echo "$(basename "$f"): $size bytes raw, $gz bytes gzip (limit $max_bytes raw)"
    if ((size > max_bytes)); then
        echo "::error::$(basename "$f") is $size bytes, over the $max_bytes byte (512 KiB) limit (see ci/check-wasm-size.sh)"
        status=1
    fi
done
exit $status
