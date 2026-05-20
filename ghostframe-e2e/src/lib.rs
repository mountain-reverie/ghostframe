//! Empty library — this crate exists to host integration tests in `tests/`.
//!
//! Heavy e2e dependencies live in this crate's `[dev-dependencies]` so that
//! the lib's `cargo test --lib` doesn't have to compile them. See
//! `docs/superpowers/specs/2026-05-20-workspace-test-crate-split-design.md`
//! for the rationale.
