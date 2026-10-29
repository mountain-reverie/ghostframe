//! `--x11-sink`: the patterns render into a BGRX buffer the size of the
//! root window, and this draws it into root.
//!
//! It replaces the DRM-master paint path (`--drm-direct`), which existed only
//! because Xorg on VKMS swallowed X drawing while it held DRM master and the
//! server captured from DRM. The server now captures over X11, so the pattern
//! goes where the capture reads it: the root window of `$DISPLAY`.
//!
//! Pixel format is unchanged from the DRM path: XRGB8888 little-endian,
//! `[B, G, R, X]` per pixel, `pitch = width * 4`. The pad byte is ignored by
//! a depth-24 root.
//!
//! The buffer is a MIT-SHM segment drawn with `ShmPutImage` when the server
//! offers MIT-SHM 1.2; otherwise a heap buffer drawn with `PutImage` in row
//! bands no larger than the server's maximum request (a 4096x2160 frame is
//! 35 MB, over the BIG-REQUESTS limit).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, Gcontext, ImageFormat, Window};
use x11rb::rust_connection::RustConnection;

/// A memfd-backed MIT-SHM segment the X server reads frames from.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
}

impl ShmSegment {
    fn attach(conn: &RustConnection, len: usize) -> io::Result<Self> {
        // SAFETY: memfd_create takes a NUL-terminated name and flags only.
        let raw = unsafe { libc::memfd_create(c"ghostframe-sink".as_ptr(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a fresh descriptor nothing else owns.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: ftruncate on a descriptor we own.
        if unsafe { libc::ftruncate(fd.as_raw_fd(), len as libc::off_t) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: maps `len` bytes of a memfd just sized to `len`.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd.as_raw_fd(),
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        // Built before the attach so an error below still unmaps.
        let mut me = Self {
            seg: 0,
            ptr: ptr.cast(),
            len,
        };
        me.seg = conn.generate_id().map_err(io::Error::other)?;
        // read_only: the server only reads what we draw.
        conn.shm_attach_fd(me.seg, fd, true)
            .map_err(io::Error::other)?
            .check()
            .map_err(io::Error::other)?;
        Ok(me)
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` maps exactly `len` bytes for the life of `self`, and
        // `&mut self` makes this the only live view.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
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

/// The root window of `$DISPLAY`, as a paintable framebuffer.
pub struct Scanout {
    conn: RustConnection,
    root: Window,
    gc: Gcontext,
    depth: u8,
    shm: Option<ShmSegment>,
    buf: Vec<u8>,
    /// Bytes per row of the buffer (`mode_w * 4`).
    pub pitch: u32,
    /// Root width in pixels.
    pub mode_w: u32,
    /// Root height in pixels.
    pub mode_h: u32,
}

/// A writable view of the frame. Dropping it draws the frame into root.
pub struct Frame<'a> {
    scanout: &'a mut Scanout,
}

impl AsMut<[u8]> for Frame<'_> {
    fn as_mut(&mut self) -> &mut [u8] {
        self.scanout.pixels_mut()
    }
}

impl Drop for Frame<'_> {
    fn drop(&mut self) {
        // A test fixture that cannot draw must fail loudly: a silently stale
        // screen reads as a capture or codec bug in the e2e that uses it.
        if let Err(e) = self.scanout.present() {
            panic!("x11_sink: drawing the frame into root failed: {e}");
        }
    }
}

impl Scanout {
    /// Connect to `display` (`None` = `$DISPLAY`) and size the buffer to root.
    pub fn connect(display: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let screen = &conn.setup().roots[screen_num];
        let (root, depth) = (screen.root, screen.root_depth);
        let (w, h) = (
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        );
        let gc = conn.generate_id()?;
        conn.create_gc(gc, root, &CreateGCAux::new())?.check()?;
        let len = w as usize * h as usize * 4;
        let shm = if shm_1_2(&conn) {
            match ShmSegment::attach(&conn, len) {
                Ok(s) => Some(s),
                Err(e) => {
                    eprintln!("x11_sink: MIT-SHM attach failed ({e}); using PutImage");
                    None
                }
            }
        } else {
            None
        };
        let buf = if shm.is_some() {
            Vec::new()
        } else {
            vec![0u8; len]
        };
        eprintln!(
            "x11_sink: root {w}x{h} depth {depth}, drawing with {}",
            if shm.is_some() {
                "ShmPutImage"
            } else {
                "PutImage"
            }
        );
        Ok(Self {
            conn,
            root,
            gc,
            depth,
            shm,
            buf,
            pitch: w * 4,
            mode_w: w,
            mode_h: h,
        })
    }

    /// Whether frames go through MIT-SHM.
    pub fn uses_shm(&self) -> bool {
        self.shm.is_some()
    }

    /// The frame to paint; it is drawn into root when the view drops.
    pub fn map(&mut self) -> Frame<'_> {
        Frame { scanout: self }
    }

    fn pixels_mut(&mut self) -> &mut [u8] {
        match &mut self.shm {
            Some(s) => s.bytes_mut(),
            None => &mut self.buf,
        }
    }

    /// Draw the current buffer into root and wait until the server has read
    /// it, so the caller may overwrite the buffer at once.
    pub fn present(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let (w, h) = (self.mode_w as u16, self.mode_h as u16);
        if let Some(s) = &self.shm {
            self.conn
                .shm_put_image(
                    self.root,
                    self.gc,
                    w,
                    h,
                    0,
                    0,
                    w,
                    h,
                    0,
                    0,
                    self.depth,
                    ImageFormat::Z_PIXMAP.into(),
                    false,
                    s.seg,
                    0,
                )?
                .check()?;
        } else {
            let row = self.pitch as usize;
            let max_rows = (self.conn.maximum_request_bytes().saturating_sub(64) / row).max(1);
            let mut y = 0usize;
            while y < usize::from(h) {
                let rows = max_rows.min(usize::from(h) - y);
                self.conn.put_image(
                    ImageFormat::Z_PIXMAP,
                    self.root,
                    self.gc,
                    w,
                    rows as u16,
                    0,
                    y as i16,
                    0,
                    self.depth,
                    &self.buf[y * row..(y + rows) * row],
                )?;
                y += rows;
            }
        }
        // A round trip: once it returns, the server has consumed the buffer.
        self.conn.get_input_focus()?.reply()?;
        Ok(())
    }
}

/// MIT-SHM 1.2 (fd passing) is present.
fn shm_1_2(conn: &RustConnection) -> bool {
    conn.shm_query_version()
        .ok()
        .and_then(|c| c.reply().ok())
        .is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::process::{Child, Command, Stdio};

    /// A private Xvfb, so a test can choose the screen size and extensions.
    struct PrivateXvfb {
        child: Child,
        display: String,
    }

    impl PrivateXvfb {
        fn spawn(args: &[&str]) -> Self {
            let mut child = Command::new("Xvfb")
                .args(["-displayfd", "1", "-nolisten", "tcp", "-noreset"])
                .args(args)
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn Xvfb");
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("piped stdout"))
                .read_line(&mut line)
                .expect("read Xvfb display number");
            let n: u32 = line.trim().parse().expect("Xvfb display number");
            Self {
                child,
                display: format!(":{n}"),
            }
        }
    }

    impl Drop for PrivateXvfb {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn pixel(x: u32, y: u32) -> [u8; 3] {
        [(x * 3) as u8, (y * 3) as u8, ((x + y) * 2) as u8]
    }

    /// Paint a gradient through the sink and read root back with GetImage.
    fn round_trip(args: &[&str], want_shm: bool) {
        let x = PrivateXvfb::spawn(args);
        let mut s = Scanout::connect(Some(&x.display)).unwrap();
        assert_eq!(s.uses_shm(), want_shm);
        let (w, h) = (s.mode_w, s.mode_h);
        {
            let mut f = s.map();
            for (i, px) in f.as_mut().chunks_exact_mut(4).enumerate() {
                let (x, y) = (i as u32 % w, i as u32 / w);
                let [b, g, r] = pixel(x, y);
                px.copy_from_slice(&[b, g, r, 0]);
            }
        }
        let (conn, n) = x11rb::connect(Some(&x.display)).unwrap();
        let root = conn.setup().roots[n].root;
        let got = conn
            .get_image(ImageFormat::Z_PIXMAP, root, 0, 0, w as u16, h as u16, !0)
            .unwrap()
            .reply()
            .unwrap()
            .data;
        for (i, px) in got.chunks_exact(4).enumerate() {
            let (x, y) = (i as u32 % w, i as u32 / w);
            assert_eq!(px[..3], pixel(x, y), "pixel ({x},{y})");
        }
    }

    #[test]
    fn shm_frames_land_in_root() {
        round_trip(&["-screen", "0", "320x200x24"], true);
    }

    /// 4096x2160x4 is 35 MB: over the request limit, so this also proves the
    /// banding.
    #[test]
    fn put_image_frames_land_in_root_without_mit_shm() {
        round_trip(
            &["-extension", "MIT-SHM", "-screen", "0", "4096x2160x24"],
            false,
        );
    }
}
