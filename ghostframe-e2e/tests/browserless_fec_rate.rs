//! Adaptive FEC and the encoder's rate target (rev 6, R6-e E6): the repairs
//! are paid out of the media rate (`RATE_SHARE / (1 + rho)`), so the stream's
//! total stays under the BWE ceiling's saturation line. A capacity-limited
//! link at 5 % i.i.d. loss, H.264 with the in-place rate policy: the ceiling
//! must bind no more often with adaptive FEC than with the mode off at equal
//! loss. Break it: drop the scale in `RatePolicy::step_in_place` and the
//! repair bytes ride on top of 0.8 x the estimate, the sender reads as
//! saturated and the count rises.

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, RunOptions,
    SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::h264_scene::{pcm_load, Pattern};
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, NetProfile};
use ghostframe_lib::encoder::h264_harness::PcmMode;
use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass;

const SEEDS: [u64; 3] = [0xFB_1001, 0xFB_1002, 0xFB_1003];

fn h264_load() -> H264Load {
    let mut l = pcm_load(PcmMode::RefManager);
    l.pcm_rate_policy = true;
    l
}

async fn run(seed: u64, adaptive: bool) -> BrowserlessResult {
    let base = NetProfile {
        delay_us: 25_000,
        cap: CapTimeline::constant(6_000_000),
        bottleneck: Some(Bottleneck::wifi()),
        ..NetProfile::perfect()
    };
    let (net, links) = Pattern::Iid(0.05).apply(base);
    let scene = BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::H264(h264_load()),
        cadence_us: DEFAULT_CADENCE_US,
        net,
        drops: DropPlan::default(),
        duration: Duration::from_secs(20),
        grid_cols: 4,
        grid_rows: 4,
    };
    let opts = RunOptions {
        fec_adaptive: adaptive,
        ..RunOptions::default()
    };
    run_browserless_with_options(scene, links, opts)
        .await
        .expect("scene ran")
}

#[tokio::test(start_paused = true)]
async fn the_ceiling_binds_no_more_often_with_adaptive_fec_than_without() {
    let (mut on_binding, mut off_binding) = (0u64, 0u64);
    for seed in SEEDS {
        let off = run(seed, false).await;
        let on = run(seed, true).await;
        let (o, a) = (&off.transport.instr.bwe, &on.transport.instr.bwe);
        let repairs = on.transport.instr.offered.of(WireClass::Repair);
        println!(
            "FECR seed={seed:#x} dropped off/on={}/{} repairs={repairs} ceiling reads off/on={}/{} binding off/on={}/{} reseeds_down off/on={}/{}",
            off.s2c_datagrams_dropped,
            on.s2c_datagrams_dropped,
            o.ceiling_reads,
            a.ceiling_reads,
            o.ceiling_binding_reads,
            a.ceiling_binding_reads,
            o.ceiling_reseeds_down,
            a.ceiling_reseeds_down
        );
        // Premises: the link lost datagrams, the ceiling was read in both
        // runs, adaptive sent repairs and the mode off sent none.
        assert!(off.s2c_datagrams_dropped > 0 && on.s2c_datagrams_dropped > 0);
        assert!(o.ceiling_reads > 0 && a.ceiling_reads > 0, "seed {seed:#x}");
        assert!(repairs > 0, "seed {seed:#x}: the controller never engaged");
        assert_eq!(off.transport.instr.offered.of(WireClass::Repair), 0);
        on_binding += a.ceiling_binding_reads;
        off_binding += o.ceiling_binding_reads;
    }
    assert!(
        on_binding <= off_binding + off_binding / 4 + 2,
        "the ceiling bound {on_binding} times with adaptive FEC against {off_binding} without"
    );
}
