//! `capture`: the daemon. Waits for X, connects the backend, joins the
//! tailnet and streams frames while a client is connected.

use std::time::Duration;

use ghostframe_lib::{GhostbridgeConfig, GhostframeServer};

use crate::capture::tick_stats::thread_cpu_us;
use crate::capture::Backend;
use crate::config::ServerConfig;
use crate::state;

/// The exit code for a failed [`X11Backend::new`](crate::capture::x11::X11Backend::new).
/// `ErrorKind::Unsupported` is its contract for "this session can never be
/// captured" (a 16 bpp or non-BGRX root): exit 2, which the unit does not
/// restart on. Anything else (a refused connection, an X server still
/// starting) may clear up, so it stays a plain failure the unit retries.
fn backend_connect_exit_code(e: &std::io::Error) -> i32 {
    if e.kind() == std::io::ErrorKind::Unsupported {
        2
    } else {
        1
    }
}

/// Run until the server shuts down. Startup failures that need the operator
/// exit the process with code 2 and a message saying what to do.
pub async fn run(cfg: ServerConfig) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Checked first, and regardless of DISPLAY: the installed unit hard-codes
    // DISPLAY=:0, so on a Wayland session DISPLAY is always set and, without
    // this, capture would stream the XWayland root without saying so.
    if let Some(w) = cfg.wayland_display.as_deref() {
        eprintln!(
            "error: Wayland sessions are not supported yet: ghostframe-server captures \
             X11, and WAYLAND_DISPLAY={w} is set. If this is an X11 session with a stale \
             value in the user manager, `systemctl --user unset-environment \
             WAYLAND_DISPLAY` clears it"
        );
        std::process::exit(2);
    }

    let state_dir_path = match cfg.state_dir().and_then(|p| {
        crate::util::ensure_private_dir(&p)?;
        Ok(p)
    }) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };
    let state_dir = state_dir_path.to_string_lossy().into_owned();
    // Held until the process exits: no second tsnet may write this state.
    let _state_lock = match state::StateLock::acquire(&state_dir_path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };

    // An unseeded node can only join non-interactively, with TS_AUTHKEY (the
    // e2e image does exactly that). A person runs `login` once instead.
    let authkey = match (
        cfg.ts_authkey.clone().filter(|k| !k.is_empty()),
        state::state_dir_seeded(&state_dir_path),
    ) {
        (Some(k), _) => k,
        (None, true) => String::new(),
        (None, false) => {
            eprintln!(
                "error: this node has not joined a tailnet yet ({state_dir:?} holds no \
                 completed join; tsnet's own tailscaled.state does not count).\n\
                 run `ghostframe-server login` (it starts the service)"
            );
            std::process::exit(2);
        }
    };
    let capture_fps = cfg.capture_fps;

    let config = bridge_config(&cfg, authkey, state_dir);

    if let Err(msg) =
        crate::capture::x11::wait_for_x11(Duration::from_secs(10), cfg.display.as_deref())
    {
        let dpy = cfg.display.as_deref().unwrap_or("(unset)");
        tracing::error!(
            "{msg}\n\
             The display is the user's own session, not ghostframe's to start. Confirm\n\
             this user can open it: `DISPLAY={dpy} xrandr --current`. The same failure\n\
             there means the X server refused the connection -- a wedged or exited\n\
             session, not a ghostframe misconfiguration."
        );
        std::process::exit(2);
    }

    // The X server is up: connect the backend (frames, damage, input, display).
    let mut backend = match crate::capture::x11::X11Backend::new(&cfg) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            let code = backend_connect_exit_code(&e);
            if code == 2 {
                eprintln!("this is a property of the session, not a transient failure");
            }
            std::process::exit(code);
        }
    };

    let lib_config = match ghostframe_lib::config::LibConfig::from_env() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("{e}; refusing to start");
            std::process::exit(2);
        }
    };
    tracing::info!(?lib_config, "effective LibConfig");

    tracing::info!("Connecting to Tailscale...");
    let server = match GhostframeServer::new(
        config,
        ":443",
        lib_config,
        backend.input_injector(),
        backend.display_controller(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            if needs_login(e.as_ref()) {
                eprintln!(
                    "error: the node is not authorised (key expired or removed?); \
                     run `ghostframe-server login`"
                );
                std::process::exit(2);
            }
            if let Some(ws) = e.downcast_ref::<ghostframe_lib::WebServerError>() {
                if matches!(ws, ghostframe_lib::WebServerError::HttpsCertsDisabled) {
                    tracing::error!(
                        "HTTPS Certificates are not enabled for this tailnet. \
                         Enable them at https://login.tailscale.com/admin/dns and restart."
                    );
                    std::process::exit(2);
                }
            }
            return Err(e);
        }
    };

    // The join completed (an auth-key join, or already-authorised state).
    // Without the marker a later start would not count this state as joined.
    if let Err(e) = state::mark_joined(&state_dir_path) {
        tracing::warn!(error = %e, "could not record the join; the next start will ask for `login`");
    }

    tracing::info!("Server ready, entering capture loop");

    let frame_interval = Duration::from_micros(1_000_000 / capture_fps);
    let mut frame_count = 0u64;
    // Tracks whether the loop is currently in idle (no-client) mode, so we
    // emit a single info log on each idle↔active flip. The IoBridge logs the
    // corresponding event on its side; this log shows the capture loop's view
    // (root grab suspended/resumed).
    // None = nothing logged yet, so the FIRST pass through the loop announces
    // whichever state it finds. Seeded `true` this stayed silent on the most
    // common startup path -- no client yet -- so the daemon logged "entering
    // capture loop" and then went permanently quiet at 0% CPU with no frames.
    // That is correct behaviour (capture is gated on a connected client) but it
    // reads exactly like a hang, and cost real time to tell apart from one.
    let mut was_idle: Option<bool> = None;

    // Optional capture-dump diagnostic: when set, write each captured frame
    // as a PNG to this directory (filename: frame_<count>.png). Useful for
    // visually verifying the server's view of the desktop alongside trace
    // logs.  Disabled when the env var is absent.
    let capture_dump_dir = cfg.capture_dump_dir.clone();
    if let Some(dir) = &capture_dump_dir {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(?dir, error = %e, "could not create capture-dump dir; PNG dump disabled");
        } else {
            tracing::info!(?dir, "capture-dump enabled — writing frame PNGs");
        }
    }

    // Heartbeat cadence — emit one debug line every Nth iteration so we can
    // tell from logs whether the capture loop is alive, blocked on
    // submit_frame (= IoBridge backpressure), or has exited.
    let heartbeat_every = cfg.heartbeat_every;
    let mut capture_failures = CaptureFailures::default();
    // getrusage costs a syscall; only pay it when the trace line is wanted.
    let cost_trace = tracing::enabled!(
        target: "ghostframe::server::capture",
        tracing::Level::TRACE
    );

    loop {
        let loop_iter_start = std::time::Instant::now();
        // Skip the capture scrape entirely when nobody is connected. Without
        // this gate, the root grab copies a full 1920x1080 framebuffer through
        // the X server every 33 ms regardless of demand.
        //
        // `wait_for_client` parks on the connect event itself. The earlier
        // version slept a fixed 500 ms and only re-read the gate on waking,
        // which put up to the whole interval between a client connecting and
        // the first frame being scraped -- for no benefit, since the gate is
        // already refreshed the moment a session's H3 handshake completes.
        if server.connected_session_count() == 0 {
            if was_idle != Some(true) {
                // Tear the damage subscription down with the gate. Draining
                // and discarding instead would leave the X server generating
                // one RAW_RECTANGLES event per damaged rectangle for output
                // nobody reads -- and would need a timer to do the
                // draining, which is the idle poll this loop no longer has.
                backend.disarm_damage();
                tracing::info!(
                    "no client connected; capture loop parked until one arrives \
                     (XDamage disarmed)"
                );
                was_idle = Some(true);
            }
            if let Err(e) = server.wait_for_client().await {
                // The IoBridge event loop is gone, so no client can ever
                // arrive. Previously this loop could only notice a dead
                // server via a failed `submit_frame` -- which it never
                // reaches while the gate is shut -- so it polled a dead
                // server at 2 Hz forever. Exits the same way the
                // `submit_frame` path does (warn + Ok), so the process exit
                // code is unchanged.
                tracing::warn!(frames = frame_count, "{e}; capture loop exiting");
                break;
            }
            // Re-check from the top: the client may already be gone again.
            continue;
        }
        if was_idle != Some(false) {
            backend.arm_damage();
            // `xdamage_armed` is the one place an operator can tell a working
            // damage path from a silently dead one: an unarmed monitor
            // reports no dirty tiles, every frame falls back to full-frame
            // comparison, and the output stays correct while costing more.
            tracing::info!(
                xdamage_armed = backend.damage_armed(),
                "client connected; capture loop resuming"
            );
            was_idle = Some(false);
        }

        let capture_start = std::time::Instant::now();
        // Only the ticks whose line is logged (`frame_count` is bumped before it).
        let sample_cpu = cost_trace && (frame_count + 1).is_multiple_of(heartbeat_every);
        let cpu_start = if sample_cpu { thread_cpu_us() } else { 0 };

        let timestamp_us = (frame_count * frame_interval.as_micros() as u64) as u32;
        let (damage, grabbed) = crate::capture::damage_then_grab(&mut backend, timestamp_us);
        let submission = match grabbed {
            Ok(mut frame) => {
                // A bitmap built for another screen size says nothing about
                // this frame: the resize happened between drain and grab.
                frame.damage_tiles = damage.into_hint_for(frame.width, frame.height);
                Some(frame)
            }
            Err(e) => {
                tracing::warn!("X11 capture failed: {e}");
                None
            }
        };

        // No await between `cpu_start` and here, so this is one thread.
        let cpu_us = if sample_cpu {
            thread_cpu_us().saturating_sub(cpu_start)
        } else {
            0
        };
        let tick_stats = backend.take_tick_stats();
        let capture_done = std::time::Instant::now();
        let capture_us = capture_done.duration_since(capture_start).as_micros() as u64;

        if let Some(frame) = submission {
            let frame_w = frame.width;
            let frame_h = frame.height;
            let frame_stride = frame.stride;
            let frame_pixels_len = frame.pixels.len();

            // Optional PNG dump of the captured frame so we can SEE what the
            // daemon is feeding into the encoder pipeline (gated on
            // GHOSTFRAME_CAPTURE_DUMP_DIR).  Runs BEFORE submit so we still
            // get a file even if submit blocks/fails.
            if let Some(dump_dir) = &capture_dump_dir {
                if !frame.pixels.is_empty() {
                    dump_frame_as_png(dump_dir, frame_count, &frame);
                }
            }

            let submit_start = std::time::Instant::now();
            let submit_res = server.submit_frame(frame).await;
            #[allow(
                clippy::disallowed_methods,
                reason = "ghostframe-server is a standalone capture-loop process with its own tokio runtime, never driven by the browserless harness's paused clock -- submit_us is real wall-clock latency telemetry for how long submit_frame() actually took"
            )]
            let submit_us = submit_start.elapsed().as_micros() as u64;
            if let Err(e) = submit_res {
                tracing::warn!(
                    frames = frame_count,
                    "submit_frame returned Err — channel closed, capture loop exiting: {e}"
                );
                break;
            }
            frame_count += 1;
            if frame_count == 1 {
                tracing::info!("first frame submitted");
            } else if frame_count.is_multiple_of(30) {
                // Bumped from 300 → 30 so e2e runs (~10-20s wall) see at
                // least one milestone.  Stays infrequent enough not to
                // dominate prod logs.
                tracing::info!(frames = frame_count, "capture running");
            }
            // Per-iter heartbeat — trace level so prod and the default e2e
            // log volume stay quiet (RUST_LOG now defaults to
            // `ghostframe=info`; the e2e harness sets `ghostframe=trace,debug`
            // which still hits trace for ghostframe targets when explicitly
            // debugging).
            // Captures the four timing components separately so we can spot
            // exactly which one stalls when the daemon goes silent.
            if frame_count.is_multiple_of(heartbeat_every) {
                tracing::trace!(
                    target: "ghostframe::server::capture",
                    frames = frame_count,
                    w = frame_w,
                    h = frame_h,
                    stride = frame_stride,
                    pixels_len = frame_pixels_len,
                    capture_us = capture_us,
                    submit_us = submit_us,
                    cpu_us = cpu_us,
                    bytes_copied = tick_stats.bytes_copied,
                    allocs = tick_stats.allocs,
                    "[CAP] iter done"
                );
            }
            capture_failures.record_success();
        } else {
            tracing::trace!(
                target: "ghostframe::server::capture",
                frames = frame_count,
                capture_us = capture_us,
                "[CAP] capture returned None (no submission this iter)"
            );
            // A capture that fails every iteration used to warn 30x/s forever
            // while the process stayed up, so systemd (Restart=on-failure)
            // never got a failure to act on. Exit non-zero instead.
            if capture_failures.record_failure() {
                tracing::error!(
                    consecutive = capture_failures.consecutive,
                    "capture failed on every attempt; exiting so the service manager restarts us"
                );
                return Err(format!(
                    "capture failed {} consecutive times",
                    capture_failures.consecutive
                )
                .into());
            }
        }

        #[allow(
            clippy::disallowed_methods,
            reason = "ghostframe-server paces real screen capture against real wall-clock frame_interval on its own tokio runtime (not the browserless harness's paused one) -- this elapsed time directly controls how long tokio::time::sleep below actually sleeps"
        )]
        let elapsed = loop_iter_start.elapsed();
        if elapsed < frame_interval {
            tokio::time::sleep(frame_interval - elapsed).await;
        }
    }

    tracing::warn!(frames = frame_count, "capture loop exited");
    Ok(())
}

/// The tailnet settings `capture` runs with.
fn bridge_config(cfg: &ServerConfig, authkey: String, state_dir: String) -> GhostbridgeConfig {
    GhostbridgeConfig {
        hostname: cfg.ts_hostname.clone(),
        // Without a key nobody is there to answer a sign-in URL, and the URL
        // in the journal would be claimable by whoever can read it: fail
        // instead (`needs_login`).
        non_interactive: authkey.is_empty(),
        // Always: a key can expire and a node can be removed after start.
        redact_auth_urls: true,
        authkey,
        state_dir,
        control_url: cfg.ts_control_url.clone(),
        quiet: false,
    }
}

/// Whether `e` (or anything it wraps) is a non-interactive `up` that found the
/// node needing a login.
fn needs_login(e: &(dyn std::error::Error + 'static)) -> bool {
    let mut cur = Some(e);
    while let Some(e) = cur {
        if let Some(ghostframe_lib::GhostbridgeError::Ffi("gbridge_up", rc)) =
            e.downcast_ref::<ghostframe_lib::GhostbridgeError>()
        {
            return *rc == ghostframe_lib::transport::ghostbridge::UP_NEEDS_LOGIN_RC;
        }
        cur = e.source();
    }
    false
}

/// Consecutive failed captures tolerated before the daemon exits (about ten
/// seconds at the default 30 fps). Transient failures (a mode switch, a
/// momentary DRM master loss) clear well inside this.
const MAX_CONSECUTIVE_CAPTURE_FAILURES: u32 = 300;

#[derive(Debug, Default)]
struct CaptureFailures {
    consecutive: u32,
}

impl CaptureFailures {
    fn record_success(&mut self) {
        self.consecutive = 0;
    }

    /// Returns true once the failure budget is exhausted.
    fn record_failure(&mut self) -> bool {
        self.consecutive = self.consecutive.saturating_add(1);
        self.consecutive >= MAX_CONSECUTIVE_CAPTURE_FAILURES
    }
}

/// Write a captured BGRA frame to `<dir>/frame_<count>.png`.  Best-effort:
/// logs and continues on any error so a failed dump never disturbs the live
/// capture path.  Gated by `GHOSTFRAME_CAPTURE_DUMP_DIR` env var.
fn dump_frame_as_png(
    dump_dir: &std::path::Path,
    count: u64,
    frame: &ghostframe_lib::server::FrameSubmission,
) {
    let path = dump_dir.join(format!("frame_{count:05}.png"));
    let w = frame.width;
    let h = frame.height;
    let stride = frame.stride as usize;
    let bgra = &frame.pixels;
    // Allocate tightly-packed RGBA8 (image crate accepts no stride param).
    let row_bytes = (w as usize).saturating_mul(4);
    if stride == 0 || bgra.len() < (h as usize).saturating_mul(stride.max(row_bytes)) {
        tracing::warn!(
            ?path,
            w,
            h,
            stride,
            len = bgra.len(),
            "pixel buffer too small for frame dims; skipping PNG dump"
        );
        return;
    }
    let mut rgba = vec![0u8; (h as usize).saturating_mul(row_bytes)];
    for y in 0..h as usize {
        let src_off = y.saturating_mul(stride);
        let dst_off = y.saturating_mul(row_bytes);
        for x in 0..w as usize {
            let s = src_off + x * 4;
            let d = dst_off + x * 4;
            // Wire format is BGRA; PNG expects RGBA. Swap B↔R and force
            // alpha=255 so the dump renders opaque even when X11's X-channel
            // happens to be zero.
            rgba[d] = bgra[s + 2];
            rgba[d + 1] = bgra[s + 1];
            rgba[d + 2] = bgra[s];
            rgba[d + 3] = 0xff;
        }
    }
    match image::save_buffer(&path, &rgba, w, h, image::ColorType::Rgba8) {
        Ok(()) => {
            tracing::debug!(target: "ghostframe::server::capture", ?path, count, "[CAP-PNG] wrote frame")
        }
        Err(e) => tracing::warn!(?path, error = %e, "failed to write capture PNG"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ghostframe_lib::GhostbridgeError;

    #[test]
    fn only_an_unsupported_session_is_exit_2_not_a_transient_failure() {
        use std::io::{Error, ErrorKind};
        let code = |k| backend_connect_exit_code(&Error::new(k, "x"));
        assert_eq!(code(ErrorKind::Unsupported), 2);
        // X not up yet, or gone again: the unit may retry.
        assert_eq!(code(ErrorKind::ConnectionRefused), 1);
        assert_eq!(code(ErrorKind::Other), 1);
        assert_eq!(code(ErrorKind::TimedOut), 1);
    }

    #[test]
    fn capture_always_redacts_and_only_a_keyless_one_is_non_interactive() {
        let cfg = ServerConfig::from_lookup(|_| None, Some("h".into()));
        let keyed = bridge_config(&cfg, "tskey-x".into(), "/s".into());
        assert!(keyed.redact_auth_urls && !keyed.non_interactive);
        let keyless = bridge_config(&cfg, String::new(), "/s".into());
        assert!(keyless.redact_auth_urls && keyless.non_interactive);
        assert!(!keyless.quiet);
    }

    #[test]
    fn only_a_needs_login_up_failure_maps_to_the_login_message() {
        let up = |rc| -> Box<dyn std::error::Error + Send + Sync> {
            Box::new(GhostbridgeError::Ffi("gbridge_up", rc))
        };
        assert!(needs_login(up(-7).as_ref()));
        assert!(!needs_login(up(-2).as_ref()), "an ordinary up failure");
        let other: Box<dyn std::error::Error + Send + Sync> =
            Box::new(GhostbridgeError::Ffi("gbridge_new", -7));
        assert!(!needs_login(other.as_ref()));
    }

    #[test]
    fn capture_failures_trip_after_budget_and_reset_on_success() {
        let mut f = CaptureFailures::default();
        for _ in 0..MAX_CONSECUTIVE_CAPTURE_FAILURES - 1 {
            assert!(!f.record_failure());
        }
        // A success in the middle of a streak restarts the count.
        f.record_success();
        for _ in 0..MAX_CONSECUTIVE_CAPTURE_FAILURES - 1 {
            assert!(!f.record_failure());
        }
        assert!(f.record_failure(), "budget exhausted must trip");
    }
}
