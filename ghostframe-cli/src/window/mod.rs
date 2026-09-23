//! The seam between the client library and a display server.
//!
//! Two backends implement this: Wayland (smithay-client-toolkit) and X11
//! (x11rb + DRI3). They are deliberately NOT symmetric in maintenance cost --
//! Wayland is mostly third-party, X11 is hand-written -- and that is accepted
//! rather than equalised.

use std::os::fd::RawFd;

use ghostframe_client_native::PublishedFrame;

use crate::geometry::Placement;

/// What the showcase needs from a display server.
pub trait Backend {
    /// Present `frame` at `placement`.
    ///
    /// The dmabuf fd and plane layout come straight from the library, which
    /// recycles a small fixed set of buffers -- cache any per-`buffer_id`
    /// import rather than re-importing per frame.
    fn present(&mut self, frame: &PublishedFrame, placement: &Placement)
        -> Result<(), WindowError>;

    /// Drain input and window events since the last call. Must not block.
    fn poll_events(&mut self) -> Result<Vec<WindowEvent>, WindowError>;

    fn minimize(&mut self) -> Result<(), WindowError>;

    /// Current surface size, for recomputing the `Placement`.
    fn output_size(&self) -> (u32, u32);

    /// A pollable fd, so the caller can wait on the display server and the
    /// client's event fd together instead of spinning.
    fn event_fd(&self) -> RawFd;
}

#[derive(Debug, Clone, PartialEq)]
pub enum WindowEvent {
    Key {
        keysym: u32,
        down: bool,
    },
    PointerMotion {
        x: i32,
        y: i32,
    },
    PointerButton {
        x: i32,
        y: i32,
        button: u8,
        down: bool,
    },
    Wheel {
        dx: i16,
        dy: i16,
    },
    Resized {
        width: u32,
        height: u32,
    },
    CloseRequested,
}

/// Errors surfaced by a [`Backend`] or by [`open`].
#[derive(Debug, thiserror::Error)]
pub enum WindowError {
    /// Neither `WAYLAND_DISPLAY` nor `DISPLAY` is set, so there is no
    /// display server to connect to. Names both variables: "no display" is
    /// a baffling message for a tool the caller just successfully logged
    /// into a tailnet with.
    #[error(
        "no display server found: neither WAYLAND_DISPLAY nor DISPLAY is set in the environment"
    )]
    NoDisplay,
    /// A display server was found, but this build can't talk to it yet.
    #[error("{0}")]
    Unsupported(String),
    /// The Wayland backend failed -- connecting, binding a required global,
    /// or a protocol-level error.
    #[error("wayland: {0}")]
    Wayland(String),
    /// A local I/O failure (fd setup, dmabuf import, ...).
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// Which backend [`open`] should construct, given what the environment
/// advertises. A pure function of the two display-server env vars, kept
/// separate from `open` so it can be unit-tested without touching real
/// process environment (and the env races that come with that under
/// parallel tests).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Selected {
    Wayland,
    X11,
}

fn select_backend(
    wayland_display: Option<&str>,
    display: Option<&str>,
) -> Result<Selected, WindowError> {
    if wayland_display.is_some() {
        Ok(Selected::Wayland)
    } else if display.is_some() {
        Ok(Selected::X11)
    } else {
        Err(WindowError::NoDisplay)
    }
}

/// Wayland if `WAYLAND_DISPLAY` is set, else X11 if `DISPLAY` is, else error.
pub fn open(_title: &str) -> Result<Box<dyn Backend>, WindowError> {
    let wayland_display = std::env::var("WAYLAND_DISPLAY").ok();
    let display = std::env::var("DISPLAY").ok();

    match select_backend(wayland_display.as_deref(), display.as_deref())? {
        // The Wayland backend lands in M2 Task 7.
        Selected::Wayland => Err(WindowError::Unsupported(
            "WAYLAND_DISPLAY is set but the Wayland backend is not implemented yet (M2 task 7)"
                .to_string(),
        )),
        // X11 lands in M2 Task 8. Left as an explicit error rather than a
        // stub `Backend` impl, so a caller can't mistake "compiles" for
        // "works".
        Selected::X11 => Err(WindowError::Unsupported(
            "DISPLAY is set but the X11 backend is not implemented yet (M2 task 8)".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn neither_var_set_names_both_in_the_error() {
        let err = select_backend(None, None).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("WAYLAND_DISPLAY"), "message was: {msg:?}");
        assert!(msg.contains("DISPLAY"), "message was: {msg:?}");
    }

    #[test]
    fn wayland_display_selects_wayland_even_if_display_is_also_set() {
        assert_eq!(
            select_backend(Some("wayland-0"), Some(":0")).unwrap(),
            Selected::Wayland
        );
    }

    #[test]
    fn only_display_selects_x11() {
        assert_eq!(select_backend(None, Some(":0")).unwrap(), Selected::X11);
    }

    #[test]
    fn only_wayland_display_selects_wayland() {
        assert_eq!(
            select_backend(Some("wayland-0"), None).unwrap(),
            Selected::Wayland
        );
    }
}
