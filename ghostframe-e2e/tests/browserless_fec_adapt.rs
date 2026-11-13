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
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, NetProfile};
use ghostframe_lib::encoder::h264_harness::PcmMode;
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
