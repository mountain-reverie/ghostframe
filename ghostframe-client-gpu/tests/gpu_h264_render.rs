//! An access unit in, a plausible image in the framebuffer out.
//!
//! "Plausible", not "correct": this asserts wiring (something drew, luma
//! and chroma both actually landed) rather than exact pixels. Task 8's Tier
//! A/B oracles in `nv12_oracle_tests.rs` already cover the NV12->RGBA
//! arithmetic bit-exactly against a real GPU target; re-deriving that here
//! from a lossy H.264 clip would only add tolerance, not coverage.
//!
//! Also hosts the M3 §10 manual measurement harnesses
//! (`decode_and_publish_timing_at_1920x1080_{unpaced,paced_60hz}`) -- not
//! correctness tests, but they reuse [`assert_looks_like_gradient`] so a run
//! that measured nothing (every `decode()` silently erroring, say) still
//! fails loudly instead of reporting a plausible-looking empty sample set.
//!
//! Two more measurement harnesses live at the bottom of this file,
//! `h264_mode_reentry_timing` and `h264_resolution_change_timing`: they
//! settle the design question of whether the ~43.5ms session-start H.264
//! cost (§10: `H264Decoder::new()` plus a cold surface-pool init inside the
//! first `avcodec_send_packet`) recurs every time the server's classifier
//! switches back into `FrameMode::H264` from `FrameMode::TileCodec`, or on a
//! mid-session resolution change. See their doc comments for the harness
//! design and [`parse_log`] for how they reuse `renderer.rs`'s existing
//! `decode_us`/`map_us`/`new_us` debug instrumentation instead of adding a
//! second one.
//!
//! Requires a GPU and VA-API. Deliberately NOT named in any CI workflow.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ghostframe_client_core::Event;
use ghostframe_client_gpu::renderer::Renderer;
use ghostframe_client_gpu::wgpu_ctx::WgpuContext;
use ghostframe_client_h264::testclip::gradient_clip;

/// Shared skip gate for every test in this file: a usable GPU, and the
/// driver's own claim of an H.264 VLD entrypoint.
///
/// Gates on `vainfo_reports_h264_vld`, not `vaapi_h264_decode_available()`:
/// the latter decodes a frame to answer, so gating on it would mean a
/// decoder regression makes every test here skip and report green -- the
/// coverage would vanish exactly when it should fail.
fn gpu_and_vaapi_or_skip() -> Option<WgpuContext> {
    let ctx = match WgpuContext::new() {
        Ok(ctx) => ctx,
        Err(_) => {
            eprintln!("no usable GPU; skipping");
            return None;
        }
    };
    match ghostframe_client_h264::vainfo_reports_h264_vld(
        ghostframe_client_h264::probe::RENDER_NODE,
    ) {
        Some(true) => Some(ctx),
        Some(false) => {
            eprintln!("driver reports no H.264 VLD entrypoint; skipping");
            None
        }
        None => {
            eprintln!("vainfo unavailable; cannot establish ground truth, skipping");
            None
        }
    }
}

/// Assert `pixels` (tightly packed RGBA8, `w`x`h`) looks like a decoded
/// `gradient_clip` frame, not a broken pipeline's output.
///
/// Three checks, each closing a hole the others miss:
/// - enough non-black pixels that something drew at all;
/// - real channel spread, so a pipeline that blitted one wrong-but-nonzero
///   constant into every texel (a broken bind group, a stuck sampler) still
///   fails, not just "some pixel is non-black";
/// - R/B diverging from G, so chroma dropped entirely (or bound to the
///   wrong slot) still fails. This does not catch a U/V swap -- both still
///   diverge from G either way -- it only closes the
///   chroma-not-reaching-the-framebuffer-at-all hole the first two checks
///   cannot.
fn assert_looks_like_gradient(pixels: &[u8], w: u32, h: u32) {
    assert_eq!(pixels.len(), w as usize * h as usize * 4);

    let mut min = [255u8; 3];
    let mut max = [0u8; 3];
    let mut nonblack = 0usize;
    for p in pixels.chunks_exact(4) {
        for c in 0..3 {
            min[c] = min[c].min(p[c]);
            max[c] = max[c].max(p[c]);
        }
        if p[0] > 8 || p[1] > 8 || p[2] > 8 {
            nonblack += 1;
        }
    }
    let total = (w * h) as usize;
    assert!(
        nonblack > total / 4,
        "only {nonblack} of {total} pixels are non-black -- the decode path drew nothing"
    );
    let spread = (0..3).map(|c| max[c] - min[c]).max().unwrap();
    assert!(
        spread > 32,
        "framebuffer is nearly uniform (max channel spread {spread}); a gradient clip \
         must produce a non-uniform framebuffer -- min={min:?} max={max:?}"
    );

    let chroma_diverges = pixels
        .chunks_exact(4)
        .any(|p| p[0].abs_diff(p[1]) > 16 || p[2].abs_diff(p[1]) > 16);
    assert!(
        chroma_diverges,
        "R and B never diverge from G -- chroma is not reaching the framebuffer"
    );
}

#[test]
fn a_decoded_frame_lands_in_the_framebuffer() {
    // Otherwise `blit_h264_frame`'s `tracing::info!("H.264 import path: ...")`
    // goes nowhere, even under `--nocapture`: there is no default subscriber.
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter("info")
        .try_init();

    let Some(ctx) = gpu_and_vaapi_or_skip() else {
        return;
    };

    let mut renderer = Renderer::new(&ctx, 640, 480, 3, &[], true).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: 640,
            height: 480,
        },
    );

    let clip = gradient_clip(640, 480, 4);
    for (i, au) in clip.iter().enumerate() {
        renderer.apply_event(
            &ctx,
            &Event::NeedsH264 {
                frame_seq: i as u32,
                timestamp_us: i as u32 * 16_667,
                is_keyframe: i == 0,
                payload: au.clone(),
            },
        );
    }
    // libx264 defaults to B-frames, so the last access unit(s) fed above can
    // still be buffered by reordering rather than decoded yet -- drain them
    // so the assertions below see the whole clip, not however much of it
    // happened to have already left the decoder.
    renderer.finish_h264(&ctx);
    renderer.flush(&ctx);

    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_looks_like_gradient(&pixels, 640, 480);
}

/// Not a correctness test: this drives `Renderer` through enough 1920x1080
/// decode/flush/publish/release cycles, at the production resolution, to
/// give M3 design doc §10 real percentile samples -- `decode_h264`'s
/// `decode_us` and `H264Decoder::new`'s `new_us` debug lines,
/// `blit_h264_frame`'s `map_us` (the `vaSyncSurface` wait -- see that call
/// site's doc comment for why `decode_us` alone understates the real decode
/// cost), and `ring::publish`'s `poll_us`, all from the same run.
///
/// `host_visible: false` on the `Renderer::new` call below, matching
/// production (`Config::debug_map_frames` defaults false, per that
/// parameter's own doc comment): export buffers are device-local, the same
/// memory type `fb.blit_rects`/`poll_us` measures in production. An earlier
/// version of this harness passed `true`, which made the export buffers
/// CPU-mappable and put a different memory type than production behind the
/// `poll_us` numbers.
///
/// Two entry points below share this body, `paced` distinguishing them:
/// `paced = false` decodes as fast as the hardware allows, back-to-back;
/// `paced = true` schedules each frame's arrival at the real 16.667ms (60Hz)
/// cadence production actually delivers at, sleeping off whatever headroom
/// processing that frame left. Both are worth having: unpaced shows the
/// hardware's raw throughput ceiling, paced shows what a real session's
/// steady arrival rate actually costs the render thread, which is the
/// number the frame-budget question in design doc §10.6 needs.
///
/// Run with e.g.
/// `RUST_LOG=ghostframe_client_gpu::renderer=debug,ghostframe_client_gpu::ring=debug
/// cargo test -p ghostframe-client-gpu --test gpu_h264_render \
/// decode_and_publish_timing -- --ignored --nocapture` and parse the emitted
/// `decode_us` / `new_us` / `map_us` / `poll_us` fields.
///
/// `#[ignore]`d: these are manual measurement harnesses, not part of the
/// default `cargo test` run -- each synthesizes and decodes ~120 1080p
/// frames (the paced one also takes ~2s of real wall-clock sleep), and the
/// point is the debug-log side effects a normal test run discards.
fn run_decode_and_publish_timing(paced: bool) {
    let _ = tracing_subscriber::fmt()
        .with_test_writer()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ghostframe_client_gpu=debug".into()),
        )
        .try_init();

    let Some(ctx) = gpu_and_vaapi_or_skip() else {
        return;
    };

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    const FRAME_COUNT: usize = 120;
    const ARRIVAL_INTERVAL: Duration = Duration::from_micros(16_667);

    eprintln!(
        "=== decode_and_publish_timing: paced={paced} ({} frames at {WIDTH}x{HEIGHT}) ===",
        FRAME_COUNT
    );

    let mut renderer = Renderer::new(&ctx, WIDTH, HEIGHT, 3, &[], false).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: WIDTH,
            height: HEIGHT,
        },
    );

    let clip = gradient_clip(WIDTH, HEIGHT, FRAME_COUNT);
    // Real wall-clock scheduling on purpose: this reproduces production's
    // actual frame-arrival cadence, not a virtual clock -- there is no
    // paused/virtual clock in this process for it to silently desync from.
    let run_start = Instant::now();
    for (i, au) in clip.iter().enumerate() {
        if paced {
            let deadline = run_start + ARRIVAL_INTERVAL * i as u32;
            let now = Instant::now();
            if now < deadline {
                std::thread::sleep(deadline - now);
            }
        }
        renderer.apply_event(
            &ctx,
            &Event::NeedsH264 {
                frame_seq: i as u32,
                timestamp_us: i as u32 * 16_667,
                is_keyframe: i == 0,
                payload: au.clone(),
            },
        );
        // Mirrors `render_thread.rs`'s real per-event loop (decode ->
        // flush -> publish -> release) rather than batching every frame's
        // decode ahead of a single flush/publish, so `ring::publish`'s
        // `poll_us` samples are the same shape the production render
        // thread produces: one publish per decoded frame, interleaved with
        // decode work, not decode work run in isolation from it.
        renderer.flush(&ctx);
        if let Some(pf) = renderer.publish(&ctx) {
            renderer.release(pf.frame_id);
        }
    }
    renderer.finish_h264(&ctx);
    renderer.flush(&ctx);

    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_looks_like_gradient(&pixels, WIDTH, HEIGHT);
}

#[test]
#[ignore = "manual measurement harness for design doc §10, see run_decode_and_publish_timing's doc"]
fn decode_and_publish_timing_at_1920x1080_unpaced() {
    run_decode_and_publish_timing(false);
}

#[test]
#[ignore = "manual measurement harness for design doc §10, see run_decode_and_publish_timing's doc"]
fn decode_and_publish_timing_at_1920x1080_paced_60hz() {
    run_decode_and_publish_timing(true);
}

// ---------------------------------------------------------------------
// §10 follow-up: does the ~43.5ms session-start cost recur on mode
// re-entry or resolution change?
// ---------------------------------------------------------------------

/// A [`std::io::Write`] sink that appends into a shared buffer. Cloning
/// shares the same `Arc`, which is what lets `tracing_subscriber::fmt`'s
/// `with_writer(move || ...)` closure hand out a fresh handle per log line
/// while every handle still lands in the one buffer this test reads back
/// afterward.
#[derive(Clone)]
struct SharedBufWriter(Arc<Mutex<Vec<u8>>>);

impl Write for SharedBufWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Install a `tracing` subscriber that writes formatted log lines into an
/// in-memory buffer instead of stderr, scoped to the current thread for the
/// life of the returned guard ([`tracing::subscriber::set_default`], not
/// global `try_init`) -- so this does not race the global-subscriber
/// double-init that would otherwise make a second ignored test in the same
/// process silently capture nothing.
///
/// `with_ansi(false)` keeps the captured text free of colour escapes, which
/// would otherwise land inside the digit runs [`field_u64`] parses.
fn capture_renderer_logs() -> (Arc<Mutex<Vec<u8>>>, tracing::subscriber::DefaultGuard) {
    let buf = Arc::new(Mutex::new(Vec::new()));
    let make_writer = {
        let buf = buf.clone();
        move || SharedBufWriter(buf.clone())
    };
    let subscriber = tracing_subscriber::fmt()
        .with_writer(make_writer)
        .with_ansi(false)
        .with_env_filter(tracing_subscriber::EnvFilter::new(
            "ghostframe_client_gpu=debug",
        ))
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (buf, guard)
}

/// Pull the unsigned integer immediately following `key` (e.g. `"decode_us="`)
/// out of one formatted log line.
fn field_u64(line: &str, key: &str) -> Option<u64> {
    let start = line.find(key)? + key.len();
    let rest = &line[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    rest[..end].parse().ok()
}

/// Every M3 §10 timing this file's two `#[ignore]`d re-entry/resolution
/// harnesses need, pulled out of `renderer.rs`'s existing `tracing::debug!`
/// lines rather than a second measurement mechanism -- see
/// [`capture_renderer_logs`] and this module's doc comment.
///
/// Keyed by the AU's `frame_seq`, which `decode_h264` and `blit_h264_frame`
/// both log. That is unambiguous for every frame this harness measures: a
/// keyframe (the only frame this harness ever times individually) cannot be
/// reordering-delayed, so it produces exactly one output frame and thus one
/// `map_dmabuf stall` line per `frame_seq`. A later B-frame-buffered AU
/// could in principle share a `frame_seq` across more than one blitted
/// frame -- but see [`LogLine::Decode`]'s doc for why this harness does
/// *not* assume a 1:1 AU-to-blit correspondence when reading the
/// re-entry/resize frame specifically.
///
/// One event per relevant log line, in the log's own chronological order --
/// not pre-aggregated into a `HashMap<frame_seq, _>`, because that would
/// silently assume every decoded AU blits exactly once, immediately. It
/// does not: `testclip::gradient_clip` opens a plain `libx264` encoder with
/// no `tune=zerolatency` (unlike the server's real `FullFrameEncoder`, see
/// `h264_vaapi.rs`'s `opts.set("tune", "zerolatency")`), so it uses
/// libx264's default B-frame reordering -- an AU's decoded picture can sit
/// buffered inside the decoder for a frame or two after `decode()` returns,
/// and `blit_h264_frame` (hence `map_dmabuf stall`) only fires once
/// `avcodec_receive_frame` actually yields it, tagged with whichever AU's
/// `decode()` call happened to trigger that release -- not necessarily the
/// AU whose picture it is. A `HashMap` keyed by the re-entry AU's own
/// `frame_seq` would then report "no `map_us` for this frame" as if nothing
/// happened, when what actually happened is the sync wait for that exact
/// picture landed a line or two later under a different `frame_seq`. See
/// [`first_map_after`], which walks this ordered log forward from the
/// re-entry `Decode` event to find it.
#[derive(Debug, Clone, Copy)]
enum LogLine {
    /// `H264Decoder::new()` stall, once per session-start decoder open.
    NewStall(u64),
    /// `decode_h264` detected a sequence gap on this `frame_seq` and
    /// therefore called `decoder.reset()` (see that field's doc on
    /// `Renderer::h264_needs_reset`) immediately before decoding it.
    GapDetected(u32),
    Decode {
        seq: u32,
        decode_us: u64,
    },
    Map {
        seq: u32,
        map_us: u64,
    },
}

fn parse_log(buf: &[u8]) -> Vec<LogLine> {
    let text = String::from_utf8_lossy(buf);
    let mut out = Vec::new();
    for line in text.lines() {
        if line.contains("H264Decoder::new stall") {
            if let Some(v) = field_u64(line, "new_us=") {
                out.push(LogLine::NewStall(v));
            }
        } else if line.contains("decoder.decode stall") {
            if let (Some(seq), Some(decode_us)) =
                (field_u64(line, "frame_seq="), field_u64(line, "decode_us="))
            {
                out.push(LogLine::Decode {
                    seq: seq as u32,
                    decode_us,
                });
            }
        } else if line.contains("map_dmabuf stall") {
            if let (Some(seq), Some(map_us)) =
                (field_u64(line, "frame_seq="), field_u64(line, "map_us="))
            {
                out.push(LogLine::Map {
                    seq: seq as u32,
                    map_us,
                });
            }
        } else if line.contains("access unit gap detected") {
            if let Some(seq) = field_u64(line, "frame_seq=") {
                out.push(LogLine::GapDetected(seq as u32));
            }
        }
    }
    out
}

fn new_us_values(log: &[LogLine]) -> Vec<u64> {
    log.iter()
        .filter_map(|l| match l {
            LogLine::NewStall(v) => Some(*v),
            _ => None,
        })
        .collect()
}

fn gap_detected_seqs(log: &[LogLine]) -> Vec<u32> {
    log.iter()
        .filter_map(|l| match l {
            LogLine::GapDetected(seq) => Some(*seq),
            _ => None,
        })
        .collect()
}

/// The `decode_us` this exact AU's `decoder.decode()` call cost. Unlike
/// `map_us`, this needs no reordering-aware lookup: `decode_h264` logs it
/// once per AU, unconditionally, immediately after that AU's own
/// `decode()` call -- so if a cold-pool-init cost from Task 9's spec §10
/// were going to show up anywhere, it is here, on the AU that actually
/// called `avcodec_send_packet`, not on whichever later AU's `decode()`
/// happens to trigger this picture's release.
fn decode_us_for(log: &[LogLine], seq: u32) -> Option<u64> {
    log.iter().find_map(|l| match l {
        LogLine::Decode { seq: s, decode_us } if *s == seq => Some(*decode_us),
        _ => None,
    })
}

/// Walk `log` forward from the `Decode` event at `from_seq` to the next
/// `Map` event, wherever it falls -- see [`LogLine::Decode`]'s doc for why
/// that is not necessarily tagged `from_seq` itself. Returns
/// `(blitted_seq, map_us, lag)`, where `lag = blitted_seq - from_seq` is how
/// many AUs later (in this synthetic B-frame-reordered clip only --
/// production's `tune=zerolatency` encoder has no such lag) the sync wait
/// for `from_seq`'s own picture actually happened.
fn first_map_after(log: &[LogLine], from_seq: u32) -> Option<(u32, u64, u32)> {
    let start = log
        .iter()
        .position(|l| matches!(l, LogLine::Decode { seq, .. } if *seq == from_seq))?;
    log[start..].iter().find_map(|l| match l {
        LogLine::Map { seq, map_us } => Some((*seq, *map_us, seq.wrapping_sub(from_seq))),
        _ => None,
    })
}

/// Steady-state `decode_us`/`map_us` samples whose AU `frame_seq` falls in
/// `seqs`, each list in log order. `sum` below pairs them by position in
/// these lists (the `i`-th decode occurrence in range with the `i`-th map
/// occurrence in range), which is only an approximate per-frame pairing
/// when the encoder buffers reordering (see [`LogLine::Decode`]'s doc) --
/// fine for an aggregate percentile over dozens of steady-state samples,
/// where every sample is close to every other regardless of exactly which
/// AU's sync wait it is paired with.
fn steady_samples(log: &[LogLine], seqs: std::ops::Range<u32>) -> (Vec<u64>, Vec<u64>, Vec<u64>) {
    let decode: Vec<u64> = log
        .iter()
        .filter_map(|l| match l {
            LogLine::Decode { seq, decode_us } if seqs.contains(seq) => Some(*decode_us),
            _ => None,
        })
        .collect();
    let map: Vec<u64> = log
        .iter()
        .filter_map(|l| match l {
            LogLine::Map { seq, map_us } if seqs.contains(seq) => Some(*map_us),
            _ => None,
        })
        .collect();
    let sum: Vec<u64> = decode.iter().zip(map.iter()).map(|(d, m)| d + m).collect();
    (decode, map, sum)
}

/// Nearest-rank percentile over an already-sorted slice.
fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn report_percentiles(label: &str, mut values: Vec<u64>) {
    if values.is_empty() {
        eprintln!("{label}: no samples captured");
        return;
    }
    values.sort_unstable();
    eprintln!(
        "{label}: n={} p50={}us p99={}us max={}us",
        values.len(),
        percentile(&values, 0.50),
        percentile(&values, 0.99),
        values[values.len() - 1],
    );
}

/// Feed one clip's access units through `renderer` starting at `start_seq`
/// (only the clip's first AU is marked `is_keyframe`, mirroring both
/// `testclip::gradient_clip`'s in-band-SPS-per-clip design and the server's
/// real re-entry/resolution-change IDR), running the same
/// decode-flush-publish-release sequence per frame as production's render
/// thread. Returns the next unused `frame_seq`.
fn drive_clip(renderer: &mut Renderer, ctx: &WgpuContext, clip: &[Vec<u8>], start_seq: u32) -> u32 {
    let mut seq = start_seq;
    for (i, au) in clip.iter().enumerate() {
        renderer.apply_event(
            ctx,
            &Event::NeedsH264 {
                frame_seq: seq,
                timestamp_us: seq.wrapping_mul(16_667),
                is_keyframe: i == 0,
                payload: au.clone(),
            },
        );
        renderer.flush(ctx);
        if let Some(pf) = renderer.publish(ctx) {
            renderer.release(pf.frame_id);
        }
        seq = seq.wrapping_add(1);
    }
    seq
}

/// M3 §10 follow-up (A): does the server's `FrameMode::H264` <->
/// `FrameMode::TileCodec` <-> `FrameMode::H264` classifier flapping repay
/// the ~43.5ms session-start cost on every re-entry?
///
/// `Renderer::h264_decoder` is opened once and never torn down --
/// `Event::FrameDimensions`'s `apply_event` arm does not touch it, and
/// nothing else does either -- so re-entry cannot pay `H264Decoder::new()`
/// again. This harness checks the other half: whether re-entry's forced IDR
/// (`io_bridge.rs`, "Re-entry into H264: force IDR for a fresh client
/// anchor") decodes at steady-state cost, or at something closer to the
/// cold-pool-init cost, given that `frame_seq` genuinely jumps across the
/// tile-codec interval on the wire (the server's `self.frame_seq` counter
/// is global and monotonic across both modes, but a client only receives
/// `NeedsH264` events for the frames actually sent as H.264 -- see
/// `io_bridge.rs`'s `process_frame_gpu`). That jump is exactly what
/// `Renderer::decode_h264`'s gap detector is built to catch, and since the
/// re-entry frame is always a keyframe, it fires `decoder.reset()`
/// (`avcodec_flush_buffers`) immediately before decoding it -- this harness
/// also confirms that fires on every transition, not just asserts it from
/// reading the code.
///
/// Three transitions (not one): a single sample cannot distinguish "the
/// first is special" from "they all cost the same", which is exactly the
/// distinction the project owner's question turns on.
///
/// `#[ignore]`d for the same reason as `decode_and_publish_timing_*`: a
/// manual measurement harness, not a correctness gate.
#[test]
#[ignore = "manual measurement harness for design doc §10 (mode re-entry cost), see this test's doc"]
fn h264_mode_reentry_timing() {
    let (log_buf, _guard) = capture_renderer_logs();

    let Some(ctx) = gpu_and_vaapi_or_skip() else {
        return;
    };

    const WIDTH: u32 = 1920;
    const HEIGHT: u32 = 1080;
    // Real production dwell: a scene busy enough to flap modes is still
    // typically stable for at least several hundred ms at a time, not
    // flickering frame-to-frame. 45 frames at the harness's 16.667ms
    // nominal spacing is ~0.75s -- long enough that this is genuinely a
    // "gap", not an off-by-one in the sequence check.
    const IDLE_GAP_FRAMES: u32 = 45;
    const WARMUP_FRAMES: usize = 40;
    // Skip the first few warmup frames when computing the steady-state
    // baseline: frame 0 of the very first clip this process ever decodes
    // pays both `H264Decoder::new()` (captured separately as `new_us`) AND
    // the cold surface-pool init inside its first `decode()`/`map_dmabuf()`
    // -- exactly the ~43.5ms session-start cost design doc §10 already
    // documents, not steady state.
    const STEADY_SKIP: u32 = 5;
    const TRANSITIONS: usize = 3;
    const POST_TRANSITION_FRAMES: usize = 10;

    eprintln!("=== h264_mode_reentry_timing ({WIDTH}x{HEIGHT}) ===");

    let mut renderer = Renderer::new(&ctx, WIDTH, HEIGHT, 3, &[], false).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: WIDTH,
            height: HEIGHT,
        },
    );

    let warmup_clip = gradient_clip(WIDTH, HEIGHT, WARMUP_FRAMES);
    let mut seq = drive_clip(&mut renderer, &ctx, &warmup_clip, 0);

    let mut transition_first_seqs = Vec::with_capacity(TRANSITIONS);
    for _ in 0..TRANSITIONS {
        // The tile-codec interval: no `NeedsH264` events at all, matching
        // what a client actually receives while the server is in
        // `FrameMode::TileCodec` -- but the server's frame_seq counter kept
        // advancing underneath it, so the next H.264 AU's frame_seq is not
        // `prev + 1`.
        seq = seq.wrapping_add(IDLE_GAP_FRAMES);
        // A fresh `gradient_clip` call is a fresh libx264 session: its
        // first AU is an IDR with in-band SPS/PPS, standing in for the
        // server's forced re-entry keyframe exactly as this test's doc
        // comment describes.
        let clip = gradient_clip(WIDTH, HEIGHT, POST_TRANSITION_FRAMES);
        transition_first_seqs.push(seq);
        seq = drive_clip(&mut renderer, &ctx, &clip, seq);
    }

    renderer.finish_h264(&ctx);
    renderer.flush(&ctx);
    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_looks_like_gradient(&pixels, WIDTH, HEIGHT);

    let log = parse_log(&log_buf.lock().unwrap());

    eprintln!(
        "H264Decoder::new() stalls observed: {:?} (expect exactly 1 -- confirms the decoder is \
         opened once per session, never torn down across mode transitions)",
        new_us_values(&log)
    );

    let (steady_decode, steady_map, steady_sum) =
        steady_samples(&log, STEADY_SKIP..WARMUP_FRAMES as u32);
    eprintln!("--- steady-state baseline (warmup frames {STEADY_SKIP}..{WARMUP_FRAMES}) ---");
    report_percentiles("decode_us", steady_decode);
    report_percentiles("map_us", steady_map);
    report_percentiles("decode_us+map_us", steady_sum);

    let gaps = gap_detected_seqs(&log);
    eprintln!("--- per-transition first-frame cost ---");
    for (i, seq) in transition_first_seqs.iter().enumerate() {
        let d = decode_us_for(&log, *seq);
        // The re-entry AU's own decode() cost is unambiguous (see
        // `decode_us_for`'s doc); its blit is not, since this synthetic
        // clip's B-frame reordering can delay this exact picture's
        // `avcodec_receive_frame` release by a frame or two -- see
        // `first_map_after`'s doc. Report both: the AU's own decode cost,
        // and wherever its picture's sync wait actually landed.
        let blit = first_map_after(&log, *seq);
        let reset_fired = gaps.contains(seq);
        match blit {
            Some((blitted_seq, map_us, lag)) => {
                let sum = d.map(|d| d + map_us);
                eprintln!(
                    "transition {}: frame_seq={} decode_us={:?} gap_detected/reset_fired={} | \
                     picture blitted at frame_seq={} (lag={} AUs) map_us={} decode_us+map_us={:?}",
                    i + 1,
                    seq,
                    d,
                    reset_fired,
                    blitted_seq,
                    lag,
                    map_us,
                    sum,
                );
            }
            None => {
                eprintln!(
                    "transition {}: frame_seq={} decode_us={:?} gap_detected/reset_fired={} | \
                     picture never blitted before the run ended",
                    i + 1,
                    seq,
                    d,
                    reset_fired,
                );
            }
        }
    }
    eprintln!(
        "gap-detected frame_seqs: {gaps:?} (expect exactly {transition_first_seqs:?} -- one per \
         transition, each on the re-entry keyframe)"
    );
}

/// M3 §10 follow-up (B): a resolution change forces a new SPS, which is the
/// case most likely to actually pay the ~30.75ms cold-pool-init cost mid-
/// session -- unlike plain mode re-entry (harness A above), a new SPS can
/// force ffmpeg to reallocate the hardware frames context, and nothing in
/// `Renderer` resets or recreates `h264_decoder` on `Event::FrameDimensions`
/// to pre-empt that.
///
/// Production-plausible sizes: 1920x1080 -> 1280x720 -> 1920x1080, matching
/// this task's brief. Two resolution changes, not one, so a second visit to
/// an already-seen size can be compared against the first visit to a new
/// one -- if ffmpeg or the driver caches/reuses anything keyed by
/// resolution, that would show up as the second change costing less than
/// the first.
///
/// Unlike harness A, `frame_seq` stays strictly sequential across both
/// resolution changes here -- a real resolution change does not pass
/// through a `FrameMode::TileCodec` interval, so the server's client-visible
/// H.264 frame_seq sequence has no gap at this boundary, and
/// `Renderer::decode_h264`'s gap detector should NOT fire `decoder.reset()`
/// here. This harness checks that too, not just decode/map cost.
///
/// `#[ignore]`d for the same reason as the harnesses above.
#[test]
#[ignore = "manual measurement harness for design doc §10 (resolution-change cost), see this test's doc"]
fn h264_resolution_change_timing() {
    let (log_buf, _guard) = capture_renderer_logs();

    let Some(ctx) = gpu_and_vaapi_or_skip() else {
        return;
    };

    const W1: u32 = 1920;
    const H1: u32 = 1080;
    const W2: u32 = 1280;
    const H2: u32 = 720;
    const WARMUP_FRAMES: usize = 30;
    const POST_RESIZE_FRAMES: usize = 15;

    eprintln!("=== h264_resolution_change_timing ({W1}x{H1} <-> {W2}x{H2}) ===");

    let mut renderer = Renderer::new(&ctx, W1, H1, 3, &[], false).expect("renderer");
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: W1,
            height: H1,
        },
    );

    let clip_1080 = gradient_clip(W1, H1, WARMUP_FRAMES);
    let mut seq = drive_clip(&mut renderer, &ctx, &clip_1080, 0);

    // Resolution change 1: 1920x1080 -> 1280x720. `FrameDimensions` first,
    // same order the server's own event stream guarantees (the tile/H.264
    // dimensions change is announced before pixels at the new size arrive).
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: W2,
            height: H2,
        },
    );
    let clip_720 = gradient_clip(W2, H2, POST_RESIZE_FRAMES);
    let resize1_first_seq = seq;
    seq = drive_clip(&mut renderer, &ctx, &clip_720, seq);

    // Resolution change 2: back to 1920x1080.
    renderer.apply_event(
        &ctx,
        &Event::FrameDimensions {
            width: W1,
            height: H1,
        },
    );
    let clip_1080_again = gradient_clip(W1, H1, POST_RESIZE_FRAMES);
    let resize2_first_seq = seq;
    seq = drive_clip(&mut renderer, &ctx, &clip_1080_again, seq);
    let _ = seq;

    renderer.finish_h264(&ctx);
    renderer.flush(&ctx);
    let pixels = renderer.debug_read_framebuffer(&ctx);
    assert_looks_like_gradient(&pixels, W1, H1);

    let log = parse_log(&log_buf.lock().unwrap());

    eprintln!(
        "H264Decoder::new() stalls observed: {:?} (expect exactly 1)",
        new_us_values(&log)
    );

    let gaps = gap_detected_seqs(&log);
    for (label, seq) in [
        ("resize 1 (1920x1080 -> 1280x720)", resize1_first_seq),
        ("resize 2 (1280x720 -> 1920x1080)", resize2_first_seq),
    ] {
        let d = decode_us_for(&log, seq);
        let reset_fired = gaps.contains(&seq);
        // See `h264_mode_reentry_timing`'s equivalent block for why the
        // decode cost is read directly by `frame_seq` but the blit cost
        // needs `first_map_after`'s reordering-aware forward search.
        match first_map_after(&log, seq) {
            Some((blitted_seq, map_us, lag)) => {
                let sum = d.map(|d| d + map_us);
                eprintln!(
                    "{label}: frame_seq={seq} decode_us={d:?} gap_detected/reset_fired=\
                     {reset_fired} | picture blitted at frame_seq={blitted_seq} (lag={lag} AUs) \
                     map_us={map_us} decode_us+map_us={sum:?}",
                );
            }
            None => {
                eprintln!(
                    "{label}: frame_seq={seq} decode_us={d:?} gap_detected/reset_fired=\
                     {reset_fired} | picture never blitted before the run ended",
                );
            }
        }
    }
    eprintln!(
        "gap-detected frame_seqs: {gaps:?} (expect empty -- a resolution change alone has no \
         client-visible frame_seq gap, so decoder.reset() should NOT fire here)"
    );
}
