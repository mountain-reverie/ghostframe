# Design note: scroll detection and copy ops in the tile pipeline

**Status:** proposal (not scheduled) · **Date:** 2026-10-07 · **Context:** [protocol comparative review](../reviews/2026-10-07-protocol-comparative-review.md) §4.4

## Problem

A document scroll is the worst case for the current pipeline: nearly every tile goes dirty
with high magnitude, the classifier sees freq > 15 Hz / mag > 0.3 across the screen, the
frame-mode cost model flips to full-frame H.264, and text goes 4:2:0-blurry until the
scroll stops and idle escalation refines it back over seconds. Yet the new information in
a scrolled frame is one strip of newly-exposed rows — everything else is a translation of
pixels the client already holds.

The goal of scroll detection is therefore **mode preservation**, not just bandwidth: keep
the most common text-heavy interaction in the lossless tile path (blit + one strip of new
tiles) instead of dropping into the lossy H.264 regime.

## Precedent — every lineage has this

| System | Mechanism |
|---|---|
| VNC/RFB | `CopyRect` encoding (protocol since the 90s); detection heuristics in x11vnc (`-scrollcopyrect`, RECORD-extension based) |
| RDP (GDI era) | `ScrBlt` orders — app scrolls arrived as screen-to-screen blits for free |
| RDP (modern, MS-RDPEGFX) | `RDPGFX_SURFACE_TO_SURFACE_PDU` + bitmap cache; AVD mixed-mode pipeline stage one is "delta / motion / cache" before classification |
| SPICE / NX / Guacamole | Copy commands arrive structurally (QXL ops, X `CopyArea`, `copy` instruction) |
| AnyDesk | DeskRT markets "linear translation" exploitation |
| Video-codec camp | Motion vectors handle it natively; AV1 has a dedicated global-motion tool |

Notably, the entire tile/classification camp carries a copy primitive, because without
motion vectors a scroll is their worst case — exactly ours.

## Design sketch

### Detection (GPU, cheap for the vertical-dominant case)

Per tile-column, compute a per-row signature (row sum or row hash) for previous and
current frame — same shape of work as the existing SAD pass. Search a small candidate set
of vertical offsets: last frame's detected offset first (scrolls are coherent across
frames), then ±N rows (N ≈ 128 covers wheel-step and page-scroll granularity). Accept a
region when ≥ some fraction of rows in a contiguous tile-column band match at one offset.
Horizontal scroll is the same transposed pass; defer it — vertical dominates desktop use.

Output: zero or one `(region, dy)` per frame (multi-region scroll detection is explicitly
out of scope; two independently scrolling panes fall back to today's behavior).

### Wire op and ordering — the ghostframe-specific hazard

`CopyRect` is trivially correct over TCP (VNC) or acked frames (RDP). Our datagram path
has no ordering and our reliability model is supersession. A blit is **stateful**: applied
too early (before straggler pre-scroll tiles) or missed entirely, every subsequent delta
is corrupt. Design it as a **generation event with a barrier**:

1. Server emits `CopyOp { frame_seq, region, dy }` reliably — either on the feedback
   stream or as a datagram with its own ACK (the per-transmission ACK machinery makes a
   barrier cheap).
2. Server bumps the generation of every tile in the region (existing
   `cancel_for_tile` + `bump_generation` path), so pre-scroll passes in flight are dropped
   by the existing stale-generation logic — no new client-side ordering rules for them.
3. Server withholds post-scroll tiles for the region until the CopyOp is ACKed
   (a per-region held-queue, bounded by the scheduler's existing structures). On ACK
   timeout, fall back: drop the CopyOp and dispatch the region as ordinary dirty tiles —
   correctness never depends on the blit landing.
4. Client applies: framebuffer blit (both clients render into a persistent GPU
   framebuffer; a texture-to-texture copy is trivial), then accepts the new generation.

### Refinement-state translation (the second-order win)

Scrolled content keeps its coefficients — only its position changed. Instead of
restarting all 14 CDF53 passes at the new location, translate the per-tile bookkeeping by
`dy` where the scroll offset is tile-aligned (dy % 32 == 0): move the refinement-pass
progress, FragmentCoverage state, and PixelPerfect flags with the content. Non-aligned
offsets re-dirty the two straddling tile rows only. This preserves accumulated refinement
across scrolls of in-progress content — the case where restart cost is highest.

Phase 1 can skip this (treat scrolled tiles as new generation at full re-encode within
the tile path); it is still a large win over H.264 mode. Phase 2 adds translation.

### Classifier / mode-policy integration

The detector runs before frame-mode decision. Tiles covered by an accepted CopyOp are
excluded from the dirty set fed to the cost model, so the model prices only the exposed
strip — which keeps the H.264 hard-overrides from firing on scrolls. Scroll frames also
should not advance the per-tile change-frequency EMA for copied tiles (a scrolled text
tile is not "15 Hz content"; letting the EMA rise defeats the purpose by pushing Rule 2
toward H264 after the scroll ends).

## Relation to inter-image delta

Sub-tile delta (RFX-style LL-band delta across generations for CDF53; dirty-row extents
for PalRLE) is the complementary idea with the same precedent family. It is a byte-saver,
not a mode-saver — ranked behind scroll detection because scroll detection changes which
codec path runs, not just the path's cost. Revisit after this lands, using the same bench
scene.

## Bench scene (build first, decide with data)

Add a browserless-harness scene: scrolling text document (continuous wheel-step scroll,
~3 rows/frame, 10 s) at 5/10/30 Mbps shaped links. Metrics:

- bytes/s during scroll (expect: today ≈ full-screen re-encode or H.264 bitrate; with
  CopyOp ≈ exposed-strip only),
- **H.264-mode occupancy** during scroll (expect: today ~100% at ≤10 Mbps; with CopyOp ~0%),
- text SSIM / per-glyph score during and 1 s after scroll,
- PixelPerfect recovery time after scroll stops (phase 2: should be ~0 for tile-aligned).

The scene is the acceptance gate: if CopyOp doesn't move H.264 occupancy and text SSIM on
the 10 Mbps row, the feature doesn't pay for its ordering complexity.

## Open questions

1. Detection threshold/hysteresis against false positives (repeating patterns — terminal
   output with identical lines can alias to a wrong dy; require the exposed-strip check to
   disambiguate: a correct dy predicts the strip, a wrong one doesn't).
2. Interaction with damage hints: the X11 capture path has DamageReport; a scroll reports
   one huge damage rect, so detection must run on pixels, not damage shape.
3. Whether CopyOp should ride the feedback stream (simplest ordering, head-of-line risk
   behind input is negligible at these sizes) or a self-ACKed datagram (consistent with
   the all-video-on-datagrams rule). Lean: feedback stream for phase 1.
4. Native + web client blit paths both exist (persistent framebuffer textures); confirm
   the web path's blit interacts correctly with the dirty-tile-driven inverse dispatch
   once that lands (full review PC-1).
