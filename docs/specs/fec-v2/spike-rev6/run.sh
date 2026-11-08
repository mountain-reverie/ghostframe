#!/usr/bin/env bash
# Rebuilds and reruns the whole spike. Output goes to results/. Relative paths only.
set -euo pipefail
cd "$(dirname "$0")"
used() { df --output=pcent "$1" | tail -1 | tr -dc 0-9; }
[ "$(used "$HOME")" -le 88 ] && [ "$(used "$(dirname "$(mktemp -u)")")" -le 85 ] || { echo "disk too full, stopping"; exit 1; }
mkdir -p results
{ lscpu | grep -E 'Model name|Flags' | sed 's/Flags:.*\(avx2\).*/Flags: (contains avx2)/' ; rustc -V; node -v; } > results/env.txt
grep -o -w -E 'ssse3|avx2|gfni|avx512f' /proc/cpuinfo | sort -u | tr '\n' ' ' >> results/env.txt

STAGE=${1:-all}   # all | native | wasm
if [ "$STAGE" != wasm ]; then
# ---- native (A, B, E, RaptorQ overhead, RLC)
export CARGO_TARGET_DIR=../target
( cd native && cargo build --release 2>&1 | tail -1 )
N=target/release/spike_native
$N correct                      > results/A_correct.txt
rc=0; $N correct --mutate-repair1 > results/A_mutant_repair1.txt || rc=$?; echo "exit=$rc (0 = mutant killed)" >> results/A_mutant_repair1.txt
rc=0; $N correct --mutate-row0    > results/A_mutant_row0.txt || rc=$?;    echo "exit=$rc (0 = mutant killed)" >> results/A_mutant_row0.txt
$N speed   > results/B_native_speed.tsv 2> results/B_native_kernel.txt
$N rq      > results/3_raptorq_overhead.tsv
$N rlc     > results/4_rlc.txt
$N varsize > results/E_varsize.txt
fi
[ "$STAGE" = native ] && { echo done; exit 0; }

# ---- wasm (C, D)
./build_wasm.sh > /dev/null
./size.sh > results/C_wasm_size.txt
run() { # run <n>
  : > results/D_wasm_run$1.tsv
  for m in 0 1 2; do for v in rusty-scalar rusty-simd128; do node bench.mjs out/$v.wasm $m >> results/D_wasm_run$1.tsv; done; done
  for v in gf gf-simd128 rq rq-simd128 rlc rlc-simd128 base; do node bench.mjs out/$v.wasm 0 >> results/D_wasm_run$1.tsv; done
}
for i in 1 2 3; do run $i; done
python3 -I aggregate.py results/D_wasm_run1.tsv results/D_wasm_run2.tsv results/D_wasm_run3.tsv > results/D_wasm_summary.tsv
echo done
