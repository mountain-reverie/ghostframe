// SPDX-License-Identifier: MIT OR Apache-2.0
//! The repair-status stream's life cycle (FEC v2 rev 6), as plain state.
//!
//! The core emits `PollOutput::RepairStatus` chunks; the first one it ever
//! emits starts with `FEC_REPAIR_STATUS_STREAM_TAG`, which tells the server
//! what this client-opened stream is. The stream is opened lazily, on the
//! first chunk, so a session with nothing to say never opens it. When the
//! open or a write fails the stream is dropped, the next chunk opens a fresh
//! one, and that one is owed the tag (the core sent its only tag on the
//! stream that just died). Mirrors `ClientNet` in `ghostframe-client-net`.
//!
//! This type decides; `streams::RepairStatusStream` does the browser calls.

use ghostframe_protocol::fec::repair_status::FEC_REPAIR_STATUS_STREAM_TAG;

/// Most bytes the pipe holds (queued while opening, or handed to the stream
/// and not yet settled) before new chunks are refused. The same value as
/// `MAX_REPAIR_STATUS_QUEUE` in `ghostframe-client-net/src/lib.rs`, which the
/// native client applies to its queue; that crate is not a dependency of this
/// one (it links quinn), so the number is repeated here.
pub const MAX_REPAIR_STATUS_QUEUE: usize = 1 << 20;

/// What the caller must do next.
#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    Nothing,
    /// Open a stream, then call [`RepairStatusPipe::opened`] or
    /// [`RepairStatusPipe::open_failed`] with this generation.
    Open(u32),
    /// Write these chunks, in order, to the stream of this generation. A
    /// failed write is reported with [`RepairStatusPipe::write_failed`].
    Write(u32, Vec<Vec<u8>>),
}

#[derive(Debug)]
enum State {
    Closed,
    /// Chunks that arrived while the stream was being opened.
    Opening(Vec<Vec<u8>>),
    Open,
}

#[derive(Debug)]
pub struct RepairStatusPipe {
    state: State,
    tag_owed: bool,
    /// Bytes queued or in flight on the current stream.
    queued: usize,
    /// Chunks refused because the cap was reached.
    dropped: u64,
    /// Bumps on every open attempt, so a late failure from a dead stream
    /// cannot kill its successor.
    generation: u32,
}

impl Default for RepairStatusPipe {
    fn default() -> Self {
        RepairStatusPipe {
            state: State::Closed,
            tag_owed: false,
            queued: 0,
            dropped: 0,
            generation: 0,
        }
    }
}

impl RepairStatusPipe {
    /// A chunk from the core.
    pub fn push(&mut self, bytes: Vec<u8>) -> Step {
        // Dropping an old chunk would corrupt the record framing; refuse the
        // new one instead (as the native client does).
        if self.queued + bytes.len() > MAX_REPAIR_STATUS_QUEUE {
            self.dropped += 1;
            return Step::Nothing;
        }
        self.queued += bytes.len();
        match &mut self.state {
            State::Closed => {
                self.generation = self.generation.wrapping_add(1);
                self.state = State::Opening(vec![bytes]);
                Step::Open(self.generation)
            }
            State::Opening(queue) => {
                queue.push(bytes);
                Step::Nothing
            }
            State::Open => Step::Write(self.generation, vec![bytes]),
        }
    }

    /// The stream of `generation` is open: flush what waited, tag first when
    /// one is owed.
    pub fn opened(&mut self, generation: u32) -> Step {
        if generation != self.generation || !matches!(self.state, State::Opening(_)) {
            return Step::Nothing;
        }
        let State::Opening(queue) = std::mem::replace(&mut self.state, State::Open) else {
            return Step::Nothing;
        };
        let mut chunks = queue;
        if self.tag_owed {
            chunks.insert(0, vec![FEC_REPAIR_STATUS_STREAM_TAG]);
            self.queued += 1;
            self.tag_owed = false;
        }
        Step::Write(generation, chunks)
    }

    /// The open failed: what queued is lost, and since the lost chunks may
    /// have held the tag, the next stream is owed one.
    pub fn open_failed(&mut self, generation: u32) {
        if generation == self.generation && matches!(self.state, State::Opening(_)) {
            self.state = State::Closed;
            self.tag_owed = true;
            self.queued = 0;
        }
    }

    /// `len` bytes handed to the stream of `generation` were written or
    /// failed: they no longer count against the cap.
    pub fn settled(&mut self, generation: u32, len: usize) {
        if generation == self.generation {
            self.queued = self.queued.saturating_sub(len);
        }
    }

    /// Chunks refused so far because the cap was reached.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// A write to the stream of `generation` failed. Idempotent: every write
    /// queued on a dead stream reports; only the first one counts.
    pub fn write_failed(&mut self, generation: u32) {
        if generation == self.generation && matches!(self.state, State::Open) {
            self.state = State::Closed;
            self.tag_owed = true;
            self.queued = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TAG: u8 = FEC_REPAIR_STATUS_STREAM_TAG;

    fn chunk(b: u8) -> Vec<u8> {
        vec![b]
    }

    #[test]
    fn nothing_is_opened_until_the_core_has_something_to_say() {
        let mut p = RepairStatusPipe::default();
        assert_eq!(p.push(vec![TAG, 1]), Step::Open(1));
    }

    #[test]
    fn chunks_that_arrive_while_opening_flush_in_order_without_a_new_tag() {
        let mut p = RepairStatusPipe::default();
        assert_eq!(p.push(vec![TAG, 1]), Step::Open(1));
        assert_eq!(p.push(chunk(2)), Step::Nothing);
        assert_eq!(p.push(chunk(3)), Step::Nothing);
        assert_eq!(
            p.opened(1),
            Step::Write(1, vec![vec![TAG, 1], chunk(2), chunk(3)])
        );
        assert_eq!(p.push(chunk(4)), Step::Write(1, vec![chunk(4)]));
    }

    #[test]
    fn a_write_failure_reopens_and_the_new_stream_starts_with_the_tag() {
        let mut p = RepairStatusPipe::default();
        p.push(vec![TAG, 1]);
        p.opened(1);
        p.write_failed(1);
        // Later failures of writes queued on the dead stream change nothing.
        p.write_failed(1);
        assert_eq!(p.push(chunk(7)), Step::Open(2));
        assert_eq!(p.opened(2), Step::Write(2, vec![vec![TAG], chunk(7)]));
        // The tag is paid once.
        assert_eq!(p.push(chunk(8)), Step::Write(2, vec![chunk(8)]));
    }

    #[test]
    fn a_late_failure_from_the_dead_stream_does_not_kill_its_successor() {
        let mut p = RepairStatusPipe::default();
        p.push(vec![TAG, 1]);
        p.opened(1);
        p.write_failed(1);
        p.push(chunk(7));
        p.opened(2);
        p.write_failed(1);
        assert_eq!(p.push(chunk(9)), Step::Write(2, vec![chunk(9)]));
    }

    #[test]
    fn a_failed_open_loses_the_queue_and_owes_the_next_stream_the_tag() {
        let mut p = RepairStatusPipe::default();
        assert_eq!(p.push(vec![TAG, 1]), Step::Open(1));
        p.push(chunk(2));
        p.open_failed(1);
        assert_eq!(p.push(chunk(3)), Step::Open(2));
        assert_eq!(p.opened(2), Step::Write(2, vec![vec![TAG], chunk(3)]));
    }

    #[test]
    fn the_cap_holds_while_opening_and_chunks_after_it_are_dropped() {
        let mut p = RepairStatusPipe::default();
        let big = vec![0u8; MAX_REPAIR_STATUS_QUEUE - 10];
        assert_eq!(p.push(big.clone()), Step::Open(1));
        // 10 bytes of room: a 11-byte chunk is refused, a 10-byte one fits.
        assert_eq!(p.push(vec![1; 11]), Step::Nothing);
        assert_eq!(p.dropped(), 1);
        assert_eq!(p.push(vec![2; 10]), Step::Nothing);
        assert_eq!(p.push(vec![3]), Step::Nothing);
        assert_eq!(p.dropped(), 2);
        // What was accepted is intact and in order; the refused ones are gone.
        assert_eq!(p.opened(1), Step::Write(1, vec![big, vec![2; 10]]));
    }

    #[test]
    fn written_bytes_free_room_and_a_failure_clears_the_count() {
        let mut p = RepairStatusPipe::default();
        p.push(vec![TAG, 1]);
        p.opened(1);
        let big = vec![0u8; MAX_REPAIR_STATUS_QUEUE - 2];
        assert!(matches!(p.push(big.clone()), Step::Write(1, _)));
        assert_eq!(p.push(vec![9]), Step::Nothing); // full: 2 + len == cap
        assert_eq!(p.dropped(), 1);
        p.settled(1, big.len());
        assert!(matches!(p.push(vec![9]), Step::Write(1, _)));
        // A failure empties the queue; late settles from the dead stream do
        // not eat into the next one.
        p.write_failed(1);
        assert_eq!(p.push(big.clone()), Step::Open(2));
        p.settled(1, big.len());
        assert_eq!(p.push(vec![0; 3]), Step::Nothing);
        assert_eq!(p.dropped(), 2);
    }

    #[test]
    fn a_stale_open_result_is_ignored() {
        let mut p = RepairStatusPipe::default();
        p.push(vec![TAG, 1]);
        p.open_failed(1);
        p.push(chunk(2));
        assert_eq!(p.opened(1), Step::Nothing);
        assert_eq!(p.opened(2), Step::Write(2, vec![vec![TAG], chunk(2)]));
    }
}
