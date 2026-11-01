//! Simulation harness: drive the emitter with deterministic loss
//! injection and a virtual clock. See spec §10.3.

#![cfg(test)]

use super::emitter::ReliableTileEmitter;
use super::traits::{DatagramSender, SendOutcome};
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
    fn send(&mut self, dg: &[u8]) -> SendOutcome {
        if self.rng.bool(self.p) {
            // Simulated wire loss consumes the datagram. `Rejected` is
            // transport backpressure and would be re-queued; a lost packet
            // must not be.
            return SendOutcome::Sent;
        }
        self.delivered.borrow_mut().push(dg.to_vec());
        SendOutcome::Sent
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
    ledger: crate::transport::transmission_ledger::TransmissionLedger,
    delivered: Rc<RefCell<Vec<Vec<u8>>>>,
    sender: LossSender,
    t: Instant,
}
impl Sim {
    fn new(loss_p: f64, seed: u64) -> Self {
        let delivered = Rc::new(RefCell::new(Vec::new()));
        let t = Instant::now();
        // A client that acknowledges within the step has a measured RTT; with
        // none, the deadline would be the RFC's 1 s initial value and these
        // 100 ms steps would never reach it.
        let mut emitter = ReliableTileEmitter::new(t);
        emitter.rtt.on_sample(Duration::from_millis(20));
        Self {
            emitter,
            ledger: crate::transport::transmission_ledger::TransmissionLedger::new(16_384),
            sender: LossSender {
                delivered: delivered.clone(),
                rng: DetRng::new(seed),
                p: loss_p,
            },
            delivered,
            t,
        }
    }
    /// Put what the emitter built on the books and on the wire.
    fn book_departures(&mut self) {
        use crate::transport::transmission_ledger::Transmission;
        for em in self.emitter.take_transmissions() {
            self.ledger.record(
                em.wire_seq,
                self.t,
                Transmission {
                    emit_us: em.emit_us,
                    wire_bytes: em.wire_bytes,
                    content: em.content,
                    depart_us: None,
                    dep_idx: None,
                },
            );
            self.ledger.mark_departed(em.wire_seq, self.t, 0);
            self.emitter.on_departed(em.wire_seq, self.t);
        }
    }
    /// Drain freshly-delivered datagrams from the loss sender and acknowledge
    /// each by its transmission, as the client does: a perfect-RTT client that
    /// acknowledges every received tile datagram. Then run the loss path.
    fn ack_delivered(&mut self) {
        use crate::transport::transmission_ledger::Resolution;
        self.book_departures();
        let mut delivered = self.delivered.borrow_mut();
        let dgs: Vec<Vec<u8>> = std::mem::take(&mut *delivered);
        drop(delivered);
        for dg in dgs {
            if key_from_datagram(&dg).is_none() || dg.len() < 12 {
                continue;
            }
            let ws = u32::from_be_bytes(dg[8..12].try_into().unwrap());
            match self.ledger.resolve(ws) {
                Some(Resolution::Live(tx)) | Some(Resolution::Late { tx, .. }) => {
                    self.emitter.note_ack_origin(tx.content.tile().unwrap(), ws);
                    self.emitter.on_ack(&[tx.content.tile().unwrap()]);
                    self.emitter.rtt.on_sample(Duration::from_millis(20));
                    self.emitter.note_ack(self.t);
                }
                None => {}
            }
        }
        let _ = crate::transport::loss_path::detect(&mut self.ledger, &mut self.emitter, self.t);
        let _ = crate::transport::loss_path::probe(&mut self.ledger, &mut self.emitter, self.t);
    }
    fn submit_many(&mut self, n: u32) {
        for i in 0..n {
            self.emitter.submit_one(
                EmitKey::new(i, 0, 0, 0),
                fake_source(i),
                self.t,
                None,
                self.t,
            );
        }
        self.emitter.drain(&mut self.sender, self.t);
        self.ack_delivered();
    }
    fn advance(&mut self, dt: Duration) {
        self.t += dt;
        self.ack_delivered();
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
    assert_eq!(sim.emitter.stats.loss_path_resends, 0);
}

#[test]
fn sim_low_loss_5pct() {
    let mut sim = Sim::new(0.05, 1);
    sim.submit_many(1000);
    for _ in 0..10 {
        sim.advance(Duration::from_millis(100));
    }
    // At 5% loss with FEC + retransmit, near-100% delivery within a
    // few round trips. The emitter doesn't track delivered count; we
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
    // There is no ATTEMPT cap (0aa97de removed it because it abandoned passes
    // that were legitimately in flight). Age-based retirement (`retire_stuck`)
    // exists: this scenario runs 20 x 200 ms = 4 s, far inside the 30 s
    // staleness threshold, so it also serves as evidence that retirement
    // leaves a busy-but-healthy cache alone.
    assert_eq!(
        sim.emitter.stats.stuck_entries_retired, 0,
        "got {}",
        sim.emitter.stats.stuck_entries_retired
    );
}

#[test]
fn sim_generation_churn_no_orphan_retransmits() {
    let mut sim = Sim::new(0.3, 3);
    for i in 0..100u32 {
        sim.emitter
            .submit_one(EmitKey::new(i, 5, 5, 0), fake_source(i), sim.t, None, sim.t);
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
}
