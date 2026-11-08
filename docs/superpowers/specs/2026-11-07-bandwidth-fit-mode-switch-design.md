# Bandwidth-fit frame-mode switch

Date: 2026-11-07. Status: approved design, not implemented.

## Problem

On a LAN the classifier flips between the tile codec and H.264 every few
seconds of intermittent motion (live run 2026-11-07 on evangeline: 8 flips in
25 s at a 120-160 Mbit/s estimate). Each exit costs a full lossless repaint.

The cause is `cost_comparison` in `Classifier::decide_core`
(`ghostframe-lib/src/tile/classifier.rs`). It compares *time*:

- tile codec: `sum(estimated_tile_us) + sum(estimated_tile_bytes) / bytes_per_us`
- H.264: `h264_frame_us (3000) + h264_frame_bytes (12 000) / bytes_per_us`

The per-tile microseconds (`cdf53_us = 90`) and the H.264 constants are
M3.5 placeholders. At 41 Mbit/s about 20 dirty CDF 5/3 tiles (1 % of a 1080p
screen, ~6 Mbit/s of tile traffic) cross the 1.3x enter factor; at 150 Mbit/s
about 43 tiles (~13 Mbit/s, 9 % of the link). More bandwidth barely moves the
threshold because the fixed per-tile microseconds dominate.

## Principle

Use H.264 only when the link cannot carry the tile codec's **first pass**
(pass 0, the bytes counted as `bytes_emitted_critical`). Refinement to lossless
may trail behind; it is not part of the decision. Encode time is not a
criterion: the tile path runs on the GPU and the dispatch-latency target is
guarded elsewhere.

## Design

### 1. The rule (replaces `cost_comparison`)

Let `D` be the predicted first-pass demand in bit/s (section 2) and `E` the
bandwidth estimate (`AdaptationContext::bwe_estimate_bps`).

- In tile mode: enter H.264 when `D > ENTER_SHARE x E` has held for
  `ENTER_HOLD` (500 ms) and at least the existing frame streak minimum.
  Reason `bandwidth_short`.
- In H.264 mode: leave when the estimate is known (section 3), `D <
  EXIT_SHARE x E` has held for `EXIT_HOLD` (1 s), and the exit repaint drains
  in time (section 2.3). Reason `bandwidth_fits`.
- Otherwise stay; reason `hysteresis_clamp` as today.

Initial values: `ENTER_SHARE = 0.70`, `EXIT_SHARE = 0.40`. The H.264 rate
already targets 0.8 x the estimate, so 0.7 to enter leaves headroom. These are
`pub(crate)` constants, overridable through `ClassifierConfig` like the
existing knobs, and swept in browserless (section 6) before they are fixed.

Unchanged, and evaluated before the rule in today's order:
`h264_encoder_unavailable`, `test_force`, `h264_unsupported`, `h264_stalled` /
`h264_stalled_holdoff`, `headroom_guard`, `loss_override`, `suspension`.

Removed: the time terms of `CostModel` (`solid_us`, `palrle_us`, `cdf53_us`,
`h264_frame_us`, `h264_frame_bytes`, `bytes_per_us`), `enter_factor`,
`exit_factor`, the refinement bias (`REFINEMENT_BIAS_PER_TILE_US`,
`refinement_bias_us`: refinement is no longer part of the decision), and their
`CostModelConfig` / `ClassifierConfig` fields. The motion fast path
(`motion_tile_*`) is dormant in production (magnitude is always 0) and is
removed with them. Config fields that are removed are rejected with a warning
naming the replacement, following the pattern of the removed H.264 backend
values.

### 2. First-pass demand

#### 2.1 Per-frame prediction

For each frame the classifier sums, over the dirty tiles' tentative states, a
per-state first-pass byte cost `c[state]`. `Skip` and `PixelPerfect` cost 0;
`H264`-classified tiles cost as `Cdf53` (they would be sent as CDF 5/3 in
tile mode). The prediction is made in both modes: in H.264 mode nothing
first-pass is sent, so a measurement alone could never say when to leave.

#### 2.2 Rate and calibration

`D` is the sum of those per-frame predictions over a rolling 1 s window of
frame timestamps, expressed in bit/s (a frame-count-independent rate, so the
capture rate does not matter).

`c[state]` starts at today's `estimated_tile_bytes` (Solid 4, PalRle 200,
Cdf53 1300) and is calibrated in tile mode by an EWMA (weight 1/16) of the
measured pass-0 bytes per tile, per state, taken where `bytes_emitted_critical`
is counted. Calibration only updates from tile-mode frames and never from
retransmissions. The calibrated values survive mode switches and are reset
with the classifier on a session reset.

#### 2.3 The exit repaint

Leaving H.264 triggers `invalidate_baseline` and a full-screen first pass.
Its size is predicted the same way over every tile of the screen (states from
the last classification; tiles never classified count as `Cdf53`). Leaving
also requires `repaint_bytes x 8 / E <= REPAINT_DRAIN_MAX` (1 s). On a slow
link a static screen therefore stays in H.264, where a static P frame costs a
few hundred bytes.

### 3. Known estimate

The estimate is known once both hold:

- it differs from `goog_cc`'s initial value (`bwe::INITIAL_BPS`, 2 Mbit/s);
- at least one probe cluster result or delivery-rate sample has been absorbed
  since the session reset.

The bridge computes this and passes it as `AdaptationContext::estimate_known`.
Until it is known the classifier stays in whatever mode it is in, with
reason `estimate_unknown`. The H.264 bridge fills probe windows with padding,
so the estimate rises on a static screen (the 2026-11-07 log shows 38 Mbit/s
about 2 s after connect). An estimate that never becomes known keeps the
session in H.264: safe, and visible as `estimate_unknown`.

### 4. Session start

The session still starts in H.264 with its IDR (`session.rs`). What changes is
the exit: section 1's rule instead of "30 frames of empty dirty set". On a LAN
that is one IDR and then the tile codec as soon as the estimate is known and
the repaint fits.

### 5. Observability

`mode.decision` (target `ghostframe::bench`) gains `first_pass_demand_bps`,
`estimate_known` and `repaint_drain_ms`, and drops `bytes_per_us` and
`refinement_deficit_tiles`. Reasons `bandwidth_short`, `bandwidth_fits` and
`estimate_unknown` replace `cost_comparison`. `docs/design/h264-transport.md`
and `AGENTS.md` are updated where they name the old reason.

### 6. Testing

- Unit tests in `classifier_decide_tests.rs`: enter above 0.70, exit below
  0.40, no flip inside the band, both holds, the unknown-estimate hold, the
  repaint-drain gate, calibration converging to a measured per-tile size.
  Each test is checked by mutating its gate out and watching it fail.
- Browserless scenes:
  - LAN-rate link (100 Mbit/s), intermittent motion: exactly one
    H.264 to tile transition per session (the start), none after.
  - 2-5 Mbit/s link, sustained full-screen motion: enters H.264 and stays.
  - A link stepping between the two: enters and leaves once per step, not per
    motion burst.
  Reported over 3+ seeds.
- Existing tests asserting `cost_comparison`, `enter_factor` or the motion fast
  path are rewritten against the new rule, not deleted.
- Threshold sweep: `ENTER_SHARE` in {0.6, 0.7, 0.8} x `EXIT_SHARE` in
  {0.3, 0.4, 0.5} over the stepping scene; record flips and time in H.264 per
  setting in the PR body before fixing the constants.

## Out of scope

- The browser client's `GateReset` storm and `h264_stalled` at 18:04 on the
  same log (separate investigation).
- Retuning the headroom guard.
