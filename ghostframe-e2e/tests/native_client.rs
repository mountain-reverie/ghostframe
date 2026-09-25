//! M1 acceptance: the native client against a live ghostframe server.
//!
//! Joins the tailnet as its own node, connects over tsnet, decodes on the
//! GPU, and asserts exact pixels in the exported dmabuf. There is no
//! direct-socket path here and none may be added -- staying inside the
//! tailnet is a security property of this system, not a test convenience.
//!
//! Requires Docker AND a GPU. Deliberately NOT named in any CI workflow
//!
//! # `TS_CONTROL_URL` must be in the PROCESS environment, not set from Rust
//!
//! Run this as:
//!
//! ```text
//! TS_CONTROL_URL=http://127.0.0.1:18080 \
//!   cargo test -p ghostframe-e2e --test native_client -- --nocapture --test-threads=1
//! ```
//!
//! ghostbridge's Go `init()` picks the DERP transport at **package-init time,
//! before `main`**, from `os.Getenv("TS_CONTROL_URL")`: non-empty means
//! headscale, so opt into `TS_DEBUG_USE_DERP_HTTP=1`; empty means the public
//! tailnet, which requires HTTPS DERP (`ghostbridge/main.go`).
//!
//! A `std::env::set_var("TS_CONTROL_URL", ..)` in the test body is therefore
//! far too late -- the decision was made when the binary loaded. The node
//! still *logs in*, because the control URL is passed separately at runtime
//! through `GhostbridgeConfig`, so headscale lists it online and everything
//! looks healthy. But DERP then speaks TLS to headscale's plain-HTTP relay
//! and every dial hangs, producing a wall of
//!
//! ```text
//! netcheck: UDP is blocked, trying HTTPS
//! derp.Recv(derp-999): tls: first record does not look like a TLS handshake
//! ```
//!
//! which reads like a network fault rather than a missing env var. The
//! giveaway is ghostbridge's very first log line:
//! `init: production path (public Tailscale)`.
//!
//! The harness sets `TS_CONTROL_URL` on the server *container*
//! (`e2e_setup.rs`), which is why containerised nodes never hit this and only
//! a host-side tsnet node does.
//! (see `ghostframe-client-gpu/README.md`'s test-target table).

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, Config};
use ghostframe_e2e::harness::net_shape::NetShape;
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

#[path = "common/client_wait.rs"]
mod client_wait;
use client_wait::wait_for_frame;

#[tokio::test(flavor = "multi_thread")]
async fn native_client_renders_the_test_pattern_into_an_exported_dmabuf() {
    // Installed before setup, deliberately, so it covers container bring-up.
    //
    // An earlier round of debugging concluded that a subscriber here stalled
    // `setup_e2e_server` (it failed 5/5 with it, passed once moved after).
    // That was wrong. Bisected properly afterwards: the setup-only test
    // passes with a stdout subscriber (41s) and without one (34s), and this
    // test passes with the subscriber restored to this position. The stall
    // was always the bootstrap bug -- a plaintext GET to the TLS port, then
    // an EOF-framed read -- and the phase attribution was an artefact of
    // cargo buffering the `[phase] containers up` marker away when `timeout`
    // killed the run, which made a completed setup look stalled.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        // The GPU under test is the CLIENT's. Leaving the server on its CPU
        // capture path keeps this test off the host's VKMS setup. (Tried
        // `true` while bisecting the stall; it made no difference, and the
        // cause was elsewhere.)
        gpu: false,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    // Reuse the tsnet node `setup_e2e_server` already joined, rather than
    // standing up a second one.
    //
    // A second `tsnet.Server` in the same OS process was observed not to
    // converge a working peer datapath: both nodes log in to headscale and
    // reach `Running`, headscale lists all three as online, but wgengine
    // reconfigures with an incomplete peer set and no packet ever crosses --
    // every dial then hangs. A single-node control run of the same harness
    // (`harness_smoke_solid_1s`) passes in ~65s under identical DERP noise,
    // which is what isolates it to the second node rather than to the
    // environment.
    //
    // Sharing is also what a real embedder wants: a host application already
    // on the tailnet should not be forced to create another node, which is
    // why `Client::attach_bridge` exists rather than this being a test-only
    // shim.
    eprintln!("[phase] containers up; building client");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "native-client-test".into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        // Diagnostic readback: this test asserts on dmabuf contents.
        debug_map_frames: true,
        display: None,
    })
    .expect("create client");
    client.attach_bridge(setup._test_node.bridge());

    eprintln!(
        "[phase] connecting to {}:443 over tsnet",
        setup.server_container_name
    );
    let connect_result = client.connect(&setup.server_container_name, 443);
    eprintln!(
        "[phase] connect returned: {:?}",
        connect_result.as_ref().map(|_| "ok")
    );
    if let Err(e) = &connect_result {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("connect over the tailnet failed: {e}");
    }

    eprintln!("[phase] waiting for the first published frame");
    let frame = match wait_for_frame(&mut client, Duration::from_secs(60)) {
        Some(f) => f,
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
        "frame_id={} buffer_id={} {}x{} damage={:?}",
        frame.frame_id, frame.buffer_id, frame.width, frame.height, frame.damage
    );

    // The export buffer has never been filled before this publish, so
    // `ExportRing::publish` must take the full-surface blit path: the
    // damage on the first frame should cover the whole surface.
    let full_surface_covered = frame
        .damage
        .iter()
        .any(|r| r.x == 0 && r.y == 0 && r.w == frame.width && r.h == frame.height);
    assert!(
        full_surface_covered,
        "expected the first frame's damage to cover the full {}x{} surface \
         (never-filled export buffer forces a full blit), got {:?}",
        frame.width, frame.height, frame.damage
    );

    let f = client.debug_map_frame(&frame).expect("map exported dmabuf");

    // The scene is solid red. Sample well away from the origin: an
    // origin-only check passes even when the stride is wrong.
    let (x, y) = (100u64, 100u64);
    assert!(
        u64::from(frame.width) > x && u64::from(frame.height) > y,
        "sample point ({x},{y}) is outside the {}x{} surface",
        frame.width,
        frame.height
    );
    let o = (f.offset + y * f.stride + x * 4) as usize;
    let px = &f.bytes[o..o + 4];
    eprintln!(
        "pixel at ({x},{y}): {px:?} (stride={}, offset={})",
        f.stride, f.offset
    );
    assert_eq!(
        (px[0], px[1], px[2]),
        (0xFF, 0x00, 0x00),
        "expected solid red (R=0xFF,G=0x00,B=0x00) at ({x},{y}), got {px:?}"
    );

    client.release_frame(frame.frame_id);
    client.disconnect().expect("disconnect");
}

/// Task 20: the native client at production scale (1920x1080 = 2040 tiles)
/// on a shaped, lossy link -- the scenario the whole native client was
/// commissioned for.
///
/// # Reproducing the production symptom
///
/// The handoff describes a production session that never finished refining
/// a *static* screen: `tiles=2040 complete=0 partial=2040 gave_up=1587`,
/// stranded tiles missing exactly the finest bit-planes (`{11,12,13}`). The
/// scale matters: the same shape at VKMS's default 1024x768 (768 tiles)
/// converges cleanly, which is why every earlier e2e scene missed it.
///
/// # Deviation from the M1 test-pattern convention: `--gradient`, not `--solid-red`
///
/// `e2e_saturated_link_starves_tiles_into_giving_up` (`e2e.rs`) reproduces
/// this with `--tile-pattern photo --subtle-drift 250`, i.e. *moving*
/// content. This test wants a screen that has stopped changing, to isolate
/// "detail never arrives" from "new content keeps superseding old passes".
/// But `--solid-red` is the wrong static content for that: every tile is a
/// single color, so `classify_tile`'s Rule 6 (`unique_colors <= 1`) always
/// picks `CodecState::Solid` -- CDF 5/3 is never entered at all, and
/// `cdf53_coverage_summary` would report `tiles=0` regardless of what the
/// link does to it. `--gradient --drm-direct` (used by
/// `e2e_cdf53_lossless_buildup_*` for exactly this reason) paints a static,
/// full-screen diagonal gradient: every 32x32 tile has hundreds of unique
/// colors, so the classifier's Rule 8 fallback forces Cdf53, and the scene
/// never changes after the first paint (see
/// `ghostframe-test-pattern/src/gradient.rs`: "paint once, sleep forever").
/// That is the static-but-detailed screen the production bug actually needs.
///
/// # Outcomes
///
/// Both are useful and neither is a failure of this test itself:
/// - Convergence (`complete == tiles`, `gave_up == 0`): the native client
///   handles production scale under loss.
/// - Stranding (`gave_up > 0` or `complete < tiles`): the first in-process,
///   greppable reproduction of the production symptom (previously only
///   observed via a user pasting browser console text).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Docker + GPU + VKMS; production-scale, several minutes"]
async fn native_client_converges_at_production_scale_under_loss() {
    eprintln!("[phase] starting headscale + ghostframe-server containers (production scale)");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--gradient --drm-direct",
        extra_env: &[
            ("GHOSTFRAME_DRM_MODE", "1920x1080"),
            ("GHOSTFRAME_ENABLE_CDF53", "1"),
            // Pin the frame mode so a cost-based H264 switch can't freeze
            // Cdf53 emission mid-refinement and masquerade as stranding --
            // see `e2e_saturated_link_starves_tiles_into_giving_up`'s doc
            // comment for how that fooled an earlier run of that test.
            ("GHOSTFRAME_FORCE_TILECODEC", "1"),
            // Deterministic outbound tile loss, independent of (and in
            // addition to) the kernel-level `tc netem` shaping applied
            // below. Seeded so a stranding reproduction is reproducible.
            ("GHOSTFRAME_OUTBOUND_LOSS_PROBABILITY", "0.01"),
            ("GHOSTFRAME_OUTBOUND_LOSS_PREDICATE", "tile"),
            ("GHOSTFRAME_OUTBOUND_LOSS_SEED", "20260922"),
        ],
        gpu: true,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!(
        "[phase] shaping {} with a tailnet-like lossy link",
        setup.server_container_name
    );
    NetShape::tailnet_like_lossy()
        .apply(&setup.server_container_name)
        .expect("apply tailnet-like lossy shape");
    let qdisc =
        NetShape::verify(&setup.server_container_name).expect("read back the applied qdisc");
    eprintln!("shaped link qdisc: {}", qdisc.trim());
    assert!(
        qdisc.contains("netem") && qdisc.contains("loss"),
        "lossy netem was not actually applied to {}; qdisc reads: {qdisc:?}. \
         A silently-unshaped run would masquerade as the easy case.",
        setup.server_container_name
    );

    eprintln!("[phase] containers up; building client");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "native-client-scale-test".into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        // Diagnostic readback: this test asserts on dmabuf contents.
        debug_map_frames: true,
        display: None,
    })
    .expect("create client");
    client.attach_bridge(setup._test_node.bridge());

    eprintln!(
        "[phase] connecting to {}:443 over tsnet",
        setup.server_container_name
    );
    let connect_result = client.connect(&setup.server_container_name, 443);
    if let Err(e) = &connect_result {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("connect over the tailnet failed: {e}");
    }

    eprintln!("[phase] waiting for the first published frame");
    let frame = match wait_for_frame(&mut client, Duration::from_secs(60)) {
        Some(f) => f,
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
        "frame_id={} buffer_id={} {}x{} damage={:?}",
        frame.frame_id, frame.buffer_id, frame.width, frame.height, frame.damage
    );
    client.release_frame(frame.frame_id);

    eprintln!("[phase] polling cdf53 coverage for up to 90s");
    let poll_deadline = Instant::now() + Duration::from_secs(90);
    loop {
        // Drain events between polls so a real `ClientEvent::Error` (e.g. a
        // connection loss under this much loss/shaping) fails loudly rather
        // than being silently absorbed by the polling loop.
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Error { message } = &ev {
                eprintln!(
                    "--- server logs ({}) ---\n{}",
                    setup.server_container_name,
                    read_server_logs_stripped(&setup.server_container_name)
                );
                panic!("client reported an error while polling coverage: {message}");
            }
        }

        let cov = client.cdf53_coverage().expect("query cdf53 coverage");
        eprintln!("[coverage] {}", cov.summary.to_log_line());

        if cov.summary.tiles > 0 && cov.summary.complete == cov.summary.tiles {
            eprintln!("[phase] converged");
            break;
        }
        if Instant::now() >= poll_deadline {
            eprintln!("[phase] 90s poll deadline reached without full convergence");
            break;
        }
        std::thread::sleep(Duration::from_secs(1));
    }

    let final_cov = client.cdf53_coverage().expect("final cdf53 coverage query");
    eprintln!("[final] {}", final_cov.summary.to_log_line());

    if final_cov.summary.gave_up > 0 || final_cov.summary.complete < final_cov.summary.tiles {
        eprintln!(
            "--- {} most-stalled incomplete tiles ---",
            final_cov.incomplete.len()
        );
        for (x, y, received, present, sweeps) in &final_cov.incomplete {
            let missing = present & !received;
            eprintln!(
                "tile ({x},{y}): received={received:#06x} present={present:#06x} \
                 missing={missing:#06x} sweep_attempts={sweeps} \
                 missing_finest_planes={:?}",
                (11u16..=13)
                    .filter(|b| missing & (1 << b) != 0)
                    .collect::<Vec<_>>()
            );
        }
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
    }

    // Load-bearing: proves this ran at production scale, not silently at
    // VKMS's 768-tile default. Without this a misconfigured resolution
    // turns the whole test back into the easy case that already converges.
    assert!(
        final_cov.summary.tiles >= 2000,
        "expected production scale (1920x1080 = 2040 tiles), got only {} tiles \
         with any coverage state -- resolution is probably misconfigured. \
         full summary: {}",
        final_cov.summary.tiles,
        final_cov.summary.to_log_line()
    );
    assert_eq!(
        final_cov.summary.gave_up,
        0,
        "{} of {} tiles exhausted the tail-sweep budget and stopped asking for \
         passes they never received -- each is stranded, rendering wrong, for \
         the rest of the session. full summary: {}",
        final_cov.summary.gave_up,
        final_cov.summary.tiles,
        final_cov.summary.to_log_line()
    );
    assert_eq!(
        final_cov.summary.complete,
        final_cov.summary.tiles,
        "{} of {} tiles are still incomplete after 90s on a screen that has \
         not changed since the first paint. full summary: {}",
        final_cov.summary.tiles - final_cov.summary.complete,
        final_cov.summary.tiles,
        final_cov.summary.to_log_line()
    );

    NetShape::clear(&setup.server_container_name).ok();
    client.disconnect().expect("disconnect");
}

/// Isolation: does `setup_e2e_server` alone bring a host-side tsnet node up?
///
/// The acceptance test above stalls inside `setup_e2e_server`, at the
/// harness's own `TestNode::join` -> `server.Up()`, before any client code
/// runs. `harness_smoke` (which goes through `run_scene`) makes the same
/// call with the same control URL and passes reliably. This test contains
/// nothing but the setup, so a failure here is a harness problem and a pass
/// here moves the fault back into the acceptance test.
/// Install a subscriber per `GF_SUBSCRIBER`, for bisecting the stall.
///
/// `none` (default) installs nothing. `stdout` reproduces what the
/// acceptance test originally did. `stderr` and `sink` vary only the
/// writer, which is what discriminates "a subscriber exists" from "the
/// subscriber writes to stdout".
fn install_subscriber_variant() -> &'static str {
    let which = std::env::var("GF_SUBSCRIBER").unwrap_or_else(|_| "none".into());
    let filter = || tracing_subscriber::EnvFilter::from_default_env();
    match which.as_str() {
        "stdout" => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::io::stdout)
                .try_init();
            "stdout"
        }
        "stderr" => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::io::stderr)
                .try_init();
            "stderr"
        }
        "sink" => {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(filter())
                .with_writer(std::io::sink)
                .try_init();
            "sink"
        }
        _ => "none",
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "diagnostic; requires Docker"]
async fn harness_setup_alone_brings_up_a_tsnet_node() {
    let variant = install_subscriber_variant();
    eprintln!("[iso] subscriber variant: {variant}");
    eprintln!("[iso] calling setup_e2e_server");
    let started = Instant::now();
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
        gpu: true,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("setup_e2e_server");
    // This test drives real Docker/tsnet setup on the real wall clock (not
    // exposed to `tokio::time::pause()`), so an `elapsed()` here is
    // measuring genuine setup latency, not silently saturating against a
    // virtual clock -- exactly the case `disallowed_methods` carves out.
    #[allow(
        clippy::disallowed_methods,
        reason = "diagnostic wall-clock timing in a real-Docker (non-virtual-clock) test"
    )]
    let elapsed = started.elapsed();
    eprintln!(
        "[iso] setup returned after {:?}; server={}",
        elapsed, setup.server_container_name
    );
}
