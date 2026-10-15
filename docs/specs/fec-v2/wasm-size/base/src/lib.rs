#[no_mangle]
pub extern "C" fn xor_encode(ptr: *const u8, k: usize, len: usize, out: *mut u8) {
    let data = unsafe { core::slice::from_raw_parts(ptr, k*len) };
    let o = unsafe { core::slice::from_raw_parts_mut(out, len) };
    for i in 0..k { for j in 0..len { o[j] ^= data[i*len+j]; } }
}
