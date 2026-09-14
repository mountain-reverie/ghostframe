# Production-Like Load Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make browserless scenes sustain production-cadence load for their whole duration, and assert on bandwidth-estimate convergence against a known link capacity instead of on probe counters.

**Architecture:** A new `LoadProfile` generates `FrameScript`s across a scene's full duration, so a scene stops being a short burst followed by silence. `BrowserlessScene.frames` becomes a `SceneLoad` enum so the 14 existing scripted scenes are untouched. Injection cadence becomes a per-scene field defaulting to production's 33.3 ms. `BrowserlessResult` gains a sampled estimate series so a test can assert the estimate tracks a `CapTimeline` step-up.

**Tech Stack:** Rust, tokio `start_paused` virtual clock, existing `NetSim`/`CapTimeline`.

Design: `docs/superpowers/specs/2026-09-13-production-like-load-design.md`. Read it first — particularly the constraints section, which explains why the capture path is not an option in this harness.

---

## File structure

| File | Responsibility |
|---|---|
| `ghostframe-e2e/src/harness/load_profile.rs` (new) | `LoadProfile`, `Churn`, frame generation, `gradient_tile`. Pure functions, unit-tested without the harness. |
| `ghostframe-e2e/src/harness/mod.rs` | Register the module. |
| `ghostframe-e2e/src/harness/browserless.rs` | `SceneLoad`, per-scene cadence, estimate sampling. |
| `ghostframe-e2e/tests/browserless_runner.rs` | Migrate existing scenes to `SceneLoad::Script`; add convergence + step-up tests; rework the two probe scenes. |

`browserless.rs` is already ~950 lines and carries the scene loop. Frame generation goes in its own file rather than growing it further.

## Landmarks

Re-derive these; they drift.

| Symbol | Location |
|---|---|
| `BrowserlessScene` | `browserless.rs` ~line 90 |
| `FrameScript` | `browserless.rs` ~line 84 |
| `FRAME_SPACING_US` | `browserless.rs` ~line 64 |
| `bwe_cell` / `bwe_snapshot.bitrate_bps` | `browserless.rs` ~296, ~374 |
| `CapTimeline::step(first_bps, at_us, then_bps)` | `netsim/profile.rs:78` |
| `shifted_gradient_tile` | `tests/browserless_runner.rs:972` |

---

## Task 1: `LoadProfile` frame generation

**Files:**
- Create: `ghostframe-e2e/src/harness/load_profile.rs`
- Modify: `ghostframe-e2e/src/harness/mod.rs`

- [x] **Step 1: Write the failing tests**

Create `ghostframe-e2e/src/harness/load_profile.rs` with only the tests:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn full_grid_covers_the_whole_duration_at_the_given_cadence() {
        let p = LoadProfile {
            cadence_us: 33_333,
            churn: Churn::FullGrid,
        };
        let frames = p.frames_for(Duration::from_secs(10), 4, 4);
        // 10s / 33.333ms = 300 ticks, and every tick rewrites all 16 tiles.
        assert_eq!(frames.len(), 300);
        assert!(frames.iter().all(|f| f.tiles.len() == 16));
    }

    #[test]
    fn region_churn_dirties_only_its_window_and_moves_it() {
        let p = LoadProfile {
            cadence_us: 33_333,
            churn: Churn::Region { tiles_per_tick: 3 },
        };
        let frames = p.frames_for(Duration::from_secs(1), 4, 4);
        assert!(frames.iter().all(|f| f.tiles.len() == 3));
        // The window must actually move: two consecutive ticks must not
        // dirty the same coordinates, or this models a static screen.
        let coords = |f: &FrameScript| f.tiles.iter().map(|(c, _)| *c).collect::<Vec<_>>();
        assert_ne!(coords(&frames[0]), coords(&frames[1]));
    }

    #[test]
    fn region_window_wraps_and_stays_in_bounds() {
        let p = LoadProfile {
            cadence_us: 1_000,
            churn: Churn::Region { tiles_per_tick: 3 },
        };
        // 200 ticks over a 2x2 grid forces many wraps.
        let frames = p.frames_for(Duration::from_millis(200), 2, 2);
        assert!(frames
            .iter()
            .flat_map(|f| f.tiles.iter())
            .all(|((x, y), _)| *x < 2 && *y < 2));
    }

    #[test]
    fn content_differs_between_ticks() {
        // A generator that emitted identical bytes every tick would produce
        // no real codec work and no backlog, quietly defeating every scene
        // built on it.
        let p = LoadProfile {
            cadence_us: 33_333,
            churn: Churn::FullGrid,
        };
        let frames = p.frames_for(Duration::from_millis(100), 1, 1);
        let bytes = |f: &FrameScript| match &f.tiles[0].1 {
            TileSpec::Cdf53 { bgra } => bgra.clone(),
            other => panic!("expected Cdf53, got {other:?}"),
        };
        assert_ne!(bytes(&frames[0]), bytes(&frames[1]));
    }
}
```

- [x] **Step 2: Run them to verify they fail**

```bash
cd /home/cedric/work/ghostframe
cargo test -p ghostframe-e2e --lib load_profile 2>&1 | tail -5
```

Expected: compile error, `cannot find type LoadProfile`.

- [x] **Step 3: Implement the generator**

Prepend to the same file, above the test module:

```rust
//! Generated, sustained scene load for the browserless harness.
//!
//! A scripted `BrowserlessScene` lists its frames explicitly, which is right
//! for small deterministic scenes and unworkable for the hundreds a
//! production-cadence scene needs. It also makes every such scene a short
//! burst followed by silence: the harness injects the script, runs out, and
//! falls back to empty heartbeats for the rest of the duration. Production's
//! capture loop free-runs instead, producing a frame every tick for as long
//! as a client is connected.
//!
//! That difference is not cosmetic. At production cadence the 8x8 CDF53 probe
//! scene completes zero probe clusters where the same scene at the harness's
//! old 16 ms cadence completed ten of ten — see
//! `docs/specs/bwe-probe-emission-timing.md`.

use std::time::Duration;

use crate::harness::browserless::FrameScript;
use crate::harness::scene_tiles::TileSpec;

/// Production's capture-to-dispatch interval: `IoBridge`'s
/// `SCHEDULER_TICK_INTERVAL_US`. A scene that injects faster than this gives
/// probe windows more emission opportunities than production ever would.
pub const PRODUCTION_CADENCE_US: u64 = 33_333;

/// How much of the screen changes on each tick.
#[derive(Clone, Debug)]
pub enum Churn {
    /// Every tile rewritten each tick. The heaviest offered load available,
    /// and the shape the existing `busy_frames_grid` helper produces.
    FullGrid,
    /// A damage window of `tiles_per_tick` tiles that walks the grid in
    /// row-major order, wrapping. Closer to what a real desktop produces — a
    /// dragged window or a scrolling pane dirties a region, not the whole
    /// screen — which changes both byte volume and which tiles reach the
    /// refinement queue.
    Region { tiles_per_tick: usize },
}

/// Sustained load for a scene's whole duration.
#[derive(Clone, Debug)]
pub struct LoadProfile {
    pub cadence_us: u64,
    pub churn: Churn,
}

impl LoadProfile {
    /// Generate one `FrameScript` per tick for `duration`.
    ///
    /// Tile content varies with the tick index so every frame carries real,
    /// distinct CDF53 bit-planes. A generator emitting identical bytes would
    /// produce no backlog and silently defeat any scene built on it.
    pub fn frames_for(&self, duration: Duration, cols: u8, rows: u8) -> Vec<FrameScript> {
        let ticks = (duration.as_micros() as u64 / self.cadence_us) as usize;
        let total = cols as usize * rows as usize;
        (0..ticks)
            .map(|i| {
                let coords: Vec<(u8, u8)> = match &self.churn {
                    Churn::FullGrid => (0..cols)
                        .flat_map(|x| (0..rows).map(move |y| (x, y)))
                        .collect(),
                    Churn::Region { tiles_per_tick } => {
                        let n = (*tiles_per_tick).min(total);
                        (0..n)
                            .map(|k| {
                                let idx = (i * n + k) % total;
                                ((idx % cols as usize) as u8, (idx / cols as usize) as u8)
                            })
                            .collect()
                    }
                };
                FrameScript {
                    tiles: coords
                        .into_iter()
                        .map(|(x, y)| {
                            (
                                (x, y),
                                TileSpec::Cdf53 {
                                    bgra: gradient_tile(i as u32, x, y),
                                },
                            )
                        })
                        .collect(),
                }
            })
            .collect()
    }
}

/// A 32x32 BGRA gradient whose content shifts with `shift`, so consecutive
/// ticks carry genuinely different bit-planes.
pub fn gradient_tile(shift: u32, tile_x: u8, tile_y: u8) -> Vec<u8> {
    let off = shift
        .wrapping_add(tile_x as u32 * 17)
        .wrapping_add(tile_y as u32 * 31);
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = (((x * 8) + off) % 256) as u8;
            let g = (((y * 8) + off * 3) % 256) as u8;
            let r = ((((x + y) * 4) + off * 5) % 256) as u8;
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    bgra
}
```

The design also lists a `Churn::Idle`. It is deliberately **not** implemented:
no scene in this plan needs it, `Region { tiles_per_tick: 1 }` covers
near-idle load, and an unused variant is a maintenance cost with no caller.
Add it when something asks for it.

Register it in `ghostframe-e2e/src/harness/mod.rs` by adding, alongside the existing `pub mod` lines:

```rust
pub mod load_profile;
```

- [x] **Step 4: Run the tests**

```bash
cargo test -p ghostframe-e2e --lib load_profile 2>&1 | grep 'test result'
```

Expected: `test result: ok. 4 passed`.

- [x] **Step 5: Measure generation cost before anything depends on it**

The design flags runtime as the main risk: a 10 s `FullGrid` scene over an 8x8 grid is 19,200 tile encodes per seed.

Time generation directly by adding this temporary test, running it, and deleting it:

```rust
#[test]
fn generation_cost_is_tolerable() {
    let t = std::time::Instant::now();
    let p = LoadProfile { cadence_us: 33_333, churn: Churn::FullGrid };
    let f = p.frames_for(Duration::from_secs(10), 8, 8);
    eprintln!("generated {} frames in {:?}", f.len(), t.elapsed());
}
```

Run: `cargo test -p ghostframe-e2e --lib generation_cost -- --nocapture 2>&1 | grep generated`

**Record the number.** If generation alone exceeds ~2 s, report it and stop — the convergence scenes will need `Region` churn or a smaller grid, and the later tasks' scene shapes need revising before they are written.

- [x] **Step 6: Commit**

```bash
git add ghostframe-e2e/src/harness/load_profile.rs ghostframe-e2e/src/harness/mod.rs
git commit -m "feat(e2e): generated sustained load for browserless scenes

A scripted scene lists frames explicitly and then goes quiet, falling back
to empty heartbeats for the rest of its duration. Production's capture loop
free-runs. LoadProfile generates one frame per tick for the whole duration,
with FullGrid and Region churn; Region walks a damage window across the
grid, which is closer to what a desktop actually dirties."
```

## Task 2: `SceneLoad` — keep scripted scenes, admit generated ones

**Files:**
- Modify: `ghostframe-e2e/src/harness/browserless.rs`
- Modify: `ghostframe-e2e/tests/browserless_runner.rs`

- [x] **Step 1: Add the enum and switch the field**

In `browserless.rs`, above `BrowserlessScene`:

```rust
/// Where a scene's frames come from.
pub enum SceneLoad {
    /// An explicit list, drained in order. Right for small deterministic
    /// scenes that assert on specific pixels.
    Script(Vec<FrameScript>),
    /// Generated across the scene's whole duration, so the scene never goes
    /// quiet. Right for anything measuring bandwidth, pacing, or probing.
    Profile(crate::harness::load_profile::LoadProfile),
}
```

Change `BrowserlessScene.frames` from `Vec<FrameScript>` to `pub load: SceneLoad`.

- [x] **Step 2: Resolve it once, at the top of `run_browserless`**

Find where `scene.frames` is first used and resolve the load into a concrete `Vec<FrameScript>` before the loop, so the loop body is unchanged:

```rust
let frames: Vec<FrameScript> = match &scene.load {
    SceneLoad::Script(f) => f.clone(),
    SceneLoad::Profile(p) => p.frames_for(
        scene.duration,
        scene.grid_cols as u8,
        scene.grid_rows as u8,
    ),
};
```

Replace every later `scene.frames` with `frames`. Check them all:

```bash
grep -n 'scene\.frames' ghostframe-e2e/src/harness/browserless.rs
```

Expected after the edit: no matches.

- [x] **Step 3: Migrate the existing scenes**

Every scene in `tests/browserless_runner.rs` changes `frames: X,` to `load: SceneLoad::Script(X),`. There are 14. Add the import:

```rust
use ghostframe_e2e::harness::browserless::SceneLoad;
```

- [x] **Step 4: Verify nothing moved**

```bash
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep 'test result'
```

Expected: `14 passed; 0 failed; 1 ignored`. **This task is behaviour-preserving.** A scene that moves means the resolution in Step 2 is wrong, not that anything improved.

- [x] **Step 5: Commit**

```bash
git add -A && git commit -m "refactor(e2e): scene load is scripted or generated

BrowserlessScene.frames becomes SceneLoad::{Script,Profile}. Every existing
scene keeps Script and is untouched; resolution happens once at the top of
run_browserless so the scene loop is unchanged."
```

## Task 3: Per-scene cadence, defaulting to production's 33.3 ms

**Files:**
- Modify: `ghostframe-e2e/src/harness/browserless.rs`
- Modify: `ghostframe-e2e/tests/browserless_runner.rs`

The harness injects every 16 ms; production dispatches every 33.3 ms. Measured blast radius of the change across the whole suite: **exactly 2 scenes of 15**, both probe scenes, both of which Task 6 rewrites.

- [x] **Step 1: Make cadence a scene field**

Replace the module constant:

```rust
/// Default injection cadence.
///
/// This was 16 ms, chosen before anything depended on it matching
/// production. It does matter: at 16 ms a 15 ms probe window nearly always
/// contains an injection, at 33.3 ms it contains one less than half the
/// time, and the 8x8 CDF53 probe scene goes from ten completions in ten
/// seeds to zero. Scenes that want the old cadence must now say so.
///
/// Re-exported rather than redeclared: `load_profile` owns the number, so a
/// scripted scene and a generated one cannot drift to different defaults.
pub use crate::harness::load_profile::PRODUCTION_CADENCE_US as DEFAULT_CADENCE_US;
```

Add to `BrowserlessScene`:

```rust
/// Interval between injections. Defaults to `DEFAULT_CADENCE_US`.
pub cadence_us: u64,
```

For a `SceneLoad::Profile`, the profile's own `cadence_us` is authoritative; resolve as:

```rust
let cadence_us = match &scene.load {
    SceneLoad::Profile(p) => p.cadence_us,
    SceneLoad::Script(_) => scene.cadence_us,
};
```

Replace the three `FRAME_SPACING_US` uses (the injection-spacing assignment and two `timestamp_us` computations — find them with `grep -n FRAME_SPACING_US`) with `cadence_us`.

- [x] **Step 2: Set it on every existing scene**

Add `cadence_us: DEFAULT_CADENCE_US,` to all 14 scenes.

- [x] **Step 3: Run the suite and expect exactly two failures**

```bash
cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep -E '^test |test result'
```

Expected: `probe_windows_can_complete_on_a_busy_link` FAILS (0 completions at production cadence — the finding, not a defect). The other 13 pass. If anything else moves, **stop and report**: the measured blast radius was two scenes, and a third means this task changed something it should not have.

- [x] **Step 4: Commit**

```bash
git add -A && git commit -m "feat(e2e): injection cadence is per-scene, defaulting to 33.3ms

The harness injected every 16ms where production dispatches every 33.3ms,
so every probe-completion number this project has published came from a
regime ~2x more favourable than production. Measured blast radius: 2 scenes
of 15, both probe scenes, both rewritten in the next commits."
```

## Task 4: Sample the bandwidth estimate over time

**Files:**
- Modify: `ghostframe-e2e/src/harness/browserless.rs`

A step-up test needs the estimate before and after the step, not just at the end.

- [x] **Step 1: Add the field**

```rust
/// `(virtual_us, bitrate_bps)` sampled at most every 100 ms of virtual
/// time. The final entry is the same value `bwe_estimate_bps` reports; the
/// series exists so a test can assert the estimate *tracks* a changing link
/// rather than merely ending near it.
pub bwe_estimate_samples: Vec<(u64, u64)>,
```

- [x] **Step 2: Sample in the scene loop**

`bwe_cell` is already cloned before the bridge is moved (`browserless.rs` ~296) and `run()` republishes into it every iteration, so the loop can read it live. Pass the clone into `drive_session`, and near the top of the loop:

```rust
let vt = now_us(base);
if vt >= next_bwe_sample_us {
    let bps = bwe_cell.lock().expect("bwe_publish mutex poisoned").bitrate_bps;
    bwe_samples.push((vt, bps));
    next_bwe_sample_us = vt + 100_000;
}
```

declaring before the loop:

```rust
let mut bwe_samples: Vec<(u64, u64)> = Vec::new();
let mut next_bwe_sample_us: u64 = 0;
```

Return `bwe_samples` alongside the existing tuple and populate the new field.

- [x] **Step 3: Check whether a generated scene still needs heartbeats**

The design flags this as something to verify rather than assume. Heartbeats
exist so `sweep_rto_retransmits` keeps being called once a script runs out;
a `Profile` scene never runs out, so they may be dead weight there. Confirm
which path a generated scene takes:

```bash
grep -n 'inject_heartbeat' ghostframe-e2e/src/harness/browserless.rs
```

If `next_frame_idx < frames.len()` holds for the whole scene, heartbeats
never fire and nothing needs changing — **record that and move on**. Do not
remove the heartbeat path: scripted scenes still depend on it.

- [x] **Step 4: Verify the series is actually populated**

```bash
cargo test -p ghostframe-e2e --test browserless_runner bytes_actually_cross -- --test-threads=1 2>&1 | grep 'test result'
```

Then add a temporary assertion to that test — `assert!(!r.bwe_estimate_samples.is_empty())` — run it, confirm it passes, and remove it. An empty series that nobody checks is exactly the kind of instrumentation this project has shipped before.

- [x] **Step 5: Commit**

```bash
git add -A && git commit -m "feat(e2e): sample the bandwidth estimate over scene time

A single end-of-scene estimate cannot show whether the estimator tracked a
changing link or merely ended near the right number. Samples every 100ms of
virtual time from the cell the bridge already republishes."
```

## Task 5: Convergence against a known cap

**Files:**
- Modify: `ghostframe-e2e/tests/browserless_runner.rs`

- [x] **Step 1: Measure before asserting**

Write the scene with a diagnostic print and **no assertion yet**:

```rust
#[tokio::test(start_paused = true)]
async fn estimate_converges_toward_a_known_cap() {
    const CAP_BPS: u64 = 8_000_000;
    let scene = BrowserlessScene {
        seed: 0xC0FFEE01,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::FullGrid,
        }),
        net: NetProfile {
            delay_us: 20_000,
            cap: CapTimeline::constant(CAP_BPS),
            ..NetProfile::perfect()
        },
        duration: Duration::from_secs(10),
        cadence_us: PRODUCTION_CADENCE_US,
        grid_cols: 8,
        grid_rows: 8,
    };
    let r = run_browserless(scene).await.expect("scene ran");
    println!(
        "cap={CAP_BPS} final={} samples={:?}",
        r.bwe_estimate_bps, r.bwe_estimate_samples
    );
}
```

Run it several times:

```bash
for i in 1 2 3 4 5; do
  cargo test -p ghostframe-e2e --test browserless_runner estimate_converges -- \
    --test-threads=1 --nocapture 2>&1 | grep -E 'cap=|test result'
done
```

**Record the spread.** The design requires the tolerance be justified by measurement, not chosen to make the test pass.

- [x] **Step 2: Decide, and state the discriminating power**

Set the tolerance from the measured spread, then write the assertion with a comment saying **what regression it would catch**. A tolerance so wide that a broken estimator would still pass is worse than no test:

```rust
    // Tolerance from the measured spread across 5 runs (record the numbers
    // here). An estimator that ignored ACK feedback entirely would report
    // its seed rate and fall outside this band, which is the regression
    // this guards.
    assert!(
        r.bwe_estimate_bps > CAP_BPS / 4 && r.bwe_estimate_bps < CAP_BPS * 3,
        "estimate {} should be within a factor of the {CAP_BPS} cap; \
         samples={:?}",
        r.bwe_estimate_bps,
        r.bwe_estimate_samples
    );
```

Replace the placeholder bounds above with the measured ones.

- [x] **Step 3: Verify it discriminates**

Temporarily halve `CAP_BPS` in the `NetProfile` only (leaving the assertion's `CAP_BPS`), re-run, and confirm the test **fails**. Restore. A convergence assertion that passes against the wrong cap is measuring nothing.

- [x] **Step 4: Commit**

```bash
git add -A && git commit -m "test(e2e): the estimate converges toward a known cap

Offered load above a CapTimeline cap, with the tolerance taken from the
measured spread rather than chosen to pass, and verified to fail against a
deliberately wrong cap."
```

## Task 6: Step-up, and retiring the probe-counter assertions

**Files:**
- Modify: `ghostframe-e2e/tests/browserless_runner.rs`

- [x] **Step 1: Write the step-up scene**

```rust
/// The scenario probing exists for: capacity increases mid-session, and the
/// estimator has to find the new headroom. No test covered this before.
#[tokio::test(start_paused = true)]
async fn estimate_follows_a_mid_scene_capacity_step_up() {
    const LOW_BPS: u64 = 4_000_000;
    const HIGH_BPS: u64 = 12_000_000;
    const STEP_AT_US: u64 = 5_000_000;
    let scene = BrowserlessScene {
        seed: 0xC0FFEE02,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::FullGrid,
        }),
        net: NetProfile {
            delay_us: 20_000,
            cap: CapTimeline::step(LOW_BPS, STEP_AT_US, HIGH_BPS),
            ..NetProfile::perfect()
        },
        duration: Duration::from_secs(10),
        cadence_us: PRODUCTION_CADENCE_US,
        grid_cols: 8,
        grid_rows: 8,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    let before: Vec<u64> = r.bwe_estimate_samples.iter()
        .filter(|(t, _)| *t < STEP_AT_US).map(|(_, b)| *b).collect();
    let after: Vec<u64> = r.bwe_estimate_samples.iter()
        .filter(|(t, _)| *t > STEP_AT_US + 2_000_000).map(|(_, b)| *b).collect();
    println!("before={before:?}\nafter={after:?} probes {}/{}",
             r.probes_completed, r.probes_abandoned);

    let med = |mut v: Vec<u64>| { v.sort_unstable(); v[v.len() / 2] };
    assert!(!before.is_empty() && !after.is_empty(), "need samples on both sides");
    assert!(
        med(after.clone()) > med(before.clone()),
        "the estimate must rise after capacity triples: before(median)={} \
         after(median)={}; if it does not, the session never discovered the \
         new headroom — which is exactly what probing is for",
        med(before), med(after)
    );
}
```

- [x] **Step 2: Run it, and report the answer either way**

```bash
for i in 1 2 3; do
  cargo test -p ghostframe-e2e --test browserless_runner step_up -- \
    --test-threads=1 --nocapture 2>&1 | grep -E 'before=|after=|test result'
done
```

**This is the experiment, not just a test.** If the estimate rises, probe abandonment is benign — the session finds headroom without completing clusters, which answers the question `docs/specs/bwe-probe-emission-timing.md` leaves open. If it does not rise, that is a real production finding about goog_cc's ramp in this system. **Report whichever happens; do not tune the scene until it passes.**

- [x] **Step 3: Rework the two probe scenes**

`probe_windows_can_complete_on_a_busy_link` asserts `probes_completed >= 1`, which is false at production cadence. Replace its assertion with the diagnostic print plus a comment pointing at the spec's cadence table, and rename it to `probe_windows_are_observed_on_a_busy_link`.

Keep `probe_windows_are_abandoned_on_a_demand_starved_link` and its `probes_completed == 0` assertion: it guards against padding creeping in, which is still a real invariant, and it passes at production cadence.

- [x] **Step 4: Full suite, repeatedly**

```bash
for i in 1 2 3 4 5; do
  cargo test -p ghostframe-e2e --test browserless_runner -- --test-threads=1 2>&1 | grep 'test result'
done
cargo clippy -p ghostframe-e2e --all-targets 2>&1 | grep -E '^(error|warning)' | head
cargo fmt -p ghostframe-e2e
```

Expected: all green, five times. Local green is weak evidence for this harness — CI is the arbiter.

- [x] **Step 5: Commit**

```bash
git add -A && git commit -m "test(e2e): the estimate must follow a mid-scene capacity step-up

The scenario probing exists for, which no test covered. Probe counters
become printed diagnostics: probe_windows_can_complete_on_a_busy_link
asserted completions that are simply false at production cadence, while the
demand-starved scene keeps its completed==0 assertion, which still guards
against padding creeping in."
```

## Task 7: Record what the step-up answered

**Files:**
- Modify: `docs/specs/bwe-probe-emission-timing.md`

- [x] **Step 1: Append the result**

Add a dated section giving the step-up numbers and stating plainly whether the estimate tracked capacity while probes were abandoned. If it did, say that abandonment is benign in this system and that the drain fix's value is therefore bounded. If it did not, say that and name it as a production concern.

- [x] **Step 2: Commit and open the PR**

---

## Done criteria

- [x] `LoadProfile::frames_for` covers a scene's whole duration; `Region` churn moves and stays in bounds.
- [x] All 14 existing scenes pass unchanged on `SceneLoad::Script`.
- [x] Cadence defaults to 33 333 µs; exactly the two probe scenes needed rework.
- [x] `bwe_estimate_samples` is populated and verified non-empty.
- [x] Convergence tolerance derived from measurement, with its discriminating power stated and verified by a deliberately wrong cap.
- [x] Step-up result reported either way, and written into the spec.
- [x] Suite green across five consecutive runs; clippy and fmt clean.

## What this plan does not do

- Touch the capture path. `process_frame_cpu` emits `Codec::Raw` only, so feeding pixels would destroy the Critical/Refinement structure these scenes measure. See the design's constraints section.
- Add impairment to the Docker e2e harness. That is the separate follow-up.
- Add an independent pacing clock. Real gap, but not the cause of anything measured here.
- Change `NetSim`. `CapTimeline::step` already exists.
