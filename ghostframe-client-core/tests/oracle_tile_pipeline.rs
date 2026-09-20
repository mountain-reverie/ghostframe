//! A model oracle and metamorphic properties for the **whole client tile
//! pipeline** — `ClientCore::handle_datagram` in, `Event::TileReady` out.
//!
//! # Why this layer, and not a lower one
//!
//! `cdf53_tile_state.rs` already oracles CDF 5/3 accumulation against
//! `cdf53::decode_passes`, and it passed throughout the period in which
//! tiles were visibly rendering wrong. It could not have failed: the defect
//! was never in pass accumulation. It was in the *bookkeeping above* it —
//! `cdf53_coverage::apply_cdf53_arrival` and `reassembly`, which decide
//! which generation a datagram belongs to and whether to reset state.
//!
//! A stale pass from a superseded generation, arriving between two passes of
//! the current one, rewound that state and permanently lost the current
//! generation's pass 0. Every unit test below this layer was green. The
//! lesson is that an oracle only covers the layer it is pointed at, so this
//! file points one at the top.
//!
//! # The model
//!
//! `model_render` is a deliberately naive reference: it ignores arrival
//! order entirely, keeps only the newest generation seen per tile, and
//! decodes that generation's passes in index order at the end. It shares no
//! code with the incremental path it checks — the only thing the two have in
//! common is `cdf53::decode_passes`, which the lower-layer oracle already
//! pins independently.
//!
//! # The metamorphic relations
//!
//! Each property below is a transformation of the *delivery schedule* that
//! must not change the rendered pixels:
//!
//! - **order** — a permutation of one generation's passes
//! - **duplication** — redelivering datagrams already seen
//! - **staleness** — interleaving datagrams from a superseded generation
//!
//! The third is the one that was broken.

use ghostframe_client_core::{ClientConfig, ClientCore, Event, TileDelivery};
use ghostframe_protocol::codec::cdf53;
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};
use proptest::prelude::*;

const TILE_X: u8 = 2;
const TILE_Y: u8 = 1;

/// One datagram, tagged with what it *is* so a schedule stays readable when
/// a property shrinks to a minimal failing case.
#[derive(Debug, Clone)]
struct Dg {
    generation: u8,
    pass_idx: u8,
    bytes: Vec<u8>,
}

fn core() -> ClientCore {
    let mut c = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            tile_delivery: TileDelivery::Decoded,
        },
        0,
    );
    while c.poll_transmit(0).is_some() {}
    c
}

/// A 32x32 BGRA tile whose content varies with `salt`, so two generations
/// carry genuinely different pixels and a test cannot pass by rendering the
/// wrong one.
fn tile_bytes(salt: u32) -> Vec<u8> {
    (0..4096)
        .map(|i| ((i as u32 * 7 + salt * 31) % 251) as u8)
        .collect()
}

/// Every pass of one generation of one tile, as single-fragment datagrams.
/// Cdf53 pass payloads measured 9-216 bytes, well inside one datagram at
/// this MTU, so each pass is exactly one `Dg` and a schedule is a
/// permutation of datagrams rather than of fragments.
fn passes_for(frame_seq: u32, generation: u8, salt: u32) -> (Vec<Dg>, Vec<Vec<u8>>) {
    let coeffs = cdf53::forward(&tile_bytes(salt));
    let payloads = cdf53::encode_passes(&coeffs);
    let dgs = payloads
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let frags = fragment_tile(
                &TileFragmentInputs {
                    frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
                    tile_x: TILE_X,
                    tile_y: TILE_Y,
                    codec: Codec::Cdf53,
                    generation,
                    pass: i as u8,
                    timestamp_us: 0,
                },
                p,
                1200,
            );
            assert_eq!(
                frags.len(),
                1,
                "pass {i} fragmented into {} datagrams; this file's schedules \
                 assume one datagram per pass",
                frags.len()
            );
            Dg {
                generation,
                pass_idx: i as u8,
                bytes: frags.into_iter().next().unwrap(),
            }
        })
        .collect();
    (dgs, payloads)
}

/// The reference. Order-independent by construction: it takes the set of
/// datagrams, keeps the newest generation, and decodes that generation's
/// passes in index order.
fn model_render(schedule: &[Dg], payloads_by_gen: &[(u8, &Vec<Vec<u8>>)]) -> Vec<u8> {
    let newest = schedule
        .iter()
        .map(|d| d.generation)
        .max_by(|a, b| {
            // 4-bit wrapping comparison, same rule the client uses.
            if ghostframe_client_core::cdf53_coverage::generation_is_newer(*a, *b) {
                std::cmp::Ordering::Greater
            } else if a == b {
                std::cmp::Ordering::Equal
            } else {
                std::cmp::Ordering::Less
            }
        })
        .expect("schedule is non-empty");

    let payloads = payloads_by_gen
        .iter()
        .find(|(g, _)| *g == newest)
        .map(|(_, p)| *p)
        .expect("newest generation has known payloads");

    let mut present: Vec<u8> = schedule
        .iter()
        .filter(|d| d.generation == newest)
        .map(|d| d.pass_idx)
        .collect();
    present.sort_unstable();
    present.dedup();

    let refs: Vec<&[u8]> = present
        .iter()
        .map(|i| payloads[*i as usize].as_slice())
        .collect();
    let coeffs = cdf53::decode_passes(&refs);
    let bgr = cdf53::inverse(&coeffs);

    let mut rgba = vec![255u8; 4096];
    for px in 0..1024 {
        rgba[px * 4] = bgr[px * 3 + 2];
        rgba[px * 4 + 1] = bgr[px * 3 + 1];
        rgba[px * 4 + 2] = bgr[px * 3];
    }
    rgba
}

/// Feed a schedule and return the last `TileReady` pixels for our tile.
fn run(schedule: &[Dg]) -> Option<Vec<u8>> {
    let mut c = core();
    let mut last = None;
    for (i, dg) in schedule.iter().enumerate() {
        for ev in c.handle_datagram(&dg.bytes, i as u64 * 1000) {
            if let Event::TileReady {
                tile_x,
                tile_y,
                rgba,
                ..
            } = ev
            {
                if tile_x == TILE_X && tile_y == TILE_Y {
                    last = Some(rgba);
                }
            }
        }
    }
    last
}

// ---------------------------------------------------------------------------
// Anchors — concrete, fast, and readable when they fail.
// ---------------------------------------------------------------------------

#[test]
fn in_order_delivery_matches_the_model() {
    let (dgs, payloads) = passes_for(1, 1, 0);
    let got = run(&dgs).expect("tile rendered");
    assert_eq!(got, model_render(&dgs, &[(1, &payloads)]));
}

/// The exact shape of the bug this file exists for: a stale pass from
/// generation 0 lands between generation 1's passes.
#[test]
fn a_stale_pass_between_current_ones_is_inert() {
    let (gen0, p0) = passes_for(1, 0, 0);
    let (gen1, p1) = passes_for(2, 1, 9);

    let mut schedule = gen0.clone();
    schedule.push(gen1[0].clone());
    // The stale intruder: generation 0's pass 12, after generation 1 began.
    schedule.push(gen0[12].clone());
    schedule.extend(gen1[1..].iter().cloned());

    let got = run(&schedule).expect("tile rendered");
    let want = model_render(&schedule, &[(0, &p0), (1, &p1)]);
    assert_eq!(
        got, want,
        "a superseded generation's pass changed the rendered result"
    );
}

// ---------------------------------------------------------------------------
// Metamorphic properties.
// ---------------------------------------------------------------------------

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Relation: permuting one generation's passes does not change the
    /// pixels once all of them have arrived.
    #[test]
    fn order_does_not_change_the_result(perm in Just(()).prop_flat_map(|_| {
        Just((0u8..14).collect::<Vec<u8>>()).prop_shuffle()
    })) {
        let (dgs, payloads) = passes_for(1, 1, 0);
        let shuffled: Vec<Dg> = perm.iter().map(|i| dgs[*i as usize].clone()).collect();
        let got = run(&shuffled).expect("tile rendered");
        prop_assert_eq!(got, model_render(&shuffled, &[(1, &payloads)]));
    }

    /// Relation: redelivering datagrams already seen is inert.
    #[test]
    fn duplicates_are_inert(dupes in prop::collection::vec(0u8..14, 0..8)) {
        let (dgs, payloads) = passes_for(1, 1, 0);
        let mut schedule = dgs.clone();
        for d in &dupes {
            schedule.push(dgs[*d as usize].clone());
        }
        let got = run(&schedule).expect("tile rendered");
        prop_assert_eq!(got, model_render(&schedule, &[(1, &payloads)]));
    }

    /// Relation: datagrams from a superseded generation are inert wherever
    /// they land in the stream.
    ///
    /// `insert_at` and `stale_pass` pick an arbitrary intrusion point and an
    /// arbitrary stale pass, so this covers the whole family the anchor test
    /// above samples one member of.
    #[test]
    fn stale_generation_datagrams_are_inert(
        insert_at in 0usize..15,
        stale_pass in 0u8..14,
        stale_count in 1usize..4,
    ) {
        let (gen0, p0) = passes_for(1, 0, 0);
        let (gen1, p1) = passes_for(2, 1, 9);

        let mut schedule: Vec<Dg> = gen0.clone();
        let at = insert_at.min(gen1.len());
        schedule.extend(gen1[..at].iter().cloned());
        for _ in 0..stale_count {
            schedule.push(gen0[stale_pass as usize].clone());
        }
        schedule.extend(gen1[at..].iter().cloned());

        let got = run(&schedule).expect("tile rendered");
        let want = model_render(&schedule, &[(0, &p0), (1, &p1)]);
        prop_assert_eq!(got, want);
    }
}

// ---------------------------------------------------------------------------
// The coverage/NACK path.
//
// Mutation testing showed the properties above do NOT cover it: disabling
// the stale-generation guard in `apply_cdf53_arrival` left all five green,
// because `Event::TileReady` pixels come from `reassembly`, which keeps its
// own copy of the guard. The two consumers are independent, so each needs
// its own property — fixing the bookkeeping alone once stopped the spurious
// NACKs and left the pixels wrong, and this is the other half of that.
// ---------------------------------------------------------------------------

use ghostframe_client_core::cdf53_coverage::{apply_cdf53_arrival, CoverageEntry};

/// Fold a schedule of `(generation, pass_idx)` arrivals into a final entry,
/// collecting every NACK raised along the way.
fn fold_arrivals(arrivals: &[(u8, u8)]) -> (Option<CoverageEntry>, Vec<(u8, u8)>) {
    let mut entry: Option<CoverageEntry> = None;
    let mut nacks = Vec::new();
    for (i, (generation, pass_idx)) in arrivals.iter().enumerate() {
        let out = apply_cdf53_arrival(
            entry,
            *generation,
            *pass_idx,
            u32::from(*generation) + 1,
            i as u64 * 1000,
            true,
        );
        for p in &out.nack_passes {
            nacks.push((*generation, *p));
        }
        entry = Some(out.entry);
    }
    (entry, nacks)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(128))]

    /// Relation: arrivals from a superseded generation leave both the
    /// coverage entry and the NACKs it raised exactly as they would have
    /// been without those arrivals.
    ///
    /// This is the property that fails when the guard at the top of
    /// `apply_cdf53_arrival` is removed.
    #[test]
    fn stale_arrivals_do_not_disturb_coverage(
        current in prop::collection::vec(0u8..14, 1..15),
        stale in prop::collection::vec(0u8..14, 1..5),
        insert_at in 1usize..15,
    ) {
        // Generation 3 supersedes generation 1.
        let clean: Vec<(u8, u8)> = current.iter().map(|p| (3u8, *p)).collect();

        // At least one generation-3 arrival must precede the intrusion.
        // With `insert_at = 0` the older generation arrives first, which
        // makes it legitimately *current* rather than stale: it establishes
        // the entry, and a gap it detects is a real gap that should be
        // NACKed. Proptest found this by shrinking to `current=[0],
        // stale=[3,1]`, where generation 1's pass 1 correctly NACKs the
        // missing pass 0 before generation 3 ever appears. Staleness is a
        // property of arrival *order*, not of the generation number.
        let at = insert_at.min(clean.len()).max(1);
        let mut noisy = clean[..at].to_vec();
        noisy.extend(stale.iter().map(|p| (1u8, *p)));
        noisy.extend(clean[at..].iter().cloned());

        let (clean_entry, clean_nacks) = fold_arrivals(&clean);
        let (noisy_entry, noisy_nacks) = fold_arrivals(&noisy);

        prop_assert_eq!(
            clean_entry.map(|e| (e.generation, e.pass_mask, e.nacked_mask)),
            noisy_entry.map(|e| (e.generation, e.pass_mask, e.nacked_mask)),
            "stale arrivals changed the coverage entry"
        );
        prop_assert_eq!(clean_nacks, noisy_nacks, "stale arrivals changed the NACKs");
    }

    /// Relation: redelivering an arrival is inert for coverage too.
    #[test]
    fn duplicate_arrivals_do_not_disturb_coverage(
        passes in prop::collection::vec(0u8..14, 1..15),
        dupes in prop::collection::vec(0usize..14, 0..6),
    ) {
        let clean: Vec<(u8, u8)> = passes.iter().map(|p| (3u8, *p)).collect();
        let mut noisy = clean.clone();
        for d in &dupes {
            noisy.push(clean[*d % clean.len()]);
        }
        let (clean_entry, _) = fold_arrivals(&clean);
        let (noisy_entry, _) = fold_arrivals(&noisy);
        prop_assert_eq!(
            clean_entry.map(|e| (e.generation, e.pass_mask)),
            noisy_entry.map(|e| (e.generation, e.pass_mask)),
        );
    }
}
