//! C ABI shim over `ghostframe-client-native`.
//!
//! This crate holds NO logic of its own: every `gf_*` function validates
//! its arguments, converts to/from the Rust types in [`ghostframe_client_native`],
//! and returns. The one exception is the small amount of bookkeeping
//! needed to keep a `gf_frame`'s `damage` pointer valid between
//! `gf_client_acquire_frame` and `gf_client_release_frame` -- see
//! [`gf_client`].
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
//! ## `struct_size` is the forward-compatibility mechanism
//!
//! [`gf_client_config`], [`gf_event`] and [`gf_frame`] all start with a
//! `struct_size` field the caller must set to `sizeof(...)` of the type it
//! was built against. A future ABI revision can append fields to any of
//! these without breaking a caller linked against an older header: this
//! library only reads a struct as far as `struct_size` promises is there,
//! and rejects a value that doesn't match a size it understands rather
//! than guessing.

#![allow(non_camel_case_types)] // `gf_client` follows C naming, not Rust's.

pub mod types;

pub use types::*;

use ghostframe_client_native::{Client, ClientError, ClientEvent, Config};
use std::collections::HashMap;
use std::ffi::CStr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

/// The opaque handle behind every `gf_client *`.
///
/// Wraps the real [`Client`] plus the one piece of state this shim layer
/// owns itself: a per-`frame_id` stash of the converted [`gf_rect`] damage
/// list, so [`gf_frame::damage`] has somewhere stable to point until
/// [`gf_client_release_frame`] frees it. `ghostframe_client_native::Client::acquire_frame`
/// hands back an owned `Vec` that would otherwise be dropped the instant
/// this call returns.
pub struct gf_client {
    inner: Client,
    damage: Mutex<HashMap<u32, Vec<gf_rect>>>,
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

/// Copy `msg`, truncated to 255 bytes + NUL, into a fixed `gf_event`/error
/// message buffer.
fn fill_message(dst: &mut [libc::c_char; 256], msg: &str) {
    let bytes = msg.as_bytes();
    let n = bytes.len().min(255);
    // SAFETY: `libc::c_char` and `u8` are both one-byte integer types on
    // every platform this library targets; the pointer cast below just
    // reinterprets the destination's signedness.
    let dst_u8: &mut [u8; 256] =
        unsafe { &mut *(dst as *mut [libc::c_char; 256] as *mut [u8; 256]) };
    dst_u8[..n].copy_from_slice(&bytes[..n]);
    dst_u8[n] = 0;
    // Zero any leftover tail from a previous, longer message.
    for b in &mut dst_u8[n + 1..] {
        *b = 0;
    }
}

/// The byte offset of [`gf_client_config::max_decode_width`] -- i.e. the
/// size of `gf_client_config` as it existed before that field (and
/// `max_decode_height`) were appended. A `struct_size` at least this large
/// covers every field this function unconditionally reads; a `struct_size`
/// at least `size_of::<gf_client_config>()` additionally covers the two
/// pre-warm fields. See [`gf_client_create`]'s doc.
const GF_CLIENT_CONFIG_SIZE_BEFORE_PREWARM: usize =
    std::mem::offset_of!(gf_client_config, max_decode_width);

/// # Safety
/// `cfg` must be null, or point to at least `cfg.struct_size` readable
/// bytes laid out as a prefix of `gf_client_config` -- i.e. a struct built
/// against this exact header, or an older header that had fewer trailing
/// fields (see [`gf_client_config`]'s doc on `struct_size`).
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
        // SAFETY: non-null per the check above; validity of the pointee is
        // the caller's contract (documented on this function). Reading
        // `struct_size` through `&*cfg` before checking it -- rather than
        // an offset-only raw read -- matches this function's long-standing
        // pattern and is sound for the same reason the rest of this
        // function's field reads are: every real C ABI lays out
        // `gf_client_config`'s fields at a fixed prefix that does not move
        // as later fields are appended, so a shorter caller allocation
        // (an older header) still has real bytes at every offset this
        // function reads up to its own `struct_size` -- checked field by
        // field below before each read that a later addition could miss.
        let cfg = unsafe { &*cfg };
        let struct_size = cfg.struct_size as usize;
        if struct_size < GF_CLIENT_CONFIG_SIZE_BEFORE_PREWARM {
            return gf_result::GF_ERR_INVALID;
        }

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

        let preferred_modifiers = if cfg.n_preferred_modifiers == 0 {
            Vec::new()
        } else if cfg.preferred_modifiers.is_null() {
            return gf_result::GF_ERR_INVALID;
        } else {
            // SAFETY: non-null per the check above; the caller's contract
            // is that `preferred_modifiers` points to at least
            // `n_preferred_modifiers` valid, readable `u64`s.
            unsafe {
                std::slice::from_raw_parts(
                    cfg.preferred_modifiers,
                    cfg.n_preferred_modifiers as usize,
                )
            }
            .to_vec()
        };

        // Read only when `struct_size` actually covers these two fields --
        // a caller built against a header from before they existed reports
        // a smaller `struct_size` here (checked above to be at least
        // `GF_CLIENT_CONFIG_SIZE_BEFORE_PREWARM`), and gets `0`/`0`
        // ("unknown, do not pre-warm") rather than whatever uninitialised
        // or unallocated bytes happen to sit past their own struct.
        let (max_decode_width, max_decode_height) =
            if struct_size >= std::mem::size_of::<gf_client_config>() {
                (cfg.max_decode_width, cfg.max_decode_height)
            } else {
                (0, 0)
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
            max_decode_width,
            max_decode_height,
        };

        let client = match Client::new(config) {
            Ok(c) => c,
            Err(_) => return gf_result::GF_ERR_IO,
        };

        let handle = Box::new(gf_client {
            inner: client,
            damage: Mutex::new(HashMap::new()),
        });
        // SAFETY: `out` is non-null per the check above.
        unsafe {
            *out = Box::into_raw(handle);
        }
        gf_result::GF_OK
    })
}

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
        // handle not yet destroyed -- reclaiming it here is exactly what
        // "destroy" means; `Client`'s `Drop` tears down its threads.
        drop(unsafe { Box::from_raw(c) });
    });
}

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
        unsafe { &*c }.inner.event_fd()
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
        unsafe { &*c }.inner.supports_h264()
    }))
    .unwrap_or(false)
}

/// Never blocks. Returns `GF_AGAIN` when the queue is empty.
///
/// # Safety
/// `c` and `out` must each be null or point to valid memory as documented
/// on their types.
#[no_mangle]
pub unsafe extern "C" fn gf_client_next_event(c: *mut gf_client, out: *mut gf_event) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() || out.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        // SAFETY: non-null per the checks above.
        let out_ref = unsafe { &mut *out };
        if out_ref.struct_size as usize != std::mem::size_of::<gf_event>() {
            return gf_result::GF_ERR_INVALID;
        }
        let handle = unsafe { &*c };
        let Some(ev) = handle.inner.next_event() else {
            return gf_result::GF_AGAIN;
        };

        // Clear every field before filling in the ones this event kind
        // actually uses, so a caller can't read stale data from a
        // previous `gf_event` reuse.
        out_ref.width = 0;
        out_ref.height = 0;
        out_ref.frame_id = 0;
        fill_message(&mut out_ref.message, "");

        match ev {
            ClientEvent::Connected => {
                out_ref.kind = gf_event_type::GF_EVENT_CONNECTED;
            }
            ClientEvent::Disconnected { reason } => {
                out_ref.kind = gf_event_type::GF_EVENT_DISCONNECTED;
                fill_message(&mut out_ref.message, &reason);
            }
            ClientEvent::Resized { width, height } => {
                out_ref.kind = gf_event_type::GF_EVENT_RESIZED;
                out_ref.width = width;
                out_ref.height = height;
            }
            ClientEvent::FrameReady { frame_id } => {
                out_ref.kind = gf_event_type::GF_EVENT_FRAME_READY;
                out_ref.frame_id = frame_id;
            }
            ClientEvent::Error { message } => {
                out_ref.kind = gf_event_type::GF_EVENT_ERROR;
                fill_message(&mut out_ref.message, &message);
            }
        }
        gf_result::GF_OK
    })
}

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
        // SAFETY: non-null per the check above.
        let handle = unsafe { &mut *c };
        match handle.inner.connect(&host, port) {
            Ok(()) => gf_result::GF_OK,
            Err(e) => map_client_error(&e),
        }
    })
}

/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_disconnect(c: *mut gf_client) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        let handle = unsafe { &mut *c };
        match handle.inner.disconnect() {
            Ok(()) => gf_result::GF_OK,
            Err(e) => map_client_error(&e),
        }
    })
}

/// Never blocks. Returns `GF_AGAIN` when no frame is ready.
///
/// # Safety
/// `c` and `out` must each be null or point to valid memory as documented
/// on their types.
#[no_mangle]
pub unsafe extern "C" fn gf_client_acquire_frame(
    c: *mut gf_client,
    out: *mut gf_frame,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() || out.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        let out_ref = unsafe { &mut *out };
        if out_ref.struct_size as usize != std::mem::size_of::<gf_frame>() {
            return gf_result::GF_ERR_INVALID;
        }
        let handle = unsafe { &mut *c };
        let Some(pf) = handle.inner.acquire_frame() else {
            return gf_result::GF_AGAIN;
        };

        let mut planes = [gf_plane {
            fd: -1,
            offset: 0,
            stride: 0,
        }; 4];
        let n_planes = pf.planes.len().min(4);
        for (slot, p) in planes.iter_mut().zip(pf.planes.iter()).take(n_planes) {
            *slot = gf_plane {
                fd: pf.fd,
                offset: p.offset as u32,
                stride: p.stride as u32,
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
        // memory actually survives past this function returning.
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

        out_ref.struct_size = std::mem::size_of::<gf_frame>() as u32;
        out_ref.frame_id = pf.frame_id;
        out_ref.buffer_id = pf.buffer_id;
        out_ref.handle_type = gf_handle_type::GF_HANDLE_DMABUF;
        out_ref.width = pf.width;
        out_ref.height = pf.height;
        out_ref.drm_modifier = pf.modifier;
        out_ref.n_planes = n_planes as u32;
        out_ref.planes = planes;
        out_ref.acquire_fence_fd = -1;
        out_ref.n_damage = n_damage;
        out_ref.damage = damage_ptr;

        gf_result::GF_OK
    })
}

/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_release_frame(c: *mut gf_client, frame_id: u32) {
    guard_void(move || {
        if c.is_null() {
            return;
        }
        let handle = unsafe { &mut *c };
        handle
            .damage
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&frame_id);
        handle.inner.release_frame(frame_id);
    });
}

/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_pointer_motion(
    c: *mut gf_client,
    x: i16,
    y: i16,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        unsafe { &mut *c }.inner.push_pointer_motion(x, y);
        gf_result::GF_OK
    })
}

/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_pointer_button(
    c: *mut gf_client,
    x: i16,
    y: i16,
    button: u8,
    down: bool,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        unsafe { &mut *c }
            .inner
            .push_pointer_button(x, y, button, down);
        gf_result::GF_OK
    })
}

/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_wheel(c: *mut gf_client, dx: i16, dy: i16) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        unsafe { &mut *c }.inner.push_wheel(dx, dy);
        gf_result::GF_OK
    })
}

/// # Safety
/// `c` must be null or a valid, live `gf_client` pointer.
#[no_mangle]
pub unsafe extern "C" fn gf_client_push_key(
    c: *mut gf_client,
    keysym: u32,
    down: bool,
) -> gf_result {
    guard(gf_result::GF_ERR_INVALID, move || {
        if c.is_null() {
            return gf_result::GF_ERR_INVALID;
        }
        unsafe { &mut *c }.inner.push_key(keysym, down);
        gf_result::GF_OK
    })
}

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

/// # Safety
/// Callable with any `gf_result` value; marked `unsafe` for consistency
/// with the rest of this crate's `extern "C"` surface, not because this
/// particular function dereferences anything unchecked.
#[no_mangle]
pub unsafe extern "C" fn gf_result_str(r: gf_result) -> *const libc::c_char {
    let s: &[u8] = match r {
        gf_result::GF_OK => b"GF_OK\0",
        gf_result::GF_AGAIN => b"GF_AGAIN\0",
        gf_result::GF_ERR_INVALID => b"GF_ERR_INVALID\0",
        gf_result::GF_ERR_STATE => b"GF_ERR_STATE\0",
        gf_result::GF_ERR_IO => b"GF_ERR_IO\0",
        gf_result::GF_ERR_GPU => b"GF_ERR_GPU\0",
    };
    s.as_ptr() as *const libc::c_char
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
