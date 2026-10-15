use std::time::Instant;
fn main() {
    for &(k, r, len, it) in &[(10usize, 1usize, 128usize, 20000u32), (10, 2, 1180, 5000), (40, 8, 1180, 500), (200, 40, 1180, 20), (200, 20, 1180, 20)] {
        let src: Vec<Vec<u8>> = (0..k).map(|i| (0..len).map(|x| ((i * 37 + x * 11 + 5) % 256) as u8).collect()).collect();
        let refs: Vec<&[u8]> = src.iter().map(|v| &v[..]).collect();
        let t = Instant::now();
        let mut rep = vec![];
        for _ in 0..it { rep = gf256_probe::encode(&refs, r, len); }
        let te = t.elapsed() / it;
        let t = Instant::now();
        for _ in 0..it {
            let mut sh: Vec<Option<Vec<u8>>> = src.iter().cloned().map(Some).chain(rep.iter().cloned().map(Some)).collect();
            for m in 0..r { sh[m] = None; }
            assert!(gf256_probe::decode(&mut sh, k, r, len));
        }
        println!("native gf256 k={k} r={r} len={len}: encode {:?}, decode(r lost) {:?}", te, t.elapsed() / it);
    }
}
