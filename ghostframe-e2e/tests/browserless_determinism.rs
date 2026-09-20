//! Is the browserless harness actually a deterministic simulator?
//!
//! A simulator whose output depends on anything but its seed cannot be used
//! to bisect a protocol bug: a rerun that behaves differently is
//! indistinguishable from a fix. This file runs one scene twice in a single
//! process and compares what came out.
//!
//! Probe only — this exists to locate divergence, not yet to gate it.

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless, BrowserlessResult, BrowserlessScene, FrameScript, SceneLoad,
    DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::harness::load_profile::gradient_tile;
use ghostframe_e2e::netsim::NetProfile;

fn scene() -> BrowserlessScene {
    BrowserlessScene {
        seed: 0xDE7E_0001,
        load: SceneLoad::Script(vec![
            FrameScript {
                tiles: vec![
                    ((0, 0), TileSpec::Solid { bgra: [10, 20, 30, 255] }),
                    ((1, 1), TileSpec::Cdf53 { bgra: gradient_tile(0, 1, 1) }),
                ],
            },
            FrameScript {
                tiles: vec![(
                    (1, 1),
                    TileSpec::Cdf53 { bgra: gradient_tile(37, 1, 1) },
                )],
            },
        ]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_secs(3),
        grid_cols: 4,
        grid_rows: 4,
    }
}

/// Every scalar worth comparing, as (name, value), so a divergence names
/// itself instead of showing up as a bare `assert_eq` on a struct.
fn digest(r: &BrowserlessResult) -> Vec<(&'static str, u64)> {
    vec![
        ("events_len", r.events.len() as u64),
        ("bytes_delivered_s2c", r.bytes_delivered_s2c),
        ("bytes_delivered_c2s", r.bytes_delivered_c2s),
        ("bytes_dropped", r.bytes_dropped),
        ("stale_generation_tiles", r.stale_generation_tiles as u64),
        ("retransmit_attempts_total", r.retransmit_attempts_total),
        ("nack_hit", r.nack_hit),
        ("nack_miss", r.nack_miss),
        ("rto_fired", r.rto_fired),
        ("critical_latency_count", r.critical_latency_count),
        ("refinement_latency_count", r.refinement_latency_count),
        ("bwe_samples_seen", r.bwe_samples_seen),
        ("bwe_estimate_bps", r.bwe_estimate_bps),
        ("probes_completed", r.probes_completed),
        ("probes_abandoned", r.probes_abandoned),
    ]
}

#[tokio::test(start_paused = true)]
async fn the_same_seed_produces_the_same_scene() {
    let a = run_browserless(scene()).await.expect("run a");
    let b = run_browserless(scene()).await.expect("run b");

    let (da, db) = (digest(&a), digest(&b));
    let diffs: Vec<String> = da
        .iter()
        .zip(db.iter())
        .filter(|((_, x), (_, y))| x != y)
        .map(|((n, x), (_, y))| format!("  {n}: {x} != {y}"))
        .collect();

    eprintln!(
        "MEASURE s2c={} c2s={} crit={} refine={}",
        a.bytes_delivered_s2c,
        a.bytes_delivered_c2s,
        a.critical_latency_count,
        a.refinement_latency_count
    );
    assert!(
        diffs.is_empty(),
        "the same seed produced two different scenes:\n{}",
        diffs.join("\n")
    );
}

/// Narrows the divergence: a scene with no tiles at all exercises only the
/// QUIC/TLS handshake and the idle path. If the byte counts differ here,
/// the source is the handshake, not tile delivery.
#[tokio::test(start_paused = true)]
async fn an_empty_scene_is_byte_identical() {
    let empty = || BrowserlessScene {
        seed: 0xDE7E_0002,
        load: SceneLoad::Script(vec![]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_millis(800),
        grid_cols: 4,
        grid_rows: 4,
    };
    let a = run_browserless(empty()).await.expect("run a");
    let b = run_browserless(empty()).await.expect("run b");
    // Premise: a scene that delivered nothing is byte-identical for the
    // uninteresting reason. Without this the comparison below passes
    // whether or not a handshake ever happened.
    // A scene that delivered nothing is byte-identical for the uninteresting
    // reason. Measured: an empty scene leaves one direction at zero, so this
    // checks the combined counter -- the same premise
    // `bytes_actually_cross_the_socketpair` asserts.
    assert!(
        a.bytes_delivered > 0,
        "empty scene delivered nothing, so byte equality proves nothing \
         about handshake determinism"
    );
    assert_eq!(
        (a.bytes_delivered_s2c, a.bytes_delivered_c2s),
        (b.bytes_delivered_s2c, b.bytes_delivered_c2s),
        "handshake-only scene is not byte-reproducible"
    );
}
