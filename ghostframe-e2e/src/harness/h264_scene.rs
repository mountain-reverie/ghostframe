//! Helpers for the H.264 browserless scenes (design T3): the client side that
//! decodes what the real `ClientCore` hands out, the analysis that compares
//! it with what the encoder meant, loss patterns and a capacity step.
//!
//! The server side is `ghostframe_lib::encoder::h264_harness`: a PCM-patch
//! source (Phase A's policy, or the `RefManager`) behind the bridge's real
//! send path, and a log of the picture each frame was meant to show. Every
//! picture carries markers (`Picture`), so a decoded one says which frames it
//! was built from.
//!
//! The client side here is the real `ClientCore` (reference gate, frame acks,
//! reset requests) over a software decoder: every access unit the gate feeds is
//! decoded with libavcodec (lenient, so wrong pictures show instead of
//! failing), and what the decoder did goes back through
//! `ClientNet::h264_result`, exactly as the native renderer's does. The frame
//! ack the server hears is the client's own 0x0A datagram, not a model of it.
//! "A wrong picture displayed" is how drift shows.

use ghostframe_client_core::{Event as CoreEvent, H264Outcome as CoreOutcome};
use ghostframe_client_net::ClientNetEvent;
use ghostframe_lib::encoder::h264_harness::{
    frame_bytes, Decoded, FeedError, PcmDecoder, PcmLogInner, PcmRefCounters, Picture, Strictness,
};
use ghostframe_protocol::protocol::REF_FRAME_NONE;

use crate::harness::browserless::{BrowserlessResult, H264Load};
use crate::netsim::NetProfile;

/// What came out of the decoder for one delivered access unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayOutcome {
    /// A picture came out. `picture` is `None` when it is not made of markers
    /// (a macroblock disagrees with itself).
    Shown {
        picture: Option<Picture>,
        damaged: bool,
    },
    /// The decoder refused the access unit.
    Rejected,
    /// The decoder accepted it and produced no picture.
    NoPicture,
}

/// One delivered access unit and what the client's decoder made of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayRecord {
    pub at_us: u64,
    pub frame_seq: u32,
    pub is_keyframe: bool,
    pub ref_frame_seq: u32,
    pub outcome: DisplayOutcome,
}

/// The client's decoder, fed from the events `ClientNet` returns.
pub struct H264Display {
    decoder: PcmDecoder,
    pub records: Vec<DisplayRecord>,
    /// Decode errors reported to the core (it sends the reset request).
    pub resets: u32,
}

impl Default for H264Display {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Display {
    pub fn new() -> Self {
        Self {
            decoder: PcmDecoder::new(Strictness::Lenient),
            records: Vec::new(),
            resets: 0,
        }
    }

    /// Decode every H.264 access unit in `events`, in order, and say what the
    /// decoder did with each, as the native renderer does: a refused access
    /// unit or one libavcodec flags damaged is a decode error (libavcodec
    /// reports damage; VA-API would not, which is the gate's job), no picture
    /// is `NotOutput`, anything else `Decoded`. The decoder is reset where the
    /// gate says so (`reset_decoder`).
    pub fn on_events(&mut self, at_us: u64, events: &[ClientNetEvent]) -> Vec<(u32, CoreOutcome)> {
        let mut results = Vec::new();
        for ev in events {
            let ClientNetEvent::Core(CoreEvent::NeedsH264 {
                frame_seq,
                is_keyframe,
                ref_frame_seq,
                reset_decoder,
                payload,
                ..
            }) = ev
            else {
                continue;
            };
            if *reset_decoder {
                self.decoder.reset();
            }
            let outcome = match self.decoder.feed(payload, i64::from(*frame_seq)) {
                Ok(Decoded { yuv, damaged, .. }) => DisplayOutcome::Shown {
                    picture: (yuv.len() == frame_bytes())
                        .then(|| Picture::read(&yuv))
                        .flatten(),
                    damaged,
                },
                Err(FeedError::Rejected(_)) => DisplayOutcome::Rejected,
                Err(FeedError::NoPicture(_)) => DisplayOutcome::NoPicture,
            };
            results.push((
                *frame_seq,
                match &outcome {
                    DisplayOutcome::Shown { damaged: false, .. } => CoreOutcome::Decoded,
                    DisplayOutcome::Shown { damaged: true, .. } | DisplayOutcome::Rejected => {
                        self.resets += 1;
                        CoreOutcome::DecodeError
                    }
                    DisplayOutcome::NoPicture => CoreOutcome::NotOutput,
                },
            ));
            self.records.push(DisplayRecord {
                at_us,
                frame_seq: *frame_seq,
                is_keyframe: *is_keyframe,
                ref_frame_seq: *ref_frame_seq,
                outcome,
            });
        }
        results
    }
}

/// What a scene read back from the PCM source and the client.
#[derive(Debug, Clone, Default)]
pub struct H264Run {
    pub truth: PcmLogInner,
    pub display: Vec<DisplayRecord>,
    /// Decode errors the client core turned into reset requests.
    pub decoder_resets: u32,
    /// The scene's length, for the tail of a freeze.
    pub duration_us: u64,
    /// The scene's frame period.
    pub cadence_us: u64,
    /// `[from, to)` in virtual time during which the tile codec carried the
    /// screen: no H.264 picture is expected, so it is not a freeze.
    pub tile_windows: Vec<(u64, u64)>,
}

/// A frame the client displayed that was not the picture the encoder meant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrongPicture {
    pub at_us: u64,
    pub frame_seq: u32,
    /// The frame the decoder really predicted from, found by matching the
    /// shown picture against every earlier meant picture; `None` when no
    /// single reference explains it (or the picture is not made of markers).
    pub effective_ref: Option<u32>,
    /// The frame the encoder meant it to predict from.
    pub meant_ref: Option<u32>,
}

/// The properties a T3 scene asserts, computed once.
#[derive(Debug, Clone, Default)]
pub struct H264Outcome {
    /// Frames the source made, and how many were IDRs.
    pub made: usize,
    pub idr_made: usize,
    /// Access units the client decoded.
    pub fed: usize,
    /// Shown, markers intact, equal to the meant picture.
    pub correct: usize,
    pub wrong: Vec<WrongPicture>,
    pub rejected: usize,
    pub no_picture: usize,
    pub damaged: usize,
    /// Made but never delivered to the decoder: lost, superseded, abandoned.
    pub never_fed: usize,
    /// Delivered frames the client had no meant picture for (a bug if any).
    pub unknown_to_truth: usize,
    /// Gaps between correct pictures, longest first, once the stream began.
    pub max_gap_us: u64,
    /// Sum over the gaps longer than `FREEZE_GAP_FRAMES` periods of the time
    /// beyond that allowance.
    pub frozen_us: u64,
    /// Number of such gaps (freeze episodes).
    pub freezes: usize,
    pub first_correct_us: Option<u64>,
    pub last_correct_us: Option<u64>,
    /// Access units fed with a `frame_seq` below one already fed (the core
    /// delivered a frame late).
    pub fed_out_of_order: usize,
    pub decoder_resets: u32,
    /// Delivered frames whose header named another reference than the one the
    /// encoder meant (`ref_frame_seq` against the truth log).
    pub header_ref_mismatches: usize,
    /// Frames the encoder made predicting from a frame the client never
    /// displayed correctly. A source whose picture of the client is true
    /// stops doing that within a round trip of a loss; one that has lost the
    /// loss keeps doing it to the end of the scene.
    pub refs_to_undisplayed: usize,
    pub refs: PcmRefCounters,
    /// The longest run of consecutive frames (by `frame_seq` among those the
    /// source made) that the client never decoded.
    pub longest_lost_run: usize,
    /// For each tile window: how long after it ended the first correct
    /// H.264 picture was shown (`None`: never).
    pub resume_us: Vec<Option<u64>>,
}

/// A gap between correct pictures this many frame periods long is a freeze:
/// the hold after an IDR (6 skipped frames on this link, a 98 ms gap) and a
/// skipped frame or two are not.
pub const FREEZE_GAP_FRAMES: u64 = 4;

impl H264Run {
    pub fn analyse(&self) -> H264Outcome {
        let truth = &self.truth.truth;
        let mut o = H264Outcome {
            made: truth.len(),
            idr_made: truth.values().filter(|t| t.idr).count(),
            fed: self.display.len(),
            decoder_resets: self.decoder_resets,
            refs: self.truth.refs,
            ..Default::default()
        };
        let mut fed_seqs = std::collections::BTreeSet::new();
        let mut good_at: Vec<u64> = Vec::new();
        for rec in &self.display {
            fed_seqs.insert(rec.frame_seq);
            let Some(meant) = truth.get(&rec.frame_seq) else {
                o.unknown_to_truth += 1;
                continue;
            };
            if rec.ref_frame_seq != meant.ref_seq.unwrap_or(REF_FRAME_NONE) {
                o.header_ref_mismatches += 1;
            }
            match &rec.outcome {
                DisplayOutcome::Rejected => o.rejected += 1,
                DisplayOutcome::NoPicture => o.no_picture += 1,
                DisplayOutcome::Shown { picture, damaged } => {
                    if *damaged {
                        o.damaged += 1;
                    }
                    if picture.as_ref() == Some(&meant.picture) {
                        o.correct += 1;
                        good_at.push(rec.at_us);
                    } else {
                        let effective_ref = picture.as_ref().and_then(|p| {
                            let tag = u16::try_from(rec.frame_seq).ok()?;
                            truth
                                .range(..rec.frame_seq)
                                .rev()
                                .find(|(_, t)| Picture::predicted(&t.picture, tag) == *p)
                                .map(|(s, _)| *s)
                        });
                        o.wrong.push(WrongPicture {
                            at_us: rec.at_us,
                            frame_seq: rec.frame_seq,
                            effective_ref,
                            meant_ref: meant.ref_seq,
                        });
                    }
                }
            }
        }
        o.resume_us = self
            .tile_windows
            .iter()
            .map(|&(_, end)| good_at.iter().find(|&&t| t >= end).map(|t| t - end))
            .collect();
        let good: std::collections::BTreeSet<u32> = self
            .display
            .iter()
            .filter(|r| {
                matches!(&r.outcome, DisplayOutcome::Shown { picture, damaged: false }
                    if truth.get(&r.frame_seq).is_some_and(|t| picture.as_ref() == Some(&t.picture)))
            })
            .map(|r| r.frame_seq)
            .collect();
        o.refs_to_undisplayed = truth
            .values()
            .filter(|t| t.ref_seq.is_some_and(|r| !good.contains(&r)))
            .count();
        o.never_fed = truth.keys().filter(|s| !fed_seqs.contains(s)).count();
        let mut run = 0usize;
        for s in truth.keys() {
            if fed_seqs.contains(s) {
                run = 0;
            } else {
                run += 1;
                o.longest_lost_run = o.longest_lost_run.max(run);
            }
        }
        o.first_correct_us = good_at.first().copied();
        o.last_correct_us = good_at.last().copied();
        let mut newest = 0u32;
        for rec in &self.display {
            if rec.frame_seq < newest {
                o.fed_out_of_order += 1;
            }
            newest = newest.max(rec.frame_seq);
        }
        // Gaps between correct pictures, and the tail to the end of the scene.
        let allow = FREEZE_GAP_FRAMES * self.cadence_us;
        let mut edges = good_at.clone();
        if !edges.is_empty() {
            edges.push(self.duration_us.max(*edges.last().unwrap()));
        }
        for w in edges.windows(2) {
            // Time the tile codec carried the screen is not a freeze.
            let exempt: u64 = self
                .tile_windows
                .iter()
                .map(|&(a, b)| b.min(w[1]).saturating_sub(a.max(w[0])))
                .sum();
            let gap = (w[1] - w[0]).saturating_sub(exempt);
            o.max_gap_us = o.max_gap_us.max(gap);
            if gap > allow {
                o.frozen_us += gap - allow;
                o.freezes += 1;
            }
        }
        o
    }
}

impl BrowserlessResult {
    /// The PCM scene's truth log and client records, if the scene had a PCM
    /// source.
    pub fn h264_run(&self) -> Option<&H264Run> {
        self.h264_pcm.as_ref()
    }
}

// ---------------------------------------------------------------- loss patterns

/// A way for the path to misbehave. Applied to a base profile (the delay and
/// capacity the scene wants), per direction, because the server -> client path
/// carries the frames and the client -> server path carries their
/// acknowledgements, and the two fail differently.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Pattern {
    /// A clean path.
    Clean,
    /// Independent loss of each frame datagram.
    Iid(f64),
    /// Gilbert-Elliott bursts on the frame path: enter and exit probabilities
    /// per datagram, loss while in the bad state.
    Burst { enter: f64, exit: f64, loss: f64 },
    /// Loss on the acknowledgement path only (client -> server): the 0x06
    /// batches and the 0x0A frame acks alike.
    AckLoss(f64),
    /// Datagrams reordered within `window_us` on the frame path.
    Reorder { window_us: u64 },
}

impl Pattern {
    /// The per-direction profiles: `(scene net, overrides)`. The scene's own
    /// `net` stays the clean base so the direction a pattern leaves alone is
    /// untouched.
    pub fn apply(
        self,
        base: NetProfile,
    ) -> (NetProfile, crate::harness::browserless::LinkOverrides) {
        let mut s2c = base.clone();
        let mut c2s = base.clone();
        match self {
            Pattern::Clean => {}
            Pattern::Iid(p) => s2c.loss = p,
            Pattern::Burst { enter, exit, loss } => {
                s2c.burst_enter = enter;
                s2c.burst_exit = exit;
                s2c.burst_loss = loss;
            }
            Pattern::AckLoss(p) => c2s.loss = p,
            Pattern::Reorder { window_us } => s2c.reorder_us = window_us,
        }
        let links = crate::harness::browserless::LinkOverrides {
            c2s: Some(c2s),
            s2c: Some(s2c),
            ..Default::default()
        };
        (base, links)
    }
}

/// A PCM scene's stream: 30 fps, 128x128 PCM pictures padded to the sizes a
/// real stream would have (an IDR of ~40 kB is 35 datagrams, a P frame of
/// ~3 kB is three).
pub fn pcm_load(mode: ghostframe_lib::encoder::h264_harness::PcmMode) -> H264Load {
    H264Load {
        cadence_us: crate::harness::browserless::DEFAULT_CADENCE_US,
        idr_bytes: 40_000,
        p_bytes: 3_000,
        gop_frames: 0,
        width: 1280,
        height: 720,
        quiet: None,
        cbr: None,
        pcm: Some(mode),
        pcm_wrong_ref_every: 0,
        pcm_rate_policy: false,
        pcm_rate_model: None,
        pcm_phase_a_rate: false,
        probe_raise: true,
        tile_phases: [None, None],
    }
}

impl H264Outcome {
    /// The longest a stretch without a correct picture may last when the
    /// stream recovers from its anchor (design T3 scene 1): each recovery
    /// attempt costs three round trips and the loss path's verdict latency,
    /// and an attempt can itself be lost, once per frame of the longest run
    /// of lost frames; plus the allowance `FREEZE_GAP_FRAMES` already grants.
    pub fn freeze_bound_us(&self, rtt_us: u64, verdict_us: u64, cadence_us: u64) -> u64 {
        (1 + self.longest_lost_run as u64) * (3 * rtt_us + verdict_us)
            + FREEZE_GAP_FRAMES * cadence_us
    }

    /// One line with every number a scene reports, for the scene log.
    pub fn summary(&self) -> String {
        format!(
            "made={} idr_made={} fed={} correct={} WRONG={} rejected={} no_picture={} damaged={} \
             never_fed={} late={} resets={} max_gap_ms={} frozen_ms={} freezes={} first_ok_ms={:?} \
             last_ok_ms={:?}",
            self.made,
            self.idr_made,
            self.fed,
            self.correct,
            self.wrong.len(),
            self.rejected,
            self.no_picture,
            self.damaged,
            self.never_fed,
            self.fed_out_of_order,
            self.decoder_resets,
            self.max_gap_us / 1000,
            self.frozen_us / 1000,
            self.freezes,
            self.first_correct_us.map(|t| t / 1000),
            self.last_correct_us.map(|t| t / 1000),
        )
    }

    /// The frames shown wrongly, `frame (effective ref, meant ref)`, up to
    /// `n` of them, for an assertion message.
    pub fn wrong_list(&self, n: usize) -> String {
        self.wrong
            .iter()
            .take(n)
            .map(|w| {
                format!(
                    "{} ({:?} not {:?})",
                    w.frame_seq, w.effective_ref, w.meant_ref
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A bandwidth step: `from_bytes_per_s` until `at_us`, then `to_bytes_per_s`,
/// behind a bottleneck buffer of `depth_ms`, with `delay_us` one way.
pub fn capacity_step_net(
    from_bytes_per_s: u64,
    at_us: u64,
    to_bytes_per_s: u64,
    delay_us: u64,
    depth_ms: u64,
) -> NetProfile {
    NetProfile {
        delay_us,
        cap: crate::netsim::CapTimeline::step(from_bytes_per_s, at_us, to_bytes_per_s),
        bottleneck: Some(crate::netsim::Bottleneck {
            depth_ms,
            aqm: None,
        }),
        ..NetProfile::perfect()
    }
}
