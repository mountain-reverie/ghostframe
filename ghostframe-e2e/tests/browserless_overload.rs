//! The overload scene in the harness: the shape of
//! `e2e_saturated_link_starves_tiles_into_giving_up`, deterministic.
//!
//! A 60x34 screen (2040 tiles) whose every tile changes four times a second,
//! behind a 2 Mbit and a 1 Mbit link with 1 % loss. Pass 0 of the whole
//! screen alone is ~300 kB against ~60 kB (2 Mbit) or ~30 kB (1 Mbit) of link
//! between changes, so the screen can never complete. Two gates:
//!
//! * every tile has a pass 0 sent at some point. The drain used to restart at
//!   the same tile after every supersede: 857 (2 Mbit) and 736 (1 Mbit) of
//!   2040 tiles on the scheduler before `rotate`, all 2040 after. The count
//!   is `Scheduler::pass0_sent_handle`, a harness-only record.
//! * the link is used: delivered server -> client bytes are at least 80 % of
//!   the link over the run. (Recorded 2026-10 as a budget granted once per
//!   frame, 6 % of the link; it is billed by elapsed time now, and master
//!   delivers ~95 %.)
//!
//! Every number prints as `OVERLOAD ...` before any assert.
//!
//! Run: `cargo test --release --test browserless_overload -- --nocapture
//! --test-threads=1`

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, FrameScript, LinkOverrides,
    RunOptions, SceneLoad,
};
use ghostframe_e2e::harness::load_profile::{gradient_tile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};

const COLS: u8 = 60;
const ROWS: u8 = 34;
const SECS: u64 = 20;
const TILES: usize = COLS as usize * ROWS as usize;
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

async fn run(seed: u64, link_bytes_per_s: u64) -> BrowserlessResult {
    let scene = BrowserlessScene {
        seed,
        tick_budget_floor_bytes: None,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(frames()),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            loss: 0.01,
            delay_us: 10_000,
            cap: CapTimeline::constant(link_bytes_per_s),
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
    println!(
        "OVERLOAD {seed:#x} link={link_bytes_per_s} B/s tiles_with_pass0={}/{TILES} \
         s2c_delivered={} ({:.0} % of link) est_final_bps={}",
        r.tiles_with_pass0_sent,
        r.bytes_delivered_s2c,
        r.bytes_delivered_s2c as f64 * 100.0 / (link_bytes_per_s * SECS) as f64,
        r.bwe_estimate_bps
    );
    r
}

#[tokio::test(start_paused = true)]
async fn an_overloaded_link_is_used_and_spread_over_the_whole_screen() {
    let mut failures = Vec::new();
    for link in [250_000u64, 125_000] {
        for seed in [0xE4_0001u64, 0xE4_0002, 0xE4_0003] {
            let r = run(seed, link).await;
            if r.bytes_delivered_s2c * 10 < link * SECS * 8 {
                failures.push(format!(
                    "seed {seed:#x} link {link}: delivered {} of {} bytes",
                    r.bytes_delivered_s2c,
                    link * SECS
                ));
            }
            if r.tiles_with_pass0_sent != TILES {
                failures.push(format!(
                    "seed {seed:#x} link {link}: only {} of {TILES} tiles ever had a pass 0 sent",
                    r.tiles_with_pass0_sent
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}
