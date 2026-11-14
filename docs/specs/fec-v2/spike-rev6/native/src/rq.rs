// SPDX-License-Identifier: MIT OR Apache-2.0
//! RaptorQ reception overhead: P(decode fails | exactly n distinct symbols received).
//! Two scenarios per (k, n): "mixed" = n symbols drawn uniformly from k sources + 2k+8 repairs
//! (what a lossy path delivers to a systematic code); "repairs-only" = all n are repair symbols
//! (RFC 6330's own worst case, no systematic help).
use super::*;
use raptorq::{Decoder as RqDec, Encoder as RqEnc};
use rand::seq::SliceRandom;

const TRIALS: usize = 4000;

pub fn run() {
    println!("scenario\tk\tn_received\ttrials\tfailures\tfail_rate");
    let mut rg = rng(99);
    for &k in &[4usize, 10, 20, 40, 200] {
        let oti = rq_oti(k * SYM);
        let data: Vec<u8> = make_sources(k, SYM, 5).concat();
        let nrep = 2 * k + 8;
        let pk = RqEnc::new(&data, oti).get_encoded_packets(nrep as u32);
        assert_eq!(pk.len(), k + nrep);
        for scen in ["mixed", "repairs-only"] {
            let pool: Vec<usize> = if scen == "mixed" { (0..pk.len()).collect() } else { (k..pk.len()).collect() };
            for n in [k, k + 1, k + 2] {
                let mut fails = 0;
                for _ in 0..TRIALS {
                    let mut idx = pool.clone();
                    idx.shuffle(&mut rg);
                    idx.truncate(n);
                    let mut d = RqDec::new(oti);
                    let mut got = None;
                    for i in idx { if let Some(o) = d.decode(pk[i].clone()) { got = Some(o); break; } }
                    if got.as_deref() != Some(&data[..]) { fails += 1; }
                }
                println!("{scen}\t{k}\t{n}\t{TRIALS}\t{fails}\t{:.5}", fails as f64 / TRIALS as f64);
            }
        }
    }
}
