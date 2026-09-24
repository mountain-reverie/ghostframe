//! Can this machine decode H.264 through VA-API?
//!
//! Called on the connect path, before HELLO is built, because the capability
//! byte is sent once at session start and never revised. A `false` here is
//! not an error: the server then sends tile codecs, which is what every
//! session did before M3.

use crate::H264Error;
use ffmpeg_sys_next as ffi;

/// Default VA-API render node. Matches
/// `ghostframe-lib/src/encoder/vaapi_device.rs`'s `VAAPI_DEVICE`.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";

/// True when this machine can decode H.264 through VA-API.
///
/// Decodes a 64x64 keyframe (embedded, see [`PROBE_CLIP`]) through VA-API and
/// returns whether a hardware frame came out. This is the only check that
/// actually proves the driver can do it: `avcodec_find_decoder` and
/// `avcodec_get_hw_config` alone only prove libavcodec was *built* with a
/// VA-API hwaccel for H.264, not that the driver has an H.264 decode profile
/// -- Mesa built without `video-codecs` passes those checks and still fails
/// here.
///
/// That gap matters because sessions begin in H.264 mode, so a false positive
/// is a black window rather than a degraded one.
///
/// Deliberately does NOT test whether the decoded surface can be imported
/// into Vulkan -- that is a separate question (see the design doc §7), and
/// conflating them would make an import bug look like missing hardware.
pub fn vaapi_h264_decode_available() -> bool {
    // ffmpeg logs libva failures straight to stderr at its default level. On a
    // machine with no VA-API -- the outcome this whole design calls normal --
    // that puts an unstructured error line on the host application's stderr,
    // which a C host embedding this library cannot suppress. Quiet it for the
    // duration of the probe and restore afterwards. `_restore` is a Drop
    // guard rather than a manual restore-at-the-end: a panic inside
    // `probe_inner` (or `probe_decodes_a_frame`, which now runs a real
    // decode loop with more failure surface than the pointer-chasing this
    // used to be) must not unwind past the restore and leave ffmpeg globally
    // quiet for the rest of the process. `QuietLogGuard::new()` also
    // serializes against every other caller (see its doc) -- no separate
    // lock needed here.
    let _restore = QuietLogGuard::new();
    let verdict = probe_inner();
    match &verdict {
        Ok(()) => true,
        Err(e) => {
            tracing::info!(reason = %e, "H.264 will not be advertised");
            false
        }
    }
}

/// RAII guard: mutes ffmpeg's global log level and restores the prior level
/// on drop, including on unwind. See [`vaapi_h264_decode_available`] for why
/// a bare save/restore pair is not unwind-safe.
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

/// The probe proper, returning WHY it failed.
///
/// Separate from the `bool` wrapper so the reason survives: `H264Error` is how
/// `gf_client_supports_h264`'s caller could one day learn the difference
/// between "no GPU", "permission denied on the render node", and "this driver
/// has no H.264 decode profile" -- three very different things for a user to
/// act on, and a bare `false` flattens them.
fn probe_inner() -> Result<(), H264Error> {
    // SAFETY: a plain registry lookup with no ownership transfer.
    let codec = unsafe { ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264) };
    if codec.is_null() {
        return Err(H264Error::Ffmpeg("no H.264 decoder in ffmpeg".into()));
    }

    // Does libavcodec actually carry a VA-API hwaccel for H.264? Without this
    // the check above is satisfied by the software decoder on every build.
    //
    // No device is opened here: that used to happen in this function too
    // (a first `av_hwdevice_ctx_create` held for the whole probe), which
    // meant two live `vaInitialize` calls on the same node whenever the
    // hwaccel config matched -- this one, and the second one
    // `probe_decodes_a_frame` opens via `H264Decoder::new()`, both inside
    // the global `LOG_MUTE` above. The first open's only remaining value was
    // distinguishing "device won't open" from "no hwaccel built", and
    // `H264Decoder::new()` already returns `VaapiUnavailable` with the
    // rendered ffmpeg error for exactly that, so the duplicate bought
    // nothing.
    let mut i = 0;
    loop {
        // SAFETY: `codec` is a valid static codec descriptor; ffmpeg returns
        // null past the end of the config list, which terminates the loop.
        let cfg = unsafe { ffi::avcodec_get_hw_config(codec, i) };
        if cfg.is_null() {
            return Err(H264Error::VaapiUnavailable(
                "libavcodec has no VA-API hwaccel for H.264 (built without it)".into(),
            ));
        }
        // SAFETY: `cfg` is non-null and points at a live static config.
        let (methods, device_type) = unsafe { ((*cfg).methods, (*cfg).device_type) };
        let has_device_ctx = methods & ffi::AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX as i32 != 0;
        if has_device_ctx && device_type == ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI {
            return probe_decodes_a_frame();
        }
        i += 1;
    }
}

/// A 64x64 gray H.264 keyframe, Annex-B, High profile, ~758 bytes. Regenerate
/// with:
///
/// ```text
/// ffmpeg -f lavfi -i color=c=gray:s=64x64:d=1 -frames:v 1 \
///   -c:v libx264 -preset veryfast -profile:v high -f h264 \
///   -y src/probe_clip.h264
/// ffprobe -loglevel error -show_entries stream=profile -of csv=p=0 \
///   src/probe_clip.h264   # MUST print: High
/// ```
///
/// Embedded rather than encoded at runtime so the probe does not depend on
/// libx264 being present in the host's ffmpeg build, and costs no encode on
/// the connect path.
///
/// **Must be High profile, because that is what the server sends.**
/// `ghostframe-lib/src/encoder/h264_vaapi.rs` sets no profile, so ffmpeg's
/// `h264_vaapi` encoder defaults to High, and VA-API advertises
/// ConstrainedBaseline / Main / High as separate *decode* profiles -- a
/// driver carrying High but not ConstrainedBaseline would fail a CBP probe
/// clip and silently lose H.264 it can decode perfectly well. Note
/// `-preset ultrafast` cannot produce High at all: it disables CABAC and
/// 8x8 DCT, the features that make it High, so `-profile:v high` alone
/// silently still yields Constrained Baseline -- hence `veryfast` above, and
/// the `ffprobe` check, which is there because the profile is not what the
/// `-profile:v` flag alone determines.
const PROBE_CLIP: &[u8] = include_bytes!("probe_clip.h264");

/// Decode one frame through VA-API. The only check that actually proves the
/// driver can do it.
fn probe_decodes_a_frame() -> Result<(), H264Error> {
    let mut decoder = crate::decoder::H264Decoder::new()?;
    let mut frames = decoder.decode(PROBE_CLIP)?;
    frames.extend(decoder.finish()?);
    let frame = frames.first().ok_or_else(|| {
        H264Error::VaapiUnavailable(
            "VA-API accepted the stream but produced no frame (driver likely has no \
             H.264 decode profile)"
                .into(),
        )
    })?;
    if frame.width() == 0 || frame.height() == 0 {
        return Err(H264Error::VaapiUnavailable(
            "decoded probe frame has zero extent".into(),
        ));
    }
    Ok(())
}

/// Ground truth for whether this driver has a **High**-profile H.264 VLD
/// decode entrypoint on `node`, established independently of anything else
/// in this crate.
///
/// `Some(true)`/`Some(false)` is what `vainfo` reports. `None` means there is
/// no ground truth to check against -- `vainfo` is missing, or exited
/// non-zero (`Command::output()` returns `Ok` even for a failed child, so
/// this checks `status.success()` rather than just reading stdout) -- and
/// callers should skip rather than guess.
///
/// Matches `VAProfileH264High` specifically, not a bare `VAProfileH264`
/// substring: the latter also matches `VAProfileH264ConstrainedBaseline`,
/// `...StereoHigh` and `...MultiviewHigh`. [`PROBE_CLIP`]'s doc comment
/// above explains at length why the probe itself must be High profile --
/// VA-API treats each profile as a separate decode capability, and a driver
/// exposing only ConstrainedBaseline correctly fails a High probe clip. A
/// profile-agnostic ground truth would call that driver's correct `false` a
/// bug: `Some(true)` from CBP-only `vainfo` output, the High probe clip
/// failing, and `probe_agrees_with_the_driver` asserting a failure that was
/// actually the right answer.
///
/// Takes `node` rather than hardcoding [`RENDER_NODE`] so a caller can name
/// whichever device it actually opened -- `vainfo --device` defaults to
/// nothing usable and `H264Decoder::with_device` already takes one. Baking
/// in `RENDER_NODE` here made every gated test on a box whose GPU is *not*
/// `renderD128` (a `renderD129` multi-GPU box, say) establish ground truth
/// against a device nobody opened: `vainfo --device renderD128` on such a
/// box exits non-zero, this returns `None`, and every test gated on it
/// silently skips -- green without having run, the exact failure mode this
/// project's CI-exemption rule exists to prevent for production code, now
/// showing up in the tests meant to catch it.
///
/// Cached per `node` behind a process-lifetime [`OnceLock`](std::sync::OnceLock):
/// roughly ten tests in this crate alone gate on this, each of which would
/// otherwise spawn `vainfo` and pay a full `vaInitialize` on the shared
/// libva instance, and the answer cannot change within a test run.
///
/// Shared by [`vaapi_h264_decode_available`]'s own test
/// (`probe_agrees_with_the_driver`, which checks the probe's verdict against
/// this), `decoder::tests::skip_without_vaapi`, and the oracles in
/// `oracle_tests` -- all of which gate on this and NOT on
/// `vaapi_h264_decode_available` -- since the probe now decodes a real
/// frame, gating on it would be circular: a regression in the decoder would
/// make the probe return `false`, which would make every test that skips on
/// it report green exactly when it should fail.
///
/// `pub` and re-exported at the crate root -- not just `pub(crate)` --
/// because other crates' own tests (`ghostframe-client-gpu`,
/// `ghostframe-e2e`) need the same ground truth and reach it through the
/// `test-support` feature (off by default; see `Cargo.toml`) rather than a
/// self-referencing dev-dependency on this crate. `#[cfg(test)]` alone
/// would not reach them: `#[cfg(test)]` is only active when THIS crate is
/// itself being tested, never when it is compiled as an ordinary dependency
/// for another crate's test build.
#[cfg(any(test, feature = "test-support"))]
pub fn vainfo_reports_h264_vld(node: &str) -> Option<bool> {
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock};

    static CACHE: OnceLock<Mutex<HashMap<String, Option<bool>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));

    if let Some(cached) = cache.lock().unwrap_or_else(|e| e.into_inner()).get(node) {
        return *cached;
    }

    let result = vainfo_reports_h264_vld_uncached(node);
    cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(node.to_string(), result);
    result
}

/// The actual `vainfo` invocation behind [`vainfo_reports_h264_vld`]'s cache.
#[cfg(any(test, feature = "test-support"))]
fn vainfo_reports_h264_vld_uncached(node: &str) -> Option<bool> {
    let vainfo = std::process::Command::new("vainfo")
        .args(["--display", "drm", "--device", node])
        .output();
    let Ok(out) = vainfo else {
        eprintln!("vainfo not installed; cannot establish ground truth, skipping");
        return None;
    };
    if !out.status.success() {
        eprintln!(
            "vainfo exited with {:?}; cannot establish ground truth, skipping",
            out.status
        );
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .any(|l| l.contains("VAProfileH264High") && l.contains("VAEntrypointVLD")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer without panicking on any machine, including one
    /// with no GPU at all. It is called on the connect path, where a panic
    /// would take down a session over a missing optional feature.
    #[test]
    fn probe_returns_a_verdict_without_panicking() {
        let _verdict: bool = vaapi_h264_decode_available();
    }

    /// On a machine whose driver reports an H.264 decode entrypoint, the probe
    /// must say so.
    ///
    /// The gate is `vainfo`, NOT the mere existence of a render node. A node
    /// exists on machines whose Mesa was built without video codecs, where the
    /// correct answer is `false` -- gating on the node would demand the wrong
    /// answer there and entrench the very defect the probe exists to avoid.
    #[test]
    fn probe_agrees_with_the_driver() {
        let Some(driver_decodes_h264) = vainfo_reports_h264_vld(RENDER_NODE) else {
            return;
        };
        if !driver_decodes_h264 {
            eprintln!("driver reports no H.264 VLD entrypoint; probe should say false");
            assert!(
                !vaapi_h264_decode_available(),
                "the driver reports no H.264 decode entrypoint, but the probe said yes -- \
                 this is the false positive that produces a black window on first paint"
            );
            return;
        }
        assert!(
            vaapi_h264_decode_available(),
            "`vainfo` reports a VAProfileH264*/VAEntrypointVLD pair but the probe says no"
        );
    }
}
