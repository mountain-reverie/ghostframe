//! `#[repr(C)]` types making up the ABI surface. Every type here is exactly
//! what cbindgen turns into `include/ghostframe_client.h`; see that
//! generated header for the authoritative C-side shape (field order and
//! padding are whatever cbindgen chose, not necessarily this file's
//! declaration order after alignment).
//!
//! Names follow C convention (`gf_result`, `GF_OK`, ...), not Rust's --
//! that's the point of this module, so `non_camel_case_types` is silenced
//! wholesale rather than per item.
#![allow(non_camel_case_types)]

/// Result code returned by every fallible `gf_*` call.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum gf_result {
    GF_OK = 0,
    /// Nothing is available right now (no queued event, no ready frame).
    /// Not an error -- callers poll again later.
    GF_AGAIN = 1,
    /// A null pointer, a wrong `struct_size`, or another malformed
    /// argument. Also returned when a call caught an internal panic (see
    /// the module doc on `lib.rs` for why no panic may cross the FFI
    /// boundary).
    GF_ERR_INVALID = 2,
    /// The call is not valid for the client's current state (e.g.
    /// `connect` called twice).
    GF_ERR_STATE = 3,
    GF_ERR_IO = 4,
    GF_ERR_GPU = 5,
}

/// How to interpret [`gf_frame::planes`].
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum gf_handle_type {
    /// Linux dmabuf: `gf_plane::fd` is a PRIME fd. The only type M1
    /// produces.
    GF_HANDLE_DMABUF = 0,
    GF_HANDLE_WIN32_NT = 1,
    GF_HANDLE_IOSURFACE = 2,
    GF_HANDLE_CPU = 3,
}

/// A damaged rectangle, in pixel coordinates.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct gf_rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// One plane of a [`gf_frame`]'s image data.
///
/// ## Ownership
///
/// `fd` is owned by the library and remains valid until the `gf_client` is
/// destroyed. The caller must NOT close it -- `dup(2)` it first if a
/// longer-lived handle is needed than the frame it arrived on.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct gf_plane {
    pub fd: i32,
    pub offset: u32,
    pub stride: u32,
}

/// One decoded frame, ready for the host to import and present.
///
/// ## Ownership
///
/// - `planes[i].fd`: owned by the library, valid until the `gf_client` is
///   destroyed. Do not close it; `dup(2)` for a longer life.
/// - `acquire_fence_fd`: owned by the CALLER, who must close it. Always
///   `-1` in M1 (the frame is already signalled by the time
///   `gf_client_acquire_frame` returns it -- there is no fence to wait on
///   yet).
/// - `damage`: points into library memory. Valid only until the matching
///   `gf_client_release_frame` call; do not retain the pointer past it.
#[repr(C)]
#[derive(Debug)]
pub struct gf_frame {
    pub struct_size: u32,
    pub frame_id: u32,
    pub buffer_id: u32,
    pub handle_type: gf_handle_type,
    pub width: u32,
    pub height: u32,
    pub drm_modifier: u64,
    pub n_planes: u32,
    pub planes: [gf_plane; 4],
    pub acquire_fence_fd: i32,
    pub n_damage: u32,
    pub damage: *const gf_rect,
}

/// Configuration for [`crate::gf_client_create`].
///
/// `struct_size` MUST be set to `sizeof(gf_client_config)` by the caller;
/// it is the forward-compatibility mechanism that lets a future ABI
/// revision add fields without breaking a caller built against an older
/// header. `struct_size` smaller than this library understands the fields
/// up to (see [`Self::max_decode_width`]'s doc) is rejected with
/// `GF_ERR_INVALID`; a `struct_size` that covers the original fields but
/// not a later addition is accepted, with the addition's fields read as
/// whatever default that field's own doc names (never misread from
/// unallocated memory).
#[repr(C)]
#[derive(Debug)]
pub struct gf_client_config {
    pub struct_size: u32,
    pub hostname: *const libc::c_char,
    pub state_dir: *const libc::c_char,
    pub authkey: *const libc::c_char,
    /// A *request*, not an assertion: whether the host wants H.264 decode if
    /// this machine can do it. `gf_client_supports_h264` returns the answer
    /// -- a host that asks for H.264 on a machine without VA-API gets
    /// `false` there and a working session on the tile codecs, not a
    /// failure.
    pub supports_h264: bool,
    pub indices_raw: bool,
    /// `0` means "use the library's default" (currently 3).
    pub n_export_buffers: u32,
    pub n_preferred_modifiers: u32,
    /// Array of `n_preferred_modifiers` `u64`s, most-preferred first. May
    /// be null when `n_preferred_modifiers == 0`.
    pub preferred_modifiers: *const u64,
    /// This client's own maximum display resolution (its screen, or
    /// eventually its negotiated virtual EDID) -- NOT the session's current
    /// frame size, which arrives separately over the wire. Appended after
    /// `preferred_modifiers`, so a caller built against a header from
    /// before this field existed reports a smaller `struct_size` and gets
    /// `0` read here rather than a rejected config -- see this struct's own
    /// doc. `0` in either `max_decode_width` or `max_decode_height` means
    /// "unknown, do not pre-warm", identical to what an old caller gets by
    /// construction: the H.264 decoder opens lazily on the first H.264
    /// frame instead of being pre-warmed in `gf_client_create`. See
    /// `ghostframe_client_native::Config::max_decode_width`.
    pub max_decode_width: u32,
    pub max_decode_height: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum gf_event_type {
    GF_EVENT_CONNECTED = 0,
    GF_EVENT_DISCONNECTED = 1,
    GF_EVENT_RESIZED = 2,
    GF_EVENT_FRAME_READY = 3,
    GF_EVENT_ERROR = 4,
}

/// One event popped by [`crate::gf_client_next_event`].
///
/// `struct_size` MUST be set to `sizeof(gf_event)` by the caller before
/// the call -- see [`gf_client_config`]'s doc for why.
#[repr(C)]
#[derive(Debug)]
pub struct gf_event {
    pub struct_size: u32,
    pub kind: gf_event_type,
    /// Valid for `GF_EVENT_RESIZED`.
    pub width: u32,
    /// Valid for `GF_EVENT_RESIZED`.
    pub height: u32,
    /// Valid for `GF_EVENT_FRAME_READY`.
    pub frame_id: u32,
    /// Valid for `GF_EVENT_DISCONNECTED` and `GF_EVENT_ERROR`. NUL-terminated,
    /// truncated to 255 bytes + NUL if the underlying message is longer.
    /// A fixed inline buffer rather than an owned pointer deliberately: a
    /// `const char *` here would need its own ownership rule and free
    /// function for what is only ever a short diagnostic string.
    pub message: [libc::c_char; 256],
}
