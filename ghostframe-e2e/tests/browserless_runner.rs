//! Task 15a/15b: the browserless netsim scene runner.
//!
//! Task 15a proved a real `IoBridge` and a real `ClientNet` can establish a
//! WebTransport session across a Unix socketpair, routed through the
//! netsim, under tokio's virtual clock (tests 1-3 below). Task 15b adds
//! tile injection and framebuffer assembly (tests 4-5): frames declared on
//! `BrowserlessScene::frames` are encoded and sent to a real `IoBridge`,
//! decoded by a real `ClientNet`, and the resulting `Event::TileReady`
//! pixels land in `BrowserlessResult.framebuffer`.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless, BrowserlessScene, FrameScript, SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, DropRule, NetProfile};

#[tokio::test(start_paused = true)]
async fn the_session_establishes_over_the_socketpair() {
    let scene = BrowserlessScene {
        seed: 1,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");
    assert!(
        result.events.contains(&ClientNetEvent::SessionReady),
        "seed 1: the WebTransport session must establish over the socketpair"
    );
    assert_eq!(result.seed, 1);
}

/// Proves datagrams genuinely crossed the socketpair (and therefore
/// `IoBridge`) rather than the handshake being short-circuited: every byte
/// counted here passed through `SocketPairPump::send`/`recv` — the exact
/// wire framing `IoBridge` speaks on the other end of the socket. A
/// short-circuited handshake (e.g. wiring `ClientNet` directly to a
/// `QuicServer` in-memory, as `ghostframe-client-net`'s own test helpers
/// do) would never touch this counter at all.
#[tokio::test(start_paused = true)]
async fn bytes_actually_cross_the_socketpair() {
    let scene = BrowserlessScene {
        seed: 2,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");
    assert!(
        result.bytes_delivered > 0,
        "seed 2: at least the QUIC handshake bytes must have crossed the socketpair"
    );
    assert_eq!(
        result.bytes_dropped, 0,
        "seed 2: NetProfile::perfect() drops nothing"
    );
}

/// Proves the netsim is genuinely in the datagram path.
///
/// The two tests above run on `NetProfile::perfect()`, where every verdict
/// is `Deliver` — so removing the netsim from the loop entirely would not
/// change their result. Only a profile that actually drops packets can
/// distinguish "routed through the simulator" from "handed straight over".
///
/// QUIC retransmits, so the session must still establish despite the loss;
/// asserting both halves means neither a bypassed simulator (no drops) nor
/// a broken retransmit path (no session) can pass.
#[tokio::test(start_paused = true)]
async fn a_lossy_link_still_establishes_and_records_drops() {
    let scene = BrowserlessScene {
        seed: 7,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(5),
        grid_cols: 4,
        grid_rows: 4,
    };

    let result = run_browserless(scene).await.expect("scene ran");

    assert!(
        result.bytes_dropped > 0,
        "seed 7: a 10% loss profile must drop something; 0 dropped bytes \
         means the netsim is not in the datagram path at all"
    );
    assert!(
        result.events.contains(&ClientNetEvent::SessionReady),
        "seed 7: the session must still establish across a lossy link — \
         QUIC retransmits (dropped {} bytes, delivered {})",
        result.bytes_dropped,
        result.bytes_delivered
    );
}

/// The plan's acceptance test: a single Solid tile, injected once the
/// session is ready, arrives at the client and decodes to the right BGRA
/// -> RGBA swizzle.
#[tokio::test(start_paused = true)]
async fn a_single_solid_tile_arrives_on_a_perfect_link() {
    let scene = BrowserlessScene {
        seed: 1,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: vec![(
                (0, 0),
                TileSpec::Solid {
                    bgra: [10, 20, 30, 255],
                },
            )],
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };

    let result = run_browserless(scene).await.expect("scene ran");

    let px = result
        .framebuffer
        .tile_rgba(0, 0)
        .expect("tile (0,0) decoded");
    assert_eq!(&px[0..4], &[30, 20, 10, 255], "BGRA -> RGBA swizzle");
    assert_eq!(result.stale_generation_tiles, 0);
}

/// Proves frames after the first are actually injected: a two-frame scene
/// rewrites tile (0,0) with a different color in frame 2, and only frame 2's
/// color must survive. `a_single_solid_tile_arrives_on_a_perfect_link` alone
/// would pass even if the injection loop only ever sent frame 0 — this is
/// the test that catches that.
///
/// Note what this does NOT prove. Pinning the per-tile generation counter to
/// a constant leaves every test in this file green, so nothing here is
/// load-bearing on the generation advancing. Solid tiles are self-contained
/// single payloads, so a stale generation cannot change their pixels.
/// Generation only becomes observable with a multi-pass codec, where
/// `Cdf53TileState` must discard the previous generation's accumulated
/// planes instead of blending them — a Cdf53 multi-frame scene (task 16) is
/// what will pin that down.
#[tokio::test(start_paused = true)]
async fn a_second_frame_overwrites_the_first_frames_tile() {
    let scene = BrowserlessScene {
        seed: 3,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![
            FrameScript {
                tiles: vec![(
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [10, 20, 30, 255],
                    },
                )],
            },
            FrameScript {
                tiles: vec![(
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [200, 150, 100, 255],
                    },
                )],
            },
        ]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        duration: Duration::from_millis(500),
        grid_cols: 4,
        grid_rows: 4,
    };

    let result = run_browserless(scene).await.expect("scene ran");

    let px = result
        .framebuffer
        .tile_rgba(0, 0)
        .expect("tile (0,0) decoded");
    assert_eq!(
        &px[0..4],
        &[100, 150, 200, 255],
        "frame 2's color (BGRA [200,150,100,255] -> RGBA) must be what the \
         client ends up rendering, proving frame 2 was actually injected"
    );
    assert_eq!(result.stale_generation_tiles, 0);
}

/// Task 16, scene 1: a single Cdf53 tile under 10% loss must still converge
/// to a byte-exact lossless reconstruction, given enough virtual time for
/// retransmission of every one of the 14 progressive passes.
///
/// Exact equality is deliberate here, not an oversight: all 14 CDF 5/3
/// passes reconstruct a tile byte-for-byte through the real client decode
/// path (`Cdf53TileState::integrate`, which converts the wavelet inverse's
/// BGR output to RGB plus alpha forced to 255 — see its doc comment). This
/// was verified empirically (1024/1024 pixels exact, max channel difference
/// 0, on this exact gradient tile). If this assertion starts failing, the
/// overwhelmingly likely cause is retransmission not completing within
/// `duration`, not a precision regression — investigate the harness's
/// virtual-time advance loop before touching this assertion.
#[tokio::test(start_paused = true)]
async fn cdf53_converges_to_lossless_under_10pct_loss() {
    let scene = BrowserlessScene {
        seed: 0x5EED,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: vec![(
                (0, 0),
                TileSpec::Cdf53 {
                    bgra: gradient_tile(),
                },
            )],
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(10),
        grid_cols: 1,
        grid_rows: 1,
    };
    let result = run_browserless(scene).await.expect("scene ran");
    let px = result.framebuffer.tile_rgba(0, 0).expect("tile decoded");
    assert_eq!(
        px,
        expected_rgba(&gradient_tile()),
        "seed 0x5EED: must converge lossless"
    );
}

/// Task 16, scene 2: a tile rewritten with a different colour on every one
/// of 20 frames, under 5% loss and 30ms of reorder, must end up rendering
/// only the *last* frame's colour — never an earlier, superseded
/// generation.
///
/// A late/reordered arrival being detected and dropped by
/// `FrameBuffer::apply_tile_ready`'s `frame_seq` staleness check is the
/// mechanism working correctly, not a failure — so this does NOT assert
/// `result.stale_generation_tiles == 0` (that counter tracks the `apply`
/// raw-payload path's generation staleness, which this scene's
/// `apply_tile_ready` ingest never touches; even if it did, a nonzero count
/// would be exactly what a reordering profile should produce, so asserting
/// it to zero would contradict the very profile this test configures).
///
/// 20 frames, not 10: the per-tile wire `generation` field is 4 bits and
/// wraps at 16 (see `browserless.rs`'s `inject_frame` doc comment), so
/// frames 16-19 reuse generations 0-3. What still disambiguates the final
/// colour is `frame_seq`, a `u32` that keeps monotonically increasing
/// across the whole scene and is what `FrameBuffer::apply_tile_ready`
/// actually keys staleness on (see its doc comment) — `generation` wrapping
/// is irrelevant to that check. Shortening this scene back to 10 frames
/// would silently delete the only coverage this file has for the
/// generation-wrap boundary; do not do that.
#[tokio::test(start_paused = true)]
async fn superseded_generations_never_render() {
    let scene = BrowserlessScene {
        seed: 0xB0B,
        datagram_send_buffer_bytes: None,
        // Same tile rewritten with a different colour on each of 20 frames.
        load: SceneLoad::Script(
            (0..20u8)
                .map(|i| FrameScript {
                    tiles: vec![(
                        (0, 0),
                        TileSpec::Solid {
                            bgra: [i * 10, 20, 30, 255],
                        },
                    )],
                })
                .collect(),
        ),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            loss: 0.05,
            reorder_us: 30_000,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(5),
        grid_cols: 1,
        grid_rows: 1,
    };
    let result = run_browserless(scene).await.expect("scene ran");

    // The last frame's colour is what must survive: [190, 20, 30, 255] BGRA.
    let px = result.framebuffer.tile_rgba(0, 0).expect("tile decoded");
    assert_eq!(
        &px[0..4],
        &[30, 20, 190, 255],
        "seed 0xB0B: the final frame's colour must win"
    );
}

/// Task 16, scene 3: goodput under a bandwidth step-down must both pass
/// traffic and shed some, proving the token-bucket cap
/// (`NetSim::decide`'s bandwidth-cap step, applied last, after every rng
/// draw) is genuinely in the datagram path rather than a no-op.
///
/// `busy_frames(8)` assumes and requires a 4x4 (16-tile) grid: each of the 8
/// frames rewrites all 16 tiles with `TileSpec::Cdf53` content that varies
/// frame-to-frame (see its doc comment), so every frame is a full 14
/// real-byte progressive passes per tile rather than degenerate/duplicate
/// content the cap could trivially absorb. 8 was chosen empirically as the
/// smallest frame count that still reliably sheds traffic (measured:
/// 223,525 B delivered / 1,200 B dropped at seed 0xCAFE) — this codec's
/// CPU cost in a debug build scales worse than linearly with frame count
/// (measured ~60 frames -> 62s wall-clock vs ~8 frames -> 2.7s for this one
/// test), so keeping the frame count to the minimum that still exercises
/// real shedding matters for suite runtime.
///
/// Why two runs rather than one step-down timeline: an earlier version of
/// this scene used `CapTimeline::step` and asserted only that some bytes
/// were delivered and some dropped. That passed with the step removed
/// entirely (verified), because all 8 frames are injected within ~128us of
/// virtual time, so the shed bytes come from the initial burst exceeding
/// the token bucket's burst-capacity allowance at session start
/// (`burst_capacity = bps / 10`) — never from traffic crossing the
/// step-down boundary. `NetSim`'s own
/// `delivered_rate_tracks_the_cap_across_a_step_down` already covers
/// step-downs at the unit level; what this scene is for is proving the cap
/// value reaches the datagram path in the assembled pipeline, which a
/// comparison between two cap values does and a single run cannot.
///
#[tokio::test(start_paused = true)]
async fn a_tighter_cap_sheds_more_traffic() {
    // Same scene, same seed, two cap values. A cap genuinely in the datagram
    // path must shed strictly more at 250 kB/s than at 2 MB/s; a cap whose
    // value never reaches the path would shed identically in both runs.
    async fn run_at(cap_bps: u64) -> (u64, u64) {
        let scene = BrowserlessScene {
            seed: 0xCAFE,
            datagram_send_buffer_bytes: None,
            load: SceneLoad::Script(busy_frames(8)),
            cadence_us: DEFAULT_CADENCE_US,
            net: NetProfile {
                cap: CapTimeline::constant(cap_bps),
                ..NetProfile::perfect()
            },
            drops: Default::default(),
            duration: Duration::from_secs(4),
            // busy_frames(8) rewrites a fixed 4x4 grid every frame; see its
            // doc comment.
            grid_cols: 4,
            grid_rows: 4,
        };
        let r = run_browserless(scene).await.expect("scene ran");
        (r.bytes_delivered, r.bytes_dropped)
    }

    let (fast_delivered, fast_dropped) = run_at(2_000_000).await;
    let (slow_delivered, slow_dropped) = run_at(250_000).await;

    assert!(
        fast_delivered > 0 && fast_dropped > 0,
        "seed 0xCAFE: the 2 MB/s cap must both pass and shed traffic \
         (delivered {fast_delivered}, dropped {fast_dropped})"
    );
    assert!(
        slow_dropped > fast_dropped,
        "seed 0xCAFE: a 250 kB/s cap must shed strictly more than a 2 MB/s cap \
         ({slow_dropped} vs {fast_dropped}); equal shedding means the cap value \
         is not reaching the datagram path"
    );

    // Deliberately NOT asserted: that the tighter cap delivers fewer bytes.
    // It delivers MORE (measured 842,639 at 250 kB/s vs 223,613 at 2 MB/s),
    // because `bytes_delivered` counts wire bytes including retransmissions,
    // and this scene is offered-limited — 8 frames of finite content, not a
    // saturating source. A tighter cap sheds more, so the RTO wheel retries
    // more, so more total bytes cross the wire. `bytes_delivered` is
    // therefore not a goodput proxy here, and an assertion that it falls
    // with the cap would be both wrong and confusing to whoever hit it.
    let _ = slow_delivered;
}

/// Task 16, scene 4: under sustained 5% loss and a tight 400kB/s cap, every
/// one of a 4x4 grid's 16 Cdf53 tiles must eventually converge to a
/// byte-exact lossless reconstruction — no tile's retransmissions may be
/// starved out by another tile's traffic indefinitely.
///
/// `is_some()` alone would pass on a single delivered pass; this asserts
/// full byte-exact convergence instead, since scene 1 above establishes
/// that full-pass CDF53 reconstruction through the real client decode path
/// is byte-exact.
#[tokio::test(start_paused = true)]
async fn every_cdf53_pass_eventually_lands() {
    let scene = BrowserlessScene {
        seed: 0xF00D,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: (0..4u8)
                .flat_map(|x| {
                    (0..4u8).map(move |y| {
                        (
                            (x, y),
                            TileSpec::Cdf53 {
                                bgra: gradient_tile(),
                            },
                        )
                    })
                })
                .collect(),
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            loss: 0.05,
            cap: CapTimeline::constant(400_000),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(20),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");
    let expected = expected_rgba(&gradient_tile());
    for x in 0..4u8 {
        for y in 0..4u8 {
            let px = result.framebuffer.tile_rgba(x, y);
            assert_eq!(
                px,
                Some(expected.as_slice()),
                "seed 0xF00D: tile ({x},{y}) never converged to lossless"
            );
        }
    }
}

/// End-to-end proof that the production ACK path feeds the estimator, and
/// that emit and arrival timestamps share a clock epoch.
///
/// Deliberately does NOT assert convergence toward the netsim's cap. The
/// scene injects `busy_frames(8)` within ~128 ms and then runs on heartbeats,
/// so there is no sustained delivery for the controller to measure: it
/// consumes ~1800 samples in a burst and the estimate moves under 2% off its
/// seed. Convergence is covered by `ghostframe-lib/tests/bwe_bench.rs`, which
/// drives continuous traffic and reaches 3.03 Mbit/s on a 3 Mbit/s link.
///
/// What this test uniquely covers is the wiring and the clocks — neither of
/// which the bench can reach, because the bench synthesises its own samples.
#[tokio::test(start_paused = true)]
async fn bwe_estimator_is_fed_and_epoch_consistent() {
    let scene = BrowserlessScene {
        seed: 0xB4E,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(busy_frames(8)),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            cap: CapTimeline::constant(1_000_000),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(6),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");

    // The estimate alone proves nothing: an estimator that received no
    // samples at all still reports its seed, and the seed sits inside the
    // plausible range asserted below. This is the assertion that shows the
    // production ACK path actually fed the controller during the scene.
    assert!(
        result.bwe_samples_seen > 0,
        "seed 0xB4E: the estimator consumed {} ACK-arrival samples — the \
         production path did not reach it",
        result.bwe_samples_seen
    );
    assert!(
        result.bwe_estimate_bps > 0,
        "seed 0xB4E: no bandwidth estimate was produced at all"
    );
    // Deliberately wide: the harness is not seed-reproducible, so this asserts
    // the estimate is in the right order of magnitude for a 1 MB/s (8 Mbit/s)
    // cap, not a precise value.
    assert!(
        (200_000..=80_000_000).contains(&result.bwe_estimate_bps),
        "seed 0xB4E: estimate {} bps is not plausible for an 8 Mbit/s cap",
        result.bwe_estimate_bps
    );

    // Emit and arrival timestamps must share a clock epoch. A non-zero count
    // here is the signature of the bug that panicked the bridge task when a
    // client-epoch value was read as an RTT.
    assert_eq!(
        result.implausible_rtt_samples, 0,
        "seed 0xB4E: {} samples had an implausible derived RTT — emit and \
         arrival timestamps are probably not on the same clock",
        result.implausible_rtt_samples
    );
}

/// A lossless link with an ordinary wide-area RTT must not retransmit.
///
/// `retransmits_fire_under_loss_but_not_on_a_perfect_link` already asserts
/// "no loss, no retransmits", but it runs at effectively zero delay, where
/// an acknowledgement is back long before any timer could fire. The bug this
/// guards lives entirely in the gap between that and a real path.
///
/// `rto_for_attempt` used to compute `min(max(2 x smoothed_rtt, 25ms),
/// BASE_RTO_MS)` for a first attempt, and `BASE_RTO_MS` was 50 ms. That `min`
/// was a *ceiling*: no matter how slow the path, the first retransmission
/// timer never exceeded 50 ms. On any link whose round trip was slower than
/// that, the timer expired before an acknowledgement could physically
/// arrive, so every datagram was retransmitted at least once — not because
/// anything was lost, but because the timer couldn't be set correctly.
///
/// Compounding it, `ReliableTileEmitter::set_smoothed_rtt` was never called
/// from anywhere, so `smoothed_rtt` also stayed at its 20 ms constructor
/// default and the ceiling was reached from below as well.
///
/// Observed in production on a tailnet path: `rto_fired=33685` and
/// `retransmit_attempts_total=44207` against 26578 fresh emissions — 1.7x
/// more retransmission than actual picture — while `emitter_ack_hits`
/// covered every single emission (`emitter_ack_misses=59`). Nothing was
/// lost; the retransmissions were spurious. Mean ACK latency was 29 ms, but
/// the max was 103 ms over 6517 samples — a fixed deadline anywhere near
/// that mean loses to the tail constantly. That storm competed with first
/// paint for wire bandwidth, which is what a user sees as a screen that
/// takes many seconds to converge.
///
/// # Gate: two fixes, measured
///
/// This began as a deliberately-failing reproduction of an open bug: **1788
/// spurious retransmissions** on a link that drops nothing, with one
/// acknowledgement arriving for every tile-pass the scene emits — so every
/// pass *was* acknowledged and every retransmission was waste.
///
/// **Fix 1 (stranding).** Since acknowledgements began naming a `wire_seq`
/// rather than content, the server had to translate that back through
/// `TransmissionLedger` before it could release a cache entry. `expire()`
/// deleted that translation when it declared a transmission lost, and
/// nothing re-established it — so an acknowledgement arriving afterwards
/// resolved to nothing, `on_ack` never ran, and the entry retransmitted at
/// the backoff ceiling for the rest of the session. Measured: 1458
/// transmissions expired, and **all 1458 were acknowledged afterwards**,
/// against a 236 ms horizon and an acknowledgement p90 of 251 ms. Nothing
/// was lost; the race was simply lost permanently. Bounded tombstones in
/// the ledger fixed that, dropping the count from 1788 to 320.
///
/// **Fix 2 (the fixed deadline itself).** The residual 320 was a second,
/// independent cause: the timer fired at 40 ms while an acknowledgement
/// couldn't arrive before ~85 ms on this link, so each in-flight emission
/// retransmitted exactly once and was then acknowledged — every one at
/// `attempts=0`, no backoff chain, max age 267 ms (the ACK p90). That's a
/// timer racing latency, not a stranding regression. Deleting the timer
/// outright was considered and rejected: the scheduler's own 2xRTT
/// `InFlight` retry covers the one case a receiver can't see (a tile whose
/// only datagram is lost, then static), but disabling the RTO entirely
/// removes the last-resort cover for every other case. Instead the
/// deadline now derives from a rolling p95 of measured ACK latency
/// (`ack_latency::AckLatencyTracker`), doubled and clamped to
/// `[150ms, 2s]` (see `rto_for_attempt`'s doc comment) — a deadline that
/// tracks the path instead of guessing at it. Measured on this scene: 320
/// -> 128, all of which are the same signature as before (deadline still
/// slightly under the true ACK latency for a bursty first-paint sender, but
/// now within the p95-derived floor's design margin rather than racing a
/// fixed 40 ms constant).
///
/// Causes checked and ruled out along the way, recorded so they are not
/// re-tried:
///
/// - Not the 50 ms `rto_for_attempt` ceiling alone (pre-fix). Removing it
///   changed the count by less than 1%.
/// - Not `set_smoothed_rtt` never being called (it wasn't, anywhere; the
///   field and setter are gone now). Wiring quinn's measured RTT through
///   moved the count by 11 out of 1788.
/// - Not the 20 ms `smoothed_rtt` default used before the first path
///   sample. Raising it to 100 ms changed nothing and broke 7 tests that
///   encoded the old timing.
/// - Not `ASSEMBLY_TIMEOUT_US` being 30 ms, below the server's own 33.3 ms
///   scheduler tick. Raising it to 250 ms changed nothing on its own,
///   although the inversion still looks wrong.
#[tokio::test(start_paused = true)]
async fn a_lossless_link_with_a_real_rtt_does_not_retransmit() {
    // 35 ms each way = 70 ms round trip: unremarkable for wifi or a tailnet
    // hop, and comfortably past the old 50 ms ceiling this test was written
    // to catch.
    const ONE_WAY_US: u64 = 35_000;

    let scene = BrowserlessScene {
        seed: 0x9E77_0001,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(busy_frames_grid(4, 6, 6)),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            delay_us: ONE_WAY_US,
            // Explicitly lossless. Every retransmission counted below is
            // therefore spurious by construction — there is nothing to
            // recover.
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(8),
        grid_cols: 6,
        grid_rows: 6,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    // The scene has to have actually delivered something, or "no
    // retransmissions" is vacuous.
    assert!(
        r.bytes_delivered_s2c > 50_000,
        "scene must carry real traffic; delivered {} bytes",
        r.bytes_delivered_s2c
    );

    println!(
        "lossless {}ms RTT: retransmit_attempts={} delivered_s2c={}",
        (ONE_WAY_US * 2) / 1000,
        r.retransmit_attempts_total,
        r.bytes_delivered_s2c
    );

    // Gates both fixes now (see the doc comment above for the full history):
    // the ledger-tombstone fix (1788 -> 320) and the ack-latency-derived RTO
    // deadline that replaced the fixed 40 ms timer (320 -> 128, measured
    // deterministically across repeated runs -- `retransmit_attempts_total`
    // does not vary with `bytes_delivered_s2c`'s minor run-to-run jitter).
    //
    // 128 is not zero: the deadline is `clamp(ack_p95 * 2, 150ms, 2s)`, and
    // on this link the measured p95 lands close enough to the 150 ms floor
    // that a first-paint burst still occasionally beats its own deadline
    // before the tracker's window reflects the burst's true latency. That
    // is a materially different signature from either prior cause (no
    // stranding, no fixed-constant race) and 128 is small enough that it no
    // longer competes meaningfully with first paint. Tightened from < 400
    // to keep a large margin above 128 for run-to-run variance while still
    // catching a regression back toward either prior cause; do not raise it
    // back toward 400 without measuring why 128 grew.
    assert!(
        r.retransmit_attempts_total < 100,
        "a lossless {}ms-RTT link retransmitted {} times, against 128 measured \
         with the ack-latency-derived RTO deadline. Nothing was lost. A number \
         near 1788 means late acknowledgements are stranding their cache \
         entries again; a number near 320 means the deadline is back to \
         racing a fixed constant instead of tracking measured latency. \
         Diagnose with GHOSTFRAME_RTO_PROBE=1 and look at the `attempts=` \
         and `rto_us=` distribution, and at the `rto_ack_latency_p95_us` / \
         `rto_first_attempt_deadline_us` fields on the `cumulative emit` log \
         line.",
        (ONE_WAY_US * 2) / 1000,
        r.retransmit_attempts_total
    );
}

/// BWE Stage 2 prereq: proves the server's `ReliableTileEmitter` retransmit
/// path (`EmitterStats::retransmit_attempts_total`) is actually reachable
/// from a browserless scene at all. A prior investigation found 238 BWE
/// samples across every scene in this file with `attempts_total` stuck at
/// zero — FEC (K=10 XOR groups, one parity) was absorbing every loss before
/// a NACK, or an RTO, was ever needed. BWE Stage 2 adds two retransmit
/// priority queues on top of this path, so it needs at least one scene
/// that genuinely drives it before it can build on it.
///
/// FEC only fails when >=2 of a K=10 group's members are lost, so this
/// needs enough concurrent in-flight traffic that groups actually fill and
/// losses land two-deep inside one. `cdf53_converges_to_lossless_under_10pct_loss`'s
/// single tile (1 tile, 14 passes/frame) doesn't generate enough concurrent
/// groups to hit that at all: measured `retransmit_attempts_total == 0`
/// deterministically (checked twice) for a single `Cdf53` frame on the
/// full 4x4 grid too (16 tiles, 224 passes) — apparently enough redundancy
/// elsewhere in the emit path that independent 10% loss over one frame's
/// worth of traffic never leaves 2 losses in the same FEC group.
///
/// `busy_frames(N)` (multiple sequential frames, each rewriting the whole
/// 4x4 grid with fresh `Cdf53` content — see its doc comment) supplies the
/// extra concurrent traffic needed, but **N matters a lot** and this was
/// not obvious up front:
/// - `busy_frames(8)`, `duration: 15s`, no cap: forces retransmits (900-1150
///   `retransmit_attempts_total` observed) but **occasionally hangs** —
///   roughly 15-30% of runs across several batches of 15-20 hit
///   `drive_session`'s `MAX_ITERS` (5000) bail-out entirely, apparently a
///   burst-pileup interaction between generation-bumping (each of the 8
///   frames supersedes the last one's still-in-flight retransmits) and the
///   uncapped simultaneous 16-tile injection. A 2 MB/s cap on top did not
///   fix it (throughput never got near 2 MB/s, so the cap never actually
///   engaged). Not used here — an occasionally-hanging test is worse than
///   a narrower one that doesn't.
/// - `busy_frames(4)`: same hang, lower but still real rate (~15% across a
///   20-run batch).
/// - `busy_frames(2)`, `duration: 10s`, no cap: the value used below.
///   40/40 runs across two batches completed normally (no hang), typically
///   in ~320-350 loop iterations (comfortably under the 5000 budget) and
///   ~1.5s wall-clock each. This is the smallest `busy_frames(N)` that
///   reliably clears the single-frame "FEC absorbs everything" floor.
///
/// The `NetProfile::perfect()` control on the same scene shape is what
/// makes the lossy-run assertion non-vacuous: without it, an unfed or
/// miswired counter, or a scene that retransmits unconditionally regardless
/// of the network, would pass the lossy assertion too. Zero here every run
/// (structurally expected: no drops means no unrecoverable FEC group, no
/// coverage gap, no unacked cache entry for RTO to fire on) is the
/// contrast that proves the lossy run's non-zero count means what it
/// claims.
#[tokio::test(start_paused = true)]
async fn retransmits_fire_under_loss_but_not_on_a_perfect_link() {
    async fn run_at(net: NetProfile, seed: u64) -> (u64, u64, u64, u64) {
        let scene = BrowserlessScene {
            seed,
            datagram_send_buffer_bytes: None,
            load: SceneLoad::Script(busy_frames(2)),
            cadence_us: DEFAULT_CADENCE_US,
            net,
            drops: Default::default(),
            duration: Duration::from_secs(10),
            grid_cols: 4,
            grid_rows: 4,
        };
        let r = run_browserless(scene).await.expect("scene ran");
        (
            r.retransmit_attempts_total,
            r.nack_hit,
            r.rto_fired,
            r.bytes_dropped,
        )
    }

    // 20 runs measured: `retransmit_attempts_total` landed at 85 or 86 every
    // time (`nack_hit` either 10 or 0, `rto_fired` making up the rest) —
    // remarkably tight for a harness whose seed doesn't fully determine its
    // random draws (see `feedback_browserless_not_seed_reproducible`).
    // Asserting `> 0` rather than pinning the exact count regardless, since
    // that note says the exact value isn't guaranteed to stay this stable.
    let (lossy_attempts, lossy_nack_hit, lossy_rto_fired, lossy_dropped) = run_at(
        NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        },
        0xDEAD,
    )
    .await;
    assert!(
        lossy_attempts > 0,
        "seed 0xDEAD: 10% loss over a busy 4x4 grid must force at least one \
         retransmit (RTO- or NACK-driven) — zero here means the reliable \
         emitter's retransmit path was never reached, not that the link \
         was quiet (nack_hit={lossy_nack_hit} rto_fired={lossy_rto_fired} \
         bytes_dropped={lossy_dropped})"
    );

    let (perfect_attempts, _, _, perfect_dropped) = run_at(NetProfile::perfect(), 0xBEEF).await;
    assert_eq!(
        perfect_attempts, 0,
        "seed 0xBEEF: NetProfile::perfect() drops nothing (bytes_dropped={perfect_dropped}), \
         so nothing should ever need retransmitting; a non-zero count here \
         would mean this scene retransmits regardless of the network, which \
         would make the assertion above vacuous"
    );
}

/// BWE Stage 2.0 baseline measurement: per-tier (`Critical` = CDF53 passes
/// 0-3, `Refinement` = passes 4-13) ACK round-trip latency, recorded in
/// `docs/specs/bwe-tier-latency-baseline.md`. This is the pre-pacer number
/// Stage 2's pacer restructure has to beat — see that doc and
/// `docs/superpowers/specs/2026-09-10-bwe-pacing-design.md`'s "Baseline
/// first" section.
///
/// Reuses the exact scene shape from
/// `retransmits_fire_under_loss_but_not_on_a_perfect_link` (`busy_frames(2)`,
/// 4x4 grid, 10% loss, 10s duration) — that scene's doc comment records why
/// `busy_frames(2)` is the smallest traffic volume that reliably generates
/// retransmit-worthy concurrent load without occasionally hitting
/// `drive_session`'s `MAX_ITERS` bail-out (`busy_frames(8)` hangs 15-30% of
/// runs; `busy_frames(2)` was 40/40 clean across two batches).
///
/// `#[ignore]`d: this is a measurement tool for humans reading stdout, not
/// a pass/fail regression gate — every run "passes" as long as the scene
/// completes, regardless of the numbers it prints. Run explicitly:
///
/// ```text
/// cargo test -p ghostframe-e2e --test browserless_runner \
///   bwe_tier_latency_baseline -- --ignored --nocapture --test-threads=1
/// ```
#[tokio::test(start_paused = true)]
#[ignore]
async fn bwe_tier_latency_baseline() {
    // `busy_frames(2)` is the smallest traffic volume the neighbouring
    // `retransmits_fire_under_loss_but_not_on_a_perfect_link` doc comment
    // measured as reliable (40/40 clean in that batch), but the harness is
    // not seed-reproducible (`feedback_browserless_not_seed_reproducible`)
    // and `drive_session`'s MAX_ITERS budget exhaustion is a known,
    // healthy-scene outcome under `start_paused` (see that panic message) —
    // it recurred here too, just less often than at `busy_frames(8)`. A
    // budget-exhausted attempt is skipped and retried with the next seed
    // rather than failing the whole measurement run.
    const TARGET_SUCCESSFUL_RUNS: u64 = 10;
    const MAX_ATTEMPTS: u64 = 30;
    let mut successes = 0u64;
    // BWE Stage 2.4 Task 5: cumulative probe-window outcomes across every
    // successful run in this invocation. Reported alongside the latency
    // numbers rather than in a separate test, since it's a "does probing
    // engage on this scene at all" question about the very same runs the
    // latency guard measures. Zero of both across every run means the
    // window never opened on this scene -- goog_cc's `ProbeController`
    // never requested a cluster -- which must be reported as a finding,
    // not silently treated as "probing ran cleanly".
    let mut probes_completed_total = 0u64;
    let mut probes_abandoned_total = 0u64;
    for i in 0..MAX_ATTEMPTS {
        if successes >= TARGET_SUCCESSFUL_RUNS {
            break;
        }
        let seed = 0xB17E_0000_u64.wrapping_add(i);
        let scene = BrowserlessScene {
            seed,
            datagram_send_buffer_bytes: None,
            load: SceneLoad::Script(busy_frames(2)),
            cadence_us: DEFAULT_CADENCE_US,
            net: NetProfile {
                loss: 0.10,
                ..NetProfile::perfect()
            },
            drops: Default::default(),
            duration: Duration::from_secs(10),
            grid_cols: 4,
            grid_rows: 4,
        };
        let r = match run_browserless(scene).await {
            Ok(r) => r,
            Err(e) => {
                println!("seed={seed:#010x} SKIPPED (harness bail, not a measurement): {e}");
                continue;
            }
        };
        successes += 1;
        probes_completed_total += r.probes_completed;
        probes_abandoned_total += r.probes_abandoned;
        println!(
            "seed={seed:#010x} probes: completed={} abandoned={}",
            r.probes_completed, r.probes_abandoned
        );
        println!(
            "seed={seed:#010x} last_sent->ACK critical: count={:>5} mean_us={:>7} max_us={:>7} buckets={:?} \
             | refinement: count={:>5} mean_us={:>7} max_us={:>7} buckets={:?}",
            r.critical_latency_count,
            r.critical_latency_mean_us,
            r.critical_latency_max_us,
            r.critical_latency_buckets,
            r.refinement_latency_count,
            r.refinement_latency_mean_us,
            r.refinement_latency_max_us,
            r.refinement_latency_buckets,
        );
        // BWE Stage 2.1: the same run's `queued_at -> ACK` pair, and the
        // within-run critical/refinement ratio for each metric — see
        // `docs/specs/bwe-tier-latency-baseline.md` for why the ratio,
        // not the absolute mean, is the number that matters here.
        println!(
            "seed={seed:#010x} queued_at->ACK  critical: count={:>5} mean_us={:>7} max_us={:>7} buckets={:?} \
             | refinement: count={:>5} mean_us={:>7} max_us={:>7} buckets={:?}",
            r.queued_critical_latency_count,
            r.queued_critical_latency_mean_us,
            r.queued_critical_latency_max_us,
            r.queued_critical_latency_buckets,
            r.queued_refinement_latency_count,
            r.queued_refinement_latency_mean_us,
            r.queued_refinement_latency_max_us,
            r.queued_refinement_latency_buckets,
        );
        if r.refinement_latency_mean_us > 0 {
            println!(
                "seed={seed:#010x} last_sent->ACK ratio (critical/refinement) = {:.3}",
                r.critical_latency_mean_us as f64 / r.refinement_latency_mean_us as f64
            );
        }
        if r.queued_refinement_latency_mean_us > 0 {
            println!(
                "seed={seed:#010x} queued_at->ACK ratio (critical/refinement) = {:.3}",
                r.queued_critical_latency_mean_us as f64
                    / r.queued_refinement_latency_mean_us as f64
            );
        }
        // Sanity check (see BWE Stage 2.1 task brief): `queued_at -> ACK`
        // starts at or before `last_sent_at -> ACK` for the same
        // population, so its mean/max can never be smaller. A violation
        // here means the plumbing is wrong, not that the network did
        // something surprising — report it rather than silently
        // continuing.
        assert!(
            r.queued_critical_latency_mean_us >= r.critical_latency_mean_us,
            "seed={seed:#010x}: queued_at->ACK critical mean ({}) < last_sent_at->ACK \
             critical mean ({}) — queued_at plumbing is wrong",
            r.queued_critical_latency_mean_us,
            r.critical_latency_mean_us
        );
        assert!(
            r.queued_refinement_latency_mean_us >= r.refinement_latency_mean_us,
            "seed={seed:#010x}: queued_at->ACK refinement mean ({}) < last_sent_at->ACK \
             refinement mean ({}) — queued_at plumbing is wrong",
            r.queued_refinement_latency_mean_us,
            r.refinement_latency_mean_us
        );
        assert!(
            r.queued_critical_latency_max_us >= r.critical_latency_max_us,
            "seed={seed:#010x}: queued_at->ACK critical max ({}) < last_sent_at->ACK \
             critical max ({}) — queued_at plumbing is wrong",
            r.queued_critical_latency_max_us,
            r.critical_latency_max_us
        );
        assert!(
            r.queued_refinement_latency_max_us >= r.refinement_latency_max_us,
            "seed={seed:#010x}: queued_at->ACK refinement max ({}) < last_sent_at->ACK \
             refinement max ({}) — queued_at plumbing is wrong",
            r.queued_refinement_latency_max_us,
            r.refinement_latency_max_us
        );
    }
    println!("{successes}/{TARGET_SUCCESSFUL_RUNS} target runs completed");
    println!(
        "probes across {successes} runs: completed={probes_completed_total} \
         abandoned={probes_abandoned_total}"
    );
    if probes_completed_total == 0 && probes_abandoned_total == 0 {
        println!(
            "NOTE: zero probe windows opened across every run -- this scene \
             never drove goog_cc's ProbeController into requesting a \
             cluster, so it says nothing about whether probing works on a \
             real link"
        );
    }
}

/// Same shape as `busy_frames`, but over a caller-chosen grid instead of a
/// fixed 4x4 — BWE Stage 2.4b's positive probe-fill scene needs enough
/// simultaneous CDF53 demand that goog_cc's first exponential probe window
/// (which opens within the first couple of injected frames — see
/// `docs/specs/bwe-probe-emission-timing.md`) lands while the scheduler
/// still has undrained backlog, rather than an already-empty queue.
fn busy_frames_grid(n: usize, cols: u8, rows: u8) -> Vec<FrameScript> {
    (0..n)
        .map(|i| FrameScript {
            tiles: (0..cols)
                .flat_map(move |x| {
                    (0..rows).map(move |y| {
                        (
                            (x, y),
                            TileSpec::Cdf53 {
                                bgra: shifted_gradient_tile(i as u32, x, y),
                            },
                        )
                    })
                })
                .collect(),
        })
        .collect()
}

/// BWE Stage 2.4b acceptance: on a busy enough link, probe clusters must
/// actually complete — not just open and get abandoned. This is the
/// positive half of the fix's acceptance bar in
/// `docs/specs/bwe-probe-emission-timing.md`.
///
/// The harness is not seed-reproducible
/// (`feedback_browserless_not_seed_reproducible`), and goog_cc's initial
/// exponential probe opens within the first couple of injected frames,
/// while a mix of the fix's immediate drain-on-open *and* ordinary
/// continuation-driven emission (`Event::DatagramsUnblocked` firing in a
/// tight burst under this scene's heavy backlog) can both land inside the
/// 15ms window — this test cannot cleanly attribute a given completion to
/// one or the other. `drain_for_probe_window_open_fills_from_existing_backlog`
/// and `drain_for_probe_window_open_is_a_noop_without_a_prior_drain` in
/// `ghostframe-lib::transport::io_bridge`'s unit tests isolate the fix's
/// mechanism directly and deterministically; this test asserts the
/// outcome-level acceptance criterion the fix exists to satisfy: a real
/// session, driven end-to-end, must be able to complete a probe cluster
/// at all. Measured empirically at 7-8 of 10 seeds completing per batch —
/// summing across seeds and asserting `>= 1` is what "reliably" can mean
/// given the harness's non-reproducibility, matching this file's existing
/// aggregate-across-seeds pattern (see `bwe_tier_latency_baseline`,
/// `retransmits_fire_under_loss_but_not_on_a_perfect_link`).
///
/// **This test does NOT validate `drain_for_probe_window_open`, and must not
/// be read as doing so.** Verified by disabling that call site and re-running:
/// completions are statistically indistinguishable with and without the fix.
/// The browserless harness runs over a socketpair with near-zero RTT, so
/// `DatagramsUnblocked`-driven continuation bursts supply emission
/// opportunities inside a probe window that production — 33.3 ms ticks, real
/// RTT — would not.
///
/// What this test asserts is narrower and still worth asserting: a real
/// session driven end-to-end *can* complete a probe cluster at all, which was
/// false before Stage 2.4. Attribution of the fix lives in the two unit tests
/// named above; they call the method directly and fail if it stops working.
#[tokio::test(start_paused = true)]
async fn probe_windows_are_opened_on_a_busy_link() {
    let mut probes_completed_total = 0u64;
    let mut probes_abandoned_total = 0u64;
    for i in 0..10u64 {
        let seed = 0xF11E_0000_u64.wrapping_add(i);
        let scene = BrowserlessScene {
            seed,
            datagram_send_buffer_bytes: None,
            load: SceneLoad::Script(busy_frames_grid(8, 8, 8)),
            cadence_us: DEFAULT_CADENCE_US,
            net: NetProfile::perfect(),
            drops: Default::default(),
            duration: Duration::from_secs(10),
            grid_cols: 8,
            grid_rows: 8,
        };
        let r = run_browserless(scene).await.expect("scene ran");
        println!(
            "seed={seed:#010x} probes: completed={} abandoned={}",
            r.probes_completed, r.probes_abandoned
        );
        probes_completed_total += r.probes_completed;
        probes_abandoned_total += r.probes_abandoned;
    }
    println!(
        "probe_windows_are_opened_on_a_busy_link: completed={probes_completed_total} \
         abandoned={probes_abandoned_total} across 10 seeds"
    );
    // What this asserted before 2026-09-13 was `probes_completed_total >= 1`,
    // and at the harness's old 16 ms injection cadence that held: ten
    // completions in ten seeds. At production's 33.3 ms it is simply false —
    // zero completions, with *or without* `drain_for_probe_window_open`. The
    // harness was injecting twice as often as production dispatches, so a
    // ~15 ms probe window nearly always contained an emission; at production
    // cadence it contains one less than half the time. See
    // `docs/specs/bwe-probe-emission-timing.md` for the full cadence table.
    //
    // Completion is therefore not something this scene can assert. What it
    // can assert is narrower and still worth guarding: the probe controller
    // is alive and opening windows at all. Zero windows would mean probing
    // stopped being driven — the state this system was actually in before
    // Stage 2.4, when `on_network_availability` was never called.
    assert!(
        probes_completed_total + probes_abandoned_total > 0,
        "a busy 8x8 CDF53 link must at least open probe windows across 10 \
         seeds (completed={probes_completed_total} \
         abandoned={probes_abandoned_total}) — zero of both means the probe \
         controller is not being driven at all, which is a regression \
         distinct from windows merely under-filling"
    );
}

/// BWE Stage 2.4b's negative control: a scene with genuinely insufficient
/// demand must still show the probe window opening and being abandoned —
/// `probes_abandoned` moving while `probes_completed` stays at zero. Without
/// this, `probe_windows_are_opened_on_a_busy_link`'s assertion could be
/// satisfied by a bridge that (incorrectly) always completes every probe
/// window regardless of demand, e.g. by padding — which the design
/// explicitly forbids (see `docs/specs/bwe-probe-emission-timing.md`'s "No
/// padding" section).
///
/// Uses a 2x2 grid rather than `bwe_tier_latency_baseline`'s 4x4 one,
/// because a negative control has to be *robustly* starved, not marginally
/// so. The 4x4 shape was neither: instrumenting `close_probe_window`
/// showed its busiest window reaching 18619 bytes against a 22500-byte
/// `min_bytes` — 83% of the way to completing, and passing `min_probes`
/// (5) forty-five times over at 224 packets. It stayed under the bar on
/// byte count alone, with under 4 KB of headroom, so any timing
/// perturbation tipped it: it began failing in CI purely from a build
/// profile change that altered task interleaving across the harness's real
/// socketpair, with no behavioural change at all. At 2x2 the busiest
/// window reaches 39% of `min_bytes` and most reach 0%, which is the
/// margin this control needs to mean anything.
///
/// The scene still carries real traffic (56 packets in that busiest
/// window) and still opens and abandons two windows per seed, so the
/// padding it is written to detect would be just as visible — more so, in
/// fact, since padding would now have to manufacture the missing 61%.
#[tokio::test(start_paused = true)]
async fn probe_windows_are_abandoned_on_a_demand_starved_link() {
    let mut probes_completed_total = 0u64;
    let mut probes_abandoned_total = 0u64;
    for i in 0..5u64 {
        let seed = 0xB17E_1000_u64.wrapping_add(i);
        let scene = BrowserlessScene {
            seed,
            datagram_send_buffer_bytes: None,
            load: SceneLoad::Script(busy_frames_grid(2, 2, 2)),
            cadence_us: DEFAULT_CADENCE_US,
            net: NetProfile {
                loss: 0.10,
                ..NetProfile::perfect()
            },
            drops: Default::default(),
            duration: Duration::from_secs(10),
            grid_cols: 2,
            grid_rows: 2,
        };
        let r = run_browserless(scene).await.expect("scene ran");
        println!(
            "seed={seed:#010x} probes: completed={} abandoned={}",
            r.probes_completed, r.probes_abandoned
        );
        probes_completed_total += r.probes_completed;
        probes_abandoned_total += r.probes_abandoned;
    }
    assert!(
        probes_abandoned_total > 0,
        "a demand-starved scene must still open and abandon at least one \
         probe window across 5 seeds (completed={probes_completed_total} \
         abandoned={probes_abandoned_total}) — zero here means the window \
         never opened at all, which says nothing about padding"
    );
    assert_eq!(
        probes_completed_total, 0,
        "this scene has genuinely insufficient demand to fill a probe \
         window; any completion here (completed={probes_completed_total}) \
         would mean either padding crept in, or the scene has accidentally \
         gained enough backlog to no longer serve as a negative control"
    );
}

/// BWE/pacing acceptance, criterion 2: **no pass starvation under sustained
/// pressure**.
///
/// `every_cdf53_pass_eventually_lands` covers "every pass of one frame
/// eventually lands", which is a different and weaker property: it sends a
/// single frame and gives it 20 s of quiet to converge, so nothing ever
/// competes with its refinement passes. Starvation is precisely what
/// happens when something does.
///
/// Here the left half of the grid is rewritten every tick for 200 frames
/// while the right half is written once and never again, on a link too slow
/// to carry the left half (400 kB/s against the offered load, 5% loss).
/// The right half's refinement passes are therefore competing with a
/// saturating stream of fresher, higher-priority work for the whole scene.
/// If the scheduler starves them, those tiles never reach lossless.
///
/// This deliberately does *not* assert on `refinement_latency_max_us`.
/// Measured at 16.2 s here, which looks alarming and is correct: under
/// sustained pressure refinement is *deprioritized*, and a bound on that
/// latency would be asserting a scheduling policy rather than the absence
/// of starvation. The distinction that matters is whether the passes
/// eventually land at all.
#[tokio::test(start_paused = true)]
async fn a_static_region_still_refines_while_a_busy_one_saturates() {
    const COLS: u8 = 8;
    const ROWS: u8 = 8;
    /// Columns at or above this index are written once, in frame 0, then
    /// never again — so anything that reaches them afterwards is refinement.
    const STATIC_FROM: u8 = 4;
    const FRAMES: usize = 200;

    let frames: Vec<FrameScript> = (0..FRAMES)
        .map(|i| FrameScript {
            tiles: (0..COLS)
                .flat_map(|x| (0..ROWS).map(move |y| (x, y)))
                .filter(|(x, _)| i == 0 || *x < STATIC_FROM)
                .map(|(x, y)| {
                    let bgra = if x < STATIC_FROM {
                        shifted_gradient_tile(i as u32, x, y)
                    } else {
                        gradient_tile()
                    };
                    ((x, y), TileSpec::Cdf53 { bgra })
                })
                .collect(),
        })
        .collect();

    let scene = BrowserlessScene {
        seed: 0xF00D_5747,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(frames),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            loss: 0.05,
            cap: CapTimeline::constant(400_000),
            bottleneck: Some(Bottleneck::wifi()),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(20),
        grid_cols: COLS as u32,
        grid_rows: ROWS as u32,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    // The scene must actually be under pressure, or there is no starvation
    // to observe and this passes vacuously.
    assert!(
        r.bytes_dropped > 0,
        "the busy half must actually saturate the link; dropped={}",
        r.bytes_dropped
    );

    let want = expected_rgba(&gradient_tile());
    let mut starved = Vec::new();
    for x in STATIC_FROM..COLS {
        for y in 0..ROWS {
            if r.framebuffer.tile_rgba(x, y) != Some(want.as_slice()) {
                starved.push((x, y));
            }
        }
    }
    assert!(
        starved.is_empty(),
        "seed 0xF00D5747: {} static tiles never converged to lossless while the \
         busy half saturated the link: {:?} -- their refinement passes were \
         starved (refinement_latency_max={}us, dropped={})",
        starved.len(),
        starved,
        r.refinement_latency_max_us,
        r.bytes_dropped
    );
}

/// A 32x32 BGRA gradient tile, so Cdf53 passes carry real, distinct
/// bit-plane content rather than a uniform tile's near-identical passes.
/// Matches `tests/framebuffer.rs`'s `gradient_bgra` pixel-for-pixel.
fn gradient_tile() -> Vec<u8> {
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = ((x * 8) % 256) as u8;
            let g = ((y * 8) % 256) as u8;
            let r = (((x + y) * 4) % 256) as u8;
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    bgra
}

/// The BGRA -> RGBA swizzle every codec's decode path applies, alpha forced
/// to 255 — matches `ghostframe-client-core/src/reassembly.rs`'s
/// `finish_assembly` (the Raw-codec arm) and, for Cdf53 specifically,
/// `Cdf53TileState::integrate`'s BGR -> RGB conversion plus forced alpha.
fn expected_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = vec![0u8; bgra.len()];
    for (chunk_in, chunk_out) in bgra.chunks_exact(4).zip(rgba.chunks_exact_mut(4)) {
        chunk_out[0] = chunk_in[2]; // R
        chunk_out[1] = chunk_in[1]; // G
        chunk_out[2] = chunk_in[0]; // B
        chunk_out[3] = 255; // alpha forced
    }
    rgba
}

/// `n` frames, each rewriting a fixed 4x4 (16-tile) grid with
/// `TileSpec::Cdf53` content that varies frame-to-frame, for scene 3
/// (`goodput_tracks_a_bandwidth_step_down`). The per-tile content is a
/// gradient tile with each channel offset by the frame index (mod 256) and
/// by the tile's own coordinates, so no two frames — and no two tiles
/// within a frame — encode to identical bytes; this keeps every one of the
/// 14 progressive passes real, varying content instead of degenerate
/// all-zero or duplicate bit-planes the bandwidth cap could trivially
/// absorb without actually shedding anything.
fn busy_frames(n: usize) -> Vec<FrameScript> {
    (0..n)
        .map(|i| FrameScript {
            tiles: (0..4u8)
                .flat_map(move |x| {
                    (0..4u8).map(move |y| {
                        (
                            (x, y),
                            TileSpec::Cdf53 {
                                bgra: shifted_gradient_tile(i as u32, x, y),
                            },
                        )
                    })
                })
                .collect(),
        })
        .collect()
}

/// A 32x32 BGRA gradient tile whose channels are offset by `shift`
/// (typically a frame index) and by tile coordinates `tile_x`/`tile_y`, so
/// `busy_frames` produces distinct, non-degenerate content per tile per
/// frame. See `busy_frames`'s doc comment for why that matters.
fn shifted_gradient_tile(shift: u32, tile_x: u8, tile_y: u8) -> Vec<u8> {
    let off = shift
        .wrapping_add(tile_x as u32 * 17)
        .wrapping_add(tile_y as u32 * 31);
    let mut bgra = Vec::with_capacity(32 * 32 * 4);
    for y in 0..32u32 {
        for x in 0..32u32 {
            let b = (((x * 8) + off) % 256) as u8;
            let g = (((y * 8) + off * 3) % 256) as u8;
            let r = ((((x + y) * 4) + off * 5) % 256) as u8;
            bgra.extend_from_slice(&[b, g, r, 255]);
        }
    }
    bgra
}

/// The link must carry datagrams concurrently: one in flight must not
/// block the ones behind it.
///
/// This exists because it did block them. Delivery used to be awaited at
/// the point of sending, making the link strictly serial — one datagram in
/// flight at a time, the next not even ruled on by `NetSim` until the
/// previous had landed. At `delay_us: 0`, which every other scene in this
/// file uses, that await returns immediately and the shape is invisible;
/// that is why the whole suite passed over it.
///
/// At a realistic RTT it was not merely slow. The client->server transmit
/// drain checks neither `MAX_ITERS` nor `overall_deadline` — both guards
/// live in the outer loop — so the scene spent `backlog x delay_us` of
/// virtual time inside a single outer iteration with no bail-out reachable.
/// Measured at 10ms one-way: no progress in 400s, loop counter frozen at
/// iteration 200 while virtual time advanced exactly one datagram per 10ms.
///
/// The assertion is a throughput floor rather than a timeout because the
/// pre-fix failure was a wall-clock hang, and a hang cannot be asserted
/// against under a paused clock — the test would hang too. A serialised
/// link can deliver at most one datagram per `delay_us`, so
/// `duration / delay_us` datagrams is a hard ceiling for the broken shape.
/// Clearing it by a wide margin is only possible if datagrams overlap in
/// flight.
#[tokio::test(start_paused = true)]
async fn a_link_with_propagation_delay_carries_datagrams_concurrently() {
    // 100 ms RTT: a plausible intercontinental path, and -- unlike the 40 ms
    // this used -- one where the serialised ceiling is genuinely
    // discriminating. Delivery here is bandwidth-limited at ~460 kbps, i.e.
    // one ~1130-byte datagram per ~19.6 ms. Against a 20 ms one-way delay
    // that put the serialised ceiling (500) within 1% of actual delivery
    // (504-509 across runs): the test cleared it by coincidence rather than
    // by demonstrating overlap, and was one small shift away from flaking.
    // At 50 ms the ceiling is 200 and delivery is ~500, so clearing it
    // requires datagrams to genuinely be in flight concurrently.
    const ONE_WAY_US: u64 = 50_000;
    const DURATION_US: u64 = 10_000_000;

    let scene = BrowserlessScene {
        seed: 0x5D1A_7E00,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(busy_frames_grid(8, 8, 8)),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile {
            delay_us: ONE_WAY_US,
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_micros(DURATION_US),
        grid_cols: 8,
        grid_rows: 8,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    // A link that never established would deliver few bytes for reasons
    // that have nothing to do with concurrency, so pin that down first.
    assert!(
        r.events.contains(&ClientNetEvent::SessionReady),
        "the delayed link must still establish a session; events={:?}",
        r.events
    );

    // Ceiling for a serialised link: one datagram per one-way delay.
    //
    // Counted in datagrams, not bytes. The byte form of this assertion was a
    // proxy -- "more than N datagrams" expressed as "more than N x 1200
    // bytes" -- and it silently depended on payloads staying near that
    // bound. Switching the Cdf53 encoder to skip empty bit-planes shrank
    // payloads and dropped delivery to 581,755 bytes against a 600,000 floor
    // while concurrency was completely unchanged, i.e. the proxy failed for a
    // reason the test does not care about. Datagram count is what the
    // serialisation argument is actually about, so assert on that directly.
    let serial_max_datagrams = DURATION_US / ONE_WAY_US;

    println!(
        "delayed link: datagrams_delivered_s2c={} vs serialised ceiling={} \
         ({} bytes delivered)",
        r.datagrams_delivered_s2c, serial_max_datagrams, r.bytes_delivered
    );
    assert!(
        r.datagrams_delivered_s2c > serial_max_datagrams,
        "a {}ms-RTT link delivered {} server->client datagrams, at or below \
         the {} a strictly serialised link could manage. Delivery has \
         regressed to awaiting each datagram's arrival at the point it is \
         sent.",
        ONE_WAY_US * 2 / 1000,
        r.datagrams_delivered_s2c,
        serial_max_datagrams
    );
}

/// Run a sustained production-cadence scene over a queueing bottleneck of a
/// given capacity, and report what the estimator made of it.
async fn run_bottleneck_scene(
    seed: u64,
    cap: CapTimeline,
    secs: u64,
) -> ghostframe_e2e::harness::browserless::BrowserlessResult {
    let scene = BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Profile(LoadProfile {
            cadence_us: PRODUCTION_CADENCE_US,
            churn: Churn::Region { tiles_per_tick: 32 },
        }),
        cadence_us: PRODUCTION_CADENCE_US,
        net: NetProfile {
            delay_us: 10_000,
            cap,
            bottleneck: Some(Bottleneck::wifi()),
            ..NetProfile::perfect()
        },
        drops: Default::default(),
        duration: Duration::from_secs(secs),
        grid_cols: 16,
        grid_rows: 16,
    };
    run_browserless(scene).await.expect("scene ran")
}

/// The estimator must tell a congested link from an uncongested one.
///
/// This deliberately does **not** assert convergence to the link rate, which
/// would be false: on the congested link the estimate sits on goog_cc's
/// `MIN_BPS` floor (200 kbps), and on the uncongested one it stays near its
/// 2 Mbps seed because nothing ever signals congestion. Asserting "within a
/// factor of capacity" would fail on both sides for opposite reasons.
///
/// What it does assert is the property that makes an estimator an estimator:
/// a link that queues and drops must produce a materially lower estimate than
/// one with headroom to spare. Measured separation is ~10x with no run-to-run
/// variance, so the 2x threshold here has a wide margin.
///
/// On the bottleneck's role, stated precisely, because an induced-failure
/// check refuted the stronger claim this comment first made: the *estimate
/// separation* above is visible against the old drop-without-queueing cap
/// too (201 kbps vs 2.02 Mbps measured). What the bottleneck changes is the
/// `bytes_dropped` guards. A token bucket drops bursts even on a link with
/// ample headroom — 1,200 bytes shed from the spacious scene — which is not
/// how an uncongested link behaves, so `dropped == 0` is assertable only
/// against a queue. It also makes the scene reproducible: queue occupancy is
/// a deterministic function of arrivals, where the bucket's drop decisions
/// were timing-sensitive and gave estimates spanning 215 kbps to 2.0 Mbps on
/// identical inputs.
///
/// The queueing model still matters for the wider point — without a
/// queuing-delay gradient goog_cc's delay-based half never runs at all — but
/// that is not what *this* assertion rests on. See
/// `docs/specs/bwe-probe-emission-timing.md`.
#[tokio::test(start_paused = true)]
async fn the_estimate_separates_a_congested_link_from_an_uncongested_one() {
    // Offered load is ~1.6 Mbps, so 480 kbps congests and 3.2 Mbps does not.
    let congested = run_bottleneck_scene(0xC0FF_EE01, CapTimeline::constant(500_000), 20).await;
    let spacious = run_bottleneck_scene(0xC0FF_EE01, CapTimeline::constant(2_000_000), 20).await;

    println!(
        "congested: est={} pacer={:?} s2c={} c2s={} dropped={} retx={}\n\
         spacious:  est={} pacer={:?} s2c={} c2s={} dropped={} retx={}",
        congested.bwe_estimate_bps,
        congested.pacer_rate_bps,
        congested.bytes_delivered_s2c,
        congested.bytes_delivered_c2s,
        congested.bytes_dropped,
        congested.retransmit_attempts_total,
        spacious.bwe_estimate_bps,
        spacious.pacer_rate_bps,
        spacious.bytes_delivered_s2c,
        spacious.bytes_delivered_c2s,
        spacious.bytes_dropped,
        spacious.retransmit_attempts_total,
    );

    // The scenes must actually be what they claim, or the comparison below
    // is between two identical links and proves nothing.
    assert!(
        congested.bytes_dropped > 0,
        "the congested scene must overflow its buffer; dropped={}",
        congested.bytes_dropped
    );
    assert_eq!(
        spacious.bytes_dropped, 0,
        "the spacious scene must have headroom to spare, but dropped {}",
        spacious.bytes_dropped
    );

    assert!(
        spacious.bwe_estimate_bps > congested.bwe_estimate_bps * 2,
        "a link with headroom must estimate materially higher than a congested \
         one: spacious={} congested={}",
        spacious.bwe_estimate_bps,
        congested.bwe_estimate_bps
    );
}

/// Capacity triples mid-session: does the estimate find the new headroom?
///
/// This is the scenario probing exists for, and the one no test covered.
/// A session that starts congested and is then handed room to grow has to
/// *discover* that room — nothing tells it. goog_cc's answer is to probe:
/// send a short burst above the current estimate and read the ACKs.
///
/// Probe clusters complete here, which is itself new: at production cadence
/// over the old drop-without-queueing cap they never did. So probing works
/// and any slow ramp is not explained by probe abandonment — which bounds
/// how much `drain_for_probe_window_open` could be worth, the open question
/// in `docs/specs/bwe-probe-emission-timing.md`.
///
/// # Why the thresholds are what they are
///
/// This test was ~50% flaky until 2026-09-20, and re-measuring showed the
/// cause was not noise but two threshold choices that did not match the
/// behaviour being measured.
///
/// **The acceptance level.** Measured final estimates on this 16 Mbps
/// post-step link, 8 runs each:
///
/// | | final estimate |
/// |---|---|
/// | with `set_transport_capacity_hint` | 12.0 - 18.2 Mbps |
/// | with the hint disabled | 7.0 - 7.2 Mbps |
///
/// The old 80% bar (12.8 Mbps) cut straight through the *with-hint* range,
/// so the test was a coin flip on its own success case. 60% (9.6 Mbps) sits
/// in the gap between the two populations: 34% above the no-hint maximum and
/// 20% below the with-hint minimum. It is a weaker-sounding number that
/// discriminates strictly better, and it is still far more than the
/// "doubling" an earlier version asserted — a 2 -> 4.1 Mbps move would not
/// come close.
///
/// **The observation window.** The scene ran 20 s with the step at 4 s, so it
/// could only ever watch 16 s of recovery — against an 18 s bound. Part of
/// the acceptance range was unobservable by construction. The scene is now
/// 26 s, giving 22 s of post-step observation against the same 18 s bound.
///
/// Time-to-converge at the 60% level now measures 4.4 - 10.1 s across 8 runs,
/// so 18 s carries real margin. Note the bound is no longer what guards
/// against the hint regressing: with the hint disabled the estimate does not
/// reach 60% *at all* within the scene, in any run. Convergence happening is
/// the discriminator; the bound just keeps "eventually" honest.
#[tokio::test(start_paused = true)]
async fn the_estimate_follows_a_mid_scene_capacity_step_up() {
    const LOW: u64 = 500_000; // bytes/s -> 4 Mbps, well below the ~10 Mbps offered
    const HIGH: u64 = 2_000_000; // bytes/s -> 16 Mbps, ample headroom
    const STEP_AT_US: u64 = 4_000_000;
    /// The acceptance bound: the estimate must reach `CONVERGED_FRACTION` of
    /// the new capacity within this long after the step.
    ///
    /// Measured 4.4-10.1 s across 8 runs at the 60% level, so this carries
    /// roughly 80% margin over the observed worst case. It must also stay
    /// under the post-step observation window (26 s scene - 4 s step = 22 s),
    /// or part of the range it admits can never be observed — which is
    /// exactly what made the old 18 s bound unreachable against a 16 s
    /// window.
    ///
    /// Before `set_transport_capacity_hint` fed goog_cc an independent
    /// ceiling, the estimate could only climb by `AimdRateControl`'s
    /// multiplicative increase, which hardcodes `alpha = 1.08` capped to one
    /// second of effect (8% per second). ALR probing cannot substitute for
    /// it: `time_for_alr_probe` fires only when the application is
    /// under-sending, which is exactly when there is too little traffic to
    /// fill a probe cluster. Disabling the hint and re-running confirms it:
    /// the estimate plateaus at ~7.1 Mbps and never reaches this bar.
    const CONVERGE_BY_US: u64 = 18_000_000;
    /// See the "Why the thresholds are what they are" section above: this
    /// sits in the measured gap between the with-hint and no-hint
    /// populations, where 0.8 sat inside the with-hint spread.
    const CONVERGED_FRACTION: f64 = 0.6;

    let r = run_bottleneck_scene(
        0xC0FF_EE02,
        CapTimeline::step(LOW, STEP_AT_US, HIGH),
        // 26 s, not 20: the step lands at 4 s, so this is what makes the
        // post-step observation window (22 s) longer than CONVERGE_BY_US.
        std::env::var("GF_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(26),
    )
    .await;

    let median = |mut v: Vec<u64>| -> u64 {
        v.sort_unstable();
        v[v.len() / 2]
    };

    // Timestamped series, so a window artifact cannot be mistaken for
    // estimator behaviour. An earlier version of this test used a fixed
    // [4s, 5s) window for the pre-step value and was flaky for exactly that
    // reason: how long the estimate stays at its 2 Mbps seed before
    // congestion is detected varies run to run, so the window sometimes
    // averaged the seed instead of the converged floor.
    let series: Vec<String> = r
        .bwe_estimate_samples
        .iter()
        .map(|(t, b)| format!("{}ms:{}", t / 1000, b))
        .collect();
    println!("step-up series: {}", series.join(" "));

    // Pre-step level, measured from 1 s in so the 2 Mbps seed has had time
    // to be replaced by something the link actually justifies. This used to
    // assert the estimate was pinned at `MIN_BPS` exactly, which only held
    // because the old scene offered so little that goog_cc was driven into
    // its floor and parked there. A scene that genuinely saturates its link
    // settles on a real value instead (~1.3 Mbps on the 4 Mbps cap), so
    // "did it climb" is both the honest question and the one with margin.
    let pre_step: Vec<u64> = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| *t >= 1_000_000 && *t < STEP_AT_US)
        .map(|(_, b)| *b)
        .collect();
    // The last three seconds: goog_cc does not react instantly and the
    // question is whether it gets there at all, not how fast.
    let tail: Vec<u64> = r
        .bwe_estimate_samples
        .iter()
        .filter(|(t, _)| *t >= r.bwe_estimate_samples.last().unwrap().0 - 3_000_000)
        .map(|(_, b)| *b)
        .collect();

    println!(
        "step-up: converged_at={:?}ms pre_step(median)={} tail(median)={} probes={}/{} pacer={:?}",
        r.bwe_estimate_samples
            .iter()
            .find(|(t, b)| *t >= STEP_AT_US
                && (*b as f64) >= (HIGH * 8) as f64 * CONVERGED_FRACTION)
            .map(|(t, _)| (t - STEP_AT_US) / 1000),
        median(pre_step.clone()),
        median(tail.clone()),
        r.probes_completed,
        r.probes_abandoned,
        r.pacer_rate_bps,
    );

    assert!(
        !pre_step.is_empty(),
        "need estimate samples between 1 s and the step at {STEP_AT_US} us"
    );
    // The pre-step link carries 4 Mbps against ~10 Mbps offered, so the
    // estimate must sit at or below that — an estimate anywhere near the
    // post-step capacity would mean the scene never congested and there is
    // no headroom discovery to observe.
    //
    // Bounded by the *actual* pre-step capacity rather than a hand-picked
    // number: an estimate above the link's real rate is an overestimate,
    // which is a bug in its own right and the dangerous direction. An
    // earlier version used a flat 3 Mbps and started failing when the
    // estimator got *better* (1.9 -> 3.03 Mbps on a 4 Mbps link), which is
    // the wrong thing for a test to punish.
    assert!(
        median(pre_step.clone()) <= LOW * 8,
        "the pre-step link carries {} bits/s; an estimate above that is an \
         overestimate, not congestion: pre_step(median)={}",
        LOW * 8,
        median(pre_step.clone())
    );
    assert!(
        !tail.is_empty(),
        "need estimate samples in the final seconds of the scene"
    );
    // The acceptance criterion proper: not merely "it moved", but that it
    // reached the new capacity, and did so within a stated bound. An earlier
    // version asserted only a doubling, which a 2 -> 4.1 Mbps move on a
    // 16 Mbps link would have satisfied while missing the cap four-fold.
    let target_bps = (HIGH * 8) as f64 * CONVERGED_FRACTION;
    let converged_at = r
        .bwe_estimate_samples
        .iter()
        .find(|(t, b)| *t >= STEP_AT_US && (*b as f64) >= target_bps)
        .map(|(t, _)| t - STEP_AT_US);

    match converged_at {
        Some(dt) => assert!(
            dt <= CONVERGE_BY_US,
            "capacity went {} -> {} bits/s at {STEP_AT_US}us; the estimate reached \
             {:.0}% of it only after {}ms, past the {}ms bound. pre_step(median)={} \
             tail(median)={}",
            LOW * 8,
            HIGH * 8,
            CONVERGED_FRACTION * 100.0,
            dt / 1000,
            CONVERGE_BY_US / 1000,
            median(pre_step.clone()),
            median(tail.clone())
        ),
        None => panic!(
            "capacity went {} -> {} bits/s at {STEP_AT_US}us and the estimate never \
             reached {:.0}% of it before the scene ended. pre_step(median)={} \
             tail(median)={}. Never discovering the new headroom is a finding, \
             not a flaky test",
            LOW * 8,
            HIGH * 8,
            CONVERGED_FRACTION * 100.0,
            median(pre_step.clone()),
            median(tail.clone())
        ),
    }
}

/// The case no receiver-driven mechanism can cover: a tile emitted exactly
/// once, whose only datagram is dropped. The client builds no assembly and
/// no coverage entry, so it cannot NACK — it does not know the tile exists.
///
/// Only a sender-side repair can render this tile. That makes this test the
/// direct evidence for whether `drain_priority_queue`'s 2xRTT `InFlight`
/// retry actually fires end-to-end, which had never been observed when the
/// repair redesign was specified.
#[tokio::test(start_paused = true)]
async fn a_solid_tile_whose_only_datagram_is_dropped_is_still_repaired() {
    let scene = BrowserlessScene {
        seed: 0x0D30_0001,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: vec![
                (
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [10, 20, 30, 255],
                    },
                ),
                (
                    (1, 1),
                    TileSpec::Solid {
                        bgra: [40, 50, 60, 255],
                    },
                ),
            ],
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        // Lossless apart from the one deliberate drop, so anything missing
        // is attributable to that drop alone.
        net: NetProfile::perfect(),
        drops: DropPlan::new(vec![DropRule {
            tile_x: 1,
            tile_y: 1,
            occurrences: vec![0],
        }]),
        duration: Duration::from_secs(5),
        grid_cols: 4,
        grid_rows: 4,
    };
    let result = run_browserless(scene).await.expect("scene ran");

    // Premise check: the injected drop must actually have fired. Without
    // this, the test passes when the drop silently never matched -- the
    // failure mode this whole plan exists to avoid, and the one that made an
    // entire earlier version of this feature inert.
    //
    // Use `drops_fired`, NOT `bytes_dropped`. The plan is consulted in
    // `IoBridge::send_to_all_sessions`, upstream of the netsim, so a
    // plan-dropped datagram never reaches the simulated link and is never
    // counted there. Measured on exactly this scene shape:
    // `drops_fired=[1] bytes_dropped=0`.
    assert_eq!(
        result.drops_fired,
        vec![1],
        "the injected drop never fired, so this test never created the case \
         it claims to test -- check the DropRule's coordinates against what \
         the scene actually emits"
    );

    // Control: the undropped tile proves the scene worked at all.
    assert!(
        result.framebuffer.tile_rgba(0, 0).is_some(),
        "control tile (0,0) never arrived -- the scene itself is broken, \
         so this test proves nothing about repair"
    );

    assert!(
        result.framebuffer.tile_rgba(1, 1).is_some(),
        "tile (1,1) had its only datagram dropped and was never repaired. \
         The client cannot NACK it: with nothing received it has no \
         assembly and no coverage entry, so it does not know the tile \
         exists. Only a sender-side repair can recover this."
    );
}

/// Production reproduction: a screen that goes static must stop asking.
///
/// Observed on a live session with a static desktop and nothing to send --
/// the server reported `dirty_count=0`, `dispatched_tiles=0`, `wire_bytes=0`
/// on every frame -- while the client had sent **106,847 NACKs** and the
/// server had performed **94,834 retransmissions** against 53,761
/// acknowledgements. More retransmission than delivery, for a picture that
/// was not changing.
///
/// The suspected mechanism is `ClientCore::tail_sweep`, which every 500 ms
/// re-NACKs every pass missing from `FULL_PASS_MASK` -- all **14** of them --
/// for any tile it has a coverage entry for. If a tile legitimately receives
/// fewer than 14 passes, the sweep asks for the remainder forever, and the
/// server answers from a cache that no longer holds them.
///
/// This asserts the property directly: once a static scene has converged,
/// NACK traffic must stop growing. A scene that keeps NACKing while nothing
/// moves is the bug, whatever its cause.
#[tokio::test(start_paused = true)]
async fn a_static_screen_stops_asking_for_passes() {
    let scene = BrowserlessScene {
        seed: 0x57A1_0001,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: (0..4u8)
                .flat_map(|x| {
                    (0..4u8).map(move |y| {
                        (
                            (x, y),
                            TileSpec::Cdf53 {
                                bgra: gradient_tile(),
                            },
                        )
                    })
                })
                .collect(),
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        // Lossless: nothing is missing, so every NACK is the client asking
        // for something it was never going to get.
        net: NetProfile::perfect(),
        drops: Default::default(),
        // Long enough for tail_sweep (500 ms) to fire ~20 times after the
        // single frame has been fully delivered.
        duration: Duration::from_secs(12),
        grid_cols: 4,
        grid_rows: 4,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    println!(
        "static-screen: nack_hit={} nack_miss={} retransmits={} s2c={}",
        r.nack_hit, r.nack_miss, r.retransmit_attempts_total, r.bytes_delivered_s2c
    );

    // 16 tiles x 14 passes = 224 passes, delivered once on a perfect link.
    // A handful of NACKs during convergence is normal; hundreds means the
    // client is asking for passes that will never come.
    let total_nacks = r.nack_hit + r.nack_miss;
    assert!(
        total_nacks < 64,
        "a static 16-tile scene on a lossless link produced {total_nacks} NACKs \
         ({} hit, {} miss) over 12 s. Nothing was lost, so the client is asking \
         for passes it will never receive -- see tail_sweep's FULL_PASS_MASK, \
         which expects all 14 passes for every tile it has ever seen.",
        r.nack_hit,
        r.nack_miss
    );
}

/// Production reproduction: the client never gives up asking for a pass it
/// cannot get.
///
/// `ClientCore::tail_sweep` runs every 500 ms and re-NACKs every pass missing
/// from `FULL_PASS_MASK` -- all 14 -- for any tile it holds a coverage entry
/// for, clearing `nacked_mask` each time so the request repeats. There is no
/// attempt limit and no give-up. A pass that will never arrive is therefore
/// requested for the entire life of the session.
///
/// Seen in production on a *static* screen with nothing to send: the server
/// reported `dirty_count=0` and `wire_bytes=0` every frame, while the client
/// had sent **106,847 NACKs** and the server had made **94,834
/// retransmissions** against 53,761 acknowledgements -- roughly 77% of those
/// NACKs finding nothing in the server's cache.
///
/// Here one tile's passes are dropped unconditionally, so they can never
/// land. The assertion is not that the tile recovers -- it cannot -- but that
/// the client stops asking.
#[tokio::test(start_paused = true)]
async fn the_client_gives_up_on_a_pass_it_can_never_get() {
    let scene = BrowserlessScene {
        seed: 0xDEAD_0002,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![FrameScript {
            tiles: vec![
                (
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [10, 20, 30, 255],
                    },
                ),
                (
                    (1, 1),
                    TileSpec::Cdf53 {
                        bgra: gradient_tile(),
                    },
                ),
            ],
        }]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        // The tile's first six passes land, so the client builds a coverage
        // entry and knows the tile exists. Everything after that -- original
        // or retransmitted -- is dropped, so the remaining passes are
        // unobtainable by construction. A tile that receives *nothing* is
        // never NACKed at all (no coverage entry), which is a different and
        // already-known blind spot.
        drops: DropPlan::new(vec![DropRule {
            tile_x: 1,
            tile_y: 1,
            occurrences: (6..100_000).collect(),
        }]),
        duration: Duration::from_secs(24),
        grid_cols: 4,
        grid_rows: 4,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    let total_nacks = r.nack_hit + r.nack_miss;
    println!(
        "give-up: drops_fired={:?} nack_hit={} nack_miss={} total={} retransmits={}",
        r.drops_fired, r.nack_hit, r.nack_miss, total_nacks, r.retransmit_attempts_total
    );

    // Eight passes are missing. Asking a handful of times each before
    // concluding the path will not deliver them is reasonable; the bound
    // below allows five attempts per pass. What must NOT happen is a count
    // that scales with how long the session has been open -- measured at
    // ~56 over 12 s and ~112 over 24 s, i.e. exactly linear in duration,
    // which is the signature of a request loop with no terminating
    // condition.
    // Structural bound: at most CDF53_PASS_COUNT (14) passes, each asked for
    // at most MAX_TAIL_SWEEP_ATTEMPTS (6) times = 84. Measured 48 here (8
    // passes actually missing x 6), and -- the property that matters --
    // identical at 12 s, 24 s and 48 s. Before the give-up it read 56 / 120 /
    // ~240, i.e. linear in how long the session had been open.
    assert!(
        total_nacks <= 84,
        "the client sent {total_nacks} NACKs ({} hit, {} miss) over 12 s for a \
         single tile whose passes can never arrive. tail_sweep re-requests \
         every missing pass every 500 ms with no attempt limit, so an \
         unobtainable pass is requested for the life of the session -- and \
         each request costs the server a cache lookup and, when it hits, a \
         retransmission that will also be dropped.",
        r.nack_hit,
        r.nack_miss
    );
}

/// Production reproduction: a second full-screen dirty frame arriving while
/// the first frame's refinement is still in flight leaves tiles permanently
/// short of a complete pass set.
///
/// `bump_generation` supersedes a tile's queued refinement work, which is
/// correct when the content changed -- those passes describe a stale
/// picture. But the client's completeness criterion is
/// `FULL_PASS_MASK = (1 << 14) - 1`: it waits for all 14 passes and has no
/// way to learn that fewer are coming, because `TileHeader` carries `pass`
/// but not `total_passes`. A tile whose refinement was superseded therefore
/// never completes on the client, however long it waits.
///
/// Measured in production on a 1920x1080 session: `dirty_count` was 2040 on
/// exactly two frames and 0 on the other 26,196, and `emitted_cdf53` froze
/// at 24,094 against the 2040 x 14 = 28,560 a full refinement needs -- 4,466
/// passes short, permanently. The screen never converged.
#[tokio::test(start_paused = true)]
async fn refinement_completes_when_a_second_frame_supersedes_the_first() {
    let a = gradient_tile();
    let mut b = gradient_tile();
    // Make frame 2 genuinely different so it dirties every tile.
    for (i, px) in b.iter_mut().enumerate() {
        *px = px.wrapping_add(((i % 7) as u8) + 11);
    }

    let tiles_of = |bgra: &Vec<u8>| -> Vec<((u8, u8), TileSpec)> {
        (0..4u8)
            .flat_map(|x| {
                let bgra = bgra.clone();
                (0..4u8).map(move |y| ((x, y), TileSpec::Cdf53 { bgra: bgra.clone() }))
            })
            .collect()
    };

    let scene = BrowserlessScene {
        seed: 0x50BE_5EED,
        datagram_send_buffer_bytes: None,
        load: SceneLoad::Script(vec![
            FrameScript {
                tiles: tiles_of(&a),
            },
            // Second full-screen dirty frame, back-to-back: its generation
            // bump supersedes frame 1's still-queued refinement passes.
            FrameScript {
                tiles: tiles_of(&b),
            },
        ]),
        cadence_us: DEFAULT_CADENCE_US,
        net: NetProfile::perfect(),
        drops: Default::default(),
        // Ample time: if convergence were merely slow this would catch it.
        duration: Duration::from_secs(20),
        grid_cols: 4,
        grid_rows: 4,
    };
    let r = run_browserless(scene).await.expect("scene ran");

    let expected = expected_rgba(&b);
    let mut wrong = Vec::new();
    for x in 0..4u8 {
        for y in 0..4u8 {
            if r.framebuffer.tile_rgba(x, y) != Some(expected.as_slice()) {
                wrong.push((x, y));
            }
        }
    }
    println!(
        "supersede: wrong_tiles={} nack_hit={} nack_miss={} retransmits={}",
        wrong.len(),
        r.nack_hit,
        r.nack_miss,
        r.retransmit_attempts_total
    );
    assert!(
        wrong.is_empty(),
        "{} of 16 tiles never converged to the second frame's content: {:?}. \
         The first frame's refinement was superseded mid-flight, and the \
         client waits for all 14 passes because TileHeader carries `pass` but \
         not `total_passes` -- it cannot learn that fewer are coming.",
        wrong.len(),
        wrong
    );
}
