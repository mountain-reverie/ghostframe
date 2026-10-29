//! `DisplayController` backed by X RandR.
//!
//! Implements `ghostframe_lib::transport::display::DisplayController`
//! (mirrors `input_inject::XTestInjector`, which implements `InputInjector`
//! for the same reason: keep the protocol/`IoBridge` layer testable without
//! a running display server). Everything X-specific -- RandR requests,
//! modelines, millimetres -- stays inside this module; the trait itself
//! must not gain an X concept. See the M4b design doc §5 and §7.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::rust_connection::RustConnection;

use ghostframe_lib::transport::display::{DisplayController, DisplayError};

use super::cvt;

/// `DisplayController` backed by X RandR.
///
/// Entirely unprivileged: RandR is an ordinary X client operation and the
/// daemon already holds the display. An earlier design injected a synthetic
/// EDID through a root helper; a spike proved the kernel ignores
/// `edid_override` on these connectors, because it consults the override
/// only when a driver's `get_modes` returns zero modes and both VKMS and
/// amdgpu's `virtual_display` use `drm_add_modes_noedid`. See the M4b
/// design doc §10.
pub struct XrandrDisplay {
    conn: RustConnection,
    root: u32,
    output: randr::Output,
    crtc: randr::Crtc,
    /// Fallback for `ceiling()` if a live `GetScreenSizeRange` query ever
    /// fails. Seeded from a real query in `new()` and refreshed on every
    /// subsequent successful call -- never a hardcoded guess.
    ceiling_fallback: Mutex<(u16, u16)>,
    /// `GHOSTFRAME_ATTACH_MAX_RESOLUTION`, read once at construction (this
    /// crate's convention -- see `main.rs`). `Some("WIDTHxHEIGHT")` lowers
    /// what `ceiling()` reports; `None` (unset) leaves the hardware ceiling
    /// alone. Applied by `clamp_ceiling()` on every path through `ceiling()`.
    max_resolution: Option<String>,
    /// The display this controller was connected to (`None` = `$DISPLAY`),
    /// so the read-back thread can open its own connection.
    display: Option<String>,
    /// How long after a change the read-back waits before looking again;
    /// zero disables it. See [`Self::spawn_readback`].
    settle: Duration,
    /// Bumped by every `set_output`; a read-back whose number is stale
    /// belongs to a superseded request and says nothing.
    generation: Arc<AtomicU64>,
    /// What the last read-back found when the desktop had overridden the
    /// request, `None` while the request stuck. Shared with the read-back
    /// threads; the tests observe it.
    overridden: Arc<Mutex<Option<Overridden>>>,
}

/// A mode change the desktop undid or altered after we applied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overridden {
    pub requested: (u16, u16),
    pub actual: (u16, u16),
}

impl XrandrDisplay {
    /// Connect to the local X server, confirm the RandR extension is
    /// present, and pick the first connected output with an available CRTC.
    ///
    /// Errors if no DISPLAY, RandR is missing, or there is no connected
    /// output -- the caller (ghostframe-server's `main`) logs and continues with no
    /// display controller, exactly like `XTestInjector::new`.
    pub fn new(
        display: Option<&str>,
        max_resolution: Option<String>,
        settle: Duration,
    ) -> anyhow::Result<Self> {
        Self::connect(display, max_resolution, settle)
    }

    /// [`Self::new`] under its older name; kept so the tests read the same.
    pub fn connect(
        display: Option<&str>,
        max_resolution: Option<String>,
        settle: Duration,
    ) -> anyhow::Result<Self> {
        use anyhow::{anyhow, Context};

        let (conn, screen_num) =
            x11rb::connect(display).map_err(|e| anyhow!("x11rb::connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;

        let ext = conn
            .query_extension(b"RANDR")
            .context("query_extension(RANDR)")?
            .reply()
            .context("RANDR query_extension reply")?;
        if !ext.present {
            return Err(anyhow!("RandR extension not present on this Xorg build"));
        }
        // RandR 1.2+ requests need the version negotiated first.
        conn.randr_query_version(1, 5)
            .context("RandR QueryVersion request")?
            .reply()
            .context("RandR QueryVersion reply")?;

        let resources = conn
            .randr_get_screen_resources(root)
            .context("RandR GetScreenResources request")?
            .reply()
            .context("RandR GetScreenResources reply")?;
        if resources.outputs.is_empty() {
            return Err(anyhow!("RandR reports zero outputs"));
        }

        let mut chosen: Option<(randr::Output, randr::Crtc)> = None;
        for &output in &resources.outputs {
            let info = conn
                .randr_get_output_info(output, resources.config_timestamp)
                .context("RandR GetOutputInfo request")?
                .reply()
                .context("RandR GetOutputInfo reply")?;
            if info.connection != randr::Connection::CONNECTED {
                continue;
            }
            let crtc = if info.crtc != 0 {
                info.crtc
            } else if let Some(&c) = info.crtcs.first() {
                c
            } else {
                continue;
            };
            chosen = Some((output, crtc));
            break;
        }
        let (output, crtc) = chosen
            .ok_or_else(|| anyhow!("no connected RandR output with an available CRTC found"))?;

        let range = conn
            .randr_get_screen_size_range(root)
            .context("RandR GetScreenSizeRange request")?
            .reply()
            .context("RandR GetScreenSizeRange reply")?;

        tracing::info!(
            output,
            crtc,
            max_width = range.max_width,
            max_height = range.max_height,
            "XrandrDisplay connected"
        );

        tracing::info!(
            max_resolution = ?max_resolution,
            "display controller ceiling clamp"
        );

        Ok(Self {
            conn,
            root,
            output,
            crtc,
            ceiling_fallback: Mutex::new((range.max_width, range.max_height)),
            max_resolution,
            display: display.map(str::to_owned),
            settle,
            generation: Arc::new(AtomicU64::new(0)),
            overridden: Arc::new(Mutex::new(None)),
        })
    }

    /// What the last read-back found, if the desktop overrode a request.
    #[cfg(test)]
    fn overridden(&self) -> Option<Overridden> {
        *self.overridden.lock().unwrap()
    }

    /// After a settle delay, look at the screen again. A window manager or
    /// desktop may restore its own saved mode some time after ours (on
    /// Enlightenment, e_randr2 does so about 1.7 s later), so a successful
    /// `set_output` does not mean the size stuck. This only observes: it
    /// never re-applies, because fighting the desktop would flicker the
    /// monitor forever. The client needs no separate notice -- capture polls
    /// the root geometry every frame and the frame size it reports follows.
    fn spawn_readback(&self, requested: (u16, u16)) {
        if self.settle.is_zero() {
            return;
        }
        let ticket = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        let generation = Arc::clone(&self.generation);
        let overridden = Arc::clone(&self.overridden);
        let display = self.display.clone();
        let (settle, crtc) = (self.settle, self.crtc);
        let spawned = std::thread::Builder::new()
            .name("randr-readback".into())
            .spawn(move || {
                std::thread::sleep(settle);
                if generation.load(Ordering::SeqCst) != ticket {
                    return; // a newer request owns the verdict
                }
                let actual = match read_actual(display.as_deref(), crtc) {
                    Ok(a) => a,
                    Err(e) => {
                        tracing::debug!(error = %e, "RandR read-back failed");
                        return;
                    }
                };
                let mut slot = overridden.lock().unwrap();
                if actual == requested {
                    *slot = None;
                    return;
                }
                let found = Overridden { requested, actual };
                if *slot != Some(found) {
                    tracing::info!(
                        "the desktop overrode the requested mode {}x{}; keeping the desktop's \
                         {}x{} (on Enlightenment this is e_randr2 restoring its saved monitor \
                         config)",
                        requested.0,
                        requested.1,
                        actual.0,
                        actual.1
                    );
                }
                *slot = Some(found);
            });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the RandR read-back thread");
        }
    }

    /// Live `GetScreenSizeRange` query, used by `ceiling()`. Split out so
    /// `ceiling()` (infallible per the trait) can fall back to the last
    /// known-good value on error rather than unwrapping.
    fn query_ceiling(&self) -> Result<(u16, u16), String> {
        let reply = self
            .conn
            .randr_get_screen_size_range(self.root)
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        Ok((reply.max_width, reply.max_height))
    }
}

impl DisplayController for XrandrDisplay {
    fn ceiling(&self) -> (u16, u16) {
        let hardware = match self.query_ceiling() {
            Ok(val) => {
                *self.ceiling_fallback.lock().unwrap() = val;
                val
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "RandR GetScreenSizeRange failed; using last known ceiling"
                );
                *self.ceiling_fallback.lock().unwrap()
            }
        };
        clamp_ceiling(hardware, self.max_resolution.as_deref())
    }

    fn set_output(&self, width: u16, height: u16, scale_milli: u16) -> Result<(), DisplayError> {
        let resources = self
            .conn
            .randr_get_screen_resources(self.root)
            .map_err(|e| DisplayError(format!("GetScreenResources request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetScreenResources reply: {e}")))?;

        let output_info = self
            .conn
            .randr_get_output_info(self.output, resources.config_timestamp)
            .map_err(|e| DisplayError(format!("GetOutputInfo request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetOutputInfo reply: {e}")))?;

        // `xrandr --output O --mode WxH` selects one of the output's own
        // modes. Do the same whenever one fits, so the window manager sees
        // a mode it already knows; only a size no mode offers gets a CVT
        // mode of our own.
        let timing = cvt::reduced_blanking(width, height, 60);
        let (mode, mode_w, mode_h) = match pick_existing_mode(
            &output_info.modes,
            output_info.num_preferred,
            &resources.modes,
            (width, height),
            (timing.h_active, timing.v_active),
        ) {
            Some(m) => (m.id, m.width, m.height),
            None => (
                self.create_cvt_mode(&resources, &timing)?,
                timing.h_active,
                timing.v_active,
            ),
        };

        let crtc_info = self
            .conn
            .randr_get_crtc_info(self.crtc, resources.config_timestamp)
            .map_err(|e| DisplayError(format!("GetCrtcInfo request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetCrtcInfo reply: {e}")))?;

        let screen_geom = self
            .conn
            .get_geometry(self.root)
            .map_err(|e| DisplayError(format!("GetGeometry request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetGeometry reply: {e}")))?;

        let set_crtc = |mode: randr::Mode, outputs: &[randr::Output]| -> Result<(), DisplayError> {
            let reply = self
                .conn
                .randr_set_crtc_config(
                    self.crtc,
                    x11rb::CURRENT_TIME,
                    resources.config_timestamp,
                    0,
                    0,
                    mode,
                    randr::Rotation::ROTATE0,
                    outputs,
                )
                .map_err(|e| DisplayError(format!("SetCrtcConfig request: {e}")))?
                .reply()
                .map_err(|e| DisplayError(format!("SetCrtcConfig reply: {e}")))?;
            if reply.status != randr::SetConfig::SUCCESS {
                return Err(DisplayError(format!(
                    "SetCrtcConfig failed: status={:?}",
                    reply.status
                )));
            }
            Ok(())
        };

        let set_screen = |w: u16, h: u16| -> Result<(), DisplayError> {
            let mm_w = mm_for_scale(w, scale_milli);
            let mm_h = mm_for_scale(h, scale_milli);
            self.conn
                .randr_set_screen_size(self.root, w, h, mm_w, mm_h)
                .map_err(|e| DisplayError(format!("SetScreenSize request: {e}")))?
                .check()
                .map_err(|e| DisplayError(format!("SetScreenSize: {e}")))
        };

        let steps = apply_steps(
            (screen_geom.width, screen_geom.height),
            crtc_info.mode,
            mode,
            (mode_w, mode_h),
        );
        let mut grabbed = false;
        let mut disabled = false;
        let result = (|| -> Result<(), DisplayError> {
            for step in &steps {
                match *step {
                    RandrStep::Grab => {
                        self.conn
                            .grab_server()
                            .map_err(|e| DisplayError(format!("GrabServer: {e}")))?;
                        grabbed = true;
                    }
                    RandrStep::DisableCrtc => {
                        set_crtc(x11rb::NONE, &[])?;
                        disabled = true;
                    }
                    RandrStep::SetScreen(w, h) => set_screen(w, h)?,
                    RandrStep::SetCrtc => {
                        set_crtc(mode, &[self.output])?;
                        disabled = false;
                    }
                    RandrStep::Ungrab => {
                        self.conn
                            .ungrab_server()
                            .map_err(|e| DisplayError(format!("UngrabServer: {e}")))?;
                        grabbed = false;
                    }
                }
            }
            Ok(())
        })();
        if result.is_err() && disabled {
            // Never leave the output dark: put the CRTC back the way it was.
            let _ = self.conn.randr_set_crtc_config(
                self.crtc,
                x11rb::CURRENT_TIME,
                resources.config_timestamp,
                crtc_info.x,
                crtc_info.y,
                crtc_info.mode,
                crtc_info.rotation,
                &crtc_info.outputs,
            );
        }
        if grabbed {
            let _ = self.conn.ungrab_server();
        }
        let _ = self.conn.flush();
        if result.is_ok() {
            self.spawn_readback((mode_w, mode_h));
        }
        result
    }
}

impl XrandrDisplay {
    /// Create (or find by name, after a BadName) and attach a CVT
    /// reduced-blanking mode for a size none of the output's modes offers.
    fn create_cvt_mode(
        &self,
        resources: &randr::GetScreenResourcesReply,
        timing: &cvt::Timing,
    ) -> Result<randr::Mode, DisplayError> {
        let (mode_w, mode_h) = (timing.h_active, timing.v_active);
        let name = format!("ghostframe_{mode_w}x{mode_h}");
        // CVT reduced blanking is +hsync -vsync (checked against `cvt -r`).
        let mode_info = randr::ModeInfo {
            id: 0,
            width: mode_w,
            height: mode_h,
            // ModeInfo.dot_clock is Hz; `Timing::pixel_clock_khz` is kHz.
            dot_clock: timing.pixel_clock_khz * 1000,
            hsync_start: timing.h_sync_start,
            hsync_end: timing.h_sync_end,
            htotal: timing.h_total,
            hskew: 0,
            vsync_start: timing.v_sync_start,
            vsync_end: timing.v_sync_end,
            vtotal: timing.v_total,
            name_len: name.len() as u16,
            mode_flags: randr::ModeFlag::HSYNC_POSITIVE | randr::ModeFlag::VSYNC_NEGATIVE,
        };
        let mode_id = match self
            .conn
            .randr_create_mode(self.root, mode_info, name.as_bytes())
            .map_err(|e| DisplayError(format!("CreateMode request: {e}")))?
            .reply()
        {
            Ok(created) => created.mode,
            // BadName: this size was created earlier and later detached from
            // every output. Reuse it by name rather than failing forever.
            Err(x11rb::errors::ReplyError::X11Error(ref e))
                if e.error_kind == x11rb::protocol::ErrorKind::Name =>
            {
                find_mode_by_name(resources, name.as_bytes()).ok_or_else(|| {
                    DisplayError(format!(
                        "CreateMode: BadName for {name:?} but no matching global mode found"
                    ))
                })?
            }
            Err(e) => return Err(DisplayError(format!("CreateMode reply: {e}"))),
        };
        self.conn
            .randr_add_output_mode(self.output, mode_id)
            .map_err(|e| DisplayError(format!("AddOutputMode request: {e}")))?
            .check()
            .map_err(|e| DisplayError(format!("AddOutputMode: {e}")))?;
        Ok(mode_id)
    }
}

/// The size the desktop shows now: the screen (what capture grabs) when the
/// CRTC's own size agrees with it, otherwise whichever disagrees, screen
/// first. Opens its own connection, so it can run from any thread.
fn read_actual(display: Option<&str>, crtc: randr::Crtc) -> anyhow::Result<(u16, u16)> {
    use anyhow::Context;
    let (conn, n) = x11rb::connect(display).context("connect")?;
    let root = conn.setup().roots[n].root;
    let geom = conn.get_geometry(root)?.reply().context("GetGeometry")?;
    let screen = (geom.width, geom.height);
    let info = conn
        .randr_get_crtc_info(crtc, 0)?
        .reply()
        .context("GetCrtcInfo")?;
    // A disabled CRTC has no size of its own; the screen is all there is.
    if info.mode == x11rb::NONE || (info.width, info.height) == screen {
        Ok(screen)
    } else {
        Ok((info.width, info.height))
    }
}

/// One request of a mode change, in the order `xrandr(1)` issues them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RandrStep {
    Grab,
    DisableCrtc,
    SetScreen(u16, u16),
    SetCrtc,
    Ungrab,
}

/// The requests for a mode change, ordered as `xrandr --output O --mode M`
/// orders them. Recorded on Xvfb (2026-10-29, xrandr 1.5) for both a grow
/// and a shrink: `crtc 0: disable`, then `screen 0: WxH`, then
/// `crtc 0: <mode>`, all inside a server grab. Disabling first makes every
/// screen size legal, so there is no grow/shrink/mixed special case.
///
/// When the CRTC already shows the target mode and the screen already has
/// the target size, only the physical size can differ (a scale change), and
/// SetScreenSize alone carries it -- the output is not blanked for nothing.
fn apply_steps(
    screen: (u16, u16),
    current_mode: randr::Mode,
    target_mode: randr::Mode,
    target: (u16, u16),
) -> Vec<RandrStep> {
    if current_mode == target_mode && screen == target {
        return vec![RandrStep::SetScreen(target.0, target.1)];
    }
    let mut steps = vec![RandrStep::Grab];
    if current_mode != x11rb::NONE {
        steps.push(RandrStep::DisableCrtc);
    }
    steps.push(RandrStep::SetScreen(target.0, target.1));
    steps.push(RandrStep::SetCrtc);
    steps.push(RandrStep::Ungrab);
    steps
}

/// The output mode to use for a `want` request, if the output already has
/// one: an exact `want` size first, then the CVT-rounded `cvt` size. Among
/// equal sizes a preferred mode (the first `num_preferred` of
/// `output_modes`) wins, then the highest refresh rate.
fn pick_existing_mode(
    output_modes: &[randr::Mode],
    num_preferred: u16,
    all_modes: &[randr::ModeInfo],
    want: (u16, u16),
    cvt: (u16, u16),
) -> Option<randr::ModeInfo> {
    let refresh_mhz = |m: &randr::ModeInfo| -> u64 {
        let total = u64::from(m.htotal) * u64::from(m.vtotal);
        (u64::from(m.dot_clock) * 1000)
            .checked_div(total)
            .unwrap_or(0)
    };
    for size in [want, cvt] {
        let best = output_modes
            .iter()
            .enumerate()
            .filter_map(|(idx, id)| {
                let info = all_modes.iter().find(|m| m.id == *id)?;
                ((info.width, info.height) == size).then_some((idx, *info))
            })
            .max_by_key(|(idx, info)| (*idx < usize::from(num_preferred), refresh_mhz(info)));
        if let Some((_, info)) = best {
            return Some(info);
        }
    }
    None
}

/// Look up a mode by name in a `GetScreenResourcesReply`. Names are packed
/// consecutively into `resources.names`, with each mode's slice given by
/// its own `name_len`, in the same order as `resources.modes` -- there is
/// no per-mode name field to compare directly.
fn find_mode_by_name(
    resources: &randr::GetScreenResourcesReply,
    name: &[u8],
) -> Option<randr::Mode> {
    let mut offset = 0usize;
    for m in &resources.modes {
        let end = offset + m.name_len as usize;
        if resources.names.get(offset..end) == Some(name) {
            return Some(m.id);
        }
        offset = end;
    }
    None
}

/// X has no scale concept: it reports a physical size and toolkits derive
/// DPI from it. Inverting the usual relation -- mm = px / (96 * scale) *
/// 25.4 -- makes scale 1.0 report exactly 96 DPI and 1.5 report 144.
fn mm_for_scale(px: u16, scale_milli: u16) -> u32 {
    // A client-supplied 0 would divide DPI to zero and blow this up to
    // u32::MAX via the saturating cast (no panic, no X error -- just a
    // screen reported as thousands of km wide). `IoBridge` doesn't validate
    // `scale_milli` before storing it, so this is the only guard.
    let scale_milli = scale_milli.max(1);
    let dpi = 96.0 * f64::from(scale_milli) / 1000.0;
    ((f64::from(px) / dpi) * 25.4).round() as u32
}

/// Lower a hardware ceiling to an operator-configured maximum.
///
/// `raw` is the value of `GHOSTFRAME_ATTACH_MAX_RESOLUTION`: `"WIDTHxHEIGHT"`
/// to clamp, `"none"` to disable, absent to disable.
///
/// **Only ever lowers.** A configured value above the hardware ceiling is
/// ignored rather than applied, because raising it would have the server ask
/// X for a mode it cannot allocate.
///
/// **Malformed input is ignored, not fatal.** A typo in a unit file must not
/// stop the daemon serving; it warns and keeps the hardware ceiling. Failing
/// closed here would turn a cosmetic mistake into an outage.
fn clamp_ceiling(hardware: (u16, u16), raw: Option<&str>) -> (u16, u16) {
    let Some(raw) = raw else {
        return hardware;
    };
    let raw = raw.trim();
    if raw.is_empty() || raw.eq_ignore_ascii_case("none") {
        return hardware;
    }
    let parsed = raw
        .split_once(['x', 'X'])
        .and_then(|(w, h)| Some((w.trim().parse::<u16>().ok()?, h.trim().parse::<u16>().ok()?)))
        .filter(|(w, h)| *w > 0 && *h > 0);
    match parsed {
        Some((w, h)) => (hardware.0.min(w), hardware.1.min(h)),
        None => {
            tracing::warn!(
                value = raw,
                "GHOSTFRAME_ATTACH_MAX_RESOLUTION is not WIDTHxHEIGHT or 'none'; ignoring"
            );
            hardware
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_for_scale_1x_1920_is_508mm() {
        // 1920 / 96 dpi = 20 in = 508 mm.
        assert_eq!(mm_for_scale(1920, 1000), 508);
    }

    #[test]
    fn mm_for_scale_2x_1920_is_half() {
        // Doubling the scale halves the reported physical size.
        assert_eq!(mm_for_scale(1920, 2000), 254);
    }

    #[test]
    fn mm_for_scale_1_5x_2560() {
        // 2560 / (96 * 1.5) = 2560 / 144 = 17.7778 in = 451.556 mm, rounds
        // to 452.
        assert_eq!(mm_for_scale(2560, 1500), 452);
    }

    #[test]
    fn mm_for_scale_zero_scale_does_not_blow_up() {
        // A malicious or buggy client sending scale_milli=0 must not
        // produce inf/NaN/u32::MAX -- it should behave as scale_milli=1.
        assert_eq!(mm_for_scale(1920, 0), mm_for_scale(1920, 1));
    }

    fn mode(id: u32, w: u16, h: u16, dot_clock: u32) -> randr::ModeInfo {
        randr::ModeInfo {
            id,
            width: w,
            height: h,
            dot_clock,
            htotal: 1000,
            vtotal: 1000,
            ..Default::default()
        }
    }

    #[test]
    fn apply_steps_follow_xrandr_order() {
        // Recorded from `xrandr --verbose --output screen --mode M` on Xvfb,
        // for a shrink and for a grow alike: disable, screen, crtc.
        let all = vec![
            RandrStep::Grab,
            RandrStep::DisableCrtc,
            RandrStep::SetScreen(1280, 720),
            RandrStep::SetCrtc,
            RandrStep::Ungrab,
        ];
        assert_eq!(apply_steps((1920, 1080), 7, 9, (1280, 720)), all);
        let grow = apply_steps((1280, 720), 9, 7, (1920, 1080));
        assert_eq!(grow[1], RandrStep::DisableCrtc);
        assert_eq!(grow[2], RandrStep::SetScreen(1920, 1080));
    }

    #[test]
    fn apply_steps_skip_the_disable_for_a_disabled_crtc() {
        assert_eq!(
            apply_steps((1920, 1080), x11rb::NONE, 9, (1280, 720)),
            vec![
                RandrStep::Grab,
                RandrStep::SetScreen(1280, 720),
                RandrStep::SetCrtc,
                RandrStep::Ungrab,
            ]
        );
    }

    #[test]
    fn apply_steps_for_the_current_mode_only_resize_the_screen() {
        // A scale-only change: the output must not blank.
        assert_eq!(
            apply_steps((1920, 1080), 9, 9, (1920, 1080)),
            vec![RandrStep::SetScreen(1920, 1080)]
        );
    }

    #[test]
    fn pick_existing_mode_prefers_exact_then_cvt_size() {
        let all = [
            mode(1, 1920, 1080, 60_000_000),
            mode(2, 1368, 768, 60_000_000),
        ];
        // Exact size present.
        assert_eq!(
            pick_existing_mode(&[1, 2], 0, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            Some(1)
        );
        // 1366 is not offered; the CVT-rounded 1368 is.
        assert_eq!(
            pick_existing_mode(&[1, 2], 0, &all, (1366, 768), (1368, 768)).map(|m| m.id),
            Some(2)
        );
        // Neither: create a CVT mode.
        assert_eq!(
            pick_existing_mode(&[1, 2], 0, &all, (1000, 700), (1000, 700)).map(|m| m.id),
            None
        );
        // A global mode not on this output does not count.
        assert_eq!(
            pick_existing_mode(&[2], 0, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            None
        );
    }

    #[test]
    fn pick_existing_mode_prefers_preferred_then_refresh() {
        // 1000x1000 totals: dot_clock / 1e6 is the refresh rate in Hz.
        let all = [
            mode(1, 1920, 1080, 60_000_000),
            mode(2, 1920, 1080, 75_000_000),
            mode(3, 1920, 1080, 50_000_000),
        ];
        // No preferred mode: the highest refresh.
        assert_eq!(
            pick_existing_mode(&[1, 2, 3], 0, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            Some(2)
        );
        // The first mode is preferred: it wins over a faster one.
        assert_eq!(
            pick_existing_mode(&[3, 1, 2], 1, &all, (1920, 1080), (1920, 1080)).map(|m| m.id),
            Some(3)
        );
    }

    #[test]
    fn unset_means_no_clamp() {
        assert_eq!(clamp_ceiling((16384, 16384), None), (16384, 16384));
    }

    #[test]
    fn a_wxh_value_lowers_the_ceiling() {
        assert_eq!(
            clamp_ceiling((16384, 16384), Some("1920x1080")),
            (1920, 1080)
        );
    }

    #[test]
    fn each_axis_clamps_independently() {
        // The five tests this started with all had either NO axis exceeding
        // or BOTH exceeding, so none of them could tell per-axis `min` apart
        // from whole-value rejection -- a review mutation to the latter left
        // every test passing. This is the case that straddles: width is
        // already under the clamp, height is over.
        assert_eq!(clamp_ceiling((1280, 2160), Some("1920x1080")), (1280, 1080));
        assert_eq!(clamp_ceiling((3840, 720), Some("1920x1080")), (1920, 720));
    }

    #[test]
    fn the_clamp_only_ever_lowers() {
        // A clamp larger than the hardware ceiling must not raise it -- that
        // would ask X for a mode it cannot allocate.
        assert_eq!(clamp_ceiling((1280, 720), Some("1920x1080")), (1280, 720));
    }

    #[test]
    fn none_disables_the_clamp() {
        // The documented escape hatch, for an operator who has decided the
        // recovery view no longer needs protecting.
        assert_eq!(clamp_ceiling((16384, 16384), Some("none")), (16384, 16384));
    }

    #[test]
    fn malformed_values_are_ignored_not_fatal() {
        // A typo in a unit file must not stop the daemon serving. Each of
        // these logs a warning and leaves the hardware ceiling in place.
        for bad in ["", "1920", "1920x", "x1080", "1920*1080", "abcxdef", "0x0"] {
            assert_eq!(
                clamp_ceiling((16384, 16384), Some(bad)),
                (16384, 16384),
                "input {bad:?} should have been ignored"
            );
        }
    }

    #[test]
    fn find_mode_by_name_locates_the_matching_entry() {
        let modes = vec![
            randr::ModeInfo {
                id: 100,
                name_len: 5,
                ..Default::default()
            },
            randr::ModeInfo {
                id: 200,
                name_len: 4,
                ..Default::default()
            },
        ];
        let resources = randr::GetScreenResourcesReply {
            modes,
            names: b"firstbbbb".to_vec(),
            ..Default::default()
        };
        assert_eq!(find_mode_by_name(&resources, b"first"), Some(100));
        assert_eq!(find_mode_by_name(&resources, b"bbbb"), Some(200));
        assert_eq!(find_mode_by_name(&resources, b"nope"), None);
    }
}

#[cfg(test)]
mod live_tests {
    //! Against a private Xvfb, whose single output is called `screen` and
    //! whose initial screen size is also its maximum.
    use super::*;
    use crate::capture::x11::randr_events::{shape, Recorder};
    use crate::capture::x11::test_support::PrivateXvfb;
    use std::process::Command;
    use std::time::Duration;

    fn xrandr(display: &str, args: &[&str]) {
        let status = Command::new("xrandr")
            .arg("-display")
            .arg(display)
            .args(args)
            .status()
            .expect("run xrandr (CI installs x11-xserver-utils)");
        assert!(status.success(), "xrandr {args:?} failed");
    }

    /// Give the output an 800x600 mode of its own, as a real output has.
    fn add_800x600(display: &str) {
        xrandr(
            display,
            &[
                "--newmode",
                "800x600_t",
                "40",
                "800",
                "840",
                "968",
                "1056",
                "600",
                "601",
                "605",
                "628",
            ],
        );
        xrandr(display, &["--addmode", "screen", "800x600_t"]);
    }

    fn mode_names(display: &str) -> Vec<String> {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let root = conn.setup().roots[n].root;
        let res = conn
            .randr_get_screen_resources(root)
            .unwrap()
            .reply()
            .unwrap();
        let mut off = 0usize;
        res.modes
            .iter()
            .map(|m| {
                let end = off + usize::from(m.name_len);
                let name = String::from_utf8_lossy(&res.names[off..end]).into_owned();
                off = end;
                name
            })
            .collect()
    }

    /// The window manager's view of a server-initiated change must match
    /// `xrandr --output screen --mode M` exactly, for a shrink and a grow.
    #[test]
    fn set_output_emits_the_same_events_as_xrandr() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let rec = Recorder::connect(Some(x.display())).unwrap();
        let ctl = XrandrDisplay::connect(Some(x.display()), None, Duration::ZERO).unwrap();
        let settle = || {
            rec.collect(Duration::from_millis(200), Duration::from_secs(3))
                .unwrap()
        };
        settle();
        for (name, other, (w, h)) in [
            ("800x600_t", "1280x1024", (800u16, 600u16)),
            ("1280x1024", "800x600_t", (1280, 1024)),
        ] {
            xrandr(x.display(), &["--output", "screen", "--mode", name]);
            let theirs = shape(&settle());
            xrandr(x.display(), &["--output", "screen", "--mode", other]);
            settle();
            ctl.set_output(w, h, 1000).unwrap();
            let ours = shape(&settle());
            assert!(!theirs.is_empty(), "xrandr produced no events");
            assert_eq!(ours, theirs, "to {w}x{h}");
        }
    }

    #[test]
    fn set_output_reuses_an_existing_mode_and_creates_cvt_only_otherwise() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = XrandrDisplay::connect(Some(x.display()), None, Duration::ZERO).unwrap();
        let before = mode_names(x.display());
        ctl.set_output(800, 600, 1000).unwrap();
        assert_eq!(mode_names(x.display()), before, "800x600_t must be reused");
        ctl.set_output(1000, 700, 1000).unwrap();
        let after = mode_names(x.display());
        assert!(
            after.iter().any(|n| n == "ghostframe_1000x700"),
            "{after:?}"
        );
        let (conn, n) = x11rb::connect(Some(x.display())).unwrap();
        let g = conn
            .get_geometry(conn.setup().roots[n].root)
            .unwrap()
            .reply()
            .unwrap();
        assert_eq!((g.width, g.height), (1000, 700));
    }

    /// A desktop that restores its own mode inside the settle window must be
    /// noticed, and the picture the client gets is the desktop's. We never
    /// re-apply.
    #[test]
    fn a_desktop_reverting_the_mode_is_noticed_and_not_fought() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl =
            XrandrDisplay::connect(Some(x.display()), None, Duration::from_millis(800)).unwrap();
        ctl.set_output(800, 600, 1000).unwrap();
        // The "window manager" restores its saved mode inside the window.
        std::thread::sleep(Duration::from_millis(300));
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(
            ctl.overridden(),
            Some(Overridden {
                requested: (800, 600),
                actual: (1280, 1024)
            })
        );
        assert_eq!(
            read_actual(Some(x.display()), ctl.crtc).unwrap(),
            (1280, 1024)
        );
    }

    /// When the request sticks there is nothing to report.
    #[test]
    fn a_mode_that_sticks_reports_no_override() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl =
            XrandrDisplay::connect(Some(x.display()), None, Duration::from_millis(300)).unwrap();
        ctl.set_output(800, 600, 1000).unwrap();
        std::thread::sleep(Duration::from_millis(900));
        assert_eq!(ctl.overridden(), None);
        assert_eq!(
            read_actual(Some(x.display()), ctl.crtc).unwrap(),
            (800, 600)
        );
    }
}
