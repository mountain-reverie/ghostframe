//! The H.264 encoder backend trait (design 4.1).
//!
//! A backend turns one NV12 picture and one [`FramePlan`] into one access
//! unit. Backends with `explicit_refs` (libva-direct) execute the plan:
//! the reference picture, the marking and the list modification are exactly
//! what the plan says, and the AU they return is parsed so the caller can
//! check it (`RefManager::frame_sent`). openh264 has its own reference
//! structure and runs in observed mode.

use thiserror::Error;

use super::refs::{FramePlan, ResidentRef};
use super::syntax::{SliceHeader, StreamParams};

#[derive(Debug, Error)]
pub(crate) enum BackendError {
    /// There is nothing to drive here: no libva, no render node, no encode
    /// entrypoint. The caller picks another backend, quietly.
    #[error("h264 backend unavailable: {0}")]
    Unavailable(String),
    /// A driver is there but cannot do what the stream model needs (a probe
    /// operation was ignored, a rate mode or packed headers are missing, the
    /// stream is outside what the writer expresses). The caller picks another
    /// backend, and it is worth a warning.
    #[error("h264 backend incapable: {0}")]
    Incapable(String),
    /// A driver call failed. The backend is failed from here on.
    #[error("h264 backend: {call} failed: {detail}")]
    Driver { call: &'static str, detail: String },
    /// The plan cannot be executed (a reference the backend does not hold,
    /// more references than the stream allows).
    #[error("h264 plan not executable: {0}")]
    Plan(String),
    /// The encoder produced bytes that do not parse as an access unit.
    #[error("h264 backend emitted an unparsable access unit")]
    Malformed,
    /// Not a fault: the encoder has taken the picture and has no output for
    /// it yet.
    #[error("h264 backend buffered the picture")]
    Buffered,
    #[error("h264 backend failed earlier")]
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RcMode {
    /// Rate-capped VBR: `bit_rate == max rate`. The only rate mode the
    /// stream model uses (CBR stuffs a static screen, S2 a).
    Vbr,
    /// Constant QP. For tests and for drivers without VBR.
    Cqp { qp: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BackendCaps {
    /// The backend executes the plan's references, marking and list
    /// modification. `false`: it runs on its own GOP and the plan is only
    /// observed.
    pub explicit_refs: bool,
    /// Long-term slots it can mark into.
    pub max_lt: u8,
    pub max_ref_frames: u8,
    pub vbr: bool,
}

/// A P frame may be this many average frames big (design 4.3).
pub(crate) const FRAME_CAP_FRAMES: u32 = 3;
/// The P-frame VBV holds at least this long at the rate.
pub(crate) const VBV_SECS: f64 = 0.1;
/// An IDR may take this long at the rate...
pub(crate) const IDR_BUDGET_SECS: f64 = 0.25;
/// ...but never less than this many bytes (unverified on the VCE, T4).
pub(crate) const IDR_BUDGET_FLOOR_BYTES: u32 = 32 * 1024;

/// What the stream is held to: the rate, a soft per-frame size cap and the
/// VBV. A change of any field is a change of the rate-control buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RateTarget {
    pub bps: u32,
    /// Cap on a P frame, bytes; 0 sends none. Soft on the VCE (S2).
    pub max_frame_bytes: u32,
    /// Cap on an IDR, bytes; 0 sends none.
    pub idr_max_bytes: u32,
    /// HRD buffer size for P frames, bits.
    pub vbv_bits: u32,
}

impl RateTarget {
    /// The steady-state target for `bps` at `fps`: cap = 3 average frames,
    /// VBV = max(0.1 s of the rate, one cap), IDR budget = max(0.25 s of the
    /// rate, 32 KB). Callers go through this so none can forget the VBV.
    pub(crate) fn steady(bps: u32, fps: u32) -> Self {
        let per_sec = u64::from(bps / 8);
        let cap = (per_sec * u64::from(FRAME_CAP_FRAMES) / u64::from(fps.max(1))) as u32;
        let vbv = ((f64::from(bps) * VBV_SECS) as u32).max(cap.saturating_mul(8));
        let idr = ((per_sec as f64 * IDR_BUDGET_SECS) as u32).max(IDR_BUDGET_FLOOR_BYTES);
        Self {
            bps,
            max_frame_bytes: cap,
            idr_max_bytes: idr,
            vbv_bits: vbv.max(1),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StreamConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rate: RateTarget,
    pub rc: RcMode,
    /// What the packed headers say.
    pub params: StreamParams,
}

impl StreamConfig {
    pub(crate) fn new(width: u32, height: u32, fps: u32, bps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            rate: RateTarget::steady(bps, fps),
            rc: RcMode::Vbr,
            params: StreamParams::writer(),
        }
    }
}

/// A borrowed NV12 picture: the Y plane at the start of `data`, the
/// interleaved UV plane at `uv_offset`.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Nv12View<'a> {
    pub data: &'a [u8],
    pub width: u32,
    pub height: u32,
    pub y_stride: usize,
    pub uv_stride: usize,
    pub uv_offset: usize,
}

impl<'a> Nv12View<'a> {
    pub(crate) fn y_row(&self, row: u32) -> &'a [u8] {
        let o = row as usize * self.y_stride;
        &self.data[o..o + self.width as usize]
    }

    pub(crate) fn uv_row(&self, row: u32) -> &'a [u8] {
        let o = self.uv_offset + row as usize * self.uv_stride;
        &self.data[o..o + self.width as usize]
    }

    /// Whether the buffer holds every row of both planes.
    pub(crate) fn is_complete(&self) -> bool {
        let y_end = (self.height as usize - 1) * self.y_stride + self.width as usize;
        let uv_rows = self.height.div_ceil(2) as usize;
        let uv_end = self.uv_offset + (uv_rows - 1) * self.uv_stride + self.width as usize;
        self.width > 0
            && self.height > 0
            && self.y_stride >= self.width as usize
            && self.uv_stride >= self.width as usize
            && self.uv_offset >= y_end
            && self.data.len() >= uv_end
    }
}

/// What the emitted access unit says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuMeta {
    pub idr: bool,
    /// The first slice header parsed from the stream the driver wrote, for
    /// `RefManager::frame_sent`. `None` for a backend whose stream cannot be
    /// parsed with our parameters (the legacy backend).
    pub header: Option<SliceHeader>,
    /// The picture the backend passed as `RefPicList0[0]`, by `frame_seq`:
    /// the backend's own DPB truth (design 3.3).
    pub ref_seq: Option<u32>,
    /// The stream parameters the backend's SPS/PPS carry, on an IDR access
    /// unit of a backend whose stream is its own (openh264). The reference
    /// manager adopts them and runs in observed mode. `None`: the stream is
    /// the one the manager's configured parameters describe.
    pub params: Option<StreamParams>,
    /// How long the backend took to code the frame, microseconds, as
    /// measured around the call (`encode_planned`); 0 where nothing coded it.
    pub encode_us: u32,
}

impl AuMeta {
    /// True when the emitted header carries the plan's references and
    /// marking: the fields a decoder follows. (The deblocking tail differs
    /// by design; the QP delta is the driver's.)
    pub(crate) fn header_matches(&self, plan: &FramePlan, params: &StreamParams) -> bool {
        let Some(got) = &self.header else {
            return false;
        };
        let want = plan.slice_header(params);
        got.idr == want.idr
            && got.nal_ref_idc == want.nal_ref_idc
            && got.frame_num == want.frame_num
            && got.poc_lsb == want.poc_lsb
            && got.rplm == want.rplm
            && got.mmco == want.mmco
            && got.long_term_reference_flag == want.long_term_reference_flag
            && self.ref_seq == plan.ref_frame_seq()
    }
}

#[derive(Debug)]
pub(crate) struct EncodedAu {
    /// Annex-B bytes: parameter sets first on an IDR.
    pub payload: Vec<u8>,
    pub meta: AuMeta,
}

/// What the stream's receiver (through the reference manager) tells a backend
/// that keeps its own long-term bookkeeping (openh264). The explicit-ref
/// backends ignore all of it. Frame numbers are H.264 `frame_num`s; an
/// `idr_pic_id` names the IDR period they belong to (a stale one is ignored).
#[allow(clippy::enum_variant_names)] // the names are openh264's LTR vocabulary
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BackendFeedback {
    /// The client decoded the carrier of a long-term mark. `frame_num` is the
    /// MARKED picture's number (for an MMCO 3 mark: the carrier's
    /// predecessor).
    LtrMarked { idr_pic_id: u32, frame_num: u32 },
    /// The carrier of a mark was lost or refused: the mark did not happen.
    /// Without this the library never marks again.
    LtrMarkFailed { idr_pic_id: u32, frame_num: u32 },
    /// Predict the next frame from a long-term picture the client holds.
    /// `last_correct_frame_num` is the CARRIER's `frame_num` (the picture that
    /// carried the mark: for an MMCO 3 mark, the marked picture's successor;
    /// the marked picture's own number is taken as an unknown reference and
    /// answered with an IDR). `current_frame_num` is the NEWEST `frame_num`
    /// encoded, not the oldest suspect: a request whose current is below the
    /// last recovery's is silently ignored.
    LtrRecover {
        idr_pic_id: u32,
        last_correct_frame_num: u32,
        current_frame_num: u32,
    },
}

pub(crate) trait H264Backend: Send {
    /// What the `h264 stats` line calls this backend.
    fn name(&self) -> &'static str;

    fn caps(&self) -> BackendCaps;

    fn open(&mut self, cfg: &StreamConfig) -> Result<(), BackendError>;

    /// Encode one picture. `refs` is the reference manager's resident set
    /// before this frame ([`RefManager::resident_refs`]): the backend lists
    /// every one of them on every frame (an unlisted reference is evicted by
    /// the driver after two frames) and finds the plan's target among them.
    ///
    /// [`RefManager::resident_refs`]: super::refs::RefManager::resident_refs
    fn encode(
        &mut self,
        nv12: &Nv12View<'_>,
        plan: &FramePlan,
        refs: &[ResidentRef],
    ) -> Result<EncodedAu, BackendError>;

    /// Change the rate in place. No IDR; takes effect on the next frame (a
    /// backend that cannot, applies it at the next IDR).
    fn set_rate(&mut self, rate: RateTarget);

    fn feedback(&mut self, fb: BackendFeedback);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_view_checks_both_planes() {
        let data = vec![0u8; 64 * 48 + 64 * 24];
        let v = Nv12View {
            data: &data,
            width: 64,
            height: 48,
            y_stride: 64,
            uv_stride: 64,
            uv_offset: 64 * 48,
        };
        assert!(v.is_complete());
        assert_eq!(v.y_row(47).len(), 64);
        assert_eq!(v.uv_row(23).len(), 64);
        assert!(!Nv12View {
            data: &data[..data.len() - 1],
            ..v
        }
        .is_complete());
        // The UV plane may not overlap the Y plane.
        assert!(!Nv12View { uv_offset: 64, ..v }.is_complete());
    }
}
