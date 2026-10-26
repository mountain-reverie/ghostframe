//! Offline analysis: how much of a loss does the reliable emitter's fixed
//! XOR parity (K = 10, one parity per group, emitted
//! `PARITY_INTERLEAVE_OFFSET` = 20 wire_seqs after the group's first source)
//! absorb under i.i.d. loss versus Gilbert-Elliott burst loss at the *same*
//! long-run rate?
//!
//! Layout, read from `reliable_emitter/{emitter,emission_queue}.rs`: group
//! `g` holds sources `wire_seq 10g ..= 10g+9`. `feed_group` schedules its
//! parity with `emit_after = 10g + 20`; `EmissionQueue::pop(next_wire_seq)`
//! promotes a parity once `next_wire_seq >= emit_after`, so in the idealised
//! (no backlog) stream the parity follows source `10g+19` and precedes source
//! `10g+20`:
//!
//! ```text
//! S0..S9 (g0) S10..S19 (g1) P0 S20..S29 (g2) P1 S30..S39 (g3) P2 ...
//! ```
//!
//! Recovery rule (single parity per group): a group recovers iff exactly one
//! of its K sources was lost AND its parity arrived. Retransmission is not
//! modelled: the figure is what FEC alone absorbs, i.e. the residual that
//! NACK/RTO must still cover.
//!
//! The mask comes from `NetSim::decide`, so iid and burst use the same
//! machinery. The burst model is the per-datagram chain
//! (`from_rate_and_burst_per_datagram`): this analysis reasons about runs of
//! consecutive datagram *indices* against fixed group boundaries, which a
//! time-based chain would turn into a function of the 100 us spacing used to
//! drive it. Run with `--nocapture` for the table.

use ghostframe_e2e::netsim::{GilbertElliott, NetProfile, NetSim, Verdict};
use ghostframe_lib::transport::reliable_emitter::{FEC_GROUP_SIZE_K, PARITY_INTERLEAVE_OFFSET};

const GROUPS: usize = 200_000;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Source(usize), // group
    Parity(usize),
}

/// The wire order of `groups` groups, parities at their real offset.
fn stream(groups: usize) -> Vec<Kind> {
    let k = FEC_GROUP_SIZE_K;
    let off = PARITY_INTERLEAVE_OFFSET as usize;
    let mut out = Vec::new();
    let mut next_parity = 0usize;
    for n in 0..groups * k {
        // Parity g is due once the next source wire_seq reaches 10g + off.
        while next_parity < groups && next_parity * k + off <= n {
            out.push(Kind::Parity(next_parity));
            next_parity += 1;
        }
        out.push(Kind::Source(n / k));
    }
    // End-of-stream flush of whatever parity is still pending.
    while next_parity < groups {
        out.push(Kind::Parity(next_parity));
        next_parity += 1;
    }
    out
}

fn mask(profile: NetProfile, seed: u64, n: usize) -> Vec<bool> {
    let mut sim = NetSim::new(profile, seed);
    (0..n)
        .map(|i| matches!(sim.decide(1200, i as u64 * 100), Verdict::Drop))
        .collect()
}

struct Outcome {
    lost_sources: usize,
    recovered: usize,
    groups_with_loss: usize,
    groups_multi: usize,
}

impl Outcome {
    fn residual(&self) -> f64 {
        (self.lost_sources - self.recovered) as f64 / self.lost_sources as f64
    }
}

fn analyse(layout: &[Kind], lost: &[bool], groups: usize) -> Outcome {
    let mut src_lost = vec![0usize; groups];
    let mut parity_ok = vec![true; groups];
    for (k, &l) in layout.iter().zip(lost) {
        match *k {
            Kind::Source(g) if l => src_lost[g] += 1,
            Kind::Parity(g) if l => parity_ok[g] = false,
            _ => {}
        }
    }
    let mut o = Outcome {
        lost_sources: src_lost.iter().sum(),
        recovered: 0,
        groups_with_loss: 0,
        groups_multi: 0,
    };
    for g in 0..groups {
        match src_lost[g] {
            0 => {}
            1 => {
                o.groups_with_loss += 1;
                if parity_ok[g] {
                    o.recovered += 1;
                }
            }
            _ => {
                o.groups_with_loss += 1;
                o.groups_multi += 1;
            }
        }
    }
    o
}

fn ge(avg: f64, len: f64) -> NetProfile {
    let mut p = NetProfile::perfect();
    GilbertElliott::from_rate_and_burst_per_datagram(avg, len, 1.0)
        .expect("feasible")
        .apply_to(&mut p);
    p
}

#[test]
fn fixed_xor_fec_residual_iid_vs_burst() {
    let layout = stream(GROUPS);
    // Layout sanity: K sources per parity, parity strictly after its group.
    let mut seen_last_src = vec![usize::MAX; GROUPS];
    for (i, k) in layout.iter().enumerate() {
        match *k {
            Kind::Source(g) => seen_last_src[g] = i,
            Kind::Parity(g) => assert!(i > seen_last_src[g], "parity {g} before its sources"),
        }
    }
    let n = layout.len();

    println!(
        "\nFEC K={FEC_GROUP_SIZE_K} offset={PARITY_INTERLEAVE_OFFSET}, {GROUPS} groups, \
         {n} datagrams, GE loss_bad=1 (bad-state sojourn = mean loss run)"
    );
    println!(
        "{:>6} {:>12} {:>10} {:>10} {:>12} {:>11} {:>9}",
        "rate", "model", "meas.loss", "src_lost", "grp>=2 lost", "recovered", "residual"
    );

    let mut iid_residual = std::collections::HashMap::new();
    let mut burst_ge_iid = 0;
    let mut burst_total = 0;
    for &rate in &[0.01, 0.02, 0.05] {
        let mut rows: Vec<(String, NetProfile)> = vec![(
            "iid".into(),
            NetProfile {
                loss: rate,
                ..NetProfile::perfect()
            },
        )];
        for &len in &[1.0, 3.0, 8.0, 20.0] {
            rows.push((format!("burst{len}"), ge(rate, len)));
        }
        for (name, profile) in rows {
            // Average over seeds to tame sampling noise in the comparison.
            let mut lost_sources = 0;
            let mut rec = 0;
            let mut multi = 0;
            let mut tot_lost = 0;
            for seed in 1..=3u64 {
                let m = mask(profile.clone(), seed, n);
                tot_lost += m.iter().filter(|d| **d).count();
                let o = analyse(&layout, &m, GROUPS);
                lost_sources += o.lost_sources;
                rec += o.recovered;
                multi += o.groups_multi;
                let _ = o.groups_with_loss;
            }
            assert!(lost_sources > 0, "premise: no source losses for {name}");
            let o = Outcome {
                lost_sources,
                recovered: rec,
                groups_with_loss: 0,
                groups_multi: multi,
            };
            println!(
                "{:>5.0}% {:>12} {:>10.4} {:>10} {:>12} {:>11} {:>8.1}%",
                rate * 100.0,
                name,
                tot_lost as f64 / (3 * n) as f64,
                lost_sources,
                multi,
                rec,
                o.residual() * 100.0
            );
            if name == "iid" {
                iid_residual.insert(rate.to_bits(), o.residual());
            } else if name != "burst1" {
                burst_total += 1;
                if o.residual() + 0.02 >= iid_residual[&rate.to_bits()] {
                    burst_ge_iid += 1;
                }
            }
        }
    }
    // Sanity only: with bursts of 3+ the residual is not meaningfully
    // *better* than iid at the same rate (2 points of slack).
    assert_eq!(
        burst_ge_iid, burst_total,
        "a burst model absorbed clearly more than iid; the hypothesis is not supported"
    );
}
