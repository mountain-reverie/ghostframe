#!/usr/bin/env bash
# Builds every wasm variant into out/<name>.wasm (relative paths only).
set -euo pipefail
cd "$(dirname "$0")/wasm"
export CARGO_TARGET_DIR=../target/wasm
mkdir -p ../out
build() { # name features rustflags
  RUSTFLAGS="$3" cargo build --release --target wasm32-unknown-unknown --no-default-features ${2:+--features "$2"} 2>&1 | grep -E "^(error|warning: unused)" -A6 || true
  cp "$CARGO_TARGET_DIR/wasm32-unknown-unknown/release/spike_wasm.wasm" "../out/$1.wasm"
}
build base ""      ""
build gf   gf      ""
build rusty-scalar rusty ""
build rusty-simd128 rusty "-C target-feature=+simd128"
build rusty-lean-scalar rusty,lean ""
build rusty-lean-simd128 rusty,lean "-C target-feature=+simd128"
build rq   rq      ""
build rq-simd128 rq "-C target-feature=+simd128"
build rlc  rlc     ""
build rlc-simd128 rlc "-C target-feature=+simd128"
build gf-simd128 gf "-C target-feature=+simd128"
build base-simd128 "" "-C target-feature=+simd128"
ls -l ../out
