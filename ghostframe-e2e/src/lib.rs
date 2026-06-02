//! ghostframe-e2e — e2e test harness library and the home of integration
//! tests that exercise the full Tailscale+QUIC+WebTransport+Chromium loop.
//!
//! The `harness` module is the public library API used by both the
//! `tests/` integration tests and the `codec_report` bench binary in
//! `ghostframe-bench`. See `docs/superpowers/specs/2026-06-01-m3.5-bench-publication-design.md`.

pub mod harness;
