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
