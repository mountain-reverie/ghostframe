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
    /// The passes on the wire. All 14 for `generation`; for
    /// `sparse_generation`, only those the content needs, as the server
    /// sends them (the others have an empty payload here).
    sent: Vec<u8>,
    /// Every pass in full, for the model render.
    dense: Vec<Vec<u8>>,
}

fn generation(generation: u8, label: u32, salt: u32) -> Generation {
    Generation {
        generation,
        label,
        payloads: cdf53::encode_passes(&cdf53::forward(&tile_bytes(salt))),
        sent: (0..14).collect(),
        dense: cdf53::encode_passes(&cdf53::forward(&tile_bytes(salt))),
    }
}

/// A flat tile, sent the way the server sends one: only the passes that
/// carry data, so a tile that held other content has planes at the absent
/// indices that only a generation reset clears.
fn sparse_generation(generation: u8, label: u32, level: u8) -> Generation {
    let coefficients = cdf53::forward(&vec![level; 4096]);
    let (_, sent_passes) = cdf53::encode_passes_sparse(&coefficients);
    let mut payloads = vec![Vec::new(); 14];
    let mut sent = Vec::new();
    for (idx, payload) in sent_passes {
        payloads[idx as usize] = payload;
        sent.push(idx);
    }
    Generation {
        generation,
        label,
        payloads,
        sent,
        dense: cdf53::encode_passes(&coefficients),
    }
}

/// The wire bytes of pass `pass` of `g`, carrying transmission id `wire_seq`.
fn datagram(g: &Generation, pass: u8, wire_seq: u32) -> Vec<u8> {
    raw_datagram(
        (TILE_X, TILE_Y),
        Codec::Cdf53,
        g.generation,
        g.label,
        pass,
        &g.payloads[pass as usize],
        wire_seq,
    )
}

fn raw_datagram(
    tile: (u8, u8),
    codec: Codec,
    generation: u8,
    label: u32,
    pass: u8,
    payload: &[u8],
    wire_seq: u32,
) -> Vec<u8> {
    let frags = fragment_tile(
        &TileFragmentInputs {
            frame_seq: label | TILE_DATAGRAM_FLAG,
            tile_x: tile.0,
            tile_y: tile.1,
            codec,
            generation,
            pass,
            timestamp_us: 0,
        },
        payload,
        1200,
    );
    assert_eq!(frags.len(), 1, "one datagram per pass");
    let mut bytes = frags.into_iter().next().unwrap();
    bytes[8..12].copy_from_slice(&wire_seq.to_be_bytes());
    bytes
}

fn model_render(g: &Generation) -> Vec<u8> {
    let refs: Vec<&[u8]> = g.dense.iter().map(|p| p.as_slice()).collect();
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
        self.deliver_bytes(datagram(g, pass, wire_seq), wire_seq)
    }

    /// Deliver an already-built datagram that carries transmission id
    /// `wire_seq`.
    fn deliver_bytes(&mut self, bytes: Vec<u8>, wire_seq: u32) -> (Effect, bool) {
        self.now_us += 1000;
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

/// A genuine datagram whose payload cannot be a CDF 5/3 pass.
fn garbage_datagram(generation: u8, label: u32, pass: u8, wire_seq: u32) -> Vec<u8> {
    raw_datagram(
        (TILE_X, TILE_Y),
        Codec::Cdf53,
        generation,
        label,
        pass,
        &[0xAB; 5],
        wire_seq,
    )
}

fn deliver_all(rig: &mut Rig, g: &Generation, passes: std::ops::Range<u8>) -> Option<Vec<u8>> {
    let mut last = None;
    for p in passes.filter(|p| g.sent.contains(p)) {
        let (e, acked) = rig.deliver(g, p);
        assert!(
            e.accepted && acked,
            "gen {} label {} pass {p}: accepted={} acked={acked}",
            g.generation,
            g.label,
            e.accepted
        );
        last = e.rgba;
    }
    last
}

// ---------------------------------------------------------------------------
// One bad label must not freeze a tile.
// ---------------------------------------------------------------------------

/// A datagram of a different generation whose label is far ahead but whose
/// payload is garbage must not become the tile's label: every genuine later
/// generation would then read as older and never be acknowledged.
#[test]
fn a_garbage_datagram_with_a_far_label_does_not_freeze_the_tile() {
    let g1 = generation(1, 100, 3);
    let g3 = generation(3, 110, 9);
    let mut rig = Rig::new();
    deliver_all(&mut rig, &g1, 0..4);

    let ws = rig.next_wire_seq;
    rig.next_wire_seq += 1;
    let (e, acked) = rig.deliver_bytes(garbage_datagram(2, 1_000_100, 0, ws), ws);
    assert!(!e.accepted && !acked, "garbage must not be accepted");

    let last = deliver_all(&mut rig, &g3, 0..14);
    assert_eq!(last.unwrap(), model_render(&g3));
}

/// The same fault from the other source: a datagram rebuilt from parity of
/// the wrong group is arbitrary bytes, and can parse as a valid pass with a
/// far label. Only the labels off the wire can vouch for a label.
#[test]
fn a_rebuilt_datagram_with_a_far_label_does_not_freeze_the_tile() {
    use ghostframe_protocol::protocol::TileParityEnvelope;
    let g1 = generation(1, 100, 3);
    let g3 = generation(3, 110, 9);
    // Same payload bytes, so the two sources are the same length; a valid
    // pass, so prevalidation cannot be what stops it.
    let far = generation(2, 1_000_100, 3);
    let mut rig = Rig::new();
    deliver_all(&mut rig, &g1, 0..4);

    let (ws_a, ws_b) = (7000u32, 7001u32);
    let a = datagram(&g1, 4, ws_a);
    let b = datagram(&far, 4, ws_b);
    let parity_payload: Vec<u8> = a.iter().zip(&b).map(|(x, y)| x ^ y).collect();
    let mut parity = Vec::new();
    TileParityEnvelope {
        group_first_wire_seq: ws_a,
        k: 2,
        parity_idx: 0,
        source_lens: vec![a.len() as u16; 2],
        parity_payload,
    }
    .encode(&mut parity);
    rig.deliver_bytes(a, ws_a);
    // Completes the group: `b` is rebuilt and replayed.
    let events = rig.core.handle_datagram(&parity, rig.now_us + 1000);
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, Event::TileReady { frame_seq, .. } if *frame_seq == far.label)),
        "a rebuilt datagram labelled 1M frames ahead was accepted"
    );

    let last = deliver_all(&mut rig, &g3, 0..14);
    assert_eq!(last.unwrap(), model_render(&g3));
}

/// With nothing received yet there is no yardstick: the first datagram of a
/// session is believed whatever its label.
#[test]
fn the_first_datagram_of_a_session_is_believed_whatever_its_label() {
    let g = generation(4, 1_000_000_000, 3);
    let mut rig = Rig::new();
    let last = deliver_all(&mut rig, &g, 0..14);
    assert_eq!(last.unwrap(), model_render(&g));
}

/// A tile unchanged for so long that its label is more than 2^29 behind the
/// newest received would, past 2^30, compare as *ahead* and reject its next
/// generation. It expires instead.
#[test]
fn a_held_label_that_has_aged_out_does_not_reject_the_next_generation() {
    let old = generation(2, 100, 3);
    let next_label = 100 + (1u32 << 30) + 10;
    let next = generation(3, next_label, 9);
    let mut rig = Rig::new();
    deliver_all(&mut rig, &old, 0..14);
    // Time passes on other tiles, in steps a serial comparison can follow.
    for (i, label) in [100 + (1u32 << 29), 100 + (1u32 << 30) + 5]
        .into_iter()
        .enumerate()
    {
        let ws = 9000 + i as u32;
        let dg = raw_datagram((0, 0), Codec::Solid, 0, label, 0, &[1, 2, 3, 255], ws);
        rig.deliver_bytes(dg, ws);
    }
    let last = deliver_all(&mut rig, &next, 0..14);
    assert_eq!(last.unwrap(), model_render(&next));
}

// ---------------------------------------------------------------------------
// The same 4-bit generation with a newer label is new content.
// ---------------------------------------------------------------------------

/// A tile returns to CDF 5/3 on the generation number it already holds.
/// Its coefficients must not mix with the old content's, and the new
/// generation's gaps must show to NACK bookkeeping.
#[test]
fn an_aliased_generation_with_a_newer_label_renders_the_new_content_exactly() {
    let old = generation(5, 100, 3);
    // Sparse, so it never sends the passes whose planes the old content
    // left behind in the integrator.
    let new = sparse_generation(5, 5000, 120);
    assert!(new.sent.len() < 14 && new.sent.len() > 1, "{:?}", new.sent);
    let mut rig = Rig::new();
    deliver_all(&mut rig, &old, 0..14);
    assert!(rig.core.cdf53_incomplete_tiles(8).is_empty());

    // Only the first pass of the new content: the tile is incomplete again.
    deliver_all(&mut rig, &new, 0..1);
    let incomplete = rig.core.cdf53_incomplete_tiles(8);
    assert!(
        incomplete.iter().any(|t| (t.0, t.1) == (TILE_X, TILE_Y)),
        "the new generation's missing passes are hidden by the old one's mask"
    );

    let last = deliver_all(&mut rig, &new, 1..14);
    assert_eq!(last.unwrap(), model_render(&new));

    // And a late pass of the old content is stale, though its number matches.
    let (e, acked) = rig.deliver(&old, 7);
    assert!(!e.accepted && !acked, "old content merged into the new");
}

/// CDF 5/3, then ten generations of PalRle/Solid that do not touch the CDF
/// 5/3 entry, then CDF 5/3 again: 11 generations on, which the 4-bit compare
/// reads as 5 behind.
#[test]
fn a_tile_returning_to_cdf53_after_other_codecs_is_accepted() {
    let before = generation(2, 100, 3);
    let after = generation(13, 400, 9);
    let mut rig = Rig::new();
    deliver_all(&mut rig, &before, 0..14);
    for (i, gen) in (3u8..13).enumerate() {
        let ws = 9500 + i as u32;
        let dg = raw_datagram(
            (TILE_X, TILE_Y),
            Codec::Solid,
            gen,
            150 + 10 * i as u32,
            0,
            &[9, 9, 9, 255],
            ws,
        );
        rig.deliver_bytes(dg, ws);
    }
    let last = deliver_all(&mut rig, &after, 0..14);
    assert_eq!(last.unwrap(), model_render(&after));
}

/// `Scheduler::resize` restarts every tile's generation at 0 while the client
/// keeps the state of tiles still in the grid; labels keep growing.
#[test]
fn generations_restarting_at_zero_after_a_resize_are_accepted() {
    let mut rig = Rig::new();
    // Held generation 7; the restart begins at 0 (9 "ahead" mod 16), then
    // restarts again from a held generation 0 on the same number.
    let before = generation(7, 100, 3);
    let restarted = generation(0, 300, 9);
    let restarted_again = generation(0, 700, 11);
    deliver_all(&mut rig, &before, 0..14);
    let last = deliver_all(&mut rig, &restarted, 0..14);
    assert_eq!(last.unwrap(), model_render(&restarted));
    let last = deliver_all(&mut rig, &restarted_again, 0..14);
    assert_eq!(last.unwrap(), model_render(&restarted_again));
}

// Generations in creation order, with random 4-bit values (any relative
// order, any distance, including repeats of a number: the aliased case),
// labels that never decrease (and sometimes repeat across close generations:
// the tie-break), delivered in random order with random late duplicates and
// other codecs' datagrams for the same tile interleaved.
proptest! {
    #![proptest_config(ProptestConfig::with_cases(96))]

    /// For any arrival order: a datagram is accepted exactly when its
    /// generation was not created before the newest one already seen, so no
    /// pass of the newest generation is ever dropped; and the final pixels are
    /// the newest generation's.
    #[test]
    fn newest_generation_always_wins(
        shuffled in Just((0u8..16).collect::<Vec<u8>>()).prop_shuffle(),
        count in 2usize..7,
        label_steps in prop::collection::vec(0u32..40, 7),
        // 0: a fresh number, 1: reuse an earlier generation's number
        alias_picks in prop::collection::vec((any::<bool>(), 0usize..7), 7),
        succ in prop::collection::vec(1u8..4, 7),
        // (generation index, pass) arrivals for the older generations
        older in prop::collection::vec((0usize..6, 0u8..14), 0..30),
        newest_order in Just((0u8..14).collect::<Vec<u8>>()).prop_shuffle(),
        slots in prop::collection::vec(0usize..64, 30),
        dup_picks in prop::collection::vec(0usize..64, 0..10),
        other_codec_at in prop::collection::vec(0usize..64, 0..4),
    ) {
        let mut label = 100u32;
        let mut all: Vec<Generation> = Vec::new();
        for i in 0..count {
            let (generation_no, new_label) = if i == 0 {
                (shuffled[0], label)
            } else {
                let prev = &all[i - 1];
                let step = label_steps[i];
                if step == 0 {
                    // Same label as the previous generation: they were sent
                    // within one server frame, so a few generations apart.
                    (prev.generation.wrapping_add(succ[i]) & 0x0F, prev.label)
                } else {
                    let (alias, which) = alias_picks[i];
                    let g = if alias { all[which % i].generation } else { shuffled[i] };
                    (g, prev.label + step)
                }
            };
            label = new_label;
            all.push(generation(generation_no, label, 20 + i as u32));
        }
        let newest = count - 1;

        // Build the arrival list: (generation index, pass).
        let mut arrivals: Vec<(usize, u8)> = newest_order
            .iter()
            .map(|p| (newest, *p))
            .collect();
        for (n, (gi, p)) in older.iter().enumerate() {
            let gi = gi % newest;
            let at = slots[n % slots.len()] % (arrivals.len() + 1);
            arrivals.insert(at, (gi, *p));
        }
        for pick in &dup_picks {
            let src = arrivals[pick % arrivals.len()];
            let at = (pick * 7) % (arrivals.len() + 1);
            arrivals.insert(at.max(1), src);
        }
        // Positions at which a Solid datagram for the same tile intervenes.
        let solid_at: std::collections::HashSet<usize> =
            other_codec_at.iter().map(|a| a % (arrivals.len() + 1)).collect();

        let mut rig = Rig::new();
        let mut newest_seen = 0usize;
        let mut last_newest_rgba = None;
        for (n, (gi, pass)) in arrivals.iter().enumerate() {
            if solid_at.contains(&n) {
                let ws = 50_000 + n as u32;
                let dg = raw_datagram(
                    (TILE_X, TILE_Y), Codec::Solid, (n % 16) as u8,
                    all[newest_seen].label, 0, &[1, 2, 3, 255], ws,
                );
                rig.deliver_bytes(dg, ws);
            }
            let g = &all[*gi];
            let (effect, acked) = rig.deliver(g, *pass);
            // Created no earlier than the newest generation seen so far.
            let expect = *gi >= newest_seen;
            newest_seen = newest_seen.max(*gi);
            prop_assert_eq!(
                effect.accepted, expect,
                "gen index {} (4-bit {}, label {}) pass {}: accepted={} but newest so far is {} (4-bit {}, label {})",
                gi, g.generation, g.label, pass, effect.accepted,
                newest_seen, all[newest_seen].generation, all[newest_seen].label
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
