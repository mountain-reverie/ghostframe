// SPDX-License-Identifier: AGPL-3.0-or-later
//! T1 for the `RefManager` in observed mode: a closed loop of manager,
//! [`Openh264Sim`] (the backend), network and client.
//!
//! The client is the real [`H264Gate`] (the one `ClientCore` runs; flags and
//! `ref_frame_seq` only) in front of a [`DecoderModel`] fed the real slice
//! headers; a frame the gate admits must be read by the model as the picture
//! the wire named. That is what catches a
//! mark the manager mislocated, a slot it guessed, or a recovery request the
//! library answered with something else. The sim is advisory (it varies the
//! long-term index and the marked frame with a seed), so no property here may
//! depend on where the library puts a mark.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use ghostframe_client_core::{H264Gate, Verdict};
use ghostframe_protocol::protocol::{FrameHeader, FRAME_FLAG_IDR, NO_ACK, REF_FRAME_NONE};
use proptest::prelude::*;

use super::backend::BackendFeedback;
use super::openh264_sim::{
    fn_before, MarkShape, Openh264Sim, SimFrame, SimKind, SimPolicy, SimStats, MARK_PERIOD,
};
use super::refs::*;
use super::syntax::{DecoderModel, StreamParams};
use super::tracker::PicKind;

const STEP: Duration = Duration::from_millis(10);
const DELAY: usize = 2;

fn config() -> RefConfig {
    RefConfig {
        // Replaced by the stream's own at its first IDR.
        params: super::syntax::StreamParams::writer(),
        frame_interval: STEP,
        initial_srtt: STEP * 4,
        observed: false,
    }
}

/// What the loop drives as the encoder: the sim, or the real library.
pub(super) trait LoopEngine {
    fn feedback(&mut self, fb: BackendFeedback);
    /// The next picture, for the manager's plan.
    fn encode(&mut self, step: usize, plan: &FramePlan) -> SimFrame;
    /// The library makes an IDR at the next frame without being asked.
    fn make_idr_next(&mut self) {
        panic!("this engine cannot make an IDR on its own");
    }
    /// The parameter sets of the newest IDR.
    fn params(&self) -> StreamParams;
    fn sim_stats(&self) -> SimStats {
        SimStats::default()
    }
}

struct SimLoop(Openh264Sim);

impl LoopEngine for SimLoop {
    fn feedback(&mut self, fb: BackendFeedback) {
        self.0.feedback(fb);
    }
    fn encode(&mut self, _step: usize, plan: &FramePlan) -> SimFrame {
        self.0.encode(plan.kind == PicKind::Idr)
    }
    fn make_idr_next(&mut self) {
        self.0.make_idr_next();
    }
    fn params(&self) -> StreamParams {
        self.0.params
    }
    fn sim_stats(&self) -> SimStats {
        self.0.stats
    }
}

enum Ev {
    Deliver(usize),
    Decoded(u32),
    Refused(u32),
    Verdict(u32),
    FragAck(u32),
}

#[derive(Clone)]
pub(super) struct Logged {
    pub step: usize,
    pub seq: u32,
    pub plan: FramePlan,
    pub sim: SimFrame,
    /// The IDR was not asked for.
    pub unrequested_idr: bool,
    pub lost: bool,
    pub fed: Option<bool>,
}

type LoseFn = Box<dyn FnMut(usize, &Logged) -> bool>;
type VerdictFn = Box<dyn FnMut(&Logged) -> bool>;

pub(super) struct Script {
    pub steps: usize,
    pub policy: SimPolicy,
    /// Loses the frame made at this step (never an IDR unless `lose_idr`).
    pub lose: LoseFn,
    /// The library makes an IDR at the frame made at these steps.
    pub own_idr_at: Vec<usize>,
    /// Frames made at these steps are encoded and then dropped locally
    /// (`frame_dropped`): the library's chain has them, the client never will.
    pub dropped_at: Vec<usize>,
    /// Frames to delay each acknowledgement by, on top of `DELAY`.
    pub ack_extra: Box<dyn FnMut(usize) -> usize>,
    pub lose_idr: bool,
    /// Frames the ledger calls lost (a verdict) although they arrive and are
    /// acknowledged, as a datagram still in the queue is.
    pub verdict_only: Option<VerdictFn>,
}

impl Script {
    pub(super) fn new(steps: usize, lose: impl FnMut(usize, &Logged) -> bool + 'static) -> Self {
        Self {
            steps,
            policy: SimPolicy::FAITHFUL,
            lose: Box::new(lose),
            own_idr_at: Vec::new(),
            dropped_at: Vec::new(),
            ack_extra: Box::new(|_| 0),
            lose_idr: false,
            verdict_only: None,
        }
    }
}

pub(super) struct Run {
    pub mgr: RefManager,
    pub log: Vec<Logged>,
    /// Every feedback event the manager produced, with the step.
    pub events: Vec<(usize, BackendFeedback)>,
    pub holds: usize,
    pub engine: Box<dyn LoopEngine>,
    /// `idr_pic_id` of the newest IDR the engine made.
    pub idr_pic_id: u32,
    client: DecoderModel,
    gate: H264Gate,
    queue: Vec<(usize, Ev)>,
    base: Instant,
}

/// Mutations of the manager's behaviour are made in the manager under test;
/// the properties are checked here against the sim and the client.
pub(super) fn run(sc: Script) -> Run {
    let engine = Box::new(SimLoop(Openh264Sim::new(sc.policy)));
    run_with(sc, engine)
}

/// The loop over any engine (the sim, or the library).
pub(super) fn run_with(sc: Script, engine: Box<dyn LoopEngine>) -> Run {
    let mut sc = sc;
    let base = Instant::now();
    let mut r = Run {
        mgr: RefManager::new(config()),
        log: Vec::new(),
        events: Vec::new(),
        holds: 0,
        engine,
        idr_pic_id: 0,
        client: DecoderModel::new(super::openh264_sim::sim_params()),
        gate: H264Gate::new(),
        queue: Vec::new(),
        base,
    };
    let mut seq = 100u32;
    // (idr_pic_id, marked_fn) of the marks fed back, and the carriers' frames
    // the client decoded: the properties of the events.
    let mut fed_marks: HashSet<(u32, u32)> = HashSet::new();
    for step in 0..sc.steps {
        let now = base + STEP * step as u32;
        let mut due = Vec::new();
        let mut later = Vec::new();
        for e in r.queue.drain(..) {
            if e.0 <= step {
                due.push(e);
            } else {
                later.push(e);
            }
        }
        r.queue = later;
        due.sort_by_key(|e| e.0);
        for (_, ev) in due {
            match ev {
                Ev::Deliver(i) => r.deliver(i, step, &mut sc),
                Ev::Decoded(s) => r.mgr.frame_decoded(s),
                Ev::Refused(s) => r.mgr.frame_refused(s),
                Ev::Verdict(s) => r.mgr.frame_suspect(s),
                Ev::FragAck(s) => {
                    r.mgr.frag_acked(s, 0, now);
                }
            }
        }
        r.mgr.tick(now, STEP * 4);
        seq += 1;
        let plan = match r.mgr.plan_next(seq, now) {
            Plan::Frame(p) => p,
            Plan::Hold => {
                r.holds += 1;
                continue;
            }
        };
        // The events the plan produced reach the backend before the frame.
        let events = r.mgr.take_backend_feedback();
        for fb in &events {
            r.events.push((step, *fb));
            check_event(&r, &mut fed_marks, fb);
            r.engine.feedback(*fb);
        }
        if sc.own_idr_at.contains(&step) {
            r.engine.make_idr_next();
        }
        let requested_idr = plan.kind == PicKind::Idr;
        let frame = r.engine.encode(step, &plan);
        let header = frame.header.clone();
        if header.idr {
            r.idr_pic_id = header.idr_pic_id;
            r.mgr.adopt_stream_params(r.engine.params());
        }
        let observed = r.mgr.observe(&plan, &header);
        let dropped = sc.dropped_at.contains(&step) && !header.idr;
        let eff = if dropped {
            r.mgr.frame_dropped(&plan, &header, now);
            observed
        } else {
            let report = r
                .mgr
                .frame_sent(&plan, &header, 1, now)
                .expect("a plan from the current epoch");
            assert!(
                report.matched,
                "step {step}: {:?} did not match: {:?}",
                frame.kind, report.effective
            );
            assert_eq!(report.plan, observed, "observe is what frame_sent applies");
            report.plan
        };
        let unrequested_idr = header.idr && !requested_idr;
        let mut l = Logged {
            step,
            seq,
            plan: eff,
            sim: frame,
            unrequested_idr,
            lost: false,
            fed: None,
        };
        l.lost = ((sc.lose)(step, &l) && (!header.idr || sc.lose_idr)) || dropped;
        let lost = l.lost;
        let is_idr = header.idr;
        let verdict_only = !lost && sc.verdict_only.as_mut().is_some_and(|f| f(&l));
        r.log.push(l);
        if verdict_only {
            r.queue.push((step + DELAY + 1, Ev::Verdict(seq)));
        }
        if lost {
            r.queue.push((step + 2 * DELAY + 2, Ev::Verdict(seq)));
        } else {
            if is_idr {
                r.queue.push((step + DELAY, Ev::FragAck(seq)));
            }
            let extra = (sc.ack_extra)(step);
            r.queue
                .push((step + DELAY + extra, Ev::Deliver(r.log.len() - 1)));
        }
    }
    r
}

/// Properties of one event the manager produced.
fn check_event(r: &Run, fed_marks: &mut HashSet<(u32, u32)>, fb: &BackendFeedback) {
    let idr_pic = r.idr_pic_id;
    match *fb {
        BackendFeedback::LtrMarked {
            idr_pic_id,
            frame_num,
        } => {
            assert_eq!(idr_pic_id, idr_pic, "feedback names the IDR period");
            // P3: confirmation needs the carrier: some logged mark with this
            // marked picture whose carrier the client decoded.
            let ok = r.log.iter().rev().any(|l| {
                l.sim.mark != MarkShape::None
                    && l.fed == Some(true)
                    && marked_fn_of(l) == Some(frame_num)
            });
            assert!(ok, "LtrMarked {frame_num} before its carrier was decoded");
            fed_marks.insert((idr_pic_id, frame_num));
        }
        BackendFeedback::LtrMarkFailed { frame_num, .. } => {
            // Only for a mark that was made.
            assert!(
                r.log
                    .iter()
                    .rev()
                    .any(|l| l.sim.mark != MarkShape::None && marked_fn_of(l) == Some(frame_num)),
                "LtrMarkFailed {frame_num} names no mark"
            );
        }
        BackendFeedback::LtrRecover {
            last_correct_frame_num: lc,
            current_frame_num: cur,
            ..
        } => {
            // P2: only a fed-back mark (or the IDR) is named, by its CARRIER.
            let named_a_fed_carrier = lc == 0
                || r.log.iter().rev().any(|l| {
                    l.sim.mark != MarkShape::None
                        && l.sim.header.frame_num == lc
                        && fed_marks.iter().any(|&(_, m)| Some(m) == marked_fn_of(l))
                });
            assert!(
                named_a_fed_carrier,
                "LtrRecover names {lc}: not the carrier of a fed-back mark"
            );
            // The newest frame number as it stands, wrap included: the
            // library compares modulo `MaxFrameNum` and drops a request whose
            // current is behind its last recovery's.
            let newest = r.log.last().map_or(0, |l| l.sim.header.frame_num);
            assert_eq!(cur, newest, "current is the newest frame number encoded");
        }
    }
}

fn marked_fn_of(l: &Logged) -> Option<u32> {
    match l.sim.mark {
        MarkShape::None => None,
        MarkShape::Mmco6 => Some(l.sim.header.frame_num),
        MarkShape::Mmco3 => Some((l.sim.header.frame_num + 0x8000 - 1) & 0x7fff),
    }
}

impl Run {
    fn deliver(&mut self, i: usize, step: usize, sc: &mut Script) {
        let l = &self.log[i];
        let w = l.plan.wire_info(None);
        let verdict = self.gate.on_frame(
            &FrameHeader {
                frame_seq: l.seq,
                frag_idx: 0,
                frag_total: 1,
                wire_seq: NO_ACK,
                timestamp_us: 0,
                flags: w.flags,
                slots: w.slots,
                ref_frame_seq: w.ref_frame_seq,
            },
            step as u64 * STEP.as_micros() as u64,
        );
        let fed = matches!(verdict, Verdict::Feed { .. });
        // The client never gets to ask for an IDR: the stream heals first.
        assert!(
            self.gate.take_request().is_none(),
            "the gate asked for an IDR at step {step}"
        );
        if fed {
            let eff = self.client.push(l.seq, &l.sim.header);
            assert!(
                !eff.violations.any(),
                "client violation at seq {} ({:?}): {:?}\nplan {:?}\nheader {:?}",
                l.seq,
                l.sim.kind,
                eff.violations,
                l.plan,
                l.sim.header
            );
            if w.ref_frame_seq != REF_FRAME_NONE {
                // The gate (wire only) admitted it; the header must really
                // read the picture the wire named.
                assert!(
                    eff.references(w.ref_frame_seq),
                    "gate admitted seq {} for ref {}, header reads {:?}\nplan {:?}\nheader {:?}",
                    l.seq,
                    w.ref_frame_seq,
                    eff.reference,
                    l.plan,
                    l.sim.header
                );
            }
        }
        let seq = l.seq;
        self.log[i].fed = Some(fed);
        let extra = (sc.ack_extra)(step);
        self.queue.push((
            step + DELAY + extra,
            if fed {
                Ev::Decoded(seq)
            } else {
                Ev::Refused(seq)
            },
        ));
    }

    /// Longest run of consecutive frames (in the order made) the client did
    /// not feed, and the step it ended at.
    pub(super) fn longest_unfed_run(&self) -> usize {
        let mut best = 0;
        let mut cur = 0;
        for l in &self.log {
            if l.fed == Some(false) || l.lost {
                cur += 1;
                best = best.max(cur);
            } else if l.fed == Some(true) {
                cur = 0;
            }
        }
        best
    }

    pub(super) fn idr_headers(&self) -> usize {
        self.log.iter().filter(|l| l.sim.header.idr).count()
    }

    pub(super) fn kinds(&self) -> BTreeMap<&'static str, usize> {
        let mut m = BTreeMap::new();
        for l in &self.log {
            let k = match l.sim.kind {
                SimKind::Idr => "idr",
                SimKind::Chain => "chain",
                SimKind::FromLt => "from_lt",
            };
            *m.entry(k).or_insert(0) += 1;
        }
        m
    }
}

/// The IDR bookkeeping identity: every IDR the stream made is one of the
/// manager's causes, and the unrequested ones are exactly the fallbacks.
pub(super) fn assert_idr_accounting(r: &Run) {
    let st = r.mgr.stats();
    let counted = st.idr_session
        + st.idr_resolution
        + st.idr_client_reset
        + st.idr_encoder_fault
        + st.idr_mtu_shrink
        + st.idr_backend_fallback
        // A re-sent IDR is the same cause again.
        ;
    assert_eq!(
        counted as usize,
        r.idr_headers(),
        "IDR headers vs counted causes: {st:?}"
    );
    let unrequested = r.log.iter().filter(|l| l.unrequested_idr).count();
    assert_eq!(
        st.idr_backend_fallback as usize, unrequested,
        "backend fallbacks are exactly the IDRs nobody asked for"
    );
}

/// After the last loss and a settling time, every frame reaches the client.
pub(super) fn assert_recovered(r: &Run, last_loss_step: usize, settle: usize) {
    for l in r.log.iter().filter(|l| l.step > last_loss_step + settle) {
        if l.step + 2 * DELAY + 2 >= r.log.last().map_or(0, |x| x.step) {
            continue;
        }
        assert_eq!(
            l.fed,
            Some(true),
            "step {} ({:?}) not fed {settle} frames after the last loss at {last_loss_step}",
            l.step,
            l.sim.kind
        );
    }
}

/// After the last loss a mark is confirmed within a few mark periods: the
/// library marks again (the failed marks were reported, nothing stays
/// unresolved), so anchors do not grow old. Skipped, and said so by the
/// return value, when the run ends before the deadline.
pub(super) fn assert_marks_resume(r: &Run, total_steps: usize) -> bool {
    let Some(last) = r.log.iter().filter(|l| l.lost).map(|l| l.step).max() else {
        return false;
    };
    let deadline = last + 4 * MARK_PERIOD as usize + 30;
    if deadline >= total_steps {
        return false;
    }
    assert!(
        r.events.iter().any(|(s, e)| *s > last
            && *s <= deadline
            && matches!(e, BackendFeedback::LtrMarked { .. })),
        "no mark confirmed between the last loss at step {last} and step {deadline}"
    );
    true
}

/// The events of one kind the manager produced.
pub(super) fn count_events(r: &Run, f: impl Fn(&BackendFeedback) -> bool) -> usize {
    r.events.iter().filter(|(_, e)| f(e)).count()
}

// ------------------------------------------------------------------ scenarios

#[test]
fn a_lossless_stream_marks_confirms_and_feeds_back_every_mark() {
    let r = run(Script::new(400, |_, _| false));
    let st = r.mgr.stats();
    assert_eq!(st.idr_session, 1);
    assert_eq!(st.idr_backend_fallback, 0);
    assert_eq!(r.idr_headers(), 1);
    // Premise: marks happen, are seen, confirmed and fed back.
    assert!(st.marks_observed >= 10, "{st:?}");
    assert_eq!(st.mark_unrepresentable, 0);
    assert!(st.p_anchors_confirmed >= 10, "{st:?}");
    let marked = r
        .events
        .iter()
        .filter(|(_, e)| matches!(e, BackendFeedback::LtrMarked { .. }))
        .count();
    assert_eq!(marked as u64, st.p_anchors_confirmed);
    assert_eq!(st.recoveries, 0);
    assert!(r.log.iter().all(|l| l.fed == Some(true) || l.step > 390));
    // Both mark shapes were exercised, and the MMCO 3 ones went on the wire as
    // a mark on the target.
    let m3 = r
        .log
        .iter()
        .filter(|l| l.sim.mark == MarkShape::Mmco3)
        .count();
    let m6 = r
        .log
        .iter()
        .filter(|l| l.sim.mark == MarkShape::Mmco6)
        .count();
    assert!(m3 >= 8 && m6 == 1, "mmco3 {m3} mmco6 {m6}");
    for l in &r.log {
        match l.sim.mark {
            MarkShape::Mmco3 => assert!(l.plan.mark.is_some() && l.plan.mark_target),
            MarkShape::Mmco6 => assert!(l.plan.mark.is_some() && !l.plan.mark_target),
            MarkShape::None => assert!(l.plan.mark.is_none() || l.sim.header.idr),
        }
    }
    assert_idr_accounting(&r);
}

#[test]
fn a_loss_burst_is_recovered_from_the_confirmed_long_term_picture_without_an_idr() {
    let r = run(Script::new(300, |step, _| (100..=104).contains(&step)));
    let st = r.mgr.stats();
    assert!(st.recoveries >= 1, "premise: a recovery happened: {st:?}");
    assert!(st.ltr_recoveries_requested >= 1);
    assert_eq!(r.idr_headers(), 1, "no IDR: {st:?}");
    assert_eq!(st.idr_backend_fallback, 0);
    assert!(st.reroots >= 1, "the sim answered with a P from the LT");
    assert!(r.engine.sim_stats().recoveries >= 1);
    assert_recovered(&r, 104, 40);
    assert_idr_accounting(&r);
}

#[test]
fn a_second_loss_is_recovered_with_a_higher_current_frame_number() {
    let r = run(Script::new(400, |step, _| {
        (100..=103).contains(&step) || (130..=133).contains(&step)
    }));
    let st = r.mgr.stats();
    assert!(st.ltr_recoveries_requested >= 2, "premise: {st:?}");
    assert_eq!(
        r.engine.sim_stats().stale_ignored,
        0,
        "no request was ignored as stale"
    );
    assert_eq!(r.idr_headers(), 1);
    assert_recovered(&r, 133, 40);
    // The currents never go back within an IDR epoch (modulo MaxFrameNum).
    let max = r.engine.params().max_frame_num();
    let curs: Vec<u32> = r
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            BackendFeedback::LtrRecover {
                current_frame_num, ..
            } => Some(*current_frame_num),
            _ => None,
        })
        .collect();
    assert!(
        curs.windows(2).all(|w| !fn_before(w[1], w[0], max)),
        "{curs:?}"
    );
}

#[test]
fn a_lost_carrier_is_reported_failed_and_the_library_marks_again() {
    // Lose the carrier of the second mark (the first MMCO 3 one).
    let mut lost_one = false;
    let r = run(Script::new(300, move |_, l| {
        if l.sim.mark == MarkShape::Mmco3 && !lost_one {
            lost_one = true;
            return true;
        }
        false
    }));
    let st = r.mgr.stats();
    assert_eq!(st.ltr_mark_failed_sent, 1, "premise: {st:?}");
    let failed_step = r
        .events
        .iter()
        .find(|(_, e)| matches!(e, BackendFeedback::LtrMarkFailed { .. }))
        .map(|(s, _)| *s)
        .expect("a failed event");
    // The library marked again and the new mark was confirmed.
    let remarks = r
        .log
        .iter()
        .filter(|l| l.step > failed_step && l.sim.mark != MarkShape::None)
        .count();
    assert!(remarks >= 2, "marks after the failure: {remarks}");
    assert!(st.p_anchors_confirmed >= 4, "{st:?}");
    assert_eq!(r.idr_headers(), 1);
    assert_idr_accounting(&r);
}

#[test]
fn an_idr_nobody_asked_for_is_a_new_epoch_counted_once_and_held_until_acked() {
    let mut sc = Script::new(200, |_, _| false);
    sc.own_idr_at = vec![100];
    let r = run(sc);
    let st = r.mgr.stats();
    assert_eq!(st.idr_backend_fallback, 1, "{st:?}");
    assert_eq!(st.idr_session, 1);
    assert_eq!(r.idr_headers(), 2);
    assert!(r.holds >= 1, "P frames held while the IDR was delivered");
    assert_eq!(r.mgr.epoch(), 2, "session start and the fallback");
    let idr = r.log.iter().find(|l| l.unrequested_idr).expect("the IDR");
    assert_eq!(idr.plan.idr_cause, Some(IdrCause::BackendFallback));
    assert!(idr.plan.wire_info(None).flags & FRAME_FLAG_IDR != 0);
    // The stream goes on from it: everything after is fed.
    assert!(r
        .log
        .iter()
        .filter(|l| l.step > 100)
        .all(|l| l.fed == Some(true) || l.step > 190));
    assert_idr_accounting(&r);
}

#[test]
fn feedback_of_an_old_epoch_never_reaches_the_library() {
    let r = run(Script::new(120, |_, _| false));
    let mut m = r.mgr;
    // Queue events (a confirmation) and then change epoch.
    m.client_reset(ResetReason::DecoderFailure);
    assert!(m.take_backend_feedback().is_empty());
    m.backend_fallback();
    assert!(m.take_backend_feedback().is_empty());
    assert!(!m.is_observed(), "the next backend says what it is");
}

/// `h264_refs_frame_lost` in observed mode: the frame the bridge could not
/// send is a loss. The manager asks the library for a recovery from the
/// confirmed long-term picture; it does not start over with an IDR.
#[test]
fn a_frame_encoded_and_dropped_locally_is_recovered_not_restarted() {
    let mut sc = Script::new(240, |_, _| false);
    sc.dropped_at = vec![100, 101];
    let r = run(sc);
    let st = r.mgr.stats();
    assert!(r.log.iter().filter(|l| l.lost).count() == 2, "premise");
    assert!(st.ltr_recoveries_requested >= 1, "{st:?}");
    assert!(st.reroots >= 1);
    assert_eq!(r.idr_headers(), 1, "no IDR: {st:?}");
    assert_recovered(&r, 102, 40);
    assert_idr_accounting(&r);
}

#[test]
fn a_lost_idr_is_sent_again_and_the_stream_goes_on_from_it() {
    let mut sc = Script::new(260, |step, _| step == 0);
    sc.lose_idr = true;
    let r = run(sc);
    let st = r.mgr.stats();
    assert!(
        st.idr_reissued >= 1,
        "premise: the IDR was sent again: {st:?}"
    );
    assert_eq!(st.idr_session as usize, r.idr_headers());
    assert!(r.holds >= 1);
    assert_recovered(&r, 1, 100);
    assert_idr_accounting(&r);
}

#[test]
fn a_mark_the_wire_cannot_carry_is_counted_and_reported_failed_so_marking_goes_on() {
    // A mark that falls on a recovery frame is the unrepresentable case.
    let mut found = None;
    'search: for at in 50..130usize {
        for seed in 0..12u64 {
            let mut sc = Script::new(300, move |step, _| (at..at + 3).contains(&step));
            sc.policy = SimPolicy {
                seed: Some(seed + 1),
                mark_on_recovery: true,
            };
            let r = run(sc);
            if r.mgr.stats().mark_unrepresentable > 0 {
                found = Some(r);
                break 'search;
            }
        }
    }
    let r = found.expect("premise: a seed puts a mark on a recovery frame");
    let st = r.mgr.stats();
    assert_eq!(st.mark_unrepresentable, 1, "{st:?}");
    assert!(st.ltr_mark_failed_sent >= 1);
    let at = r
        .log
        .iter()
        .position(|l| l.sim.mark != MarkShape::None && l.plan.mark.is_none() && !l.sim.header.idr)
        .expect("the unrepresentable mark");
    assert!(
        r.log[at + 1..]
            .iter()
            .any(|l| l.sim.mark != MarkShape::None && l.plan.mark.is_some()),
        "the library marked again"
    );
    // It never became an anchor.
    assert!(r.log[at].plan.mark.is_none());
}

// ------------------------------------------------------------------ proptest

/// (seed, loss bursts, reorder windows, mark on recovery frames, jitter, ack seed)
type Case = (
    u64,
    Vec<(usize, usize)>,
    Vec<(usize, usize)>,
    bool,
    usize,
    u64,
);

fn arb_script() -> impl Strategy<Value = Case> {
    (
        any::<u64>(),
        proptest::collection::vec((1usize..420, 1usize..6), 0..4),
        proptest::collection::vec((40usize..420, 1usize..12), 0..3),
        any::<bool>(),
        1usize..4,
        any::<u64>(),
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(160))]

    /// Whatever slot the sim picks and whichever frame it marks, the wire
    /// stays truthful (the gate admits only what the header really
    /// references), recovery asks only for fed-back marks by their carrier,
    /// the IDR count is the causes plus the fallbacks, and the stream heals.
    #[test]
    fn observed_streams_stay_truthful_and_heal_over_random_loss(
        (seed, bursts, reorder, mark_on_recovery, jitter, ack_seed) in arb_script()
    ) {
        let bursts2 = bursts.clone();
        let mut sc = Script::new(700, move |step, _| {
            bursts2.iter().any(|&(at, len)| step >= at && step < at + len)
        });
        sc.policy = SimPolicy { seed: Some(seed), mark_on_recovery };
        let mut s = ack_seed | 1;
        let reorder2 = reorder.clone();
        sc.ack_extra = Box::new(move |step| {
            s ^= s << 13; s ^= s >> 7; s ^= s << 17;
            if reorder2.iter().any(|&(at, len)| step >= at && step < at + len) {
                (s % (jitter as u64 + 1)) as usize
            } else {
                0
            }
        });
        let r = run(sc);
        assert_idr_accounting(&r);
        let last = bursts
            .iter()
            .chain(reorder.iter())
            .map(|&(a, l)| a + l)
            .max()
            .unwrap_or(0);
        // A burst may take the only anchor with it: a re-sent IDR is allowed,
        // but then it is the same cause again and still accounted.
        assert_recovered(&r, last + jitter, 90);
        assert_marks_resume(&r, 700);
        let st = r.mgr.stats();
        prop_assert_eq!(st.plan_mismatch, 0);
    }
}

/// The proptest above proves nothing if its loop never recovers, never marks a
/// recovery frame, never loses a carrier: sweep the same generator over fixed
/// seeds and require every path to have run. The first seeds lose frames
/// around the first mark's carrier, the one place a failed mark can leave the
/// client without the index the library goes on to use.
#[test]
fn the_random_sweep_exercises_recoveries_failed_marks_and_unrepresentable_marks() {
    let mut totals = RefStats::default();
    let mut failed_events = 0;
    let mut first_carrier_lost = 0;
    let mut resumed = 0;
    let mut idr_resends = 0;
    let mut unrep_runs = 0;
    for seed in 0..120u64 {
        let at = if seed % 4 == 0 {
            26 + (seed as usize / 4) % 16
        } else {
            60 + (seed as usize * 7) % 200
        };
        let mut sc = Script::new(480, move |step, _| {
            (at..at + 3).contains(&step) || (at + 61..at + 63).contains(&step)
        });
        sc.policy = SimPolicy {
            seed: Some(seed + 1),
            mark_on_recovery: true,
        };
        let r = run(sc);
        assert_idr_accounting(&r);
        assert_recovered(&r, at + 63, 90);
        resumed += usize::from(assert_marks_resume(&r, 480));
        let s = r.mgr.stats();
        totals.recoveries += s.recoveries;
        totals.ltr_recoveries_requested += s.ltr_recoveries_requested;
        totals.mark_unrepresentable += s.mark_unrepresentable;
        totals.marks_observed += s.marks_observed;
        totals.reroots += s.reroots;
        // The reports the library actually got, not the counter beside them.
        failed_events += count_events(&r, |e| matches!(e, BackendFeedback::LtrMarkFailed { .. }));
        idr_resends += s.idr_reissued;
        if r.log
            .iter()
            .find(|l| l.sim.mark == MarkShape::Mmco6 && !l.sim.header.idr)
            .is_some_and(|l| l.lost)
        {
            first_carrier_lost += 1;
        }
        // A mark the wire cannot carry is reported failed, so the library goes
        // on marking: another mark follows it.
        if let Some(first) = r.log.iter().position(|l| {
            l.sim.mark != MarkShape::None && l.plan.mark.is_none() && !l.sim.header.idr
        }) {
            unrep_runs += 1;
            assert!(
                r.log[first + 1..]
                    .iter()
                    .any(|l| l.sim.mark != MarkShape::None && l.plan.mark.is_some()),
                "seed {seed}: no mark after the unrepresentable one at step {}",
                r.log[first].step
            );
        }
    }
    assert!(
        unrep_runs >= 3,
        "premise: runs with an unrepresentable mark"
    );
    eprintln!(
        "sweep totals: {totals:?} failed_events {failed_events} first_carrier_lost \
         {first_carrier_lost} resumed {resumed} idr_resends {idr_resends}"
    );
    assert!(totals.ltr_recoveries_requested >= 120, "{totals:?}");
    assert!(totals.reroots >= 100, "{totals:?}");
    assert!(failed_events >= 20, "{failed_events}");
    assert!(
        first_carrier_lost >= 5,
        "premise: runs that lose the first mark's carrier: {first_carrier_lost}"
    );
    assert!(
        resumed >= 60,
        "premise: marks_resume checked in {resumed} runs"
    );
    assert!(totals.mark_unrepresentable >= 3, "{totals:?}");
}

// ------------------------------------------------- the library's own rules

/// The first mark's carrier is lost. The library re-makes the mark into the
/// SAME long-term index (the index advances only on a success), so the client,
/// which never got the first one, is not given an MMCO 3 into an index it
/// lacks: no client violation, and marking goes on.
#[test]
fn a_lost_first_mark_is_made_again_into_its_own_index() {
    let mut lost_one = false;
    let r = run(Script::new(300, move |_, l| {
        if l.sim.mark == MarkShape::Mmco6 && !l.sim.header.idr && !lost_one {
            lost_one = true;
            return true;
        }
        false
    }));
    let marks: Vec<&Logged> = r
        .log
        .iter()
        .filter(|l| l.sim.mark != MarkShape::None)
        .collect();
    assert!(
        marks.len() >= 3,
        "premise: marks after the loss: {}",
        marks.len()
    );
    assert!(marks[0].lost, "premise: the first carrier was lost");
    assert_eq!(
        count_events(&r, |e| matches!(e, BackendFeedback::LtrMarkFailed { .. })),
        1,
        "the loss was reported"
    );
    assert_eq!(
        marks[1].sim.mark,
        MarkShape::Mmco6,
        "no mark has succeeded yet: the same shape again"
    );
    assert_eq!(marks[1].sim.mark_index(), marks[0].sim.mark_index());
    assert!(
        r.mgr.stats().p_anchors_confirmed >= 3,
        "{:?}",
        r.mgr.stats()
    );
    assert_eq!(r.idr_headers(), 1);
    assert!(assert_marks_resume(&r, 300));
    assert_idr_accounting(&r);
}

/// A suspect carrier is not yet a lost one: the ledger's verdict is a
/// hypothesis (most of them under loss are datagrams still queued), and the
/// decoded acknowledgement that follows confirms the mark.
#[test]
fn a_suspect_carrier_that_is_acknowledged_late_still_confirms_its_mark() {
    let mut seen_mark = false;
    let mut sc = Script::new(200, |_, _| false);
    // The carrier is judged lost (a verdict), and acknowledged after it.
    sc.verdict_only = Some(Box::new(move |l: &Logged| {
        let first = l.sim.mark == MarkShape::Mmco6 && !l.sim.header.idr && !seen_mark;
        seen_mark |= first;
        first
    }));
    let r = run(sc);
    let st = r.mgr.stats();
    assert!(
        st.suspect_overridden >= 1,
        "premise: a suspect was overridden: {st:?}"
    );
    assert_eq!(
        count_events(&r, |e| matches!(e, BackendFeedback::LtrMarkFailed { .. })),
        0,
        "{st:?}"
    );
    assert!(count_events(&r, |e| matches!(e, BackendFeedback::LtrMarked { .. })) >= 4);
    assert_eq!(st.late_confirm_ignored, 0, "{st:?}");
    assert_idr_accounting(&r);
}

/// The frame number wraps (32768 frames, about 18 minutes at 30 fps) with no
/// periodic IDR to reset it. Every request names the newest frame number as it
/// stands, and the library, which compares modulo `MaxFrameNum`, answers each.
#[test]
fn recoveries_after_the_frame_num_wrap_name_the_current_frame_number() {
    const STEPS: usize = 33_200;
    let r = run(Script::new(STEPS, |step, _| {
        [200, 32_700, 32_880, 32_890, 33_000, 33_010]
            .iter()
            .any(|&at| (at..at + 4).contains(&step))
    }));
    let currents: Vec<u32> = r
        .events
        .iter()
        .filter_map(|(_, e)| match e {
            BackendFeedback::LtrRecover {
                current_frame_num, ..
            } => Some(*current_frame_num),
            _ => None,
        })
        .collect();
    let before = currents.iter().filter(|&&c| c > 30_000).count();
    let after = currents.iter().filter(|&&c| c < 1_000).count();
    assert!(
        before >= 1 && after >= 2,
        "premise: requests either side of the wrap: {currents:?}"
    );
    assert_eq!(
        r.engine.sim_stats().stale_ignored,
        0,
        "the library ignored a request as stale: {currents:?}"
    );
    assert_eq!(r.idr_headers(), 1);
    assert_recovered(&r, 33_013, 60);
    assert_idr_accounting(&r);
}

/// The confirmation of a mark and the loss that needs recovering reach the
/// library in one batch: it latches the feedback for the next frame, so the
/// recovery is answered from the older anchor. That is a re-root like any
/// other, and the manager does not ask again.
pub(super) fn cobatched(at: usize, total: usize) -> Script {
    let mut sc = Script::new(total, move |step, _| step == at || step == at + 1);
    // The first mark's carrier (step 33) is acknowledged late, so that its
    // confirmation arrives with the verdict on the frames after it.
    sc.ack_extra = Box::new(|step| usize::from(step >= 30) * 3);
    sc
}

/// Whether a confirmation and a recovery naming it were sent in one batch.
pub(super) fn has_cobatch(r: &Run) -> bool {
    r.events.iter().any(|(s, e)| {
        let BackendFeedback::LtrMarked { frame_num: m, .. } = *e else {
            return false;
        };
        r.events.iter().any(|(s2, e2)| {
            matches!(*e2, BackendFeedback::LtrRecover { last_correct_frame_num: c, .. }
                if *s2 == *s && (c == m || c == (m + 1) % 0x8000))
        })
    })
}

#[test]
fn a_confirmation_and_a_recovery_in_one_batch_are_one_recovery() {
    let mut found = 0;
    for at in 30..46 {
        let r = run(cobatched(at, 200));
        if !has_cobatch(&r) {
            continue;
        }
        found += 1;
        let st = r.mgr.stats();
        assert_eq!(
            st.ltr_recoveries_requested, 1,
            "at {at}: asked again: {st:?}"
        );
        assert!(st.reroots >= 1, "at {at}: {st:?}");
        assert_eq!(r.idr_headers(), 1);
        assert_recovered(&r, at + 1, 40);
        assert_idr_accounting(&r);
    }
    assert!(
        found >= 1,
        "premise: a run batched the confirmation with the recovery"
    );
}

/// The bridge counts the IDRs it sent; an IDR the library made unasked and the
/// bridge then dropped locally never reached the wire, so it is not a counted
/// fallback (it is still a new epoch).
#[test]
fn an_unrequested_idr_dropped_locally_is_not_counted_as_sent() {
    for sent in [true, false] {
        let now = Instant::now();
        let mut m = RefManager::new(config());
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        let Plan::Frame(idr) = m.plan_next(100, now) else {
            panic!("the session IDR");
        };
        let f = sim.encode(true);
        m.adopt_stream_params(sim.params);
        m.frame_sent(&idr, &f.header, 1, now)
            .expect("current epoch");
        m.frag_acked(100, 0, now);
        let Plan::Frame(p) = m.plan_next(101, now + STEP) else {
            panic!("a P after the acknowledged IDR");
        };
        assert_ne!(p.kind, PicKind::Idr);
        sim.make_idr_next();
        let unasked = sim.encode(false);
        assert!(unasked.header.idr, "premise: the library made an IDR");
        if sent {
            m.frame_sent(&p, &unasked.header, 1, now + STEP);
        } else {
            m.frame_dropped(&p, &unasked.header, now + STEP);
        }
        assert_eq!(
            m.stats().idr_backend_fallback,
            u64::from(sent),
            "sent: {sent}"
        );
        assert_eq!(m.epoch(), 2, "a new epoch either way");
    }
}
