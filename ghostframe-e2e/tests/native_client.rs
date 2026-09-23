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

use ghostframe_client_native::{Client, ClientEvent, Config, PublishedFrame};
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

/// Drain queued events (logging each) and return the first published frame,
/// or `None` if `timeout` elapses first. Panics eagerly on a
/// `ClientEvent::Error` rather than waiting out the full timeout, since that
/// event means the library has already given up.
fn wait_for_frame(client: &mut Client, timeout: Duration) -> Option<PublishedFrame> {
    let deadline = Instant::now() + timeout;
    loop {
        while let Some(ev) = client.next_event() {
            tracing::info!(?ev, "client event");
            if let ClientEvent::Error { message } = &ev {
                panic!("client reported an error while waiting for a frame: {message}");
            }
        }
        if let Some(frame) = client.acquire_frame() {
            return Some(frame);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn native_client_renders_the_test_pattern_into_an_exported_dmabuf() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--solid-red",
        extra_env: &[],
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
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "native-client-test".into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: false,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
    })
    .expect("create client");
    client.attach_bridge(setup._test_node.bridge());

    let connect_result = client.connect(&setup.server_container_name, 443);
    if let Err(e) = &connect_result {
        eprintln!(
            "--- server logs ({}) ---\n{}",
            setup.server_container_name,
            read_server_logs_stripped(&setup.server_container_name)
        );
        panic!("connect over the tailnet failed: {e}");
    }

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
