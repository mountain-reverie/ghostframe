//! VA-API H.264 decode: access units in, hardware surfaces out.
//!
//! Mirrors ffmpeg's canonical `hw_decode.c`: set `hw_device_ctx`, override
//! `get_format` to pin AV_PIX_FMT_VAAPI, and let the decoder allocate its own
//! hardware frames context. The `get_format` override is what stops ffmpeg
//! silently falling back to software decode -- which would still *work*, and
//! would quietly cost a full-frame CPU download per frame.

use crate::H264Error;
use ffmpeg_sys_next as ffi;
use std::ffi::{c_void, CString};
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

    /// Download this surface to system memory as tightly packed NV12.
    ///
    /// The fallback when the dmabuf cannot be imported (tiled modifier,
    /// misaligned plane offset, or a driver row pitch that disagrees with
    /// the producer's). Costs a full-frame copy across the bus; correct
    /// everywhere. Mirrors `oracle_tests::hw_frame_to_nv12`, which this same
    /// crate's exactness oracle already exercises against a software
    /// decode, byte for byte.
    pub fn download_nv12(&self) -> Option<(Vec<u8>, Vec<u8>)> {
        let w = self.width() as usize;
        let h = self.height() as usize;
        // SAFETY: `self.frame` is a live VAAPI frame owned by `self` for the
        // duration of this call. `sw` is a fresh allocation freed on every
        // path out of this function through the named `sw` binding (never a
        // throwaway `&mut { sw }` temporary), so a future edit that touches
        // `sw` after `av_frame_free` would see ffmpeg's null-out instead of
        // reading a dangling pointer.
        unsafe {
            let mut sw = ffi::av_frame_alloc();
            if sw.is_null() {
                return None;
            }
            (*sw).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
            if ffi::av_hwframe_transfer_data(sw, self.frame, 0) < 0 {
                ffi::av_frame_free(&mut sw);
                return None;
            }
            let y_stride = (*sw).linesize[0] as usize;
            let uv_stride = (*sw).linesize[1] as usize;
            let mut luma = Vec::with_capacity(w * h);
            for row in 0..h {
                luma.extend_from_slice(std::slice::from_raw_parts(
                    (*sw).data[0].add(row * y_stride),
                    w,
                ));
            }
            let chroma_h = h.div_ceil(2);
            let mut chroma = Vec::with_capacity(w * chroma_h);
            for row in 0..chroma_h {
                chroma.extend_from_slice(std::slice::from_raw_parts(
                    (*sw).data[1].add(row * uv_stride),
                    w,
                ));
            }
            ffi::av_frame_free(&mut sw);
            Some((luma, chroma))
        }
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

/// `initial_pool_size` for a pre-warmed hardware frames pool: matches
/// ffmpeg's own sizing for VA-API H.264 decode (`vaapi_decode_make_config`
/// in `libavcodec/vaapi_decode.c`, the `!CONFIG_VAAPI_1` branch): one base
/// surface plus 16 for H.264's DPB (`ref_frame_count` plus short/long-term
/// reference slack). Ffmpeg normally computes this itself inside
/// `avcodec_get_hw_frames_parameters`, but that path only runs when ffmpeg
/// builds its own frames context -- since [`H264Decoder::with_prewarm`]
/// builds one ahead of time instead, nothing upstream sizes it for us.
const PREWARM_POOL_SIZE: i32 = 1 + 16;

/// A hardware frames pool built ahead of time at
/// [`H264Decoder::with_prewarm`]'s max resolution, so the VA-API driver's
/// (often disproportionately expensive) first-surface allocation happens
/// during `H264Decoder::new`/`with_prewarm`, not inside the first
/// `avcodec_send_packet` of a session -- see this module's doc and design
/// doc §10.
///
/// Reached from [`get_vaapi_format`] through `AVCodecContext::opaque`,
/// which ffmpeg never touches itself (`avcodec.h`: "Set by user" for both
/// encoding and decoding). [`H264Decoder`] boxes this so its heap address
/// stays fixed even if the `H264Decoder` itself is moved -- moving a `Box`
/// moves the pointer, not the pointee -- and owns it for at least as long
/// as `ctx` is open: `H264Decoder::drop` frees `ctx` (which may hold its
/// own separate reference on `frames`, taken by `get_vaapi_format`) before
/// this struct's own `Drop` unrefs the master reference.
struct PrewarmPool {
    /// The one reference this struct owns. Every `AVCodecContext` that ends
    /// up using the pool holds an *independent* reference of its own
    /// (`av_buffer_ref` in `get_vaapi_format`), so dropping this one does
    /// not invalidate frames a still-open codec context is using -- see
    /// [`Drop for PrewarmPool`](#impl-Drop-for-PrewarmPool).
    frames: *mut ffi::AVBufferRef,
    width: u32,
    height: u32,
}

// SAFETY: exclusive ownership of `frames`, freed exactly once in `Drop`; no
// thread affinity in the underlying `AVHWFramesContext`/VA-API device.
unsafe impl Send for PrewarmPool {}

impl Drop for PrewarmPool {
    fn drop(&mut self) {
        // SAFETY: `frames` was allocated by `av_hwframe_ctx_alloc` and
        // initialised by `av_hwframe_ctx_init` in `build_prewarm_pool`,
        // and this struct owns that one reference exclusively. Any codec
        // context that copied a separate reference in `get_vaapi_format`
        // holds its own independent refcount on the same underlying
        // buffer (`av_buffer_ref` does not share ownership of *this*
        // handle), so unreffing this one does not free the buffer out
        // from under a codec context still using it -- ffmpeg's
        // underlying `AVBufferPool` is only torn down once every
        // reference, this one included, has been unreffed.
        unsafe { ffi::av_buffer_unref(&mut self.frames) };
    }
}

/// Build and `av_hwframe_ctx_init` an `AVHWFramesContext` sized to
/// `width`x`height` on `hw_device`, with a fixed VA-API surface pool sized
/// for H.264's DPB (see [`PREWARM_POOL_SIZE`]).
///
/// `format`/`sw_format` mirror what [`get_vaapi_format`] pins the decoder
/// to (`AV_PIX_FMT_VAAPI` over `AV_PIX_FMT_NV12`) -- `ff_get_format`
/// (`libavcodec/decode.c`) rejects a supplied `hw_frames_ctx` whose
/// `format` disagrees with the chosen pixel format, so these must match.
fn build_prewarm_pool(
    hw_device: *mut ffi::AVBufferRef,
    width: u32,
    height: u32,
) -> Result<*mut ffi::AVBufferRef, H264Error> {
    // SAFETY: `hw_device` is a live `AVBufferRef` owned by the caller
    // (`H264Decoder::open`) for at least the duration of this call.
    // `av_hwframe_ctx_alloc` takes its own reference on it internally
    // (`libavutil/hwcontext.c`: `av_buffer_ref(device_ref_in)`) rather than
    // consuming the caller's, so `hw_device` is untouched by this
    // function and remains the caller's to free. `frames_ref` is a fresh
    // allocation freed on the error path below; on success ownership
    // passes to the caller.
    unsafe {
        let mut frames_ref = ffi::av_hwframe_ctx_alloc(hw_device);
        if frames_ref.is_null() {
            return Err(H264Error::Ffmpeg("av_hwframe_ctx_alloc failed".into()));
        }
        let frames = (*frames_ref).data as *mut ffi::AVHWFramesContext;
        (*frames).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*frames).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames).width = width as i32;
        (*frames).height = height as i32;
        (*frames).initial_pool_size = PREWARM_POOL_SIZE;

        let ret = ffi::av_hwframe_ctx_init(frames_ref);
        if ret < 0 {
            ffi::av_buffer_unref(&mut frames_ref);
            return Err(H264Error::Ffmpeg(format!(
                "av_hwframe_ctx_init (prewarm {width}x{height}): {}",
                ffmpeg_next::Error::from(ret)
            )));
        }
        Ok(frames_ref)
    }
}

/// Pin AV_PIX_FMT_VAAPI out of the decoder's offered format list, and --
/// when [`H264Decoder`] was built with [`H264Decoder::with_prewarm`] and
/// the pre-warmed pool is large enough for this stream -- hand the decoder
/// a ready-made hardware frames pool instead of letting it allocate its
/// own.
///
/// # Safety
/// Called by ffmpeg with a valid NUL-terminated (`AV_PIX_FMT_NONE`) format
/// array. Returning a format not in that list is undefined behaviour, so the
/// fallback returns `AV_PIX_FMT_NONE`, which ffmpeg treats as "cannot decode".
///
/// `ctx` is non-null (ffmpeg's documented `get_format` contract) and safe to
/// dereference read-write for the duration of this call. This function runs
/// on ffmpeg's own stack (inside `avcodec_send_packet`, via `ff_get_format`)
/// and must never panic; every path below is infallible arithmetic or a
/// checked ffmpeg call.
unsafe extern "C" fn get_vaapi_format(
    ctx: *mut ffi::AVCodecContext,
    fmts: *const ffi::AVPixelFormat,
) -> ffi::AVPixelFormat {
    if fmts.is_null() || ctx.is_null() {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }
    // SAFETY: `fmts` is non-null, and per this function's documented
    // contract (ffmpeg's `AVCodecContext::get_format`) it points at an array
    // that is read-only here and NUL-terminated with `AV_PIX_FMT_NONE`,
    // which bounds the walk below.
    let has_vaapi = unsafe {
        let mut p = fmts;
        let mut found = false;
        while *p != ffi::AVPixelFormat::AV_PIX_FMT_NONE {
            if *p == ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                found = true;
                break;
            }
            p = p.add(1);
        }
        found
    };
    if !has_vaapi {
        return ffi::AVPixelFormat::AV_PIX_FMT_NONE;
    }

    // SAFETY: `ctx` is non-null (checked above). `(*ctx).opaque`, if
    // non-null, was written by `H264Decoder::open` to a `Box<PrewarmPool>`
    // leaked for exactly this purpose before `ctx` was ever handed to
    // ffmpeg, and stays valid for as long as `ctx` itself does --
    // `H264Decoder::drop` frees `ctx` before reclaiming the box (see
    // `PrewarmPool`'s doc). `(*ctx).coded_width`/`coded_height` are safe to
    // read here: ffmpeg's H.264 decoder sets them (`init_dimensions` in
    // `h264_slice.c`) before ever calling `get_format`, which this function
    // is -- verified against ffmpeg's `libavcodec/h264_slice.c` (n9.0.1).
    // `ff_get_format` (`libavcodec/decode.c`) also guarantees
    // `(*ctx).hw_frames_ctx` is already null here (`ff_hwaccel_uninit`
    // unrefs it immediately before calling this callback), so assigning it
    // below never leaks a previous reference.
    unsafe {
        if !(*ctx).opaque.is_null() {
            let pool = &*((*ctx).opaque as *const PrewarmPool);
            let coded_w = (*ctx).coded_width as i64;
            let coded_h = (*ctx).coded_height as i64;
            if coded_w >= 0
                && coded_h >= 0
                && pool.width as i64 >= coded_w
                && pool.height as i64 >= coded_h
            {
                let r = ffi::av_buffer_ref(pool.frames);
                if !r.is_null() {
                    (*ctx).hw_frames_ctx = r;
                }
                // `av_buffer_ref` returning null (allocation failure) is
                // left as the graceful fallback below: `hw_frames_ctx`
                // stays null and ffmpeg allocates its own frames context
                // sized to the stream, exactly as it would for a stream
                // that outgrew the pre-warmed pool.
            }
            // Else: the stream is larger than the pre-warmed pool.
            // `hw_frames_ctx` stays null; `ff_decode_get_hw_frames_ctx`
            // (`libavcodec/decode.c`) then builds a correctly-sized one on
            // demand -- slower, but correct. This is the graceful
            // oversized-stream path.
        }
    }

    ffi::AVPixelFormat::AV_PIX_FMT_VAAPI
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
    /// `Some` only when constructed via [`H264Decoder::with_prewarm`] with
    /// both dimensions non-zero AND the pre-warm actually succeeded.
    /// `(*ctx).opaque` points at this box's heap allocation for as long as
    /// `ctx` is open -- see [`PrewarmPool`]'s doc for the lifetime
    /// argument. Declared last so the derived part of `Drop` (which runs
    /// after this struct's own `Drop::drop` body) frees it after `ctx`
    /// itself has already been freed.
    prewarm: Option<Box<PrewarmPool>>,
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
        Self::open(crate::probe::RENDER_NODE, 0, 0)
    }

    pub fn with_device(node: &str) -> Result<Self, H264Error> {
        Self::open(node, 0, 0)
    }

    /// Open the decoder and, when both `max_width` and `max_height` are
    /// non-zero, pre-build a hardware frames pool sized to them --
    /// `0`x`0` (or either dimension `0`) means "unknown, do not pre-warm",
    /// identical to [`H264Decoder::new`].
    ///
    /// A pre-warm attempt that fails does NOT fail the whole call: the
    /// decoder still opens and works, exactly as if `max_width`/
    /// `max_height` had been `0` -- ffmpeg allocates its own frames context
    /// per resolution encountered instead. See [`get_vaapi_format`]'s doc
    /// for the runtime fallback this enables when a stream turns out
    /// larger than `max_width`x`max_height` too: never a hard failure,
    /// only ever a speed difference.
    pub fn with_prewarm(max_width: u32, max_height: u32) -> Result<Self, H264Error> {
        Self::open(crate::probe::RENDER_NODE, max_width, max_height)
    }

    fn open(node: &str, max_width: u32, max_height: u32) -> Result<Self, H264Error> {
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

            // Pre-warm is best-effort: a failure here does not fail
            // `open()` itself, it just means this decoder behaves exactly
            // as if `max_width`/`max_height` were `0` -- see
            // `with_prewarm`'s doc.
            let prewarm = if max_width > 0 && max_height > 0 {
                match build_prewarm_pool(hw_device, max_width, max_height) {
                    Ok(frames) => {
                        let boxed = Box::new(PrewarmPool {
                            frames,
                            width: max_width,
                            height: max_height,
                        });
                        // `ctx.opaque` gets the box's heap address, not
                        // ownership: `H264Decoder` (via `prewarm` below)
                        // keeps the one owning `Box` for its whole
                        // lifetime. Moving a `Box` moves only the pointer
                        // wrapper -- the heap allocation this address
                        // points at does not move -- so this pointer stays
                        // valid even after `H264Decoder` itself is moved.
                        let ptr: *mut PrewarmPool = &*boxed as *const PrewarmPool as *mut _;
                        (*ctx).opaque = ptr as *mut c_void;
                        Some(boxed)
                    }
                    Err(e) => {
                        tracing::warn!(
                            error = %e,
                            max_width,
                            max_height,
                            "H.264 decoder pre-warm failed; falling back to \
                             per-resolution frames-context allocation"
                        );
                        None
                    }
                }
            } else {
                None
            };

            Ok(H264Decoder {
                ctx,
                hw_device,
                packet,
                prewarm,
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

    /// The pre-warmed hardware frames pool's dimensions, if this decoder
    /// actually has one. `None` covers every non-pre-warmed case alike:
    /// `new`/`with_device` (never asked for one), `with_prewarm(0, _)` /
    /// `with_prewarm(_, 0)` ("unknown, do not pre-warm"), and a
    /// `with_prewarm` call whose pool build itself failed and fell back
    /// gracefully -- see `with_prewarm`'s doc. Test/diagnostic use: lets a
    /// caller confirm which path a given decoder actually took without
    /// depending on log-message text.
    pub fn prewarm_pool_dims(&self) -> Option<(u32, u32)> {
        self.prewarm.as_ref().map(|p| (p.width, p.height))
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
        // solely by this struct and freed exactly once. `avcodec_free_context`
        // closes the codec (running `ff_hwaccel_uninit`, which unrefs
        // whatever separate `hw_frames_ctx` reference `get_vaapi_format` gave
        // it) before `self.prewarm`'s own `Drop` (run automatically, after
        // this function returns, since `prewarm` is a plain struct field)
        // unrefs the master reference -- but the order between those two
        // unrefs does not actually matter for correctness: each is an
        // independent refcount on the same underlying `AVBufferPool`
        // (`av_buffer_ref` in `get_vaapi_format` created a distinct handle),
        // which ffmpeg only tears down once every reference, from either
        // side, has been unreffed.
        unsafe {
            ffi::av_packet_free(&mut self.packet);
            ffi::avcodec_free_context(&mut self.ctx);
            ffi::av_buffer_unref(&mut self.hw_device);
        }
        // `self.prewarm` (an `Option<Box<PrewarmPool>>`) drops here, after
        // this body, via the compiler-generated field drop -- see
        // `PrewarmPool`'s own `Drop` for what that does.
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
    ///
    /// Passes `crate::probe::RENDER_NODE` explicitly -- every test below
    /// opens the decoder through `H264Decoder::new()`, which opens exactly
    /// that node, so ground truth must be established against the same
    /// device the test actually exercises.
    fn skip_without_vaapi() -> bool {
        match crate::probe::vainfo_reports_h264_vld(crate::probe::RENDER_NODE) {
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
        // `msg.contains("send_packet")` alone would also match the
        // `"send_packet(NULL)"` label `finish()` uses -- checking for the
        // bare label followed by `:` (the exact prefix `submit_label`
        // produces for `au: Some(_)`) is what actually pins this down to
        // the `decode()` call, not just any call through `submit`.
        assert!(
            msg.contains("send_packet:"),
            "error should name plain send_packet (not send_packet(NULL)), the call that \
             actually failed; got: {msg}"
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

    /// `with_prewarm(0, 0)` must behave exactly like `new()`: no pool, and
    /// decode still works. Preserves today's behaviour for hosts that do
    /// not know their max resolution.
    #[test]
    fn prewarm_zero_means_no_prewarm() {
        if skip_without_vaapi() {
            return;
        }
        let mut dec = H264Decoder::with_prewarm(0, 0).expect("open decoder");
        assert_eq!(dec.prewarm_pool_dims(), None);
        let clip = gradient_clip(640, 480, 3);
        let mut frames = 0;
        for au in &clip {
            frames += dec.decode(au).expect("decode").len();
        }
        frames += dec.finish().expect("finish").len();
        assert!(
            frames > 0,
            "decoder with no pre-warm still produced nothing"
        );
    }

    /// A stream that exactly matches the pre-warmed size must decode
    /// correctly using the warm pool.
    #[test]
    fn prewarm_matching_stream_decodes() {
        if skip_without_vaapi() {
            return;
        }
        let mut dec = H264Decoder::with_prewarm(640, 480).expect("open decoder");
        assert_eq!(dec.prewarm_pool_dims(), Some((640, 480)));
        let clip = gradient_clip(640, 480, 3);
        let mut frames = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                assert_eq!(frame.width(), 640);
                assert_eq!(frame.height(), 480);
                frames += 1;
            }
        }
        frames += dec.finish().expect("finish").len();
        assert!(frames > 0, "pre-warmed decoder produced nothing");
    }

    /// A stream *smaller* than the pre-warmed pool must still decode
    /// correctly (the pool is large enough, `get_vaapi_format` hands it
    /// over per the `>=` check).
    #[test]
    fn prewarm_larger_than_stream_still_decodes() {
        if skip_without_vaapi() {
            return;
        }
        let mut dec = H264Decoder::with_prewarm(1920, 1080).expect("open decoder");
        assert_eq!(dec.prewarm_pool_dims(), Some((1920, 1080)));
        let clip = gradient_clip(640, 480, 3);
        let mut frames = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                assert_eq!(frame.width(), 640);
                assert_eq!(frame.height(), 480);
                frames += 1;
            }
        }
        frames += dec.finish().expect("finish").len();
        assert!(
            frames > 0,
            "decoder produced nothing for a stream smaller than the pool"
        );
    }

    /// The owner's "bigger frame" case: a stream *larger* than the
    /// pre-warmed pool must still decode correctly -- `get_vaapi_format`'s
    /// `>=` check must fail closed (leave `hw_frames_ctx` null, let ffmpeg
    /// allocate its own) rather than handing over an undersized pool that
    /// would corrupt or crash the decode.
    #[test]
    fn prewarm_smaller_than_stream_still_decodes_correctly() {
        if skip_without_vaapi() {
            return;
        }
        let mut dec = H264Decoder::with_prewarm(320, 240).expect("open decoder");
        assert_eq!(dec.prewarm_pool_dims(), Some((320, 240)));
        let clip = gradient_clip(640, 480, 3);
        let mut frames = 0;
        for au in &clip {
            for frame in dec.decode(au).expect("decode") {
                assert_eq!(frame.width(), 640);
                assert_eq!(frame.height(), 480);
                frames += 1;
            }
        }
        frames += dec.finish().expect("finish").len();
        assert_eq!(
            frames, 3,
            "decoder must decode every frame of an oversized stream correctly, not just avoid a crash"
        );
    }
}
