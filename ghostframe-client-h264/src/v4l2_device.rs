//! Which `/dev/videoN` is the stateless H.264 decoder — asked with raw ioctls.
//!
//! `probe.rs` needs an **independent ground truth** for the oracles: "does this
//! machine have a hardware H.264 decoder?" answered without going through the
//! thing being validated. That is the same role `vainfo_reports_h264_vld` plays
//! for the VA-API backend, and it only works if it shares no code with the
//! decode path. Reaching the answer through `cros_codecs::v4l2r` would share the
//! dependency that selects and opens the device.
//!
//! So: `VIDIOC_ENUM_FMT`, and nothing else.
//!
//! [`find_h264_decoder`] is also how the decoder picks its node, and why
//! `probe::default_device()` is a function rather than a constant: `/dev/videoN`
//! numbering is **not stable across boots**. On the reference machine rkvdec and
//! the hantro decoder swapped places, video3 and video1, over a single reboot.
//! Enumerating `S264` is the only durable way to name the right one.
//!
//! This module used to be twice this size, predicting which node a
//! `cros-codecs` scan would settle on so the decoder could refuse a mismatch.
//! GStreamer selects the device itself, so all of that is gone and this is the
//! only part that survived the move — because an oracle sharing a code path with
//! the thing it validates is not an oracle, and `VIDIOC_ENUM_FMT` shares nothing
//! with a GStreamer pipeline.

use std::ffi::c_ulong;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// `V4L2_PIX_FMT_H264_SLICE` — `S264`, the coded format a *stateless* decoder
/// accepts. Not `H264`: that fourcc is what a stateful decoder takes, and the
/// difference is the whole reason the Request API is involved.
const V4L2_PIX_FMT_H264_SLICE: u32 = fourcc(b'S', b'2', b'6', b'4');

const V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE: u32 = 10;

/// `_IOWR('V', 2, struct v4l2_fmtdesc)`, expanded by hand.
///
/// aarch64 and x86-64 agree on the encoding (dir 2 bits at 30, size 14 bits at
/// 16, type at 8, nr at 0) and on `size_of::<v4l2_fmtdesc>() == 64`, so one
/// constant covers both. `_IOC_READ | _IOC_WRITE` is `3 << 30`.
const VIDIOC_ENUM_FMT: c_ulong = (3 << 30)
    | ((std::mem::size_of::<V4l2Fmtdesc>() as c_ulong) << 16)
    | ((b'V' as c_ulong) << 8)
    | 2;

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    (a as u32) | ((b as u32) << 8) | ((c as u32) << 16) | ((d as u32) << 24)
}

/// `struct v4l2_fmtdesc` from `<linux/videodev2.h>`. 64 bytes; `mbus_code`
/// predates every kernel this client targets, so there is no short-struct
/// variant to worry about.
#[repr(C)]
#[derive(Clone, Copy)]
struct V4l2Fmtdesc {
    index: u32,
    type_: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

/// How many `/dev/videoN` to look at. Ten covers every board this has run on
/// with room to spare; the numbering is not stable across boots, so the answer
/// is found by enumeration rather than assumed.
const MAX_DEVICE_NO: u32 = 10;

fn open_video(path: &Path) -> Option<OwnedFd> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .ok()
        .map(OwnedFd::from)
}

/// Every pixel format the device's OUTPUT mplane queue accepts.
///
/// Empty for a node with no such queue, which is also how a non-mplane device
/// (the RGA, a UVC camera) falls out without a separate check.
fn output_formats(fd: &OwnedFd) -> Vec<u32> {
    let mut out = Vec::new();
    for index in 0..32u32 {
        let mut desc = V4l2Fmtdesc {
            index,
            type_: V4L2_BUF_TYPE_VIDEO_OUTPUT_MPLANE,
            flags: 0,
            description: [0; 32],
            pixelformat: 0,
            mbus_code: 0,
            reserved: [0; 3],
        };
        // SAFETY: `fd` is an open file descriptor for this call's duration and
        // `desc` is a correctly-sized, fully-initialised `v4l2_fmtdesc` that
        // the kernel only writes within.
        let ret = unsafe { libc::ioctl(fd.as_raw_fd(), VIDIOC_ENUM_FMT, &mut desc) };
        if ret < 0 {
            // EINVAL ends the enumeration, which is the normal exit. Anything
            // else (ENOTTY on a node with no OUTPUT queue) means the same thing
            // to us: nothing more to learn here.
            break;
        }
        out.push(desc.pixelformat);
    }
    out
}

/// Does `path` accept `S264` on its OUTPUT queue?
///
/// This is the independent ground truth, and it is deliberately narrow: it
/// proves the *driver* advertises stateless H.264, not that a decode will
/// succeed. `probe.rs` runs a real decode for that, and the two disagreeing is
/// informative rather than a bug in either.
pub fn enumerates_h264_slice(path: &Path) -> bool {
    match open_video(path) {
        Some(fd) => output_formats(&fd).contains(&V4L2_PIX_FMT_H264_SLICE),
        None => false,
    }
}

/// The first `/dev/videoN` that accepts `S264`, i.e. the device we want.
pub fn find_h264_decoder() -> Option<PathBuf> {
    (0..MAX_DEVICE_NO)
        .map(|n| PathBuf::from(format!("/dev/video{n}")))
        .find(|p| enumerates_h264_slice(p))
}

/// Serialises tests that drive the real decoder.
///
/// One stateless decoder, one request queue: two `H264Decoder`s streaming on the
/// same node at once is not a configuration any of this is designed for, and
/// `cargo test`'s default parallelism will happily try it -- this crate's own
/// suite has the probe's two tests plus six oracles, all of which open one.
/// Mirrors `ffmpeg_log::QuietLogGuard`'s reasoning: a process-wide resource
/// needs a process-wide lock, and recovering a poisoned guard with `into_inner`
/// still serialises correctly (poison here only means "an earlier test
/// panicked", which is information the test runner already has).
#[cfg(any(test, feature = "test-support"))]
pub fn exclusive_device_access() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ioctl encoding is computed rather than taken from a header, so it
    /// is worth pinning against the value the C preprocessor produces.
    /// `_IOWR('V', 2, struct v4l2_fmtdesc)` with a 64-byte struct is
    /// 0xc0405602.
    #[test]
    fn enum_fmt_ioctl_matches_the_kernel_header() {
        assert_eq!(std::mem::size_of::<V4l2Fmtdesc>(), 64);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
    }

    #[test]
    fn h264_slice_fourcc_is_s264() {
        assert_eq!(
            V4L2_PIX_FMT_H264_SLICE,
            u32::from_le_bytes([b'S', b'2', b'6', b'4'])
        );
    }

    /// A path that cannot be opened is not a decoder, and asking must not
    /// panic -- this runs on CI machines with no `/dev/video*` at all.
    #[test]
    fn a_missing_device_is_simply_not_a_decoder() {
        assert!(!enumerates_h264_slice(Path::new(
            "/dev/ghostframe-no-such-video-device"
        )));
    }
}
