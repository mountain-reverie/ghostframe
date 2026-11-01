//! The probe cache of H.264 fragments: what the emitter keeps of the access
//! units whose fragments are not all acknowledged yet.
//!
//! It exists for two readers. The loss path asks it whether a fragment is
//! still current: an IDR fragment is sent again on a verdict (the IDR has
//! nothing to switch to), and any fragment is sent again by the probe timeout
//! to draw an acknowledgement. Without an entry a fragment's content is "gone",
//! and the probe declares a transmission whose content is gone lost on the
//! spot: an H.264 fragment with no entry would be declared lost en masse by
//! any acknowledgement stall, and goog_cc would be told so.
//!
//! An entry lives until its fragment is acknowledged (by any transmission of
//! it), its frame is superseded by an IDR, the cache is trimmed to its newest
//! frames, or the session ends. P-frame fragments are never sent again by a
//! verdict: a recovery IDR replaces the picture instead.

use crate::transport::reliable_emitter::cache::CacheEntry;
use std::collections::{BTreeMap, HashMap};

/// Most frames the cache holds. Bounds the memory of a stalled
/// acknowledgement path (frames keep being produced while nothing is
/// acknowledged) and so how far back the probe's "content is current" reaches.
/// The H.264 in-flight bound of the design (`max(32, 2 srtt / frame + 8)`)
/// is larger on a long path; the backpressure term stops frames long before
/// this matters on a healthy one.
pub(crate) const H264_CACHE_FRAMES: usize = 64;

/// One fragment handed to [`super::ReliableTileEmitter::submit_h264_fragment`].
pub struct H264Submit {
    pub frame_seq: u32,
    pub frag_idx: u16,
    pub is_idr: bool,
    /// The whole frame datagram, `wire_seq` still `NO_ACK`.
    pub bytes: bytes::Bytes,
    /// When the frame became available (the capture), for `queued_at -> ACK`.
    pub queued_at: std::time::Instant,
    /// The probe cluster the fragment is tagged for, if a window is open.
    pub probe: Option<crate::transport::reliable_emitter::cache::ProbeTag>,
}

/// One frame's entries.
struct FrameEntries {
    is_idr: bool,
    frags: HashMap<u16, CacheEntry>,
}

/// Fragments of the frames not yet fully acknowledged.
#[derive(Default)]
pub struct H264Cache {
    frames: BTreeMap<u32, FrameEntries>,
    entries: usize,
    bytes: usize,
}

impl H264Cache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fragments held.
    pub fn len(&self) -> usize {
        self.entries
    }

    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// Frames with at least one fragment held.
    pub fn frames(&self) -> usize {
        self.frames.len()
    }

    /// Datagram bytes held: what is sent and not yet acknowledged.
    pub fn bytes_outstanding(&self) -> usize {
        self.bytes
    }

    pub fn get(&self, frame_seq: u32, frag_idx: u16) -> Option<&CacheEntry> {
        self.frames.get(&frame_seq)?.frags.get(&frag_idx)
    }

    pub fn get_mut(&mut self, frame_seq: u32, frag_idx: u16) -> Option<&mut CacheEntry> {
        self.frames.get_mut(&frame_seq)?.frags.get_mut(&frag_idx)
    }

    /// Whether the held fragment's frame is an IDR. `None` if the frame holds
    /// nothing.
    pub fn is_idr(&self, frame_seq: u32) -> Option<bool> {
        self.frames.get(&frame_seq).map(|f| f.is_idr)
    }

    pub fn insert(&mut self, frame_seq: u32, frag_idx: u16, is_idr: bool, entry: CacheEntry) {
        let frame = self
            .frames
            .entry(frame_seq)
            .or_insert_with(|| FrameEntries {
                is_idr,
                frags: HashMap::new(),
            });
        self.bytes += entry.datagram.len();
        match frame.frags.insert(frag_idx, entry) {
            Some(old) => self.bytes -= old.datagram.len(),
            None => self.entries += 1,
        }
    }

    /// Release a fragment: it was acknowledged. Returns the entry if it was held.
    pub fn remove(&mut self, frame_seq: u32, frag_idx: u16) -> Option<CacheEntry> {
        let frame = self.frames.get_mut(&frame_seq)?;
        let entry = frame.frags.remove(&frag_idx)?;
        self.entries -= 1;
        self.bytes -= entry.datagram.len();
        if frame.frags.is_empty() {
            self.frames.remove(&frame_seq);
        }
        Some(entry)
    }

    /// Forget every frame older than `frame_seq`: an IDR at `frame_seq`
    /// supersedes them. Returns the fragments dropped.
    pub fn drop_before(&mut self, frame_seq: u32) -> usize {
        let keep = self.frames.split_off(&frame_seq);
        let old = std::mem::replace(&mut self.frames, keep);
        let mut dropped = 0;
        for f in old.into_values() {
            for e in f.frags.into_values() {
                dropped += 1;
                self.bytes -= e.datagram.len();
            }
        }
        self.entries -= dropped;
        dropped
    }

    /// Forget every frame from `frame_seq` on: they chain from a frame that
    /// will not be decoded, so nothing of them is worth probing. Returns the
    /// fragments dropped.
    pub fn drop_from(&mut self, frame_seq: u32) -> usize {
        let gone = self.frames.split_off(&frame_seq);
        let mut dropped = 0;
        for f in gone.into_values() {
            for e in f.frags.into_values() {
                dropped += 1;
                self.bytes -= e.datagram.len();
            }
        }
        self.entries -= dropped;
        dropped
    }

    /// Keep only the newest `max_frames` frames. Returns the fragments dropped.
    pub fn trim_to(&mut self, max_frames: usize) -> usize {
        if self.frames.len() <= max_frames {
            return 0;
        }
        if max_frames == 0 {
            let dropped = self.entries;
            self.clear();
            return dropped;
        }
        let cut = *self
            .frames
            .keys()
            .rev()
            .nth(max_frames - 1)
            .expect("more frames than max_frames");
        self.drop_before(cut)
    }

    pub fn clear(&mut self) {
        self.frames.clear();
        self.entries = 0;
        self.bytes = 0;
    }
}

/// The probe padding in flight: datagrams of a probe cluster that repeat
/// something the client already has. They hold no frame slot, so the frame
/// cache's trimming and dropping never reach them, and nothing is ever sent
/// again from them. An entry exists to carry the probe tag to the
/// acknowledgement, and to count its bytes as in flight meanwhile.
#[derive(Default)]
pub struct PadRing {
    entries: std::collections::HashMap<u32, PadEntry>,
    order: std::collections::VecDeque<u32>,
    bytes: usize,
}

#[derive(Debug, Clone, Copy)]
pub struct PadEntry {
    pub probe: crate::transport::reliable_emitter::cache::ProbeTag,
    pub queued_at: std::time::Instant,
    pub len: usize,
}

/// Most padding datagrams held at once (a window pads at most a few hundred
/// kilobytes, and an entry lives two seconds at most).
pub(crate) const PAD_RING_CAP: usize = 512;
/// An unacknowledged padding entry is forgotten after this long.
pub(crate) const PAD_ENTRY_TTL: std::time::Duration = std::time::Duration::from_secs(2);

impl PadRing {
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Datagram bytes of the padding in flight.
    pub fn bytes(&self) -> usize {
        self.bytes
    }

    pub fn get(&self, wire_seq: u32) -> Option<&PadEntry> {
        self.entries.get(&wire_seq)
    }

    pub fn insert(&mut self, wire_seq: u32, entry: PadEntry) {
        while self.entries.len() >= PAD_RING_CAP {
            let Some(old) = self.order.pop_front() else {
                break;
            };
            self.remove(old);
        }
        self.bytes += entry.len;
        if let Some(old) = self.entries.insert(wire_seq, entry) {
            self.bytes -= old.len;
        } else {
            self.order.push_back(wire_seq);
        }
    }

    /// Release an entry: its datagram was acknowledged. Returns whether it was held.
    pub fn remove(&mut self, wire_seq: u32) -> bool {
        match self.entries.remove(&wire_seq) {
            Some(e) => {
                self.bytes -= e.len;
                true
            }
            None => false,
        }
    }

    /// Forget entries older than [`PAD_ENTRY_TTL`] at `now`. Returns how many.
    pub fn expire(&mut self, now: std::time::Instant) -> usize {
        let mut n = 0;
        while let Some(&ws) = self.order.front() {
            match self.entries.get(&ws) {
                None => {
                    self.order.pop_front();
                }
                Some(e) if now.saturating_duration_since(e.queued_at) > PAD_ENTRY_TTL => {
                    self.order.pop_front();
                    self.remove(ws);
                    n += 1;
                }
                Some(_) => break,
            }
        }
        n
    }

    pub fn clear(&mut self) {
        self.entries.clear();
        self.order.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::time::Instant;

    fn entry(len: usize) -> CacheEntry {
        let now = Instant::now();
        CacheEntry {
            datagram: Bytes::from(vec![0u8; len]),
            queued_at: now,
            first_sent_at: now,
            last_sent_at: now,
            attempts: 0,
            loss_attempts: 0,
            latest_ws: 0,
            probe: None,
        }
    }

    #[test]
    fn counts_follow_inserts_and_removes() {
        let mut c = H264Cache::new();
        c.insert(5, 0, true, entry(100));
        c.insert(5, 1, true, entry(50));
        c.insert(6, 0, false, entry(10));
        assert_eq!((c.len(), c.frames(), c.bytes_outstanding()), (3, 2, 160));
        assert_eq!(c.is_idr(5), Some(true));
        assert_eq!(c.is_idr(6), Some(false));
        assert!(c.remove(5, 0).is_some());
        assert!(c.remove(5, 0).is_none(), "released once");
        assert_eq!((c.len(), c.bytes_outstanding()), (2, 60));
        assert!(c.remove(5, 1).is_some());
        assert_eq!(c.frames(), 1, "an emptied frame leaves");
        assert!(c.is_idr(5).is_none());
    }

    #[test]
    fn an_idr_supersedes_every_older_frame_and_only_those() {
        let mut c = H264Cache::new();
        for f in 1..=4 {
            c.insert(f, 0, f == 1, entry(10));
            c.insert(f, 1, f == 1, entry(10));
        }
        assert_eq!(c.drop_before(3), 4);
        assert_eq!((c.len(), c.frames(), c.bytes_outstanding()), (4, 2, 40));
        assert!(c.get(2, 0).is_none() && c.get(3, 1).is_some());
        assert_eq!(c.drop_before(3), 0, "idempotent");
    }

    #[test]
    fn dropping_from_a_frame_forgets_it_and_every_later_one() {
        let mut c = H264Cache::new();
        for f in 1..=4 {
            c.insert(f, 0, false, entry(10));
        }
        assert_eq!(c.drop_from(3), 2);
        assert_eq!((c.len(), c.frames(), c.bytes_outstanding()), (2, 2, 20));
        assert!(c.get(2, 0).is_some() && c.get(3, 0).is_none());
        assert_eq!(c.drop_from(3), 0);
    }

    #[test]
    fn trimming_keeps_the_newest_frames() {
        let mut c = H264Cache::new();
        for f in 10..20 {
            c.insert(f, 0, false, entry(1));
        }
        assert_eq!(c.trim_to(10), 0, "nothing over the bound");
        assert_eq!(c.trim_to(4), 6);
        assert_eq!(c.frames(), 4);
        assert!(c.get(15, 0).is_none() && c.get(16, 0).is_some());
        assert_eq!(c.trim_to(0), 4, "zero keeps nothing");
    }

    #[test]
    fn reinserting_a_fragment_replaces_it_without_double_counting() {
        let mut c = H264Cache::new();
        c.insert(1, 0, false, entry(10));
        c.insert(1, 0, false, entry(30));
        assert_eq!((c.len(), c.bytes_outstanding()), (1, 30));
    }
}

/// The emitter's H.264 half: the submit, resend, acknowledgement and purge
/// paths, and the queue accounting behind the encoder's backpressure.
#[cfg(test)]
mod emitter_tests {
    use super::*;
    use crate::transport::protocol::{fragment_frame, FRAME_HEADER_SIZE, NO_ACK};
    use crate::transport::reliable_emitter::emission_queue::{Emission, WireClass};
    use crate::transport::reliable_emitter::{
        Content, ReliableTileEmitter, RetxCause, RetxRequest,
    };
    use bytes::Bytes;
    use std::time::{Duration, Instant};

    const FRAG: usize = 100;

    /// The fragments of one frame of `n` fragments, as `send_h264_frame`
    /// builds them (every one `NO_ACK` until the emitter stamps it).
    fn frame(frame_seq: u32, is_idr: bool, n: usize) -> Vec<Bytes> {
        fragment_frame(frame_seq, 0, is_idr, &vec![0xAB; FRAG * n], FRAG)
            .into_iter()
            .map(Bytes::from)
            .collect()
    }

    fn submit(e: &mut ReliableTileEmitter, frame_seq: u32, is_idr: bool, n: usize, t: Instant) {
        for (i, dg) in frame(frame_seq, is_idr, n).into_iter().enumerate() {
            e.submit_h264_fragment(
                H264Submit {
                    frame_seq,
                    frag_idx: i as u16,
                    is_idr,
                    bytes: dg,
                    queued_at: t,
                    probe: None,
                },
                t,
            );
        }
    }

    fn stamped(em: &Emission) -> u32 {
        em.wire_seq()
            .expect("a tracked fragment carries a wire_seq")
    }

    #[test]
    fn a_fragment_gets_a_wire_seq_a_transmission_report_and_a_cache_entry() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        submit(&mut e, 7, true, 3, t);
        let sent = e.take_transmissions();
        assert_eq!(sent.len(), 3);
        for (i, tx) in sent.iter().enumerate() {
            assert_eq!(
                tx.content,
                Content::H264Frag {
                    frame_seq: 7,
                    frag_idx: i as u16
                }
            );
            assert_ne!(tx.wire_seq, NO_ACK);
            assert_eq!(tx.wire_bytes, FRAME_HEADER_SIZE + FRAG);
        }
        // What the queue hands the wire carries exactly the reported wire_seq.
        for tx in &sent {
            let em = e.next_emission(t).expect("queued");
            assert!(matches!(em, Emission::Source(_, WireClass::H264)));
            assert_eq!(stamped(&em), tx.wire_seq);
        }
        assert_eq!(e.h264.len(), 3);
        assert_eq!(e.bytes_in_flight(), 3 * (FRAME_HEADER_SIZE + FRAG));
        assert!(e.pto_deadline().is_none(), "nothing has departed yet");
        e.on_departed(sent[0].wire_seq, t);
        assert!(
            e.pto_deadline().is_some(),
            "the probe timer runs for H.264 fragments alone"
        );
    }

    #[test]
    fn a_verdict_resends_an_idr_fragment_and_not_a_p_fragment_and_a_probe_resends_either() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        submit(&mut e, 1, true, 2, t);
        submit(&mut e, 2, false, 2, t);
        let ws: Vec<u32> = e.take_transmissions().iter().map(|x| x.wire_seq).collect();
        let idr = |i| Content::H264Frag {
            frame_seq: 1,
            frag_idx: i,
        };
        let p = |i| Content::H264Frag {
            frame_seq: 2,
            frag_idx: i,
        };
        assert_eq!(
            e.request_retransmit(idr(0), ws[0], RetxCause::Loss),
            RetxRequest::Queued
        );
        assert_eq!(
            e.request_retransmit(p(0), ws[2], RetxCause::Loss),
            RetxRequest::NotRetransmitted,
            "a P fragment is replaced by a recovery IDR, not sent late"
        );
        assert_eq!(
            e.request_retransmit(p(1), ws[3], RetxCause::Probe),
            RetxRequest::Queued,
            "but the probe sends it, to draw an acknowledgement"
        );
        // Asking again while the first request waits is one resend.
        assert_eq!(
            e.request_retransmit(idr(0), ws[0], RetxCause::Loss),
            RetxRequest::AlreadyQueued
        );
        let mut n = 0;
        let spent = e.tick_loss_budgeted(t, usize::MAX, usize::MAX, &mut n);
        assert_eq!(n, 2);
        assert_eq!(spent, 2 * (FRAME_HEADER_SIZE + FRAG));
        let again = e.take_transmissions();
        assert_eq!(again.len(), 2);
        assert!(
            again.iter().all(|x| !ws.contains(&x.wire_seq)),
            "new wire_seqs"
        );
        // Both resends are in the queue behind the 4 originals, as loss resends.
        let mut classes = Vec::new();
        while let Some(em) = e.next_emission(t) {
            classes.push(em.class());
        }
        assert_eq!(classes[4..], [WireClass::LossRetransmit; 2]);
        // The original transmission is no longer the newest of its fragment.
        assert_eq!(
            e.request_retransmit(idr(0), ws[0], RetxCause::Loss),
            RetxRequest::NewerInFlight
        );
        assert_eq!(e.stats.h264_idr_resends, 1);
        assert_eq!(e.stats.h264_probe_resends, 1);
        assert_eq!(e.stats.h264_p_verdicts_not_resent, 1);
    }

    #[test]
    fn an_acknowledgement_releases_the_fragment_once_whichever_transmission_it_named() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        submit(&mut e, 4, true, 1, t);
        assert!(e.on_h264_ack(4, 0));
        assert!(!e.on_h264_ack(4, 0), "a duplicate finds nothing");
        assert!(e.h264.is_empty());
        assert_eq!((e.stats.h264_ack_hits, e.stats.h264_ack_misses), (1, 1));
        // Nothing cached: the probe timer is off and a request finds no content.
        assert!(e.pto_deadline().is_none());
        assert_eq!(
            e.request_retransmit(
                Content::H264Frag {
                    frame_seq: 4,
                    frag_idx: 0
                },
                0,
                RetxCause::Probe
            ),
            RetxRequest::ContentGone
        );
    }

    #[test]
    fn an_idr_supersedes_older_frames_in_the_cache_and_in_the_queue() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        submit(&mut e, 1, true, 2, t);
        submit(&mut e, 2, false, 2, t);
        e.queue_untracked(
            Bytes::from(crate::transport::protocol::build_frame_parity_datagram(
                2, 0, false, 2, &[0u8; 10],
            )),
            WireClass::Parity,
        );
        // One datagram has already left the queue (frame 1, fragment 0).
        let first = e.next_emission(t).expect("queued");
        let first_ws = stamped(&first);
        let all = e.take_transmissions();
        let queued_before = e.queued_h264_bytes();
        assert_eq!(
            queued_before,
            3 * (FRAME_HEADER_SIZE + FRAG) + FRAME_HEADER_SIZE + 10
        );

        // An IDR at frame 3 supersedes frames 1 and 2.
        let removed = e.supersede_h264_before(3);
        let mut expect: Vec<u32> = all.iter().skip(1).map(|x| x.wire_seq).collect();
        expect.sort_unstable();
        let mut got = removed.clone();
        got.sort_unstable();
        assert_eq!(
            got, expect,
            "the tracked fragments still queued, not the one that left"
        );
        assert!(!removed.contains(&first_ws));
        assert_eq!(e.h264.len(), 0, "the cache forgot both frames");
        assert_eq!(
            e.queued_h264_bytes(),
            0,
            "the queue holds none, parity included"
        );
        assert!(e.next_emission(t).is_none());
        assert_eq!(
            e.stats.h264_queue_purged, 4,
            "three fragments and the parity"
        );
        assert_eq!(e.stats.h264_cache_dropped, 4);
    }

    #[test]
    fn the_untracked_parity_names_no_transmission() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        e.queue_untracked(
            Bytes::from(crate::transport::protocol::build_frame_parity_datagram(
                9, 0, false, 2, &[1u8; 10],
            )),
            WireClass::Parity,
        );
        let em = e.next_emission(t).expect("queued");
        assert_eq!(em.class(), WireClass::Parity);
        assert_eq!(em.wire_seq(), None, "NO_ACK is not a wire_seq");
        assert!(e.take_transmissions().is_empty(), "and is not reported");
    }

    #[test]
    fn the_queue_counts_frame_bytes_through_hold_and_pop() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        submit(&mut e, 1, false, 2, t);
        let per = FRAME_HEADER_SIZE + FRAG;
        assert_eq!(e.queued_h264_bytes(), 2 * per);
        let em = e.next_emission(t).unwrap();
        assert_eq!(e.queued_h264_bytes(), per);
        e.hold_emission(em);
        assert_eq!(
            e.queued_h264_bytes(),
            2 * per,
            "a held emission is queued again"
        );
        // A tile datagram is not frame traffic.
        let mut tile = vec![0u8; 25];
        tile[0] = 0x80;
        e.submit_one(
            crate::transport::reliable_emitter::EmitKey::new(1, 0, 0, 0),
            Bytes::from(tile),
            t,
            None,
            t,
        );
        assert_eq!(e.queued_h264_bytes(), 2 * per);
    }

    #[test]
    fn the_cache_is_trimmed_to_its_newest_frames() {
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        for f in 0..(H264_CACHE_FRAMES as u32 + 5) {
            submit(&mut e, f, false, 1, t);
        }
        e.trim_h264_cache();
        assert_eq!(e.h264.frames(), H264_CACHE_FRAMES);
        assert!(e.h264.get(4, 0).is_none() && e.h264.get(5, 0).is_some());
        assert_eq!(e.stats.h264_cache_dropped, 5);
        let _ = Duration::ZERO;
    }

    /// H.264 fragments take wire_seqs from the same counter as tile passes but
    /// are not tile XOR group members: a tile group they interrupt is
    /// abandoned (no parity with a false membership), and a later contiguous
    /// run of tile passes still forms a group of its own.
    #[test]
    fn h264_fragments_do_not_join_tile_parity_groups() {
        use crate::transport::reliable_emitter::{EmitKey, FEC_GROUP_SIZE_K};
        let t = Instant::now();
        let tile = |e: &mut ReliableTileEmitter, i: u32| {
            e.submit_one(
                EmitKey::new(i, 0, 0, 0),
                Bytes::from(vec![0x80u8; 25]),
                t,
                None,
                t,
            );
        };
        let parity_groups = |e: &mut ReliableTileEmitter| {
            let mut now = t;
            let mut groups = Vec::new();
            for _ in 0..50 {
                now += Duration::from_millis(1);
                while let Some(em) = e.next_emission(now) {
                    if let Some(g) = em.parity_group() {
                        groups.push(g);
                    }
                }
            }
            groups
        };
        let mut e = ReliableTileEmitter::new(t);
        e.set_tile_fec(true);
        let half = FEC_GROUP_SIZE_K as u32 / 2;
        for i in 0..half {
            tile(&mut e, i);
        }
        submit(&mut e, 99, false, 1, t); // one H.264 fragment in the run
        for i in half..FEC_GROUP_SIZE_K as u32 - 1 {
            tile(&mut e, i);
        }
        assert!(
            parity_groups(&mut e).is_empty(),
            "K - 1 tile passes and a fragment are not a group of K"
        );
        // The run restarts after the fragment: the tile passes that follow it
        // (wire_seq half + 1 onwards, contiguous) form one group of K at their
        // own start, never one that begins before the fragment.
        let after_fragment = half + 1;
        for i in 100..100 + FEC_GROUP_SIZE_K as u32 {
            tile(&mut e, i);
        }
        assert_eq!(parity_groups(&mut e), vec![after_fragment]);
    }

    #[test]
    fn padding_is_flagged_tagged_billed_in_flight_and_never_sent_again() {
        use crate::transport::protocol::FRAME_FLAG_PAD;
        use crate::transport::reliable_emitter::cache::ProbeTag;
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        let template = frame(3, false, 1).remove(0);
        assert_eq!(
            template[16] & FRAME_FLAG_PAD,
            0,
            "premise: the original is no pad"
        );
        let tag = ProbeTag {
            id: 1_000_000_001,
            min_probes: 5,
            min_bytes: 5000,
            bytes_sent_before: 0,
        };
        let len = e.submit_h264_pad(&template, tag, t);
        assert_eq!(len, template.len());
        let sent = e.take_transmissions();
        assert_eq!(sent.len(), 1);
        let ws = sent[0].wire_seq;
        assert_eq!(
            sent[0].content,
            Content::Pad(ws),
            "named by its own wire_seq"
        );
        let em = e.next_emission(t).expect("queued");
        let Emission::Source(bytes, _) = &em else {
            panic!("a source emission")
        };
        assert_eq!(
            bytes[16] & FRAME_FLAG_PAD,
            FRAME_FLAG_PAD,
            "flagged on the wire"
        );
        assert_eq!(stamped(&em), ws);
        assert_eq!(bytes[..8], template[..8], "it still names the old frame");
        // The tag waits for the acknowledgement; the bytes count as in flight.
        assert_eq!(e.pads.get(ws).map(|p| p.probe), Some(tag));
        assert_eq!(e.bytes_in_flight(), len);
        // No path sends it again, whoever asks and whatever the cause.
        for cause in [RetxCause::Loss, RetxCause::Probe] {
            assert_eq!(
                e.request_retransmit(Content::Pad(ws), ws, cause),
                RetxRequest::ContentGone
            );
        }
        // It holds no frame slot: the frame cache never saw it.
        assert!(e.h264.is_empty() && e.h264.frames() == 0);
    }

    #[test]
    fn padding_is_forgotten_when_acknowledged_expired_or_the_session_ends() {
        use crate::transport::reliable_emitter::cache::ProbeTag;
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        let template = frame(3, false, 1).remove(0);
        let tag = ProbeTag {
            id: 1_000_000_002,
            min_probes: 5,
            min_bytes: 5000,
            bytes_sent_before: 0,
        };
        for _ in 0..3 {
            e.submit_h264_pad(&template, tag, t);
        }
        let ws: Vec<u32> = e.take_transmissions().iter().map(|x| x.wire_seq).collect();
        assert_eq!(e.pads.len(), 3);
        assert!(e.pads.remove(ws[0]), "acknowledged");
        assert!(!e.pads.remove(ws[0]), "once");
        e.expire_h264_pad(t + PAD_ENTRY_TTL);
        assert_eq!(e.pads.len(), 2, "not yet: the TTL is exclusive");
        e.expire_h264_pad(t + PAD_ENTRY_TTL + Duration::from_millis(1));
        assert!(e.pads.is_empty() && e.pads.bytes() == 0, "expired");
        e.submit_h264_pad(&template, tag, t);
        assert_eq!(e.pads.len(), 1);
        e.clear_cache();
        assert!(e.pads.is_empty());
    }

    #[test]
    fn the_pad_ring_is_bounded() {
        use crate::transport::reliable_emitter::cache::ProbeTag;
        let t = Instant::now();
        let mut e = ReliableTileEmitter::new(t);
        let template = frame(3, false, 1).remove(0);
        let tag = ProbeTag {
            id: 1_000_000_003,
            min_probes: 5,
            min_bytes: 5000,
            bytes_sent_before: 0,
        };
        for _ in 0..PAD_RING_CAP + 50 {
            e.submit_h264_pad(&template, tag, t);
        }
        assert_eq!(e.pads.len(), PAD_RING_CAP);
        assert_eq!(e.pads.bytes(), PAD_RING_CAP * template.len());
    }
}
