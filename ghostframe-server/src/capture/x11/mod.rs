//! The X11 backend: full-root frames, XDamage, XTest input and RandR.

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

use crate::config::ServerConfig;

/// The X11 desktop on `$DISPLAY`.
pub struct X11Backend {
    frames: frames::X11Frames,
    damage: Option<damage::XDamageMonitor>,
    input: Option<Arc<dyn InputInjector>>,
    display: Option<Arc<dyn DisplayController>>,
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
        let damage = damage::XDamageMonitor::new(cfg.display.as_deref());
        if damage.is_some() {
            tracing::info!("XDamage available; armed only while a client is connected");
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
    /// The lib's GPU path does not consume these hints today (only the CPU
    /// comparison in `process_frame_cpu_at` reads `damage_tiles`); the cursor
    /// merge is here for the day it does.
    fn drain_damage(&mut self) -> Option<Vec<(u32, u32)>> {
        // The cursor is blended into every frame but X reports no damage for
        // it. Poll it on every drain (it is also the cursor `capture` then
        // draws) and union its old and new tiles into any hinted set;
        // otherwise a moved pointer leaves a trail whenever something else
        // is damaged.
        let cursor_tiles = self.frames.poll_cursor();
        // An empty drain means no damage events since the last call, but the
        // first frame for a newly connected client must still be compared in
        // full, so empty maps to `None` ("check every tile").
        let tiles = self.damage.as_ref()?.drain_damage();
        merge_cursor_damage(tiles, cursor_tiles)
    }
}

/// Hints for the lib: the drained damage plus the cursor's tiles, or `None`
/// ("compare every tile") when nothing was damaged. Cursor tiles alone never
/// make a hint: with no other change the first frame for a new client must
/// still be compared in full.
fn merge_cursor_damage(
    mut damage: Vec<(u32, u32)>,
    cursor: Vec<(u32, u32)>,
) -> Option<Vec<(u32, u32)>> {
    if damage.is_empty() {
        return None;
    }
    let mut seen: std::collections::HashSet<_> = damage.iter().copied().collect();
    for t in cursor {
        if seen.insert(t) {
            damage.push(t);
        }
    }
    Some(damage)
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
    use x11rb::protocol::xproto::{ConnectionExt, CreateGCAux, Rectangle};

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
            damage: damage::XDamageMonitor::new(Some(d)),
            input: None,
            display: None,
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

        let hint = backend.drain_damage().expect("damage was reported");
        for tile in [(7, 7), (0, 0), (3, 2)] {
            assert!(hint.contains(&tile), "{tile:?} missing from {hint:?}");
        }
    }

    #[test]
    fn cursor_tiles_join_the_damage_without_duplicates() {
        assert_eq!(
            merge_cursor_damage(vec![(5, 5), (1, 1)], vec![(1, 1), (0, 0)]),
            Some(vec![(5, 5), (1, 1), (0, 0)])
        );
    }

    #[test]
    fn no_damage_stays_unknown_whatever_the_cursor_did() {
        assert_eq!(merge_cursor_damage(vec![], vec![(0, 0)]), None);
    }
}
