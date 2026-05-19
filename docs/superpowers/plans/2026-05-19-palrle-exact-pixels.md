# `palrle_exact` Pixel Verification Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an `e2e_palrle_exact_pixels` end-to-end test that drives a paint-once four-pattern PalRle scene and asserts exact RGBA values at 16 sample points — closing the M3.2c B1 follow-up.

**Architecture:** New `ghostframe-test-pattern/src/palrle_exact.rs` module mirrors `solid_per_tile.rs`'s DRM-direct paint-once structure. Four 32×32 tiles at well-separated locations, each with a distinct 2-color shape (checkerboard / horizontal stripes / vertical stripes / 2×2 blocks). Single shared 2-color palette so a channel-swizzle bug fails across all 16 samples uniformly. E2E reads pixels back via the existing `__readPixel` hook.

**Tech Stack:** Rust (test-pattern + e2e). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-05-19-palrle-exact-pixels-design.md`

---

## File Structure

- `ghostframe-test-pattern/src/palrle_exact.rs` — NEW. Scene paint + `SAMPLES` constant + `run`. ~100 lines.
- `ghostframe-test-pattern/src/lib.rs` — add `pub mod palrle_exact;` (1 line).
- `ghostframe-test-pattern/src/main.rs` — add CLI flag + dispatch branch before existing `--drm-direct && solid_per_tile` branch. ~10 lines.
- `ghostframe-lib/tests/e2e.rs` — new `e2e_palrle_exact_pixels` test. ~35 lines.

---

## Task 1: Create `palrle_exact.rs` module

**Files:**
- Create: `ghostframe-test-pattern/src/palrle_exact.rs`
- Modify: `ghostframe-test-pattern/src/lib.rs`

- [ ] **Step 1: Write the module**

Create `/home/cedric/work/ghostframe/ghostframe-test-pattern/src/palrle_exact.rs` with exactly:

```rust
//! `--palrle-exact --drm-direct` mode.
//!
//! Paint-once scene with four 32×32 PalRle test tiles, each with a
//! distinct 2-color pattern. Drives `e2e_palrle_exact_pixels` (M3.2c
//! B1 follow-up). The four tiles are at tile coords (5,5), (10,5),
//! (5,10), (10,10) — well-separated so dirty detection treats each
//! independently. Background is dark grey (BG_PIXEL = 0x141414).
//!
//! ## Pixel format
//!
//! XRGB8888 little-endian: bytes in memory are `[B, G, R, X]`. Pack
//! via `u32::to_le_bytes()` on `0x00RR_GGBB` literals. X stays 0x00
//! (don't-care) per the project convention — the production
//! Solid/PalRle shaders pass X through to the framebuffer alpha
//! channel unchanged, so `samples()` reports `[R, G, B, 0x00]` per
//! sample point.
//!
//! ## Patterns
//!
//! Each tile uses the same 2-color palette: A = red (0xFF0000),
//! B = blue (0x0000FF). The per-pixel formula varies:
//!
//! | Tile (tile_x, tile_y) | Formula (px, py = 0..31 within tile)            |
//! |-----------------------|--------------------------------------------------|
//! | (5, 5)  checkerboard  | `if (px + py) % 2 == 0 { A } else { B }`         |
//! | (10, 5) hstripes      | `if py % 2 == 0 { A } else { B }`                |
//! | (5, 10) vstripes      | `if px % 2 == 0 { A } else { B }`                |
//! | (10, 10) 2×2 blocks   | `if (px/2 + py/2) % 2 == 0 { A } else { B }`     |

use std::thread;
use std::time::Duration;

use drm::control::Device as ControlDevice;

use crate::drm_direct::{msync_buffer, setup_dumb_scanout};

/// Dark-grey background. Packed XRGB8888 → bytes `[0x14, 0x14, 0x14, 0x00]`.
const BG_PIXEL: u32 = 0x0014_1414;

/// Test palette colour A: pure red. Packed XRGB8888 little-endian.
const COLOR_A: u32 = 0x00FF_0000;

/// Test palette colour B: pure blue. Packed XRGB8888 little-endian.
const COLOR_B: u32 = 0x0000_00FF;

const TILE: u32 = 32;
const T_CHECKER: (u32, u32) = (5, 5);
const T_HSTRIPES: (u32, u32) = (10, 5);
const T_VSTRIPES: (u32, u32) = (5, 10);
const T_BLOCKS2X2: (u32, u32) = (10, 10);

/// One classifier-PalRle sample location plus the expected RGBA value
/// the WebGPU client's `__readPixel` returns at that coordinate.
///
/// Expected RGBA = BGRA→RGBA swizzle of the wire bytes, alpha byte
/// passed through unchanged. The test pattern paints with X=0x00, so
/// the alpha component reads back as 0x00. (Canvas alphaMode='opaque'
/// makes this visually identical to alpha=0xFF.)
pub struct ExactSample {
    pub x: u32,
    pub y: u32,
    pub expected_rgba: [u8; 4],
}

const RGBA_RED: [u8; 4] = [0xFF, 0x00, 0x00, 0x00];
const RGBA_BLUE: [u8; 4] = [0x00, 0x00, 0xFF, 0x00];

/// Returns the 16 sample points the e2e asserts on. 4 samples per
/// tile × 4 tiles. Each tile slot resolves to absolute pixel coords
/// via `tile.x * TILE + px`.
pub fn samples() -> Vec<ExactSample> {
    let (cx, cy) = (T_CHECKER.0 * TILE, T_CHECKER.1 * TILE);
    let (hx, hy) = (T_HSTRIPES.0 * TILE, T_HSTRIPES.1 * TILE);
    let (vx, vy) = (T_VSTRIPES.0 * TILE, T_VSTRIPES.1 * TILE);
    let (bx, by) = (T_BLOCKS2X2.0 * TILE, T_BLOCKS2X2.1 * TILE);
    vec![
        // Checkerboard — (px+py)%2 == 0 → A.
        ExactSample { x: cx + 0, y: cy + 0, expected_rgba: RGBA_RED  }, // (0,0) even → A
        ExactSample { x: cx + 1, y: cy + 0, expected_rgba: RGBA_BLUE }, // (1,0) odd  → B
        ExactSample { x: cx + 0, y: cy + 1, expected_rgba: RGBA_BLUE }, // (0,1) odd  → B
        ExactSample { x: cx + 1, y: cy + 1, expected_rgba: RGBA_RED  }, // (1,1) even → A

        // Horizontal stripes — py%2 == 0 → A.
        ExactSample { x: hx + 0,  y: hy + 0, expected_rgba: RGBA_RED  }, // row 0 → A
        ExactSample { x: hx + 0,  y: hy + 1, expected_rgba: RGBA_BLUE }, // row 1 → B
        ExactSample { x: hx + 31, y: hy + 0, expected_rgba: RGBA_RED  }, // row 0 right end → A
        ExactSample { x: hx + 31, y: hy + 1, expected_rgba: RGBA_BLUE }, // row 1 right end → B

        // Vertical stripes — px%2 == 0 → A.
        ExactSample { x: vx + 0, y: vy + 0,  expected_rgba: RGBA_RED  }, // col 0 → A
        ExactSample { x: vx + 1, y: vy + 0,  expected_rgba: RGBA_BLUE }, // col 1 → B
        ExactSample { x: vx + 0, y: vy + 31, expected_rgba: RGBA_RED  }, // col 0 bottom → A
        ExactSample { x: vx + 1, y: vy + 31, expected_rgba: RGBA_BLUE }, // col 1 bottom → B

        // 2×2 blocks — (px/2 + py/2)%2 == 0 → A.
        ExactSample { x: bx + 0, y: by + 0, expected_rgba: RGBA_RED  }, // block (0,0) → A
        ExactSample { x: bx + 1, y: by + 0, expected_rgba: RGBA_RED  }, // still block (0,0) → A
        ExactSample { x: bx + 2, y: by + 0, expected_rgba: RGBA_BLUE }, // block (1,0) → B
        ExactSample { x: bx + 0, y: by + 2, expected_rgba: RGBA_BLUE }, // block (0,1) → B
    ]
}

/// Paint the scene once, then enter a keepalive loop that re-maps the
/// dumb buffer and msyncs without writing — the pattern is static.
pub fn run(card_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut scanout = setup_dumb_scanout(card_path)?;
    let width = scanout.mode_w;
    let height = scanout.mode_h;
    let pitch = scanout.pitch as usize;
    eprintln!(
        "palrle_exact: scanout {}x{} active — painting four PalRle test tiles",
        width, height
    );

    {
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer: {e}"))?;
        let bytes = map.as_mut();

        // Background fill — every active pixel set to BG_PIXEL.
        let bg_bytes = BG_PIXEL.to_le_bytes();
        let row_active = width as usize * 4;
        for y in 0..height as usize {
            let row_start = y * pitch;
            let row = &mut bytes[row_start..row_start + row_active];
            for chunk in row.chunks_exact_mut(4) {
                chunk.copy_from_slice(&bg_bytes);
            }
        }

        // Paint each test tile.
        paint_tile(bytes, pitch, width, height, T_CHECKER,   |px, py| (px + py) % 2 == 0);
        paint_tile(bytes, pitch, width, height, T_HSTRIPES,  |_px, py| py % 2 == 0);
        paint_tile(bytes, pitch, width, height, T_VSTRIPES,  |px, _py| px % 2 == 0);
        paint_tile(bytes, pitch, width, height, T_BLOCKS2X2, |px, py| (px / 2 + py / 2) % 2 == 0);

        msync_buffer(bytes);
    }

    eprintln!("palrle_exact: scene painted — entering keepalive loop");

    let keepalive = Duration::from_millis(33);
    loop {
        thread::sleep(keepalive);
        let mut map = scanout
            .card
            .map_dumb_buffer(&mut scanout.db)
            .map_err(|e| format!("map_dumb_buffer keepalive: {e}"))?;
        msync_buffer(map.as_mut());
    }
}

/// Paint a single 32×32 test tile at (tile_x*32, tile_y*32). `is_a`
/// returns true when the pixel at offset (px, py) within the tile
/// should be COLOR_A; false → COLOR_B.
fn paint_tile<F: Fn(u32, u32) -> bool>(
    bytes: &mut [u8],
    pitch: usize,
    width: u32,
    height: u32,
    tile: (u32, u32),
    is_a: F,
) {
    let a_bytes = COLOR_A.to_le_bytes();
    let b_bytes = COLOR_B.to_le_bytes();
    let tile_x0 = tile.0 * TILE;
    let tile_y0 = tile.1 * TILE;
    for py in 0..TILE {
        let ay = tile_y0 + py;
        if ay >= height {
            return;
        }
        for px in 0..TILE {
            let ax = tile_x0 + px;
            if ax >= width {
                continue;
            }
            let off = (ay as usize) * pitch + (ax as usize) * 4;
            let pixel = if is_a(px, py) { &a_bytes } else { &b_bytes };
            bytes[off..off + 4].copy_from_slice(pixel);
        }
    }
}
```

- [ ] **Step 2: Add the module to lib.rs**

Edit `/home/cedric/work/ghostframe/ghostframe-test-pattern/src/lib.rs`. The current file (post-2026-05-17) declares modules including `solid_per_tile`. Add the new module declaration in alphabetical order — between `mode_switch` (or wherever fits) and `solid_per_tile`:

```bash
grep -n "^pub mod" /home/cedric/work/ghostframe/ghostframe-test-pattern/src/lib.rs
```

Find the line `pub mod solid_per_tile;` and add immediately ABOVE it:

```rust
pub mod palrle_exact;
```

- [ ] **Step 3: Verify the test-pattern crate builds**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-test-pattern 2>&1 | tail -5
```

Expected: succeeds. The module's `pub` items (`ExactSample`, `samples`, `run`) are reachable; the helper `paint_tile` is private (not used outside the module).

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-test-pattern/src/palrle_exact.rs ghostframe-test-pattern/src/lib.rs
git commit -m "$(cat <<'EOF'
feat(test-pattern): palrle_exact module — four 2-color PalRle tiles

Paint-once scene with four 32×32 tiles at non-adjacent coords:
  - (5, 5)  checkerboard A/B at 1px
  - (10, 5) horizontal stripes
  - (5, 10) vertical stripes
  - (10, 10) 2×2 blocks

Shared 2-color palette (red/blue) so channel-swizzle bugs fail
across all samples uniformly. samples() returns 16 (x, y,
expected_rgba) tuples for the upcoming e2e_palrle_exact_pixels.

Task 2 wires the CLI flag; Task 3 adds the e2e.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 2: Wire `--palrle-exact` CLI flag

**Files:**
- Modify: `ghostframe-test-pattern/src/main.rs`

- [ ] **Step 1: Add the clap flag**

Find the `Args` struct (around line 50-65). Find the existing `solid_per_tile: bool` field:

```bash
grep -nA2 "solid_per_tile: bool" /home/cedric/work/ghostframe/ghostframe-test-pattern/src/main.rs
```

Add immediately AFTER the `solid_per_tile` field's closing line:

```rust
    /// Paint a four-tile PalRle test scene (checkerboard / horizontal
    /// stripes / vertical stripes / 2×2 blocks) with a known 2-color
    /// palette. Combine with `--drm-direct` to paint to the DRM
    /// dumb-buffer scanout. Drives `e2e_palrle_exact_pixels`.
    #[arg(long)]
    palrle_exact: bool,
```

- [ ] **Step 2: Add the dispatch branch**

Find the existing `if args.drm_direct && args.solid_per_tile { ... }` branch (around line 83):

```bash
grep -nB1 -A2 "drm_direct && args.solid_per_tile" /home/cedric/work/ghostframe/ghostframe-test-pattern/src/main.rs
```

Add a new dispatch branch immediately BEFORE the `solid_per_tile` one:

```rust
    // DRM-direct palrle-exact mode: paint four 32×32 PalRle test tiles
    // with known 2-color patterns. Drives `e2e_palrle_exact_pixels`
    // (M3.2c B1 follow-up).
    if args.drm_direct && args.palrle_exact {
        return ghostframe_test_pattern::palrle_exact::run(&args.drm_device);
    }
```

Order matters here because `--drm-direct` alone (without sub-mode) also has a branch later that requires `--mode-switch-cycle` — the early `&&` dispatch branches must come before that one. Mirror the existing solid_per_tile placement (right above it in the dispatch chain).

- [ ] **Step 3: Verify CLI parses + binary builds**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build -p ghostframe-test-pattern 2>&1 | tail -3
cd /home/cedric/work/ghostframe && cargo run -p ghostframe-test-pattern -- --help 2>&1 | grep -i palrle-exact
```

Expected: build succeeds; `--help` lists the new flag with its description.

- [ ] **Step 4: Commit**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-test-pattern/src/main.rs
git commit -m "$(cat <<'EOF'
feat(test-pattern): --palrle-exact --drm-direct CLI flag

Wires the new palrle_exact module (Task 1) into the test-pattern CLI.
Dispatch branch placed immediately before the existing
solid_per_tile branch, matching the precedence order.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 3: Add `e2e_palrle_exact_pixels` test

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

- [ ] **Step 1: Find a good location for the new test**

```bash
grep -n "fn e2e_palrle_5pct_loss\|fn e2e_solid_per_tile_pixels\|fn e2e_palrle_oob_index" /home/cedric/work/ghostframe/ghostframe-lib/tests/e2e.rs
```

The new test fits next to the other PalRle and solid_per_tile tests. Place it right after `e2e_solid_per_tile_pixels` (the most similar test in shape — same setup helper, similar SAMPLES-driven assertion).

- [ ] **Step 2: Add the test**

Insert this verbatim after the closing `}` of `e2e_solid_per_tile_pixels`:

```rust
/// W3/B1 — Exact-pixel verification of the PalRLE compute shader.
///
/// Drives `--palrle-exact --drm-direct`'s four 32×32 PalRle tiles
/// (checkerboard / horizontal stripes / vertical stripes / 2×2 blocks,
/// all sharing a 2-color red/blue palette) and asserts exact RGBA at
/// 16 sample points. Catches nibble-swap, per-pixel arithmetic,
/// BGRA→RGBA swizzle, and tile-coord bugs that the existing PalRle
/// tests (e2e_palrle_5pct_loss, e2e_text_clarity, e2e_palrle_oob_index)
/// don't surface under text-luminance or codec-classification checks.
///
/// Closes M3.2c B1 follow-up.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_palrle_exact_pixels() -> Result<()> {
    use ghostframe_test_pattern::palrle_exact::samples;

    let setup = setup_e2e_webgpu_gpu("--palrle-exact --drm-direct").await?;
    // 5s covers QUIC slow-start + first-frame H.264 phase + classifier
    // transition to PalRle for the four 2-color test tiles.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Sanity: PalRle codec (wire enum 2) must appear on the wire.
    let codecs: Vec<u8> = setup
        .page
        .evaluate("window.__ghostframeRecordedCodecs || []")
        .await?
        .into_value()?;
    assert!(
        codecs.contains(&2u8),
        "expected Codec::PalRle (2); saw codecs: {:?}",
        codecs
    );

    // Exact-pixel assertions across all four test tiles.
    for sample in samples() {
        let probe = format!("window.__readPixel({}, {})", sample.x, sample.y);
        let got: Vec<u8> = setup.page.evaluate(probe.as_str()).await?.into_value()?;
        assert_eq!(
            got,
            sample.expected_rgba.to_vec(),
            "pixel ({}, {}) mismatch: got {:?}, expected {:?}",
            sample.x, sample.y, got, sample.expected_rgba
        );
    }

    Ok(())
}
```

- [ ] **Step 3: Verify the test compiles**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo build --tests -p ghostframe-lib 2>&1 | tail -5
```

Expected: build succeeds. The `use ghostframe_test_pattern::palrle_exact::samples;` import resolves because the lib.rs declared the module pub in Task 1.

- [ ] **Step 4: Rebuild the test-server container**

The test-pattern binary needs to land in the container image.

```
cd /home/cedric/work/ghostframe && docker build -t ghostframe/test-server:latest -f tests/containers/test-server/Dockerfile . 2>&1 | tail -3
```

Expected: "Successfully tagged ghostframe/test-server:latest". If "no space left on device", run `docker system prune -a -f` first.

- [ ] **Step 5: Run the test in isolation**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e e2e_palrle_exact_pixels -- --test-threads=1 --nocapture 2>&1 | tee /tmp/palrle_exact.log | tail -15
```

Expected: 1 passed; 0 failed.

If FAIL:
- **Codec assertion fails** (`expected Codec::PalRle (2); saw codecs: [...]`): the four test tiles classified as Bc1 or H.264 instead. Most likely cause: classifier never transitioned to PalRle within 5s. Bump the wait to 7s and retry. If still failing, the tiles' unique-color count may have drifted — inspect server logs for `palrle.frame stats` to see `unique_frame_palettes` counts.
- **Specific samples mismatch**: the failure message includes the sample coord, got, and expected. Patterns:
  - `got: [0, 0, 0xFF, 0]` where expected `[0xFF, 0, 0, 0]` (R↔B inverted) across ALL 16 → channel swizzle regression.
  - Adjacent samples swap (e.g., `(160, 160)=BLUE` instead of RED, `(161, 160)=RED` instead of BLUE) on the checkerboard tile → nibble unpack high↔low swap.
  - One whole tile's samples all return `[0x14, 0x14, 0x14, 0]` (BG color) → that tile didn't get painted or didn't render. Check `palrle.frame` server logs for emission of that tile coord.
  - Random scattered mismatches → likely a timing race; bump wait and retry.

- [ ] **Step 6: Commit (only if Step 5 passes)**

```bash
cd /home/cedric/work/ghostframe
git add ghostframe-lib/tests/e2e.rs
git commit -m "$(cat <<'EOF'
test(e2e): exact-pixel verification of PalRLE compute shader (B1)

e2e_palrle_exact_pixels drives the new --palrle-exact --drm-direct
test pattern and asserts 16 sample pixels across four PalRle tiles
with distinct 2-color shapes. Closes M3.2c B1 follow-up — last
unshipped item from the original M3.2c milestone plan.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>
EOF
)"
```

---

## Task 4: Suite regression + memory update

**Files:**
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`
- Modify: `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md`

- [ ] **Step 1: Run the full e2e suite for regression**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --test e2e -- --test-threads=1 2>&1 | tee /tmp/palrle_exact_full.log | tail -5
```

Expected: **24 passed, 0 failed, 2 ignored** (was 23/0/2 — the new test adds one to the pass count).

If a previously-passing test regresses, investigate the failing test's panic before committing memory updates. Most likely candidates: nothing — the new test pattern doesn't share state with any existing test.

- [ ] **Step 2: Run lib unit tests + vitest sanity**

```
cd /home/cedric/work/ghostframe && CARGO_INCREMENTAL=0 cargo test -p ghostframe-lib --lib 2>&1 | tail -3
cd /home/cedric/work/ghostframe/ghostframe-web-client && npm test 2>&1 | tail -3
```

Expected: 282 lib tests passed (no change — no new lib tests); 27 vitest tests passed (no change — no client changes).

- [ ] **Step 3: Update `project_m32c_near_complete.md`**

In `~/.claude/projects/-home-cedric-work-ghostframe/memory/project_m32c_near_complete.md`:

Find the suite-status line:
```
`cargo test -p ghostframe-lib --test e2e -- --test-threads=1`: **23 passed, 0 failed, 2 ignored**. (Count unchanged from 2026-05-18; `e2e_indices_raw_handshake` gained a wire-level indices_raw assertion on 2026-05-19 — see B2 closure note below.)
```

Replace with:
```
`cargo test -p ghostframe-lib --test e2e -- --test-threads=1`: **24 passed, 0 failed, 2 ignored**. (Was 23/0/2 — `e2e_palrle_exact_pixels` (B1) landed 2026-05-19.)
```

In the "Remaining for full M3.2c closure" section, remove the `palrle_exact` bullet entirely (it's now closed). The section may now be empty or only have a closing note; if empty, replace the section body with:

```
**All M3.2c original-plan items now closed.** B1 (palrle_exact exact-pixel verification) shipped 2026-05-19 — see commits `<task 1 sha>` (test pattern), `<task 2 sha>` (CLI), `<task 3 sha>` (e2e).
```

Fill in commit SHAs from `git log --oneline -5` after Tasks 1-3 land.

- [ ] **Step 4: Update MEMORY.md index line**

In `~/.claude/projects/-home-cedric-work-ghostframe/memory/MEMORY.md`, find the project_m32c line:

```
- [M3.2c complete (all spec items closed)](project_m32c_near_complete.md) — 2026-05-18/19: B3 + B7 + W4 + W5 + A5/B6 + B2 all landed (latent prod bugs (on_session_reset gate + LRU thrashing) closed Mon; B2 wire-level indices_raw assertion landed Tue); 23 e2e pass, 2 ignored (palrle_session_reset M3.5 scope, resolution_change pre-existing)
```

Replace with:

```
- [M3.2c complete (all original-plan items closed)](project_m32c_near_complete.md) — 2026-05-18/19: B1 + B2 + B3 + B7 + W4 + W5 + A5/B6 all landed. 24 e2e pass, 2 ignored (palrle_session_reset M3.5 scope, resolution_change pre-existing).
```

- [ ] **Step 5: Final commit check**

```bash
cd /home/cedric/work/ghostframe
git status --short
```

Memory files (`~/.claude/projects/...`) are NOT under git — no commit needed for them. If the project tree is clean (modulo the pre-existing `.claude/`, `librust_out.rlib`, `tmux-client-25660.log` untracked entries), the plan is complete.

If something project-side remains uncommitted (shouldn't be the case at this point), commit with a `chore: cleanup` message.

---

## Notes on parallelization

Task 1 must complete before Task 2 (Task 2 imports `palrle_exact::run`). Task 2 must complete before Task 3 (Task 3's setup helper invokes `--palrle-exact --drm-direct`, which needs the CLI dispatch wired). Task 4 runs last. No parallelism opportunities; 4 sequential dispatches in subagent-driven execution.

## Contingency

The only failure mode the plan anticipates is Task 3 Step 5's assertion failures. Inline diagnostics are in the step description. If a specific bug class is uncovered (e.g., channel swap that's been latent in the WGSL shader), that's a real find — STOP and report; do NOT commit the test until the production fix lands or the test is `#[ignore]`'d with the diagnosis-based rationale.
