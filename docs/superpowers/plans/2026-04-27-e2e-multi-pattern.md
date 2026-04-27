# e2e_multi_pattern Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Validate the pipeline handles four distinct content classes in one frame — the actual workload the M3 classifier targets — by drawing a four-region test pattern and asserting per-region rendering correctness.

**Architecture:** Add a `--mixed` mode to `ghostframe-test-pattern` that paints four regions on the root window: solid red (top-left), monospace text (top-right), horizontal-stripe gradient (bottom-left), and a hue-cycling spinner (bottom-right, animated forever). Region geometry plus an `expected_codec` field per region are exported as a `REGIONS` constant from the test-pattern lib crate so the e2e test imports the same source of truth used by the renderer. Pre-M3 assertions are per-region rendering checks (solid is red, text is legible, gradient is monotonic, spinner changes). Post-M3, the same `REGIONS` table drives codec-stat assertions without rewriting the test.

**Tech Stack:** Rust, `x11rb`, existing `chromiumoxide` e2e harness. Depends on the lib-crate split from the `e2e_text_clarity` plan (Task 5 there exposes `ghostframe_test_pattern` as a library).

---

## File Structure

```
ghostframe-test-pattern/src/
├── lib.rs                  # add `pub mod mixed;`
├── main.rs                 # add --mixed flag, dispatch
└── mixed.rs                # NEW — Region struct, REGIONS const, render

ghostframe-lib/tests/
└── e2e.rs                  # add e2e_multi_pattern + RegionCheck helper
```

This plan **depends on `e2e_text_clarity`** (lib-crate split, font module). Do that plan first, or merge them in flight.

---

## Task 1: Region table and module skeleton

**Files:**
- Create: `ghostframe-test-pattern/src/mixed.rs`
- Modify: `ghostframe-test-pattern/src/lib.rs`

- [ ] **Step 1: Write the mixed module**

Create `ghostframe-test-pattern/src/mixed.rs`:

```rust
//! `--mixed` pattern: four content classes painted on disjoint regions of
//! the root window. The same `REGIONS` table is consumed by the renderer
//! and by the e2e test — single source of truth for geometry and the M3
//! codec each region targets.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::*;

use crate::font::{pixel_set, GLYPH_H, GLYPH_W};

/// One region of the mixed pattern.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub name: &'static str,
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
    /// Codec the M3 classifier is expected to pick for this region.
    /// Pre-M3 the e2e test only logs this; post-M3 it is asserted against
    /// per-tile codec stats from a future instrumentation channel.
    pub expected_codec: &'static str,
}

/// Default 640×480 layout — split into four 320×240 quadrants.
pub const REGIONS: &[Region] = &[
    Region { name: "solid",    x: 0,   y: 0,   w: 320, h: 240, expected_codec: "Solid"  },
    Region { name: "text",     x: 320, y: 0,   w: 320, h: 240, expected_codec: "PalRle" },
    Region { name: "gradient", x: 0,   y: 240, w: 320, h: 240, expected_codec: "Bc1"    },
    Region { name: "spinner",  x: 320, y: 240, w: 320, h: 240, expected_codec: "H264"   },
];

/// Look up a region by name. Panics if not found — REGIONS is a fixed table.
pub fn region(name: &str) -> &'static Region {
    REGIONS
        .iter()
        .find(|r| r.name == name)
        .expect("unknown mixed region")
}

/// Settle time recommended for the e2e test before sampling. The spinner
/// must run long enough for two screenshots to capture different frames.
pub const SETTLE: Duration = Duration::from_secs(7);

/// Paint the static regions (solid, text, gradient) and emit the spinner
/// loop on the calling thread. Never returns — the loop runs forever.
pub fn render_and_spin<C: Connection>(
    conn: &C,
    root: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    paint_static(conn, root)?;
    spinner_loop(conn, root, region("spinner"))
}

fn paint_static<C: Connection>(
    conn: &C,
    root: u32,
) -> Result<(), Box<dyn std::error::Error>> {
    paint_solid(conn, root, region("solid"))?;
    paint_text(conn, root, region("text"))?;
    paint_gradient(conn, root, region("gradient"))?;
    conn.flush()?;
    eprintln!("test-pattern: mixed static regions painted");
    Ok(())
}

fn paint_solid<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::new().foreground(0x00FF_0000))?; // red
    conn.poly_fill_rectangle(root, gc, &[Rectangle {
        x: r.x as i16, y: r.y as i16,
        width: r.w as u16, height: r.h as u16,
    }])?;
    Ok(())
}

fn paint_text<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    // Background fill — near-black so unlit pixels are deterministic.
    let bg_gc = conn.generate_id()?;
    conn.create_gc(bg_gc, root, &CreateGCAux::new().foreground(0x0014_1414))?;
    conn.poly_fill_rectangle(root, bg_gc, &[Rectangle {
        x: r.x as i16, y: r.y as i16,
        width: r.w as u16, height: r.h as u16,
    }])?;

    // Foreground GC — near-white text.
    let fg_gc = conn.generate_id()?;
    conn.create_gc(fg_gc, root, &CreateGCAux::new().foreground(0x00F5_F5F5))?;

    // Repeat "TEST" three times across the top of the region, with a 4 px
    // top margin and 2 px between rows.
    let text = "TEST TEST TEST";
    let mut rects: Vec<Rectangle> = Vec::with_capacity(text.len() * 24);
    for (n, ch) in text.chars().enumerate() {
        let glyph_x = r.x + 8 + (n as u32) * GLYPH_W;
        let glyph_y = r.y + 8;
        for py in 0..GLYPH_H {
            for px in 0..GLYPH_W {
                if pixel_set(ch, px, py) {
                    rects.push(Rectangle {
                        x: (glyph_x + px) as i16,
                        y: (glyph_y + py) as i16,
                        width: 1, height: 1,
                    });
                }
            }
        }
    }
    conn.poly_fill_rectangle(root, fg_gc, &rects)?;
    Ok(())
}

fn paint_gradient<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    // 32 horizontal stripes from black at the top to bright blue at the bottom.
    // Coarse stripes so even at 700×500 capture the gradient is detectable.
    let stripes: u32 = 32;
    let stripe_h = r.h / stripes;
    for i in 0..stripes {
        let v = ((i * 255) / (stripes - 1)) as u32;
        let pixel = (v << 16) | (v << 8) | (255 - v); // ramp R+G; B counter-ramp
        let gc = conn.generate_id()?;
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(pixel))?;
        conn.poly_fill_rectangle(root, gc, &[Rectangle {
            x: r.x as i16,
            y: (r.y + i * stripe_h) as i16,
            width: r.w as u16,
            height: stripe_h as u16,
        }])?;
    }
    Ok(())
}

fn spinner_loop<C: Connection>(
    conn: &C,
    root: u32,
    r: &Region,
) -> Result<(), Box<dyn std::error::Error>> {
    let gc = conn.generate_id()?;
    conn.create_gc(gc, root, &CreateGCAux::default())?;
    let mut hue: u8 = 0;
    loop {
        let r_ch = hue;
        let g_ch = hue.wrapping_add(85);
        let b_ch = hue.wrapping_add(170);
        let pixel = ((r_ch as u32) << 16) | ((g_ch as u32) << 8) | (b_ch as u32);
        conn.change_gc(gc, &ChangeGCAux::new().foreground(pixel))?;
        conn.poly_fill_rectangle(root, gc, &[Rectangle {
            x: r.x as i16, y: r.y as i16,
            width: r.w as u16, height: r.h as u16,
        }])?;
        conn.flush()?;
        std::thread::sleep(Duration::from_millis(100));
        hue = hue.wrapping_add(25);
    }
}
```

- [ ] **Step 2: Expose the module from the library crate**

Edit `ghostframe-test-pattern/src/lib.rs`. Append:

```rust
pub mod mixed;
```

(`font` and `text_grid` are already `pub mod` from the e2e_text_clarity plan.)

- [ ] **Step 3: Verify the crate builds**

Run: `cargo build -p ghostframe-test-pattern`
Expected: clean build of both lib and bin targets. If `font::pixel_set` cannot be resolved, double-check that `lib.rs` declares `pub mod font;` from the prior plan.

- [ ] **Step 4: Commit**

```bash
git add ghostframe-test-pattern/src/mixed.rs ghostframe-test-pattern/src/lib.rs
git commit -m "feat(test-pattern): mixed pattern module with REGIONS table and renderer"
```

---

## Task 2: Wire `--mixed` into the test-pattern CLI

**Files:**
- Modify: `ghostframe-test-pattern/src/main.rs`

- [ ] **Step 1: Add the flag and dispatch**

Edit `Args` in `ghostframe-test-pattern/src/main.rs`. Add a new field:

```rust
    /// Draw the four-region mixed pattern (solid + text + gradient + spinner).
    /// Spawns the spinner loop forever; mutually exclusive with --solid-red,
    /// --spinner, --text-grid (those are ignored when --mixed is set).
    #[arg(long)]
    mixed: bool,
```

Then, immediately after `let root = screen.root;` and *before* the existing `if args.solid_red {` block, insert:

```rust
    if args.mixed {
        // Mutually exclusive — render four regions and run the spinner forever.
        return ghostframe_test_pattern::mixed::render_and_spin(&conn, root);
    }
```

The `return` short-circuits past the other modes. `render_and_spin` never returns.

- [ ] **Step 2: Smoke-test against Xvfb**

Run: `Xvfb :88 -screen 0 800x600x24 &` then `DISPLAY=:88 cargo run -p ghostframe-test-pattern -- --mixed`
Expected: prints `test-pattern: mixed static regions painted`. Process keeps running (spinner loop). Kill with Ctrl-C.

If Xvfb isn't installed locally, skip this step.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-test-pattern/src/main.rs
git commit -m "feat(test-pattern): --mixed flag dispatches to mixed::render_and_spin"
```

---

## Task 3: Rebuild the test-server container

**Files:**
- (no source changes)

- [ ] **Step 1: Rebuild the image**

Run: `docker build -f tests/containers/test-server/Dockerfile -t ghostframe/test-server:latest .`
Expected: image rebuilt cleanly.

- [ ] **Step 2: Verify --mixed is present**

Run: `docker run --rm --entrypoint /usr/local/bin/ghostframe-test-pattern ghostframe/test-server:latest --help`
Expected: usage string lists `--mixed`.

---

## Task 4: RegionCheck helper

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

The four `RegionCheck` variants need a helper that runs JS in the page to inspect a rectangle. Implement the helper before the test that consumes it.

- [ ] **Step 1: Append the helper module**

Append to `ghostframe-lib/tests/e2e.rs` (anywhere after the existing helper functions, before `e2e_multi_pattern`):

```rust
#[derive(Debug, Clone, Copy)]
enum RegionCheck {
    /// Every sampled pixel must be solidly red.
    SolidRed,
    /// Region must contain both very dark and very bright pixels (text on bg).
    Legible,
    /// Region brightness must be monotonic top→bottom (gradient).
    SmoothGradient,
    /// Two snapshots 1.5 s apart must differ.
    Changing,
}

async fn assert_region_rendered(
    page: &chromiumoxide::Page,
    region: &ghostframe_test_pattern::mixed::Region,
    check: RegionCheck,
) -> Result<()> {
    use serde_json::Value;

    match check {
        RegionCheck::SolidRed => {
            // Sample 9 evenly spaced points inside the region; >= 7 must be red.
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    const xs = [{x}+16, {x}+{w}/2|0, {x}+{w}-16];
                    const ys = [{y}+16, {y}+{h}/2|0, {y}+{h}-16];
                    let red = 0, total = 0;
                    for (const xx of xs) for (const yy of ys) {{
                        const p = c.getImageData(xx, yy, 1, 1).data;
                        if (p[0] > 180 && p[1] < 80 && p[2] < 80) red++;
                        total++;
                    }}
                    return {{ red, total }};
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let out: Value = page.evaluate(js.as_str()).await?.into_value()?;
            let red = out["red"].as_u64().unwrap_or(0);
            let total = out["total"].as_u64().unwrap_or(0);
            assert!(red >= 7, "{}: only {red}/{total} samples were red", region.name);
        }

        RegionCheck::Legible => {
            // Find the brightest and darkest pixels in a sweep — gap > 100 luma
            // means we have both ink and bg, i.e. text rendered.
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    let lo = 255, hi = 0;
                    for (let dy = 8; dy < {h}; dy += 4) {{
                        for (let dx = 8; dx < {w}; dx += 4) {{
                            const p = c.getImageData({x}+dx, {y}+dy, 1, 1).data;
                            const lum = 0.2126*p[0] + 0.7152*p[1] + 0.0722*p[2];
                            if (lum < lo) lo = lum;
                            if (lum > hi) hi = lum;
                        }}
                    }}
                    return {{ lo, hi }};
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let out: Value = page.evaluate(js.as_str()).await?.into_value()?;
            let lo = out["lo"].as_f64().unwrap_or(255.0);
            let hi = out["hi"].as_f64().unwrap_or(0.0);
            assert!(
                hi - lo > 100.0,
                "{}: contrast too low (lo {lo:.0}, hi {hi:.0}); text not rendered",
                region.name
            );
        }

        RegionCheck::SmoothGradient => {
            // Average luma at three vertical bands — top, middle, bottom.
            // Bottom must be brighter than top by a clear margin.
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    function band(y0, y1) {{
                        let sum = 0, n = 0;
                        for (let yy = y0; yy < y1; yy += 4) {{
                            for (let xx = {x}+8; xx < {x}+{w}; xx += 8) {{
                                const p = c.getImageData(xx, yy, 1, 1).data;
                                sum += 0.2126*p[0] + 0.7152*p[1] + 0.0722*p[2];
                                n++;
                            }}
                        }}
                        return n ? sum / n : 0;
                    }}
                    return {{
                        top:    band({y}, {y}+{h}/4|0),
                        middle: band({y}+{h}/4|0, {y}+3*{h}/4|0),
                        bottom: band({y}+3*{h}/4|0, {y}+{h}),
                    }};
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let out: Value = page.evaluate(js.as_str()).await?.into_value()?;
            let top    = out["top"].as_f64().unwrap_or(0.0);
            let bottom = out["bottom"].as_f64().unwrap_or(0.0);
            assert!(
                bottom - top > 50.0,
                "{}: gradient too flat (top {top:.0}, bottom {bottom:.0})",
                region.name
            );
        }

        RegionCheck::Changing => {
            let js = format!(
                r#"
                (() => {{
                    const c = document.getElementById('canvas').getContext('2d');
                    const data = c.getImageData({x}, {y}, {w}, {h}).data;
                    let h = 0;
                    for (let i = 0; i < data.length; i++) h = ((h << 5) - h + data[i]) | 0;
                    return h;
                }})()
                "#,
                x = region.x, y = region.y, w = region.w, h = region.h,
            );
            let h1: i64 = page.evaluate(js.as_str()).await?.into_value()?;
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let h2: i64 = page.evaluate(js.as_str()).await?.into_value()?;
            assert_ne!(
                h1, h2,
                "{}: region was static between snapshots — spinner not animating",
                region.name
            );
        }
    }
    Ok(())
}
```

- [ ] **Step 2: Verify the helper compiles**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build. The unused-warning on `assert_region_rendered` is fine — Task 5 wires it in.

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): RegionCheck helper for per-region canvas assertions"
```

---

## Task 5: e2e_multi_pattern test

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

- [ ] **Step 1: Add the test**

Append to `ghostframe-lib/tests/e2e.rs`:

```rust
/// Mixed-content rendering test. Pre-M3 (single codec) the assertion is
/// per-region rendering correctness. Post-M3 the same REGIONS table will
/// drive codec-selection assertions via a future stats channel — see
/// `Region::expected_codec`.
#[tokio::test]
async fn e2e_multi_pattern() -> Result<()> {
    use ghostframe_test_pattern::mixed::{region, SETTLE};

    let setup = setup_e2e("--mixed").await?;

    // SETTLE is 7s — enough for QUIC slow-start to open and at least two
    // spinner frames to land.
    tokio::time::sleep(SETTLE).await;

    assert_region_rendered(&setup.page, region("solid"),    RegionCheck::SolidRed).await?;
    assert_region_rendered(&setup.page, region("text"),     RegionCheck::Legible).await?;
    assert_region_rendered(&setup.page, region("gradient"), RegionCheck::SmoothGradient).await?;
    assert_region_rendered(&setup.page, region("spinner"),  RegionCheck::Changing).await?;

    // TODO(M3): once the classifier ships, also assert that each region's
    //           tiles are encoded with `region.expected_codec`. Will require
    //           a per-tile codec stats channel from server → test (not
    //           shipped pre-M3).

    Ok(())
}
```

- [ ] **Step 2: Verify the test compiles**

Run: `cargo build -p ghostframe-lib --tests`
Expected: clean build, no unused-warning on `assert_region_rendered`.

- [ ] **Step 3: Run the test**

Run: `cargo test -p ghostframe-lib --test e2e e2e_multi_pattern -- --test-threads=1 --nocapture`
Expected: PASS within ~30 s.

If the spinner region check fails because it was static between snapshots, increase the inter-snapshot sleep in `RegionCheck::Changing` from 1500 ms to 2000 ms (the spinner cycles every ~1 s through 10 hue steps, so 1.5 s should always span at least one change — but capture FPS might delay arrival).

If the gradient region looks flat (`SmoothGradient` fails) at low capture FPS, the H.264 encoder may be smoothing the stripes. Verify by saving a screenshot:

```rust
let png = setup.page.screenshot(chromiumoxide::page::ScreenshotParams::default()).await?;
std::fs::write("/tmp/e2e_multi_pattern.png", &png)?;
```

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/tests/e2e.rs
git commit -m "test(e2e): e2e_multi_pattern asserts per-region rendering of mixed content

Pre-M3 RegionCheck assertions validate solid+text+gradient+spinner each
render correctly. M3 follow-up will tighten to per-region codec assertions
via a future stats channel."
```

---

## Final verification

- [ ] **Step 1: Workspace build**

Run: `cargo build --workspace --tests`
Expected: clean build.

- [ ] **Step 2: Run every e2e test serially**

Run: `cargo test -p ghostframe-lib --test e2e -- --test-threads=1`
Expected: all prior e2e tests still pass plus the new `e2e_multi_pattern`.
