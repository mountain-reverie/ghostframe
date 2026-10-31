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

## PR 5: band grab (`damage_mode = on`), before/after

Same harness (`baseline::band_table`, `cargo test -p ghostframe-server band_table -- --ignored --nocapture`),
same host, run under `flock -o` on the shared bench lock. "off" is the
whole-root grab (the before; identical to the PR 2 path), "on" the band grab
with the default sweep, "on, no sweep" the damage-proportional part alone.
The harness holds three frames so the four slots rotate as in the daemon, and
runs 10 warm-up ticks; the pointer is parked at (10,10). 90 ticks per run, 3
runs (seeds 1000-1002), median across runs with min-max for `capture_us`.
The window is `drain_damage` + net plan + `capture`, as before.

| layout | scene | damage_mode | capture_us (median, min-max) | cpu_us | MB grabbed/tick | requests/tick |
|---|---|---|---|---|---|---|
| 1080p | idle | off | 1060 (1038-1078) | 36 | 8.29 | 1.0 |
| 1080p | idle | on | 136 (132-141) | 42 | 0.80 | 1.7 |
| 1080p | idle | on, no sweep | 79 (79-81) | 34 | 0.25 | 1.0 |
| 1080p | typing | off | 1041 (1036-1044) | 44 | 8.29 | 1.0 |
| 1080p | typing | on | 198 (196-212) | 46 | 1.52 | 1.6 |
| 1080p | typing | on, no sweep | 143 (143-150) | 40 | 0.98 | 1.0 |
| 1080p | scrolling | off | 895 (883-907) | 58 | 8.29 | 1.0 |
| 1080p | scrolling | on | 949 (941-1181) | 75 | 8.29 | 1.0 |
| 1080p | video | off | 981 (923-1006) | 68 | 8.29 | 1.0 |
| 1080p | video | on | 1082 (1070-1171) | 90 | 8.29 | 1.0 |
| dual-1080p | idle | off | 2304 (2025-2637) | 55 | 16.59 | 1.0 |
| dual-1080p | idle | on | 253 (248-293) | 62 | 1.60 | 1.7 |
| dual-1080p | idle | on, no sweep | 129 (127-145) | 48 | 0.49 | 1.0 |
| dual-1080p | typing | off | 2085 (2014-2236) | 54 | 16.59 | 1.0 |
| dual-1080p | typing | on | 445 (417-452) | 66 | 3.03 | 1.6 |
| dual-1080p | typing | on, no sweep | 323 (309-332) | 63 | 1.97 | 1.0 |
| dual-1080p | scrolling | off | 2396 (2322-2821) | 84 | 16.59 | 1.0 |
| dual-1080p | scrolling | on | 2907 (2822-3437) | 117 | 16.59 | 1.0 |
| dual-1080p | video | off | 2758 (2455-2811) | 101 | 16.59 | 1.0 |
| dual-1080p | video | on | 2589 (2578-2596) | 115 | 16.59 | 1.0 |
| 4K | idle | off | 4814 (4158-4945) | 63 | 33.18 | 1.0 |
| 4K | idle | on | 439 (430-457) | 78 | 2.69 | 1.8 |
| 4K | idle | on, no sweep | 136 (133-145) | 54 | 0.49 | 1.0 |
| 4K | typing | off | 3978 (3968-4000) | 60 | 33.18 | 1.0 |
| 4K | typing | on | 500 (492-506) | 71 | 4.12 | 1.7 |
| 4K | typing | on, no sweep | 282 (276-290) | 62 | 1.97 | 1.0 |
| 4K | scrolling | off | 4109 (4105-4189) | 66 | 33.18 | 1.0 |
| 4K | scrolling | on | 3046 (2875-3326) | 87 | 22.66 | 1.0 |
| 4K | video | off | 4635 (4626-4855) | 100 | 33.18 | 1.0 |
| 4K | video | on | 4874 (4736-4928) | 146 | 33.18 | 1.0 |

Reading it:

- Idle and typing fall by 5-8x (1080p) and 8-11x (4K) in `capture_us`, and
  by 5-12x in bytes the X server writes. `cpu_us` is flat (the grab is the X
  server's work, outside this process).
- **Typing is not resolution independent in time.** The rows grabbed are
  (`on_mode_typing_grabs_the_same_rows_at_any_resolution`), but the bands are
  full width, so bytes and time grow with the width: no-sweep typing is 143 us
  at 1080p and 282 us at 4K (2x the width, 2x the bytes). With the default
  sweep it is 198 and 500 us: the sweep slice is 1/12 of the rows and each of
  the four slots takes it once, so 4 of 5 ticks carry about 7 % of the screen,
  which scales with the resolution. Against the full grab (1041 / 3978 us) the
  gap widens with the resolution, but the cost is not flat. A narrower slice
  or a rectangle copy would flatten it; not done here (design decision 3).
- Saturation: scrolling and video fall back to the full grab (video always,
  scrolling at 1080p always, at 4K on part of the ticks: 22.7 of 33.2 MB on average); video pays +10 % / +5 %
  `capture_us` for the plan (`begin_tick`, ring copy), within the run-to-run
  spread of the full grab. No regression beyond that.
- Dual-1080p scrolling (a 3840x1080 root, where the scroll area never gets under
  the fraction) is +21 % on `on` (2396 to 2907 us, wide min-max): the cost of
  the plan plus a full grab. Not a gain; to be watched.
- Round trips: requests per tick are 1.0-1.8 on average (bounded at 16 by
  `MAX_BANDS`, tested), all sent before the first reply is read.

Gate: `on_mode_idle_and_typing_grab_their_band_area` (every tick's
`bytes_grabbed` equals the planned band area, via `RunReport::validate`, and
over 90 % of ticks stay under 40 % of the root);
`the_harness_rejects_a_counter_that_disagrees_with_the_plan` (the check is
live). Correctness is in `band_tests.rs`: every delivered slot equals a fresh
full grab over 160 random ticks. Mutations, each failing that property:
- drop the age union: `tick 4: the delivered slot differs from a fresh full grab at byte 381992 (pixel 266,186), grab was Bands([Band { y0: 224, y1: 352 }])`
- drop the old cursor tiles: `tick 4: ... at byte 1025944 (pixel 486,500), grab was Bands([Band { y0: 160, y1: 352 }])`
- skip a band: `tick 4: ... at byte 1025944 (pixel 486,500), grab was Bands([Band { y0: 160, y1: 352 }, Band { y0: 480, y1: 544 }])`
An injected miss (damage lost before the grab decides anything) shows stale
and is repaired by the sweep within `S*k` ticks plus the pool
(`x11_a_lost_damage_report_is_repaired_by_the_sweep`).
