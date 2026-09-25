//! Discover this machine's own display, so [`crate::commands::connect`] can
//! tell the server what it can show (`ghostframe_client_native::Config::display`)
//! and later negotiate a mode on resize
//! (`ghostframe_client_native::Client::request_display_mode`).
//!
//! ## Why the Wayland probe reads the scale and the X11 probe derives it
//!
//! Scale is authoritative; millimetres are advisory (see
//! `ghostframe_client_core::ClientDisplay`'s doc and the M4b design doc
//! §4.1/§7): a future headless-Wayland backend can set a head's scale but
//! not its physical size -- `wlr-output-management`'s `physical_size`
//! "cannot be changed by clients". `wl_output.scale` already gives the
//! scale directly, so [`probe_wayland`] uses it verbatim. X11's RandR has
//! no such concept -- `GetOutputInfo` gives only millimetres and a mode --
//! so [`scale_from_mm`] derives a scale from those instead.
//!
//! Both probes are independent, one-shot connections to the display
//! server, separate from the one [`crate::window::open`] keeps for the
//! session -- opening a second short-lived client connection to enumerate
//! outputs is an ordinary thing for a Wayland/X11 client to do, and keeps
//! this module decoupled from the [`crate::window::Backend`] it runs
//! alongside.
//!
//! ## What is and isn't covered by tests
//!
//! [`scale_from_mm`] is pure arithmetic and is unit-tested below.
//! [`probe_wayland`] and [`probe_x11`] talk to a real display server and
//! are NOT covered by any test in this crate -- there is no headless
//! Wayland/X11 server in this test binary's environment. They are
//! exercised only by actually running `ghostframe connect` under Wayland or
//! X11.

use ghostframe_client_native::ClientDisplay;

/// Reference DPI a `scale_milli` of `1000` (1.0x) corresponds to. Matches
/// the CSS/Wayland/every-desktop-environment convention of 96 DPI = 100%.
const UNITY_DPI: f64 = 96.0;

/// Plausible band for a display's DPI, used to decide whether millimetres
/// reported over RandR can be trusted at all.
///
/// Below it: a huge physical size for the given pixel width is not a
/// monitor (a `mm` of exactly `0` -- the projector/TV case -- is handled
/// separately, before this band is even consulted). Above it: `mm` is too
/// small to be believed for the given pixel width (a confused driver, or a
/// phone-density panel misreporting as a desktop output). A normal desktop
/// monitor sits around 90-110 DPI; this errs wide (30-400) so a legitimate
/// but unusual display -- a large projected screen, a dense laptop panel --
/// is not mistaken for bad data.
const MIN_PLAUSIBLE_DPI: f64 = 30.0;
const MAX_PLAUSIBLE_DPI: f64 = 400.0;

const MM_PER_INCH: f64 = 25.4;

/// Derive a `scale_milli` (thousandths; `1000` = 1.0x) from a mode's pixel
/// width and an output's physical width in millimetres.
///
/// Falls back to `1000` (unity) whenever the millimetres can't be trusted:
/// `mm == 0` (projectors and many TVs report this -- dividing by it would
/// be undefined, and guessing a physical size would be worse than not
/// scaling at all), or the implied DPI falls outside
/// [`MIN_PLAUSIBLE_DPI`, `MAX_PLAUSIBLE_DPI`] (`mm` came from a confused or
/// lying driver, or this isn't a real display).
///
/// Rounds to the nearest thousandth, ties away from zero (`f64::round`'s
/// behaviour) -- there is no wire or UI reason to prefer one tie-breaking
/// direction over the other here.
pub fn scale_from_mm(px: u16, mm: u16) -> u16 {
    if mm == 0 {
        tracing::debug!(
            px,
            mm,
            "display_probe: zero millimetres, falling back to unity scale"
        );
        return 1000;
    }
    let inches = f64::from(mm) / MM_PER_INCH;
    let dpi = f64::from(px) / inches;
    if !(MIN_PLAUSIBLE_DPI..=MAX_PLAUSIBLE_DPI).contains(&dpi) {
        tracing::debug!(
            px,
            mm,
            dpi,
            "display_probe: implausible DPI, falling back to unity scale"
        );
        return 1000;
    }
    let scale_milli = (dpi / UNITY_DPI * 1000.0).round();
    // Bounded by the DPI plausibility check above (400 / 96 * 1000 ~=
    // 4166), always well inside u16's range -- the cast never truncates.
    scale_milli as u16
}

/// Probe the local display server for [`ClientDisplay`], preferring
/// Wayland over X11 the same way [`crate::window::open`] does (checking
/// `WAYLAND_DISPLAY` before `DISPLAY`) -- so what gets probed always
/// matches what the window backend actually opens.
///
/// `None` if neither display-server variable is set, or if the probe that
/// ran failed for any reason (no output found, the required protocol/
/// extension missing, a malformed reply, ...). Every failure is logged at
/// `warn`, naming which backend ran, before returning `None` -- a wrong or
/// missing scale is otherwise very hard to diagnose from the server side,
/// which only ever sees the final negotiated number.
pub fn probe() -> Option<ClientDisplay> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let result = probe_wayland();
        if result.is_none() {
            tracing::warn!("display_probe: wayland probe failed; no DisplayInfo will be sent");
        }
        return result;
    }
    if std::env::var_os("DISPLAY").is_some() {
        let result = probe_x11();
        if result.is_none() {
            tracing::warn!("display_probe: x11 probe failed; no DisplayInfo will be sent");
        }
        return result;
    }
    tracing::warn!(
        "display_probe: neither WAYLAND_DISPLAY nor DISPLAY is set; no DisplayInfo will be sent"
    );
    None
}

/// Probe over Wayland: a short-lived connection that binds only
/// `wl_output` (via SCTK's `OutputState`), reads the first output's
/// current mode and `wl_output.scale`, and disconnects.
///
/// Not `wp_fractional_scale_v1`: the CLI's Wayland backend
/// (`window::wayland`) does not bind that protocol today (it deliberately
/// presents 1:1 pixels -- see that module's doc), so using it here would
/// add a new protocol dependency for a value the integer `wl_output.scale`
/// already gives directly.
fn probe_wayland() -> Option<ClientDisplay> {
    use smithay_client_toolkit::output::{OutputHandler, OutputState};
    use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
    use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
    use wayland_client::globals::registry_queue_init;
    use wayland_client::protocol::wl_output;
    use wayland_client::{Connection, QueueHandle};

    struct ProbeState {
        registry_state: RegistryState,
        output_state: OutputState,
    }

    impl OutputHandler for ProbeState {
        fn output_state(&mut self) -> &mut OutputState {
            &mut self.output_state
        }
        fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
        fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {
        }
        fn output_destroyed(
            &mut self,
            _: &Connection,
            _: &QueueHandle<Self>,
            _: wl_output::WlOutput,
        ) {
        }
    }

    impl ProvidesRegistryState for ProbeState {
        fn registry(&mut self) -> &mut RegistryState {
            &mut self.registry_state
        }
        registry_handlers![OutputState];
    }

    delegate_registry!(ProbeState);
    delegate_dispatch2!(ProbeState);

    let conn = Connection::connect_to_env()
        .inspect_err(|e| tracing::warn!("wayland: connecting to the compositor: {e}"))
        .ok()?;
    let (globals, mut event_queue) = registry_queue_init::<ProbeState>(&conn)
        .inspect_err(|e| tracing::warn!("wayland: enumerating globals: {e}"))
        .ok()?;
    let qh = event_queue.handle();
    let mut state = ProbeState {
        registry_state: RegistryState::new(&globals),
        output_state: OutputState::new(&globals, &qh),
    };

    // One roundtrip: `OutputState::new` binds every `wl_output` global
    // `registry_queue_init` already saw; this flushes the geometry/mode/
    // scale/done events each bound output sends in response, which is what
    // turns `OutputState::info` from `None` into `Some`. Mirrors SCTK's own
    // `examples/list_outputs.rs`.
    event_queue
        .roundtrip(&mut state)
        .inspect_err(|e| tracing::warn!("wayland: roundtrip while probing outputs: {e}"))
        .ok()?;

    let output = state.output_state.outputs().next().or_else(|| {
        tracing::warn!("wayland: no wl_output advertised by the compositor");
        None
    })?;
    let info = state.output_state.info(&output).or_else(|| {
        tracing::warn!("wayland: output has no info (compositor never sent `done`?)");
        None
    })?;
    let mode = info.modes.iter().find(|m| m.current).or_else(|| {
        tracing::warn!("wayland: output advertised no current mode");
        None
    })?;

    let (width, height) = mode.dimensions;
    let (mm_width, mm_height) = info.physical_size;
    // `scale_factor` is a positive integer per protocol (1 = 100%); `max(1)`
    // is only a defence against a compositor that sends `0`, which would
    // otherwise collapse the display to a nonsensical zero scale.
    let scale_milli = u16::try_from(info.scale_factor.max(1).saturating_mul(1000)).unwrap_or(1000);

    // Not named `display`: that identifier collides with the field-value
    // helper `tracing::field::display` inside the `tracing::info!` call
    // below, which resolves the field value to the wrong `display` and
    // fails to compile with a baffling "no field `max_width`" error.
    let probed = ClientDisplay {
        max_width: u16::try_from(width).ok()?,
        max_height: u16::try_from(height).ok()?,
        scale_milli,
        mm_width: u16::try_from(mm_width.max(0)).unwrap_or(0),
        mm_height: u16::try_from(mm_height.max(0)).unwrap_or(0),
    };
    tracing::info!(
        max_width = probed.max_width,
        max_height = probed.max_height,
        scale_milli = probed.scale_milli,
        mm_width = probed.mm_width,
        mm_height = probed.mm_height,
        "display_probe: wayland probe succeeded"
    );
    Some(probed)
}

/// Probe over X11: a short-lived RandR query for the first connected
/// output with an active CRTC, mirroring `ghostframe-xdaemon`'s
/// `XrandrDisplay::new` (same extension check, same "first connected
/// output" choice) but read-only and one-shot -- this never applies a
/// mode, it only reads the current one.
fn probe_x11() -> Option<ClientDisplay> {
    use x11rb::connection::Connection as X11Connection;
    use x11rb::protocol::randr::{self, ConnectionExt as RandrConnectionExt};
    use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;

    let (conn, screen_num) = x11rb::connect(None)
        .inspect_err(|e| tracing::warn!("x11: connecting to the display server: {e}"))
        .ok()?;
    let root = conn.setup().roots[screen_num].root;

    let ext = conn
        .query_extension(b"RANDR")
        .ok()?
        .reply()
        .inspect_err(|e| tracing::warn!("x11: RandR query_extension reply: {e}"))
        .ok()?;
    if !ext.present {
        tracing::warn!("x11: RandR extension not present on this X server");
        return None;
    }

    let resources = conn
        .randr_get_screen_resources(root)
        .ok()?
        .reply()
        .inspect_err(|e| tracing::warn!("x11: RandR GetScreenResources reply: {e}"))
        .ok()?;

    for &output in &resources.outputs {
        let info = conn
            .randr_get_output_info(output, resources.config_timestamp)
            .ok()?
            .reply()
            .inspect_err(|e| tracing::warn!("x11: RandR GetOutputInfo reply: {e}"))
            .ok()?;
        if info.connection != randr::Connection::CONNECTED || info.crtc == 0 {
            continue;
        }
        let crtc_info = conn
            .randr_get_crtc_info(info.crtc, resources.config_timestamp)
            .ok()?
            .reply()
            .inspect_err(|e| tracing::warn!("x11: RandR GetCrtcInfo reply: {e}"))
            .ok()?;
        if crtc_info.width == 0 || crtc_info.height == 0 {
            continue;
        }

        let mm_width = u16::try_from(info.mm_width).unwrap_or(0);
        let mm_height = u16::try_from(info.mm_height).unwrap_or(0);
        // Not named `display` -- see the identical comment in `probe_wayland`.
        let probed = ClientDisplay {
            max_width: crtc_info.width,
            max_height: crtc_info.height,
            scale_milli: scale_from_mm(crtc_info.width, mm_width),
            mm_width,
            mm_height,
        };
        tracing::info!(
            max_width = probed.max_width,
            max_height = probed.max_height,
            scale_milli = probed.scale_milli,
            mm_width = probed.mm_width,
            mm_height = probed.mm_height,
            "display_probe: x11 probe succeeded"
        );
        return Some(probed);
    }

    tracing::warn!("x11: no connected RandR output with an active CRTC found");
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_standard_96_dpi_display_is_unity() {
        // 1920 px across 508 mm is exactly 96 DPI.
        assert_eq!(scale_from_mm(1920, 508), 1000);
    }

    #[test]
    fn derives_scale_from_a_normal_monitor() {
        // 2560 px across 597 mm is about 109 DPI -> ~1.13x
        let s = scale_from_mm(2560, 597);
        assert!((1100..=1200).contains(&s), "expected ~1.13, got {s}");
    }

    #[test]
    fn zero_millimetres_falls_back_to_unity() {
        // Projectors and many TVs report 0. Deriving from it would divide
        // by zero or produce an absurd DPI.
        assert_eq!(scale_from_mm(1920, 0), 1000);
    }

    #[test]
    fn an_implausible_dpi_falls_back_to_unity() {
        // 1920 px across 10 mm is not a display.
        assert_eq!(scale_from_mm(1920, 10), 1000);
    }
}
