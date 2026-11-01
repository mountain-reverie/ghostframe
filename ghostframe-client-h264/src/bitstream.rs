//! Just enough H.264 bitstream reading to answer two questions the decoders
//! cannot answer for us, plus one rewrite. Backend-independent and pure: no
//! decoder, no allocation beyond the NAL being looked at.
//!
//! 1. **Was a reference picture lost?** [`RefContinuity`]. The transport cannot
//!    tell: `frame_seq` is the server's *capture* counter, which skips every
//!    unchanged capture, and frame assembly drops an incomplete access unit
//!    without a trace. The stream itself can: with
//!    `gaps_in_frame_num_value_allowed_flag == 0`, every reference picture's
//!    `frame_num` is the previous reference picture's plus one (7.4.3), so a
//!    jump is a lost reference picture and nothing else. A lost *non*-reference
//!    picture is invisible here, which is the right answer: nothing predicts
//!    from it, so the decoder needs no reset.
//!
//! 2. **What does the SPS say?** [`Sps`], parsed through the VUI, because both
//!    the question above and the rewrite below need fields at its far end.
//!
//! 3. **[`low_latency_sps`]**: raise the VUI's `max_dec_frame_buffering` to
//!    `max_num_ref_frames + 1` on a stream that already declares
//!    `max_num_reorder_frames == 0`. GStreamer's H.264 base class only bumps a
//!    picture out early (its low-latency path) while the DPB has an empty slot,
//!    and it counts reference pictures that were already output as occupying
//!    one. A no-reorder stream whose SPS says `max_dec_frame_buffering ==
//!    max_num_ref_frames` -- what `h264_vaapi` emits -- therefore always holds
//!    its newest picture until the next access unit arrives. One extra slot is
//!    what the low-latency path needs, and it changes no decoded pixel: the
//!    stream promises no reordering, so the extra slot never holds anything the
//!    decoder would not have output anyway.
//!
//! Every parser returns `None` on anything it does not understand rather than
//! guessing, and callers treat `None` as "no opinion".

/// NAL unit types this module looks at (Table 7-1).
const NAL_SLICE_NON_IDR: u8 = 1;
const NAL_SLICE_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;

/// `profile_idc` values whose SPS carries the chroma/bit-depth/scaling block
/// (7.3.2.1.1).
const HIGH_PROFILES: [u8; 13] = [100, 110, 122, 244, 44, 83, 86, 118, 128, 138, 139, 134, 135];

/// One NAL unit of an Annex-B access unit: its byte range in the access unit,
/// header byte included, start code and trailing zero bytes excluded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalSpan {
    pub start: usize,
    pub end: usize,
}

impl NalSpan {
    fn nal_type(&self, au: &[u8]) -> u8 {
        au[self.start] & 0x1F
    }
}

/// Split an Annex-B access unit into its NAL units.
pub fn nal_spans(au: &[u8]) -> Vec<NalSpan> {
    // Payload starts: the byte after every `00 00 01`.
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut spans = Vec::with_capacity(starts.len());
    for (k, &start) in starts.iter().enumerate() {
        // Up to the next start code, then back over the zeros before it: the
        // leading zero of a 4-byte start code, or trailing_zero_8bits. Neither
        // is NAL content.
        let mut end = starts.get(k + 1).map_or(au.len(), |&next| next - 3);
        while end > start && au[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            spans.push(NalSpan { start, end });
        }
    }
    spans
}

/// Strip emulation-prevention bytes: every `00 00 03` becomes `00 00`.
fn unescape(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

/// Insert emulation-prevention bytes so no `00 00 0x` (x <= 3) appears.
fn escape(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + rbsp.len() / 64 + 1);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        zeros = if b == 0 { zeros + 1 } else { 0 };
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bit(&mut self) -> Option<u32> {
        let byte = *self.data.get(self.pos / 8)?;
        let b = (byte >> (7 - self.pos % 8)) & 1;
        self.pos += 1;
        Some(u32::from(b))
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        debug_assert!(n <= 32);
        let mut v: u64 = 0;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        u32::try_from(v).ok()
    }

    fn flag(&mut self) -> Option<bool> {
        Some(self.bit()? == 1)
    }

    /// ue(v). Refuses codes longer than 32 bits: no syntax element read here
    /// can legally be that large, so one is corruption.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0;
        while self.bit()? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        let rest = self.bits(leading)?;
        u32::try_from((1u64 << leading) - 1 + u64::from(rest)).ok()
    }

    fn se(&mut self) -> Option<i32> {
        let k = self.ue()?;
        let mag = i32::try_from(k.div_ceil(2)).ok()?;
        Some(if k % 2 == 1 { mag } else { -mag })
    }
}

struct BitWriter {
    out: Vec<u8>,
    used: u32,
}

impl BitWriter {
    fn new() -> Self {
        Self {
            out: Vec::new(),
            used: 0,
        }
    }

    fn bit(&mut self, b: u32) {
        if self.used.is_multiple_of(8) {
            self.out.push(0);
        }
        if b != 0 {
            let last = self.out.len() - 1;
            self.out[last] |= 1 << (7 - self.used % 8);
        }
        self.used += 1;
    }

    fn ue(&mut self, v: u32) {
        let v1 = u64::from(v) + 1;
        let len = 64 - v1.leading_zeros();
        for _ in 1..len {
            self.bit(0);
        }
        for i in (0..len).rev() {
            self.bit(((v1 >> i) & 1) as u32);
        }
    }

    /// rbsp_trailing_bits: a stop bit, then zeros to the byte boundary.
    fn finish(mut self) -> Vec<u8> {
        self.bit(1);
        self.out
    }
}

/// The SPS fields this crate needs (7.3.2.1.1 and E.1.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sps {
    pub id: u32,
    pub separate_colour_plane: bool,
    pub log2_max_frame_num: u32,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    /// VUI `bitstream_restriction`, when present.
    pub restriction: Option<BitstreamRestriction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BitstreamRestriction {
    pub max_num_reorder_frames: u32,
    pub max_dec_frame_buffering: u32,
    /// Bit offset, in the RBSP, of the `max_dec_frame_buffering` code.
    /// Nothing but rbsp_trailing_bits follows it, which is what makes
    /// rewriting it a prefix copy.
    mdfb_bit: usize,
}

fn skip_scaling_list(r: &mut BitReader, size: usize) -> Option<()> {
    let (mut last, mut next) = (8i32, 8i32);
    for _ in 0..size {
        if next != 0 {
            next = (last + r.se()? + 256) % 256;
        }
        if next != 0 {
            last = next;
        }
    }
    Some(())
}

fn skip_hrd(r: &mut BitReader) -> Option<()> {
    let cpb_cnt = r.ue()? + 1;
    if cpb_cnt > 32 {
        return None;
    }
    r.bits(8)?; // bit_rate_scale, cpb_size_scale
    for _ in 0..cpb_cnt {
        r.ue()?;
        r.ue()?;
        r.flag()?;
    }
    r.bits(20)?; // four 5-bit length fields
    Some(())
}

/// Parse an SPS NAL (header byte included, still escaped).
pub fn parse_sps(nal: &[u8]) -> Option<Sps> {
    parse_sps_rbsp(&unescape(nal.get(1..)?))
}

fn parse_sps_rbsp(rbsp: &[u8]) -> Option<Sps> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.bits(8)? as u8;
    r.bits(16)?; // constraint flags + reserved, level_idc
    let id = r.ue()?;
    let mut separate_colour_plane = false;
    if HIGH_PROFILES.contains(&profile_idc) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            separate_colour_plane = r.flag()?;
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.flag()?; // qpprime_y_zero_transform_bypass_flag
        if r.flag()? {
            let lists = if chroma_format_idc == 3 { 12 } else { 8 };
            for i in 0..lists {
                if r.flag()? {
                    skip_scaling_list(&mut r, if i < 6 { 16 } else { 64 })?;
                }
            }
        }
    }
    let log2_max_frame_num = r.ue()? + 4;
    if log2_max_frame_num > 16 {
        return None;
    }
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.flag()?;
            r.se()?;
            r.se()?;
            let cycle = r.ue()?;
            if cycle > 255 {
                return None;
            }
            for _ in 0..cycle {
                r.se()?;
            }
        }
        2 => {}
        _ => return None,
    }
    let max_num_ref_frames = r.ue()?;
    let gaps_in_frame_num_allowed = r.flag()?;
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if !r.flag()? {
        r.flag()?; // mb_adaptive_frame_field_flag
    }
    r.flag()?; // direct_8x8_inference_flag
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?;
        }
    }
    let mut restriction = None;
    if r.flag()? {
        // vui_parameters()
        if r.flag()? && r.bits(8)? == 255 {
            r.bits(32)?; // sar_width, sar_height
        }
        if r.flag()? {
            r.flag()?;
        }
        if r.flag()? {
            r.bits(4)?; // video_format, video_full_range_flag
            if r.flag()? {
                r.bits(24)?;
            }
        }
        if r.flag()? {
            r.ue()?;
            r.ue()?;
        }
        if r.flag()? {
            r.bits(32)?;
            r.bits(32)?;
            r.flag()?;
        }
        let nal_hrd = r.flag()?;
        if nal_hrd {
            skip_hrd(&mut r)?;
        }
        let vcl_hrd = r.flag()?;
        if vcl_hrd {
            skip_hrd(&mut r)?;
        }
        if nal_hrd || vcl_hrd {
            r.flag()?; // low_delay_hrd_flag
        }
        r.flag()?; // pic_struct_present_flag
        if r.flag()? {
            r.flag()?; // motion_vectors_over_pic_boundaries_flag
            for _ in 0..4 {
                r.ue()?;
            }
            let max_num_reorder_frames = r.ue()?;
            let mdfb_bit = r.pos;
            let max_dec_frame_buffering = r.ue()?;
            restriction = Some(BitstreamRestriction {
                max_num_reorder_frames,
                max_dec_frame_buffering,
                mdfb_bit,
            });
        }
    }
    Some(Sps {
        id,
        separate_colour_plane,
        log2_max_frame_num,
        max_num_ref_frames,
        gaps_in_frame_num_allowed,
        restriction,
    })
}

/// `(pps_id, sps_id)` from a PPS NAL (header byte included, still escaped).
fn parse_pps_ids(nal: &[u8]) -> Option<(u32, u32)> {
    let rbsp = unescape(nal.get(1..nal.len().min(16))?);
    let mut r = BitReader::new(&rbsp);
    Some((r.ue()?, r.ue()?))
}

/// The DPB size [`low_latency_sps`] raises a stream to, or `None` when the
/// stream should be left alone: no VUI restriction, a stream that reorders,
/// or a DPB that already has the spare slot.
fn low_latency_dpb(sps: &Sps) -> Option<u32> {
    let r = sps.restriction?;
    let want = sps.max_num_ref_frames + 1;
    // 16 is the largest DPB any level allows (A.3.1, item h).
    (r.max_num_reorder_frames == 0 && r.max_dec_frame_buffering < want && want <= 16)
        .then_some(want)
}

/// Rewrite every SPS in `au` so a no-reorder stream gets one spare DPB slot
/// (see the module docs). `None` when nothing needed changing, so the common
/// case -- an access unit with no SPS -- costs one scan and no copy.
pub fn low_latency_sps(au: &[u8]) -> Option<Vec<u8>> {
    let spans = nal_spans(au);
    let mut out: Option<Vec<u8>> = None;
    let mut copied_to = 0;
    for span in spans {
        if span.nal_type(au) != NAL_SPS {
            continue;
        }
        let nal = &au[span.start..span.end];
        let rbsp = unescape(&nal[1..]);
        let Some(sps) = parse_sps_rbsp(&rbsp) else {
            continue;
        };
        let Some(dpb) = low_latency_dpb(&sps) else {
            continue;
        };
        let mdfb_bit = sps.restriction.expect("low_latency_dpb checked").mdfb_bit;
        let mut w = BitWriter::new();
        let mut r = BitReader::new(&rbsp);
        for _ in 0..mdfb_bit {
            w.bit(r.bit().expect("bits before an already-parsed field"));
        }
        w.ue(dpb);
        let mut new_nal = vec![nal[0]];
        new_nal.extend_from_slice(&escape(&w.finish()));

        let buf = out.get_or_insert_with(|| Vec::with_capacity(au.len() + 4));
        buf.extend_from_slice(&au[copied_to..span.start]);
        buf.extend_from_slice(&new_nal);
        copied_to = span.end;
    }
    let mut buf = out?;
    buf.extend_from_slice(&au[copied_to..]);
    Some(buf)
}

/// What [`RefContinuity::observe`] concluded about one access unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Continuity {
    /// The access unit follows on from the last reference picture.
    Continuous,
    /// At least one reference picture between the last one seen and this
    /// access unit never arrived.
    Gap { expected: u32, got: u32 },
    /// No opinion: no IDR or no parameter sets seen yet, a stream that allows
    /// frame_num gaps, or a slice header this module cannot read.
    Unknown,
}

/// Tracks `frame_num` across access units to detect a lost reference picture.
/// See the module docs for why this, and not `frame_seq`.
///
/// Not handled: `memory_management_control_operation == 5`, which resets
/// `frame_num` without an IDR. An encoder emitting it would read as a gap
/// here, costing one needless decoder reset at the next keyframe -- the same
/// cost the detector this replaced paid on every keyframe.
#[derive(Debug, Default)]
pub struct RefContinuity {
    sps: Vec<Sps>,
    /// `pps_id -> sps_id`.
    pps: Vec<(u32, u32)>,
    prev_ref_frame_num: Option<u32>,
}

impl RefContinuity {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one access unit, in decode order.
    pub fn observe(&mut self, au: &[u8]) -> Continuity {
        for span in nal_spans(au) {
            let nal = &au[span.start..span.end];
            match span.nal_type(au) {
                NAL_SPS => {
                    if let Some(sps) = parse_sps(nal) {
                        self.sps.retain(|s| s.id != sps.id);
                        self.sps.push(sps);
                    }
                }
                NAL_PPS => {
                    if let Some((pps_id, sps_id)) = parse_pps_ids(nal) {
                        self.pps.retain(|&(p, _)| p != pps_id);
                        self.pps.push((pps_id, sps_id));
                    }
                }
                t @ (NAL_SLICE_NON_IDR | NAL_SLICE_IDR) => {
                    // The first slice decides for the whole access unit.
                    return self.slice(nal, t == NAL_SLICE_IDR);
                }
                _ => {}
            }
        }
        Continuity::Unknown
    }

    fn slice(&mut self, nal: &[u8], idr: bool) -> Continuity {
        let Some((frame_num, is_ref, gaps_allowed, max)) = self.read_slice(nal) else {
            return Continuity::Unknown;
        };
        if gaps_allowed {
            self.prev_ref_frame_num = None;
            return Continuity::Unknown;
        }
        let verdict = match (idr, self.prev_ref_frame_num) {
            (true, _) | (false, None) => {
                if idr {
                    Continuity::Continuous
                } else {
                    Continuity::Unknown
                }
            }
            (false, Some(prev)) => {
                let next = (prev + 1) % max;
                // Equal to `prev` is a second field of a reference pair.
                if frame_num == next || frame_num == prev {
                    Continuity::Continuous
                } else {
                    Continuity::Gap {
                        expected: next,
                        got: frame_num,
                    }
                }
            }
        };
        if is_ref {
            self.prev_ref_frame_num = Some(frame_num);
        }
        verdict
    }

    /// `(frame_num, nal_ref_idc != 0, gaps allowed, MaxFrameNum)`.
    fn read_slice(&self, nal: &[u8]) -> Option<(u32, bool, bool, u32)> {
        let is_ref = (nal.first()? >> 5) & 3 != 0;
        // The fields needed sit within the first few bytes; 32 covers the
        // worst case (three 32-bit ue codes, a 2-bit field, a 16-bit frame_num).
        let rbsp = unescape(nal.get(1..nal.len().min(33))?);
        let mut r = BitReader::new(&rbsp);
        r.ue()?; // first_mb_in_slice
        r.ue()?; // slice_type
        let pps_id = r.ue()?;
        let sps_id = self.pps.iter().find(|&&(p, _)| p == pps_id)?.1;
        let sps = self.sps.iter().find(|s| s.id == sps_id)?;
        if sps.separate_colour_plane {
            r.bits(2)?;
        }
        let frame_num = r.bits(sps.log2_max_frame_num)?;
        Some((
            frame_num,
            is_ref,
            sps.gaps_in_frame_num_allowed,
            1 << sps.log2_max_frame_num,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build an Annex-B access unit from RBSPs, each with its NAL header byte.
    fn au(nals: &[(u8, Vec<u8>)]) -> Vec<u8> {
        let mut out = Vec::new();
        for (header, rbsp) in nals {
            out.extend_from_slice(&[0, 0, 0, 1, *header]);
            out.extend_from_slice(&escape(rbsp));
        }
        out
    }

    /// A Main-profile SPS RBSP with a VUI ending in a bitstream restriction.
    /// `pad_zeros` inserts runs of zero bits (a large `max_bytes_per_pic_denom`
    /// code) so the escaped form contains emulation-prevention bytes.
    fn sps_rbsp(
        log2_max_frame_num: u32,
        refs: u32,
        reorder: u32,
        mdfb: u32,
        gaps: bool,
        pad_zeros: bool,
    ) -> Vec<u8> {
        let mut w = BitWriter::new();
        for b in [0, 1, 0, 0, 1, 1, 0, 1] {
            w.bit(b); // profile_idc 77
        }
        for _ in 0..8 {
            w.bit(0); // constraint flags
        }
        for b in [0, 0, 1, 0, 1, 0, 0, 0] {
            w.bit(b); // level_idc 40
        }
        w.ue(0); // sps id
        w.ue(log2_max_frame_num - 4);
        w.ue(2); // poc type 2
        w.ue(refs);
        w.bit(u32::from(gaps));
        w.ue(119); // width in mbs - 1
        w.ue(67); // height in map units - 1
        w.bit(1); // frame_mbs_only
        w.bit(1); // direct_8x8
        w.bit(1); // frame_cropping
        for v in [0, 0, 0, 4] {
            w.ue(v);
        }
        w.bit(1); // vui present
        w.bit(0); // aspect
        w.bit(0); // overscan
        w.bit(0); // video signal
        w.bit(0); // chroma loc
                  // timing_info: its two mostly-zero 32-bit fields escape, so only the
                  // `pad_zeros` variant carries them.
        w.bit(u32::from(pad_zeros));
        if pad_zeros {
            for i in 0..64 {
                w.bit(u32::from(i == 31 || i == 62));
            }
            w.bit(0); // fixed frame rate
        }
        w.bit(0); // nal hrd
        w.bit(0); // vcl hrd
        w.bit(0); // pic struct
        w.bit(1); // bitstream_restriction
        w.bit(1);
        w.ue(if pad_zeros { (1 << 30) - 1 } else { 2 });
        w.ue(1);
        w.ue(16);
        w.ue(16);
        w.ue(reorder);
        w.ue(mdfb);
        w.finish()
    }

    fn pps_rbsp() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0);
        w.ue(0);
        w.finish()
    }

    fn slice_rbsp(frame_num: u32, log2: u32) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0); // first_mb
        w.ue(0); // slice_type
        w.ue(0); // pps id
        for i in (0..log2).rev() {
            w.bit((frame_num >> i) & 1);
        }
        w.finish()
    }

    const SPS_H: u8 = 0x67;
    const PPS_H: u8 = 0x68;
    const IDR_H: u8 = 0x65;
    const P_REF_H: u8 = 0x41;
    const P_NONREF_H: u8 = 0x01;

    fn idr_au(log2: u32) -> Vec<u8> {
        au(&[
            (SPS_H, sps_rbsp(log2, 1, 0, 1, false, false)),
            (PPS_H, pps_rbsp()),
            (IDR_H, slice_rbsp(0, log2)),
        ])
    }

    fn p_au(frame_num: u32, log2: u32, is_ref: bool) -> Vec<u8> {
        let h = if is_ref { P_REF_H } else { P_NONREF_H };
        au(&[(h, slice_rbsp(frame_num, log2))])
    }

    #[test]
    fn escape_round_trips_and_removes_start_code_emulation() {
        let raw = [0, 0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7];
        let esc = escape(&raw);
        assert!(
            !esc.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2),
            "escaped stream still emulates a start code: {esc:?}"
        );
        assert_eq!(unescape(&esc), raw);
    }

    #[test]
    fn nal_spans_handles_three_and_four_byte_start_codes() {
        let data = [
            0, 0, 0, 1, 0x67, 9, 0, 0, 1, 0x68, 8, 0, 0, 0, 0, 1, 0x65, 7,
        ];
        let spans = nal_spans(&data);
        let bodies: Vec<&[u8]> = spans.iter().map(|s| &data[s.start..s.end]).collect();
        assert_eq!(bodies, [&[0x67, 9][..], &[0x68, 8], &[0x65, 7]]);
    }

    #[test]
    fn sps_fields_parse_including_the_vui_restriction() {
        let sps =
            parse_sps(&au(&[(SPS_H, sps_rbsp(8, 1, 0, 1, false, true))])[4..]).expect("parse");
        assert_eq!(sps.log2_max_frame_num, 8);
        assert_eq!(sps.max_num_ref_frames, 1);
        assert!(!sps.gaps_in_frame_num_allowed);
        let r = sps.restriction.expect("restriction");
        assert_eq!(
            (r.max_num_reorder_frames, r.max_dec_frame_buffering),
            (0, 1)
        );
    }

    #[test]
    fn a_missing_reference_picture_is_a_gap_and_nothing_else_is() {
        let mut c = RefContinuity::new();
        assert_eq!(
            c.observe(&p_au(3, 4, true)),
            Continuity::Unknown,
            "before any IDR"
        );
        assert_eq!(c.observe(&idr_au(4)), Continuity::Continuous);
        assert_eq!(c.observe(&p_au(1, 4, true)), Continuity::Continuous);
        // A non-reference picture does not advance frame_num, so losing one
        // must not read as a gap.
        assert_eq!(c.observe(&p_au(2, 4, false)), Continuity::Continuous);
        assert_eq!(c.observe(&p_au(2, 4, true)), Continuity::Continuous);
        // frame_num 3 lost.
        assert_eq!(
            c.observe(&p_au(4, 4, true)),
            Continuity::Gap {
                expected: 3,
                got: 4
            }
        );
        // Re-synchronised: one loss is reported once.
        assert_eq!(c.observe(&p_au(5, 4, true)), Continuity::Continuous);
    }

    #[test]
    fn frame_num_wraps_at_max_frame_num() {
        let mut c = RefContinuity::new();
        c.observe(&idr_au(4));
        for n in 1..16 {
            assert_eq!(
                c.observe(&p_au(n, 4, true)),
                Continuity::Continuous,
                "n={n}"
            );
        }
        assert_eq!(
            c.observe(&p_au(0, 4, true)),
            Continuity::Continuous,
            "16 wraps to 0"
        );
        assert_eq!(c.observe(&p_au(1, 4, true)), Continuity::Continuous);
    }

    #[test]
    fn an_idr_resynchronises_after_a_gap() {
        let mut c = RefContinuity::new();
        c.observe(&idr_au(4));
        assert!(matches!(
            c.observe(&p_au(7, 4, true)),
            Continuity::Gap { .. }
        ));
        assert_eq!(c.observe(&idr_au(4)), Continuity::Continuous);
        assert_eq!(c.observe(&p_au(1, 4, true)), Continuity::Continuous);
    }

    #[test]
    fn a_stream_allowing_frame_num_gaps_gets_no_opinion() {
        let mut c = RefContinuity::new();
        let idr = au(&[
            (SPS_H, sps_rbsp(4, 1, 0, 1, true, false)),
            (PPS_H, pps_rbsp()),
            (IDR_H, slice_rbsp(0, 4)),
        ]);
        assert_eq!(c.observe(&idr), Continuity::Unknown);
        assert_eq!(c.observe(&p_au(9, 4, true)), Continuity::Unknown);
    }

    #[test]
    fn low_latency_sps_adds_exactly_one_spare_slot_and_keeps_everything_else() {
        for pad in [false, true] {
            let original = idr_au_with(sps_rbsp(8, 1, 0, 1, false, pad));
            let has_epb = |a: &[u8]| {
                let s = nal_spans(a)[0];
                a[s.start..s.end].windows(3).any(|w| w == [0, 0, 3])
            };
            assert_eq!(
                has_epb(&original),
                pad,
                "premise: pad={pad} controls escaping"
            );
            let rewritten = low_latency_sps(&original).expect("needs rewriting");
            assert_eq!(has_epb(&rewritten), pad, "escaping lost or invented");
            let before = parse_sps(&original[4..nal_spans(&original)[0].end]).expect("before");
            let spans = nal_spans(&rewritten);
            let after = parse_sps(&rewritten[spans[0].start..spans[0].end]).expect("after");
            assert_eq!(after.restriction.expect("r").max_dec_frame_buffering, 2);
            assert_eq!(
                Sps {
                    restriction: None,
                    ..after.clone()
                },
                Sps {
                    restriction: None,
                    ..before
                },
                "pad={pad}: a field other than max_dec_frame_buffering changed"
            );
            assert_eq!(after.restriction.expect("r").max_num_reorder_frames, 0);
            // The NALs after the SPS are carried over byte for byte.
            let tail = |a: &[u8]| a[nal_spans(a)[1].start - 4..].to_vec();
            assert_eq!(tail(&rewritten), tail(&original), "pad={pad}");
        }
    }

    fn idr_au_with(sps: Vec<u8>) -> Vec<u8> {
        au(&[(SPS_H, sps), (PPS_H, pps_rbsp()), (IDR_H, slice_rbsp(0, 8))])
    }

    #[test]
    fn low_latency_sps_leaves_reordering_and_already_spare_streams_alone() {
        assert_eq!(
            low_latency_sps(&idr_au_with(sps_rbsp(8, 1, 1, 2, false, false))),
            None
        );
        assert_eq!(
            low_latency_sps(&idr_au_with(sps_rbsp(8, 1, 0, 2, false, false))),
            None
        );
        assert_eq!(low_latency_sps(&p_au(1, 8, true)), None, "no SPS, no copy");
    }

    #[test]
    fn truncated_input_is_no_opinion_not_a_panic() {
        let full = idr_au(4);
        for cut in 0..full.len() {
            let mut c = RefContinuity::new();
            let _ = c.observe(&full[..cut]);
            let _ = low_latency_sps(&full[..cut]);
        }
    }
}
