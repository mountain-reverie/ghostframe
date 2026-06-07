//! VAAPI / DRM device-context helpers shared by the H.264 encoders.
//!
//! These functions wrap raw FFmpeg `AVBufferRef` lifecycles. They are all
//! `unsafe` because they take or return raw FFmpeg buffer pointers; callers
//! must ensure the underlying ffmpeg library has been initialized
//! (`ffmpeg::init()`).

use std::ffi::CString;
use std::ptr;

use ffmpeg::frame;
use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as ffi;

/// Default VA-API render device path.
pub const VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// RAII wrapper around `*mut AVBufferRef` so we don't leak hw contexts.
pub struct BufRef(pub *mut ffi::AVBufferRef);

impl Drop for BufRef {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe { ffi::av_buffer_unref(&mut self.0) };
        }
    }
}

// SAFETY: AVBufferRef is refcounted and thread-safe.
unsafe impl Send for BufRef {}

/// Create a VA-API hardware device context for the given render node.
pub unsafe fn create_hw_device_ctx(
    device_path: &str,
) -> Result<*mut ffi::AVBufferRef, ffmpeg::Error> {
    let device_cstr = CString::new(device_path).map_err(|_| ffmpeg::Error::InvalidData)?;
    let mut hw_device_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
    let ret = ffi::av_hwdevice_ctx_create(
        &mut hw_device_ctx,
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
        device_cstr.as_ptr(),
        ptr::null_mut(),
        0,
    );
    if ret < 0 {
        return Err(ffmpeg::Error::from(ret));
    }
    Ok(hw_device_ctx)
}

/// Create a DRM device context. Used to derive a VAAPI context that supports
/// DRM PRIME frame import (zero-copy DMA-BUF upload to hardware surfaces).
pub unsafe fn create_drm_device_ctx(
    device_path: &str,
) -> Result<*mut ffi::AVBufferRef, ffmpeg::Error> {
    let device_cstr = CString::new(device_path).map_err(|_| ffmpeg::Error::InvalidData)?;
    let mut drm_ctx: *mut ffi::AVBufferRef = ptr::null_mut();
    let ret = ffi::av_hwdevice_ctx_create(
        &mut drm_ctx,
        ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_DRM,
        device_cstr.as_ptr(),
        ptr::null_mut(),
        0,
    );
    if ret < 0 {
        return Err(ffmpeg::Error::from(ret));
    }
    Ok(drm_ctx)
}

/// Create a VA-API hardware frames context (NV12 sw format).
pub unsafe fn create_hw_frames_ctx(
    hw_device_ctx: *mut ffi::AVBufferRef,
    width: u32,
    height: u32,
    pool_size: i32,
) -> Result<*mut ffi::AVBufferRef, ffmpeg::Error> {
    let hw_frames_ref = ffi::av_hwframe_ctx_alloc(hw_device_ctx);
    if hw_frames_ref.is_null() {
        return Err(ffmpeg::Error::InvalidData);
    }

    let frames_ctx = (*hw_frames_ref).data as *mut ffi::AVHWFramesContext;
    (*frames_ctx).format = ffi::AVPixelFormat::AV_PIX_FMT_VAAPI;
    (*frames_ctx).sw_format = ffi::AVPixelFormat::AV_PIX_FMT_NV12;
    (*frames_ctx).width = width as i32;
    (*frames_ctx).height = height as i32;
    (*frames_ctx).initial_pool_size = pool_size;

    let ret = ffi::av_hwframe_ctx_init(hw_frames_ref);
    if ret < 0 {
        ffi::av_buffer_unref(&mut { hw_frames_ref });
        return Err(ffmpeg::Error::from(ret));
    }

    Ok(hw_frames_ref)
}

/// Upload a software NV12 frame to a VA-API hardware surface.
pub unsafe fn upload_to_hw_surface(
    hw_frames_ctx: *mut ffi::AVBufferRef,
    sw_frame: &frame::Video,
) -> Result<frame::Video, ffmpeg::Error> {
    let mut hw_frame = frame::Video::empty();
    let hw_ptr = hw_frame.as_mut_ptr();

    let ret = ffi::av_hwframe_get_buffer(hw_frames_ctx, hw_ptr, 0);
    if ret < 0 {
        return Err(ffmpeg::Error::from(ret));
    }

    let ret = ffi::av_hwframe_transfer_data(hw_ptr, sw_frame.as_ptr(), 0);
    if ret < 0 {
        return Err(ffmpeg::Error::from(ret));
    }

    Ok(hw_frame)
}
