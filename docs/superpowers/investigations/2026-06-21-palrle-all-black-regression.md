# PalRle "all-black" regression — investigation handover

**Status:** unresolved on `feature/reliable-tile-emitter`. Root cause not isolated. This document captures everything I learned so a follow-up session can resume cleanly.

**Affected tests** (all chromium variants — firefox blocked separately on missing geckodriver):

- `e2e_palette_eviction_chromium` (`--palette-churn 300`, no session reset)
- `e2e_palrle_session_reset_chromium` (`--text-grid --drm-direct`, `browser.new_page` after 4 s)
- `e2e_decode_error_thin_uncached` (`--solid-per-tile --drm-direct`, `setup.page().reload()` after 5 s, `GHOSTFRAME_SKIP_PALETTE_SESSION_RESET=1`)

## Shared symptom

The WebGPU framebuffer at the test-sampled region reads **pure `[0, 0, 0]`** despite the wire showing many PalRle datagrams for the same tiles.

- `palette_eviction`: 4 sample points within the 64×64 churn region at (100, 100); all four read `[0,0,0]` across 8 polls × 250 ms (32 samples total).  Server-side wire shows ~14 PalRle + 1 Solid per region tile in 8 s.
- `palrle_session_reset`: `ink=0, bg=0` at the text-grid sample after `browser.new_page`.
- `decode_error_thin_uncached`: server emits 692 PalRle datagrams total, 395 ACK batches arrive, but `client_decode_error=0` and `force_rebundle=0`.  The `test-hook: preserving palette_table.delivered` log fires exactly once after the reload, confirming the session reset path ran.

## What's already been ruled out

### Not the PalRle code itself

`git log --oneline master..HEAD -- ghostframe-lib/src/encoder/ ghostframe-web-client/src/webgpu/ ghostframe-web-client/src/palette_shadow.ts` returns **empty**.

The PalRle compute pipeline, WGSL shader, palette atlas buffer, and palette shadow are byte-identical to master.  Master is documented to pass the regression sweep cleanly (memory note: *"M3.3 complete on master (2026-06-01) — full e2e_cdf53_* 9/9 + regression sweep clean"*).  Whatever broke these tests lives in the 70 transport / scheduler commits this branch carries.

### Not the CDP-load timeout

Pre-existing failure for `palette_eviction` was `rgb=(0,0,0)` (real assertion failure, not `Error: evaluate` or `Error: screenshot`).  Pre-existing failure for `palrle_session_reset` *was* a CDP timeout — but after the `CAPTURE_FPS_DRM_DIRECT=2` bulk-fix (commit `3f64a18`), the failure changed shape to the assertion failure `ink=0, bg=0`, which is the same symptom the other two show.  So the underlying bug was already there, hidden by the CDP timeout.

### Not a timing-window flake

8 polls × 4 sample coords = 32 reads, all `[0,0,0]`.  The X11 wipe-to-black is 1/6 of each palette-churn iteration; landing 32 reads in wipe windows is statistically ~0.

### Not the new ACK overlap path (alone)

Walked the supersede-then-late-ACK paths in `io_bridge.rs` (lines ~1117 and ~1605).  `fragment_coverage.take` is `remove`-semantics, so the second ACK in an overlap batch hits `None` and skips the palette release.  Guards (`if ref_count > 0`) prevent saturating-sub off-by-one on a redundant release.  My EmitKey + overlap fixes restored cdf53 PixelPerfect convergence (768/768) without disturbing PalRle bookkeeping correctness on paper.  But the failure predates my fixes, so this rules in/out only the recent edits, not the earlier reliable-tile-emitter wiring.

## Diagnostic data captured during the hunt

### `decode_error_thin_uncached` server logs (after my CAPTURE_FPS_DRM_DIRECT=2 revert)

```
HELLO=2, test-hook=1, force_rebundle=0, client_decode_error=0, ack_batch_received=395
cumulative emit ... emitted_solid=65376 emitted_palrle=692 emitted_cdf53=0 emitted_h264=105
   fec_parity_emitted=6605 rto_fired=225150 rto_max_retransmits_reached=49418
   emitter_ack_hits=6566 emitter_ack_misses=17038 retransmit_attempts_total=225150
```

- `test-hook=1` confirms `fire_session_reset` ran with `preserve_delivered=true`.
- `client_decode_error=0` despite `emitted_palrle=692` means **either** the server isn't actually emitting THIN after the preserve, **or** the client isn't erroring on it.  Since the client clears its palette shadow on session reset (`renderer.onSessionReset` zeros the 16 KB atlas + `paletteShadow.clear()`), any THIN PalRle would trigger ERR_THIN_UNCACHED_PALETTE → `decode_error_batcher.report` → feedback stream.  Something interrupts that chain.
- `rto_max_retransmits_reached=49418` is high.  ACK throughput is restored vs. pre-fix but a lot of entries still age out without ACK — symptom of the JS main thread being saturated, but at the rates this test runs (5 s + 6 s segments), this shouldn't matter for correctness.

### `palette_eviction_chromium` client recorded tiles (one run)

```
all_codecs: {2: 102, 3: 2045}            # 102 PalRle, 2045 Solid
all_recorded: 2147
region_codec_hist (tiles 3,3 / 3,4 / 4,3 / 4,4 over 8 s):
  (3,3):codec=2: 12   (3,3):codec=3: 1
  (3,4):codec=2: 12   (3,4):codec=3: 1
  (4,3):codec=2: 12   (4,3):codec=3: 1
  (4,4):codec=2: 12   (4,4):codec=3: 1
region_tile_total: 52
```

- Each of the four region tiles received 12 PalRle + 1 Solid datagrams during the 8 s test.  Delivery at the wire layer is healthy.
- Samples taken at 4 different (x, y) coords inside the region all read `[0, 0, 0]` — not the wipe palette colour, just *zero*.

### One detail that hasn't been chased

A second run of the same test showed `region_codec_hist` with **14 PalRle and 0 Solid** for each region tile (no wipe captured by the server's coarse 2-fps sampling).  Even with no Solid wipe in the histogram, pixels still read black.  That rules out "server's most recent tile for the region happened to be a black Solid wipe" as the explanation.

## Working hypotheses

In rough order of likelihood:

### H1 — Server emits BUNDLED but with a zero palette buffer

Some transport-layer change is overwriting `palette_table.entries[id]` between the moment `acquire_or_allocate` returns an id and the moment the wire payload is built, *or* `encode_pal_rle_payload` is reading the wrong slot.

Why plausible: under churn, many slots get rewritten per second.  Any race between `write_bytes(id)` and the per-frame emission loop could ship the WRONG slot's bytes (or a slot that was just zeroed).

Disproof / test: add a `tracing::warn!` inside `encode_pal_rle_payload` if any of the `count` bundled colours is `[0, 0, 0, 0]`, then run the test.

### H2 — Server emits THIN but a stale `delivered` flag points the client at a slot that was just rewritten on the client side

Subtle interaction between server-side LRU eviction and client-side `upsertPalette` writes: if two consecutive bundled palettes use the same slot id back-to-back, the client's later `upsertPalette` clobbers the earlier palette bytes, but the server still thinks slot `id` has the EARLIER palette.

Disproof / test: client-side, log every `upsertPalette(id, bgra)` call.  Server-side, log every `write_bytes(id, palette)` call.  Run both tests and compare the (id, palette-fingerprint) sequences.

### H3 — The reliable-tile-emitter's retransmit cache replays a STALE PalRle bundle

Commit `5b894c2` ("io_bridge: route tile emission through ReliableTileEmitter") starts caching every tile-pass bundle for RTO retry.  Under churn the SAME slot id gets reassigned to many different palettes; a retransmit of an OLD cache entry would push OLD palette bytes to the client, but the client would dutifully `upsertPalette` and overwrite its slot with the OLD bytes.  Next frame's THIN PalRle would index into the wrong palette.

Why interesting: this exactly matches the all-black pattern *if* the OLD palette was the iteration-0 palette `[(0,0,0), (0,0,0), (0,0,0), (255,255,255)]` (3 of 4 colours are zero, only colour index 3 is white).  If indices in the live tile reference colour 0 / 1 / 2, the rendered pixel is `[0,0,0]`.

Disproof / test: add `tracing::debug!` at every `reliable_emitter.tick()` retransmit hit, logging the (frame_seq, tile_x, tile_y, pass_idx, bytes-len) so we can correlate against the original emission's payload.  Cross-reference with whether the client's `upsertPalette` is being called with stale bytes.

### H4 — `browser.new_page` doesn't actually drop session 1 cleanly

Affects only `palrle_session_reset` (commit `aed1e71` replaced `setup.page().reload()` with `browser.new_page(page_url)`).  In chromiumoxide, `new_page` opens a new tab without closing the previous one's WebTransport session.  Server keeps shipping to the OLD tab while the test reads from the NEW tab.  The new tab's palette atlas is freshly zero; the old tab's atlas is fine but not visible to the test.

Disproof / test: switch `palrle_session_reset` back to `setup.page().reload()`.  If it passes, this hypothesis holds and the test fixture needs to be reworked rather than the codec path.

## Concrete next steps

1. **Add the diagnostics from H1, H2, H3 above** to the lib + web-client side, rebuild the container, run the three failing tests in turn.  Total wall time: ~15 min of edit + ~10 min of container rebuild + ~5 min of test runs.

2. **If H1 fires** (zero bundled palette colours seen): trace upward through `acquire_or_allocate` and the per-frame `dispatch_dirty_tiles_via_scheduler` loop to find the source of the zero.

3. **If H2 fires** ((id, palette-fingerprint) drift between server and client): the leak is in the SHADOW-vs-ATLAS sync semantics.  Likely root cause: `upsertPalette` happens at decode time on a per-tile bundled, but the SLOT-LIFECYCLE is per-frame; an out-of-order bundle for slot `id` can shift the atlas to a palette that no longer matches what the indices were encoded against.

4. **If H3 fires** (retransmit of a stale bundle): add a per-cache-entry "supersede on bump" hook so retransmits stop for tiles whose generation has advanced.  Pair with the existing `cancel_for_tile` call so a cache entry's bytes never outlive their generation.

5. **If H4 fires** (`new_page` leaks the old session): fix the test fixture or document why `new_page` isn't equivalent to `reload`.

## Files I left in a clean state

```
ghostframe-e2e/tests/e2e.rs        # diagnostics reverted to a clean shape
ghostframe-lib/src/transport/*.rs  # last commit is mine; lib builds clean
```

All my fixes for the *other* eight failing tests are committed and don't depend on resolving this regression.  The cdf53 work — the reliable-tile-emitter's actual purpose — is end-to-end correct:

- `e2e_lossless_golden_png`: whole-frame strict pixel-perfect compare converges in 2 polls (~500 ms)
- `e2e_cdf53_lossless_buildup_chromium`: 768 PixelPerfect transitions (one per gradient tile)

## Commits worth re-reading

| sha | one-liner |
|---|---|
| `5b894c2` | io_bridge: route tile emission through ReliableTileEmitter |
| `2b66a7b` | io_bridge: ACK dispatch routes through emitter.on_ack |
| `1b7dfbf` | io_bridge: retire fragment_coverage drop_cdf53_for_tile + snapshot redundant sites |
| `92e5121` | scheduler: add cancel_callback fired from bump_generation* |
| `1f9990b` | io_bridge: replace dangling-pointer cancel callback with direct call |
| `5fd2b1c` | io_bridge: stamp TILE_DATAGRAM_FLAG into the emitter cache key |
| `ba05357` | transport/ack: accept the overlap entries the client already sends |
| `aed1e71` | test(e2e): convert e2e_palrle_session_reset into _chromium + _firefox variants |

The earliest candidate for the regression is `5b894c2` — that's where the reliable-tile-emitter starts intercepting every tile fragment.  If H1/H2/H3 don't fire under instrumentation, bisecting from `5b894c2` forward (about 8 commits to test) is the next step.
