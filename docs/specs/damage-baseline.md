# Damage-driven encoding: PR 0 baseline

Measured by `capture::x11::baseline` (see the module docs), the harness for
row 0 of the plan in [damage-driven-encoding-design.md](damage-driven-encoding-design.md).
Reproduce on a quiet machine:

```bash
flock -o <bench.lock> cargo test -p ghostframe-server \
    baseline_table -- --ignored --nocapture
```

## What is measured

One tick = what the capture loop runs between its `capture_us` timestamps:
`drain_damage` + `capture`, after a scene paint that is not timed.

- `capture_us`: wall time of that span.
- `cpu_us`: CPU time (user+system, `getrusage(RUSAGE_THREAD)`) of the capture
  thread over the same span. The X server's own CPU is not included.
- `bytes/tick`: the cursor image (1024 bytes for the 16x16 Xvfb cursor) and,
  before PR 2, the root grab (`ShmGetImage` then `to_vec`, `W*H*4`). Since
  PR 2 the SHM grab is written by the X server into a pooled slot and this
  process copies nothing; only the `GetImage` fallback still counts the
  reply buffer.
- `allocs/tick`: real heap allocations at the counted sites: the root grab,
  the cursor reply, and the damage path (the tile set's growth steps derived
  from its final capacity, the result list, the cursor tile list, and the
  merge's `seen` set and list growth). Empty collections allocate nothing and
  count nothing. Not a global allocator count. Pooling (PR 2) took the grab
  out of it; a slot allocated for the first time counts too, so the warm-up
  ticks are not zero.
- `hint tiles/tick`: tiles in the `drain_damage` hint (0 = `None`, "compare
  everything").

In the live daemon the same figures (`cpu_us`, `bytes_copied`, `allocs`) are on
the `[CAP] iter done` trace line (`RUST_LOG=ghostframe::server::capture=trace`).
`cpu_us` is only sampled on the ticks whose line is logged.

Scenes (deterministic, seeds 1000-1002): idle; typing (30x16 px every third
tick, i.e. 10 Hz at 30 fps); scrolling (up to 1920x900 region, 32 px per tick);
video (full screen, 16x9 grid of new colours every tick). 90 measured ticks per
run after 2 warm-up ticks, 3 runs per cell. Cells show the median across runs;
`capture_us` also shows min-max across runs.

Host: Xvfb 24-bit root, MIT-SHM, dev build profile as the tests use
(`opt-level` optimised, debuginfo). Dual-1080p is one 3840x1080 root.

## Baseline: Xvfb sink

| layout | scene | capture_us (median, min-max) | cpu_us | bytes/tick | allocs/tick | hint tiles/tick |
|---|---|---|---|---|---|---|
| 1080p | idle | 2151 (2008-2465) | 526 | 8295424 | 3.0 | 0 |
| 1080p | typing | 1763 (1675-1769) | 307 | 8295424 | 4.7 | 2 |
| 1080p | scrolling | 2144 (2124-2173) | 779 | 8295424 | 24.0 | 1740 |
| 1080p | video | 2333 (2331-2366) | 976 | 8295424 | 26.0 | 2040 |
| 4K | idle | 6726 (6717-6734) | 2319 | 33178624 | 3.0 | 0 |
| 4K | typing | 6722 (6718-6741) | 2315 | 33178624 | 4.7 | 2 |
| 4K | scrolling | 7091 (7040-7099) | 2565 | 33178624 | 25.0 | 1744 |
| 4K | video | 8513 (8405-8535) | 3536 | 33178624 | 30.0 | 8160 |
| dual-1080p | idle | 3377 (3336-3377) | 1008 | 16589824 | 3.0 | 0 |
| dual-1080p | typing | 3380 (3342-3382) | 996 | 16589824 | 4.7 | 2 |
| dual-1080p | scrolling | 3628 (3625-3632) | 1214 | 16589824 | 25.0 | 1742 |
| dual-1080p | video | 4451 (4431-4456) | 1681 | 16589824 | 28.0 | 4080 |

Reading it:

- The grab dominates and is the same in every scene: 8.3 MB and about 2 ms per
  tick at 1080p, 33 MB and about 6.7 ms at 4K (30 ticks/s is 1 GB/s of copying
  at 4K). An idle screen costs as much as a typing one (the 1080p idle median
  is noisier than typing: see its min-max); this is the idle cost the design
  note said was missing, and it scales with the screen, not with damage.
- Bytes copied are scene-independent, confirming stage 3 of the design note
  is screen-proportional.
- Busy scenes add on top of the grab, in the timed span: video is +8% wall
  and +86% CPU over idle at 1080p (+27% / +52% at 4K, +32% / +67% dual). That
  extra is the capture thread's own damage handling (parsing `DamageNotify`
  events, filling the tile hash set, the cursor merge), which is why allocs
  rise from 3 to 24-30 and hint tiles reach 2040-8160. The X server's drawing
  is outside the span (the paint is synced before the timer starts).
- The Xvfb sink has no compositor, so damage never saturates beyond the
  scene's own area.

## Baseline: owner's compositor

Pending owner run. Purpose: see damage saturation (a compositor that redraws
the whole screen on every frame reports full-screen damage even when idle).
Run the daemon with `RUST_LOG=ghostframe::server::capture=trace` in the
owner's session and read `bytes_copied`, `cpu_us`, `capture_us` from
`[CAP] iter done`.

**Row 0 of the plan is not closed until the owner's compositor / Xorg-sink run
is recorded here.**

## Gate

- Non-zero: `RunReport::validate` requires non-zero wall time, CPU time and
  allocations, and that every tick (not just the mean) copied at least
  `W*H*4` bytes. The idle and busy tests also require strictly more than
  `W*H*4` (the cursor image), and the determinism test compares per-tick hints
  and a pixel hash for typing, scrolling and video, and that another seed
  changes the pixels.
- Mutations, each failing a test: counting half the grab (`tick 0 copied
  461824 bytes, below the W*H*4 = 921600 floor`); dropping the cursor bytes
  (`tick 0: ... bytes_copied: 921600 ...` against the `> W*H*4` assertion, in
  both the idle and busy tests); ignoring the seed (`typing: seed does not
  change the content`); stubbing `take_stats` to zeros (`counter bytes_copied
  is zero`). `the_harness_rejects_a_zeroed_counter` covers each of the four
  counters on synthetic data.

## PR 2: pooled frame buffers (before/after)

Same harness, same host, run back to back, `flock`ed. "Before" is the head of
PR 1 (`5e2288a0`: it already differs from the PR 0 table above in allocs,
because PR 1 changed the damage path); "after" is PR 2.

| layout | scene | bytes/tick before | after | allocs/tick before | after | capture_us before | after | cpu_us before | after |
|---|---|---|---|---|---|---|---|---|---|
| 1080p | idle | 8295424 | 1024 | 2.0 | 1.0 | 1796 | 497 | 336 | 38 |
| 1080p | typing | 8295424 | 1024 | 2.7 | 1.7 | 1765 | 496 | 307 | 38 |
| 1080p | video | 8295424 | 1024 | 4.0 | 3.0 | 1956 | 640 | 553 | 71 |
| 4K | idle | 33178624 | 1024 | 2.0 | 1.0 | 6785 | 3979 | 2327 | 52 |
| 4K | typing | 33178624 | 1024 | 2.7 | 1.7 | 6770 | 3980 | 2384 | 56 |
| 4K | video | 33178624 | 1024 | 4.0 | 3.0 | 7497 | 4694 | 2527 | 183 |

Which copy disappeared: the segment-to-`Vec` `to_vec` (and its allocation).
Bytes fall by exactly `W*H*4` (8294400 at 1080p, 33177600 at 4K). The X server
still writes the pixels into the segment (that is the grab itself, in the X
server's process, and not counted); the segment *is* the frame the library
receives, so there is no copy in the daemon. The cursor blend is in place.

Not changed: the library still compares the full frame against its previous
image (PR 3 onward), and the `GetImage` fallback (no MIT-SHM, e.g. a remote
display) still pays one full-frame copy and allocation, because x11rb
allocates the reply; the reply buffer becomes the frame unpooled.

`capture_us` falls more than the copy alone would suggest at 1080p
(1796 -> 497): the `to_vec` page-faulted a fresh 8 MB allocation every tick.
The 4K figure still carries the X server's own grab, in the timed span.

Gate: `a_warm_shm_grab_allocates_and_copies_nothing` (warm-up must count the
slot allocations, steady state at most the cursor reply and far below
`W*H*4`, and the GetImage path still counts the full copy);
`RunReport::validate` now rejects any tick at or above `W*H*4`
(`the_harness_rejects_a_full_frame_copy`).
