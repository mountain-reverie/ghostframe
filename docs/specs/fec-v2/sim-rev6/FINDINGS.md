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

## 13. R6-e2 (plan task S3): the split notch in the controller, measured end to end

Scenes: `ghostframe-e2e/tests/browserless_fec_split.rs`; the grid is `GHOSTFRAME_FEC_SWEEP=full cargo test --test browserless_fec_split the_split_grid -- --nocapture` (about 10 minutes), every run prints a `FECSPLIT run` line. PCM rate model (`pcm_rate_model`, H.264, `RefManager`), a rate cap behind a 150 ms tail-drop buffer, 25 ms one way, Gilbert-Elliott loss 1 in the bad state, 2 % on average, mean burst 3 or 8. **5 seeds, 30 virtual seconds per run** (the CI cell: 3 seeds, 20 s). Numbers are in the scratchpad logs of the S3 task. Section numbering follows section 12 (S0), which lives on the implementation branch.

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
- **The gain is not the interleave.** `m1` (no interleave, same span) equals `split`: the interleave alone is -0.01 +- 0.01 (150) and +0.01 +- 0.01 (200). With the notch's T cut to 20 ms (a test edit, not kept) `m1` equals `single` (5.96 against 5.95 on the 3-seed CI cell) and `split` over `m1` is +0.24 +- 0.16. What this data says: at these rates and this loss the late frames are cut by **the span of the notch** (T = the frame interval, so a block holds one frame instead of straddling two), not by interleaving. The interleave's own value (S0: contiguous 10.17 against interleaved 1.09 at 200 Mbit/s b8 with the per-block rule) is a burst-recovery property that this link (b8, 2 %) does not stress: a single block of 223 with 32 rows survives it. Whether the interleave matters on a link with longer or denser bursts is not measured here.
- **Overhead:** repair over source wire bytes, 0.239 (single) against 0.245 (split) at 150, 0.242 against 0.249 at 200; the split spends 0.6 to 0.7 points more.
- **Recovery wait.** The client's histogram (blocks rebuilt from repair rows, first arrival to rebuild): p95 is in the <= 20 ms bucket in 9 of the 10 b8 split runs (30 ms in one), 10 to 20 ms for the single block. S0's model p95 was 52-63 ms (up to 73 per seed); the harness does not reproduce it, because a block that closes on `k_max` (223 sources, 14 ms at 150 Mbit/s) sends its repair right behind it. What does reach the table's top: 66 of 2457 rebuilt blocks (2.7 %) at 150 b8 and 79 of 3043 (2.6 %) at 200 b8 wait more than 80 ms, the single block 39 of 1253 (3.1 %) and 36 of 1208 (3.0 %): they are the blocks recovered by a requested row, one client window (T_adv 50 ms + 10 ms) plus a round trip after the first arrival. `late_rows` is 0 and client declines are 2 to 7 per 5 runs in every arm.
- **`T_adv`.** The split's T is 34 ms (33.3 rounded up) and the block advertises 34 + `QUEUE_TARGET` 10 = 44 ms, which the table rounds up to code 6 (50 ms); the client's first window is 60 ms. `T_MS_MAX` (50) + the queue allowance (10) = 60 is inside the table's 66 ms top, by a const assertion. Above that (a `T` the knob cannot reach, or a wait above the window): code 7 saturates at 66 ms; a block that outlives its window is asked for a row (request), not declined, and a row that arrives after the give-up is counted in `late_rows` (0 in these 50 runs).

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
