# Web client paint-cost bench (spec section 7, gate G3)

Branch `feat/web-renderer-wgpu` against master `60b10bb1`, one host, Chromium
over the local headscale, GPU harness. Method: `tools/web-paint-bench.sh` and
the `#[ignore]`d test `e2e_bench_web_paint_us`. The base is master with the
same tick timing patched into `main.ts` plus a paint-step timer around
`encodeAndPresentFrame` (never committed). Each cell is three runs of 30 s;
the table shows the median of the three, in microseconds. No threshold is set
here: whether any slower cell blocks the merge is the owner's call.

Three metrics, because the branch moves tile application from the paint step
into the datagram step:

- **framePaints**: `__ghostframe_framePaints`, `raf - first receipt` per frame.
  A coarse stamp (it waits for the server to be two sequences ahead); it is
  empty at 4096x2160 on both sides (0 samples). The reason is not verified: the test now prints paints= and recorded_tiles= next to the line, so a rerun records whether paints were empty or the join failed (the recorded-tiles FIFO is capped at 32768). Treat the 4K framePaints cell as unmeasured; a usable one needs an animated source rather than --gradient.
- **tick**: main-thread CPU of each rAF tick that presented a frame
  (`__ghostframeTickUs`), identical TypeScript on both sides. The comparable
  cost number.
- **paint step**: informational, not comparable alone (master's includes tile
  upload and draw, the branch's does not include the apply).

## Results (median of 3 runs, us)

| mode | label | framePaints p50 | framePaints p99 | tick p50 | tick p99 | tick max | paint step p50 | paint step p99 | presented ticks (3 runs) |
|---|---|---:|---:|---:|---:|---:|---:|---:|---|
| 1920x1080 | base | 68300 | 71900 | 3200 | 6400 | 8200 | 400 | 2200 | 58, 58, 58 |
| 1920x1080 | branch | 75600 | 78200 | 2300 | 5500 | 7700 | 400 | 1000 | 70, 71, 70 |
| 4096x2160 | base | n/a (0 frames) | n/a | 2700 | 6100 | 10600 | 400 | 2500 | 246, 262, 252 |
| 4096x2160 | branch | n/a (0 frames) | n/a | 2600 | 4900 | 8700 | 700 | 1200 | 309, 305, 301 |

Cells where the branch is slower:

- 1920x1080 framePaints: p50 75600 against 68300 (+7300, +11 %), p99 78200
  against 71900 (+6300). Raw p50 base 66300 / 68300 / 69100, branch 75600 /
  76900 / 70800; raw p99 base 85800 / 70000 / 71900, branch 78200 / 81600 /
  72400. Only 5 frames per run, and the metric is dominated by the "two
  sequences ahead" wait; the third branch run overlaps the base range.
- 4096x2160 paint step p50: 700 against 400 (+300), consistent across the
  three branch runs (700 each). Its p99 and max are lower (1200 against 2500,
  2000 against 4100), and the whole tick is not slower (p50 2600 against 2700).

Tick p50, p99 and max are lower on the branch in both modes. Raw runs (p50 /
p99 / max, us):

| cell | run 1 | run 2 | run 3 |
|---|---|---|---|
| 1080 base tick | 3200 / 7200 / 8200 | 3200 / 6400 / 8700 | 3200 / 6400 / 8000 |
| 1080 branch tick | 2400 / 5500 / 7800 | 2300 / 5600 / 7100 | 2300 / 4800 / 7700 |
| 4K base tick | 2700 / 6900 / 10100 | 2600 / 5900 / 11200 | 2700 / 6100 / 10600 |
| 4K branch tick | 2600 / 4900 / 9300 | 2600 / 5000 / 8500 | 2700 / 4600 / 8700 |
| 1080 base paint step | 400 / 2200 / 2900 | 500 / 2200 / 2500 | 400 / 2600 / 2600 |
| 1080 branch paint step | 400 / 1000 / 2000 | 400 / 1399 / 1700 | 500 / 799 / 1799 |
| 4K base paint step | 400 / 2300 / 3500 | 400 / 2500 / 4100 | 400 / 2500 / 4200 |
| 4K branch paint step | 700 / 1200 / 1900 | 700 / 1200 / 2000 | 700 / 1100 / 2099 |

The branch presents more frames in the same 30 s (70 against 58 at 1080p, 305
against 253 at 4K), so its rAF ticks do more work in total; the per-tick cost
above is still not higher.

## codec_report (client median per scene, ms; 3 runs)

Scenes `photo` and `motion` delivered 0 client frames on both sides in every
run (the harness limitation, not a result). `mode_switch`:

| label | client p10 | client median | frames |
|---|---|---|---|
| base | 1003.2 / 1021.0 / 1026.7 | 1055.4 / 1063.8 / 1069.2 | 9 / 9 / 9 |
| branch | 1010.6 / 1021.6 / 1012.7 | 1057.9 / 1066.7 / 1060.0 | 18 / 9 / 17 |

Branch median is higher in runs 1 and 2 by 2.5 and 2.9 ms (run 3 lower by 9.2
ms); within the run-to-run scatter of the base itself (14 ms).

## Notes

- At 4096x2160 `framePaints` is empty on both sides; only the tick and paint
  step exist there.
- The test originally asserted a non-empty framePaints; it failed at 4K after
  printing its results, so the assertion now requires only a live tick timing.
  The base 4K runs used the old assertion (they printed, then failed); the
  measurement code is identical.
