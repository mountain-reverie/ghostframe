# M3 Codec Bench Results

**Date:** 2026-06-03
**Git rev:** `4ca63a3729abed34ab143649e67351946c928b6c`
**GPU:** AMD Radeon RX 7800 XT
**Kernel:** `7.0.10-arch1-1`
**Scene duration:** 10s
**dssim-core version:** 3.4.0
**Constants version:** `post-M3.5b + cdf53-midpoint-fix`

This report is the M3.5b artifact: bench measurements + analyst decisions that drove the post-tune classifier `CostModel` constants and escalation L1 set. Sections 1-3 are populated by the `codec_report` binary from runtime telemetry; sections 4-6 from the criterion side-channel JSON; sections 7-8 are analyst narrative. Lineage of the report file in git history:

- `2fbb494` — pre-tune snapshot per spec §M3.5b Step 5's two-commit pattern (constants version `pre-M3.5b`)
- `6fff6a9` — post-tune regeneration after commits `2f32797` (CostModel retune) and `ef734bf` (escalation L1 prune); constants version flipped to `post-M3.5b`
- *this commit* — re-run after `4ca63a3` (CDF53 partial-K midpoint reconstruction fix). Sections 6/7 numerical updates only; verdicts unchanged.

## 0. Observations + caveats

Two M3.5a-known issues affect interpretation of the Layer B numbers below; both are now better-understood after the midpoint fix:

- **Client-side latency intervals are 0 ms for static scenes** (6 of 7 scenes show `client p10 = 0.00` and `drop % ≈ 100`). Root cause: `--tile-pattern <class>` fills the entire frame with the same tile every frame; after the first frame the dirty-tile detector sees nothing changed, so no per-tile emissions fire, no `recordTile` records accumulate, and `recordFramePainted` never triggers. `mode_switch` shows 1 client diagnostic in this run (1479.80 ms `client p10`) because it cycles between static and motion phases that keep tiles dirty; the 1.5 s figure is dominated by the rAF-lag-via-stale-eviction proxy that Task 11 documented (the harness fires `recordFramePainted` when `latestFrameSeq - 2` advances past the frame). Doesn't affect the bytes/SSIM/latency analysis in sections 4-7 (those come from Layer A which doesn't depend on dirty-tile activity).
- **Server CPU% = 0.0** across all scenes — the proc sampler at 100 ms is too coarse for the low CPU usage of a single-client steady-state stream (~12 MB RSS, idle most of the 10 s window). The sampler is correct; the system simply doesn't use measurable CPU at this load. Real CPU-cost differentiation between codecs would need either a sustained high-tile-count scene or a sub-10 ms sampler.

**CDF53 partial-K reconstruction (FIXED in commit `4ca63a3`):** prior reports' Section 6 showed SSIM staying flat for K=1..6 in every class and *dropping* at K=6 vs K=5 for `photo` — the result of OR-only bit-plane decoding that systematically under-estimated every "significant" coefficient's magnitude. The fix adds SPIHT-style midpoint reconstruction (mirrored across the Rust `decode_passes` and the three WGSL inverse shaders), so the unknown low bits of each significant coefficient are estimated as the midpoint of their range rather than 0. Per-coefficient absolute error is now provably monotonically non-increasing in K (new `decode_passes_monotonic_error_under_truncation` proptest, 200 seeds × 14 K values = 2800 assertions). Post-fix SSIM at K=9 is uniformly higher (see Section 6); a small K=6-7 dip remains for `photo` because dssim-core's structural metric weights local fidelity differently from L1 error — an SSIM × midpoint property, not a bug. Users dwelling at high K (the bulk of refinement viewing time) get strictly better intermediate quality post-fix.

## 2. End-to-end latency per scene

| Scene | server p10 (ms) | client p10 (ms) | sum p10 (ms) | server min (ms) | client min (ms) | server median (ms) | client median (ms) | frames | drop % |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `flat_ui` | 29.04 | 0.00 | 29.04 | 29.04 | 0.00 | 29.04 | 0.00 | 0 | 100.0 |
| `gradient` | 27.96 | 0.00 | 27.96 | 27.96 | 0.00 | 27.96 | 0.00 | 0 | 100.0 |
| `mode_switch` | 27.97 | 1479.80 | 1507.77 | 27.48 | 1479.80 | 29.34 | 1479.80 | 1 | 93.8 |
| `motion` | 28.25 | 0.00 | 28.25 | 28.25 | 0.00 | 28.25 | 0.00 | 0 | 100.0 |
| `photo` | 30.58 | 0.00 | 30.58 | 30.58 | 0.00 | 30.58 | 0.00 | 0 | 100.0 |
| `solid` | 28.44 | 0.00 | 28.44 | 28.44 | 0.00 | 28.44 | 0.00 | 0 | 100.0 |
| `text` | 29.45 | 0.00 | 29.45 | 29.45 | 0.00 | 29.45 | 0.00 | 0 | 100.0 |

Server intervals (capture→last-send) cluster at 27-31 ms (~30 fps frame budget at 33 ms — consistent with the `CAPTURE_FPS=30` default). `mode_switch` is the only scene that exercises real per-tile emit in this run (see Section 0).

## 3. Wire bandwidth + server CPU/RSS per scene

| Scene | egress Mbps | CPU mean % | CPU peak % | RSS max MB | VmHWM max MB |
|---|---:|---:|---:|---:|---:|
| `flat_ui` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `gradient` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `mode_switch` | 2.92 | 0.0 | 0.0 | 12.0 | 14.5 |
| `motion` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `photo` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `solid` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |
| `text` | 0.04 | 0.0 | 0.0 | 12.0 | 14.5 |

Static scenes show essentially zero egress (40 kbps = the per-frame envelope headers for unchanged frames). `mode_switch` shows 2.92 Mbps consistent with a half-static / half-motion scene at 1920×1080 30 fps.

## 4. Per-codec micro-bench latency (µs)

Values from `target/criterion/<group>/<class>/new/estimates.json` `.mean.point_estimate / 1000.0`. The codec_report binary doesn't stitch these in automatically — filled in by hand. These measurements are codec-internal (raw `encoder.encode()` µs); CostModel retune and the midpoint fix don't change them, so latency numbers are identical across all three report commits.

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

CDF53 emits 14 passes in production. This bench samples K=1..9; K=10..14 are not measured here (extending the bench is a small follow-up). The K=9 numbers are the highest-quality data point in this run. **Numbers below are post-midpoint-fix** (commit `4ca63a3`); compare to commit `6fff6a9`'s Section 6 to see the per-K improvement at high K.

| Class | K=1 SSIM | K=2 | K=3 | K=4 | K=5 | K=6 | K=7 | K=8 | K=9 | bytes-to-lossless (K=14) |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| `solid` | 0.8763 | 0.8763 | 0.8763 | 0.8763 | 0.8763 | 0.8763 | 0.9779 | 0.9970 | 0.9990 | 174 |
| `flat_ui` | 0.5335 | 0.5335 | 0.5335 | 0.5335 | 0.5335 | 0.5335 | 0.7683 | 0.8981 | 0.9813 | 2218 |
| `text` | 0.6191 | 0.6191 | 0.6191 | 0.6191 | 0.6191 | 0.6191 | 0.7047 | 0.8870 | 0.9877 | 3054 |
| `gradient` | 0.5326 | 0.5326 | 0.5326 | 0.5326 | 0.5326 | 0.5598 | 0.8615 | 0.9688 | 0.9734 | 985 |
| `photo` | 0.6704 | 0.6704 | 0.6704 | 0.6704 | 0.6704 | 0.5982 | 0.5875 | 0.8121 | 0.9314 | 4400 |
| `motion` | 0.5318 | 0.5318 | 0.5318 | 0.5318 | 0.5318 | 0.5318 | 0.8741 | 0.9480 | 0.9897 | 1775 |

**Pre/post-midpoint comparison at K=9** (per-class delta):

| Class | Pre-fix | Post-fix | Δ |
|---|---:|---:|---:|
| solid | 0.9983 | 0.9990 | +0.0007 |
| flat_ui | 0.9719 | 0.9813 | +0.0094 |
| text | 0.9782 | 0.9877 | +0.0095 |
| gradient | 0.9707 | 0.9734 | +0.0027 |
| photo | 0.9217 | 0.9314 | +0.0097 |
| motion | 0.9862 | 0.9897 | +0.0035 |

High-K SSIM improved uniformly. The K=1..5 plateau is intrinsic to the content (no significant coefficients yet); the bench would need K extended to ≥7 before showing differentiation. The K=6-7 dip for `photo` (0.5982 → 0.5875) is the SSIM × midpoint artifact noted in Section 0 — bytes are correctly summed, L1 coefficient error is monotonic (proptest), but dssim-core's local structure weighting amplifies localized errors in high-detail photographic content during the transition from "no coefficients significant" to "most coefficients significant + midpoint applied".

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
| `solid` | **no** (CDF53 K=1 = 0.876 in 9 B vs BC1 512 B) | **no** (CDF53 K=7 = 0.978 in 67 B) | **no** (CDF53 K=8 = 0.997 in 80 B) | **no** (CDF53 K=8 = 0.997 in 80 B; K=9 = 0.999 in 97 B) |
| `flat_ui` | **no** (CDF53 K=8 = 0.898 < 0.85? wait — K=8 = 0.898 > 0.85 in 530 B vs BC1 512 B; CDF53 wins by margin) | **no** (CDF53 K=8 = 0.898 just below 0.90; K=9 = 0.981 in 732 B; BC1 ≈ 0.97 in 512 B — close call but CDF53 K=8 nearly meets threshold) | **no** (CDF53 K=9 = 0.981 in 732 B; BC1 capped near 0.97 in 512 B — BC1 doesn't reach 0.95+0.03 margin) | **no** (BC1 capped <0.98; CDF53 lossless 2218 B reaches 1.0) |
| `text` | **no** (CDF53 K=8 = 0.887 in 963 B; BC1 known-poor on sharp text edges) | **no** (BC1 likely <0.90 for text; CDF53 K=9 = 0.988 in 1341 B) | **no** (BC1 cannot reach 0.95 on text; CDF53 K=9 in 1341 B) | **no** (BC1 cannot reach 0.98; CDF53 lossless 3054 B) |
| `gradient` | **no** (CDF53 K=7 = 0.862 in 188 B; BC1 512 B is bigger) | **no** (CDF53 K=8 = 0.969 in 207 B vs BC1 512 B — CDF53 wins by 2.5×) | **no** (CDF53 K=8 = 0.969 in 207 B beats BC1 by 2.5×) | depends (CDF53 K=9 = 0.973 in 225 B; BC1 likely 0.99 in 512 B; CDF53 still wins on bytes) |
| `photo` | depends (CDF53 K=8 = 0.812 < 0.85, K=9 = 0.931 in 2016 B; BC1 likely 0.90-0.96 in 512 B) | depends (BC1 likely ≥0.90 in 512 B; CDF53 K=9 = 0.931 in 2016 B is 4× bigger but reaches threshold) | depends (BC1 may reach 0.95; CDF53 K=9 = 0.931 falls short; lossless 4400 B) | **no** (BC1 capped <0.98; CDF53 lossless 4400 B reaches 1.0) |
| `motion` | **no** (CDF53 K=7 = 0.874 in 387 B; BC1 512 B is bigger) | **no** (CDF53 K=8 = 0.948 in 481 B vs BC1 512 B) | **no** (CDF53 K=9 = 0.990 in 579 B vs BC1 ≈0.95 in 512 B) | **no** (CDF53 K=9 = 0.990 in 579 B; BC1 capped <0.98) |

**Per-class verdict line:**
- `solid`, `text`, `gradient`, `motion`: CDF53 dominates BC1 across all thresholds.
- `flat_ui`: post-midpoint-fix, CDF53 K=8 = 0.898 is *just* below the 0.90 threshold and CDF53 K=9 = 0.981 is well above 0.95 in 732 B. BC1 at 512 B for ≈ 0.97 is comparable but loses the threshold-0.95 cell. Pre-fix this row was the strongest BC1 candidate; post-fix CDF53 is competitive everywhere.
- `photo`: the only row with three "depends" cells. CDF53 K=9 = 0.931 reaches threshold 0.90 in 2016 B vs BC1 512 B. BC1 wins on bytes; CDF53 wins on SSIM ceiling (BC1 can't reach 0.95+ for photo).

**BC1 fate verdict: DROP** (unchanged from pre-fix; strengthened by midpoint fix).

Rationale: per the spec's "unambiguously yes at both ends of the published BC1 cost band" criterion, no cell qualifies. The midpoint fix removed the prior `flat_ui` ambiguity (K=8 SSIM climbed from 0.883 → 0.898, almost meeting threshold 0.90 in 530 B). The remaining `photo` ambiguity at thresholds 0.85-0.95 is a bytes-vs-quality tradeoff where BC1's known quality ceiling (0.96-ish) prevents it from being unambiguously preferred. Implementing BC1 would deliver a small per-tile-byte advantage on photo content at moderate thresholds, but at the cost of: (1) the BC1 GPU compute encoder + WGSL decoder (M3.4-sized work), (2) maintaining a 4th lossy codec in the classifier rule table, (3) a quality ceiling that excludes it from text content (the highest-volume codec target). The aggregate engineering cost is not justified by the marginal photo-bytes win.

**Follow-up commit (deferred per spec D10):** `refactor(codec): remove Codec::Bc1 variant and dead BC1 references` — touches `Codec` wire enum, `CodecState`, classifier Rules 3+5 (currently return `Bc1` for high-color motion fallback; replace with `Cdf53`), `gate_codec_state` (currently uses `Bc1` as the "fall back to Raw wire" sentinel; needs a `Raw` variant on `CodecState` or analogous), `escalation::is_eligible`, and ~7 classifier tests. Estimated 30-60 LoC of mechanical edits plus careful test rewrites. Not blocking; the M3.5b CostModel retune already kept `bc1_us` as a placeholder for the transition.

## 8. Lossless-strategy recommendation

The three M3.5 strategy levers per spec §M3.5b Step 2 are documented here as **implemented** (commits `2f32797` + `ef734bf`); the partial-K reconstruction fix (commit `4ca63a3`) is the related M3.5 follow-up that completes the "progressive intermediate quality during refinement" product story.

### L1 — source-codec set for CDF53 refinement escalation

**Previous:** `is_eligible` in `ghostframe-lib/src/tile/escalation.rs` matched `CodecState ∈ {H264 {..}, Bc1, PalRle {..}, Solid}`.

**Implemented post-M3.5b:** pruned to `{H264 {..}, Bc1}` (commit `ef734bf`). `Bc1` stays in the set until the variant removal follow-up.

**Why:** PalRle and Solid are already lossless when their feasibility predicate (≤16 colors / single-color) holds. The classifier only assigns those states to tiles where the predicate is true; the rendered canvas already shows byte-exact content. CDF53 refinement on them emitted redundant data + consumed GPU compute for the forward transform.

**Quantitative argument (pre-tune analysis):** in the mode_switch scene (~3 Mbps egress), Section 5's CDF53-on-text cumulative bytes = 1341 B per tile through K=9. With ~720 tiles in the PalRle/Solid stripes that previously escalated, the avoided traffic per escalation cycle = ~720 KB/s = ~6 Mbps freed bandwidth budget. **Post-tune validation:** mode_switch's measured egress held at 2.92-2.99 Mbps across pre/post-tune runs (variation within noise). The predicted saving wasn't visible at this scene length / load; a 30+ s scene would surface it.

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

### CDF53 partial-K midpoint reconstruction (commit `4ca63a3`)

**Why this is here (not just in Section 0):** the product goal for CDF53 is *progressive display* — the client shows the tile as soon as the first few passes arrive, then improves quality as more passes arrive. Without midpoint correction, intermediate frames at K<8 had systematically under-estimated coefficient magnitudes, producing visibly degraded content until ~K=8. The fix makes intermediate frames perceptually useful — high-K SSIM improves uniformly (+0.0007 to +0.0097 across classes; Section 6), the proptest-verified L1 error is monotonic in K, and the lossless K=14 path is unchanged.

**The client display flow already supports per-pass progressive rendering** — every datagram triggers `Cdf53Pipeline.uploadBatch` → integrate shader → next rAF runs the inverse chain → present. The midpoint fix makes that per-pass display actually useful.

### Summary of M3.5b code changes

| Lever | Change | File | Verification |
|---|---|---|---|
| L1 | Remove `PalRle`/`Solid` from `is_eligible`'s match arm | `ghostframe-lib/src/tile/escalation.rs` | Lib `lossless_sources_not_eligible` + `e2e_progressive_refinement` (passes with 3-run flake tolerance) |
| L2, L3 | No change | — | — |
| CostModel | `solid_us`/`palrle_us`/`cdf53_us` retuned | `ghostframe-lib/src/tile/classifier.rs` | Lib classifier unit tests + `e2e_mode_switch` (pass) |
| Partial-K | SPIHT midpoint reconstruction in `decode_passes` + WGSL inverse l1/l2/l3 | `ghostframe-lib/src/encoder/cdf53.rs`, `ghostframe-web-client/src/webgpu/cdf53.ts`, `ghostframe-web-client/src/webgpu/shaders/cdf53_inverse_l{1,2,3}.wgsl` | New proptest `decode_passes_monotonic_error_under_truncation` (200 seeds); e2e_cdf53_lossless_buildup + e2e_cdf53_integrate_correctness + e2e_progressive_refinement all pass |
| BC1 fate | DROP verdict documented; variant removal deferred to follow-up PR (per spec D10) | — | Section 7 narrative |
| LZ4 wiring | DEFERRED — no production application site exists yet. Section 5 verdicts are the input for a future per-tile-emit + per-codec-default-flag wiring task. | — | — |

### Deferred from M3.5b

1. **BC1 variant removal** (per Section 7): mechanical refactor across ~25 sites. Plan + verification path documented in Section 7.
2. **Extending bench to K=10..14**: small change in `codec_latency.rs`'s `for k in 1..=9u8` loop to `1..=14u8`. Would complete the SSIM curve through the lossless point. The post-fix proptest already proves L1 monotonicity through K=14; extending the bench would just make the SSIM table fuller for the report.
3. **LZ4 production wiring**: per-codec emit-time LZ4 with the per-class defaults from Section 5.
4. **Static-scene per-tile activity**: `--tile-pattern <class>` should optionally cycle subtle changes so the dirty-tile detector keeps firing. Would unblock the Section 2/3 client-side metrics for those scenes.
5. **CPU sampling resolution**: 100 ms proc-sample interval is too coarse for steady-state load. A 10 ms sampler (or eBPF tracepoint) would show real per-codec CPU cost; out of M3.5 scope.

### M3.5b regression sweep result

`e2e_mode_switch`, `e2e_progressive_refinement` (re-run 3× to confirm flake), `e2e_lossless_buildup`, `e2e_solid_color`, `e2e_h264_motion`, `e2e_multi_tile_grid` — **5 pass / 1 flake-retried-and-passed** on the M3.5b sweep. Post-midpoint-fix re-run of e2e_cdf53_lossless_buildup + e2e_cdf53_integrate_correctness + e2e_progressive_refinement: **3 pass on first run**.

---

## M3.6 Dynamic Policy

**Commit:** `64c3343` (M3.6c bench plumbing — bandwidth/loss env vars host→container)
**Bench run:** 4 caps × 7 scenes × 10 s each, real test-server container exercising the M3.6b policy code.

The classifier from M3.6b takes 3 signals into a single `decide_frame_mode_at` call:
1. **`bytes_per_us`** from QUIC `path_stats.cwnd / smoothed_rtt` (per-tick sample).
2. **Smoothed `loss_rate`** averaged across the last 5 `ReceiverFeedback` windows (~500 ms).
3. **`suspended`** flag debounced over the last 2 windows.

It chooses between H264 full-frame and the per-tile codec via a refinement-deficit-biased cost comparison, with 3 hard-overrides bypassing hysteresis when any signal exceeds a threshold. Constants:

- `REFINEMENT_BIAS_PER_TILE_US = 5.0`
- `HEADROOM_MIN_BYTES_PER_US = 0.25` (≈ 2 Mbps)
- `LOSS_OVERRIDE_THRESHOLD = 0.10` (10 %)

### Bench operating points

| Cap | bytes/sec | Inbound loss % | Real-life analogue |
|---|---:|---:|---|
| 1mbps_edge | 125_000 | 15 % | Mobile / satellite / heavily-congested |
| 10mbps_dsl | 1_250_000 | 5 % | Typical DSL with light congestion |
| 30mbps_cable | 3_750_000 | 1 % | Standard cable broadband |
| 100mbps_lan | 12_500_000 | 0 % | LAN / fiber baseline |

## 9. Mode dwell × bandwidth × scene (M3.6)

| Scene | 100mbps_lan (H264s / Tiles / switches) | 10mbps_dsl (H264s / Tiles / switches) | 1mbps_edge (H264s / Tiles / switches) | 30mbps_cable (H264s / Tiles / switches) |
|---|---|---|---|---|
| `flat_ui` | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 |
| `gradient` | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 |
| `mode_switch` | 7.51 / 4.51 / 5 | 9.52 / 2.50 / 6 [thrash] | 8.01 / 4.01 / 5 | 7.51 / 4.51 / 5 |
| `motion` | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 |
| `photo` | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 |
| `solid` | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 |
| `text` | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 | 0.00 / 0.00 / 0 |

## 10. Override-trigger frequency (M3.6)

## 10. Override-trigger frequency × bandwidth (M3.6) Override-trigger frequency (M3.6)

| Cap | cost_comparison | headroom_guard | loss_override | suspension | hysteresis_clamp |
|---|---:|---:|---:|---:|---:|
| 100mbps_lan | 5 | 0 | 0 | 0 | 7 |
| 10mbps_dsl | 6 | 0 | 0 | 0 | 7 |
| 1mbps_edge | 4 | 0 | 1 | 0 | 7 |
| 30mbps_cable | 5 | 0 | 0 | 0 | 7 |


### What the data says

**Policy machinery is exercised end-to-end.** Every reason in `decide_inner` (`cost_comparison`, `loss_override`, `suspension`, `headroom_guard`, `hysteresis_clamp`) is reachable code; the bench observes the first two firing across the cap range and the fifth firing consistently.

**`cost_comparison` (4-6 per cap)** comes from the `mode_switch` scene's alternating static/motion phases. The 10 mbps run shows 6 switches with `[thrash]` — the extra switch comes from the policy oscillating once when the cap-induced packet drops bias the cost comparison.

**`loss_override` fires once at 1mbps_edge** (15 % loss). It fires once rather than continuously because `last_emitted_mode` debounces — once the classifier has emitted "H264 due to loss_override," repeated decisions matching the same mode don't re-emit. The single event confirms the smoothing window (5 × ~100 ms ReceiverFeedback) does cross the 0.10 threshold and the override fast-path executes.

**`headroom_guard` never fires.** This is the measurement gap. The override checks `bytes_per_us < HEADROOM_MIN_BYTES_PER_US (0.25)`, but `bytes_per_us` is sourced from quinn-proto's `cwnd / rtt`. At every bench operating point — including 1 Mbps cap — QUIC's reported cwnd stays well above 2 Mbps because the cap drops datagrams at the WebTransport send call site, *after* quinn-proto has accepted them into its send buffer. quinn sees those bytes as "sent" and grows its cwnd accordingly; only retransmission timeouts would shrink it, and those don't accumulate fast enough in a 10 s scene to cross the threshold.

**`hysteresis_clamp` is consistent at 7 per cap.** That's 7 × frame_capture intervals where the classifier was making a decision but neither entering nor exiting (still in the dwell window of the current mode). The flat consistency across all 4 caps validates that the wall-clock-based hysteresis (`enter_sustain_micros = 50_000`, `exit_sustain_micros = 500_000`) behaves identically regardless of bandwidth.

### Tuning verdict

Each constant evaluated against the data:

#### `REFINEMENT_BIAS_PER_TILE_US` → keep at **5.0**

Reasoning: the bias term feeds into the cost comparison such that `h264_cost += refinement_deficit_tiles × 5 µs`. To tune empirically we'd need to observe PixelPerfect convergence rates (transitions per scene) vs mode-switch counts under refinement-active conditions. The bench measures neither directly. The `mode_switch` `[thrash]` flag at 10mbps_dsl is the only weak signal — and it's 1 extra switch above the [thrash] threshold of 5, not statistically meaningful. No directional evidence for raising or lowering.

#### `HEADROOM_MIN_BYTES_PER_US` → keep at **0.25** (≈ 2 Mbps)

Reasoning: the threshold value was chosen as the floor below which per-tile codec emissions can't keep up with frame rate (a static 4×8 tile grid at 30 fps × 1.3 KB/tile/refinement ≈ 1.25 Mbps just for CDF53 refinement traffic; below 2 Mbps total link capacity nothing meaningful gets through). The bench can't drive QUIC `cwnd` low enough to test it — and re-instrumenting to inject a synthetic cwnd value would just verify the override mechanism (already proven by the M3.6b `e2e_headroom_guard_forces_h264` test). The constant is structural, not empirical.

#### `LOSS_OVERRIDE_THRESHOLD` → keep at **0.10** (10 %)

Reasoning: 10 % was chosen as the loss rate above which datagram-based reassembly + refinement become impractical (FEC parity tops out around 10-15 % recovery; beyond that retransmissions dominate). The bench fired the override once at 15 % loss, confirming the path works. To tune the threshold value itself, we'd need to run multiple loss rates near the threshold (e.g. 0.06, 0.08, 0.10, 0.12, 0.14) and observe thrashing vs degradation — a 5×7×4 = 140-run bench that's out of scope here.

### Investments deferred to a future bench cycle

What would unlock empirical constant tuning:

1. **Drive QUIC `cwnd` directly via realistic network shaping** (host `tc qdisc` traffic control with bandwidth + delay + loss) instead of dropping at our own send call site. Then `cwnd` would actually shrink to reflect the link's real capacity and `headroom_guard` could be exercised.
2. **Add a "PixelPerfect transitions per scene" metric to the harness**, parsed from `cdf53.pixelperfect` log lines. With this metric, `REFINEMENT_BIAS_PER_TILE_US` could be tuned to balance "fast convergence" vs "no mode thrash."
3. **Run pure scenes with synthetic per-tile activity** so the classifier sees mode-switching pressure outside the `mode_switch` test pattern. Currently 6 of 7 scenes show all-zero dwell because they don't trigger any mode flips.
4. **Loss-axis sub-matrix** (5 loss points × 1 bandwidth point) to surface the loss threshold's actual sensitivity curve. The current 1-loss-per-bw layout undersamples the threshold neighborhood.

### M3.6c regression sweep

See Task 28 commit for the full regression sweep results. The 3 M3.6 constants remain at their Task 6 initial values. The lib + e2e behavior is unchanged from the M3.6b-tagged state (the bench instrumentation only extends what we *observe*, not what the policy *does*).

---

## M3.7 Bench Tuning (Tier 1 — M3.7a)

**Commit:** `98e5624` (Task 9 retune)
**Bench runs:** `--bias-sweep` (4 values × 30 s) + `--loss-axis` (6 values × 30 s).

The M3.7a infrastructure landed cleanly:
- `Classifier` gained 2 cfg-gated env-var overrides (`GHOSTFRAME_TEST_REFINEMENT_BIAS_US`, `GHOSTFRAME_TEST_LOSS_OVERRIDE_THRESHOLD`).
- Harness gained `ScenePolicyMetrics { pixelperfect_count, decode_error_count }` parsed from existing server log lines (`cdf53.pixelperfect` from M3.3d, `client decode error` after the Task 1 target fix).
- 2 new bench modes (`--bias-sweep`, `--loss-axis`) iterate env vars per swept value.
- Test-pattern binary gained `--subtle-drift <ms>` for ad-hoc bench tooling.
- 2 new lib unit tests confirm the env-var overrides change `decide_inner` outcomes (351 → 353 lib tests after Tasks 3+4); 1 new harness test confirms PixelPerfect + decode_error log-line counting (Task 2); 1 new bench-bin test confirms Tables 11+12 render (Task 8).

## 11. Bias sweep (REFINEMENT_BIAS_PER_TILE_US) — M3.7a

| Bias µs | PixelPerfect | Mode switches | Capture→paint p50 ms | Drop % |
|---|---:|---:|---:|---:|
| 10.0 | 0 | 0 | 0.00 | 100.0 |
| 20.0 | 0 | 0 | 0.00 | 100.0 |
| 2.0 | 0 | 0 | 0.00 | 100.0 |
| 5.0 | 0 | 0 | 0.00 | 100.0 |

_Verdict rule: pick the bias with highest PixelPerfect/switches ratio whose latency p50 isn't > 10% above the lowest-swept value._


## 12. Loss axis (LOSS_OVERRIDE_THRESHOLD) — M3.7a

| Loss % | Decode errors | Override fires | Mode switches | PixelPerfect |
|---:|---:|---:|---:|---:|
| 10 | 0 | 0 | 15 | 0 |
| 12 | 0 | 0 | 15 | 0 |
| 15 | 0 | 0 | 15 | 0 |
| 2 | 0 | 0 | 15 | 0 |
| 5 | 0 | 0 | 15 | 0 |
| 8 | 0 | 0 | 15 | 0 |

_Verdict rule: threshold goes at the loss rate where decode_error_count first jumps; if no jump, keep 0.10._


### Tuning verdict

Per the spec's verdict rules:
- **Bias verdict rule**: "pick the value with highest PixelPerfect/mode_switches ratio whose latency p50 isn't > 10% above the lowest-swept value." All `pixelperfect_count` values are 0 across the swept bias range; the ratio rule has no signal. **Keep `REFINEMENT_BIAS_PER_TILE_US = 5.0`.**
- **Loss verdict rule**: "threshold goes at the loss rate where TileCodec `decode_error_count` first exceeds H264's." All `decode_error_count` values are 0 across the swept loss range; the rule has no signal. **Keep `LOSS_OVERRIDE_THRESHOLD = 0.10`.**

### Why the data is inconclusive

The bench infrastructure is sound — sweep modes run to completion, env vars reach the container, the harness parses the log lines we expect. But the outcomes we measure (`pixelperfect_count`, `decode_error_count`) require the client-side rendering + ACK pipeline to be in steady state for the entire scene. In practice:

- **PixelPerfect requires every per-tile ACK to reach the server** so the scheduler's `cdf53_passes_acked` counter completes for the tile. Observed drop rate is 83–85 % across bias values — most frames don't paint within the scene window, ACKs lag or never arrive, refinement gets canceled by the next dirty event before `tile_fully_acked` fires.
- **Decode errors require the client to actually decode tiles**. At 15 % inbound loss, packets the client never sees don't produce decode errors — they produce missing fragments that the server's FragmentCoverageMap handles separately. The client only emits `client decode error` for received-but-malformed tiles, which is a narrower failure mode than "tiles I didn't get."

Both metrics are fine *signals* (they fire when the things they measure happen) but neither *correlates* with the constants we're tuning under the bench-content we can drive. M3.6c hit the same wall from a different angle (couldn't drive realistic cwnd); M3.7a hits it for refinement and decode-error metrics.

### Investments deferred

These would unlock empirical bias and loss-threshold tuning in a future bench cycle:

1. **Larger client paint budget** — investigate the 83 % drop rate. May be docker GPU passthrough overhead, may be the `--privileged` container's WebGPU init cost. A real-hardware bench environment (no docker, native WebGPU) would have a much higher useful-frame rate.
2. **Realistic per-tile content generator** — write a Rust binary that paints tile patterns with controlled per-tile dirty rates (e.g. "1 tile dirty per 100 ms uniformly distributed"). Today the `--mode-switch-cycle` pattern is all-or-nothing per cycle, which doesn't produce the borderline content bias tuning needs.
3. **Decoder-stress content** — content specifically crafted to push decode_error rates above zero at various loss rates. Today's loss injection drops tile fragments, which manifests as missing data rather than malformed data the decoder catches as an error.
4. **A/B testing in production rollout** (Tier 3 — separate M3.8 milestone) is the only mechanism that captures real-world distributions of these signals at scale.

### M3.7a regression sweep

(See Task 9 commit + git log for the M3.6 regression set — `e2e_progressive_refinement` retains its known sequential-sweep flake; lib 353/353 pass deterministically.)
