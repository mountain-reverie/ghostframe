//! The X11 backend: full-root frames, XDamage, XTest input and RandR.

pub mod cvt;
pub mod damage;
pub mod display;
pub mod frames;
pub mod input;
#[cfg(test)]
pub(crate) mod test_support;

use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ghostframe_lib::transport::display::DisplayController;
use ghostframe_lib::transport::input_inject::InputInjector;
use ghostframe_lib::FrameSubmission;

use crate::config::XdaemonConfig;

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
    pub fn new(cfg: &XdaemonConfig) -> io::Result<Self> {
        let frames = frames::X11Frames::connect(None, cfg.dump_first_frame.clone())?;

        // Not armed yet: damage reporting is switched on and off with the
        // capture gate, so the X server does no damage bookkeeping for us
        // while no client is connected.
        let damage = damage::XDamageMonitor::new();
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
        let display: Option<Arc<dyn DisplayController>> =
            match display::XrandrDisplay::new(cfg.attach_max_resolution.clone()) {
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

    fn drain_damage(&mut self) -> Option<Vec<(u32, u32)>> {
        // An empty drain means no damage events since the last call, but the
        // first frame for a newly connected client must still be compared in
        // full, so empty maps to `None` ("check every tile").
        let tiles = self.damage.as_ref()?.drain_damage();
        if tiles.is_empty() {
            None
        } else {
            Some(tiles)
        }
    }
}

/// Block until `$DISPLAY` accepts an X11 connection, or the timeout expires.
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
        let err = match x11rb::connect(None) {
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
