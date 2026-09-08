//! Serialises tests that mutate process-global environment variables.
//!
//! `cargo test` runs tests as threads within a single process, but
//! `std::env::set_var` / `remove_var` mutate state shared by the whole
//! process. Tests in `tile::classifier_decide_tests`, `transport::io_bridge`,
//! and `transport::bandwidth_cap` all read and write `GHOSTFRAME_*` env vars
//! (e.g. `GHOSTFRAME_FORCE_TILECODEC`, `GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY`)
//! to steer behaviour normally driven by the environment. Without
//! coordination, one test's `set_var` can be clobbered by another test's
//! `remove_var`/`set_var` mid-run, or a test that only *reads* a variable can
//! observe a value written by a concurrently running test — producing
//! failures that never reproduce in isolation.
//!
//! The fix mirrors the VA-API contention lock in
//! `encoder::h264_vaapi_tests` (see commit 92421c9), except this one must be
//! *shared* across modules, since the racing tests live in different files.
//! Any test that sets **or reads** a `GHOSTFRAME_*` env var must take this
//! lock for its full duration.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Hold for the duration of any test that sets or reads a `GHOSTFRAME_*` env var.
///
/// Poisoning is ignored: a panic in one such test must not cascade into
/// spurious failures in unrelated tests, which would obscure the real
/// failure.
pub(crate) fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
