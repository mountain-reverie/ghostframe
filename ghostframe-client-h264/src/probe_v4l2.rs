//! Can this machine decode H.264 on a V4L2 stateless decoder?
//!
//! The `v4l2` backend's half of the capability probe; `probe.rs` is the VA-API
//! one. Both expose the same three names -- [`h264_decode_available`],
//! [`default_device`], [`driver_reports_h264_decode`] -- so callers never cfg.
//!
//! Called on the connect path, before HELLO is built, because the capability
//! byte is sent once at session start and never revised. A `false` here is not
//! an error: the server then sends tile codecs, which is what every session did
//! before M3 -- and on this hardware that is also the common path, since most
//! desktop content is not video. It must stay clean and quiet.
//!
//! ## Why this runs a real decode
//!
//! It would be cheaper to stop at `VIDIOC_ENUM_FMT` reporting `S264`. That is
//! not enough, for the same shape of reason the VA-API probe gives for not
//! trusting `avcodec_get_hw_config`: enumerating a coded format proves the
//! *driver* advertises it, not that a decode completes. On this hardware there
//! is a second, sharper reason -- `cros-codecs 0.0.6` may open the wrong device
//! entirely (see [`crate::decoder`]), and a probe that only asked the driver
//! would answer `true` for a session that then produces nothing.
//!
//! Sessions begin in H.264 mode, so a false positive is a black window rather
//! than a degraded one.

use crate::v4l2_device;
use crate::H264Error;

/// The device this backend decodes on by default: whichever `/dev/videoN`
/// enumerates `S264`.
///
/// Discovered, not constant -- which is why the shared API is a function. Falls
/// back to a name that cannot exist so a caller passing it on gets a clean
/// refusal from [`crate::decoder::H264Decoder::with_device`] rather than
/// silently probing `/dev/video0`.
pub fn default_device() -> String {
    v4l2_device::find_h264_decoder()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/dev/video-no-h264-decoder".to_string())
}

/// True when this machine can decode H.264 on a stateless V4L2 decoder.
///
/// Decodes the same embedded 64x64 keyframe the VA-API probe uses and checks a
/// frame came out.
///
/// Deliberately does NOT test whether the decoded buffer can be imported into
/// the GPU -- that is a separate question, and conflating them would make an
/// import bug look like missing hardware.
pub fn h264_decode_available() -> bool {
    match probe_inner() {
        Ok(()) => true,
        Err(e) => {
            tracing::info!(reason = %e, "H.264 will not be advertised");
            false
        }
    }
}

/// The same clip the VA-API probe embeds, for the same reasons -- see
/// `probe.rs`'s `PROBE_CLIP` for why it must be High profile and how to
/// regenerate it. Shared file, one copy on disk.
const PROBE_CLIP: &[u8] = include_bytes!("probe_clip.h264");

fn probe_inner() -> Result<(), H264Error> {
    let mut decoder = crate::decoder::H264Decoder::new()?;
    let mut frames = decoder.decode(PROBE_CLIP)?;
    frames.extend(decoder.finish()?);
    let frame = frames.first().ok_or_else(|| {
        H264Error::V4l2Unavailable("the decoder accepted the stream but produced no frame".into())
    })?;
    if frame.width() == 0 || frame.height() == 0 {
        return Err(H264Error::V4l2Unavailable(
            "decoded probe frame has zero extent".into(),
        ));
    }
    Ok(())
}

/// Ground truth for whether this machine has a stateless H.264 decoder,
/// established independently of everything above.
///
/// `VIDIOC_ENUM_FMT` reporting `V4L2_PIX_FMT_H264_SLICE`, asked with raw
/// ioctls in [`crate::v4l2_device`] -- **not** through `cros-codecs`. That
/// independence is the whole value: an oracle that shares a code path with the
/// thing it validates is not an oracle, and here the shared path would be
/// precisely the device selection that has been wrong.
///
/// Always `Some`: unlike `vainfo`, there is no external tool that might be
/// missing, so "cannot establish ground truth" does not arise. `Option` is kept
/// only so the two backends present one signature.
///
/// Note this can legitimately disagree with [`h264_decode_available`]:
/// `Some(true)` with a `false` probe is exactly what an unpatched `cros-codecs`
/// produces, and a test that gates on this rather than on the probe will
/// correctly refuse to skip in that case.
#[cfg(any(test, feature = "test-support"))]
pub fn driver_reports_h264_decode(node: &str) -> Option<bool> {
    Some(v4l2_device::enumerates_h264_slice(std::path::Path::new(
        node,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The probe must answer without panicking on any machine, including one
    /// with no V4L2 decoder at all. It is called on the connect path, where a
    /// panic would take down a session over a missing optional feature.
    #[test]
    fn probe_returns_a_verdict_without_panicking() {
        let _device = v4l2_device::exclusive_device_access();
        let _verdict: bool = h264_decode_available();
    }

    /// On a machine whose driver enumerates `S264`, the probe must say so --
    /// unless the reason it cannot is the known device-selection defect, which
    /// this asserts is the *only* accepted excuse rather than letting any
    /// failure pass.
    #[test]
    fn probe_agrees_with_the_driver() {
        let _device = v4l2_device::exclusive_device_access();
        let node = default_device();
        let Some(true) = driver_reports_h264_decode(&node) else {
            eprintln!("no S264-capable device; nothing to check against");
            return;
        };
        if h264_decode_available() {
            return;
        }
        // The one tolerated disagreement: cros-codecs will open a *different*
        // device, so `with_device` refuses by design. Anything else is a real
        // regression.
        //
        // "Will open" rather than "would scan to": a stale
        // `CROS_CODECS_V4L2_DEVICE` counts as pointing elsewhere, and on this
        // hardware that is not hypothetical -- node numbering moves across
        // boots. Treating a set-but-wrong override as agreement is what let this
        // assertion pass while the decode went to hantro.
        let effective = v4l2_device::device_cros_codecs_will_open();
        let path = std::path::Path::new(&node);
        assert!(
            effective.as_deref() != Some(path),
            "the driver enumerates S264 on {node} and cros-codecs will open exactly \
             that, but the probe still said no"
        );
        eprintln!(
            "probe declined because cros-codecs will open {effective:?} rather than {node}; \
             apply tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch and set \
             {}={node}",
            v4l2_device::DEVICE_OVERRIDE_ENV
        );
    }
}
