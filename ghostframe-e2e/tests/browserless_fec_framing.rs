//! Both ends on the rev 6 framing: a clean scene still delivers, over bundles.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessScene, FrameScript, LinkOverrides, RunOptions,
    SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{DropPlan, DropRule, NetProfile};

fn framed() -> RunOptions {
    RunOptions::default()
}

#[tokio::test(start_paused = true)]
async fn a_single_solid_tile_arrives_through_bundles_on_a_perfect_link() {
    let scene = BrowserlessScene {
        seed: 1,
        tick_budget_floor_bytes: None,
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
    let result = run_browserless_with_options(scene, LinkOverrides::default(), framed())
        .await
        .expect("scene ran");
    let px = result
        .framebuffer
        .tile_rgba(0, 0)
        .expect("tile (0,0) decoded");
    assert_eq!(&px[0..4], &[30, 20, 10, 255], "BGRA -> RGBA swizzle");
}

#[tokio::test(start_paused = true)]
async fn a_churning_scene_delivers_and_bills_exactly_under_framing() {
    let scene = BrowserlessScene {
        seed: 0xB0_B057,
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
        duration: Duration::from_secs(4),
        grid_cols: 16,
        grid_rows: 16,
    };
    let r = run_browserless_with_options(scene, LinkOverrides::default(), framed())
        .await
        .expect("scene ran");
    assert!(r.events.contains(&ClientNetEvent::SessionReady));
    assert!(r.s2c_datagrams_offered > 0);
    let i = &r.transport.instr;
    println!(
        "FRAMING offered_total={} quinn={} admission={:?} send_errs={} ledger_unknown_acks={} \
         tiles_decoded={}",
        i.offered.total(),
        i.quinn_bytes_enqueued,
        i.admission,
        r.send_datagram_errs,
        i.ledger_unknown_acks,
        r.framebuffer.tile_rgba(0, 0).is_some()
    );
    assert_eq!(i.offered.total(), i.quinn_bytes_enqueued);
    assert_eq!(
        i.admission.charged_bytes + i.admission.soft_charged_bytes,
        i.offered.total()
    );
    assert_eq!(r.send_datagram_errs, 0);
    assert!(
        r.framebuffer.tile_rgba(0, 0).is_some() || r.framebuffer.tile_rgba(8, 8).is_some(),
        "tiles reached the client framebuffer"
    );
}

/// The framing-on twin of the runner's drop-recovery scene: a record the drop
/// plan eats before packing must still be repaired by the loss path.
#[tokio::test(start_paused = true)]
async fn under_framing_a_solid_tile_whose_only_datagram_is_dropped_is_still_repaired() {
    let scene = BrowserlessScene {
        seed: 0x0D30_0001,
        tick_budget_floor_bytes: None,
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
    let result = run_browserless_with_options(scene, LinkOverrides::default(), framed())
        .await
        .expect("scene ran");

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

/// The knob on: the same churning scene, with blocks. Every datagram is still
/// billed exactly, and repairs left (billed as `WireClass::Repair`).
#[tokio::test(start_paused = true)]
async fn a_churning_scene_with_blocks_on_sends_and_bills_repairs() {
    use ghostframe_lib::config::FecBlockParams;
    use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass;
    let scene = BrowserlessScene {
        seed: 0xB0_B057,
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
        duration: Duration::from_secs(4),
        grid_cols: 16,
        grid_rows: 16,
    };
    let opts = RunOptions {
        fec_blocks: Some(FecBlockParams {
            k_max: 64,
            t_ms: 5,
            r: 2,
        }),
        ..framed()
    };
    let r = run_browserless_with_options(scene, LinkOverrides::default(), opts)
        .await
        .expect("scene ran");
    let i = &r.transport.instr;
    let repair_b = i.offered.of(WireClass::Repair);
    println!(
        "BLOCKS offered_total={} quinn={} repair_B={} bundles={} send_errs={}",
        i.offered.total(),
        i.quinn_bytes_enqueued,
        repair_b,
        i.offered.bundles,
        r.send_datagram_errs
    );
    assert!(repair_b > 0, "repairs left the server");
    assert_eq!(i.offered.total(), i.quinn_bytes_enqueued);
    assert_eq!(r.send_datagram_errs, 0);
    assert!(
        r.framebuffer.tile_rgba(0, 0).is_some() || r.framebuffer.tile_rgba(8, 8).is_some(),
        "tiles reached the client framebuffer"
    );
}
