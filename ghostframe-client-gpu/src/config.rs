//! The only module in this crate that reads environment variables.
//!
//! Mirrors the convention CI enforces on `ghostframe-lib`, where an
//! `env::var` call outside `config.rs` fails the build. Nothing here yet;
//! diagnostics knobs land with the renderer.
