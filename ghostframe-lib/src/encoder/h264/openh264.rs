//! The openh264 backend (design 4.1, B7): Cisco's software encoder behind
//! [`H264Backend`], loaded at run time.
//!
//! openh264 runs its own reference structure (an LTR cycle it marks and
//! recovers on feedback) and the plan is only observed: `explicit_refs` is
//! `false`, only an IDR is asked for, and the access unit that comes back is
//! parsed for the reference manager to read what the library actually did.
//!
//! Settings that matter, each learned the hard way (see `docs/design/
//! h264-transport.md`): camera usage, because long-term references are
//! silently off under screen-content usage; scene-change detection off,
//! because it makes IDRs nobody asked for; frame skip off, because a skipped
//! picture breaks `frame_seq` <-> `frame_num`; `CONSTANT_ID` so SPS/PPS ids do
//! not alternate per IDR; one thread and one slice per picture, so an access
//! unit has one slice header.
//!
//! "OpenH264 Video Codec provided by Cisco Systems, Inc." Cisco's patent grant
//! covers only the binary Cisco distributes; a distribution-built library
//! carries none, and patent licensing is the operator's. The library is
//! neither shipped nor linked.

use std::ffi::c_int;
use std::path::PathBuf;

use super::backend::{
    AuMeta, BackendCaps, BackendError, BackendFeedback, EncodedAu, H264Backend, Nv12View,
    RateTarget, StreamConfig,
};
use super::openh264_sys::{
    Encoder, FrameBs, Lib, SBitrateInfo, SEncParamExt, SLtrMarkingFeedback, SLtrRecoverRequest,
    SSourcePicture, CAMERA_VIDEO_REAL_TIME, CISCO_ATTRIBUTION, CONSTANT_ID,
    ENCODER_LTR_MARKING_FEEDBACK, ENCODER_LTR_RECOVERY_REQUEST, ENCODER_OPTION_BITRATE,
    ENCODER_OPTION_DATAFORMAT, ENCODER_OPTION_MAX_BITRATE, LOW_COMPLEXITY, LTR_MARKING_FAILED,
    LTR_MARKING_SUCCESS, LTR_RECOVERY_REQUEST, RC_BITRATE_MODE, SM_SINGLE_SLICE, SPATIAL_LAYER_0,
    VIDEO_FORMAT_I420, VIDEO_FRAME_TYPE_IDR, VIDEO_FRAME_TYPE_INVALID, VIDEO_FRAME_TYPE_SKIP,
};
use super::refs::{FramePlan, ResidentRef};
use super::syntax::{parse_access_unit, StreamParams};
use super::tracker::PicKind;

const LOG_TARGET: &str = "ghostframe::h264";

/// Smallest picture openh264 encodes (64x64 produces nothing).
pub(crate) const MIN_DIM: u32 = 64;
/// D-B7-9: the largest picture openh264 is offered (single thread: 8 ms a P
/// frame at 1080p, 22-25 ms at 4K) and the highest frame rate submitted.
pub(crate) const MAX_PIXELS: u64 = 1920 * 1080;
pub(crate) const MAX_FPS: u32 = 30;
/// LTR marking period, frames. Retuned in B8.
const LTR_MARK_PERIOD: u32 = 30;
/// A rate change smaller than this fraction of the rate in force is not
/// passed on (each `SetOption` restarts the library's rate control).
const RATE_HYSTERESIS_DIV: u32 = 20;

/// The largest access unit believed: the library's output buffer bound.
const MAX_AU_BYTES: usize = 64 << 20;

/// Lowest QP the rate control may use. There is no per-frame size cap, and
/// after a quiet stretch the rate control is rich enough to code a scene
/// change of noise at QP 0: a frame bigger than the library's output buffer,
/// which fails `EncodeFrame` (rc 3) and costs the session. At 20 the same
/// frame is 744 KB at 720p and codes (see the heavy-scene test).
const MIN_QP: c_int = 20;

/// LOW complexity: 40 to 47 % faster than MEDIUM per P frame (2.7 against
/// 5.2 ms at 720p, 6.8 against 11.7 ms at 1080p, calm content), at the same
/// bytes and the same PSNR within 0.1 dB at 1 Mbit/s (calm 52.8 against
/// 52.7 dB, busy 38.76 against 38.82 dB; 720p, synthetic content). See the
/// `openh264_timing_probe` test.
const COMPLEXITY: c_int = LOW_COMPLEXITY;

/// What the backend counted, for tests.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Openh264Stats {
    pub frames: u64,
    pub idr_forced: u64,
    /// `SetOption(BITRATE)` calls made (each with a `MAX_BITRATE` twin).
    pub rate_sets: u64,
    pub feedback_applied: u64,
    pub feedback_rejected: u64,
}

struct Session {
    enc: Encoder,
    cfg: StreamConfig,
    frame: FrameBs,
    i420: Vec<u8>,
    /// What the library was last told, bits per second.
    rate_bps: u32,
    params: Option<StreamParams>,
    stats: Openh264Stats,
}

pub(crate) struct OpenH264Backend {
    lib_path: Option<PathBuf>,
    complexity: c_int,
    /// Test seam: leave scene-change detection on (the library default) to
    /// show the setting is load-bearing.
    #[cfg(test)]
    scene_change_detect: bool,
    session: Option<Session>,
    /// The rate last asked for, applied at the next frame.
    pending_rate: Option<RateTarget>,
    failed: bool,
}

impl OpenH264Backend {
    /// `lib_path`: the configured library (`GHOSTFRAME_H264_OPENH264_LIB`);
    /// `None` tries the system sonames.
    pub(crate) fn new(lib_path: Option<PathBuf>) -> Self {
        Self {
            lib_path,
            complexity: COMPLEXITY,
            #[cfg(test)]
            scene_change_detect: false,
            session: None,
            pending_rate: None,
            failed: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_complexity(mut self, c: c_int) -> Self {
        self.complexity = c;
        self
    }

    #[cfg(test)]
    pub(crate) fn with_scene_change_detect(mut self) -> Self {
        self.scene_change_detect = true;
        self
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub(crate) fn stats(&self) -> Openh264Stats {
        self.session.as_ref().map(|s| s.stats).unwrap_or_default()
    }

    /// The `SEncParamExt` a session opens with.
    fn params_for(
        enc: &mut Encoder,
        cfg: &StreamConfig,
        complexity: c_int,
        scene_change_detect: bool,
    ) -> Result<SEncParamExt, BackendError> {
        let mut p = enc.default_params().map_err(|rc| {
            BackendError::Incapable(format!("openh264 GetDefaultParams returned {rc}"))
        })?;
        let bps = i32::try_from(cfg.rate.bps.max(1)).unwrap_or(i32::MAX);
        let (w, h) = (cfg.width as c_int, cfg.height as c_int);
        let fps = cfg.fps.clamp(1, MAX_FPS) as f32;
        p.i_usage_type = CAMERA_VIDEO_REAL_TIME;
        p.i_pic_width = w;
        p.i_pic_height = h;
        p.i_target_bitrate = bps;
        p.i_max_bitrate = bps;
        p.i_rc_mode = RC_BITRATE_MODE;
        p.f_max_frame_rate = fps;
        p.i_temporal_layer_num = 1;
        p.i_spatial_layer_num = 1;
        p.i_complexity_mode = complexity;
        p.ui_intra_period = 0;
        p.e_sps_pps_id_strategy = CONSTANT_ID;
        p.i_entropy_coding_mode_flag = 1;
        p.b_enable_frame_skip = false;
        p.i_multiple_thread_idc = 1;
        p.b_enable_long_term_reference = true;
        p.i_ltr_ref_num = 2;
        p.i_ltr_mark_period = LTR_MARK_PERIOD;
        p.b_enable_scene_change_detect = scene_change_detect;
        p.i_min_qp = MIN_QP;
        let l = &mut p.s_spatial_layers[0];
        l.i_video_width = w;
        l.i_video_height = h;
        l.f_frame_rate = fps;
        l.i_spatial_bitrate = bps;
        l.i_max_spatial_bitrate = bps;
        l.s_slice_argument.ui_slice_mode = SM_SINGLE_SLICE;
        Ok(p)
    }

    fn check_dims(cfg: &StreamConfig) -> Result<(), BackendError> {
        let (w, h) = (cfg.width, cfg.height);
        if w % 2 != 0 || h % 2 != 0 {
            return Err(BackendError::Incapable(format!(
                "openh264 needs even picture sizes, got {w}x{h}"
            )));
        }
        if w < MIN_DIM || h < MIN_DIM {
            return Err(BackendError::Incapable(format!(
                "openh264 encodes nothing below {MIN_DIM}x{MIN_DIM}, got {w}x{h}"
            )));
        }
        if u64::from(w) * u64::from(h) > MAX_PIXELS || cfg.fps > MAX_FPS {
            let why = format!(
                "openh264 is offered up to 1920x1080 at {MAX_FPS} fps, got {w}x{h} at {} fps",
                cfg.fps
            );
            // Said once per configuration, not at every IDR: the stream goes
            // to the tile codec (or libva-direct) and this is why.
            static LAST: std::sync::Mutex<Option<(u32, u32, u32)>> = std::sync::Mutex::new(None);
            let key = (w, h, cfg.fps);
            let mut last = LAST.lock().unwrap_or_else(|e| e.into_inner());
            if *last != Some(key) {
                *last = Some(key);
                tracing::warn!(target: LOG_TARGET, "{why}: not using openh264 for this stream");
            }
            return Err(BackendError::Incapable(why));
        }
        Ok(())
    }

    fn apply_rate(s: &mut Session, bps: u32) {
        let bps = bps.max(1);
        if s.rate_bps.abs_diff(bps) < s.rate_bps / RATE_HYSTERESIS_DIV {
            return;
        }
        let v = i32::try_from(bps).unwrap_or(i32::MAX);
        // The one spatial layer, by index: with `SPATIAL_LAYER_ALL` a raise
        // above the opening maximum is refused (the maximum is raised, the
        // target then is not).
        let order = [ENCODER_OPTION_BITRATE, ENCODER_OPTION_MAX_BITRATE];
        let mut rcs = [0; 2];
        for (rc, option) in rcs.iter_mut().zip(order) {
            *rc = s.enc.set_option(
                option,
                &mut SBitrateInfo {
                    i_layer: SPATIAL_LAYER_0,
                    i_bitrate: v,
                },
            );
        }
        if rcs != [0, 0] {
            tracing::warn!(target: LOG_TARGET, ?rcs, bps, "openh264 refused a rate change");
            return;
        }
        s.rate_bps = bps;
        s.stats.rate_sets += 1;
    }
}

/// NV12 to planar I420, stride-aware, into `dst` (resized once, reused).
pub(crate) fn nv12_to_i420(src: &Nv12View<'_>, dst: &mut Vec<u8>) {
    let (w, h) = (src.width as usize, src.height as usize);
    let (cw, ch) = (w / 2, h / 2);
    dst.resize(w * h + 2 * cw * ch, 0);
    let (y, uv) = dst.split_at_mut(w * h);
    let (u, v) = uv.split_at_mut(cw * ch);
    if src.y_stride == w {
        y.copy_from_slice(&src.data[..w * h]);
    } else {
        for (row, out) in y.chunks_exact_mut(w).enumerate() {
            out.copy_from_slice(src.y_row(row as u32));
        }
    }
    for row in 0..ch {
        let s = src.uv_row(row as u32);
        let (ur, vr) = (
            &mut u[row * cw..(row + 1) * cw],
            &mut v[row * cw..(row + 1) * cw],
        );
        for ((p, uo), vo) in s.chunks_exact(2).zip(ur).zip(vr) {
            *uo = p[0];
            *vo = p[1];
        }
    }
}

impl H264Backend for OpenH264Backend {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            explicit_refs: false,
            max_lt: 2,
            max_ref_frames: 3,
            vbr: true,
        }
    }

    fn open(&mut self, cfg: &StreamConfig) -> Result<(), BackendError> {
        self.session = None;
        self.failed = false;
        Self::check_dims(cfg)?;
        let lib = Lib::get(self.lib_path.as_deref())
            .map_err(|e| BackendError::Unavailable(e.to_string()))?;
        let mut enc = lib.create_encoder().map_err(BackendError::Unavailable)?;
        enc.install_trace();
        #[cfg(test)]
        let scd = self.scene_change_detect;
        #[cfg(not(test))]
        let scd = false;
        let params = Self::params_for(&mut enc, cfg, self.complexity, scd)?;
        let rc = enc.initialize_ext(&params);
        if rc != 0 {
            return Err(BackendError::Incapable(format!(
                "openh264 InitializeExt returned {rc} for {}x{}",
                cfg.width, cfg.height
            )));
        }
        let mut fmt = VIDEO_FORMAT_I420;
        let rc = enc.set_option(ENCODER_OPTION_DATAFORMAT, &mut fmt);
        if rc != 0 {
            return Err(BackendError::Incapable(format!(
                "openh264 refused I420 input ({rc})"
            )));
        }
        tracing::info!(
            target: LOG_TARGET,
            width = cfg.width,
            height = cfg.height,
            lib = %lib.origin,
            version = %lib.version,
            "openh264 session open: {CISCO_ATTRIBUTION}"
        );
        self.session = Some(Session {
            enc,
            cfg: *cfg,
            frame: FrameBs::new(lib.layout),
            i420: Vec::new(),
            rate_bps: cfg.rate.bps.max(1),
            params: None,
            stats: Openh264Stats::default(),
        });
        self.pending_rate = None;
        Ok(())
    }

    fn encode(
        &mut self,
        nv12: &Nv12View<'_>,
        plan: &FramePlan,
        _refs: &[ResidentRef],
    ) -> Result<EncodedAu, BackendError> {
        if self.failed {
            return Err(BackendError::Failed);
        }
        let pending = self.pending_rate.take();
        let Some(s) = self.session.as_mut() else {
            return Err(BackendError::Failed);
        };
        if (nv12.width, nv12.height) != (s.cfg.width, s.cfg.height) || !nv12.is_complete() {
            return Err(BackendError::Plan(
                "NV12 buffer does not match the session".into(),
            ));
        }
        if let Some(r) = pending {
            Self::apply_rate(s, r.bps);
        }
        let out = Self::encode_inner(s, nv12, plan);
        if matches!(
            out,
            Err(BackendError::Driver { .. } | BackendError::Malformed | BackendError::Incapable(_))
        ) {
            self.failed = true;
        }
        out
    }

    fn set_rate(&mut self, rate: RateTarget) {
        self.pending_rate = Some(rate);
    }

    fn feedback(&mut self, fb: BackendFeedback) {
        let Some(s) = self.session.as_mut() else {
            return;
        };
        let rc = match fb {
            BackendFeedback::LtrMarked {
                idr_pic_id,
                frame_num,
            } => s.enc.set_option(
                ENCODER_LTR_MARKING_FEEDBACK,
                &mut SLtrMarkingFeedback {
                    ui_feedback_type: LTR_MARKING_SUCCESS,
                    ui_idr_pic_id: idr_pic_id,
                    i_ltr_frame_num: frame_num as c_int,
                    i_layer_id: 0,
                },
            ),
            BackendFeedback::LtrMarkFailed {
                idr_pic_id,
                frame_num,
            } => s.enc.set_option(
                ENCODER_LTR_MARKING_FEEDBACK,
                &mut SLtrMarkingFeedback {
                    ui_feedback_type: LTR_MARKING_FAILED,
                    ui_idr_pic_id: idr_pic_id,
                    i_ltr_frame_num: frame_num as c_int,
                    i_layer_id: 0,
                },
            ),
            BackendFeedback::LtrRecover {
                idr_pic_id,
                last_correct_frame_num,
                current_frame_num,
            } => s.enc.set_option(
                ENCODER_LTR_RECOVERY_REQUEST,
                &mut SLtrRecoverRequest {
                    ui_feedback_type: LTR_RECOVERY_REQUEST,
                    ui_idr_pic_id: idr_pic_id,
                    i_last_correct_frame_num: last_correct_frame_num as c_int,
                    i_current_frame_num: current_frame_num as c_int,
                    i_layer_id: 0,
                },
            ),
        };
        if rc == 0 {
            s.stats.feedback_applied += 1;
        } else {
            s.stats.feedback_rejected += 1;
            tracing::warn!(target: LOG_TARGET, rc, ?fb, "openh264 refused feedback");
        }
    }
}

impl OpenH264Backend {
    fn encode_inner(
        s: &mut Session,
        nv12: &Nv12View<'_>,
        plan: &FramePlan,
    ) -> Result<EncodedAu, BackendError> {
        nv12_to_i420(nv12, &mut s.i420);
        let (w, h) = (nv12.width as usize, nv12.height as usize);
        let (cw, ch) = (w / 2, h / 2);
        if plan.kind == PicKind::Idr {
            let rc = s.enc.force_intra();
            if rc != 0 {
                return Err(BackendError::Driver {
                    call: "ForceIntraFrame",
                    detail: format!("returned {rc}"),
                });
            }
            s.stats.idr_forced += 1;
        }
        let base = s.i420.as_mut_ptr();
        let pic = SSourcePicture {
            i_color_format: VIDEO_FORMAT_I420,
            i_stride: [w as c_int, cw as c_int, cw as c_int, 0],
            // SAFETY: the three planes lie inside `i420` (sized by
            // `nv12_to_i420`); the library only reads them.
            p_data: unsafe {
                [
                    base,
                    base.add(w * h),
                    base.add(w * h + cw * ch),
                    std::ptr::null_mut(),
                ]
            },
            i_pic_width: w as c_int,
            i_pic_height: h as c_int,
            ui_time_stamp: (s.stats.frames * 1000 / u64::from(s.cfg.fps.max(1))) as i64,
            b_psnr_y: false,
            b_psnr_u: false,
            b_psnr_v: false,
        };
        let rc = s.enc.encode(&pic, &mut s.frame);
        let ft = s.frame.frame_type();
        if rc != 0 {
            return Err(BackendError::Driver {
                call: "EncodeFrame",
                detail: format!("returned {rc}"),
            });
        }
        if ft == VIDEO_FRAME_TYPE_SKIP || ft == VIDEO_FRAME_TYPE_INVALID {
            return Err(BackendError::Driver {
                call: "EncodeFrame",
                detail: format!("no picture coded (frame type {ft})"),
            });
        }
        // The library's own total first: it bounds what a layer may hold, and
        // a library whose output does not match the layout we read it with is
        // refused before a layer pointer is followed. The layout itself is
        // chosen by version (`Layout::for_version`), so this is a second line.
        let frame_size = usize::try_from(s.frame.frame_size()).unwrap_or(0);
        if frame_size == 0 || frame_size > MAX_AU_BYTES || s.frame.layer_num() == 0 {
            return Err(BackendError::Incapable(format!(
                "openh264 {} output does not match the {:?} layout (frame size {}, {} layers)",
                s.enc.lib().version,
                s.enc.lib().layout,
                s.frame.frame_size(),
                s.frame.layer_num()
            )));
        }
        let mut payload = Vec::new();
        for i in 0..s.frame.layer_num() {
            // SAFETY: filled by the `encode` above, which nothing has
            // followed.
            let bytes = unsafe { s.frame.layer_bytes(i, frame_size) }.ok_or_else(|| {
                BackendError::Driver {
                    call: "EncodeFrame",
                    detail: format!("layer {i} is inconsistent"),
                }
            })?;
            payload.extend_from_slice(bytes);
        }
        if payload.is_empty() {
            return Err(BackendError::Driver {
                call: "EncodeFrame",
                detail: "empty access unit".into(),
            });
        }
        // The library's own total and a start code, once more over what was
        // assembled: a wrong layout is Incapable here, not Malformed in the
        // parser.
        if frame_size != payload.len() || !payload.starts_with(&[0, 0, 0, 1]) {
            return Err(BackendError::Incapable(format!(
                "openh264 {} output does not match the {:?} layout (frame size {}, got {} bytes)",
                s.enc.lib().version,
                s.enc.lib().layout,
                s.frame.frame_size(),
                payload.len()
            )));
        }
        if ft == VIDEO_FRAME_TYPE_IDR || s.params.is_none() {
            let p = StreamParams::from_annexb(&payload).map_err(|e| {
                BackendError::Incapable(format!("openh264 stream parameters: {e:?}"))
            })?;
            s.params = Some(p);
        }
        let params = s.params.ok_or(BackendError::Malformed)?;
        let header = parse_access_unit(&params, &payload).map_err(|e| {
            tracing::warn!(target: LOG_TARGET, "openh264 access unit does not parse: {e:?}");
            BackendError::Malformed
        })?;
        s.stats.frames += 1;
        Ok(EncodedAu {
            meta: AuMeta {
                idr: header.idr,
                header: Some(header),
                ref_seq: None,
            },
            payload,
        })
    }
}
