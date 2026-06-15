//! X11 framebuffer capture via GetImage, composite-aware.
//!
//! Fallback capture path for environments without DRM (e.g., Docker with
//! Xorg dummy driver, or a guest-mode install where Xorg holds DRM master
//! and the writeback path is unreachable). Captures via X11 `GetImage` —
//! slower than DRM (CPU copy through the X server) but works with any
//! X11 server, including Xvfb and the dummy driver.
//!
//! ## Compositor handling
//!
//! Modern WMs (Enlightenment, Mutter, KWin, etc.) ship a built-in
//! compositor that calls `XCompositeRedirectSubwindows(root, Manual)`.
//! Once a compositor is active, top-level windows are redirected to
//! off-screen pixmaps and the root window pixmap stops receiving their
//! contents — a vanilla `GetImage(root, …)` then captures only the
//! desktop background (typically solid black on first launch), not the
//! visible windows.
//!
//! Resolution order on connect:
//!
//! 1. **Query the COMPOSITE extension.** If absent, capture from root
//!    (legacy / Xvfb behaviour) and warn.
//! 2. **Try `RedirectSubwindows(root, Automatic)`.** No compositor is
//!    running — the X server now composes children into root for us, so
//!    capture from root. This is the desired path when the user hasn't
//!    started an external compositor.
//! 3. **`BadAccess` from step 2** means another compositor (the WM)
//!    already redirected. Fall back to `GetOverlayWindow(root)` and
//!    capture from the **composite overlay window** — the per-screen
//!    window the X server creates above all redirected children for
//!    compositors to render into. Most XRender-based compositors (E20
//!    default, picom legacy) draw their final blend here, so `GetImage`
//!    on it returns what's visible to the user. GL-backed compositors
//!    that render directly to the framebuffer via DRI3 may leave the
//!    overlay pixmap empty — that case is documented in the journal as
//!    a warn-level hint to disable the WM compositor.
//!
//! The redirect/overlay handle is held for the lifetime of the
//! `X11Capture` so the X server doesn't reclaim it.

use std::io;

use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::composite::{
    ConnectionExt as CompositeExt, Redirect as CompositeRedirect,
};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};
use x11rb::protocol::ErrorKind;
use x11rb::rust_connection::RustConnection;

/// Read CLOCK_MONOTONIC and return the value as nanoseconds.
fn monotonic_now_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime is async-signal-safe; CLOCK_MONOTONIC always
    // available on Linux.
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    (ts.tv_sec as u64) * 1_000_000_000 + (ts.tv_nsec as u64)
}

/// How `X11Capture` is sourcing pixels — recorded for diagnosability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureSource {
    /// We called `RedirectSubwindows(root, Automatic)` ourselves; the X
    /// server composites children into root, and `GetImage(root)`
    /// returns the full composited desktop.
    RootRedirected,
    /// Another client holds the redirect (a WM compositor). We read the
    /// composite overlay window, which compositors typically render to.
    Overlay,
    /// Composite extension is unavailable. `GetImage(root)` returns
    /// whatever's painted into the root pixmap — the bare desktop
    /// background without window contents on most modern WMs.
    Root,
}

/// Captures frames from the X11 root window or the composite overlay
/// window depending on what the server allows.
pub struct X11Capture {
    conn: RustConnection,
    /// The window to call `GetImage` on every frame. Either the X
    /// server's root, or the composite overlay window if a compositor
    /// is running.
    capture_window: u32,
    /// Root window of the screen, kept so callers can issue other
    /// requests against it (today: none; used for future damage hooks).
    #[allow(dead_code)]
    root: u32,
    width: u16,
    height: u16,
    source: CaptureSource,
}

impl X11Capture {
    /// Connect to the X display specified by `$DISPLAY`.
    pub fn new() -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = screen.width_in_pixels;
        let height = screen.height_in_pixels;

        let (capture_window, source) = pick_capture_target(&conn, root);
        tracing::info!(
            width,
            height,
            ?source,
            capture_window,
            "X11 capture connected to display"
        );

        Ok(Self {
            conn,
            capture_window,
            root,
            width,
            height,
            source,
        })
    }

    /// How this capture is sourcing pixels (root / overlay / redirected
    /// root). Useful for tests and diagnostic logging; not part of the
    /// hot path.
    #[allow(dead_code)]
    pub fn source(&self) -> &'static str {
        match self.source {
            CaptureSource::RootRedirected => "root-redirected",
            CaptureSource::Overlay => "overlay",
            CaptureSource::Root => "root",
        }
    }

    /// Capture the root (or overlay) window as a `FrameSubmission`.
    ///
    /// The returned pixels are in the X server's native format (typically
    /// BGRA or BGRX on little-endian systems with 24-bit TrueColor).
    pub fn capture(&self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.capture_window,
                0,
                0,
                self.width,
                self.height,
                !0, // all bit planes
            )
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;

        // Record capture completion after GetImage reply arrives; pixel data
        // is now stable in reply.data. Approximate but sufficient for M3.5
        // bench latency measurement on this degraded fallback path.
        let capture_done_ns = monotonic_now_ns();

        let width = self.width as u32;
        let height = self.height as u32;
        let stride = width * 4; // 32-bit pixels = 4 bytes

        Ok(FrameSubmission {
            width,
            height,
            stride,
            pixels: reply.data,
            dmabuf_fd: None,
            timestamp_us,
            damage_tiles: None,
            capture_done_ns,
        })
    }
}

/// Decide which window we should call `GetImage` on for every frame.
/// Logs the chosen path at info level; failures log at warn and fall
/// through to the next strategy.
fn pick_capture_target(conn: &RustConnection, root: u32) -> (u32, CaptureSource) {
    // Step 1: COMPOSITE extension probe.
    let ext = match conn.query_extension(b"Composite") {
        Ok(cookie) => match cookie.reply() {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "Composite extension query reply failed; capturing root only");
                return (root, CaptureSource::Root);
            }
        },
        Err(e) => {
            tracing::warn!(error = %e, "Composite extension query failed; capturing root only");
            return (root, CaptureSource::Root);
        }
    };
    if !ext.present {
        tracing::warn!("COMPOSITE extension not present; capturing root only");
        return (root, CaptureSource::Root);
    }

    // Step 2: Try to be the compositor ourselves. CompositeRedirectAutomatic
    // makes the server composite children into root for us so GetImage(root)
    // returns the full desktop. Only one client can hold a redirect; if a
    // WM compositor is already running this fails with BadAccess and we
    // fall through to the overlay path.
    //
    // The send-vs-check split: composite_redirect_subwindows() returns a
    // VoidCookie (or ConnectionError if the request couldn't be sent at
    // all). cookie.check() blocks for the server's reply and surfaces
    // protocol-level errors (BadAccess, BadWindow, …) as a ReplyError —
    // these two error types are not interconvertible in x11rb 0.13, so
    // we match each separately rather than chaining .and_then.
    let send_result = conn.composite_redirect_subwindows(root, CompositeRedirect::AUTOMATIC);
    match send_result {
        Ok(cookie) => match cookie.check() {
            Ok(()) => {
                tracing::info!(
                    "CompositeRedirectAutomatic acquired on root; capturing composited root"
                );
                return (root, CaptureSource::RootRedirected);
            }
            Err(ReplyError::X11Error(err)) if err.error_kind == ErrorKind::Access => {
                tracing::info!(
                    "CompositeRedirectAutomatic denied (existing compositor); trying overlay window"
                );
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "CompositeRedirectSubwindows reply error; trying overlay window"
                );
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "CompositeRedirectSubwindows send failed; trying overlay window"
            );
        }
    }

    // Step 3: A compositor is active. Read its overlay window — the
    // per-screen window X servers create above all redirected children
    // for compositors to render into. XRender-based compositors (E20
    // default, picom-legacy) blit the final composite here, so GetImage
    // on it shows the visible desktop. GL-only compositors that render
    // directly to the framebuffer via DRI3 may leave it empty; that
    // case is unsalvageable without a GL/DRI3 readback path and is
    // documented as a `disable compositor` hint at warn level.
    let overlay_send = conn.composite_get_overlay_window(root);
    match overlay_send {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => {
                tracing::info!(
                    overlay_window = reply.overlay_win,
                    "using composite overlay window for capture",
                );
                tracing::warn!(
                    "if the captured frame stays empty, the active WM compositor likely uses a \
                     GL/DRI3 backend that doesn't write to the overlay window. Disable the WM's \
                     compositor (Enlightenment: Settings → Compositor → disable) or switch to \
                     a non-compositing WM (openbox, fluxbox, twm) so the composited result lands \
                     in the X root pixmap."
                );
                (reply.overlay_win, CaptureSource::Overlay)
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "composite_get_overlay_window reply failed; falling back to root"
                );
                (root, CaptureSource::Root)
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "composite_get_overlay_window send failed; falling back to root"
            );
            (root, CaptureSource::Root)
        }
    }
}
