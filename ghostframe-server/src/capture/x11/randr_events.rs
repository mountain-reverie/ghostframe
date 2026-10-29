//! Records the RandR notifications a window manager receives.
//!
//! A window manager learns about a mode change only through these events, so
//! "indistinguishable from `xrandr`" (ghostframe-server design, section 4) is
//! checked by recording them: once around `xrandr --output O --mode M`, once
//! around `XrandrDisplay::set_output`, and comparing.
//!
//! Shared by the `randr_probe` example (the live investigation on a real
//! desktop) and the display tests (on a private Xvfb).

use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt as _, EventMask};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;

/// One notification, reduced to the fields a window manager acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RandrEvent {
    Screen {
        width: u16,
        height: u16,
        mm_width: u16,
        mm_height: u16,
    },
    Crtc {
        crtc: u32,
        mode: u32,
        x: i16,
        y: i16,
        width: u16,
        height: u16,
    },
    Output {
        output: u32,
        crtc: u32,
        mode: u32,
    },
    OutputProperty {
        output: u32,
        atom: u32,
    },
    RootConfigure {
        width: u16,
        height: u16,
    },
}

/// A second X client that listens to root's RandR and structure events.
pub struct Recorder {
    conn: RustConnection,
}

impl Recorder {
    /// Connect to `display` (`None` = `$DISPLAY`) and subscribe.
    pub fn connect(display: Option<&str>) -> Result<Self, Box<dyn std::error::Error>> {
        let (conn, screen_num) = x11rb::connect(display)?;
        let root = conn.setup().roots[screen_num].root;
        // RandR 1.2+ notifications are only sent to a client that negotiated
        // the version first.
        conn.randr_query_version(1, 5)?.reply()?;
        conn.randr_select_input(
            root,
            randr::NotifyMask::SCREEN_CHANGE
                | randr::NotifyMask::CRTC_CHANGE
                | randr::NotifyMask::OUTPUT_CHANGE
                | randr::NotifyMask::OUTPUT_PROPERTY,
        )?
        .check()?;
        conn.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?
        .check()?;
        Ok(Self { conn })
    }

    /// Collect events until `quiet` passes without one, or `max` elapses.
    /// Each event carries its offset from the call.
    pub fn collect(
        &self,
        quiet: Duration,
        max: Duration,
    ) -> Result<Vec<(Duration, RandrEvent)>, Box<dyn std::error::Error>> {
        let start = Instant::now();
        let mut last = start;
        let mut out = Vec::new();
        loop {
            while let Some(ev) = self.conn.poll_for_event()? {
                last = Instant::now();
                if let Some(e) = classify(&ev) {
                    out.push((last.duration_since(start), e));
                }
            }
            let now = Instant::now();
            if now.duration_since(last) >= quiet || now.duration_since(start) >= max {
                return Ok(out);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// The events this module records; everything else is `None`.
pub fn classify(ev: &Event) -> Option<RandrEvent> {
    match ev {
        Event::RandrScreenChangeNotify(e) => Some(RandrEvent::Screen {
            width: e.width,
            height: e.height,
            mm_width: e.mwidth,
            mm_height: e.mheight,
        }),
        Event::RandrNotify(e) if e.sub_code == randr::Notify::CRTC_CHANGE => {
            let c = e.u.as_cc();
            Some(RandrEvent::Crtc {
                crtc: c.crtc,
                mode: c.mode,
                x: c.x,
                y: c.y,
                width: c.width,
                height: c.height,
            })
        }
        Event::RandrNotify(e) if e.sub_code == randr::Notify::OUTPUT_CHANGE => {
            let o = e.u.as_oc();
            Some(RandrEvent::Output {
                output: o.output,
                crtc: o.crtc,
                mode: o.mode,
            })
        }
        Event::RandrNotify(e) if e.sub_code == randr::Notify::OUTPUT_PROPERTY => {
            let p = e.u.as_op();
            Some(RandrEvent::OutputProperty {
                output: p.output,
                atom: p.atom,
            })
        }
        Event::ConfigureNotify(e) => Some(RandrEvent::RootConfigure {
            width: e.width,
            height: e.height,
        }),
        _ => None,
    }
}

/// The event stream with ids and timing dropped, one line per event: the
/// form two recordings are compared in.
pub fn shape(events: &[(Duration, RandrEvent)]) -> Vec<String> {
    events
        .iter()
        .map(|(_, e)| match *e {
            RandrEvent::Screen { width, height, .. } => format!("screen {width}x{height}"),
            RandrEvent::Crtc { mode: 0, .. } => "crtc off".to_string(),
            RandrEvent::Crtc { width, height, .. } => format!("crtc {width}x{height}"),
            RandrEvent::Output { mode: 0, .. } => "output off".to_string(),
            RandrEvent::Output { .. } => "output on".to_string(),
            RandrEvent::OutputProperty { .. } => "output property".to_string(),
            RandrEvent::RootConfigure { width, height } => format!("root {width}x{height}"),
        })
        .collect()
}
