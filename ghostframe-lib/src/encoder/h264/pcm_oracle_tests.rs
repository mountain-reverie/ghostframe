//! T2: the `RefManager`'s plans, written as real H.264 streams and decoded by
//! libavcodec.
//!
//! The T1 simulation (`refs_tests`) runs server, network and client gate with
//! the header writer in the path and a [`DecoderModel`] standing in for the
//! decoder. Here the model is replaced by the real thing: every frame the
//! client gate admitted is written with the PCM-patch writer and fed to
//! libavcodec (software, `err_recognition = explode`, low delay). The decoded
//! picture must be exactly the analytic picture of the reference the plan
//! named, so a wrong list, a missing RPLM or a bad marking shows up as a
//! picture built from another frame's markers.
//!
//! Frames the network lost or the gate refused are never fed: the decoder sees
//! the `frame_num` gaps and POC jumps the real client would.

use std::collections::BTreeMap;

use proptest::prelude::*;

use super::pcm_decoder::{PcmDecoder, Strictness};
use super::pcm_writer::{access_unit, parameter_sets, pcm_params, Picture, HEIGHT, WIDTH};
use super::refs::RefTarget;
use super::refs_tests::{arb_script, run_sim, script_from, Fate, Logged, Script, Sim, STEP};
use super::syntax::{DecoderModel, GapMode, Mmco, RefPic, Rplm, SliceHeader};
use super::tracker::PicKind;

/// libavcodec as the oracle: software H.264, strict, no output delay allowed.
struct Decoder(PcmDecoder);

impl Decoder {
    fn new() -> Self {
        Self(PcmDecoder::new(Strictness::Strict))
    }

    /// Feed one access unit; the picture must come back from this very call
    /// with the same pts (design 3.5: zero output delay).
    fn feed(&mut self, au: &[u8], pts: i64) -> Result<Vec<u8>, String> {
        let d = self.0.feed(au, pts).map_err(|e| e.to_string())?;
        if d.damaged {
            return Err(format!("picture {pts} is flagged damaged"));
        }
        if d.pts != Some(pts) {
            return Err(format!("picture pts {:?}, fed {pts}", d.pts));
        }
        if (d.width, d.height) != (WIDTH, HEIGHT) {
            return Err(format!("picture is {}x{}", d.width, d.height));
        }
        Ok(d.yuv)
    }
}

/// What a checked stream exercised, so a test can assert its premise.
#[derive(Debug, Default)]
struct Report {
    checked: usize,
    idrs: usize,
    st_refs: usize,
    lt_refs: [usize; 2],
    non_ref: usize,
    marks: usize,
    /// Frames fed after a `frame_num` gap.
    gaps: usize,
    /// Largest `frame_num` gap, in frames.
    max_gap: u32,
    /// Gaps that contain the 127 -> 0 wrap.
    wrap_gaps: usize,
    first_mark: Option<Vec<Mmco>>,
}

/// Write every frame the client fed, in order, decode it, and compare with the
/// analytic picture. `mutate` rewrites each header on its way to the stream
/// (identity for the real thing; a defect for a negative control). The error
/// names the first frame that disagrees.
fn check(log: &[Logged], mutate: &dyn Fn(&mut SliceHeader)) -> Result<Report, String> {
    let mut fed_so_far: Vec<&Logged> = Vec::new();
    check_inner(log, mutate, &mut fed_so_far).map_err(|e| {
        let ctx: Vec<String> = fed_so_far
            .iter()
            .rev()
            .take(40)
            .map(|l| {
                format!(
                    "  {} {:?} idc{} fn{} -> {:?} mark {:?} mmco {:?} rplm {:?}",
                    l.plan.frame_seq,
                    l.plan.kind,
                    l.plan.nal_ref_idc,
                    l.plan.frame_num,
                    l.plan.ref_frame_seq(),
                    l.plan.mark,
                    l.plan.mmco,
                    l.plan.rplm
                )
            })
            .collect();
        format!("{e}\nfed before (newest first):\n{}", ctx.join("\n"))
    })
}

fn check_inner<'a>(
    log: &'a [Logged],
    mutate: &dyn Fn(&mut SliceHeader),
    fed_so_far: &mut Vec<&'a Logged>,
) -> Result<Report, String> {
    let p = pcm_params();
    let mut dec = Decoder::new();
    let mut model = DecoderModel::new(p);
    let mut pics: BTreeMap<u32, Picture> = BTreeMap::new();
    let mut rep = Report::default();
    let mut prev_ref_fn = 0u32;
    for l in log.iter().filter(|l| l.fed == Some(true)) {
        fed_so_far.push(l);
        let (plan, seq) = (&l.plan, l.plan.frame_seq);
        let tag = u16::try_from(seq).expect("sequence numbers fit the marker");
        let mut h = l.header.clone();
        mutate(&mut h);

        let expected = match (plan.kind, plan.ref_frame_seq()) {
            (PicKind::Idr, _) => Picture::idr(tag),
            (_, Some(t)) => Picture::predicted(
                pics.get(&t).ok_or(format!(
                    "frame {seq}: its plan names {t}, which was never fed"
                ))?,
                tag,
            ),
            (kind, None) => return Err(format!("frame {seq}: a {kind:?} frame without a target")),
        };

        let mut au = Vec::new();
        if plan.kind == PicKind::Idr {
            au.extend(parameter_sets(&p));
        }
        au.extend(access_unit(&p, &h, tag));
        let yuv = dec
            .feed(&au, i64::from(seq))
            .map_err(|e| format!("frame {seq} ({:?}): {e}", plan.kind))?;
        let decoded = Picture::read(&yuv).ok_or(format!(
            "frame {seq}: the decoded picture is not made of markers"
        ))?;
        if decoded != expected {
            // Which reference did the decoder really predict from?
            let source = pics
                .iter()
                .find(|(_, r)| Picture::predicted(r, tag) == decoded)
                .map(|(s, _)| *s);
            return Err(format!(
                "frame {seq} ({:?}): decoded from {source:?}, plan named {:?}",
                plan.kind,
                plan.ref_frame_seq()
            ));
        }

        // The decoder's reference bookkeeping, as far as it is observable:
        // the model replays the same headers, and with `explode` an overfull
        // DPB would already have been an error above.
        let eff = model.push(seq, &h);
        if eff.violations.any() || eff.dpb_peak > p.max_num_ref_frames as usize {
            return Err(format!(
                "frame {seq}: DPB {:?} peak {}",
                eff.violations, eff.dpb_peak
            ));
        }
        // libavcodec conceals a `frame_num` gap by copying the previous
        // picture into the "non-existing" frame, so a header that names one
        // can decode to the right pixels by luck. The model does not forgive it.
        if let Some(t) = plan.ref_frame_seq() {
            if !eff.references(t) {
                return Err(format!(
                    "frame {seq}: the header reads {:?}, plan named {t}",
                    eff.reference
                ));
            }
        }

        rep.checked += 1;
        if plan.kind == PicKind::Idr {
            rep.idrs += 1;
            prev_ref_fn = 0;
        } else {
            let gap = (plan.frame_num + p.max_frame_num() - prev_ref_fn) % p.max_frame_num();
            if gap > 1 || (gap == 0 && plan.nal_ref_idc != 0) {
                rep.gaps += 1;
                rep.max_gap = rep.max_gap.max(gap);
                if plan.frame_num < prev_ref_fn {
                    rep.wrap_gaps += 1;
                }
            }
            if plan.nal_ref_idc != 0 {
                prev_ref_fn = plan.frame_num;
            } else {
                rep.non_ref += 1;
            }
            match plan.target {
                Some(RefTarget::Short { .. }) => rep.st_refs += 1,
                Some(RefTarget::Long { slot, .. }) => rep.lt_refs[usize::from(slot)] += 1,
                None => {}
            }
            if plan.mark.is_some() {
                rep.marks += 1;
                rep.first_mark.get_or_insert_with(|| plan.mmco.clone());
            }
        }
        pics.insert(seq, expected);
    }
    Ok(rep)
}

fn ok(log: &[Logged]) -> Report {
    check(log, &|_| {}).unwrap_or_else(|e| panic!("{e}"))
}

fn lossy(
    steps: usize,
    mut lose: impl FnMut(usize, &super::refs::FramePlan) -> bool + 'static,
) -> Sim {
    run_sim(Script::new(steps, move |step, plan| Fate {
        lose: lose(step, plan),
        ..Fate::default()
    }))
}

// ------------------------------------------------------------------ scenarios

#[test]
fn healthy_chain_wraps_frame_num_and_swaps_both_slots() {
    let sim = run_sim(Script::lossless(400));
    let r = ok(&sim.log);
    assert!(r.checked >= 395, "{r:?}");
    assert!(r.marks >= 10 && r.st_refs > 250, "{r:?}");
    // The chain crossed MaxFrameNum (128) more than twice, with no gap.
    assert!(sim.log.iter().any(|l| l.plan.frame_num < 5 && l.step > 250));
    assert_eq!(r.gaps, 0, "{r:?}");
    assert_eq!(r.idrs, 1);
}

#[test]
fn first_mark_carries_mmco_4_1_6_and_decodes() {
    let sim = run_sim(Script::lossless(60));
    let r = ok(&sim.log);
    assert!(
        matches!(
            r.first_mark.as_deref(),
            Some([
                Mmco::MaxLong(2),
                Mmco::ForgetShort(_),
                Mmco::CurrentToLong(1)
            ])
        ),
        "{:?}",
        r.first_mark
    );
}

#[test]
fn loss_recovers_from_the_anchor_and_rejoins() {
    let mut lost = false;
    let sim = lossy(160, move |step, plan| {
        step >= 60 && plan.kind == PicKind::Healthy && !std::mem::replace(&mut lost, true)
    });
    let r = ok(&sim.log);
    assert!(sim.log.iter().any(|l| l.lost));
    assert!(
        r.lt_refs.iter().sum::<usize>() >= 1,
        "re-root from an LT: {r:?}"
    );
    assert!(r.non_ref >= 1 && r.gaps >= 1, "{r:?}");
    assert!(
        sim.log.iter().any(|l| l.fed == Some(false)),
        "gate refusals"
    );
}

#[test]
fn repeated_losses_recover_from_both_long_term_slots() {
    let sim = lossy(500, |step, plan| {
        plan.kind == PicKind::Healthy && step % 37 == 20
    });
    let r = ok(&sim.log);
    assert!(r.lt_refs[0] >= 1 && r.lt_refs[1] >= 1, "{r:?}");
    assert!(r.marks >= 10, "{r:?}");
}

#[test]
fn two_consecutive_losses_reroot_again() {
    let mut lost = 0;
    let sim = lossy(200, move |step, plan| {
        if step >= 60 && lost == 0 && plan.kind == PicKind::Healthy {
            lost = 1;
            true
        } else if lost == 1 && plan.kind == PicKind::Reroot {
            lost = 2;
            true
        } else {
            false
        }
    });
    let r = ok(&sim.log);
    assert!(
        sim.log
            .iter()
            .filter(|l| l.plan.kind == PicKind::Reroot)
            .count()
            >= 2,
        "{r:?}"
    );
}

#[test]
fn a_gap_that_contains_the_frame_num_wrap_is_bridged() {
    // Reference frames advance frame_num; losing re-roots from step 124 on
    // leaves the client a gap from about 123 across 127 -> 0.
    let sim = lossy(300, |step, _| (124..146).contains(&step));
    let r = ok(&sim.log);
    assert!(r.wrap_gaps >= 1 && r.max_gap >= 2, "{r:?}");
}

#[test]
fn a_gap_followed_by_the_frame_num_wrap_is_bridged() {
    let sim = lossy(300, |step, _| (30..60).contains(&step));
    let r = ok(&sim.log);
    assert!(r.gaps >= 1 && r.max_gap >= 2 && r.wrap_gaps == 0, "{r:?}");
    assert!(sim
        .log
        .iter()
        .any(|l| l.step > 100 && l.plan.frame_num < 20));
}

#[test]
fn a_freeze_longer_than_64_frames_does_not_lose_the_poc_msb() {
    let sim = lossy(300, |step, _| (100..175).contains(&step));
    let r = ok(&sim.log);
    let before = sim
        .log
        .iter()
        .rfind(|l| l.step < 100 && l.fed == Some(true));
    let after = sim
        .log
        .iter()
        .find(|l| l.step >= 175 && l.fed == Some(true));
    let (b, a) = (before.unwrap(), after.unwrap());
    assert!(
        a.plan.poc - b.plan.poc > 128,
        "freeze spans {} POC",
        a.plan.poc - b.plan.poc
    );
    assert!(r.checked > 150, "{r:?}");
}

#[test]
fn an_anchor_120_frames_old_still_decodes() {
    let mut lost = false;
    let mut sc = Script::new(200, move |step, plan| {
        let mut f = Fate {
            suppress_ack: step < 130,
            ..Fate::default()
        };
        if step >= 130 && !lost && plan.kind == PicKind::Healthy {
            lost = true;
            f.lose = true;
        }
        f
    });
    sc.srtt = Box::new(|_| STEP * 100_000);
    let sim = run_sim(sc);
    let r = ok(&sim.log);
    let idr = sim.log[0].plan.frame_seq;
    let reroot = sim
        .log
        .iter()
        .find(|l| l.plan.kind == PicKind::Reroot)
        .expect("re-root");
    assert_eq!(reroot.plan.ref_frame_seq(), Some(idr));
    assert!(reroot.plan.frame_seq - idr >= 120);
    assert_eq!(reroot.fed, Some(true));
    assert!(r.lt_refs[0] >= 1, "{r:?}");
}

#[test]
fn a_client_reset_restarts_from_an_idr_and_the_old_pictures_are_gone() {
    let mut sc = Script::lossless(200);
    sc.client_reset_at = vec![100];
    let sim = run_sim(sc);
    let r = ok(&sim.log);
    assert_eq!(r.idrs, 2, "{r:?}");
}

#[test]
fn a_lost_idr_is_sent_again_and_decodes() {
    let mut first = true;
    let sc = Script::new(260, move |_, plan| Fate {
        lose: plan.kind == PicKind::Idr && std::mem::replace(&mut first, false),
        ..Fate::default()
    });
    let sim = run_sim(sc);
    let r = ok(&sim.log);
    assert_eq!(r.idrs, 1, "the first IDR never reached the client");
}

#[test]
fn reordering_and_late_acks_never_decode_a_wrong_picture() {
    let mut n = 0;
    let sim = run_sim(Script::new(300, move |_, _| {
        n += 1;
        Fate {
            jitter: n % 4,
            ack_extra: n % 7,
            lose: n % 29 == 0,
            ..Fate::default()
        }
    }));
    let r = ok(&sim.log);
    assert!(sim.log.iter().any(|l| l.fed == Some(false)), "refusals");
    assert!(r.checked > 100, "{r:?}");
}

#[test]
fn survivors_of_frames_the_client_drops_after_the_fact_decode_to_their_reference() {
    // The spike's drop test: delete frames from a finished stream, behind the
    // server's back. The client gate refuses what no longer has its reference
    // (so the decoder never sees it); everything it does admit, including the
    // frames after the deleted run that still reach an older picture, must
    // equal the analytic picture of the reference its plan named.
    let sim = lossy(500, |step, plan| {
        plan.kind == PicKind::Healthy && step % 37 == 20
    });
    let p = pcm_params();
    let mut dec = Decoder::new();
    let mut model = DecoderModel::new(p);
    let mut pics: BTreeMap<u32, Picture> = BTreeMap::new();
    let (mut compared, mut refused) = (0, 0);
    for (i, l) in sim.log.iter().enumerate() {
        if l.fed != Some(true) || (100..140).contains(&i) {
            continue;
        }
        let (seq, tag) = (l.plan.frame_seq, l.plan.frame_seq as u16);
        let expected = match l.plan.ref_frame_seq() {
            _ if l.plan.kind == PicKind::Idr => Picture::idr(tag),
            Some(t) if model.peek(seq, &l.header).references(t) && pics.contains_key(&t) => {
                Picture::predicted(&pics[&t], tag)
            }
            _ => {
                refused += 1;
                continue;
            }
        };
        model.push(seq, &l.header);
        let mut au = Vec::new();
        if l.plan.kind == PicKind::Idr {
            au.extend(parameter_sets(&p));
        }
        au.extend(access_unit(&p, &l.header, tag));
        let yuv = dec.feed(&au, i64::from(seq)).expect("survivor decodes");
        assert_eq!(
            Picture::read(&yuv).as_ref(),
            Some(&expected),
            "survivor {seq} differs from its declared reference"
        );
        compared += 1;
        pics.insert(seq, expected);
    }
    assert!(
        compared > 60 && refused >= 1,
        "premise: {compared} compared, {refused} refused"
    );
}

// ------------------------------------------------------------- negative controls

fn recovery_sim() -> Sim {
    let mut lost = false;
    lossy(200, move |step, plan| {
        step >= 60 && plan.kind == PicKind::Healthy && !std::mem::replace(&mut lost, true)
    })
}

#[test]
fn control_a_long_term_reference_named_with_the_wrong_rplm_idc_is_detected() {
    let sim = recovery_sim();
    ok(&sim.log);
    let wrong = |h: &mut SliceHeader| {
        for op in &mut h.rplm {
            if let Rplm::Long(n) = *op {
                // idc 0 (a short-term subtract) instead of idc 2.
                *op = Rplm::Sub(n);
            }
        }
    };
    let err = check(&sim.log, &wrong).expect_err("idc 0 for a long-term picture must fail");
    eprintln!("detected: {err}");
}

#[test]
fn control_no_rplm_after_a_mark_is_detected() {
    let sim = run_sim(Script::lossless(60));
    ok(&sim.log);
    let err = check(&sim.log, &|h| h.rplm.clear()).expect_err("no RPLM must fail");
    eprintln!("detected: {err}");
}

#[test]
fn control_a_first_mark_without_mmco_1_is_detected() {
    let sim = run_sim(Script::lossless(60));
    ok(&sim.log);
    let err = check(&sim.log, &|h| {
        if let Some(ops) = &mut h.mmco {
            ops.retain(|o| !matches!(o, Mmco::ForgetShort(_)));
        }
    })
    .expect_err("an MMCO-1-less first mark overflows the DPB");
    eprintln!("detected: {err}");
}

#[test]
fn control_a_wrong_frame_num_is_detected() {
    let sim = run_sim(Script::lossless(60));
    let err = check(&sim.log, &|h| {
        if !h.idr && h.frame_num == 20 {
            h.frame_num = 21;
        }
    })
    .expect_err("a skipped frame_num is a gap onto the wrong picture");
    eprintln!("detected: {err}");
}

// ------------------------------------------------------------------- proptest

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Over the T1 fate space (loss, reordering, late acks, spurious verdicts,
    /// a lost IDR): every frame the client gate admitted decodes in libavcodec
    /// to the analytic picture of the reference its plan named.
    #[test]
    fn libavcodec_decodes_every_admitted_frame_to_its_planned_reference(params in arb_script()) {
        let mut sc = script_from(&params, 400);
        let mut sim = Sim::new();
        sim.run(&mut sc, true);
        // A script that loses the IDR for good feeds the decoder nothing.
        prop_assume!(sim.log.iter().any(|l| l.fed == Some(true)));
        match check(&sim.log, &|_| {}) {
            Ok(r) => prop_assert!(r.checked >= 1 && r.idrs >= 1, "{:?}", r),
            Err(e) => prop_assert!(false, "{}", e),
        }
    }
}

// ------------------------------------------------------- startup epochs

/// A startup epoch: heavy early loss, half of it found only by the decode
/// deadline (no verdict), from a seed. Generated by a small LCG so the same
/// epoch can be named in a test and swept by the ignored brute force.
fn startup_epoch(seed: u64) -> Script {
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let mut next = move || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (x >> 33) as u32
    };
    let delay = 1 + (next() % 3) as usize;
    let fates: Vec<(bool, bool)> = (0..30)
        .map(|_| (next() % 100 < 35, next() % 2 == 0))
        .collect();
    let mut sc = Script::new(90, move |step, plan| {
        let (lose, no_verdict) = fates[step % fates.len()];
        Fate {
            lose: lose && step < 30 && plan.kind != PicKind::Idr,
            no_verdict,
            ..Fate::default()
        }
    });
    sc.delay = delay;
    sc
}

fn startup_epoch_checks(seed: u64) -> Result<Report, String> {
    let mut sc = startup_epoch(seed);
    let mut sim = Sim::new();
    sim.run(&mut sc, false);
    if !sim.log.iter().any(|l| l.fed == Some(true)) {
        return Ok(Report::default());
    }
    check(&sim.log, &|_| {})
}

#[test]
fn startup_epochs_that_broke_the_oldest_victim_rule_decode() {
    // Found by sweeping startup epochs while a mark still named the oldest
    // short-term picture: a re-root after non-reference recovery frames
    // forgot a lost reference frame libavcodec had never inferred ("unref
    // short failure", an overfull DPB, a rejected access unit).
    for seed in [294, 2550, 5017] {
        let r = startup_epoch_checks(seed).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert!(r.marks >= 1 && r.non_ref >= 1, "seed {seed}: {r:?}");
    }
}

/// Brute force over 20000 startup epochs. Every
/// epoch is fed to libavcodec (explode) and replayed through a model that
/// infers gap frames the way libavcodec does; no MMCO 1 may name a picture the
/// client was never fed, and nothing may fail to decode. Slow, so not in the
/// default run: `cargo test -p ghostframe-lib --lib startup_epoch_sweep --
/// --ignored --nocapture`.
#[test]
#[ignore = "brute force; run by hand"]
fn startup_epoch_sweep() {
    let n: u64 = 20_000;
    let p = pcm_params();
    let (mut bad_victims, mut failures, mut mmco1) = (0u64, 0u64, 0u64);
    for seed in 0..n {
        let mut sc = startup_epoch(seed);
        let mut sim = Sim::new();
        sim.run(&mut sc, false);
        let mut model = DecoderModel::new(p).with_gap_mode(GapMode::Libavcodec);
        for l in sim.log.iter().filter(|l| l.fed == Some(true)) {
            let eff = model.push(l.plan.frame_seq, &l.header);
            mmco1 += eff.forgotten.len() as u64;
            bad_victims += eff
                .forgotten
                .iter()
                .filter(|f| !matches!(f, RefPic::Real(_)))
                .count() as u64;
        }
        if let Err(e) = startup_epoch_checks(seed) {
            failures += 1;
            eprintln!("seed {seed}: {e}");
        }
    }
    eprintln!(
        "{n} epochs: {mmco1} MMCO 1 fed, {bad_victims} bad victims, {failures} libavcodec failures"
    );
    assert_eq!((bad_victims, failures), (0, 0));
    assert!(mmco1 > 0, "premise: no MMCO 1 reached the client");
}
