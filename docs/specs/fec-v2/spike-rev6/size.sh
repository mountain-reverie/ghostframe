#!/usr/bin/env bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# gzip -9 and brotli -q 11 sizes of every wasm in out/, with the delta over base.wasm.
cd "$(dirname "$0")/out"
sz() { wc -c < "$1"; }
gz() { gzip -9 -c "$1" | wc -c; }
br() { brotli -q 11 -c "$1" | wc -c; }
bgz=$(gz base.wasm); bbr=$(br base.wasm); braw=$(sz base.wasm)
printf 'variant\traw\tgzip\tbrotli\td_raw\td_gzip\td_brotli\n'
for f in base gf rusty-lean-scalar rusty-lean-simd128 rusty-scalar rusty-simd128 rq rq-simd128 rlc; do
  r=$(sz $f.wasm); g=$(gz $f.wasm); b=$(br $f.wasm)
  printf '%s\t%d\t%d\t%d\t%d\t%d\t%d\n' $f $r $g $b $((r-braw)) $((g-bgz)) $((b-bbr))
done

# Same table after binaryen's wasm-opt -Oz (available in the wasm-pack cache; skipped when absent).
WO=$(ls "$HOME"/.cache/.wasm-pack/wasm-opt-*/bin/wasm-opt 2>/dev/null | head -1)
WO=${WO:-$(command -v wasm-opt || true)}
if [ -n "$WO" ]; then
  mkdir -p opt
  for f in base gf rusty-lean-scalar rusty-lean-simd128 rusty-scalar rusty-simd128 rq rq-simd128 rlc; do "$WO" -Oz --all-features $f.wasm -o opt/$f.wasm; done
  cd opt
  bgz=$(gz base.wasm); bbr=$(br base.wasm); braw=$(sz base.wasm)
  printf '\nwasm-opt -Oz\nvariant\traw\tgzip\tbrotli\td_raw\td_gzip\td_brotli\n'
  for f in base gf rusty-lean-scalar rusty-lean-simd128 rusty-scalar rusty-simd128 rq rq-simd128 rlc; do
    r=$(sz $f.wasm); g=$(gz $f.wasm); b=$(br $f.wasm)
    printf '%s\t%d\t%d\t%d\t%d\t%d\t%d\n' $f $r $g $b $((r-braw)) $((g-bgz)) $((b-bbr))
  done
else echo "wasm-opt not available; sizes are cargo output only"; fi
