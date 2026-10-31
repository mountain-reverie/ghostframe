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
    /// The damaged tiles as coordinates. Only `Tiles` is a hint; `Unknown`
    /// and `None` both stay `None` ("compare every tile"). Used by the
    /// baseline harness; the library is told through `FrameSubmission::damage`.
    #[cfg(test)]
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

    /// The screen size in pixels the next tick will most likely grab, or
    /// `None` if the backend cannot say (it then cannot consume damage).
    fn screen_size(&self) -> Option<(u32, u32)> {
        None
    }

    /// Tell the next [`Backend::capture`] what this tick's damage says, so a
    /// backend that can grab less than the whole screen does. `damage` is
    /// the drain just made, `slice` the sweep slice due, `trusted` whether
    /// hints may be believed at all. Without a call, `capture` grabs
    /// everything; the default ignores the call.
    fn plan_grab(&mut self, _damage: &Damage, _slice: Option<safety_net::Slice>, _trusted: bool) {}

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

/// One capture tick that consumes damage (`damage_mode = on`): drain, let
/// the safety net plan the tick, tell the backend, grab.
///
/// The net is told the tick before the grab (it is what schedules the sweep
/// slice and says whether hints are believed). Falls back to
/// [`damage_then_grab`] for a backend that cannot report its screen size.
#[cfg(test)]
pub fn damage_plan_grab<B: Backend + ?Sized>(
    backend: &mut B,
    net: &mut safety_net::SafetyNet,
    timestamp_us: u32,
) -> (Damage, std::io::Result<FrameSubmission>) {
    damage_net_grab(backend, net, timestamp_us, true)
}

/// [`damage_plan_grab`] with the choice of acting on the plan. With `act`
/// false (`shadow`) the net sees the tick before the grab exactly as in `on`,
/// so both modes count a failed grab's tick the same way, but the backend
/// still grabs the whole screen.
pub fn damage_net_grab<B: Backend + ?Sized>(
    backend: &mut B,
    net: &mut safety_net::SafetyNet,
    timestamp_us: u32,
    act: bool,
) -> (Damage, std::io::Result<FrameSubmission>) {
    let Some((w, h)) = backend.screen_size() else {
        return damage_then_grab(backend, timestamp_us);
    };
    let damage = backend.drain_damage();
    let armed = backend.damage_armed();
    let plan = net.begin_tick(timestamp_us, &damage, w, h, armed);
    if act {
        backend.plan_grab(
            &damage,
            plan.slice,
            plan.trust == safety_net::Trust::Trusted,
        );
    }
    let frame = backend.capture(timestamp_us);
    (damage, frame)
}
