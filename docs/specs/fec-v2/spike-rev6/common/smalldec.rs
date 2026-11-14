// SPDX-License-Identifier: MIT OR Apache-2.0
//! Decode through an e x e system instead of rusty_erasure's k x k inversion.
//!
//! rusty_erasure's `Coder::recover` inverts a k x k matrix whatever the loss count, which costs
//! O(k^3) (190 us at k = 200 for ONE loss). Here we solve only for the e missing sources:
//!   missing = inv(A_m) * (repair_used + A_k * known)           (GF(2^8): minus == plus)
//! which is one linear map from [known sources, used repairs] (k shards) to the e missing
//! sources. We hand that map to rusty_erasure as the parity block of a custom matrix, so the
//! SIMD kernel does all the per-byte work. Only public rusty_erasure API is used.
use super::gf256::{coef, inv, mul};
use rusty_erasure::{coder, Matrix};

/// `shards`: k sources then r repairs, None = erased. Returns (source index, bytes) for each
/// missing source, or None when fewer than e repairs are present.
pub fn decode_ee(k: usize, r: usize, shards: &[Option<&[u8]>], len: usize,
                 coef_of: &dyn Fn(usize, usize) -> u8) -> Option<Vec<(usize, Vec<u8>)>> {
    let missing: Vec<usize> = (0..k).filter(|&j| shards[j].is_none()).collect();
    let e = missing.len();
    if e == 0 { return Some(vec![]); }
    let rows: Vec<usize> = (0..r).filter(|&i| shards[k + i].is_some()).take(e).collect();
    if rows.len() < e { return None; }
    let known: Vec<usize> = (0..k).filter(|&j| shards[j].is_some()).collect();
    if e == 1 && rows[0] == 0 && (0..k).all(|j| coef_of(0, j) == 1) {
        // repair 0 is plain XOR: no matrix work at all
        let mut out = vec![0u8; len];
        let mut srcs: Vec<&[u8]> = known.iter().map(|&j| shards[j].unwrap()).collect();
        srcs.push(shards[k].unwrap());
        if srcs.len() == 1 { out.copy_from_slice(srcs[0]); }
        else { rusty_erasure::raid::xor_gen(&srcs, &mut out).ok()?; }
        return Some(vec![(missing[0], out)]);
    }
    // inverse of the e x e matrix A_m[a][b] = coef(rows[a], missing[b]) (Gauss-Jordan)
    let mut a: Vec<Vec<u8>> = rows.iter().map(|&i| missing.iter().map(|&j| coef_of(i, j)).collect()).collect();
    let mut iv: Vec<Vec<u8>> = (0..e).map(|x| (0..e).map(|y| (x == y) as u8).collect()).collect();
    for c in 0..e {
        let p = (c..e).find(|&x| a[x][c] != 0)?;
        a.swap(c, p); iv.swap(c, p);
        let s = inv(a[c][c]);
        for x in 0..e { a[c][x] = mul(a[c][x], s); iv[c][x] = mul(iv[c][x], s); }
        for row in 0..e {
            if row != c && a[row][c] != 0 {
                let f = a[row][c];
                for x in 0..e { let t = mul(f, a[c][x]); a[row][x] ^= t; let t = mul(f, iv[c][x]); iv[row][x] ^= t; }
            }
        }
    }
    // parity block P (e x k): columns = known sources, then the e used repairs
    let nk = known.len();
    let mut m = vec![0u8; (k + e) * k];
    for j in 0..k { m[j * k + j] = 1; }
    let ak: Vec<Vec<u8>> = rows.iter().map(|&i| known.iter().map(|&l| coef_of(i, l)).collect()).collect();
    for j in 0..e {
        for c in 0..nk {
            let mut s = 0u8;
            for x in 0..e { s ^= mul(iv[j][x], ak[x][c]); }
            m[(k + j) * k + c] = s;
        }
        for x in 0..e { m[(k + j) * k + nk + x] = iv[j][x]; }
    }
    let cd = coder(Matrix::from_bytes(k + e, k, m).ok()?).ok()?;
    let mut inputs: Vec<&[u8]> = known.iter().map(|&l| shards[l].unwrap()).collect();
    inputs.extend(rows.iter().map(|&i| shards[k + i].unwrap()));
    let mut out = vec![vec![0u8; len]; e];
    let mut om: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
    cd.encode(&inputs, &mut om).ok()?;
    Some(missing.into_iter().zip(out).collect())
}

pub fn decode_ee_ours(k: usize, r: usize, shards: &[Option<&[u8]>], len: usize) -> Option<Vec<(usize, Vec<u8>)>> {
    decode_ee(k, r, shards, len, &|i, j| coef(i, j, r))
}
