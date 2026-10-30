//! The X11 backend: full-root frames, XDamage, XTest input and RandR.

#[cfg(test)]
pub(crate) mod baseline;
pub mod cvt;
pub mod damage;
pub mod display;
pub mod frames;
pub mod input;
#[cfg(test)]
pub(crate) mod randr_events;
#[cfg(test)]
pub(crate) mod test_support;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostframe_lib::transport::display::DisplayController;
use ghostframe_lib::transport::input_inject::InputInjector;
use ghostframe_lib::FrameSubmission;

use crate::capture::{Damage, TileBitmap};
use crate::config::ServerConfig;

/// The X11 desktop on `$DISPLAY`.
pub struct X11Backend {
    frames: frames::X11Frames,
    damage: Option<damage::XDamageMonitor>,
    input: Option<Arc<dyn InputInjector>>,
    display: Option<Arc<dyn DisplayController>>,
    /// Allocations of `drain_damage` (the frames own theirs).
    stats: crate::capture::tick_stats::TickStats,
}

impl X11Backend {
    /// Connect every part. Frames are required; damage, input and display
    /// control are each optional and logged when missing, so a desktop that
    /// lacks one still streams.
    ///
    /// An error of kind `Unsupported` means the session can never be
    /// captured (see `frames::check_pixel_format`); every other kind may be
    /// transient. `capture` exits 2 on the first and 1 on the rest.
    pub fn new(cfg: &ServerConfig) -> io::Result<Self> {
        let frames =
            frames::X11Frames::connect(cfg.display.as_deref(), cfg.dump_first_frame.clone())?;

        // Not armed yet: damage reporting is switched on and off with the
        // capture gate, so the X server does no damage bookkeeping for us
        // while no client is connected.
        let damage = damage::XDamageMonitor::new(cfg.display.as_deref(), cfg.damage_report);
        if let Some(d) = &damage {
            tracing::info!(
                report = ?d.report_level(),
                "XDamage available; armed only while a client is connected"
            );
        } else {
            tracing::info!("XDamage not available, using full-frame dirty detection");
        }

        // If the XTEST extension isn't compiled into this X server, log and
        // continue: frames still stream, just no input.
        let input: Option<Arc<dyn InputInjector>> = match input::XTestInjector::new(
            cfg.display.as_deref(),
        ) {
            Ok(inj) => {
                tracing::info!("XTest input injector ready");
                Some(Arc::new(inj))
            }
            Err(e) => {
                tracing::warn!(error = %e, "XTest injector unavailable; input forwarding disabled");
                None
            }
        };

        // RandR is an ordinary, unprivileged X client operation. If it is
        // unavailable (extension missing, no connected output), continue with
        // no display controller: a fixed resolution beats refusing to start.
        let display: Option<Arc<dyn DisplayController>> = match display::XrandrDisplay::new(
            cfg.display.as_deref(),
            cfg.max_resolution.clone(),
            cfg.randr_settle,
        ) {
            Ok(ctl) => {
                tracing::info!("XRandR display controller ready");
                Some(Arc::new(ctl))
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "XRandR display controller unavailable; resolution negotiation disabled"
                );
                None
            }
        };

        Ok(Self {
            frames,
            damage,
            input,
            display,
            stats: Default::default(),
        })
    }
}

impl super::Backend for X11Backend {
    fn input_injector(&self) -> Option<Arc<dyn InputInjector>> {
        self.input.clone()
    }

    fn display_controller(&self) -> Option<Arc<dyn DisplayController>> {
        self.display.clone()
    }

    fn capture(&mut self, timestamp_us: u32) -> io::Result<FrameSubmission> {
        self.frames.capture(timestamp_us)
    }

    fn arm_damage(&mut self) {
        if let Some(d) = self.damage.as_mut() {
            d.arm();
        }
    }

    fn disarm_damage(&mut self) {
        if let Some(d) = self.damage.as_mut() {
            d.disarm();
        }
    }

    fn damage_armed(&self) -> bool {
        self.damage.as_ref().is_some_and(|d| d.is_armed())
    }

    /// Drained damage plus the cursor's tiles.
    ///
    /// The lib does not consume these hints today (see
    /// `Damage::into_hint`); the cursor merge is here for the day it does.
    fn take_tick_stats(&mut self) -> crate::capture::tick_stats::TickStats {
        let mut s = std::mem::take(&mut self.stats);
        s.add(self.frames.take_stats());
        s
    }

    fn drain_damage(&mut self) -> Damage {
        // The cursor is blended into every frame but X reports no damage for
        // it. Poll it on every drain (it is also the cursor `capture` then
        // draws) and union its old and new tiles into the damage; otherwise a
        // moved pointer leaves a trail whenever something else is damaged.
        let cursor_tiles = self.frames.poll_cursor();
        let (w, h) = self.frames.size();
        if cursor_tiles.capacity() > 0 {
            self.stats.alloc();
        }
        let Some(d) = self.damage.as_mut() else {
            return Damage::Unknown;
        };
        let drained = d.drain_damage(w, h);
        self.stats.allocs += d.take_allocs();
        merge_cursor_damage(drained, &cursor_tiles, (w, h), &mut self.stats)
    }
}

/// The drained damage with the cursor's tiles folded in. `Unknown` stays
/// `Unknown`; a moved cursor turns `None` into `Tiles`, because the cursor is
/// part of the pixels. `size` is the screen the cursor tiles refer to.
fn merge_cursor_damage(
    damage: Damage,
    cursor: &[(u32, u32)],
    size: (u32, u32),
    stats: &mut crate::capture::tick_stats::TickStats,
) -> Damage {
    let mut bitmap = match damage {
        Damage::Unknown => return Damage::Unknown,
        Damage::None if cursor.is_empty() => return Damage::None,
        Damage::None => {
            stats.alloc(); // the bitmap
            TileBitmap::new(size.0, size.1)
        }
        Damage::Tiles(b) => b,
    };
    for &(tx, ty) in cursor {
        bitmap.set(tx, ty);
    }
    Damage::Tiles(bitmap)
}

/// Block until `display` (`None` = `$DISPLAY`) accepts an X11 connection, or
/// the timeout expires.
///
/// `After=graphical-session.target` says nothing about whether the user's
/// session will accept a new client. Without this gate the server would bring
/// up tsnet (a ~5 s operation) against an X server it cannot talk to, and the
/// only symptom downstream would be "no frames".
pub fn wait_for_x11(timeout: Duration, display: Option<&str>) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    let mut attempt: u32 = 0;
    let last_err = loop {
        attempt += 1;
        let err = match x11rb::connect(display) {
            Ok(_) => {
                if attempt > 1 {
                    tracing::info!(attempts = attempt, "X11 display ready");
                }
                return Ok(());
            }
            Err(e) => e.to_string(),
        };
        if Instant::now() >= deadline {
            break err;
        }
        std::thread::sleep(Duration::from_millis(250));
    };
    let display = display.unwrap_or("(unset)");
    Err(format!(
        "X11 display {display} not ready after {timeout:?} ({attempt} attempt(s)); \
         last error: {last_err}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::x11::test_support::PrivateXvfb;
    use crate::capture::Backend;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{
        ChangeWindowAttributesAux, ConnectionExt, CreateGCAux, Rectangle,
    };

    #[test]
    fn wait_for_x11_connects_to_the_display_it_was_given() {
        // The process environment's DISPLAY (xvfb-run sets one) must not
        // decide the outcome: a bogus argument fails, a real one succeeds.
        let x = PrivateXvfb::spawn(&["-screen", "0", "64x48x24"]);
        wait_for_x11(Duration::from_millis(10), Some(x.display())).unwrap();
        let err = wait_for_x11(Duration::from_millis(10), Some(":48000")).unwrap_err();
        assert!(err.contains(":48000"), "{err}");
    }

    /// Damage hints at the backend level: a pointer warp raises no damage,
    /// so with other damage present the hint must still cover the tiles the
    /// cursor left and entered.
    #[test]
    fn drain_damage_covers_the_cursor_tiles_alongside_real_damage() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "256x256x24"]);
        let d = x.display();
        let mut backend = X11Backend {
            frames: frames::X11Frames::connect(Some(d), None).unwrap(),
            damage: damage::XDamageMonitor::new(Some(d), crate::config::DamageReport::NonEmpty),
            input: None,
            display: None,
            stats: Default::default(),
        };
        backend.arm_damage();
        assert!(backend.damage_armed());

        let (conn, n) = x11rb::connect(Some(d)).unwrap();
        let root = conn.setup().roots[n].root;
        let warp = |px: i16, py: i16| {
            conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, px, py)
                .unwrap();
            conn.get_input_focus().unwrap().reply().unwrap();
        };

        // Draw the cursor at (10,10), then prime: drop everything so far.
        warp(10, 10);
        backend.capture(0).unwrap();
        let first = backend.drain_damage();
        assert_eq!(first, Damage::Unknown, "the first drain after arming");
        backend.capture(0).unwrap();
        let _ = backend.drain_damage();

        // Real damage in tile (7,7), and a silent pointer move.
        let gc = conn.generate_id().unwrap();
        conn.create_gc(gc, root, &CreateGCAux::new().foreground(0xffffff))
            .unwrap();
        conn.poly_fill_rectangle(
            root,
            gc,
            &[Rectangle {
                x: 230,
                y: 230,
                width: 10,
                height: 10,
            }],
        )
        .unwrap();
        conn.get_input_focus().unwrap().reply().unwrap();
        warp(100, 70);

        let Damage::Tiles(hint) = backend.drain_damage() else {
            panic!("damage was reported");
        };
        for (tx, ty) in [(7, 7), (0, 0), (3, 2)] {
            assert!(hint.get(tx, ty), "({tx},{ty}) missing from {hint:?}");
        }
    }

    /// An unmoved cursor is no damage; a new cursor image in the same place
    /// is. The two glyph cursors below have the same size and hotspot, so only
    /// the image serial tells them apart: drop the serial from the unchanged
    /// check and the swap goes unnoticed.
    #[test]
    fn x11_a_cursor_image_swap_in_place_is_damage_and_standing_still_is_not() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "256x256x24"]);
        let d = x.display();
        let mut backend = X11Backend {
            frames: frames::X11Frames::connect(Some(d), None).unwrap(),
            damage: damage::XDamageMonitor::new(Some(d), crate::config::DamageReport::NonEmpty),
            input: None,
            display: None,
            stats: Default::default(),
        };
        backend.arm_damage();

        let (conn, n) = x11rb::connect(Some(d)).unwrap();
        let root = conn.setup().roots[n].root;
        let font = conn.generate_id().unwrap();
        conn.open_font(font, b"cursor").unwrap();
        let glyph_cursor = |conn: &x11rb::rust_connection::RustConnection| {
            let c = conn.generate_id().unwrap();
            // XC_left_ptr and its mask, black on white.
            conn.create_glyph_cursor(c, font, font, 68, 69, 0, 0, 0, 0xffff, 0xffff, 0xffff)
                .unwrap();
            c
        };
        let (a, b) = (glyph_cursor(&conn), glyph_cursor(&conn));
        let settle = || {
            conn.get_input_focus().unwrap().reply().unwrap();
        };
        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().cursor(a))
            .unwrap();
        conn.warp_pointer(x11rb::NONE, root, 0, 0, 0, 0, 100, 70)
            .unwrap();
        settle();

        assert_eq!(backend.drain_damage(), Damage::Unknown);
        backend.capture(0).unwrap();
        assert_eq!(
            backend.drain_damage(),
            Damage::None,
            "a cursor that did not move or change is not damage"
        );
        backend.capture(1).unwrap();

        conn.change_window_attributes(root, &ChangeWindowAttributesAux::new().cursor(b))
            .unwrap();
        settle();
        let Damage::Tiles(t) = backend.drain_damage() else {
            panic!("a new cursor image in the same place went unnoticed");
        };
        assert!(t.get(3, 2), "the cursor's tile is missing from {t:?}");
    }

    #[test]
    fn cursor_tiles_join_the_damage() {
        let mut b = TileBitmap::new(256, 256);
        b.set(5, 5);
        b.set(1, 1);
        let Damage::Tiles(m) = merge_cursor_damage(
            Damage::Tiles(b),
            &[(1, 1), (0, 0)],
            (256, 256),
            &mut Default::default(),
        ) else {
            panic!("tiles stay tiles");
        };
        assert_eq!(m.iter().collect::<Vec<_>>(), vec![(0, 0), (1, 1), (5, 5)]);
    }

    #[test]
    fn unknown_stays_unknown_and_quiet_stays_quiet() {
        assert_eq!(
            merge_cursor_damage(
                Damage::Unknown,
                &[(0, 0)],
                (64, 64),
                &mut Default::default()
            ),
            Damage::Unknown
        );
        assert_eq!(
            merge_cursor_damage(Damage::None, &[], (64, 64), &mut Default::default()),
            Damage::None
        );
    }

    #[test]
    fn a_moved_cursor_alone_is_damage() {
        // The cursor is part of the pixels: with no X damage but a moved
        // pointer the old and new tiles changed.
        let Damage::Tiles(m) = merge_cursor_damage(
            Damage::None,
            &[(0, 0), (3, 2)],
            (256, 256),
            &mut Default::default(),
        ) else {
            panic!("a cursor move is damage");
        };
        assert_eq!(m.count(), 2);
    }
}
