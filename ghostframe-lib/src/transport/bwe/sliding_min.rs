//! A sliding-window minimum in O(1) amortised per sample.
//!
//! The classic monotonic deque: a sample that arrives while a smaller (or
//! equal) one is already inside the window can never be the minimum again
//! (it expires no earlier than the smaller one does), so it is dropped on
//! arrival. The front of the deque is the minimum. Used for the capacity
//! hint (a minimum over a span of time, taken every loop turn) and for the
//! path's minimum round trip.

use std::collections::VecDeque;

#[derive(Debug)]
pub(crate) struct SlidingMin<T, V> {
    /// Times strictly increasing, values strictly increasing.
    deque: VecDeque<(T, V)>,
}

impl<T, V> Default for SlidingMin<T, V> {
    fn default() -> Self {
        Self {
            deque: VecDeque::new(),
        }
    }
}

impl<T: PartialOrd + Copy, V: PartialOrd + Copy> SlidingMin<T, V> {
    /// Add a sample taken at `at` (not earlier than the previous one).
    pub(crate) fn push(&mut self, at: T, value: V) {
        while self.deque.back().is_some_and(|(_, v)| *v >= value) {
            self.deque.pop_back();
        }
        self.deque.push_back((at, value));
    }

    /// Forget samples taken before `cut`.
    pub(crate) fn expire_before(&mut self, cut: T) {
        while self.deque.front().is_some_and(|(t, _)| *t < cut) {
            self.deque.pop_front();
        }
    }

    pub(crate) fn min(&self) -> Option<V> {
        self.deque.front().map(|(_, v)| *v)
    }

    pub(crate) fn clear(&mut self) {
        self.deque.clear();
    }

    /// Samples the deque holds (not the samples pushed: dominated ones are
    /// gone).
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.deque.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Against the naive minimum over the same window, on a pseudo-random
    /// walk with expiry.
    #[test]
    fn it_agrees_with_the_naive_minimum() {
        let mut m = SlidingMin::<u64, u64>::default();
        let mut all: Vec<(u64, u64)> = Vec::new();
        let mut x: u64 = 12345;
        for t in 0..5_000u64 {
            x = x
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let v = (x >> 33) % 1000;
            m.push(t, v);
            all.push((t, v));
            let cut = t.saturating_sub(37);
            m.expire_before(cut);
            let naive = all
                .iter()
                .filter(|(ts, _)| *ts >= cut)
                .map(|(_, v)| *v)
                .min();
            assert_eq!(m.min(), naive, "t={t}");
        }
    }

    #[test]
    fn a_dominated_sample_is_dropped_on_arrival() {
        let mut m = SlidingMin::<u64, u64>::default();
        for t in 0..100 {
            m.push(t, 5);
        }
        assert_eq!(m.len(), 1);
        assert_eq!(m.min(), Some(5));
    }
}
