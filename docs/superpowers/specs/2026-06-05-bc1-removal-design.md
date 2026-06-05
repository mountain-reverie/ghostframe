# BC1 Removal — Design

**Status:** approved 2026-06-05
**Branch:** `refactor/bc1-removal` (suggested; pick at execution time)
**Motivation:** M3.5b bench data verdict (DROP) in `docs/specs/m3-codec-bench-results.md` §7.

## 1. Goal & scope

Remove BC1 from the codebase. BC1 was the M3.2-era high-color tile-codec fallback. M3.5b's bench data showed CDF53 dominates BC1 across every measured (class, threshold) cell, and BC1's quality ceiling on text content rules it out of the highest-volume codec target. BC1 was never implemented end-to-end (no encoder, no WGSL decoder); the variant has lingered only as a classifier-state placeholder and a wire-enum reservation.

### Scope IN

- Wire enum: compact `Codec::{Solid 4→3, Raw 5→4, Cdf53 6→5}`. `Bc1 = 3` discriminant disappears.
- Variant deletion: `CodecState::Bc1` and `CostModel::bc1_us`.
- Gating: delete `cdf53_enabled` field, `GHOSTFRAME_ENABLE_CDF53` env var, and `gate_codec_state()` entirely. CDF53 becomes baseline-mandatory.
- HELLO: drop the `supports_cdf53` capability bit (bit 1 → reserved).
- Classifier Rules 3 & 5 high-color fallback: `Bc1` → `Cdf53 { passes_sent: 0, max_passes: CDF53_PASS_COUNT }`.
- Escalation source-codec set: `{H264, Bc1}` → `{H264}`.
- Tests: ~7 classifier-rule tests rewritten; 1 proptest strategy arm dropped; 1 fragment_coverage test rewritten; HELLO cap-bit tests deleted/rewritten; doc-comment scrubs across io_bridge / scheduler / encoder / fragment_coverage.
- Web client: `decoder.ts` enum mirror updated; `dist/` rebuilt.
- Bench: `bc1_us` field removed from `CostModel`; codec-report Section 7 collapses to a stub pointing at git history; the handwritten narrative in `docs/specs/m3-codec-bench-results.md` is preserved as the historical record.

### Scope OUT

- LZ4 wiring (deferred M3.5b follow-up; tracked separately).
- Any new codec, classifier rule, or escalation policy change beyond the mechanical Rule-3/5 fallback redirect.
- Backwards compatibility with clients on the old wire bytes — pre-1.0 project, both ends controlled.

### Risk surface

Two wire-format breaks land in this branch:
1. HELLO byte semantics (bit 1 ceases to be `supports_cdf53`; becomes reserved).
2. Tile-codec discriminant compaction (Solid/Raw/Cdf53 byte values shift).

Mitigated by the layered 5-commit landing (Section 3). Each commit independently buildable + green.

## 2. Decisions

1. **Wire enum: compact, not reserve.** `Bc1 = 3` is removed and Solid/Raw/Cdf53 shift down. The discriminant `3` does not become a permanent hole. Both ends are controlled, both update together.
2. **CDF53 is mandatory, not gated.** The env-var operator knob and the HELLO capability bit both go away. No client/server mismatch is possible — clients that don't speak CDF53 simply don't exist after this change.
3. **No new `CodecState::Raw` sentinel.** The earlier candidate "add a Raw variant to replace the Bc1-as-gate-sentinel role" is unnecessary once the gate itself is removed.
4. **Rule 3 / 5 fallback is `Cdf53`, not Raw.** When the classifier picks the high-color or unknown-color branch, the tile enters CDF53 refinement directly. The io_bridge `_ => Raw` catch-all stops firing for those tiles — they emit through the full CDF53 pipeline.
5. **Escalation source-set narrows to `{H264}`.** With BC1 gone and PalRle/Solid already pruned in M3.5b, only full-frame H.264 snapshots remain as lossy sources that need idle-escalation refinement.
6. **Bench Section 7 collapses to a stub.** The handwritten narrative in `docs/specs/m3-codec-bench-results.md` §7 is the historical record. The generated report's Section 7 becomes a single line pointing at git history so Section numbering stays stable.

## 3. Commit sequence

Five small commits, each independently buildable + green. Each estimated under ~50 LoC.

### Commit 1 — Make CDF53 always-on

- `ghostframe-lib/src/transport/io_bridge.rs`: delete the `cdf53_enabled: bool` field, the env-var read block, and the field initialisers (production + test fixtures). At call sites that check `self.cdf53_enabled && caps.supports_cdf53`, collapse to `caps.supports_cdf53` (kept temporarily so Commit 2 can collapse it the rest of the way without conflicting with Commit 1's diff). Delete the `gate_codec_state(...)` call sites.
- `ghostframe-lib/src/tile/classifier.rs`: delete the entire `gate_codec_state` function.
- Doc-comment scrub in `io_bridge.rs`: the comments at lines 238 and 760 both reference `gate_codec_state`, which no longer exists after this commit — delete or rewrite both. These comments also mention `Bc1`, but they're addressed here (not in Commit 4) because their primary subject is the gate, not the variant.
- Tests: drop any test fixture line setting `cdf53_enabled: false`; delete any test whose only purpose was gate-closed behaviour.

**Behavioural change:** the `GHOSTFRAME_ENABLE_CDF53` operator rollback knob disappears. CDF53 is always wired.

**Independently green because:** the classifier still produces `CodecState::Bc1` from Rules 3/5 in this commit, and the `_ => Raw` io_bridge catch-all still maps it to a Raw wire emission. No behaviour shift for those tiles yet.

### Commit 2 — Drop HELLO `supports_cdf53` bit

- `ghostframe-lib/src/transport/client_caps.rs`: remove the `supports_cdf53` field from `ClientCapabilities`; drop bit 1 from `encode()` and `decode()`; update module doc to mark bit 1 as reserved. Delete/rewrite the cap-bit-specific tests (`decode_enables_supports_cdf53`, `decode_supports_cdf53_only`, `encode_roundtrip_both_caps`, `decode_legacy_client_has_no_cdf53_support`). Extend `decode_ignores_reserved_bits` to cover bit 1.
- `ghostframe-lib/src/transport/io_bridge.rs`: collapse the remaining `caps.supports_cdf53` consultations (the half-step from Commit 1) to unconditional CDF53 paths.
- `ghostframe-web-client/src/feedback.ts`: remove `supportsCdf53` field + bit-1 encode line.
- `ghostframe-web-client/src/main.ts:348`: drop `supportsCdf53: true` from the HELLO encode call.

**Wire change #1:** HELLO bit 1 reserved going forward.

**Independently green because:** the encoded HELLO byte simply has bit 1 cleared; the server no longer reads it. Web-client builds without the field.

### Commit 3 — Rewire classifier Rules 3 & 5 + escalation prune

- `ghostframe-lib/src/tile/classifier.rs`:
  - Rule 3 (line 184–190): the `!uc_known || unique_colors > 16` branch returns `CodecState::Cdf53 { passes_sent: 0, max_passes: CDF53_PASS_COUNT as u8 }` instead of `Bc1`.
  - Rule 5 (line 196–209): same redirect for both the high-color and unknown-color fallback branches.
  - Update rule-region doc comments (lines 144–149, 192–195, 221).
  - Keep `bc1_us` field and `Bc1` match arms in `estimated_tile_us` / `estimated_tile_bytes` for now — variant deletion is Commit 4.
- `ghostframe-lib/src/tile/classifier_classify_tests.rs`: rewrite the seven `_picks_bc1` / `_falls_back_to_bc1` tests to assert `Cdf53 { passes_sent: 0, max_passes: ... }`. For `_prefers_solid_over_bc1` and `_prefers_palrle_over_bc1`, change the `prev` parameter from `CodecState::Bc1` to `CodecState::Cdf53 { .. }` (since `Bc1` is going away in Commit 4 and the test's intent — "previous lossy emission doesn't suppress the lossless rule" — is unchanged).
- `ghostframe-lib/src/tile/escalation.rs`: `is_eligible()` drops `CodecState::Bc1` from the match; source-codec set becomes `{H264 { .. }}` only. Update module-level doc comments. Rewrite the `both_remaining_lossy_sources_eligible` test to `only_h264_is_eligible`.
- `ghostframe-lib/src/tile/proptest_strategies.rs:89`: delete the `Just(CodecState::Bc1)` arm.

**Behavioural change at runtime:** tiles that previously fell to BC1 (and out as Raw on the wire) now enter the CDF53 refinement pipeline. The bench data argues this is a net win on bandwidth and quality for every class except photo at low SSIM thresholds.

**Concern:** Rule 3 (high freq + low magnitude + many colors) now produces refinement work for tiles that previously got one Raw blast and were done. CDF53 cumulative bytes are below BC1's 512 B by K=8 for every measured class except photo at SSIM ≤ 0.95 — net win confirmed by the bench narrative.

**Independently green because:** the classifier still produces a *valid* `CodecState`; the variant is still in the enum; io_bridge can still receive any of the existing variants.

### Commit 4 — Delete `CodecState::Bc1` + `CostModel::bc1_us`

- `ghostframe-lib/src/tile/mod.rs:86`: remove the `Bc1,` variant.
- `ghostframe-lib/src/tile/classifier.rs`: remove `bc1_us` field, its `Default` initialiser and explanatory comments, and the `Bc1` match arms in `estimated_tile_us` / `estimated_tile_bytes`.
- `ghostframe-lib/src/transport/fragment_coverage.rs`: scrub doc comments at lines 116, 265. Rewrite the test fixture at lines 279–289 to use `Codec::Solid` instead of `Codec::Bc1` (the test's intent is "non-Cdf53 entries for the same tile survive a take()" — any non-Cdf53 codec works).
- `ghostframe-lib/src/transport/io_bridge.rs`: scrub the three remaining doc comments mentioning Bc1 (lines 939, 988, 2043; lines 238 and 760 were already scrubbed in Commit 1). The line 2043 comment about "the CPU may have classified this tile as Bc1 (e.g. on the first frame when freq is medium and Rule 5 fires)" becomes "the CPU classified this tile as Cdf53 via Rule 5; the GPU compact list is authoritative".
- `ghostframe-lib/src/transport/scheduler.rs:708`: drop `Bc1` from the codec enumeration in the doc comment.
- `ghostframe-lib/src/encoder/pal_rle.rs:11`: change `"through the classifier to BC1/Cdf53"` to `"to Cdf53"`.
- `ghostframe-bench/src/fixtures/flat_ui.rs:5`, `gradient.rs:4`, `benches/codec_latency.rs:3`: scrub BC1 mentions from fixture/bench module doc strings.

**Compiler-driven sweep:** removing `CodecState::Bc1` will trigger Rust's exhaustiveness check at every remaining match site. If anything's been missed in Commits 1–3, the compiler points at it. Fix in place.

**Independently green because:** by this point, no code produces `Bc1` (Commit 3 fixed Rules 3/5; Commit 1 deleted `gate_codec_state`). The variant is genuinely unused at runtime.

### Commit 5 — Compact `Codec` wire enum + web client + bench Section 7

- `ghostframe-lib/src/transport/protocol.rs`:
  - `Codec` enum: remove `Bc1 = 3`; renumber `Solid: 4 → 3`, `Raw: 5 → 4`, `Cdf53: 6 → 5`. `Skip = 0`, `H264 = 1`, `PalRle = 2` are unchanged.
  - `Codec::from_u8`: drop the `3 => Ok(Codec::Bc1)` arm; renumber the rest. Bytes 6+ fall to `UnknownCodec`.
  - Update `tile_header_codec_lz4_packing`'s byte assertion: `Codec::Raw = 4` so `(4 << 1) | 1 = 9` (was 11).
- `ghostframe-web-client/src/decoder.ts:17`: update the enum literal to match — `Skip = 0, H264 = 1, PalRle = 2, Solid = 3, Raw = 4, Cdf53 = 5`. Grep for raw integer comparisons against the codec field; update if any. Rebuild `dist/` (e2e tests reference the prebuilt bundle — see `feedback_e2e_web_client_dist` memory).
- `ghostframe-bench/src/bin/codec_report/report.rs`: Section 7 collapses to a stub: `## 7. BC1 (removed)` + a single sentence pointing at git history. Section numbering stays stable.
- `ghostframe-bench/tests/codec_report_smoke.rs:32`: update the header-contains assertion to match the new Section 7 line.

**Wire change #2:** any stale client emitting `Solid=4`, `Raw=5`, or `Cdf53=6` will decode as different codecs (or `UnknownCodec`) on the new server. No migration path; both ends update together. Pre-1.0 controlled-deployment context makes this acceptable.

**Independently green because:** lib tests + web-client build + e2e sweep all gate this commit. The wire change is local — once both ends update, the test suite's tile-roundtrip + e2e coverage exercises every codec byte.

## 4. Testing strategy

### Per-commit gate

Each commit ends green on:
- `cargo test -p ghostframe-lib`
- The e2e CDF53 sweep (the cheap subset that doesn't need Docker is the local signal; the full Docker sweep gates the merge).

### Cross-cutting verification at end of branch

- `cargo test --workspace` (332+ lib tests).
- E2E sweep: `e2e_cdf53_*` (9 tests), `e2e_progressive_refinement`, `e2e_mode_switch`, `e2e_lossless_buildup`, `e2e_decode_error_thin_uncached` — all the tests that exercise medium/high-color tile paths that previously fell to BC1-as-Raw.
- Bench codec-report smoke test (Section 7 header assertion).
- `npm install && npm run build` in `ghostframe-web-client/`.
- Final grep: `grep -rn "Bc1\|bc1\|BC1" --include="*.rs" --include="*.wgsl" --include="*.ts" ghostframe-lib ghostframe-bench ghostframe-web-client/src` should return no matches except in `docs/specs/m3-codec-bench-results.md` (historical narrative, preserved).

### Risk areas the e2e sweep specifically checks

1. **`e2e_progressive_refinement`** — medium-frequency high-color tiles now go through CDF53 refinement instead of one-shot Raw. PixelPerfect transition counts should increase or stay the same.
2. **`e2e_mode_switch`** — classifier under load + bandwidth shaping; headroom guard and loss override still trigger correctly when the fallback path produces more CDF53 traffic.
3. **`codec_report_smoke`** — Section 7 header rename.

### No new tests required

Every variant-removal site is already covered by an existing test (seven `_picks_bc1` tests, the escalation test, the fragment_coverage test, the wire-roundtrip tests in `protocol.rs`, the HELLO encode/decode tests). The test work in this design is *rewriting* assertions, not adding new ones — Rust's exhaustiveness checking + existing e2e coverage are sufficient.

## 5. Out-of-scope follow-ups

- LZ4 wiring on Raw / PalRle payloads (M3.5b deferred work).
- `Codec::Bc1` variant removal from any third-party consumers of the FFI header (none expected; this is internal). The FFI header regen runs at the end of the branch.
