//! C ABI shim over `ghostframe-client-native`.
//!
//! This crate holds NO logic of its own: every `gf_*` function validates
//! its arguments, converts to/from the Rust types in [`ghostframe_client_native`],
//! and returns. The exceptions are the small amount of bookkeeping needed to
//! keep a `gf_frame`'s `damage` pointer valid between
//! `gf_client_acquire_frame` and `gf_client_release_frame`, and the lock that
//! makes the threading contract on [`gf_client`] true.
//!
//! ## No panic may cross the FFI boundary
//!
//! Unwinding out of an `extern "C" fn` into C code is undefined behaviour.
//! Every function whose body can panic (a `.unwrap()` deep in a dependency,
//! an allocation failure, ...) is wrapped in [`std::panic::catch_unwind`];
//! a caught panic is reported as [`gf_result::GF_ERR_INVALID`] (or, for
//! functions that don't return a `gf_result`, treated as a no-op/error
//! sentinel appropriate to that function).
//!
//! ## `struct_size`
//!
//! [`gf_client_config`], [`gf_event`] and [`gf_frame`] all start with a
//! `struct_size` field the caller must set to `sizeof(...)` of the type it
//! was built against. This revision accepts EXACTLY the size it was built
//! with and rejects anything else with `GF_ERR_INVALID`: it does not read or
//! write a shorter or longer struct. The field exists so a future revision
//! can recognise an older caller and handle it; no such handling exists yet.
//! The field is read through a raw pointer before any reference to the
//! struct is formed, so a wrong size never causes a reference to memory the
//! caller did not promise.

#![allow(non_camel_case_types)] // `gf_client` follows C naming, not Rust's.

pub mod types;

pub use types::*;

use ghostframe_client_native::{Client, ClientError, ClientEvent, Config};
use std::collections::HashMap;
use std::ffi::CStr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::{Mutex, RwLock, RwLockReadGuard, TryLockError};

/// Most export buffers a host may ask for. Each is a GPU allocation; an
/// unchecked `u32` here is a remote-sized allocation loop.
const MAX_EXPORT_BUFFERS: u32 = 16;
/// Most preferred DRM modifiers a host may list. Real drivers report a few
/// dozen at most.
const MAX_PREFERRED_MODIFIERS: u32 = 256;

/// An embedded ghostframe session.
///
/// ## Threading contract
///
/// - Every `gf_client_*` function may be called from any thread, including
///   concurrently with any other, EXCEPT `gf_client_destroy`.
/// - `gf_client_destroy` must be the last call: no other call on the same
///   handle may be running or start afterwards. The library cannot enforce
///   this; calling it concurrently is undefined behaviour.
/// - `gf_client_connect` and `gf_client_disconnect` take the handle
///   exclusively for their duration (`connect` blocks for seconds; they
///   wait for each other rather than cancelling). While one runs, other
///   threads' calls do not block: `gf_client_acquire_frame` returns
///   `GF_AGAIN`, `gf_client_push_*` and `gf_client_request_display_mode`
///   return `GF_ERR_STATE`, and `gf_client_release_frame` has no effect
///   (retry it afterwards). `gf_client_next_event`, `gf_client_event_fd`
///   and `gf_client_supports_h264` always work, so the event fd can be put
///   in a poll set before `connect`.
/// - Events are not tied to a thread: `gf_client_next_event` may be called
///   from any thread, and each event is delivered to exactly one caller.
///
/// Frames do not survive `gf_client_disconnect`: every `gf_frame` acquired
/// before it, with its fds and `damage` pointer, is invalid afterwards (do
/// not release it), and frame ids restart on the next `gf_client_connect`.
/// Like `gf_client_destroy`, disconnect must be externally synchronised
/// against any thread still using an acquired frame (importing its fds or
/// reading `damage`).
pub struct gf_client {
    // Read-locked (with `try_read`, never blocking) by every call;
    // write-locked only by connect/disconnect. This is what makes the
    // contract above sound: no `&mut Client` ever coexists with a `&Client`.
    inner: RwLock<Client>,
    // Cached at create time so they stay answerable while connect holds the
    // write lock: both are fixed for the `Client`'s life.
    event_fd: i32,
    supports_h264: bool,
    // The event queue has its own lock, so events stay readable (and the
    // level-triggered event fd stays drainable) while connect holds the
    // write lock.
    events: std::sync::Arc<ghostframe_client_native::event::EventQueue>,
    // Not part of the public contract: per-`frame_id` stash of the converted
    // damage list, so `gf_frame::damage` has somewhere stable to point until
    // `gf_client_release_frame`. `Client::acquire_frame` hands back an owned
    // `Vec` that would otherwise be dropped when the call returns.
    damage: Mutex<HashMap<u32, Vec<gf_rect>>>,
}

// A `gf_client` is shared across threads by contract.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<gf_client>();
};

impl gf_client {
    /// Shared access, or `None` while connect/disconnect holds the handle.
    fn read(&self) -> Option<RwLockReadGuard<'_, Client>> {
        match self.inner.try_read() {
            Ok(g) => Some(g),
            Err(TryLockError::Poisoned(e)) => Some(e.into_inner()),
            Err(TryLockError::WouldBlock) => None,
        }
    }
}

/// Run `f`, catching any panic and mapping it to `err`. Every `extern "C"
/// fn` body goes through this (or [`guard_void`]) -- see the module doc.
fn guard<F: FnOnce() -> gf_result>(err: gf_result, f: F) -> gf_result {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(err)
}

/// Like [`guard`], for functions with no return value.
fn guard_void<F: FnOnce()>(f: F) {
    let _ = catch_unwind(AssertUnwindSafe(f));
}

fn map_client_error(e: &ClientError) -> gf_result {
    match e {
        ClientError::Gpu(_) => gf_result::GF_ERR_GPU,
        ClientError::Connect(_) => gf_result::GF_ERR_STATE,
        // `DebugMap` and `Cdf53Coverage` are test/diagnostic-only
        // (`Client::debug_map_frame` / `Client::cdf53_coverage` are not
        // exposed through this C ABI at all), but the match must stay
        // exhaustive -- grouped with the other I/O-flavored failures rather
        // than given its own `gf_result` variant nothing would ever produce
        // through this surface.
        ClientError::Bridge(_)
        | ClientError::Io(_)
        | ClientError::Bootstrap(_)
        | ClientError::Net(_)
        | ClientError::DebugMap(_)
        | ClientError::Cdf53Coverage(_) => gf_result::GF_ERR_IO,
    }
}

/// Write `msg` into a fixed `gf_event` message buffer: control characters
/// (including ESC, so a network-supplied string cannot drive the host's
/// terminal) become `?`, the result is truncated to 255 bytes on a UTF-8
/// character boundary, NUL-terminated, and the tail zeroed.
///
/// Writes through a raw pointer: `dst` points into caller-owned memory that
/// may be uninitialised, and a reference to it would be unsound.
///
/// # Safety
/// `dst` must be valid for writing 256 bytes.
unsafe fn fill_message(dst: *mut [libc::c_char; 256], msg: &str) {
    let mut buf = [0u8; 256];
    let mut n = 0usize;
    for ch in msg.chars() {
        let ch = if ch.is_control() { '?' } else { ch };
        let len = ch.len_utf8();
        if n + len > 255 {
            break;
        }
        ch.encode_utf8(&mut buf[n..n + len]);
        n += len;
    }
    // `.cast()` rather than `as`: `c_char` is `i8` on x86-64 but `u8` on
    // aarch64, so an `as` cast is real on one and a no-op on the other, and
    // `clippy::unnecessary_cast` fires on the no-op (ARM only; CI is x86).
    // SAFETY: caller guarantees 256 writable bytes; `u8` and `c_char` have
    // the same size and alignment.
    unsafe { dst.cast::<[u8; 256]>().write(buf) };
}

/// Create a client. Does not touch the network or the GPU; see
/// `gf_client_connect`.
///
/// `cfg->struct_size` must be `sizeof(gf_client_config)`. `*out` is written
/// only on `GF_OK`. `n_export_buffers` above 16 and `n_preferred_modifiers`
/// above 256 are rejected with `GF_ERR_INVALID`.
///
/// # Safety
/// `cfg` must be null or point to a valid, readable `gf_client_config`.
/// `out` must be null or point to a valid, writable `*mut gf_client`.
#[no_mangle]
pub unsafe extern "C" fn gf_client_create(
    cfg: *const gf_client_config,
    out: *mut *mut gf_client,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if cfg.is_null() || out.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null; reads one `u32` through a raw pointer, forming
        // no reference to a struct whose size is not yet established.
        let claimed = unsafe { std::ptr::addr_of!((*cfg).struct_size).read() };
        if claimed as usize != std::mem::size_of::<gf_client_config>() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: size verified above; validity of the pointee is the
        // caller's contract (documented on this function).
        let cfg = unsafe { &*cfg };

        let hostname = match cstr_to_string(cfg.hostname) {
            Some(s) => s,
            None => return gf_result::GF_ERR_INVALID,
        };
        let state_dir = match cstr_to_string(cfg.state_dir) {
            Some(s) => s,
            None => return gf_result::GF_ERR_INVALID,
        };
        let authkey = match cstr_to_string(cfg.authkey) {
            Some(s) => s,
            None => return gf_result::GF_ERR_INVALID,
        };

        if cfg.n_export_buffers > MAX_EXPORT_BUFFERS
            || cfg.n_preferred_modifiers > MAX_PREFERRED_MODIFIERS
        {
            return gf_result::GF_ERR_INVALID;
        }
        let preferred_modifiers = if cfg.n_preferred_modifiers == 0 {
            Vec::new()
        } else if cfg.preferred_modifiers.is_null()
            || !(cfg.preferred_modifiers as usize).is_multiple_of(std::mem::align_of::<u64>())
        {
            return gf_result::GF_ERR_INVALID;
        } else {
            // SAFETY: non-null and aligned per the check above, count is
            // capped; the caller's contract is that `preferred_modifiers`
            // points to at least `n_preferred_modifiers` readable `u64`s.
            unsafe {
                std::slice::from_raw_parts(
                    cfg.preferred_modifiers,
                    cfg.n_preferred_modifiers as usize,
                )
            }
            .to_vec()
        };

        let config = Config {
            hostname,
            authkey,
            state_dir: state_dir.into(),
            supports_h264: cfg.supports_h264,
            indices_raw: cfg.indices_raw,
            n_export_buffers: cfg.n_export_buffers,
            preferred_modifiers,
            // The C ABI never exposes debug_map_frame, so a C consumer has
            // no use for CPU-mappable exports -- and paying for them would
            // pin every buffer to a small BAR aperture.
            debug_map_frames: false,
            display: None,
        };

        let client = match Client::new(config) {
            Ok(c) => c,
            Err(e) => return map_client_error(&e),
        };

        let handle = Box::new(gf_client {
            event_fd: client.event_fd(),
            supports_h264: client.supports_h264(),
            events: client.event_queue(),
            inner: RwLock::new(client),
            damage: Mutex::new(HashMap::new()),
        });
        // SAFETY: `out` is non-null per the check above.
        unsafe {
            *out = Box::into_raw(handle);
        }
        gf_result::GF_OK
    })
}

/// Destroy a client: disconnects if connected, closes every fd handed out
/// in a `gf_frame`, frees the handle. Must be the last call on the handle;
/// see the threading contract on `gf_client`.
///
/// # Safety
/// `c` must be null or a pointer previously returned by
/// [`gf_client_create`] and not yet passed to `gf_client_destroy`.
#[no_mangle]
pub unsafe extern "C" fn gf_client_destroy(c: *mut gf_client) {
    guard_void(move || {
        if c.is_null() {
            return;
        }
        // SAFETY: non-null, and per this function's contract, a live
        // handle not yet destroyed and used by no other thread --
        // reclaiming it here is exactly what "destroy" means; `Client`'s
        // `Drop` tears down its threads.
        drop(unsafe { Box::from_raw(c) });
    });
}

/// The eventfd to add to the host's `poll`/`epoll` set; readable whenever
/// `gf_client_next_event` has something to return. Valid for the life of the
/// handle, including before `connect`. Returns -1 for a null client.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_event_fd(c: *const gf_client) -> i32 {
    catch_unwind(AssertUnwindSafe(move || {
        if c.is_null() {
            return -1;
        }
        // SAFETY: non-null per the check above; validity is the caller's
        // contract.
        unsafe { &*c }.event_fd
    }))
    .unwrap_or(-1)
}

/// The H.264 capability the client actually advertised.
///
/// `gf_client_config.supports_h264` is a *request*; this is the *answer*. A
/// host that asked for H.264 on a machine without VA-API gets `false` here
/// and a working session on the tile codecs.
///
/// Returns `false` for a null client.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_supports_h264(c: *const gf_client) -> bool {
    catch_unwind(AssertUnwindSafe(move || {
        if c.is_null() {
            return false;
        }
        // SAFETY: non-null per the check above; validity is the caller's
        // contract.
        unsafe { &*c }.supports_h264
    }))
    .unwrap_or(false)
}

/// Pop the oldest queued event. Never blocks, even during connect.
/// Returns `GF_AGAIN` when the queue is empty.
///
/// `out->struct_size` must be `sizeof(gf_event)`. `out` is written only on
/// `GF_OK`; all of its fields are then initialised.
///
/// # Safety
/// `c` and `out` must each be null or point to valid memory as documented
/// on their types.
#[no_mangle]
pub unsafe extern "C" fn gf_client_next_event(
    c: *const gf_client,
    out: *mut gf_event,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() || out.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null; a raw `u32` read, no reference formed.
        let claimed = unsafe { std::ptr::addr_of!((*out).struct_size).read() };
        if claimed as usize != std::mem::size_of::<gf_event>() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null per the checks above; validity is the caller's.
        let handle = unsafe { &*c };
        let Some(ev) = handle.events.pop() else {
            return gf_result::GF_AGAIN;
        };

        // Every field is written through a raw pointer: the caller's memory
        // may be uninitialised (a `malloc`ed `gf_event`), and a `&mut`
        // over a Rust enum / `bool` that holds garbage is already UB.
        let o = out;
        let (kind, width, height, frame_id, expected, message): (_, u32, u32, u32, bool, &str) =
            match &ev {
                ClientEvent::Connected => (gf_event_type::GF_EVENT_CONNECTED, 0, 0, 0, false, ""),
                ClientEvent::Disconnected { reason, expected } => (
                    gf_event_type::GF_EVENT_DISCONNECTED,
                    0,
                    0,
                    0,
                    *expected,
                    reason.as_str(),
                ),
                ClientEvent::Resized { width, height } => (
                    gf_event_type::GF_EVENT_RESIZED,
                    *width,
                    *height,
                    0,
                    false,
                    "",
                ),
                ClientEvent::FrameReady { frame_id } => (
                    gf_event_type::GF_EVENT_FRAME_READY,
                    0,
                    0,
                    *frame_id,
                    false,
                    "",
                ),
                ClientEvent::Error { message } => (
                    gf_event_type::GF_EVENT_ERROR,
                    0,
                    0,
                    0,
                    false,
                    message.as_str(),
                ),
            };
        // SAFETY: `o` is non-null and points at a `gf_event` of the size
        // the caller promised.
        unsafe {
            std::ptr::addr_of_mut!((*o).kind).write(kind);
            std::ptr::addr_of_mut!((*o).width).write(width);
            std::ptr::addr_of_mut!((*o).height).write(height);
            std::ptr::addr_of_mut!((*o).frame_id).write(frame_id);
            std::ptr::addr_of_mut!((*o).disconnect_expected).write(expected);
            fill_message(std::ptr::addr_of_mut!((*o).message), message);
        }
        gf_result::GF_OK
    })
}

/// Bring the session up: join the tailnet, open the transport, start the
/// library's threads. BLOCKS for seconds. Takes the handle exclusively for
/// its duration (see the threading contract on `gf_client`).
///
/// `host` must be NUL-terminated UTF-8. Returns `GF_ERR_STATE` if already
/// connected, `GF_ERR_GPU` if no usable GPU, `GF_ERR_IO` for network and
/// bootstrap failures.
///
/// # Safety
/// `c` and `host` must each be null or point to valid memory as documented
/// on their types; `host` must be NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn gf_client_connect(
    c: *mut gf_client,
    host: *const libc::c_char,
    port: u16,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() || host.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        let Some(host) = cstr_to_string(host) else {
            return gf_result::GF_ERR_INVALID;
        };
        // SAFETY: non-null; a shared reference, as every call but destroy
        // forms. Exclusivity over the `Client` comes from the write lock.
        let handle = unsafe { &*c };
        let mut client = handle.inner.write().unwrap_or_else(|e| e.into_inner());
        match client.connect(&host, port) {
            Ok(()) => gf_result::GF_OK,
            Err(e) => map_client_error(&e),
        }
    })
}

/// Tear the session down: stop the library's threads and close the
/// tailnet session. Idempotent. Takes the handle exclusively for its
/// duration.
///
/// Invalidates every frame acquired before it: their fds are closed and
/// their `damage` pointers freed. Do not use or release them afterwards.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_disconnect(c: *mut gf_client) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: as in `gf_client_connect`.
        let handle = unsafe { &*c };
        let mut client = handle.inner.write().unwrap_or_else(|e| e.into_inner());
        let r = match client.disconnect() {
            Ok(()) => gf_result::GF_OK,
            Err(e) => map_client_error(&e),
        };
        // The frames died with the session; their ids restart on reconnect,
        // so a stale entry here would collide with a new frame's.
        handle
            .damage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        r
    })
}

/// Take the newest ready frame. Never blocks. Returns `GF_AGAIN` when no
/// frame is ready (or while connect/disconnect is running).
///
/// `out->struct_size` must be `sizeof(gf_frame)`. `out` is written only on
/// `GF_OK`. Every frame returned must be passed to
/// `gf_client_release_frame` exactly once (unless `gf_client_disconnect`
/// intervenes: see the threading contract on `gf_client`).
///
/// # Safety
/// `c` and `out` must each be null or point to valid memory as documented
/// on their types.
#[no_mangle]
pub unsafe extern "C" fn gf_client_acquire_frame(
    c: *const gf_client,
    out: *mut gf_frame,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() || out.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null; a raw `u32` read, no reference formed.
        let claimed = unsafe { std::ptr::addr_of!((*out).struct_size).read() };
        if claimed as usize != std::mem::size_of::<gf_frame>() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null; validity is the caller's contract.
        let handle = unsafe { &*c };
        let Some(guard) = handle.read() else {
            return gf_result::GF_AGAIN;
        };
        let Some(pf) = guard.acquire_frame() else {
            return gf_result::GF_AGAIN;
        };

        let mut planes = [gf_plane {
            fd: -1,
            offset: 0,
            stride: 0,
        }; 4];
        let n_planes = pf.planes.len().min(4);
        for (slot, p) in planes.iter_mut().zip(pf.planes.iter()).take(n_planes) {
            let (Ok(offset), Ok(stride)) = (u32::try_from(p.offset), u32::try_from(p.stride))
            else {
                // Does not fit the ABI; hand the buffer straight back.
                guard.release_frame(pf.frame_id);
                return gf_result::GF_ERR_GPU;
            };
            *slot = gf_plane {
                fd: pf.fd,
                offset,
                stride,
            };
        }

        let damage: Vec<gf_rect> = pf
            .damage
            .iter()
            .map(|r| gf_rect {
                x: r.x,
                y: r.y,
                w: r.w,
                h: r.h,
            })
            .collect();
        let n_damage = damage.len() as u32;

        // Stash the converted damage in the handle so its allocation
        // outlives this call, then take the pointer from the stashed copy
        // (not the pre-insert `Vec`) -- the pointer must point at whatever
        // memory actually survives past this function returning. Frame ids
        // are unique for the life of a session, so this never replaces (and
        // frees) the list of a frame the host still holds.
        let damage_ptr = {
            let mut store = handle.damage.lock().unwrap_or_else(|e| e.into_inner());
            store.insert(pf.frame_id, damage);
            let stored = store.get(&pf.frame_id).expect("just inserted");
            if stored.is_empty() {
                std::ptr::null()
            } else {
                stored.as_ptr()
            }
        };

        // SAFETY: `out` is non-null and points at a `gf_frame` of the size
        // the caller promised; written field by field because its memory
        // may be uninitialised.
        unsafe {
            std::ptr::addr_of_mut!((*out).frame_id).write(pf.frame_id);
            std::ptr::addr_of_mut!((*out).buffer_id).write(pf.buffer_id);
            std::ptr::addr_of_mut!((*out).handle_type).write(gf_handle_type::GF_HANDLE_DMABUF);
            std::ptr::addr_of_mut!((*out).width).write(pf.width);
            std::ptr::addr_of_mut!((*out).height).write(pf.height);
            std::ptr::addr_of_mut!((*out).drm_modifier).write(pf.modifier);
            std::ptr::addr_of_mut!((*out).n_planes).write(n_planes as u32);
            std::ptr::addr_of_mut!((*out).planes).write(planes);
            std::ptr::addr_of_mut!((*out).acquire_fence_fd).write(-1);
            std::ptr::addr_of_mut!((*out).n_damage).write(n_damage);
            std::ptr::addr_of_mut!((*out).damage).write(damage_ptr);
        }

        gf_result::GF_OK
    })
}

/// Return a frame to the library. Releasing a `frame_id` that was not
/// acquired, or was already released, does nothing.
///
/// After this call the frame's fds may be closed (immediately, if the
/// session was resized since it was acquired) and its `damage` pointer is
/// freed.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_release_frame(c: *const gf_client, frame_id: u32) {
    guard_void(move || {
        if c.is_null() {
            return;
        }
        // SAFETY: non-null; validity is the caller's contract.
        let handle = unsafe { &*c };
        // Only a frame this shim handed out is forwarded. An id that was
        // never acquired may still be sitting unacquired in the library's
        // slot, and releasing its buffer would free a dmabuf that
        // `gf_client_acquire_frame` can still hand out.
        let mut store = handle.damage.lock().unwrap_or_else(|e| e.into_inner());
        let Some(damage) = store.remove(&frame_id) else {
            return;
        };
        drop(store);
        match handle.read() {
            Some(guard) => guard.release_frame(frame_id),
            // connect/disconnect is running. Keep the frame acquirable-for-
            // release so the caller can retry; dropping it here would leak
            // the buffer for the rest of the session.
            None => {
                handle
                    .damage
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(frame_id, damage);
            }
        }
    });
}

/// Run `f` against the shared client, mapping "handle busy" and "not
/// connected" to `GF_ERR_STATE`.
fn with_connected_client(c: *const gf_client, f: impl FnOnce(&Client)) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null; validity is the caller's contract on the
        // calling `extern "C"` function.
        let handle = unsafe { &*c };
        match handle.read() {
            Some(g) if g.is_connected() => {
                f(&g);
                gf_result::GF_OK
            }
            _ => gf_result::GF_ERR_STATE,
        }
    })
}

/// Ask the server to switch to `width` x `height`. Fire and forget: the
/// outcome arrives as `GF_EVENT_RESIZED`. Call it on every window resize;
/// the server debounces. `GF_ERR_STATE` before connect or after disconnect.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_request_display_mode(
    c: *const gf_client,
    width: u16,
    height: u16,
) -> gf_result {
    with_connected_client(c, |cl| cl.request_display_mode(width, height))
}

/// Send a pointer motion. `GF_ERR_STATE` before connect or after disconnect.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_pointer_motion(
    c: *const gf_client,
    x: i16,
    y: i16,
) -> gf_result {
    with_connected_client(c, |cl| cl.push_pointer_motion(x, y))
}

/// Send a pointer button press or release. `GF_ERR_STATE` before connect
/// or after disconnect.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_pointer_button(
    c: *const gf_client,
    x: i16,
    y: i16,
    button: u8,
    down: bool,
) -> gf_result {
    with_connected_client(c, |cl| cl.push_pointer_button(x, y, button, down))
}

/// Send a wheel scroll. `GF_ERR_STATE` before connect or after disconnect.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_wheel(c: *const gf_client, dx: i16, dy: i16) -> gf_result {
    with_connected_client(c, |cl| cl.push_wheel(dx, dy))
}

/// Send a key press or release (X11 keysym). `GF_ERR_STATE` before connect
/// or after disconnect.
///
/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_key(
    c: *const gf_client,
    keysym: u32,
    down: bool,
) -> gf_result {
    with_connected_client(c, |cl| cl.push_key(keysym, down))
}

/// Library version. Any pointer may be null.
///
/// # Safety
/// `major`, `minor` and `patch` must each be null or a valid, writable
/// `u32` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_version(major: *mut u32, minor: *mut u32, patch: *mut u32) {
    guard_void(move || {
        let parts: Vec<u32> = env!("CARGO_PKG_VERSION")
            .split('.')
            .map(|p| p.parse().unwrap_or(0))
            .collect();
        let (maj, min, pat) = (
            parts.first().copied().unwrap_or(0),
            parts.get(1).copied().unwrap_or(0),
            parts.get(2).copied().unwrap_or(0),
        );
        if !major.is_null() {
            unsafe { *major = maj };
        }
        if !minor.is_null() {
            unsafe { *minor = min };
        }
        if !patch.is_null() {
            unsafe { *patch = pat };
        }
    });
}

/// A static, NUL-terminated name for a `gf_result` value, e.g. `"GF_OK"`.
/// Takes the raw integer so a value that is not a valid `gf_result` (from a
/// newer library, or corrupt) yields `"GF_ERR_UNKNOWN"` instead of
/// undefined behaviour. Never returns null; the string is never freed.
///
/// # Safety
/// Callable with any value; `unsafe` only for consistency with the rest of
/// the `extern "C"` surface.
#[no_mangle]
pub unsafe extern "C" fn gf_result_str(r: u32) -> *const libc::c_char {
    let s: &[u8] = match r {
        x if x == gf_result::GF_OK as u32 => b"GF_OK\0",
        x if x == gf_result::GF_AGAIN as u32 => b"GF_AGAIN\0",
        x if x == gf_result::GF_ERR_INVALID as u32 => b"GF_ERR_INVALID\0",
        x if x == gf_result::GF_ERR_STATE as u32 => b"GF_ERR_STATE\0",
        x if x == gf_result::GF_ERR_IO as u32 => b"GF_ERR_IO\0",
        x if x == gf_result::GF_ERR_GPU as u32 => b"GF_ERR_GPU\0",
        _ => b"GF_ERR_UNKNOWN\0",
    };
    s.as_ptr().cast()
}

/// # Safety
/// `p` must be null or point to a valid, NUL-terminated C string.
unsafe fn cstr_to_string(p: *const libc::c_char) -> Option<String> {
    if p.is_null() {
        return None;
    }
    // SAFETY: non-null per the check above; validity is the caller's
    // contract, documented on every function that calls this helper.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message_of(msg: &str) -> [u8; 256] {
        let mut dst = [0x55 as libc::c_char; 256];
        unsafe { fill_message(&mut dst, msg) };
        dst.map(|b| b.to_ne_bytes()[0])
    }

    #[test]
    fn control_bytes_are_neutralised() {
        let m = message_of("a\x1b[31mred\x07\nb\x7f");
        let end = m.iter().position(|&b| b == 0).unwrap();
        let s = std::str::from_utf8(&m[..end]).unwrap();
        assert_eq!(s, "a?[31mred??b?");
        assert!(m[end..].iter().all(|&b| b == 0), "tail not zeroed");
    }

    #[test]
    fn truncation_never_splits_a_character() {
        // 254 ASCII bytes then a 3-byte char: only the ASCII fits.
        let msg = format!("{}\u{20ac}", "x".repeat(254));
        let m = message_of(&msg);
        let end = m.iter().position(|&b| b == 0).unwrap();
        assert_eq!(end, 254);
        std::str::from_utf8(&m[..end]).expect("valid utf8");
        // And a long message stops at 255 bytes + NUL.
        let m = message_of(&"y".repeat(1000));
        assert_eq!(m.iter().position(|&b| b == 0), Some(255));
    }

    #[test]
    fn error_mapping_table() {
        use ghostframe_client_native::ClientError as E;
        assert_eq!(
            map_client_error(&E::Connect("x".into())),
            gf_result::GF_ERR_STATE
        );
        assert_eq!(
            map_client_error(&E::Bootstrap("x".into())),
            gf_result::GF_ERR_IO
        );
        assert_eq!(
            map_client_error(&E::Io(std::io::Error::other("x"))),
            gf_result::GF_ERR_IO
        );
    }
}
