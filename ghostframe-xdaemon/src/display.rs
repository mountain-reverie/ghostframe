//! `DisplayController` backed by X RandR.
//!
//! Implements `ghostframe_lib::transport::display::DisplayController`
//! (mirrors `input_inject::XTestInjector`, which implements `InputInjector`
//! for the same reason: keep the protocol/`IoBridge` layer testable without
//! a running display server). Everything X-specific -- RandR requests,
//! modelines, millimetres -- stays inside this module; the trait itself
//! must not gain an X concept. See the M4b design doc §5 and §7.

use std::sync::Mutex;

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::rust_connection::RustConnection;

use ghostframe_lib::transport::display::{DisplayController, DisplayError};

use crate::cvt;

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
}

impl XrandrDisplay {
    /// Connect to the local X server, confirm the RandR extension is
    /// present, and pick the first connected output with an available CRTC.
    ///
    /// Errors if no DISPLAY, RandR is missing, or there is no connected
    /// output -- the caller (xdaemon's `main`) logs and continues with no
    /// display controller, exactly like `XTestInjector::new`.
    pub fn new() -> anyhow::Result<Self> {
        use anyhow::{anyhow, Context};

        let (conn, screen_num) =
            x11rb::connect(None).map_err(|e| anyhow!("x11rb::connect: {e}"))?;
        let root = conn.setup().roots[screen_num].root;

        let ext = conn
            .query_extension(b"RANDR")
            .context("query_extension(RANDR)")?
            .reply()
            .context("RANDR query_extension reply")?;
        if !ext.present {
            return Err(anyhow!("RandR extension not present on this Xorg build"));
        }

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

        Ok(Self {
            conn,
            root,
            output,
            crtc,
            ceiling_fallback: Mutex::new((range.max_width, range.max_height)),
        })
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
        match self.query_ceiling() {
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
        }
    }

    fn set_output(&self, width: u16, height: u16, scale_milli: u16) -> Result<(), DisplayError> {
        let timing = cvt::reduced_blanking(width, height, 60);
        // The actually-applied mode is `h_active x v_active`, not the raw
        // requested `width x height`: CVT rounds h_active up to a multiple
        // of 8 (v_active never differs from `height` -- there's no vertical
        // rounding), and every X-facing use below (the dedup match, the
        // mode name, and `SetScreenSize`) must agree with what the mode
        // object itself says, or a non-8-aligned request creates a mode
        // whose own dedup check can never find it again.
        let mode_w = timing.h_active;
        let mode_h = timing.v_active;

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

        // X accumulates modes for the lifetime of the connection; only
        // create a new one if this output doesn't already have one of the
        // exact size requested. Without this check, a client dragging a
        // resize handle would leave the server full of one-off modes that
        // are never cleaned up.
        let existing_mode = output_info.modes.iter().copied().find(|&mode_id| {
            resources
                .modes
                .iter()
                .any(|m| m.id == mode_id && m.width == mode_w && m.height == mode_h)
        });

        let mode = match existing_mode {
            Some(id) => id,
            None => {
                let name = format!("ghostframe_{mode_w}x{mode_h}");
                // Verified locally against `cvt(1)`'s own Modeline output
                // (`cvt 1920 1080 60 -r` -> "... +hsync -vsync"): CVT
                // reduced-blanking inverts standard CVT's sync polarity.
                // `cvt.rs` itself carries no flags/polarity data -- this
                // check is local to this module.
                let mode_info = randr::ModeInfo {
                    id: 0,
                    width: mode_w,
                    height: mode_h,
                    // ModeInfo.dot_clock is Hz; `Timing::pixel_clock_khz` is kHz.
                    //
                    // Verified against a live X server rather than assumed:
                    // `xrandr --verbose` reports 1920x1080 at 148.500MHz with
                    // htotal 2640 and a horizontal clock of 56.25KHz, and
                    // 148_500_000 / 2640 = 56_250 exactly. Only a Hz-valued
                    // dot_clock makes that arithmetic close, so the *1000 is
                    // right. A kHz value here would produce a mode whose
                    // clock is 1000x too slow -- one X accepts and no display
                    // can drive.
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
                match self
                    .conn
                    .randr_create_mode(self.root, mode_info, name.as_bytes())
                    .map_err(|e| DisplayError(format!("CreateMode request: {e}")))?
                    .reply()
                {
                    Ok(created) => {
                        self.conn
                            .randr_add_output_mode(self.output, created.mode)
                            .map_err(|e| DisplayError(format!("AddOutputMode request: {e}")))?
                            .check()
                            .map_err(|e| DisplayError(format!("AddOutputMode: {e}")))?;
                        created.mode
                    }
                    // `RRModeCreateUser` checks for a name collision before
                    // creating anything and returns BadName if one exists --
                    // which happens if this exact size was created earlier
                    // and later detached from every output (e.g. a prior
                    // `DeleteOutputMode`, or a multi-output setup where
                    // another output made it first). Without this fallback
                    // that size becomes permanently uncreatable for the life
                    // of the X server; instead, look the existing mode up by
                    // name and attach it to this output.
                    Err(x11rb::errors::ReplyError::X11Error(ref e))
                        if e.error_kind == x11rb::protocol::ErrorKind::Name =>
                    {
                        let mode_id =
                            find_mode_by_name(&resources, name.as_bytes()).ok_or_else(|| {
                                DisplayError(format!(
                                    "CreateMode: BadName for {name:?} but no matching \
                                     global mode found"
                                ))
                            })?;
                        self.conn
                            .randr_add_output_mode(self.output, mode_id)
                            .map_err(|e| DisplayError(format!("AddOutputMode request: {e}")))?
                            .check()
                            .map_err(|e| DisplayError(format!("AddOutputMode: {e}")))?;
                        mode_id
                    }
                    Err(e) => return Err(DisplayError(format!("CreateMode reply: {e}"))),
                }
            }
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

        let set_crtc = || -> Result<(), DisplayError> {
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
                    &[self.output],
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

        for step in resize_steps(
            (screen_geom.width, screen_geom.height),
            (crtc_info.width, crtc_info.height),
            (mode_w, mode_h),
        ) {
            match step {
                ResizeStep::SetScreen(w, h) => set_screen(w, h)?,
                ResizeStep::SetCrtc => set_crtc()?,
            }
        }

        Ok(())
    }
}

/// One step of applying a size change through RandR's screen/CRTC coupling.
/// `SetCrtc` always switches to the caller's final target mode -- there is
/// never an "intermediate" CRTC mode, only an intermediate screen size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResizeStep {
    SetScreen(u16, u16),
    SetCrtc,
}

/// Determine the ordered steps needed to move from the current screen size
/// and CRTC mode to `new`, obeying X's two independent, differently-scoped
/// guards (verified against the xserver source, `rrscreen.c` /
/// `rrcrtc.c`):
///
/// - `ProcRRSetScreenSize`: legal only if the **new** screen still fits
///   every enabled CRTC's **current** mode (`crtc.x + mode.width <=
///   new_width`, ditto height) -- i.e. it depends on `crtc`, not `screen`.
/// - `ProcRRSetCrtcConfig`: legal only if the **new** mode fits the
///   **current** screen (`x + mode.width <= screen.width`, ditto height) --
///   i.e. it depends on `screen`, not `crtc`.
///
/// A plain increase in both dimensions (`new >= crtc` in both) can go
/// screen-first: growing the screen to `new` trivially satisfies the first
/// guard (the *old*, smaller CRTC mode still fits), and the CRTC switch
/// that follows trivially satisfies the second (the screen is now exactly
/// `new`). A plain decrease relative to the *current screen* (`new <=
/// screen` in both) is the mirror: CRTC-first, since switching to the
/// smaller mode first trivially satisfies the second guard against the
/// *old*, larger screen, and the screen shrink that follows trivially
/// satisfies the first (the CRTC is now exactly `new`).
///
/// Neither two-step order covers every case -- most notably a mixed resize
/// (one dimension grows, the other shrinks), where the growing dimension
/// wants "screen first" and the shrinking one wants "CRTC first" at the
/// same time, so both direct one-shot orders are illegal. `xrandr(1)`
/// itself falls back to a three-step sequence here: grow the screen to a
/// superset of both the current CRTC mode and the new mode (always legal --
/// it only ever grows relative to both dimensions the first guard checks),
/// switch the CRTC to the new mode (now legal -- the intermediate screen is
/// `>=` it in both dimensions), then shrink the screen down to the exact
/// new size (now legal -- the CRTC already matches it exactly). The two
/// fast paths above are just optimisations of this general sequence for the
/// cases where the middle step is a no-op.
fn resize_steps(screen: (u16, u16), crtc: (u16, u16), new: (u16, u16)) -> Vec<ResizeStep> {
    let (screen_w, screen_h) = screen;
    let (crtc_w, crtc_h) = crtc;
    let (new_w, new_h) = new;

    let screen_first_legal = new_w >= crtc_w && new_h >= crtc_h;
    let crtc_first_legal = new_w <= screen_w && new_h <= screen_h;

    if screen_first_legal {
        vec![ResizeStep::SetScreen(new_w, new_h), ResizeStep::SetCrtc]
    } else if crtc_first_legal {
        vec![ResizeStep::SetCrtc, ResizeStep::SetScreen(new_w, new_h)]
    } else {
        let mid_w = screen_w.max(crtc_w).max(new_w);
        let mid_h = screen_h.max(crtc_h).max(new_h);
        vec![
            ResizeStep::SetScreen(mid_w, mid_h),
            ResizeStep::SetCrtc,
            ResizeStep::SetScreen(new_w, new_h),
        ]
    }
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

    #[test]
    fn resize_steps_plain_growth_is_screen_first() {
        // Both dimensions increase relative to the current CRTC mode.
        let steps = resize_steps((1920, 1080), (1920, 1080), (2560, 1440));
        assert_eq!(
            steps,
            vec![ResizeStep::SetScreen(2560, 1440), ResizeStep::SetCrtc]
        );
    }

    #[test]
    fn resize_steps_plain_shrink_is_crtc_first() {
        // Both dimensions decrease relative to the current screen.
        let steps = resize_steps((1920, 1080), (1920, 1080), (1280, 720));
        assert_eq!(
            steps,
            vec![ResizeStep::SetCrtc, ResizeStep::SetScreen(1280, 720)]
        );
    }

    #[test]
    fn resize_steps_mixed_width_shrinks_height_grows_needs_three_steps() {
        // The exact failing case from the review: drag a corner so width
        // shrinks and height grows. Steady state going in has screen ==
        // crtc == 1920x1080; target 1280x1440. Neither two-step order is
        // legal (screen-first needs new >= crtc in both dims; crtc-first
        // needs new <= screen in both dims), so this must fall back to the
        // three-step sequence.
        let steps = resize_steps((1920, 1080), (1920, 1080), (1280, 1440));
        assert_eq!(
            steps,
            vec![
                ResizeStep::SetScreen(1920, 1440),
                ResizeStep::SetCrtc,
                ResizeStep::SetScreen(1280, 1440),
            ]
        );
    }

    #[test]
    fn resize_steps_mixed_width_grows_height_shrinks_needs_three_steps() {
        // The mirror mixed case: width grows, height shrinks.
        let steps = resize_steps((1920, 1080), (1920, 1080), (2560, 800));
        assert_eq!(
            steps,
            vec![
                ResizeStep::SetScreen(2560, 1080),
                ResizeStep::SetCrtc,
                ResizeStep::SetScreen(2560, 800),
            ]
        );
    }

    #[test]
    fn resize_steps_does_not_assume_screen_equals_crtc() {
        // Defensive case: screen and CRTC mode have drifted apart (e.g. a
        // partially-applied previous change). Growing relative to the CRTC
        // mode but shrinking relative to the (larger) screen must still
        // take the three-step path, not the screen-first fast path -- a
        // fast path here would enable the CRTC into a mode that already
        // fits without ever touching the oversized screen, but would
        // leave the screen wrong. Concretely: screen is 2000x2000 (drifted
        // large), crtc mode is 1920x1080, new target is 1920x1500 --
        // this is >= crtc in both dims (screen-first legal) so it *does*
        // take the fast path; assert that explicitly rather than assuming.
        let steps = resize_steps((2000, 2000), (1920, 1080), (1920, 1500));
        assert_eq!(
            steps,
            vec![ResizeStep::SetScreen(1920, 1500), ResizeStep::SetCrtc]
        );
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
