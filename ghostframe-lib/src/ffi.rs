//! C FFI for ghostframe-lib.
//!
//! The Rust `GhostframeServer` is heap-allocated inside an `FfiHandle` that
//! also owns the tokio `Runtime`.  The runtime must outlive the server because
//! `GhostframeServer::new()` spawns a background task (the IoBridge event
//! loop) that runs on that runtime.

use std::ffi::CStr;
use std::os::raw::c_char;

use crate::server::{FrameSubmission, GhostframeServer};
use crate::transport::ghostbridge::GhostbridgeConfig;

/// Bundles a `GhostframeServer` with the tokio `Runtime` that owns its
/// background tasks.  Drop order matters: the server (and its spawned
/// IoBridge task) must be dropped *before* the runtime shuts down.
///
/// Opaque to C callers (cbindgen emits a forward declaration).
/// Rust callers should use `GhostframeServer` directly.
#[doc(hidden)]
pub struct FfiHandle {
    server: GhostframeServer,
    /// Kept alive so the IoBridge task spawned inside `GhostframeServer::new`
    /// continues to run.  Dropped after `server`.
    _rt: tokio::runtime::Runtime,
}

/// Opaque pointer returned to C callers.
pub type GfServerHandle = *mut FfiHandle;

/// Create and start a new GhostframeServer.
/// Returns a handle on success, or null on failure.
///
/// # Safety
/// All string pointers must be valid, NUL-terminated C strings.
#[no_mangle]
pub unsafe extern "C" fn gf_server_new(
    hostname: *const c_char,
    authkey: *const c_char,
    state_dir: *const c_char,
    control_url: *const c_char,
) -> GfServerHandle {
    let config = GhostbridgeConfig {
        hostname: CStr::from_ptr(hostname).to_string_lossy().into_owned(),
        authkey: CStr::from_ptr(authkey).to_string_lossy().into_owned(),
        state_dir: CStr::from_ptr(state_dir).to_string_lossy().into_owned(),
        control_url: CStr::from_ptr(control_url).to_string_lossy().into_owned(),
    };

    let rt = match tokio::runtime::Runtime::new() {
        Ok(rt) => rt,
        Err(_) => return std::ptr::null_mut(),
    };

    match rt.block_on(GhostframeServer::new(config, ":4443")) {
        Ok(server) => Box::into_raw(Box::new(FfiHandle { server, _rt: rt })),
        Err(e) => {
            eprintln!("gf_server_new failed: {e}");
            std::ptr::null_mut()
        }
    }
}

/// Submit a frame for tiling and transmission.
/// Returns 0 on success, -1 on failure.
///
/// # Safety
/// `handle` must be a valid pointer from `gf_server_new`.
/// `pixels` must be valid for `stride * height` bytes.
#[no_mangle]
pub unsafe extern "C" fn gf_server_submit_frame(
    handle: GfServerHandle,
    width: u32,
    height: u32,
    stride: u32,
    pixels: *const u8,
    timestamp_us: u32,
) -> i32 {
    if handle.is_null() || pixels.is_null() {
        return -1;
    }
    let ffi = &*handle;
    let size = (stride * height) as usize;
    let pixel_data = std::slice::from_raw_parts(pixels, size).to_vec();

    let frame = FrameSubmission {
        width,
        height,
        stride,
        pixels: pixel_data,
        dmabuf_fd: None,
        timestamp_us,
        damage_tiles: None,
        capture_done_ns: 0,
    };

    // Use the stored runtime to drive the async send.
    if ffi._rt.block_on(ffi.server.submit_frame(frame)).is_ok() {
        0
    } else {
        -1
    }
}

/// Destroy a GhostframeServer and free its resources.
///
/// # Safety
/// `handle` must be a valid pointer from `gf_server_new`, or null.
#[no_mangle]
pub unsafe extern "C" fn gf_server_destroy(handle: GfServerHandle) {
    if !handle.is_null() {
        let _ = Box::from_raw(handle);
    }
}
