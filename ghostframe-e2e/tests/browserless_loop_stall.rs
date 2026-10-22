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
//! These scenes run in `HarnessMode::Concurrent`: starvation of the loop is a
//! concurrency effect, and the default stepped harness writes and reads at one
//! virtual instant, so every delay it reports is 0 by construction and the
//! assertions below could not fail. The tests assert the delays are non-zero
//! as a premise, and a mutation (holding the frame inline in the injected arm)
//! was checked to fail them.
//!
//! Two properties. An idle loop serves packets within the harness floor. And a
//! frame's hold does not delay them: the injected arm hands a frame with a
//! `cost` to a task, like the real frame work now runs on a worker thread, so
//! a packet written early in a hold is still served promptly. (Before frame
//! work left the loop this scene pinned the opposite: every packet written
//! inside a hold waited out the rest of it, 1527 of 1527.) The hold is applied
//! in the injected arm, not in `process_frame`, so this scene shows the
//! harness model, and the real-clock `c2_frame_off_loop` test shows the
//! production code.

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, HarnessMode, LinkOverrides,
    RunOptions, SceneLoad,
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
        RunOptions {
            frame_cost: cost,
            // Event-loop starvation is a concurrency effect: stepped, the
            // harness writes and the bridge reads at the same virtual
            // instant and every delay is 0 by construction.
            mode: HarnessMode::Concurrent,
        },
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
    // Non-vacuity: under the stepped harness every delay is exactly 0 (the
    // write and the read share one virtual instant), so the bound below could
    // not fail. A concurrent run measures real scheduling gaps.
    assert!(
        percentile(d.clone(), 1.0) > 0,
        "premise: the delays are measured, not zero by construction ({})",
        describe(&d)
    );
    let p99 = percentile(d.clone(), 0.99);
    assert!(p99 < 15_000, "idle p99 {p99} us is above the harness floor");
}

#[tokio::test(start_paused = true)]
async fn a_frame_held_off_the_loop_does_not_delay_the_packets_that_arrive_meanwhile() {
    let r = run(Some(COST)).await;
    // Windows come from the bridge: when a frame's hold really ran.
    assert!(
        r.frame_cost_windows.len() > 20,
        "premise: frames were held ({} windows)",
        r.frame_cost_windows.len()
    );
    // Held inline (this scene's baseline, before frame work left the loop)
    // the same scene had p99 about 19 ms against the idle run's 7 ms and a
    // median of 8 against 3: every packet written inside a hold waited out
    // the rest of it. Off the loop it must look like the idle run.
    let idle = run(None).await;
    let idle_d: Vec<u64> = steady(&idle).iter().map(|x| x.1).collect();
    let d: Vec<u64> = steady(&r).iter().map(|x| x.1).collect();
    println!("METRIC idle {}", describe(&idle_d));
    println!("METRIC held {}", describe(&d));
    assert!(
        d.len() > 500,
        "premise: packets while frames were held ({})",
        d.len()
    );
    assert!(
        percentile(d.clone(), 1.0) > 0 && percentile(idle_d.clone(), 1.0) > 0,
        "premise: the delays are measured, not zero by construction"
    );
    let slack_us = 3_000;
    for (name, p) in [("p50", 0.5), ("p90", 0.9), ("p99", 0.99)] {
        let (h, i) = (percentile(d.clone(), p), percentile(idle_d.clone(), p));
        assert!(
            h <= i + slack_us,
            "{name} with frames held is {h} us against {i} us idle: packets waited for the hold"
        );
    }
}
