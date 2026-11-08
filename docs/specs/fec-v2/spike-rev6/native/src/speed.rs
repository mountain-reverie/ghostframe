//! Measurement B: native encode / decode time per (k, r), 1180-byte symbols.
//! Output: TSV  candidate  op  k  r  lost  median_us  p95_us
use super::*;
use raptorq::{Decoder as RqDec, Encoder as RqEnc};

const KS: [usize; 9] = [1, 2, 4, 10, 20, 40, 64, 128, 200];
const RS: [usize; 4] = [1, 2, 4, 8];
const ITERS: usize = 200;

fn emit(cand: &str, op: &str, k: usize, r: usize, lost: usize, m: (f64, f64)) {
    println!("{cand}\t{op}\t{k}\t{r}\t{lost}\t{:.2}\t{:.2}", m.0, m.1);
}

pub fn run() {
    let probe = ours_coder(10, 2);
    eprintln!("rusty_erasure kernel set: {}", probe.kernels().name);
    for &k in &KS {
        for &r in &RS {
            let src = make_sources(k, SYM, 1);
            let refs: Vec<&[u8]> = src.iter().map(|v| &v[..]).collect();
            let losses: Vec<usize> = if r == 1 { vec![1] } else { vec![1, r] };

            // ---- rusty_erasure, our XOR-first matrix, and its built-in Cauchy
            for (name, c) in [("rusty-ours", ours_coder(k, r)), ("rusty-cauchy", builtin_coder(k, r))] {
                let rep = rusty_encode(&c, &src);
                let mut out = vec![vec![0u8; SYM]; r];
                emit(name, "encode", k, r, 0, bench(20, ITERS, || (), |_| {
                    let mut m: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
                    c.encode(&refs, &mut m).unwrap();
                }));
                for &lost in &losses {
                    let all: Vec<&[u8]> = src.iter().chain(rep.iter()).map(|v| &v[..]).collect();
                    let shards: Vec<Option<&[u8]>> = (0..k + r).map(|i| if i < lost { None } else { Some(all[i]) }).collect();
                    let rebuild: Vec<usize> = (0..lost).collect();
                    let mut o = vec![vec![0u8; SYM]; lost];
                    emit(name, "decode_warm", k, r, lost, bench(20, ITERS, || (), |_| {
                        let mut m: Vec<&mut [u8]> = o.iter_mut().map(|v| &mut v[..]).collect();
                        c.recover(&shards, &rebuild, &mut m).unwrap();
                    }));
                    assert!(o.iter().enumerate().all(|(i, v)| v[..] == *all[i]));
                    if name == "rusty-ours" {
                        // cold: matrix + coder built per block, as when k varies per block
                        emit(name, "decode_cold", k, r, lost, bench(20, ITERS, || (), |_| {
                            let c2 = ours_coder(k, r);
                            let mut m: Vec<&mut [u8]> = o.iter_mut().map(|v| &mut v[..]).collect();
                            c2.recover(&shards, &rebuild, &mut m).unwrap();
                        }));
                    }
                    if name == "rusty-ours" {
                        // e x e system solved by us, per-byte work in rusty_erasure's kernel (cold: all included)
                        emit(name, "decode_ee_cold", k, r, lost, bench(20, ITERS, || (), |_| {
                            let g = smalldec::decode_ee_ours(k, r, &shards, SYM).unwrap();
                            std::hint::black_box(&g);
                        }));
                        let g = smalldec::decode_ee_ours(k, r, &shards, SYM).unwrap();
                        assert!(g.iter().all(|(j, b)| b[..] == *all[*j]));
                    }
                    if name == "rusty-ours" && lost == 1 && k >= 2 {
                        // single-loss fast path: plain XOR of the k-1 surviving sources and repair 0
                        let surv: Vec<&[u8]> = (1..k).map(|j| all[j]).chain(std::iter::once(all[k])).collect();
                        let mut x = vec![0u8; SYM];
                        emit(name, "decode_xor_fastpath", k, r, 1, bench(20, ITERS, || (), |_| {
                            rusty_erasure::raid::xor_gen(&surv, &mut x).unwrap();
                        }));
                        assert!(x[..] == *all[0]);
                    }
                }
            }

            // ---- in-house scalar oracle
            {
                let rep = gf256::encode(&refs, r, SYM);
                emit("gf256-scalar", "encode", k, r, 0, bench(5, ITERS, || (), |_| { std::hint::black_box(gf256::encode(&refs, r, SYM)); }));
                for &lost in &losses {
                    let full: Vec<Option<Vec<u8>>> = src.iter().chain(rep.iter()).cloned().map(Some).collect();
                    emit("gf256-scalar", "decode", k, r, lost, bench(5, ITERS,
                        || { let mut s = full.clone(); for m in 0..lost { s[m] = None; } s },
                        |mut s| { assert!(gf256::decode(&mut s, k, r, SYM)); }));
                }
            }

            // ---- raptorq (decode gets r+3 repairs so it can finish; it stops as soon as it can)
            {
                let oti = rq_oti(k * SYM);
                let data: Vec<u8> = src.concat();
                emit("raptorq", "encode", k, r, 0, bench(5, ITERS, || (), |_| {
                    std::hint::black_box(RqEnc::new(&data, oti).get_encoded_packets(r as u32));
                }));
                let pk = RqEnc::new(&data, oti).get_encoded_packets((r + 3) as u32);
                for &lost in &losses {
                    let mut fails = 0u32;
                    let m = bench(5, ITERS, || pk[lost..].to_vec(), |p| {
                        let mut d = RqDec::new(oti);
                        let mut res = None;
                        for x in p { if let Some(o) = d.decode(x) { res = Some(o); break; } }
                        if res.as_deref() != Some(&data[..]) { fails += 1; }
                    });
                    emit("raptorq", &format!("decode(fails={fails})"), k, r, lost, m);
                }
            }
        }
    }
}
