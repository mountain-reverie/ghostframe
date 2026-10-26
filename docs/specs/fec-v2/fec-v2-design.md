# FEC v2 and loss-adaptive repair for ghostframe: research and design

**Date:** 2026-10-15. **Code base:** origin/master `5098db6`. Line numbers refer to that commit (`git show 5098db6:<path>`).
**Scope:** test the owner's hypothesis that broken FEC causes the retransmission volume, survey the state of the art (including "Kyber"), and propose a design, protocol change and schedule.
**Revision 4 (2026-10-26, owner decision):** the ACK v4 design is replaced (§4.9). Repair symbols are transmissions with their own `bundle_seq`, ACKed like sources. A source the client rebuilt is not acknowledged as an arrival. A separate RECOVERED report releases its content (an ACK v4 section in revision 4; a reliable-stream record since revision 5). This replaces revision 1's `recovered` bit in `arrival_time_ms_lo16`. Code references in §4.9 are to origin/master `93e5efec`.
**Revision 5 (2026-10-26, owner decisions on §6.4 items 14-16):** RECOVERED, CONTENT_REJECTED and a new DECLINED record leave the ACK datagram and travel on a reliable client→server QUIC stream, the *repair-status stream*, a second client-opened bidirectional stream (§4.9.2). Each is sent once, and quinn or the browser's WebTransport retransmits it on loss; the 3-copy repeat and the 48-id cap are gone. ACK v4 (0x08) is arrivals only. Because every terminal client outcome of a block is now stated reliably, `T_def` becomes a 60 s backstop for the lossless (tile) classes, an alarm for a client bug, and H.264 has no deferral at all (§4.9.5). The deadline release no longer drops lossless content: it ends the block's repair phase and hands the content back to ARQ (§4.8.2 step 4). Legacy 0x04 tile parity is off for every session from D3-c on.

**How claims are marked.** **[M]** measured, by WS-C3's browserless runs (`data_c3_rows.txt`, copied from `gf-c3/target/tmp`) or by the scripts in this directory. **[C]** read from code, with file:line. **[L]** from the literature, cited in §8. **[E]** my estimate or inference, not confirmed.

Files in this directory:

| file | what |
|---|---|
| `fec-v2-design.md` | this document |
| `part1_bound.py`, `part1_bound_out.txt` | upper bound on the retransmissions FEC could avoid, from C3's rows |
| `fec_sim.py`, `fec_sim_out.txt` | FEC unit/code/interleave simulation with packet-level Gilbert-Elliott loss |
| `fec_sim_rtt.py`, `fec_sim_rtt_out.txt` | revision 3: reconstruction-first vs timer-driven repair at 20 / 75 ms RTT |
| `wasm-size/` | throwaway crates: wasm size and speed of `reed-solomon-simd`, `reed-solomon-erasure`, `raptorq`, and a 150-line GF(2^8) Cauchy RS prototype (`gf256/`, with an erasure-pattern test) |
| `sota-notes.md` | the literature survey notes, with URLs (§1 revised from Kyber's code) |
| (not committed) `kyber-src/`, `kyber-core/` | read-only shallow clones of Kyber's GitLab projects at the commits listed in §2.1, used only for reading (AGPL; nothing built or run, no code copied) |
| `data_c3_rows.txt` | C3's raw browserless rows (input to `part1_bound.py`) |

---

## 0. Executive summary

**The hypothesis is half right.** It is right about where we need to end up and wrong about why there is so much retransmission today.

- **Wrong as a diagnosis of today's volume.** In C3's lossy browserless scenes, 86–98 % of retransmissions are RTO timer fires and 2–10 % are NACK repairs **[M]**. The timer fires 2.7–26 times per datagram the network actually dropped (26x at 1 % i.i.d. loss) **[M]**. A perfect FEC removes at most one retransmission per lost datagram. So **even a perfect FEC would avoid at most 4–34 % of today's retransmissions** in those scenes, about 13 % at the median; at 1–2 % i.i.d. loss the bound is 4–7 % **[M, bound]**. The rest are timer fires on datagrams that were never lost. They come from a transport cascade, read from code **[C]** and consistent with every measured number but not yet confirmed by an instrument **[E]**:
  1. A lost QUIC packet makes Cubic cut its window (β = 0.7). Application FEC cannot hide that loss from quinn.
  2. Datagrams then wait in quinn's 16 MB datagram queue, which quinn's RTT never sees.
  3. The ledger's loss horizon of 3 x RTT, floored at 60 ms, declares the waiting datagrams lost.
  4. ACKs arriving after that horizon are dropped from the latency tracker, so the RTO deadline cannot learn the delay and stays pinned at its 150 ms floor.
  5. Every datagram still queued past that floor gets an RTO retransmission, and those retransmissions join the same queue.

  On top of that, multi-fragment passes keep only one fragment in the retransmit cache, so a NACK for any other fragment does nothing and the RTO re-sends the wrong one **[C]**. None of this is an FEC problem. It is D1 (BWE/CC coupling, RTO/ledger) and C4 (per-fragment cache).
- **Right as a direction.** Today's FEC really is broken: **[M]**
  - Tile XOR recovers 0–0.8 % of losses, because ~11 tile datagrams share one QUIC packet and a lost packet empties a whole contiguous parity group.
  - H.264 parity is sent and then thrown away by every client.
  - The server has no pre-FEC loss measurement, so nothing could adapt it.

  Once the cascade is fixed, the retransmissions that are left *are* loss repairs. FEC is then what turns a repair that costs one RTT plus detection (≥150 ms today) into one that costs a few packet slots. For H.264, which has no retransmission at all, FEC is the only repair there is.

**What "Kyber" does (revision 2, read from its source; §2.1).** "Kyber" is Jean-Baptiste Kempf's QUIC/WebTransport streaming SDK, not the post-quantum KEM. Its code (`gitlab.com/kyber/core/kymux`, unforked quinn 0.11 with the default Cubic) has **no loss estimator, no adaptive FEC and no adaptive retransmission at the application level**:
- FEC is **static**: RaptorQ with `max(ceil(30 %·symbols), 2)` repair symbols per frame, each symbol one full datagram.
- The only loss metric is a post-FEC count of dropped frames, together with quinn's RTT and lost packets, and all of it is only displayed.
- Bitrate is set by hand.
- The **default mode is a reliable QUIC stream**, so any "learning the loss rate and adapting retransmission" is quinn's generic RFC 9002 loss recovery (my inference **[E]**).

The owner's claim is therefore refuted as a description of Kyber's own logic. What carries over: one symbol per full datagram (corroborates the bundles), and a minimum repair count for small frames. (Revision 2's option of putting refinement traffic on QUIC streams is withdrawn in revision 3, §4.8.1: quinn's stream recovery is sender-side and blind to our FEC.) The adaptive loop to copy is still WebRTC's: FEC sized from a table indexed by loss with a 10 s max filter, NACK-only below 20 ms RTT, parity billed to the BWE, recovered packets reported as not received. FlEC does the same over QUIC with an online Gilbert-Elliott estimate and deadline-timed repair. Microsoft dropped FEC from RDPEUDP2 **[L]**.

**Recommended design ("FEC v2"), in one paragraph.**
- **Bundle tile datagrams.** The server packs tile datagrams into **MTU-sized bundles**, so one QUIC DATAGRAM = one QUIC packet = one FEC symbol = one ACK identity. That fixes the coalescing problem at its root: in simulation, recovery at 1 % loss goes from 0 % to 91 % at the same 10 % overhead. It also cuts datagram and ACK rates about 10x, which the 4K latency target needs anyway.
- **One code for both paths.** Protect bundles, and H.264 frames per frame, with a **systematic GF(2^8) Cauchy Reed-Solomon code whose first repair row is plain XOR**. A single loss recovers at XOR cost. The prototype is ~5 KB gzip of wasm, decodes a k=10, r=2 block in 18 µs in wasm, and passes an erasure-pattern test **[M]**.
- **Adapt (k, r, interleave depth) per traffic class** from **client-reported pre-FEC loss, burst-length histogram and RTT**, using a WebRTC-style table plus an RTT/deadline rule for FEC versus retransmission:
  - refinement passes get no FEC;
  - bursts longer than the deadline allows are left to the repair request.
- **Reconstruction first, retransmission only when reconstruction is impossible** (revision 3, §4.8):
  - the receiver closes each block's recovery window (last symbol + measured allowance);
  - only then, and only if the deadline still allows (`now + RTT + A ≤ deadline`), does it send REPAIR_REQUEST(block, m) for m **new repair symbols**;
  - the sender keeps no timer on FEC-protected data except a last-resort one at window + RTT for lost feedback;
  - at high RTT, redundancy is sized to cover the loss up front.

  Modelled: a FEC-blind sender timer wastes 82–96 % of its retransmissions; reconstruction-first wastes none (`fec_sim_rtt.py`) **[E]**.
- **Bill parity against the bandwidth estimate**, and report **recovered ≠ received** to the estimator. Since revision 4 (§4.9) this needs no estimator code:
  - repair symbols are transmissions, ACKed like sources;
  - a rebuilt source is never ACKed as an arrival, so the RFC 9002 detector declares it lost;
  - a RECOVERED record on a reliable client→server stream (the repair-status stream, revision 5) releases its content. The same stream carries DECLINED, which the client sends when it holds k symbols but does not rebuild, and CONTENT_REJECTED, for a record that fails prevalidation. With all three reliable, the server never waits on a guess: `T_def` is a 60 s alarm for the tile classes, not a latency mechanism, and H.264 has none.
- **Caps-gated:** HELLO bit 2.
- **Backward compatible:** two new server→client discriminators (BUNDLE 0x09, REPAIR 0x0A), sent only to clients that set the bit, and three new client→server datagram types (LOSS_REPORT 0x07, ACK v4 0x08, REPAIR_REQUEST 0x0B) that old servers already drop harmlessly **[C]**. The client sends the three, and opens the repair-status stream, only after it has received its first FEC-v2 bundle.

**Where it fits** (details in §6):
- **Wave C, now.** C3's three no-wire PRs (decoder duplicate fix, client consumes the existing H.264 XOR parity, tile FEC off by default). Plus a measurement-only PR: client-side pre-FEC loss and burst counters, populate `datagrams_recovered_fec`, server logs quinn path loss and RTO-cause counters. The PR changes no behaviour and no wire format. It is what lets D1 confirm or refute the cascade above. C4 fixes the per-fragment cache.
- **Wave D1.** The retransmission cascade, plus the sender-timer gating hook for protected classes (an interface; no behaviour change until D3). It gates any tile FEC work.
- **After D1 (a new "D3 fec-v2" stream, 6 PRs).** Bundling, then the codec, then FEC emission, then adaptation.
- **H.264 per-frame RS** depends on nothing in D1 and can land at the end of Wave C or early in D, coordinated with C5 (GOP / reference invalidation).

**Owner decisions** (§6.4; revision 3 adds the reconstruction-first policy, its latency cost under bursts, and repair-symbol requests; the QUIC-stream option is withdrawn). Accept that the storm is a transport defect and gate tile FEC behind D1. Approve bundling as a wire change. Choose the codec (recommended: in-house GF(2^8) Cauchy RS; alternative: `reed-solomon-simd`). Spend HELLO bit 2. Set the recovered-versus-received semantics for the estimator. Set an overhead cap. Set the native-client version-skew policy. *Revision 5:* items 14, 15 and 16 (the ACK v4 details, the meaning of an arrival, and a second stream for the repair-status records) were decided on 2026-10-26.

---

## 1. Part 1: does broken FEC cause the retransmission?

### 1.1 What the code does with a datagram, end to end

**Server send path**
- `IoBridge::emit_via_scheduler` drains the scheduler within a per-tick budget of `min(0.9 × cwnd/rtt × 33 ms × AIMD, goog_cc pacer)` (`io_bridge/pacing.rs:503-516, 541-551, 264-276`) **[C]**.
- Every fragment of a tile-pass goes to `ReliableTileEmitter::submit_batch` under **one** `EmitKey` (`io_bridge/emit.rs:619-659`) **[C]**.
- `submit_one` allocates a `wire_seq` and caches the bytes. It schedules the RTO **at submit time** (`reliable_emitter/emitter.rs:207-218`) and feeds the XOR group builder (`:220`, `:241-257`) **[C]**.
- The cache entry holds **one** fragment (`fragments: smallvec![bytes]`, `emitter.rs:207`). A second fragment of the same pass **replaces** it (`reliable_emitter/cache.rs:171-184`). Each fragment still pushes its own RTO heap entry **[C]**.
- Transmissions enter the ledger when the turn drains them from the emitter, stamped with the drain time, not the wire departure (`io_bridge.rs:1701-1712`) **[C]**.
- Parity has no `wire_seq`, no ledger record, no ACK and no retransmit. It is not counted in `tile_wire_bytes`, so the pacer does not bill it (`io_bridge/emit.rs:564-568`; C3 A1) **[C]**.
- quinn queues every DATAGRAM in a FIFO of up to 16 MB (`quinn.rs:111-180, 462-464`; quinn-proto `connection/datagrams.rs:28-58`). It packs as many as fit into each packet (`datagrams.rs:166-184`), which is the coalescing. Every DATAGRAM-bearing packet is ack-eliciting and congestion-controlled, and a lost one makes Cubic multiply its window by β = 0.7 (quinn-proto `congestion/cubic.rs:12, 165-196`; RFC 9221 §5) **[C][L]**.

**Client receive path**
- A tile datagram feeds the 40-entry parity window, then `handle_source_tile` (`ghostframe-client-core/src/reassembly.rs:160-172`) **[C]**.
- Non-CDF53 tiles are ACKed on receipt by `wire_seq` (`reassembly.rs:249-270`). CDF53 fragments are ACKed only when the whole pass has assembled and pre-validated (`reassembly.rs:686-711`) **[C]**.
- **A FEC-rebuilt datagram is ACKed under its original `wire_seq`**, with the arrival time of the parity (`reassembly.rs:137-145, 251-263`), for CDF53 too (`reassembly.rs:373-374`). The comment at `reassembly.rs:698-700` describes the dead fragment-parity path, not this one **[C]**.
- H.264 parity is dropped on arrival (`frame_assembly.rs:62-66`) **[C]**.
- `ReceiverFeedback.datagrams_recovered_fec` exists on the wire (`ghostframe-protocol/src/feedback.rs:11, 18`) and is never incremented by the live path. The server reads only the suspension flag from that message (`io_bridge/bwe_glue.rs:408-420`) **[C]**.

**Server ACK path**
- Each ACK entry resolves through the ledger: `Live` counts as delivered in `loss_window`, `Late` retracts a loss (`io_bridge/ack_dispatch.rs:147-190`) **[C]**.
- Only `Live` ACKs become BWE samples (`ack_dispatch.rs:243-253`) **and the RTO deadline tracker's samples** (`io_bridge.rs:1741-1770`, then `set_ack_deadline` at `:1799`) **[C]**.
- The ledger declares lost anything outstanding past `clamp(3 × quinn srtt, 60 ms, 600 ms)` (`io_bridge/bwe_glue.rs:101-105, 533-536, 452-470`). Those losses feed goog_cc and the classifier's loss rate **[C]**.

**So what does a FEC recovery do to the sender today?** It **does** suppress the retransmission: the rebuilt datagram is ACKed, the ledger resolves it as `Live`, and `on_ack` drops the cache entry. It is also counted as *delivered* to `loss_window` and to goog_cc, carrying the parity's arrival time **[C]**. Two consequences:
1. The server's only loss rate is post-FEC, so an adaptive FEC fed from it would read its own success as "no loss" and oscillate.
2. A recovered datagram puts a late arrival time into the delay-gradient input.

Both need fixing, but neither is why there are so many retransmissions.

### 1.2 How many retransmissions FEC could possibly avoid

`part1_bound.py` takes C3's medians: browserless `burst_loss` scene, 16x16 grid, 10 ms one-way delay, s2c loss only, 12 s virtual time, 3 repetitions. Network-lost datagrams = netsim-dropped QUIC packets x datagrams per packet. Retransmissions are split by mechanism (`part1_bound_out.txt`) **[M]**:

| FEC | model | packets dropped | datagrams the network lost | retransmissions | RTO share | NACK-hit share | RTO fires per lost datagram | **max share FEC could avoid** |
|---|---|---|---|---|---|---|---|---|
| off | iid 1 % | 48 | 746 | 20 086 | 96.9 % | 3.2 % | 26.1 | **3.7 %** |
| off | iid 2 % | 70 | 1 216 | 18 502 | 96.2 % | 3.7 % | 14.6 | **6.6 %** |
| off | iid 5 % | 103 | 1 791 | 13 276 | 98.0 % | 2.0 % | 7.3 | **13.5 %** |
| off | iid 10 % | 138 | 1 829 | 7 075 | 96.2 % | 3.8 % | 3.7 | **25.9 %** |
| off | GE 2 % burst 3 | 117 | 1 774 | 21 178 | 96.2 % | 4.3 % | 11.5 | **8.4 %** |
| off | GE 2 % burst 8 | 185 | 2 358 | 14 774 | 90.3 % | 9.8 % | 5.7 | **16.0 %** |
| off | GE 10 % burst 8 | 125 | 5 879 | 17 421 | 91.1 % | 8.6 % | 2.7 | **33.7 %** |
| prod K=10 | iid 1 % | 48 | 737 | 19 866 | 96.0 % | 4.0 % | 25.9 | 3.7 % |
| prod K=10 | iid 2 % | 71 | 1 171 | 17 961 | 97.6 % | 2.4 % | 15.0 | 6.5 % |

(C3 itself flags the GE-b8 rows' datagrams-per-packet as unreliable; the burst-8 bounds are soft.) The clean control has **zero** retransmissions with the RTO enabled, so the cascade is *triggered* by loss, not by the timer alone **[M]**.

Two more measured facts point to delay, not loss. These are inferences from measured numbers **[E]**:
- **Datagrams emitted but never received at scene end are 4–31x the network losses** (`unrcv/net_lost` column). Something between the emitter and the client application holds or drops most of them. The two candidates are the outgoing quinn queue, still backlogged at scene end, and the client's 64 KiB datagram receive buffer, which drops the oldest datagram silently (`ghostframe-client-net/src/lib.rs:209`; quinn-proto `datagrams.rs:132-136`). The second is also a real loss source in production, invisible to every counter we have.
- **Critical-tier completions collapse from 11 264 to ~440 at 1 % loss, while the survivors' latency stays ≤ 84 ms.** This is what censoring looks like. A completion is counted only from a `Live` ACK, so every pass ACKed after the ~60 ms horizon disappears from the statistics instead of appearing as slow (`ack_dispatch.rs:243-253`).

### 1.3 The causal chain

Steps 1–5 are code-backed. That they *dominate* is the leading inference **[E]**: it fits every number in §1.2, but no instrument has confirmed it yet.

```
QUIC packet lost (1 per ~100 at 1 %)
 └─(a) quinn Cubic: cwnd x 0.7 (cubic.rs:184-196), recovery over ~seconds
      (app FEC above QUIC cannot prevent this: the packet IS lost)
 └─(b) offered rate > new cwnd/RTT: the budget is computed per tick, retransmit share up to 50 %
      (pacing.rs:172, 378-437), the AIMD and pacer terms lag
      → datagrams wait in quinn's FIFO (16 MB, quic.rs:111-180); quinn's srtt does not include it
 └─(c) ledger horizon = 3 x srtt, floor 60 ms (bwe_glue.rs:101-105, 533-536)
      → waiting datagrams declared LOST to goog_cc + loss_window (false losses)
 └─(d) Late ACKs never reach the deadline tracker (ack_dispatch.rs:243-253; io_bridge.rs:1741-1770)
      → p95 censored ≤ horizon → deadline pinned at ACK_DEADLINE_FLOOR = 150 ms (rto.rs:105, 181-186)
 └─(e) queueing delay > 150 ms → RTO fires for every in-flight pass (emitter.rs:421-516),
      each with a new wire_seq + parity, into the same queue → (b) again
 └─(f) multi-fragment passes: cache holds only the last fragment (emitter.rs:207, cache.rs:175-177);
      NACK for frag ≥1 dropped (emitter.rs:611-613), NACK for frag 0 re-sends the LAST fragment;
      N heap entries per key → N fires per round (emitter.rs:218, rto.rs:45-66)
```

- **What FEC can fix:** at most the left-hand trigger. It can repair the datagrams in the lost packet, which is the 4–34 % in §1.2. It cannot stop (a) when FEC lives above QUIC, and it does nothing for (b)–(f).
- **What needs transport work:**
  - (a)–(b): D1, the CC coupling (custom controller or cwnd/queue awareness);
  - (c)–(d): D1, the RTO and ledger clocks and censoring;
  - (f): C4.
- **How to confirm** (the Wave C measurement PR, §6.1): in the iid-1 % scene, log over time
  - `quinn_send_buffer_space` (already in a log line),
  - quinn `PathStats.{cwnd, lost_packets, congestion_events}`,
  - client `datagram_receive` drops (quinn: `frame_rx.datagram` minus application reads),
  - an RTOPROBE histogram of `age_since_first_us` for fires whose original transmission is later ACKed (`ledger.acked_after_declared_lost`, `transmission_ledger.rs:62-66`).

  If the chain holds, most RTO fires are for transmissions that were ACKed later, and they cluster after `congestion_events` increments. A run with `GHOSTFRAME_NO_RTO=1` then shows how much NACK alone repairs.

### 1.4 Verdict

Fixing FEC alone would leave ≥ 66 % of the retransmissions (≥ 93 % at 1–2 % i.i.d. loss) and the latency collapse in place. The owner's goal (low latency, little retransmission) needs the D1/C4 transport fixes **and** a working, adaptive FEC. The second is worthless for tiles until the first lands, because the scene cannot show a win: C3's FEC on/off and interleave variants changed neither latency nor retransmission count **[M]**. H.264 is the exception: no retransmission exists there, so per-frame FEC pays off at once.

---

## 2. State of the art

(Condensed. Full notes and every URL are in `sota-notes.md` and §8.)

### 2.1 "Kyber", read from its source (added 2026-10-15, revision 2)

**What was read.** `https://gitlab.com/kyber/kyber.git` (commit `dcba6a6e602a92140cba01a698b2160477335fca`, 2026-10-09) turned out to be **only the website and docs repository**, with no transport code; it is cloned at `kyber-src/`. The code lives in sibling projects of the same GitLab group, found through the public group API. They are shallow-cloned read-only under `kyber-core/`:

| project | commit |
|---|---|
| `kymux` (transport: kynet, kyproto) | `af10dbb3ea016135c0ec8e01143179ad4bda005b` |
| `kyctl` (controller, client) | `92c632f9e889917a34aaa7697fb88b4991054b1c` |
| `kymedia` (encoder service) | `b0efdfa251a19274ff1d9bf6324deb6bee10c026` |
| `kyber-web` (web client) | `02b32fa0c748b6388bdb2e2704603c39f91a1283` |
| `kysdk` | `ebca32a08f6488ea23750943ded52119efd584d6` |
| `kyutil` | `1ae6ee42ac1be9a91f0d25089da37e7f1ac3e42a` |

**Licence.** AGPLv3 or commercial (`kyber-src/COPYING.md`). Nothing was built or executed. **No Kyber code was transcribed into this document or into ghostframe.** Only behaviours and parameters are described, with citations. Paths below are relative to `kyber-core/`. Every statement is **[C-kyber]**, read from code, unless marked otherwise.

**Transport stack.**
- **Unforked** `quinn 0.11.9` / `quinn-proto 0.11.14`, the same quinn-proto version as ghostframe, plus `wtransport 0.7` (`kymux/kynet/Cargo.toml:23-30`).
- **`TransportConfig::default()`**, with only the idle timeout and keep-alive set (`kymux/kynet/src/driver/quinn.rs:96-105`, `driver/common.rs:73-82`). So quinn's default Cubic is in use, with default datagram buffers and no custom congestion controller.
- The web client asks the browser for `congestionControl: LowLatency` (`kyctl/kyclient/src/kymux_backend/net_kyproto.rs:200`; `kymux/kynet/src/driver/webtransport_js.rs:47-84`).

**Delivery modes.** The video protocol is chosen by configuration; nothing adapts it at runtime (`kyctl/kyctl-common/src/kymux.rs:26-33`):

| mode | how it works |
|---|---|
| **`Reliable`** (the default) | one QUIC stream (`kymux/kyproto/src/protocol/driver/av/reliable.rs`) |
| `GopStream` | one QUIC stream per GOP |
| `Unreliable` | datagrams with a receive buffer, no repair |
| `UnreliableFec` | datagrams protected by RaptorQ |

**1. Loss estimation.**
- The only loss counter Kyber computes is `ProtocolStats.dropped_packets` (`kymux/kyproto/src/lib.rs:272-275`). The receiver increments it by the number of frame packets it skipped after its re-order and loss buffer ran out (`video_unreliable_fec.rs:635-643`). That is a *post-FEC*, per-frame count, cumulative and unsmoothed.
- RTT and `packets_lost` come straight from quinn's `ConnectionStats.path` (`kymux/kynet/src/driver/quinn.rs:182-185`; `wtransport.rs:152-155`). The browser driver returns `None` for both (`webtransport_js.rs:314-317`).
- `run_stats_poller` sends these three numbers, MessagePack-encoded, at a fixed interval over a metrics channel (`kyctl/kycontroller/src/kymux_controller/mod.rs:849-871`; struct at `kymux_controller/metrics.rs:23-28`).
- Their consumers are the client's metrics API, the C API and the JSON export (`kyctl/kyclient/src/capi.rs:1623-1640, 1777-1797`; `kyclient/src/metrics.rs:157,178`). **No decision reads them.**
- There is **no burst-length, reorder or pre-FEC measurement** anywhere in kymux.

**2. Retransmission adaptation.**
- **No application-level adaptive decision exists** between retransmitting, adding FEC and dropping.
- In `Reliable` and `GopStream`, retransmission is **QUIC's own stream loss recovery**: quinn's RFC 9002 packet- and time-threshold loss detection plus PTO, which adapts to RTT. My inference **[E]** is that this is what "learns the loss rate and adapts its retransmission" refers to. It is quinn's generic machinery, available to any user of QUIC streams.
- `GopStream` opens a new stream at every keyframe, so that "no more retransmissions will occur for the old GOPs" (`video_gopstream.rs:20-27, 81-82`). In the code, though, the old stream is closed with **`finish()`, not `reset()`** (`:178-181`). `finish()` lets quinn keep retransmitting the old GOP. Only the receiver stops reading it (`:345-358`). The documented supersession therefore does not happen on the sender side.
- In the datagram modes, the receiver waits up to `MAX_BUFFERING = 50 ms` from the earliest completed packet still held back, then skips missing packets (`video_unreliable_fec.rs:457, 836-844`; the module doc says 20 ms at `:255`, the constant is 50 ms). This is a fixed receive deadline, not an adaptive one.
- The **owner's claim is refuted at the application level and plausibly true only through QUIC's stream recovery.**

**3. FEC.**
- **Crate and placement:** `raptorq 1.7` (`kymux/kyproto/Cargo.toml:23`). Encoding runs inline in the send driver (`video_unreliable_fec.rs:337`); decoding runs in the receiver's process task (`:568-659`), in wasm on the web client.
- **Block:** **one source block per media packet, i.e. per encoded frame** (`raptorq::Encoder::with_defaults`, `:337`).
- **Symbol size:** **`max_datagram_size − 26`**, so each symbol is one full datagram (`:296, 320-327`). This sidesteps QUIC coalescing: one symbol per packet. The slides' "hopefully … on their own UDP datagrams" is true by construction.
- **Repair count: static,** `max(ceil(0.3 × source_symbols), 2)` (`:343-344`; table at `:92-104`). **No signal adapts it.**
- **Send order:** all source and repair datagrams go out back to back as `get_encoded_packets` returns them (`:348-368`). There is no interleaving across frames and no pacing. A burst longer than the frame's repair count loses the frame.
- **Audio:** the same scheme (`audio_unreliable_fec.rs:88`).
- **Configuration packets** (SPS/PPS) go on a reliable stream, and media are grouped by config epoch (`:21-23, 124-129, 397-414`).

**4. Congestion control and recovery.**
- **Congestion control:** quinn's default only. There is no application BWE and no automatic encoder rate.
- **Bitrate:** set **manually**, by an HTTP endpoint (`kyctl/kycontroller/src/main.rs:504-575`) or the web API `set_bitrate` (`kyber-web/src/lib.rs:824-826`). The default is 20 Mbit/s in the web client (`kyber-web/src/lib.rs:184`) and 8 Mbit/s in the controller (`avservices_manager.rs:1003`).
- **Keyframes:** the encoder uses **intra-refresh by default** (`kymedia/kyavservice/src/config.rs:264-265`). An IDR is forced only on a new subscription and when a fan-out output **lags** (`kyctl/kycontroller/src/avservices_manager.rs:638-655, 900-913`).
- **No loss-triggered keyframe or reference-invalidation request** was found.

**5. What ghostframe should adopt, and what it should reject.**

Adopt:
- **(a)** **One FEC symbol per full-size datagram.** This confirms §4.1's bundles.
- **(b)** **A minimum repair count per frame.** Kyber uses at least 2, Moonlight `minRequiredFecPackets`. Small frames suffer most from losing their only fragment. Today ghostframe gives a one-fragment H.264 frame *no* parity at all (`ghostframe-protocol/src/fec.rs:55-59`).
- **(c)** **Send reference data reliably, separately from the lossy media.** Kyber sends SPS/PPS on a stream. ghostframe's analogue is the frame-dimension and palette state.
- **(d)** **A bounded receive deadline with skip-ahead and a drop counter.** That counter is `h264_frames_lost` in LOSS_REPORT.
- **(e)** **`congestionControl: "low-latency"`** on the browser's WebTransport constructor. It affects only the browser's sending direction (ACKs, NACKs, input). It is cheap, and support should be checked.
- **(f)** *(Revision 2 proposed carrying refinement passes on QUIC streams with `RESET_STREAM`. **Withdrawn in revision 3** and moved to the rejected list below; see §4.8.1.)*

Reject:
- **(g)** Static 30 % parity on every frame, which costs 30 % on a clean link.
- **(h)** RaptorQ for our block sizes (+114 KB of wasm, measured in §4.2).
- **(i)** Sending a frame's repair symbols straight after its sources with no interleaving.
- **(j)** Metrics that are displayed but never fed back.
- **(k)** Bitrate set by hand.
- **(l)** (revision 3) Using QUIC stream reliability, or any sender-side timer, for data that is FEC-protected or could be. quinn's recovery cannot see our reconstruction. On a 75 ms path it retransmits data RS has already rebuilt: 82–96 % wasted in the model, §4.8.1.

### 2.2 Game streaming and remote desktops
- **Moonlight / Sunshine.** [R9–R13] **[L]**
  - The code is RS over GF(2^8) (nanors), so a block holds at most 255 shards.
  - `fec_percentage` defaults to 20 and gives `ceil(data·F/100)` parity shards per frame block. The client can demand a minimum parity count, which raises protection for small frames. A frame has at most 4 blocks; beyond that FEC is off for that frame. The encoder bitrate is scaled by (100−F)/100, so parity is billed.
  - **Not adaptive:** the server only logs the client's loss statistics.
  - No video retransmission. Recovery is by reference-frame invalidation or an IDR request; "speculative RFI" fires as soon as the losses exceed the parity.
  - This is the model for our H.264 path.
- **Microsoft RDP.** [R14, R15] **[L]**
  - The older UDP transport (MS-RDPEUDP) has one GF(2^8) FEC packet per block of up to 255 packets, recovering one loss each, and declares loss after 3 later packets.
  - The newer one (**MS-RDPEUDP2**) "does not include an FEC layer". Microsoft moved to ARQ for desktop traffic.
- **PCoIP / HP Anyware** retransmits image data only if it is still current [R16]. That is deadline- and supersession-aware ARQ, which ghostframe partly has already.
- **Teams** uses per-frame RS, switched on only after loss is seen (Tambur paper, [R17]). Zoom is known only from a patent [R18]. Parsec publishes no FEC [R19]. Chrome Remote Desktop uses WebRTC [R20].

### 2.3 WebRTC (upstream code) [R21–R25] **[L]**
- **FEC rate.** Taken from a table indexed by bits per frame and loss (Q8). The loss input passes through a **max filter over 10 × 1 s bins**, so FEC is sized for the worst recent second.
- **NACK/FEC hybrid.** Below `kLowRttNackMs = 20` ms of RTT it is NACK only, with delta FEC at 0. The high-RTT cutoff is disabled by default, so NACK stays on at any RTT. FEC is also off for small frames when RTT is under 200 ms.
- **Billing.** FEC and NACK overhead come out of the BWE estimate (capped at 50 %), and the encoder gets the rest.
- **Retransmission timing.** The receiver re-NACKs every RTT; the sender refuses a resend within one RTT of the previous one.
- **Feedback.** **FEC-recovered packets are not reported as received** in transport-wide feedback or to the BWE, and are left out of loss statistics, so reported loss is *pre-repair*. Transport-wide CC feedback: [R26], RFC 8888 [R27].
- **Loss-based BWE v2** separates "inherent" (random) loss from loss caused by sending too fast, fitted by maximum likelihood. That is exactly the congestion-versus-random split §4.3 needs.
- WebRTC's burst-optimised FEC masks exist but go unused, because there is no burst feedback. ghostframe's LOSS_REPORT histogram supplies that feedback.

### 2.4 QUIC and FEC [R28–R36] **[L]**
- **RFC 9265** (coding for QUIC): "a recovered packet must be considered as a lost packet" for congestion control. FEC v2 follows this by never acknowledging a rebuilt source as an arrival, and by releasing its content through a separate RECOVERED record (§4.9, revision 4; on the reliable repair-status stream since revision 5; revision 1 used a `recovered` flag).
- **draft-michel-quic-fec** adds SOURCE_SYMBOL / REPAIR / SYMBOL_ACK frames inside QUIC, and forbids a source symbol spanning packets.
- **QUIC-FEC** (2019): FEC slows transfers at low loss and low delay, so it must be adaptive.
- **FlEC** (IEEE/ACM ToN 2023):
  - estimates uniform loss *and* Gilbert burst parameters online;
  - sends repair at the last moment that still meets the deadline;
  - keeps repair inside the congestion window;
  - sent **no repair at 5–70 ms one-way delay**, because ACKs arrived first;
  - showed that blind protection can double the bytes sent.

  This is the closest published analogue of §4.3.
- **Codes and frameworks:** Tetrys (RFC 9407), sliding-window RLC (RFC 8681, framework RFC 8680), FECFRAME (RFC 6363), RaptorQ (RFC 6330), and draft-swett-nwcrg-coding-for-quic (not fetched; no claims made).
- **None of these designs addresses many small application datagrams coalesced into one QUIC packet.** All of them protect packet- or frame-sized units. That is the gap §3 and §4.1 close.

### 2.5 Codes and libraries [R37–R44] **[L]** (wasm numbers in §4.2 are our own **[M]**)
- `reed-solomon-simd` (Leopard-style, GF(2^16)): even shard sizes; up to 32768 + 32768 shards; SIMD backends for AVX2, SSSE3 and Neon only, so wasm runs scalar.
- `reed-solomon-erasure`: its SIMD path is C, unusable in wasm.
- `raptorq`: rateless; decode failure probability ≈ 1/256^(h+1) with h extra symbols.
- cm256 and longhair: GF(256) Cauchy, k < 256. wirehair: O(N) fountain code needing about N + 0.02 symbols.
- **Streaming codes** (Badr/Khisti) and **Tambur** (NSDI'23): a streaming code with a 3-frame deadline cut undecodable frames by 26 % with 35 % less redundancy than Teams' per-frame RS. In the Teams traces, **38 % of loss events were bursts of ≥ 2 packets and 38 % spanned more than one frame**. A later critique says Tambur over-provisions [R45].

### 2.6 Loss and burst estimation [R46–R53] **[L]**
- **Burst metrics:** Gilbert-Elliott parameters can be fitted from run lengths. RFC 3611 defines burst/gap metrics (Gmin = 16).
- **Pre- versus post-repair reporting:** RFC 5725 and RFC 7509 define post-repair loss reporting. Pre-repair loss is the input FEC sizing needs.
- **Loss detection:** RFC 9002 declares loss at 3 packets or 9/8 RTT.
- **Deadline-aware reliability:** PR-SCTP (RFC 3758) and DTP (draft-shi-quic-dtp).
- **BBR** treats more than 2 % loss in a round as congestion.
- **Two-state Gilbert-Elliott underfits Wi-Fi.** No authoritative mean Wi-Fi burst length was found; "1–3 packets" is folklore. **We therefore measure it** (LOSS_REPORT histogram) instead of assuming it.

---

## 3. What fixes the coalescing problem: simulation

`fec_sim.py` (`fec_sim_out.txt`). Loss is per QUIC packet, Gilbert-Elliott with loss_bad = 1, over 60 000 packets. Unbundled, 11 tile datagrams per packet, as measured. Ideal decoders. "Delay" is measured in packet slots from a lost symbol's own send slot to the slot that made it decodable. At 1 000 packets/s (~9 Mbit) one slot is 1 ms; at 10 000 packets/s, 0.1 ms. These are estimates **[E]**, not measurements:

| scheme (overhead) | iid 1 % resid / recovered | iid 2 % | iid 5 % | GE 2 % b3 | GE 2 % b8 | delay p50/p95 (iid 2 %) |
|---|---|---|---|---|---|---|
| no FEC | 0.93 % / – | 1.95 % | 4.99 % | 2.00 % | 1.98 % | – |
| **today**: XOR K=10 over contiguous datagrams (10 %) | 0.93 % / **0 %** | 0 % | 0 % | 0 % | 0 % | – |
| C3 interleave, stride 12 (10 %) | 0.08 % / 92 % | 80 % | 60 % | 14 % | 4 % | 7 / 12 |
| **bundles** + XOR k=10 (10 %) | 0.08 % / 91 % | 83 % | 60 % | 13 % | 2 % | 5 / 10 |
| bundles + XOR k=5 (20 %) | 0.05 % / 95 % | 90 % | 77 % | 14 % | 5 % | 3 / 5 |
| bundles + RS(20,2) (10 %) | 0.01 % / 99 % | 94 % | 72 % | 24 % | 5 % | 11 / 20 |
| bundles + RS(10,2) (20 %) | 0.00 % / 99.8 % | 98 % | 88 % | 27 % | 8 % | 6 / 10 |
| bundles + RS(20,4) (20 %) | 0.00 % / 100 % | 99.6 % | 97 % | 52 % | 17 % | 11 / 20 |
| bundles + RS(10,2), depth-4 interleave (20 %) | 100 % | 98 % | 90 % | **86 %** | 41 % | 24 / 40 |
| bundles + RS(10,2), depth 8 (20 %) | 100 % | 99 % | 90 % | **96 %** | **71 %** | 48 / 80 |
| bundles + sliding-window RLC w=20, 1 repair per 5 (20 %) | 100 % | 100 % | 99.6 % | 57 % | 17 % | 4 / 11 |
| bundles + RLC w=30, 1 repair per 10 (10 %) | 100 % | 99 % | 86 % | 39 % | 8 % | 8 / 22 |

What the table says:
1. **Coalescing, not the XOR code, is what makes today's tile FEC recover nothing** (0 % in every model, matching C3's 0–0.8 % measured). Any layout that puts one symbol per packet works: bundles, or interleaving by the packing factor. For i.i.d. loss that alone takes recovery from 0 % to 80–92 % at the same 10 % overhead. Bundling is the better of the two: it does not depend on how quinn happens to pack, and it brings the other gains described in §4.1. (The simulation packs perfectly regularly. C3 measured stride-12 interleave at only 46–60 % in the real scene, because real packing varies. That is a further argument for bundles, which make the packing deterministic.)
2. **RS beats XOR at equal overhead** once loss exceeds ~2 %. RS(20,4) at 20 % recovers 97 % at 5 % i.i.d. loss, against 77 % for XOR k=5.
3. **Bursts need interleaving depth roughly equal to the burst length, and depth costs delay** (k+r)·D slots. At 1 ms per slot, depth 8 means 80 ms at p95, more than a tailnet RTT (5–40 ms). Bursts longer than the deadline allows are therefore better left to a fast NACK than given more FEC. This is the WebRTC FEC/NACK-by-RTT rule again, derived for our case.
4. **Sliding-window RLC has the lowest delay** for i.i.d. loss (p95 8–11 slots against 10–20 for an RS block of the same rate). It is no better for bursts, and its decoder (Gaussian elimination over a moving window) is the most complex. Not recommended for the first version; revisit only if the block span is shown to cost latency.
5. **H.264** (C3's `h264_mc2.py`, not re-run). A 40-fragment frame at 2 % i.i.d. loss is lost 55 % of the time today, 4.3 % with the existing K=5 XOR consumed, 0.2 % with per-frame RS at 10 % and ~0 % at 20 %. Under bursts (mean 4) RS at 20 % gets 2.6 %. **[M, offline model]**

---

## 4. Design for ghostframe

### 4.1 FEC unit and placement

**Inside QUIC (a quinn-proto fork, FlEC / draft-michel-quic-fec style): rejected.** The browser end of the connection is Chrome's or Firefox's own QUIC stack behind WebTransport. Neither implements any QUIC FEC extension, and we cannot change it. QUIC-level FEC would therefore work only for the native client, and would mean forking quinn-proto. The decisive constraint: **FEC has to be application-level, carried in DATAGRAM frames.**

**Unit: an MTU-sized bundle of tile datagrams.** The emitter's drain packs whatever tile datagrams are queued into bundles of up to `max_datagram_size`, about 1 150–1 200 B. A bundle is flushed when it is full or at the end of a drain, so bundling adds no latency. Each bundle:
- is **one QUIC DATAGRAM frame, which in practice fills one QUIC packet**. A packet loss is then a symbol loss and the FEC arithmetic of §3 holds.
- carries a **`bundle_seq`**, the transmission identity, which replaces the per-tile `wire_seq` for ACK and ledger. ACK volume drops ~10x (one entry per bundle). Retransmission stays content-based, as today: the cache keeps per-pass bytes, and a retransmitted pass is re-bundled under a new `bundle_seq`.
- puts each inner record behind its existing `DatagramHeader` and `TileHeader`, prefixed by a `u16` length. The client unbundles and feeds each record to today's `handle_source_tile`, so reassembly, the CDF53 deferred ACK and decoding are untouched.

Side benefits (the first is measured, the rest estimated **[E]**):
- **[M]** The datagram rate drops about 10x. That is 23k → ~2k datagrams/s at 1024x768 and 61k → ~6k/s at 1080p, memory item (c) "needs batching for 4K".
- Less per-datagram work on both sides: quinn frame headers, the wasm/JS boundary crossing per datagram, the ack batcher.
- No new loss granularity: a lost packet already takes ~11 datagrams with it today.

**Placement in time: encode at emission, not at dispatch.** Parity for a block is built as bundles leave the emission queue. Encoding cost is then bounded by the link rate, not by the size of the dirty frame. At 100 Mbit/s with r/k = 0.2, a scalar GF(2^8) kernel does 12.5 MB/s x 0.2 x ~1 ns/byte ≈ 2.5 ms of CPU per second. This dissolves C3's 4K concern (1–3.5 ms of dispatch-thread XOR per frame) **[E from M]**.

**Per-class blocks.** H.264 keeps per-frame blocks: a frame is generated atomically, so its fragments are a natural block, and the deadline is the frame. Tile bundles use small running blocks of k ≤ 20, one stream per class (critical tiles, H.264). **Refinement passes get no FEC.** They have no deadline and are superseded often; NACK is cheaper.

### 4.2 Code choice

Measured in this directory (`wasm-size/`). Wasm is `wasm32-unknown-unknown`, `opt-level=s`, LTO, stripped, run in Node 26 (V8). Native is release on this host. The cost of a trivial probe crate is subtracted. For scale, today's client wasm is 161.6 KB raw / 65.7 KB gzip / 51.7 KB brotli.

| code | wasm Δ (gzip) | wasm decode k=10 r=2 1180 B | wasm decode k=40 r=8 | native encode k=10 r=2 | native encode k=200 r=40 | notes |
|---|---|---|---|---|---|---|
| XOR (today) | 0 | – (1 loss only) | – | ~1 µs | – | burst-blind, 1 loss per group |
| **GF(2^8) Cauchy RS, row 0 = XOR (in-house prototype)** | **~+5 KB** | **18 µs** (1 loss in k=10: 3 µs) | 281 µs | 16 µs (scalar) | 10 ms scalar (a SIMD nibble-table kernel should be ~10x faster [E]) | k+r ≤ 256; 512-byte tables; a single loss decodes as XOR |
| `reed-solomon-simd` 3.1 (Leopard, GF(2^16)) | +10 KB | **~890 µs** (fixed FWHT cost; native ~205 µs) | 1.5 ms | ~1 µs | 62 µs | fastest encode; no wasm SIMD path; no XOR fast path |
| `reed-solomon-erasure` 6 (GF(2^8)) | **+76 KB** | 24 µs | 601 µs | – | (wasm 45 ms) | 64 KB multiplication table embedded |
| `raptorq` 2.0 | **+114 KB** | 258 µs | 646 µs | – | (wasm 3.1 ms) | rateless; ~2-symbol overhead per block hurts at small k |

**Recommendation:** an in-house systematic GF(2^8) Cauchy RS in `ghostframe-protocol`, with no new dependency.
- Rows and columns are normalised so repair 0 is plain XOR. A block with one loss, the common case, then costs today's XOR on both sides, and r = 1 *is* XOR.
- `wasm-size/gf256/` has the ~150-line prototype and a test that recovers every tested pattern of up to r erasures for (5,1), (10,2), (12,3), (20,4) and (40,8). It is MDS by construction: every square submatrix of a Cauchy matrix is nonsingular, and row or column scaling preserves that.
- Before shipping, add a SIMD multiply-accumulate kernel (`pshufb` / `vqtbl1q` / wasm `i8x16.swizzle` nibble tables). Wasm SIMD needs `+simd128` for the whole bundle, which is an owner decision. Until then, encoding at emission time keeps the scalar kernel within budget.
- H.264 frames above 255 fragments split into blocks of ≤ 64 fragments.

**Alternative:** `reed-solomon-simd`, for the fastest server encode and no kernel to maintain. It costs ~0.9 ms of wasm main thread per lossy block. That is ~8 % of a core at 4K and 2 % loss **[E]**, too much for small tile blocks.

**RaptorQ** (Kyber's choice) fits frames of hundreds of symbols. It costs 114 KB of wasm and its reception overhead punishes k ≈ 10. Not for us (agreeing with the protocol review's Kyber addendum).

### 4.3 Adaptation: the estimation loop

**Inputs.** Each is a new metric, specified in §4.4.

| input | source | why |
|---|---|---|
| pre-FEC bundle loss rate *p* | client, from `bundle_seq` gaps (with a reorder window); cross-checked by server quinn `PathStats.lost_packets / sent_packets` | the channel, not our own success; post-FEC loss is what made C3's (a) unusable |
| burst structure: mean loss run *b*, run-length histogram, Gilbert-Elliott (p_GB, p_BG) | client run lengths; GE fitted by run-length means (RFC 3611 burst/gap style) | chooses r vs interleave depth |
| residual loss, recovered count, parity useful/wasted | client decoder | closes the loop: did the chosen (k, r, D) meet its target? |
| RTT, rttvar, min RTT | server quinn path stats | the cost of ARQ |
| deadline per class | server: H.264 = 1–2 frame intervals; critical tile = configured latency target (e.g. 100 ms); refinement = none | chooses FEC vs ARQ |
| congestion signal | goog_cc overuse (delay gradient), quinn congestion events, receive-buffer drops | separates congestion loss from random loss |

**Policy.** *(Revision 3: for FEC-protected classes, steps 2, 3 and 6 are superseded by §4.8.2 and §4.8.4: reconstruction first, a receiver-triggered repair request, and RTT-dependent sizing.)* Evaluated per class about every 250 ms, with EWMA smoothing over ~2 s or 500 bundles and hysteresis.
1. **No FEC** while the estimated pre-FEC loss is below 0.3 % (a clean link pays zero, unlike today's permanent 10–15 %), and always for refinement passes.
2. **Is ARQ fast enough?** `t_arq = detect + RTT`, where `detect` is the receiver's gap detection: about ¼ RTT reorder window plus the ACK batching delay. If `t_arq ≤ deadline` and *p* is small, run "NACK-first": a minimal tail FEC (r = 1 per k = 20) only to absorb the occasional single loss without the RTT.
3. **Otherwise** pick (k, r) from a precomputed table: the smallest r/k meeting `residual_target` (e.g. 10⁻³ for critical tiles, 10⁻² for H.264 P-frames, 10⁻³ for keyframes) under the fitted GE model. Bounds: r/k ≤ `fec_max_overhead` (default 25 %) and the span `(k+r)·D·slot ≤ deadline − t_arq`. This is WebRTC's `kFecRateTable` approach, with burst structure added. The table is generated offline by a script like `fec_sim.py` and checked in as data.
4. **Interleave depth** D = min(ceil(b), D_max(deadline)). Bursts longer than the deadline allows go to ARQ, plus a reference-invalidation or keyframe request for H.264.
5. **Congestion versus random loss.** If loss coincides with goog_cc overuse or quinn congestion events, treat it as congestion: **do not raise FEC**, because FEC adds load to a congested link; let the BWE cut. FEC rises only for loss under a flat delay gradient (random or wireless loss).
6. **Deadline-aware retransmission** applies to both RTO and NACK. Do not retransmit a pass whose tile was superseded, which is already the case. Do not retransmit an H.264 fragment at all. Skip a critical-tile retransmit if `now + RTT/2 > emit + deadline`; send fresh content instead. Count each skip.

**Congestion-control interaction.**
- **Parity bytes are billed** to the pacer and the BWE budget, unlike today (`io_bridge/emit.rs:564-568`). A protected stream is then `rate·(1 + r/k)`, and the encoder target shrinks accordingly, as WebRTC does.
- **Recovered is not received for the estimator.** A recovered bundle releases content: ACK the content, cancel the retransmit. For goog_cc it is reported as *lost*, with no delay sample. FEC repairs content, and the network still dropped the packet. This fixes the deliberate inaccuracy noted at `reassembly.rs:251-263` and the late arrival times entering the delay gradient. WebRTC does the same in transport-wide feedback (§2) **[L]**. *(Revision 4: there is no flag and no estimator change. The rebuilt source is simply not ACKed as an arrival, so D1's RFC 9002 detector declares it lost. Content is released by a RECOVERED report that never touches congestion state, §4.9.)*
- **quinn's Cubic will still cut on every random loss.** Only D1's CC decoupling (a custom `congestion_controller_factory` controller that follows the app BWE, or at least does not punish isolated random loss) removes trigger (a) of §1.3. FEC v2 is not a substitute for D1.

### 4.4 Missing metrics: what, where, and how they travel

**Client, per session, in client-core** (shared by wasm and native). Each counter is cumulative plus per report interval.
- `bundles_received`, `bundles_lost_pre_fec`, plus the same three for the H.264 path (fragments)
- `bundles_recovered`, `residual_lost` (never recovered nor received by deadline)
- `dup_recovered`: a recovery of something already received; must be 0 after C3 PR 1
- `parity_received`, `parity_used`, `parity_wasted` (block had no loss), `parity_insufficient` (too many losses)
- `loss_run_hist[8]`: run lengths 1, 2, 3, 4, 5–8, 9–16, 17–32, >32
- `gap_run_mean`
- `reorder_max` (in `bundle_seq` distance) and `reorder_late` (arrived after its gap was declared)
- `recovery_delay_ms` p50 and p95 (lost slot → decoded)
- `rx_buffer_drops`: native, quinn `frame_rx.datagram` minus application reads; web, `WebTransport.getStats().datagrams.droppedIncoming` where supported
- `late_retransmit` (a retransmitted copy arriving for content already complete or superseded)
- Also populate the existing `ReceiverFeedback.datagrams_recovered_fec` (a no-wire-change fix, Wave C).

**Feedback wire format.** Client → server, QUIC datagram. **Type 0x07 is safe today**: `classify_inbound` maps 0x07–0x0F to `Unknown` (`ghostframe-protocol/src/protocol.rs:931`), and `dispatch_inbound_datagram` counts it as unroutable and drops it (`io_bridge/ack_dispatch.rs:452-468`) **[C]**.

```
LOSS_REPORT (0x07), sent every 250 ms or every 128 bundles, whichever first, only after the client has
received at least one FEC-v2 bundle (so an old server never sees it; if it did, it drops it harmlessly):
 [0]     0x07
 [1]     version = 1
 [2..4]  interval_ms        u16 LE
 [4..8]  first_bundle_seq   u32 LE   range covered (inclusive)
 [8..12] last_bundle_seq    u32 LE
 [12..14] received          u16      datagrams received off the wire in the range, BUNDLE and REPAIR
                                    (revision 4: repair symbols take bundle_seqs from the same counter)
 [14..16] lost_pre_fec      u16      bundle_seq gaps in range after the reorder window: sources AND
                                    repairs, i.e. channel loss
 [16..18] recovered         u16      sources rebuilt
 [18..20] residual_lost     u16      sources neither received nor rebuilt by close(B) or the deadline
 [20..22] parity_rx         u16      REPAIR datagrams received (a subset of `received`)
 [22..24] parity_used       u16
 [24..26] parity_wasted     u16
 [26..42] loss_run_hist     8 x u16
 [42..44] reorder_max       u16
 [44..46] recovery_p95_ms   u16
 [46..48] rx_buffer_drops   u16
 [48..50] h264_frags_lost_pre_fec u16
 [50..52] h264_frags_recovered    u16
 [52..54] h264_frames_lost  u16
 (54 bytes; unknown trailing bytes ignored, so later versions may append)

ACK_BATCH v4 (0x08), revision 5 (2026-10-26); full specification in section 4.9.2:
 [0] 0x08
 [1] n u8 (1..=72), then n x { bundle_seq u32 LE, arrival_time_ms_lo16 u16 LE }
     arrivals only: every BUNDLE and every REPAIR symbol the network delivered, never a rebuilt source
Sent only to a server that sent FEC-v2 bundles. RECOVERED, DECLINED and CONTENT_REJECTED are not in
the datagram: they are records on the reliable repair-status stream (section 4.9.2). (Revision 1 put
a `recovered` bit in bit 15 of arrival_time_ms_lo16; revision 4 a RECOVERED section in this
datagram, sent 3 times; both replaced, see section 4.9.9.)
```

**Server.** New fields on the `cumulative emit` line:
- `fec_parity_sent{tile,h264}`, `fec_parity_bytes`, `fec_overhead_ratio`
- per class: `fec_k`, `fec_r`, `fec_depth`
- `client_pre_fec_loss`, `client_residual_loss`, `client_recovered`, `parity_useful_ratio`, `burst_mean`, `ge_p_gb`, `ge_p_bg`
- `retx_by_cause{nack,rto,sched_retry,stuck}`
- `rto_spurious`: an RTO whose original transmission was later ACKed, from the ledger's `acked_after_declared_lost`; already counted, now attributed
- `retx_skipped_deadline`, `late_useless_retx`
- `quinn_lost_packets`, `quinn_sent_packets`, `quinn_congestion_events`, `quinn_cwnd`, `quinn_queue_bytes` (= send buffer − `send_buffer_space`)
- `client_rx_buffer_drops`

### 4.5 Wire and protocol changes, and compatibility

| change | direction | gating | old peer behaviour |
|---|---|---|---|
| HELLO caps **bit 2 = FEC v2** (bundles + RS + LOSS_REPORT + ACK v4) | c→s | the client sets it | old server: reserved bits ignored (`transport/client_caps.rs:13, 43-44`), so it keeps sending legacy traffic |
| **BUNDLE (0x09)**: `[0x09][ver/flags u8][bundle_seq u32][block_id u16][idx_in_block u8][k u8][r u8][records: (len u16, DatagramHeader+TileHeader+payload)…]` | s→c | **only to a client whose HELLO set bit 2** | an old client must never receive it. A byte below 0x80, ≥ 20 B and not 0x04 goes to H.264 assembly (`reassembly.rs:152-158`) and can set `latest_full_frame_seq` (C3 trap) |
| **REPAIR (0x0A)**: `[0x0A][ver u8][block_id u16][k u8][r u8][repair_idx u8][depth u8][bundle_seq u32][lens: k x u16][repair payload]`. *Revision 4:* the u32 is the repair symbol's **own** `bundle_seq`, drawn from the same counter as BUNDLEs, and no longer `first_bundle_seq`. Block membership comes from each BUNDLE's `block_id` and `idx_in_block`, so the field was redundant, and repair symbols are now transmissions (§4.9.1) | s→c | same | same trap; never sent to old clients |
| H.264 per-frame RS: frame parity datagrams with `frag_idx = n+1+j` (never `== frag_total`), `reserved` = scheme 1, an 11-byte RS header (C3 design) | s→c | bit 2 | old client: `is_parity()` true, so dropped (`frame_assembly.rs:64`); legacy XOR still sent to old clients |
| LOSS_REPORT 0x07, ACK v4 0x08 (arrivals only, §4.9.2) | c→s | sent only after FEC-v2 traffic arrives | old server: `Unknown`, counted and dropped |
| **Repair-status stream** (revision 5): a second client-opened bidirectional stream carrying RECOVERED (0x10), CONTENT_REJECTED (0x11) and DECLINED (0x12) records, `[type u8][len u16 LE][body]` (§4.9.2) | c→s | opened only after FEC-v2 traffic arrives | never reaches an old server. Its feedback dispatcher (`io_bridge.rs`, `dispatch_feedback_bytes`) would discard the rest of the buffer at the first unknown type |

- **Mixed versions.** The web client is embedded in the server binary (`ghostbridge/web_server.go`, `//go:embed`), so web and server always match. Only the **native client** can be skewed:
  - a new native client against an old server: the bit is ignored and the client sees only legacy traffic;
  - an old native client against a new server: the bit is never set and the server sends legacy traffic.

  Legacy 0x04 tile-parity *emission* is off for every session from D3-c on (§6.4 item 14(c), decided); its code stays until the owner declares old native clients unsupported, and D3-f deletes it (§6.4 item 8).
- **Do not reuse 0x04 for a new parity layout.** Old decoders assume contiguous membership and would fabricate datagrams (C3 A5).
- **Four-crate rule.** `ghostframe-protocol` (formats, codec), `ghostframe-lib` (server), `ghostframe-client-core` (decode, metrics), and `ghostframe-client-wasm` (export the new counters; constants). Add the new discriminators to `stream_message_types_are_distinct`-style tests on the datagram side.

### 4.6 Validation plan

Each test asserts a **property**, and each comes with a deliberate break that must make it fail (AGENTS.md).

| step | test tier | acceptance (property) | break it and watch it fail |
|---|---|---|---|
| GF(2^8) codec | unit + proptest (`ghostframe-protocol`) | for random k ≤ 64, r ≤ 16 and any erasure set of size ≤ r, decode returns the exact sources; with > r erasures it returns `None`, never wrong bytes; r=1 output == XOR | flip one coefficient in `coef()`; make `decode` accept r+1 erasures; drop the row normalisation (row-0==XOR assertion fails) |
| bundling | unit (server packer, client unpacker) + browserless | the unpacked record sequence equals the packed one; bundle bytes ≤ `max_datagram_size`; with FEC off, the browserless clean scene delivers identical tiles and the s2c datagram count drops ≥ 5x | let the packer exceed max size (quinn TooLarge, scene fails); drop the last record (tile assertion fails) |
| client metrics | unit with a scripted loss pattern (revision 4: drops on sources and on repair symbols) | `lost_pre_fec` == number of gaps injected, sources and repairs; `recovered + residual_lost == src_lost_pre_fec` (sources only; a lost repair is a gap that is neither); `src_lost_pre_fec + lost repairs == lost_pre_fec`; the run histogram equals the injected runs; `dup_recovered == 0` | count recoveries as receptions (equality fails); restore C3 defect 1 (dup > 0) |
| GE estimator | unit | over 50k synthetic packets from a known GE, the fitted (p, b) are within ±10 % and monotonic in the true parameters | swap p_GB and p_BG |
| adaptation table | unit | r/k is non-decreasing in p and in b; non-increasing in deadline − t_arq; 0 below the loss floor; ≤ cap | invert the RTT comparison (the monotonicity property fails) |
| loop stability | browserless with WS-A8's GE model, looped 12x (the harness is not seed-reproducible; feedback_browserless_not_seed_reproducible) | on a step 0 % → 2 % → 0 % loss, overhead rises and returns to 0 within 3 s; no more than 2 (k,r) changes per second; `residual ≤ target` in the steady state of the 2 % phase | feed post-FEC loss instead of pre-FEC (it oscillates; the change-rate property fails) |
| repair actually avoided | browserless, after D1 | `retransmit_attempts / net_lost_bundles ≤ 1.2` with FEC off (the D1 exit condition) and `≤ 0.3` with FEC v2 at 2 % iid; `rto_spurious ≤ 5 %` of fires | turn FEC emission off (the ratio returns to ~1) |
| H.264 RS | client-core access-unit test with NetSim GE (C3 plan) + e2e `OUTBOUND_LOSS_PREDICATE=all` under the heavy lock | frame-loss rate within 1.5x of the offline Monte Carlo for the same model; decoded frames bit-exact | no-op recovery (frame loss returns to the "none" column) |
| ACK v4: repair as transmission, RECOVERED, DECLINED, CONTENT_REJECTED on the repair-status stream, deferral (revisions 4-5) | protocol proptest, client-core unit, server unit, loopback, browserless | see §4.9.8: each test with its mutation | see §4.9.8 |
| caps safety | unit + e2e | a client without bit 2 receives zero 0x09/0x0A datagrams (counter over a whole e2e session); an old-format client still renders | send one bundle regardless of caps (the old client's frame assembly sees it; the test that counts H.264 assemblies created from non-H.264 bytes fails) |

E2E loss injection sits at the plaintext seam, not at the QUIC packet (feedback_harness_hook_plaintext_seam). With bundling, a datagram is a packet, so seam-level loss becomes representative. Without bundling it was not, which is one more reason the bundles come first.

---

### 4.7 What changed after reading Kyber's code (revision 2, 2026-10-15)

| item | before | after | why |
|---|---|---|---|
| H.264 FEC | per-frame RS, r = ceil(ρ·n) | add a **minimum repair count r_min = 2 for frames of ≤ 6 fragments**; one-fragment frames get 2 repair copies instead of none | Kyber `max(ceil(0.3n), 2)`; Moonlight minRequiredFecPackets; today `fec.rs:55-59` leaves a group of 1 unprotected |
| adaptation table (§4.3 step 3) | smallest r/k meeting the target | the same, **floored at r_min for the H.264 class** | as above |
| bundles (§4.1) | recommended | **unchanged, now corroborated**: Kyber's symbol = `max_datagram_size − 26` is the same "one symbol per full datagram" rule | `video_unreliable_fec.rs:320-327` |
| refinement passes (§4.1, §4.3 step 1) | ARQ by app NACK/RTO, no FEC | *revision 2 proposed QUIC streams with `RESET_STREAM`; **withdrawn in revision 3** (§4.8.1)*. Refinement keeps D1-fixed ARQ | quinn's stream recovery is sender-side and FEC-blind |
| client hint | — | set `congestionControl: "low-latency"` on the browser WebTransport | `net_kyproto.rs:200`. Affects only the browser's sending direction |
| receive deadline metric | `h264_frames_lost` | the same, defined explicitly as "skipped after the receive deadline". The H.264 receive deadline becomes a named constant reported in LOSS_REPORT's interval | Kyber's `MAX_BUFFERING` and `dropped_packets` |
| "learning" loop | modelled on WebRTC/FlEC | **unchanged.** Kyber has no such loop to copy (metrics are display-only), so the WebRTC/FlEC model stands | §2.1 points 1–2 |

### 4.8 Reconstruction first, retransmission only when reconstruction is impossible (revision 3, owner direction 2026-10-15)

**What changed and why.** The owner pointed out that on a high-latency path (75 ms RTT) any repair the *sender* starts on its own timer is likely to be wasted: it fires before the sender can know that Reed-Solomon has already rebuilt the data. This revision makes reconstruction the first repair for every FEC-protected class. Retransmission becomes a fallback that only the **receiver** can trigger, and only after reconstruction is shown to be impossible.

Concretely:
- §4.7's stream option is dropped (old decision #10).
- The repair policy below replaces §4.3 steps 2 and 6 for protected classes.
- New wire messages are added (REPAIR_REQUEST, extra repair symbols).
- Adaptation becomes explicitly RTT-dependent.
- D1 gains a sender-timer gating hook.

#### 4.8.1 Why the stream option and every FEC-blind sender timer are rejected

- **QUIC stream recovery is blind to our FEC.** quinn's stream loss recovery (RFC 9002) is sender-side. It declares a packet lost once 3 later packets are ACKed, or after 9/8 RTT, and re-sends it. It cannot know that the receiver has rebuilt the data from repair symbols. On a 75 ms path that re-send costs ≥ 75 ms and duplicates data RS would have restored a few ms after the block ended.
- **The same objection applies to any timer that fires on FEC-protected data before the receiver has reported**, including today's application RTO.
- **Measured in the model (`fec_sim_rtt.py`, `fec_sim_rtt_out.txt`; estimates **[E]**).** A timer of the QUIC kind (send + RTT + 3 slots, ACK-aware but FEC-blind) on top of RS(10,2):

  | path | retransmits sent / 1000 sources | of which **wasted** (arrived after RS had already rebuilt the data, or after the deadline) |
  |---|---|---|
  | 2 % i.i.d., 20 ms RTT | 14 | 13.3 (95 %) |
  | 2 % i.i.d., 75 ms RTT | 14 | 13.5 (96 %) |
  | 5 % i.i.d., 75 ms RTT | 39 | 31.9 (82 %) |

  These are exactly the retransmissions the owner expected.
- **The Kyber `finish()`-not-`reset()` trap of §2.1 remains a further reason**, but it is not the main one.
- **Moved to the rejected list:** "refinement passes on QUIC streams with `RESET_STREAM`" (§2.1 item (f), §4.7 row, old decision #10).

#### 4.8.2 Repair policy

**Protected classes are tile bundles and H.264 frames.** The policy for each block B:

1. **Reconstruction first, always.** The block decodes as soon as any k of its symbols have arrived. That is the normal path; nothing else happens.
2. **The receiver decides when B is unrecoverable.** It computes B's **recovery window**:

   `close(B) = t_expected(last symbol of B) + A`

   - `t_expected` is the arrival time of B's first symbol plus `(n−1) × ia`, where `ia` is the measured EWMA of bundle inter-arrival times for that class. The block header carries k, r and idx (§4.5).
   - A, the allowance, is `max(2 × ia, reorder_p99, 2 ms)`, where `reorder_p99` is the measured reorder depth in time.
   - If at `close(B)` the receiver holds fewer than k symbols, B is **unrecoverable as sent**.
   - For large blocks (H.264 keyframes, n > 32), the receiver may declare B unrecoverable earlier: as soon as r + 1 of B's symbols are known missing.

   The model shows that early declaration does **not** pay for small blocks (k = 10, a span of ~12 slots). Asking early for the shortfall known so far forces a second round when later symbols are lost too, so it is used only where the remaining span exceeds ~RTT/4 **[E]**.
3. **Fallback = REPAIR_REQUEST(B, m) for m more repair symbols**, where m is the shortfall. It is sent only if `now + RTT + A ≤ deadline(B)`, the earliest deadline of the block's still-missing content, i.e. RTT-gated as in WebRTC.
   - The sender answers with m **new** repair symbols, with indices never used before for that block, and sends them at once at the head of the pacer.
   - If the answer is itself short at `now + RTT + A`, repeat while the deadline allows.
4. **When the deadline does not allow any round:**
   - **tiles:** no more repair. *Revision 5:* this applies only while B is `Open`; an open deferral suspends it (§4.9.5, "Block state"). Tile content is lossless, so the deadline ends B's **repair phase**, not the delivery of its content. The server drops B's RS encode state, and every source of B not yet accounted for (no arrival entry, no RECOVERED) falls back to ARQ: it is re-encoded fresh if the tile is still current (the supersession path that already exists), or re-sent from the cache otherwise. The new transmission belongs to no FEC block (it goes out in an r = 0 bundle), so ordinary ARQ applies to it (§4.8.3, "Protection is per transmission") and it is retransmitted until it is delivered. Nothing is ever released as lost without a replacement on its way (§4.9.3, "Retention").
   - **H.264:** the frame is dropped and `h264_frames_lost` is counted. C5's keyframe request or reference invalidation fires (it is a new receiver→server signal and belongs to C5).
5. **Unprotected classes keep today's ARQ**, as fixed by D1/C4: CDF53 refinement passes, and any class whose adaptive r is 0. That means fragment and coverage NACKs plus the last-resort RTO (since D1: the RFC 9002 detector and the connection PTO, §4.8.3).

**Repair symbols versus re-sending sources: recommendation, repair symbols.**

The count is the same (m), but repair symbols win on every other axis:
1. **Any m of them fix any m losses.** The request does not have to name which sources are missing. It cannot target the wrong datagram, and it survives the receiver's partial knowledge, for example a missing symbol that is merely reordered and arrives later.
2. **A lost response just shrinks the next request.** No identity bookkeeping, no duplicate risk.
3. **It is exactly what an MDS code is for.** The sender keeps the block's k sources until the block deadline and encodes new rows on demand.

Costs:
- The block must stay cached until its deadline. At 9 Mbit/s with a 150 ms deadline that is ~170 KB **[E]**.
- One encode per request: m rows × k × 1.2 KB, which is microseconds.
- The GF(2^8) code limits `k + r + extra ≤ 256`. That is fine for k ≤ 64 blocks, and the request is clamped there.

Re-sending specific sources would keep the old fragment-NACK machinery, but it inherits the identity problems §1.3 (f) shows.

#### 4.8.3 What the sender's timers may still do (interaction with D1)

*Revision 4: this subsection is rewritten against the transport D1 actually landed on master (`93e5efec`).* The per-pass RTO that revision 3 replaced (`emitter.rs:207-218` at `5098db6`) no longer exists. D1 replaced it with an RFC 9002 threshold detector and one probe timeout per connection, and the `TimerPolicy` hook revision 3 asked D1 for was **not** landed. D3-d therefore adds the protected-class rules below directly in the paths that exist.

**Retransmit paths on master, and what each does for protected content.** All of them end in `ReliableTileEmitter::request_retransmit`, which asks `classify_retx` (`reliable_emitter/emitter.rs`) whether the content is current:

| path | today | for protected content (D3-d) |
|---|---|---|
| `loss_path::detect`: every threshold verdict calls `request_retransmit(.., RetxCause::Loss)` | resends the pass | **refused**: the verdict is congestion state only |
| `loss_path::detect`: `take_due_deferred`, passes a verdict parked after `LOSS_RETX_MAX_ATTEMPTS`, resent as `RetxCause::Probe` | resends the pass | **refused** (protected content is never parked there) |
| `loss_path::probe`: the PTO, one timer per connection, probing the newest 2 outstanding transmissions | resends their passes; a `ContentGone` candidate is declared lost by PTO | a protected candidate is answered with **one new repair symbol of its block**, which is a transmission and draws an ACK, never with a source. A block in deferral (§4.9.5) is skipped |
| TILE_NACK (`pending_nack_set`) | resends the pass | the client sends REPAIR_REQUEST, not TILE_NACK, for protected classes; a TILE_NACK naming protected content is refused and counted |

The refusal is a new `RetxRequest::Protected` returned by `classify_retx` under causes `Loss` and `Probe`. It is counted as `protected_timer_suppressed` (§4.8.5).

**Protection is per transmission, not per content key (revision 5).** `classify_retx` returns `Protected` only when the transmission it is asked about, the ledger record handed to it, is a member of an FEC block (`block: Some(_)` on the record). A content retransmit made by DECLINED, CONTENT_REJECTED, `T_def` or the deadline hand-off goes out in an r = 0 bundle, as refinement passes do. Its record has no block, so a loss verdict or PTO on it retransmits as for any unprotected transmission, and D1's ARQ carries it until it is delivered. Keyed on content instead, the refusal would apply to that retransmit too: lost, it would have no block, so no RECOVERED and no deadline, and it would be stranded for good. The retransmit does not join a new FEC block either. The hand-off happens because FEC's budget for that content is spent, the retransmit is a handful of records, and a block would put already-late content back through deferral and deadlines; ARQ ends with the content delivered. A content key can therefore have a protected original and an unprotected retransmit outstanding at once: a verdict on the first is refused, a verdict on the second retransmits.

**A protected source's content is therefore retransmitted only through one of these paths:**
- a REPAIR_REQUEST, answered with new repair symbols (not sources);
- a DECLINED record (revision 5, §4.9.2), answered at once with a content retransmit of the sources it names;
- a CONTENT_REJECTED record, answered with a retransmit of the rejected record's tile (§4.9.3);
- the deferral backstop `T_def` of §4.9.5 (60 s for tiles), answered with a content retransmit. With RECOVERED and DECLINED reliable it fires only on a client bug, so `deferral_timeout` is an alarm;
- the deadline release (§4.8.2 step 4): for tiles, the repair phase ends and the content falls back to ARQ; for H.264, the frame is dropped. **Every protected class MUST implement the deadline release before it is switched to the protected rules** (§6.4 item 14(d), decided): it is the backstop when the client cannot speak, because it never learnt the block exists or holds fewer than k symbols and its REPAIR_REQUESTs were lost (§4.9.5, "When the server's view is wrong").

Only the first and the deadline release apply to H.264: H.264 content is never retransmitted (§4.3 step 6), so H.264 has no deferral, no `T_def` and no content path (§4.9.5).

**The last-resort timer** for lost feedback remains:

`T_lr(B) = send(last symbol of B) + W_B + RTT + 2·rttvar`

where `W_B` is the sender's estimate of the receiver's window: block span plus the allowance reported in LOSS_REPORT. Any of these cancels it:
- **every** source of B accounted for: each one either has an arrival entry or is named by a RECOVERED id (one arrival for some symbol of B is not enough: it says nothing about the other sources);
- a REPAIR_REQUEST for B;
- a LOSS_REPORT whose range covers B;
- *(revision 5)* a DECLINED record for B, or the start of B's deferral (held(B) ≥ k): the client can decode, its outcome will arrive reliably, and a repair symbol would not help it.

If `T_lr` fires, the sender sends `r_lr = 1` extra repair symbol (a transmission with its own `bundle_seq`) and re-arms once. `T_lr` itself never resends sources. After the deadline it releases B, as in §4.8.2 step 4.

**The ledger** declares a protected transmission (source or repair) lost **only through D1's RFC 9002 detector** (packet threshold `K_PKT`, time threshold 9/8 RTT, `transmission_ledger.rs`), which works only on ACKs, plus `declare_lost_by_pto` for a probe candidate whose content is gone. LOSS_REPORT and REPAIR_REQUEST are not ledger loss sources: they feed adaptation and repair. (Revision 3 also listed them and a horizon; D1 removed the horizon, which also removes §1.3 step (c) for all traffic.) **A loss verdict on protected data is congestion state only**: `loss_window` and goog_cc, never a retransmission.

**The PTO and `T_def`.** *(Revision 5: a deferral normally closes within one uplink delivery of the RECOVERED record, so the interplay below matters for milliseconds, not for `T_def`'s 60 s. On the client-bug path a PTO whose candidates are all in deferral sends nothing and backs off as for any fire.)* The PTO runs from `max(newest departure, last_ack_at, last fire) + srtt + 4·rttvar + max_ack_delay`, so after the ACK that brings held(B) to k it can expire before `T_def`. That is harmless because of the table above: the probe skips a block in deferral and never sends protected source content. The PTO stops (`pto_deadline` is `None`) when no content is cached. A RECOVERED that releases the last cached content can therefore disarm it. That is the same effect any content release has today, and it is correct: the PTO exists to recover content, and repair records hold none. Outstanding repair records are declared lost by the detector on the next ACK, or leave the ledger at retention expiry.

**The split between D1 and D3:**
- **D1** (landed) made the timers honest: departure-time clocks, the RFC 9002 detector, the connection PTO, deferred passes. It did not add a per-class policy hook, and none is needed before D3.
- **D3-d** adds `RetxRequest::Protected` in `classify_retx`, the probe's repair-symbol answer, `T_lr`, the deadline release for every protected class, REPAIR_REQUEST serving (client side in D3-b) and the deferral of §4.9.5.
- Today's H.264 path has no timer at all, so it only gains the request path.

#### 4.8.4 Adaptation: RTT-dependent, biased toward FEC

Because the fallback costs at least `RTT + A` and may simply not fit before the deadline, the redundancy chosen up front must cover the expected loss whenever `RTT` is large compared with the class budget. This replaces §4.3 steps 2–3 for protected classes:

- Let `slack = deadline_budget − (block span + A)`, and let `rounds = floor(slack / (RTT + A))`, the number of repair rounds that can still land.
- **rounds = 0** (e.g. H.264 with a 50 ms budget at 75 ms RTT): choose (k, r, D) so that *first-shot* residual ≤ target. FEC alone must carry it.
- **rounds ≥ 1**: choose the smallest r that brings `P(block short) × P(short after one round)` within the target. That is lighter FEC, because a single cheap, targeted round backs it up.
- **r_min** applies as before: 2 for small H.264 frames. The 0.3 % no-FEC floor applies only where `rounds ≥ 1`. **Where `rounds = 0`, the floor is r = 1 at any non-zero measured loss.**
- **Bursts:** depth D only where the span `(k+r)·D·slot` fits within `slack` once.
- **WebRTC comparison:** WebRTC's split is "NACK below 20 ms RTT, FEC above". Ours is the same idea expressed per class deadline, and biased toward FEC: even at low RTT the protected classes keep their FEC, and the request is the fallback.

The table is precomputed offline over (p, b, RTT, budget) with `fec_sim_rtt.py`'s model.

**Model results** (RS(10,·), 1 ms slots, tile budget 120 ms and H.264 budget 50 ms after the one-way delay; `fec_sim_rtt_out.txt`) **[E]**. Columns: residual / repair-latency p95 for sources whose original was lost / overhead / retransmits per 1000 sources (wasted):

| path | policy | tile | H.264 |
|---|---|---|---|
| 20 ms, 2 % iid | timer + RS(10,2) | 0 / 20 ms / 21.4 % / 14 (13.3) | 0 / 20 / 21.5 % / 15 (14.0) |
| 20 ms, 2 % iid | **reconstruction-first, RS(10,2)** | **0 / 20 / 20.0 % / 0.1 (0)** | 0 / 20 / 20.0 % / 0.2 (0) |
| 20 ms, GE 2 % b3 | timer + RS(10,2) | 0 / 33 / 21.9 % / 19 (4.8) | 1e-5 / 33 / 22.2 % / 22 (4.6) |
| 20 ms, GE 2 % b3 | reconstruction-first, RS(10,2) | 0 / **44** / 20.8 % / 8.5 (0) | 5.4e-4 / 43 / 20.8 % / 8.5 (0) |
| 75 ms, 2 % iid | timer + RS(10,2) | 0 / 47.5 / 21.4 % / 14 (13.5) | 3.4e-4 / 47.5 / 21.5 % / 15 (14.5) |
| 75 ms, 2 % iid | **reconstruction-first, RS(10,2)** | **0 / 47.5 / 20.0 % / 0.1 (0)** | 4.1e-4 / 47.5 / 20.0 % / 0 |
| 75 ms, 2 % iid | reconstruction-first, RS(10,3) | 0 / 47.5 / 30 % / 0 | **0** / 47.5 / 30 % / 0 |
| 75 ms, 5 % iid | timer + RS(10,2) | 2.6e-4 / 115.5 / 23.9 % / 39 (31.9) | 4.6e-3 / 47.5 / 23.7 % / 38 (35.6) |
| 75 ms, 5 % iid | reconstruction-first, RS(10,2) | 3.5e-4 / 122.5 / 20.2 % / 2.5 (0) | 5.6e-3 / 47.5 / 20 % / 0 |
| 75 ms, 5 % iid | **reconstruction-first, RS(10,3)** | **0 / 48.5 / 30 % / 0.4 (0)** | 1.1e-3 / 47.5 / 30 % / 0 |
| 75 ms, 5 % iid | reconstruction-first, RS(10,4) | 0 / 48.5 / 40 % / 0.1 | **1.1e-4** / 48.5 / 40 % / 0 |
| 75 ms, GE 2 % b3 | timer + RS(10,2) | 4.1e-4 / 115.5 / 21.9 % / 19 (4.8) | 1.4e-2 / 48.5 / 21.9 % / 20 (19.1) |
| 75 ms, GE 2 % b3 | reconstruction-first, RS(10,4) | 7.0e-4 / 126.5 / 40.4 % / 4.0 (0) | 7.6e-3 / 49.5 / 40 % / 0 |
| (baseline) 75 ms, 2 % iid | timer, no FEC | 4.2e-4 / 115.5 / 2.1 % / 20.7 | **2.0e-2** (every loss is late) |

What it says:
1. **Reconstruction-first wastes no retransmissions in the model, against 82–96 % wasted for a FEC-blind timer.** It also sends 50–150x fewer repairs at the same residual for i.i.d. loss, at both RTTs.
2. **At 75 ms the fallback rarely helps H.264.** A 50 ms budget leaves `rounds = 0`, so the adaptation must raise r: RS(10,3) at 2 %, RS(10,4) at 5 %. This is the "cover the expected loss up front" rule, made quantitative.
3. **Cost, honestly:** under bursts at 20 ms RTT, reconstruction-first's p95 latency for lost sources is ~44 ms against ~33 ms for the timer. It waits for the window before asking, where the timer fires at RTT + 3 slots, and that is the price of not wasting. It narrows with larger r, which turns more bursts into immediate reconstructions. Within the tile budget it changes no residual.
4. **Bursts at 75 ms are expensive for every policy.** Residual 0.07–1.4 % stays until interleave depth D is applied (§3: depth 4–8 recovers 86–96 % of 3-packet bursts), which only fits where the slack allows. H.264 there needs C5's recovery. The model's limits: no jitter, no reordering, feedback never lost, repairs lost i.i.d.

#### 4.8.5 Wire and metrics additions (all under HELLO bit 2)

```
REPAIR_REQUEST (client -> server datagram 0x0B; 0x07-0x0F are Unknown to old servers and dropped, protocol.rs:931)
 [0] 0x0B  [1] version=1  [2] class (0 = tile bundle, 1 = H.264)  [3] count n (1..16)
 n x { block_id u32 LE (tile: block_id; H.264: frame_seq), have u8 (symbols held), missing u8 (m requested),
       first_new_idx u8 (lowest repair index the client has not seen, so a duplicate request is idempotent) }
REPAIR (server -> client, 0x0A as in section 4.5): the repair_idx field may now exceed the block's original r (a
 requested extra symbol). The client accepts any idx < 256 - k.
LOSS_REPORT (0x07) gains, at its tail (older parsers ignore trailing bytes):
 [54..56] recovery_allowance_ms u16  (A, as the client computes it)
 [56..58] inter_arrival_us u16       (ia EWMA, bundles)
 [58..60] blocks_unrecoverable u16   (closed short)
 [60..62] repair_requests_sent u16
 [62..64] repair_symbols_received_useful u16
 [64..66] repair_late u16            (requested symbols that arrived after the block decoded or after its deadline)
 [66..68] repair_skipped_deadline u16 (requests not sent because RTT + A did not fit)
 [68..70] src_lost_pre_fec u16     (revision 4) sources missing in closed blocks, by idx_in_block;
                                   recovered + residual_lost == src_lost_pre_fec <= lost_pre_fec
```

New fields on the server's `cumulative emit` line:
- `repair_requests_rx`, `repair_symbols_sent`
- `lr_timer_fired`, the last-resort timer; non-zero only when feedback is lost
- `retx_avoided_by_reconstruction`: protected sources lost pre-FEC and released with no repair traffic, counted from RECOVERED ids (§4.9)
- `recovered_rx` (distinct RECOVERED ids that released content), `recovered_unknown`, `repair_acked`, `deferral_started`, `deferral_released` (closed by RECOVERED), `deferral_timeout` (closed by `T_def`, content retransmitted; **an alarm**: non-zero means a client bug or a wedged repair-status stream, and it is logged at warn, rate-limited), `recovered_ids_rx_distinct` (distinct RECOVERED ids received whatever the ledger state, for the FECORACLE), `have_mismatch` (REPAIR_REQUEST `have` < k while held(B) ≥ k), `protected_retx_refused` (per path: verdict, deferred, PTO, TILE_NACK), `deadline_released` (repair phase ended, content handed to ARQ); revision 4, §4.8.3 and §4.9.5
- *(revision 5)* `declined_rx{pruned,evicted,invalid_symbol,length_bound,header_mismatch}`, `deferral_declined` (closed by DECLINED), `content_rejected_rx`, `h264_declined`, `repair_status_malformed` (records the server could not parse; the rest of that stream's buffer is discarded, so this is also an alarm), `deferral_pinned_max` (high-water mark of ledger records pinned by open deferrals), `deferral_pinned_overflow` (deferrals resolved early by the pin cap), `block_closed_late_trigger`, `repair_status_stream_reopened` (client) and `repair_status_stream_stopped` (server)
- `repair_after_recovery` and `repair_after_deadline`, which the client reports as `repair_late`
- `protected_timer_suppressed`: protected entries that would have had a timer under today's policy

**Validation additions** (in the style of §4.6):
- **Unit, client.** For scripted symbol arrival patterns, a REPAIR_REQUEST goes out only when held < k at `close(B)`, with `missing = k − held`. **Break:** fire at the first gap; the "no request for a recoverable block" property fails.
- **Unit, server.** A protected source's content is never retransmitted by `loss_path::detect`, `take_due_deferred`, the PTO or a TILE_NACK; its only content retransmits are DECLINED, CONTENT_REJECTED, the `T_def` backstop and the deadline hand-off to ARQ (§4.9.5). **Break:** let `classify_retx` return `Queued` for protected keys; the counter-based test fails.
- **Browserless after D3.** `repair_after_recovery == 0` and `retx_avoided_by_reconstruction / lost_pre_fec ≥ 0.9` at 2 % i.i.d. with a 75 ms netsim delay (12 runs). **Break:** re-enable submit-time RTO for protected entries; wasted repairs > 0.

### 4.9 ACK v4: repair symbols are transmissions, rebuilt sources are not arrivals (revision 4, owner decision 2026-10-26)

**What changed and why.** Revisions 1-3 gave ACK v4 a `recovered` bit, taken from bit 15 of `arrival_time_ms_lo16`, on an entry that otherwise looked like an arrival. On 2026-10-26 the owner replaced it with the design below. It keeps apart two things an acknowledgement used to mix:
- **what the network carried** drives congestion control, RTT and bandwidth estimation;
- **what the client holds** drives content release.

The receiver states both, and the sender guesses neither. This is the consensus shape in the literature **[L]**:
- RFC 9265 §6.3 Recommendation 1;
- FlEC's SYMBOL_ACK (draft-michel-quic-fec §5.5–6);
- WebRTC transport-wide feedback: FEC packets carry their own transport sequence number, and recovered media packets are not reported as received;
- Tetrys' window update (RFC 9407).

**Revision 5 (owner decisions, 2026-10-26, §6.4 items 14-15).** Rules 1, 2 and 4 are unchanged. RECOVERED moves from an ACK v4 section to a reliable client→server stream, where it is sent once (rule 3, §4.9.2). Rule 5 is decided: arrivals report the network, and a CONTENT_REJECTED record on the same stream reports a bad record. A new DECLINED record lets the client say that it holds k symbols of a block and will not rebuild it. With every terminal client outcome reliable, `T_def` becomes a 60 s backstop for the tile classes, and H.264 has no deferral (§4.9.5).

#### 4.9.1 Rules

1. **A repair symbol is a transmission.** Every REPAIR datagram (0x0A) takes a `bundle_seq` from the same per-session counter as the BUNDLEs (0x09). That includes the extra symbols sent for a REPAIR_REQUEST and the `T_lr` symbol. Each one is:
   - recorded in the server's transmission ledger with its departure time, size and `(block_id, k)`;
   - billed to the pacer (§4.3) and registered with goog_cc as a sent packet;
   - ACKed by the client on arrival, exactly like a source bundle.

   RTT, the RFC 9002 loss detector, goog_cc feedback and the delivery-rate sampler therefore see every byte the network carried. Because repairs and sources share one counter, the sources of a block are **not** contiguous in `bundle_seq`, so no `first + idx` arithmetic can name them. **An RS source symbol is the whole BUNDLE datagram**, from its `0x09` byte through its last record, header and `bundle_seq` included, zero-padded to the block's symbol length. REPAIR's `lens` array gives each source datagram's true length. A rebuilt source therefore carries its own `bundle_seq`, `block_id` and `idx_in_block`, and the client reads the id it reports from the rebuilt bytes (§4.9.2, "Ids"). A repair record has no content, so its ACK is transport-only and releases nothing. For FEC-v2 traffic this settles PR #239's open question: parity bytes were delivered but not counted (the ~0.85 payload factor), and now they are counted. Pre-FEC loss measured from `bundle_seq` gaps (§4.3) then measures every packet of the channel, repairs included.
2. **A source the client rebuilt is not an arrival.** The client never puts a rebuilt source's `bundle_seq` in an arrival entry. Its absence becomes a loss through the RFC 9002 threshold detector D1 built: `K_PKT` later departures acknowledged, or 9/8 RTT elapsed. That is the same path as any other lost packet. Congestion control reads it as lost and untimed (RFC 9265 Rec. 1), and no congestion code path knows about FEC.
3. **A separate RECOVERED record releases content.** It names the `bundle_seq` of each rebuilt source. *Revision 5:* it travels on the **repair-status stream**, a reliable client→server QUIC stream (§4.9.2), and each id is written once; quinn, or the browser's QUIC stack, retransmits it on loss. (Revision 4 carried it in an ACK v4 section, three times.)
   - **It MUST NOT feed congestion state.** It produces none of the following: a loss verdict or retraction, an RTT, rttvar or `min_rtt` sample, a goog_cc arrival, a delivery-rate sample (the batch-level `delivery_sampler.end_ack` included), `K_PKT` evidence, a PTO restart (`last_ack_at` is not moved) or a backoff change. It does not move `largest_acked`. Its only effect on the PTO is the one every content release has: releasing the last cached content disarms it, which is correct because the PTO exists to recover content (§4.8.3).
   - **Its effects are limited to two.** It releases the record's content: `on_ack` drops the cache entry, marks coverage and cancels any retransmission. It also closes the deferral of §4.9.5.
   - **It does not resolve the ledger record.** The record stays outstanding until the detector declares it lost or its original arrives. On a record already declared lost it reads the tombstone without consuming it (today's `resolve_at` removes the tombstone), so a late original still finds it and takes the spurious-verdict path.
4. **A late original is an ordinary arrival.** Sometimes the original of a source the client already rebuilt arrives later. The client ACKs it with an arrival entry like any other arrival: it deduplicates the content, never the ACK. On the server, that ACK is one of two things:
   - a Live ACK, if no verdict has been declared yet. It is a normal delivered transmission with its real arrival time;
   - an ACK of a transmission already declared lost. That takes the **existing spurious-verdict path**: `loss_window` retraction, `pending_losses` withdrawal if not yet flushed, a late delivery-rate sample, and `K_PKT` evidence.

   Content release is idempotent, so the earlier RECOVERED changes neither case. **No "rebuilt then received" rule is needed, and that is the reason this design was chosen over the flag.** With a rebuilt bit, the same `bundle_seq` could be ACKed twice: first as rebuilt, then as received. The server had to tell the second ACK apart from an overlap repeat of the first. That needed a time-bounded `rebuilt` set and its own ledger branch. Here an identity appears as an arrival entry at most once per network arrival, and a repeat of it resolves to "already acknowledged", as it does today.
5. **Arrivals report the network; CONTENT_REJECTED reports bad records** (§6.4 item 15, option A, decided 2026-10-26). Every received datagram gets an arrival entry, including a bundle with a record that fails prevalidation. The client names that record in a CONTENT_REJECTED record on the repair-status stream, and the server re-sends its tile (§4.9.3). This replaces today's deferred ACK, in which a CDF53 pass that fails prevalidation is deliberately not acknowledged (`ghostframe-client-core/src/reassembly.rs`) so that it is retransmitted: with bundles, withholding the arrival would tell congestion control that the network lost a packet it delivered, and would hold back the bundle's other records. A rebuilt bundle follows the same rule: RECOVERED names the bundle, and CONTENT_REJECTED names its bad record, written first on the same stream (§4.9.2, "Ordering"). The existing DECODE_ERROR message (0x04 on the feedback stream) is not a substitute: it is keyed by tile and code, not by transmission, and rate-limited to 1/s per tile and 32/s in all, so it cannot say which record to withhold from release. It stays as the diagnostic and palette-recovery path it is today.

#### 4.9.2 Encoding

*Revision 5:* the client→server report now uses two channels. The ACK datagram carries arrivals only, because arrivals must be timely and a lost one is covered by the overlap repeat. RECOVERED, CONTENT_REJECTED and DECLINED travel on a reliable stream, because each one must arrive and none is needed within the next few milliseconds.

```
ACK_BATCH v4 (0x08), client -> server datagram. Arrivals only. The client switches from 0x06 to
 0x08 when it receives its first FEC-v2 bundle (0x09 or 0x0A), never on the HELLO bit alone: until
 then the server may be one that does not send bundles, and it would drop 0x08 as Unknown and see
 no ACKs at all.
 [0]          0x08
 [1]          n  u8, 1..=72                    fresh + overlap, as in 0x06
 [2 .. 2+6n]  n x { bundle_seq u32 LE, arrival_time_ms_lo16 u16 LE }    6 B per entry
 Valid only if n >= 1 and the datagram ends exactly after the last entry.

REPAIR-STATUS STREAM, client -> server, reliable. A second client-opened bidirectional stream. The
 server never writes on it, and the client does not finish it while the session lives. The client
 opens it when it switches to 0x08. Its content is a sequence of records:
 [0]          type u8      0x10 RECOVERED | 0x11 CONTENT_REJECTED | 0x12 DECLINED
 [1..3]       len  u16 LE  body length: a non-zero multiple of the type's entry size, <= 1020
 [3 .. 3+len] len / entry_size entries:
   RECOVERED         { bundle_seq u32 LE }                                              4 B
   CONTENT_REJECTED  { bundle_seq u32 LE, record_idx u8 }                               5 B
   DECLINED          { class u8 (0 = tile bundle, 1 = H.264), block_id u32 LE (H.264: frame_seq),
                       reason u8, missing u64 LE }                                     14 B
     reason: 1 pruned, 2 evicted, 3 invalid_symbol, 4 length_bound, 5 header_mismatch
     missing: bit i set = source idx_in_block i that the client neither received nor rebuilt
              (tile k <= 64, which the codec clamps; 0 and ignored for H.264, whose frame is lost whole)
 A record split across stream reads waits for its tail. An unknown type, or a len that is 0, not a
 multiple of the entry size, or > 1020, or a stream header with the wrong session id, is a protocol
 error. A length-prefixed reliable stream cannot be resynchronised at an arbitrary read boundary, so
 the server stops reading the whole stream: it counts repair_status_malformed, discards the stream's
 buffer, and sends STOP_SENDING with an application error code. The client sees its write fail and
 reopens the stream, resending its retained log (see "Stream lifecycle"). A third malformed stream
 in one session closes the connection with an application error. A client with more entries than
 fit writes several records.
```

The records are length-prefixed, unlike the existing feedback-stream messages, which have a fixed size per type. A parser can therefore check a record's length before it reads it, and a later revision can add an entry field without a new type.

- **Which stream (checked on master `cf533c22`).** The client already opens one reliable stream once the session is up: the bidirectional **feedback stream**.
  - Native: `ghostframe-client-net/src/lib.rs`, `feedback_stream`, opened with `open(Dir::Bi)` at `SessionReady` and written from `PollOutput::Stream`.
  - Web: `ghostframe-web-client/src/main.ts`, `transport.createBidirectionalStream()`, written through `feedbackWriter`.
  - It carries HELLO, ReceiverFeedback, DECODE_ERROR, INPUT, DISPLAY_INFO and DISPLAY_MODE (`ghostframe-protocol/src/feedback_stream.rs`).
  - The server routes every peer-opened bidi stream after CONNECT to `feedback_queue` (`transport/webtransport.rs`, `try_decode_connect`) and parses it in `IoBridge::dispatch_feedback_bytes` (`io_bridge.rs`).
  - Nothing else uses a stream. ACKs, NACKs and, later, LOSS_REPORT and REPAIR_REQUEST are datagrams (web: `transport.datagrams.writable`, `ackWriter`). There is no client unidirectional stream: the server reads peer uni streams as HTTP/3 control streams (`try_decode_settings`).
- **Decided (owner, 2026-10-26, §6.4 item 16): a second bidirectional stream, not the feedback stream.** The owner's reason: the two streams have a clearly different intent and purpose. Head-of-line blocking points the same way. The feedback stream carries keystrokes and pointer events. On it, a lost packet holding a RECOVERED record would also hold back every input event written after it until the sender retransmits that packet, about one RTT later. The effect is modest **[E]**. Input alone (pointer events at 60-125 Hz plus periodic feedback) already puts ~5-9 packets on the stream per RTT at 75 ms, so at 1 % uplink loss ~5-9 % of input events already wait behind a lost packet. Repair-status writes during heavy recovery (≤ 55 per second, §4.9.2 "Size cost") add ~4 more per RTT, about +4 points.
  - A second stream costs one `open(Dir::Bi)` or `createBidirectionalStream()` per front end.
  - It is bidirectional rather than unidirectional because the server already routes peer bidi streams to the feedback dispatcher, while its uni path would have to learn the WebTransport stream type.
  - The alternative, the same records on the existing feedback stream, was rejected. The client MUST write the records on the repair-status stream. The server dispatches by message type on any stream, so a misrouted record is still parsed, but test 4 fails it.
- **This is not the stream option that §4.8.1 rejects.** That objection is to carrying FEC-protected *content* on a stream: quinn's recovery would re-send data that RS had already rebuilt. The repair-status records are feedback with no FEC of their own, so stream recovery duplicates nothing, and reliability is exactly what they need.
- **What the second stream needs on the server:**
  - **Per-stream buffers (required).** `feedback_queue` drops the stream id today, and `feedback_recv_bufs` accumulates per connection handle. With two streams, one stream's bytes could complete the other's partial message. `feedback_queue` must carry the `StreamId`, and the accumulator must be keyed by `(handle, StreamId)`.
  - The three types join `FEEDBACK_STREAM_MSG_TYPES`, so `stream_message_types_are_distinct` covers them. 0x10-0x12 are chosen so they alias no datagram discriminator (0x04-0x0B are all in use there), which removes a source of confusion the module doc of `feedback_stream.rs` already warns about.
  - **Per-stream WebTransport header strip and validation (required).** A browser starts every bidi stream it opens with the WebTransport stream signal, `varint(0x41)` (bytes `0x40 0x41`), then `varint(session_id)`. No code on master strips it. The live web session already shows the effect on the feedback stream: the guest journal of Sep 25, 17:50:10.205, logs "unknown feedback-stream message type; dropping rest of buffer" with `msg_type=64` (0x40), then HELLO 0.7 ms later. It works only because Chrome sent the header in a packet of its own. Had the header and HELLO shared a read, HELLO would have been discarded with the rest of the buffer.
    - On the first bytes of each peer-opened bidi stream after CONNECT, the server MUST strip the header if present and check that `session_id` is the session's CONNECT stream id. A wrong id is a malformed stream (below).
    - The native client writes no header. Detection is unambiguous: every feedback-stream and repair-status message type is below 0x40, so a first byte ≥ 0x40 can only be the signal.
    - The header may arrive split across reads, so the strip is per stream and waits for enough bytes.
    - The fix for the existing feedback stream (HELLO) is being made on master now, as a small separate code PR. D3 relies on it and extends it to the second stream. Tests 17 and 18 cover it; test 18 runs a real browser, because the native client and test 15 never send a header.
- **What it needs on the clients.**
  - Client-core gets a new `PollOutput::RepairStatus(Vec<u8>)` (`event.rs`), so each front end routes it to the second stream. The wasm `pollTransmit` reports it as `kind: 'RepairStatus'`.
  - The web client's `drainTransmit` today `await`s `feedbackWriter.write` inside the loop that also writes the ACK datagrams. A repair-status write must not be awaited there: a stalled stream would hold the ACKs back. Write it without awaiting, with a `catch`, as `ackWriter` is written.
  - The client batches records at the ACK batcher's flush (`FLUSH_INTERVAL_US`, 5 ms): at most one record per type per flush.
- **Stream lifecycle.**
  - **Opening.** The web client's `createBidirectionalStream()` is asynchronous, and native `open(Dir::Bi)` returns `None` when the peer's stream limit is reached. Records produced before the stream exists are held in order: in the client-core outbox, which emits `RepairStatus` only once the front end has reported the stream open, or in the front end's FIFO. A native `None` is retried on every poll, as `feedback_stream` is today. Nothing is dropped while waiting.
  - **Retained log.** The client keeps every record it wrote for `T_DEF_LOSSLESS` (60 s) after writing it. After that the server has acted on its own (`T_def`). At ~55 ids/s that is ~3 k entries, ~13 KB **[E]**.
  - **Reset, STOP_SENDING or a writer error.** The client opens a new repair-status stream and writes every record in its retained log again, in the original order. Neither the browser nor quinn tells the application which stream bytes the server has read, so resending is the only safe choice. It is correct because the server's handling of every record type is idempotent: RECOVERED is a release (§4.9.2, "Ordering"); DECLINED on a closed block is a no-op (§4.9.5, "Block state"); CONTENT_REJECTED whose generation is no longer current is a no-op (§4.9.3). On the server, a stream that ends or is reset drops its buffer, including any partial record. The new stream is accepted like the first (`MAX_PENDING_BIDI_STREAMS` bounds how many can be pending). Counter: `repair_status_stream_reopened`.
- **Ids.** A RECOVERED id is the rebuilt source's own `bundle_seq`, so the server finds it with the same O(1) ledger lookup as an arrival. The client reads it from the rebuilt datagram's own header: the RS source symbol covers the whole BUNDLE datagram (§4.9.1 rule 1). The client then checks that the rebuilt header's `block_id` and `idx_in_block` match the hole it filled. A mismatch is a decode failure: it is counted, and the client sends DECLINED with reason `header_mismatch`. Covering the header costs ~10 B per symbol (<1 % of a 1.2 kB bundle). The alternatives for naming sources were all rejected:
  - REPAIR listing its block's k source `bundle_seq`s: 4k B per repair symbol, ~4 % at k = 10;
  - a separate source counter, so that `first_bundle_seq + idx` works: two transmission-identity spaces for one ledger, which is what rule 1 exists to avoid;
  - `first + idx` over the shared counter: wrong as soon as a repair of another block, or a retransmission, departs between two sources.

  Alternatives for encoding the RECOVERED ids:
  - `(block_id, bitmap)`: smaller only for blocks with several recoveries, and it needs a block index on the server for a report that is a few per cent of entries;
  - `(block_id, idx)` pairs: no smaller than a `u32`.
- **The arrival entry is unchanged and its time stays 16 bits.** No bit is taken from `arrival_time_ms_lo16`. The arrival unwrapper (`bwe/timeline.rs`) assumes a 16-bit period. It is correct only while consecutive samples are less than 32 s apart. A 15-bit field would halve that to 16 s, which is shorter than the ACK silence of an idle static desktop.
- **Size cost, against revision 4's three datagram copies** **[E]**:
  - a RECOVERED id costs 4 B, once. Each write adds the 3 B record header and a QUIC STREAM frame header (type, stream id, offset and length varints, ~6-10 B);
  - at 1080p after bundling (~6k bundles/s), 1 % loss and ~90 % recovered, that is ~55 recoveries/s and at most 55 writes/s of one or two ids: ~0.8 KB/s. Revision 4 cost ~0.7 KB/s (12 B per id). Arrival entries are ~36 KB/s;
  - STREAM frames share QUIC packets with the ACK datagrams when both are pending, so the stream mostly adds frames, not packets. Revision 4's repeat-only flushes, up to two extra small datagrams after the last recovery, are gone;
  - the byte count is a wash. **The gain is certainty:** revision 4 lost a RECOVERED whenever three consecutive batches were lost, and that cost a content retransmit; here a RECOVERED is not lost while the connection lives. It also deletes the per-id copy counter, the 48-id cap and the repeat flush;
  - repair symbols add their own arrival entries: +20 % entries at r/k = 0.2. They are priced like any other transmission, because they are one.
- **Latency on uplink loss.** The sender's RFC 9002 recovery retransmits a lost stream packet (quinn natively, the browser's QUIC stack on the web). It does so by packet threshold once 3 later packets are acknowledged, which the continuous ACK datagrams provide within a few flushes, by time threshold at 9/8 RTT, or at the PTO. A lost RECOVERED therefore arrives about 1 to 1.5 RTT late, where revision 4's next copy came 5 ms later. The effect is that the content stays cached and B's deferral stays open one RTT longer. Nothing is retransmitted, because `T_def` is 60 s. A DECLINED delayed by an RTT delays a rare retransmit by an RTT. That is the trade the owner chose: bandwidth and certainty, for a few milliseconds of cache lifetime.
- **Head-of-line blocking within the stream.** One lost packet holds back the records written after it. All of them are content releases and repair-phase outcomes, and none is latency-critical once `T_def` is a backstop, so this is acceptable. REPAIR_REQUEST stays a datagram for this reason: it is deadline-gated (`now + RTT + A ≤ deadline`, §4.8.2), a lost one is simply sent again, smaller, and HOL blocking would push it past the deadline.
- **Congestion control.** The stream shares the client connection's congestion window with the ACK datagrams. At ~1 KB/s it is negligible. Flow control does not bind, but the reason is not the volume: stream credit is returned only as the server application reads. The server reads the stream on every readable event (`WebTransportSession::on_stream_readable` drains everything available) and dispatches it in the same event-loop pass as the feedback stream (`io_bridge.rs`, `drain_feedback`). If that ever changes, the window can fill whatever the volume.
- **RECOVERED is still congestion-inert.** The channel changes nothing in rule 3. The server's record handlers call content release, the deferral and the retransmit request, and nothing else. QUIC's own acknowledgements of the stream's packets belong to quinn and never reach the application's estimator.
- **Ordering across the two channels.** The stream and the datagrams are not ordered with respect to each other. A source's late original can be acknowledged on a datagram before or after its RECOVERED arrives on the stream, and held(B) can reach k before or after a RECOVERED or DECLINED for B arrives. The server handling is defined so that every order gives the same final state (§4.9.3):
  - content release is idempotent: the first of {arrival entry, RECOVERED} releases, and the second is a no-op;
  - RECOVERED never resolves a ledger record and never touches congestion state. Whether a late original's ACK is Live or a spurious verdict therefore depends only on the detector, never on when RECOVERED came;
  - B's **accounted set** (sources with an arrival entry or a RECOVERED id) grows by union. The deferral covers only the sources still unaccounted when held(B) reaches k, and closes when the set covers them all;
  - DECLINED retransmits the named sources that are not yet accounted, and closes B for good, so arrivals that land after it cannot open a deferral (§4.9.5, "Block state"). A later arrival or RECOVERED of a named source makes that retransmit redundant (counted, safe), never wrong;
  - CONTENT_REJECTED is handled without the cache entry, because the bundle's arrival entry may already have released it (§4.9.3).

  Within the stream, order is kept. For a rebuilt bundle with a rejected record, the client writes CONTENT_REJECTED before RECOVERED, so the server never releases a record it is about to be told is bad.
- **When the client emits each record.**
  - **RECOVERED:** for tile bundles only, at the point where the rebuilt bundle enters the record path an arrived bundle takes. A RECOVERED id means, for content, exactly what the source's arrival entry means.
  - **CONTENT_REJECTED:** where today's code withholds the deferred ACK, that is, when a record fails prevalidation, whether its bundle arrived or was rebuilt.
  - **DECLINED:** when the client holds at least k symbols of B and does not produce its sources. It gives one of the reasons of §4.9.5 and the missing sources. One DECLINED per block; the client does not repeat it, because the stream is reliable.
- **Discriminator.** ACK v4 keeps 0x08, although its layout is now 0x06's. What changed is the meaning of an entry's id: a `bundle_seq` from the bundle counter, with repair symbols acknowledged and rebuilt sources absent, against 0x06's `wire_seq`. Both namespaces are u32 counters that start near zero. An entry read in the wrong one is therefore a valid-looking id that resolves the wrong ledger record, silently. Both namespaces are live in one session around the switch: 0x06 batches still in flight for pre-bundle traffic arrive after the first 0x08. The type byte is what tells the server which namespace a batch belongs to, and a stale binary that sends the wrong one fails loudly (`WrongMsgType`, counted) instead of mis-resolving. That is the project's convention (`ghostframe-protocol/src/ack.rs`): bump the discriminator when the meaning changes. It costs one constant, and the codec is shared with 0x06.
  - Register 0x08 in `INBOUND_DISCRIMINATORS` and `classify_inbound` (`ghostframe-protocol/src/protocol.rs`).
  - 0x06 remains the ACK until the client receives its first FEC-v2 bundle, and for every session that never receives one. At D3-f it joins the `AckBatchV1` arm.
  - The message-type constants are `pub const`, so `ghostframe-lib/include/ghostframe.h` must be regenerated (the cbindgen gate). The same applies to any new cap constant, unless it is `pub(crate)`.

#### 4.9.3 Server handling of each report

| report (channel) | ledger state of the `bundle_seq` | action |
|---|---|---|
| arrival, source (datagram) | Live | as today: release the content; record it delivered in `loss_window`; take an RTT sample, a goog_cc arrival and a delivery-rate sample; advance `largest_acked`; count toward held(B); add to B's accounted set |
| arrival, repair (datagram) | Live | the same, but no content to release and nothing to account |
| arrival, either (datagram) | declared lost | the existing spurious-verdict path (retract, late delivery sample, `K_PKT` evidence); release the content if it is a source; count toward held(B) |
| arrival, either (datagram) | already acknowledged, or unknown | nothing (overlap repeat, or past retention) |
| RECOVERED (stream) | Live | release the content; add to B's accounted set; close the deferral if this was its last unaccounted source; **the record stays outstanding**, and the detector declares it lost unless its original arrives |
| RECOVERED (stream) | declared lost | release the content; add to B's accounted set; close the deferral and cancel any queued content retransmit |
| RECOVERED (stream) | already acknowledged | nothing: the original arrived first |
| RECOVERED (stream) | unknown, or names a repair record | nothing; `recovered_unknown += 1` |
| DECLINED, tile (stream) | B `Open` or `Deferred` | for each source in `missing` that is not in B's accounted set: a content retransmit now, in an r = 0 bundle under a new `bundle_seq`, or the tile re-encoded fresh if it was superseded. B moves to `Closed(Declined)` (§4.9.5, "Block state"), which closes a deferral if one was open (`deferral_declined`), cancels `T_lr`, drops B's repair state and stops any later deferral from opening |
| DECLINED, tile (stream) | B `Closed` | nothing; `block_closed_late_trigger += 1` |
| DECLINED, H.264 (stream) | any | the frame is lost: `h264_declined += 1`, and C5's keyframe or reference-invalidation request fires now instead of at the deadline. Drop the block state. Nothing is retransmitted |
| CONTENT_REJECTED (stream) | any, before or after the bundle's arrival entry | the named record is not delivered. The server **bumps the tile's generation and re-encodes it fresh** from the current framebuffer (the supersession path), in an r = 0 bundle. The bump drops the old generation's coverage (`drop_for_tile` on `bump_generation`), including any coverage the bundle's arrival entry already advanced and any PixelPerfect transition it caused. The final content state therefore does not depend on whether the arrival came first. A rejection whose record's generation is no longer the tile's current one is a no-op (counted), which makes a repeated, resent or late rejection idempotent. Re-sending the cached bytes instead would leave the arrival's coverage advance in place in one order and not the other, and might fail prevalidation again. Congestion state is untouched: the arrival entry already said the network delivered the datagram. The bundle's other records are released as usual |

**Retention (revision 5).** A RECOVERED or DECLINED can be delayed by one uplink stream retransmission, ~1-1.5 RTT, and on the client-bug path a deferral lasts `T_def` = 60 s. Retaining every protected record for 60 s would mean ~360 k records at 6 k bundles/s. Instead:
- every protected record, tombstones included, is retained for at least `max(today's floor of 600 ms, T_lr(B), deadline(B))`. A RECOVERED that needs one stream retransmission arrives about one-way + flush + ~2 RTT (detection and retransmit) after the rebuild, so the floor covers RTTs up to ~240 ms **[E]**. A RECOVERED later than that finds nothing (`recovered_unknown`), and its content is handed to ARQ at the deadline: a wasted retransmit, never a wrong tile. A RECOVERED reads a tombstone without consuming it (§4.9.1 rule 3);
- a source record in an **open deferral is pinned**: it is exempt from retention expiry and from capacity eviction until the deferral closes (RECOVERED, DECLINED or `T_def`) or the session resets. Repair records are never pinned. Normally the pinned set is a few tens of records (recoveries per second times one uplink delivery). On the client-bug path it is recoveries per second times 60 s, ~3 k at 1080p and 1 % loss **[E]**. `deferral_pinned_max` reports the high-water mark. A cap (`MAX_DEFERRAL_PINNED`, `pub(crate)`, 16 384 suggested) resolves the oldest deferral early by its `T_def` action, a content retransmit and never a release (`deferral_pinned_overflow`), so a buggy client cannot grow server memory without bound;
- **lossless content is retained until it is delivered**: by an arrival entry, by a RECOVERED, or by supersession with a newer encode of the same tile, which is itself retained until delivered. No path releases tile content as lost without a replacement transmission on its way. This is the "fully lossless screen" goal stated as an invariant, and test 12 checks it.

#### 4.9.4 What a rebuilt source costs each consumer

| consumer | rebuilt source | its repair symbols |
|---|---|---|
| RFC 9002 detector, `K_PKT` | a loss, by threshold, like any other | arrivals (they advance `largest_acked` and count toward the packet threshold) |
| srtt / rttvar / `min_rtt` | no sample | samples, with real arrival times |
| goog_cc | lost (`pending_losses`) | received packets with their sizes and arrival times |
| delivery-rate sampler (PR #239) | not delivered | delivered |
| content (cache, coverage, retransmission) | released by RECOVERED | none |

What goog_cc reads is pre-FEC loss. With FEC on, it reads more loss than today's post-FEC figure. That is intended (RFC 9265), not a regression, and a regression gate must not be tuned to hide it.

#### 4.9.5 Reconstruction-first deferral on the server

Repair symbols are now ACKed, so the server can count from arrival entries alone how many symbols of block B the client holds: `held(B)`, the number of distinct ACKed `bundle_seq`s among B's sources and repairs. The count is exact up to ACK loss, which the overlap repeat mostly covers. It is one counter per open block, updated in O(1) per ACK from the `(block_id, k)` on each record, next to B's accounted set (§4.9.2, "Ordering").

- **held(B) < k.** Nothing new. The client's recovery window and REPAIR_REQUEST (§4.8.2) handle it, and `T_lr` remains the last resort.
- **held(B) ≥ k, tile classes** (CDF 5/3, PalRLE, Solid and every other tile codec: everything except H.264). The client can decode B. Under reconstruction-first it decodes as soon as the k-th symbol arrives. A lost source of B is therefore **not retransmitted when the detector declares it lost**. `T_lr` is cancelled (§4.8.3), and the source's retransmission is **deferred** until one of three things happens:
  - RECOVERED names it: the content is released, and the deferral closes once every lost source is accounted for (`deferral_released`);
  - DECLINED names B: the content is retransmitted at once (`deferral_declined`, §4.9.3);
  - the backstop expires at

    `T_def(B) = t_k + T_DEF_LOSSLESS`,   `T_DEF_LOSSLESS = 60 s`

    where `t_k` is the server time at which the ACK that brought held(B) to k arrived. `T_DEF_LOSSLESS` is a named `pub(crate)` constant in the ledger, so it stays out of `ghostframe.h`.

  **Why 60 s and no RTT formula** (owner decision 2026-10-26, §6.4 item 14(a): "pretty much forever, as we want to get to a fully lossless screen; maybe 1 min").
  - Revision 4's `T_def = t_k + 2·max_ack_delay + max(srtt, 4·rttvar, ack_delay_p99)` was a latency mechanism. It guessed when a RECOVERED that had not come would never come, because three lost datagram copies and a declined rebuild both looked like a slow RECOVERED.
  - Revision 5 removes both reasons. RECOVERED is reliable, so a slow one is never a lost one. A declined rebuild says so, with DECLINED. Every terminal outcome of a block the client knows about is now a reliable client statement, and the server has nothing left to guess.
  - What remains for `T_def` is a client that says nothing at all while the connection lives: a client bug (a rebuild that emits neither RECOVERED nor DECLINED, or a client that does not reopen the stream after a STOP_SENDING), or a wedged repair-status stream (a front end that stopped writing it).
  - A dead client is not that case. quinn closes the connection after its idle timeout, the default 30 s, since neither side sets `max_idle_timeout` (`transport/quic.rs`, `ghostframe-client-net/src/lib.rs`). `reset_session` then clears the deferral before 60 s.
  - So `T_def` is a backstop that keeps a bug from stranding a tile forever, and **`deferral_timeout` is an alarm**: every test and scene asserts it is 0 (§4.9.8 tests 9b and 10). 60 s rather than "forever", so that the screen still converges on a buggy client. Not shorter, because a fire on a slow, correct client wastes a retransmit, and a short value would invite tuning it as a latency knob again.
  - The `ack_delay_p99` ring that revision 4 needed for the slack is dropped.
- **At `T_def` with neither RECOVERED nor DECLINED (`deferral_timeout`).** **The server never releases content on this deduction.** The client can decline a rebuild for reasons the server cannot predict, and it now names them in DECLINED. Today's XOR decoder declines in four cases:
  - a group that the evicted-group guard (`max_evicted`) rejects;
  - a member that is not a tile datagram;
  - a length outside the source-length bound;
  - pending state that was pruned.

  The RS decoder will have the same kinds of reason, which are DECLINED's reason codes: block state pruned, evicted, a symbol that fails validation, a length bound, a rebuilt header that does not match its hole. Releasing content on a deduction would leave a tile permanently wrong with no repair, which breaks PixelPerfect. **Content release stays a client statement.** The fallback is a **content retransmit**: the source's records are re-bundled under a new `bundle_seq`, or the tile is re-encoded fresh if it was superseded. It is not more repair symbols: a client that held k symbols and did not decode would not decode with k + 1. `T_def` also logs at warn, rate-limited, because it means a bug.
- **H.264: excluded** (owner, 2026-10-26). H.264 is not ARQ-lossless. No fragment is ever retransmitted (§4.3 step 6), and a late frame is worthless to a decoder that needs its references in order. A content retransmit at `T_def` would have nothing to do. So H.264 gets **no deferral and no `T_def`**, neither revision 4's short formula nor the 60 s one: the short formula existed only to bound the latency of a content retransmit, and H.264 has none. Its rules stay what revision 3 gave it:
  - reconstruction, then REPAIR_REQUEST while its deadline (1-2 frame intervals) allows;
  - at the deadline, the frame is dropped and counted (`h264_frames_lost`), and C5's keyframe or reference-invalidation request fires;
  - DECLINED for an H.264 block (class 1, `block_id` = `frame_seq`) declares the frame lost at once, so C5's request leaves without waiting for the deadline.

  The server has no held(B) and no RECOVERED for H.264. H.264 fragments have no `bundle_seq` and are not ACKed on master: client reassembly ACKs tile datagrams only. FEC v2 does not make them ledger transmissions; H.264 keeps per-frame blocks keyed by `frame_seq` and `frag_idx` (§4.1, §4.5). The client sends no RECOVERED for a rebuilt H.264 fragment. It counts it in LOSS_REPORT (`h264_frags_recovered`). If H.264 fragments ever become ACKed transmissions, their id namespace under 0x08 has to be defined first.
- **A verdict before held(B) ≥ k is known** affects congestion state only. The deferral starts when held(B) reaches k. If B's deadline passes first, its repair phase ends and the content falls back to ARQ (§4.8.2 step 4).
- **When the server's view is wrong (revision 5).** Revision 4 listed three cases that left the server with no trigger. Two of them now resolve by a reliable client statement:
  - held(B) undercounted (arrival entries lost beyond the 8-entry overlap) and the client rebuilt B: its RECOVERED arrives and releases the content. No deferral was needed;
  - held(B) undercounted and the client declined: its DECLINED arrives, and the content is retransmitted at once.

  The case that remains is a client that cannot speak about B. Either it never received a symbol of B (a burst took the whole block), or it holds fewer than k and its REPAIR_REQUESTs were lost or no round fits. The server then knows held(B) < k. `T_lr` sends one repair symbol and re-arms once; if the client now knows B and asks, REPAIR_REQUEST serves it. Otherwise the **deadline release** ends B's repair phase and hands the content to ARQ (§4.8.2 step 4). This is why the deadline release is a precondition of switching any class to the protected rules (§6.4 item 14(d), decided). Without it, these cases strand content. Test 12 (§4.9.8) covers them.

  One waste remains possible. If the client rebuilt B and its RECOVERED is slower than the deadline (one uplink stream retransmission at an RTT near the tile budget), the deadline hand-off re-sends content the client already holds. That is safe, because RECOVERED and the re-sent content are both idempotent **[E]**.

**Block state (revision 5).** Each tile block B on the server is in exactly one state:
- `Open`: the repair phase, held(B) < k as far as the server knows;
- `Deferred`: held(B) ≥ k, waiting for the client's statement;
- `Closed(reason)`: terminal. The reason is `Accounted` (every source has an arrival entry or a RECOVERED id), `Declined` (DECLINED, or a REPAIR_REQUEST with `have ≥ k`), `HandedOff` (the deadline release) or `TimedOut` (`T_def`).

Transitions:
- `Open → Deferred` when held(B) reaches k;
- `Deferred → Open` on a REPAIR_REQUEST with `have < k` (`have_mismatch`): the client's statement wins and the repair phase resumes;
- `Open → Closed(Accounted | Declined | HandedOff)`;
- `Deferred → Closed(Accounted | Declined | TimedOut)`.

Rules:
- **An open deferral suspends the deadline release.** In `Deferred` the client holds ≥ k symbols, and its RECOVERED or DECLINED is on its way reliably. At 75 ms RTT and a 120 ms tile budget, one uplink stream retransmission already puts a RECOVERED past the deadline. A hand-off there would re-send content the client holds. The deadline therefore acts only in `Open`. A deferred block is closed by the client's statement, or at worst by `T_def`.
- **`Closed` is terminal.** Later ACKs of B's symbols still count toward held(B), still release content and still feed congestion state as usual, but they never open a deferral. A later DECLINED, `T_lr`, deadline or REPAIR_REQUEST for B is a no-op, counted as `block_closed_late_trigger`. A later RECOVERED still releases content, idempotently.

The first terminal trigger closes B, and its content action is the only one taken. That is what makes the cross-channel orders converge:
- **DECLINED before held(B) reaches k** (the DECLINED on the stream overtakes the arrival datagrams): B goes `Open → Closed(Declined)` and the sources in `missing` are retransmitted. When the arrivals land, held(B) reaches k on a closed block, so no deferral opens and `T_def` is never armed. In the other order, `Deferred → Closed(Declined)` retransmits the same sources: the mask names sources the client never had, and the late arrivals are of symbols it did have. The only difference arises when the late original of a masked source arrives after DECLINED. Its retransmit is then redundant: counted, and safe.
- **A hand-off, then late ACKs that bring held(B) to k:** B is already `Closed(HandedOff)`, so no deferral opens. A RECOVERED that follows releases the content, and the hand-off's retransmit then finds its content released (`ContentGone`) if it is lost.

**What changes in §4.8, and what stays.**
- **Stays:**
  - the client-side recovery window `close(B)`;
  - REPAIR_REQUEST for held < k, answered with m *new* repair symbols (each one now a transmission with its own `bundle_seq`), still a datagram;
  - the deadline gating;
  - `T_lr` and the rejection of every FEC-blind sender timer;
  - the RTT-dependent adaptation (§4.8.4).
- **Changes:**
  - **A server-side trigger for held ≥ k.** REPAIR_REQUEST cannot cover this case, because a client that holds k symbols never asks. RECOVERED, DECLINED and the `T_def` backstop cover it.
  - **Block state** is explicit and terminal (above).
  - **Cancelling `T_lr`** now reads "every source of B has an arrival entry or a RECOVERED id, or B has a DECLINED, or B's deferral has started" (§4.8.3). Revision 3 said "an ACK covering B's sources".
  - **Loss verdicts on protected traffic** come from D1's RFC 9002 detector. They feed congestion state and never schedule a protected retransmission by themselves (§4.8.3, revision 4 reconciliation).
  - **The deadline release of tile content** ends the repair phase and hands the content to ARQ, instead of releasing it as lost (revision 5, §4.8.2 step 4).
  - **Cross-checking REPAIR_REQUEST.** The request's `have` field can be checked against held(B).
    - The informative case is **held(B) ≥ k with `have` < k**: the client acknowledged symbols it no longer holds, for example because it pruned or invalidated them after the ACK. The client's statement is authoritative. The server serves the request with m new repair symbols, as asked, moves B from `Deferred` back to `Open` so that no content retransmit doubles the repair, and counts `have_mismatch`. Repair symbols, not a content retransmit, because the client says it still holds `have` symbols, and any m new symbols complete the block, whereas a content retransmit would resend every missing source.
    - A request with `have ≥ k` is malformed: that client should have decoded, or sent DECLINED. It is treated like a DECLINED with every unaccounted source missing: content retransmit and `Closed(Declined)`, counted. The terminal state stops a later held(B) = k from opening a deferral.

#### 4.9.6 Server cleanup in D3-c: delete the XOR-era heuristic

When the ledger moves to `bundle_seq` in D3-c, delete the server's inference of tile XOR recoveries. Every recovery is then stated by the client, and the heuristic is wrong for RS with r ≥ 2 anyway: its "exactly one member missing" test fails as soon as a block can rebuild two.
- `TransmissionLedger::fec_possible`, the `Resolution::Late { fec_possible }` field and the `late_acks_fec_possible` counter. The counter lives in `transmission_ledger.rs` and `instruments.rs`, is carried by `io_bridge/transport_probe.rs`, and is printed on the `loss path` event (`io_bridge/frame.rs`). Tell PR-10's native validation about the field's removal.
- The `parity_departed` / `parity_order` ring, `PARITY_RING_CAP` and `note_parity_departed`, plus their `reset_session` lines.
- `DepartureTracker::tag_last_as_parity` and `take_departed_parity` (`departure.rs`), with the bridge plumbing in `io_bridge/transport_probe.rs` and the `parity_group` hand-off in `io_bridge/emit.rs` that feeds them.
- The `fec_possible` branch of the `Late` arm in `io_bridge/ack_dispatch.rs`, so that one spurious-verdict path is left.

`Emission::parity_group` stays only while the legacy 0x04 queue exists. Since 0x04 emission is off for every session from D3-c on (§6.4 item 14(c), decided), it carries nothing after D3-c, and D3-f deletes it with the queue. Nothing is added in their place, because this design needs no `rebuilt` set. The server change is a net deletion.

#### 4.9.7 Code sites, by crate

**ghostframe-protocol** (D3-a)
- `ack.rs`:
  - the 0x08 constant and an `AckBatchV4` type that shares 0x06's entry codec (arrivals only, 1..=72 entries);
  - the module doc, and the tests.
- `AckEntry` keeps its layout. Its `wire_seq` field names a `bundle_seq` under v4.
- `protocol.rs`: `InboundKind::AckBatchV4`, `INBOUND_DISCRIMINATORS`, `classify_inbound` and their tests.
- `feedback_stream.rs`: the three record types (0x10 RECOVERED, 0x11 CONTENT_REJECTED, 0x12 DECLINED) in `FEEDBACK_STREAM_MSG_TYPES` and the distinctness test; a codec for the length-prefixed records, with the DECLINED reason codes, and an incremental parser that waits for a split record's tail.
- REPAIR (0x0A) header: the own `bundle_seq` field (§4.5); `lens` are whole-datagram source lengths.
- The RS block codec takes whole BUNDLE datagrams as source symbols (§4.9.1 rule 1).
- Regenerate `ghostframe-lib/include/ghostframe.h` for any new `pub const`.

**ghostframe-client-core** (D3-b)
- `ack_batcher.rs`: unchanged except the type byte, which switches to 0x08 on the first FEC-v2 bundle. No RECOVERED queue, copy count or repeat flush.
- A repair-status outbox: queues RECOVERED, CONTENT_REJECTED and DECLINED entries and emits at most one record per type per ACK flush, CONTENT_REJECTED before RECOVERED for the same bundle. It holds records until the front end reports the stream open, keeps the 60 s retained log, and replays it when the stream is reopened (§4.9.2, "Stream lifecycle").
- `event.rs`: `PollOutput::RepairStatus(Vec<u8>)`.
- The unbundle and reassembly path:
  - an arrival entry for every BUNDLE and every REPAIR received, including the original of a source already rebuilt, and including a bundle with a record that fails prevalidation (rule 5);
  - an RS rebuild produces a RECOVERED id, never an arrival entry. The id is read from the rebuilt datagram's header and checked against the hole's `block_id` and `idx_in_block`; a mismatch produces DECLINED `header_mismatch`;
  - a block with ≥ k symbols that is not rebuilt produces one DECLINED with its reason and missing-source mask;
  - the deferred ACK for a CDF53 pass that fails prevalidation (`reassembly.rs`) becomes CONTENT_REJECTED under bundles. The 0x06 path keeps today's behaviour.
- The client-local `Provenance { Received, Rebuilt }` (`reassembly.rs`) is what chooses between the arrival entry and RECOVERED. It stays a client-core type: the wire carries the choice of channel, not the enum. Its other uses are unchanged: the loss tracker's timing, `wire_loss`, and the newest-label yardstick.
- Tests: `tests/oracle_ack.rs`, `tests/reassembly.rs`, and new outbox tests (§4.9.8).

**ghostframe-client-net** (D3-b)
- `lib.rs`: open the repair-status stream with `open(Dir::Bi)` next to `feedback_stream`, when the first FEC-v2 bundle arrives, retrying on every poll while it returns `None`; route `PollOutput::RepairStatus` to it; on a write error (`Stopped`, reset), reopen and replay.

**ghostframe-client-wasm** (D3-b)
- `boundary.rs`: `pollTransmit` reports `kind: 'RepairStatus'`. `WasmAckEntry` is unchanged.
- `units.rs`: `parse_ack_envelope` reads v4.
- `constants.rs`: export 0x08 and the three record types.

**ghostframe-web-client** (D3-b)
- `main.ts`: a second `transport.createBidirectionalStream()`, opened on the first `RepairStatus` output, and its writer; outputs produced before it resolves wait in a FIFO; a rejected write reopens the stream and replays the log. `drainTransmit` writes to it without awaiting, as it writes `ackWriter`, so a stalled stream cannot hold back the ACK datagrams.
- Nothing in `ghostframe-web-client/src` parses ACKs or calls `WasmAckBatcher` today. Confirm that by grep at implementation time. The `just build-release` and `just containers-build` traps in AGENTS.md apply, because the bundle is embedded.

**ghostframe-lib, server** (D3-c, deferral in D3-d)
- `transport/webtransport.rs`: `feedback_queue` carries the `StreamId` with each chunk; a stream that ends or is reset drops its buffer; `STOP_SENDING` for a malformed stream.
- The per-stream WebTransport header strip and session-id check (§4.9.2): on master for the feedback stream (the separate HELLO fix), extended in D3 to the repair-status stream.
- `io_bridge.rs`, `dispatch_feedback_bytes`: the accumulator `feedback_recv_bufs` is keyed by `(handle, StreamId)`; the three record types are dispatched to a new handler module (for example `io_bridge/repair_status.rs`) that calls only content release, the accounted set, the deferral and `request_retransmit`. It must not reach `record_ack_bwe_samples`, `loss_window` or the RTT estimator. HELLO-gating applies as for every other feedback message.
- `io_bridge/ack_dispatch.rs`: decode v4; the arrival loop as today, now resolving repair records too and feeding held(B) and the accounted set. (Revision 4's "skip `delivery_sampler.end_ack` when `n_arr == 0`" is gone: a 0x08 batch always has arrivals.)
- `transport/transmission_ledger.rs`:
  - records for repair symbols (no content, with `(block_id, k)`), per-block held counters and accounted sets;
  - the per-block state (`Open`, `Deferred`, `Closed(reason)`), `T_DEF_LOSSLESS` and its wake time alongside `next_loss_time`; pinning of records in an open deferral and `MAX_DEFERRAL_PINNED`;
  - a `block: Option<BlockId>` on each record, which is what `classify_retx` reads;
  - a non-consuming tombstone lookup for RECOVERED (`resolve_at` consumes);
  - retention of at least `max(600 ms, T_lr, deadline)`;
  - `reset_session` clears all of it;
  - the deletions of §4.9.6.
- `reliable_emitter/` and `departure.rs`:
  - repair symbols allocate a `bundle_seq`, are recorded at departure and are billed;
  - goog_cc's sent-packet history includes them.
- `transport/loss_path.rs` and `reliable_emitter/emitter.rs` (D3-d):
  - `classify_retx` and `request_retransmit` return a new `RetxRequest::Protected` under `RetxCause::Loss` and `RetxCause::Probe` when the **transmission** (its ledger record) is a member of an FEC block, never on the content key alone (§4.8.3); That covers `loss_path::detect`'s per-verdict retransmit and `take_due_deferred`;
  - `loss_path::probe` answers a protected candidate with one new repair symbol of its block, and skips a block in deferral;
  - the TILE_NACK path refuses protected content;
  - DECLINED, CONTENT_REJECTED (a generation bump and re-encode), the `T_def` backstop and the deadline hand-off are the callers that request a content retransmit of protected content. Each sends it in an r = 0 bundle.
- The deadline release for every protected class (§4.9.5): for tiles, the hand-off to ARQ; for H.264, the frame drop.
- `io_bridge.rs` event loop: wake for the earliest `T_def`.
- Instruments: the counters of §4.8.5.
- `io_bridge/tests.rs` and `coverage_pin_tests.rs`: the `AckEntry` literals are mechanical edits.

**ghostframe-e2e**
- `loss_detector_replay.rs`: model the client's rebuild, and ACK the late original as an arrival.
- The browserless harness: the repair-status stream must exist in the harness too, and its c2s packets must be subject to netsim loss, or test 10's uplink-loss premise is never reached.
- The browserless FECORACLE becomes an equality (§4.9.8 test 10).

#### 4.9.8 Test plan

Each test asserts a property and names the mutation that must make it fail. Before trusting a green run, apply the mutation and record the failing output (AGENTS.md). Browserless numbers use at least 3 seeds, and 5 for gates on chaotic scenes.

| # | tier | property | mutation that must fail it |
|---|---|---|---|
| 1 | protocol proptest | 0x08 round trip for every `n` in 1..=72 with arbitrary entries; rejects `n = 0`, a short entry and trailing bytes. Repair-status records: round trip for each type and every entry count up to the 1020 B cap; rejects `len = 0`, a `len` that is not a multiple of the entry size, `len > 1020` and an unknown type; a record fed in arbitrary splits (byte by byte included) decodes to the same entries | the decoder accepts trailing bytes; the record parser accepts a `len` that is not a multiple of the entry size; the parser consumes a partial record instead of waiting for its tail |
| 2 | protocol | 0x08 classifies as `AckBatchV4` in the every-envelope-byte test, and is distinct from every other inbound discriminator. 0x10-0x12 are in `FEEDBACK_STREAM_MSG_TYPES`, distinct from every stream type and from every datagram discriminator | delete the `classify_inbound` arm; give RECOVERED the value of an existing stream type |
| 3 | client-core | for a scripted symbol sequence in each codec family: a source rebuilt by RS appears in a RECOVERED record and never in arrivals; every received source **and every received repair symbol** appears in arrivals; the late original of a rebuilt source produces an arrival entry | (a) put the rebuilt source in arrivals; (b) deduplicate the original's ACK; (c) skip the ACK of repair symbols; (d) derive the RECOVERED id as `first + idx`, with repair symbols of another block interleaved between the sources (the reported id names the wrong transmission) |
| 4 | client-core outbox | each RECOVERED id is emitted exactly once, as `PollOutput::RepairStatus`, never as a `Datagram` and never as `Stream`, within one `FLUSH_INTERVAL_US` of the rebuild; a block with ≥ k symbols whose rebuild fails (one scripted case per reason) emits exactly one DECLINED with that reason and the right missing mask; for a rebuilt bundle with a bad record, CONTENT_REJECTED precedes RECOVERED in the output order | emit RECOVERED twice; route it as `Stream` (the feedback stream) or as a datagram; emit DECLINED with an empty mask; swap the CONTENT_REJECTED/RECOVERED order |
| 5 | server unit | **RECOVERED is congestion-inert**: after delivering only RECOVERED records through the stream dispatcher, the following are identical to their values before: srtt, rttvar, `min_rtt`, `latest_rtt`, `loss_window`, `pending_losses`, `largest_acked`, the `K_PKT` evidence, the delivery-rate sampler state and goog_cc's feedback queue. The content was released (cache entry gone). Premise check: the content-release counter moved | route RECOVERED ids through the arrival resolver |
| 6a | server unit | a source named only by RECOVERED is declared lost by the detector, by packet threshold (`K_PKT` later arrivals) and, in a second case, by time threshold, with `LossReason` set accordingly | let RECOVERED resolve the record as acknowledged (no verdict follows) |
| 6b | server unit | **a verdict on protected data never retransmits.** The content is still cached (no RECOVERED) and held(B) < k. Loss verdicts, `take_due_deferred` and PTO fires queue no source retransmission (`RetxRequest::Protected`), and a PTO fire queues one repair symbol instead. Premise check: verdicts > 0, PTO fires > 0, content cached throughout. (Test 6a alone is vacuous here: once the content is released, today's code also answers `ContentGone`) | keep `loss_path::detect`'s retransmit for protected keys; let the probe resend a protected source |
| 7 | server unit | late original after RECOVERED and a verdict: the spurious-verdict path runs (retraction +1, `K_PKT` evidence +1, one late delivery sample). Late original before a verdict: a Live delivery. A repeat of either: nothing | add a "content already released, skip" special case to the arrival path (the retraction is lost) |
| 7b | server unit, **both cross-channel orders** | for one rebuilt source whose original also arrives, run the same script twice: (i) the arrival datagram, then the RECOVERED record; (ii) the RECOVERED record, then the arrival datagram; each with and without a loss verdict in between. The final congestion state (srtt, `loss_window`, `K_PKT` evidence, sampler, goog_cc queue) and content state (cache, coverage, accounted set, block state) are equal across the two orders, and content was released exactly once. A third script delivers RECOVERED before any other symbol of B is ACKed: when held(B) later reaches k, no deferral opens for that source. **A fourth pair delivers DECLINED (iii) before and (iv) after the arrivals that bring held(B) to k**: in both, the same sources are retransmitted once, B ends `Closed(Declined)`, `deferral_timeout == 0` and no `T_def` is armed after the close. Premise check for (iii): held(B) reached k after the DECLINED | let RECOVERED resolve the record (order (ii) loses the spurious-verdict retraction and the states differ); count a RECOVERED that finds the record acknowledged as a release (released twice); do not consult the accounted set when the deferral opens (the third script opens one); let held(B) = k open a deferral on a `Closed` block (order (iii) arms `T_def` and retransmits twice) |
| 8 | server unit + browserless | a repair ACK yields an RTT sample, a goog_cc arrival and a delivery sample carrying its bytes. In a lossless scene with FEC on, delivered bytes equal the bytes netsim carried, to the datagram (no payload factor) | do not record repair symbols in the ledger (their ACKs resolve unknown, so the equality fails) |
| 9 | server unit | deferral: with held(B) ≥ k and a source lost, nothing is retransmitted before RECOVERED, DECLINED or `T_def`; RECOVERED closes the deferral; **DECLINED triggers the content retransmit in the same event-loop pass**, long before `T_def`, and only for the sources in its mask that are not accounted; content is **never released without RECOVERED**. Premise check: `deferral_started > 0`, `deferral_declined > 0` | (a) release content at `T_def` on deduction (the declined-rebuild case keeps a wrong tile); (b) retransmit on the verdict (the no-retransmit-before assertion fails); (c) ignore DECLINED (the retransmit waits for `T_def` and the same-pass assertion fails); (d) retransmit every source of B on DECLINED (an accounted source is re-sent) |
| 9b | server unit, scripted clock | **`T_def` fires only when the client is silent.** (i) A correct client whose RECOVERED and DECLINED records are delayed by up to 4 RTT (scripted stream retransmissions): `deferral_timeout == 0` and every deferral closes by a client record. (ii) A silent client that rebuilds and sends nothing: `T_def` fires once per block at exactly `t_k + T_DEF_LOSSLESS`, the content is retransmitted, `deferral_timeout` counts it and the warn line is emitted. (iii) A session reset at 30 s clears the deferral and `T_def` never fires. Premise check: in (i) the records really were delayed past revision 4's formula (> `t_k + 2·max_ack_delay + srtt`) | put `T_def` back to revision 4's formula (case (i) fires); never arm `T_def` (case (ii) never retransmits); leave deferrals across `reset_session` (case (iii) fires) |
| 9c | server unit | **H.264 is excluded**: H.264 symbols create no ledger record, no held(B) and no block state, so no deferral and no `T_def` exist for them; a DECLINED for an H.264 block counts `h264_declined`, fires the C5 hook and queues no content retransmit; the client emits no RECOVERED for a rebuilt H.264 fragment | apply the tile deferral to H.264 frames; answer an H.264 DECLINED with a content retransmit; emit RECOVERED for an H.264 rebuild |
| 9d | server unit, scripted clock, 75 ms RTT, 120 ms tile budget | **an open deferral suspends the deadline release**: held(B) reaches k, then the RECOVERED is delayed by one scripted stream retransmission past `deadline(B)`. No hand-off happens, no content is retransmitted, and B closes `Accounted` when the RECOVERED lands. Second script: B is handed off at its deadline while `Open`, then late arrival entries bring held(B) to k. No deferral opens, `T_def` is never armed and `block_closed_late_trigger` counts the late transition. Premise checks: the RECOVERED really arrived after `deadline(B)`; held(B) really reached k after the hand-off | let the deadline release act in `Deferred` (a retransmit appears in the first script); let held(B) = k open a deferral on a `Closed` block (the second script arms `T_def`) |
| 9e | server unit | **protection is per transmission**: a hand-off (or DECLINED) retransmit is itself lost. The loss verdict retransmits it through ARQ, and the content is delivered by its next arrival. Meanwhile a verdict on the protected original is still refused. Premise check: the retransmit's record has no block, and its verdict fired | key `RetxRequest::Protected` on the content key (the lost retransmit is refused, and the content is never delivered) |
| 10 | browserless FECORACLE (burst_loss sweep), **with and without c2s loss** | in every run, server `recovered_ids_rx_distinct` (distinct RECOVERED ids received, **whatever the ledger state**) equals client `bundles_recovered`, exactly, **also with uplink loss**, because the stream retransmits a lost record. `repair_after_recovery == 0`; `deferral_timeout == 0` in every run. Premises, stated and checked: recoveries > 0 in every run (iid 2 %); in the c2s-loss runs, uplink packets were lost (netsim's c2s drop count > 0 and the client's `PathStats.lost_packets` > 0; quinn exposes no per-stream retransmit count) and at least one RECOVERED reached the server more than one RTT after the ACK batch that completed its block, a lag that only a stream retransmission explains | send RECOVERED on an unreliable datagram (the equality fails in the c2s-loss runs); the client reports rebuilt sources as arrivals (`recovered_rx` drops to 0) |
| 11 | replay (`loss_detector_replay.rs`) + regression scenes | with the client rebuild modelled, every late original after a rebuild is read as a spurious verdict, and `K_PKT` reaches its ceiling under 16-interval reordering with no misread. The D1 regression scenes pass; goog_cc's loss input equals the client's pre-FEC loss within the reorder window, and that is treated as correct | drop the arrival ACK of the late original (the `K_PKT` property fails) |
| 12 | server unit, scripted | **the deadline release backstops a client that cannot speak, and loses nothing.** (i) All symbols of B lost; (ii) held < k with every REPAIR_REQUEST dropped. In both, no deferral starts, `T_lr` fires once, sends one repair symbol and re-arms. At `deadline(B)` the repair phase ends and every unaccounted source of B is re-sent (or its tile re-encoded fresh if superseded) as an ordinary transmission, and that transmission is retained until an arrival entry acknowledges it. Content is never stranded and never released as lost: at the end of the script the tile has been delivered. (iii) held(B) undercounted (arrival entries dropped beyond the overlap) while the client rebuilt B: its RECOVERED releases the content and `deadline_released == 0` for B. Premise checks: `deferral_started == 0` in (i)-(ii), `lr_timer_fired ≥ 1`, the overlap really was exceeded in (iii) | remove the deadline release (the content stays cached past the deadline, never delivered); make the release drop the content without a replacement (the tile is never delivered); let any single arrival for B cancel `T_lr` |
| 13 | client-core | ACK v4 is not sent and the repair-status stream is not opened before the first FEC-v2 bundle arrives, even with HELLO bit 2 set: until then every ACK is 0x06 and no `RepairStatus` output exists | switch to 0x08 on the HELLO bit; open the stream at `SessionReady` |
| 14 | server unit, **both cross-channel orders** | CONTENT_REJECTED for record r of a received bundle, delivered (i) before and (ii) after the bundle's arrival entry, where in (ii) the arrival advances r's tile coverage to PixelPerfect: in both orders the bundle's other records are released, r's tile gets exactly one generation bump and one fresh re-encode, and the **content state is equal across the orders** (tile generation, coverage, PixelPerfect state, cache), as is congestion state, which equals that of an ordinary arrival. A repeated or replayed rejection is a no-op | re-send the cached bytes instead of bumping the generation (order (ii) keeps the coverage advance and the PixelPerfect state, so the states differ); look the record up in the cache and skip when it is gone (order (ii) re-sends nothing); drop the generation check (a replayed rejection bumps twice) |
| 15 | loopback (`loopback_h3`-style, quinn-proto pair) | **stream isolation**: write a RECOVERED record and drive `poll_transmit` until it has left in a packet of its own, then drop exactly that packet. Only then write an INPUT event on the feedback stream and transmit it. The INPUT reaches the server before the RECOVERED retransmit does. Premise checks: the dropped transmit carried the repair-status STREAM frame and the INPUT left in a later transmit (both are asserted from the drop hook's view of transmits, so the two cannot share a packet); the RECOVERED arrives late, by at least one retransmission | write repair-status records on the feedback stream (the input event waits behind the lost record) |
| 16 | server unit | two streams of one connection are parsed separately: a message split across reads on one stream, interleaved with a complete record on the other, decodes to the same messages as either alone | keep `feedback_recv_bufs` keyed by handle only (the interleaved bytes corrupt the split message) |
| 17 | server unit | **WebTransport stream header**: the feedback stream and the repair-status stream, each prefixed by `0x40 0x41 varint(session_id)`, delivered (a) with the header in a read of its own, (b) with the header and the first message in one read, (c) with the header split across two reads, and (d) with no header (native). All four decode the same messages, HELLO included. A header carrying another session id is a malformed stream (STOP_SENDING, `repair_status_malformed`) | do not strip the header (case (b) loses the first message, which is exactly the journal's line); strip without checking the session id (the wrong-id case decodes) |
| 18 | e2e, Chromium (heavy lock) | **browser-opened streams**: a real browser session with tile loss injected opens both streams. The server receives HELLO on the feedback stream and RECOVERED records on the repair-status stream, with `repair_status_malformed == 0`, no "unknown feedback-stream message type" line in the log, and `recovered_ids_rx_distinct > 0` | remove the header strip (the server logs the unknown type, `msg_type=64`, and discards records) |
| 19 | server unit | **pin cap**: a silent client leaves more open deferrals than `MAX_DEFERRAL_PINNED`. The pinned count never exceeds the cap; each overflow resolves the oldest deferral by a content retransmit (`deferral_pinned_overflow`); no deferred content is released without RECOVERED. Premise check: the cap was reached | remove the cap (the pinned count passes it); evict pinned records silently (their content is never retransmitted) |
| 20 | client-core + server unit | **stream lifecycle**: (a) records produced before the stream opens are all emitted, in order, once it opens; (b) the stream is reset mid-session after some records: the client reopens and replays its retained log, and the server's final state equals that of a run without the reset; (c) a malformed record: the server stops the stream, the client reopens and replays, and records written after the bad one are applied; a third malformed stream closes the connection | drop records produced before the open; skip the replay after a reopen (the lost records' blocks end in `T_def`); resynchronise at the next read instead of stopping the stream (the records after the bad one are misparsed) |

#### 4.9.9 Alternatives considered

A separate analysis (2026-10-26, kept outside the repo) compared this design with the alternatives and came to the following conclusions.
- **The rebuilt bit in `arrival_time_ms_lo16`** (revision 1):
  - It costs nothing on the wire, but it halves the arrival-time unwrap period to 16 s, which is shorter than an idle desktop's ACK silence.
  - On its own it does not tell a late original from an overlap repeat. That needs a "rebuilt then received" rule and a time-bounded `rebuilt` set on the server.
- **A trailing rebuilt bitmap** (the analysis's preferred form of the flag) avoids the unwrap hazard but keeps the same second-ACK rule.
- **Keeping or tightening today's `fec_possible` heuristic** cannot separate a rebuilt `Live` ACK from a real one once queueing exceeds the parity lag. Under RS r ≥ 2 it is simply wrong: the model gives ~91 % of losses recovered at 1 % loss, so any guess would dominate the loss signal.
- **Deducing recoveries on the server** ("held ≥ k, so it was rebuilt") is unsafe, because the client can decline a rebuild (§4.9.5).
- **A per-parity group-outcome report** is tied to XOR k = 10 geometry, which FEC v2 replaces.

The analysis judged a separate recovered report to carry the same information as the flag at more cost: a second redundancy design, and a release that races the loss verdict. The owner chose that shape with two changes, which together remove both costs:
- the second redundancy design is quinn's own stream recovery (revision 5), not one of ours;
- loss verdicts on protected data no longer trigger retransmission (§4.8.3), so the race is harmless.

Making repair symbols transmissions is what the flag design lacked. It gives goog_cc and the delivery sampler the bytes the network actually carried, and it gives the server `held(B)`, the basis of the deferral.

**Revision 5 alternatives.**
- **RECOVERED in the ACK datagram, three copies** (revision 4). The same bytes per recovery, but a recovery is lost when three consecutive batches are, and that loss must then be covered by a short `T_def` that wastes a retransmit whenever it guesses wrong. It also needs a copy counter, an id cap and a repeat flush. Replaced on the owner's question "maybe we could rely on QUIC reliable stream for this?".
- **DECLINED as a field of REPAIR_REQUEST.** REPAIR_REQUEST is a datagram that asks for repair symbols, and a lost one is re-sent smaller. DECLINED needs a content retransmit, not symbols, and it must not be lost: a lost DECLINED would wait for the 60 s backstop. It goes on the reliable stream.
- **REPAIR_REQUEST on the stream as well.** Rejected: it is deadline-gated, and a stream retransmission (≈ 1 RTT) or HOL blocking would push it past the deadline it was sent to meet.
- **A DECLINED reason for "short at the deadline"**, so that the client also reports blocks it holds fewer than k symbols of. Not added: the server already knows held(B) < k from arrivals and acts at the deadline, and a client report cannot cover the case that matters, a block of which the client received nothing.
- **Folding CONTENT_REJECTED into DECODE_ERROR (0x04 on the feedback stream).** DECODE_ERROR names a tile, not a transmission, and is rate-limited; it cannot say which record to withhold. Kept separate.

---

## 5. Risks

- **Bundling turns "a pass is one datagram" into "a pass is a record in a bundle"**:
  - ACK identity, ledger and BWE samples change unit, and goog_cc sees ~10x fewer, larger packets. That is closer to what the network does, but it is a D1 retune.
  - Do it after D1 has stable baselines, or as a step inside D1, never alongside it.
- **CPU.** The scalar GF(2^8) kernel is ~1 ns/byte per repair row **[M]**. Encoding at emission time bounds it by the link rate. Without that change (encoding at dispatch, as `feed_group` does today), a 4K first paint could cost milliseconds on the dispatch thread.
- **Wasm size.** About +5 KB gzip for the in-house codec, or +10 KB for `reed-solomon-simd` **[M]**. Not +76/+114 KB.
- **Adaptive-loop oscillation.** Mitigated by pre-FEC input, EWMA, hysteresis and the change-rate property test.
- **Native version skew.** Two code paths (legacy XOR, v2) until the legacy one is retired.
- **Expected gains are model estimates** (§3) until the browserless scene runs post-D1.
- **A wedged repair-status stream is silent** (revision 5). If a front end stops writing the stream, or does not reopen it after the server stops a malformed one, content releases stop and the only symptom is `T_def` firing 60 s later. A third malformed stream closes the connection, so a client stuck in a malformed-record loop fails loudly rather than slowly. `deferral_timeout` and `repair_status_malformed` are therefore alarms, asserted 0 in every test and scene, and logged at warn in production.

---

## 6. Part 4: sequencing

### 6.1 Wave C (now). No wire change.

| PR | owner WS | content | depends on |
|---|---|---|---|
| C3-1 | C3 | client decoder: do not buffer a parity whose group is complete (dup rebuilds 0.7 % → 0); O(k) pending index | — |
| C3-2 | C3 | client-core consumes the existing H.264 K=5/K=2 XOR parity (`frame_assembly.rs:62-66`) | — |
| C3-3 | C3 | tile FEC off by default (recovers ≤ 0.8 %, costs ~15 % of bytes) | C4 (shares emitter files), after C1 |
| **C3-M (new)** | C3 | **measurement only**: client pre-FEC loss and run-length counters from `wire_seq` gaps (logged, plus the existing `datagrams_recovered_fec` populated); server logs quinn `PathStats.{cwnd,lost_packets,sent_packets,congestion_events}`, `quinn_queue_bytes`, `rto_spurious`, `retx_by_cause` on the `cumulative emit` line; client `rx_buffer_drops` (native) | — (touches only counters; coordinate the emitter line with C1/C4) |
| C3-W (new, tiny) | C3 or A6 owner | web client: `congestionControl: "low-latency"` on the WebTransport constructor (from Kyber); check browser support | — |
| C4 | C4 | per-fragment cache and ACK (fixes the wrong-fragment NACK/RTO of §1.3 (f)) | C1 |

**Tile XOR FEC is off by default from now on (revision 4, 2026-10-26).** C3-3 is in progress separately. Once it lands, tile traffic carries no 0x04 parity until D3-d emits RS repair symbols. The consequences:
- The server's `fec_possible` heuristic is exactly right by construction: no parity departs, so it is always false.
- No ACK wire change is needed before D3.
- Wave D1 therefore lands no rebuilt flag. The XOR heuristic code stays, tested and inert, until D3-c deletes it (§4.9.6).

### 6.2 Wave D1. Transport, the actual retransmission fix. Gates every tile FEC PR.

Add to D1's scope (revision 3 also asked for a **per-class timer-policy hook**, `TimerPolicy::{Arq, ProtectedLastResort}`, and a per-class horizon. *Revision 4: D1 landed neither and replaced the horizon with an RFC 9002 detector; D3-d adds the protected rules in `classify_retx` instead, §4.8.3*):
- RTO and ledger clocks measured from wire departure, or from quinn acceptance at the very least;
- an uncensored deadline tracker (include `Late` ACK latencies);
- RTO floor and horizon reconciled so the horizon is not shorter than the deadline;
- a deadline-aware retransmit skip;
- the "recovered = lost for the estimator" input. *Revision 4: no D1 work is left here.* Under ACK v4 (§4.9) a rebuilt source is never ACKed as an arrival, so the RFC 9002 detector D1 built declares it lost, with no special case. The server half (RECOVERED handling, repair symbols in the ledger) lands in D3-c, and the deferral in D3-d. Until then tile XOR FEC is off by default (C3-3), so there is nothing to misread;
- the quinn CC decoupling already planned.

**Exit property for FEC to begin:** `retransmit_attempts / network_lost ≤ 1.2` in the iid-1 % browserless scene with FEC off.

### 6.3 After D1: new stream "D3 fec-v2" (6 PRs, three of them wire-changing)

| PR | content | wire |
|---|---|---|
| D3-a | `ghostframe-protocol`: BUNDLE/REPAIR/LOSS_REPORT/ACK-v4 formats (**ACK v4 = arrivals only, 0x08; repair-status stream records RECOVERED/CONTENT_REJECTED/DECLINED, rev. 5; REPAIR carries its own `bundle_seq`, rev. 4**), GF(2^8) Cauchy codec + proptests, HELLO bit 2 constant; no behaviour | format only |
| D3-b | client-core/wasm/native: unbundle, REPAIR decode, metrics, LOSS_REPORT + ACK v4 emission (**ACK every received repair symbol and every late original; rebuilt sources only in RECOVERED, rev. 4; RECOVERED/DECLINED/CONTENT_REJECTED on a second reliable stream, sent once, rev. 5**), **recovery window + REPAIR_REQUEST (rev. 3)**; set HELLO bit 2 | yes (client) |
| D3-c | server: bundling for bit-2 clients, FEC off; ledger/ACK by `bundle_seq` (**repair records, RECOVERED as content release only, rev. 4; per-stream feedback accumulators and the repair-status record handlers, rev. 5**); **delete the XOR-era `fec_possible` heuristic and parity-departure plumbing (§4.9.6)**; **legacy 0x04 tile parity off for every session (§6.4 item 14(c))**; re-baseline the D1 scenes | yes (server) |
| D3-d | server: block FEC emission at emission time, parity billed **and recorded as transmissions**; fixed (k, r) per class behind a knob; **protected classes switched to the protected retransmit rules (`RetxRequest::Protected`, probe answered with a repair symbol, `T_lr`, deadline release for every protected class as an explicit deliverable gated by test 12, §4.8.3); serve REPAIR_REQUEST with new repair symbols; block cache until deadline (rev. 3); held(B) ≥ k deferral for tile classes, closed by RECOVERED or DECLINED, with the 60 s `T_def` backstop (§4.9.5, rev. 5)** | — |
| D3-e | adaptation controller + offline-generated **RTT- and deadline-dependent** table (§4.8.4) + LOSS_REPORT ingestion; telemetry fields | — |
| D3-f | delete legacy 0x04 tile parity (already off since D3-c) and the dead fragment-parity client code, once old native clients are retired | removal |

**H.264 per-frame RS (C3's PRs 4–6)** reuses D3-a's codec and bit 2, with frame parity `frag_idx = n+1+j`. It does not depend on D1, so it can run right after D3-a, possibly at the end of Wave C. Add the r_min = 2 floor for small frames (Kyber, Moonlight). Pair it with **C5**: the RS residual (`h264_frames_lost` in LOSS_REPORT) should trigger reference invalidation or a keyframe, which today does not exist (GOP 11, no loss-triggered IDR).

### 6.4 Decisions for the owner (with my recommendation)

1. **Do you accept that today's retransmission volume is a transport defect (D1/C4), and gate tile FEC behind D1's exit property?** *Recommend yes.* The data bounds FEC's share at 4–34 %.
2. **Bundling as the fix for coalescing** (a wire change touching all four crates), versus interleaving by the packing factor (also a wire change, fewer side benefits, fragile to quinn's packing). *Recommend bundling.*
3. **Codec.** In-house GF(2^8) Cauchy RS with an XOR row (~5 KB, fast at small k, needs a SIMD kernel later), versus `reed-solomon-simd` (no kernel to maintain, ~0.9 ms wasm decode per lossy block), versus RaptorQ (+114 KB). *Recommend in-house.*
4. **Enable wasm `+simd128`** for the bundle? It speeds the codec, and every evergreen browser supports it. *Recommend deciding with D3-d*, measuring first.
5. **HELLO bit 2 = "FEC v2"** as one bit for bundles, RS, LOSS_REPORT and ACK v4, rather than separate bits. *Recommend one bit*, to keep the compatibility matrix at two states.
6. **Recovered ≠ received for the estimator** (D1 owns it). *Recommend yes*, matching WebRTC. **Decided 2026-10-26** in the shape of §4.9: repair symbols are transmissions, rebuilt sources are not arrivals, and RECOVERED releases content only. It needs no estimator code.
7. **Overhead policy.** Off below 0.3 % pre-FEC loss, cap 25 %, no FEC on refinement passes. *Recommend yes*, tunable.
8. **Native-client skew.** How long to keep the legacy 0x04 path? *Recommend two native releases*, then D3-f.
9. **"Do none of the tile wire work"** remains legitimate. C3-1/2/3 + C4 + D1 capture most of the latency win; D3 buys the last tail and burst resilience.
10. **(new, Kyber) H.264 minimum repair count r_min = 2 for small frames.** *Recommend yes*, folded into the H.264 RS PRs.
11. **(new, revision 3) Reconstruction-first repair policy** (§4.8): no sender timer on FEC-protected data except a last-resort one at window + RTT; the receiver requests m new repair symbols only when the block is unrecoverable and the deadline still allows. *Recommend yes*: the owner's direction, supported by the model.
12. **(new, revision 3) Accept the latency cost under bursts at low RTT** (p95 ~44 ms against ~33 ms for a timer at 20 ms RTT, 2 % GE bursts), in exchange for zero wasted repairs. *Recommend yes*, reducible with larger r where bursts are measured.
13. **(new, revision 3) Repair by new repair symbols, not by re-sent sources.** *Recommend repair symbols* (§4.8.2).
14. **(new, revision 4) Details of the ACK v4 design** (§4.9). **Decided 2026-10-26** (revision 5):
    - **(a) `T_def`.** Owner: "T_def should be pretty much forever, as we want to get to a fully lossless screen (so this doesn't apply to H.264). Maybe 1 min." **Decided:** for the tile classes, `T_def = t_k + T_DEF_LOSSLESS`, 60 s, a backstop and not a latency mechanism. It is safe only because RECOVERED and DECLINED are reliable (b), so it fires only on a client bug, and `deferral_timeout` is an alarm. H.264 has no deferral and no `T_def` (§4.9.5). Revision 4's RTT formula and its `ack_delay_p99` ring are dropped.
    - **(b) RECOVERED redundancy.** Owner: "maybe we could rely on QUIC reliable stream for this?" **Decided:** RECOVERED, CONTENT_REJECTED and the new DECLINED leave the ACK datagram for a reliable client→server stream, each sent once (§4.9.2). The 3-copy repeat and the 48-id cap are removed. ACK v4 (0x08) is arrivals only and keeps its own discriminator. Cost, latency on uplink loss, HOL blocking, cross-channel ordering and congestion control are analysed in §4.9.2.
    - **(c) The legacy 0x04 path after D3-c.** **Decided:** 0x04 tile-parity emission is off for every session from D3-c on. D3-f still deletes the code.
    - **(d) The deadline release for every protected class.** **Decided:** an explicit D3-d deliverable, gated by test 12. Reconciled with (a): for the tile classes the deadline ends the repair phase and hands the content to ARQ; lossless content is retained until delivered (§4.8.2 step 4, §4.9.3 "Retention").
15. **(new, revision 4) What an arrival entry means for content** (§4.9.1 rule 5). **Decided 2026-10-26: option A.** Arrivals report the network; CONTENT_REJECTED (`bundle_seq u32`, `record_idx u8`) reports a bad record. Per 14(b) it rides the reliable repair-status stream rather than an ACK section, so the "processed before arrivals in the same message" rule of revision 4 becomes "handled without the cache entry, in either order" (§4.9.3, test 14). Options B (withhold the arrival) and C (per-record content ACKs) are rejected.
16. **(new, revision 5) Which stream carries the repair-status records** (§4.9.2). **Decided 2026-10-26: a second client-opened bidirectional stream.** Owner: "they have clear intent/purpose difference". Head-of-line blocking against input points the same way, though modestly (~+4 points of input events delayed by an RTT at 75 ms and 1 % uplink loss during heavy recovery **[E]**). It requires the server to keep per-stream buffers and to strip and validate the WebTransport stream header per stream. Master already needs that strip for the feedback stream's HELLO, and it is being fixed there in a separate small PR, which D3 builds on (§4.9.2, tests 17-18).

---

## 7. What I could not establish

- Whether the outgoing queue in quinn or the client's receive-buffer drops dominate the "unreceived" datagrams of §1.2. C3-M's counters decide it.
- Kyber: resolved in revision 2 by reading its code (§2.1). Not established: whether its non-public commercial builds differ from the public AGPL code.
- Real burst-length distributions on the owner's tailnet and Wi-Fi paths. The literature gives typical ranges (§2); the LOSS_REPORT histogram will measure them.

---

## 8. References

Full annotated notes: `sota-notes.md`.

- [R1] RFC 9221, An Unreliable Datagram Extension to QUIC: https://www.rfc-editor.org/rfc/rfc9221
- [R2] Kempf, Kyber, FRnOG 39 slides: https://media.frnog.org/FRnOG_39/FRnOG_39-6.pdf
- [R3] https://kyber.tech/ ; https://jbkempf.com/
- [R4] Kempf, Akademy 2023: https://conf.kde.org/event/5/contributions/147/attachments/76/92/Akademy2023_Kempf.pdf
- [R5] Interview: https://streaminglearningcenter.com/codecs/an-interview-with-jean-baptiste-kempf-of-kyber.html ; https://gigazine.net/gsc_news/en/20250804-vlc-kyber/
- [R6] SVTA talk: https://university.svta.org/conference-proceedin/kyber-a-new-approach-for-real-time-video-and-controls-streaming-based-on-quic/
- [R7] Talk recordings: https://www.youtube.com/watch?v=0RvosCplkCc ; https://www.youtube.com/watch?v=lWZt3ehI7us ; https://www.youtube.com/watch?v=nqVzOMebNx8
- [R8] https://github.com/lenylvt/syber
- [R9] https://github.com/LizardByte/Sunshine/blob/master/src/stream.cpp
- [R10] https://github.com/LizardByte/Sunshine/blob/master/docs/configuration.md ; https://github.com/LizardByte/Sunshine/blob/master/src/rtsp.cpp
- [R11] https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/RtpVideoQueue.c
- [R12] https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/VideoDepacketizer.c ; https://github.com/moonlight-stream/moonlight-common-c/blob/master/src/ControlStream.c
- [R13] nanors: https://github.com/sleepybishop/nanors
- [R14] MS-RDPEUDP: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeudp/
- [R15] MS-RDPEUDP2: https://winprotocoldocs-bhdugrdyduf5h2e4.b02.azurefd.net/MS-RDPEUDP2/%5bMS-RDPEUDP2%5d.pdf
- [R16] PCoIP retransmission: https://anyware.hp.com/knowledge/what-is-intelligent-packet-re-transmission-in-pcoip-protocol ; https://docs.teradici.com/knowledge/what-is-an-acceptable-level-of-packet-loss
- [R17] Rudow et al., Tambur, NSDI'23: https://www.usenix.org/conference/nsdi23/presentation/rudow (PDF https://www.usenix.org/system/files/nsdi23-rudow.pdf)
- [R18] Zoom patent: https://patents.google.com/patent/US9813193
- [R19] Parsec: https://parsec.app/blog/a-networking-protocol-built-for-the-lowest-latency-interactive-game-streaming-1fd5a03a6007/
- [R20] Chrome Remote Desktop: https://chromium.googlesource.com/chromium/src.git/+/main/remoting/protocol/webrtc_video_stream.h
- [R21] https://webrtc.googlesource.com/src/+/main/modules/video_coding/media_opt_util.cc
- [R22] https://webrtc.googlesource.com/src/+/main/modules/rtp_rtcp/source/ulpfec_generator.cc
- [R23] https://webrtc.googlesource.com/src/+/main/modules/video_coding/nack_requester.cc ; https://github.com/webrtc-sdk/webrtc/blob/master/modules/rtp_rtcp/source/rtp_packet_history.cc
- [R24] https://webrtc.googlesource.com/src/+/main/modules/congestion_controller/goog_cc/loss_based_bwe_v2.cc
- [R25] FlexFEC, RFC 8627: https://www.rfc-editor.org/rfc/rfc8627
- [R26] Transport-wide CC: https://datatracker.ietf.org/doc/html/draft-holmer-rmcat-transport-wide-cc-extensions-01
- [R27] RFC 8888: https://www.rfc-editor.org/rfc/rfc8888
- [R28] RFC 9265, Forward Erasure Correction for QUIC loss recovery: https://www.rfc-editor.org/rfc/rfc9265.html
- [R29] draft-michel-quic-fec: https://datatracker.ietf.org/doc/html/draft-michel-quic-fec
- [R30] QUIC-FEC (2019): https://arxiv.org/abs/1904.11326
- [R31] FlEC (ToN 2023): https://arxiv.org/abs/2208.07741
- [R32] Tetrys, RFC 9407: https://www.rfc-editor.org/rfc/rfc9407
- [R33] RLC, RFC 8681: https://www.rfc-editor.org/rfc/rfc8681 ; framework RFC 8680: https://www.rfc-editor.org/rfc/rfc8680
- [R34] FECFRAME, RFC 6363: https://www.rfc-editor.org/rfc/rfc6363
- [R35] RaptorQ, RFC 6330: https://www.rfc-editor.org/rfc/rfc6330
- [R36] RFC 9002: https://www.rfc-editor.org/rfc/rfc9002
- [R37] https://github.com/AndersTrier/reed-solomon-simd
- [R38] https://github.com/catid/leopard
- [R39] https://github.com/rust-rse/reed-solomon-erasure
- [R40] https://github.com/cberner/raptorq
- [R41] https://github.com/catid/cm256
- [R42] https://github.com/catid/wirehair
- [R43] Streaming codes, burst + isolated: https://arxiv.org/abs/1208.0072 ; https://arxiv.org/abs/1308.3827 ; https://arxiv.org/abs/1801.05919
- [R44] Microsoft Research streaming codes: https://www.microsoft.com/en-us/research/?p=157762
- [R45] ReVo critique: https://arxiv.org/pdf/2604.27441 ; TAROT: https://arxiv.org/abs/2602.09880
- [R46] RFC 3611 (XR burst/gap): https://www.rfc-editor.org/rfc/rfc3611 ; RFC 7003: https://www.rfc-editor.org/rfc/rfc7003
- [R47] RFC 5725 (post-repair loss RLE): https://www.rfc-editor.org/rfc/rfc5725
- [R48] RFC 7509 (post-repair loss count): https://www.rfc-editor.org/rfc/rfc7509
- [R49] PR-SCTP, RFC 3758: https://www.rfc-editor.org/rfc/rfc3758
- [R50] DTP: https://datatracker.ietf.org/doc/html/draft-shi-quic-dtp
- [R51] BBR: https://datatracker.ietf.org/doc/draft-ietf-ccwg-bbr/
- [R52] Wi-Fi loss modelling: https://www.tkn.tu-berlin.de/bib/happ2013fault-impact/ ; https://www.gta.ufrj.br/ftp/gta/TechReports/CaRe07.pdf
- [R53] W3C WebTransport (datagram stats such as `droppedIncoming`; verify browser support before relying on it): https://www.w3.org/TR/webtransport/
- Code: quinn-proto 0.11.14 (`src/connection/datagrams.rs`, `src/congestion/cubic.rs`, `src/connection/stats.rs`) from the local cargo registry.
