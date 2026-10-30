//! Capture backends.
//!
//! A backend bundles everything `GhostframeServer` consumes from the desktop
//! it serves: frames, damage, an optional input injector and an optional display controller.
//! X11 is the only one today; a PipeWire/Wayland backend implements the same
//! trait later (design: `docs/superpowers/specs/2026-10-29-ghostframe-server-x11-design.md`).

pub mod tick_stats;
pub mod x11;

use std::sync::Arc;

use ghostframe_lib::transport::display::DisplayController;
use ghostframe_lib::transport::input_inject::InputInjector;
use ghostframe_lib::FrameSubmission;

/// One desktop to serve.
pub trait Backend {
    /// Input injection, or `None` if the desktop offers none (frames still
    /// stream; input is dropped).
    fn input_injector(&self) -> Option<Arc<dyn InputInjector>>;

    /// Resolution control, or `None` (the session keeps its size).
    fn display_controller(&self) -> Option<Arc<dyn DisplayController>>;

    /// Grab one frame. `damage_tiles` is left `None`; the capture loop fills
    /// it from [`Backend::drain_damage`].
    fn capture(&mut self, timestamp_us: u32) -> std::io::Result<FrameSubmission>;

    /// Start damage reporting (a client connected). No-op without support.
    fn arm_damage(&mut self);

    /// Stop damage reporting (no client). No-op without support.
    fn disarm_damage(&mut self);

    /// Whether damage reporting is live: the one operator-visible sign that
    /// it has not silently failed.
    fn damage_armed(&self) -> bool;

    /// Tiles damaged since the last call, or `None` for "unknown: check
    /// every tile" (no damage support, or no event since the last call).
    fn drain_damage(&mut self) -> Option<Vec<(u32, u32)>>;

    /// Bytes copied and allocations made at the counted sites since the last
    /// call (see [`tick_stats::TickStats`]); resets them. Backends that do
    /// not count return zeros.
    fn take_tick_stats(&mut self) -> tick_stats::TickStats {
        tick_stats::TickStats::default()
    }
}
