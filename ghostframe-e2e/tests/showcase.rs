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

use std::sync::atomic::{AtomicU64, Ordering};
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
        extra_env: &[],
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
