# Design note: damage-driven encoding

**Status:** design, not implemented. **Date:** 2026-10-29 · **Base:** master at 173ba45f
**Context:** [ghostframe-server X11 spec](../superpowers/specs/2026-10-29-ghostframe-server-x11-design.md) §2 (non-goal: "`FrameSubmission::damage_tiles` is ignored by the library today"), [remediation scoreboard](../reviews/remediation-scoreboard.md) (C2 measurement, B2 note).

**Scope caveat.** CPU CDF 5/3 pass encode dominates a busy frame (63 ms at 4K/5 % dirty, measured). This design removes the screen-proportional work and makes idle nearly free; on its own it does **not** reach "well under 15 ms" for a full-screen tile frame. That target needs the pass encode moved or sped up, which is outside this note. Expect the win on idle, typing and mixed sessions, and as throughput headroom elsewhere.

Citations are `path:line` on master. Numbers marked **(measured)** come from the scoreboard; numbers marked **(estimate)** are arithmetic or recollection and must be re-measured by PR 0 before anyone relies on them.

## Goal

Better latency and throughput most of the time, for high-quality interactive sessions. Standing target: a full-screen tile frame well under 15 ms, holding at 4K and dual-screen; per-tile and per-ACK work O(1).

The idea: the X server already knows which pixels changed. Today ghostframe rediscovers that by grabbing, uploading and comparing every pixel of every tick. The design makes *work scale with changed pixels*, while keeping the pixel comparison as the authority so that a wrong damage report can cost time but never correctness.

## 1. Today's per-tick path

Default `CAPTURE_FPS` is 30 (`ghostframe-server/src/config.rs:32`). Each tick of the capture loop (`ghostframe-server/src/commands/capture.rs`):

| # | Stage | Where | Scales with | Cost |
|---|---|---|---|---|
| 1 | `drain_damage`: poll XDamage queue, `damage_subtract`, hash-insert every tile of every rect | `capture/x11/damage.rs:130-165`, `rect_to_tiles` :263 | events x tiles per rect | small; O(rects x tiles) hash inserts. A full-screen rect at 4K is 8160 inserts (estimate) |
| 2 | `poll_cursor`: `XFixesGetCursorImage` round trip, every tick | `capture/x11/frames.rs:400`, called from `capture/x11/mod.rs:138` | constant | one sync round trip (estimate 50-100 us) |
| 3 | `grab`: `ShmGetImage` of the whole root, then `seg.bytes()[..want].to_vec()` | `capture/x11/frames.rs:346-372` | **screen** | X server copy plus a second full copy and allocation: 8.3 MB at 1080p, 33 MB at 4K per tick (estimate: ~1-3 ms and ~4-10 ms of memcpy) |
| 4 | `blit_cursor` into the frame | `frames.rs:427` (`blit_cursor` is defined at :552) | cursor size | negligible |
| 5 | `drain_damage` result written to `frame.damage_tiles`, frame sent over an mpsc channel | `commands/capture.rs:246-251, 280`; `ghostframe-lib/src/server.rs:203` | constant | the field then dies (below) |
| 6 | `process_frame` -> `begin_gpu_frame` -> worker `Analyse`; the loop thread also runs `collect_escalation_candidates` (O(tiles)) | `ghostframe-lib/src/transport/io_bridge/frame.rs:265, 764, 1312` | tiles | small (estimate: tens of us at 4K) |
| 7 | Upload: `import_pixels` makes a **new** HOST_VISIBLE linear image, maps it, copies every row, makes a view | `capture/gpu_pipeline/frame.rs:303-349, 1249-1340` | **screen** | third full-frame copy (estimate: same order as stage 3) |
| 8 | GPU, one command buffer: `tile_sad` (all tiles), NV12 conversion (all pixels), `tile_analysis`, PalRLE compact/fold/index, CDF 5/3 compact + wavelet L1-L3 for dirty tiles, snapshot copy of the **whole frame** into `prev_image` every tick (`record_snapshot_copy`, `gpu_pipeline/frame.rs:606`) | `gpu_pipeline/frame.rs:463-700`, `record_snapshot_copy` :2248, shaders `tile_sad.comp` etc. | SAD, NV12, snapshot: **screen**; compact and wavelet: **dirty tiles** | **(measured)** GPU total 7 ms at 1080p/5 % dirty, 18.5 ms at 1080p full, 27 ms at 4K/5 %, 71.5 ms at 4K full (`docs/reviews/remediation-scoreboard.md:226-233`). The 5 % vs full gap shows the dirty-proportional part; the 5 % figure is the floor the screen-proportional part leaves. No idle-screen number exists |
| 9 | Classifier and frame-mode decision (`classify_dirty_tiles`), `pixelperfect_sweep` every frame over all tiles, scheduler dispatch | `frame.rs:1363, 1546`; `tile/classifier.rs` | dirty tiles; the sweep is O(tiles) | small |
| 10 | CDF 5/3 pass encode on the CPU | `encode_passes_sparse` | **dirty tiles** (~134 us/tile, **(measured)**) | 16 ms at 1080p/5 %; 273 ms at 1080p full; 1.15 s at 4K full. Dominant stage, but already proportional to damage |
| 11 | H.264 branch: full-frame encode from the NV12 buffer; skipped only when no tile is dirty | `frame_backend.rs:331`, `frame.rs:189` (`h264_finish_action`) | screen when it runs | not measured (VA-API is off for agents) |
| 12 | Emission through the scheduler | `io_bridge/emit.rs` | dirty tiles plus refinement queue | small |

### Where `damage_tiles` is dropped

`damage_tiles` is **not** dropped in the server or the channel; it is dropped by the library. Its only reader is `frame.rs:427`, inside `process_frame_cpu_at`, which is `#[cfg(test)]` (attribute at `frame.rs:343`, fn at :344); its caller `process_frame` (`frame.rs:300-318`) marks that arm "unreachable in production". The GPU path moves the whole `FrameSubmission` into `ToWorker::Analyse` (`frame.rs:794-815`, `ToWorker::Analyse` at :806) and the worker never looks at it. `X11Backend::drain_damage` says so itself (`capture/x11/mod.rs:130-131`). The C ABI constructs it as `None` (`ghostframe-lib/src/ffi.rs:140`); the Rust server path is the only producer.

A second defect in the producer: `drain_damage` returns `None` for three different things: "no damage, nothing changed", "unknown, compare everything", and "damage was never created" (the `?` at `capture/x11/mod.rs:143` returns early when `self.damage` is `None`). `merge_cursor_damage` is at :151. A damage-driven consumer needs a tri-state.

### What scales with what

- **Screen size, every tick, even when idle:** stages 3, 7, and the SAD, NV12 and snapshot parts of 8. Note `prev_image` is the *previous frame*, not a last-committed reference, and `SAD_THRESHOLD` is 64 (`gpu_pipeline/mod.rs:50`): a change that stays under 64 per tick is never committed, however far it drifts over many ticks. Estimate for a 4K idle tick: ~3 CPU copies of 33 MB plus a full-frame GPU compare and convert, 30 times a second. The scoreboard's "5 % dirty" rows are therefore not near the idle cost; an idle measurement is PR 0.
- **Changed pixels:** the compact, wavelet, PalRLE and CDF pass encode (stages 8 partial, 10, 12).
- **Tile count, every tick (O(tiles), not O(dirty)):** `pixelperfect_sweep`, `collect_escalation_candidates`, and the SAD dispatch (one workgroup per tile). Cheap at 8160 tiles but contradicts "O(1)" in spirit; see PR 6.

## 2. Where damage comes from

### XDamage

Spec: <https://xorg.freedesktop.org/archive/current/doc/damageproto/damageproto.txt> (read 2026-10-29).

| Report level | Event sent when | Fit |
|---|---|---|
| `RawRectangles` | every screen modification, one bounding rect each, overlaps not merged | what ghostframe uses (`damage.rs:101`). Event flood under scroll or video; the doc comment at `damage.rs:15-26` already notes the cost |
| `DeltaRectangles` | only damage outside the existing region, reporting just the new part | fewer events; still per-rect |
| `BoundingBox` | only when the bounding box of accumulated damage grows | one big box; useless for scattered updates |
| `NonEmpty` | region goes empty to non-empty, and again if a subtract leaves a non-empty region | one event per tick when active, zero when idle |

`DamageSubtract(damage, repair, parts)`: with `repair = None`, `parts` receives the whole damage region and the damage is cleared. `parts` is an XFixes region, readable with `XFixesFetchRegion`. The spec guarantees rectangles cover modified pixels and may include extra area; it says **nothing** about when pixels are readable relative to the event, so the safe ordering is ours to define.

**Recommended consumption: `NonEmpty` + `DamageSubtract(parts)` + `FetchRegion`.** Per tick, if a notify arrived (or always, if we want to avoid state): subtract into a region, fetch the region's rectangles (server-side banded and coalesced). Two small round trips only when something changed; zero events when idle; the rectangle list is bounded by the server's region coalescing instead of by the app's draw-call count. `RawRectangles` remains the fallback (PR 1 measures both). The monitor then builds a tile bitmap (`Vec<u64>`, one bit per tile) instead of a `HashSet` of tuples.

**Ordering with `ShmGetImage` (already correct today, must stay):** subtract first, then grab. Any pixel drawn after the subtract produces a fresh damage event and is re-reported next tick, so a drawing racing the grab is at worst grabbed early and repeated, never lost. `drain_damage` runs before `capture` in the loop (`commands/capture.rs:246-249`) and `damage.rs:136-159` drains before subtracting; both are load-bearing and each needs a test that fails if reordered (section 5).

### How other tools do it

- **x11vnc** (<https://github.com/LibVNC/x11vnc/blob/master/src/xdamage.c>): uses XDamage only as a *hint*. It accumulates rectangles into a ring of regions, ignores rectangles above `xdamage_max_area` (default 20000 px) and tall ones, says in comments that damage cannot be trusted (e.g. a focus change reports a whole toplevel), and keeps scanline polling as the backstop. If the event rate is absurd it marks the whole screen, and after repeated floods disables XDamage for 300 s (`xdamage_crazy_delay`). Direct framebuffer copy from damage is disabled by default and commented "not working yet".
- **TigerVNC x0vncserver** (<https://github.com/TigerVNC/tigervnc/blob/master/unix/x0vncserver/XDesktop.cxx>): `XDamageCreate(root, XDamageReportRawRectangles)`, forwards each notify rect to `add_changed`; no `XDamageSubtract` in this file; full-screen `add_changed` on resize; polls the screen only if the extension is absent.
- **Sunshine X11 grab** (<https://github.com/LizardByte/Sunshine/blob/master/src/platform/linux/x11grab.cpp>): no XDamage at all. Every frame is a full-monitor `xcb_shm_get_image` (or `XGetImage` fallback) plus `XFixesGetCursorImage` blend. That is the same full-grab cost model as ghostframe today, which is the baseline this design improves on.
- **xrdp / xorgxrdp** (<https://github.com/neutrinolabs/xorgxrdp>): the capture lives inside an Xorg driver module and collects damage in-server rather than over the X protocol. Not read in detail for this note; it shows the in-server variant, which we cannot use for attach mode (we capture the operator's own session as an ordinary X client).

The lesson from the two VNC servers: damage is trusted enough to narrow the work but never as the sole truth; every mature implementation has a size cap, a flood escape and a polling backstop. We take the same posture, with a cheaper backstop because our comparison is on the GPU.

### The cursor

The cursor is a separate source: X reports no damage for it and ghostframe composites it into the pixels (`frames.rs:413-431`). `cursor_dirty_tiles` (`frames.rs:193`) already yields the old and new cursor tiles, merged in `mod.rs:132-145`. Two issues for a damage-driven design:

1. The cursor poll is a synchronous round trip every tick. At idle it becomes the dominant cost. Replace by `XFixesSelectCursorInput` (cursor-image change events) plus `QueryPointer` for position; still one round trip per idle tick, but no image transfer. Tracked as PR 8; measure first, it may not matter.
2. The cursor must stay consistent with a **persistent** frame buffer. `blit_cursor` mutates the buffer in place, so the rows under the *old* cursor rectangle must always be re-grabbed from X to restore clean pixels. That is exactly what including the old tiles in the hint does; it becomes a tested invariant (property in section 5), not an accident.

### RandR and resize

Geometry is re-read once per tick (`frames.rs:316-336`, `refresh_geometry`). Any change must produce `Unknown` damage for that tick, reallocate the persistent buffers, and invalidate the GPU baseline (the library already handles a dimension change: `prev_image` is dropped and the first-frame branch marks everything dirty, `gpu_pipeline/frame.rs:382`). The Damage object stays valid on the root across a mode change, but region contents at the moment of change are meaningless; treat the next two ticks after any geometry change as `Unknown`. RandR events (`capture/x11/randr_events.rs`) are the trigger already used elsewhere.

### Coverage gaps

XDamage on the root reports drawing that reaches the root's pixels. It can miss:

- **Unredirected fullscreen applications** (compositor unredirect, DRI3/Present page flips): pixels reach the scanout without passing through the server's drawing path, so no DamageNotify. This is the main hole and is also the x11vnc "cannot trust" case. **It may be worse than a missed report:** a page-flipped window may not be visible to `GetImage`/`ShmGetImage` on the root at all, in which case the existing full grab is already stale for it and no sweep can repair it. Unverified; section 6 lists it for an owner-present run, and this note does not promise that the sweep fixes it.
- Some GL clients under non-compositing setups on drivers that flip.
- Damage lost when the event queue is flooded and we disable the monitor (our own escape hatch, like x11vnc's).
- **Saturation:** compositors that repaint the whole screen for every change report damage on (nearly) every tile, every tick. Then damage buys nothing and the design must cost nothing extra. PR 0 measures on the owner's actual compositor, not only on the Xorg sink.
- Over-reporting is the opposite gap: harmless for correctness (SAD says nothing changed) but wasteful.

Fallbacks, in layers (details in section 3.4):

1. **SAD stays authoritative over every tile whose pixels reach the GPU.** Damage chooses which rows are grabbed and uploaded; every tile in an uploaded band is a SAD candidate (rule H1, section 3.2). A false positive costs a grab and a compare.
2. **Rolling verification sweep**, specified in ticks (section 3.4).
3. **Trust state machine** driven by confirmed corrections (section 3.4).
4. **Flood escape**: more than E raw events per tick, or a damaged area above a size cap (x11vnc style), becomes `Unknown` for that tick: one full grab beats thousands of small ones.

What is guaranteed: a missed report leaves a tile **stale for at most the sweep period N** (plus the pipeline depth), not forever. Correctness of everything the GPU is shown is preserved; completeness of what the server shows the GPU is bounded, not absolute.

## 3. What to skip when damage is known

### 3.1 Buffers: ownership, age, and the grab set

**Ownership (B2).** `FrameSubmission.pixels` is an owned `Vec<u8>` (`ghostframe-lib/src/server.rs:24`), sent on a channel of depth 2 (`server.rs:103`) with one frame in flight in the bridge (`frame.rs:765`), and there is no path back to the producer. A persistent capture buffer would have to be copied into a fresh `Vec` on each submit, which defeats the point. So `FrameSubmission` changes:

```rust
pub struct FrameSubmission {
    pub pixels: FrameBuf,        // pooled; slot returns to the producer on Drop
    pub content: Content,        // Pixels | Unchanged
    pub damage: Damage,          // Unknown | None | Tiles { hint, verify }
    /* geometry, timestamp, capture_done_ns as before */
}
```

`FrameBuf` is a handle into a fixed pool owned by `GhostframeServer`; dropping it (wherever the frame ends: worker, dropped frame, error) returns the slot to a free list the capture loop waits on. Pool size is the pipeline depth: 2 queued + 1 in flight + 1 being grabbed = **4 slots**. If the free list is empty the capture loop waits (backpressure, replacing `submit_frame` blocking on a full channel). The C ABI function `gf_server_submit_frame` keeps its signature: it copies the caller's pointer into a non-pooled `FrameBuf` and sends `Damage::Unknown` (`ffi.rs:140`). A `FrameBuf` type does not need to appear in the header; if a future C entry point exposes damage, regenerate `ghostframe-lib/include/ghostframe.h` and let `just ci-local` diff it.

**Complete-buffer invariant (M2).** Every `Content::Pixels` submission carries a complete, current buffer for the whole root, with `damage` only saying what changed. Consequence: if the library drops or fails a frame (`ensure_baseline_invalidated`, `frame.rs:838`) it needs nothing from the server; it treats the next submission as `Unknown` and does a full upload from the complete buffer.

**Buffer age (B3).** With a ring of slots, the slot picked for tick *t* last held tick *t-k*. Grabbing only tick *t*'s damage into it would leave rows that changed in the intervening ticks showing old content, and content would flicker back. The rule is the one EGL calls buffer age: the **grab set for a slot is the union of the damage bitmaps (and verify slices, and cursor old and new tiles) of every tick since that slot was last made current**. The server keeps the last K damage masks, K = pool size, and ORs them per slot. The cursor's old position is part of every tick's damage, so the union restores every position the cursor has been blitted at since the slot was last written. A slot with unknown age (first use, resize, `Unknown` tick, trust demotion) gets a full grab. Cost: the grab area for a slot is the union over up to 4 ticks, a bounded multiple of per-tick damage.

Which rows are *uploaded* is a different, smaller set (section 3.2): this tick's `hint` plus `verify` tiles. The extra union rows are fresh in the slot but already uploaded on the ticks that reported them.

**Grab primitive.** `ShmGetImage` writes a rectangle *packed* at `shmoffset`, not at its position in a larger image, so per-rectangle grabs cannot land in place. **Row bands** do: a full-width band `y0..y1` is contiguous at offset `y0 * stride`. The grab set becomes bands (merge bands closer than a few tile rows), all requested back to back and flushed once. If the banded area exceeds a fraction F of the screen, one full grab instead (F is set from PR 4's measurement, not guessed). Bands waste width (a 30x16 px glyph grabs a 1920-wide, 32-row band: 245 KB instead of 2 KB), still ~35x less than a 1080p frame (estimate).

**Tearing (M1).** One `GetImage` is atomic against other clients. Separate band requests are not: the server may process another client's drawing between two bands, so a single tick can show a frame that never existed. This is repaired by the damage itself: anything drawn after `DamageSubtract` is re-reported next tick and re-grabbed. Pipelining the requests keeps the window small. `XGrabServer` is rejected (it stalls the whole session). A full grab is atomic, which is a further reason for the F fallback.

### 3.2 Library side: one snapshot policy, and the SAD authority rule

**Today's snapshot policy** (section 1): `prev_image` is the previous frame, copied in full every tick; dirty means `SAD > 64` against the previous frame. **Proposed single policy for both paths:** `prev_image` becomes a **per-tile last-committed reference**: a tile's reference is updated only when the tile is a SAD candidate this tick and is committed dirty. Non-candidates and tiles under threshold keep their reference.

Behaviour change, stated plainly: sub-threshold drift becomes detectable. Today a slow fade whose per-tick change stays under 64 is never committed, however far it drifts; with a last-committed reference the accumulated difference crosses the threshold and the tile is sent. This is an improvement in fidelity (and a small extra emission for slowly changing content), and it is what makes a partial snapshot copy well defined. It lands as its own PR (PR 3) with its own gate, applied to the full-compare path first, so the oracle in PR 6 compares like with like (H3).

**SAD authority rule (H1).** Skipping tiles must not lose SAD's authority over what reaches the encoder. The rule: **every tile whose pixels reach the GPU this tick is a SAD candidate.** The upload copies a band; every tile in the band is a candidate (not only the hinted tiles inside it). Escalation and refinement (stage 4d in `gpu_pipeline/frame.rs`, `collect_escalation_candidates` at `frame.rs:1312`) read the current uploaded image; because anything that differs between it and the reference is a candidate, they never encode new content under an old generation. The only way new content reaches an encoder without SAD seeing it is if the server never uploaded it, i.e. a missed report, which leaves the tile stale in the upload image too and is bounded by N (section 3.4).

Library changes, in order of payoff (all behind the PR 6 gating and the default-off knob below):

1. **Persistent upload image** instead of `import_pixels` allocating a new one every tick (`gpu_pipeline/frame.rs:1249`): allocated once per resolution; copy only the upload bands; full copy for `Unknown`.
2. **Hinted `tile_sad`**: a candidate mask is pushed; non-candidate workgroups exit at once (or an indirect dispatch over the candidate list).
3. **NV12 persistent and partial**: `ensure_nv12_buffer` is already persistent per resolution (`gpu_pipeline/frame.rs:154`); converting only candidate 2x2 blocks keeps it exact if *every* candidate is converted whatever the mode. This preserves the TileCodec-to-H.264 switch (section 3.5).
4. **Partial reference update** as defined above, via `vkCmdCopyImage` regions for committed tiles.
5. **`Content::Unchanged`**, section 3.3.

The `no_commit` cushion and the first-frame branch (`frame.rs:382-460`) stay `Unknown`-only paths.

### 3.3 The progressive-refinement / PixelPerfect state machine, and the `Unchanged` tick

Refinement, PixelPerfect transitions and idle escalation are driven by the frame tick, not by damage: `pixelperfect_sweep` runs every frame before the dirty-tile early return (call at `frame.rs:1033`, body at :1546) so ACKs in static phases still drive transitions; idle escalation counts idle frames (K=512, idle > 30) and its forward dispatch reads pixels from the current image (`collect_escalation_candidates`, `frame.rs:1312`); the scheduler drains un-sent refinement passes when `dirty_xy` is empty. An idle screen therefore cannot stop ticking the library.

`Content::Unchanged` means "no new pixels, run the state machine". The worker skips upload, SAD, NV12 and analysis, and the escalation forward dispatch reads the **persistent upload image** (H2). It must not read `prev_image`: under the last-committed policy `prev_image` is a reference that can differ (sub-threshold) from what the screen last showed, and idle escalation must refine what the screen shows. The server sends `Unchanged` only when: damage is `None`, the cursor tile set is empty, no sweep slice is due this tick (section 3.4), the baseline is committed (no `no_commit` cushion), and geometry is unchanged. Otherwise it sends `Pixels`.

Idle ticks stay at the capture rate (proposed default); converting the idle-escalation counters from frames to time, so idle ticks can drop to a few Hz, is optional and separate (PR 8). Property kept and tested: an undamaged tile reaches PixelPerfect and idle escalation at the same tick in damage mode and in full-compare mode.

### 3.4 Safety net: sweep, confirmation, trust, feedback

**Sweep cadence, in ticks (H4).** The screen is divided into S slices of rows. One slice is added to the grab set, the upload bands and the SAD candidate mask (as `verify` tiles) every k ticks. Sweep period = S * k ticks. Proposed default **k = 5, S = 12: N = 60 ticks = 2 s at 30 fps**; each slice is 1/12 of the rows (180 rows at 4K, ~2.8 MB; estimate). A "slice due" tick is a `Pixels` tick, so with `Unchanged` an idle session alternates 1 pixels tick and 4 unchanged ticks, 6 small grabs per second. The slice also drives the grab, not only the mask: a slice that is in the mask but was not grabbed would compare stale pixels with stale pixels.

**False corrections (B4).** A draw landing between `DamageSubtract` and the grab is in the grabbed pixels but is only reported next tick. A slice covering that tile sees SAD-dirty with no hint, which would look like a miss. Rule: a SAD-dirty tile with no `hint` bit this tick is only a *suspect*. It is a confirmed **correction** only if the tile is also absent from the damage masks of the next tick(s) (the server keeps the last K masks and checks by tick number, so a delayed feedback is still judged correctly). Negative test, required: steady typing with the sweep on yields **zero** confirmed corrections.

**Feedback path (B5).** SAD runs in the library; the trust state and the grab live in the server; `submit_frame` is one-way. Add a library-to-server feedback: after each analysed frame the worker publishes `FrameFeedback { tick, suspects: TileMask, dropped: bool }` through a `watch` or small bounded channel on `GhostframeServer` (latest value wins plus a per-tick history of the last K). The capture loop reads it at the top of each tick. The trust state machine lives in the server, where it already decides the grab set.

**Trust state machine.** `Trusted` -> `Distrusted` on a confirmed correction: damage hints are ignored, every tick is a full grab and full compare (today's behaviour) for T seconds (proposed default **T = 10 s**, global, not per region), then back to `Trusted` after a clean period. Corrections are counted and logged rate-limited, in the `cumulative emit` style, so the gap is observable. `Distrusted` is also the state whenever `damage_armed()` is false.

**Shadow mode.** Before any consumption, the safety net runs in shadow: the server computes the hint and sweeps but still grabs the whole screen; the library compares SAD-dirty tiles against `hint | verify` and reports would-have-been misses. This measures the real miss rate on the owner's compositor before trust is ever granted (PR 4).

### 3.5 H.264-mode regions

`FrameMode::H264` encodes the whole frame from the NV12 buffer (`frame_backend.rs:331-376`); it already skips when no tile is dirty (`h264_finish_action`, `frame.rs:189`). With damage:

- Idle H.264 sessions already skip the encode. The win is upstream: no grab, upload or NV12 conversion of the unchanged screen.
- Partial NV12 update (3.2 item 3) keeps the encoder input exact.
- Not in scope: encoding only damaged macroblocks. Proposed default: leave H.264 alone; revisit after the measurements.
- `collect_escalation_candidates` skips escalation under H.264 (`frame.rs:1312-1330`). The full-screen video case is the damage-saturated case: damage is ~100 % every tick, the banded area exceeds F, and one full grab runs, as today. The design must not be slower than today there (gate in section 5).

### 3.6 The "dirty rows" readback from the scoreboard

The B2 note (`remediation-scoreboard.md:66`) says dirty-rows readback is API only: `readback_dmabuf_rows` (`capture/dmabuf.rs:410`) exists and is tested but nothing calls it. It is a dma-buf mmap -> host readback for the DRM capture path, which master no longer uses (the X11 spec keeps dma-buf ingest unused). It does not help the X11 path directly, but its contract (rows must cover all changes, `invalidate()` on dropped frames) is the same contract as 3.1 and its test `dirty_rows_copy_exactly_the_listed_rows` (`dmabuf.rs:865`) is a template. Leave it alone; reuse the contract wording. A future Wayland/PipeWire backend with dma-buf damage rects would plug in here.

## 4. Correctness risks

| Risk | Consequence | Mitigation |
|---|---|---|
| Missed damage (unredirected GL, driver flip, lost events) | stale tile, not forever | sweep (3.4): staleness <= N plus pipeline depth; trust demotion after a confirmed correction. A page-flipped window invisible to root `GetImage` is not fixable here (section 2) |
| Trusting damage before the net exists | persistent slot stale, SAD compares stale to stale | order of PRs: net and shadow mode first, consumption behind a default-off knob (section 5) |
| Drain/subtract/grab reordering | lost updates | keep subtract-before-grab; a test that fails on reorder |
| Ring slot age ignored | content flickers back | union-of-damage rule per slot (3.1); property test against a fresh full grab |
| Draw between subtract and grab | false correction, constant demotion | next-tick confirmation (3.4); negative typing test |
| Persistent buffer + in-place cursor blit | cursor trail or ghost | old and new cursor tiles in every tick's damage, hence in every slot's union |
| Band-grab tearing | one inconsistent frame | repaired by re-reported damage (3.1) |
| `None` conflated with `Unknown` and "never armed" | first frame to a new client not sent in full | tri-state `Damage`; `Unknown` forced on new session, `no_commit` cushion, geometry change, session reset, capture error, `Distrusted`, unarmed monitor |
| Dropped frame in the library | snapshot/upload image out of sync | complete-buffer invariant: the next submission is uploaded in full (M2) |
| Skipped tiles bypass SAD, new content under an old generation | mixed PixelPerfect tile | SAD authority rule: every uploaded tile is a candidate (3.2) |
| Damage bitmap sized for old geometry after resize | OOB or wrong tiles | bitmap carries `(cols, rows)`; mismatch is `Unknown` |
| Multi-monitor root | tile grid is on the *root* (3840x1080 for dual 1080p); gaps black | root-relative tiles already; test on a two-output Xorg with a non-aligned offset |
| Roots wider than 8192 px | `pixelperfect_sweep` casts tile coordinates to `u8` (`frame.rs:1553`, `idx as u32 % cols) as u8`) and wraps | pre-existing, independent of damage; noted so a dual-4K root is not mistaken for a damage bug |
| Damage rect negative origin or off-screen | wrong tile range | `rect_to_tiles` clamps and over-approximates by design (`damage.rs:256-262`) |
| Event flood | O(events) CPU in the X client queue | `NonEmpty` + parts, or flood escape |
| Compositor repaints everything | damage saturated, no gain | measured in PR 0 on the owner's compositor; falls back to full grab above F |

## 5. Plan: small PRs in order, each safe on its own

**Ordering decision (B1).** Two options: (a) land the sweep together with the first consumer of damage; (b) land the net first, with a default-off knob guarding consumption. Chosen: **(b)**, because (a) makes one PR carry the producer, the buffers, the library change and the net, and a review cannot isolate a missed-damage bug in it. Instead: the net and a *shadow mode* come first (PR 4), the consumers (PR 5-6) land behind `damage_mode = off | shadow | on`, default `off`, and a final tiny PR flips the default once the owner has seen the shadow miss-rate and the fullscreen-flip run. Every merged PR changes behaviour only in ways its own gate proves, and none trusts damage by default.

Scenes: *idle* (static desktop), *typing* (a 30x16 px update at 10 Hz), *scrolling* (a 1920x900 region scrolling 32 px per tick), *video* (full screen, 30 Hz new content). Metrics per scene: server CPU per tick (`getrusage` over the run), `capture_us` and `submit_us` from the `[CAP] iter done` trace (`commands/capture.rs:319`), worker `gpu_us`, `process_frame` stage laps (`stage_trace_enabled`, `frame.rs:150`), bytes copied and allocations per tick. Resolutions: 1080p, 4K, dual-1080p root. Report 3+ runs.

**Gates are properties, not magic numbers:** idle cost independent of resolution except the O(tiles) terms; per-tick cost monotone and proportional in the dirty fraction {0, 1, 5, 25, 100 %}; no regression at saturation, compared in the same process with damage mode switched by config. Every gate comes with a mutation that must fail it; record the failing output in the PR's gates table.

| PR | Change | Measurable gate | Correctness gate (mutation-proven) |
|---|---|---|---|
| **0. Baseline harness** | Per-tick CPU, byte and allocation counters; scene driver for the four scenes on the Xorg `--x11-sink` pattern; baseline table for 1080p/4K/dual, on the Xorg sink **and on the owner's compositor** (to see damage saturation). No behaviour change | The measurement itself: prove non-zero (the idle scene shows a nonzero `capture_us`); records the missing idle cost | Mutation: stub a counter to 0; the harness rejects the run |
| **1. Honest damage producer** | Tri-state `Damage` in the `Backend` trait (`Unknown/None/Tiles`) including the never-armed case; tile bitmap replaces the `HashSet`; `NonEmpty` + `DamageSubtract(parts)` + `FetchRegion` behind a knob next to `RawRectangles`; flood escape; first-frame, resize and error force `Unknown`. The library still ignores it | `drain_damage` cost is O(1) in event count: with N synthetic rects per tick, the cost slope vs N is below `RawRectangles`' slope. Decides the report level (open decision 6) | X11 tests in the `x11_*` style (xvfb-run): real draws produce a superset of the touched tiles (property over random rects); reorder drain and subtract and the lost-update test fails |
| **2. Pooled frame buffers** | `FrameBuf` pool with return-on-drop in `FrameSubmission` (4 slots), the capture loop grabs into a slot and sends it; C ABI keeps its signature and sends `Unknown`; still a full grab, full compare | Steady-state allocations per tick drop to 0 and one full-frame copy per tick disappears (the `to_vec`): bytes copied falls by `W*H*4`; idle 1080p and 4K | Slot returned on every exit path: submit error, dropped frame, worker stop (a leak test fails the pool-exhaustion gate; mutation: skip the return in one path and the loop stalls, caught by a timeout test). Pixel output identical to before |
| **3. Last-committed reference** | `prev_image` becomes a per-tile last-committed reference, partial snapshot copy; applies to the full-compare path; documents the drift change | `record_snapshot_copy` cost becomes proportional to committed tiles (proportionality property); 5 % dirty GPU time falls | Drift test: a ramp under 64 per tick is sent once the accumulated change crosses the threshold (was never sent); mutation: restore whole-frame copy and the drift test fails. The existing e2e_cdf53_* and progressive-refinement scenes stay green |
| **4. Safety net in shadow mode** | `FrameFeedback` library-to-server path; sweep slices (k, S) added to the grab and the mask; suspect, confirm, correction counter; trust state machine; `damage_mode = off\|shadow\|on` (default `off`). `shadow`: hints computed, sweeps run, whole screen still grabbed, would-have-been misses counted | Sweep cost is O(1) per tick for a fixed resolution; with damage fully ignored, every tile is verified within S*k ticks (a property over ticks, not a time assertion) | **Negative:** steady typing with the sweep on gives zero confirmed corrections (B4). **Positive:** a test decorator inside `X11Backend` drops the damage for a rect *before `capture()` decides the grab set* (M4), the tile is repaired within the sweep, the correction counter is non-zero, and the state reaches `Distrusted`. Mutation: disable the confirmation and the typing test fails; disable the sweep and the injected miss persists |
| **5. Server consumes damage: band grab + age ring** | Behind `damage_mode = on` (default off): band grab with the union-of-damage buffer age rule, full grab above F. The library still receives complete buffers and still compares and uploads in full, so SAD stays authoritative over everything | Idle and typing: bytes copied per tick equal the band area (a property: the counter equals the union band area); `capture_us` for typing is resolution-independent | Property: for random damage sequences including cursor moves, each slot equals a fresh full grab after every tick (deterministic painter on the sink). Mutations: drop the age union, drop the old cursor tiles, skip a band, each fails the property. Injected miss: repaired by the sweep |
| **6. Library consumes damage** | `Damage::Tiles{hint,verify}` drives persistent upload image with band copy, hinted SAD with the authority rule (every uploaded tile is a candidate), partial NV12, partial reference. CPU test path ignores it | Typing and scrolling: upload time and `gpu_us` become proportional to the damaged fraction; the 7 ms floor at 1080p/5 % moves toward the 5 % part | **Oracle:** under any `Tiles(S)` with S a superset of the truly changed tiles, the emitted dirty list and the committed reference equal the `Unknown` run's (proptest over frame pairs, on the project's GPU runner), now well defined because both paths use PR 3's policy (H3). Mutation: omit a changed tile and assert the oracle fails. A band with a changed non-hinted tile inside it is still sent (authority rule); mutation: restrict candidates to the hinted tiles and that test fails. Browserless, 3+ seeds |
| **7. Content-free tick** | `Content::Unchanged`: the worker runs the state machine from the persistent upload image; the server sends it only under the rule in 3.3 | Idle: per-tick CPU and bytes copied fall to the O(tiles) sweep floor; typing latency (damage to first datagram) not worse | **Library-level oracle (M3):** the same scripted ACK/loss sequence is fed in damage mode and in full-compare mode on a scripted frame sequence, and the scheduler and `metrics_tracker` state are compared tick by tick (PixelPerfect, escalation, generations). Mutation: make `Unchanged` skip `pixelperfect_sweep` and the comparison fails. The e2e refinement scenes remain a smoke test, not the gate |
| **8. Cheap idle (optional, measure first)** | Cursor via `XFixesSelectCursorInput` + `QueryPointer`; event-driven refinement set instead of O(tiles) sweeps; idle tick rate decoupled with time-based escalation counters | Idle CPU independent of resolution | Escalation timing equivalence in time units; cursor ghost property from PR 5 |
| **9. Flip the default** | `damage_mode = on` by default, after the owner reviews shadow-mode miss rates and the fullscreen-flip run | none beyond the above | none new; one-line revert |

**Which scenes can measure it:**

- *Existing:* `ghostframe-e2e/tests/c2_loop_stall_measure.rs` builds frames with a `dirty_frac` (`:102-122`) and measures `process_frame` stalls; the model for library-side proportionality (PR 3, 6, 7). The browserless suite (`scene_tiles.rs`, `browserless_loop_stall.rs`) exercises the library without X, so it validates the library oracles but never the X11 grab. Browserless runs go under the shared bench lock, one scene binary at a time.
- *Needed, new:* an **Xorg damage scene** on the e2e container's `--x11-sink` (`DEVELOPERS.md:174`): the test pattern gains *idle, typing, scroll, video* modes with a deterministic seed, at 4K and with a two-output layout. Only this scene exercises XDamage, `ShmGetImage` bands, the age ring and the cursor together. A fullscreen-flip case is not reproducible on Xvfb/Xorg-dummy; it is the owner-present run.
- The e2e container needs `just containers-build` after any `ghostframe-lib` or `ghostframe-server` change; X11 unit tests need `xvfb-run`.

Wire format: unchanged. Four crates consume the wire format (`ghostframe-protocol`, `ghostframe-lib`, `ghostframe-client-core`, `ghostframe-client-wasm`); none is touched.

## 6. Open decisions for the owner

Each default is a **proposed default**; the owner confirms or overrides.

1. **Sweep period.** Proposed: N = 2 s, as k = 5 ticks per slice, S = 12 slices (30 fps). Cost vs worst-case stale-tile age.
2. **Distrust policy.** Proposed: global demotion for T = 10 s, after next-tick confirmation of a correction. Per-region is more complex and wins only in mixed sessions.
3. **Grab shape.** Proposed: row bands, every band tile a SAD candidate. Narrow rectangles with a scatter copy only if the typing gate asks.
4. **H.264 regions.** Proposed: leave the full-frame encoder alone.
5. **Idle tick rate.** Proposed: idle ticks at the capture rate until PR 8.
6. **Report level.** Proposed: `NonEmpty` + `FetchRegion`, with `RawRectangles` kept as a fallback setting, decided by PR 1's measurement on the owner's compositors.
7. **`FrameSubmission` shape.** Proposed: change `FrameSubmission` itself (tri-state damage, pooled/returned `FrameBuf`, `Content::Unchanged`); regenerate `ghostframe.h` if cbindgen sees a change; C callers send `Unknown`. Nothing is in production, so no compatibility shim.
8. **Fullscreen unredirected GL.** Proposed: accept self-demotion to full cost where the page-flipped window is visible to the grab; do not touch the user's compositor. If such a window is *not* visible to root `GetImage`, that is a pre-existing capture gap, not a damage problem.
9. **Owner-present run.** Needed for: H.264 and VA-API numbers (agents cannot run VA-API); the fullscreen page-flip case (does root `GetImage` see it, does XDamage report it); shadow-mode miss rates on the owner's compositor; and the default flip (PR 9).
10. **Default-flip policy.** Proposed: flip only after shadow mode shows a miss rate the owner is comfortable with and the sweep catches every injected miss; keep the knob afterwards.

## 7. Appendix: `FrameSubmission` constructors to update in PR 2

`ghostframe-server/src/capture/x11/frames.rs:444`, `ghostframe-lib/src/ffi.rs:140`, `ghostframe-lib/src/server.rs:298` (test), `ghostframe-e2e/tests/c2_loop_stall_measure.rs:233`, `ghostframe-e2e/tests/c2_frame_off_loop.rs:108`, and the seven sites in `ghostframe-lib/src/transport/io_bridge/tests.rs`. PR 2 changes the pixel buffer type; PR 6 adds the damage variants, with the same sites.
