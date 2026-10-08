//! Can this machine decode H.264 through GStreamer?
//!
//! The `gstreamer` backend's half of the capability probe; `probe.rs` is the
//! VA-API one. Both expose the same three names -- [`h264_decode_available`],
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
//! *driver* advertises it, not that a decode completes.
//!
//! On this backend there is a second, sharper reason. Everything that can go
//! wrong goes wrong in *negotiation*, not in the driver: a GStreamer missing the
//! `v4l2codecs` plugin, older than 1.24.1, or unable to agree dmabuf caps all
//! leave a pipeline that builds and then produces nothing. Only a real decode
//! distinguishes those from a working one.
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

/// Minimum GStreamer this backend needs, and why.
///
/// 1.24.1 is where `v4l2codecs` gained DMA_DRM caps, which is the only way to
/// *require* a dmabuf rather than hope for one. Below it the decoder silently
/// hands back system memory at some resolutions -- measured: dmabuf at 640x480
/// and a full-frame CPU copy at 1080p on 1.22.10. The gstreamer crates' `v1_24`
/// feature makes this a link-time requirement too; this constant is for saying
/// so in a log line rather than in a linker error.
pub const MIN_GSTREAMER: (u32, u32, u32) = (1, 24, 1);

/// True when this machine can decode H.264 through GStreamer.
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
    // Version first, so "GStreamer is too old" reads as that rather than as a
    // negotiation failure five layers down.
    let (major, minor, micro, _) = gstreamer::version();
    if (major, minor, micro) < MIN_GSTREAMER {
        return Err(H264Error::Gst(format!(
            "GStreamer {major}.{minor}.{micro} is older than the {}.{}.{} this backend \
             needs for DMA_DRM caps; without them a dmabuf cannot be required and the \
             decoder may hand back system memory",
            MIN_GSTREAMER.0, MIN_GSTREAMER.1, MIN_GSTREAMER.2
        )));
    }
    let mut decoder = crate::decoder::H264Decoder::new()?;
    let mut frames = decoder.decode(PROBE_CLIP)?;
    frames.extend(decoder.finish()?);
    let frame = frames.first().ok_or_else(|| {
        H264Error::Gst("the pipeline accepted the stream but produced no frame".into())
    })?;
    if frame.width() == 0 || frame.height() == 0 {
        return Err(H264Error::Gst("decoded probe frame has zero extent".into()));
    }
    Ok(())
}

/// Ground truth for whether this machine has a stateless H.264 decoder,
/// established independently of everything above.
///
/// `VIDIOC_ENUM_FMT` reporting `V4L2_PIX_FMT_H264_SLICE`, asked with raw ioctls
/// in [`crate::v4l2_device`] -- **not** through GStreamer. That independence is
/// the whole value: an oracle sharing a code path with the thing it validates is
/// not an oracle. It is also why that module survived the move off cros-codecs
/// when the rest of the V4L2 plumbing did not.
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

    /// On a machine whose driver enumerates `S264`, the probe must say so.
    ///
    /// No tolerated disagreement. An earlier version of this test accepted one
    /// -- cros-codecs opening a different node than the one asked for -- and
    /// that escape hatch is how a stale device override once hid a real failure
    /// for a whole test run. GStreamer selects the device itself, so there is
    /// nothing left to excuse: a driver that advertises `S264` and a probe that
    /// says no is a bug.
    #[test]
    fn probe_agrees_with_the_driver() {
        let _device = v4l2_device::exclusive_device_access();
        let node = default_device();
        let Some(true) = driver_reports_h264_decode(&node) else {
            eprintln!("no S264-capable device; nothing to check against");
            return;
        };
        assert!(
            h264_decode_available(),
            "the driver enumerates S264 on {node} but the probe said no"
        );
    }
}
