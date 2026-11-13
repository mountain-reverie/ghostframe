// SPDX-License-Identifier: AGPL-3.0-or-later
//! libavcodec's software H.264 decoder, for streams the PCM writer wrote.
//!
//! Two uses with different appetites. The T2 oracle wants the strictest
//! decoder there is (`err_recognition = explode`, any damage is an error). The
//! browserless harness's client wants what a player does: carry on past
//! errors, conceal, and show whatever comes out. Both feed one access unit at
//! a time and expect the picture back from the same call (zero output delay,
//! design 3.5).

extern crate ffmpeg_next as ffmpeg;

use ffmpeg::codec::{self, decoder};
use ffmpeg_sys_next as ffi;

use super::pcm_writer::{FRAME_BYTES, HEIGHT, WIDTH};

/// How hard the decoder insists on a clean stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strictness {
    /// `err_recognition = explode`: the oracle.
    Strict,
    /// libavcodec's defaults: gaps are concealed and decoding goes on.
    Lenient,
}

/// A picture that came out of a `feed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// The I420 frame, tightly packed (`FRAME_BYTES`), when it has the PCM
    /// writer's size; empty otherwise.
    pub yuv: Vec<u8>,
    /// `decode_error_flags` or the corrupt flag was set on the frame.
    pub damaged: bool,
    pub pts: Option<i64>,
    pub width: usize,
    pub height: usize,
}

/// Why a `feed` produced no picture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedError {
    /// `send_packet` refused the access unit.
    Rejected(String),
    /// The access unit was accepted and no picture came out of the call.
    NoPicture(String),
}

impl std::fmt::Display for FeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rejected(e) => write!(f, "decoder rejected the access unit: {e}"),
            Self::NoPicture(e) => write!(f, "no picture came out of the call that fed it: {e}"),
        }
    }
}

pub struct PcmDecoder {
    strictness: Strictness,
    video: decoder::Video,
}

impl PcmDecoder {
    pub fn new(strictness: Strictness) -> Self {
        Self {
            strictness,
            video: Self::open(strictness),
        }
    }

    fn open(strictness: Strictness) -> decoder::Video {
        ffmpeg::init().expect("ffmpeg init");
        if strictness == Strictness::Lenient {
            // A client that meets a damaged stream is the point; libavcodec
            // would print a line per concealed macroblock.
            ffmpeg::log::set_level(ffmpeg::log::Level::Quiet);
        }
        let codec = decoder::find(codec::Id::H264).expect("libavcodec has an H.264 decoder");
        let mut ctx = codec::Context::new_with_codec(codec);
        ctx.set_flags(codec::Flags::LOW_DELAY);
        // SAFETY: the context is not opened yet and owned here.
        unsafe {
            if strictness == Strictness::Strict {
                (*ctx.as_mut_ptr()).err_recognition = ffi::AV_EF_EXPLODE
                    | ffi::AV_EF_CAREFUL
                    | ffi::AV_EF_COMPLIANT
                    | ffi::AV_EF_BITSTREAM;
            }
            (*ctx.as_mut_ptr()).thread_count = 1;
        }
        ctx.decoder()
            .open_as(codec)
            .and_then(|o| o.video())
            .expect("open the H.264 decoder")
    }

    /// Throw the decoder away and open a fresh one: what a client does on a
    /// reset.
    pub fn reset(&mut self) {
        self.video = Self::open(self.strictness);
    }

    /// Feed one access unit; a picture must come back from this very call.
    pub fn feed(&mut self, au: &[u8], pts: i64) -> Result<Decoded, FeedError> {
        let mut pkt = ffmpeg::Packet::copy(au);
        pkt.set_pts(Some(pts));
        pkt.set_dts(Some(pts));
        self.video
            .send_packet(&pkt)
            .map_err(|e| FeedError::Rejected(e.to_string()))?;
        let mut f = ffmpeg::frame::Video::empty();
        self.video
            .receive_frame(&mut f)
            .map_err(|e| FeedError::NoPicture(e.to_string()))?;
        // SAFETY: `f` holds a decoded frame; the field is plain data.
        let flags = unsafe { (*f.as_ptr()).decode_error_flags };
        let damaged = flags != 0 || f.flags().contains(ffmpeg::frame::Flags::CORRUPT);
        let (width, height) = (f.width() as usize, f.height() as usize);
        let mut yuv = Vec::new();
        if (width, height) == (WIDTH, HEIGHT) {
            yuv.reserve(FRAME_BYTES);
            for (plane, (w, h)) in [
                (WIDTH, HEIGHT),
                (WIDTH / 2, HEIGHT / 2),
                (WIDTH / 2, HEIGHT / 2),
            ]
            .into_iter()
            .enumerate()
            {
                let (data, stride) = (f.data(plane), f.stride(plane));
                for y in 0..h {
                    yuv.extend_from_slice(&data[y * stride..y * stride + w]);
                }
            }
        }
        Ok(Decoded {
            yuv,
            damaged,
            pts: f.pts(),
            width,
            height,
        })
    }
}
