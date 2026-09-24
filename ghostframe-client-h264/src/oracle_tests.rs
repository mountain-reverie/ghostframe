//! Oracle: VA-API decode == software decode, byte for byte.
//!
//! Requires VA-API hardware, so these tests run in CI (picked up by `cargo
//! test --workspace --lib`, `.github/workflows/ci.yml:71`) but self-skip
//! there: the GPU-less CI runner has no `vainfo`, and every test below gates
//! on [`crate::probe::vainfo_reports_h264_vld`] returning `None` (not on
//! `vaapi_h264_decode_available()` -- see the comment on each test for why).
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
fn skip_without_independently_verified_vaapi() -> bool {
    match crate::probe::vainfo_reports_h264_vld() {
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
    let take = |dec: &mut ffmpeg::decoder::Video, out: &mut Vec<(Vec<u8>, Vec<u8>)>| {
        let mut frame = ffmpeg::frame::Video::empty();
        while dec.receive_frame(&mut frame).is_ok() {
            let y_stride = frame.stride(0);
            let mut luma = Vec::with_capacity((w * h) as usize);
            for row in 0..h as usize {
                luma.extend_from_slice(&frame.data(0)[row * y_stride..row * y_stride + w as usize]);
            }
            // YUV420P -> NV12: interleave U and V. Exact, not a conversion.
            let u_stride = frame.stride(1);
            let v_stride = frame.stride(2);
            let mut chroma = Vec::with_capacity((w * h / 2) as usize);
            for row in 0..(h / 2) as usize {
                let u = &frame.data(1)[row * u_stride..row * u_stride + (w / 2) as usize];
                let v = &frame.data(2)[row * v_stride..row * v_stride + (w / 2) as usize];
                for i in 0..(w / 2) as usize {
                    chroma.push(u[i]);
                    chroma.push(v[i]);
                }
            }
            out.push((luma, chroma));
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
        let sw = ffi::av_frame_alloc();
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
        let mut chroma = Vec::with_capacity((w * h / 2) as usize);
        for row in 0..(h / 2) as usize {
            let p = (*sw).data[1].add(row * uv_stride);
            chroma.extend_from_slice(std::slice::from_raw_parts(p, w as usize));
        }
        ffi::av_frame_free(&mut { sw });
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
/// The modifier field cannot answer this on GFX8 (spec §7.1), so compare the
/// bytes directly: `av_hwframe_transfer_data` is authoritative, and an mmap of
/// the dmabuf at the descriptor's offsets and pitches must match it if -- and
/// only if -- the surface is linear at that layout.
///
/// A match means `import_nv12`'s LINEAR path is sound here despite the missing
/// metadata. A mismatch means tiled, and the CPU copy is confirmed on evidence
/// rather than on an absent field.
///
/// Takes `w`/`h` rather than hardcoding this module's `W`/`H`: radeonsi
/// chooses tiling per surface from dimensions and alignment, so "linear at
/// 640x480" does not establish "linear at 1920x1080" -- and 1080p, not
/// 640x480, is the resolution production actually runs at. Both call sites
/// below (`..._at_640x480`, `..._at_1080p`) log their own `[m3]` line so
/// each resolution's result is visible independently; neither is tuned to
/// agree with the other, and a difference between them would itself be the
/// finding (linear at one size, tiled at another) that Task 6's runtime
/// pitch check exists to catch.
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
    let len = (p.chroma.offset + p.chroma.pitch * (h as u64 / 2)) as usize;

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
        let row = &bytes[y * p.luma.pitch as usize..y * p.luma.pitch as usize + w as usize];
        luma_diff += row
            .iter()
            .zip(&want_luma[y * w as usize..(y + 1) * w as usize])
            .filter(|(a, b)| a != b)
            .count();
    }

    let mut chroma_diff = 0usize;
    for y in 0..(h / 2) as usize {
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

    let total = (w * h) as usize + (w * h / 2) as usize;
    eprintln!(
        "[m3] dmabuf-vs-download {w}x{h}: {} of {} bytes differ (luma {}, chroma {})",
        luma_diff + chroma_diff,
        total,
        luma_diff,
        chroma_diff
    );

    // Deliberately NOT an assertion of linearity: both outcomes are valid
    // findings, and which one holds decides whether `import_nv12` can take
    // the LINEAR path on this hardware. What IS asserted is that the
    // comparison actually ran over real data -- a decode that silently
    // produced nothing would make every loop above a no-op and report
    // "0 differ" indistinguishable from a genuine linear match.
    assert!(!want_luma.is_empty(), "nothing was compared");
}

#[test]
fn the_exported_dmabuf_is_linear_at_the_descriptors_layout_640x480() {
    assert_dmabuf_is_linear_at(640, 480);
}

/// Production resolution. See [`assert_dmabuf_is_linear_at`]'s doc for why
/// the 640x480 result alone does not establish this one.
#[test]
fn the_exported_dmabuf_is_linear_at_the_descriptors_layout_1080p() {
    assert_dmabuf_is_linear_at(1920, 1080);
}
