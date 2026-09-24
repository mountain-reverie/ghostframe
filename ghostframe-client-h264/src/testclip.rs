//! Test-only: synthesize a known H.264 clip with libx264.
//!
//! Not `#[cfg(test)]` because the oracle in `ghostframe-e2e` and the
//! `client-gpu` tests need it too -- both live in other crates, where this
//! crate's own `#[cfg(test)]` is never active. The `test-support` cargo
//! feature (see `Cargo.toml`) is the mechanism that reaches across that
//! boundary while keeping this module, with its nine panicking paths, out
//! of a release build of this production crate by default.
//!
//! In-band SPS/PPS is load-bearing here: no `AV_CODEC_FLAG_GLOBAL_HEADER` is
//! set below, so every access unit this produces carries its own parameter
//! sets. That is *why* `H264Decoder`'s tests work by feeding it access units
//! one at a time with nothing extracted out-of-band first -- an encoder
//! configured for global headers would produce a stream this decoder, as
//! written, could not parse standalone.

use ffmpeg_next as ffmpeg;

/// Encode `n` frames of a moving gradient at `w`x`h`; return Annex-B access
/// units, one per frame.
///
/// The gradient matters: a flat colour compresses to almost nothing and
/// would exercise none of the decoder's transform paths. Both luma AND
/// chroma vary along both `x` and `y` -- not decoration: it is what lets
/// `ghostframe-client-h264/src/oracle_tests.rs`'s linearity check actually
/// detect tiling. A pattern invariant down one axis of a plane cannot tell
/// a correctly-laid-out plane from one whose rows along that axis have been
/// permuted, and would report "0 differ" over a genuinely tiled surface.
/// At resolutions whose chroma plane has more than 256 rows (1920x1080's
/// has 540), the `y * 3 % 256` term wraps: source rows `y` and `y + 256`
/// are byte-identical *before* encoding. Row-distinctness in the linearity
/// check at that size survives only because H.264's lossy quantization
/// noise perturbs the two differently on the way through the encoder and
/// back, not because the source pattern itself stays distinct that far
/// down the plane.
///
/// # Panics
/// On any ffmpeg failure: libx264 missing from the build, the encoder
/// refusing this width/height/format, or a hard encode error. This is test
/// support, not production code -- a clip that fails to synthesize should
/// fail the test loudly, not be worked around.
pub fn gradient_clip(w: u32, h: u32, n: usize) -> Vec<Vec<u8>> {
    ffmpeg::init().expect("ffmpeg init");
    // Quiets libx264's own per-frame stats (`[libx264 @ ...] frame I: ...`),
    // which it `av_log`s at INFO and which would otherwise flood every
    // `cargo test` in the workspace that touches this function. Same guard
    // `probe.rs` built for the same reason, against the same noise source.
    let _quiet = crate::probe::QuietLogGuard::new();
    // `find_by_name`, not `find(Id::H264)`: the latter returns whichever
    // H.264 encoder registers first, which can be `h264_vaapi` or
    // `h264_nvenc`. Those then fail on a YUV420P software frame with no
    // hardware frames context, while the `.expect` below claims libx264 is
    // missing. Same house rule as
    // `ghostframe-lib/src/encoder/h264_vaapi.rs:159`.
    let codec = ffmpeg::encoder::find_by_name("libx264").expect("libx264 not available");
    let ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    let mut enc = ctx.encoder().video().expect("video encoder");
    enc.set_width(w);
    enc.set_height(h);
    enc.set_format(ffmpeg::format::Pixel::YUV420P);
    enc.set_time_base(ffmpeg::Rational::new(1, 60));
    let mut enc = enc.open_as(codec).expect("open libx264");

    let mut out = Vec::new();
    let drain = |enc: &mut ffmpeg::encoder::video::Encoder, out: &mut Vec<Vec<u8>>| {
        let mut pkt = ffmpeg::Packet::empty();
        loop {
            match enc.receive_packet(&mut pkt) {
                Ok(()) => out.push(pkt.data().expect("packet data").to_vec()),
                // EAGAIN ("send more input before another packet is ready")
                // and EOF are the two expected reasons this stops yielding
                // packets. Matching only on `.is_ok()` would treat a genuine
                // encode failure the same way -- silently truncating the
                // clip mid-test instead of failing it.
                Err(ffmpeg::Error::Other { errno }) if errno == libc::EAGAIN => break,
                Err(ffmpeg::Error::Eof) => break,
                Err(e) => panic!("libx264 receive_packet failed: {e}"),
            }
        }
    };

    for i in 0..n {
        let mut f = ffmpeg::frame::Video::new(ffmpeg::format::Pixel::YUV420P, w, h);
        let y_stride = f.stride(0);
        for y in 0..h as usize {
            let row = &mut f.data_mut(0)[y * y_stride..y * y_stride + w as usize];
            for (x, px) in row.iter_mut().enumerate() {
                *px = ((x + y + i * 16) % 256) as u8;
            }
        }
        for plane in 1..3 {
            let stride = f.stride(plane);
            for y in 0..(h / 2) as usize {
                let row = &mut f.data_mut(plane)[y * stride..y * stride + (w / 2) as usize];
                for (x, px) in row.iter_mut().enumerate() {
                    *px = ((x * 2 + y * 3 + plane * 40 + i * 8) % 256) as u8;
                }
            }
        }
        f.set_pts(Some(i as i64));
        enc.send_frame(&f).expect("send_frame");
        drain(&mut enc, &mut out);
    }
    enc.send_eof().expect("send_eof");
    drain(&mut enc, &mut out);
    out
}
