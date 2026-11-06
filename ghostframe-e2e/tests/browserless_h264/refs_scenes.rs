//! T3 scenes on the real acked-reference policy (design 10, B5b).
//!
//! The same scenes, seeds and client as `pcm_scenes` (the legacy baseline), but
//! the stream is planned by production's `RefManager` path: the manager, the
//! wire header from the plan, the send path, the ledger feedback and the
//! client's own 0x0A and 0x09 are all production's; the PCM backend stands in
//! for the encoder (`encoder::h264_harness`). Every scene runs three seeds,
//! states its premise (the loss fired, the reorder happened, the manager
//! recovered) before the effect, and asserts properties (bounded, no wrong
//! picture) rather than numbers that only hold for one fixture.
use super::*;
use ghostframe_e2e::harness::h264_scene::{capacity_step_net, pcm_load, H264Outcome, Pattern};
use ghostframe_lib::encoder::h264_harness::PcmMode;

const SEEDS: [u64; 3] = [0x7101, 0x7102, 0x7103];
const SECS: u64 = 10;
/// One-way delay of `link_20ms`, doubled.
const RTT_US: u64 = 40_000;
/// The loss path's verdict latency on this link (time threshold plus the
/// reordering allowance), generously.
const VERDICT_US: u64 = 100_000;

fn refs() -> H264Load {
    pcm_load(PcmMode::RefManager)
}

async fn run_on(
    seed: u64,
    secs: u64,
    base: NetProfile,
    pattern: Pattern,
    load: H264Load,
    drops: DropPlan,
    opts: RunOptions,
) -> (BrowserlessResult, H264Outcome) {
    let (net, links) = pattern.apply(base);
    let r = run_browserless_with_options(scene(seed, secs, net, load, drops), links, opts)
        .await
        .expect("scene");
    let o = r.h264_run().map(|x| x.analyse()).unwrap_or_default();
    (r, o)
}

async fn run_pattern(seed: u64, secs: u64, pattern: Pattern) -> (BrowserlessResult, H264Outcome) {
    run_on(
        seed,
        secs,
        link_20ms(),
        pattern,
        refs(),
        DropPlan::default(),
        RunOptions::default(),
    )
    .await
}

fn line(name: &str, seed: u64, r: &BrowserlessResult, o: &H264Outcome) {
    let c = &o.refs;
    println!(
        "{name} {seed:#x}: {} | idr={} recov={}/{} reroots={} anchors={} cand={} overridden={} \
         refused={} decoded={} holds={} acks_dropped={} | drops={}/{} verdicts p={} idr={}",
        o.summary(),
        o.idr_made,
        c.recoveries,
        c.recovered,
        c.reroots,
        c.anchors_confirmed,
        c.candidates_marked,
        c.suspect_overridden,
        c.frames_refused,
        c.frames_decoded_acked,
        c.holds,
        r.h264.frame_acks_dropped,
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_offered,
        r.h264.stats.verdicts_p_frag,
        r.h264.stats.verdicts_idr_frag,
    );
}

/// What holds in every acked-reference scene: the measurement is sound, the
/// client displayed no wrong picture, the manager's plan matched the stream,
/// and the stream is alive at the end.
fn assert_sound(ctx: &str, secs: u64, r: &BrowserlessResult, o: &H264Outcome) {
    assert_eq!(o.unknown_to_truth, 0, "{ctx}");
    assert_eq!(o.header_ref_mismatches, 0, "{ctx}");
    assert!(o.fed > 0 && o.correct > 0, "{ctx}: nothing was displayed");
    assert!(
        o.wrong.is_empty(),
        "{ctx}: wrong pictures displayed: {}",
        o.wrong_list(8)
    );
    assert_eq!((o.rejected, o.no_picture, o.damaged), (0, 0, 0), "{ctx}");
    assert_eq!(o.decoder_resets, 0, "{ctx}");
    assert_eq!(o.correct, o.fed, "{ctx}: a fed frame was not right");
    assert_eq!(o.refs.plan_mismatch, 0, "{ctx}");
    assert_eq!(o.refs.backend_failures, 0, "{ctx}");
    // The bridge and the source agree on the IDR count.
    assert_eq!(o.idr_made as u64, r.h264.stats.frames_idr, "{ctx}");
    assert!(
        o.last_correct_us.unwrap_or(0) + 1_000_000 >= secs * 1_000_000,
        "{ctx}: dead at the end: {:?}",
        o.last_correct_us
    );
}

/// The effects of a lossy path on the acked-reference policy: one IDR for the
/// whole scene, recoveries from the anchor, frames chained only from frames the
/// client showed, and every freeze bounded by what the recovery costs.
fn assert_recovers(ctx: &str, secs: u64, r: &BrowserlessResult, o: &H264Outcome) {
    // Premise: the path dropped frame datagrams, the loss path judged P
    // fragments lost, and frames never reached the client.
    assert!(r.s2c_datagrams_dropped > 0, "{ctx}: nothing was dropped");
    assert!(r.h264.stats.verdicts_p_frag > 0, "{ctx}: no verdict");
    assert!(o.never_fed > 0, "{ctx}: no frame was lost for good");
    assert_sound(ctx, secs, r, o);
    let c = &o.refs;
    assert_eq!(o.idr_made, 1, "{ctx}: the manager makes no loss IDR");
    assert_eq!(c.idr_session, 1, "{ctx}");
    assert!(c.recoveries >= 1 && c.p_recovery > 0, "{ctx}: {c:?}");
    assert!(c.recovered + 1 >= c.recoveries, "{ctx}: {c:?}");
    assert!(
        o.refs_to_undisplayed < o.made / 4,
        "{ctx}: {} of {} frames chained from a frame the client never showed",
        o.refs_to_undisplayed,
        o.made
    );
    let bound = o.freeze_bound_us(
        RTT_US,
        VERDICT_US,
        ghostframe_e2e::harness::browserless::DEFAULT_CADENCE_US,
    );
    assert!(
        o.max_gap_us <= bound,
        "{ctx}: gap {} us beyond the bound {bound} us (longest lost run {})",
        o.max_gap_us,
        o.longest_lost_run
    );
}

/// A clean stream: one IDR, every frame right, no freeze, candidates confirmed.
#[tokio::test(start_paused = true)]
async fn refs_clean_stream_shows_every_frame_right_with_one_idr() {
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, 6, Pattern::Clean).await;
        line("refs-clean", seed, &r, &o);
        let ctx = format!("refs-clean {seed:#x}");
        assert_sound(&ctx, 6, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert_eq!(o.freezes, 0, "{ctx}");
        assert!(o.correct > 100, "{ctx}");
        let c = &o.refs;
        assert_eq!(
            (c.recoveries, c.p_recovery, c.idr_reissued),
            (0, 0, 0),
            "{ctx}"
        );
        assert!(
            c.anchors_confirmed > 0 && c.frames_decoded_acked > 0,
            "{ctx}: {c:?}"
        );
        // No parity on the wire, no GOP: every frame past the first is a P.
        assert_eq!(
            r.h264.stats.idr_by_cause[IdrCause::GopPeriodic.index()],
            0,
            "{ctx}"
        );
    }
}

/// Scene 1: independent loss, 1 % and 5 %.
#[tokio::test(start_paused = true)]
async fn refs_scene_1_iid_loss_recovers_from_the_anchor_with_one_idr() {
    for (p, name) in [(0.01, "iid1"), (0.05, "iid5")] {
        for seed in SEEDS {
            let (r, o) = run_pattern(seed, SECS, Pattern::Iid(p)).await;
            line(name, seed, &r, &o);
            assert_recovers(&format!("{name} {seed:#x}"), SECS, &r, &o);
        }
    }
}

/// Scene 2: bursts, about 4 datagrams a burst at 80 % loss.
#[tokio::test(start_paused = true)]
async fn refs_scene_2_burst_loss_recovers_from_the_anchor_with_one_idr() {
    let burst = Pattern::Burst {
        enter: 0.01,
        exit: 0.3,
        loss: 0.8,
    };
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, SECS, burst).await;
        line("burst", seed, &r, &o);
        assert_recovers(&format!("burst {seed:#x}"), SECS, &r, &o);
    }
}

/// Scene 3: the frame feedback (0x0A) alone is lost, for 1.5 s. Every frame
/// arrives, so nothing needs repair, but the manager hears nothing: the
/// candidates it marked age out (decode deadline) and it recovers from the last
/// anchor it confirmed, without corruption and without an IDR. When the feedback
/// returns, its decoded bits override the suspicion.
#[tokio::test(start_paused = true)]
async fn refs_scene_3_lost_frame_acks_age_the_candidates_without_corruption() {
    for seed in SEEDS {
        let opts = RunOptions {
            frame_ack_loss: Some((
                1.0,
                Duration::from_millis(2_000),
                Duration::from_millis(3_500),
            )),
            ..RunOptions::default()
        };
        let (r, o) = run_on(
            seed,
            SECS,
            link_20ms(),
            Pattern::Clean,
            refs(),
            DropPlan::default(),
            opts,
        )
        .await;
        line("noacks", seed, &r, &o);
        let ctx = format!("noacks {seed:#x}");
        let c = &o.refs;
        // Premise: only the feedback was lost, and the manager noticed the
        // silence (frames went suspect by their deadline).
        assert_eq!(r.s2c_datagrams_dropped, 0, "{ctx}");
        assert!(r.h264.frame_acks_dropped > 10, "{ctx}");
        assert!(c.deadline_suspects > 0, "{ctx}: {c:?}");
        assert_sound(&ctx, SECS, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert_eq!(c.idr_reissued, 0, "{ctx}");
        // Everything sent arrived: nothing was lost for good.
        assert!(o.never_fed <= 2, "{ctx}: {} frames lost", o.never_fed);
        // The silence was survived by recovering from the anchor, and the
        // returning decoded bits won over the suspicion.
        assert!(c.recoveries >= 1, "{ctx}: {c:?}");
        assert!(c.suspect_overridden > 0, "{ctx}: {c:?}");
        assert!(c.anchors_confirmed > 5, "{ctx}: confirmation resumed");
    }
}

/// Scene 4: reordering. Within a quarter of a frame period datagrams swap
/// inside a frame and nothing is late. At 45 ms, longer than a frame period,
/// frames complete out of order: the gate refuses the late ones (a refused bit,
/// never a decode), the loss path mistakes late datagrams for lost, and the
/// manager recovers from its anchor. No wrong picture, no drift, one IDR.
#[tokio::test(start_paused = true)]
async fn refs_scene_4_reordering_refuses_late_frames_and_never_drifts() {
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, SECS, Pattern::Reorder { window_us: 8_000 }).await;
        line("reorder8", seed, &r, &o);
        let ctx = format!("reorder8 {seed:#x}");
        assert_sound(&ctx, SECS, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert!(o.correct > 200, "{ctx}");
    }
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, SECS, Pattern::Reorder { window_us: 45_000 }).await;
        line("reorder45", seed, &r, &o);
        let ctx = format!("reorder45 {seed:#x}");
        // Premise: the path reordered, dropped nothing, and the loss path took
        // lateness for loss.
        let (lo, hi) = r
            .s2c_transit_samples
            .iter()
            .fold((u64::MAX, 0), |(lo, hi), &(_, d)| (lo.min(d), hi.max(d)));
        assert!(hi - lo >= 22_000, "{ctx}: transit {lo}..{hi} us");
        assert_eq!(r.s2c_datagrams_dropped, 0, "{ctx}");
        assert!(
            r.h264.stats.verdicts_p_frag > 0,
            "{ctx}: no late datagram was judged lost"
        );
        assert_sound(&ctx, SECS, &r, &o);
        let c = &o.refs;
        // Late frames are refused, never decoded out of order.
        assert_eq!(o.fed_out_of_order, 0, "{ctx}");
        assert!(
            c.frames_refused > 0 || o.never_fed > 0,
            "{ctx}: nothing was late: {c:?}"
        );
        assert_eq!(
            o.idr_made, 1,
            "{ctx}: reordering is not a reason for an IDR"
        );
        assert!(
            o.refs_to_undisplayed < o.made / 2,
            "{ctx}: {} of {} frames chained from a frame never shown",
            o.refs_to_undisplayed,
            o.made
        );
    }
}

/// Scene 5: H.264 -> tile -> H.264 -> tile -> H.264. A return is a recovery P
/// from the newest confirmed anchor, not an IDR: one IDR for the scene, and the
/// stream resumes within a few round trips of each return.
#[tokio::test(start_paused = true)]
async fn refs_scene_5_tile_transitions_return_to_h264_from_the_anchor() {
    for seed in SEEDS {
        let mut l = refs();
        l.tile_phases = [Some((90, 150)), Some((210, 250))];
        let (r, o) = run_on(
            seed,
            SECS,
            link_20ms(),
            Pattern::Clean,
            l,
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        line("tiles", seed, &r, &o);
        let ctx = format!("tiles {seed:#x}");
        // Premise: the tile stretches happened, and H.264 ran either side.
        let tiles = r
            .events
            .iter()
            .filter(|e| matches!(e, ClientNetEvent::Core(CoreEvent::TileReady { .. })))
            .count();
        assert!(tiles >= 90, "{ctx}: {tiles} tile paints");
        assert_sound(&ctx, SECS, &r, &o);
        let c = &o.refs;
        assert_eq!(o.idr_made, 1, "{ctx}: only the session's IDR");
        assert_eq!(
            r.h264.stats.idr_by_cause[IdrCause::Reentry.index()],
            0,
            "{ctx}"
        );
        assert_eq!(c.reentries, 2, "{ctx}: {c:?}");
        assert!(c.reentry_bytes > 0, "{ctx}");
        // Each return resumes at once: the recovery P is the first frame, and
        // it needs no IDR and no hold.
        assert_eq!(o.resume_us.len(), 2, "{ctx}");
        for (i, t) in o.resume_us.iter().enumerate() {
            let t = t.unwrap_or_else(|| panic!("{ctx}: no picture after tile stretch {i}"));
            assert!(t <= 4 * RTT_US, "{ctx}: stretch {i} resumed after {t} us");
        }
    }
}

/// Scene 7: lost IDR. Its fragments are resent by the loss path, the manager
/// holds its P frames until the IDR is delivered, and the first thing the client
/// decodes is the IDR, right. The manager does not give up on it and send
/// another.
#[tokio::test(start_paused = true)]
async fn refs_scene_7_a_lost_idr_is_resent_and_p_frames_wait_for_it() {
    for seed in SEEDS {
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
        let mut net = link_20ms();
        net.jitter_us = 2_000;
        let (r, o) = run_on(
            seed,
            6,
            net,
            Pattern::Clean,
            refs(),
            drops,
            RunOptions::default(),
        )
        .await;
        line("lostidr", seed, &r, &o);
        let ctx = format!("lostidr {seed:#x}");
        let c = &o.refs;
        // Premise: every rule fired, the loss path resent, P frames were held.
        assert_eq!(r.frame_drops_fired, vec![2, 1, 1], "{ctx}");
        assert!(r.emitter_stats.h264_idr_resends >= 3, "{ctx}");
        assert!(c.holds >= 1, "{ctx}: the manager held nothing");
        assert_sound(&ctx, 6, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}: repaired, not replaced");
        assert_eq!(c.idr_reissued, 0, "{ctx}");
        let run = r.h264_run().unwrap();
        assert_eq!(
            run.display.first().map(|d| d.frame_seq),
            Some(0),
            "{ctx}: the IDR is the first thing the client decoded"
        );
        // Decoded within K round trips of the IDR being sent: the session takes
        // 313 ms, the repair adds a few round trips (one per lost resend).
        assert!(
            o.first_correct_us
                .is_some_and(|t| t <= 313_000 + 12 * RTT_US),
            "{ctx}: {:?}",
            o.first_correct_us
        );
        assert!(o.correct > 100, "{ctx}");
    }
}

/// Scene 8: an acknowledgement stall. The per-datagram acks (0x06) stop for
/// 0.8 s while nothing is lost, and the frame feedback (0x0A) is late by 0.9 s
/// (it resumes just after the stall ends). The loss path judges
/// silent datagrams lost before the client's word about them arrives; those
/// verdicts are hypotheses, and the decoded bits that follow override them: one
/// IDR for the scene (the session's), no frame is killed by a verdict, the
/// stream and the frame feedback go on.
#[tokio::test(start_paused = true)]
async fn refs_scene_8_an_ack_stall_costs_no_idr_and_a_decoded_bit_beats_a_verdict() {
    for seed in [0x4401u64, 0x4402, 0x4403] {
        let opts = RunOptions {
            ack_blackout: Some((Duration::from_millis(3_000), Duration::from_millis(3_800))),
            frame_ack_loss: Some((
                1.0,
                Duration::from_millis(3_000),
                Duration::from_millis(3_900),
            )),
            ..RunOptions::default()
        };
        let mut net = link_20ms();
        net.jitter_us = 2_000;
        let (r, o) = run_on(
            seed,
            7,
            net,
            Pattern::Clean,
            refs(),
            DropPlan::default(),
            opts,
        )
        .await;
        line("ackstall", seed, &r, &o);
        let ctx = format!("ackstall {seed:#x}");
        let s = &r.h264.stats;
        let c = &o.refs;
        // Premise: acknowledgements were dropped and verdicts followed on
        // frames that arrived.
        assert!(r.h264.acks_blacked_out > 10, "{ctx}");
        assert!(s.verdicts_p_frag >= 20, "{ctx}: {}", s.verdicts_p_frag);
        assert_eq!(r.s2c_datagrams_dropped, 0, "{ctx}");
        assert_sound(&ctx, 7, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}: a stall is not a reason for an IDR");
        // The verdicts were hypotheses and the decoded bits beat them: some
        // verdict came before its frame's decoded bit and lost to it.
        assert!(c.suspect_overridden > 0, "{ctx}: {c:?}");
        // Recoveries are bounded by the suspicions there were.
        assert!(
            c.recoveries <= c.verdicts + c.deadline_suspects,
            "{ctx}: {c:?}"
        );
        // The frame feedback kept flowing through the stall.
        assert!(c.frames_decoded_acked + 3 >= s.frames_sent, "{ctx}: {c:?}");
        // The estimate survived (the loss-based estimator's step is bounded).
        let pre = estimate_at(&r, 3_000_000);
        assert!(
            r.bwe_estimate_bps >= estimate_floor(pre),
            "{ctx}: pre-stall {pre}, end {}",
            r.bwe_estimate_bps
        );
    }
}

/// Scene 10: the slot-overwrite race. The frame feedback comes back late and
/// shuffled (up to 600 ms), so a candidate can be given up on and its slot
/// reused before its decoded bit arrives; with loss on top, a recovery may be
/// wanted exactly then. The manager must not treat the late bit as confirming
/// the new occupant, and must never recover from a picture the client does not
/// hold: no wrong picture, no stale-slot reference, one IDR.
#[tokio::test(start_paused = true)]
async fn refs_scene_10_a_late_decoded_bit_never_confirms_an_overwritten_slot() {
    let mut late_total = 0u64;
    for seed in SEEDS {
        let mut c2s = link_20ms();
        c2s.reorder_us = 600_000;
        let base = link_20ms();
        let (net, mut links) = Pattern::Iid(0.03).apply(base);
        links.c2s = Some({
            let mut p = links.c2s.take().unwrap_or(c2s.clone());
            p.reorder_us = 600_000;
            p
        });
        let r = run_browserless_with_options(
            scene(seed, SECS, net, refs(), DropPlan::default()),
            links,
            RunOptions::default(),
        )
        .await
        .expect("scene");
        let o = r.h264_run().map(|x| x.analyse()).unwrap_or_default();
        line("slotrace", seed, &r, &o);
        let ctx = format!("slotrace {seed:#x}");
        let c = &o.refs;
        assert!(r.s2c_datagrams_dropped > 0, "{ctx}");
        assert_sound(&ctx, SECS, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert!(c.recoveries >= 1, "{ctx}: {c:?}");
        late_total += c.late_confirm_ignored;
    }
    // Premise: the race really happened (a decoded bit for an overwritten
    // candidate arrived and was ignored) in at least one seed.
    assert!(late_total > 0, "the scene never produced the race");
}

/// Probing without a periodic IDR: the application's own probe clusters and the
/// padding that fills them still run on an acked-reference stream, so a capacity
/// step-up has something to find (design 4.3). The burst used to be the GOP's
/// IDR; there is none now.
#[tokio::test(start_paused = true)]
async fn refs_probing_works_without_a_periodic_idr() {
    for seed in SEEDS {
        let mut l = refs();
        l.pcm_rate_policy = true;
        let (r, o) = run_on(
            seed,
            20,
            super::bottleneck_net(super::CapTimeline::constant(500_000), 20_000, 0, 600),
            Pattern::Clean,
            l,
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        line("probing", seed, &r, &o);
        let ctx = format!("probing {seed:#x}");
        assert_sound(&ctx, 20, &r, &o);
        let s = &r.h264.stats;
        assert_eq!(o.idr_made, 1, "{ctx}: no periodic IDR in 20 s");
        assert_eq!(s.idr_by_cause[IdrCause::GopPeriodic.index()], 0, "{ctx}");
        assert!(s.app_probes > 0, "{ctx}: the application never probed");
        assert!(s.probe_padding_windows > 0, "{ctx}: nothing was padded");
    }
}

/// The loss scenes again with production's rate policy on (and the padding it
/// brings): the stream still recovers from its anchor with one IDR. The
/// padding-on-a-reordering-path stall is a known limit and is not run here.
#[tokio::test(start_paused = true)]
async fn refs_loss_scenes_hold_with_the_rate_policy_on() {
    let burst = Pattern::Burst {
        enter: 0.01,
        exit: 0.3,
        loss: 0.8,
    };
    for (name, pattern) in [
        ("rate-iid1", Pattern::Iid(0.01)),
        ("rate-iid5", Pattern::Iid(0.05)),
        ("rate-burst", burst),
    ] {
        for seed in SEEDS {
            let mut l = refs();
            l.pcm_rate_policy = true;
            let (r, o) = run_on(
                seed,
                SECS,
                link_20ms(),
                pattern,
                l,
                DropPlan::default(),
                RunOptions::default(),
            )
            .await;
            line(name, seed, &r, &o);
            assert_recovers(&format!("{name} {seed:#x}"), SECS, &r, &o);
        }
    }
}

/// The path MTU falls after discovery has raised it (see the legacy scene in
/// `pcm_scenes`), on an acked-reference session. Fragments cut for the larger
/// size and queued or cached outgrow the connection and are retired. The
/// Phase A answer (an IDR for the new size) is not this policy's: the manager
/// treats each retired frame as lost and recovers from its anchor, with
/// frames cut for the new size; only the IDR still in flight, which cannot
/// complete, is cut again. So: the stream lives, no wrong picture is shown,
/// the manager recovered, and the IDR count stays bounded.
#[tokio::test(start_paused = true)]
async fn refs_an_mtu_that_falls_after_discovery_recovers_from_the_anchor() {
    for (k, seed) in SEEDS.into_iter().enumerate() {
        // `Pattern::Clean` draws nothing from the seed: each run gets its own
        // fall instant and delay, so the fall meets the cadence differently.
        let k = k as u64;
        let (net, mut links) = Pattern::Clean.apply(capacity_step_net(
            500_000,
            3_500_000 + k * 11_000,
            60_000,
            20_000 + k * 1_300,
            300,
        ));
        links.s2c_mtu_step = Some((4_000_000 + k * 37_000, 1_200));
        let load = refs();
        let r = run_browserless_with_options(
            scene(seed, SECS, net, load, DropPlan::default()),
            links,
            RunOptions::default(),
        )
        .await
        .expect("scene");
        let o = r.h264_run().map(|x| x.analyse()).unwrap_or_default();
        line("refs-mtu-fall", seed, &r, &o);
        let c = &o.refs;
        let s = &r.h264.stats;
        println!(
            "refs-mtu-fall {seed:#x}: oversize_h264={} mtu_retired={}/{} idr_mtu_shrink={} \
             frozen_ms={} idr_made={} recov={} correct={}",
            s.oversize_h264,
            c.mtu_retired,
            c.mtu_retired_idr,
            c.idr_mtu_shrink,
            o.frozen_us / 1000,
            o.idr_made,
            c.recoveries,
            o.correct
        );
        let ctx = format!("refs-mtu-fall {seed:#x}");
        // Premises: the link black-holed large packets, the fall caught
        // fragments already built, and the retirement went to the manager.
        assert!(r.s2c_datagrams_dropped > 0, "{ctx}: nothing was dropped");
        assert!(s.oversize_h264 > 0, "{ctx}: no fragment outgrew the MTU");
        assert!(c.active, "{ctx}: not an acked-reference session");
        assert!(c.mtu_retired > 0, "{ctx}: the manager was told of nothing");
        // Nothing stale reached quinn, and the Phase A request was not made.
        assert_eq!(s.send_too_large, 0, "{ctx}");
        assert_eq!(
            s.idr_requests_by_cause[IdrCause::MtuShrink.index()],
            0,
            "{ctx}: the Phase A MTU IDR was requested"
        );
        // No wedge, no wrong picture, alive at the end.
        assert_sound(&ctx, SECS, &r, &o);
        assert!(o.correct > 150, "{ctx}: {} frames shown", o.correct);
        assert!(o.frozen_us <= 1_500_000, "{ctx}: frozen {} us", o.frozen_us);
        // This scene proves routing (`mtu_retired > 0`, no Phase A MtuShrink
        // request, `send_too_large == 0`) and harmlessness. `recoveries` is
        // also satisfied by ordinary black-hole loss, so it does not tie to
        // the retirement; the suspect-then-recover link is proved by the
        // P-fragment unit test in io_bridge/tests.rs.
        // The manager recovered, and the IDRs are the session's, the
        // abandoned IDR in flight (if the fall caught it) and nothing else.
        assert!(c.recoveries >= 1, "{ctx}: {c:?}");
        assert!(c.recovered + 1 >= c.recoveries, "{ctx}: {c:?}");
        assert_eq!(c.idr_session, 1, "{ctx}");
        assert!(c.idr_mtu_shrink <= c.mtu_retired_idr, "{ctx}: {c:?}");
        assert!(o.idr_made <= 3, "{ctx}: {} IDRs", o.idr_made);
    }
}
