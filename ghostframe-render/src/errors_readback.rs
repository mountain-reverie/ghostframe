// SPDX-License-Identifier: MIT OR Apache-2.0
//! The non-blocking PalRLE errors readback: one staging buffer, at most one
//! readback holding it, results kept until taken.
//!
//! The rule it exists for (from the TypeScript renderer it replaces): WebGPU
//! rejects a `queue.submit` that uses a buffer which is not unmapped. In the
//! TypeScript the errors copy rode in the tick's one encoder, so the rejection
//! took the tick's Solid draws, CDF 5/3 work and present with it. Here the copy
//! is a submit of its own, so what is lost is that batch's errors plus an
//! uncaptured validation error; either way the copy happens only if
//! [`ErrorsReadback::try_begin`] says the staging buffer is free, and the
//! buffer is unmapped before [`ErrorsReadback::complete`] publishes.

use std::sync::{Arc, Mutex, MutexGuard};

/// One finished readback: `(tile_x, tile_y, code)` for every non-zero slot.
pub type Found = Vec<(u8, u8, u32)>;

enum State {
    Idle,
    /// A copy was submitted and `map_async` is pending or mapped.
    InFlight(Vec<(u8, u8)>),
    /// The staging buffer is unmapped again; the result waits for `take`.
    Ready(Result<Found, String>),
}

#[derive(Clone)]
pub struct ErrorsReadback {
    state: Arc<Mutex<State>>,
}

impl Default for ErrorsReadback {
    fn default() -> Self {
        Self::new()
    }
}

fn lock(m: &Mutex<State>) -> MutexGuard<'_, State> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl ErrorsReadback {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(State::Idle)),
        }
    }

    /// Claim the staging buffer for a readback of the batch `tiles` (slot `i`
    /// belongs to `tiles[i]`). `false` when a readback is in flight **or its
    /// result has not been taken**: the caller must not copy into the staging
    /// buffer, and the batch's errors go unreported (as they did before).
    pub fn try_begin(&self, tiles: Vec<(u8, u8)>) -> bool {
        let mut s = lock(&self.state);
        match *s {
            State::Idle => {
                *s = State::InFlight(tiles);
                true
            }
            State::InFlight(_) | State::Ready(_) => false,
        }
    }

    /// Whether a readback holds the staging buffer **or has a result nobody
    /// has taken yet**. The idle skip must treat both as work: once the map
    /// callback has run, the result sits in `Ready` and only a `take` (from
    /// `TileRenderer::take_decode_errors`) reports it; a screen that stopped
    /// changing would otherwise never report its error.
    pub fn pending(&self) -> bool {
        matches!(*lock(&self.state), State::InFlight(_) | State::Ready(_))
    }

    /// Called by the map callback **after** the staging buffer is unmapped.
    /// `words` are the first 32-bit word of each error slot, in batch order.
    pub fn complete(&self, words: Result<Vec<u32>, String>) {
        let mut s = lock(&self.state);
        let tiles = match std::mem::replace(&mut *s, State::Idle) {
            State::InFlight(t) => t,
            // A completion with nothing in flight is a bug in the caller; keep
            // whatever is there rather than invent a result.
            other @ (State::Idle | State::Ready(_)) => {
                *s = other;
                return;
            }
        };
        *s = State::Ready(words.map(|w| {
            w.iter()
                .zip(tiles.iter())
                .filter(|(code, _)| **code != 0)
                .map(|(code, (x, y))| (*x, *y, *code))
                .collect()
        }));
    }

    /// The finished readback, if any; frees the staging buffer for the next.
    pub fn take(&self) -> Option<Result<Found, String>> {
        let mut s = lock(&self.state);
        match std::mem::replace(&mut *s, State::Idle) {
            State::Ready(r) => Some(r),
            other @ (State::Idle | State::InFlight(_)) => {
                *s = other;
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_readback_cannot_start_while_one_holds_the_staging_buffer() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(0, 0)]));
        assert!(rb.pending());
        assert!(!rb.try_begin(vec![(1, 1)]), "staging buffer is held");
    }

    #[test]
    fn a_finished_result_is_kept_until_taken_and_blocks_the_next_begin() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(2, 3), (4, 5)]));
        rb.complete(Ok(vec![0, 5]));
        assert!(
            rb.pending(),
            "a result nobody has taken is still pending: the idle skip must not hide it"
        );
        assert!(
            !rb.try_begin(vec![(9, 9)]),
            "an untaken result must not be overwritten"
        );
        assert_eq!(rb.take(), Some(Ok(vec![(4, 5, 5)])));
        assert_eq!(rb.take(), None);
        assert!(!rb.pending(), "taking ends it");
        assert!(rb.try_begin(vec![(9, 9)]), "taking frees the buffer");
    }

    #[test]
    fn zero_slots_and_slots_past_the_batch_are_ignored() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(1, 1)]));
        rb.complete(Ok(vec![0, 7, 7, 7]));
        assert_eq!(rb.take(), Some(Ok(vec![])));
    }

    #[test]
    fn a_failed_map_is_reported_and_frees_the_buffer() {
        let rb = ErrorsReadback::new();
        assert!(rb.try_begin(vec![(0, 0)]));
        rb.complete(Err("device lost".into()));
        assert_eq!(rb.take(), Some(Err("device lost".to_string())));
        assert!(rb.try_begin(vec![(0, 0)]));
    }

    #[test]
    fn a_stray_completion_changes_nothing() {
        let rb = ErrorsReadback::new();
        rb.complete(Ok(vec![1]));
        assert_eq!(rb.take(), None);
        assert!(rb.try_begin(vec![(0, 0)]));
    }
}
