# e2e_text_clarity Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate that monospace text captured on the X server appears legibly on the browser canvas. Pre-M3 (H.264 only) the assertion is per-pixel contrast between known glyph "ink" and "background" positions, plus temporal stability. Post-M3 the same harness tightens to the spec's `SSIM > 0.99 after 5s idle` (build-to-lossless) without rewriting test infrastructure.

**Architecture:** Extend `ghostframe-test-pattern` with a `--text-grid` mode that renders a fixed string at known pixel coordinates using a hand-bundled 6×8 monospace font (no font crate, no fontconfig dependency). The e2e test consumes the same coordinate constants and asserts contrast at sampled positions. A future M3 task replaces the contrast assertion with an SSIM check against a deterministic reference PNG.

**Tech Stack:** Rust, `x11rb` (already a test-pattern dep), `chromiumoxide` page evaluation (existing e2e harness), `serde_json` (existing dev-dep) for sampled-pixel readback.

---

## File Structure

```
ghostframe-test-pattern/src/
├── main.rs                 # add --text-grid arg, wire to render
├── font.rs                 # NEW — 6×8 glyph table, constant
└── text_grid.rs            # NEW — render routine + exported coord constants

ghostframe-lib/tests/
└── e2e.rs                  # add e2e_text_clarity
```

`text_grid.rs` is intentionally inside `ghostframe-test-pattern` so the *same* constants the renderer uses are also imported by the test — single source of truth for ink/bg pixel coordinates.

---

## Task 1: Hand-bundled 6×8 font

**Files:**
- Create: `ghostframe-test-pattern/src/font.rs`
- Modify: `ghostframe-test-pattern/src/main.rs` (add `mod font;`)

- [ ] **Step 1: Write the font module**

Create `ghostframe-test-pattern/src/font.rs`:

```rust
//! Hand-rolled 6×8 monospace bitmap font, just enough characters for the
//! `--text-grid` test pattern. Each glyph is 8 rows of u8; bit 5 is the
//! left-most pixel ("ink"), bit 0 is the right-most pixel.

pub const GLYPH_W: u32 = 6;
pub const GLYPH_H: u32 = 8;

pub fn glyph(c: char) -> &'static [u8; 8] {
    match c {
        ' ' => &[0,0,0,0,0,0,0,0],
        '/' => &[
            0b000001,
            0b000010,
            0b000010,
            0b000100,
            0b000100,
            0b001000,
            0b010000,
            0b000000,
        ],
        'A' => &[
            0b001100,
            0b010010,
            0b100001,
            0b100001,
            0b111111,
            0b100001,
            0b100001,
            0b000000,
        ],
        'E' => &[
            0b111111,
            0b100000,
            0b100000,
            0b111110,
            0b100000,
            0b100000,
            0b111111,
            0b000000,
        ],
        'F' => &[
            0b111111,
            0b100000,
            0b100000,
            0b111110,
            0b100000,
            0b100000,
            0b100000,
            0b000000,
        ],
        'G' => &[
            0b011110,
            0b100001,
            0b100000,
            0b100111,
            0b100001,
            0b100001,
            0b011110,
            0b000000,
        ],
        'H' => &[
            0b100001,
            0b100001,
            0b100001,
            0b111111,
            0b100001,
            0b100001,
            0b100001,
            0b000000,
        ],
        'I' => &[
            0b011110,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b011110,
            0b000000,
        ],
        'L' => &[
            0b100000,
            0b100000,
            0b100000,
            0b100000,
            0b100000,
            0b100000,
            0b111111,
            0b000000,
        ],
        'M' => &[
            0b100001,
            0b110011,
            0b101101,
            0b100001,
            0b100001,
            0b100001,
            0b100001,
            0b000000,
        ],
        'O' => &[
            0b011110,
            0b100001,
            0b100001,
            0b100001,
            0b100001,
            0b100001,
            0b011110,
            0b000000,
        ],
        'R' => &[
            0b111110,
            0b100001,
            0b100001,
            0b111110,
            0b100100,
            0b100010,
            0b100001,
            0b000000,
        ],
        'S' => &[
            0b011111,
            0b100000,
            0b100000,
            0b011110,
            0b000001,
            0b000001,
            0b111110,
            0b000000,
        ],
        'T' => &[
            0b111111,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b001100,
            0b000000,
        ],
        // Anything we didn't ship a glyph for renders as a solid block so a
        // typo in the test string fails loudly rather than silently blanking.
        _ => &[
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b111111,
            0b000000,
        ],
    }
}

/// Returns true if pixel (px, py) inside a `GLYPH_W × GLYPH_H` glyph box is "ink".
pub fn pixel_set(c: char, px: u32, py: u32) -> bool {
    if px >= GLYPH_W || py >= GLYPH_H {
        return false;
    }
    let row = glyph(c)[py as usize];
    let bit = 1u8 << (GLYPH_W - 1 - px);
    (row & bit) != 0
}
```

- [ ] **Step 2: Wire into main.rs**

Edit `ghostframe-test-pattern/src/main.rs`. Add at the top, after the existing `use` block:

```rust
mod font;
mod text_grid;
```

(The `text_grid` module lands in Task 2 — adding the declaration now keeps the diff tidy.)

Stub the missing module so the build still passes after this commit:

Create `ghostframe-test-pattern/src/text_grid.rs` with a minimal placeholder:

```rust
//! Stub — populated in Task 2.
```

- [ ] **Step 3: Verify the test-pattern crate still builds**

Run: `cargo build -p ghostframe-test-pattern`
Expected: clean build.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-test-pattern/src/font.rs ghostframe-test-pattern/src/text_grid.rs ghostframe-test-pattern/src/main.rs
git commit -m "feat(test-pattern): hand-bundled 6x8 bitmap font for text-grid mode"
```

---

## Task 2: text_grid render routine + exported coordinates

**Files:**
- Modify: `ghostframe-test-pattern/src/text_grid.rs`

- [ ] **Step 1: Replace the stub with the renderer + constants**

Overwrite `ghostframe-test-pattern/src/text_grid.rs`:

```rust
//! `--text-grid` pattern: paint a known ASCII string at known coordinates.
//!
//! The rendered text and pixel coords are part of the public surface of this
//! module so the e2e test imports them directly — there is exactly one source
//! of truth for "where does ink land on the X server".

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

use crate::font::{pixel_set, GLYPH_H, GLYPH_W};

/// Background colour painted on the root before drawing glyphs.
pub const BG_PIXEL: u32 = 0x0014_1414; // near-black RGB(0x14,0x14,0x14)
/// Glyph "ink" colour.
pub const FG_PIXEL: u32 = 0x00F5_F5F5; // near-white RGB(0xF5,0xF5,0xF5)

/// Top-left of the rendered text block, in root-window pixels.
pub const ORIGIN_X: u32 = 80;
pub const ORIGIN_Y: u32 = 100;

/// String drawn on the root window. Letters chosen to fit the bundled font.
pub const TEXT: &str = "GHOSTFRAME / TILE / TEST";

/// Pixels of inter-character spacing (no spacing between glyph cells —
/// the font itself includes a 1-px gutter on the right of each glyph).
pub const CHAR_SPACING: u32 = 0;

/// Returns the (x, y) of the top-left of the n-th character in TEXT.
pub fn char_origin(n: usize) -> (u32, u32) {
    let x = ORIGIN_X + (n as u32) * (GLYPH_W + CHAR_SPACING);
    (x, ORIGIN_Y)
}

/// Returns the absolute pixel coordinate of glyph-local pixel (px, py)
/// inside the n-th character of TEXT.
pub fn glyph_pixel(n: usize, px: u32, py: u32) -> (u32, u32) {
    let (ox, oy) = char_origin(n);
    (ox + px, oy + py)
}

/// Hand-picked sample positions. The e2e test asserts contrast across these
/// pairs: each ink pixel paired with a same-row background pixel.
pub struct SamplePair {
    pub ink: (u32, u32),
    pub bg: (u32, u32),
}

/// 4 ink/bg sample pairs spread across the rendered string. Hard-coded
/// rather than computed so a regression in the font table or origin is
/// caught here, not papered over.
///
/// Positions chosen on this string ("GHOSTFRAME / TILE / TEST"):
///   index 0 = 'G'  — top-left bowl of the G is ink at glyph-px (1,0)
///   index 6 = 'A'  — apex pixel at glyph-px (2,1)
///   index 14 = 'T' — crossbar at glyph-px (1,0)
///   index 20 = 'T' — same letter, different position
pub const SAMPLES: &[SamplePair] = &[
    SamplePair {
        ink: (ORIGIN_X + 1,  ORIGIN_Y + 0),
        bg:  (ORIGIN_X + 0,  ORIGIN_Y + 0),
    },
    SamplePair {
        ink: (ORIGIN_X + 6 * GLYPH_W + 2, ORIGIN_Y + 1),
        bg:  (ORIGIN_X + 6 * GLYPH_W + 0, ORIGIN_Y + 1),
    },
    SamplePair {
        ink: (ORIGIN_X + 14 * GLYPH_W + 1, ORIGIN_Y + 0),
        bg:  (ORIGIN_X + 14 * GLYPH_W + 0, ORIGIN_Y + 1),
    },
    SamplePair {
        ink: (ORIGIN_X + 20 * GLYPH_W + 1, ORIGIN_Y + 0),
        bg:  (ORIGIN_X + 20 * GLYPH_W + 0, ORIGIN_Y + 1),
    },
];

/// Paint the root window background and glyphs. Returns once the X server
/// has flushed every operation.
pub fn render<C: Connection>(conn: &C, root: u32) -> Result<(), Box<dyn std::error::Error>> {
    // 1. Background.
    conn.change_window_attributes(
        root,
        &ChangeWindowAttributesAux::new().background_pixel(BG_PIXEL),
    )?;
    conn.clear_area(false, root, 0, 0, 0, 0)?;

    // 2. Foreground GC.
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::new().foreground(FG_PIXEL))?;

    // 3. Build a list of rectangles to fill — one per ink pixel.
    let mut rects: Vec<Rectangle> = Vec::with_capacity(TEXT.len() * 24);
    for (n, ch) in TEXT.chars().enumerate() {
        for py in 0..GLYPH_H {
            for px in 0..GLYPH_W {
                if pixel_set(ch, px, py) {
                    let (ax, ay) = glyph_pixel(n, px, py);
                    rects.push(Rectangle {
                        x: ax as i16,
                        y: ay as i16,
                        width: 1,
                        height: 1,
                    });
                }
            }
        }
    }

    // 4. Issue the fills in a single request.
    conn.poly_fill_rectangle(root, gc, &rects)?;
    conn.flush()?;

    eprintln!("test-pattern: text-grid rendered ({} ink pixels)", rects.len());
    Ok(())
}
```

- [ ] **Step 2: Verify text_grid compiles**

Run: `cargo build -p ghostframe-test-pattern`
Expected: clean build.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-test-pattern/src/text_grid.rs
git commit -m "feat(test-pattern): text-grid render + exported sample coords"
```

---

## Task 3: Wire `--text-grid` into the test-pattern CLI

**Files:**
- Modify: `ghostframe-test-pattern/src/main.rs`

- [ ] **Step 1: Add the CLI flag and dispatch**

Find the `Args` struct and add the new field:

```rust
#[derive(Parser)]
struct Args {
    /// Fill the root window with solid red.
    #[arg(long)]
    solid_red: bool,

    /// Draw a cycling-color region that changes every frame (simulates motion).
    #[arg(long)]
    spinner: bool,

    /// Draw a known monospace string at fixed coordinates for the e2e
    /// text-clarity test. Mutually compatible with --solid-red (text-grid
    /// is rendered last, after solid-red has cleared the background).
    #[arg(long)]
    text_grid: bool,

    /// (Ignored — kept for CLI compat.) Window width.
    #[arg(long, default_value = "640")]
    width: u16,

    /// (Ignored — kept for CLI compat.) Window height.
    #[arg(long, default_value = "480")]
    height: u16,
}
```

Then, just before the `if args.spinner` block in `main`, add:

```rust
    if args.text_grid {
        text_grid::render(&conn, root)?;
    }
```

The full updated main.rs ordering becomes: solid_red → text_grid → spinner → wait_for_event.

- [ ] **Step 2: Run the binary against an Xvfb to smoke-test**

Run: `Xvfb :88 -screen 0 800x600x24 &` then `DISPLAY=:88 cargo run -p ghostframe-test-pattern -- --text-grid`
Expected: prints `test-pattern: text-grid rendered (...)`. Process stays alive (waiting for events). Kill it with Ctrl-C.

If Xvfb isn't installed locally, skip this step — CI will catch any breakage.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-test-pattern/src/main.rs
git commit -m "feat(test-pattern): --text-grid flag dispatching to text_grid::render"
```

---

## Task 4: Rebuild test-server container

**Files:**
- (no source changes; this task documents the container refresh)

- [ ] **Step 1: Rebuild the test-server image**

The test-pattern binary is baked into `ghostframe/test-server`. Rebuild:

Run: `docker build -f tests/containers/test-server/Dockerfile -t ghostframe/test-server:latest .`
Expected: image rebuilt without errors. Look for "Successfully tagged ghostframe/test-server:latest".

- [ ] **Step 2: Verify the new flag is present**

Run: `docker run --rm --entrypoint /usr/local/bin/ghostframe-test-pattern ghostframe/test-server:latest --help`
Expected: usage string includes `--text-grid`.

- [ ] **Step 3: Commit (no source change — note in the next task's commit)**

Skip — there is no source change for this task. The next commit (Task 5) will reference container rebuild in its message body.

---

## Task 5: Expose text_grid coords from the test-pattern crate

**Files:**
- Modify: `ghostframe-test-pattern/Cargo.toml`
- Modify: `ghostframe-test-pattern/src/main.rs`
- Create: `ghostframe-test-pattern/src/lib.rs`

To let `ghostframe-lib` integration tests import `SAMPLES`, the test-pattern crate needs a library crate alongside its binary.

- [ ] **Step 1: Convert the crate to bin+lib**

Edit `ghostframe-test-pattern/Cargo.toml`. Insert before `[dependencies]`:

```toml
[lib]
name = "ghostframe_test_pattern"
path = "src/lib.rs"

[[bin]]
name = "ghostframe-test-pattern"
path = "src/main.rs"
```

- [ ] **Step 2: Create the library entrypoint**

Create `ghostframe-test-pattern/src/lib.rs`:

```rust
//! Library half of `ghostframe-test-pattern`.
//!
//! Re-exports the modules the e2e tests need to import (font tables, text-grid
//! coordinates). The binary CLI lives in `main.rs`.

pub mod font;
pub mod text_grid;
```

- [ ] **Step 3: Update `main.rs` to consume the library modules**

Edit `ghostframe-test-pattern/src/main.rs`. Replace the existing `mod font;` and `mod text_grid;` declarations with:

```rust
use ghostframe_test_pattern::{font, text_grid};
```

Cargo will figure out the cross-bin/lib reference automatically because both targets share the same `Cargo.toml`.

- [ ] **Step 4: Verify the binary still builds**

Run: `cargo build -p ghostframe-test-pattern`
Expected: clean build of both lib and bin targets.

- [ ] **Step 5: Add a dev-dependency from ghostframe-lib onto the lib half**

Edit `ghostframe-lib/Cargo.toml`. Append to `[dev-dependencies]`:

```toml
ghostframe-test-pattern = { path = "../ghostframe-test-pattern" }
```

- [ ] **Step 6: Verify the lib still builds**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build.

- [ ] **Step 7: Commit**

```bash
git add ghostframe-test-pattern/Cargo.toml ghostframe-test-pattern/src/lib.rs ghostframe-test-pattern/src/main.rs ghostframe-lib/Cargo.toml
git commit -m "refactor(test-pattern): expose font + text_grid via lib crate for e2e import

Container image must be rebuilt: docker build -f tests/containers/test-server/Dockerfile -t ghostframe/test-server:latest ."
```

---

## Task 6: Write the e2e_text_clarity test

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

- [ ] **Step 1: Add the test function**

Append to `ghostframe-lib/tests/e2e.rs`, after the existing `e2e_codec_transition` test:

```rust
/// Pre-M3: validate text legibility via per-pixel contrast at known glyph
/// positions. Post-M3 (CDF 5/3 refinement) this test should be tightened
/// to assert SSIM > 0.99 against a reference PNG; see TODO below.
#[tokio::test]
async fn e2e_text_clarity() -> Result<()> {
    use ghostframe_test_pattern::text_grid::SAMPLES;

    let setup = setup_e2e("--text-grid").await?;

    // Allow QUIC slow-start + a couple of frames so every glyph tile arrives.
    tokio::time::sleep(Duration::from_secs(6)).await;

    // ── (a) Per-pair contrast: ink position must be much brighter than bg.
    for (i, pair) in SAMPLES.iter().enumerate() {
        let probe_js = format!(
            r#"
            (() => {{
                const canvas = document.getElementById('canvas');
                const ctx = canvas.getContext('2d');
                const ink = ctx.getImageData({ix}, {iy}, 1, 1).data;
                const bg  = ctx.getImageData({bx}, {by}, 1, 1).data;
                return {{
                    ink: {{ r: ink[0], g: ink[1], b: ink[2] }},
                    bg:  {{ r: bg[0],  g: bg[1],  b: bg[2]  }},
                }};
            }})()
            "#,
            ix = pair.ink.0, iy = pair.ink.1,
            bx = pair.bg.0,  by = pair.bg.1,
        );
        let probe: serde_json::Value = setup.page.evaluate(probe_js.as_str()).await?.into_value()?;

        let ink_lum = luminance(&probe["ink"]);
        let bg_lum  = luminance(&probe["bg"]);

        assert!(
            ink_lum - bg_lum > 80.0,
            "sample {i}: insufficient contrast (ink {ink_lum:.0} - bg {bg_lum:.0}); pair={pair:?}"
        );
        assert!(ink_lum > 150.0, "sample {i}: ink too dark — luminance {ink_lum:.0}");
        assert!(bg_lum  <  80.0, "sample {i}: bg too bright — luminance {bg_lum:.0}");
    }

    // ── (b) Stability: two snapshots 2s apart must be byte-identical.
    let hash_js = r#"
        (() => {
            const canvas = document.getElementById('canvas');
            const ctx = canvas.getContext('2d');
            const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
            let h = 0;
            for (let i = 0; i < data.length; i++) h = ((h << 5) - h + data[i]) | 0;
            return h;
        })()
    "#;
    let h1: i64 = setup.page.evaluate(hash_js).await?.into_value()?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    let h2: i64 = setup.page.evaluate(hash_js).await?.into_value()?;
    assert_eq!(h1, h2, "text canvas drifted between snapshots");

    // TODO(M3): replace the contrast check with SSIM > 0.99 against
    //           tests/fixtures/text_grid_reference.png once CDF 5/3
    //           refinement is wired and lossless reconstruction works.

    Ok(())
}

fn luminance(c: &serde_json::Value) -> f64 {
    let r = c["r"].as_f64().unwrap_or(0.0);
    let g = c["g"].as_f64().unwrap_or(0.0);
    let b = c["b"].as_f64().unwrap_or(0.0);
    // Rec. 709 luma — close enough for "is this pixel ink or bg".
    0.2126 * r + 0.7152 * g + 0.0722 * b
}
```

- [ ] **Step 2: Confirm the imports compile**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build. If `ghostframe_test_pattern::text_grid::SAMPLES` doesn't resolve, re-check Task 5 step 5.

- [ ] **Step 3: Run the test (requires a rebuilt test-server image)**

Run: `cargo test -p ghostframe-lib --test e2e e2e_text_clarity -- --test-threads=1 --nocapture`
Expected: PASS within ~30 s.

If the contrast assertion fails, capture a screenshot for diagnosis. Add this code temporarily inside the test body just before the failing assertion:

```rust
let png = setup.page.screenshot(chromiumoxide::page::ScreenshotParams::default()).await?;
std::fs::write("/tmp/e2e_text_clarity_fail.png", &png)?;
eprintln!("screenshot saved to /tmp/e2e_text_clarity_fail.png");
```

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): e2e_text_clarity asserts contrast at known glyph positions

Pre-M3 assertion is per-pixel contrast. M3 follow-up tightens to SSIM>0.99
against a reference PNG once CDF 5/3 refinement lands."
```

---

## Final verification

- [ ] **Step 1: Workspace build**

Run: `cargo build --workspace --tests`
Expected: clean build across all crates including the new lib half of test-pattern.

- [ ] **Step 2: Run all e2e tests serially**

Run: `cargo test -p ghostframe-lib --test e2e -- --test-threads=1`
Expected: every prior e2e test still passes plus the new `e2e_text_clarity`.

- [ ] **Step 3: Confirm container is rebuilt**

If skipped earlier:

Run: `docker build -f tests/containers/test-server/Dockerfile -t ghostframe/test-server:latest .`
Expected: image up-to-date.
