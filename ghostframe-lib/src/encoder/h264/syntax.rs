//! H.264 slice-header syntax: a writer, a parser, and a decoder model that
//! reports which picture a P slice really references.
//!
//! The encoder (VCE) predicts from `RefPicList0[0]` as the app passes it, while
//! a decoder follows the slice header. The two must agree, so every P slice
//! carries an explicit reference list modification (RPLM) and the model here
//! replays default list initialisation, RPLM, MMCO marking, the sliding window
//! and `frame_num` gaps exactly as a decoder does. A test (or the backend's
//! `AuMeta` check) feeds parsed headers to [`DecoderModel`] and compares the
//! effective reference with the plan's.
//!
//! Scope: frame coding (no fields, no MBAFF), P and I slices, POC type 0, no
//! weighted prediction, one slice per picture's header of interest.

/// Stream parameters the header syntax depends on (SPS and PPS fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamParams {
    /// `log2_max_frame_num_minus4 + 4`. Mesa forces 7 (MaxFrameNum 128).
    pub log2_max_frame_num: u32,
    /// `log2_max_pic_order_cnt_lsb_minus4 + 4`.
    pub log2_max_poc_lsb: u32,
    pub max_num_ref_frames: u32,
    pub gaps_in_frame_num_allowed: bool,
    /// PPS `num_ref_idx_l0_default_active_minus1 + 1`.
    pub num_ref_idx_l0_default: u32,
    /// PPS `entropy_coding_mode_flag`; adds `cabac_init_idc` to P headers.
    pub cabac: bool,
    /// PPS `deblocking_filter_control_present_flag`.
    pub deblocking_control_present: bool,
}

impl Default for StreamParams {
    fn default() -> Self {
        Self {
            log2_max_frame_num: 7,
            log2_max_poc_lsb: 16,
            max_num_ref_frames: 4,
            gaps_in_frame_num_allowed: true,
            num_ref_idx_l0_default: 1,
            cabac: false,
            deblocking_control_present: true,
        }
    }
}

impl StreamParams {
    pub(crate) fn max_frame_num(&self) -> u32 {
        1 << self.log2_max_frame_num
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SliceType {
    P,
    I,
}

/// One `ref_pic_list_modification` operation. Values are as coded: the
/// `abs_diff_pic_num_minus1` and the `long_term_pic_num`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rplm {
    /// `modification_of_pic_nums_idc` 0: subtract.
    Sub(u32),
    /// idc 1: add.
    Add(u32),
    /// idc 2: a long-term picture by `long_term_pic_num`.
    Long(u32),
}

/// One memory management control operation (as coded).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mmco {
    /// 1: short-term picture unused (`difference_of_pic_nums_minus1`).
    ForgetShort(u32),
    /// 2: long-term picture unused (`long_term_pic_num`).
    ForgetLong(u32),
    /// 3: short-term picture becomes long-term (`diff_minus1`, `long_term_frame_idx`).
    ShortToLong(u32, u32),
    /// 4: `max_long_term_frame_idx_plus1`.
    MaxLong(u32),
    /// 5: everything unused. Never emitted; modelled only to flag it.
    ClearAll,
    /// 6: the current picture becomes long-term at `long_term_frame_idx`.
    CurrentToLong(u32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Deblock {
    pub disable_idc: u32,
    pub alpha_c0_offset_div2: i32,
    pub beta_offset_div2: i32,
}

/// The slice header fields that matter to references, plus the tail the
/// bitstream needs (QP delta, deblocking) so a writer can emit a whole header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SliceHeader {
    pub nal_ref_idc: u8,
    pub idr: bool,
    pub slice_type: SliceType,
    pub first_mb_in_slice: u32,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    /// `Some(n)`: `num_ref_idx_active_override_flag` with `n` active entries.
    pub num_ref_idx_override: Option<u32>,
    /// Empty: `ref_pic_list_modification_flag_l0 = 0`.
    pub rplm: Vec<Rplm>,
    pub no_output_of_prior_pics: bool,
    pub long_term_reference_flag: bool,
    /// `None`: sliding window (`adaptive_ref_pic_marking_mode_flag = 0`).
    pub mmco: Option<Vec<Mmco>>,
    pub cabac_init_idc: u32,
    pub slice_qp_delta: i32,
    pub deblock: Deblock,
}

impl SliceHeader {
    /// A P slice with no list modification and sliding-window marking.
    pub(crate) fn p(frame_num: u32, poc_lsb: u32, nal_ref_idc: u8) -> Self {
        Self {
            nal_ref_idc,
            idr: false,
            slice_type: SliceType::P,
            first_mb_in_slice: 0,
            pps_id: 0,
            frame_num,
            idr_pic_id: 0,
            poc_lsb,
            num_ref_idx_override: Some(1),
            rplm: Vec::new(),
            no_output_of_prior_pics: false,
            long_term_reference_flag: false,
            mmco: None,
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            deblock: Deblock {
                disable_idc: 0,
                alpha_c0_offset_div2: 0,
                beta_offset_div2: 0,
            },
        }
    }

    pub(crate) fn idr(idr_pic_id: u32, long_term_reference_flag: bool) -> Self {
        Self {
            nal_ref_idc: 3,
            idr: true,
            slice_type: SliceType::I,
            num_ref_idx_override: None,
            idr_pic_id,
            long_term_reference_flag,
            ..Self::p(0, 0, 3)
        }
    }
}

// ---------------------------------------------------------------- bit I/O

/// MSB-first bit writer.
#[derive(Debug, Default, Clone)]
pub(crate) struct BitWriter {
    bytes: Vec<u8>,
    bit_len: usize,
}

impl BitWriter {
    pub(crate) fn put_bit(&mut self, bit: bool) {
        if self.bit_len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if bit {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 0x80 >> (self.bit_len % 8);
        }
        self.bit_len += 1;
    }

    pub(crate) fn put_bits(&mut self, value: u64, n: u32) {
        for i in (0..n).rev() {
            self.put_bit((value >> i) & 1 == 1);
        }
    }

    pub(crate) fn put_ue(&mut self, value: u32) {
        let x = u64::from(value) + 1;
        let len = 64 - x.leading_zeros();
        self.put_bits(0, len - 1);
        self.put_bits(x, len);
    }

    pub(crate) fn put_se(&mut self, value: i32) {
        let mapped = if value > 0 {
            (value as u32) * 2 - 1
        } else {
            value.unsigned_abs() * 2
        };
        self.put_ue(mapped);
    }

    pub(crate) fn bit_len(&self) -> usize {
        self.bit_len
    }

    /// The bits written, the last byte zero-padded.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// Ran out of bits.
    Truncated,
    /// Not a coded slice NAL (type 1 or 5).
    NotASlice,
    /// A syntax element outside what this module models (B/SP/SI slices,
    /// weighted prediction, a malformed `ue(v)`).
    Unsupported(&'static str),
}

struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn bit(&mut self) -> Result<bool, ParseError> {
        let byte = self.bytes.get(self.pos / 8).ok_or(ParseError::Truncated)?;
        let bit = byte & (0x80 >> (self.pos % 8)) != 0;
        self.pos += 1;
        Ok(bit)
    }

    fn bits(&mut self, n: u32) -> Result<u64, ParseError> {
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 1) | u64::from(self.bit()?);
        }
        Ok(v)
    }

    fn ue(&mut self) -> Result<u32, ParseError> {
        let mut zeros = 0;
        while !self.bit()? {
            zeros += 1;
            if zeros > 32 {
                return Err(ParseError::Unsupported("ue(v) longer than 32 bits"));
            }
        }
        let rest = self.bits(zeros)?;
        u32::try_from((1u64 << zeros) - 1 + rest)
            .map_err(|_| ParseError::Unsupported("ue(v) overflows u32"))
    }

    fn se(&mut self) -> Result<i32, ParseError> {
        let k = self.ue()?;
        let magnitude = i32::try_from(k.div_ceil(2))
            .map_err(|_| ParseError::Unsupported("se(v) overflows i32"))?;
        Ok(if k % 2 == 1 { magnitude } else { -magnitude })
    }
}

// ------------------------------------------------------------- NAL helpers

pub(crate) const NAL_SLICE: u8 = 1;
pub(crate) const NAL_IDR: u8 = 5;

/// Insert emulation-prevention bytes into an RBSP.
pub(crate) fn escape_rbsp(rbsp: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rbsp.len() + 4);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Remove emulation-prevention bytes.
pub(crate) fn unescape_nal(nal: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(nal.len());
    let mut zeros = 0;
    for &b in nal {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Split an Annex-B stream into NAL units (start codes removed, trailing
/// zero bytes of a unit trimmed).
pub(crate) fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut nals = Vec::new();
    for (n, &s) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(data.len(), |&next| next - 3);
        while end > s && data[end - 1] == 0 {
            end -= 1;
        }
        if end > s {
            nals.push(&data[s..end]);
        }
    }
    nals
}

// ------------------------------------------------------------------ writer

/// The header bits of a slice (RBSP, no NAL header), unescaped and not
/// byte-aligned: `bit_len` says how many bits of `bytes` are the header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HeaderBits {
    pub bytes: Vec<u8>,
    pub bit_len: usize,
}

/// Write the slice header up to and including the deblocking fields.
pub(crate) fn write_slice_header(params: &StreamParams, h: &SliceHeader) -> HeaderBits {
    let mut w = BitWriter::default();
    w.put_ue(h.first_mb_in_slice);
    w.put_ue(match h.slice_type {
        SliceType::P => 0,
        SliceType::I => 2,
    });
    w.put_ue(h.pps_id);
    w.put_bits(u64::from(h.frame_num), params.log2_max_frame_num);
    if h.idr {
        w.put_ue(h.idr_pic_id);
    }
    w.put_bits(u64::from(h.poc_lsb), params.log2_max_poc_lsb);
    if h.slice_type == SliceType::P {
        w.put_bit(h.num_ref_idx_override.is_some());
        if let Some(n) = h.num_ref_idx_override {
            w.put_ue(n - 1);
        }
        w.put_bit(!h.rplm.is_empty());
        if !h.rplm.is_empty() {
            for op in &h.rplm {
                match *op {
                    Rplm::Sub(v) => {
                        w.put_ue(0);
                        w.put_ue(v);
                    }
                    Rplm::Add(v) => {
                        w.put_ue(1);
                        w.put_ue(v);
                    }
                    Rplm::Long(v) => {
                        w.put_ue(2);
                        w.put_ue(v);
                    }
                }
            }
            w.put_ue(3);
        }
    }
    if h.nal_ref_idc != 0 {
        if h.idr {
            w.put_bit(h.no_output_of_prior_pics);
            w.put_bit(h.long_term_reference_flag);
        } else {
            w.put_bit(h.mmco.is_some());
            if let Some(ops) = &h.mmco {
                for op in ops {
                    match *op {
                        Mmco::ForgetShort(d) => {
                            w.put_ue(1);
                            w.put_ue(d);
                        }
                        Mmco::ForgetLong(n) => {
                            w.put_ue(2);
                            w.put_ue(n);
                        }
                        Mmco::ShortToLong(d, idx) => {
                            w.put_ue(3);
                            w.put_ue(d);
                            w.put_ue(idx);
                        }
                        Mmco::MaxLong(p) => {
                            w.put_ue(4);
                            w.put_ue(p);
                        }
                        Mmco::ClearAll => w.put_ue(5),
                        Mmco::CurrentToLong(idx) => {
                            w.put_ue(6);
                            w.put_ue(idx);
                        }
                    }
                }
                w.put_ue(0);
            }
        }
    }
    if params.cabac && h.slice_type == SliceType::P {
        w.put_ue(h.cabac_init_idc);
    }
    w.put_se(h.slice_qp_delta);
    if params.deblocking_control_present {
        w.put_ue(h.deblock.disable_idc);
        if h.deblock.disable_idc != 1 {
            w.put_se(h.deblock.alpha_c0_offset_div2);
            w.put_se(h.deblock.beta_offset_div2);
        }
    }
    let bit_len = w.bit_len();
    HeaderBits {
        bytes: w.into_bytes(),
        bit_len,
    }
}

/// The NAL unit (header byte plus escaped RBSP, no start code) for `h`
/// followed by `slice_data` (already byte-aligned by the caller's writer;
/// the header is zero-padded to a byte boundary first, which is only valid
/// for CAVLC streams whose slice data starts byte-aligned).
#[cfg(test)]
pub(crate) fn slice_nal(params: &StreamParams, h: &SliceHeader) -> Vec<u8> {
    let hb = write_slice_header(params, h);
    let nal_type = if h.idr { NAL_IDR } else { NAL_SLICE };
    let mut nal = vec![(h.nal_ref_idc << 5) | nal_type];
    nal.extend(escape_rbsp(&hb.bytes));
    nal
}

// ------------------------------------------------------------------ parser

/// Parse a slice NAL unit (header byte, escaped RBSP, no start code).
/// Returns the header and the number of RBSP bits it occupies.
pub(crate) fn parse_slice_header(
    params: &StreamParams,
    nal: &[u8],
) -> Result<(SliceHeader, usize), ParseError> {
    let first = *nal.first().ok_or(ParseError::Truncated)?;
    let nal_ref_idc = (first >> 5) & 3;
    let idr = match first & 0x1f {
        NAL_IDR => true,
        NAL_SLICE => false,
        _ => return Err(ParseError::NotASlice),
    };
    let rbsp = unescape_nal(&nal[1..]);
    let mut r = BitReader {
        bytes: &rbsp,
        pos: 0,
    };
    let first_mb_in_slice = r.ue()?;
    let slice_type = match r.ue()? {
        0 | 5 => SliceType::P,
        2 | 7 => SliceType::I,
        _ => return Err(ParseError::Unsupported("slice type")),
    };
    let pps_id = r.ue()?;
    let frame_num = r.bits(params.log2_max_frame_num)? as u32;
    let idr_pic_id = if idr { r.ue()? } else { 0 };
    let poc_lsb = r.bits(params.log2_max_poc_lsb)? as u32;
    let mut num_ref_idx_override = None;
    let mut rplm = Vec::new();
    if slice_type == SliceType::P {
        if r.bit()? {
            num_ref_idx_override = Some(r.ue()? + 1);
        }
        if r.bit()? {
            loop {
                let idc = r.ue()?;
                match idc {
                    0 => rplm.push(Rplm::Sub(r.ue()?)),
                    1 => rplm.push(Rplm::Add(r.ue()?)),
                    2 => rplm.push(Rplm::Long(r.ue()?)),
                    3 => break,
                    _ => return Err(ParseError::Unsupported("modification_of_pic_nums_idc")),
                }
                if rplm.len() > 33 {
                    return Err(ParseError::Unsupported("too many RPLM operations"));
                }
            }
        }
    }
    let mut no_output_of_prior_pics = false;
    let mut long_term_reference_flag = false;
    let mut mmco = None;
    if nal_ref_idc != 0 {
        if idr {
            no_output_of_prior_pics = r.bit()?;
            long_term_reference_flag = r.bit()?;
        } else if r.bit()? {
            let mut ops = Vec::new();
            loop {
                let op = match r.ue()? {
                    0 => break,
                    1 => Mmco::ForgetShort(r.ue()?),
                    2 => Mmco::ForgetLong(r.ue()?),
                    3 => {
                        let d = r.ue()?;
                        Mmco::ShortToLong(d, r.ue()?)
                    }
                    4 => Mmco::MaxLong(r.ue()?),
                    5 => Mmco::ClearAll,
                    6 => Mmco::CurrentToLong(r.ue()?),
                    _ => {
                        return Err(ParseError::Unsupported(
                            "memory_management_control_operation",
                        ))
                    }
                };
                ops.push(op);
                if ops.len() > 66 {
                    return Err(ParseError::Unsupported("too many MMCO operations"));
                }
            }
            mmco = Some(ops);
        }
    }
    let cabac_init_idc = if params.cabac && slice_type == SliceType::P {
        r.ue()?
    } else {
        0
    };
    let slice_qp_delta = r.se()?;
    let mut deblock = Deblock {
        disable_idc: 0,
        alpha_c0_offset_div2: 0,
        beta_offset_div2: 0,
    };
    if params.deblocking_control_present {
        deblock.disable_idc = r.ue()?;
        if deblock.disable_idc != 1 {
            deblock.alpha_c0_offset_div2 = r.se()?;
            deblock.beta_offset_div2 = r.se()?;
        }
    }
    Ok((
        SliceHeader {
            nal_ref_idc,
            idr,
            slice_type,
            first_mb_in_slice,
            pps_id,
            frame_num,
            idr_pic_id,
            poc_lsb,
            num_ref_idx_override,
            rplm,
            no_output_of_prior_pics,
            long_term_reference_flag,
            mmco,
            cabac_init_idc,
            slice_qp_delta,
            deblock,
        },
        r.pos,
    ))
}

/// The header of the first slice in an Annex-B access unit.
pub(crate) fn parse_access_unit(
    params: &StreamParams,
    annexb: &[u8],
) -> Result<SliceHeader, ParseError> {
    for nal in split_annexb(annexb) {
        match parse_slice_header(params, nal) {
            Err(ParseError::NotASlice) => continue,
            other => return other.map(|(h, _)| h),
        }
    }
    Err(ParseError::NotASlice)
}

// ------------------------------------------------- effective-reference model

/// What a reference slot holds, as a decoder knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RefPic {
    /// A picture the caller tagged (the plan's `frame_seq`).
    Real(u32),
    /// A "non-existing" frame the gap process inserted: its content is
    /// whatever the decoder invents, never a valid reference.
    NonExisting,
    /// No picture: the list is shorter than asked or the named one is not in
    /// the DPB.
    Missing,
}

/// Rule violations the model noticed while replaying a stream. A stream the
/// manager planned must produce none when the model sees every frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Violations {
    /// More reference frames than `max_num_ref_frames` after marking.
    pub dpb_overflow: bool,
    /// `long_term_frame_idx` at or above MaxLongTermFrameIdx + 1.
    pub lt_idx_out_of_range: bool,
    /// The sliding window had no short-term picture to remove.
    pub sliding_window_empty: bool,
    /// An RPLM or MMCO named a picture that is not there.
    pub bad_pic_num: bool,
    /// A `frame_num` gap with gaps not allowed.
    pub gap_not_allowed: bool,
    /// MMCO 5 (never emitted, not modelled further).
    pub mmco5: bool,
}

impl Violations {
    pub(crate) fn any(&self) -> bool {
        *self != Self::default()
    }
}

/// The result of replaying one slice header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Effective {
    /// Entry 0 of the final RefPicList0 for a P slice, `None` for I slices.
    pub reference: Option<RefPic>,
    /// The whole final list (`num_ref_idx_active` entries).
    pub list0: Vec<RefPic>,
    /// Picture order count (frame POC, msb inferred).
    pub poc: i64,
    /// Reference frames resident after this picture's marking.
    pub dpb_len: usize,
    pub violations: Violations,
}

impl Effective {
    /// True when the slice really predicts from the picture `seq`.
    pub(crate) fn references(&self, seq: u32) -> bool {
        self.reference == Some(RefPic::Real(seq))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModelPic {
    pic: RefPic,
    frame_num: u32,
    lt_idx: Option<u32>,
}

/// A decoder's reference state. Feed it the slice headers a decoder would be
/// given, in order, each tagged with the caller's picture id. Frames the
/// client never fed (lost, refused) are simply not pushed; the resulting
/// `frame_num` gaps are modelled.
#[derive(Debug, Clone)]
pub(crate) struct DecoderModel {
    params: StreamParams,
    pics: Vec<ModelPic>,
    prev_ref_frame_num: u32,
    prev_poc_msb: i64,
    prev_poc_lsb: u32,
    /// MaxLongTermFrameIdx + 1; 0 is "no long-term frame indices".
    max_lt_idx_plus1: u32,
}

impl DecoderModel {
    pub(crate) fn new(params: StreamParams) -> Self {
        Self {
            params,
            pics: Vec::new(),
            prev_ref_frame_num: 0,
            prev_poc_msb: 0,
            prev_poc_lsb: 0,
            max_lt_idx_plus1: 0,
        }
    }

    /// What `h` would reference, without changing the model.
    pub(crate) fn peek(&self, tag: u32, h: &SliceHeader) -> Effective {
        self.clone().push(tag, h)
    }

    /// The picture resident in long-term slot `idx`.
    pub(crate) fn long_term(&self, idx: u32) -> Option<RefPic> {
        self.pics
            .iter()
            .find(|p| p.lt_idx == Some(idx))
            .map(|p| p.pic)
    }

    /// Short-term pictures, newest first by the order they were added.
    pub(crate) fn short_term(&self) -> Vec<RefPic> {
        self.pics
            .iter()
            .filter(|p| p.lt_idx.is_none())
            .map(|p| p.pic)
            .collect()
    }

    pub(crate) fn dpb_len(&self) -> usize {
        self.pics.len()
    }

    fn frame_num_wrap(&self, frame_num: u32, cur: u32) -> i64 {
        if frame_num > cur {
            i64::from(frame_num) - i64::from(self.params.max_frame_num())
        } else {
            i64::from(frame_num)
        }
    }

    /// Sliding window (8.2.5.3), run before a non-IDR reference picture is
    /// added without adaptive marking, and for each gap frame.
    fn sliding_window(&mut self, cur_frame_num: u32, v: &mut Violations) {
        if self.pics.len() as u32 >= self.params.max_num_ref_frames.max(1) {
            let oldest = self
                .pics
                .iter()
                .enumerate()
                .filter(|(_, p)| p.lt_idx.is_none())
                .min_by_key(|(_, p)| self.frame_num_wrap(p.frame_num, cur_frame_num))
                .map(|(i, _)| i);
            match oldest {
                Some(i) => {
                    self.pics.remove(i);
                }
                None => v.sliding_window_empty = true,
            }
        }
    }

    fn find_short(&self, pic_num: i64, cur: u32) -> Option<usize> {
        self.pics
            .iter()
            .position(|p| p.lt_idx.is_none() && self.frame_num_wrap(p.frame_num, cur) == pic_num)
    }

    pub(crate) fn push(&mut self, tag: u32, h: &SliceHeader) -> Effective {
        let mut v = Violations::default();
        let max_fn = self.params.max_frame_num();
        let max_lsb = 1i64 << self.params.log2_max_poc_lsb;

        if h.idr {
            self.pics.clear();
            self.prev_ref_frame_num = 0;
            self.prev_poc_msb = 0;
            self.prev_poc_lsb = 0;
        } else if h.frame_num != self.prev_ref_frame_num
            && h.frame_num != (self.prev_ref_frame_num + 1) % max_fn
        {
            if self.params.gaps_in_frame_num_allowed {
                let mut unused = (self.prev_ref_frame_num + 1) % max_fn;
                while unused != h.frame_num {
                    self.sliding_window(unused, &mut v);
                    self.pics.push(ModelPic {
                        pic: RefPic::NonExisting,
                        frame_num: unused,
                        lt_idx: None,
                    });
                    self.prev_ref_frame_num = unused;
                    unused = (unused + 1) % max_fn;
                }
            } else {
                v.gap_not_allowed = true;
            }
        }

        // POC type 0 (8.2.1.1).
        let lsb = i64::from(h.poc_lsb);
        let prev_lsb = i64::from(self.prev_poc_lsb);
        let msb = if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
            self.prev_poc_msb + max_lsb
        } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
            self.prev_poc_msb - max_lsb
        } else {
            self.prev_poc_msb
        };
        let poc = msb + lsb;

        // Reference list for P slices (8.2.4).
        let cur = h.frame_num;
        let mut reference = None;
        let mut list0 = Vec::new();
        if h.slice_type == SliceType::P {
            let n = h
                .num_ref_idx_override
                .unwrap_or(self.params.num_ref_idx_l0_default) as usize;
            let mut short: Vec<usize> = (0..self.pics.len())
                .filter(|&i| self.pics[i].lt_idx.is_none())
                .collect();
            short.sort_by_key(|&i| {
                std::cmp::Reverse(self.frame_num_wrap(self.pics[i].frame_num, cur))
            });
            let mut long: Vec<usize> = (0..self.pics.len())
                .filter(|&i| self.pics[i].lt_idx.is_some())
                .collect();
            long.sort_by_key(|&i| self.pics[i].lt_idx);
            let mut list: Vec<Option<usize>> = short.into_iter().chain(long).map(Some).collect();
            list.truncate(n);
            list.resize(n, None);

            let mut pred = i64::from(cur);
            let mut idx = 0usize;
            for op in &h.rplm {
                let found = match *op {
                    Rplm::Sub(m) | Rplm::Add(m) => {
                        let abs = i64::from(m) + 1;
                        let max = i64::from(max_fn);
                        let no_wrap = if matches!(op, Rplm::Sub(_)) {
                            if pred - abs < 0 {
                                pred - abs + max
                            } else {
                                pred - abs
                            }
                        } else if pred + abs >= max {
                            pred + abs - max
                        } else {
                            pred + abs
                        };
                        pred = no_wrap;
                        let pic_num = if no_wrap > i64::from(cur) {
                            no_wrap - max
                        } else {
                            no_wrap
                        };
                        self.find_short(pic_num, cur)
                    }
                    Rplm::Long(num) => self.pics.iter().position(|p| p.lt_idx == Some(num)),
                };
                if found.is_none() {
                    v.bad_pic_num = true;
                }
                if idx < n {
                    list.insert(idx, found);
                    idx += 1;
                    if let Some(f) = found {
                        let tail: Vec<_> = list[idx..]
                            .iter()
                            .copied()
                            .filter(|e| *e != Some(f))
                            .collect();
                        list.truncate(idx);
                        list.extend(tail);
                    }
                    list.truncate(n);
                    list.resize(n, None);
                }
            }
            list0 = list
                .iter()
                .map(|e| e.map_or(RefPic::Missing, |i| self.pics[i].pic))
                .collect();
            reference = list0.first().copied();
        }

        // Marking (8.2.5).
        let mut cur_frame_num = h.frame_num;
        if h.idr {
            let lt = h.nal_ref_idc != 0 && h.long_term_reference_flag;
            self.max_lt_idx_plus1 = u32::from(lt);
            self.pics.push(ModelPic {
                pic: RefPic::Real(tag),
                frame_num: 0,
                lt_idx: lt.then_some(0),
            });
        } else if h.nal_ref_idc != 0 {
            let mut cur_lt = None;
            match &h.mmco {
                None => self.sliding_window(cur, &mut v),
                Some(ops) => {
                    for op in ops {
                        match *op {
                            Mmco::ForgetShort(d) => {
                                let pic_num = i64::from(cur) - (i64::from(d) + 1);
                                match self.find_short(pic_num, cur) {
                                    Some(i) => {
                                        self.pics.remove(i);
                                    }
                                    None => v.bad_pic_num = true,
                                }
                            }
                            Mmco::ForgetLong(n) => {
                                match self.pics.iter().position(|p| p.lt_idx == Some(n)) {
                                    Some(i) => {
                                        self.pics.remove(i);
                                    }
                                    None => v.bad_pic_num = true,
                                }
                            }
                            Mmco::ShortToLong(d, idx) => {
                                let pic_num = i64::from(cur) - (i64::from(d) + 1);
                                if idx >= self.max_lt_idx_plus1 {
                                    v.lt_idx_out_of_range = true;
                                }
                                self.pics.retain(|p| p.lt_idx != Some(idx));
                                match self.find_short(pic_num, cur) {
                                    Some(j) => self.pics[j].lt_idx = Some(idx),
                                    None => v.bad_pic_num = true,
                                }
                            }
                            Mmco::MaxLong(p) => {
                                self.max_lt_idx_plus1 = p;
                                self.pics.retain(|pic| pic.lt_idx.is_none_or(|i| i < p));
                            }
                            Mmco::ClearAll => {
                                v.mmco5 = true;
                                self.pics.clear();
                                cur_frame_num = 0;
                            }
                            Mmco::CurrentToLong(idx) => {
                                if idx >= self.max_lt_idx_plus1 {
                                    v.lt_idx_out_of_range = true;
                                }
                                self.pics.retain(|p| p.lt_idx != Some(idx));
                                cur_lt = Some(idx);
                            }
                        }
                    }
                }
            }
            self.pics.push(ModelPic {
                pic: RefPic::Real(tag),
                frame_num: cur_frame_num,
                lt_idx: cur_lt,
            });
            if self.pics.len() as u32 > self.params.max_num_ref_frames {
                // Non-conforming. Report it, then do what libavcodec does:
                // discard the oldest short-term picture.
                v.dpb_overflow = true;
                let oldest = (0..self.pics.len())
                    .filter(|&i| self.pics[i].lt_idx.is_none())
                    .min_by_key(|&i| self.frame_num_wrap(self.pics[i].frame_num, cur));
                if let Some(i) = oldest {
                    self.pics.remove(i);
                }
            }
        }
        if h.nal_ref_idc != 0 {
            self.prev_ref_frame_num = cur_frame_num;
            self.prev_poc_msb = msb;
            self.prev_poc_lsb = if v.mmco5 { 0 } else { h.poc_lsb };
        }

        Effective {
            reference,
            list0,
            poc,
            dpb_len: self.pics.len(),
            violations: v,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn params() -> StreamParams {
        StreamParams::default()
    }

    fn p_ref(frame_num: u32, poc_lsb: u32) -> SliceHeader {
        SliceHeader::p(frame_num, poc_lsb, 2)
    }

    /// An IDR into LT 0 followed by `n` plain reference P frames (tags 1..=n,
    /// frame_num 1..=n). Returns the model.
    fn after_idr_and_ps(n: u32) -> DecoderModel {
        let mut m = DecoderModel::new(params());
        m.push(0, &SliceHeader::idr(1, true));
        for i in 1..=n {
            let mut h = p_ref(i % 128, 2 * i);
            h.rplm = vec![if i == 1 { Rplm::Long(0) } else { Rplm::Sub(0) }];
            let e = m.push(i, &h);
            assert!(!e.violations.any(), "frame {i}: {:?}", e.violations);
        }
        m
    }

    proptest! {
        #[test]
        fn ue_se_round_trip(u in any::<u32>(), s in any::<i32>().prop_filter("i32::MIN has no se(v)", |v| *v != i32::MIN)) {
            let mut w = BitWriter::default();
            w.put_ue(u);
            w.put_se(s);
            w.put_bits(0b101, 3);
            let bytes = w.into_bytes();
            let mut r = BitReader { bytes: &bytes, pos: 0 };
            prop_assert_eq!(r.ue().unwrap(), u);
            prop_assert_eq!(r.se().unwrap(), s);
            prop_assert_eq!(r.bits(3).unwrap(), 0b101);
        }

        #[test]
        fn escape_round_trip(data in proptest::collection::vec(prop_oneof![Just(0u8), Just(1u8), Just(3u8), any::<u8>()], 0..64)) {
            let esc = escape_rbsp(&data);
            // No start-code emulation survives.
            prop_assert!(!esc.windows(3).any(|w| w[0] == 0 && w[1] == 0 && w[2] <= 2));
            prop_assert_eq!(unescape_nal(&esc), data);
        }

        #[test]
        fn header_round_trip(
            idr in any::<bool>(),
            lt_flag in any::<bool>(),
            frame_num in 0u32..128,
            poc_lsb in 0u32..65536,
            ref_idc in 0u8..4,
            override_n in proptest::option::of(1u32..5),
            rplm in proptest::collection::vec(prop_oneof![
                (0u32..200).prop_map(Rplm::Sub),
                (0u32..200).prop_map(Rplm::Add),
                (0u32..4).prop_map(Rplm::Long),
            ], 0..4),
            mmco in proptest::option::of(proptest::collection::vec(prop_oneof![
                (0u32..8).prop_map(Mmco::ForgetShort),
                (0u32..4).prop_map(Mmco::ForgetLong),
                (0u32..8, 0u32..4).prop_map(|(d, i)| Mmco::ShortToLong(d, i)),
                (0u32..4).prop_map(Mmco::MaxLong),
                Just(Mmco::ClearAll),
                (0u32..4).prop_map(Mmco::CurrentToLong),
            ], 0..5)),
            qp in -20i32..20,
            dis in 0u32..3,
            cabac in any::<bool>(),
            deblock_ctl in any::<bool>(),
            idr_id in 0u32..65536,
        ) {
            let p = StreamParams { cabac, deblocking_control_present: deblock_ctl, ..params() };
            let ref_idc = if idr { ref_idc.max(1) } else { ref_idc };
            let mut h = if idr { SliceHeader::idr(idr_id, lt_flag) } else { SliceHeader::p(frame_num, poc_lsb, ref_idc) };
            h.nal_ref_idc = ref_idc;
            h.poc_lsb = poc_lsb;
            h.slice_qp_delta = qp;
            if !idr {
                h.num_ref_idx_override = override_n;
                h.rplm = rplm;
                h.mmco = if ref_idc != 0 { mmco } else { None };
                h.cabac_init_idc = if cabac { 1 } else { 0 };
            }
            h.no_output_of_prior_pics = idr && lt_flag;
            h.deblock = if deblock_ctl {
                Deblock { disable_idc: dis, alpha_c0_offset_div2: if dis == 1 { 0 } else { -2 }, beta_offset_div2: if dis == 1 { 0 } else { 3 } }
            } else {
                Deblock { disable_idc: 0, alpha_c0_offset_div2: 0, beta_offset_div2: 0 }
            };
            if !h.idr { h.idr_pic_id = 0; }
            if h.idr && ref_idc == 0 { h.long_term_reference_flag = false; }
            if !h.idr || ref_idc == 0 { h.long_term_reference_flag = false; h.no_output_of_prior_pics = false; }
            let hb = write_slice_header(&p, &h);
            let nal = slice_nal(&p, &h);
            let (parsed, bits) = parse_slice_header(&p, &nal).unwrap();
            prop_assert_eq!(parsed, h);
            prop_assert_eq!(bits, hb.bit_len);
        }
    }

    #[test]
    fn escape_known_vector() {
        assert_eq!(escape_rbsp(&[0, 0, 1]), vec![0, 0, 3, 1]);
        assert_eq!(escape_rbsp(&[0, 0, 0]), vec![0, 0, 3, 0]);
        assert_eq!(escape_rbsp(&[0, 0, 4]), vec![0, 0, 4]);
        assert_eq!(unescape_nal(&[0, 0, 3, 1]), vec![0, 0, 1]);
    }

    #[test]
    fn annexb_split_and_access_unit() {
        let p = params();
        let mut idr = SliceHeader::idr(7, true);
        idr.slice_qp_delta = -3;
        let mut au = vec![0, 0, 0, 1, 0x67, 1, 2, 3]; // an SPS-like NAL first
        au.extend([0, 0, 0, 1]);
        au.extend(slice_nal(&p, &idr));
        let nals = split_annexb(&au);
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0][0] & 0x1f, 7);
        assert_eq!(parse_access_unit(&p, &au).unwrap(), idr);
        assert_eq!(
            parse_access_unit(&p, &[0, 0, 1, 0x67, 1]),
            Err(ParseError::NotASlice)
        );
    }

    #[test]
    fn parser_rejects_truncated_and_foreign_input() {
        let p = params();
        let nal = slice_nal(&p, &SliceHeader::idr(0, true));
        assert_eq!(
            parse_slice_header(&p, &nal[..2]).unwrap_err(),
            ParseError::Truncated
        );
        assert_eq!(
            parse_slice_header(&p, &[]).unwrap_err(),
            ParseError::Truncated
        );
        assert_eq!(
            parse_slice_header(&p, &[0x67, 0x80]).unwrap_err(),
            ParseError::NotASlice
        );
        // slice_type 1 (B) is out of scope.
        let mut w = BitWriter::default();
        w.put_ue(0);
        w.put_ue(1);
        let mut b = vec![0x41];
        b.extend(w.into_bytes());
        assert!(matches!(
            parse_slice_header(&p, &b),
            Err(ParseError::Unsupported(_))
        ));
    }

    #[test]
    fn idr_with_lt_flag_leaves_only_slot_zero_until_mmco4() {
        let mut m = DecoderModel::new(params());
        m.push(0, &SliceHeader::idr(1, true));
        assert_eq!(m.long_term(0), Some(RefPic::Real(0)));
        let mut mark_without_4 = p_ref(1, 2);
        mark_without_4.mmco = Some(vec![Mmco::CurrentToLong(1)]);
        assert!(m.peek(1, &mark_without_4).violations.lt_idx_out_of_range);
        let mut mark = p_ref(1, 2);
        mark.mmco = Some(vec![Mmco::MaxLong(2), Mmco::CurrentToLong(1)]);
        let e = m.push(1, &mark);
        assert!(!e.violations.any());
        assert_eq!(m.long_term(1), Some(RefPic::Real(1)));
        assert_eq!(m.long_term(0), Some(RefPic::Real(0)));
    }

    #[test]
    fn default_list_after_mmco6_names_the_older_short_term() {
        // The reason every P carries RPLM: after N is marked long-term, the
        // default list0[0] is the newest short-term picture, N-1.
        let mut m = after_idr_and_ps(4);
        let mut mark = p_ref(5, 10);
        mark.rplm = vec![Rplm::Sub(0)];
        mark.mmco = Some(vec![
            Mmco::MaxLong(2),
            Mmco::ForgetShort(2),
            Mmco::CurrentToLong(1),
        ]);
        let e = m.push(5, &mark);
        assert!(!e.violations.any(), "{:?}", e.violations);
        assert_eq!(m.long_term(1), Some(RefPic::Real(5)));

        let bare = p_ref(6, 12);
        assert!(
            m.peek(6, &bare).references(4),
            "default list0[0] is the older short-term"
        );
        let mut explicit = bare.clone();
        explicit.rplm = vec![Rplm::Long(1)];
        assert!(m.peek(6, &explicit).references(5));
    }

    #[test]
    fn rplm_short_term_across_the_frame_num_wrap() {
        let mut m = DecoderModel::new(params());
        m.push(0, &SliceHeader::idr(0, false));
        // Walk frame_num 1..=127, then 0, 1, 2 (tags equal the running count).
        let mut tag = 0;
        for k in 1..=130u32 {
            tag += 1;
            m.push(tag, &p_ref(k % 128, 2 * k));
        }
        // Now resident short-term: tags 127..=130 (frame_num 127, 0, 1, 2).
        let cur = 131 % 128;
        // Target tag 128 (frame_num 0): diff 3.
        let mut h = p_ref(cur, 262);
        h.rplm = vec![Rplm::Sub(2)];
        assert!(m.peek(200, &h).references(128));
        // Target tag 127 (frame_num 127, FrameNumWrap -1): diff 4.
        h.rplm = vec![Rplm::Sub(3)];
        assert!(m.peek(200, &h).references(127));
    }

    #[test]
    fn frame_num_gap_inserts_non_existing_frames_and_spares_long_term() {
        let mut m = after_idr_and_ps(2);
        // Frames 3..=5 never reach the decoder; frame_num 6 arrives.
        let mut h = p_ref(6, 12);
        h.rplm = vec![Rplm::Sub(0)]; // frame_num 5: a non-existing frame
        let e = m.push(6, &h);
        assert_eq!(e.reference, Some(RefPic::NonExisting));
        assert_eq!(
            m.long_term(0),
            Some(RefPic::Real(0)),
            "the IDR in LT 0 survives the gap"
        );
        // Real tags 1 and 2 were pushed out of the window by the gap frames.
        let mut h = p_ref(7, 14);
        h.rplm = vec![Rplm::Long(0)];
        assert!(m.peek(7, &h).references(0));
        assert!(m.short_term().iter().all(|p| *p != RefPic::Real(1)));
        assert!(m.dpb_len() <= 4);
    }

    #[test]
    fn gap_not_allowed_is_flagged() {
        let mut m = DecoderModel::new(StreamParams {
            gaps_in_frame_num_allowed: false,
            ..params()
        });
        m.push(0, &SliceHeader::idr(0, false));
        assert!(m.push(1, &p_ref(5, 10)).violations.gap_not_allowed);
    }

    #[test]
    fn non_reference_frames_do_not_advance_frame_num() {
        let mut m = after_idr_and_ps(1);
        // Two non-ref frames sharing frame_num 2, then a ref frame with it.
        let mut nr = SliceHeader::p(2, 4, 0);
        nr.rplm = vec![Rplm::Long(0)];
        for tag in [10, 11] {
            let e = m.push(tag, &nr);
            assert!(!e.violations.any());
            assert!(e.references(0));
        }
        let e = m.push(12, &p_ref(2, 6));
        assert!(!e.violations.any());
        assert_eq!(m.short_term(), vec![RefPic::Real(1), RefPic::Real(12)]);
    }

    #[test]
    fn sliding_window_drops_oldest_short_term_not_long_term() {
        let m = after_idr_and_ps(6);
        assert_eq!(m.long_term(0), Some(RefPic::Real(0)));
        assert_eq!(
            m.short_term(),
            vec![RefPic::Real(4), RefPic::Real(5), RefPic::Real(6)]
        );
    }

    #[test]
    fn overfull_marking_is_flagged() {
        let mut m = after_idr_and_ps(4); // LT0 + 3 ST = 4
        let mut mark = p_ref(5, 10);
        mark.mmco = Some(vec![Mmco::MaxLong(2), Mmco::CurrentToLong(1)]);
        let e = m.push(5, &mark);
        assert!(e.violations.dpb_overflow);
        assert!(e.dpb_len <= 4, "the overflow is reported and then repaired");
    }

    #[test]
    fn mmco_variants_move_pictures() {
        let mut m = after_idr_and_ps(3); // LT0, ST 1,2,3
                                         // Promote tag 2 (frame_num 2) to LT 1, forget tag 1.
        let mut h = p_ref(4, 8);
        h.mmco = Some(vec![
            Mmco::MaxLong(2),
            Mmco::ShortToLong(1, 1),
            Mmco::ForgetShort(2),
        ]);
        let e = m.push(4, &h);
        assert!(!e.violations.any(), "{:?}", e.violations);
        assert_eq!(m.long_term(1), Some(RefPic::Real(2)));
        assert_eq!(m.short_term(), vec![RefPic::Real(3), RefPic::Real(4)]);
        // Forget LT 0.
        let mut h = p_ref(5, 10);
        h.mmco = Some(vec![Mmco::ForgetLong(0)]);
        m.push(5, &h);
        assert_eq!(m.long_term(0), None);
        // MaxLong(1) drops LT 1.
        let mut h = p_ref(6, 12);
        h.mmco = Some(vec![Mmco::MaxLong(1)]);
        m.push(6, &h);
        assert_eq!(m.long_term(1), None);
        // Naming a picture that is not there is flagged.
        let mut h = p_ref(7, 14);
        h.rplm = vec![Rplm::Long(1)];
        let e = m.push(7, &h);
        assert!(e.violations.bad_pic_num);
        assert_eq!(e.reference, Some(RefPic::Missing));
    }

    #[test]
    fn poc_survives_a_long_freeze_with_16_bit_lsb_and_not_with_8() {
        for (bits, survives) in [(16u32, true), (8, false)] {
            let p = StreamParams {
                log2_max_poc_lsb: bits,
                ..params()
            };
            let mut m = DecoderModel::new(p);
            m.push(0, &SliceHeader::idr(0, true));
            let mask = (1u32 << bits) - 1;
            // The client sees nothing for 100 frames (POC advances by 2 each),
            // then a P frame arrives.
            let mut h = p_ref(1, (2 * 101) & mask);
            h.rplm = vec![Rplm::Long(0)];
            let poc = m.push(1, &h).poc;
            assert_eq!(poc == 202, survives, "lsb bits {bits}: poc {poc}");
        }
    }
}
