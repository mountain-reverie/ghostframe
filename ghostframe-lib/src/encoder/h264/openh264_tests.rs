//! B7-2: the openh264 backend against the real library.
//!
//! Every test here needs libopenh264 (2.4.x, 2.5.x or 2.6.x) and fails
//! without it, with an install hint: a skipped test is not a passing one.
//! Debian/Ubuntu: `apt install libopenh264-7`; Fedora: the Cisco repository;
//! or point `GHOSTFRAME_H264_OPENH264_LIB` at a library when running them.
//!
//! The decoder is libavcodec (`PcmDecoder`, `err_recognition = explode` for
//! the strict reads), so a stream that decodes here decodes in a client.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use super::backend::{
    BackendError, BackendFeedback, EncodedAu, H264Backend, Nv12View, RateTarget, StreamConfig,
};
use super::observed_tests::{
    assert_idr_accounting, assert_marks_resume, assert_recovered, cobatched, count_events,
    has_cobatch, run_with, LoopEngine, Script,
};
use super::openh264::{nv12_to_i420, OpenH264Backend};
use super::openh264_sim::{sim_params, MarkShape, Openh264Sim, SimFrame, SimPolicy};
use super::openh264_sys::{Lib, LOW_COMPLEXITY, MEDIUM_COMPLEXITY};
use super::pcm_decoder::{PcmDecoder, Strictness};
use super::refs::{FramePlan, IdrCause, Intent};
use super::syntax::{Mmco, Rplm, SliceHeader, StreamParams};
use super::tracker::PicKind;

// ---------------------------------------------------------------- content

#[derive(Clone, Copy, Debug)]
enum Scene {
    /// A gradient, static text rows and a moving textured block: every
    /// picture differs from every other.
    Calm,
    /// `Calm` plus per-pixel noise of this amplitude.
    Busy(u8),
    /// Nothing like `Calm`: a different pattern per seed.
    Other(u32),
    /// Full-range noise, new every frame: the worst case for a rate control.
    Noise,
}

fn xorshift(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *state = x;
    x
}

/// A packed NV12 picture.
fn scene_nv12(i: usize, w: usize, h: usize, scene: Scene) -> Vec<u8> {
    let mut y = vec![0u8; w * h];
    let (cw, ch) = (w / 2, h / 2);
    let mut u = vec![128u8; cw * ch];
    let v = vec![128u8; cw * ch];
    let mut rng = 0x9e37_79b9_7f4a_7c15u64 ^ ((i as u64 + 1) * 0x1000_0000_01b3);
    match scene {
        Scene::Calm | Scene::Busy(_) => {
            for r in 0..h {
                for c in 0..w {
                    y[r * w + c] = (30 + (r + c) / 3 + i * 3 / 2) as u8;
                }
            }
            for r in 0..16 {
                for c in 0..w {
                    if (r / 4) % 2 == 0 && (c / 3) % 2 == 0 {
                        y[r * w + c] = 235;
                    }
                }
            }
            let (bx, by) = ((i * 5) % (w - 24), 24 + (i * 3) % (h - 24 - 16));
            for r in 0..16 {
                for c in 0..24 {
                    y[(by + r) * w + bx + c] = (90 + (((c ^ r) + i * 7) & 63)) as u8;
                }
            }
            for r in 0..8 {
                for c in 0..12 {
                    u[((by + r * 2) / 2) * cw + bx / 2 + c] = (100 + (i & 31)) as u8;
                }
            }
            if let Scene::Busy(amp) = scene {
                for p in y.iter_mut() {
                    *p = p.saturating_add((xorshift(&mut rng) % u64::from(amp.max(1))) as u8);
                }
            }
        }
        Scene::Other(seed) => {
            for r in 0..h {
                for c in 0..w {
                    y[r * w + c] =
                        (((r as u32 * (seed * 7 + 3)) ^ (c as u32 * 13 + seed)) & 255) as u8;
                }
            }
            for p in u.iter_mut() {
                *p = (seed * 29) as u8;
            }
        }
        Scene::Noise => {
            for p in y.iter_mut() {
                *p = xorshift(&mut rng) as u8;
            }
            for p in u.iter_mut() {
                *p = xorshift(&mut rng) as u8;
            }
        }
    }
    let mut nv12 = y;
    nv12.reserve(w * h / 2);
    for k in 0..cw * ch {
        nv12.push(u[k]);
        nv12.push(v[k]);
    }
    nv12
}

fn view(data: &[u8], w: usize, h: usize) -> Nv12View<'_> {
    Nv12View {
        data,
        width: w as u32,
        height: h as u32,
        y_stride: w,
        uv_stride: w,
        uv_offset: w * h,
    }
}

fn plan(seq: u32, idr: bool) -> FramePlan {
    FramePlan {
        epoch: 0,
        frame_seq: seq,
        kind: if idr { PicKind::Idr } else { PicKind::Healthy },
        idr_cause: idr.then_some(IdrCause::SessionStart),
        nal_ref_idc: 3,
        frame_num: 0,
        poc: 0,
        target: None,
        intent: Intent::Executed,
        mark: None,
        mark_target: false,
        mmco: Vec::new(),
        rplm: Vec::new(),
    }
}

fn require_lib() {
    if let Err(e) = Lib::get(None) {
        panic!(
            "{e}\nthese tests need libopenh264 2.4.x-2.6.x: Debian/Ubuntu `apt install \
             libopenh264-7`, Fedora the Cisco repository (or run them with \
             GHOSTFRAME_H264_OPENH264_LIB set)"
        );
    }
}

fn config(w: usize, h: usize, bps: u32) -> StreamConfig {
    StreamConfig::new(w as u32, h as u32, 30, bps)
}

fn open(w: usize, h: usize, bps: u32) -> OpenH264Backend {
    require_lib();
    let mut be = OpenH264Backend::new(None);
    be.open(&config(w, h, bps)).expect("open");
    be
}

#[allow(
    clippy::disallowed_methods,
    reason = "measures real CPU time of a synchronous encode; no tokio clock is involved"
)]
fn since(t: Instant) -> Duration {
    t.elapsed()
}

fn sha(b: &[u8]) -> String {
    Sha256::digest(b)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

// ------------------------------------------------------------------- rig

#[derive(Debug, Clone, Copy)]
struct Mark {
    idx: usize,
    carrier_fn: u32,
    marked_fn: u32,
    mmco6: bool,
    idr_pic_id: u32,
}

struct Au {
    payload: Vec<u8>,
    header: SliceHeader,
}

/// A backend driven frame by frame, with the marking feedback a client would
/// send (unless withheld) and the stream it produced.
struct Rig {
    be: OpenH264Backend,
    w: usize,
    h: usize,
    aus: Vec<Au>,
    idr_pic_id: u32,
    /// Every mark seen, and the ones fed back.
    marks: Vec<Mark>,
    confirmed: Vec<Mark>,
    due: Vec<(usize, Mark)>,
    /// Confirm marks this many frames after their carrier; `None` never.
    confirm_delay: Option<usize>,
}

impl Rig {
    fn new(w: usize, h: usize, bps: u32) -> Self {
        Self::with(open(w, h, bps), w, h)
    }

    fn with(be: OpenH264Backend, w: usize, h: usize) -> Self {
        Self {
            be,
            w,
            h,
            aus: Vec::new(),
            idr_pic_id: 0,
            marks: Vec::new(),
            confirmed: Vec::new(),
            due: Vec::new(),
            confirm_delay: Some(2),
        }
    }

    /// Encode one frame; the first is an IDR, others only on `force_idr`.
    fn step(&mut self, nv12: &[u8], force_idr: bool) -> &Au {
        let k = self.aus.len();
        if let Some(d) = self.confirm_delay {
            let _ = d;
            let (now, later): (Vec<_>, Vec<_>) = self.due.drain(..).partition(|(at, _)| *at <= k);
            self.due = later;
            for (_, m) in now {
                self.be.feedback(BackendFeedback::LtrMarked {
                    idr_pic_id: m.idr_pic_id,
                    frame_num: m.marked_fn,
                });
                self.confirmed.push(m);
            }
        }
        let au = self
            .be
            .encode(
                &view(nv12, self.w, self.h),
                &plan(k as u32, k == 0 || force_idr),
                &[],
            )
            .unwrap_or_else(|e| panic!("frame {k}: {e}"));
        self.record(au)
    }

    fn record(&mut self, au: EncodedAu) -> &Au {
        let k = self.aus.len();
        let header = au.meta.header.clone().expect("a parsed header");
        assert_eq!(au.meta.idr, header.idr);
        if header.idr {
            self.idr_pic_id = header.idr_pic_id;
            // A new IDR period: marks and feedback of the old one are void.
            self.due.clear();
        }
        let mmco = header.mmco.clone().unwrap_or_default();
        let mark = if mmco.iter().any(|m| matches!(m, Mmco::CurrentToLong(_))) {
            Some((header.frame_num, true))
        } else if mmco.iter().any(|m| matches!(m, Mmco::ShortToLong(..))) {
            // The picture before the carrier becomes long-term.
            Some(((header.frame_num + 0x8000 - 1) & 0x7fff, false))
        } else {
            None
        };
        if let Some((marked_fn, mmco6)) = mark {
            let m = Mark {
                idx: k,
                carrier_fn: header.frame_num,
                marked_fn,
                mmco6,
                idr_pic_id: self.idr_pic_id,
            };
            self.marks.push(m);
            if let Some(d) = self.confirm_delay {
                self.due.push((k + d, m));
            }
        }
        self.aus.push(Au {
            payload: au.payload,
            header,
        });
        self.aus.last().unwrap()
    }

    fn newest_frame_num(&self) -> u32 {
        self.aus.last().expect("a frame").header.frame_num
    }

    /// Ask for a recovery from the newest confirmed mark, the way the
    /// reference manager will: the CARRIER's frame_num as the last correct
    /// picture, the newest frame_num encoded as the current one.
    fn recover(&mut self) -> Mark {
        let m = *self.confirmed.last().expect("a confirmed mark");
        self.recover_with(m, m.carrier_fn, self.newest_frame_num());
        m
    }

    fn recover_with(&mut self, m: Mark, last_correct: u32, current: u32) {
        self.be.feedback(BackendFeedback::LtrRecover {
            idr_pic_id: m.idr_pic_id,
            last_correct_frame_num: last_correct,
            current_frame_num: current,
        });
    }

    fn idr_count_after_first(&self) -> usize {
        self.aus.iter().skip(1).filter(|a| a.header.idr).count()
    }
}

/// Decode the stream without the frames in `skip`; the picture of each frame
/// fed, `None` where the decoder produced none or flagged damage.
fn decode(aus: &[Au], strictness: Strictness, skip: &BTreeSet<usize>) -> Vec<Option<Vec<u8>>> {
    let mut d = PcmDecoder::new(strictness);
    aus.iter()
        .enumerate()
        .filter(|(i, _)| !skip.contains(i))
        .map(|(i, a)| {
            d.feed(&a.payload, i as i64)
                .ok()
                .filter(|p| !p.damaged)
                .map(|p| p.yuv)
        })
        .collect()
}

/// For each frame index, the picture decoded with `skip` omitted, or `None`.
fn decode_indexed(
    aus: &[Au],
    strictness: Strictness,
    skip: &BTreeSet<usize>,
) -> Vec<Option<String>> {
    let got = decode(aus, strictness, skip);
    let mut it = got.into_iter();
    (0..aus.len())
        .map(|i| {
            if skip.contains(&i) {
                None
            } else {
                it.next().unwrap().map(|p| sha(&p))
            }
        })
        .collect()
}

// ----------------------------------------------------------------- tests

/// Test 1 (the layout) lives with the bindings: `openh264_sys::tests`.
#[test]
fn openh264_library_loads_and_reports_a_supported_version() {
    require_lib();
    let lib = Lib::get(None).unwrap();
    eprintln!(
        "openh264 {} ({:?} layout) from {}",
        lib.version, lib.layout, lib.origin
    );
    assert!(lib.origin.starts_with("libopenh264.so"));
}

/// Test 2: the stream the backend makes is decodable, carries marks of both
/// shapes, and the settings that keep it quiet hold.
#[test]
fn openh264_encodes_marks_and_pins_the_stream_shape() {
    let mut rig = Rig::new(128, 128, 100_000);
    for i in 0..70 {
        rig.step(&scene_nv12(i, 128, 128, Scene::Calm), false);
    }
    assert!(rig.aus[0].header.idr);
    assert_eq!(rig.idr_count_after_first(), 0, "no unrequested IDR");
    assert!(
        rig.aus[0].payload.windows(4).any(|w| w == [0, 0, 0, 1]),
        "Annex B"
    );
    // Premise: marks happened, so the shape checks below look at something.
    eprintln!("marks: {:?}", rig.marks);
    assert!(
        rig.marks.len() >= 2,
        "premise: marks >= 2, got {}",
        rig.marks.len()
    );
    assert!(
        rig.marks[0].mmco6,
        "the first mark is MMCO 6: {:?}",
        rig.marks[0]
    );
    assert!(
        rig.marks[1..].iter().all(|m| !m.mmco6),
        "later marks are MMCO 3: {:?}",
        rig.marks
    );
    // Every slice parses (the backend would have failed otherwise) and the
    // whole stream decodes strictly.
    let pics = decode(&rig.aus, Strictness::Strict, &BTreeSet::new());
    assert_eq!(pics.len(), 70);
    assert!(
        pics.iter().all(Option::is_some),
        "a picture for every access unit"
    );
    let distinct: BTreeSet<String> = pics.iter().flatten().map(|p| sha(p)).collect();
    assert_eq!(distinct.len(), 70, "premise: every picture differs");
    assert_eq!(rig.be.stats().frames, 70);
    assert_eq!(rig.be.stats().idr_forced, 1);
}

/// An IDR comes when the plan is one and at no other time; it carries the
/// parameter sets, a new `idr_pic_id`, and restarts `frame_num`.
#[test]
fn openh264_makes_an_idr_exactly_when_the_plan_is_one() {
    let mut rig = Rig::new(128, 128, 100_000);
    for i in 0..60 {
        rig.step(&scene_nv12(i, 128, 128, Scene::Calm), i == 30);
    }
    let idrs: Vec<usize> = (0..60).filter(|&i| rig.aus[i].header.idr).collect();
    assert_eq!(idrs, vec![0, 30]);
    let has_params = |i: usize| {
        super::syntax::split_annexb(&rig.aus[i].payload)
            .iter()
            .any(|n| n[0] & 0x1f == super::syntax::NAL_SPS)
    };
    assert!(has_params(0) && has_params(30), "parameter sets on an IDR");
    assert!((1..60).filter(|&i| i != 30).all(|i| !has_params(i)));
    assert_ne!(
        rig.aus[0].header.idr_pic_id, rig.aus[30].header.idr_pic_id,
        "a new IDR period"
    );
    assert_eq!(rig.aus[31].header.frame_num, 1, "frame_num restarts");
    assert_eq!(rig.be.stats().idr_forced, 2);
    let pics = decode(&rig.aus, Strictness::Strict, &BTreeSet::new());
    assert!(pics.iter().all(Option::is_some));
}

/// Scene-change detection is off: unrelated content in the middle of the
/// stream produces no IDR. With it on (the library default) it does.
#[test]
fn openh264_scene_change_detection_is_off() {
    fn run(be: OpenH264Backend) -> Rig {
        let mut rig = Rig::with(be, 128, 128);
        for i in 0..70 {
            let scene = if (35..45).contains(&i) {
                Scene::Other(5)
            } else {
                Scene::Calm
            };
            rig.step(&scene_nv12(i, 128, 128, scene), false);
        }
        rig
    }
    // Premise: the swap changes at least 90 % of the luma samples.
    let (a, b) = (
        scene_nv12(34, 128, 128, Scene::Calm),
        scene_nv12(35, 128, 128, Scene::Other(5)),
    );
    let changed = a[..128 * 128]
        .iter()
        .zip(&b[..128 * 128])
        .filter(|(x, y)| x != y)
        .count();
    assert!(changed * 10 >= 128 * 128 * 9, "premise: {changed} changed");

    require_lib();
    let off = run(open(128, 128, 100_000));
    assert_eq!(off.idr_count_after_first(), 0, "scd off: no IDR on a swap");
    let mut on_be = OpenH264Backend::new(None).with_scene_change_detect();
    on_be.open(&config(128, 128, 100_000)).unwrap();
    let on = run(on_be);
    assert!(
        on.idr_count_after_first() > 0,
        "premise: with scd on the library makes IDRs of its own"
    );
}

/// A mark whose carrier never arrived: without feedback the library marks
/// once and never again; `LtrMarkFailed` makes it mark again (MMCO-6 shape).
#[test]
fn openh264_mark_failed_feedback_makes_the_library_mark_again() {
    fn run(send_failed: bool) -> Rig {
        let mut rig = Rig::new(128, 128, 100_000);
        rig.confirm_delay = None;
        let mut failed_at = None;
        for i in 0..110 {
            if send_failed && failed_at.is_none() {
                if let Some(m) = rig.marks.first().copied().filter(|m| i >= m.idx + 3) {
                    rig.be.feedback(BackendFeedback::LtrMarkFailed {
                        idr_pic_id: m.idr_pic_id,
                        frame_num: m.marked_fn,
                    });
                    failed_at = Some(i);
                }
            }
            rig.step(&scene_nv12(i, 128, 128, Scene::Calm), false);
        }
        rig
    }
    require_lib();
    let silent = run(false);
    assert_eq!(
        silent.marks.len(),
        1,
        "premise: with no feedback there is one mark in 110 frames: {:?}",
        silent.marks
    );
    let healed = run(true);
    let first = healed.marks[0];
    assert!(first.mmco6);
    let again = healed
        .marks
        .iter()
        .skip(1)
        .find(|m| m.idx <= first.idx + 3 + 40);
    assert!(
        again.is_some_and(|m| m.mmco6),
        "an MMCO-6-shaped re-mark within 40 frames of FAILED: {:?}",
        healed.marks
    );
    assert_eq!(healed.idr_count_after_first(), 0);
}

/// One recovery scenario on 128x128: `drops` are frames the client never
/// receives; before encoding `recover_at` the request goes out.
struct Recovery {
    rig: Rig,
    dropped: BTreeSet<usize>,
    mark: Mark,
}

fn recovery_run(drops: std::ops::RangeInclusive<usize>, total: usize, request: bool) -> Recovery {
    let mut rig = Rig::new(128, 128, 100_000);
    let recover_at = drops.end() + 1;
    let mut mark = None;
    for i in 0..total {
        if request && i == recover_at {
            mark = Some(rig.recover());
        }
        rig.step(&scene_nv12(i, 128, 128, Scene::Calm), false);
    }
    let mark = mark.unwrap_or_else(|| *rig.confirmed.last().expect("a confirmed mark"));
    Recovery {
        rig,
        dropped: drops.collect(),
        mark,
    }
}

/// How many pictures after the loss the client decodes exactly as the full
/// decode of the same stream does.
fn matching_after(r: &Recovery) -> (usize, usize) {
    let full = decode_indexed(&r.rig.aus, Strictness::Strict, &BTreeSet::new());
    let lossy = decode_indexed(&r.rig.aus, Strictness::Lenient, &r.dropped);
    let after = r.dropped.iter().max().unwrap() + 1;
    let n = r.rig.aus.len();
    let ok = (after..n).filter(|&f| lossy[f] == full[f]).count();
    (ok, n - after)
}

/// Test 3: after a loss the library predicts from the long-term picture the
/// client confirmed, and every later picture matches the full decode.
#[test]
fn openh264_recovery_from_a_confirmed_long_term_picture() {
    // Frames 36-40 are lost. The first mark (MMCO 6) is the confirmed one.
    let good = recovery_run(36..=40, 90, true);
    eprintln!(
        "marks {:?}; confirmed {:?}",
        good.rig.marks, good.rig.confirmed
    );
    assert!(
        good.mark.idx + 2 < 36,
        "premise: the mark was confirmed before the loss: {:?}",
        good.mark
    );
    assert_eq!(good.rig.idr_count_after_first(), 0, "no IDR");
    let (ok, of) = matching_after(&good);
    assert!(of >= 40);
    assert_eq!(ok, of, "every later picture equals the full decode");
    // The recovery P is a reference P from the long-term picture.
    let rp = &good.rig.aus[41].header;
    assert!(rp.nal_ref_idc != 0 && !rp.idr);
    assert!(
        rp.rplm
            .iter()
            .any(|r| matches!(r, super::syntax::Rplm::Long(_))),
        "the recovery P names the long-term picture: {rp:?}"
    );

    // Control: the same loss with no request. The stream predicts from the
    // lost frames and the decoder never gets back.
    let control = recovery_run(36..=40, 90, false);
    let (cok, cof) = matching_after(&control);
    assert!(
        cok < cof / 2,
        "premise: without a request the client stays wrong ({cok}/{cof})"
    );
}

/// The rules the library applies silently, each pinned by an input that
/// breaks it: the carrier's number (not the marked picture's) and the newest
/// frame number (not an older one).
#[test]
fn openh264_recovery_request_rules() {
    // (a) The marked picture's frame_num as the last correct picture, for a
    // mark whose marked picture is not the carrier (MMCO 3): the library
    // answers with an IDR. Frames 66-69 lost; the newest mark is MMCO 3.
    let scenario = |last_correct_is_marked: bool| {
        let mut rig = Rig::new(128, 128, 100_000);
        let mut mark = None;
        for i in 0..100 {
            if i == 70 {
                let m = *rig.confirmed.last().unwrap();
                assert!(!m.mmco6, "premise: an MMCO 3 mark: {:?}", rig.confirmed);
                assert!(m.idx + 2 < 66, "premise: confirmed before the loss");
                let lc = if last_correct_is_marked {
                    m.marked_fn
                } else {
                    m.carrier_fn
                };
                rig.recover_with(m, lc, rig.newest_frame_num());
                mark = Some(m);
            }
            rig.step(&scene_nv12(i, 128, 128, Scene::Calm), false);
        }
        Recovery {
            rig,
            dropped: (66..=69).collect(),
            mark: mark.unwrap(),
        }
    };
    let good = scenario(false);
    assert_eq!(good.rig.idr_count_after_first(), 0);
    let (ok, of) = matching_after(&good);
    assert_eq!(ok, of, "carrier's frame_num: recovered");
    let bad = scenario(true);
    assert!(
        bad.rig.idr_count_after_first() > 0,
        "premise: the marked picture's frame_num is answered with an IDR"
    );

    // (b) A request whose current frame_num is below the last recovery's is
    // ignored, until the next mark. Loss 36-40, recovery at 41. A repeat for
    // that loss (current 38) at 50 changes nothing. A second loss (46-48)
    // asked for with the stale current 40 is not recovered; with the newest
    // (48) it is.
    let run = |second_current_stale: bool, stale_repeat: bool| {
        let mut rig = Rig::new(128, 128, 100_000);
        let mut first = None;
        for i in 0..100 {
            if i == 41 {
                first = Some(rig.recover());
            }
            if i == 50 && stale_repeat {
                let m = first.unwrap();
                rig.recover_with(m, m.carrier_fn, 38);
            }
            if i == 49 {
                let m = *rig.confirmed.last().unwrap();
                assert!(m.idx + 2 < 46, "premise: confirmed before the second loss");
                let cur = if second_current_stale {
                    40
                } else {
                    rig.newest_frame_num()
                };
                rig.recover_with(m, m.carrier_fn, cur);
            }
            rig.step(&scene_nv12(i, 128, 128, Scene::Calm), false);
        }
        rig
    };
    let both_lost: BTreeSet<usize> = (36..=40).chain(46..=48).collect();
    let verdict = |rig: &Rig| {
        let full = decode_indexed(&rig.aus, Strictness::Strict, &BTreeSet::new());
        let lossy = decode_indexed(&rig.aus, Strictness::Lenient, &both_lost);
        (49..rig.aus.len()).filter(|&f| lossy[f] != full[f]).count()
    };
    let fresh = run(false, false);
    assert_eq!(fresh.idr_count_after_first(), 0);
    assert_eq!(verdict(&fresh), 0, "premise: both recoveries work");
    // The stale repeat changes nothing in the stream.
    let repeated = run(false, true);
    assert_eq!(
        fresh
            .aus
            .iter()
            .map(|a| sha(&a.payload))
            .collect::<Vec<_>>(),
        repeated
            .aus
            .iter()
            .map(|a| sha(&a.payload))
            .collect::<Vec<_>>(),
        "a request with current below the last recovery's is ignored"
    );
    let stale = run(true, false);
    assert!(
        verdict(&stale) > 0,
        "premise: a stale current is ignored, so the client stays wrong"
    );
}

/// Test 4: rate changes, the probe-raise pattern, and a heavy scene.
fn bytes_of(rig: &Rig, range: std::ops::Range<usize>) -> usize {
    rig.aus[range].iter().map(|a| a.payload.len()).sum()
}

#[test]
fn openh264_set_rate_takes_effect_without_an_idr() {
    let (w, h) = (1280, 720);
    let mut be = open(w, h, 2_000_000);
    let mut rig_aus = Vec::new();
    let mut rate_changes = 0;
    for i in 0..100 {
        if i == 50 {
            be.set_rate(RateTarget::steady(500_000, 30));
        }
        let nv = scene_nv12(i, w, h, Scene::Busy(24));
        let au = be
            .encode(&view(&nv, w, h), &plan(i as u32, i == 0), &[])
            .unwrap();
        rig_aus.push(au);
        rate_changes = be.stats().rate_sets;
    }
    assert_eq!(rate_changes, 1, "one SetOption pair for one change");
    let sum = |r: std::ops::Range<usize>| rig_aus[r].iter().map(|a| a.payload.len()).sum::<usize>();
    let before = sum(20..50);
    let after = sum(70..100);
    eprintln!("rate 2 -> 0.5 Mbit: {before} B / 30 frames before, {after} B after");
    assert!(
        after * 5 <= before * 2,
        "bytes fall by >= 2.5x: {before} -> {after}"
    );
    assert!(rig_aus.iter().skip(1).all(|a| !a.meta.idr), "no IDR");
    // A change under 5 % is not passed on.
    be.set_rate(RateTarget::steady(510_000, 30));
    let nv = scene_nv12(100, w, h, Scene::Busy(24));
    be.encode(&view(&nv, w, h), &plan(100, false), &[]).unwrap();
    assert_eq!(be.stats().rate_sets, 1, "hysteresis");
}

#[test]
fn openh264_probe_raise_pattern_is_two_rate_sets_per_window() {
    // The bridge raises the rate for up to three frames of a probe window and
    // drops back (h264_rate.rs h264_probe_raise): two changes per window.
    let (w, h) = (1280, 720);
    let base = 2_000_000u32;
    let raised = 3_200_000u32;
    let mut be = open(w, h, base);
    let windows = 4u64;
    let mut aus = Vec::new();
    for i in 0..120usize {
        let in_window = (0..windows as usize).any(|k| {
            let s = 20 + 25 * k;
            (s..s + 3).contains(&i)
        });
        be.set_rate(RateTarget::steady(
            if in_window { raised } else { base },
            30,
        ));
        let nv = scene_nv12(i, w, h, Scene::Busy(24));
        aus.push(
            be.encode(&view(&nv, w, h), &plan(i as u32, i == 0), &[])
                .expect("no encode error across rate changes"),
        );
    }
    assert_eq!(
        be.stats().rate_sets,
        2 * windows,
        "two SetOption per window"
    );
    assert!(aus.iter().skip(1).all(|a| !a.meta.idr), "no IDR");
    assert_eq!(be.stats().frames, 120, "no skipped frame");
    let raised_bytes: usize = (0..windows as usize)
        .map(|k| {
            aus[20 + 25 * k..23 + 25 * k]
                .iter()
                .map(|a| a.payload.len())
                .sum::<usize>()
        })
        .sum();
    let normal: usize = aus[5..20].iter().map(|a| a.payload.len()).sum();
    eprintln!(
        "probe raise: {} B per raised frame, {} B per normal frame",
        raised_bytes / (3 * windows as usize),
        normal / 15
    );
}

#[test]
fn openh264_a_heavy_scene_is_absorbed_without_an_idr_or_a_skip() {
    let (w, h) = (1280, 720);
    let bps = 2_000_000u32;
    let mut be = open(w, h, bps);
    let cap = RateTarget::steady(bps, 30);
    let avg = (bps / 8 / 30) as usize;
    let mut sizes = Vec::new();
    let mut worst = Duration::ZERO;
    let mut idrs = 0;
    for i in 0..90usize {
        // A scene change into noise at 40, back to calm at 60.
        let scene = if (40..60).contains(&i) {
            Scene::Noise
        } else {
            Scene::Calm
        };
        let nv = scene_nv12(i, w, h, scene);
        let t = Instant::now();
        let au = be
            .encode(&view(&nv, w, h), &plan(i as u32, i == 0), &[])
            .unwrap_or_else(|e| panic!("frame {i}: {e}"));
        worst = worst.max(since(t));
        idrs += usize::from(au.meta.idr && i > 0);
        sizes.push(au.payload.len());
    }
    let max = *sizes.iter().max().unwrap();
    let over = sizes
        .iter()
        .skip(1)
        .filter(|&&s| s as u32 > cap.max_frame_bytes)
        .count();
    eprintln!(
        "heavy scene at {bps} bps: average frame {avg} B, cap {} B, max frame {max} B \
         ({}x average), {over} frames over the cap, slowest encode {:.1} ms",
        cap.max_frame_bytes,
        max / avg.max(1),
        worst.as_secs_f64() * 1e3
    );
    assert_eq!(idrs, 0, "no IDR from the scene change");
    assert_eq!(be.stats().frames, 90, "no frame skipped");
    assert!(
        sizes[40..45].iter().any(|&s| s > 3 * avg),
        "premise: the scene change is heavy"
    );
    assert!(
        over > 0,
        "premise: the cap is exceeded, as the library has no hard cap"
    );
    assert!(max <= 1_000_000, "a frame is bounded: {max} B");
    assert!(worst < Duration::from_millis(400), "{worst:?}");
}

/// Test 5: strided NV12 is the same picture as packed NV12.
#[test]
fn openh264_strided_nv12_encodes_like_packed() {
    let (w, h) = (128usize, 96usize);
    let (ys, uvs) = (w + 40, w + 24);
    let mut a = open(w, h, 200_000);
    let mut b = open(w, h, 200_000);
    for i in 0..8 {
        let packed = scene_nv12(i, w, h, Scene::Calm);
        // The same picture with row padding full of junk, and a gap between
        // the planes.
        let uv_offset = h * ys + 64;
        let mut padded = vec![0xAAu8; uv_offset + (h / 2) * uvs];
        for r in 0..h {
            padded[r * ys..r * ys + w].copy_from_slice(&packed[r * w..(r + 1) * w]);
        }
        for r in 0..h / 2 {
            padded[uv_offset + r * uvs..uv_offset + r * uvs + w]
                .copy_from_slice(&packed[w * h + r * w..w * h + (r + 1) * w]);
        }
        let pv = Nv12View {
            data: &padded,
            width: w as u32,
            height: h as u32,
            y_stride: ys,
            uv_stride: uvs,
            uv_offset,
        };
        let x = a
            .encode(&view(&packed, w, h), &plan(i as u32, i == 0), &[])
            .unwrap();
        let y = b.encode(&pv, &plan(i as u32, i == 0), &[]).unwrap();
        assert_eq!(x.payload, y.payload, "frame {i}");
    }
    // The conversion alone, against a direct computation.
    let packed = scene_nv12(3, w, h, Scene::Busy(30));
    let mut i420 = Vec::new();
    nv12_to_i420(&view(&packed, w, h), &mut i420);
    let (y, rest) = i420.split_at(w * h);
    let (u, v) = rest.split_at(w * h / 4);
    assert_eq!(y, &packed[..w * h]);
    for k in 0..w * h / 4 {
        assert_eq!(
            (u[k], v[k]),
            (packed[w * h + 2 * k], packed[w * h + 2 * k + 1])
        );
    }
}

/// Test 6: a missing library is `Unavailable`, naming the path.
#[test]
fn openh264_missing_library_is_unavailable() {
    let bogus = std::path::PathBuf::from("/nonexistent/dir/libopenh264-missing.so.8");
    let mut be = OpenH264Backend::new(Some(bogus.clone()));
    let err = be.open(&config(128, 128, 200_000)).expect_err("no library");
    match err {
        BackendError::Unavailable(m) => {
            assert!(m.contains("libopenh264-missing.so.8"), "{m}")
        }
        other => panic!("expected Unavailable, got {other:?}"),
    }
    // An explicit path that fails is not papered over by the system library.
    assert!(Lib::get(Some(&bogus)).is_err());
}

/// What the backend refuses before it loads anything.
#[test]
fn openh264_refuses_sizes_it_cannot_serve() {
    for (w, h, fps) in [
        (127u32, 128u32, 30u32),
        (128, 127, 30),
        (32, 32, 30),
        (63, 64, 30),
        (1920, 1088, 30),
        (3840, 2160, 30),
        (1280, 720, 60),
    ] {
        let mut be = OpenH264Backend::new(Some("/nonexistent/x.so".into()));
        match be.open(&StreamConfig::new(w, h, fps, 1_000_000)) {
            Err(BackendError::Incapable(_)) => {}
            other => panic!("{w}x{h}@{fps}: expected Incapable, got {other:?}"),
        }
    }
    // The ceiling itself is not refused on size grounds (the missing library
    // is what fails).
    let mut be = OpenH264Backend::new(Some("/nonexistent/x.so".into()));
    assert!(matches!(
        be.open(&StreamConfig::new(1920, 1080, 30, 1_000_000)),
        Err(BackendError::Unavailable(_))
    ));
}

/// Test 7: timing, printed and not gated: LOW against MEDIUM complexity, and
/// the NV12 to I420 conversion, at 720p and 1080p.
#[test]
fn openh264_timing_probe() {
    require_lib();
    for (w, h) in [(1280usize, 720usize), (1920, 1080)] {
        // Conversion alone.
        let nv = scene_nv12(1, w, h, Scene::Calm);
        let mut buf = Vec::new();
        nv12_to_i420(&view(&nv, w, h), &mut buf);
        let t = Instant::now();
        for _ in 0..200 {
            nv12_to_i420(&view(&nv, w, h), &mut buf);
        }
        eprintln!(
            "{w}x{h}: NV12 -> I420 {:.3} ms",
            since(t).as_secs_f64() * 1e3 / 200.0
        );
        for (name, scene) in [("calm", Scene::Calm), ("busy", Scene::Busy(12))] {
            for (cname, c) in [("LOW", LOW_COMPLEXITY), ("MEDIUM", MEDIUM_COMPLEXITY)] {
                let mut be = OpenH264Backend::new(None).with_complexity(c);
                be.open(&config(w, h, 4_000_000)).unwrap();
                let frames: Vec<Vec<u8>> = (0..61).map(|i| scene_nv12(i, w, h, scene)).collect();
                let mut p_time = Duration::ZERO;
                let mut idr_time = Duration::ZERO;
                let mut bytes = 0usize;
                for (i, nv) in frames.iter().enumerate() {
                    let t = Instant::now();
                    let au = be
                        .encode(&view(nv, w, h), &plan(i as u32, i == 0), &[])
                        .unwrap();
                    let d = since(t);
                    if i == 0 {
                        idr_time = d;
                    } else {
                        p_time += d;
                        bytes += au.payload.len();
                    }
                }
                eprintln!(
                    "{w}x{h} {name} {cname}: P {:.2} ms/frame, IDR {:.1} ms, {} B/P frame at 4 Mbit",
                    p_time.as_secs_f64() * 1e3 / 60.0,
                    idr_time.as_secs_f64() * 1e3,
                    bytes / 60
                );
            }
        }
    }
}

// ------------------------------------------- differential: library vs sim

/// What the reference manager reads from a header, without slot indices or
/// frame numbers: the properties the sim promises to share with the library.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Shape {
    idr: bool,
    /// The list modification names a long-term index.
    from_lt: bool,
    nal_ref_idc: u8,
    mark: MarkShape,
    /// For a mark: whether it writes the same long-term index as the mark
    /// before it in the IDR period (the IDR's own picture counts as index 0).
    /// The absolute index is the library's choice; whether a failed mark is
    /// made again into its own index, and a success moves on, is a rule.
    same_slot: Option<bool>,
}

fn shape_of(h: &SliceHeader) -> Shape {
    let mmco = h.mmco.clone().unwrap_or_default();
    let mark = if mmco.iter().any(|m| matches!(m, Mmco::CurrentToLong(_))) {
        MarkShape::Mmco6
    } else if mmco.iter().any(|m| matches!(m, Mmco::ShortToLong(..))) {
        MarkShape::Mmco3
    } else {
        MarkShape::None
    };
    Shape {
        idr: h.idr,
        from_lt: h.rplm.iter().any(|r| matches!(r, Rplm::Long(_))),
        nal_ref_idc: h.nal_ref_idc,
        mark,
        same_slot: None,
    }
}

/// The long-term index a header's marking writes.
fn mark_index(h: &SliceHeader) -> Option<u32> {
    SimFrame::from_header(h.clone(), false).mark_index()
}

/// The library, or the sim, behind one interface.
trait Engine {
    fn frame(&mut self, i: usize, scene: Scene, idr: bool) -> SliceHeader;
    fn feedback(&mut self, fb: BackendFeedback);
}

struct RealEngine(Rig);

impl Engine for RealEngine {
    fn frame(&mut self, i: usize, scene: Scene, idr: bool) -> SliceHeader {
        let nv12 = scene_nv12(i, 128, 128, scene);
        self.0.step(&nv12, idr).header.clone()
    }
    fn feedback(&mut self, fb: BackendFeedback) {
        self.0.be.feedback(fb);
    }
}

struct SimEngine(Openh264Sim);

impl Engine for SimEngine {
    fn frame(&mut self, _i: usize, _scene: Scene, idr: bool) -> SliceHeader {
        self.0.encode(idr).header
    }
    fn feedback(&mut self, fb: BackendFeedback) {
        self.0.feedback(fb);
    }
}

/// What the script did, for the premise.
#[derive(Debug, Default, Clone, Copy)]
struct Premise {
    marks: usize,
    mark_failed: usize,
    recoveries: usize,
    stale_requests: usize,
    scene_change_frames: usize,
    unknown_requests: usize,
    marked_number_requests: usize,
    idrs_after_first: usize,
    cobatched: usize,
}

const DIFF_FRAMES: usize = 200;
const R1: usize = 50;
const R2: usize = 105;
const STALE: usize = 112;
const SCENE: usize = 125;
const UNKNOWN: usize = 150;
/// A request naming the MARKED picture of an MMCO 3 mark instead of its carrier.
const MARKED_NUM: usize = 170;
/// The first confirmation from here on is batched with a recovery request.
const COBATCH_FROM: usize = 128;

/// The event script of the differential test. Everything it does it decides
/// from the headers it has seen, so the same code drives both engines: it
/// feeds back each mark (two frames after its carrier), reports the second
/// failed instead (three frames after), asks for a recovery from the newest
/// fed-back mark twice (the second with a higher `current`), repeats a stale
/// request, swaps the content for one frame (no IDR may follow), and names an
/// unknown frame (a P, then an IDR).
fn drive(e: &mut dyn Engine) -> (Vec<Shape>, Vec<SliceHeader>, Premise) {
    let mut shapes = Vec::new();
    let mut headers: Vec<SliceHeader> = Vec::new();
    let mut p = Premise::default();
    // (due, feedback), and the marks fed back: (carrier_fn, marked_fn).
    let mut due: Vec<(usize, BackendFeedback)> = Vec::new();
    let mut fed: Vec<(u32, u32)> = Vec::new();
    let mut idr_pic_id = 0;
    let mut marks_this_period = 0;
    let mut prev_idx = 0;
    let mut cobatch_done = false;
    for i in 0..DIFF_FRAMES {
        let (now, later): (Vec<_>, Vec<_>) = due.drain(..).partition(|(d, _)| *d <= i);
        due = later;
        let mut confirmed_now = false;
        for (_, fb) in now {
            if let BackendFeedback::LtrMarked { frame_num, .. } = fb {
                if let Some(m) = headers
                    .iter()
                    .rev()
                    .find(|h| {
                        let s = shape_of(h);
                        (s.mark == MarkShape::Mmco6 && h.frame_num == frame_num)
                            || (s.mark == MarkShape::Mmco3
                                && (h.frame_num + 0x7fff) & 0x7fff == frame_num)
                    })
                    .map(|h| h.frame_num)
                {
                    fed.push((m, frame_num));
                    confirmed_now = true;
                }
            }
            e.feedback(fb);
        }
        // A loss found in the very batch that confirms a mark: the library
        // latches the confirmation for the next frame, so the recovery is
        // answered from the older anchor, not from the mark just confirmed.
        if confirmed_now && i >= COBATCH_FROM && !cobatch_done {
            cobatch_done = true;
            let newest = headers.last().map_or(0, |h| h.frame_num);
            e.feedback(BackendFeedback::LtrRecover {
                idr_pic_id,
                last_correct_frame_num: fed.last().expect("just confirmed").0,
                current_frame_num: newest,
            });
            p.cobatched += 1;
        }
        let newest = headers.last().map_or(0, |h| h.frame_num);
        let newest_fed = fed.last().map(|m| m.0);
        match i {
            R1 | R2 => {
                if let Some(c) = newest_fed {
                    e.feedback(BackendFeedback::LtrRecover {
                        idr_pic_id,
                        last_correct_frame_num: c,
                        current_frame_num: newest,
                    });
                    p.recoveries += 1;
                }
            }
            STALE => {
                if let Some(c) = newest_fed {
                    e.feedback(BackendFeedback::LtrRecover {
                        idr_pic_id,
                        last_correct_frame_num: c,
                        current_frame_num: 3,
                    });
                    p.stale_requests += 1;
                }
            }
            MARKED_NUM => {
                if let Some(&(c, m)) = fed.iter().rev().find(|(c, m)| c != m) {
                    let _ = c;
                    e.feedback(BackendFeedback::LtrRecover {
                        idr_pic_id,
                        last_correct_frame_num: m,
                        current_frame_num: newest,
                    });
                    p.marked_number_requests += 1;
                }
            }
            UNKNOWN => {
                e.feedback(BackendFeedback::LtrRecover {
                    idr_pic_id,
                    last_correct_frame_num: newest + 5,
                    current_frame_num: newest,
                });
                p.unknown_requests += 1;
            }
            _ => {}
        }
        let scene = if i == SCENE {
            p.scene_change_frames += 1;
            Scene::Other(9)
        } else {
            Scene::Calm
        };
        let h = e.frame(i, scene, i == 0);
        let mut s = shape_of(&h);
        if let Some(idx) = mark_index(&h) {
            s.same_slot = Some(idx == prev_idx);
            prev_idx = idx;
        }
        if h.idr {
            if i > 0 {
                p.idrs_after_first += 1;
            }
            idr_pic_id = h.idr_pic_id;
            prev_idx = 0;
            fed.clear();
            fed.push((0, 0));
            due.clear();
            marks_this_period = 0;
        }
        if s.mark != MarkShape::None {
            p.marks += 1;
            marks_this_period += 1;
            let marked = match s.mark {
                MarkShape::Mmco6 => h.frame_num,
                _ => (h.frame_num + 0x7fff) & 0x7fff,
            };
            if marks_this_period == 2 {
                p.mark_failed += 1;
                due.push((
                    i + 3,
                    BackendFeedback::LtrMarkFailed {
                        idr_pic_id,
                        frame_num: marked,
                    },
                ));
            } else {
                due.push((
                    i + 2,
                    BackendFeedback::LtrMarked {
                        idr_pic_id,
                        frame_num: marked,
                    },
                ));
            }
        }
        shapes.push(s);
        headers.push(h);
    }
    (shapes, headers, p)
}

/// The gate for the sim: the same event script on the real library and on the
/// sim gives the same kind of frame at every position (IDR or not, from a
/// long-term picture or the previous frame, the mark's shape), never
/// comparing slot indices or frame numbers. A divergence is the sim being
/// wrong about the library, and says where.
#[test]
fn openh264_the_sim_makes_the_same_kinds_of_frames_as_the_library() {
    require_lib();
    let mut real_rig = Rig::new(128, 128, 100_000);
    real_rig.confirm_delay = None;
    let (real, real_headers, p) = drive(&mut RealEngine(real_rig));
    let (sim, _, ps) = drive(&mut SimEngine(Openh264Sim::new(SimPolicy::FAITHFUL)));
    eprintln!("differential premise (library): {p:?}");
    eprintln!("differential premise (sim):     {ps:?}");

    // The premise: the script exercised what it claims to, on the library.
    assert!(p.marks >= 4, "marks: {p:?}");
    assert_eq!(p.mark_failed, 1, "{p:?}");
    assert_eq!(p.recoveries, 2, "two recoveries asked for: {p:?}");
    assert_eq!(p.stale_requests, 1);
    assert_eq!(p.cobatched, 1, "{p:?}");
    // The failed second mark was made again into its own index, and the next
    // mark, after a success, moved on.
    let rels: Vec<Option<bool>> = real.iter().filter_map(|s| s.same_slot.map(Some)).collect();
    assert!(
        rels.contains(&Some(true)) && rels.contains(&Some(false)),
        "premise: both slot relations occur: {rels:?}"
    );
    assert_eq!(p.scene_change_frames, 1);
    assert_eq!(p.unknown_requests, 1);
    let recovery_frames = (1..real.len())
        .filter(|&i| {
            real[i].from_lt
                && !real[i].idr
                && !real[i - 1].idr
                && real[i - 1].mark != MarkShape::Mmco6
        })
        .count();
    assert!(
        recovery_frames >= 2,
        "premise: recovery frames: {recovery_frames}"
    );
    // A scene change is not an IDR; a request naming the marked picture's
    // number is answered with one P from a long-term picture and then an IDR;
    // a number that is nothing is one P from a long-term picture and no IDR.
    assert!(!real[SCENE].idr && !real[SCENE + 1].idr, "scd is off");
    assert_eq!(
        p.idrs_after_first, 1,
        "only the marked-number request's: {p:?}"
    );
    assert_eq!(p.marked_number_requests, 1);
    let fallback_at = real
        .iter()
        .skip(1)
        .position(|s| s.idr)
        .map(|x| x + 1)
        .unwrap();
    assert_eq!(fallback_at, MARKED_NUM + 1, "a P, then an IDR");
    assert!(real[MARKED_NUM].from_lt && !real[MARKED_NUM].idr);
    assert!(real[UNKNOWN].from_lt && !real[UNKNOWN].idr && !real[UNKNOWN + 1].idr);

    if real != sim {
        let at = real.iter().zip(&sim).position(|(a, b)| a != b).unwrap();
        let lo = at.saturating_sub(3);
        let hi = (at + 4).min(real.len());
        panic!(
            "the sim diverges from the library at frame {at}\n library: {:?}\n sim:     {:?}\n headers: {:?}",
            &real[lo..hi],
            &sim[lo..hi],
            &real_headers[lo..hi]
        );
    }
}

// ---------------------------- closed loop: manager, library, wire, client

/// The library as the closed loop's encoder: the manager's plans go in, the
/// access units come out, parsed.
struct RealLoop {
    be: OpenH264Backend,
    i: usize,
    params: Option<StreamParams>,
}

impl RealLoop {
    fn boxed() -> Box<dyn LoopEngine> {
        Box::new(Self {
            be: open(128, 128, 100_000),
            i: 0,
            params: None,
        })
    }
}

impl LoopEngine for RealLoop {
    fn feedback(&mut self, fb: BackendFeedback) {
        self.be.feedback(fb);
    }
    fn encode(&mut self, _step: usize, plan: &FramePlan) -> SimFrame {
        let nv12 = scene_nv12(self.i, 128, 128, Scene::Calm);
        self.i += 1;
        let au = self
            .be
            .encode(&view(&nv12, 128, 128), plan, &[])
            .unwrap_or_else(|e| panic!("frame {}: {e}", self.i));
        if au.meta.params.is_some() {
            self.params = au.meta.params;
        }
        SimFrame::from_header(au.meta.header.expect("a parsed header"), false)
    }
    fn params(&self) -> StreamParams {
        self.params.unwrap_or_else(sim_params)
    }
}

/// Manager, library, a lossy and reordering wire, the client's `H264Gate` and a
/// `DecoderModel`: the properties of the sim loop (a truthful wire, events that
/// name what they should, no client violation, healing, IDR accounting), on the
/// real library. The sim is advisory; this is where a manager that leans on
/// the sim's choices would be found out.
#[test]
fn openh264_the_manager_heals_a_lossy_reordering_wire_in_a_closed_loop() {
    require_lib();
    // Loss bursts, with a stretch of reordered acknowledgements between.
    let mut s = 0x2545_f491_4f6c_dd1du64;
    let mut sc = Script::new(360, |step, _| {
        (70..=74).contains(&step) || (140..=142).contains(&step) || (200..=203).contains(&step)
    });
    sc.ack_extra = Box::new(move |step| {
        if (100..130).contains(&step) {
            (xorshift(&mut s) % 4) as usize
        } else {
            0
        }
    });
    let r = run_with(sc, RealLoop::boxed());
    let st = r.mgr.stats();
    assert!(
        st.ltr_recoveries_requested >= 2,
        "premise: recoveries: {st:?}"
    );
    assert!(st.reroots >= 2, "premise: re-roots: {st:?}");
    assert!(
        r.log.iter().filter(|l| l.lost).count() >= 12,
        "premise: losses"
    );
    assert_eq!(r.idr_headers(), 1, "no IDR: {st:?}");
    assert_eq!(st.plan_mismatch, 0, "{st:?}");
    assert_recovered(&r, 203, 60);
    assert_idr_accounting(&r);
    assert!(
        assert_marks_resume(&r, 360),
        "premise: the run outlives the deadline"
    );
}

/// The first mark's carrier is lost: the library re-makes the mark into its
/// own index and no client ever lacks the index an MMCO 3 then writes.
#[test]
fn openh264_the_loop_survives_the_first_marks_carrier_being_lost() {
    require_lib();
    let mut lost_one = false;
    let r = run_with(
        Script::new(320, move |_, l| {
            if l.sim.mark == MarkShape::Mmco6 && !l.sim.header.idr && !lost_one {
                lost_one = true;
                return true;
            }
            false
        }),
        RealLoop::boxed(),
    );
    let marks: Vec<_> = r
        .log
        .iter()
        .filter(|l| l.sim.mark != MarkShape::None)
        .collect();
    assert!(
        marks.len() >= 3 && marks[0].lost,
        "premise: the first carrier was lost"
    );
    assert_eq!(
        count_events(&r, |e| matches!(e, BackendFeedback::LtrMarkFailed { .. })),
        1
    );
    assert_eq!(marks[1].sim.mark_index(), marks[0].sim.mark_index());
    assert_eq!(r.idr_headers(), 1);
    assert!(r.mgr.stats().p_anchors_confirmed >= 3);
    assert!(assert_marks_resume(&r, 320));
    assert_idr_accounting(&r);
}

/// A confirmation and a recovery in one batch: the library recovers from the
/// older anchor and the manager takes that as the re-root, once.
#[test]
fn openh264_a_confirmation_and_a_recovery_in_one_batch_are_one_recovery() {
    require_lib();
    let mut found = 0;
    for at in 34..40 {
        let r = run_with(cobatched(at, 200), RealLoop::boxed());
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
        assert_eq!(r.idr_headers(), 1, "at {at}");
        assert_recovered(&r, at + 1, 40);
        assert_idr_accounting(&r);
    }
    assert!(
        found >= 1,
        "premise: a run batched the confirmation with the recovery"
    );
}

/// The frame number wraps at 32768 frames, and with no periodic IDR a stream
/// lives past it. The library compares frame numbers modulo `MaxFrameNum` and
/// silently drops a recovery whose current is behind its last recovery's; the
/// manager must keep naming the newest. 128x128 keeps the 33k frames to
/// seconds.
#[test]
fn openh264_recoveries_after_the_frame_num_wrap_get_a_recovery_frame() {
    require_lib();
    const STEPS: usize = 33_100;
    let r = run_with(
        Script::new(STEPS, |step, _| {
            [300, 32_700, 32_880, 32_890, 33_000]
                .iter()
                .any(|&at| (at..at + 3).contains(&step))
        }),
        RealLoop::boxed(),
    );
    let st = r.mgr.stats();
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
    assert!(
        currents.iter().any(|&c| c > 30_000)
            && currents.iter().filter(|&&c| c < 1_000).count() >= 2,
        "premise: requests either side of the wrap: {currents:?}"
    );
    // Every request was answered by a re-root (a frame from the long-term
    // picture), none ignored, no IDR to repair it.
    assert_eq!(r.idr_headers(), 1, "{st:?}");
    assert!(st.reroots as usize >= currents.len(), "{currents:?} {st:?}");
    assert_recovered(&r, 33_003, 60);
    assert_idr_accounting(&r);
}
