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

use ffmpeg::codec;
use ffmpeg::encoder;
use ffmpeg::format::Pixel;
use ffmpeg::frame;
use ffmpeg::software::scaling;
use ffmpeg::Packet;
use ffmpeg::Rational;

use ffmpeg_sys_next as ffi;
use tracing::{info, warn};

use super::h264_vaapi::FFMPEG_INIT;
use super::vaapi_device::{self, vaapi_device_path, BufRef};
use super::EncodedTile;
use crate::transport::protocol::Codec as GfCodec;

const TILE_W: u32 = 32;
const TILE_H: u32 = 32;

/// Candidate encoding resolutions to try when the tile size is below the
/// hardware minimum. Sorted ascending; the first one that succeeds wins.
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
    pub(super) use_vaapi: bool,
    /// The dimensions the encoder was actually opened at (may be > TILE_W/H
    /// when padded for hardware minimum constraints).
    pub(super) enc_w: u32,
    pub(super) enc_h: u32,
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
            let hw_device_ctx = vaapi_device::create_hw_device_ctx(vaapi_device_path())?;

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
