//! Text-under-motion scene runner: a scrolling (or, as control, static) text
//! document on a shaped link, measured from server telemetry and from
//! screenshots of the browser's canvas.
//!
//! Stimulus: `ghostframe-test-pattern --x11-sink --scroll-text`. The
//! reference frames come from the *same* pure renderer the stimulus paints
//! with; because this library cannot depend on `ghostframe-test-pattern`
//! (it depends on `ghostframe-bench`, which depends on this crate), the
//! renderer is passed in as a closure -- see `examples/scroll_scene.rs`.
//!
//! # Metrics (all report-only; nothing here asserts a threshold)
//!
//! Timeline: `start`/`stop` are the `scroll-event` lines the test pattern logs
//! with `CLOCK_MONOTONIC` ns -- the same clock the server's `ghostframe::bench`
//! telemetry stamps -- so the scroll window `[start, stop]` is placed on the
//! telemetry timeline exactly.
//!
//! * `wire_bytes_per_s_scroll`: sum of `total_wire_bytes` over telemetry
//!   frames whose `capture_done_ns` falls in `[start, stop]`, divided by
//!   `stop - start`. Server-reported per-frame bytes (what `last_send`
//!   accounted), not NIC bytes.
//! * `h264_frame_frac` / `h264_time_frac`: fraction of those frames (resp. of
//!   the window's time, each frame weighted by the gap to the next frame)
//!   whose `mode == "h264"`. `mode_flips`: `mode decision` events with a
//!   `frame_seq` inside the window's frame range.
//! * `ssim_during_*`: every `shot_interval` a Chromium screenshot of the
//!   canvas is compared with reference renders. The screenshot lags the source
//!   (capture + network + decode + screenshot latency), so the score is the
//!   BEST SSIM over a window of candidate scroll offsets
//!   (`[k_expected-300, k_expected+15]` frames; candidates are ranked by a cheap
//!   pixel MAE and the top 3 get a full SSIM). This measures how sharp/blurry
//!   the displayed text is, not how late it is. The winning lag is reported
//!   separately as `lag_frames_*`.
//! * `ssim_post1s`: SSIM of the first screenshot taken >= 1 s after `stop`
//!   against the final static frame (no offset search).
//! * `recovery_exact_s`: seconds from `stop` to the first screenshot whose RGB
//!   is byte-identical to the final frame (PixelPerfect, as seen by the user);
//!   `recovery_ssim999_s`: same with SSIM >= 0.999. Resolution is the poll period (screenshot latency,
//!   `poll_period_ms_median`). Not reached within the cap => `null` and
//!   `recovered: false`.
//! * `pixelperfect_lines`: whole-run count of the server's
//!   `cdf53.pixelperfect` log lines (the log lines carry no monotonic
//!   timestamp, so this cannot be windowed).
//!
//! Premises asserted (the run errors if they fail): the browser connected,
//! telemetry is non-empty, the container logged both scroll events, and
//! (scrolling runs only) the scroll window held >= `MIN_SCROLL_FRAMES` frames.

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use image::RgbImage;
use serde_json::{json, Value};

use super::scene::{
    launch_scenario_stack, monotonic_now_ns, parse_server_telemetry, FrameMode, SceneSpec,
    ServerTelemetryRecord,
};

/// A scrolling run whose window holds fewer telemetry frames than this did
/// not really scroll (or the telemetry is broken).
pub const MIN_SCROLL_FRAMES: usize = 30;
const GO_FILE: &str = "/tmp/ghostframe-scroll-go";
/// Candidate-offset window around the expected scroll position. The browser
/// can lag the source by seconds on a constrained link, so the search reaches
/// back as far as the whole 10 s scroll.
const CANDIDATES_BEHIND: i64 = 300;
const CANDIDATES_AHEAD: i64 = 15;

/// `(width, height, offset_rows) -> tightly packed BGRX frame`.
pub type RefRender<'a> = &'a (dyn Fn(u32, u32, u64) -> Vec<u8> + Sync);

#[derive(Clone, Debug)]
pub struct ScrollRunSpec {
    pub label: String,
    /// 0 = static control.
    pub rows_per_frame: u32,
    pub scroll_secs: u64,
    pub fps: u32,
    pub bandwidth_kbps: u32,
    pub delay_ms: u32,
    pub loss_pct: u32,
    pub width: u32,
    pub height: u32,
    pub recovery_cap: Duration,
    pub shot_interval: Duration,
}

struct Shot {
    img: RgbImage,
    mid_ns: u64,
}

async fn grab(page: &chromiumoxide::Page, w: u32, h: u32) -> Result<Shot> {
    use chromiumoxide::cdp::browser_protocol::page::{CaptureScreenshotFormat, Viewport};
    let params = chromiumoxide::page::ScreenshotParams::builder()
        .format(CaptureScreenshotFormat::Png)
        .clip(
            Viewport::builder()
                .x(0)
                .y(0)
                .width(w as f64)
                .height(h as f64)
                .scale(1)
                .build()
                .map_err(|e| anyhow!("clip: {e}"))?,
        )
        .capture_beyond_viewport(true)
        .build();
    let t0 = monotonic_now_ns();
    let png = tokio::time::timeout(Duration::from_secs(25), page.screenshot(params))
        .await
        .map_err(|_| anyhow!("screenshot timed out"))??;
    let t1 = monotonic_now_ns();
    let img = image::load_from_memory(&png)
        .context("decode screenshot")?
        .to_rgb8();
    if img.width() < w || img.height() < h {
        bail!(
            "screenshot {}x{} smaller than {w}x{h}",
            img.width(),
            img.height()
        );
    }
    let img = if img.width() != w || img.height() != h {
        image::imageops::crop_imm(&img, 0, 0, w, h).to_image()
    } else {
        img
    };
    Ok(Shot {
        img,
        mid_ns: (t0 + t1) / 2,
    })
}

/// BGRX rows `[row0, row0+h)` of a tight `w`-wide strip -> RGB image.
fn bgrx_to_rgb(strip: &[u8], w: u32, row0: u32, h: u32) -> RgbImage {
    let mut out = RgbImage::new(w, h);
    for y in 0..h {
        let src = &strip[((row0 + y) * w * 4) as usize..((row0 + y + 1) * w * 4) as usize];
        for x in 0..w {
            let p = &src[x as usize * 4..x as usize * 4 + 4];
            out.put_pixel(x, y, image::Rgb([p[2], p[1], p[0]]));
        }
    }
    out
}

fn mismatched_pixels(a: &RgbImage, b: &RgbImage) -> u64 {
    a.as_raw()
        .chunks_exact(3)
        .zip(b.as_raw().chunks_exact(3))
        .filter(|(x, y)| x != y)
        .count() as u64
}

/// `[x0, y0, x1, y1]` of the mismatching pixels, if any.
fn mismatch_bbox(a: &RgbImage, b: &RgbImage) -> Option<[u32; 4]> {
    let mut bb: Option<[u32; 4]> = None;
    for (x, y, p) in a.enumerate_pixels() {
        if p != b.get_pixel(x, y) {
            bb = Some(match bb {
                None => [x, y, x, y],
                Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
            });
        }
    }
    bb
}

fn ssim(a: &RgbImage, b: &RgbImage) -> f64 {
    image_compare::rgb_hybrid_compare(a, b)
        .map(|s| s.score)
        .unwrap_or(f64::NAN)
}

/// Cheap mean-abs-difference between the screenshot and a BGRX strip window,
/// on a 2x2-subsampled grid.
fn mae(shot: &RgbImage, strip: &[u8], w: u32, row0: u32, h: u32) -> f64 {
    let raw = shot.as_raw();
    let (mut sum, mut n) = (0u64, 0u64);
    for y in (0..h).step_by(2) {
        for x in (0..w).step_by(2) {
            let s = ((y * w + x) * 3) as usize;
            let r = (((row0 + y) * w + x) * 4) as usize;
            sum += (raw[s] as i32 - strip[r + 2] as i32).unsigned_abs() as u64
                + (raw[s + 1] as i32 - strip[r + 1] as i32).unsigned_abs() as u64
                + (raw[s + 2] as i32 - strip[r] as i32).unsigned_abs() as u64;
            n += 1;
        }
    }
    sum as f64 / (n * 3) as f64
}

pub fn median(v: &[f64]) -> Option<f64> {
    let mut v: Vec<f64> = v.iter().copied().filter(|x| x.is_finite()).collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    Some(v[v.len() / 2])
}

fn scroll_event(logs: &str, phase: &str) -> Option<(u64, String)> {
    let tag = format!("scroll-event phase={phase} ");
    let line = logs.lines().find(|l| l.contains(&tag))?;
    let ns = line
        .split("mono_ns=")
        .nth(1)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some((ns, line.to_string()))
}

struct DuringSample {
    t_rel_s: f64,
    best_ssim: f64,
    best_lag_frames: i64,
    best_mae: f64,
}

fn analyse_during(
    shot: &Shot,
    spec: &ScrollRunSpec,
    start_ns: u64,
    render: RefRender<'_>,
) -> DuringSample {
    let (w, h) = (spec.width, spec.height);
    let frames = spec.scroll_secs * spec.fps as u64;
    let period_ns = 1e9 / spec.fps as f64;
    let k_exp = ((shot.mid_ns as f64 - start_ns as f64) / period_ns).floor() as i64;
    let rows = spec.rows_per_frame as u64;
    let (k_lo, k_hi) = if rows == 0 {
        (0, 0)
    } else {
        (
            (k_exp - CANDIDATES_BEHIND).clamp(0, frames as i64),
            (k_exp + CANDIDATES_AHEAD).clamp(0, frames as i64),
        )
    };
    // One tall strip covers every candidate: the document is procedural, so
    // frame(o) is rows [o-o_lo, o-o_lo+h) of the strip rendered at o_lo.
    let o_lo = k_lo as u64 * rows;
    let span = (k_hi - k_lo) as u64 * rows;
    let strip = render(w, h + span as u32, o_lo);
    let mut ranked: Vec<(i64, f64)> = (k_lo..=k_hi)
        .map(|k| {
            let row0 = ((k - k_lo) as u64 * rows) as u32;
            (k, mae(&shot.img, &strip, w, row0, h))
        })
        .collect();
    ranked.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap());
    let mut best = (f64::NEG_INFINITY, 0i64, ranked[0].1);
    for &(k, m) in ranked.iter().take(3) {
        let row0 = ((k - k_lo) as u64 * rows) as u32;
        let s = ssim(&shot.img, &bgrx_to_rgb(&strip, w, row0, h));
        if s > best.0 {
            best = (s, k_exp - k, m);
        }
    }
    DuringSample {
        t_rel_s: (shot.mid_ns as f64 - start_ns as f64) / 1e9,
        best_ssim: best.0,
        best_lag_frames: best.1,
        best_mae: best.2,
    }
}

pub async fn run_scroll_run(spec: &ScrollRunSpec, render: RefRender<'_>) -> Result<Value> {
    let (w, h) = (spec.width, spec.height);
    // Environment consumed by the container entrypoint / the page. CDF53 and
    // 30 fps are what produce the PixelPerfect signal (see codec_report).
    std::env::set_var("GHOSTFRAME_ENABLE_CDF53", "1");
    std::env::set_var("CAPTURE_FPS", spec.fps.to_string());
    std::env::set_var("GHOSTFRAME_DRM_MODE", format!("{w}x{h}"));
    std::env::set_var("GHOSTFRAME_SCENE_PAGE_QUERY", "e2e=lossless");
    std::env::set_var("SHAPE_BANDWIDTH_KBPS", spec.bandwidth_kbps.to_string());
    std::env::set_var("SHAPE_DELAY_MS", spec.delay_ms.to_string());
    std::env::set_var("SHAPE_LOSS_PCT", spec.loss_pct.to_string());

    let scene = SceneSpec {
        name: "scroll_text",
        test_pattern_args: [
            "--x11-sink",
            "--scroll-text",
            "--scroll-rows-per-frame",
            &spec.rows_per_frame.to_string(),
            "--scroll-secs",
            &spec.scroll_secs.to_string(),
            "--scroll-fps",
            &spec.fps.to_string(),
            "--scroll-go-file",
            GO_FILE,
        ]
        .iter()
        .map(|s| s.to_string())
        .collect(),
        duration: Duration::ZERO,
        initial_mode: FrameMode::TileCodec,
    };
    let loadavg_start = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let stack = launch_scenario_stack(&scene).await?;
    let page = &stack.page;
    let container = stack.server_container_name.clone();
    let frames = spec.scroll_secs * spec.fps as u64;
    let final_offset = frames * spec.rows_per_frame as u64;

    // ---- pre-scroll: wait for the client to show frame 0 exactly. This also
    // validates the reference renderer against the real pipeline.
    let strip0 = render(w, h, 0);
    let ref0 = bgrx_to_rgb(&strip0, w, 0, h);
    let pre_deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    let (mut pre_exact, mut pre_ssim, mut pre_mismatch) = (false, f64::NAN, u64::MAX);
    let mut pre_bbox: Option<[u32; 4]> = None;
    let mut shot_errors = 0u32;
    while tokio::time::Instant::now() < pre_deadline {
        match grab(page, w, h).await {
            Ok(s) => {
                pre_mismatch = mismatched_pixels(&s.img, &ref0);
                pre_ssim = ssim(&s.img, &ref0);
                pre_bbox = mismatch_bbox(&s.img, &ref0);
                if let Ok(dir) = std::env::var("GHOSTFRAME_SCROLL_DUMP_DIR") {
                    let _ = s.img.save(format!("{dir}/pre_shot.png"));
                    let _ = ref0.save(format!("{dir}/pre_ref.png"));
                }
                if pre_mismatch == 0 {
                    pre_exact = true;
                    break;
                }
            }
            Err(_) => shot_errors += 1,
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }

    // ---- go.
    let go_t0 = monotonic_now_ns();
    let touch = tokio::process::Command::new("docker")
        .args(["exec", &container, "touch", GO_FILE])
        .output()
        .await?;
    if !touch.status.success() {
        bail!(
            "touch go-file failed: {}",
            String::from_utf8_lossy(&touch.stderr)
        );
    }
    let go_t1 = monotonic_now_ns();
    // Best a-priori estimate of when scrolling began (refined by the log).
    let start_est = (go_t0 + go_t1) / 2;
    let stop_est = start_est + spec.scroll_secs * 1_000_000_000;

    // ---- shots during the scroll.
    let mut during_shots: Vec<Shot> = Vec::new();
    while monotonic_now_ns() + 300_000_000 < stop_est {
        match grab(page, w, h).await {
            Ok(s) => during_shots.push(s),
            Err(_) => shot_errors += 1,
        }
        tokio::time::sleep(spec.shot_interval).await;
    }

    // ---- wait for the stop instant, then poll for recovery. The final frame
    // is the one at `final_offset` (== frame 0 for the static control).
    while monotonic_now_ns() < stop_est {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let final_strip = render(w, h, final_offset);
    let ref_final = bgrx_to_rgb(&final_strip, w, 0, h);
    struct Poll {
        t_ns: u64,
        mismatch: u64,
        ssim: Option<tokio::task::JoinHandle<f64>>,
    }
    let ref_final = std::sync::Arc::new(ref_final);
    let mut polls: Vec<Poll> = Vec::new();
    let poll_end = stop_est + spec.recovery_cap.as_nanos() as u64;
    while monotonic_now_ns() < poll_end {
        match grab(page, w, h).await {
            Ok(s) => {
                let mismatch = mismatched_pixels(&s.img, &ref_final);
                let exact = mismatch == 0;
                // SSIM runs off-thread so the poll period stays the
                // screenshot latency, not screenshot + SSIM.
                let ssim = if exact {
                    None
                } else {
                    let r = ref_final.clone();
                    let img = s.img;
                    Some(tokio::task::spawn_blocking(move || ssim(&img, &r)))
                };
                polls.push(Poll {
                    t_ns: s.mid_ns,
                    mismatch,
                    ssim,
                });
                // Keep going until we have both the 1 s sample and recovery.
                let have_1s = polls.iter().any(|p| p.t_ns >= stop_est + 1_000_000_000);
                if exact && have_1s {
                    break;
                }
            }
            Err(_) => shot_errors += 1,
        }
    }
    let loadavg_end = std::fs::read_to_string("/proc/loadavg").unwrap_or_default();
    let mut poll_ssims: Vec<f64> = Vec::new();
    for p in polls.iter_mut() {
        poll_ssims.push(match p.ssim.take() {
            Some(h) => h.await.unwrap_or(f64::NAN),
            None => 1.0,
        });
    }

    // ---- logs -> timeline + telemetry.
    let logs = super::cleanup::read_server_logs_stripped(&container);
    let (start_ev, start_line) = scroll_event(&logs, "start").ok_or_else(|| {
        let excerpt: Vec<&str> = logs
            .lines()
            .filter(|l| l.contains("scroll") || l.contains("test-pattern"))
            .take(8)
            .collect();
        anyhow!(
            "container never logged scroll-event phase=start (is the ghostframe/test-server \
             image built from this tree? it is host-global); log bytes {}; lines: {:?}",
            logs.len(),
            excerpt
        )
    })?;
    let (stop_ev, stop_line) = scroll_event(&logs, "stop")
        .ok_or_else(|| anyhow!("container never logged scroll-event phase=stop"))?;
    let (telemetry, decisions, policy) = parse_server_telemetry(&logs);
    if telemetry.is_empty() {
        bail!("no server telemetry (client never received frames?)");
    }
    let in_win: Vec<&ServerTelemetryRecord> = telemetry
        .iter()
        .filter(|r| r.capture_done_ns >= start_ev && r.capture_done_ns <= stop_ev)
        .collect();
    if spec.rows_per_frame > 0 && in_win.len() < MIN_SCROLL_FRAMES {
        bail!(
            "scroll window held {} telemetry frames (< {MIN_SCROLL_FRAMES}); did it scroll?",
            in_win.len()
        );
    }
    let win_s = (stop_ev - start_ev) as f64 / 1e9;
    let bytes: u64 = in_win.iter().map(|r| r.total_wire_bytes as u64).sum();
    let h264_frames = in_win.iter().filter(|r| r.mode == "h264").count();
    let mut h264_time = 0f64;
    let mut total_time = 0f64;
    for (i, r) in in_win.iter().enumerate() {
        let next = in_win
            .get(i + 1)
            .map(|n| n.capture_done_ns)
            .unwrap_or(stop_ev);
        let dt = next.saturating_sub(r.capture_done_ns) as f64;
        total_time += dt;
        if r.mode == "h264" {
            h264_time += dt;
        }
    }
    let (seq_lo, seq_hi) = (
        in_win.first().map(|r| r.frame_seq),
        in_win.last().map(|r| r.frame_seq),
    );
    let flips = match (seq_lo, seq_hi) {
        (Some(lo), Some(hi)) => decisions
            .iter()
            .filter(|d| d.frame_seq >= lo && d.frame_seq <= hi)
            .count(),
        _ => 0,
    };
    // Bytes in the first 5 s after the stop (refinement / settling traffic).
    let post_bytes: u64 = telemetry
        .iter()
        .filter(|r| r.capture_done_ns > stop_ev && r.capture_done_ns <= stop_ev + 5_000_000_000)
        .map(|r| r.total_wire_bytes as u64)
        .sum();

    // ---- screenshot analysis against the true start.
    let during: Vec<DuringSample> = during_shots
        .iter()
        .filter(|s| s.mid_ns >= start_ev && s.mid_ns <= stop_ev)
        .map(|s| analyse_during(s, spec, start_ev, render))
        .collect();
    let during_ssim: Vec<f64> = during.iter().map(|d| d.best_ssim).collect();
    let during_lag: Vec<f64> = during.iter().map(|d| d.best_lag_frames as f64).collect();

    let rel = |t: u64| (t as f64 - stop_ev as f64) / 1e9;
    let post1 = polls.iter().find(|p| p.t_ns >= stop_ev + 1_000_000_000);
    let post1_idx = polls.iter().position(|p| p.t_ns >= stop_ev + 1_000_000_000);
    let ssim_post1s = post1_idx.map(|i| poll_ssims[i]);
    let recovery_exact = polls.iter().find(|p| p.mismatch == 0).map(|p| rel(p.t_ns));
    let recovery_999 = polls
        .iter()
        .zip(&poll_ssims)
        .find(|(_, s)| **s >= 0.999)
        .map(|(p, _)| rel(p.t_ns));
    let poll_periods: Vec<f64> = polls
        .windows(2)
        .map(|w| (w[1].t_ns - w[0].t_ns) as f64 / 1e6)
        .collect();
    let post1_mismatch_frac = post1.map(|p| p.mismatch as f64 / (w as f64 * h as f64));

    Ok(json!({
        "label": spec.label,
        "rows_per_frame": spec.rows_per_frame,
        "kbps": spec.bandwidth_kbps,
        "delay_ms": spec.delay_ms,
        "loss_pct": spec.loss_pct,
        "screen": format!("{w}x{h}"),
        "window_s": win_s,
        "frames_in_window": in_win.len(),
        "wire_bytes_in_window": bytes,
        "wire_bytes_per_s_scroll": bytes as f64 / win_s,
        "wire_bytes_5s_after_stop": post_bytes,
        "h264_frame_frac": (!in_win.is_empty()).then(|| h264_frames as f64 / in_win.len() as f64),
        "h264_time_frac": (total_time > 0.0).then(|| h264_time / total_time),
        "mode_flips": flips,
        "ssim_during_median": median(&during_ssim),
        "ssim_during_min": during_ssim.iter().cloned().fold(f64::INFINITY, f64::min).is_finite().then(|| during_ssim.iter().cloned().fold(f64::INFINITY, f64::min)),
        "ssim_during_n": during_ssim.len(),
        "ssim_during_all": during.iter().map(|d| json!({"t": d.t_rel_s, "ssim": d.best_ssim, "lag_frames": d.best_lag_frames, "mae": d.best_mae})).collect::<Vec<_>>(),
        "lag_frames_median": median(&during_lag),
        "ssim_post1s": ssim_post1s,
        "post1s_mismatch_frac": post1_mismatch_frac,
        "recovered": recovery_exact.is_some(),
        "recovery_exact_s": recovery_exact,
        "recovery_ssim999_s": recovery_999,
        "poll_period_ms_median": median(&poll_periods),
        "polls": polls.len(),
        "pixelperfect_lines": policy.pixelperfect_count,
        "decode_errors": policy.decode_error_count,
        "pre_scroll_exact": pre_exact,
        "pre_scroll_ssim": pre_ssim,
        "pre_scroll_mismatch_px": if pre_mismatch == u64::MAX { Value::Null } else { json!(pre_mismatch) },
        "pre_scroll_mismatch_bbox": pre_bbox,
        "loadavg_start": loadavg_start.trim(),
        "loadavg_end": loadavg_end.trim(),
        "screenshot_errors": shot_errors,
        "start_line": start_line,
        "stop_line": stop_line,
    }))
}
