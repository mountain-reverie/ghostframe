//! Ordering of CDF 5/3 generations by the datagram's `frame_seq`, not by the
//! 4-bit generation.
//!
//! The 4-bit generation wraps at 16 and its "newer" test only separates
//! generations less than 8 apart. A pass 9..=15 generations late used to read
//! as *newer*: the client rewound the tile to that old generation, then
//! dropped every pass of the real current one as stale, un-acked, leaving the
//! tile on old content. These tests drive `ClientCore::handle_datagram` so
//! both consumers of the rule (coverage bookkeeping and the integrator gate in
//! `reassembly`) are covered.
//!
//! The server labels every datagram of a tile generation with the `frame_seq`
//! of that generation's first datagram, so a generation's label is fixed and
//! later generations carry larger ones. Fixtures follow that.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, PollOutput, TileDelivery};
use ghostframe_protocol::ack::AckBatch;
use ghostframe_protocol::codec::cdf53;
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};
use proptest::prelude::*;

const TILE_X: u8 = 2;
const TILE_Y: u8 = 1;

fn core() -> ClientCore {
    let mut c = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            tile_delivery: TileDelivery::Decoded,
            display: None,
        },
        0,
    );
    while c.poll_transmit(0).is_some() {}
    c
}

fn tile_bytes(salt: u32) -> Vec<u8> {
    (0..4096)
        .map(|i| ((i as u32 * 7 + salt * 31) % 251) as u8)
        .collect()
}

/// One generation of the tile: its 14 pass payloads, generation and label.
#[derive(Clone)]
struct Generation {
    generation: u8,
    label: u32,
    payloads: Vec<Vec<u8>>,
}

fn generation(generation: u8, label: u32, salt: u32) -> Generation {
    Generation {
        generation,
        label,
        payloads: cdf53::encode_passes(&cdf53::forward(&tile_bytes(salt))),
    }
}

/// The wire bytes of pass `pass` of `g`, carrying transmission id `wire_seq`.
fn datagram(g: &Generation, pass: u8, wire_seq: u32) -> Vec<u8> {
    let frags = fragment_tile(
        &TileFragmentInputs {
            frame_seq: g.label | TILE_DATAGRAM_FLAG,
            tile_x: TILE_X,
            tile_y: TILE_Y,
            codec: Codec::Cdf53,
            generation: g.generation,
            pass,
            timestamp_us: 0,
        },
        &g.payloads[pass as usize],
        1200,
    );
    assert_eq!(frags.len(), 1, "one datagram per pass");
    let mut bytes = frags.into_iter().next().unwrap();
    bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
    bytes
}

fn model_render(g: &Generation) -> Vec<u8> {
    let refs: Vec<&[u8]> = g.payloads.iter().map(|p| p.as_slice()).collect();
    let bgr = cdf53::inverse(&cdf53::decode_passes(&refs));
    let mut rgba = vec![255u8; 4096];
    for px in 0..1024 {
        rgba[px * 4] = bgr[px * 3 + 2];
        rgba[px * 4 + 1] = bgr[px * 3 + 1];
        rgba[px * 4 + 2] = bgr[px * 3];
    }
    rgba
}

/// What one delivered datagram did.
struct Effect {
    /// The pass reached the integrator (`TileReady`).
    accepted: bool,
    /// Pixels of that `TileReady`, if any.
    rgba: Option<Vec<u8>>,
}

struct Rig {
    core: ClientCore,
    now_us: u64,
    next_wire_seq: u32,
}

impl Rig {
    fn new() -> Self {
        Self {
            core: core(),
            now_us: 0,
            next_wire_seq: 1000,
        }
    }

    /// Deliver pass `pass` of `g` as a fresh transmission and report whether
    /// it was accepted and acknowledged.
    fn deliver(&mut self, g: &Generation, pass: u8) -> (Effect, bool) {
        let wire_seq = self.next_wire_seq;
        self.next_wire_seq += 1;
        self.now_us += 1000;
        let bytes = datagram(g, pass, wire_seq);
        let events = self.core.handle_datagram(&bytes, self.now_us);
        let mut effect = Effect {
            accepted: false,
            rgba: None,
        };
        for ev in events {
            if let Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                ..
            } = ev
            {
                if (tile_x, tile_y) == (TILE_X, TILE_Y) {
                    effect.accepted = true;
                    effect.rgba = Some(rgba);
                }
            }
        }
        // Flush the ACK batcher and look for this transmission.
        self.now_us += 100_000;
        let _ = self.core.on_timeout(self.now_us);
        let mut acked = false;
        while let Some(out) = self.core.poll_transmit(self.now_us) {
            if let PollOutput::Datagram(dg) = out {
                if let Ok(batch) = AckBatch::decode(&dg) {
                    acked |= batch.entries.iter().any(|e| e.wire_seq == wire_seq);
                }
            }
        }
        (effect, acked)
    }
}

/// A pass `k` generations old, for every `k` the 4-bit compare gets wrong
/// (9..=15) and for those it gets right, arriving after the current
/// generation began. The current generation's passes must all still land.
#[test]
fn a_late_pass_of_an_old_generation_does_not_replace_the_current_one() {
    for k in 1u8..=15 {
        let old = generation(0, 10, 3);
        let cur = generation(k, 10 + u32::from(k), 9);
        let mut rig = Rig::new();

        // The old generation is held, then the current one starts.
        for p in 0..3 {
            let (e, acked) = rig.deliver(&old, p);
            assert!(e.accepted && acked, "k={k}: old gen pass {p} first arrival");
        }
        for p in 0..4 {
            let (e, acked) = rig.deliver(&cur, p);
            assert!(e.accepted && acked, "k={k}: current gen pass {p}");
        }
        // The late old pass.
        let (e, acked) = rig.deliver(&old, 12);
        assert!(
            !e.accepted,
            "k={k}: a {k}-generations-old pass was accepted"
        );
        assert!(!acked, "k={k}: a stale pass was acknowledged");
        // Everything else of the current generation must still be accepted
        // and acknowledged, and render the current content.
        let mut last = None;
        for p in 4..14 {
            let (e, acked) = rig.deliver(&cur, p);
            assert!(
                e.accepted && acked,
                "k={k}: current gen pass {p} dropped after a late old pass \
                 (accepted={}, acked={acked})",
                e.accepted
            );
            last = e.rgba;
        }
        assert_eq!(last.unwrap(), model_render(&cur), "k={k}: wrong content");
    }
}

/// A retransmission of the current generation -- same label, a later
/// transmission -- is accepted and acknowledged, even after the generation's
/// other passes and after a late pass of an older one.
#[test]
fn a_retransmission_of_the_current_generation_is_accepted() {
    let old = generation(3, 10, 3);
    let cur = generation(14, 40, 9); // 11 ahead: 4-bit compare says "older"
    let mut rig = Rig::new();
    for p in 0..14 {
        rig.deliver(&cur, p);
    }
    let (e, _) = rig.deliver(&old, 5);
    assert!(!e.accepted);
    for p in [0u8, 5, 13] {
        let (e, acked) = rig.deliver(&cur, p);
        assert!(e.accepted && acked, "retransmitted pass {p} dropped");
    }
}

// Generations in label order, with random 4-bit values (any relative order,
// any distance), delivered in random order with random late duplicates.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// For any arrival order: a datagram is accepted exactly when its
    /// generation is not older than the newest one already seen, so no pass of
    /// the newest generation is ever dropped; and the final pixels are the
    /// newest generation's.
    #[test]
    fn newest_generation_always_wins(
        gens in Just((0u8..16).collect::<Vec<u8>>()).prop_shuffle(),
        count in 2usize..6,
        label_steps in prop::collection::vec(1u32..40, 6),
        // (generation index, pass) arrivals for the older generations
        older in prop::collection::vec((0usize..5, 0u8..14), 0..30),
        newest_order in Just((0u8..14).collect::<Vec<u8>>()).prop_shuffle(),
        slots in prop::collection::vec(0usize..64, 30),
        dup_picks in prop::collection::vec(0usize..64, 0..10),
    ) {
        let mut label = 100u32;
        let all: Vec<Generation> = (0..count)
            .map(|i| {
                label += label_steps[i];
                generation(gens[i], label, 20 + i as u32)
            })
            .collect();
        let newest = count - 1;

        // Build the arrival list: (generation index, pass).
        let mut arrivals: Vec<(usize, u8)> = newest_order
            .iter()
            .map(|p| (newest, *p))
            .collect();
        // Insert each older arrival at a random position.
        for (n, (gi, p)) in older.iter().enumerate() {
            let gi = gi % newest;
            let at = slots[n % slots.len()] % (arrivals.len() + 1);
            arrivals.insert(at, (gi, *p));
        }
        // Late duplicates of anything already in the list.
        for pick in &dup_picks {
            let src = arrivals[pick % arrivals.len()];
            let at = (pick * 7) % (arrivals.len() + 1);
            arrivals.insert(at.max(1), src);
        }

        let mut rig = Rig::new();
        let mut newest_seen_label = 0u32;
        let mut last_newest_rgba = None;
        for (gi, pass) in &arrivals {
            let g = &all[*gi];
            let (effect, acked) = rig.deliver(g, *pass);
            // Newest-by-label seen so far, including this datagram.
            let expect = g.label >= newest_seen_label;
            newest_seen_label = newest_seen_label.max(g.label);
            prop_assert_eq!(
                effect.accepted, expect,
                "gen index {} (4-bit {}) pass {}: accepted={} but newest-so-far label {}",
                gi, g.generation, pass, effect.accepted, newest_seen_label
            );
            prop_assert_eq!(acked, expect, "ack disagrees with acceptance");
            if *gi == newest {
                last_newest_rgba = effect.rgba;
            }
        }
        let want = model_render(&all[newest]);
        prop_assert_eq!(last_newest_rgba.expect("newest rendered"), want);
    }
}
