//! XOR parity primitive for FEC. Left-pads shorter source slices with
//! zeros so XOR is defined over a group of sources with varying lengths.

/// XOR each byte across all provided slices, left-padding shorter slices
/// with zeros. The result length is the maximum input length.
pub fn xor_payloads(sources: &[&[u8]]) -> Vec<u8> {
    let max_len = sources.iter().map(|s| s.len()).max().unwrap_or(0);
    let mut out = vec![0u8; max_len];
    for src in sources {
        let pad = max_len - src.len();
        for (i, &b) in src.iter().enumerate() {
            out[pad + i] ^= b;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_of_two_equal_length_buffers_recovers_either() {
        let a = vec![0x12, 0x34, 0x56];
        let b = vec![0xAB, 0xCD, 0xEF];
        let parity = xor_payloads(&[&a, &b]);
        // Recover a from (parity XOR b)
        let recovered_a = xor_payloads(&[&parity, &b]);
        assert_eq!(recovered_a, a);
    }

    #[test]
    fn xor_left_pads_shorter_slices() {
        let a = vec![0xFF];                  // length 1
        let b = vec![0x11, 0x22, 0x33];      // length 3
        let parity = xor_payloads(&[&a, &b]);
        assert_eq!(parity.len(), 3);
        // Logical bytes: [00 00 FF] XOR [11 22 33] = [11 22 CC]
        assert_eq!(parity, vec![0x11, 0x22, 0xCC]);
    }

    #[test]
    fn empty_group_returns_empty_parity() {
        assert_eq!(xor_payloads(&[]), Vec::<u8>::new());
    }

    #[test]
    fn round_trip_three_sources() {
        let a = vec![1, 2, 3, 4];
        let b = vec![10, 20, 30, 40];
        let c = vec![100, 110, 120, 130];
        let parity = xor_payloads(&[&a, &b, &c]);
        // Recover c from (parity XOR a XOR b)
        let recovered_c = xor_payloads(&[&parity, &a, &b]);
        assert_eq!(recovered_c, c);
    }
}
