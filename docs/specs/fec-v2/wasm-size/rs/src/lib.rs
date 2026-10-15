use reed_solomon_simd::{ReedSolomonEncoder, ReedSolomonDecoder};
const MAXB: usize = 1200 * 512;
static mut IN: [u8; MAXB] = [0; MAXB];
static mut OUT: [u8; MAXB] = [0; MAXB];
#[no_mangle] pub extern "C" fn in_ptr() -> *mut u8 { unsafe { core::ptr::addr_of_mut!(IN) as *mut u8 } }
#[no_mangle]
pub extern "C" fn rs_encode(k: usize, r: usize, len: usize) -> usize {
    let data = unsafe { &*core::ptr::addr_of!(IN) };
    let mut enc = ReedSolomonEncoder::new(k, r, len).unwrap();
    for i in 0..k { enc.add_original_shard(&data[i*len..(i+1)*len]).unwrap(); }
    let res = enc.encode().unwrap();
    let mut n = 0;
    for s in res.recovery_iter() { unsafe { (&mut *core::ptr::addr_of_mut!(OUT))[k*len+n..k*len+n+len].copy_from_slice(s); } n += len; }
    unsafe { (&mut *core::ptr::addr_of_mut!(OUT))[..k*len].copy_from_slice(&data[..k*len]); }
    n
}
/// decode with the first `lost` originals missing, using recovery shards 0..lost
#[no_mangle]
pub extern "C" fn rs_decode(k: usize, r: usize, len: usize, lost: usize) -> usize {
    let data = unsafe { &*core::ptr::addr_of!(OUT) };
    let mut dec = ReedSolomonDecoder::new(k, r, len).unwrap();
    for i in lost..k { dec.add_original_shard(i, &data[i*len..(i+1)*len]).unwrap(); }
    for j in 0..lost { dec.add_recovery_shard(j, &data[(k+j)*len..(k+j+1)*len]).unwrap(); }
    let res = dec.decode().unwrap();
    let mut ok = 0;
    for (i, s) in res.restored_original_iter() { if s == &data[i*len..(i+1)*len] { ok += 1; } }
    ok
}
