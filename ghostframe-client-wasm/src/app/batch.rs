// SPDX-License-Identifier: MIT OR Apache-2.0
//! Inbound datagram batching (PC-3). The read loop only queues, with each
//! datagram's arrival instant (ACK timing and suspension detection read it per
//! datagram); the queue reaches the core in one call per tick. rAF is paused in
//! a hidden tab, so two fallbacks keep intake moving: a backlog threshold and a
//! timer. Neither fires in normal foreground use, where the tick runs every
//! ~16 ms.

/// Datagrams queued before a flush is forced.
pub const FLUSH_BACKLOG: usize = 32;
/// How long a datagram may wait for a tick before the timer flushes it.
pub const FLUSH_FALLBACK_MS: i32 = 50;

/// Datagrams back to back in one buffer, with their lengths and arrival
/// stamps: the `concat`/`lens`/`arrivals_us` form the core's batch call takes.
#[derive(Debug, Default)]
pub struct DatagramBatch {
    concat: Vec<u8>,
    lens: Vec<u32>,
    arrivals_us: Vec<f64>,
}

impl DatagramBatch {
    /// Queue one datagram of `len` bytes. `fill` writes it straight into the
    /// batch (the browser side copies from the JS `Uint8Array` here: one copy
    /// into wasm memory, no intermediate `Vec`).
    pub fn push_with(&mut self, len: usize, arrival_us: f64, fill: impl FnOnce(&mut [u8])) {
        let start = self.concat.len();
        self.concat.resize(start + len, 0);
        fill(&mut self.concat[start..]);
        self.lens.push(len as u32);
        self.arrivals_us.push(arrival_us);
    }

    pub fn len(&self) -> usize {
        self.lens.len()
    }

    pub fn is_empty(&self) -> bool {
        self.lens.is_empty()
    }

    /// `(concat, lens, arrivals_us)` for `core::feed_datagrams`.
    pub fn parts(&self) -> (&[u8], &[u32], &[f64]) {
        (&self.concat, &self.lens, &self.arrivals_us)
    }

    /// Empty the batch, keeping its capacity.
    pub fn clear(&mut self) {
        self.concat.clear();
        self.lens.clear();
        self.arrivals_us.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlushDecision {
    /// Hand the batch to the core now.
    Now,
    /// Arm the fallback timer.
    ArmTimer,
    /// The timer is armed or the tick will come: nothing to do.
    Wait,
}

/// What to do after queueing a datagram. A hidden tab processes at once (rAF
/// is paused and timers are clamped to >= 1 s there), so ACKs stay prompt.
pub fn flush_decision(queued: usize, hidden: bool, timer_armed: bool) -> FlushDecision {
    if queued >= FLUSH_BACKLOG || hidden {
        FlushDecision::Now
    } else if !timer_armed {
        FlushDecision::ArmTimer
    } else {
        FlushDecision::Wait
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_batch_holds_datagrams_back_to_back_with_their_stamps() {
        let mut b = DatagramBatch::default();
        b.push_with(3, 10.5, |d| d.copy_from_slice(&[1, 2, 3]));
        b.push_with(2, 11.0, |d| d.copy_from_slice(&[4, 5]));
        assert_eq!(b.len(), 2);
        let (c, l, a) = b.parts();
        assert_eq!(
            (c, l, a),
            (&[1, 2, 3, 4, 5][..], &[3, 2][..], &[10.5, 11.0][..])
        );
        b.clear();
        assert!(b.is_empty() && b.parts().0.is_empty());
    }

    #[test]
    fn a_backlog_or_a_hidden_tab_flushes_now() {
        assert_eq!(
            flush_decision(FLUSH_BACKLOG, false, true),
            FlushDecision::Now
        );
        assert_eq!(flush_decision(1, true, false), FlushDecision::Now);
        assert_eq!(flush_decision(1, true, true), FlushDecision::Now);
    }

    #[test]
    fn otherwise_the_first_datagram_arms_the_timer_and_the_rest_wait() {
        assert_eq!(flush_decision(1, false, false), FlushDecision::ArmTimer);
        assert_eq!(
            flush_decision(FLUSH_BACKLOG - 1, false, true),
            FlushDecision::Wait
        );
    }

    #[test]
    fn the_constants_are_the_ones_main_ts_used() {
        assert_eq!((FLUSH_BACKLOG, FLUSH_FALLBACK_MS), (32, 50));
    }
}
