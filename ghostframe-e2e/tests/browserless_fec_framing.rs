//! Both ends on the rev 6 framing (`RunOptions::fec_framing`): a clean scene
//! still delivers, over bundles.

use std::time::Duration;

use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessScene, FrameScript, LinkOverrides, RunOptions,
    SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::load_profile::{Churn, LoadProfile, PRODUCTION_CADENCE_US};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::NetProfile;

fn framed() -> RunOptions {
    RunOptions {
        fec_framing: true,
        ..RunOptions::default()
    }
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
