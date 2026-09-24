//! Oracle: VA-API decode == software decode, byte for byte.
//!
//! Requires VA-API hardware, so these tests run in CI (picked up by `cargo
//! test --workspace --lib`, `.github/workflows/ci.yml:71`) but self-skip
//! there: the GPU-less CI runner has no `vainfo`, so every test below gates
//! on [`crate::probe::vainfo_reports_h264_vld`], runs on `Some(true)`, and
//! skips on `Some(false)`/`None` -- see the comment on each test for why the
//! gate is that function and not `vaapi_h264_decode_available()`.
//! No `#[ignore]` and no CI exclusion by name: this module runs everywhere
//! it can, and is silent only where it genuinely cannot establish ground
//! truth.
//!
//! A `#[cfg(test)]` module in `src/`, not `tests/*.rs`: these oracles test
//! this crate's own behaviour, and a unit-test module can see `#[cfg(test)]`
//! items directly (`testclip`, `probe::vainfo_reports_h264_vld`) with no
//! `test-support` feature and no self-referencing dev-dependency. Putting
//! this in `tests/*.rs` -- a separate crate unit that never sees
//! `#[cfg(test)]` -- was the earlier shape, and it forced `test-support` to
//! be a default feature just so `cargo test` would build it, which shipped
//! the panicking `testclip` encoder in every release build of every
//! downstream crate that forgot `default-features = false`. See
//! `Cargo.toml`: `test-support` is off by default again, and only crates
//! that need `gradient_clip`/`vainfo_reports_h264_vld` from their OWN tests
//! (client-gpu, ghostframe-e2e) opt in explicitly.

use crate::decoder::H264Decoder;
use crate::testclip::gradient_clip;
use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as ffi;

/// Resolution the exactness oracle (`hardware_decode_matches_software_
/// decode_exactly`) runs at. Arbitrary but fixed, so the mutation check
/// recorded on that test stays reproducible.
const W: u32 = 640;
const H: u32 = 480;

/// `DRM_FORMAT_MOD_LINEAR`.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// `DRM_FORMAT_MOD_INVALID`. On this GPU generation (GFX8) it is the only
/// value `vaExportSurfaceHandle` can return, for a linear surface and a
/// tiled one alike -- spec §7.1. §7.2 measured this specific GPU's surfaces
/// as linear at that layout despite the field saying nothing, which is why
/// `assert_dmabuf_is_linear_at` treats INVALID as an assertable case rather
/// than an unknown one below.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// `Some(true)`/`None` gate shared by every oracle in this module: skip
/// unless independent ground truth (`vainfo`, NOT this crate's own probe)
/// says the driver can decode H.264.
///
/// Gating on `vaapi_h264_decode_available()` would be circular: since that
/// probe decodes a real frame through `H264Decoder`, a regression in the
/// decoder would make the probe return `false`, which would make every
/// oracle skip and report green exactly when it should fail. Task 3's
/// review rejected that circularity once already; it must not come back at
/// every oracle.
///
/// Passes `crate::probe::RENDER_NODE` explicitly -- every test in this
/// module opens the decoder through `H264Decoder::new()`, which opens
/// exactly that node, so ground truth must be established against the
/// device the test actually exercises. See `vainfo_reports_h264_vld`'s doc
/// for why hardcoding it inside that function was itself a silent-skip bug.
fn skip_without_independently_verified_vaapi() -> bool {
    match crate::probe::vainfo_reports_h264_vld(crate::probe::RENDER_NODE) {
        Some(true) => false,
        Some(false) => {
            eprintln!("driver reports no H.264 VLD entrypoint; skipping");
            true
        }
        None => {
            eprintln!("vainfo unavailable; cannot establish ground truth, skipping");
            true
        }
    }
}

/// Decode with libavcodec's software H.264 decoder; return NV12 planes per
/// frame as (luma, chroma), tightly packed at `w` and `w` bytes per row.
fn software_decode_nv12(clip: &[Vec<u8>], w: u32, h: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
    ffmpeg::init().expect("ffmpeg init");
    let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264).expect("no h264 decoder");
    let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut dec = ctx.decoder().video().expect("video decoder");

    let mut out = Vec::new();
    // Matches `dec: &mut ffmpeg::decoder::Video` explicitly (not
    // `.is_ok()`), same as `testclip::gradient_clip`'s own `drain` closure
    // on the encode side: EAGAIN and EOF are the two expected reasons this
    // stops yielding frames. Collapsing every other error into "no more
    // frames" would silently truncate `out` on a genuine decode failure,
    // surfacing later as a misleading "frame counts differ" instead of the
    // actual cause.
    let take = |dec: &mut ffmpeg::decoder::Video, out: &mut Vec<(Vec<u8>, Vec<u8>)>| loop {
        let mut frame = ffmpeg::frame::Video::empty();
        match dec.receive_frame(&mut frame) {
            Ok(()) => {
                let y_stride = frame.stride(0);
                let mut luma = Vec::with_capacity((w * h) as usize);
                for row in 0..h as usize {
                    luma.extend_from_slice(
                        &frame.data(0)[row * y_stride..row * y_stride + w as usize],
                    );
                }
                // YUV420P -> NV12: interleave U and V. Exact, not a conversion.
                let u_stride = frame.stride(1);
                let v_stride = frame.stride(2);
                let chroma_w = w.div_ceil(2) as usize;
                let chroma_h = h.div_ceil(2) as usize;
                let mut chroma = Vec::with_capacity(chroma_w * chroma_h * 2);
                for row in 0..chroma_h {
                    let u = &frame.data(1)[row * u_stride..row * u_stride + chroma_w];
                    let v = &frame.data(2)[row * v_stride..row * v_stride + chroma_w];
                    for i in 0..chroma_w {
                        chroma.push(u[i]);
                        chroma.push(v[i]);
                    }
                }
                out.push((luma, chroma));
            }
            Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
            Err(ffmpeg::Error::Eof) => break,
            Err(e) => panic!("software h264 decode receive_frame failed: {e}"),
        }
    };

    for au in clip {
        let pkt = ffmpeg::Packet::copy(au);
        dec.send_packet(&pkt).expect("send_packet");
        take(&mut dec, &mut out);
    }
    dec.send_eof().expect("send_eof");
    take(&mut dec, &mut out);
    out
}

/// Download a VA-API surface to system memory as NV12, tightly packed at
/// `w`/`h` (the frame's own display dimensions, not necessarily this
/// module's `W`/`H` consts -- the linearity check runs this at 1080p too).
fn hw_frame_to_nv12(frame: &crate::decoder::HwFrame, w: u32, h: u32) -> (Vec<u8>, Vec<u8>) {
    // SAFETY: `frame` holds a live VAAPI AVFrame; `sw` is freed before return.
    unsafe {
        let mut sw = ffi::av_frame_alloc();
        assert!(!sw.is_null(), "av_frame_alloc");
        (*sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
        let ret = ffi::av_hwframe_transfer_data(sw, frame.as_ptr(), 0);
        assert!(ret >= 0, "av_hwframe_transfer_data = {ret}");

        let y_stride = (*sw).linesize[0] as usize;
        let uv_stride = (*sw).linesize[1] as usize;
        let mut luma = Vec::with_capacity((w * h) as usize);
        for row in 0..h as usize {
            let p = (*sw).data[0].add(row * y_stride);
            luma.extend_from_slice(std::slice::from_raw_parts(p, w as usize));
        }
        let chroma_h = h.div_ceil(2) as usize;
        let mut chroma = Vec::with_capacity((w as usize) * chroma_h);
        for row in 0..chroma_h {
            let p = (*sw).data[1].add(row * uv_stride);
            chroma.extend_from_slice(std::slice::from_raw_parts(p, w as usize));
        }
        // Named `mut` binding, not a throwaway `&mut { sw }` temporary --
        // `decoder.rs`'s `drain` explicitly rejects that idiom (it hides
        // ffmpeg's null-out from anything that touches the variable
        // afterwards) three commits before this file existed; it should not
        // reappear here just because this is test code.
        ffi::av_frame_free(&mut sw);
        (luma, chroma)
    }
}

/// **Mutation check (recorded, not just run):** temporarily added
/// `luma[0] = luma[0].wrapping_add(1);` right before this function's `return`
/// -- corrupting one byte of the hardware side of the comparison below --
/// and re-ran `hardware_decode_matches_software_decode_exactly`. It FAILED,
/// naming `frame 0: hardware and software decode disagree on 1 luma and 0
/// chroma bytes`, exactly as expected. Then reverted. This is what
/// establishes the oracle actually exercises the comparison it claims to,
/// rather than passing by construction.
#[test]
fn hardware_decode_matches_software_decode_exactly() {
    if skip_without_independently_verified_vaapi() {
        return;
    }

    let clip = gradient_clip(W, H, 8);
    let sw = software_decode_nv12(&clip, W, H);

    let mut dec = H264Decoder::new().expect("open hw decoder");
    let mut hw = Vec::new();
    for au in &clip {
        for frame in dec.decode(au).expect("decode") {
            hw.push(hw_frame_to_nv12(&frame, W, H));
        }
    }
    for frame in dec.finish().expect("finish") {
        hw.push(hw_frame_to_nv12(&frame, W, H));
    }

    assert_eq!(hw.len(), sw.len(), "frame counts differ");
    assert!(!hw.is_empty(), "nothing decoded");

    for (i, ((hw_y, hw_uv), (sw_y, sw_uv))) in hw.iter().zip(sw.iter()).enumerate() {
        // The exactness gate below rests on a zip, which silently truncates
        // to the shorter side. Both vectors are `W*H` (luma) / `W*H/2`
        // (chroma) by construction of `hw_frame_to_nv12`/
        // `software_decode_nv12` above, so this cannot fire today -- but the
        // gate should rest on an assertion, not on that construction staying
        // true forever.
        assert_eq!(
            hw_y.len(),
            sw_y.len(),
            "frame {i}: luma plane length mismatch"
        );
        assert_eq!(
            hw_uv.len(),
            sw_uv.len(),
            "frame {i}: chroma plane length mismatch"
        );
        let y_diff = hw_y.iter().zip(sw_y).filter(|(a, b)| a != b).count();
        let uv_diff = hw_uv.iter().zip(sw_uv).filter(|(a, b)| a != b).count();
        assert_eq!(
            (y_diff, uv_diff),
            (0, 0),
            "frame {i}: hardware and software decode disagree on {y_diff} luma and \
             {uv_diff} chroma bytes. H.264's inverse transform is specified exactly, \
             so conforming decoders cannot differ -- this is a real bug in how the \
             hardware decoder is driven, not codec noise. Do NOT add a tolerance."
        );
    }
}

/// Is the exported dmabuf laid out exactly as its descriptor claims, at
/// resolution `w`x`h`?
///
/// The modifier field cannot answer this directly on GFX8 (spec §7.1), so
/// this compares the bytes directly: `av_hwframe_transfer_data` is
/// authoritative, and an mmap of the dmabuf at the descriptor's offsets and
/// pitches must match it if -- and only if -- the surface is linear at that
/// layout.
///
/// **This asserts, it does not just print, on `LINEAR` and the
/// GFX8-structural `INVALID`** (spec §7.2 measured both this GPU's decode
/// surfaces as linear despite `INVALID` saying nothing on its own): a
/// nonzero count there means the property §7.2 established no longer holds,
/// and `import_nv12`'s zero-copy path (Task 6) needs re-evaluating before
/// it can be trusted again. A real tiled modifier is a different, legitimate
/// outcome on other hardware this test also runs on -- comparing a tiled
/// buffer against a linear read is *expected* to differ and says nothing
/// about a bug, so that case logs the counts and skips instead of failing.
///
/// **On visibility:** the `[m3]` diagnostic line goes through `eprintln!`,
/// which libtest captures and discards on a passing test -- invisible under
/// a bare `cargo test`. That used to be this test's only failure channel
/// (the earlier version asserted nothing), which meant the
/// architecture-deciding number could regress to a genuine mismatch and the
/// test would still read green. Now the number is behind `assert_eq!`: on
/// the pass path visibility is exactly as good as every other passing
/// test's diagnostics in this crate (none), but on the regression this
/// exists to catch, libtest unconditionally dumps captured output --
/// `[m3]` line included -- alongside the failure, with no `--nocapture`
/// required. That is the case that has to be visible, and it now is.
///
/// Takes `w`/`h` rather than hardcoding this module's `W`/`H`: radeonsi
/// chooses tiling per surface from dimensions and alignment, so "linear at
/// 640x480" does not establish "linear at 1920x1080" -- and 1080p, not
/// 640x480, is the resolution production actually runs at. Both call sites
/// below (`..._640x480`, `..._1080p`) log their own `[m3]` line so each
/// resolution's result is visible independently; neither is tuned to agree
/// with the other.
///
/// No explicit `DMA_BUF_IOCTL_SYNC` around the mmap read below, unlike every
/// other CPU mmap of a dmabuf in this repo (`export.rs`,
/// `ghostframe-xdaemon/src/drm_capture.rs`). Believed unnecessary here:
/// `av_hwframe_transfer_data` above already ran a full GPU readback through
/// ffmpeg/libva before this function ever calls `mmap`, which is itself a
/// synchronization point, and this mmap is read-only, so there is no write
/// this process could race against its own read. If that belief is wrong,
/// the failure direction is a false *mismatch* (reading stale/torn bytes
/// through the mmap before the surface settles), not a false *match* -- it
/// cannot be what produced the 0-byte-differ results below.
fn assert_dmabuf_is_linear_at(w: u32, h: u32) {
    if skip_without_independently_verified_vaapi() {
        return;
    }

    let clip = gradient_clip(w, h, 3);
    let mut dec = H264Decoder::new().expect("open hw decoder");
    let mut frames = Vec::new();
    for au in &clip {
        frames.extend(dec.decode(au).expect("decode"));
    }
    frames.extend(dec.finish().expect("finish"));
    let frame = frames.first().expect("no frame decoded");

    // Authoritative pixels.
    let (want_luma, want_chroma) = hw_frame_to_nv12(frame, w, h);

    // The same surface, seen as raw memory.
    let mapped = frame.map_dmabuf().expect("map to dmabuf");
    let p = mapped.planes();
    let chroma_h = p.chroma_height() as u64;
    let len = (p.chroma.offset + p.chroma.pitch * chroma_h) as usize;

    // SAFETY: `p.fd` is a live dmabuf owned by `mapped`, and `len` is within
    // the object size the descriptor reports. Read-only, shared.
    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            libc::PROT_READ,
            libc::MAP_SHARED,
            p.fd,
            0,
        )
    };
    assert!(
        ptr != libc::MAP_FAILED,
        "mmap of the decoder's dmabuf failed: {}. Without a CPU mapping this \
         question cannot be settled from this test.",
        std::io::Error::last_os_error()
    );
    // SAFETY: `ptr` is a valid mapping of `len` bytes, live until munmap below.
    let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, len) };

    let mut luma_diff = 0usize;
    for y in 0..h as usize {
        let off = p.luma.offset as usize + y * p.luma.pitch as usize;
        let row = &bytes[off..off + w as usize];
        luma_diff += row
            .iter()
            .zip(&want_luma[y * w as usize..(y + 1) * w as usize])
            .filter(|(a, b)| a != b)
            .count();
    }

    let mut chroma_diff = 0usize;
    for y in 0..p.chroma_height() as usize {
        let off = p.chroma.offset as usize + y * p.chroma.pitch as usize;
        let row = &bytes[off..off + w as usize];
        chroma_diff += row
            .iter()
            .zip(&want_chroma[y * w as usize..(y + 1) * w as usize])
            .filter(|(a, b)| a != b)
            .count();
    }

    // SAFETY: `ptr`/`len` are exactly what mmap returned and nothing else
    // holds the mapping.
    unsafe { libc::munmap(ptr, len) };

    let total = want_luma.len() + want_chroma.len();
    let diff = luma_diff + chroma_diff;
    let modifier = p.modifier;

    if modifier == DRM_FORMAT_MOD_LINEAR || modifier == DRM_FORMAT_MOD_INVALID {
        assert_eq!(
            diff, 0,
            "[m3] dmabuf-vs-download {w}x{h}: {diff} of {total} bytes differ (luma \
             {luma_diff}, chroma {chroma_diff}) at modifier=0x{modifier:016x}. Spec §7.2 \
             established this GPU generation's decode surfaces are linear at the \
             descriptor's layout (LINEAR and the GFX8-structural INVALID both read as \
             linear here) -- a nonzero count means that has changed, and Task 6's \
             zero-copy import path needs re-evaluating before it can be trusted."
        );
        eprintln!(
            "[m3] dmabuf-vs-download {w}x{h}: {diff} of {total} bytes differ (luma \
             {luma_diff}, chroma {chroma_diff}) at modifier=0x{modifier:016x}"
        );
    } else {
        // A real tiled layout, correctly declared. Comparing a tiled buffer
        // against a linear read is *expected* to differ and says nothing
        // about a bug on this hardware -- skip rather than fail.
        eprintln!(
            "[m3] dmabuf-vs-download {w}x{h}: modifier=0x{modifier:016x} is a real, \
             declared tiled layout (neither LINEAR nor GFX8-structural INVALID); \
             {diff} of {total} bytes differ (luma {luma_diff}, chroma {chroma_diff}), \
             which is expected for a tiled surface read as linear and is not a failure. \
             Skipping the assertion."
        );
    }
}

#[test]
fn the_exported_dmabuf_is_linear_at_the_descriptors_layout_640x480() {
    assert_dmabuf_is_linear_at(W, H);
}

/// Production resolution. See [`assert_dmabuf_is_linear_at`]'s doc for why
/// the 640x480 result alone does not establish this one.
#[test]
fn the_exported_dmabuf_is_linear_at_the_descriptors_layout_1080p() {
    assert_dmabuf_is_linear_at(1920, 1080);
}
