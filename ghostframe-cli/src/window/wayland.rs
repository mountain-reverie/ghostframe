//! The Wayland [`Backend`]: smithay-client-toolkit (SCTK) + wayland-client.
//!
//! A handful of decisions here are load-bearing enough to spell out once,
//! rather than leave implicit in the code below:
//!
//! - **Fullscreen before the first commit.** [`WaylandBackend::open`] calls
//!   `window.set_fullscreen(None)` before the initial `commit()`, so the
//!   compositor sizes the surface correctly from its very first configure
//!   rather than mapping a windowed surface we would then immediately
//!   resize.
//!
//! - **No composited background.** xdg-shell already requires a
//!   fullscreened surface smaller than the output to be centred by the
//!   compositor, with the remainder filled black -- exactly the
//!   presentation this showcase wants. Adding a subsurface, a background
//!   surface, or `wp_viewporter` would duplicate work the compositor
//!   already does, and create a second source of truth for the offset. We
//!   still compute that offset ourselves via [`Placement`] (in the
//!   caller, not here), because input must be mapped back to the remote
//!   image and the compositor never tells us where it decided to put the
//!   surface.
//!
//! - **One `wl_buffer` per `buffer_id`, cached.** The library recycles a
//!   small fixed set of export buffers (3 by default); re-importing a
//!   dmabuf every frame would be pure waste and would show up as jitter.
//!   [`AppData::buffer_for`] only imports when a `buffer_id` is new or its
//!   dimensions/modifier changed (e.g. across a resize).
//!
//! - **Keysyms need no translation.** SCTK yields `xkeysym::Keysym`
//!   (re-exported here as `Keysym`); `.raw()` is exactly the X11 keysym
//!   the ghostframe wire carries. This is the one place the native client
//!   is simpler than the browser client, which has to translate from
//!   `KeyboardEvent.code`.
//!
//! - **Pointer buttons are remapped.** SCTK reports Linux evdev button
//!   codes (`BTN_LEFT` = 0x110, ...); the wire expects X11-style button
//!   numbers (1 = left, 2 = middle, 3 = right). See
//!   [`map_evdev_button`] and its test -- transposing left and right here
//!   is easy to introduce and maddening for a user to notice.

use std::collections::HashMap;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

use smithay_client_toolkit::compositor::{CompositorHandler, CompositorState};
use smithay_client_toolkit::dmabuf::{DmabufFeedback, DmabufHandler, DmabufState};
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::reexports::protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1, zwp_linux_dmabuf_feedback_v1,
};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym};
use smithay_client_toolkit::seat::pointer::{self, PointerEvent, PointerEventKind, PointerHandler};
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::xdg::window::{
    Window, WindowConfigure, WindowDecorations, WindowHandler,
};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};

use wayland_client::backend::WaylandError;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{
    wl_buffer, wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface,
};
use wayland_client::{Connection, EventQueue, QueueHandle};

use ghostframe_client_native::PublishedFrame;

use crate::geometry::Placement;
use crate::window::{Backend, WindowError, WindowEvent};

/// `DRM_FORMAT_ABGR8888` (fourcc `'AB24'`, i.e. `0x34324241`): the DRM
/// fourcc for exactly the byte order Vulkan's `R8G8B8A8_UNORM` produces in
/// memory (R at the lowest address). `ghostframe-client-gpu`'s export path
/// (`ghostframe-client-gpu/src/export.rs`, `FORMAT`) always exports in that
/// Vulkan format, so this is the one fourcc this backend will ever need.
const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;

/// One cached `wl_buffer` import for a ring `buffer_id`, plus enough of the
/// `PublishedFrame` it was built from to tell whether a later frame with
/// the same `buffer_id` still matches it (a resize reuses buffer ids with
/// new dimensions).
struct CachedBuffer {
    wl_buffer: wl_buffer::WlBuffer,
    width: u32,
    height: u32,
    modifier: u64,
}

/// evdev button codes (from `linux/input-event-codes.h`, re-exported as
/// constants by SCTK's `seat::pointer` module) to the X11-style button
/// numbers the ghostframe wire uses: 1 = left, 2 = middle, 3 = right.
/// Anything else (side/extra/forward/back buttons) is reported as `None`
/// and dropped rather than guessed at.
fn map_evdev_button(evdev: u32) -> Option<u8> {
    match evdev {
        pointer::BTN_LEFT => Some(1),
        pointer::BTN_MIDDLE => Some(2),
        pointer::BTN_RIGHT => Some(3),
        _ => None,
    }
}

/// Import `frame`'s dmabuf as a new `wl_buffer`, one `add` per plane.
fn import_dmabuf(
    dmabuf_state: &DmabufState,
    frame: &PublishedFrame,
    qh: &QueueHandle<AppData>,
) -> Result<wl_buffer::WlBuffer, WindowError> {
    let params = dmabuf_state
        .create_params(qh)
        .map_err(|e| WindowError::Wayland(format!("zwp_linux_buffer_params_v1: {e}")))?;

    // SAFETY: `frame.fd` is a valid, open dmabuf fd owned by the library's
    // `ExportedImage` for as long as this `buffer_id` stays live in the
    // export ring (see `PublishedFrame::fd`'s doc: "do not close it").
    // `BorrowedFd` never closes the fd; `params.add` only reads it to
    // build a Wayland request.
    let fd = unsafe { BorrowedFd::borrow_raw(frame.fd) };

    for (i, plane) in frame.planes.iter().enumerate() {
        let offset: u32 = plane.offset.try_into().map_err(|_| {
            WindowError::Wayland(format!(
                "plane {i} offset {} does not fit in the protocol's u32",
                plane.offset
            ))
        })?;
        let stride: u32 = plane.stride.try_into().map_err(|_| {
            WindowError::Wayland(format!(
                "plane {i} stride {} does not fit in the protocol's u32",
                plane.stride
            ))
        })?;
        // `DmabufParams::add` splits `frame.modifier` into the hi/lo u32
        // halves the wire protocol wants; SCTK 0.21 does that internally,
        // so there is no manual bit-splitting to get wrong here.
        params.add(fd, i as u32, offset, stride, frame.modifier);
    }

    let (buffer, _params_proxy) = params.create_immed(
        frame.width as i32,
        frame.height as i32,
        DRM_FORMAT_ABGR8888,
        zwp_linux_buffer_params_v1::Flags::empty(),
        qh,
    );
    Ok(buffer)
}

/// State passed to every SCTK/wayland-client callback. Not `pub`: only
/// [`WaylandBackend`] (this module's [`Backend`] impl) touches it.
struct AppData {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    dmabuf_state: DmabufState,
    window: Window,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    /// The surface size the compositor last configured us to, i.e. the
    /// fullscreen output's size (or a sub-region of it under tiling). This
    /// is `output_size()`, not the remote image's resolution.
    width: u32,
    height: u32,
    /// Set on the first `configure`. `open()` blocks until this is true,
    /// since the protocol forbids attaching a buffer before it and we
    /// don't know the real surface size until then.
    configured: bool,
    /// Drained by `poll_events`.
    events: Vec<WindowEvent>,
    buffers: HashMap<u32, CachedBuffer>,
    feedback: Option<DmabufFeedback>,
}

impl AppData {
    /// The cached `wl_buffer` for `frame.buffer_id`, importing (or
    /// re-importing, if the buffer's shape changed under a resize) only
    /// when necessary.
    fn buffer_for(
        &mut self,
        frame: &PublishedFrame,
        qh: &QueueHandle<AppData>,
    ) -> Result<wl_buffer::WlBuffer, WindowError> {
        if let Some(cached) = self.buffers.get(&frame.buffer_id) {
            if cached.width == frame.width
                && cached.height == frame.height
                && cached.modifier == frame.modifier
            {
                return Ok(cached.wl_buffer.clone());
            }
        }

        let wl_buffer = import_dmabuf(&self.dmabuf_state, frame, qh)?;
        let out = wl_buffer.clone();
        let stale = self.buffers.insert(
            frame.buffer_id,
            CachedBuffer {
                wl_buffer,
                width: frame.width,
                height: frame.height,
                modifier: frame.modifier,
            },
        );
        // A `buffer_id` reused with a different shape (a resize) leaves
        // the old wl_buffer with nothing left pointing at it -- destroy it
        // explicitly rather than leaking the compositor-side object.
        if let Some(stale) = stale {
            stale.wl_buffer.destroy();
        }
        Ok(out)
    }
}

/// The Wayland [`Backend`]. Owns the connection, its event queue, and the
/// dispatch state ([`AppData`]) SCTK's handler traits are implemented on.
pub struct WaylandBackend {
    conn: Connection,
    event_queue: EventQueue<AppData>,
    qh: QueueHandle<AppData>,
    state: AppData,
}

impl WaylandBackend {
    pub(super) fn open(title: &str) -> Result<Self, WindowError> {
        let conn = Connection::connect_to_env()
            .map_err(|e| WindowError::Wayland(format!("connecting to the compositor: {e}")))?;
        let (globals, mut event_queue) = registry_queue_init::<AppData>(&conn)
            .map_err(|e| WindowError::Wayland(format!("enumerating globals: {e}")))?;
        let qh = event_queue.handle();

        let compositor = CompositorState::bind(&globals, &qh)
            .map_err(|e| WindowError::Wayland(format!("wl_compositor: {e}")))?;
        let xdg_shell = XdgShell::bind(&globals, &qh)
            .map_err(|e| WindowError::Wayland(format!("xdg_wm_base: {e}")))?;
        let dmabuf_state = DmabufState::new(&globals, &qh);
        if dmabuf_state.version().is_none() {
            return Err(WindowError::Wayland(
                "compositor does not advertise zwp_linux_dmabuf_v1 v3+".to_string(),
            ));
        }

        let surface = compositor.create_surface(&qh);
        let window = xdg_shell.create_window(surface, WindowDecorations::ServerDefault, &qh);
        window.set_title(title);
        window.set_app_id("io.ghostframe.client");

        // Fullscreen BEFORE the first commit -- see the module doc.
        window.set_fullscreen(None);
        window.commit();

        let mut state = AppData {
            registry_state: RegistryState::new(&globals),
            seat_state: SeatState::new(&globals, &qh),
            output_state: OutputState::new(&globals, &qh),
            dmabuf_state,
            window,
            keyboard: None,
            pointer: None,
            width: 0,
            height: 0,
            configured: false,
            events: Vec::new(),
            buffers: HashMap::new(),
            feedback: None,
        };

        // Block until the compositor's initial configure: only then do we
        // know the real (fullscreen) surface size, and xdg-shell forbids
        // attaching a buffer before it.
        while !state.configured {
            event_queue.blocking_dispatch(&mut state).map_err(|e| {
                WindowError::Wayland(format!("waiting for the initial configure: {e}"))
            })?;
        }

        // Best-effort: ask for the compositor's preferred dmabuf
        // format/modifier tranches so a caller can read them back via
        // `dmabuf_feedback()` and feed them into `Config::preferred_modifiers`
        // (M2 Task 9). A compositor stuck on protocol version 3 simply
        // never calls back here; `dmabuf_feedback()` then stays `None`,
        // which the caller falls back on `dmabuf_modifiers()` for.
        if let Ok(feedback) = state.dmabuf_state.get_default_feedback(&qh) {
            let _ = feedback;
            let _ = event_queue.roundtrip(&mut state);
        }

        Ok(Self {
            conn,
            event_queue,
            qh,
            state,
        })
    }

    /// The compositor's preferred dmabuf format/modifier tranches, if a
    /// zwp_linux_dmabuf_v1 v4+ compositor has sent them by now. `None`
    /// either means "not sent yet" (call `poll_events` again) or "this
    /// compositor only speaks v3", in which case
    /// [`WaylandBackend::dmabuf_modifiers`] is the fallback.
    pub fn dmabuf_feedback(&self) -> Option<&DmabufFeedback> {
        self.state.feedback.as_ref()
    }

    /// Format/modifier pairs advertised the pre-feedback way (protocol
    /// version 3): the fallback for [`WaylandBackend::dmabuf_feedback`] on
    /// a compositor that never sends feedback at all. Empty on a v4+
    /// compositor, which advertises modifiers exclusively through
    /// feedback instead.
    pub fn dmabuf_modifiers(&self) -> &[smithay_client_toolkit::dmabuf::DmabufFormat] {
        self.state.dmabuf_state.modifiers()
    }
}

impl Backend for WaylandBackend {
    /// Overrides the trait's empty default with the actual list from
    /// [`WaylandBackend::dmabuf_modifiers`] -- the v3 pre-feedback
    /// modifiers, already fetched by the roundtrip in `open`. `connect`
    /// (M2 Task 9) reads this before constructing the `Client`, so a v4+
    /// compositor's async feedback tranches (`dmabuf_feedback`) are
    /// deliberately not awaited here.
    fn preferred_dmabuf_modifiers(&self) -> Vec<u64> {
        self.dmabuf_modifiers().iter().map(|f| f.modifier).collect()
    }

    /// `placement` is accepted to satisfy the trait but unused here:
    /// xdg-shell already centres a fullscreened surface smaller than the
    /// output and fills the remainder black on its own (see the module
    /// doc), so attaching the buffer at surface-local (0,0) produces
    /// exactly that placement with no extra compositing on our part.
    fn present(
        &mut self,
        frame: &PublishedFrame,
        _placement: &Placement,
    ) -> Result<(), WindowError> {
        let buffer = self.state.buffer_for(frame, &self.qh)?;
        let surface = self.state.window.wl_surface();
        surface.attach(Some(&buffer), 0, 0);
        for rect in &frame.damage {
            surface.damage_buffer(rect.x as i32, rect.y as i32, rect.w as i32, rect.h as i32);
        }
        surface.commit();
        self.conn
            .flush()
            .map_err(|e| WindowError::Wayland(format!("flush after present: {e}")))?;
        Ok(())
    }

    fn poll_events(&mut self) -> Result<Vec<WindowEvent>, WindowError> {
        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| WindowError::Wayland(format!("dispatch: {e}")))?;

        // Non-blocking by construction: `read()` returns `WouldBlock`
        // rather than parking this thread if the socket had nothing
        // waiting, which is the entire point of a `poll_events` a
        // poll/epoll-driven host loop can call unconditionally.
        if let Some(guard) = self.event_queue.prepare_read() {
            match guard.read() {
                Ok(_) => {}
                Err(WaylandError::Io(ref e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(WindowError::Wayland(format!("reading the socket: {e}"))),
            }
        }

        self.event_queue
            .dispatch_pending(&mut self.state)
            .map_err(|e| WindowError::Wayland(format!("dispatch: {e}")))?;

        // Flush anything queued by the dispatch above (e.g. a `wl_buffer`
        // destroy from a stale-cache eviction) or by `present`/`minimize`
        // calls made since the last poll.
        self.conn
            .flush()
            .map_err(|e| WindowError::Wayland(format!("flush: {e}")))?;

        Ok(std::mem::take(&mut self.state.events))
    }

    fn minimize(&mut self) -> Result<(), WindowError> {
        self.state.window.set_minimized();
        self.conn
            .flush()
            .map_err(|e| WindowError::Wayland(format!("flush after minimize: {e}")))?;
        Ok(())
    }

    fn output_size(&self) -> (u32, u32) {
        (self.state.width, self.state.height)
    }

    fn event_fd(&self) -> RawFd {
        self.conn.as_fd().as_raw_fd()
    }
}

impl CompositorHandler for AppData {
    fn scale_factor_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_factor: i32,
    ) {
        // The showcase presents 1:1 pixels; scaling is an explicit
        // non-goal (see `geometry` module doc).
    }

    fn transform_changed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _new_transform: wl_output::Transform,
    ) {
        // Not needed: the showcase never rotates its output.
    }

    fn frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _time: u32,
    ) {
        // We never request a frame callback (`present` paces itself off
        // the library's own published frames), so this is never called.
    }

    fn surface_enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }

    fn surface_leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _surface: &wl_surface::WlSurface,
        _output: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for AppData {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }

    fn new_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn update_output(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }

    fn output_destroyed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _output: wl_output::WlOutput,
    ) {
    }
}

impl SeatHandler for AppData {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard if self.keyboard.is_none() => {
                match self.seat_state.get_keyboard(qh, &seat, None) {
                    Ok(keyboard) => self.keyboard = Some(keyboard),
                    Err(e) => tracing::warn!(
                        "keyboard capability advertised but get_keyboard failed: {e}"
                    ),
                }
            }
            Capability::Pointer if self.pointer.is_none() => {
                match self.seat_state.get_pointer(qh, &seat) {
                    Ok(pointer) => self.pointer = Some(pointer),
                    Err(e) => {
                        tracing::warn!("pointer capability advertised but get_pointer failed: {e}")
                    }
                }
            }
            _ => {}
        }
    }

    fn remove_capability(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        match capability {
            Capability::Keyboard => {
                if let Some(keyboard) = self.keyboard.take() {
                    keyboard.release();
                }
            }
            Capability::Pointer => {
                if let Some(pointer) = self.pointer.take() {
                    pointer.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _seat: wl_seat::WlSeat) {
    }
}

impl KeyboardHandler for AppData {
    fn enter(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
        _raw: &[u32],
        _keysyms: &[Keysym],
    ) {
        // Keys already held when focus arrives are not synthesised as
        // key-down events here; reconciling that against the chord state
        // machine is Task 9's remit, not this backend's.
    }

    fn leave(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _surface: &wl_surface::WlSurface,
        _serial: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.events.push(WindowEvent::Key {
            keysym: event.keysym.raw(),
            down: true,
        });
    }

    fn repeat_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        // A repeat is another down-edge on the wire; the remote's own
        // input stack handles auto-repeat from there, same as it would
        // for a natively-repeating physical keyboard.
        self.events.push(WindowEvent::Key {
            keysym: event.keysym.raw(),
            down: true,
        });
    }

    fn release_key(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        event: KeyEvent,
    ) {
        self.events.push(WindowEvent::Key {
            keysym: event.keysym.raw(),
            down: false,
        });
    }

    fn update_modifiers(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _keyboard: &wl_keyboard::WlKeyboard,
        _serial: u32,
        _modifiers: smithay_client_toolkit::seat::keyboard::Modifiers,
        _raw_modifiers: smithay_client_toolkit::seat::keyboard::RawModifiers,
        _layout: u32,
    ) {
        // The prefix-chord state machine (`crate::chord`) tracks modifier
        // state itself from the raw keysym stream (`press_key`/
        // `release_key` above), not from this summarised event.
    }
}

impl PointerHandler for AppData {
    fn pointer_frame(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _pointer: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        for event in events {
            if event.surface != *self.window.wl_surface() {
                continue;
            }
            let (x, y) = (
                event.position.0.round() as i32,
                event.position.1.round() as i32,
            );
            match event.kind {
                PointerEventKind::Enter { .. } | PointerEventKind::Leave { .. } => {}
                PointerEventKind::Motion { .. } => {
                    self.events.push(WindowEvent::PointerMotion { x, y });
                }
                PointerEventKind::Press { button, .. } => match map_evdev_button(button) {
                    Some(button) => self.events.push(WindowEvent::PointerButton {
                        x,
                        y,
                        button,
                        down: true,
                    }),
                    None => tracing::debug!(button, "unmapped pointer button ignored"),
                },
                PointerEventKind::Release { button, .. } => match map_evdev_button(button) {
                    Some(button) => self.events.push(WindowEvent::PointerButton {
                        x,
                        y,
                        button,
                        down: false,
                    }),
                    None => tracing::debug!(button, "unmapped pointer button ignored"),
                },
                PointerEventKind::Axis {
                    horizontal,
                    vertical,
                    ..
                } => {
                    self.events.push(WindowEvent::Wheel {
                        dx: axis_delta(horizontal.absolute),
                        dy: axis_delta(vertical.absolute),
                    });
                }
            }
        }
    }
}

/// Round and clamp a `wl_pointer` axis delta (pixels) into the wire's
/// `i16`. Saturates rather than wraps: a pathological scroll burst must
/// not turn into a delta of the opposite sign.
fn axis_delta(v: f64) -> i16 {
    v.round().clamp(i16::MIN as f64, i16::MAX as f64) as i16
}

impl DmabufHandler for AppData {
    fn dmabuf_state(&mut self) -> &mut DmabufState {
        &mut self.dmabuf_state
    }

    fn dmabuf_feedback(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _proxy: &zwp_linux_dmabuf_feedback_v1::ZwpLinuxDmabufFeedbackV1,
        feedback: DmabufFeedback,
    ) {
        self.feedback = Some(feedback);
    }

    fn created(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
        _buffer: wl_buffer::WlBuffer,
    ) {
        // `create_immed` (what `import_dmabuf` uses) hands back the
        // `wl_buffer` synchronously; this event only fires for the
        // non-immediate `create` request, which this backend never sends.
    }

    fn failed(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _params: &zwp_linux_buffer_params_v1::ZwpLinuxBufferParamsV1,
    ) {
        tracing::warn!("compositor rejected a dmabuf buffer import");
    }

    fn released(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _buffer: &wl_buffer::WlBuffer,
    ) {
        // A release just means the compositor is done reading this buffer
        // for the commit that used it; the cache in `AppData::buffers`
        // keeps it alive for reuse regardless.
    }
}

impl WindowHandler for AppData {
    fn request_close(&mut self, _conn: &Connection, _qh: &QueueHandle<Self>, _window: &Window) {
        self.events.push(WindowEvent::CloseRequested);
    }

    fn configure(
        &mut self,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
        _window: &Window,
        configure: WindowConfigure,
        _serial: u32,
    ) {
        // `None` in either axis means "no suggested change", not zero --
        // fall back to the current size, and only floor to 1 on the very
        // first configure (before which `self.width`/`height` are 0 and
        // there is no prior size to keep).
        let new_width = configure
            .new_size
            .0
            .map(NonZeroU32::get)
            .unwrap_or(self.width)
            .max(1);
        let new_height = configure
            .new_size
            .1
            .map(NonZeroU32::get)
            .unwrap_or(self.height)
            .max(1);

        let changed = self.configured && (new_width != self.width || new_height != self.height);
        self.width = new_width;
        self.height = new_height;
        self.configured = true;

        if changed {
            self.events.push(WindowEvent::Resized {
                width: new_width,
                height: new_height,
            });
        }
    }
}

impl ProvidesRegistryState for AppData {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(AppData);
delegate_dispatch2!(AppData);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn left_and_right_are_not_transposed() {
        // The specific bug this test exists to catch: swapping these two
        // is easy to type and invisible until someone right-clicks and
        // gets a left-click on the remote.
        assert_eq!(map_evdev_button(pointer::BTN_LEFT), Some(1));
        assert_eq!(map_evdev_button(pointer::BTN_RIGHT), Some(3));
    }

    #[test]
    fn middle_maps_to_two() {
        assert_eq!(map_evdev_button(pointer::BTN_MIDDLE), Some(2));
    }

    #[test]
    fn an_unrecognised_evdev_code_is_not_guessed_at() {
        assert_eq!(map_evdev_button(pointer::BTN_SIDE), None);
        assert_eq!(map_evdev_button(0xdead), None);
    }

    #[test]
    fn axis_delta_saturates_rather_than_wraps() {
        assert_eq!(axis_delta(1_000_000.0), i16::MAX);
        assert_eq!(axis_delta(-1_000_000.0), i16::MIN);
        assert_eq!(axis_delta(3.4), 3);
        assert_eq!(axis_delta(-3.6), -4);
    }

    #[test]
    fn drm_format_abgr8888_matches_the_fourcc_spec() {
        // fourcc_code('A','B','2','4') per drm_fourcc.h -- see the
        // `DRM_FORMAT_ABGR8888` doc comment for why this is the format
        // the GPU export path always produces.
        let expected = u32::from(b'A')
            | (u32::from(b'B') << 8)
            | (u32::from(b'2') << 16)
            | (u32::from(b'4') << 24);
        assert_eq!(DRM_FORMAT_ABGR8888, expected);
    }
}
