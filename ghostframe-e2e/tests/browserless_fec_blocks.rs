//! FEC blocks (rev 6, R6-d D8): the scenes of the block mode, over three
//! seeds each. Browserless is chaotic, so every assertion is a property
//! (premise non-zero, bounded, convergent, never reported lost) and the
//! bounds come from the measured spread (printed as `FECB ...` lines),
//! with margin. The server/client window constants are checked by a lib
//! unit test (`the_server_and_the_client_agree_on_the_recovery_window`).

use std::time::Duration;

use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, FrameScript, H264Load,
    LinkOverrides, RunOptions, SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::harness::h264_scene::{pcm_load, H264Outcome, Pattern};
use ghostframe_e2e::harness::scene_tiles::TileSpec;
use ghostframe_e2e::netsim::{DropPlan, NetProfile};
use ghostframe_lib::config::FecBlockParams;
use ghostframe_lib::encoder::h264_harness::PcmMode;
use ghostframe_lib::transport::reliable_emitter::emission_queue::WireClass;

const SEEDS: [u64; 3] = [0xFB_0001, 0xFB_0002, 0xFB_0003];
const EXTRA_SEEDS: [u64; 5] = [0xFB_0004, 0xFB_0005, 0xFB_0006, 0xFB_0007, 0xFB_0008];
const GRID: u32 = 16;

fn knob(k_max: u8, t_ms: u8, r: u8) -> RunOptions {
    RunOptions {
        fec_blocks: Some(FecBlockParams {
            k_max,
            t_ms,
            r,
            m: 1,
        }),
        ..RunOptions::default()
    }
}

fn link(delay_us: u64, loss: f64) -> NetProfile {
    NetProfile {
        delay_us,
        loss,
        ..NetProfile::perfect()
    }
}

// ---- tile scenes --------------------------------------------------------

/// A distinct, incompressible-enough 32x32 tile for `(x, y)`.
fn tile_pixels(x: u8, y: u8) -> Vec<u8> {
    let mut v = Vec::with_capacity(4096);
    let mut s = (u32::from(x) << 8 | u32::from(y)).wrapping_mul(0x9E37_79B1) | 1;
    for _ in 0..1024 {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        v.extend_from_slice(&[s as u8, (s >> 8) as u8, (s >> 16) as u8, 255]);
    }
    v
}

/// Four frames of 16 distinct tiles each: 64 tiles, then quiet.
fn tile_script() -> (SceneLoad, Vec<(u8, u8)>) {
    let mut all = Vec::new();
    let mut frames = Vec::new();
    for f in 0..4u8 {
        let mut tiles = Vec::new();
        for i in 0..16u8 {
            let pos = (i, f * 2);
            all.push(pos);
            tiles.push((
                pos,
                TileSpec::Cdf53 {
                    bgra: tile_pixels(pos.0, pos.1),
                },
            ));
        }
        frames.push(FrameScript { tiles });
    }
    (SceneLoad::Script(frames), all)
}

fn tile_scene(seed: u64, secs: u64) -> (BrowserlessScene, Vec<(u8, u8)>) {
    let (load, tiles) = tile_script();
    (
        BrowserlessScene {
            seed,
            tick_budget_floor_bytes: None,
            datagram_send_buffer_bytes: None,
            load,
            cadence_us: DEFAULT_CADENCE_US,
            net: link(10_000, 0.0),
            drops: DropPlan::default(),
            duration: Duration::from_secs(secs),
            grid_cols: GRID,
            grid_rows: GRID,
        },
        tiles,
    )
}

async fn run_tiles(
    seed: u64,
    secs: u64,
    s2c: NetProfile,
    c2s: Option<NetProfile>,
    opts: RunOptions,
) -> (BrowserlessResult, usize, usize) {
    let (scene, tiles) = tile_scene(seed, secs);
    let r = run_browserless_with_options(
        scene,
        LinkOverrides {
            s2c: Some(s2c),
            c2s,
            ..Default::default()
        },
        opts,
    )
    .await
    .expect("scene ran");
    let present = tiles
        .iter()
        .filter(|&&(x, y)| r.framebuffer.tile_rgba(x, y).is_some())
        .count();
    (r, present, tiles.len())
}

fn present(r: &BrowserlessResult) -> usize {
    let (_, tiles) = tile_script();
    tiles
        .iter()
        .filter(|&&(x, y)| r.framebuffer.tile_rgba(x, y).is_some())
        .count()
}

fn per_drop(r: &BrowserlessResult) -> f64 {
    r.loss_path_resends as f64 / r.s2c_datagrams_dropped.max(1) as f64
}

fn tline(name: &str, seed: u64, r: &BrowserlessResult, present: usize, total: usize) {
    let i = &r.transport.instr;
    println!(
        "FECB {name} {seed:#x}: tiles={present}/{total} dropped={} resends={} per_drop={:.3} \
         repair_B={} rebuilt={} declined_rx={} handoffs={} queued={} served={} suppressed={} \
         nack_on_prot={} alarm={}",
        r.s2c_datagrams_dropped,
        r.loss_path_resends,
        per_drop(r),
        i.offered.of(WireClass::Repair),
        i.fec_recovered_rx,
        i.fec_declined_rx,
        i.block_handoffs,
        i.block_handoff_queued,
        i.block_requests_served,
        i.block_protected_timer_suppressed,
        i.block_nack_on_protected,
        i.block_deferral_timeout,
    );
}

/// Scene 1: i.i.d. 3 % packet loss.
#[tokio::test(start_paused = true)]
async fn scene_1_tiles_iid_loss_decode_and_the_loss_path_works_less() {
    for seed in SEEDS {
        let (on, p_on, n) = run_tiles(seed, 10, link(10_000, 0.03), None, knob(64, 5, 2)).await;
        let (off, p_off, _) =
            run_tiles(seed, 10, link(10_000, 0.03), None, RunOptions::default()).await;
        tline("s1-on", seed, &on, p_on, n);
        tline("s1-off", seed, &off, p_off, n);
        let i = &on.transport.instr;
        // Premises: the link dropped, repairs left, the client rebuilt.
        assert!(on.s2c_datagrams_dropped > 0 && off.s2c_datagrams_dropped > 0);
        assert!(i.offered.of(WireClass::Repair) > 0, "no repair was sent");
        assert!(i.fec_recovered_rx > 0, "the client rebuilt nothing");
        assert!(off.loss_path_resends > 0, "the control resent nothing");
        assert_eq!(p_on, n, "seed {seed:#x}: a tile never decoded");
        assert_eq!(p_off, n, "control converged too");
        assert_eq!(i.block_deferral_timeout, 0);
        assert!(
            per_drop(&on) < 0.5 * per_drop(&off),
            "seed {seed:#x}: resends per dropped datagram {:.3} vs {:.3} off",
            per_drop(&on),
            per_drop(&off)
        );
    }
}

/// Scene 3: bursts (mean length 3, Gilbert-Elliott), hand-offs allowed.
#[tokio::test(start_paused = true)]
async fn scene_3_burst_loss_converges() {
    for seed in SEEDS {
        let mut s2c = link(10_000, 0.0);
        s2c.burst_enter = 0.02;
        s2c.burst_exit = 1.0 / 3.0;
        s2c.burst_loss = 1.0;
        let (r, p, n) = run_tiles(seed, 12, s2c, None, knob(16, 5, 2)).await;
        tline("s3", seed, &r, p, n);
        assert!(r.s2c_datagrams_dropped > 0, "premise: bursts dropped");
        assert!(r.transport.instr.offered.of(WireClass::Repair) > 0);
        assert_eq!(p, n, "seed {seed:#x}: a tile was stranded");
        assert_eq!(r.transport.instr.block_deferral_timeout, 0);
        // Hand-offs happen; whatever they queued reached ARQ, never more.
        let i = &r.transport.instr;
        assert!(i.block_handoff_queued <= i.block_handoffs);
    }
}

/// Scene 4: a lossy link on both directions with r = 1, so blocks lose
/// their repairs and sources together and the feedback is late: only the
/// block's deadline (or DECLINED) can release the content to ARQ.
#[tokio::test(start_paused = true)]
async fn scene_4_blocks_that_lose_repairs_and_sources_hand_off_and_deliver() {
    for seed in SEEDS {
        let (r, p, n) = run_tiles(
            seed,
            14,
            link(10_000, 0.3),
            Some(link(10_000, 0.3)),
            knob(8, 5, 1),
        )
        .await;
        tline("s4", seed, &r, p, n);
        let i = &r.transport.instr;
        assert!(i.offered.of(WireClass::Repair) > 0);
        assert!(
            i.block_handoffs > 0,
            "premise: blocks failed and handed off"
        );
        assert!(i.block_handoff_queued > 0, "hand-offs reached ARQ");
        assert_eq!(p, n, "seed {seed:#x}: hand-off did not deliver every tile");
    }
}

/// Scene 6: the status stream (RECOVERED, DECLINED, requests) starved: the
/// client-to-server path 300 ms slower than the forward one. No duplicate
/// storm, and every tile arrives.
#[tokio::test(start_paused = true)]
async fn scene_6_a_delayed_status_stream_does_not_storm() {
    for seed in SEEDS {
        let (on, p, n) = run_tiles(
            seed,
            14,
            link(10_000, 0.03),
            Some(link(310_000, 0.0)),
            knob(64, 5, 2),
        )
        .await;
        let (off, _, _) = run_tiles(
            seed,
            14,
            link(10_000, 0.03),
            Some(link(310_000, 0.0)),
            RunOptions::default(),
        )
        .await;
        tline("s6-on", seed, &on, p, n);
        tline("s6-off", seed, &off, present(&off), n);
        assert!(on.s2c_datagrams_dropped > 0);
        assert!(on.transport.instr.offered.of(WireClass::Repair) > 0);
        assert!(on.transport.instr.fec_recovered_rx > 0 || on.transport.instr.block_handoffs > 0);
        assert_eq!(p, n, "seed {seed:#x}: a tile never arrived");
        // Bounded: not a storm. Duplicates may rise over the control (the
        // client's NACKs race the late RECOVERED) but stay of its order.
        assert!(
            on.loss_path_resends <= 2 * off.loss_path_resends + 20,
            "seed {seed:#x}: {} resends vs {} off",
            on.loss_path_resends,
            off.loss_path_resends
        );
        assert!(
            on.retransmit_attempts_total <= 3 * off.retransmit_attempts_total + 100,
            "seed {seed:#x}: {} retransmits vs {} off",
            on.retransmit_attempts_total,
            off.retransmit_attempts_total
        );
    }
}

/// Scene 7: unknown k. Heavy loss on the forward path with r = 1 and a
/// block as wide as a tile's passes, so blocks lose their LAST source, their
/// repair and trailing sources together: the client has no k, its DECLINED
/// bitmap stops at the highest source it saw, and only the server's own
/// accounted set can name the trailing sources. Every tile must converge to
/// its exact pixels (a stranded trailing pass leaves it unrefined).
#[tokio::test(start_paused = true)]
async fn scene_7_declined_blocks_with_an_unknown_k_hand_off_every_unaccounted_source() {
    for seed in SEEDS {
        let (r, p, n) = run_tiles(seed, 40, link(10_000, 0.5), None, knob(14, 5, 1)).await;
        tline("s7", seed, &r, p, n);
        let (_, tiles) = tile_script();
        let exact = tiles
            .iter()
            .filter(|&&(x, y)| {
                let want: Vec<u8> = tile_pixels(x, y)
                    .chunks(4)
                    .flat_map(|c| [c[2], c[1], c[0], c[3]])
                    .collect();
                r.framebuffer.tile_rgba(x, y) == Some(want.as_slice())
            })
            .count();
        println!("FECB s7 {seed:#x}: exact={exact}/{n}");
        let i = &r.transport.instr;
        assert!(i.fec_declined_rx > 0, "premise: the client declined blocks");
        assert!(i.block_handoff_queued > 0);
        assert_eq!(
            exact, n,
            "seed {seed:#x}: a tile never reached its exact pixels"
        );
    }
}

// ---- H.264 scenes -------------------------------------------------------

fn h264_load() -> H264Load {
    let mut l = pcm_load(PcmMode::RefManager);
    l.pcm_rate_policy = true;
    l
}

async fn run_h264(
    seed: u64,
    secs: u64,
    base: NetProfile,
    pattern: Pattern,
    opts: RunOptions,
) -> (BrowserlessResult, H264Outcome) {
    let (net, links) = pattern.apply(base);
    let scene = BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::H264(h264_load()),
        cadence_us: DEFAULT_CADENCE_US,
        net,
        drops: DropPlan::default(),
        duration: Duration::from_secs(secs),
        grid_cols: 4,
        grid_rows: 4,
    };
    let r = run_browserless_with_options(scene, links, opts)
        .await
        .expect("scene");
    let o = r.h264_run().map(|x| x.analyse()).unwrap_or_default();
    (r, o)
}

fn hline(name: &str, seed: u64, r: &BrowserlessResult, o: &H264Outcome) {
    let s = &r.h264.stats;
    println!(
        "FECB {name} {seed:#x}: dropped={} idr={} suspect={} recov={} rebuilt={} parked={} \
         lost_decline={} lost_deadline={} alarm={} never_fed={} wrong={} max_gap_ms={}",
        r.s2c_datagrams_dropped,
        o.idr_made,
        s.frames_suspect,
        o.refs.recoveries,
        r.transport.instr.fec_recovered_rx,
        s.verdicts_parked_by_fec,
        s.content_lost_decline,
        s.content_lost_deadline,
        r.transport.instr.block_deferral_timeout,
        o.never_fed,
        o.wrong.len(),
        o.max_gap_us / 1000,
    );
}

fn ledger_identity(r: &BrowserlessResult) -> bool {
    let s = &r.h264.stats;
    s.tx_recorded >= s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten
}

/// Scene 2: H.264 under i.i.d. 3 %: the manager hears less loss.
#[tokio::test(start_paused = true)]
async fn scene_2_h264_iid_loss_the_manager_hears_less() {
    let mut parked_total = 0;
    for seed in SEEDS {
        let base = link(50_000, 0.0);
        let (on, o_on) = run_h264(seed, 10, base.clone(), Pattern::Iid(0.05), knob(64, 5, 2)).await;
        let (off, o_off) =
            run_h264(seed, 10, base, Pattern::Iid(0.05), RunOptions::default()).await;
        hline("s2-on", seed, &on, &o_on);
        hline("s2-off", seed, &off, &o_off);
        let i = &on.transport.instr;
        assert!(on.s2c_datagrams_dropped > 0);
        assert!(i.offered.of(WireClass::Repair) > 0 && i.fec_recovered_rx > 0);
        assert_eq!(o_on.wrong.len(), 0, "a wrong picture was shown");
        assert!(
            on.h264.stats.frames_suspect < off.h264.stats.frames_suspect,
            "seed {seed:#x}: suspect {} vs {} off",
            on.h264.stats.frames_suspect,
            off.h264.stats.frames_suspect
        );
        assert!(
            o_on.refs.recoveries < o_off.refs.recoveries,
            "seed {seed:#x}: recoveries {} vs {} off",
            o_on.refs.recoveries,
            o_off.refs.recoveries
        );
        assert!(o_on.idr_made <= o_off.idr_made, "IDRs rose");
        assert!(ledger_identity(&on) && ledger_identity(&off));
        parked_total += on.h264.stats.verdicts_parked_by_fec;
    }
    // The mechanism ran: ledger verdicts on protected fragments were parked
    // (an `on_verdict` that always passes leaves this 0).
    assert!(parked_total > 0, "no verdict was ever parked");
}

/// Scene 5: a high-RTT path (125 ms each way). A frame the repairs rebuild
/// must never reach the manager as lost: the deadline and the deferral
/// alarm are built on the live round trip, and stay quiet.
#[tokio::test(start_paused = true)]
async fn scene_5_high_rtt_h264_a_rebuilt_frame_is_never_reported_lost() {
    let mut parked_total = 0;
    for seed in SEEDS.into_iter().chain(EXTRA_SEEDS) {
        let base = link(125_000, 0.0);
        let (on, o_on) = run_h264(seed, 14, base.clone(), Pattern::Iid(0.03), knob(64, 5, 2)).await;
        let (off, o_off) =
            run_h264(seed, 14, base, Pattern::Iid(0.03), RunOptions::default()).await;
        hline("s5-on", seed, &on, &o_on);
        hline("s5-off", seed, &off, &o_off);
        let i = &on.transport.instr;
        let s = &on.h264.stats;
        assert!(on.s2c_datagrams_dropped > 0);
        assert!(i.offered.of(WireClass::Repair) > 0);
        assert!(i.fec_recovered_rx > 0, "premise: the client rebuilt frames");
        parked_total += s.verdicts_parked_by_fec;
        // Measured over 8 seeds: 0..=3 deadline losses against 34..=35
        // rebuilt frames (a block that no repair could save is lost for
        // good, legitimately). With the RTT term removed from the H.264
        // deadline the same scene reports 1167: the bound has two orders
        // of margin on that side and a factor of 2.5 on this one.
        assert!(
            s.content_lost_deadline * 4 <= i.fec_recovered_rx,
            "seed {seed:#x}: {} deadline losses against {} rebuilt",
            s.content_lost_deadline,
            i.fec_recovered_rx
        );
        assert_eq!(i.block_deferral_timeout, 0, "the deferral alarm fired");
        assert_eq!(o_on.wrong.len(), 0);
        assert!(
            s.frames_suspect <= off.h264.stats.frames_suspect,
            "seed {seed:#x}: suspect {} vs {} off",
            s.frames_suspect,
            off.h264.stats.frames_suspect
        );
        assert!(ledger_identity(&on));
    }
    assert!(parked_total > 0, "premise: no verdict was ever parked");
}
