//! WS-C2 harness baseline: can a browserless scene SEE a frame that holds the
//! server's event loop?
//!
//! A real frame's cost is CPU time inside `IoBridge::process_frame`, which a
//! paused tokio clock cannot see (virtual time does not advance while the CPU
//! is busy), so the browserless scenes could not show the stall. Here a scene
//! frame carries a virtual `cost`: the loop awaits it inline before applying
//! the frame. That models "the loop is busy for `cost`" in virtual time while
//! the client and netsim tasks keep running, and
//! `BrowserlessResult::inbound_service_delays` reports how long each
//! client -> server packet then waits for the loop to take it.
//!
//! These two tests pin the BASELINE: an idle loop serves packets within the
//! harness floor, a packet written inside a hold waits out the rest of it.
//! Once frame processing leaves the loop the second property is the one that
//! must flip. The hold is applied in the injected arm, not in `process_frame`,
//! so moving `process_frame` does not move it: the flip has to move the hold
//! to wherever the frame work runs, or the test proves nothing about it.

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, LinkOverrides, RunOptions,
    SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::netsim::NetProfile;
use std::time::Duration;

const COST: Duration = Duration::from_millis(30);

async fn run(cost: Option<Duration>) -> BrowserlessResult {
    let secs = 6;
    let profile = LoadProfile {
        cadence_us: PRODUCTION_CADENCE_US,
        churn: Churn::Region { tiles_per_tick: 16 },
    };
    let frames = profile.frames_for(Duration::from_secs(secs), 16, 16);
    let scene = BrowserlessScene {
        seed: 0xC2C2_0001,
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
    };
    run_browserless_with_options(
        scene,
        LinkOverrides::default(),
        RunOptions { frame_cost: cost },
    )
    .await
    .expect("scene ran")
}

/// Packets written after the handshake settles; startup traffic is noise.
const WARMUP_US: u64 = 1_500_000;

fn steady(r: &BrowserlessResult) -> Vec<(u64, u64)> {
    r.inbound_service_delays
        .iter()
        .copied()
        .filter(|(t, _)| *t >= WARMUP_US)
        .collect()
}

fn describe(v: &[u64]) -> String {
    format!(
        "n={} p50={} p90={} p99={} max={}",
        v.len(),
        percentile(v.to_vec(), 0.5),
        percentile(v.to_vec(), 0.9),
        percentile(v.to_vec(), 0.99),
        percentile(v.to_vec(), 1.0)
    )
}

fn percentile(mut v: Vec<u64>, p: f64) -> u64 {
    v.sort_unstable();
    v[((v.len() - 1) as f64 * p) as usize]
}

/// The harness has a floor of several virtual ms between a write and the
/// bridge task reading it (paused-clock scheduling, not the bridge), so the
/// properties below are relative to a run without a cost, not absolute zero.
#[tokio::test(start_paused = true)]
async fn an_idle_loop_takes_inbound_packets_within_the_harness_floor() {
    let r = run(None).await;
    let d: Vec<u64> = steady(&r).iter().map(|x| x.1).collect();
    println!("METRIC idle {}", describe(&d));
    assert!(
        d.len() > 500,
        "premise: the client sent packets ({})",
        d.len()
    );
    assert!(r.frame_cost_windows.is_empty());
    let p99 = percentile(d.clone(), 0.99);
    assert!(p99 < 15_000, "idle p99 {p99} us is above the harness floor");
}

#[tokio::test(start_paused = true)]
async fn a_frame_that_holds_the_loop_delays_the_packets_that_arrive_meanwhile() {
    let r = run(Some(COST)).await;
    // Windows come from the bridge, not from the driver: they are when the
    // loop really sat in the hold.
    assert!(
        r.frame_cost_windows.len() > 20,
        "premise: the bridge held the loop ({} windows)",
        r.frame_cost_windows.len()
    );
    // A packet written inside a hold [start, end) cannot be taken before the
    // hold ends, whatever the scheduling floor: its delay is at least the
    // rest of the hold. Count how many honour that, within a small slack.
    let slack_us = 1_000;
    let (mut inside, mut honoured) = (0u32, 0u32);
    for &(t, delay) in steady(&r).iter() {
        if let Some(&(_, end)) = r.frame_cost_windows.iter().find(|&&(a, b)| t >= a && t < b) {
            inside += 1;
            if delay + slack_us >= end - t {
                honoured += 1;
            }
        }
    }
    println!(
        "METRIC held in_hold_packets={inside} waited_out_the_hold={honoured} (hold {} us)",
        COST.as_micros()
    );
    assert!(
        inside >= 200,
        "premise: packets written inside holds ({inside})"
    );
    assert!(
        honoured * 100 >= inside * 95,
        "{honoured} of {inside} packets written inside a hold waited out the rest of it"
    );
}
