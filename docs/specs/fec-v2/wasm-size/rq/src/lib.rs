extern crate alloc;
use alloc::vec::Vec;
use raptorq::{Encoder, Decoder, ObjectTransmissionInformation, EncodingPacket};
const MAXB: usize = 1200 * 300;
static mut IN: [u8; MAXB] = [0; MAXB];
static mut PK: Vec<EncodingPacket> = Vec::new();
#[no_mangle] pub extern "C" fn in_ptr() -> *mut u8 { unsafe { core::ptr::addr_of_mut!(IN) as *mut u8 } }
#[no_mangle]
pub extern "C" fn rs_encode(k: usize, r: usize, len: usize) -> usize {
    let data = unsafe { &(&*core::ptr::addr_of!(IN))[..k*len] };
    let oti = ObjectTransmissionInformation::with_defaults(data.len() as u64, len as u16);
    let enc = Encoder::new(data, oti);
    let pk = enc.get_encoded_packets(r as u32);
    let n = pk.len();
    unsafe { *core::ptr::addr_of_mut!(PK) = pk; }
    n
}
#[no_mangle]
pub extern "C" fn rs_decode(k: usize, _r: usize, len: usize, lost: usize) -> usize {
    let pk = unsafe { &*core::ptr::addr_of!(PK) };
    let oti = ObjectTransmissionInformation::with_defaults((k*len) as u64, len as u16);
    let mut dec = Decoder::new(oti);
    let mut out = None;
    for p in pk.iter().skip(lost) { if let Some(o) = dec.decode(p.clone()) { out = Some(o); break; } }
    match out { Some(o) if o.len() == k*len => 1, _ => 0 }
}
