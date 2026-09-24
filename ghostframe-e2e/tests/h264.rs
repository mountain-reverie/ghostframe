//! M3 acceptance: H.264 frames from a live server, decoded on the GPU.
//!
//! Requires Docker, a GPU, VA-API, and the host VKMS setup (the server's
//! full-frame H.264 encoder is fed by the GPU capture pipeline, so the CPU
//! capture path `native_client.rs` uses produces no H.264 at all). This is
//! the same `gpu: true` + `--drm-direct` shape `e2e_mode_switch_chromium`
//! uses, not the M1 test's shape. Deliberately NOT named in any CI workflow
//! -- see `.github/workflows/e2e.yml` for why `native_client.rs` and
//! `showcase.rs` are unlisted; the same reasoning applies here.
//!
//! Run as:
//! ```text
//! TS_CONTROL_URL=http://127.0.0.1:18080 \
//!   cargo test -p ghostframe-e2e --test h264 -- --nocapture --test-threads=1
//! ```
//!
//! # `TS_CONTROL_URL` must be in the PROCESS environment, not set from Rust
//!
//! ghostbridge's Go `init()` reads it before `main` to choose the DERP
//! transport. A `std::env::set_var` in the test body is far too late: the
//! node still logs in and headscale lists it online, so everything looks
//! healthy right up until DERP speaks TLS to a plain-HTTP relay and every
//! dial hangs. See `native_client.rs`'s module doc for the full story.
//!
//! # What this test does and does not prove
//!
//! A pass here shows that H.264 encoded by a live server, carried over a
//! real tsnet tailnet, decodes on VA-API hardware and renders into an
//! exported dmabuf with plausible (non-flat) pixel content. It does **not**
//! independently show which import path the decoder took: this test
//! installs no tracing subscriber before `setup_e2e_server` (one installed
//! there stalled `native_client.rs` 5/5 times -- see that file's module
//! doc), so there is no `H.264 import path: ...` line in its output to
//! check. That claim -- zero-copy dmabuf import, on this same hardware and
//! code path -- is established separately, by Task 9's `gpu_h264_render`.

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, ClientEvent, Config, PublishedFrame};
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

/// Pump events until a frame is published, or `timeout` elapses. Copied from
/// `native_client.rs:55`: `Client` exposes no blocking frame call, and an
/// `acquire_frame` poll that never drains `next_event` would swallow the
/// very error events that explain a stall.
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
async fn h264_frames_from_the_server_render_into_the_dmabuf() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers (gpu capture)");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--gradient --drm-direct",
        // Pin H.264 rather than relying on the adaptation policy choosing
        // it. Sessions already start in H.264 (`io_bridge.rs:3435`), but
        // "already" is how a test comes to depend on a default nobody
        // meant it to.
        extra_env: &[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264")],
        // The server's full-frame H.264 encoder reads NV12 from the GPU
        // capture pipeline (`analysis.nv12_data`, populated by
        // `capture/gpu_pipeline`). With CPU capture there is no H.264 to
        // receive at all, and this test would time out waiting for frames
        // that were never encoded.
        gpu: true,
        webgpu: false,
        url_query_extra: "",
    })
    .await
    .expect("bring up headscale + ghostframe server");

    eprintln!("[phase] containers up; building client");
    let state_dir = tempfile::tempdir().expect("tempdir");
    let mut client = Client::new(Config {
        hostname: "h264-client-test".into(),
        authkey: String::new(), // unused: the bridge below is already up
        state_dir: state_dir.path().to_path_buf(),
        supports_h264: true,
        indices_raw: false,
        n_export_buffers: 3,
        preferred_modifiers: vec![],
        // Diagnostic readback: this test asserts on dmabuf contents.
        debug_map_frames: true,
    })
    .expect("create client");

    // If the probe says no, the test cannot mean anything and should say so
    // rather than time out waiting for H.264 frames that VA-API can't
    // decode on this machine.
    assert!(
        client.supports_h264(),
        "this machine must have VA-API H.264 decode to run the M3 acceptance test"
    );

    // Share the harness's tsnet node: a second tsnet.Server in one process
    // does not converge a working peer datapath (both nodes log in,
    // headscale lists them online, and no packet crosses).
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
            panic!(
                "no frame published within 60s -- check the server actually entered \
                 H.264 mode (grep the container logs above for \"h264\")"
            );
        }
    };

    eprintln!(
        "frame_id={} buffer_id={} {}x{} damage={:?}",
        frame.frame_id, frame.buffer_id, frame.width, frame.height, frame.damage
    );

    let f = client.debug_map_frame(&frame).expect("map exported dmabuf");

    // Rows are stride-padded, NOT tightly packed: `DebugFrameBytes` carries
    // `offset` and `stride` precisely because the export buffer's row
    // pitch is the driver's choice. Indexing as `y * width * 4` reads the
    // wrong pixels on any padded surface, and fails in a way that looks
    // like a decode bug rather than an indexing bug.
    let sample = |x: u64, y: u64| -> [u8; 4] {
        let o = (f.offset + y * f.stride + x * 4) as usize;
        [f.bytes[o], f.bytes[o + 1], f.bytes[o + 2], f.bytes[o + 3]]
    };

    // H.264 is lossy, so this test asserts structure rather than exact
    // values: a gradient test pattern must actually vary across the
    // decoded frame. The exact-value gates belong to the three exactness
    // oracles elsewhere (hw decode == sw decode, shader == reference, plus
    // the NV12 conversion oracle) -- this is not the place to add an SSIM
    // score or a percentage tolerance, because there is no reference frame
    // here to compare against, only the live server's own encode of a
    // pattern this test does not control pixel-for-pixel.
    //
    // Threshold justification: 32 (of 256 possible red values) is chosen
    // to be far above what H.264 quantisation noise could produce on a
    // flat region (typically single-digit variation at any reasonable QP)
    // while being far below the gradient's actual full-frame swing
    // (~0..255 by construction in `ghostframe-test-pattern`'s gradient
    // mode). A near-flat frame within this threshold means decode or
    // colour conversion produced a broken (e.g. all-zero, or NV12
    // misread-as-RGB) image, not ordinary lossy compression.
    let mut lo = 255u8;
    let mut hi = 0u8;
    for y in (0..u64::from(frame.height)).step_by(16) {
        for x in (0..u64::from(frame.width)).step_by(16) {
            let px = sample(x, y);
            lo = lo.min(px[0]);
            hi = hi.max(px[0]);
        }
    }

    eprintln!("[m3] red channel spans {lo}..{hi} across the decoded frame");
    assert!(
        hi - lo > 32,
        "decoded red channel spans only {lo}..{hi}. The test pattern is a gradient, \
         so a near-flat frame means the decode or the colour conversion failed -- \
         not ordinary H.264 quantisation. A frame that never arrived at all fails \
         earlier, in wait_for_frame."
    );

    client.release_frame(frame.frame_id);
    client.disconnect().expect("disconnect");
}
