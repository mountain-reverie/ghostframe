//! Can this machine decode H.264 through VA-API?
//!
//! Called on the connect path, before HELLO is built, because the capability
//! byte is sent once at session start and never revised. A `false` here is
//! not an error: the server then sends tile codecs, which is what every
//! session did before M3.

use ffmpeg_sys_next as ffi;
use std::ffi::CString;
use std::ptr;

/// Default VA-API render node. Matches
/// `ghostframe-lib/src/encoder/vaapi_device.rs`'s `VAAPI_DEVICE`.
pub const RENDER_NODE: &str = "/dev/dri/renderD128";

/// True when a VA-API device opens AND ffmpeg has an H.264 decoder that can
/// use it.
///
/// Deliberately does NOT test whether the decoded surface can be imported
/// into Vulkan -- that is a separate question (see the design doc §7), and
/// conflating them would make an import bug look like missing hardware.
pub fn vaapi_h264_decode_available() -> bool {
    let Ok(path) = CString::new(RENDER_NODE) else {
        return false;
    };

    // SAFETY: `path` is a valid NUL-terminated string that outlives the call;
    // `hw_dev` is a valid out-param. On failure ffmpeg leaves it null and we
    // never deref it.
    unsafe {
        let mut hw_dev: *mut ffi::AVBufferRef = ptr::null_mut();
        let ret = ffi::av_hwdevice_ctx_create(
            &mut hw_dev,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            path.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if ret < 0 {
            tracing::info!(
                ret,
                node = RENDER_NODE,
                "VA-API device did not open; H.264 will not be advertised"
            );
            return false;
        }
        ffi::av_buffer_unref(&mut hw_dev);

        let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
        if codec.is_null() {
            tracing::info!("ffmpeg has no H.264 decoder; H.264 will not be advertised");
            return false;
        }
    }
    true
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

    /// On a machine that reports VA-API H.264 decode, the probe must say so.
    /// Skipped where the device node is absent, because there the correct
    /// answer is genuinely `false` and asserting `true` would be asserting
    /// the hardware, not the code.
    #[test]
    fn probe_agrees_with_the_render_node() {
        if !std::path::Path::new("/dev/dri/renderD128").exists() {
            eprintln!("no /dev/dri/renderD128; skipping");
            return;
        }
        assert!(
            vaapi_h264_decode_available(),
            "a render node exists but the probe says no H.264 decode -- if this \
             machine genuinely lacks it, check `vainfo | grep VAProfileH264`"
        );
    }
}
