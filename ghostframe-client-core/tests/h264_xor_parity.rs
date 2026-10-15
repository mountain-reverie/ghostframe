//! The client consumes the H.264 XOR parity the server already sends
//! (`io_bridge/frame.rs`: K=5 for P-frames, K=2 for keyframes). Before this,
//! `frame_assembly.rs` dropped every parity datagram, so the 20-50% overhead
//! bought nothing.

use ghostframe_client_core::{ClientConfig, ClientCore, Event};
use ghostframe_protocol::fec;
use ghostframe_protocol::protocol::{
    build_frame_parity_datagram, fragment_frame, max_fec_frame_fragment_payload, FRAME_HEADER_SIZE,
};

fn core() -> ClientCore {
    let mut c = ClientCore::new(
        ClientConfig {
            indices_raw_enabled: true,
            supports_h264: true,
            ..Default::default()
        },
        0,
    );
    while c.poll_transmit(0).is_some() {}
    c
}

/// Exactly what the server puts on the wire for one access unit: sources then
/// parities (mirrors `io_bridge/frame.rs`).
fn wire(seq: u32, key: bool, payload: &[u8]) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let srcs = fragment_frame(seq, 7, key, payload, max_fec_frame_fragment_payload(1200));
    let mut parities = Vec::new();
    if srcs.len() > 1 {
        let refs: Vec<&[u8]> = srcs.iter().map(|d| &d[FRAME_HEADER_SIZE..]).collect();
        for (_, p) in fec::generate_parity(&refs, fec::fec_group_size(key)) {
            parities.push(build_frame_parity_datagram(
                seq,
                7,
                key,
                srcs.len() as u16,
                &p,
            ));
        }
    }
    (srcs, parities)
}

fn payload(len: usize, salt: u8) -> Vec<u8> {
    // Non-zero last byte, like a NAL (RBSP stop bit).
    let mut v: Vec<u8> = (0..len)
        .map(|i| (i as u8).wrapping_mul(31).wrapping_add(salt))
        .collect();
    *v.last_mut().unwrap() = 0x80;
    v
}

fn deliver(c: &mut ClientCore, dgs: impl IntoIterator<Item = Vec<u8>>) -> Vec<Event> {
    dgs.into_iter()
        .flat_map(|d| c.handle_datagram(&d, 0))
        .collect()
}

fn frames(evs: &[Event]) -> Vec<(u32, &Vec<u8>)> {
    evs.iter()
        .filter_map(|e| match e {
            Event::NeedsH264 {
                frame_seq, payload, ..
            } => Some((*frame_seq, payload)),
            _ => None,
        })
        .collect()
}

#[test]
fn every_single_loss_is_recovered_byte_exact_including_the_short_last_fragment() {
    for key in [false, true] {
        for len in [3000usize, 5000, 6123] {
            let p = payload(len, 3);
            let (srcs, parities) = wire(10, key, &p);
            let k = fec::fec_group_size(key);
            for lost in 0..srcs.len() {
                // A trailing group of one fragment gets no parity (fec.rs:
                // "groups of size 1 are skipped"): nothing to recover from.
                if lost == srcs.len() - 1 && srcs.len() % k == 1 {
                    continue;
                }
                let mut c = core();
                let sent: Vec<Vec<u8>> = srcs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != lost)
                    .map(|(_, d)| d.clone())
                    .chain(parities.iter().cloned())
                    .collect();
                let evs = deliver(&mut c, sent);
                let got = frames(&evs);
                assert_eq!(
                    got.len(),
                    1,
                    "key={key} len={len} lost={lost}: frame not recovered"
                );
                assert_eq!(
                    got[0].1, &p,
                    "key={key} len={len} lost={lost}: bytes differ"
                );
                assert_eq!(
                    c.h264_fragments_recovered(),
                    1,
                    "recovery counter must be non-zero"
                );
            }
        }
    }
}

#[test]
fn a_parity_that_arrives_before_the_late_source_still_completes_the_frame() {
    let p = payload(5000, 9);
    let (srcs, parities) = wire(4, false, &p);
    assert!(srcs.len() >= 4);
    // Group 0 loses sources 1 and 2 at first; 2 shows up after the parity.
    let mut c = core();
    let mut first: Vec<Vec<u8>> = srcs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 1 && *i != 2)
        .map(|(_, d)| d.clone())
        .collect();
    first.extend(parities.iter().cloned());
    assert!(frames(&deliver(&mut c, first)).is_empty());
    let evs = deliver(&mut c, [srcs[2].clone()]);
    let got = frames(&evs);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, &p);
}

#[test]
fn two_losses_in_one_group_are_not_recovered_and_never_yield_wrong_bytes() {
    let p = payload(5000, 1);
    let (srcs, parities) = wire(4, false, &p);
    let mut c = core();
    let sent: Vec<Vec<u8>> = srcs
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 0 && *i != 1)
        .map(|(_, d)| d.clone())
        .chain(parities.iter().cloned())
        .collect();
    assert!(frames(&deliver(&mut c, sent)).is_empty());
    assert_eq!(c.h264_fragments_recovered(), 0);
}

#[test]
fn parity_never_advances_the_frame_window_or_drops_real_frames() {
    // A parity naming a far-future frame (or one with no assembly at all) must
    // not move `latest_full_frame_seq`: a real, older frame still has to render.
    let p = payload(5000, 5);
    let (srcs, parities) = wire(10, false, &p);
    let mut c = core();
    // Hostile / stray parity for frame 1_000_000 with a plausible header.
    let (_, far) = wire(1_000_000, false, &payload(5000, 6));
    deliver(&mut c, far);
    let evs = deliver(&mut c, srcs.into_iter().chain(parities));
    assert_eq!(frames(&evs).len(), 1, "a stray parity dropped a real frame");
}

#[test]
fn a_parity_cannot_create_an_assembly() {
    let (_, parities) = wire(77, false, &payload(5000, 2));
    let mut c = core();
    deliver(&mut c, parities);
    assert_eq!(c.assembly_stats().frame_assemblies, 0);
}

/// Deterministic Gilbert-Elliott packet loss (loss_bad = 1): P(g->b) from the
/// target rate and mean burst, like `ghostframe_e2e::netsim::GilbertElliott`.
struct Ge {
    s: u64,
    bad: bool,
    p_gb: f64,
    p_bg: f64,
}
impl Ge {
    fn new(rate: f64, burst: f64, seed: u64) -> Self {
        let p_bg = 1.0 / burst.max(1.0);
        let p_gb = if burst <= 1.0 {
            rate
        } else {
            rate * p_bg / (1.0 - rate)
        };
        Ge {
            s: seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407),
            bad: false,
            p_gb,
            p_bg,
        }
    }
    fn u(&mut self) -> f64 {
        self.s ^= self.s << 13;
        self.s ^= self.s >> 7;
        self.s ^= self.s << 17;
        (self.s >> 11) as f64 / (1u64 << 53) as f64
    }
    fn lost(&mut self) -> bool {
        if self.p_bg >= 1.0 {
            return self.u() < self.p_gb;
        }
        let u = self.u();
        self.bad = if self.bad {
            u >= self.p_bg
        } else {
            u < self.p_gb
        };
        self.bad
    }
}

/// Frame-loss rate with and without the client using parity, over the same loss
/// pattern. A property, not a threshold: using parity must help, must actually
/// recover something, and every frame it hands over must be byte-exact.
#[test]
fn parity_cuts_frame_loss_under_iid_and_burst_loss() {
    for (rate, burst) in [(0.02, 1.0), (0.05, 1.0), (0.02, 4.0)] {
        let (mut with_p, mut without_p, trials) = (0u32, 0u32, 1500u32);
        let mut recovered_total = 0u64;
        let mut ge = Ge::new(rate, burst, 42);
        for t in 0..trials {
            let p = payload(48_000, t as u8); // ~40 fragments
            let (srcs, parities) = wire(t + 1, false, &p);
            let mask: Vec<bool> = (0..srcs.len() + parities.len())
                .map(|_| ge.lost())
                .collect();
            let mut c = core();
            let mut plain = core();
            let all = srcs.iter().chain(parities.iter());
            let mut with = Vec::new();
            let mut base = Vec::new();
            for (d, &l) in all.zip(&mask) {
                if !l {
                    with.extend(c.handle_datagram(d, 0));
                    // Baseline: the same client with the parity withheld.
                    if !parities.contains(d) {
                        base.extend(plain.handle_datagram(d, 0));
                    }
                }
            }
            if frames(&with).len() == 1 {
                assert_eq!(frames(&with)[0].1, &p, "recovered frame not byte-exact");
            } else {
                with_p += 1;
            }
            if frames(&base).is_empty() {
                without_p += 1;
            }
            recovered_total += c.h264_fragments_recovered();
        }
        println!(
            "H264PARITY rate={rate} burst={burst} frames=40frag trials={trials} frame_loss_without_parity={:.3} frame_loss_with={:.3} recovered_fragments={recovered_total}",
            without_p as f64 / trials as f64,
            with_p as f64 / trials as f64
        );
        assert!(recovered_total > 0, "premise: nothing was ever recovered");
        assert!(with_p < without_p, "parity did not reduce frame loss");
    }
}

#[test]
fn hostile_parity_headers_change_nothing() {
    let p = payload(5000, 8);
    let (srcs, parities) = wire(4, false, &p);
    let n = srcs.len();
    let mut c = core();
    deliver(&mut c, srcs.iter().skip(1).cloned()); // fragment 0 missing
                                                   // Forge parities: group runs past frag_total; oversized group_len; a
                                                   // frag_total that disagrees with the assembly; group_len 1.
    let mk = |gs: u16, gl: u8, total: u16| {
        let mut payload = Vec::new();
        payload.extend_from_slice(&gs.to_be_bytes());
        payload.push(gl);
        payload.extend_from_slice(&[0xAA; 100]);
        build_frame_parity_datagram(4, 7, false, total, &payload)
    };
    for d in [
        mk(n as u16 - 1, 5, n as u16),
        mk(0, 200, n as u16),
        mk(0, 5, n as u16 + 1),
        mk(0, 1, n as u16),
    ] {
        assert!(c.handle_datagram(&d, 0).is_empty());
    }
    assert_eq!(c.h264_fragments_recovered(), 0);
    // The genuine parity still works afterwards and the bytes are exact.
    let evs = deliver(&mut c, parities);
    let got = frames(&evs);
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].1, &p);
}
