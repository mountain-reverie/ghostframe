//! H.264 tile encoder using VA-API hardware acceleration with libx264 fallback.
//!
//! Each encoder instance maintains inter-frame state for a single 32x32 tile
//! position, enabling P-frames to reference the previous tile content.
//!
//! VA-API hardware encoders typically require a minimum resolution (e.g. 128x128
//! on AMD). When the tile size is below the hardware minimum, the encoder pads
//! the frame to the minimum size.  The H.264 SPS in the bitstream carries the
//! actual coded dimensions so the decoder can discover them automatically.

extern crate ffmpeg_next as ffmpeg;

use std::ffi::CString;
use std::ptr;
use std::sync::Once;

use ffmpeg::codec;
use ffmpeg::encoder;
use ffmpeg::format::Pixel;
use ffmpeg::frame;
use ffmpeg::software::scaling;
use ffmpeg::Packet;
use ffmpeg::Rational;

use ffmpeg_sys_next as ffi;
use tracing::{info, warn};

static FFMPEG_INIT: Once = Once::new();

use super::EncodedTile;
use crate::transport::protocol::Codec as GfCodec;

const TILE_W: u32 = 32;
const TILE_H: u32 = 32;

/// Default VA-API render device path.
const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Candidate encoding resolutions to try when the tile size is below the
/// hardware minimum. Sorted ascending; the first one that succeeds wins.
const VAAPI_CANDIDATE_SIZES: &[(u32, u32)] = &[
    (TILE_W, TILE_H), // try native size first
    (128, 128),       // common AMD minimum
    (256, 256),       // fallback
];

/// RAII wrapper around `*mut AVBufferRef` so we don't leak hw contexts.
struct BufRef(*mut ffi::AVBufferRef);

impl Drop for BufRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0) };
        }
    }
}

// SAFETY: AVBufferRef is refcounted and thread-safe.
unsafe impl Send for BufRef {}

/// H.264 encoder for 32x32 BGRA tiles.
///
/// Tries `h264_vaapi` first, falls back to `libx264` if hardware encoding
/// is unavailable. Input is always 32x32 BGRA (4096 bytes); output is H.264
/// NAL units suitable for network transmission.
pub struct H264VaapiEncoder {
    encoder: encoder::Video,
    scaler: scaling::Context,
    pts: i64,
    /// True when the encoder is h264_vaapi (need HW frame upload in encode()).
    use_vaapi: bool,
    /// The dimensions the encoder was actually opened at (may be > TILE_W/H
    /// when padded for hardware minimum constraints).
    enc_w: u32,
    enc_h: u32,
    /// Keeps the HW device context alive for the encoder's lifetime.
    _hw_device_ctx: Option<BufRef>,
    /// HW frames context — we need this to allocate HW frames for upload.
    hw_frames_ctx: Option<BufRef>,
}

// SAFETY: ffmpeg encoder/scaler contexts contain raw pointers but do not use
// thread-local state. A single encoder instance is only accessed from one
// thread at a time (the IoBridge event loop), so Send is safe.
unsafe impl Send for H264VaapiEncoder {}

impl H264VaapiEncoder {
    /// Create a new encoder, preferring VA-API but falling back to libx264.
    pub fn new() -> Result<Self, ffmpeg::Error> {
        FFMPEG_INIT.call_once(|| {
            ffmpeg::init().expect("ffmpeg::init() failed");
        });

        // Try VA-API first, attempting multiple resolutions to find one
        // above the hardware minimum.
        if let Some(vaapi_codec) = encoder::find_by_name("h264_vaapi") {
            let mut last_err = None;
            for &(w, h) in VAAPI_CANDIDATE_SIZES {
                match Self::try_open_vaapi(vaapi_codec, w, h) {
                    Ok(this) => {
                        info!(
                            enc_w = this.enc_w,
                            enc_h = this.enc_h,
                            "H264VaapiEncoder: using h264_vaapi hardware encoder"
                        );
                        return Ok(this);
                    }
                    Err(e) => {
                        info!(
                            w, h,
                            err = %e,
                            "VA-API: failed to open encoder at this resolution, trying next"
                        );
                        last_err = Some(e);
                    }
                }
            }
            if let Some(e) = last_err {
                warn!("H264VaapiEncoder: VA-API open failed at all resolutions ({e}), falling back to libx264");
            }
        }

        let x264_codec = encoder::find_by_name("libx264").ok_or(ffmpeg::Error::EncoderNotFound)?;
        info!("H264VaapiEncoder: using libx264 software encoder");
        Self::try_open_sw(x264_codec)
    }

    // -----------------------------------------------------------------------
    // VA-API helpers
    // -----------------------------------------------------------------------

    /// Create a VA-API hardware device context for the given render node.
    unsafe fn create_hw_device_ctx(
        device_path: &str,
    ) -> Result<*mut ffi::AVBufferRef, ffmpeg::Error> {
        let device_cstr = CString::new(device_path).map_err(|_| ffmpeg::Error::InvalidData)?;
        let mut hw_device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
        let ret = ffi::av_hwdevice_ctx_create(
            &mut hw_device_ctx,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
            device_cstr.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }
        Ok(hw_device_ctx)
    }

    /// Create a VA-API hardware frames context.
    unsafe fn create_hw_frames_ctx(
        hw_device_ctx: *mut ffi::AVBufferRef,
        width: u32,
        height: u32,
        pool_size: i32,
    ) -> Result<*mut ffi::AVBufferRef, ffmpeg::Error> {
        let hw_frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
        if hw_frames_ref.is_null() {
            return Err(ffmpeg::Error::InvalidData);
        }

        let frames_ctx = (*hw_frames_ref).data as *mut ffi::AVHWFramesContext;
        (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
        (*frames_ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
        (*frames_ctx).width = width as i32;
        (*frames_ctx).height = height as i32;
        (*frames_ctx).initial_pool_size = pool_size;

        let ret = ffi::av_hwframe_ctx_init(hw_frames_ref);
        if ret < 0 {
            ffi::av_buffer_unref(&mut { hw_frames_ref });
            return Err(ffmpeg::Error::from(ret));
        }

        Ok(hw_frames_ref)
    }

    /// Upload a software NV12 frame to a VA-API hardware surface.
    unsafe fn upload_to_hw_surface(
        hw_frames_ctx: *mut ffi::AVBufferRef,
        sw_frame: &frame::Video,
    ) -> Result<frame::Video, ffmpeg::Error> {
        let mut hw_frame = frame::Video::empty();
        let hw_ptr = hw_frame.as_mut_ptr();

        let ret = ffi::av_hwframe_get_buffer(hw_frames_ctx, hw_ptr, 0);
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }

        let ret = ffi::av_hwframe_transfer_data(hw_ptr, sw_frame.as_ptr(), 0);
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }

        Ok(hw_frame)
    }

    // -----------------------------------------------------------------------
    // Encoder open paths
    // -----------------------------------------------------------------------

    /// Open VA-API encoder at the specified dimensions.
    fn try_open_vaapi(
        enc_codec: codec::codec::Codec,
        enc_w: u32,
        enc_h: u32,
    ) -> Result<Self, ffmpeg::Error> {
        unsafe {
            // 1. Create HW device context.
            let hw_device_ctx = Self::create_hw_device_ctx(VAAPI_DEVICE)?;

            // 2. Create HW frames context at the requested resolution.
            let hw_frames_ctx = match Self::create_hw_frames_ctx(hw_device_ctx, enc_w, enc_h, 10) {
                Ok(ctx) => ctx,
                Err(e) => {
                    ffi::av_buffer_unref(&mut { hw_device_ctx });
                    return Err(e);
                }
            };

            // 3. Set up the encoder context.
            let mut ctx = codec::context::Context::new_with_codec(enc_codec)
                .encoder()
                .video()?;

            ctx.set_width(enc_w);
            ctx.set_height(enc_h);
            ctx.set_time_base(Rational(1, 30));
            ctx.set_frame_rate(Some(Rational(30, 1)));
            ctx.set_format(Pixel::VAAPI);

            // 4. Assign hw_frames_ctx BEFORE opening the encoder.
            let ctx_ptr = ctx.as_mut_ptr();
            (*ctx_ptr).hw_frames_ctx = ffi::av_buffer_ref(hw_frames_ctx);
            if (*ctx_ptr).hw_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut { hw_frames_ctx });
                ffi::av_buffer_unref(&mut { hw_device_ctx });
                return Err(ffmpeg::Error::InvalidData);
            }

            let mut opts = ffmpeg::Dictionary::new();
            opts.set("g", "30");

            // 5. Open the encoder — this is where hw constraints are checked.
            let opened = match ctx.open_with(opts) {
                Ok(enc) => enc,
                Err(e) => {
                    // hw_frames_ctx ref on ctx_ptr is freed when ctx drops.
                    // We still own our refs.
                    ffi::av_buffer_unref(&mut { hw_frames_ctx });
                    ffi::av_buffer_unref(&mut { hw_device_ctx });
                    return Err(e);
                }
            };

            // Scaler: BGRA 32x32 -> NV12 32x32 (tile-sized).
            // We blit the tile-sized NV12 into the padded frame manually.
            let scaler = scaling::Context::get(
                Pixel::BGRA,
                TILE_W,
                TILE_H,
                Pixel::NV12,
                TILE_W,
                TILE_H,
                scaling::Flags::FAST_BILINEAR,
            )?;

            Ok(Self {
                encoder: opened,
                scaler,
                pts: 0,
                use_vaapi: true,
                enc_w,
                enc_h,
                _hw_device_ctx: Some(BufRef(hw_device_ctx)),
                hw_frames_ctx: Some(BufRef(hw_frames_ctx)),
            })
        }
    }

    /// Open libx264 software encoder at native tile resolution.
    fn try_open_sw(enc_codec: codec::codec::Codec) -> Result<Self, ffmpeg::Error> {
        let mut ctx = codec::context::Context::new_with_codec(enc_codec)
            .encoder()
            .video()?;

        ctx.set_width(TILE_W);
        ctx.set_height(TILE_H);
        ctx.set_time_base(Rational(1, 30));
        ctx.set_frame_rate(Some(Rational(30, 1)));
        ctx.set_format(Pixel::YUV420P);

        let mut opts = ffmpeg::Dictionary::new();
        opts.set("preset", "ultrafast");
        opts.set("tune", "zerolatency");
        opts.set("g", "30");

        let opened = ctx.open_with(opts)?;

        let scaler = scaling::Context::get(
            Pixel::BGRA,
            TILE_W,
            TILE_H,
            opened.format(),
            TILE_W,
            TILE_H,
            scaling::Flags::FAST_BILINEAR,
        )?;

        Ok(Self {
            encoder: opened,
            scaler,
            pts: 0,
            use_vaapi: false,
            enc_w: TILE_W,
            enc_h: TILE_H,
            _hw_device_ctx: None,
            hw_frames_ctx: None,
        })
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Encode a 32x32 BGRA tile (4096 bytes) into an H.264 NAL unit.
    ///
    /// Returns `Ok(None)` if the encoder buffers the frame without producing
    /// output yet (unlikely with zerolatency, but possible with VA-API).
    pub fn encode(&mut self, bgra_tile: &[u8]) -> Result<Option<EncodedTile>, ffmpeg::Error> {
        if bgra_tile.len() != (TILE_W * TILE_H * 4) as usize {
            return Err(ffmpeg::Error::InvalidData);
        }

        // Build a BGRA frame from the raw bytes.
        let mut bgra_frame = frame::Video::new(Pixel::BGRA, TILE_W, TILE_H);
        let stride = bgra_frame.stride(0);
        let plane = bgra_frame.data_mut(0);
        for y in 0..TILE_H as usize {
            let src_start = y * (TILE_W as usize) * 4;
            let dst_start = y * stride;
            plane[dst_start..dst_start + (TILE_W as usize) * 4]
                .copy_from_slice(&bgra_tile[src_start..src_start + (TILE_W as usize) * 4]);
        }

        if self.use_vaapi {
            // Convert BGRA 32x32 -> NV12 32x32
            let mut tile_nv12 = frame::Video::empty();
            self.scaler.run(&bgra_frame, &mut tile_nv12)?;

            // Build a padded NV12 frame at enc_w x enc_h.
            // Black in NV12: Y=16 (studio black), U=V=128 (neutral chroma).
            let mut nv12_frame = frame::Video::new(Pixel::NV12, self.enc_w, self.enc_h);

            // Fill Y plane with studio black
            let y_plane = nv12_frame.data_mut(0);
            for b in y_plane.iter_mut() {
                *b = 16;
            }
            // Fill UV plane with neutral chroma
            let uv_plane = nv12_frame.data_mut(1);
            for b in uv_plane.iter_mut() {
                *b = 128;
            }

            // Blit tile Y data into top-left corner.
            let y_stride = nv12_frame.stride(0);
            let tile_y_stride = tile_nv12.stride(0);
            let tile_y = tile_nv12.data(0);
            let y_plane = nv12_frame.data_mut(0);
            for row in 0..TILE_H as usize {
                let src = &tile_y[row * tile_y_stride..row * tile_y_stride + TILE_W as usize];
                let dst_off = row * y_stride;
                y_plane[dst_off..dst_off + TILE_W as usize].copy_from_slice(src);
            }

            // Blit tile UV data into top-left corner.
            // NV12 chroma is half-res and interleaved (U,V pairs).
            let uv_stride = nv12_frame.stride(1);
            let tile_uv_stride = tile_nv12.stride(1);
            let tile_uv = tile_nv12.data(1);
            let uv_plane = nv12_frame.data_mut(1);
            let chroma_w = TILE_W as usize; // interleaved U,V = width bytes
            let chroma_h = (TILE_H / 2) as usize;
            for row in 0..chroma_h {
                let src = &tile_uv[row * tile_uv_stride..row * tile_uv_stride + chroma_w];
                let dst_off = row * uv_stride;
                uv_plane[dst_off..dst_off + chroma_w].copy_from_slice(src);
            }

            nv12_frame.set_pts(Some(self.pts));
            self.pts += 1;

            // Upload to hardware surface and encode.
            let hw_frames_ref = self
                .hw_frames_ctx
                .as_ref()
                .expect("use_vaapi but no hw_frames_ctx");
            let mut hw_frame = unsafe { Self::upload_to_hw_surface(hw_frames_ref.0, &nv12_frame)? };
            hw_frame.set_pts(nv12_frame.pts());
            self.encoder.send_frame(&hw_frame)?;
        } else {
            // Software path: scale BGRA -> YUV420P at native tile size.
            let mut sw_frame = frame::Video::empty();
            self.scaler.run(&bgra_frame, &mut sw_frame)?;
            sw_frame.set_pts(Some(self.pts));
            self.pts += 1;
            self.encoder.send_frame(&sw_frame)?;
        }

        self.receive_packet()
    }

    fn receive_packet(&mut self) -> Result<Option<EncodedTile>, ffmpeg::Error> {
        let mut packet = Packet::empty();
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or(&[]).to_vec();
                Ok(Some(EncodedTile {
                    codec: GfCodec::H264,
                    payload: data,
                }))
            }
            Err(ffmpeg::Error::Other {
                errno: libc::EAGAIN,
            }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// FullFrameEncoder — full-resolution H.264 encoder with DMA-BUF zero-copy
// ---------------------------------------------------------------------------

use std::os::unix::io::RawFd;

/// The result of encoding a full video frame.
pub struct FullFrameEncoded {
    pub payload: Vec<u8>,
    pub is_keyframe: bool,
}

/// I-frame interval: one IDR + 10 P-frames = ~6 keyframes/sec at 60 fps.
const FULL_FRAME_GOP: u32 = 11;

/// Full-frame H.264 encoder.
///
/// Encodes at the native capture resolution using VA-API with NV12 HOST_VISIBLE
/// buffer upload (from the GPU compute shader) or CPU software fallback.
pub struct FullFrameEncoder {
    encoder: encoder::Video,
    /// A scaler is only needed on the software path (BGRA → YUV420P).
    scaler: Option<scaling::Context>,
    pts: i64,
    use_vaapi: bool,
    enc_w: u32,
    enc_h: u32,
    _hw_device_ctx: Option<BufRef>,
    hw_frames_ctx: Option<BufRef>,
    /// DRM device context for DMA-BUF zero-copy import.
    _drm_device_ctx: Option<BufRef>,
    /// One-shot keyframe request: set by `request_keyframe()`, cleared on next
    /// `encode_nv12_buffer` / `encode_frame` call.
    keyframe_pending: bool,
}

// SAFETY: same reasoning as H264VaapiEncoder.
unsafe impl Send for FullFrameEncoder {}

impl FullFrameEncoder {
    /// Create a full-frame encoder, preferring VA-API, falling back to libx264.
    pub fn new(width: u32, height: u32) -> Result<Self, ffmpeg::Error> {
        FFMPEG_INIT.call_once(|| {
            ffmpeg::init().expect("ffmpeg::init() failed");
        });

        if let Some(vaapi_codec) = encoder::find_by_name("h264_vaapi") {
            match Self::try_open_vaapi(vaapi_codec, width, height) {
                Ok(this) => {
                    info!(
                        enc_w = this.enc_w,
                        enc_h = this.enc_h,
                        "FullFrameEncoder: using h264_vaapi hardware encoder"
                    );
                    return Ok(this);
                }
                Err(e) => {
                    warn!(
                        err = %e,
                        "FullFrameEncoder: VA-API open failed, falling back to libx264"
                    );
                }
            }
        }

        let x264_codec = encoder::find_by_name("libx264").ok_or(ffmpeg::Error::EncoderNotFound)?;
        info!("FullFrameEncoder: using libx264 software encoder");
        Self::try_open_sw(x264_codec, width, height)
    }

    pub fn width(&self) -> u32 {
        self.enc_w
    }

    pub fn height(&self) -> u32 {
        self.enc_h
    }

    /// Request that the next encoded frame be an IDR (instantaneous decoder
    /// refresh) regardless of GOP timing. The flag is one-shot: it is
    /// consumed at the top of the next `encode_nv12_buffer` or `encode_frame`
    /// call (cleared atomically with the `force_idr` decision).
    ///
    /// If that encode call returns `Err`, the request is dropped silently —
    /// callers must call `request_keyframe()` again on retry.
    ///
    /// Used by the M3.0 mode-switch logic so a `TileCodec → H264` transition
    /// gives the client a fresh decoding anchor.
    pub fn request_keyframe(&mut self) {
        self.keyframe_pending = true;
    }

    /// Test-only: peek at the pending-keyframe flag so reconnect coverage can
    /// verify `request_keyframe()` was called without driving a real encode.
    #[cfg(test)]
    pub fn keyframe_pending(&self) -> bool {
        self.keyframe_pending
    }

    /// Create a DRM device context for the given render node.
    /// Used to derive a VAAPI context that supports DRM PRIME frame import.
    unsafe fn create_drm_device_ctx(
        device_path: &str,
    ) -> Result<*mut ffi::AVBufferRef, ffmpeg::Error> {
        let device_cstr = CString::new(device_path).map_err(|_| ffmpeg::Error::InvalidData)?;
        let mut drm_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
        let ret = ffi::av_hwdevice_ctx_create(
            &mut drm_ctx,
            ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
            device_cstr.as_ptr(),
            ptr::null_mut(),
            0,
        );
        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }
        Ok(drm_ctx)
    }

    // -----------------------------------------------------------------------
    // Encoder open paths
    // -----------------------------------------------------------------------

    fn try_open_vaapi(
        enc_codec: codec::codec::Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, ffmpeg::Error> {
        unsafe {
            // Create DRM device first, then derive VAAPI from it.
            // This enables DRM PRIME → VAAPI frame mapping for zero-copy DMA-BUF import.
            let drm_device_ctx = Self::create_drm_device_ctx(VAAPI_DEVICE);
            let hw_device_ctx = if let Ok(drm_ctx) = drm_device_ctx.as_ref() {
                // Derive VAAPI from DRM — enables DRM PRIME frame import
                let mut vaapi_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
                let ret = ffi::av_hwdevice_ctx_create_derived(
                    &mut vaapi_ctx,
                    ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                    *drm_ctx,
                    0,
                );
                if ret < 0 || vaapi_ctx.is_null() {
                    // Fall back to direct VAAPI device
                    H264VaapiEncoder::create_hw_device_ctx(VAAPI_DEVICE)?
                } else {
                    vaapi_ctx
                }
            } else {
                H264VaapiEncoder::create_hw_device_ctx(VAAPI_DEVICE)?
            };

            // NV12 hw_frames_ctx for encoder output
            let hw_frames_ctx =
                match H264VaapiEncoder::create_hw_frames_ctx(hw_device_ctx, width, height, 10) {
                    Ok(ctx) => ctx,
                    Err(e) => {
                        ffi::av_buffer_unref(&mut { hw_device_ctx });
                        return Err(e);
                    }
                };

            let mut ctx = codec::context::Context::new_with_codec(enc_codec)
                .encoder()
                .video()?;

            ctx.set_width(width);
            ctx.set_height(height);
            ctx.set_time_base(Rational(1, 60));
            ctx.set_frame_rate(Some(Rational(60, 1)));
            ctx.set_format(Pixel::VAAPI);

            let ctx_ptr = ctx.as_mut_ptr();
            (*ctx_ptr).hw_frames_ctx = ffi::av_buffer_ref(hw_frames_ctx);
            if (*ctx_ptr).hw_frames_ctx.is_null() {
                ffi::av_buffer_unref(&mut { hw_frames_ctx });
                ffi::av_buffer_unref(&mut { hw_device_ctx });
                return Err(ffmpeg::Error::InvalidData);
            }

            let mut opts = ffmpeg::Dictionary::new();
            opts.set("g", &FULL_FRAME_GOP.to_string());

            let opened = match ctx.open_with(opts) {
                Ok(enc) => enc,
                Err(e) => {
                    ffi::av_buffer_unref(&mut { hw_frames_ctx });
                    ffi::av_buffer_unref(&mut { hw_device_ctx });
                    return Err(e);
                }
            };

            Ok(Self {
                encoder: opened,
                scaler: None,
                pts: 0,
                use_vaapi: true,
                enc_w: width,
                enc_h: height,
                _hw_device_ctx: Some(BufRef(hw_device_ctx)),
                hw_frames_ctx: Some(BufRef(hw_frames_ctx)),
                _drm_device_ctx: drm_device_ctx.ok().map(BufRef),
                keyframe_pending: false,
            })
        }
    }

    fn try_open_sw(
        enc_codec: codec::codec::Codec,
        width: u32,
        height: u32,
    ) -> Result<Self, ffmpeg::Error> {
        let mut ctx = codec::context::Context::new_with_codec(enc_codec)
            .encoder()
            .video()?;

        ctx.set_width(width);
        ctx.set_height(height);
        ctx.set_time_base(Rational(1, 60));
        ctx.set_frame_rate(Some(Rational(60, 1)));
        ctx.set_format(Pixel::YUV420P);

        let mut opts = ffmpeg::Dictionary::new();
        opts.set("preset", "ultrafast");
        opts.set("tune", "zerolatency");
        opts.set("g", &FULL_FRAME_GOP.to_string());

        let opened = ctx.open_with(opts)?;

        // Note: we don't preinitialize the scaler here. `encode_frame`
        // (BGRA input) and `encode_nv12_buffer` (NV12 input) each lazily
        // create a scaler with the correct *input* pixel format on first
        // use. Pre-creating a BGRA scaler would cause `encode_nv12_buffer`
        // to receive `Error::InputChanged` from `scaler.run` because the
        // input format wouldn't match (NV12 vs BGRA).

        Ok(Self {
            encoder: opened,
            scaler: None,
            pts: 0,
            use_vaapi: false,
            enc_w: width,
            enc_h: height,
            _hw_device_ctx: None,
            hw_frames_ctx: None,
            _drm_device_ctx: None,
            keyframe_pending: false,
        })
    }

    // -----------------------------------------------------------------------
    // Public API
    // -----------------------------------------------------------------------

    /// Encode a full frame supplied as a DMA-BUF file descriptor.
    ///
    /// `fd` is a DMA-BUF (or memfd) containing `height * stride` bytes of
    /// BGRA pixel data.  On the VA-API path we attempt a zero-copy
    /// `av_hwframe_map()` and fall back to `mmap` + `av_hwframe_transfer_data`
    /// if the driver doesn't support it.  On the software path we always mmap.
    ///
    /// Returns `Ok(None)` when the encoder buffered the frame without emitting
    /// output yet.
    pub fn encode_frame(
        &mut self,
        fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        let pts = self.pts;
        self.pts += 1;

        let force_idr = pts % FULL_FRAME_GOP as i64 == 0 || self.keyframe_pending;
        self.keyframe_pending = false;

        if self.use_vaapi {
            self.encode_vaapi(fd, width, height, stride, pts, force_idr)
        } else {
            self.encode_sw(fd, width, height, stride, pts, force_idr)
        }
    }

    /// Encode a full frame from a software NV12 buffer produced by the GPU compute shader.
    ///
    /// `nv12_data` points to a HOST_VISIBLE GPU buffer: Y plane at offset 0,
    /// UV plane at `uv_offset`. The buffer is borrowed — no ownership transfer occurs.
    ///
    /// On the VA-API path the NV12 data is uploaded to a VAAPI surface via
    /// `av_hwframe_transfer_data`. On the software path a CPU YUV420P scaler is
    /// used instead (for machines without VA-API).
    ///
    /// Returns `Ok(None)` if the encoder buffered the frame without emitting output yet.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn encode_nv12_buffer(
        &mut self,
        nv12_data: *const u8,
        width: u32,
        height: u32,
        y_stride: u32,
        uv_stride: u32,
        uv_offset: u32,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        let pts = self.pts;
        self.pts += 1;
        let force_idr = pts % FULL_FRAME_GOP as i64 == 0 || self.keyframe_pending;
        self.keyframe_pending = false;

        unsafe {
            // Build a software NV12 AVFrame whose data pointers point into nv12_data.
            // We do NOT own this memory — set buf[0] to null so ffmpeg won't free it.
            let mut sw_frame = frame::Video::empty();
            let sw_ptr = sw_frame.as_mut_ptr();
            (*sw_ptr).format = ffi::AVPixelFormat::AV_PIX_FMT_NV12 as i32;
            (*sw_ptr).width = width as i32;
            (*sw_ptr).height = height as i32;
            // Y plane
            (*sw_ptr).data[0] = nv12_data as *mut u8;
            (*sw_ptr).linesize[0] = y_stride as i32;
            // UV plane (interleaved)
            (*sw_ptr).data[1] = nv12_data.add(uv_offset as usize) as *mut u8;
            (*sw_ptr).linesize[1] = uv_stride as i32;

            sw_frame.set_pts(Some(pts));

            if self.use_vaapi {
                // Upload software NV12 → VAAPI hardware surface.
                let hw_frames_ref = self
                    .hw_frames_ctx
                    .as_ref()
                    .expect("use_vaapi but no hw_frames_ctx");

                let mut hw_frame =
                    H264VaapiEncoder::upload_to_hw_surface(hw_frames_ref.0, &sw_frame)?;
                hw_frame.set_pts(Some(pts));

                if force_idr {
                    (*hw_frame.as_mut_ptr()).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                    (*hw_frame.as_mut_ptr()).flags |= ffi::AV_FRAME_FLAG_KEY;
                }

                self.encoder.send_frame(&hw_frame)?;
            } else {
                // Software fallback: convert NV12 → YUV420P via swscale.
                let scaler = self.scaler.get_or_insert_with(|| {
                    scaling::Context::get(
                        Pixel::NV12,
                        width,
                        height,
                        Pixel::YUV420P,
                        width,
                        height,
                        scaling::Flags::FAST_BILINEAR,
                    )
                    .expect("swscale NV12→YUV420P context failed")
                });

                let mut yuv_frame = frame::Video::empty();
                scaler.run(&sw_frame, &mut yuv_frame)?;
                yuv_frame.set_pts(Some(pts));

                if force_idr {
                    (*yuv_frame.as_mut_ptr()).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                    (*yuv_frame.as_mut_ptr()).flags |= ffi::AV_FRAME_FLAG_KEY;
                }

                self.encoder.send_frame(&yuv_frame)?;
            }
        }

        self.receive_full_packet()
    }

    // -----------------------------------------------------------------------
    // VA-API encode path (encode_frame / encode_vaapi / mmap_scale_upload)
    // -----------------------------------------------------------------------

    fn encode_vaapi(
        &mut self,
        fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        pts: i64,
        force_idr: bool,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        unsafe {
            let hw_frames_ref = self
                .hw_frames_ctx
                .as_ref()
                .expect("use_vaapi but no hw_frames_ctx");

            // Try zero-copy DMA-BUF import via DRM PRIME → av_hwframe_map().
            // This avoids any CPU pixel access — the DMA-BUF stays on the GPU.
            let hw_frame = match Self::try_drm_prime_import(
                hw_frames_ref.0,
                fd,
                width,
                height,
                stride,
                pts,
                force_idr,
            ) {
                Ok(frame) => frame,
                Err(_) => {
                    // Fall back to mmap + CPU scale + upload for non-GPU-local
                    // buffers (e.g. memfds used in tests).
                    // NOTE: This fallback will fail for real GPU DMA-BUFs
                    // (device VRAM can't be mmap'd). A VPP BGRA→NV12 pipeline
                    // is needed for true zero-copy encode — tracked as a gap.
                    Self::mmap_scale_upload(
                        hw_frames_ref.0,
                        fd,
                        width,
                        height,
                        stride,
                        self.enc_w,
                        self.enc_h,
                        pts,
                        force_idr,
                    )?
                }
            };

            self.encoder.send_frame(&hw_frame)?;
        }

        self.receive_full_packet()
    }

    /// Zero-copy DMA-BUF import: build a DRM PRIME frame and map it into a
    /// VA-API surface. The VAAPI device must be derived from a DRM device
    /// (set up in `try_open_vaapi`) for this mapping to work.
    ///
    /// The VA-API driver imports the DMA-BUF directly — no CPU pixel access.
    /// Color conversion (XRGB8888→NV12) happens on the GPU via VPP.
    unsafe fn try_drm_prime_import(
        hw_frames_ctx: *mut ffi::AVBufferRef,
        fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        pts: i64,
        force_idr: bool,
    ) -> Result<frame::Video, ffmpeg::Error> {
        // DRM_FORMAT_XRGB8888 = fourcc('X','R','2','4')
        const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

        let buf_size = (height as usize) * (stride as usize);

        // Build the DRM frame descriptor. Box keeps it stable in memory.
        let mut desc = Box::new(std::mem::zeroed::<ffi::AVDRMFrameDescriptor>());
        desc.nb_objects = 1;
        desc.objects[0] = ffi::AVDRMObjectDescriptor {
            fd: libc::dup(fd),
            size: buf_size,
            format_modifier: DRM_FORMAT_MOD_INVALID,
        };
        desc.nb_layers = 1;
        desc.layers[0].format = DRM_FORMAT_XRGB8888;
        desc.layers[0].nb_planes = 1;
        desc.layers[0].planes[0] = ffi::AVDRMPlaneDescriptor {
            object_index: 0,
            offset: 0,
            pitch: stride as isize,
        };

        // Create DRM PRIME frame wrapping the descriptor.
        let mut drm_frame = frame::Video::empty();
        let drm_ptr = drm_frame.as_mut_ptr();
        (*drm_ptr).format = ffi::AVPixelFormat::AV_PIX_FMT_DRM_PRIME as i32;
        (*drm_ptr).width = width as i32;
        (*drm_ptr).height = height as i32;
        (*drm_ptr).data[0] = desc.as_mut() as *mut ffi::AVDRMFrameDescriptor as *mut u8;

        // Allocate a VAAPI hardware frame.
        let mut hw_frame = frame::Video::empty();
        let hw_ptr = hw_frame.as_mut_ptr();
        let ret = ffi::av_hwframe_get_buffer(hw_frames_ctx, hw_ptr, 0);
        if ret < 0 {
            libc::close(desc.objects[0].fd);
            return Err(ffmpeg::Error::from(ret));
        }

        // Map the DRM PRIME frame into the VAAPI surface.
        // This works because the VAAPI device was derived from the DRM device,
        // so ffmpeg knows how to cross-map between DRM and VAAPI.
        let ret = ffi::av_hwframe_map(hw_ptr, drm_ptr, ffi::AV_HWFRAME_MAP_READ as i32);

        // Clean up dup'd fd and desc pointer
        libc::close(desc.objects[0].fd);
        (*drm_ptr).data[0] = ptr::null_mut();

        if ret < 0 {
            return Err(ffmpeg::Error::from(ret));
        }

        hw_frame.set_pts(Some(pts));
        if force_idr {
            (*hw_ptr).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
            (*hw_ptr).flags |= ffi::AV_FRAME_FLAG_KEY;
        }

        Ok(hw_frame)
    }

    /// Fallback: mmap the fd, scale BGRA→NV12 on CPU, upload to VA-API surface.
    #[allow(clippy::too_many_arguments)]
    unsafe fn mmap_scale_upload(
        hw_frames_ctx: *mut ffi::AVBufferRef,
        fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        enc_w: u32,
        enc_h: u32,
        pts: i64,
        force_idr: bool,
    ) -> Result<frame::Video, ffmpeg::Error> {
        let buf_size = (height * stride) as usize;
        let ptr = libc::mmap(
            ptr::null_mut(),
            buf_size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        );
        if ptr == libc::MAP_FAILED {
            return Err(ffmpeg::Error::InvalidData);
        }

        let mut scaler = scaling::Context::get(
            Pixel::BGRA,
            width,
            height,
            Pixel::NV12,
            enc_w,
            enc_h,
            scaling::Flags::FAST_BILINEAR,
        )?;

        let mut bgra_frame = frame::Video::new(Pixel::BGRA, width, height);
        {
            let frame_stride = bgra_frame.stride(0);
            let plane = bgra_frame.data_mut(0);
            let src_bytes = std::slice::from_raw_parts(ptr as *const u8, buf_size);
            for y in 0..height as usize {
                let src_row =
                    &src_bytes[y * stride as usize..y * stride as usize + width as usize * 4];
                let dst_off = y * frame_stride;
                plane[dst_off..dst_off + width as usize * 4].copy_from_slice(src_row);
            }
        }

        let mut nv12_frame = frame::Video::empty();
        let scale_result = scaler.run(&bgra_frame, &mut nv12_frame);
        libc::munmap(ptr, buf_size);
        scale_result?;

        let mut hw = H264VaapiEncoder::upload_to_hw_surface(hw_frames_ctx, &nv12_frame)?;
        hw.set_pts(Some(pts));
        if force_idr {
            (*hw.as_mut_ptr()).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
            (*hw.as_mut_ptr()).flags |= ffi::AV_FRAME_FLAG_KEY;
        }

        Ok(hw)
    }

    // -----------------------------------------------------------------------
    // Software encode path
    // -----------------------------------------------------------------------

    fn encode_sw(
        &mut self,
        fd: RawFd,
        width: u32,
        height: u32,
        stride: u32,
        pts: i64,
        force_idr: bool,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        let buf_size = (height * stride) as usize;

        unsafe {
            let ptr = libc::mmap(
                ptr::null_mut(),
                buf_size,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd,
                0,
            );
            if ptr == libc::MAP_FAILED {
                return Err(ffmpeg::Error::InvalidData);
            }

            // Build BGRA frame from mmap.
            let mut bgra_frame = frame::Video::new(Pixel::BGRA, width, height);
            {
                let frame_stride = bgra_frame.stride(0);
                let plane = bgra_frame.data_mut(0);
                let src_bytes = std::slice::from_raw_parts(ptr as *const u8, buf_size);
                for y in 0..height as usize {
                    let src_row =
                        &src_bytes[y * stride as usize..y * stride as usize + width as usize * 4];
                    let dst_off = y * frame_stride;
                    plane[dst_off..dst_off + width as usize * 4].copy_from_slice(src_row);
                }
            }

            libc::munmap(ptr, buf_size);

            // Scale BGRA → YUV420P. The scaler is lazily created on first
            // use so it always matches the actual input pixel format
            // (BGRA here vs NV12 in `encode_nv12_buffer`).
            let scaler = self.scaler.get_or_insert_with(|| {
                scaling::Context::get(
                    Pixel::BGRA,
                    width,
                    height,
                    Pixel::YUV420P,
                    width,
                    height,
                    scaling::Flags::FAST_BILINEAR,
                )
                .expect("swscale BGRA→YUV420P context failed")
            });
            let mut yuv_frame = frame::Video::empty();
            scaler.run(&bgra_frame, &mut yuv_frame)?;
            yuv_frame.set_pts(Some(pts));

            if force_idr {
                (*yuv_frame.as_mut_ptr()).pict_type = ffi::AVPictureType::AV_PICTURE_TYPE_I;
                (*yuv_frame.as_mut_ptr()).flags |= ffi::AV_FRAME_FLAG_KEY;
            }

            self.encoder.send_frame(&yuv_frame)?;
        }

        self.receive_full_packet()
    }

    // -----------------------------------------------------------------------
    // Packet receive
    // -----------------------------------------------------------------------

    fn receive_full_packet(&mut self) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        let mut packet = Packet::empty();
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or(&[]).to_vec();
                let is_keyframe = is_keyframe_nal(&data);
                Ok(Some(FullFrameEncoded {
                    payload: data,
                    is_keyframe,
                }))
            }
            Err(ffmpeg::Error::Other {
                errno: libc::EAGAIN,
            }) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Detect whether an H.264 Annex-B bitstream contains a keyframe NAL unit.
///
/// Looks for NAL type 5 (IDR slice) or NAL type 7 (SPS, which always
/// precedes an IDR in the same AU).
fn is_keyframe_nal(data: &[u8]) -> bool {
    let mut i = 0;
    while i + 4 <= data.len() {
        // Annex-B start code: 0x00 0x00 0x00 0x01 or 0x00 0x00 0x01
        if data[i] == 0 && data[i + 1] == 0 {
            let nal_byte_offset = if data[i + 2] == 0 && i + 4 < data.len() && data[i + 3] == 1 {
                i + 4
            } else if data[i + 2] == 1 {
                i + 3
            } else {
                i += 1;
                continue;
            };
            if nal_byte_offset < data.len() {
                let nal_type = data[nal_byte_offset] & 0x1F;
                if nal_type == 5 || nal_type == 7 {
                    return true;
                }
            }
            i = nal_byte_offset;
        } else {
            i += 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_solid_red_tile() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut encoder = match H264VaapiEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping H264 test (no encoder available): {e}");
                return;
            }
        };

        eprintln!(
            "encoder backend: {} ({}x{})",
            if encoder.use_vaapi {
                "h264_vaapi"
            } else {
                "libx264"
            },
            encoder.enc_w,
            encoder.enc_h,
        );

        let mut tile = vec![0u8; 32 * 32 * 4];
        for pixel in tile.chunks_exact_mut(4) {
            pixel[0] = 0; // B
            pixel[1] = 0; // G
            pixel[2] = 255; // R
            pixel[3] = 255; // A
        }

        let result = encoder.encode(&tile).unwrap();
        // VA-API may buffer the first frame (no zerolatency equivalent),
        // so accept None on VA-API and try a second frame.
        if encoder.use_vaapi && result.is_none() {
            eprintln!("VA-API buffered first frame, sending second...");
            let result2 = encoder.encode(&tile).unwrap();
            assert!(
                result2.is_some(),
                "VA-API should produce output after two frames"
            );
            let encoded = result2.unwrap();
            assert_eq!(encoded.codec, GfCodec::H264);
            assert!(!encoded.payload.is_empty());
        } else {
            assert!(
                result.is_some(),
                "first frame should produce output with zerolatency"
            );
            let encoded = result.unwrap();
            assert_eq!(encoded.codec, GfCodec::H264);
            assert!(!encoded.payload.is_empty());
            // Padded frames will be larger, relax the size check for VA-API.
            if !encoder.use_vaapi {
                assert!(encoded.payload.len() < 4096);
            }
        }
    }

    #[test]
    fn encode_multiple_frames_produces_smaller_p_frames() {
        let _ = tracing_subscriber::fmt::try_init();

        let mut encoder = match H264VaapiEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping H264 test (no encoder available): {e}");
                return;
            }
        };

        eprintln!(
            "encoder backend: {} ({}x{})",
            if encoder.use_vaapi {
                "h264_vaapi"
            } else {
                "libx264"
            },
            encoder.enc_w,
            encoder.enc_h,
        );

        let mut tile = vec![0u8; 32 * 32 * 4];
        for pixel in tile.chunks_exact_mut(4) {
            pixel[2] = 255;
            pixel[3] = 255;
        }

        // For VA-API, we may need to drain a few frames before we get output.
        let mut outputs: Vec<EncodedTile> = Vec::new();
        for _ in 0..4 {
            if let Some(encoded) = encoder.encode(&tile).unwrap() {
                outputs.push(encoded);
            }
            if outputs.len() >= 2 {
                break;
            }
        }

        assert!(
            outputs.len() >= 2,
            "expected at least 2 output packets, got {}",
            outputs.len()
        );

        let first = &outputs[0];
        let second = &outputs[1];
        assert!(
            second.payload.len() <= first.payload.len(),
            "P-frame of static content should be <= I-frame: {} vs {}",
            second.payload.len(),
            first.payload.len()
        );
    }

    // -----------------------------------------------------------------------
    // FullFrameEncoder tests
    // -----------------------------------------------------------------------

    /// Create a memfd of the given size, fill it with `fill_fn`, and return
    /// the raw file descriptor.  The caller is responsible for closing it.
    fn make_bgra_memfd(width: u32, height: u32, fill_fn: impl Fn(usize) -> [u8; 4]) -> RawFd {
        use std::ffi::CString;
        let name = CString::new("test_frame").unwrap();
        let fd = unsafe { libc::syscall(libc::SYS_memfd_create, name.as_ptr(), 0) as RawFd };
        assert!(fd >= 0, "memfd_create failed");

        let stride = width * 4;
        let total = (height * stride) as usize;
        let mut buf = vec![0u8; total];
        for (i, pixel) in buf.chunks_exact_mut(4).enumerate() {
            let rgba = fill_fn(i);
            pixel.copy_from_slice(&rgba);
        }

        // Write the buffer to the memfd.
        let written = unsafe { libc::write(fd, buf.as_ptr() as *const libc::c_void, total) };
        assert_eq!(written as usize, total, "write to memfd failed");
        // Seek back to beginning so mmap can read from offset 0.
        unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };

        fd
    }

    #[test]
    fn full_frame_encode_from_memfd() {
        let _ = tracing_subscriber::fmt::try_init();

        let width: u32 = 640;
        let height: u32 = 480;

        let mut encoder = match FullFrameEncoder::new(width, height) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping full_frame_encode test (no encoder): {e}");
                return;
            }
        };

        eprintln!(
            "FullFrameEncoder backend: {} ({}x{})",
            if encoder.use_vaapi {
                "h264_vaapi"
            } else {
                "libx264"
            },
            encoder.enc_w,
            encoder.enc_h,
        );

        assert_eq!(encoder.width(), width);
        assert_eq!(encoder.height(), height);

        // Solid red BGRA pixels.
        let fd = make_bgra_memfd(width, height, |_| [0, 0, 255, 255]);

        let stride = width * 4;

        // Encode twice — first should be a keyframe (pts == 0).
        // VA-API may buffer the first; try up to three frames.
        let mut got_output = false;
        let mut got_keyframe = false;
        for _ in 0..3 {
            // Re-seek before each encode call (mmap reads from beginning).
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
            match encoder.encode_frame(fd, width, height, stride) {
                Ok(Some(encoded)) => {
                    assert!(
                        !encoded.payload.is_empty(),
                        "encoded payload must not be empty"
                    );
                    if encoded.is_keyframe {
                        got_keyframe = true;
                    }
                    got_output = true;
                    break;
                }
                Ok(None) => continue,
                Err(e) => panic!("encode_frame failed: {e}"),
            }
        }

        unsafe { libc::close(fd) };

        assert!(
            got_output,
            "encoder should have produced output within 3 frames"
        );
        assert!(got_keyframe, "first output should be a keyframe");
    }

    #[test]
    fn full_frame_keyframe_interval() {
        let _ = tracing_subscriber::fmt::try_init();

        let width: u32 = 128;
        let height: u32 = 128;

        let mut encoder = match FullFrameEncoder::new(width, height) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping full_frame_keyframe_interval test (no encoder): {e}");
                return;
            }
        };

        let fd = make_bgra_memfd(width, height, |_| [0, 255, 0, 255]); // solid green
        let stride = width * 4;

        let mut keyframe_pts: Vec<i64> = Vec::new();
        let mut output_idx: i64 = 0;

        // Encode 14 frames (more than one full GOP of 11).
        for _ in 0..14 {
            unsafe { libc::lseek(fd, 0, libc::SEEK_SET) };
            match encoder.encode_frame(fd, width, height, stride) {
                Ok(Some(encoded)) => {
                    assert!(!encoded.payload.is_empty());
                    if encoded.is_keyframe {
                        keyframe_pts.push(output_idx);
                    }
                    output_idx += 1;
                }
                Ok(None) => {}
                Err(e) => panic!("encode_frame failed: {e}"),
            }
        }

        unsafe { libc::close(fd) };

        eprintln!("keyframe output indices: {keyframe_pts:?}");

        // We must have gotten at least one keyframe (the very first frame).
        assert!(
            !keyframe_pts.is_empty(),
            "expected at least one keyframe in 14 frames"
        );

        // If we got at least two keyframes, check that the interval is ≈ GOP size.
        if keyframe_pts.len() >= 2 {
            let interval = keyframe_pts[1] - keyframe_pts[0];
            assert!(
                interval >= 9 && interval <= 13,
                "keyframe interval should be ~11 frames, got {interval}"
            );
        }
    }

    #[test]
    fn request_keyframe_forces_idr_outside_gop_boundary() {
        let width = 320;
        let height = 240;
        let mut encoder = match FullFrameEncoder::new(width, height) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping request_keyframe test (no encoder): {e}");
                return;
            }
        };
        // PTS 0 is automatically a keyframe; consume it.
        let nv12_size = (width * height * 3 / 2) as usize;
        let nv12 = vec![128u8; nv12_size];
        let _ =
            encoder.encode_nv12_buffer(nv12.as_ptr(), width, height, width, width, width * height);
        // PTS 1 normally would NOT be a keyframe (FULL_FRAME_GOP = 11). Request one,
        // then drain frames until a packet emerges — the encoder may buffer the first
        // few PTS values before emitting the IDR. The first emitted packet must be a
        // keyframe; if no packet appears within a reasonable window the test fails.
        encoder.request_keyframe();
        let mut keyframe_observed = false;
        let mut keyframe_pts = None;
        for pts in 1..=4 {
            if let Ok(Some(out)) = encoder.encode_nv12_buffer(
                nv12.as_ptr(),
                width,
                height,
                width,
                width,
                width * height,
            ) {
                assert!(out.is_keyframe,
                    "first packet after request_keyframe() must be IDR (got P-frame at drain pts={pts})");
                keyframe_observed = true;
                keyframe_pts = Some(pts);
                break;
            }
        }
        assert!(
            keyframe_observed,
            "encoder buffered all PTS 1-4 frames; no IDR ever emitted"
        );

        // Subsequent encodes (continuing PTS sequence after the keyframe) should NOT
        // all be keyframes — the latch was one-shot. Drain another 5 frames; at least
        // one must be a P-frame. Stop well before PTS 11 (next natural GOP boundary).
        let start_pts = keyframe_pts.unwrap() + 1;
        let mut saw_p_frame = false;
        for _pts in start_pts..(start_pts + 5).min(10) {
            if let Ok(Some(out)) = encoder.encode_nv12_buffer(
                nv12.as_ptr(),
                width,
                height,
                width,
                width,
                width * height,
            ) {
                if !out.is_keyframe {
                    saw_p_frame = true;
                    break;
                }
            }
        }
        assert!(saw_p_frame, "latch must not persist beyond one frame");
    }
}
