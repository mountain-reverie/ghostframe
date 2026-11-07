//! The overload scene in the harness: the shape of
//! `e2e_saturated_link_starves_tiles_into_giving_up`, deterministic.
//!
//! A 60x34 screen (2040 tiles) whose every tile changes four times a second,
//! behind a 2 Mbit link with 1 % loss. Each change is megabytes of passes
//! against a link moving 250 kB/s, so the screen can never complete. What the
//! scene gates is that the scheduler is *offered* the link: the bytes the
//! server puts on the wire over the run are at least 80 % of what the
//! bandwidth estimate allowed in that time. (Recorded 2026-10 as 6 %, from a
//! budget granted once per frame; it is billed by elapsed time now, and this
//! pins it.)
//!
//! How the bytes are spread over the screen is a scheduler property, tested
//! in `scheduler.rs` (`sustained_supersede_still_reaches_every_tile`). It is
//! not visible here: the client emits no event for a lone pass 0.
//!
//! Every number prints as `OVERLOAD <seed> <name> <value>` before any assert.
//!
//! Run: `cargo test --release --test browserless_overload -- --nocapture
//! --test-threads=1`

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, FrameScript, LinkOverrides,
    RunOptions, SceneLoad, OFFERED_BIN_US,
};
use ghostframe_e2e::harness::load_profile::{gradient_tile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};

const COLS: u8 = 60;
const ROWS: u8 = 34;
const SECS: u64 = 20;
/// 2 Mbit: a full change is far beyond what the link carries between changes,
/// even for pass 0 alone (~300 kB for the screen against ~60 kB a change).
const LINK_BYTES_PER_S: u64 = 250_000;
/// A change every 8 frames of 33.3 ms: four a second.
const FRAMES_PER_CHANGE: u64 = 8;

fn frames() -> Vec<FrameScript> {
    let n = SECS * 1_000_000 / PRODUCTION_CADENCE_US;
    (0..n)
        .map(|i| {
            if i % FRAMES_PER_CHANGE != 0 {
                return FrameScript { tiles: Vec::new() };
            }
            let shift = (i / FRAMES_PER_CHANGE) as u32 * 7 + 1;
            FrameScript {
                tiles: (0..COLS)
                    .flat_map(|x| {
                        (0..ROWS).map(move |y| {
                            (
                                (x, y),
                                TileSpec::Cdf53 {
                                    bgra: gradient_tile(shift, x, y),
                                },
                            )
                        })
                    })
                    .collect(),
            }
        })
        .collect()
}

struct Measured {
    r: BrowserlessResult,
    /// Bytes offered over the run / bytes the estimate said were available.
    offered_share: f64,
}

async fn run(seed: u64) -> Measured {
    let scene = BrowserlessScene {
        seed,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(frames()),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            loss: 0.01,
            delay_us: 10_000,
            cap: CapTimeline::constant(LINK_BYTES_PER_S),
            bottleneck: Some(Bottleneck::wifi()),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(SECS),
        grid_cols: u32::from(COLS),
        grid_rows: u32::from(ROWS),
    };
    let r = run_browserless_with_options(scene, LinkOverrides::default(), RunOptions::default())
        .await
        .expect("scene ran");
    let offered: u64 = r.s2c_offered_bins.iter().map(|&b| u64::from(b)).sum();
    // Samples are every >= 100 ms of virtual time; integrate the step function.
    let mut avail = 0.0f64;
    for w in r.bwe_estimate_samples.windows(2) {
        avail += w[0].1 as f64 / 8.0 * (w[1].0 - w[0].0) as f64 / 1e6;
    }
    let measured = Measured {
        offered_share: offered as f64 / avail.max(1.0),
        r,
    };
    let m = &measured;
    println!(
        "OVERLOAD {seed:#x} offered_bytes={offered} avail_est_bytes={avail:.0} offered_share={:.3} \
         s2c_delivered={} est_final_bps={} bin_us={OFFERED_BIN_US}",
        m.offered_share, m.r.bytes_delivered_s2c, m.r.bwe_estimate_bps
    );
    measured
}

#[tokio::test(start_paused = true)]
async fn the_scheduler_is_offered_the_link_not_a_slice_per_frame() {
    for seed in [0xE4_0001u64, 0xE4_0002, 0xE4_0003] {
        let m = run(seed).await;
        assert!(m.r.bytes_delivered_s2c > 100_000, "premise: nothing moved");
        assert!(
            m.offered_share >= 0.8,
            "seed {seed:#x}: the server offered {:.0} % of the estimate",
            m.offered_share * 100.0
        );
    }
}
