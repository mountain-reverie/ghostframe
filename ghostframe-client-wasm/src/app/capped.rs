// SPDX-License-Identifier: MIT OR Apache-2.0
//! The trimming rule of the e2e logs (`appendCapped` in the old
//! `diagnostics.ts`): a log keeps its most recent `cap` entries, and is trimmed
//! back to `cap` only once it reaches twice that, so a reader sees between
//! `cap` and `2 * cap` entries, newest last. Never one-out-one-in: dropping the
//! head of a 32,768-element JS array costs V8 a move of the other 32,767
//! (64 us a call, measured), which once stalled the datagram loop for seconds.

/// Entries every e2e log keeps (sized for the busiest e2e: ~80 dirty tiles x
/// 30 fps x ~5 s settle, plus headroom).
pub const MAX_RECORDED_ENTRIES: usize = 32_768;

/// The length of a log, tracked on the Rust side so the JS array's `length`
/// is never read back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CappedCount {
    len: usize,
    cap: usize,
}

impl CappedCount {
    pub fn new(cap: usize) -> Self {
        CappedCount {
            len: 0,
            cap: cap.max(1),
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// One entry was appended. `Some(n)`: remove the `n` oldest entries now
    /// (the log reached twice its cap); the length is back to `cap`.
    pub fn push(&mut self) -> Option<usize> {
        self.len += 1;
        if self.len >= 2 * self.cap {
            let drop = self.len - self.cap;
            self.len = self.cap;
            Some(drop)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_between_cap_and_twice_cap_newest_last() {
        // diagnostics.test.ts "keeps the newest entries, between cap and twice cap".
        let mut c = CappedCount::new(4);
        let mut log: Vec<u32> = Vec::new();
        for i in 0..20 {
            log.push(i);
            if let Some(n) = c.push() {
                log.drain(..n);
            }
            assert!(c.len() == log.len() && log.len() < 8, "{log:?}");
        }
        assert!(log.len() >= 4);
        assert_eq!(*log.last().unwrap(), 19);
        assert_eq!(log, (20 - log.len() as u32..20).collect::<Vec<_>>());
    }

    #[test]
    fn trims_rarely_not_per_entry() {
        // "costs the same per entry at the production cap as at a small one":
        // one trim per `cap` pushes once full, never one per push.
        let mut c = CappedCount::new(MAX_RECORDED_ENTRIES);
        let trims = (0..10 * MAX_RECORDED_ENTRIES)
            .filter(|_| c.push().is_some())
            .count();
        assert_eq!(trims, 9);
    }
}
