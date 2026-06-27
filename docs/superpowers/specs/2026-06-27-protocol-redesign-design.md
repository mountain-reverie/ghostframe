# Pass-Progressive Codec Transport — Protocol Redesign Proposal

**Status:** proposal / awaiting review
**Date:** 2026-06-27
**Author:** Claude (research synthesis); review by Cedric

## Context

We have a UDP-based remote-desktop product that transports a progressive wavelet codec (CDF 5/3) over QUIC unreliable datagrams. The codec is provably bit-exact end-to-end (5 separate byte-equality tests on real captured content + synthetic data + GPU stages). Production blur is **not** from the codec — it's from the transport delivering fewer than 14 passes per tile under loss.

Current state delivers all 14/14 eventually but:
- Convergence is slow (minutes on lossy tailnet links).
- NACK volume blew past 10× actual losses before the receive-side dedup was added.
- We have no bandwidth estimator at the application layer.
- We have no priority ordering between visually-critical passes (0–3) and refinement passes (4–13).
- AIMD-on-frame-budget reacts at frame cadence (33 ms); state-of-the-art controllers pace at packet cadence (microseconds).

This proposal redesigns the transport around five principles distilled from state-of-the-art real-time media protocols.

## Five Design Principles

### 1. Two-signal congestion estimation: delay + loss

Every modern controller (GCC, SCReAM, BBR) rejects pure loss-based estimation. **Loss is too slow a signal** — by the time you see it, you've over-committed bandwidth. Delay-gradient detects congestion before queueing tips into drop.

For us: add receiver-side **arrival timestamps** in the ACK envelope; run a delay-gradient estimator on the sender. Treat loss as a *fallback* signal, not the primary.

### 2. Pacing model decoupled from CWND

Modern controllers compute pacing rate from a *model* (BBR's `pacing_gain × BtlBw`; SCReAM's `cwnd × 8 / srtt`) and use CWND as a *safety cap* on in-flight bytes. AIMD-on-frame-budget is fundamentally coarser than per-packet pacing.

For us: replace the per-frame AIMD multiplier with a continuous pacing-rate model. Drain the emission queue at that rate via a leaky bucket (libwebrtc PacedSender shape). Keep `send_buffer_space()` as a safety check.

### 3. Priority by visual importance, propagated everywhere

Universal in SVC/MoQ/libwebrtc/RemoteFX: low-frequency layers get priority at the scheduler, pacer, NACK suppressor, FEC ratio, and retransmit budget — at *every* layer of the stack.

For us: passes 0–3 (LL3 + first 4 bit-planes) are critical. Passes 4–13 are refinement. Wire this distinction into:
- Scheduler emission order
- Pacer priority queue
- NACK aggressiveness (passes 0–3 NACK fast, ≤3 retries; passes 4–13 NACK conservatively, ≤1 retry)
- FEC ratio (more parity for critical passes if loss is high)
- Retransmit budget (RTO retries only fire for passes 0–3 by default)

### 4. Adaptive FEC sized to observed loss + RTT

Fixed K=10:R=1 (9% overhead) is wrong on either side of ~9% loss. Sunshine ships a per-frame `fec_percentage` adjustable at runtime; the literature is consistent that **FEC rate should track observed PLR with hysteresis**.

For us: replace fixed FEC ratio with a controller that adapts in three bands:
- PLR < 2%: FEC 5% (cheap, just for parity-of-1 recovery of single losses)
- 2% ≤ PLR < 10%: linear ramp 5%→25%
- PLR ≥ 10%: 25% + force IDR-equivalent (regenerate all 14 passes from current GPU state) for tiles where loss has invalidated the refinement chain

FEC block size = min(per-frame source datagrams, 64) so block decode is incremental.

### 5. Deadline-tagged retransmits — no infinite retry

MoQ formalizes what every real system does implicitly: a tile-pass that can't reach the receiver in time is garbage. Frames have presentation deadlines. Once past deadline (or frame superseded), all its parity and retransmits are wasted bandwidth.

For us: every cache entry carries a `presentation_deadline` (frame_emit_ts + max_delivery_ms). On every RTO tick, evict entries past `now + 1×RTT`. Never retry past deadline. This bounds memory and stops perpetual churn on un-deliverable tiles.

## Proposed Architecture

```
┌───────────────────────────────────────────────────────────┐
│                  Application: GPU pipeline                │
└───────────────────────────────────────────────────────────┘
              │ tile-pass datagrams (priority-tagged)
              ▼
┌───────────────────────────────────────────────────────────┐
│  Priority Pacer (leaky bucket, multi-queue)               │
│    P0: Pass 0-3 retransmits                               │
│    P1: Pass 0-3 fresh                                     │
│    P2: Pass 0-3 FEC parity                                │
│    P3: Pass 4-13 fresh                                    │
│    P4: Pass 4-13 FEC parity                               │
│    P5: Pass 4-13 retransmits                              │
│    drain rate = pacing_rate from controller               │
└───────────────────────────────────────────────────────────┘
              │ paced datagrams
              ▼
┌───────────────────────────────────────────────────────────┐
│  Congestion Controller (delay-gradient + loss)            │
│    Inputs: ACK arrival timestamps, loss events, RTT       │
│    Outputs: pacing_rate, cwnd, fec_ratio_target           │
│    Algorithm: SCReAM-v2-shaped or GCC-shaped              │
└───────────────────────────────────────────────────────────┘
              │ quinn send_datagram
              ▼
┌───────────────────────────────────────────────────────────┐
│              QUIC (Quinn) datagram extension              │
└───────────────────────────────────────────────────────────┘

              ▲
              │ wire
              ▼
┌───────────────────────────────────────────────────────────┐
│             Receiver: gap-detect + ACK                    │
│   Per-pass bitmap (existing)                              │
│   + arrival_offset (per pass, 7 bits in 1/128 s units)    │
│   + frame_seq, pass_seq                                   │
│   NACK suppression by priority + RTT window               │
└───────────────────────────────────────────────────────────┘
```

## Concrete Wire-Format Changes

### ACK envelope (current → proposed)

**Current (per pass, 7 bytes):**
```
[frame_seq: 4B][tile_x: 1B][tile_y: 1B][pass_idx: 1B]
```

**Proposed (per pass, 8 bytes — adds 1B arrival offset):**
```
[frame_seq: 4B][tile_x: 1B][tile_y: 1B][pass_idx: 1B]
[arrival_offset: 1B in 1/128 s units, relative to envelope send time]
```

The 1-byte arrival_offset gives ~8 ms resolution over 2 s window, sufficient for delay-gradient input. Receiver fills it as `(now - first_recv_in_this_envelope) >> log2(7.8125ms)`. Sender's controller subtracts envelope-send-ts from receiver's arrival-time-implied to get one-way delay.

### NACK envelope: add `retry_count` field

**Current (per entry, 8 bytes):**
```
[frame_seq: 4B][tile_x: 1B][tile_y: 1B][pass_idx: 1B][frag_idx: 1B]
```

Keep the wire format; the receiver-side dedup we just added (`nackedMask` per generation) is the right abstraction. Add a per-pass `retry_count` only at the cache-entry level on the server — drop NACKs whose cache entry has hit the per-pass retry cap (3 for passes 0–3, 1 for passes 4–13).

### Datagram header: priority bit

Steal one bit from the existing `frame_seq | TILE_DATAGRAM_FLAG` field (top 2 bits are flag) to encode `IS_CRITICAL_PASS` (= pass_idx < 4). Receiver uses this for NACK prioritization without parsing the tile header.

## Phased Implementation

This is too big to do at once. Proposing four phases:

### Phase 1: Instrumentation + measurement (no behavior change)

- Add receiver-side arrival timestamps to ACK envelope.
- Add sender-side bandwidth/loss/delay logging at 1 s intervals.
- Build a delay-gradient estimator that *runs in parallel* but doesn't yet drive emission.
- Verify it matches expected behavior (low gradient on healthy link; rising gradient as loss starts).

**Deliverable:** journal log line every second with `bw_est, plr_observed, delay_gradient_us` — observable but not yet acted on.

### Phase 2: Priority pacer + queue restructure

- Replace single FIFO emission queue with multi-queue priority structure (libwebrtc PacedSender shape).
- Initial pacing rate = `bw_est × 0.95` (use the new estimator from Phase 1).
- Keep RTO + FEC unchanged for this phase — only the pacer changes.

**Deliverable:** smoother emission, no bursts, no Blocked storms even under sustained loss.

### Phase 3: Adaptive FEC

- Replace fixed K=10:R=1 with controller-driven FEC ratio.
- Per-frame `fec_percentage` field on the wire (1 byte).
- Hysteresis bands as described in Principle 4.
- Per-tile IDR fallback (regenerate all 14 passes) when loss invalidates refinement.

**Deliverable:** noticeably faster convergence under 5–20% loss; FEC overhead drops on clean links.

### Phase 4: Deadline-aware cache

- Add `presentation_deadline` to every cache entry.
- RTO tick evicts entries past `now + 1×RTT`.
- Server logs `evictions_past_deadline` so we can tune `max_delivery_ms`.
- Coordinate with classifier: tile that goes past deadline drops back to H264 fallback for that area until next dirty cycle.

**Deliverable:** bounded memory, no perpetual retry churn, clean fallback behavior.

## What we keep from current design

- **Per-pass ACK bitmap** (the dedup primitive is correct).
- **Gap-detection NACK on receive** + `nackedMask` (recent addition, already proven).
- **DatagramsUnblocked-driven continuation** (event-driven drain is correct).
- **Session-end cache clear** (Event::Connected prune from Task 19).
- **`pending_cache_entries` diagnostic** (proven useful in production debugging).

## Decisions needed from human

1. **Controller base: SCReAM v2 vs GCC vs custom?** SCReAM v2 is cleaner and smaller (~500 LoC) but in active draft. GCC is battle-tested in libwebrtc but bigger. Custom risks reinventing.
2. **Priority delineation: pass 0–3 vs 0–7 vs LL3-only?** The first 4 passes carry LL3 (16×16 low-pass) + first 3 bit-planes. LL3-alone is the critical layer for visual recognition; 0–7 adds first refinement. The literature is consistent that *just the base layer* is the "critical" tier; 0–3 may be too generous.
3. **Should pass 0 (LL3) use streams instead of datagrams?** MoQ-style hybrid. Trade-off: stream head-of-line vs reliable delivery of the critical layer. Probably worth a separate experiment.
4. **Deadline length: 1×RTT, 5×RTT, frame_duration?** Each is defensible. 5×RTT (~250 ms at 50 ms RTT) is common in interactive video; frame_duration (33 ms at 30 fps) is aggressive but matches the "tile is for this frame" semantic.
5. **Should we land Phase 1 first as a measurement-only PR**, then evaluate before committing to Phase 2-4? I'd recommend yes — the Phase 1 data tells us whether our model assumptions hold on real networks.

## Out of scope for this proposal

- Multipath / Multistream resilience (e.g. send pass 0 over both QUIC and TCP). Worth considering later.
- Encoder-side rate-control coupling (changing codec output bitrate based on transport feedback). Our codec is what it is; we can't trade quality for bandwidth at the encoder.
- Receiver-side reconstruction quality estimation (telling the sender "I'm getting SSIM X, send me more parity"). Interesting but deferred.

## Sources

Full research synthesis with citations is in the prior turn's research output. Key references:
- GCC: draft-ietf-rmcat-gcc-02, Carlucci et al. analysis
- SCReAM: RFC 8298 + v2 draft (draft-johansson-ccwg-rfc8298bis-screamv2-03)
- Sunshine/Moonlight: moonlight-common-c source, Sunshine docs
- MoQ: draft-ietf-moq-transport-18, Cloudflare blog
- libwebrtc: chromium.googlesource.com/external/webrtc PacedSender g3doc
- RFC 9221 (QUIC datagrams), draft-ietf-quic-ack-frequency
- BBRv2/v3: draft-cardwell-iccrg-bbr-congestion-control
