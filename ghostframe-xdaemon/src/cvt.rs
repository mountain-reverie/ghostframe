//! VESA CVT (Coordinated Video Timings) reduced-blanking (RB v1) modeline
//! calculation.
//!
//! RandR's `CreateMode` request needs a full modeline (pixel clock, h/v
//! sync start/end, h/v totals) — not just a resolution. This module derives
//! one from width/height/refresh using the VESA CVT 1.2 reduced-blanking
//! algorithm.
//!
//! `cvt(1)` (package `libxcvt` — "standalone version of the X server
//! implementation of the VESA CVT standard timing modelines generator",
//! i.e. this literally IS what Xorg itself uses to answer `CreateMode`) is
//! the ground truth for this module. Every constant and rounding rule below
//! was checked against `cvt <w> <h> <refresh> -r` output, not against a
//! remembered copy of the spec. That mattered twice:
//!
//! - A webfetched copy of the Linux kernel's `drm_cvt_mode()`
//!   (drivers/gpu/drm/drm_modes.c) disagreed with observed `cvt(1)` behavior
//!   on the direction of the horizontal granularity rounding (kernel source
//!   read as round-down; `cvt(1)` rounds up — `cvt 1921 1080 60 -r` yields
//!   h_active=1928, not 1920).
//! - An initial implementation used the kernel's truncate-`hperiod`-first
//!   formulation for the pixel clock, and separately looked up the vertical
//!   sync width from the *unrounded* input width. Both diverge from `cvt(1)`
//!   for the ~5% of inputs that aren't already 8-pixel aligned: see the
//!   `matches_cvt_for_1915x1080_unaligned_aspect_regression` and
//!   `matches_cvt_for_912x480_clock_precision_regression` tests below for
//!   the pinned reproductions, and `src/bin/cvt_sweep.rs` (mentioned below)
//!   for how a wide sweep against the real oracle caught both.
//!
//! Where implementation and oracle disagreed, `cvt(1)` won, per this task's
//! stated ground truth.
//!
//! If this ever needs re-deriving, regenerate reference vectors with
//! `cvt <width> <height> <refresh> -r` and rebuild the arithmetic against
//! those step for step — the CVT algorithm is entirely integer/fixed-point,
//! and matching float math will drift off by rounding.
//!
//! A one-off wide sweep against the real `cvt(1)` binary (not a unit test —
//! it shells out) lives at `ghostframe-xdaemon/src/bin/cvt_sweep.rs`. Run it
//! with `cargo run -p ghostframe-xdaemon --example cvt_sweep` before touching
//! this module's arithmetic; it re-includes this file's source directly so
//! it can never drift out of sync with what actually ships.

/// A full modeline: everything RandR's `CreateMode` needs beyond width and
/// height.
// Not yet consumed outside this module: the RandR `CreateMode` call site
// lands in a later M4b task. Remove this once that wiring lands.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timing {
    pub pixel_clock_khz: u32,
    pub h_active: u16,
    pub h_sync_start: u16,
    pub h_sync_end: u16,
    pub h_total: u16,
    pub v_active: u16,
    pub v_sync_start: u16,
    pub v_sync_end: u16,
    pub v_total: u16,
}

/// Compute a VESA CVT reduced-blanking (RB v1) modeline for `width` x
/// `height` at `refresh_hz`. Progressive scan only (no interlace factor).
///
/// # Panics
///
/// This is a direct transcription of the CVT integer arithmetic, and is
/// not guarded against degenerate input: `height == 0` or `refresh_hz == 0`
/// divides by zero, and `refresh_hz >= 2174` underflows an intermediate
/// `u64` subtraction (`tmp1`), which panics in debug builds and produces
/// garbage output in release builds. No real display uses a 2174 Hz
/// refresh rate or a zero-line/zero-Hz mode, and the call site (a later
/// M4b task) is expected to floor/ceiling client-supplied width, height and
/// refresh before this ever runs — so this is documented rather than
/// guarded. Revisit if this function stops being fed only pre-validated
/// input.
// Not yet consumed outside this module: see `Timing`'s doc comment.
#[allow(dead_code)]
pub fn reduced_blanking(width: u16, height: u16, refresh_hz: u16) -> Timing {
    // --- Fixed CVT-RB v1 parameters (VESA CVT 1.2, reduced blanking) ---
    // Horizontal granularity: h_active is rounded to a multiple of this.
    const H_GRANULARITY: u64 = 8;
    // Total horizontal blanking, pixels. Split as sync (32) + back porch
    // (H_BLANK/2 = 80); front porch is H_BLANK/2 - H_SYNC = 48, though the
    // code below derives hsync_end/hsync_start directly rather than naming
    // the front porch separately.
    const H_BLANK: u64 = 160;
    // Horizontal sync pulse width, pixels.
    const H_SYNC: u64 = 32;
    // Vertical front porch, lines (fixed for reduced blanking).
    const V_FRONT_PORCH: u64 = 3;
    // Minimum vertical back porch, lines.
    const MIN_V_BPORCH: u64 = 6;
    // Minimum vertical blanking time, microseconds.
    const MIN_VBLANK_US: u64 = 460;
    // Fixed-point scale factor used throughout the CVT reference algorithm.
    const HV_FACTOR: u64 = 1000;
    // Pixel clock is truncated down to a multiple of this (0.25 MHz).
    const CLOCK_STEP_KHZ: u64 = 250;

    let width = width as u64;
    let height = height as u64;
    let vfieldrate = refresh_hz as u64;

    // h_active: round UP to the nearest multiple of H_GRANULARITY.
    // Verified against cvt(1): `cvt 1921 1080 60 -r` -> h_active 1928, and
    // `cvt 1928 1080 60 -r` -> h_active 1928 (already a multiple, no-op).
    let h_active = width.div_ceil(H_GRANULARITY) * H_GRANULARITY;
    // No vertical rounding for progressive modes.
    let v_active = height;

    // Vertical sync width: selected from the display's aspect ratio, using
    // height against the *rounded* h_active — NOT the raw input width.
    // Verified: 1280x1024 (5:4) needs vsync=7 (`cvt 1280 1024 60 -r`), the
    // 16:9 vectors all need vsync=5, and an aspect matching no table entry
    // falls back to 10 (`cvt 1920 1081 60 -r`, which is not exactly any
    // listed ratio). Comparing against the raw width instead of h_active is
    // a real, verified divergence from `cvt(1)` for any non-8-aligned
    // input: `cvt 1915 1080 60 -r` rounds h_active up to 1920 and picks the
    // 16:9 table entry (vsync=5) from that, not from the unrounded 1915
    // (which matches no table entry and would wrongly fall back to 10) —
    // see `matches_cvt_for_1915x1080_unaligned_aspect_regression` below.
    let v_sync_width: u64 = if height.is_multiple_of(3) && height * 4 / 3 == h_active {
        4 // 4:3
    } else if height.is_multiple_of(9) && height * 16 / 9 == h_active {
        5 // 16:9
    } else if height.is_multiple_of(10) && height * 16 / 10 == h_active {
        6 // 16:10
    } else if (height.is_multiple_of(4) && height * 5 / 4 == h_active)
        || (height.is_multiple_of(9) && height * 15 / 9 == h_active)
    {
        7 // 5:4 or 15:9
    } else {
        10 // non-standard aspect
    };

    // Estimated horizontal period, in the CVT algorithm's internal
    // fixed-point units. Only its use below (mirroring the CVT reference
    // algorithm step for step) matters, not the unit itself.
    let tmp1 = HV_FACTOR * 1_000_000 - MIN_VBLANK_US * HV_FACTOR * vfieldrate;
    let hperiod = tmp1 / (v_active * vfieldrate);

    // Vertical blanking interval, in lines, derived from the minimum
    // vertical blanking time, floored to the sum of the fixed porches plus
    // the aspect-selected sync width.
    let mut vbi_lines = MIN_VBLANK_US * HV_FACTOR / hperiod + 1;
    let min_vbi = V_FRONT_PORCH + v_sync_width + MIN_V_BPORCH;
    if vbi_lines < min_vbi {
        vbi_lines = min_vbi;
    }

    let v_total = v_active + vbi_lines;
    let h_total = h_active + H_BLANK;
    let h_sync_end = h_active + H_BLANK / 2;
    let h_sync_start = h_sync_end - H_SYNC;
    let v_sync_start = v_active + V_FRONT_PORCH;
    let v_sync_end = v_sync_start + v_sync_width;

    // Pixel clock, truncated DOWN to a 0.25 MHz (250 kHz) step.
    //
    // This is deliberately NOT `h_total * HV_FACTOR * 1000 / hperiod` (the
    // kernel `drm_cvt_mode()` formulation, and this module's original
    // implementation): `hperiod` above was already truncated to an integer,
    // and dividing by that truncated value discards a fraction that can
    // push the result across a 250 kHz boundary. `cvt(1)` (libxcvt) keeps
    // full precision by never materializing `hperiod` for the clock
    // calculation — so neither do we: substitute
    // `hperiod = tmp1 / (v_active * vfieldrate)` symbolically and multiply
    // through before dividing, so the only truncation is the final integer
    // division, done once, at the last step. Verified: `cvt 912 480 60 -r`
    // needs 31.50 MHz; going through truncated `hperiod` first produces
    // 31.75 MHz — one step too high — for ~2% of 8-aligned resolutions at
    // 60 Hz. See `matches_cvt_for_912x480_clock_precision_regression`.
    let raw_khz = h_total * HV_FACTOR * 1000 * v_active * vfieldrate / tmp1;
    let pixel_clock_khz = raw_khz - raw_khz % CLOCK_STEP_KHZ;

    Timing {
        pixel_clock_khz: pixel_clock_khz as u32,
        h_active: h_active as u16,
        h_sync_start: h_sync_start as u16,
        h_sync_end: h_sync_end as u16,
        h_total: h_total as u16,
        v_active: v_active as u16,
        v_sync_start: v_sync_start as u16,
        v_sync_end: v_sync_end as u16,
        v_total: v_total as u16,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated with `cvt 1920 1080 60 -r`:
    //   Modeline "1920x1080R" 138.50 1920 1968 2000 2080 1080 1083 1088 1111
    #[test]
    fn matches_cvt_for_1920x1080() {
        let t = reduced_blanking(1920, 1080, 60);
        assert_eq!(t.pixel_clock_khz, 138_500);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (1968, 2000, 2080)
        );
        assert_eq!(
            (t.v_sync_start, t.v_sync_end, t.v_total),
            (1083, 1088, 1111)
        );
    }

    // Generated with `cvt 2560 1440 60 -r`:
    //   Modeline "2560x1440R" 241.50 2560 2608 2640 2720 1440 1443 1448 1481
    #[test]
    fn matches_cvt_for_2560x1440() {
        let t = reduced_blanking(2560, 1440, 60);
        assert_eq!(t.pixel_clock_khz, 241_500);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (2608, 2640, 2720)
        );
        assert_eq!(
            (t.v_sync_start, t.v_sync_end, t.v_total),
            (1443, 1448, 1481)
        );
    }

    // Generated with `cvt 1280 1024 60 -r` (5:4 aspect — exercises the
    // aspect-dependent vertical sync width table; vsync=7 here vs 5 for the
    // 16:9 cases above):
    //   Modeline "1280x1024R" 90.75 1280 1328 1360 1440 1024 1027 1034 1054
    #[test]
    fn matches_cvt_for_1280x1024_5_4_aspect() {
        let t = reduced_blanking(1280, 1024, 60);
        assert_eq!(t.pixel_clock_khz, 90_750);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (1328, 1360, 1440)
        );
        assert_eq!(
            (t.v_sync_start, t.v_sync_end, t.v_total),
            (1027, 1034, 1054)
        );
    }

    // Generated with `cvt 1280 720 60 -r`:
    //   Modeline "1280x720R" 63.75 1280 1328 1360 1440 720 723 728 741
    #[test]
    fn matches_cvt_for_1280x720() {
        let t = reduced_blanking(1280, 720, 60);
        assert_eq!(t.pixel_clock_khz, 63_750);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (1328, 1360, 1440)
        );
        assert_eq!((t.v_sync_start, t.v_sync_end, t.v_total), (723, 728, 741));
    }

    // Generated with `cvt 3840 2160 60 -r`:
    //   Modeline "3840x2160R" 533.00 3840 3888 3920 4000 2160 2163 2168 2222
    #[test]
    fn matches_cvt_for_3840x2160() {
        let t = reduced_blanking(3840, 2160, 60);
        assert_eq!(t.pixel_clock_khz, 533_000);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (3888, 3920, 4000)
        );
        assert_eq!(
            (t.v_sync_start, t.v_sync_end, t.v_total),
            (2163, 2168, 2222)
        );
    }

    // Generated with `cvt 1283 817 60 -r`:
    //   Modeline "1288x817R" 72.75 1288 1336 1368 1448 817 820 830 841
    //
    // Pins the horizontal-granularity round-UP direction: 1283 is not a
    // multiple of 8, and every other vector in this file already is, so
    // without this test the round-up rule (verified against `cvt(1)` in the
    // module doc comment) could silently regress to round-down and nothing
    // here would catch it. `h_total` alone would catch it only indirectly;
    // assert `h_active` directly.
    #[test]
    fn matches_cvt_for_1283x817_pins_horizontal_round_up() {
        let t = reduced_blanking(1283, 817, 60);
        assert_eq!(t.h_active, 1288);
        assert_eq!(t.pixel_clock_khz, 72_750);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (1336, 1368, 1448)
        );
        assert_eq!((t.v_sync_start, t.v_sync_end, t.v_total), (820, 830, 841));
    }

    // Generated with `cvt 1915 1080 60 -r`:
    //   Modeline "1915x1080R" 138.50 1920 1968 2000 2080 1080 1083 1088 1111
    //
    // Regression pin: the vertical sync width must be looked up against the
    // *rounded* h_active (1920, a 16:9 match -> vsync=5), not the raw input
    // width (1915, which matches no aspect table entry and would wrongly
    // fall back to the default vsync=10, producing v_sync_end=1093 and
    // v_total=1111 by coincidence but v_sync_end wrong along the way).
    #[test]
    fn matches_cvt_for_1915x1080_unaligned_aspect_regression() {
        let t = reduced_blanking(1915, 1080, 60);
        assert_eq!(t.h_active, 1920);
        assert_eq!(t.pixel_clock_khz, 138_500);
        assert_eq!(
            (t.h_sync_start, t.h_sync_end, t.h_total),
            (1968, 2000, 2080)
        );
        assert_eq!(
            (t.v_sync_start, t.v_sync_end, t.v_total),
            (1083, 1088, 1111)
        );
    }

    // Generated with `cvt 912 480 60 -r`:
    //   Modeline "912x480R" 31.50 912 960 992 1072 480 483 493 499
    //
    // Regression pin: truncating the internal `hperiod` before dividing to
    // get the pixel clock (the kernel `drm_cvt_mode()` formulation) yields
    // 31.75 MHz here — one 250 kHz step above the correct 31.50 MHz that
    // `cvt(1)` (which keeps full precision) produces.
    #[test]
    fn matches_cvt_for_912x480_clock_precision_regression() {
        let t = reduced_blanking(912, 480, 60);
        assert_eq!(t.pixel_clock_khz, 31_500);
        assert_eq!((t.h_sync_start, t.h_sync_end, t.h_total), (960, 992, 1072));
        assert_eq!((t.v_sync_start, t.v_sync_end, t.v_total), (483, 493, 499));
    }
}
