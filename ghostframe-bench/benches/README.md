# ghostframe-lib benches

Three Criterion / iai-callgrind suites covering the M3 §12 codec-comparison
requirements and the §9.4 latency/throughput tracking obligations.

| Bench | Tool | Requires |
|---|---|---|
| `codec_latency` | Criterion | `--features gpu-bench` for H.264; otherwise no-op |
| `pipeline_throughput` | Criterion | nothing (CPU only) |
| `codec_callgrind` | iai-callgrind | `valgrind` installed |

## Running

```
# Quick smoke (CI-friendly)
cargo bench -p ghostframe-lib --bench pipeline_throughput -- --test
cargo bench -p ghostframe-lib --bench codec_latency        -- --test

# Full numbers (local, with GPU + valgrind)
cargo bench -p ghostframe-lib --features gpu-bench
```

## Adding an M3 codec

1. Implement `BenchEncoder` for the new codec in `benches/codec_latency.rs`.
2. Add it to `run_codecs()`. Wrap with `Lz4Wrapper` to measure the LZ4
   break-even §12 calls for.
3. The same `ContentClass` fixtures and BenchmarkId namespace are reused;
   no harness changes needed.

## Text-under-motion scene (scrolling text on a shaped link)

Not a Criterion bench: a Docker + VKMS + Chromium scene, driven by an example in
`ghostframe-e2e` (it needs `ghostframe-test-pattern`'s renderer for reference
frames, and `ghostframe-test-pattern -> ghostframe-bench -> ghostframe-e2e`
forbids a normal dependency the other way).

Stimulus: `ghostframe-test-pattern --drm-direct --scroll-text` scrolls a
procedural text document at `--scroll-rows-per-frame` (default 3) rows per frame,
30 fps, for `--scroll-secs` (default 10), then holds the last frame.
`--scroll-rows-per-frame 0` is the static control (same timeline, nothing moves).
The scene runs at 1920x1080 on a `tc`-shaped link (`SHAPE_BANDWIDTH_KBPS`,
`SHAPE_DELAY_MS`, `SHAPE_LOSS_PCT`) with CDF53 enabled and 30 fps capture.

Metric definitions are in the module docs of
`ghostframe-e2e/src/harness/scroll_scene.rs`. The scene is report-only.

```bash
export CARGO_BUILD_JOBS=4 TMPDIR=$PWD/target/tmp
# The container image bundles the test pattern: rebuild after changing it, and
# hold the host lock for build AND run together (image/container names are global).
flock /home/cedric/work/.gf-heavy.lock bash -c '
  just containers-build &&
  cargo run --release -p ghostframe-e2e --example scroll_scene -- \
     run --label scroll_10 --mbps 10 --rows 3 --n 3 --out /tmp/scroll.jsonl &&
  cargo run --release -p ghostframe-e2e --example scroll_scene -- \
     run --label static_10 --mbps 10 --rows 0 --n 3 --out /tmp/scroll.jsonl'
cargo run --release -p ghostframe-e2e --example scroll_scene -- summarize /tmp/scroll.jsonl
```

Needs the web client `dist/` (`just build-web`). Flags: `--mbps --rows --secs
--fps --delay-ms --loss-pct --width --height --recovery-cap-s
--shot-interval-ms --n --label --out`.
