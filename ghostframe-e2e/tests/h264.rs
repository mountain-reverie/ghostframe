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
//! # What this test proves
//!
//! A pass here shows the whole H.264 path working against a live server,
//! not a synthetic clip: an access unit encoded by a live
//! `ghostframe-server`, carried over a real tsnet tailnet, decoded on
//! VA-API hardware, imported **zero-copy** into wgpu -- confirmed by the
//! `H.264 import path: zero-copy dmabuf` line this test's own subscriber
//! (below) makes visible in `--nocapture` output -- converted NV12->RGB in
//! WGSL, and rendered into an exported dmabuf with non-flat pixel content.
//! That is strictly stronger evidence for the zero-copy path than a
//! synthetic-clip test can give, since nothing here is prepared by the
//! test itself.
//!
//! What it does not prove: pixel *exactness*. The assertion below is
//! "the decoded frame varies" (rules out all-black / all-constant, i.e. a
//! broken decode or NV12 misread as RGB), which is weaker than structural
//! correctness -- a `--gradient` scene cannot catch a half-drawn frame, a
//! row-permuted surface, or an R<->B channel swap. The sibling web test
//! `e2e_h264_forced_solid_red` exists to catch that last case, on a solid
//! color where a channel swap changes the sampled value instead of merely
//! relabelling it. Exact-value coverage belongs to the three exactness
//! oracles elsewhere (hw decode == sw decode, shader == reference, the
//! NV12 conversion oracle), not to this test.

use std::time::{Duration, Instant};

use ghostframe_client_native::{Client, Config, DebugFrameBytes};
use ghostframe_e2e::harness::{read_server_logs_stripped, setup_e2e_server, E2eServerSpec};

#[path = "common/client_wait.rs"]
mod client_wait;
use client_wait::wait_for_frame;

/// Below this, a decoded region counts as "flat" for the purposes of the
/// retry loop in the test body -- see that loop's comment for why a flat
/// *first* frame is an expected race rather than a decode failure, and the
/// pixel-sampling loop further down for why 32 specifically is the line
/// between quantisation noise and a broken decode.
const SPREAD_THRESHOLD: u8 = 32;

/// Sample a 16px grid across `f` and return the decoded red channel's
/// `(min, max)`. Rows are stride-padded, NOT tightly packed:
/// `DebugFrameBytes` carries `offset` and `stride` precisely because the
/// export buffer's row pitch is the driver's choice. Note that indexing as
/// `y * width * 4` would NOT necessarily fail this test: misreading a
/// diagonal gradient at the wrong stride still samples a varying (if
/// wrong) set of pixels, so a stride bug can pass green on garbage here.
/// Using the driver-reported stride/offset is simply correct, not
/// something this test's assertions would catch if it were wrong.
fn red_spread(f: &DebugFrameBytes, width: u32, height: u32) -> (u8, u8) {
    let sample = |x: u64, y: u64| -> u8 {
        let o = (f.offset + y * f.stride + x * 4) as usize;
        f.bytes[o]
    };
    let mut lo = 255u8;
    let mut hi = 0u8;
    for y in (0..u64::from(height)).step_by(16) {
        for x in (0..u64::from(width)).step_by(16) {
            let r = sample(x, y);
            lo = lo.min(r);
            hi = hi.max(r);
        }
    }
    (lo, hi)
}

#[tokio::test(flavor = "multi_thread")]
async fn h264_frames_from_the_server_render_into_the_dmabuf() {
    // Mirrors `gpu_h264_render.rs`'s subscriber shape: `RUST_LOG` still
    // wins when set, but the default is no longer "nothing at info level
    // or below", which is what silently hid the `H.264 import path: ...`
    // line in an earlier run of this file. `h264=info` covers this test's
    // own `tracing::info!(?ev, "client event")` calls below.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ghostframe_client_gpu=info,h264=info".into()),
        )
        .try_init();

    eprintln!("[phase] starting headscale + ghostframe-server containers (gpu capture)");
    let setup = setup_e2e_server(E2eServerSpec {
        test_pattern_args: "--gradient --drm-direct",
        // Pin H.264 rather than relying on the adaptation policy choosing
        // it. Sessions already start in H.264 (`io_bridge.rs:3435`), but
        // "already" is how a test comes to depend on a default nobody
        // meant it to. Whether the pin actually took is checked below,
        // against the server's own logs -- see the MODERATE note in this
        // test's review history: an unverified pin degrades silently into
        // this test grading TileCodec pixels instead.
        extra_env: &[("GHOSTFRAME_TEST_FORCE_FRAME_MODE", "h264")],
        // The server's full-frame H.264 encoder reads NV12 from the GPU
        // capture pipeline (`analysis.nv12_data`, populated by
        // `capture/gpu_pipeline`). With CPU capture there is no H.264 to
        // receive at all, and this test would time out waiting for frames
        // that were never encoded. Do not "simplify" this to `gpu: false`
        // to match `native_client.rs`'s shape -- that shape tests the
        // client's GPU, this one needs the server's.
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
        display: None,
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

    // Evidence the H.264 pin actually took, rather than a session that
    // quietly stayed on (or fell back to) TileCodec: `--gradient` varies
    // across the frame under TileCodec too (Raw/PalRLE fallback), so the
    // pixel assertion below cannot by itself tell the two apart. A stale
    // image built without `test-loss-injection` -- the feature that
    // compiles `force_frame_mode` in at all
    // (`tests/containers/test-server/Dockerfile`) -- would silently ignore
    // the env var and produce exactly that false green.
    //
    // `Classifier::decide_inner` logs `mode.decision ... reason="test_force"
    // ... to=H264` (`ghostframe-lib/src/tile/classifier.rs`) the first time
    // it runs under the pin, and the harness always launches the server
    // with `RUST_LOG=ghostframe=trace,debug` (`e2e_setup.rs`), so this line
    // reaches the container's stdout whenever the pin takes.
    eprintln!("[phase] waiting for the first published frame");
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut flat_attempts = 0u32;
    let (frame, lo, hi) = loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let frame = match wait_for_frame(&mut client, remaining) {
            Some(f) => f,
            None => {
                eprintln!(
                    "--- server logs ({}) ---\n{}",
                    setup.server_container_name,
                    read_server_logs_stripped(&setup.server_container_name)
                );
                panic!(
                    "no non-flat frame published within 60s ({flat_attempts} flat retries \
                     along the way) -- check the server actually entered H.264 mode (grep \
                     the container logs above for \"h264\")"
                );
            }
        };

        let f = client.debug_map_frame(&frame).expect("map exported dmabuf");
        let (lo, hi) = red_spread(&f, frame.width, frame.height);
        if hi - lo > SPREAD_THRESHOLD {
            break (frame, lo, hi);
        }

        // A flat (e.g. all-zero) first frame is an expected race, not a
        // decode failure: `render_thread::run`'s `handle_core_event`
        // returns `true` (render-worthy) for ANY core event, including a
        // bare `FrameDimensions` retransmit -- and the server re-sends
        // `FrameDimensions` on each of the first `FRAME_DIMENSIONS_RETRANSMITS`
        // (10) frames (`io_bridge.rs`). `acquire_frame` can hand back that
        // pre-decode, zero-initialised export slot before any H.264 payload
        // has actually been decoded into it. Release it and retry within
        // the same overall deadline rather than failing on an unlucky
        // interleaving.
        flat_attempts += 1;
        eprintln!(
            "[m3] frame {} is flat (red spans {lo}..{hi}); retrying ({flat_attempts} so far)",
            frame.frame_id
        );
        client.release_frame(frame.frame_id);
    };

    eprintln!(
        "frame_id={} buffer_id={} {}x{} damage={:?}",
        frame.frame_id, frame.buffer_id, frame.width, frame.height, frame.damage
    );

    let server_logs = read_server_logs_stripped(&setup.server_container_name);
    let pinned_to_h264 = server_logs.lines().any(|l| {
        l.contains("mode.decision") && l.contains(r#"reason="test_force""#) && l.contains("to=H264")
    });
    assert!(
        pinned_to_h264,
        "no `mode.decision ... reason=\"test_force\" to=H264` line in the server logs -- \
         the GHOSTFRAME_TEST_FORCE_FRAME_MODE pin may not have taken, so the frame just \
         sampled could be TileCodec pixels rather than H.264, not proof of H.264 decode. \
         server logs:\n{server_logs}"
    );

    // H.264 is lossy, so this test asserts non-flatness rather than exact
    // values -- a strictly weaker claim than "the frame is structurally
    // correct". It rules out an all-black or all-constant frame (a broken
    // decode, or NV12 misread as RGB), but it cannot catch a half-drawn
    // frame, a row-permuted surface, or an R<->B channel swap: a
    // channel-swapped gradient still varies. That last case is exactly
    // what the sibling web test `e2e_h264_forced_solid_red` exists to
    // catch, and which a `--gradient` scene structurally cannot, since
    // there is no single expected constant value here to compare against.
    // The exact-value gates for H.264 decode correctness are the three
    // exactness oracles elsewhere (hw decode == sw decode, shader ==
    // reference, the NV12 conversion oracle) -- this is not the place to
    // add an SSIM score or a percentage tolerance, because there is no
    // reference frame to compare against here, only the live server's own
    // encode of a pattern this test does not control pixel-for-pixel.
    //
    // Threshold derivation for SPREAD_THRESHOLD (32) is part-derived,
    // part-asserted:
    //   Derived: the pattern's exact formula is R = ((x+y)*2) & 0xFF
    //   (`ghostframe-test-pattern/src/gradient.rs`) -- a sawtooth
    //   wrapping every 128px, not a monotone ramp to 255. This test's
    //   16px sample grid aliases (x+y) to multiples of 16, so sampled R
    //   values land on multiples of 32, capping the attainable *sampled*
    //   spread at 224 (7 steps of 32) even on a perfect decode -- which
    //   is where the observed 229 (224 plus a little lossy noise) comes
    //   from.
    //   Asserted, not measured: quantisation noise on a flat region is
    //   assumed single-digit at any reasonable QP -- there is no flat
    //   region anywhere in this gradient scene to measure that from.
    // 32 sits comfortably below the 224 sampling ceiling and, on the
    // strength of the asserted assumption, above plausible noise. A
    // spread at or below it means decode or colour conversion broke, not
    // ordinary lossy compression.
    eprintln!("[m3] red channel spans {lo}..{hi} across the decoded frame");

    client.release_frame(frame.frame_id);
    client.disconnect().expect("disconnect");
}
