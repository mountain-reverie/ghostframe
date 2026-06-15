//! X11 framebuffer capture, composite + DRI3 aware.
//!
//! Capture path for environments without DRM (e.g. Docker with Xorg dummy
//! driver, or guest-mode installs where Xorg holds DRM master and our
//! DRM writeback path is unreachable).
//!
//! ## Compositor handling
//!
//! Modern WMs (Enlightenment, Mutter, KWin, etc.) ship a built-in
//! compositor that calls `XCompositeRedirectSubwindows(root, Manual)`.
//! Once a compositor is active, top-level windows render into off-screen
//! pixmaps and the root window pixmap stops receiving their contents —
//! `GetImage(root, …)` then captures only the desktop background.
//!
//! Resolution order on connect:
//!
//! 1. **Query COMPOSITE.** Absent → capture from root (Xvfb-style) and warn.
//! 2. **`GetOverlayWindow(root)`.** The per-screen window above all
//!    redirected children that compositors render their final blend into.
//! 3. **Query DRI3.** If present, use the DRI3 path per-frame: name the
//!    overlay's current backing pixmap via `CompositeNameWindowPixmap`,
//!    then `DRI3BuffersFromPixmap` to get the dma-buf fd plus modifier.
//!    Hand the dma-buf to the GPU pipeline via `FrameSubmission.dmabuf_fd`
//!    — this is what works for OpenGL/EGL compositors (Enlightenment-GL,
//!    Mutter-GLES, KWin-EGL, picom-glx) whose composited output lives in
//!    a DMA-BUF the X server can't pixel-copy through `GetImage`.
//! 4. **Fall back to `GetImage` on the overlay** when DRI3 is absent or
//!    rejects the pixmap. Works for XRender compositors; produces black
//!    frames for GL compositors (see the `disable compositor` hint logged
//!    on the rare instances we end up here).
//!
//! ## DRI3 path — modifier handling
//!
//! `DRI3BuffersFromPixmap` returns a `modifier: u64` describing the
//! buffer's tiling/compression layout. Today the GPU pipeline's
//! `import_dmabuf` in `ghostframe-lib` imports the dma-buf as
//! `VkImageTiling::LINEAR`, so we only forward dma-bufs whose modifier
//! is `DRM_FORMAT_MOD_LINEAR (0)` or `DRM_FORMAT_MOD_INVALID
//! (u64::MAX)` (treated as "implementation-defined linear" by older
//! DRI3 servers that predate the modifier query). For non-linear
//! modifiers (AMD GFX9_64K_S, Intel Y-tiled, DCC-compressed) we log
//! once and fall through to `GetImage` until the GPU pipeline gains
//! `VK_EXT_image_drm_format_modifier` support — see the open follow-up.
//!
//! We deliberately do NOT call `RedirectSubwindows(root, Automatic)`
//! ourselves: only one client may hold an Automatic redirect on a given
//! parent's subwindows, and racing the WM compositor's `Manual` redirect
//! (systemd ordering only guarantees the binary launched, not that it
//! grabbed the redirect) leaves the WM compositor `BadAccess`-broken
//! with no on-screen rendering.

use std::io;
use std::os::unix::io::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::dri3::ConnectionExt as Dri3Ext;
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
    /// DRI3 path: per-frame `CompositeNameWindowPixmap` +
    /// `DRI3BuffersFromPixmap` on the overlay's current backing,
    /// handing the dma-buf fd to the GPU pipeline.
    Dri3Overlay,
    /// `GetImage` on the composite overlay window — works for XRender
    /// compositors only. Used when DRI3 is missing or the compositor
    /// hands out modifiers our GPU import path doesn't yet support.
    Overlay,
    /// COMPOSITE extension unavailable. `GetImage(root)` returns
    /// whatever's painted into the root pixmap — typically the bare
    /// desktop background on modern WMs.
    Root,
}

/// DRM_FORMAT_MOD_LINEAR. Buffer is row-major, no tiling, no compression.
/// Our existing `import_dmabuf` in `ghostframe-lib` assumes this.
const DRM_FORMAT_MOD_LINEAR: u64 = 0;
/// DRM_FORMAT_MOD_INVALID. Returned by older DRI3 servers that predate
/// the modifier query — treat as "implementation-defined linear" since
/// any tiling info has been lost by the time the pixmap reaches us.
const DRM_FORMAT_MOD_INVALID: u64 = u64::MAX;

/// Captures frames from the X11 root window or the composite overlay
/// window, optionally via DRI3 zero-copy dma-buf import.
pub struct X11Capture {
    conn: RustConnection,
    /// Window to capture from. Either root or the composite overlay.
    capture_window: u32,
    #[allow(dead_code)]
    root: u32,
    width: u16,
    height: u16,
    source: CaptureSource,
    /// `true` once we've logged a "modifier not supported" warning —
    /// flips dri3 capture off for the rest of the process lifetime to
    /// avoid spamming the journal at 30 fps. Lazily flipped on first
    /// non-linear modifier; never reset.
    dri3_disabled_for_modifier: AtomicBool,
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
            dri3_disabled_for_modifier: AtomicBool::new(false),
        })
    }

    /// Capture identifier for tests and diagnostic logging.
    #[allow(dead_code)]
    pub fn source(&self) -> &'static str {
        match self.source {
            CaptureSource::Dri3Overlay => "dri3-overlay",
            CaptureSource::Overlay => "overlay",
            CaptureSource::Root => "root",
        }
    }

    /// Capture a single frame.
    ///
    /// When `source == Dri3Overlay`, returns a `FrameSubmission` whose
    /// `dmabuf_fd` is set (the GPU pipeline imports zero-copy). On any
    /// DRI3 failure (unsupported modifier, BadDrawable from a swapped-
    /// out pixmap, sync error) the call falls through to `GetImage` on
    /// the overlay so the encoder never starves.
    pub fn capture(&self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        if self.source == CaptureSource::Dri3Overlay
            && !self.dri3_disabled_for_modifier.load(Ordering::Relaxed)
        {
            match self.try_capture_dri3(timestamp_us) {
                Ok(submission) => return Ok(submission),
                Err(e) => {
                    // Per-frame DRI3 errors are unusual but recoverable;
                    // trace and fall through to GetImage so the pipeline
                    // keeps running. Modifier-unsupported is the only
                    // case that latches off permanently (handled inside).
                    tracing::trace!(error = %e, "DRI3 capture failed; falling back to GetImage for this frame");
                }
            }
        }
        self.capture_via_get_image(timestamp_us)
    }

    /// DRI3 dma-buf capture — names the overlay window's current backing
    /// pixmap, retrieves its dma-buf fd via `BuffersFromPixmap`, and
    /// returns it on `FrameSubmission.dmabuf_fd`. Always frees the
    /// transient pixmap ID before returning.
    fn try_capture_dri3(&self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        // Name the overlay's CURRENT backing pixmap. The compositor
        // swaps pixmaps each Present, so we name fresh every frame —
        // a single cached name would race the swap and we'd capture an
        // already-recycled buffer.
        let pixmap_id = self.conn.generate_id().map_err(io::Error::other)?;
        self.conn
            .composite_name_window_pixmap(self.capture_window, pixmap_id)
            .map_err(io::Error::other)?;
        // No check() — saves a round-trip. If the call failed the next
        // BuffersFromPixmap will surface BadDrawable and we fall back.

        let buffers_cookie = match self.conn.dri3_buffers_from_pixmap(pixmap_id) {
            Ok(c) => c,
            Err(e) => {
                let _ = self.conn.free_pixmap(pixmap_id);
                return Err(io::Error::other(format!("dri3_buffers_from_pixmap send: {e}")));
            }
        };
        let reply = match buffers_cookie.reply() {
            Ok(r) => r,
            Err(e) => {
                let _ = self.conn.free_pixmap(pixmap_id);
                return Err(io::Error::other(format!("dri3_buffers_from_pixmap reply: {e}")));
            }
        };

        let capture_done_ns = monotonic_now_ns();
        let _ = self.conn.free_pixmap(pixmap_id);

        if reply.buffers.is_empty() {
            return Err(io::Error::other("BuffersFromPixmap returned 0 buffers"));
        }
        if reply.buffers.len() > 1 {
            return Err(io::Error::other(format!(
                "BuffersFromPixmap returned {} planes; single-plane RGB only",
                reply.buffers.len()
            )));
        }

        // Modifier gate. The GPU pipeline's `import_dmabuf` uses
        // `vk::ImageTiling::LINEAR`, which only matches LINEAR/INVALID
        // modifiers. Other modifiers (AMD GFX9/10 tiled, Intel Y-tiled,
        // DCC-compressed) would feed scrambled bytes to the encoder.
        if reply.modifier != DRM_FORMAT_MOD_LINEAR
            && reply.modifier != DRM_FORMAT_MOD_INVALID
        {
            self.dri3_disabled_for_modifier.store(true, Ordering::Relaxed);
            tracing::warn!(
                modifier = format!("0x{:016x}", reply.modifier),
                depth = reply.depth,
                bpp = reply.bpp,
                "compositor pixmap uses non-LINEAR DRM modifier; disabling DRI3 capture for \
                 the rest of this process. The GPU pipeline's import path is LINEAR-only \
                 today — needs VK_EXT_image_drm_format_modifier wiring to consume tiled / \
                 DCC-compressed buffers. Falling back to GetImage (will be empty on GL \
                 compositors; consider disabling the WM compositor as a workaround)."
            );
            return Err(io::Error::other("non-linear modifier"));
        }

        // Take ownership of the single dma-buf fd. `buffers` is a
        // Vec<OwnedFd>; the IntoIterator pulls the fd out cleanly.
        let fd: OwnedFd = reply
            .buffers
            .into_iter()
            .next()
            .expect("len == 1 verified above");

        let stride = reply.strides.first().copied().unwrap_or_else(|| {
            // Server didn't send a stride array (shouldn't happen per
            // dri3proto §BuffersFromPixmap, but be defensive). Compute
            // from width × bpp; works for LINEAR which is the only
            // path that reaches here.
            (reply.width as u32) * 4
        });
        let width = reply.width as u32;
        let height = reply.height as u32;

        Ok(FrameSubmission {
            width,
            height,
            stride,
            pixels: Vec::new(),
            dmabuf_fd: Some(fd),
            timestamp_us,
            damage_tiles: None,
            capture_done_ns,
        })
    }

    /// GetImage fallback. Used directly when no compositor / DRI3 path
    /// is available, and as a per-frame fallback when DRI3 errors.
    fn capture_via_get_image(&self, timestamp_us: u32) -> io::Result<FrameSubmission> {
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

        let capture_done_ns = monotonic_now_ns();

        let width = self.width as u32;
        let height = self.height as u32;
        let stride = width * 4;

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

/// Decide which window we should capture from and whether DRI3 is
/// usable for that window. Logs the chosen path at info level; failures
/// log at warn and fall through to the next strategy.
fn pick_capture_target(conn: &RustConnection, root: u32) -> (u32, CaptureSource) {
    // Step 1: COMPOSITE extension probe.
    let composite_present = match conn.query_extension(b"Composite") {
        Ok(cookie) => match cookie.reply() {
            Ok(r) => r.present,
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
    if !composite_present {
        tracing::warn!("COMPOSITE extension not present; capturing root only");
        return (root, CaptureSource::Root);
    }

    // Step 2: Get the composite overlay window.
    let overlay = match conn.composite_get_overlay_window(root) {
        Ok(cookie) => match cookie.reply() {
            Ok(reply) => {
                tracing::info!(
                    overlay_window = reply.overlay_win,
                    "using composite overlay window for capture",
                );
                reply.overlay_win
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "composite_get_overlay_window reply failed; falling back to root"
                );
                return (root, CaptureSource::Root);
            }
        },
        Err(e) => {
            tracing::warn!(
                error = %e,
                "composite_get_overlay_window send failed; falling back to root"
            );
            return (root, CaptureSource::Root);
        }
    };

    // Step 3: DRI3 probe. If usable, we'll capture via dma-buf each
    // frame; if not, we stay on the GetImage path against the overlay.
    let dri3_present = match conn.query_extension(b"DRI3") {
        Ok(cookie) => cookie.reply().map(|r| r.present).unwrap_or(false),
        Err(_) => false,
    };
    if !dri3_present {
        tracing::warn!(
            "DRI3 extension not present; using GetImage on overlay. \
             This works for XRender compositors; GL/DRI3 compositors will \
             render empty frames — disable the WM compositor or switch to \
             a non-compositing WM (openbox, fluxbox, twm) if so."
        );
        return (overlay, CaptureSource::Overlay);
    }

    tracing::info!(
        "DRI3 available; capturing overlay window via BuffersFromPixmap dma-buf zero-copy"
    );
    (overlay, CaptureSource::Dri3Overlay)
}
