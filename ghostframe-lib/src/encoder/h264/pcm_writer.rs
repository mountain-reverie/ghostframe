//! A tiny software H.264 writer for the T2 oracle and the browserless harness: real, decodable CAVLC
//! streams with no encoder and no GPU.
//!
//! Every picture is a 8x8 grid of macroblocks. An IDR is all I_PCM. A P
//! picture is a run of P_Skip macroblocks (zero motion, no residual: an exact
//! copy of `RefPicList0[0]` because deblocking is off) plus one I_PCM
//! macroblock, its *marker cell*. The marker encodes the picture's
//! `frame_seq`, so a decoded picture says which pictures it was built from:
//! it carries the markers of its whole ancestry, newest on top.
//!
//! Slice headers are the real writer's, from the `RefManager`'s plans; only
//! the slice data and the parameter sets are written here.

use super::syntax::BitWriter;
use super::syntax::{
    escape_rbsp, write_slice_header, SliceHeader, SliceType, StreamParams, NAL_IDR, NAL_SLICE,
};

pub(crate) const MB_COLS: usize = 8;
pub(crate) const MB_ROWS: usize = 8;
pub(crate) const CELLS: usize = MB_COLS * MB_ROWS;
pub(crate) const WIDTH: usize = MB_COLS * 16;
pub(crate) const HEIGHT: usize = MB_ROWS * 16;
pub(crate) const FRAME_BYTES: usize = WIDTH * HEIGHT * 3 / 2;

const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// The stream the writer produces: CAVLC (PCM needs no entropy coder, and a
/// P slice's skip runs are plain `ue(v)`), deblocking control present so the
/// slice header can switch the filter off.
pub(crate) fn pcm_params() -> StreamParams {
    StreamParams {
        cabac: false,
        ..StreamParams::writer()
    }
}

/// Which macroblock a picture's marker lives in.
pub fn marker_cell(frame_seq: u32) -> usize {
    frame_seq as usize % CELLS
}

fn trailing_bits(w: &mut BitWriter) {
    w.put_bit(true);
    while !w.bit_len().is_multiple_of(8) {
        w.put_bit(false);
    }
}

fn nal(ref_idc: u8, kind: u8, rbsp: Vec<u8>) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, (ref_idc << 5) | kind];
    out.extend(escape_rbsp(&rbsp));
    out
}

/// SPS and PPS, Annex B. Main profile, POC type 0, `gaps_in_frame_num` as the
/// params say, and a VUI saying nothing is reordered, so a decoder outputs
/// each picture as it is fed (design 3.1).
pub(crate) fn parameter_sets(p: &StreamParams) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.put_bits(77, 8);
    w.put_bits(0, 8);
    w.put_bits(40, 8);
    w.put_ue(0);
    w.put_ue(p.log2_max_frame_num - 4);
    w.put_ue(0);
    w.put_ue(p.log2_max_poc_lsb - 4);
    w.put_ue(p.max_num_ref_frames);
    w.put_bit(p.gaps_in_frame_num_allowed);
    w.put_ue(MB_COLS as u32 - 1);
    w.put_ue(MB_ROWS as u32 - 1);
    w.put_bit(true); // frame_mbs_only_flag
    w.put_bit(true); // direct_8x8_inference_flag
    w.put_bit(false); // frame_cropping_flag
    w.put_bit(true); // vui_parameters_present_flag
    for _ in 0..7 {
        // aspect ratio, overscan, video signal, chroma loc, timing, NAL HRD,
        // VCL HRD: all absent.
        w.put_bit(false);
    }
    w.put_bit(false); // pic_struct_present_flag
    w.put_bit(true); // bitstream_restriction_flag
    w.put_bit(true); // motion_vectors_over_pic_boundaries_flag
    w.put_ue(0);
    w.put_ue(0);
    w.put_ue(16);
    w.put_ue(16);
    w.put_ue(0); // max_num_reorder_frames
    w.put_ue(p.max_num_ref_frames); // max_dec_frame_buffering
    trailing_bits(&mut w);
    let mut out = nal(3, NAL_SPS, w.into_bytes());

    let mut w = BitWriter::default();
    w.put_ue(0);
    w.put_ue(0);
    w.put_bit(p.cabac);
    w.put_bit(p.bottom_field_pic_order_in_frame_present);
    w.put_ue(0); // num_slice_groups_minus1
    w.put_ue(p.num_ref_idx_l0_default - 1);
    w.put_ue(0);
    w.put_bit(p.weighted_pred);
    w.put_bits(0, 2);
    w.put_se(0);
    w.put_se(0);
    w.put_se(0);
    w.put_bit(p.deblocking_control_present);
    w.put_bit(false); // constrained_intra_pred_flag
    w.put_bit(p.redundant_pic_cnt_present);
    trailing_bits(&mut w);
    out.extend(nal(3, NAL_PPS, w.into_bytes()));
    out
}

/// The 384 samples of the marker macroblock for `frame_seq`: luma rows 0-7
/// hold the low byte, rows 8-15 the high byte; Cb repeats the low byte
/// inverted, Cr the high byte inverted. A decoder that mixed two pictures'
/// samples inside one macroblock would disagree with itself.
fn marker_samples(frame_seq: u16) -> [u8; 384] {
    let (lo, hi) = (frame_seq as u8, (frame_seq >> 8) as u8);
    let mut s = [0u8; 384];
    for y in 0..16 {
        let v = if y < 8 { lo } else { hi };
        s[y * 16..y * 16 + 16].fill(v);
    }
    s[256..320].fill(!lo);
    s[320..384].fill(!hi);
    s
}

fn put_pcm_mb(w: &mut BitWriter, frame_seq: u16) {
    while !w.bit_len().is_multiple_of(8) {
        w.put_bit(false);
    }
    for b in marker_samples(frame_seq) {
        w.put_bits(u64::from(b), 8);
    }
}

/// One access unit (a single slice NAL, no parameter sets) for `h`. An IDR
/// paints every macroblock with `frame_seq`; a P paints only its marker cell
/// and skips the rest, copying them from its reference. Deblocking is forced
/// off: a filtered skip run would no longer be an exact copy.
pub(crate) fn access_unit(p: &StreamParams, h: &SliceHeader, frame_seq: u16) -> Vec<u8> {
    let mut h = h.clone();
    h.deblock.disable_idc = 1;
    let hb = write_slice_header(p, &h);
    let mut w = BitWriter::default();
    for i in 0..hb.bit_len {
        w.put_bit(hb.bytes[i / 8] & (0x80 >> (i % 8)) != 0);
    }
    match h.slice_type {
        SliceType::I => {
            for _ in 0..CELLS {
                w.put_ue(25); // I_PCM
                put_pcm_mb(&mut w, frame_seq);
            }
        }
        SliceType::P => {
            let cell = marker_cell(u32::from(frame_seq));
            w.put_ue(cell as u32); // mb_skip_run
            w.put_ue(30); // 5 + I_PCM
            put_pcm_mb(&mut w, frame_seq);
            let rest = CELLS - cell - 1;
            if rest > 0 {
                w.put_ue(rest as u32);
            }
        }
    }
    trailing_bits(&mut w);
    let kind = if h.idr { NAL_IDR } else { NAL_SLICE };
    nal(h.nal_ref_idc, kind, w.into_bytes())
}

/// The analytic picture: for each macroblock, the `frame_seq` whose marker it
/// shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picture {
    pub cells: [u16; CELLS],
}

impl Picture {
    pub fn idr(frame_seq: u16) -> Self {
        Self {
            cells: [frame_seq; CELLS],
        }
    }

    /// The picture a P frame decodes to: `reference` with the marker cell
    /// repainted.
    pub fn predicted(reference: &Picture, frame_seq: u16) -> Self {
        let mut p = reference.clone();
        p.cells[marker_cell(u32::from(frame_seq))] = frame_seq;
        p
    }

    /// The I420 frame a decoder returns for this picture.
    pub fn render(&self) -> Vec<u8> {
        let mut out = vec![0u8; FRAME_BYTES];
        let (luma, chroma) = out.split_at_mut(WIDTH * HEIGHT);
        let (cb, cr) = chroma.split_at_mut(chroma.len() / 2);
        for (c, &seq) in self.cells.iter().enumerate() {
            let s = marker_samples(seq);
            let (mx, my) = (c % MB_COLS, c / MB_COLS);
            for y in 0..16 {
                let o = (my * 16 + y) * WIDTH + mx * 16;
                luma[o..o + 16].copy_from_slice(&s[y * 16..y * 16 + 16]);
            }
            for y in 0..8 {
                let o = (my * 8 + y) * (WIDTH / 2) + mx * 8;
                cb[o..o + 8].copy_from_slice(&s[256 + y * 8..264 + y * 8]);
                cr[o..o + 8].copy_from_slice(&s[320 + y * 8..328 + y * 8]);
            }
        }
        out
    }

    /// Read the markers back from a decoded I420 frame. `None` when a
    /// macroblock is not a marker (its samples disagree with themselves).
    pub fn read(yuv: &[u8]) -> Option<Self> {
        if yuv.len() != FRAME_BYTES {
            return None;
        }
        let mut cells = [0u16; CELLS];
        for (c, cell) in cells.iter_mut().enumerate() {
            let (mx, my) = (c % MB_COLS, c / MB_COLS);
            let lo = yuv[(my * 16) * WIDTH + mx * 16];
            let hi = yuv[(my * 16 + 8) * WIDTH + mx * 16];
            *cell = u16::from(lo) | (u16::from(hi) << 8);
        }
        let back = Self { cells };
        (back.render() == yuv).then_some(back)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markers_round_trip_and_differ() {
        let a = Picture::idr(100);
        assert_eq!(Picture::read(&a.render()), Some(a.clone()));
        let b = Picture::predicted(&a, 101);
        assert_ne!(a.render(), b.render());
        assert_eq!(Picture::read(&b.render()), Some(b));
        let mut bad = a.render();
        bad[WIDTH * HEIGHT + 3] ^= 1;
        assert_eq!(Picture::read(&bad), None);
    }
}
