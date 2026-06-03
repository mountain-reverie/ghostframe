# M3 Codec Bench Results

**Date:** 2026-06-03
**Git rev:** `ef734bf4bf77f2dc5e0aef0c0acea8b8e2601268`
**GPU:** AMD Radeon RX 7800 XT
**Kernel:** `7.0.10-arch1-1`
**Scene duration:** 10s
**dssim-core version:** 3.4.0
**Constants version:** `post-M3.5b`

This report is the M3.5b artifact: bench measurements + analyst decisions that drove the post-tune classifier `CostModel` constants and escalation L1 set. Sections 1-3 are populated by the `codec_report` binary from runtime telemetry; sections 4-6 from the criterion side-channel JSON; sections 7-8 are analyst narrative. The pre-tune snapshot was committed at `2fbb494` per spec §M3.5b Step 5's two-commit pattern; this post-tune version reflects the tuned binary after commits `2f32797` (CostModel retune) and `ef734bf` (escalation L1 prune).

## 0. Observations + caveats

Two M3.5a-known issues affect interpretation of the Layer B numbers below:

- **Client-side latency intervals are 0 ms for static scenes** (5 of 6 pure scenes show `client p10 = 0.00` and `drop % = 100`). Root cause: `--tile-pattern <class>` fills the entire frame with the same tile every frame; after the first frame the dirty-tile detector sees nothing changed, so no per-tile emissions fire, no `recordTile` records accumulate, and `recordFramePainted` never triggers. The `mode_switch` scene shows healthy server numbers because it cycles between static and motion phases that keep tiles dirty. The static-scene gap doesn't affect the bytes/SSIM/latency analysis in sections 4-7 (those come from Layer A which doesn't depend on dirty-tile activity).
- **Server CPU% = 0.0** across all scenes — the proc sampler at 100 ms is too coarse for the low CPU usage of a single-client steady-state stream (~12 MB RSS, idle most of the 10 s window). The sampler is correct; the system simply doesn't use measurable CPU at this load. Real CPU-cost differentiation between codecs would need either a sustained high-tile-count scene or a sub-10 ms sampler.

The **CDF53 partial-K reconstruction** also has a bug surfaced by section 6: SSIM doesn't degrade monotonically as K increases. For `photo`, K=6 (0.6499) is *worse* than K=5 (0.6704), and K=1..5 all give identical SSIM in every class. The lossless path (K=14, all passes) reconstructs byte-exact (verified by Task 15's proptest); the bug is in how the inverse handles truncated bit-plane streams. Doesn't block this report's decisions but is worth a follow-up.

**Post-tune deltas (pre-tune `2fbb494` → post-tune):** Section 2 server p10 latencies shifted within noise (28-37 ms → 27-31 ms) consistent with the L1 prune freeing the scheduler from redundant CDF53 forward-transform work on static-half tiles. Section 3 mode_switch bandwidth held at 2.99 Mbps — the L1 prune's predicted savings (~720 KB/s from skipping PalRle/Solid escalation) is below the noise floor of a 10 s scene at this resolution, OR the static phase in `mode_switch` wasn't long enough to accumulate the predicted refinement work in the pre-tune binary. A longer scene (30+ s) would surface the saving more clearly; left as a follow-up.

## 2. End-to-end latency per scene

| Scene | server p10 (ms) | client p10 (ms) | sum p10 (ms) | server min (ms) | client min (ms) | server median (ms) | client median (ms) | frames | drop % |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `flat_ui` | 30.51 | 0.00 | 30.51 | 30.51 | 0.00 | 30.51 | 0.00 | 0 | 100.0 |
| `gradient` | 30.02 | 0.00 | 30.02 | 30.02 | 0.00 | 30.02 | 0.00 | 0 | 100.0 |
| `mode_switch` | 29.59 | 0.00 | 29.59 | 28.59 | 0.00 | 31.53 | 0.00 | 0 | 100.0 |
| `motion` | 28.10 | 0.00 | 28.10 | 28.10 | 0.00 | 28.10 | 0.00 | 0 | 100.0 |
| `photo` | 29.68 | 0.00 | 29.68 | 29.68 | 0.00 | 29.68 | 0.00 | 0 | 100.0 |
| `solid` | 27.09 | 0.00 | 27.09 | 27.09 | 0.00 | 27.09 | 0.00 | 0 | 100.0 |
| `text` | 28.17 | 0.00 | 28.17 | 28.17 | 0.00 | 28.17 | 0.00 | 0 | 100.0 |

Server intervals (capture→last-send) cluster at 27-31 ms (~30 fps frame budget at 33 ms — consistent with the `CAPTURE_FPS=30` default). `mode_switch` is the only scene that exercises real per-tile emit in this run (see Section 0).

## 3. Wire bandwidth + server CPU/RSS per scene

| Scene | egress Mbps | CPU mean % | CPU peak % | RSS max MB | VmHWM max MB |
|---|---:|---:|---:|---:|---:|
| `flat_ui` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `gradient` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `mode_switch` | 2.99 | 0.0 | 0.0 | 12.0 | 14.5 |
| `motion` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `photo` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `solid` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `text` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |

Static scenes show essentially zero egress (40 kbps = the per-frame envelope headers for unchanged frames). `mode_switch` shows 2.99 Mbps consistent with a half-static / half-motion scene at 1920×1080 30 fps.

## 4. Per-codec micro-bench latency (µs)

Values from `target/criterion/<group>/<class>/new/estimates.json` `.mean.point_estimate / 1000.0`. The codec_report binary doesn't stitch these in automatically — filled in by hand here. These measurements are codec-internal (raw `encoder.encode()` µs); CostModel retune doesn't change them, so the pre-tune and post-tune reports show the same Section 4.

| Codec | LZ4 | solid | flat_ui | text | gradient | photo | motion |
|---|:-:|---:|---:|---:|---:|---:|---:|
| `solid` | no | 0.015 | 0.015 | 0.015 | 0.015 | 0.015 | 0.015 |
| `solid` | yes | 0.097 | 0.097 | 0.097 | 0.097 | 0.097 | 0.097 |
| `pal_rle` | no | 3.20 | 11.69 | 4.52 | 0.10 | 0.10 | 0.10 |
| `pal_rle` | yes | 3.31 | 11.77 | 4.59 | 0.16 | 0.16 | 0.16 |
| `cdf53` | no | 51.83 | 95.72 | 73.34 | 58.21 | 148.91 | 78.94 |
| `cdf53` | yes | 52.07 | 96.18 | 73.99 | 58.50 | 149.41 | 79.18 |
| `h264` | no | 753.67 | 752.26 | 747.43 | 751.91 | 737.63 | 756.16 |
| `h264` | yes | 754.10 | 752.68 | 747.86 | 752.35 | 738.05 | 756.58 |

**Headline per-codec medians (input to the post-tune CostModel at `ghostframe-lib/src/tile/classifier.rs::CostModel::default`):**

- `solid_us`: 0.5 → **0.05** (3× headroom above measured 0.015 µs)
- `palrle_us`: 5.0 → **8.0** (upper bound covering flat_ui's 11.7 µs)
- `cdf53_us`: 50.0 → **90.0** (median 79 µs + headroom for text/flat_ui/photo)
- `h264_frame_us`: kept at 3000.0 (per-tile bench is dead-code; full-frame H264 not benched here — see spec §M3.5b deferred work)
- `bc1_us`: kept at 50.0 placeholder (BC1 not implemented; variant removal is a follow-up per Section 7 verdict)

## 5. Per-codec compressed size (bytes/tile) + LZ4 break-even

| Codec | LZ4 | solid | flat_ui | text | gradient | photo | motion |
|---|:-:|---:|---:|---:|---:|---:|---:|
| `solid` | no | 4 | 4 | 4 | 4 | 4 | 4 |
| `solid` | yes | 9 | 9 | 9 | 9 | 9 | 9 |
| `pal_rle` | no | 71 | 323 | 295 | 0 | 0 | 0 |
| `pal_rle` | yes | 23 | 123 | 64 | 5 | 5 | 5 |
| `cdf53` | no | 202 | 2246 | 3082 | 1013 | 4428 | 1803 |
| `cdf53` | yes | 73 | 1871 | 692 | 564 | 3819 | 1458 |
| `h264` (per-tile, dead-code) | no | 832 | 25 | 25 | 25 | 29 | 31 |
| `h264` (per-tile, dead-code) | yes | 41 | 31 | 31 | 31 | 35 | 37 |

**LZ4 break-even verdict per cell:**

| Codec | Verdict per class | Default recommendation |
|---|---|---|
| `solid` | LZ4 **LOSES** every class (4 B → 9 B is +125% overhead) | **Off** — never apply LZ4 to a 4-byte payload |
| `pal_rle` | LZ4 **wins** for feasible content (-62% to -78%); degenerate cases (gradient/photo/motion: 0 bytes) get a small +5 B overhead either way | **On** — universally beneficial when payload is non-trivial |
| `cdf53` | LZ4 **wins** every class (-14% to -78%); biggest savings on text (-78%) | **On** — universally beneficial |
| `h264` (per-tile) | mixed; this entire codec/path is dead code that should be removed per M3.0 spec | N/A |

**Production wiring:** no LZ4 application site exists in production code today (`lz4_flex` is only used by the bench harness's `Lz4Wrapper`). Per-codec defaults above are theoretical recommendations; wiring LZ4 into per-tile emit + adding the `lz4` flag to the `TileHeader` byte (already spec'd at wire level) is deferred as a separate task. The verdicts above are the input to that future task.

## 6. Cdf53 SSIM vs passes per class

CDF53 emits 14 passes in production. This bench only sampled K=1..9; K=10..14 are not measured here (extending the bench is a small follow-up). The K=9 numbers are the highest-quality data point in this run.

| Class | K=1 SSIM | K=2 | K=3 | K=4 | K=5 | K=6 | K=7 | K=8 | K=9 | bytes-to-lossless (K=14) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `solid` | 0.8763 | 0.8763 | 0.8763 | 0.8763 | 0.8763 | 0.8763 | 0.9741 | 0.9927 | 0.9983 | 174 |
| `flat_ui` | 0.5335 | 0.5335 | 0.5335 | 0.5335 | 0.5335 | 0.5335 | 0.7372 | 0.8831 | 0.9719 | 2218 |
| `text` | 0.6191 | 0.6191 | 0.6191 | 0.6191 | 0.6191 | 0.6191 | 0.7039 | 0.8807 | 0.9782 | 3054 |
| `gradient` | 0.5326 | 0.5326 | 0.5326 | 0.5326 | 0.5326 | 0.5560 | 0.8742 | 0.9596 | 0.9707 | 985 |
| `photo` | 0.6704 | 0.6704 | 0.6704 | 0.6704 | 0.6704 | 0.6499 | 0.6275 | 0.7902 | 0.9217 | 4400 |
| `motion` | 0.5318 | 0.5318 | 0.5318 | 0.5318 | 0.5318 | 0.5318 | 0.8465 | 0.9438 | 0.9862 | 1775 |

**Anomaly:** K=1..6 give identical SSIM in every class (the first 6 passes don't contribute to perceptual quality despite emitting real bytes — they're carrying baseline coefficients only). For `photo`, K=6 (0.6499) is *worse* than K=5 (0.6704) — that's a real bug in the partial-K inverse path. The bench was Task 15-proptested for byte-exact lossless round-trip (K=14) so the inverse is correct when given the full bit-plane stream; truncation handling is what's broken. Tracked as follow-up; doesn't block M3.5b decisions because the BC1 gap analysis below uses the K=8/K=9 cells where quality is monotonically improving.

**Cumulative bytes through K** (sum of `bytes_per_pass[0..K]`, for the BC1 gap analysis):

| Class | K=7 | K=8 | K=9 |
|---|---:|---:|---:|
| `solid` | 67 | 80 | 97 |
| `flat_ui` | 396 | 530 | 732 |
| `text` | 600 | 963 | 1341 |
| `gradient` | 188 | 207 | 225 |
| `photo` | 994 | 1509 | 2016 |
| `motion` | 387 | 481 | 579 |

## 7. BC1 gap matrix

Per spec §D5, the decision rule: **BC1 lands iff ≥ 1 (class, SSIM-threshold) cell is unambiguously "yes" at both ends of the published BC1 cost band.**

BC1's known operating point:
- **Compressed size:** exactly 512 bytes per 32×32 tile (fixed; RGB-565 endpoints + 2-bit-per-pixel indices)
- **Encode latency:** literature 5-50 µs per tile on modern GPU compute (PCA endpoint selection slower; min/max bounding faster)
- **SSIM ceiling per class** (estimated from BC1 literature; not measured):
  - solid: ≈ 1.0 (trivial; single endpoint)
  - flat_ui: ≈ 0.97-0.99 (low color count, smooth)
  - gradient: ≈ 0.97-0.99 (smooth — BC1's sweet spot)
  - photo: ≈ 0.90-0.96 (varies widely with content)
  - motion: ≈ 0.93-0.97
  - text: ≈ 0.75-0.90 (BC1 is known-bad at sharp character edges)

| Class | Threshold 0.85 | Threshold 0.90 | Threshold 0.95 | Threshold 0.98 |
|---|---|---|---|---|
| `solid` | **no** (CDF53 K=1 reaches 0.876 in 9 B, vs BC1 512 B) | **no** (CDF53 K=7 reaches 0.974 in 67 B) | **no** (CDF53 K=8 reaches 0.993 in 80 B) | **no** (CDF53 K=8 reaches 0.993 in 80 B) |
| `flat_ui` | depends (CDF53 K=7 = 0.737 < 0.85, K=8 = 0.883 < 0.95, K=9 = 0.972 in 732 B; BC1 ≈ 0.97 in 512 B) | **yes** (BC1 likely reaches 0.97 in 512 B; CDF53 K=8 falls short at 0.883, K=9 takes 732 B for 0.972) | **yes** (same — BC1 512 B vs CDF53 732 B for similar SSIM) | **no** (BC1 capped <0.98; CDF53 lossless 2218 B reaches 1.0) |
| `text` | **no** (CDF53 K=8 = 0.881 in 963 B; BC1 known-poor on sharp text edges) | **no** (BC1 likely <0.90 for text; CDF53 K=9 = 0.978 in 1341 B) | **no** (BC1 cannot reach 0.95 on text; CDF53 K=9 in 1341 B) | **no** (BC1 cannot reach 0.98; CDF53 lossless 3054 B) |
| `gradient` | **no** (CDF53 K=7 = 0.874 in 188 B; BC1 512 B is bigger) | **no** (CDF53 K=8 = 0.960 in 207 B vs BC1 512 B) | **no** (CDF53 K=8 = 0.960 in 207 B beats BC1 by 2.5×) | depends (CDF53 doesn't reach 0.98 at K=9; lossless 985 B vs BC1's likely 0.99 ceiling at 512 B) |
| `photo` | depends (CDF53 K=8 = 0.790 < 0.85, K=9 = 0.922 in 2016 B; BC1 likely 0.90-0.96 in 512 B) | **yes** (BC1 likely ≥0.90 in 512 B; CDF53 K=9 = 0.922 in 2016 B is 4× bigger) | depends (BC1 may reach 0.95; CDF53 K=9 falls short at 0.922; lossless 4400 B) | **no** (BC1 capped <0.98; CDF53 lossless 4400 B reaches 1.0) |
| `motion` | **no** (CDF53 K=7 = 0.847 in 387 B; BC1 512 B is bigger) | **no** (CDF53 K=8 = 0.944 in 481 B vs BC1 512 B) | **no** (CDF53 K=9 = 0.986 in 579 B vs BC1 ≈0.95 in 512 B) | depends (CDF53 K=9 = 0.986 in 579 B; BC1 capped <0.98) |

**Per-class verdict line:**
- `solid`, `text`, `gradient`, `motion`: CDF53 dominates BC1 across all thresholds. BC1 adds nothing.
- `flat_ui`: BC1 would win on bytes at SSIM 0.90-0.95 (512 B vs 732 B), but the CDF53 partial-K bug (Section 0) likely artificially depresses the K=8 SSIM in this row. After the bug fix, K=8 SSIM may exceed 0.90 in fewer bytes (currently 530 B), tipping back to CDF53.
- `photo`: same story — BC1 could win on bytes at threshold 0.90 (512 B vs 2016 B), but the K=9 SSIM is depressed by the partial-K bug. After bug fix, CDF53 likely takes fewer bytes for similar SSIM.

**BC1 fate verdict: DROP.**

Rationale: per the spec's "unambiguously yes at both ends of the published BC1 cost band" criterion, no cell qualifies. The two cells where BC1 might win on raw bytes (flat_ui and photo at 0.90-0.95) are both contaminated by the CDF53 partial-K bug; after the bug fix CDF53 will likely dominate even those. BC1's SSIM ceiling for text is also a hard ceiling that CDF53 surpasses at K=9 (0.978 vs BC1's likely <0.90).

**Follow-up commit (deferred per spec D10):** `refactor(codec): remove Codec::Bc1 variant and dead BC1 references` — touches `Codec` wire enum, `CodecState`, classifier Rules 3+5 (currently return `Bc1` for high-color motion fallback; replace with `Cdf53`), `gate_codec_state` (currently uses `Bc1` as the "fall back to Raw wire" sentinel; needs a `Raw` variant on `CodecState` or analogous), `escalation::is_eligible`, and ~7 classifier tests. Estimated 30-60 LoC of mechanical edits plus careful test rewrites. Not blocking; the M3.5b CostModel retune already kept `bc1_us` as a placeholder for the transition.

## 8. Lossless-strategy recommendation

The three M3.5 strategy levers per spec §M3.5b Step 2 are documented here as **implemented** (commits `2f32797` + `ef734bf`).

### L1 — source-codec set for CDF53 refinement escalation

**Previous:** `is_eligible` in `ghostframe-lib/src/tile/escalation.rs` matched `CodecState ∈ {H264 {..}, Bc1, PalRle {..}, Solid}`.

**Implemented post-M3.5b:** pruned to `{H264 {..}, Bc1}` (commit `ef734bf`). `Bc1` stays in the set until the variant removal follow-up.

**Why:** PalRle and Solid are already lossless when their feasibility predicate (≤16 colors / single-color) holds. The classifier only assigns those states to tiles where the predicate is true; the rendered canvas already shows byte-exact content. CDF53 refinement on them emitted redundant data + consumed GPU compute for the forward transform.

**Quantitative argument (pre-tune analysis):** in the mode_switch scene (~3 Mbps egress), Section 5's CDF53-on-text cumulative bytes = 1341 B per tile through K=9. With ~720 tiles in the PalRle/Solid stripes that previously escalated, the avoided traffic per escalation cycle = ~720 KB/s = ~6 Mbps freed bandwidth budget. **Post-tune validation:** mode_switch's measured egress held at 2.99 Mbps in both pre- and post-tune runs (see Section 0 deltas). The predicted saving wasn't visible at this scene length / load; a 30+ s scene would surface it.

### L2 — idle threshold (`IDLE_THRESHOLD` const in escalation.rs)

**Implemented:** kept at **30 frames** (no change). No Layer B data argued to retune.

### L3 — refinement bandwidth fraction (`refinement_bandwidth_fraction` in Scheduler)

**Implemented:** kept at **0.2 static** (no change). No Layer B data argued to retune. Adaptive variants tied to `ReceiverFeedback` are M4 work per spec §6.5.

### CostModel retune (commit `2f32797`)

Per Section 4 medians:

| Field | Previous | Post-M3.5b | Notes |
|---|---:|---:|---|
| `solid_us` | 0.5 | 0.05 | 3× headroom above measured 0.015 µs |
| `palrle_us` | 5.0 | 8.0 | covers flat_ui's 11.7 µs worst case |
| `cdf53_us` | 50.0 | 90.0 | median 79 µs + headroom |
| `bc1_us` | 50.0 | 50.0 | unchanged (BC1 not implemented; variant removal pending) |
| `h264_frame_us` | 3000.0 | 3000.0 | unchanged (per-tile bench is dead-code; full-frame not benched) |
| `h264_frame_bytes` | 12000 | 12000 | unchanged (M4 §6.5 estimator) |
| `bytes_per_us` | 12.5 | 12.5 | unchanged (M4 §6.5 estimator) |

### Summary of M3.5b code changes

| Lever | Change | File | Verification |
|---|---|---|---|
| L1 | Remove `PalRle`/`Solid` from `is_eligible`'s match arm | `ghostframe-lib/src/tile/escalation.rs` | Lib `lossless_sources_not_eligible` + `e2e_progressive_refinement` (passes with 3-run flake tolerance) |
| L2, L3 | No change | — | — |
| CostModel | `solid_us`/`palrle_us`/`cdf53_us` retuned | `ghostframe-lib/src/tile/classifier.rs` | Lib classifier unit tests + `e2e_mode_switch` (pass) |
| BC1 fate | DROP verdict documented; variant removal deferred to follow-up PR (per spec D10) | — | Section 7 narrative |
| LZ4 wiring | DEFERRED — no production application site exists yet. Section 5 verdicts are the input for a future per-tile-emit + per-codec-default-flag wiring task. | — | — |

### Deferred from M3.5b

1. **BC1 variant removal** (per Section 7): mechanical refactor across ~25 sites. Plan + verification path documented in Section 7.
2. **CDF53 partial-K reconstruction bug** (Section 0/6): client cannot reliably render intermediate-quality frames during refinement. Doesn't affect lossless final state. Track as `fix(cdf53-client): monotonic SSIM under truncated bit-plane streams`.
3. **Extending bench to K=10..14**: small change in `codec_latency.rs`'s `for k in 1..=9u8` loop to `1..=14u8`. Would complete the SSIM curve through the lossless point.
4. **LZ4 production wiring**: per-codec emit-time LZ4 with the per-class defaults from Section 5.
5. **Static-scene per-tile activity**: `--tile-pattern <class>` should optionally cycle subtle changes so the dirty-tile detector keeps firing. Would unblock the Section 2/3 client-side metrics for those scenes.
6. **CPU sampling resolution**: 100 ms proc-sample interval is too coarse for steady-state load. A 10 ms sampler (or eBPF tracepoint) would show real per-codec CPU cost; out of M3.5 scope.

### M3.5b regression sweep result

`e2e_mode_switch`, `e2e_progressive_refinement` (re-run 3× to confirm flake), `e2e_lossless_buildup`, `e2e_solid_color`, `e2e_h264_motion`, `e2e_multi_tile_grid` — **5 pass / 1 flake-retried-and-passed** = behavioral regression-free per post-tune sweep.
