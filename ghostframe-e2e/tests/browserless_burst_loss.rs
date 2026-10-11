//! Does the retransmit machinery treat burst loss differently from i.i.d.
//! loss at the same long-run rate?
//!
//! Three runs of the same sustained tile load: a clean control, ~2% s2c i.i.d.
//! loss, and ~2% s2c Gilbert-Elliott loss with mean burst 8 datagrams. Loss is
//! on the server -> client direction only.
//!
//! These scenes REPORT (println!, use `--nocapture`); they do not assert
//! timing or recovery thresholds. They assert only premises that make the
//! report meaningful: the session came up, the lossy runs dropped something,
//! the burst run's measured mean loss run is clearly longer than the iid
//! run's, and the control dropped nothing.
//!
//! Harness caveats: only `NetSim::decide` is seed-reproducible, not the scene
//! (see `netsim/mod.rs`), so counters vary run to run; the virtual clock is
//! paused (a healthy scene is ~350 loop iterations per virtual second).

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_links, BrowserlessResult, BrowserlessScene, LinkOverrides, SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::netsim::{GilbertElliott, NetProfile};

const COLS: u32 = 16;
const ROWS: u32 = 16;
const RATE: f64 = 0.02;
const BURST: f64 = 8.0;

fn scene(seed: u64, secs: u64) -> BrowserlessScene {
    BrowserlessScene {
        seed,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region { tiles_per_tick: 32 },
        }),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: COLS,
        grid_rows: ROWS,
    }
}

/// Loss on the s2c direction only; c2s keeps the scene's clean profile.
fn s2c_only(mutate: impl FnOnce(&mut NetProfile)) -> LinkOverrides {
    let mut p = scene(0, 1).net;
    mutate(&mut p);
    LinkOverrides {
        s2c: Some(p),
        ..Default::default()
    }
}

fn tiles_present(r: &BrowserlessResult) -> usize {
    (0..COLS as u8)
        .flat_map(|x| (0..ROWS as u8).map(move |y| (x, y)))
        .filter(|&(x, y)| r.framebuffer.tile_rgba(x, y).is_some())
        .count()
}

fn report(name: &str, r: &BrowserlessResult) {
    let ratio = if r.s2c_datagrams_dropped == 0 {
        0.0
    } else {
        r.retransmit_attempts_total as f64 / r.s2c_datagrams_dropped as f64
    };
    println!(
        "BURSTREPORT {name}: offered={} dropped={} realised_loss={:.4} loss_runs={} \
         mean_run={:.3} max_run={} retransmit_attempts_total={} nack_hit={} nack_miss={} \
         rto_fired={} retx_per_drop={:.3} tiles_present={}/{} stale={} \
         s2c_bytes={} s2c_dg={} total_bytes={}",
        r.s2c_datagrams_offered,
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_dropped as f64 / r.s2c_datagrams_offered.max(1) as f64,
        r.s2c_loss_runs,
        r.s2c_mean_loss_run(),
        r.s2c_loss_run_max,
        r.retransmit_attempts_total,
        r.nack_hit,
        r.nack_miss,
        r.rto_fired,
        ratio,
        tiles_present(r),
        COLS * ROWS,
        r.stale_generation_tiles,
        r.bytes_delivered_s2c,
        r.datagrams_delivered_s2c,
        r.bytes_delivered,
    );
}

fn assert_established(name: &str, r: &BrowserlessResult) {
    assert!(
        r.events.contains(&ClientNetEvent::SessionReady),
        "{name}: session never established"
    );
    assert!(r.s2c_datagrams_offered > 0, "{name}: no s2c traffic at all");
}

#[tokio::test(start_paused = true)]
async fn iid_versus_burst_loss_at_equal_average_rate() {
    const SECS: u64 = 12;
    let seed = 0xB0_B057;

    let clean = run_browserless_with_links(scene(seed, SECS), LinkOverrides::default())
        .await
        .expect("scene ran");
    report("clean", &clean);

    let iid = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| p.loss = RATE))
        .await
        .expect("scene ran");
    report("iid2%", &iid);

    let ge = GilbertElliott::from_rate_and_burst(RATE, BURST, 1.0).expect("feasible");
    let burst = run_browserless_with_links(scene(seed, SECS), s2c_only(|p| ge.apply_to(p)))
        .await
        .expect("scene ran");
    report("burst2%_len8", &burst);

    for (n, r) in [("clean", &clean), ("iid", &iid), ("burst", &burst)] {
        assert_established(n, r);
    }
    assert_eq!(
        clean.s2c_datagrams_dropped, 0,
        "control link must not drop anything"
    );
    assert!(iid.s2c_datagrams_dropped > 0, "iid run dropped nothing");
    assert!(burst.s2c_datagrams_dropped > 0, "burst run dropped nothing");
    assert!(
        burst.s2c_mean_loss_run() > iid.s2c_mean_loss_run() * 2.0,
        "measurement must move with the model: iid mean run {:.2}, burst {:.2}",
        iid.s2c_mean_loss_run(),
        burst.s2c_mean_loss_run()
    );
}
