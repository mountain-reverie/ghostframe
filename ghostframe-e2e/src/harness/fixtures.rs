//! E2E test fixtures (golden images, codec test vectors).

/// CDF53 codec fixture: ~1 KB of known-good encoded passes. Used by
/// `tests/e2e/*` to validate decode round-trips without re-running
/// the encoder on the test machine.
pub static CDF53_FIXTURE_JSON: &[u8] = include_bytes!("fixtures/cdf53_fixture.json");
