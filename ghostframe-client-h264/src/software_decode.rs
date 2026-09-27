//! libavcodec's software H.264 decoder, as the golden both oracles compare to.
//!
//! Shared by `oracle_tests` (VA-API) and `oracle_tests_v4l2`. It lives outside
//! both because "what should the pixels be?" is not a backend question: the
//! whole point of an exactness oracle is that the answer is computed by
//! something with no hardware in it.
//!
//! Available whenever ffmpeg is linked -- the `vaapi` backend, or any build with
//! `test-support`. That is why a `v4l2` build's `test-support` pulls ffmpeg in
//! (see `Cargo.toml`): the software decoder is the golden, not a leak of the
//! other backend.

use ffmpeg_next as ffmpeg;

/// Decode with libavcodec's software H.264 decoder; return NV12 planes per
/// frame as (luma, chroma), tightly packed at `w` and `w` bytes per row.
pub fn software_decode_nv12(clip: &[Vec<u8>], w: u32, h: u32) -> Vec<(Vec<u8>, Vec<u8>)> {
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
