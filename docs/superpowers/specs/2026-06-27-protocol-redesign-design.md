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

### 5. Retransmit-until-superseded (NOT deadline-based)

**Rejected the deadline-drop approach** after design review. Our product target is lossless reconstruction; the existing invariant is correct: a cache entry lives until ACK, `cancel_for_tile` (bump_generation from dirty-path or escalation), or `clear_cache` (session end). This is already what we have — we keep it.

The trade-off accepted: unbounded memory in pathological cases (sustained 100% loss with no dirty changes). Mitigated by per-tile cap on simultaneous in-flight passes if needed.

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

## Controller choice: str0m's `bwe::Bwe` (GoogCC port)

Survey of Rust congestion-control crates landed on **str0m's `bwe::Bwe` module** as the battle-tested choice:

- **Production-grade GoogCC port** (Google Congestion Control as used in libwebrtc — trendline delay-based + loss-based + ALR detection + multi-stage probe controller).
- **MIT/Apache-2.0**, actively maintained (v0.21.0, 2026-06-27), 1.3M total downloads, used by BitWHIP SFU and others.
- **Standalone constructible** — `Bwe::new(initial_bitrate)` doesn't require an Rtc session.
- **Input shape matches our ACK protocol:** TWCC-style records `(packet_seq, arrival_timestamp)` → matches what we'd add to the ACK envelope in Phase 1.
- **Output:** `poll_estimate() -> Bitrate`. We derive pacing rate from that and feed it to the pacer.
- **Gaps:** doesn't directly produce a CWND (we'd compute `cwnd = bitrate × srtt / 8`) and doesn't suggest a FEC ratio (we wire that ourselves from observed PLR).
- **Adaptation effort:** 1–2 days. `cargo add str0m`, use only `bwe` module.

Alternatives ranked:
1. **quinn-proto `congestion::Bbr`** — vendor + adapt, marked experimental, 2-4 days.
2. **SCReAM C++ port to Rust** — purpose-built for media but no Rust port exists, 2-3 weeks.

Decision: **use str0m::bwe as a dependency.** Re-evaluate if the GoogCC delay-gradient model proves insufficient under our specific loss patterns (in that case fall back to porting SCReAM v2).

## Phased Implementation (revised)

This is too big to do at once. Proposing **three phases** (Phase 4 from the original draft was the deadline-based cache eviction, dropped per design review — lossless reconstruction is the product target).

### Phase 1: Instrumentation + measurement (no behavior change)

- **Embed server-side send timestamp in datagram header** (8 bytes for tile-pass datagrams). Client computes `(client_recv_now − server_send_ts)` per arrival. Absolute clock skew is irrelevant — relative comparison between paths/streams is what matters.
- **Receiver-side arrival timestamps in ACK envelope** (1 byte per pass, 1/128 s units, relative to envelope send time).
- **Per-tier (passes 0-3 vs 4-13) latency + bandwidth tracking on both server and client.** Histograms over rolling windows; reported in stats log every 2 s. Required for the QUIC-streams experiment evaluation.
- **Add str0m::bwe as a dependency.** Wire ACK arrival records into `Bwe::update()`. Run the estimator in parallel — log `bwe_estimate, plr_observed, delay_gradient_us, rtt` every second but **don't yet drive emission**.
- **Verify on real networks:** estimator should track expected behavior on healthy link (steady estimate) and rising loss (estimate drops, plr rises).

**Deliverable:** journal logs + page-log stats line with the new metrics. No behavior change yet. We have observability and the baseline data needed to evaluate Phase 2-3 changes.

### Phase 2: Priority pacer + queue restructure + str0m drives emission

- Replace single FIFO emission queue with multi-queue priority structure (libwebrtc PacedSender shape).
  - P0: Pass 0-3 retransmits
  - P1: Pass 0-3 fresh
  - P2: Pass 0-3 FEC parity
  - P3: Pass 4-13 fresh
  - P4: Pass 4-13 FEC parity
  - P5: Pass 4-13 retransmits
- Drain rate = `bwe.poll_estimate() × 0.95`. CWND = `bitrate × srtt / 8` as a safety cap on in-flight bytes.
- Keep RTO + FEC unchanged for this phase — only the pacer + controller-driven rate change.

**Priority delineation for first attempt: passes 0–3.** Re-evaluate after real-world data.

**Deliverable:** smoother emission, no bursts, no Blocked storms even under sustained loss. Pass 0–3 latency drops measurably (from Phase 1 metrics) without sacrificing pass 4–13 throughput on clean links.

### Phase 3: Adaptive FEC + per-pass NACK suppression

- Replace fixed K=10:R=1 with controller-driven FEC ratio.
- Per-frame `fec_percentage` field on the wire (1 byte).
- Hysteresis bands:
  - PLR < 2%: FEC 5%
  - 2% ≤ PLR < 10%: linear ramp 5%→25%
  - PLR ≥ 10%: 25% + force per-tile regeneration (bump_generation, re-emit fresh 14 passes from current GPU state) when loss has invalidated the refinement chain
- NACK suppression by priority:
  - Pass 0-3: NACK aggressively, ≤3 retries per pass, suppress < 0.5×RTT
  - Pass 4-13: NACK conservatively, ≤1 retry, suppress < 2×RTT

**Deliverable:** noticeably faster convergence under 5–20% loss; FEC overhead drops on clean links.

### Optional experimental: QUIC streams for pass 0 (LL3)

Run **only if Phase 1+2 metrics suggest pass 0 latency is bottlenecking convergence.** Use a small reliable QUIC stream for pass 0 of each tile; keep passes 1-13 on datagrams. Measure on both sides:
- Per-pass end-to-end latency (server send → client integrate-complete)
- Per-tier bandwidth utilization
- Head-of-line blocking incidents (stream stalls affecting subsequent datagram throughput)

If pass-0 latency drops measurably without harming pass 1-13 throughput or introducing HOL stalls: ship it. Otherwise discard.

## What we keep from current design

- **Per-pass ACK bitmap** (the dedup primitive is correct).
- **Gap-detection NACK on receive** + `nackedMask` (recent addition, already proven).
- **DatagramsUnblocked-driven continuation** (event-driven drain is correct).
- **Session-end cache clear** (Event::Connected prune from Task 19).
- **`pending_cache_entries` diagnostic** (proven useful in production debugging).

## Decisions resolved

1. **Controller base:** str0m's `bwe::Bwe` (GoogCC port) — battle-tested Rust dep, MIT/Apache, 1–2 days to integrate.
2. **Priority delineation:** passes 0–3 for first attempt; re-evaluate from real data.
3. **QUIC streams for pass 0:** experimental, predicated on Phase 1 metrics showing it would help. Requires per-pass latency + bandwidth tracking on both sides (folded into Phase 1).
4. **Deadline-based eviction:** rejected. Lossless reconstruction is the product target; cache entries live until ACK/cancel/clear.
5. **Land Phase 1 first as measurement-only PR**, evaluate before Phase 2-3: confirmed.

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
