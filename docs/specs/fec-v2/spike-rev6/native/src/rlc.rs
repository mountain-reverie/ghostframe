// SPDX-License-Identifier: MIT OR Apache-2.0
//! cu-fec (RFC 8681 sliding-window RLC, GF(2^8), full density) with a window of w source symbols
//! and L losses repaired by exactly L (or L+1) repair symbols.
//! Output sections: cost (TSV) and failure probability (TSV).
use super::*;
use cu_fec::{DensityThreshold, EncodingSymbolId as Esi, Field, RepairParameters, RlcConfig, RlcDecoder, RlcEncoder};
use rand::seq::SliceRandom;

const CAP: usize = 1200;
const WCAP: usize = 64;
const ECAP: usize = 8;
type Enc = RlcEncoder<CAP, WCAP>;
type Dec = RlcDecoder<CAP, WCAP, ECAP>;

struct Pool { cfg: RlcConfig, src: Vec<Vec<u8>>, repairs: Vec<(cu_fec::RepairPayloadId, Vec<u8>)> }

fn pool(w: usize, nrep: usize) -> Pool {
    let cfg = RlcConfig::new(SYM, w, Field::Gf256).unwrap();
    let src = make_sources(w, SYM, 11);
    let mut enc = Box::new(Enc::new(cfg, Esi::new(0)).unwrap());
    for s in &src { enc.push_source(s).unwrap(); }
    let repairs = (0..nrep).map(|key| {
        let mut out = vec![0u8; SYM];
        let id = enc.encode_repair(RepairParameters::new(key as u16 + 1, DensityThreshold::FULL), &mut out).unwrap();
        (id, out)
    }).collect();
    Pool { cfg, src, repairs }
}

/// Feed all but `lost` sources and the given repairs; true when every source is known afterwards.
fn try_decode(p: &Pool, lost: &[usize], reps: &[usize]) -> bool {
    let w = p.src.len();
    let mut d = Box::new(Dec::new(p.cfg, ECAP).unwrap());
    for j in 0..w { if !lost.contains(&j) { let _ = d.receive_source(Esi::new(j as u32), &p.src[j]).unwrap(); } }
    for &i in reps { let _ = d.receive_repair(p.repairs[i].0, &p.repairs[i].1).unwrap(); }
    (0..w).all(|j| d.symbol(Esi::new(j as u32)) == Some(&p.src[j][..]))
}

pub fn run() {
    println!("## cost\nwindow\tlosses\tencode_one_repair_us(med)\tp95\tdecode_us(med)\tp95");
    let mut rg = rng(7);
    for &w in &[20usize, 40] {
        let p = pool(w, 300);
        let cfg = p.cfg;
        let mut enc = Box::new(Enc::new(cfg, Esi::new(0)).unwrap());
        for s in &p.src { enc.push_source(s).unwrap(); }
        let mut out = vec![0u8; SYM];
        let mut key = 0u16;
        let (em, ep) = bench(20, 300, || (), |_| {
            key = key.wrapping_add(1);
            enc.encode_repair(RepairParameters::new(key, DensityThreshold::FULL), &mut out).unwrap();
        });
        for l in 1..=4usize {
            // pick the same L lost + first L repairs whose decode succeeds, so we time a successful decode
            let lost: Vec<usize> = (0..l).collect();
            let reps: Vec<usize> = (0..l).collect();
            let ok = try_decode(&p, &lost, &reps);
            let (dm, dp) = bench(20, 300, || (), |_| { std::hint::black_box(try_decode(&p, &lost, &reps)); });
            println!("{w}\t{l}\t{em:.1}\t{ep:.1}\t{dm:.1}{}\t{dp:.1}", if ok { "" } else { "(this draw failed)" });
        }
        let _ = &mut rg;
    }
    println!("## failure probability (random lost positions, random distinct repair keys; decode includes all received sources)\nwindow\tlosses\trepairs_received\ttrials\tfailures\tfail_rate");
    for &w in &[20usize, 40] {
        let p = pool(w, 256);
        for l in 1..=4usize {
            for extra in [0usize, 1] {
                let trials = 20000;
                let mut fails = 0;
                for _ in 0..trials {
                    let mut lost: Vec<usize> = (0..w).collect(); lost.shuffle(&mut rg); lost.truncate(l);
                    let mut reps: Vec<usize> = (0..p.repairs.len()).collect(); reps.shuffle(&mut rg); reps.truncate(l + extra);
                    if !try_decode(&p, &lost, &reps) { fails += 1; }
                }
                println!("{w}\t{l}\t{}\t{trials}\t{fails}\t{:.5}", l + extra, fails as f64 / trials as f64);
            }
        }
    }
}
