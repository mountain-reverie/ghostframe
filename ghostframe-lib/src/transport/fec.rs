/// Size of the parity packet header in bytes:
///   - group_start: u16 BE (2 bytes)
///   - group_len:   u8    (1 byte)
pub const PARITY_HEADER_SIZE: usize = 3;

/// XOR all payloads together, zero-padding shorter slices to `max_len`.
pub fn xor_payloads(payloads: &[&[u8]], max_len: usize) -> Vec<u8> {
    let mut result = vec![0u8; max_len];
    for payload in payloads {
        for (i, &byte) in payload.iter().enumerate() {
            if i < max_len {
                result[i] ^= byte;
            }
        }
    }
    result
}

/// Group every `k` source fragment payloads and XOR them together.
///
/// Returns a list of `(group_start, parity_payload)` pairs where
/// `parity_payload` = [group_start: u16 BE][group_len: u8][xor_data].
///
/// Groups of size 1 are skipped (no recovery possible).
/// Returns an empty vec for k=0 or empty input.
pub fn generate_parity(fragment_payloads: &[&[u8]], k: usize) -> Vec<(u16, Vec<u8>)> {
    if k == 0 || fragment_payloads.is_empty() {
        return vec![];
    }

    let mut result = Vec::new();

    for (chunk_idx, chunk) in fragment_payloads.chunks(k).enumerate() {
        // Skip groups of size 1 — no recovery possible with a single fragment
        if chunk.len() <= 1 {
            continue;
        }

        let group_start = (chunk_idx * k) as u16;
        let group_len = chunk.len() as u8;

        let max_len = chunk.iter().map(|p| p.len()).max().unwrap_or(0);
        let xor_data = xor_payloads(chunk, max_len);

        let mut parity_payload = Vec::with_capacity(PARITY_HEADER_SIZE + xor_data.len());
        parity_payload.extend_from_slice(&group_start.to_be_bytes());
        parity_payload.push(group_len);
        parity_payload.extend_from_slice(&xor_data);

        result.push((group_start, parity_payload));
    }

    result
}

/// Decode a parity payload header into `(group_start, group_len, xor_data)`.
///
/// Returns `None` if the payload is shorter than `PARITY_HEADER_SIZE`.
pub fn decode_parity_payload(payload: &[u8]) -> Option<(u16, u8, &[u8])> {
    if payload.len() < PARITY_HEADER_SIZE {
        return None;
    }
    let group_start = u16::from_be_bytes([payload[0], payload[1]]);
    let group_len = payload[2];
    let xor_data = &payload[PARITY_HEADER_SIZE..];
    Some((group_start, group_len, xor_data))
}

/// Recover a missing fragment by XORing all received payloads with the parity XOR data.
///
/// `received` contains the payloads of all fragments that were received (not the missing one).
/// `parity_xor_data` is the raw XOR data from the parity packet (after stripping the header).
pub fn recover_fragment(received: &[&[u8]], parity_xor_data: &[u8]) -> Vec<u8> {
    let max_len = received
        .iter()
        .map(|p| p.len())
        .max()
        .unwrap_or(0)
        .max(parity_xor_data.len());

    // Start with the parity XOR data, then XOR in each received fragment.
    // Since parity = frag0 XOR frag1 XOR ... XOR fragN-1,
    // missing = parity XOR all_received
    let mut all: Vec<&[u8]> = Vec::with_capacity(received.len() + 1);
    all.push(parity_xor_data);
    all.extend_from_slice(received);

    xor_payloads(&all, max_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_payloads_basic() {
        let a = &[0x0Fu8, 0xF0u8, 0xAAu8][..];
        let b = &[0xF0u8, 0x0Fu8, 0x55u8][..];
        let result = xor_payloads(&[a, b], 3);
        assert_eq!(result, vec![0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn xor_payloads_zero_pads_shorter() {
        let a = &[0xFFu8, 0xFFu8, 0xFFu8][..];
        let b = &[0xFFu8][..]; // shorter — remaining bytes treated as 0x00
        let result = xor_payloads(&[a, b], 3);
        // byte 0: 0xFF ^ 0xFF = 0x00
        // byte 1: 0xFF ^ 0x00 = 0xFF  (b is padded with zero)
        // byte 2: 0xFF ^ 0x00 = 0xFF
        assert_eq!(result, vec![0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn generate_parity_k4_eight_fragments() {
        // 8 fragments of 1 byte each, K=4 → 2 parity packets
        let payloads: Vec<Vec<u8>> = (0u8..8).map(|i| vec![i * 10]).collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|v| v.as_slice()).collect();

        let parity = generate_parity(&refs, 4);
        assert_eq!(parity.len(), 2);

        // Group 0: frags 0..3 → group_start=0, group_len=4
        let (gs0, pp0) = &parity[0];
        assert_eq!(*gs0, 0u16);
        let (g_start, g_len, xor_data) = decode_parity_payload(pp0).unwrap();
        assert_eq!(g_start, 0);
        assert_eq!(g_len, 4);
        let expected_xor0 = 0u8 ^ 10 ^ 20 ^ 30;
        assert_eq!(xor_data, &[expected_xor0]);

        // Group 1: frags 4..7 → group_start=4, group_len=4
        let (gs1, pp1) = &parity[1];
        assert_eq!(*gs1, 4u16);
        let (g_start1, g_len1, xor_data1) = decode_parity_payload(pp1).unwrap();
        assert_eq!(g_start1, 4);
        assert_eq!(g_len1, 4);
        let expected_xor1 = 40u8 ^ 50 ^ 60 ^ 70;
        assert_eq!(xor_data1, &[expected_xor1]);
    }

    #[test]
    fn generate_parity_skips_group_of_one() {
        // Single fragment with K=4 → group size is 1, should be skipped
        let payloads: Vec<Vec<u8>> = vec![vec![0xAB]];
        let refs: Vec<&[u8]> = payloads.iter().map(|v| v.as_slice()).collect();
        let parity = generate_parity(&refs, 4);
        assert!(parity.is_empty(), "group of 1 should produce no parity");
    }

    #[test]
    fn generate_parity_trailing_short_group() {
        // 5 fragments, K=4 → group0 has 4 frags (ok), group1 has 1 frag (skip)
        let payloads: Vec<Vec<u8>> = (0u8..5).map(|i| vec![i]).collect();
        let refs: Vec<&[u8]> = payloads.iter().map(|v| v.as_slice()).collect();
        let parity = generate_parity(&refs, 4);
        assert_eq!(parity.len(), 1, "only first group of 4 should produce parity");
        assert_eq!(parity[0].0, 0u16);
    }

    #[test]
    fn recover_single_lost_fragment() {
        // 4 fragments, lose fragment index 2
        let frags: Vec<Vec<u8>> = vec![
            vec![0x01, 0x02],
            vec![0x03, 0x04],
            vec![0x05, 0x06], // this one is "lost"
            vec![0x07, 0x08],
        ];
        let refs: Vec<&[u8]> = frags.iter().map(|v| v.as_slice()).collect();

        let parity = generate_parity(&refs, 4);
        assert_eq!(parity.len(), 1);

        let (_, parity_payload) = &parity[0];
        let (_, _, xor_data) = decode_parity_payload(parity_payload).unwrap();

        // Received = all except frag[2]
        let received: Vec<&[u8]> = vec![frags[0].as_slice(), frags[1].as_slice(), frags[3].as_slice()];
        let recovered = recover_fragment(&received, xor_data);

        assert_eq!(recovered, frags[2], "recovered fragment should match original");
    }

    #[test]
    fn recover_first_fragment() {
        // 4 fragments, lose fragment index 0
        let frags: Vec<Vec<u8>> = vec![
            vec![0xDE, 0xAD], // lost
            vec![0xBE, 0xEF],
            vec![0xCA, 0xFE],
            vec![0xBA, 0xBE],
        ];
        let refs: Vec<&[u8]> = frags.iter().map(|v| v.as_slice()).collect();

        let parity = generate_parity(&refs, 4);
        let (_, parity_payload) = &parity[0];
        let (_, _, xor_data) = decode_parity_payload(parity_payload).unwrap();

        let received: Vec<&[u8]> = vec![frags[1].as_slice(), frags[2].as_slice(), frags[3].as_slice()];
        let recovered = recover_fragment(&received, xor_data);

        assert_eq!(recovered, frags[0]);
    }

    #[test]
    fn recover_with_variable_length_fragments() {
        // Fragments of different lengths — shorter ones are zero-padded in XOR
        let frags: Vec<Vec<u8>> = vec![
            vec![0x01],             // 1 byte (lost)
            vec![0x02, 0x03],       // 2 bytes
            vec![0x04, 0x05, 0x06], // 3 bytes
        ];
        let refs: Vec<&[u8]> = frags.iter().map(|v| v.as_slice()).collect();

        let parity = generate_parity(&refs, 3);
        assert_eq!(parity.len(), 1);

        let (_, parity_payload) = &parity[0];
        let (_, _, xor_data) = decode_parity_payload(parity_payload).unwrap();

        // xor_data should be 3 bytes (max length)
        // byte 0: 0x01 ^ 0x02 ^ 0x04 = 0x07
        // byte 1: 0x00 ^ 0x03 ^ 0x05 = 0x06
        // byte 2: 0x00 ^ 0x00 ^ 0x06 = 0x06
        assert_eq!(xor_data, &[0x07, 0x06, 0x06]);

        // Recover frag[0] (1 byte) from frag[1] and frag[2] + parity
        let received: Vec<&[u8]> = vec![frags[1].as_slice(), frags[2].as_slice()];
        let recovered = recover_fragment(&received, xor_data);

        // recovered byte 0 = 0x01, bytes 1..2 = 0x00 (padding artifacts)
        assert_eq!(recovered[0], 0x01, "first byte should match original fragment");
        // The recovered slice has length equal to the max of all inputs (3 bytes here)
        // bytes beyond the original are zero (XOR cancels out)
        assert_eq!(recovered[1], 0x00);
        assert_eq!(recovered[2], 0x00);
    }

    #[test]
    fn decode_parity_payload_too_short() {
        assert!(decode_parity_payload(&[]).is_none());
        assert!(decode_parity_payload(&[0x00]).is_none());
        assert!(decode_parity_payload(&[0x00, 0x01]).is_none());
        // Exactly 3 bytes is ok (empty xor_data)
        assert!(decode_parity_payload(&[0x00, 0x01, 0x02]).is_some());
    }

    #[test]
    fn empty_input() {
        // Empty fragment list
        let empty: Vec<&[u8]> = vec![];
        assert!(generate_parity(&empty, 4).is_empty());

        // k=0 with non-empty input
        let payloads: Vec<Vec<u8>> = vec![vec![0x01], vec![0x02]];
        let refs: Vec<&[u8]> = payloads.iter().map(|v| v.as_slice()).collect();
        assert!(generate_parity(&refs, 0).is_empty());
    }
}
