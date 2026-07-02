//! Simulation harness: drive the emitter with deterministic loss
//! injection and a virtual clock. See spec §10.3.

#![cfg(test)]

use super::emitter::ReliableTileEmitter;
use super::traits::DatagramSender;
use super::*;
use bytes::Bytes;
use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// Deterministic RNG (linear-congruential) for reproducible loss patterns.
struct DetRng(u64);
impl DetRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }
    fn next_u32(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 32) as u32
    }
    fn bool(&mut self, p: f64) -> bool {
        let threshold = (p * (u32::MAX as f64)) as u32;
        self.next_u32() < threshold
    }
}

/// Sender that drops each datagram with probability `p`.
struct LossSender {
    delivered: Rc<RefCell<Vec<Vec<u8>>>>,
    rng: DetRng,
    p: f64,
}
impl DatagramSender for LossSender {
    fn send(&mut self, dg: &[u8]) {
        if self.rng.bool(self.p) {
            return;
        }
        self.delivered.borrow_mut().push(dg.to_vec());
    }
}

fn fake_source(seq: u32) -> Bytes {
    let mut v = vec![0u8; 25];
    let fs = 0x8000_0000 | seq;
    v[0..4].copy_from_slice(&fs.to_be_bytes());
    v[4..6].copy_from_slice(&0u16.to_be_bytes());
    v[6..8].copy_from_slice(&1u16.to_be_bytes());
    v[12..16].copy_from_slice(&0u32.to_be_bytes());
    Bytes::from(v)
}

/// Decode the EmitKey from a tile datagram's header bytes. Returns None
/// for parity envelopes (which have a different first byte).
fn key_from_datagram(dg: &[u8]) -> Option<EmitKey> {
    if dg.len() < 20 {
        return None;
    }
    let fs = u32::from_be_bytes(dg[0..4].try_into().ok()?);
    // Parity envelopes start with 0x04 in the first byte; tile datagrams
    // carry the TILE_DATAGRAM_FLAG (bit 31) and have frame_seq otherwise.
    if fs & 0x8000_0000 == 0 {
        return None;
    }
    let frame_seq = fs & 0x7FFF_FFFF;
    let tile_x = dg[16];
    let tile_y = dg[17];
    let pass_idx = dg[19] & 0x0F;
    Some(EmitKey::new(frame_seq, tile_x, tile_y, pass_idx))
}

struct Sim {
    emitter: ReliableTileEmitter,
    delivered: Rc<RefCell<Vec<Vec<u8>>>>,
    sender: LossSender,
    t: Instant,
}
impl Sim {
    fn new(loss_p: f64, seed: u64) -> Self {
        let delivered = Rc::new(RefCell::new(Vec::new()));
        Self {
            emitter: ReliableTileEmitter::new(),
            sender: LossSender {
                delivered: delivered.clone(),
                rng: DetRng::new(seed),
                p: loss_p,
            },
            delivered,
            t: Instant::now(),
        }
    }
    /// Drain freshly-delivered datagrams from the loss sender and feed
    /// matching ACKs back to the emitter — models a perfect-RTT client
    /// that ACKs every received tile-pass. Without this feedback loop the
    /// RTO wheel would fire spurious retransmits even on a clean wire.
    fn ack_delivered(&mut self) {
        let mut delivered = self.delivered.borrow_mut();
        let keys: Vec<EmitKey> = delivered
            .iter()
            .filter_map(|dg| key_from_datagram(dg))
            .collect();
        delivered.clear();
        drop(delivered);
        if !keys.is_empty() {
            self.emitter.on_ack(&keys);
        }
    }
    fn submit_many(&mut self, n: u32) {
        for i in 0..n {
            self.emitter
                .submit_one(EmitKey::new(i, 0, 0, 0), fake_source(i), self.t);
        }
        self.emitter.drain(&mut self.sender, self.t);
        self.ack_delivered();
    }
    fn advance(&mut self, dt: Duration) {
        self.t += dt;
        self.emitter.tick(self.t, usize::MAX);
        self.emitter.drain(&mut self.sender, self.t);
        self.ack_delivered();
    }
}

#[test]
fn sim_clean_wire_no_retransmits() {
    let mut sim = Sim::new(0.0, 42);
    sim.submit_many(1000);
    for _ in 0..5 {
        sim.advance(Duration::from_millis(100));
    }
    assert_eq!(sim.emitter.stats.rto_fired, 0);
    assert_eq!(sim.emitter.stats.rto_max_retransmits_reached, 0);
}

#[test]
fn sim_low_loss_5pct() {
    let mut sim = Sim::new(0.05, 1);
    sim.submit_many(1000);
    for _ in 0..10 {
        sim.advance(Duration::from_millis(100));
    }
    // At 5% loss with FEC + retransmit, near-100% delivery within a
    // few RTO windows. The emitter doesn't track delivered count; we
    // approximate it via cache.len() (entries still pending) being small.
    assert!(
        sim.emitter.cache.len() < 50,
        "most entries should be drained"
    );
}

#[test]
fn sim_high_loss_50pct() {
    let mut sim = Sim::new(0.5, 2);
    sim.submit_many(1000);
    for _ in 0..20 {
        sim.advance(Duration::from_millis(200));
    }
    // Retirement was removed — rto_max_retransmits_reached must stay 0.
    assert_eq!(
        sim.emitter.stats.rto_max_retransmits_reached, 0,
        "got {}",
        sim.emitter.stats.rto_max_retransmits_reached
    );
}

#[test]
fn sim_generation_churn_no_orphan_retransmits() {
    let mut sim = Sim::new(0.3, 3);
    for i in 0..100u32 {
        sim.emitter
            .submit_one(EmitKey::new(i, 5, 5, 0), fake_source(i), sim.t);
        sim.emitter.drain(&mut sim.sender, sim.t);
        // Every 10 submissions bump tile (5,5).
        if i % 10 == 9 {
            sim.emitter.cancel_for_tile(5, 5);
        }
    }
    // Run RTOs.
    for _ in 0..10 {
        sim.advance(Duration::from_millis(200));
    }
    assert_eq!(
        sim.emitter.cache.stats.lru_eviction, 0,
        "no LRU pressure under generation churn"
    );
}
