//! E2E test fixtures (golden images, codec test vectors).

/// CDF53 codec fixture: ~1 KB of known-good encoded passes. Used by
/// `tests/e2e/*` to validate decode round-trips without re-running
/// the encoder on the test machine.
pub static CDF53_FIXTURE_JSON: &[u8] = include_bytes!("fixtures/cdf53_fixture.json");

/// A real desktop-capture screenshot (1920×1080 RGB, PNG-encoded).
/// Captured from a live ghostframe X session via `import -window root`
/// — includes UI chrome, text, gradient backgrounds, and subpixel
/// anti-aliasing, which the synthetic gradient test pattern doesn't
/// exercise. Used by `e2e_cdf53_real_capture_roundtrip_exact` to
/// verify the CPU encode→decode→inverse chain is byte-exact on real
/// content, not just on smooth ramps.
pub static REAL_CAPTURE_PNG: &[u8] = include_bytes!("fixtures/real_capture.png");
