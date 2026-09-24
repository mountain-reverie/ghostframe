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

    /// Raw pointer to the underlying frame, for callers that need ffmpeg
    /// APIs this crate does not wrap -- the decode oracle in
    /// `oracle_tests` downloads through `av_hwframe_transfer_data` to build
    /// the authoritative NV12 comparison. The frame stays owned by `self`;
    /// this pointer must not outlive it.
    pub fn as_ptr(&self) -> *const ffi::AVFrame {
        self.frame
    }

    /// Map this hardware surface to a DRM_PRIME dmabuf description.
    ///
    /// The returned [`MappedFrame`] owns the mapping; its
    /// [`planes()`](MappedFrame::planes)`.fd` is valid exactly as long as it
    /// lives, and must not be closed by the caller. `av_hwframe_map` takes
    /// its own reference on `self.frame`'s underlying VA-API surface, so the
    /// returned `MappedFrame` does not borrow from `self` and may outlive
    /// it -- `self` can be dropped (freeing this `HwFrame`'s own reference)
    /// while the mapping, and the dmabuf fd inside it, stay valid.
    pub fn map_dmabuf(&self) -> Result<MappedFrame, H264Error> {
        // SAFETY: `self.frame` is a live VAAPI frame owned by `self` for the
        // duration of this call. `drm` is a fresh allocation freed on every
        // error path below; on success ownership passes to `MappedFrame`,
        // which frees it exactly once in its own `Drop`.
        unsafe {
            let mut drm = ffi::av_frame_alloc();
            if drm.is_null() {
                return Err(H264Error::Ffmpeg("av_frame_alloc failed".into()));
            }
            (*drm).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
            let ret = ffi::av_hwframe_map(
                drm,
                self.frame,
                ffi::AV_HWFRAME_MAP_READ as i32 | ffi::AV_HWFRAME_MAP_DIRECT as i32,
            );
            if ret < 0 {
                ffi::av_frame_free(&mut drm);
                return Err(H264Error::Ffmpeg(format!(
                    "av_hwframe_map: {}",
                    ffmpeg_next::Error::from(ret)
                )));
            }
            let desc = (*drm).data[0] as *const ffi::AVDRMFrameDescriptor;
            if desc.is_null() {
                ffi::av_frame_free(&mut drm);
                return Err(H264Error::Descriptor(
                    "mapped frame has no descriptor".into(),
                ));
            }
            // SAFETY: `desc` is non-null and was just populated by
            // `av_hwframe_map` above; it stays live for as long as `drm`
            // does, which outlives this call.
            match crate::DmabufPlanes::from_descriptor(&*desc, self.width(), self.height()) {
                Ok(planes) => Ok(MappedFrame { drm, planes }),
                Err(e) => {
                    ffi::av_frame_free(&mut drm);
                    Err(e)
                }
            }
        }
    }
}

impl Drop for HwFrame {
    fn drop(&mut self) {
        // SAFETY: `frame` was allocated by `av_frame_alloc` and is dropped
        // exactly once here.
        unsafe { ffi::av_frame_free(&mut self.frame) };
    }
}

/// A hardware frame mapped to DRM_PRIME. Holds the mapped `AVFrame` alive,
/// because the fd inside [`crate::DmabufPlanes`] is a borrow into it: ffmpeg
/// closes the dmabuf fd when the mapped frame is unrefed, so `planes().fd`
/// is valid only for as long as this `MappedFrame` lives.
///
/// `planes` is deliberately private with a `&self` accessor rather than a
/// `pub` field. `DmabufPlanes` is `Copy`, so a `pub` field would let a
/// caller copy it out and use the fd after `self` drops -- not "fd is
/// closed" (which at least fails loudly) but "fd number was recycled by
/// the kernel", where a later `dup()` succeeds and silently imports an
/// unrelated file. [`Self::planes`] borrowing `&self` turns that into a
/// borrow-checker error at the call site instead: `import_nv12(&ctx,
/// m.planes())` ties the import call to `m`'s lifetime, and copying out
/// now requires writing `*m.planes()` explicitly -- exactly where a reader
/// should pause.
pub struct MappedFrame {
    drm: *mut ffi::AVFrame,
    planes: crate::DmabufPlanes,
}

// SAFETY: exclusive ownership of `drm`, freed exactly once in `Drop`.
unsafe impl Send for MappedFrame {}

impl MappedFrame {
    /// Borrow the dmabuf description. See the struct docs for why this is
    /// an accessor and not a `pub` field.
    pub fn planes(&self) -> &crate::DmabufPlanes {
        &self.planes
    }
}

impl Drop for MappedFrame {
    fn drop(&mut self) {
        // SAFETY: allocated by `av_frame_alloc` in `map_dmabuf`, dropped
        // exactly once here.
        unsafe { ffi::av_frame_free(&mut self.drm) };
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

/// Log frames that a caller can never see because `Result` can carry the
/// error that made them orphaned, or the frames themselves, but not both.
fn warn_dropped(frames: &[HwFrame], cause: &str) {
    if !frames.is_empty() {
        tracing::warn!(
            count = frames.len(),
            "discarding frames completed before {cause}"
        );
    }
}

/// Which ffmpeg call [`H264Decoder::submit`] is making, for error messages
/// -- so a log line can say whether `decode()` or `finish()` produced it,
/// which the two calls sharing one error format previously lost.
fn submit_label(au: Option<&[u8]>) -> &'static str {
    if au.is_some() {
        "send_packet"
    } else {
        "send_packet(NULL)"
    }
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
    /// On `EAGAIN` -- the decoder's internal buffer is full -- drains what
    /// is ready and retries the send once, rather than treating "resend
    /// this packet" as success and silently dropping the access unit. For
    /// H.264 a dropped access unit means corruption until the next IDR,
    /// with nothing to tell the caller it happened. See
    /// [`Self::submit_with_retry`] for the shared mechanics.
    pub fn decode(&mut self, au: &[u8]) -> Result<Vec<HwFrame>, H264Error> {
        self.submit_with_retry(Some(au))
    }

    /// Signal end of stream and drain what the decoder still holds.
    ///
    /// **Terminal.** After this the decoder returns `AVERROR_EOF` for every
    /// subsequent packet; use [`H264Decoder::reset`] to make it usable
    /// again. Deliberately NOT called `flush`: ffmpeg's
    /// `avcodec_flush_buffers` means the opposite thing (discard state and
    /// continue), and `reset` below is the wrapper for that.
    ///
    /// Shares [`Self::submit_with_retry`] with [`Self::decode`], so an
    /// `EAGAIN` here -- the decoder's internal buffer still full when the
    /// EOF signal is sent -- is drained and retried exactly like a normal
    /// access unit, rather than reported as a hard failure.
    pub fn finish(&mut self) -> Result<Vec<HwFrame>, H264Error> {
        self.submit_with_retry(None)
    }

    /// Send one packet to the decoder, or (`None`) the end-of-stream signal.
    ///
    /// `Ok(false)` means `EAGAIN`: the decoder's internal buffer is full and
    /// the caller must drain before resending the same input. `Ok(true)`
    /// means the input was accepted, *or* -- only on the EOF path (`au ==
    /// None`) -- that this is a second-or-later EOF signal and the decoder
    /// already reported `AVERROR_EOF` for the first one. Per `avcodec.h`,
    /// the first flush packet returns success and every one after it
    /// returns `AVERROR_EOF`; treating that as `Ok(true)` is what makes
    /// [`Self::finish`] idempotent.
    ///
    /// **That tolerance is gated to `au.is_none()` on purpose.** `finish()`
    /// puts the decoder into a state where every subsequent
    /// `avcodec_send_packet` -- packet or null -- returns `AVERROR_EOF`.
    /// Folding both senders into this one function once let `AVERROR_EOF`
    /// on a real access unit read as `Ok(true)` too, which made
    /// `decode()` after `finish()` (without an intervening [`Self::reset`])
    /// silently swallow the access unit and return an empty `Ok` instead of
    /// the error `77cc743` exists to guarantee.
    /// See `decode_after_finish_without_reset_is_an_error`.
    fn submit(&mut self, au: Option<&[u8]>) -> Result<bool, H264Error> {
        let ret = match au {
            Some(au) => {
                // SAFETY: `au` outlives the call, which copies what it needs
                // into ffmpeg's own buffers; `self.packet` is a live
                // allocation reset after every use so it never retains a
                // dangling pointer into `au`.
                unsafe {
                    (*self.packet).data = au.as_ptr() as *mut u8;
                    (*self.packet).size = au.len() as i32;
                    let ret = ffi::avcodec_send_packet(self.ctx, self.packet);
                    (*self.packet).data = ptr::null_mut();
                    (*self.packet).size = 0;
                    ret
                }
            }
            None => {
                // SAFETY: a null packet is ffmpeg's documented end-of-stream
                // signal.
                unsafe { ffi::avcodec_send_packet(self.ctx, ptr::null()) }
            }
        };
        if ret == ffi::AVERROR(libc::EAGAIN) {
            return Ok(false);
        }
        if ret == ffi::AVERROR_EOF && au.is_none() {
            return Ok(true);
        }
        if ret < 0 {
            return Err(H264Error::Ffmpeg(format!(
                "{}: {}",
                submit_label(au),
                ffmpeg_next::Error::from(ret)
            )));
        }
        Ok(true)
    }

    /// Submit `au` (or, for `None`, the end-of-stream signal), draining and
    /// retrying once on `EAGAIN` before giving up.
    ///
    /// This is the whole drain-retry-warn dance shared by [`Self::decode`]
    /// and [`Self::finish`], which used to be two independent copies that
    /// had already diverged: one warned about frames dropped on a hard
    /// resend error, the other silently discarded them. Factoring it out
    /// makes that omission impossible rather than merely fixed, on every
    /// exit that would otherwise drop frames the decoder already produced
    /// -- including the final drain below, after a successful submit --
    /// since `Result` can carry the error or the frames but not both, so
    /// they're logged instead of vanishing without a trace.
    fn submit_with_retry(&mut self, au: Option<&[u8]>) -> Result<Vec<HwFrame>, H264Error> {
        let mut out = match self.submit(au) {
            Ok(true) => Vec::new(),
            Ok(false) => {
                let out = self.drain()?;
                match self.submit(au) {
                    Ok(true) => out,
                    Ok(false) => {
                        warn_dropped(&out, "the decoder was still full after drain-and-retry");
                        return Err(H264Error::Ffmpeg(format!(
                            "{}: decoder still full after drain-and-retry",
                            submit_label(au)
                        )));
                    }
                    Err(e) => {
                        warn_dropped(&out, "a hard resend error");
                        return Err(e);
                    }
                }
            }
            Err(e) => {
                if let Ok(orphaned) = self.drain() {
                    warn_dropped(&orphaned, "a hard decode error");
                }
                return Err(e);
            }
        };
        match self.drain() {
            Ok(more) => {
                out.extend(more);
                Ok(out)
            }
            Err(e) => {
                warn_dropped(&out, "a hard error on the final drain");
                Err(e)
            }
        }
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

    /// `finish()` is terminal: once it succeeds, `avcodec_send_packet`
    /// returns `AVERROR_EOF` for every packet sent afterwards, real or
    /// null, until [`H264Decoder::reset`]. `decode()` must surface that as
    /// an error, not silently accept and drop the access unit -- this is
    /// exactly the regression a refactor introduced when `send_packet` and
    /// `send_eof` were merged into one `submit` and the EOF tolerance
    /// leaked onto the packet path. `reset_makes_the_decoder_usable_after_
    /// finish` above always calls `reset()` first, so it cannot catch this;
    /// this test deliberately does not.
    #[test]
    fn decode_after_finish_without_reset_is_an_error() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 2);
        let mut dec = H264Decoder::new().expect("open decoder");
        for au in &clip {
            dec.decode(au).expect("decode");
        }
        dec.finish().expect("finish");

        let msg = match dec.decode(&clip[0]) {
            Err(e) => e.to_string(),
            Ok(_) => panic!(
                "decode() after finish() without reset() must report an error, not silently \
                 accept and drop the access unit"
            ),
        };
        assert!(
            msg.contains("send_packet"),
            "error should name send_packet, the call that actually failed; got: {msg}"
        );
    }

    /// The `if ret == AVERROR_EOF && au.is_none() { return Ok(true) }` gate
    /// in [`H264Decoder::submit`] is what makes a second `finish()` return
    /// `Ok` instead of erroring -- per `avcodec.h`, only the first
    /// EOF-signalling `send_packet(NULL)` succeeds; every one after it
    /// returns `AVERROR_EOF`, which this gate turns back into `Ok(true)`.
    /// Removing the whole gate (not just its `au.is_none()` half -- that
    /// half's own regression is `decode_after_finish_without_reset_is_an_
    /// error`'s job) would leave every one of the other 21 tests in this
    /// file passing, since none of them call `finish()` twice: this is the
    /// one that would catch it.
    #[test]
    fn finish_is_idempotent() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 2);
        let mut dec = H264Decoder::new().expect("open decoder");
        for au in &clip {
            dec.decode(au).expect("decode");
        }
        dec.finish().expect("first finish");
        dec.finish().expect("second finish must also be Ok");
    }

    /// The real descriptor from real hardware. Records the modifier in the
    /// failure message so a tiled surface names itself rather than showing
    /// up later as corrupted pixels.
    #[test]
    fn maps_a_decoded_frame_to_a_dmabuf() {
        if skip_without_vaapi() {
            return;
        }
        let clip = gradient_clip(640, 480, 3);
        let mut dec = H264Decoder::new().expect("open decoder");
        let mut mapped = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                let m = frame.map_dmabuf().expect("map to dmabuf");
                let planes = m.planes();
                assert!(planes.fd >= 0, "dmabuf fd must be valid");
                assert_eq!(planes.width, 640);
                assert_eq!(planes.height, 480);
                assert_eq!(planes.fourcc_luma, crate::DRM_FORMAT_R8);
                assert_eq!(planes.fourcc_chroma, crate::DRM_FORMAT_GR88);
                assert!(
                    planes.luma.offset + planes.luma.pitch * planes.height as u64 <= planes.size,
                    "luma plane overruns the {}-byte dmabuf object",
                    planes.size
                );
                assert!(
                    planes.luma.pitch >= 640,
                    "luma pitch {} is narrower than the frame",
                    planes.luma.pitch
                );
                eprintln!(
                    "[m3] modifier=0x{:016x} size={} luma(off={},pitch={}) chroma(off={},pitch={})",
                    planes.modifier,
                    planes.size,
                    planes.luma.offset,
                    planes.luma.pitch,
                    planes.chroma.offset,
                    planes.chroma.pitch
                );
                mapped += 1;
            }
        }
        assert!(mapped > 0, "no frame was mapped");
    }
}
