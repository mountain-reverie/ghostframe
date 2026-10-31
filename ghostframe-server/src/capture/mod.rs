//! Capture backends.
//!
//! A backend bundles everything `GhostframeServer` consumes from the desktop
//! it serves: frames, damage, an optional input injector and an optional display controller.
//! X11 is the only one today; a PipeWire/Wayland backend implements the same
//! trait later (design: `docs/superpowers/specs/2026-10-29-ghostframe-server-x11-design.md`).

pub mod safety_net;
pub mod tick_stats;
pub mod tile_bitmap;
pub mod x11;

use std::sync::Arc;

use ghostframe_lib::transport::display::DisplayController;
use ghostframe_lib::transport::input_inject::InputInjector;
use ghostframe_lib::FrameSubmission;

pub use tile_bitmap::TileBitmap;

/// What a backend knows about the pixels that changed since the last
/// [`Backend::drain_damage`].
///
/// Three states, because "nothing changed", "I do not know" and "I never
/// could know" are different facts and a consumer that skips work on damage
/// must tell them apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Damage {
    /// No knowledge: compare everything. Reported for the first frame after
    /// damage reporting starts, after a resize, after any error or event
    /// flood, and when damage reporting is absent or not armed.
    Unknown,
    /// Reporting is live and nothing changed.
    None,
    /// Reporting is live; the changed pixels lie within these tiles (a
    /// superset of the truly changed ones).
    Tiles(TileBitmap),
}

impl Damage {
    /// [`Damage::into_hint`] for a frame of `width` x `height` pixels. A
    /// `Tiles` bitmap built for another screen size (a resize between the
    /// drain and the grab, even one that keeps the tile grid) says nothing
    /// about this frame and yields no hint.
    pub fn into_hint_for(self, width: u32, height: u32) -> Option<Vec<(u32, u32)>> {
        match self {
            Damage::Tiles(ref b) if !b.matches(width, height) => None,
            d => d.into_hint(),
        }
    }

    /// The hint in `FrameSubmission::damage_tiles` form. Only `Tiles` is a
    /// hint; `Unknown` and `None` both stay `None` ("compare every tile"),
    /// which is always safe. The library ignores the field today, so a
    /// literal "no tiles" is not forwarded.
    pub fn into_hint(self) -> Option<Vec<(u32, u32)>> {
        match self {
            Damage::Tiles(b) => Some(b.iter().collect()),
            Damage::Unknown | Damage::None => Option::None,
        }
    }
}

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

    /// The pool this backend's frames come from, if it has one. A capture
    /// loop awaits [`ghostframe_lib::FramePool::wait_free`] before each tick:
    /// `capture` is synchronous and fails rather than waits when every slot
    /// is still in flight.
    fn frame_pool(&self) -> Option<ghostframe_lib::FramePool> {
        None
    }

    /// Start damage reporting (a client connected). No-op without support.
    fn arm_damage(&mut self);

    /// Stop damage reporting (no client). No-op without support.
    fn disarm_damage(&mut self);

    /// Whether damage reporting is live: the one operator-visible sign that
    /// it has not silently failed.
    fn damage_armed(&self) -> bool;

    /// Damage since the last call; see [`Damage`].
    fn drain_damage(&mut self) -> Damage;

    /// Bytes copied and allocations made at the counted sites since the last
    /// call (see [`tick_stats::TickStats`]); resets them. Backends that do
    /// not count return zeros.
    fn take_tick_stats(&mut self) -> tick_stats::TickStats {
        tick_stats::TickStats::default()
    }
}

/// One capture tick: the damage first, then the grab.
///
/// The order is the correctness argument. Draining subtracts the damage
/// region; anything drawn after that is reported again next tick, so a draw
/// racing the grab is at worst grabbed early and repeated. Reversed, a draw
/// between the grab and the subtract is consumed by this tick's report while
/// missing from this tick's pixels, and nothing reports it again.
pub fn damage_then_grab<B: Backend + ?Sized>(
    backend: &mut B,
    timestamp_us: u32,
) -> (Damage, std::io::Result<FrameSubmission>) {
    let damage = backend.drain_damage();
    let frame = backend.capture(timestamp_us);
    (damage, frame)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A drain at one size and a grab at another that keeps the tile grid
    /// (512x512 and 500x500 are both 16x16 tiles) must not forward the hint.
    #[test]
    fn a_hint_for_another_screen_size_is_dropped_even_on_the_same_tile_grid() {
        let mut b = TileBitmap::new(512, 512);
        b.set(3, 3);
        assert_eq!(b.grid(), TileBitmap::new(500, 500).grid());
        assert_eq!(
            Damage::Tiles(b.clone()).into_hint_for(500, 500),
            None,
            "stale hint forwarded across a resize"
        );
        assert_eq!(Damage::Tiles(b).into_hint_for(512, 512), Some(vec![(3, 3)]));
    }
}
