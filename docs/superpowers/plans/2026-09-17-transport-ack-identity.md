# Transport-Identity ACKs — implementation plan

Spec: `docs/superpowers/specs/2026-09-17-transport-ack-identity-design.md`

Built on `investigate/degradation-detection`, which carries the loss plumbing
(`losses` in `packet_feedbacks`, the must-accompany-an-ACK constraint) and the
idle-gate commit that never landed. This work supersedes that branch's partial
NACK/RTO loss sourcing — the ledger replaces it as the loss source, and those
call sites are removed in task 6.

## Task 1 — a retransmission gets a fresh `wire_seq`

`emitter.rs`: `tick` (RTO) and `on_nack` re-queue cached bytes with the
original `wire_seq` intact. Both allocate a fresh one and stamp `bytes[8..12]`,
next to the existing `emit_us` re-stamp.

Test: retransmitting a submitted pass yields a datagram whose `wire_seq`
differs from the original, and the original is never emitted twice.

## Task 2 — the transmission ledger

New `transport/transmission_ledger.rs`. `record(wire_seq, emit_us, bytes,
EmitKey)` on emit; `resolve(wire_seq) -> Option<Record>` on ACK;
`expire(now, horizon) -> Vec<Record>` for the sweep. Bounded by horizon plus a
hard cap that evicts oldest-first and counts.

Tests: record/resolve/expire each classify exactly once; a resolve after expiry
returns `None`; the cap evicts oldest and counts.

## Task 3 — wire format

`ghostframe-protocol/src/ack.rs`: `AckEntry { wire_seq: u32, arrival_time_ms_lo16: u16 }`,
6 bytes. `ACK_BATCH_MSG_TYPE` 0x04 -> 0x06 (0x05 is `TILE_NACK_ENVELOPE`).
Register in `INBOUND_DISCRIMINATORS` so the collision and routed-byte tests
cover it.

Tests: round-trip including overlap entries; stale peer fails loud on the
message-type byte.

## Task 4 — client emits `wire_seq`

`reassembly.rs` already extracts `wire_seq` for the parity decoder; pass it to
`ack_batcher.add`. `ack_batcher.rs` entries carry it.

## Task 5 — server resolves `wire_seq`

Emit path records into the ledger. ACK path resolves to `EmitKey` and feeds the
existing consumers (`FragmentCoverageMap`, Cdf53 counting, PalRle tracking,
cancellation) unchanged. Unknown `wire_seq` ignored and counted.

## Task 6 — feed goog_cc from the ledger

Received from resolves, lost from the horizon sweep. Remove the NACK/RTO loss
sourcing from task 0's branch — superseded. Keep the must-accompany-an-ACK
rule: a loss-only report panics inside goog_cc.

Horizon: `clamp(RTTS x smoothed_rtt, FLOOR, CEIL)` per the spec.

## Task 7 — the property test

For arbitrary interleavings of emit / ACK / supersede / expire, every emitted
`wire_seq` is classified exactly once. Required by the spec; this is the
invariant everything rests on.

## Task 8 — acceptance

16 -> 4 Mbps degradation scene settles within 1.5x of the new capacity in every
run, not 1 in 6. Record the loss fraction goog_cc observes against the link's
real shed rate — that number is the direct evidence the accounting is complete.
