//! T4: the libva-direct backend against the real driver, decode-based.
//!
//! Local only. CI has no encoder and skips these by name prefix
//! (`h264_direct_`, see `.github/workflows/ci.yml`); a developer with a GPU
//! always sees them run. Run them one at a time through the GPU wrapper:
//!
//! ```text
//! GF_ALLOW_VAAPI=1 flock -o <bench lock> <gf-gpu-run> \
//!     cargo test -p ghostframe-lib --lib h264_direct_ -- --test-threads=1
//! ```
//!
//! Each scenario drives a real `RefManager` with simulated acknowledgements
//! and losses, encodes every plan with [`VaapiDirect`], and then decodes the
//! output with libavcodec (the encoder's own reconstruction cannot be read
//! back, S2). The oracle, as in S2:
//!
//! 1. the full stream decodes with `err_recognition=explode`, one picture per
//!    access unit, in the same call (zero output delay);
//! 2. the stream with the lost access units removed decodes every frame that
//!    does not depend on a lost one to the very same bytes as the full decode;
//! 3. every emitted header carries what the plan said, and the manager's
//!    replay of it agrees (`plan_mismatch` stays 0).
//!
//! Premise counters keep each scenario honest: a loss scenario must have
//! frames that depended on the loss, and they must differ.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::time::{Duration, Instant};

use ffmpeg_next as ffmpeg;
use ffmpeg_sys_next as ffi;

use super::backend::{
    AuMeta, BackendError, H264Backend, Nv12View, RateTarget, RcMode, StreamConfig,
};
use super::refs::{FramePlan, Plan, RefConfig, RefManager, RefStats};
use super::syntax::{Mmco, SliceHeader, StreamParams};
use super::tracker::PicKind;
use super::vaapi_direct::VaapiDirect;
use crate::encoder::h264_vaapi::FFMPEG_INIT;

// ------------------------------------------------------------------ source

fn hash(mut x: u32) -> u32 {
    x ^= x >> 16;
    x = x.wrapping_mul(0x7feb_352d);
    x ^= x >> 15;
    x = x.wrapping_mul(0x846c_a68b);
    x ^ (x >> 16)
}

/// Moving checkerboard, ramps and noise: every frame differs from every
/// other, so a wrong reference shows up in the decode. The top left corner
/// carries the frame number in binary.
fn source(w: u32, h: u32, n: i32) -> Vec<u8> {
    let (wu, hu) = (w as usize, h as usize);
    let mut out = vec![0u8; wu * hu * 3 / 2];
    for j in 0..h as i32 {
        for i in 0..w as i32 {
            let mut v = if ((i + 5 * n) / 24 + (j - 3 * n) / 24) & 1 != 0 {
                60
            } else {
                180
            };
            v += ((i * 3 + j * 2 + 11 * n) & 63) - 32;
            v += (hash((((i + 5 * n) >> 2) * 7919 + ((j - 3 * n) >> 2) * 104729) as u32) & 31)
                as i32
                - 16;
            if j < 40 && i < 16 * 20 {
                v = if (n >> (i / 16)) & 1 != 0 { 235 } else { 16 };
            }
            out[j as usize * wu + i as usize] = v.clamp(0, 255) as u8;
        }
    }
    let uv = wu * hu;
    for j in 0..h as i32 / 2 {
        for i in 0..w as i32 / 2 {
            let o = uv + j as usize * wu + 2 * i as usize;
            out[o] = (128 + ((i + 3 * n) & 63) - 32) as u8;
            out[o + 1] = (128 + ((j - 2 * n) & 63) - 32) as u8;
        }
    }
    out
}

fn view(data: &[u8], w: u32, h: u32) -> Nv12View<'_> {
    Nv12View {
        data,
        width: w,
        height: h,
        y_stride: w as usize,
        uv_stride: w as usize,
        uv_offset: (w * h) as usize,
    }
}

// ----------------------------------------------------------------- decoder

pub(crate) struct Decoded {
    /// Planar Y, U, V of each picture, cropped, by `pts` (= frame_seq).
    pub(crate) frames: BTreeMap<u32, Vec<u8>>,
    /// Pictures delivered by the call that consumed their own access unit.
    pub(crate) same_call: BTreeSet<u32>,
    pub(crate) errors: Vec<(u32, String)>,
}

pub(crate) fn decode(aus: &[(u32, &[u8])], explode: bool) -> Decoded {
    FFMPEG_INIT.call_once(|| ffmpeg::init().expect("ffmpeg::init"));
    let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::H264).expect("software h264 decoder");
    let mut ctx = ffmpeg::codec::context::Context::new_with_codec(codec);
    // SAFETY: `ctx` is allocated and not yet opened.
    unsafe {
        (*ctx.as_mut_ptr()).flags |= ffi::AV_CODEC_FLAG_LOW_DELAY as i32;
        if explode {
            (*ctx.as_mut_ptr()).err_recognition |= ffi::AV_EF_EXPLODE;
        }
    }
    let mut dec = ctx.decoder().video().expect("open decoder");
    let mut out = Decoded {
        frames: BTreeMap::new(),
        same_call: BTreeSet::new(),
        errors: Vec::new(),
    };
    for &(seq, data) in aus {
        let mut pkt = ffmpeg::Packet::copy(data);
        pkt.set_pts(Some(i64::from(seq)));
        if let Err(e) = dec.send_packet(&pkt) {
            out.errors.push((seq, e.to_string()));
            continue;
        }
        loop {
            let mut f = ffmpeg::frame::Video::empty();
            match dec.receive_frame(&mut f) {
                Ok(()) => {
                    let pts = f.pts().expect("decoded frame has a pts") as u32;
                    let (w, h) = (f.width() as usize, f.height() as usize);
                    let mut px = Vec::with_capacity(w * h * 3 / 2);
                    for (p, (pw, ph)) in [(w, h), (w / 2, h / 2), (w / 2, h / 2)].iter().enumerate()
                    {
                        for row in 0..*ph {
                            px.extend_from_slice(&f.data(p)[row * f.stride(p)..][..*pw]);
                        }
                    }
                    if pts == seq {
                        out.same_call.insert(pts);
                    }
                    out.frames.insert(pts, px);
                }
                Err(ffmpeg::Error::Other {
                    errno: libc::EAGAIN,
                }) => break,
                Err(e) => {
                    out.errors.push((seq, e.to_string()));
                    break;
                }
            }
        }
    }
    out
}

/// Luma PSNR of a decoded picture against the NV12 source, dB.
fn psnr(src_nv12: &[u8], decoded: &[u8], w: u32, h: u32) -> f64 {
    let n = (w * h) as usize;
    let sse: f64 = src_nv12[..n]
        .iter()
        .zip(&decoded[..n])
        .map(|(&a, &b)| f64::from((i32::from(a) - i32::from(b)).pow(2) as u32))
        .sum();
    if sse == 0.0 {
        return 99.0;
    }
    10.0 * (255.0 * 255.0 * n as f64 / sse).log10()
}

// ---------------------------------------------------------------- scenario

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    width: u32,
    height: u32,
    frames: u32,
    bps: u32,
    /// Frames whose access units never reach the client.
    lost: Range<u32>,
    /// Frames the client never acknowledges as decoded.
    silent: Range<u32>,
    /// Virtual time per frame.
    step: Duration,
    srtt: Duration,
    /// Frames until a decoded or refused bit comes back.
    ack_delay: u32,
    /// Frames until a ledger verdict on a lost frame.
    verdict_delay: u32,
    /// Called on the plan before it reaches the backend (negative control).
    corrupt: Option<fn(u32, &mut FramePlan)>,
    /// A new rate and the frame it is set at.
    rate_change: Option<(u32, u32)>,
    rc: RcMode,
    /// Lowest luma PSNR against the source for a frame that depends on no loss.
    psnr_floor: f64,
}

impl Scenario {
    fn new(name: &'static str, width: u32, height: u32, frames: u32) -> Self {
        Self {
            name,
            width,
            height,
            frames,
            bps: 6_000_000,
            lost: 0..0,
            silent: 0..0,
            step: Duration::from_millis(33),
            srtt: Duration::from_millis(66),
            ack_delay: 2,
            verdict_delay: 3,
            corrupt: None,
            rate_change: None,
            rc: RcMode::Vbr,
            psnr_floor: 30.0,
        }
    }

    fn is_lost(&self, seq: u32) -> bool {
        self.lost.contains(&seq)
    }
}

struct Sent {
    seq: u32,
    plan: FramePlan,
    au: Vec<u8>,
    meta: AuMeta,
    /// The manager's replay of the emitted header found the plan's reference.
    matched: bool,
    lost: bool,
    /// Lost, or predicted from a poisoned picture.
    poisoned: bool,
    encode_us: u128,
}

struct Run {
    sc: Scenario,
    sent: Vec<Sent>,
    stats: RefStats,
    /// Bytes of access units by frame.
    full: Decoded,
    dropped: Decoded,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ack {
    Decoded,
    Refused,
    Suspect,
}

fn open(sc: &Scenario) -> Option<VaapiDirect> {
    let mut cfg = StreamConfig::new(sc.width, sc.height, 30, sc.bps);
    cfg.params = StreamParams::writer();
    cfg.rc = sc.rc;
    let mut b = VaapiDirect::new();
    match b.open(&cfg) {
        Ok(()) => Some(b),
        // Skipped only when there is nothing to drive: no libva, no render
        // node, no encode entrypoint. A driver that is there but rejected by
        // the probe (`Incapable`) or errors (`Driver`) is a failure.
        Err(BackendError::Unavailable(why)) => {
            eprintln!("skipping {}: no usable libva encoder ({why})", sc.name);
            None
        }
        Err(e) => panic!("{}: open failed: {e}", sc.name),
    }
}

fn run(sc: &Scenario) -> Option<Run> {
    let mut backend = open(sc)?;
    let params = StreamParams::writer();
    let mut mgr = RefManager::new(RefConfig {
        params,
        frame_interval: sc.step,
        initial_srtt: sc.srtt,
        observed: false,
    });
    let t0 = Instant::now();
    let mut sent: Vec<Sent> = Vec::new();
    let mut poisoned: BTreeSet<u32> = BTreeSet::new();
    let mut due: BTreeMap<u32, Vec<(u32, Ack)>> = BTreeMap::new();
    for n in 0..sc.frames {
        let now = t0 + sc.step * n;
        for (seq, ack) in due.remove(&n).unwrap_or_default() {
            match ack {
                Ack::Decoded if sc.silent.contains(&seq) => {}
                Ack::Decoded => mgr.frame_decoded(seq),
                Ack::Refused => mgr.frame_refused(seq),
                Ack::Suspect => mgr.frame_suspect(seq),
            }
        }
        if let Some((at, bps)) = sc.rate_change {
            if at == n {
                backend.set_rate(RateTarget::steady(bps, 30));
            }
        }
        mgr.tick(now, sc.srtt);
        let Plan::Frame(plan) = mgr.plan_next(n, now) else {
            panic!("{}: unexpected hold at frame {n}", sc.name);
        };
        let mut sent_plan = plan.clone();
        if let Some(f) = sc.corrupt {
            f(n, &mut sent_plan);
        }
        let refs = mgr.resident_refs();
        let src = source(sc.width, sc.height, n as i32);
        let t = Instant::now();
        let au = backend
            .encode(&view(&src, sc.width, sc.height), &sent_plan, &refs)
            .unwrap_or_else(|e| panic!("{}: frame {n}: {e}", sc.name));
        #[allow(
            clippy::disallowed_methods,
            reason = "measures the real encode time of a synchronous driver call; no tokio clock here"
        )]
        let encode_us = t.elapsed().as_micros();
        let header: &SliceHeader = au.meta.header.as_ref().expect("direct AU is parsed");
        let report = mgr
            .frame_sent(&plan, header, 1, now)
            .expect("plan is from the current epoch");
        mgr.frag_acked(n, 0, now);

        let lost = sc.is_lost(n);
        let dependent = au.meta.ref_seq.is_some_and(|r| poisoned.contains(&r));
        let bad = lost || dependent;
        if bad {
            poisoned.insert(n);
        }
        let (at, ack) = if lost {
            (n + sc.verdict_delay, Ack::Suspect)
        } else if dependent {
            (n + sc.ack_delay, Ack::Refused)
        } else {
            (n + sc.ack_delay, Ack::Decoded)
        };
        due.entry(at).or_default().push((n, ack));
        sent.push(Sent {
            seq: n,
            plan,
            au: au.payload,
            meta: au.meta,
            matched: report.matched,
            lost,
            poisoned: bad,
            encode_us,
        });
    }
    let all: Vec<(u32, &[u8])> = sent.iter().map(|s| (s.seq, s.au.as_slice())).collect();
    let kept: Vec<(u32, &[u8])> = sent
        .iter()
        .filter(|s| !s.lost)
        .map(|s| (s.seq, s.au.as_slice()))
        .collect();
    let full = decode(&all, true);
    let dropped = decode(&kept, false);
    let stats = mgr.stats();
    Some(Run {
        sc: sc.clone(),
        sent,
        stats,
        full,
        dropped,
    })
}

impl Run {
    fn mean_encode_ms(&self) -> f64 {
        self.sent.iter().map(|s| s.encode_us).sum::<u128>() as f64 / self.sent.len() as f64 / 1000.0
    }

    /// The oracle. Returns `(compared, poisoned survivors, diverged)`.
    fn verify(&self) -> (usize, usize, usize) {
        let sc = &self.sc;
        let params = StreamParams::writer();
        // 3. every header says what the plan said.
        for s in &self.sent {
            assert!(
                s.meta.header_matches(&s.plan, &params),
                "{}: frame {} header differs from its plan: {:?} vs {:?}",
                sc.name,
                s.seq,
                s.meta.header,
                s.plan
            );
            assert!(
                s.matched,
                "{}: frame {} not matched by the manager",
                sc.name, s.seq
            );
        }
        assert_eq!(self.stats.plan_mismatch, 0, "{}", sc.name);

        // 1. the full decode.
        assert!(
            self.full.errors.is_empty(),
            "{}: full decode errors {:?}",
            sc.name,
            self.full.errors
        );
        for s in &self.sent {
            assert!(
                self.full.same_call.contains(&s.seq),
                "{}: frame {} was not output by the call that fed it",
                sc.name,
                s.seq
            );
        }
        assert_eq!(self.full.frames.len(), self.sent.len(), "{}", sc.name);

        // 2. drop test.
        let (mut compared, mut survivors, mut diverged) = (0, 0, 0);
        for s in self.sent.iter().filter(|s| !s.lost) {
            let got = self.dropped.frames.get(&s.seq);
            if s.poisoned {
                survivors += 1;
                if got != self.full.frames.get(&s.seq) {
                    diverged += 1;
                }
                continue;
            }
            let got = got.unwrap_or_else(|| {
                panic!(
                    "{}: clean frame {} missing from the drop decode",
                    sc.name, s.seq
                )
            });
            assert!(
                got == &self.full.frames[&s.seq],
                "{}: clean frame {} differs from the full decode (plan {:?})",
                sc.name,
                s.seq,
                s.plan
            );
            let src = source(sc.width, sc.height, s.seq as i32);
            let db = psnr(&src, got, sc.width, sc.height);
            assert!(
                db >= sc.psnr_floor,
                "{}: clean frame {} is {db:.1} dB from the source (floor {}): the encoder \
                 predicted from a picture the header does not name",
                sc.name,
                s.seq,
                sc.psnr_floor
            );
            compared += 1;
        }
        assert!(compared > 0, "{}: nothing was compared", sc.name);
        (compared, survivors, diverged)
    }

    /// Lowest PSNR against the source over the frames that depend on no loss.
    fn min_psnr(&self) -> f64 {
        self.sent
            .iter()
            .filter(|s| !s.poisoned)
            .map(|s| {
                let src = source(self.sc.width, self.sc.height, s.seq as i32);
                psnr(
                    &src,
                    &self.full.frames[&s.seq],
                    self.sc.width,
                    self.sc.height,
                )
            })
            .fold(f64::MAX, f64::min)
    }

    fn clean_tail(&self, n: u32) -> bool {
        let last = self.sc.frames;
        self.sent
            .iter()
            .filter(|s| s.seq + n >= last)
            .all(|s| !s.poisoned)
    }

    fn report(&self, compared: usize, survivors: usize, diverged: usize) {
        let bytes: usize = self.sent.iter().map(|s| s.au.len()).sum();
        eprintln!(
            "{}: {} frames {}x{}, {} B ({:.0} kbit/s), encode {:.2} ms/frame, compared {compared} \
             clean, {survivors} poisoned survivors ({diverged} diverged), min PSNR {:.1} dB, {:?}",
            self.sc.name,
            self.sent.len(),
            self.sc.width,
            self.sc.height,
            bytes,
            bytes as f64 * 8.0 / (f64::from(self.sc.frames) / 30.0) / 1000.0,
            self.mean_encode_ms(),
            self.min_psnr(),
            self.stats,
        );
    }
}

/// Run a scenario and apply the oracle; `None` when there is no encoder.
fn check(sc: Scenario) -> Option<Run> {
    let _gpu = crate::gpu_test_lock::lock();
    let run = run(&sc)?;
    let (compared, survivors, diverged) = run.verify();
    run.report(compared, survivors, diverged);
    if survivors > 0 {
        assert!(
            diverged > 0,
            "{}: {survivors} frames depended on a loss and none differs: the oracle is blind",
            sc.name
        );
    }
    Some(run)
}

// ------------------------------------------------------------------- tests

#[test]
fn h264_direct_healthy_chain() {
    let Some(r) = check(Scenario::new("healthy_chain", 640, 360, 60)) else {
        return;
    };
    let st = r.stats;
    assert_eq!(st.idr_session, 1);
    assert_eq!(st.recoveries, 0);
    assert!(st.p_healthy >= 55, "{st:?}");
    // The premise: anchors were marked and confirmed, so the MMCO and
    // long-term paths ran.
    assert!(
        st.candidates_marked >= 5 && st.anchors_confirmed >= 5,
        "{st:?}"
    );
    assert!(r.sent.iter().all(|s| !s.poisoned));
    assert!(r.min_psnr() > 30.0, "stream drifts: {}", r.min_psnr());
}

#[test]
fn h264_direct_loss_and_anchor_recovery() {
    let mut sc = Scenario::new("loss_recovery", 640, 360, 100);
    sc.lost = 40..41;
    let Some(r) = check(sc) else { return };
    let st = r.stats;
    assert!(st.recoveries >= 1 && st.recovered >= 1, "{st:?}");
    assert!(st.reroots >= 1, "{st:?}");
    assert!(r.sent.iter().filter(|s| s.poisoned && !s.lost).count() >= 2);
    assert!(r.clean_tail(20));
    assert_eq!(st.idr_session, 1, "no IDR for a loss: {st:?}");
}

#[test]
fn h264_direct_two_slot_swap_and_frame_num_wrap() {
    let mut sc = Scenario::new("two_slot_swap", 640, 360, 260);
    sc.lost = 150..151;
    let Some(r) = check(sc) else { return };
    let marks: BTreeSet<u8> = r
        .sent
        .iter()
        .filter(|s| s.plan.kind != PicKind::Idr)
        .filter_map(|s| s.plan.mark)
        .collect();
    assert_eq!(
        marks,
        BTreeSet::from([0, 1]),
        "both long-term slots are used"
    );
    assert!(r.stats.anchors_confirmed >= 15, "{:?}", r.stats);
    // Every mark carries MMCO 4 (plus1 = 2) before MMCO 6.
    for s in r
        .sent
        .iter()
        .filter(|s| s.plan.mark.is_some() && s.plan.kind != PicKind::Idr)
    {
        let ops = s.meta.header.as_ref().unwrap().mmco.as_ref().unwrap();
        assert_eq!(ops.first(), Some(&Mmco::MaxLong(2)));
        assert!(matches!(ops.last(), Some(Mmco::CurrentToLong(_))));
    }
    // frame_num wrapped (MaxFrameNum 128) while the stream ran.
    let fns: Vec<u32> = r.sent.iter().map(|s| s.plan.frame_num).collect();
    assert!(fns.contains(&127) && fns.windows(2).any(|w| w[1] < w[0]));
    assert!(r.clean_tail(20));
}

#[test]
fn h264_direct_long_freeze_across_the_wrap() {
    let mut sc = Scenario::new("long_freeze", 640, 360, 330);
    sc.psnr_floor = 28.0;
    sc.lost = 100..171;
    let Some(r) = check(sc) else { return };
    let st = r.stats;
    assert!(st.recoveries >= 1 && st.recovered >= 1, "{st:?}");
    assert!(
        st.p_recovery >= 10,
        "a 70-frame freeze is spent in recovery: {st:?}"
    );
    assert!(r.clean_tail(30));
    assert_eq!(st.idr_session, 1);
}

#[test]
fn h264_direct_anchor_kept_120_frames() {
    let mut sc = Scenario::new("anchor_120", 640, 360, 170);
    // No decoded bit after frame 12, so no later candidate is confirmed and
    // the anchor of the first frames stays the root; virtual time is slow
    // enough that no decode deadline fires meanwhile.
    sc.silent = 13..122;
    sc.lost = 122..123;
    sc.verdict_delay = 3;
    sc.step = Duration::from_millis(1);
    sc.srtt = Duration::from_millis(2);
    sc.ack_delay = 1;
    let Some(r) = check(sc) else { return };
    let age = r
        .sent
        .iter()
        .filter(|s| matches!(s.plan.kind, PicKind::Recovery | PicKind::Reroot))
        .filter_map(|s| s.plan.ref_frame_seq().map(|t| s.seq - t))
        .max()
        .expect("a recovery frame was planned");
    assert!(age >= 110, "recovered from an anchor only {age} frames old");
    assert!(r.clean_tail(20));
}

#[test]
fn h264_direct_1080p_is_cropped_and_clean() {
    let mut sc = Scenario::new("1080p", 1920, 1080, 20);
    sc.psnr_floor = 20.0;
    let Some(r) = check(sc) else {
        return;
    };
    assert_eq!(r.full.frames[&0].len(), 1920 * 1080 * 3 / 2);
}

#[test]
fn h264_direct_rate_change_needs_no_idr() {
    let mut sc = Scenario::new("rate_change", 640, 360, 90);
    sc.bps = 8_000_000;
    sc.psnr_floor = 22.0;
    sc.rate_change = Some((45, 1_500_000));
    let Some(r) = check(sc) else { return };
    assert_eq!(r.stats.idr_session, 1);
    let mean = |a: u32, b: u32| {
        let v: Vec<usize> = r
            .sent
            .iter()
            .filter(|s| s.seq >= a && s.seq < b)
            .map(|s| s.au.len())
            .collect();
        v.iter().sum::<usize>() as f64 / v.len() as f64
    };
    let (before, after) = (mean(10, 45), mean(55, 90));
    eprintln!("rate_change: mean P {before:.0} B before, {after:.0} B after");
    assert!(after < before * 0.7, "{before} -> {after}");
}

/// The negative control: with the list modification removed from the header
/// the stream still decodes without an error, but a frame after a mark
/// predicts from the wrong picture. The manager's replay must say so, and
/// the picture must be visibly wrong. If this passes with the fix in place,
/// the oracle is not testing what it claims.
#[test]
fn h264_direct_without_rplm_diverges() {
    let mut sc = Scenario::new("no_rplm_control", 640, 360, 40);
    sc.corrupt = Some(|n, plan| {
        if n >= 12 && plan.kind != PicKind::Idr {
            plan.rplm.clear();
        }
    });
    let _gpu = crate::gpu_test_lock::lock();
    let Some(run) = run(&sc) else { return };
    assert!(run.stats.plan_mismatch >= 1, "{:?}", run.stats);
    assert!(run.sent.iter().any(|s| !s.matched));
    // The first frame whose reference differs from the plan's decodes to
    // something far from the source.
    let first = run.sent.iter().find(|s| !s.matched).unwrap();
    let src = source(sc.width, sc.height, first.seq as i32);
    let bad = psnr(&src, &run.full.frames[&first.seq], sc.width, sc.height);
    let healthy = {
        let ok = run.sent.iter().find(|s| s.seq == 8).unwrap();
        psnr(
            &source(sc.width, sc.height, 8),
            &run.full.frames[&ok.seq],
            sc.width,
            sc.height,
        )
    };
    eprintln!(
        "no_rplm_control: frame {} {bad:.1} dB, healthy frame {healthy:.1} dB",
        first.seq
    );
    assert!(bad < healthy - 8.0, "{bad} vs {healthy}");
}

#[test]
fn h264_direct_refuses_a_plan_it_cannot_execute() {
    let sc = Scenario::new("bad_plan", 640, 360, 3);
    let _gpu = crate::gpu_test_lock::lock();
    let Some(mut b) = open(&sc) else { return };
    let mut mgr = RefManager::new(RefConfig {
        params: StreamParams::writer(),
        frame_interval: sc.step,
        initial_srtt: sc.srtt,
        observed: false,
    });
    let now = Instant::now();
    let src = source(640, 360, 0);
    let Plan::Frame(idr) = mgr.plan_next(0, now) else {
        panic!()
    };
    let au = b.encode(&view(&src, 640, 360), &idr, &[]).unwrap();
    mgr.frame_sent(&idr, au.meta.header.as_ref().unwrap(), 1, now);
    mgr.frag_acked(0, 0, now);
    let Plan::Frame(p) = mgr.plan_next(1, now) else {
        panic!()
    };
    // The manager's resident set is withheld: the target is unknown.
    let err = b.encode(&view(&src, 640, 360), &p, &[]).unwrap_err();
    assert!(matches!(err, BackendError::Plan(_)), "{err}");
    // A refused plan does not take the backend down.
    let refs = mgr.resident_refs();
    b.encode(&view(&src, 640, 360), &p, &refs).unwrap();
}

/// Widths that are not a multiple of 16 need the right-edge crop as well.
#[test]
fn h264_direct_1366x768_is_cropped_on_the_right() {
    let mut sc = Scenario::new("1366x768", 1366, 768, 20);
    sc.psnr_floor = 22.0;
    let Some(r) = check(sc) else { return };
    assert_eq!(r.full.frames[&0].len(), 1366 * 768 * 3 / 2);
}

/// CQP uses its QP everywhere: a low QP is big and sharp, a high one small
/// and soft.
#[test]
fn h264_direct_cqp_honours_its_qp() {
    let run_qp = |name, qp| {
        let mut sc = Scenario::new(name, 640, 360, 20);
        sc.rc = RcMode::Cqp { qp };
        sc.psnr_floor = 15.0;
        check(sc)
    };
    let (Some(lo), Some(hi)) = (run_qp("cqp_20", 20), run_qp("cqp_38", 38)) else {
        return;
    };
    let bytes = |r: &Run| r.sent.iter().map(|s| s.au.len()).sum::<usize>();
    eprintln!(
        "cqp: qp20 {} B {:.1} dB, qp38 {} B {:.1} dB",
        bytes(&lo),
        lo.min_psnr(),
        bytes(&hi),
        hi.min_psnr()
    );
    assert!(bytes(&lo) > bytes(&hi) * 2);
    assert!(lo.min_psnr() > hi.min_psnr() + 6.0);
}

/// A reference left out of the resident set is released by the backend; a
/// later plan that names it is an error, not a silent wrong reference (S2:
/// the driver evicts an unlisted reference and fails with
/// INVALID_PARAMETER).
#[test]
fn h264_direct_an_omitted_reference_is_an_error() {
    let sc = Scenario::new("omitted_ref", 640, 360, 4);
    let _gpu = crate::gpu_test_lock::lock();
    let Some(mut b) = open(&sc) else { return };
    let mut mgr = RefManager::new(RefConfig {
        params: StreamParams::writer(),
        frame_interval: sc.step,
        initial_srtt: sc.srtt,
        observed: false,
    });
    let now = Instant::now();
    let src = source(640, 360, 0);
    let mut plans = Vec::new();
    for n in 0..3 {
        let Plan::Frame(p) = mgr.plan_next(n, now) else {
            panic!()
        };
        let mut refs = mgr.resident_refs();
        // From the second P on, the IDR (long-term slot 0) is left out.
        if n == 2 {
            refs.retain(|r| r.seq != 0);
        }
        let au = b.encode(&view(&src, 640, 360), &p, &refs).unwrap();
        mgr.frame_sent(&p, au.meta.header.as_ref().unwrap(), 1, now);
        mgr.frag_acked(n, 0, now);
        plans.push(p);
    }
    // The manager still holds the IDR; a recovery plan names it.
    let mut p = plans[2].clone();
    p.target = plans[1].target;
    assert_eq!(p.target.map(|t| t.seq()), Some(0));
    let refs = mgr.resident_refs();
    assert!(refs.iter().any(|r| r.seq == 0));
    let err = b.encode(&view(&src, 640, 360), &p, &refs).unwrap_err();
    assert!(matches!(err, BackendError::Plan(_)), "{err}");
}

/// The probe runs once per process: the first open pays for it, the second
/// does not.
#[test]
fn h264_direct_probe_result_is_cached() {
    let sc = Scenario::new("probe_cache", 640, 360, 1);
    let _gpu = crate::gpu_test_lock::lock();
    super::vaapi_direct::clear_probe_cache();
    let time_open = || {
        let mut cfg = StreamConfig::new(sc.width, sc.height, 30, sc.bps);
        cfg.params = StreamParams::writer();
        let mut b = VaapiDirect::new();
        let t = Instant::now();
        let r = b.open(&cfg);
        #[allow(
            clippy::disallowed_methods,
            reason = "measures real driver time; no tokio clock here"
        )]
        let d = t.elapsed();
        (r, d)
    };
    let (first, cold) = time_open();
    match first {
        Ok(()) => {}
        Err(BackendError::Unavailable(why)) => return eprintln!("skipping probe_cache: {why}"),
        Err(e) => panic!("{e}"),
    }
    let (second, warm) = time_open();
    second.unwrap();
    eprintln!("probe_cache: first open {cold:?}, cached open {warm:?}");
    assert!(warm < cold);
}

// ------------------------------------------------------------ rate control
//
// B6 (design 4.3): the rate path against the real VCE. Local only, serial on
// the GPU lock like every T4 test. The content is `source` at 720p, full
// motion: about 1.7 Mbit/s is its QP 51 floor (S2), so every rate here is
// above that. Each test states its premises (the rate buffers really went
// out, the AUs decoded) before the effect, and the ones that measure print
// what they measured: a number the model in `pcm_rate.rs` is built from.

const RATE_W: u32 = 1280;
const RATE_H: u32 = 720;

/// Bytes of an average frame at `bps`, 30 fps.
fn avg_frame(bps: u32) -> f64 {
    f64::from(bps) / 8.0 / 30.0
}

fn mean(v: &[usize]) -> f64 {
    v.iter().sum::<usize>() as f64 / v.len().max(1) as f64
}

struct RateRun {
    sizes: Vec<usize>,
    idr: Vec<bool>,
    counters: super::vaapi_direct::RateCounters,
    /// Pictures the full stream decoded to, and decoder errors.
    decoded: usize,
    errors: usize,
}

impl RateRun {
    /// The frames the stream is judged on decoded, every one of them.
    fn assert_decoded(&self, name: &str) {
        assert_eq!(self.errors, 0, "{name}: decoder errors");
        assert_eq!(self.decoded, self.sizes.len(), "{name}: decoded != sent");
    }
}

/// A healthy chain (every frame acknowledged two frames later) coded under
/// `target(n)` for frame `n`, with an IDR at 0 and at each of `idr_at` (the
/// manager's resolution-change epoch). `None`: no encoder here.
fn run_rates(
    name: &'static str,
    frames: u32,
    open_bps: u32,
    target: &dyn Fn(u32) -> RateTarget,
    idr_at: &[u32],
    nudge: bool,
) -> Option<RateRun> {
    run_rates_with(
        name,
        frames,
        open_bps,
        target,
        idr_at,
        nudge,
        super::vaapi_direct::ResendVariant::Full,
    )
}

fn run_rates_with(
    name: &'static str,
    frames: u32,
    open_bps: u32,
    target: &dyn Fn(u32) -> RateTarget,
    idr_at: &[u32],
    nudge: bool,
    variant: super::vaapi_direct::ResendVariant,
) -> Option<RateRun> {
    let mut sc = Scenario::new(name, RATE_W, RATE_H, frames);
    sc.bps = open_bps;
    let mut backend = open(&sc)?;
    backend.set_nudge_for_test(nudge);
    backend.set_resend_variant_for_test(variant);
    let mut mgr = RefManager::new(RefConfig {
        params: StreamParams::writer(),
        frame_interval: sc.step,
        initial_srtt: sc.srtt,
        observed: false,
    });
    let t0 = Instant::now();
    let mut aus: Vec<(u32, Vec<u8>)> = Vec::new();
    let mut idr = Vec::new();
    for n in 0..frames {
        let now = t0 + sc.step * n;
        if n >= sc.ack_delay {
            mgr.frame_decoded(n - sc.ack_delay);
        }
        if idr_at.contains(&n) {
            mgr.resolution_change();
        }
        backend.set_rate(target(n));
        mgr.tick(now, sc.srtt);
        let Plan::Frame(plan) = mgr.plan_next(n, now) else {
            panic!("{name}: unexpected hold at frame {n}");
        };
        let refs = mgr.resident_refs();
        let src = source(RATE_W, RATE_H, n as i32);
        let au = backend
            .encode(&view(&src, RATE_W, RATE_H), &plan, &refs)
            .unwrap_or_else(|e| panic!("{name}: frame {n}: {e}"));
        let header: &SliceHeader = au.meta.header.as_ref().expect("direct AU is parsed");
        mgr.frame_sent(&plan, header, 1, now)
            .expect("plan is from the current epoch");
        mgr.frag_acked(n, 0, now);
        idr.push(au.meta.idr);
        aus.push((n, au.payload));
    }
    let all: Vec<(u32, &[u8])> = aus.iter().map(|(s, a)| (*s, a.as_slice())).collect();
    let full = decode(&all, true);
    Some(RateRun {
        sizes: aus.iter().map(|(_, a)| a.len()).collect(),
        idr,
        counters: backend.rate_counters(),
        decoded: full.frames.len(),
        errors: full.errors.len(),
    })
}

/// The IDR is held to its ceiling and the VBV it is sent under, and the raise
/// is real. `MaxFrameSize` is a ceiling, not a target (S2 b), and VBR does not
/// fill it: the IDR is limited by the content at its QP. Measured at 3 Mbit/s
/// on the RX 480 (2026-11-05): a budget of 93750 B gave IDRs of 55.9 and
/// 56.7 KB (0.60x), against 26.2 and 26.4 KB with no raise. So the asserts are
/// the ones that hold for a ceiling: no more than 1.15x the budget (the soft
/// cap's measured band), more than 1.2x the unraised IDR (the raised VBV
/// matters: 2.1x at 3 Mbit/s, 1.4x at 12, where the unraised IDR is already
/// near what the content costs). There is no floor tied to the budget, and no
/// comparison with the P frame cap: at 12 Mbit/s the IDR is content-limited
/// (141 KB) below the P cap (150 KB). The ratios are printed: they are what the PCM model's
/// `idr_fill` is set from.
#[test]
fn h264_direct_the_idr_cap_and_vbv_are_honoured() {
    let _gpu = crate::gpu_test_lock::lock();
    for bps in [3_000_000u32, 12_000_000] {
        let steady = RateTarget::steady(bps, 30);
        let name = "idr_cap";
        let Some(raised) = run_rates(name, 40, bps, &|_| steady, &[20], true) else {
            return;
        };
        let mut plain = steady;
        plain.idr_max_bytes = 0;
        let Some(unraised) = run_rates(name, 40, bps, &|_| plain, &[20], true) else {
            return;
        };
        raised.assert_decoded("raised");
        unraised.assert_decoded("unraised");
        // Premise: the buffers went out, the cap among them, and the stream
        // really had the two IDRs.
        assert!(raised.counters.cap_sends > 0, "{:?}", raised.counters);
        assert_eq!(raised.idr.iter().filter(|&&i| i).count(), 2);
        let idrs = |r: &RateRun| -> Vec<usize> {
            (0..r.sizes.len())
                .filter(|&i| r.idr[i])
                .map(|i| r.sizes[i])
                .collect()
        };
        let budget = steady.idr_max_bytes as f64;
        let (r, u) = (idrs(&raised), idrs(&unraised));
        eprintln!(
            "idr_cap {bps}: budget {budget:.0} B, IDRs {r:?} (x{:.2}, x{:.2}), no raise {u:?}, P cap {} B",
            r[0] as f64 / budget,
            r[1] as f64 / budget,
            steady.max_frame_bytes
        );
        for &size in &r {
            let ratio = size as f64 / budget;
            assert!(
                ratio <= 1.15,
                "IDR {size} B = {ratio:.2} x its budget, over the ceiling"
            );
        }
        for (a, b) in r.iter().zip(&u) {
            assert!(
                *a as f64 > 1.2 * *b as f64,
                "the raised VBV did not matter: {a} against {b}"
            );
        }
        // P frames, once the IDR's first few are past.
        let late: Vec<usize> = (0..40)
            .filter(|&i| !raised.idr[i] && i % 20 >= 4)
            .map(|i| raised.sizes[i])
            .collect();
        let worst = *late.iter().max().unwrap() as f64 / f64::from(steady.max_frame_bytes);
        eprintln!(
            "idr_cap {bps}: P mean {:.0} B ({:.2} x avg), worst P {worst:.2} x cap",
            mean(&late),
            mean(&late) / avg_frame(bps)
        );
        assert!(worst <= 3.0, "a P frame at {worst:.2} x the cap");
    }
}

/// The rate follows `set_rate` mid-stream with no IDR (S2 a: x0.25 and x4):
/// each settled segment averages the rate it was told, and the new rate is
/// there within four frames. The first frames after each change are printed.
#[test]
fn h264_direct_the_rate_tracks_set_rate_mid_stream_without_an_idr() {
    let _gpu = crate::gpu_test_lock::lock();
    let target = |n: u32| {
        RateTarget::steady(
            match n {
                0..45 => 12_000_000,
                45..90 => 3_000_000,
                _ => 12_000_000,
            },
            30,
        )
    };
    let Some(r) = run_rates("rate_tracks", 135, 12_000_000, &target, &[], true) else {
        return;
    };
    r.assert_decoded("rate_tracks");
    assert!(r.counters.rc_sends >= 3, "premise: {:?}", r.counters);
    assert_eq!(
        r.idr.iter().filter(|&&i| i).count(),
        1,
        "an IDR for a rate change"
    );
    for (change, bps) in [(45usize, 3_000_000u32), (90, 12_000_000)] {
        let first: Vec<String> = r.sizes[change..change + 6]
            .iter()
            .map(|&s| format!("{:.2}", s as f64 / avg_frame(bps)))
            .collect();
        eprintln!("rate_tracks: after the change to {bps}: x avg {first:?}");
        let settled = mean(&r.sizes[change + 4..change + 8]) / avg_frame(bps);
        assert!(
            (0.6..=1.5).contains(&settled),
            "frames 4-7 after the change: x{settled:.2}"
        );
    }
    for (a, b, bps) in [
        (10, 45, 12_000_000u32),
        (51, 90, 3_000_000),
        (96, 135, 12_000_000),
    ] {
        let ratio = mean(&r.sizes[a..b]) / avg_frame(bps);
        eprintln!("rate_tracks: {a}..{b} at {bps}: x{ratio:.2} of the average frame");
        assert!((0.7..=1.45).contains(&ratio), "{a}..{b}: x{ratio:.2}");
    }
}

/// The noisy rate sequence a policy applying every 250 ms produces (4 Hz,
/// each step a random 0.7-1.4x of the last, 3-10 Mbit/s), RC and HRD re-sent
/// every time at 75 % initial fullness. If each re-send reset the VCE's buffer
/// level, every step would start with a burst of up to three quarters of the
/// VBV; the stream must track with none: any one second at most 1.35 x what
/// the rates told allow, no P frame over twice its cap, and the first frame
/// after a change inside S2's step transient. The per-step ratios are printed:
/// they answer the question the PCM model's `resend_resets_level` leaves open.
#[test]
fn h264_direct_a_noisy_rate_sequence_at_4hz_tracks_without_a_burst() {
    let _gpu = crate::gpu_test_lock::lock();
    const STEP: u32 = 8;
    let mut rates = vec![6_000_000u32];
    let mut x = 0x2545_f491_4f6c_dd1du64;
    for _ in 0..40 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let f = 0.7 + (x % 1000) as f64 / 1000.0 * 0.7;
        let last = *rates.last().unwrap();
        rates.push(((f64::from(last) * f) as u32).clamp(3_000_000, 10_000_000));
    }
    let bps_at = |n: u32| rates[(n / STEP) as usize];
    let target = |n: u32| RateTarget::steady(bps_at(n), 30);
    let frames = 240;
    let Some(r) = run_rates("noisy_rate", frames, rates[0], &target, &[], true) else {
        return;
    };
    r.assert_decoded("noisy_rate");
    let steps = u64::from(frames / STEP);
    assert!(
        r.counters.rc_sends >= steps - 2,
        "premise: every step re-sent the buffers: {:?}",
        r.counters
    );
    assert_eq!(r.idr.iter().filter(|&&i| i).count(), 1);
    let mut worst_window = 0.0f64;
    for a in 5..(frames - 30) as usize {
        let got: usize = r.sizes[a..a + 30].iter().sum();
        let allowed: f64 = (a..a + 30).map(|n| avg_frame(bps_at(n as u32))).sum();
        worst_window = worst_window.max(got as f64 / allowed);
    }
    let firsts: Vec<f64> = (1..steps as u32)
        .filter(|s| bps_at(s * STEP) != bps_at(s * STEP - 1))
        .map(|s| r.sizes[(s * STEP) as usize] as f64 / avg_frame(bps_at(s * STEP)))
        .collect();
    let worst_p = (1..frames as usize)
        .map(|n| r.sizes[n] as f64 / f64::from(target(n as u32).max_frame_bytes))
        .fold(0.0, f64::max);
    eprintln!(
        "noisy_rate: worst 1 s window x{worst_window:.2} of the rates told, first frame after a \
         step x avg: mean {:.2} max {:.2} over {} steps, worst P {worst_p:.2} x cap",
        firsts.iter().sum::<f64>() / firsts.len() as f64,
        firsts.iter().copied().fold(0.0, f64::max),
        firsts.len()
    );
    assert!(firsts.len() > 15, "premise: the sequence stepped");
    assert!(
        worst_window <= 1.35,
        "a burst: x{worst_window:.2} in one second"
    );
    assert!(worst_p <= 2.0, "a P frame at {worst_p:.2} x its cap");
    assert!(
        firsts.iter().sum::<f64>() / (firsts.len() as f64) <= 1.8,
        "the first frame after a step is above S2's transient"
    );
}

/// A cap-only change latches because the re-sent rate buffers carry a
/// bits_per_second 16 bps away from the last (S2 b), and does not without
/// the nudge. At 6 Mbit/s the cap falls from three average frames to 60 % of
/// one at frame 30; the mean P afterwards must fall with the nudge and stay
/// with it off.
#[test]
fn h264_direct_the_16_bps_nudge_latches_a_cap_only_change() {
    let _gpu = crate::gpu_test_lock::lock();
    let bps = 6_000_000;
    let target = |n: u32| {
        let mut t = RateTarget::steady(bps, 30);
        if n >= 30 {
            t.max_frame_bytes = (avg_frame(bps) * 0.6) as u32;
        }
        t
    };
    let tail = |r: &RateRun| (mean(&r.sizes[5..30]), mean(&r.sizes[40..90]));
    let Some(with) = run_rates("nudge_on", 90, bps, &target, &[], true) else {
        return;
    };
    let Some(without) = run_rates("nudge_off", 90, bps, &target, &[], false) else {
        return;
    };
    with.assert_decoded("nudge_on");
    without.assert_decoded("nudge_off");
    assert!(with.counters.nudges > 0, "premise: {:?}", with.counters);
    assert_eq!(
        without.counters.nudges, 0,
        "premise: {:?}",
        without.counters
    );
    assert!(
        with.counters.cap_sends >= 2 && without.counters.cap_sends >= 2,
        "premise: the new cap was sent in both runs: {:?} {:?}",
        with.counters,
        without.counters
    );
    let (b_on, a_on) = tail(&with);
    let (b_off, a_off) = tail(&without);
    eprintln!(
        "nudge: mean P before/after the cap change: with {b_on:.0}/{a_on:.0} B, \
         without {b_off:.0}/{a_off:.0} B (average frame {:.0} B, new cap {:.0} B)",
        avg_frame(bps),
        avg_frame(bps) * 0.6
    );
    assert!(a_on < 0.85 * b_on, "the cap did not latch with the nudge");
    assert!(
        a_off > 0.9 * b_off,
        "the cap latched without the nudge: it is not what latches it"
    );
}

/// Probe-raise spike (B6-5's gate, a measurement). Can the VCE be made to
/// code a probe-sized P frame (S2: MaxFrameSize is a ceiling, a x4 rate step
/// gave 1.8x on the first frame and settled in 3-4)? At 2.5 Mbit/s, content
/// above the floor, three tries at frame 40, each printed per frame as bytes
/// and as a multiple of the average frame:
///   (a) MaxFrameSize raised to 8x, the VBV raised for one frame;
///   (b) the rate bumped to the window rate (8 Mbit/s) for three frames;
///   (c) the control: nothing changes.
/// Nothing is asserted but that every run codes and decodes.
#[test]
fn h264_direct_probe_raise_spike_prints_p_frame_sizes() {
    let _gpu = crate::gpu_test_lock::lock();
    let base_bps = 2_500_000u32;
    let base = RateTarget::steady(base_bps, 30);
    let window_bps = 8_000_000u32;
    let raised = |n: u32| {
        let mut t = base;
        if n == 40 {
            t.max_frame_bytes *= 8;
            t.vbv_bits *= 8;
        }
        t
    };
    let bumped = |n: u32| {
        if (40..43).contains(&n) {
            RateTarget::steady(window_bps, 30)
        } else {
            base
        }
    };
    let control = |_: u32| base;
    let tries: [(&str, &dyn Fn(u32) -> RateTarget); 3] = [
        ("a: cap x8 + VBV x8 for one frame", &raised),
        ("b: window rate for three frames", &bumped),
        ("c: control", &control),
    ];
    eprintln!(
        "probe_raise: average frame at {base_bps} bit/s {:.0} B, P cap {} B, window-rate frame {:.0} B",
        avg_frame(base_bps),
        base.max_frame_bytes,
        avg_frame(window_bps)
    );
    for (label, target) in tries {
        let Some(r) = run_rates("probe_raise", 60, base_bps, target, &[], true) else {
            return;
        };
        r.assert_decoded(label);
        let shown: Vec<String> = (36..52)
            .map(|n| {
                format!(
                    "{n}:{}B({:.1}x)",
                    r.sizes[n],
                    r.sizes[n] as f64 / avg_frame(base_bps)
                )
            })
            .collect();
        eprintln!("probe_raise {label}: {}", shown.join(" "));
        eprintln!(
            "probe_raise {label}: rc_sends {} cap_sends {} nudges {}; biggest frame in 40..46: {} B",
            r.counters.rc_sends,
            r.counters.cap_sends,
            r.counters.nudges,
            r.sizes[40..46].iter().max().unwrap()
        );
    }
}

/// Probe raise, lever B, re-measured on the backend as it is now (the HRD
/// restarts at 40 % fullness on a P re-send of the rate buffers; the spike
/// `h264_direct_probe_raise_spike_prints_p_frame_sizes` ran at 75 % and gave
/// 51.0 / 28.9 / 29.9 kB for a window-rate frame of 33.3 kB, 110 kB for a
/// 100 kB target). At 2.5 Mbit/s with content above the floor, eight times:
/// the rate goes to the window rate (8 Mbit/s, a 100 ms window is 100 kB) for
/// three frames, then back. Printed: the three frames and the three after the
/// window, as bytes and as multiples of the window-rate average frame, their
/// sum against the window's 100 kB, and the mean of that ratio, which is what
/// [`PcmRateModel::raise_yield`] stands for (the model's profile alone is
/// 1.00 / 0.56 / 0.87 of the average, a ratio of 0.81 of the target). Also
/// printed: the stream's own average frame in the settled stretch before each
/// window (the 10.4 kB of the spike) and the dip after the window.
///
/// Asserted: it codes and decodes, the buffers went out for every raise and
/// every restore, and a loose sanity band on the mean ratio (0.3 to 2.5).
/// Take the number to `PcmRateModel::with_raise_yield` and to scene 9.
///
/// Run: `GF_ALLOW_VAAPI=1 cargo test -p ghostframe-lib --lib
/// h264_direct_probe_raise_lever_b_at_40pct -- --test-threads=1 --nocapture`
/// on the GPU lock like the other T4 tests.
#[test]
fn h264_direct_probe_raise_lever_b_at_40pct() {
    let _gpu = crate::gpu_test_lock::lock();
    const BASE_BPS: u32 = 2_500_000;
    const WINDOW_BPS: u32 = 8_000_000;
    const RAISES: u32 = 8;
    const SPACING: u32 = 30;
    const FIRST: u32 = 40;
    const WINDOW_BYTES: f64 = WINDOW_BPS as f64 / 8.0 * 0.1;
    let base = RateTarget::steady(BASE_BPS, 30);
    let raised_at =
        |n: u32| n >= FIRST && (n - FIRST) % SPACING < 3 && (n - FIRST) / SPACING < RAISES;
    let target = |n: u32| {
        if raised_at(n) {
            RateTarget::steady(WINDOW_BPS, 30)
        } else {
            base
        }
    };
    let frames = FIRST + RAISES * SPACING;
    let Some(r) = run_rates("probe_raise_40", frames, BASE_BPS, &target, &[], true) else {
        return;
    };
    r.assert_decoded("probe_raise_40");
    // One send of the buffers into the window, one out of it, per raise.
    assert!(
        r.counters.rc_sends as u32 >= 2 * RAISES,
        "premise: every raise and every restore re-sent the buffers: {:?}",
        r.counters
    );
    let avg_w = avg_frame(WINDOW_BPS);
    let mut ratios = Vec::new();
    for k in 0..RAISES {
        let at = (FIRST + k * SPACING) as usize;
        let win = &r.sizes[at..at + 3];
        let after = &r.sizes[at + 3..at + 6];
        let before = mean(&r.sizes[at - 10..at]);
        let sum: usize = win.iter().sum();
        let ratio = sum as f64 / WINDOW_BYTES;
        ratios.push(ratio);
        eprintln!(
            "probe_raise_40 #{k}: window {} B ({:.2}x avg each) = {sum} B = {ratio:.2} of the {WINDOW_BYTES:.0} B asked; after {} B; stream avg before {before:.0} B",
            win.iter().map(|b| b.to_string()).collect::<Vec<_>>().join("/"),
            mean(win) / avg_w,
            after.iter().map(|b| b.to_string()).collect::<Vec<_>>().join("/"),
        );
    }
    let mean_ratio = ratios.iter().sum::<f64>() / ratios.len() as f64;
    eprintln!(
        "probe_raise_40: mean yield {mean_ratio:.2} (min {:.2}, max {:.2}); the model's profile alone is 0.81; first frame mean {:.2}x the window-rate average; counters {:?}",
        ratios.iter().copied().fold(f64::MAX, f64::min),
        ratios.iter().copied().fold(0.0, f64::max),
        (0..RAISES)
            .map(|k| r.sizes[(FIRST + k * SPACING) as usize] as f64 / avg_w)
            .sum::<f64>()
            / f64::from(RAISES),
        r.counters
    );
    assert!(
        (0.3..=2.5).contains(&mean_ratio),
        "yield {mean_ratio:.2} outside the sanity band"
    );
}

/// How the first frame after a rate step depends on the size of the step, and
/// whether a re-send alone does it (B6's open question: it decides whether the
/// in-place rate policy can move the rate in small steps). At 720p, 30 fps,
/// around 6 Mbit/s: steps of +-5, 10, 20 and 30 % (the relative change of the
/// rate), four of each, each held 15 frames and then undone (a return step is
/// a step too and is tabled apart), and a pure re-send: the same rate with the
/// cap one byte different, so the RC and HRD buffers go out again with only
/// the 16 bps nudge changed.
///
/// Printed per transition: the 4 frames after it as multiples of the new
/// average frame, then per step size the mean of each of those four frames and
/// the settled mean (frames 8-14 of the segment). If the first frame is the
/// same at 5 % as at 30 %, and the re-send shows it too, the transient is per
/// change, not per size. Asserted: every run codes and decodes, the buffers
/// went out for the steps, and a loose sanity bound (a first frame below 4x
/// and a settled segment within 0.5-1.6x of the average).
///
/// Run: `GF_ALLOW_VAAPI=1 cargo test -p ghostframe-lib --lib
/// h264_direct_small_rate_steps_first_frame_ratio -- --test-threads=1
/// --nocapture`, on the GPU lock like the other T4 tests.
#[test]
fn h264_direct_small_rate_steps_first_frame_ratio() {
    let _gpu = crate::gpu_test_lock::lock();
    const BASE: u32 = 6_000_000;
    const SEG: u32 = 15;
    const REPS: usize = 4;
    const PCTS: [i32; 8] = [-30, -20, -10, -5, 5, 10, 20, 30];
    // (rate told, kind) per segment: 0 is the warm-up, then out and back.
    let mut segs: Vec<(u32, String)> = vec![(BASE, "warm".to_string()); 2];
    for rep in 0..REPS {
        // Rotate the sizes so none keeps a position in the stream.
        for k in 0..PCTS.len() {
            let pct = PCTS[(k + rep * 3) % PCTS.len()];
            let bps = (f64::from(BASE) * (1.0 + f64::from(pct) / 100.0)) as u32;
            segs.push((bps, format!("out {pct:+}%")));
            segs.push((BASE, format!("back {pct:+}%")));
        }
    }
    let frames = segs.len() as u32 * SEG;
    let target = |n: u32| RateTarget::steady(segs[(n / SEG) as usize].0, 30);
    let Some(r) = run_rates("small_steps", frames, BASE, &target, &[], true) else {
        return;
    };
    r.assert_decoded("small_steps");
    assert!(
        r.counters.rc_sends as usize >= segs.len() - 4,
        "premise: every step re-sent the buffers: {:?}",
        r.counters
    );
    let mut groups: BTreeMap<String, Vec<[f64; 5]>> = BTreeMap::new();
    for (i, (bps, kind)) in segs.iter().enumerate().skip(2) {
        let at = i * SEG as usize;
        let avg = avg_frame(*bps);
        let row = |k: usize| r.sizes[at + k] as f64 / avg;
        let settled = mean(&r.sizes[at + 8..at + SEG as usize]) / avg;
        let f = [row(0), row(1), row(2), row(3), settled];
        eprintln!(
            "small_steps {kind} to {bps}: x avg {:.2} {:.2} {:.2} {:.2}, settled {settled:.2}",
            f[0], f[1], f[2], f[3]
        );
        assert!(f[0] < 4.0, "{kind}: first frame x{:.2}", f[0]);
        assert!(
            (0.5..=1.6).contains(&settled),
            "{kind}: settled x{settled:.2}"
        );
        groups.entry(kind.clone()).or_default().push(f);
    }
    for (kind, v) in &groups {
        let m = |k: usize| v.iter().map(|f| f[k]).sum::<f64>() / v.len() as f64;
        eprintln!(
            "small_steps MEAN {kind:<10} n={}: x avg {:.2} {:.2} {:.2} {:.2}, settled {:.2}",
            v.len(),
            m(0),
            m(1),
            m(2),
            m(3),
            m(4)
        );
    }

    // A pure re-send: the same rate, the cap one byte apart, every 15 frames.
    let resend_segs = 10u32;
    let cap_target = |n: u32| {
        let mut t = RateTarget::steady(BASE, 30);
        t.max_frame_bytes += (n / SEG) % 2;
        t
    };
    let Some(rs) = run_rates(
        "small_resend",
        resend_segs * SEG,
        BASE,
        &cap_target,
        &[],
        true,
    ) else {
        return;
    };
    rs.assert_decoded("small_resend");
    assert!(
        rs.counters.nudges > 0,
        "premise: the re-send carried the nudge: {:?}",
        rs.counters
    );
    let avg = avg_frame(BASE);
    let mut firsts = Vec::new();
    for s in 2..resend_segs as usize {
        let at = s * SEG as usize;
        let f: Vec<f64> = (0..4).map(|k| rs.sizes[at + k] as f64 / avg).collect();
        let settled = mean(&rs.sizes[at + 8..at + SEG as usize]) / avg;
        eprintln!(
            "small_steps RESEND {s}: x avg {:.2} {:.2} {:.2} {:.2}, settled {settled:.2}",
            f[0], f[1], f[2], f[3]
        );
        assert!(f[0] < 4.0, "re-send: first frame x{:.2}", f[0]);
        firsts.push(f[0]);
    }
    eprintln!(
        "small_steps RESEND MEAN first frame x{:.2} over {}",
        firsts.iter().sum::<f64>() / firsts.len() as f64,
        firsts.len()
    );
}

/// What a re-send of the rate buffers must carry (B6). T4 measured that every
/// re-send, a rate step of any size or the same rate with the 16 bps nudge, costs
/// a first frame of 1.4-1.9x the average. The hypothesis: the burst comes from
/// the HRD re-send resetting the VCE's buffer level to its initial fullness
/// (75 %), not from the RC change. At 6 Mbit/s, 720p, 30 fps, each variant
/// runs two streams, 15 frames to a segment:
///   - pure: 10 segments, the same rate, the cap one byte apart (so only the
///     16 bps nudge changes);
///   - steps: 16 segments of 6.0, 6.6, 6.0, 5.4 Mbit/s repeated (+-10 %).
///
/// Variants: (i) RC + HRD + MaxFrameSize as production sends them, (ii) RC +
/// MaxFrameSize, no HRD, (iii) HRD at 100 % initial fullness, (iv) HRD at 25 %,
/// (v) RC only (the cap and VBV stay as last sent; a step's cap goes stale).
/// An IDR always carries everything. Per variant it prints the first frame
/// after each re-send as a multiple of the new average frame, the mean and max,
/// the mean of frame 2 and the settled mean (frames 8-14). Asserted: every run
/// codes and decodes and no first frame is above 4x.
///
/// Run: `GF_ALLOW_VAAPI=1 cargo test -p ghostframe-lib --lib
/// h264_direct_resend_variants_first_frame -- --test-threads=1 --nocapture`
/// on the GPU lock like the other T4 tests.
#[test]
fn h264_direct_resend_variants_first_frame() {
    use super::vaapi_direct::ResendVariant as V;
    let _gpu = crate::gpu_test_lock::lock();
    const BASE: u32 = 6_000_000;
    const SEG: u32 = 15;
    let variants = [
        ("i  RC+HRD75+Max (today)", V::Full),
        ("ii RC+Max, no HRD", V::NoHrd),
        ("iii RC+HRD100+Max", V::HrdFull),
        ("iv RC+HRD25+Max", V::HrdLow),
        ("v  RC only", V::RcOnly),
    ];
    let step_rates = [6.0, 6.6, 6.0, 5.4];
    let step_segs = 16u32;
    let step_bps = |seg: u32| (f64::from(BASE) * step_rates[(seg % 4) as usize] / 6.0) as u32;
    let pure = |n: u32| {
        let mut t = RateTarget::steady(BASE, 30);
        t.max_frame_bytes += (n / SEG) % 2;
        t
    };
    let steps = |n: u32| RateTarget::steady(step_bps(n / SEG), 30);
    for (label, v) in variants {
        for (kind, segs) in [("pure", 10u32), ("steps", step_segs)] {
            let target: &dyn Fn(u32) -> RateTarget = if kind == "pure" { &pure } else { &steps };
            let Some(r) = run_rates_with("resend_variants", segs * SEG, BASE, target, &[], true, v)
            else {
                return;
            };
            r.assert_decoded(label);
            assert!(
                r.counters.rc_sends as u32 >= segs - 3,
                "premise: every segment re-sent: {:?}",
                r.counters
            );
            // (first frame, direction) per re-send, from segment 2 on.
            let (mut up, mut down, mut same) = (Vec::new(), Vec::new(), Vec::new());
            let (mut second, mut settled) = (Vec::new(), Vec::new());
            for s in 2..segs {
                let at = (s * SEG) as usize;
                let bps = target(s * SEG).bps;
                let avg = avg_frame(bps);
                let first = r.sizes[at] as f64 / avg;
                assert!(first < 4.0, "{label} {kind}: first frame x{first:.2}");
                match bps.cmp(&target(s * SEG - 1).bps) {
                    std::cmp::Ordering::Greater => up.push(first),
                    std::cmp::Ordering::Less => down.push(first),
                    std::cmp::Ordering::Equal => same.push(first),
                }
                second.push(r.sizes[at + 1] as f64 / avg);
                settled.push(mean(&r.sizes[at + 8..at + SEG as usize]) / avg);
            }
            let m = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
            let fmt = |v: &[f64]| {
                format!(
                    "n={} mean {:.2} max {:.2} [{}]",
                    v.len(),
                    m(v),
                    v.iter().copied().fold(0.0, f64::max),
                    v.iter()
                        .map(|x| format!("{x:.2}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
            };
            eprintln!(
                "resend_variants {label} / {kind}: up {} | down {} | same-rate {} | frame 2 mean {:.2} | settled mean {:.2}",
                fmt(&up),
                fmt(&down),
                fmt(&same),
                m(&second),
                m(&settled)
            );
        }
    }
}

/// B6: which initial HRD fullness makes a re-send of RC+HRD+Max neutral. Every
/// fullness is re-sent as a pure re-send (same rate) and as +-10 % steps, about
/// eight of each at 720p, 30 fps around 6 Mbit/s. Prints frames 1, 2 and 3
/// after the re-send as multiples of the average frame (n, mean, min, max) and
/// the settled mean. Asserted: the runs decode, and no mean is above 4x.
///
/// Run: `GF_ALLOW_VAAPI=1 cargo test -p ghostframe-lib --lib
/// h264_direct_resend_fullness_sweep -- --test-threads=1 --nocapture` on the
/// GPU lock like the other T4 tests.
#[test]
fn h264_direct_resend_fullness_sweep() {
    use super::vaapi_direct::ResendVariant as V;
    let _gpu = crate::gpu_test_lock::lock();
    const BASE: u32 = 6_000_000;
    const SEG: u32 = 15;
    let step_rates = [6.0, 6.6, 6.0, 5.4];
    let step_bps = |seg: u32| (f64::from(BASE) * step_rates[(seg % 4) as usize] / 6.0) as u32;
    let pure = |n: u32| {
        let mut t = RateTarget::steady(BASE, 30);
        t.max_frame_bytes += (n / SEG) % 2;
        t
    };
    let steps = |n: u32| RateTarget::steady(step_bps(n / SEG), 30);
    for pct in [30u32, 40, 50, 60] {
        for (kind, segs) in [("pure", 10u32), ("steps", 10u32)] {
            let target: &dyn Fn(u32) -> RateTarget = if kind == "pure" { &pure } else { &steps };
            let Some(r) = run_rates_with(
                "resend_sweep",
                segs * SEG,
                BASE,
                target,
                &[],
                true,
                V::HrdPct(pct),
            ) else {
                return;
            };
            r.assert_decoded("resend_sweep");
            assert!(
                r.counters.rc_sends as u32 >= segs - 3,
                "premise: every segment re-sent: {:?}",
                r.counters
            );
            let mut after: [Vec<f64>; 3] = Default::default();
            let mut settled = Vec::new();
            for s in 2..segs {
                let at = (s * SEG) as usize;
                let avg = avg_frame(target(s * SEG).bps);
                for (k, v) in after.iter_mut().enumerate() {
                    v.push(r.sizes[at + k] as f64 / avg);
                }
                settled.push(mean(&r.sizes[at + 8..at + SEG as usize]) / avg);
            }
            let m = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
            let fmt = |v: &[f64]| {
                format!(
                    "n={} mean {:.2} min {:.2} max {:.2}",
                    v.len(),
                    m(v),
                    v.iter().copied().fold(f64::MAX, f64::min),
                    v.iter().copied().fold(0.0, f64::max)
                )
            };
            eprintln!(
                "resend_sweep HRD{pct} / {kind}: frame 1 {} | frame 2 {} | frame 3 {} | settled mean {:.2}",
                fmt(&after[0]),
                fmt(&after[1]),
                fmt(&after[2]),
                m(&settled)
            );
            assert!(
                m(&after[0]) < 4.0,
                "HRD{pct} {kind}: frame 1 mean x{:.2}",
                m(&after[0])
            );
        }
    }
}
