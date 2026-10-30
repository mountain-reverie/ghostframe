# Design note: damage-driven encoding

**Status:** design, not implemented. **Date:** 2026-10-29 · **Base:** master at 173ba45f
**Context:** [ghostframe-server X11 spec](../superpowers/specs/2026-10-29-ghostframe-server-x11-design.md) §2 (non-goal: "`FrameSubmission::damage_tiles` is ignored by the library today"), [remediation scoreboard](../reviews/remediation-scoreboard.md) (C2 measurement, B2 note).

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
| 4 | `blit_cursor` into the frame | `frames.rs:427` | cursor size | negligible |
| 5 | `drain_damage` result written to `frame.damage_tiles`, frame sent over an mpsc channel | `commands/capture.rs:246-251, 280`; `ghostframe-lib/src/server.rs:203` | constant | the field then dies (below) |
| 6 | `process_frame` -> `begin_gpu_frame` -> worker `Analyse`; the loop thread also runs `collect_escalation_candidates` (O(tiles)) | `ghostframe-lib/src/transport/io_bridge/frame.rs:265, 764, 1312` | tiles | small (estimate: tens of us at 4K) |
| 7 | Upload: `import_pixels` makes a **new** HOST_VISIBLE linear image, maps it, copies every row, makes a view | `capture/gpu_pipeline/frame.rs:303-349, 1249-1340` | **screen** | third full-frame copy (estimate: same order as stage 3) |
| 8 | GPU, one command buffer: `tile_sad` (all tiles), NV12 conversion (all pixels), `tile_analysis`, PalRLE compact/fold/index, CDF 5/3 compact + wavelet L1-L3 for dirty tiles, snapshot copy of the whole frame into `prev_image` | `gpu_pipeline/frame.rs:463-700`, `record_snapshot_copy` :2248, shaders `tile_sad.comp` etc. | SAD, NV12, snapshot: **screen**; compact and wavelet: **dirty tiles** | **(measured)** GPU total 7 ms at 1080p/5 % dirty, 18.5 ms at 1080p full, 27 ms at 4K/5 %, 71.5 ms at 4K full (`docs/reviews/remediation-scoreboard.md:226-233`). The 5 % vs full gap shows the dirty-proportional part; the 5 % figure is the floor the screen-proportional part leaves. No idle-screen number exists |
| 9 | Classifier and frame-mode decision (`classify_dirty_tiles`), `pixelperfect_sweep` every frame over all tiles, scheduler dispatch | `frame.rs:1363, 1546`; `tile/classifier.rs` | dirty tiles; the sweep is O(tiles) | small |
| 10 | CDF 5/3 pass encode on the CPU | `encode_passes_sparse` | **dirty tiles** (~134 us/tile, **(measured)**) | 16 ms at 1080p/5 %; 273 ms at 1080p full; 1.15 s at 4K full. Dominant stage, but already proportional to damage |
| 11 | H.264 branch: full-frame encode from the NV12 buffer; skipped only when no tile is dirty | `frame_backend.rs:331`, `frame.rs:189` (`h264_finish_action`) | screen when it runs | not measured (VA-API is off for agents) |
| 12 | Emission through the scheduler | `io_bridge/emit.rs` | dirty tiles plus refinement queue | small |

### Where `damage_tiles` is dropped

`damage_tiles` is **not** dropped in the server or the channel; it is dropped by the library. Its only reader is `frame.rs:427`, inside `process_frame_cpu_at`, which is `#[cfg(test)]`-only (`frame.rs:300-318`, "unreachable in production"). The GPU path moves the whole `FrameSubmission` into `ToWorker::Analyse` (`frame.rs:794-815`, `ToWorker::Analyse` at :806) and the worker never looks at it. `X11Backend::drain_damage` says so itself (`capture/x11/mod.rs:130-131`). The C ABI constructs it as `None` (`ghostframe-lib/src/ffi.rs:140`); the Rust server path is the only producer.

A second defect in the producer: `drain_damage` returns `None` for two different things, "no damage, nothing changed" and "unknown, compare everything" (`capture/x11/mod.rs:132-145`, `merge_cursor_damage` :151). A damage-driven consumer needs a tri-state.

### What scales with what

- **Screen size, every tick, even when idle:** stages 3, 7, and the SAD, NV12 and snapshot parts of 8. Estimate for a 4K idle tick: ~3 CPU copies of 33 MB plus a full-frame GPU compare and convert, 30 times a second. The scoreboard's "5 % dirty" rows are therefore not near the idle cost; an idle measurement is PR 0.
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

1. The cursor poll is a synchronous round trip every tick. At idle it becomes the dominant cost. Replace by `XFixesSelectCursorInput` (cursor-image change events) plus `QueryPointer` for position; still one round trip per idle tick, but no image transfer. Tracked as PR 5; measure first, it may not matter.
2. The cursor must stay consistent with a **persistent** frame buffer. `blit_cursor` mutates the buffer in place, so the rows under the *old* cursor rectangle must always be re-grabbed from X to restore clean pixels. That is exactly what including the old tiles in the hint does; it becomes a tested invariant (property in section 5), not an accident.

### RandR and resize

Geometry is re-read once per tick (`frames.rs:316-336`, `refresh_geometry`). Any change must produce `Unknown` damage for that tick, reallocate the persistent buffers, and invalidate the GPU baseline (the library already handles a dimension change: `prev_image` is dropped and the first-frame branch marks everything dirty, `gpu_pipeline/frame.rs:382`). The Damage object stays valid on the root across a mode change, but region contents at the moment of change are meaningless; treat the next two ticks after any geometry change as `Unknown`. RandR events (`capture/x11/randr_events.rs`) are the trigger already used elsewhere.

### Coverage gaps

XDamage on the root reports drawing that reaches the root's pixels. It can miss:

- **Unredirected fullscreen applications** (compositor unredirect, DRI3/Present page flips): pixels reach the scanout without passing through the server's drawing path, so no DamageNotify. This is the main hole and is also the x11vnc "cannot trust" case.
- Some GL clients under non-compositing setups on drivers that flip.
- Damage lost when the event queue is flooded and we disable the monitor (our own escape hatch, like x11vnc's).
- Over-reporting is the opposite gap: harmless (SAD says nothing changed) but wasteful.

Fallbacks, in layers:

1. **Authoritative GPU compare stays.** Damage selects *candidate* tiles; `tile_sad` against the snapshot still decides what is dirty. A false positive costs a grab and a compare; a false negative is the only dangerous case.
2. **Rolling verification sweep.** Every tick, add a rotating slice of rows to the candidate set so the whole screen is verified every N seconds (default proposal N = 5). Cost is O(1) per tick for a given resolution (a few percent of one full grab per second; 33 MB / 5 s = 6.6 MB/s at 4K, estimate). Staleness from a missed report is bounded by N.
3. **Trust state machine.** Any sweep correction (the sweep found a tile dirty that damage did not hint) moves the monitor to `Distrusted`: damage hints are ignored and every tick is a full compare, today's behaviour, for T seconds (proposal 10 s), counting clean full compares to return to `Trusted`. A fullscreen GL app thus self-demotes to today's cost within one sweep period instead of showing a stale screen. Corrections are counted and logged rate-limited, so the gap is observable.
4. **Flood escape**: more than E raw events per tick, or more than a size cap of damaged area (x11vnc style), becomes `Unknown` for that tick: one full grab beats thousands of small ones.

## 3. What to skip when damage is known

### 3.1 Grab only damaged pixels

`ShmGetImage` writes a rectangle *packed* at `shmoffset`, not at its position in a larger image. So per-rectangle grabs cannot land in place in a full-size buffer. **Row bands do**: a full-width band `y0..y1` is contiguous at offset `y0 * stride`, exactly where it belongs. Design:

- Keep a persistent full-size frame buffer (a small ring, depth = frames in flight; the library holds one frame in flight, `frame.rs:765`), replacing the per-tick `to_vec()` (`frames.rs:372`).
- Convert the tile bitmap into row bands (merge bands closer than a few tile rows). Issue all `ShmGetImage` requests for the tick back to back and flush once, pipelined, so the server handles them consecutively.
- If the banded area exceeds a fraction F of the screen (proposal 40 %, to be set by the PR 2 measurement, not guessed), do one full grab: fewer requests, no worse than today.
- Band granularity wastes width (a 30x16 typing rect grabs a 1920-wide, 32-row band: 245 KB instead of 2 KB). Still ~35x less than a 1080p frame (estimate). Narrow rect grabs into scratch plus a scatter copy are a later refinement if typing latency demands it (open decision 3).

The banded approach also matches the library's existing rows idiom: `readback_dmabuf_rows(fd, w, h, stride, rows: &[Range<u32>])` (`capture/dmabuf.rs:410`) and its contract ("`rows` must cover every row the producer changed ... call `invalidate()` whenever a frame was dropped").

### 3.2 Upload, compare and convert only damaged tiles

New `FrameSubmission` shape (the field exists; give it a consumer):

```rust
pub enum Damage { Unknown, None, Tiles(TileMask) }   // tri-state; Unknown = compare all
```

Library changes, in order of payoff:

1. **Persistent upload image** instead of `import_pixels` allocating a new image every tick (`gpu_pipeline/frame.rs:1249`): allocate once per resolution, copy only the dirty bands (`Damage::Tiles`), full copy for `Unknown`.
2. **`tile_sad` hinted**: push a candidate mask; non-candidate workgroups exit at once (or dispatch indirect over the candidate list; O(candidates)). Non-candidate tiles are known-clean, not "compared and found equal", so their snapshot is untouched.
3. **NV12 persistent and partial**: `ensure_nv12_buffer` is already per-resolution persistent (`gpu_pipeline/frame.rs:154`), so converting only candidate 2x2 blocks keeps it correct provided *every* candidate is converted whatever the mode. This preserves the TileCodec-to-H.264 switch, which needs a complete NV12 frame (section 3.4).
4. **Snapshot copy partial**: `record_snapshot_copy` (`:2248`) copies only tiles that SAD found dirty (rows or tiles via `vkCmdCopyImage` regions).
5. **Skip entirely on `Damage::None`** (below).

Correctness note: the SAD result is a subset of the candidate mask by construction; the snapshot of a candidate tile is updated iff the tile is committed dirty, exactly as now. The `no_commit` cushion and first-frame branch (`frame.rs:382-460`) stay `Unknown`-only paths.

### 3.3 The progressive-refinement / PixelPerfect state machine

This is the main trap. Refinement, PixelPerfect transitions and idle escalation are driven *by the frame tick*, not by damage: `pixelperfect_sweep` runs "every frame, unconditionally, before the dirty-tile early return" so ACKs arriving in static phases still drive transitions (call at `frame.rs:1033`, body at :1546); idle escalation counts idle frames (K=512, idle > 30) and its forward dispatch reads pixels from the current frame (`collect_escalation_candidates`, `frame.rs:1312`; stage 4d in `gpu_pipeline/frame.rs`); the scheduler keeps draining un-sent refinement passes when `dirty_xy` is empty (the CDF 5/3 early-return comment in `on_analysed`).

So an idle screen cannot simply stop ticking the library, or undamaged tiles stop refining and never reach PixelPerfect. Design:

- Add a **content-free tick**: `FrameSubmission` carries `content: Pixels | Unchanged`. `Unchanged` means "same pixels as the last committed frame; run the state machine". The worker treats the retained `prev_image` snapshot as the current image for escalation forward dispatch (it is by definition identical to the last committed frame), and skips upload, SAD, NV12 and analysis. The server sends `Unchanged` when damage is `None`, the cursor is unchanged, no sweep slice is due, and the baseline is committed (`prev_image` exists, not in the `no_commit` cushion, no geometry change). Otherwise it sends pixels.
- The tick rate for `Unchanged` stays the capture rate at first, so idle-escalation frame counts keep their meaning. Converting those counters to time (so idle ticks can drop to a few Hz) is optional and separate (PR 6); doing it silently would change the escalation latency users see.
- Damage that arrives for a tile in mid-refinement bumps its generation exactly as a SAD-dirty tile does today: the SAD still produces the dirty list, so `drop_for_tile`/`bump_generation` paths are untouched.
- Property to keep (and test, section 5): a tile that is not damaged for D ticks reaches the same PixelPerfect/escalation state at the same tick under damage-driven mode as under full-compare mode.

### 3.4 H.264-mode regions

`FrameMode::H264` encodes the whole frame from the NV12 buffer (`frame_backend.rs:331-376`); it already skips when no tile is dirty (`h264_finish_action`, `frame.rs:189`). With damage:

- Idle H.264 sessions already skip the encode. The win is upstream: no grab, upload or NV12 conversion of the unchanged screen (3.1-3.3).
- Partial NV12 update (3.2 item 3) keeps the encoder input exact.
- *Not in scope*: encoding only damaged macroblocks. VA-API/ffmpeg offer ROI hints and skip-block control only indirectly; the encoder already sees a mostly static frame and spends few bits on it. Revisit after PR 3 numbers (open decision 4).
- Damage makes the **mode decision cheaper and more honest**: `collect_escalation_candidates` skips escalation under H.264 (`frame.rs:1312-1330`). The "full-screen video" case is the damage-saturated case: damage is ~100 % every tick, so Distrust-style full compare is correct and free of overhead; the design must not make it slower than today (gate in section 5).

### 3.5 The "dirty rows" readback from the scoreboard

The B2 note (`remediation-scoreboard.md:66`) says dirty-rows readback is API only: `readback_dmabuf_rows` (`capture/dmabuf.rs:410`) exists and is tested but nothing calls it. It is a **dma-buf mmap -> host** readback for the DRM capture path, which master no longer uses (the X11 spec §5 keeps dma-buf ingest unused). It does not help the X11 path directly, but its contract (rows must cover all changes, `invalidate()` on dropped frames) is the same contract as 3.1, and its tests (`dirty_rows_copy_exactly_the_listed_rows`, `dmabuf.rs:865`) are a template. If a future Wayland/PipeWire backend yields dma-bufs with damage rects, this is where they plug in. Decision: leave it alone in this work; reuse its contract wording.

## 4. Correctness risks

| Risk | Consequence | Mitigation |
|---|---|---|
| Missed damage (unredirected GL, driver flip, lost events) | stale tile forever | rolling sweep (2.2), trust state machine with demotion to full compare, correction counter. A tile is never stale longer than the sweep period N plus one tick |
| Drain/subtract/grab reordering | lost updates | keep subtract-before-grab; test that fails on reorder |
| Tearing between band grabs | a mixed frame for one tick | bands pipelined back to back; any drawing after the subtract re-reports, so the next tick repairs it. Not worse than a single GetImage racing a compositor swap. `XGrabServer` rejected: stalls the whole session |
| Persistent buffer + in-place cursor blit | cursor trail or ghost | old and new cursor tiles always in the candidate set (exists, `mod.rs:132`); property test |
| `None` conflated with `Unknown` | first frame for a new client not sent in full | tri-state `Damage`; `Unknown` forced on: new session, `force_dirty_frames`/`no_commit` cushion, geometry change, session reset, capture error, Distrusted |
| Dropped frame / `InFlight` stall | snapshot and damage out of sync (the dma-buf `invalidate()` contract) | any dropped or failed frame forces the next one `Unknown`; the server cannot know the library dropped one, so the library marks `ensure_baseline_invalidated` (`frame.rs:838`) and the server learns through the existing return path or a counter (PR 3 decides) |
| Damage bitmap sized for old geometry after resize | out-of-bounds or silently wrong tiles | bitmap carries `(cols, rows)`; library treats mismatch as `Unknown` |
| Multi-monitor root | tile grid is on the *root* (the whole virtual screen, 3840x1080 for dual 1080p); gaps between monitors are black | root-relative tiles already; XDamage on root covers all outputs. Test on a two-output Xorg with a non-aligned offset |
| Damage rect negative origin or beyond screen | wrong tile range | `rect_to_tiles` clamps and over-approximates by design (`damage.rs:256-262`); region fetch gives clipped rects but keep the clamp |
| Event flood | O(events) CPU and memory in the X client queue | `NonEmpty` + parts, or flood escape (2.2 item 4) |
| Hint trusted when XDamage was never armed | permanent staleness | `Distrusted` unless `damage_armed()` (`capture/mod.rs`), which already exists for exactly this |

## 5. Plan: small PRs in order

Each PR is independently shippable and revertible. Measurements use one harness (PR 0) on four **scenes**: *idle* (static desktop), *typing* (one 30x16 px glyph-size update at 10 Hz, text-like), *scrolling* (a 1920x900 region scrolling 32 px per tick), *video* (full screen, 30 Hz new content). Metrics per scene: server CPU time per tick (process CPU / ticks, from `getrusage` over the run), `capture_us` and `submit_us` from the `[CAP] iter done` trace (`commands/capture.rs:319`), worker `gpu_us`, and `process_frame` stage laps (`stage_trace_enabled`, `frame.rs:150`), bytes copied per tick. Three resolutions: 1080p, 4K, dual-1080p root. Report 3+ runs, as the browserless notes require.

**Gates are properties, not magic numbers.** Instead of "idle tick < 2 ms":

- *Idle independence:* `idle_cost(4K) / idle_cost(1080p)` is below a bound derived from the tile count ratio of the O(tiles) terms only (the sweeps), and `idle_cost` after PR 2 is below `idle_cost` before PR 0's baseline by at least the screen-proportional share measured in PR 0.
- *Proportionality:* across dirty fractions {0, 1, 5, 25, 100 %} per-tick cost is monotone non-decreasing and the slope is bounded by the full-compare slope.
- *No regression at saturation:* video scene cost within noise of the baseline (compared in the same process, same run, damage mode switched by config).
- *Mutation proof for every gate:* revert the feature (or inject the bug named) and the gate must fail. Record the failing output in the PR gates table.

| PR | Change | Measurable gate | Correctness gate (mutation-proven) |
|---|---|---|---|
| **0. Baseline harness** | Server-side per-tick CPU and byte counters; scene driver for the four scenes via the Xorg `--x11-sink` test pattern; record a baseline table for 1080p/4K/dual. No behaviour change | The measurement itself: prove non-zero (counters increment, the idle scene shows nonzero `capture_us`). Also records the missing **idle** cost | Mutation: stub the counter to 0; the harness must reject the run |
| **1. Honest damage producer** | Tri-state `Damage` in the `Backend` trait (`Unknown/None/Tiles`); tile bitmap replaces `HashSet`; `NonEmpty` + `DamageSubtract(parts)` + `FetchRegion` behind a config knob next to `RawRectangles`; flood escape; first-frame/resize/error force `Unknown`. Library still ignores it | per-tick `drain_damage` cost in scrolling and video is O(1) in event count: with N synthetic rects per tick (property: cost slope vs N is below RawRectangles' slope) | X11 unit tests in the existing `x11_*` style (xvfb-run, `damage.rs` tests): real draws produce a superset of the touched tiles (property over random rects, like `rect_to_tiles` tests); reorder `drain` and `subtract` (mutation) and the lost-update test fails |
| **2. Persistent buffer + band grab (server side only)** | Replace `to_vec()` with a ring of persistent buffers; band grab for `Tiles`, full grab for `Unknown` or above F; cursor tiles always candidates. Library still receives full frames (stride-exact buffers), so output is unchanged | Idle and typing: bytes copied per tick falls from `W*H*4` to the band bytes (assert the counter equals the band area, a property); `capture_us` for typing is independent of resolution (1080p vs 4K ratio near 1) | Property: for random damage sequences including cursor moves, the persistent buffer equals a fresh full grab after every tick (a test X sink scene with a deterministic painter). Mutation: drop the old cursor tiles from the hint, the equality property fails. Mutation: skip a band, fails |
| **3. Library consumes `Damage`** | `FrameSubmission.damage` tri-state; persistent upload image with dirty-band copy; hinted `tile_sad`; partial NV12; partial snapshot copy. CPU test path ignores it as before. `ghostframe.h` regeneration if the type is exported. Update all constructors (list in section 7) | Typing and scrolling: `gpu_us` and upload time per tick drop and become proportional to the damaged fraction (proportionality property); the existing 7 ms floor at 1080p/5 % moves toward the 5 % part only | **Oracle:** under any `Damage::Tiles(S)` where S is a superset of the truly changed tiles, the emitted dirty list and the committed snapshot equal the `Unknown` run's, byte for byte (proptest over random frame pairs, GPU lock via the project's GPU runner). Mutation: make S omit a changed tile and assert the oracle fails, proving it can see missing damage. Browserless seeds: 3+ |
| **4. Content-free tick** | `Unchanged` submissions; worker runs the state machine from `prev_image`; server sends it for `Damage::None` with committed baseline | Idle: per-tick CPU and bytes copied drop to the O(tiles) sweep floor; idle p50 `process_frame` falls (compare to PR 0). Typing latency (damage to first datagram) not worse | **State-machine equivalence:** an undamaged tile reaches PixelPerfect and idle escalation at the same tick as in full-compare mode (the `e2e_progressive_refinement`, `e2e_cdf53_*` and lossless_buildup scenes unchanged and green). Mutation: make `Unchanged` skip `pixelperfect_sweep`, and those scenes fail |
| **5. Safety net: sweep + trust state machine** | Rolling verification slice every tick; correction counter; `Distrusted` demotion and recovery; rate-limited warn; metrics in the `cumulative emit` style | Sweep cost is O(1) per tick for a fixed resolution; period N verified: with damage fully ignored, every tile is verified within N seconds (a property over ticks, not a time assertion) | **Injected missed damage:** a test `Backend` decorator drops the damage for a chosen rect (decorator, not an env var, to respect `ci/check-env-reads.sh`). The stale tile is repaired within the sweep period; the correction counter is non-zero; the monitor reaches `Distrusted`. Mutation: disable the sweep, and the stale tile persists, failing the gate. A fullscreen painter that emits no damage must also be repaired |
| **6. Cheap idle (optional, measure first)** | Cursor via `XFixesSelectCursorInput` + `QueryPointer`; event-driven refinement set instead of O(tiles) sweeps; idle tick rate decoupled from capture rate with time-based escalation counters | Idle CPU independent of resolution (the Idle independence property); wake latency from damage to tick unchanged | Escalation timing equivalence in time units, not frames; the cursor ghost property from PR 2 |

**Which scenes can measure it:**

- *Existing:* `ghostframe-e2e/tests/c2_loop_stall_measure.rs` already builds frames with a `dirty_frac` (`:102-122`) and measures `process_frame` stalls; it is the model for the library-side proportionality measurement (PR 3, 4). The browserless suite (`scene_tiles.rs`, `browserless_loop_stall.rs`) exercises the library without X, so it validates PR 3-5 oracles but never the X11 grab. All browserless runs go under the shared bench lock, one scene binary at a time.
- *Needed, new:* an **Xorg damage scene** on the e2e container's `--x11-sink` (`DEVELOPERS.md:174`, plan `Task 4`): the test pattern gains modes *idle*, *typing*, *scroll*, *video*, *fullscreen-flip* (an update path that bypasses damage, e.g. a client that writes through a mapped path or uses a Present-style flip if available; otherwise the decorator in PR 5 is the substitute), and a deterministic seed so runs are comparable. It must support at least 4K and a two-output layout. Only this scene exercises XDamage, `ShmGetImage` bands and the cursor together; no existing scene does.
- The e2e container needs `just containers-build` after any `ghostframe-lib` or `ghostframe-server` change (AGENTS.md); X11 unit tests need `xvfb-run`.

Wire-format note: nothing in this plan changes the wire format. Four crates consume the wire format (`ghostframe-protocol`, `ghostframe-lib`, `ghostframe-client-core`, `ghostframe-client-wasm`); none is touched.

## 6. Open decisions for the owner

1. **Tolerated staleness bound.** Sweep period N (proposal 5 s) trades verification cost for worst-case stale-tile age. Is 5 s acceptable for a missed-damage tile, or should it be 1-2 s at higher cost?
2. **Distrust policy.** Demote to full compare for T = 10 s on any correction (proposal), or demote per-region? Per-region is more complex and wins only in mixed sessions (a GL window plus a terminal).
3. **Band grab vs rectangle grab.** Row bands are simple and land in place; narrow rects save bandwidth for typing but need a scatter copy. Start with bands and add rects only if the typing gate asks (proposal)?
4. **H.264 regions.** Leave the full-frame encoder alone (proposal), or invest in ROI/skip hints after PR 3 numbers?
5. **`Unchanged` tick rate.** Keep idle ticks at the capture rate until PR 6 (proposal), or allow a lower idle rate immediately and accept that idle escalation latency (counted in frames) lengthens?
6. **XDamage report level.** Adopt `NonEmpty` + region fetch (proposal, needs a real-session measurement on the owner's compositor setups) or stay on `RawRectangles` with the flood escape?
7. **Where tri-state lives.** Add to `FrameSubmission` (breaking for every constructor, ~11 sites, and the C ABI) or a separate `submit_damage` message? Nothing is in production, so the owner's no-compat rule favours changing the struct.
8. **Fullscreen unredirected GL.** Accept the self-demotion behaviour (full cost, no staleness) or also disable unredirection (a compositor-specific hint) for attach mode? The former needs nothing from the user's session; the latter touches it.
9. **Owner-present measurements.** H.264 numbers need VA-API, which agents cannot run; PR 3 and 4 gates for the H.264 mode, and any real-compositor fullscreen measurement, need a solo run on a real session.

## 7. Appendix: `FrameSubmission` constructors to update in PR 3

`ghostframe-server/src/capture/x11/frames.rs:444`, `ghostframe-lib/src/ffi.rs:140`, `ghostframe-lib/src/server.rs:298` (test), `ghostframe-e2e/tests/c2_loop_stall_measure.rs:233`, `ghostframe-e2e/tests/c2_frame_off_loop.rs:108`, and the seven sites in `ghostframe-lib/src/transport/io_bridge/tests.rs`.
