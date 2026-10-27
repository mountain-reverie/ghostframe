//! D1 PR-6 (`transport/bwe-rate-source`): the gates for where the estimate
//! comes from and what bounds it.
//!
//! Four scenes, each a deterministic function of its seed (the harness is
//! stepped; `browserless_determinism` pins that), so a gate here is a bound
//! on a trajectory, not a flake budget.
//!
//! * **P6a, startup map.** The first probe ladder goog_cc asks for must not
//!   be capped by a cwnd reading taken before anything was sent, whichever
//!   key exchange the handshake used. "Capped" means the first cluster's
//!   target is below `3 x` the start rate. Reported per cell: the ladder, the
//!   time for the estimate to reach 80 % of the link rate, and where it
//!   settles. The glue half of the gate is the lib test
//!   `the_first_probe_ladder_is_never_capped_whatever_the_handshake_cost`.
//! * **P6b, capacity steps.** 5 -> 15 -> 5 Mbit, each level held 20 s.
//! * **P6c, CoDel.** nested_cc's `s1_c10_codel`, over five seeds.
//! * **P6d, app-limited.** 10 s at about a tenth of the link, then a burst.
//!   The estimate must find the link within `T_up` of the burst, and must not
//!   have fallen while the sender had nothing to send: a ceiling taken from
//!   what an idle sender acknowledged would pin it at that tenth.
//!
//! Every scene prints its numbers as `P6 <scene> <name> <value>` before it
//! asserts, so a failing run on an unfixed tree still reports the baseline.
//!
//! Run: `cargo test --test browserless_bwe_rate_source -- --nocapture
//! --test-threads=1`

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, FrameScript, LinkOverrides,
    RunOptions, SceneLoad,
};
use ghostframe_e2e::harness::cc_probe::{self as cc, BAND};
use ghostframe_e2e::harness::load_profile::{
    gradient_tile, Churn, LoadProfile, PRODUCTION_CADENCE_US,
};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, CoDel, NetProfile};
use std::time::Duration;

const S: u64 = 1_000_000;
/// Bytes per second in one Mbit/s.
const MBIT: u64 = 125_000;
/// goog_cc's start rate (`BweWrapper::INITIAL_BPS`), which the startup ladder
/// multiplies: `p1 = 3`, `p2 = 6` in `ProbeControllerConfig`.
const START_BPS: u64 = 2_000_000;
const LADDER_P1: u64 = 3;
/// Overdrive used by the nested_cc scenes: enough offered load to bind the
/// link, little enough that the emitter backlog can drain.
const OVERDRIVE: f64 = 1.3;

/// Tiles per 33.3 ms tick that offer about `offered_mbit` (slope measured by
/// `browserless_nested_cc::calibrate_offered_load`: ~0.30 Mbit/s per
/// tile-per-tick).
fn tiles_for(offered_mbit: f64) -> usize {
    (offered_mbit / 0.30).ceil() as usize
}

fn region(tiles_per_tick: usize) -> LoadProfile {
    LoadProfile {
        cadence_us: PRODUCTION_CADENCE_US,
        churn: Churn::Region { tiles_per_tick },
    }
}

#[derive(Clone, Copy)]
enum Queue {
    DropTail100,
    CoDel,
}

fn bottleneck(q: Queue) -> Bottleneck {
    match q {
        Queue::DropTail100 => Bottleneck {
            depth_ms: 100,
            aqm: None,
        },
        Queue::CoDel => Bottleneck {
            depth_ms: 100,
            aqm: Some(CoDel::rfc_defaults()),
        },
    }
}

async fn run_frames(
    seed: u64,
    net: NetProfile,
    frames: Vec<FrameScript>,
    secs: u64,
    grid: (u8, u8),
    classical_kx: bool,
) -> BrowserlessResult {
    let scene = BrowserlessScene {
        seed,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(frames),
        cadence_us: PRODUCTION_CADENCE_US,
        net,
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: grid.0 as u32,
        grid_rows: grid.1 as u32,
    };
    run_browserless_with_options(
        scene,
        LinkOverrides::default(),
        RunOptions {
            classical_kx,
            // PR-6's gates are on the Cubic arm's rate source (hint, cwnd term).
            congestion: Some(ghostframe_lib::config::CongestionMode::Cubic),
            ..Default::default()
        },
    )
    .await
    .expect("scene ran")
}

/// A 16x16 grid under a constant heavy load over a 10 ms one-way bottleneck.
async fn run_heavy(
    seed: u64,
    cap: CapTimeline,
    q: Queue,
    secs: u64,
    offered_mbit: f64,
) -> BrowserlessResult {
    let frames = region(tiles_for(offered_mbit)).frames_for(Duration::from_secs(secs), 16, 16);
    run_frames(
        seed,
        NetProfile {
            delay_us: 10_000,
            cap,
            bottleneck: Some(bottleneck(q)),
            ..NetProfile::perfect()
        },
        frames,
        secs,
        (16, 16),
        false,
    )
    .await
}

fn p6(scene: &str, name: &str, val: impl std::fmt::Display) {
    println!("P6 {scene} {name} {val}");
}

fn opt<T: std::fmt::Display>(v: Option<T>) -> String {
    v.map_or("none".to_string(), |x| x.to_string())
}

fn ms(v: Option<u64>) -> String {
    opt(v.map(|x| x / 1000))
}

fn median(s: &[(u64, u64)], from: u64, to: u64) -> Option<u64> {
    cc::median_bps(s, from, to)
}

/// Mean of the samples in `[from, to)`: the series is sampled on a fixed
/// cadence, so this is the time average of the estimate. The median hides a
/// bimodal phase (an estimate that spends half its time at each of two values).
fn mean(s: &[(u64, u64)], from: u64, to: u64) -> Option<f64> {
    let w = cc::window(s, from, to);
    if w.is_empty() {
        return None;
    }
    Some(w.iter().map(|(_, v)| *v as f64).sum::<f64>() / w.len() as f64)
}

fn max_in(s: &[(u64, u64)], from: u64, to: u64) -> Option<u64> {
    cc::window(s, from, to).iter().map(|(_, v)| *v).max()
}

/// What every scene reports about the transport, so a reader can see which
/// term set the admission rate and whether the estimator was fed.
fn report_transport(scene: &str, r: &BrowserlessResult) {
    let i = &r.transport.instr;
    p6(scene, "final_est_bps", r.bwe_estimate_bps);
    p6(scene, "pacer_rate_bps", opt(r.pacer_rate_bps));
    p6(
        scene,
        "budget_terms",
        format!(
            "pacer={} cwnd={} floor={} probe={}",
            i.budget.pacer, i.budget.cwnd, i.budget.floor, i.budget.probe
        ),
    );
    p6(
        scene,
        "gate_stops",
        format!(
            "depth={} tokens={}",
            i.budget.gate_depth, i.budget.gate_tokens
        ),
    );
    p6(
        scene,
        "probes",
        format!(
            "requested={} done={} abandoned={} targets_bps={:?}",
            i.bwe.probe_requests,
            r.probes_completed,
            r.probes_abandoned,
            i.bwe.first_probe_targets_bps
        ),
    );
    p6(
        scene,
        "hint",
        format!(
            "feeds={} before_feedback={} unbounded={}",
            i.hint_feeds, i.hint_feeds_before_feedback, i.hint_unbounded
        ),
    );
    p6(
        scene,
        "ceiling",
        format!(
            "reads={} saturated={} binding={} last_acked_bps={} last_ceiling_bps={} reseeds[down={} up={} down_skipped_gap={}] lifts[checked={} undone={} checked_1s={} undone_1s={} blocked_by_queue={} blocked_by_loss={}] down_while_loss_limited={} loss_state_feedbacks={:?} payload_fraction={:.3}",
            i.bwe.ceiling_reads,
            i.bwe.ceiling_saturated_reads,
            i.bwe.ceiling_binding_reads,
            i.bwe.ceiling_acked_bps,
            i.bwe.ceiling_bps,
            i.bwe.ceiling_reseeds_down,
            i.bwe.ceiling_reseeds_up,
            i.bwe.ceiling_reseeds_skipped_gap,
            i.bwe.ceiling_lifts_checked,
            i.bwe.ceiling_lifts_undone,
            i.bwe.ceiling_lifts_checked_1s,
            i.bwe.ceiling_lifts_undone_1s,
            i.bwe.ceiling_lifts_blocked_queue,
            i.bwe.ceiling_lifts_blocked_loss,
            i.bwe.ceiling_reseeds_down_loss_limited,
            i.bwe.loss_state_feedbacks,
            i.bwe.payload_fraction_ppm as f64 / 1e6
        ),
    );
    p6(
        scene,
        "delivery",
        format!(
            "marked={} app_limited_marks={} samples={} app_limited_samples={} pacer_limited_samples={} latest_bps={} latest_app_limited={} max_bps={} max_unpaced_bps={}",
            i.delivery.marked,
            i.delivery.app_limited_marks,
            i.delivery.samples,
            i.delivery.app_limited_samples,
            i.delivery.pacer_limited_samples,
            i.delivery.latest_bps,
            i.delivery.latest_app_limited,
            i.delivery.max_bps,
            i.delivery.max_unpaced_bps
        ),
    );
    p6(
        scene,
        "loss_path",
        format!(
            "fired={} spurious={} useful={}",
            r.loss_path_resends, r.loss_path_resends_spurious, r.loss_path_resends_useful
        ),
    );
    p6(
        scene,
        "ledger_losses",
        format!(
            "declared={} flushed={} real_drops={}",
            i.ledger_losses_declared, i.ledger_losses_flushed, r.s2c_datagrams_dropped
        ),
    );
    p6(
        scene,
        "fifo_lag_us",
        format!(
            "p50={} p95={} max={}",
            i.lag_p50_us, i.lag_p95_us, i.lag_max_us
        ),
    );
    p6(scene, "bytes_s2c", r.bytes_delivered_s2c);
    p6(scene, "critical_lat_mean_us", r.critical_latency_mean_us);
    p6(scene, "implausible_rtt", r.implausible_rtt_samples);
}

fn series_line(scene: &str, r: &BrowserlessResult) {
    p6(
        scene,
        "series_100ms",
        r.bwe_estimate_samples
            .iter()
            .map(|(t, b)| format!("{}:{}", t / 1000, b / 1000))
            .collect::<Vec<_>>()
            .join(","),
    );
}

/// Premises: the estimator ran and moved, and the session carried load. A
/// dead estimator pinned at its seed cannot report anything.
fn premises(r: &BrowserlessResult, scene: &str, min_points: usize) {
    assert!(r.bwe_samples_seen > 0, "{scene}: estimator never fed");
    assert!(
        r.bwe_estimate_samples.len() >= min_points,
        "{scene}: only {} series points",
        r.bwe_estimate_samples.len()
    );
    assert!(
        cc::estimate_moved(&r.bwe_estimate_samples),
        "{scene}: the estimate never moved off its first value"
    );
    assert!(
        r.bytes_delivered_s2c > 0,
        "{scene}: no server->client bytes"
    );
}

// ---------------------------------------------------------------------------
// P6c: CoDel, five seeds
// ---------------------------------------------------------------------------

/// nested_cc's `s1_slow_start_c10_codel` seed.
const P6C_SEED: u64 = 0xCC01_1010;
/// The scene runs these seeds itself, XORed into `P6C_SEED`. A committed list,
/// not an environment variable: CI sets none.
const P6C_SEEDS: [u64; 5] = [0, 1, 2, 3, 4];

/// Gate: the settled estimate is at most 1.15 x the link's rate **in the units
/// the estimate counts**, and it converges on at least four of five seeds.
///
/// The estimate is a rate of goog_cc's countable bytes (the wire size of each
/// acknowledged transmission, no framing), so it is compared with the link's
/// wire rate times the payload fraction the run measured
/// (`payload_fraction_ppm`, the same field `browserless_nested_cc` uses).
/// The old gate divided by the bare wire rate and bounded it at 1.05; the
/// payload fraction in this scene is about 0.856, so 1.05 was 1.05 / 0.856 =
/// 1.23 in these units. In these units master settles at 1.050 to 1.090 (mean
/// 1.074) and the data is <= 1.09; the PR-3 overshoot of 1.26 against the wire
/// rate reads about 1.26 / 0.856 = 1.47. 1.15 is a tightening from 1.23, with
/// room over the data, and leaves the overshoot far outside. It also no longer
/// depends on how much of the link carries parity.
#[tokio::test(start_paused = true)]
async fn p6c_codel_c10_settles_at_the_link_rate_on_five_seeds() {
    const CAP_MBIT: u64 = 10;
    let cap_bps = (CAP_MBIT * 1_000_000) as f64;
    let mut ratios = Vec::new();
    let mut converged = 0;
    for &i in &P6C_SEEDS {
        let name = format!("p6c_seed{i}");
        let r = run_heavy(
            P6C_SEED ^ i,
            CapTimeline::constant(CAP_MBIT * MBIT),
            Queue::CoDel,
            20,
            OVERDRIVE * CAP_MBIT as f64,
        )
        .await;
        premises(&r, &name, 100);
        let s = &r.bwe_estimate_samples;
        let end = s.last().unwrap().0 + 1;
        let tail = end.saturating_sub(5 * S);
        let settled = median(s, tail, end).expect("settled window");
        let frac = r.transport.instr.bwe.payload_fraction_ppm as f64 / 1e6;
        assert!(frac > 0.5 && frac <= 1.0, "{name}: payload fraction {frac}");
        let ratio = settled as f64 / (cap_bps * frac);
        let conv = cc::convergence_time(s, 0, end, cap_bps, BAND, 3 * S);
        p6(&name, "settled_median_bps", settled);
        p6(&name, "settled_over_cap_payload", format!("{ratio:.3}"));
        p6(&name, "t_converge_ms", ms(conv));
        p6(
            &name,
            "t_reach_50pct_ms",
            ms(cc::time_to_first_reach(s, 0, cap_bps, 0.5)),
        );
        report_transport(&name, &r);
        series_line(&name, &r);
        ratios.push(ratio);
        converged += conv.is_some() as usize;
    }
    let mean = ratios.iter().sum::<f64>() / ratios.len() as f64;
    p6(
        "p6c",
        "summary",
        format!(
            "settled_over_cap per seed={:?} mean={mean:.3} converged={converged}/5",
            ratios.iter().map(|r| format!("{r:.3}")).collect::<Vec<_>>()
        ),
    );
    // Countable bytes against the link's wire rate times the payload fraction
    // (see the gate's doc comment for the arithmetic).
    for (i, ratio) in ratios.iter().enumerate() {
        assert!(
            *ratio <= 1.15,
            "seed {i}: settled / (cap x payload fraction) {ratio:.3} is above 1.15 \
             (the unbounded estimate sat at ~1.47 in these units)"
        );
    }
    assert!(
        converged >= 4,
        "converged on only {converged} of 5 seeds (need 4)"
    );
}

// ---------------------------------------------------------------------------
// P6b: capacity steps, each held 20 s
// ---------------------------------------------------------------------------

/// `MIN_TIME_BETWEEN_ALR_PROBES` in goog_cc's `probe_controller.rs:215`.
const MIN_TIME_BETWEEN_ALR_PROBES_US: u64 = 5 * S;
/// `T_up = 2 x MIN_TIME_BETWEEN_ALR_PROBES + 1 s`: one ALR probe to leave the
/// stale estimate, one to follow the chain up, and a second for the ack
/// round trips and the estimator's own settling.
const T_UP_US: u64 = 2 * MIN_TIME_BETWEEN_ALR_PROBES_US + S;
/// `T_react_cold` of the design's section 6.6 for a step of `k = R / C`:
/// `105 ms x k + 35 ms`, the delay detector's blind period after a reset
/// (21 packet groups acknowledged at the bottleneck's drain rate).
fn t_react_cold_us(k: f64) -> u64 {
    ((105_000.0 * k) + 35_000.0) as u64
}

/// Gate (design P6b): after each step up the estimate reaches 0.8 x the new
/// capacity within `T_up`; after a step down it is within 1.2 x the new
/// capacity by `T_react_cold + 1 s` and stays below 1.5 x thereafter.
#[tokio::test(start_paused = true)]
async fn p6b_the_estimate_follows_capacity_steps_each_held_20s() {
    const LOW: u64 = 5;
    const HIGH: u64 = 15;
    const UP_AT: u64 = 20 * S;
    const DOWN_AT: u64 = 40 * S;
    const SECS: u64 = 62;
    let name = "p6b";
    let cap = CapTimeline {
        points: vec![(0, LOW * MBIT), (UP_AT, HIGH * MBIT), (DOWN_AT, LOW * MBIT)],
    };
    let r = run_heavy(
        0xCC03_0001,
        cap,
        Queue::DropTail100,
        SECS,
        OVERDRIVE * HIGH as f64,
    )
    .await;
    premises(&r, name, 300);
    let s = &r.bwe_estimate_samples;
    let end = s.last().unwrap().0 + 1;
    let low = (LOW * 1_000_000) as f64;
    let high = (HIGH * 1_000_000) as f64;

    let t_up = cc::time_to_first_reach(s, UP_AT, high, 0.8).filter(|t| UP_AT + t < DOWN_AT);
    let t_down = {
        let first = s
            .iter()
            .find(|(t, v)| *t >= DOWN_AT && (*v as f64) <= 1.2 * low)
            .map(|(t, _)| t - DOWN_AT);
        first
    };
    let down_bound = t_react_cold_us(HIGH as f64 / LOW as f64) + S;
    let after_down_max = t_down.and_then(|d| max_in(s, DOWN_AT + d, end));
    p6(name, "t_up_reach_80pct_ms", ms(t_up));
    p6(name, "t_up_bound_ms", T_UP_US / 1000);
    p6(name, "t_down_within_1_2x_ms", ms(t_down));
    p6(name, "t_down_bound_ms", down_bound / 1000);
    p6(
        name,
        "max_after_down_over_low",
        opt(after_down_max.map(|m| format!("{:.3}", m as f64 / low))),
    );
    for (label, from, to, cap) in [
        ("low1", UP_AT - 5 * S, UP_AT, low),
        ("high", DOWN_AT - 5 * S, DOWN_AT, high),
        ("low2", end - 5 * S, end, low),
    ] {
        p6(
            name,
            &format!("{label}_median_over_cap"),
            opt(median(s, from, to).map(|m| format!("{:.3}", m as f64 / cap))),
        );
        p6(
            name,
            &format!("{label}_mean_over_cap"),
            opt(mean(s, from, to).map(|m| format!("{:.3}", m / cap))),
        );
    }
    for (label, from, to, cap) in [
        ("low1_phase", 0, UP_AT, low),
        ("high_phase", UP_AT, DOWN_AT, high),
        ("low2_phase", DOWN_AT, end, low),
    ] {
        p6(
            name,
            &format!("{label}_mean_over_cap"),
            opt(mean(s, from, to).map(|m| format!("{:.3}", m / cap))),
        );
    }
    report_transport(name, &r);
    series_line(name, &r);

    let up = t_up.expect("the estimate never reached 0.8 x the new capacity before the step down");
    assert!(
        up <= T_UP_US,
        "step up: 0.8 x capacity after {} ms, past T_up = {} ms",
        up / 1000,
        T_UP_US / 1000
    );
    let down = t_down.expect("the estimate never came within 1.2 x the lower capacity");
    assert!(
        down <= down_bound,
        "step down: within 1.2 x after {} ms, past {} ms",
        down / 1000,
        down_bound / 1000
    );
    let worst = after_down_max.unwrap();
    assert!(
        (worst as f64) < 1.5 * low,
        "step down: the estimate went back up to {:.2} x the capacity",
        worst as f64 / low
    );
    // The estimate is a term of the admission rate in a material share of
    // passes (the PR-0 instrument): with goog_cc's default pacing factor of
    // 2.5 the pacer term sat out of reach of the cwnd term (3 % of passes in
    // this scene); at 1.0 it binds in 19 to 77 % across five seeds (41 % at
    // this one); the PR-3 base: 10 %.
    let b = &r.transport.instr.budget;
    assert!(
        b.pacer * 8 >= b.cwnd,
        "the estimate bound the admission rate in only {} of {} passes",
        b.pacer,
        b.pacer + b.cwnd
    );
}

// ---------------------------------------------------------------------------
// P6d: app-limited, then a burst (design P7c style)
// ---------------------------------------------------------------------------

/// A link of 10 Mbit and 20 ms round trip, as design scene P7c. 10 s of
/// cursor-sized updates (about a tenth of the link), then a change of the
/// whole region at 1.3 x the link for 15 s. Returns the time from the burst to
/// the estimate reaching 0.8 x the link.
async fn app_limited_then_burst(name: &str, loss: f64) -> Option<u64> {
    const CAP_MBIT: u64 = 10;
    const IDLE_SECS: u64 = 10;
    const BURST_SECS: u64 = 15;
    let cap_bps = (CAP_MBIT * 1_000_000) as f64;
    let mut frames =
        region(tiles_for(0.1 * CAP_MBIT as f64)).frames_for(Duration::from_secs(IDLE_SECS), 16, 16);
    let burst_at_tick = frames.len();
    frames.extend(region(tiles_for(OVERDRIVE * CAP_MBIT as f64)).frames_for(
        Duration::from_secs(BURST_SECS),
        16,
        16,
    ));
    let r = run_frames(
        0xCC07_0001,
        NetProfile {
            delay_us: 10_000,
            loss,
            cap: CapTimeline::constant(CAP_MBIT * MBIT),
            bottleneck: Some(bottleneck(Queue::DropTail100)),
            ..NetProfile::perfect()
        },
        frames,
        IDLE_SECS + BURST_SECS + 1,
        (16, 16),
        false,
    )
    .await;
    premises(&r, name, 200);
    let s = &r.bwe_estimate_samples;
    // The series is on the scene clock and the first frame lands once the
    // session is up (a few hundred ms), so this reads the reach time from a
    // moment slightly before the burst: stricter, never looser.
    let burst_at = burst_at_tick as u64 * PRODUCTION_CADENCE_US;
    let before = median(s, burst_at.saturating_sub(3 * S), burst_at);
    let reach = cc::time_to_first_reach(s, burst_at, cap_bps, 0.8);
    p6(name, "burst_at_ms", burst_at / 1000);
    p6(
        name,
        "est_before_burst_over_cap",
        opt(before.map(|m| format!("{:.3}", m as f64 / cap_bps))),
    );
    p6(name, "t_reach_80pct_ms", ms(reach));
    p6(name, "t_up_bound_ms", T_UP_US / 1000);
    report_transport(name, &r);
    series_line(name, &r);
    // Ten seconds of a sender that wanted a tenth of the link must not have
    // lowered the estimate: it starts at `START_BPS`, and an idle sender's
    // own rate says nothing about the path.
    let b = before.expect("estimate samples in the 3 s before the burst");
    assert!(
        b as f64 >= 0.9 * START_BPS as f64,
        "{name}: the estimate fell to {b} while the sender had nothing to send"
    );
    reach
}

/// Gate (the app-limited guard, design scene P7c without its loss): the
/// estimate reaches 0.8 x the link within `T_up` of the burst. An estimate
/// bounded by what an idle sender acknowledged would sit near a tenth of the
/// link and have to climb from there.
#[tokio::test(start_paused = true)]
async fn p6d_an_app_limited_sender_is_not_clamped_to_its_own_rate() {
    let reach = app_limited_then_burst("p6d", 0.0)
        .await
        .expect("the estimate never reached 0.8 x the link after the burst");
    assert!(
        reach <= T_UP_US,
        "0.8 x the link after {} ms, past T_up = {} ms",
        reach / 1000,
        T_UP_US / 1000
    );
}

/// The same with design P7c's 1 % iid loss. Reported, not gated here: the
/// loss-based estimator holds probes off in its loss-limited states, a
/// matter for the controller that replaces this one (PR-7 gates P7c).
#[tokio::test(start_paused = true)]
async fn p6d_with_one_percent_loss_is_reported() {
    let reach = app_limited_then_burst("p6d_loss1", 0.01).await;
    p6(
        "p6d_loss1",
        "reached_within_t_up",
        reach.is_some_and(|r| r <= T_UP_US),
    );
}

// ---------------------------------------------------------------------------
// P6a: startup map
// ---------------------------------------------------------------------------

/// One cell of the startup map: a full-screen first paint (60x34 tiles of
/// gradient, as the investigation scene) followed by sustained change at
/// 1.3 x the link, over a bottleneck of `rate_mbit` and a round trip of
/// `rtt_ms`, with the handshake's key exchange chosen.
struct Cell {
    rate_mbit: u64,
    rtt_ms: u64,
    classical: bool,
}

impl Cell {
    fn name(&self) -> String {
        format!(
            "p6a_{}mbit_{}ms_{}",
            self.rate_mbit,
            self.rtt_ms,
            if self.classical {
                "classical"
            } else {
                "hybrid"
            }
        )
    }
}

struct CellResult {
    capped: bool,
    t80_ms: Option<u64>,
}

async fn run_cell(c: &Cell) -> CellResult {
    let name = c.name();
    let (cols, rows) = (60u8, 34u8);
    let first_paint = FrameScript {
        tiles: (0..cols)
            .flat_map(|x| {
                (0..rows).map(move |y| {
                    (
                        (x, y),
                        TileSpec::Cdf53 {
                            bgra: gradient_tile(0, x, y),
                        },
                    )
                })
            })
            .collect(),
    };
    let secs = 20;
    // The first paint, then 1.3 x the link in continuing change, so the
    // estimate has traffic to be measured on for as long as the cell runs
    // (a single frame leaves an application-limited sender after a second).
    let mut frames = vec![first_paint];
    frames.extend(
        region(tiles_for(OVERDRIVE * c.rate_mbit as f64)).frames_for(
            // The full duration, not `secs - 1`: injection starts after the
            // handshake, so a shorter profile left about a second of
            // heartbeat-only tail and `final_est_bps` was read inside it.
            Duration::from_secs(secs),
            cols,
            rows,
        ),
    );
    let r = run_frames(
        0x5B10_C4ED ^ (c.rate_mbit << 8) ^ c.rtt_ms ^ ((c.classical as u64) << 32),
        NetProfile {
            delay_us: c.rtt_ms * 1000 / 2,
            cap: CapTimeline::constant(c.rate_mbit * MBIT),
            bottleneck: Some(Bottleneck::wifi()),
            ..NetProfile::perfect()
        },
        frames,
        secs,
        (cols, rows),
        c.classical,
    )
    .await;
    let i = &r.transport.instr;
    let targets = i.bwe.first_probe_targets_bps;
    let cap_bps = (c.rate_mbit * 1_000_000) as f64;
    let t80 = cc::time_to_first_reach(&r.bwe_estimate_samples, 0, cap_bps, 0.8);
    // The ladder is `p1 x start` and `p2 x start`; capped is the first rung
    // collapsed to about the start rate, or no ladder at all.
    let capped = i.bwe.probe_requests == 0 || targets[0] < LADDER_P1 * START_BPS;
    p6(&name, "first_probe_targets_bps", format!("{targets:?}"));
    p6(&name, "capped", capped);
    p6(&name, "t_reach_80pct_ms", ms(t80));
    let tail = r.bwe_estimate_samples.last().map_or(0, |(t, _)| *t);
    p6(
        &name,
        "settled_over_cap",
        opt(median(
            &r.bwe_estimate_samples,
            tail.saturating_sub(5 * S),
            tail + 1,
        )
        .map(|m| format!("{:.3}", m as f64 / cap_bps))),
    );
    report_transport(&name, &r);
    CellResult {
        capped,
        t80_ms: t80.map(|t| t / 1000),
    }
}

async fn run_map(cells: &[Cell]) {
    let mut capped = 0;
    let mut rows = Vec::new();
    for c in cells {
        let res = run_cell(c).await;
        capped += res.capped as usize;
        rows.push(format!(
            "{}={}",
            c.name(),
            res.t80_ms.map_or("never".to_string(), |t| format!("{t}ms"))
        ));
    }
    p6(
        "p6a",
        "summary",
        format!("capped={capped}/{} t80: {}", cells.len(), rows.join(" ")),
    );
    assert_eq!(
        capped,
        0,
        "{capped} of {} cells had their first probe ladder capped",
        cells.len()
    );
}

/// The CI subset of the startup map (the design's Q10: the full map runs at
/// PR-0, PR-7 and PR-10, a smoke set elsewhere): the two corners that
/// stress the race hardest and a middle cell, both key exchanges. A cell is
/// one full-screen first paint over the bottleneck.
#[tokio::test(start_paused = true)]
async fn p6a_startup_ladder_is_never_capped_smoke() {
    let mut cells = Vec::new();
    for classical in [false, true] {
        for (rate_mbit, rtt_ms) in [(2, 100), (10, 40), (20, 10)] {
            cells.push(Cell {
                rate_mbit,
                rtt_ms,
                classical,
            });
        }
    }
    run_map(&cells).await;
}

/// The full map: rates 2/5/10/20 Mbit x round trips 10/40/100 ms x both key
/// exchanges. Run by hand for PR-0, PR-7 and PR-10 (design Q10).
#[tokio::test(start_paused = true)]
#[ignore = "full startup map, ~24 cells; run on demand"]
async fn p6a_startup_ladder_is_never_capped_full_map() {
    let mut cells = Vec::new();
    for classical in [false, true] {
        for rate_mbit in [2, 5, 10, 20] {
            for rtt_ms in [10, 40, 100] {
                cells.push(Cell {
                    rate_mbit,
                    rtt_ms,
                    classical,
                });
            }
        }
    }
    run_map(&cells).await;
}

// ---------------------------------------------------------------------------
// PR-6b: the per-transmission delivery-rate sampler
// ---------------------------------------------------------------------------

/// A 10 Mbit link, drop-tail, with `offered_mbit` of load for `secs`.
async fn delivery_scene(seed: u64, offered_mbit: f64, secs: u64) -> BrowserlessResult {
    run_heavy(
        seed,
        CapTimeline::constant(10 * MBIT),
        Queue::DropTail100,
        secs,
        offered_mbit,
    )
    .await
}

fn report_delivery(scene: &str, r: &BrowserlessResult) {
    let d = &r.transport.instr.delivery;
    let max = r
        .delivery_rate_samples
        .iter()
        .map(|(_, v)| *v)
        .max()
        .unwrap_or(0);
    p6(
        scene,
        "delivery_sampler",
        format!(
            "marked={} app_limited_marks={} acked={} samples={} app_limited_samples={} \
             latest_bps={} peak_windowed_max_bps={}",
            d.marked,
            d.app_limited_marks,
            d.acked,
            d.samples,
            d.app_limited_samples,
            d.latest_bps,
            max
        ),
    );
}

/// A sender that wants a tenth of the link never holds work back and never
/// leaves quinn a queue, so what it sends is app-limited: the departures are
/// marked, and the samples they produce say so. Premises are counts that are
/// non-zero only if the wiring (F1 backlog and FIFO signals) was reached.
#[tokio::test(start_paused = true)]
async fn d6b_a_sender_with_little_to_send_is_marked_app_limited() {
    let r = delivery_scene(0xD6B0_0001, 1.0, 8).await;
    premises(&r, "d6b_sparse", 50);
    report_delivery("d6b_sparse", &r);
    let d = &r.transport.instr.delivery;
    assert!(d.marked > 100, "only {} departures marked", d.marked);
    assert!(d.samples > 20, "only {} samples", d.samples);
    assert!(
        d.app_limited_marks > d.marked / 2,
        "{} of {} departures marked app-limited",
        d.app_limited_marks,
        d.marked
    );
    assert!(
        d.app_limited_samples > d.samples / 2,
        "{} of {} samples app-limited",
        d.app_limited_samples,
        d.samples
    );
}

/// An overdriven sender has backlog throughout, so most of what it sends is
/// not app-limited, and the sampler reads a rate: nonzero, and of the link's
/// order (the exact figure is reported, not gated; it is PR-7's input).
#[tokio::test(start_paused = true)]
async fn d6b_a_saturated_sender_is_not_marked_app_limited() {
    let r = delivery_scene(0xD6B0_0002, OVERDRIVE * 10.0, 12).await;
    premises(&r, "d6b_saturated", 100);
    report_delivery("d6b_saturated", &r);
    let d = &r.transport.instr.delivery;
    assert!(d.samples > 100, "only {} samples", d.samples);
    assert!(
        d.app_limited_marks < d.marked / 2,
        "{} of {} departures marked app-limited under overload",
        d.app_limited_marks,
        d.marked
    );
    assert!(
        d.app_limited_samples < d.samples / 2,
        "{} of {} samples app-limited under overload",
        d.app_limited_samples,
        d.samples
    );
    let max = r
        .delivery_rate_samples
        .iter()
        .map(|(_, v)| *v)
        .max()
        .unwrap_or(0);
    assert!(max > 0, "the windowed maximum never rose");
}
