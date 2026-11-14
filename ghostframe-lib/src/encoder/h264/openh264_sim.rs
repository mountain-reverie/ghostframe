// SPDX-License-Identifier: AGPL-3.0-or-later
//! `Openh264Sim`: a deterministic stand-in for libopenh264's long-term
//! reference machinery, for driving the observed-mode `RefManager` without
//! the library.
//!
//! It emits slice headers shaped like the real library's (see the B7 plan,
//! findings 1.2/2-6 and the differential test against the library in
//! `observed_diff_tests`): one slice per picture, every P a reference, the
//! previous frame as `RefPicList0[0]` through an explicit `Sub(0)`, the first
//! P after an IDR (or after an MMCO 6 mark) through `Long(idx)`; one long-term
//! mark about every 31 frames, the first by MMCO 4 + 1 + 6 (the carrier itself
//! becomes long-term), later ones by MMCO 3 on the previous frame; a mark only
//! once the previous one has been fed back; `LtrMarkFailed` makes it mark
//! again, into the SAME long-term index (the index advances only on a
//! success; the MMCO 6 shape when no mark was ever fed back, else MMCO 3); a
//! recovery request names the CARRIER's frame number of a fed-back mark and a
//! current number not behind the last recovery's (compared modulo
//! `MaxFrameNum`, as the library does; the rule resets at each new mark), and
//! makes the next frame a P from that long-term picture; anything else is an
//! unknown frame: one more P, then an IDR. Mark feedback is latched and
//! applied at the next frame, after a recovery request has chosen its
//! picture, as in the library.
//!
//! It is **advisory**. The library chooses the long-term index and the frame
//! that carries a mark by rules the sim does not copy; the sim varies both
//! with a seed (and sometimes puts a mark on a recovery frame, where the
//! library does too), so the manager cannot lean on any one placement. The
//! real library is the gate: the differential test replays one event script
//! on both and compares kind, target class, mark shape and IDR, never slot
//! indices.

use super::backend::BackendFeedback;
use super::syntax::{DecoderModel, Mmco, Rplm, SliceHeader, StreamParams};

/// Frames from one mark's carrier to the next's.
pub(crate) const MARK_PERIOD: u64 = 31;
/// Frames from the IDR to the first mark's carrier.
pub(crate) const FIRST_MARK_AT: u64 = 32;

/// `a` is behind `b` in frame numbers modulo `max`: the library's
/// `CompareFrameNum`, the nearest distance decides.
pub(crate) fn fn_before(a: u32, b: u32, max: u32) -> bool {
    let d = (b + max - a) % max;
    d != 0 && d < max / 2
}

/// The stream parameters of the committed openh264 fixture (High, POC type 2,
/// `log2_max_frame_num` 15, three reference frames, gaps allowed).
pub(crate) fn sim_params() -> StreamParams {
    StreamParams::from_annexb(include_bytes!(
        "testdata/openh264_2_6_camera_ltr_128x128.h264"
    ))
    .expect("the fixture's parameter sets parse")
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct SimPolicy {
    /// Seed of the placement choices; `None` is the library's observed
    /// placement (the first mark in index 1, then alternating).
    pub seed: Option<u64>,
    /// A mark that falls due on a recovery frame is made anyway, with the
    /// seed's say; otherwise it waits for the frame after.
    pub mark_on_recovery: bool,
}

impl SimPolicy {
    pub(crate) const FAITHFUL: Self = Self {
        seed: None,
        mark_on_recovery: false,
    };
}

#[derive(Debug, Clone, Copy)]
struct Lt {
    marked_fn: u32,
    carrier_fn: u32,
    /// `LtrMarked` was received for it (the IDR's picture counts as fed).
    fed: bool,
    /// Its mark failed: the library does not recover from it.
    void: bool,
    /// Which mark it was (the IDR's picture is 0).
    order: u64,
}

/// What kind of frame the sim just made, for tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SimKind {
    Idr,
    /// P from the previous frame.
    Chain,
    /// P from a long-term picture (a recovery, or the frame after an IDR or
    /// an MMCO 6 mark).
    FromLt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MarkShape {
    None,
    /// MMCO 4 + 1 + 6: the carrier becomes long-term.
    Mmco6,
    /// MMCO 3: the carrier's predecessor becomes long-term.
    Mmco3,
}

/// One frame the sim made.
#[derive(Debug, Clone)]
pub(crate) struct SimFrame {
    pub header: SliceHeader,
    pub kind: SimKind,
    pub mark: MarkShape,
    /// A recovery frame (a `FromLt` the request asked for).
    pub recovery: bool,
}

impl SimFrame {
    /// The long-term index a mark in this frame writes to.
    pub(crate) fn mark_index(&self) -> Option<u32> {
        self.header.mmco.iter().flatten().find_map(|m| match *m {
            Mmco::CurrentToLong(i) | Mmco::ShortToLong(_, i) => Some(i),
            _ => None,
        })
    }

    /// The frame a header describes, for an engine that is not the sim (the
    /// library): its kind and mark shape, read from the header alone.
    pub(crate) fn from_header(header: SliceHeader, recovery: bool) -> Self {
        let mmco = header.mmco.clone().unwrap_or_default();
        let mark = if mmco.iter().any(|m| matches!(m, Mmco::CurrentToLong(_))) {
            MarkShape::Mmco6
        } else if mmco.iter().any(|m| matches!(m, Mmco::ShortToLong(..))) {
            MarkShape::Mmco3
        } else {
            MarkShape::None
        };
        let kind = if header.idr {
            SimKind::Idr
        } else if header.rplm.iter().any(|r| matches!(r, Rplm::Long(_))) {
            SimKind::FromLt
        } else {
            SimKind::Chain
        };
        Self {
            header,
            kind,
            mark,
            recovery,
        }
    }
}

pub(crate) struct Openh264Sim {
    pub params: StreamParams,
    policy: SimPolicy,
    rng: u64,
    idr_pic_id: u32,
    /// Frames since the IDR (the IDR is 0).
    idx: u64,
    next_fn: u32,
    st: Vec<u32>,
    lt: [Option<Lt>; 2],
    /// The frame before was an IDR or an MMCO 6 carrier: the long-term slot
    /// holding it, which the next frame predicts from.
    prev_lt: Option<usize>,
    last_carrier_idx: u64,
    ever_marked: bool,
    /// A mark (slot) whose feedback has not been applied.
    unresolved: Option<usize>,
    /// Feedback received for it, applied at the next frame: (slot, success).
    latched: Option<(usize, bool)>,
    /// A failed mark is made again into this index.
    retry_slot: Option<usize>,
    /// The next mark has the MMCO 6 shape (the first, or after a failure).
    first_shape: bool,
    /// A mark of this IDR period was fed back: a failed mark is then re-made
    /// as an MMCO 3 (measured on 2.6.0), else as the MMCO 6 of a first mark.
    mark_fed: bool,
    last_slot: Option<usize>,
    recover_to: Option<usize>,
    last_recovery_fn: Option<u32>,
    /// Frames until an IDR the library makes on its own (an unknown frame).
    idr_in: Option<u32>,
    force_idr: bool,
    model: DecoderModel,
    pub stats: SimStats,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SimStats {
    pub marks: u64,
    pub recoveries: u64,
    pub stale_ignored: u64,
    pub unknown_frames: u64,
    pub own_idrs: u64,
}

impl Openh264Sim {
    pub(crate) fn new(policy: SimPolicy) -> Self {
        let params = sim_params();
        Self {
            params,
            policy,
            rng: policy.seed.unwrap_or(1) | 1,
            idr_pic_id: 0,
            idx: 0,
            next_fn: 0,
            st: Vec::new(),
            lt: [None; 2],
            prev_lt: None,
            last_carrier_idx: 0,
            ever_marked: false,
            unresolved: None,
            latched: None,
            retry_slot: None,
            first_shape: true,
            mark_fed: false,
            last_slot: None,
            recover_to: None,
            last_recovery_fn: None,
            idr_in: None,
            force_idr: false,
            model: DecoderModel::new(params),
            stats: SimStats::default(),
        }
    }

    pub(crate) fn current_idr_pic_id(&self) -> u32 {
        self.idr_pic_id
    }

    fn rand(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// The frame numbers the sim currently holds long-term (for tests).
    pub(crate) fn lt_frames(&self) -> [Option<(u32, u32)>; 2] {
        self.lt.map(|l| l.map(|l| (l.marked_fn, l.carrier_fn)))
    }

    pub(crate) fn feedback(&mut self, fb: BackendFeedback) {
        match fb {
            BackendFeedback::LtrMarked {
                idr_pic_id,
                frame_num,
            } if idr_pic_id == self.idr_pic_id => {
                if let Some(k) = self.unresolved {
                    if self.lt[k].is_some_and(|l| l.marked_fn == frame_num) {
                        self.latched = Some((k, true));
                    }
                }
            }
            BackendFeedback::LtrMarkFailed {
                idr_pic_id,
                frame_num,
            } if idr_pic_id == self.idr_pic_id => {
                if let Some(k) = self.unresolved {
                    if self.lt[k].is_some_and(|l| l.marked_fn == frame_num) {
                        self.latched = Some((k, false));
                    }
                }
            }
            BackendFeedback::LtrRecover {
                idr_pic_id,
                last_correct_frame_num,
                current_frame_num,
            } if idr_pic_id == self.idr_pic_id => {
                let max = self.params.max_frame_num();
                if self
                    .last_recovery_fn
                    .is_some_and(|last| fn_before(current_frame_num, last, max))
                {
                    // Silently ignored, until the next mark.
                    self.stats.stale_ignored += 1;
                    return;
                }
                // What the library does with the number (measured on 2.6.0):
                // the carrier of a fed-back mark is recovered from; the
                // carrier of a mark never fed back, or the MARKED picture of
                // an MMCO 3 mark, is an invalid long-term reference: one P
                // from a long-term picture, then an IDR; a number that is
                // nothing at all is one P from a long-term picture and the
                // stream goes on.
                let live = |k: usize| self.lt[k].filter(|l| !l.void);
                // The mark whose feedback is latched is not usable yet: the
                // request is answered from the pictures the library already
                // holds as received (the older anchor).
                let latched = self.latched.map(|(k, _)| k);
                let carrier = (0..2)
                    .find(|&k| live(k).is_some_and(|l| l.carrier_fn == last_correct_frame_num));
                let marked = (0..2).find(|&k| {
                    live(k).is_some_and(|l| {
                        l.marked_fn == last_correct_frame_num && l.carrier_fn != l.marked_fn
                    })
                });
                match (carrier, marked) {
                    (Some(k), _) if Some(k) == latched => {
                        let older = (0..2)
                            .filter(|&j| j != k && live(j).is_some_and(|l| l.fed))
                            .max_by_key(|&j| live(j).map_or(0, |l| l.order));
                        self.recover_to = older;
                        self.stats.recoveries += 1;
                    }
                    (Some(k), _) if self.lt[k].is_some_and(|l| l.fed) => {
                        self.recover_to = Some(k);
                        self.stats.recoveries += 1;
                    }
                    (Some(k), _) | (None, Some(k)) => {
                        self.recover_to = Some(k);
                        self.idr_in.get_or_insert(1);
                        self.stats.unknown_frames += 1;
                    }
                    (None, None) => {
                        let newest = (0..2)
                            .filter(|&k| live(k).is_some_and(|l| l.fed))
                            .max_by_key(|&k| live(k).map_or(0, |l| l.order));
                        self.recover_to = newest;
                        self.stats.unknown_frames += 1;
                    }
                }
            }
            // A stale IDR period: ignored.
            _ => {}
        }
    }

    /// The next picture. `idr` is the manager's request (`ForceIntraFrame`).
    pub(crate) fn encode(&mut self, idr: bool) -> SimFrame {
        if let Some((k, ok)) = self.latched.take() {
            if ok {
                self.lt[k].as_mut().expect("latched a held mark").fed = true;
                self.mark_fed = true;
            } else {
                self.lt[k].as_mut().expect("latched a held mark").void = true;
                self.retry_slot = Some(k);
                self.first_shape = !self.mark_fed;
            }
            self.unresolved = None;
        }
        let own_idr = match self.idr_in {
            Some(0) => {
                self.idr_in = None;
                true
            }
            Some(n) => {
                self.idr_in = Some(n - 1);
                false
            }
            None => false,
        };
        if idr || own_idr || self.force_idr || self.idr_pic_id == 0 {
            self.force_idr = false;
            if own_idr && !idr {
                self.stats.own_idrs += 1;
            }
            return self.make_idr();
        }
        self.make_p()
    }

    /// The library makes an IDR at the next frame without being asked (scene
    /// change detection left on, or its own fallback).
    pub(crate) fn make_idr_next(&mut self) {
        self.force_idr = true;
    }

    fn make_idr(&mut self) -> SimFrame {
        self.idr_pic_id += 1;
        self.idx = 0;
        self.next_fn = 1;
        self.st.clear();
        self.lt = [
            Some(Lt {
                marked_fn: 0,
                carrier_fn: 0,
                fed: true,
                void: false,
                order: 0,
            }),
            None,
        ];
        self.prev_lt = Some(0);
        self.last_carrier_idx = 0;
        self.ever_marked = false;
        self.unresolved = None;
        self.latched = None;
        self.retry_slot = None;
        self.first_shape = true;
        self.mark_fed = false;
        self.last_slot = None;
        self.recover_to = None;
        self.last_recovery_fn = None;
        self.idr_in = None;
        let header = SliceHeader::idr(self.idr_pic_id, true);
        self.model = DecoderModel::new(self.params);
        let eff = self.model.push(0, &header);
        assert!(!eff.violations.any(), "sim IDR: {:?}", eff.violations);
        SimFrame {
            header,
            kind: SimKind::Idr,
            mark: MarkShape::None,
            recovery: false,
        }
    }

    fn pick_slot(&mut self, first: bool) -> usize {
        // A failed mark is made again into its own index; the index advances
        // only on a success (the library's `iCurLtrIdx`). While an index is
        // empty the mark goes there (the first mark, in index 1); once both
        // are held, and a mark has succeeded, the choice is the library's,
        // which the seed varies.
        if let Some(k) = self.retry_slot.take() {
            return k;
        }
        if let Some(empty) = (0..2).find(|&k| self.lt[k].is_none()) {
            return empty;
        }
        let _ = first;
        if self.policy.seed.is_some() {
            return (self.rand() & 1) as usize;
        }
        1 - self.last_slot.unwrap_or(1)
    }

    fn make_p(&mut self) -> SimFrame {
        self.idx += 1;
        let fnum = self.next_fn;
        self.next_fn = (self.next_fn + 1) % self.params.max_frame_num();
        let recovery = self.recover_to.take();
        let mut h = SliceHeader::p(fnum, 0, 3);
        h.num_ref_idx_override = Some(1);
        let kind = if let Some(k) = recovery {
            h.rplm = vec![Rplm::Long(k as u32)];
            self.last_recovery_fn = Some(fnum);
            SimKind::FromLt
        } else if let Some(k) = self.prev_lt {
            h.rplm = vec![Rplm::Long(k as u32)];
            SimKind::FromLt
        } else {
            h.rplm = vec![Rplm::Sub(0)];
            SimKind::Chain
        };
        self.prev_lt = None;

        // A mark falls due `MARK_PERIOD` frames after the last one's carrier
        // (the first at `FIRST_MARK_AT`), and only once the last is resolved.
        let due_at = if self.ever_marked || !self.first_shape {
            self.last_carrier_idx + MARK_PERIOD
        } else {
            FIRST_MARK_AT
        };
        let mut due = self.unresolved.is_none() && self.idx >= due_at;
        if due && recovery.is_some() {
            due =
                self.policy.mark_on_recovery && self.policy.seed.is_some() && self.rand() & 1 == 0;
        }
        let mut shape = MarkShape::None;
        if due {
            self.ever_marked = true;
            self.last_carrier_idx = self.idx;
            self.last_recovery_fn = None;
            self.stats.marks += 1;
            let first = std::mem::take(&mut self.first_shape);
            let slot = self.pick_slot(first);
            self.last_slot = Some(slot);
            if first {
                shape = MarkShape::Mmco6;
                h.mmco = Some(vec![
                    Mmco::MaxLong(2),
                    Mmco::ForgetShort(0),
                    Mmco::CurrentToLong(slot as u32),
                ]);
                self.lt[slot] = Some(Lt {
                    marked_fn: fnum,
                    carrier_fn: fnum,
                    fed: false,
                    void: false,
                    order: self.stats.marks,
                });
                self.prev_lt = Some(slot);
            } else {
                shape = MarkShape::Mmco3;
                h.mmco = Some(vec![Mmco::ShortToLong(0, slot as u32)]);
                let marked_fn =
                    (fnum + self.params.max_frame_num() - 1) % self.params.max_frame_num();
                self.lt[slot] = Some(Lt {
                    marked_fn,
                    carrier_fn: fnum,
                    fed: false,
                    void: false,
                    order: self.stats.marks,
                });
            }
            self.unresolved = Some(slot);
        }
        // The model decides what the DPB holds (and checks the sim's own
        // stream); mirror its short-term list.
        let eff = self.model.push(fnum, &h);
        assert!(
            !eff.violations.any(),
            "sim stream broke a rule: {:?} at frame {fnum}: {h:?}",
            eff.violations
        );
        self.st.clear();
        SimFrame {
            header: h,
            kind,
            mark: shape,
            recovery: recovery.is_some(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drive(sim: &mut Openh264Sim, n: usize) -> Vec<SimFrame> {
        (0..n).map(|_| sim.encode(false)).collect()
    }

    fn first_idr(sim: &mut Openh264Sim) -> SimFrame {
        let f = sim.encode(true);
        assert_eq!(f.kind, SimKind::Idr);
        f
    }

    fn marks(frames: &[SimFrame]) -> Vec<(usize, MarkShape)> {
        frames
            .iter()
            .enumerate()
            .filter(|(_, f)| f.mark != MarkShape::None)
            .map(|(i, f)| (i, f.mark))
            .collect()
    }

    #[test]
    fn without_feedback_the_sim_marks_once_and_never_again() {
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        first_idr(&mut sim);
        let fs = drive(&mut sim, 200);
        assert_eq!(
            marks(&fs),
            vec![(31, MarkShape::Mmco6)],
            "frame 32 after the IDR"
        );
    }

    #[test]
    fn marked_feedback_lets_the_next_mark_follow_in_the_mmco3_shape() {
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        let idr = first_idr(&mut sim);
        let pic = idr.header.idr_pic_id;
        let mut fs = Vec::new();
        for i in 0..100 {
            let f = sim.encode(false);
            if f.mark == MarkShape::Mmco6 {
                sim.feedback(BackendFeedback::LtrMarked {
                    idr_pic_id: pic,
                    frame_num: f.header.frame_num,
                });
            }
            if f.mark == MarkShape::Mmco3 {
                let marked = f.header.frame_num - 1;
                sim.feedback(BackendFeedback::LtrMarked {
                    idr_pic_id: pic,
                    frame_num: marked,
                });
            }
            let _ = i;
            fs.push(f);
        }
        let m = marks(&fs);
        assert_eq!(
            m.iter().map(|x| x.1).collect::<Vec<_>>(),
            vec![MarkShape::Mmco6, MarkShape::Mmco3, MarkShape::Mmco3]
        );
        assert_eq!(m[1].0 - m[0].0, MARK_PERIOD as usize);
    }

    #[test]
    fn a_failed_mark_is_made_again_in_the_mmco6_shape() {
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        let pic = first_idr(&mut sim).header.idr_pic_id;
        let mut fs = Vec::new();
        for _ in 0..120 {
            let f = sim.encode(false);
            if f.mark == MarkShape::Mmco6 && fs.iter().all(|g: &SimFrame| g.mark == MarkShape::None)
            {
                sim.feedback(BackendFeedback::LtrMarkFailed {
                    idr_pic_id: pic,
                    frame_num: f.header.frame_num,
                });
            }
            fs.push(f);
        }
        let m = marks(&fs);
        assert!(
            m.len() >= 2 && m.iter().all(|x| x.1 == MarkShape::Mmco6),
            "{m:?}"
        );
    }

    /// The rules the library applies silently: the carrier's number, a fed-back
    /// mark, and a current number not below the last recovery's.
    #[test]
    fn recovery_rules_match_the_library() {
        let setup = || {
            let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
            let pic = first_idr(&mut sim).header.idr_pic_id;
            let mut carrier = 0;
            for _ in 0..40 {
                let f = sim.encode(false);
                if f.mark == MarkShape::Mmco6 {
                    carrier = f.header.frame_num;
                    sim.feedback(BackendFeedback::LtrMarked {
                        idr_pic_id: pic,
                        frame_num: carrier,
                    });
                }
            }
            (sim, pic, carrier)
        };
        let req = |last, cur, pic| BackendFeedback::LtrRecover {
            idr_pic_id: pic,
            last_correct_frame_num: last,
            current_frame_num: cur,
        };
        // A fed-back mark by its carrier: the next frame is a P from the LT.
        let (mut sim, pic, c) = setup();
        sim.feedback(req(c, 45, pic));
        let f = sim.encode(false);
        assert!(f.recovery && f.kind == SimKind::FromLt && !f.header.idr);
        // From the IDR's picture while it is still held.
        let (mut sim, pic, _) = setup();
        sim.feedback(req(0, 45, pic));
        assert!(sim.encode(false).recovery);
        // A number that is nothing: one P from a long-term picture, no IDR.
        let (mut sim, pic, c) = setup();
        sim.feedback(req(c + 5, 45, pic));
        let (a, b) = (sim.encode(false), sim.encode(false));
        assert!(a.kind == SimKind::FromLt && !a.header.idr && !b.header.idr);
        assert_eq!(sim.stats.unknown_frames, 1);
        // The marked picture's number of an MMCO 3 mark, or a mark that was
        // never fed back: one P from a long-term picture, then an IDR.
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        let pic = first_idr(&mut sim).header.idr_pic_id;
        let mut carrier = 0;
        for _ in 0..40 {
            let f = sim.encode(false);
            if f.mark == MarkShape::Mmco6 {
                carrier = f.header.frame_num;
            }
        }
        sim.feedback(req(carrier, 45, pic));
        let (a, b) = (sim.encode(false), sim.encode(false));
        assert!(!a.header.idr && b.header.idr, "recovery from an unfed mark");
        // A stale current is ignored until the next mark.
        let (mut sim, pic, c) = setup();
        sim.feedback(req(c, 45, pic));
        let _ = sim.encode(false); // the recovery P (frame 41)
        let before = sim.stats.recoveries;
        sim.feedback(req(c, 38, pic));
        assert_eq!(sim.stats.stale_ignored, 1);
        assert!(!sim.encode(false).recovery);
        assert_eq!(sim.stats.recoveries, before);
        // The newest current is accepted.
        sim.feedback(req(c, 60, pic));
        assert!(sim.encode(false).recovery);
        // A stale IDR period is ignored.
        let (mut sim, pic, c) = setup();
        sim.feedback(req(c, 45, pic + 7));
        assert!(!sim.encode(false).recovery);
    }

    #[test]
    fn frame_numbers_compare_modulo_max_frame_num() {
        let max = 32768;
        assert!(fn_before(10, 20, max));
        assert!(!fn_before(20, 10, max));
        assert!(!fn_before(7, 7, max));
        // Across the wrap, the nearest distance decides.
        assert!(
            fn_before(32_760, 5, max),
            "32760 is behind 5 after the wrap"
        );
        assert!(!fn_before(5, 32_760, max));
    }

    /// The index advances only on a success: a failed mark is made again into
    /// its own index, the one after a success into the other.
    #[test]
    fn a_failed_mark_keeps_its_index_and_a_success_moves_it() {
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        let pic = first_idr(&mut sim).header.idr_pic_id;
        let mut idx = Vec::new();
        let mut n = 0;
        for _ in 0..260 {
            let f = sim.encode(false);
            if f.mark == MarkShape::None {
                continue;
            }
            n += 1;
            idx.push(f.mark_index().expect("a mark has an index"));
            let marked = match f.mark {
                MarkShape::Mmco6 => f.header.frame_num,
                _ => f.header.frame_num - 1,
            };
            // Fail the first two, then succeed.
            sim.feedback(if n <= 2 {
                BackendFeedback::LtrMarkFailed {
                    idr_pic_id: pic,
                    frame_num: marked,
                }
            } else {
                BackendFeedback::LtrMarked {
                    idr_pic_id: pic,
                    frame_num: marked,
                }
            });
        }
        assert!(idx.len() >= 5, "{idx:?}");
        assert_eq!(idx[0], idx[1], "{idx:?}");
        assert_eq!(idx[1], idx[2], "{idx:?}");
        assert_ne!(idx[2], idx[3], "{idx:?}");
        assert_ne!(idx[3], idx[4], "{idx:?}");
    }

    /// A recovery asked for in the batch that confirms the mark it names is
    /// answered from the older anchor (the library latches mark feedback for
    /// the next frame).
    #[test]
    fn a_recovery_in_the_batch_of_its_confirmation_comes_from_the_older_anchor() {
        let mut sim = Openh264Sim::new(SimPolicy::FAITHFUL);
        let pic = first_idr(&mut sim).header.idr_pic_id;
        let mut carrier = 0;
        for _ in 0..40 {
            let f = sim.encode(false);
            if f.mark == MarkShape::Mmco6 {
                carrier = f.header.frame_num;
            }
        }
        sim.feedback(BackendFeedback::LtrMarked {
            idr_pic_id: pic,
            frame_num: carrier,
        });
        sim.feedback(BackendFeedback::LtrRecover {
            idr_pic_id: pic,
            last_correct_frame_num: carrier,
            current_frame_num: 45,
        });
        let f = sim.encode(false);
        assert!(f.recovery && f.kind == SimKind::FromLt && !f.header.idr);
        // The IDR's picture (index 0), not the mark just confirmed (index 1).
        assert_eq!(f.header.rplm, vec![Rplm::Long(0)]);
        assert_eq!(sim.stats.unknown_frames, 0, "no IDR follows");
        assert!(!sim.encode(false).header.idr);
    }
}
