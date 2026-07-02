//! XOR parity primitive for FEC. Left-pads shorter source slices with
//! zeros so XOR is defined over a group of sources with varying lengths.

/// Result of accumulating the K-th source — the parity datagram to emit
/// and the group metadata the wire envelope needs.
#[derive(Debug, Clone)]
pub struct GroupResult {
    pub group_first_wire_seq: u32,
    pub k: u8,
    /// Length of the first source in the group (encoded into the parity
    /// envelope's `group_first_payload_len`).
    pub first_len: u16,
    pub parity: Vec<u8>,
}

/// Accumulates K source datagram byte buffers, then on the K-th
/// `add()` returns the FEC parity.
pub struct GroupBuilder {
    target_k: usize,
    first_wire_seq: Option<u32>,
    first_len: u16,
    sources: Vec<Vec<u8>>,
}

impl GroupBuilder {
    pub fn new(k: usize) -> Self {
        Self {
            target_k: k,
            first_wire_seq: None,
            first_len: 0,
            sources: Vec::with_capacity(k),
        }
    }

    pub fn add(&mut self, wire_seq: u32, source_bytes: &[u8]) -> Option<GroupResult> {
        if self.first_wire_seq.is_none() {
            self.first_wire_seq = Some(wire_seq);
            self.first_len = source_bytes.len() as u16;
        }
        self.sources.push(source_bytes.to_vec());
        if self.sources.len() < self.target_k {
            return None;
        }
        // Group full — compute parity and reset.
        let refs: Vec<&[u8]> = self.sources.iter().map(|v| v.as_slice()).collect();
        let parity = xor_payloads(&refs);
        let result = GroupResult {
            group_first_wire_seq: self.first_wire_seq.unwrap(),
            k: self.target_k as u8,
            first_len: self.first_len,
            parity,
        };
        self.reset();
        Some(result)
    }

    fn reset(&mut self) {
        self.first_wire_seq = None;
        self.first_len = 0;
        self.sources.clear();
    }
}

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
        let a = vec![0xFF]; // length 1
        let b = vec![0x11, 0x22, 0x33]; // length 3
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

#[cfg(test)]
mod group_tests {
    use super::*;
    use crate::transport::reliable_emitter::FEC_GROUP_SIZE_K;

    #[test]
    fn group_builder_fires_after_k_sources() {
        let mut g = GroupBuilder::new(FEC_GROUP_SIZE_K);
        for i in 0..FEC_GROUP_SIZE_K - 1 {
            assert!(g.add(0, &[i as u8]).is_none(), "no fire before K");
        }
        let result = g.add(0, &[99]);
        let Some(GroupResult {
            group_first_wire_seq,
            k,
            parity,
            first_len,
        }) = result
        else {
            panic!("expected fire");
        };
        assert_eq!(k as usize, FEC_GROUP_SIZE_K);
        assert_eq!(group_first_wire_seq, 0);
        assert_eq!(first_len, 1);
        assert!(!parity.is_empty());
        // After fire, the builder resets — first add returns None again
        assert!(g.add(0, &[1]).is_none());
    }

    #[test]
    fn group_builder_tracks_first_wire_seq() {
        let mut g = GroupBuilder::new(3);
        assert!(g.add(100, &[1]).is_none());
        assert!(g.add(101, &[2]).is_none());
        let r = g.add(102, &[3]).unwrap();
        assert_eq!(r.group_first_wire_seq, 100);
    }
}
