//! The X11 [`Backend`]: x11rb (libxcb-backed) + DRI3 + Present.
//!
//! A handful of decisions here are load-bearing enough to spell out once,
//! rather than leave implicit in the code below:
//!
//! - **`XCBConnection`, not `RustConnection`.** `ghostframe-xdaemon` (the
//!   server side) uses x11rb's pure-Rust `RustConnection`, which has no
//!   `libxcb` dependency. This backend cannot: keycode -> keysym
//!   translation goes through `xkbcommon-x11`
//!   (`xkb_x11_keymap_new_from_device` and friends), and that C API needs
//!   a real `xcb_connection_t*`. Only x11rb's `XCBConnection` (feature
//!   `allow-unsafe-code`, linked against `libxcb`) can hand one over via
//!   `as-raw-xcb-connection`. This is the same choice other Rust X11
//!   clients that need XKB make (e.g. winit's x11 backend) -- it is not
//!   specific to this crate's needs, but it is a real, new runtime
//!   dependency on `libxcb.so` that `ghostframe-xdaemon` does not have.
//!
//! - **Fullscreen before the first map.** [`X11Backend::open`] sets
//!   `_NET_WM_STATE` to `_NET_WM_STATE_FULLSCREEN` via `change_property`
//!   on the window before `map_window`, so a WM that honours initial
//!   properties sizes the window correctly from the start rather than
//!   mapping windowed and immediately resizing. Not every WM honours
//!   this; if a post-map `get_property` roundtrip shows the state didn't
//!   take, `open` falls back to the ICCCM/EWMH client-message route (a
//!   `_NET_WM_STATE` `ClientMessage` to the root window).
//!
//! - **We clear the surround ourselves.** Unlike Wayland, where
//!   xdg-shell requires the compositor to letterbox a fullscreen surface
//!   smaller than the output, X11 gives no such guarantee: `present`
//!   only paints the centred rectangle the image occupies, and whatever
//!   was on screen before (WM/root background, a previous larger image)
//!   would otherwise show through the border. [`X11Backend::clear_surround`]
//!   fills the four border bands with `PolyFillRectangle`, called only
//!   when the placement actually changes (a resize, or the first frame)
//!   -- not per frame, which at 1080p would be a real cost and would
//!   undo the point of damage tracking. [`X11Backend`] also clears the
//!   *entire* window on every `ConfigureNotify`, since a resize can
//!   change the image's position by more than the border bands track
//!   (the old content is at stale coordinates until the next `present`).
//!
//! - **One `Pixmap` per `buffer_id`, cached.** Mirrors
//!   `wayland.rs`'s `AppData::buffer_for`: the GPU export ring recycles a
//!   small fixed set of dmabufs, so [`X11Backend::pixmap_for`] only calls
//!   `dri3_pixmap_from_buffers` when a `buffer_id` is new or its
//!   dimensions/modifier changed (e.g. across a resize). The imported
//!   `Pixmap` is presented again on every subsequent frame that reuses
//!   the same `buffer_id`, exactly like re-attaching the same
//!   `wl_buffer`: it wraps the same underlying GEM object, so it always
//!   shows whatever the exporter most recently wrote into it.
//!
//! - **The dmabuf fd is `dup`'d before import.** `frame.fd` is owned by
//!   the library's export ring for as long as `buffer_id` stays live (see
//!   `PublishedFrame::fd`'s doc: "do not close it"). `dri3_pixmap_from_buffers`
//!   hands the fd to the X server, which takes ownership of *its* copy;
//!   x11rb's `RawFdContainer` (an `OwnedFd` on Unix) closes whatever we
//!   give it once the request is sent. Handing over `frame.fd` directly
//!   would close the exporter's fd out from under it -- the session would
//!   then die after exactly one frame per `buffer_id`. [`dup_fd`] gives
//!   the request its own fd to own and close.
//!
//! - **A likely colour-channel caveat, left unresolved.** DRI3's
//!   `PixmapFromBuffers` has no explicit pixel-format field; the pixel
//!   layout is inferred entirely from `depth`/`bpp` using the same fixed
//!   table Mesa's DRI3 loader uses (depth 24/bpp 32 -> `XRGB8888`, depth
//!   32/bpp 32 -> `ARGB8888` -- blue at the lowest memory address in
//!   both). `ghostframe-client-gpu`'s export path always produces
//!   `DRM_FORMAT_ABGR8888` (red at the lowest address -- see
//!   `wayland.rs`'s `DRM_FORMAT_ABGR8888` doc, which Wayland is told
//!   about explicitly via the dmabuf protocol's fourcc field). DRI3 has
//!   no equivalent field to tell the server "this is actually ABGR". The
//!   result is very likely a systematic red/blue channel swap on real
//!   hardware -- worth confirming at the same time this backend gets its
//!   first manual X11 test, since nothing in this build (no display, no
//!   GPU) can catch it. Fixing it, if confirmed, belongs in
//!   `ghostframe-client-gpu`'s export path or a per-backend format
//!   choice, not here.

use std::collections::HashMap;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use x11rb::atom_manager;
use x11rb::connection::Connection as X11Connection;
use x11rb::protocol::dri3::ConnectionExt as Dri3ConnectionExt;
use x11rb::protocol::present::ConnectionExt as PresentConnectionExt;
use x11rb::protocol::xproto::{self, ConnectionExt as XprotoConnectionExt};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::xcb_ffi::XCBConnection;

use xkbcommon::xkb;

use ghostframe_client_native::PublishedFrame;

use crate::geometry::Placement;
use crate::window::{Backend, WindowError, WindowEvent};

atom_manager! {
    pub Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        WM_CHANGE_STATE,
        _NET_WM_STATE,
        _NET_WM_STATE_FULLSCREEN,
    }
}

/// The ICCCM `WM_STATE` "iconic" value (`IconicState`). Not exposed as a
/// constant by x11rb (its `properties` module only surfaces the enum used
/// for reading `WM_HINTS`, not the raw wire value this client message
/// needs), so it's spelled out here with its source.
const ICCCM_ICONIC_STATE: u32 = 3;

/// EWMH `_NET_WM_STATE` client message `source_indication`/`action` values
/// (`_NET_WM_STATE_ADD` = 1). Also not an x11rb constant.
const NET_WM_STATE_ADD: u32 = 1;

/// A key-press classification: an ordinary button click, or a scroll
/// notch. X11 reports wheel motion as button numbers 4-7 rather than a
/// distinct event type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ButtonKind {
    /// An X11-numbered button (1 = left, 2 = middle, 3 = right, and
    /// anything else the server hands us -- forwarded unchanged, unlike
    /// the Wayland backend's evdev remapping, since X11's numbering
    /// already matches the wire).
    Click(u8),
    /// One wheel notch. Magnitude is fixed at 1 (matching the "one tick"
    /// convention the wire already uses -- see
    /// `ghostframe-web-client/src/input/wire.ts`'s `wheel` handler, which
    /// also normalises to +-1 regardless of the browser's reported
    /// `deltaY`/`deltaX` magnitude); X11's button click carries no
    /// magnitude of its own to preserve.
    Scroll { dx: i16, dy: i16 },
}

/// Classify a `ButtonPress`/`ButtonRelease` `detail` field.
///
/// Buttons 4/5 are the vertical wheel (4 = up, 5 = down); 6/7 are the
/// horizontal wheel (6 = left, 7 = right) -- the standard X.Org/XFree86
/// numbering. Silently forwarding button 4 as an ordinary click is the
/// kind of bug that looks like "scrolling pastes things": see this
/// module's test `wheel_buttons_are_not_forwarded_as_clicks`.
fn classify_button(detail: u8) -> ButtonKind {
    match detail {
        4 => ButtonKind::Scroll { dx: 0, dy: -1 },
        5 => ButtonKind::Scroll { dx: 0, dy: 1 },
        6 => ButtonKind::Scroll { dx: -1, dy: 0 },
        7 => ButtonKind::Scroll { dx: 1, dy: 0 },
        other => ButtonKind::Click(other),
    }
}

/// The four border bands `placement` leaves uncovered inside a
/// `placement.out_w` x `placement.out_h` window -- top, bottom, left,
/// right, in that order, omitting any band with zero area (an image
/// pinned to an edge, or one that exactly fills an axis). Pure so it can
/// be tested without a connection: see this module's
/// `surround_rects` tests.
fn surround_rects(p: &Placement) -> Vec<xproto::Rectangle> {
    let out_w = p.out_w as i32;
    let out_h = p.out_h as i32;
    let img_right = p.origin_x.saturating_add(p.image_w as i32).min(out_w);
    let img_bottom = p.origin_y.saturating_add(p.image_h as i32).min(out_h);
    let origin_x = p.origin_x.clamp(0, out_w);
    let origin_y = p.origin_y.clamp(0, out_h);

    let mut rects = Vec::with_capacity(4);
    if origin_y > 0 {
        rects.push(rect(0, 0, out_w, origin_y));
    }
    if img_bottom < out_h {
        rects.push(rect(0, img_bottom, out_w, out_h - img_bottom));
    }
    if origin_x > 0 {
        rects.push(rect(0, origin_y, origin_x, img_bottom - origin_y));
    }
    if img_right < out_w {
        rects.push(rect(
            img_right,
            origin_y,
            out_w - img_right,
            img_bottom - origin_y,
        ));
    }
    rects.retain(|r| r.width > 0 && r.height > 0);
    rects
}

/// Build an `xproto::Rectangle` from `i32` pixel coordinates, clamping to
/// what the wire type (`i16`/`u16`) can hold. X11 window coordinates are
/// `i16` at the protocol level anyway, so this is a defensive floor/ceiling
/// rather than something expected to bite in practice.
fn rect(x: i32, y: i32, w: i32, h: i32) -> xproto::Rectangle {
    xproto::Rectangle {
        x: x.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        y: y.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        width: w.clamp(0, u16::MAX as i32) as u16,
        height: h.clamp(0, u16::MAX as i32) as u16,
    }
}

/// `dup(2)` `fd` into a fresh, owned fd. Required before handing a
/// dmabuf fd to `dri3_pixmap_from_buffers`: see this module's doc on fd
/// ownership.
fn dup_fd(fd: RawFd) -> Result<OwnedFd, WindowError> {
    // SAFETY: `dup` is called on `fd`, which the caller guarantees is a
    // valid, open descriptor for the duration of this call. `dup` either
    // returns a new descriptor this function now owns exclusively, or -1
    // on error; nothing here retains the return value on the error path.
    let duped = unsafe { libc::dup(fd) };
    if duped < 0 {
        return Err(WindowError::Io(std::io::Error::last_os_error()));
    }
    // SAFETY: `duped` was just returned by `dup` above: it is open, valid,
    // and not owned by anything else yet.
    Ok(unsafe { OwnedFd::from_raw_fd(duped) })
}

/// One cached `Pixmap` import for a ring `buffer_id`, plus enough of the
/// `PublishedFrame` it was built from to tell whether a later frame with
/// the same `buffer_id` still matches it (a resize reuses buffer ids with
/// new dimensions).
struct CachedPixmap {
    pixmap: xproto::Pixmap,
    width: u32,
    height: u32,
    modifier: u64,
}

/// Build the XKB state for `conn`'s core keyboard device via
/// `xkbcommon-x11`. `Context` and `Keymap` are dropped at the end of this
/// function -- `State` takes its own internal ref to the keymap (which in
/// turn holds the context alive), so nothing here needs to outlive it.
fn init_xkb_state(conn: &XCBConnection) -> Result<xkb::State, WindowError> {
    let mut major_out = 0u16;
    let mut minor_out = 0u16;
    let mut base_event = 0u8;
    let mut base_error = 0u8;
    let ok = xkb::x11::setup_xkb_extension(
        conn,
        xkb::x11::MIN_MAJOR_XKB_VERSION,
        xkb::x11::MIN_MINOR_XKB_VERSION,
        xkb::x11::SetupXkbExtensionFlags::NoFlags,
        &mut major_out,
        &mut minor_out,
        &mut base_event,
        &mut base_error,
    );
    if !ok {
        return Err(WindowError::X11(
            "the X server does not support the XKB extension (xkb_x11_setup_xkb_extension failed)"
                .to_string(),
        ));
    }

    let device_id = xkb::x11::get_core_keyboard_device_id(conn);
    if device_id < 0 {
        return Err(WindowError::X11(
            "no core keyboard device (xkb_x11_get_core_keyboard_device_id < 0)".to_string(),
        ));
    }

    let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap =
        xkb::x11::keymap_new_from_device(&context, conn, device_id, xkb::KEYMAP_COMPILE_NO_FLAGS);
    Ok(xkb::x11::state_new_from_device(&keymap, conn, device_id))
}

/// The X11 [`Backend`]: x11rb + DRI3 + Present. See the module doc for the
/// load-bearing decisions.
pub struct X11Backend {
    conn: XCBConnection,
    window: xproto::Window,
    root: xproto::Window,
    atoms: Atoms,
    gc: xproto::Gcontext,
    depth: u8,
    width: u32,
    height: u32,
    pixmaps: HashMap<u32, CachedPixmap>,
    /// The last `Placement` the surround was cleared for; `None` before
    /// the first `present`. Re-cleared only when this changes -- see the
    /// module doc.
    last_placement: Option<Placement>,
    next_serial: u32,
    xkb_state: xkb::State,
    events: Vec<WindowEvent>,
}

impl X11Backend {
    pub(super) fn open(title: &str) -> Result<Self, WindowError> {
        let (conn, screen_num) = XCBConnection::connect(None)
            .map_err(|e| WindowError::X11(format!("connecting to the X server: {e}")))?;

        // DRI3 1.2 is required for `PixmapFromBuffers` (the
        // modifier-aware, multi-plane import this backend relies on);
        // query both extensions up front so a server lacking either fails
        // here, with a clear message, rather than on the first `present`.
        conn.dri3_query_version(1, 2)
            .map_err(|e| WindowError::X11(format!("DRI3 QueryVersion: {e}")))?
            .reply()
            .map_err(|e| WindowError::X11(format!("DRI3 extension unavailable: {e}")))?;
        conn.present_query_version(1, 0)
            .map_err(|e| WindowError::X11(format!("Present QueryVersion: {e}")))?
            .reply()
            .map_err(|e| WindowError::X11(format!("Present extension unavailable: {e}")))?;

        let atoms = Atoms::new(&conn)
            .map_err(|e| WindowError::X11(format!("interning atoms: {e}")))?
            .reply()
            .map_err(|e| WindowError::X11(format!("interning atoms: {e}")))?;

        let screen = conn.setup().roots[screen_num].clone();
        let root = screen.root;
        let depth = screen.root_depth;
        let visual = screen.root_visual;

        let window = conn
            .generate_id()
            .map_err(|e| WindowError::X11(format!("generate_id (window): {e}")))?;
        let gc = conn
            .generate_id()
            .map_err(|e| WindowError::X11(format!("generate_id (gc): {e}")))?;

        // Sized to the screen as a starting guess; the WM's first
        // `ConfigureNotify` (awaited below) supplies the real fullscreen
        // size, same as the Wayland backend waits for its first
        // `configure`.
        let init_width = screen.width_in_pixels;
        let init_height = screen.height_in_pixels;

        let win_aux = xproto::CreateWindowAux::new()
            .event_mask(
                xproto::EventMask::KEY_PRESS
                    | xproto::EventMask::KEY_RELEASE
                    | xproto::EventMask::BUTTON_PRESS
                    | xproto::EventMask::BUTTON_RELEASE
                    | xproto::EventMask::POINTER_MOTION
                    | xproto::EventMask::STRUCTURE_NOTIFY,
            )
            .background_pixel(screen.black_pixel);

        conn.create_window(
            depth,
            window,
            root,
            0,
            0,
            init_width,
            init_height,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            visual,
            &win_aux,
        )
        .map_err(|e| WindowError::X11(format!("create_window: {e}")))?;

        conn.create_gc(
            gc,
            window,
            &xproto::CreateGCAux::new().foreground(screen.black_pixel),
        )
        .map_err(|e| WindowError::X11(format!("create_gc: {e}")))?;

        conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_NAME,
            xproto::AtomEnum::STRING,
            title.as_bytes(),
        )
        .map_err(|e| WindowError::X11(format!("WM_NAME: {e}")))?;
        conn.change_property8(
            xproto::PropMode::REPLACE,
            window,
            xproto::AtomEnum::WM_CLASS,
            xproto::AtomEnum::STRING,
            b"ghostframe\0ghostframe\0",
        )
        .map_err(|e| WindowError::X11(format!("WM_CLASS: {e}")))?;
        conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms.WM_PROTOCOLS,
            xproto::AtomEnum::ATOM,
            &[atoms.WM_DELETE_WINDOW],
        )
        .map_err(|e| WindowError::X11(format!("WM_PROTOCOLS: {e}")))?;

        // Fullscreen BEFORE mapping -- a `change_property` on the
        // unmapped window, per the module doc. Avoids the flash of a
        // windowed frame a post-map client message produces.
        conn.change_property32(
            xproto::PropMode::REPLACE,
            window,
            atoms._NET_WM_STATE,
            xproto::AtomEnum::ATOM,
            &[atoms._NET_WM_STATE_FULLSCREEN],
        )
        .map_err(|e| WindowError::X11(format!("_NET_WM_STATE: {e}")))?;

        conn.map_window(window)
            .map_err(|e| WindowError::X11(format!("map_window: {e}")))?;
        conn.flush()
            .map_err(|e| WindowError::X11(format!("flush after map: {e}")))?;

        // Block for the WM's first configure: only then do we know the
        // real (fullscreen) surface size.
        let (width, height) = loop {
            let event = conn
                .wait_for_event()
                .map_err(|e| WindowError::X11(format!("waiting for the initial configure: {e}")))?;
            if let Event::ConfigureNotify(ev) = event {
                if ev.window == window {
                    break (u32::from(ev.width), u32::from(ev.height));
                }
            }
        };

        // Fallback: not every WM applies `_NET_WM_STATE` from an
        // unmapped window's initial properties. If a post-map
        // `get_property` roundtrip shows the fullscreen state didn't
        // take, ask again the EWMH way: a client message to the root
        // window.
        let has_fullscreen = conn
            .get_property(
                false,
                window,
                atoms._NET_WM_STATE,
                xproto::AtomEnum::ATOM,
                0,
                1024,
            )
            .map_err(|e| WindowError::X11(format!("_NET_WM_STATE get_property: {e}")))?
            .reply()
            .map(|reply| {
                reply
                    .value32()
                    .map(|mut values| values.any(|atom| atom == atoms._NET_WM_STATE_FULLSCREEN))
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        if !has_fullscreen {
            let event = xproto::ClientMessageEvent::new(
                32,
                window,
                atoms._NET_WM_STATE,
                [
                    NET_WM_STATE_ADD,
                    atoms._NET_WM_STATE_FULLSCREEN,
                    0,
                    1, // source indication: normal application
                    0,
                ],
            );
            conn.send_event(
                false,
                root,
                xproto::EventMask::SUBSTRUCTURE_NOTIFY | xproto::EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )
            .map_err(|e| WindowError::X11(format!("_NET_WM_STATE client message: {e}")))?;
            conn.flush()
                .map_err(|e| WindowError::X11(format!("flush after fullscreen fallback: {e}")))?;
        }

        let xkb_state = init_xkb_state(&conn)?;

        let mut backend = Self {
            conn,
            window,
            root,
            atoms,
            gc,
            depth,
            width,
            height,
            pixmaps: HashMap::new(),
            last_placement: None,
            next_serial: 0,
            xkb_state,
            events: Vec::new(),
        };
        backend.clear_window()?;
        Ok(backend)
    }

    /// Fill the entire window black. Called on every real
    /// `ConfigureNotify` (a resize can move the image's old content to
    /// stale coordinates, which a border-only clear wouldn't reach) and
    /// once at the end of `open`.
    fn clear_window(&mut self) -> Result<(), WindowError> {
        let rectangle = rect(0, 0, self.width as i32, self.height as i32);
        self.conn
            .poly_fill_rectangle(self.window, self.gc, &[rectangle])
            .map_err(|e| WindowError::X11(format!("clearing the window: {e}")))?;
        self.conn
            .flush()
            .map_err(|e| WindowError::X11(format!("flush after clear: {e}")))?;
        Ok(())
    }

    /// Fill the border bands `placement` leaves uncovered. Only called
    /// when `placement` differs from the last call -- see the module doc.
    fn clear_surround(&mut self, placement: &Placement) -> Result<(), WindowError> {
        let rects = surround_rects(placement);
        if rects.is_empty() {
            return Ok(());
        }
        self.conn
            .poly_fill_rectangle(self.window, self.gc, &rects)
            .map_err(|e| WindowError::X11(format!("clearing the surround: {e}")))?;
        Ok(())
    }

    /// The cached `Pixmap` for `frame.buffer_id`, importing (or
    /// re-importing, if the buffer's shape changed under a resize) only
    /// when necessary. Mirrors `wayland.rs`'s `AppData::buffer_for`.
    fn pixmap_for(&mut self, frame: &PublishedFrame) -> Result<xproto::Pixmap, WindowError> {
        if let Some(cached) = self.pixmaps.get(&frame.buffer_id) {
            if cached.width == frame.width
                && cached.height == frame.height
                && cached.modifier == frame.modifier
            {
                return Ok(cached.pixmap);
            }
        }

        let plane0 = frame
            .planes
            .first()
            .ok_or_else(|| WindowError::X11("PublishedFrame has no planes".to_string()))?;
        let stride0: u32 = plane0.stride.try_into().map_err(|_| {
            WindowError::X11(format!(
                "plane 0 stride {} does not fit in the protocol's u32",
                plane0.stride
            ))
        })?;
        let offset0: u32 = plane0.offset.try_into().map_err(|_| {
            WindowError::X11(format!(
                "plane 0 offset {} does not fit in the protocol's u32",
                plane0.offset
            ))
        })?;
        let width: u16 = frame.width.try_into().map_err(|_| {
            WindowError::X11(format!("frame width {} does not fit in u16", frame.width))
        })?;
        let height: u16 = frame.height.try_into().map_err(|_| {
            WindowError::X11(format!("frame height {} does not fit in u16", frame.height))
        })?;

        // dup before handing off -- see the module doc on fd ownership.
        let owned_fd = dup_fd(frame.fd)?;

        let pixmap = self
            .conn
            .generate_id()
            .map_err(|e| WindowError::X11(format!("generate_id (pixmap): {e}")))?;
        self.conn
            .dri3_pixmap_from_buffers(
                pixmap,
                self.window,
                width,
                height,
                stride0,
                offset0,
                0,
                0,
                0,
                0,
                0,
                0,
                self.depth,
                32, // bpp: matches the 4-byte-per-pixel export format
                frame.modifier,
                vec![owned_fd],
            )
            .map_err(|e| WindowError::X11(format!("dri3_pixmap_from_buffers: {e}")))?;

        let stale = self.pixmaps.insert(
            frame.buffer_id,
            CachedPixmap {
                pixmap,
                width: frame.width,
                height: frame.height,
                modifier: frame.modifier,
            },
        );
        // A `buffer_id` reused with a different shape (a resize) leaves
        // the old Pixmap with nothing left pointing at it -- free it
        // explicitly rather than leaking the server-side resource.
        if let Some(stale) = stale {
            let _ = self.conn.free_pixmap(stale.pixmap);
        }

        Ok(pixmap)
    }
}

impl Backend for X11Backend {
    fn present(
        &mut self,
        frame: &PublishedFrame,
        placement: &Placement,
    ) -> Result<(), WindowError> {
        if self.last_placement != Some(*placement) {
            self.clear_surround(placement)?;
            self.last_placement = Some(*placement);
        }

        let pixmap = self.pixmap_for(frame)?;

        self.next_serial = self.next_serial.wrapping_add(1);
        self.conn
            .present_pixmap(
                self.window,
                pixmap,
                self.next_serial,
                0, // valid region: None -- the whole pixmap is valid
                0, // update region: None -- treat the whole pixmap as updated
                placement.origin_x as i16,
                placement.origin_y as i16,
                0, // target_crtc: None -- let the server pick
                0, // wait_fence: None
                0, // idle_fence: None
                0, // options: None
                0, // target_msc
                0, // divisor
                0, // remainder: present as soon as possible
                &[],
            )
            .map_err(|e| WindowError::X11(format!("present_pixmap: {e}")))?;

        self.conn
            .flush()
            .map_err(|e| WindowError::X11(format!("flush after present: {e}")))?;
        Ok(())
    }

    fn poll_events(&mut self) -> Result<Vec<WindowEvent>, WindowError> {
        while let Some(event) = self
            .conn
            .poll_for_event()
            .map_err(|e| WindowError::X11(format!("polling for events: {e}")))?
        {
            match event {
                Event::KeyPress(ev) => {
                    let keycode = xkb::Keycode::from(ev.detail);
                    let keysym = self.xkb_state.key_get_syms(keycode).first().copied();
                    self.xkb_state.update_key(keycode, xkb::KeyDirection::Down);
                    if let Some(sym) = keysym {
                        self.events.push(WindowEvent::Key {
                            keysym: sym.raw(),
                            down: true,
                        });
                    }
                }
                Event::KeyRelease(ev) => {
                    let keycode = xkb::Keycode::from(ev.detail);
                    let keysym = self.xkb_state.key_get_syms(keycode).first().copied();
                    self.xkb_state.update_key(keycode, xkb::KeyDirection::Up);
                    if let Some(sym) = keysym {
                        self.events.push(WindowEvent::Key {
                            keysym: sym.raw(),
                            down: false,
                        });
                    }
                }
                Event::ButtonPress(ev) => match classify_button(ev.detail) {
                    ButtonKind::Click(button) => self.events.push(WindowEvent::PointerButton {
                        x: i32::from(ev.event_x),
                        y: i32::from(ev.event_y),
                        button,
                        down: true,
                    }),
                    ButtonKind::Scroll { dx, dy } => {
                        self.events.push(WindowEvent::Wheel { dx, dy })
                    }
                },
                Event::ButtonRelease(ev) => {
                    // Scroll "buttons" arrive as an immediate press/release
                    // pair per notch; the Wheel event was already emitted
                    // on the press half, so the release half is dropped
                    // here rather than double-firing it.
                    if let ButtonKind::Click(button) = classify_button(ev.detail) {
                        self.events.push(WindowEvent::PointerButton {
                            x: i32::from(ev.event_x),
                            y: i32::from(ev.event_y),
                            button,
                            down: false,
                        });
                    }
                }
                Event::MotionNotify(ev) => {
                    self.events.push(WindowEvent::PointerMotion {
                        x: i32::from(ev.event_x),
                        y: i32::from(ev.event_y),
                    });
                }
                Event::ConfigureNotify(ev) if ev.window == self.window => {
                    let new_width = u32::from(ev.width);
                    let new_height = u32::from(ev.height);
                    let changed = new_width != self.width || new_height != self.height;
                    self.width = new_width;
                    self.height = new_height;
                    if changed {
                        self.clear_window()?;
                        self.events.push(WindowEvent::Resized {
                            width: new_width,
                            height: new_height,
                        });
                    }
                }
                Event::ClientMessage(ev) => {
                    let data = ev.data.as_data32();
                    if ev.format == 32
                        && ev.window == self.window
                        && data[0] == self.atoms.WM_DELETE_WINDOW
                    {
                        self.events.push(WindowEvent::CloseRequested);
                    }
                }
                Event::Error(err) => {
                    tracing::warn!(?err, "X server reported a protocol error");
                }
                _ => {}
            }
        }

        self.conn
            .flush()
            .map_err(|e| WindowError::X11(format!("flush: {e}")))?;

        Ok(std::mem::take(&mut self.events))
    }

    fn minimize(&mut self) -> Result<(), WindowError> {
        // ICCCM iconify: a `WM_CHANGE_STATE` client message to the root
        // window, since a client cannot unmap its own window and expect
        // the WM to treat that as an iconify (it would look like a
        // withdrawal instead).
        let event = xproto::ClientMessageEvent::new(
            32,
            self.window,
            self.atoms.WM_CHANGE_STATE,
            [ICCCM_ICONIC_STATE, 0, 0, 0, 0],
        );
        self.conn
            .send_event(
                false,
                self.root,
                xproto::EventMask::SUBSTRUCTURE_NOTIFY | xproto::EventMask::SUBSTRUCTURE_REDIRECT,
                event,
            )
            .map_err(|e| WindowError::X11(format!("WM_CHANGE_STATE client message: {e}")))?;
        self.conn
            .flush()
            .map_err(|e| WindowError::X11(format!("flush after minimize: {e}")))?;
        Ok(())
    }

    fn output_size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    fn event_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd;
        self.conn.as_raw_fd()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_buttons_classify_as_clicks() {
        assert_eq!(classify_button(1), ButtonKind::Click(1));
        assert_eq!(classify_button(2), ButtonKind::Click(2));
        assert_eq!(classify_button(3), ButtonKind::Click(3));
    }

    #[test]
    fn wheel_buttons_are_not_forwarded_as_clicks() {
        // The specific bug this test exists to catch: silently forwarding
        // button 4 as `PointerButton` looks like "scrolling pastes
        // things" to whoever notices it on the remote end.
        assert_eq!(classify_button(4), ButtonKind::Scroll { dx: 0, dy: -1 });
        assert_eq!(classify_button(5), ButtonKind::Scroll { dx: 0, dy: 1 });
        assert_eq!(classify_button(6), ButtonKind::Scroll { dx: -1, dy: 0 });
        assert_eq!(classify_button(7), ButtonKind::Scroll { dx: 1, dy: 0 });
    }

    #[test]
    fn vertical_and_horizontal_wheel_signs_are_distinct() {
        // Up and left must not collapse to the same delta, nor down and
        // right -- a transposition here is as easy to introduce as the
        // evdev left/right button swap the Wayland backend guards against.
        let up = classify_button(4);
        let down = classify_button(5);
        let left = classify_button(6);
        let right = classify_button(7);
        assert_ne!(up, down);
        assert_ne!(left, right);
        assert_ne!(up, left);
    }

    #[test]
    fn an_unrecognised_button_is_forwarded_not_dropped() {
        // Side/extra/forward/back buttons (8+) have no wheel meaning;
        // unlike the Wayland backend's evdev map (which has no wire
        // representation for them and drops them), X11's button number
        // *is* the wire's button number, so there's no reason to guess
        // and drop here -- forward it and let the remote decide.
        assert_eq!(classify_button(8), ButtonKind::Click(8));
    }

    fn placement(
        origin_x: i32,
        origin_y: i32,
        image_w: u32,
        image_h: u32,
        out_w: u32,
        out_h: u32,
    ) -> Placement {
        Placement {
            origin_x,
            origin_y,
            image_w,
            image_h,
            out_w,
            out_h,
        }
    }

    #[test]
    fn image_smaller_than_output_yields_four_bands() {
        // A 100x100 image centred in a 300x200 output: origin (100, 50).
        let p = placement(100, 50, 100, 100, 300, 200);
        let rects = surround_rects(&p);
        assert_eq!(rects.len(), 4, "{rects:?}");

        let total: u32 = rects.iter().map(|r| r.width as u32 * r.height as u32).sum();
        let covered = p.image_w * p.image_h;
        assert_eq!(
            total + covered,
            p.out_w * p.out_h,
            "bands plus the image must tile the whole output exactly"
        );
    }

    #[test]
    fn image_exactly_filling_output_yields_no_bands() {
        let p = placement(0, 0, 300, 200, 300, 200);
        assert!(surround_rects(&p).is_empty());
    }

    #[test]
    fn image_wider_than_output_pins_to_zero_and_has_no_horizontal_bands() {
        // `Placement::centre` pins origin to 0 and crops when the image
        // exceeds the output on an axis (see geometry.rs); the left/right
        // bands must vanish, not go negative.
        let p = placement(0, 25, 400, 100, 300, 150);
        let rects = surround_rects(&p);
        // No left/right band: nothing to clamp x to 0 or width to
        // out_w - img_right, both would be <= 0.
        for r in &rects {
            assert!(r.x >= 0);
            assert!(r.width as i32 + r.x as i32 <= p.out_w as i32);
        }
        // Only top/bottom bands remain (image fills the full width).
        assert!(rects.iter().all(|r| r.width as u32 == p.out_w));
    }

    #[test]
    fn image_pinned_to_top_left_has_no_top_or_left_band() {
        let p = placement(0, 0, 100, 100, 300, 200);
        let rects = surround_rects(&p);
        assert_eq!(rects.len(), 2, "{rects:?}"); // only bottom + right
        for r in &rects {
            assert!(r.x >= 0 && r.y >= 0);
        }
    }

    #[test]
    fn rect_helper_clamps_negative_dimensions_to_zero() {
        let r = rect(0, 0, -5, -5);
        assert_eq!(r.width, 0);
        assert_eq!(r.height, 0);
    }
}
