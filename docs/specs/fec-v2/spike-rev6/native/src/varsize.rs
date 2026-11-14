// SPDX-License-Identifier: MIT OR Apache-2.0
//! Measurement E: blocks that mix datagram lengths (1180 B except the last, 100 B).
use super::*;
use cu_fec::{EncodingSymbolId as Esi, Field, RlcConfig, RlcEncoder};
use raptorq::{Decoder as RqDec, Encoder as RqEnc};
use rand::seq::SliceRandom;

/// Pad a datagram into a fixed-size shard: [len u16 LE][data][zeros]. The prefix lets a lost short
/// symbol be restored with its true length (the length rides inside the coded bytes).
fn pad(d: &[u8], shard: usize) -> Vec<u8> {
    assert!(d.len() + 2 <= shard);
    let mut v = vec![0u8; shard];
    v[..2].copy_from_slice(&(d.len() as u16).to_le_bytes());
    v[2..2 + d.len()].copy_from_slice(d);
    v
}
fn unpad(s: &[u8]) -> &[u8] { let n = u16::from_le_bytes([s[0], s[1]]) as usize; &s[2..2 + n] }

pub fn run() -> i32 {
    let (k, r) = (10usize, 2usize);
    let shard = SYM + 2; // 1180 B datagram payload + 2 B length prefix
    let mut rg = rng(3);
    let mut sizes = vec![SYM; k]; sizes[k - 1] = 100;
    let dgrams: Vec<Vec<u8>> = sizes.iter().enumerate().map(|(i, &n)| make_sources(1, n, i as u64).pop().unwrap()).collect();

    // 1. what each library does with unequal lengths
    let c = ours_coder(k, r);
    let uneq: Vec<&[u8]> = dgrams.iter().map(|v| &v[..]).collect();
    let mut out = vec![vec![0u8; SYM]; r];
    let mut om: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
    let e = c.encode(&uneq, &mut om);
    println!("rusty_erasure encode with unequal lengths: {:?}", e);
    assert!(e.is_err());
    let cfg = RlcConfig::new(SYM, 20, Field::Gf256).unwrap();
    let mut enc = Box::new(RlcEncoder::<1200, 64>::new(cfg, Esi::new(0)).unwrap());
    println!("cu-fec push_source(100 B) into a 1180 B config: {:?}", enc.push_source(&dgrams[k - 1]).err());
    assert!(enc.push_source(&dgrams[k - 1]).is_err());

    // 2. rusty_erasure: pad (zeros + length prefix), lose the short symbol and one more, recover, unpad
    let padded: Vec<Vec<u8>> = dgrams.iter().map(|d| pad(d, shard)).collect();
    let c = ours_coder(k, r);
    let rep = rusty_encode(&c, &padded);
    let mut ok = 0;
    for lost_other in 0..k - 1 {
        let all: Vec<&[u8]> = padded.iter().chain(rep.iter()).map(|v| &v[..]).collect();
        let sh: Vec<Option<&[u8]>> = (0..k + r).map(|i| if i == k - 1 || i == lost_other { None } else { Some(all[i]) }).collect();
        let got = rusty_recover(&c, &sh, &[lost_other, k - 1], shard).unwrap();
        assert_eq!(unpad(&got[1]), &dgrams[k - 1][..]);
        assert_eq!(unpad(&got[0]), &dgrams[lost_other][..]);
        ok += 1;
    }
    println!("rusty_erasure padded block (9 x 1180 B + 100 B): short symbol + 1 other lost, {ok}/{ok} recover with exact length");

    // 3. timing: all-full block vs short-last block vs random sizes (shards are always `shard` bytes)
    let full: Vec<Vec<u8>> = make_sources(k, shard, 9);
    let rand_sizes: Vec<Vec<u8>> = (0..k).map(|i| { let n = rand::Rng::gen_range(&mut rg, 100..=SYM); pad(&make_sources(1, n, 50 + i as u64)[0], shard) }).collect();
    let mut tsv = vec![];
    for (name, blk) in [("all-1180", &full), ("short-last(100)", &padded), ("random-100..1180", &rand_sizes)] {
        let refs: Vec<&[u8]> = blk.iter().map(|v| &v[..]).collect();
        let mut o = vec![vec![0u8; shard]; r];
        let enc_t = bench(20, 400, || (), |_| { let mut m: Vec<&mut [u8]> = o.iter_mut().map(|v| &mut v[..]).collect(); c.encode(&refs, &mut m).unwrap(); });
        let rep = rusty_encode(&c, blk);
        let all: Vec<&[u8]> = blk.iter().chain(rep.iter()).map(|v| &v[..]).collect();
        let sh: Vec<Option<&[u8]>> = (0..k + r).map(|i| if i < 2 { None } else { Some(all[i]) }).collect();
        let mut o2 = vec![vec![0u8; shard]; 2];
        let dec_t = bench(20, 400, || (), |_| { let mut m: Vec<&mut [u8]> = o2.iter_mut().map(|v| &mut v[..]).collect(); c.recover(&sh, &[0, 1], &mut m).unwrap(); });
        tsv.push(format!("rusty-ours k=10 r=2 {name}: encode {:.2} us, decode(2 lost, warm) {:.2} us", enc_t.0, dec_t.0));
    }
    // cost of padding itself (copy 100 B into a 1182 B zeroed shard)
    let d = &dgrams[k - 1];
    let pad_t = bench(50, 2000, || (), |_| { std::hint::black_box(pad(d, shard)); });
    tsv.push(format!("pad one 100 B datagram into a 1182 B shard (alloc + zero + copy): {:.3} us", pad_t.0));
    for l in tsv { println!("{l}"); }

    // 4. raptorq: the object is just bytes; the last source symbol is padded internally and the
    //    decoder returns exactly transfer_length bytes. Repair packets are always symbol-sized.
    let total = (k - 1) * SYM + 100;
    let data: Vec<u8> = dgrams.concat();
    assert_eq!(data.len(), total);
    let oti = rq_oti(total);
    let pk = RqEnc::new(&data, oti).get_encoded_packets(r as u32 + 3);
    println!("raptorq: {} packets for a {} B object; packet payload sizes: {:?}", pk.len(), total,
             pk.iter().map(|p| p.data().len()).collect::<std::collections::BTreeSet<_>>());
    let mut d = RqDec::new(oti);
    let mut res = None;
    for p in pk.iter().skip(2).cloned() { if let Some(o) = d.decode(p) { res = Some(o); break; } }
    println!("raptorq decode of the short-last object (2 sources lost): len {:?}, exact={}", res.as_ref().map(|v| v.len()), res.as_deref() == Some(&data[..]));
    assert_eq!(res.as_deref(), Some(&data[..]));
    for (name, dd) in [("raptorq all-1180", make_sources(k, SYM, 1).concat()), ("raptorq short-last", data.clone())] {
        let oti = rq_oti(dd.len());
        let enc_t = bench(10, 300, || (), |_| { std::hint::black_box(RqEnc::new(&dd, oti).get_encoded_packets(2)); });
        let pk = RqEnc::new(&dd, oti).get_encoded_packets(5);
        let dec_t = bench(10, 300, || pk[2..].to_vec(), |p| { let mut d = RqDec::new(oti); for x in p { if d.decode(x).is_some() { break; } } });
        println!("{name} k=10 r=2: encode {:.1} us, decode(2 lost) {:.1} us", enc_t.0, dec_t.0);
    }
    let _ = &mut rg; let _ = Vec::<usize>::new().shuffle(&mut rg);
    0
}
