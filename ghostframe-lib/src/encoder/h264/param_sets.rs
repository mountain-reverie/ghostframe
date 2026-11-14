// SPDX-License-Identifier: AGPL-3.0-or-later
//! Packed SPS and PPS for the libva-direct backend (design 3.1, S2 g).
//!
//! The backend hands these to the driver as packed headers. Mesa parses them
//! and regenerates the SPS and PPS in the stream from the values, which is why
//! the PPS it emits differs from the one written here (no deblocking control,
//! `StreamParams::emitted`). The bytes carry a start code and are not
//! escaped: the packed-header buffer says `has_emulation_bytes = 0`, and the
//! VUI timing fields contain `00 00 00 01`.

use super::syntax::{BitWriter, StreamParams};

/// What the SPS says about the picture and its timing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SeqConfig {
    /// Visible size; the coded size is rounded up to whole macroblocks and the
    /// difference cropped.
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl SeqConfig {
    pub(crate) fn mb_width(&self) -> u32 {
        self.width.div_ceil(16)
    }

    pub(crate) fn mb_height(&self) -> u32 {
        self.height.div_ceil(16)
    }

    /// Reject what the writer cannot express: odd sizes (the crop unit is two
    /// luma samples) and streams above level 5.2.
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.width == 0
            || self.height == 0
            || !self.width.is_multiple_of(2)
            || !self.height.is_multiple_of(2)
        {
            return Err(format!(
                "{}x{}: sizes must be even",
                self.width, self.height
            ));
        }
        if self.level_idc().is_none() {
            return Err(format!(
                "{}x{} at {} fps exceeds level 5.2",
                self.width, self.height, self.fps
            ));
        }
        Ok(())
    }

    /// The lowest `level_idc` from 4.2 up whose frame size and macroblock
    /// rate (Table A-1) hold the stream; `None` above level 5.2.
    pub(crate) fn level_idc(&self) -> Option<u8> {
        let mbs = self.mb_width() * self.mb_height();
        let rate = u64::from(mbs) * u64::from(self.fps);
        // (level_idc, MaxFS, MaxMBPS)
        [
            (42, 8_704, 522_240),
            (50, 22_080, 589_824),
            (51, 36_864, 983_040),
            (52, 36_864, 2_073_600),
        ]
        .into_iter()
        .find(|&(_, fs, mbps)| mbs <= fs && rate <= mbps)
        .map(|(l, _, _)| l)
    }

    /// Rows of the coded picture below the visible one (cropped in the SPS).
    pub(crate) fn crop_bottom_rows(&self) -> u32 {
        self.mb_height() * 16 - self.height
    }

    /// Columns of the coded picture right of the visible one.
    pub(crate) fn crop_right_cols(&self) -> u32 {
        self.mb_width() * 16 - self.width
    }
}

const PROFILE_HIGH: u32 = 100;

fn nal_header(w: &mut BitWriter, nal_ref_idc: u32, nal_type: u32) {
    w.put_bits(1, 32);
    w.put_bits(0, 1);
    w.put_bits(u64::from(nal_ref_idc), 2);
    w.put_bits(u64::from(nal_type), 5);
}

fn trailing_bits(w: &mut BitWriter) {
    w.put_bit(true);
    while !w.bit_len().is_multiple_of(8) {
        w.put_bit(false);
    }
}

/// The SPS NAL unit: High profile, POC type 0, `max_num_ref_frames` and the
/// frame number and POC widths of `params`, gaps allowed, and a VUI with
/// timing and `bitstream_restriction{reorder 0, max_dec_frame_buffering =
/// max_num_ref_frames}` so a decoder that sizes its output queue from the VUI
/// emits at once.
pub(crate) fn write_sps(params: &StreamParams, c: &SeqConfig) -> Vec<u8> {
    let mut w = BitWriter::default();
    nal_header(&mut w, 3, 7);
    w.put_bits(u64::from(PROFILE_HIGH), 8);
    w.put_bits(0, 8);
    w.put_bits(u64::from(c.level_idc().expect("validated")), 8);
    w.put_ue(0);
    // High profile: 4:2:0, 8 bit, no transform bypass, no scaling matrix.
    w.put_ue(1);
    w.put_ue(0);
    w.put_ue(0);
    w.put_bit(false);
    w.put_bit(false);
    w.put_ue(params.log2_max_frame_num - 4);
    w.put_ue(0);
    w.put_ue(params.log2_max_poc_lsb - 4);
    w.put_ue(params.max_num_ref_frames);
    w.put_bit(params.gaps_in_frame_num_allowed);
    w.put_ue(c.mb_width() - 1);
    w.put_ue(c.mb_height() - 1);
    w.put_bit(true);
    w.put_bit(true);
    let (right, bottom) = (c.crop_right_cols(), c.crop_bottom_rows());
    w.put_bit(right != 0 || bottom != 0);
    if right != 0 || bottom != 0 {
        // Offsets are in units of two samples (4:2:0).
        w.put_ue(0);
        w.put_ue(right / 2);
        w.put_ue(0);
        w.put_ue(bottom / 2);
    }
    w.put_bit(true);
    // aspect ratio, overscan, video signal, chroma location: absent
    for _ in 0..4 {
        w.put_bit(false);
    }
    w.put_bit(true);
    w.put_bits(1, 32);
    w.put_bits(u64::from(c.fps) * 2, 32);
    w.put_bit(true);
    // nal_hrd, vcl_hrd, pic_struct_present: absent
    for _ in 0..3 {
        w.put_bit(false);
    }
    w.put_bit(true);
    w.put_bit(true);
    w.put_ue(0);
    w.put_ue(0);
    w.put_ue(16);
    w.put_ue(16);
    w.put_ue(0);
    w.put_ue(params.max_num_ref_frames);
    trailing_bits(&mut w);
    w.into_bytes()
}

/// The PPS NAL unit. `init_qp` is `pic_init_qp`; `transform_8x8_mode` is
/// always 0 (the firmware forces it).
pub(crate) fn write_pps(params: &StreamParams, init_qp: u8) -> Vec<u8> {
    let mut w = BitWriter::default();
    nal_header(&mut w, 3, 8);
    w.put_ue(0);
    w.put_ue(0);
    w.put_bit(params.cabac);
    w.put_bit(params.bottom_field_pic_order_in_frame_present);
    w.put_ue(0);
    w.put_ue(params.num_ref_idx_l0_default - 1);
    w.put_ue(0);
    w.put_bit(params.weighted_pred);
    w.put_bits(0, 2);
    w.put_se(i32::from(init_qp) - 26);
    w.put_se(0);
    w.put_se(0);
    w.put_bit(params.deblocking_control_present);
    w.put_bit(false);
    w.put_bit(params.redundant_pic_cnt_present);
    // transform_8x8_mode, pic_scaling_matrix_present, second chroma offset
    w.put_bit(false);
    w.put_bit(false);
    w.put_se(0);
    trailing_bits(&mut w);
    w.into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter()
            .map(|x| format!("{x:02x}"))
            .collect::<Vec<_>>()
            .join(" ")
    }

    // Captured from the S2 spike's writer (`enc2.c`: High, CABAC, transform
    // 8x8 off, VUI, 30 fps, QP 26), which Mesa accepted and which decoded.
    #[test]
    fn matches_the_spike_writer() {
        let p = StreamParams::writer();
        let sps = |w, h| {
            write_sps(
                &p,
                &SeqConfig {
                    width: w,
                    height: h,
                    fps: 30,
                },
            )
        };
        assert_eq!(
            hex(&sps(640, 360)),
            "00 00 00 01 67 64 00 2a ac 24 69 60 a0 2f f9 61 00 00 00 01 00 00 00 3c 8f 08 84 65 80"
        );
        assert_eq!(
            hex(&sps(1920, 1080)),
            "00 00 00 01 67 64 00 2a ac 24 69 60 78 02 27 e5 84 00 00 00 04 00 00 00 f2 3c 22 11 96"
        );
        assert_eq!(hex(&write_pps(&p, 26)), "00 00 00 01 68 ee 3c 30");
    }

    #[test]
    fn levels_crop_and_validation() {
        let c = |w, h, fps| SeqConfig {
            width: w,
            height: h,
            fps,
        };
        assert_eq!(c(1920, 1080, 30).level_idc(), Some(42));
        assert_eq!(c(1920, 1080, 60).level_idc(), Some(42));
        // Macroblock rate, not frame size, pushes 1080p120 to level 5.1.
        assert_eq!(c(1920, 1080, 120).level_idc(), Some(51));
        assert_eq!(c(3840, 2160, 30).level_idc(), Some(51));
        assert_eq!(c(3840, 2160, 60).level_idc(), Some(52));
        assert_eq!(c(3840, 2160, 120).level_idc(), None);
        assert!(c(3840, 2160, 120).validate().is_err());
        assert!(c(1920, 1080, 30).validate().is_ok());
        assert_eq!(c(1920, 1080, 30).crop_bottom_rows(), 8);
        assert_eq!(c(1280, 720, 30).crop_bottom_rows(), 0);
        assert_eq!(c(1366, 768, 30).crop_right_cols(), 10);
        assert!(c(1367, 768, 30).validate().is_err());
        assert!(c(1366, 767, 30).validate().is_err());
    }
}
