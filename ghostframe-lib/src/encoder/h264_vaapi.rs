//! H.264 tile encoder using VA-API hardware acceleration with libx264 fallback.
//!
//! Each encoder instance maintains inter-frame state for a single 32x32 tile
//! position, enabling P-frames to reference the previous tile content.

extern crate ffmpeg_next as ffmpeg;

use ffmpeg::codec;
use ffmpeg::encoder;
use ffmpeg::format::Pixel;
use ffmpeg::frame;
use ffmpeg::software::scaling;
use ffmpeg::Packet;
use ffmpeg::Rational;

use super::EncodedTile;
use crate::transport::protocol::Codec as GfCodec;

const TILE_W: u32 = 32;
const TILE_H: u32 = 32;

/// H.264 encoder for 32x32 BGRA tiles.
///
/// Tries `h264_vaapi` first, falls back to `libx264` if hardware encoding
/// is unavailable. Input is always 32x32 BGRA (4096 bytes); output is H.264
/// NAL units suitable for network transmission.
pub struct H264VaapiEncoder {
    encoder: encoder::Video,
    scaler: scaling::Context,
    frame_count: u64,
    pts: i64,
}

impl H264VaapiEncoder {
    /// Create a new encoder, preferring VA-API but falling back to libx264.
    pub fn new() -> Result<Self, ffmpeg::Error> {
        ffmpeg::init()?;

        // Try VA-API first; if it fails to open, fall back to libx264.
        if let Some(vaapi_codec) = encoder::find_by_name("h264_vaapi") {
            if let Ok(this) = Self::try_open(vaapi_codec, true) {
                return Ok(this);
            }
        }

        let x264_codec = encoder::find_by_name("libx264")
            .ok_or(ffmpeg::Error::EncoderNotFound)?;
        Self::try_open(x264_codec, false)
    }

    /// Attempt to open an encoder with the given codec.
    fn try_open(enc_codec: codec::codec::Codec, use_vaapi: bool) -> Result<Self, ffmpeg::Error> {
        let mut ctx = codec::context::Context::new_with_codec(enc_codec)
            .encoder()
            .video()?;

        ctx.set_width(TILE_W);
        ctx.set_height(TILE_H);
        ctx.set_time_base(Rational(1, 30));
        ctx.set_frame_rate(Some(Rational(30, 1)));

        if use_vaapi {
            ctx.set_format(Pixel::VAAPI);
        } else {
            ctx.set_format(Pixel::YUV420P);
        }

        let mut opts = ffmpeg::Dictionary::new();
        if !use_vaapi {
            opts.set("preset", "ultrafast");
            opts.set("tune", "zerolatency");
        }
        opts.set("g", "30");

        let opened = ctx.open_with(opts)?;

        // For the scaler, convert BGRA to what the encoder actually needs.
        // VA-API frames need NV12 upload; libx264 uses YUV420P directly.
        let target_fmt = if use_vaapi { Pixel::NV12 } else { opened.format() };

        let scaler = scaling::Context::get(
            Pixel::BGRA,
            TILE_W,
            TILE_H,
            target_fmt,
            TILE_W,
            TILE_H,
            scaling::Flags::FAST_BILINEAR,
        )?;

        Ok(Self {
            encoder: opened,
            scaler,
            frame_count: 0,
            pts: 0,
        })
    }

    /// Encode a 32x32 BGRA tile (4096 bytes) into an H.264 NAL unit.
    ///
    /// Returns `Ok(None)` if the encoder buffers the frame without producing
    /// output yet (unlikely with zerolatency, but possible with VA-API).
    pub fn encode(&mut self, bgra_tile: &[u8]) -> Result<Option<EncodedTile>, ffmpeg::Error> {
        assert_eq!(
            bgra_tile.len(),
            (TILE_W * TILE_H * 4) as usize,
            "expected 4096-byte BGRA tile"
        );

        // Build a BGRA frame from the raw bytes.
        let mut bgra_frame = frame::Video::new(Pixel::BGRA, TILE_W, TILE_H);
        let stride = bgra_frame.stride(0);
        let plane = bgra_frame.data_mut(0);
        // Copy row by row in case stride != width*4
        for y in 0..TILE_H as usize {
            let src_start = y * (TILE_W as usize) * 4;
            let dst_start = y * stride;
            plane[dst_start..dst_start + (TILE_W as usize) * 4]
                .copy_from_slice(&bgra_tile[src_start..src_start + (TILE_W as usize) * 4]);
        }

        // Convert BGRA -> encoder pixel format (NV12 or YUV420P)
        let mut enc_frame = frame::Video::empty();
        self.scaler.run(&bgra_frame, &mut enc_frame)?;
        enc_frame.set_pts(Some(self.pts));
        self.pts += 1;
        self.frame_count += 1;

        // Send frame to encoder
        self.encoder.send_frame(&enc_frame)?;

        // Collect output packet(s)
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
            Err(ffmpeg::Error::Other { errno: libc::EAGAIN }) => Ok(None),
            Err(e) if is_eagain(&e) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// Check if an ffmpeg error represents EAGAIN (encoder needs more input).
fn is_eagain(e: &ffmpeg::Error) -> bool {
    // ffmpeg-next may report EAGAIN differently across platforms
    matches!(e, ffmpeg::Error::Other { errno } if *errno == libc::EAGAIN)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_solid_red_tile() {
        let mut encoder = match H264VaapiEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping H264 test (no encoder available): {e}");
                return;
            }
        };

        let mut tile = vec![0u8; 32 * 32 * 4];
        for pixel in tile.chunks_exact_mut(4) {
            pixel[0] = 0; // B
            pixel[1] = 0; // G
            pixel[2] = 255; // R
            pixel[3] = 255; // A
        }

        let result = encoder.encode(&tile).unwrap();
        assert!(
            result.is_some(),
            "first frame should produce output with zerolatency"
        );
        let encoded = result.unwrap();
        assert_eq!(encoded.codec, GfCodec::H264);
        assert!(!encoded.payload.is_empty());
        assert!(encoded.payload.len() < 4096);
    }

    #[test]
    fn encode_multiple_frames_produces_smaller_p_frames() {
        let mut encoder = match H264VaapiEncoder::new() {
            Ok(e) => e,
            Err(e) => {
                eprintln!("Skipping H264 test (no encoder available): {e}");
                return;
            }
        };

        let mut tile = vec![0u8; 32 * 32 * 4];
        for pixel in tile.chunks_exact_mut(4) {
            pixel[2] = 255;
            pixel[3] = 255;
        }

        let first = encoder.encode(&tile).unwrap().unwrap();
        let second = encoder.encode(&tile).unwrap().unwrap();
        assert!(
            second.payload.len() <= first.payload.len(),
            "P-frame of static content should be <= I-frame: {} vs {}",
            second.payload.len(),
            first.payload.len()
        );
    }
}
