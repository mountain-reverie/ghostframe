//! Library-to-server feedback: which tiles the full compare found changed.
//!
//! The server decides what to grab; the GPU compare that says what really
//! changed runs in the library. The server's safety net (damage hints versus
//! sweep versus truth) needs that answer back, one record per analysed frame,
//! without touching the wire format. The worker publishes a [`FrameFeedback`]
//! into a [`FeedbackSink`] and the capture loop drains it at the top of its
//! next tick.
//!
//! Cost: one bit per tile (`ceil(tiles / 64)` words) built from the dirty list
//! the compare already produced, so O(dirty tiles) to fill and O(tiles / 64)
//! to move. The sink is bounded and drops its oldest record when full, so a
//! server that stops draining cannot grow it; it is also off until
//! [`FeedbackSink::enable`], so a session that does not use it pays nothing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::tile::TILE_SIZE;

/// Records kept when the server is slow to drain.
const SINK_CAPACITY: usize = 32;

/// What the compare of one frame concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FeedbackKind {
    /// The tiles whose pixels differ from the reference: one bit per tile,
    /// row-major (`y * cols + x`), `ceil(cols * rows / 64)` words.
    Dirty(Vec<u64>),
    /// Every tile came back dirty. That is what a frame compared against no
    /// reference looks like (the first frame, a session reset, a resize), as
    /// well as a full-screen redraw, so it says nothing about any one tile.
    Saturated,
    /// The frame never reached a compare (no client, a failed GPU stage, a
    /// session that ended meanwhile).
    Dropped,
}

/// The library's verdict on one submitted frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameFeedback {
    /// The `FrameSubmission::timestamp_us` of the frame this is about: the
    /// producer's tick identity (the server stamps one per tick).
    pub timestamp_us: u32,
    /// The frame size in pixels; the tile grid of `kind` follows from it.
    pub width: u32,
    pub height: u32,
    pub kind: FeedbackKind,
}

impl FrameFeedback {
    /// Build the verdict from the flat indices of a dirty list.
    /// (Public so the server's tests can pin the bit layout against its own.)
    pub fn from_dirty(timestamp_us: u32, width: u32, height: u32, dirty: &[u32]) -> Self {
        let tiles = tile_count(width, height);
        let kind = if tiles > 0 && dirty.len() >= tiles {
            FeedbackKind::Saturated
        } else {
            let mut words = vec![0u64; tiles.div_ceil(64)];
            for &i in dirty {
                let i = i as usize;
                if i < tiles {
                    words[i / 64] |= 1 << (i % 64);
                }
            }
            FeedbackKind::Dirty(words)
        };
        Self {
            timestamp_us,
            width,
            height,
            kind,
        }
    }

    pub(crate) fn dropped(timestamp_us: u32, width: u32, height: u32) -> Self {
        Self {
            timestamp_us,
            width,
            height,
            kind: FeedbackKind::Dropped,
        }
    }
}

fn tile_count(width: u32, height: u32) -> usize {
    (width.div_ceil(TILE_SIZE) as usize) * (height.div_ceil(TILE_SIZE) as usize)
}

/// A bounded, switchable mailbox from the frame worker to the capture loop.
#[derive(Debug, Default)]
pub struct FeedbackSink {
    enabled: AtomicBool,
    queue: Mutex<VecDeque<FrameFeedback>>,
}

impl FeedbackSink {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start recording. Off by default.
    pub fn enable(&self) {
        self.enabled.store(true, Ordering::Release);
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Record `fb`, dropping the oldest record when full. No-op while off.
    pub(crate) fn publish(&self, fb: FrameFeedback) {
        if !self.is_enabled() {
            return;
        }
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        if q.len() >= SINK_CAPACITY {
            q.pop_front();
        }
        q.push_back(fb);
    }

    /// Move every record, oldest first, into `out`.
    pub fn drain_into(&self, out: &mut Vec<FrameFeedback>) {
        let mut q = self.queue.lock().unwrap_or_else(|e| e.into_inner());
        out.extend(q.drain(..));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dirty_list_becomes_bits_row_major() {
        // 100x40 px: 4x2 tiles of 32.
        let fb = FrameFeedback::from_dirty(7, 100, 40, &[0, 5]);
        assert_eq!(fb.kind, FeedbackKind::Dirty(vec![0b10_0001]));
        assert_eq!(fb.timestamp_us, 7);
    }

    #[test]
    fn every_tile_dirty_is_saturated_not_a_mask() {
        let all: Vec<u32> = (0..8).collect();
        assert_eq!(
            FrameFeedback::from_dirty(0, 100, 40, &all).kind,
            FeedbackKind::Saturated
        );
        // One short of all is still a mask.
        assert!(matches!(
            FrameFeedback::from_dirty(0, 100, 40, &all[..7]).kind,
            FeedbackKind::Dirty(_)
        ));
    }

    #[test]
    fn the_sink_is_off_until_enabled_and_bounded_after() {
        let s = FeedbackSink::new();
        s.publish(FrameFeedback::dropped(1, 64, 64));
        let mut out = Vec::new();
        s.drain_into(&mut out);
        assert!(out.is_empty(), "recorded while off");
        s.enable();
        for t in 0..(SINK_CAPACITY as u32 + 5) {
            s.publish(FrameFeedback::dropped(t, 64, 64));
        }
        s.drain_into(&mut out);
        assert_eq!(out.len(), SINK_CAPACITY);
        assert_eq!(out[0].timestamp_us, 5, "the oldest records were dropped");
        s.drain_into(&mut out);
        assert_eq!(out.len(), SINK_CAPACITY, "drain empties the sink");
    }
}
