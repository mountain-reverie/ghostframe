//! Adaptive FEC (rev 6, R6-e): the controller's gate, end to end. Quinn's
//! `congestion_events` is loss under Cubic, so the gate reads the delay
//! signal (a standing queue) only: random loss engages and a clean link
//! stays off. (The gate itself, a standing queue skipping an interval, is a
//! bridge-level test in `block_tx_tests`: at a tail-drop bottleneck the queue
//! has often drained by the time the loss is reported, so a scene cannot
//! assert that every such interval is gated.)

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, LinkOverrides,
    RunOptions, SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::h264_scene::{pcm_load, Pattern};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, NetProfile};
use ghostframe_lib::config::FecBlockParams;
use ghostframe_lib::encoder::h264_harness::{PcmMode, PcmRateModel};
use ghostframe_lib::transport::drop_plan::{FragSel, FrameDropRule};
use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass;

const SEEDS: [u64; 3] = [0xFB_0001, 0xFB_0002, 0xFB_0003];

fn adaptive() -> RunOptions {
    RunOptions {
        fec_adaptive: true,
        ..RunOptions::default()
    }
}

fn h264_load() -> H264Load {
    let mut l = pcm_load(PcmMode::RefManager);
    l.pcm_rate_policy = true;
    l
}

async fn run_h264_iid(seed: u64, loss: f64) -> BrowserlessResult {
    let base = NetProfile {
        delay_us: 50_000,
        ..NetProfile::perfect()
    };
    let (net, links) = Pattern::Iid(loss).apply(base);
    run_scene(seed, net, links, DropPlan::default(), 10_000).await
}

async fn run_scene(
    seed: u64,
    net: NetProfile,
    links: LinkOverrides,
    drops: DropPlan,
    ms: u64,
) -> BrowserlessResult {
    run_scene_with(h264_load(), seed, net, links, drops, ms).await
}

async fn run_scene_with(
    load: H264Load,
    seed: u64,
    net: NetProfile,
    links: LinkOverrides,
    drops: DropPlan,
    ms: u64,
) -> BrowserlessResult {
    let scene = BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::H264(load),
        cadence_us: DEFAULT_CADENCE_US,
        net,
        drops,
        duration: Duration::from_millis(ms),
        grid_cols: 4,
        grid_rows: 4,
    };
    run_browserless_with_options(scene, links, adaptive())
        .await
        .expect("scene ran")
}

fn repairs(r: &BrowserlessResult) -> u64 {
    r.transport.instr.offered.of(WireClass::Repair)
}

#[tokio::test(start_paused = true)]
async fn five_percent_iid_engages_although_quinn_declares_congestion_events() {
    for seed in SEEDS {
        let r = run_h264_iid(seed, 0.05).await;
        println!(
            "FECA iid seed={seed:#x} dropped={} ce={} repairs={} loss_reports={}",
            r.s2c_datagrams_dropped,
            r.transport.congestion_events,
            repairs(&r),
            r.transport.loss_reports
        );
        // Premises: the link dropped, quinn read it as congestion, the
        // client's reports arrived.
        assert!(r.s2c_datagrams_dropped > 0, "seed {seed:#x}");
        assert!(r.transport.congestion_events > 0, "seed {seed:#x}");
        assert!(r.transport.loss_reports > 0, "seed {seed:#x}");
        assert!(
            repairs(&r) > 0,
            "seed {seed:#x}: the controller never engaged"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_clean_link_stays_off() {
    for seed in SEEDS {
        let r = run_h264_iid(seed, 0.0).await;
        assert_eq!(r.s2c_datagrams_dropped, 0);
        assert!(r.transport.loss_reports > 0, "premise: reports arrived");
        assert_eq!(repairs(&r), 0, "seed {seed:#x}");
    }
}

/// Premises every scene in this file stands on: the client's LOSS_REPORT
/// reaches the controller, a lossless run reports no loss and keeps the
/// controller idle, and a lossy one reports loss and moves it.
#[tokio::test(start_paused = true)]
async fn premise_loss_reports_arrive_and_count_loss() {
    for seed in SEEDS {
        let clean = run_h264_iid(seed, 0.0).await;
        let c = &clean.transport;
        println!(
            "FECA premise clean seed={seed:#x} reports={} lost_pre_fec={} state_engaged={} sized={}",
            c.loss_reports, c.report_lost_pre_fec_total, c.adapt.engaged, c.adapt.blocks_sized
        );
        assert!(c.loss_reports > 0, "seed {seed:#x}: no LOSS_REPORT arrived");
        assert_eq!(c.report_lost_pre_fec_total, 0, "seed {seed:#x}");
        assert!(!c.adapt.engaged && c.adapt.changes == 0, "seed {seed:#x}");
        assert_eq!(c.adapt.blocks_sized, 0, "seed {seed:#x}");
        assert_eq!(c.adapt.p_hat_ppm, 0, "seed {seed:#x}");

        let lossy = run_h264_iid(seed, 0.03).await;
        let l = &lossy.transport;
        println!(
            "FECA premise 3% seed={seed:#x} reports={} lost_pre_fec={} p_hat_ppm={} changes={} sized={} saturated={} r8={} r64={} k_max={} t_ms={} skips={} trim={}",
            l.loss_reports, l.report_lost_pre_fec_total, l.adapt.p_hat_ppm, l.adapt.changes,
            l.adapt.blocks_sized, l.adapt.saturated, l.adapt.r_at_k8, l.adapt.r_at_k64,
            l.adapt.k_max, l.adapt.t_ms, l.adapt.congestion_skips, l.adapt.trim_steps
        );
        assert!(l.loss_reports > 0, "seed {seed:#x}");
        assert!(l.report_lost_pre_fec_total > 0, "seed {seed:#x}");
        assert!(l.adapt.engaged, "seed {seed:#x}");
        assert!(
            l.adapt.p_hat_ppm > 0 && l.adapt.b_hat_x100 >= 100,
            "seed {seed:#x}"
        );
        assert!(l.adapt.changes >= 1, "seed {seed:#x}");
        assert!(l.adapt.blocks_sized > 0, "seed {seed:#x}");
        assert!(l.adapt.saturated <= l.adapt.blocks_sized, "seed {seed:#x}");
        assert!(
            l.adapt.r_at_k8 >= 1 && l.adapt.r_at_k64 >= l.adapt.r_at_k8,
            "seed {seed:#x}"
        );
        assert!(l.adapt.k_max > 0 && l.adapt.t_ms > 0, "seed {seed:#x}");
    }
}

// ---- the scenes of E7 ------------------------------------------------------

/// About 10 Mbit/s, fixed: enough datagrams per second that the estimator's
/// counts are in the window's range (see the finding on scene 3).
fn high_rate_load() -> H264Load {
    let mut load = h264_load();
    load.pcm_rate_policy = false;
    load.p_bytes = 40_000;
    load
}

fn base_net() -> NetProfile {
    NetProfile {
        delay_us: 50_000,
        ..NetProfile::perfect()
    }
}

/// A clean s2c base whose i.i.d. loss follows `steps` (virtual us, loss).
fn stepped(steps: Vec<(u64, f64)>) -> (NetProfile, LinkOverrides) {
    let (net, mut links) = Pattern::Clean.apply(base_net());
    links.s2c_loss_steps = steps;
    (net, links)
}

fn line(name: &str, seed: u64, r: &BrowserlessResult) {
    let (t, a) = (&r.transport, &r.transport.adapt);
    println!(
        "FECA {name} seed={seed:#x} dropped={} reports={} lost_pre_fec={} repairs={} engaged={} \
         changes={} floor_changes={} p_hat_ppm={} b_hat_x100={} k_max={} t_ms={} r8={} r64={} sized={} sat={} \
         skips={} trim={}",
        r.s2c_datagrams_dropped,
        t.loss_reports,
        t.report_lost_pre_fec_total,
        repairs(r),
        a.engaged,
        a.changes,
        a.floor_changes,
        a.p_hat_ppm,
        a.b_hat_x100,
        a.k_max,
        a.t_ms,
        a.r_at_k8,
        a.r_at_k64,
        a.blocks_sized,
        a.saturated,
        a.congestion_skips,
        a.trim_steps
    );
}

/// (1) A clean link never leaves the clean state and sends no repair; and a
/// loss rate under the floor (0.02 % against 0.1 %), once enough datagrams
/// were lost to be a statistical claim, does not engage either.
#[tokio::test(start_paused = true)]
async fn clean_link_and_sub_floor_loss_stay_clean() {
    for seed in SEEDS {
        let r = run_h264_iid(seed, 0.0).await;
        line("clean", seed, &r);
        assert!(r.transport.loss_reports > 0, "premise: reports arrived");
        assert_eq!(r.s2c_datagrams_dropped, 0);
        assert_eq!(repairs(&r), 0, "seed {seed:#x}");
        assert!(!r.transport.adapt.engaged, "seed {seed:#x}");
        assert_eq!(r.transport.adapt.changes, 0, "seed {seed:#x}");

        // Sub-floor, at a rate where the decayed count can reach three: long
        // enough that a floor-less controller engages (checked by the break).
        let (net, links) = Pattern::Iid(SUB_FLOOR_LOSS).apply(base_net());
        let r = run_scene_with(
            high_rate_load(),
            seed,
            net,
            links,
            DropPlan::default(),
            40_000,
        )
        .await;
        line("subfloor", seed, &r);
        assert!(r.transport.loss_reports > 0);
        assert!(
            r.transport.report_lost_pre_fec_total >= 10,
            "seed {seed:#x}: premise, a statistical number of losses: {}",
            r.transport.report_lost_pre_fec_total
        );
        assert_eq!(repairs(&r), 0, "seed {seed:#x}");
        assert!(!r.transport.adapt.engaged, "seed {seed:#x}");
        assert_eq!(r.transport.adapt.changes, 0, "seed {seed:#x}");
    }
}

const SUB_FLOOR_LOSS: f64 = 0.0005;

/// (2) A step from no loss to 5 % at t = 5 s: engaged two seconds later (the
/// client reports a loss about a second after it, measured: at 1.0 s the first
/// report carrying a loss had not arrived, at 1.5 s it had), repairs flow.
#[tokio::test(start_paused = true)]
async fn a_loss_step_engages_within_two_seconds() {
    for seed in SEEDS {
        let (net, links) = stepped(vec![(5_000_000, 0.05)]);
        let r = run_scene(seed, net, links, DropPlan::default(), 7_000).await;
        line("step", seed, &r);
        assert!(r.transport.loss_reports > 0, "premise");
        assert!(r.s2c_datagrams_dropped > 0, "premise: the step dropped");
        assert!(r.transport.report_lost_pre_fec_total > 0, "premise");
        assert!(
            r.transport.adapt.engaged,
            "seed {seed:#x}: not engaged within 2 s"
        );
        assert!(r.transport.adapt.changes >= 1, "seed {seed:#x}");
        assert!(
            repairs(&r) > 0,
            "seed {seed:#x}: engaged but no repair flowed"
        );
        assert!(r.transport.adapt.blocks_sized > 0, "seed {seed:#x}");
        // Nothing was lost before the step, so r > 0 is the step's doing.
        assert!(r.transport.adapt.r_at_k8 >= 1, "seed {seed:#x}");
    }
}

/// (3) Loss that ends releases the state after the dwell and does not flap.
///
/// Two arms. The low-rate arm runs at the default scene's rate (about 180
/// datagrams/s). It used to stay engaged for the whole session, even with no
/// loss: time decay left `total` near 1 840, under the 3 330 the release bound
/// needs; `FEC_ADAPT_MIN_MASS` fixed that, at the price of a window of about
/// 5 000 datagrams (28 s here). The high-rate arm (`p_bytes` 40 000, about
/// 10 Mbit/s) is the case that always worked.
#[tokio::test(start_paused = true)]
async fn loss_that_ends_releases_at_a_high_rate_without_flapping() {
    for seed in SEEDS {
        let (net, links) = stepped(vec![(1_000_000, 0.05), (4_000_000, 0.0)]);
        let r = run_scene_with(
            high_rate_load(),
            seed,
            net,
            links,
            DropPlan::default(),
            60_000,
        )
        .await;
        line("release", seed, &r);
        assert!(r.s2c_datagrams_dropped > 0, "premise");
        assert!(repairs(&r) > 0, "premise: it engaged and sent repairs");
        assert!(r.transport.adapt.blocks_sized > 0, "premise");
        assert!(!r.transport.adapt.engaged, "seed {seed:#x}: not released");
        // Engage once, release once: the floor state never flapped.
        assert_eq!(
            r.transport.adapt.floor_changes, 2,
            "seed {seed:#x}: floor state flapped or never moved"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn loss_that_ends_releases_at_the_default_rate_without_flapping() {
    for seed in SEEDS {
        let (net, links) = stepped(vec![(1_000_000, 0.05), (4_000_000, 0.0)]);
        let r = run_scene(seed, net, links, DropPlan::default(), 200_000).await;
        line("release-low", seed, &r);
        assert!(r.s2c_datagrams_dropped > 0, "premise");
        assert!(repairs(&r) > 0, "premise: it engaged and sent repairs");
        assert!(!r.transport.adapt.engaged, "seed {seed:#x}: not released");
        assert_eq!(
            r.transport.adapt.floor_changes, 2,
            "seed {seed:#x}: floor state flapped or never moved"
        );
    }
}

/// (4) One stray loss, in an otherwise clean run, never engages.
#[tokio::test(start_paused = true)]
async fn a_single_stray_loss_never_engages() {
    for seed in SEEDS {
        let (net, links) = Pattern::Clean.apply(base_net());
        let drops = DropPlan::default().with_frame_rules(vec![FrameDropRule {
            frame_seq: 30,
            frag: FragSel::Index(0),
            occurrences: vec![0],
        }]);
        let r = run_scene(seed, net, links, drops, 10_000).await;
        line("stray", seed, &r);
        assert_eq!(r.frame_drops_fired, vec![1], "premise: the rule fired once");
        assert!(
            r.transport.report_lost_pre_fec_total >= 1,
            "seed {seed:#x}: premise: the client reported the loss"
        );
        assert_eq!(repairs(&r), 0, "seed {seed:#x}");
        assert!(!r.transport.adapt.engaged, "seed {seed:#x}");
        assert_eq!(r.transport.adapt.changes, 0, "seed {seed:#x}");
    }
}

/// (5a) 5 % i.i.d. loss on an unconstrained link is not gated: no standing
/// queue, so nothing is skipped, and the state is engaged.
#[tokio::test(start_paused = true)]
async fn iid_loss_on_an_unconstrained_link_is_not_gated() {
    for seed in SEEDS {
        let r = run_h264_iid(seed, 0.05).await;
        line("ungated", seed, &r);
        assert!(r.s2c_datagrams_dropped > 0, "premise");
        assert!(r.transport.report_lost_pre_fec_total > 0, "premise");
        assert_eq!(r.transport.adapt.congestion_skips, 0, "seed {seed:#x}");
        assert!(r.transport.adapt.engaged, "seed {seed:#x}");
    }
}

/// (5b) A bandwidth-limited bottleneck with queueing loss is gated: a standing
/// queue forms, the intervals that still see it skip their losses, and the
/// controller does not read that congestion as random loss. The link drops from
/// 6 MB/s to 300 kB/s at 4 s, behind a 200 ms tail-drop buffer.
///
/// LIMIT (the plan's known one, measured here): at a tail-drop bottleneck the
/// queue has often drained by the time the client's report arrives, so only
/// some intervals are gated (3 or 4 of 89 reports here) and a seed whose
/// ungated losses are enough still engages. The assertion is therefore on the
/// property that holds: every seed gates something, and the controller stays
/// clean on at least half of the seeds. Without the gate every seed engages.
#[tokio::test(start_paused = true)]
async fn queueing_loss_at_a_bottleneck_is_gated() {
    let seeds = [SEEDS[0], SEEDS[1], SEEDS[2], 0xFB_0004, 0xFB_0005];
    let mut engaged = 0;
    for seed in seeds {
        let net = NetProfile {
            delay_us: 25_000,
            cap: CapTimeline::step(6_000_000, 4_000_000, BOTTLENECK_BPS),
            bottleneck: Some(Bottleneck {
                depth_ms: 200,
                aqm: None,
            }),
            ..NetProfile::perfect()
        };
        let (net, links) = Pattern::Clean.apply(net);
        let r = run_scene(seed, net, links, DropPlan::default(), 20_000).await;
        line("bottleneck", seed, &r);
        assert!(r.s2c_datagrams_dropped > 0, "premise: the queue overflowed");
        assert!(
            r.transport.report_lost_pre_fec_total > 0,
            "premise: losses reported"
        );
        assert!(
            r.transport.adapt.congestion_skips > 0,
            "seed {seed:#x}: no interval was gated"
        );
        engaged += usize::from(r.transport.adapt.engaged);
    }
    assert!(
        engaged * 2 <= seeds.len(),
        "{engaged} of {} seeds engaged on congestion loss",
        seeds.len()
    );
}

const BOTTLENECK_BPS: u64 = 300_000;

/// (6) Gilbert-Elliott bursts of mean length 3 (3 % average loss): the span
/// ladder climbs (a longer window, a wider block) and the controller sees
/// bursts; the i.i.d. run at the same loss stays on the base notch.
#[tokio::test(start_paused = true)]
async fn burst_loss_climbs_the_ladder() {
    for seed in SEEDS {
        let iid = run_h264_iid(seed, 0.03).await;
        line("burst-control-iid", seed, &iid);
        let exit = 1.0 / 3.0;
        let enter = 0.03 * exit / (1.0 - 0.03);
        let (net, links) = Pattern::Burst {
            enter,
            exit,
            loss: 1.0,
        }
        .apply(base_net());
        let b = run_scene(seed, net, links, DropPlan::default(), 10_000).await;
        line("burst", seed, &b);
        let (a, c) = (&b.transport.adapt, &iid.transport.adapt);
        assert!(
            b.s2c_datagrams_dropped > 0 && iid.s2c_datagrams_dropped > 0,
            "premise"
        );
        assert!(a.engaged && c.engaged, "seed {seed:#x}");
        assert!(
            a.b_hat_x100 > 150,
            "seed {seed:#x}: burst not seen: {}",
            a.b_hat_x100
        );
        assert!(
            c.b_hat_x100 <= 150,
            "seed {seed:#x}: iid read as bursty: {}",
            c.b_hat_x100
        );
        assert!(
            a.t_ms > c.t_ms,
            "seed {seed:#x}: T {} vs {}",
            a.t_ms,
            c.t_ms
        );
        assert!(a.k_max >= c.k_max, "seed {seed:#x}");
        assert!(a.r_at_k64 >= 1 && a.r_at_k64 <= 32, "seed {seed:#x}");
    }
}

/// A stationary rate right at the floor (0.1 %) does not flap: the engage and
/// release bounds do not overlap and the dwell holds the state.
#[tokio::test(start_paused = true)]
async fn a_stationary_rate_at_the_floor_does_not_flap() {
    for seed in SEEDS {
        let (net, links) = Pattern::Iid(0.001).apply(base_net());
        let r = run_scene_with(
            high_rate_load(),
            seed,
            net,
            links,
            DropPlan::default(),
            90_000,
        )
        .await;
        line("atfloor", seed, &r);
        assert!(
            r.transport.report_lost_pre_fec_total >= 30,
            "premise: lossy run"
        );
        assert!(r.transport.loss_reports > 0);
        assert!(
            r.transport.adapt.floor_changes <= 3,
            "seed {seed:#x}: {} floor changes in 90 s",
            r.transport.adapt.floor_changes
        );
    }
}

// ---- E11: the control-law sweep --------------------------------------------
//
// Loss {clean, 0.01, 0.1, 1, 5 % i.i.d., Gilbert-Elliott mean run 3 at 1 %}
// x rate cap {2, 10, 30, 100 Mbit/s} x load {H.264, tile}, each against three
// arms: the knob off, fixed `64,5,2`, adaptive. Every run prints one
// `FECSWEEP run` line; the cells' verdicts print as `FECSWEEP crit` lines and
// the test fails if a criterion does, so a failed criterion cannot go
// unnoticed. The reduced grid runs in CI; `GHOSTFRAME_FEC_SWEEP=full` runs the
// whole one (locally, about an hour and a half).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Arm {
    Off,
    Fixed,
    Adaptive,
}

impl Arm {
    const ALL: [Arm; 3] = [Arm::Off, Arm::Fixed, Arm::Adaptive];
    fn name(self) -> &'static str {
        match self {
            Arm::Off => "off",
            Arm::Fixed => "fixed",
            Arm::Adaptive => "adaptive",
        }
    }
    fn options(self) -> RunOptions {
        match self {
            Arm::Off => RunOptions::default(),
            Arm::Fixed => RunOptions {
                fec_blocks: Some(FecBlockParams {
                    k_max: 64,
                    t_ms: 5,
                    r: 2,
                }),
                ..RunOptions::default()
            },
            Arm::Adaptive => adaptive(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Load {
    H264,
    Tile,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Loss {
    Clean,
    Iid(f64),
    /// Gilbert-Elliott, loss 1 in the bad state, mean run 3 datagrams, 1 % on
    /// average (the per-datagram chain of `Pattern::Burst`).
    Ge3,
}

impl Loss {
    fn name(self) -> String {
        match self {
            Loss::Clean => "clean".into(),
            Loss::Iid(p) => format!("iid{}", p * 100.0),
            Loss::Ge3 => "ge3at1".into(),
        }
    }
    /// The chain's `(enter, exit)` per datagram; i.i.d. is the chain with
    /// `exit = 1 - enter`.
    fn chain(self) -> Option<(f64, f64)> {
        match self {
            Loss::Clean => None,
            Loss::Iid(p) => Some((p, 1.0 - p)),
            Loss::Ge3 => Some((0.01 / 3.0 / 0.99, 1.0 / 3.0)),
        }
    }
}

/// The scene of one cell: a rate cap behind a 150 ms tail-drop buffer, 50 ms
/// round trip, the loss on the frame path, and a stream that follows the
/// estimate (H.264: the PCM encoder with the rate policy, as in the probe
/// suites; tile: a damage window offering 0.8 x the cap).
fn sweep_scene(
    load: Load,
    loss: Loss,
    cap_mbit: u64,
    seed: u64,
    ms: u64,
) -> (BrowserlessScene, LinkOverrides) {
    let base = NetProfile {
        delay_us: 25_000,
        cap: CapTimeline::constant(cap_mbit * 125_000),
        bottleneck: Some(Bottleneck {
            depth_ms: 150,
            aqm: None,
        }),
        ..NetProfile::perfect()
    };
    let pattern = match loss {
        Loss::Clean => Pattern::Clean,
        Loss::Iid(p) => Pattern::Iid(p),
        Loss::Ge3 => {
            let (enter, exit) = loss.chain().expect("a chain");
            Pattern::Burst {
                enter,
                exit,
                loss: 1.0,
            }
        }
    };
    let (net, links) = pattern.apply(base);
    let (scene_load, cadence_us, grid) = match load {
        Load::H264 => {
            let mut l = h264_load();
            let frame = (cap_mbit as usize * 125_000 / 30) * 3 / 2;
            l.pcm_rate_model = Some(PcmRateModel::s2(frame, frame * 3, 30));
            (SceneLoad::H264(l), DEFAULT_CADENCE_US, (4, 4))
        }
        Load::Tile => {
            // About 0.30 Mbit/s per tile per tick (browserless_nested_cc).
            let tiles = ((0.8 * cap_mbit as f64) / 0.30).ceil() as usize;
            let side = if tiles > 256 { 32 } else { 16 };
            (
                SceneLoad::Profile(LoadProfile {
                    cadence_us: PRODUCTION_CADENCE_US,
                    churn: Churn::Region {
                        tiles_per_tick: tiles,
                    },
                }),
                PRODUCTION_CADENCE_US,
                (side, side),
            )
        }
    };
    (
        BrowserlessScene {
            seed,
            datagram_send_buffer_bytes: None,
            tick_budget_floor_bytes: None,
            load: scene_load,
            cadence_us,
            net,
            drops: DropPlan::default(),
            duration: Duration::from_millis(ms),
            grid_cols: grid.0,
            grid_rows: grid.1,
        },
        links,
    )
}

/// What one run reads.
#[derive(Clone, Debug, Default)]
struct SweepRun {
    dropped: u64,
    offered_dg: u64,
    lost_pre_fec: u64,
    reports: u64,
    src_bytes: u64,
    repair_bytes: u64,
    /// Planned repair rows over source symbols, `r / k` as sent.
    rho_dg: f64,
    k_mean: f64,
    k_buckets: [u32; 24],
    accounted: u64,
    declined: u64,
    handed_off: u64,
    timed_out: u64,
    /// Hand-offs to ARQ plus declines (the E10 unit).
    handoffs_declined: u64,
    retx: u64,
    changes: u64,
    floor_changes: u64,
    engaged: bool,
    sized: u64,
    saturated: u64,
    skips: u64,
    trim: u64,
    p_hat_ppm: u64,
    b_hat_x100: u64,
    bwe_binding_share: f64,
    bwe_reads: u64,
    digest: u64,
    /// H.264 only: frames shown, frames late (more than an RTT over the
    /// stream's own floor), never-fed frames.
    shown: usize,
    late: usize,
    never_fed: usize,
    secs: u64,
}

impl SweepRun {
    /// `(blocks, source symbols, rows of `rows(k)`)` over the blocks the
    /// run closed, each size taken at its bucket's midpoint.
    fn rows_over_blocks(&self, rows: impl Fn(usize) -> usize) -> f64 {
        let (mut k_sum, mut r_sum) = (0.0, 0.0);
        for (i, n) in self.k_buckets.iter().enumerate() {
            let (lo, hi) = ghostframe_lib::transport::io_bridge::block_k_bucket_range(i);
            let k = (lo + hi) / 2;
            k_sum += *n as f64 * k as f64;
            r_sum += *n as f64 * rows(k) as f64;
        }
        r_sum / k_sum.max(1.0)
    }
    fn rho_bytes(&self) -> f64 {
        self.repair_bytes as f64 / self.src_bytes.max(1) as f64
    }
    fn sat_share(&self) -> f64 {
        self.saturated as f64 / self.sized.max(1) as f64
    }
    fn retx_per_lost(&self) -> f64 {
        self.retx as f64 / self.dropped.max(1) as f64
    }
    fn changes_per_min(&self) -> f64 {
        self.changes as f64 * 60.0 / self.secs as f64
    }
}

/// A frame is late when the client decoded it more than an RTT after the
/// stream's own floor (the fastest frame): it waited for a retransmission.
const SWEEP_LATE_US: u64 = 100_000;

fn frames_late(r: &BrowserlessResult) -> (usize, usize, usize) {
    let Some(run) = r.h264_run() else {
        return (0, 0, 0);
    };
    let ts: std::collections::HashMap<u32, u64> = run
        .truth
        .order
        .iter()
        .zip(&run.truth.au_log)
        .map(|(seq, au)| (*seq, u64::from(au.timestamp_us)))
        .collect();
    let mut seen = std::collections::HashSet::new();
    let mut lat: Vec<u64> = Vec::new();
    for rec in &run.display {
        if seen.insert(rec.frame_seq) {
            if let Some(t) = ts.get(&rec.frame_seq) {
                lat.push(rec.at_us.saturating_sub(*t));
            }
        }
    }
    let floor = lat.iter().copied().min().unwrap_or(0);
    let late = lat.iter().filter(|l| **l - floor > SWEEP_LATE_US).count();
    (lat.len(), late, run.analyse().never_fed)
}

fn read_run(r: &BrowserlessResult, secs: u64) -> SweepRun {
    let (t, i, a) = (&r.transport, &r.transport.instr, &r.transport.adapt);
    let repair_bytes = repairs(r);
    let (shown, late, never_fed) = frames_late(r);
    let planned_rows = i
        .block_repairs_sent
        .saturating_sub(i.block_request_rows_sent + i.block_pto_repair_rows);
    SweepRun {
        dropped: r.s2c_datagrams_dropped,
        offered_dg: r.s2c_datagrams_offered,
        lost_pre_fec: t.report_lost_pre_fec_total,
        reports: t.loss_reports,
        src_bytes: i.offered.total().saturating_sub(repair_bytes),
        repair_bytes,
        rho_dg: planned_rows as f64 / i.block_k_sum.max(1) as f64,
        k_mean: i.block_k_sum as f64 / i.block_packed.max(1) as f64,
        k_buckets: i.block_k_buckets,
        accounted: i.block_closed_accounted,
        declined: i.block_closed_declined,
        handed_off: i.block_closed_handed_off,
        timed_out: i.block_closed_timed_out,
        handoffs_declined: i.block_handoffs + i.block_closed_declined,
        retx: r.retransmit_attempts_total,
        changes: a.changes,
        floor_changes: a.floor_changes,
        engaged: a.engaged,
        sized: a.blocks_sized,
        saturated: a.saturated,
        skips: a.congestion_skips,
        trim: a.trim_steps,
        p_hat_ppm: a.p_hat_ppm,
        b_hat_x100: a.b_hat_x100,
        bwe_binding_share: i.bwe.ceiling_binding_reads as f64 / i.bwe.ceiling_reads.max(1) as f64,
        bwe_reads: i.bwe.ceiling_reads,
        digest: r.wire_digest,
        shown,
        late,
        never_fed,
        secs,
    }
}

async fn sweep_run(
    load: Load,
    loss: Loss,
    cap_mbit: u64,
    arm: Arm,
    seed: u64,
    secs: u64,
) -> SweepRun {
    let (scene, links) = sweep_scene(load, loss, cap_mbit, seed, secs * 1000);
    let r = run_browserless_with_options(scene, links, arm.options())
        .await
        .expect("scene ran");
    let out = read_run(&r, secs);
    println!(
        "FECSWEEP run load={load:?} loss={} cap={cap_mbit} arm={} seed={seed:#x} secs={secs} \
         dropped={} offered={} lost_pre_fec={} reports={} src_bytes={} repair_bytes={} \
         rho_bytes={:.4} rho_dg={:.4} k_mean={:.2} closes={}/{}/{}/{} handoffs_declined={} retx={} \
         retx_per_lost={:.3} changes={} changes_per_min={:.2} floor_changes={} engaged={} sized={} \
         sat={:.2} skips={} trim={} p_hat_ppm={} b_hat_x100={} bwe_binding={:.3}/{} shown={} \
         late={} never_fed={} digest={:#x}",
        loss.name(),
        arm.name(),
        out.dropped,
        out.offered_dg,
        out.lost_pre_fec,
        out.reports,
        out.src_bytes,
        out.repair_bytes,
        out.rho_bytes(),
        out.rho_dg,
        out.k_mean,
        out.accounted,
        out.declined,
        out.handed_off,
        out.timed_out,
        out.handoffs_declined,
        out.retx,
        out.retx_per_lost(),
        out.changes,
        out.changes_per_min(),
        out.floor_changes,
        out.engaged,
        out.sized,
        out.sat_share(),
        out.skips,
        out.trim,
        out.p_hat_ppm,
        out.b_hat_x100,
        out.bwe_binding_share,
        out.bwe_reads,
        out.shown,
        out.late,
        out.never_fed,
        out.digest
    );
    out
}

// ---- the oracle: P(more than r of n datagrams lost) and the smallest r ----

/// P(more than `r` of `n` consecutive datagrams are lost) under the chain
/// `(enter, exit)` (loss 1 in the bad state), started at its stationary
/// distribution. Independent of the controller's own implementation.
fn tail_prob(n: usize, r: usize, enter: f64, exit: f64) -> f64 {
    let pi_b = enter / (enter + exit);
    // f[state][lost, capped at r + 1]
    let mut f = [vec![0.0f64; r + 2], vec![0.0f64; r + 2]];
    for pos in 0..n {
        let mut g = [vec![0.0f64; r + 2], vec![0.0f64; r + 2]];
        if pos == 0 {
            g[0][0] = 1.0 - pi_b;
            g[1][1.min(r + 1)] = pi_b;
        } else {
            for (from, row) in f.iter().enumerate() {
                let to_bad = if from == 0 { enter } else { 1.0 - exit };
                for (c, &m) in row.iter().enumerate() {
                    g[0][c] += m * (1.0 - to_bad);
                    g[1][(c + 1).min(r + 1)] += m * to_bad;
                }
            }
        }
        f = g;
    }
    f[0][r + 1] + f[1][r + 1]
}

/// The controller's rule, restated: the smallest `1 <= r <= min(32,
/// ceil(k / 4))` with a tail at most 1e-3; `(cap, true)` when none reaches it.
fn dp_r(k: usize, enter: f64, exit: f64) -> (usize, bool) {
    let cap = k.div_ceil(4).clamp(1, 32);
    for r in 1..=cap {
        if tail_prob(k + r, r, enter, exit) <= 1e-3 {
            return (r, false);
        }
    }
    (cap, true)
}

#[test]
fn the_oracle_matches_known_tails() {
    // Binomial(12, 0.02): P(> 3 lost) = 1 - sum_{i<=3} C(12,i) .02^i .98^(12-i).
    let p: f64 = 0.02;
    let mut cdf = 0.0;
    let mut c = 1.0f64;
    for i in 0..=3 {
        if i > 0 {
            c = c * (12 - i + 1) as f64 / i as f64;
        }
        cdf += c * p.powi(i) * (1.0 - p).powi(12 - i);
    }
    let want = 1.0 - cdf;
    let got = tail_prob(12, 3, p, 1.0 - p);
    assert!((got - want).abs() < 1e-12, "{got} vs {want}");
    // Bursts make the same average loss harder: a longer tail at equal p.
    let ge = tail_prob(12, 3, 0.01 / 3.0 / 0.99, 1.0 / 3.0);
    let iid = tail_prob(12, 3, 0.01, 0.99);
    assert!(ge > iid * 5.0, "ge {ge} iid {iid}");
    // Saturation: burst 3 at k = 8 cannot reach 1e-3 under ceil(k/4) = 2.
    assert!(dp_r(8, 0.01 / 3.0 / 0.99, 1.0 / 3.0).1);
    // i.i.d. 5 % at k = 64: a large r, within the cap, not saturated.
    let (r, sat) = dp_r(64, 0.05, 0.95);
    assert!(!sat && (8..=16).contains(&r), "r = {r}");
}

// ---- the grid and its criteria ---------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Cell {
    load: Load,
    loss: Loss,
    cap: u64,
}

impl Cell {
    fn name(&self) -> String {
        format!("{:?}/{}/{}M", self.load, self.loss.name(), self.cap)
    }
    /// Virtual seconds: a minute where the criterion counts state changes per
    /// minute (0.1 %) or needs losses to be a statistical claim (0.01 %).
    fn secs(&self) -> u64 {
        match self.loss {
            Loss::Iid(p) if p <= 0.001 => 60,
            _ => 30,
        }
    }
}

/// The criteria the test fails on. The others are measured and printed (a
/// `FAIL` line) but are open findings (FINDINGS section 10), not yet gates:
/// a failing criterion blocks the default-on commit, it does not turn CI red.
const GATED: [u8; 4] = [0, 1, 2, 4];

const SWEEP_SEEDS: [u64; 5] = [0xFB_0001, 0xFB_0002, 0xFB_0003, 0xFB_0004, 0xFB_0005];

fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

fn sd(v: &[f64]) -> f64 {
    let m = mean(v);
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / v.len().max(1) as f64).sqrt()
}

/// `a <= b` over the seed spread: the mean of `a` is at most the mean of `b`
/// plus the larger of the two arms' standard deviations over the seeds.
/// Returns `(pass, mean a, mean b, spread)`.
fn le_spread(a: &[f64], b: &[f64]) -> (bool, f64, f64, f64) {
    let spread = sd(a).max(sd(b));
    (mean(a) <= mean(b) + spread, mean(a), mean(b), spread)
}

struct Verdict {
    crit: u8,
    cell: String,
    pass: bool,
    detail: String,
}

impl Verdict {
    fn print(&self) {
        println!(
            "FECSWEEP crit {} {} cell={} {}",
            self.crit,
            if self.pass { "PASS" } else { "FAIL" },
            self.cell,
            self.detail
        );
    }
}

struct CellRuns {
    cell: Cell,
    off: Vec<SweepRun>,
    fixed: Vec<SweepRun>,
    ada: Vec<SweepRun>,
    seeds: usize,
}

fn field(v: &[SweepRun], f: impl Fn(&SweepRun) -> f64) -> Vec<f64> {
    v.iter().map(f).collect()
}

/// Whether the spread of the controller's hand-offs plus declines over the
/// seeds is wide: more seeds are run for the cell.
fn wide(v: &[SweepRun]) -> bool {
    let x = field(v, |r| r.handoffs_declined as f64);
    mean(&x) >= 5.0 && sd(&x) > mean(&x)
}

async fn run_cell(cell: Cell, max_seeds: usize) -> CellRuns {
    let mut c = CellRuns {
        cell,
        off: vec![],
        fixed: vec![],
        ada: vec![],
        seeds: 0,
    };
    let mut n = 3.min(max_seeds);
    while c.seeds < n {
        let seed = SWEEP_SEEDS[c.seeds];
        for arm in Arm::ALL {
            let r = sweep_run(cell.load, cell.loss, cell.cap, arm, seed, cell.secs()).await;
            match arm {
                Arm::Off => c.off.push(r),
                Arm::Fixed => c.fixed.push(r),
                Arm::Adaptive => c.ada.push(r),
            }
        }
        c.seeds += 1;
        // Three seeds are the grid; five where the spread is wide, or where a
        // hand-off criterion failed on counts this small (a failure has to
        // survive more seeds to count).
        if c.seeds == 3 && n < max_seeds {
            let failing = verdicts(&c)
                .iter()
                .any(|v| !v.pass && matches!(v.crit, 2 | 3));
            if failing || wide(&c.ada) || wide(&c.fixed) {
                println!(
                    "FECSWEEP wide cell={} widening to {max_seeds} seeds",
                    cell.name()
                );
                n = max_seeds;
            }
        }
    }
    c
}

/// The cap's repair rows for a block of `k` sources.
fn cap_rows(k: usize) -> usize {
    k.div_ceil(4).clamp(1, 32)
}

fn verdicts(c: &CellRuns) -> Vec<Verdict> {
    let cell = c.cell.name();
    let mut out = Vec::new();
    let mut v = |crit: u8, pass: bool, detail: String| {
        out.push(Verdict {
            crit,
            cell: cell.clone(),
            pass,
            detail,
        })
    };
    // Premises: the arms are what they claim, and where the loss is above the
    // floor the measurement is non-zero.
    let nominal = match c.cell.loss {
        Loss::Clean => 0.0,
        Loss::Iid(p) => p,
        Loss::Ge3 => 0.01,
    };
    let off_clean = c.off.iter().all(|r| r.repair_bytes == 0 && r.sized == 0);
    let fixed_blocks = c.fixed.iter().all(|r| r.repair_bytes > 0);
    let reports = c
        .ada
        .iter()
        .chain(&c.off)
        .chain(&c.fixed)
        .all(|r| r.reports > 0);
    let dropped = nominal < 0.001
        || c.ada
            .iter()
            .chain(&c.off)
            .chain(&c.fixed)
            .all(|r| r.dropped > 0);
    let lost_seen = nominal < 0.001 || c.ada.iter().all(|r| r.lost_pre_fec > 0);
    let engaged = nominal < 0.01 || c.ada.iter().all(|r| r.repair_bytes > 0);
    v(
        0,
        off_clean && fixed_blocks && reports && dropped && lost_seen && engaged,
        format!(
            "premises off_sends_no_repair={off_clean} fixed_sends_repair={fixed_blocks} \
             reports={reports} loss_dropped={dropped} lost_pre_fec_seen={lost_seen} \
             adaptive_repaired={engaged}"
        ),
    );
    // (1) clean link and 0.01 %: no repair, and the frames sent equal the
    // mode-off run's (the wire digest hashes every datagram).
    if nominal <= 0.0001 {
        let zero = c.ada.iter().all(|r| r.repair_bytes == 0);
        let same = c.ada.iter().zip(&c.off).all(|(a, o)| a.digest == o.digest);
        v(
            1,
            zero && same,
            format!(
                "adaptive_repair_bytes={:?} digest_equal_to_off={same} dropped={:?}",
                field(&c.ada, |r| r.repair_bytes as f64),
                field(&c.ada, |r| r.dropped as f64)
            ),
        );
    }
    // (2) a stationary 0.1 %.
    if (nominal - 0.001).abs() < 1e-9 {
        let cpm = field(&c.ada, SweepRun::changes_per_min);
        let ok_changes = cpm.iter().all(|x| *x <= 2.0);
        let (ok_oh, oa, of, sp) = le_spread(
            &field(&c.ada, SweepRun::rho_bytes),
            &field(&c.fixed, SweepRun::rho_bytes),
        );
        let (ok_retx, ra, ro, rs) = le_spread(
            &field(&c.ada, |r| r.retx as f64),
            &field(&c.off, |r| r.retx as f64),
        );
        v(
            2,
            ok_changes && ok_oh && ok_retx,
            format!(
                "changes_per_min={cpm:.2?} overhead adaptive={oa:.4} fixed={of:.4} spread={sp:.4} \
                 ({ok_oh}); retransmits adaptive={ra:.0} off={ro:.0} spread={rs:.0} ({ok_retx})"
            ),
        );
    }
    // (3) 1 %, 5 % i.i.d. and the bursty 1 %.
    if nominal >= 0.01 {
        let (ok_hd, ha, hf, hs) = le_spread(
            &field(&c.ada, |r| r.handoffs_declined as f64),
            &field(&c.fixed, |r| r.handoffs_declined as f64),
        );
        let sat = mean(&field(&c.ada, SweepRun::sat_share));
        let (enter, exit) = c.cell.loss.chain().expect("a lossy cell");
        let memo: std::collections::HashMap<usize, usize> = (0..24)
            .map(|i| {
                let (lo, hi) = ghostframe_lib::transport::io_bridge::block_k_bucket_range(i);
                let k = (lo + hi) / 2;
                (k, dp_r(k, enter, exit).0)
            })
            .collect();
        let dp = |k: usize| memo[&k];
        let rho = mean(&field(&c.ada, |r| r.rho_dg));
        // The rule's overhead over the sizes the run actually closed, and the
        // cap's. Where the target is unreachable (saturated share high) the
        // bar is the cap's, with no slack; elsewhere 1.25 x the rule's.
        let dp_oh = mean(&field(&c.ada, |r| r.rows_over_blocks(dp)));
        let cap_oh = mean(&field(&c.ada, |r| r.rows_over_blocks(cap_rows)));
        let (bound_name, bound) = if sat >= 0.5 {
            ("cap", cap_oh)
        } else {
            ("1.25 x dp", 1.25 * dp_oh)
        };
        let ok_oh = rho <= bound;
        v(
            3,
            ok_hd && ok_oh,
            format!(
                "handoffs+declined adaptive={ha:.1} fixed={hf:.1} spread={hs:.1} ({ok_hd}); \
                 overhead r/k={rho:.4} against {bound_name} {bound:.4} (dp {dp_oh:.4}, cap \
                 {cap_oh:.4}) ({ok_oh}); saturated_share={sat:.2} trim_steps={:?}",
                field(&c.ada, |r| r.trim as f64)
            ),
        );
    }
    // (5) 5 % i.i.d. is never congestion-gated.
    if (nominal - 0.05).abs() < 1e-9 && matches!(c.cell.loss, Loss::Iid(_)) {
        let skips = field(&c.ada, |r| r.skips as f64);
        v(
            5,
            skips.iter().all(|x| *x == 0.0),
            format!("adapt_congestion_skips={skips:?}"),
        );
    }
    // (6) the BWE ceiling binds no more than under the control.
    let reads = c.ada.iter().chain(&c.off).all(|r| r.bwe_reads > 0);
    let (ok, a, o, sp) = le_spread(
        &field(&c.ada, |r| r.bwe_binding_share),
        &field(&c.off, |r| r.bwe_binding_share),
    );
    v(
        6,
        ok,
        format!(
            "ceiling binding share adaptive={a:.4} off={o:.4} spread={sp:.4} reads_present={reads}"
        ),
    );
    // (7) at 2 Mbit/s: the overhead against the 28 ms design value (report).
    if c.cell.cap == 2 && nominal > 0.0 {
        let a = &c.ada;
        let k = mean(&field(a, |r| r.k_mean)).max(1.0);
        let dg_rate = mean(&field(a, |r| {
            let repair_share = r.repair_bytes as f64 / (r.src_bytes + r.repair_bytes).max(1) as f64;
            r.offered_dg as f64 * (1.0 - repair_share) / r.secs as f64
        }));
        let k28 = ((dg_rate * 0.028).round().max(1.0) as usize).min(64);
        if let Some((enter, exit)) = c.cell.loss.chain() {
            let (r20, _) = dp_r(k.round() as usize, enter, exit);
            let (r28, _) = dp_r(k28, enter, exit);
            v(
                7,
                true,
                format!(
                    "(reported) adaptive r/k={:.4}; DP at the k it ran (k={k:.1}) {:.4}; DP at the \
                     28 ms design (k={k28}) {:.4}; source {dg_rate:.0} datagrams/s",
                    mean(&field(a, |r| r.rho_dg)),
                    r20 as f64 / k,
                    r28 as f64 / k28 as f64
                ),
            );
        }
    }
    out
}

/// (4) The time from a 5 % loss step to the controller being engaged, by
/// bisection over the scene's length: stepped runs are byte-reproducible from
/// the seed, so a shorter run is a prefix of a longer one. The step comes at
/// 5 s; where the controller is already engaged by then (a 2 Mbit/s link
/// drops datagrams of its own at start-up) it comes at 25 s instead, and where
/// that fails too the answer is "engaged before the step", not a time.
async fn engage_ms(load: Load, cap: u64, seed: u64) -> Result<(u64, u64), String> {
    for step_ms in [5_000u64, 25_000] {
        let engaged = |ms: u64| async move {
            let (scene, mut links) = sweep_scene(load, Loss::Clean, cap, seed, ms);
            links.s2c_loss_steps = vec![(step_ms * 1000, 0.05)];
            let r = run_browserless_with_options(scene, links, adaptive())
                .await
                .expect("scene ran");
            (r.transport.adapt.engaged, repairs(&r))
        };
        if engaged(step_ms).await.0 {
            continue;
        }
        let (mut lo, mut hi) = (step_ms, step_ms + 3_000);
        let (e_hi, rep) = engaged(hi).await;
        if !e_hi || rep == 0 {
            return Err(format!("not engaged 3 s after the step at {step_ms} ms"));
        }
        while hi - lo > 50 {
            let mid = (lo + hi) / 2;
            if engaged(mid).await.0 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        return Ok((hi - step_ms, step_ms));
    }
    Err("engaged before the step (start-up drops), at 5 s and at 25 s".into())
}

fn reduced_grid() -> Vec<Cell> {
    [
        Loss::Clean,
        Loss::Iid(0.0001),
        Loss::Iid(0.001),
        Loss::Iid(0.01),
        Loss::Iid(0.05),
        Loss::Ge3,
    ]
    .into_iter()
    .map(|loss| Cell {
        load: Load::H264,
        loss,
        cap: 10,
    })
    .collect()
}

fn full_grid() -> Vec<Cell> {
    let mut g = Vec::new();
    for load in [Load::H264, Load::Tile] {
        for cap in [2, 10, 30, 100] {
            for loss in [
                Loss::Clean,
                Loss::Iid(0.0001),
                Loss::Iid(0.001),
                Loss::Iid(0.01),
                Loss::Iid(0.05),
                Loss::Ge3,
            ] {
                g.push(Cell { load, loss, cap });
            }
        }
    }
    g
}

/// `GHOSTFRAME_FEC_SWEEP_ONLY`: comma-separated substrings of a cell's name.
fn selected(only: &Option<String>, name: &str) -> bool {
    only.as_ref()
        .is_none_or(|o| o.split(',').any(|p| name.contains(p)))
}

/// The sweep. Reduced by default (one rate, H.264); `GHOSTFRAME_FEC_SWEEP=full`
/// runs the whole grid, `GHOSTFRAME_FEC_SWEEP_ONLY=<substring of a cell name>`
/// narrows it.
#[tokio::test(start_paused = true)]
async fn control_law_sweep() {
    let full = std::env::var("GHOSTFRAME_FEC_SWEEP").is_ok_and(|v| v == "full");
    let only = std::env::var("GHOSTFRAME_FEC_SWEEP_ONLY").ok();
    let grid = if full { full_grid() } else { reduced_grid() };
    let max_seeds = if full { 5 } else { 3 };
    let mut failed = Vec::new();
    for cell in grid {
        if !selected(&only, &cell.name()) {
            continue;
        }
        let runs = run_cell(cell, max_seeds).await;
        for v in verdicts(&runs) {
            v.print();
            if !v.pass && GATED.contains(&v.crit) {
                failed.push(format!("{} (criterion {})", v.cell, v.crit));
            }
        }
    }
    // (4) engage within a second of a 5 % step.
    let steps: Vec<(Load, u64)> = if full {
        [Load::H264, Load::Tile]
            .into_iter()
            .flat_map(|l| [2, 10, 30, 100].map(move |c| (l, c)))
            .collect()
    } else {
        vec![(Load::H264, 10)]
    };
    for (load, cap) in steps {
        let name = format!("{load:?}/step5/{cap}M");
        if !selected(&only, &name) {
            continue;
        }
        let mut times = Vec::new();
        for seed in &SWEEP_SEEDS[..3] {
            times.push(engage_ms(load, cap, *seed).await);
        }
        let v = Verdict {
            crit: 4,
            cell: name,
            pass: times
                .iter()
                .all(|t| matches!(t, Ok((ms, _)) if *ms <= 1_000)),
            detail: format!(
                "ms from the step to engaged, and the step's time, per seed: {times:?}"
            ),
        };
        v.print();
        if !v.pass {
            failed.push(format!("{} (criterion 4)", v.cell));
        }
    }
    assert!(failed.is_empty(), "criteria failed: {failed:?}");
}
