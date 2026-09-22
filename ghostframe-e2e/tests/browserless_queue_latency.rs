//! How long does tile work sit in the scheduler before it reaches the wire?
//!
//! Production measurement, 2026-09-20, a 1920x1080 session at connect:
//!
//! | counter | value |
//! |---|---|
//! | `critical_latency_mean_us` (last_sent -> ACK) | 22,748 (22.7 ms) |
//! | `queued_critical_latency_mean_us` (queued -> ACK) | 2,289,529 (2.3 s) |
//! | `queued_critical_latency_max_us` | 16,844,729 (**16.8 s**) |
//!
//! Work spent roughly 100x longer waiting in the scheduler than it spent on
//! the wire. The first frame marks every tile dirty -- 2040 tiles, ~9200
//! Cdf53 passes -- and the whole burst is enqueued at once, so the tail of it
//! waits for the queue ahead of it to drain.
//!
//! The drain is pull-driven: `Event::DatagramsUnblocked` ->
//! `resume_scheduler_continuation`, "scheduler.tick runs when quinn signals
//! it has room, not when the next frame arrives" (`io_bridge.rs`). Each
//! resume is clamped to quinn's `datagram_send_buffer` capacity, which is
//! deliberate -- an unclamped `tick(usize::MAX)` against a first-frame burst
//! once overran that buffer and lost datagrams outright.
//!
//! That makes the *event rate*, not the bandwidth budget, the binding
//! constraint on how fast a backlog clears. Measured in production at ~580
//! datagrams/s sustained against a budget (`bytes_per_us=6.19`) permitting
//! roughly 25x more.
//!
//! These tests exist to make that behaviour observable in the harness rather
//! than only in a production journal. The harness runs the real
//! `IoBridge::run` loop, so the `DatagramsUnblocked` arm and the continuation
//! bookkeeping are the production ones; and `apply_injected_frame` drains
//! against `base_budget_bytes()`, the same bytes_per_us-derived budget the
//! capture path uses.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless, BrowserlessResult, BrowserlessScene, FrameScript, SceneLoad,
    DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::load_profile::gradient_tile;
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};

/// One frame that rewrites every tile in the grid, i.e. the shape of a
/// first frame: nothing is unchanged, so every tile is dirty at once.
fn full_grid_frame(cols: u8, rows: u8) -> FrameScript {
    FrameScript {
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
    }
}

fn burst_scene(cols: u8, rows: u8, secs: u64) -> BrowserlessScene {
    BrowserlessScene {
        seed: 0x0B0B_0001,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![full_grid_frame(cols, rows)]),
        cadence_us: DEFAULT_CADENCE_US,
        // No loss, no delay, no cap: anything the queue does here is the
        // scheduler's own pacing, not the link's.
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: u32::from(cols),
        grid_rows: u32::from(rows),
    }
}

/// The same burst over a capacity-limited path like production's.
///
/// `CapTimeline` is in **bytes** per second despite the parameter name --
/// see `docs/specs/bwe-googcc-review.md`'s measurement traps. 250_000 B/s
/// = 2 Mbps, the order of magnitude a tailnet/DERP path delivers.
fn capped_scene(cols: u8, rows: u8, cap_bytes_per_s: u64) -> BrowserlessScene {
    let mut scene = burst_scene(cols, rows, 30);
    scene.net = NetProfile {
        delay_us: 10_000,
        cap: CapTimeline::constant(cap_bytes_per_s),
        bottleneck: Some(Bottleneck::wifi()),
        ..NetProfile::perfect()
    };
    scene
}

fn ratio(queued: u64, wire: u64) -> f64 {
    if wire == 0 { 0.0 } else { queued as f64 / wire as f64 }
}

fn report(name: &str, r: &BrowserlessResult) {
    println!(
        "{name}: crit(queued_mean={} wire_mean={} ratio={:.2} max={} n={}) \
         refn(queued_mean={} wire_mean={} ratio={:.2} max={} n={}) \
         s2c_bytes={} s2c_dg={}",
        r.queued_critical_latency_mean_us,
        r.critical_latency_mean_us,
        ratio(r.queued_critical_latency_mean_us, r.critical_latency_mean_us),
        r.queued_critical_latency_max_us,
        r.queued_critical_latency_count,
        r.queued_refinement_latency_mean_us,
        r.refinement_latency_mean_us,
        ratio(r.queued_refinement_latency_mean_us, r.refinement_latency_mean_us),
        r.queued_refinement_latency_max_us,
        r.queued_refinement_latency_count,
        r.bytes_delivered_s2c,
        r.datagrams_delivered_s2c,
    );
}

/// Premise for every assertion below: the scene must actually have produced
/// a backlog and measured it. A scene that delivered nothing, or whose
/// latency histogram never got a sample, makes any bound below vacuous.
fn assert_measured(r: &BrowserlessResult, min_samples: u64) {
    assert!(
        r.events.contains(&ClientNetEvent::SessionReady),
        "session never established; nothing below means anything"
    );
    assert!(
        r.queued_critical_latency_count >= min_samples,
        "only {} queued-latency samples (< {min_samples}); the scene did not \
         produce a measurable backlog, so a latency bound proves nothing",
        r.queued_critical_latency_count
    );
}

/// Baseline: a handful of tiles has no backlog to speak of, so queueing
/// delay should be a small multiple of the wire latency rather than orders
/// of magnitude above it. This is the control for the burst test below --
/// without it, a large queued latency there could be blamed on the metric
/// rather than on the backlog.
#[tokio::test(start_paused = true)]
async fn a_small_frame_does_not_queue() {
    let r = run_browserless(burst_scene(2, 2, 5)).await.expect("scene ran");
    report("small", &r);
    assert_measured(&r, 4);

    assert!(
        r.queued_critical_latency_max_us < 1_000_000,
        "a 4-tile frame should reach the wire promptly, but the worst \
         critical pass waited {} us in the scheduler",
        r.queued_critical_latency_max_us
    );
}

/// The production shape: every tile dirty in one frame.
///
/// Reports how long the backlog takes to clear. The bound is deliberately
/// generous -- this is a guard against the pathology growing, not a tuning
/// target -- but it is far below what production exhibited (16.8 s on a
/// 2040-tile grid).
#[tokio::test(start_paused = true)]
async fn a_full_grid_first_frame_clears_without_multi_second_queueing() {
    // 16x16 = 256 tiles. An order of magnitude under production's 2040, so a
    // bound that holds here is a weak claim about production -- but the
    // scaling test below is what speaks to that.
    let r = run_browserless(burst_scene(16, 16, 20))
        .await
        .expect("scene ran");
    report("burst_16x16", &r);
    assert_measured(&r, 64);

    assert!(
        r.queued_critical_latency_max_us < 5_000_000,
        "critical-tier work waited up to {} us ({:.1} s) in the scheduler on \
         a lossless, uncapped link. Wire latency was {} us, so this is \
         queueing delay, not transmission. The drain is paced by \
         Event::DatagramsUnblocked and clamped per resume to quinn's send \
         buffer, which makes the event rate -- not the bandwidth budget -- \
         the binding constraint on clearing a backlog.",
        r.queued_critical_latency_max_us,
        r.queued_critical_latency_max_us as f64 / 1e6,
        r.critical_latency_mean_us,
    );
}

/// Queueing delay must respond to backlog size and to link capacity.
///
/// This is the signal that the metric tracks something real. It does:
/// measured at 2 Mbps (250_000 B/s, `CapTimeline` is bytes -- see
/// `bwe-googcc-review.md`'s measurement traps), `queued_critical_latency_max_us`
/// goes 209,000 at 16x16 to 603,000 at 60x34, and drops to 99,000 when the
/// cap is raised 4x.
#[tokio::test(start_paused = true)]
async fn queueing_delay_responds_to_backlog_and_capacity() {
    let small = run_browserless(capped_scene(16, 16, 250_000)).await.expect("ran");
    let large = run_browserless(capped_scene(60, 34, 250_000)).await.expect("ran");
    let roomy = run_browserless(capped_scene(60, 34, 1_000_000)).await.expect("ran");
    report("capped_16x16", &small);
    report("capped_60x34", &large);
    report("roomy_60x34", &roomy);
    assert_measured(&small, 64);
    assert_measured(&large, 64);
    assert_measured(&roomy, 64);

    assert!(
        large.queued_critical_latency_max_us > small.queued_critical_latency_max_us,
        "13x the tiles on the same link must queue longer, but 60x34 waited \
         {} us against 16x16's {} us -- the metric is not tracking backlog",
        large.queued_critical_latency_max_us,
        small.queued_critical_latency_max_us
    );
    assert!(
        roomy.queued_critical_latency_max_us < large.queued_critical_latency_max_us,
        "4x the capacity must clear the same backlog faster, but the roomy \
         link waited {} us against the tight link's {} us",
        roomy.queued_critical_latency_max_us,
        large.queued_critical_latency_max_us
    );
}

/// Refinement-tier queueing **is** reproduced here, and this pins it.
///
/// Refinement passes (4-13) drain behind the critical ones under pass-major
/// ordering, so on a loaded scene they wait -- which is exactly the
/// behaviour production exhibits:
///
/// | | queued->ACK mean | wire mean | ratio |
/// |---|---|---|---|
/// | production (1920x1080 at connect) | 1,644,411 us | 33,467 us | 49x |
/// | this scene (60x34 at 8 Mbps) | ~2,445,000 us | ~138,000 us | ~18x |
///
/// Same phenomenon, same order. A regression that stopped separating the
/// two clocks, or that let the backlog grow without bound, moves these.
#[tokio::test(start_paused = true)]
async fn refinement_work_queues_behind_critical_work() {
    let r = run_browserless(capped_scene(60, 34, 1_000_000)).await.expect("ran");
    report("refinement_queueing", &r);
    assert_measured(&r, 64);
    assert!(
        r.queued_refinement_latency_count > 1_000,
        "only {} refinement samples; this scene did not exercise the \
         refinement tier enough to say anything about its queueing",
        r.queued_refinement_latency_count
    );

    let observed = ratio(
        r.queued_refinement_latency_mean_us,
        r.refinement_latency_mean_us,
    );
    assert!(
        observed > 3.0,
        "refinement work reached the wire almost as soon as it was queued \
         (queued_mean={} us, wire_mean={} us, ratio={observed:.2}). Production \
         separates these by ~49x, so a ratio near 1 means this scene stopped \
         producing a backlog and the bound below is vacuous.",
        r.queued_refinement_latency_mean_us,
        r.refinement_latency_mean_us
    );
    assert!(
        r.queued_refinement_latency_max_us < 10_000_000,
        "worst refinement pass waited {:.1} s in the scheduler",
        r.queued_refinement_latency_max_us as f64 / 1e6
    );
}

/// **A known fidelity gap, pinned so it cannot be forgotten.**
///
/// Critical-tier queueing is *not* reproduced **on a lossless link**. In
/// production the first frame leaves critical passes waiting 2,289,529 us on
/// average against 22,748 us on the wire -- a 100x separation, 16.8 s at
/// worst. On a lossless link here the two are *identical*, not close: equal,
/// at every backlog size and link capacity measured, including production's
/// own 2040-tile grid at 2 Mbps.
///
/// Scope matters. `retransmission_alone_separates_queued_from_sent` shows
/// that adding loss *does* separate the two clocks (ratio ~6.9), because a
/// retransmit moves `last_sent_at` and leaves `queued_at` alone. That is a
/// different cause from backlog, and it is why this test pins a lossless
/// scene: it is isolating scheduler queueing specifically.
///
/// Refinement queueing (above) *is* reproduced, so this is not a broken
/// metric -- it is specifically the critical tier, which drains first under
/// pass-major ordering and here always fits the first drain.
///
/// The cause is documented at `scheduler_continuation_after_drain` in
/// `io_bridge.rs`: the netsim's token bucket is consumed inside
/// `send_to_all_sessions`, *below* quinn's `datagram_send_buffer`. It shapes
/// what reaches the wire but cannot stop the scheduler from over-popping into
/// quinn. The socketpair write never blocks, so the send buffer never fills,
/// so `Event::DatagramsUnblocked` never throttles the drain -- and that
/// pull-driven pacing is what makes production's critical passes wait.
///
/// This asserts the gap still exists. When backpressure reaches the send
/// buffer, it will fail -- and that failure is the signal to replace it with
/// a real bound on the critical-tier ratio.
#[tokio::test(start_paused = true)]
async fn critical_tier_queueing_is_not_yet_reproduced() {
    let r = run_browserless(capped_scene(60, 34, 250_000)).await.expect("ran");
    report("critical_fidelity", &r);
    assert_measured(&r, 64);

    assert_eq!(
        r.queued_critical_latency_mean_us, r.critical_latency_mean_us,
        "queued->ACK and last_sent->ACK have diverged for the critical tier \
         ({} vs {} us). If backpressure now reaches quinn's \
         datagram_send_buffer, this harness can finally reproduce production's \
         critical-tier queueing (2.3 s mean / 16.8 s max against 22.7 ms on \
         the wire) -- delete this test and assert on the ratio instead.",
        r.queued_critical_latency_mean_us,
        r.critical_latency_mean_us
    );
}

// ---------------------------------------------------------------------------
// What the queued-vs-wire gap actually measures.
// ---------------------------------------------------------------------------

use ghostframe_e2e::netsim::{DropPlan, DropRule};

/// Loss alone separates `queued_at -> ACK` from `last_sent_at -> ACK`, with
/// no scheduler backlog involved.
///
/// A retransmission updates `last_sent_at` and leaves `queued_at` at the
/// original enqueue, so a pass that needed several attempts reports a large
/// queued latency and a small wire latency. That is a different cause from
/// "the work sat in the scheduler behind a backlog", and the two are
/// indistinguishable in the aggregate counters.
///
/// Measured here: one tile's first 40 transmissions dropped, then a clean
/// link. Critical tier comes back at ratio ~6.9 with a 3.1 s worst case --
/// on a scene whose lossless twin reports ratio 1.00.
///
/// This matters for reading production, which had `rto_fired=6739` against
/// 9208 original passes. Its 16.8 s `queued_critical_latency_max_us` cannot
/// be attributed to scheduler queueing without separating the two causes
/// first.
///
/// Incidentally the tile recovers: `nack_miss=0`, the emitter's cache still
/// held every requested pass and served it. Bounded loss does not strand a
/// tile here.
#[tokio::test(start_paused = true)]
async fn retransmission_alone_separates_queued_from_sent() {
    let mut scene = burst_scene(8, 8, 25);
    scene.drops = DropPlan::new(vec![DropRule {
        tile_x: 2,
        tile_y: 2,
        occurrences: (0..40).collect(),
    }]);
    let r = run_browserless(scene).await.expect("scene ran");
    report("retransmit_separation", &r);

    // Premise: the drop must have fired enough to force retransmissions.
    assert_eq!(r.drops_fired.len(), 1, "expected one rule; got {:?}", r.drops_fired);
    assert!(
        r.drops_fired[0] >= 7,
        "drop rule fired only {} times -- too few to force a retransmit cycle",
        r.drops_fired[0]
    );
    assert!(
        r.retransmit_attempts_total > 0,
        "no retransmissions occurred, so this scene cannot demonstrate \
         retransmission-driven latency separation"
    );

    let observed = ratio(
        r.queued_critical_latency_mean_us,
        r.critical_latency_mean_us,
    );
    println!(
        "retransmit separation: ratio={observed:.2} nack_hit={} nack_miss={} retransmits={}",
        r.nack_hit, r.nack_miss, r.retransmit_attempts_total
    );
    assert!(
        observed > 2.0,
        "expected retransmission to separate the two clocks, but ratio={observed:.2} \
         (queued_mean={} us, wire_mean={} us)",
        r.queued_critical_latency_mean_us,
        r.critical_latency_mean_us
    );
}
