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

use super::nal_parser::is_keyframe_nal;
use super::vaapi_device::{self, BufRef, VAAPI_DEVICE};
use super::EncodedTile;
use crate::transport::protocol::Codec as GfCodec;

const TILE_W: u32 = 32;
const TILE_H: u32 = 32;

/// Candidate encoding resolutions to try when the tile size is below the
/// hardware minimum. Sorted ascending; the first one that succeeds wins.
/// Mmap a BGRA framebuffer from a file descriptor and copy it into an owned
/// `frame::Video`. The mmap is released before this function returns; the
/// returned frame owns its own buffer.
///
/// Shared by the VAAPI mmap fallback path (`mmap_scale_upload`) and the
/// software encode path (`encode_sw`) — both of these need to lift BGRA pixels
/// off a DMA-BUF / memfd into an FFmpeg frame before handing the frame to a
/// scaler with a stage-specific destination format (NV12 for VAAPI, YUV420P
/// for libx264). A stride mismatch between source and destination at the
/// copy loop has historically been the most subtle bug class here; centralising
/// the copy keeps both call sites in lockstep.
///
/// # Safety
///
/// Caller must ensure `fd` is a valid file descriptor backing at least
/// `height * stride` bytes of readable memory.
unsafe fn mmap_bgra_frame(
    fd: RawFd,
    width: u32,
    height: u32,
    stride: u32,
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

    let mut bgra_frame = frame::Video::new(Pixel::BGRA, width, height);
    {
        let frame_stride = bgra_frame.stride(0);
        let plane = bgra_frame.data_mut(0);
        let src_bytes = std::slice::from_raw_parts(ptr as *const u8, buf_size);
        for y in 0..height as usize {
            let src_row = &src_bytes[y * stride as usize..y * stride as usize + width as usize * 4];
            let dst_off = y * frame_stride;
            plane[dst_off..dst_off + width as usize * 4].copy_from_slice(src_row);
        }
    }

    libc::munmap(ptr, buf_size);
    Ok(bgra_frame)
}

const VAAPI_CANDIDATE_SIZES: &[(u32, u32)] = &[
    (TILE_W, TILE_H), // try native size first
    (128, 128),       // common AMD minimum
    (256, 256),       // fallback
];

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
            let hw_device_ctx = vaapi_device::create_hw_device_ctx(VAAPI_DEVICE)?;

            // 2. Create HW frames context at the requested resolution.
            let hw_frames_ctx =
                match vaapi_device::create_hw_frames_ctx(hw_device_ctx, enc_w, enc_h, 10) {
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
            let mut hw_frame =
                unsafe { vaapi_device::upload_to_hw_surface(hw_frames_ref.0, &nv12_frame)? };
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

/// Inputs that describe one DMA-BUF-backed frame to be encoded.
///
/// Threaded through `encode_vaapi` → `try_drm_prime_import` / `mmap_scale_upload`
/// and `encode_sw` so the source-buffer description and per-frame metadata
/// travel as one value.
struct DmabufFrameInputs {
    fd: RawFd,
    width: u32,
    height: u32,
    stride: u32,
    pts: i64,
    force_idr: bool,
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
            let drm_device_ctx = vaapi_device::create_drm_device_ctx(VAAPI_DEVICE);
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
                    vaapi_device::create_hw_device_ctx(VAAPI_DEVICE)?
                } else {
                    vaapi_ctx
                }
            } else {
                vaapi_device::create_hw_device_ctx(VAAPI_DEVICE)?
            };

            // NV12 hw_frames_ctx for encoder output
            let hw_frames_ctx =
                match vaapi_device::create_hw_frames_ctx(hw_device_ctx, width, height, 10) {
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

        let input = DmabufFrameInputs {
            fd,
            width,
            height,
            stride,
            pts,
            force_idr,
        };

        if self.use_vaapi {
            self.encode_vaapi(&input)
        } else {
            self.encode_sw(&input)
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

                let mut hw_frame = vaapi_device::upload_to_hw_surface(hw_frames_ref.0, &sw_frame)?;
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
        input: &DmabufFrameInputs,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        unsafe {
            let hw_frames_ref = self
                .hw_frames_ctx
                .as_ref()
                .expect("use_vaapi but no hw_frames_ctx");

            // Try zero-copy DMA-BUF import via DRM PRIME → av_hwframe_map().
            // This avoids any CPU pixel access — the DMA-BUF stays on the GPU.
            let hw_frame = match Self::try_drm_prime_import(hw_frames_ref.0, input) {
                Ok(frame) => frame,
                Err(_) => {
                    // Fall back to mmap + CPU scale + upload for non-GPU-local
                    // buffers (e.g. memfds used in tests).
                    // NOTE: This fallback will fail for real GPU DMA-BUFs
                    // (device VRAM can't be mmap'd). A VPP BGRA→NV12 pipeline
                    // is needed for true zero-copy encode — tracked as a gap.
                    Self::mmap_scale_upload(hw_frames_ref.0, input, self.enc_w, self.enc_h)?
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
        input: &DmabufFrameInputs,
    ) -> Result<frame::Video, ffmpeg::Error> {
        // DRM_FORMAT_XRGB8888 = fourcc('X','R','2','4')
        const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
        const DRM_FORMAT_MOD_INVALID: u64 = 0x00ffffffffffffff;

        let &DmabufFrameInputs {
            fd,
            width,
            height,
            stride,
            pts,
            force_idr,
        } = input;
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
    unsafe fn mmap_scale_upload(
        hw_frames_ctx: *mut ffi::AVBufferRef,
        input: &DmabufFrameInputs,
        enc_w: u32,
        enc_h: u32,
    ) -> Result<frame::Video, ffmpeg::Error> {
        let &DmabufFrameInputs {
            fd,
            width,
            height,
            stride,
            pts,
            force_idr,
        } = input;

        let bgra_frame = mmap_bgra_frame(fd, width, height, stride)?;

        let mut scaler = scaling::Context::get(
            Pixel::BGRA,
            width,
            height,
            Pixel::NV12,
            enc_w,
            enc_h,
            scaling::Flags::FAST_BILINEAR,
        )?;

        let mut nv12_frame = frame::Video::empty();
        scaler.run(&bgra_frame, &mut nv12_frame)?;

        let mut hw = vaapi_device::upload_to_hw_surface(hw_frames_ctx, &nv12_frame)?;
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
        input: &DmabufFrameInputs,
    ) -> Result<Option<FullFrameEncoded>, ffmpeg::Error> {
        let &DmabufFrameInputs {
            fd,
            width,
            height,
            stride,
            pts,
            force_idr,
        } = input;

        unsafe {
            let bgra_frame = mmap_bgra_frame(fd, width, height, stride)?;

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

#[cfg(test)]
#[path = "h264_vaapi_tests.rs"]
mod tests;
