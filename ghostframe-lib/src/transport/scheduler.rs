//! Round-robin tile-work scheduler with batched-ACK driven retry.
//!
//! Per M3.1 umbrella, this is the single point of dispatch for every tile-codec
//! emission. M3.1 ships single-pass codecs only; the `pass_idx`/`total_passes`
//! fields are reserved for M3.3 progressive refinement.

use crate::transport::protocol::Codec;
use std::collections::VecDeque;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkState {
    Pending,
    InFlight,
    Acked,
    Superseded,
}

#[derive(Debug, Clone)]
pub struct TileWork {
    pub tile_x: u8,
    pub tile_y: u8,
    pub generation: u8,    // 4 bits effective
    pub pass_idx: u8,      // 4 bits effective; 0 for single-pass codecs
    pub total_passes: u8,
    pub codec: Codec,
    pub payload: Vec<u8>,
    pub queued_at: Instant,
    pub last_sent_at: Option<Instant>,
    pub state: WorkState,
}

impl TileWork {
    /// Test-only constructor that fills timing fields with `Instant::now()`.
    #[cfg(test)]
    pub fn raw_for_test(tile_x: u8, tile_y: u8, generation: u8, payload: Vec<u8>) -> Self {
        Self {
            tile_x,
            tile_y,
            generation,
            pass_idx: 0,
            total_passes: 1,
            codec: Codec::Raw,
            payload,
            queued_at: Instant::now(),
            last_sent_at: None,
            state: WorkState::Pending,
        }
    }
}

pub struct Scheduler {
    cols: u32,
    rows: u32,
    /// Per-tile generation counter, indexed `y * cols + x`. 4-bit wrap.
    generations: Vec<u8>,
    /// FIFO of pending/in-flight tile work. M3.1 is single-pass so FIFO is
    /// equivalent to round-robin; M3.3 will revisit this when multi-pass
    /// refinement starts mixing fresh work with refinement passes.
    queue: VecDeque<TileWork>,
    /// QUIC RTT estimate used to drive 2×RTT retry. Updated by `set_rtt`.
    rtt: Duration,
    /// Reserved for M3.3 refinement bandwidth partitioning. Unused in M3.1.
    #[allow(dead_code)]
    refinement_fraction: f32,
}

impl Scheduler {
    pub fn new(cols: u32, rows: u32) -> Self {
        Self {
            cols,
            rows,
            generations: vec![0; (cols as usize) * (rows as usize)],
            queue: VecDeque::new(),
            rtt: Duration::from_millis(20),
            refinement_fraction: 0.2,
        }
    }

    pub fn resize(&mut self, cols: u32, rows: u32) {
        self.cols = cols;
        self.rows = rows;
        self.generations = vec![0; (cols as usize) * (rows as usize)];
        self.queue.clear();
    }

    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    pub fn cols(&self) -> u32 { self.cols }
    pub fn rows(&self) -> u32 { self.rows }
    pub fn queue_len(&self) -> usize { self.queue.len() }

    pub fn generation_for(&self, tile_x: u8, tile_y: u8) -> u8 {
        let idx = (tile_y as usize) * (self.cols as usize) + (tile_x as usize);
        self.generations.get(idx).copied().unwrap_or(0)
    }

    /// Stub — replaced with real impl in Task 6 (this task only needs
    /// it to compile so the resize test can verify queue clearing).
    pub fn enqueue(&mut self, work: TileWork) {
        self.queue.push_back(work);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sized_scheduler_has_empty_queue_and_zeroed_generations() {
        let s = Scheduler::new(5, 4);
        assert_eq!(s.queue_len(), 0);
        assert_eq!(s.generation_for(2, 1), 0);
        assert_eq!(s.cols(), 5);
        assert_eq!(s.rows(), 4);
    }

    #[test]
    fn resize_clears_state() {
        let mut s = Scheduler::new(2, 2);
        s.enqueue(TileWork::raw_for_test(0, 0, 0, vec![1, 2, 3]));
        assert_eq!(s.queue_len(), 1);
        s.resize(8, 6);
        assert_eq!(s.queue_len(), 0);
        assert_eq!(s.generation_for(7, 5), 0);
    }
}
