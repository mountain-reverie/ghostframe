//! X11 framebuffer capture, composite-aware via per-window pixmap
//! enumeration.
//!
//! ## Why per-window
//!
//! Modern WMs (Enlightenment, Mutter, KWin) ship a built-in compositor
//! that calls `XCompositeRedirectSubwindows(root, Manual)`. After
//! redirect, top-level windows render into off-screen pixmaps and the
//! root window pixmap stops receiving their contents — `GetImage(root)`
//! captures only the desktop background.
//!
//! We tried the obvious alternative — `XCompositeGetOverlayWindow` +
//! `GetImage` on the overlay — but that only works for XRender-based
//! compositors. EGL/DRI3 compositors (Enlightenment-GL, Mutter-GLES)
//! attach a DRI3 EGL surface to the overlay and render via OpenGL,
//! Presenting pixmaps that the X server tracks under the Present
//! extension's machinery, not Composite's. `CompositeNameWindowPixmap`
//! on the overlay returns `BadMatch` because the overlay isn't a
//! redirected drawable — it's a server-created drawing target.
//!
//! So we go window-by-window:
//!
//! 1. `XQueryTree(root)` returns root's children in **stacking order**
//!    (bottom to top per X11 spec §10.7).
//! 2. For each child that's `Viewable` and overlaps the screen:
//!    `CompositeNameWindowPixmap(win, …)` to alias the redirected
//!    pixmap as a drawable, then `GetImage` on that pixmap. The
//!    pixmap contents are whatever the app last rendered into the
//!    window — exactly what the WM's compositor would have composited.
//! 3. Blit each window's pixels into a reusable target framebuffer at
//!    the window's screen-space position. Bottom-to-top order means
//!    occluded regions are correctly overwritten by the topmost window.
//! 4. `XFixesGetCursorImage` for the cursor — pre-multiplied ARGB
//!    pixels + hot-spot — alpha-blended on top.
//!
//! No per-window XShape clipping yet (windows treated as rectangular)
//! and no per-window opacity (treated as fully opaque); compositor
//! drop-shadows aren't captured since they're rendered by the WM into
//! the overlay, not the windows themselves. These are cosmetic gaps
//! we accept for the first cut; functional content (window contents +
//! cursor) is correct.
//!
//! ## Fallbacks
//!
//! - No COMPOSITE extension → `GetImage(root)` (legacy Xvfb path).
//! - No active compositor (no `_NET_WM_CM_S0` selection owner) →
//!   `GetImage(root)` (root pixmap already holds the visible content).
//! - Per-window `NameWindowPixmap` fails (BadMatch on an unredirected
//!   child) → that window is skipped this frame and a debug log fires
//!   the first time it happens.

use std::io;

use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeExt;
use x11rb::protocol::xfixes::{ConnectionExt as XfixesExt, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, MapState};
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

/// Selected capture strategy. Logged at startup and via `source()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Strategy {
    /// Per-window `CompositeNameWindowPixmap` + `GetImage` of each
    /// top-level window, blitted into a reusable target buffer in
    /// stacking order.
    PerWindowComposite,
    /// `GetImage(root)` — used when COMPOSITE is unavailable or when
    /// no compositor is running (root pixmap already holds the visible
    /// content, Xvfb-style).
    RootGetImage,
}

/// Captures frames from the X server.
pub struct X11Capture {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    strategy: Strategy,
    /// Reusable target buffer for per-window composition. Allocated to
    /// `width * height * 4` once; cleared (memset to 0) each frame.
    target: Vec<u8>,
    /// One-shot flag for the first successful per-window composite —
    /// logs window count + dimensions so operators see we're not just
    /// silently returning empty frames.
    first_composite_logged: bool,
}

impl X11Capture {
    /// Connect to `$DISPLAY` and pick a capture strategy.
    pub fn new() -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let width = screen.width_in_pixels;
        let height = screen.height_in_pixels;

        let strategy = pick_strategy(&conn, root);
        tracing::info!(width, height, ?strategy, "X11 capture connected to display");

        let buf_size = (width as usize) * (height as usize) * 4;
        Ok(Self {
            conn,
            root,
            width,
            height,
            strategy,
            target: vec![0u8; buf_size],
            first_composite_logged: false,
        })
    }

    /// Capture identifier for tests and diagnostic logging.
    #[allow(dead_code)]
    pub fn source(&self) -> &'static str {
        match self.strategy {
            Strategy::PerWindowComposite => "per-window-composite",
            Strategy::RootGetImage => "root",
        }
    }

    /// Capture a single frame.
    pub fn capture(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        let frame = match self.strategy {
            Strategy::PerWindowComposite => self.capture_composite(timestamp_us)?,
            Strategy::RootGetImage => self.capture_root(timestamp_us)?,
        };
        // One-shot diagnostic: dump the first captured frame to the path
        // in `GHOSTFRAME_DUMP_FIRST_FRAME` so we can byte-compare against
        // `xwd -root` / `import -window root`. Unset on first call so
        // subsequent frames are not re-dumped. Only fires on the CPU
        // path (pixels.len() > 0); the DMA-BUF path would write 0 bytes.
        if !frame.pixels.is_empty() {
            if let Ok(path) = std::env::var("GHOSTFRAME_DUMP_FIRST_FRAME") {
                let header = format!(
                    "ghostframe-bgra width={} height={} stride={}\n",
                    frame.width, frame.height, frame.stride
                );
                let _ = std::fs::write(format!("{path}.meta"), header.as_bytes());
                let _ = std::fs::write(&path, &frame.pixels);
                std::env::remove_var("GHOSTFRAME_DUMP_FIRST_FRAME");
                tracing::info!(
                    path = %path,
                    width = frame.width,
                    height = frame.height,
                    bytes = frame.pixels.len(),
                    "GHOSTFRAME_DUMP_FIRST_FRAME: dumped raw BGRA to disk; \
                     compare with `xwd -root` / `import -window root` to \
                     verify whether XGetImage(root) is returning the visible \
                     desktop or stale/empty content"
                );
            }
        }
        Ok(frame)
    }

    /// Per-window composite path.
    fn capture_composite(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        // Reset target to opaque black — windows below the topmost
        // partially-covered region still need backing pixels.
        // memset to 0x00 RGB + 0xFF alpha (BGRA layout). Browsers
        // render the A=0 alpha case as transparent which then composes
        // against the page background — force A=255 so the canvas
        // shows opaque black where no window is.
        for chunk in self.target.chunks_exact_mut(4) {
            chunk[0] = 0; // B
            chunk[1] = 0; // G
            chunk[2] = 0; // R
            chunk[3] = 0xff; // A
        }

        // Enumerate root's children in stacking order (bottom → top).
        let tree = self
            .conn
            .query_tree(self.root)
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;

        let mut composited = 0usize;
        let mut skipped_unmapped = 0usize;
        let mut skipped_bad_match = 0usize;

        for &child in &tree.children {
            // Skip non-Viewable windows. GetWindowAttributes + GetGeometry
            // are batched per window because the protocol is async — but
            // for clarity we do them serially per window here. Roughly
            // tens of children on a real desktop; not worth the
            // pipelining complexity yet.
            let attrs = match self.conn.get_window_attributes(child) {
                Ok(c) => match c.reply() {
                    Ok(r) => r,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };
            if attrs.map_state != MapState::VIEWABLE {
                skipped_unmapped += 1;
                continue;
            }

            let geom = match self.conn.get_geometry(child) {
                Ok(c) => match c.reply() {
                    Ok(r) => r,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

            // Off-screen / zero-size → skip.
            if geom.width == 0 || geom.height == 0 {
                continue;
            }
            if geom.x >= self.width as i16
                || geom.y >= self.height as i16
                || (geom.x + geom.width as i16) <= 0
                || (geom.y + geom.height as i16) <= 0
            {
                continue;
            }

            // Name the redirected pixmap.
            let pixmap_id = self.conn.generate_id().map_err(io::Error::other)?;
            let name_cookie = self
                .conn
                .composite_name_window_pixmap(child, pixmap_id)
                .map_err(io::Error::other)?;
            if let Err(e) = name_cookie.check() {
                // BadMatch on a non-redirected child — happens for
                // override-redirect popup windows the WM didn't grab,
                // or for the overlay window. Skip silently after the
                // first occurrence.
                if !self.first_composite_logged {
                    tracing::debug!(
                        window = child,
                        error = %e,
                        "NameWindowPixmap failed; skipping window"
                    );
                }
                skipped_bad_match += 1;
                continue;
            }

            let img = match self.conn.get_image(
                ImageFormat::Z_PIXMAP,
                pixmap_id,
                0,
                0,
                geom.width,
                geom.height,
                !0,
            ) {
                Ok(cookie) => match cookie.reply() {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::debug!(window = child, error = %e, "GetImage on window pixmap failed");
                        let _ = self.conn.free_pixmap(pixmap_id);
                        continue;
                    }
                },
                Err(e) => {
                    tracing::debug!(window = child, error = %e, "GetImage send failed");
                    let _ = self.conn.free_pixmap(pixmap_id);
                    continue;
                }
            };
            let _ = self.conn.free_pixmap(pixmap_id);

            blit_window(
                &mut self.target,
                self.width as i32,
                self.height as i32,
                &img.data,
                BlitRect {
                    dst_x: geom.x as i32,
                    dst_y: geom.y as i32,
                    src_w: geom.width as i32,
                    src_h: geom.height as i32,
                },
            );
            composited += 1;
        }

        // Cursor overlay. Best-effort — if XFixes isn't present we just
        // skip; the captured frame is correct minus the cursor.
        if let Ok(cookie) = self.conn.xfixes_get_cursor_image() {
            if let Ok(cursor) = cookie.reply() {
                blit_cursor(
                    &mut self.target,
                    self.width as i32,
                    self.height as i32,
                    &cursor,
                );
            }
        }

        let capture_done_ns = monotonic_now_ns();

        if !self.first_composite_logged {
            tracing::info!(
                children = tree.children.len(),
                composited,
                skipped_unmapped,
                skipped_bad_match,
                "first per-window composite frame produced"
            );
            self.first_composite_logged = true;
        }

        let stride = (self.width as u32) * 4;
        Ok(FrameSubmission {
            width: self.width as u32,
            height: self.height as u32,
            stride,
            pixels: self.target.clone(),
            dmabuf_fd: None,
            timestamp_us,
            damage_tiles: None,
            capture_done_ns,
        })
    }

    /// `GetImage(root)` — used when no compositor is running.
    fn capture_root(&self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        let reply = self
            .conn
            .get_image(
                ImageFormat::Z_PIXMAP,
                self.root,
                0,
                0,
                self.width,
                self.height,
                !0,
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

/// Pick a strategy by probing COMPOSITE + `_NET_WM_CM_S0`.
///
/// `GHOSTFRAME_FORCE_ROOT_GET_IMAGE=1` is a diagnostic escape hatch:
/// always use the plain GetImage(root) path regardless of compositor
/// presence. Lets operators reproduce exactly what `xwd -root` /
/// `import -window root` do, to verify whether their compositor's
/// rendering is reachable via the legacy path.
fn pick_strategy(conn: &RustConnection, root: u32) -> Strategy {
    if std::env::var("GHOSTFRAME_FORCE_ROOT_GET_IMAGE")
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
    {
        tracing::info!(
            "GHOSTFRAME_FORCE_ROOT_GET_IMAGE set; bypassing compositor probe and \
             using plain GetImage(root) — same call `import -window root` makes"
        );
        return Strategy::RootGetImage;
    }
    let composite_present = match conn.query_extension(b"Composite") {
        Ok(c) => c.reply().map(|r| r.present).unwrap_or(false),
        Err(_) => false,
    };
    if !composite_present {
        tracing::warn!(
            "COMPOSITE extension not present; falling back to GetImage on root. \
             GL/EGL-compositor desktops will appear empty."
        );
        return Strategy::RootGetImage;
    }

    // XFixes is required for cursor capture — not strictly necessary for
    // the composite path itself, but log if absent so operators know
    // why the cursor is missing.
    let xfixes_present = match conn.query_extension(b"XFIXES") {
        Ok(c) => c.reply().map(|r| r.present).unwrap_or(false),
        Err(_) => false,
    };
    if !xfixes_present {
        tracing::warn!(
            "XFIXES extension not present; per-window composite will work but \
             the captured frame won't include the cursor"
        );
    } else {
        // XFixes requires version negotiation before use. We don't
        // care about the reply, only that the server records the
        // negotiated version.
        if let Ok(c) = conn.xfixes_query_version(5, 0) {
            let _ = c.reply();
        }
    }

    // Probe for an active compositor via `_NET_WM_CM_S0` selection
    // ownership (EWMH §"Compositing Manager"). If no client owns it,
    // there's no compositor — root pixmap holds the visible content.
    let cm_owner = match conn.intern_atom(false, b"_NET_WM_CM_S0") {
        Ok(c) => match c.reply() {
            Ok(atom_reply) => match conn.get_selection_owner(atom_reply.atom) {
                Ok(c) => c.reply().map(|r| r.owner).unwrap_or(0),
                Err(_) => 0,
            },
            Err(_) => 0,
        },
        Err(_) => 0,
    };
    if cm_owner == 0 {
        tracing::info!("no compositor owns _NET_WM_CM_S0; capturing root pixmap directly");
        return Strategy::RootGetImage;
    }

    tracing::info!(
        compositor_owner = cm_owner,
        "compositor detected; using per-window NameWindowPixmap + GetImage composite \
         (root={root})"
    );
    Strategy::PerWindowComposite
}

/// Bounds-and-source descriptor passed to `blit_window` — kept as a
/// struct so the function signature stays narrow.
struct BlitRect {
    /// Destination origin within the target framebuffer.
    dst_x: i32,
    dst_y: i32,
    /// Source dimensions; source layout is `src_w * 4` BGRA bytes per row.
    src_w: i32,
    src_h: i32,
}

/// Copy a window's BGRA pixels into the target framebuffer at the
/// destination origin in `rect`, clipping to the framebuffer's bounds.
///
/// Source layout: contiguous BGRA rows, stride = `rect.src_w * 4`. Treats
/// the window as fully opaque (no per-pixel alpha blending) — opacity
/// support is a follow-up.
fn blit_window(target: &mut [u8], target_w: i32, target_h: i32, src: &[u8], rect: BlitRect) {
    let BlitRect {
        dst_x,
        dst_y,
        src_w,
        src_h,
    } = rect;
    // Compute the clipped destination rectangle.
    let x0 = dst_x.max(0);
    let y0 = dst_y.max(0);
    let x1 = (dst_x + src_w).min(target_w);
    let y1 = (dst_y + src_h).min(target_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let src_off_x = (x0 - dst_x) as usize;
    let src_off_y = (y0 - dst_y) as usize;
    let copy_w = (x1 - x0) as usize;
    let copy_h = (y1 - y0) as usize;

    let target_stride = (target_w as usize) * 4;
    let src_stride = (src_w as usize) * 4;

    // Defensive: source buffer must have enough bytes.
    let needed = src_off_y * src_stride + src_off_x * 4 + (copy_h - 1) * src_stride + copy_w * 4;
    if needed > src.len() {
        return;
    }

    for row in 0..copy_h {
        let src_row = (src_off_y + row) * src_stride + src_off_x * 4;
        let dst_row = ((y0 as usize) + row) * target_stride + (x0 as usize) * 4;
        let src_slice = &src[src_row..src_row + copy_w * 4];
        let dst_slice = &mut target[dst_row..dst_row + copy_w * 4];
        // The window pixmap may have depth=24 (X-padded BGRX) — we
        // copy verbatim and let the encoder ignore the alpha channel.
        dst_slice.copy_from_slice(src_slice);
        // Force alpha to 255 in case the pixmap is depth-24 with
        // garbage alpha. The encoder treats this as opaque BGR; we
        // don't want stray alpha values to confuse downstream stages.
        for i in (3..copy_w * 4).step_by(4) {
            dst_slice[i] = 0xff;
        }
    }
}

/// Alpha-blend the cursor onto the target framebuffer.
///
/// `XFixesGetCursorImage` returns 32-bit pre-multiplied ARGB pixels
/// (per spec); on little-endian platforms those bytes are BGRA in
/// memory, with the alpha channel ALREADY pre-multiplied into the
/// color channels. So the standard "over" composite simplifies to:
///
///   out = src + dst * (1 - src.a)
///
/// applied per channel.
fn blit_cursor(target: &mut [u8], target_w: i32, target_h: i32, cursor: &GetCursorImageReply) {
    let cx = (cursor.x as i32) - (cursor.xhot as i32);
    let cy = (cursor.y as i32) - (cursor.yhot as i32);
    let cw = cursor.width as i32;
    let ch = cursor.height as i32;

    let x0 = cx.max(0);
    let y0 = cy.max(0);
    let x1 = (cx + cw).min(target_w);
    let y1 = (cy + ch).min(target_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let src_off_x = (x0 - cx) as usize;
    let src_off_y = (y0 - cy) as usize;
    let copy_w = (x1 - x0) as usize;
    let copy_h = (y1 - y0) as usize;

    let target_stride = (target_w as usize) * 4;

    for row in 0..copy_h {
        for col in 0..copy_w {
            let src_idx = (src_off_y + row) * (cw as usize) + (src_off_x + col);
            if src_idx >= cursor.cursor_image.len() {
                continue;
            }
            let argb = cursor.cursor_image[src_idx];
            // ARGB unpacking; on little-endian X11 reports in
            // host byte order, so bits are A R G B from MSB → LSB.
            let a = (argb >> 24) & 0xff;
            let r = (argb >> 16) & 0xff;
            let g = (argb >> 8) & 0xff;
            let b = argb & 0xff;
            if a == 0 {
                continue; // fully transparent pixel — nothing to do
            }

            let dst_idx = ((y0 as usize) + row) * target_stride + ((x0 as usize) + col) * 4;
            let inv_a = 255 - a;
            // Target is BGRA. Source channels are already pre-multiplied
            // by alpha (XFixes guarantee), so we just add + scale dst.
            let db = target[dst_idx] as u32;
            let dg = target[dst_idx + 1] as u32;
            let dr = target[dst_idx + 2] as u32;
            target[dst_idx] = (b + (db * inv_a + 127) / 255).min(255) as u8;
            target[dst_idx + 1] = (g + (dg * inv_a + 127) / 255).min(255) as u8;
            target[dst_idx + 2] = (r + (dr * inv_a + 127) / 255).min(255) as u8;
            target[dst_idx + 3] = 0xff;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_solid(w: i32, h: i32, color: [u8; 4]) -> Vec<u8> {
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        for px in buf.chunks_exact_mut(4) {
            px.copy_from_slice(&color);
        }
        buf
    }

    #[test]
    fn blit_window_inside_bounds_copies_pixels() {
        let mut target = vec![0u8; 8 * 8 * 4];
        let src = make_solid(2, 2, [10, 20, 30, 0]); // BGRA, alpha will be forced to 255
        blit_window(
            &mut target,
            8,
            8,
            &src,
            BlitRect {
                dst_x: 1,
                dst_y: 1,
                src_w: 2,
                src_h: 2,
            },
        );
        // Pixel (1,1) and (2,2) should be [10,20,30,255]; (0,0) untouched.
        let idx = |x: usize, y: usize| (y * 8 + x) * 4;
        assert_eq!(&target[idx(1, 1)..idx(1, 1) + 4], &[10, 20, 30, 0xff]);
        assert_eq!(&target[idx(2, 2)..idx(2, 2) + 4], &[10, 20, 30, 0xff]);
        assert_eq!(&target[idx(0, 0)..idx(0, 0) + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_window_clips_to_target_left_edge() {
        let mut target = vec![0u8; 4 * 4 * 4];
        let src = make_solid(4, 4, [99, 0, 0, 0]);
        // dst_x = -2: 2 leftmost cols of src are clipped, right 2 land in target cols 0..2
        blit_window(
            &mut target,
            4,
            4,
            &src,
            BlitRect {
                dst_x: -2,
                dst_y: 0,
                src_w: 4,
                src_h: 4,
            },
        );
        // Row 0 col 0 should be src[(0,2)] which is solid color.
        let idx = |x: usize, y: usize| (y * 4 + x) * 4;
        assert_eq!(target[idx(0, 0)], 99);
        // Col 2 is outside the blit (clip stopped at x=1) → untouched 0.
        assert_eq!(target[idx(2, 0)], 0);
        assert_eq!(target[idx(3, 0)], 0);
    }

    #[test]
    fn blit_window_fully_off_screen_is_noop() {
        let mut target = vec![42u8; 4 * 4 * 4];
        let backup = target.clone();
        let src = make_solid(2, 2, [99, 0, 0, 0]);
        blit_window(
            &mut target,
            4,
            4,
            &src,
            BlitRect {
                dst_x: -5,
                dst_y: -5,
                src_w: 2,
                src_h: 2,
            },
        );
        assert_eq!(target, backup);
    }

    #[test]
    fn blit_window_forces_alpha_255_for_depth24_sources() {
        let mut target = vec![0u8; 2 * 2 * 4];
        // Source has alpha=0 (X depth-24 padding); blit must rewrite to 255.
        let src = vec![1, 2, 3, 0, 4, 5, 6, 0, 7, 8, 9, 0, 10, 11, 12, 0];
        blit_window(
            &mut target,
            2,
            2,
            &src,
            BlitRect {
                dst_x: 0,
                dst_y: 0,
                src_w: 2,
                src_h: 2,
            },
        );
        for px in target.chunks_exact(4) {
            assert_eq!(px[3], 0xff);
        }
    }
}
