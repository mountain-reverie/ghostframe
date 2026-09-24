//! M2 acceptance: the showcase window against a live server, under Weston.
//!
//! This is the first runtime proof of the M2 window code. Everything before
//! it was verified by compilation and unit tests only, because the dev
//! machine has no display server.
//!
//! Requires Docker, a GPU and Weston. Deliberately NOT named in any CI
//! workflow.
//!
//! # Driving approach: the library directly, not the CLI binary
//!
//! `ghostframe connect` needs a logged-in tsnet node, and the harness
//! already owns one (`setup._test_node`). Two ways to get the window loop
//! running against it:
//!
//! (a) Spawn the `ghostframe` binary as a subprocess and give it its own
//!     tsnet node. `native_client.rs`'s module doc established that a
//!     *second* `tsnet.Server` in the same OS *process* never converges a
//!     working peer datapath; a subprocess sidesteps that specific failure
//!     mode, but it is unverified territory and could easily burn a day
//!     bisecting something else entirely (e.g. whether the two nodes' DERP
//!     sessions interfere via headscale, or whether stdout/stderr plumbing
//!     from a spawned GUI-ish process is even reliable in this harness).
//!
//! (b) Drive the library directly: build a `Client` in-test with
//!     `attach_bridge(setup._test_node.bridge())` (exactly `native_client.rs`'s
//!     proven pattern), open a `window::Backend` against Weston, and run
//!     the CLI's own event loop. This is what this test does.
//!
//! (b) was chosen. It reuses the one pattern already proven to converge a
//! peer datapath, and it still exercises exactly the code this milestone
//! added -- the window backends and the render/event loop -- which is the
//! runtime proof that matters here. The cost is that it does not exercise
//! `ghostframe-cli`'s own argument parsing or its `login`-state check in
//! `commands::connect`; those are covered by `ghostframe-cli`'s existing
//! unit/integration tests (`tests/cli.rs`) without needing a display at all.
//!
//! `commands::run_window_loop` was made `pub` and given `on_frame_presented`/
//! `should_quit` hooks for exactly this: so a test can call the real loop
//! with an already-connected `Client` and an already-open `Backend`,
//! without going through `connect`'s login check or its own tsnet node. See
//! that function's doc comment in `ghostframe-cli/src/commands.rs`.
//!
//! # The quit chord: routing is asserted separately, not synthesised here
//!
//! Driving a genuine `d`/`h`-after-prefix key sequence through Weston would
//! need either a virtual-input Wayland protocol (`virtual-keyboard-unstable-v1`
//! or `wlr-virtual-pointer-unstable-v1`) or compositor-side injection. Neither
//! is available: this build's Weston (15.0.1) only advertises
//! `weston-content-protection`, `weston-debug`, `weston-direct-display` and
//! `weston-output-capture` (checked via `pacman -Ql weston | grep protocol`),
//! and the headless backend has no real input devices for the compositor to
//! synthesise from on its own. So genuine synthetic input is not reachable
//! here, and this test does not fake one.
//!
//! What it does instead: the chord -> `LoopAction` routing itself is already
//! covered, without a display, by `ghostframe-cli/tests/event_loop.rs`'s
//! `a_completed_quit_chord_yields_quit` and `close_requested_yields_quit`.
//! This test proves the *loop's* clean-exit path -- return `Ok(())`, no
//! panic, after however many events were pending -- using the `should_quit`
//! hook `run_window_loop` now exposes for testability, which returns exactly
//! where `ChordAction::Quit` or `WindowEvent::CloseRequested` would.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostframe_cli::chord::{Chord, Prefix};
use ghostframe_cli::commands::run_window_loop;
use ghostframe_cli::window;
use ghostframe_client_native::{Client, Config};
use ghostframe_e2e::harness::{
    read_server_logs_stripped, setup_e2e_server, spawn_weston_headless, E2eServerSpec,
};

/// How many real, compositor-accepted presents this test waits for before
/// asking the loop to stop. More than one, so a single lucky frame can't
/// pass this the way it could for a "process stayed alive" assertion.
const FRAMES_TO_OBSERVE: u64 = 3;

#[tokio::test(flavor = "multi_thread")]
async fn showcase_presents_frames_under_weston_and_exits_cleanly() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        // Pin tile mode. Sessions start in H.264 (`io_bridge.rs:3435`), so
        // once the CLI advertises the capability this test would silently
        // become an H.264 test instead of the tile-path test it was written
        // as. That is covered by `tests/h264.rs`; this one stays what it is.
        extra_env: &[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "tile")],
        // The GPU under test is the CLIENT's (via the Wayland/dmabuf path);
        // keep the server on its CPU capture path, matching native_client.rs.
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    eprintln!("[phase] spawning headless weston");
    let weston = spawn_weston_headless().expect("spawn weston (is the weston package installed?)");

    // Point backend selection at Weston's WAYLAND socket, not its XWayland
    // DISPLAY -- the browser tests use the XWayland side, this one wants
    // the native Wayland backend the M2 window code actually added.
    // `DISPLAY` is explicitly unset so `window::select_backend` picks
    // Wayland deterministically rather than by the accident of whatever
    // was in this process's environment already (see its doc: WAYLAND_DISPLAY
    // wins if both are set, but don't rely on that here).
    eprintln!(
        "[phase] pointing WAYLAND_DISPLAY at weston's socket ({}), unsetting DISPLAY",
        weston.wayland_display
    );
    std::env::set_var("WAYLAND_DISPLAY", &weston.wayland_display);
    std::env::set_var("XDG_RUNTIME_DIR", weston.runtime_dir());
    std::env::remove_var("DISPLAY");

    eprintln!("[phase] opening the window backend");
    let mut backend =
        window::open("ghostframe-showcase-test").expect("open a window backend against weston");
    let preferred_modifiers = backend.preferred_dmabuf_modifiers();
    eprintln!("[phase] backend open; preferred dmabuf modifiers: {preferred_modifiers:?}");

    // Reuse the tsnet node `setup_e2e_server` already joined -- see the
    // module doc for why this, and not a second (CLI-owned) node, was
    // chosen.
    eprintln!("[phase] containers + backend up; building client");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "showcase-test".into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers,
        debug_map_frames: false,
        max_decode_width: 0,
        max_decode_height: 0,
    })
    .expect("create client");
    client.attach_bridge(setup._test_node.bridge());

    eprintln!(
        "[phase] connecting to {}:443 over tsnet",
        setup.server_container_name
    );
    if let Err(e) = client.connect(&setup.server_container_name, 443) {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("connect over the tailnet failed: {e}");
    }

    let mut chord = Chord::new(Prefix::CtrlAltB);
    let frames_presented = AtomicU64::new(0);
    // Bounds the run independently of frame count: a bug that stops frames
    // arriving must fail loudly rather than hang the test process forever
    // (`run_window_loop`'s own poll loop has no deadline of its own -- that
    // is the caller's job, and this is that caller).
    #[allow(
        clippy::disallowed_methods,
        reason = "real wall-clock deadline for a real-Docker/real-compositor test, not exposed to any virtual clock"
    )]
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut timed_out = false;

    eprintln!("[phase] running the window loop, waiting for {FRAMES_TO_OBSERVE} presented frames");
    let result = run_window_loop(
        &mut client,
        backend.as_mut(),
        &mut chord,
        &mut |n| {
            frames_presented.store(n, Ordering::SeqCst);
            eprintln!("[phase] presented frame {n}");
        },
        &mut || {
            if frames_presented.load(Ordering::SeqCst) >= FRAMES_TO_OBSERVE {
                return true;
            }
            #[allow(
                clippy::disallowed_methods,
                reason = "same real wall-clock deadline check as above"
            )]
            let past_deadline = Instant::now() >= deadline;
            if past_deadline {
                timed_out = true;
                return true;
            }
            false
        },
    );

    eprintln!(
        "[phase] window loop returned: {:?}",
        result.as_ref().map(|_| "ok")
    );
    if result.is_err() || timed_out {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
    }

    // Clean exit, no panic: `run_window_loop` itself must return `Ok(())`.
    result.expect("window loop ran to completion without error");

    assert!(
        !timed_out,
        "timed out after 60s waiting for {FRAMES_TO_OBSERVE} presented frames; only got {}",
        frames_presented.load(Ordering::SeqCst)
    );

    // Load-bearing: a frame was actually presented THROUGH THE REAL WAYLAND
    // BACKEND (attach + damage + commit accepted by weston), not merely that
    // the process stayed alive -- that would pass even with a black window
    // (see `on_frame_presented`'s placement in `run_window_loop`: it only
    // fires after a successful `backend.present`).
    let final_count = frames_presented.load(Ordering::SeqCst);
    assert!(
        final_count >= FRAMES_TO_OBSERVE,
        "expected at least {FRAMES_TO_OBSERVE} frames presented through the Wayland backend, got {final_count}"
    );

    let _ = client.disconnect();
}

/// M2 Task 11: measure `ExportRing::publish`'s frame pacing, feeding the
/// design doc's §5 "measure first" decision.
///
/// # Why an `#[ignore]`d test, not a `--log-frame-pacing` CLI flag
///
/// This machine has no display server (`loginctl` reports `Type=tty`, see
/// the design doc §4), so a flag on `ghostframe connect` would need a real
/// compositor to produce a number here regardless -- the only place that
/// already drives a real Wayland present is this crate's `showcase.rs`
/// rig, under headless Weston. A test that runs long enough to gather a
/// distribution is a more natural fit for that rig than a flag nobody
/// could exercise on this machine anyway; the flag remains a reasonable
/// follow-up for someone measuring on a real compositor later.
///
/// # Caveat: Weston headless is not a real compositor
///
/// Frames here are `wl_buffer.attach` + `damage` + `commit` accepted by a
/// software, headless Weston instance -- there is no real display, no vsync
/// signal, no real compositor-side scanout. That means the *inter-frame
/// interval* numbers below are shaped by this test's content source (an X11
/// `--spinner` repaint loop, see below) and Weston's own internal pacing,
/// not by a genuine 60 Hz display. The *`publish` stall* number is not
/// subject to that caveat -- it is wall-clock time spent inside
/// `device.poll(PollType::wait_indefinitely())` on the render thread,
/// measured directly, and that cost is identical regardless of what (or
/// whether) anything is on the other end of the Wayland connection.
///
/// # Scene: `--spinner`
///
/// `--solid-red` (the acceptance test's scene) paints once and then sits
/// idle -- almost no tile traffic after frame 1, which would starve this
/// measurement. `--spinner` repaints a 64x64 region every 100 ms
/// indefinitely (`ghostframe-test-pattern/src/main.rs`), giving a steady
/// ~10 Hz stream of real damage for the whole run.
///
/// That cadence also means this run incidentally covers both blit shapes
/// `ExportRing::publish` can take: the export ring has 3 buffers, each with
/// `filled_at_gen: None` until its first fill, so the first 3 published
/// frames force a full-surface blit (`fb.width x fb.height`, the worst
/// case) before the ring settles into small partial blits of the 64x64
/// spinner region for the rest of the run (the steady-state case). Both are
/// visible in the reported `poll_us` samples.
///
/// # Isolating the `publish` stall
///
/// `ExportRing::publish` (`ghostframe-client-gpu/src/ring.rs`) times its
/// `device.poll` call and reports it via `tracing::debug!` on the
/// `ghostframe_client_gpu::ring` target -- off by default, so it costs
/// nothing on the hot path in production. This test installs a JSON
/// tracing subscriber scoped to exactly that target (as a GLOBAL default,
/// not `set_default`'s thread-local one: `publish` runs on the render
/// thread, a separate OS thread from this test body -- see
/// `ghostframe-client-native/src/render_thread.rs`'s module doc -- and only
/// a global default is visible there) and scrapes `poll_us` back out of the
/// captured JSON lines. Run this test alone (`--ignored`, no other test in
/// the same binary) so nothing else's `tracing_subscriber::fmt().try_init()`
/// wins the race for the global default first.
///
/// Run with:
/// ```text
/// cargo test -p ghostframe-e2e --test showcase -- --ignored \
///     measure_publish_frame_pacing --nocapture
/// ```
#[tokio::test(flavor = "multi_thread")]
#[ignore = "long-running (~45s of steady frames + container/Weston setup); \
            run manually to feed the M2 §5 fence-export decision, see doc comment"]
async fn measure_publish_frame_pacing() {
    // How long to hold the loop open collecting frames, once frames start
    // arriving. Not "30-60s of wall clock from process start" -- container
    // + tsnet + Weston setup alone costs real time (~70s on this machine)
    // and none of that belongs in the pacing sample.
    const MEASURE_DURATION: Duration = Duration::from_secs(45);
    // Backstop only; --spinner's ~10 Hz cadence over 45s means this should
    // never bind.
    const MAX_FRAMES: u64 = 100_000;

    #[derive(Default, Clone)]
    struct CapturingWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for CapturingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for CapturingWriter {
        type Writer = CapturingWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let poll_log = Arc::new(Mutex::new(Vec::<u8>::new()));
    let writer = CapturingWriter(Arc::clone(&poll_log));
    // GLOBAL default (`try_init`, not `set_default`): `ExportRing::publish`
    // runs on the render thread, not this one. If this fails (something
    // else already installed a global subscriber in this process), the
    // `poll_us` samples below will come back empty -- reported explicitly
    // rather than silently producing a misleading zero-stall number.
    let subscriber_installed = tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_target(false)
        .with_env_filter("ghostframe_client_gpu::ring=debug")
        .try_init()
        .is_ok();
    if !subscriber_installed {
        eprintln!(
            "[phase] WARNING: could not install the global tracing subscriber (one is \
             already set in this process) -- run this test alone, not alongside others \
             in the same binary, or the publish-stall numbers below will be empty"
        );
    }

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--spinner",
        // Pin tile mode. Sessions start in H.264 (`io_bridge.rs:3435`), so
        // once the CLI advertises the capability this test would silently
        // become an H.264 test instead of the tile-path test it was written
        // as. That is covered by `tests/h264.rs`; this one stays what it is.
        extra_env: &[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "tile")],
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    eprintln!("[phase] spawning headless weston");
    let weston = spawn_weston_headless().expect("spawn weston (is the weston package installed?)");
    std::env::set_var("WAYLAND_DISPLAY", &weston.wayland_display);
    std::env::set_var("XDG_RUNTIME_DIR", weston.runtime_dir());
    std::env::remove_var("DISPLAY");

    eprintln!("[phase] opening the window backend");
    let mut backend =
        window::open("ghostframe-pacing-test").expect("open a window backend against weston");
    let preferred_modifiers = backend.preferred_dmabuf_modifiers();

    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "pacing-test".into(),
        authkey: String::new(),
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers,
        debug_map_frames: false,
        max_decode_width: 0,
        max_decode_height: 0,
    })
    .expect("create client");
    client.attach_bridge(setup._test_node.bridge());

    eprintln!(
        "[phase] connecting to {}:443 over tsnet",
        setup.server_container_name
    );
    if let Err(e) = client.connect(&setup.server_container_name, 443) {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("connect over the tailnet failed: {e}");
    }

    let mut chord = Chord::new(Prefix::CtrlAltB);
    // Micros-since-first-frame timestamps; `RefCell` (not `AtomicU64` like
    // the acceptance test) because this needs a growable log, not a
    // counter, and both closures below only ever run on this one thread.
    let frame_log: RefCell<Vec<u64>> = RefCell::new(Vec::new());
    #[allow(
        clippy::disallowed_methods,
        reason = "real wall-clock measurement window for a real-Docker/real-compositor test"
    )]
    let start = Instant::now();
    let mut measure_deadline: Option<Instant> = None;
    let mut timed_out_waiting_for_first_frame = false;
    #[allow(
        clippy::disallowed_methods,
        reason = "real wall-clock deadline for a real-Docker/real-compositor test"
    )]
    let first_frame_deadline = Instant::now() + Duration::from_secs(60);

    eprintln!(
        "[phase] running the window loop for {MEASURE_DURATION:?} of steady frames \
         (after the first frame arrives)"
    );
    let result = run_window_loop(
        &mut client,
        backend.as_mut(),
        &mut chord,
        &mut |n| {
            #[allow(
                clippy::disallowed_methods,
                reason = "real wall-clock measurement, see `start` above"
            )]
            let now = Instant::now();
            frame_log
                .borrow_mut()
                .push(now.duration_since(start).as_micros() as u64);
            if n % 50 == 0 {
                eprintln!("[phase] presented frame {n}");
            }
        },
        &mut || {
            #[allow(
                clippy::disallowed_methods,
                reason = "real wall-clock deadline check, see `start` above"
            )]
            let now = Instant::now();
            let count = frame_log.borrow().len() as u64;
            if count == 0 {
                if now >= first_frame_deadline {
                    timed_out_waiting_for_first_frame = true;
                    return true;
                }
                return false;
            }
            let deadline = *measure_deadline.get_or_insert(now + MEASURE_DURATION);
            count >= MAX_FRAMES || now >= deadline
        },
    );

    eprintln!(
        "[phase] window loop returned: {:?}",
        result.as_ref().map(|_| "ok")
    );
    if result.is_err() || timed_out_waiting_for_first_frame {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
    }
    result.expect("window loop ran to completion without error");
    assert!(
        !timed_out_waiting_for_first_frame,
        "no frame presented within 60s of connecting"
    );

    let _ = client.disconnect();

    // --- report: frame count / wall duration / inter-frame p50 & p99 ---
    let timestamps = frame_log.into_inner();
    let frame_count = timestamps.len();
    let wall_duration_us = timestamps.last().copied().unwrap_or(0);
    let mut intervals: Vec<u64> = timestamps.windows(2).map(|w| w[1] - w[0]).collect();
    intervals.sort_unstable();
    let percentile = |sorted: &[u64], p: f64| -> Option<u64> {
        if sorted.is_empty() {
            return None;
        }
        let idx = ((sorted.len() - 1) as f64 * p).round() as usize;
        Some(sorted[idx.min(sorted.len() - 1)])
    };
    let p50_us = percentile(&intervals, 0.50);
    let p99_us = percentile(&intervals, 0.99);

    // --- report: publish's device.poll stall, scraped from the tracing capture ---
    let captured = String::from_utf8(poll_log.lock().unwrap().clone()).unwrap_or_default();
    let mut poll_samples_us: Vec<u64> = captured
        .lines()
        .filter_map(|line| {
            let key = "\"poll_us\":";
            let start = line.find(key)? + key.len();
            let rest = &line[start..];
            let end = rest
                .find(|c: char| !c.is_ascii_digit())
                .unwrap_or(rest.len());
            rest[..end].parse::<u64>().ok()
        })
        .collect();
    poll_samples_us.sort_unstable();
    let poll_p50_us = percentile(&poll_samples_us, 0.50);
    let poll_p99_us = percentile(&poll_samples_us, 0.99);
    let poll_max_us = poll_samples_us.last().copied();
    // The first `n_export_buffers` (3) publishes force a full-surface blit
    // (see the doc comment above); everything after that is the
    // steady-state partial-blit case. Report both.
    let (poll_first3, poll_rest): (Vec<u64>, Vec<u64>) = {
        let mut in_order: Vec<u64> = Vec::new();
        for line in captured.lines() {
            let key = "\"poll_us\":";
            if let Some(start) = line.find(key) {
                let rest = &line[start + key.len()..];
                let end = rest
                    .find(|c: char| !c.is_ascii_digit())
                    .unwrap_or(rest.len());
                if let Ok(v) = rest[..end].parse::<u64>() {
                    in_order.push(v);
                }
            }
        }
        let split = in_order.len().min(3);
        (in_order[..split].to_vec(), in_order[split..].to_vec())
    };

    eprintln!("=== M2 §5 frame-pacing measurement ===");
    eprintln!("frames presented:      {frame_count}");
    eprintln!(
        "wall duration:         {:.2} s",
        wall_duration_us as f64 / 1_000_000.0
    );
    eprintln!(
        "inter-frame interval:  p50={:?} p99={:?}",
        p50_us.map(Duration::from_micros),
        p99_us.map(Duration::from_micros)
    );
    eprintln!(
        "publish() poll stall:  n={} p50={:?} p99={:?} max={:?}",
        poll_samples_us.len(),
        poll_p50_us.map(Duration::from_micros),
        poll_p99_us.map(Duration::from_micros),
        poll_max_us.map(Duration::from_micros)
    );
    eprintln!("publish() poll stall, first 3 (full-surface blit): {poll_first3:?} us");
    eprintln!(
        "publish() poll stall, remaining {} (partial blit): p50={:?} p99={:?}",
        poll_rest.len(),
        {
            let mut r = poll_rest.clone();
            r.sort_unstable();
            percentile(&r, 0.50).map(Duration::from_micros)
        },
        {
            let mut r = poll_rest.clone();
            r.sort_unstable();
            percentile(&r, 0.99).map(Duration::from_micros)
        }
    );
    eprintln!("=== end measurement ===");

    assert!(
        frame_count > 0,
        "expected at least one frame presented before reporting pacing"
    );
}
