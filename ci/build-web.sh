#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Builds the browser client into ghostframe-web-client/dist/ (ghostbridge
# //go:embeds it): cargo builds the wasm, the wasm-bindgen CLI writes the JS
# glue, wasm-opt optimises (the flags wasm-pack 0.15.0 used), and both files
# are named by content hash so a browser never runs a stale pair. One script
# for `just build-web` and every CI job. No Node.
#
# Tools: wasm-bindgen-cli at exactly the version in Cargo.lock, binaryen
# version_117. `just web-tools` installs both into target/web-tools/; a
# wasm-bindgen / wasm-opt on PATH is used when it is the right version.
set -euo pipefail
cd "$(dirname "$0")/.."

BINARYEN_VERSION=117
want_bindgen=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
tools=${GF_WEB_TOOLS:-$PWD/target/web-tools}
PATH="$tools/bin:$tools/binaryen-version_${BINARYEN_VERSION}/bin:$PATH"

have_bindgen=$(wasm-bindgen --version 2>/dev/null | awk '{print $2}' || true)
if [[ "$have_bindgen" != "$want_bindgen" ]]; then
    echo "error: wasm-bindgen CLI is '${have_bindgen:-missing}', Cargo.lock has $want_bindgen; run: just web-tools" >&2
    exit 1
fi
have_opt=$(wasm-opt --version 2>/dev/null | sed -n 's/.*version \([0-9]*\).*/\1/p' || true)
if [[ "$have_opt" != "$BINARYEN_VERSION" ]]; then
    echo "error: wasm-opt is '${have_opt:-missing}', want binaryen version_$BINARYEN_VERSION; run: just web-tools" >&2
    exit 1
fi

cargo build -p ghostframe-client-wasm --target wasm32-unknown-unknown --release
# Where cargo put it: CARGO_TARGET_DIR or build.target-dir move it, and a
# hardcoded ./target would hand wasm-bindgen a stale wasm without an error.
target_dir=$(cargo metadata --format-version 1 --no-deps | sed -n 's/.*"target_directory":"\([^"]*\)".*/\1/p')
if [[ -z "$target_dir" ]]; then
    echo "error: cargo metadata gave no target_directory" >&2
    exit 1
fi
built="$target_dir/wasm32-unknown-unknown/release/ghostframe_client_wasm.wasm"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
wasm-bindgen --target web --no-typescript --out-dir "$work" "$built"
wasm-opt -O "$work/ghostframe_client_wasm_bg.wasm" -o "$work/opt.wasm"

hash=$(cat "$work/opt.wasm" "$work/ghostframe_client_wasm.js" | sha256sum | cut -c1-12)
js="ghostframe_client_wasm-$hash.js"
wasm="ghostframe_client_wasm_bg-$hash.wasm"
dist=ghostframe-web-client/dist
rm -rf "$dist"
mkdir -p "$dist/assets"
cp "$work/ghostframe_client_wasm.js" "$dist/assets/$js"
cp "$work/opt.wasm" "$dist/assets/$wasm"
sed -e "s/@GF_JS@/$js/" -e "s/@GF_WASM@/$wasm/" ghostframe-web-client/index.html > "$dist/index.html"
if grep -q '@GF_' "$dist/index.html"; then
    echo "error: a placeholder survived in $dist/index.html" >&2
    exit 1
fi
echo "built $dist: $js, $wasm ($(stat -c %s "$dist/assets/$wasm") bytes)"
