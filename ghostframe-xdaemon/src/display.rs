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
                .any(|m| m.id == mode_id && m.width == width && m.height == height)
        });

        let mode = match existing_mode {
            Some(id) => id,
            None => {
                let name = format!("ghostframe_{width}x{height}");
                // RB v1 modes are +hsync -vsync; verified against `cvt(1)`'s
                // own Modeline output (see cvt.rs's module docs) rather than
                // assumed from a remembered copy of the spec.
                let mode_info = randr::ModeInfo {
                    id: 0,
                    width: timing.h_active,
                    height: timing.v_active,
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
                let created = self
                    .conn
                    .randr_create_mode(self.root, mode_info, name.as_bytes())
                    .map_err(|e| DisplayError(format!("CreateMode request: {e}")))?
                    .reply()
                    .map_err(|e| DisplayError(format!("CreateMode reply: {e}")))?;
                self.conn
                    .randr_add_output_mode(self.output, created.mode)
                    .map_err(|e| DisplayError(format!("AddOutputMode request: {e}")))?
                    .check()
                    .map_err(|e| DisplayError(format!("AddOutputMode: {e}")))?;
                created.mode
            }
        };

        let crtc_info = self
            .conn
            .randr_get_crtc_info(self.crtc, resources.config_timestamp)
            .map_err(|e| DisplayError(format!("GetCrtcInfo request: {e}")))?
            .reply()
            .map_err(|e| DisplayError(format!("GetCrtcInfo reply: {e}")))?;

        let mm_width = mm_for_scale(width, scale_milli);
        let mm_height = mm_for_scale(height, scale_milli);

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

        let set_screen = || -> Result<(), DisplayError> {
            self.conn
                .randr_set_screen_size(self.root, width, height, mm_width, mm_height)
                .map_err(|e| DisplayError(format!("SetScreenSize request: {e}")))?
                .check()
                .map_err(|e| DisplayError(format!("SetScreenSize: {e}")))
        };

        // Ordering hazard: X returns BadMatch if the screen shrinks below a
        // CRTC still using a larger mode, and BadMatch if a CRTC is enabled
        // with a mode that doesn't fit the *current* screen. So a size
        // increase in both dimensions is safe to apply screen-first (the
        // screen is already big enough by the time the CRTC switches to the
        // bigger mode); anything else -- a shrink, or a mixed change where
        // one dimension grows and the other shrinks -- must reconfigure the
        // CRTC first, down to a mode that still fits the old, larger screen,
        // before the screen itself changes to match.
        let growing = width >= crtc_info.width && height >= crtc_info.height;
        if growing {
            set_screen()?;
            set_crtc()?;
        } else {
            set_crtc()?;
            set_screen()?;
        }

        Ok(())
    }
}

/// X has no scale concept: it reports a physical size and toolkits derive
/// DPI from it. Inverting the usual relation -- mm = px / (96 * scale) *
/// 25.4 -- makes scale 1.0 report exactly 96 DPI and 1.5 report 144.
fn mm_for_scale(px: u16, scale_milli: u16) -> u32 {
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
}
