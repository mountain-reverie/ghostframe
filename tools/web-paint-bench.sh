#!/bin/bash
# SPDX-License-Identifier: MIT OR Apache-2.0
# Section 7 paint-cost bench of the all-Rust web client spec, one label.
#   tools/web-paint-bench.sh <label> <worktree> <out-dir>
# Run the base (master plus the tick/paint timing patched in, never committed)
# first, then the branch; the images share one tag, so each label rebuilds them.
# Takes ~/work/.gf-heavy.lock itself (GF_HEAVY_LOCK to override). See docs/specs/web-paint-bench.md.
[ $# -eq 3 ] && [ -n "$1" ] && [ -n "$2" ] && [ -n "$3" ] || { echo "usage: $0 <label> <worktree> <out-dir>" >&2; exit 2; }
label=$1; wt=$2; out=$3
GPU_RUN=${GF_GPU_RUN:-$HOME/work/gf-gpu-run}
command -v "$GPU_RUN" > /dev/null || { echo "GF_GPU_RUN '$GPU_RUN' is not a command; set GF_GPU_RUN to the GPU wrapper" >&2; exit 1; }
if [ -z "$GF_BENCH_LOCKED" ]; then
  exec env GF_BENCH_LOCKED=1 flock -o "${GF_HEAVY_LOCK:-$HOME/work/.gf-heavy.lock}" "$0" "$@"
fi
mkdir -p "$out"
cd "$wt" || exit 1
just build-web > "$out/$label-build-web.log" 2>&1 || { echo "build-web $label failed"; exit 1; }
just containers-build > "$out/$label-containers.log" 2>&1 || { echo "containers $label failed"; exit 1; }
for mode in 1920x1080 4096x2160; do
  for n in 1 2 3; do
    GF_BENCH_MODE=$mode GF_BENCH_SECS=30 "$GPU_RUN" \
      cargo test --test e2e e2e_bench_web_paint_us -- --ignored --exact --nocapture > "$out/$label-$mode-$n.log" 2>&1
    grep -ah "^BENCH" "$out/$label-$mode-$n.log" || echo "$label $mode run $n: NO RESULT"
  done
done
for n in 1 2 3; do
  "$GPU_RUN" cargo run --release -p ghostframe-bench --bin codec_report -- \
    --scenes photo,motion,mode_switch --scene-duration 20s --output "$out/$label-report-$n.md" \
    > "$out/$label-report-$n.log" 2>&1 || echo "$label codec_report run $n failed"
done
echo "DONE $label"
