//! `ClientConfig::cdf53_wire`: the core validates a CDF 5/3 pass and hands up
//! its RLE streams instead of the expanded planes. Everything else -- ACKs,
//! DecodeErrors, coverage NACKs -- must be exactly what the planes route
//! gives, because the same peer sees both.

use ghostframe_client_core::cdf53_prevalidate::decode_cdf53_wire;
use ghostframe_client_core::{
    ClientConfig, ClientCore, DecodeErrorCode, Event, PollOutput, TileData, TileDelivery,
};
use ghostframe_protocol::codec::cdf53::{encode_passes_sparse, forward};
use ghostframe_protocol::protocol::{fragment_tile, Codec, TileFragmentInputs, TILE_DATAGRAM_FLAG};

fn core(delivery: TileDelivery, wire: bool) -> ClientCore {
    let mut core = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            tile_delivery: delivery,
            cdf53_wire: wire,
            ..Default::default()
        },
        0,
    );
    while core.poll_transmit(0).is_some() {}
    core
}

fn datagram(frame_seq: u32, generation: u8, pass: u8, payload: &[u8]) -> Vec<u8> {
    let mut v = fragment_tile(
        &TileFragmentInputs {
            frame_seq: frame_seq | TILE_DATAGRAM_FLAG,
            tile_x: 0,
            tile_y: 0,
            codec: Codec::Cdf53,
            generation,
            pass,
            timestamp_us: 0,
        },
        payload,
        1200,
    );
    assert_eq!(v.len(), 1, "one datagram per pass");
    v.remove(0)
}

fn tile_passes() -> Vec<(u8, Vec<u8>)> {
    let bgra: Vec<u8> = (0..1024u32)
        .flat_map(|i| {
            let (x, y) = ((i % 32) as u8, (i / 32) as u8);
            [x.wrapping_mul(9), y.wrapping_mul(5) ^ x, x ^ y, 255]
        })
        .collect();
    encode_passes_sparse(&forward(&bgra)).1
}

/// Run `datagrams` through a core, return its events and everything it
/// queued to send after the ACK debounce.
fn run(mut c: ClientCore, datagrams: &[Vec<u8>]) -> (Vec<Event>, Vec<PollOutput>) {
    let mut events = Vec::new();
    for (i, d) in datagrams.iter().enumerate() {
        events.extend(c.handle_datagram(d, i as u64 * 100));
    }
    // Past the ACK debounce, the NACK debounce and the tail sweep.
    let mut out = Vec::new();
    for t in [50_000u64, 300_000, 1_000_000] {
        c.on_timeout(t);
        while let Some(o) = c.poll_transmit(t) {
            out.push(o);
        }
    }
    (events, out)
}

/// A wire event rewritten as the planes event it stands for.
fn expand(ev: Event) -> Event {
    match ev {
        Event::TilePayload {
            frame_seq,
            tile_x,
            tile_y,
            generation,
            data:
                TileData::Cdf53Wire {
                    pass_idx,
                    streams,
                    present_passes,
                    new_generation,
                },
        } => {
            let mut planes = [0u8; 384];
            decode_cdf53_wire(&streams, &mut planes).expect("validated streams expand");
            Event::TilePayload {
                frame_seq,
                tile_x,
                tile_y,
                generation,
                data: TileData::Cdf53 {
                    pass_idx,
                    bit_planes: planes.to_vec(),
                    present_passes,
                    new_generation,
                },
            }
        }
        other => other,
    }
}

fn burst() -> Vec<Vec<u8>> {
    let passes = tile_passes();
    let mut dgs = Vec::new();
    for (idx, p) in passes.iter() {
        dgs.push(datagram(10, 1, *idx, p));
    }
    // Hostile / odd passes: corrupt RLE length, truncated, pass index 14,
    // trailing bytes, a duplicate.
    let (idx1, good1) = passes[1].clone();
    let mut bad_rle = good1.clone();
    bad_rle[2] = bad_rle[2].wrapping_add(1);
    dgs.push(datagram(10, 1, idx1, &bad_rle));
    dgs.push(datagram(10, 1, idx1, &good1[..good1.len() / 2]));
    dgs.push(datagram(10, 1, 14, &good1));
    let mut trailing = good1.clone();
    trailing.extend_from_slice(&[1, 2, 3]);
    dgs.push(datagram(10, 1, idx1, &trailing));
    dgs.push(datagram(10, 1, idx1, &good1));
    dgs
}

#[test]
fn wire_mode_emits_the_wire_variant_and_default_does_not() {
    let dgs = burst();
    let (plain, _) = run(core(TileDelivery::Payload, false), &dgs);
    let (wire, _) = run(core(TileDelivery::Payload, true), &dgs);
    let is_wire = |e: &Event| {
        matches!(
            e,
            Event::TilePayload {
                data: TileData::Cdf53Wire { .. },
                ..
            }
        )
    };
    let n_planes = plain
        .iter()
        .filter(|e| {
            matches!(
                e,
                Event::TilePayload {
                    data: TileData::Cdf53 { .. },
                    ..
                }
            )
        })
        .count();
    assert!(n_planes > 5, "the burst reached the measured path");
    assert!(!plain.iter().any(is_wire), "default off");
    assert_eq!(wire.iter().filter(|e| is_wire(e)).count(), n_planes);
}

#[test]
fn wire_and_planes_routes_agree_on_events_acks_and_nacks() {
    let dgs = burst();
    let (plain_ev, plain_out) = run(core(TileDelivery::Payload, false), &dgs);
    let (wire_ev, wire_out) = run(core(TileDelivery::Payload, true), &dgs);
    let wire_ev: Vec<Event> = wire_ev.into_iter().map(expand).collect();
    assert_eq!(wire_ev, plain_ev);
    assert_eq!(wire_out, plain_out);
    // The hostile passes really were rejected, and the good ones ACKed.
    let errors = plain_ev
        .iter()
        .filter(|e| matches!(e, Event::DecodeError { .. }))
        .count();
    assert!(errors >= 3, "got {errors} decode errors");
    assert!(plain_out
        .iter()
        .any(|o| matches!(o, PollOutput::Datagram(_))));
}

/// Every pass of a tile in order, except that one is corrupt and never
/// retransmitted: later passes arrive, so coverage sees the gap and NACKs it.
fn gap_burst() -> Vec<Vec<u8>> {
    let passes = tile_passes();
    passes
        .iter()
        .enumerate()
        .map(|(i, (idx, p))| {
            if i == 2 {
                let mut bad = p.clone();
                bad[2] = bad[2].wrapping_add(1);
                datagram(10, 1, *idx, &bad)
            } else {
                datagram(10, 1, *idx, p)
            }
        })
        .collect()
}

#[test]
fn a_gap_provokes_the_same_nack_in_both_routes() {
    let dgs = gap_burst();
    let (plain_ev, plain_out) = run(core(TileDelivery::Payload, false), &dgs);
    let (wire_ev, wire_out) = run(core(TileDelivery::Payload, true), &dgs);
    let wire_ev: Vec<Event> = wire_ev.into_iter().map(expand).collect();
    assert_eq!(wire_ev, plain_ev);
    assert_eq!(wire_out, plain_out);
    let nacks = |outs: &[PollOutput]| {
        outs.iter()
            .filter(|o| {
                matches!(o, PollOutput::Datagram(d)
                    if d.first() == Some(&ghostframe_protocol::protocol::TILE_NACK_ENVELOPE))
            })
            .count()
    };
    // Proves the comparison above is not between two empty outputs.
    assert!(
        nacks(&plain_out) >= 1,
        "planes route sent no NACK: {plain_out:?}"
    );
    assert!(nacks(&wire_out) >= 1, "wire route sent no NACK");
}

#[test]
fn a_rejected_pass_gets_the_decode_error_and_no_wire_event() {
    let passes = tile_passes();
    let (idx, good) = passes[0].clone();
    // Pass 0 with bit 0 of the bitmap cleared.
    let mut bad = good.clone();
    bad[1] &= !1;
    let (ev, out) = run(
        core(TileDelivery::Payload, true),
        &[datagram(1, 1, idx, &bad)],
    );
    assert!(matches!(
        ev.as_slice(),
        [Event::DecodeError {
            code: DecodeErrorCode::Cdf53BadPass,
            ..
        }]
    ));
    assert!(
        !out.iter().any(|o| matches!(o, PollOutput::Datagram(d) if d.first() == Some(&ghostframe_protocol::ack::ACK_BATCH_MSG_TYPE))),
        "no ACK for a rejected pass"
    );
}

#[test]
fn the_flag_is_ignored_in_decoded_mode() {
    let dgs = burst();
    let (a, _) = run(core(TileDelivery::Decoded, false), &dgs);
    let (b, _) = run(core(TileDelivery::Decoded, true), &dgs);
    assert_eq!(a, b);
    assert!(a.iter().any(|e| matches!(e, Event::TileReady { .. })));
}

/// Realistic passes: noisy, photo-like and text-like tiles, sparse-encoded.
fn corpus() -> Vec<(u8, Vec<u8>)> {
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        (seed >> 24) as u8
    };
    let mut out = Vec::new();
    for kind in 0..3u32 {
        for variant in 0..8u32 {
            let tile: Vec<u8> = (0..1024u32)
                .flat_map(|i| {
                    let (x, y) = (i % 32, i / 32);
                    let px: [u8; 3] = match kind {
                        0 => [rnd(), rnd(), rnd()],
                        1 => {
                            let v = ((x * 5 + y * 3 + variant * 17) as u8).wrapping_add(rnd() >> 5);
                            [v, v.wrapping_add(30), 255 - v]
                        }
                        _ => {
                            let t = if (x / 3 + y / 5 + variant) % 3 == 0 {
                                0x20
                            } else {
                                0xE0
                            };
                            [t, t, t]
                        }
                    };
                    [px[0], px[1], px[2], 255]
                })
                .collect();
            out.extend(encode_passes_sparse(&forward(&tile)).1);
        }
    }
    out
}

/// `cargo test -p ghostframe-client-core --release --test cdf53_wire -- --ignored --nocapture net_side_cost`
///
/// What the net thread pays per pass: the validate-and-expand it did before,
/// the validate-only it does now, and the whole `handle_datagram` path with
/// the flag off and on.
#[test]
#[ignore = "measurement, prints numbers"]
#[allow(clippy::disallowed_methods, reason = "measuring real elapsed time")]
fn net_side_cost() {
    use ghostframe_client_core::cdf53_prevalidate::{prevalidate_cdf53, validate_cdf53};
    use std::hint::black_box;
    use std::time::Instant;
    let passes = corpus();
    let n = passes.len();
    let bytes: usize = passes.iter().map(|(_, p)| p.len()).sum();
    println!("{n} passes, mean {} B", bytes / n);
    let reps = 2000;
    let time = |f: &mut dyn FnMut()| {
        f(); // warm
        let t = Instant::now();
        for _ in 0..reps {
            f();
        }
        t.elapsed().as_secs_f64() * 1e9 / (reps * n) as f64
    };
    for _ in 0..3 {
        let pre = time(&mut || {
            for (i, p) in &passes {
                black_box(prevalidate_cdf53(black_box(p), 1, *i).is_ok());
            }
        });
        let val = time(&mut || {
            for (i, p) in &passes {
                black_box(validate_cdf53(black_box(p), *i).is_ok());
            }
        });
        println!("prevalidate {pre:7.1} ns/pass   validate {val:7.1} ns/pass");
    }
    // The whole path: 10k datagrams, a tile per datagram batch.
    let dgs: Vec<Vec<u8>> = (0..10_000usize)
        .map(|k| {
            let (idx, p) = &passes[k % n];
            let mut v = fragment_tile(
                &TileFragmentInputs {
                    frame_seq: (k as u32 / 14) | TILE_DATAGRAM_FLAG,
                    tile_x: (k % 60) as u8,
                    tile_y: ((k / 60) % 34) as u8,
                    codec: Codec::Cdf53,
                    generation: 1,
                    pass: *idx,
                    timestamp_us: 0,
                },
                p,
                1200,
            );
            v.remove(0)
        })
        .collect();
    for wire in [false, true, false, true] {
        let mut c = core(TileDelivery::Payload, wire);
        let t = Instant::now();
        let mut events = 0usize;
        for (i, d) in dgs.iter().enumerate() {
            events += c.handle_datagram(d, i as u64 * 10).len();
        }
        let ms = t.elapsed().as_secs_f64() * 1e3;
        println!(
            "handle_datagram x{} wire={wire}: {ms:6.1} ms ({:.2} us/datagram), {events} events",
            dgs.len(),
            ms * 1e3 / dgs.len() as f64
        );
    }
}
