//! Adaptive FEC (rev 6, R6-e): the controller's gate, end to end. Quinn's
//! `congestion_events` is loss under Cubic, so the gate reads the delay
//! signal (a standing queue) only: random loss engages and a clean link
//! stays off. (The gate itself, a standing queue skipping an interval, is a
//! bridge-level test in `block_tx_tests`: at a tail-drop bottleneck the queue
//! has often drained by the time the loss is reported, so a scene cannot
//! assert that every such interval is gated.)

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, RunOptions,
    SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::h264_scene::{pcm_load, Pattern};
use ghostframe_e2e::netsim::{DropPlan, NetProfile};
use ghostframe_lib::encoder::h264_harness::PcmMode;
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
    let scene = BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::H264(h264_load()),
        cadence_us: DEFAULT_CADENCE_US,
        net,
        drops: DropPlan::default(),
        duration: Duration::from_secs(10),
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
