# Bandwidth-fit frame-mode switch Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** The frame-mode classifier switches to H.264 only when the tile
codec's first pass does not fit the bandwidth estimate, and leaves H.264 as
soon as the estimate is known and the first pass fits.

**Architecture:** A new pure module `tile/first_pass.rs` prices tiles at
calibrated first-pass sizes and keeps a 1 s demand window. `Classifier`
replaces its time-based `cost_comparison` with a share-of-estimate rule plus
hysteresis, an `estimate_known` gate and a repaint-drain gate. `IoBridge`
feeds it: `estimate_known` from the BWE snapshot, the repaint size from the
metrics tracker, calibration from the emit loop, and repaint notifications.

**Tech Stack:** Rust (ghostframe-lib), tracing, cargo test, `just` recipes.

**Spec:** `docs/superpowers/specs/2026-11-07-bandwidth-fit-mode-switch-design.md`.
Read it first.

**Branch:** `classifier/bandwidth-fit-mode-switch` (exists, holds the spec).

## Ground rules for the implementer

- Read `AGENTS.md` "Traps" and "Testing habits that pay here" before starting.
- The gate is `cargo clippy --workspace --all-targets -- -D warnings`, not a
  test summary: a test file that fails to compile vanishes from `cargo test`.
- Never `git add -A`; stage the paths each task lists.
- Every new test is checked by mutating the code it guards and watching it fail
  (each task says which mutation). Revert the mutation before committing.
- Lib tests: `cargo test -p ghostframe-lib --lib tile::` (fast). Full lib:
  `just test-unit`.

## File map

| File | Change |
|---|---|
| `ghostframe-lib/src/tile/first_pass.rs` | **new**: `FirstPassKind`, `FirstPassModel`, `DemandWindow`, `repaint_state` |
| `ghostframe-lib/src/tile/first_pass_tests.rs` | **new**: unit tests for the above |
| `ghostframe-lib/src/tile/mod.rs` | declare `first_pass`, drop `CostModel` re-export |
| `ghostframe-lib/src/tile/classifier.rs` | bandwidth rule replaces cost path; `CostModel`, refinement bias, motion fast path, factors removed |
| `ghostframe-lib/src/tile/classifier_cost_tests.rs` | **deleted** (its subject is gone) |
| `ghostframe-lib/src/tile/classifier_decide_tests.rs` | rewritten cost-path tests, new `bandwidth_fit` module |
| `ghostframe-lib/src/config.rs` | `CostModelConfig` and `refinement_bias_us` removed with a one-time warning; `enter_share` / `exit_share` added |
| `ghostframe-lib/src/transport/io_bridge/bwe_glue.rs` | `estimate_known` |
| `ghostframe-lib/src/transport/io_bridge.rs` | field `bwe_samples_at_session_reset` |
| `ghostframe-lib/src/transport/io_bridge/session.rs` | baseline the samples, `note_repaint`, comment |
| `ghostframe-lib/src/transport/io_bridge/frame.rs` | repaint size, screen tiles, `note_repaint` on handoff, drop deficit setter |
| `ghostframe-lib/src/transport/io_bridge/pacing.rs` | `note_repaint` on stuck-entry re-dirty |
| `ghostframe-lib/src/transport/io_bridge/emit.rs` | calibration observations |
| `ghostframe-lib/src/transport/io_bridge/tests.rs` | two classifier tests adapted, one new bridge test |
| `ghostframe-e2e/tests/browserless_h264.rs` | `guard_release` sets `estimate_known` |
| `ghostframe-e2e/tests/e2e.rs`, `ghostframe-e2e/src/harness/scene.rs`, `ghostframe-bench/src/bin/codec_report/{report,aggregate,bandwidth_matrix,loss_axis}.rs` | reason names |
| `docs/design/h264-transport.md`, `AGENTS.md` | reason names, removed variables |

---

### Task 1: `first_pass` module

**Files:**
- Create: `ghostframe-lib/src/tile/first_pass.rs`
- Create: `ghostframe-lib/src/tile/first_pass_tests.rs`
- Modify: `ghostframe-lib/src/tile/mod.rs` (add `pub mod first_pass;` next to `pub mod classifier;`)

- [ ] **Step 1: Write the failing tests** in `first_pass_tests.rs`:

```rust
use super::*;
use crate::tile::{CodecState, TileMetrics, UNIQUE_COLORS_UNKNOWN};

const CDF53: CodecState = CodecState::Cdf53 {
    passes_sent: 0,
    present_passes: 0x3FFF,
};

#[test]
fn the_priors_price_each_state_until_something_is_observed() {
    let m = FirstPassModel::default();
    assert_eq!(m.state_bytes(CodecState::Skip), 0.0);
    assert_eq!(m.state_bytes(CodecState::PixelPerfect), 0.0);
    assert_eq!(m.state_bytes(CodecState::Solid), PRIOR_BYTES[0]);
    assert_eq!(m.state_bytes(CodecState::PalRle { palette_id: 3 }), PRIOR_BYTES[1]);
    assert_eq!(m.state_bytes(CDF53), PRIOR_BYTES[2]);
    // An H264-classified tile goes out as CDF 5/3 in tile mode.
    assert_eq!(
        m.state_bytes(CodecState::H264 { frames_in_h264: 4 }),
        PRIOR_BYTES[2]
    );
}

#[test]
fn calibration_converges_to_the_measured_size_per_tile() {
    let mut m = FirstPassModel::default();
    // 40 tiles of 500 B each, batch after batch.
    for _ in 0..200 {
        m.observe(FirstPassKind::Cdf53, 40 * 500, 40);
    }
    let got = m.state_bytes(CDF53);
    assert!((got - 500.0).abs() < 5.0, "{got}");
    // Other kinds are untouched.
    assert_eq!(m.state_bytes(CodecState::Solid), PRIOR_BYTES[0]);
}

#[test]
fn passes_split_across_batches_still_average_per_tile() {
    // A tile's critical passes may go out in a later batch than its pass 0:
    // bytes with no new tile, then a tile with fewer bytes. The ratio of the
    // two averages is still bytes per tile.
    let mut m = FirstPassModel::default();
    for _ in 0..400 {
        m.observe(FirstPassKind::Cdf53, 600, 1);
        m.observe(FirstPassKind::Cdf53, 400, 0);
    }
    // An EWMA of an alternating input oscillates around its mean; ending on
    // the low half leaves the ratio about 3 % high.
    let got = m.state_bytes(CDF53);
    assert!((got - 1000.0).abs() < 50.0, "{got}");
}

#[test]
fn an_empty_observation_changes_nothing() {
    let mut m = FirstPassModel::default();
    m.observe(FirstPassKind::PalRle, 0, 0);
    assert_eq!(m.state_bytes(CodecState::PalRle { palette_id: 0 }), PRIOR_BYTES[1]);
}

#[test]
fn the_window_is_bytes_per_second_over_the_last_second() {
    let mut w = DemandWindow::default();
    // 30 frames of 1000 B over one second: 30 kB/s = 240 kbit/s.
    let mut rate = 0;
    for i in 0..30u64 {
        rate = w.push(i * 33_333, 1000.0);
    }
    assert_eq!(rate, 240_000);
    // Two seconds later with nothing new, the old frames have left.
    assert_eq!(w.push(3_000_000, 0.0), 0);
}

#[test]
fn the_window_forgets_frames_older_than_one_second() {
    let mut w = DemandWindow::default();
    w.push(0, 10_000.0);
    // Exactly one window later the first frame is out.
    assert_eq!(w.push(DEMAND_WINDOW_US, 1000.0), 8_000);
}

#[test]
fn repaint_state_follows_the_colour_count() {
    let mut t = TileMetrics::default();
    t.unique_colors = 1;
    assert_eq!(repaint_state(&t), CodecState::Solid);
    t.unique_colors = 16;
    assert!(matches!(repaint_state(&t), CodecState::PalRle { .. }));
    t.unique_colors = 17;
    assert!(matches!(repaint_state(&t), CodecState::Cdf53 { .. }));
    t.unique_colors = UNIQUE_COLORS_UNKNOWN;
    assert!(matches!(repaint_state(&t), CodecState::Cdf53 { .. }));
}
```

Before writing it, check `TileMetrics` implements `Default` and has a public
`unique_colors` field (`grep -n 'pub struct TileMetrics' -A20
ghostframe-lib/src/tile/mod.rs`). If it has no `Default`, build it the way
`classifier_classify_tests.rs` builds one and use that helper instead.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ghostframe-lib --lib tile::first_pass`
Expected: compile error, `first_pass` has no items.

- [ ] **Step 3: Write `first_pass.rs`**

```rust
//! First-pass demand: what the tile codec's first pass costs on the wire.
//!
//! "First pass" is the critical tier: the only pass of a Solid or PalRle
//! tile, passes `0..=CRITICAL_PASS_MAX` of a CDF 5/3 tile (the bytes the
//! bridge counts as `bytes_emitted_critical`). The frame-mode classifier
//! compares its rate to the bandwidth estimate (design
//! `docs/superpowers/specs/2026-11-07-bandwidth-fit-mode-switch-design.md`).

use super::{CodecState, TileMetrics, UNIQUE_COLORS_UNKNOWN};
use std::collections::VecDeque;

/// The per-tile size a state is priced at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirstPassKind {
    Solid,
    PalRle,
    Cdf53,
}

impl FirstPassKind {
    /// `None` for a state that sends nothing. An H264-classified tile is
    /// priced as CDF 5/3: that is what it would be sent as in tile mode.
    pub fn of(state: CodecState) -> Option<Self> {
        match state {
            CodecState::Skip | CodecState::PixelPerfect => None,
            CodecState::Solid => Some(Self::Solid),
            CodecState::PalRle { .. } => Some(Self::PalRle),
            CodecState::Cdf53 { .. } | CodecState::H264 { .. } => Some(Self::Cdf53),
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Solid => 0,
            Self::PalRle => 1,
            Self::Cdf53 => 2,
        }
    }
}

/// Starting per-tile sizes (Solid, PalRle, Cdf53), bytes: the old
/// `CostModel::estimated_tile_bytes`. Calibration replaces them.
pub(crate) const PRIOR_BYTES: [f64; 3] = [4.0, 200.0, 1300.0];
/// EWMA weight of one observed emission batch.
pub(crate) const CALIBRATION_WEIGHT: f64 = 1.0 / 16.0;

/// Per-kind first-pass bytes per tile, calibrated from what tile mode sends.
///
/// Two averages per kind, of bytes and of new tiles per batch; their ratio is
/// bytes per tile even when a tile's critical passes straddle batches. Both
/// start at (prior, 1), so the prior holds until data arrives.
#[derive(Debug, Clone, Copy)]
pub struct FirstPassModel {
    bytes: [f64; 3],
    tiles: [f64; 3],
}

impl Default for FirstPassModel {
    fn default() -> Self {
        Self {
            bytes: PRIOR_BYTES,
            tiles: [1.0; 3],
        }
    }
}

impl FirstPassModel {
    /// Bytes one tile in `state` costs on its first pass.
    pub fn state_bytes(&self, state: CodecState) -> f64 {
        FirstPassKind::of(state).map_or(0.0, |k| self.tile_bytes(k))
    }

    fn tile_bytes(&self, kind: FirstPassKind) -> f64 {
        let i = kind.index();
        if self.tiles[i] < 1e-6 {
            return PRIOR_BYTES[i];
        }
        self.bytes[i] / self.tiles[i]
    }

    /// One emission batch's first-pass wire bytes of `kind` (no
    /// retransmissions) and how many of them were a tile's pass 0.
    pub fn observe(&mut self, kind: FirstPassKind, bytes: u64, tiles: u32) {
        if bytes == 0 && tiles == 0 {
            return;
        }
        let i = kind.index();
        self.bytes[i] += CALIBRATION_WEIGHT * (bytes as f64 - self.bytes[i]);
        self.tiles[i] += CALIBRATION_WEIGHT * (f64::from(tiles) - self.tiles[i]);
    }
}

/// Length of the demand window, µs.
pub(crate) const DEMAND_WINDOW_US: u64 = 1_000_000;

/// First-pass bytes of each frame over the last [`DEMAND_WINDOW_US`].
/// At most one entry per frame, so it stays small (about 60 at 60 fps).
#[derive(Debug, Clone, Default)]
pub struct DemandWindow {
    frames: VecDeque<(u64, f64)>,
    sum: f64,
}

impl DemandWindow {
    /// Add a frame's bytes at `now_us`, drop frames a full window old, and
    /// return the demand in bit/s.
    pub fn push(&mut self, now_us: u64, bytes: f64) -> u64 {
        self.frames.push_back((now_us, bytes));
        self.sum += bytes;
        while let Some(&(t, b)) = self.frames.front() {
            if now_us.saturating_sub(t) < DEMAND_WINDOW_US {
                break;
            }
            self.frames.pop_front();
            self.sum -= b;
        }
        if self.frames.is_empty() {
            self.sum = 0.0;
        }
        (self.sum.max(0.0) * 8.0 * 1e6 / DEMAND_WINDOW_US as f64) as u64
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.sum = 0.0;
    }
}

/// The state a tile would be repainted in, from its last colour count: the
/// classifier's own colour rules (Rules 6-8 of `classify_tile`) without the
/// motion and idle rules, since a repaint sends every tile once.
pub fn repaint_state(metrics: &TileMetrics) -> CodecState {
    let uc = metrics.unique_colors;
    if uc != UNIQUE_COLORS_UNKNOWN && uc <= 1 {
        CodecState::Solid
    } else if uc != UNIQUE_COLORS_UNKNOWN && uc <= 16 {
        CodecState::PalRle { palette_id: 0 }
    } else {
        CodecState::Cdf53 {
            passes_sent: 0,
            present_passes: 0x3FFF,
        }
    }
}

#[cfg(test)]
#[path = "first_pass_tests.rs"]
mod tests;
```

Note: `the_window_forgets_frames_older_than_one_second` pushes 10 000 B at 0
and 1000 B at `DEMAND_WINDOW_US`; with the `< DEMAND_WINDOW_US` keep rule the
first leaves, leaving 1000 B = 8000 bit/s. If the test reads 88 000, the keep
rule is `<=`; fix the code, not the test.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p ghostframe-lib --lib tile::first_pass`
Expected: 7 passed.

- [ ] **Step 5: Mutation check.** Change `CodecState::H264 { .. } => Some(Self::Cdf53)`
to `None` (move it to the `Skip` arm): the first test must fail. Change the
calibration weight to `0.0`: `calibration_converges...` must fail. Revert both.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/tile/first_pass.rs ghostframe-lib/src/tile/first_pass_tests.rs ghostframe-lib/src/tile/mod.rs
git commit -m "feat(tile): first-pass demand model and window"
```

---

### Task 2: The classifier decides on bandwidth

This is the core change. It removes the time-based cost model, so it also
rewrites the tests that exercised it, in the same commit (the tree must
compile and pass at every commit).

**Files:**
- Modify: `ghostframe-lib/src/tile/classifier.rs`
- Modify: `ghostframe-lib/src/tile/mod.rs:131` (`pub use classifier::{Classifier, CostModel};` → `pub use classifier::Classifier;`)
- Delete: `ghostframe-lib/src/tile/classifier_cost_tests.rs`
- Modify: `ghostframe-lib/src/tile/classifier_decide_tests.rs`
- Modify: `ghostframe-lib/src/config.rs` (only what this task needs to compile: see Step 5)

- [ ] **Step 1: Write the new tests first.** Append to
`classifier_decide_tests.rs`:

```rust
/// The bandwidth rule (design 2026-11-07): H.264 only when the first pass
/// does not fit the estimate.
mod bandwidth_fit {
    use super::*;

    const FRAME_US: u64 = 33_333; // 30 fps
    const CDF53: CodecState = CodecState::Cdf53 {
        passes_sent: 0,
        present_passes: 0x3FFF,
    };

    fn ctx(estimate_bps: u64) -> AdaptationContext {
        AdaptationContext {
            supports_h264: true,
            bwe_estimate_bps: estimate_bps,
            estimate_known: true,
            smoothed_rtt_us: 1_000.0,
            ..Default::default()
        }
    }

    /// `n` CDF 5/3 tiles at the 1300 B prior: `n * 1300 * 8 * 30` bit/s.
    fn tiles(n: usize) -> Vec<CodecState> {
        vec![CDF53; n]
    }

    fn classifier(estimate_bps: u64) -> Classifier {
        let mut c = Classifier::new(ClassifierConfig::default());
        c.set_adaptation_context(ctx(estimate_bps));
        c.set_screen(2040, 0);
        c
    }

    /// Run frames from `from_us` for `dur_us`, returning the last decision
    /// and every transition `(time, to, reason)`.
    fn drive(
        c: &mut Classifier,
        from_us: u64,
        dur_us: u64,
        t: &[CodecState],
        prev: &mut FrameMode,
        flips: &mut Vec<(u64, FrameMode, &'static str)>,
    ) {
        let mut now = from_us;
        while now < from_us + dur_us {
            let (m, r) = c.decide_inner(now, t, *prev);
            if m != *prev {
                flips.push((now, m, r));
            }
            *prev = m;
            now += FRAME_US;
        }
    }

    #[test]
    fn a_first_pass_over_the_enter_share_enters_h264_after_the_hold() {
        // 10 Mbit/s estimate; 40 tiles = 12.5 Mbit/s > 0.7 x 10.
        let mut c = classifier(10_000_000);
        let mut prev = FrameMode::TileCodec;
        let mut flips = Vec::new();
        drive(&mut c, 0, 2_000_000, &tiles(40), &mut prev, &mut flips);
        assert_eq!(flips.len(), 1, "{flips:?}");
        let (at, to, reason) = flips[0];
        assert_eq!((to, reason), (FrameMode::H264, "bandwidth_short"));
        assert!(at >= 500_000, "entered before the hold: {at}");
    }

    #[test]
    fn a_first_pass_under_the_enter_share_stays_on_tiles() {
        // 100 Mbit/s; the same 40 tiles are 12.5 % of it.
        let mut c = classifier(100_000_000);
        let mut prev = FrameMode::TileCodec;
        let mut flips = Vec::new();
        drive(&mut c, 0, 5_000_000, &tiles(40), &mut prev, &mut flips);
        assert!(flips.is_empty(), "{flips:?}");
    }

    #[test]
    fn a_first_pass_under_the_exit_share_leaves_h264_after_the_hold() {
        let mut c = classifier(100_000_000);
        let mut prev = FrameMode::H264;
        let mut flips = Vec::new();
        drive(&mut c, 0, 3_000_000, &tiles(40), &mut prev, &mut flips);
        assert_eq!(flips.len(), 1, "{flips:?}");
        let (at, to, reason) = flips[0];
        assert_eq!((to, reason), (FrameMode::TileCodec, "bandwidth_fits"));
        assert!(at >= 1_000_000, "left before the hold: {at}");
    }

    #[test]
    fn between_the_shares_neither_mode_moves() {
        // 20 Mbit/s; 40 tiles = 12.5 Mbit/s = 62 %: under enter, over exit.
        for start in [FrameMode::TileCodec, FrameMode::H264] {
            let mut c = classifier(20_000_000);
            let mut prev = start;
            let mut flips = Vec::new();
            drive(&mut c, 0, 5_000_000, &tiles(40), &mut prev, &mut flips);
            assert!(flips.is_empty(), "{start:?}: {flips:?}");
        }
    }

    #[test]
    fn an_unknown_estimate_holds_h264_whatever_the_demand() {
        let mut c = classifier(100_000_000);
        c.set_adaptation_context(AdaptationContext {
            estimate_known: false,
            ..ctx(100_000_000)
        });
        let mut prev = FrameMode::H264;
        let mut flips = Vec::new();
        drive(&mut c, 0, 5_000_000, &[], &mut prev, &mut flips);
        assert!(flips.is_empty(), "{flips:?}");
        let (m, r) = c.decide_inner(5_000_000, &[], FrameMode::H264);
        assert_eq!((m, r), (FrameMode::H264, "estimate_unknown"));
    }

    #[test]
    fn a_repaint_that_cannot_drain_in_time_keeps_a_static_screen_in_h264() {
        // 2040 tiles x 1300 B = 2.65 MB: 7 s at 3 Mbit/s, under 0.25 s at
        // 100 Mbit/s. A static screen has no demand at all.
        let mut slow = classifier(3_000_000);
        slow.set_screen(2040, 2040 * 1300);
        let mut prev = FrameMode::H264;
        let mut flips = Vec::new();
        drive(&mut slow, 0, 5_000_000, &[], &mut prev, &mut flips);
        assert!(flips.is_empty(), "{flips:?}");

        let mut fast = classifier(100_000_000);
        fast.set_screen(2040, 2040 * 1300);
        let mut prev = FrameMode::H264;
        let mut flips = Vec::new();
        drive(&mut fast, 0, 5_000_000, &[], &mut prev, &mut flips);
        assert_eq!(flips.len(), 1, "{flips:?}");
    }

    #[test]
    fn a_repaint_frame_after_note_repaint_is_not_demand() {
        // 25 Mbit/s: a 2040-tile repaint frame alone is 21 Mbit/s over the
        // window, over 0.7 x 25. Noted as a repaint, it must not count.
        let mut c = classifier(25_000_000);
        c.note_repaint(0);
        let full = tiles(2040);
        let mut prev = FrameMode::TileCodec;
        let (m, _) = c.decide_inner(0, &full, prev);
        prev = m;
        let mut flips = Vec::new();
        drive(&mut c, FRAME_US, 2_000_000, &tiles(4), &mut prev, &mut flips);
        assert!(flips.is_empty(), "{flips:?}");
    }

    #[test]
    fn a_full_screen_frame_outside_the_grace_is_demand() {
        let mut c = classifier(25_000_000);
        c.note_repaint(0);
        let full = tiles(2040);
        let mut prev = FrameMode::TileCodec;
        let mut flips = Vec::new();
        // Full-screen motion starting after the 1 s grace.
        drive(&mut c, 1_100_000, 2_000_000, &full, &mut prev, &mut flips);
        assert_eq!(flips.first().map(|f| f.1), Some(FrameMode::H264), "{flips:?}");
    }

    /// The live 2026-11-07 symptom: a LAN estimate and bursts of motion
    /// (40 tiles for 1.5 s, quiet for 2 s). Starting in H.264 as a session
    /// does, the only transition is the start.
    #[test]
    fn intermittent_motion_on_a_lan_flips_once() {
        let mut c = classifier(150_000_000);
        let mut prev = FrameMode::H264;
        let mut flips = Vec::new();
        let mut t = 0;
        for _ in 0..8 {
            drive(&mut c, t, 1_500_000, &tiles(40), &mut prev, &mut flips);
            t += 1_500_000;
            drive(&mut c, t, 2_000_000, &[], &mut prev, &mut flips);
            t += 2_000_000;
        }
        assert_eq!(flips.len(), 1, "{flips:?}");
        assert_eq!(flips[0].1, FrameMode::TileCodec);
    }

    #[test]
    fn sustained_full_screen_motion_on_a_slow_link_stays_in_h264() {
        let mut c = classifier(3_000_000);
        let mut prev = FrameMode::H264;
        let mut flips = Vec::new();
        drive(&mut c, 0, 10_000_000, &tiles(2040), &mut prev, &mut flips);
        assert!(flips.is_empty(), "{flips:?}");
    }

    /// An estimate stepping between 3 and 150 Mbit/s every 5 s under
    /// steady 150-tile motion (47 Mbit/s: under 0.4 x 150, over 0.7 x 3):
    /// one transition per step.
    #[test]
    fn a_stepping_estimate_flips_once_per_step() {
        let mut c = classifier(150_000_000);
        let mut prev = FrameMode::TileCodec;
        let mut flips = Vec::new();
        let mut t = 0;
        for step in 0..6 {
            let est = if step % 2 == 0 { 150_000_000 } else { 3_000_000 };
            c.set_adaptation_context(ctx(est));
            drive(&mut c, t, 5_000_000, &tiles(150), &mut prev, &mut flips);
            t += 5_000_000;
        }
        // Steps 1..=5 each change which side of the band the demand is on.
        assert_eq!(flips.len(), 5, "{flips:?}");
    }

    #[test]
    fn calibration_moves_the_threshold() {
        // 40 tiles at the 1300 B prior is 12.5 Mbit/s: over 0.7 x 15.
        // Calibrated to 300 B per tile it is 2.9 Mbit/s: under.
        let mut c = classifier(15_000_000);
        for _ in 0..400 {
            c.observe_first_pass(crate::tile::first_pass::FirstPassKind::Cdf53, 40 * 300, 40);
        }
        let mut prev = FrameMode::TileCodec;
        let mut flips = Vec::new();
        drive(&mut c, 0, 3_000_000, &tiles(40), &mut prev, &mut flips);
        assert!(flips.is_empty(), "{flips:?}");
    }
}
```

Check the arithmetic of each scenario against `ENTER_SHARE = 0.70`,
`EXIT_SHARE = 0.40`, before running: 40 tiles × 1300 B × 30 fps × 8 =
12.48 Mbit/s; 150 tiles = 46.8 Mbit/s; 2040 tiles = 636 Mbit/s; 4 tiles =
1.25 Mbit/s. The window fills over its first second, so a demand ramps up
from 0: an enter fires about 0.5 s after the ramp crosses the share, not 0.5 s
after the first frame.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p ghostframe-lib --lib tile::classifier::decide_tests::bandwidth_fit`
Expected: compile errors (`estimate_known`, `set_screen`, `note_repaint`,
`observe_first_pass` do not exist).

- [ ] **Step 3: Change `classifier.rs`.** In order:

1. Delete `CostModel`, its `Default`, its `impl` (lines 8-120 today) and the
   `#[cfg(test)] mod cost_tests;` block. Update the module doc's first line to
   "Per-tile classifier and bandwidth-fit frame-mode decision." Change the
   `use` line to `use super::first_pass::{DemandWindow, FirstPassKind, FirstPassModel};`
   plus `use super::{CodecState, FrameMode, TileMetrics};` (drop `TILE_BYTES`
   if nothing else uses it; let the compiler tell you).
2. `AdaptationContext`: add after `bwe_estimate_bps`:

```rust
    /// The bridge has seen feedback about this session's path: the estimate
    /// is off goog_cc's initial value and moved by at least one sample since
    /// the session reset. Until then the bandwidth rule holds the mode
    /// (`estimate_unknown`). `false` in `Default`.
    pub estimate_known: bool,
```
   and `estimate_known: false,` in its `Default`. Update the `bytes_per_us`
   doc to say the classifier no longer reads it (it is the bridge's
   probe-inclusive model rate, kept for the bridge's own tests and logs).
3. Delete `REFINEMENT_BIAS_PER_TILE_US`. Add, next to the headroom constants:

```rust
/// Enter H.264 when the first-pass demand stays above this share of the
/// estimate. Below the 0.8 the H.264 rate targets, so H.264 has headroom.
pub(crate) const ENTER_SHARE: f64 = 0.70;
/// Leave H.264 when the demand stays below this share of the estimate.
pub(crate) const EXIT_SHARE: f64 = 0.40;
/// Leaving H.264 repaints the whole screen; leave only if that repaint's
/// first pass drains within this long at the estimate.
pub(crate) const REPAINT_DRAIN_MAX_US: u64 = 1_000_000;
/// After a repaint is requested, a frame covering most of the screen is the
/// repaint, not demand, for this long.
pub(crate) const REPAINT_GRACE_US: u64 = 1_000_000;
```
4. `Classifier` struct: delete `cost`, `enter_factor`, `exit_factor`,
   `motion_tile_threshold`, `motion_tile_min_absolute`,
   `refinement_deficit_tiles`, `refinement_bias_us_override`. Add:

```rust
    /// Calibrated first-pass size per tile.
    first_pass: FirstPassModel,
    /// First-pass bytes of the last second of frames.
    demand: DemandWindow,
    /// The demand at the last decision, bit/s (logged).
    last_demand_bps: u64,
    /// Tiles on the screen, and the predicted first-pass bytes of
    /// repainting all of them (0 when unknown or not in H.264).
    screen_tiles: u32,
    repaint_bytes: u64,
    /// A repaint was requested; until then a near-full-screen frame is not
    /// demand.
    repaint_grace_until_us: Option<u64>,
    /// `ENTER_SHARE` / `EXIT_SHARE`, or their `ClassifierConfig` overrides.
    enter_share: f64,
    exit_share: f64,
```
   In `new`: `enter_sustain_micros: 500_000`, `exit_sustain_micros: 1_000_000`,
   frames mins stay 3 and 30; `first_pass: FirstPassModel::default()`,
   `demand: DemandWindow::default()`, `last_demand_bps: 0`, `screen_tiles: 0`,
   `repaint_bytes: 0`, `repaint_grace_until_us: None`, and

```rust
            enter_share: config.enter_share.map_or(ENTER_SHARE, f64::from),
            exit_share: config.exit_share.map_or(EXIT_SHARE, f64::from),
```
   Update the field docs of `enter_sustain_micros` / `exit_sustain_micros`
   ("Default 500_000 µs" / "Default 1_000_000 µs").
5. `reset`: also `self.demand.clear(); self.first_pass = FirstPassModel::default();
   self.last_demand_bps = 0; self.repaint_bytes = 0; self.repaint_grace_until_us = None;`
   (keep `screen_tiles`: the screen did not change).
6. `set_adaptation_context`: only `self.adaptation_context = ctx;` (drop the
   `bytes_per_us` mirror and fix its doc). Delete `set_refinement_deficit_tiles`.
   Add:

```rust
    /// The screen's tile count and the predicted first-pass bytes of
    /// repainting it (see `first_pass::repaint_state`). The bridge calls this
    /// every frame; `repaint_bytes` may be 0 outside H.264.
    pub fn set_screen(&mut self, screen_tiles: u32, repaint_bytes: u64) {
        self.screen_tiles = screen_tiles;
        self.repaint_bytes = repaint_bytes;
    }

    /// A full-screen repaint was requested at `now_us` (session start, the
    /// H.264 exit, a stuck-entry re-dirty): for `REPAINT_GRACE_US` a frame
    /// dirtying at least 90 % of the screen is that repaint, not demand.
    pub fn note_repaint(&mut self, now_us: u64) {
        self.repaint_grace_until_us = Some(now_us.saturating_add(REPAINT_GRACE_US));
    }

    /// One emission batch's first-pass bytes of `kind` (see
    /// `FirstPassModel::observe`).
    pub fn observe_first_pass(&mut self, kind: FirstPassKind, bytes: u64, tiles: u32) {
        self.first_pass.observe(kind, bytes, tiles);
    }

    /// The calibrated per-tile model, for the bridge's repaint estimate.
    pub fn first_pass(&self) -> FirstPassModel {
        self.first_pass
    }
```
7. `decide_inner`: record the frame's demand **before** `decide_core`, so
   override paths do not leave holes in the window:

```rust
        let frame_bytes: f64 = tentative_states
            .iter()
            .map(|s| self.first_pass.state_bytes(*s))
            .sum();
        let in_grace = self.repaint_grace_until_us.is_some_and(|t| now_us < t);
        let is_repaint = in_grace
            && self.screen_tiles > 0
            && (tentative_states.len() as u64) * 10 >= u64::from(self.screen_tiles) * 9;
        self.last_demand_bps = self
            .demand
            .push(now_us, if is_repaint { 0.0 } else { frame_bytes });
        let out = self.decide_core(now_us, tentative_states, prev_mode);
```
   (the healthy-since bookkeeping after it stays).
8. `decide_core`: replace everything from the comment `// Cost path.` to the
   end of the function with:

```rust
        // Bandwidth path (design 2026-11-07): H.264 only when the tile codec's
        // first pass does not fit the estimate.
        if !ctx.estimate_known || ctx.bwe_estimate_bps == 0 {
            self.state = ClassifierHysteresis::default();
            return (prev_mode, "estimate_unknown");
        }
        let estimate = ctx.bwe_estimate_bps as f64;
        let demand = self.last_demand_bps as f64;
        let enter_now = demand > self.enter_share * estimate;
        let exit_now = demand < self.exit_share * estimate
            && self.repaint_drain_us() <= REPAINT_DRAIN_MAX_US;

        // Hybrid hysteresis: a transition requires BOTH the wall-clock
        // dwell AND the consecutive-call streak. The streak is preemption-
        // immune (no calls during a pause = no advance); the wall-clock
        // floor prevents over-eager flips at high FPS. See 7e3e041
        // regression in `feedback_classifier_hysteresis_micros_regression`.
        match prev_mode {
            FrameMode::TileCodec => {
                if enter_now {
                    let started = self.state.enter_started_us.get_or_insert(now_us);
                    self.state.enter_streak = self.state.enter_streak.saturating_add(1);
                    self.state.exit_started_us = None;
                    self.state.exit_streak = 0;
                    let micros_elapsed =
                        now_us.saturating_sub(*started) >= self.enter_sustain_micros;
                    let frames_elapsed = self.state.enter_streak >= self.enter_sustain_frames_min;
                    if micros_elapsed && frames_elapsed {
                        self.state.enter_started_us = None;
                        self.state.enter_streak = 0;
                        return (FrameMode::H264, "bandwidth_short");
                    }
                } else {
                    self.state.enter_started_us = None;
                    self.state.enter_streak = 0;
                }
                (FrameMode::TileCodec, "hysteresis_clamp")
            }
            FrameMode::H264 => {
                if exit_now {
                    let started = self.state.exit_started_us.get_or_insert(now_us);
                    self.state.exit_streak = self.state.exit_streak.saturating_add(1);
                    self.state.enter_started_us = None;
                    self.state.enter_streak = 0;
                    let micros_elapsed =
                        now_us.saturating_sub(*started) >= self.exit_sustain_micros;
                    let frames_elapsed = self.state.exit_streak >= self.exit_sustain_frames_min;
                    if micros_elapsed && frames_elapsed {
                        self.state.exit_started_us = None;
                        self.state.exit_streak = 0;
                        return (FrameMode::TileCodec, "bandwidth_fits");
                    }
                } else {
                    self.state.exit_started_us = None;
                    self.state.exit_streak = 0;
                }
                (FrameMode::H264, "hysteresis_clamp")
            }
        }
    }

    /// How long repainting the screen's first pass takes at the estimate, µs.
    fn repaint_drain_us(&self) -> u64 {
        let est = self.adaptation_context.bwe_estimate_bps;
        if est == 0 {
            return u64::MAX;
        }
        ((self.repaint_bytes as f64) * 8.0 * 1e6 / est as f64) as u64
    }
```
   Update `decide_inner`'s doc list of reasons: "headroom_guard",
   "loss_override", "suspension", "bandwidth_short", "bandwidth_fits",
   "estimate_unknown", "hysteresis_clamp".
9. `decide_frame_mode_at`'s `mode.decision` event: replace the
   `bytes_per_us` and `refinement_deficit_tiles` fields with

```rust
                first_pass_demand_bps = self.last_demand_bps,
                estimate_known = ctx.estimate_known,
                repaint_drain_ms = self.repaint_drain_us().min(u64::MAX / 2) / 1000,
```
   and delete its "M3.0 caveat" doc paragraph about the motion fast path.
   Replace it with one sentence: "Empty `tentative_states` is valid: a frame
   with no dirty tiles adds no demand."

- [ ] **Step 4: Rewrite the old decide tests.** In `classifier_decide_tests.rs`:
   - Delete every test whose subject no longer exists: the motion fast path
     (`enters_h264_after_sustain_frames_via_motion_fastpath` and any test using
     `motion_tile_*`), the refinement bias (`refinement_bias_promotes_*`,
     `env_var_override_refinement_bias_us`, `refinement_deficit_biases_*`),
     and any test that only asserts `cost.*` arithmetic. List them in the
     commit message.
   - Every remaining test that set `bytes_per_us` to steer the cost path now
     steers the bandwidth path: set `bwe_estimate_bps` and
     `estimate_known: true` in its `AdaptationContext`, and size its
     `tentative_states` with the arithmetic in Step 1's note. Keep each test's
     intent (its name says it); where the name says "cost", rename it.
   - In `mod guard` (the `ctx()` helper near line 927): add
     `estimate_known: true` and `bwe_estimate_bps: 100_000_000` to `ctx()`,
     and change `heavy_motion()` to return 2040 `Cdf53` tiles (636 Mbit/s, over
     0.7 x any estimate the module uses). Update its doc comment.
   - `after_the_holdoff_the_cost_path_still_needs_its_own_dwell` becomes
     `after_the_holdoff_the_bandwidth_path_still_needs_its_own_dwell`; its
     `run` window must now be at least `500_000` µs past the holdoff (enter
     hold), e.g. `t + 700_000`.
   - Every assertion on `"cost_comparison"` becomes `"bandwidth_short"`
     (entering) or `"bandwidth_fits"` (leaving).
   - `a_low_model_rate_with_a_healthy_estimate_no_longer_sticks_in_h264`: the
     estimate of 4 Mbit/s with an idle screen now needs `estimate_known: true`
     and a run of at least 1.1 s (exit hold); it already runs 3 s.

- [ ] **Step 5: Config, minimal.** `config.rs` still builds a `CostModelConfig`
   and `refinement_bias_us` that nothing reads; leave them for Task 3, but add
   the two fields the classifier now reads, to `ClassifierConfig`:

```rust
    /// Overrides `ENTER_SHARE`: enter H.264 when the first-pass demand stays
    /// above this share of the estimate. `GHOSTFRAME_MODE_ENTER_SHARE`,
    /// `(0.0, 1.0]`.
    pub enter_share: Option<f32>,
    /// Overrides `EXIT_SHARE`. `GHOSTFRAME_MODE_EXIT_SHARE`, `(0.0, 1.0]`,
    /// and below the enter share (see `from_lookup`).
    pub exit_share: Option<f32>,
```
   with `enter_share: None, exit_share: None` wherever `ClassifierConfig` is
   built literally (`grep -n 'ClassifierConfig {' -r ghostframe-lib ghostframe-e2e ghostframe-server`),
   and in `from_lookup` set both to `None` for now (Task 3 parses them).

- [ ] **Step 6: Run the classifier tests**

Run: `cargo test -p ghostframe-lib --lib tile::`
Expected: all pass, including the 12 `bandwidth_fit` tests.

- [ ] **Step 7: Mutation checks** (each must make at least one `bandwidth_fit`
test fail; revert after each):
   - `enter_now = demand > self.enter_share * estimate` → `demand > 0.0`
   - drop `&& self.repaint_drain_us() <= REPAINT_DRAIN_MAX_US`
   - `is_repaint` → `false`
   - in `decide_core`, delete the `estimate_unknown` early return
   - `exit_sustain_micros: 1_000_000` → `0`

- [ ] **Step 8: Compile the workspace** (the bridge still calls the removed
`set_refinement_deficit_tiles`; remove that call and its `deficit` line in
`frame.rs` ~1465 now, nothing else in the bridge):

Run: `cargo clippy --workspace --all-targets -- -D warnings`
Expected: clean except errors in `config.rs` tests that reference `CostModel`
(lines ~1199-1235). Delete those two tests here (Task 3 replaces them); any
other error, fix in this task.

- [ ] **Step 9: Commit**

```bash
git rm ghostframe-lib/src/tile/classifier_cost_tests.rs
git add ghostframe-lib/src/tile/classifier.rs ghostframe-lib/src/tile/classifier_decide_tests.rs ghostframe-lib/src/tile/mod.rs ghostframe-lib/src/config.rs ghostframe-lib/src/transport/io_bridge/frame.rs
git commit -m "feat(classifier): switch on whether the first pass fits the estimate"
```
(add any other file Step 8 touched, by name).

---

### Task 3: Configuration

**Files:**
- Modify: `ghostframe-lib/src/config.rs`

- [ ] **Step 1: Failing tests** (in `config.rs`'s test module, next to the
existing classifier-override tests near line 1160; reuse its `lookup` helper):

```rust
    #[test]
    fn mode_shares_parse_and_an_inverted_pair_is_ignored() {
        let c = ClassifierConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_MODE_ENTER_SHARE", "0.8"),
            ("GHOSTFRAME_MODE_EXIT_SHARE", "0.3"),
        ]));
        assert_eq!((c.enter_share, c.exit_share), (Some(0.8), Some(0.3)));

        // Exit at or above enter would flap: both are dropped.
        let c = ClassifierConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_MODE_ENTER_SHARE", "0.5"),
            ("GHOSTFRAME_MODE_EXIT_SHARE", "0.5"),
        ]));
        assert_eq!((c.enter_share, c.exit_share), (None, None));

        // One alone is checked against the other's default.
        let c = ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_MODE_EXIT_SHARE", "0.9")]));
        assert_eq!((c.enter_share, c.exit_share), (None, None));

        for bad in ["0", "-0.1", "1.5", "nan", "x"] {
            let c = ClassifierConfig::from_lookup(lookup(&[("GHOSTFRAME_MODE_ENTER_SHARE", bad)]));
            assert_eq!(c.enter_share, None, "{bad}");
        }
    }

    #[test]
    fn the_removed_cost_variables_are_ignored() {
        // They parse to nothing; the warning is logged once per process.
        let c = ClassifierConfig::from_lookup(lookup(&[
            ("GHOSTFRAME_COST_CDF53_US", "10"),
            ("GHOSTFRAME_REFINEMENT_BIAS_US", "5"),
        ]));
        assert_eq!(c, ClassifierConfig::default());
    }
```
If `ClassifierConfig` does not derive `PartialEq`, add it (its fields all do).

- [ ] **Step 2: Run, expect failure**

Run: `cargo test -p ghostframe-lib --lib config::`
Expected: first test fails (`None` parsed), second may pass or fail; both must
pass after Step 3.

- [ ] **Step 3: Implement.**
   - Delete `CostModelConfig`, its `impl`, the `cost` field of
     `ClassifierConfig`, the `refinement_bias_us` field, and their parsing.
   - Add a `share` parser and the pair check in `ClassifierConfig::from_lookup`:

```rust
fn share(raw: Option<String>) -> Option<f32> {
    raw.and_then(|s| s.parse::<f32>().ok())
        .filter(|v| v.is_finite() && *v > 0.0 && *v <= 1.0)
}
```
   and, in `from_lookup` before building `Self`:

```rust
        let enter = share(get("GHOSTFRAME_MODE_ENTER_SHARE"));
        let exit = share(get("GHOSTFRAME_MODE_EXIT_SHARE"));
        let enter_eff = enter.map_or(crate::tile::classifier::ENTER_SHARE, f64::from);
        let exit_eff = exit.map_or(crate::tile::classifier::EXIT_SHARE, f64::from);
        let (enter_share, exit_share) = if exit_eff < enter_eff {
            (enter, exit)
        } else {
            static WARNED: std::sync::Once = std::sync::Once::new();
            WARNED.call_once(|| {
                tracing::warn!(
                    target: "ghostframe::config",
                    "GHOSTFRAME_MODE_EXIT_SHARE ({exit_eff}) must be below \
                     GHOSTFRAME_MODE_ENTER_SHARE ({enter_eff}); ignoring both"
                );
            });
            (None, None)
        };
        const REMOVED: [&str; 8] = [
            "GHOSTFRAME_COST_SOLID_US",
            "GHOSTFRAME_COST_PALRLE_US",
            "GHOSTFRAME_COST_CDF53_US",
            "GHOSTFRAME_COST_H264_FRAME_US",
            "GHOSTFRAME_COST_H264_FRAME_BYTES",
            "GHOSTFRAME_COST_BYTES_PER_US",
            "GHOSTFRAME_REFINEMENT_BIAS_US",
            "GHOSTFRAME_TEST_REFINEMENT_BIAS_US",
        ];
        if let Some(name) = REMOVED.iter().find(|n| get(n).is_some()) {
            static WARNED_REMOVED: std::sync::Once = std::sync::Once::new();
            WARNED_REMOVED.call_once(|| {
                tracing::warn!(
                    target: "ghostframe::config",
                    "{name} was removed and is ignored: the frame mode follows \
                     the bandwidth estimate (GHOSTFRAME_MODE_ENTER_SHARE / \
                     GHOSTFRAME_MODE_EXIT_SHARE)"
                );
            });
        }
```
   `ENTER_SHARE` / `EXIT_SHARE` are `pub(crate)`; that is enough inside the crate.
   - Fix the `effective LibConfig` doc strings and any `ClassifierConfig {`
     literal the compiler flags.
   - `ci/` has an env-read guard (`just ci-local` runs it): run
     `grep -rn 'env-read\|check-env' ci/ Justfile` and make sure the two new
     names are read only through `get` here.

- [ ] **Step 4: Run**

Run: `cargo test -p ghostframe-lib --lib config:: && cargo clippy --workspace --all-targets -- -D warnings`
Expected: pass, clean.

- [ ] **Step 5: Mutation check.** Replace the pair check by `(enter, exit)`:
the second assertion of the first test must fail. Revert.

- [ ] **Step 6: Commit**

```bash
git add ghostframe-lib/src/config.rs
git commit -m "feat(config): mode share overrides; the cost-model variables are removed"
```

---

### Task 4: The bridge feeds the classifier

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs` (field + init ~line 1613)
- Modify: `ghostframe-lib/src/transport/io_bridge/bwe_glue.rs:631-650`
- Modify: `ghostframe-lib/src/transport/io_bridge/session.rs:150-200`
- Modify: `ghostframe-lib/src/transport/io_bridge/frame.rs` (~1460-1480, ~1564-1580)
- Modify: `ghostframe-lib/src/transport/io_bridge/pacing.rs:495`
- Modify: `ghostframe-lib/src/transport/io_bridge/emit.rs` (the drain loop ~795-960)
- Test: `ghostframe-lib/src/transport/io_bridge/tests.rs`

- [ ] **Step 1: Failing bridge test** (append to `tests.rs`; `make_bridge_for_test`
is the existing helper used near line 3450):

```rust
/// A session reset carries the previous session's estimate, so the value
/// alone cannot say the path is known: it takes a feedback sample after the
/// reset.
#[tokio::test(start_paused = true)]
async fn the_estimate_is_unknown_until_a_sample_follows_the_session_reset() {
    let mut bridge = make_bridge_for_test().await;
    bridge.bwe_samples_at_session_reset = bridge.bwe.snapshot().samples_seen;
    bridge.apply_path_stats_snapshot(1_000.0);
    assert!(!bridge.adaptation_context.estimate_known);

    // One sample after the reset, with the estimate off its initial value.
    bridge.bwe_samples_at_session_reset = 0;
    bridge.test_force_estimate_known_inputs(12_000_000, 1);
    bridge.apply_path_stats_snapshot(1_000.0);
    assert!(bridge.adaptation_context.estimate_known);
}
```
Before writing `test_force_estimate_known_inputs`, look for an existing way
the bridge tests feed the BWE (`grep -n 'fn .*bwe\|AckArrival' tests.rs
bwe_glue.rs | head`). If one exists (feeding real ack arrivals), use it and
delete this helper from the plan; otherwise add, under
`#[cfg(any(test, feature = "test-loss-injection"))]` in `bwe_glue.rs`:

```rust
    /// Pin the two inputs of `estimate_known` (estimate, samples seen).
    pub(crate) fn test_force_estimate_known_inputs(&mut self, bps: u64, samples: u64) {
        self.test_estimate_known_inputs = Some((bps, samples));
    }
```
with a matching `test_estimate_known_inputs: Option<(u64, u64)>` field (cfg-gated
the same way, `None` at construction, next to `test_force_bytes_per_us`).

- [ ] **Step 2: Run, expect a compile failure**

Run: `cargo test -p ghostframe-lib --lib the_estimate_is_unknown`

- [ ] **Step 3: Implement.**
   - `io_bridge.rs`: field `bwe_samples_at_session_reset: u64` (doc: "`bwe`
     samples seen when the current session was reset; `estimate_known` needs
     one more"), initialised `0`.
   - `bwe_glue.rs` `apply_path_stats_snapshot`, after `bwe_estimate_bps`:

```rust
        let snap = self.bwe.snapshot();
        #[cfg(any(test, feature = "test-loss-injection"))]
        let (snap_bps, snap_samples) = self
            .test_estimate_known_inputs
            .unwrap_or((snap.bitrate_bps, snap.samples_seen));
        #[cfg(not(any(test, feature = "test-loss-injection")))]
        let (snap_bps, snap_samples) = (snap.bitrate_bps, snap.samples_seen);
        let estimate_known = snap_bps != crate::transport::bwe::BweWrapper::INITIAL_BPS
            && snap_samples > self.bwe_samples_at_session_reset;
```
     (reuse `snap` for `bwe_estimate_bps` instead of a second `snapshot()`
     call), and in the `test_force_bytes_per_us` match, a forced rate also
     forces `estimate_known = true` (those tests pin the rate to drive the
     headroom guard and must not be held by `estimate_unknown`). Then
     `self.adaptation_context.estimate_known = estimate_known;`. Check the
     `BweWrapper` path with `grep -n 'pub struct BweWrapper' -r ghostframe-lib/src`.
   - `session.rs`, in the reset next to `self.classifier.reset();`:

```rust
        self.bwe_samples_at_session_reset = self.bwe.snapshot().samples_seen;
        self.adaptation_context.estimate_known = false;
```
     and after `self.force_dirty_frames = 20;`:

```rust
        // The cushion's all-dirty frames are a repaint, not demand.
        let now_us = now_std().duration_since(self.epoch).as_micros() as u64;
        self.classifier.note_repaint(now_us);
```
     (use whatever `now_std` import the file already has). Replace the comment
     at lines 150-155 ("Classifier exits back to TileCodec naturally after
     exit_sustain frames of empty dirty") with: "The classifier leaves H.264
     once the estimate is known and the tile codec's first pass fits it
     (design 2026-11-07)."
   - `frame.rs` `classify_dirty_tiles`, before `decide_frame_mode_at`:

```rust
        let screen_tiles = cols * rows;
        let repaint_bytes = if prev_mode == FrameMode::H264 {
            let fp = self.classifier.first_pass();
            self.metrics_tracker
                .metrics()
                .iter()
                .map(|m| fp.state_bytes(crate::tile::first_pass::repaint_state(m)))
                .sum::<f64>() as u64
        } else {
            0
        };
        self.classifier.set_screen(screen_tiles, repaint_bytes);
```
     This walks every tile once per frame only while in H.264 (8160 tiles at
     4K, a sum of floats); it is not per-tile dispatch work. In the
     `prev_mode == H264 && new_mode == TileCodec` handoff block, after
     `InvalidateBaseline(1)`, add `self.classifier.note_repaint(now_us);`.
     Update the comment at ~1449 ("the no-motion signal the classifier needs to
     exit H264 after exit_sustain") to "an empty dirty set adds no demand".
   - If the CPU path (the `dirty_tracker` path near `frame.rs:412`) also calls
     the classifier, give it the same `set_screen` call (`grep -n
     decide_frame_mode_at ghostframe-lib/src/transport/io_bridge/*.rs`).
   - `pacing.rs:495`, after `InvalidateBaseline(2)`: the same `note_repaint`
     with the bridge's `now_us` expression.
   - `emit.rs`, in the drain loop: before the loop, `let mut fp_obs = [(0u64, 0u32); 3];`
     (Solid, PalRle, Cdf53). Inside, after `tile_wire_bytes_total` is final and
     only when `!is_retry`:

```rust
            if !is_retry {
                let kind = match work.codec {
                    Codec::Solid => Some((0, crate::tile::first_pass::FirstPassKind::Solid)),
                    Codec::PalRle => Some((1, crate::tile::first_pass::FirstPassKind::PalRle)),
                    Codec::Cdf53 if matches!(pass_tier(work.pass_idx), PassTier::Critical) => {
                        Some((2, crate::tile::first_pass::FirstPassKind::Cdf53))
                    }
                    Codec::Cdf53 | Codec::H264 | Codec::Skip => None,
                };
                if let Some((i, _)) = kind {
                    fp_obs[i].0 += u64::from(tile_wire_bytes_total);
                    if work.pass_idx == 0 {
                        fp_obs[i].1 += 1;
                    }
                }
            }
```
     and after the loop:

```rust
        use crate::tile::first_pass::FirstPassKind as K;
        for (i, kind) in [K::Solid, K::PalRle, K::Cdf53].into_iter().enumerate() {
            let (bytes, tiles) = fp_obs[i];
            self.classifier.observe_first_pass(kind, bytes, tiles);
        }
```
     (`observe` ignores `(0, 0)`.) Drop the tuple's unused second element if
     clippy complains; it is there for readability only.

- [ ] **Step 4: Adapt the two classifier tests in `tests.rs`:**
   - `classifier_reset_clears_streaks` (~950): set an adaptation context with
     `supports_h264: true, bwe_estimate_bps: 1_000_000_000, estimate_known: true`
     and `c.set_screen(2040, 0)` before the busy frames, and make `busy`
     2040 `Cdf53` tiles (the old `H264` tiles fed the deleted motion path).
     With a 1 Gbit/s estimate those tiles are 63 %: raise the tile count or
     lower the estimate so the demand is over 0.7 x the estimate (e.g. 100
     Mbit/s). The assertion (one frame after reset does not promote) stays.
   - `frame_mode_exit_dwell_tracks_virtual_time_via_bridge_epoch` (~3450): set
     the same known-estimate context on `fixed` (and on the broken-path clone),
     and run 60 iterations of 20 ms (1.2 s, over `exit_sustain_micros` and the
     30-frame streak) instead of 30. Update its comments (`exit_factor *
     h264_cost`, `500_000us`).

- [ ] **Step 5: Run**

Run: `cargo test -p ghostframe-lib --lib io_bridge:: && cargo clippy --workspace --all-targets -- -D warnings`
Expected: pass, clean.

- [ ] **Step 6: Mutation checks:** remove `&& snap_samples > self.bwe_samples_at_session_reset`:
the new test's first assertion must fail. Remove the `note_repaint` call in
`session.rs`: no unit test catches it, so say so in the commit message and
rely on Task 7's live check for it.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs ghostframe-lib/src/transport/io_bridge/bwe_glue.rs ghostframe-lib/src/transport/io_bridge/session.rs ghostframe-lib/src/transport/io_bridge/frame.rs ghostframe-lib/src/transport/io_bridge/pacing.rs ghostframe-lib/src/transport/io_bridge/emit.rs ghostframe-lib/src/transport/io_bridge/tests.rs
git commit -m "feat(io_bridge): feed the bandwidth rule: known estimate, repaint size, calibration"
```

---

### Task 5: Consumers of the reason names

**Files:**
- Modify: `ghostframe-e2e/tests/browserless_h264.rs` (`guard_release`, ~1265)
- Modify: `ghostframe-e2e/tests/e2e.rs` (lines ~1248, 1331, 2273, 2458, 3256, 3370, 3586, 3738, 5173)
- Modify: `ghostframe-e2e/src/harness/scene.rs:74`
- Modify: `ghostframe-bench/src/bin/codec_report/{report.rs,aggregate.rs,bandwidth_matrix.rs,loss_axis.rs}`
- Modify: `ghostframe-client-core/src/loss_tracker.rs:77` (comment only)

- [ ] **Step 1:** `guard_release`: add `estimate_known: true` to its
`AdaptationContext`. The guard's release now also needs the exit hold of 1 s
(it was 0.5 s); read the assertions of the test that calls it and widen a
release-time bound by 0.5 s only if the run shows it is needed, saying so in a
comment.

- [ ] **Step 2:** `e2e.rs`: every `"cost_comparison"` becomes
`"bandwidth_short"` (entering H.264) or `"bandwidth_fits"` (leaving). Comments
that say "after `exit_sustain_frames = 30` of static content" become "once the
estimate is known and the first pass fits (at least 1 s and 30 frames)". The
test at ~3370 counts entries `by_cost`: rename it `by_bandwidth`. A test that
pinned a low capture rate (2 fps) to reason about the 30-frame streak now
needs 30 frames *and* 1 s: check its timing comment still holds at 2 fps
(15 s); if the test's own timeout is shorter, raise the timeout, and say why.

- [ ] **Step 3:** bench and harness: replace the `cost_comparison` key and
column with `bandwidth_short` and `bandwidth_fits` (two columns), and add
`estimate_unknown` to the reason list in `aggregate.rs` and `scene.rs`'s doc.
Update the bench's report test (~report.rs:580-654) to the new keys.

- [ ] **Step 4: Run**

Run: `cargo clippy --workspace --all-targets -- -D warnings && cargo test -p ghostframe-bench && cargo test -p ghostframe-e2e --test browserless_h264 guard`
Expected: clean, pass. (`guard_release` is called by a named test; run that
test by name, `grep -n 'guard_release(' ghostframe-e2e/tests/browserless_h264.rs`.)

- [ ] **Step 5: Commit**

```bash
git add ghostframe-e2e/tests/browserless_h264.rs ghostframe-e2e/tests/e2e.rs ghostframe-e2e/src/harness/scene.rs ghostframe-bench/src/bin/codec_report/report.rs ghostframe-bench/src/bin/codec_report/aggregate.rs ghostframe-bench/src/bin/codec_report/bandwidth_matrix.rs ghostframe-bench/src/bin/codec_report/loss_axis.rs ghostframe-client-core/src/loss_tracker.rs
git commit -m "chore: follow the classifier's new reason names"
```

---

### Task 6: Documentation

**Files:**
- Modify: `docs/design/h264-transport.md` (the variables table ~575 and every `cost_comparison` mention)
- Modify: `AGENTS.md` (any `cost_comparison` or `GHOSTFRAME_COST_*` mention; add the two share variables where the classifier is described)

- [ ] **Step 1:** `grep -rn 'cost_comparison\|GHOSTFRAME_COST_\|REFINEMENT_BIAS\|exit_sustain\|enter_factor' docs/design AGENTS.md DEVELOPERS.md README.md`
and update each hit. In `h264-transport.md`'s variables table, replace the
`GHOSTFRAME_COST_*` row with:

`| GHOSTFRAME_MODE_ENTER_SHARE, GHOSTFRAME_MODE_EXIT_SHARE | classifier: H.264 when the tile codec's first pass stays above the enter share of the estimate (0.70), back when below the exit share (0.40) | compiled in |`

In `AGENTS.md`, under "Reading a live session", add one paragraph:

> The frame mode follows the bandwidth estimate: `mode.decision` reasons
> `bandwidth_short` (the tile codec's first pass stayed above 0.70 x the
> estimate for 500 ms) and `bandwidth_fits` (below 0.40 x for 1 s, with the
> exit repaint drainable in 1 s). Until the estimate has moved since the
> session reset the reason is `estimate_unknown` and the session stays in
> H.264. `first_pass_demand_bps` on the same line is the demand it compared.

- [ ] **Step 2: Commit**

```bash
git add docs/design/h264-transport.md AGENTS.md
git commit -m "docs: the frame mode follows the bandwidth estimate"
```

---

### Task 7: Gates, threshold sweep, live check

- [ ] **Step 1: Local gate.** Run: `just ci-local`. Expected: green. Any red is
fixed in the commit it belongs to (amend only an unpushed commit).

- [ ] **Step 2: Threshold sweep.** Temporarily add (do not commit) a test in
`bandwidth_fit` that runs `a_stepping_estimate_flips_once_per_step`'s timeline
and `intermittent_motion_on_a_lan_flips_once`'s for each `ENTER_SHARE` in
{0.6, 0.7, 0.8} x `EXIT_SHARE` in {0.3, 0.4, 0.5} (through
`ClassifierConfig { enter_share, exit_share, .. }`), printing flips and
fraction of time in H.264. Paste the 9-row table into the PR body. If a pair
other than (0.7, 0.4) is strictly better on both timelines, stop and ask the
owner before changing the constants.

- [ ] **Step 3: e2e.** These need the GPU: take the lock first (see the
memory note on `.gf-gpu.lock`, never run GPU suites in parallel). Then
`just containers-build` (required after a ghostframe-lib change) and run the
e2e tests whose reason strings changed:
`cargo test -p ghostframe-e2e --test e2e -- e2e_mode_switch e2e_progressive_refinement --test-threads=1`
plus every e2e test name Task 5 touched. Record pass/fail per test in the PR
body; capture any failing test name before re-running.

- [ ] **Step 4: Live check (owner-run).** `just build-release`, then as the
session user `./target/release/ghostframe-server install --force`, run the
native-client test script against evangeline for 10 minutes, then:

```bash
journalctl _UID=$(id -u <user>) --since "-10min" --no-pager | grep -c 'mode decision'
journalctl _UID=$(id -u <user>) --since "-10min" --no-pager | grep -c 'new QUIC connection accepted'
```
Expected: the two counts are equal (one transition per session, the start),
or the difference is explained by real full-screen motion in the session.

- [ ] **Step 5: PR.** Push the branch and open a PR against `master` with the
sweep table, the e2e results and the live counts (or "live check pending,
owner-run").
