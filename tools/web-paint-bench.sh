#!/bin/bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Section 7 paint-cost bench of the all-Rust web client spec, one label.
#   tools/web-paint-bench.sh <label> <worktree> <out-dir>
# Run the base (master plus the tick/paint timing patched in, never committed)
# first, then the branch; the images share one tag, so each label rebuilds them.
# Hold ~/work/.gf-heavy.lock around the whole call. See docs/specs/web-paint-bench.md.
label=$1; wt=$2; out=$3
mkdir -p "$out"
cd "$wt" || exit 1
just build-web > "$out/$label-build-web.log" 2>&1 || { echo "build-web $label failed"; exit 1; }
just containers-build > "$out/$label-containers.log" 2>&1 || { echo "containers $label failed"; exit 1; }
for mode in 1920x1080 4096x2160; do
  for n in 1 2 3; do
    GF_BENCH_MODE=$mode GF_BENCH_SECS=30 "${GF_GPU_RUN:-gf-gpu-run}" \
      cargo test --test e2e e2e_bench_web_paint_us -- --ignored --exact --nocapture > "$out/$label-$mode-$n.log" 2>&1
    grep -ah "^BENCH" "$out/$label-$mode-$n.log" || echo "$label $mode run $n: NO RESULT"
  done
done
for n in 1 2 3; do
  "${GF_GPU_RUN:-gf-gpu-run}" cargo run --release -p ghostframe-bench --bin codec_report -- \
    --scenes photo,motion,mode_switch --scene-duration 20s --output "$out/$label-report-$n.md" \
    > "$out/$label-report-$n.log" 2>&1 || echo "$label codec_report run $n failed"
done
echo "DONE $label"
