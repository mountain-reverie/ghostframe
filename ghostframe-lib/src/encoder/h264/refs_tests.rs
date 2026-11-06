//! T1 for the `RefManager`: a closed-loop simulation of server, network and
//! client, with the real slice-header writer and parser in the path.
//!
//! The client is the design-7.1 gate in front of a [`DecoderModel`]. The gate
//! knows only what the wire tells it (flags, `ref_frame_seq`, what it fed); the
//! model knows what the slice header really references. A frame the gate
//! admits must be read by the model as the picture the plan named: that is the
//! property that catches a wrong plan, an overwritten slot or a missing RPLM.

use std::collections::{BTreeMap, HashSet};
use std::time::{Duration, Instant};

use proptest::prelude::*;

use super::refs::*;
use super::syntax::*;
use super::tracker::{seq_lt, FrameState, PicKind};

pub(super) const STEP: Duration = Duration::from_millis(10);

pub(super) fn params() -> StreamParams {
    StreamParams::emitted()
}

fn config() -> RefConfig {
    RefConfig {
        params: params(),
        frame_interval: STEP,
        initial_srtt: STEP * 4,
    }
}

/// What the network and client do with one planned frame.
#[derive(Clone, Default)]
pub(super) struct Fate {
    pub(super) lose: bool,
    /// Extra delivery delay, in frames (reordering).
    pub(super) jitter: usize,
    /// Extra delay, in frames, on this frame's ack.
    pub(super) ack_extra: usize,
    /// The decoded/refused ack never comes (per-frame ack mode only).
    pub(super) suppress_ack: bool,
    /// A ledger verdict on a frame that did arrive.
    pub(super) spurious_verdict: bool,
    /// No verdict on a lost frame; only the decode deadline finds it.
    pub(super) no_verdict: bool,
}

pub(super) type FateFn = Box<dyn FnMut(usize, &FramePlan) -> Fate>;

pub(super) struct Script {
    pub(super) steps: usize,
    pub(super) delay: usize,
    pub(super) fate: FateFn,
    pub(super) srtt: Box<dyn FnMut(usize) -> Duration>,
    pub(super) resolution_at: Vec<usize>,
    pub(super) client_reset_at: Vec<usize>,
    pub(super) reentry_at: Vec<usize>,
    /// Frame sequence increment per step (tile frames share the counter).
    pub(super) seq_stride: u32,
    /// Acks are cumulative 0x0A masks instead of one event per frame.
    pub(super) cumulative: bool,
    pub(super) strip_rplm: bool,
    pub(super) strip_mmco1: bool,
    /// The client measures `frame_num` gaps the way libavcodec does.
    pub(super) lavc_gaps: bool,
    /// Fragments in an IDR; a lost-free IDR acknowledges them evenly between
    /// its first ack and its delivery.
    pub(super) idr_frags: u16,
}

impl Script {
    pub(super) fn new(steps: usize, fate: impl FnMut(usize, &FramePlan) -> Fate + 'static) -> Self {
        Self {
            steps,
            delay: 2,
            fate: Box::new(fate),
            srtt: Box::new(|_| STEP * 4),
            resolution_at: vec![],
            client_reset_at: vec![],
            reentry_at: vec![],
            seq_stride: 1,
            cumulative: false,
            strip_rplm: false,
            strip_mmco1: false,
            lavc_gaps: false,
            idr_frags: 1,
        }
    }

    pub(super) fn lossless(steps: usize) -> Self {
        Self::new(steps, |_, _| Fate::default())
    }
}

#[derive(Clone)]
pub(super) struct Logged {
    pub(super) step: usize,
    pub(super) plan: FramePlan,
    pub(super) lost: bool,
    /// `Some(fed)` once delivered.
    pub(super) fed: Option<bool>,
    /// The effective reference on the full stream matched the plan.
    pub(super) matched: bool,
    pub(super) effective: Effective,
    /// The slice header as the server emitted it (after any script mutation).
    pub(super) header: SliceHeader,
}

enum Ev {
    Deliver {
        plan: FramePlan,
        header: Box<SliceHeader>,
        fate: Fate,
    },
    Decoded(u32),
    Refused(u32),
    Ack {
        newest: u32,
        decoded: u64,
        refused: u64,
    },
    Verdict(u32),
    FragAck(u32, u16),
}

/// The client gate of design 7.1, from the wire alone.
#[derive(Default)]
struct Gate {
    good: HashSet<u32>,
    lt: [Option<u32>; 2],
    newest_ref: Option<u32>,
    last_fed: Option<u32>,
}

impl Gate {
    fn admit(&self, plan: &FramePlan) -> bool {
        if self.last_fed.is_some_and(|l| !seq_lt(l, plan.frame_seq)) {
            return false;
        }
        match plan.target {
            None => true,
            Some(RefTarget::Short { seq, .. }) => {
                self.good.contains(&seq) && self.newest_ref == Some(seq)
            }
            Some(RefTarget::Long { slot, seq }) => self.lt[usize::from(slot)] == Some(seq),
        }
    }

    fn feed(&mut self, plan: &FramePlan) {
        let seq = plan.frame_seq;
        self.last_fed = Some(seq);
        if plan.kind == PicKind::Idr {
            self.good.clear();
            self.lt = [Some(seq), None];
            self.newest_ref = Some(seq);
        } else if plan.nal_ref_idc != 0 {
            self.newest_ref = Some(seq);
            if let Some(k) = plan.mark {
                self.lt[usize::from(k)] = Some(seq);
            }
        }
        self.good.insert(seq);
    }
}

pub(super) struct Sim {
    pub(super) mgr: RefManager,
    client: DecoderModel,
    gate: Gate,
    handled: BTreeMap<u32, bool>,
    base: Instant,
    queue: Vec<(usize, Ev)>,
    pub(super) log: Vec<Logged>,
    pub(super) max_client_peak: usize,
    pub(super) max_enc_peak: usize,
    /// Assert the properties a correct stream has; the negative controls
    /// turn this off.
    strict: bool,
    /// Frames the manager held back while the IDR was being delivered.
    pub(super) holds: usize,
}

impl Sim {
    pub(super) fn new() -> Self {
        Self {
            mgr: RefManager::new(config()),
            client: DecoderModel::new(params()),
            gate: Gate::default(),
            handled: BTreeMap::new(),
            base: Instant::now(),
            queue: Vec::new(),
            log: Vec::new(),
            max_client_peak: 0,
            max_enc_peak: 0,
            strict: true,
            holds: 0,
        }
    }

    fn at(&self, step: usize) -> Instant {
        self.base + STEP * step as u32
    }

    fn deliver(
        &mut self,
        step: usize,
        sc: &Script,
        plan: &FramePlan,
        header: &SliceHeader,
        fate: &Fate,
    ) {
        let seq = plan.frame_seq;
        let fed = self.gate.admit(plan);
        if fed {
            self.gate.feed(plan);
            let eff = self.client.push(seq, header);
            self.max_client_peak = self.max_client_peak.max(eff.dpb_peak);
            if self.strict {
                // Every MMCO 1 must name a picture the client really holds,
                // not one a gap process may or may not have invented.
                assert!(
                    eff.forgotten.iter().all(|f| matches!(f, RefPic::Real(_))),
                    "frame {seq} forgets {:?}: plan {plan:?}",
                    eff.forgotten
                );
                assert!(
                    !eff.violations.any(),
                    "client violation at seq {seq}: {:?} plan {plan:?} client st {:?} lt0 {:?} lt1 {:?} marks {:?} slots {}",
                    eff.violations,
                    self.client.short_term(),
                    self.client.long_term(0),
                    self.client.long_term(1),
                    self.log
                        .iter()
                        .rev()
                        .take(14)
                        .map(|l| (l.plan.frame_seq, l.plan.kind, l.plan.mark, l.plan.frame_num, l.lost, l.fed, self.mgr.frame_state(l.plan.frame_seq)))
                        .collect::<Vec<_>>(),
                    self.mgr.debug_slots()
                );
                assert!(
                    eff.dpb_peak <= 4,
                    "client holds {} references",
                    eff.dpb_peak
                );
                // The gate (wire only) admitted it; the model (the header's real
                // meaning) must read the picture the plan named.
                if let Some(t) = plan.ref_frame_seq() {
                    assert!(
                        eff.references(t),
                        "gate admitted frame {seq} but its header reads {:?}, plan {plan:?}",
                        eff.reference
                    );
                }
            }
        }
        if let Some(l) = self.log.iter_mut().rev().find(|l| l.plan.frame_seq == seq) {
            l.fed = Some(fed);
        }
        self.handled.insert(seq, fed);
        let due = step + sc.delay + fate.ack_extra;
        if sc.cumulative {
            let newest = self
                .handled
                .keys()
                .copied()
                .fold(seq, |n, k| if seq_lt(n, k) { k } else { n });
            let (mut decoded, mut refused) = (0u64, 0u64);
            for i in 0..64u32 {
                match self.handled.get(&newest.wrapping_sub(i)) {
                    Some(true) => decoded |= 1 << i,
                    Some(false) => refused |= 1 << i,
                    None => {}
                }
            }
            self.queue.push((
                due,
                Ev::Ack {
                    newest,
                    decoded,
                    refused,
                },
            ));
        } else if !fate.suppress_ack {
            self.queue.push((
                due,
                if fed {
                    Ev::Decoded(seq)
                } else {
                    Ev::Refused(seq)
                },
            ));
        }
        if fate.spurious_verdict {
            self.queue.push((step + 2 * sc.delay, Ev::Verdict(seq)));
        }
    }

    pub(super) fn run(&mut self, sc: &mut Script, strict: bool) {
        self.strict = strict;
        if sc.lavc_gaps {
            self.client = DecoderModel::new(params()).with_gap_mode(GapMode::Libavcodec);
        }
        let p = params();
        let mut seq = 100u32;
        let mut last_ref_fn: Option<u32> = None;
        for step in 0..sc.steps {
            let now = self.at(step);
            if sc.resolution_at.contains(&step) {
                self.mgr.resolution_change();
            }
            if sc.client_reset_at.contains(&step) {
                self.mgr.client_reset(ResetReason::DecoderFailure);
            }
            if sc.reentry_at.contains(&step) {
                self.mgr.mode_reentry();
            }
            // Due events, in due order (stable: ties keep scheduling order).
            let mut due: Vec<(usize, Ev)> = Vec::new();
            let mut later = Vec::new();
            for e in self.queue.drain(..) {
                if e.0 <= step {
                    due.push(e);
                } else {
                    later.push(e);
                }
            }
            self.queue = later;
            due.sort_by_key(|e| e.0);
            for (_, ev) in due {
                match ev {
                    Ev::Deliver { plan, header, fate } => {
                        self.deliver(step, sc, &plan, &header, &fate)
                    }
                    Ev::Decoded(s) => self.mgr.frame_decoded(s),
                    Ev::Refused(s) => self.mgr.frame_refused(s),
                    Ev::Ack {
                        newest,
                        decoded,
                        refused,
                    } => self.mgr.frame_ack(newest, decoded, refused),
                    Ev::Verdict(s) => self.mgr.frame_suspect(s),
                    Ev::FragAck(s, i) => {
                        self.mgr.frag_acked(s, i, now);
                    }
                }
            }

            self.mgr.tick(now, (sc.srtt)(step));
            seq += sc.seq_stride;
            let plan = match self.mgr.plan_next(seq, now) {
                Plan::Frame(p) => p,
                Plan::Hold => {
                    self.holds += 1;
                    continue;
                }
            };

            let mut header = plan.slice_header(&p);
            if sc.strip_rplm {
                header.rplm.clear();
            }
            if sc.strip_mmco1 {
                if let Some(ops) = &mut header.mmco {
                    ops.retain(|o| !matches!(o, Mmco::ForgetShort(_)));
                }
            }
            let nal = slice_nal(&p, &header);
            let (parsed, _) = parse_slice_header(&p, &nal).expect("writer output parses");
            assert_eq!(parsed, header, "writer/parser round trip");

            // Properties of the plan itself.
            if strict {
                if plan.kind == PicKind::Idr {
                    last_ref_fn = Some(0);
                } else {
                    let expect = (last_ref_fn.expect("IDR first") + 1) % p.max_frame_num();
                    assert_eq!(plan.frame_num, expect, "P5: frame_num of {:?}", plan.kind);
                    if plan.nal_ref_idc != 0 {
                        last_ref_fn = Some(plan.frame_num);
                    }
                }
                assert_eq!(plan.nal_ref_idc == 0, plan.kind == PicKind::Recovery, "P5");
                if let (Some(k), Some(a)) = (plan.mark, self.mgr.anchor()) {
                    if plan.kind != PicKind::Idr {
                        assert_ne!(k, a.slot, "P2: marking into the confirmed anchor's slot");
                    }
                }
            }

            let epoch_before = self.mgr.epoch();
            let report = self
                .mgr
                .frame_sent(
                    &plan,
                    &parsed,
                    if plan.kind == PicKind::Idr {
                        sc.idr_frags
                    } else {
                        1
                    },
                    now,
                )
                .expect("a plan from the current epoch");
            self.max_enc_peak = self.max_enc_peak.max(report.effective.dpb_peak);
            if strict {
                assert!(
                    report.matched,
                    "step {step}: plan {plan:?} -> effective {:?}",
                    report.effective
                );
                assert!(report.effective.dpb_peak <= 4, "encoder-side occupancy");
                if self.mgr.epoch() == epoch_before {
                    // The manager's DPB and the model fed the same headers.
                    let refs = self.mgr.resident_refs();
                    let st: Vec<RefPic> = refs
                        .iter()
                        .filter(|r| r.lt_idx.is_none())
                        .map(|r| RefPic::Real(r.seq))
                        .collect();
                    assert_eq!(
                        st,
                        self.mgr.enc_model().short_term(),
                        "resident ST at step {step}"
                    );
                    for k in 0..2u8 {
                        let ours = refs
                            .iter()
                            .find(|r| r.lt_idx == Some(k))
                            .map(|r| RefPic::Real(r.seq));
                        assert_eq!(
                            ours,
                            self.mgr.enc_model().long_term(u32::from(k)),
                            "resident LT{k} at step {step}"
                        );
                    }
                }
            }

            let mut fate = (sc.fate)(step, &plan);
            if plan.kind == PicKind::Idr && !strict {
                fate.lose = false;
            }
            self.log.push(Logged {
                step,
                plan: plan.clone(),
                lost: fate.lose,
                fed: None,
                matched: report.matched,
                effective: report.effective,
                header: parsed.clone(),
            });
            if fate.lose {
                if !fate.no_verdict {
                    self.queue.push((step + 2 * sc.delay + 2, Ev::Verdict(seq)));
                }
            } else {
                if plan.kind == PicKind::Idr {
                    let n = usize::from(sc.idr_frags);
                    for i in 0..n {
                        let due = step + sc.delay + fate.jitter * (i + 1) / n;
                        self.queue.push((due, Ev::FragAck(seq, i as u16)));
                    }
                }
                self.queue.push((
                    step + sc.delay + fate.jitter,
                    Ev::Deliver {
                        plan,
                        header: Box::new(parsed),
                        fate,
                    },
                ));
            }
        }
    }

    fn kinds(&self) -> Vec<PicKind> {
        self.log.iter().map(|l| l.plan.kind).collect()
    }
}

pub(super) fn run_sim(mut sc: Script) -> Sim {
    let mut sim = Sim::new();
    sim.run(&mut sc, true);
    sim
}

/// Drive the manager by hand: plan, write, parse, send.
struct Hand {
    m: RefManager,
    t0: Instant,
    seq: u32,
}

impl Hand {
    fn new() -> Self {
        Self {
            m: RefManager::new(config()),
            t0: Instant::now(),
            seq: 0,
        }
    }

    fn at(&self, step: u32) -> Instant {
        self.t0 + STEP * step
    }

    fn send(&mut self, step: u32) -> FramePlan {
        self.seq += 1;
        let now = self.at(step);
        let plan = self.m.plan_next(self.seq, now).frame();
        let h = plan.slice_header(&params());
        let r = self.m.frame_sent(&plan, &h, 1, now).unwrap();
        assert!(r.matched, "{plan:?} {:?}", r.effective);
        if plan.kind == PicKind::Idr {
            // One fragment, transport-acked at once.
            self.m.frag_acked(plan.frame_seq, 0, now);
        }
        plan
    }
}

// ------------------------------------------------------------------ scenarios

#[test]
fn healthy_stream_alternates_the_two_slots_and_never_needs_recovery() {
    let sim = run_sim(Script::lossless(400));
    let st = sim.mgr.stats();
    assert_eq!(st.idr_session, 1);
    assert_eq!(st.recoveries, 0);
    assert!(sim.log.iter().all(|l| l.plan.kind != PicKind::Recovery));
    assert!(sim
        .log
        .iter()
        .skip(1)
        .all(|l| l.fed == Some(true) || l.step >= 398));
    let marks: Vec<u8> = sim
        .log
        .iter()
        .filter(|l| l.plan.kind != PicKind::Idr)
        .filter_map(|l| l.plan.mark)
        .collect();
    assert!(marks.len() >= 10, "marks happened: {marks:?}");
    assert_eq!(marks[0], 1);
    assert!(
        marks.windows(2).all(|w| w[0] != w[1]),
        "slots alternate: {marks:?}"
    );
    assert!(st.anchors_confirmed >= 10, "{st:?}");
    assert!(sim.max_client_peak <= 4 && sim.max_enc_peak <= 4);
    let refs = sim.mgr.resident_refs();
    assert!(
        refs.len() <= 4 && refs.iter().filter(|r| r.lt_idx.is_some()).count() == 2,
        "{refs:?}"
    );
}

#[test]
fn first_mark_after_idr_carries_mmco_4_1_6_and_steady_state_marks_drop_the_1() {
    let sim = run_sim(Script::lossless(120));
    let marks: Vec<&Logged> = sim
        .log
        .iter()
        .filter(|l| l.plan.mark.is_some() && l.plan.kind != PicKind::Idr)
        .collect();
    assert!(marks.len() >= 3);
    assert!(
        matches!(
            marks[0].plan.mmco.as_slice(),
            [
                Mmco::MaxLong(2),
                Mmco::ForgetShort(_),
                Mmco::CurrentToLong(1)
            ]
        ),
        "{:?}",
        marks[0].plan.mmco
    );
    // The second mark replaces the confirmed IDR in LT 0: no MMCO 1.
    assert_eq!(
        marks[1].plan.mmco,
        vec![Mmco::MaxLong(2), Mmco::CurrentToLong(0)]
    );
    assert_eq!(
        marks[2].plan.mmco,
        vec![Mmco::MaxLong(2), Mmco::CurrentToLong(1)]
    );
}

#[test]
fn without_mmco_1_the_first_mark_overflows_the_dpb() {
    let mut sc = Script::lossless(40);
    sc.strip_mmco1 = true;
    let mut sim = Sim::new();
    sim.run(&mut sc, false);
    let first = sim
        .log
        .iter()
        .find(|l| l.plan.mark.is_some() && l.plan.kind != PicKind::Idr)
        .unwrap();
    assert!(first.effective.violations.dpb_overflow);
    assert!(first.effective.dpb_peak > 4 && first.effective.dpb_len <= 4);
    assert!(!first.matched);
}

#[test]
fn a_mark_into_a_slot_the_client_may_lack_always_drops_the_oldest_short_term() {
    // The first candidate never reaches the client. The second goes into the
    // same slot, which the server still believes occupied: with the encoder's
    // count alone it would carry no MMCO 1 and leave the client one reference
    // over (1 LT + 3 ST + current). The sim fails hard on any client overflow.
    let mut lost = false;
    let sc = Script::new(150, move |_, plan| Fate {
        lose: plan.kind != PicKind::Idr
            && plan.mark.is_some()
            && !std::mem::replace(&mut lost, true),
        ..Fate::default()
    });
    let sim = run_sim(sc);
    let marks: Vec<&Logged> = sim
        .log
        .iter()
        .filter(|l| l.plan.mark == Some(1) && l.plan.kind != PicKind::Idr)
        .collect();
    assert!(marks[0].lost);
    assert!(
        marks[1]
            .plan
            .mmco
            .iter()
            .any(|o| matches!(o, Mmco::ForgetShort(_))),
        "{:?}",
        marks[1].plan.mmco
    );
    assert!(sim.max_client_peak <= 4);
}

#[test]
fn a_mark_never_names_a_short_term_picture_the_client_lost() {
    // Every frame but the IDR is lost for 80 steps. The client then holds the
    // IDR and, at best, "non-existing" frames for the gaps, which decoders do
    // not agree on (libavcodec invents none before a non-reference picture
    // that follows a gap), so an MMCO 1 naming a lost frame fails there. The
    // marks must wait for a picture that is not lost, and then resume.
    let sc = Script::new(260, |step, plan| Fate {
        lose: plan.kind != PicKind::Idr && (1..80).contains(&step),
        ..Fate::default()
    });
    let sim = run_sim(sc);
    let (mut named, mut named_after) = (0, 0);
    for l in &sim.log {
        for op in &l.plan.mmco {
            let Mmco::ForgetShort(d) = *op else { continue };
            // A mark that is itself lost never reaches a decoder.
            if l.lost {
                continue;
            }
            let fnum = (l.plan.frame_num + 128 - (d + 1)) % 128;
            let victim = sim
                .log
                .iter()
                .rev()
                .find(|v| v.step < l.step && v.plan.nal_ref_idc != 0 && v.plan.frame_num == fnum)
                .expect("the victim was sent");
            assert!(
                !victim.lost,
                "step {} drops lost frame {} (step {})",
                l.step, victim.plan.frame_seq, victim.step
            );
            named += 1;
            named_after += usize::from(l.step >= 80);
        }
    }
    assert!(
        named >= 1 && named_after >= 1,
        "premise: {named} drops, {named_after} after the loss"
    );
}

#[test]
fn no_rplm_diverges_exactly_where_the_default_list_differs() {
    let sim = run_sim(Script::lossless(80));
    assert!(sim.log.iter().all(|l| l.matched));
    let mut sc = Script::lossless(80);
    sc.strip_rplm = true;
    let mut bare = Sim::new();
    bare.run(&mut sc, false);
    let diverged: Vec<&Logged> = bare.log.iter().filter(|l| !l.matched).collect();
    assert!(
        !diverged.is_empty(),
        "the model must flag the stream without RPLM"
    );
    let first = diverged[0];
    let at = bare
        .log
        .iter()
        .position(|l| l.plan.frame_seq == first.plan.frame_seq)
        .unwrap();
    assert!(
        bare.log[at - 1].plan.mark.is_some(),
        "the first divergence follows a mark"
    );
    assert!(
        matches!(first.effective.reference, Some(RefPic::Real(s)) if Some(s) != first.plan.ref_frame_seq())
    );
}

#[test]
fn loss_recovers_from_the_anchor_then_rejoins_the_healthy_chain() {
    let mut lost_once = false;
    let sc = Script::new(160, move |step, plan| {
        let mut f = Fate::default();
        if step >= 60 && plan.kind == PicKind::Healthy && !lost_once {
            lost_once = true;
            f.lose = true;
        }
        f
    });
    let sim = run_sim(sc);
    let lost_at = sim.log.iter().position(|l| l.lost).unwrap();
    let after: Vec<&Logged> = sim.log[lost_at + 1..].iter().collect();
    let first_reroot = after
        .iter()
        .find(|l| l.plan.kind == PicKind::Reroot)
        .expect("a re-root");
    let Some(RefTarget::Long { seq, .. }) = first_reroot.plan.target else {
        panic!("re-root must name a long-term anchor");
    };
    // The anchor was decoded by the client before the loss.
    assert!(sim.log[..lost_at]
        .iter()
        .any(|l| l.plan.frame_seq == seq && l.fed == Some(true)));
    assert_eq!(first_reroot.fed, Some(true), "the client holds the anchor");
    assert!(after
        .iter()
        .all(|l| l.plan.kind != PicKind::Recovery || l.plan.nal_ref_idc == 0));
    assert!(sim.mgr.is_healthy());
    let tail: Vec<&Logged> = sim
        .log
        .iter()
        .filter(|l| (140..156).contains(&l.step))
        .collect();
    assert!(
        tail.iter()
            .all(|l| l.plan.kind == PicKind::Healthy && l.fed == Some(true)),
        "settled"
    );
    assert_eq!(
        sim.mgr.stats().idr_session + sim.mgr.stats().idr_client_reset,
        1,
        "no second IDR"
    );
    // Frames sent between the loss and the verdict were refused, not drifted.
    assert!(sim.log.iter().any(|l| l.fed == Some(false)));
}

#[test]
fn two_consecutive_losses_reroot_again_one_srtt_later() {
    let mut lost = 0;
    let sc = Script::new(200, move |step, plan| {
        let mut f = Fate::default();
        if step >= 60 && lost == 0 && plan.kind == PicKind::Healthy {
            lost = 1;
            f.lose = true;
        } else if lost == 1 && plan.kind == PicKind::Reroot {
            lost = 2;
            f.lose = true; // the first re-root dies too
        }
        f
    });
    let sim = run_sim(sc);
    let reroots: Vec<&Logged> = sim
        .log
        .iter()
        .filter(|l| l.plan.kind == PicKind::Reroot)
        .collect();
    assert!(reroots.len() >= 2);
    assert!(reroots[0].lost);
    assert!(reroots[1].step - reroots[0].step >= 4, "one srtt apart");
    assert_eq!(reroots[1].fed, Some(true));
    assert!(sim.mgr.is_healthy());
}

#[test]
fn a_slow_reroot_ack_does_not_trigger_a_second_reroot_or_a_stale_chain() {
    // The first re-root's ack takes longer than a re-root interval but less
    // than the decode deadline: no second re-root, the chain continues from it.
    let mut first_reroot_seen = false;
    let mut lost = false;
    let mut sc = Script::new(160, move |step, plan| {
        let mut f = Fate::default();
        if step >= 60 && !lost && plan.kind == PicKind::Healthy {
            lost = true;
            f.lose = true;
        }
        if plan.kind == PicKind::Reroot && !std::mem::replace(&mut first_reroot_seen, true) {
            f.ack_extra = 12;
        }
        f
    });
    sc.delay = 2;
    let sim = run_sim(sc);
    assert_eq!(sim.mgr.stats().reroots, 1, "{:?}", sim.mgr.stats());
    let exit = sim
        .log
        .iter()
        .position(|l| l.plan.kind == PicKind::Reroot)
        .unwrap();
    assert!(sim.log[exit + 1..]
        .iter()
        .filter(|l| l.plan.kind == PicKind::Healthy)
        .all(|l| l.fed == Some(true) || l.step >= 158));
}

#[test]
fn exit_from_recovery_waits_for_the_newest_reroot() {
    // R1's ack is so late that R1 passes its decode deadline, so R2 is sent;
    // R1's decoded bit then arrives while R2 is still in flight. The client's
    // newest reference is R2, so the chain must continue from R2, and only
    // once R2 is decoded: leaving on R1 would target a picture the gate
    // refuses (it is no longer the newest reference).
    let mut roots = 0;
    let mut lost = false;
    let sc = Script::new(200, move |step, plan| {
        let mut f = Fate::default();
        if step >= 60 && !lost && plan.kind == PicKind::Healthy {
            lost = true;
            f.lose = true;
        }
        if plan.kind == PicKind::Reroot {
            roots += 1;
            f.ack_extra = match roots {
                1 => 30,
                2 => 10,
                _ => 0,
            };
        }
        f
    });
    let sim = run_sim(sc);
    let st = sim.mgr.stats();
    assert!(st.reroots >= 2 && st.recovered == 1, "{st:?}");
    let after: Vec<&Logged> = sim
        .log
        .iter()
        .skip_while(|l| l.plan.kind != PicKind::Reroot)
        .filter(|l| l.plan.kind == PicKind::Healthy)
        .collect();
    assert!(!after.is_empty());
    assert!(
        after.iter().all(|l| l.fed == Some(true) || l.step >= 198),
        "the chain broke after the exit"
    );
}

#[test]
fn an_anchor_120_frames_old_still_recovers_the_stream() {
    // Nothing is acknowledged for 130 frames and the decode deadline is far
    // away (a huge srtt), so the IDR stays the newest anchor while frame_num
    // wraps; then a loss forces recovery from it.
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
    let idr = sim.log[0].plan.frame_seq;
    let reroot = sim
        .log
        .iter()
        .find(|l| l.plan.kind == PicKind::Reroot)
        .expect("re-root");
    assert_eq!(
        reroot.plan.target,
        Some(RefTarget::Long { slot: 0, seq: idr })
    );
    assert!(
        reroot.plan.frame_seq - idr >= 120,
        "anchor age {}",
        reroot.plan.frame_seq - idr
    );
    assert_eq!(reroot.fed, Some(true));
    assert!(
        sim.log.iter().any(|l| l.plan.frame_num < 5 && l.step > 100),
        "frame_num wrapped"
    );
}

#[test]
fn an_unacknowledged_idr_is_probed_and_confirmed_by_the_probe_being_decoded() {
    // Nothing is acknowledged before step 20 and no deadline passes, then a
    // frame is lost: no anchor, no chain. Non-reference probes at the IDR draw
    // the decoded bit (a decoded frame proves its reference was decoded).
    let mut lost = false;
    let mut sc = Script::new(150, move |step, plan| {
        let mut f = Fate {
            suppress_ack: step < 20,
            ..Fate::default()
        };
        if step >= 20 && !lost && plan.kind == PicKind::Healthy {
            lost = true;
            f.lose = true;
        }
        f
    });
    sc.srtt = Box::new(|_| STEP * 100_000);
    let sim = run_sim(sc);
    let st = sim.mgr.stats();
    assert!(st.idr_probes > 0, "{st:?}");
    assert_eq!(st.idr_reissued, 0, "{st:?}");
    assert!(
        sim.log.iter().any(|l| l.plan.kind == PicKind::Reroot),
        "the IDR became the anchor"
    );
    assert!(sim.mgr.is_healthy());
    assert!(sim
        .log
        .iter()
        .filter(|l| l.step >= 120 && l.step < 147)
        .all(|l| l.plan.kind == PicKind::Healthy && l.fed == Some(true)));
}

#[test]
fn a_lost_idr_is_sent_again_with_its_original_cause() {
    let mut first = true;
    let sc = Script::new(260, move |_, plan| Fate {
        lose: plan.kind == PicKind::Idr && std::mem::replace(&mut first, false),
        ..Fate::default()
    });
    let sim = run_sim(sc);
    let st = sim.mgr.stats();
    assert_eq!((st.idr_reissued, st.idr_session), (1, 2), "{st:?}");
    let idrs: Vec<&Logged> = sim
        .log
        .iter()
        .filter(|l| l.plan.kind == PicKind::Idr)
        .collect();
    assert_eq!(idrs.len(), 2);
    assert!(idrs
        .iter()
        .all(|l| l.plan.idr_cause == Some(IdrCause::SessionStart)));
    assert!(idrs[0].lost && idrs[1].fed == Some(true));
    assert!(sim.mgr.is_healthy());
    assert!(sim
        .log
        .iter()
        .filter(|l| l.step >= 230 && l.step < 257)
        .all(|l| l.fed == Some(true)));
}

#[test]
fn a_refused_idr_is_sent_again_with_its_original_cause() {
    let mut h = Hand::new();
    let idr = h.send(0);
    h.m.client_reset(ResetReason::GateReset);
    let idr = {
        let _ = idr;
        h.send(1)
    };
    assert_eq!(idr.idr_cause, Some(IdrCause::ClientReset));
    h.m.frame_refused(idr.frame_seq);
    let again = h.m.plan_next(10, h.at(2)).frame();
    assert_eq!(
        (again.kind, again.idr_cause),
        (PicKind::Idr, Some(IdrCause::ClientReset))
    );
    assert_eq!(h.m.stats().idr_reissued, 1);
}

#[test]
fn late_decoded_bit_for_an_overwritten_candidate_is_ignored() {
    let mut h = Hand::new();
    let idr = h.send(0);
    h.m.frame_decoded(idr.frame_seq);
    assert_eq!(
        h.m.anchor(),
        Some(Anchor {
            slot: 0,
            seq: idr.frame_seq
        })
    );
    let mut step = 1;
    let c1 = loop {
        let p = h.send(step);
        step += 1;
        if p.mark.is_some() {
            break p;
        }
        h.m.frame_decoded(p.frame_seq);
    };
    assert_eq!(c1.mark, Some(1));
    // C1 is never acknowledged. Its deadline passes and it becomes suspect.
    step += 30;
    h.m.tick(h.at(step), STEP * 4);
    assert_eq!(h.m.frame_state(c1.frame_seq), Some(FrameState::Suspect));
    // C2 is marked into the same slot and also lost.
    // A mark that must drop a short-term picture rides a healthy frame, so
    // the re-root that leaves recovery is decoded first.
    let c2 = loop {
        let p = h.send(step);
        step += 1;
        if p.mark.is_some() {
            break p;
        }
        h.m.frame_decoded(p.frame_seq);
        assert!(step < 100, "no mark became due");
    };
    assert_eq!(
        c2.mark,
        Some(1),
        "same slot: the confirmed anchor sits in 0"
    );
    // Leaving recovery superseded C1 (dead) or the occupant rule ignores its
    // bit: either way a late decoded bit must not move the anchor.
    h.m.frame_decoded(c1.frame_seq);
    assert_eq!(
        h.m.anchor(),
        Some(Anchor {
            slot: 0,
            seq: idr.frame_seq
        })
    );
    h.m.frame_decoded(c2.frame_seq);
    assert_eq!(
        h.m.anchor(),
        Some(Anchor {
            slot: 1,
            seq: c2.frame_seq
        })
    );
}

#[test]
fn recovery_names_the_confirmed_anchor_never_a_newer_unconfirmed_candidate() {
    let mut h = Hand::new();
    let idr = h.send(0);
    h.m.frame_decoded(idr.frame_seq);
    let mut step = 1;
    let cand = loop {
        let p = h.send(step);
        step += 1;
        if p.mark.is_some() {
            break p;
        }
        h.m.frame_decoded(p.frame_seq);
    };
    let next = h.send(step);
    h.m.frame_suspect(next.frame_seq);
    step += 1;
    let r = h.send(step);
    assert_eq!(r.kind, PicKind::Reroot);
    assert_eq!(
        r.target,
        Some(RefTarget::Long {
            slot: 0,
            seq: idr.frame_seq
        }),
        "not candidate {}",
        cand.frame_seq
    );
    assert!(r.mark.is_none_or(|k| k == 1));
}

#[test]
fn a_decoded_bit_for_a_frame_implies_its_reference_decoded() {
    let mut h = Hand::new();
    let idr = h.send(0);
    let p1 = h.send(1);
    let p2 = h.send(2);
    h.m.frame_suspect(idr.frame_seq);
    h.m.frame_decoded(p2.frame_seq);
    assert_eq!(h.m.frame_state(p1.frame_seq), Some(FrameState::Decoded));
    assert_eq!(h.m.frame_state(idr.frame_seq), Some(FrameState::Decoded));
    assert_eq!(
        h.m.anchor().map(|a| a.seq),
        Some(idr.frame_seq),
        "the IDR is confirmed through its dependents"
    );
}

#[test]
fn frame_ack_applies_cumulative_masks_idempotently_and_counts_unknown_once() {
    let mut h = Hand::new();
    let seqs: Vec<u32> = (0..6).map(|i| h.send(i).frame_seq).collect();
    let newest = seqs[5];
    // Bit i = newest - i: the newest (5) refused, 4 and 3 decoded, and a bit
    // for a frame never sent (newest - 20). Decoding 3 and 4 implies 0..2.
    let decoded = 0b110 | (1 << 20);
    let refused = 0b1;
    let was = h.m.stats().acks_unknown;
    h.m.frame_ack(newest, decoded, refused);
    let st = |h: &Hand, i: usize| h.m.frame_state(seqs[i]);
    assert_eq!(st(&h, 5), Some(FrameState::Dead));
    assert_eq!(st(&h, 4), Some(FrameState::Decoded));
    assert_eq!(st(&h, 3), Some(FrameState::Decoded));
    assert_eq!(
        st(&h, 2),
        Some(FrameState::Decoded),
        "implied by its dependents"
    );
    assert_eq!(st(&h, 0), Some(FrameState::Decoded));
    assert_eq!(h.m.stats().acks_unknown, was + 1);
    // The same cumulative ack again changes nothing, not even the counter.
    let before = h.m.stats();
    for _ in 0..3 {
        h.m.frame_ack(newest, decoded, refused);
    }
    assert_eq!(h.m.stats(), before);
}

#[test]
fn chain_breaks_before_any_anchor_probe_the_idr_instead_of_waiting() {
    let mut h = Hand::new();
    let idr = h.send(0);
    h.m.frame_suspect(idr.frame_seq);
    let probe = h.send(1);
    assert_eq!(probe.kind, PicKind::Recovery);
    assert_eq!(
        probe.target,
        Some(RefTarget::Long {
            slot: 0,
            seq: idr.frame_seq
        })
    );
    assert_eq!(probe.nal_ref_idc, 0);
    // A silent IDR is sent again after three decode deadlines.
    h.m.tick(h.at(100), STEP * 4);
    let again = h.m.plan_next(50, h.at(100)).frame();
    assert_eq!(again.kind, PicKind::Idr);
}

#[test]
fn idr_only_for_session_resolution_and_client_reset() {
    let mut sc = Script::lossless(300);
    sc.resolution_at = vec![80];
    sc.client_reset_at = vec![160];
    let sim = run_sim(sc);
    let causes: Vec<IdrCause> = sim.log.iter().filter_map(|l| l.plan.idr_cause).collect();
    assert_eq!(
        causes,
        vec![
            IdrCause::SessionStart,
            IdrCause::ResolutionChange,
            IdrCause::ClientReset
        ]
    );
    let st = sim.mgr.stats();
    assert_eq!(
        (
            st.idr_session,
            st.idr_resolution,
            st.idr_client_reset,
            st.idr_encoder_fault,
            st.idr_reissued
        ),
        (1, 1, 1, 0, 0)
    );
    for l in sim.log.iter().filter(|l| l.plan.kind == PicKind::Idr) {
        assert_eq!((l.plan.frame_num, l.plan.poc), (0, 0));
    }
}

#[test]
fn reset_reasons_map_to_causes() {
    for (reason, cause) in [
        (ResetReason::DecoderFailure, IdrCause::ClientReset),
        (ResetReason::NoDecoder, IdrCause::ClientReset),
        (ResetReason::GateReset, IdrCause::ClientReset),
        (ResetReason::Resolution, IdrCause::ResolutionChange),
    ] {
        let mut m = RefManager::new(config());
        m.client_reset(reason);
        assert_eq!(m.pending_idr(), Some(cause));
    }
}

#[test]
fn tile_to_h264_reentry_is_a_reroot_from_the_anchor_or_the_idr_path() {
    // With an anchor: the first frame after re-entry re-roots from it.
    let mut sc = Script::lossless(120);
    sc.reentry_at = vec![80];
    let sim = run_sim(sc);
    let at = sim.log.iter().find(|l| l.step == 80).unwrap();
    assert_eq!(at.plan.kind, PicKind::Reroot);
    assert!(matches!(at.plan.target, Some(RefTarget::Long { .. })));
    assert_eq!(at.fed, Some(true));
    assert_eq!(sim.mgr.stats().reentries, 1);
    assert!(sim.mgr.is_healthy());
    // Without one (the IDR is still unconfirmed): probe it, no new IDR cause.
    let mut h = Hand::new();
    h.send(0);
    h.m.mode_reentry();
    let p = h.send(1);
    assert_eq!(p.kind, PicKind::Recovery);
}

#[test]
fn a_stale_epoch_plan_is_ignored() {
    let mut m = RefManager::new(config());
    let t0 = Instant::now();
    let idr = m.plan_next(1, t0).frame();
    m.resolution_change();
    assert!(m
        .frame_sent(&idr, &idr.slice_header(&params()), 1, t0)
        .is_none());
    assert_eq!(
        m.pending_idr(),
        Some(IdrCause::ResolutionChange),
        "the stale IDR did not consume the request"
    );
    assert_eq!(m.stats().idr_session, 0);
}

#[test]
fn a_mismatched_frame_is_dead_never_prev_and_three_restart_the_stream() {
    let mut h = Hand::new();
    h.send(0);
    for n in 1..=3u32 {
        h.seq += 1;
        let now = h.at(n);
        let plan = h.m.plan_next(h.seq, now).frame();
        // The encoder ignores the plan and names an empty long-term slot.
        let mut bad = plan.slice_header(&params());
        bad.rplm = vec![Rplm::Long(1)];
        let r = h.m.frame_sent(&plan, &bad, 1, now).unwrap();
        assert!(!r.matched);
        if n < 3 {
            assert_eq!(h.m.frame_state(plan.frame_seq), Some(FrameState::Dead));
        }
    }
    assert_eq!(h.m.stats().plan_mismatch, 3);
    assert_eq!(h.m.pending_idr(), Some(IdrCause::EncoderFault));
}

#[test]
fn a_mismatched_or_dropped_frame_never_becomes_prev() {
    let mut h = Hand::new();
    let idr = h.send(0);
    h.m.frame_decoded(idr.frame_seq);
    let ok = h.send(1);
    // An encoded-then-dropped reference frame is in the encoder's DPB only.
    h.seq += 1;
    let now = h.at(2);
    let dropped = h.m.plan_next(h.seq, now).frame();
    h.m.frame_dropped(&dropped, &dropped.slice_header(&params()), now);
    assert_eq!(h.m.frame_state(dropped.frame_seq), Some(FrameState::Dead));
    // The client never gets it and would gap-fill over its frame_num, so the
    // chain re-roots from the anchor instead of continuing from `ok`.
    let next = h.send(3);
    assert_eq!(next.kind, PicKind::Reroot);
    assert_eq!(next.ref_frame_seq(), Some(idr.frame_seq));
    assert_ne!(next.ref_frame_seq(), Some(dropped.frame_seq));
    assert_ne!(next.ref_frame_seq(), Some(ok.frame_seq));
    // ... and the encoder-side DPB still lists everything it holds.
    assert!(h
        .m
        .resident_refs()
        .iter()
        .any(|r| r.seq == dropped.frame_seq));
    assert_eq!(next.frame_num, (dropped.frame_num + 1) % 128);
}

#[test]
fn frame_num_wraps_at_128_across_tile_gaps_in_frame_seq() {
    let mut sc = Script::lossless(300);
    sc.seq_stride = 3;
    let sim = run_sim(sc);
    let fns: Vec<u32> = sim.log.iter().map(|l| l.plan.frame_num).collect();
    assert!(fns.contains(&127) && fns.windows(2).any(|w| w == [127, 0]));
    assert!(sim
        .log
        .iter()
        .skip(1)
        .all(|l| l.fed == Some(true) || l.step >= 298));
}

#[test]
fn client_gaps_do_not_break_effective_references() {
    let sc = Script::new(300, |step, plan| Fate {
        lose: step > 20 && step < 170 && step % 5 == 0 && plan.kind != PicKind::Idr,
        ..Fate::default()
    });
    let sim = run_sim(sc);
    assert!(sim.mgr.stats().recoveries >= 1);
    assert!(sim
        .log
        .iter()
        .filter(|l| l.plan.kind == PicKind::Reroot && !l.lost)
        .all(|l| l.fed == Some(true)));
}

#[test]
fn recovery_frames_do_not_advance_frame_num_or_the_dpb() {
    let mut lost = false;
    let sc = Script::new(120, move |step, plan| {
        let mut f = Fate::default();
        if step >= 50 && !lost && plan.kind == PicKind::Healthy {
            lost = true;
            f.lose = true;
        }
        f
    });
    let sim = run_sim(sc);
    let recov: Vec<&Logged> = sim
        .log
        .iter()
        .filter(|l| l.plan.kind == PicKind::Recovery)
        .collect();
    assert!(!recov.is_empty());
    for r in recov {
        let before = sim
            .log
            .iter()
            .take_while(|l| l.plan.frame_seq < r.plan.frame_seq)
            .filter(|l| l.plan.nal_ref_idc != 0)
            .last()
            .unwrap();
        assert_eq!(r.plan.frame_num, (before.plan.frame_num + 1) % 128);
        assert_eq!(
            r.effective.dpb_len, before.effective.dpb_len,
            "non-reference: DPB unchanged"
        );
    }
}

#[test]
fn cumulative_acks_with_reordering_and_a_moving_srtt_still_converge() {
    let mut n = 0u32;
    let mut sc = Script::new(300, move |step, plan| {
        n = n.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        Fate {
            lose: step < 180 && n >> 16 & 7 == 0 && plan.kind != PicKind::Idr,
            jitter: if step < 180 {
                (n >> 20 & 3) as usize
            } else {
                0
            },
            ..Fate::default()
        }
    });
    sc.cumulative = true;
    sc.srtt = Box::new(|step| STEP * (2 + (step as u32 % 9)));
    let sim = run_sim(sc);
    assert!(sim.mgr.stats().recoveries > 0);
    assert!(sim.mgr.is_healthy());
    assert!(sim
        .log
        .iter()
        .filter(|l| (270..295).contains(&l.step))
        .all(|l| l.plan.kind == PicKind::Healthy && l.fed == Some(true)));
}

#[test]
fn poc_stays_monotonic_across_the_16_bit_lsb_wrap() {
    let sim = run_sim(Script::lossless(33_000));
    assert!(sim.log.last().unwrap().plan.poc > 65_536);
    assert!(sim
        .log
        .windows(2)
        .all(|w| w[1].effective.poc == w[0].effective.poc + 2));
}

// ----------------------------------------------------------------- proptests

/// (lose, ack_extra, spurious, no_verdict, suppress_ack, jitter)
pub(super) type FateRow = (bool, u8, bool, bool, bool, u8);
/// (delay, fates, loss_until, seq_stride, cumulative, srtt_ms_base, slow_idr)
/// `slow_idr` is 0 for a prompt IDR, else the extra steps (80 or more, so
/// more than 750 ms) its 8 fragments take to arrive and decode.
pub(super) type ScriptParams = (usize, Vec<FateRow>, usize, u32, bool, u64, usize);

/// The script a [`ScriptParams`] value describes.
pub(super) fn script_from(params: &ScriptParams, steps: usize) -> Script {
    let (delay, fates, loss_until, stride, cumulative, srtt_ms, slow_idr) = params.clone();
    let mut sc = Script::new(steps, move |step, plan| {
        let slow = if plan.kind == PicKind::Idr && slow_idr > 0 {
            Some(slow_idr)
        } else {
            None
        };
        if step >= loss_until {
            return Fate {
                jitter: slow.unwrap_or(0),
                ..Fate::default()
            };
        }
        let (lose, extra, spurious, nov, supp, jitter) = fates[step % fates.len()];
        Fate {
            // An IDR can be lost early on; later ones always arrive.
            lose: lose && (plan.kind != PicKind::Idr || step < 100),
            jitter: slow.unwrap_or(usize::from(jitter)),
            ack_extra: usize::from(extra),
            suppress_ack: supp && step < 100,
            spurious_verdict: spurious,
            no_verdict: nov,
        }
    });
    sc.delay = delay;
    sc.seq_stride = stride;
    sc.cumulative = cumulative;
    if slow_idr > 0 {
        sc.idr_frags = 8;
    }
    sc.srtt = Box::new(move |step| Duration::from_millis(srtt_ms + (step as u64 % 7) * 3));
    sc
}

pub(super) fn arb_script() -> impl Strategy<Value = ScriptParams> {
    (
        1usize..4,
        proptest::collection::vec(
            (
                prop::bool::weighted(0.15),
                0u8..30,
                prop::bool::weighted(0.05),
                prop::bool::weighted(0.1),
                prop::bool::weighted(0.05),
                0u8..4,
            ),
            200,
        ),
        150usize..200,
        1u32..4,
        any::<bool>(),
        10u64..60,
        prop_oneof![Just(0usize), 80usize..130],
    )
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Over random loss (the IDR included), ack delay and loss, reordering,
    /// spurious and missing verdicts, a moving srtt and both ack styles:
    /// effective == planned on the full stream (asserted for every frame), the
    /// manager's DPB equals the model's on every frame, the gate never admits a
    /// frame whose header reads another picture, the client never holds more
    /// than four references, every delivered re-root finds its anchor, and
    /// once losses stop the stream settles to healthy frames the client
    /// decodes.
    #[test]
    fn plans_match_effective_references_over_random_loss(params in arb_script(), lavc in any::<bool>()) {
        let mut sc = script_from(&params, 700);
        sc.lavc_gaps = lavc;
        let (delay, loss_until, stride, cumulative, srtt_ms, slow_idr) =
            (params.0, params.2, params.3, params.4, params.5, params.6);
        let mut sim = Sim::new();
        sim.run(&mut sc, true);

        let st = sim.mgr.stats();
        prop_assert_eq!(st.idr_encoder_fault + st.idr_client_reset + st.idr_resolution, 0);
        prop_assert_eq!(st.idr_session, 1 + st.idr_reissued);
        prop_assert!(sim.max_client_peak <= 4 && sim.max_enc_peak <= 4);
        for l in &sim.log {
            prop_assert!(l.matched);
            // A re-root names a confirmed anchor: unless the network lost or
            // reordered it, the client has it.
            if l.plan.kind == PicKind::Reroot && !l.lost && l.fed == Some(false) {
                prop_assert!(jitter_allows(&sim, l), "anchor missing at the client: {:?}", l.plan.target);
            }
        }
        prop_assert!(sim.mgr.is_healthy());
        let tail: Vec<&Logged> = sim.log.iter().filter(|l| (650..695).contains(&l.step)).collect();
        prop_assert!(!tail.is_empty(), "params {:?} holds {} stats {:?} log {} last {:?}", (delay, loss_until, stride, cumulative, srtt_ms, slow_idr), sim.holds, st, sim.log.len(), sim.log.last().map(|l| (l.step, l.plan.kind)));
        for l in tail {
            prop_assert_eq!(l.plan.kind, PicKind::Healthy);
            prop_assert_eq!(l.fed, Some(true));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(1000))]

    /// Startup epochs under heavy loss, half of it never reported by a
    /// verdict (only the decode deadline finds it), with a client that infers
    /// gap frames either way: the strict simulation asserts that every MMCO 1
    /// names a picture the client really holds and that nothing overflows.
    #[test]
    fn startup_losses_never_make_a_mark_forget_a_missing_picture(
        delay in 1usize..4,
        fates in proptest::collection::vec((prop::bool::weighted(0.35), any::<bool>()), 30),
        lavc in any::<bool>(),
    ) {
        let mut sc = Script::new(90, move |step, plan| {
            let (lose, no_verdict) = fates[step % fates.len()];
            Fate {
                lose: lose && step < 30 && plan.kind != PicKind::Idr,
                no_verdict,
                ..Fate::default()
            }
        });
        sc.delay = delay;
        sc.lavc_gaps = lavc;
        let mut sim = Sim::new();
        sim.run(&mut sc, true);
        prop_assert!(sim.max_client_peak <= 4);
    }
}

/// A delivered re-root can be refused only for reordering (a newer frame was
/// fed first), never because the client lacks the anchor.
fn jitter_allows(sim: &Sim, l: &Logged) -> bool {
    sim.log.iter().any(|o| {
        seq_lt(l.plan.frame_seq, o.plan.frame_seq) && o.step <= l.step + 6 && o.fed == Some(true)
    }) || sim
        .log
        .iter()
        .any(|o| o.plan.kind == PicKind::Idr && seq_lt(l.plan.frame_seq, o.plan.frame_seq))
}

#[test]
fn a_slow_idr_is_not_given_up_on_while_it_is_being_delivered() {
    // 1080p IDRs are 100-290 KB: at 2 Mbit/s that is 0.4-1.1 s of delivery.
    // Every IDR here takes 800 ms to arrive (8 fragments acked along the way)
    // with srtt 40 ms. It must be delivered once, with no probe P and no
    // second IDR, and P frames start only when it is complete.
    let mut sc = Script::new(400, |_, plan| Fate {
        jitter: if plan.kind == PicKind::Idr { 78 } else { 0 },
        ..Fate::default()
    });
    sc.idr_frags = 8;
    let sim = run_sim(sc);
    let st = sim.mgr.stats();
    assert_eq!(
        (st.idr_session, st.idr_reissued, st.idr_probes),
        (1, 0, 0),
        "{st:?}"
    );
    assert!(sim.holds >= 70, "held {} frames", sim.holds);
    let first_p = sim
        .log
        .iter()
        .find(|l| l.plan.kind != PicKind::Idr)
        .unwrap();
    assert!(
        first_p.step >= 78,
        "a P was planned at step {}",
        first_p.step
    );
    assert!(sim.mgr.is_healthy());
    assert!(sim
        .log
        .iter()
        .filter(|l| l.step >= 100 && l.step < 397)
        .all(|l| l.fed == Some(true)));
}

#[test]
fn idr_that_makes_no_progress_is_sent_again_with_a_growing_timeout() {
    let mut h = Hand::new();
    let plan = h.m.plan_next(1, h.at(0)).frame();
    let au = plan.slice_header(&params());
    h.m.frame_sent(&plan, &au, 4, h.at(0)).unwrap();
    // No fragment is ever acknowledged: held for three deadlines (750 ms)...
    assert_eq!(h.m.plan_next(2, h.at(50)), Plan::Hold);
    assert_eq!(h.m.plan_next(3, h.at(75)), Plan::Hold);
    let second = h.m.plan_next(4, h.at(76)).frame();
    assert_eq!(
        (second.kind, second.idr_cause),
        (PicKind::Idr, Some(IdrCause::SessionStart))
    );
    h.m.frame_sent(&second, &second.slice_header(&params()), 4, h.at(76))
        .unwrap();
    // ... then twice as long, then four times as long, and no more.
    assert_eq!(h.m.plan_next(5, h.at(76 + 150)), Plan::Hold);
    let third = h.m.plan_next(6, h.at(76 + 151)).frame();
    assert_eq!(third.kind, PicKind::Idr);
    h.m.frame_sent(&third, &third.slice_header(&params()), 4, h.at(227))
        .unwrap();
    assert_eq!(h.m.plan_next(7, h.at(227 + 300)), Plan::Hold);
    assert_eq!(h.m.plan_next(8, h.at(227 + 301)).frame().kind, PicKind::Idr);
    let fourth = h.m.plan_next(9, h.at(227 + 301)).frame();
    h.m.frame_sent(&fourth, &fourth.slice_header(&params()), 4, h.at(528))
        .unwrap();
    assert_eq!(
        h.m.plan_next(10, h.at(528 + 300)),
        Plan::Hold,
        "capped at four times"
    );
    assert_eq!(
        h.m.plan_next(11, h.at(528 + 301)).frame().kind,
        PicKind::Idr
    );
    assert_eq!(h.m.stats().idr_reissued, 4);
}

#[test]
fn each_acked_fragment_restarts_the_idr_clock_and_the_deadline_starts_when_it_is_complete() {
    let mut h = Hand::new();
    let idr = h.m.plan_next(1, h.at(0)).frame();
    h.m.frame_sent(&idr, &idr.slice_header(&params()), 4, h.at(0))
        .unwrap();
    // One fragment every 500 ms: never 750 ms without progress, so held.
    for (i, ms) in [500u32, 1000, 1500, 2000].into_iter().enumerate() {
        assert_eq!(h.m.plan_next(10 + i as u32, h.at(ms / 10 - 1)), Plan::Hold);
        h.m.frag_acked(idr.frame_seq, i as u16, h.at(ms / 10));
    }
    // Complete at 2.0 s: now P frames may go, and the IDR is not suspect
    // although it was sent 2 s ago.
    h.m.tick(h.at(200 + 10), STEP * 4);
    assert_eq!(h.m.frame_state(idr.frame_seq), Some(FrameState::Sent));
    assert!(matches!(h.m.plan_next(20, h.at(210)), Plan::Frame(_)));
    // A verdict on a half-delivered IDR is ignored; on a complete one it counts.
    let mut g = Hand::new();
    let i2 = g.m.plan_next(1, g.at(0)).frame();
    g.m.frame_sent(&i2, &i2.slice_header(&params()), 4, g.at(0))
        .unwrap();
    g.m.frame_suspect(i2.frame_seq);
    assert_eq!(g.m.frame_state(i2.frame_seq), Some(FrameState::Sent));
    g.m.tick(g.at(100), STEP * 4);
    assert_eq!(
        g.m.frame_state(i2.frame_seq),
        Some(FrameState::Sent),
        "no deadline while incomplete"
    );
}

#[test]
fn proptest_loop_exercises_recovery_and_ignored_confirmations() {
    // The property test above is only meaningful if it reaches these paths.
    let sc = Script::new(300, |step, _| Fate {
        lose: step < 200 && step % 7 == 3,
        ack_extra: if step % 11 == 0 { 28 } else { 0 },
        ..Fate::default()
    });
    let sim = run_sim(sc);
    let st = sim.mgr.stats();
    assert!(
        st.recoveries > 3 && st.reroots > 3 && st.p_recovery > 3,
        "{st:?}"
    );
    assert!(
        st.candidates_marked > 3 && st.anchors_confirmed > 3,
        "{st:?}"
    );
    assert!(
        st.deadline_suspects > 0,
        "late acks must trip the decode deadline: {st:?}"
    );
    assert!(sim.kinds().contains(&PicKind::Recovery));
}

#[test]
fn a_slow_idr_survives_more_than_a_ring_of_frame_sequence_numbers() {
    // Tile frames burn sequence numbers while the IDR is delivered: by the
    // time it is decoded its slot in the 256-frame ring has been reused.
    let mut sc = Script::new(300, |_, plan| Fate {
        jitter: if plan.kind == PicKind::Idr { 123 } else { 0 },
        ..Fate::default()
    });
    sc.delay = 3;
    sc.seq_stride = 2;
    sc.idr_frags = 8;
    let sim = run_sim(sc);
    let st = sim.mgr.stats();
    assert_eq!(
        (st.idr_session, st.idr_reissued, st.acks_unknown),
        (1, 0, 0),
        "{st:?}"
    );
    assert!(st.anchors_confirmed > 0);
    assert!(sim.mgr.is_healthy());
}

/// `FramePlan::wire_info` names the picture a mark lands on: the frame
/// (MMCO 6, `MARK_LT`) or the frame it predicts from (MMCO 3, `MARK_TARGET`).
#[test]
fn wire_info_flags_a_target_mark_apart_from_a_carrier_mark() {
    use ghostframe_protocol::protocol::{
        FRAME_FLAG_MARK_LT, FRAME_FLAG_MARK_TARGET, FRAME_FLAG_TARGET_IS_LT,
    };
    let plan = FramePlan {
        epoch: 1,
        frame_seq: 9,
        kind: PicKind::Healthy,
        idr_cause: None,
        nal_ref_idc: 2,
        frame_num: 5,
        poc: 10,
        target: Some(RefTarget::Short {
            seq: 8,
            frame_num: 4,
        }),
        mark: Some(1),
        mark_target: false,
        mmco: Vec::new(),
        rplm: Vec::new(),
    };
    let carrier = plan.wire_info(None);
    assert_eq!(carrier.flags, FRAME_FLAG_MARK_LT);
    assert_eq!((carrier.slots, carrier.ref_frame_seq), (1, 8));

    let target = FramePlan {
        mark_target: true,
        ..plan.clone()
    }
    .wire_info(None);
    assert_eq!(target.flags, FRAME_FLAG_MARK_TARGET);
    assert_eq!((target.slots, target.ref_frame_seq), (1, 8));

    // No mark, no bit, whatever `mark_target` says.
    let none = FramePlan {
        mark: None,
        mark_target: true,
        ..plan.clone()
    }
    .wire_info(None);
    assert_eq!(none.flags, 0);

    // It travels with a long-term target flag unchanged.
    let lt = FramePlan {
        mark_target: true,
        target: Some(RefTarget::Long { slot: 0, seq: 3 }),
        ..plan
    }
    .wire_info(None);
    assert_eq!(lt.flags, FRAME_FLAG_MARK_TARGET | FRAME_FLAG_TARGET_IS_LT);
    assert_eq!(lt.slots, 1);
}
