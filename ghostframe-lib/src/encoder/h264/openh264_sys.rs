// SPDX-License-Identifier: AGPL-3.0-or-later
//! Minimal hand-written bindings for Cisco's openh264 encoder, loaded with
//! `dlopen` at run time.
//!
//! openh264 is neither shipped nor linked. The library is looked up the way
//! the loader finds any system library (`libopenh264.so.8`, `.so.7`, `.so`),
//! or at the absolute path the operator configured. The structs below are
//! transcribed from `codec_api.h`, `codec_app_def.h` and `codec_def.h`
//! (openh264 v2.6.0, BSD-2-Clause, Copyright (c) 2014 Cisco Systems, Inc.;
//! see NOTICE); only the calls and fields the encoder uses are named.
//!
//! # ABI
//!
//! `SEncParamExt`, `SSourcePicture` and `SLayerBSInfo` grew in 2.6.0:
//! `bPsnrY/U/V` were appended to the first two and `float rPsnr[3]` to the
//! layer info, which sits in a 128-element array inside `SFrameBSInfo`. The
//! array stride and everything after it differ between the 2.4.x/2.5.x layout
//! and the 2.6.x one. The 2.6 layout is allocated for every struct the library
//! fills or reads (zero-filled, so an older library reads a prefix and a newer
//! one sees zeroed new fields) and the per-layer stride is read by version.
//! Any other version is refused: a guess would write through the wrong
//! offsets. The vtable is append-only across versions.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The sonames tried, in order, when no path is configured: 2.6.x, 2.4.x and
/// 2.5.x (Ubuntu 24.04, Debian 13, Fedora's Cisco repository), then whatever
/// the unversioned name points at.
pub(crate) const SONAMES: [&str; 3] = ["libopenh264.so.8", "libopenh264.so.7", "libopenh264.so"];

/// The sentence Cisco's patent grant asks for wherever the user controls the
/// codec (OpenH264 binary licence, condition 3).
pub(crate) const CISCO_ATTRIBUTION: &str = "OpenH264 Video Codec provided by Cisco Systems, Inc.";

pub(crate) const MAX_LAYER_NUM_OF_FRAME: usize = 128;
pub(crate) const MAX_SLICES_NUM_TMP: usize = 35;
pub(crate) const MAX_SPATIAL_LAYER_NUM: usize = 4;

// EUsageType
pub(crate) const CAMERA_VIDEO_REAL_TIME: c_int = 0;
// RC_MODES
pub(crate) const RC_BITRATE_MODE: c_int = 1;
// ECOMPLEXITY_MODE
pub(crate) const LOW_COMPLEXITY: c_int = 0;
pub(crate) const MEDIUM_COMPLEXITY: c_int = 1;
// EParameterSetStrategy
pub(crate) const CONSTANT_ID: c_int = 0;
// SliceModeEnum
pub(crate) const SM_SINGLE_SLICE: c_int = 0;
// EVideoFormatType / EVideoFrameType
pub(crate) const VIDEO_FORMAT_I420: c_int = 23;
pub(crate) const VIDEO_FRAME_TYPE_INVALID: c_int = 0;
pub(crate) const VIDEO_FRAME_TYPE_IDR: c_int = 1;
pub(crate) const VIDEO_FRAME_TYPE_SKIP: c_int = 4;
// LAYER_NUM
pub(crate) const SPATIAL_LAYER_0: c_int = 0;
// KEY_FRAME_REQUEST_TYPE
pub(crate) const LTR_RECOVERY_REQUEST: c_uint = 1;
pub(crate) const LTR_MARKING_SUCCESS: c_uint = 4;
pub(crate) const LTR_MARKING_FAILED: c_uint = 5;
// ENCODER_OPTION
pub(crate) const ENCODER_OPTION_DATAFORMAT: c_int = 0;
pub(crate) const ENCODER_OPTION_BITRATE: c_int = 5;
pub(crate) const ENCODER_OPTION_MAX_BITRATE: c_int = 6;
pub(crate) const ENCODER_LTR_RECOVERY_REQUEST: c_int = 15;
pub(crate) const ENCODER_LTR_MARKING_FEEDBACK: c_int = 16;
pub(crate) const ENCODER_OPTION_TRACE_LEVEL: c_int = 25;
pub(crate) const ENCODER_OPTION_TRACE_CALLBACK: c_int = 26;
pub(crate) const ENCODER_OPTION_TRACE_CALLBACK_CONTEXT: c_int = 27;
// WELS_LOG_*
const WELS_LOG_ERROR: c_int = 1;
pub(crate) const WELS_LOG_WARNING: c_int = 2;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Version {
    pub major: c_uint,
    pub minor: c_uint,
    pub revision: c_uint,
    pub reserved: c_uint,
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.revision)
    }
}

/// Which `SLayerBSInfo` / `SFrameBSInfo` layout a library speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Layout {
    /// 2.4.x and 2.5.x: 40-byte layer info.
    V24,
    /// 2.6.x: 56-byte layer info (`rPsnr[3]` appended).
    V26,
}

impl Layout {
    /// The layout of a supported version, `None` for any other.
    pub(crate) fn for_version(v: Version) -> Option<Self> {
        match (v.major, v.minor) {
            (2, 4) | (2, 5) => Some(Self::V24),
            (2, 6) => Some(Self::V26),
            _ => None,
        }
    }

    pub(crate) fn layer_stride(self) -> usize {
        match self {
            Self::V24 => 40,
            Self::V26 => 56,
        }
    }

    /// Offset of `SFrameBSInfo::eFrameType`, after the layer array.
    fn frame_type_offset(self) -> usize {
        8 + MAX_LAYER_NUM_OF_FRAME * self.layer_stride()
    }

    fn frame_size_offset(self) -> usize {
        self.frame_type_offset() + 4
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SSliceArgument {
    pub ui_slice_mode: c_int,
    pub ui_slice_num: c_uint,
    pub ui_slice_mb_num: [c_uint; MAX_SLICES_NUM_TMP],
    pub ui_slice_size_constraint: c_uint,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SSpatialLayerConfig {
    pub i_video_width: c_int,
    pub i_video_height: c_int,
    pub f_frame_rate: f32,
    pub i_spatial_bitrate: c_int,
    pub i_max_spatial_bitrate: c_int,
    pub ui_profile_idc: c_int,
    pub ui_level_idc: c_int,
    pub i_dlayer_qp: c_int,
    pub s_slice_argument: SSliceArgument,
    pub b_video_signal_type_present: bool,
    pub ui_video_format: u8,
    pub b_full_range: bool,
    pub b_color_description_present: bool,
    pub ui_color_primaries: u8,
    pub ui_transfer_characteristics: u8,
    pub ui_color_matrix: u8,
    pub b_aspect_ratio_present: bool,
    pub e_aspect_ratio: c_int,
    pub s_aspect_ratio_ext_width: u16,
    pub s_aspect_ratio_ext_height: u16,
}

/// `SEncParamExt`, 2.6 layout (2.4/2.5 end three bytes earlier, before
/// `bPsnrY`).
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct SEncParamExt {
    pub i_usage_type: c_int,
    pub i_pic_width: c_int,
    pub i_pic_height: c_int,
    pub i_target_bitrate: c_int,
    pub i_rc_mode: c_int,
    pub f_max_frame_rate: f32,
    pub i_temporal_layer_num: c_int,
    pub i_spatial_layer_num: c_int,
    pub s_spatial_layers: [SSpatialLayerConfig; MAX_SPATIAL_LAYER_NUM],
    pub i_complexity_mode: c_int,
    pub ui_intra_period: c_uint,
    pub i_num_ref_frame: c_int,
    pub e_sps_pps_id_strategy: c_int,
    pub b_prefix_nal_adding_ctrl: bool,
    pub b_enable_ssei: bool,
    pub b_simulcast_avc: bool,
    pub i_padding_flag: c_int,
    pub i_entropy_coding_mode_flag: c_int,
    pub b_enable_frame_skip: bool,
    pub i_max_bitrate: c_int,
    pub i_max_qp: c_int,
    pub i_min_qp: c_int,
    pub ui_max_nal_size: c_uint,
    pub b_enable_long_term_reference: bool,
    pub i_ltr_ref_num: c_int,
    pub i_ltr_mark_period: c_uint,
    pub i_multiple_thread_idc: u16,
    pub b_use_load_balancing: bool,
    pub i_loop_filter_disable_idc: c_int,
    pub i_loop_filter_alpha_c0_offset: c_int,
    pub i_loop_filter_beta_offset: c_int,
    pub b_enable_denoise: bool,
    pub b_enable_background_detection: bool,
    pub b_enable_adaptive_quant: bool,
    pub b_enable_frame_cropping_flag: bool,
    pub b_enable_scene_change_detect: bool,
    pub b_is_lossless_link: bool,
    pub b_fix_rc_over_shoot: bool,
    pub i_idr_bitrate_ratio: c_int,
    pub b_psnr_y: bool,
    pub b_psnr_u: bool,
    pub b_psnr_v: bool,
}

/// `SSourcePicture`, 2.6 layout.
#[repr(C)]
pub(crate) struct SSourcePicture {
    pub i_color_format: c_int,
    pub i_stride: [c_int; 4],
    pub p_data: [*mut u8; 4],
    pub i_pic_width: c_int,
    pub i_pic_height: c_int,
    pub ui_time_stamp: i64,
    pub b_psnr_y: bool,
    pub b_psnr_u: bool,
    pub b_psnr_v: bool,
}

/// `SLayerBSInfo`, 2.6 layout. Every field before `r_psnr` sits at the same
/// offset in the 2.4/2.5 layout, so reading them through this type at the old
/// stride is sound.
#[repr(C)]
pub(crate) struct SLayerBSInfo {
    pub ui_temporal_id: u8,
    pub ui_spatial_id: u8,
    pub ui_quality_id: u8,
    pub e_frame_type: c_int,
    pub ui_layer_type: u8,
    pub i_sub_seq_id: c_int,
    pub i_nal_count: c_int,
    pub p_nal_length_in_byte: *mut c_int,
    pub p_bs_buf: *mut u8,
    pub r_psnr: [f32; 3],
}

#[repr(C)]
pub(crate) struct SLtrRecoverRequest {
    pub ui_feedback_type: c_uint,
    pub ui_idr_pic_id: c_uint,
    pub i_last_correct_frame_num: c_int,
    pub i_current_frame_num: c_int,
    pub i_layer_id: c_int,
}

#[repr(C)]
pub(crate) struct SLtrMarkingFeedback {
    pub ui_feedback_type: c_uint,
    pub ui_idr_pic_id: c_uint,
    pub i_ltr_frame_num: c_int,
    pub i_layer_id: c_int,
}

#[repr(C)]
pub(crate) struct SBitrateInfo {
    pub i_layer: c_int,
    pub i_bitrate: c_int,
}

/// `SFrameBSInfo`, allocated in the largest (2.6) layout and read through
/// the stride of the library's own.
pub(crate) struct FrameBs {
    // u64 words: the structure holds pointers and a 64-bit time stamp.
    buf: Box<[u64]>,
    layout: Layout,
}

impl FrameBs {
    const BYTES: usize = 8 + MAX_LAYER_NUM_OF_FRAME * 56 + 4 + 4 + 8;

    pub(crate) fn new(layout: Layout) -> Self {
        Self {
            buf: vec![0u64; Self::BYTES.div_ceil(8)].into_boxed_slice(),
            layout,
        }
    }

    pub(crate) fn clear(&mut self) {
        self.buf.fill(0);
    }

    fn base(&self) -> *const u8 {
        self.buf.as_ptr().cast()
    }

    fn read_i32(&self, off: usize) -> i32 {
        assert!(off + 4 <= self.buf.len() * 8);
        // SAFETY: in bounds (checked), 4-aligned (the buffer is 8-aligned and
        // every offset used is a multiple of 4).
        unsafe { self.base().add(off).cast::<i32>().read() }
    }

    pub(crate) fn layer_num(&self) -> usize {
        self.read_i32(0).clamp(0, MAX_LAYER_NUM_OF_FRAME as i32) as usize
    }

    pub(crate) fn frame_type(&self) -> i32 {
        self.read_i32(self.layout.frame_type_offset())
    }

    pub(crate) fn frame_size(&self) -> i32 {
        self.read_i32(self.layout.frame_size_offset())
    }

    /// The bytes of layer `i`: its NAL units, back to back (start codes
    /// included), and the lengths the library reported. `None` when the
    /// library's own bookkeeping is inconsistent: more NAL units than an
    /// access unit of ours holds, a length table that is not 4-aligned, a
    /// length that is negative or runs past `limit` (the frame size the
    /// library reported), or lengths that sum past it. Those are checked
    /// before any pointer in the layer is followed, so a layer read at the
    /// wrong stride fails here instead of faulting.
    ///
    /// # Safety
    /// The buffer must have been filled by a successful `EncodeFrame` and the
    /// encoder must not have run again since: `pBsBuf` points into the
    /// encoder's output buffer.
    pub(crate) unsafe fn layer_bytes(&self, i: usize, limit: usize) -> Option<&[u8]> {
        if i >= MAX_LAYER_NUM_OF_FRAME {
            return None;
        }
        let off = 8 + i * self.layout.layer_stride();
        // SAFETY: the layer lies inside the buffer (i < 128, buffer holds the
        // 2.6 stride), and the prefix fields read here are at the same
        // offsets in both layouts.
        let layer = unsafe { &*self.base().add(off).cast::<SLayerBSInfo>() };
        let n = usize::try_from(layer.i_nal_count).ok()?;
        if n == 0 {
            return Some(&[]);
        }
        if n > MAX_NALS_PER_LAYER
            || layer.p_nal_length_in_byte.is_null()
            || !layer.p_nal_length_in_byte.is_aligned()
            || layer.p_bs_buf.is_null()
        {
            return None;
        }
        // SAFETY: openh264 reports `n` lengths and a buffer of their sum.
        let lens = unsafe { std::slice::from_raw_parts(layer.p_nal_length_in_byte, n) };
        let mut total = 0usize;
        for &l in lens {
            total = total.checked_add(usize::try_from(l).ok()?)?;
            if total > limit {
                return None;
            }
        }
        // SAFETY: as above.
        Some(unsafe { std::slice::from_raw_parts(layer.p_bs_buf, total) })
    }
}

/// The most NAL units one layer of an access unit of this backend holds.
const MAX_NALS_PER_LAYER: usize = 16;

type EncoderPtr = *mut *const Vtbl;

/// `ISVCEncoderVtbl`. Only the head of the table is named; the rest is not
/// needed and the table is append-only.
#[repr(C)]
struct Vtbl {
    initialize: unsafe extern "C" fn(EncoderPtr, *const c_void) -> c_int,
    initialize_ext: unsafe extern "C" fn(EncoderPtr, *const SEncParamExt) -> c_int,
    get_default_params: unsafe extern "C" fn(EncoderPtr, *mut SEncParamExt) -> c_int,
    uninitialize: unsafe extern "C" fn(EncoderPtr) -> c_int,
    encode_frame: unsafe extern "C" fn(EncoderPtr, *const SSourcePicture, *mut c_void) -> c_int,
    encode_parameter_sets: unsafe extern "C" fn(EncoderPtr, *mut c_void) -> c_int,
    force_intra_frame: unsafe extern "C" fn(EncoderPtr, bool) -> c_int,
    set_option: unsafe extern "C" fn(EncoderPtr, c_int, *mut c_void) -> c_int,
    get_option: unsafe extern "C" fn(EncoderPtr, c_int, *mut c_void) -> c_int,
}

type TraceFn = unsafe extern "C" fn(*mut c_void, c_int, *const c_char);

/// openh264's own log lines, into `tracing`. Its warnings are routine (it
/// complains that frame skip is off at every rate change), so they go to
/// debug; errors are errors.
unsafe extern "C" fn trace(_ctx: *mut c_void, level: c_int, msg: *const c_char) {
    if msg.is_null() {
        return;
    }
    // SAFETY: openh264 passes a NUL-terminated string valid for the call.
    let text = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
    let text = text.trim_end();
    if level & WELS_LOG_ERROR != 0 {
        tracing::error!(target: "ghostframe::openh264", "{text}");
    } else {
        tracing::debug!(target: "ghostframe::openh264", "{text}");
    }
}

/// A loaded library: the entry points and what version it is.
pub(crate) struct Lib {
    pub version: Version,
    pub layout: Layout,
    /// What was opened: the soname or the configured path.
    pub origin: String,
    create: unsafe extern "C" fn(*mut EncoderPtr) -> c_int,
    destroy: unsafe extern "C" fn(EncoderPtr),
}

type LoadResult = Result<Lib, String>;

fn dlopen(name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).map_err(|_| format!("{name:?} has a NUL"))?;
    // SAFETY: plain dlopen by name or absolute path. The handle is never
    // closed: the library stays mapped for the life of the process.
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        // SAFETY: dlerror returns a thread-local string or null.
        let e = unsafe { libc::dlerror() };
        let why = if e.is_null() {
            String::from("unknown error")
        } else {
            // SAFETY: non-null, NUL-terminated.
            unsafe { CStr::from_ptr(e) }.to_string_lossy().into_owned()
        };
        return Err(why);
    }
    Ok(h)
}

fn dlsym(h: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).expect("symbol name has no NUL");
    // SAFETY: `h` is a live handle from `dlopen`.
    let p = unsafe { libc::dlsym(h, c.as_ptr()) };
    if p.is_null() {
        return Err(format!("missing symbol {name}"));
    }
    Ok(p)
}

impl Lib {
    /// Load one candidate and confirm its ABI.
    fn open_one(name: &str) -> Result<Lib, String> {
        let h = dlopen(name)?;
        let ver = dlsym(h, "WelsGetCodecVersion")?;
        let create = dlsym(h, "WelsCreateSVCEncoder")?;
        let destroy = dlsym(h, "WelsDestroySVCEncoder")?;
        // SAFETY: the symbols have the declared C signatures.
        let (version, create, destroy) = unsafe {
            let get_version = std::mem::transmute::<*mut c_void, extern "C" fn() -> Version>(ver);
            (
                get_version(),
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(*mut EncoderPtr) -> c_int>(
                    create,
                ),
                std::mem::transmute::<*mut c_void, unsafe extern "C" fn(EncoderPtr)>(destroy),
            )
        };
        let Some(layout) = Layout::for_version(version) else {
            return Err(format!(
                "openh264 {version} is not supported (2.4.x, 2.5.x and 2.6.x are)"
            ));
        };
        Ok(Lib {
            version,
            layout,
            origin: name.to_string(),
            create,
            destroy,
        })
    }

    /// Try the candidates in order; the first that loads and has a supported
    /// version wins. The error names every one and why it failed: one line
    /// for the operator.
    pub(crate) fn load_from(candidates: &[String]) -> LoadResult {
        let mut why = Vec::new();
        for c in candidates {
            match Self::open_one(c) {
                Ok(l) => return Ok(l),
                Err(e) => why.push(format!("{c}: {e}")),
            }
        }
        Err(format!(
            "no usable openh264 library ({}); install libopenh264 (Debian/Ubuntu: \
             libopenh264-7, Fedora: the Cisco repository) or set \
             GHOSTFRAME_H264_OPENH264_LIB",
            why.join("; ")
        ))
    }

    /// The library at `path` when one is configured (and no other: an explicit
    /// choice that fails is an error, not a hint), else the first of
    /// [`SONAMES`] the loader finds. Loaded once per distinct `path`; a
    /// failure is cached too.
    pub(crate) fn get(path: Option<&Path>) -> Result<&'static Lib, &'static str> {
        static CACHE: Mutex<Vec<(Option<PathBuf>, &'static LoadResult)>> = Mutex::new(Vec::new());
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        let key = path.map(Path::to_path_buf);
        let entry = match cache.iter().find(|(k, _)| *k == key) {
            Some((_, r)) => *r,
            None => {
                let candidates: Vec<String> = match path {
                    Some(p) => vec![p.to_string_lossy().into_owned()],
                    None => SONAMES.iter().map(|s| (*s).to_string()).collect(),
                };
                let r: &'static LoadResult = Box::leak(Box::new(Self::load_from(&candidates)));
                cache.push((key, r));
                r
            }
        };
        entry.as_ref().map_err(String::as_str)
    }

    /// The library this process is configured to use
    /// (`GHOSTFRAME_H264_OPENH264_LIB`, read through `config` like the
    /// daemon's), for tests and the browserless harness, so that they load
    /// what the daemon would: the same path into [`Lib::get`], no second search.
    #[cfg(any(test, feature = "browserless-harness"))]
    pub(crate) fn configured_path() -> Option<PathBuf> {
        crate::config::TransportConfig::from_env().openh264_lib
    }

    pub(crate) fn create_encoder(&'static self) -> Result<Encoder, String> {
        let mut raw: EncoderPtr = std::ptr::null_mut();
        // SAFETY: the symbol is WelsCreateSVCEncoder.
        let rc = unsafe { (self.create)(&mut raw) };
        if rc != 0 || raw.is_null() {
            return Err(format!("WelsCreateSVCEncoder returned {rc}"));
        }
        Ok(Encoder {
            raw,
            lib: self,
            initialized: false,
        })
    }
}

/// An `ISVCEncoder`. `Uninitialize` and `Destroy` on drop.
pub(crate) struct Encoder {
    raw: EncoderPtr,
    lib: &'static Lib,
    initialized: bool,
}

// SAFETY: the encoder is used from one thread at a time (`&mut self`); it has
// no thread affinity.
unsafe impl Send for Encoder {}

impl Encoder {
    fn vt(&self) -> &Vtbl {
        // SAFETY: `raw` points at the library's object, whose first word is
        // the vtable pointer; both outlive `self`.
        unsafe { &**self.raw }
    }

    pub(crate) fn default_params(&mut self) -> Result<SEncParamExt, c_int> {
        // SAFETY: all-zero is a valid SEncParamExt (integers, floats, bools).
        let mut p: SEncParamExt = unsafe { std::mem::zeroed() };
        // SAFETY: `p` is at least as large as the library's struct.
        let rc = unsafe { (self.vt().get_default_params)(self.raw, &mut p) };
        if rc == 0 {
            Ok(p)
        } else {
            Err(rc)
        }
    }

    pub(crate) fn initialize_ext(&mut self, p: &SEncParamExt) -> c_int {
        // SAFETY: `p` is the 2.6 layout, a superset of any supported one.
        let rc = unsafe { (self.vt().initialize_ext)(self.raw, p) };
        self.initialized = rc == 0;
        rc
    }

    pub(crate) fn set_option<T>(&mut self, option: c_int, value: &mut T) -> c_int {
        // SAFETY: each option's payload type is the caller's; the library
        // reads or writes through the pointer for the call only.
        unsafe { (self.vt().set_option)(self.raw, option, (value as *mut T).cast()) }
    }

    /// Route the library's log to `tracing`.
    pub(crate) fn install_trace(&mut self) {
        let mut level = WELS_LOG_WARNING;
        let rc = self.set_option(ENCODER_OPTION_TRACE_LEVEL, &mut level);
        let mut cb: TraceFn = trace;
        let rc2 = self.set_option(ENCODER_OPTION_TRACE_CALLBACK, &mut cb);
        let mut ctx: *mut c_void = std::ptr::null_mut();
        let rc3 = self.set_option(ENCODER_OPTION_TRACE_CALLBACK_CONTEXT, &mut ctx);
        if rc != 0 || rc2 != 0 || rc3 != 0 {
            tracing::debug!(rc, rc2, rc3, "openh264 trace callback not installed");
        }
    }

    pub(crate) fn force_intra(&mut self) -> c_int {
        // SAFETY: plain call.
        unsafe { (self.vt().force_intra_frame)(self.raw, true) }
    }

    pub(crate) fn encode(&mut self, pic: &SSourcePicture, out: &mut FrameBs) -> c_int {
        out.clear();
        // SAFETY: `out` is the largest layout; the picture planes the caller
        // set up are valid for the call.
        unsafe { (self.vt().encode_frame)(self.raw, pic, out.buf.as_mut_ptr().cast()) }
    }

    pub(crate) fn lib(&self) -> &'static Lib {
        self.lib
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        if self.initialized {
            // SAFETY: a live, initialised encoder.
            unsafe { (self.vt().uninitialize)(self.raw) };
        }
        // SAFETY: created by this library's WelsCreateSVCEncoder.
        unsafe { (self.lib.destroy)(self.raw) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    // Printed by `tools/openh264-fixture/layout.c` (see that file for the
    // recipe), compiled once against the v2.4.1 headers and once against the
    // v2.6.0 headers, x86-64:
    //
    //   struct                 2.4.1  2.6.0
    //   SEncParamExt             920    924
    //   SSpatialLayerConfig      200    200
    //   SSourcePicture            72     80
    //   SLayerBSInfo              40     56
    //   SFrameBSInfo            5144   7192
    //   SFrameBSInfo.eFrameType 5128   7176
    //   .iFrameSizeInBytes      5132   7180
    //
    // Headers: https://raw.githubusercontent.com/cisco/openh264/v2.4.1/
    // codec/api/wels/{codec_api,codec_app_def,codec_def,codec_ver}.h and the
    // same under v2.6.0 (v2.5.0 has the 2.4.1 layout).
    #[test]
    fn openh264_layouts_match_the_c_headers() {
        // The 2.6 structs we allocate.
        assert_eq!(size_of::<SEncParamExt>(), 924);
        assert_eq!(size_of::<SSpatialLayerConfig>(), 200);
        assert_eq!(size_of::<SSliceArgument>(), 152);
        assert_eq!(size_of::<SSourcePicture>(), 80);
        assert_eq!(size_of::<SLayerBSInfo>(), 56);
        assert_eq!(FrameBs::BYTES.div_ceil(8) * 8, 7192);
        // The 2.4 prefix: the same struct minus the appended fields.
        assert_eq!(offset_of!(SEncParamExt, b_psnr_y), 920);
        assert_eq!(offset_of!(SSourcePicture, b_psnr_y), 72);
        assert_eq!(offset_of!(SLayerBSInfo, r_psnr), 40);
        assert_eq!(Layout::V24.layer_stride(), 40);
        assert_eq!(Layout::V26.layer_stride(), 56);
        assert_eq!(Layout::V24.frame_type_offset(), 5128);
        assert_eq!(Layout::V24.frame_size_offset(), 5132);
        assert_eq!(Layout::V26.frame_type_offset(), 7176);
        assert_eq!(Layout::V26.frame_size_offset(), 7180);

        // Offsets (identical in both versions).
        assert_eq!(offset_of!(SEncParamExt, i_pic_width), 4);
        assert_eq!(offset_of!(SEncParamExt, i_target_bitrate), 12);
        assert_eq!(offset_of!(SEncParamExt, i_rc_mode), 16);
        assert_eq!(offset_of!(SEncParamExt, f_max_frame_rate), 20);
        assert_eq!(offset_of!(SEncParamExt, i_temporal_layer_num), 24);
        assert_eq!(offset_of!(SEncParamExt, i_spatial_layer_num), 28);
        assert_eq!(offset_of!(SEncParamExt, s_spatial_layers), 32);
        assert_eq!(offset_of!(SEncParamExt, i_complexity_mode), 832);
        assert_eq!(offset_of!(SEncParamExt, ui_intra_period), 836);
        assert_eq!(offset_of!(SEncParamExt, i_num_ref_frame), 840);
        assert_eq!(offset_of!(SEncParamExt, e_sps_pps_id_strategy), 844);
        assert_eq!(offset_of!(SEncParamExt, b_prefix_nal_adding_ctrl), 848);
        assert_eq!(offset_of!(SEncParamExt, i_entropy_coding_mode_flag), 856);
        assert_eq!(offset_of!(SEncParamExt, b_enable_frame_skip), 860);
        assert_eq!(offset_of!(SEncParamExt, i_max_bitrate), 864);
        assert_eq!(offset_of!(SEncParamExt, i_max_qp), 868);
        assert_eq!(offset_of!(SEncParamExt, i_min_qp), 872);
        assert_eq!(offset_of!(SEncParamExt, ui_max_nal_size), 876);
        assert_eq!(offset_of!(SEncParamExt, b_enable_long_term_reference), 880);
        assert_eq!(offset_of!(SEncParamExt, i_ltr_ref_num), 884);
        assert_eq!(offset_of!(SEncParamExt, i_ltr_mark_period), 888);
        assert_eq!(offset_of!(SEncParamExt, i_multiple_thread_idc), 892);
        assert_eq!(offset_of!(SEncParamExt, i_loop_filter_disable_idc), 896);
        assert_eq!(offset_of!(SEncParamExt, b_enable_denoise), 908);
        assert_eq!(offset_of!(SEncParamExt, b_enable_scene_change_detect), 912);
        assert_eq!(offset_of!(SEncParamExt, i_idr_bitrate_ratio), 916);
        assert_eq!(offset_of!(SSpatialLayerConfig, f_frame_rate), 8);
        assert_eq!(offset_of!(SSpatialLayerConfig, i_spatial_bitrate), 12);
        assert_eq!(offset_of!(SSpatialLayerConfig, i_max_spatial_bitrate), 16);
        assert_eq!(offset_of!(SSpatialLayerConfig, s_slice_argument), 32);
        assert_eq!(offset_of!(SSourcePicture, i_stride), 4);
        assert_eq!(offset_of!(SSourcePicture, p_data), 24);
        assert_eq!(offset_of!(SSourcePicture, i_pic_width), 56);
        assert_eq!(offset_of!(SSourcePicture, ui_time_stamp), 64);
        assert_eq!(offset_of!(SLayerBSInfo, e_frame_type), 4);
        assert_eq!(offset_of!(SLayerBSInfo, i_nal_count), 16);
        assert_eq!(offset_of!(SLayerBSInfo, p_nal_length_in_byte), 24);
        assert_eq!(offset_of!(SLayerBSInfo, p_bs_buf), 32);
        assert_eq!(size_of::<SLtrRecoverRequest>(), 20);
        assert_eq!(size_of::<SLtrMarkingFeedback>(), 16);
        assert_eq!(size_of::<SBitrateInfo>(), 8);
        assert_eq!(size_of::<Version>(), 16);
    }

    #[test]
    fn openh264_only_the_versions_with_a_known_layout_are_supported() {
        let v = |major, minor| Version {
            major,
            minor,
            revision: 0,
            reserved: 0,
        };
        assert_eq!(Layout::for_version(v(2, 4)), Some(Layout::V24));
        assert_eq!(Layout::for_version(v(2, 5)), Some(Layout::V24));
        assert_eq!(Layout::for_version(v(2, 6)), Some(Layout::V26));
        for other in [v(2, 3), v(2, 7), v(1, 8), v(3, 0)] {
            assert_eq!(Layout::for_version(other), None, "{other}");
        }
    }

    #[test]
    fn openh264_frame_buffer_reads_the_layer_stride_of_its_layout() {
        // Two layers placed by hand at each layout's stride: reading at the
        // wrong stride lands in the wrong layer.
        for layout in [Layout::V24, Layout::V26] {
            let mut fb = FrameBs::new(layout);
            let bytes: [u8; 6] = [0, 0, 0, 1, 0x65, 0x88];
            let lens = [6i32];
            let base = fb.buf.as_mut_ptr().cast::<u8>();
            // SAFETY: inside the buffer; test-only pokes at the layout.
            unsafe {
                base.cast::<i32>().write(2);
                let l1 = base.add(8 + layout.layer_stride()).cast::<SLayerBSInfo>();
                (*l1).i_nal_count = 1;
                (*l1).p_nal_length_in_byte = lens.as_ptr().cast_mut();
                (*l1).p_bs_buf = bytes.as_ptr().cast_mut();
                base.add(layout.frame_type_offset())
                    .cast::<i32>()
                    .write(VIDEO_FRAME_TYPE_IDR);
                base.add(layout.frame_size_offset()).cast::<i32>().write(6);
            }
            assert_eq!(fb.layer_num(), 2);
            assert_eq!(fb.frame_type(), VIDEO_FRAME_TYPE_IDR);
            assert_eq!(fb.frame_size(), 6);
            // SAFETY: the pointers above outlive the reads.
            unsafe {
                assert_eq!(fb.layer_bytes(0, 6), Some(&[][..]));
                assert_eq!(fb.layer_bytes(1, 6), Some(&bytes[..]));
                // A frame size smaller than the lengths is inconsistent.
                assert_eq!(fb.layer_bytes(1, 5), None);
            }
        }
    }

    /// A layer header read at the wrong stride is garbage: it is refused
    /// before a pointer in it is followed.
    #[test]
    fn openh264_frame_buffer_refuses_a_layer_that_is_not_one() {
        let mut fb = FrameBs::new(Layout::V26);
        let base = fb.buf.as_mut_ptr().cast::<u8>();
        // SAFETY: inside the buffer. The pointers are bogus and must never be
        // followed, which is what is being tested.
        unsafe {
            base.cast::<i32>().write(2);
            let l = base.add(8).cast::<SLayerBSInfo>();
            // Too many NAL units for the table to be real.
            (*l).i_nal_count = i32::MAX;
            (*l).p_nal_length_in_byte = 0x1000 as *mut i32;
            (*l).p_bs_buf = 0x2000 as *mut u8;
            assert_eq!(fb.layer_bytes(0, 100), None);
            // A plausible count and a misaligned table.
            (*l).i_nal_count = 3;
            (*l).p_nal_length_in_byte = 0x1001 as *mut i32;
            assert_eq!(fb.layer_bytes(0, 100), None);
        }
    }

    #[test]
    fn openh264_missing_library_names_what_was_tried() {
        let err = Lib::load_from(&[
            "/nonexistent/libopenh264-test.so".to_string(),
            "libopenh264-no-such-soname.so.99".to_string(),
        ])
        .err()
        .expect("neither exists");
        assert!(err.contains("/nonexistent/libopenh264-test.so"), "{err}");
        assert!(err.contains("libopenh264-no-such-soname.so.99"), "{err}");
        assert!(err.contains("GHOSTFRAME_H264_OPENH264_LIB"), "{err}");
    }
}
