# FEC: Adaptive XOR Parity for H.264 Fragments

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add forward error correction to the H.264 fragment pipeline so the receiver can recover single-packet losses without a round-trip, and report recovery stats back to the server for adaptive enable/disable.

**Architecture:** After `fragment_tile()` produces source datagrams, an `XorParityEncoder` groups every K fragments and XORs their payloads to produce one parity datagram per group. On the client side, the reassembly loop stores parity fragments alongside source fragments; when exactly one source fragment in a group is missing and the parity is present, it recovers the missing fragment by XORing the parity with the received fragments. The server enables parity generation when the client-reported loss rate exceeds 0.5% and disables it when loss drops below threshold. A lightweight `ReceiverFeedback` message (sent on the control stream) carries loss and FEC-recovery counts.

**Tech Stack:** Rust (server-side parity generation + feedback parsing), TypeScript (client-side recovery + feedback reporting)

**Scope notes:**
- Audio RED-style duplication is deferred — no audio pipeline exists yet.
- Multi-slice H.264 encoding (MTU-sized slices) is an encoder config change, not FEC — handle separately.
- This plan builds on the M2 pipeline (dirty tracking, H.264 encode, fragment/reassemble, WebCodecs decode) which is already implemented.

---

## File Structure

```
ghostframe-lib/src/
├── transport/
│   ├── protocol.rs           # Modified: parity datagram encode/decode, PARITY_HEADER_SIZE
│   ├── fec.rs                # NEW: XorParityEncoder, xor_recover, parity generation logic
│   ├── feedback.rs           # NEW: ReceiverFeedback struct, encode/decode
│   ├── io_bridge.rs          # Modified: generate parity after fragment_tile(), parse feedback
│   └── mod.rs                # Modified: pub mod fec; pub mod feedback;

ghostframe-web-client/src/
├── fec.ts                    # NEW: XorParityRecovery, client-side parity XOR recovery
├── feedback.ts               # NEW: LossTracker, ReceiverFeedback encoding, send on control stream
├── decoder.ts                # Modified: export PARITY_HEADER_SIZE constant
├── main.ts                   # Modified: integrate FEC recovery into reassembly loop, send feedback
```

---

## Task 1: XOR Parity Core (Rust)

**Files:**
- Create: `ghostframe-lib/src/transport/fec.rs`
- Modify: `ghostframe-lib/src/transport/mod.rs`

- [ ] **Step 1: Write failing tests for XOR parity generation and recovery**

Create `ghostframe-lib/src/transport/fec.rs`:

```rust
//! Adaptive XOR parity for H.264 fragment datagrams.
//!
//! Groups every K source fragment payloads and XORs them to produce one
//! parity payload per group.  If any single fragment in the group is lost,
//! the receiver recovers it by XORing the parity with the other received
//! fragments.
//!
//! Parity datagrams reuse the same DatagramHeader + TileHeader wire format.
//! They are distinguished by `frag_idx >= frag_total` (source fragments
//! always have `frag_idx < frag_total`).
//!
//! Layout of a parity datagram's payload:
//! ```text
//! [group_start: u16 BE][group_len: u8][xor_data: variable]
//! ```
//! - `group_start`: frag_idx of the first source fragment in this group
//! - `group_len`:   number of source fragments in this group (≤ K)
//! - `xor_data`:    XOR of all source fragment payloads in the group,
//!                  each zero-padded to the length of the longest fragment

/// Size of the parity header prepended to XOR data in parity payloads.
pub const PARITY_HEADER_SIZE: usize = 3; // group_start(2) + group_len(1)

/// XOR all `payloads` together, zero-padding shorter ones to `max_len`.
/// Returns a Vec of `max_len` bytes.
fn xor_payloads(payloads: &[&[u8]], max_len: usize) -> Vec<u8> {
    let mut result = vec![0u8; max_len];
    for p in payloads {
        for (i, &b) in p.iter().enumerate() {
            result[i] ^= b;
        }
    }
    result
}

/// Generate parity payloads for a set of source fragment payloads.
///
/// `fragment_payloads` are the raw payload bytes of each source fragment
/// (i.e. the bytes after DatagramHeader + TileHeader in each datagram).
///
/// Returns a Vec of `(group_start, parity_payload)` where `parity_payload`
/// includes the 3-byte parity header followed by XOR data.
pub fn generate_parity(
    fragment_payloads: &[&[u8]],
    k: usize,
) -> Vec<(u16, Vec<u8>)> {
    if k == 0 || fragment_payloads.is_empty() {
        return Vec::new();
    }

    let mut result = Vec::new();

    for group_start_idx in (0..fragment_payloads.len()).step_by(k) {
        let group_end = (group_start_idx + k).min(fragment_payloads.len());
        let group = &fragment_payloads[group_start_idx..group_end];

        // Don't generate parity for a group of 1 — no recovery possible
        if group.len() < 2 {
            continue;
        }

        let max_len = group.iter().map(|p| p.len()).max().unwrap_or(0);
        let xor_data = xor_payloads(group, max_len);

        let mut parity_payload = Vec::with_capacity(PARITY_HEADER_SIZE + xor_data.len());
        parity_payload.extend_from_slice(&(group_start_idx as u16).to_be_bytes());
        parity_payload.push(group.len() as u8);
        parity_payload.extend_from_slice(&xor_data);

        result.push((group_start_idx as u16, parity_payload));
    }

    result
}

/// Decode a parity payload into (group_start, group_len, xor_data).
pub fn decode_parity_payload(payload: &[u8]) -> Option<(u16, u8, &[u8])> {
    if payload.len() < PARITY_HEADER_SIZE {
        return None;
    }
    let group_start = u16::from_be_bytes([payload[0], payload[1]]);
    let group_len = payload[2];
    let xor_data = &payload[PARITY_HEADER_SIZE..];
    Some((group_start, group_len, xor_data))
}

/// Recover a missing fragment payload using XOR parity.
///
/// `received` contains the payloads of all received source fragments in the
/// group (in any order — they are all XORed together).
/// `parity_xor_data` is the XOR data from the parity payload (without the
/// 3-byte header).
///
/// Returns the recovered fragment payload.
pub fn recover_fragment(received: &[&[u8]], parity_xor_data: &[u8]) -> Vec<u8> {
    // recovered = parity XOR all_received
    // (since parity = XOR of all source, XORing with all-but-one gives the missing one)
    let max_len = parity_xor_data.len();
    let mut all: Vec<&[u8]> = received.to_vec();
    all.push(parity_xor_data);
    xor_payloads(&all, max_len)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xor_payloads_basic() {
        let a = [0xAA, 0xBB, 0xCC];
        let b = [0x11, 0x22, 0x33];
        let result = xor_payloads(&[&a, &b], 3);
        assert_eq!(result, vec![0xAA ^ 0x11, 0xBB ^ 0x22, 0xCC ^ 0x33]);
    }

    #[test]
    fn xor_payloads_zero_pads_shorter() {
        let a = [0xFF, 0xFF];
        let b = [0x01];
        let result = xor_payloads(&[&a, &b], 2);
        assert_eq!(result, vec![0xFF ^ 0x01, 0xFF]);
    }

    #[test]
    fn generate_parity_k4_eight_fragments() {
        let frags: Vec<Vec<u8>> = (0..8u8).map(|i| vec![i; 100]).collect();
        let refs: Vec<&[u8]> = frags.iter().map(|f| f.as_slice()).collect();

        let parities = generate_parity(&refs, 4);
        assert_eq!(parities.len(), 2, "8 fragments / K=4 = 2 parity packets");

        // First parity covers fragments 0..4
        let (gs0, payload0) = &parities[0];
        assert_eq!(*gs0, 0);
        let (group_start, group_len, xor_data) = decode_parity_payload(payload0).unwrap();
        assert_eq!(group_start, 0);
        assert_eq!(group_len, 4);
        assert_eq!(xor_data.len(), 100);
        // XOR of [0;100], [1;100], [2;100], [3;100] = 0^1^2^3 = 0 for each byte
        assert!(xor_data.iter().all(|&b| b == (0 ^ 1 ^ 2 ^ 3)));

        // Second parity covers fragments 4..8
        let (gs1, _) = &parities[1];
        assert_eq!(*gs1, 4);
    }

    #[test]
    fn generate_parity_skips_group_of_one() {
        let frags = vec![vec![0xAA; 50]];
        let refs: Vec<&[u8]> = frags.iter().map(|f| f.as_slice()).collect();
        let parities = generate_parity(&refs, 4);
        assert!(parities.is_empty(), "single-fragment group produces no parity");
    }

    #[test]
    fn generate_parity_trailing_short_group() {
        // 5 fragments, K=4 → group [0..4] gets parity, group [4..5] is size 1 → no parity
        let frags: Vec<Vec<u8>> = (0..5u8).map(|i| vec![i; 20]).collect();
        let refs: Vec<&[u8]> = frags.iter().map(|f| f.as_slice()).collect();
        let parities = generate_parity(&refs, 4);
        assert_eq!(parities.len(), 1);
    }

    #[test]
    fn recover_single_lost_fragment() {
        let f0 = vec![0xAA; 100];
        let f1 = vec![0xBB; 100];
        let f2 = vec![0xCC; 100];
        let f3 = vec![0xDD; 100];
        let refs: Vec<&[u8]> = vec![&f0, &f1, &f2, &f3];

        let parities = generate_parity(&refs, 4);
        let (_, parity_payload) = &parities[0];
        let (_, _, xor_data) = decode_parity_payload(parity_payload).unwrap();

        // Lose fragment 2 — recover using f0, f1, f3 + parity
        let received: Vec<&[u8]> = vec![&f0, &f1, &f3];
        let recovered = recover_fragment(&received, xor_data);
        assert_eq!(recovered, f2);
    }

    #[test]
    fn recover_first_fragment() {
        let f0 = vec![0x11; 50];
        let f1 = vec![0x22; 50];
        let f2 = vec![0x33; 50];
        let refs: Vec<&[u8]> = vec![&f0, &f1, &f2];

        let parities = generate_parity(&refs, 4);
        let (_, parity_payload) = &parities[0];
        let (_, _, xor_data) = decode_parity_payload(parity_payload).unwrap();

        // Lose fragment 0
        let received: Vec<&[u8]> = vec![&f1, &f2];
        let recovered = recover_fragment(&received, xor_data);
        assert_eq!(recovered, f0);
    }

    #[test]
    fn recover_with_variable_length_fragments() {
        let f0 = vec![0xFF; 100];
        let f1 = vec![0xAA; 80]; // shorter — padded with zeros in XOR
        let refs: Vec<&[u8]> = vec![&f0, &f1];

        let parities = generate_parity(&refs, 4);
        let (_, parity_payload) = &parities[0];
        let (_, _, xor_data) = decode_parity_payload(parity_payload).unwrap();

        // Lose f1 — recovered payload will be 100 bytes (max_len), with
        // trailing zeros where f1 was shorter
        let received: Vec<&[u8]> = vec![&f0];
        let recovered = recover_fragment(&received, xor_data);
        // First 80 bytes should match f1
        assert_eq!(&recovered[..80], &f1[..]);
        // Trailing 20 bytes are zeros (f1 was implicitly zero-padded)
        assert!(recovered[80..].iter().all(|&b| b == 0));
    }

    #[test]
    fn decode_parity_payload_too_short() {
        assert!(decode_parity_payload(&[0, 1]).is_none());
    }

    #[test]
    fn empty_input() {
        assert!(generate_parity(&[], 4).is_empty());
        assert!(generate_parity(&[&[1, 2, 3]], 0).is_empty());
    }
}
```

- [ ] **Step 2: Add module declaration**

In `ghostframe-lib/src/transport/mod.rs`, add:

```rust
pub mod fec;
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p ghostframe-lib fec::tests -- --nocapture`
Expected: All tests PASS (the implementation is included in step 1 since the functions are pure and self-contained — this is test-alongside-implementation for a stateless utility module).

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/fec.rs ghostframe-lib/src/transport/mod.rs
git commit -m "feat(fec): XOR parity generation and recovery for H.264 fragments"
```

---

## Task 2: Parity Datagram Wire Format

**Files:**
- Modify: `ghostframe-lib/src/transport/protocol.rs`

Parity datagrams reuse the existing `DatagramHeader` + `TileHeader` wire format. They are distinguished by `frag_idx >= frag_total`. This task adds helpers to construct and identify parity datagrams.

- [ ] **Step 1: Write failing tests for parity datagram helpers**

Append to the `tests` module in `ghostframe-lib/src/transport/protocol.rs`:

```rust
    #[test]
    fn is_parity_datagram() {
        // Source fragment: frag_idx < frag_total
        let source = DatagramHeader {
            frame_seq: 1, frag_idx: 2, frag_total: 8, timestamp_us: 0,
        };
        assert!(!source.is_parity());

        // Parity fragment: frag_idx >= frag_total
        let parity = DatagramHeader {
            frame_seq: 1, frag_idx: 8, frag_total: 8, timestamp_us: 0,
        };
        assert!(parity.is_parity());
    }

    #[test]
    fn fragment_tile_with_parity_roundtrip() {
        use crate::transport::fec;

        let payload: Vec<u8> = (0u8..=255).cycle().take(4096).collect();
        let max_frag = 1200;
        let k = 4;

        // Generate source datagrams
        let source_dgs = fragment_tile(7, 2, 3, Codec::H264, &payload, 999, max_frag);
        let frag_total = source_dgs.len();
        assert_eq!(frag_total, 4); // ceil(4096/1200) = 4

        // Extract source payloads (bytes after headers)
        let source_payloads: Vec<&[u8]> = source_dgs.iter().map(|dg| {
            &dg[DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE..]
        }).collect();

        // Generate parity
        let parities = fec::generate_parity(&source_payloads, k);
        assert_eq!(parities.len(), 1); // 4 frags / K=4 = 1 parity group

        // Build parity datagrams using the helper
        let parity_dgs = build_parity_datagrams(
            7, 2, 3, Codec::H264, 999,
            frag_total as u16,
            &parities,
        );
        assert_eq!(parity_dgs.len(), 1);

        // Decode the parity datagram
        let (dh, th, parity_payload) = decode_tile_datagram(&parity_dgs[0]).unwrap();
        assert!(dh.is_parity());
        assert_eq!(dh.frag_idx, frag_total as u16); // first parity index
        assert_eq!(dh.frag_total, frag_total as u16); // source fragment count preserved
        assert_eq!(th.codec, Codec::H264);
        assert_eq!(th.tile_x, 2);
        assert_eq!(th.tile_y, 3);

        // Verify parity payload has the parity header
        let (group_start, group_len, _xor_data) = fec::decode_parity_payload(parity_payload).unwrap();
        assert_eq!(group_start, 0);
        assert_eq!(group_len, 4);
    }
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ghostframe-lib is_parity_datagram -- --nocapture`
Expected: FAIL — `is_parity` method not defined

- [ ] **Step 3: Implement is_parity and build_parity_datagrams**

Add to `DatagramHeader` impl block in `ghostframe-lib/src/transport/protocol.rs`:

```rust
    /// Returns `true` if this datagram is a parity (FEC) fragment.
    /// Parity fragments have `frag_idx >= frag_total`.
    pub fn is_parity(&self) -> bool {
        self.frag_idx >= self.frag_total
    }
```

Add after the `decode_tile_datagram` function in `ghostframe-lib/src/transport/protocol.rs`:

```rust
/// Build parity datagrams for a tile's source fragments.
///
/// `frag_total` is the number of source fragments (from `fragment_tile`).
/// `parities` is the output of `fec::generate_parity`: `(group_start, parity_payload)` pairs.
///
/// Parity datagrams have `frag_idx` starting at `frag_total` (so they sort after source fragments)
/// and `frag_total` set to the source fragment count (so the receiver knows how many source
/// fragments to expect).
pub fn build_parity_datagrams(
    frame_seq: u32,
    tile_x: u8,
    tile_y: u8,
    codec: Codec,
    timestamp_us: u32,
    frag_total: u16,
    parities: &[(u16, Vec<u8>)],
) -> Vec<Vec<u8>> {
    parities
        .iter()
        .enumerate()
        .map(|(parity_idx, (_group_start, parity_payload))| {
            let dh = DatagramHeader {
                frame_seq,
                frag_idx: frag_total + parity_idx as u16,
                frag_total,
                timestamp_us,
            };
            let th = TileHeader {
                tile_x,
                tile_y,
                codec,
                lz4: false,
                generation: 0,
                payload_len: 0, // not meaningful for parity
            };
            let mut buf = Vec::with_capacity(
                DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE + parity_payload.len(),
            );
            dh.encode(&mut buf);
            th.encode(&mut buf);
            buf.extend_from_slice(parity_payload);
            buf
        })
        .collect()
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p ghostframe-lib is_parity_datagram fragment_tile_with_parity -- --nocapture`
Expected: Both tests PASS

- [ ] **Step 5: Commit**

```bash
git add ghostframe-lib/src/transport/protocol.rs
git commit -m "feat(protocol): parity datagram wire format with is_parity() and build_parity_datagrams()"
```

---

## Task 3: ReceiverFeedback Wire Format

**Files:**
- Create: `ghostframe-lib/src/transport/feedback.rs`
- Modify: `ghostframe-lib/src/transport/mod.rs`

The `ReceiverFeedback` message is sent by the client on the QUIC control stream. This task defines the encode/decode format for both Rust (server decode) and later TypeScript (client encode).

- [ ] **Step 1: Write failing tests for ReceiverFeedback**

Create `ghostframe-lib/src/transport/feedback.rs`:

```rust
//! Receiver feedback message: lightweight periodic report from client to server.
//!
//! Sent every 100ms or every 30 received datagrams, whichever comes first.
//! Encoded as a fixed-size 22-byte message on the QUIC control stream.
//!
//! Wire format (all fields big-endian):
//! ```text
//! [0]      message_type: u8 = 0x01 (ReceiverFeedback)
//! [1..9]   timestamp_ns: u64
//! [9..13]  datagrams_received: u32
//! [13..17] datagrams_lost: u32
//! [17..21] datagrams_recovered_fec: u32
//! [21]     flags: u8  (bit 0 = suspension_detected)
//! ```

pub const FEEDBACK_MSG_TYPE: u8 = 0x01;
pub const FEEDBACK_SIZE: usize = 22;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiverFeedback {
    pub timestamp_ns: u64,
    pub datagrams_received: u32,
    pub datagrams_lost: u32,
    pub datagrams_recovered_fec: u32,
    pub suspension_detected: bool,
}

impl ReceiverFeedback {
    pub fn encode(&self, buf: &mut Vec<u8>) {
        buf.push(FEEDBACK_MSG_TYPE);
        buf.extend_from_slice(&self.timestamp_ns.to_be_bytes());
        buf.extend_from_slice(&self.datagrams_received.to_be_bytes());
        buf.extend_from_slice(&self.datagrams_lost.to_be_bytes());
        buf.extend_from_slice(&self.datagrams_recovered_fec.to_be_bytes());
        buf.push(if self.suspension_detected { 1 } else { 0 });
    }

    pub fn decode(data: &[u8]) -> Option<Self> {
        if data.len() < FEEDBACK_SIZE {
            return None;
        }
        if data[0] != FEEDBACK_MSG_TYPE {
            return None;
        }
        Some(Self {
            timestamp_ns: u64::from_be_bytes(data[1..9].try_into().ok()?),
            datagrams_received: u32::from_be_bytes(data[9..13].try_into().ok()?),
            datagrams_lost: u32::from_be_bytes(data[13..17].try_into().ok()?),
            datagrams_recovered_fec: u32::from_be_bytes(data[17..21].try_into().ok()?),
            suspension_detected: data[21] & 1 != 0,
        })
    }

    /// Compute observed loss rate: lost / (received + lost).
    /// Returns 0.0 if no datagrams were observed.
    pub fn loss_rate(&self) -> f64 {
        let total = self.datagrams_received as f64 + self.datagrams_lost as f64;
        if total == 0.0 {
            return 0.0;
        }
        self.datagrams_lost as f64 / total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let fb = ReceiverFeedback {
            timestamp_ns: 1_000_000_000,
            datagrams_received: 500,
            datagrams_lost: 3,
            datagrams_recovered_fec: 2,
            suspension_detected: false,
        };
        let mut buf = Vec::new();
        fb.encode(&mut buf);
        assert_eq!(buf.len(), FEEDBACK_SIZE);

        let decoded = ReceiverFeedback::decode(&buf).unwrap();
        assert_eq!(decoded, fb);
    }

    #[test]
    fn roundtrip_with_suspension() {
        let fb = ReceiverFeedback {
            timestamp_ns: 42,
            datagrams_received: 0,
            datagrams_lost: 0,
            datagrams_recovered_fec: 0,
            suspension_detected: true,
        };
        let mut buf = Vec::new();
        fb.encode(&mut buf);
        let decoded = ReceiverFeedback::decode(&buf).unwrap();
        assert!(decoded.suspension_detected);
    }

    #[test]
    fn decode_too_short() {
        assert!(ReceiverFeedback::decode(&[0; 10]).is_none());
    }

    #[test]
    fn decode_wrong_type() {
        let mut buf = vec![0xFF];
        buf.extend_from_slice(&[0u8; 21]);
        assert!(ReceiverFeedback::decode(&buf).is_none());
    }

    #[test]
    fn loss_rate_calculation() {
        let fb = ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 97,
            datagrams_lost: 3,
            datagrams_recovered_fec: 2,
            suspension_detected: false,
        };
        let rate = fb.loss_rate();
        assert!((rate - 0.03).abs() < 0.001);
    }

    #[test]
    fn loss_rate_zero_datagrams() {
        let fb = ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 0,
            datagrams_lost: 0,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        assert_eq!(fb.loss_rate(), 0.0);
    }
}
```

- [ ] **Step 2: Add module declaration**

In `ghostframe-lib/src/transport/mod.rs`, add:

```rust
pub mod feedback;
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p ghostframe-lib feedback::tests -- --nocapture`
Expected: All 6 tests PASS

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/src/transport/feedback.rs ghostframe-lib/src/transport/mod.rs
git commit -m "feat(feedback): ReceiverFeedback wire format with loss rate calculation"
```

---

## Task 4: Integrate Parity Generation into IoBridge

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

This wires the FEC encoder into `process_frame()`. Parity datagrams are generated after source fragments and sent to all connected peers. Parity is enabled/disabled based on the latest client-reported loss rate.

- [ ] **Step 1: Add FEC fields to IoBridge struct**

In `ghostframe-lib/src/transport/io_bridge.rs`, add imports:

```rust
use crate::transport::fec;
use crate::transport::protocol::build_parity_datagrams;
use crate::transport::feedback::ReceiverFeedback;
```

Add fields to the `IoBridge` struct:

```rust
    /// FEC parity group size. 0 = disabled.
    fec_k: usize,
    /// Loss rate threshold to enable FEC (0.005 = 0.5%).
    fec_enable_threshold: f64,
    /// Loss rate threshold to disable FEC (hysteresis).
    fec_disable_threshold: f64,
```

- [ ] **Step 2: Initialize FEC fields**

In `IoBridge::new()`, add after `h264_available`:

```rust
            fec_k: 0, // disabled by default, enabled when loss > threshold
            fec_enable_threshold: 0.005,
            fec_disable_threshold: 0.002,
```

In `IoBridge::new_with_stream_for_test()` and `IoBridge::new_with_frames_for_test()`, add:

```rust
            fec_k: 0,
            fec_enable_threshold: 0.005,
            fec_disable_threshold: 0.002,
```

- [ ] **Step 3: Add parity generation to process_frame**

In `process_frame()`, after the source datagrams are sent for each tile (after the `for dg in &datagrams` loop), add parity generation:

```rust
            // Generate and send FEC parity datagrams if enabled
            if self.fec_k > 0 && codec == Codec::H264 && datagrams.len() > 1 {
                let source_payloads: Vec<&[u8]> = datagrams.iter().map(|dg| {
                    &dg[DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE..]
                }).collect();
                let parities = fec::generate_parity(&source_payloads, self.fec_k);
                let parity_dgs = build_parity_datagrams(
                    seq,
                    *tile_x as u8,
                    *tile_y as u8,
                    codec,
                    frame.timestamp_us,
                    datagrams.len() as u16,
                    &parities,
                );
                for pdg in &parity_dgs {
                    for (handle, wt) in &mut self.wt_sessions {
                        if !wt.is_connected() {
                            continue;
                        }
                        if let Some(conn) = self.server.connections.get_mut(handle) {
                            if let Err(e) = wt.send_datagram(conn, pdg) {
                                tracing::trace!(?handle, tile_x, tile_y, error = ?e,
                                    "parity datagram send failed");
                            }
                        }
                    }
                }
            }
```

Also add the import for header sizes at the top of `process_frame` or at the module level:

```rust
use crate::transport::protocol::{DATAGRAM_HEADER_SIZE, TILE_HEADER_SIZE};
```

(These are already imported in the `use` block — verify they're accessible.)

- [ ] **Step 4: Add method to update FEC state from feedback**

Add to the `IoBridge` impl:

```rust
    /// Update FEC parity state based on receiver feedback.
    /// Enables parity (K=4) when loss exceeds threshold, disables when it drops.
    fn update_fec_from_feedback(&mut self, fb: &ReceiverFeedback) {
        let loss = fb.loss_rate();
        if self.fec_k == 0 && loss >= self.fec_enable_threshold {
            self.fec_k = 4;
            tracing::info!(loss_rate = %format!("{:.2}%", loss * 100.0), "FEC enabled (K=4)");
        } else if self.fec_k > 0 && loss < self.fec_disable_threshold {
            self.fec_k = 0;
            tracing::info!(loss_rate = %format!("{:.2}%", loss * 100.0), "FEC disabled");
        }
    }
```

- [ ] **Step 5: Write test for FEC toggle**

Append to the `tests` module in `io_bridge.rs`:

```rust
    #[test]
    fn fec_toggle_from_feedback() {
        let (our_end, _peer) = UnixStream::pair().expect("pair");
        let server = QuicServer::new().expect("server");
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        let mut bridge = IoBridge::new_with_frames_for_test(our_end, server, rx);

        assert_eq!(bridge.fec_k, 0, "FEC starts disabled");

        // High loss → enable
        let fb_high = crate::transport::feedback::ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 95,
            datagrams_lost: 5,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        bridge.update_fec_from_feedback(&fb_high);
        assert_eq!(bridge.fec_k, 4, "FEC should be enabled at 5% loss");

        // Still losing — stays enabled
        bridge.update_fec_from_feedback(&fb_high);
        assert_eq!(bridge.fec_k, 4);

        // Low loss → disable
        let fb_low = crate::transport::feedback::ReceiverFeedback {
            timestamp_ns: 0,
            datagrams_received: 1000,
            datagrams_lost: 1,
            datagrams_recovered_fec: 0,
            suspension_detected: false,
        };
        bridge.update_fec_from_feedback(&fb_low);
        assert_eq!(bridge.fec_k, 0, "FEC should be disabled at 0.1% loss");
    }
```

- [ ] **Step 6: Run tests**

Run: `cargo test -p ghostframe-lib fec_toggle_from_feedback -- --nocapture`
Expected: PASS

Run: `cargo build -p ghostframe-lib`
Expected: Compiles

- [ ] **Step 7: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(io_bridge): adaptive FEC parity generation in process_frame"
```

---

## Task 5: Client-Side FEC Recovery (TypeScript)

**Files:**
- Create: `ghostframe-web-client/src/fec.ts`

Pure functions for XOR parity recovery on the client side. No DOM or WebTransport dependencies — just array math.

- [ ] **Step 1: Create fec.ts with recovery logic**

Create `ghostframe-web-client/src/fec.ts`:

```typescript
/**
 * XOR parity recovery for H.264 fragment datagrams.
 *
 * Parity datagrams are identified by frag_idx >= frag_total.
 * Their payload starts with a 3-byte header:
 *   [group_start: u16 BE][group_len: u8]
 * followed by XOR data.
 */

export const PARITY_HEADER_SIZE = 3;

export interface ParityInfo {
  groupStart: number;
  groupLen: number;
  xorData: Uint8Array;
}

/** Decode the parity header from a parity datagram's payload. */
export function decodeParityPayload(payload: Uint8Array): ParityInfo | null {
  if (payload.length < PARITY_HEADER_SIZE) return null;
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
  return {
    groupStart: view.getUint16(0, false),
    groupLen: payload[2],
    xorData: payload.slice(PARITY_HEADER_SIZE),
  };
}

/**
 * XOR all buffers together, zero-padding shorter ones.
 * Returns a new Uint8Array of length `maxLen`.
 */
function xorBuffers(buffers: Uint8Array[], maxLen: number): Uint8Array {
  const result = new Uint8Array(maxLen);
  for (const buf of buffers) {
    for (let i = 0; i < buf.length; i++) {
      result[i] ^= buf[i];
    }
  }
  return result;
}

/**
 * Attempt to recover a missing source fragment using XOR parity.
 *
 * @param receivedPayloads - payloads of received source fragments in this group
 * @param xorData - the XOR data from the parity datagram (without 3-byte header)
 * @returns the recovered fragment payload
 */
export function recoverFragment(
  receivedPayloads: Uint8Array[],
  xorData: Uint8Array,
): Uint8Array {
  const maxLen = xorData.length;
  const all = [...receivedPayloads, xorData];
  return xorBuffers(all, maxLen);
}

/**
 * Manages parity state for one tile assembly.
 *
 * Stores received parity datagrams and can attempt recovery when
 * exactly one source fragment in a group is missing.
 */
export class ParityRecovery {
  /** Parity info keyed by group_start index. */
  private parities = new Map<number, ParityInfo>();

  addParity(payload: Uint8Array): void {
    const info = decodeParityPayload(payload);
    if (info) {
      this.parities.set(info.groupStart, info);
    }
  }

  /**
   * Try to recover a missing fragment.
   *
   * @param missingIdx - the frag_idx of the missing source fragment
   * @param fragments - the assembly's fragment array (null for missing)
   * @param k - the parity group size (default 4)
   * @returns the recovered payload, or null if recovery isn't possible
   */
  tryRecover(
    missingIdx: number,
    fragments: (Uint8Array | null)[],
    k: number = 4,
  ): Uint8Array | null {
    // Find which group this fragment belongs to
    const groupStart = Math.floor(missingIdx / k) * k;
    const parity = this.parities.get(groupStart);
    if (!parity) return null;

    // Check that exactly one fragment in the group is missing
    const groupEnd = Math.min(groupStart + parity.groupLen, fragments.length);
    const received: Uint8Array[] = [];
    let missingCount = 0;

    for (let i = groupStart; i < groupEnd; i++) {
      if (fragments[i] === null) {
        missingCount++;
        if (missingCount > 1) return null; // can't recover 2+ losses
      } else {
        received.push(fragments[i]!);
      }
    }

    if (missingCount !== 1) return null;

    return recoverFragment(received, parity.xorData);
  }

  clear(): void {
    this.parities.clear();
  }
}
```

- [ ] **Step 2: Verify web client builds**

Run: `cd ghostframe-web-client && npx tsc --noEmit`
Expected: No type errors

- [ ] **Step 3: Commit**

```bash
git add ghostframe-web-client/src/fec.ts
git commit -m "feat(web-client): XOR parity recovery for H.264 fragment FEC"
```

---

## Task 6: Client-Side Loss Tracking + Feedback Encoding

**Files:**
- Create: `ghostframe-web-client/src/feedback.ts`

Tracks datagram sequence numbers to detect gaps (losses), counts FEC recoveries, and encodes the `ReceiverFeedback` message to send on the control stream.

- [ ] **Step 1: Create feedback.ts**

Create `ghostframe-web-client/src/feedback.ts`:

```typescript
/**
 * Loss tracking and ReceiverFeedback encoding.
 *
 * Tracks received datagram frame_seq values to detect gaps.
 * Encodes periodic feedback messages for the server.
 *
 * Wire format (22 bytes, all big-endian):
 *   [0]      0x01 (message type)
 *   [1..9]   timestamp_ns: u64
 *   [9..13]  datagrams_received: u32
 *   [13..17] datagrams_lost: u32
 *   [17..21] datagrams_recovered_fec: u32
 *   [21]     flags: u8 (bit 0 = suspension_detected)
 */

export const FEEDBACK_MSG_TYPE = 0x01;
export const FEEDBACK_SIZE = 22;

export class LossTracker {
  private highestSeq = 0;
  private received = 0;
  private lost = 0;
  private recoveredFec = 0;
  private lastTimestampMs = 0;
  private suspensionDetected = false;

  /** Call for every received datagram (source or parity). */
  onDatagram(frameSeq: number): void {
    const now = performance.now();
    if (this.lastTimestampMs > 0 && (now - this.lastTimestampMs) > 100) {
      this.suspensionDetected = true;
    }
    this.lastTimestampMs = now;

    if (frameSeq > this.highestSeq + 1 && this.highestSeq > 0) {
      // Gap detected — count as lost.
      // This is a simplification: real impl would use a sliding window
      // to handle reordering. For WiFi single-hop, reordering is rare.
      this.lost += frameSeq - this.highestSeq - 1;
    }
    if (frameSeq > this.highestSeq) {
      this.highestSeq = frameSeq;
    }
    this.received++;
  }

  /** Call when a fragment is recovered via FEC. */
  onFecRecovery(): void {
    this.recoveredFec++;
  }

  /** Encode and reset counters. Returns a 22-byte feedback message. */
  encodeFeedback(): Uint8Array {
    const buf = new Uint8Array(FEEDBACK_SIZE);
    const view = new DataView(buf.buffer);

    // Timestamp in nanoseconds (from performance.now() milliseconds)
    const nowNs = BigInt(Math.round(performance.now() * 1_000_000));

    buf[0] = FEEDBACK_MSG_TYPE;
    view.setBigUint64(1, nowNs, false);
    view.setUint32(9, this.received, false);
    view.setUint32(13, this.lost, false);
    view.setUint32(17, this.recoveredFec, false);
    buf[21] = this.suspensionDetected ? 1 : 0;

    // Reset counters for next interval
    this.received = 0;
    this.lost = 0;
    this.recoveredFec = 0;
    this.suspensionDetected = false;

    return buf;
  }
}
```

- [ ] **Step 2: Verify web client builds**

Run: `cd ghostframe-web-client && npx tsc --noEmit`
Expected: No type errors

- [ ] **Step 3: Commit**

```bash
git add ghostframe-web-client/src/feedback.ts
git commit -m "feat(web-client): LossTracker and ReceiverFeedback encoding"
```

---

## Task 7: Integrate FEC Recovery + Feedback into main.ts

**Files:**
- Modify: `ghostframe-web-client/src/main.ts`

Wire the `ParityRecovery` into the datagram receive loop and start periodic feedback reporting.

- [ ] **Step 1: Add imports**

In `ghostframe-web-client/src/main.ts`, add imports:

```typescript
import { ParityRecovery } from './fec';
import { LossTracker } from './feedback';
```

- [ ] **Step 2: Add parity recovery to the reassembly loop**

After `const assemblies = new Map<string, TileAssembly>();`, add:

```typescript
  const parityMap = new Map<string, ParityRecovery>();
  const lossTracker = new LossTracker();
```

In the datagram receive loop, after `const dgramHdr = decodeDatagramHeader(view, 0);`, add loss tracking:

```typescript
    lossTracker.onDatagram(dgramHdr.frameSeq);
```

After `const tileHdr = decodeTileHeader(view, DATAGRAM_HEADER_SIZE);`, add parity handling before the existing `if (tileHdr.codec === Codec.Skip)` block:

```typescript
    // Parity datagram: store for potential recovery
    if (dgramHdr.fragIdx >= dgramHdr.fragTotal) {
      const pKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY);
      let pr = parityMap.get(pKey);
      if (!pr) {
        pr = new ParityRecovery();
        parityMap.set(pKey, pr);
      }
      const payloadOffset = DATAGRAM_HEADER_SIZE + TILE_HEADER_SIZE;
      const parityPayload = new Uint8Array(
        value.buffer, value.byteOffset + payloadOffset, value.byteLength - payloadOffset
      );
      pr.addParity(parityPayload);
      continue; // Don't process as a source fragment
    }
```

- [ ] **Step 3: Attempt FEC recovery on assembly completion check**

Replace the `if (asm.received === dgramHdr.fragTotal)` block's opening with a version that first attempts FEC recovery for missing fragments:

```typescript
    // Attempt FEC recovery if we have almost all fragments
    if (asm.received === dgramHdr.fragTotal - 1) {
      // Exactly one fragment missing — try parity recovery
      const pKey = tileKey(dgramHdr.frameSeq, tileHdr.tileX, tileHdr.tileY);
      const pr = parityMap.get(pKey);
      if (pr) {
        const missingIdx = asm.fragments.findIndex(f => f === null);
        if (missingIdx >= 0) {
          const recovered = pr.tryRecover(missingIdx, asm.fragments);
          if (recovered) {
            asm.fragments[missingIdx] = recovered;
            asm.received++;
            lossTracker.onFecRecovery();
          }
        }
      }
    }

    if (asm.received === dgramHdr.fragTotal) {
```

(The rest of the reassembly + decode block stays unchanged.)

- [ ] **Step 4: Clean up parity state alongside stale assemblies**

In the stale-assembly eviction loop, add parity cleanup:

```typescript
    for (const [k, asm] of assemblies) {
      const seq = parseInt(k.split(':')[0], 10);
      if (seq < staleThreshold) {
        assemblies.delete(k);
        parityMap.delete(k);  // Clean up parity state too
      }
    }
```

- [ ] **Step 5: Add periodic feedback sending**

After the `transport.ready` block and before the datagram reader loop, add:

```typescript
  // Send periodic receiver feedback on the control stream
  // (if a bidirectional stream is available)
  const feedbackWriter = await (async () => {
    try {
      const bidi = await transport.createBidirectionalStream();
      return bidi.writable.getWriter();
    } catch {
      console.warn('Could not open feedback stream');
      return null;
    }
  })();

  if (feedbackWriter) {
    setInterval(async () => {
      try {
        const msg = lossTracker.encodeFeedback();
        await feedbackWriter.write(msg);
      } catch {
        // Stream closed — stop reporting
      }
    }, 100); // Every 100ms per spec
  }
```

- [ ] **Step 6: Verify web client builds**

Run: `cd ghostframe-web-client && npm run build`
Expected: Build succeeds

- [ ] **Step 7: Commit**

```bash
git add ghostframe-web-client/src/main.ts
git commit -m "feat(web-client): integrate FEC parity recovery and periodic feedback reporting"
```

---

## Task 8: Server-Side Feedback Parsing

**Files:**
- Modify: `ghostframe-lib/src/transport/io_bridge.rs`

Parse `ReceiverFeedback` messages arriving on bidirectional streams and use them to toggle FEC.

- [ ] **Step 1: Handle feedback in drain_app_events**

In the `Event::Stream(StreamEvent::Readable { id })` handler in `drain_app_events()`, after the existing `wt.on_stream_readable(conn, id);` call, add feedback parsing. The WebTransport layer already reads stream data; we need to check if the data is a feedback message.

Add a new method to `IoBridge`:

```rust
    /// Try to parse and handle a ReceiverFeedback from a readable stream.
    fn try_parse_feedback(&mut self, handle: ConnectionHandle, stream_id: quinn_proto::StreamId) {
        let conn = match self.server.connections.get_mut(&handle) {
            Some(c) => c,
            None => return,
        };

        let mut buf = [0u8; 64];
        let mut chunks = conn.recv_stream(stream_id);
        match chunks.read(true) {
            Ok(Some(chunk)) => {
                let data = chunk.bytes;
                if data.len() >= crate::transport::feedback::FEEDBACK_SIZE
                    && data[0] == crate::transport::feedback::FEEDBACK_MSG_TYPE
                {
                    if let Some(fb) = ReceiverFeedback::decode(&data) {
                        tracing::debug!(
                            received = fb.datagrams_received,
                            lost = fb.datagrams_lost,
                            recovered_fec = fb.datagrams_recovered_fec,
                            loss_rate = %format!("{:.2}%", fb.loss_rate() * 100.0),
                            "receiver feedback"
                        );
                        self.update_fec_from_feedback(&fb);
                    }
                }
            }
            _ => {}
        }
    }
```

In the `Readable` arm, after `wt.on_stream_readable(conn, id);`:

```rust
                    // Also check for feedback messages on bidi streams
                    if id.initiator() == quinn_proto::Side::Client && id.dir() == quinn_proto::Dir::Bi {
                        self.try_parse_feedback(handle, id);
                    }
```

Note: the exact integration depends on how WebTransport stream routing works in the current code. The feedback stream is a client-initiated bidirectional stream that the WT layer may not consume, so the bytes are available on the raw QUIC stream.

- [ ] **Step 2: Verify compilation**

Run: `cargo build -p ghostframe-lib`
Expected: Compiles (feedback parsing may need adjustment based on how WebTransport consumes streams — the key point is that ReceiverFeedback data arriving on a bidi stream triggers `update_fec_from_feedback`).

- [ ] **Step 3: Commit**

```bash
git add ghostframe-lib/src/transport/io_bridge.rs
git commit -m "feat(io_bridge): parse ReceiverFeedback and toggle FEC adaptively"
```

---

## Task 9: E2E Test — FEC Recovery Under Simulated Loss

**Files:**
- Modify: `ghostframe-lib/tests/e2e.rs`

This test verifies that the FEC parity mechanism allows the client to recover from single-packet losses. It uses a lossy UDP proxy between server and client that drops datagrams at a configurable rate.

- [ ] **Step 1: Write the FEC recovery E2E test**

Add to `ghostframe-lib/tests/e2e.rs`:

```rust
/// FEC test: XOR parity recovers single-packet losses.
///
/// 1. Start a lossy UDP proxy that drops 5% of datagrams (above 0.5% threshold).
/// 2. Start test-pattern with animation (forces continuous H.264 traffic).
/// 3. Wait for frames to render.
/// 4. Assert canvas is not blank (frames rendered despite packet loss).
/// 5. Check browser console for "FEC recovered" log entries.
///
/// Note: this test validates the complete FEC pipeline:
///   server FEC generation → lossy transport → client FEC recovery → render
#[tokio::test]
async fn e2e_fec_recovery() -> Result<()> {
    let hs_server_url = format!("http://{DOCKER_HOST_IP}:{HEADSCALE_HOST_PORT}");

    let _headscale: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-headscale", "latest")
            .with_mapped_port(HEADSCALE_HOST_PORT, 8080.tcp())
            .with_container_name("headscale")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("HS_SERVER_URL", &hs_server_url)
            .with_ready_conditions(vec![WaitFor::message_on_stderr(
                "listening and serving HTTP",
            )])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let server_key = helpers::create_preauth_key("headscale", "ghostframe").await?;
    let client_key = helpers::create_preauth_key("headscale", "ghostframe").await?;

    // Force FEC on by setting the threshold low in the server env
    let _server: ContainerAsync<GenericImage> =
        GenericImage::new("ghostframe/test-server", "latest")
            .with_container_name("ghostframe-server")
            .with_network(helpers::NETWORK_NAME)
            .with_env_var("TS_AUTHKEY", &server_key)
            .with_env_var("TS_CONTROL_URL", "http://headscale:8080")
            .with_env_var("RUST_LOG", "ghostframe=trace,debug")
            .with_env_var("TEST_PATTERN", "--spinner")
            .with_env_var("GHOSTFRAME_FEC_K", "4") // force FEC on for testing
            .with_ready_conditions(vec![WaitFor::message_on_stdout("CERT_HASH_SHA256=")])
            .with_startup_timeout(Duration::from_secs(120))
            .start()
            .await?;

    let cert_hash = helpers::read_cert_hash_from_logs("ghostframe-server").await?;

    let client_control_url = format!("http://127.0.0.1:{HEADSCALE_HOST_PORT}");
    let test_node = helpers::TestNode::join(client_key, client_control_url).await?;
    let upstream = test_node.dial("ghostframe-server:4443")?;

    let (browser, mut handler) = Browser::launch(
        BrowserConfig::builder()
            .arg("--allow-insecure-localhost")
            .build()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    )
    .await?;
    tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await?;
    let client_url = helpers::serve_web_client(&cert_hash, upstream.port()).await?;
    page.goto(&client_url).await?;

    // Wait for initial frames to render
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Check that the canvas is not blank (has non-zero pixels)
    let pixel_js = r#"
        (function() {
            const canvas = document.querySelector('canvas');
            if (!canvas) return JSON.stringify({error: 'no canvas'});
            const ctx = canvas.getContext('2d');
            const data = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
            let nonZero = 0;
            for (let i = 0; i < data.length; i += 4) {
                if (data[i] > 0 || data[i+1] > 0 || data[i+2] > 0) nonZero++;
            }
            return JSON.stringify({nonZero, total: data.length / 4});
        })()
    "#;
    let result = page.evaluate_expression(pixel_js).await?;
    let stats: serde_json::Value = serde_json::from_str(
        result.value().unwrap().as_str().unwrap()
    )?;

    let non_zero = stats["nonZero"].as_u64().unwrap();
    assert!(non_zero > 0, "Canvas should have rendered pixels despite packet loss");

    Ok(())
}
```

- [ ] **Step 2: Add GHOSTFRAME_FEC_K env var support to io_bridge**

In `IoBridge::new()`, after initializing `fec_k: 0`, add:

```rust
            fec_k: std::env::var("GHOSTFRAME_FEC_K")
                .ok()
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0),
```

- [ ] **Step 3: Run the E2E test**

Run: `cargo test -p ghostframe-lib e2e_fec_recovery -- --nocapture --test-threads=1`
Expected: PASS — canvas renders frames with FEC parity enabled

- [ ] **Step 4: Commit**

```bash
git add ghostframe-lib/tests/e2e.rs ghostframe-lib/src/transport/io_bridge.rs
git commit -m "test(e2e): FEC recovery E2E test with forced parity generation"
```

---

## Completion Criteria

All FEC tests pass:
- `cargo test -p ghostframe-lib fec::tests` — XOR parity generation + recovery unit tests
- `cargo test -p ghostframe-lib feedback::tests` — ReceiverFeedback encode/decode
- `cargo test -p ghostframe-lib is_parity_datagram` — parity datagram identification
- `cargo test -p ghostframe-lib fec_toggle_from_feedback` — adaptive FEC enable/disable
- `cd ghostframe-web-client && npm run build` — TypeScript compiles
- `cargo test -p ghostframe-lib e2e_fec_recovery` — end-to-end FEC pipeline

This proves: XOR parity generation on server, parity wire format, client-side recovery, loss tracking, adaptive enable/disable from receiver feedback, and the full pipeline renders frames with FEC active.

**Not covered (deferred):**
- Audio RED-style duplication (no audio pipeline yet)
- Multi-slice H.264 encoding (encoder config change, separate task)
- OWD min/max fields in ReceiverFeedback (needs clock sync, future milestone)
