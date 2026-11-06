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

/// [`refs`] under production's rate policy, and the padding and probe raise it
/// brings: the loss and reordering scenes run with it, since that is the stream
/// they are about.
fn refs_rated() -> H264Load {
    let mut l = refs();
    l.pcm_rate_policy = true;
    l
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
    run_pattern_with(seed, secs, pattern, refs_rated()).await
}

async fn run_pattern_with(
    seed: u64,
    secs: u64,
    pattern: Pattern,
    load: H264Load,
) -> (BrowserlessResult, H264Outcome) {
    run_on(
        seed,
        secs,
        link_20ms(),
        pattern,
        load,
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
        let (r, o) = run_pattern_with(seed, 6, Pattern::Clean, refs()).await;
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
        assert_rated_reorder(&ctx, seed, 8_000, &r, &o).await;
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
        assert_rated_reorder(&ctx, seed, 45_000, &r, &o).await;
    }
}

/// The reordering scenes run under the rate policy (and its padding and probe
/// raise). What that must not do: starve the stream (frames made at least 0.8 x
/// those of the same scene with the policy off), or turn the path's lateness
/// into more than it was (the loss path still judged P fragments lost, nothing
/// was dropped). Premise: the policy ran (a rate was told) and probed.
async fn assert_rated_reorder(
    ctx: &str,
    seed: u64,
    window_us: u64,
    r: &BrowserlessResult,
    o: &H264Outcome,
) {
    let (r_off, o_off) = run_pattern_with(seed, SECS, Pattern::Reorder { window_us }, refs()).await;
    assert_eq!(
        r_off.h264.stats.rate_target_bps, 0,
        "{ctx}: premise: off is off"
    );
    assert!(
        r.h264.stats.rate_target_bps > 0
            && (r.h264.stats.probe_padding_windows > 0 || r.h264.refs.probe_raise_windows > 0),
        "{ctx}: premise: the policy ran and probed"
    );
    assert_eq!(r.s2c_datagrams_dropped, 0, "{ctx}");
    assert!(r.h264.stats.verdicts_p_frag > 0, "{ctx}");
    assert!(
        o.made * 10 >= o_off.made * 8,
        "{ctx}: {} frames made with the policy, {} without",
        o.made,
        o_off.made
    );
    println!(
        "{ctx}: policy on made {} (verdicts {}, padding windows {}, raise windows {}), off made {}",
        o.made,
        r.h264.stats.verdicts_p_frag,
        r.h264.stats.probe_padding_windows,
        r.h264.refs.probe_raise_windows,
        o_off.made
    );
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

/// The share of the estimate the stream may spend (`h264_rate::RATE_SHARE`,
/// pinned by a lib test).
const RATE_SHARE: f64 = 0.8;

/// How far the encoder's rate may lag the estimate it follows: the policy
/// evaluates every 250 ms on a 250 ms EWMA, and the estimate it read may have
/// been half a second old.
const POLICY_LAG_US: u64 = 500_000;

/// Capacity-drop timelines of scene 6, `(name, link points in bytes/s, run
/// length in seconds, when the capacity drops in us)`. A is a drop from 6 to 2
/// Mbit/s on a settled stream; B rises 2 -> 6 Mbit/s and drops back to 2 1.5 s
/// later, the drop the old 2 s reopen lock would have held off.
type Timeline = (&'static str, Vec<(u64, u64)>, u64, u64);

fn scene6_timelines() -> [Timeline; 2] {
    [
        (
            "A",
            vec![(0, 750_000), (12_000_000, 250_000)],
            24,
            12_000_000,
        ),
        (
            "B",
            vec![(0, 250_000), (6_000_000, 750_000), (7_500_000, 250_000)],
            20,
            7_500_000,
        ),
    ]
}

/// What scene 6 reads off a run, from the encoder's own decisions.
struct DropRun {
    r: BrowserlessResult,
    /// Coded access units: `(scene time us, bytes, raised)`.
    aus: Vec<(u64, usize, bool)>,
    secs: u64,
    /// Frames the encoder gate skipped (injected, not made), per scene time.
    injected: Vec<u64>,
}

impl DropRun {
    async fn new(
        seed: u64,
        points: &[(u64, u64)],
        secs: u64,
        tweak: impl FnOnce(&mut H264Load),
    ) -> Self {
        let net = super::bottleneck_net(
            super::CapTimeline {
                points: points.to_vec(),
            },
            20_000,
            0,
            600,
        );
        let mut l = super::refs_cbr_load();
        tweak(&mut l);
        let (r, _) = run_on(
            seed,
            secs,
            net,
            Pattern::Clean,
            l,
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        let aus = r
            .h264_run()
            .expect("an H.264 scene")
            .truth
            .au_log
            .iter()
            .map(|a| (u64::from(a.timestamp_us), a.bytes, a.raised))
            .collect();
        let injected = r
            .injections
            .iter()
            .filter(|(_, heartbeat)| !heartbeat)
            .map(|(t, _)| *t)
            .collect();
        Self {
            r,
            aus,
            secs,
            injected,
        }
    }

    /// Bytes of the access units coded at the rate in force (not the probe
    /// raise) in `[from, from + 1 s)`.
    fn bytes_in(&self, from: u64) -> usize {
        self.aus
            .iter()
            .filter(|(t, _, raised)| !raised && (from..from + 1_000_000).contains(t))
            .map(|a| a.1)
            .sum()
    }

    /// The highest estimate in `[from - lag, from + 1 s)`.
    fn estimate_max(&self, from: u64) -> u64 {
        self.r
            .bwe_estimate_samples
            .iter()
            .filter(|(t, _)| *t + POLICY_LAG_US >= from && *t < from + 1_000_000)
            .map(|x| x.1)
            .max()
            .unwrap_or(0)
    }

    /// Windows `[t, t + 1 s)` every 100 ms from `from` to the end of the run.
    fn windows(&self, from: u64) -> impl Iterator<Item = u64> + '_ {
        (from..=self.secs * 1_000_000 - 1_000_000).step_by(100_000)
    }

    /// The worst ratio of coded bytes to `RATE_SHARE x` the estimate, over the
    /// windows from `from`.
    fn worst_ratio(&self, from: u64) -> (f64, u64) {
        self.windows(from)
            .map(|t| {
                let est = self.estimate_max(t).max(1) as f64;
                (self.bytes_in(t) as f64 * 8.0 / (RATE_SHARE * est), t)
            })
            .fold((0.0, 0), |a, b| if b.0 > a.0 { b } else { a })
    }

    fn skips_after(&self, from: u64, len: u64) -> usize {
        self.injected
            .iter()
            .filter(|t| (from..from + len).contains(*t))
            .count()
            .saturating_sub(
                self.aus
                    .iter()
                    .filter(|(t, _, _)| (from..from + len).contains(t))
                    .count(),
            )
    }

    /// The longest a frame took from its capture to the client showing it,
    /// for frames captured from `from`.
    fn max_latency_us(&self, from: u64) -> u64 {
        self.r
            .h264_run()
            .expect("an H.264 scene")
            .display
            .iter()
            .filter(|d| u64::from(d.frame_seq) * DEFAULT_CADENCE_US >= from)
            .map(|d| {
                d.at_us
                    .saturating_sub(u64::from(d.frame_seq) * DEFAULT_CADENCE_US)
            })
            .max()
            .unwrap_or(0)
    }
}

/// Scene 6, rate control on an acked-reference stream: a capacity drop.
///
/// The stream is `refs_cbr_load` (a PCM encoder that sizes its frames from the
/// rate it is told, content that wants more than any rate here) under the
/// in-place policy, on a link that falls from 6 to 2 Mbit/s (A), or rises
/// 2 -> 6 and falls back 1.5 s later (B: the drop shortly after a rise). The
/// assertions are about the encoder's decisions, not the wire (the pacer holds
/// the wire to the estimate whatever the encoder does):
///  * the access units coded at the rate in force (not the probe raise, which
///    is a burst by design) never exceed `1.15 x RATE_SHARE x` the estimate in
///    any 1 s window from one RTT after the drop, the estimate being the
///    highest in the window and the half second before it (the policy's lag);
///  * backpressure skips after the drop are at most half those of the control
///    (the same scene with no rate policy) in the 10 s after the drop, and at
///    most a third of the control's from 2 s to 10 s;
///  * the frame queue delay (capture to shown) stays within the link's buffer
///    and a second.
///
/// Premises: the link's fall is seen (the estimate fell by 30 %), the policy
/// moved the rate down (and up, on B) in place, the encoder gate skipped
/// frames for the queue, and the rate model held frames to the rate.
///
/// Mutations: no rate policy (`pcm_rate_policy` off) fails the premise and the
/// skip bound; Phase A's 2 s lock (`pcm_phase_a_rate`) and a soft cap at 1.7
/// (`with_cap_overshoot`) do not fail these windows, which do not reach the
/// cap or the lock (the lock is caught by `in_place_a_drop_shortly_after_a_rise_is_not_locked_out`
/// in the policy's tests).
#[tokio::test(start_paused = true)]
async fn refs_scene_6_a_capacity_drop_is_followed_by_the_encoder() {
    for (name, points, secs, drop_at) in scene6_timelines() {
        for seed in SEEDS {
            let run = DropRun::new(seed, &points, secs, |_| {}).await;
            let control = DropRun::new(seed, &points, secs, |l| l.pcm_rate_policy = false).await;
            let ctx = format!("scene6 {name} {seed:#x}");
            let refs = &run.r.h264.refs;
            let model = &run.r.h264_run().expect("an H.264 scene").truth.model;
            // Premises.
            let before = run
                .r
                .bwe_estimate_samples
                .iter()
                .filter(|(t, _)| *t < drop_at)
                .map(|x| x.1)
                .max()
                .unwrap_or(0);
            let after = run
                .r
                .bwe_estimate_samples
                .iter()
                .filter(|(t, _)| *t >= drop_at + 500_000)
                .map(|x| x.1)
                .min()
                .unwrap_or(u64::MAX);
            assert!(
                after as f64 <= 0.7 * before as f64,
                "{ctx}: premise: the estimate fell by 30 %: {before} -> {after}"
            );
            assert!(refs.rate_applies_down >= 1, "{ctx}: premise: {refs:?}");
            assert!(
                name == "A" || refs.rate_applies_up >= 1,
                "{ctx}: premise: B rises in place first"
            );
            assert!(
                run.r.h264.stats.frames_skipped_backpressure > 0,
                "{ctx}: premise: the gate skipped for the queue"
            );
            assert!(
                model.frames_bucket_empty > 0,
                "{ctx}: premise: the model held frames to the rate"
            );
            // The encoder follows the estimate.
            let (ratio, at) = run.worst_ratio(drop_at + RTT_US);
            // Skips.
            let skips = run.skips_after(drop_at, 10_000_000);
            let control_skips = control.skips_after(drop_at, 10_000_000);
            let latency = run.max_latency_us(drop_at + RTT_US);
            println!(
                "{ctx}: worst coded/(0.8 est) {ratio:.2} at {:+.1} s; skips in 10 s {skips} (control {control_skips}); max latency {} ms; applies up {} down {}",
                (at as f64 - drop_at as f64) / 1e6,
                latency / 1000,
                refs.rate_applies_up,
                refs.rate_applies_down
            );
            assert!(
                ratio <= 1.15,
                "{ctx}: coded {ratio:.2} x {RATE_SHARE} x the estimate in the 1 s from {at}"
            );
            assert!(
                skips * 2 <= control_skips,
                "{ctx}: {skips} skips in 10 s against the control's {control_skips}"
            );
            let settled = run.skips_after(drop_at + 2_000_000, 8_000_000);
            let control_settled = control.skips_after(drop_at + 2_000_000, 8_000_000);
            assert!(
                settled * 3 <= control_settled,
                "{ctx}: {settled} skips from 2 s to 10 s after the drop against the control's {control_settled}"
            );
            assert!(
                latency <= 600_000 + 1_000_000,
                "{ctx}: a frame took {latency} us to be shown"
            );
        }
    }
}

/// Scene 6C, the drop right after a spike: the link rises 2 -> 6 Mbit/s at 6 s
/// and falls back to 2 Mbit/s 600 ms after the probe's spike, while the policy still holds
/// the probe's spike (the 2 s hold of `SPIKE_HOLD_IN_PLACE`). A hold that
/// cannot be let go rides the estimate's fall out and keeps the stream at the
/// spike's rate for the rest of the hold. On the real stack (review 2,
/// 2026-11-06): 42 KB over the link in the 3 s after the drop, a latency near
/// 700 ms and 116 of the 150 frames of the 5 s shown, against nothing over,
/// about 270 ms and 142 for Phase A (scene 6 A and B do not see it: A drops
/// from a settled 6 Mbit/s and B falls 1.5 s after its rise). The twin's spike lands at 7.4 s on every
/// seed (the first estimate over 3 Mbit/s after the rise), so the drop is
/// placed 600 ms after it, found by a first run without a drop.
///
/// Asserted over three seeds, from the encoder's own access units and the
/// client's display: the bytes coded over the link's rate in the 3 s after the
/// drop, the frames shown in the 5 s after it, and the longest capture-to-shown
/// time. Premises: the drop is seen, the spike was taken (the policy applied
/// the rise) and the encoder gate skipped frames.
///
/// Mutation, asserted: `spike_tune: Some((2000, false))`, the hold that is never
/// let go early, must fail two of the bounds: more than `SCENE6C_OVER_BYTES`
/// over the link summed over the seeds (144 kB against the shipped 98 kB), and
/// fewer than `SCENE6C_SHOWN` frames shown on at least two seeds (114, 115 of
/// three; the third, 140, passes). It does not fail the latency bound.
#[tokio::test(start_paused = true)]
async fn refs_scene_6c_a_drop_right_after_a_spike_is_followed_early() {
    let (mut over, mut fixed_over, mut fixed_short) = (0, 0, 0);
    for seed in SEEDS {
        let ctx = format!("scene6c {seed:#x}");
        let (shipped, fixed) = (
            scene6c(seed, None).await,
            scene6c(seed, Some((2_000, false))).await,
        );
        println!("{ctx}: shipped {shipped:?}; never-released 2 s hold {fixed:?}");
        assert!(shipped.premise_ok, "{ctx}: premise: {shipped:?}");
        assert!(fixed.premise_ok, "{ctx}: premise (mutation): {fixed:?}");
        over += shipped.over_bytes;
        fixed_over += fixed.over_bytes;
        fixed_short += usize::from(fixed.shown < SCENE6C_SHOWN);
        assert!(
            shipped.shown >= SCENE6C_SHOWN,
            "{ctx}: {} of 150 frames shown in the 5 s after the drop (bound {SCENE6C_SHOWN})",
            shipped.shown
        );
        assert!(
            shipped.latency_us <= SCENE6C_LATENCY_US,
            "{ctx}: a frame took {} us to be shown (bound {SCENE6C_LATENCY_US})",
            shipped.latency_us
        );
    }
    assert!(
        over <= SCENE6C_OVER_BYTES,
        "{over} bytes over the link in the 3 s after the drops (bound {SCENE6C_OVER_BYTES})"
    );
    // The mutation is load-bearing: the hold that is never let go breaks the
    // bounds the shipped hold keeps.
    assert!(
        fixed_over > SCENE6C_OVER_BYTES,
        "the never-released hold put {fixed_over} bytes over the link: not over the bound {SCENE6C_OVER_BYTES}, so the bound does not discriminate"
    );
    assert!(
        fixed_short >= 2,
        "the never-released hold showed fewer than {SCENE6C_SHOWN} frames on {fixed_short} seeds, need 2"
    );
}

const SCENE6C_OVER_BYTES: u64 = 120_000;
/// Per seed: shipped 129, 146, 138 frames; never let go 114, 140, 115.
/// Over the link, summed over the seeds: shipped 98 kB, never let go 144 kB,
/// bound `SCENE6C_OVER_BYTES`.
const SCENE6C_SHOWN: usize = 125;
/// The longest capture-to-shown time. Not a discriminator in the twin (both
/// holds reach 410 to 500 ms); a regression guard against the 689 ms review 2
/// measured on the real stack.
const SCENE6C_LATENCY_US: u64 = 700_000;

#[derive(Debug)]
struct Scene6c {
    premise_ok: bool,
    over_bytes: u64,
    shown: usize,
    latency_us: u64,
}

async fn scene6c(seed: u64, spike_tune: Option<(u64, bool)>) -> Scene6c {
    const RISE_AT: u64 = 6_000_000;
    // The spike is the probe's result: the first estimate over 3 Mbit/s after
    // the rise. Which second it lands in depends on the seed, so a first run
    // without a drop finds it, and the drop comes 600 ms after it (inside the
    // 2 s hold; a run up to the drop is identical to the first).
    let rise = vec![(0, 250_000), (RISE_AT, 750_000)];
    let first = DropRun::new(seed, &rise, 20, |l| l.spike_tune = spike_tune).await;
    let spike_at = first
        .r
        .bwe_estimate_samples
        .iter()
        .find(|(t, e)| *t >= RISE_AT && *e >= 3_000_000)
        .map(|x| x.0);
    let Some(spike_at) = spike_at else {
        return Scene6c {
            premise_ok: false,
            over_bytes: 0,
            shown: 0,
            latency_us: 0,
        };
    };
    let drop_at = (spike_at + 600_000) / 100_000 * 100_000;
    let points = vec![(0, 250_000), (RISE_AT, 750_000), (drop_at, 250_000)];
    let run = DropRun::new(seed, &points, 20, |l| l.spike_tune = spike_tune).await;
    let refs = &run.r.h264.refs;
    // Bytes coded over the link's 2 Mbit/s (25 kB per 100 ms) in the 3 s after
    // the drop, summed over the 100 ms windows that exceed it.
    let over_bytes: u64 = (0..30)
        .map(|k| {
            let from = drop_at + k * 100_000;
            let coded: u64 = run
                .aus
                .iter()
                .filter(|(t, _, _)| (from..from + 100_000).contains(t))
                .map(|a| a.1 as u64)
                .sum();
            coded.saturating_sub(25_000)
        })
        .sum();
    let shown = run
        .r
        .h264_run()
        .expect("an H.264 scene")
        .display
        .iter()
        .filter(|d| {
            (drop_at..drop_at + 5_000_000).contains(&(u64::from(d.frame_seq) * DEFAULT_CADENCE_US))
        })
        .count();
    println!(
        "scene6c {seed:#x} tune {spike_tune:?}: spike at {spike_at}, drop at {drop_at}, applies up {} down {}",
        refs.rate_applies_up, refs.rate_applies_down
    );
    Scene6c {
        premise_ok: refs.rate_applies_up >= 1 && spike_at < drop_at,
        over_bytes,
        shown,
        latency_us: run.max_latency_us(drop_at),
    }
}
