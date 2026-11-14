// SPDX-License-Identifier: AGPL-3.0-or-later
//! T3 scenes with the real openh264 library as the encoder (design 10, B7-4).
//!
//! The same client, links and seeds as `refs_scenes`, but the access units are
//! made by libopenh264 itself, in observed mode: the library keeps its own
//! reference structure (an LTR cycle it marks and recovers on feedback), the
//! `RefManager` reads what each access unit says and feeds the receiver's word
//! back (`LtrMarked`, `LtrMarkFailed`, `LtrRecover`). The picture is a canvas
//! of flat levels (`encoder::h264_harness::canvas_*`); the client's picture is
//! read back by nearest level, so a cell that drifted shows as a wrong picture
//! and a lossy codec does not. Scenes that need explicit references (the
//! slot-overwrite race, the non-reference probe P) stay in `refs_scenes`.
//!
//! Every scene runs three seeds and states its premises before its effect: the
//! cells were read (none ambiguous, none read far from its level), the loss
//! fired, the library was asked for recoveries and made them. The library must
//! be installed (`libopenh264-7` on Debian/Ubuntu) or named by
//! `GHOSTFRAME_H264_OPENH264_LIB`, which the bridge and these scenes read the
//! way the daemon does (`just test-openh264 <path>` runs them against one); a
//! missing one fails the scene with that message.
//!
//! The oracle is the comparison of the picture the client showed with the
//! canvas the source painted (`wrong`). The truth log's reference of a frame is
//! the manager's own reading of the stream, so a manager that misread would
//! agree with itself: `header_ref_mismatches` is not asserted here, and
//! `refs_to_undisplayed` measures how stale the encoder's picture of the
//! client is, not whether the stream is right.
use super::refs_scenes::{run_on, DropRun, RTT_US, SECS, SEEDS, VERDICT_US};
use super::*;
use ghostframe_e2e::harness::h264_scene::{pcm_load, H264Outcome, Pattern, FREEZE_GAP_FRAMES};
use ghostframe_lib::encoder::h264_harness::{openh264_library, openh264_rate_refusals, PcmMode};

fn oh(scene_change_every: u32) -> H264Load {
    oh_canvas(scene_change_every, 0, 0)
}

fn oh_canvas(scene_change_every: u32, churn_every: u32, stale_every: u32) -> H264Load {
    let mut l = pcm_load(PcmMode::OpenH264 {
        scene_change_every,
        churn_every,
        stale_every,
    });
    // Production's rate policy, padding and probe raise: the stream the loss
    // scenes are about.
    l.pcm_rate_policy = true;
    l
}

fn require_lib() {
    match openh264_library() {
        Ok(which) => println!("openh264 library: {which}"),
        Err(e) => panic!(
            "{e}\nthese scenes need libopenh264 2.4.x-2.6.x: Debian/Ubuntu `apt install \
             libopenh264-7`, Fedora the Cisco repository (or set GHOSTFRAME_H264_OPENH264_LIB \
             to its absolute path)"
        ),
    }
}

async fn run_oh(
    seed: u64,
    secs: u64,
    net: NetProfile,
    pattern: Pattern,
    load: H264Load,
    drops: DropPlan,
    opts: RunOptions,
) -> (BrowserlessResult, H264Outcome) {
    require_lib();
    run_on(seed, secs, net, pattern, load, drops, opts).await
}

/// The 20 ms link with 2 ms of jitter: what makes the three seeds differ on a
/// path that drops nothing (without it they print the same numbers three
/// times).
fn jittery() -> NetProfile {
    let mut net = link_20ms();
    net.jitter_us = 2_000;
    net
}

async fn run_pattern(
    seed: u64,
    secs: u64,
    pattern: Pattern,
    load: H264Load,
) -> (BrowserlessResult, H264Outcome) {
    run_oh(
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
    let l = r.h264_run().map(|x| x.lossy_cells).unwrap_or_default();
    println!(
        "{name} {seed:#x}: {} | idr={} recov={}/{} reroots={} ltr_req={} anchors={} marks={} \
         mark_failed={} fallback={} refused={} holds={} | cells {}/{} ambiguous, worst err {} | \
         drops={}/{} verdicts p={} over_cap={}",
        o.summary(),
        o.idr_made,
        c.recoveries,
        c.recovered,
        c.reroots,
        c.ltr_recoveries_requested,
        c.anchors_confirmed,
        c.marks_observed,
        c.ltr_mark_failed_sent,
        c.idr_backend_fallback,
        c.frames_refused,
        c.holds,
        l.read,
        l.ambiguous,
        l.max_err,
        r.s2c_datagrams_dropped,
        r.s2c_datagrams_offered,
        r.h264.stats.verdicts_p_frag,
        r.h264.stats.frames_over_cap,
    );
}

/// The largest distance of a block's centre from the level it was read as that
/// a scene tolerates (the levels are 192 apart; the halfway point is 96). The
/// baseline is 4 (2.6.0 and 2.4.1); a reading past this is the encoder spending
/// its quantisers, and fails as an unreliable measurement, not as a wrong
/// picture.
const MAX_ERR: u32 = 8;
/// As [`MAX_ERR`] for a scene that starves the rate control on purpose (the
/// rate scenes): the encoder is allowed to spend its highest quantisers, up to
/// the point where the reader calls a block ambiguous (two thirds of the way to
/// the halfway point). The worst seen is 48 (scene 9).
const MAX_ERR_STARVED: u32 = 64;

/// What holds in every openh264 scene: the library ran, the measurement is
/// sound (every delivered frame was read cell by cell, none ambiguous, none
/// read farther from its level than `max_err`), the client displayed no wrong
/// picture, and the stream is alive at the end.
fn assert_sound(ctx: &str, secs: u64, r: &BrowserlessResult, o: &H264Outcome, max_err: u32) {
    assert_eq!(
        r.h264.refs.backend, "openh264",
        "{ctx}: premise: the library encoded"
    );
    let cells = r.h264_run().expect("an H.264 scene").lossy_cells;
    assert!(o.fed > 0 && o.correct > 0, "{ctx}: nothing was displayed");
    assert_eq!(
        cells.read,
        64 * o.fed as u64,
        "{ctx}: premise: every delivered frame was read, every cell"
    );
    assert_eq!(
        cells.ambiguous, 0,
        "{ctx}: premise: no cell was ambiguous: {cells:?}"
    );
    assert!(
        cells.max_err <= max_err,
        "{ctx}: measurement unsound, not a wrong picture: a block read {} from its level \
         (limit {max_err}): {cells:?}",
        cells.max_err
    );
    // The library marked long-term pictures and the receiver's word made them
    // anchors (it is what every recovery rests on).
    assert!(
        o.refs.marks_observed >= 2 && o.refs.anchors_confirmed >= 2,
        "{ctx}: premise: no anchors: {:?}",
        o.refs
    );
    assert_eq!(o.unknown_to_truth, 0, "{ctx}");
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
    assert_eq!(o.idr_made as u64, r.h264.stats.frames_idr, "{ctx}");
    assert!(
        o.last_correct_us.unwrap_or(0) + 1_000_000 >= secs * 1_000_000,
        "{ctx}: dead at the end: {:?}",
        o.last_correct_us
    );
}

/// What one recovery from the library's long-term picture costs: the request
/// reaches the library after the loss path's verdict, and the recovery frame
/// takes a round trip out and the receiver's word a round trip back.
const RECOVERY_US: u64 = 3 * RTT_US + VERDICT_US;

/// The longest stretch without a correct picture a loss scene may have: a
/// recovery, and one more for a recovery that was itself lost, and what
/// `FREEZE_GAP_FRAMES` already grants. Fixed by the link and the scene's
/// timing, not by how much was lost, so a slower recovery cannot raise it.
/// The worst of the three seeds is 300 ms (bursts).
fn gap_bound_us() -> u64 {
    2 * RECOVERY_US + FREEZE_GAP_FRAMES * ghostframe_e2e::harness::browserless::DEFAULT_CADENCE_US
}

/// The most time beyond the freeze allowance a loss scene may spend frozen,
/// in all: twice the worst of the three seeds on 2.6.0 and 2.4.1 (335 ms, bursts).
const FROZEN_CEILING_US: u64 = 700_000;

/// The effects of a lossy path on the observed-mode policy: the library is
/// never reset (one IDR, none of its own), it is asked for recoveries from its
/// long-term picture and makes them, the manager sees them as re-roots, and
/// every freeze is bounded by what a recovery costs.
fn assert_recovers(ctx: &str, secs: u64, r: &BrowserlessResult, o: &H264Outcome) {
    // Premise: the path dropped frame datagrams, the loss path judged P
    // fragments lost, and frames never reached the client.
    assert!(r.s2c_datagrams_dropped > 0, "{ctx}: nothing was dropped");
    assert!(r.h264.stats.verdicts_p_frag > 0, "{ctx}: no verdict");
    assert!(o.never_fed > 0, "{ctx}: no frame was lost for good");
    assert_sound(ctx, secs, r, o, MAX_ERR);
    let c = &o.refs;
    assert_eq!(o.idr_made, 1, "{ctx}: the library makes no loss IDR");
    assert_eq!(c.idr_session, 1, "{ctx}");
    assert_eq!(
        c.idr_backend_fallback, 0,
        "{ctx}: an IDR nobody asked for: {c:?}"
    );
    assert!(
        c.marks_observed > 0 && c.anchors_confirmed > 0,
        "{ctx}: {c:?}"
    );
    assert!(
        c.recoveries >= 1 && c.ltr_recoveries_requested >= 1 && c.reroots >= 1,
        "{ctx}: {c:?}"
    );
    assert!(c.recovered + 1 >= c.recoveries, "{ctx}: {c:?}");
    assert!(
        o.refs_to_undisplayed < o.made / 4,
        "{ctx}: {} of {} frames chained from a frame the client never showed",
        o.refs_to_undisplayed,
        o.made
    );
    let bound = gap_bound_us();
    assert!(
        o.max_gap_us <= bound,
        "{ctx}: gap {} us beyond the bound {bound} us (longest lost run {})",
        o.max_gap_us,
        o.longest_lost_run
    );
    assert!(
        o.frozen_us <= FROZEN_CEILING_US,
        "{ctx}: frozen {} us beyond {FROZEN_CEILING_US} us",
        o.frozen_us
    );
}

/// A clean stream: one IDR, every frame right, no freeze, marks confirmed.
#[tokio::test(start_paused = true)]
async fn openh264_clean_stream_shows_every_frame_right_with_one_idr() {
    for seed in SEEDS {
        let (r, o) = run_oh(
            seed,
            6,
            jittery(),
            Pattern::Clean,
            oh(0),
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        line("oh-clean", seed, &r, &o);
        let ctx = format!("oh-clean {seed:#x}");
        assert_sound(&ctx, 6, &r, &o, MAX_ERR);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert_eq!(o.freezes, 0, "{ctx}");
        assert!(o.correct > 100, "{ctx}");
        let c = &o.refs;
        assert_eq!(
            (
                c.recoveries,
                c.p_recovery,
                c.idr_reissued,
                c.idr_backend_fallback
            ),
            (0, 0, 0, 0),
            "{ctx}"
        );
        // The library marked a long-term picture every period and the
        // receiver's word made them anchors.
        assert!(
            c.marks_observed >= 4 && c.anchors_confirmed >= 4 && c.frames_decoded_acked > 0,
            "{ctx}: {c:?}"
        );
        assert_eq!(c.ltr_mark_failed_sent, 0, "{ctx}: {c:?}");
    }
}

/// The measurement itself: a source that paints the previous frame's canvas on
/// every 50th frame (the truth log keeps the right one) shows exactly those
/// frames as wrong pictures. The difference is a handful of marker bits in one
/// cell, so a reader too coarse to see it (or a comparison that tolerated it)
/// fails here, while the clean scene cannot tell whether it would have noticed.
#[tokio::test(start_paused = true)]
async fn openh264_a_stale_paint_is_shown_as_a_wrong_picture() {
    for seed in SEEDS {
        let (r, o) = run_oh(
            seed,
            6,
            jittery(),
            Pattern::Clean,
            oh_canvas(0, 0, 50),
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        line("oh-stale", seed, &r, &o);
        let ctx = format!("oh-stale {seed:#x}");
        let run = r.h264_run().expect("an H.264 scene");
        // Premise: the cells were read soundly, and stale frames were shown.
        assert_eq!(run.lossy_cells.ambiguous, 0, "{ctx}");
        assert!(run.lossy_cells.max_err <= MAX_ERR, "{ctx}");
        let stale: Vec<u32> = run
            .display
            .iter()
            .map(|d| d.frame_seq)
            .filter(|s| *s > 0 && s % 50 == 7)
            .collect();
        assert!(stale.len() >= 2, "{ctx}: {stale:?}");
        let wrong: Vec<u32> = o.wrong.iter().map(|w| w.frame_seq).collect();
        assert_eq!(
            wrong, stale,
            "{ctx}: the wrong pictures are the stale paints"
        );
        // And they are found to be the previous frame's canvas.
        assert!(
            o.wrong
                .iter()
                .all(|w| w.effective_ref == Some(w.frame_seq - 1)),
            "{ctx}: {}",
            o.wrong_list(8)
        );
    }
}

/// Scene 1: independent loss, 1 % and 5 %.
#[tokio::test(start_paused = true)]
async fn openh264_scene_1_iid_loss_recovers_from_the_long_term_picture_with_one_idr() {
    for (p, name) in [(0.01, "oh-iid1"), (0.05, "oh-iid5")] {
        for seed in SEEDS {
            let (r, o) = run_pattern(seed, SECS, Pattern::Iid(p), oh(0)).await;
            line(name, seed, &r, &o);
            assert_recovers(&format!("{name} {seed:#x}"), SECS, &r, &o);
        }
    }
}

/// Scene 2: bursts, about 4 datagrams a burst at 80 % loss. A burst can take
/// a mark's carrier: the mark is reported failed and the library re-marks.
#[tokio::test(start_paused = true)]
async fn openh264_scene_2_burst_loss_recovers_with_one_idr() {
    let burst = Pattern::Burst {
        enter: 0.01,
        exit: 0.3,
        loss: 0.8,
    };
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, SECS, burst, oh(0)).await;
        line("oh-burst", seed, &r, &o);
        assert_recovers(&format!("oh-burst {seed:#x}"), SECS, &r, &o);
    }
}

/// Scene 3: the frame feedback (0x0A) alone is lost, for 1.5 s. Every frame
/// arrives, nothing needs repair, but the manager hears nothing: its
/// candidates age out and it recovers from the last anchor it confirmed,
/// without corruption and without an IDR; the returning decoded bits override
/// the suspicion.
#[tokio::test(start_paused = true)]
async fn openh264_scene_3_lost_frame_acks_cost_no_corruption_and_no_idr() {
    for seed in SEEDS {
        let opts = RunOptions {
            frame_ack_loss: Some((
                1.0,
                Duration::from_millis(2_000),
                Duration::from_millis(3_500),
            )),
            ..RunOptions::default()
        };
        let (r, o) = run_oh(
            seed,
            SECS,
            jittery(),
            Pattern::Clean,
            oh(0),
            DropPlan::default(),
            opts,
        )
        .await;
        line("oh-noacks", seed, &r, &o);
        let ctx = format!("oh-noacks {seed:#x}");
        let c = &o.refs;
        // Premise: only the feedback was lost, and the manager noticed the
        // silence (frames went suspect by their deadline).
        assert_eq!(r.s2c_datagrams_dropped, 0, "{ctx}");
        assert!(r.h264.frame_acks_dropped > 10, "{ctx}");
        assert!(c.deadline_suspects > 0, "{ctx}: {c:?}");
        assert_sound(&ctx, SECS, &r, &o, MAX_ERR);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert_eq!(c.idr_reissued + c.idr_backend_fallback, 0, "{ctx}: {c:?}");
        // (Jitter can reorder a frame past its successor: the gate refuses it.)
        assert!(o.never_fed <= 4, "{ctx}: {} frames lost", o.never_fed);
        // The silence was survived by asking the library to recover from the
        // anchor, and the returning decoded bits won over the suspicion.
        assert!(
            c.recoveries >= 1 && c.ltr_recoveries_requested >= 1,
            "{ctx}: {c:?}"
        );
        assert!(c.recovered + 1 >= c.recoveries, "{ctx}: {c:?}");
        assert!(c.suspect_overridden > 0, "{ctx}: {c:?}");
        assert!(c.anchors_confirmed > 5, "{ctx}: confirmation resumed");
    }
}

/// Scene 4: reordering. Within a quarter of a frame period datagrams swap
/// inside a frame and nothing is late. At 45 ms, longer than a frame period,
/// frames complete out of order: the gate refuses the late ones (a refused bit,
/// never a decode), the loss path mistakes late datagrams for lost, and the
/// manager recovers from the library's long-term picture. No wrong picture,
/// no drift, one IDR.
#[tokio::test(start_paused = true)]
async fn openh264_scene_4_reordering_refuses_late_frames_and_never_drifts() {
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, SECS, Pattern::Reorder { window_us: 8_000 }, oh(0)).await;
        line("oh-reorder8", seed, &r, &o);
        let ctx = format!("oh-reorder8 {seed:#x}");
        assert_sound(&ctx, SECS, &r, &o, MAX_ERR);
        assert_eq!(o.idr_made, 1, "{ctx}");
        assert!(o.correct > 200, "{ctx}");
    }
    for seed in SEEDS {
        let (r, o) = run_pattern(seed, SECS, Pattern::Reorder { window_us: 45_000 }, oh(0)).await;
        line("oh-reorder45", seed, &r, &o);
        let ctx = format!("oh-reorder45 {seed:#x}");
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
        assert_sound(&ctx, SECS, &r, &o, MAX_ERR);
        let c = &o.refs;
        assert_eq!(o.fed_out_of_order, 0, "{ctx}");
        assert!(
            c.frames_refused > 0 || o.never_fed > 0,
            "{ctx}: nothing was late: {c:?}"
        );
        assert_eq!(
            (o.idr_made, c.idr_backend_fallback),
            (1, 0),
            "{ctx}: reordering is not a reason for an IDR"
        );
        assert!(
            c.recoveries >= 1 && c.ltr_recoveries_requested >= 1,
            "{ctx}: {c:?}"
        );
        assert!(
            o.refs_to_undisplayed < o.made / 2,
            "{ctx}: {} of {} frames chained from a frame never shown",
            o.refs_to_undisplayed,
            o.made
        );
        // Lateness costs recoveries, not freezes: the baseline is 104 ms and
        // 0 ms frozen.
        assert!(
            o.max_gap_us <= gap_bound_us() && o.frozen_us <= 250_000,
            "{ctx}: gap {} us (bound {} us), frozen {} us",
            o.max_gap_us,
            gap_bound_us(),
            o.frozen_us
        );
    }
}

/// Scene 5: H.264 -> tile -> H.264 -> tile -> H.264. A return is a recovery P
/// from the library's long-term picture, not an IDR: the scene's one IDR (at
/// most one more, and then counted as the library's fallback), and the stream
/// resumes within a few round trips of each return.
#[tokio::test(start_paused = true)]
async fn openh264_scene_5_tile_transitions_return_to_h264_without_an_idr() {
    for seed in SEEDS {
        let mut l = oh(0);
        l.tile_phases = [Some((90, 150)), Some((210, 250))];
        let (r, o) = run_oh(
            seed,
            SECS,
            jittery(),
            Pattern::Clean,
            l,
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        line("oh-tiles", seed, &r, &o);
        let ctx = format!("oh-tiles {seed:#x}");
        // Premise: the tile stretches happened, and H.264 ran either side.
        let tiles = r
            .events
            .iter()
            .filter(|e| matches!(e, ClientNetEvent::Core(CoreEvent::TileReady { .. })))
            .count();
        assert!(tiles >= 90, "{ctx}: {tiles} tile paints");
        assert_sound(&ctx, SECS, &r, &o, MAX_ERR);
        let c = &o.refs;
        assert!(o.idr_made <= 2, "{ctx}: {} IDRs", o.idr_made);
        assert_eq!(
            o.idr_made as u64,
            1 + c.idr_backend_fallback,
            "{ctx}: every IDR past the first is counted as the library's own: {c:?}"
        );
        assert_eq!(
            r.h264.stats.idr_by_cause[IdrCause::Reentry.index()],
            0,
            "{ctx}"
        );
        assert_eq!(c.reentries, 2, "{ctx}: {c:?}");
        assert!(c.reentry_bytes > 0, "{ctx}");
        assert_eq!(o.resume_us.len(), 2, "{ctx}");
        for (i, t) in o.resume_us.iter().enumerate() {
            let t = t.unwrap_or_else(|| panic!("{ctx}: no picture after tile stretch {i}"));
            assert!(t <= 4 * RTT_US, "{ctx}: stretch {i} resumed after {t} us");
        }
    }
}

/// Scene 7: lost IDR. Its fragments are resent by the loss path, the manager
/// holds its P frames until the IDR is delivered, and the first thing the
/// client decodes is the IDR, right.
#[tokio::test(start_paused = true)]
async fn openh264_scene_7_a_lost_idr_is_resent_and_p_frames_wait_for_it() {
    for seed in SEEDS {
        let drops = DropPlan::default().with_frame_rules(vec![
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Index(1),
                occurrences: vec![0, 1],
            },
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Index(3),
                occurrences: vec![0],
            },
            FrameDropRule {
                frame_seq: 0,
                frag: FragSel::Last,
                occurrences: vec![0],
            },
        ]);
        let (r, o) = run_oh(
            seed,
            6,
            jittery(),
            Pattern::Clean,
            oh(0),
            drops,
            RunOptions::default(),
        )
        .await;
        line("oh-lostidr", seed, &r, &o);
        let ctx = format!("oh-lostidr {seed:#x}");
        let c = &o.refs;
        // Premise: every rule fired, the loss path resent, P frames were held.
        assert_eq!(r.frame_drops_fired, vec![2, 1, 1], "{ctx}");
        assert!(r.emitter_stats.h264_idr_resends >= 3, "{ctx}");
        assert!(c.holds >= 1, "{ctx}: the manager held nothing");
        assert_sound(&ctx, 6, &r, &o, MAX_ERR);
        assert_eq!(o.idr_made, 1, "{ctx}: repaired, not replaced");
        assert_eq!(c.idr_reissued + c.idr_backend_fallback, 0, "{ctx}");
        let run = r.h264_run().unwrap();
        assert_eq!(
            run.display.first().map(|d| d.frame_seq),
            Some(0),
            "{ctx}: the IDR is the first thing the client decoded"
        );
        assert!(
            o.first_correct_us
                .is_some_and(|t| t <= 313_000 + 12 * RTT_US),
            "{ctx}: {:?}",
            o.first_correct_us
        );
        assert!(o.correct > 100, "{ctx}");
    }
}

/// The link of scene 6 before and after its fall (bytes a second): 3 and
/// 0.8 Mbit/s.
const SCENE6_LINK: [(u64, u64); 2] = [(0, 375_000), (6_000_000, 100_000)];
/// The texture repainted in half the cells of every frame: about 1.3 Mbit/s of
/// new texture, more than the link carries after the fall and less than it
/// carries before, so the library's rate control is what holds the stream to
/// the estimate after the fall and the content, not the rate, limits it before.
const CHURN: u32 = 2;

/// Scene 6, rate control with the real rate control: the link falls from 3 to
/// 0.8 Mbit/s at 6 s (the estimate before it is at least twice the link after
/// it, on every seed). The content wants about 1.3 Mbit/s, so after the fall
/// the library's rate control is what holds the stream to the estimate.
/// Asserted from the encoder's own access units, those coded at the rate in
/// force (not the probe raise): in no second from one RTT after the drop do
/// they exceed 1.2 x the share x the estimate (soft: the library has no
/// per-frame cap), and on average over the seconds after the fall they are at
/// least 0.6 x it (a library that ignored the rate it was told, or a policy
/// that never told it, fails one side or the other); no wrong picture, one IDR
/// and no IDR of the library's own; the frame queue delay stays within the
/// link's buffer and a second.
///
/// Premises: the estimate fell to under half of its earlier peak and that
/// peak was at least twice the link, the policy moved the rate down in place,
/// the library was told the rates (`rate_target_bps`), and the probe raise ran.
/// The control (policy off) is told no rate: its `rate_target_bps` stays zero.
#[tokio::test(start_paused = true)]
async fn openh264_scene_6_the_library_follows_the_estimate_down() {
    let drop_at = SCENE6_LINK[1].0;
    let link_after_bps = SCENE6_LINK[1].1 * 8;
    for seed in SEEDS {
        require_lib();
        let run = DropRun::with_jitter(seed, &SCENE6_LINK, 16, 2_000, |l| {
            *l = oh_canvas(0, CHURN, 0)
        })
        .await;
        let o = run.r.h264_run().expect("an H.264 scene").analyse();
        let ctx = format!("oh-capdrop {seed:#x}");
        let refs = &run.r.h264.refs;
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
            before >= 2 * link_after_bps,
            "{ctx}: premise: the drop is a drop: the estimate before it {before} bit/s is not \
             twice the link after it ({link_after_bps} bit/s)"
        );
        assert!(
            after.saturating_mul(2) <= before,
            "{ctx}: premise: the estimate fell to under half: {before} -> {after}"
        );
        assert!(
            run.r.h264.stats.rate_target_bps > 0,
            "{ctx}: premise: a rate was told"
        );
        assert!(refs.rate_applies_down >= 1, "{ctx}: premise: {refs:?}");
        assert!(
            refs.probe_raise_windows > 0 || run.r.h264.stats.probe_padding_windows > 0,
            "{ctx}: premise: the stream probed"
        );
        assert_sound(&ctx, 16, &run.r, &o, MAX_ERR_STARVED);
        assert_eq!((o.idr_made, refs.idr_backend_fallback), (1, 0), "{ctx}");
        let (ratio, at) = run.worst_ratio(drop_at + RTT_US);
        let mean = run.mean_ratio(drop_at + 1_000_000);
        let latency = run.max_latency_us(drop_at + RTT_US);
        println!(
            "{ctx}: est {before} -> {after}; coded/(0.8 est) worst {ratio:.2} at {:+.1} s, \
             mean {mean:.2}; skips in 10 s {}; max latency {} ms; applies up {} down {}; \
             max_gap {} ms",
            (at as f64 - drop_at as f64) / 1e6,
            run.skips_after(drop_at, 10_000_000),
            latency / 1000,
            refs.rate_applies_up,
            refs.rate_applies_down,
            o.max_gap_us / 1000,
        );
        assert!(
            ratio <= 1.2,
            "{ctx}: coded {ratio:.2} x {} x the estimate in the 1 s from {at}",
            super::refs_scenes::RATE_SHARE
        );
        assert!(
            mean >= 0.6,
            "{ctx}: coded only {mean:.2} x {} x the estimate after the fall: the library does \
             not spend the rate it was told",
            super::refs_scenes::RATE_SHARE
        );
        assert!(
            latency <= 600_000 + 1_000_000,
            "{ctx}: a frame took {latency} us to be shown"
        );
    }
}

/// The link of scene 6b before and after its rise (bytes a second): 0.8 and
/// 3 Mbit/s.
const SCENE6B_LINK: [(u64, u64); 2] = [(0, 100_000), (5_000_000, 375_000)];

/// Scene 6b, the rise: the link rises from 0.8 to 3 Mbit/s at 5 s and the
/// estimate follows it. The library must accept the new rate (openh264 2.4.x
/// refused every rise: its layer maximum, set after the target, was still the
/// opening rate) and the stream must carry more: the coded bytes of the
/// seconds after the rise settle are at least 2 x those before the rise (the
/// estimate rose about 3 x; the content wants about 1.3 Mbit/s, which caps the
/// stream well under the rate told).
///
/// Premises: the estimate rose to at least 1.5 x its level in the 1.5 s
/// before the rise, the policy
/// moved the rate up in place (`rate_applies_up`), a rate was told, the
/// content wanted more than the stream carried before the rise.
#[tokio::test(start_paused = true)]
async fn openh264_scene_6b_the_library_follows_the_estimate_up() {
    let rise_at = SCENE6B_LINK[1].0;
    for seed in SEEDS {
        require_lib();
        let refused_before = openh264_rate_refusals();
        let run = DropRun::with_jitter(seed, &SCENE6B_LINK, 16, 2_000, |l| {
            *l = oh_canvas(0, CHURN, 0)
        })
        .await;
        let o = run.r.h264_run().expect("an H.264 scene").analyse();
        let ctx = format!("oh-caprise {seed:#x}");
        let refs = &run.r.h264.refs;
        let before = run
            .r
            .bwe_estimate_samples
            .iter()
            .filter(|(t, _)| (rise_at - 1_500_000..rise_at).contains(t))
            .map(|x| x.1)
            .max()
            .unwrap_or(0);
        let after = run
            .r
            .bwe_estimate_samples
            .iter()
            .filter(|(t, _)| *t >= rise_at)
            .map(|x| x.1)
            .max()
            .unwrap_or(0);
        assert!(
            before > 0 && after * 2 >= before * 3,
            "{ctx}: premise: the estimate rose by 1.5 x: {before} -> {after}"
        );
        let told = run.r.h264.stats.rate_target_bps;
        assert!(told > 0, "{ctx}: premise: a rate was told");
        // The library takes every rate it is told, the rises included (a
        // refused rise is retried at the next frame, so the bytes alone
        // would hide it).
        assert_eq!(
            openh264_rate_refusals() - refused_before,
            0,
            "{ctx}: openh264 refused a rate change"
        );
        assert!(refs.rate_applies_up >= 1, "{ctx}: premise: {refs:?}");
        // Coded bits a second, the stream at the rate in force.
        let mean_bps = |from: u64, to: u64| {
            let n = (to - from) / 100_000;
            (0..n)
                .map(|k| run.bytes_in(from + 100_000 * k))
                .sum::<usize>() as f64
                * 8.0
                / n as f64
        };
        let low = mean_bps(2_000_000, rise_at);
        let high = mean_bps(rise_at + 5_000_000, 15_000_000);
        println!(
            "{ctx}: est {before} -> {after}; told {told}; coded {low:.0} -> {high:.0} bit/s; \
             applies up {} down {}",
            refs.rate_applies_up, refs.rate_applies_down,
        );
        assert!(
            high >= 2.0 * low,
            "{ctx}: the stream did not follow the rise up: {low:.0} -> {high:.0} bit/s"
        );
        assert_sound(&ctx, 16, &run.r, &o, MAX_ERR_STARVED);
    }
}

/// Scene 9, heavy content: the whole picture changes every 45 frames (1.5 s),
/// on a 1.5 Mbit/s link. The library has no per-frame cap, so each change codes
/// in one frame well over the average frame (about 10 kB against a cap of
/// 6 to 7 kB). The stream must absorb it: nothing is shown wrong, the
/// library makes no IDR for the change (scene-change detection is off:
/// switched on, it answers each change with one nobody asked for), the
/// overshoot is counted (`frames_over_cap`) and the longest gap between
/// correct pictures stays within what a frame that size costs the pacer.
#[tokio::test(start_paused = true)]
async fn openh264_scene_9_a_scene_change_is_absorbed_without_an_idr() {
    for seed in SEEDS {
        let net = bottleneck_net(
            CapTimeline {
                points: vec![(0, 187_500)],
            },
            20_000,
            2_000,
            600,
        );
        let (r, o) = run_oh(
            seed,
            SECS,
            net,
            Pattern::Clean,
            oh(45),
            DropPlan::default(),
            RunOptions::default(),
        )
        .await;
        line("oh-heavy", seed, &r, &o);
        let ctx = format!("oh-heavy {seed:#x}");
        let aus = &r.h264_run().expect("an H.264 scene").truth.au_log;
        let p_avg = aus
            .iter()
            .filter(|a| !a.idr)
            .map(|a| a.bytes)
            .sum::<usize>()
            / aus.iter().filter(|a| !a.idr).count().max(1);
        let big = aus
            .iter()
            .filter(|a| !a.idr)
            .map(|a| a.bytes)
            .max()
            .unwrap_or(0);
        // Premise: the changes happened (a P frame far above the average
        // exists) and the overshoot was counted.
        assert!(
            big > 8 * p_avg.max(1),
            "{ctx}: largest P {big} B, average {p_avg} B"
        );
        assert!(
            r.h264.stats.frames_over_cap > 0,
            "{ctx}: nothing was over the cap"
        );
        assert!(r.h264.stats.max_cap_ratio_pct > 100, "{ctx}");
        assert_sound(&ctx, SECS, &r, &o, MAX_ERR_STARVED);
        let c = &o.refs;
        assert_eq!(
            (o.idr_made, c.idr_backend_fallback),
            (1, 0),
            "{ctx}: a scene change is not a reason for an IDR"
        );
        // A frame of `big` bytes takes `big` / the stream's rate to cross the
        // pacer and can pass its decode deadline, which costs one recovery
        // (never an IDR): the gap is the crossing plus what a recovery costs.
        // Recoveries are bounded by the changes there were.
        let rate_bps = u64::from(r.h264.stats.rate_target_bps).max(1);
        let crossing_us = big as u64 * 8 * 1_000_000 / rate_bps;
        let bound = crossing_us + gap_bound_us();
        assert!(c.recoveries <= SECS * 1000 / 1500 + 1, "{ctx}: {c:?}");
        assert!(
            o.max_gap_us <= bound,
            "{ctx}: gap {} us beyond {bound} us (largest P {big} B at {rate_bps} bit/s, {crossing_us} us to cross)",
            o.max_gap_us
        );
    }
}
