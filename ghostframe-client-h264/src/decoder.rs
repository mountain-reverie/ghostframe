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
    fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if fmts.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    // SAFETY: `fmts` is non-null, and per this function's documented
    // contract (ffmpeg's `AVCodecContext::get_format`) it points at an array
    // that is read-only here and NUL-terminated with `AV_PIX_FMT_NONE`,
    // which bounds the walk below.
    unsafe {
        let mut p = fmts;
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
            }
            p = p.add(1);
        }
    }
    ffi::AVPixelFormat::AV_PIX_FMT_NONE
}

pub struct H264Decoder {
    ctx: *mut ffi::AVCodecContext,
    hw_device: *mut ffi::AVBufferRef,
    packet: *mut ffi::AVPacket,
}

// SAFETY: `Send` only requires that this struct be safe to move to another
// thread and used there -- not safe for concurrent use from two threads at
// once, which would be `Sync` (deliberately not implemented: ffmpeg codec
// contexts are not safe for concurrent access). Neither `AVCodecContext` nor
// ffmpeg's VA-API device context carries thread-affine state, and libva's
// DRM display handle has no thread affinity either, so a full move -- opened
// on one thread, driven on another -- is sound.
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
                    "av_hwdevice_ctx_create({node}): {}",
                    ffmpeg_next::Error::from(ret)
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

            // `av_buffer_ref` can return null (allocation failure); left
            // unchecked, `avcodec_open2` below still succeeds with a null
            // `hw_device_ctx`, and the first decode then fails opaquely
            // inside `get_format` instead of here, with a clear cause.
            let dup = ffi::av_buffer_ref(hw_device);
            if dup.is_null() {
                let mut c = ctx;
                ffi::avcodec_free_context(&mut c);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg("av_buffer_ref(hw_device) failed".into()));
            }
            (*ctx).hw_device_ctx = dup;
            (*ctx).get_format = Some(get_vaapi_format);

            let ret = ffi::avcodec_open2(ctx, codec, ptr::null_mut());
            if ret < 0 {
                let mut c = ctx;
                ffi::avcodec_free_context(&mut c);
                ffi::av_buffer_unref(&mut hw_device);
                return Err(H264Error::Ffmpeg(format!(
                    "avcodec_open2: {}",
                    ffmpeg_next::Error::from(ret)
                )));
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
    ///
    /// On `EAGAIN` -- the decoder's internal buffer is full -- this drains
    /// what is ready and retries the send once, rather than treating
    /// "resend this packet" as success and silently dropping the access
    /// unit. For H.264 a dropped access unit means corruption until the
    /// next IDR, with nothing to tell the caller it happened. `drain()`
    /// empties the decoder after every call here, which is why EAGAIN is
    /// normally unreachable in testing; it exists for whatever buffering
    /// condition triggers it first in production.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<HwFrame>, H264Error> {
        let mut out = match self.send_packet(au) {
            Ok(true) => Vec::new(),
            Ok(false) => {
                let out = self.drain()?;
                if !self.send_packet(au)? {
                    return Err(H264Error::Ffmpeg(
                        "send_packet: decoder still full after drain-and-retry".into(),
                    ));
                }
                out
            }
            Err(e) => {
                // The send failed outright, but frames the decoder finished
                // before this call could still be sitting in its output
                // queue. Drain and log rather than silently discarding them
                // along with the error this call reports -- `Result` can't
                // carry both, so they can't be returned to the caller, but
                // they don't have to vanish without a trace either.
                if let Ok(orphaned) = self.drain() {
                    if !orphaned.is_empty() {
                        tracing::warn!(
                            count = orphaned.len(),
                            "discarding frames completed before a hard decode error"
                        );
                    }
                }
                return Err(e);
            }
        };
        out.extend(self.drain()?);
        Ok(out)
    }

    /// Send one packet to the decoder.
    ///
    /// `Ok(false)` means `EAGAIN`: the decoder's internal buffer is full and
    /// the caller must drain before resending the same packet. `Ok(true)`
    /// means the packet was accepted.
    fn send_packet(&mut self, au: &[u8]) -> Result<bool, H264Error> {
        // SAFETY: `au` outlives the call, which copies what it needs into
        // ffmpeg's own buffers; `self.packet` is a live allocation reset
        // after every use so it never retains a dangling pointer into `au`.
        unsafe {
            (*self.packet).data = au.as_ptr() as *mut u8;
            (*self.packet).size = au.len() as i32;
            let ret = ffi::avcodec_send_packet(self.ctx, self.packet);
            (*self.packet).data = ptr::null_mut();
            (*self.packet).size = 0;
            if ret == ffi::AVERROR(libc::EAGAIN) {
                return Ok(false);
            }
            if ret < 0 {
                return Err(H264Error::Ffmpeg(format!(
                    "send_packet: {}",
                    ffmpeg_next::Error::from(ret)
                )));
            }
        }
        Ok(true)
    }

    /// Signal end of stream and drain what the decoder still holds.
    ///
    /// **Terminal.** After this the decoder returns `AVERROR_EOF` for every
    /// subsequent packet; use [`H264Decoder::reset`] to make it usable
    /// again. Deliberately NOT called `flush`: ffmpeg's
    /// `avcodec_flush_buffers` means the opposite thing (discard state and
    /// continue), and `reset` below is the wrapper for that.
    pub fn finish(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        // SAFETY: a null packet is ffmpeg's documented end-of-stream signal.
        unsafe {
            let ret = ffi::avcodec_send_packet(self.ctx, ptr::null());
            if ret < 0 && ret != ffi::AVERROR_EOF {
                return Err(H264Error::Ffmpeg(format!(
                    "send_packet(NULL): {}",
                    ffmpeg_next::Error::from(ret)
                )));
            }
        }
        self.drain()
    }

    /// Discard buffered state and continue decoding -- ffmpeg's
    /// `avcodec_flush_buffers`.
    ///
    /// Needed on stream discontinuity: unrecovered loss, a resolution
    /// change, or a session reset. Without it the decoder keeps trying to
    /// reference frames that will never arrive, and every output until the
    /// next keyframe is built on stale references. Also the way back from
    /// [`H264Decoder::finish`]'s terminal `AVERROR_EOF` state.
    pub fn reset(&mut self) {
        // SAFETY: `self.ctx` is an open codec context owned solely by `self`.
        unsafe { ffi::avcodec_flush_buffers(self.ctx) };
    }

    fn drain(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        let mut out = Vec::new();
        loop {
            // SAFETY: `av_frame_alloc` returns an owned frame or null; the
            // frame is either moved into `out` or freed before we return.
            let mut frame = unsafe { ffi::av_frame_alloc() };
            if frame.is_null() {
                return Err(H264Error::Ffmpeg("av_frame_alloc failed".into()));
            }
            // SAFETY: `self.ctx` is open; `frame` is a fresh allocation.
            let ret = unsafe { ffi::avcodec_receive_frame(self.ctx, frame) };
            if ret < 0 {
                // SAFETY: nothing took ownership of `frame`. Freed through a
                // named `mut` binding, not a throwaway `&mut { frame }`
                // temporary, so ffmpeg's null-out lands somewhere a future
                // edit that touches `frame` after this point would see it,
                // instead of silently reading a dangling pointer.
                unsafe { ffi::av_frame_free(&mut frame) };
                if ret == ffi::AVERROR(libc::EAGAIN) || ret == ffi::AVERROR_EOF {
                    return Ok(out);
                }
                return Err(H264Error::Ffmpeg(format!(
                    "receive_frame: {}",
                    ffmpeg_next::Error::from(ret)
                )));
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

    /// Skip unless independent ground truth (`vainfo`, NOT this crate's own
    /// probe) says the driver can decode H.264.
    ///
    /// Gating on `vaapi_h264_decode_available()` would be circular: since
    /// the probe decodes a real frame through `H264Decoder`, any regression
    /// in the decoder makes the probe return `false`, which makes this
    /// helper skip the test, which reports green exactly when the decoder
    /// is broken.
    fn skip_without_vaapi() -> bool {
        match crate::probe::vainfo_reports_h264_vld() {
            Some(true) => false,
            Some(false) => {
                eprintln!("driver reports no H.264 VLD entrypoint; skipping");
                true
            }
            None => true, // vainfo_reports_h264_vld already explained why
        }
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
        for frame in dec.finish().expect("finish") {
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

    /// `finish()` is terminal; `reset()` is the way back. Task 9 depends on
    /// this for stream discontinuity (loss, resolution change, session
    /// reset), so it needs to actually work, not just compile.
    #[test]
    fn reset_makes_the_decoder_usable_after_finish() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 2);
        let mut dec = H264Decoder::new().expect("open decoder");
        for au in &clip {
            dec.decode(au).expect("decode");
        }
        dec.finish().expect("finish");
        dec.reset();

        let mut frames = 0;
        for au in &clip {
            frames += dec.decode(au).expect("decode after reset").len();
        }
        frames += dec.finish().expect("finish after reset").len();
        assert!(frames > 0, "decoder produced nothing after reset");
    }
}
