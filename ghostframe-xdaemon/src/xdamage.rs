//! XDamage integration: monitors root window damage events and maps
//! damaged pixel rectangles to tile grid coordinates.

use std::collections::HashSet;

use x11rb::connection::Connection;
use x11rb::protocol::damage::{self, ConnectionExt as DamageExt};
use x11rb::protocol::xproto::Drawable;
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use ghostframe_lib::tile::TILE_SIZE;

/// Tracks X11 damage events on the root window.
///
/// Armed and disarmed around the capture loop's idle gate rather than held for
/// the daemon's lifetime. `RAW_RECTANGLES` is the most expensive report level
/// -- one event per damaged rectangle, no coalescing -- so a Damage object
/// held while no client is connected costs the X server that bookkeeping and
/// those socket writes for output nobody reads, and grows an event backlog in
/// the server's per-client output buffer that nothing drains. While disarmed
/// no Damage object exists, so the server generates nothing at all.
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
}

impl XDamageMonitor {
    /// Connect to `$DISPLAY` and confirm the XDamage extension is present.
    ///
    /// Returns `None` if the extension is unavailable. Does **not** arm: the
    /// daemon starts with no client connected, and arming then would be the
    /// leak this type exists to avoid. Call [`arm`] when a client attaches.
    ///
    /// [`arm`]: XDamageMonitor::arm
    pub fn new() -> Option<Self> {
        let (conn, screen_num) = RustConnection::connect(None).ok()?;

        // Query the damage extension. This also registers extension info so
        // events are properly parsed once we do arm.
        conn.damage_query_version(1, 1).ok()?.reply().ok()?;

        let target = conn.setup().roots[screen_num].root;
        let damage_id = conn.generate_id().ok()?;

        Some(Self {
            conn,
            target,
            damage_id,
            armed: false,
        })
    }

    /// Whether damage reporting is currently active.
    ///
    /// Logged on each idle transition. A monitor that silently fails to arm
    /// reports no dirty tiles, the capture loop falls back to full-frame
    /// comparison, and every frame is still *correct* -- just more expensive,
    /// with nothing to say why. This is what makes that visible.
    pub fn is_armed(&self) -> bool {
        self.armed
    }

    /// Start reporting damage on `target` (the root window in production).
    /// Idempotent.
    pub fn arm(&mut self) {
        if self.armed {
            return;
        }
        // Anything still queued predates this arm. Letting it through would
        // attribute stale rectangles to the newly connected client's first
        // frame -- and since a non-empty drain means "check only these
        // tiles", that first frame would be partial rather than full.
        self.discard_pending();

        if self
            .conn
            .damage_create(
                self.damage_id,
                self.target,
                damage::ReportLevel::RAW_RECTANGLES,
            )
            .is_err()
            || self.conn.flush().is_err()
        {
            tracing::warn!("XDamage arm failed; falling back to full-frame dirty detection");
            return;
        }
        self.armed = true;
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

    /// Drain all pending damage events and return the set of dirty tile
    /// coordinates. Non-blocking: returns an empty vec if no damage events are
    /// pending, and always empty while disarmed.
    pub fn drain_damage(&self) -> Vec<(u32, u32)> {
        if !self.armed {
            return Vec::new();
        }
        let mut tile_set = HashSet::new();

        // Drain pending events FIRST, then subtract. This ensures we consume
        // all notifications before resetting the damage region — otherwise
        // events queued between subtract and poll could be lost.
        loop {
            match self.conn.poll_for_event() {
                Ok(Some(Event::DamageNotify(ev))) => {
                    rect_to_tiles(
                        ev.area.x,
                        ev.area.y,
                        ev.area.width,
                        ev.area.height,
                        &mut tile_set,
                    );
                }
                Ok(Some(_)) => {
                    // Ignore non-damage events
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        // Subtract (reset) damage region after draining all events.
        let _ = self.conn.damage_subtract(self.damage_id, 0u32, 0u32);
        let _ = self.conn.flush();

        tile_set.into_iter().collect()
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

/// Map a damaged pixel rectangle onto the tile grid, inserting every tile it
/// touches into `out`.
///
/// Negative origins are clamped to 0 rather than shifting the extent, so a
/// rectangle straddling the screen edge over-approximates by the clipped
/// amount. Over-approximating costs a redundant tile; under-approximating
/// would drop a changed tile from the frame.
fn rect_to_tiles(x: i16, y: i16, width: u16, height: u16, out: &mut HashSet<(u32, u32)>) {
    let x = x.max(0) as u32;
    let y = y.max(0) as u32;
    let w = u32::from(width);
    let h = u32::from(height);

    let tx_start = x / TILE_SIZE;
    let ty_start = y / TILE_SIZE;
    let tx_end = (x + w).div_ceil(TILE_SIZE);
    let ty_end = (y + h).div_ceil(TILE_SIZE);

    for ty in ty_start..ty_end {
        for tx in tx_start..tx_end {
            out.insert((tx, ty));
        }
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod rect_tests {
    use super::*;

    fn tiles(x: i16, y: i16, w: u16, h: u16) -> Vec<(u32, u32)> {
        let mut out = HashSet::new();
        rect_to_tiles(x, y, w, h, &mut out);
        let mut v: Vec<_> = out.into_iter().collect();
        v.sort_unstable();
        v
    }

    #[test]
    fn a_rect_inside_one_tile_dirties_only_that_tile() {
        assert_eq!(tiles(1, 1, 2, 2), vec![(0, 0)]);
        // TILE_SIZE is 32: (40, 40) sits in tile (1, 1).
        assert_eq!(tiles(40, 40, 2, 2), vec![(1, 1)]);
    }

    #[test]
    fn a_rect_exactly_filling_a_tile_does_not_spill_into_the_next() {
        // The off-by-one that matters: 0..32 is tile 0 alone, and the
        // div_ceil must not round 32 up to tile 1. An extra tile here would
        // be invisible in output (just redundant work); the mirrored error on
        // the start edge would drop a changed tile.
        assert_eq!(tiles(0, 0, 32, 32), vec![(0, 0)]);
        assert_eq!(tiles(32, 32, 32, 32), vec![(1, 1)]);
    }

    #[test]
    fn a_rect_crossing_a_boundary_dirties_both_tiles() {
        assert_eq!(tiles(31, 0, 2, 1), vec![(0, 0), (1, 0)]);
        assert_eq!(tiles(0, 31, 1, 2), vec![(0, 0), (0, 1)]);
    }

    #[test]
    fn a_rect_spanning_several_tiles_dirties_the_whole_block() {
        // 0..65 x 0..33 covers tiles 0..3 across and 0..2 down.
        assert_eq!(
            tiles(0, 0, 65, 33),
            vec![(0, 0), (0, 1), (1, 0), (1, 1), (2, 0), (2, 1),]
        );
    }

    #[test]
    fn a_negative_origin_is_clamped_and_over_approximates() {
        // Clamping the origin without shrinking the extent reports one tile
        // more than was really damaged. Deliberate: see `rect_to_tiles`.
        // Under-approximating would drop a changed tile from the frame.
        assert_eq!(tiles(-10, 0, 10, 1), vec![(0, 0)]);
        assert_eq!(tiles(-10, 0, 40, 1), vec![(0, 0), (1, 0)]);
    }

    #[test]
    fn a_zero_area_rect_dirties_nothing_on_a_tile_boundary() {
        assert!(tiles(0, 0, 0, 0).is_empty());
        assert!(tiles(32, 32, 0, 0).is_empty());
    }
}

/// Tests that need a live X server on `$DISPLAY`.
///
/// They create and destroy a Damage object on the real root window and draw
/// nothing, so running them against a developer's own session is invisible --
/// a Damage object is a per-client server resource.
///
/// CI has no X server, so the `unit` job runs `cargo test --workspace --bins`
/// under `xvfb-run`. The skip belongs in the workflow, never here: a runtime
/// self-skip would mean a developer who breaks this never sees it fail.
#[cfg(test)]
mod x11_tests {
    use super::*;

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
        let mut m = XDamageMonitor::new().expect("XDamage extension on $DISPLAY");
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
        let mut m = XDamageMonitor::new().expect("XDamage extension on $DISPLAY");

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
    fn x11_drain_is_empty_while_disarmed() {
        let m = XDamageMonitor::new().expect("XDamage extension on $DISPLAY");
        assert!(
            m.drain_damage().is_empty(),
            "a monitor that was never armed cannot have damage to report"
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

        let mut m = XDamageMonitor::new().expect("XDamage extension on $DISPLAY");
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

        let tiles = m.drain_damage();
        assert!(
            !tiles.is_empty(),
            "arming produced no damage: the capture loop would silently fall \
             back to full-frame comparison forever"
        );
        assert!(
            tiles.contains(&(1, 1)),
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
}
