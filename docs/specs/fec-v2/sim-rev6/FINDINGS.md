# Revision 6 simulation: block closing and code family

**Every number here is a model estimate [E], not a measurement.** Emission timing comes from the code (`TIMING.md`, with file:line). Loss, the link and the receiver are modelled.

## Method
- **Script and runtime.** `python3 -I uniform_sim.py` takes about 100 s on 12 cores. Self-checks are in `selftest.py`; a deliberately broken scheme is caught. Seed spread is in `seedcheck.py`.
- **Sender.** It is paced at {2, 10, 30, 100} Mbit/s. Bundles are 1 180 B, one per QUIC packet. Passes follow the code's cadence: token bucket, `D_target` = 10 ms of rate, and passes at least 1 ms apart with a backlog (`TIMING.md` §2).
- **Traffic mixes:**
  - H.264 at 30 fps, with P frames of about rate/30 and occasional IDRs;
  - tile bursts separated by idle gaps;
  - both together (tiles are 31 % of the bytes).
- **Loss models.** Per packet: i.i.d. 0.5, 1, 2 and 5 %; Gilbert-Elliott 2 % with mean burst 2, 3 and 8.
- **Reported delays.** Absolute latencies carry a 15–30 ms offset from a standing bottleneck queue, so the tables report:
  - the **excess** recovery delay: lost source's departure → decodable, minus its loss-free arrival;
  - **frame lateness**: H.264 frame completion beyond 33 ms (1 frame) and 66 ms (2 frames) past the loss-free completion.
- **ARQ comparison.** It is analytic: RTT + detection (¼ RTT + 5 ms ACK batching). That is 17.5 / 55 / 105 ms beyond the bundle's own arrival at one-way delays of 5 / 20 / 40 ms. FEC's excess delay does not depend on the one-way delay, so the delays were not run separately.
- **Seeds.** Each cell is one seed of about 60k bundles. Under Gilbert-Elliott loss, residuals move by up to ±40 % between seeds (`seedcheck_out.txt`), so schemes are ranked only where they differ by more than that.
- **Not modelled:**
  - CPU cost (that is the codec spike, `../spike-rev6/RESULTS.md`);
  - extra admission passes triggered by ACKs;
  - time-correlated loss;
  - reordering;
  - lost feedback.

## Timing that drives everything (`TIMING.md`)
- The median number of bundles per admission pass is **1 at 2 and 10 Mbit/s, 2 at 30, and 9 at 100.** A block closed at the end of each pass is therefore tiny except at 100 Mbit/s.
- quinn's pacer runs at about 2.5× the rate, so it does not smooth the passes.

## Answers (H.264 at 20 % nominal overhead and i.i.d. 2 % loss, unless noted)

### 1. Closing an RS block at the end of each drain: rejected at 30 Mbit/s and below
- **Real overhead.** With r_min 1 it is 100 / 69 / 45 / 22 % at 2 / 10 / 30 / 100 Mbit/s. With r_min 2 it is 200 / 131 / 169 / 22 %.
  - At 2–10 Mbit/s a block is one bundle, so this is a repetition code. Latency is excellent (1–9 ms excess), but the cost is not acceptable.
- **At 100 Mbit/s** it works for i.i.d. loss: 22 % overhead, residual 0.00 %. With mean-burst-3 loss, 45–50 % of losses still need ARQ and about 30 % of frames are late.
- **Settings.** k_max 64 with r_min 1 is the better choice, but neither setting rescues the rule, and interleaving does not either.

### 2. A time cap is needed
- **T = 5 ms** reaches the nominal overhead from 10 Mbit/s up (21–23 %, recovery p95 13–17 ms). At 2 Mbit/s it still costs 68 %, because one slot is 4.7 ms.
- **T ≈ max(5 ms, 6 slots)**, bounded by one frame interval. At 2 Mbit/s that is 25–33 ms, for 27–32 % overhead.
- **Bursts need larger blocks.** With mean-burst-3 loss at 30 Mbit/s, late frames fall from 4–9 % at T = 5 ms to 0.1 % at T = 25 ms.

### 3. RaptorQ, one large block per frame (Kyber's approach)
- **H.264 frames.** It wins under bursts because the block is large compared with the burst. Late frames with burst-3 loss are 0.0 / 0.3 / 2.2 / 1.9 % at 100 / 30 / 10 / 2 Mbit/s.
  - **Per-frame RS with k + r ≤ 256 comes within 0.5 point of it.** The win comes from the block size, not from the code.
  - Its reception overhead (one spare symbol) makes no visible difference.
- **Tiles.** One block per burst is unusable: the recovered-bundle p95 is 9.7 s / 2.1 s / 0.74 s / 160 ms at 2 / 10 / 30 / 100 Mbit/s, against 55 ms for ARQ. Capped at k = 64 it behaves like RS.
- **Cost.** r_min 2 gives 43 % overhead at 2 Mbit/s. The CPU and wasm costs are in the codec spike: 267–3 160 µs to decode in wasm, and +113 KB.

### 4. Sliding-window RLC
- **Latency and residual.** From 10 Mbit/s up it has the best of both at exactly the nominal overhead. Recovery p95 is 9.4 / 3.1 / 0.9 ms at 10 / 30 / 100 Mbit/s, against 13–17 ms for time-capped RS.
- **Bursts need a window of 64.** With burst-3 loss on tiles, 1.2–2.8 % of losses still need ARQ at w = 64, against 18–24 % at w = 32. Late frames are 2.4 / 0.0 / 1.7 % at 10 / 30 / 100 Mbit/s.
- **At 2 Mbit/s** it is slower (p95 42 ms) but costs 20 %, against 35–68 % for RS.
- **Idle flush is essential.** It means sending the pending repair as soon as the sender goes idle.
  - p99 recovery on tiles: 606 / 395 / 81 / 36 ms without it, 75 / 16 / 3.5 / 1.9 ms with it.
  - For H.264 below 10 Mbit/s: 0.80 % late frames with it, 3.57 % without, at 2 Mbit/s and 10 % nominal overhead.
  - It costs about 3 extra points of overhead on small frames there.
- **Caveat.** This sim's RLC decoder is ideal (near-MDS, independent with probability 1 − 1/256). The codec spike measured cu-fec's real decoder at 126 µs to 4 ms in wasm.

### 5. Uniform versus per-class
- **H.264 is not hurt** by going uniform: the same code matches or beats per-frame RS.
- **The cost is the tile bytes.**
  - At 20 % nominal overhead the tiles add about 6 points of total traffic (they are 31 % of the mix). That buys a mean repair delay of 0.4–18 ms instead of 55 ms.
  - ARQ costs about the loss rate (2 % of tile bytes), so FEC at 20 % costs 10× that.
  - **Parity matched to the loss (about 1.5–2 × p) is enough.** At a fixed 20 %, a 3 MB first paint at 10 Mbit/s pays 0.5 s.
- **At 2 Mbit/s** the per-class design is no cheaper: 26 % against 24.9 %.

## What this means for the design (input to `../fec-v2-rev6.md` §4)
- Close blocks on whichever comes first:
  - k_max = 64;
  - a time cap T = max(5 ms, 6 slots), bounded by the frame interval;
  - the sender going idle (flush).
- A per-pass close is never enough on its own.
- r_min = 1. Size the parity from the measured loss, not a fixed percentage.
- RS with this closing rule matches RaptorQ-per-frame on H.264, and is usable for tiles where RaptorQ-per-burst is not.
- Sliding-window RLC is 4–15 ms better at p95 from 10 Mbit/s up, and cheaper at 2 Mbit/s. It needs a fast decoder that does not exist off the shelf.

## Raw output
Only `summary_out.txt` and `seedcheck_out.txt` are committed. The full per-cell tables (`uniform_sim_out.txt`, `uniform_sim_frames_out.txt`, `uniform_sim_arq_out.txt`, `uniform_sim_classes_out.txt`, about 3.3 MB) are regenerated by `python3 -I uniform_sim.py`.
