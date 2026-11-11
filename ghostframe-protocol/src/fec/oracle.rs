//! Test-only scalar reference for the rev 6 code, independent of
//! `rusty_erasure`: its own GF(2^8)/0x11d tables, its own coefficient, a
//! byte-loop encode and a Gauss-Jordan decode. Production code must agree
//! with it byte for byte; see `codec` tests.
// Index loops mirror the textbook algebra on purpose: this is the reference.
#![allow(clippy::needless_range_loop)]

struct Tables {
    exp: [u8; 512],
    log: [u8; 256],
}

fn tables() -> &'static Tables {
    use std::sync::OnceLock;
    static T: OnceLock<Tables> = OnceLock::new();
    T.get_or_init(|| {
        let mut exp = [0u8; 512];
        let mut log = [0u8; 256];
        let mut x: u16 = 1;
        for i in 0..255 {
            exp[i] = x as u8;
            log[x as usize] = i as u8;
            x <<= 1;
            if x & 0x100 != 0 {
                x ^= 0x11d;
            }
        }
        for i in 255..512 {
            exp[i] = exp[i - 255];
        }
        Tables { exp, log }
    })
}

pub fn mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    let t = tables();
    t.exp[t.log[a as usize] as usize + t.log[b as usize] as usize]
}

pub fn inv(a: u8) -> u8 {
    assert_ne!(a, 0, "oracle inv(0)");
    let t = tables();
    t.exp[255 - t.log[a as usize] as usize]
}

/// Same definition as `fec::coef::coef`, computed with these tables.
pub fn coef(i: u8, j: u8) -> u8 {
    assert!(u16::from(i) + u16::from(j) <= 254);
    let c = |i: u8, j: u8| inv((255 - i) ^ j);
    mul(c(i, j), inv(c(0, j)))
}

fn muladd(dst: &mut [u8], src: &[u8], c: u8) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d ^= mul(*s, c);
    }
}

/// Repair rows `rows` over `sources`, each zero-padded to the longest.
pub fn encode(sources: &[&[u8]], rows: &[u8]) -> Vec<Vec<u8>> {
    let len = sources.iter().map(|s| s.len()).max().unwrap_or(0);
    rows.iter()
        .map(|&i| {
            let mut out = vec![0u8; len];
            for (j, s) in sources.iter().enumerate() {
                muladd(&mut out, s, coef(i, j as u8));
            }
            out
        })
        .collect()
}

/// Rebuild missing sources from repairs `(row, bytes)`, all `len` long.
/// Returns `None` when there are fewer repairs than missing sources.
pub fn decode(
    sources: &[Option<&[u8]>],
    repairs: &[(u8, &[u8])],
    len: usize,
) -> Option<Vec<(usize, Vec<u8>)>> {
    let missing: Vec<usize> = (0..sources.len())
        .filter(|&j| sources[j].is_none())
        .collect();
    let e = missing.len();
    if e == 0 {
        return Some(Vec::new());
    }
    if repairs.len() < e {
        return None;
    }
    let used = &repairs[..e];
    // b = repair - sum(coef * known source)
    let mut b: Vec<Vec<u8>> = used
        .iter()
        .map(|&(i, r)| {
            let mut v = r.to_vec();
            v.resize(len, 0);
            for (j, s) in sources.iter().enumerate() {
                if let Some(s) = s {
                    muladd(&mut v, s, coef(i, j as u8));
                }
            }
            v
        })
        .collect();
    let mut a: Vec<Vec<u8>> = used
        .iter()
        .map(|&(i, _)| missing.iter().map(|&j| coef(i, j as u8)).collect())
        .collect();
    for col in 0..e {
        let p = (col..e).find(|&x| a[x][col] != 0)?;
        a.swap(col, p);
        b.swap(col, p);
        let s = inv(a[col][col]);
        for x in 0..e {
            a[col][x] = mul(a[col][x], s);
        }
        for v in b[col].iter_mut() {
            *v = mul(*v, s);
        }
        let pivot_row = b[col].clone();
        let pivot_a = a[col].clone();
        for row in 0..e {
            if row != col && a[row][col] != 0 {
                let f = a[row][col];
                for x in 0..e {
                    a[row][x] ^= mul(f, pivot_a[x]);
                }
                muladd(&mut b[row], &pivot_row, f);
            }
        }
    }
    Some(missing.into_iter().zip(b).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn oracle_coef_agrees_with_production_everywhere() {
        for i in 0..=254u8 {
            for j in 0..=(254 - i) {
                assert_eq!(coef(i, j), crate::fec::coef::coef(i, j), "coef({i}, {j})");
            }
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(300))]
        /// Every square submatrix of rows x sources is nonsingular (MDS),
        /// for blocks anywhere in the valid range.
        #[test]
        fn every_submatrix_is_invertible(
            k in 1usize..=240,
            seed in any::<u64>(),
        ) {
            let max_rows = (256 - k).min(16);
            let mut rng = seed;
            let mut next = |m: usize| { rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407); ((rng >> 33) as usize) % m };
            let e = 1 + next(max_rows.min(k));
            let mut rows: Vec<u8> = (0..max_rows as u8).collect();
            let mut cols: Vec<usize> = (0..k).collect();
            for x in 0..e { let y = x + next(rows.len() - x); rows.swap(x, y); }
            for x in 0..e { let y = x + next(cols.len() - x); cols.swap(x, y); }
            // decode e random sources from e random rows: succeeds iff the submatrix is invertible
            let srcs: Vec<Vec<u8>> = (0..k).map(|j| vec![(j as u8).wrapping_mul(31) ^ 0x5a; 8]).collect();
            let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
            let used_rows = &rows[..e];
            let reps = encode(&refs, used_rows);
            let mut have: Vec<Option<&[u8]>> = refs.iter().map(|s| Some(*s)).collect();
            for &j in &cols[..e] { have[j] = None; }
            let rep_in: Vec<(u8, &[u8])> = used_rows.iter().copied().zip(reps.iter().map(|v| v.as_slice())).collect();
            let got = decode(&have, &rep_in, 8).expect("singular submatrix");
            for (j, bytes) in got { prop_assert_eq!(&bytes, &srcs[j]); }
        }
    }
}
