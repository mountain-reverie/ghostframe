// SPDX-License-Identifier: MIT OR Apache-2.0
//! FEC v2 rev 6 spike: native measurements. Subcommands: correct, speed, rq, rlc, varsize.
#[path = "../../common/gf256.rs"]
pub mod gf256;
#[path = "../../common/smalldec.rs"]
pub mod smalldec;
mod correct;
mod rlc;
mod rq;
mod speed;
mod varsize;

use rand::{rngs::StdRng, Rng, SeedableRng};
use rusty_erasure::{coder, Coder, Matrix};
use std::time::Instant;

pub const SYM: usize = 1180;

/// RaptorQ parameters with exact 1180-byte symbols (`with_defaults` would round the symbol
/// size down to a multiple of 8, i.e. 1176, and change K). One source block, one sub-block.
pub fn rq_oti(len: usize) -> raptorq::ObjectTransmissionInformation {
    raptorq::ObjectTransmissionInformation::new(len as u64, SYM as u16, 1, 1, 1)
}

pub fn rng(seed: u64) -> StdRng { StdRng::seed_from_u64(seed) }

pub fn make_sources(k: usize, len: usize, seed: u64) -> Vec<Vec<u8>> {
    let mut r = rng(seed);
    (0..k).map(|_| (0..len).map(|_| r.gen()).collect()).collect()
}

/// Our normalised Cauchy matrix (repair row 0 = ones) through the custom-matrix API.
pub fn ours_coder(k: usize, r: usize) -> Coder {
    coder(Matrix::from_bytes(k + r, k, gf256::ours_matrix(k, r, None)).unwrap()).unwrap()
}
/// rusty_erasure's built-in Cauchy.
pub fn builtin_coder(k: usize, r: usize) -> Coder { coder(Matrix::cauchy(k, r).unwrap()).unwrap() }

pub fn rusty_encode(c: &Coder, src: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let len = src[0].len();
    let refs: Vec<&[u8]> = src.iter().map(|v| &v[..]).collect();
    let mut out = vec![vec![0u8; len]; c.p()];
    let mut m: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
    c.encode(&refs, &mut m).unwrap();
    out
}

/// Recover `rebuild` indices from `shards` (k sources then r repairs, None = erased).
pub fn rusty_recover(c: &Coder, shards: &[Option<&[u8]>], rebuild: &[usize], len: usize)
    -> Result<Vec<Vec<u8>>, rusty_erasure::RecoverError> {
    let mut out = vec![vec![0u8; len]; rebuild.len()];
    let mut m: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
    c.recover(shards, rebuild, &mut m)?;
    Ok(out)
}

/// Median / p95 / min in microseconds over `iters` timed runs after `warm` warm-ups.
/// `f` gets the iteration index; setup that must not be timed is done by the caller via `prep`.
pub fn bench<S>(warm: usize, iters: usize, mut prep: impl FnMut() -> S, mut f: impl FnMut(S)) -> (f64, f64) {
    for _ in 0..warm { let s = prep(); f(s); }
    let mut t = Vec::with_capacity(iters);
    for _ in 0..iters {
        let s = prep();
        let t0 = Instant::now();
        f(s);
        t.push(t0.elapsed().as_nanos() as f64 / 1000.0);
    }
    t.sort_by(|a, b| a.partial_cmp(b).unwrap());
    (t[t.len() / 2], t[(t.len() * 95 / 100).min(t.len() - 1)])
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("");
    let flag = |f: &str| args.iter().any(|a| a == f);
    let code = match cmd {
        "correct" => correct::run(flag("--mutate-repair1"), flag("--mutate-row0")),
        "speed" => { speed::run(); 0 }
        "rq" => { rq::run(); 0 }
        "rlc" => { rlc::run(); 0 }
        "varsize" => varsize::run(),
        _ => { eprintln!("usage: spike_native correct|speed|rq|rlc|varsize"); 2 }
    };
    std::process::exit(code);
}
