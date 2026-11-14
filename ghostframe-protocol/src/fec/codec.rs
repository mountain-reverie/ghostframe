// SPDX-License-Identifier: MIT OR Apache-2.0
//! Encode repair rows and rebuild missing sources (rev 6 §3.2).
//!
//! Byte work runs on `rusty_erasure`'s SIMD kernels through a custom matrix.
//! Decode never calls `Coder::recover`: that inverts the full k x k matrix even
//! for one loss (280 us at k = 200 natively). Instead it solves only the e x e
//! system for the e missing sources and hands the resulting linear map to the
//! kernel as the parity block of a matrix; one loss with repair row 0 present
//! is a plain XOR.
//!
//! Sources may be shorter than the block's symbol: they are zero-padded. A
//! rebuilt source comes back at the full symbol length; the coded symbol's own
//! length prefix (`frame`) says how much of it is real.
use super::coef::{coef, max_row_for};
use rusty_erasure::{coder, gf, Matrix};
use std::borrow::Cow;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FecError {
    #[error("block has no sources")]
    NoSources,
    #[error("block has {0} sources; at most 255 fit with a repair row")]
    TooManySources(usize),
    #[error("repair submatrix is singular")]
    Singular,
    #[error("repair row {row} is outside a block of {k} sources (k + row + 1 > 256)")]
    RowOutOfRange { k: usize, row: u8 },
    #[error("repair row {0} given twice")]
    DuplicateRow(u8),
    #[error("{missing} sources missing but only {repairs} repair symbols")]
    NotEnoughRepairs { missing: usize, repairs: usize },
    #[error("symbol is {got} bytes, expected at most {expected}")]
    SymbolLength { expected: usize, got: usize },
    #[error("erasure coder: {0}")]
    Coder(String),
}

fn check_rows(k: usize, rows: impl Iterator<Item = u8>) -> Result<(), FecError> {
    if k >= 256 {
        return Err(FecError::TooManySources(k));
    }
    let max = max_row_for(k).ok_or(FecError::NoSources)?;
    let mut seen = [false; 256];
    for row in rows {
        if row > max {
            return Err(FecError::RowOutOfRange { k, row });
        }
        if std::mem::replace(&mut seen[row as usize], true) {
            return Err(FecError::DuplicateRow(row));
        }
    }
    Ok(())
}

fn padded(s: &[u8], len: usize) -> Cow<'_, [u8]> {
    if s.len() == len {
        Cow::Borrowed(s)
    } else {
        let mut v = s.to_vec();
        v.resize(len, 0);
        Cow::Owned(v)
    }
}

/// Run `parity_rows` (each `k` coefficients, row-major) over `inputs` (`k`
/// equal-length shards) on the SIMD kernel.
fn apply(inputs: &[&[u8]], parity_rows: &[u8], len: usize) -> Result<Vec<Vec<u8>>, FecError> {
    let k = inputs.len();
    let p = parity_rows.len() / k;
    let mut m = vec![0u8; (k + p) * k];
    for j in 0..k {
        m[j * k + j] = 1;
    }
    m[k * k..].copy_from_slice(parity_rows);
    let matrix = Matrix::from_bytes(k + p, k, m).map_err(|e| FecError::Coder(e.to_string()))?;
    let cd = coder(matrix).map_err(|e| FecError::Coder(e.to_string()))?;
    let mut out = vec![vec![0u8; len]; p];
    let mut refs: Vec<&mut [u8]> = out.iter_mut().map(|v| v.as_mut_slice()).collect();
    cd.encode(inputs, &mut refs)
        .map_err(|e| FecError::Coder(e.to_string()))?;
    Ok(out)
}

/// Repair symbols for `rows` over the block's `sources` (in `idx_in_block`
/// order). Each output is as long as the longest source.
pub fn encode_repairs(sources: &[&[u8]], rows: &[u8]) -> Result<Vec<Vec<u8>>, FecError> {
    let k = sources.len();
    check_rows(k, rows.iter().copied())?;
    let len = sources.iter().map(|s| s.len()).max().unwrap_or(0);
    if rows.is_empty() || len == 0 {
        return Ok(vec![vec![0u8; len]; rows.len()]);
    }
    let pads: Vec<Cow<'_, [u8]>> = sources.iter().map(|s| padded(s, len)).collect();
    let inputs: Vec<&[u8]> = pads.iter().map(|c| c.as_ref()).collect();
    let mut coefs = Vec::with_capacity(rows.len() * k);
    for &i in rows {
        coefs.extend((0..k).map(|j| coef(i, j as u8)));
    }
    apply(&inputs, &coefs, len)
}

/// Rebuild every missing source. `sources[j]` is `Some` for each source of the
/// block that arrived; `repairs` are the `(row, bytes)` that arrived, all the
/// block's symbol length. Returns `(j, bytes)` per missing source, each the
/// symbol length.
pub fn recover(
    sources: &[Option<&[u8]>],
    repairs: &[(u8, &[u8])],
) -> Result<Vec<(usize, Vec<u8>)>, FecError> {
    let k = sources.len();
    check_rows(k, repairs.iter().map(|&(r, _)| r))?;
    let missing: Vec<usize> = (0..k).filter(|&j| sources[j].is_none()).collect();
    let e = missing.len();
    if e == 0 {
        return Ok(Vec::new());
    }
    if repairs.len() < e {
        return Err(FecError::NotEnoughRepairs {
            missing: e,
            repairs: repairs.len(),
        });
    }
    let len = repairs[0].1.len();
    for &(_, r) in repairs {
        if r.len() != len {
            return Err(FecError::SymbolLength {
                expected: len,
                got: r.len(),
            });
        }
    }
    for s in sources.iter().flatten() {
        if s.len() > len {
            return Err(FecError::SymbolLength {
                expected: len,
                got: s.len(),
            });
        }
    }
    // Prefer row 0 so a single loss takes the XOR path.
    let mut used: Vec<(u8, &[u8])> = repairs.to_vec();
    used.sort_by_key(|&(r, _)| r);
    used.truncate(e);

    if e == 1 && used[0].0 == 0 {
        let mut out = used[0].1.to_vec();
        for s in sources.iter().flatten() {
            for (o, b) in out.iter_mut().zip(s.iter()) {
                *o ^= b;
            }
        }
        return Ok(vec![(missing[0], out)]);
    }

    // Invert A_m (e x e), A_m[a][b] = coef(used[a].row, missing[b]).
    let mut a: Vec<Vec<u8>> = used
        .iter()
        .map(|&(i, _)| missing.iter().map(|&j| coef(i, j as u8)).collect())
        .collect();
    let mut iv: Vec<Vec<u8>> = (0..e)
        .map(|x| (0..e).map(|y| u8::from(x == y)).collect())
        .collect();
    for c in 0..e {
        let p = (c..e).find(|&x| a[x][c] != 0).ok_or(FecError::Singular)?;
        a.swap(c, p);
        iv.swap(c, p);
        let s = gf::inv(a[c][c]);
        for x in 0..e {
            a[c][x] = gf::mul(a[c][x], s);
            iv[c][x] = gf::mul(iv[c][x], s);
        }
        let (ac, ic) = (a[c].clone(), iv[c].clone());
        for row in 0..e {
            if row != c && a[row][c] != 0 {
                let f = a[row][c];
                for x in 0..e {
                    a[row][x] ^= gf::mul(f, ac[x]);
                    iv[row][x] ^= gf::mul(f, ic[x]);
                }
            }
        }
    }
    // missing = inv(A_m) * (repair + A_k * known): one linear map over the
    // inputs [known sources..., used repairs...] (k shards in total).
    let known: Vec<usize> = (0..k).filter(|&j| sources[j].is_some()).collect();
    let mut rows = vec![0u8; e * k];
    for j in 0..e {
        for (c, &l) in known.iter().enumerate() {
            let mut s = 0u8;
            for x in 0..e {
                s ^= gf::mul(iv[j][x], coef(used[x].0, l as u8));
            }
            rows[j * k + c] = s;
        }
        for x in 0..e {
            rows[j * k + known.len() + x] = iv[j][x];
        }
    }
    let pads: Vec<Cow<'_, [u8]>> = known
        .iter()
        .map(|&l| padded(sources[l].expect("known"), len))
        .collect();
    let mut inputs: Vec<&[u8]> = pads.iter().map(|c| c.as_ref()).collect();
    inputs.extend(used.iter().map(|&(_, r)| r));
    let out = apply(&inputs, &rows, len)?;
    Ok(missing.into_iter().zip(out).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fec::oracle;
    use proptest::prelude::*;

    fn block(k: usize, len: usize, seed: u8) -> Vec<Vec<u8>> {
        (0..k)
            .map(|j| {
                (0..len)
                    .map(|b| (b as u8).wrapping_mul(13) ^ (j as u8).wrapping_mul(7) ^ seed)
                    .collect()
            })
            .collect()
    }

    fn all_subsets(n: usize, size: usize) -> Vec<Vec<usize>> {
        let mut out = Vec::new();
        let mut cur = Vec::new();
        fn go(
            start: usize,
            n: usize,
            size: usize,
            cur: &mut Vec<usize>,
            out: &mut Vec<Vec<usize>>,
        ) {
            if cur.len() == size {
                out.push(cur.clone());
                return;
            }
            for x in start..n {
                cur.push(x);
                go(x + 1, n, size, cur, out);
                cur.pop();
            }
        }
        go(0, n, size, &mut cur, &mut out);
        out
    }

    /// Erase every pattern of up to r symbols (sources and repairs) and check
    /// the rebuilt sources are exact.
    fn exhaustive(k: usize, r: usize) {
        let srcs = block(k, 40, k as u8);
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let rows: Vec<u8> = (0..r as u8).collect();
        let reps = encode_repairs(&refs, &rows).unwrap();
        for size in 1..=r {
            for erased in all_subsets(k + r, size) {
                let have: Vec<Option<&[u8]>> = (0..k)
                    .map(|j| (!erased.contains(&j)).then_some(refs[j]))
                    .collect();
                let rin: Vec<(u8, &[u8])> = (0..r)
                    .filter(|i| !erased.contains(&(k + i)))
                    .map(|i| (i as u8, reps[i].as_slice()))
                    .collect();
                for (j, bytes) in recover(&have, &rin).unwrap() {
                    assert_eq!(bytes, srcs[j], "k={k} r={r} erased={erased:?}");
                }
            }
        }
    }

    #[test]
    fn exhaustive_small_blocks() {
        for (k, r) in [(1, 1), (2, 1), (4, 2), (5, 1), (10, 2), (10, 4), (12, 3)] {
            exhaustive(k, r);
        }
    }

    #[test]
    fn repair_bytes_match_the_oracle() {
        for (k, r) in [
            (1, 1),
            (3, 2),
            (10, 4),
            (40, 8),
            (64, 8),
            (200, 8),
            (247, 9),
        ] {
            let srcs = block(k, 1180, 3);
            let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
            let rows: Vec<u8> = (0..r as u8).collect();
            assert_eq!(
                encode_repairs(&refs, &rows).unwrap(),
                oracle::encode(&refs, &rows),
                "k={k} r={r}"
            );
        }
    }

    #[test]
    fn row_zero_is_xor() {
        let srcs = block(9, 100, 1);
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let x = srcs.iter().fold(vec![0u8; 100], |mut acc, s| {
            acc.iter_mut().zip(s).for_each(|(a, b)| *a ^= b);
            acc
        });
        assert_eq!(encode_repairs(&refs, &[0]).unwrap()[0], x);
    }

    #[test]
    fn rows_can_be_sparse_and_late() {
        // A block that planned r = 2 then served a request for rows 5 and 9.
        let srcs = block(20, 64, 9);
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let rows = [5u8, 9];
        let reps = encode_repairs(&refs, &rows).unwrap();
        assert_eq!(reps, oracle::encode(&refs, &rows));
        let mut have: Vec<Option<&[u8]>> = refs.iter().map(|s| Some(*s)).collect();
        have[3] = None;
        have[17] = None;
        let got = recover(&have, &[(5, &reps[0]), (9, &reps[1])]).unwrap();
        assert_eq!(got, vec![(3, srcs[3].clone()), (17, srcs[17].clone())]);
    }

    #[test]
    fn short_sources_are_zero_padded() {
        let srcs = [vec![1u8; 100], vec![2u8; 30], vec![3u8; 1180]];
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let reps = encode_repairs(&refs, &[0, 1]).unwrap();
        assert!(reps.iter().all(|r| r.len() == 1180));
        let have = [Some(refs[0]), None, Some(refs[2])];
        let got = recover(&have, &[(1, &reps[1])]).unwrap();
        assert_eq!(&got[0].1[..30], &srcs[1][..]);
        assert!(got[0].1[30..].iter().all(|&b| b == 0));
    }

    #[test]
    fn too_many_losses_fail_cleanly() {
        let srcs = block(10, 50, 4);
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let reps = encode_repairs(&refs, &[0, 1]).unwrap();
        let mut have: Vec<Option<&[u8]>> = refs.iter().map(|s| Some(*s)).collect();
        have[0] = None;
        have[1] = None;
        have[2] = None;
        assert_eq!(
            recover(&have, &[(0, &reps[0]), (1, &reps[1])]),
            Err(FecError::NotEnoughRepairs {
                missing: 3,
                repairs: 2
            })
        );
    }

    #[test]
    fn rejected_inputs() {
        let s: &[u8] = &[1u8; 8];
        assert_eq!(encode_repairs(&[], &[0]), Err(FecError::NoSources));
        assert_eq!(
            encode_repairs(&[s; 256], &[0]),
            Err(FecError::TooManySources(256))
        );
        assert_eq!(
            encode_repairs(&[s; 200], &[56]),
            Err(FecError::RowOutOfRange { k: 200, row: 56 })
        );
        assert_eq!(
            encode_repairs(&[s; 2], &[1, 1]),
            Err(FecError::DuplicateRow(1))
        );
        let long = [0u8; 9];
        assert_eq!(
            recover(&[Some(&long[..]), None], &[(0, s)]),
            Err(FecError::SymbolLength {
                expected: 8,
                got: 9
            })
        );
    }

    fn rec_exact(k: usize, len: usize, rows: &[u8], erase: &[usize], order: &[usize]) {
        let srcs = block(k, len, 9);
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let reps = encode_repairs(&refs, rows).unwrap();
        let mut have: Vec<Option<&[u8]>> = refs.iter().map(|s| Some(*s)).collect();
        for &j in erase {
            have[j] = None;
        }
        let rin: Vec<(u8, &[u8])> = order
            .iter()
            .map(|&x| (rows[x], reps[x].as_slice()))
            .collect();
        let got = recover(&have, &rin).unwrap();
        assert_eq!(got.len(), erase.len());
        for (j, bytes) in got {
            assert_eq!(bytes, srcs[j]);
        }
    }

    #[test]
    fn repairs_out_of_order_and_surplus() {
        rec_exact(20, 40, &[0, 5, 9], &[3, 17], &[2, 1, 0]);
    }

    #[test]
    fn sparse_rows_with_surplus() {
        rec_exact(10, 40, &[2, 7, 11], &[4], &[2, 0, 1]);
    }

    #[test]
    fn full_256_symbol_block() {
        let rows: Vec<u8> = (0..128).collect();
        let erase: Vec<usize> = (0..128).collect();
        let order: Vec<usize> = (0..128).collect();
        rec_exact(128, 64, &rows, &erase, &order);
    }

    #[test]
    fn xor_fast_path_with_short_known_sources() {
        let lens = [100usize, 30, 1180, 7];
        let srcs: Vec<Vec<u8>> = lens
            .iter()
            .map(|&l| block(1, l, l as u8).remove(0))
            .collect();
        let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
        let reps = encode_repairs(&refs, &[0]).unwrap();
        let have: Vec<Option<&[u8]>> = vec![None, Some(refs[1]), Some(refs[2]), Some(refs[3])];
        let got = recover(&have, &[(0, &reps[0])]).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, 0);
        assert_eq!(&got[0].1[..100], &srcs[0][..]);
        assert!(got[0].1[100..].iter().all(|&b| b == 0));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(200))]
        #[test]
        fn random_erasures_recover_and_agree_with_oracle(
            k in 1usize..=200, extra in 0usize..=8, seed in any::<u64>(), len in 1usize..=1200,
        ) {
            let r = (1 + extra).min(256 - k);
            let srcs: Vec<Vec<u8>> = (0..k).map(|j| (0..len).map(|b| ((seed >> (j % 56)) as u8) ^ (b as u8) ^ (j as u8)).collect()).collect();
            let refs: Vec<&[u8]> = srcs.iter().map(|v| v.as_slice()).collect();
            let rows: Vec<u8> = (0..r as u8).collect();
            let reps = encode_repairs(&refs, &rows).unwrap();
            prop_assert_eq!(&reps, &oracle::encode(&refs, &rows));
            // erase min(r, k) sources chosen from the seed
            let e = r.min(k);
            let mut idx: Vec<usize> = (0..k).collect();
            let mut s = seed;
            for x in 0..e { s = s.wrapping_mul(6364136223846793005).wrapping_add(1); let y = x + (s >> 33) as usize % (k - x); idx.swap(x, y); }
            let mut have: Vec<Option<&[u8]>> = refs.iter().map(|v| Some(*v)).collect();
            for &j in &idx[..e] { have[j] = None; }
            let rin: Vec<(u8, &[u8])> = rows.iter().copied().zip(reps.iter().map(|v| v.as_slice())).collect();
            let got = recover(&have, &rin).unwrap();
            prop_assert_eq!(got.len(), e);
            for (j, bytes) in got { prop_assert_eq!(&bytes, &srcs[j]); }
        }
    }
}
