//! E2E test fixtures (golden images, codec test vectors).
//!
//! The `tests/fixtures/cdf53_fixture.json` lives at the original path
//! for git-history continuity; this module exposes its bytes via
//! `include_bytes!` for harness-library consumers.

/// CDF53 codec fixture: ~1 KB of known-good encoded passes. Used by
/// `tests/e2e/*` to validate decode round-trips without re-running
/// the encoder on the test machine.
pub static CDF53_FIXTURE_JSON: &[u8] =
    include_bytes!("../../tests/fixtures/cdf53_fixture.json");
