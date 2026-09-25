//! One-off verification tool: sweeps a wide range of width/height/refresh
//! combinations through `cvt::reduced_blanking` and compares every field
//! against the real `cvt(1)` binary (package `libxcvt`), the oracle this
//! module is built to match.
//!
//! This is deliberately an **example**, not a `#[test]` and not a
//! `src/bin/*` binary. Not a test because it shells out to an external
//! process for thousands of cases, which is far too slow and
//! environment-dependent for `cargo test`. Not a `src/bin/*` because
//! `cargo build -p ghostframe-xdaemon` builds every binary in the package,
//! and the e2e container image does exactly that
//! (`tests/containers/test-server/Dockerfile`) — a dev-only tool would be
//! compiled into and shipped with that image. `cargo build -p` does not
//! build examples, while `clippy --all-targets` still does, so this stays
//! lint-clean without riding along into the container.
//!
//! Run it by hand before touching the arithmetic in `../src/cvt.rs`:
//!
//! ```text
//! cargo run -p ghostframe-xdaemon --example cvt_sweep
//! ```
//!
//! It re-includes `../cvt.rs` verbatim via `#[path]` (not a copy), so it can
//! never silently drift out of sync with what actually ships.
//!
//! Known, intentional divergence: 1354x768 and 1360x768 hit a `cvt(1)`
//! special case for the "1366x768" laptop-panel resolution (it snaps the
//! *active pixel count* itself to 1366, a non-multiple-of-8 value, rather
//! than applying the general rounding rule) — see the module doc comment on
//! `../cvt.rs` for detail. This sweep expects exactly those two mismatches
//! and reports anything beyond them as a failure.

#[path = "../src/cvt.rs"]
mod cvt;

use std::process::Command;

/// Resolutions known to hit `cvt(1)`'s "1366x768 panel" special case and
/// intentionally NOT matched by this module. See the module doc comment.
/// `cvt(1)` snaps a range of nearby input widths into this bucket (observed:
/// 1354 and 1360; there may be others nearby that this sweep's width step
/// doesn't happen to land on).
const KNOWN_EXCEPTIONS: &[(u16, u16)] = &[(1354, 768), (1360, 768)];

struct CvtFields {
    pixel_clock_khz: u32,
    h_active: u16,
    h_sync_start: u16,
    h_sync_end: u16,
    h_total: u16,
    v_active: u16,
    v_sync_start: u16,
    v_sync_end: u16,
    v_total: u16,
}

/// Runs `cvt <width> <height> <refresh> -r` and parses its Modeline output.
fn run_cvt(width: u16, height: u16, refresh_hz: u16) -> Option<CvtFields> {
    let output = Command::new("cvt")
        .args([
            width.to_string(),
            height.to_string(),
            refresh_hz.to_string(),
            "-r".to_string(),
        ])
        .output()
        .expect("failed to run cvt(1) - is the libxcvt package installed?");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let modeline = stdout.lines().find(|l| l.starts_with("Modeline"))?;

    // `Modeline "1920x1080R"  138.50  1920 1968 2000 2080  1080 1083 1088 1111 +hsync -vsync`
    let mut fields = modeline.split_whitespace();
    fields.next()?; // "Modeline"
    fields.next()?; // quoted name, e.g. "1920x1080R"
    let pclk_mhz: f64 = fields.next()?.parse().ok()?;
    let h_active: u16 = fields.next()?.parse().ok()?;
    let h_sync_start: u16 = fields.next()?.parse().ok()?;
    let h_sync_end: u16 = fields.next()?.parse().ok()?;
    let h_total: u16 = fields.next()?.parse().ok()?;
    let v_active: u16 = fields.next()?.parse().ok()?;
    let v_sync_start: u16 = fields.next()?.parse().ok()?;
    let v_sync_end: u16 = fields.next()?.parse().ok()?;
    let v_total: u16 = fields.next()?.parse().ok()?;

    Some(CvtFields {
        pixel_clock_khz: (pclk_mhz * 1000.0).round() as u32,
        h_active,
        h_sync_start,
        h_sync_end,
        h_total,
        v_active,
        v_sync_start,
        v_sync_end,
        v_total,
    })
}

fn fields_match(ours: &cvt::Timing, oracle: &CvtFields) -> bool {
    ours.pixel_clock_khz == oracle.pixel_clock_khz
        && ours.h_active == oracle.h_active
        && ours.h_sync_start == oracle.h_sync_start
        && ours.h_sync_end == oracle.h_sync_end
        && ours.h_total == oracle.h_total
        && ours.v_active == oracle.v_active
        && ours.v_sync_start == oracle.v_sync_start
        && ours.v_sync_end == oracle.v_sync_end
        && ours.v_total == oracle.v_total
}

fn main() {
    let heights: &[u16] = &[
        480, 600, 720, 768, 800, 900, 1024, 1050, 1080, 1200, 1350, 1440, 1600, 2160,
    ];
    let refreshes: &[u16] = &[60, 120];

    let mut total = 0u32;
    let mut mismatches = 0u32;
    let mut unexpected = 0u32;

    let mut width = 640u16;
    while width <= 3848 {
        for &height in heights {
            for &refresh_hz in refreshes {
                total += 1;
                let Some(oracle) = run_cvt(width, height, refresh_hz) else {
                    eprintln!(
                        "WARN: cvt(1) produced no Modeline for {width}x{height}@{refresh_hz}"
                    );
                    continue;
                };
                let ours = cvt::reduced_blanking(width, height, refresh_hz);
                if !fields_match(&ours, &oracle) {
                    mismatches += 1;
                    let expected = KNOWN_EXCEPTIONS.contains(&(width, height));
                    if !expected {
                        unexpected += 1;
                    }
                    println!(
                        "{}MISMATCH {width}x{height}@{refresh_hz}: ours=[{} {} {} {} {} {} {} {} {}] cvt=[{} {} {} {} {} {} {} {} {}]",
                        if expected { "(known) " } else { "" },
                        ours.pixel_clock_khz, ours.h_active, ours.h_sync_start, ours.h_sync_end, ours.h_total,
                        ours.v_active, ours.v_sync_start, ours.v_sync_end, ours.v_total,
                        oracle.pixel_clock_khz, oracle.h_active, oracle.h_sync_start, oracle.h_sync_end, oracle.h_total,
                        oracle.v_active, oracle.v_sync_start, oracle.v_sync_end, oracle.v_total,
                    );
                }
            }
        }
        width += 7;
    }

    println!(
        "\n{total} cases, {mismatches} mismatches ({unexpected} unexpected, {} known 1366-panel exceptions)",
        mismatches - unexpected
    );
    if unexpected > 0 {
        eprintln!("FAIL: {unexpected} unexpected mismatch(es) against cvt(1)");
        std::process::exit(1);
    }
    println!("OK: only known exceptions diverge from cvt(1)");
}
