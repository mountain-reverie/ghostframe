//! Measurement A: rusty_erasure with our XOR-first matrix against the gf256 oracle.
use super::*;

thread_local! { static MUT: std::cell::Cell<Option<(usize, usize, u8)>> = std::cell::Cell::new(None); }
struct Tally { checks: u64, fails: u64, by_kind: std::collections::BTreeMap<&'static str, u64> }
impl Tally {
    fn ok(&mut self, cond: bool, kind: &'static str) {
        self.checks += 1;
        if !cond { self.fails += 1; *self.by_kind.entry(kind).or_default() += 1; }
    }
}

/// All subsets of 0..n of size `e` (lexicographic).
fn subsets(n: usize, e: usize) -> Vec<Vec<usize>> {
    fn rec(start: usize, n: usize, e: usize, cur: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
        if cur.len() == e { out.push(cur.clone()); return; }
        for i in start..n { cur.push(i); rec(i + 1, n, e, cur, out); cur.pop(); }
    }
    let mut out = vec![]; rec(0, n, e, &mut vec![], &mut out); out
}

fn check_pattern(t: &mut Tally, c: &Coder, src: &[Vec<u8>], rep: &[Vec<u8>], erased: &[usize], k: usize, len: usize) {
    let all: Vec<&[u8]> = src.iter().chain(rep.iter()).map(|v| &v[..]).collect();
    let shards: Vec<Option<&[u8]>> = (0..all.len()).map(|i| if erased.contains(&i) { None } else { Some(all[i]) }).collect();
    // rebuild every erased shard (sources and repairs)
    match rusty_recover(c, &shards, erased, len) {
        Ok(out) => {
            let good = erased.iter().zip(&out).all(|(&i, o)| o[..] == *all[i]);
            t.ok(good, "decode_bytes");
            // and the oracle must agree on the sources it can restore
            let mut o: Vec<Option<Vec<u8>>> = (0..all.len()).map(|i| shards[i].map(|s| s.to_vec())).collect();
            let rr = rep.len();
            t.ok(gf256::decode(&mut o, k, rr, len) && (0..k).all(|j| o[j].as_deref() == Some(all[j])), "oracle_decode");
        }
        Err(_) => t.ok(false, "decode_err"),
    }
    // the e x e decoder (common/smalldec.rs) must restore every missing source too
    let r = rep.len();
    let cf = |i: usize, j: usize| { let mut v = gf256::coef(i, j, r); if let Some((mi, mj, x)) = MUT.with(|m| m.get()) { if i == mi && j == mj { v ^= x; } } v };
    let got = smalldec::decode_ee(k, r, &shards, len, &cf);
    t.ok(got.map_or(false, |g| g.len() == erased.iter().filter(|&&i| i < k).count() && g.iter().all(|(j, b)| b[..] == *all[*j])), "smalldec_bytes");
}

pub fn run(mutate_repair1: bool, mutate_row0: bool) -> i32 {
    let mutate = if mutate_repair1 { Some((1usize, 0usize, 1u8)) } else if mutate_row0 { Some((0, 0, 2)) } else { None };
    MUT.with(|m| m.set(mutate));
    let mut t = Tally { checks: 0, fails: 0, by_kind: Default::default() };
    let len = 64;
    let mut rg = rng(42);
    let report = |s: String| println!("{s}");
    // Exhaustive: every pattern of size <= r over the k+r shards.
    for &(k, r) in &[(2usize, 1usize), (4, 2), (5, 1), (10, 2), (10, 4), (12, 3), (20, 4)] {
        let c = coder(Matrix::from_bytes(k + r, k, gf256::ours_matrix(k, r, mutate)).unwrap()).unwrap();
        let src = make_sources(k, len, (k * 100 + r) as u64);
        let refs: Vec<&[u8]> = src.iter().map(|v| &v[..]).collect();
        let rep = rusty_encode(&c, &src);
        let orc = gf256::encode(&refs, r, len);
        t.ok(rep == orc, "encode_vs_oracle");
        let mut n = 0u64;
        for e in 1..=r { for p in subsets(k + r, e) { check_pattern(&mut t, &c, &src, &rep, &p, k, len); n += 1; } }
        // > r erasures must fail cleanly and leave the output untouched
        for e in (r + 1)..=(r + 2).min(k + r) {
            for _ in 0..200 {
                let mut p: Vec<usize> = (0..k + r).collect();
                use rand::seq::SliceRandom; p.shuffle(&mut rg); p.truncate(e); p.sort();
                let all: Vec<&[u8]> = src.iter().chain(rep.iter()).map(|v| &v[..]).collect();
                let sh: Vec<Option<&[u8]>> = (0..k + r).map(|i| if p.contains(&i) { None } else { Some(all[i]) }).collect();
                let mut out = vec![vec![0xAAu8; len]; p.len()];
                let mut m: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
                let res = c.recover(&sh, &p, &mut m);
                t.ok(res.is_err() && out.iter().all(|o| o.iter().all(|&b| b == 0xAA)), "over_r_clean_failure");
            }
        }
        report(format!("exhaustive k={k} r={r}: {n} patterns of size 1..={r}, plus {} over-r failures", 400));
    }
    // Random patterns (size 1..=r, half of them exactly r)
    for &(k, r) in &[(40usize, 8usize), (64, 8), (200, 8)] {
        let c = coder(Matrix::from_bytes(k + r, k, gf256::ours_matrix(k, r, mutate)).unwrap()).unwrap();
        let src = make_sources(k, len, (k * 100 + r) as u64);
        let refs: Vec<&[u8]> = src.iter().map(|v| &v[..]).collect();
        let rep = rusty_encode(&c, &src);
        t.ok(rep == gf256::encode(&refs, r, len), "encode_vs_oracle");
        for trial in 0..600 {
            let e = if trial % 2 == 0 { r } else { rg.gen_range(1..=r) };
            let mut p: Vec<usize> = (0..k + r).collect();
            use rand::seq::SliceRandom; p.shuffle(&mut rg); p.truncate(e); p.sort();
            check_pattern(&mut t, &c, &src, &rep, &p, k, len);
        }
        report(format!("random k={k} r={r}: 600 patterns of size 1..={r} (half exactly {r})"));
    }
    // r = 1 output equals the XOR of the sources, and row 0 is all ones, for the whole grid.
    for &k in &[1usize, 2, 4, 10, 20, 40, 64, 128, 200] {
        for &r in &[1usize, 2, 4, 8] {
            let c = coder(Matrix::from_bytes(k + r, k, gf256::ours_matrix(k, r, mutate)).unwrap()).unwrap();
            let src = make_sources(k, len, 7);
            let rep = rusty_encode(&c, &src);
            let mut x = vec![0u8; len];
            for s in &src { for (a, b) in x.iter_mut().zip(s) { *a ^= *b; } }
            t.ok(rep[0] == x, "row0_is_xor");
            t.ok((0..k).all(|j| c.matrix().get(k, j) == Some(1)), "row0_all_ones");
            t.ok(rep == gf256::encode(&src.iter().map(|v| &v[..]).collect::<Vec<_>>(), r, len), "encode_vs_oracle");
        }
    }
    println!("checks={} failures={} by_kind={:?}", t.checks, t.fails, t.by_kind);
    if mutate.is_some() {
        if t.fails > 0 { println!("MUTANT KILLED: the test bites ({} failing checks)", t.fails); 0 }
        else { println!("MUTANT SURVIVED: the test does not bite"); 1 }
    } else if t.fails == 0 { println!("PASS"); 0 } else { println!("FAIL"); 1 }
}
