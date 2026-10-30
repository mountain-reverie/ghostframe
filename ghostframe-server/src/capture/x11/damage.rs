//! XDamage integration: reports which screen tiles changed since the last
//! drain, as an honest tri-state [`Damage`].
//!
//! Two report levels, chosen by `GHOSTFRAME_DAMAGE_REPORT`:
//!
//! - `NonEmpty` (default): the server sends one event when the damage region
//!   goes from empty to non-empty. A drain then reads the whole region with
//!   `DamageSubtract(parts)` + `XFixesFetchRegion`; the server has already
//!   banded and coalesced the rectangles, so the cost follows the region's
//!   shape rather than the application's draw-call count. An idle screen costs
//!   no round trip at all.
//! - `RawRectangles`: one event per damaged rectangle, every one read and
//!   mapped to tiles. The fallback.
//!
//! Anything that makes the answer untrustworthy yields `Damage::Unknown`: not
//! armed, the first drain after arming, a geometry change (and the tick after
//! it), a protocol error, a broken connection, an event flood.

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::xfixes::{self, ConnectionExt as XfixesExt};
use x11rb::protocol::xproto::Drawable;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::capture::{Damage, TileBitmap};
use crate::config::DamageReport;

/// Events (raw) or region rectangles (non-empty) per drain above which the
/// answer degrades to `Unknown`: one full comparison beats thousands of small
/// ones. Same posture as x11vnc's flood escape.
pub(crate) const FLOOD_LIMIT: usize = 512;

/// Drains that stay `Unknown` once a geometry change is seen. Region contents
/// at the moment of a mode change are meaningless, and the capture side only
/// notices the new size one tick after this side does.
const GEOMETRY_SETTLE_TICKS: u8 = 2;

/// Tracks X11 damage events on the root window.
///
/// Armed and disarmed around the capture loop's idle gate rather than held for
/// the daemon's lifetime: a Damage object held while no client is connected
/// costs the X server bookkeeping and socket writes for output nobody reads,
/// and grows an event backlog in the server's per-client output buffer that
/// nothing drains. While disarmed no Damage object exists, so the server
/// generates nothing at all.
///
/// Disarming is invisible to every other X client: a Damage object is a
/// per-client server resource, so tearing ours down does not affect the
/// compositor's or any application's.
pub struct XDamageMonitor {
    conn: RustConnection,
    /// Drawable damage is reported for. Production always passes the root
    /// window; the X11 tests retarget it to a scratch pixmap so they can
    /// generate real damage without drawing on a live session.
    target: Drawable,
    /// Allocated once in `new` and reused across every arm/disarm cycle.
    /// x11rb's `generate_id` has no counterpart that releases an XID, so
    /// taking a fresh one per arm would consume the client's XID range one
    /// reconnect at a time. Re-creating a resource under an XID we destroyed
    /// ourselves is legal (`x11_arm_disarm_cycle_is_accepted_by_the_server`
    /// holds the server to it).
    damage_id: damage::Damage,
    /// Whether a Damage object currently exists under `damage_id`. Arm and
    /// disarm are idempotent on it, so the capture loop can call either on
    /// every idle transition without tracking the previous state itself.
    armed: bool,
    /// The report level in effect. `NonEmpty` degrades to `Raw` at `new` when
    /// the server lacks XFixes regions.
    report: DamageReport,
    /// XFixes region that receives `DamageSubtract`'s `parts`; created once.
    parts: xfixes::Region,
    flood_limit: usize,
    /// The next drain must be `Unknown` (just armed, or the last one erred).
    force_unknown: bool,
    /// Screen size the last drain saw, and the `Unknown` drains left after it
    /// changed.
    dims: Option<(u32, u32)>,
    settle: u8,
    /// Bitmap reused across drains, so an idle tick allocates nothing. A
    /// result is a clone of it.
    scratch: TileBitmap,
    /// Heap allocations since `take_allocs`: the scratch (re-)allocation, the
    /// result clone, the region rectangles fetched from the server.
    allocs: u32,
}

impl XDamageMonitor {
    /// Connect to `display` (`None` = `$DISPLAY`) and confirm the XDamage
    /// extension is present.
    ///
    /// Returns `None` if the extension is unavailable. Does **not** arm: the
    /// daemon starts with no client connected, and arming then would be the
    /// leak this type exists to avoid. Call [`arm`] when a client attaches.
    ///
    /// [`arm`]: XDamageMonitor::arm
    pub fn new(display: Option<&str>, report: DamageReport) -> Option<Self> {
        let (conn, screen_num) = RustConnection::connect(display).ok()?;

        // Query the damage extension. This also registers extension info so
        // events are properly parsed once we do arm.
        conn.damage_query_version(1, 1).ok()?.reply().ok()?;

        let target = conn.setup().roots[screen_num].root;
        let damage_id = conn.generate_id().ok()?;
        let parts = conn.generate_id().ok()?;

        // Regions need XFixes 2+. Without them, the raw level still works.
        let mut report = report;
        let have_regions = conn
            .xfixes_query_version(5, 0)
            .ok()
            .and_then(|c| c.reply().ok())
            .is_some()
            && conn.xfixes_create_region(parts, &[]).is_ok()
            && conn.flush().is_ok();
        if !have_regions && report == DamageReport::NonEmpty {
            tracing::warn!("XFixes regions unavailable; XDamage falls back to RawRectangles");
            report = DamageReport::Raw;
        }

        Some(Self {
            conn,
            target,
            damage_id,
            armed: false,
            report,
            parts,
            flood_limit: FLOOD_LIMIT,
            force_unknown: true,
            dims: None,
            settle: 0,
            scratch: TileBitmap::empty(),
            allocs: 0,
        })
    }

    /// Heap allocations the drains made since the last call; resets.
    pub fn take_allocs(&mut self) -> u32 {
        std::mem::take(&mut self.allocs)
    }

    /// The next drain reports `Unknown`. Called when a frame was lost after
    /// its damage was already consumed.
    pub fn distrust_next_drain(&mut self) {
        self.force_unknown = true;
    }

    /// Whether damage reporting is currently active.
    ///
    /// Logged on each idle transition. A monitor that silently fails to arm
    /// reports `Unknown`, the capture loop compares every frame in full, and
    /// every frame is still *correct* -- just more expensive, with nothing to
    /// say why. This is what makes that visible.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// The report level in effect (after any fallback).
    pub fn report_level(&self) -> DamageReport {
        self.report
    }

    /// Start reporting damage on `target` (the root window in production).
    /// Idempotent.
    pub fn arm(&mut self) {
        if self.armed {
            return;
        }
        // Anything still queued predates this arm. Letting it through would
        // attribute stale rectangles to the newly connected client's first
        // frame.
        self.discard_pending();

        let level = match self.report {
            DamageReport::NonEmpty => damage::ReportLevel::NON_EMPTY,
            DamageReport::Raw => damage::ReportLevel::RAW_RECTANGLES,
        };
        if self
            .conn
            .damage_create(self.damage_id, self.target, level)
            .is_err()
            || self.conn.flush().is_err()
        {
            tracing::warn!("XDamage arm failed; falling back to full-frame dirty detection");
            return;
        }
        self.armed = true;
        // The first frame for a newly connected client must be compared in
        // full whatever the (empty) damage region says.
        self.force_unknown = true;
        self.dims = None;
        self.settle = 0;
    }

    /// Stop reporting damage and discard anything already queued. Idempotent.
    pub fn disarm(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.conn.damage_destroy(self.damage_id);
        // Round-trip so the destroy is known to have been processed. After it
        // the server emits no further DamageNotify for `damage_id`, which is
        // what makes the discard below exhaustive instead of racing events
        // the server had already queued.
        let _ = self.conn.sync();
        self.discard_pending();
        self.armed = false;
    }

    /// Damage since the previous drain on a `width` x `height` screen.
    /// Non-blocking.
    ///
    /// Order, both halves load-bearing:
    ///
    /// - Within a drain, events are read before the subtract, because with
    ///   `NonEmpty` the subtract is gated on having seen a notification (an
    ///   idle tick costs no round trip). A draw that lands between the two is
    ///   part of what the subtract returns; one that lands after raises a
    ///   fresh notification for the next drain.
    /// - Across the tick, the drain (and so the subtract) must come *before*
    ///   the grab. A draw after the subtract is re-reported next tick, so at
    ///   worst it is grabbed early and repeated. Grab first and a draw that
    ///   lands between grab and subtract is consumed by this drain but absent
    ///   from this frame: lost. `damage_then_grab` fixes that order and
    ///   `x11_a_draw_after_the_grab_is_reported_next_tick` holds it.
    ///
    pub fn drain_damage(&mut self, width: u32, height: u32) -> Damage {
        self.drain_inner(width, height)
    }

    fn drain_inner(&mut self, width: u32, height: u32) -> Damage {
        if !self.armed {
            return Damage::Unknown;
        }
        let mut unknown = std::mem::take(&mut self.force_unknown);
        if self.dims != Some((width, height)) {
            if self.dims.is_some() {
                self.settle = GEOMETRY_SETTLE_TICKS;
            }
            self.dims = Some((width, height));
        }
        if self.settle > 0 {
            self.settle -= 1;
            unknown = true;
        }

        if !self.scratch.matches(width, height) || self.scratch.grid() == (0, 0) {
            self.scratch = TileBitmap::new(width, height);
            self.allocs += 1;
        }
        let mut bitmap = std::mem::replace(&mut self.scratch, TileBitmap::empty());
        // Step 1: read every pending event. Step 2: subtract.
        let (notified, read_err) = self.read_events(&mut bitmap);
        let (flooded, sub_err) = self.subtract(notified, &mut bitmap);
        let errored = read_err | sub_err;

        let result = if errored {
            // Not trusted again until a drain completes cleanly; this one and
            // the next compare everything.
            self.force_unknown = true;
            Damage::Unknown
        } else if unknown || flooded {
            Damage::Unknown
        } else if bitmap.is_empty() {
            Damage::None
        } else {
            self.allocs += 1; // the clone handed out
            Damage::Tiles(bitmap.clone())
        };
        bitmap.clear();
        self.scratch = bitmap;
        result
    }

    /// Step 1: read every pending event. Returns how many `DamageNotify`
    /// arrived and whether anything went wrong. In `Raw` mode each event's
    /// rectangle goes into `bitmap` (up to the flood limit).
    fn read_events(&self, bitmap: &mut TileBitmap) -> (usize, bool) {
        let mut notified = 0usize;
        let mut errored = false;
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::DamageNotify(ev))) => {
                    notified += 1;
                    if self.report == DamageReport::Raw && notified <= self.flood_limit {
                        bitmap.set_rect(ev.area.x, ev.area.y, ev.area.width, ev.area.height);
                    }
                }
                Ok(Some(Event::Error(_))) => errored = true,
                Ok(Some(_)) => {}
                Ok(None) => break,
                Err(_) => {
                    errored = true;
                    break;
                }
            }
        }
        (notified, errored)
    }

    /// Step 2: reset the damage region. In both modes only when step 1 saw a
    /// notification (for `NonEmpty` that gate is why the order matters), and
    /// the region it returns goes into `bitmap`. Returns `(flooded, errored)`.
    fn subtract(&mut self, notified: usize, bitmap: &mut TileBitmap) -> (bool, bool) {
        let mut flooded = false;
        let mut errored = false;
        match self.report {
            // Nothing was reported, so there is nothing to reset: an idle tick
            // sends no request at all.
            DamageReport::Raw if notified > 0 => {
                flooded = notified > self.flood_limit;
                errored = self
                    .conn
                    .damage_subtract(self.damage_id, 0u32, 0u32)
                    .is_err();
                if self.conn.flush().is_err() {
                    errored = true;
                }
            }
            DamageReport::Raw => {}
            DamageReport::NonEmpty if notified > 0 => match self.subtract_into_parts() {
                Some(rects) => {
                    if !rects.is_empty() {
                        self.allocs += 1; // the fetched rectangle list
                    }
                    if rects.len() <= self.flood_limit {
                        for r in rects {
                            bitmap.set_rect(r.x, r.y, r.width, r.height);
                        }
                    } else {
                        flooded = true;
                    }
                }
                None => errored = true,
            },
            DamageReport::NonEmpty => {}
        }
        (flooded, errored)
    }

    /// `DamageSubtract(repair = None, parts)` then `XFixesFetchRegion(parts)`.
    /// `None` on any failure.
    fn subtract_into_parts(&self) -> Option<Vec<x11rb::protocol::xproto::Rectangle>> {
        let subtract = self
            .conn
            .damage_subtract(self.damage_id, 0u32, self.parts)
            .ok()?;
        let reply = self
            .conn
            .xfixes_fetch_region(self.parts)
            .ok()?
            .reply()
            .ok()?;
        // DamageSubtract has no reply. If the server rejected it, `parts`
        // still holds whatever it held before and the fetch above succeeded
        // anyway: trusting it would report stale (or no) damage. The fetch
        // reply is later in sequence, so this check costs no round trip.
        subtract.check().ok()?;
        Some(reply.rectangles)
    }

    /// Drop every event currently queued on our connection, damage or not.
    ///
    /// Used on both sides of an arm/disarm so no event crosses the boundary.
    ///
    /// Damage requests carry no reply, so a rejected one surfaces only as an
    /// error event here -- which is why these are counted and logged rather
    /// than thrown away with everything else. Silence is the failure mode:
    /// damage reporting stops and the capture loop quietly falls back to
    /// full-frame comparison.
    fn discard_pending(&self) {
        let mut errors = 0usize;
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            if matches!(event, Event::Error(_)) {
                errors += 1;
            }
        }
        if errors > 0 {
            tracing::warn!(
                errors,
                "X protocol errors on the damage connection; damage reporting may be inactive"
            );
        }
    }

    /// Point damage reporting at a freshly created off-screen pixmap instead
    /// of the root window, returning it.
    ///
    /// Test-only. Lets the X11 tests generate genuine damage by drawing into
    /// a drawable of their own: drawing on the root to provoke damage would
    /// scribble a rectangle onto the screen of any developer who runs
    /// `cargo test` against their live session.
    #[cfg(test)]
    fn retarget_to_scratch_pixmap(&mut self, width: u16, height: u16) -> Drawable {
        use x11rb::protocol::xproto::ConnectionExt as _;

        assert!(!self.armed, "retarget before arming, not after");
        let depth = self.conn.setup().roots[0].root_depth;
        let pixmap = self
            .conn
            .generate_id()
            .expect("generate_id for scratch pixmap");
        // `self.target` is still the root here, which is the drawable whose
        // screen and depth the new pixmap is created against.
        self.conn
            .create_pixmap(depth, pixmap, self.target, width, height)
            .expect("create_pixmap");
        self.target = pixmap;
        pixmap
    }

    /// Count the `DamageNotify` events queued on the wire, ignoring the
    /// `armed` flag.
    ///
    /// Test-only. `drain_damage` short-circuits on `armed`, so it reports an
    /// empty drain after `disarm` whether or not the server actually stopped
    /// sending. This reads what the server is really doing.
    #[cfg(test)]
    fn count_pending_damage_events(&self) -> usize {
        let mut n = 0;
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            if matches!(event, Event::DamageNotify(_)) {
                n += 1;
            }
        }
        n
    }

    /// Drain the event queue and return only the X protocol errors.
    ///
    /// Test-only counterpart to `discard_pending`: the X11 tests need to see
    /// the errors it would otherwise log and drop.
    #[cfg(test)]
    fn take_pending_errors(&self) -> Vec<x11rb::x11_utils::X11Error> {
        let mut errors = Vec::new();
        while let Ok(Some(event)) = self.conn.poll_for_event() {
            if let Event::Error(e) = event {
                errors.push(e);
            }
        }
        errors
    }
}

impl Drop for XDamageMonitor {
    fn drop(&mut self) {
        if self.armed {
            let _ = self.conn.damage_destroy(self.damage_id);
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// Tests that need a live X server: each starts its own private Xvfb
/// (`test_support::PrivateXvfb`), so they never touch `$DISPLAY`.
#[cfg(test)]
mod x11_tests {
    use super::*;
    use crate::capture::x11::test_support::PrivateXvfb;

    /// The arm/disarm cycle reuses one XID across every reconnect, which is
    /// only sound if the server accepts `damage_create` under an XID the
    /// client destroyed itself. If it answers `BadIDChoice` instead, the
    /// second arm silently stops reporting damage and the capture loop falls
    /// back to full-frame comparison forever -- correct output, quietly worse.
    ///
    /// Damage requests carry no reply, so a rejection is delivered as an error
    /// *event*, not as a failed call. `sync()` forces the round trip that
    /// makes the error queueable; `take_pending_errors` is what reads it.
    /// Without both, a rejected request is indistinguishable from success.
    #[test]
    fn x11_arm_disarm_cycle_is_accepted_by_the_server() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "256x256x24"]);
        let mut m = XDamageMonitor::new(Some(x.display()), DamageReport::NonEmpty)
            .expect("XDamage extension");
        assert!(!m.is_armed(), "new() must not arm: the daemon starts idle");

        for cycle in 0..3 {
            m.arm();
            assert!(m.is_armed(), "arm {cycle} did not take");
            m.conn.sync().expect("sync after arm");
            let errors = m.take_pending_errors();
            assert!(
                errors.is_empty(),
                "cycle {cycle}: the server rejected damage_create under a \
                 reused XID: {errors:?}"
            );

            // disarm round-trips and drains internally, so the queue is clean
            // again before the next arm.
            m.disarm();
            assert!(!m.is_armed(), "disarm {cycle} did not take");
        }
    }

    /// Both calls are idempotent, so the capture loop can invoke them on every
    /// idle transition without tracking what it did last time. A second
    /// `damage_create` under a live XID is `BadIDChoice`, so dropping the
    /// `armed` guard would show up as a queued error here.
    #[test]
    fn x11_arm_and_disarm_are_idempotent() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "256x256x24"]);
        let mut m = XDamageMonitor::new(Some(x.display()), DamageReport::NonEmpty)
            .expect("XDamage extension");

        m.disarm();
        assert!(!m.is_armed(), "disarming an unarmed monitor is a no-op");

        m.arm();
        m.arm();
        assert!(m.is_armed());
        m.conn.sync().expect("sync after double arm");
        let errors = m.take_pending_errors();
        assert!(
            errors.is_empty(),
            "the second arm reached the server and was rejected: {errors:?}"
        );

        m.disarm();
        m.disarm();
        assert!(!m.is_armed());
    }

    /// A disarmed monitor must report nothing, whatever the screen is doing.
    /// This is the property the capture loop's gate depends on.
    #[test]
    fn x11_drain_is_unknown_while_never_armed() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "256x256x24"]);
        let mut m = XDamageMonitor::new(Some(x.display()), DamageReport::NonEmpty)
            .expect("XDamage extension");
        assert_eq!(
            m.drain_damage(256, 256),
            Damage::Unknown,
            "a monitor that was never armed knows nothing, which is not \
             the same as nothing changed"
        );
    }

    /// The test the rest of this module cannot replace: damage must actually
    /// flow after `arm`, and the server must actually stop after `disarm`.
    ///
    /// Both failures are silent. An `arm` that sets the flag without creating
    /// the Damage object leaves every drain empty, the capture loop falls back
    /// to full-frame comparison, and output stays pixel-correct while costing
    /// more for the rest of the process's life. A `disarm` that only clears
    /// the flag leaves the backlog this whole mechanism exists to prevent --
    /// and `drain_damage` would not show it, because it short-circuits on the
    /// flag. Hence `count_pending_damage_events`, which reads the wire.
    #[test]
    fn x11_arming_makes_damage_flow_and_disarming_stops_the_server() {
        use x11rb::protocol::xproto::ConnectionExt as _;
        use x11rb::protocol::xproto::{CreateGCAux, Rectangle};

        let x = PrivateXvfb::spawn(&["-screen", "0", "256x256x24"]);
        let mut m = XDamageMonitor::new(Some(x.display()), DamageReport::NonEmpty)
            .expect("XDamage extension");
        let pixmap = m.retarget_to_scratch_pixmap(128, 128);

        let gc = m.conn.generate_id().expect("generate_id for gc");
        m.conn
            .create_gc(gc, pixmap, &CreateGCAux::new().foreground(0x00ff_ffff))
            .expect("create_gc");

        // A 10x10 fill at (33, 33) falls inside tile (1, 1) with TILE_SIZE 32.
        let fill = |m: &XDamageMonitor| {
            m.conn
                .poly_fill_rectangle(
                    pixmap,
                    gc,
                    &[Rectangle {
                        x: 33,
                        y: 33,
                        width: 10,
                        height: 10,
                    }],
                )
                .expect("poly_fill_rectangle");
            // Damage is generated as the server processes the draw, so the
            // round trip is what makes the poll below deterministic rather
            // than a race against the server.
            m.conn.sync().expect("sync after draw");
        };

        m.arm();
        assert!(m.is_armed());
        fill(&m);

        // The first drain after arming is Unknown by design; it also
        // consumes the notification, so look at the next one.
        assert_eq!(m.drain_damage(128, 128), Damage::Unknown);
        fill(&m);
        let Damage::Tiles(tiles) = m.drain_damage(128, 128) else {
            panic!(
                "arming produced no damage: the capture loop would silently \
                 fall back to full-frame comparison forever"
            );
        };
        assert!(
            tiles.get(1, 1),
            "a fill at (33, 33) must dirty tile (1, 1), got {tiles:?}"
        );

        m.disarm();
        fill(&m);
        assert_eq!(
            m.count_pending_damage_events(),
            0,
            "the server must stop generating damage after disarm -- an \
             unconsumed backlog here is the leak this mechanism removes"
        );

        let _ = m.conn.free_gc(gc);
        let _ = m.conn.free_pixmap(pixmap);
    }

    // ── Producer behaviour against a real server ────────────────────────

    use x11rb::protocol::xproto::{ConnectionExt as _, CreateGCAux, Rectangle};

    const SCRATCH: u16 = 512;

    /// An armed monitor over a scratch pixmap, with the arm-time `Unknown`
    /// already consumed and one settled drain behind it.
    struct Rig {
        m: XDamageMonitor,
        pixmap: Drawable,
        gc: u32,
        _x: PrivateXvfb,
    }

    impl Rig {
        fn new(level: DamageReport) -> Self {
            let x = PrivateXvfb::spawn(&["-screen", "0", "512x512x24"]);
            let mut m = XDamageMonitor::new(Some(x.display()), level).expect("XDamage extension");
            assert_eq!(m.report_level(), level, "the server lacks the level");
            let pixmap = m.retarget_to_scratch_pixmap(SCRATCH, SCRATCH);
            let gc = m.conn.generate_id().unwrap();
            m.conn
                .create_gc(gc, pixmap, &CreateGCAux::new().foreground(0x00ff_ffff))
                .unwrap();
            m.arm();
            m.conn.sync().unwrap();
            assert!(m.take_pending_errors().is_empty());
            assert_eq!(
                m.drain_damage(SCRATCH.into(), SCRATCH.into()),
                Damage::Unknown,
                "the first drain after arming must be Unknown"
            );
            Self {
                m,
                pixmap,
                gc,
                _x: x,
            }
        }

        /// One request per rectangle, as separate draw calls are in
        /// practice. (One `PolyFillRectangle` carrying many rectangles is
        /// reported as a single bounding box by the server.)
        fn draw(&self, rects: &[Rectangle]) {
            for r in rects {
                self.draw_batch(std::slice::from_ref(r));
            }
        }

        fn draw_batch(&self, rects: &[Rectangle]) {
            self.m
                .conn
                .poly_fill_rectangle(self.pixmap, self.gc, rects)
                .unwrap();
            // The server generates damage while processing the draw.
            self.m.conn.sync().unwrap();
        }

        fn drain(&mut self) -> Damage {
            self.m.drain_damage(SCRATCH.into(), SCRATCH.into())
        }
    }

    fn rect(x: i16, y: i16, w: u16, h: u16) -> Rectangle {
        Rectangle {
            x,
            y,
            width: w,
            height: h,
        }
    }

    /// Tiles a rectangle touches, computed by intersection and not through
    /// `set_rect`, so the two cannot share a bug.
    fn touched(r: &Rectangle) -> Vec<(u32, u32)> {
        let (x0, y0) = (i32::from(r.x), i32::from(r.y));
        let (x1, y1) = (x0 + i32::from(r.width), y0 + i32::from(r.height));
        let n = i32::from(SCRATCH) / 32;
        let mut v = Vec::new();
        for ty in 0..n {
            for tx in 0..n {
                let (a, b) = (tx * 32, ty * 32);
                if x0 < a + 32 && x1 > a && y0 < b + 32 && y1 > b {
                    v.push((tx as u32, ty as u32));
                }
            }
        }
        v
    }

    fn lcg(state: &mut u64) -> u32 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (*state >> 33) as u32
    }

    /// Real draws produce a superset of the touched tiles, at both report
    /// levels, over random rectangles (overlapping, edge-straddling, 1 px).
    #[test]
    fn x11_real_draws_report_a_superset_of_the_touched_tiles() {
        for level in [DamageReport::NonEmpty, DamageReport::Raw] {
            let mut rig = Rig::new(level);
            let mut seed = 0x5eed_u64;
            let mut seen_tiles = 0usize;
            for trial in 0..60 {
                let n = 1 + lcg(&mut seed) % 12;
                let rects: Vec<Rectangle> = (0..n)
                    .map(|_| {
                        let w = 1 + (lcg(&mut seed) % 120) as u16;
                        let h = 1 + (lcg(&mut seed) % 120) as u16;
                        let x = (lcg(&mut seed) % (SCRATCH as u32 - u32::from(w) + 1)) as i16;
                        let y = (lcg(&mut seed) % (SCRATCH as u32 - u32::from(h) + 1)) as i16;
                        rect(x, y, w, h)
                    })
                    .collect();
                if trial % 2 == 0 {
                    rig.draw(&rects);
                } else {
                    rig.draw_batch(&rects);
                }
                let Damage::Tiles(got) = rig.drain() else {
                    panic!("{level:?} trial {trial}: draws were not reported as tiles");
                };
                for r in &rects {
                    for (tx, ty) in touched(r) {
                        assert!(
                            got.get(tx, ty),
                            "{level:?} trial {trial}: tile ({tx},{ty}) of {r:?} missing"
                        );
                    }
                }
                seen_tiles += got.count();
                // Nothing left behind: the next drain is quiet.
                assert_eq!(rig.drain(), Damage::None, "{level:?} trial {trial}");
            }
            assert!(seen_tiles > 60, "the property never saw damage");
        }
    }

    #[test]
    fn x11_an_idle_armed_screen_is_none_not_unknown() {
        for level in [DamageReport::NonEmpty, DamageReport::Raw] {
            let mut rig = Rig::new(level);
            for _ in 0..3 {
                assert_eq!(rig.drain(), Damage::None, "{level:?}");
            }
        }
    }

    /// More than `flood_limit` events or rectangles degrade to `Unknown`
    /// instead of a long tile list; the next quiet drain is trusted again.
    #[test]
    fn x11_a_flood_degrades_to_unknown_and_recovers() {
        for level in [DamageReport::NonEmpty, DamageReport::Raw] {
            let mut rig = Rig::new(level);
            rig.m.flood_limit = 4;
            // Distinct, non-touching rects so the server cannot merge them.
            let rects: Vec<Rectangle> = (0..8).map(|i| rect(i * 40, i * 40, 4, 4)).collect();
            rig.draw(&rects);
            assert_eq!(rig.drain(), Damage::Unknown, "{level:?}");
            rig.draw(&[rect(0, 0, 4, 4)]);
            assert!(matches!(rig.drain(), Damage::Tiles(_)), "{level:?}");
            assert_eq!(rig.drain(), Damage::None, "{level:?}");
        }
    }

    /// A resize makes this drain and the next `Unknown`, then trust returns.
    #[test]
    fn x11_a_geometry_change_forces_unknown_for_two_drains() {
        let mut rig = Rig::new(DamageReport::NonEmpty);
        assert_eq!(rig.m.drain_damage(480, 512), Damage::Unknown);
        assert_eq!(rig.m.drain_damage(480, 512), Damage::Unknown);
        assert_eq!(rig.m.drain_damage(480, 512), Damage::None);
        // And the bitmap carries the new grid.
        rig.draw(&[rect(1, 1, 2, 2)]);
        let Damage::Tiles(t) = rig.m.drain_damage(480, 512) else {
            panic!("expected tiles");
        };
        assert!(t.matches(480, 512));
    }

    /// Re-arming starts from Unknown again: the first frame of a new client
    /// is compared in full.
    #[test]
    fn x11_rearming_forces_unknown() {
        let mut rig = Rig::new(DamageReport::NonEmpty);
        rig.m.disarm();
        rig.m.arm();
        assert_eq!(rig.drain(), Damage::Unknown);
        assert_eq!(rig.drain(), Damage::None);
    }
    /// A rejected `DamageSubtract` leaves `parts` as it was, and the fetch
    /// still succeeds. That must not read as "nothing changed".
    #[test]
    fn x11_a_rejected_subtract_is_unknown_not_stale_damage() {
        let mut rig = Rig::new(DamageReport::NonEmpty);
        rig.draw(&[rect(40, 40, 4, 4)]);
        // A Damage XID the server never heard of: BadDamage on the subtract,
        // while the region fetch (a valid region) succeeds.
        rig.m.damage_id = rig.m.conn.generate_id().unwrap();
        assert_eq!(rig.drain(), Damage::Unknown);
        assert!(
            rig.m.force_unknown,
            "an error also distrusts the next drain"
        );
    }
}
