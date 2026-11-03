//! Per-frame state ring for the `RefManager` (design 3.2 and 5.3).
//!
//! A ring of [`TRACKER_RING`] frames keyed by `frame_seq % 256` with a seq
//! check. Everything is O(1) per datagram or acknowledgement; the only scans
//! are `FrameTable::overdue` (once per frame decision) and the dependency walk
//! in `usable`, which stops at the first decoded frame.

use std::time::Instant;

pub(crate) const TRACKER_RING: usize = 256;

/// `frame_seq` is 31 bits on the wire (bit 31 is the tile flag).
const FRAME_SEQ_MASK: u32 = 0x7FFF_FFFF;

/// Is `a` strictly older than `b`? RFC 1982 serial arithmetic on the 31-bit
/// `frame_seq`, as the rest of the repo compares it, so a wrap is safe.
pub(crate) fn seq_lt(a: u32, b: u32) -> bool {
    let forward = b.wrapping_sub(a) & FRAME_SEQ_MASK;
    forward != 0 && forward < (1 << 30)
}

/// What a frame is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PicKind {
    Idr,
    /// Predicts from the previous reference frame.
    Healthy,
    /// Non-reference P from the anchor, during recovery.
    Recovery,
    /// Reference P from the anchor that restarts the healthy chain.
    Reroot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrameState {
    Sent,
    /// The client reported it decoded (0x0A). Overrides `Suspect`.
    Decoded,
    /// A ledger verdict or the decode deadline: a hypothesis.
    Suspect,
    /// A fact: refused by the client, or suspect and superseded.
    Dead,
}

#[derive(Debug, Clone)]
pub(crate) struct FrameRec {
    pub seq: u32,
    pub kind: PicKind,
    /// The picture the slice predicts from (`None` for an IDR).
    pub target: Option<u32>,
    /// The frame this one's usability depends on: `target` for a healthy
    /// chain, `None` when it predicts from a confirmed anchor (the client
    /// holds that picture by definition).
    pub dep: Option<u32>,
    pub reference: bool,
    /// The long-term slot this frame was marked into, if any (an IDR: 0).
    pub mark_slot: Option<u8>,
    pub state: FrameState,
    pub sent_at: Instant,
    /// Position in the sequence of H.264 frames sent.
    pub send_idx: u64,
    frags_total: u16,
    acked: Vec<u64>,
    acked_count: u16,
}

impl FrameRec {
    pub(crate) fn new(
        seq: u32,
        kind: PicKind,
        frags_total: u16,
        sent_at: Instant,
        send_idx: u64,
    ) -> Self {
        Self {
            seq,
            kind,
            target: None,
            dep: None,
            reference: false,
            mark_slot: None,
            state: FrameState::Sent,
            sent_at,
            send_idx,
            frags_total,
            acked: vec![0; usize::from(frags_total).div_ceil(64)],
            acked_count: 0,
        }
    }

    pub(crate) fn complete(&self) -> bool {
        self.acked_count == self.frags_total
    }
}

/// What a fragment acknowledgement did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FragAck {
    Unknown,
    Duplicate,
    Acked { complete: bool },
}

pub(crate) struct FrameTable {
    ring: Vec<Option<FrameRec>>,
}

impl Default for FrameTable {
    fn default() -> Self {
        Self {
            ring: (0..TRACKER_RING).map(|_| None).collect(),
        }
    }
}

impl FrameTable {
    fn slot(seq: u32) -> usize {
        seq as usize % TRACKER_RING
    }

    pub(crate) fn insert(&mut self, rec: FrameRec) {
        let s = Self::slot(rec.seq);
        self.ring[s] = Some(rec);
    }

    pub(crate) fn get(&self, seq: u32) -> Option<&FrameRec> {
        self.ring[Self::slot(seq)].as_ref().filter(|f| f.seq == seq)
    }

    fn get_mut(&mut self, seq: u32) -> Option<&mut FrameRec> {
        self.ring[Self::slot(seq)].as_mut().filter(|f| f.seq == seq)
    }

    pub(crate) fn clear(&mut self) {
        self.ring.iter_mut().for_each(|s| *s = None);
    }

    pub(crate) fn state(&self, seq: u32) -> Option<FrameState> {
        self.get(seq).map(|f| f.state)
    }

    pub(crate) fn frag_acked(&mut self, seq: u32, frag_idx: u16) -> FragAck {
        let Some(f) = self.get_mut(seq) else {
            return FragAck::Unknown;
        };
        let (word, bit) = (usize::from(frag_idx / 64), 1u64 << (frag_idx % 64));
        if frag_idx >= f.frags_total || f.acked[word] & bit != 0 {
            return FragAck::Duplicate;
        }
        f.acked[word] |= bit;
        f.acked_count += 1;
        FragAck::Acked {
            complete: f.complete(),
        }
    }

    /// `Sent|Suspect -> Decoded`. Returns false when the frame is unknown or
    /// dead (a decoded bit never resurrects a fact).
    pub(crate) fn set_decoded(&mut self, seq: u32) -> bool {
        match self.get_mut(seq) {
            Some(f) if matches!(f.state, FrameState::Sent | FrameState::Suspect) => {
                f.state = FrameState::Decoded;
                true
            }
            Some(f) => f.state == FrameState::Decoded,
            None => false,
        }
    }

    /// `Sent -> Suspect`. Returns true when the state changed.
    pub(crate) fn set_suspect(&mut self, seq: u32) -> bool {
        match self.get_mut(seq) {
            Some(f) if f.state == FrameState::Sent => {
                f.state = FrameState::Suspect;
                true
            }
            _ => false,
        }
    }

    /// `Sent|Suspect -> Dead`. A decoded frame stays decoded.
    pub(crate) fn set_dead(&mut self, seq: u32) -> bool {
        match self.get_mut(seq) {
            Some(f) if matches!(f.state, FrameState::Sent | FrameState::Suspect) => {
                f.state = FrameState::Dead;
                true
            }
            _ => false,
        }
    }

    /// Suspect frames older than `before` (by seq) become dead: the chain has
    /// moved past them.
    pub(crate) fn supersede_suspects(&mut self, before: u32) {
        for f in self.ring.iter_mut().flatten() {
            if seq_lt(f.seq, before) && f.state == FrameState::Suspect {
                f.state = FrameState::Dead;
            }
        }
    }

    /// Mark every `Sent` frame older than `deadline` as suspect; returns how
    /// many changed. One scan per frame decision.
    pub(crate) fn suspect_overdue(&mut self, now: Instant, deadline: std::time::Duration) -> u32 {
        let mut n = 0;
        for f in self.ring.iter_mut().flatten() {
            if f.state == FrameState::Sent && now.saturating_duration_since(f.sent_at) > deadline {
                f.state = FrameState::Suspect;
                n += 1;
            }
        }
        n
    }

    /// A frame is usable when the client has it, or it may yet arrive and the
    /// frame it depends on is usable in turn. A dependency that left the ring
    /// is not usable (it would have been judged long ago).
    pub(crate) fn usable(&self, seq: u32) -> bool {
        let mut cur = seq;
        for _ in 0..=TRACKER_RING {
            let Some(f) = self.get(cur) else {
                return false;
            };
            match f.state {
                FrameState::Decoded => return true,
                FrameState::Suspect | FrameState::Dead => return false,
                FrameState::Sent => match f.dep {
                    Some(d) => cur = d,
                    None => return true,
                },
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn rec(seq: u32, dep: Option<u32>, t: Instant) -> FrameRec {
        let mut r = FrameRec::new(seq, PicKind::Healthy, 3, t, u64::from(seq));
        r.dep = dep;
        r.target = dep;
        r
    }

    #[test]
    fn fragments_complete_once_and_unknown_frames_are_reported() {
        let mut t = FrameTable::default();
        t.insert(rec(5, None, Instant::now()));
        assert_eq!(t.frag_acked(5, 0), FragAck::Acked { complete: false });
        assert_eq!(t.frag_acked(5, 0), FragAck::Duplicate);
        assert_eq!(t.frag_acked(5, 9), FragAck::Duplicate);
        assert_eq!(t.frag_acked(5, 1), FragAck::Acked { complete: false });
        assert_eq!(t.frag_acked(5, 2), FragAck::Acked { complete: true });
        assert_eq!(t.frag_acked(6, 0), FragAck::Unknown);
        // A seq 256 later reuses the slot and the old one is gone.
        t.insert(rec(5 + 256, None, Instant::now()));
        assert!(t.get(5).is_none());
    }

    #[test]
    fn usability_follows_the_dependency_chain_and_decoded_overrides() {
        let now = Instant::now();
        let mut t = FrameTable::default();
        t.insert(rec(1, None, now));
        t.insert(rec(2, Some(1), now));
        t.insert(rec(3, Some(2), now));
        assert!(t.usable(3));
        assert!(t.set_suspect(1));
        assert!(!t.usable(1) && !t.usable(2) && !t.usable(3));
        // A decoded bit overrides the verdict; dependents recover.
        assert!(t.set_decoded(1));
        assert!(t.usable(3));
        // A decoded dependent is usable whatever its target did.
        t.set_dead(2);
        assert!(!t.usable(3));
        assert!(!t.set_decoded(2), "dead is a fact");
    }

    #[test]
    fn serial_comparison_survives_the_wrap() {
        assert!(seq_lt(1, 2) && !seq_lt(2, 1) && !seq_lt(5, 5));
        assert!(seq_lt(0x7FFF_FFFE, 1), "1 is newer than 2^31 - 2");
        assert!(!seq_lt(1, 0x7FFF_FFFE));
    }

    #[test]
    fn deadline_and_supersession() {
        let t0 = Instant::now();
        let mut t = FrameTable::default();
        t.insert(rec(1, None, t0));
        t.insert(rec(2, None, t0 + Duration::from_millis(300)));
        assert_eq!(
            t.suspect_overdue(t0 + Duration::from_millis(400), Duration::from_millis(250)),
            1
        );
        assert_eq!(t.state(1), Some(FrameState::Suspect));
        assert_eq!(t.state(2), Some(FrameState::Sent));
        t.supersede_suspects(2);
        assert_eq!(t.state(1), Some(FrameState::Dead));
    }
}
