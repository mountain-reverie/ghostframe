//! `DisplayController` backed by X RandR.
//!
//! Implements `ghostframe_lib::transport::display::DisplayController`
//! (mirrors `input_inject::XTestInjector`, which implements `InputInjector`
//! for the same reason: keep the protocol/`IoBridge` layer testable without
//! a running display server). Everything X-specific -- RandR requests,
//! modelines, millimetres -- stays inside this module; the trait itself
//! must not gain an X concept. See the M4b design doc §5 and §7.

use std::io::Read as _;
use std::os::fd::{AsFd as _, AsRawFd as _, OwnedFd};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::{self, ConnectionExt as RandrExt};
use x11rb::protocol::xproto::ConnectionExt as XProtoExt;
use x11rb::protocol::Event;
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
    /// `GHOSTFRAME_MAX_RESOLUTION`, read once at construction (this
    /// crate's convention -- see `main.rs`). `Some("WIDTHxHEIGHT")` lowers
    /// what `ceiling()` reports; `None` (unset) leaves the hardware ceiling
    /// alone. Applied by `clamp_ceiling()` on every path through `ceiling()`.
    max_resolution: Option<String>,
    /// The read-back worker; `None` when the read-back is off (settle of
    /// zero) or its connection failed.
    readback: Option<Readback>,
    /// The physical size (mm) the screen has now, as far as we know: seeded
    /// from the connection setup, set by every `set_output`, and kept up to
    /// date by the worker from ScreenChangeNotify. What a failed change puts
    /// back. (`GetScreenInfo` is no use: its sizes carry the output's
    /// physical size, not the screen's.)
    known_mm: Arc<Mutex<(u32, u32)>>,
    /// Bumped at the top of every `set_output`, so any call -- failed ones
    /// too -- disarms the read-back of the one before it.
    generation: Arc<AtomicU64>,
    /// What the last read-back found when the desktop had overridden the
    /// request, `None` while the request stuck. Shared with the worker; the
    /// tests observe it.
    #[cfg_attr(not(test), allow(dead_code))]
    overridden: Arc<Mutex<Option<Overridden>>>,
    /// Read-backs that reached a verdict (tests prove the worker ran).
    #[cfg_attr(not(test), allow(dead_code))]
    completed: Arc<AtomicU64>,
    /// Set by the worker as it exits (tests watch it).
    #[cfg(test)]
    worker_exited: Arc<AtomicBool>,
    /// Test fault: fail the step that sets the target CRTC mode.
    #[cfg(test)]
    fail_set_crtc: std::sync::atomic::AtomicBool,
}

/// When the read-back reports. It reports once ALL of `settle` has passed
/// and no RandR event has arrived for `quiet`; or when `cap` has passed.
///
/// Observed on the owner's Enlightenment: stock E reverts a change about
/// 1.7 s after it (`hotplug_response`, 1 s, plus its own work); the patched
/// E defers adopting or reverting for about 3 s plus `hotplug_response`
/// after its own applies, so about 4 s. A settle of 5 s clears both, the
/// quiet period waits out a revert that is still in flight, and the cap
/// stops a desktop that never goes quiet from holding the verdict forever.
#[derive(Debug, Clone, Copy)]
pub struct ReadbackTiming {
    pub settle: Duration,
    pub quiet: Duration,
    pub cap: Duration,
}

impl ReadbackTiming {
    pub fn from_settle(settle: Duration) -> Self {
        Self {
            settle,
            quiet: Duration::from_millis(1500),
            cap: settle.max(Duration::from_secs(12)),
        }
    }
}

/// The handle on the read-back worker. Dropping `wake` hangs up the pipe the
/// worker polls, which is how it learns to exit while idle.
struct Readback {
    /// The newest unverified request; a newer one replaces it.
    slot: Arc<Mutex<Option<Request>>>,
    wake: UnixStream,
    handle: Option<std::thread::JoinHandle<()>>,
    /// A dup of the worker's X socket. The pipe cannot wake a worker that is
    /// blocked in a round trip to an X server that has stopped answering;
    /// shutting this socket down can.
    x_fd: OwnedFd,
    /// The worker sends once as it exits.
    done: Mutex<mpsc::Receiver<()>>,
    /// Set before the shutdown, so the worker's exit is not reported as a
    /// failure.
    stopping: Arc<AtomicBool>,
}

/// How long `Drop` waits for the worker before it gives up and detaches it.
const WORKER_JOIN_TIMEOUT: Duration = Duration::from_millis(500);

impl Drop for XrandrDisplay {
    fn drop(&mut self) {
        // The last owner of this controller can be capture's exit path; it
        // must never hang there, or the service manager never gets to
        // restart us.
        if let Some(Readback {
            wake,
            handle,
            x_fd,
            done,
            stopping,
            ..
        }) = self.readback.take()
        {
            stopping.store(true, Ordering::SeqCst);
            // SAFETY: shutdown(2) on an fd we own; failure is harmless.
            unsafe { libc::shutdown(x_fd.as_raw_fd(), libc::SHUT_RDWR) };
            drop(wake);
            match done.into_inner().unwrap().recv_timeout(WORKER_JOIN_TIMEOUT) {
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    tracing::warn!("the RandR read-back worker did not stop; detaching it");
                }
                _ => {
                    if let Some(h) = handle {
                        let _ = h.join();
                    }
                }
            }
        }
    }
}

/// One applied request for the worker to verify.
#[derive(Clone, Copy)]
struct Request {
    ticket: u64,
    requested: (u16, u16),
}

/// A mode change the desktop undid or altered after we applied it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Overridden {
    pub requested: (u16, u16),
    /// The screen size, which is what capture grabs and the client sees.
    pub screen: (u16, u16),
    /// The CRTC's own size; `None` when it is disabled.
    pub crtc: Option<(u16, u16)>,
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
        let timing = (!settle.is_zero()).then(|| ReadbackTiming::from_settle(settle));
        Self::with_timing(display, max_resolution, timing)
    }

    /// [`Self::new`] with the read-back timing spelled out (`None` = off).
    pub fn with_timing(
        display: Option<&str>,
        max_resolution: Option<String>,
        timing: Option<ReadbackTiming>,
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

        let generation = Arc::new(AtomicU64::new(0));
        let overridden = Arc::new(Mutex::new(None));
        let completed = Arc::new(AtomicU64::new(0));
        let root_setup = &conn.setup().roots[screen_num];
        let known_mm = Arc::new(Mutex::new((
            u32::from(root_setup.width_in_millimeters),
            u32::from(root_setup.height_in_millimeters),
        )));
        #[cfg(test)]
        let worker_exited = Arc::new(AtomicBool::new(false));
        let readback = timing.and_then(|timing| {
            match spawn_readback_worker(
                display,
                crtc,
                timing,
                Worker {
                    generation: Arc::clone(&generation),
                    overridden: Arc::clone(&overridden),
                    completed: Arc::clone(&completed),
                    known_mm: Arc::clone(&known_mm),
                    stopping: Arc::new(AtomicBool::new(false)),
                    #[cfg(test)]
                    exited: Arc::clone(&worker_exited),
                },
            ) {
                Ok(rb) => Some(rb),
                Err(e) => {
                    tracing::warn!(error = %e, "RandR read-back unavailable");
                    None
                }
            }
        });

        Ok(Self {
            conn,
            root,
            output,
            crtc,
            ceiling_fallback: Mutex::new((range.max_width, range.max_height)),
            max_resolution,
            readback,
            known_mm,
            generation,
            overridden,
            completed,
            #[cfg(test)]
            worker_exited,
            #[cfg(test)]
            fail_set_crtc: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// What the last read-back found, if the desktop overrode a request.
    #[cfg(test)]
    fn overridden(&self) -> Option<Overridden> {
        *self.overridden.lock().unwrap()
    }

    /// Queue a read-back of the screen for `requested`. A window manager or
    /// desktop may restore its own saved mode some time after ours (see
    /// [`ReadbackTiming`]), so a successful `set_output` does not mean the
    /// size stuck. The worker only observes: it never re-applies, because
    /// fighting the desktop would flicker the monitor forever. The client
    /// needs no separate notice -- capture polls the root geometry every
    /// frame and the frame size it reports follows.
    fn queue_readback(&self, ticket: u64, requested: (u16, u16)) {
        if let Some(rb) = &self.readback {
            *rb.slot.lock().unwrap() = Some(Request { ticket, requested });
            // A full pipe already holds a wake-up; that is as good as ours.
            // send(2) with MSG_NOSIGNAL rather than write: a dead worker must
            // not raise SIGPIPE, and this process links Go, whose runtime
            // owns signal handling.
            // SAFETY: a valid fd and a one-byte buffer for the call.
            unsafe {
                libc::send(
                    rb.wake.as_raw_fd(),
                    [1u8].as_ptr().cast(),
                    1,
                    libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT,
                )
            };
        }
    }

    /// Delete every mode of ours that nothing shows, so repeated resizes do
    /// not pile up modes -- including one the desktop reverted away from,
    /// which the previous call never saw as "current". Best effort: a mode
    /// still in use (or anything that fails) is simply left. `resources` is
    /// from before this call's own mode was created, so that one is safe.
    fn collect_modes(&self, resources: &randr::GetScreenResourcesReply) {
        let in_use: Vec<randr::Mode> = resources
            .crtcs
            .iter()
            .map(|&c| {
                self.conn
                    .randr_get_crtc_info(c, 0)
                    .ok()
                    .and_then(|r| r.reply().ok())
                    .map_or(u32::MAX, |i| i.mode)
            })
            .collect();
        let mut offset = 0usize;
        for m in &resources.modes {
            let end = offset + m.name_len as usize;
            let ours = resources
                .names
                .get(offset..end)
                .is_some_and(|n| n.starts_with(b"ghostframe_"));
            offset = end;
            if !ours || in_use.contains(&m.id) || in_use.contains(&u32::MAX) {
                continue;
            }
            if let Ok(c) = self.conn.randr_delete_output_mode(self.output, m.id) {
                c.ignore_error();
            }
            if let Ok(c) = self.conn.randr_destroy_mode(m.id) {
                c.ignore_error();
            }
        }
        let _ = self.conn.flush();
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
        // Before anything can fail: a failed call must still disarm the
        // read-back of the one before it.
        // Under the verdict lock, so a verdict is either wholly before this
        // bump (and then superseded by it) or wholly after.
        let ticket = {
            let _verdicts = self.overridden.lock().unwrap();
            self.generation.fetch_add(1, Ordering::SeqCst) + 1
        };
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

        // The physical size to put back if the change fails half way.
        let prev_mm = *self.known_mm.lock().unwrap();

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
                .map_err(|e| DisplayError(format!("SetScreenSize: {e}")))?;
            *self.known_mm.lock().unwrap() = (mm_w, mm_h);
            Ok(())
        };

        let steps = apply_steps(
            (screen_geom.width, screen_geom.height),
            crtc_info.mode,
            mode,
            (mode_w, mode_h),
        );
        let mut grabbed = false;
        let mut disabled = false;
        let mut resized = false;
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
                    RandrStep::SetScreen(w, h) => {
                        set_screen(w, h)?;
                        resized = true;
                    }
                    RandrStep::SetCrtc => {
                        #[cfg(test)]
                        if self.fail_set_crtc.load(Ordering::SeqCst) {
                            return Err(DisplayError("injected SetCrtcConfig failure".into()));
                        }
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
        if result.is_err() {
            // Still inside the grab: put everything back the way it was.
            // The screen first -- with the CRTC off any size is legal, and a
            // CRTC cannot be restored into a screen smaller than its mode.
            if resized {
                let back = self
                    .conn
                    .randr_set_screen_size(
                        self.root,
                        screen_geom.width,
                        screen_geom.height,
                        prev_mm.0,
                        prev_mm.1,
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.check().map_err(|e| e.to_string()));
                match back {
                    Ok(()) => *self.known_mm.lock().unwrap() = prev_mm,
                    Err(e) => {
                        tracing::warn!(error = %e, "could not restore the screen size after a failed mode change");
                    }
                }
            }
            if disabled {
                // Never leave the output dark.
                let back = self
                    .conn
                    .randr_set_crtc_config(
                        self.crtc,
                        x11rb::CURRENT_TIME,
                        resources.config_timestamp,
                        crtc_info.x,
                        crtc_info.y,
                        crtc_info.mode,
                        crtc_info.rotation,
                        &crtc_info.outputs,
                    )
                    .map_err(|e| e.to_string())
                    .and_then(|c| c.reply().map_err(|e| e.to_string()))
                    .and_then(|r| {
                        if r.status == randr::SetConfig::SUCCESS {
                            Ok(())
                        } else {
                            Err(format!("status={:?}", r.status))
                        }
                    });
                if let Err(e) = back {
                    tracing::warn!(error = %e, "could not restore the CRTC after a failed mode change; trying the requested mode");
                    // The screen is whatever it is now; if the restore of it
                    // failed, the requested mode is the one that fits.
                    if let Err(e2) = set_crtc(mode, &[self.output]) {
                        tracing::warn!(
                            error = %e2,
                            mode,
                            mode_w,
                            mode_h,
                            "could not light the output again with the requested mode; it may be dark"
                        );
                    }
                }
            }
        }
        if grabbed {
            let _ = self.conn.ungrab_server();
        }
        let _ = self.conn.flush();
        if result.is_ok() {
            self.collect_modes(&resources);
            self.queue_readback(ticket, (mode_w, mode_h));
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

/// What the server shows right now.
struct Actual {
    screen: (u16, u16),
    crtc: Option<(u16, u16)>,
}

fn read_actual(conn: &RustConnection, root: u32, crtc: randr::Crtc) -> anyhow::Result<Actual> {
    use anyhow::Context;
    let geom = conn.get_geometry(root)?.reply().context("GetGeometry")?;
    let info = conn
        .randr_get_crtc_info(crtc, 0)?
        .reply()
        .context("GetCrtcInfo")?;
    Ok(Actual {
        screen: (geom.width, geom.height),
        // A disabled CRTC has no size of its own.
        crtc: (info.mode != x11rb::NONE).then_some((info.width, info.height)),
    })
}

/// What the worker shares with the controller.
struct Worker {
    generation: Arc<AtomicU64>,
    overridden: Arc<Mutex<Option<Overridden>>>,
    completed: Arc<AtomicU64>,
    known_mm: Arc<Mutex<(u32, u32)>>,
    stopping: Arc<AtomicBool>,
    #[cfg(test)]
    exited: Arc<AtomicBool>,
}

/// Is a read-back due? `elapsed` since the request was applied, `since_event`
/// since the last RandR event: after `settle` AND once `quiet`, or at `cap`
/// whatever the desktop is still doing.
///
/// `since_applied` is the time since the request was applied. It equals
/// `elapsed` for a fresh request; a re-opened one starts `elapsed` at
/// `settle`, but the watch still ends `cap` after the apply.
fn due(
    elapsed: Duration,
    since_event: Duration,
    since_applied: Duration,
    t: &ReadbackTiming,
) -> bool {
    (elapsed >= t.settle && since_event >= t.quiet) || elapsed >= t.cap || since_applied >= t.cap
}

/// A request being watched towards its verdict.
struct Pending {
    req: Request,
    /// Counts `settle` and `cap` from here.
    start: Instant,
    /// When the request was applied; bounds a later re-opening.
    applied: Instant,
}

/// The worker behind [`ReadbackTiming`]: one thread, one persistent
/// connection subscribed to screen and CRTC changes, and a deadline that
/// every new request restarts. It sleeps in poll(2) on the X socket and a
/// wake pipe, with a timeout of exactly the next deadline (none when
/// nothing is pending), so an idle controller costs no wake-ups. Dropping
/// the pipe's other end ends the thread.
fn spawn_readback_worker(
    display: Option<&str>,
    crtc: randr::Crtc,
    timing: ReadbackTiming,
    shared: Worker,
) -> anyhow::Result<Readback> {
    use anyhow::{anyhow, Context};
    let (conn, n) = x11rb::connect(display).map_err(|e| anyhow!("x11rb::connect: {e}"))?;
    let root = conn.setup().roots[n].root;
    conn.randr_query_version(1, 5)?.reply()?;
    conn.randr_select_input(
        root,
        randr::NotifyMask::SCREEN_CHANGE | randr::NotifyMask::CRTC_CHANGE,
    )?
    .check()
    .context("RRSelectInput")?;
    let (wake, mut wake_rx) = UnixStream::pair().context("wake pipe")?;
    wake.set_nonblocking(true)?;
    wake_rx.set_nonblocking(true)?;
    let x_fd = conn
        .stream()
        .as_fd()
        .try_clone_to_owned()
        .context("dup the X socket")?;
    let (done_tx, done) = mpsc::channel::<()>();
    let stopping = Arc::clone(&shared.stopping);
    let slot: Arc<Mutex<Option<Request>>> = Arc::new(Mutex::new(None));
    let worker_slot = Arc::clone(&slot);
    let handle = std::thread::Builder::new()
        .name("randr-readback".into())
        .spawn(move || {
            readback_loop(
                &conn,
                root,
                crtc,
                timing,
                &shared,
                &worker_slot,
                &mut wake_rx,
            );
            #[cfg(test)]
            shared.exited.store(true, Ordering::SeqCst);
            let _ = done_tx.send(());
        })
        .context("spawn randr-readback")?;
    Ok(Readback {
        slot,
        wake,
        handle: Some(handle),
        x_fd,
        done: Mutex::new(done),
        stopping,
    })
}

fn readback_loop(
    conn: &RustConnection,
    root: u32,
    crtc: randr::Crtc,
    timing: ReadbackTiming,
    shared: &Worker,
    slot: &Mutex<Option<Request>>,
    wake_rx: &mut UnixStream,
) {
    let mut pending: Option<Pending> = None;
    // The request last given a verdict, and when it was applied. A screen
    // change after it, with no newer call and inside `cap` of the apply,
    // re-opens it: a revert later than the verdict is still caught.
    let mut last: Option<(Request, Instant)> = None;
    let mut last_event = Instant::now();
    loop {
        // Everything already read is handled before sleeping.
        let mut saw_event = false;
        loop {
            match conn.poll_for_event() {
                Ok(Some(ev)) => {
                    last_event = Instant::now();
                    saw_event = true;
                    if let Event::RandrScreenChangeNotify(e) = ev {
                        // A rotated screen reports its physical size turned.
                        let turned = u16::from(e.rotation)
                            & (u16::from(randr::Rotation::ROTATE90)
                                | u16::from(randr::Rotation::ROTATE270))
                            != 0;
                        let (w, h) = (u32::from(e.mwidth), u32::from(e.mheight));
                        *shared.known_mm.lock().unwrap() = if turned { (h, w) } else { (w, h) };
                    }
                }
                Ok(None) => break,
                Err(e) => {
                    worker_failed(shared, "reading X events", &e);
                    return;
                }
            }
        }
        let now = Instant::now();
        if let Some(req) = slot.lock().unwrap().take() {
            pending = Some(Pending {
                req,
                start: now,
                applied: now,
            });
            last = None;
        } else if pending.is_none() {
            match last {
                Some((_, applied)) if now.duration_since(applied) >= timing.cap => last = None,
                Some((req, applied))
                    if saw_event && shared.generation.load(Ordering::SeqCst) == req.ticket =>
                {
                    pending = Some(Pending {
                        req,
                        start: now.checked_sub(timing.settle).unwrap_or(now),
                        applied,
                    });
                }
                _ => {}
            }
        }

        // How long to sleep: to the next moment anything can change.
        let mut timeout: Option<Duration> = None;
        if let Some(p) = &pending {
            if shared.generation.load(Ordering::SeqCst) != p.req.ticket {
                pending = None; // superseded by a newer call
                continue;
            }
            let elapsed = now.duration_since(p.start);
            if due(
                elapsed,
                now.duration_since(last_event),
                now.duration_since(p.applied),
                &timing,
            ) {
                let Pending { req, applied, .. } = pending.take().expect("checked Some above");
                last = Some((req, applied));
                match read_actual(conn, root, crtc) {
                    Ok(actual) => record_verdict(
                        &shared.generation,
                        &shared.overridden,
                        &shared.completed,
                        req,
                        &actual,
                    ),
                    Err(e) => tracing::debug!(error = %e, "RandR read-back failed"),
                }
                continue;
            }
            let quiet_at = last_event + timing.quiet;
            let settled_at = p.start + timing.settle;
            let deadline = settled_at
                .max(quiet_at)
                .min(p.start + timing.cap)
                .min(p.applied + timing.cap);
            timeout = Some(deadline.saturating_duration_since(now));
        } else if let Some((_, applied)) = last {
            timeout = Some((applied + timing.cap).saturating_duration_since(now));
        }

        let mut fds = [
            libc::pollfd {
                fd: conn.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_rx.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // Round up, so waking a hair early does not spin at zero.
        let ms = timeout.map_or(-1, |d| {
            i32::try_from(d.as_micros().div_ceil(1000)).unwrap_or(i32::MAX)
        });
        // SAFETY: `fds` is a valid array of two pollfd for the call's duration.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, ms) };
        if r < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            worker_failed(shared, "poll", &std::io::Error::last_os_error());
            return;
        }
        if fds[1].revents != 0 {
            let mut buf = [0u8; 64];
            loop {
                match wake_rx.read(&mut buf) {
                    Ok(0) => return, // the controller was dropped
                    Ok(_) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                    Err(e) => {
                        worker_failed(shared, "reading the wake pipe", &e);
                        return;
                    }
                }
            }
        }
    }
}

/// The worker is leaving because something failed. Said once, unless the
/// controller is the one stopping it (then the X socket was shut on purpose).
fn worker_failed(shared: &Worker, what: &str, err: &dyn std::fmt::Display) {
    if !shared.stopping.load(Ordering::SeqCst) {
        tracing::warn!(error = %err, "the RandR read-back stopped while {what}; later overrides will go unreported");
    }
}

/// Decide the verdict under the `overridden` lock, after the read, against
/// the newest generation: a `set_output` that began while the worker was
/// reading owns the verdict, and this one is dropped.
fn record_verdict(
    generation: &AtomicU64,
    overridden: &Mutex<Option<Overridden>>,
    completed: &AtomicU64,
    req: Request,
    actual: &Actual,
) {
    let mut slot = overridden.lock().unwrap();
    if generation.load(Ordering::SeqCst) != req.ticket {
        return;
    }
    completed.fetch_add(1, Ordering::SeqCst);
    if actual.screen == req.requested {
        *slot = None;
        return;
    }
    let found = Overridden {
        requested: req.requested,
        screen: actual.screen,
        crtc: actual.crtc,
    };
    if *slot != Some(found) {
        let (cw, ch) = actual.crtc.unwrap_or((0, 0));
        tracing::info!(
            requested_w = req.requested.0,
            requested_h = req.requested.1,
            screen_w = actual.screen.0,
            screen_h = actual.screen.1,
            crtc_w = cw,
            crtc_h = ch,
            "the desktop overrode the requested mode {}x{}; keeping the desktop's \
             {}x{} (e.g. Enlightenment's e_randr2 restoring its saved monitor config)",
            req.requested.0,
            req.requested.1,
            actual.screen.0,
            actual.screen.1
        );
    }
    *slot = Some(found);
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
/// `raw` is the value of `GHOSTFRAME_MAX_RESOLUTION`: `"WIDTHxHEIGHT"`
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
                "GHOSTFRAME_MAX_RESOLUTION is not WIDTHxHEIGHT or 'none'; ignoring"
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
    use std::time::{Duration, Instant};

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
        let ctl = XrandrDisplay::with_timing(Some(x.display()), None, None).unwrap();
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
        let ctl = XrandrDisplay::with_timing(Some(x.display()), None, None).unwrap();
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

    /// Short timings so the tests are quick: report after 1 s settle and 0.5 s
    /// of quiet, at the latest after 6 s.
    const T: ReadbackTiming = ReadbackTiming {
        settle: Duration::from_millis(1000),
        quiet: Duration::from_millis(500),
        cap: Duration::from_millis(6000),
    };

    /// Wide enough that scheduling jitter cannot reorder the steps of a test
    /// that must act before the settle time.
    const WIDE: ReadbackTiming = ReadbackTiming {
        settle: Duration::from_millis(2500),
        quiet: Duration::from_millis(1500),
        cap: Duration::from_millis(10_000),
    };

    fn with_readback(x: &PrivateXvfb) -> XrandrDisplay {
        XrandrDisplay::with_timing(Some(x.display()), None, Some(T)).unwrap()
    }

    fn wait_for(what: &str, mut ok: impl FnMut() -> bool) {
        let start = Instant::now();
        while !ok() {
            assert!(
                Instant::now().duration_since(start) < Duration::from_secs(10),
                "timed out: {what}"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    /// The screen's physical size as a new client sees it.
    fn screen_mm(display: &str) -> (u16, u16) {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let r = &conn.setup().roots[n];
        (r.width_in_millimeters, r.height_in_millimeters)
    }

    /// The pid of the X server behind `display`, from the socket's peer.
    fn server_pid(display: &str) -> libc::pid_t {
        let sock = std::os::unix::net::UnixStream::connect(format!(
            "/tmp/.X11-unix/X{}",
            display.trim_start_matches(':')
        ))
        .unwrap();
        let mut cred: libc::ucred = unsafe { std::mem::zeroed() };
        let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: `cred` and `len` are valid for the call.
        let r = unsafe {
            libc::getsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&raw mut cred).cast(),
                &mut len,
            )
        };
        assert_eq!(r, 0);
        cred.pid
    }

    fn screen_size(display: &str) -> (u16, u16) {
        let (conn, n) = x11rb::connect(Some(display)).unwrap();
        let g = conn
            .get_geometry(conn.setup().roots[n].root)
            .unwrap()
            .reply()
            .unwrap();
        (g.width, g.height)
    }

    /// A desktop that reverts the mode shortly after ours is noticed, and the
    /// picture the client gets is the desktop's. We never re-apply.
    #[test]
    fn a_desktop_reverting_the_mode_is_noticed_and_not_fought() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = with_readback(&x);
        ctl.set_output(800, 600, 1000).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        wait_for("override", || ctl.overridden().is_some());
        assert_eq!(
            ctl.overridden(),
            Some(Overridden {
                requested: (800, 600),
                screen: (1280, 1024),
                crtc: Some((1280, 1024)),
            })
        );
        // Not fought: the desktop's size still stands a moment later.
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(screen_size(x.display()), (1280, 1024));
    }

    /// A revert that arrives after the verdict -- the patched Enlightenment's
    /// case, with a settle shorter than its delay -- is still caught: a screen
    /// change after a verdict re-opens it.
    #[test]
    fn a_late_revert_is_caught() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = with_readback(&x);
        ctl.set_output(800, 600, 1000).unwrap();
        wait_for("first verdict", || {
            ctl.completed.load(Ordering::SeqCst) == 1
        });
        assert_eq!(ctl.overridden(), None, "800x600 stuck so far");
        std::thread::sleep(Duration::from_millis(700));
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        wait_for("late override", || ctl.overridden().is_some());
        assert_eq!(ctl.overridden().unwrap().screen, (1280, 1024));
    }

    /// Some -> None: an override, then a request that sticks clears it, and
    /// the worker demonstrably ran for both. Its own, wide timing: the revert
    /// must land well before the first verdict, or the count below is wrong.
    #[test]
    fn a_request_that_sticks_clears_an_earlier_override() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = XrandrDisplay::with_timing(Some(x.display()), None, Some(WIDE)).unwrap();
        let start = Instant::now();
        ctl.set_output(800, 600, 1000).unwrap();
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 0);
        assert!(Instant::now().duration_since(start) < WIDE.settle);
        wait_for("override", || ctl.overridden().is_some());
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 1);
        ctl.set_output(800, 600, 1000).unwrap();
        wait_for("second verdict", || {
            ctl.completed.load(Ordering::SeqCst) == 2
        });
        assert_eq!(ctl.overridden(), None);
        assert_eq!(screen_size(x.display()), (800, 600));
    }

    /// Two requests inside one window: only the second owns the verdict, even
    /// though the first would have "stuck" at its own size.
    #[test]
    fn a_superseded_request_gives_no_verdict() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = with_readback(&x);
        ctl.set_output(800, 600, 1000).unwrap();
        ctl.set_output(1280, 1024, 1000).unwrap();
        wait_for("verdict", || ctl.completed.load(Ordering::SeqCst) >= 1);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(ctl.overridden(), None);
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 1);
    }

    /// A call that fails still bumps the generation, so it disarms the
    /// read-back of the call before it: no verdict is reached for either.
    #[test]
    fn a_failed_call_disarms_the_previous_readback() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = with_readback(&x);
        ctl.set_output(800, 600, 1000).unwrap();
        ctl.fail_set_crtc.store(true, Ordering::SeqCst);
        assert!(ctl.set_output(1280, 1024, 1000).is_err());
        std::thread::sleep(Duration::from_millis(2500));
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 0);
    }

    /// A SetCrtcConfig that fails after the screen was resized leaves the
    /// output as it was: same screen size, CRTC showing the old mode.
    #[test]
    fn a_failed_change_puts_the_screen_and_crtc_back() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = with_readback(&x);
        // A scale of 2 gives the screen a physical size no default would.
        ctl.set_output(1280, 1024, 2000).unwrap();
        let mm = screen_mm(x.display());
        assert_eq!(mm, (169, 135), "precondition: a distinctive size");
        ctl.fail_set_crtc.store(true, Ordering::SeqCst);
        assert!(ctl.set_output(800, 600, 1000).is_err());
        assert_eq!(screen_size(x.display()), (1280, 1024));
        assert_eq!(screen_mm(x.display()), mm, "the physical size came back");
        let info = ctl
            .conn
            .randr_get_crtc_info(ctl.crtc, 0)
            .unwrap()
            .reply()
            .unwrap();
        assert_ne!(info.mode, x11rb::NONE, "the output went dark");
        assert_eq!((info.width, info.height), (1280, 1024));
        // The grab was released: an ordinary request still works.
        ctl.fail_set_crtc.store(false, Ordering::SeqCst);
        ctl.set_output(800, 600, 1000).unwrap();
        assert_eq!(screen_size(x.display()), (800, 600));
    }

    /// The race itself, made deterministic: a call that begins between the
    /// worker's read and its write owns the verdict, so the stale one must
    /// not be recorded.
    #[test]
    fn a_verdict_is_dropped_when_a_call_began_after_the_read() {
        let generation = AtomicU64::new(2); // a newer call already bumped it
        let overridden = Mutex::new(None);
        let completed = AtomicU64::new(0);
        let req = Request {
            ticket: 1,
            requested: (800, 600),
        };
        let actual = Actual {
            screen: (1280, 1024),
            crtc: Some((1280, 1024)),
        };
        record_verdict(&generation, &overridden, &completed, req, &actual);
        assert_eq!(*overridden.lock().unwrap(), None);
        assert_eq!(completed.load(Ordering::SeqCst), 0);
        generation.store(1, Ordering::SeqCst);
        record_verdict(&generation, &overridden, &completed, req, &actual);
        assert!(overridden.lock().unwrap().is_some());
    }

    #[test]
    fn due_follows_settle_quiet_and_cap() {
        let t = ReadbackTiming {
            settle: Duration::from_secs(5),
            quiet: Duration::from_millis(1500),
            cap: Duration::from_secs(12),
        };
        let ms = Duration::from_millis;
        // Settle not reached, however quiet the screen has been.
        assert!(!due(ms(4000), ms(4000), ms(4000), &t));
        // Settle reached, but an event 500 ms ago.
        assert!(!due(ms(5200), ms(500), ms(5200), &t));
        // Both reached.
        assert!(due(ms(5200), ms(1500), ms(5200), &t));
        // Never quiet, but the cap passed.
        assert!(!due(ms(11_900), ms(10), ms(11_900), &t));
        assert!(due(ms(12_000), ms(10), ms(12_000), &t));
        // A re-opened watch: `elapsed` starts at settle, but the cap counts
        // from the apply.
        assert!(!due(ms(5100), ms(10), ms(11_900), &t));
        assert!(due(ms(5100), ms(10), ms(12_000), &t));
    }

    /// Dropping the controller ends the worker (Drop joins it, so this would
    /// hang if the hang-up were not noticed; the thread bounds it).
    #[test]
    fn the_worker_exits_when_the_controller_is_dropped() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        let ctl = with_readback(&x);
        let exited = Arc::clone(&ctl.worker_exited);
        assert!(!exited.load(Ordering::SeqCst));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            drop(ctl);
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("drop did not return: the worker never noticed the hang-up");
        assert!(exited.load(Ordering::SeqCst));
    }

    /// An X server that stops answering must not hang the drop: the worker
    /// is blocked in a round trip, which only shutting its socket down
    /// interrupts. (The 500 ms join bound is the second line of defence and
    /// is not separately exercised: with the shutdown working it never fires.)
    #[test]
    fn dropping_the_controller_does_not_wait_for_a_stopped_x_server() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        let t = ReadbackTiming {
            settle: Duration::from_millis(100),
            quiet: Duration::from_millis(50),
            cap: Duration::from_millis(1000),
        };
        let ctl = XrandrDisplay::with_timing(Some(x.display()), None, Some(t)).unwrap();
        ctl.set_output(1000, 700, 1000).unwrap();
        let pid = server_pid(x.display());
        // SAFETY: signalling the private Xvfb this test started.
        unsafe { libc::kill(pid, libc::SIGSTOP) };
        // The read-back falls due at ~150 ms and blocks on the stopped server.
        std::thread::sleep(Duration::from_millis(600));
        let (tx, rx) = std::sync::mpsc::channel();
        let began = Instant::now();
        std::thread::spawn(move || {
            drop(ctl);
            let _ = tx.send(());
        });
        let returned = rx.recv_timeout(Duration::from_secs(3));
        let took = Instant::now().duration_since(began);
        // SAFETY: as above.
        unsafe { libc::kill(pid, libc::SIGCONT) };
        returned.expect("drop hung on a stopped X server");
        assert!(
            took < Duration::from_millis(400),
            "took {took:?}: only the join bound saved it"
        );
    }

    /// Events inside the quiet period postpone the verdict. The desktop
    /// changes the mode away shortly before the settle time and back shortly
    /// after it: judged at the settle time (no quiet period) the screen would
    /// look overridden; judged once quiet, it is not.
    #[test]
    fn the_verdict_waits_for_the_screen_to_go_quiet() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let ctl = XrandrDisplay::with_timing(Some(x.display()), None, Some(WIDE)).unwrap();
        let start = Instant::now();
        ctl.set_output(800, 600, 1000).unwrap();
        std::thread::sleep(Duration::from_millis(2000));
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        let away = Instant::now();
        // Preconditions, pinned: still before settle, nothing judged yet.
        assert!(away.duration_since(start) < WIDE.settle, "ran too late");
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 0);
        std::thread::sleep(Duration::from_millis(1000));
        xrandr(x.display(), &["--output", "screen", "--mode", "800x600_t"]);
        // Back inside the quiet period of the change away, after settle.
        assert!(
            Instant::now().duration_since(away) < WIDE.quiet,
            "ran too late"
        );
        assert!(Instant::now().duration_since(start) > WIDE.settle);
        let polling = Instant::now();
        while Instant::now().duration_since(polling) < Duration::from_millis(5500) {
            assert_eq!(ctl.overridden(), None, "judged mid-flap");
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 1);
    }

    /// The watch for a late revert ends `cap` after the apply: a change long
    /// after that is the user's, not the desktop undoing us.
    #[test]
    fn the_late_revert_watch_is_bounded() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        add_800x600(x.display());
        let t = ReadbackTiming {
            settle: Duration::from_millis(300),
            quiet: Duration::from_millis(200),
            cap: Duration::from_millis(2000),
        };
        let ctl = XrandrDisplay::with_timing(Some(x.display()), None, Some(t)).unwrap();
        ctl.set_output(800, 600, 1000).unwrap();
        wait_for("first verdict", || {
            ctl.completed.load(Ordering::SeqCst) == 1
        });
        std::thread::sleep(Duration::from_millis(3000));
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        std::thread::sleep(Duration::from_millis(1200));
        assert_eq!(ctl.completed.load(Ordering::SeqCst), 1);
        assert_eq!(ctl.overridden(), None);
    }

    /// A mode of ours the desktop reverted away from is still collected by
    /// the next call, though it was never the "current" mode.
    #[test]
    fn a_reverted_mode_is_collected_on_the_next_call() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        let ctl = with_readback(&x);
        ctl.set_output(1000, 700, 1000).unwrap();
        xrandr(x.display(), &["--output", "screen", "--mode", "1280x1024"]);
        ctl.set_output(900, 650, 1000).unwrap();
        let ours: Vec<String> = mode_names(x.display())
            .into_iter()
            .filter(|n| n.starts_with("ghostframe_"))
            .collect();
        assert_eq!(ours, ["ghostframe_904x650"]);
    }

    #[test]
    fn old_cvt_modes_are_deleted_once_unused() {
        let x = PrivateXvfb::spawn(&["-screen", "0", "1280x1024x24"]);
        let ctl = with_readback(&x);
        for (w, h) in [(1000, 700), (900, 650), (1100, 800)] {
            ctl.set_output(w, h, 1000).unwrap();
        }
        let ours: Vec<String> = mode_names(x.display())
            .into_iter()
            .filter(|n| n.starts_with("ghostframe_"))
            .collect();
        // CVT rounds the width up to a multiple of 8: 1100 -> 1104.
        assert_eq!(ours, ["ghostframe_1104x800"]);
    }
}
