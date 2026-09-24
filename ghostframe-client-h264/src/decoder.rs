//! VA-API H.264 decode: access units in, hardware surfaces out.
//!
//! Mirrors ffmpeg's canonical `hw_decode.c`: set `hw_device_ctx`, override
//! `get_format` to pin AV_PIX_FMT_VAAPI, and let the decoder allocate its own
//! hardware frames context. The `get_format` override is what stops ffmpeg
//! silently falling back to software decode -- which would still *work*, and
//! would quietly cost a full-frame CPU download per frame.

use crate::H264Error;
use ffmpeg_sys_next as ffi;
use std::ffi::CString;
use std::ptr;

/// One decoded hardware frame. Owns its `AVFrame`.
pub struct HwFrame {
    pub(crate) frame: *mut ffi::AVFrame,
}

// SAFETY: `AVFrame` is refcounted and carries no thread affinity; this struct
// owns its pointer exclusively and frees it exactly once in `Drop`.
unsafe impl Send for HwFrame {}

impl HwFrame {
    pub fn width(&self) -> u32 {
        // SAFETY: `frame` is non-null and live for `self`'s lifetime.
        unsafe { (*self.frame).width as u32 }
    }

    pub fn height(&self) -> u32 {
        // SAFETY: as above.
        unsafe { (*self.frame).height as u32 }
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        // SAFETY: `frame` was allocated by `av_frame_alloc` and is dropped
        // exactly once here.
        unsafe { ffi::av_frame_free(&mut self.frame) };
    }
}

/// Pin AV_PIX_FMT_VAAPI out of the decoder's offered format list.
///
/// # Safety
/// Called by ffmpeg with a valid NUL-terminated (`AV_PIX_FMT_NONE`) format
/// array. Returning a format not in that list is undefined behaviour, so the
/// fallback returns `AV_PIX_FMT_NONE`, which ffmpeg treats as "cannot decode".
unsafe extern "C" fn get_vaapi_format(
    _ctx: *mut ffi::AVCodecContext,
    mut fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    while *fmts != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
        if *fmts == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
            return ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        }
        fmts = fmts.add(1);
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

pub struct H264Decoder {
    ctx: *mut ffi::AVCodecContext,
    hw_device: *mut ffi::AVBufferRef,
    packet: *mut ffi::AVPacket,
}

// SAFETY: every pointer here is owned exclusively by this struct, freed once
// in `Drop`, and never shared. ffmpeg codec contexts are not thread-safe for
// concurrent use, which `&mut self` on every method already enforces.
unsafe impl Send for H264Decoder {}

impl H264Decoder {
    pub fn new() -> Result<Self, H264Error> {
        Self::with_device(crate::probe::RENDER_NODE)
    }

    pub fn with_device(node: &str) -> Result<Self, H264Error> {
        let path = CString::new(node)
            .map_err(|_| H264Error::VaapiUnavailable(format!("bad device path {node:?}")))?;

        // SAFETY: all out-params are valid; every early return frees what it
        // has allocated so far, in reverse order of allocation.
        unsafe {
            let mut hw_device: *mut ffi::AVBufferRef = ptr::null_mut();
            let ret = ffi::av_hwdevice_ctx_create(
                &mut hw_device,
                ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                path.as_ptr(),
                ptr::null_mut(),
                0,
            );
            if ret < 0 {
                return Err(H264Error::VaapiUnavailable(format!(
                    "av_hwdevice_ctx_create({node}) = {ret}"
                )));
            }

            let codec = ffi::avcodec_find_decoder(ffi::AVCodecID::AV_CODEC_ID_H264);
            if codec.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("no H.264 decoder in ffmpeg".into()));
            }

            let ctx = ffi::avcodec_alloc_context3(codec);
            if ctx.is_null() {
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("avcodec_alloc_context3 failed".into()));
            }
            (*ctx).hw_device_ctx = ffi::av_buffer_ref(hw_device);
            (*ctx).get_format = Some(get_vaapi_format);

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                let mut c = ctx;
                ffi::avcodec_free_context(&mut c);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg(format!("avcodec_open2 = {ret}")));
            }

            let packet = ffi::av_packet_alloc();
            if packet.is_null() {
                let mut c = ctx;
                ffi::avcodec_free_context(&mut c);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("av_packet_alloc failed".into()));
            }

            Ok(H264Decoder {
                ctx,
                hw_device,
                packet,
            })
        }
    }

    /// Feed one access unit; return every frame it completed.
    ///
    /// An empty result is normal, not an error: the decoder emits nothing
    /// until it has a keyframe, and B-frame reordering delays output.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<HwFrame>, H264Error> {
        // SAFETY: `au` outlives the `send_packet` call, which copies what it
        // needs; `self.packet` is a live allocation reset after every use.
        unsafe {
            (*self.packet).data = au.as_ptr() as *mut u8;
            (*self.packet).size = au.len() as i32;
            let ret = ffi::avcodec_send_packet(self.ctx, self.packet);
            (*self.packet).data = ptr::null_mut();
            (*self.packet).size = 0;
            if ret < 0 && ret != ffi::AVERROR(libc::EAGAIN) {
                return Err(H264Error::Ffmpeg(format!("send_packet = {ret}")));
            }
        }
        self.drain()
    }

    /// Drain frames the decoder is still holding. Call at end of stream.
    pub fn flush(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        // SAFETY: a null packet is ffmpeg's documented end-of-stream signal.
        unsafe {
            let ret = ffi::avcodec_send_packet(self.ctx, ptr::null());
            if ret < 0 && ret != ffi::AVERROR_EOF {
                return Err(H264Error::Ffmpeg(format!("send_packet(NULL) = {ret}")));
            }
        }
        self.drain()
    }

    fn drain(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        let mut out = Vec::new();
        loop {
            // SAFETY: `av_frame_alloc` returns an owned frame or null; the
            // frame is either moved into `out` or freed before we return.
            let frame = unsafe { ffi::av_frame_alloc() };
            if frame.is_null() {
                return Err(H264Error::Ffmpeg("av_frame_alloc failed".into()));
            }
            // SAFETY: `self.ctx` is open; `frame` is a fresh allocation.
            let ret = unsafe { ffi::avcodec_receive_frame(self.ctx, frame) };
            if ret < 0 {
                // SAFETY: nothing took ownership of `frame`.
                unsafe { ffi::av_frame_free(&mut { frame }) };
                if ret == ffi::AVERROR(libc::EAGAIN) || ret == ffi::AVERROR_EOF {
                    return Ok(out);
                }
                return Err(H264Error::Ffmpeg(format!("receive_frame = {ret}")));
            }
            out.push(HwFrame { frame });
        }
    }
}

impl Drop for H264Decoder {
    fn drop(&mut self) {
        // SAFETY: freeing in reverse allocation order; each pointer is owned
        // solely by this struct and freed exactly once.
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testclip::gradient_clip;

    fn skip_without_vaapi() -> bool {
        if !crate::probe::vaapi_h264_decode_available() {
            eprintln!("no VA-API H.264 decode here; skipping");
            return true;
        }
        false
    }

    #[test]
    fn decodes_a_clip_into_vaapi_surfaces() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 5);
        assert!(!clip.is_empty(), "the test clip encoder produced nothing");

        let mut dec = H264Decoder::new().expect("open decoder");
        let mut frames = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                assert_eq!(frame.width(), 640);
                assert_eq!(frame.height(), 480);
                frames += 1;
            }
        }
        for frame in dec.flush().expect("flush") {
            assert_eq!(frame.width(), 640);
            frames += 1;
        }
        assert_eq!(frames, 5, "expected one decoded frame per encoded frame");
    }

    /// A decoder fed garbage must report, not panic and not wedge. The
    /// transport can deliver a corrupt access unit whenever FEC fails to
    /// recover one.
    #[test]
    fn garbage_input_does_not_panic() {
        if skip_without_vaapi() {
            return;
        }
        let mut dec = H264Decoder::new().expect("open decoder");
        let _ = dec.decode(&[0x00, 0x00, 0x00, 0x01, 0xff, 0xff, 0xff]);
    }
}
