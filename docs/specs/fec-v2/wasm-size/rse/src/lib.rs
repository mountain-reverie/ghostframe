extern crate alloc;
use alloc::vec::Vec;
use reed_solomon_erasure::galois_8::ReedSolomon;
const MAXB: usize = 1200 * 300;
static mut IN: [u8; MAXB] = [0; MAXB];
#[no_mangle] pub extern "C" fn in_ptr() -> *mut u8 { unsafe { core::ptr::addr_of_mut!(IN) as *mut u8 } }
static mut SH: Vec<Vec<u8>> = Vec::new();
#[no_mangle]
pub extern "C" fn rs_encode(k: usize, r: usize, len: usize) -> usize {
    let data = unsafe { &*core::ptr::addr_of!(IN) };
    let rs = ReedSolomon::new(k, r).unwrap();
    let mut shards: Vec<Vec<u8>> = (0..k+r).map(|i| if i < k { data[i*len..(i+1)*len].to_vec() } else { alloc::vec![0u8; len] }).collect();
    rs.encode(&mut shards).unwrap();
    unsafe { *core::ptr::addr_of_mut!(SH) = shards; }
    r
}
#[no_mangle]
pub extern "C" fn rs_decode(k: usize, r: usize, _len: usize, lost: usize) -> usize {
    let rs = ReedSolomon::new(k, r).unwrap();
    let sh = unsafe { &*core::ptr::addr_of!(SH) };
    let mut opt: Vec<Option<Vec<u8>>> = sh.iter().cloned().map(Some).collect();
    for i in 0..lost { opt[i] = None; }
    rs.reconstruct_data(&mut opt).unwrap();
    let mut ok = 0; for i in 0..lost { if opt[i].as_ref() == Some(&sh[i]) { ok += 1; } }
    ok
}
