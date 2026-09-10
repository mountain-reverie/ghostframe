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
use ghostframe_e2e::harness::browserless::{run_browserless, BrowserlessScene, FrameScript};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{CapTimeline, NetProfile};

#[tokio::test(start_paused = true)]
async fn the_session_establishes_over_the_socketpair() {
    let scene = BrowserlessScene {
        seed: 1,
        frames: vec![],
        net: NetProfile::perfect(),
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
        frames: vec![],
        net: NetProfile::perfect(),
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
        frames: vec![],
        net: NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        },
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
        frames: vec![FrameScript {
            tiles: vec![(
                (0, 0),
                TileSpec::Solid {
                    bgra: [10, 20, 30, 255],
                },
            )],
        }],
        net: NetProfile::perfect(),
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
        frames: vec![
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
        ],
        net: NetProfile::perfect(),
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
        frames: vec![FrameScript {
            tiles: vec![(
                (0, 0),
                TileSpec::Cdf53 {
                    bgra: gradient_tile(),
                },
            )],
        }],
        net: NetProfile {
            loss: 0.10,
            ..NetProfile::perfect()
        },
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
        // Same tile rewritten with a different colour on each of 20 frames.
        frames: (0..20u8)
            .map(|i| FrameScript {
                tiles: vec![(
                    (0, 0),
                    TileSpec::Solid {
                        bgra: [i * 10, 20, 30, 255],
                    },
                )],
            })
            .collect(),
        net: NetProfile {
            loss: 0.05,
            reorder_us: 30_000,
            ..NetProfile::perfect()
        },
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
            frames: busy_frames(8),
            net: NetProfile {
                cap: CapTimeline::constant(cap_bps),
                ..NetProfile::perfect()
            },
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
        frames: vec![FrameScript {
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
        }],
        net: NetProfile {
            loss: 0.05,
            cap: CapTimeline::constant(400_000),
            ..NetProfile::perfect()
        },
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
