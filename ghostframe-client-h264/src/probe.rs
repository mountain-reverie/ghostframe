//! Can this machine decode H.264 through VA-API?
//!
//! Called on the connect path, before HELLO is built, because the capability
//! byte is sent once at session start and never revised. A `false` here is
//! not an error: the server then sends tile codecs, which is what every
//! session did before M3.

use crate::{BufRef, H264Error};
use ffmpeg_sys_next as ffi;
use std::ffi::CString;
use std::ptr;

/// Default VA-API render node. Matches
/// `ghostframe-lib/src/encoder/vaapi_device.rs`'s `VAAPI_DEVICE`.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";

/// True when this machine can plausibly decode H.264 through VA-API.
///
/// **This is a necessary condition, not a sufficient one, and Task 3 replaces
/// it with a functional check.** `avcodec_find_decoder(AV_CODEC_ID_H264)`
/// returns libavcodec's *software* decoder and is entirely independent of
/// VA-API -- there is no `h264_vaapi` decoder, only an `h264` decoder with a
/// VA-API hwaccel. So the strongest thing reachable without decoding a frame
/// is: a VA-API device opens, AND libavcodec was built with a VA-API hwaccel
/// for H.264. A driver whose H.264 profile is missing entirely (Mesa built
/// without `video-codecs`, which several distributions shipped for years)
/// still passes this.
///
/// That gap matters because sessions begin in H.264 mode, so a false positive
/// is a black window rather than a degraded one. It is tolerable only because
/// nothing advertises the capability until Task 10, by which point Task 3 has
/// made this check functional.
///
/// Deliberately does NOT test whether the decoded surface can be imported
/// into Vulkan -- that is a separate question (see the design doc §7), and
/// conflating them would make an import bug look like missing hardware.
pub fn vaapi_h264_decode_available() -> bool {
    // ffmpeg logs libva failures straight to stderr at its default level. On a
    // machine with no VA-API -- the outcome this whole design calls normal --
    // that puts an unstructured error line on the host application's stderr,
    // which a C host embedding this library cannot suppress. Quiet it for the
    // duration of the probe and restore afterwards.
    // SAFETY: `av_log_get_level`/`av_log_set_level` are plain global accessors.
    let prior_log_level = unsafe {
        let prior = ffi::av_log_get_level();
        ffi::av_log_set_level(ffi::AV_LOG_QUIET);
        prior
    };
    let verdict = probe_inner();
    // SAFETY: as above; restores exactly what was read.
    unsafe { ffi::av_log_set_level(prior_log_level) };
    match &verdict {
        Ok(()) => true,
        Err(e) => {
            tracing::info!(reason = %e, "H.264 will not be advertised");
            false
        }
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
    let path = CString::new(RENDER_NODE)
        .map_err(|_| H264Error::VaapiUnavailable(format!("bad device path {RENDER_NODE:?}")))?;

    // RAII: `BufRef` unrefs on drop, so every early return below is leak-free
    // without repeating the cleanup. Mirrors
    // `ghostframe-lib/src/encoder/vaapi_device.rs`'s wrapper of the same name.
    let mut raw: *mut ffi::AVBufferRef = ptr::null_mut();
    // SAFETY: `path` outlives the call; `raw` is a valid out-param that ffmpeg
    // leaves null on failure, which the check below respects before any deref.
    let ret = unsafe {
        ffi::av_hwdevice_ctx_create(
            &mut raw,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            ptr::null_mut(),
            0,
        )
    };
    if ret < 0 {
        // Render the errno: -13 (permission -- not in the `render` group) and
        // -2 (no such device) are the two common causes and they need
        // completely different fixes. A bare negative integer tells a user
        // nothing.
        return Err(H264Error::VaapiUnavailable(format!(
            "av_hwdevice_ctx_create({RENDER_NODE}): {}",
            ffmpeg_next::Error::from(ret)
        )));
    }
    let _device = BufRef(raw);

    // SAFETY: a plain registry lookup with no ownership transfer.
    let codec = unsafe { ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264) };
    if codec.is_null() {
        return Err(H264Error::Ffmpeg("no H.264 decoder in ffmpeg".into()));
    }

    // Does libavcodec actually carry a VA-API hwaccel for H.264? Without this
    // the check above is satisfied by the software decoder on every build.
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
            return Ok(());
        }
        i += 1;
    }
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
        let vainfo = std::process::Command::new("vainfo").output();
        let Ok(out) = vainfo else {
            eprintln!("vainfo not installed; cannot establish ground truth, skipping");
            return;
        };
        let text = String::from_utf8_lossy(&out.stdout);
        let driver_decodes_h264 = text
            .lines()
            .any(|l| l.contains("VAProfileH264") && l.contains("VAEntrypointVLD"));
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
