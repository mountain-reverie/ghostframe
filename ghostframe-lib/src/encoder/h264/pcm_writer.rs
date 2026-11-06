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

// ---------------------------------------------------------------- lossy canvas
//
// A real encoder (openh264) does not reproduce PCM samples. Its source is a
// canvas in which each 16x16 cell carries its marker as nine flat 4x4 blocks,
// each one bit: dark or light. Four are the centre of the cell's luma, four
// are the cell's Cb blocks and one is its Cr. The levels are 192 apart, so a
// rate control starved enough to spend its highest quantisers (an error of
// 10 to 20 on a flat block) cannot turn a bit; the reader takes the nearest
// level of each block's centre and calls a centre farther than
// `AMBIGUOUS_ABOVE` from both *ambiguous*, the lossy analogue of a macroblock
// that is not a marker. The cell's luma border is texture (noise), so a scene
// change costs bits, and so does `churn`.

const DARK: u32 = 32;
const LIGHT: u32 = 224;
/// A block centre farther than this from both levels is not read with
/// confidence (the halfway point is 96).
const AMBIGUOUS_ABOVE: u32 = 64;
/// Luma blocks `(x, y)` in 4-sample units, bits 0 to 3; Cb blocks in the cell's
/// 2x2 chroma blocks, bits 4 to 7; Cr, the whole chroma cell, bit 8.
const LUMA_BITS: [(usize, usize); 4] = [(1, 1), (2, 1), (1, 2), (2, 2)];
/// Sequences are read modulo this (9 bits: one a block).
pub(crate) const LOSSY_SEQ_SPAN: u32 = 512;

/// The newest scene change at or before `seq`: a multiple of the period, or
/// the start of the scene when there is none.
fn last_scene_change(seq: u32, every: u32) -> u32 {
    seq.checked_div(every).map_or(0, |q| q * every)
}

/// The value cell `c` holds in the canvas of frame `seq`: the newest frame at
/// or before it whose marker cell is `c`, or the last scene change when that is
/// newer. Scene changes repaint every cell.
pub fn canvas_cell(seq: u32, c: usize, scene_change_every: u32) -> u16 {
    let base = last_scene_change(seq, scene_change_every);
    let from_marker = i64::from(seq) - ((i64::from(seq) + CELLS as i64 - c as i64) % CELLS as i64);
    u16::try_from(from_marker.max(i64::from(base))).expect("a harness scene has few frames")
}

/// The analytic picture of frame `seq`'s canvas.
pub fn canvas_picture(seq: u32, scene_change_every: u32) -> Picture {
    let mut cells = [0u16; CELLS];
    for (c, cell) in cells.iter_mut().enumerate() {
        *cell = canvas_cell(seq, c, scene_change_every);
    }
    Picture { cells }
}

fn noise(base: u32, cell: usize, i: usize) -> u8 {
    let mut x = base
        .wrapping_mul(0x9E37_79B1)
        .wrapping_add((cell as u32).wrapping_mul(0x85EB_CA6B))
        .wrapping_add((i as u32).wrapping_mul(0xC2B2_AE35))
        | 1;
    x ^= x << 13;
    x ^= x >> 17;
    x ^= x << 5;
    16 + (x % 220) as u8
}

/// The frame whose noise cell `c` shows in frame `seq`: the newest scene
/// change, or, when `churn_every` is not 0, the newest frame at which the
/// cell's border was repainted. A cell is repainted every `churn_every`
/// frames, staggered by cell, so `CELLS / churn_every` cells change in every
/// frame: a steady rate of new texture for a rate control to follow, where a
/// scene change is a single spike.
fn noise_base(seq: u32, c: usize, scene_change_every: u32, churn_every: u32) -> u32 {
    let scene = last_scene_change(seq, scene_change_every);
    if churn_every == 0 {
        return scene;
    }
    let age = (u64::from(seq) + c as u64) % u64::from(churn_every);
    scene.max(seq.saturating_sub(age as u32))
}

/// The NV12 canvas of frame `seq` (`WIDTH` x `HEIGHT`, tightly packed). Each
/// cell's border is noise that changes with a scene change and, when
/// `churn_every` is not 0, steadily (see [`noise_base`]); neither changes what
/// a cell reads as.
pub fn canvas_nv12(seq: u32, scene_change_every: u32, churn_every: u32, out: &mut Vec<u8>) {
    assert!(
        seq < LOSSY_SEQ_SPAN,
        "a lossy scene is shorter than {LOSSY_SEQ_SPAN} frames"
    );
    out.clear();
    out.resize(FRAME_BYTES, 128);
    let (luma, uv) = out.split_at_mut(WIDTH * HEIGHT);
    for c in 0..CELLS {
        let v = u32::from(canvas_cell(seq, c, scene_change_every));
        let base = noise_base(seq, c, scene_change_every, churn_every);
        let (mx, my) = (c % MB_COLS, c / MB_COLS);
        let level = |bit: usize| (if v >> bit & 1 == 1 { LIGHT } else { DARK }) as u8;
        for y in 0..16 {
            for x in 0..16 {
                luma[(my * 16 + y) * WIDTH + mx * 16 + x] = noise(base, c, y * 16 + x);
            }
        }
        for (bit, &(bx, by)) in LUMA_BITS.iter().enumerate() {
            for y in 0..4 {
                for x in 0..4 {
                    luma[(my * 16 + by * 4 + y) * WIDTH + mx * 16 + bx * 4 + x] = level(bit);
                }
            }
        }
        for y in 0..8 {
            for x in 0..8 {
                let o = (my * 8 + y) * WIDTH + (mx * 8 + x) * 2;
                uv[o] = level(4 + (y / 4) * 2 + x / 4);
                uv[o + 1] = level(8);
            }
        }
    }
}

/// What a lossy read of a decoded I420 frame found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LossyRead {
    /// Each cell's nearest level, decoded (modulo [`LOSSY_SEQ_SPAN`]).
    pub picture: Picture,
    /// Cells whose centre was farther than `AMBIGUOUS_ABOVE` from every level.
    pub ambiguous: usize,
    /// The largest distance of a centre from the level it was read as.
    pub max_err: u32,
}

/// Read the canvas back from a decoded I420 frame. `None` when the frame is
/// not the canvas's size.
pub fn read_lossy(yuv: &[u8]) -> Option<LossyRead> {
    if yuv.len() != FRAME_BYTES {
        return None;
    }
    let (luma, chroma) = yuv.split_at(WIDTH * HEIGHT);
    let (cb, cr) = chroma.split_at(chroma.len() / 2);
    // The mean of the 2x2 samples at the centre of the 4x4 block at `(x0, y0)`.
    let mean = |plane: &[u8], stride: usize, x0: usize, y0: usize| -> u32 {
        let mut sum = 0u32;
        for y in y0 + 1..y0 + 3 {
            for x in x0 + 1..x0 + 3 {
                sum += u32::from(plane[y * stride + x]);
            }
        }
        sum / 4
    };
    let mut cells = [0u16; CELLS];
    let (mut ambiguous, mut max_err) = (0, 0);
    for (c, cell) in cells.iter_mut().enumerate() {
        let (mx, my) = (c % MB_COLS, c / MB_COLS);
        let mut v = 0u32;
        let mut worst = 0;
        let mut bit = |bit: usize, m: u32| {
            v |= u32::from(m > (DARK + LIGHT) / 2) << bit;
            worst = worst.max(m.abs_diff(DARK).min(m.abs_diff(LIGHT)));
        };
        for (i, &(bx, by)) in LUMA_BITS.iter().enumerate() {
            bit(i, mean(luma, WIDTH, mx * 16 + bx * 4, my * 16 + by * 4));
        }
        for i in 0..4 {
            let (x0, y0) = (mx * 8 + i % 2 * 4, my * 8 + i / 2 * 4);
            bit(4 + i, mean(cb, WIDTH / 2, x0, y0));
        }
        bit(8, mean(cr, WIDTH / 2, mx * 8 + 2, my * 8 + 2));
        *cell = v as u16;
        max_err = max_err.max(worst);
        if worst > AMBIGUOUS_ABOVE {
            ambiguous += 1;
        }
    }
    Some(LossyRead {
        picture: Picture { cells },
        ambiguous,
        max_err,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn i420_of(nv12: &[u8]) -> Vec<u8> {
        let (y, uv) = nv12.split_at(WIDTH * HEIGHT);
        let mut out = y.to_vec();
        out.extend(uv.chunks_exact(2).map(|p| p[0]));
        out.extend(uv.chunks_exact(2).map(|p| p[1]));
        out
    }

    /// Churn repaints `CELLS / churn_every` cells' texture per frame, never
    /// changes what a cell reads as, and is what a scene change resets.
    #[test]
    fn churn_repaints_a_steady_share_of_the_texture_and_not_the_picture() {
        let (mut a, mut b) = (Vec::new(), Vec::new());
        let texture = |nv12: &[u8], c: usize| -> Vec<u8> {
            let (mx, my) = (c % MB_COLS, c / MB_COLS);
            (0..16)
                .flat_map(|y| (0..16).map(move |x| (y, x)))
                .filter(|&(y, x)| !((4..12).contains(&x) && (4..12).contains(&y)))
                .map(|(y, x)| nv12[(my * 16 + y) * WIDTH + mx * 16 + x])
                .collect()
        };
        for churn in [2u32, 4, 8] {
            for seq in 100..110 {
                canvas_nv12(seq, 0, churn, &mut a);
                canvas_nv12(seq + 1, 0, churn, &mut b);
                let changed = (0..CELLS)
                    .filter(|&c| texture(&a, c) != texture(&b, c))
                    .count();
                assert_eq!(changed, CELLS / churn as usize, "churn {churn} seq {seq}");
                let read = read_lossy(&i420_of(&b)).expect("right size");
                assert_eq!(read.picture, canvas_picture(seq + 1, 0), "{churn} {seq}");
            }
        }
        // Without churn the texture holds still between scene changes.
        canvas_nv12(100, 0, 0, &mut a);
        canvas_nv12(101, 0, 0, &mut b);
        assert!((0..CELLS).all(|c| texture(&a, c) == texture(&b, c)));
    }

    /// The canvas reads back as the picture it is, for every frame of a scene
    /// (with and without scene changes), and a decoder's error is told from a
    /// misread: up to the ambiguity limit it reads right, past the halfway
    /// point between two levels it does not, and says so.
    #[test]
    fn the_lossy_canvas_round_trips_and_an_error_is_told_from_a_misread() {
        let mut nv12 = Vec::new();
        let mut previous: Option<Picture> = None;
        for every in [0, 7] {
            for seq in (0..LOSSY_SEQ_SPAN).step_by(37).chain([63, 64, 511]) {
                canvas_nv12(seq, every, 0, &mut nv12);
                let yuv = i420_of(&nv12);
                let read = read_lossy(&yuv).expect("right size");
                assert_eq!(read.picture, canvas_picture(seq, every), "{every} {seq}");
                assert_eq!((read.ambiguous, read.max_err), (0, 0), "{every} {seq}");
                assert_eq!(read.picture.cells[marker_cell(seq)], seq as u16);
                if let Some(p) = previous.replace(read.picture.clone()) {
                    assert_ne!(p, read.picture, "consecutive reads differ");
                }
            }
        }
        // An error moves a block's centre toward the other level. Up to
        // `AMBIGUOUS_ABOVE` it reads right and is measured (so the checks
        // below are not vacuous); past it, the block is called ambiguous
        // (before and after the halfway point, 96: the frame cannot say which
        // side it fell on); only an error beyond 128 reads as the other level
        // and passes for sound, which is what the truth log is for.
        canvas_nv12(100, 0, 0, &mut nv12);
        let clean = i420_of(&nv12);
        let at = (9 / MB_COLS * 16 + 4) * WIDTH + 9 % MB_COLS * 16 + 4;
        let was = u32::from(clean[at + WIDTH + 1]);
        let toward_middle = |d: u32| -> u32 {
            if was == LIGHT {
                LIGHT - d
            } else {
                DARK + d
            }
        };
        for (d, ambiguous, right) in [
            (14, 0, true),
            (64, 0, true),
            (65, 1, true),
            (95, 1, true),
            (97, 1, false),
            (150, 0, false),
        ] {
            let mut yuv = clean.clone();
            for dy in 1..3 {
                for dx in 1..3 {
                    yuv[at + dy * WIDTH + dx] = toward_middle(d) as u8;
                }
            }
            let read = read_lossy(&yuv).unwrap();
            assert_eq!(read.ambiguous, ambiguous, "{d}");
            assert_eq!(read.picture == canvas_picture(100, 0), right, "{d}");
            assert_eq!(read.max_err, if d > 96 { 192 - d } else { d }, "{d}");
        }
    }

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
