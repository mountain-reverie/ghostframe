//! T3 scenes with real pictures (design 10, B3), on the Phase A policy.
//!
//! What runs: the real `IoBridge` send path, ledger and loss path; the real
//! `ClientNet`/`ClientCore`; and a PCM-patch source in place of the encoder, so
//! the access units are real H.264 that libavcodec decodes on the client. Every
//! picture carries markers, so the client side can say which picture it
//! displayed and compare it with the one the encoder meant (see
//! `ghostframe_e2e::harness::h264_scene`).
//!
//! The scenes below pin the **legacy policy** (GOP and loss IDR, each P from
//! the frame before it) as the baseline B5 is measured against. They assert
//! what is true of it today, and most of what is true is a cost, not a defect:
//! the client core withholds a P frame whose reference never arrived, so no
//! wrong picture is displayed, and what the policy pays is IDRs and freezes.
//! Assertions that say "the legacy policy freezes" or "makes an IDR per loss"
//! are meant to be inverted by B5, which replaces the policy.
//!
//! Every scene runs three seeds and states its premise (the loss fired, the
//! reorder happened) before the effect.
use super::*;
use ghostframe_e2e::harness::h264_scene::{capacity_step_net, pcm_load, H264Outcome, Pattern};
use ghostframe_lib::encoder::h264_harness::PcmMode;

const SEEDS: [u64; 3] = [0x7101, 0x7102, 0x7103];
const SECS: u64 = 10;

fn legacy() -> H264Load {
    pcm_load(PcmMode::Legacy)
}

async fn pcm_run_on(
    seed: u64,
    secs: u64,
    base: NetProfile,
    pattern: Pattern,
    load: H264Load,
    drops: DropPlan,
) -> (BrowserlessResult, H264Outcome) {
    let (net, links) = pattern.apply(base);
    let r = run_browserless_with_options(
        scene(seed, secs, net, load, drops),
        links,
        RunOptions::default(),
    )
    .await
    .expect("scene");
    let o = r.h264_run().map(|x| x.analyse()).unwrap_or_default();
    (r, o)
}

async fn pcm_run(
    seed: u64,
    secs: u64,
    pattern: Pattern,
    load: H264Load,
) -> (BrowserlessResult, H264Outcome) {
    pcm_run_on(seed, secs, link_20ms(), pattern, load, DropPlan::default()).await
}

fn line(name: &str, seed: u64, r: &BrowserlessResult, o: &H264Outcome) {
    let s = &r.h264.stats;
    println!(
        "{name} {seed:#x}: {} | idr: session={} loss={} reentry={} reset={} hold_to={} | \
         verdicts p={} idr={} | drops={}/{} skipped hold={} bp={}",
        o.summary(),
        s.idr_by_cause[IdrCause::Session.index()],
        s.idr_by_cause[IdrCause::Loss.index()],
        s.idr_by_cause[IdrCause::Reentry.index()],
        s.idr_by_cause[IdrCause::ClientReset.index()],
        s.idr_by_cause[IdrCause::HoldTimeout.index()],
        s.verdicts_p_frag,
        s.verdicts_idr_frag,
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_offered,
        s.frames_skipped_hold,
        s.frames_skipped_backpressure,
    );
}

/// What holds in every scene on the legacy policy, lossy or not. These are
/// the properties that say the measurement is sound: the two IDR counters
/// (the bridge's, from the transport, and the source's, from the stream it
/// wrote) agree, every delivered frame is one the source made, its header
/// names the reference the encoder meant, and nothing the client decoded was
/// a wrong picture or made the decoder fail.
fn assert_sound(ctx: &str, r: &BrowserlessResult, o: &H264Outcome) {
    assert_eq!(
        o.idr_made as u64, r.h264.stats.frames_idr,
        "{ctx}: the source and the bridge disagree on the IDR count"
    );
    assert_eq!(o.unknown_to_truth, 0, "{ctx}");
    assert_eq!(o.header_ref_mismatches, 0, "{ctx}");
    assert!(o.fed > 0 && o.correct > 0, "{ctx}: nothing was displayed");
    assert!(
        o.wrong.is_empty(),
        "{ctx}: wrong pictures displayed: {}",
        o.wrong_list(8)
    );
    assert_eq!(o.rejected, 0, "{ctx}: the decoder failed");
    assert_eq!(o.no_picture, 0, "{ctx}: the decoder produced no picture");
    assert_eq!(o.decoder_resets, 0, "{ctx}");
    assert_eq!(o.damaged, 0, "{ctx}");
}

/// A lossy path on the legacy policy. The loss IDR rate limit holds (one a
/// second), every IDR after the first is a loss IDR, and the stream is
/// alive at the end. The price, which B5 removes: each loss costs an IDR and
/// a freeze.
fn assert_legacy_under_loss(ctx: &str, r: &BrowserlessResult, o: &H264Outcome) {
    let s = &r.h264.stats;
    // Premise: the path dropped frame datagrams and the loss path judged some
    // of them lost, on a P frame (an IDR fragment is resent, not a verdict).
    assert!(r.s2c_datagrams_dropped > 0, "{ctx}: nothing was dropped");
    assert!(
        s.verdicts_p_frag > 0,
        "{ctx}: no P fragment was judged lost"
    );
    assert_sound(ctx, r, o);
    assert_eq!(s.idr_by_cause[IdrCause::Session.index()], 1, "{ctx}");
    assert!(s.idr_by_cause[IdrCause::Loss.index()] >= 1, "{ctx}");
    assert!(
        o.idr_made as u64 <= 1 + SECS,
        "{ctx}: {} IDRs in {SECS} s breaks the loss IDR rate limit",
        o.idr_made
    );
    assert!(o.freezes >= 1, "{ctx}: a loss cost no freeze");
    assert!(
        o.last_correct_us.unwrap_or(0) + 3_000_000 >= SECS * 1_000_000,
        "{ctx}: the stream was dead at the end: {:?}",
        o.last_correct_us
    );
}

/// A clean stream: one IDR, every frame shown and right, no freeze.
#[tokio::test(start_paused = true)]
async fn a_clean_stream_shows_every_frame_right_with_one_idr() {
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, SECS, Pattern::Clean, legacy()).await;
        line("clean", seed, &r, &o);
        let ctx = format!("clean {seed:#x}");
        assert_sound(&ctx, &r, &o);
        // Premise: a real stream was decoded (not a handful of frames).
        assert!(o.correct > 200, "{ctx}: {} frames", o.correct);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert_eq!(o.correct, o.fed, "{ctx}");
        assert_eq!(o.freezes, 0, "{ctx}: max gap {} us", o.max_gap_us);
    }
}

/// Scene 1: independent loss, 1 % and 5 %.
#[tokio::test(start_paused = true)]
async fn scene_1_iid_loss_costs_idrs_and_freezes_but_shows_no_wrong_picture() {
    for (p, name) in [(0.01, "iid1"), (0.05, "iid5")] {
        for seed in SEEDS {
            let (r, o) = pcm_run(seed, SECS, Pattern::Iid(p), legacy()).await;
            line(name, seed, &r, &o);
            assert_legacy_under_loss(&format!("{name} {seed:#x}"), &r, &o);
        }
    }
}

/// Scene 2: bursts. Gilbert-Elliott, about 4 datagrams a burst at 80 % loss.
#[tokio::test(start_paused = true)]
async fn scene_2_burst_loss_costs_idrs_and_freezes_but_shows_no_wrong_picture() {
    let burst = Pattern::Burst {
        enter: 0.01,
        exit: 0.3,
        loss: 0.8,
    };
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, SECS, burst, legacy()).await;
        line("burst", seed, &r, &o);
        assert_legacy_under_loss(&format!("burst {seed:#x}"), &r, &o);
    }
}

/// The acknowledgement path alone loses datagrams (the frame ack, 0x0A, does
/// not exist yet; today these are the per-datagram 0x06 batches). Nothing the
/// server sent was lost, yet the legacy policy reads the missing
/// acknowledgements as loss and makes IDRs for them.
#[tokio::test(start_paused = true)]
async fn lost_acknowledgements_alone_cost_the_legacy_policy_idrs() {
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, SECS, Pattern::AckLoss(0.3), legacy()).await;
        line("ackloss", seed, &r, &o);
        let ctx = format!("ackloss {seed:#x}");
        // Premise: only the acknowledgement direction dropped anything.
        assert_eq!(r.s2c_datagrams_dropped, 0, "{ctx}");
        assert!(r.bytes_dropped > 0, "{ctx}: no acknowledgement was lost");
        assert_sound(&ctx, &r, &o);
        assert!(
            r.h264.stats.verdicts_p_frag > 0,
            "{ctx}: no verdict on a frame that arrived"
        );
        assert!(o.idr_made >= 2, "{ctx}: no IDR for the lost acks");
    }
}

/// Scene 4: reordering, benign and hostile.
///
/// Within 8 ms (a quarter of the frame period) datagrams swap places inside a
/// frame and nothing is lost: one IDR. At 45 ms, longer than a frame period,
/// the loss path's time rule reads late datagrams as lost, resends IDR
/// fragments, and makes loss IDRs; on some seeds it stalls for seconds. Either
/// way the client displays no wrong picture.
#[tokio::test(start_paused = true)]
async fn scene_4_reordering_shows_no_wrong_picture() {
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, SECS, Pattern::Reorder { window_us: 8_000 }, legacy()).await;
        line("reorder8", seed, &r, &o);
        let ctx = format!("reorder8 {seed:#x}");
        assert_sound(&ctx, &r, &o);
        assert!(o.idr_made <= 3, "{ctx}: {} IDRs", o.idr_made);
        assert!(o.correct > 200, "{ctx}");
    }
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, SECS, Pattern::Reorder { window_us: 45_000 }, legacy()).await;
        line("reorder45", seed, &r, &o);
        let ctx = format!("reorder45 {seed:#x}");
        // Premise: the path really reordered (one-way transit spread over
        // half the window), and the loss path took it for loss.
        let (lo, hi) = r
            .s2c_transit_samples
            .iter()
            .fold((u64::MAX, 0), |(lo, hi), &(_, d)| (lo.min(d), hi.max(d)));
        assert!(hi - lo >= 22_000, "{ctx}: transit {lo}..{hi} us");
        assert_eq!(
            r.s2c_datagrams_dropped, 0,
            "{ctx}: reordering drops nothing"
        );
        let s = &r.h264.stats;
        assert!(
            s.verdicts_p_frag + s.verdicts_idr_frag > 0,
            "{ctx}: the loss path never mistook a late datagram for a lost one"
        );
        assert_sound(&ctx, &r, &o);
        // A known-bad ceiling, not a target. Reordering and jitter make the
        // loss path declare loss with zero real drops (premise above), and on
        // seeds 1 and 3 the stream is nearly dead for the whole scene (6 to 8
        // frames made, ~9.1 to 9.3 s frozen). That is the loss path's
        // intolerance of reordering, tracked as a follow-up; the bounds only
        // make a worse regression fail.
        assert!(o.made >= 4, "{ctx}: only {} frames made", o.made);
        assert!(o.frozen_us <= 9_500_000, "{ctx}: frozen {} us", o.frozen_us);
    }
}

/// Scene 5: H.264 -> tile -> H.264 -> tile -> H.264. Legacy policy: each return
/// is an IDR (session 1 + reentry 2); B5's is a recovery P from the anchor
/// (IDR 1). The tile stretches paint the screen (`TileReady`), nothing of the
/// abandoned H.264 frames is shown wrong, and the stream resumes.
#[tokio::test(start_paused = true)]
async fn scene_5_tile_h264_transitions_cost_an_idr_per_return() {
    for seed in SEEDS {
        let mut l = legacy();
        l.tile_phases = [Some((90, 150)), Some((210, 250))];
        let (r, o) = pcm_run(seed, SECS, Pattern::Clean, l).await;
        line("tiles", seed, &r, &o);
        let ctx = format!("tiles {seed:#x}");
        // Premise: the tile stretches happened (100 tile frames, one tile each)
        // and the stream was in H.264 either side of each.
        let tiles = r
            .events
            .iter()
            .filter(|e| matches!(e, ClientNetEvent::Core(CoreEvent::TileReady { .. })))
            .count();
        assert!(tiles >= 90, "{ctx}: {tiles} tile paints");
        assert_sound(&ctx, &r, &o);
        let s = &r.h264.stats;
        assert_eq!(s.idr_by_cause[IdrCause::Session.index()], 1, "{ctx}");
        assert_eq!(s.idr_by_cause[IdrCause::Reentry.index()], 2, "{ctx}");
        assert_eq!(o.idr_made, 3, "{ctx}");
        // Each return resumes with a correct picture within a second (the
        // re-entry IDR is 35 datagrams on a ~1 Mbit/s estimate, and the P
        // frames wait for it; seen 0.3 to 0.6 s).
        println!("   resume_us={:?}", o.resume_us);
        assert_eq!(o.resume_us.len(), 2, "{ctx}");
        for (i, t) in o.resume_us.iter().enumerate() {
            let t = t.unwrap_or_else(|| panic!("{ctx}: no picture after tile stretch {i}"));
            assert!(t <= 1_000_000, "{ctx}: stretch {i} resumed after {t} us");
        }
    }
}

/// Scene 7: lost IDR. Its fragments are resent by the loss path, the P frames
/// wait for it, and the client's first picture is the IDR, right.
#[tokio::test(start_paused = true)]
async fn scene_7_a_lost_idr_is_resent_and_p_frames_wait_for_it() {
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
        let (r, o) = pcm_run_on(seed, 6, net, Pattern::Clean, legacy(), drops).await;
        line("lostidr", seed, &r, &o);
        let ctx = format!("lostidr {seed:#x}");
        let s = &r.h264.stats;
        // Premise: every rule fired (the index-3 rule twice, the resend was
        // lost too), the loss path resent, and P frames were held.
        assert_eq!(r.frame_drops_fired, vec![2, 1, 1], "{ctx}");
        assert!(r.emitter_stats.h264_idr_resends >= 3, "{ctx}");
        assert!(s.frames_skipped_hold >= 1, "{ctx}: nothing was held");
        assert_sound(&ctx, &r, &o);
        assert_eq!(o.idr_made, 1, "{ctx}: the IDR was repaired, not replaced");
        assert_eq!(s.hold_timeouts, 0, "{ctx}");
        let run = r.h264_run().unwrap();
        assert_eq!(
            run.display.first().map(|d| d.frame_seq),
            Some(0),
            "{ctx}: the IDR is the first thing the client decoded"
        );
        // Decoded within 1 s of the scene's first frame (the session itself
        // takes 313 ms; the repair adds 90 to 270 ms).
        assert!(
            o.first_correct_us.is_some_and(|t| t <= 1_000_000),
            "{ctx}: {:?}",
            o.first_correct_us
        );
        assert!(o.correct > 100, "{ctx}");
    }
}

/// A capacity step down, with frames of fixed size (no rate control; that is
/// B6): the bottleneck buffer fills and the encoder gate skips frames for the
/// queue. The pictures that do get through are right and no IDR is needed.
#[tokio::test(start_paused = true)]
async fn a_capacity_step_down_skips_frames_but_shows_no_wrong_picture() {
    for seed in SEEDS {
        // 500 kB/s, then 60 kB/s from 4 s; the stream offers about 90 kB/s.
        let net = capacity_step_net(500_000, 4_000_000, 60_000, 20_000, 300);
        let (r, o) = pcm_run_on(
            seed,
            SECS,
            net,
            Pattern::Clean,
            legacy(),
            DropPlan::default(),
        )
        .await;
        line("capstep", seed, &r, &o);
        let ctx = format!("capstep {seed:#x}");
        // Premise: the link was overdriven, the gate skipped for the queue.
        assert!(
            r.h264.stats.frames_skipped_backpressure >= 20,
            "{ctx}: {} skips",
            r.h264.stats.frames_skipped_backpressure
        );
        assert_sound(&ctx, &r, &o);
        assert!(o.never_fed < o.made / 4, "{ctx}");
    }
}

/// The client-side check discriminates: with a deliberate defect in the PCM
/// source (every 7th frame written as a non-reference picture while the truth
/// log counts it as the reference of the next), a clean path shows wrong
/// pictures, and the check names the reference the decoder really used. The
/// control, the same scene without the defect, shows none.
#[tokio::test(start_paused = true)]
async fn a_wrong_reference_in_the_stream_is_a_wrong_picture_on_the_client() {
    for seed in SEEDS {
        let (r, control) = pcm_run(seed, 4, Pattern::Clean, legacy()).await;
        assert_sound(&format!("control {seed:#x}"), &r, &control);
        let mut l = legacy();
        l.pcm_wrong_ref_every = 7;
        let (r, o) = pcm_run(seed, 4, Pattern::Clean, l).await;
        line("mutant", seed, &r, &o);
        assert!(o.fed > 100 && r.s2c_datagrams_dropped == 0, "premise");
        assert!(
            o.wrong.len() > o.fed / 2,
            "{seed:#x}: only {} of {} pictures were seen to be wrong",
            o.wrong.len(),
            o.fed
        );
        let first = &o.wrong[0];
        assert_eq!(first.frame_seq, 8, "the frame after the first defect");
        assert_ne!(first.effective_ref, first.meant_ref);
        assert_eq!(first.effective_ref, Some(0), "it predicted from the IDR");
    }
}

/// Same seed, same behaviour: two runs of a lossy scene agree on everything
/// the scenes read (the transport's counters, what each frame was meant to
/// show, every delivery and its instant, what the client decoded), and another
/// seed does not (so the agreement is not an empty scene).
///
/// The wire digest is held to the same standard only on a link with no netsim
/// randomness (the loss is the harness's own, `DropPlan`). With the netsim's
/// i.i.d. loss, or with jitter, the server's handshake flight differs in
/// length by a few bytes between two same-seed runs (seed 0x7201, 5 % loss:
/// datagram 13, 180 ms in, lengths 709 to 714), with no difference in
/// behaviour: every counter, delivery and instant above is identical. The
/// cause is the server's TLS identity: a fresh ECDSA P-256 certificate per run
/// (`rcgen::KeyPair::generate` in `transport/quic.rs`) and ECDSA signatures
/// with a random nonce, whose DER encoding is 70 to 72 bytes. The Handshake
/// packets that carry them are not padded, so the length shows. A fixed key
/// pair would not remove it (the signature stays random), so it is only
/// documented; the scripted filler scenes show it equally.
#[tokio::test(start_paused = true)]
async fn the_same_seed_gives_byte_identical_counters() {
    let one = |seed| async move {
        let (r, o) = pcm_run(seed, 8, Pattern::Iid(0.05), legacy()).await;
        let run = r.h264_run().unwrap().clone();
        (
            format!("{:?}", r.h264.stats),
            format!("{:?}", run.truth),
            format!("{:?}", run.display),
            format!("{o:?}"),
            format!("{:?}", r.emitter_stats),
            r.s2c_datagrams_dropped,
            r.bytes_delivered_c2s,
            r.wire_datagrams,
        )
    };
    let a = one(0x7201).await;
    let b = one(0x7201).await;
    assert!(
        a.5 > 0 && a.2.len() > 1000,
        "premise: loss fired, frames decoded"
    );
    assert_eq!(a, b, "same seed, different counters");
    let c = one(0x7202).await;
    assert_ne!(a.2, c.2, "another seed must give another run");

    // The wire itself, with the harness's own loss (scene 7's).
    let wire = |seed| async move {
        let drops = DropPlan::default().with_frame_rules(vec![
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Index(3),
                occurrences: vec![0, 1],
            },
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Last,
                occurrences: vec![0],
            },
        ]);
        let (r, _) = pcm_run_on(seed, 4, link_20ms(), Pattern::Clean, legacy(), drops).await;
        assert_eq!(r.frame_drops_fired, vec![2, 1], "premise");
        (
            r.wire_digest,
            r.wire_datagrams,
            r.bytes_delivered_s2c,
            format!("{:?}", r.h264_run().unwrap().display),
        )
    };
    let (a, b) = (wire(0x7203).await, wire(0x7203).await);
    assert_eq!(a, b, "same seed, different wire");
    assert_ne!(
        a.0,
        wire(0x7204).await.0,
        "another seed must give another wire"
    );
}

/// The `RefManager` source, end to end through the harness with no loss: its
/// plans are written as real streams behind the real send path, the client
/// decodes every one to the picture the plan named, headers carry the plan's
/// reference, and the manager did what it does when healthy (IDR once, marks
/// candidates, confirms them from the acknowledgements, never recovers).
#[tokio::test(start_paused = true)]
async fn the_ref_manager_source_runs_end_to_end_with_no_loss() {
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, 6, Pattern::Clean, pcm_load(PcmMode::RefManager)).await;
        line("refm", seed, &r, &o);
        let ctx = format!("refm {seed:#x}");
        assert_sound(&ctx, &r, &o);
        assert!(o.correct > 100, "{ctx}");
        assert_eq!(o.correct, o.fed, "{ctx}");
        assert_eq!(o.idr_made, 1, "{ctx}");
        let c = o.refs;
        // Premise: the manager planned (not the legacy path), marked
        // candidates, and heard the acknowledgements that confirm them.
        assert_eq!(c.idr_session, 1, "{ctx}");
        assert!(c.p_healthy as usize + 1 >= o.made, "{ctx}: {c:?}");
        assert!(
            c.candidates_marked > 0 && c.anchors_confirmed > 0,
            "{ctx}: {c:?}"
        );
        assert_eq!(
            (c.recoveries, c.p_recovery, c.plan_mismatch, c.idr_reissued),
            (0, 0, 0, 0),
            "{ctx}: {c:?}"
        );
        // The wire carries the plan: marked frames say so.
        let marked = r
            .events
            .iter()
            .filter(|e| {
                matches!(e, ClientNetEvent::Core(CoreEvent::NeedsH264 { flags, .. })
                    if flags & ghostframe_protocol::protocol::FRAME_FLAG_MARK_LT != 0)
            })
            .count();
        assert!(marked as u64 >= c.candidates_marked, "{ctx}: {marked}");
    }
}

/// The `RefManager` source under loss. Its picture of the client must come
/// from what the client decoded (the modelled 0x0A), not from what the
/// transport completed: a P frame that arrives whole behind a lost one proves
/// nothing about the lost one, and `RefManager::frame_decoded` would mark the
/// whole chain behind it decoded. At 5 % some P frames are lost beyond what
/// the XOR parity repairs (premise: frames never reached the client). The
/// manager must then recover from its anchor and stop chaining from frames
/// the client never displayed; with the transport-complete stand-in it kept
/// planning healthy frames off the lost one to the end of the scene
/// (`refs_to_undisplayed` 262 of 272 with it, against a handful without).
///
/// With the legacy ClientCore this scene froze for about 9 s after the first
/// real loss: it held back every P frame whose `ref_frame_seq` was not the
/// newest it had delivered, and a recovery P names the anchor. With the
/// client gate (design 7.1) the stream recovers: no freeze, no wrong picture.
#[tokio::test(start_paused = true)]
async fn the_ref_manager_source_recovers_from_a_real_loss_not_a_transport_completion() {
    for seed in SEEDS {
        let (r, o) = pcm_run(seed, 10, Pattern::Iid(0.05), pcm_load(PcmMode::RefManager)).await;
        line("refm-loss", seed, &r, &o);
        let ctx = format!("refm-loss {seed:#x}");
        let run = r.h264_run().unwrap();
        let c = o.refs;
        println!("   {c:?} acks={}", run.truth.refs.frame_acks_applied);
        // Premise: frames were really lost (not repaired by parity), the loss
        // path judged P fragments lost, and the client's acks reached the
        // manager.
        assert!(r.s2c_datagrams_dropped > 0, "{ctx}");
        assert!(r.h264.stats.verdicts_p_frag > 0, "{ctx}");
        assert!(o.never_fed > 0, "{ctx}: no frame was lost for good");
        assert!(run.truth.refs.frame_acks_applied > 0, "{ctx}");
        assert_eq!(o.unknown_to_truth, 0, "{ctx}");
        assert_eq!(o.header_ref_mismatches, 0, "{ctx}");
        assert!(o.wrong.is_empty(), "{ctx}: {}", o.wrong_list(8));
        assert_eq!(o.idr_made, 1, "{ctx}: the manager makes no loss IDR");
        // Effect: it noticed, and went to its anchor.
        assert!(c.recoveries >= 1 && c.p_recovery > 0, "{ctx}: {c:?}");
        assert_eq!(c.plan_mismatch, 0, "{ctx}");
        assert!(
            o.refs_to_undisplayed < o.made / 4,
            "{ctx}: {} of {} frames chained from a frame the client never showed",
            o.refs_to_undisplayed,
            o.made
        );
        // The client gate (design 7.1) is the real one now: a recovery P that
        // names the anchor is fed, not held back, so the stream recovers where
        // the legacy client froze for about 9 s. Everything the gate feeds
        // decodes to the picture the plan named, no long frozen stretch, and the
        // manager's recoveries all land (the last may be in flight).
        assert_eq!(o.correct, o.fed, "{ctx}");
        assert_eq!((o.rejected, o.no_picture, o.damaged), (0, 0, 0), "{ctx}");
        // A freeze here is a gap over four frame periods. The stream carries no
        // parity now (an acked-reference stream repairs from its anchor), so
        // every lost frame is a recovery, not a repair: bounded, not rare.
        assert!(
            o.frozen_us < 600_000 && o.freezes <= 16,
            "{ctx}: {} freezes, {} us",
            o.freezes,
            o.frozen_us
        );
        assert!(o.max_gap_us < 400_000, "{ctx}: gap {} us", o.max_gap_us);
        assert!(c.recovered + 1 >= c.recoveries, "{ctx}: {c:?}");
        assert!(
            o.last_correct_us.unwrap_or(0) + 1_000_000 >= 10_000_000,
            "{ctx}: dead at the end"
        );
    }
}
