use reed_solomon_simd::{ReedSolomonEncoder, ReedSolomonDecoder};
use std::time::Instant;
fn run(k: usize, r: usize, len: usize, lost: usize, iters: u32) {
    let data: Vec<Vec<u8>> = (0..k).map(|i| (0..len).map(|j| ((i*131+j*7) & 255) as u8).collect()).collect();
    let t = Instant::now();
    let mut rec = vec![];
    for _ in 0..iters { let mut e = ReedSolomonEncoder::new(k, r, len).unwrap(); for s in &data { e.add_original_shard(s).unwrap(); } let res = e.encode().unwrap(); rec = res.recovery_iter().map(|s| s.to_vec()).collect(); }
    let te = t.elapsed() / iters;
    let t = Instant::now();
    for _ in 0..iters { let mut d = ReedSolomonDecoder::new(k, r, len).unwrap(); for i in lost..k { d.add_original_shard(i, &data[i]).unwrap(); } for j in 0..lost { d.add_recovery_shard(j, &rec[j]).unwrap(); } let res = d.decode().unwrap(); assert_eq!(res.restored_original_iter().count(), lost); }
    let td = t.elapsed() / iters;
    println!("native k={k} r={r} len={len} lost={lost}: encode {:?}, decode {:?}", te, td);
}
fn main() { run(10,1,128,1,2000); run(10,2,1180,2,1000); run(12,3,1180,3,500); run(40,8,1180,8,200); run(200,40,1180,40,50); run(200,40,1180,1,50); }
