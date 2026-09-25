//! M4b acceptance: a client negotiates its display resolution end to end.
//!
//! Joins the tailnet as its own node, connects over tsnet advertising a
//! monitor maximum and a scale (`ClientDisplay`), waits for the session to
//! actually be served (a published frame), then asks the server to switch
//! to a specific mode (`Client::request_display_mode`) and asserts the
//! server *actually changed resolution* -- observable through the
//! `ClientEvent::Resized` event the client already receives from the
//! existing frame-dimensions message.
//!
//! # This is the only end-to-end coverage of the whole milestone
//!
//! Everything upstream of this test is unit-tested in isolation:
//! `DisplayInfo`/`DisplayMode` wire encode/decode
//! (`ghostframe-lib/src/transport/display.rs`), the debounce + clamp +
//! width-alignment dispatch logic (`ghostframe-lib/src/transport/
//! io_bridge.rs`), and `XrandrDisplay::set_output`'s RandR sequencing
//! (`ghostframe-xdaemon/src/display.rs`). None of those tests drive a real
//! Xorg server, so none of them can tell us whether the RandR calls this
//! milestone assembled actually change what a client sees. This test is
//! the one place that finds out. If it goes red, that is a finding about
//! the milestone, not a test to be adjusted until it's green.
//!
//! Requires Docker AND a GPU (the host's VKMS setup). Deliberately NOT
//! named in any CI workflow -- see the exemption comment in
//! `.github/workflows/e2e.yml`.
//!
//! # `TS_CONTROL_URL` must be in the PROCESS environment, not set from Rust
//!
//! Run this as:
//!
//! ```text
//! TS_CONTROL_URL=http://127.0.0.1:18080 \
//!   cargo test -p ghostframe-e2e --test display_negotiation -- --nocapture --test-threads=1
//! ```
//!
//! ghostbridge's Go `init()` reads `TS_CONTROL_URL` at package-init time,
//! before `main` -- see `native_client.rs`'s module docs for the full
//! diagnosis of what a too-late `std::env::set_var` looks like (a wall of
//! `derp.Recv`/TLS handshake errors that reads like a network fault).
//!
//! # Both clients share the harness's tsnet node
//!
//! Only one client connects here, but it attaches to
//! `setup._test_node.bridge()` like every other native-client e2e test --
//! `native_client.rs` documents at length why a second `tsnet.Server` in
//! this process would not converge a working peer datapath.
//!
//! # Why this file has its own event waiter
//!
//! `client_wait::wait_for_frame` drains `next_event` and keeps only
//! published frames -- it discards `ClientEvent::Resized` along the way.
//! That is correct for every other caller of it and wrong for this test,
//! so (like `eviction.rs`'s `wait_for_disconnect`) this file defines its
//! own waiter rather than changing the shared one for a single caller.

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientDisplay, ClientEvent, Config};
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

#[path = "common/client_wait.rs"]
mod client_wait;
use client_wait::wait_for_frame;

/// Pump events until a `ClientEvent::Resized` arrives, returning its
/// `(width, height)`. Does not panic on `ClientEvent::Error` -- unlike
/// `wait_for_frame`'s reasoning, a resize that fails server-side (RandR
/// rejects the mode) is exactly the finding this test exists to surface,
/// not a bug in the harness.
/// Pump events until a `Resized` reports `want`, or the timeout elapses.
/// Returns every size observed, so a failure can say what actually arrived.
///
/// **Do not return the first `Resized` you see.** The client emits one
/// whenever it learns frame dimensions, including the initial ones at
/// connect -- so a first-match waiter returns the size the session started
/// at and fails within milliseconds, long before the server's 250 ms
/// debounce has elapsed. That is exactly the bug this helper replaced: the
/// test reported "Resized to 640x480" and looked like a server failure,
/// while the server was still correctly waiting out its debounce.
fn wait_for_resize_to(
    client: &mut Client,
    want: (u32, u32),
    timeout: Duration,
) -> (bool, Vec<(u32, u32)>) {
    let deadline = Instant::now() + timeout;
    let mut seen: Vec<(u32, u32)> = Vec::new();
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Resized { width, height } = ev {
                if seen.last() != Some(&(width, height)) {
                    seen.push((width, height));
                }
                if (width, height) == want {
                    return (true, seen);
                }
            }
        }
        if Instant::now() >= deadline {
            return (false, seen);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn make_client(hostname: &str, state_dir: &std::path::Path) -> Client {
    Client::new(Config {
        hostname: hostname.into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        debug_map_frames: false,
        display: Some(ClientDisplay {
            max_width: 2560,
            max_height: 1440,
            scale_milli: 1000,
            mm_width: 597,
            mm_height: 336,
        }),
    })
    .expect("create client")
}

#[tokio::test(flavor = "multi_thread")]
async fn client_requests_a_mode_and_the_server_actually_applies_it() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    eprintln!("[phase] containers up; building client");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = make_client("display-negotiation-test", state_dir.path());
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
        panic!("client connect over the tailnet failed: {e}");
    }

    // Wait for a frame before requesting a mode change -- a published
    // frame proves HELLO (and DisplayInfo) landed and the session is
    // actively being served. Without this the test can race and request a
    // mode change on a session the server hasn't finished setting up,
    // passing or failing for the wrong reason.
    eprintln!("[phase] waiting for the first published frame");
    let first_frame = match wait_for_frame(&mut client, Duration::from_secs(60)) {
        Some(frame) => frame,
        None => {
            eprintln!(
                "--- server logs ({}) ---\n{}",
                setup.server_container_name,
                read_server_logs_stripped(&setup.server_container_name)
            );
            panic!("no frame published within 60s");
        }
    };
    eprintln!(
        "[phase] first frame: {}x{}",
        first_frame.width, first_frame.height
    );

    const TARGET_W: u32 = 1280;
    const TARGET_H: u32 = 800;
    assert!(
        first_frame.width != TARGET_W || first_frame.height != TARGET_H,
        "the container's startup resolution is already {}x{} -- pick a different \
         TARGET_W/TARGET_H for this test, or a resize to the size it already was \
         would pass without proving anything",
        TARGET_W,
        TARGET_H
    );

    eprintln!("[phase] requesting {TARGET_W}x{TARGET_H}");
    client.request_display_mode(TARGET_W as u16, TARGET_H as u16);

    // Well past the server's 250ms debounce.
    eprintln!("[phase] waiting for a Resized event reporting the new mode");
    let (matched, seen) =
        wait_for_resize_to(&mut client, (TARGET_W, TARGET_H), Duration::from_secs(30));
    eprintln!("[result] matched={matched} sizes observed: {seen:?}");

    if !matched {
        eprintln!(
            "--- server logs ({}) ---
{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
    }
    assert!(
        matched,
        "server reported a resize but not to the requested mode -- clamping/alignment \
         may have altered it unexpectedly, or a stale FrameDimensions from before the \
         request raced this assertion"
    );

    eprintln!("[phase] done");
    let _ = client.disconnect();
}

/// Settles an open question recorded in the M4b design doc (§2.1): is the
/// Xorg `Virtual` line (or, here, the largest listed `Modes` entry, since
/// `tests/containers/test-server/xorg-vkms.conf` sets no explicit `Virtual`
/// and Xorg falls back to that) a hard framebuffer ceiling, or merely the
/// *initial* size that RandR can grow past?
///
/// `XrandrDisplay::ceiling()` (`ghostframe-xdaemon/src/display.rs`) queries
/// `GetScreenSizeRange`, which on the dev box reports `16384x16384`
/// regardless of `Virtual` -- so the driver's advertised *capability* is
/// not the limit. What's untested is whether a `SetScreenSize` call that
/// actually asks RandR to grow past the configured startup size succeeds.
/// This test requests 2560x1440, comfortably past the container's
/// 1920x1080 startup size (`xorg-vkms.conf`'s largest `Modes` entry), and
/// asserts the resize actually lands at that size.
///
/// If this test **passes**: `Virtual`/`Modes` is not a ceiling, and the
/// packaging conservatism the design doc flagged (bumping `Virtual` up
/// front) is prudence rather than necessity.
///
/// If this test **fails**: the startup size IS a ceiling, the design doc's
/// original premise was right, and raising the container's configured
/// startup size is required, not optional. Either way, do not weaken this
/// assertion to make it pass -- the failure itself is the finding.
#[tokio::test(flavor = "multi_thread")]
async fn client_requests_a_mode_larger_than_the_containers_startup_size() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    eprintln!("[phase] containers up; building client");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = make_client("display-negotiation-grow-test", state_dir.path());
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
        panic!("client connect over the tailnet failed: {e}");
    }

    eprintln!("[phase] waiting for the first published frame");
    let first_frame = match wait_for_frame(&mut client, Duration::from_secs(60)) {
        Some(frame) => frame,
        None => {
            eprintln!(
                "--- server logs ({}) ---\n{}",
                setup.server_container_name,
                read_server_logs_stripped(&setup.server_container_name)
            );
            panic!("no frame published within 60s");
        }
    };
    eprintln!(
        "[phase] first (startup) frame: {}x{} -- the container's configured startup size \
         (xorg-vkms.conf's largest Modes entry, since it sets no explicit Virtual line)",
        first_frame.width, first_frame.height
    );

    // Larger than the container's 1920x1080 startup size in BOTH
    // dimensions, so a partial/clamped result is unambiguous evidence of a
    // ceiling rather than of hitting one dimension's limit only.
    const TARGET_W: u32 = 2560;
    const TARGET_H: u32 = 1440;
    assert!(
        TARGET_W > first_frame.width && TARGET_H > first_frame.height,
        "expected the target ({TARGET_W}x{TARGET_H}) to exceed the observed startup size \
         ({}x{}) in both dimensions -- the container's Xorg config changed; pick a new \
         target above its largest configured Modes entry",
        first_frame.width,
        first_frame.height
    );

    eprintln!("[phase] requesting {TARGET_W}x{TARGET_H} (past the startup size)");
    client.request_display_mode(TARGET_W as u16, TARGET_H as u16);

    eprintln!("[phase] waiting for a Resized event");
    let (matched, seen) =
        wait_for_resize_to(&mut client, (TARGET_W, TARGET_H), Duration::from_secs(30));
    eprintln!("[result] matched={matched} sizes observed: {seen:?}");

    if !matched {
        eprintln!(
            "--- server logs ({}) ---
{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
    }
    assert!(
        matched,
        "FINDING: the server did not grow to the requested size past its startup \
         resolution -- Virtual/Modes acts as a ceiling here (or something else clamped \
         the request); see the server log dumped above for which"
    );

    eprintln!(
        "[result] FINDING: RandR grew the framebuffer past the container's startup \
               size ({}x{} -> {}x{}) -- Virtual/Modes is not a hard ceiling",
        first_frame.width, first_frame.height, TARGET_W, TARGET_H
    );

    eprintln!("[phase] done");
    let _ = client.disconnect();
}
