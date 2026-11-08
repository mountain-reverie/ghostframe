//! Wasm probe: one crate, one feature per candidate (see Cargo.toml). Same exports for all:
//!   in_ptr()                       -> address of the input buffer (k * len bytes of sources)
//!   set_mode(m)                    -> candidate-specific variant (rusty: 0 ours, 1 built-in Cauchy, 2 ours e x e)
//!   rs_encode(k, r, len)           -> builds the repair symbols, keeps them for decode
//!   rs_decode(k, r, len, lost)     -> loses the first `lost` sources, restores them, returns how many
//!                                     came back byte-exact (matrices / coders are built inside: cold, per block)
#![allow(static_mut_refs)]

#[cfg(any(feature = "gf", feature = "rusty"))]
#[path = "../../common/gf256.rs"]
mod gf256;
#[cfg(all(feature = "rusty", not(feature = "lean")))]
#[path = "../../common/smalldec.rs"]
mod smalldec;

const MAXB: usize = 1200 * 300;
static mut IN: [u8; MAXB] = [0; MAXB];
static mut MODE: u32 = 0;
static mut ST: Option<codec::State> = None;

#[no_mangle] pub extern "C" fn in_ptr() -> *mut u8 { unsafe { core::ptr::addr_of_mut!(IN) as *mut u8 } }
#[no_mangle] pub extern "C" fn set_mode(m: u32) { unsafe { MODE = m } }
#[no_mangle] pub extern "C" fn rs_encode(k: usize, r: usize, len: usize) -> usize {
    let data = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(IN) as *const u8, k * len) };
    unsafe { ST = Some(codec::encode(k, r, len, data, MODE)); }
    r
}
#[no_mangle] pub extern "C" fn rs_decode(k: usize, r: usize, len: usize, lost: usize) -> usize {
    let data = unsafe { core::slice::from_raw_parts(core::ptr::addr_of!(IN) as *const u8, k * len) };
    codec::decode(unsafe { ST.as_ref().unwrap() }, k, r, len, lost, data, unsafe { MODE })
}

// ---------------------------------------------------------------- baseline: XOR, r = 1
#[cfg(not(any(feature = "gf", feature = "rusty", feature = "rq", feature = "rlc")))]
mod codec {
    pub struct State(Vec<Vec<u8>>);
    pub fn encode(k: usize, _r: usize, len: usize, data: &[u8], _m: u32) -> State {
        let mut x = vec![0u8; len];
        for i in 0..k { for j in 0..len { x[j] ^= data[i * len + j]; } }
        State(vec![x])
    }
    pub fn decode(s: &State, k: usize, _r: usize, len: usize, lost: usize, data: &[u8], _m: u32) -> usize {
        if lost != 1 { return 0; }
        let mut x = s.0[0].clone();
        for i in 1..k { for j in 0..len { x[j] ^= data[i * len + j]; } }
        (x[..] == data[..len]) as usize
    }
}

// ---------------------------------------------------------------- in-house scalar oracle
#[cfg(feature = "gf")]
mod codec {
    use super::gf256;
    pub struct State(Vec<Vec<u8>>);
    pub fn encode(k: usize, r: usize, len: usize, data: &[u8], _m: u32) -> State {
        let src: Vec<&[u8]> = (0..k).map(|i| &data[i * len..(i + 1) * len]).collect();
        State(gf256::encode(&src, r, len))
    }
    pub fn decode(s: &State, k: usize, r: usize, len: usize, lost: usize, data: &[u8], _m: u32) -> usize {
        let mut sh: Vec<Option<Vec<u8>>> = (0..k).map(|i| Some(data[i * len..(i + 1) * len].to_vec())).chain(s.0.iter().cloned().map(Some)).collect();
        for i in 0..lost { sh[i] = None; }
        if !gf256::decode(&mut sh, k, r, len) { return 0; }
        (0..lost).filter(|&i| sh[i].as_deref() == Some(&data[i * len..(i + 1) * len])).count()
    }
}

// ---------------------------------------------------------------- rusty_erasure
#[cfg(feature = "rusty")]
mod codec {
    use super::gf256;
    #[cfg(not(feature = "lean"))]
    use super::smalldec;
    use rusty_erasure::{coder, Matrix};
    pub struct State(Vec<Vec<u8>>);
    fn coder_for(k: usize, r: usize, mode: u32) -> rusty_erasure::Coder {
        let m = if cfg!(not(feature = "lean")) && mode == 1 { Matrix::cauchy(k, r).unwrap() } else { Matrix::from_bytes(k + r, k, gf256::ours_matrix(k, r, None)).unwrap() };
        coder(m).unwrap()
    }
    pub fn encode(k: usize, r: usize, len: usize, data: &[u8], mode: u32) -> State {
        let c = coder_for(k, r, mode);
        let src: Vec<&[u8]> = (0..k).map(|i| &data[i * len..(i + 1) * len]).collect();
        let mut out = vec![vec![0u8; len]; r];
        let mut m: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
        c.encode(&src, &mut m).unwrap();
        State(out)
    }
    pub fn decode(s: &State, k: usize, r: usize, len: usize, lost: usize, data: &[u8], mode: u32) -> usize {
        let shards: Vec<Option<&[u8]>> = (0..k).map(|i| if i < lost { None } else { Some(&data[i * len..(i + 1) * len]) })
            .chain(s.0.iter().map(|v| Some(&v[..]))).collect();
        #[cfg(not(feature = "lean"))]
        if mode == 2 {
            let got = smalldec::decode_ee_ours(k, r, &shards, len).unwrap();
            return got.iter().filter(|(i, b)| b[..] == data[i * len..(i + 1) * len]).count();
        }
        let got: Vec<(usize, Vec<u8>)> = {
            let c = coder_for(k, r, mode);
            let rebuild: Vec<usize> = (0..lost).collect();
            let mut out = vec![vec![0u8; len]; lost];
            let mut m: Vec<&mut [u8]> = out.iter_mut().map(|v| &mut v[..]).collect();
            c.recover(&shards, &rebuild, &mut m).unwrap();
            out.into_iter().enumerate().collect()
        };
        got.iter().filter(|(i, b)| b[..] == data[i * len..(i + 1) * len]).count()
    }
}

// ---------------------------------------------------------------- raptorq
#[cfg(feature = "rq")]
mod codec {
    use raptorq::{Decoder, Encoder, EncodingPacket, ObjectTransmissionInformation as Oti};
    pub struct State(Vec<EncodingPacket>);
    pub fn encode(k: usize, r: usize, len: usize, data: &[u8], _m: u32) -> State {
        // r + 3 repairs so that decode with `lost = r` can still finish (it stops as soon as it can)
        State(Encoder::new(data, Oti::new((k * len) as u64, len as u16, 1, 1, 1)).get_encoded_packets(r as u32 + 3))
    }
    pub fn decode(s: &State, k: usize, _r: usize, len: usize, lost: usize, data: &[u8], _m: u32) -> usize {
        let mut d = Decoder::new(Oti::new((k * len) as u64, len as u16, 1, 1, 1));
        for p in s.0[lost..].iter().cloned() { if let Some(o) = d.decode(p) { return if o == data { lost } else { 0 }; } }
        0
    }
}

// ---------------------------------------------------------------- cu-fec (k = window <= 64, r repairs, `lost` of each)
#[cfg(feature = "rlc")]
mod codec {
    use cu_fec::{DensityThreshold, EncodingSymbolId as Esi, Field, RepairParameters, RepairPayloadId, RlcConfig, RlcDecoder, RlcEncoder};
    pub struct State(Vec<(RepairPayloadId, Vec<u8>)>);
    pub fn encode(k: usize, r: usize, len: usize, data: &[u8], _m: u32) -> State {
        let cfg = RlcConfig::new(len, k, Field::Gf256).unwrap();
        let mut e = Box::new(RlcEncoder::<1200, 64>::new(cfg, Esi::new(0)).unwrap());
        for i in 0..k { e.push_source(&data[i * len..(i + 1) * len]).unwrap(); }
        State((0..r).map(|i| {
            let mut o = vec![0u8; len];
            let id = e.encode_repair(RepairParameters::new(i as u16 + 1, DensityThreshold::FULL), &mut o).unwrap();
            (id, o)
        }).collect())
    }
    pub fn decode(s: &State, k: usize, _r: usize, len: usize, lost: usize, data: &[u8], _m: u32) -> usize {
        let cfg = RlcConfig::new(len, k, Field::Gf256).unwrap();
        let mut d = Box::new(RlcDecoder::<1200, 64, 8>::new(cfg, 8).unwrap());
        for i in lost..k { let _ = d.receive_source(Esi::new(i as u32), &data[i * len..(i + 1) * len]).unwrap(); }
        for (id, b) in s.0.iter().take(lost) { let _ = d.receive_repair(*id, b).unwrap(); }
        (0..lost).filter(|&i| d.symbol(Esi::new(i as u32)) == Some(&data[i * len..(i + 1) * len])).count()
    }
}
