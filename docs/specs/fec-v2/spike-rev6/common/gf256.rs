//! (Copied from ../wasm-size/gf256; used as the oracle.) Throwaway prototype: systematic Cauchy RS over GF(2^8) whose first repair
//! row is all ones (so single-erasure recovery is plain XOR). Research only.
#![allow(clippy::needless_range_loop)]

const fn tables() -> ([u8; 512], [u8; 256]) {
    let mut exp = [0u8; 512];
    let mut log = [0u8; 256];
    let mut x: u16 = 1;
    let mut i = 0;
    while i < 255 {
        exp[i] = x as u8;
        log[x as usize] = i as u8;
        x <<= 1;
        if x & 0x100 != 0 { x ^= 0x11d; }
        i += 1;
    }
    let mut j = 255;
    while j < 512 { exp[j] = exp[j - 255]; j += 1; }
    (exp, log)
}
const T: ([u8; 512], [u8; 256]) = tables();
static EXP: [u8; 512] = T.0;
static LOG: [u8; 256] = T.1;
#[inline] pub fn mul(a: u8, b: u8) -> u8 { if a == 0 || b == 0 { 0 } else { EXP[LOG[a as usize] as usize + LOG[b as usize] as usize] } }
#[inline] pub fn inv(a: u8) -> u8 { EXP[255 - LOG[a as usize] as usize] }
/// dst ^= c * src
fn muladd(dst: &mut [u8], src: &[u8], c: u8) {
    if c == 0 { return; }
    if c == 1 { for (d, s) in dst.iter_mut().zip(src) { *d ^= *s; } return; }
    let lc = LOG[c as usize] as usize;
    for (d, s) in dst.iter_mut().zip(src) { if *s != 0 { *d ^= EXP[lc + LOG[*s as usize] as usize]; } }
}
/// Repair coefficient for repair row i (< r), source column j (< k).
/// Cauchy 1/(x_i + y_j), x_i = i, y_j = r + j, rows/cols normalised so that
/// row 0 and column 0 are all ones (scaling keeps every square submatrix
/// nonsingular, so the code stays MDS). Requires k + r <= 256.
pub fn coef(i: usize, j: usize, r: usize) -> u8 {
    let c = |i: usize, j: usize| inv((i as u8) ^ ((r + j) as u8));
    // a_ij = c(i,j) * c(0,0) / (c(0,j) * c(i,0))
    mul(mul(c(i, j), c(0, 0)), inv(mul(c(0, j), c(i, 0))))
}
pub fn encode(src: &[&[u8]], r: usize, len: usize) -> Vec<Vec<u8>> {
    let mut out = vec![vec![0u8; len]; r];
    for i in 0..r { for (j, s) in src.iter().enumerate() { muladd(&mut out[i], s, coef(i, j, r)); } }
    out
}
/// shards: k sources then r repairs, None = erased. Restores sources in place.
pub fn decode(shards: &mut [Option<Vec<u8>>], k: usize, r: usize, len: usize) -> bool {
    let missing: Vec<usize> = (0..k).filter(|&j| shards[j].is_none()).collect();
    let e = missing.len();
    if e == 0 { return true; }
    let rows: Vec<usize> = (0..r).filter(|&i| shards[k + i].is_some()).take(e).collect();
    if rows.len() < e { return false; }
    // b_i = repair_i - sum over known sources
    let mut b: Vec<Vec<u8>> = rows.iter().map(|&i| {
        let mut v = shards[k + i].clone().unwrap();
        for j in 0..k { if let Some(s) = &shards[j] { muladd(&mut v, s, coef(i, j, r)); } }
        v
    }).collect();
    // e x e matrix A[a][m] = coef(rows[a], missing[m]); Gauss-Jordan
    let mut a: Vec<Vec<u8>> = rows.iter().map(|&i| missing.iter().map(|&j| coef(i, j, r)).collect()).collect();
    for col in 0..e {
        let p = (col..e).find(|&x| a[x][col] != 0).unwrap();
        a.swap(col, p); b.swap(col, p);
        let iv = inv(a[col][col]);
        for x in 0..e { a[col][x] = mul(a[col][x], iv); }
        let (bc, rest) = { let t = b[col].clone(); (t, ()) }; let _ = rest;
        let mut bc = bc; for v in bc.iter_mut() { *v = mul(*v, iv); } b[col] = bc.clone();
        for row in 0..e {
            if row != col && a[row][col] != 0 {
                let f = a[row][col];
                for x in 0..e { let t = mul(f, a[col][x]); a[row][x] ^= t; }
                muladd(&mut b[row], &bc, f);
            }
        }
    }
    for (m, &j) in missing.iter().enumerate() { shards[j] = Some(core::mem::take(&mut b[m])); }
    let _ = len;
    true
}

/// Our normalised Cauchy encode matrix in ISA-L layout: (k+r) x k, identity on top,
/// then r repair rows (row 0 all ones). `mutate` perturbs one coefficient (test-bites proof).
pub fn ours_matrix(k: usize, r: usize, mutate: Option<(usize, usize, u8)>) -> Vec<u8> {
    let mut m = vec![0u8; (k + r) * k];
    for j in 0..k { m[j * k + j] = 1; }
    for i in 0..r { for j in 0..k { m[(k + i) * k + j] = coef(i, j, r); } }
    if let Some((i, j, x)) = mutate.filter(|&(i, _, _)| i < r) { m[(k + i) * k + j] ^= x; }
    m
}
