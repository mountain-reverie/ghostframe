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
//! remembered copy of the spec. That mattered: a webfetched copy of the
//! Linux kernel's `drm_cvt_mode()` (drivers/gpu/drm/drm_modes.c) disagreed
//! with observed `cvt(1)` behavior on the direction of the horizontal
//! granularity rounding (kernel source read as round-down; `cvt(1)` rounds
//! up — `cvt 1921 1080 60 -r` yields h_active=1928, not 1920). Where the two
//! disagreed, `cvt(1)` won, per this task's stated ground truth.
//!
//! If this ever needs re-deriving, regenerate reference vectors with
//! `cvt <width> <height> <refresh> -r` and rebuild the arithmetic against
//! those step for step — the CVT algorithm is entirely integer/fixed-point,
//! and matching float math will drift off by rounding.

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

    // Vertical sync width: selected from the display's aspect ratio using
    // the *unrounded* width/height. Verified: 1280x1024 (5:4) needs
    // vsync=7 (`cvt 1280 1024 60 -r`), the 16:9 vectors all need vsync=5,
    // and an aspect matching no table entry falls back to 10
    // (`cvt 1920 1081 60 -r`, which is not exactly any listed ratio).
    let v_sync_width: u64 = if height.is_multiple_of(3) && height * 4 / 3 == width {
        4 // 4:3
    } else if height.is_multiple_of(9) && height * 16 / 9 == width {
        5 // 16:9
    } else if height.is_multiple_of(10) && height * 16 / 10 == width {
        6 // 16:10
    } else if (height.is_multiple_of(4) && height * 5 / 4 == width)
        || (height.is_multiple_of(9) && height * 15 / 9 == width)
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

    // Pixel clock from the horizontal totals and period, truncated DOWN to
    // a 0.25 MHz (250 kHz) step.
    let raw_khz = h_total * HV_FACTOR * 1000 / hperiod;
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
}
