//! X11 framebuffer capture via GetImage.
//!
//! Fallback capture path for environments without DRM (e.g., Docker with
//! Xorg dummy driver). Captures the root window using X11's GetImage request.
//! This is slower than DRM capture (CPU copy through the X server) but works
//! with any X11 server including Xvfb and the dummy driver.

use std::io;

use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat};
use x11rb::rust_connection::RustConnection;

/// Captures frames from the X11 root window.
pub struct X11Capture {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
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
        tracing::info!(width, height, "X11 capture connected to display");
        Ok(Self { conn, root, width, height })
    }

    /// Capture the root window as a `FrameSubmission`.
    ///
    /// The returned pixels are in the X server's native format (typically
    /// BGRA or BGRX on little-endian systems with 24-bit TrueColor).
    pub fn capture(&self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width,
                self.height,
                !0, // all bit planes
            )
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;

        let width = self.width as u32;
        let height = self.height as u32;
        let stride = width * 4; // 32-bit pixels = 4 bytes

        Ok(FrameSubmission {
            width,
            height,
            stride,
            pixels: reply.data,
            timestamp_us,
        })
    }
}
