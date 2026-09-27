//! Muting ffmpeg's global log level, safely.
//!
//! Lives apart from `probe.rs` because it is not a probe concern and not a
//! backend concern: it is needed wherever libavcodec is linked. That is the
//! `vaapi` backend, and *also* a `v4l2` build with `test-support`, where ffmpeg
//! is the software golden and libx264 the clip generator -- see `Cargo.toml`.

use ffmpeg_sys_next as ffi;

/// RAII guard: mutes ffmpeg's global log level and restores the prior level
/// on drop, including on unwind. See
/// [`crate::probe::h264_decode_available`] for why a bare save/restore pair is
/// not unwind-safe.
///
/// `pub(crate)`, not private: `testclip::gradient_clip` uses this too, to
/// quiet libx264's own per-frame stats spam (`av_log`-ged at INFO) rather
/// than let it flood every `cargo test` in the workspace.
///
/// Holds a lock on a shared, process-wide mutex for its entire lifetime,
/// not just a save/restore pair on the log level: `av_log_set_level` is an
/// unsynchronized global, so two concurrent holders (this crate's own test
/// suite alone has the probe's tests and ~10 gated oracle/decoder tests,
/// all under `cargo test`'s default parallelism, and some of *those* call
/// `gradient_clip`, which now takes this guard too) can interleave
/// save/save/restore/restore and strand the level at QUIET -- or, worse,
/// leave a window where it is back at the noisy default while a second
/// caller's encode/decode is mid-flight, which is what let libx264's stats
/// spam through even after `gradient_clip` started taking a guard, until
/// this lock was folded in here rather than left to each call site to
/// remember separately. Holding the lock for a guard's whole lifetime also
/// means one holder briefly suppresses every other thread's ffmpeg logging
/// -- an accepted cost, since the alternative is unsuppressable stderr
/// noise on paths this design calls normal.
pub(crate) struct QuietLogGuard {
    // The guarded data is `()` -- poisoning carries no meaning here, only
    // "some earlier guard-holder panicked while holding the lock".
    // `PoisonError` still owns the guard, so recovering it with
    // `into_inner` still serializes correctly; it is not a bypass.
    // Deliberately not `.unwrap()`: that turns a benign poison into a panic
    // on the connect path (`vaapi_h264_decode_available`) or a test.
    _lock: std::sync::MutexGuard<'static, ()>,
    prior: libc::c_int,
}

impl QuietLogGuard {
    pub(crate) fn new() -> Self {
        static LOG_MUTE: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let _lock = LOG_MUTE
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // SAFETY: `av_log_get_level`/`av_log_set_level` are plain global
        // accessors with no preconditions.
        let prior = unsafe {
            let prior = ffi::av_log_get_level();
            ffi::av_log_set_level(ffi::AV_LOG_QUIET);
            prior
        };
        QuietLogGuard { _lock, prior }
    }
}

impl Drop for QuietLogGuard {
    fn drop(&mut self) {
        // SAFETY: as above; restores exactly what was read in `new`, even if
        // we get here by unwinding out of `probe_inner` or a caller's own
        // code. `_lock` releases after this, once `Drop` returns.
        unsafe { ffi::av_log_set_level(self.prior) };
    }
}
