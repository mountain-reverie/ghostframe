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
//! 2. **`GetOverlayWindow(root)`** — when COMPOSITE is present, capture
//!    from the composite overlay window: the per-screen window above
//!    all redirected children that compositors render into. XRender-
//!    based compositors (E20 default, picom-legacy) blit the final
//!    composite here, so `GetImage` returns the visible desktop.
//!    GL-backed compositors that bypass XRender and write straight to
//!    the framebuffer via DRI3 may leave the overlay pixmap empty — we
//!    log a warn-level hint to disable the WM compositor or switch to
//!    a non-compositing WM in that case.
//!
//! We deliberately do NOT call `RedirectSubwindows(root, Automatic)`
//! ourselves. The systemd dependency chain (`ghostframe-wm.service`
//! before `ghostframe-xdaemon.service`) doesn't actually guarantee the
//! WM's compositor has claimed its redirect yet — "service started"
//! just means the binary launched. We can race and win the redirect;
//! the WM then gets `BadAccess` when it tries to set `Manual` and its
//! compositor silently fails to start, leaving the desktop visually
//! broken even though our capture briefly looks correct. Always
//! deferring to the overlay window keeps the WM's compositor alive and
//! gives us the same composited output the WM produces for its own
//! drawing target.

use std::io;

use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};
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
    /// We're reading the composite overlay window — the per-screen
    /// window that XRender-based WM compositors render their final
    /// blend into.
    Overlay,
    /// COMPOSITE extension unavailable. `GetImage(root)` returns
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

    // Step 2: Read the composite overlay window — the per-screen window
    // X servers create above all redirected children for compositors to
    // render into. XRender-based compositors (E20 default, picom-legacy)
    // blit the final composite here, so GetImage on it shows the
    // visible desktop. GL-only compositors that bypass XRender and
    // write directly to the framebuffer via DRI3 may leave it empty;
    // that case is unsalvageable without a GL/DRI3 readback path and is
    // documented as a `disable compositor` hint at warn level.
    //
    // We do NOT attempt CompositeRedirectAutomatic ourselves: only one
    // client may hold an Automatic redirect on a given parent's
    // subwindows, and if we win the race against the WM compositor's
    // Manual redirect (the systemd ordering only guarantees the binary
    // launched, not that it grabbed the redirect), the WM's compositor
    // gets BadAccess and silently fails to start, leaving the desktop
    // visually broken.
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
