//! Legacy per-tile-assembly fragment FEC.
//!
//! Port of `ghostframe-web-client/src/fec.ts`. Parity payloads cover
//! contiguous groups of `k` fragments (k=4) within a single tile
//! assembly's fragment list, keyed by `group_start`. Recovery XORs the
//! parity's `xor_data` with all *received* fragments in the group; this
//! only works when exactly one fragment in the group is missing.

use std::collections::HashMap;

use crate::event::TileKey;

pub const PARITY_HEADER_SIZE: usize = 3;

#[derive(Debug, Clone)]
pub struct ParityInfo {
    pub group_start: u16,
    pub group_len: u8,
    pub xor_data: Vec<u8>,
}

/// Decode the 3-byte parity header from a parity datagram payload.
/// Layout: group_start (u16 BE) | group_len (u8) | xor_data...
/// Returns `None` if the payload is too short to contain the header.
pub fn decode_parity_payload(payload: &[u8]) -> Option<ParityInfo> {
    if payload.len() < PARITY_HEADER_SIZE {
        return None;
    }
    let group_start = u16::from_be_bytes([payload[0], payload[1]]);
    let group_len = payload[2];
    let xor_data = payload[PARITY_HEADER_SIZE..].to_vec();
    Some(ParityInfo {
        group_start,
        group_len,
        xor_data,
    })
}

/// XOR all received payloads together with `xor_data`, producing the
/// recovered fragment. The result length matches `xor_data.len()`; shorter
/// buffers are implicitly zero-padded (left-aligned, i.e. XOR from index 0).
pub fn recover_fragment(received_payloads: &[Vec<u8>], xor_data: &[u8]) -> Vec<u8> {
    let max_len = xor_data.len();
    let mut out = vec![0u8; max_len];
    for buf in received_payloads.iter().chain(std::iter::once(&xor_data.to_vec())) {
        for (i, b) in buf.iter().enumerate() {
            if i < max_len {
                out[i] ^= b;
            }
        }
    }
    out
}

#[derive(Default)]
pub struct FragmentParity {
    /// Parity info keyed by (tile key, group_start).
    parities: HashMap<TileKey, HashMap<u16, ParityInfo>>,
}

impl FragmentParity {
    pub fn new() -> Self {
        FragmentParity {
            parities: HashMap::new(),
        }
    }

    pub fn store(&mut self, key: TileKey, parity_payload: &[u8]) {
        if let Some(info) = decode_parity_payload(parity_payload) {
            self.parities
                .entry(key)
                .or_default()
                .insert(info.group_start, info);
        }
    }

    /// Try to recover a missing fragment given the current fragments (None
    /// entries missing), k=4 groups. Returns `(missing_idx, recovered)`.
    pub fn try_recover(
        &self,
        key: TileKey,
        fragments: &[Option<Vec<u8>>],
    ) -> Option<(usize, Vec<u8>)> {
        let group_map = self.parities.get(&key)?;
        // Try every group that has stored parity info; find one with
        // exactly one missing fragment among `fragments`.
        for (&group_start_u16, parity) in group_map.iter() {
            let group_start = group_start_u16 as usize;
            let group_end = std::cmp::min(group_start + parity.group_len as usize, fragments.len());
            let mut received: Vec<Vec<u8>> = Vec::new();
            let mut missing_count = 0usize;
            let mut missing_idx = None;
            let mut ok = true;
            for i in group_start..group_end {
                match &fragments[i] {
                    None => {
                        missing_count += 1;
                        missing_idx = Some(i);
                        if missing_count > 1 {
                            ok = false;
                            break;
                        }
                    }
                    Some(payload) => received.push(payload.clone()),
                }
            }
            if ok && missing_count == 1 {
                if let Some(idx) = missing_idx {
                    return Some((idx, recover_fragment(&received, &parity.xor_data)));
                }
            }
        }
        None
    }

    /// Try to recover a specific fragment index using its k=4 group,
    /// mirroring `ParityRecovery.tryRecover(missingIdx, fragments, k)`.
    pub fn try_recover_idx(
        &self,
        key: TileKey,
        missing_idx: usize,
        fragments: &[Option<Vec<u8>>],
        k: usize,
    ) -> Option<Vec<u8>> {
        let group_start = (missing_idx / k) * k;
        let group_map = self.parities.get(&key)?;
        let parity = group_map.get(&(group_start as u16))?;

        let group_end = std::cmp::min(group_start + parity.group_len as usize, fragments.len());
        let mut received: Vec<Vec<u8>> = Vec::new();
        let mut missing_count = 0usize;
        for i in group_start..group_end {
            match &fragments[i] {
                None => {
                    missing_count += 1;
                    if missing_count > 1 {
                        return None;
                    }
                }
                Some(payload) => received.push(payload.clone()),
            }
        }
        if missing_count != 1 {
            return None;
        }
        Some(recover_fragment(&received, &parity.xor_data))
    }

    pub fn remove(&mut self, key: &TileKey) {
        self.parities.remove(key);
    }
}
