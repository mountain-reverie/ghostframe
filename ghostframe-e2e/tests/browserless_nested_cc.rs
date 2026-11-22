// SPDX-License-Identifier: AGPL-3.0-or-later
//! Measurement scenes for the "nested congestion control" hypothesis
//! (docs/reviews/2026-10-07-protocol-comparative-review.md section 4.2 and
//! section 5 item 2): the server's goog_cc bandwidth estimator is layered on
//! quinn's Cubic controller, and may (a) converge to whatever queue Cubic
//! maintains, (b) read submit rather than wire-departure timestamps at cwnd
//! exhaustion, (c) have its probes clipped by cwnd, and oscillate across
//! slow-start-after-idle and probe-during-Cubic-growth.
//!
//! # These are instruments, not gates
//!
//! Scenes REPORT through `println!` lines of the form
//! `METRIC <scene> <name> <value>` and assert **premises only**: the session
//! ran, the estimator was fed, the series is long enough, the estimate moved
//! (a dead estimator pinned at its seed cannot report "converged"), and, for
//! the idle scene, that the scene script was well formed. No timing
//! threshold is asserted. A later change (exposing wire-departure timestamps
//! to the BWE) is compared by re-running this binary and diffing the METRIC
//! lines.
//!
//! The metric definitions live in `ghostframe_e2e::harness::cc_probe`, as
//! pure functions with unit tests over synthetic series.
//!
//! Run: `cargo test --test browserless_nested_cc -- --nocapture --test-threads=1`
//!
//! The harness is not seed-reproducible end to end, so loop a scene and look
//! at the spread rather than trusting one run.

use ghostframe_e2e::harness::browserless::{
    run_browserless, BrowserlessResult, BrowserlessScene, FrameScript, SceneLoad,
};
use ghostframe_e2e::harness::cc_probe::{self as cc, BAND, REVERSAL_THRESHOLD};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, CoDel, NetProfile};
use std::time::Duration;

const S: u64 = 1_000_000;
const MBIT: u64 = 125_000; // bytes/s per Mbit/s

/// Heavy enough to exceed every capacity used here (the BWE paces the
/// emission, so offered load above the link is what makes the link bind).
fn heavy(offered_mbit: f64) -> LoadProfile {
    LoadProfile {
        cadence_us: PRODUCTION_CADENCE_US,
        churn: Churn::Region {
            tiles_per_tick: tiles_for(offered_mbit),
        },
    }
}

/// Tiles per 33.3 ms tick that offer about `offered_mbit`. Slope measured by
/// `calibrate_offered_load` (2026-10-10, uncapped link): ~0.30 Mbit/s per
/// tile-per-tick (8 -> 2.5, 32 -> 9.7, 64 -> 19.2, 160 -> 47 Mbit/s).
/// `GF_CC_TILES` overrides it.
fn tiles_for(offered_mbit: f64) -> usize {
    std::env::var("GF_CC_TILES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| (offered_mbit / 0.30).ceil() as usize)
}

/// Offered load as a multiple of capacity: enough to bind the link, little
/// enough that the emitter backlog can drain.
const OVERDRIVE: f64 = 1.3;

#[derive(Clone, Copy)]
enum Queue {
    DropTail100,
    /// A shallow buffer: it overflows at a standing queue of 25 ms, so a
    /// sender above the link rate loses packets to it within a few frames.
    DropTail25,
    /// Shallower still: at 5 or 10 ms the sender's own bursts overflow it
    /// below the link rate, so a steady loss of a few percent is the buffer's.
    DropTail10,
    DropTail5,
    CoDel,
}

fn bottleneck(q: Queue) -> Bottleneck {
    match q {
        Queue::DropTail100 => Bottleneck {
            depth_ms: 100,
            aqm: None,
        },
        Queue::DropTail25 => Bottleneck {
            depth_ms: 25,
            aqm: None,
        },
        Queue::DropTail10 => Bottleneck {
            depth_ms: 10,
            aqm: None,
        },
        Queue::DropTail5 => Bottleneck {
            depth_ms: 5,
            aqm: None,
        },
        Queue::CoDel => Bottleneck {
            depth_ms: 100,
            aqm: Some(CoDel::rfc_defaults()),
        },
    }
}

/// `idle`: `[start_us, end_us)` ranges (measured from the first injected
/// frame) whose frames are emptied. An empty `FrameScript` still ticks the
/// scene at the production cadence but dirties no tile.
async fn run(
    seed: u64,
    cap: CapTimeline,
    q: Queue,
    secs: u64,
    idle: &[(u64, u64)],
    offered_mbit: f64,
) -> BrowserlessResult {
    let profile = heavy(offered_mbit);
    let mut frames: Vec<FrameScript> = profile.frames_for(Duration::from_secs(secs), 16, 16);
    for (i, f) in frames.iter_mut().enumerate() {
        let t = i as u64 * PRODUCTION_CADENCE_US;
        if idle.iter().any(|(a, b)| t >= *a && t < *b) {
            f.tiles.clear();
        }
    }
    let scene = BrowserlessScene {
        seed,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(frames),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            cap,
            bottleneck: Some(bottleneck(q)),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: 16,
        grid_rows: 16,
    };
    run_browserless(scene).await.expect("scene ran")
}

fn fmt_opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("none".to_string(), |x| x.to_string())
}

fn ms(v: Option<u64>) -> String {
    fmt_opt(v.map(|x| x / 1000))
}

fn metric(scene: &str, name: &str, val: impl std::fmt::Display) {
    println!("METRIC {scene} {name} {val}");
}

/// Premises every scene must satisfy before its numbers mean anything.
fn assert_premises(r: &BrowserlessResult, scene: &str, min_points: usize) {
    assert!(r.bwe_samples_seen > 0, "{scene}: estimator never fed");
    assert!(
        r.bwe_estimate_samples.len() >= min_points,
        "{scene}: only {} series points",
        r.bwe_estimate_samples.len()
    );
    assert!(
        cc::estimate_moved(&r.bwe_estimate_samples),
        "{scene}: estimate never moved off its first value; a dead estimator \
         cannot be said to converge"
    );
    assert!(
        r.bytes_delivered_s2c > 0,
        "{scene}: no server->client bytes; the session did not carry load"
    );
    // The delivery-rate sampler (D1 PR-6b) was fed, and its series is as long
    // as the estimate's: a sampler that never fired reads as zero everywhere.
    let d = &r.transport.instr.delivery;
    assert!(
        d.samples > 0 && d.marked > 0,
        "{scene}: delivery-rate sampler never produced a sample"
    );
    assert_eq!(
        r.delivery_rate_samples.len(),
        r.bwe_estimate_samples.len(),
        "{scene}: delivery series sampled at other instants than the estimate"
    );
}

/// The delivery-rate sampler against the link and against goog_cc, over
/// `[from_us, to_us)`: the median of its windowed maximum, as a fraction of
/// `cap_bps` and of the median estimate. Report only (D1 PR-6b feeds PR-7).
fn report_delivery(
    scene: &str,
    label: &str,
    r: &BrowserlessResult,
    cap_bps: f64,
    from_us: u64,
    to_us: u64,
) {
    let est = cc::median_bps(&r.bwe_estimate_samples, from_us, to_us);
    // The payload fraction last measured: the share of link bytes that are
    // ledger-tracked datagram bytes. The sampler is not scaled by it; the
    // `/payload` columns are for comparing with a link capacity.
    let frac = r.transport.instr.bwe.payload_fraction_ppm as f64 / 1e6;
    for (kind, series) in [
        ("max", &r.delivery_rate_samples),
        ("unpaced_max", &r.delivery_rate_unpaced_samples),
    ] {
        let med = cc::median_bps(series, from_us, to_us);
        let peak = cc::window(series, from_us, to_us)
            .iter()
            .map(|(_, v)| *v)
            .max();
        metric(
            scene,
            &format!("delivery_{label}_{kind}_median_bps"),
            fmt_opt(med),
        );
        metric(
            scene,
            &format!("delivery_{label}_{kind}_peak_bps"),
            fmt_opt(peak),
        );
        metric(
            scene,
            &format!("delivery_{label}_{kind}_over_cap"),
            fmt_opt(med.map(|m| format!("{:.3}", m as f64 / cap_bps))),
        );
        metric(
            scene,
            &format!("delivery_{label}_{kind}_over_payload_over_cap"),
            fmt_opt(
                med.filter(|_| frac > 0.0)
                    .map(|m| format!("{:.3}", m as f64 / frac / cap_bps)),
            ),
        );
        metric(
            scene,
            &format!("delivery_{label}_{kind}_over_est"),
            fmt_opt(
                med.zip(est)
                    .map(|(m, e)| format!("{:.3}", m as f64 / e as f64)),
            ),
        );
    }
}

/// The D1 transport instruments for this scene, one METRIC line each: the
/// FIFO, the departure lag, and the ledger's verdicts against the real drops.
/// `expired_before_departure` is reported here only; the P3 gate is
/// `assert_p3` in `browserless_burst_loss` (this scene's FIFO never holds
/// enough for the counter to be able to move).
fn report_d1(scene: &str, r: &BrowserlessResult) {
    let i = &r.transport.instr;
    metric(scene, "d1_fifo_max_B", i.fifo_depth_max);
    metric(scene, "d1_fifo_mean_B", i.fifo_depth_mean);
    metric(
        scene,
        "d1_depart_lag_us",
        format!(
            "n={} p50={} p95={} max={}",
            i.lag_samples, i.lag_p50_us, i.lag_p95_us, i.lag_max_us
        ),
    );
    metric(
        scene,
        "d1_admission",
        format!(
            "d_target_max_B={} depth_violations={} gate_stops[depth={} tokens={}] pacer_term={}",
            i.admission.d_target_max,
            i.admission.depth_violations,
            i.budget.gate_depth,
            i.budget.gate_tokens,
            i.budget.pacer
        ),
    );
    metric(scene, "d1_departures_stamped", i.ledger_departures_stamped);
    metric(
        scene,
        "d1_expired_before_departure",
        i.ledger_expired_before_departure,
    );
    metric(
        scene,
        "d1_evicted_before_departure",
        i.ledger_evicted_before_departure,
    );
    metric(
        scene,
        "d1_loss_path",
        format!(
            "verdicts[packet={} time={} pto={}] k_pkt={} spurious={} retx[loss={} probe={}] pto_fires={} verdict_no_retx[gone={} newer={}]",
            i.loss_by_packet_threshold,
            i.loss_by_time_threshold,
            i.loss_by_pto,
            i.k_pkt,
            i.ledger_spurious_losses,
            i.loss_retransmits,
            i.pto_probe_retransmits,
            i.pto_fires,
            i.verdict_content_gone,
            i.verdict_newer_in_flight
        ),
    );
    metric(
        scene,
        "d1_ledger_losses",
        format!(
            "declared={} flushed={} cap_trimmed={} real_drops={}",
            i.ledger_losses_declared,
            i.ledger_losses_flushed,
            i.ledger_losses_overflowed,
            r.s2c_datagrams_dropped
        ),
    );
    metric(
        scene,
        "d1_rto",
        format!(
            "fired={} spurious={} useful={}",
            r.loss_path_resends, r.loss_path_resends_spurious, r.loss_path_resends_useful
        ),
    );
}

/// Everything that is not a timing metric: queue/latency/probe context so a
/// reader can see whether the estimate tracks the queue rather than the link.
fn report_context(scene: &str, r: &BrowserlessResult, cap_bps: f64, from_us: u64, to_us: u64) {
    let s = &r.bwe_estimate_samples;
    metric(scene, "final_est_bps", r.bwe_estimate_bps);
    metric(scene, "pacer_rate_bps", fmt_opt(r.pacer_rate_bps));
    metric(scene, "probes_completed", r.probes_completed);
    metric(scene, "probes_abandoned", r.probes_abandoned);
    metric(scene, "bytes_s2c", r.bytes_delivered_s2c);
    metric(scene, "bytes_dropped", r.bytes_dropped);
    metric(scene, "retransmit_attempts", r.retransmit_attempts_total);
    metric(scene, "send_datagram_errs", r.send_datagram_errs);
    metric(scene, "emission_queue_peak", r.emission_queue_peak);
    metric(scene, "critical_lat_n", r.critical_latency_count);
    metric(scene, "critical_lat_mean_us", r.critical_latency_mean_us);
    metric(scene, "critical_lat_max_us", r.critical_latency_max_us);
    metric(
        scene,
        "critical_lat_buckets",
        format!("{:?}", r.critical_latency_buckets),
    );
    metric(
        scene,
        "queued_crit_lat_mean_us",
        r.queued_critical_latency_mean_us,
    );
    metric(scene, "implausible_rtt", r.implausible_rtt_samples);
    report_d1(scene, r);
    let d = &r.transport.instr.delivery;
    metric(
        scene,
        "delivery_counts",
        format!(
            "marked={} app_limited_marks={} acked={} samples={} app_limited_samples={} pacer_limited_samples={} latest_bps={} latest_app_limited={}",
            d.marked,
            d.app_limited_marks,
            d.acked,
            d.samples,
            d.app_limited_samples,
            d.pacer_limited_samples,
            d.latest_bps,
            d.latest_app_limited
        ),
    );
    metric(
        scene,
        "delivery_vs_netsim_s2c",
        format!(
            "payload_fraction_ppm={} delivered_bytes={} netsim_s2c_bytes={} ratio={:.3}",
            r.transport.instr.bwe.payload_fraction_ppm,
            d.delivered_bytes,
            r.bytes_delivered_s2c,
            d.delivered_bytes as f64 / r.bytes_delivered_s2c.max(1) as f64
        ),
    );
    report_delivery(scene, "settled", r, cap_bps, from_us, to_us);
    metric(
        scene,
        "delivery_series_100ms",
        r.delivery_rate_samples
            .iter()
            .map(|(t, b)| format!("{}:{}", t / 1000, b / 1000))
            .collect::<Vec<_>>()
            .join(","),
    );
    if let Some(m) = cc::median_bps(s, from_us, to_us) {
        metric(scene, "settled_median_bps", m);
        metric(
            scene,
            "settled_over_cap",
            format!("{:.3}", m as f64 / cap_bps),
        );
    }
    metric(
        scene,
        "series_100ms",
        s.iter()
            .map(|(t, b)| format!("{}:{}", t / 1000, b / 1000))
            .collect::<Vec<_>>()
            .join(","),
    );
}

/// S1: cold start against a fixed-capacity bottleneck.
async fn slow_start(name: &str, cap_mbit: u64, q: Queue, seed: u64) {
    let cap_bps = (cap_mbit * 1_000_000) as f64;
    let secs = 20;
    let r = run(
        seed,
        CapTimeline::constant(cap_mbit * MBIT),
        q,
        secs,
        &[],
        OVERDRIVE * cap_mbit as f64,
    )
    .await;
    assert_premises(&r, name, 100);
    if name == "s1_c10_droptail" {
        // The 1.39 resends per loss seen on 2026-10-29 no longer reproduces:
        // nothing is dropped and nothing is retransmitted here.
        assert!(
            r.s2c_datagrams_dropped == 0 && r.retransmit_attempts_total == 0,
            "{name}: real_drops={} retransmit_attempts={}",
            r.s2c_datagrams_dropped,
            r.retransmit_attempts_total
        );
    }
    let s = &r.bwe_estimate_samples;
    let end = s.last().unwrap().0 + 1;
    metric(
        name,
        "t_reach_50pct_ms",
        ms(cc::time_to_first_reach(s, 0, cap_bps, 0.5)),
    );
    metric(
        name,
        "t_first_enter_band_ms",
        ms(cc::time_to_first_enter(s, 0, cap_bps, BAND)),
    );
    metric(
        name,
        "t_converge_ms",
        ms(cc::convergence_time(s, 0, end, cap_bps, BAND, 3 * S)),
    );
    let tail_from = end.saturating_sub(5 * S);
    metric(
        name,
        "reversals_last5s",
        cc::direction_reversals(s, tail_from, end, REVERSAL_THRESHOLD),
    );
    metric(
        name,
        "p2p_over_mean_last5s",
        fmt_opt(cc::peak_to_peak_over_mean(s, tail_from, end).map(|v| format!("{v:.3}"))),
    );
    report_context(name, &r, cap_bps, tail_from, end);
}

#[tokio::test(start_paused = true)]
async fn s1_slow_start_c10_droptail() {
    slow_start("s1_c10_droptail", 10, Queue::DropTail100, 0xCC01_0010).await;
}

#[tokio::test(start_paused = true)]
async fn s1_slow_start_c5_droptail() {
    slow_start("s1_c5_droptail", 5, Queue::DropTail100, 0xCC01_0005).await;
}

#[tokio::test(start_paused = true)]
async fn s1_slow_start_c20_droptail() {
    slow_start("s1_c20_droptail", 20, Queue::DropTail100, 0xCC01_0020).await;
}

#[tokio::test(start_paused = true)]
async fn s1_slow_start_c10_codel() {
    slow_start("s1_c10_codel", 10, Queue::CoDel, 0xCC01_1010).await;
}

/// S2: load, an idle gap, load again. `gap` of `None` is the control.
async fn idle_scene(name: &str, gap: Option<(u64, u64)>, seed: u64) {
    const C_MBIT: u64 = 10;
    let cap_bps = (C_MBIT * 1_000_000) as f64;
    let secs = 36;
    let idle: Vec<(u64, u64)> = gap.into_iter().collect();
    let r = run(
        seed,
        CapTimeline::constant(C_MBIT * MBIT),
        Queue::DropTail100,
        secs,
        &idle,
        OVERDRIVE * C_MBIT as f64,
    )
    .await;
    assert_premises(&r, name, 200);
    let s = &r.bwe_estimate_samples;
    let end = s.last().unwrap().0 + 1;
    // Pre-gap reference: median over the 4 s before the gap (or before the
    // nominal gap position in the control, for a like-for-like comparison).
    let (g0, g1) = gap.unwrap_or((14 * S, 22 * S));
    let reference = cc::median_bps(s, g0 - 4 * S, g0).expect("pre-gap samples") as f64;
    metric(name, "pre_gap_ref_bps", reference as u64);
    metric(
        name,
        "pre_gap_ref_over_cap",
        format!("{:.3}", reference / cap_bps),
    );
    metric(
        name,
        "dip_depth_gap_to_end",
        fmt_opt(cc::dip_depth(s, g0, end, reference).map(|v| format!("{v:.3}"))),
    );
    metric(
        name,
        "dip_depth_in_gap",
        fmt_opt(cc::dip_depth(s, g0, g1, reference).map(|v| format!("{v:.3}"))),
    );
    metric(
        name,
        "est_at_resume_bps",
        fmt_opt(s.iter().find(|(t, _)| *t >= g1).map(|(_, b)| *b)),
    );
    metric(
        name,
        "t_recover_to_ref_ms",
        ms(cc::recovery_time(s, g1, reference, BAND)),
    );
    // Strict version: back within the band *and* staying there.
    metric(
        name,
        "t_reconverge_to_ref_ms",
        ms(cc::convergence_time(s, g1, end, reference, BAND, 3 * S)),
    );
    metric(
        name,
        "t_reach_90pct_ref_ms",
        ms(cc::time_to_first_reach(s, g1, reference, 0.9)),
    );
    let tail_from = end.saturating_sub(5 * S);
    metric(
        name,
        "reversals_last5s",
        cc::direction_reversals(s, tail_from, end, REVERSAL_THRESHOLD),
    );
    report_context(name, &r, cap_bps, tail_from, end);
}

#[tokio::test(start_paused = true)]
async fn s2_slow_start_after_idle_c10() {
    idle_scene("s2_idle8s_c10", Some((14 * S, 22 * S)), 0xCC02_0001).await;
}

/// Control for S2: identical scene without the gap. Proves the dip /
/// recovery metrics move with the gap rather than being scene noise.
#[tokio::test(start_paused = true)]
async fn s2_control_no_idle_c10() {
    idle_scene("s2_control_c10", None, 0xCC02_0002).await;
}

// A cross-run "idle gap offers no load" premise test was removed: it subtracted
// byte totals from three separate (non-byte-reproducible) runs and asserted a
// threshold on the emitter's residual retransmit traffic, a known-bad quantity.
// The s2 scenes' gap is NOT wire-idle, so read their metrics as retransmit
// drain, not as slow-start-after-idle.

/// S3: sustained heavy load across a capacity step up and back down.
#[tokio::test(start_paused = true)]
async fn s3_probe_under_load_capacity_steps() {
    const LOW: u64 = 5;
    const HIGH: u64 = 15;
    const UP_AT: u64 = 10 * S;
    const DOWN_AT: u64 = 28 * S;
    let name = "s3_steps_5_15_5";
    let cap = CapTimeline {
        points: vec![(0, LOW * MBIT), (UP_AT, HIGH * MBIT), (DOWN_AT, LOW * MBIT)],
    };
    let r = run(
        0xCC03_0001,
        cap,
        Queue::DropTail100,
        42,
        &[],
        OVERDRIVE * HIGH as f64,
    )
    .await;
    assert_premises(&r, name, 300);
    let s = &r.bwe_estimate_samples;
    let end = s.last().unwrap().0 + 1;
    let low = (LOW * 1_000_000) as f64;
    let high = (HIGH * 1_000_000) as f64;

    metric(
        name,
        "t_converge_low_initial_ms",
        ms(cc::convergence_time(s, 0, UP_AT, low, BAND, 3 * S)),
    );
    // Following the step up: first enter and stay, within the high phase.
    metric(
        name,
        "t_up_reach_75pct_ms",
        ms(cc::time_to_first_reach(s, UP_AT, high, 0.75)),
    );
    metric(
        name,
        "t_up_enter_band_ms",
        ms(cc::time_to_first_enter(s, UP_AT, high, BAND)),
    );
    metric(
        name,
        "t_up_converge_ms",
        ms(cc::convergence_time(s, UP_AT, DOWN_AT, high, BAND, 3 * S)),
    );
    metric(
        name,
        "t_down_converge_ms",
        ms(cc::convergence_time(s, DOWN_AT, end, low, BAND, 3 * S)),
    );
    // Steady state: last 5 s of each plateau.
    for (label, from, to, cap) in [
        ("low1", UP_AT - 5 * S, UP_AT, low),
        ("high", DOWN_AT - 5 * S, DOWN_AT, high),
        ("low2", end - 5 * S, end, low),
    ] {
        metric(
            name,
            &format!("{label}_reversals_last5s"),
            cc::direction_reversals(s, from, to, REVERSAL_THRESHOLD),
        );
        metric(
            name,
            &format!("{label}_p2p_over_mean_last5s"),
            fmt_opt(cc::peak_to_peak_over_mean(s, from, to).map(|v| format!("{v:.3}"))),
        );
        metric(
            name,
            &format!("{label}_median_over_cap"),
            fmt_opt(cc::median_bps(s, from, to).map(|m| format!("{:.3}", m as f64 / cap))),
        );
        report_delivery(name, label, &r, cap, from, to);
    }
    report_context(name, &r, high, DOWN_AT - 5 * S, DOWN_AT);
}

/// The p95 of the queue wait of the datagrams offered in `[from_us, to_us)`,
/// ms: their transit time less the link's propagation delay (10 ms here).
fn queue_delay_p95_ms(r: &BrowserlessResult, from_us: u64, to_us: u64) -> Option<u64> {
    let mut v: Vec<u64> = r
        .s2c_transit_samples
        .iter()
        .filter(|(t, _)| *t >= from_us && *t < to_us)
        .map(|(_, d)| d.saturating_sub(10_000))
        .collect();
    if v.is_empty() {
        return None;
    }
    v.sort_unstable();
    Some(v[(v.len() * 95 / 100).min(v.len() - 1)] / 1000)
}

/// What a loss-aware estimator is for: a shallow tail-drop buffer whose
/// overflow is the congestion signal. Report-only for now. Per seed: the loss
/// the link inflicted, the queue it left, where the estimate settled against
/// the cap, and how long it took after a capacity step.
fn shallow_report(name: &str, r: &BrowserlessResult, cap_bps: f64, step_us: u64, end_us: u64) {
    let s = &r.bwe_estimate_samples;
    let settle_from = end_us.saturating_sub(8 * S);
    metric(name, "bytes_dropped", r.bytes_dropped);
    metric(
        name,
        "loss_rate_pct",
        format!(
            "{:.2}",
            100.0 * r.s2c_datagrams_dropped as f64 / r.s2c_datagrams_offered.max(1) as f64
        ),
    );
    metric(
        name,
        "queue_delay_p95_ms_whole_run",
        fmt_opt(queue_delay_p95_ms(r, 0, end_us)),
    );
    metric(
        name,
        "queue_delay_p95_ms_settled",
        fmt_opt(queue_delay_p95_ms(r, settle_from, end_us)),
    );
    metric(name, "final_est_bps", r.bwe_estimate_bps);
    if let Some(m) = cc::median_bps(s, settle_from, end_us) {
        metric(name, "settled_median_bps", m);
        metric(
            name,
            "settled_over_cap",
            format!("{:.3}", m as f64 / cap_bps),
        );
    }
    metric(
        name,
        "t_settle_after_step_ms",
        ms(cc::convergence_time(
            s,
            step_us,
            end_us,
            cap_bps,
            BAND,
            3 * S,
        )),
    );
    metric(name, "bytes_s2c", r.bytes_delivered_s2c);
    metric(name, "retransmit_attempts", r.retransmit_attempts_total);
}

/// S4a: a 25 ms tail-drop buffer and a capacity step from 15 down to 5 Mbit/s
/// held for 20 s, offered load above the high rate throughout. Three seeds.
/// Gates, on every seed: the estimate does not collapse (at least a quarter of
/// the low cap); the drop rate over the run stays under 8 % and the settled
/// estimate is at most the cap. Measured: this branch drops 1.7 to 2.1 % and
/// settles at 0.81 to 0.92 x the cap; a loss-blind master drops 16 to 18 % and
/// settles at 1.12 to 1.13 x, over both bounds. Everything else is reported.
#[tokio::test(start_paused = true)]
async fn s4_shallow_droptail_capacity_step_15_to_5() {
    const LOW: u64 = 5;
    const HIGH: u64 = 15;
    const STEP_AT: u64 = 10 * S;
    const END: u64 = 30 * S;
    let low = (LOW * 1_000_000) as f64;
    for (i, seed) in [0xCC04_0001u64, 0xCC04_0002, 0xCC04_0003]
        .into_iter()
        .enumerate()
    {
        let name = format!("s4a_shallow_step_15_5_seed{}", i + 1);
        let cap = CapTimeline {
            points: vec![(0, HIGH * MBIT), (STEP_AT, LOW * MBIT)],
        };
        let r = run(
            seed,
            cap,
            Queue::DropTail25,
            END / S,
            &[],
            OVERDRIVE * HIGH as f64,
        )
        .await;
        assert_premises(&r, &name, 200);
        let end = r.bwe_estimate_samples.last().unwrap().0 + 1;
        shallow_report(&name, &r, low, STEP_AT, end);
        let settled =
            cc::median_bps(&r.bwe_estimate_samples, end.saturating_sub(8 * S), end).unwrap();
        assert!(
            settled as f64 >= 0.25 * low,
            "{name}: the estimate settled at {settled} bit/s, under a quarter of the {low} cap"
        );
        let loss_pct =
            100.0 * r.s2c_datagrams_dropped as f64 / r.s2c_datagrams_offered.max(1) as f64;
        assert!(
            loss_pct < 8.0,
            "{name}: {loss_pct:.2} % of the datagrams were dropped (measured 1.7 to 2.1 %, a loss-blind master 16 to 18 %)"
        );
        assert!(
            settled as f64 <= low,
            "{name}: the estimate settled at {settled} bit/s, above the {low} cap ({:.3} x; measured 0.81 to 0.92, a loss-blind master 1.12)",
            settled as f64 / low
        );
    }
}

/// S4b: the steady overload case: a constant 8 Mbit/s behind the same 25 ms
/// buffer, 1.3 x offered, 30 s. Three seeds. Report-only apart from the
/// no-collapse floor: it does not discriminate (a loss-blind master passes any
/// bound this branch does), so no tighter gate is placed on it.
#[tokio::test(start_paused = true)]
async fn s4_shallow_droptail_steady_overload_c8() {
    const CAP: u64 = 8;
    const END: u64 = 30 * S;
    let cap_bps = (CAP * 1_000_000) as f64;
    for (i, seed) in [0xCC04_0011u64, 0xCC04_0012, 0xCC04_0013]
        .into_iter()
        .enumerate()
    {
        let name = format!("s4b_shallow_overload_c8_seed{}", i + 1);
        let r = run(
            seed,
            CapTimeline::constant(CAP * MBIT),
            Queue::DropTail25,
            END / S,
            &[],
            OVERDRIVE * CAP as f64,
        )
        .await;
        assert_premises(&r, &name, 200);
        let end = r.bwe_estimate_samples.last().unwrap().0 + 1;
        shallow_report(&name, &r, cap_bps, 0, end);
        let settled =
            cc::median_bps(&r.bwe_estimate_samples, end.saturating_sub(8 * S), end).unwrap();
        assert!(
            settled as f64 >= 0.25 * cap_bps,
            "{name}: the estimate settled at {settled} bit/s, under a quarter of the {cap_bps} cap"
        );
    }
}

/// Approximate loss over `[from_us, to_us)` of virtual time, in percent: the
/// bytes the server offered the link in that span against the bytes that came
/// out, counted as the delivered datagrams times the run's mean datagram size.
/// Both are attributed to the time the datagram was offered. `None` when
/// nothing was offered.
fn window_loss_pct(r: &BrowserlessResult, from_us: u64, to_us: u64) -> Option<f64> {
    use ghostframe_e2e::harness::browserless::OFFERED_BIN_US;
    let lo = (from_us / OFFERED_BIN_US) as usize;
    let hi = ((to_us / OFFERED_BIN_US) as usize).min(r.s2c_offered_bins.len());
    let offered: u64 = r
        .s2c_offered_bins
        .get(lo..hi)?
        .iter()
        .map(|&b| b as u64)
        .sum();
    if offered == 0 {
        return None;
    }
    let mean = r.bytes_delivered_s2c as f64 / r.datagrams_delivered_s2c.max(1) as f64;
    let delivered = r
        .s2c_transit_samples
        .iter()
        .filter(|(t, _)| *t >= from_us && *t < to_us)
        .count() as f64
        * mean;
    Some((100.0 * (1.0 - delivered / offered as f64)).max(0.0))
}

/// Restores the production setting of the inherent-loss guard when the scene
/// ends, panic or not.
struct GuardReset;
impl Drop for GuardReset {
    fn drop(&mut self) {
        ghostframe_lib::transport::bwe::set_inherent_loss_guard(false);
    }
}

/// S4c: steady congestion loss of 2 to 5 % on a shallow tail-drop buffer,
/// with goog_cc's inherent-loss guard off (production) against on. Report
/// only; asserts premises (the estimator ran, the link dropped) and nothing
/// about the numbers.
///
/// The concern: with `not_increase_if_inherent_loss_less_than_average_loss`
/// off, loss under about 5 % may be fitted as inherent, so an estimate above
/// the link could stay there while the loss passes 5.75 %; and the delay of the
/// client's confirmation may pin a loss on the wrong sending rate.
///
/// The sender's rate is the estimator's, so the loss cannot be set directly.
/// Two ways to put the sender in the regime, and the scene reports whether it
/// got there (`loss_pct_settled`) rather than assuming it:
///
/// * `step`: a 25 ms buffer behind a 10 Mbit/s link that steps down by 2, 3.5
///   or 5 % at 10 s. A sender that stays where it was loses that share.
/// * `shallow`: a constant 10 Mbit/s link behind a 10 or 5 ms buffer, which the
///   sender's own bursts overflow below the link rate.
///
/// Per run: the loss in the 10 s after the step (step cells), the loss over the
/// last 20 s, the settled estimate over the link (the post-step link for step
/// cells) and the settled queue delay. Three seeds per cell; each arm runs all
/// the cells before the other. Loss is `window_loss_pct`, an approximation.
///
/// Run on demand (about 15 minutes):
/// `cargo test --release --test browserless_nested_cc s4c -- --ignored --nocapture --test-threads=1`
#[tokio::test(start_paused = true)]
#[ignore = "report-only comparison, ~15 min; run on demand"]
async fn s4c_shallow_droptail_steady_congestion_loss_guard_off_vs_on() {
    const HIGH: u64 = 10;
    const STEP_AT: u64 = 10 * S;
    let _reset = GuardReset;
    // (cell, queue, link before the step, step time, link after, seconds)
    let mut cells: Vec<(String, Queue, u64, Option<u64>, f64, u64)> = Vec::new();
    for step_pct in [2.0f64, 3.5, 5.0] {
        let low_bps = HIGH as f64 * 1e6 * (1.0 - step_pct / 100.0);
        cells.push((
            format!("step{step_pct}pct"),
            Queue::DropTail25,
            HIGH * MBIT,
            Some(STEP_AT),
            low_bps,
            50,
        ));
    }
    for (n, q) in [
        ("shallow10ms", Queue::DropTail10),
        ("shallow5ms", Queue::DropTail5),
    ] {
        cells.push((n.to_string(), q, HIGH * MBIT, None, HIGH as f64 * 1e6, 40));
    }
    for guard_on in [false, true] {
        ghostframe_lib::transport::bwe::set_inherent_loss_guard(guard_on);
        let arm = if guard_on { "guard_on" } else { "guard_off" };
        for (cell, q, high, step_at, cap_bps, secs) in &cells {
            for (i, seed) in [0xCC04_0021u64, 0xCC04_0022, 0xCC04_0023]
                .into_iter()
                .enumerate()
            {
                let name = format!("s4c_{arm}_{cell}_seed{}", i + 1);
                let points = match step_at {
                    Some(at) => vec![(0, *high), (*at, (cap_bps / 8.0) as u64)],
                    None => vec![(0, *high)],
                };
                let r = run(
                    seed,
                    CapTimeline { points },
                    *q,
                    *secs,
                    &[],
                    OVERDRIVE * HIGH as f64,
                )
                .await;
                assert_premises(&r, &name, 200);
                assert!(
                    r.s2c_datagrams_dropped > 0,
                    "{name}: the link never dropped"
                );
                let end = r.bwe_estimate_samples.last().unwrap().0 + 1;
                let from = end.saturating_sub(20 * S);
                let pct = |v: Option<f64>| v.map_or("none".to_string(), |x| format!("{x:.2}"));
                if let Some(at) = step_at {
                    metric(
                        &name,
                        "loss_pct_first10s",
                        pct(window_loss_pct(&r, *at, at + 10 * S)),
                    );
                }
                metric(
                    &name,
                    "loss_pct_settled",
                    pct(window_loss_pct(&r, from, end)),
                );
                if let Some(m) = cc::median_bps(&r.bwe_estimate_samples, from, end) {
                    metric(&name, "settled_median_bps", m);
                    metric(
                        &name,
                        "settled_over_cap",
                        format!("{:.3}", m as f64 / cap_bps),
                    );
                }
                metric(
                    &name,
                    "queue_delay_p95_ms_settled",
                    fmt_opt(queue_delay_p95_ms(&r, from, end)),
                );
            }
        }
    }
}

/// Not a scene under test: prints the server->client rate each churn level
/// achieves on an uncapped link, so "heavy" is a measured number rather than
/// a guess. Needed because the pacer terms make the offered
/// load far smaller than the raw tile bytes would suggest.
///
/// `#[ignore]`d (~100 s): run with `-- --ignored --nocapture` when re-deriving
/// the load profiles.
#[tokio::test(start_paused = true)]
#[ignore = "calibration printout, ~100 s; run on demand"]
async fn calibrate_offered_load() {
    for tiles in [8usize, 16, 32, 64, 96, 160] {
        let profile = LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region {
                tiles_per_tick: tiles,
            },
        };
        let secs = 8;
        let frames = profile.frames_for(Duration::from_secs(secs), 16, 16);
        let r = run_browserless(BrowserlessScene {
            seed: 0xCC00_0001,
            tick_budget_floor_bytes: None,
            datagram_send_buffer_bytes: None,
            load: SceneLoad::Script(frames),
            cadence_us: PRODUCTION_CADENCE_US,
            net: NetProfile {
                delay_us: 10_000,
                ..NetProfile::perfect()
            },
            drops: Default::default(),
            duration: Duration::from_secs(secs),
            grid_cols: 16,
            grid_rows: 16,
        })
        .await
        .expect("scene ran");
        println!(
            "METRIC calibrate tiles_per_tick {tiles} s2c_mbit_per_s {:.2} est_bps {}",
            r.bytes_delivered_s2c as f64 * 8.0 / secs as f64 / 1e6,
            r.bwe_estimate_bps
        );
        assert!(r.bytes_delivered_s2c > 0);
    }
}
