# FEC v2, revision 6: one erasure code over every server→client datagram

**Date:** 2026-11-07. **Code base:** origin/master `8626c5c1`; line numbers refer to that commit.
**Status:** design draft, not implemented. It revises [fec-v2-design.md](fec-v2-design.md) (revision 5, 2026-10-26). Anything in revision 5 that this document does not replace still holds. §1 says what carries over and what changes.

**How claims are marked** (as in revision 5):
- **[M]** measured;
- **[C]** read from code, with file:line;
- **[L]** from the literature;
- **[E]** estimate or inference.

The simulations are **[E]**. Where a number comes from one seed, the text says so; spreads come from the 5-seed check (`sim-rev6/seedcheck_out.txt`).

Files added by this revision:

| file | what |
|---|---|
| `fec-v2-rev6.md` | this document |
| `libs-survey-2026-11.md` | the library survey, 2026-11-07 (crates.io, GitHub, wasm probes) |
| `spike-rev6/` | the codec shootout: correctness, native and wasm speed, wasm size (`RESULTS.md`; rerun with `run.sh`) |
| `sim-rev6/` | emission timing read from the code (`TIMING.md`) and the simulation of block closing and code families (`FINDINGS.md`) |

---

## 0. Summary

- **What.** One erasure code covers every server→client datagram. H.264 fragments, tile passes, probe padding and control traffic all take the same path, with no split by class.
  - The server packs what each admission pass emits into MTU-sized bundles: one bundle = one QUIC packet = one symbol.
  - Bundles are grouped into Reed-Solomon blocks. A block closes at the first of:
    - k_max sources;
    - a time cap T;
    - the emitter running empty.
  - Both k_max and T adapt to the measured loss (§4, §5).
- **The rule the simulation teaches: protection against bursts comes from block span, and span costs latency.**
  - Under i.i.d. loss, small blocks (T ≈ 5 ms) suffice and recover in 13–17 ms at p95 **[E]**.
  - Under mean-burst-3 loss, H.264 needs a block span of about one frame interval. With 5 seeds at 30 Mbit/s, late frames are:
    - 7–10 % at T = 5 ms;
    - 0.9–1.5 % at T = 25 ms;
    - 0.7–1.6 % for RaptorQ with one block per frame.
  - At 100–200 Mbit/s, k_max = 64 binds (64 bundles last 3–6 ms): 13–19 % of frames are late at mean burst 8. Above about 70 Mbit/s a frame exceeds one GF(2^8) block, so it is split into interleaved blocks, which leave 0.4–1.8 % late, indistinguishable from one ideal block (§8.1) **[E]**.
- **Codec.** A systematic GF(2^8) Cauchy RS with repair row 0 = XOR, run on `rusty_erasure` 0.4.3 with our own matrix and our own e×e decoder.
  - Wasm cost: +9 to +12 KB gzip with `+simd128`.
  - About 3 µs in wasm to rebuild one loss at k ≤ 40 **[M]**.
  - The in-house prototype stays as a differential test oracle.
- **RaptorQ, Kyber's choice, is rejected.** What it gives under bursts is a large block, and RS with blocks of the same span comes within about 0.5 point of it **[E]**. Its costs **[M]**:
  - +113 KB of wasm;
  - a decode floor of about 270 µs in wasm (measured at k = 10);
  - one spare symbol per block.

  Above about 70 Mbit/s a 30 fps frame exceeds 256 bundles. Splitting it into interleaved GF(2^8) blocks matches one ideal GF(2^16) block, and matches RaptorQ per frame, within the seed spread (§8.1) **[E]**. So no large-field code is needed.
- **Sliding window: deferred.**
  - For: it is the latency winner under i.i.d. loss from 30 Mbit/s up **[E]**:
    - p95 3.1 / 0.9 ms at 30 / 100 Mbit/s, against 13–17 ms for time-capped RS;
    - at 10 Mbit/s it is level with RS.
  - Against:
    - under burst loss it is much worse for tiles (p95 72–447 ms at 10 Mbit/s against 17–21 ms) **[E]**;
    - no decoder that is fast enough exists;
    - revision 5's repair machinery is block-shaped.
- **Adaptation.**
  - Parity is sized for recovery on the first attempt, matched to the measured loss (about 1.5–2 × p, not a fixed 20 %). Measured bursts raise k_max and T before they raise r.
  - No FEC on a clean link.
  - Probe padding becomes extra repair symbols.
- **What carries over from revision 5, and what is new.** Revision 5's ACK v4, repair-status stream, reconstruction-first repair and deadline release carry over. **H.264 needs a change revision 5 did not make:** its fragments are ledger transmissions on master, and the acked-reference manager reads their acknowledgements, so a rebuilt fragment must release content to it (§5.3).
- **Retransmission** stays only as the lossless fallback for tiles. It may be removed for H.264 in the last PR.

---

## 1. What changes and what carries over

**Owner direction (2026-11-07).**
- **No split by class.** Erasure-code **every** server→client datagram at packet level. H.264 fragments, tile passes (PalRLE, CDF 5/3 pass 0 and refinement), probe padding and control datagrams all go through one encoder.
- **Low latency first.**
  - A retransmission costs at least an RTT plus detection. That is always too late for H.264, and usually too late for the critical tile tier.
  - FEC must carry the expected loss *first shot*.
  - Retransmission stays only as the lossless fallback for tile content. It may be removed for H.264 entirely at the end of this work.
- **A gate is dropped.** Revision 5's exit condition, "retransmissions per network loss ≤ 1.2 with FEC off" (§6.2), is no longer a gate. It measures ARQ, which this design stops relying on for latency.

**Replaced from revision 5:**

| rev 5 | rev 6 |
|---|---|
| §4.1 per-class blocks: H.264 per frame, tile bundles in running blocks, refinement passes unprotected | one stream of bundles and one block sequence; every bundle is protected while r > 0 (§2, §4) |
| §4.3 / §4.8.4 per-class adaptation, NACK-first at low RTT | one adaptation loop sized for first-shot recovery; burst protection by block span (§5) |
| §4.5 BUNDLE 0x09 / REPAIR 0x0A as new first bytes; H.264 frame parity at `frag_idx = n+1+j` | every s→c datagram is a frame of the FEC framing (§6.1). There are no free first bytes on master |
| HELLO caps bit 2 | bit 2 is `HELLO_CAP_H264_ACKED_REF` on master (`ghostframe-protocol/src/feedback_stream.rs:65`). Rev 6 uses **no bit**: the framing is unconditional (§8, item 7, decided) |
| REPAIR_REQUEST 0x0B and DECLINED: `class u8`, `block_id u32` (H.264: `frame_seq`) | same numbers; **the bodies change**: no class, `block_id u16` (§6.2) |
| ACK 0x06 retired only for FEC-v2 traffic; fragment and coverage NACKs kept for unprotected traffic (rev 5 §4.8.2 step 5) | ACK 0x06 retired; **TILE_NACK 0x05 kept** for tile content carried in PLAIN frames (§6.2) |
| §4.9.5: H.264 fragments carry no `bundle_seq` and get no RECOVERED | H.264 fragments travel in bundles like everything else; a rebuilt fragment's RECOVERED feeds the acked-reference manager (§5.3) |
| §4.2 codec: in-house GF(2^8) Cauchy RS | revisited with today's libraries, and against RaptorQ and sliding-window codes (§3) |
| §6.2 "D1 gates tile FEC" | D1 has landed; no gate |

**Carried over unchanged** (the revision 5 section is in brackets):
- FEC is application-level, inside QUIC DATAGRAM frames. Browsers cannot run a QUIC FEC extension (§4.1).
- One bundle = one QUIC DATAGRAM = one QUIC packet = one FEC symbol (§3, §4.1).
- **ACK v4** (§4.9):
  - Repair symbols are transmissions with their own sequence number. They are recorded in the ledger, billed to the pacer and ACKed on arrival.
  - A rebuilt source is never an arrival, so the RFC 9002 detector declares it lost.
  - RECOVERED, DECLINED and CONTENT_REJECTED travel on the reliable **repair-status stream** (owner decisions 2026-10-26, items 14–16).
- **Reconstruction first** (§4.8):
  - the sender keeps no timer on protected data;
  - the receiver closes a block's recovery window, then sends REPAIR_REQUEST for *new* repair symbols only if the deadline allows;
  - the last-resort timer `T_lr` stays;
  - the deadline release hands lossless content back to ARQ (§4.8.2 step 4).
- Pre-FEC loss and burst measurement from sequence gaps, and LOSS_REPORT (§4.4).
- Parity is billed to the bandwidth estimate (§4.3).

---

## 2. Where the layer sits in today's code

### 2.1 Server
- **Every s→c datagram goes through `IoBridge::send_counted`** (`ghostframe-lib/src/transport/io_bridge/emit.rs:294`) **[C]**.
  - It is reached through `send_to_all_sessions` (`emit.rs:277`). That covers the gated drain `drain_reliable_emitter` (`emit.rs:28` → `:82`) and ungated control traffic such as frame dimensions (`emit.rs:436`).
  - It ends in `WebTransportServer::send_datagram` (`webtransport.rs:424`, then `conn.datagrams().send` at `:453`).
- **The packer therefore sits at `send_counted`, per session**, not in the drain. `send_to_all_sessions` fans out to every session, and `bundle_seq`, the open bundle and the open block are per-session state.
- **Two sends bypass it** and must be routed through the packer: the pong reply (`io_bridge.rs:2843`) and the eviction notice, which is sent 3 times (`io_bridge/session.rs:532`) **[C]**. Both go out as PLAIN frames (§6.1).
- **Packing.** Datagrams are appended in order to the open bundle. It is flushed:
  - when the next datagram does not fit;
  - at the end of each drain;
  - before any PLAIN frame.

  Packing therefore adds no wait.
- **Size limit.** A bundle's records are capped so that its *coded symbol* plus the repair header fits `max_size()`. A repair is 10 bytes longer than the longest coded symbol of its block (§6.1).
- **quinn coalesces small datagrams.** quinn-proto writes queued DATAGRAM frames into one packet until the next one does not fit (`quinn-proto 0.11.14 connection/mod.rs:3336-3345`) **[C]**.
  - A full bundle fills a packet.
  - A short final bundle of a drain can share a packet with ACK frames, which is harmless. It can also share one with the next short bundle, which correlates their loss (§8, item 6).
- **Identity.** Each bundle and each repair symbol gets a `bundle_seq` from one per-session counter.
  - It replaces `wire_seq` as the ledger's transmission identity (`WireSeqAllocator`, `reliable_emitter/wire_seq.rs:23`; `TransmissionLedger::record`, `transmission_ledger.rs:555`).
  - Records inside a bundle carry `wire_seq = NO_ACK`. Otherwise the client's frame path would ACK them a second time by content (`frame_assembly.rs:64-72`).
- **MTU shrink.** `retire_oversize` (`emit.rs:128`) handles a datagram larger than the live limit.
  - With bundles, a shrink closes the open block at its current size.
  - Repair symbols computed at the old size that no longer fit are dropped, and the block counts as sent with fewer repairs.
  - The packer re-derives the cap for the next bundle.
  - H.264's `idr_mtu_shrink` remains for a fragment that no longer fits a bundle.

### 2.2 Client
- **One choke point.** Datagrams arrive at:
  - native: `ghostframe-client-net/src/lib.rs:397-416`;
  - wasm: `WasmClientCore::handle_datagram(s)` (`ghostframe-client-wasm/src/core.rs:105, :130`).

  Both reach `ClientCore::handle_datagram_payloads` (`ghostframe-client-core/src/reassembly.rs:126`) **[C]**. The frame parser, FEC decoder and unbundler go first in it. Each received or rebuilt record is then dispatched as today, carrying a `Provenance`:
  - tiles already have one (`reassembly.rs:52-58`);
  - the H.264 frame path gains one.
- **Short-datagram drops.** `main.ts:769` and `reassembly.rs:147` drop anything under 20 bytes, which today means ping/pong.
  - A PLAIN pong is longer than 20 bytes, so it reaches the core.
  - The TS-side pong handling must move into the PLAIN record dispatch, or it becomes dead code.
- **rAF batching.** The web client hands datagrams to wasm once per animation frame, or sooner when 32 are queued (`main.ts:698, 752-795`).
  - Arrival times are preserved.
  - Below about 18 Mbit/s, decoding and ACKs wait up to one display frame (16.7 ms at 60 Hz) **[E]**. That is a floor under recovery latency in the browser, and a separate work item (§8, item 9).
- **Bounds against a hostile or buggy server:**
  - at most N_open blocks held;
  - a REPAIR whose `k` exceeds k_max_limit (configured, ≤ 255 − r), or whose `k` disagrees with an earlier repair of the same block, is dropped and counted;
  - block state is freed at recovery, DECLINED, or when `block_id` falls a window behind.

### 2.3 Paths that interact with FEC
- **Retransmission paths on master** (rev 5 §4.8.3 still applies):
  - loss verdicts and PTO probes end in `request_retransmit` (`emitter.rs:1552`), which consults `classify_retx` (`emitter.rs:1437-1478`);
  - TILE_NACK ends in `on_nack` (`emitter.rs:1298`).
- **H.264 is retransmitted today, partly:** IDR fragments on a loss verdict, and any fragment on a PTO probe (`resend_h264_fragment`, `emitter.rs:687`; counters `h264_idr_resends` and `h264_probe_resends`). Revision 5 assumed it had no retransmission at all.
  - Under rev 6 these resends are refused while the fragment's block is recoverable.
  - They are removed if the owner drops H.264 retransmission (§7, R6-f).
- **The acked-reference manager reads per-fragment acknowledgements** (§5.3):
  - an acknowledgement goes through `frag_acked` and `on_h264_frame_acked` (`io_bridge/h264_tx.rs:555-572`);
  - a loss verdict on a P-frame fragment makes the frame suspect (`note_h264_verdict_in`, `h264_tx.rs:594`).
- **Probe padding** (`submit_h264_pad`, `emitter.rs:641`, called from `h264_rate.rs:710`) sends copies of acknowledged P-frame fragments to fill goog_cc's probe windows. Under rev 6 it becomes repair symbols (§5.1).
- **Existing FEC code to delete:**
  - `reliable_emitter/parity.rs` (`GroupBuilder`, 222 lines) and `feed_group` (`emitter.rs:810`);
  - `TransportConfig::tile_fec` / `GHOSTFRAME_TILE_FEC` (`config.rs:242-244, :451`) and the browserless `RunOptions::tile_fec`;
  - envelope 0x04 (`protocol.rs:637-720`);
  - `ParityDecoder` (`ghostframe-client-core/src/parity_decoder.rs`, 465 lines);
  - the ledger's parity ring (`transmission_ledger.rs:127`) and the XOR `fec_possible` heuristic (`transmission_ledger.rs:148-165`);
  - the admission parity reserve sized for k = 10 XOR (`PARITY_RESERVE_GROUP`), to be re-derived for repair symbols.

---

## 3. Code choice

### 3.1 Candidates

The survey (`libs-survey-2026-11.md`) looked at about 20 Rust crates and the main C libraries. Four went to the spike (`spike-rev6/RESULTS.md`).
- **Setup.** Native ran on a Ryzen 5 3600 (Zen 2), where rusty_erasure picked its AVX2 kernel; there is no GFNI on that chip. Wasm ran in Node 26 (V8), not in a browser. Symbols are 1 180 B.
- **Noise.** Wasm timings vary 1.5–1.8× from run to run, but the ranking held in every run.

| | `rusty_erasure` 0.4.3, our matrix | in-house GF(2^8) Cauchy (`wasm-size/gf256`) | `raptorq` 2.0.1 (Kyber's choice) | `cu-fec` 1.3.0 (RFC 8681 sliding window) |
|---|---|---|---|---|
| licence, maturity | MIT/Apache; an ISA-L port about 10 weeks old, one maintainer | ours | Apache-2.0, mature | new (≈300 downloads) |
| code | systematic MDS, GF(2^8), k + r ≤ 256 | same | rateless fountain | random linear, not MDS |
| repair row 0 = XOR | **yes**: we supply the matrix | yes | n/a | n/a |
| wasm Δ gzip | lean build (no e×e path): +6.8 KB scalar, +9.0 KB `+simd128`; full build with the e×e path: +10.0 / **+12.2 KB** | +2.7 KB | **+113 KB** | +5.0 KB |
| native encode µs, (k, r) = (10, 2) / (40, 8) / (200, 8) | **1.9 / 28 / 158** | 11.9 / 339 / 1 774 | 65 / 234 / 1 204 | (not comparable) |
| wasm decode µs, 1 loss, k = 10 / 40 / 200 | **~3 / ~3 / 12** (simd128, e×e) | 11–31 at k ≤ 40 | 267 / 721 / 3 160, whatever the loss | 126 at w = 10 |
| wasm decode µs, 8 losses, k = 40 / 200 | **64 / 300** | 303 at k = 40 | (as above) | up to 3 950 at w = 40 |
| decode failures with exactly k received | 0 (MDS) | 0 | 0.3–0.7 %; 0 of 4 000 at k + 1 | 0.3–0.5 % for 2–4 losses |

All numbers in this table are **[M]**.

### 3.2 Two findings the survey alone would have missed

- **Upstream `Coder::recover` inverts the full k×k matrix even for one loss.** That costs 2.7 µs at k = 10 and 280 µs at k = 200 natively, and it never uses the XOR row.
  - The spike's `spike-rev6/common/smalldec.rs` (about 70 lines, public API only) solves only the e×e system for the e missing sources. It runs the byte work through rusty's SIMD kernel, and uses `xor_gen` for a single loss.
  - The decode rows above use it. **We ship this wrapper and never call `recover` for k ≥ 40.**
- **"Decode returns the sources" does not detect a broken matrix.** A mutated matrix still round-trips. Only comparing repair bytes with an independent oracle catches it.
  - The in-house `gf256` therefore stays permanently, as a test-only differential oracle with the mutation test.
  - Spike A ran 53 627 checks, and both mutants were caught **[M]**.

### 3.3 Is Reed-Solomon still the right family, or should we use RaptorQ as Kyber does?

**RS, not RaptorQ.**

**What protects against bursts is the span of the block, not the code.** Under mean-burst-3 loss **[E]**:
- RaptorQ with one block per H.264 frame leaves 0.7–1.6 % of frames late at 30 Mbit/s (5 seeds).
- Time-capped RS blocks of k ≤ 64 at T = 25 ms leave 0.9–1.5 %.
- The same RS at T = 5 ms leaves 7–10 %.
- At 10 Mbit/s: RaptorQ 1.9–2.2 %, RS at T = 25 ms 2.1–2.9 %.
- At 100 Mbit/s the RS cap of 64 sources binds, and one seed gives RS 4.1 % against RaptorQ's 0.0 %. RS blocks of up to 200 sources per frame close that gap.

**Rateless repair is not needed.** Cauchy RS also sends "new repair symbols on request": any unused row up to 256 − k.

**RaptorQ costs us more** **[M]**:
- +113 KB of wasm;
- a wasm decode floor of about 270 µs, measured at k = 10;
- one spare symbol per block.

Used as one block per tile burst, its recovery p95 is 0.16–9.7 s **[E]**.

**Large-field codes are not needed above 70 Mbit/s either.** Above about 70 Mbit/s a 30 fps frame is more than 256 bundles, so one GF(2^8) block cannot span it.
- Splitting the frame into m interleaved GF(2^8) blocks gives the same late-frame rate as one ideal GF(2^16) block and as RaptorQ per frame. The 5-seed means are within 0.09 point at 100–200 Mbit/s under burst-8 loss **[E]**, §8.1.
- A contiguous split without interleaving is clearly worse.

**The sliding-window code is the one real alternative.** The uniform design is what makes it attractive: it has no block boundaries, and boundaries are the hard part of §4.

For it **[E]**:
- Under i.i.d. loss it has the best latency from 30 Mbit/s up: p95 3.1 / 0.9 ms against 13–17 ms for time-capped RS.
- At 10 Mbit/s it is level with RS (9.4 ms each on tiles).
- At 2 Mbit/s it costs 20 % where RS costs 27–32 %.

Against it:
- **Bursts.** With a 64-symbol window at 10 Mbit/s, tile recovery p95 is 72–447 ms and H.264 recovery p95 78–87 ms, against 17–22 ms for RS at T = 5 ms **[E]**.
- **No decoder.** None is fast enough: cu-fec takes 126 µs to 4 ms in wasm **[M]**.
- **Not MDS:** it needs one spare repair.
- **Machinery.** Revision 5's repair machinery is block-shaped:
  - REPAIR_REQUEST(block, m);
  - DECLINED's missing bitmap;
  - the held(B) ≥ k deferral;
  - the deadline release.

**Recommendation:** RS blocks with `rusty_erasure` and the e×e wrapper. The frame header keeps a scheme byte (§6), so a window mode can be added later. Revisit it for the native client once LOSS_REPORT shows real burst lengths.

---

## 4. Blocks: when they close, how big they are

**Timing read from code** (`sim-rev6/TIMING.md`):
- With a backlog, admission passes are at least 1 ms apart and at least one datagram's tokens apart (`pacing.rs:146-153`).
- In the simulation the median pass emits 1 bundle at 2 and 10 Mbit/s, 2 at 30 and 9 at 100. TIMING's analytic table says about 1 / 1 / 3 / 10 **[E from C]**.

**Closing a block at the end of each pass alone is rejected.** At 2–10 Mbit/s most blocks are a single bundle, so r ≥ 1 is mostly a repetition code. The real overhead with r_min = 1 is 100 / 69 / 45 / 22 % at 2 / 10 / 30 / 100 Mbit/s **[E]**.

**Rule.** A block closes at the first of:
1. **k = k_max.** The default is 64; §5 raises it under measured bursts, up to 256 − r.
2. **The time cap T**, counted from the block's first source. The default is T = min(max(5 ms, 6 bundle slots), frame interval):
   - 28 ms at 2 Mbit/s (30 fps);
   - 5.6 ms at 10 Mbit/s;
   - 5 ms from 30 Mbit/s up.

   §5 raises it to the frame interval under measured bursts.
3. **Empty:** the emitter has no content left (`pacing::Stop::Empty`, `emit.rs:102`), as opposed to running out of tokens or depth with content still queued.
4. **MTU shrink** (§2.1).

**When the repairs go out:**
- **A block closed on k_max or Empty:** in the same drain, right after its last source, inside the admission budget. Repair symbols are charged like sources, as parity is today (`admit_one`, `admission.rs:275`).
  - If there are not enough tokens, they go first in the next pass.
  - A close on Empty also arms the token wake, so the repair does not wait for new content.
- **A block closed by T:** the timer is one more deadline in `next_wake_deadline` (`io_bridge.rs:2107`).

Each repair is recorded in the ledger as a transmission (rev 5 §4.9.1).

**What the simulation does and does not support:**
- **Time cap.** It supports one, but ran only fixed values (T = 2, 5, 10, 25, 33 ms). The formula above was not simulated.
- **Empty close.** For RS it was not simulated. The simulator closes RS blocks only on k_max or the timer. Its evidence comes from the sliding-window runs:
  - recovery p99 on tiles: 606 ms without a flush, 75 ms with one, at 2 Mbit/s;
  - late H.264 frames: 3.57 % without, 0.80 % with.

  Treat rule 3 as necessary but **unmeasured for RS**. Add it to `sim-rev6` before R6-d.
- **H.264 frames.** A frame is queued at commit (`frame.rs:1749`), and the emitter usually runs empty before the next one. Rules 2 and 3 therefore make a block roughly one frame's worth of traffic at low rate. Neither rule knows about frames **[E]**.
- **Bursts need span.**
  - Mean-burst-3 loss at 30 Mbit/s, 5 seeds: late frames are 7.3–9.6 % at T = 5 ms and 0.9–1.5 % at T = 25 ms **[E]**.
  - The cost is latency: recovery p95 is 18–20 ms at T = 5 ms and 21–24 ms at T = 25 ms.
  - At 100 Mbit/s, 64 bundles take about 6 ms, so k_max binds before T. Burst protection there needs k_max raised, not T.
- **Interleaving across blocks** did not pay in the simulation **[E]** and is dropped.

**k is not known while sources are sent.** A source carries only `(block_id, idx)`, plus a LAST flag when the packer already knows the bundle closes the block (k_max reached, or the final bundle of an Empty drain). A repair carries k and the block's planned r (§6).

**How the client finds out k, by case:**
- **LAST flag or a repair arrives:** the client learns k from it.
- **Neither:** a repair it can rebuild from means at least k of k + r symbols arrived. If none can arrive because every repair was lost, the client closes the block at `first source + T_adv + A`:
  - `T_adv` is the server's current T, advertised on the feedback stream;
  - `A` is the allowance of rev 5 §4.8.2.

  It then sends REPAIR_REQUEST with `have` and `missing` taken from `idx` gaps. The server's `T_lr` (rev 5 §4.8.3) also covers a block whose repairs and LAST source were all lost: it sends one more repair, which carries k.

---

## 5. Adaptation

**One loop, one estimate.**
- **Input:** the client's pre-FEC loss rate and run-length histogram (LOSS_REPORT, rev 5 §4.4), and RTT.
- **Output:** (r per block, k_max, T).
- **Cadence:** about every 250 ms, with an EWMA and hysteresis (rev 5 §4.3).

**First-shot sizing.** The owner's direction is that retransmission is too late, so every block is sized for `rounds = 0` (rev 5 §4.8.4):
- **r:** the smallest r with P(more than r of the k + r symbols lost) ≤ target, under the fitted Gilbert-Elliott model, with r_min = 1. A table is precomputed offline for k = 1..255.
  - Matched to the loss, not a fixed percentage: about 1.5–2 × p suffices for i.i.d. loss.
  - At a fixed 20 %, a 3 MB first paint at 10 Mbit/s would pay 0.5 s **[E]**.
- **Bursts widen blocks before they add parity.**
  - When the fitted mean burst exceeds about 1.5, T rises toward the frame interval and k_max toward 256 − r.
  - **Above about 70 Mbit/s, blocks span a frame by splitting it** into m = ceil((k + r)/256) interleaved blocks (§8.1).
    - The small-block default is not enough there.
    - With 5 seeds at 100–200 Mbit/s and 20 % overhead, k_max = 64 leaves 4–5 % of frames late at mean burst 3 and 13–19 % at burst 8. Frame-spanning blocks leave 0.4–1.8 % at burst 8 (`sim-rev6/FINDINGS.md` §6) **[E]**.
  - **The cost is recovery latency for what is not an H.264 frame.**
    - Frame-spanning blocks put a lost source's recovery after the frame's last symbol: p95 62–70 ms at 100–200 Mbit/s, against 10–20 ms for small blocks **[E]**.
    - For H.264 that costs nothing. A frame cannot be decoded before all its fragments are there, and frame lateness is the metric above.
    - For tiles in the same block it is real latency. Under measured bursts it is still well under ARQ's RTT + detection.
  - Only when the span is at its limit (a frame) does r rise instead.
- **Clean link: no FEC.**
  - Below a measured pre-FEC loss floor, r = 0 and traffic goes in PLAIN frames (§6). r_min = 1 alone would cost at least 1/k: about 17 % at 2 Mbit/s with no loss.
  - Above the floor, r = 1 at the first measured loss.
  - The floor value is an owner decision (§8, item 5).
- **Congestion versus random loss** is unchanged from rev 5 §4.3 step 5. Loss that coincides with delay growth, or with quinn congestion events, does not raise r.

### 5.1 Padding becomes repair
- **Today:** goog_cc's probe windows are filled with copies of acknowledged P-frame fragments (`submit_h264_pad`, `emitter.rs:641`; `h264_rate.rs:710`). The client acknowledges them and discards them.
- **Rev 6:** the padding is **extra repair symbols** of the most recently closed block, or of the open block, which is then closed.
  - They are full transmissions: each has a `bundle_seq`, an ACK and a ledger record.
  - The probe bytes protect content instead of being thrown away.
- **Three differences goog_cc's accounting must absorb:**
  1. A padding symbol's size is the block's symbol size, not a free choice, so the window is filled in whole symbols.
  2. A block has at most 256 − k rows. A long window can run out and must move to the next block. If no block is retained (none since the link went idle), it falls back to PLAIN filler.
  3. The block's sources must stay cached until its padding is sent.
- **"A lost pad pauses the raise"** (`h264_tx.rs:751-757`) becomes "a lost padding repair symbol pauses the raise". The ledger marks which repair symbols were padding.
- The probe accounting is therefore **close to** today's, not identical. R6-e validates it against the browserless BWE suites.

### 5.2 Retransmission under rev 6
- **Tiles:**
  1. FEC first shot.
  2. Then REPAIR_REQUEST.
  3. Then the deadline release to ARQ (rev 5 §4.8.2 step 4).

  Lossless content is always delivered in the end. Content in PLAIN frames (r = 0) keeps today's ARQ: the RFC 9002 detector on `bundle_seq`, and TILE_NACK.
- **H.264:**
  - FEC first shot, then the acked-reference manager's recovery for what remains.
  - REPAIR_REQUEST rarely fits a frame's deadline. It is kept because it costs nothing when unused.
  - The IDR and PTO fragment resends (`resend_h264_fragment`, `emitter.rs:687`) are refused while the block is recoverable.
  - **Removing H.264 retransmission entirely** is the last PR (§7, R6-f), decided on LOSS_REPORT data.

### 5.3 H.264 under ACK v4: a rebuilt fragment must not start a recovery

Revision 5 §4.9.5 assumed H.264 fragments carry no `bundle_seq` and are not ACKed. On master they are ledger transmissions, and the acked-reference manager is driven by their acknowledgements:
- `frag_acked` and `on_h264_frame_acked` (`h264_tx.rs:555-572`);
- a loss verdict on a P-frame fragment makes the frame suspect (`note_h264_verdict_in`, `h264_tx.rs:594`).

Under ACK v4 a rebuilt source is never an arrival, so the detector judges the bundle lost. **If nothing else changes, every frame FEC repaired starts a recovery**, which throws away the point of H.264 FEC.

Rev 6 therefore separates transport from content for H.264, as rev 5 did for tiles:
- **The ledger verdict on a bundle stays congestion state.** It feeds goog_cc and `loss_window` only, never the manager.
- **The manager is driven by content.** A fragment is delivered when its bundle arrives, or when a RECOVERED names the bundle. It is lost when its block is closed short: a DECLINED record, or a REPAIR_REQUEST whose deadline passes.
  - The server maps bundle → fragments, as the tile cache maps bundle → passes.
- **RECOVERED becomes latency-relevant for H.264.** The repair-status stream is reliable, but a lost stream packet delays the release by an RTT.
  - FRAME_ACK (0x0A, the decoded mask) already tells the manager which frames decoded.
  - R6-d must decide whether FRAME_ACK alone is enough to clear a suspect frame (`suspect_overridden` exists today), or whether the manager waits for RECOVERED.

---

## 6. Wire format

### 6.1 Server → client: every datagram is an FEC frame

**Why a new framing.** No first byte is free on master **[C]**:
- 0x00–0x7F is the high byte of an H.264 `frame_seq`;
- bit 31 marks a tile;
- 0x04 is the parity envelope (`protocol.rs:404-436, :648`).

**The framing is unconditional** (owner, 2026-11-07, §8 item 7). **Every** s→c datagram of every session is one of the frames below, from the first datagram on. There is no HELLO bit and no unframed path, and the old discriminators live only inside records.

```
SOURCE   [0] 0x01  [1] scheme | flags     scheme (low 4 bits): 0 = RS GF(2^8) Cauchy, row 0 XOR;
                                          flags: bit 7 LAST (this source closes its block)
         [2..6]  bundle_seq u32 LE        transmission identity (ledger, ACK v4)
         [6..8]  block_id   u16 LE        wraps; see "Bounds" below
         [8]     idx_in_block u8          0..k_max-1
         [9..]   records: { len u16 LE, datagram bytes as today (tile / H.264 fragment / sentinel) }...
REPAIR   [0] 0x02  [1] scheme
         [2..6]  bundle_seq u32 LE        own identity, same counter as SOURCE (rev 5 §4.9.1)
         [6..8]  block_id   u16 LE
         [8]     k u8                     the block's source count
         [9]     r u8                     the block's planned repair count at close
         [10]    repair_idx u8            0 = XOR row; >= r for requested or padding symbols; < 256 - k
         [11..]  repair payload (the length of the block's longest coded symbol)
PLAIN    [0] 0x03  [1..5] bundle_seq u32 LE  [5..] records    no block: pong, eviction, r = 0 traffic,
                                                              and the ARQ fallback
```

**The coded symbol of a source** is `len u16 LE ‖ bundle_seq ‖ records`, zero-padded to the block's longest coded symbol.
- `block_id` and `idx` are left out: the receiver knows them from the block and position it is rebuilding.
- A rebuilt source therefore carries its exact length and its `bundle_seq`. There is no `lens` array (spike E).
- **Systematic decoding works before k is known.** Source bytes do not depend on k.
- **Repair coefficients depend only on (repair_idx, idx_in_block).** They do not depend on k or on the planned r: `coef(i, j) = c(i, j) / c(0, j)` with `c(i, j) = 1 / (x_i + y_j)`, `x_i = 255 − i` and `y_j = j` over GF(2^8)/0x11d.
  - Every block is a submatrix of one 256-point Cauchy matrix, so it is MDS whenever k + (largest repair_idx + 1) ≤ 256.
  - Row 0 is all ones (XOR).
  - This is what lets the server fold each source into the repair rows as it is sent (`Coder::update`), and lets requested and padding rows (repair_idx ≥ r) belong to the same code.
  - The spike's matrix (`x_i = i`, `y_j = r + j`) depended on r, so it could do neither.
- **Sizes.** For a source with R bytes of records, the frame is 9 + R bytes and its coded symbol 6 + R. The largest repair of the block is therefore 17 + R_max. The packer caps R at `max_size() − 17`, which wastes 8 bytes on a full source bundle.

**Bounds.**
- `block_id` u16 wraps after 65 536 blocks: more than 30 s at 2 000 blocks/s.
- The client holds at most N_open blocks. A block that falls a window of 1 024 behind the newest `block_id` is pruned and DECLINED (rev 5 reason "pruned").
- The server's block cache is bounded by the deadline release, which is far shorter than a wrap.

**Short frames.** The core's under-20-byte guard (`reassembly.rs:147`) no longer applies under the framing. Ping/pong becomes a PLAIN record.

### 6.2 Client → server
- **ACK v4 0x08** (arrivals by `bundle_seq`), **LOSS_REPORT 0x07** and **REPAIR_REQUEST 0x0B** keep their revision-5 numbers. All three are free on master (`classify_inbound`, `protocol.rs:955-987`).
- **REPAIR_REQUEST and DECLINED bodies change:** the `class` byte is dropped, and `block_id` becomes u16 to match §6.1. H.264 is no longer addressed by `frame_seq`.
- **The repair-status stream** carries RECOVERED, CONTENT_REJECTED and DECLINED, as in revision 5. Its record types 0x10–0x12 are not used on master's feedback stream, whose types are 0x01, 0x03–0x05 and 0x07–0x09 (`feedback_stream.rs:28-50`).
- **0x0A is FRAME_ACK** on master (`protocol.rs:844`). Revision 5's REPAIR 0x0A went server→client and is frame kind 0x02 above, so nothing collides.
- **ACK 0x06 is retired** under the framing. **TILE_NACK 0x05 stays** for tile content in PLAIN frames, the unprotected traffic of rev 5 §4.8.2 step 5. Content in a block uses REPAIR_REQUEST and DECLINED instead.

---

## 7. Sequencing

| PR | content | wire |
|---|---|---|
| R6-a | `ghostframe-protocol`: frame formats (SOURCE / REPAIR / PLAIN, ACK v4, LOSS_REPORT, REPAIR_REQUEST, repair-status records); the codec (`rusty_erasure` pinned or vendored, our matrix, the e×e decoder); `gf256` as a test-only oracle with differential proptests and the mutation test | formats only |
| R6-b | client-core / wasm / native: framing, unbundle, decode, bounds (§2.2), ACK v4, LOSS_REPORT, repair-status stream, metrics, REPAIR_REQUEST, H.264 `Provenance` | client |
| R6-c | server: per-session packer at `send_counted`, pong and eviction through it, `bundle_seq` as the ledger identity, records with `NO_ACK`, everything in PLAIN (r = 0); **delete the XOR tile FEC** (§2.3); re-baseline the browserless scenes (one datagram per packet changes ACK and BWE granularity, rev 5 §5); measure the coalescing rate of short bundles (§8, item 6) | server |
| R6-d | server: block closing (§4, with the Empty close added to `sim-rev6` first); repairs at close; the H.264 content/transport split (§5.3); protected retransmit rules; REPAIR_REQUEST serving; deadline release; a fixed (k_max, T, r) behind a knob | — |
| R6-e | adaptation (§5) and padding as repair (§5.1), validated against the BWE suites | — |
| R6-f | remove H.264 retransmission (`resend_h264_fragment`) if LOSS_REPORT data supports it | removal |

The validation plans of rev 5 §4.6 and §4.9.8 apply: every property test comes with the break that must fail it. From R6-c on, the gates include the browserless suites over at least 3 seeds and the Docker e2e (a server path changes).

---

## 8. Decisions for the owner

**Decided 2026-11-07:** every item except 3 is decided as recommended below; items 4 and 6 follow the analysis in §8.1 and §8.2. Item 3 (RS now, a sliding window later) stands as recommended.

1. **Codec: `rusty_erasure` with our matrix and the e×e decoder.** **Decided: pinned `=0.4.3`**, with `gf256` as the permanent differential oracle.
   - Vendoring (about 5 000 lines) was the alternative.
   - The in-house code alone remains the fallback, with multi-repair encode 6–12× slower.
2. **Build the web client with `+simd128`.** It is supported by every evergreen browser. **Decided: yes.** Measure the whole bundle when it is switched on.
3. **RS blocks now, a sliding window later** (§3.3). *Recommend RS.* The scheme byte keeps the door open.
4. **Frame-spanning blocks above about 70 Mbit/s** (§3.3). The choice is between:
   - (a) two or more interleaved GF(2^8) blocks per frame;
   - (b) a GF(2^16) code for large blocks, RaptorQ or Leopard, with about 0.3–0.9 ms of wasm decode per block.

   **Decided: (a), interleaved** (§8.1).
5. **Clean-link floor** (§5): the measured pre-FEC loss below which r = 0. **Decided: 0.1 %**, with r = 1 restored at the first loss above it. Revision 5 used 0.3 % where a repair round fits, and r = 1 at any loss where none does.
6. **The last bundle of a pass.** Pad it to `max_size()`, or accept that quinn occasionally puts two short bundles in one packet. **Decided: accept coalescing and don't pad**; count it in R6-c (§8.2).
7. **Negotiation.** **Decided: the framing is unconditional.** There is no HELLO bit, and every s→c datagram of every session is framed (§6.1). There are no deployments to stay compatible with (owner, 2026-10-15). This removes a compatibility matrix and the pre-HELLO ambiguity.
8. **Remove H.264 retransmission** (R6-f). **Decided:** decide on LOSS_REPORT data after R6-e.
9. **The web client's rAF batching** puts up to 16.7 ms under browser recovery below about 18 Mbit/s (§2.2). **Decided:** a separate work item, worth doing for its own sake.

### 8.1 Item 4: blocks that span a frame above about 70 Mbit/s

**The problem.** A 30 fps frame at 100 Mbit/s is about 350 bundles. GF(2^8) caps a block at k + r ≤ 256. Below about 70 Mbit/s a frame fits in one block and nothing needs deciding.

**(a) Two or more GF(2^8) blocks per frame, interleaved** (bundle i goes to block i mod m).
- **For:**
  - Same codec, same wire format, same fast decoder, no new dependency.
  - Interleaving splits a burst of b losses across the m blocks, about b/m each, so burst protection is largely kept.
  - All m blocks still end with the frame, so no delay is added.
  - Each block's decode stays in the µs range.
- **Against:**
  - Not MDS over the frame. The same total repair R, split R/m per block, fails when the losses split unevenly, even if their total is ≤ R.
  - The penalty grows with the burst length relative to m, and the residual at the same overhead is somewhat higher than one block.
  - The k_max and T logic gains a "split" mode.

**(b) One GF(2^16) block over the frame**, using Leopard (`reed-solomon-simd`, MDS) or RaptorQ (not MDS).
- **For:**
  - One block whose code is optimal over the whole frame: the lowest residual for a given overhead.
  - Leopard encodes fast natively (62 µs at k = 200, rev 5 §4.2).
- **Against:**
  - A second codec in the client: +10 KB (Leopard) or +113 KB (RaptorQ).
  - A second differential oracle to maintain.
  - A fixed decode cost per lossy block of about 0.9 ms in wasm for Leopard, whose wasm SIMD support is still an unmerged pull request. At 30 fps that is about 3 % of the main thread per lossy frame.
  - No XOR fast path: one loss costs the same as many.
  - Leopard needs even shard sizes; RaptorQ needs a spare symbol.

**Simulated** (`sim-rev6/split_sim.py`, `FINDINGS.md` §6) **[E]**:
- **Grid:** 70–200 Mbit/s; H.264 only and the mix; i.i.d. 1–2 % and Gilbert-Elliott 2 % with mean burst 3, 8 and 16; overhead 10–30 %. 5 seeds for the burst-3 and burst-8 cells at 100 and 200 Mbit/s.
- **Interleaved (a) and ideal MDS (b) cannot be told apart:**
  - the 5-seed means differ by at most 0.09 point, inside the seed spread (0.1–0.4);
  - the sign varies from cell to cell;
  - no cell of the full grid differs by more than 1 point.
- **RaptorQ per frame matches (b)** in every cell.
- **The contiguous split** (control, no interleave) is clearly worse under bursts: 2.1 % of frames late at burst 8, against 0.4–1.2 %. Interleaving is what keeps (a) at MDS level.
- **Typical numbers** (H.264, burst 8, 20 % overhead, 5-seed means; late frames for (a) / (b)):
  - 100 Mbit/s: 1.23 / 1.22 %;
  - 200 Mbit/s: 0.40 / 0.49 %.

**Decided: (a)**, with m = ceil((k + r)/256), bundle i in block i mod m, and r = ceil(R/m).
- It needs no second codec and no second oracle.
- It has no fixed 0.9 ms wasm decode per lossy frame.
- It keeps the XOR fast path.
- The model leaves out CPU cost and time-correlated loss. Both favour (a).

### 8.2 Item 6: pad the last bundle of a pass, or accept coalescing

**When two bundles share a packet in practice.**
- quinn-proto puts DATAGRAM frames into one packet only while the *next* one fits (§2.1). A full bundle never shares.
- In a pass "full, full, short", the short bundle is followed by the next pass's first bundle, usually full, so it does not share either.
- Two bundles share a packet only when both are short and both sit in quinn's queue at once:
  - tiny passes back to back (a cursor-sized tile update, a small P-frame at idle, a pong);
  - a short bundle behind a backlog at high rate.
- At 2–10 Mbit/s quinn's queue is nearly empty between passes (`D_target` is 2–10 datagrams), so it should be rare **[E]**. At high rate with a standing queue it is more likely, but there the passes fill bundles, so few are short **[E]**.

**What sharing costs when it happens.**
- A lost packet erases two symbols, usually of the same block: a burst of 2 for RS to absorb.
- It is a protection loss, not a correctness problem:
  - every symbol is still decodable from k others;
  - the client's pre-FEC loss measurement sees the 2-burst, so the adaptation raises protection for it automatically, if a little pessimistically.
- Congestion control is unaffected: two bundles lost together are two `bundle_seq` losses, as two datagrams in one packet are today.

**What padding costs.**
- Every short final bundle is grown to about 1 200 B.
- That is the worst case at low rate and small damage, exactly where most passes are a single short bundle. Small updates could cost 2–10× their bytes:
  - a 150 B cursor-area update becomes 1 200 B;
  - at 2 Mbit/s that is wasted capacity, billed to the pacer like any other byte.
- It also delays nothing, but it does take tokens that the next frame would have used.

**A third option** is to hold a short bundle briefly so the next pass can fill it. It is rejected: it adds latency, which is the one thing this design must not do.

**Decided: accept coalescing and don't pad.**
- Count it in R6-c. The server sees it without quinn's help: a short bundle handed to quinn while its datagram queue already holds a short bundle can share.
- Pad only if that count shows a real rate of shared packets *and* LOSS_REPORT shows the 2-bursts costing residual.
