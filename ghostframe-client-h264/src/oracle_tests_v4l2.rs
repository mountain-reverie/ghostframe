//! Oracle: rkvdec decode == software decode, byte for byte.
//!
//! The `v4l2` backend's exactness oracle, and the only test that distinguishes
//! a correct chroma offset from a nearly-correct one. A wrong offset here does
//! not error or crash — it yields a plausible image with shifted colour, which
//! is why comparing against a software decode is the whole design and not a
//! nicety.
//!
//! Requires a V4L2 stateless decoder, so these self-skip everywhere else. The
//! gate is [`crate::v4l2_device::enumerates_h264_slice`] — raw `VIDIOC_ENUM_FMT`,
//! independent of `cros-codecs` — and **not** `h264_decode_available()`, which
//! decodes a frame to answer: gating on it would mean a decoder regression makes
//! every oracle here skip and report green exactly when it should fail. Same
//! circularity `oracle_tests` refuses for VA-API.
//!
//! The one other accepted skip is an unpatched `cros-codecs`, which cannot open
//! the right device (see [`crate::decoder`]). That is not swallowed either:
//! `probe_v4l2::tests::probe_agrees_with_the_driver` asserts it is the *only*
//! reason the probe may decline.

use crate::decoder::H264Decoder;
use crate::software_decode::software_decode_nv12;
use crate::testclip::gradient_clip;

/// Where the exactness comparison runs. 640x480 is the case where display and
/// coded height coincide, which is exactly why it is not the only case tested.
const W: u32 = 640;
const H: u32 = 480;

/// 1080 is not a multiple of 16, so rkvdec codes it at 1088 and the chroma
/// plane does *not* start at `stride * 1080`. A test only at [`W`]x[`H`] proves
/// nothing about that.
const TALL_W: u32 = 1920;
const TALL_H: u32 = 1080;

/// Frames per clip. Enough to cycle the driver's buffer pool several times,
/// which is what makes the index-vs-frame-object export bug visible: with three
/// buffers, reuse starts around frame 5 and a per-frame-object export table
/// reorders everything after it.
const FRAMES: usize = 12;

/// Run `f` against the real decoder, holding it exclusively, or skip.
///
/// One entry point rather than an `open()` helper, because the device lock has
/// to span the whole test: `cargo test`'s parallelism would otherwise have two
/// oracles streaming on one decoder. `None` means "skipped, and the reason is on
/// stderr" -- every caller returns early on it, and the reason is always either
/// no hardware or the unpatched-cros-codecs refusal that
/// `probe_v4l2::tests::probe_agrees_with_the_driver` pins down.
fn with_decoder<T>(f: impl FnOnce(&mut H264Decoder) -> T) -> Option<T> {
    let _device = crate::v4l2_device::exclusive_device_access();
    let node = crate::probe::default_device();
    if !crate::v4l2_device::enumerates_h264_slice(std::path::Path::new(&node)) {
        eprintln!("no /dev/video* node enumerates S264; skipping");
        return None;
    }
    match H264Decoder::with_device(&node) {
        Ok(mut d) => Some(f(&mut d)),
        Err(e) => {
            eprintln!("cannot open {node}: {e}; skipping");
            None
        }
    }
}

/// Decode a clip and hand every frame to `each`, in output order.
///
/// Frames are passed while still alive rather than collected and returned: an
/// `HwFrame` pins its V4L2 capture buffer, so collecting a whole clip's worth
/// would need more buffers than the driver has. Whatever `each` extracts is what
/// survives.
fn decode_each<T>(
    decoder: &mut H264Decoder,
    clip: &[Vec<u8>],
    mut each: impl FnMut(&crate::decoder::HwFrame) -> T,
) -> Vec<T> {
    let mut out = Vec::new();
    for au in clip {
        for frame in decoder.decode(au).expect("hardware decode") {
            out.push(each(&frame));
        }
    }
    for frame in decoder.finish().expect("drain decoder") {
        out.push(each(&frame));
    }
    out
}

/// Every frame of a clip as tightly packed NV12.
fn hardware_decode_nv12(decoder: &mut H264Decoder, clip: &[Vec<u8>]) -> Vec<(Vec<u8>, Vec<u8>)> {
    decode_each(decoder, clip, |frame| {
        frame
            .download_nv12()
            .expect("download decoded frame to NV12")
    })
}

/// Compare a hardware decode against a software one, plane by plane.
fn assert_matches_software(w: u32, h: u32) {
    let clip = gradient_clip(w, h, FRAMES);
    let Some(hw) = with_decoder(|d| hardware_decode_nv12(d, &clip)) else {
        return;
    };
    let sw = software_decode_nv12(&clip, w, h);
    assert!(!sw.is_empty(), "software decode produced no frames");
    assert_eq!(
        hw.len(),
        sw.len(),
        "hardware produced {} frames, software {} -- a count mismatch is usually \
         frames emitted in the wrong order, not frames lost; see ExportTable",
        hw.len(),
        sw.len()
    );
    for (i, ((hw_y, hw_uv), (sw_y, sw_uv))) in hw.iter().zip(sw.iter()).enumerate() {
        assert_eq!(
            hw_y, sw_y,
            "frame {i}: luma differs between hardware and software decode"
        );
        assert_eq!(
            hw_uv, sw_uv,
            "frame {i}: chroma differs between hardware and software decode -- \
             the chroma plane offset is the first thing to suspect"
        );
    }
}

/// The exactness oracle at a 16-aligned height.
///
/// **Mutation check (recorded, not just run):** changed
/// `V4l2Frame::chroma_offset` to `self.stride() * (self.coded_height() + 1)` --
/// one row of chroma offset, the nearly-correct case this whole oracle exists
/// for -- and re-ran the suite on rkvdec. Exactly the two exactness oracles
/// failed (this one and the 1080p one); the other four still passed, which is
/// the right outcome and worth recording:
///
/// - `the_chroma_offset_follows_the_drivers_coded_height` checks the offset is
///   *past* the display height, and 1089 rows still is. It catches the opposite
///   mistake -- deriving from the display height -- not this one.
/// - `a_one_row_chroma_shift_does_not_match_the_golden` reads one row past
///   whatever the offset says, so under the mutation it read two rows late and
///   still, correctly, did not match.
///
/// So the byte-for-byte comparison is the only thing standing between a
/// one-row error and a release. Reverted after measuring. The same
/// perturbation is reachable from outside the crate with `CHROMA_SHIFT_ROWS=1`
/// in `tools/hw-probe/v4l2-expbuf-rs`.
#[test]
fn hardware_decode_matches_software_decode_exactly() {
    assert_matches_software(W, H);
}

/// The same, where the coded height is not the display height.
///
/// This is the case that distinguishes "reads the driver's numbers" from
/// "assumes the display height": at 1080p rkvdec codes 1088 rows, so a layout
/// derived from 1080 puts chroma 8 rows early. 640x480 cannot catch that.
#[test]
fn hardware_decode_matches_software_decode_at_a_non_16_aligned_height() {
    assert_matches_software(TALL_W, TALL_H);
}

/// The chroma offset comes from the driver's coded height, and at 1080p that is
/// demonstrably not the display height.
///
/// A structural assertion rather than a pixel one, so a future edit that
/// switches to the display height fails here even on hardware whose coded and
/// display heights happen to agree.
#[test]
fn the_chroma_offset_follows_the_drivers_coded_height() {
    let clip = gradient_clip(TALL_W, TALL_H, 2);
    let Some(layouts) = with_decoder(|d| {
        decode_each(d, &clip, |frame| {
            *frame.map_dmabuf().expect("describe the dmabuf").planes()
        })
    }) else {
        return;
    };
    let planes = layouts.first().expect("no frame decoded at 1080p");

    assert_eq!(planes.width, TALL_W);
    assert_eq!(planes.height, TALL_H);
    assert_eq!(
        planes.luma.offset, 0,
        "luma must start at the buffer's base"
    );
    assert_eq!(
        planes.chroma.pitch, planes.luma.pitch,
        "NV12's chroma plane has the same row pitch as luma"
    );

    let rows = planes.chroma.offset / planes.luma.pitch;
    assert!(
        rows > u64::from(TALL_H),
        "chroma starts after {rows} luma rows, but the display height is {TALL_H}: \
         either this driver codes 1080p unpadded (surprising) or the offset was \
         derived from the display height instead of the coded one"
    );
    assert_eq!(
        planes.chroma.offset % planes.luma.pitch,
        0,
        "the chroma offset must be a whole number of rows"
    );
}

/// Reading one row late must NOT match the golden.
///
/// An executable mutation check, not a recorded one: it reads the same buffer at
/// a deliberately wrong chroma offset and asserts the bytes disagree with the
/// software decode. Without this, "the offset is right" rests on a comparison
/// that would also pass if the oracle were looking at the wrong thing entirely
/// -- a uniform frame, say, where every offset gives the same bytes.
#[test]
fn a_one_row_chroma_shift_does_not_match_the_golden() {
    let clip = gradient_clip(W, H, 2);
    // Read the shifted copy INSIDE the closure: the mapping borrows the frame,
    // which pins the capture buffer, and the bytes must be read before either is
    // released.
    // Three layers of Option, each meaning something different: no hardware,
    // no frame, no room past the chroma plane. Any of them is a skip.
    let Some(shifted_chroma) = with_decoder(|d| {
        decode_each(d, &clip, read_chroma_one_row_late)
            .into_iter()
            .next()
    })
    .flatten()
    .flatten() else {
        return;
    };
    let sw = software_decode_nv12(&clip, W, H);
    let (_, golden_chroma) = sw.first().expect("software decode produced no frames");
    assert_ne!(
        &shifted_chroma, golden_chroma,
        "chroma read one row late still matched the software golden -- this oracle \
         cannot tell a correct offset from a wrong one, so the clip is too uniform \
         to be testing anything"
    );
}

/// The chroma plane as it would look if the offset were one row too late.
///
/// `None` when the buffer has no room past the real chroma plane to read a
/// shifted copy, in which case the mutation cannot be tested on this hardware
/// and the caller says so rather than passing vacuously.
fn read_chroma_one_row_late(frame: &crate::decoder::HwFrame) -> Option<Vec<u8>> {
    let mapped = frame.map_dmabuf().expect("describe the dmabuf");
    let planes = mapped.planes();
    let pitch = planes.luma.pitch as usize;
    let chroma_rows = planes.chroma_height() as usize;
    let shifted = planes.chroma.offset as usize + pitch;
    let len = planes.size as usize;
    if shifted + pitch * chroma_rows > len {
        eprintln!(
            "no room past the chroma plane ({shifted} + {pitch}*{chroma_rows} > {len}); \
             the one-row mutation cannot be read on this buffer"
        );
        return None;
    }

    // SAFETY: `planes.fd` is a dmabuf owned by `mapped` for the rest of this
    // test; the mapping is read-only, sized to the length the driver reported,
    // and unmapped below.
    let base = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            planes.fd,
            0,
        )
    };
    assert_ne!(base, libc::MAP_FAILED, "mmap of the decoded dmabuf failed");
    let mut wrong = Vec::with_capacity(W as usize * chroma_rows);
    for row in 0..chroma_rows {
        // SAFETY: bounds checked above against `len`.
        let p = unsafe { (base as *const u8).add(shifted + row * pitch) };
        wrong.extend_from_slice(unsafe { std::slice::from_raw_parts(p, W as usize) });
    }
    // SAFETY: the pair returned by the `mmap` above.
    unsafe { libc::munmap(base, len) };
    Some(wrong)
}

/// The exported buffer is linear, and its planes fit inside it.
///
/// Linearity is not incidental: `import_gles.rs` will import with
/// `DRM_FORMAT_MOD_LINEAR`, and rkvdec producing a tiled buffer would make that
/// import silently wrong. The extent check mirrors what the VA-API path applies
/// to a DRM descriptor, here against numbers the driver chose.
#[test]
fn the_exported_dmabuf_is_linear_and_its_planes_fit() {
    let clip = gradient_clip(W, H, 2);
    let Some(layouts) = with_decoder(|d| {
        decode_each(d, &clip, |frame| {
            *frame.map_dmabuf().expect("describe the dmabuf").planes()
        })
    }) else {
        return;
    };
    let planes = layouts.first().expect("no frame decoded");

    assert_eq!(
        planes.modifier, 0,
        "DRM_FORMAT_MOD_LINEAR expected; a tiled modifier means the GLES import \
         path needs a detiling step it does not have"
    );
    assert_eq!(planes.fourcc_luma, crate::DRM_FORMAT_R8);
    assert_eq!(planes.fourcc_chroma, crate::DRM_FORMAT_GR88);
    assert!(planes.fd >= 0, "no dmabuf fd");
    let luma_end = planes.luma.offset + planes.luma.pitch * u64::from(planes.height);
    let chroma_end = planes.chroma.offset + planes.chroma.pitch * u64::from(planes.chroma_height());
    assert!(luma_end <= planes.size, "luma runs past the allocation");
    assert!(chroma_end <= planes.size, "chroma runs past the allocation");
    assert!(
        planes.luma.pitch >= u64::from(planes.width),
        "row pitch narrower than the frame"
    );
}

/// Every frame must come out exactly once and in order.
///
/// The regression test for the export table being keyed by V4L2 buffer index
/// rather than by frame object: keyed wrongly, each frame is still a real frame,
/// so only a whole-sequence comparison catches it. Distinct from the exactness
/// oracles above because it says what the failure *means* — duplicates and gaps
/// against the golden, not corruption.
#[test]
fn frames_come_out_once_each_and_in_order() {
    let clip = gradient_clip(W, H, FRAMES);
    let Some(hw) = with_decoder(|d| hardware_decode_nv12(d, &clip)) else {
        return;
    };
    let sw = software_decode_nv12(&clip, W, H);
    assert!(!hw.is_empty(), "hardware decode produced no frames");
    let position_in_golden = |plane: &Vec<u8>| sw.iter().position(|(y, _)| y == plane);
    let mapped: Vec<Option<usize>> = hw.iter().map(|(y, _)| position_in_golden(y)).collect();
    let expected: Vec<Option<usize>> = (0..hw.len()).map(Some).collect();
    assert_eq!(
        mapped, expected,
        "hardware frames map onto the golden sequence as {mapped:?} rather than in \
         order. Repeats and gaps here are the signature of a dmabuf exported per \
         frame object instead of per V4L2 buffer index -- every frame is genuine, \
         just the wrong one"
    );
}
