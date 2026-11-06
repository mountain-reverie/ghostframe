//! Minimal hand-written libva bindings for the direct H.264 encoder.
//!
//! libva and libva-drm are `dlopen`ed, so a machine without them (or without
//! a usable device) simply fails to open the backend and the caller falls
//! back. Only the calls and structs the encoder uses are declared; every
//! struct is `repr(C)` and checked against the sizes and offsets a C compiler
//! reports for `<va/va.h>` and `<va/va_enc_h264.h>` (libva 2.24), see the
//! tests. Bit-field unions are plain `u32`s with the bit positions named
//! below, also checked against the compiler.

use std::ffi::{c_char, c_int, c_uint, c_void, CStr, CString};
use std::sync::OnceLock;

pub(crate) type VADisplay = *mut c_void;
pub(crate) type VAStatus = c_int;
pub(crate) type VAGenericID = c_uint;
pub(crate) type VAConfigID = VAGenericID;
pub(crate) type VAContextID = VAGenericID;
pub(crate) type VASurfaceID = VAGenericID;
pub(crate) type VABufferID = VAGenericID;
pub(crate) type VAImageID = VAGenericID;

pub(crate) const VA_INVALID_ID: c_uint = 0xffff_ffff;
pub(crate) const VA_PROGRESSIVE: c_int = 1;
pub(crate) const VA_RT_FORMAT_YUV420: c_uint = 0x1;
pub(crate) const VA_FOURCC_NV12: u32 = 0x3231_564E;
pub(crate) const VA_LSB_FIRST: u32 = 1;

pub(crate) const VA_PROFILE_H264_HIGH: c_int = 7;
pub(crate) const VA_ENTRYPOINT_ENC_SLICE: c_int = 6;

pub(crate) const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
pub(crate) const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
pub(crate) const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;

pub(crate) const VA_RC_VBR: u32 = 0x4;
pub(crate) const VA_RC_CQP: u32 = 0x10;
pub(crate) const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 1;
pub(crate) const VA_ENC_PACKED_HEADER_PICTURE: u32 = 2;
pub(crate) const VA_ENC_PACKED_HEADER_SLICE: u32 = 4;

pub(crate) const VA_ENC_CODED_BUFFER_TYPE: c_int = 21;
pub(crate) const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: c_int = 22;
pub(crate) const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: c_int = 23;
pub(crate) const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: c_int = 24;
pub(crate) const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: c_int = 25;
pub(crate) const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: c_int = 26;
pub(crate) const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: c_int = 27;

pub(crate) const VA_PACKED_HEADER_SEQUENCE: u32 = 1;
pub(crate) const VA_PACKED_HEADER_PICTURE: u32 = 2;
pub(crate) const VA_PACKED_HEADER_SLICE: u32 = 3;

pub(crate) const VA_MISC_FRAME_RATE: u32 = 0;
pub(crate) const VA_MISC_RATE_CONTROL: u32 = 1;
pub(crate) const VA_MISC_MAX_FRAME_SIZE: u32 = 4;
pub(crate) const VA_MISC_HRD: u32 = 5;

pub(crate) const VA_PICTURE_H264_INVALID: u32 = 0x1;
pub(crate) const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x8;
pub(crate) const VA_PICTURE_H264_LONG_TERM_REFERENCE: u32 = 0x10;

pub(crate) const VA_PADDING_LOW: usize = 4;

/// `VACodedBufferSegment.status`: the coded buffer was too small.
pub(crate) const VA_CODED_BUF_STATUS_SLICE_OVERFLOW_MASK: u32 = 0x200;

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAConfigAttrib {
    pub type_: c_int,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub top_field_order_cnt: i32,
    pub bottom_field_order_cnt: i32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

impl VAPictureH264 {
    pub(crate) const INVALID: Self = Self {
        picture_id: VA_INVALID_ID,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        top_field_order_cnt: 0,
        bottom_field_order_cnt: 0,
        va_reserved: [0; VA_PADDING_LOW],
    };
}

/// `seq_fields` bit positions.
pub(crate) mod seq_bits {
    pub(crate) const CHROMA_FORMAT_IDC: u32 = 0;
    pub(crate) const FRAME_MBS_ONLY: u32 = 2;
    pub(crate) const DIRECT_8X8_INFERENCE: u32 = 5;
    pub(crate) const LOG2_MAX_FRAME_NUM_MINUS4: u32 = 6;
    pub(crate) const POC_TYPE: u32 = 10;
    pub(crate) const LOG2_MAX_POC_LSB_MINUS4: u32 = 12;
}

/// `vui_fields` bit positions.
pub(crate) mod vui_bits {
    pub(crate) const TIMING_INFO: u32 = 1;
    pub(crate) const BITSTREAM_RESTRICTION: u32 = 2;
    pub(crate) const LOG2_MAX_MV_LENGTH_HORIZONTAL: u32 = 3;
    pub(crate) const LOG2_MAX_MV_LENGTH_VERTICAL: u32 = 8;
    pub(crate) const FIXED_FRAME_RATE: u32 = 13;
    pub(crate) const MV_OVER_PIC_BOUNDARIES: u32 = 15;
}

/// `pic_fields` bit positions (`reference_pic_flag` is two bits wide).
pub(crate) mod pic_bits {
    pub(crate) const IDR: u32 = 0;
    pub(crate) const REFERENCE: u32 = 1;
    pub(crate) const ENTROPY_CODING_MODE: u32 = 3;
    pub(crate) const DEBLOCKING_CONTROL_PRESENT: u32 = 9;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncPictureParameterBufferH264 {
    pub curr_pic: VAPictureH264,
    pub reference_frames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    pub pic_fields: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub ref_pic_list0: [VAPictureH264; 32],
    pub ref_pic_list1: [VAPictureH264; 32],
    /// Weighted-prediction tables: unused, always zero.
    pub weights: [u8; 778],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    pub rc_flags: u32,
    pub icq_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// `VAEncMiscParameterBufferMaxFrameSize` (va.h): a deprecated `type` word
/// first, which Mesa would otherwise read as the cap, then the cap in bits.
#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncMiscParameterBufferMaxFrameSize {
    pub type_: u32,
    pub max_frame_size: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VAEncMiscParameterFrameRate {
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub(crate) struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; VA_PADDING_LOW],
}

/// Plain-data structs for which all-zero bytes is a valid value: integers,
/// arrays of them and raw pointers, no references, no enums.
///
/// # Safety
/// Only implement for such types.
pub(crate) unsafe trait Zeroable: Copy {}

macro_rules! zeroable {
    ($($t:ty),*) => { $(
        // SAFETY: repr(C) struct of integers, integer arrays and raw pointers.
        unsafe impl Zeroable for $t {}
    )* };
}
zeroable!(
    VAImage,
    VAImageFormat,
    VAEncSequenceParameterBufferH264,
    VAEncPictureParameterBufferH264,
    VAEncSliceParameterBufferH264,
    VAEncMiscParameterRateControl
);

pub(crate) fn zeroed<T: Zeroable>() -> T {
    // SAFETY: `Zeroable` promises all-zero bytes is a valid `T`.
    unsafe { std::mem::zeroed() }
}

macro_rules! va_fns {
    ($($field:ident : $sym:literal => fn($($arg:ty),*) -> $ret:ty;)*) => {
        /// The loaded entry points. Lives for the process (the libraries are
        /// never unloaded).
        pub(crate) struct Va {
            $(pub(crate) $field: unsafe extern "C" fn($($arg),*) -> $ret,)*
        }

        impl Va {
            fn load_symbols(va: *mut c_void, drm: *mut c_void) -> Result<Self, String> {
                Ok(Self {
                    $($field: {
                        let p = sym(if $sym == "vaGetDisplayDRM" { drm } else { va }, $sym)?;
                        // SAFETY: the symbol has the declared C signature.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) -> $ret>(p) }
                    },)*
                })
            }
        }
    };
}

va_fns! {
    get_display_drm: "vaGetDisplayDRM" => fn(c_int) -> VADisplay;
    initialize: "vaInitialize" => fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus;
    terminate: "vaTerminate" => fn(VADisplay) -> VAStatus;
    error_str: "vaErrorStr" => fn(VAStatus) -> *const c_char;
    get_config_attributes: "vaGetConfigAttributes" => fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int) -> VAStatus;
    create_config: "vaCreateConfig" => fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int, *mut VAConfigID) -> VAStatus;
    destroy_config: "vaDestroyConfig" => fn(VADisplay, VAConfigID) -> VAStatus;
    create_surfaces: "vaCreateSurfaces" => fn(VADisplay, c_uint, c_uint, c_uint, *mut VASurfaceID, c_uint, *mut c_void, c_uint) -> VAStatus;
    destroy_surfaces: "vaDestroySurfaces" => fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus;
    create_context: "vaCreateContext" => fn(VADisplay, VAConfigID, c_int, c_int, c_int, *mut VASurfaceID, c_int, *mut VAContextID) -> VAStatus;
    destroy_context: "vaDestroyContext" => fn(VADisplay, VAContextID) -> VAStatus;
    create_buffer: "vaCreateBuffer" => fn(VADisplay, VAContextID, c_int, c_uint, c_uint, *const c_void, *mut VABufferID) -> VAStatus;
    destroy_buffer: "vaDestroyBuffer" => fn(VADisplay, VABufferID) -> VAStatus;
    map_buffer: "vaMapBuffer" => fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus;
    unmap_buffer: "vaUnmapBuffer" => fn(VADisplay, VABufferID) -> VAStatus;
    begin_picture: "vaBeginPicture" => fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus;
    render_picture: "vaRenderPicture" => fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus;
    end_picture: "vaEndPicture" => fn(VADisplay, VAContextID) -> VAStatus;
    sync_surface: "vaSyncSurface" => fn(VADisplay, VASurfaceID) -> VAStatus;
    create_image: "vaCreateImage" => fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus;
    destroy_image: "vaDestroyImage" => fn(VADisplay, VAImageID) -> VAStatus;
    put_image: "vaPutImage" => fn(VADisplay, VASurfaceID, VAImageID, c_int, c_int, c_uint, c_uint, c_int, c_int, c_uint, c_uint) -> VAStatus;
}

fn open_lib(name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).expect("library name has no NUL");
    // SAFETY: plain dlopen of a system library by soname.
    let h = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if h.is_null() {
        return Err(format!("cannot load {name}"));
    }
    Ok(h)
}

fn sym(lib: *mut c_void, name: &str) -> Result<*mut c_void, String> {
    let c = CString::new(name).expect("symbol name has no NUL");
    // SAFETY: `lib` is a live handle from `open_lib`.
    let p = unsafe { libc::dlsym(lib, c.as_ptr()) };
    if p.is_null() {
        return Err(format!("missing symbol {name}"));
    }
    Ok(p)
}

impl Va {
    /// Load libva and libva-drm once; the result (including a failure) is
    /// cached.
    pub(crate) fn get() -> Result<&'static Va, &'static str> {
        static VA: OnceLock<Result<Va, String>> = OnceLock::new();
        VA.get_or_init(|| {
            let va = open_lib("libva.so.2")?;
            let drm = open_lib("libva-drm.so.2")?;
            Va::load_symbols(va, drm)
        })
        .as_ref()
        .map_err(String::as_str)
    }

    pub(crate) fn error_string(&self, status: VAStatus) -> String {
        // SAFETY: vaErrorStr returns a static NUL-terminated string.
        unsafe {
            let p = (self.error_str)(status);
            if p.is_null() {
                format!("status {status}")
            } else {
                CStr::from_ptr(p).to_string_lossy().into_owned()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    // Printed by a C program compiled against <va/va.h> and
    // <va/va_enc_h264.h> (libva 2.24, x86-64).
    #[test]
    fn layouts_match_the_c_headers() {
        assert_eq!(size_of::<VAConfigAttrib>(), 8);
        assert_eq!(size_of::<VAImageFormat>(), 48);
        assert_eq!(size_of::<VAImage>(), 120);
        assert_eq!(size_of::<VAPictureH264>(), 36);
        assert_eq!(size_of::<VAEncSequenceParameterBufferH264>(), 1132);
        assert_eq!(size_of::<VAEncPictureParameterBufferH264>(), 648);
        assert_eq!(size_of::<VAEncSliceParameterBufferH264>(), 3140);
        assert_eq!(size_of::<VAEncPackedHeaderParameterBuffer>(), 28);
        assert_eq!(size_of::<VAEncMiscParameterRateControl>(), 60);
        assert_eq!(size_of::<VAEncMiscParameterHRD>(), 24);
        assert_eq!(size_of::<VAEncMiscParameterFrameRate>(), 24);
        assert_eq!(size_of::<VAEncMiscParameterBufferMaxFrameSize>(), 24);
        assert_eq!(
            offset_of!(VAEncMiscParameterBufferMaxFrameSize, max_frame_size),
            4
        );
        assert_eq!(size_of::<VACodedBufferSegment>(), 48);

        assert_eq!(offset_of!(VAImage, buf), 52);
        assert_eq!(offset_of!(VAImage, width), 56);
        assert_eq!(offset_of!(VAImage, data_size), 60);
        assert_eq!(offset_of!(VAImage, num_planes), 64);
        assert_eq!(offset_of!(VAImage, pitches), 68);
        assert_eq!(offset_of!(VAImage, offsets), 80);
        assert_eq!(offset_of!(VAPictureH264, frame_idx), 4);
        assert_eq!(offset_of!(VAPictureH264, flags), 8);
        assert_eq!(offset_of!(VAPictureH264, top_field_order_cnt), 12);
        assert_eq!(offset_of!(VAPictureH264, bottom_field_order_cnt), 16);

        type Seq = VAEncSequenceParameterBufferH264;
        assert_eq!(offset_of!(Seq, level_idc), 1);
        assert_eq!(offset_of!(Seq, intra_period), 4);
        assert_eq!(offset_of!(Seq, bits_per_second), 16);
        assert_eq!(offset_of!(Seq, picture_width_in_mbs), 24);
        assert_eq!(offset_of!(Seq, seq_fields), 28);
        assert_eq!(offset_of!(Seq, bit_depth_luma_minus8), 32);
        assert_eq!(offset_of!(Seq, num_ref_frames_in_pic_order_cnt_cycle), 34);
        assert_eq!(offset_of!(Seq, offset_for_non_ref_pic), 36);
        assert_eq!(offset_of!(Seq, offset_for_ref_frame), 44);
        assert_eq!(offset_of!(Seq, frame_cropping_flag), 1068);
        assert_eq!(offset_of!(Seq, frame_crop_bottom_offset), 1084);
        assert_eq!(offset_of!(Seq, vui_parameters_present_flag), 1088);
        assert_eq!(offset_of!(Seq, vui_fields), 1092);
        assert_eq!(offset_of!(Seq, aspect_ratio_idc), 1096);
        assert_eq!(offset_of!(Seq, num_units_in_tick), 1108);
        assert_eq!(offset_of!(Seq, time_scale), 1112);

        type Pic = VAEncPictureParameterBufferH264;
        assert_eq!(offset_of!(Pic, reference_frames), 36);
        assert_eq!(offset_of!(Pic, coded_buf), 612);
        assert_eq!(offset_of!(Pic, pic_parameter_set_id), 616);
        assert_eq!(offset_of!(Pic, frame_num), 620);
        assert_eq!(offset_of!(Pic, pic_init_qp), 622);
        assert_eq!(offset_of!(Pic, chroma_qp_index_offset), 625);
        assert_eq!(offset_of!(Pic, pic_fields), 628);

        type Slice = VAEncSliceParameterBufferH264;
        assert_eq!(offset_of!(Slice, macroblock_info), 8);
        assert_eq!(offset_of!(Slice, slice_type), 12);
        assert_eq!(offset_of!(Slice, idr_pic_id), 14);
        assert_eq!(offset_of!(Slice, pic_order_cnt_lsb), 16);
        assert_eq!(offset_of!(Slice, delta_pic_order_cnt_bottom), 20);
        assert_eq!(offset_of!(Slice, direct_spatial_mv_pred_flag), 32);
        assert_eq!(offset_of!(Slice, ref_pic_list0), 36);
        assert_eq!(offset_of!(Slice, ref_pic_list1), 1188);
        assert_eq!(offset_of!(Slice, weights), 2340);
        assert_eq!(offset_of!(Slice, cabac_init_idc), 3118);
        assert_eq!(offset_of!(Slice, slice_beta_offset_div2), 3122);

        assert_eq!(offset_of!(VAEncPackedHeaderParameterBuffer, bit_length), 4);
        assert_eq!(
            offset_of!(VAEncPackedHeaderParameterBuffer, has_emulation_bytes),
            8
        );
        assert_eq!(offset_of!(VAEncMiscParameterRateControl, rc_flags), 24);
        assert_eq!(offset_of!(VAEncMiscParameterRateControl, max_qp), 32);
        assert_eq!(
            offset_of!(VAEncMiscParameterRateControl, target_frame_size),
            40
        );
        assert_eq!(offset_of!(VACodedBufferSegment, buf), 16);
        assert_eq!(offset_of!(VACodedBufferSegment, next), 24);
    }

    // The bit-field unions as the compiler packs them for the field values
    // the backend sets.
    #[test]
    fn bit_positions_match_the_c_headers() {
        use seq_bits as s;
        let seq = 1 << s::CHROMA_FORMAT_IDC
            | 1 << s::FRAME_MBS_ONLY
            | 1 << s::DIRECT_8X8_INFERENCE
            | 3 << s::LOG2_MAX_FRAME_NUM_MINUS4
            | 12 << s::LOG2_MAX_POC_LSB_MINUS4;
        assert_eq!(seq, 0xc0e5);
        use vui_bits as v;
        let vui = 1 << v::TIMING_INFO
            | 1 << v::BITSTREAM_RESTRICTION
            | 16 << v::LOG2_MAX_MV_LENGTH_HORIZONTAL
            | 16 << v::LOG2_MAX_MV_LENGTH_VERTICAL
            | 1 << v::FIXED_FRAME_RATE
            | 1 << v::MV_OVER_PIC_BOUNDARIES;
        assert_eq!(vui, 0xb086);
        use pic_bits as p;
        let pic = 1 << p::IDR
            | 1 << p::REFERENCE
            | 1 << p::ENTROPY_CODING_MODE
            | 1 << p::DEBLOCKING_CONTROL_PRESENT;
        assert_eq!(pic, 0x20b);
    }
}
