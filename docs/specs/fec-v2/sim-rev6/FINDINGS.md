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

## 6. Item 4: split GF(2^8) blocks vs one GF(2^16) block (above 70 Mbit/s)

**Every number is a model estimate [E].** Script `split_sim.py` (about 3 min on 12 cores, `python3 -I split_sim.py`; `--selftest` runs the checks); output `split_sim_out.txt` (full grid, seeds, gap list).

### Method
- **Reused.** Sender, bottleneck, Gilbert-Elliott loss, receiver and frame metrics are `uniform_sim.py`'s. The only change there is an additive `fam == "split"` branch in `enqueue_unit` (existing runs are unaffected). H.264 frames get the planned blocks. In the mix, tile bursts get plain RS blocks of at most 64 at the same overhead.
- **Schemes at equal nominal overhead** (rho = R / k_frame, R = max(1, ceil(rho k))):
  - **a_il:** m = ceil((k+R)/256) GF(2^8) RS blocks per frame (raised until every k_b + r_b ≤ 256), bundle i to block i mod m, r = ceil(R/m) each. The ceiling makes the real total up to m − 1 repairs above R (visible in `ovh`: 20.3 % against 20.2 % for one block).
  - **a_contig:** same m and r, contiguous assignment (control).
  - **b_mds:** one ideal MDS block over the frame, r = R. It stands for Leopard / GF(2^16); no decode cost is modelled.
  - **c_rq:** RaptorQ per frame, r = R.
  - **d_T5, d_T33:** the rev-6 default, time-capped RS, k_max 64, T = 5 ms and T = frame interval (`rs_time`).
  - The repairs of a frame queue behind its sources, round-robin across blocks.
- **Grid** (one seed, 4 000 frames = 133 s of video per cell): 70 / 100 / 150 / 200 Mbit/s, H.264-only and mix, iid 1 % and 2 %, GE 2 % with mean burst 3 / 8 / 16, rho 10 / 20 / 30 %. That is 144 timelines.
- **Seeds.** 5 seeds of 4 000 frames (demand, timeline and loss all re-drawn) for GE b3 and b8 at 100 and 200 Mbit/s, rho 20 %, both mixes. The grid is single-seed; at 1 point of late frames one cell is about 40 frames, and GE cells move by tens of percent between seeds, so grid differences below about 0.5 point are noise.
- **Metrics.** Late frames (> 33 ms / > 66 ms after the loss-free completion, or never complete), residual source loss and recovery-wait p95. The last two are over H.264 source bundles only.
- **Self-test** (`--selftest`): block structure (cover, ≤ 256, total repairs in [R, R + m)), and a deliberately broken (a) with r = 0 on one block. At 100 Mbit/s, rho 20 %, iid 2 %: residual 0.478 % against 0.000 %, late frames 35.7 % against 0.0 %. GE b3: 0.523 % against 0.000 %, 32.9 % against 0.0 %.

### Key table: late frames > 33 ms (%), mean (min–max) over 5 seeds, rho 20 %

| Mbit/s | mix | loss | a_il | a_contig | b_mds | c_rq | d_T5 | d_T33 |
|---|---|---|---|---|---|---|---|---|
| 100 | h264 | ge2b3 | 0.16 (0.05-0.22) | 0.18 (0.05-0.25) | 0.17 (0.07-0.25) | 0.18 (0.07-0.25) | 4.27 (3.80-4.67) | 2.14 (1.98-2.25) |
| 100 | h264 | ge2b8 | 1.23 (1.04-1.33) | 2.10 (1.98-2.27) | 1.22 (1.14-1.36) | 1.22 (1.14-1.36) | 13.90 (12.94-14.72) | 11.17 (10.59-11.75) |
| 200 | h264 | ge2b3 | 0.01 (0.00-0.05) | 0.03 (0.00-0.05) | 0.03 (0.00-0.07) | 0.03 (0.00-0.07) | 4.55 (4.30-4.91) | 4.14 (3.93-4.27) |
| 200 | h264 | ge2b8 | 0.40 (0.27-0.54) | 2.09 (1.78-2.32) | 0.49 (0.37-0.64) | 0.49 (0.37-0.64) | 19.41 (18.44-20.54) | 19.22 (18.35-19.85) |
| 100 | mix | ge2b3 | 0.39 (0.32-0.47) | 0.39 (0.32-0.47) | 0.39 (0.27-0.47) | 0.39 (0.27-0.47) | 2.88 (2.17-3.28) | 1.48 (1.36-1.78) |
| 100 | mix | ge2b8 | 1.75 (1.38-1.98) | 2.24 (1.95-2.54) | 1.80 (1.75-1.83) | 1.81 (1.75-1.85) | 9.50 (8.40-10.12) | 7.72 (7.04-8.10) |
| 200 | mix | ge2b3 | 0.09 (0.07-0.10) | 0.11 (0.10-0.12) | 0.11 (0.05-0.17) | 0.11 (0.05-0.17) | 3.36 (2.89-3.75) | 2.63 (2.10-3.11) |
| 200 | mix | ge2b8 | 0.95 (0.89-0.99) | 1.97 (1.88-2.05) | 0.87 (0.64-1.09) | 0.87 (0.64-1.11) | 13.66 (12.77-14.62) | 13.36 (12.57-14.12) |

Residual source loss follows the same ordering (for example 100 Mbit/s h264 b8: 0.121 / 0.249 / 0.127 % for a_il / a_contig / b_mds; seeds in `split_sim_out.txt`).

### Answer
- **(a) interleaved against (b) one MDS block: no measurable gap.**
  - On the 5-seed cells the means differ by at most 0.09 point of late frames (0.02 for GE b3, 0.09 for b8), inside the seed spread of 0.1–0.4 point. The sign is not consistent: a_il is lower in 5 of 8 cells, equal in 1 and higher in 2.
  - Over all 120 grid cells (single seed), the largest a_il − b_mds is +0.74 point (70 Mbit/s mix, rho 10 %, b8), the largest negative −0.96. **Nowhere does the gap exceed 1 point** (0 of 120). The point is not a decision threshold the data approaches.
  - Where a_il is slightly better than b_mds it is not a real advantage: both have the same R, so MDS dominates cell for cell. The differences come from a different number of packets, hence a different alignment to the same loss process.
- **Contiguous split (no interleave) is clearly worse under bursts:** 2.1 against 1.2 % (100 Mbit/s b8), 2.1 against 0.4 % (200 Mbit/s b8), and up to 6.5 % at 150 Mbit/s b16. Interleaving is what keeps (a) near MDS; it is not optional.
- **(c) RaptorQ equals (b) in every cell** (identical to two decimals in most). The spare-symbol failure probability never shows at r ≥ 10, so it does not argue for or against it.
- **(d) the rev-6 default is far behind at these rates:** 4–5 % late frames at b3 and 13–19 % at b8 (T = 5 ms and T = 33 ms alike). At 100 Mbit/s a k_max of 64 is about 6 ms of traffic, so the block is shorter than a burst. This is a larger effect than (a) against (b) by an order of magnitude.
- **Cost of the per-frame codes (a, b, c).** Repairs follow the whole frame, so the recovery-wait p95 is about 62–70 ms for all three (and 72–100 ms at b16) against 10–20 ms for time-capped RS (`w95` column). This is equal across (a), (b) and (c) and so does not change the choice. It is the price of the span, and it makes late frames the metric where (d) loses.
- **Not in the model:** the CPU cost of (b) (about 0.9 ms per lossy frame in wasm, §8.1), the second codec, and a hybrid in which a frame's repairs are sent progressively.

### Consequence for §8.1
The condition "choose (b) only if (a) leaves a gap of more than a point or so of late frames" is not met anywhere in the grid, with the interleaved assignment. Recommendation: (a), m = ceil((k + R)/256), interleaved assignment i mod m, r = ceil(R/m). The contiguous variant must not be used. The larger finding is that, above 70 Mbit/s, the frame-spanning blocks of (a) beat the current time-capped default by 4–19 points of late frames under bursts.

Caveats: single-seed grid; 70 Mbit/s mostly fits one block (m = 1, so (a) = (b) by construction); the loss model is per packet with no time correlation; ideal MDS decode.

## 7. Rule 3 (Empty close) for RS blocks

**Every number is a model estimate [E].** Script `empty_sim.py` (about 10 s, `python3 -I empty_sim.py`), output `empty_sim_out.txt` (full table and block structure).

### Method
- **Simulator change.** `uniform_sim.py`, family `rs_time`, gets two switches (existing runs are unaffected):
  - `T = "formula"` is the design's `min(max(5 ms, 6 bundle slots), frame interval)`: 28.3 / 5.7 / 5 / 5 ms at 2 / 10 / 30 / 100 Mbit/s.
  - `"empty": True` adds rule 3. At the end of a pass (`do_pass`), if the content queue is empty, the open block closes and its repairs go out in that pass, charged like sources. That is what `Stop::Empty` sees: tokens or depth ending the pass with content still queued does not close.
  - `empty_kmin` (no Empty close below this k) and `empty_rmin` (minimum repairs at an Empty close) are the two parameter changes tried.
- **Grid.** Variants: `kT` (k_max 64 and T), `kT+empty`, `kT+empty r2` (r at least 2 on an Empty close), `kT+empty k>=4` and `k>=8`. Rates 2 / 10 / 30 / 100 Mbit/s; H.264 and tiles-only traffic; i.i.d. 1 % and 5 %, and Gilbert-Elliott 2 % with mean burst 3; nominal overhead 20 %, r_min 1; 5 seeds (demand and loss re-drawn), 60 000 source bundles each.
- **Metrics.** Late frames (> 33 ms after the loss-free completion, H.264), tile recovery wait p95 / p99 over all lost bundles, and `c0` p95: the same for lost bundles in bursts of 1-6 bundles (55 % of bursts), the case rule 3 targets. Overhead is repair / source bundles; k is the mean block size.

### Result (mean of 5 seeds; late frames with seed min-max)

| Mbit/s | variant | H.264 late %, iid 5 % | H.264 late %, GE b3 | tiles p95 / p99 ms, iid 1 % | tiles c0 p95 ms, iid 1 % / GE b3 | ovh % | mean k |
|---|---|---|---|---|---|---|---|
| 2 | kT | 1.03 (0.9-1.1) | 2.62 (2.5-2.8) | 33.0 / 47.2 | 30.2 / 28.3 | 31.1 | 5.6 |
| 2 | kT+empty | 1.09 (0.9-1.2) | 2.57 (2.4-2.8) | 33.0 / 49.1 | 24.5 / 30.2 | 31.3 | 5.4 |
| 2 | kT+empty r2 | 0.70 (0.6-0.8) | 2.48 (2.3-2.7) | 33.0 / 45.3 | 29.3 / 34.0 | 37.2 | 5.4 |
| 10 | kT | 2.72 (2.5-3.0) | 7.74 (7.4-8.1) | 8.5 / 17.2 | 7.7 / 6.2 | 27.9 | 7.9 |
| 10 | kT+empty | 2.46 (2.1-2.8) | 7.86 (7.7-8.2) | 9.1 / 18.1 | 5.9 / 6.0 | 27.9 | 7.9 |
| 30 | kT | 2.26 (1.9-2.7) | 8.59 (8.0-9.0) | 13.2 / 18.8 | 6.2 / 5.1 | 21.9 | 20.8 |
| 30 | kT+empty | 1.98 (1.0-2.6) | 8.61 (8.1-9.1) | 13.2 / 18.9 | 4.0 / 3.4 | 21.9 | 20.7 |
| 100 | kT | 0.27 (0.0-1.0) | 3.22 (2.4-4.1) | 12.3 / 14.9 | 6.7 / 8.3 | 20.6 | 52.4 |
| 100 | kT+empty | 0.14 (0.0-0.3) | 3.29 (2.1-4.1) | 12.1 / 14.9 | 3.0 / 3.7 | 20.6 | 52.2 |

(ovh and k are for the H.264 mix at GE b3; the tiles rows differ by at most 1 point.)

### Block size at low rate (the design's caveat)
Share of blocks closed on Empty, their mean k, and the share of source bundles in blocks with k <= 3 (H.264, `kT` -> `kT+empty`):

| Mbit/s | closed on Empty | k of those | sources in k <= 3 blocks |
|---|---|---|---|
| 2 | 43 % | 3.9 | 7.0 -> 8.7 % |
| 10 | 14 % | 8.7 | 0.6 -> 0.7 % |
| 30 | 16 % | 25.5 | 0.1 -> 0.1 % |
| 100 | 14 % | 31 | 0.0 -> 0.0 % |

### Conclusion
- **Rule 3 holds for RS: it costs nothing measurable and helps small tile bursts.**
  - Overhead moves by at most 0.2 point in every row, and mean k by at most 0.2 on H.264 (1.6 of 45 on 100 Mbit/s tiles). Late frames and tile p95 / p99 differ by less than the seed spread in every row.
  - It cuts the p95 recovery of small tile bursts (1-6 bundles) by 2-4 ms from 30 Mbit/s up (6.7 -> 3.0 ms at 100 Mbit/s, 6.2 -> 4.0 at 30). Below that it does nothing: at 2 and 10 Mbit/s the queue is rarely empty while a big tile burst drains, and the c0 gain is 0-2 ms and not consistent across loss models.
  - It does not move H.264 late frames. A lost bundle waits for the end of its block, which T or k_max set; Empty only shortens the last block of each frame, by less than T, and that is far under the 33 ms threshold.
- **The design's caveat is real but is T's doing, not rule 3's.** At 2 Mbit/s 43 % of H.264 blocks close on Empty with mean k 3.9, but `kT` alone already gives k 5.6 and 31 % overhead (T = 28 ms is one frame). Adding Empty raises the share of sources in k <= 3 blocks from 7.0 to 8.7 % and the late frames under burst-3 loss do not change (2.62 -> 2.57 %). The cost of small blocks is paid with or without rule 3, and the burst span lost is inside the seed spread.
- **Parameter changes.**
  - `empty_kmin` 4 or 8: no effect at all (rows identical to `kT+empty` or `kT`). Not needed.
  - `empty_rmin` 2 helps only where blocks are tiny: at 2 Mbit/s i.i.d. 5 % late frames fall from 1.03 to 0.70 %, for +6 points of overhead (31 -> 37 %); at 10 Mbit/s and up nothing. Not recommended as a default. If wanted, apply it only when the advertised T is the frame interval (about 2 Mbit/s).
  - No change to rules 1-4. Keep rule 3 as written.
- **Not modelled:** the real emitter's queue also holds retransmissions and control content, which would make Empty rarer; tile bursts and frames share one queue here, as in the sender model, with no priorities.

## 8. R6-e (plan task E1): the T cap, the ladder, the split gate, and the exact tail oracle

**Every number is a model estimate [E].** Scripts: `adapt_oracle.py` (exact tail, `--selftest`, `--vectors`), `adapt_sim.py` (`--part cap|ladder|split|lowrate`, `--selftest`, about 25 min on 12 cores), `adapt_summary.py` (the comparisons quoted here, read from `adapt_sim_out.txt`). `selftest.py` runs both self-tests. The only change to `uniform_sim.py` is the additive `spec["radapt"]`: r per block from `adapt_oracle.r_for` instead of a fixed overhead; existing runs are unaffected.

### Method
- **r rule.** Every block gets `r_for(k, p, b, 1e-3)` from its own k, with the TRUE (p, b) of the loss process (a perfect estimator; the estimator's error is E3's and E11's). Plan caps: r <= 32; r <= max(1, ceil(k/4)) for k >= 4; the plan lifts the 25 % cap for k < 4 (sim: cap 32). When the target is not reachable inside the cap, r is the cap.
- **Loss.** Gilbert-Elliott, loss 1 in the bad state, mean burst b, stationary start; b = 1 is i.i.d. (the sim's `ge_chain`). The DP and the sim's chain are the same process, so the DP is the right oracle for a block's consecutive datagrams.
- **Cells.** `rs_time` T in {5, 10, 15, 20, 25, 28, 33} ms x k_max in {64, 128, 223} x 2 / 10 / 30 / 100 / 200 Mbit/s x {h264, mix} x GE 2 % with b 3 and 8, **5 seeds** (demand, timeline and loss re-drawn), 600 frames (<= 30 Mbit/s) or 300 frames (100, 200 Mbit/s) of video per seed. Ladder cells: 10 / 30 / 100 Mbit/s x p {0.5, 2, 5 %} x b {1, 3, 8}, four notches. Low-rate cells: 2 and 10 Mbit/s, 10 seeds.
- **Metrics.** late33 / late66 = % of H.264 frames completing more than 1 / 2 frame intervals after the loss-free completion (or never); tile recovery p95 = `tw95` in `adapt_sim_out.txt` (recovered lost tile bundles, ms of FEC delay); residual; overhead = repairs / sources over the whole run.
- **Seed spread.** A late33 difference is called real only if it exceeds the larger of the two cells' seed ranges (max - min). With 300 to 600 frames per seed one frame is 0.17 to 0.33 point, so ranges of 1 to 4 points are normal for GE cells. Cells inside the spread are **not ranked**.
- **Oracle self-test** (`adapt_oracle.py --selftest`): i.i.d. against the binomial tail (relative 1e-12), GE distribution sums to 1 and has mean n p, DP against Monte Carlo (5 sigma, four cases where the start state matters), **a DP that starts in the good state fails the same check**, the tail is monotone in r, bursts raise it, and `r_for` is the smallest r (r - 1 misses the target unless the cap bound). Sim self-test: overhead rises with loss, and r = 0 for every block shows residual 4.7 % against 0.08 % under the rule.

### The numbers E3, E4 and E11 consume
- **Oracle for E4.** `adapt_oracle_vectors.csv`: columns `n,r,p,mean_burst,tail` (tail = P(more than r of n datagrams lost), `%.17g`), 4608 rows over p {0.001, 0.005, 0.01, 0.02, 0.05, 0.1} x mean_burst {1, 1.5, 3, 8} x n {4, 10, 24, 64, 128, 223} x r 1..32. mean_burst 1 is the binomial tail. n = k + r of the block; GE rows start in the stationary state. For r >= n the tail is 0. E4 should compare `r_for` against `adapt_oracle.r_for` through these rows (smallest r with tail <= 1e-3 inside the caps) and must reproduce the `*` cells below.
- **r_for(k) at target 1e-3** (`*` = target not reachable inside the cap; the value is the cap; k < 4 uncapped as the plan words it):

| b | p | k=1 | 3 | 4 | 10 | 24 | 64 | 128 | 223 |
|---|---|---|---|---|---|---|---|---|---|
| 1 | 0.1 % | 1 | 1 | 1 | 1 | 1 | 2 | 2 | 3 |
| 1 | 0.5 % | 1 | 1 | 1 | 2 | 2 | 3 | 4 | 6 |
| 1 | 1 % | 1 | 1 | 1 | 2 | 3 | 4 | 6 | 8 |
| 1 | 2 % | 1 | 2 | 1* | 3 | 4 | 6 | 9 | 13 |
| 1 | 5 % | 2 | 3 | 1* | 3* | 6 | 11 | 16 | 24 |
| 1 | 10 % | 3 | 4 | 1* | 3* | 6* | 16* | 28 | 32* |
| 3 | 0.1 % | 1 | 2 | 1* | 3* | 6 | 8 | 10 | 12 |
| 3 | 0.5 % | 4 | 6 | 1* | 3* | 6* | 13 | 15 | 18 |
| 3 | 1 % | 6 | 7 | 1* | 3* | 6* | 15 | 19 | 22 |
| 3 | 2 % | 8 | 9 | 1* | 3* | 6* | 16* | 23 | 29 |
| 3 | 5 % | 10 | 12 | 1* | 3* | 6* | 16* | 32* | 32* |
| 3 | 10 % | 12 | 14 | 1* | 3* | 6* | 16* | 32* | 32* |
| 8 | 0.1 % | 1 | 2 | 1* | 3* | 6* | 16* | 22 | 26 |
| 8 | 0.5 % | 13 | 14 | 1* | 3* | 6* | 16* | 32* | 32* |
| 8 | 2 % | 23 | 25 | 1* | 3* | 6* | 16* | 32* | 32* |
| 8 | 10 % | 32* | 32* | 1* | 3* | 6* | 16* | 32* | 32* |

  Two facts E4 must carry. (1) **The 25 % cap binds** for any burst of 3 or more at k <= 64 and for b = 8 at every k: the 1e-3 target is not reachable and r is the cap. (2) **The k < 4 exception makes the rule non-monotone**: at b = 3, p = 2 %, k = 3 asks r = 9 (300 % overhead) and k = 4 asks r = 1.
- **Remove the k < 4 exception** (plan decision 6). `adapt_sim.py --part lowrate`, 10 seeds, 2 Mbit/s, T 20, k_max 64:

| mix | loss | overhead, exception (cap 32) | cap 3 | no exception (cap 1) | late33 (all three) |
|---|---|---|---|---|---|
| h264 | GE b3 | 45.9 (44-49) | 42.6 | 39.1 (39-40) | 2.7-2.9, inside the spread |
| h264 | GE b8 | 55.3 (50-62) | 42.6 | 39.1 | 2.7-3.0 |
| mix | GE b3 | 74.8 (58-103) | 55.2 | 40.0 (40-40) | 1.6-2.3, inside the spread |
| mix | GE b8 | 116.7 (80-178) | 55.2 | 40.0 | 2.1-2.3 |

  The extra parity buys no measurable late-frame gain; the overhead is 40 to 117 % against a 25 % cap everywhere else. Recommend `r <= max(1, ceil(k/4))` for every k.
- **T cap and k_max** (below): T = 20 ms with k_max 128 up to ~60 Mbit/s and 223 above; overhead at 100 / 200 Mbit/s with 223: 14-17 % against 25 % with 128.
- **Ladder** (below) and the split gate (below).

### (i) Does T <= 20 ms lose against T = 25 / 28 / 33 ms? No, not measurably.
Comparing T = 20 with T = 25, 28, 33 for every k_max, rate, mix and burst (b 3 and 8), counting only differences larger than both seed ranges:
- **<= 30 Mbit/s: 107 of 108 comparisons are inside the spread**; one is real (30 Mbit/s h264 b8, k_max 128: T = 33 is 1.7 points better). Other cells at 10 Mbit/s lean the same way (T = 28 lower by 0.7-1.2 points on all four) but inside the spread, so they are not ranked.
- **100 and 200 Mbit/s: 72 of 72 inside the spread.** At these rates k_max binds long before T (223 bundles are 21 ms at 100 Mbit/s, 10 ms at 200).
- **Tile recovery p95 is lower with T = 20 than with T = 33** (mix, k_max 128, GE b3): 27.8 against 38.1 ms at 10 Mbit/s and 23.6 against 33.5 ms at 30 Mbit/s (ranges 23.6-33.0 and 33.0-33.7; 30 Mbit/s separates, 10 Mbit/s overlaps), at the price of 2.9 points more overhead at 30 Mbit/s (25.0 against 22.1, ranges do not overlap). At 100 Mbit/s they are equal. So the cap is not only free but buys tile latency.
- Per cell the best configuration is almost always inside a set of 12-21 of 21 configurations indistinguishable from it (`adapt_summary.py`, part ii); T = 5 ms is the best or tied at 2 Mbit/s only because there the sim's blocks are 1-3 sources and the "best" has 450-830 % overhead (the exception above), which is not a result.

### (ii) The ladder
Data: late33, mean (seed range), 5 seeds, `n0` = (T_base, 64), `n1` = (10 ms, 128), `n2a` = (20 ms, 128), `n2` = (20 ms, 223). Selected cells (all in `adapt_sim_out.txt`):

| Mbit/s | p, b | n0 | n1 | n2a | n2 |
|---|---|---|---|---|---|
| 30 | 0.5 %, 1 | 0.09 (0.31) | 0.09 | 0.00 | 0.00 |
| 30 | 2 %, 3 | 7.45 (1.69) | 1.97 (1.54) | 0.49 (1.08) | 0.68 |
| 30 | 2 %, 8 | 10.28 (2.61) | 6.62 (1.54) | 3.02 (0.76) | 3.17 |
| 30 | 5 %, 8 | 22.37 (4.00) | 15.14 (2.46) | 8.62 (2.16) | 9.02 |
| 10 | 5 %, 3 | 17.91 (4.15) | 14.02 (1.38) | 6.99 (3.07) | 6.99 |
| 100 | 2 %, 3 | 1.83 (1.71) | 0.69 (1.71) | 0.29 (0.00) | 0.23 (0.57) |
| 100 | 5 %, 8 | 28.23 (6.00) | 11.37 (3.43) | 8.74 (3.14) | 12.17 (4.29) |

- **i.i.d. loss (b = 1) needs no widening**: n0 is indistinguishable from the best in 7 of 9 cells (the other two, 10 and 30 Mbit/s at 5 %, take n1) (10 / 30 / 100 Mbit/s x p), and widening only lowers overhead (a 30 Mbit/s 0.5 % cell: 9.2 % for n0 against 4.7 %).
- **Bursts (b >= 3) need the wide notch**: n0 loses by 7 to 20 points where the difference is real (30 Mbit/s from p = 2 %, 100 Mbit/s p = 5 % and p = 2 % b8), and n1 (T = 10) still loses to n2a where the gap exceeds both ranges: 30 Mbit/s b8 at p 2 % and 5 %, 30 Mbit/s b3 at p 5 %, 10 Mbit/s b3 at p 5 % (30 Mbit/s b3 p 2 % is inside the spread).
- **k_max 223 against 128** is indistinguishable in late33 everywhere (at <= 30 Mbit/s 20 ms holds fewer than 128 bundles, so they are the same block) but at 100 Mbit/s it cuts overhead from 25 % to 17 % (the 25 % cap bound at 128 and the target is reached with 223 only at b <= 3).
- **Ladder to implement** (keyed on b_hat as in plan decision 6, with hysteresis up above 1.5 and down below 1.25): b_hat <= 1.5: (T_base, 64); 1.5 < b_hat: (20 ms, k_max = bundles in 20 ms at the source rate, clamped to [64, 223]). **An intermediate (10 ms, 128) notch is not supported by the data**: it is never the unique best, and where it is indistinguishable (b3, p 0.5 %) so is n2a. If E3 keeps three notches it must treat n1 as a transition step only. The loss level p moves r, not the notch: no cell shows p changing which notch wins at fixed b.
- r at each notch is `r_for(k, p_hat, b_hat)` above. Overhead observed: 5-31 % depending on p, b.

### (iii) What the 20 ms cap costs at 2 Mbit/s, against the design's 28 ms
10 seeds, k_max 64, `r <= max(1, ceil(k/4))` for all k (no exception), late33 unchanged within the spread:

| mix | loss | overhead T = 20 | T = 28 | difference |
|---|---|---|---|---|
| h264 | i.i.d. 2 % | 39.1 (39-40) | 32.4 (32-33) | +6.7 points |
| h264 | GE b3 | 39.1 | 32.4 | +6.7 |
| mix | i.i.d. 2 % | 40.0 (40-40) | 34.7 (33-37) | +5.3 |
| mix | GE b8 | 40.0 | 34.7 | +5.3 |

The cap costs about **+5 to +7 points of overhead at 2 Mbit/s (a 15-21 % relative increase)** and no measurable late-frame change; at 10 Mbit/s the cost is 0 to +2 points. With the plan's k < 4 exception the same cells cost +3 to +21 points but both sides are inflated. Decision 6 keeps T <= 20 ms for R6-e; this is the price of not changing the wire (`T_adv`).

### (iv) The split gate (decision 9)
Late33 (seed range), overhead, 5 seeds: T = 20 single block (k_max 128 / 223) against the interleaved `a_il` of §6 at rho 0.1 / 0.2 / 0.3.

| Mbit/s | mix | loss | T20 k128 | T20 k223 | a_il 10 % | a_il 20 % | a_il 30 % |
|---|---|---|---|---|---|---|---|
| 100 | h264 | b3 | 0.40 (0.28) o 18.7 | 0.06 (0.29) o 14.8 | 1.77 o 10.3 | 0.00 o 20.3 | 0.00 o 30.3 |
| 100 | h264 | b8 | 1.94 (1.71) o 25.1 | 2.23 (2.85) o 16.9 | 3.94 o 10.3 | 1.09 (0.85) o 20.3 | 0.34 o 30.3 |
| 100 | mix | b8 | 2.00 (3.43) o 25.2 | 0.97 (2.28) o 17.3 | 5.89 o 10.7 | 2.06 o 20.5 | 0.40 o 30.6 |
| 200 | h264 | b3 | 0.74 (1.71) o 18.3 | 0.40 (1.14) o 13.7 | 0.80 o 10.3 | 0.00 o 20.3 | 0.00 o 30.3 |
| 200 | h264 | b8 | 3.77 (2.86) o 25.1 | 4.06 (1.43) o 15.3 | 2.80 o 10.3 | 0.29 (0.86) o 20.3 | 0.11 o 30.3 |
| 200 | mix | b8 | 2.06 (2.29) o 25.1 | 2.91 (2.57) o 15.4 | 4.40 o 10.6 | 0.63 (0.86) o 20.5 | 0.23 o 30.5 |

- **At 100 Mbit/s: no real gap.** The single capped block at 15-17 % overhead is within the spread of `a_il` at 20 %, and `a_il` at 10 % (lower overhead) is worse.
- **At 200 Mbit/s with GE b8: a real gap of 3.5 points** (h264: 3.77 / 4.06 against 0.29, both ranges under 3) at a lower overhead for `a_il` (20 %) than the capped block's 25 % (k128). In the mix the gap is 1.4-2.3 points and inside the spread. At b3 the two are the same.
- **Gate:** the follow-up PR (R6-e2) is justified **only for 150-200 Mbit/s and above under bursty loss**; at <= 100 Mbit/s the capped single block is enough. The single capped block already closes most of §6's gap against the 5 ms / 33 ms defaults (13-19 points there, 3-4 here), because k_max 223 and the DP r do the work. Not measured: 150 Mbit/s, and unequal overhead means the comparison is a bound, not a like-for-like.

### Plan decisions these results contradict or change
1. **Decision 6, the k < 4 exception to the 25 % cap**: remove it (above). A k = 3 block must not ask for 9 parities.
2. **Decision 6, `T` and `k_max` ladder in {T_base, 10, 20} x {64, 128, 255 - r_max}**: the 10 ms notch is not supported; k_max should follow the rate (223 above ~60 Mbit/s).
3. **Decision 6, "r_for ... such that P <= 1e-3"**: it is unreachable inside the 25 % cap for b >= 3 at k <= 64 and for b = 8 everywhere. The cap binds, so the achieved tail there is above 1e-3 and the closed-loop trim and E11 must treat r = cap as "saturated", not as a failure.
4. **Decision 9** holds for <= 100 Mbit/s; at 200 Mbit/s under burst 8 it does not (follow-up justified).

## 9. R6-e (plan task E10): padding against the BWE suites, and the value gate

**Decision: the padding-as-repair code (E8 and E9) is deleted. The benefit is zero within the seed spread, and the BWE regression found in part 1 is not caused by it.** The code was reverted in four commits on the R6-e branch (E9 and the A/B scene, E8's pad rows and tag map, E8's `ResolvedAck` id; nothing else read the id, every consumer bound it to `_`). The measurement can be reproduced at the commit that added the A/B switch, which is the parent of the first revert.

### Part 2, the value gate (done first because it decides the code)

Setup: 10 Mbit/s behind a 150 ms tail-drop buffer, 50 ms one way, 5 % i.i.d. loss (5-7 % of datagrams dropped per run, the injected loss plus the bottleneck's own), adaptive mode, a rate-following H.264 stream (PCM encoder with the S2 rate model, in-place rate policy), 60 s per run. Both arms run the same seed; the only difference is a test-only switch in `IoBridge` (not an environment variable) that makes `pad_row_room` return `None`, so no window reserves rows and every pad is a copy, as before E9. 20 seeds: two sets of 10 (`0xFB0101..0A`, `0xFB0201..0A`), because the first set leaned towards "on" and the second leaned away from it, which is the answer to whether the lean was real.

Premises, all met in every run: loss in both arms; the controller engaged (`p_hat` 3-5 %); probe windows opened (12-16 per run, one per 4-5 s: the application's own probe is 5 s and goog_cc's are rarer, so **the plan's "a probe window about every second" was not reachable without changing a production constant, and the run is longer instead**); `probe_padding_repair_datagrams > 0` in every "on" run and exactly 0 in every "off" run.

"Frames late" is the closest lateness counter the harness has: the client's first decode of a frame more than 100 ms (the link's round trip, so a frame that waited for a retransmission) after the fastest frame's delay, measured from the encoder's logged capture timestamp. Never-fed frames and freeze episodes are `H264Outcome`'s. `handoffs + blocks_closed_declined` is 0 in all 40 runs.

| seed | handoffs + declined (on / off) | frames late (on / off) | never fed (on / off) | freezes (on / off) | pad datagrams as repair / all pad (on) | probe windows (on / off) | datagrams dropped (on / off) |
|---|---|---|---|---|---|---|---|
| fb0101 | 0 / 0 | 32 / 44 | 35 / 77 | 14 / 22 | 50 / 2541 | 15 / 15 | 1933 / 2105 |
| fb0102 | 0 / 0 | 52 / 47 | 14 / 48 | 14 / 21 | 54 / 2512 | 13 / 13 | 2396 / 2255 |
| fb0103 | 0 / 0 | 79 / 95 | 164 / 169 | 23 / 25 | 59 / 3884 | 14 / 14 | 3784 / 4020 |
| fb0104 | 0 / 0 | 101 / 87 | 126 / 191 | 15 / 29 | 50 / 3896 | 13 / 13 | 4186 / 3947 |
| fb0105 | 0 / 0 | 53 / 87 | 16 / 56 | 17 / 24 | 62 / 3359 | 16 / 15 | 2103 / 2522 |
| fb0106 | 0 / 0 | 39 / 40 | 18 / 28 | 13 / 15 | 53 / 2242 | 13 / 13 | 2101 / 2188 |
| fb0107 | 0 / 0 | 38 / 29 | 23 / 19 | 15 / 15 | 57 / 2787 | 14 / 15 | 2177 / 2123 |
| fb0108 | 0 / 0 | 111 / 101 | 69 / 142 | 18 / 21 | 50 / 3371 | 14 / 15 | 3382 / 3865 |
| fb0109 | 0 / 0 | 47 / 40 | 54 / 23 | 19 / 15 | 50 / 2458 | 13 / 13 | 2117 / 2178 |
| fb010a | 0 / 0 | 104 / 101 | 141 / 146 | 19 / 19 | 50 / 3340 | 12 / 12 | 4142 / 4271 |
| fb0201 | 0 / 0 | 81 / 38 | 18 / 42 | 19 / 15 | 86 / 3911 | 21 / 16 | 2153 / 2146 |
| fb0202 | 0 / 0 | 44 / 32 | 50 / 39 | 19 / 17 | 51 / 2289 | 13 / 13 | 2241 / 1840 |
| fb0203 | 0 / 0 | 21 / 40 | 50 / 24 | 17 / 15 | 56 / 2330 | 15 / 13 | 2020 / 2152 |
| fb0204 | 0 / 0 | 60 / 29 | 14 / 44 | 17 / 19 | 64 / 2858 | 15 / 13 | 2256 / 1752 |
| fb0205 | 0 / 0 | 57 / 56 | 151 / 81 | 36 / 21 | 50 / 2794 | 14 / 14 | 2663 / 2688 |
| fb0206 | 0 / 0 | 51 / 48 | 6 / 11 | 15 / 15 | 58 / 2949 | 14 / 13 | 2217 / 2167 |
| fb0207 | 0 / 0 | 54 / 60 | 22 / 27 | 15 / 16 | 46 / 2219 | 13 / 13 | 2086 / 2228 |
| fb0208 | 0 / 0 | 43 / 37 | 21 / 21 | 14 / 14 | 46 / 2280 | 13 / 13 | 2236 / 2227 |
| fb0209 | 0 / 0 | 60 / 37 | 16 / 42 | 12 / 17 | 51 / 2771 | 14 / 14 | 2138 / 1637 |
| fb020a | 0 / 0 | 50 / 59 | 71 / 19 | 23 / 14 | 44 / 2691 | 13 / 13 | 2260 / 2384 |

Per metric over the 20 seeds (on minus off, per seed; the standard error is the paired spread over the square root of 20):

| metric | on mean | off mean | on - off mean | paired sd | se | seeds on lower / on higher |
|---|---|---|---|---|---|---|
| handoffs + declined closes | 0.0 | 0.0 | 0.0 | 0.0 | 0.0 | 0 / 0 |
| frames late | 58.9 | 55.4 | +3.5 | 17.4 | 3.9 | 7 / 13 |
| never-fed frames | 54.0 | 62.5 | -8.5 | 35.8 | 8.0 | 13 / 6 |
| freeze episodes | 17.7 | 18.4 | -0.8 | 6.2 | 1.4 | 10 / 6 |
| frozen ms | 1839 | 1936 | -97 | 574 | 128 | 12 / 8 |
| p95 excess latency ms | 69.0 | 64.2 | +4.9 | 20.3 | 4.5 | 7 / 12 |

Reading: no difference exceeds about one standard error, and the two sets disagree in sign (never-fed frames: -23.9 against +6.9 per run). The reason is in the last column of the table: **1087 of 57482 pad datagrams (1.9 %) were paid as repair rows**, because a window reserves at most one block's pad budget (`max(r, 4)` rows), and the run offers about 700 datagrams per second of which 5 % or more are lost. At most 50-60 rows per run can each rebuild one symbol of one block that lost more than r, a case the controller sizes to 1e-3. A benefit of dozens of frames from about 50 rows is not plausible; the chaotic divergence of the two arms after the first window (the offered datagram counts differ by up to 20 % between arms of the same seed) is. The optional client-side counter (decodes that used a row at or past r with no REPAIR_REQUEST outstanding) was not built: with this ceiling on the rows, the answer is bounded by 1087 over 20 runs, and the outcome counters above already show nothing to explain.

### Part 1, regression check against the BWE suites

Setup. `GHOSTFRAME_FEC_BLOCKS=adaptive` (read by the harness when a scene sets no mode), loss through the existing `GHOSTFRAME_OUTBOUND_BURST_*` variables: 2 % average, mean burst 1 ms, loss-realisation salts 1, 2 and 3, each against the mode-off control with the same loss. `browserless_bwe_capacity` and `browserless_h264` read the burst variables themselves, so they needed no edit. `browserless_bwe_rate_source` passes default link overrides and reads nothing, so for that suite alone a **temporary, uncommitted** three-line hook in `run_browserless_with_options` applied the burst variables when no override was given (removed afterwards; no suite was edited or forked). The sus_use and reached numbers come from `p_b_seed_population` (`GF_POP_N=6 GF_POP_ROW=0`, the suite's own printout), run at the pad-as-repair tip (`aca12df0`) and again after the revert.

**Unmodified suites, default mode (off), once, at `aca12df0`:** `browserless_bwe_capacity` 2 passed; `browserless_bwe_rate_source` 7 passed, 1 ignored; `browserless_h264` 30 passed, 11 failed, 1 ignored. The 11 failures are all `openh264_scenes::*`, which panic with "no usable openh264 library" because this machine has libopenh264 2.7.0 and the suite supports 2.4.x to 2.6.x; the test source says so, and CI installs 2.4.1. So the E8/E9 paths broke nothing the default-mode suites can see. (One lib test, `h264_direct_worker_path_survives_a_loss_bit_exact`, fails here for the same missing-encoder reason, "the worker did not execute the plan"; it was not run at an earlier commit.)

**H.264 step-up, 2 -> 6 Mbit/s, `p_b_seed_population`, 6 timelines per salt (`0x6701..06`).** Milliseconds from the step, `reached / sus_est / sus_use`, `-` = never. T_UP is 11 s.

| salt | seed | off | adaptive at `aca12df0` | adaptive after the revert |
|---|---|---|---|---|
| 1 | 6701 | 1000/12400/13900 | 6500/21100/- | 11900/-/- |
| 1 | 6702 | 900/10200/20400 | 900/4300/- | 900/2900/- |
| 1 | 6703 | 4700/4700/- | 7600/7600/- | 11700/-/- |
| 1 | 6704 | 1200/2900/2900 | 900/-/- | 900/6700/15700 |
| 1 | 6705 | 1200/2500/2500 | 700/-/- | 700/-/- |
| 1 | 6706 | 3800/3800/4100 | 1200/2900/- | 1200/-/- |
| 2 | 6701 | 1300/8700/8700 | 700/-/- | 700/-/- |
| 2 | 6702 | 1300/12400/16400 | 1500/18800/- | 1400/-/- |
| 2 | 6703 | 5700/5700/17400 | 800/-/- | 800/-/- |
| 2 | 6704 | 8800/8800/14300 | 6700/-/- | 15100/15100/- |
| 2 | 6705 | 1300/2900/2900 | 1200/-/- | 1200/-/- |
| 2 | 6706 | 1200/1200/1200 | 6500/-/- | -/-/- |
| 3 | 6701 | 5100/5100/- | 1900/7400/- | 1900/-/- |
| 3 | 6702 | 1200/4000/9300 | 900/-/- | 900/-/- |
| 3 | 6703 | 1200/1200/1200 | 1000/3300/- | 1100/-/- |
| 3 | 6704 | 5200/5200/10600 | 1200/-/- | 1200/-/- |
| 3 | 6705 | 1200/1200/1200 | 1400/3800/- | 1400/1400/- |
| 3 | 6706 | 1200/2800/2800 | 700/4900/- | 700/4400/- |

Hits inside T_UP per salt (1, 2, 3): `sus_use` off 3, 3, 5 (11 of 18), adaptive 0, 0, 0, after the revert 0, 0, 0; `sus_est` off 5, 5, 6, adaptive 3, 0, 4, after the revert 2, 0, 2; `reached` (a transient spike) 6, 6, 6 for off and adaptive, 4, 4, 6 after the revert. **This is a regression outside the seed spread, and it is not padding**: removing E8 and E9 does not change it. With no loss the two modes are identical (6 timelines: `reached` 6, `sus_est` 4, `sus_use` 4 for both), as the plan's golden says. It appears only when adaptive mode has engaged on loss. **Open item for E11 / the owner, not fixed here:** the likely mechanism is the encoder-target scaling of E6 (`media = RATE_SHARE x estimate / (1 + rho)`), because `sus_use` counts only the access units' bytes against 0.8 of the line, so a stream that spends part of its grant on repairs cannot read as "using" the headroom; whether the estimate itself is also lower is what `sus_est` (worse as well) and the next table say. That reading is a hypothesis; I did not isolate E6.

**Capacity, `browserless_bwe_capacity`** (2 % loss, 3 salts; the two scenes' assertions assume no loss, `the_estimate_separates_...` fails its "spacious link has no drops" premise in both arms by construction, so the numbers are read, not the verdicts). Step-up scene, `converged_at` ms and tail median Mbit/s: off 1800 / 2300 / 2300 and 10.3 / 11.0 / 13.1; adaptive 300 / 1200 / 1500 and 17.0 / 17.4 / 12.4. Adaptive converges earlier and holds a higher estimate under loss in every salt (the repairs keep the loss-based estimator from reading every loss as congestion). No regression.

**Rate source, `browserless_bwe_rate_source`** (tile load; every H.264 pad path, including E8 and E9, is unreachable there, `h264_probe_pad` has one caller on the H.264 frame path). Off: 7 of 7 pass at all three salts. Adaptive: 6 of 7, `p6c_codel_c10...` failing at all three salts. Its settled median over five seeds is 9.2-9.8 Mbit/s off and 7.9-8.3 Mbit/s adaptive on the 10 Mbit/s link; its assertion is "settled over (cap x payload fraction)" at most 1.15, and adaptive reads 1.16, 1.23, 1.21 (mean per salt, off 0.97). The settled estimate is therefore lower under adaptive, and the assertion's denominator (payload fraction) does not count the repair bytes the numerator's link carries, so the unit is not like for like. p6b reaches 80 % of the step in 0.5 s adaptive against 1.7-2.0 s off. Not padding; reported for E11 with the sus_use item.

Part 1 verdict: E8 and E9 are not responsible for any difference; adaptive mode under loss is worse than the control on `sus_use` and `sus_est` of the H.264 step-up and on the rate-source settled estimate, better on capacity convergence. The first two are outside the seed spread and need a decision before default-on (E13).

### What changed
- Deleted: E9 (`h264_probe_pad` reservation, the close-time top-up, the deadline, the counters `probe_padding_repair_datagrams` / `_bytes`), E8's `reserve_pad_rows`, `RepairJob::probe`, the `bundle_seq -> ProbeTag` map and the `ResolvedAck` id.
- Plan decision 8 is superseded: padding stays PLAIN copies of an acknowledged fragment in every mode, and the byte-identity of knob off, r = 0 and padding is automatic.

## 10. R6-e (plan task E11): the control-law sweep

**Verdict: the default-on commit (E13) stays blocked.** Criteria (2) and the premises hold everywhere that was run; (1) and (4) hold at 10 Mbit/s and above and fail at 2 Mbit/s H.264 (start-up tail drops); (3), (5) and (6) fail in measured cells, for reasons that are not a mis-tuned constant (below). No constant was tuned. The proposed changes are at the end and are the owner's to take or leave.

Tested at `bf2a7b1c` on `362f6a4f` (counters) on `df4c8cc6`: `ghostframe-e2e/tests/browserless_fec_adapt.rs::control_law_sweep`. The reduced grid (H.264, 10 Mbit/s, six losses, 3 seeds, widened to 5 where a hand-off criterion fails) runs in CI in about 75 s; `GHOSTFRAME_FEC_SWEEP=full` runs the whole grid, `GHOSTFRAME_FEC_SWEEP_ONLY=<comma-separated substrings of a cell name>` narrows it. The test fails on the premises and on criteria 1, 2 and 4 (the ones that hold at the CI cell); 3, 5 and 6 print `FAIL` and are open findings, not gates, so that CI says what is measured without being red over a decision that is the owner's.

### 10.0 What was run, and what was not

- **Grid:** loss {clean, 0.01, 0.1, 1, 5 % i.i.d., Gilbert-Elliott per-datagram chain, mean run 3 at 1 % on average} x cap {2, 10, 30, 100 Mbit/s} x load {H.264, tile} x arm {mode off, fixed `64,5,2`, adaptive}. The link is the cap behind a 150 ms tail-drop buffer, 50 ms round trip, loss on the frame path. H.264: the PCM encoder with the rate policy (frames follow the estimate, as in the probe suites). Tile: a damage window offering 0.8 x the cap. 30 s per run, 60 s at 0.01 % and 0.1 % (changes per minute, and enough losses to be a statistical claim).
- **Seeds:** 3 per cell (`0xFB0001..3`). Widened to 5 (`..5`) where the spread of hand-offs + declines is wide (mean >= 5 and sd > mean) or a hand-off criterion (2, 3) failed on counts this small: H.264 at 2, 10, 30 Mbit/s and tile at 2 Mbit/s were re-run with that rule (cells show their seed count). H.264 at 100 Mbit/s and tile at 10 Mbit/s stayed at 3 seeds (cost). "No higher than" means: the adaptive mean at most the control's mean plus the larger of the two standard deviations over the seeds. Browserless in stepped mode is byte-reproducible per seed, so a repeated seed adds nothing; the seeds are the only spread there is.
- **Cut for time (the 2 h rule):** **tile at 30 Mbit/s and at 100 Mbit/s** (the tile step scenes at 30 Mbit/s ran about four times slower than at 10 Mbit/s, where a 30 s run takes about 40 s; the 54 runs of that rate would have taken more than two hours). Only the criterion (4) step scenes ran at tile 30 Mbit/s (3 seeds); tile at 100 Mbit/s has no data. Everything else of the grid ran: 36 cells x 3 arms, plus the step scenes.
- **Measures.** *Overhead* (bytes): repair bytes offered over the other offered bytes. *r/k*: planned repair rows over the source symbols of the blocks sized (the quantity the rule controls; datagram units). *Closes*: `CloseReason` counted at the table (`Accounted`, `Declined`, `HandedOff`, `TimedOut`); *hand-offs + declines* is the E10 unit (`block_handoffs + block_closed_declined`). *Frames late* (H.264): the client's first decode more than 100 ms (an RTT) over the stream's own fastest frame. *Retransmits per lost datagram*: `retransmit_attempts_total` over datagrams the link dropped (for tile it counts refinement re-sends too, so a ratio above 1 is not "per datagram"; read it only against the off arm). *Oracle:* the exact tail of the block under the i.i.d. or Gilbert-Elliott chain, restated in the test and checked against the closed-form binomial; the rule's overhead is that rule applied to the block sizes the run actually closed (a 24-bucket size histogram from the bridge), and the cap's is `ceil(k/4)` (<= 32) on the same sizes.
- **Premises (every cell, all arms): all hold.** The off arm sends no repair and sizes no block; the fixed arm sends repair (it always has blocks); every arm received LOSS_REPORTs; where the loss is at or above 0.1 % datagrams were dropped in every arm and the reports counted lost datagrams; where it is at or above 1 % the adaptive arm sent repair. Criterion (1)'s digest comparison is live: pointing it at the fixed arm instead of off made it fail on the clean cell.

### 10.1 Criteria

**(1) Clean link and 0.01 %: no repair, frames equal to the mode off.**

| cell | adaptive repair bytes per seed | wire digest equals off | datagrams dropped per seed | verdict |
|---|---|---|---|---|
| H264/clean/100M | 0, 0, 0 | true | 0, 0, 0 | pass |
| H264/clean/10M | 0, 0, 0 | true | 0, 0, 0 | pass |
| H264/clean/2M | 1221667, 0, 1386009 | false | 34, 31, 33 | **FAIL** |
| H264/clean/30M | 0, 0, 0 | true | 0, 0, 0 | pass |
| H264/iid0.01/100M | 0, 0, 0 | true | 25, 29, 33 | pass |
| H264/iid0.01/10M | 0, 0, 0 | true | 5, 0, 4 | pass |
| H264/iid0.01/2M | 2485376, 0, 2420097 | false | 35, 31, 34 | **FAIL** |
| H264/iid0.01/30M | 0, 0, 0 | true | 9, 8, 12 | pass |
| Tile/clean/10M | 0, 0, 0 | true | 0, 0, 0 | pass |
| Tile/clean/2M | 0, 0, 0 | true | 0, 0, 0 | pass |
| Tile/iid0.01/10M | 0, 0, 0 | true | 6, 1, 5 | pass |
| Tile/iid0.01/2M | 0, 0, 0 | true | 2, 0, 1 | pass |

**(2) Stationary 0.1 %: at most 2 changes per minute, overhead at most fixed's, retransmits no worse than off.**

| cell | changes/min per seed | overhead adaptive / fixed (spread) | retransmits adaptive / off (spread) | verdict |
|---|---|---|---|---|
| H264/iid0.1/100M | 0.00, 0.00, 0.00 | 0.0000 / 0.0427 (0.0000) | 0 / 0 (0) | pass |
| H264/iid0.1/10M | 0.00, 0.00, 0.00 | 0.0000 / 0.3219 (0.0029) | 0 / 0 (0) | pass |
| H264/iid0.1/2M | 1.00, 0.00, 1.00 | 0.1891 / 1.6389 (0.1345) | 0 / 0 (0) | pass |
| H264/iid0.1/30M | 0.00, 0.00, 0.00 | 0.0000 / 0.1274 (0.0051) | 0 / 0 (0) | pass |
| Tile/iid0.1/10M | 1.00, 2.00, 1.00 | 0.0533 / 0.4810 (0.0302) | 533 / 806 (206) | pass |
| Tile/iid0.1/2M | 0.00, 0.00, 0.00 | 0.0000 / 1.3927 (0.0013) | 100 / 100 (17) | pass |

**(3) 1 %, 5 % i.i.d. and bursty 1 %: hand-offs + declines no higher than fixed `64,5,2`; overhead (planned repair rows over source symbols, per datagram) within 1.25 x the DP rule, or the cap's where the saturated share is >= 0.5.**

| cell | seeds | hand-offs+declines adaptive / fixed (spread) | overhead r/k | bar | saturated share | trim steps (per seed) | verdict |
|---|---|---|---|---|---|---|---|
| H264/ge3at1/100M | 3 | 1.0 / 190.0 (6.7) ok | 0.1728 ok | 1.25 x dp 0.2049 | 0.09 | 0, 0, 0 | pass |
| H264/ge3at1/10M | 5 | 6.2 / 18.6 (3.3) ok | 0.2948 **over** | cap 0.2593 | 0.98 | 1, 1, 1, 1, 1 | **FAIL** |
| H264/ge3at1/2M | 5 | 6.2 / 0.6 (1.9) **worse** | 0.3557 **over** | cap 0.2886 | 1.00 | 2, 2, 1, 1, 1 | **FAIL** |
| H264/ge3at1/30M | 5 | 2.4 / 66.4 (8.1) ok | 0.2520 **over** | cap 0.2502 | 0.58 | 0, 0, 0, 0, 0 | **FAIL** |
| H264/iid1/100M | 3 | 0.7 / 37.3 (1.9) ok | 0.0829 ok | 1.25 x dp 0.1076 | 0.00 | 0, 0, 0 | pass |
| H264/iid1/10M | 5 | 1.0 / 0.4 (0.6) ok | 0.1970 ok | 1.25 x dp 0.2545 | 0.00 | 0, 0, 0, 0, 0 | pass |
| H264/iid1/2M | 3 | 0.3 / 0.0 (0.5) ok | 0.2713 ok | cap 0.2713 | 1.00 | 0, 0, 0 | pass |
| H264/iid1/30M | 3 | 1.3 / 3.0 (1.4) ok | 0.1467 ok | 1.25 x dp 0.1818 | 0.01 | 0, 0, 0 | pass |
| H264/iid5/100M | 3 | 3.0 / 1229.0 (87.8) ok | 0.1815 ok | 1.25 x dp 0.2461 | 0.09 | 0, 0, 0 | pass |
| H264/iid5/10M | 5 | 9.2 / 18.4 (3.3) ok | 0.3324 **over** | cap 0.2928 | 0.84 | 4, 3, 3, 3, 1 | **FAIL** |
| H264/iid5/2M | 5 | 7.8 / 0.6 (1.8) **worse** | 0.4078 **over** | cap 0.2833 | 1.00 | 3, 2, 2, 2, 3 | **FAIL** |
| H264/iid5/30M | 3 | 6.0 / 230.0 (39.8) ok | 0.2357 ok | cap 0.2634 | 0.59 | 0, 2, 0 | pass |
| Tile/ge3at1/10M | 3 | 312.0 / 1519.7 (258.4) ok | 0.2671 **over** | cap 0.2507 | 0.98 | 3, 0, 0 | **FAIL** |
| Tile/ge3at1/2M | 5 | 181.0 / 96.6 (49.3) **worse** | 0.3758 **over** | cap 0.3121 | 0.99 | 2, 1, 1, 1, 1 | **FAIL** |
| Tile/iid1/10M | 3 | 3.3 / 5.3 (4.5) ok | 0.2021 ok | 1.25 x dp 0.2134 | 0.13 | 0, 0, 0 | pass |
| Tile/iid1/2M | 3 | 7.7 / 4.3 (6.1) ok | 0.2527 ok | 1.25 x dp 0.3827 | 0.31 | 0, 0, 0 | pass |
| Tile/iid5/10M | 3 | 394.7 / 939.3 (168.6) ok | 0.2897 **over** | cap 0.2808 | 0.93 | 3, 2, 1 | **FAIL** |
| Tile/iid5/2M | 5 | 88.8 / 39.0 (24.3) **worse** | 0.3446 **over** | cap 0.3106 | 0.99 | 0, 2, 1, 2, 1 | **FAIL** |

**(4) Engage within 1 s of a 5 % loss step (the step at 5 s; milliseconds to the engaged state, bisected to 50 ms).**

| cell | per seed | verdict |
|---|---|---|
| H264/step5/100M | 234 ms, 187 ms, 234 ms | pass |
| H264/step5/10M | 703 ms, 703 ms, 703 ms | pass |
| H264/step5/2M | engaged before the step, 1031 ms, engaged before the step | **FAIL** |
| H264/step5/30M | 281 ms, 281 ms, 281 ms | pass |
| Tile/step5/10M | 609 ms, 421 ms, 750 ms | pass |
| Tile/step5/2M | 703 ms, 1687 ms, 1406 ms | **FAIL** |
| Tile/step5/30M | 328 ms, 328 ms, 328 ms | pass |

**(5) 5 % i.i.d. is never congestion-gated (`adapt_congestion_skips == 0`).** Skips against LOSS_REPORTs received, adaptive arm:

| cell | skips per seed | verdict |
|---|---|---|
| H264/iid5/100M | 21, 22, 25 | **FAIL** |
| H264/iid5/10M | 12, 12, 16, 24, 16 | **FAIL** |
| H264/iid5/2M | 11, 10, 11, 8, 10 | **FAIL** |
| H264/iid5/30M | 42, 33, 37 | **FAIL** |
| Tile/iid5/10M | 50, 52, 60 | **FAIL** |
| Tile/iid5/2M | 29, 29, 33, 20, 31 | **FAIL** |

Share of reports gated, summed over seeds: H.264 2 Mbit/s 8.4 %, 10 Mbit/s 9.8 %, 30 Mbit/s 7.0 %, 100 Mbit/s 1.9 %; tile 2 Mbit/s 23.5 %, 10 Mbit/s 15.6 %. The controller was engaged in every one of these runs.

**(6) The BWE ceiling binds no more than under the control** (`ceiling_binding_reads / ceiling_reads`, mean over seeds, adaptive against off, over the larger of the two standard deviations).

| cell | adaptive | off | spread | verdict |
|---|---|---|---|---|
| H264/clean/100M | 0.0016 | 0.0016 | 0.0012 | pass |
| H264/clean/10M | 0.0042 | 0.0042 | 0.0025 | pass |
| H264/clean/2M | 0.0311 | 0.0042 | 0.0158 | **FAIL** |
| H264/clean/30M | 0.0049 | 0.0049 | 0.0017 | pass |
| H264/ge3at1/100M | 0.0044 | 0.0000 | 0.0019 | **FAIL** |
| H264/ge3at1/10M | 0.0176 | 0.0051 | 0.0094 | **FAIL** |
| H264/ge3at1/2M | 0.0256 | 0.0042 | 0.0066 | **FAIL** |
| H264/ge3at1/30M | 0.0073 | 0.0045 | 0.0048 | pass |
| H264/iid0.01/100M | 0.0008 | 0.0008 | 0.0006 | pass |
| H264/iid0.01/10M | 0.0042 | 0.0042 | 0.0007 | pass |
| H264/iid0.01/2M | 0.0170 | 0.0040 | 0.0091 | **FAIL** |
| H264/iid0.01/30M | 0.0031 | 0.0031 | 0.0008 | pass |
| H264/iid0.1/100M | 0.0003 | 0.0003 | 0.0004 | pass |
| H264/iid0.1/10M | 0.0042 | 0.0042 | 0.0013 | pass |
| H264/iid0.1/2M | 0.0142 | 0.0026 | 0.0098 | **FAIL** |
| H264/iid0.1/30M | 0.0159 | 0.0159 | 0.0140 | pass |
| H264/iid1/100M | 0.0024 | 0.0005 | 0.0018 | **FAIL** |
| H264/iid1/10M | 0.0163 | 0.0038 | 0.0106 | **FAIL** |
| H264/iid1/2M | 0.0244 | 0.0052 | 0.0083 | **FAIL** |
| H264/iid1/30M | 0.0115 | 0.0006 | 0.0070 | **FAIL** |
| H264/iid5/100M | 0.0026 | 0.0001 | 0.0004 | **FAIL** |
| H264/iid5/10M | 0.0236 | 0.0093 | 0.0077 | **FAIL** |
| H264/iid5/2M | 0.0509 | 0.0078 | 0.0201 | **FAIL** |
| H264/iid5/30M | 0.0062 | 0.0070 | 0.0064 | pass |
| Tile/clean/10M | 0.0109 | 0.0109 | 0.0005 | pass |
| Tile/clean/2M | 0.0000 | 0.0000 | 0.0000 | pass |
| Tile/ge3at1/10M | 0.1142 | 0.0107 | 0.1017 | **FAIL** |
| Tile/ge3at1/2M | 0.1642 | 0.0002 | 0.0095 | **FAIL** |
| Tile/iid0.01/10M | 0.0053 | 0.0053 | 0.0002 | pass |
| Tile/iid0.01/2M | 0.0000 | 0.0000 | 0.0000 | pass |
| Tile/iid0.1/10M | 0.0050 | 0.0050 | 0.0005 | pass |
| Tile/iid0.1/2M | 0.0000 | 0.0000 | 0.0000 | pass |
| Tile/iid1/10M | 0.0394 | 0.0082 | 0.0099 | **FAIL** |
| Tile/iid1/2M | 0.2140 | 0.0000 | 0.0275 | **FAIL** |
| Tile/iid5/10M | 0.2345 | 0.0070 | 0.0072 | **FAIL** |
| Tile/iid5/2M | 0.2217 | 0.0121 | 0.0399 | **FAIL** |

**(7) At 2 Mbit/s: overhead against the 28 ms design value** (reported, not judged). `r/k` as sent; the DP rule at the mean `k` the run actually closed; the DP rule at the `k` a 28 ms block would hold at the run's source rate.

| cell | adaptive r/k | DP at the k it ran | DP at 28 ms (k) | source datagrams/s |
|---|---|---|---|---|
| H264/ge3at1/2M | 0.3557 | 0.2461 (k=4.1) | 0.4000 (k=5) | 164 |
| H264/iid0.01/2M | 0.2020 | 0.4033 (k=2.5) | 0.2000 (k=5) | 164 |
| H264/iid0.1/2M | 0.1837 | 0.3920 (k=2.6) | 0.2000 (k=5) | 167 |
| H264/iid1/2M | 0.2713 | 0.2420 (k=4.1) | 0.4000 (k=5) | 169 |
| H264/iid5/2M | 0.4078 | 0.2623 (k=3.8) | 0.2500 (k=4) | 154 |
| Tile/ge3at1/2M | 0.3758 | 0.3034 (k=6.6) | 0.3333 (k=6) | 211 |
| Tile/iid0.01/2M | 0.0000 | 1.0000 (k=1.0) | 0.1111 (k=9) | 304 |
| Tile/iid0.1/2M | 0.0000 | 1.0000 (k=1.0) | 0.1250 (k=8) | 303 |
| Tile/iid1/2M | 0.2527 | 0.3121 (k=6.4) | 0.2857 (k=7) | 238 |
| Tile/iid5/2M | 0.3446 | 0.3045 (k=6.6) | 0.3333 (k=6) | 208 |

### 10.2 Per cell (means over the seeds shown)

Overhead is repair over other offered bytes. "Declined + handed-off + timed-out" is the share of closed blocks that did not end `Accounted`. Frames late is H.264 only. The adaptive columns for the clean, 0.01 % and (mostly) 0.1 % cells show a controller that stayed off.

| cell | seeds | overhead (repair/source bytes) off / fixed / adaptive | declined+handed-off closes (share of closed blocks) fixed / adaptive | frames late (H.264) off / fixed / adaptive | retransmits per lost dg off / fixed / adaptive | adapt changes/min | saturated share | engaged |
|---|---|---|---|---|---|---|---|---|
| H264/clean/2M | 3 | 0.000 / 1.627 / 0.201 | 0.00% / 0.06% | 11 / 174 / 16 | 0.000 / 0.000 / 0.000 | 1.3 | 0.67 | 2/3 |
| H264/iid0.01/2M | 3 | 0.000 / 1.683 / 0.203 | 0.00% / 0.23% | 16 / 332 / 21 | 0.000 / 0.000 / 0.000 | 0.7 | 0.67 | 2/3 |
| H264/iid0.1/2M | 3 | 0.000 / 1.639 / 0.189 | 0.02% / 0.06% | 10 / 322 / 16 | 0.000 / 0.000 / 0.000 | 0.7 | 0.67 | 2/3 |
| H264/iid1/2M | 3 | 0.000 / 1.631 / 0.255 | 0.02% / 0.04% | 10 / 154 / 18 | 0.003 / 0.000 / 0.003 | 2.0 | 1.00 | 3/3 |
| H264/iid5/2M | 5 | 0.000 / 1.514 / 0.401 | 0.07% / 0.95% | 5 / 204 / 10 | 0.003 / 0.000 / 0.003 | 2.0 | 1.00 | 5/5 |
| H264/ge3at1/2M | 5 | 0.000 / 1.568 / 0.335 | 0.28% / 0.95% | 12 / 139 / 16 | 0.000 / 0.000 / 0.000 | 2.0 | 1.00 | 5/5 |
| H264/clean/10M | 3 | 0.000 / 0.324 / 0.000 | 0.00% / - | 6 / 16 / 6 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid0.01/10M | 3 | 0.000 / 0.319 / 0.000 | 0.00% / - | 12 / 32 / 12 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid0.1/10M | 3 | 0.000 / 0.322 / 0.000 | 0.00% / - | 9 / 32 / 9 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid1/10M | 5 | 0.000 / 0.304 / 0.177 | 0.02% / 0.06% | 4 / 22 / 11 | 0.002 / 0.000 / 0.002 | 21.2 | 0.00 | 5/5 |
| H264/iid5/10M | 5 | 0.000 / 0.313 / 0.308 | 0.85% / 0.61% | 3 / 25 / 9 | 0.001 / 0.000 / 0.001 | 19.2 | 0.84 | 5/5 |
| H264/ge3at1/10M | 5 | 0.000 / 0.326 / 0.254 | 1.03% / 0.82% | 6 / 16 / 9 | 0.000 / 0.000 / 0.000 | 2.0 | 0.99 | 5/5 |
| H264/clean/30M | 3 | 0.000 / 0.136 / 0.000 | 0.00% / - | 1 / 5 / 1 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid0.01/30M | 3 | 0.000 / 0.132 / 0.000 | 0.00% / - | 3 / 5 / 3 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid0.1/30M | 3 | 0.000 / 0.127 / 0.000 | 0.00% / - | 9 / 12 / 9 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid1/30M | 3 | 0.000 / 0.127 / 0.139 | 0.10% / 0.04% | 3 / 13 / 8 | 0.001 / 0.000 / 0.001 | 3.3 | 0.01 | 3/3 |
| H264/iid5/30M | 3 | 0.000 / 0.128 / 0.228 | 8.28% / 0.26% | 1 / 3 / 8 | 0.000 / 0.000 / 0.000 | 5.3 | 0.59 | 3/3 |
| H264/ge3at1/30M | 5 | 0.000 / 0.128 / 0.239 | 2.71% / 0.29% | 3 / 8 / 10 | 0.000 / 0.000 / 0.000 | 2.0 | 0.58 | 5/5 |
| H264/clean/100M | 3 | 0.000 / 0.044 / 0.000 | 0.00% / - | 1 / 1 / 1 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid0.01/100M | 3 | 0.000 / 0.043 / 0.000 | 0.00% / - | 1 / 1 / 1 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid0.1/100M | 3 | 0.000 / 0.043 / 0.000 | 0.00% / - | 1 / 1 / 1 | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| H264/iid1/100M | 3 | 0.000 / 0.044 / 0.081 | 1.42% / 0.02% | 1 / 1 / 1 | 0.001 / 0.000 / 0.001 | 4.7 | 0.00 | 3/3 |
| H264/iid5/100M | 3 | 0.000 / 0.059 / 0.178 | 46.51% / 0.13% | 1 / 1 / 1 | 0.000 / 0.000 / 0.000 | 6.7 | 0.09 | 3/3 |
| H264/ge3at1/100M | 3 | 0.000 / 0.048 / 0.168 | 7.22% / 0.10% | 1 / 1 / 1 | 0.000 / 0.000 / 0.000 | 13.3 | 0.09 | 3/3 |
| Tile/clean/2M | 3 | 0.000 / 1.381 / 0.000 | 0.00% / - | n/a | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| Tile/iid0.01/2M | 3 | 0.000 / 1.390 / 0.000 | 0.00% / - | n/a | 6.167 / 0.000 / 6.167 | 0.0 | 0.00 | 0/3 |
| Tile/iid0.1/2M | 3 | 0.000 / 1.393 / 0.000 | 0.00% / - | n/a | 7.208 / 0.000 / 7.208 | 0.0 | 0.00 | 0/3 |
| Tile/iid1/2M | 3 | 0.000 / 1.368 / 0.409 | 0.01% / 0.10% | n/a | 10.632 / 0.043 / 1.000 | 2.0 | 0.31 | 3/3 |
| Tile/iid5/2M | 5 | 0.000 / 1.350 / 0.581 | 0.23% / 0.62% | n/a | 9.159 / 0.080 / 0.372 | 2.0 | 0.99 | 5/5 |
| Tile/ge3at1/2M | 5 | 0.000 / 1.377 / 0.499 | 0.42% / 1.19% | n/a | 8.882 / 0.999 / 3.139 | 2.0 | 0.99 | 5/5 |
| Tile/clean/10M | 3 | 0.000 / 0.476 / 0.000 | 0.00% / - | n/a | 0.000 / 0.000 / 0.000 | 0.0 | 0.00 | 0/3 |
| Tile/iid0.01/10M | 3 | 0.000 / 0.480 / 0.000 | 0.00% / - | n/a | 14.556 / 0.000 / 14.556 | 0.0 | 0.00 | 0/3 |
| Tile/iid0.1/10M | 3 | 0.000 / 0.481 / 0.053 | 0.00% / 0.00% | n/a | 15.825 / 0.058 / 10.682 | 1.3 | 0.07 | 3/3 |
| Tile/iid1/10M | 3 | 0.000 / 0.477 / 0.244 | 0.02% / 0.04% | n/a | 15.983 / 0.018 / 0.239 | 4.0 | 0.13 | 3/3 |
| Tile/iid5/10M | 3 | 0.000 / 0.420 / 0.442 | 1.33% / 0.53% | n/a | 13.347 / 0.469 / 0.323 | 96.0 | 0.93 | 3/3 |
| Tile/ge3at1/10M | 3 | 0.000 / 0.478 / 0.347 | 1.19% / 0.54% | n/a | 15.854 / 3.990 / 1.275 | 2.0 | 0.98 | 3/3 |

### 10.3 Findings

1. **At 2 Mbit/s the "clean" link is not clean, and adaptive engages on it (criteria 1 and 4).** The stream's start-up overflows the 150 ms buffer: 31 to 35 datagrams are dropped per run in every arm, the mode off included. The controller engages on them (1.2 to 2.5 MB of repair per run, a 20 % overhead on a link with no injected loss), the frames then differ from the off run, and the step scene finds it already engaged at 5 s and still at 25 s in 2 of 3 seeds (it releases slowly at this rate). The attribution is direct: with the buffer at 1500 ms the same cell drops 0, adaptive stays off and criterion (1) passes (3 seeds). This is the known limit of the standing-queue gate (decision 4): by the time the report arrives the queue has drained, so the loss reads as random. At 10 Mbit/s and above the same start-up does not overflow and (1) holds in every cell, tile included.
2. **Criterion (3)'s overhead bar fails only where the closed-loop trim is on, and what trips the trim is not an undersized `r`.** Every failing overhead cell but one has `trim_steps` >= 1 (H.264 2 and 10 Mbit/s at 5 % and bursty, tile 2 and 10 Mbit/s likewise); the cells with `trim_steps` 0 pass, and the one near-miss with 0 steps (H.264 30 Mbit/s bursty, 0.2520 against 0.2502) is bucket rounding. The trim adds one row beyond the `ceil(k/4)` cap (plan decision 6), so the saturated bar, which is the cap, is exceeded by construction once it is on. The investigation of the 63 % trim activity (10.4) finds the declines that switch it on: a block that needs a repair request is declined whenever the requested row returns after the client's constant 43 ms window, which at a 40 ms round trip is most of the time (6 of 8 requests at i.i.d. 2 %, 39 of 58 under the 1 ms bursts). More parity cannot help a row that arrives late.
3. **At 2 Mbit/s adaptive has more hand-offs and declines than fixed `64,5,2`, at a quarter to a half of its bytes** (H.264 5 %: 7.8 against 0.6; bursty: 6.2 against 0.6; tile 5 %: 88.8 against 39.0; tile bursty: 181.0 against 96.6, all outside the spread). At this rate a frame is about 5 datagrams, so a block is `k` 1 to 6 and fixed's `r = 2` is 150 to 165 % overhead (measured 1.51 to 1.69); adaptive spends 34 to 58 %. The criterion "no higher than fixed" compares against a control that spends two to four times the bytes, so at 2 Mbit/s it is a statement about overhead more than about the control law. At 10 Mbit/s and above adaptive's hand-offs + declines are no higher than fixed's wherever the overhead criterion's cell was not otherwise failed: at 100 Mbit/s adaptive 0.7 to 3.0 against fixed 37 to 1229, at a higher overhead (0.08 to 0.18 against 0.04 to 0.06: fixed's `r = 2` is thin at `k` up to 64).
4. **Criterion (5) fails in every 5 % i.i.d. cell that has a bottleneck**, and the controller was engaged in all of them: 8 to 24 % of the reports at 2 to 30 Mbit/s were gated (1.9 % at 100 Mbit/s), more on tile (15.6 and 23.5 %). The standing queue that gates them is, presumably (not isolated), the stream's own: probe windows and pad bursts from a rate-following stream behind a 150 ms buffer. The unconstrained-link scene (E7) passes with 0 skips; this is the first measurement with a bottleneck. The harm is bounded (a gated interval's losses are not counted, so `p_hat` is a little low and the state stays engaged), but it is not zero as the criterion is written.
5. **Criterion (6): adaptive's BWE ceiling binds more than the control's, outside the seed spread, in 19 of 36 cells; in the other 17 it is equal or inside the spread.** Absolute shares are small for H.264 (0.2 to 5 % against 0 to 1 %) and large for tile under loss (11 to 23 % against 0 to 2 %). The 17 passing cells are the clean, 0.01 % and (mostly) 0.1 % ones, where adaptive is off and the arms are identical. **Not diagnosed here.** It is the same direction as the E10 finding (step-up under loss worse than the control) and needs its own look before default-on.
6. **State churn under loss is high where it is not asserted.** Adaptive changes per minute: 19 to 21 at H.264 10 Mbit/s with 1 % and 5 % i.i.d. loss, 96 at tile 10 Mbit/s with 5 %, 0 to 13 elsewhere. Criterion (2) bounds it only at 0.1 %, where it holds (<= 2 in every run: 0 to 1 for H.264, 0 to 2 for tile).
7. **Criterion (7), the 28 ms design value, does not show at these rates because `T` is not what closes the blocks.** At 2 Mbit/s H.264 the source is about 165 datagrams/s: `k` is 2.5 to 4, about one frame's datagrams (164 / 30 = 5.5; the close cause was not counted per block here), while a 28 ms block would hold about 5, so `T = 20` and `T = 28` give almost the same blocks. The measured r/k (0.18 to 0.41) is at or below the DP rule on the sizes it ran in the unsaturated cells and above it where the trim is on. E1's +5 to +7 points were a simulation of time-closed blocks; if the packer closes at the frame's end, as the `k` suggests, that removes them at these rates.
8. **Criterion (4) holds from 10 Mbit/s up and fails at 2:** H.264 703 ms at 10, 281 ms at 30, 187 to 234 ms at 100; tile 421 to 750 ms at 10, 328 ms at 30; at 2 Mbit/s tile 703, 1406 and 1687 ms (the report interval is 250 ms and a 2 Mbit/s stream puts few datagrams in each, so the floor test's lower bound plausibly needs more reports; not isolated) and H.264 1031 ms in the one seed where it was not already engaged at start-up.

### 10.4 The decline rate behind the 63 % trim activity (investigation done before the sweep)

**Question.** `p_b_seed_population`, adaptive, 2 % loss, 2 to 6 Mbit/s: `DeclinedTrim` active on 63 % of blocks, where i.i.d. 2 % at `k` about 8 and `r` 2 to 3 should decline about 0.1 %.

**Answer: two causes, neither a defect to fix here.**

1. *The loss in that run is not i.i.d. per datagram.* The `GHOSTFRAME_OUTBOUND_BURST_*` variables drive a time-denominated Gilbert-Elliott chain (a 1 ms mean burst), and a pacer train leaves in microseconds, so one bad interval takes a whole run of consecutive datagrams. Measured (temporary instrumentation, 3 seeds, 2317 blocks): 39 declined (1.7 %, above the 1 % that arms the trim); `r` was 4 for 65 % of all blocks (`k` 9 to 12: the `ceil(k/4)` of 3 plus the trim); the declined blocks' missing bitmaps are `0x1f` and `0x3f` in 20 of 28 cases in the first run, that is the first 5 or 6 sources of the block, while its repairs (typically all 4) arrived. With true per-datagram i.i.d. 2 % (`NetProfile::loss`) the same scene declines 6 of 2387 blocks in the first three seeds (0.25 %) and none in three further ones (6 of 4943, 0.12 %), against the 0.1 % expected; the trim needs 3 declines inside 256 blocks and is not on. A 5-in-9 loss is beyond any `r` the rule may pick.
2. *In both runs the final decline is the constant 43 ms client window, not the parity.* Every block that was declined had asked for a repair (`requested = true`), and in every one of them the requested row **arrived after the client had given up**: 0.9 to 7.9 ms after at i.i.d. 2 % (6 of 6 declines: 0.9, 1.5, 1.9, 2.5, 2.6, 7.9 ms), 0.9 to 21 ms after under the bursts (11 of 11 in the one seed read, median 2.7 ms). Requests that reached the client: 8 at i.i.d. (6 declined, 75 %), 58 under bursts (39 declined, 67 %). The client closes a block 43 ms (`close_at = first arrival + 33 + 10 ms`) after its first datagram, asks for one more row, and gives the block up 43 ms after the request; at a 40 ms round trip plus the repair's turn in the pacer and the link the row returns about 44 to 51 ms after the request. This is the constant client window that the advertised-timing follow-up (R6-e2) is meant to remove. Quantified, not fixed. Not isolated separately, because the lateness explains every decline observed: a block spanning frames, REPAIR_REQUEST timing (it is sent at the window's end by construction), and something counted as `Declined` that is not (the table's `Declined` is only the client's DECLINED record, which the client's `give_up` alone produces).
3. A related effect on the sweep: the `Declined` that trips the trim in the sweep's 5 % and bursty cells is this same late-row decline, and the trim's extra row does not shorten the round trip (finding 2 above).

### 10.5 Proposed changes (not made; the owner decides)

- **Trim.** Either (a) cap `r_for + trim` at the `ceil(k/4)` cap (the trim then cannot exceed the saturated bar; it can still raise `r` where there is room), or (b) count a block as `Declined` for the trim only if it was declined *without* an outstanding request that returned after the window (needs the client to say so, i.e. R6-e2), or (c) leave it and restate criterion (3)'s saturated bar as "the cap plus the trim row". (a) is a small change in the sizer; (b) removes the cause.
- **Gate (criteria 1, 4 at 2 Mbit/s; 5).** The standing-queue gate sees a queue that has drained, or the stream's own probe queue. A gate on the *datagrams' own* queueing (the departure ledger already stamps departures: skip losses of datagrams that were sent into a non-empty queue) would catch the start-up overflow and would not skip loss that is random. This is a change of mechanism, not of a constant.
- **Criteria as written.** (3) at 2 Mbit/s compares with a control that spends two to four times the bytes: compare at equal overhead (hand-offs + declines per repair byte), or restate it for 10 Mbit/s and above. (5) "never gated" cannot hold behind a bottleneck with a rate-following stream: restate as "engaged in every run and the gated share of reports under X %", X from this table (it is 1.9 to 23.5 %, unlike the unconstrained 0).
- **Criterion (6)** needs a diagnosis, not a proposal: find what binds the ceiling more under adaptive (the repair bytes in the acknowledged-rate window and the sending-rate estimate are the first suspects, as in E10).
- **Sequence.** R6-e2 (advertised timing) first removes the cause of finding 2; then re-run this sweep (`GHOSTFRAME_FEC_SWEEP=full`, about an hour without tile at 30 and 100 Mbit/s) before E13.

## 11. R6-e2 (plan task TB0): where the tile repair bytes go

Measurement only (`the_repair_bytes_are_split_by_cause` in `browserless_fec_adapt.rs`, `FECSPLIT` and `FECCRIT3` lines). Adaptive arm, 10 Mbit/s, 150 ms buffer, 30 s, 5 seeds (`0xFB0001`-`5`), mean ± sd. Every number is a share of the **source wire bytes**, except the rows/k rows, which are shares of source symbols.

### What the bytes are

| | Tile iid5 | Tile iid1 | Tile ge3at1 | H.264 iid5 | H.264 ge3at1 |
|---|---|---|---|---|---|
| **repair bytes / source bytes** | 0.373 ± 0.038 | 0.253 ± 0.018 | 0.326 ± 0.026 | 0.277 ± 0.003 | 0.238 ± 0.015 |
| planned rows at the average symbol (`r/k` x symbol bytes) | 0.271 | 0.189 | 0.246 | 0.268 | 0.228 |
| **padding to the block's longest symbol** | **0.098 ± 0.032** | 0.062 ± 0.017 | 0.077 ± 0.013 | 0.006 | 0.007 |
| headers (12 B a row) | 0.004 | 0.002 | 0.003 | 0.002 | 0.002 |
| request rows | 0.0004 | 0.0001 | 0.0007 | 0.0008 | 0.0016 |
| PTO rows | 0 | 0 | 0 | 0 | 0 |
| `T_lr` rows | 0.0001 | 0 | 0 | 0 | 0 |
| trim (rows above the cap) | 0 (never on) | 0 | 0 | 0 | 0 |
| mean `k`, mean `k_eff` | 15.4, 11.9 | 14.7, 11.7 | 27.3, 21.1 | 8.3, 8.3 | 16.2, 16.2 |
| blocks of mixed symbol sizes | 86 % | 90 % | 98 % | 0 % | 0.06 % |
| delivered per second (tile passes, frames) | 6228 ± 95 | 6879 ± 30 | 6909 ± 62 | 27.0 | 27.8 |

- **The 0.45 of E11-RCA (one seed: 0.443) divides as 0.28 planned, 0.15 padding, 0.005 headers and about 0.001 for every extra row together.** The five-seed mean is lower (0.373) and the padding is the only part that is large and not in the plan: a quarter to a third of the repair bytes, 6 to 10 points of source bytes, where the controller plans none.
- **The cause is the symbol size.** A repair row is as long as the block's longest symbol (a full tile bundle), and a tile block holds bundles of very different sizes: `k_eff = ceil(sum(len) / max(len))` is 11.9 for a mean `k` of 15.4, so the block's `k` rows' worth of source is 23 % fewer bytes than `k` full symbols, while the cap `ceil(k / 4)` rows are all full length. H.264 blocks have `k_eff = k` (a frame is MTU-sized fragments): padding 0.6 points.
- **Requests, PTO, `T_lr` and the trim are not the problem for tile**: together under 0.1 point, and the trim was never switched on in 25 runs (`rows_trim = 0`). Candidates 2 and 3 of decision 6 would save nothing. Candidate 1, a byte cap on `r` for tile blocks, attacks the 6-10 points.

### Criterion (3): the residual after S1b

`read_run`'s planned rows counted `T_lr` rows (they are in neither of its subtrahends); they are now counted by cause (`rows[0]` is the blocks' own `r`, trim included). Achieved `r/k` against the bar (`FECCRIT3`, 5 seeds):

| cell | achieved r/k | rule at the estimate | step-limit hold | trim | exact-`k` cap | bar | request / PTO / `T_lr` (not in r/k) |
|---|---|---|---|---|---|---|---|
| H264/iid5 | 0.2981 | 0.2982 | 0 | 0 | 0.2982 | 0.2964 (cap, midpoints) | 0.0008 / 0 / 0 |
| H264/ge3at1 | 0.2735 | 0.2735 | 0 | 0 | 0.2740 | 0.2572 (cap) | 0.0018 / 0 / 0 |
| Tile/iid5 | 0.2744 | 0.2738 | 0.0007 | 0 | 0.2765 | 0.2700 (cap) | 0.0005 / 0 / 0.0001 |
| Tile/ge3at1 | 0.2598 | 0.2598 | 0 | 0 | 0.2629 | 0.2505 (cap) | 0.0005 / 0 / 0 |

- **There is no overhead above the rule.** Achieved equals the rule's rows at the estimate to within 0.001, which equals the count cap at the blocks' exact `k` (the target is unreachable in these four cells: `r` sits at `ceil(k / 4)`). Request rows are not in `r/k`; the `T_lr` rows were 0.0001 at most; padding is a byte quantity and not in a rows ratio; the trim and the step limit contribute 0.0007 at most.
- **The whole gap to the bar is rounding in the bar itself**: it takes `ceil(k / 4) / k` at the middle of each size bucket (`block_k_bucket_range`: 17-24 reads as 20, which gives 0.25), while the blocks that were closed sit at 17, 18, 21 and the like, where the ratio is 0.28-0.29. Exact-`k` cap minus bar: 0.0018 (H264/iid5), 0.0168 (H264/ge3at1), 0.0065 (Tile/iid5), 0.0124 (Tile/ge3at1). A bar made from the exact `k` histogram (`k_hist` is in the run now) would read 0.298 / 0.274 / 0.277 / 0.263 and the four cells would pass by 0.000-0.003. This is a defect of the criterion's bar, not of the controller; the bar is not changed here.

### TB1: the byte cap on tile blocks' `r`, measured and not adopted

The data picked candidate 1 (padding is the only large unplanned part). It was built as the plan describes (`r <= max(1, ceil(sum(len) / (4 max(len))))`, all-tile blocks only, after `RSizer::r_for_paced`; unit and property tests, and a harness switch for the control arm) and measured against the same build with the cap off, adaptive arm, same seeds. **It did not improve delivered tile passes per second on the plan's cell (Tile/iid5/10M), so it was reverted** (E10's precedent: no dead gated code). The patch is kept outside the tree for the owner: `tb1-byte-cap.patch` in the session scratchpad.

| cell (adaptive, cap off -> on) | seeds | repair / source | delivered tiles/s | requests per block | hand-offs + declines | cap bound blocks |
|---|---|---|---|---|---|---|
| Tile/iid5/10M | 5 | 0.373 ± 0.038 -> **0.291 ± 0.007** | 6228 ± 95 -> **6231 ± 129** | 0.0056 -> **0.0174** | 99 -> 133 | 770-1910 per run |
| Tile/iid5/2M | 3 | 0.523 -> 0.365 | 1088 ± 6 -> **1224 ± 4** | 0.0087 -> 0.061 | 0 -> 30 | 643-694 |
| Tile/iid1/10M | 3 | 0.243 -> 0.258 | 6881 ± 28 -> 6896 ± 37 | 0.0007 -> 0.0017 | 0 -> 7 | 366-493 |
| Tile/ge3at1/10M | 3 | 0.341 -> 0.277 | 6880 ± 64 -> 6912 ± 18 | 0.0053 -> 0.0090 | 27 -> 0 | 841-877 |
| H264/iid5/10M, H264/ge3at1/10M | 1 | wire digest identical, `adapt_cap_bytes` 0 | | | | |

- **The cap does what it was built for**: repair bytes over source bytes fall by 8 points at 10 Mbit/s i.i.d. 5 % (outside the seed spread, and the byte bound `r x max <= 0.25 sum + max` holds on every run with the cap and fails without it) and by 16 points at 2 Mbit/s.
- **The bytes it frees do not become delivered tiles at 10 Mbit/s.** Offered / estimate falls from 0.82 to 0.78 and delivered stays at 6230 (difference 3, spread 129): the stream does not offer more source with the room (not diagnosed here), so the saving is not goodput. Meanwhile requests per block triple (0.0056 to 0.0174, outside the spread) and hand-offs plus declines rise 99 to 133: fewer rows per block cost recoveries. Tile ack latency is unchanged (mean 64.8 ms to 63.1 ms; p95 bucket 100 ms in all runs).
- **At 2 Mbit/s the same cap raises delivery by 12 % (1088 to 1224, seeds within 6)**, which is the cell where the C0 match saw 1070 against 1522 for the off arm. That is a partial close (the control is still 20 % ahead), paid for with 7 x the requests and 30 hand-offs plus declines. It is not enough to meet the plan's bar on its own cell, and it is the owner's decision whether a cap that helps only below 10 Mbit/s is worth keeping.
- **H.264 is untouched by construction and by measurement**: the cap applies to all-tile blocks only; the adaptive wire digest of H264/iid5/10M and H264/ge3at1/10M is bit-identical with the cap on and off.
- **Open, not diagnosed:** why the room the cap frees at 10 Mbit/s is not filled with source. The matched off arm of C0 delivered 7276 tiles/s against 6103 for the adaptive arm there; TB0 shows 0.10 of the 0.37 repair share is padding and TB1 removes most of it without moving delivery, so repair size is not what separates the two arms at 10 Mbit/s.

## 12. S0 split gate: the entry rule and the per-block r rule (R6-e2 plan task S0)

**Every number is a model estimate [E].** The scripts are `adapt_sim.py --part split2` (the arms and the grid), `adapt_summary.py --split2` (the tables below) and the output `split2_out.txt` (one line per cell and seed). The gate is `python3 -I adapt_sim.py --selftest` (also run by `selftest.py`). Nothing above this section changed: the new code is additive (`split2_*`, `plan_rf`, `main_split2`), and `adapt_sim_out.txt` and every other output are byte-identical.

**Numbering.** Section 11 (the tile repair budget, TB0 / TB1) was appended on another branch and lives in the implementation branch until the merge; this section is numbered 12 so the two do not collide.

### Method
- **Cells.** 100 / 150 / 200 Mbit/s x GE 2 % with mean burst 3 / 5 / 8 x h264 and mix, **10 seeds** per cell (the plan asked for 5; the full grid took about 12 minutes, so 10 were run everywhere rather than widening only where the spread was wide), 300 frames (10 s of video) per seed. A seed re-draws demand, timeline and loss. One cell is about 0.33 point of late frames per late frame, so the spreads below are wide.
- **Arms.**
  - `T20k223`: one block, T 20 ms, k_max 223, r from the exact tail rule at the true (p, b), target 1e-3 (E1's single block, the current wide notch).
  - `il_rho0.1 / 0.2 / 0.3`: section 6's `a_il`, `R = ceil(rho k)`, `r = ceil(R/m)`, interleaved.
  - `il_rf`: the plan's per-block rule: `m = clamp(ceil(k (1 + 0.2) / 255), 1, 8)`, block `j` gets `r_for(k_j, p, max(1, b/m))` (true p, b: a perfect estimator, so the estimator's own error is not in these numbers), target 1e-3, interleaved.
  - `il_rf_t1e-4` (not in the plan: added because `il_rf` fell short, below): the same rule at target 1e-4.
  - `contig_rho0.2` and `contig_rf`: the same two rules with the contiguous assignment (the control).
- **Mean frame size** (h264 cells, bundles of 1180 bytes): 240 / 360 / 479 at 100 / 150 / 200 Mbit/s (median 177 / 266 / 355); frames needing m = 1 (by the plan's `m` formula): 215 / 117 / 82 of 300, so the single-block case is common at 100 Mbit/s and rarer above it (m = 2 for 91 / 146 / 134 frames; the rest m 3-8).
- **"Outside the spread"** is the rule E1 used: the difference of two means must exceed the larger of the two cells' seed ranges (max - min). It is strict (ranges of 10 seeds). A paired view is printed as well and called out as such: the same seed gives the same demand and loss seed in both arms (the packet counts differ by arm, so the pairing is approximate), so the difference per seed has a standard error of the mean, and "seeds where the arm is no later" counts of 10. The paired view is a supplement and does not replace the rule above.
- **Cells inside the spread are named, not ranked.**

### (i) Where the split beats the single block
Late33 (%), mean over 10 seeds; d = single - arm, in points; "spread" the larger seed range.

| Mbit/s | mix | burst | single | il_rho0.2 (d, spread) | il_rf (d, spread, paired) |
|---|---|---|---|---|---|
| 100 | h264 | 3 | 0.11 | 0.03 (+0.09, 0.29) | 0.77 (-0.66, 1.43; 1/10 seeds no later) |
| 100 | h264 | 5 | 0.63 | 0.40 (+0.23, 1.14) | 0.57 (+0.06, 1.14; 6/10) |
| 100 | h264 | 8 | 2.17 | 1.46 (+0.71, 3.14) | 0.89 (+1.29, 3.14; +1.29 +- 0.32, 10/10) |
| 150 | h264 | 3 | 0.31 | 0.03 (+0.29, 0.86) | 1.17 (-0.86, 0.86; 0/10) |
| 150 | h264 | 5 | 0.91 | 0.11 (+0.80, 1.43) | 1.09 (-0.17, 1.43; 5/10) |
| 150 | h264 | 8 | 3.23 | 1.20 (+2.03, 2.57) | 1.31 (+1.91, 2.57; +1.91 +- 0.28, 10/10) |
| 200 | h264 | 3 | 0.51 | 0.03 (+0.49, 1.14) | 1.40 (-0.89, 1.43; 0/10) |
| 200 | h264 | 5 | 1.23 | 0.11 (+1.11, 1.43) | 1.03 (+0.20, 1.43; 6/10) |
| **200** | **h264** | **8** | **4.23** | **0.26 (+3.97, 2.29) outside** | **1.09 (+3.14, 2.29) outside** |
| 200 | mix | 8 | 2.94 | 0.71 (+2.23, 2.57) | 1.51 (+1.43, 2.57; +1.43 +- 0.30, 10/10) |

(The full table with all 18 cells, both arms of the rule and the paired columns is `python3 -I adapt_summary.py --split2`.)

- **The only cell outside the spread is 200 Mbit/s, h264, burst 8** (both `il_rho0.2` and `il_rf`; the single block's 4.23 against 0.26 and 1.09). E1's 3.5 points at the same cell at 5 seeds is 3.97 here.
- **150 Mbit/s, burst 8** (the cell E1 did not measure): `il_rho0.2` is 2.03 better and `il_rf` 1.91 better than the single block, **inside the spread** (2.57) by the strict rule, outside it by the paired view (10 of 10 seeds, +2.03 +- 0.35 and +1.91 +- 0.28). Name it as inside the spread; the data says it is probably real and too small a gap to prove with 10 seeds of 10 s.
- **At burst 3 the split does not help**, and with `il_rf` it is later than the single block: -0.66 / -0.86 / -0.89 point at 100 / 150 / 200 Mbit/s h264 (0 of 10 seeds no later at 150 and 200), inside the larger seed range but one-sided. At burst 5, `il_rf` and the single block are the same (-0.17 to +0.20, 5-6 of 10 seeds); `il_rho0.2` is better by 0.8 to 1.1 at 150 / 200 (10 of 10 seeds) at 5 points more overhead.
- **At 100 Mbit/s: no gap** at any burst outside the spread (b8: +0.71 for `il_rho0.2`, +1.29 for `il_rf`, spread 3.14); the split is then a frame-spanning block at m = 1 for 215 of 300 frames, i.e. almost the same codec as the single block.
- **Mix cells:** every difference is inside the spread (200 Mbit/s b8: +2.23 / +1.43, spread 2.57); b3 slightly in the single block's favour for `il_rf` (-1.09 at 200).
- **What the sim fixes.** The gate can use `m_needed >= 2` as its rate condition: m = 1 is by construction the single block (no `m` to enter on), and above it the split's gap grows with the burst; the rate itself is not a separate axis in this data (the gap at b8 is +1.3 / +1.9 / +3.1 at 100 / 150 / 200 for `il_rf`, all with the same sign). The burst is the axis: no benefit at 3, none for `il_rf` at 5, a gain at 8.
  - `FEC_SPLIT_MIN_M = 2`.
  - `FEC_SPLIT_B_ENTER = 6.0`, `FEC_SPLIT_B_EXIT = 4.0`: between 5 (the last burst where `il_rf` equals the single block) and 8 (the first where it wins in every seed), with 2 of hysteresis. The sim did not measure 6 or 7, so the entry is a midpoint, not a measured crossing. Both are in units of the controller's `b_hat` (mean burst of the GE process the sim draws from; the estimator's bias against it is not in this data and is S3's to check).

### (ii) Per-block `r_for(k_b, p, b/m)` against `R = ceil(rho k)/m`
**It does not match `il_rho0.2`'s late frames, and it spends 3 to 14 points less overhead.** Paired difference `il_rf - il_rho0.2` in late33 (+ = `il_rf` later), mean +- s.e.m. over 10 seeds:

| cell | late33 d | seeds `il_rf` later | overhead il_rf / il_rho0.2 |
|---|---|---|---|
| 150 h264 b3 | +1.14 +- 0.11 | 10/10 | 8.3 / 20.3 |
| 150 h264 b5 | +0.97 +- 0.11 | 10/10 | 10.9 / 20.3 |
| 150 h264 b8 | +0.11 +- 0.22 | 6/10 | 14.3 / 20.3 |
| 200 h264 b3 | +1.37 +- 0.14 | 10/10 | 7.2 / 20.3 |
| 200 h264 b5 | +0.91 +- 0.09 | 10/10 | 9.3 / 20.3 |
| 200 h264 b8 | +0.83 +- 0.14 | 9/10 | 12.3 / 20.3 |
| 200 mix b8 | +0.80 +- 0.28 | 8/10 | 11.1 / 20.5 |
| 100 h264 b8 | -0.57 +- 0.24 | 2/10 | 17.2 / 20.3 |

By the strict rule only **150 Mbit/s h264 b3** is outside the spread (+1.14 against 0.86); the other cells are 0.1 to 1.4 points against ranges of 0.9 to 2.3, inside it and not ranked. By the paired view `il_rf` is later in 8 of the 9 cells at 150-200 Mbit/s (6 to 10 of 10 seeds). Its residual source loss is 0.12-0.17 % against 0.00-0.09 %.
- **The overhead is the whole difference.** `il_rho0.2` is a constant 20.3 %; `il_rf` follows (p, b): 7 to 17 %. On the overhead axis `il_rf` sits where `il_rho0.1` (10.3 %) is not: at 200 Mbit/s b8, `il_rho0.1` 2.80 at 10.3 % against `il_rf` 1.09 at 12.3 %.
- **Tightening the target narrows it.** `il_rf_t1e-4`: `il_rf_t1e-4 - il_rho0.2` is +0.31 / +0.74 / +0.77 (100 / 150 / 200 h264 b3), +0.09 / +0.63 / +0.57 (b5), -0.43 / -0.06 / +0.97 (b8) at overhead 8.6-18.5 % against 20.3 %. At 150 Mbit/s b8 it is level (-0.06 +- 0.18) with 16.0 % against 20.3 %; at 200 b8 it is still +0.97 later (10/10 seeds) at 14.0 %.
- **Which rule S3 uses.** The per-block rule `r_for(k_b, p_hat, max(1, b_hat / m))`, **with its target set to 1e-4 in the Split notch** (a named constant S3 adds, `FEC_SPLIT_R_TARGET`), not `ceil(rho k)/m`: the controller has no fixed `rho`, and `il_rf_t1e-4` is the only rule here that is better than the single block at burst 8 (at b8, d against the single block +1.14 / +2.09 / +3.00 at 100 / 150 / 200 h264, no later in 10 of 10 seeds at each; only 200 is outside the strict spread) **at about the single block's overhead** (b8: 18.5 / 16.0 / 14.0 % against 16.9 / 15.6 / 15.2 %). It is not as good as a flat 20 %: the 200 Mbit/s b8 cell is 1.23 (`il_rf_t1e-4`) against 0.26 (`il_rho0.2`). If S3 needs that cell, the price is about 6 points of overhead. The likely reason the 1e-3 per-block rule falls short is that `tail(k_b + r, r, p, b/m)` treats a block as `k_b + r` consecutive datagrams of a GE process with mean burst `b/m`; a block that interleaves a whole frame sees the frame's bursts split over `m` blocks, which is not a GE process of mean `b/m` (a geometric run thinned by `m` has mass at 0 and a longer tail). This is a hypothesis; the sim did not test it.

### (iii) Recovery wait, p95 (ms), mean over seeds (min-max of seeds)
| Mbit/s | mix | single block | il_rho0.2 | il_rf | il_rf_t1e-4 |
|---|---|---|---|---|---|
| 100 | h264 b8 | 25.4 (20.5-29.4) | 62.4 (52.2-73.1) | 62.1 (55.7-71.9) | 55.2 (48.6-63.2) |
| 150 | h264 b8 | 18.5 (17.6-19.3) | 68.9 (50.4-131.2) | 52.1 (44.9-61.3) | 53.0 (43.5-63.6) |
| 200 | h264 b8 | 18.9 (18.2-19.5) | 61.8 (52.8-67.3) | 52.9 (48.2-55.5) | 55.4 (46.7-59.5) |
| 200 | mix b8 | 18.6 (17.3-19.8) | 38.8 (34.6-44.0) | 33.8 (28.0-38.9) | 32.9 (26.1-39.3) |

- **The split costs about 33 to 44 ms of p95 recovery wait in h264 cells** (single block 18-25 ms; split arms 52-63 ms; 100 Mbit/s 55-62) and 15 to 22 ms in the mix (33-46 against 19-25). Section 6 found 62-70 against 10-20. The three split arms are alike (they send the same frame-spanning repairs); the adaptive ones are 6-10 ms shorter at 150-200 Mbit/s because fewer repairs follow the frame.
- **`T_adv` of 50-66 ms advertises it, and is at its edge:** the per-seed p95 reaches 73 ms for `il_rho0.2` at 100 b8, 69 for `il_rf` at 100 b5 (per-seed maximum), and **one seed at 131 ms** (`il_rho0.2`, 150 Mbit/s b8 h264). A client deadline derived from `T_adv` 50-66 ms is exceeded by the p95 in a minority of seeds; S3's lateness check must read the recovery wait, not only `T_adv`.

### (iv) The contiguous control
Late33 (%), mean (min-max of 10 seeds):

| Mbit/s | mix | burst | il_rho0.2 | contig_rho0.2 | il_rf | contig_rf |
|---|---|---|---|---|---|---|
| 100 | h264 | 8 | 1.46 (0.86-3.14) | 2.23 (1.14-4.00) | 0.89 (0.00-1.71) | 3.34 (1.71-4.86) |
| 150 | h264 | 8 | 1.20 (0.29-2.57) | 2.37 (1.43-3.43) | 1.31 (0.57-2.29) | 7.14 (5.43-9.71) |
| 200 | h264 | 3 | 0.03 (0.00-0.29) | 0.03 (0.00-0.29) | 1.40 (0.86-2.29) | 11.94 (9.71-14.00) |
| 200 | h264 | 5 | 0.11 (0.00-0.29) | 0.40 (0.00-0.86) | 1.03 (0.57-1.71) | 12.09 (9.71-14.86) |
| 200 | h264 | 8 | 0.26 (0.00-0.86) | 1.80 (1.14-2.86) | 1.09 (0.57-1.43) | 10.17 (8.29-11.43) |

- **With `R = ceil(rho k)/m` the contiguous gap grows with the burst and is clear only at b8:** 200 Mbit/s: 1.80 (1.14-2.86) against 0.26 (0.00-0.86), +1.54 (section 6: 2.1 against 0.4). The two seed ranges do not overlap, but the difference is just under the larger range's width (1.72), so by the strict rule it is **inside the spread**; 150 Mbit/s b8: 2.37 against 1.20 (+1.17), inside; at b3 and b5 the two are the same (0.03 / 0.03; 0.40 / 0.11).
- **With the per-block rule the interleave is not optional at any burst:** `contig_rf` is 8.3-14.0 % at 200 Mbit/s (b3: 11.94 against 1.40; the ranges do not touch) and 6-10 % at 150, because the rule sizes each block for its share of a burst (`b/m`), which holds only when the burst is shared out. The same gap shows in the mix (200 Mbit/s b3 6.69 against 1.31). The contiguous variant is the control that must fail, and it does, by a factor of 5 to 9 at 150-200 Mbit/s.

### Break checks (the selftest)
- **`r = 0` on one block** (`plan_rf(broken=True)`, 150 Mbit/s h264, b3, 200 frames): late33 60.00 % against 1.60 %, residual source loss 0.824 % against 0.139 %, overhead 6.0 % against 8.5 %. The selftest asserts residual more than 3x the rule's + 0.05 and late frames more than 5 points above it. As in section 6.
- **Deliberately contiguous `a_il`** (`plan_rf(contig=True)`, 200 Mbit/s h264 b8, 5 seeds of 300 frames): late33 9.89 (8.29-11.43) against 0.97 (0.57-1.14) interleaved; the selftest asserts the means differ by more than the larger seed range. The contiguous variant of the rho rule (`contig_rho0.2`) is not used for this check: at 200 Mbit/s b8 its gap is just inside the strict spread (iv). It also asserts the structure of the rule (cover, `k_b + r <= 255`, `r >= 1`, `i mod m` assignment).
- The existing checks (`selftest.py`, `adapt_sim.py --selftest` first half, `split_sim.py --selftest`) pass unchanged.

### Values for R6-e2 S3
| constant | value | from |
|---|---|---|
| `FEC_SPLIT_MIN_M` | 2 | (i): m = 1 is the single block; no gap at 100 Mbit/s (m = 1 for 72 % of frames) |
| `FEC_SPLIT_B_ENTER` | 6.0 | (i): `il_rf` equals the single block at b5 and wins in every seed at b8 |
| `FEC_SPLIT_B_EXIT` | 4.0 | hysteresis of 2 below the entry; `il_rf` is later than the single block at b3 |
| per-block rule | `r_for(k_b, p_hat, max(1, b_hat / m))` at target 1e-4 (`FEC_SPLIT_R_TARGET`) | (ii) |
| `T_adv` | read the recovery wait: p95 52-63 ms h264, per-seed up to 73 and one seed 131 | (iii) |

**Caveats.** True (p, b), no estimator error; the burst values 6 and 7 were not run; the gap at 150 Mbit/s b8 is inside the strict spread; 300 frames per seed; GE loss only (E11's i.i.d. cells have b = 1 and never enter the Split notch).

## 13. R6-e2 (plan task S3): the split notch, and why it became the frame notch, measured end to end

### Decision (owner): the interleave is deleted, the notch stays
The sections below record the experiments with the interleave (`split`, m > 1 blocks per frame) as they were run; the code that built them is gone (the S2 and S2m commits are reverted, `frozen_s1.rs`, `BlockGroup`, `ClosedBlock.group`/`group_m`, the round-robin repair queueing, `FecBlockParams.m` and the knob's fourth field, `FEC_SPLIT_MIN_M`, `FEC_SPLIT_R_TARGET`, the per-block `b_hat / m` sizing). Why:
- **The interleave alone (split - m1, paired, 5 seeds, 30 s) is inside 2 standard errors at every burst:** +0.01 +- 0.01 (200 Mbit/s b8), +0.10 +- 0.10 (200 b16), -0.05 +- 0.05 (200 b32), +0.12 +- 0.15 (150 b16); 150 b8 -0.01 +- 0.01. The decision rule was fixed in advance (E10's).
- **The Python simulation's prediction was not reproduced by the harness.** Sections 6, 8 and 12 found the interleaved split 3.5 to 4 points better than a single block at 200 Mbit/s b8 (and the contiguous variant 8 to 12 points worse with the per-block rule); the harness finds the notch 0.7 points better at b8 and 2.0 to 2.4 at b16 and b32, all of it from the span (m1 reproduces it), and the interleave adds nothing. The harness has ARQ and the H.264 reference recovery the model does not, and a block of 223 with up to 32 rows survives these bursts. Whether the interleave would matter on a link the harness does not model is not measured.
- **What stays, renamed the frame notch (`Notch::Frame`):** T = the frame interval (34 ms at 30 fps, `T_MS_MAX` 50), the block advertising it through `T_adv` (code 6, 50 ms; first window 60 ms), `k_max` 223, one block per close; entry `b_hat > 6.0` (`FEC_FRAME_B_ENTER`) and a frame over one block (`fill > 1`), exit `b_hat < 4.0` (`FEC_FRAME_B_EXIT`) or `fill <= 0.9`; one change per dwell. Both bursts constants are still midpoints, not measured crossings (the harness measured 3 and 8, 16, 32). The fill test is the old `m_needed >= 2` stated as what it was: the notch is measured only where a frame no longer fits one 255-symbol block; below that the ladder is byte-identical to the two-notch ladder (unit test and scene). Nothing in the entry or exit only made sense with m > 1 except that condition, which keeps its meaning.
- **The per-block target** (1e-4 against 1e-3) measured identical (rows hit the cap), so the notch uses the default target.

### The frame notch as kept (`browserless_fec_frame_notch.rs`, 5 seeds, 30 s; CI runs 150 Mbit/s b8 with 3 seeds, 20 s)
Late33 % (single = notch forbidden), paired gain = single - notch, mean +- s.e.m.:

| cell | single | frame notch | paired gain | seeds no later | `b_hat` mean | overhead single / notch |
|---|---|---|---|---|---|---|
| 150 Mbit/s b3 | 3.22 | 3.22 | 0 (never entered, same run) | 5/5 | 3.00 | 0.193 / 0.193 |
| 150 Mbit/s b8 | 3.60 | 2.88 | +0.72 +- 0.15 | 5/5 | 7.83 | 0.239 / 0.243 |
| 200 Mbit/s b3 | 3.04 | 3.04 | 0 | 5/5 | 2.94 | 0.194 / 0.194 |
| 200 Mbit/s b8 | 3.55 | 2.86 | +0.69 +- 0.11 | 5/5 | 8.30 | 0.242 / 0.240 |
| 200 Mbit/s b16 | 3.84 | 1.84 | +2.00 +- 0.19 | 5/5 | 15.37 | 0.245 / 0.247 |

Per seed, late frames (single / notch, count of frames late): 150 b8: 14/5, 49/42, 44/42, 10/4, 39/31; 200 b8: 13/5, 47/41, 46/41, 13/5, 35/31; 200 b16: 36/19, 17/5, 36/20, 21/4, 49/27. Every seed is no later; every gain is inside the seed range (3.9 to 4.5 points: the per-seed late share runs 0.6 to 6 %), so it is judged on the pairs.

### Open note
`b_hat` reads low at burst 32 (21.3 against 32; per run 15.6 to 24.8); at burst 16 it reads right (15.4 at 200 Mbit/s, 18.1 at 150): probably the report's 8-bucket run histogram capping what a mean can read. Not verified. The notch's entry (6.0) is far below, so the decision is unaffected, but a controller that wants the true mean burst above about 20 cannot get it from this report.


Scenes (as run for the interleave experiments; the file is now `browserless_fec_frame_notch.rs` with one arm fewer): `browserless_fec_split.rs`; the grid was `GHOSTFRAME_FEC_SWEEP=full cargo test --test browserless_fec_split the_split_grid -- --nocapture` (about 10 minutes), every run prints a `FECSPLIT run` line. PCM rate model (`pcm_rate_model`, H.264, `RefManager`), a rate cap behind a 150 ms tail-drop buffer, 25 ms one way, Gilbert-Elliott loss 1 in the bad state, 2 % on average, mean burst 3 or 8. **5 seeds, 30 virtual seconds per run** (the CI cell: 3 seeds, 20 s). Numbers are in the scratchpad logs of the S3 task. Section numbering follows section 12 (S0), which lives on the implementation branch.

**Arms**, all adaptive, same seed = same demand and loss draw: `single` (split forbidden: the two-notch ladder, one block, T 20 ms, k_max 223); `m1` (the split notch, `m` pinned to 1: its T, k_max and per-block rule, no interleave); `split` (the product). Late = decoded more than 33 ms (one frame) after the stream's floor (S0's "late33"; at 100 ms, the sweep's RTT, 1 frame in 870 is late in every arm, which is why 33 ms is the unit).

### Late frames, mean % of frames shown (seed range)
| cell | single | m1 | split | split - single, paired (mean +- s.e.m., seeds no later) | outside the seed range? |
|---|---|---|---|---|---|
| 150 Mbit/s b3 | 3.22 | 3.22 | 3.22 | 0 (the arms are the same run: `b_hat` 3.0, the split never entered) | no |
| 150 Mbit/s b8 | 3.60 | 2.88 | 2.89 | +0.71 +- 0.15, 5 of 5 | **no** (range 4.50) |
| 200 Mbit/s b3 | 3.04 | 3.04 | 3.04 | 0 (same) | no |
| 200 Mbit/s b8 | 3.55 | 2.86 | 2.85 | +0.70 +- 0.10, 5 of 5 | **no** (range 4.19) |

- **At b3 the split does not engage** (`b_hat` mean 2.97, s.d. 0.20, 20 runs; `split_entries` 0 in every run) and the three arms are byte-identical (same wire digest).
- **At b8 it engages on every seed** (`split_entries` 1, `notch_changes` 2: Wide, then Split, in all 20 split runs; `m` 2 to 3 at 150 Mbit/s, 2 to 4 at 200), and the split has fewer late frames than the single block on every seed (5 of 5 at both rates; the paired gain is 0.7 points, 5 to 7 standard errors). **But the gain is inside the seed range** (the per-seed late share runs 0.6 to 5.4 %, set by the seed's demand), so the plan's "a gap outside the spread at 200 Mbit/s b8" is **not met by the strict rule**: 0.70 against a range of 4.19. E1 and S0 found 3.5 to 4 points at this cell in the model; the harness finds 0.7. The harness has the ARQ and the H.264 reference recovery the model does not, and the single block's late share at b8 (3.55) is only 0.5 above its b3 floor (3.04), so there is little left for the split to remove.
- **The gain is not the interleave** (and the same holds at bursts 16 and 32, below). `m1` (no interleave, same span) equals `split`: the interleave alone is -0.01 +- 0.01 (150) and +0.01 +- 0.01 (200). With the notch's T cut to 20 ms (a test edit, not kept) `m1` equals `single` (5.96 against 5.95 on the 3-seed CI cell) and `split` over `m1` is +0.24 +- 0.16. What this data says: at these rates and this loss the late frames are cut by **the span of the notch** (T = the frame interval, so a block holds one frame instead of straddling two), not by interleaving. The interleave's own value (S0: contiguous 10.17 against interleaved 1.09 at 200 Mbit/s b8 with the per-block rule) is a burst-recovery property that this link (b8, 2 %) does not stress: a single block of 223 with 32 rows survives it. Whether the interleave matters on a link with longer or denser bursts is not measured here.
- **Overhead:** repair over source wire bytes, 0.239 (single) against 0.245 (split) at 150, 0.242 against 0.249 at 200; the split spends 0.6 to 0.7 points more.
- **Recovery wait.** The client's histogram (blocks rebuilt from repair rows, first arrival to rebuild): p95 is in the <= 20 ms bucket in 9 of the 10 b8 split runs (30 ms in one), 10 to 20 ms for the single block. S0's model p95 was 52-63 ms (up to 73 per seed); the harness does not reproduce it, because a block that closes on `k_max` (223 sources, 14 ms at 150 Mbit/s) sends its repair right behind it. What does reach the table's top: 66 of 2457 rebuilt blocks (2.7 %) at 150 b8 and 79 of 3043 (2.6 %) at 200 b8 wait more than 80 ms, the single block 39 of 1253 (3.1 %) and 36 of 1208 (3.0 %): they are the blocks recovered by a requested row, one client window (T_adv 50 ms + 10 ms) plus a round trip after the first arrival. `late_rows` is 0 and client declines are 2 to 7 per 5 runs in every arm.
- **`T_adv`.** The split's T is 34 ms (33.3 rounded up) and the block advertises 34 + `QUEUE_TARGET` 10 = 44 ms, which the table rounds up to code 6 (50 ms); the client's first window is 60 ms. `T_MS_MAX` (50) + the queue allowance (10) = 60 is inside the table's 66 ms top, by a const assertion. Above that (a `T` the knob cannot reach, or a wait above the window): code 7 saturates at 66 ms; a block that outlives its window is asked for a row (request), not declined, and a row that arrives after the give-up is counted in `late_rows` (0 in these 50 runs).

### Long bursts: does the interleave matter where one block's rows cannot absorb the burst?
Same scenes, mean burst 16 and 32 (200 Mbit/s) and 16 (150 Mbit/s), 5 seeds, 30 s, the three arms. Decision rule set in advance (E10's): split - m1 inside about 2 standard errors at every burst means the interleave has no measurable value in this harness. (The exploration cell was not committed; the numbers are from `s3_long.log` in the S3 scratchpad.)

| cell | single | m1 | split | m (per seed) | split - single (paired) | **m1 - single** | **split - m1 (paired, s.e.m.)** | `b_hat` mean |
|---|---|---|---|---|---|---|---|---|
| 200 Mbit/s b16 | 3.84 | 1.84 | 1.74 | 3 x5 | +2.10 +- 0.22, 5/5 | +2.00 | **+0.10 +- 0.10** | 16.0 |
| 200 Mbit/s b32 | 4.66 | 2.26 | 2.31 | 3 x5 | +2.35 +- 0.24, 5/5 | +2.40 | **-0.05 +- 0.05** | 21.3 |
| 150 Mbit/s b16 | 3.62 | 1.37 | 1.25 | 2,2,2,2,3 | +2.37 +- 0.17, 5/5 | +2.25 | **+0.12 +- 0.15** | 18.1 |

- **split - m1 is inside 2 standard errors at every burst (b8, b16, b32): the interleave has no measurable value in this harness.** The whole gain, now 2.0 to 2.4 points (10 standard errors) over the single block, is the notch's span (m1 reproduces it). Overhead single 0.244-0.250, split 0.251-0.256; hand-offs plus declines are alike in m1 and single and higher in the split at b32 (e.g. seed 4: 15 for m1, 32 for the split), late frames equal.
- `b_hat` reads low at b32 (21.3 against 32, per-run 15.6-24.8): probably because the report's run histogram has 8 buckets and its top bucket caps what a mean can read (not verified); both arms enter the split regardless (it is far above 6).
- The harness's single block at b32 has only 4.7 % late frames, against 3.0 % at b3: `r` = 32 rows of 223 and ARQ keep it recoverable. Nothing was deleted; the decision is the owner's.

### The client's held-bytes cap: not raised
`FEC_RX_MAX_HELD_BYTES` = 4 MiB. New measurement-only `BlockStats::held_bytes_peak` (the most held at once, read before the cap evicts): largest over the 20 split runs of the grid 0.41 MB (0.48 MB over all 60 runs), **`declined_held_bytes` 0 in every run**, `declined_open_cap` 0. A complete block leaves the store at once, so what is held is the blocks still waiting for repair rows, about the lost tail of one frame; 4 MiB is about 8 times the largest peak and 5 times a whole frame-spanning group (0.8 MB at 200 Mbit/s). No change; the wasm heap cost is none. A link that loses whole frames at once would hold more; the counter is on the client now, so a deployment can see it.

### The estimator against the injected burst
`b_hat` (the controller's burst estimate, read at the end of each 30 s run) against the injected mean run: **b3: 2.97, s.d. 0.20, range 2.65-3.30 (20 runs); b8: 8.08, s.d. 1.01, range 6.42-9.49 (20 runs).** No bias (0 % and 1 %); the spread at b8 is a unit, so the entry (6.0) is crossed on every seed while the exit (4.0) is four standard deviations away. S0's assumption of an unbiased `b_hat` holds in this harness.

### Break checks
- **`m` forced to 1 in the notch's span:** the b8 scene fails on its premise (`seed #0: 0 interleaved groups`); the late-frame gain is unchanged (+0.72, the point above).
- **Hysteresis removed (exit at the entry):** the flap bound fails: `seed #0: 4 notch changes`, `split_entries` [2, 1, 2] against 1 with the hysteresis, notch changes [4, 3, 4] against 2. The ladder unit test fails too (`b = 5.9 is inside the band`, `inside the band: no change at all`).
- **`FEC_SPLIT_R_TARGET` 1e-3 instead of 1e-4:** no effect on late frames or overhead at any of the four cells (200 Mbit/s b8: late 2.85 / overhead 0.249, the same to the digit; `b_hat` mean 7.89 against 8.00 from a different trajectory). The rows hit the cap (`ceil(k / 4)`, 32) at both targets here. S0's reason for 1e-4 stands in the model; this link does not discriminate it.
- Unit breaks: entering `Split` at any rate fails `below_two_blocks_the_ladder_is_the_one_before_the_split` (`rate 500000: left (34, 223), right (20, 64)`); an unscaled `b_hat` fails `per_block_r_falls_as_m_rises_and_stays_inside_255`.

### Caveats
GE loss only; one link shape; the single block's lateness at b8 is small, so a gap of 0.7 points is most of what there was; 150 and 200 Mbit/s only (`m` 2 to 4: the clamp at 8 and the held-bytes cap at larger frames are untested); `b_hat` is a read at the end of a run, not a time series.

## 14. R6-e2 (plan task SW): the E11 sweep re-run

Tested at `59d8f18e` (branch `fec/v2-rev6`) with the code of §10's follow-ups: the offered-bytes accounting fix, E6 rate scaling, `T_adv` plus the RTT hint (S1/S1b, `DeclinedLate`), criterion 6 at saturating demand (`SATURATION_SHARE` 0.93), criteria 3, 5 and 6 as restated, and the frame notch (`Notch::Frame`; the interleave is deleted). No code and no criterion was changed. Same grid as §10 (`GHOSTFRAME_FEC_SWEEP=full`), but **3 seeds everywhere** (`GHOSTFRAME_FEC_SWEEP_SEEDS=3`, seeds `0xFB0001..3`): §10 widened some cells to 5, so spreads are not comparable cell for cell.

**Verdict: the criteria do not all hold, so E13 stays blocked, but the failures moved.** Criteria 1 and 4 are unchanged from §10 (they fail only at 2 Mbit/s, and the numbers are identical). Criterion 5 and 6 changed definition and now mostly pass. Criterion 3's hand-off half now passes in every judged cell; only its overhead bar fails, by 0.001 to 0.016 except in one cell (below).

### 14.0 What was run, and what was not

- **Run:** the whole H.264 grid (4 rates x 6 losses), tile at 2, 10 and 30 Mbit/s (6 losses each), the 1500 ms deep-buffer cell, the step scenes (criterion 4) for H.264 at 2, 10, 30, 100 and tile at 2, 10, 30 Mbit/s, the frame-notch grid (5 seeds, 30 s) and `browserless_fec_rate`. The first three are `browserless_fec_adapt.rs::control_law_sweep`.
- **Not swept: tile at 100 Mbit/s** (and its step scene), as in §10. A tile run at 100 Mbit/s took about 4 to 9 minutes of wall time; the cell would take over an hour. An attempt was cut short by the disk rule (/home over 88 %): 11 runs of Tile/clean/100M and 19 of Tile/iid1/100M completed, with no verdict (a verdict needs the whole cell), so they are not reported as results.
- **Parallel runs (owner allowed two at once):** the main sweep ran alone to Tile/iid5/30M. The frame-notch tests and the rate test ran beside it (about 10 minutes, overlapping the end of Tile/iid5/30M and the start of Tile/ge3at1/30M). The partial shards of Tile at 100 Mbit/s ran two at once, and the two step-scene shards (H.264 plus deep buffer, tile) ran two at once. Stepped browserless is byte-reproducible per seed, and the step scenes reproduce §10's per-seed milliseconds exactly, so load is not expected to move a verdict; but **the two cells that overlapped (Tile/iid5/30M criteria 5 and 9, Tile/ge3at1/30M criterion 6) were not re-run alone** (the re-run was refused by the disk rule), so treat those two as unconfirmed against a flake. No `.expect("scene ran")` stall occurred.
- **Wall time:** about 3 h 40 min for the main sweep (killed in Tile/clean/100M), 108 min for the two partial shards, 7 min for the steps and deep-buffer cell (two shards), about 10 min for the frame-notch and rate tests.
- **Premises (criterion 0): hold in all 42 cells run** (off sends no repair, fixed sends repair, reports arrive, loss reached the links, adaptive repaired where loss is at or above 1 %).

### 14.1 Per cell, criteria across the 3 seeds

A criterion is judged on all 3 seeds of a cell together (hand-offs and counts as means over the seeds against the spread, as in §10). `rep` is reported, not judged (criterion 3 below 10 Mbit/s). Column 9 is report-only (padding gate); `n/m` is no repair sent, so nothing to measure.

| cell | 1 | 2 | 3 | 5 | 6 | 9 (report) |
|---|---|---|---|---|---|---|
| H264/clean/2M | **FAIL** | - | - | - | pass | - |
| H264/iid0.01/2M | **FAIL** | - | - | - | pass | - |
| H264/iid0.1/2M | - | pass | - | - | pass | - |
| H264/iid1/2M | - | - | rep | - | pass | - |
| H264/iid5/2M | - | - | rep | **FAIL** | pass | - |
| H264/ge3at1/2M | - | - | rep | - | pass | - |
| H264/clean/10M | pass | - | - | - | pass | - |
| H264/iid0.01/10M | pass | - | - | - | pass | - |
| H264/iid0.1/10M | - | pass | - | - | pass | - |
| H264/iid1/10M | - | - | pass | - | pass | - |
| H264/iid5/10M | - | - | **FAIL** | pass | pass | - |
| H264/ge3at1/10M | - | - | **FAIL** | - | pass | - |
| H264/clean/30M | pass | - | - | - | pass | - |
| H264/iid0.01/30M | pass | - | - | - | pass | - |
| H264/iid0.1/30M | - | pass | - | - | pass | - |
| H264/iid1/30M | - | - | pass | - | **FAIL** | - |
| H264/iid5/30M | - | - | pass | pass | **FAIL** | - |
| H264/ge3at1/30M | - | - | **FAIL** | - | pass | - |
| H264/clean/100M | pass | - | - | - | pass | - |
| H264/iid0.01/100M | pass | - | - | - | pass | - |
| H264/iid0.1/100M | - | pass | - | - | pass | - |
| H264/iid1/100M | - | - | pass | - | pass | - |
| H264/iid5/100M | - | - | pass | pass | **FAIL** | - |
| H264/ge3at1/100M | - | - | pass | - | pass | - |
| Tile/clean/2M | pass | - | - | - | **FAIL** | - |
| Tile/iid0.01/2M | pass | - | - | - | **FAIL** | n/m |
| Tile/iid0.1/2M | - | pass | - | - | **FAIL** | n/m |
| Tile/iid1/2M | - | - | rep | - | **FAIL** | over 1.15 |
| Tile/iid5/2M | - | - | rep | **FAIL** | pass | over 1.15 |
| Tile/ge3at1/2M | - | - | rep | - | **FAIL** | over 1.15 |
| Tile/clean/10M | pass | - | - | - | pass | - |
| Tile/iid0.01/10M | pass | - | - | - | pass | n/m |
| Tile/iid0.1/10M | - | pass | - | - | pass | over 1.15 |
| Tile/iid1/10M | - | - | pass | - | pass | over 1.15 |
| Tile/iid5/10M | - | - | **FAIL** | pass | pass | over 1.15 |
| Tile/ge3at1/10M | - | - | **FAIL** | - | pass | over 1.15 |
| Tile/clean/30M | pass | - | - | - | **FAIL** | - |
| Tile/iid0.01/30M | pass | - | - | - | pass | n/m |
| Tile/iid0.1/30M | - | **FAIL** | - | - | pass | over 1.15 |
| Tile/iid1/30M | - | - | **FAIL** | - | pass | over 1.15 |
| Tile/iid5/30M | - | - | pass | **FAIL** | pass | over 1.15 |
| Tile/ge3at1/30M | - | - | pass | - | **FAIL** | ok |
| H264/clean/2M/buf1500ms | pass | - | - | - | pass | - |
Criterion 4 (engage within 1 s of a 5 % step, ms per seed):

| cell | per seed | verdict |
|---|---|---|
| H264/step5/2M | engaged before the step, 1031, engaged before the step | **FAIL** (as §10) |
| H264/step5/10M | 703, 703, 703 | pass |
| H264/step5/30M | 281, 281, 281 | pass |
| H264/step5/100M | 234, 187, 234 | pass |
| Tile/step5/2M | 703, 1687, 1406 | **FAIL** (as §10) |
| Tile/step5/10M | 609, 421, 750 | pass |
| Tile/step5/30M | 328, 328, 328 | pass |

Criterion 7 (2 Mbit/s overhead against the 28 ms design value, reported): passes in all 10 cells, as in §10.

### 14.2 Comparison with §10

- **Criterion 1: unchanged.** H.264 at 2 Mbit/s still engages on the start-up overflow (31 to 35 datagrams dropped per run in every arm), repair 1.2 to 2.5 MB in two of three seeds, digest differs: H264/clean/2M and iid0.01/2M fail with the same drop counts as §10 (34, 31, 33 and 35, 31, 34) and almost the same bytes (1.23 / 0 / 1.42 MB against 1.22 / 0 / 1.39 MB). The deep-buffer cell (1500 ms) passes: 0 drops, no repair, digest equal (the attribution of §10 finding 1 holds). The S1/S1b and accounting changes did not touch this cause. Everything at 10 Mbit/s and above passes.
- **Criterion 2: no regression where §10 measured.** Every cell that §10 ran passes with the same shape (H.264 0 to 1 changes per minute, tile at 2 and 10 Mbit/s 0 to 2). **New finding in a cell §10 did not run: Tile/iid0.1/30M fails with 5, 4, 5 changes per minute** (overhead 0.072 against fixed 0.096 and retransmits 728 against 2747 are fine, so it is churn only; criterion 2's bound is 2 per minute).
- **Criterion 3 (restated, judged from 10 Mbit/s):** the hand-offs + declines half passes in all 15 judged cells (§10 failed it in 4 cells at 2 Mbit/s where it is now only reported, and at 2 Mbit/s the adaptive arm is no longer worse than fixed in the H.264 5 % cell (3.3 against 6.7) or the tile cells (0 / 16.7 against 30 / 46.7)). The overhead half fails in 6 cells; five are against the cap built at the bucket midpoint, the rounding §11 identified: H264/iid5/10M 0.2976 against 0.2950, H264/ge3at1/10M 0.2734 against 0.2577, H264/ge3at1/30M 0.2514 against 0.2501, Tile/iid5/10M 0.2735 against 0.2698, Tile/ge3at1/10M 0.2621 against 0.2505. `trim_steps` is 0 in every cell, so the trim is no longer the cause (it was in §10). **The one cell that is not rounding: Tile/iid1/30M, r/k 0.1533 against 1.25 x the DP rule 0.1047** (saturated share 0.18; hand-offs + declines 0 against fixed's 162, so it is overspending, not underprotecting). New data (§10 had no tile at 30 Mbit/s); not diagnosed.
- **Criterion 4: identical to §10 per seed** (the stepped harness is reproducible). Fails only at 2 Mbit/s.
- **Criterion 5 (restated: gated share no more than the control's plus the sd):** 4 of 7 cells pass (H.264 10, 30, 100 Mbit/s and Tile/iid5/10M); 3 fail: H264/iid5/2M (9.4 % against the control's 7.3 %, sd 1.9), Tile/iid5/2M (24.7 against 21.7, sd 2.4) and **Tile/iid5/30M (4.8 % against 0.5 %, sd 0.7)**. §10 had all six failing under the old "never gated" wording, so this is a gain; the 30 Mbit/s tile cell is a real excess (unconfirmed under load, see 14.0). The controller was engaged in every run.
- **Criterion 6 (at saturating demand): 10 of 42 cells fail, against 19 of 36 in §10, but three causes.**
  1. **Premise not met, not a codec result (6 cells):** the off arm offers 0.918 to 0.923 of the link at 2 Mbit/s tile (clean, 0.01 %, 0.1 %, 1 %, bursty) and Tile/clean/30M (mean 0.932 printed, over 0.93, yet the premise reads false; the per-seed values were not looked at). In the clean, 0.01 % and 0.1 % cells the arms are identical (ratio 1.000), so there is nothing to fail. At 1 % and bursty 2 Mbit/s the passes/s bound holds (1390 against 1304; 1216 against 1169).
  2. **H.264 estimate below the control by 2 to 4 % outside the spread (3 cells):** iid1/30M (24.4 against 26.7 Mbit/s, sd 1.6), iid5/30M (25.5 against 26.4, sd 1.0), iid5/100M (93.4 against 96.9, sd 1.9). Frames delivered per second are far better than the control's (28 to 29 against 0.3 to 13): the control stream collapses under that loss while adaptive keeps playing, so the lower estimate buys a working stream; the ceiling binding share is 0.7 to 1.3 % against 0 to 0.7 %.
  3. **One real tile miss: Tile/ge3at1/30M**, passes/s 19650 against a bound of 19705 (54 passes, 0.3 %, under the bound; ratio 0.761 against the off arm). Unconfirmed alone (overlapped with the notch run).
  Tile at 10 Mbit/s passes everywhere, including iid5 (6031 passes/s against a bound of 5038).
- **No regression found.** Every criterion that §10 ran and passed still passes; the three that §10 failed (1, 3, 4 at 2 Mbit/s; 5; 6) are unchanged (1, 4), improved by the restatements (3, 5, 6), or moved to cells §10 did not cover (tile 30 Mbit/s: criterion 2, 3, 5, 6 one cell each).

### 14.3 Criterion 5 at 2 Mbit/s

Fails in both loads: H264/iid5/2M 9.4 % gated against the control 7.3 % (sd 1.9, so 0.2 points over the bar); Tile/iid5/2M 24.7 % against 21.7 % (sd 2.4, 0.6 over). Both are small excesses over the control; at 2 Mbit/s the tile reports hold few datagrams each and the control's own share is 22 %. The controller was engaged in all runs. This is the one place criterion 5 fails at a link rate where the stream is the rate-following one, and it fails by a margin of less than one sd.

### 14.4 Criterion 9 (padding, report only)

Worst repair bytes over the planned rows' bytes at the average symbol, against the 1.15 reference (measured cells only; H.264 is 1.00 by construction and prints nothing):

| cell | worst | padding share of repair bytes |
|---|---|---|
| Tile/iid1/2M | 1.699 | 0.41 |
| Tile/iid5/2M | 1.695 | 0.40 |
| Tile/ge3at1/2M | 1.722 | 0.41 |
| Tile/iid0.1/10M | 1.368 | 0.25 to 0.27 |
| Tile/iid1/10M | 1.269 | 0.18 to 0.21 |
| Tile/iid5/10M | 1.542 | 0.17 to 0.35 |
| Tile/ge3at1/10M | 1.382 | 0.22 to 0.27 |
| Tile/iid0.1/30M | 1.159 | 0.11 to 0.14 |
| Tile/iid1/30M | 1.164 | 0.13 to 0.14 |
| Tile/iid5/30M | 1.161 | 0.14 |
| Tile/ge3at1/30M | 1.118 | 0.10 (under) |

Padding is above the reference in every measured tile cell except bursty at 30 Mbit/s, and grows as the rate falls (0.40 of repair bytes at 2 Mbit/s, 0.2 to 0.35 at 10, 0.1 to 0.14 at 30), consistent with §11's finding that symbol-size mismatch in mixed tile blocks is the cause. TB1's byte cap was not adopted, so nothing here is new.

### 14.5 The frame notch (`browserless_fec_frame_notch.rs`, 5 seeds, 30 s; CI cell 3 seeds, 20 s)

All three CI tests pass (the 150 Mbit/s b8 cell, the one-block control `a_frame_that_fits_one_block_never_enters_the_frame_notch`, and the grid). Late33 %, single = notch forbidden, paired gain = single - notch:

| cell | single | frame notch | paired gain | seeds no later | `b_hat` mean | overhead single / notch |
|---|---|---|---|---|---|---|
| 150 Mbit/s b3 | 3.22 | 3.22 | 0 (never entered) | 5/5 | 3.00 | 0.193 / 0.193 |
| 150 Mbit/s b8 | 3.60 | 2.88 | +0.72 +- 0.15 | 5/5 | 7.83 | 0.239 / 0.243 |
| 200 Mbit/s b3 | 3.04 | 3.04 | 0 | 5/5 | 2.94 | 0.194 / 0.194 |
| 200 Mbit/s b8 | 3.55 | 2.86 | +0.69 +- 0.11 | 5/5 | 8.30 | 0.242 / 0.240 |
| 200 Mbit/s b16 | 3.84 | 1.84 | +2.00 +- 0.19 | 5/5 | 15.37 | 0.245 / 0.247 |

These equal §13's table to the digit (same seeds, deterministic harness). The CI cell (150 Mbit/s b8, 3 seeds, 20 s): 5.95 against 5.24, +0.72 +- 0.26, 3/3.

**`b_hat` at burst 32: not observable here.** The kept grid stops at burst 16 (the interleave experiments that ran b32 were deleted with the interleave); b_hat reads 15.4 at 200 Mbit/s b16 (per run 12.9 to 20.0), as §13 found. §13's open note (21.3 against 32 at b32, the 8-bucket run histogram suspected) is neither confirmed nor refuted by this run.

### 14.6 `browserless_fec_rate.rs`

`the_ceiling_stays_rare_and_the_estimate_whole_with_adaptive_fec` passes (3 seeds): ceiling binding share 0.0153 and saturated share 0.0786; the estimate mean after 5 s is higher with adaptive on (14.1, 13.4, 14.7 Mbit/s) than off (12.8, 9.5, 13.0 Mbit/s).

### 14.7 For the owner

- Criteria 1 and 4 at 2 Mbit/s are the same open item as §10 (start-up drops engage the controller; the proposed gate on the datagrams' own queueing is untaken).
- New, in cells §10 never ran: Tile at 30 Mbit/s fails criterion 2 (iid 0.1 %, 4 to 5 changes per minute), criterion 3's overhead (iid 1 %, 0.153 against 0.105), criterion 5 (iid 5 %, 4.8 % against 0.5 %) and criterion 6 (bursty, 54 passes under the bound). The last two ran beside the frame-notch tests and should be re-run alone before being relied on.
- Criterion 3's overhead failures at 10 and 30 Mbit/s are bucket-midpoint rounding (§11: the bar, not the controller); the exact-`k` bar §11 proposed is not implemented.
- Tile at 100 Mbit/s remains unmeasured.

## 15. The ledger loss gate (criteria 1 and 4 at 2 Mbit/s)

Code: `ghostframe-lib/src/transport/fec_tx/queue_loss.rs` (classifier and credit), `departure.rs` (`discard_oversized`), the hooks in `io_bridge/{ack_dispatch,bwe_glue,repair_status,transport_probe}.rs`, `LossEstimator::on_report_excusing` in `fec_tx/adapt.rs`. Counters on the `fec v2 adapt` log line: `adapt_queue_losses`, `adapt_local_discards`, `adapt_black_holes`, `adapt_losses_excused`; the sweep prints a `FECGATE` line per adaptive run.

### 15.1 Why the standing-queue gate misses the start-up overflow

1. The session's first frame (about 60 kB) bursts into the 150 ms buffer at 2 Mbit/s: 31 tail drops at about 830 to 900 ms.
2. quinn-proto 0.11.14 (`connection/mod.rs:1767-1775`) reads that burst of full-size losses as an MTU black hole, resets the MTU to `min_mtu` and calls `datagrams.drop_oversized`: about 11 queued full-size datagrams are discarded and never reach the wire. The departure tracker read them as departed (and the datagrams behind them as departed early).
3. The LOSS_REPORT carrying those losses arrives about 600 ms later, after the queue has drained, so `bwe.standing_queue()` is false. The client files a loss under the report that confirms it, not one whose `first..last_bundle_seq` range contains it, so the report's range cannot be used to find the lost bundles.

### 15.2 What was measured to choose the rule (ledger verdicts by bundle identity; `q` = depart-timed one-way delay minus its 10 s windowed minimum, taken at the nearest acknowledged bundle departed before the lost one)

| losses | `q_before >= 50 ms` | `>= 80 ms` | `>= 100 ms` |
|---|---|---|---|
| overflow (clean 2 Mbit/s start-up; 6 -> 2 Mbit/s step-down) | 100 % | 100 % | 100 % |
| i.i.d. 5 % after the first 2 s (H.264 2 and 10 Mbit/s, 2 Mbit/s with a 1500 ms buffer, tile 2 Mbit/s) | 0.5 to 5 % | 0 to 1.4 % | 0 % |
| black-hole discards | no signal: they never reached the wire | | |

`q` peaks at the buffer depth (about 155 ms behind 150 ms). The discards are therefore identified by identity, not by `q`.

### 15.3 As built

- **Queue loss.** A source bundle the ledger judges lost is a queue loss when the nearest acknowledged bundle before it (by `bundle_seq`) had `q >= max(Q_FLOOR_MS, Q_FRACTION x q_max)`, `q_max` the largest `q` of the last 10 s. Repair and control bundles (no records) are left out. A verdict a late acknowledgement contradicts is withdrawn.
- **Local discards.** `check_black_hole` reads `black_holes_detected` before each datagram is mirrored and before each poll, so every tracker entry then was queued when the black hole happened. On a rise it asks the tracker to drop every entry of `max_size()` bytes or more (`drop_oversized`'s own rule: it keeps a datagram only if it is *shorter* than `max_size`), moves the byte arithmetic by their length (`cum_departed = cum_enqueued - discarded - outgoing_total`, so neither the discarded ones nor those behind them read as departed), and marks them local. Their ledger records are still stamped at the next poll, so retransmission and the loss clock work as before. The first datagram of a tracker has no known length and is never treated as discarded.
- **Credit.** Queue and local verdicts add a credit (2 s expiry). On each LOSS_REPORT `excused = min(credit, lost_pre_fec)` is spent; the estimator adds `lost_pre_fec - excused` to `lost`, all of the report's datagrams to `total`, and scales the run histogram by the share kept (a report whose every loss is excused adds no runs). The standing-queue gate is unchanged and still skips a report whole; a skipped report also spends the credit (those losses are already out of the estimate) but does not count them as excused.

### 15.4 Constants, from the sweep (`H264/clean/2M`, `H264/iid5/2M`, `H264/iid5/10M`, 3 seeds, `queue_losses` summed over the three seeds' runs; start-up overflow is 31 queue + 11 local per run at 2 Mbit/s)

| `Q_FLOOR` | `F` | `H264/clean/2M` queue losses (3 runs) | `H264/iid5/10M` queue losses (no start-up overflow there; every one is random loss) |
|---|---|---|---|
| 50 ms | 0.8 | 31, 31, 31 | 40, 37, 31 (of about 1 000 lost per run) |
| 50 ms | 0.5 | 31, 31, 31 | 40, 39, 35 |
| 50 ms | 1.0 | 16, 10, 10 | 4, 1, 1 |
| **80 ms** | **0.8** | **31, 31, 31** | **0, 0, 0** |

`F` between 0.5 and 0.8 changed nothing (the bar is the floor wherever the queue is shallow and 0.8 x 155 = 124 ms behind the 150 ms buffer, which every overflow loss clears); `F = 1.0` demands the peak itself and covers only a third of the overflow. The floor does the work against random loss: at 50 ms 3 to 4 % of a 5 % i.i.d. link's losses were taken for queue losses (the 0.5 to 5 % of 15.2), at 80 ms none. The price: a link whose whole buffer is under about 100 ms never reaches the bar, and keeps the old behaviour (the loss counts). Not measured: a floor of 30 ms (the run was stopped, the 50 ms row already shows the false-positive rate rising as the floor falls).

### 15.5 The race (does the ledger judge before the report that carries the loss arrives?)

Yes, on the measured link (40 ms round trip, 250 ms report cadence). `verdicts_behind_report` (verdicts for a bundle at or below the `last_bundle_seq` of a report already folded) is 0 in all six runs of `H264/clean/2M` and `iid0.01/2M`, and 0, 1, 1 in `iid0.1/2M`; `credits_behind_report` (the queue and local ones among them, which is what the start-up produces) is 0 in every one of these 9 runs. A verdict needs the next acknowledgement, about one round trip; the report needs the client's next 250 ms tick and the trip back. Under 5 % i.i.d. loss `verdicts_behind_report` is 9 to 67 per run, but those are random losses (path verdicts), and `credits_behind_report` is 1 to 19 (the false positives of 15.4 at the 50 ms floor). So reports do not win the race here and **no deferral was built**: a report is folded when it arrives. A late credit is not lost: it waits for the next report, 2 s at most. Both counters are on the `FECGATE` line and the probe, so a link where it does happen shows. (A longer ack path, or one that batches acknowledgements for longer than the report interval, was not run.)

### 15.6 Results (`GHOSTFRAME_FEC_SWEEP=full`, `GHOSTFRAME_FEC_SWEEP_ONLY="H264/clean/2M,H264/iid0.0001/2M,H264/step5/2M,Tile/clean/2M,Tile/step5/2M,H264/iid0.05/2M,H264/iid0.01/2M,H264/clean/10M"`, 3 seeds; before = origin/master)

(The names are matched as substrings of the cell names, which print the percentage: `iid0.01` is 0.01 %, `iid5` is 5 %; `iid0.0001`, `iid0.05` match nothing. The cells that ran are the seven below. The 1 % and 5 % cells are in 15.7.)

| cell | crit | before | after |
|---|---|---|---|
| H264/clean/2M | 1 | FAIL: repair 1.07 to 1.12 MB, digest differs, 31 to 32 dropped | PASS: repair 0 bytes, digest equal to off, 31 to 32 dropped |
| H264/clean/2M | 6 | FAIL (adaptive ceiling binds 0.0156 against 0.0025) | PASS (identical to the control: 0.0043) |
| H264/iid0.01/2M | 1 | FAIL: repair 2.1 to 2.2 MB, 32 dropped | PASS: repair 0, 32 dropped |
| H264/iid0.01/2M | 6 | FAIL (0.0086 against 0.0040) | PASS (0.0039, identical) |
| H264/clean/10M | 1, 6 | PASS | PASS |
| Tile/clean/2M | 1 | PASS | PASS |
| Tile/clean/2M | 6 | FAIL (premise: off offered/link 0.922, needs 0.93) | FAIL, same premise, same numbers |
| H264/clean/2M/buf1500ms | 1, 6 | PASS | PASS |
| H264/step5/2M | 4 | FAIL: "engaged before the step (start-up drops), at 5 s and at 25 s", 3 of 3 seeds | **FAIL, for a different reason**: engaged 1 500, 2 531 and 1 500 ms after the step (limit 1 000 ms) |
| Tile/step5/2M | 4 | PASS (703, 703, 937 ms) | PASS (703, 703, 937 ms) |

Per-run counters of the gate (H264, 2 Mbit/s, clean and 0.01 %): `lost_pre_fec` 42 or 43 = `queue_losses` 31 + `local_losses` 11, `losses_excused` 31 (the 11 local ones sit in a report the standing queue skipped: `adapt_congestion_skips` 1), `black_holes` 1 or 2, `local_discards` 11, `engaged = false`.

**Criterion 4 at 2 Mbit/s is not closed by the gate.** The "engaged before the step" error is gone: the controller is clean at 5 s. What remains is the estimator's own statistics: a 2 Mbit/s H.264 stream sends about 165 datagrams/s, 5 % of that is 8 a second, the report's loss is filed about 600 ms after the loss, and the engage test needs about 6 lost datagrams over a total of about 900 (`(l - 2 sqrt(l)) / n >= 0.001`, `l >= 3`): 0.75 s of loss plus the report delay is 1.5 s, and 2.5 s in the seed with sparse early losses (`p_hat` 0.0011 at 1 s, 0.0028 at 2 s). It is the same figure as section 10.3 point 8 (1 031 ms in the one seed that was not already engaged). The gate does not delay it: after the step `losses_excused` stays at 31 (the start-up's) and `queue_losses` does not move. Nothing was loosened. Shortening it means a faster engage rule at low rates, a separate decision.

### 15.7 Cells run only with the gate (no matching before; floor 50 ms unless noted)

H.264 at 2 Mbit/s with 0.1 % loss: criterion 2 PASS, 0 changes per minute, adaptive repair 0 (the start-up is excused). With 1 %, 5 % and bursty (ge3 at 1 %) loss the controller engages in all 12 of 12 runs, as it must (0.1 % stays clean by design). In those runs `queue_losses` is 31 to 40: the start-up's 31 plus 0 to 9 false positives from the random loss at the 50 ms floor (at 80 ms: 28 to 31 in `iid5/2M`, 0 at 10 Mbit/s where there is no start-up). The estimate is therefore low by those few losses, on a total of 81 to 1 028 reported per run. H.264 5 % at 10 Mbit/s: criterion 5 PASS (gated 0.0946 against the control's 0.0873), criterion 3 FAIL on the saturation share (`trim_steps` 0, not touched here). H.264 5 % at 2 Mbit/s: criterion 5 FAIL (gated 0.0924 against 0.0721, spread 0.0127), the section 10.3 point 4 failure; no master run of this cell was made, and the change only removes losses from the estimate and never gates a report.

### 15.8 Review fixes (branch `fec/llg-review`)

- **Wrap.** Every key is unwrapped against the newest acknowledged `bundle_seq` (i64, serial-number arithmetic); a test crosses the u32 wrap, and removing the unwrap fails it.
- **Credit.** A report spends only credits at or below its `last_bundle_seq`; a skipped report consumes its range's credits and leaves the later ones. A late ack of a spent credit is a debit against the next report's excused count (`debits`, `debits_paid`; 0 in every sweep cell here). `losses_excused + losses_skipped >= lost_pre_fec` is the invariant the new browserless test `the_start_up_overflow_is_excused_and_adaptive_stays_clean` asserts on 3 seeds.
- **Clock steps.** A `q` past 5 s restarts the OWD window and classifies nothing (`clock_resets`).
- **Hot path.** quinn's black-hole detector runs only inside `handle_datagram` and `handle_timeout`; the count is read once after each, not per enqueued datagram.
- **What the wiring test catches** (each removal alone): `check_black_hole` after `handle_datagram` (premise: a black hole), both calls, the excuse call (`excused` 0), the verdict hook and the ack hook (premise: queue losses were classified). Removing only the call after `handle_timeout` stays green: in this scene the black hole is detected on an acknowledgement, never on the loss timer, so nothing here exercises that path.

## 16. The engage rule follows the traffic volume (criterion 4 at 2 Mbit/s)

Code: `LossEstimator` in `ghostframe-lib/src/transport/fec_tx/adapt.rs` (`engage_shown`, `poisson_upper_tail`, `FEC_ENGAGE_TAU_US`, `FEC_ENGAGE_ALPHA`); the description is in the module doc and in `fec-v2-rev6.md` section 5.

### 16.1 Where the 1.5 to 2.5 s went (per-report trace, `H264/step5/2M`, step at 5 s, seeds 0xFB0001 to 3; `l`, `n` are the estimator's decayed counts after the report)

The stream is about 150 datagrams/s (38 per 250 ms report), so `n` is only 800 to 1 000 when the step comes: the decay floor (`FEC_ADAPT_MIN_MASS` 5 000) and the count window never bind in this scene. They are not the cause here; they are the cause of a step that comes after a long clean spell (16.4).

| seed | first report with a loss | `l` at that report | reports until the test passed | engaged at | what the test needed at that `n` |
|---|---|---|---|---|---|
| 1 | 5 735 (735 ms after the step) | 3.00 | 5 985 (3.99), 6 235 (congested, 2 lost, skipped), 6 485 (5.95) | 1 485 ms | `l - 2 sqrt(l) >= 0.001 n`: `l >= 5.8` at `n` = 985 |
| 2 | 5 985 (985 ms) | 1.00 | 6 235 (4 lost, congested, skipped), 6 735 (1.99), 6 985 (2.98), 7 235 (3.97), 7 485 (6.95) | 2 485 ms (2 531 ms bisected) | the same, 5.8 |
| 3 | 5 735 (735 ms) | 4.00 | 5 985 (4.98), 6 235 (congested), 6 485 (6.94) | 1 485 ms | the same |

Three terms, in order of size:

1. **The count the test demands: about 6 (the z margin on the observed count).** `l - 2 sqrt(l) >= 0.001 n` takes its variance from the observed `l`. With 3 to 5 losses that reads as `2 sqrt(5) = 4.5` of noise on a null that expects `0.001 x 900 = 0.9`. 6 losses at 7.5 losses/s is 0.8 s of 5 % loss, on top of the report delay.
2. **The report delay: 0.6 to 1.0 s.** No loss is in a report before 735 ms (seed 2: 985 ms): the ledger judges after the next acknowledgement and the client files a loss under the report that confirms it (15.1). Nothing in the estimator.
3. **The standing-queue gate** skips 2 and 4 losses in the congested reports (seeds 1 and 2, one report each): about one report of delay in seed 1, and in seed 2 the 4 losses that would have made 5 at 1 235 ms.

Cadence (250 ms) only costs the rounding to the next report. `MIN_LOSSES` (3) was never the binding term.

### 16.2 The rule as built

Engaging asks whether the loss rate is shown to be above the floor. Under the null (rate = floor) the `n` datagrams in the window lose `mu0 = 0.001 n`, Poisson. The estimator engages when `l >= 3` **and** the exact upper tail `P(X >= l | mu0) <= 0.0228` (the same one-sided 97.7 % as the old `z = 2`). `mu0` is the traffic: the bar in whole losses is

| `n` in the window | 200 to 500 | 800 to 1 000 | 2 000 | 5 000 | 10 000 |
|---|---|---|---|---|---|
| losses needed | 3 | 4 | 6 | 11 | 18 |

(old rule: 6 at 1 000, 12 at 5 000, 19 at 10 000). The change is largest where few datagrams make the window, which is where a loss is a whole damaged frame (a 2 Mbit/s frame is about 5 datagrams). Three losses in 1 000 datagrams are 8 % likely at the floor: not engaged (`excused_losses_count_in_total_but_not_in_lost` keeps asserting it).

The test reads its own accumulator pair with a window in **seconds of traffic**: time constant 5 s (`FEC_ENGAGE_TAU_US`), or the 10 000-datagram count window, whichever forgets more, and **no minimum mass**. At 165 datagrams/s that is about 800 datagrams, not the 5 000 (30 s) the release side holds up; above about 2 000 datagrams/s (20 Mbit/s) the count window is the shorter and the pair equals the old one. Release is untouched: it reads the slow pair, with the same bound, dwell and minimum mass (leaving r >= 1 stays the cautious move).

### 16.3 Results (`GHOSTFRAME_FEC_SWEEP=full`, the cells and 3 seeds of the task; before = 9e510742)

| cell | crit | before (ms from the step, 3 seeds) | after |
|---|---|---|---|
| H264/step5/2M | 4 | 1 500, 2 531, 1 500: FAIL | **750, 2 250, 750: FAIL on seed 2** |
| H264/step5/10M | 4 | PASS | 468, 703, 468: PASS |
| H264/step5/30M | 4 | PASS | 281, 234, 234: PASS |
| H264/step5/100M | 4 | PASS | 187, 140, 187: PASS |
| Tile/step5/2M | 4 | 703, 703, 937 | 703, 703, 937: PASS (unchanged) |
| Tile/step5/10M, 30M, 100M | 4 | PASS | 468 / 421 / 421, 234 x3, 140 / 140 / 187: PASS |
| H264 clean 2M, 10M; iid0.01 2M; Tile clean 2M; H264 clean 2M with the 1 500 ms buffer | 1 | PASS | PASS (repair 0 bytes, digest equal to off) |
| H264 iid0.1 2M and 10M, Tile iid0.1 2M | 2 | PASS | PASS (changes per minute 0, 0, 0 / 0, 2, 0 / 0, 0, 0; adaptive repair 0 at 2 M) |

Seed 2 at 2 Mbit/s still fails and **no engage rule can fix it**. Its first loss is in the report at 985 ms (one loss), the next report (1 235 ms) is a congested one whose 4 losses the standing-queue gate skips by design, and the third counted loss arrives at 2 235 ms. Engaging inside 1 000 ms would take engaging on 1 loss, which `a_single_stray_loss_never_engages` forbids; counting the gated report's losses would reach 5 at 1 235 ms, still late. The criterion is bounded below by when the reports bring the third non-gated loss: 735 ms for seeds 1 and 3, 2 235 ms for seed 2, and the rule now meets that bound in all three (750, 2 250, 750, the 15 ms past 735 / 2 235 being the bisection's 50 ms steps). Criterion 4 as written ("engaged within 1 s, every seed") cannot hold at 2 Mbit/s with 3 losses of evidence and a 0.6 to 1 s report delay: a restatement (a share of seeds, or against the third loss's arrival) is the owner's call.

### 16.4 A step after a long clean spell (unit test `a_late_step_at_a_low_rate_engages_as_fast_as_an_early_one`, 160 datagrams/s, 5 % step)

With the engage pair on the slow window (the break check) a step 60 s into a clean link engages after 2.25 s; on its own window it engages as fast as one after 5 s (within 0.75 s, and under 2 s). That is the dilution the owner described, and it is the term the 5 s window removes. It is not exercised by the sweep's step scenes (the step is at 5 s).

### 16.5 Gates and break checks

`browserless_fec_adapt` 28 of 28, `browserless_fec_rate` 1, `browserless_fec_frame_notch` 3, `browserless_fec_blocks` 7, `browserless_fec_framing` 5, `cargo test --lib -p ghostframe-lib transport::` 1 101 passed, `clippy --workspace --all-targets -D warnings` and `fmt --check` clean. Unit tests added: `the_poisson_tail_is_the_exact_one`, `the_engage_bar_follows_the_volume_of_traffic` (fails when the exact tail is swapped back for `(l - 2 sqrt(l)) / n`), `a_late_step_at_a_low_rate_engages_as_fast_as_an_early_one` (fails when the engage test reads the slow pair). Stationary 0.1 % unit runs (20 seeds x 3 rates x 5 min, at most 2 changes a minute) and 0.01 % (never engages) pass unchanged. At exactly the floor the controller can still engage by a Poisson fluctuation (about 2 % per independent window, as before); it stays engaged then, because release needs the slow window's upper bound under the floor.
