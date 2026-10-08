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

    let lib_config = crate::config::LibConfig::from_env();
    match rt.block_on(GhostframeServer::new(
        config, ":443", lib_config, None, None,
    )) {
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

/// Number of WebTransport clients currently connected.
///
/// `0` means no consumer exists for a frame, so the caller's capture loop
/// should skip the scrape entirely: without this gate a capture backend keeps
/// copying full framebuffers at the frame rate regardless of demand. Returns
/// `0` for a null handle, which is also the "do not capture" answer — a
/// caller that lost its handle has nothing to feed anyway.
///
/// Non-blocking. Prefer `gf_server_wait_for_client` while idle rather than
/// polling this in a sleep loop: the sleep interval lands in full between a
/// client connecting and the first frame being captured.
///
/// # Safety
/// `handle` must be a valid pointer from `gf_server_new`, or null.
#[no_mangle]
pub unsafe extern "C" fn gf_server_connected_session_count(handle: GfServerHandle) -> usize {
    if handle.is_null() {
        return 0;
    }
    (*handle).server.connected_session_count()
}

/// Block until at least one WebTransport client is connected.
///
/// This is the idle gate for a C capture loop: it wakes on the connect event
/// itself, so the first frame goes out without the latency a poll interval
/// would add. Returns as soon as a client is already connected.
///
/// `timeout_ms` of 0 waits indefinitely.
///
/// Returns:
/// - `> 0` — the number of connected clients; start capturing.
/// - `0`   — `timeout_ms` elapsed with no client. Call again to keep waiting.
/// - `-1`  — terminal: null handle, or the server event loop has exited and no
///   client can ever arrive. The caller should stop, not retry.
///
/// Blocks the calling thread. The server's own event loop runs on its internal
/// multi-threaded runtime and keeps making progress meanwhile, which is what
/// lets the client this call is waiting for actually connect.
///
/// # Safety
/// `handle` must be a valid pointer from `gf_server_new`, or null.
#[no_mangle]
pub unsafe extern "C" fn gf_server_wait_for_client(handle: GfServerHandle, timeout_ms: u32) -> i32 {
    if handle.is_null() {
        return -1;
    }
    let ffi = &*handle;
    let wait = ffi.server.wait_for_client();

    let connected = if timeout_ms == 0 {
        match ffi._rt.block_on(wait) {
            Ok(n) => n,
            Err(_) => return -1,
        }
    } else {
        let dur = std::time::Duration::from_millis(u64::from(timeout_ms));
        match ffi._rt.block_on(tokio::time::timeout(dur, wait)) {
            // Timed out: no client yet, but the server is still alive.
            Err(_) => return 0,
            Ok(Ok(n)) => n,
            Ok(Err(_)) => return -1,
        }
    };

    // `wait_for_client` only returns Ok on a non-zero count, so this cannot
    // collide with the timeout return. Saturate rather than wrap: a count
    // past i32::MAX is impossible here (it is bounded by live QUIC
    // connections), and clamping keeps the sign contract regardless.
    i32::try_from(connected).unwrap_or(i32::MAX)
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

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A C embedder with no handle must be told "no consumer", not crash.
    #[test]
    fn connected_session_count_of_null_is_zero() {
        assert_eq!(
            unsafe { gf_server_connected_session_count(std::ptr::null_mut()) },
            0
        );
    }

    /// The null check has to come *before* the wait, not after. A caller that
    /// passes a null handle (or one whose `gf_server_new` returned null and
    /// went unchecked) must get the terminal -1 immediately. Reorder the two
    /// and this call blocks the thread forever on a channel that does not
    /// exist -- a hang with no log line, which is the single worst failure
    /// mode this FFI can have. `timeout_ms` is 0 here, the
    /// wait-indefinitely value, precisely so the test hangs rather than
    /// passing by luck if the order is ever broken.
    #[test]
    fn wait_for_client_rejects_null_without_blocking() {
        assert_eq!(
            unsafe { gf_server_wait_for_client(std::ptr::null_mut(), 0) },
            -1
        );
    }
}
