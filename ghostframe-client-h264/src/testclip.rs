//! Test-only: synthesize a known H.264 clip with libx264.
//!
//! Not `#[cfg(test)]` because the oracle in `ghostframe-e2e` and the
//! `client-gpu` tests need it too. Costs nothing in a release build beyond
//! the code size of one function.

use ffmpeg_next as ffmpeg;

/// Encode `n` frames of a moving gradient at `w`x`h`; return Annex-B access
/// units, one per frame.
///
/// The gradient matters: a flat colour compresses to almost nothing and
/// would exercise none of the decoder's transform paths.
pub fn gradient_clip(w: u32, h: u32, n: usize) -> Vec<Vec<u8>> {
    ffmpeg::init().expect("ffmpeg init");
    let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264).expect("libx264 not available");
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
        while enc.receive_packet(&mut pkt).is_ok() {
            out.push(pkt.data().expect("packet data").to_vec());
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
                    *px = ((x * 2 + plane * 40 + i * 8) % 256) as u8;
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
