//! What happens to tile work that quinn refuses?
//!
//! # The invariant a redesign would rest on
//!
//! `IoBridge::send_to_all_sessions` handles a rejected datagram like this:
//!
//! ```ignore
//! if let Err(e) = wt.send_datagram(conn, dg) {
//!     self.datagram_send_errs = self.datagram_send_errs.saturating_add(1);
//!     // ... log once ... and that is all
//! }
//! ```
//!
//! The datagram is dropped. By then the scheduler has already popped the
//! work -- `drain_refinement_pass_major` uses `queue.remove(idx)`, not
//! `InFlight` retention -- so the only path back is the emitter's RTO
//! timer, bounded by `MAX_RETRANSMITS`.
//!
//! That matters because `SendDatagramError::Blocked` is not an exotic error.
//! It is the *only* thing that makes quinn emit `Event::DatagramsUnblocked`,
//! which is the signal the scheduler's continuation mechanism waits on.
//! quinn's own high-level API is built around provoking it: `send_datagram_wait`
//! attempts the send, takes `Blocked`, then awaits a `Notify` fed by that
//! event. A design that treats `Blocked` as normal flow control needs
//! rejected work to survive; today it survives only by accident of RTO.
//!
//! # Why these scenes need a small send buffer
//!
//! With the production 16 MiB `datagram_send_buffer_size`, `Blocked` never
//! happens: measured `send_datagram_errs_total=0` across every browser and
//! browserless run. So the drop path is unreachable, and a scene that did
//! not shrink the buffer would assert about it while never executing it.
//! `BrowserlessScene::datagram_send_buffer_bytes` shrinks it per scene,
//! which is also the knob a BDP-sized buffer would eventually use.
//!
//! # Status
//!
//! `work_rejected_by_a_full_send_buffer_still_reaches_the_client` is expected
//! to FAIL today. It is the acceptance gate for making rejection lossless,
//! and the measurement that shows whether a redesign helped.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless, BrowserlessResult, BrowserlessScene, FrameScript, SceneLoad,
    DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::load_profile::gradient_tile;
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, NetProfile};

/// Every tile in the grid, as Cdf53 -- the shape of a first frame, and the
/// only load big enough to overrun a small send buffer.
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

/// A burst into a capacity-limited link, with a send buffer small enough
/// that the burst cannot fit in it.
///
/// `CapTimeline` is **bytes** per second despite the parameter name -- see
/// `docs/specs/bwe-googcc-review.md`'s measurement traps.
fn squeezed_scene(cols: u8, rows: u8, send_buffer: Option<usize>) -> BrowserlessScene {
    BrowserlessScene {
        seed: 0x5B10_C4ED,
        datagram_send_buffer_bytes: send_buffer,
        tick_budget_floor_bytes: None,
        load: SceneLoad::Script(vec![full_grid_frame(cols, rows)]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            delay_us: 12_000,
            cap: CapTimeline::constant(250_000),
            bottleneck: Some(Bottleneck::wifi()),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(30),
        grid_cols: u32::from(cols),
        grid_rows: u32::from(rows),
    }
}

/// Tiles that actually have pixels -- the user-visible outcome.
///
/// Delivered *bytes* are the wrong metric, and an earlier version of this
/// test used them. The roomy run retransmits heavily (measured
/// rto_fired=7,696 and retransmits=7,801 against the squeezed run's 693), so
/// counting bytes rewards retransmit waste: a run can "deliver" more bytes
/// while rendering no more screen.
fn tiles_rendered(r: &BrowserlessResult, cols: u8, rows: u8) -> usize {
    (0..cols)
        .flat_map(|x| (0..rows).map(move |y| (x, y)))
        .filter(|(x, y)| r.framebuffer.tile_rgba(*x, *y).is_some())
        .count()
}

fn report(name: &str, r: &BrowserlessResult) {
    println!(
        "{name}: send_errs={} emit_q_peak={} s2c_dg={} s2c_bytes={} \
         rto_fired={} retransmits={} nack_hit={} nack_miss={} stale={}",
        r.send_datagram_errs,
        r.emission_queue_peak,
        r.datagrams_delivered_s2c,
        r.bytes_delivered_s2c,
        r.rto_fired,
        r.retransmit_attempts_total,
        r.nack_hit,
        r.nack_miss,
        r.stale_generation_tiles,
    );
}

/// Control: the production buffer never rejects anything.
///
/// This is what makes the `Blocked` path unreachable in every other scene,
/// and it is why the test below has to shrink the buffer deliberately.
#[tokio::test(start_paused = true)]
async fn the_production_send_buffer_never_rejects_a_datagram() {
    let r = run_browserless(squeezed_scene(8, 8, None))
        .await
        .expect("scene ran");
    report("default_16mib", &r);
    assert!(
        r.events.contains(&ClientNetEvent::SessionReady),
        "session must establish, or nothing below means anything"
    );
    assert!(
        r.datagrams_delivered_s2c > 0,
        "no datagrams crossed; the scene did not run"
    );
    assert_eq!(
        r.send_datagram_errs, 0,
        "a 16 MiB send buffer is not supposed to reject anything on this \
         scene; if it does, the Blocked path is reachable in production and \
         the sibling test is no longer hypothetical"
    );
}

/// A small send buffer must provoke rejections -- the premise for the test
/// below, isolated so a failure there cannot be mistaken for this.
#[tokio::test(start_paused = true)]
async fn a_small_send_buffer_does_provoke_rejections() {
    let r = run_browserless(squeezed_scene(8, 8, Some(64 * 1024)))
        .await
        .expect("scene ran");
    report("squeezed_64kib", &r);
    assert!(
        r.send_datagram_errs > 0,
        "a 64 KiB send buffer did not reject a single datagram against a \
         full-grid Cdf53 burst on a 2 Mbit link. Either the burst is too \
         small or the scheduler's capacity clamp absorbed it -- either way \
         the Blocked path was never exercised"
    );
}

/// The transport must back off from a full send buffer, not hammer it.
///
/// # What this measures, and what it cannot
///
/// Two things changed together when `DatagramSender::send` gained an
/// outcome: `drain` now **stops** at the first rejection, and the rejected
/// emission is **re-queued**. Measured separately, the back-off is what
/// moves the numbers:
///
/// | | hammering (before) | stops (drop) | stops + re-queues |
/// |---|---|---|---|
/// | send rejections | 4,726 | 337 | 319 |
/// | retransmits | 3,424 | 746 | 691 |
/// | stale tiles | 1,170 | 338 | 353 |
///
/// Before, `drain` walked the entire queue calling `send` on every emission,
/// each refused and discarded -- thousands of rejections and thousands of
/// lost datagrams per drain. Stopping at the first refusal cuts that ~14x.
///
/// The re-queue is worth having for correctness -- it is the difference
/// between losing one datagram per drain and losing none -- but it is not
/// visible here: a single loss per drain is well within what the emitter's
/// RTO recovers in a 30 s scene. That invariant is unit-tested instead, in
/// `emitter.rs`: `a_rejected_datagram_is_re_queued_not_dropped` and
/// `a_re_queued_datagram_keeps_its_place_and_its_wire_seq`, both
/// mutation-verified.
///
/// So this asserts the two things it can actually discriminate: the screen
/// still renders in full, and rejections stay bounded instead of scaling
/// with queue depth.
///
/// Measured at production scale (60x34 = 2040 tiles), 64 KiB vs 16 MiB send
/// buffer, everything else identical:
///
/// |  | 16 MiB | 64 KiB |
/// |---|---|---|
/// | bytes delivered | 3,591,308 | 2,353,116 |
/// | send rejections | 0 | 4,726 |
/// | **stale generation tiles** | **7,316** | **1,170** |
///
/// Both halves matter. The small buffer cuts stale tiles 6.3x -- that is the
/// bufferbloat symptom disappearing, datagrams no longer arriving so late
/// that the tile has been superseded -- which is the whole reason to shrink
/// it. And it costs 34% of delivery, because rejected work is dropped.
///
/// So the redesign has a clear target: keep the low staleness, recover the
/// delivery. Both numbers are asserted below so neither can be traded away
/// silently.
///
/// Work the scheduler popped and quinn refused must still reach the client.
/// Today `send_to_all_sessions` drops it and only the emitter's bounded RTO
/// can recover it, so under sustained rejection the delivered byte count
/// falls far short of the same scene with a buffer that never rejects.
///
/// Compares like with like: identical grid, identical link, identical seed;
/// only the send-buffer size differs. A lossless rejection path would put
/// the two within a modest factor -- the small-buffer run should be *paced*
/// by the link, not *lossy*.
#[tokio::test(start_paused = true)]
async fn work_rejected_by_a_full_send_buffer_still_reaches_the_client() {
    // Production scale: 1920x1080 is 60x34 = 2040 tiles. At 8x8 the drop
    // path is survivable -- 92 rejections, 31 RTO firings, 90% delivery --
    // because the emitter's bounded RTO can cover that many. The browser
    // e2e at 2040 tiles measured 15,944 rejections and 11% delivery, so the
    // collapse needs the real tile count to appear.
    let roomy = run_browserless(squeezed_scene(60, 34, None))
        .await
        .expect("roomy scene ran");
    let squeezed = run_browserless(squeezed_scene(60, 34, Some(64 * 1024)))
        .await
        .expect("squeezed scene ran");
    report("roomy", &roomy);
    report("squeezed", &squeezed);

    // Premise: the squeezed run must actually have hit the drop path.
    assert!(
        squeezed.send_datagram_errs > 0,
        "the squeezed run rejected nothing, so this comparison says nothing \
         about the Blocked path"
    );

    // The claim: rejection should cost pacing, not the screen. Both runs
    // offer the same work over the same link for the same duration, so a
    // lossless rejection path leaves comparable amounts rendered.
    const COLS: u8 = 60;
    const ROWS: u8 = 34;
    let roomy_tiles = tiles_rendered(&roomy, COLS, ROWS);
    let squeezed_tiles = tiles_rendered(&squeezed, COLS, ROWS);
    let ratio = squeezed_tiles as f64 / roomy_tiles.max(1) as f64;
    println!(
        "tiles rendered squeezed={squeezed_tiles} roomy={roomy_tiles} \
         (ratio {ratio:.3}); bytes {} vs {}; retransmits {} vs {}",
        squeezed.bytes_delivered_s2c,
        roomy.bytes_delivered_s2c,
        squeezed.retransmit_attempts_total,
        roomy.retransmit_attempts_total,
    );
    // The benefit side. A smaller buffer should reduce staleness, because a
    // datagram that waits seconds in a FIFO can arrive after its tile has
    // been superseded. If a future change raises delivery by re-growing the
    // buffer, this catches it: staleness would climb straight back.
    println!(
        "stale tiles squeezed={} roomy={}",
        squeezed.stale_generation_tiles, roomy.stale_generation_tiles
    );
    assert!(
        squeezed.stale_generation_tiles <= roomy.stale_generation_tiles,
        "the small buffer was supposed to reduce staleness (that is the point \
         of shrinking it) but produced {} stale tiles against the roomy run's \
         {}. If this fails, re-check the premise: a buffer that rejects work \
         without delivering it can look 'less stale' only because less \
         arrived at all.",
        squeezed.stale_generation_tiles,
        roomy.stale_generation_tiles
    );

    // Rejections must be bounded by how often we attempt a drain, not by how
    // much work is queued. ~20,400 passes are enqueued here; hammering
    // produced 4,726 rejections (23% of the queue), backing off produces
    // ~330 (1.6%). 1,000 sits an order of magnitude below the former and
    // 3x above the latter.
    assert!(
        squeezed.send_datagram_errs < 1_000,
        "{} rejections against ~20,400 queued passes: the drain is hammering \
         a full send buffer rather than stopping at the first refusal. Each \
         rejection past the first is a datagram offered to a transport that \
         has already said no.",
        squeezed.send_datagram_errs
    );

    // The backlog must not simply migrate out of quinn's send buffer into
    // ours. Measured: the squeezed run peaks at ~1,574 queued emissions and
    // the roomy run at ~3,904, against ~20,400 passes enqueued -- so most
    // work waits in the scheduler's refinement queue, which is where it can
    // still be superseded. A tight clamp makes the scheduler pop *less*, so
    // the small buffer has the shallower emitter queue of the two.
    //
    // This is the measurement that retired step B of
    // docs/specs/blocked-path-redesign.md (scheduler backpressure on
    // emitter-queue depth): the existing quinn-capacity clamp already bounds
    // it. 5,000 is ~3x the observed peak and a quarter of the enqueued work,
    // so it catches a migration without being brittle.
    assert!(
        squeezed.emission_queue_peak < 5_000,
        "the emitter's emission queue peaked at {} against ~20,400 enqueued \
         passes (roomy run: {}). The backlog has moved out of quinn's send \
         buffer into ours, which is the case scheduler-side backpressure \
         would exist to prevent.",
        squeezed.emission_queue_peak,
        roomy.emission_queue_peak
    );

    assert!(
        ratio > 0.9,
        "a full send buffer cost the screen, not just pacing: the squeezed \
         run rendered {squeezed_tiles} of {COLS}x{ROWS} tiles against the \
         roomy run's {roomy_tiles} (ratio {ratio:.3}), with {} rejections and \
         {} RTO firings. Rejected work must be re-queued rather than dropped: \
         drain_refinement_pass_major pops with queue.remove, so a datagram \
         the transport refuses has no scheduler-side retry.",
        squeezed.send_datagram_errs,
        squeezed.rto_fired
    );
}

/// A fast link, to check a small buffer does not starve the pipe.
///
/// The main sweep runs at 2 Mbit, where the bandwidth-delay product is ~6 KB
/// and 32 KiB is five times it. On a fast path the BDP is the binding number:
/// 50 Mbit with 50 ms RTT is ~312 KB in flight, and a buffer below that
/// cannot keep the pipe full however well it bounds latency. A default has
/// to survive both.
fn fast_link_scene(cols: u8, rows: u8, send_buffer: Option<usize>) -> BrowserlessScene {
    let mut scene = squeezed_scene(cols, rows, send_buffer);
    scene.net = NetProfile {
        delay_us: 25_000, // 50 ms RTT
        cap: CapTimeline::constant(6_250_000), // 50 Mbit, in BYTES/s
        bottleneck: Some(Bottleneck::wifi()),
        ..NetProfile::perfect()
    };
    scene
}

/// Sustained load on a fast link, to see whether a small buffer caps
/// throughput.
///
/// The single-frame fast-link probe cannot answer this: every buffer size
/// delivered the same ~2.66 MB, which is the offered work, not the link's
/// capacity. If offered load never exceeds what the link can carry, a
/// throughput ceiling is invisible.
///
/// It matters because the scheduler clamps each drain to
/// `QUINN_SEND_BUFFER_SAFETY_FRACTION * send_buffer_space()`, so throughput
/// is bounded by roughly `drains_per_second * 0.8 * buffer_size`. At 32 KiB
/// and ~30 drains/s that is ~780 KB/s -- about 6 Mbit, which would cap a
/// 50 Mbit path. This scene offers far more work than the link can carry so
/// the ceiling, if there is one, shows up as delivered bytes.
fn saturating_fast_scene(cols: u8, rows: u8, send_buffer: Option<usize>) -> BrowserlessScene {
    let frames: Vec<FrameScript> = (0..40).map(|_| full_grid_frame(cols, rows)).collect();
    let mut scene = fast_link_scene(cols, rows, send_buffer);
    scene.load = SceneLoad::Script(frames);
    scene
}

/// Sweep the send-buffer size, so step C's default is chosen from data.
///
/// Reports the quantities the choice trades off: how much of the screen
/// renders, how much staleness the buffer's depth causes, how many bytes it
/// costs, and whether rejections stay bounded.
#[tokio::test(start_paused = true)]
#[ignore = "diagnostic-only: run on demand with --ignored"]
async fn probe_saturating_fast_link() {
    const COLS: u8 = 60;
    const ROWS: u8 = 34;
    for size in [Some(32 * 1024), Some(64 * 1024), Some(256 * 1024), Some(1024 * 1024), None] {
        let r = run_browserless(saturating_fast_scene(COLS, ROWS, size)).await.expect("ran");
        let label = size.map(|s| format!("{}KiB", s / 1024)).unwrap_or("16MiB".into());
        println!(
            "SAT {label:>7}: bytes={:>9} dg={:>6} stale={:>6} retx={:>6} \
             send_errs={:>6} emit_q_peak={:>6}",
            r.bytes_delivered_s2c,
            r.datagrams_delivered_s2c,
            r.stale_generation_tiles,
            r.retransmit_attempts_total,
            r.send_datagram_errs,
            r.emission_queue_peak,
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "diagnostic-only: run on demand with --ignored"]
async fn probe_send_buffer_sizes_on_a_fast_link() {
    const COLS: u8 = 60;
    const ROWS: u8 = 34;
    for size in [Some(32 * 1024), Some(256 * 1024), Some(1024 * 1024), None] {
        let r = run_browserless(fast_link_scene(COLS, ROWS, size)).await.expect("ran");
        let label = size.map(|s| format!("{}KiB", s / 1024)).unwrap_or("16MiB".into());
        println!(
            "FAST {label:>7}: tiles={:>4} stale={:>5} bytes={:>8} retx={:>5} \
             send_errs={:>5} emit_q_peak={:>5}",
            tiles_rendered(&r, COLS, ROWS),
            r.stale_generation_tiles,
            r.bytes_delivered_s2c,
            r.retransmit_attempts_total,
            r.send_datagram_errs,
            r.emission_queue_peak,
        );
    }
}

#[tokio::test(start_paused = true)]
#[ignore = "diagnostic-only: run on demand with --ignored"]
async fn probe_send_buffer_sizes() {
    const COLS: u8 = 60;
    const ROWS: u8 = 34;
    for size in [
        Some(32 * 1024),
        Some(64 * 1024),
        Some(128 * 1024),
        Some(256 * 1024),
        Some(1024 * 1024),
        None, // 16 MiB production default
    ] {
        let r = run_browserless(squeezed_scene(COLS, ROWS, size)).await.expect("ran");
        let label = size.map(|s| format!("{}KiB", s / 1024)).unwrap_or("16MiB".into());
        println!(
            "SWEEP {label:>7}: tiles={:>4} stale={:>5} bytes={:>8} retx={:>5} \
             send_errs={:>5} emit_q_peak={:>5}",
            tiles_rendered(&r, COLS, ROWS),
            r.stale_generation_tiles,
            r.bytes_delivered_s2c,
            r.retransmit_attempts_total,
            r.send_datagram_errs,
            r.emission_queue_peak,
        );
    }
}

// ---------------------------------------------------------------------------
// Bandwidth as an axis
// ---------------------------------------------------------------------------

/// A floor low enough that `base_budget_bytes`'s bandwidth term always wins.
///
/// The production floor is 256 KiB per 33.3 ms tick, i.e. 7.86 MB/s, so with
/// `SCHEDULER_TICK_BUDGET_FRACTION = 0.90` the bandwidth term only overtakes
/// it above ~70 Mbit/s. Every link measured in this file is below that, which
/// means the budget has been the constant 256 KiB in all of them.
const LOW_FLOOR_BYTES: usize = 8 * 1024;

fn mbit(m: f64) -> u64 {
    (m * 1_000_000.0 / 8.0) as u64
}

/// A saturating scene on a link of the given capacity.
///
/// Saturating matters: a scene that offers less than the link can carry
/// measures offered load, not capacity, and reports every configuration as
/// identical. That mistake cost a whole round of fast-link measurement -- see
/// `saturating_fast_scene`.
fn ladder_scene(cols: u8, rows: u8, bytes_per_s: u64, floor: Option<usize>) -> BrowserlessScene {
    let mut scene = saturating_fast_scene(cols, rows, None);
    scene.net = NetProfile {
        delay_us: 25_000, // 50 ms RTT
        cap: CapTimeline::constant(bytes_per_s),
        bottleneck: Some(Bottleneck::wifi()),
        ..NetProfile::perfect()
    };
    scene.tick_budget_floor_bytes = floor;
    scene
}

/// Sweep capacity across the floor's knee, with the floor in and out of play.
///
/// Two static points cannot validate a control law that claims invariance
/// over bandwidth, and they especially cannot find a knee that sits outside
/// both of them. The ladder straddles ~70 Mbit so the floor's effect becomes
/// an observation rather than an inference.
///
/// Read it as two columns: with the production floor, the budget is a
/// constant and should look bandwidth-independent; with `LOW_FLOOR_BYTES` the
/// bandwidth term binds and delivered bytes should track capacity.
#[tokio::test(start_paused = true)]
#[ignore = "diagnostic-only: run on demand with --ignored"]
async fn probe_bandwidth_ladder() {
    const COLS: u8 = 60;
    const ROWS: u8 = 34;
    for m in [1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 200.0] {
        for (label, floor) in [("floor=256K", None), ("floor=8K", Some(LOW_FLOOR_BYTES))] {
            let r = run_browserless(ladder_scene(COLS, ROWS, mbit(m), floor))
                .await
                .expect("ran");
            println!(
                "LADDER {m:>6} Mbit {label:>10}: bytes={:>9} tiles={:>4} stale={:>6} \
                 retx={:>6} send_errs={:>6} emit_q_peak={:>6} qlat_max_us={:>8}",
                r.bytes_delivered_s2c,
                tiles_rendered(&r, COLS, ROWS),
                r.stale_generation_tiles,
                r.retransmit_attempts_total,
                r.send_datagram_errs,
                r.emission_queue_peak,
                r.queued_critical_latency_max_us,
            );
        }
    }
}

/// The axis that killed a structurally similar design.
///
/// `docs/specs/bwe-googcc-review.md` records a source-rate token bucket
/// refilled at goog_cc's `target_rate`: it was a net regression (752,637 ->
/// 548,602 delivered, against a 600,000 "not serialised" floor) because the
/// estimate takes 5-7 s to reach capacity. Any budget derived from that same
/// estimate inherits the same lag, so static capacities are not a sufficient
/// test -- the interesting behaviour is where the estimate is *wrong*.
///
/// Step-down is the safety case (stale-high estimate overfills, which is the
/// bloat this work exists to remove) and step-up is the throughput case
/// (stale-low estimate pins the sender below the link).
#[tokio::test(start_paused = true)]
#[ignore = "diagnostic-only: run on demand with --ignored"]
async fn probe_capacity_transitions() {
    const COLS: u8 = 60;
    const ROWS: u8 = 34;
    let cases: [(&str, Vec<(u64, u64)>); 3] = [
        ("step_down_50_to_5", vec![(0, mbit(50.0)), (3_000_000, mbit(5.0))]),
        ("step_up_5_to_50", vec![(0, mbit(5.0)), (3_000_000, mbit(50.0))]),
        (
            "oscillate_50_5_50",
            vec![(0, mbit(50.0)), (2_000_000, mbit(5.0)), (4_000_000, mbit(50.0))],
        ),
    ];
    for (name, points) in cases {
        for (label, floor) in [("floor=256K", None), ("floor=8K", Some(LOW_FLOOR_BYTES))] {
            let mut scene = ladder_scene(COLS, ROWS, mbit(50.0), floor);
            scene.net.cap = CapTimeline { points: points.clone() };
            let r = run_browserless(scene).await.expect("ran");
            println!(
                "TRANS {name:>18} {label:>10}: bytes={:>9} tiles={:>4} stale={:>6} \
                 retx={:>6} send_errs={:>6} emit_q_peak={:>6} qlat_max_us={:>8}",
                r.bytes_delivered_s2c,
                tiles_rendered(&r, COLS, ROWS),
                r.stale_generation_tiles,
                r.retransmit_attempts_total,
                r.send_datagram_errs,
                r.emission_queue_peak,
                r.queued_critical_latency_max_us,
            );
        }
    }
}
