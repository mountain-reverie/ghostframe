#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Installs the web build's pinned tools into $GF_WEB_TOOLS (default
# target/web-tools/, repo-local, nothing in $HOME; CI sets it outside target/
# because rust-cache prunes target/): wasm-bindgen-cli at Cargo.lock's version and binaryen
# version_117 (the wasm-opt wasm-pack 0.15.0 used). One source for
# `just web-tools` and the CI composite action _build-web-client.
set -euo pipefail
cd "$(dirname "$0")/.."

tools=${GF_WEB_TOOLS:-$PWD/target/web-tools}
v=$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)
cargo install --root "$tools" wasm-bindgen-cli --version "$v" --locked
b=117
if [[ ! -x "$tools/binaryen-version_$b/bin/wasm-opt" ]]; then
    mkdir -p "$tools"
    curl -fsSL "https://github.com/WebAssembly/binaryen/releases/download/version_$b/binaryen-version_$b-x86_64-linux.tar.gz" \
        | tar -xz -C "$tools"
fi
