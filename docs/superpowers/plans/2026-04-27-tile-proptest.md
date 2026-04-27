# Tile proptest harness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Encode the §4.2/§4.3 spec invariants on `TileGrid` and `DirtyTracker` as `proptest` properties so they regression-protect the M3 classifier work the day it lands.

**Architecture:** Add `proptest` as a dev-dependency, expose reusable BGRA-frame and damage-hint strategies in a new `tile::proptest_strategies` module, and add a sibling `tile::tests_proptest` module that asserts eight invariants drawn from the spec. The strategies and the test module are `#[cfg(test)]` only — they don't ship in the cdylib. M3 classifier invariants (C1–C6 from the design doc) are not implemented in this plan; they require types that do not exist yet.

**Tech Stack:** Rust, `proptest = "1"`, existing `ghostframe-lib` tile module.

---

## File Structure

```
ghostframe-lib/
├── Cargo.toml                          # add proptest dev-dependency
└── src/tile/
    ├── mod.rs                          # add cfg(test) module declarations
    ├── proptest_strategies.rs          # NEW — generators
    └── tests_proptest.rs               # NEW — invariants
```

Both new files are `#[cfg(test)]` only and live in `src/tile/` so they share the crate's private items (`TileGrid`, `DirtyTracker`, `TILE_SIZE`, `BPP`, `TILE_BYTES`).

---

## Task 1: Add proptest dev-dependency

**Files:**
- Modify: `ghostframe-lib/Cargo.toml`

- [ ] **Step 1: Add proptest under `[dev-dependencies]`**

Edit `ghostframe-lib/Cargo.toml`. Append to the existing `[dev-dependencies]` block:

```toml
proptest = "1"
```

(The workspace pins many crates with `=`, but proptest is a pure dev-dependency with no cross-crate version coupling — a caret range keeps it unobtrusive. The maintainer can tighten later if needed.)

- [ ] **Step 2: Verify the crate still builds**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build, no warnings about proptest.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/Cargo.toml
git commit -m "chore(tile): add proptest dev-dependency"
```

---

## Task 2: Strategies module skeleton

**Files:**
- Create: `ghostframe-lib/src/tile/proptest_strategies.rs`
- Modify: `ghostframe-lib/src/tile/mod.rs` (add `mod proptest_strategies;` under `#[cfg(test)]`)

- [ ] **Step 1: Create the strategies module**

Write `ghostframe-lib/src/tile/proptest_strategies.rs`:

```rust
//! Reusable `proptest` strategies for tile-level testing.
//!
//! Frames are produced as `(pixels, stride, width, height)` tuples in the same
//! shape `DirtyTracker::update` expects. Sizes are bounded so generated frames
//! stay under ~1 MB per case and shrinking is fast.

use proptest::collection::vec;
use proptest::prelude::*;

use super::{BPP, TILE_SIZE};

/// Maximum frame dimension generated. 256 = up to 8x8 tiles, plenty to
/// exercise edge tiles, multiple rows, and damage-hint subsets while keeping
/// each case small.
pub const MAX_DIM: u32 = 256;

/// A generated BGRA frame.
#[derive(Debug, Clone)]
pub struct Frame {
    pub pixels: Vec<u8>,
    pub stride: u32,
    pub width: u32,
    pub height: u32,
}

impl Frame {
    /// Number of bytes the buffer must hold for `(stride, height)`.
    pub fn buf_len(&self) -> usize {
        (self.stride * self.height) as usize
    }
}

/// Generate a dimension in `[1, MAX_DIM]`.
pub fn dim() -> impl Strategy<Value = u32> {
    1u32..=MAX_DIM
}

/// Generate a frame with stride exactly `width * BPP` (no padding).
pub fn frame_packed() -> impl Strategy<Value = Frame> {
    (dim(), dim()).prop_flat_map(|(width, height)| {
        let stride = width * BPP;
        let len = (stride * height) as usize;
        vec(any::<u8>(), len).prop_map(move |pixels| Frame {
            pixels,
            stride,
            width,
            height,
        })
    })
}

/// Generate a frame with arbitrary stride padding in `[0, 16]` pixels per row.
pub fn frame_padded() -> impl Strategy<Value = Frame> {
    (dim(), dim(), 0u32..=16).prop_flat_map(|(width, height, pad_px)| {
        let stride = (width + pad_px) * BPP;
        let len = (stride * height) as usize;
        vec(any::<u8>(), len).prop_map(move |pixels| Frame {
            pixels,
            stride,
            width,
            height,
        })
    })
}

/// Generate a damage-hint set as a (possibly empty) Vec of `(tile_x, tile_y)`
/// pairs bounded to the grid implied by `(width, height)`.
pub fn damage_hints(width: u32, height: u32) -> impl Strategy<Value = Vec<(u32, u32)>> {
    let cols = width.div_ceil(TILE_SIZE);
    let rows = height.div_ceil(TILE_SIZE);
    let total = (cols * rows) as usize;
    let max_hints = total.min(64);
    vec((0..cols, 0..rows), 0..=max_hints)
}
```

- [ ] **Step 2: Wire the module into `tile/mod.rs`**

Edit `ghostframe-lib/src/tile/mod.rs`. Find the existing `#[cfg(test)] mod dirty_tests {` block and add the new module declarations *above* it (still inside `#[cfg(test)]`):

```rust
#[cfg(test)]
mod proptest_strategies;

#[cfg(test)]
mod dirty_tests {
    // ... existing content unchanged ...
}
```

- [ ] **Step 3: Verify the strategies compile**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build. If `BPP` or `TILE_SIZE` aren't visible from the new module, double-check that `proptest_strategies.rs` lives in `src/tile/` (not `tests/`).

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/tile/proptest_strategies.rs ghostframe-lib/src/tile/mod.rs
git commit -m "test(tile): add proptest strategies for BGRA frames and damage hints"
```

---

## Task 3: TileGrid invariants

**Files:**
- Create: `ghostframe-lib/src/tile/tests_proptest.rs`
- Modify: `ghostframe-lib/src/tile/mod.rs` (add `mod tests_proptest;` under `#[cfg(test)]`)

- [ ] **Step 1: Create the test module with the four `TileGrid` properties**

Write `ghostframe-lib/src/tile/tests_proptest.rs`:

```rust
//! Property-based invariants for `TileGrid` and `DirtyTracker`.
//!
//! These encode rules from §4.1 / §4.2 of the spec so that the M3 classifier
//! work — which builds on top of these primitives — has a regression net.

use proptest::prelude::*;

use super::proptest_strategies::{damage_hints, dim, frame_packed, frame_padded, Frame, MAX_DIM};
use super::{DirtyTracker, TileGrid, BPP, TILE_BYTES, TILE_SIZE};

// ── TileGrid ────────────────────────────────────────────────────────────────

proptest! {
    /// Inv 1: tile_count == ceil(w/32) * ceil(h/32) for every dimension.
    #[test]
    fn tile_grid_count_matches_ceil_div(w in dim(), h in dim()) {
        let grid = TileGrid::new(w, h);
        let expected = w.div_ceil(TILE_SIZE) * h.div_ceil(TILE_SIZE);
        prop_assert_eq!(grid.tile_count(), expected);
        prop_assert_eq!(grid.cols, w.div_ceil(TILE_SIZE));
        prop_assert_eq!(grid.rows, h.div_ceil(TILE_SIZE));
    }

    /// Inv 1b: iter_coords yields exactly tile_count distinct (col, row) pairs.
    #[test]
    fn tile_grid_iter_coords_complete(w in dim(), h in dim()) {
        let grid = TileGrid::new(w, h);
        let coords: Vec<_> = grid.iter_coords().collect();
        prop_assert_eq!(coords.len() as u32, grid.tile_count());

        let mut sorted = coords.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), coords.len(), "iter_coords yielded duplicates");
    }

    /// Inv 2: extract_tile is total — never panics for any in-range coord
    /// regardless of how short the pixel buffer or how odd the stride is.
    #[test]
    fn extract_tile_never_panics(
        w in dim(),
        h in dim(),
        stride_pad in 0u32..=8,
        buf_trunc in 0usize..=16,
    ) {
        let grid = TileGrid::new(w, h);
        let stride = w * BPP + stride_pad * BPP;
        let full_len = (stride * h) as usize;
        let actual_len = full_len.saturating_sub(buf_trunc);
        let pixels = vec![0u8; actual_len];

        for (tx, ty) in grid.iter_coords() {
            let tile = grid.extract_tile(&pixels, stride, tx, ty);
            prop_assert_eq!(tile.len(), TILE_BYTES);
        }
    }

    /// Inv 8: stride independence — padded vs packed produces identical
    /// extracted tiles for the same per-pixel data.
    #[test]
    fn extract_tile_stride_independent(
        w in dim(),
        h in dim(),
        pad_px in 1u32..=8,
    ) {
        // Build packed and padded buffers carrying identical visible pixels.
        let stride_packed = w * BPP;
        let stride_padded = (w + pad_px) * BPP;

        // Deterministic pixel content so the two buffers carry the same data.
        let mut packed = vec![0u8; (stride_packed * h) as usize];
        let mut padded = vec![0u8; (stride_padded * h) as usize];
        for y in 0..h {
            for x in 0..w {
                let v = ((x ^ y) & 0xFF) as u8;
                let off_packed = (y * stride_packed + x * BPP) as usize;
                let off_padded = (y * stride_padded + x * BPP) as usize;
                packed[off_packed..off_packed + 4].copy_from_slice(&[v, v.wrapping_add(1), v.wrapping_add(2), 255]);
                padded[off_padded..off_padded + 4].copy_from_slice(&[v, v.wrapping_add(1), v.wrapping_add(2), 255]);
            }
        }

        let grid = TileGrid::new(w, h);
        for (tx, ty) in grid.iter_coords() {
            let a = grid.extract_tile(&packed, stride_packed, tx, ty);
            let b = grid.extract_tile(&padded, stride_padded, tx, ty);
            prop_assert_eq!(a, b);
        }
    }
}
```

- [ ] **Step 2: Wire the module into `tile/mod.rs`**

Edit `ghostframe-lib/src/tile/mod.rs`. Add a third `#[cfg(test)] mod` declaration alongside the other two:

```rust
#[cfg(test)]
mod proptest_strategies;

#[cfg(test)]
mod tests_proptest;

#[cfg(test)]
mod dirty_tests {
    // ... unchanged ...
}
```

- [ ] **Step 3: Run the new properties**

Run: `cargo test -p ghostframe-lib --lib tile::tests_proptest -- --nocapture`
Expected: 4 tests pass, each running 256 cases (proptest default). No shrinking output.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/tile/tests_proptest.rs ghostframe-lib/src/tile/mod.rs
git commit -m "test(tile): proptest invariants for TileGrid"
```

---

## Task 4: DirtyTracker invariants

**Files:**
- Modify: `ghostframe-lib/src/tile/tests_proptest.rs`

- [ ] **Step 1: Append the `DirtyTracker` properties**

Append to `ghostframe-lib/src/tile/tests_proptest.rs`:

```rust
// ── DirtyTracker ────────────────────────────────────────────────────────────

proptest! {
    /// Inv 3 (idempotence): same frame submitted twice → second update is empty.
    #[test]
    fn dirty_same_frame_twice_clean(frame in frame_packed()) {
        let grid = TileGrid::new(frame.width, frame.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);
        let again = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);
        prop_assert!(again.is_empty(), "expected no dirty tiles, got {:?}", again);
    }

    /// Inv 4 (first-frame): first update reports every tile coord exactly once.
    #[test]
    fn dirty_first_frame_all_dirty_unique(frame in frame_packed()) {
        let grid = TileGrid::new(frame.width, frame.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let dirty = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);

        prop_assert_eq!(dirty.len() as u32, grid.tile_count());

        let mut sorted = dirty.clone();
        sorted.sort_unstable();
        sorted.dedup();
        prop_assert_eq!(sorted.len(), dirty.len(), "first-frame dirty list had duplicates");
    }

    /// Inv 5 (hint subset): update_with_hints can only report tiles in the
    /// hint set, and only those whose pixels actually differ.
    #[test]
    fn dirty_with_hints_is_subset(
        frame_a in frame_packed(),
        // Force frame_b to share dimensions with frame_a so the tracker isn't reset.
        delta in proptest::collection::vec(any::<u8>(), 0..1024),
    ) {
        let grid = TileGrid::new(frame_a.width, frame_a.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame_a.pixels, frame_a.stride, frame_a.width, frame_a.height);

        // Build frame_b: same buffer with a few bytes overwritten.
        let mut buf = frame_a.pixels.clone();
        for (i, b) in delta.iter().enumerate().take(buf.len()) {
            buf[i] = *b;
        }

        // Use every grid tile as a hint — equivalent to update().
        let hints: Vec<_> = grid.iter_coords().collect();
        let dirty_hints = tracker.update_with_hints(
            &buf, frame_a.stride, frame_a.width, frame_a.height, &hints,
        );

        // Every reported tile must appear in the hint set.
        for d in &dirty_hints {
            prop_assert!(hints.contains(d), "{:?} reported but not hinted", d);
        }
    }

    /// Inv 6 (no_commit idempotent): two consecutive update_no_commit calls
    /// with the same frame return identical dirty sets.
    #[test]
    fn dirty_no_commit_idempotent(
        frame_a in frame_packed(),
        frame_b_seed in any::<u8>(),
    ) {
        let grid = TileGrid::new(frame_a.width, frame_a.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame_a.pixels, frame_a.stride, frame_a.width, frame_a.height);

        // Frame B differs by overwriting one pixel with a derived value.
        let mut buf = frame_a.pixels.clone();
        if !buf.is_empty() {
            buf[0] = buf[0].wrapping_add(frame_b_seed.max(1));
        }

        let first = tracker.update_no_commit(&buf, frame_a.stride, frame_a.width, frame_a.height);
        let second = tracker.update_no_commit(&buf, frame_a.stride, frame_a.width, frame_a.height);
        prop_assert_eq!(first, second);
    }

    /// Inv 7 (resize-on-mismatch): submitting a frame at a new (cols, rows)
    /// resets prior state, so the next update reports every tile dirty.
    #[test]
    fn dirty_resize_clears_state(
        frame_a in frame_packed(),
        // Small dim deltas guarantee a different (cols, rows).
        new_w in (TILE_SIZE * 2)..=MAX_DIM,
        new_h in (TILE_SIZE * 2)..=MAX_DIM,
    ) {
        prop_assume!(
            new_w.div_ceil(TILE_SIZE) != frame_a.width.div_ceil(TILE_SIZE)
                || new_h.div_ceil(TILE_SIZE) != frame_a.height.div_ceil(TILE_SIZE)
        );

        let grid_a = TileGrid::new(frame_a.width, frame_a.height);
        let mut tracker = DirtyTracker::new(grid_a.cols, grid_a.rows);
        let _ = tracker.update(&frame_a.pixels, frame_a.stride, frame_a.width, frame_a.height);

        let stride_b = new_w * BPP;
        let buf_b = vec![0u8; (stride_b * new_h) as usize];
        let dirty = tracker.update(&buf_b, stride_b, new_w, new_h);

        let grid_b = TileGrid::new(new_w, new_h);
        prop_assert_eq!(dirty.len() as u32, grid_b.tile_count());
    }

    /// Inv 5 supplement: any hint outside the grid is silently dropped.
    #[test]
    fn dirty_with_hints_out_of_range_dropped(
        frame in frame_packed(),
        bogus_x in (MAX_DIM / TILE_SIZE + 8)..(MAX_DIM / TILE_SIZE + 32),
        bogus_y in (MAX_DIM / TILE_SIZE + 8)..(MAX_DIM / TILE_SIZE + 32),
    ) {
        let grid = TileGrid::new(frame.width, frame.height);
        let mut tracker = DirtyTracker::new(grid.cols, grid.rows);
        let _ = tracker.update(&frame.pixels, frame.stride, frame.width, frame.height);

        let hints = vec![(bogus_x, bogus_y)];
        let dirty = tracker.update_with_hints(
            &frame.pixels, frame.stride, frame.width, frame.height, &hints,
        );
        prop_assert!(dirty.is_empty(), "out-of-range hint produced dirty tiles: {:?}", dirty);
    }
}
```

- [ ] **Step 2: Run the new properties**

Run: `cargo test -p ghostframe-lib --lib tile::tests_proptest -- --nocapture`
Expected: 10 tests pass total (4 from Task 3 + 6 here). No shrinking output.

If `dirty_resize_clears_state` shrinks to a counter-example, the bug is in `DirtyTracker::resize` — investigate before forcing the property to pass.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/tile/tests_proptest.rs
git commit -m "test(tile): proptest invariants for DirtyTracker"
```

---

## Task 5: Padded-stride sweep on DirtyTracker

**Files:**
- Modify: `ghostframe-lib/src/tile/tests_proptest.rs`

- [ ] **Step 1: Append a frame_padded property**

Append to `ghostframe-lib/src/tile/tests_proptest.rs`:

```rust
proptest! {
    /// Inv 8 supplement: DirtyTracker on padded-stride frames produces the
    /// same dirty set as on the equivalent packed-stride frame.
    ///
    /// Builds two buffers carrying identical visible pixels — one packed
    /// (stride == width * BPP) and one with row padding — and asserts both
    /// trackers see the same per-tile change pattern.
    #[test]
    fn dirty_packed_vs_padded_match(
        w in dim(),
        h in dim(),
        pad_px in 1u32..=8,
        flip_x in 0u32..MAX_DIM,
        flip_y in 0u32..MAX_DIM,
    ) {
        let stride_packed = w * BPP;
        let stride_padded = (w + pad_px) * BPP;

        let mut packed = vec![0u8; (stride_packed * h) as usize];
        let mut padded = vec![0u8; (stride_padded * h) as usize];

        // Frame A — all zeros (already initialised). Frame B — flip one pixel.
        let fx = flip_x % w;
        let fy = flip_y % h;
        let off_packed = (fy * stride_packed + fx * BPP) as usize;
        let off_padded = (fy * stride_padded + fx * BPP) as usize;

        let grid = TileGrid::new(w, h);
        let mut t_packed = DirtyTracker::new(grid.cols, grid.rows);
        let mut t_padded = DirtyTracker::new(grid.cols, grid.rows);

        // Settle on frame A.
        let _ = t_packed.update(&packed, stride_packed, w, h);
        let _ = t_padded.update(&padded, stride_padded, w, h);

        // Apply the same visible pixel change to both.
        packed[off_packed] = 0xFF;
        padded[off_padded] = 0xFF;

        let dirty_packed = t_packed.update(&packed, stride_packed, w, h);
        let dirty_padded = t_padded.update(&padded, stride_padded, w, h);

        prop_assert_eq!(dirty_packed, dirty_padded);
    }
}
```

- [ ] **Step 2: Run the full proptest module**

Run: `cargo test -p ghostframe-lib --lib tile::tests_proptest -- --nocapture`
Expected: 11 tests pass.

- [ ] **Step 3: Confirm runtime is healthy**

Run: `time cargo test -p ghostframe-lib --lib tile::tests_proptest`
Expected: total wall time < 30 s. If a property is taking far longer than the others, reduce its `MAX_DIM` or add `proptest_assume!` filters.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/tile/tests_proptest.rs
git commit -m "test(tile): proptest stride-independence between packed and padded"
```

---

## Task 6: Document the M3 follow-up

**Files:**
- Modify: `ghostframe-lib/src/tile/tests_proptest.rs`

- [ ] **Step 1: Append a forward-looking comment block**

Append to `ghostframe-lib/src/tile/tests_proptest.rs`:

```rust
// ── M3 classifier invariants (TODO) ─────────────────────────────────────────
//
// The following properties cannot be implemented today because the types they
// reference do not exist yet — they will land with the M3 classifier work
// (§4.2 / §4.3 of docs/specs/ghostframe-initial-spec.md).
//
// Add them in the same `proptest! { ... }` style once `TileMetrics` and
// `CodecState` are defined:
//
// * C1 — idle_frames > 0          ⇒ CodecState::Skip
// * C2 — change_freq < 5 Hz AND
//        change_magnitude < 0.1
//        sustained 30 frames      ⇒ never CodecState::H264
// * C3 — unique_colors == 1       ⇒ CodecState::Solid
// * C4 — codec selection is a pure function of TileMetrics
// * C5 — high-frequency low-magnitude (cursor blink) never enters H.264
// * C6 — H.264 → idle transition must traverse a lossy intermediate codec
//        before refinement begins
//
// Strategies: build a `TileMetrics` strategy in `proptest_strategies.rs`
// alongside `frame_packed()`. The classifier function under test should
// accept `&TileMetrics` and a `&CodecState` (current state) and return the
// next `CodecState`.
```

- [ ] **Step 2: Verify the file still compiles and tests pass**

Run: `cargo test -p ghostframe-lib --lib tile::tests_proptest`
Expected: 11 tests pass.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/tile/tests_proptest.rs
git commit -m "docs(tile): note M3 classifier proptest invariants to add later"
```

---

## Final verification

- [ ] **Step 1: Run the full library test suite**

Run: `cargo test -p ghostframe-lib --lib`
Expected: every existing test passes plus the 11 new proptest cases. No new warnings.

- [ ] **Step 2: Run all crates**

Run: `cargo build --workspace --tests`
Expected: clean build across `ghostframe-lib`, `ghostframe-xdaemon`, and `ghostframe-test-pattern`.
