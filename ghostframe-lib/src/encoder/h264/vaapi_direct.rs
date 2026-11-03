//! The libva-direct backend (design 4.2): raw libva, executing a
//! [`FramePlan`] exactly.
//!
//! One slice per picture, High profile with CABAC, VBR (or CQP), packed SPS,
//! PPS and slice headers written by [`super::syntax`] and [`super::param_sets`].
//! The call sequence mirrors the S2 spike (`enc2.c`), which is bit-exact
//! against libavcodec on this driver (Mesa radeonsi VCE):
//!
//! * `ReferenceFrames[]` lists every resident reference on every frame; an
//!   unlisted one is evicted after two frames and a later reference fails.
//! * `RefPicList0[0]` is the plan's target and the packed slice header carries
//!   the matching explicit RPLM (VCE predicts from the list, a decoder
//!   follows the header).
//! * A rate change is a new rate-control buffer and HRD buffer on a P frame
//!   (rate only; frame-size caps and VBV changes are B6's).
//! * `vaGetImage` on a recon surface does not return the recon, so nothing
//!   here reads it back; the oracle is a decode.

use std::ffi::{c_int, c_uint, c_void};
use std::fs::File;
use std::os::fd::AsRawFd;
use std::ptr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::{debug, warn};

use super::backend::{
    AuMeta, BackendCaps, BackendError, BackendFeedback, EncodedAu, H264Backend, Nv12View,
    RateTarget, RcMode, StreamConfig,
};
use super::param_sets::{write_pps, write_sps, SeqConfig};
use super::refs::{FramePlan, Plan, RefConfig, RefManager, RefTarget, ResidentRef};
use super::syntax::{
    parse_access_unit, write_slice_header, Mmco, StreamParams, NAL_IDR, NAL_SLICE,
};
use super::tracker::PicKind;
use super::va_sys::*;
use crate::encoder::vaapi_device::vaapi_device_path;

/// `pic_init_qp` for VBR, where the rate control moves it from there.
const VBR_INIT_QP: u8 = 26;
/// Recon surfaces: every resident reference, the picture being coded, and one
/// of slack.
fn recon_pool(params: &StreamParams) -> usize {
    params.max_num_ref_frames as usize + 2
}

/// A recon surface and the picture in it.
#[derive(Clone, Copy)]
struct Recon {
    surface: VASurfaceID,
    /// `(frame_seq, poc)` of the picture held; free when `None`.
    held: Option<(u32, u32)>,
}

/// Everything that exists while the backend is open.
struct Session {
    va: &'static Va,
    dpy: VADisplay,
    _device: File,
    config: VAConfigID,
    context: VAContextID,
    input: VASurfaceID,
    recon: Vec<Recon>,
    coded: VABufferID,
    image: VAImage,
    image_created: bool,
    cfg: StreamConfig,
    seq: SeqConfig,
    emitted: StreamParams,
    sps: Vec<u8>,
    pps: Vec<u8>,
    rate: RateTarget,
    /// HRD buffer, bits: about 100 ms of the rate at open, fixed (B6 tunes it).
    vbv_bits: u32,
    qp: u8,
    rate_dirty: bool,
    failed: bool,
}

fn check(va: &Va, call: &'static str, status: VAStatus) -> Result<(), BackendError> {
    if status == 0 {
        Ok(())
    } else {
        Err(BackendError::Driver {
            call,
            detail: format!("{status} ({})", va.error_string(status)),
        })
    }
}

impl Session {
    fn open(cfg: &StreamConfig) -> Result<Self, BackendError> {
        let va = Va::get().map_err(|e| BackendError::Unavailable(e.to_string()))?;
        if cfg.width < 128 || cfg.height < 128 {
            // The AMD encoder's minimum frame size.
            return Err(BackendError::Incapable("frame smaller than 128x128".into()));
        }
        let seq = SeqConfig {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
        };
        seq.validate().map_err(BackendError::Incapable)?;
        let path = vaapi_device_path();
        let device = File::options()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| BackendError::Unavailable(format!("{path}: {e}")))?;
        // SAFETY: the fd stays open for the session's life (`_device`).
        let dpy = unsafe { (va.get_display_drm)(device.as_raw_fd()) };
        if dpy.is_null() {
            return Err(BackendError::Unavailable("vaGetDisplayDRM".into()));
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: `dpy` is a display just obtained; the out-params are locals.
        let st = unsafe { (va.initialize)(dpy, &mut major, &mut minor) };
        if st != 0 {
            // SAFETY: the display is valid; terminate pairs with the attempt.
            unsafe { (va.terminate)(dpy) };
            return Err(BackendError::Unavailable(format!(
                "vaInitialize: {}",
                va.error_string(st)
            )));
        }
        let qp = match cfg.rc {
            RcMode::Vbr => VBR_INIT_QP,
            RcMode::Cqp { qp } => qp,
        };
        let mut s = Self {
            va,
            dpy,
            _device: device,
            config: VA_INVALID_ID,
            context: VA_INVALID_ID,
            input: VA_INVALID_ID,
            recon: Vec::new(),
            coded: VA_INVALID_ID,
            image: zeroed(),
            image_created: false,
            cfg: *cfg,
            seq,
            emitted: StreamParams {
                deblocking_control_present: false,
                ..cfg.params
            },
            sps: write_sps(&cfg.params, &seq),
            pps: write_pps(&cfg.params, qp),
            rate: cfg.rate,
            vbv_bits: (cfg.rate.bps / 10).max(1),
            qp,
            rate_dirty: false,
            failed: false,
        };
        // On error `s` drops and releases whatever was created.
        s.create_resources()?;
        debug!(
            width = cfg.width,
            height = cfg.height,
            va = %format!("{major}.{minor}"),
            "libva-direct session open"
        );
        Ok(s)
    }

    fn create_resources(&mut self) -> Result<(), BackendError> {
        let va = self.va;
        let cfg = self.cfg;
        let rc_value = match cfg.rc {
            RcMode::Vbr => VA_RC_VBR,
            RcMode::Cqp { .. } => VA_RC_CQP,
        };
        let mut probe = [
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: 0,
            },
        ];
        // SAFETY: `probe` is a live array of the declared length.
        check(va, "vaGetConfigAttributes", unsafe {
            (va.get_config_attributes)(
                self.dpy,
                VA_PROFILE_H264_HIGH,
                VA_ENTRYPOINT_ENC_SLICE,
                probe.as_mut_ptr(),
                probe.len() as c_int,
            )
        })
        .map_err(|e| BackendError::Unavailable(e.to_string()))?;
        if probe[0].value & rc_value == 0 {
            return Err(BackendError::Incapable(format!(
                "rate control {:?} not supported",
                cfg.rc
            )));
        }
        let want_packed = VA_ENC_PACKED_HEADER_SEQUENCE
            | VA_ENC_PACKED_HEADER_PICTURE
            | VA_ENC_PACKED_HEADER_SLICE;
        if probe[1].value & want_packed != want_packed {
            return Err(BackendError::Incapable(
                "packed headers not supported".into(),
            ));
        }

        let mut attribs = [
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: rc_value,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: want_packed,
            },
        ];
        // SAFETY: as above; `self.config` is a local out-param.
        check(va, "vaCreateConfig", unsafe {
            (va.create_config)(
                self.dpy,
                VA_PROFILE_H264_HIGH,
                VA_ENTRYPOINT_ENC_SLICE,
                attribs.as_mut_ptr(),
                attribs.len() as c_int,
                &mut self.config,
            )
        })?;

        let (cw, ch) = (self.seq.mb_width() * 16, self.seq.mb_height() * 16);
        let n_recon = recon_pool(&cfg.params);
        let mut surfaces = vec![0 as VASurfaceID; n_recon + 1];
        // SAFETY: `surfaces` has room for the count asked for.
        check(va, "vaCreateSurfaces", unsafe {
            (va.create_surfaces)(
                self.dpy,
                VA_RT_FORMAT_YUV420,
                cw,
                ch,
                surfaces.as_mut_ptr(),
                surfaces.len() as c_uint,
                ptr::null_mut(),
                0,
            )
        })?;
        self.input = surfaces[0];
        self.recon = surfaces[1..]
            .iter()
            .map(|&surface| Recon {
                surface,
                held: None,
            })
            .collect();
        // SAFETY: the surfaces were just created; the context out-param is a field.
        check(va, "vaCreateContext", unsafe {
            (va.create_context)(
                self.dpy,
                self.config,
                cw as c_int,
                ch as c_int,
                VA_PROGRESSIVE,
                surfaces.as_mut_ptr(),
                surfaces.len() as c_int,
                &mut self.context,
            )
        })?;
        // An uncompressed NV12 picture plus a margin bounds any access unit.
        let coded_size = cw * ch * 3 / 2 + 65_536;
        // SAFETY: a null data pointer asks for an uninitialised coded buffer.
        check(va, "vaCreateBuffer(coded)", unsafe {
            (va.create_buffer)(
                self.dpy,
                self.context,
                VA_ENC_CODED_BUFFER_TYPE,
                coded_size,
                1,
                ptr::null(),
                &mut self.coded,
            )
        })?;
        let mut fmt = VAImageFormat {
            fourcc: VA_FOURCC_NV12,
            byte_order: VA_LSB_FIRST,
            bits_per_pixel: 12,
            ..zeroed()
        };
        // SAFETY: `fmt` and `self.image` are live.
        check(va, "vaCreateImage", unsafe {
            (va.create_image)(
                self.dpy,
                &mut fmt,
                cw as c_int,
                ch as c_int,
                &mut self.image,
            )
        })?;
        self.image_created = true;
        Ok(())
    }

    /// Copy the picture into the input surface, replicating the last row and
    /// column into the macroblock padding.
    fn upload(&mut self, nv12: &Nv12View<'_>) -> Result<(), BackendError> {
        let va = self.va;
        let (cw, ch) = (
            self.seq.mb_width() as usize * 16,
            self.seq.mb_height() as usize * 16,
        );
        let (w, h) = (nv12.width as usize, nv12.height as usize);
        let mut p: *mut c_void = ptr::null_mut();
        // SAFETY: the image buffer is live and mapped for the copy only.
        check(va, "vaMapBuffer(image)", unsafe {
            (va.map_buffer)(self.dpy, self.image.buf, &mut p)
        })?;
        let base = p as *mut u8;
        let (yo, uvo) = (
            self.image.offsets[0] as usize,
            self.image.offsets[1] as usize,
        );
        let (yp, uvp) = (
            self.image.pitches[0] as usize,
            self.image.pitches[1] as usize,
        );
        // SAFETY: the image is `cw` x `ch` NV12 with the pitches and offsets
        // the driver reported; every row written lies inside it.
        unsafe {
            for row in 0..ch {
                let src = nv12.y_row(row.min(h - 1) as u32);
                let dst = std::slice::from_raw_parts_mut(base.add(yo + row * yp), cw);
                dst[..w].copy_from_slice(src);
                dst[w..].fill(src[w - 1]);
            }
            for row in 0..ch / 2 {
                let src = nv12.uv_row(row.min(h.div_ceil(2) - 1) as u32);
                let dst = std::slice::from_raw_parts_mut(base.add(uvo + row * uvp), cw);
                dst[..w].copy_from_slice(&src[..w]);
                for x in (w..cw).step_by(2) {
                    dst[x] = src[w - 2];
                    dst[x + 1] = src[w - 1];
                }
            }
        }
        // SAFETY: as above.
        check(va, "vaUnmapBuffer(image)", unsafe {
            (va.unmap_buffer)(self.dpy, self.image.buf)
        })?;
        // SAFETY: surface and image are live.
        check(va, "vaPutImage", unsafe {
            (va.put_image)(
                self.dpy,
                self.input,
                self.image.image_id,
                0,
                0,
                cw as c_uint,
                ch as c_uint,
                0,
                0,
                cw as c_uint,
                ch as c_uint,
            )
        })
    }

    fn buffer(
        &self,
        ids: &mut Vec<VABufferID>,
        call: &'static str,
        ty: c_int,
        size: usize,
        data: *const c_void,
    ) -> Result<(), BackendError> {
        let mut id = VA_INVALID_ID;
        // SAFETY: `data` points at `size` readable bytes.
        let st = unsafe {
            (self.va.create_buffer)(self.dpy, self.context, ty, size as c_uint, 1, data, &mut id)
        };
        check(self.va, call, st)?;
        ids.push(id);
        Ok(())
    }

    fn typed<T: Copy>(
        &self,
        ids: &mut Vec<VABufferID>,
        call: &'static str,
        ty: c_int,
        v: &T,
    ) -> Result<(), BackendError> {
        self.buffer(ids, call, ty, size_of::<T>(), (v as *const T).cast())
    }

    /// A miscellaneous parameter buffer: the type word then the payload.
    fn misc<T: Copy>(&self, ids: &mut Vec<VABufferID>, ty: u32, v: &T) -> Result<(), BackendError> {
        let mut bytes = Vec::with_capacity(4 + size_of::<T>());
        bytes.extend_from_slice(&ty.to_ne_bytes());
        // SAFETY: `T` is a plain repr(C) struct; its bytes are read once.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>())
        });
        self.buffer(
            ids,
            "vaCreateBuffer(misc)",
            VA_ENC_MISC_PARAMETER_BUFFER_TYPE,
            bytes.len(),
            bytes.as_ptr().cast(),
        )
    }

    /// A packed header: its parameter buffer and its data buffer.
    fn packed(&self, ids: &mut Vec<VABufferID>, ty: u32, bytes: &[u8]) -> Result<(), BackendError> {
        let p = VAEncPackedHeaderParameterBuffer {
            type_: ty,
            bit_length: (bytes.len() * 8) as u32,
            has_emulation_bytes: 0,
            va_reserved: [0; VA_PADDING_LOW],
        };
        self.typed(
            ids,
            "vaCreateBuffer(packed header)",
            VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE,
            &p,
        )?;
        self.buffer(
            ids,
            "vaCreateBuffer(packed data)",
            VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
            bytes.len(),
            bytes.as_ptr().cast(),
        )
    }

    fn picture(
        &self,
        surface: VASurfaceID,
        poc: u32,
        plan: &FramePlan,
        refs: &[(&ResidentRef, Recon, u32)],
    ) -> VAEncPictureParameterBufferH264 {
        let idr = plan.kind == PicKind::Idr;
        let mut pp: VAEncPictureParameterBufferH264 = zeroed();
        pp.curr_pic = VAPictureH264 {
            picture_id: surface,
            frame_idx: plan.frame_num,
            flags: 0,
            top_field_order_cnt: poc as i32,
            bottom_field_order_cnt: poc as i32,
            va_reserved: [0; VA_PADDING_LOW],
        };
        if let Some(slot) = plan.mark {
            pp.curr_pic.flags = VA_PICTURE_H264_LONG_TERM_REFERENCE;
            pp.curr_pic.frame_idx = u32::from(slot);
        }
        pp.reference_frames = [VAPictureH264::INVALID; 16];
        for (i, (r, rec, rpoc)) in refs.iter().enumerate() {
            let (idx, flags) = match r.lt_idx {
                Some(k) => (u32::from(k), VA_PICTURE_H264_LONG_TERM_REFERENCE),
                None => (r.frame_num, VA_PICTURE_H264_SHORT_TERM_REFERENCE),
            };
            pp.reference_frames[i] = VAPictureH264 {
                picture_id: rec.surface,
                frame_idx: idx,
                flags,
                top_field_order_cnt: *rpoc as i32,
                bottom_field_order_cnt: *rpoc as i32,
                va_reserved: [0; VA_PADDING_LOW],
            };
        }
        pp.coded_buf = self.coded;
        pp.frame_num = plan.frame_num as u16;
        pp.pic_init_qp = self.qp;
        pp.pic_fields = u32::from(idr) << pic_bits::IDR
            | u32::from(plan.nal_ref_idc != 0) << pic_bits::REFERENCE
            | u32::from(self.cfg.params.cabac) << pic_bits::ENTROPY_CODING_MODE
            | u32::from(self.cfg.params.deblocking_control_present)
                << pic_bits::DEBLOCKING_CONTROL_PRESENT;
        pp
    }

    fn sequence(&self) -> VAEncSequenceParameterBufferH264 {
        use seq_bits as s;
        use vui_bits as v;
        let p = &self.cfg.params;
        let mut sp: VAEncSequenceParameterBufferH264 = zeroed();
        sp.level_idc = self.seq.level_idc().expect("validated at open");
        sp.ip_period = 1;
        sp.bits_per_second = self.rate.bps;
        sp.max_num_ref_frames = p.max_num_ref_frames;
        sp.picture_width_in_mbs = self.seq.mb_width() as u16;
        sp.picture_height_in_mbs = self.seq.mb_height() as u16;
        sp.seq_fields = 1 << s::CHROMA_FORMAT_IDC
            | 1 << s::FRAME_MBS_ONLY
            | 1 << s::DIRECT_8X8_INFERENCE
            | (p.log2_max_frame_num - 4) << s::LOG2_MAX_FRAME_NUM_MINUS4
            | (p.log2_max_poc_lsb - 4) << s::LOG2_MAX_POC_LSB_MINUS4;
        let crop = self.seq.crop_bottom_rows();
        let right = self.seq.crop_right_cols();
        if crop != 0 || right != 0 {
            sp.frame_cropping_flag = 1;
            sp.frame_crop_right_offset = right / 2;
            sp.frame_crop_bottom_offset = crop / 2;
        }
        sp.vui_parameters_present_flag = 1;
        sp.vui_fields = 1 << v::TIMING_INFO
            | 1 << v::BITSTREAM_RESTRICTION
            | 16 << v::LOG2_MAX_MV_LENGTH_HORIZONTAL
            | 16 << v::LOG2_MAX_MV_LENGTH_VERTICAL
            | 1 << v::FIXED_FRAME_RATE
            | 1 << v::MV_OVER_PIC_BOUNDARIES;
        sp.num_units_in_tick = 1;
        sp.time_scale = self.seq.fps * 2;
        sp
    }

    /// The rate-control group: rate and HRD. The frame-size cap and VBV
    /// changes belong to the rate-control PR (B6).
    fn rate_buffers(&mut self, ids: &mut Vec<VABufferID>) -> Result<(), BackendError> {
        let mut rc: VAEncMiscParameterRateControl = zeroed();
        rc.bits_per_second = self.rate.bps;
        rc.target_percentage = 100;
        rc.window_size = 1000;
        rc.initial_qp = u32::from(self.qp);
        self.misc(ids, VA_MISC_RATE_CONTROL, &rc)?;
        let hrd = VAEncMiscParameterHRD {
            initial_buffer_fullness: self.vbv_bits / 4 * 3,
            buffer_size: self.vbv_bits,
            va_reserved: [0; VA_PADDING_LOW],
        };
        self.misc(ids, VA_MISC_HRD, &hrd)
    }

    fn encode(
        &mut self,
        nv12: &Nv12View<'_>,
        plan: &FramePlan,
        refs: &[ResidentRef],
    ) -> Result<EncodedAu, BackendError> {
        if self.failed {
            return Err(BackendError::Failed);
        }
        if nv12.width != self.cfg.width || nv12.height != self.cfg.height || !nv12.is_complete() {
            return Err(BackendError::Plan(
                "NV12 view does not match the stream".into(),
            ));
        }
        let idr = plan.kind == PicKind::Idr;
        if refs.len() > self.cfg.params.max_num_ref_frames as usize || (idr && !refs.is_empty()) {
            return Err(BackendError::Plan(format!(
                "{} resident references for a {} frame",
                refs.len(),
                if idr { "IDR" } else { "P" }
            )));
        }
        // Resolve the resident set to surfaces first: a plan that cannot be
        // executed must leave the backend's DPB as it was.
        let mut resolved = Vec::with_capacity(refs.len());
        for r in refs {
            let rec = self
                .recon
                .iter()
                .find_map(|c| {
                    c.held
                        .filter(|&(s, _)| s == r.seq)
                        .map(|(_, poc)| (*c, poc))
                })
                .ok_or_else(|| {
                    BackendError::Plan(format!("reference {} is not in the backend's DPB", r.seq))
                })?;
            resolved.push((r, rec.0, rec.1));
        }
        let target = match plan.target {
            Some(t) => Some(
                *resolved
                    .iter()
                    .find(|(r, _, _)| r.seq == t.seq())
                    .ok_or_else(|| {
                        BackendError::Plan(format!("target {} is not resident", t.seq()))
                    })?,
            ),
            None if idr => None,
            None => return Err(BackendError::Plan("P frame without a target".into())),
        };
        // Release every surface whose picture the manager no longer holds.
        for r in &mut self.recon {
            if r.held
                .is_some_and(|(seq, _)| !refs.iter().any(|x| x.seq == seq))
            {
                r.held = None;
            }
        }
        let slot = self
            .recon
            .iter()
            .position(|r| r.held.is_none())
            .ok_or_else(|| BackendError::Plan("no free recon surface".into()))?;
        let surface = self.recon[slot].surface;

        // What goes into RefPicList0[0] is the surface the backend holds for
        // the target: its seq is the backend's own truth, not the plan's.
        let ref_seq = target.and_then(|(_, rec, _)| rec.held.map(|(seq, _)| seq));
        let result = self.submit(nv12, plan, surface, &resolved, target);
        match result {
            Ok(payload) => {
                self.recon[slot].held = Some((plan.frame_seq, plan.poc));
                let header = parse_access_unit(&self.emitted, &payload).ok();
                let Some(header) = header else {
                    self.failed = true;
                    return Err(BackendError::Malformed);
                };
                Ok(EncodedAu {
                    meta: AuMeta {
                        idr: header.idr,
                        header: Some(header),
                        ref_seq,
                    },
                    payload,
                })
            }
            Err(e) => {
                self.failed = true;
                Err(e)
            }
        }
    }

    fn submit(
        &mut self,
        nv12: &Nv12View<'_>,
        plan: &FramePlan,
        surface: VASurfaceID,
        resolved: &[(&ResidentRef, Recon, u32)],
        target: Option<(&ResidentRef, Recon, u32)>,
    ) -> Result<Vec<u8>, BackendError> {
        let va = self.va;
        let idr = plan.kind == PicKind::Idr;
        self.upload(nv12)?;

        // pic_init_qp (PPS and picture parameters) is the QP, so no delta.
        let mut header = plan.slice_header(&self.cfg.params);
        header.slice_qp_delta = 0;
        let bits = write_slice_header(&self.cfg.params, &header);
        let mut slice_nal = vec![
            0,
            0,
            0,
            1,
            (header.nal_ref_idc << 5) | if idr { NAL_IDR } else { NAL_SLICE },
        ];
        slice_nal.extend_from_slice(&bits.bytes);

        let mut ids: Vec<VABufferID> = Vec::with_capacity(16);
        let built = self.build_and_run(&mut ids, plan, surface, resolved, target, &slice_nal);
        for id in ids {
            // SAFETY: each id was created on this context and not yet destroyed.
            unsafe { (va.destroy_buffer)(self.dpy, id) };
        }
        built
    }

    fn build_and_run(
        &mut self,
        ids: &mut Vec<VABufferID>,
        plan: &FramePlan,
        surface: VASurfaceID,
        resolved: &[(&ResidentRef, Recon, u32)],
        target: Option<(&ResidentRef, Recon, u32)>,
        slice_nal: &[u8],
    ) -> Result<Vec<u8>, BackendError> {
        let va = self.va;
        let idr = plan.kind == PicKind::Idr;
        let cqp = matches!(self.cfg.rc, RcMode::Cqp { .. });
        if idr {
            let sp = self.sequence();
            self.typed(
                ids,
                "vaCreateBuffer(sequence)",
                VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE,
                &sp,
            )?;
            let fr = VAEncMiscParameterFrameRate {
                framerate: self.seq.fps | 1 << 16,
                framerate_flags: 0,
                va_reserved: [0; VA_PADDING_LOW],
            };
            self.misc(ids, VA_MISC_FRAME_RATE, &fr)?;
            let (sps, pps) = (self.sps.clone(), self.pps.clone());
            self.packed(ids, VA_PACKED_HEADER_SEQUENCE, &sps)?;
            self.packed(ids, VA_PACKED_HEADER_PICTURE, &pps)?;
        }
        if !cqp && (idr || self.rate_dirty) {
            self.rate_buffers(ids)?;
            self.rate_dirty = false;
        }
        let pp = self.picture(surface, plan.poc, plan, resolved);
        self.typed(
            ids,
            "vaCreateBuffer(picture)",
            VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE,
            &pp,
        )?;
        self.packed(ids, VA_PACKED_HEADER_SLICE, slice_nal)?;

        let mut sl: VAEncSliceParameterBufferH264 = zeroed();
        sl.num_macroblocks = self.seq.mb_width() * self.seq.mb_height();
        sl.slice_type = if idr { 2 } else { 0 };
        sl.idr_pic_id = (plan.epoch & 0xffff) as u16;
        sl.pic_order_cnt_lsb = (plan.poc & ((1 << self.cfg.params.log2_max_poc_lsb) - 1)) as u16;
        sl.ref_pic_list0 = [VAPictureH264::INVALID; 32];
        sl.ref_pic_list1 = [VAPictureH264::INVALID; 32];
        if let Some((r, rec, rpoc)) = target {
            let (idx, flags) = match r.lt_idx {
                Some(k) => (u32::from(k), VA_PICTURE_H264_LONG_TERM_REFERENCE),
                None => (r.frame_num, VA_PICTURE_H264_SHORT_TERM_REFERENCE),
            };
            sl.ref_pic_list0[0] = VAPictureH264 {
                picture_id: rec.surface,
                frame_idx: idx,
                flags,
                top_field_order_cnt: rpoc as i32,
                bottom_field_order_cnt: rpoc as i32,
                va_reserved: [0; VA_PADDING_LOW],
            };
        }
        self.typed(
            ids,
            "vaCreateBuffer(slice)",
            VA_ENC_SLICE_PARAMETER_BUFFER_TYPE,
            &sl,
        )?;

        // SAFETY: input surface, context and buffers are live.
        check(va, "vaBeginPicture", unsafe {
            (va.begin_picture)(self.dpy, self.context, self.input)
        })?;
        // SAFETY: `ids` holds the buffers created above.
        check(va, "vaRenderPicture", unsafe {
            (va.render_picture)(self.dpy, self.context, ids.as_mut_ptr(), ids.len() as c_int)
        })?;
        // SAFETY: the picture was begun above.
        check(va, "vaEndPicture", unsafe {
            (va.end_picture)(self.dpy, self.context)
        })?;
        // SAFETY: as above.
        check(va, "vaSyncSurface", unsafe {
            (va.sync_surface)(self.dpy, self.input)
        })?;

        let mut seg: *mut c_void = ptr::null_mut();
        // SAFETY: the coded buffer is live; the segment list is read before
        // the unmap.
        check(va, "vaMapBuffer(coded)", unsafe {
            (va.map_buffer)(self.dpy, self.coded, &mut seg)
        })?;
        let mut out = Vec::new();
        let mut overflow = false;
        let mut cur = seg as *const VACodedBufferSegment;
        // SAFETY: the driver returns a NULL-terminated list of segments, each
        // pointing at `size` bytes.
        unsafe {
            while !cur.is_null() {
                let s = &*cur;
                if s.status & VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK != 0 || s.buf.is_null() {
                    overflow = true;
                    break;
                }
                out.extend_from_slice(std::slice::from_raw_parts(
                    s.buf.cast::<u8>(),
                    s.size as usize,
                ));
                cur = s.next as *const VACodedBufferSegment;
            }
        }
        // SAFETY: as above.
        check(va, "vaUnmapBuffer(coded)", unsafe {
            (va.unmap_buffer)(self.dpy, self.coded)
        })?;
        if overflow {
            return Err(BackendError::Driver {
                call: "coded buffer",
                detail: "overflow or empty segment".into(),
            });
        }
        Ok(out)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let va = self.va;
        // SAFETY: each handle was created by this session; ids that were
        // never created are VA_INVALID_ID and skipped.
        unsafe {
            if self.image_created {
                (va.destroy_image)(self.dpy, self.image.image_id);
            }
            if self.coded != VA_INVALID_ID {
                (va.destroy_buffer)(self.dpy, self.coded);
            }
            if self.context != VA_INVALID_ID {
                (va.destroy_context)(self.dpy, self.context);
            }
            if self.input != VA_INVALID_ID {
                let mut all: Vec<VASurfaceID> = std::iter::once(self.input)
                    .chain(self.recon.iter().map(|r| r.surface))
                    .collect();
                (va.destroy_surfaces)(self.dpy, all.as_mut_ptr(), all.len() as c_int);
            }
            if self.config != VA_INVALID_ID {
                (va.destroy_config)(self.dpy, self.config);
            }
            (va.terminate)(self.dpy);
        }
    }
}

// SAFETY: the session is used by one thread at a time (`&mut self`); libva
// display handles may move between threads.
unsafe impl Send for Session {}

/// What the probe's verdict depends on.
#[derive(Clone, PartialEq)]
struct ProbeKey {
    device: &'static str,
    params: StreamParams,
    rc: RcMode,
}

/// Verdicts reached in this process: `Ok`, or the reason the driver was
/// found incapable. Failures that say nothing about the driver (no device, a
/// driver call erroring) are not cached.
static PROBES: Mutex<Vec<(ProbeKey, Result<(), String>)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn clear_probe_cache() {
    PROBES.lock().unwrap_or_else(|p| p.into_inner()).clear();
}

/// [`probe`], once per process and configuration.
fn probe_cached(cfg: &StreamConfig) -> Result<(), BackendError> {
    let key = ProbeKey {
        device: vaapi_device_path(),
        params: cfg.params,
        rc: cfg.rc,
    };
    // Held across the probe so concurrent opens wait for one verdict.
    let mut cache = PROBES.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((_, v)) = cache.iter().find(|(k, _)| *k == key) {
        return v.clone().map_err(BackendError::Incapable);
    }
    let started = Instant::now();
    let verdict = probe(cfg);
    debug!(
        ms = Instant::now()
            .saturating_duration_since(started)
            .as_millis() as u64,
        ok = verdict.is_ok(),
        "libva-direct capability probe"
    );
    match &verdict {
        Ok(()) => cache.push((key, Ok(()))),
        Err(BackendError::Incapable(why)) => cache.push((key, Err(why.clone()))),
        Err(_) => {}
    }
    verdict
}

/// Check, on a 128x128 session, that the driver executes what the stream
/// model needs: an IDR into a long-term slot, a P with an RPLM to the
/// long-term picture, a P with RPLM to a short-term picture that marks itself
/// long-term (MMCO 4, 1, 6) and a P whose RPLM names that new long-term slot.
/// The IDR is acknowledged so the manager marks a candidate at the third
/// frame; each plan's shape is asserted (a probe that never marks proves
/// nothing). Each AU is parsed and replayed through the `RefManager`, so a
/// driver that ignores an operation is found without a decode.
fn probe(cfg: &StreamConfig) -> Result<(), BackendError> {
    const SIZE: u32 = 128;
    let interval = Duration::from_millis(33);
    // a_min = ceil(srtt / interval) = 2: the first mark is the third frame.
    let srtt = interval * 2;
    let mut s = Session::open(&StreamConfig {
        width: SIZE,
        height: SIZE,
        ..*cfg
    })?;
    let mut mgr = RefManager::new(RefConfig {
        params: cfg.params,
        frame_interval: interval,
        initial_srtt: srtt,
    });
    let (y, uv) = ((SIZE * SIZE) as usize, (SIZE * SIZE / 2) as usize);
    let data: Vec<u8> = (0..y + uv).map(|i| (i * 7 % 251) as u8).collect();
    let view = Nv12View {
        data: &data,
        width: SIZE,
        height: SIZE,
        y_stride: SIZE as usize,
        uv_stride: SIZE as usize,
        uv_offset: y,
    };
    let t0 = Instant::now();
    for n in 0..4u32 {
        let now = t0 + interval * n;
        mgr.tick(now, srtt);
        let Plan::Frame(plan) = mgr.plan_next(n, now) else {
            return Err(BackendError::Plan("probe: plan held".into()));
        };
        let what = [
            "IDR",
            "P with RPLM to the long-term IDR",
            "P with MMCO 4, 1, 6",
            "P with RPLM to the newly marked slot",
        ][n as usize];
        let shape_ok = match n {
            0 => plan.kind == PicKind::Idr,
            1 => {
                matches!(plan.target, Some(RefTarget::Long { slot: 0, .. })) && plan.mark.is_none()
            }
            2 => {
                matches!(plan.target, Some(RefTarget::Short { .. }))
                    && plan.mark == Some(1)
                    && plan.mmco.contains(&Mmco::MaxLong(2))
                    && plan.mmco.iter().any(|m| matches!(m, Mmco::ForgetShort(_)))
                    && plan.mmco.last() == Some(&Mmco::CurrentToLong(1))
            }
            _ => matches!(plan.target, Some(RefTarget::Long { slot: 1, .. })),
        };
        if !shape_ok {
            return Err(BackendError::Plan(format!(
                "probe: the manager's plan for \"{what}\" has the wrong shape: {plan:?}"
            )));
        }
        let refs = mgr.resident_refs();
        let au = s.encode(&view, &plan, &refs)?;
        let header = au
            .meta
            .header
            .as_ref()
            .filter(|_| au.meta.header_matches(&plan, &cfg.params))
            .ok_or_else(|| {
                BackendError::Incapable(format!("probe: {what}: header differs from the plan"))
            })?;
        let report = mgr
            .frame_sent(&plan, header, 1, now)
            .ok_or_else(|| BackendError::Plan("probe: stale plan".into()))?;
        if !report.matched {
            return Err(BackendError::Incapable(format!(
                "probe: {what}: the stream does not reference what the plan names"
            )));
        }
        mgr.frag_acked(plan.frame_seq, 0, now);
        if n == 0 {
            mgr.frame_decoded(0);
        }
    }
    Ok(())
}

pub(crate) struct VaapiDirect {
    session: Option<Session>,
}

impl VaapiDirect {
    pub(crate) fn new() -> Self {
        Self { session: None }
    }
}

impl H264Backend for VaapiDirect {
    fn caps(&self) -> BackendCaps {
        BackendCaps {
            explicit_refs: true,
            max_lt: 2,
            max_ref_frames: self
                .session
                .as_ref()
                .map_or(4, |s| s.cfg.params.max_num_ref_frames as u8),
            vbr: true,
        }
    }

    fn open(&mut self, cfg: &StreamConfig) -> Result<(), BackendError> {
        self.session = None;
        // Checked before anything touches libva, so it holds everywhere.
        if cfg.width < 128 || cfg.height < 128 {
            return Err(BackendError::Incapable("frame smaller than 128x128".into()));
        }
        SeqConfig {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
        }
        .validate()
        .map_err(BackendError::Incapable)?;
        probe_cached(cfg)?;
        self.session = Some(Session::open(cfg)?);
        Ok(())
    }

    fn encode(
        &mut self,
        nv12: &Nv12View<'_>,
        plan: &FramePlan,
        refs: &[ResidentRef],
    ) -> Result<EncodedAu, BackendError> {
        let s = self.session.as_mut().ok_or(BackendError::Failed)?;
        s.encode(nv12, plan, refs).inspect_err(|e| {
            warn!(err = %e, "libva-direct encode failed; backend is down");
        })
    }

    fn set_rate(&mut self, rate: RateTarget) {
        // Bits per second only; RC and HRD go out on the next P frame.
        let Some(s) = self.session.as_mut() else {
            return;
        };
        if rate != s.rate {
            s.rate = rate;
            s.rate_dirty = true;
        }
    }

    fn feedback(&mut self, _fb: BackendFeedback) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_rejects_what_the_writer_cannot_express() {
        for (w, h, fps) in [
            (1367, 768, 30),
            (1366, 767, 30),
            (64, 64, 30),
            (3840, 2160, 120),
        ] {
            let mut b = VaapiDirect::new();
            let err = b
                .open(&StreamConfig::new(w, h, fps, 4_000_000))
                .unwrap_err();
            assert!(
                matches!(err, BackendError::Incapable(_)),
                "{w}x{h}@{fps}: {err}"
            );
        }
    }
}
