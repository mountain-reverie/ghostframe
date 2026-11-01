//! H.264 as a member of the transport (design PR A3), in the stepped
//! browserless harness.
//!
//! What runs: the real `IoBridge` event loop, a real `ClientNet` that
//! advertises H.264 and acknowledges the fragments it receives, a seeded netsim
//! between them. What is scripted: the encoder. `SceneLoad::H264` injects one
//! frame per tick; the bridge puts it through its real H.264 gate (IDR hold,
//! backpressure skip, pending IDR requests) and its real send path (fragments,
//! ledger, admission, probe cache, XOR parity), and the "encoder" makes an IDR
//! exactly when the bridge asked for one. Payloads are filler: the client
//! reassembles (`NeedsH264`) and decodes nothing.
//!
//! What this therefore covers: that H.264 datagrams are ledger transmissions
//! (acknowledged or judged exactly once), that the bandwidth estimator learns
//! in H.264 mode, that an IDR's lost fragments are repaired and P frames wait
//! for it, that an acknowledgement stall is not read as loss, and that the
//! encoder is not run into a backed-up queue. What it cannot: anything about
//! picture content or reference chains (the Phase B PCM writer), the ffmpeg
//! encoder's own GOP and rate control, or the web client's decoder.
//!
//! Every scene states its premise as an assertion before the effect it is
//! about: an effect asserted over a drop that never fired proves nothing.

use std::time::Duration;

use ghostframe_client_core::Event as CoreEvent;
use ghostframe_client_net::ClientNetEvent;
use ghostframe_e2e::harness::browserless::{
    run_browserless_with_options, BrowserlessResult, BrowserlessScene, H264Load, LinkOverrides,
    RunOptions, SceneLoad, DEFAULT_CADENCE_US,
};
use ghostframe_e2e::netsim::{
    Bottleneck, CapTimeline, DropPlan, FragSel, FrameDropRule, NetProfile,
};
use ghostframe_lib::transport::io_bridge::IdrCause;

const CADENCE_US: u64 = DEFAULT_CADENCE_US;

fn load(idr_bytes: usize, p_bytes: usize) -> H264Load {
    H264Load {
        cadence_us: CADENCE_US,
        idr_bytes,
        p_bytes,
        gop_frames: 0,
        width: 1280,
        height: 720,
        quiet: None,
    }
}

fn scene(
    seed: u64,
    secs: u64,
    net: NetProfile,
    load: H264Load,
    drops: DropPlan,
) -> BrowserlessScene {
    BrowserlessScene {
        seed,
        datagram_send_buffer_bytes: None,
        tick_budget_floor_bytes: None,
        load: SceneLoad::H264(load),
        cadence_us: CADENCE_US,
        net,
        drops,
        duration: Duration::from_secs(secs),
        grid_cols: 4,
        grid_rows: 4,
    }
}

async fn run(s: BrowserlessScene, opts: RunOptions) -> anyhow::Result<BrowserlessResult> {
    let links = LinkOverrides::from_burst_env(&s.net);
    run_browserless_with_options(s, links, opts).await
}

fn link_20ms() -> NetProfile {
    NetProfile {
        delay_us: 20_000,
        ..NetProfile::perfect()
    }
}

/// `(frame_seq, is_keyframe)` of every frame the client reassembled, in order.
fn delivered(r: &BrowserlessResult) -> Vec<(u32, bool)> {
    r.events
        .iter()
        .filter_map(|e| match e {
            ClientNetEvent::Core(CoreEvent::NeedsH264 {
                frame_seq,
                is_keyframe,
                ..
            }) => Some((*frame_seq, *is_keyframe)),
            _ => None,
        })
        .collect()
}

fn report(name: &str, r: &BrowserlessResult) {
    let s = &r.h264.stats;
    println!(
        "{name}: sent={} idr={} (session={} loss={} hold_timeout={} gop={}) acked={} suspect={} \
         tx rec={} live={} lost={} forgot={} late={} outstanding={} \
         verdicts idr={} p={} spurious={} skipped hold={} bp={} p_hold_ms={} purged={} \
         bwe_samples={} bwe_bps={} pto_fires={} loss_by_pto={} idr_resends={} probe_resends={} \
         delivered={} s2c_drop={}/{}",
        s.frames_sent,
        s.frames_idr,
        s.idr_by_cause[IdrCause::Session.index()],
        s.idr_by_cause[IdrCause::Loss.index()],
        s.idr_by_cause[IdrCause::HoldTimeout.index()],
        s.idr_by_cause[IdrCause::GopPeriodic.index()],
        s.frames_acked,
        s.frames_suspect,
        s.tx_recorded,
        s.tx_acked_live,
        s.tx_judged_lost,
        s.tx_forgotten,
        s.tx_acked_late,
        r.h264.ledger_records,
        s.verdicts_idr_frag,
        s.verdicts_p_frag,
        s.verdicts_already_acked,
        s.frames_skipped_hold,
        s.frames_skipped_backpressure,
        s.p_hold_ms,
        s.queue_purged,
        r.bwe_samples_seen,
        r.bwe_estimate_bps,
        r.emitter_stats.pto_fires,
        r.transport.instr.loss_by_pto,
        r.emitter_stats.h264_idr_resends,
        r.emitter_stats.h264_probe_resends,
        delivered(r).len(),
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_offered,
    );
}

/// A healthy stream: every H.264 datagram is acknowledged, none is judged
/// lost, the books balance, and the estimator has samples (it had none before
/// H.264 joined the ledger).
#[tokio::test(start_paused = true)]
async fn a_healthy_h264_stream_is_acknowledged_and_feeds_the_estimator() {
    let r = run(
        scene(
            0x1101,
            6,
            link_20ms(),
            load(40_000, 3_000),
            DropPlan::default(),
        ),
        RunOptions::default(),
    )
    .await
    .expect("scene");
    report("healthy", &r);
    let s = &r.h264.stats;
    // Premises.
    assert!(s.frames_sent > 100, "the stream ran: {}", s.frames_sent);
    assert!(s.tx_recorded > 300, "and had fragments: {}", s.tx_recorded);
    // Every recorded transmission has exactly one outcome.
    assert_eq!(
        s.tx_recorded,
        s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten + r.h264.ledger_records as u64,
        "{s:?}"
    );
    assert_eq!(s.tx_judged_lost, 0, "nothing is lost on a perfect link");
    assert!(
        r.h264.ledger_records < 40,
        "and little is outstanding at the end: {}",
        r.h264.ledger_records
    );
    // The estimator learns in H.264 mode.
    assert!(
        r.bwe_samples_seen as u64 >= s.tx_acked_live / 2,
        "samples_seen {} vs acked {}",
        r.bwe_samples_seen,
        s.tx_acked_live
    );
    // The client reassembled the stream, starting from the IDR.
    let d = delivered(&r);
    assert!(
        d.len() as u64 + 3 >= s.frames_sent,
        "{} of {}",
        d.len(),
        s.frames_sent
    );
    assert_eq!(d[0], (d[0].0, true), "the first frame delivered is the IDR");
    assert_eq!(s.frames_idr, 1, "and it is the only IDR");
    assert_eq!(s.idr_by_cause[IdrCause::Session.index()], 1);
}

/// The IDR's fragments are lost (first transmissions of three of them, the
/// resend of one, and the last fragment, which is found only once the resends
/// behind it are acknowledged: nothing else follows it while P frames are
/// held). The loss path sends them again, P frames wait for the IDR, and the
/// client's first frame is the IDR.
#[tokio::test(start_paused = true)]
async fn a_lost_idr_is_repaired_and_p_frames_wait_for_it() {
    for seed in [0x2201u64, 0x2202, 0x2203] {
        let mut net = link_20ms();
        net.jitter_us = 2_000;
        let drops = DropPlan::default().with_frame_rules(vec![
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Index(3),
                occurrences: vec![0, 1],
            },
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Index(10),
                occurrences: vec![0],
            },
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Last,
                occurrences: vec![0],
            },
        ]);
        let r = run(
            scene(seed, 6, net, load(40_000, 3_000), drops),
            RunOptions::default(),
        )
        .await
        .expect("scene");
        report(&format!("lost-idr {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: every rule fired (the index-3 rule twice: the resend was
        // lost too), the loss path resent, and P frames were held back.
        assert_eq!(r.frame_drops_fired, vec![2, 1, 1], "seed {seed:#x}");
        assert!(r.emitter_stats.h264_idr_resends >= 3, "seed {seed:#x}");
        assert!(
            s.frames_skipped_hold >= 1,
            "seed {seed:#x}: nothing was held"
        );
        // The IDR is the client's first frame, and nothing else was needed
        // for it: no second IDR, no hold timeout.
        let d = delivered(&r);
        assert_eq!(d.first(), Some(&(0, true)), "seed {seed:#x}: {d:?}");
        assert_eq!(s.frames_idr, 1, "seed {seed:#x}");
        assert_eq!(
            s.hold_timeouts, 0,
            "seed {seed:#x}: repaired, not timed out"
        );
        assert_eq!(s.verdicts_p_frag, 0, "seed {seed:#x}: no P frame was sent");
        // The stream then runs: P frames are delivered in order after it.
        // The interim policy's price at 3 % loss: a loss IDR owed to the rate
        // limit holds the stream (the P frames would chain from the suspect
        // frame), up to a second per IDR, and there is about one a second. The
        // stream keeps running at a fraction of the frames (100 to 166 of ~290
        // here); the bound only says it does not stall for good.
        assert!(d.len() > 60, "seed {seed:#x}: {}", d.len());
        assert!(
            d.windows(2).all(|w| w[0].0 < w[1].0),
            "seed {seed:#x}: in order"
        );
        // The hold ended well inside the ceiling.
        assert!(
            s.p_hold_ms < 2_000,
            "seed {seed:#x}: held {} ms",
            s.p_hold_ms
        );
    }
}

/// The estimate a scene should end above: half of what it was before the
/// disturbance (the design's bound for the acknowledgement stall; it is not
/// tuned). What it is measured against: the stall's only loss events are
/// verdicts on datagrams whose acknowledgements were lost (3 to 25 of ~400-600
/// transmissions), which goog_cc's loss-based controller meets with a bounded
/// step per update. Across the six runs below the lowest end-of-scene to
/// pre-stall ratio is 0.70 (seed 0x3301, quiet screen), against 0.5 here. The
/// estimate of an application-limited sender wanders by itself (a stall-free
/// control of the quiet scene dips to 0.60 of its pre-stall value between
/// IDR bursts), so it is the end of the scene that is compared, as the design
/// has it, not the minimum.
fn estimate_floor(pre_stall_bps: u64) -> u64 {
    pre_stall_bps / 2
}

fn estimate_at(r: &BrowserlessResult, at_us: u64) -> u64 {
    r.bwe_estimate_samples
        .iter()
        .rfind(|(t, _)| *t <= at_us)
        .map(|x| x.1)
        .expect("a sample before the stall")
}

/// An acknowledgement stall on a screen that has gone quiet: the last frames'
/// acknowledgements are lost, nothing new is sent, so the probe timeout is the
/// only thing running. It must send the newest fragments again (new
/// wire_seqs, a real probe) and declare nothing lost on its own authority.
///
/// The stall is 800 ms against a probe timeout of about 100 ms on this 40 ms
/// path, i.e. more than twice the PTO: the timer fires and backs off at least
/// three times (asserted: `pto_fires >= 3`).
#[tokio::test(start_paused = true)]
async fn an_acknowledgement_stall_on_a_quiet_screen_is_probed_not_declared_lost() {
    for seed in [0x3301u64, 0x3302, 0x3303] {
        let mut l = load(40_000, 3_000);
        // Frame 89 is the last before the screen goes quiet (t = 3.097 s);
        // the acknowledgements stop at 3.10 s and come back at 3.90 s.
        l.quiet = Some((90, 130));
        let opts = RunOptions {
            ack_blackout: Some((Duration::from_millis(3_100), Duration::from_millis(3_900))),
            ..RunOptions::default()
        };
        let mut net = link_20ms();
        net.jitter_us = 2_000;
        let r = run(scene(seed, 7, net, l, DropPlan::default()), opts)
            .await
            .expect("scene");
        report(&format!("quiet-stall {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: acknowledgements were really dropped, the probe timeout
        // fired repeatedly, and what it sent was fragments.
        assert!(r.h264.acks_blacked_out > 0, "seed {seed:#x}");
        assert!(r.emitter_stats.pto_fires >= 3, "seed {seed:#x}");
        assert!(r.emitter_stats.h264_probe_resends >= 3, "seed {seed:#x}");
        // The property: no fragment was declared lost by the probe's
        // content-gone rule. (Verdicts after the stall come from the packet
        // threshold, below.)
        assert_eq!(r.transport.instr.loss_by_pto, 0, "seed {seed:#x}");
        // The stall was on the acknowledgement path only: the client got every frame.
        let d = delivered(&r);
        assert!(d.len() as u64 + 3 >= s.frames_sent, "seed {seed:#x}");
        // IDRs: the session's, and at most the one the verdict burst after the
        // stall asks for (verdicts on frames that request covers ask for
        // nothing: `verdicts_superseded`).
        assert!(s.verdicts_p_frag > 0, "seed {seed:#x}: premise");
        assert!(s.frames_idr <= 2, "seed {seed:#x}: {} IDRs", s.frames_idr);
        assert!(
            s.idr_by_cause[IdrCause::Loss.index()] <= 1,
            "seed {seed:#x}"
        );
        // The estimate was not collapsed.
        let pre = estimate_at(&r, 3_100_000);
        let floor = estimate_floor(pre);
        println!(
            "  ratio end/pre {:.2}",
            r.bwe_estimate_bps as f64 / pre as f64
        );
        assert!(
            r.bwe_estimate_bps >= floor,
            "seed {seed:#x}: pre-stall {pre}, floor {floor}, end {}",
            r.bwe_estimate_bps
        );
        // Frames kept being acknowledged: the ones that never complete are
        // those with a fragment whose acknowledgement was lost for good (each
        // such fragment is a verdict), plus what is still in flight.
        assert!(
            s.frames_acked + s.verdicts_p_frag + 6 >= s.frames_sent,
            "seed {seed:#x}: {s:?}"
        );
        assert!(
            s.frames_acked * 10 >= s.frames_sent * 8,
            "seed {seed:#x}: {s:?}"
        );
        // And the books balance.
        assert_eq!(
            s.tx_recorded,
            s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten + r.h264.ledger_records as u64,
            "seed {seed:#x}: {s:?}"
        );
    }
}

/// An acknowledgement stall under a continuous stream: nothing is lost, but
/// for a while the server hears nothing, then hears of later datagrams and
/// judges the silent ones lost by the packet threshold. Those verdicts are
/// spurious; what they may cost is bounded by the IDR rate limit and by
/// ignoring verdicts an IDR already covers: one IDR, not one per frame (64
/// verdicts, 22 suspect frames).
#[tokio::test(start_paused = true)]
async fn an_acknowledgement_stall_under_a_busy_stream_costs_one_idr() {
    for seed in [0x4401u64, 0x4402, 0x4403] {
        let opts = RunOptions {
            ack_blackout: Some((Duration::from_millis(3_000), Duration::from_millis(3_800))),
            ..RunOptions::default()
        };
        let mut net = link_20ms();
        net.jitter_us = 2_000;
        let r = run(
            scene(seed, 7, net, load(40_000, 3_000), DropPlan::default()),
            opts,
        )
        .await
        .expect("scene");
        report(&format!("busy-stall {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: acknowledgements were dropped, and many verdicts followed.
        assert!(r.h264.acks_blacked_out > 10, "seed {seed:#x}");
        assert!(
            s.verdicts_p_frag >= 20,
            "seed {seed:#x}: {}",
            s.verdicts_p_frag
        );
        // The stall does not make the stream stop or the client miss frames.
        let d = delivered(&r);
        assert!(d.len() as u64 + 3 >= s.frames_sent, "seed {seed:#x}");
        // One IDR for the whole verdict burst, none per suspect frame. Design
        // section 5.2 (T3 scene 8) wants a total of 1, the session's: the loss
        // IDR here answers verdicts on datagrams that did arrive, because
        // their acknowledgements were lost. That gap closes in Phase B, when
        // a 0x0A `decoded` bit overrides a verdict; until then the interim
        // policy pays one IDR per stall, and this gate keeps it at that.
        assert_eq!(s.idr_by_cause[IdrCause::Loss.index()], 1, "seed {seed:#x}");
        assert_eq!(s.frames_idr, 2, "seed {seed:#x}");
        assert!(
            s.verdicts_superseded > 0,
            "seed {seed:#x}: the burst was covered"
        );
        let pre = estimate_at(&r, 3_000_000);
        let floor = estimate_floor(pre);
        println!(
            "  ratio end/pre {:.2}",
            r.bwe_estimate_bps as f64 / pre as f64
        );
        assert!(
            r.bwe_estimate_bps >= floor,
            "seed {seed:#x}: pre-stall {pre}, floor {floor}, end {}",
            r.bwe_estimate_bps
        );
        assert_eq!(
            s.tx_recorded,
            s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten + r.h264.ledger_records as u64,
            "seed {seed:#x}: {s:?}"
        );
    }
}

/// An acknowledgement blackout long enough that no frame completes for much
/// more than the 500 ms progress bound: the bridge raises `h264_stalled` (the
/// classifier's way out of H.264, which the scripted encoder path does not
/// run, so the flag is what is observable here), keeps it up through the
/// blackout, and drops it when frames complete again. A healthy stretch before
/// it raises nothing.
#[tokio::test(start_paused = true)]
async fn a_long_acknowledgement_blackout_raises_h264_stalled_and_recovery_clears_it() {
    const BLACKOUT_MS: (u64, u64) = (3_000, 5_000);
    for seed in [0x5501u64, 0x5502, 0x5503] {
        let opts = RunOptions {
            ack_blackout: Some((
                Duration::from_millis(BLACKOUT_MS.0),
                Duration::from_millis(BLACKOUT_MS.1),
            )),
            ..RunOptions::default()
        };
        let r = run(
            scene(
                seed,
                9,
                link_20ms(),
                load(40_000, 3_000),
                DropPlan::default(),
            ),
            opts,
        )
        .await
        .expect("scene");
        report(&format!("long-blackout {seed:#x}"), &r);
        let s = &r.h264.stats;
        println!(
            "  stalls inflight={} no_progress={} reset={} stalled_ms={} at_end={}",
            s.stalls_inflight,
            s.stalls_no_progress,
            s.stalls_reset,
            s.h264_stalled_ms,
            r.h264.stalled
        );
        // Premises: acknowledgements were dropped, and frames were sent
        // before, during and after.
        assert!(r.h264.acks_blacked_out > 10, "seed {seed:#x}");
        assert!(s.frames_acked > 30, "seed {seed:#x}: {}", s.frames_acked);
        // The stall: raised (once, for the blackout), accounted for roughly
        // as long as the blackout minus the progress bound, and not still up.
        assert!(
            s.stalls_inflight + s.stalls_no_progress >= 1,
            "seed {seed:#x}: {s:?}"
        );
        assert!(
            s.h264_stalled_ms >= 1_000 && s.h264_stalled_ms <= 3_000,
            "seed {seed:#x}: stalled {} ms for a 2000 ms blackout",
            s.h264_stalled_ms
        );
        assert!(!r.h264.stalled, "seed {seed:#x}: still stalled at the end");
    }
}

/// And a healthy run never raises it: the flag is not a side effect of load.
#[tokio::test(start_paused = true)]
async fn a_healthy_h264_stream_never_stalls() {
    for seed in [0x5601u64, 0x5602] {
        let r = run(
            scene(
                seed,
                6,
                link_20ms(),
                load(40_000, 3_000),
                DropPlan::default(),
            ),
            RunOptions::default(),
        )
        .await
        .expect("scene");
        report(&format!("healthy-stall {seed:#x}"), &r);
        let s = &r.h264.stats;
        assert!(s.frames_acked > 100, "premise, seed {seed:#x}: {s:?}");
        assert_eq!(
            (
                s.stalls_inflight,
                s.stalls_no_progress,
                s.stalls_reset,
                s.h264_stalled_ms
            ),
            (0, 0, 0, 0),
            "seed {seed:#x}"
        );
    }
}

/// A ~100 KB first IDR over a 1 Mbit/s link takes 0.8 s to drain, longer than
/// the 500 ms no-progress bound, with no whole frame complete until the end.
/// Fragments are acknowledged all the while: that is progress, not a stall
/// (a whole-frame notion of progress flapped the session tile <-> H.264 every
/// few seconds on a starved link).
#[tokio::test(start_paused = true)]
async fn a_large_idr_on_a_slow_link_is_not_a_stall() {
    const LINK_BYTES_PER_S: u64 = 125_000;
    const IDR_BYTES: usize = 100_000;
    for seed in [0x5701u64, 0x5702, 0x5703] {
        let net = NetProfile {
            delay_us: 20_000,
            cap: CapTimeline::constant(LINK_BYTES_PER_S),
            bottleneck: Some(Bottleneck {
                depth_ms: 3_000,
                aqm: None,
            }),
            ..NetProfile::perfect()
        };
        let r = run(
            scene(seed, 8, net, load(IDR_BYTES, 1_500), DropPlan::default()),
            RunOptions::default(),
        )
        .await
        .expect("scene");
        report(&format!("slow-idr {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: the IDR alone takes longer than the bound to cross the
        // link, and it did cross (the stream went on).
        assert!(
            IDR_BYTES as u64 * 1000 / LINK_BYTES_PER_S > 500,
            "seed {seed:#x}: the IDR must outlast the 500 ms bound"
        );
        assert!(s.frames_acked >= 10, "seed {seed:#x}: {s:?}");
        assert_eq!(
            (s.stalls_inflight, s.stalls_no_progress, s.stalls_reset),
            (0, 0, 0),
            "seed {seed:#x}: {s:?}"
        );
        assert_eq!(s.h264_stalled_ms, 0, "seed {seed:#x}");
    }
}

/// A link that carries a fifth of what the encoder produces (1 Mbit/s against
/// 20 KB P frames at 30 fps), behind a buffer deep enough that nothing is
/// dropped. The encoder is not run into a queue it cannot drain: frames are
/// skipped, the queue and the delay stay bounded, and every captured frame is
/// either sent or skipped.
#[tokio::test(start_paused = true)]
async fn an_overdriven_link_skips_frames_instead_of_queueing_them() {
    const LINK_BYTES_PER_S: u64 = 125_000;
    const P_BYTES: usize = 20_000;
    for seed in [0x5501u64, 0x5502, 0x5503] {
        let net = NetProfile {
            delay_us: 10_000,
            jitter_us: 1_000,
            // Bytes per second despite the name.
            cap: CapTimeline::constant(LINK_BYTES_PER_S),
            // Deep enough that nothing is dropped: the queue the encoder's
            // frame skip must keep short would otherwise hide in tail drops.
            bottleneck: Some(Bottleneck {
                depth_ms: 3_000,
                aqm: None,
            }),
            ..NetProfile::perfect()
        };
        let r = run(
            scene(seed, 10, net, load(10_000, P_BYTES), DropPlan::default()),
            RunOptions::default(),
        )
        .await
        .expect("scene");
        report(&format!("overdriven {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: nothing was dropped, the encoder produced far more than the
        // link carries, and frames were skipped for the queue.
        assert_eq!(r.s2c_datagrams_dropped, 0, "seed {seed:#x}");
        assert!(s.frames_skipped_backpressure > 100, "seed {seed:#x}: {s:?}");
        // Every captured frame was sent or skipped; none vanished.
        let injected = r.injections.iter().filter(|(_, hb)| !hb).count() as u64;
        assert_eq!(
            s.frames_sent + s.frames_skipped_hold + s.frames_skipped_backpressure,
            injected,
            "seed {seed:#x}: {s:?}"
        );
        // The queue the gate allows: one frame period of the highest estimate
        // seen, two gate depths (10 ms of it each), plus the one frame that
        // is encoded when the queue is just under the limit (and its parity,
        // 20 % on a P frame). At the link's rate that is the queueing delay.
        let est_max = r.bwe_estimate_samples.iter().map(|x| x.1).max().unwrap() as f64 / 8.0;
        let limit = est_max / 30.0 + 2.0 * 0.010 * est_max;
        let frame = P_BYTES as f64 * 1.25;
        let bound_bytes = limit + frame;
        let bound_delay_us = (bound_bytes / LINK_BYTES_PER_S as f64 * 1e6) as u64;
        let mut queue_us: Vec<u64> = r
            .s2c_transit_samples
            .iter()
            .map(|x| x.1.saturating_sub(10_000))
            .collect();
        queue_us.sort_unstable();
        let p95 = queue_us[queue_us.len() * 95 / 100];
        assert!(
            p95 <= bound_delay_us + 50_000,
            "seed {seed:#x}: p95 queueing {p95} us, bound {bound_delay_us} us (+50 ms)"
        );
        assert!(
            r.emission_queue_peak as f64 <= bound_bytes / 1_100.0 + 10.0,
            "seed {seed:#x}: emitter queue peaked at {} datagrams, bound {}",
            r.emission_queue_peak,
            bound_bytes / 1_100.0
        );
        // The stream still moves: frames are delivered to the end of the scene.
        let d = delivered(&r);
        assert!(d.len() as u64 + 3 >= s.frames_sent, "seed {seed:#x}");
        assert!(d.len() >= 20, "seed {seed:#x}: {}", d.len());
    }
}

/// Independent loss on the stream: the books balance, every IDR is repaired
/// and delivered, and loss-triggered IDRs keep to one per second.
#[tokio::test(start_paused = true)]
async fn under_random_loss_the_books_balance_and_idrs_keep_to_the_rate_limit() {
    const SECS: u64 = 10;
    for seed in [0x6601u64, 0x6602, 0x6603] {
        let net = NetProfile {
            loss: 0.03,
            delay_us: 20_000,
            jitter_us: 2_000,
            ..NetProfile::perfect()
        };
        let r = run(
            scene(seed, SECS, net, load(15_000, 3_000), DropPlan::default()),
            RunOptions::default(),
        )
        .await
        .expect("scene");
        report(&format!("loss3% {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: the network dropped datagrams, verdicts followed on P
        // fragments, and the loss path sent IDR fragments again.
        assert!(r.s2c_datagrams_dropped > 20, "seed {seed:#x}");
        assert!(s.verdicts_p_frag > 0, "seed {seed:#x}: {s:?}");
        // Every transmission has exactly one outcome.
        assert_eq!(
            s.tx_recorded,
            s.tx_acked_live + s.tx_judged_lost + s.tx_forgotten + r.h264.ledger_records as u64,
            "seed {seed:#x}: {s:?}"
        );
        // The estimator keeps learning.
        assert!(
            r.bwe_samples_seen as u64 >= s.tx_acked_live / 2,
            "seed {seed:#x}"
        );
        // Loss-triggered IDRs: at most one per second over the scene.
        let loss_idrs = s.idr_by_cause[IdrCause::Loss.index()];
        assert!(
            loss_idrs >= 1,
            "seed {seed:#x}: loss never triggered an IDR"
        );
        assert!(
            loss_idrs <= SECS + 1,
            "seed {seed:#x}: {loss_idrs} loss IDRs"
        );
        // Every IDR but one still in flight reached the client, and the hold
        // never gave up (the loss path repaired each one).
        let delivered_idrs = delivered(&r).iter().filter(|(_, k)| *k).count() as u64;
        assert!(
            delivered_idrs + 1 >= s.frames_idr,
            "seed {seed:#x}: {delivered_idrs} of {}",
            s.frames_idr
        );
        assert_eq!(s.hold_timeouts, 0, "seed {seed:#x}");
        // Frames are in order on the wire as the client sees them.
        let d = delivered(&r);
        // The interim policy's price at 3 % loss: a loss IDR owed to the rate
        // limit holds the stream (the P frames would chain from the suspect
        // frame), up to a second per IDR, and there is about one a second. The
        // stream keeps running at a fraction of the frames (100 to 166 of ~290
        // here); the bound only says it does not stall for good.
        assert!(d.len() > 60, "seed {seed:#x}: {}", d.len());
    }
}

/// The encoder is still on its own GOP of 11 until A4: its periodic IDRs hold
/// nothing. On a healthy link the only hold is the session's IDR, so P frames
/// are skipped for hold only while that one drains.
///
/// The bound: the session IDR is 8 KB sent at the initial 2 Mbit/s estimate,
/// about 32 ms plus a 40 ms round trip, i.e. 3 frame periods at 30 fps; the
/// assertion allows 6 (200 ms). Holding on every periodic IDR as well would
/// cost about that again at each of the ~16 IDRs of the 6 s scene, 50 frames
/// or more.
#[tokio::test(start_paused = true)]
async fn periodic_idrs_hold_no_p_frames() {
    for seed in [0x7701u64, 0x7702, 0x7703] {
        let mut l = load(8_000, 2_000);
        l.gop_frames = 11;
        let mut net = link_20ms();
        net.jitter_us = 2_000;
        let r = run(
            scene(seed, 6, net, l, DropPlan::default()),
            RunOptions::default(),
        )
        .await
        .expect("scene");
        report(&format!("gop11 {seed:#x}"), &r);
        let s = &r.h264.stats;
        // Premises: the GOP really produced periodic IDRs, all delivered.
        let gop = s.idr_by_cause[IdrCause::GopPeriodic.index()];
        assert!(gop >= 10, "seed {seed:#x}: {gop} periodic IDRs");
        assert_eq!(
            s.idr_by_cause[IdrCause::Session.index()],
            1,
            "seed {seed:#x}"
        );
        let delivered_idrs = delivered(&r).iter().filter(|(_, k)| *k).count() as u64;
        assert!(delivered_idrs + 1 >= s.frames_idr, "seed {seed:#x}");
        // The property.
        assert_eq!(s.p_holds, 1, "seed {seed:#x}: only the session's IDR holds");
        assert!(
            s.frames_skipped_hold <= 6,
            "seed {seed:#x}: {} frames skipped for hold",
            s.frames_skipped_hold
        );
    }
}
