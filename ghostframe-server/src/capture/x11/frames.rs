//! X11 frames: a full grab of the root window, plus the cursor.
//!
//! One method, the one `x11vnc` and ffmpeg's `x11grab` use: `ShmGetImage`
//! of the whole root into a MIT-SHM segment, falling back to
//! `GetImage(root)` when MIT-SHM is absent or the segment cannot be attached
//! (a remote display cannot share memory, and fd passing fails over TCP).
//!
//! There used to be a second method, per-window `CompositeNameWindowPixmap`
//! capture, chosen once at startup by probing whether the compositor painted
//! into root. On Enlightenment at autologin the probe was inconclusive (no
//! redirected root child of at least 64x64 existed yet), the per-window path
//! was chosen for the life of the process, and it composited nothing: the
//! client saw only the cursor. `GetImage(root)` on the same display returned
//! the full desktop. Root is what every X compositor this server targets
//! paints, so root is what is captured.
//!
//! The root path never drew the cursor (X does not include it in an image).
//! It is blended here from `XFixesGetCursorImage`, at `(x - xhot, y - yhot)`.

use std::collections::HashSet;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use crate::capture::x11::damage::rect_to_tiles;
use crate::util::monotonic_now_ns;
use ghostframe_lib::FrameSubmission;
use x11rb::connection::Connection;
use x11rb::protocol::shm::{self, ConnectionExt as ShmExt};
use x11rb::protocol::xfixes::{ConnectionExt as XfixesExt, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

/// Which request fills a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabPath {
    /// `ShmGetImage` into a shared segment.
    Shm,
    /// `GetImage(root)`: the pixels travel over the X socket.
    GetImage,
}

/// A memfd-backed MIT-SHM segment the X server writes root pixels into.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

/// Why a segment could not be attached.
enum AttachError {
    /// memfd/ftruncate/mmap failed: local and possibly transient (memory
    /// pressure), so the next resize tries again.
    Os(io::Error),
    /// The X server refused or the request could not be sent (BadAccess /
    /// BadMatch on a remote display, fd passing failing over TCP): this
    /// connection will never share memory.
    X(io::Error),
}

impl ShmSegment {
    /// Create a `len`-byte memfd, map it, and attach it to the server
    /// writable (`ShmGetImage` writes into it).
    fn attach(conn: &RustConnection, len: usize) -> Result<Self, AttachError> {
        // SAFETY: memfd_create takes a NUL-terminated name and flags only.
        let raw = unsafe { libc::memfd_create(c"ghostframe-shm".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(AttachError::Os(io::Error::last_os_error()));
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: ftruncate on a descriptor we own.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(AttachError::Os(io::Error::last_os_error()));
        }
        // SAFETY: maps `len` bytes of a memfd just sized to `len`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(AttachError::Os(io::Error::last_os_error()));
        }
        // Built before the attach so an error below still unmaps.
        let mut me = Self {
            seg: 0,
            ptr: ptr.cast(),
            len,
        };
        me.seg = conn
            .generate_id()
            .map_err(|e| AttachError::X(io::Error::other(e)))?;
        // x11rb sends the fd and closes our copy; the mapping keeps the memory.
        conn.shm_attach_fd(me.seg, fd, false)
            .map_err(|e| AttachError::X(io::Error::other(e)))?
            .check()
            .map_err(|e| AttachError::X(io::Error::other(e)))?;
        Ok(me)
    }

    fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps exactly `len` bytes for the life of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
}

impl Drop for ShmSegment {
    fn drop(&mut self) {
        // SAFETY: `ptr`/`len` come from a successful mmap, unmapped once.
        unsafe {
            libc::munmap(self.ptr.cast(), self.len);
        }
    }
}

/// Captures full-root frames from one X display.
pub struct X11Frames {
    conn: RustConnection,
    root: u32,
    width: u16,
    height: u16,
    /// The segment for the current root size; `None` means `GetImage`.
    shm: Option<ShmSegment>,
    /// False once MIT-SHM is known not to work on this connection, so a
    /// resize does not retry (and re-log) a failing attach every time.
    shm_usable: bool,
    /// One-shot raw-frame dump target (`GHOSTFRAME_DUMP_FIRST_FRAME`); taken on first use.
    dump_first_frame: Option<String>,
    /// The cursor as seen by the last `poll_cursor`, consumed by `capture`
    /// so one frame uses one cursor query.
    cursor: Option<GetCursorImageReply>,
    /// Where the cursor was drawn as of the last poll.
    last_cursor: Option<CursorRect>,
}

/// A cursor image's rectangle in root coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CursorRect {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
}

impl CursorRect {
    fn of(c: &GetCursorImageReply) -> Self {
        Self {
            x: i32::from(c.x) - i32::from(c.xhot),
            y: i32::from(c.y) - i32::from(c.yhot),
            w: u32::from(c.width),
            h: u32::from(c.height),
        }
    }
}

/// Tiles to recompare because the cursor moved or changed: those under the
/// previous and the current rectangle. X reports no damage for a pointer
/// move or a cursor-image change, yet the cursor is blended into the frame,
/// so without these a hinted comparison keeps a stale cursor until something
/// else touches the tile. Clipped to the `screen_w` x `screen_h` grid.
fn cursor_dirty_tiles(
    prev: Option<CursorRect>,
    cur: Option<CursorRect>,
    screen_w: u32,
    screen_h: u32,
) -> Vec<(u32, u32)> {
    let mut out = HashSet::new();
    for r in [prev, cur].into_iter().flatten() {
        // Clip to the screen in pixels first: only on-screen tiles exist,
        // and the i16/u16 arguments of `rect_to_tiles` must not wrap.
        let x0 = r.x.max(0) as u32;
        let y0 = r.y.max(0) as u32;
        let x1 = (i64::from(r.x) + i64::from(r.w)).clamp(0, i64::from(screen_w)) as u32;
        let y1 = (i64::from(r.y) + i64::from(r.h)).clamp(0, i64::from(screen_h)) as u32;
        if x0 >= x1 || y0 >= y1 {
            continue;
        }
        rect_to_tiles(
            x0 as i16,
            y0 as i16,
            (x1 - x0) as u16,
            (y1 - y0) as u16,
            &mut out,
        );
    }
    let mut v: Vec<_> = out.into_iter().collect();
    v.sort_unstable();
    v
}

impl X11Frames {
    /// Connect to `display` (`None` = `$DISPLAY`).
    pub fn connect(display: Option<&str>, dump_first_frame: Option<String>) -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(display)
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);
        check_pixel_format(conn.setup(), screen.root_depth)?;

        // XFixes needs its version negotiated before GetCursorImage. Without
        // it the frame is still correct, minus the cursor.
        match conn.xfixes_query_version(5, 0).map(|c| c.reply()) {
            Ok(Ok(_)) => {}
            _ => tracing::warn!("XFIXES unavailable; frames will not include the cursor"),
        }

        let shm_usable = shm_supported(&conn);
        let mut me = Self {
            conn,
            root,
            width,
            height,
            shm: None,
            shm_usable,
            dump_first_frame,
            cursor: None,
            last_cursor: None,
        };
        me.ensure_segment();
        tracing::info!(width, height, path = ?me.path(), "X11 capture connected to display");
        Ok(me)
    }

    /// How frames are grabbed right now.
    pub fn path(&self) -> GrabPath {
        if self.shm.is_some() {
            GrabPath::Shm
        } else {
            GrabPath::GetImage
        }
    }

    /// Make the segment match the root size, or drop to `GetImage` for good
    /// if MIT-SHM cannot be used.
    fn ensure_segment(&mut self) {
        if !self.shm_usable {
            return;
        }
        let len = usize::from(self.width) * usize::from(self.height) * 4;
        if self.shm.as_ref().is_some_and(|s| s.len == len) {
            return;
        }
        if let Some(old) = self.shm.take() {
            // Checked, so a refusal is read here and not left in the event
            // queue; the segment is going away either way.
            if let Err(e) = self
                .conn
                .shm_detach(old.seg)
                .map_err(io::Error::other)
                .and_then(|c| c.check().map_err(io::Error::other))
            {
                tracing::debug!(error = %e, "MIT-SHM detach failed");
            }
        }
        match ShmSegment::attach(&self.conn, len) {
            Ok(s) => self.shm = Some(s),
            Err(AttachError::X(e)) => {
                tracing::warn!(error = %e, "MIT-SHM segment attach refused; using GetImage(root)");
                self.shm_usable = false;
            }
            Err(AttachError::Os(e)) => {
                // Local and possibly transient: GetImage for now, and the
                // next resize tries again.
                tracing::warn!(error = %e, "MIT-SHM segment allocation failed; using GetImage(root) until the next resize");
            }
        }
    }

    /// Poll the root window's current geometry and, if it changed since the
    /// last capture, adopt it and resize the segment.
    ///
    /// Polled once per captured frame rather than driven by
    /// `RRScreenChangeNotify`: one `GetGeometry` round trip next to the grab
    /// is negligible, and unlike an event subscription it cannot miss a
    /// notification or need a second connection to listen on.
    fn refresh_geometry(&mut self) -> io::Result<()> {
        let geom = self
            .conn
            .get_geometry(self.root)
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;
        if let Some((w, h)) = resize_needed((self.width, self.height), (geom.width, geom.height)) {
            tracing::info!(
                old_width = self.width,
                old_height = self.height,
                new_width = w,
                new_height = h,
                "X11 root geometry changed; resizing capture target"
            );
            self.width = w;
            self.height = h;
            self.ensure_segment();
        }
        Ok(())
    }

    /// The root pixels, BGRX, `width * 4` per row, without the cursor.
    fn grab(&mut self) -> io::Result<Vec<u8>> {
        let (w, h) = (self.width, self.height);
        let want = usize::from(w) * usize::from(h) * 4;
        if let Some(seg) = &self.shm {
            let reply = self
                .conn
                .shm_get_image(
                    self.root,
                    0,
                    0,
                    w,
                    h,
                    !0,
                    ImageFormat::Z_PIXMAP.into(),
                    seg.seg,
                    0,
                )
                .map_err(io::Error::other)?
                .reply()
                .map_err(io::Error::other)?;
            if reply.size as usize != want {
                return Err(io::Error::other(format!(
                    "ShmGetImage returned {} bytes for {w}x{h}, expected {want} (root is not 32 bpp)",
                    reply.size
                )));
            }
            return Ok(seg.bytes()[..want].to_vec());
        }
        let reply = self
            .conn
            .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)
            .map_err(io::Error::other)?
            .reply()
            .map_err(io::Error::other)?;
        if reply.data.len() != want {
            return Err(io::Error::other(format!(
                "GetImage returned {} bytes for {w}x{h}, expected {want}",
                reply.data.len()
            )));
        }
        Ok(reply.data)
    }

    fn query_cursor(&self) -> Option<GetCursorImageReply> {
        self.conn
            .xfixes_get_cursor_image()
            .ok()
            .and_then(|c| c.reply().ok())
    }

    /// Query the cursor for the next frame and return the tiles its
    /// movement or image change dirtied (see [`cursor_dirty_tiles`]).
    pub fn poll_cursor(&mut self) -> Vec<(u32, u32)> {
        let cursor = self.query_cursor();
        let cur = cursor.as_ref().map(CursorRect::of);
        let tiles = cursor_dirty_tiles(
            self.last_cursor,
            cur,
            u32::from(self.width),
            u32::from(self.height),
        );
        self.last_cursor = cur;
        self.cursor = cursor;
        tiles
    }

    /// Capture a single frame: root, then the cursor on top.
    pub fn capture(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        self.refresh_geometry()?;
        let mut pixels = self.grab()?;
        // Best effort: without XFixes the frame is correct minus the cursor.
        let cursor = self.cursor.take().or_else(|| self.query_cursor());
        if let Some(cursor) = cursor {
            blit_cursor(
                &mut pixels,
                i32::from(self.width),
                i32::from(self.height),
                &cursor,
            );
        }
        let capture_done_ns = monotonic_now_ns();
        let width = u32::from(self.width);
        let height = u32::from(self.height);
        let frame = FrameSubmission {
            width,
            height,
            stride: width * 4,
            pixels,
            dmabuf_fd: None,
            timestamp_us,
            damage_tiles: None,
            capture_done_ns,
        };
        // One-shot diagnostic: dump the first frame to the path in
        // `GHOSTFRAME_DUMP_FIRST_FRAME`, to byte-compare against `xwd -root`.
        if let Some(path) = self.dump_first_frame.take() {
            let header = format!(
                "ghostframe-bgra width={} height={} stride={}\n",
                frame.width, frame.height, frame.stride
            );
            let _ = std::fs::write(format!("{path}.meta"), header.as_bytes());
            let _ = std::fs::write(&path, &frame.pixels);
            tracing::info!(
                path = %path,
                width = frame.width,
                height = frame.height,
                bytes = frame.pixels.len(),
                "GHOSTFRAME_DUMP_FIRST_FRAME: dumped raw BGRA to disk"
            );
        }
        Ok(frame)
    }
}

/// Refuse a display whose root is not little-endian 32 bits per pixel.
///
/// Everything downstream reads BGRX at `width * 4` per row. A 16 bpp root
/// returns half-size images and used to panic `blit_cursor`; fail at connect
/// with a message instead.
fn check_pixel_format(setup: &x11rb::protocol::xproto::Setup, root_depth: u8) -> io::Result<()> {
    let unsupported = |msg: String| io::Error::new(io::ErrorKind::Unsupported, msg);
    if setup.image_byte_order != ImageOrder::LSB_FIRST {
        return Err(unsupported(
            "X server image byte order is MSB-first; only LSB-first (BGRX) roots are supported"
                .into(),
        ));
    }
    if root_depth != 24 && root_depth != 32 {
        return Err(unsupported(format!(
            "X root depth is {root_depth}; capture needs 24 (or 32). \
             Start the X server with a 24-bit screen"
        )));
    }
    match setup.pixmap_formats.iter().find(|f| f.depth == root_depth) {
        Some(f) if f.bits_per_pixel == 32 => Ok(()),
        Some(f) => Err(unsupported(format!(
            "X root depth {root_depth} is stored at {} bits per pixel; capture needs 32",
            f.bits_per_pixel
        ))),
        None => Err(unsupported(format!(
            "X server lists no pixmap format for root depth {root_depth}"
        ))),
    }
}

/// MIT-SHM 1.2 (`AttachFd`) is present on this connection.
fn shm_supported(conn: &RustConnection) -> bool {
    match conn.shm_query_version().map(|c| c.reply()) {
        Ok(Ok(v)) => (v.major_version, v.minor_version) >= (1, 2),
        _ => false,
    }
}

/// Decide whether the cached capture geometry must be rebuilt.
///
/// Called once per captured frame, so the unchanged case must be free.
/// A zero dimension is ignored rather than honoured: X can report one
/// briefly mid-transition, and resizing to it would yield empty frames.
fn resize_needed(current: (u16, u16), actual: (u16, u16)) -> Option<(u16, u16)> {
    if actual.0 == 0 || actual.1 == 0 || actual == current {
        return None;
    }
    Some(actual)
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

    #[test]
    fn a_changed_root_geometry_requires_a_resize() {
        assert_eq!(resize_needed((640, 480), (1280, 800)), Some((1280, 800)));
    }

    #[test]
    fn an_unchanged_geometry_does_not() {
        // This runs once per captured frame. Reallocating a multi-megabyte
        // buffer every frame because the check was sloppy would be a worse
        // bug than the one being fixed.
        assert_eq!(resize_needed((1280, 800), (1280, 800)), None);
    }

    #[test]
    fn a_zero_geometry_is_ignored() {
        // X can briefly report 0 during a mode transition. Resizing to it
        // would yield empty frames or panic on a zero-sized buffer.
        assert_eq!(resize_needed((1280, 800), (0, 0)), None);
        assert_eq!(resize_needed((1280, 800), (1280, 0)), None);
    }

    /// A 3x2 opaque white cursor with its hotspot at (1, 1), pointer at
    /// (5, 4): the image's top-left lands at (4, 3), nowhere else changes.
    #[test]
    fn blit_cursor_places_the_image_at_pointer_minus_hotspot() {
        let (w, h) = (10i32, 8i32);
        let mut target = vec![0u8; (w * h * 4) as usize];
        let cursor = GetCursorImageReply {
            x: 5,
            y: 4,
            width: 3,
            height: 2,
            xhot: 1,
            yhot: 1,
            cursor_image: vec![0xffff_ffff; 6],
            ..Default::default()
        };
        blit_cursor(&mut target, w, h, &cursor);
        for y in 0..h {
            for x in 0..w {
                let px = &target[((y * w + x) * 4) as usize..][..4];
                let inside = (4..7).contains(&x) && (3..5).contains(&y);
                let want: &[u8] = if inside { &[255; 4] } else { &[0; 4] };
                assert_eq!(px, want, "pixel ({x},{y})");
            }
        }
    }

    fn px_cursor(x: i16, y: i16, w: u16, h: u16, argb: u32) -> GetCursorImageReply {
        GetCursorImageReply {
            x,
            y,
            width: w,
            height: h,
            cursor_image: vec![argb; usize::from(w) * usize::from(h)],
            ..Default::default()
        }
    }

    #[test]
    fn blit_cursor_clips_at_the_top_left_corner() {
        // Pointer at (-1,-1), no hotspot: only the cursor's bottom-right
        // pixel is on screen.
        let mut target = vec![0u8; 4 * 4 * 4];
        blit_cursor(&mut target, 4, 4, &px_cursor(-1, -1, 2, 2, 0xffff_ffff));
        for (i, px) in target.chunks_exact(4).enumerate() {
            let want: &[u8] = if i == 0 { &[255; 4] } else { &[0; 4] };
            assert_eq!(px, want, "pixel index {i}");
        }
    }

    #[test]
    fn blit_cursor_larger_than_the_target_is_clipped_not_overrun() {
        // 2x the target in both dimensions: every target pixel is covered,
        // and nothing writes past the buffer.
        let mut target = vec![0u8; 3 * 2 * 4];
        blit_cursor(&mut target, 3, 2, &px_cursor(0, 0, 6, 4, 0xffff_ffff));
        assert_eq!(target, vec![255u8; 3 * 2 * 4]);
    }

    #[test]
    fn blit_cursor_keeps_colour_channels_in_bgra_order() {
        // Opaque ARGB 0xff_11_22_33: R=0x11, G=0x22, B=0x33; BGRA in memory.
        let mut target = vec![0u8; 4];
        blit_cursor(&mut target, 1, 1, &px_cursor(0, 0, 1, 1, 0xff11_2233));
        assert_eq!(target, [0x33, 0x22, 0x11, 0xff]);
    }

    fn rect(x: i32, y: i32, w: u32, h: u32) -> CursorRect {
        CursorRect { x, y, w, h }
    }

    #[test]
    fn a_moved_cursor_dirties_the_tiles_it_left_and_the_ones_it_entered() {
        // 32 px tiles. Old rect in tile (0,0); new rect in tile (3,2).
        let t = cursor_dirty_tiles(Some(rect(2, 2, 8, 8)), Some(rect(100, 70, 8, 8)), 256, 256);
        assert_eq!(t, vec![(0, 0), (3, 2)]);
    }

    #[test]
    fn a_cursor_straddling_tiles_dirties_all_of_them_and_is_clipped_to_the_screen() {
        let t = cursor_dirty_tiles(None, Some(rect(28, 28, 8, 8)), 256, 256);
        assert_eq!(t, vec![(0, 0), (0, 1), (1, 0), (1, 1)]);
        // Hanging off the top-left and past the bottom-right: only real tiles.
        let t = cursor_dirty_tiles(
            Some(rect(-4, -4, 8, 8)),
            Some(rect(250, 250, 20, 20)),
            256,
            256,
        );
        assert_eq!(t, vec![(0, 0), (7, 7)]);
        // Entirely off screen: nothing.
        assert!(cursor_dirty_tiles(None, Some(rect(-50, 0, 8, 8)), 256, 256).is_empty());
    }

    #[test]
    fn a_resized_cursor_image_dirties_the_old_and_new_extent() {
        // Same hotspot-adjusted origin, bigger image: old rect in one tile,
        // new rect reaching the next.
        let t = cursor_dirty_tiles(Some(rect(0, 0, 8, 8)), Some(rect(0, 0, 40, 8)), 256, 256);
        assert_eq!(t, vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn blit_cursor_blends_premultiplied_alpha() {
        // 50% black over white: out = 0 + 255 * (255 - 128) / 255 = 127.
        let mut target = vec![255u8; 4];
        let cursor = GetCursorImageReply {
            x: 0,
            y: 0,
            width: 1,
            height: 1,
            cursor_image: vec![0x8000_0000],
            ..Default::default()
        };
        blit_cursor(&mut target, 1, 1, &cursor);
        assert_eq!(target, [127, 127, 127, 255]);
    }
}

#[cfg(test)]
mod live_tests {
    //! Against a private Xvfb (`test_support::PrivateXvfb`), so painting
    //! root or disabling MIT-SHM never touches another test or a session.
    use super::*;
    use crate::capture::x11::test_support::PrivateXvfb;
    use x11rb::protocol::xproto::{ChangeWindowAttributesAux, CreateGCAux};

    const W: u16 = 64;
    const H: u16 = 48;

    /// Paint a deterministic, non-uniform image into root.
    fn paint_root(display: &str) {
        paint_root_sized(display, W, H);
    }

    fn paint_root_sized(display: &str, w: u16, h: u16) {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let screen = &conn.setup().roots[n];
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, screen.root, &CreateGCAux::new())
            .unwrap();
        let mut data = vec![0u8; usize::from(w) * usize::from(h) * 4];
        for (i, px) in data.chunks_exact_mut(4).enumerate() {
            let (x, y) = (i % usize::from(w), i / usize::from(w));
            px.copy_from_slice(&[(x * 7) as u8, (y * 5) as u8, (x ^ y) as u8, 0]);
        }
        conn.put_image(
            ImageFormat::Z_PIXMAP,
            screen.root,
            gc,
            w,
            h,
            0,
            0,
            0,
            screen.root_depth,
            &data,
        )
        .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();
    }

    fn root_get_image(display: &str) -> Vec<u8> {
        root_get_image_sized(display, W, H)
    }

    fn root_get_image_sized(display: &str, w: u16, h: u16) -> Vec<u8> {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let s = &conn.setup().roots[n];
        conn.get_image(ImageFormat::Z_PIXMAP, s.root, 0, 0, w, h, !0)
            .unwrap()
            .reply()
            .unwrap()
            .data
    }

    #[test]
    fn shm_grab_is_byte_identical_to_get_image() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        paint_root(x.display());
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        assert_eq!(f.path(), GrabPath::Shm);
        let shm = f.grab().unwrap();
        assert!(shm.iter().any(|&b| b != 0), "root was not painted");
        assert_eq!(shm, root_get_image(x.display()));
    }

    #[test]
    fn falls_back_to_get_image_without_mit_shm() {
        let x = PrivateXvfb::spawn(&["-extension", "MIT-SHM", "-screen", "0", "64x48x24"]);
        paint_root(x.display());
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        assert_eq!(f.path(), GrabPath::GetImage);
        let frame = f.capture(0).unwrap();
        assert_eq!((frame.width, frame.height, frame.stride), (64, 48, 256));
        assert_eq!(f.grab().unwrap(), root_get_image(x.display()));
    }

    /// The live cursor (a glyph from the server's built-in cursor font) is
    /// drawn into the frame, and only inside its rectangle at the hotspot.
    #[test]
    fn capture_draws_the_cursor_at_its_hotspot() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let (conn, n) = x11rb::connect(Some(x.display())).unwrap();
        let root = conn.setup().roots[n].root;
        let font = conn.generate_id().unwrap();
        conn.open_font(font, b"cursor").unwrap();
        let cursor = conn.generate_id().unwrap();
        // XC_crosshair (34) with its mask (35): white on black.
        conn.create_glyph_cursor(cursor, font, font, 34, 35, 0, 0, 0, 0xffff, 0xffff, 0xffff)
            .unwrap();
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().cursor(cursor))
            .unwrap();
        conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 30, 20)
            .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();

        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        let plain = f.grab().unwrap();
        let frame = f.capture(0).unwrap();
        let cur = f.conn.xfixes_get_cursor_image().unwrap().reply().unwrap();
        let (cx, cy) = (
            i32::from(cur.x) - i32::from(cur.xhot),
            i32::from(cur.y) - i32::from(cur.yhot),
        );
        let mut changed = 0;
        for (i, (a, b)) in plain
            .chunks_exact(4)
            .zip(frame.pixels.chunks_exact(4))
            .enumerate()
        {
            if a[..3] != b[..3] {
                changed += 1;
                let (px, py) = ((i % 64) as i32, (i / 64) as i32);
                assert!(
                    (cx..cx + i32::from(cur.width)).contains(&px)
                        && (cy..cy + i32::from(cur.height)).contains(&py),
                    "pixel ({px},{py}) changed outside the cursor at ({cx},{cy})"
                );
            }
        }
        assert!(changed > 0, "the cursor was not drawn");
    }

    /// Resize the root of `display` the way `xrandr --fb` does: disable the
    /// CRTC (it would not fit), then set the screen size.
    fn set_root_size(display: &str, w: u16, h: u16) {
        use x11rb::protocol::randr::{self, ConnectionExt as _};
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[n].root;
        let res = conn
            .randr_get_screen_resources(root)
            .unwrap()
            .reply()
            .unwrap();
        for crtc in &res.crtcs {
            conn.randr_set_crtc_config(
                *crtc,
                x11rb::CURRENT_TIME,
                res.config_timestamp,
                0,
                0,
                0,
                randr::Rotation::ROTATE0,
                &[],
            )
            .unwrap()
            .reply()
            .unwrap();
        }
        conn.randr_set_screen_size(root, w, h, 10, 10)
            .unwrap()
            .check()
            .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();
    }

    /// The segment must follow the root size across shrinks and growth.
    /// Connecting after a shrink makes the segment smaller than the root's
    /// later sizes, so a segment that is not re-created fails the grab.
    #[test]
    fn the_segment_follows_a_shrinking_and_growing_root() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        set_root_size(x.display(), 32, 24);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        assert_eq!(f.path(), GrabPath::Shm);

        for (w, h) in [(64u16, 48u16), (32, 24), (64, 48)] {
            set_root_size(x.display(), w, h);
            paint_root_sized(x.display(), w, h);
            let frame = f.capture(0).unwrap();
            assert_eq!((frame.width, frame.height), (u32::from(w), u32::from(h)));
            assert_eq!(frame.pixels.len(), usize::from(w) * usize::from(h) * 4);
            assert_eq!(f.path(), GrabPath::Shm, "after resizing to {w}x{h}");
            let plain = f.grab().unwrap();
            assert!(plain.iter().any(|&b| b != 0), "root not painted at {w}x{h}");
            assert_eq!(plain, root_get_image_sized(x.display(), w, h), "{w}x{h}");
        }
    }

    #[test]
    fn a_16_bit_root_is_refused_at_connect() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x16"]);
        let err = X11Frames::connect(Some(x.display()), None)
            .err()
            .expect("a 16 bpp root must not connect");
        assert_eq!(err.kind(), io::ErrorKind::Unsupported);
        assert!(err.to_string().contains("root depth is 16"), "{err}");
    }

    /// A pointer warp raises no damage event; `poll_cursor` is the only
    /// thing that reports the tiles it left and entered.
    #[test]
    fn poll_cursor_reports_the_tiles_of_a_warped_pointer() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "128x96x24"]);
        let (conn, n) = x11rb::connect(Some(x.display())).unwrap();
        let root = conn.setup().roots[n].root;
        let warp = |px: i16, py: i16| {
            conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, px, py)
                .unwrap();
            conn.get_input_focus().unwrap().reply().unwrap();
        };
        let tile_of = |px: i16, py: i16| ((px as u32) / 32, (py as u32) / 32);
        warp(10, 10);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        f.poll_cursor();
        warp(100, 70);
        let tiles = f.poll_cursor();
        assert!(
            tiles.contains(&tile_of(10, 10)),
            "left tile missing: {tiles:?}"
        );
        assert!(
            tiles.contains(&tile_of(100, 70)),
            "entered tile missing: {tiles:?}"
        );
    }
}
