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

use std::cell::Cell;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use super::bands::{self, Grab};
use crate::capture::safety_net::Slice;
use crate::capture::{Damage, TileBitmap};
use crate::util::monotonic_now_ns;
use ghostframe_lib::{FrameBuf, FramePool, FrameStorage, FrameSubmission, PoolSlot};
use x11rb::connection::Connection;
use x11rb::cookie::Cookie;
use x11rb::protocol::shm::{self, ConnectionExt as ShmExt};
use x11rb::protocol::xfixes::{ConnectionExt as XfixesExt, GetCursorImageReply};
use x11rb::protocol::xproto::{ConnectionExt, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

/// Frames to capture with GetImage after a local MIT-SHM allocation error
/// before the retry; the retry runs on the frame after these.
const SHM_RETRY_FRAMES: u32 = 300;

/// Frame slots: 2 queued in the submit channel + 1 in the bridge + 1 being
/// grabbed (design: damage-driven-encoding-design.md, section 3.1).
pub const POOL_SLOTS: usize = 4;

/// Count a pending retry down by one frame; true when it is due now.
fn retry_due(counter: &mut Option<u32>) -> bool {
    match counter {
        Some(0) => {
            *counter = None;
            true
        }
        Some(n) => {
            *n -= 1;
            false
        }
        None => false,
    }
}

/// Which request fills a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrabPath {
    /// `ShmGetImage` into a shared segment.
    Shm,
    /// `GetImage(root)`: the pixels travel over the X socket.
    GetImage,
}

/// A memfd-backed MIT-SHM segment the X server writes root pixels into. It is
/// also a frame slot's storage: the frame handed to the library is this very
/// memory, so the grab involves no copy in this process.
struct ShmSegment {
    seg: shm::Seg,
    ptr: *mut u8,
    len: usize,
    /// The grab tick whose content this segment holds, or `None` when it is
    /// fresh or a grab into it did not finish: the segment's age, for the
    /// band grab. Lives here because the segment is the slot's memory and
    /// follows it through the pool.
    held: Cell<Option<u64>>,
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
                libc::PROT_READ | libc::PROT_WRITE,
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
            held: Cell::new(None),
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
}

// SAFETY: the mapping is plain memory owned by this value; the X server's
// writes happen only inside a `ShmGetImage` the owner is waiting on.
unsafe impl Send for ShmSegment {}

impl FrameStorage for ShmSegment {
    fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` maps exactly `len` bytes for the life of `self`.
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }
    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` is the only handle in this
        // process. The X server also maps the memory, but it writes only
        // during a `ShmGetImage` naming this segment id, and only the holder
        // of the slot that owns this segment issues one.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
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
    /// Byte length of a root frame while the SHM path is active; `None`
    /// means `GetImage`. The segments themselves live in `pool`'s slots.
    shm_len: Option<usize>,
    /// The frame slots (SHM segments on the SHM path).
    pool: FramePool,
    /// False once MIT-SHM is known not to work on this connection, so a
    /// resize does not retry (and re-log) a failing attach every time.
    shm_usable: bool,
    /// One-shot raw-frame dump target (`GHOSTFRAME_DUMP_FIRST_FRAME`); taken on first use.
    dump_first_frame: Option<String>,
    /// The outcome of the last `poll_cursor`, consumed by `capture` so one
    /// frame uses one cursor query. The outer `None` means "no poll since
    /// the last capture"; `Some(None)` means a poll whose query failed, which
    /// `capture` must not retry.
    polled_cursor: Option<Option<GetCursorImageReply>>,
    /// Where the cursor was last DRAWN into a frame (set by `capture`, so a
    /// failed capture does not advance it).
    last_cursor: Option<CursorRect>,
    /// Serial of the cursor image drawn with `last_cursor`: the same rectangle
    /// with a new serial is a new image and must be redrawn.
    last_cursor_serial: Option<u32>,
    /// Frames left until MIT-SHM is retried after a local allocation error.
    shm_retry_in: Option<u32>,
    /// The last MIT-SHM allocation failed locally and has not succeeded since;
    /// set so that a persistent failure warns once, not on every retry.
    shm_alloc_failing: bool,
    /// Copies and allocations since the last `take_stats` (PR 0 counters).
    stats: crate::capture::tick_stats::TickStats,
    /// What the next `capture` may take from the damage (`on` mode); `None`
    /// means a full grab.
    plan: Option<Plan>,
    /// The planned tick's damage, reused every tick.
    plan_mask: TileBitmap,
    /// Grab ticks so far; a segment's `held` is one of these.
    tick: u64,
    /// The grab set of each of the last [`AGE_RING`] ticks, indexed by
    /// `tick % AGE_RING`: damage, sweep slice and (for an `Unknown` or
    /// distrusted tick) everything. A slot last made current at tick `s` is
    /// refreshed with the union of ticks `s+1..=t` (buffer age, design 3.1).
    ring: Vec<AgeEntry>,
    /// Slots last made current before this tick have unknown age: set when
    /// the ring is cleared (resize, failed grab, a tick without a plan).
    age_floor: u64,
    /// Scratch for the union, reused every tick.
    union: TileBitmap,
    /// What the last `capture` grabbed, for the tests.
    #[cfg(test)]
    pub(crate) last_grab: Option<Grab>,
    /// Test-only faults: see [`Mutation`].
    #[cfg(test)]
    pub(crate) mutation: Mutation,
    /// Test-only fault: the next band grab with two or more bands fails after
    /// its first band has been written.
    #[cfg(test)]
    pub(crate) fail_second_band_once: bool,
}

/// Ticks of grab set kept. A slot idle for more than this many ticks has
/// unknown age and gets a full grab.
const AGE_RING: usize = 8;

#[derive(Debug, Clone)]
struct AgeEntry {
    /// The tick this entry describes; 0 when empty (ticks start at 1).
    tick: u64,
    mask: TileBitmap,
}

/// What a consumer of damage tells the next grab.
#[derive(Debug, Clone)]
struct Plan {
    /// Whether `plan_mask` holds this tick's damage (cursor tiles included).
    /// False when it cannot be believed: unknown, a stale size, distrusted.
    known: bool,
    slice: Option<Slice>,
}

/// Faults the band-grab tests inject to prove each rule load-bearing.
#[cfg(test)]
#[allow(dead_code, reason = "the variants are constructed by band_tests")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Mutation {
    #[default]
    None,
    /// Refresh only this tick's grab set, not the union over the slot's age.
    DropAgeUnion,
    /// Leave the cursor's previous tiles out of the damage.
    DropOldCursorTiles,
    /// Do not request the last band.
    SkipBand,
    /// Leave the tiles under the cursor about to be blended out of the grab.
    DropBlendCursorTiles,
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
    let mut out = TileBitmap::new(screen_w, screen_h);
    for r in [prev, cur].into_iter().flatten() {
        cursor_tiles_into(&mut out, r);
    }
    let mut v: Vec<_> = out.iter().collect();
    v.sort_unstable();
    v
}

/// Mark the tiles under a cursor rectangle, clipped to the bitmap's screen.
fn cursor_tiles_into(out: &mut TileBitmap, r: CursorRect) {
    let (screen_w, screen_h) = out.px();
    // Clip to the screen in pixels first: the i16/u16 arguments of
    // `set_rect` must not wrap.
    let x0 = r.x.max(0) as u32;
    let y0 = r.y.max(0) as u32;
    let x1 = (i64::from(r.x) + i64::from(r.w)).clamp(0, i64::from(screen_w)) as u32;
    let y1 = (i64::from(r.y) + i64::from(r.h)).clamp(0, i64::from(screen_h)) as u32;
    if x0 < x1 && y0 < y1 {
        out.set_rect(x0 as i16, y0 as i16, (x1 - x0) as u16, (y1 - y0) as u16);
    }
}

impl X11Frames {
    /// Connect to `display` (`None` = `$DISPLAY`).
    pub fn connect(display: Option<&str>, dump_first_frame: Option<String>) -> io::Result<Self> {
        let (conn, screen_num) = x11rb::connect(display)
            .map_err(|e| io::Error::new(io::ErrorKind::ConnectionRefused, e))?;
        let screen = &conn.setup().roots[screen_num];
        let root = screen.root;
        let (width, height) = (screen.width_in_pixels, screen.height_in_pixels);
        check_pixel_format(conn.setup(), screen)?;

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
            shm_len: None,
            pool: FramePool::new(POOL_SLOTS),
            shm_usable,
            dump_first_frame,
            polled_cursor: None,
            last_cursor: None,
            last_cursor_serial: None,
            shm_retry_in: None,
            shm_alloc_failing: false,
            stats: Default::default(),
            plan: None,
            plan_mask: TileBitmap::empty(),
            tick: 0,
            ring: Vec::new(),
            age_floor: 0,
            union: TileBitmap::empty(),
            #[cfg(test)]
            last_grab: None,
            #[cfg(test)]
            mutation: Mutation::None,
            #[cfg(test)]
            fail_second_band_once: false,
        };
        me.ensure_segment();
        tracing::info!(width, height, path = ?me.path(), "X11 capture connected to display");
        Ok(me)
    }

    /// How frames are grabbed right now.
    pub fn path(&self) -> GrabPath {
        if self.shm_len.is_some() {
            GrabPath::Shm
        } else {
            GrabPath::GetImage
        }
    }

    /// Detach a segment from the X server. Checked, so a refusal is read
    /// here and not left in the event queue; the segment is going away
    /// either way.
    fn detach(&self, seg: &ShmSegment) {
        if let Err(e) = self
            .conn
            .shm_detach(seg.seg)
            .map_err(io::Error::other)
            .and_then(|c| c.check().map_err(io::Error::other))
        {
            tracing::debug!(error = %e, "MIT-SHM detach failed");
        }
    }

    /// Detach and drop every free segment that does not fit the current SHM
    /// size (all of them once the SHM path is off). Without this a LIFO free
    /// list would keep old-size segments, and their memory, until popped.
    fn release_stale(&mut self) {
        let want = self.shm_len;
        let stale = self.pool.take_free_where(|s| {
            s.as_any()
                .downcast_ref::<ShmSegment>()
                .is_none_or(|seg| Some(seg.len) != want)
        });
        for old in stale {
            if let Ok(seg) = old.into_any().downcast::<ShmSegment>() {
                self.detach(&seg);
            }
        }
    }

    /// Record why a segment could not be attached.
    fn attach_failed(&mut self, err: AttachError) {
        self.shm_len = None;
        self.release_stale();
        match err {
            AttachError::X(e) => {
                tracing::warn!(error = %e, "MIT-SHM segment attach refused; using GetImage(root)");
                self.shm_usable = false;
            }
            AttachError::Os(e) => {
                // Local and possibly transient: GetImage for now, retried at
                // the next resize or after SHM_RETRY_FRAMES frames. Only the
                // first failure is news; a persistent one would otherwise warn
                // on every retry.
                if self.shm_alloc_failing {
                    tracing::debug!(error = %e, "MIT-SHM segment allocation failed again; using GetImage(root)");
                } else {
                    tracing::warn!(error = %e, "MIT-SHM segment allocation failed; using GetImage(root) for now");
                }
                self.shm_alloc_failing = true;
                self.shm_retry_in = Some(SHM_RETRY_FRAMES);
            }
        }
    }

    /// Make the SHM path match the root size, or drop to `GetImage` for good
    /// if MIT-SHM cannot be used. Proves an attach works at the new size and
    /// leaves that segment in a free slot; the other slots are filled (and
    /// stale ones replaced) lazily by `grab`.
    fn ensure_segment(&mut self) {
        self.shm_retry_in = None;
        if !self.shm_usable {
            return;
        }
        let len = usize::from(self.width) * usize::from(self.height) * 4;
        if self.shm_len == Some(len) {
            return;
        }
        match ShmSegment::attach(&self.conn, len) {
            Ok(seg) => {
                self.stats.alloc();
                self.shm_len = Some(len);
                self.shm_alloc_failing = false;
                self.release_stale();
                match self.pool.try_acquire() {
                    Some(mut slot) => {
                        self.discard_storage(&mut slot);
                        slot.set_storage(Box::new(seg));
                    }
                    // Every slot is out with a frame; the probe has done its job.
                    None => self.detach(&seg),
                }
            }
            Err(e) => self.attach_failed(e),
        }
    }

    /// Remove and release the slot's storage, detaching it if it is a segment.
    fn discard_storage(&mut self, slot: &mut PoolSlot) {
        if let Some(old) = slot.take_storage() {
            if let Ok(seg) = old.into_any().downcast::<ShmSegment>() {
                self.detach(&seg);
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
            self.ring.clear();
            // Whatever the slots hold is the old geometry, even when the byte
            // size is the same (1920x1080 <-> 1080x1920) and the segments stay.
            self.age_floor = self.tick + 1;
            self.ensure_segment();
        }
        Ok(())
    }

    /// The root pixels, BGRX, `width * 4` per row, without the cursor, in
    /// `slot`. On the SHM path the X server writes them straight into the
    /// slot's segment: nothing is allocated or copied here once the slots are
    /// warm. The `GetImage` path takes the reply's buffer as the frame (the
    /// slot goes back unused).
    fn grab(&mut self, mut slot: PoolSlot, mut grab: Grab) -> io::Result<FrameBuf> {
        let (w, h) = (self.width, self.height);
        let want = usize::from(w) * usize::from(h) * 4;
        if self.shm_len == Some(want) {
            let fits = slot
                .storage()
                .and_then(|s| s.as_any().downcast_ref::<ShmSegment>())
                .is_some_and(|s| s.len == want);
            if !fits {
                // A never-used slot, or one that came back from before a resize:
                // nothing in it to refresh, so everything is grabbed.
                grab = Grab::Full;
                self.discard_storage(&mut slot);
                self.release_stale();
                match ShmSegment::attach(&self.conn, want) {
                    Ok(seg) => {
                        self.stats.alloc();
                        slot.set_storage(Box::new(seg));
                    }
                    Err(e) => {
                        self.attach_failed(e);
                        return self.grab_get_image(w, h, want);
                    }
                }
            }
            let (seg_id, held) = slot
                .storage()
                .and_then(|s| s.as_any().downcast_ref::<ShmSegment>())
                .map(|s| (s.seg, &s.held))
                .expect("storage was just installed");
            // Not current until every request below has been answered.
            held.set(None);
            let full = [bands::Band {
                y0: 0,
                y1: u32::from(h),
            }];
            let wanted: &[bands::Band] = match &grab {
                Grab::Full => &full,
                Grab::Bands(b) => b.as_slice(),
            };
            #[cfg(test)]
            let wanted = match (self.mutation, wanted) {
                (Mutation::SkipBand, [rest @ .., _]) if matches!(grab, Grab::Bands(_)) => rest,
                _ => wanted,
            };
            let stride = usize::from(w) * 4;
            // Pipelined: every request goes out before the first reply is
            // read, so a tick costs one round trip however many bands.
            let mut cookies: [Option<Cookie<'_, RustConnection, shm::GetImageReply>>;
                bands::MAX_BANDS] = Default::default();
            for (b, c) in wanted.iter().zip(cookies.iter_mut()) {
                *c = Some(
                    self.conn
                        .shm_get_image(
                            self.root,
                            0,
                            b.y0 as i16,
                            w,
                            b.rows() as u16,
                            !0,
                            ImageFormat::Z_PIXMAP.into(),
                            seg_id,
                            b.y0 * stride as u32,
                        )
                        .map_err(io::Error::other)?,
                );
            }
            let mut grabbed = 0usize;
            for (i, (b, c)) in wanted.iter().zip(cookies).enumerate() {
                #[cfg(test)]
                if i == 1 && std::mem::take(&mut self.fail_second_band_once) {
                    return Err(io::Error::other("injected band failure"));
                }
                let _ = i;
                let reply = c
                    .expect("a cookie per band")
                    .reply()
                    .map_err(io::Error::other)?;
                let want_b = b.rows() as usize * stride;
                if reply.size as usize != want_b {
                    return Err(io::Error::other(format!(
                        "ShmGetImage returned {} bytes for {w}x{} rows, expected {want_b} (root is not 32 bpp)",
                        reply.size,
                        b.rows()
                    )));
                }
                grabbed += want_b;
            }
            self.stats.bytes_grabbed += grabbed as u64;
            self.stats.grab_requests += wanted.len() as u32;
            held.set(Some(self.tick));
            #[cfg(test)]
            {
                self.last_grab = Some(grab);
            }
            return slot
                .into_buf()
                .map_err(|_| io::Error::other("frame slot lost its storage"));
        }
        drop(slot);
        self.grab_get_image(w, h, want)
    }

    fn grab_get_image(&mut self, w: u16, h: u16, want: usize) -> io::Result<FrameBuf> {
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
        // x11rb allocated and filled this reply buffer; it becomes the frame.
        self.stats.alloc_copy(want);
        self.stats.bytes_grabbed += want as u64;
        self.stats.grab_requests += 1;
        Ok(FrameBuf::from(reply.data))
    }

    /// The pool the frames come from, for a producer loop that waits on it.
    pub fn pool(&self) -> FramePool {
        self.pool.clone()
    }

    fn query_cursor(&mut self) -> Option<GetCursorImageReply> {
        let reply = self
            .conn
            .xfixes_get_cursor_image()
            .ok()
            .and_then(|c| c.reply().ok());
        if let Some(r) = &reply {
            self.stats.alloc_copy(r.cursor_image.len() * 4);
        }
        reply
    }

    /// Copies and allocations since the last call; resets them.
    pub fn take_stats(&mut self) -> crate::capture::tick_stats::TickStats {
        std::mem::take(&mut self.stats)
    }

    /// Root size in pixels as of the last capture (or connect).
    pub fn size(&self) -> (u32, u32) {
        (u32::from(self.width), u32::from(self.height))
    }

    /// Query the cursor for the next frame and return the tiles its
    /// movement or image change dirtied (see [`cursor_dirty_tiles`]).
    ///
    /// Diffs against where the cursor was last drawn, not last polled.
    pub fn poll_cursor(&mut self) -> Vec<(u32, u32)> {
        let cursor = self.query_cursor();
        let cur = cursor.as_ref().map(CursorRect::of);
        // Same rectangle and same image as the one already drawn: nothing to
        // redraw, so an idle desktop with a visible cursor reports no damage.
        let unchanged = cur == self.last_cursor
            && cursor.as_ref().map(|c| c.cursor_serial) == self.last_cursor_serial;
        #[cfg(test)]
        let prev = if self.mutation == Mutation::DropOldCursorTiles {
            None
        } else {
            self.last_cursor
        };
        #[cfg(not(test))]
        let prev = self.last_cursor;
        let tiles = if unchanged {
            Vec::new()
        } else {
            cursor_dirty_tiles(prev, cur, u32::from(self.width), u32::from(self.height))
        };
        self.polled_cursor = Some(cursor);
        tiles
    }

    /// Tell the next [`X11Frames::capture`] what the damage says (`on` mode).
    ///
    /// `damage` is the drain's report with the cursor's tiles merged in;
    /// `slice` the sweep slice due this tick; `trusted` false when hints must
    /// not be believed. Without a call a capture grabs the whole root, so a
    /// caller that does not consume damage changes nothing.
    pub fn plan_grab(&mut self, damage: &Damage, slice: Option<Slice>, trusted: bool) {
        let (w, h) = (u32::from(self.width), u32::from(self.height));
        if !self.plan_mask.matches(w, h) {
            self.plan_mask = TileBitmap::new(w, h);
        }
        let known = trusted
            && match damage {
                Damage::Tiles(b) if b.matches(w, h) => {
                    self.plan_mask.copy_from(b);
                    true
                }
                Damage::None => {
                    self.plan_mask.clear();
                    true
                }
                Damage::Tiles(_) | Damage::Unknown => false,
            };
        self.plan = Some(Plan { known, slice });
    }

    /// Decide what this tick grabs into a slot last made current at tick
    /// `held`, and file the tick's grab set in the age ring.
    ///
    /// The set for the slot is the union of the grab sets of every tick since
    /// it was last current (buffer age, design 3.1), plus the tiles under the
    /// cursor about to be blended. A slot of unknown or too old an age, an
    /// unbelievable tick, or a union above [`bands::FULL_GRAB_ABOVE_PCT`] of
    /// the rows gets the full grab.
    fn choose(&mut self, held: Option<u64>, cursor: Option<CursorRect>) -> Grab {
        self.tick += 1;
        let t = self.tick;
        let Some(plan) = self.plan.take() else {
            self.ring.clear();
            self.age_floor = t;
            return Grab::Full;
        };
        let (w, h) = (u32::from(self.width), u32::from(self.height));
        if !self.ring.first().is_some_and(|e| e.mask.matches(w, h)) {
            self.ring = (0..AGE_RING)
                .map(|_| AgeEntry {
                    tick: 0,
                    mask: TileBitmap::new(w, h),
                })
                .collect();
            self.union = TileBitmap::new(w, h);
        }
        let entry = &mut self.ring[(t % AGE_RING as u64) as usize];
        entry.tick = t;
        // The plan was made before the geometry was re-read: a mask for
        // another size says nothing about this one.
        if plan.known && self.plan_mask.matches(w, h) {
            entry.mask.copy_from(&self.plan_mask);
            if let Some(s) = plan.slice {
                entry.mask.set_rows(s.lo, s.hi);
            }
        } else {
            entry.mask.set_all();
        }
        let Some(s) = held.filter(|&s| s < t && s >= self.age_floor && t - s <= AGE_RING as u64)
        else {
            return Grab::Full;
        };
        #[cfg(test)]
        let from = if self.mutation == Mutation::DropAgeUnion {
            t
        } else {
            s + 1
        };
        #[cfg(not(test))]
        let from = s + 1;
        self.union.clear();
        for u in from..=t {
            let e = &self.ring[(u % AGE_RING as u64) as usize];
            if e.tick != u || !e.mask.matches(w, h) {
                return Grab::Full;
            }
            self.union.or_with(&e.mask);
        }
        #[cfg(test)]
        let blend_tiles = self.mutation != Mutation::DropBlendCursorTiles;
        #[cfg(not(test))]
        let blend_tiles = true;
        if let Some(c) = cursor.filter(|_| blend_tiles) {
            cursor_tiles_into(&mut self.union, c);
        }
        bands::plan(&self.union, h)
    }

    /// Capture a single frame: root, then the cursor on top.
    pub fn capture(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        self.refresh_geometry()?;
        if retry_due(&mut self.shm_retry_in) {
            self.ensure_segment();
        }
        let slot = self.pool.try_acquire().ok_or_else(|| {
            io::Error::new(io::ErrorKind::WouldBlock, "every frame slot is in flight")
        })?;
        // The cursor is resolved before the grab: its rectangle is part of
        // the grab set (it is blended into the slot, so its rows must be
        // fresh before the blend). Best effort: without XFixes the frame is
        // correct minus the cursor.
        let cursor = match self.polled_cursor.take() {
            Some(polled) => polled,
            None => self.query_cursor(),
        };
        let held = slot
            .storage()
            .and_then(|s| s.as_any().downcast_ref::<ShmSegment>())
            .and_then(|s| s.held.get());
        let choice = self.choose(held, cursor.as_ref().map(CursorRect::of));
        let mut pixels = match self.grab(slot, choice) {
            Ok(p) => p,
            Err(e) => {
                // The tick's grab set was consumed and nothing carries it:
                // every slot's age is unknown from here.
                self.ring.clear();
                self.age_floor = self.tick + 1;
                return Err(e);
            }
        };
        self.last_cursor = cursor.as_ref().map(CursorRect::of);
        self.last_cursor_serial = cursor.as_ref().map(|c| c.cursor_serial);
        if let Some(cursor) = cursor {
            blit_cursor(
                pixels.bytes_mut(),
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
fn check_pixel_format(
    setup: &x11rb::protocol::xproto::Setup,
    screen: &x11rb::protocol::xproto::Screen,
) -> io::Result<()> {
    let root_depth = screen.root_depth;
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
    let visual = screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == screen.root_visual);
    if let Some(v) = visual {
        if !masks_are_bgrx(v.red_mask, v.green_mask, v.blue_mask) {
            return Err(unsupported(format!(
                "X root visual has channel masks r={:#x} g={:#x} b={:#x}; \
                 capture needs 0xff0000/0xff00/0xff (BGRX)",
                v.red_mask, v.green_mask, v.blue_mask
            )));
        }
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

/// The channel layout every later stage assumes: B in byte 0, G in 1, R in 2.
fn masks_are_bgrx(red: u32, green: u32, blue: u32) -> bool {
    (red, green, blue) == (0x00ff_0000, 0x0000_ff00, 0x0000_00ff)
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
    let CursorRect { x: cx, y: cy, w, h } = CursorRect::of(cursor);
    let (cw, ch) = (w as i32, h as i32);

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

    /// A setup whose root is a 24-bit TrueColor visual with the given masks,
    /// little-endian, 32 bits per pixel: everything `check_pixel_format` reads.
    fn synthetic_setup(red: u32, green: u32, blue: u32) -> x11rb::protocol::xproto::Setup {
        use x11rb::protocol::xproto::{
            BackingStore, Depth, EventMask, Format, ImageOrder, Setup, VisualClass, Visualtype,
        };
        let visual = Visualtype {
            visual_id: 0x21,
            class: VisualClass::TRUE_COLOR,
            bits_per_rgb_value: 8,
            colormap_entries: 256,
            red_mask: red,
            green_mask: green,
            blue_mask: blue,
        };
        let screen = x11rb::protocol::xproto::Screen {
            root: 0x100,
            default_colormap: 0x20,
            white_pixel: 0xffff_ffff,
            black_pixel: 0,
            current_input_masks: EventMask::NO_EVENT,
            width_in_pixels: 1280,
            height_in_pixels: 1024,
            width_in_millimeters: 338,
            height_in_millimeters: 271,
            min_installed_maps: 1,
            max_installed_maps: 1,
            root_visual: visual.visual_id,
            backing_stores: BackingStore::NOT_USEFUL,
            save_unders: false,
            root_depth: 24,
            allowed_depths: vec![Depth {
                depth: 24,
                visuals: vec![visual],
            }],
        };
        Setup {
            status: 1,
            protocol_major_version: 11,
            protocol_minor_version: 0,
            length: 0,
            release_number: 0,
            resource_id_base: 0,
            resource_id_mask: 0,
            motion_buffer_size: 0,
            maximum_request_length: 0xffff,
            image_byte_order: ImageOrder::LSB_FIRST,
            bitmap_format_bit_order: ImageOrder::LSB_FIRST,
            bitmap_format_scanline_unit: 32,
            bitmap_format_scanline_pad: 32,
            min_keycode: 8,
            max_keycode: 255,
            vendor: Vec::new(),
            pixmap_formats: vec![Format {
                depth: 24,
                bits_per_pixel: 32,
                scanline_pad: 32,
            }],
            roots: vec![screen],
        }
    }

    #[test]
    fn a_bgrx_root_visual_is_accepted() {
        let setup = synthetic_setup(0x00ff_0000, 0x0000_ff00, 0x0000_00ff);
        check_pixel_format(&setup, &setup.roots[0]).expect("BGRX root must be accepted");
    }

    #[test]
    fn an_rgb_ordered_root_visual_is_refused() {
        // Same depth and bpp as the BGRX case: only the channel order differs,
        // so this refusal can only come from the mask check.
        let setup = synthetic_setup(0x0000_00ff, 0x0000_ff00, 0x00ff_0000);
        let err =
            check_pixel_format(&setup, &setup.roots[0]).expect_err("RGB root must be refused");
        assert!(err.to_string().contains("channel masks"), "{err}");
    }

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

    #[test]
    fn cursor_rect_subtracts_the_hotspot_and_can_straddle_a_tile() {
        // Pointer at x=40 with hotspot 10: the image starts at 30, so an
        // 8-wide cursor covers 30..38 and crosses the tile boundary at 32.
        let c = GetCursorImageReply {
            x: 40,
            y: 40,
            xhot: 10,
            yhot: 10,
            width: 8,
            height: 8,
            ..Default::default()
        };
        let r = CursorRect::of(&c);
        assert_eq!(r, rect(30, 30, 8, 8));
        assert_eq!(
            cursor_dirty_tiles(None, Some(r), 256, 256),
            vec![(0, 0), (0, 1), (1, 0), (1, 1)]
        );
    }

    #[test]
    fn masks_must_be_bgrx() {
        assert!(masks_are_bgrx(0xff0000, 0xff00, 0xff));
        assert!(!masks_are_bgrx(0xff, 0xff00, 0xff0000)); // RGB order
        assert!(!masks_are_bgrx(0xf800, 0x7e0, 0x1f)); // 565
    }

    #[test]
    fn shm_retry_counts_down_then_fires_once() {
        let mut c = None;
        assert!(!retry_due(&mut c));
        c = Some(2);
        assert!(!retry_due(&mut c));
        assert!(!retry_due(&mut c));
        assert!(retry_due(&mut c));
        assert_eq!(c, None);
        assert!(!retry_due(&mut c));
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
        let shm = f
            .grab(f.pool().try_acquire().unwrap(), Grab::Full)
            .unwrap()
            .to_vec();
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
        assert_eq!(
            f.grab(f.pool().try_acquire().unwrap(), Grab::Full)
                .unwrap()
                .to_vec(),
            root_get_image(x.display())
        );
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
        let plain = f
            .grab(f.pool().try_acquire().unwrap(), Grab::Full)
            .unwrap()
            .to_vec();
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
            let plain = f
                .grab(f.pool().try_acquire().unwrap(), Grab::Full)
                .unwrap()
                .to_vec();
            assert!(plain.iter().any(|&b| b != 0), "root not painted at {w}x{h}");
            assert_eq!(plain, root_get_image_sized(x.display(), w, h), "{w}x{h}");
        }
    }

    fn paint_fill(display: &str, w: u16, h: u16, v: u8) {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let screen = &conn.setup().roots[n];
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, screen.root, &CreateGCAux::new())
            .unwrap();
        let data = vec![v; usize::from(w) * usize::from(h) * 4];
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

    /// The point of PR 2, as a property: once the slots are warm a tick
    /// allocates nothing for the grab and copies only the cursor image, a
    /// tiny fraction of the root. Non-zero is proved by the warm-up ticks,
    /// which must count the segment allocations, and by the GetImage path,
    /// which still counts a full-frame copy.
    #[test]
    fn a_warm_shm_grab_allocates_and_copies_nothing() {
        const WANT: u64 = 64 * 48 * 4;
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        paint_root(x.display());
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        assert_eq!(f.path(), GrabPath::Shm);
        // Not reset after connect: the probe segment's attach counts too.
        // Hold POOL_SLOTS frames at once so every slot is allocated.
        let held: Vec<_> = (0..POOL_SLOTS).map(|_| f.capture(0).unwrap()).collect();
        let warm = f.take_stats();
        drop(held);
        let mut cursor_allocs = None;
        for i in 0..8 {
            let frame = f.capture(0).unwrap();
            drop(frame);
            let s = f.take_stats();
            // Only the cursor reply, the same every tick, never a full root.
            let c = *cursor_allocs.get_or_insert(s.allocs);
            assert!(c <= 1 && s.allocs == c, "tick {i}: {s:?}");
            assert!(s.bytes_copied < WANT / 4, "tick {i} copied {s:?}");
        }
        // Warm-up = one cursor reply per capture plus one attach per slot;
        // the cursor part alone (steady cost) must not satisfy this.
        let c = cursor_allocs.unwrap();
        assert!(
            warm.allocs >= POOL_SLOTS as u32 * (1 + c),
            "warm-up must count every slot allocation, got {warm:?} (cursor {c}/tick)"
        );
        // The same display through GetImage still pays the full copy, so
        // the counter above can read low.
        let mut g = X11Frames::connect(Some(x.display()), None).unwrap();
        g.shm_usable = false;
        g.shm_len = None;
        drop(g.capture(0).unwrap());
        let s = g.take_stats();
        assert!(s.bytes_copied >= WANT, "GetImage path: {s:?}");
    }

    /// The pooled SHM frame is byte-identical to the GetImage frame (the
    /// unchanged old path) for the same screen, including after the slot
    /// held a different picture.
    #[test]
    fn a_recycled_slot_shows_the_current_screen_not_its_old_content() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let mut shm = X11Frames::connect(Some(x.display()), None).unwrap();
        let mut plain = X11Frames::connect(Some(x.display()), None).unwrap();
        plain.shm_usable = false;
        plain.shm_len = None;
        assert_eq!(shm.path(), GrabPath::Shm);
        assert_eq!(plain.path(), GrabPath::GetImage);
        for v in [0x11u8, 0x80, 0xee, 0x05, 0x40, 0x99] {
            paint_fill(x.display(), 64, 48, v);
            let a = shm.capture(0).unwrap();
            let b = plain.capture(0).unwrap();
            assert!(a.pixels.is_pooled() && !b.pixels.is_pooled());
            assert_eq!(&*a.pixels, &*b.pixels, "fill {v:#x}");
            assert!(a.pixels.contains(&v), "fill {v:#x} not seen");
        }
    }

    /// Every slot is out: capture says so instead of allocating a fifth, and
    /// dropping one frame frees exactly one slot.
    #[test]
    fn exhausting_the_pool_fails_the_capture_and_a_drop_recovers() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        let mut held: Vec<_> = (0..POOL_SLOTS).map(|_| f.capture(0).unwrap()).collect();
        assert_eq!(f.pool().free_slots(), 0);
        let err = f.capture(0).err().expect("a fifth frame appeared");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
        drop(held.pop());
        assert_eq!(f.pool().free_slots(), 1);
        held.push(f.capture(0).unwrap());
        drop(held);
        assert_eq!(f.pool().free_slots(), POOL_SLOTS);
    }

    /// A grab that fails after taking a slot (the X server rejects the
    /// request) returns the slot, on the SHM path and on the GetImage path.
    #[test]
    fn a_failed_grab_returns_its_slot() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        f.root = 0x00de_ad00; // not a drawable: BadDrawable
        let err = f
            .grab(f.pool().try_acquire().unwrap(), Grab::Full)
            .unwrap_err();
        assert!(!err.to_string().is_empty());
        assert_eq!(
            f.pool().free_slots(),
            POOL_SLOTS,
            "slot leaked on SHM error"
        );
        f.shm_len = None;
        f.grab(f.pool().try_acquire().unwrap(), Grab::Full)
            .unwrap_err();
        assert_eq!(
            f.pool().free_slots(),
            POOL_SLOTS,
            "slot leaked on GetImage error"
        );
    }

    /// Frames leave the pipeline three ways besides being consumed, and each
    /// must return its slot: a failed submit (the channel is closed and the
    /// frame comes back in the error), frames still queued when the receiver
    /// stops, and a worker that drops a frame without looking at it. Every
    /// wait carries a timeout, so a path that keeps its slot is a failure
    /// and not a hang.
    #[tokio::test]
    async fn every_exit_path_returns_the_slot() {
        use std::time::Duration;
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        let pool = f.pool();
        let wait = |pool: FramePool| async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                while pool.free_slots() != POOL_SLOTS {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
        };

        // Failed submit: the receiver is gone before the send.
        let (tx, rx) = tokio::sync::mpsc::channel::<FrameSubmission>(2);
        drop(rx);
        let sub = f.capture(0).unwrap();
        let rejected = tx.send(sub).await.unwrap_err();
        drop(rejected);
        wait(pool.clone())
            .await
            .expect("slot not returned after a failed submit");

        // Queued frames when the receiver stops, plus one the worker took.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<FrameSubmission>(2);
        for _ in 0..3 {
            tx.send(f.capture(0).unwrap()).await.unwrap();
            if pool.free_slots() == POOL_SLOTS - 2 {
                break;
            }
        }
        let worker = rx.recv().await.unwrap();
        drop(worker);
        drop(rx);
        wait(pool.clone())
            .await
            .expect("slots not returned after the receiver stopped");
        drop(tx);

        // Worker thread stop: a frame owned by a thread that exits.
        let sub = f.capture(0).unwrap();
        std::thread::spawn(move || drop(sub)).join().unwrap();
        wait(pool)
            .await
            .expect("slot not returned after a worker thread ended");
    }

    /// The backpressure the loop relies on: with every slot out, `wait_free`
    /// does not resolve; it resolves when a frame is dropped.
    #[tokio::test]
    async fn the_capture_wait_ends_when_the_pipeline_returns_a_frame() {
        use std::time::Duration;
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        let pool = f.pool();
        let mut held: Vec<_> = (0..POOL_SLOTS).map(|_| f.capture(0).unwrap()).collect();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), pool.wait_free())
                .await
                .is_err(),
            "wait_free resolved with every slot held"
        );
        let p = pool.clone();
        let waiter = tokio::spawn(async move { p.wait_free().await });
        tokio::task::yield_now().await;
        drop(held.pop());
        tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("a frame was returned and the capture wait never ended")
            .unwrap();
    }

    /// After a shrink no segment of the old length survives, even though only
    /// one slot is used per tick (a LIFO free list would never pop the rest).
    #[test]
    fn a_resize_releases_every_old_size_segment() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        let held: Vec<_> = (0..POOL_SLOTS).map(|_| f.capture(0).unwrap()).collect();
        drop(held);
        let old = 64 * 48 * 4;
        let count = |f: &X11Frames, len: usize| {
            let mut n = 0;
            let free = f.pool.take_free_where(|s| {
                if s.as_any()
                    .downcast_ref::<ShmSegment>()
                    .is_some_and(|g| g.len == len)
                {
                    n += 1;
                }
                false
            });
            assert!(free.is_empty());
            n
        };
        assert_eq!(count(&f, old), POOL_SLOTS);
        set_root_size(x.display(), 32, 24);
        for _ in 0..8 {
            drop(f.capture(0).unwrap());
        }
        assert_eq!(count(&f, old), 0, "old-size segments survived the resize");
        assert!(count(&f, 32 * 24 * 4) >= 1);
        // Falling back to GetImage for good releases the rest.
        f.shm_usable = false;
        f.shm_len = None;
        f.release_stale();
        assert_eq!(count(&f, 32 * 24 * 4), 0);
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
        // The diff is against where the cursor was DRAWN.
        f.capture(0).unwrap();
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

    /// `last_cursor` is where the cursor was DRAWN: polling twice without a
    /// capture in between must report the same vacated tile both times.
    #[test]
    fn polling_does_not_advance_where_the_cursor_was_drawn() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "128x96x24"]);
        let (conn, n) = x11rb::connect(Some(x.display())).unwrap();
        let root = conn.setup().roots[n].root;
        let warp = |px: i16, py: i16| {
            conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, px, py)
                .unwrap();
            conn.get_input_focus().unwrap().reply().unwrap();
        };
        warp(10, 10);
        let mut f = X11Frames::connect(Some(x.display()), None).unwrap();
        f.capture(0).unwrap();
        warp(100, 70);
        assert!(f.poll_cursor().contains(&(0, 0)));
        assert!(
            f.poll_cursor().contains(&(0, 0)),
            "second poll forgot the drawn position"
        );
    }
}
