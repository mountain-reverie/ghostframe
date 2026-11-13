//! Adaptive FEC and the encoder's rate target (rev 6, R6-e E6): the repairs
//! are paid out of the media rate (`RATE_SHARE / (1 + rho)`), so the stream's
//! total stays under the BWE ceiling's saturation line. A capacity-limited
//! link at 5 % i.i.d. loss, H.264 with the in-place rate policy: the ceiling
//! must stay a rare event with adaptive FEC (a small share of its readings
//! bind, a small share find the sender saturated) and the estimate must not be
//! lower than with the mode off at equal loss. Break it: drop the scale in
//! `RatePolicy::step_in_place` and the repair bytes ride on top of 0.8 x the
//! estimate, the sender reads as saturated and both shares jump.
//!
//! Recalibrated in the commit that made repair bytes count as offered
//! (`payload_fraction` had read 0.70 with FEC engaged, which scaled every
//! saturation reading by 0.7 and hid the overshoot below). The scale cannot
//! hold the stream's total at exactly `RATE_SHARE x` the estimate (the
//! overhead it follows is an EWMA, and a repair row is a burst), so with the
//! accounting right the ceiling binds a little more often than with the mode
//! off; the bounds below sit between that and what removing the scale gives.

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, RunOptions,
    SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::h264_scene::{pcm_load, Pattern};
use ghostframe_e2e::netsim::{Bottleneck, CapTimeline, DropPlan, NetProfile};
use ghostframe_lib::encoder::h264_harness::{PcmMode, PcmRateModel};
use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass;

const SEEDS: [u64; 3] = [0xFB_1001, 0xFB_1002, 0xFB_1003];

fn h264_load() -> H264Load {
    let mut l = pcm_load(PcmMode::RefManager);
    l.pcm_rate_policy = true;
    // The frame size follows the rate the policy applied, so the stream
    // really spends 0.8 x the estimate (or less, with repairs paid for).
    l.pcm_rate_model = Some(PcmRateModel::s2(40_000, 120_000, 30));
    l
}

async fn run(seed: u64, adaptive: bool) -> BrowserlessResult {
    let base = NetProfile {
        delay_us: 25_000,
        cap: CapTimeline::constant(2_000_000),
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
async fn the_ceiling_stays_rare_and_the_estimate_whole_with_adaptive_fec() {
    let (mut on_binding, mut on_sat, mut on_reads) = (0u64, 0u64, 0u64);
    for seed in SEEDS {
        let off = run(seed, false).await;
        let on = run(seed, true).await;
        let (o, a) = (&off.transport.instr.bwe, &on.transport.instr.bwe);
        let repairs = on.transport.instr.offered.of(WireClass::Repair);
        println!(
            "FECR seed={seed:#x} dropped off/on={}/{} repairs={repairs} ceiling reads off/on={}/{} saturated off/on={}/{} binding off/on={}/{} reseeds_down off/on={}/{}",
            off.s2c_datagrams_dropped,
            on.s2c_datagrams_dropped,
            o.ceiling_reads,
            a.ceiling_reads,
            o.ceiling_saturated_reads,
            a.ceiling_saturated_reads,
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
        on_sat += a.ceiling_saturated_reads;
        on_reads += a.ceiling_reads;
        // The estimate the ceiling exists to protect: FEC must not cost it.
        let mean = |r: &BrowserlessResult| {
            let v: Vec<u64> = r
                .bwe_estimate_samples
                .iter()
                .filter(|x| x.0 > 5_000_000)
                .map(|x| x.1)
                .collect();
            assert!(!v.is_empty(), "seed {seed:#x}: no estimate samples");
            v.iter().sum::<u64>() as f64 / v.len() as f64
        };
        let (m_off, m_on) = (mean(&off), mean(&on));
        println!("FECR seed={seed:#x} estimate mean after 5 s off/on={m_off:.0}/{m_on:.0}");
        assert!(
            m_on >= 0.9 * m_off,
            "seed {seed:#x}: the estimate averaged {m_on:.0} with adaptive FEC against {m_off:.0} without"
        );
    }
    // Measured at 16 Mbit/s (the cap is bytes per second) over these seeds, per
    // run and not a constant of the code: with the scale, 965 of 71k readings
    // bind (0.5 to 2.3 % per seed) and 4160 find the sender saturated (4.9 to
    // 6.5 %); with the mode off 76 and 340. With the scale removed 9720 bind
    // (7.3 to 13 % per seed) and 56k are saturated (52 to 64 %).
    let (binding, sat) = (
        on_binding as f64 / on_reads as f64,
        on_sat as f64 / on_reads as f64,
    );
    println!("FECR binding share {binding:.4}, saturated share {sat:.4}");
    assert!(
        binding <= 0.04,
        "the ceiling bound on {binding:.3} of its readings with adaptive FEC"
    );
    assert!(
        sat <= 0.15,
        "the sender read as saturated on {sat:.3} of the readings with adaptive FEC"
    );
}
