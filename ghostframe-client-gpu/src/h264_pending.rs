//! Which fed H.264 access units are still waiting for their picture.
//!
//! A decoder may delay its output: the GStreamer backend hands a picture back
//! on the `decode` call after the one that fed it. The frame ack must still say
//! "decoded" for such a frame, so the renderer keeps the `frame_seq`s it has fed
//! and not yet answered, in feed order, and answers each when its picture comes
//! out. Decoders here output in decode order (no B-frames), as the web
//! client's `decoder.ts` also assumes, so a pending seq older than an output
//! one will never get a picture and is answered `NotOutput`.
//!
//! Position in the FIFO decides, never numeric order, so `frame_seq` wrap is
//! not a case.

use std::collections::VecDeque;

use ghostframe_client_core::H264Outcome;

/// Most access units that may wait for a picture. Past this the oldest is
/// refused: a decoder this far behind is not keeping up, and an unanswered seq
/// would otherwise leave the server waiting.
pub(crate) const MAX_PENDING: usize = 16;

/// A delay of more than this many units is worth a log line; one or two is the
/// normal shape of a pipelined decoder.
pub(crate) const WARN_PENDING: usize = 4;

#[derive(Debug, Default)]
pub(crate) struct PendingFrames {
    fifo: VecDeque<u32>,
}

impl PendingFrames {
    pub(crate) fn len(&self) -> usize {
        self.fifo.len()
    }

    /// An access unit was handed to the decoder. Returns the answer owed for
    /// the unit pushed out of a full FIFO, if any.
    pub(crate) fn fed(&mut self, frame_seq: u32) -> Option<(u32, H264Outcome)> {
        let evicted = if self.fifo.len() >= MAX_PENDING {
            self.fifo.pop_front().map(|s| (s, H264Outcome::NotOutput))
        } else {
            None
        };
        self.fifo.push_back(frame_seq);
        evicted
    }

    /// A picture tagged `pts` came out. Older pending units are refused, this
    /// one is decoded. A pts that is not pending (already answered, or from
    /// before a reset) answers nothing.
    pub(crate) fn output(&mut self, pts: Option<i64>) -> Vec<(u32, H264Outcome)> {
        let Some(seq) = pts.and_then(|p| u32::try_from(p).ok()) else {
            return Vec::new();
        };
        let Some(at) = self.fifo.iter().position(|&s| s == seq) else {
            return Vec::new();
        };
        let mut out: Vec<_> = self
            .fifo
            .drain(..at)
            .map(|s| (s, H264Outcome::NotOutput))
            .collect();
        self.fifo.pop_front();
        out.push((seq, H264Outcome::Decoded));
        out
    }

    /// Answer every pending unit `NotOutput`: the decoder was reset or failed,
    /// so none of them will produce a picture.
    pub(crate) fn refuse_all(&mut self) -> Vec<(u32, H264Outcome)> {
        self.fifo
            .drain(..)
            .map(|s| (s, H264Outcome::NotOutput))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A decoder that outputs each unit `delay` calls after it was fed, in
    /// decode order, optionally dropping some outputs.
    struct FakeDecoder {
        delay: usize,
        held: VecDeque<i64>,
        drop_pts: Vec<i64>,
    }

    impl FakeDecoder {
        fn new(delay: usize) -> Self {
            Self {
                delay,
                held: VecDeque::new(),
                drop_pts: Vec::new(),
            }
        }
        fn decode(&mut self, pts: i64) -> Vec<i64> {
            self.held.push_back(pts);
            let mut out = Vec::new();
            while self.held.len() > self.delay {
                let p = self.held.pop_front().unwrap();
                if !self.drop_pts.contains(&p) {
                    out.push(p);
                }
            }
            out
        }
    }

    /// The renderer's per-call sequence, minus the decoder.
    fn step(
        p: &mut PendingFrames,
        d: &mut FakeDecoder,
        seq: u32,
        results: &mut Vec<(u32, H264Outcome)>,
    ) {
        results.extend(p.fed(seq));
        for pts in d.decode(i64::from(seq)) {
            results.extend(p.output(Some(pts)));
        }
    }

    fn count(r: &[(u32, H264Outcome)], o: H264Outcome) -> usize {
        r.iter().filter(|(_, x)| *x == o).count()
    }

    #[test]
    fn zero_delay_decodes_each_frame_in_its_own_call() {
        let (mut p, mut d, mut r) = (PendingFrames::default(), FakeDecoder::new(0), vec![]);
        for seq in 10..20 {
            step(&mut p, &mut d, seq, &mut r);
            assert_eq!(r.last(), Some(&(seq, H264Outcome::Decoded)));
            assert_eq!(p.len(), 0);
        }
        assert_eq!(count(&r, H264Outcome::Decoded), 10);
    }

    #[test]
    fn delays_still_decode_every_frame_once_they_come_out() {
        for delay in [1, 2] {
            let (mut p, mut d, mut r) = (PendingFrames::default(), FakeDecoder::new(delay), vec![]);
            for seq in 0..50 {
                step(&mut p, &mut d, seq, &mut r);
                assert!(p.len() <= delay + 1);
            }
            // The last `delay` frames are still inside the decoder.
            assert_eq!(count(&r, H264Outcome::Decoded), 50 - delay, "delay {delay}");
            assert_eq!(count(&r, H264Outcome::NotOutput), 0, "delay {delay}");
            assert_eq!(p.len(), delay);
        }
    }

    #[test]
    fn a_gap_refuses_the_skipped_unit_only() {
        let (mut p, mut d, mut r) = (PendingFrames::default(), FakeDecoder::new(1), vec![]);
        d.drop_pts.push(5);
        for seq in 0..10 {
            step(&mut p, &mut d, seq, &mut r);
        }
        assert_eq!(count(&r, H264Outcome::NotOutput), 1);
        assert!(r.contains(&(5, H264Outcome::NotOutput)));
        for seq in (0..9).filter(|s| *s != 5) {
            assert!(r.contains(&(seq, H264Outcome::Decoded)), "seq {seq}");
        }
    }

    #[test]
    fn error_then_reset_refuses_pending_and_ignores_late_pictures() {
        let (mut p, mut d, mut r) = (PendingFrames::default(), FakeDecoder::new(2), vec![]);
        for seq in 0..5 {
            step(&mut p, &mut d, seq, &mut r);
        }
        // Units 3 and 4 are inside the decoder; the next one errors.
        assert_eq!(p.len(), 2);
        r.extend(p.refuse_all());
        assert!(r.contains(&(3, H264Outcome::NotOutput)));
        assert!(r.contains(&(4, H264Outcome::NotOutput)));
        assert_eq!(p.len(), 0);
        // A picture for an already-refused unit comes out late: no answer.
        assert!(p.output(Some(3)).is_empty());
        // After the reset the stream resumes cleanly.
        let mut d = FakeDecoder::new(1);
        let mut r = vec![];
        for seq in 100..104 {
            step(&mut p, &mut d, seq, &mut r);
        }
        assert_eq!(count(&r, H264Outcome::Decoded), 3);
        assert_eq!(count(&r, H264Outcome::NotOutput), 0);
    }

    #[test]
    fn untagged_and_unknown_pts_answer_nothing() {
        let mut p = PendingFrames::default();
        p.fed(1);
        assert!(p.output(None).is_empty());
        assert!(p.output(Some(-1)).is_empty());
        assert!(p.output(Some(99)).is_empty());
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn the_fifo_is_bounded_and_wraps_cleanly() {
        let mut p = PendingFrames::default();
        let mut evicted = 0;
        for i in 0..(MAX_PENDING as u32 + 5) {
            if p.fed((u32::MAX - 2).wrapping_add(i)).is_some() {
                evicted += 1;
            }
            assert!(p.len() <= MAX_PENDING);
        }
        assert_eq!(evicted, 5);
        // Position, not numeric order, decides across the u32 wrap.
        let seq = (u32::MAX - 2).wrapping_add(10);
        let r = p.output(Some(i64::from(seq)));
        assert_eq!(r.last(), Some(&(seq, H264Outcome::Decoded)));
    }
}
