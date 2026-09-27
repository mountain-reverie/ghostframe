//! Which `/dev/videoN` is the stateless H.264 decoder — asked with raw ioctls.
//!
//! Two callers, and the second is why this uses `libc::ioctl` directly instead
//! of the perfectly good `v4l2r` that `cros-codecs` already re-exports:
//!
//! 1. [`H264Decoder::with_device`](crate::decoder::H264Decoder::with_device)
//!    needs to know whether the device it was asked for is the one the decoder
//!    will actually get.
//! 2. `probe.rs` needs an **independent ground truth** for the oracles —
//!    "does this machine have a hardware H.264 decoder?" answered without
//!    going through the thing being validated. That is the same role
//!    `vainfo_reports_h264_vld` plays for the VA-API backend, and it only
//!    works if it shares no code with the decode path. Reaching the answer
//!    through `cros_codecs::v4l2r` would share the dependency that selects and
//!    opens the device, which is exactly the part that has been wrong.
//!
//! So: `VIDIOC_QUERYCAP` and `VIDIOC_ENUM_FMT`, and nothing else.
//!
//! ## Why [`cros_codecs_would_pick`] exists at all
//!
//! `cros-codecs 0.0.6` selects its device by scanning `/dev/video0..`, taking
//! the first node that has an OUTPUT mplane queue and a matching media device,
//! **with no check that it decodes anything**. On RK3399 that is `/dev/video0`
//! — the hantro *encoder* — and decode dies with `Unrecoverable decoding
//! error` even though rkvdec on `/dev/video3` handles the same stream fine.
//! `C2V4L2DecoderOptions::video_device_path` exists for this and is marked
//! `TODO: This is currently unused`.
//!
//! We carry a patch that honours `CROS_CODECS_V4L2_DEVICE`
//! (`tools/hw-probe/v4l2-expbuf-rs/cros-codecs-0.0.6.patch`, upstreamable).
//! This module lets the decoder detect, before it tries, whether that patch is
//! doing its job — and refuse cleanly if it is not, so the session falls back
//! to tile codecs instead of opening an encoder and failing per frame.

use std::ffi::c_ulong;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Path, PathBuf};

/// The environment variable the carried cros-codecs patch reads.
///
/// Deliberately *read* here and never written: `setenv(3)` races `getenv(3)`
/// in any other thread, and this crate is driven from a render thread inside a
/// multi-threaded process. Packaging sets it; see the crate docs.
pub const DEVICE_OVERRIDE_ENV: &str = "CROS_CODECS_V4L2_DEVICE";

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

/// How many `/dev/videoN` to look at. Matches `cros-codecs`' own
/// `MAX_DEVICE_NO`, because [`cros_codecs_would_pick`] has to see the same
/// nodes in the same order to predict the same answer.
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

/// The device `cros-codecs`' unpatched scan would settle on.
///
/// Replicates `enumerate_devices()`: first node that opens and has an OUTPUT
/// mplane queue. It skips the media-device lookup, which makes this the
/// *optimistic* prediction — if even this says the wrong device, the real scan
/// certainly does. Read-only; it opens nothing it does not close.
pub fn cros_codecs_would_pick() -> Option<PathBuf> {
    for n in 0..MAX_DEVICE_NO {
        let path = PathBuf::from(format!("/dev/video{n}"));
        let Some(fd) = open_video(&path) else {
            continue;
        };
        if !output_formats(&fd).is_empty() {
            return Some(path);
        }
    }
    None
}

/// Whether the override in the environment names `path`.
pub fn override_selects(path: &Path) -> bool {
    std::env::var_os(DEVICE_OVERRIDE_ENV).is_some_and(|v| Path::new(&v) == path)
}

/// The node `cros-codecs` will actually open: the override if one is set,
/// otherwise its own scan.
///
/// The override wins because that is what the carried patch does, and modelling
/// it that way is the only way to catch a **stale** override. `/dev/videoN`
/// numbering is not stable across boots here -- rkvdec and the hantro decoder
/// swapped places (video3 and video1) over a single reboot -- so an override
/// recorded in a service file or a shell profile quietly starts naming the wrong
/// driver, and "an override is set" is no evidence it is the right one.
///
/// Imperfect in one direction, and safely so: with the patch applied an
/// *unusable* override falls back to the scan, which this reports as a mismatch
/// rather than following. That yields a clean refusal and a message telling the
/// caller to fix the override -- better than guessing right by accident.
pub fn device_cros_codecs_will_open() -> Option<PathBuf> {
    match std::env::var_os(DEVICE_OVERRIDE_ENV) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => cros_codecs_would_pick(),
    }
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

    #[test]
    fn override_selects_compares_the_whole_path() {
        // Reading only: whatever the ambient value is, it is not this.
        assert!(!override_selects(Path::new(
            "/dev/video-that-nobody-would-set"
        )));
    }
}
