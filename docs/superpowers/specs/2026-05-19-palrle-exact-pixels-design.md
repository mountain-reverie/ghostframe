# B1 — `palrle_exact` Pixel Verification — Design

**Date**: 2026-05-19
**Milestone**: Closes the M3.2c B1 follow-up (`palrle_exact` Tasks 10-11 in the original M3.2c plan, deferred during the initial milestone closure).
**Predecessor**: `docs/superpowers/plans/2026-05-15-m3.2c-verification.md` § Task 10 + Task 11.

## Background

M3.2c's W3/B1 verification ("PalRLE GPU compute shader correctness") was originally split between two artifacts:
- An end-to-end render check via a controlled-content test pattern (`palrle_exact`), and
- Several lossier checks (text legibility via `e2e_text_clarity`, codec classification via `e2e_palrle_5pct_loss`, etc.).

The lossier checks shipped during the milestone closure. The `palrle_exact` exact-pixel verification was deferred and tracked as an optional follow-up. With B2 wire-emission closed (commit `d4b4ba1`), this is the last unshipped M3.2c artifact.

The value of an exact-pixel PalRle test, beyond the existing tests, is catching shader bugs that don't show up under text-luminance or codec-classification assertions:
- **Nibble unpack swap** (high↔low) — text remains legible if nibbles swap, since each character's pixels share the same color; only per-pixel alternating patterns expose it.
- **Per-pixel coordinate arithmetic** (`pixel_in_tile = y*32 + x`) — wrong stride would show as visual shear.
- **BGRA→RGBA channel swizzle** — text rendered in two colors would still pass luminance assertions even if R↔B swap.
- **Tile-coord arithmetic** (`work.tile_x * 32 + pixel_x_in_tile`) — wrong tile placement would shift the whole rendered tile, which exact-pixel sampling catches but luminance doesn't.

The existing PalRle tests pass for reasons orthogonal to these bugs. A focused exact-pixel test surfaces them deterministically.

## Goal

Add an `e2e_palrle_exact_pixels` end-to-end test that drives a deterministic four-pattern PalRle scene and asserts exact RGBA values at 16 sample points. End state: M3.2c original-plan items (Tasks 1-22) all closed.

## Approach

### Test pattern: four PalRle tiles, four distinct shapes

DRM-direct paint-once scene on the VKMS scanout (1024×768 default). Background is dark grey (`BG_PIXEL = 0x00141414`). Four 32×32 test tiles at well-separated locations, each using the same 2-color palette (`COLOR_A = 0x00FF0000` red, `COLOR_B = 0x000000FF` blue). Pixel formula varies per tile:

| Tile (tile_x, tile_y) | Pixel formula (px, py = 0..31 within tile) | Catches |
|---|---|---|
| (5, 5) — checkerboard | `if (px + py) % 2 == 0 { A } else { B }` | nibble-swap (high↔low), per-pixel arithmetic |
| (10, 5) — horizontal stripes | `if py % 2 == 0 { A } else { B }` | row arithmetic (y-step error) |
| (5, 10) — vertical stripes | `if px % 2 == 0 { A } else { B }` | column arithmetic (x-step error), nibble-swap |
| (10, 10) — 2×2 blocks | `if (px/2 + py/2) % 2 == 0 { A } else { B }` | column-pair / row-pair arithmetic |

The classifier picks PalRle for all four (each tile has `unique_colors = 2`, satisfying Rule 5 medium-freq or Rule 7 low-freq paths to PalRle). Background tiles classify as Solid post-first-frame (uc=1) and don't interfere.

Tiles are non-adjacent so dirty-detection treats each independently — no spurious cross-tile coalescing.

### Sample points and expected RGBA

16 sample points total (4 per tile). Expected RGBA = `[R, G, B, 0x00]` — alpha is 0x00 because the production Solid/PalRle WGSL shaders pass the wire X byte through to the framebuffer alpha channel unchanged, and the test pattern paints with X=0x00. (Same convention as `solid_per_tile.rs::samples` — established during B3 closure on 2026-05-18.)

```rust
const RGBA_RED:  [u8; 4] = [0xFF, 0x00, 0x00, 0x00];
const RGBA_BLUE: [u8; 4] = [0x00, 0x00, 0xFF, 0x00];

pub const SAMPLES: &[(u32, u32, [u8; 4])] = &[
    // Tile (5, 5) checkerboard — tile origin at (160, 160).
    (160, 160, RGBA_RED),    // (px=0, py=0): (0+0)%2==0 → A
    (161, 160, RGBA_BLUE),   // (px=1, py=0): odd      → B
    (160, 161, RGBA_BLUE),   // (px=0, py=1): odd      → B
    (161, 161, RGBA_RED),    // (px=1, py=1): even     → A

    // Tile (10, 5) horizontal stripes — tile origin at (320, 160).
    (320, 160, RGBA_RED),    // (px=0, py=0): row 0 → A
    (320, 161, RGBA_BLUE),   // (px=0, py=1): row 1 → B
    (351, 160, RGBA_RED),    // (px=31, py=0): row 0 → A (right end)
    (351, 161, RGBA_BLUE),   // (px=31, py=1): row 1 → B

    // Tile (5, 10) vertical stripes — tile origin at (160, 320).
    (160, 320, RGBA_RED),    // (px=0, py=0):  col 0 → A
    (161, 320, RGBA_BLUE),   // (px=1, py=0):  col 1 → B
    (160, 351, RGBA_RED),    // (px=0, py=31): col 0 → A (bottom)
    (161, 351, RGBA_BLUE),   // (px=1, py=31): col 1 → B

    // Tile (10, 10) 2×2 blocks — tile origin at (320, 320).
    (320, 320, RGBA_RED),    // (px=0, py=0): (0/2 + 0/2) % 2 == 0 → A
    (321, 320, RGBA_RED),    // (px=1, py=0): (0   + 0  ) % 2 == 0 → A (same block)
    (322, 320, RGBA_BLUE),   // (px=2, py=0): (1   + 0  ) % 2 == 1 → B
    (320, 322, RGBA_BLUE),   // (px=0, py=2): (0   + 1  ) % 2 == 1 → B
];
```

How specific bug classes show up:
- **Nibble swap (high↔low)**: pixels at even index get the high nibble's value instead of the low. Checkerboard tile shows all 4 sample assertions failing in a recognizable pattern (each pair `(160,160)/(161,160)` inverts). Vertical stripes similarly all-fail.
- **R↔B channel swap**: all 16 sample assertions fail in pairs — every RGBA_RED comes back as `[0x00, 0x00, 0xFF, 0x00]` and vice versa.
- **Tile-coord arithmetic error**: one or more entire tiles return BG color (the shader wrote pixels OUTSIDE the framebuffer or to a wrong tile). Tile X shifted by N would show as adjacent-tile sampling.
- **Per-pixel row stride error**: vertical stripes still pass (each row is same anyway), but horizontal stripes and checkerboard fail.
- **Per-pixel column stride error**: horizontal stripes still pass, vertical stripes and checkerboard fail.

### Where the test pattern logic lives

New module `ghostframe-test-pattern/src/palrle_exact.rs`. Modeled after `solid_per_tile.rs`:
- Open DRM device via `setup_dumb_scanout` (existing helper).
- One-time paint of full scene (BG + 4 test tiles).
- `msync_buffer` and enter a keepalive loop (re-map per tick + msync to keep DMA-BUF coherent). Same structure as `text_grid_drm.rs`.
- Pub: `pub const SAMPLES: &[(u32, u32, [u8; 4])]`, `pub fn run(card_path: &str) -> Result<...>`.

Wired into CLI as `--palrle-exact --drm-direct`. Dispatched before the existing `--drm-direct && text_grid` / `--drm-direct && solid_per_tile` branches in `main.rs`.

### E2E test shape

In `ghostframe-lib/tests/e2e.rs`:

```rust
/// W3/B1 — Exact-pixel verification of the PalRLE compute shader.
///
/// Drives `--palrle-exact --drm-direct`'s four 32×32 PalRle tiles (each
/// with a distinct 2-color pattern) and asserts exact RGBA values at
/// 16 sample points. Catches nibble-swap, per-pixel arithmetic,
/// channel-swizzle, and tile-coord bugs that the existing PalRle tests
/// (e2e_palrle_5pct_loss, e2e_text_clarity, e2e_palrle_oob_index) don't
/// surface.
///
/// Closes M3.2c B1 follow-up.
#[tokio::test(flavor = "multi_thread")]
async fn e2e_palrle_exact_pixels() -> Result<()> {
    use ghostframe_test_pattern::palrle_exact::SAMPLES;

    let setup = setup_e2e_webgpu_gpu("--palrle-exact --drm-direct").await?;
    // 5s covers QUIC slow-start + first-frame H.264 phase + classifier
    // transition to PalRle for the 2-color test tiles.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Sanity: PalRle codec must appear on the wire.
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

    // Exact-pixel assertions.
    for &(x, y, expected) in SAMPLES {
        let probe = format!("window.__readPixel({}, {})", x, y);
        let got: Vec<u8> = setup.page.evaluate(probe.as_str()).await?.into_value()?;
        assert_eq!(
            got,
            expected.to_vec(),
            "pixel ({}, {}) mismatch: got {:?}, expected {:?}",
            x, y, got, expected
        );
    }
    Ok(())
}
```

## Files Touched

- **Create** `ghostframe-test-pattern/src/palrle_exact.rs` (~100 lines: scene paint + `SAMPLES` + `run`).
- **Modify** `ghostframe-test-pattern/src/lib.rs` (one line: `pub mod palrle_exact;`).
- **Modify** `ghostframe-test-pattern/src/main.rs` (CLI flag + dispatch branch).
- **Modify** `ghostframe-lib/tests/e2e.rs` (new test `e2e_palrle_exact_pixels`).

No client-side changes. No design-doc updates needed (no codec change).

## Testing

The e2e IS the test. Encoder + shader behavior is already covered by existing unit tests:
- `encode_pal_rle_payload`, `encode_pal_rle_payload_indices_raw`, `encode_pal_rle_indices` (pal_rle.rs).
- `phase_b_emits_indices_raw_when_caps_enabled_and_thin` (io_bridge.rs).
- Per-codec WGSL is exercised by the rest of the e2e suite implicitly.

`e2e_palrle_exact_pixels` is the END-TO-END verification that all the pieces — encoder, wire format, prevalidator (RLE decode + indices_raw passthrough), GPU dispatch, shader nibble-unpack + palette-lookup + swizzle + textureStore — compose correctly under controlled content.

Suite regression after landing: expect 24 passed, 0 failed, 2 ignored (one more than the current 23 thanks to the new test).

## Out of Scope

- **Thin (indices_raw) variant of this test**. The current test exercises the bundled-first-emission path. The shader code path for thin is IDENTICAL to bundled (the prevalidator decodes RLE before GPU dispatch), so testing one covers the other. If WGSL ever splits into two paths, revisit.
- **Per-tile classifier verification beyond the codec-sanity check**. The codec list assertion at step 3 catches the "classifier didn't pick PalRle" failure mode.
- **Adversarial patterns** (palette with 16 colors, max RLE run lengths, all-zero indices). The four shapes cover the common arithmetic bug classes; corner cases are out of scope for what's intended as a smoke-grade exact-pixel test.
- **Animating the pattern** (e.g., color flip frame-to-frame). Paint-once is sufficient — the shader path is identical for first-frame bundled and subsequent thin.
- **Extending solid_per_tile** instead of a new module. solid_per_tile's purpose is Solid-pipeline verification with a motion region. Adding diverse PalRle test tiles there would muddy its intent. New module is cleaner.

## Risk

- **Classifier picks Bc1 or H264 instead of PalRle** for the test tiles: low. Each test tile has `uc=2`, which fires Rule 5 (medium-freq, uc≤16 → PalRle) or Rule 7 (low-freq, uc≤16 → PalRle). Both paths landed in M3.2c. The codec-sanity assertion catches misclassification.
- **VKMS scanout dimensions differ from 1024×768**: low. The four tiles fit any resolution ≥ 352×352 wide (largest sample at x=351, y=351). If the host runs a smaller mode (rare), the test would fail in a clear way and we'd add a runtime dim check (mirrors solid_per_tile's pattern).
- **Paint-once doesn't reach PalRle steady state within 5s**: low. Empirically `solid_per_tile` reaches PalRle within ~1-2s on the same harness; paint-once should be at least as fast (less classifier churn from motion). If observed flaky, bump to 7s.
- **Edge pixels on partial-resolution scanouts**: not applicable. The four test tiles are at interior tile coords (5-10), well away from the right/bottom edges where partial-tile rendering matters (W5).

## Pointers

- Original plan: `docs/superpowers/plans/2026-05-15-m3.2c-verification.md` Tasks 10-11.
- Sister test pattern (same DRM-direct + paint-once approach): `ghostframe-test-pattern/src/solid_per_tile.rs`.
- Wire format reference: `docs/superpowers/specs/2026-05-13-palrle-codec-design.md`.
- Shader reference: `ghostframe-web-client/src/webgpu/shaders/palrle_decode.wgsl`.
- Alpha convention: `solid_per_tile.rs::CornerSample` docstring + the X-byte passthrough behavior documented during B3 closure (commit `78f4bfd`).
