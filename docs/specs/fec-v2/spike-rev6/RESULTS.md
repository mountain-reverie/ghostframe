# FEC v2 revision 6 code-selection spike: results

Throwaway measurement spike. Question: which erasure code for packet-level FEC over all server-to-client
QUIC datagrams (tiles and H.264 fragments), 1180-byte symbols, small blocks closed at the end of each emission
drain (so k varies: 1..200, r in 1..8), receiver in wasm32 in a browser. Context: `../fec-v2-design.md` section 4.2,
`../libs-survey-2026-11.md`. Everything here is reproducible with `./run.sh` (`./run.sh native` or `./run.sh wasm`
for one half). Raw numbers are in `results/`; the tables below are generated from them by `make_tables.py`.

## Setup

- Host: AMD Ryzen 5 3600 (Zen 2), x86-64. rusty_erasure picked its **x86_64/avx2** kernel (no GFNI, no AVX-512 on this CPU;
  `results/B_native_kernel.txt`). rustc 1.96.1, Node 26.11.1 (V8). No browser was used for wasm.
- Candidates: `rusty_erasure =0.4.3` (two matrices: its built-in Cauchy, and **our normalised Cauchy with repair row 0 = all
  ones** through `Matrix::from_bytes`); the in-house scalar `gf256` prototype from `../wasm-size/gf256` (copied to
  `common/gf256.rs`, used as the oracle); `raptorq 2.0.1`; `cu-fec 1.3.0` (RFC 8681 RLC, GF(2^8), full density).
- Layout: `common/` (shared code), `native/` (one binary, subcommands `correct|speed|rq|rlc|varsize`), `wasm/` (one
  crate, one cargo feature per candidate, identical exports), `bench.mjs` (wasm timing), `size.sh`, `run.sh`.
  Each crate has an empty `[workspace]`; target dirs are `target/` (git-ignored).
- Native timing: release, 200 timed iterations after warm-up, median (p95 is in the raw files). Inputs that a call consumes
  are cloned outside the timed region. Wasm timing: `performance.now()` per call, 50 warm-up + 400 timed iterations, 3 separate
  node processes per variant. Wasm builds: `opt-level=s`, LTO, `codegen-units=1`, `panic=abort`, strip; `+simd128` where stated.
  `wasm-opt` (binaryen, found in the wasm-pack cache) was used **only** for the second size table; timings use the plain cargo output.
- "Cold" means the matrix and `Coder` are built inside the timed call (k varies per block, so a cached coder per (k, r) is
  optimistic); "warm" means the coder exists already.

## Finding that shapes the verdict: `Coder::recover` inverts a k x k matrix

rusty_erasure's `recover` selects k survivors and inverts the full k x k matrix whatever the loss count, so even **one** loss
costs O(k^3): 2.7 us at k=10, 18 us at k=40, **280 us at k=200** native (the survey measured the same 616 us in wasm). It also
never takes an XOR shortcut, even with our all-ones row 0. I therefore added `common/smalldec.rs` (about 70 lines, public API
only): it solves the e x e system for the e missing sources and hands the resulting linear map to rusty_erasure's SIMD
kernel as the parity block of a custom matrix; for e = 1 with repair 0 present it calls `raid::xor_gen`. It is validated by
the same exhaustive tests as everything else (measurement A). Rows below named "e x e" use it; "recover" is upstream as is.

## A. Correctness (must-have): PASS

`results/A_correct.txt`: **53 627 checks, 0 failures** (release build).
- Repair symbols from rusty_erasure with our XOR-first matrix are byte-identical to the gf256 oracle for all 7 listed (k, r)
  configurations and for the full 36-point (k, r) grid; repair row 0 is all ones and equals the XOR of the sources on the grid.
- Decode (rebuilding every erased shard, sources and repairs, and checking the oracle decoder agrees): **every** erasure pattern of
  size <= r over the k+r shards for (2,1) 3, (4,2) 21, (5,1) 6, (10,2) 78, (10,4) 1470, (12,3) 575, (20,4) 12 950 patterns;
  600 random patterns each for (40,8), (64,8), (200,8) (half of them exactly r erasures).
- More than r erasures: 400 random patterns per exhaustive configuration (2800 total) all returned `Err(TooManyMissing)` and
  left the output buffers untouched (sentinel bytes), no wrong bytes.
- `smalldec` restored every missing source in all the same patterns.
- **The test bites.** Mutant 1: flip one coefficient of repair row 1 (`--mutate-repair1`): 13 954 failing checks
  (`encode_vs_oracle` 35, `oracle_decode` 13 873, `decode_err` 23, `smalldec_bytes` 23). Mutant 2: change a row-0 coefficient
  from 1 to 3 (`--mutate-row0`): 14 303 failing checks including `row0_is_xor` 36 and `row0_all_ones` 36. Both mutants are killed
  (exit code recorded in `results/A_mutant_*.txt`). Note what bites: rusty's own decode of a mutated matrix still returns the
  original bytes (the code stays self-consistent while it stays invertible), so the byte-identity comparison against the
  independent oracle is the load-bearing check, not "decode returns the sources".

## B. Native speed (us, 1180-byte symbols)

Reading guide: encode is per block (all r repairs). Decode "1 loss" loses source 0; "r losses" loses the first r sources with all
r repairs present. Full grid k in {1..200} x r in {1,2,4,8} is in `results/B_native_speed.tsv` (includes p95).
RaptorQ decode was fed r+3 repairs and stops when it can (0 failures in every timed config).


**Native encode, us (median of 200), rusty_erasure, our XOR-first matrix**

| r \ k | 1 | 2 | 4 | 10 | 20 | 40 | 64 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|---|
| r=1 | 0.24 | 0.34 | 0.54 | 1.05 | 2.19 | 3.81 | 5.56 | 14.3 | 23.3 |
| r=2 | 0.35 | 0.52 | 0.89 | 1.92 | 3.89 | 7.44 | 12.4 | 25.8 | 41.6 |
| r=4 | 0.59 | 0.89 | 1.55 | 3.67 | 7.37 | 14.5 | 23.4 | 48.3 | 78.2 |
| r=8 | 1.12 | 1.77 | 3.16 | 5.91 | 14.4 | 28.2 | 45.3 | 95.6 | 158 |

**Native encode, us (median of 200), rusty_erasure, built-in Cauchy**

| r \ k | 1 | 2 | 4 | 10 | 20 | 40 | 64 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|---|
| r=1 | 0.24 | 0.34 | 0.54 | 1.06 | 2.21 | 4.29 | 6.55 | 14.3 | 22.8 |
| r=2 | 0.35 | 0.52 | 0.90 | 1.92 | 3.88 | 7.33 | 12.3 | 25.2 | 42.2 |
| r=4 | 0.59 | 0.93 | 1.56 | 3.44 | 7.33 | 13.9 | 22.1 | 47.6 | 79.0 |
| r=8 | 1.11 | 1.79 | 3.18 | 6.49 | 14.8 | 27.9 | 45.6 | 97.0 | 156 |

**Native encode, us (median of 200), in-house gf256 scalar oracle**

| r \ k | 1 | 2 | 4 | 10 | 20 | 40 | 64 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|---|
| r=1 | 0.20 | 0.29 | 0.37 | 0.67 | 1.16 | 2.48 | 3.57 | 6.27 | 11.4 |
| r=2 | 0.40 | 1.73 | 4.22 | 11.9 | 25.2 | 51.3 | 82.7 | 169 | 252 |
| r=4 | 0.74 | 4.51 | 11.8 | 35.7 | 72.7 | 151 | 245 | 491 | 776 |
| r=8 | 1.36 | 9.95 | 27.7 | 78.8 | 171 | 339 | 564 | 1112 | 1774 |

**Native encode, us (median of 200), raptorq 2.0.1**

| r \ k | 1 | 2 | 4 | 10 | 20 | 40 | 64 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|---|
| r=1 | 66.2 | 66.2 | 66.9 | 59.8 | 133 | 211 | 468 | 1044 | 1200 |
| r=2 | 66.5 | 67.0 | 67.7 | 65.3 | 144 | 217 | 483 | 1058 | 1242 |
| r=4 | 68.0 | 68.2 | 67.2 | 61.2 | 146 | 214 | 441 | 998 | 1198 |
| r=8 | 69.1 | 69.6 | 63.4 | 65.1 | 141 | 234 | 450 | 743 | 1204 |

**Native decode, us (median of 200), 1 loss**

| candidate | r | 1 | 2 | 4 | 10 | 20 | 40 | 64 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|---|---|
| rusty ours, `recover`, coder cached | r=2 | 0.51 | 0.70 | 1.14 | 2.65 | 5.77 | 17.9 | 41.5 | 130 | 280 |
| rusty ours, `recover`, matrix+coder built per block | r=2 | 0.67 | 0.92 | 1.50 | 3.32 | 7.42 | 21.7 | 47.2 | 149 | 297 |
| rusty ours, e x e solve + rusty kernel (cold) | r=2 | 0.30 | 0.58 | 0.64 | 1.03 | 1.76 | 3.21 | 4.69 | 8.99 | 14.0 |
| rusty built-in Cauchy, `recover`, cached | r=2 | 0.51 | 0.71 | 1.14 | 2.65 | 5.84 | 17.2 | 43.8 | 140 | 283 |
| gf256 scalar oracle | r=2 | 2.00 | 2.09 | 2.29 | 2.33 | 3.59 | 4.87 | 7.19 | 13.0 | 18.2 |
| raptorq | r=2 | 58.9 | 58.6 | 59.6 | 58.2 | 133 | 211 | 307 | 985 | 1056 |
| rusty ours, plain XOR of survivors (`raid::xor_gen`) | r>=1 | - | 0.20 | 0.22 | 0.40 | 0.92 | 1.77 | 2.96 | 5.99 | 9.78 |

**Native decode, us (median of 200), r losses (first r sources lost, all r repairs present)**

| candidate | r | 1 | 2 | 4 | 10 | 20 | 40 | 64 | 128 | 200 |
|---|---|---|---|---|---|---|---|---|---|---|
| rusty ours, `recover`, coder cached | r=2 | 0.67 | 0.96 | 1.64 | 3.79 | 8.68 | 25.9 | 58.3 | 192 | 409 |
| rusty ours, `recover`, coder cached | r=8 | 1.69 | 2.69 | 4.88 | 11.9 | 29.1 | 72.6 | 155 | 465 | 1022 |
| rusty ours, `recover`, matrix+coder built per block | r=2 | 0.83 | 1.18 | 2.01 | 4.68 | 10.1 | 28.0 | 63.8 | 203 | 399 |
| rusty ours, `recover`, matrix+coder built per block | r=8 | 2.06 | 3.31 | 6.05 | 12.6 | 34.4 | 84.1 | 172 | 510 | 1073 |
| rusty ours, e x e solve + rusty kernel (cold) | r=2 | 0.86 | 1.52 | 2.23 | 3.74 | 6.86 | 12.4 | 20.1 | 39.0 | 63.3 |
| rusty ours, e x e solve + rusty kernel (cold) | r=8 | 0.88 | 1.53 | 3.68 | 14.4 | 27.7 | 50.8 | 79.7 | 164 | 257 |
| rusty built-in Cauchy, `recover`, cached | r=2 | 0.67 | 0.96 | 1.65 | 3.87 | 8.82 | 24.5 | 63.0 | 194 | 387 |
| rusty built-in Cauchy, `recover`, cached | r=8 | 1.70 | 2.65 | 4.90 | 12.1 | 29.1 | 73.6 | 168 | 541 | 1012 |
| gf256 scalar oracle | r=2 | 1.98 | 3.93 | 6.50 | 13.8 | 27.1 | 52.4 | 84.7 | 162 | 248 |
| gf256 scalar oracle | r=8 | 2.04 | 5.30 | 20.4 | 97.6 | 184 | 347 | 579 | 1141 | 1798 |
| raptorq | r=2 | 58.3 | 57.5 | 58.9 | 54.8 | 128 | 212 | 302 | 985 | 1061 |
| raptorq | r=8 | 58.6 | 58.9 | 53.3 | 54.9 | 121 | 219 | 307 | 680 | 1070 |



Highlights: rusty_erasure encodes k=10 r=2 in 1.9 us, k=40 r=8 in 28 us, k=200 r=8 in 158 us (about 12 GB/s of multiply-accumulate),
6x to 12x faster than the scalar prototype for k >= 10 and r >= 2, and 7x to 35x faster than raptorq. The built-in Cauchy and our
XOR-first matrix cost the same to encode and to `recover`; the matrix choice only matters for the 1-loss shortcut. raptorq has a
flat floor of about 60 us for k <= 10 (its smallest source block is 10 symbols) against 0.24 us for rusty at k=1.

## C. Wasm size (bytes; delta over a baseline probe with the same glue and a byte-loop XOR codec)

Baseline: 15 519 raw / 6 686 gzip / 5 825 brotli. gzip -9, brotli -q 11.

```
variant	raw	gzip	brotli	d_raw	d_gzip	d_brotli
base	15519	6686	5825	0	0	0
gf	21454	9425	8171	5935	2739	2346
rusty-lean-scalar	32049	13532	11724	16530	6846	5899
rusty-lean-simd128	37636	15651	13502	22117	8965	7677
rusty-scalar	40377	16662	14417	24858	9976	8592
rusty-simd128	46506	18920	16236	30987	12234	10411
rq	201938	120061	110041	186419	113375	104216
rq-simd128	201371	119790	109922	185852	113104	104097
rlc	26286	11691	9985	10767	5005	4160

wasm-opt -Oz
variant	raw	gzip	brotli	d_raw	d_gzip	d_brotli
base	13876	6400	5475	0	0	0
gf	19199	8923	7708	5323	2523	2233
rusty-lean-scalar	28268	12936	11207	14392	6536	5732
rusty-lean-simd128	33648	14971	12972	19772	8571	7497
rusty-scalar	35661	15963	13775	21785	9563	8300
rusty-simd128	41539	18187	15726	27663	11787	10251
rq	188316	120701	110315	174440	114301	104840
rq-simd128	187752	120428	109986	173876	114028	104511
rlc	23843	11025	9432	9967	4625	3957
```

"lean" = our matrix + `encode` + `recover` only; the unsuffixed rusty rows also carry the built-in Cauchy and the e x e path.
gzip deltas (plain cargo output): gf256 prototype +2.7 KB, cu-fec +5.0 KB, rusty_erasure lean +6.8 KB scalar / +9.0 KB simd128,
full +10.0 / +12.2 KB, raptorq +113 KB. wasm-opt -Oz shrinks every variant by 0.5 to 0.7 KB gzip except raptorq (raw -13 KB, but gzip +0.6 KB).
`+simd128` costs rusty about 2.2 KB gzip. For scale, today's client wasm is 65.7 KB gzip. Not measured: the size effect of
`+simd128` on the other 160 KB of the client bundle.

## D. Wasm speed (Node/V8, us)

All 142 configurations restored the exact bytes (flag line below the table). Cold: matrix and coder are built inside each call.
Encode times in wasm are in `results/D_wasm_summary.tsv` (rusty simd128 k=40 r=8: 52 us; k=200 r=8: 256 us; scalar 393 / 1967).

**Wasm (Node 26 / V8) decode, us: best of 3 runs of the per-run median [slowest of the 3 run medians], 400 timed iterations per run after 50 warm-up**

| candidate | k=10 r=2 lost=1 | k=10 r=2 lost=2 | k=10 r=8 lost=8 | k=40 r=2 lost=1 | k=40 r=8 lost=1 | k=40 r=8 lost=8 | k=200 r=2 lost=1 | k=200 r=8 lost=1 | k=200 r=8 lost=8 |
|---|---|---|---|---|---|---|---|---|---|
| rusty ours, recover (scalar) | 16 [28] | 29 [47] | 109 [194] | 67 [119] | 77 [134] | 448 [806] | 573 [996] | 623 [1083] | 2866 [5166] |
| rusty ours, recover (simd128) | 6.3 [8.3] | 8.4 [13] | 24 [40] | 25 [42] | 35 [58] | 107 [179] | 360 [604] | 410 [696] | 1198 [2030] |
| rusty built-in Cauchy (scalar) | 16 [28] | 29 [51] | 108 [189] | 67 [117] | 75 [128] | 447 [790] | 572 [975] | 613 [1068] | 2877 [5027] |
| rusty built-in Cauchy (simd128) | 6.1 [11] | 8.3 [13] | 24 [38] | 25 [41] | 32 [56] | 106 [176] | 359 [606] | 399 [680] | 1158 [1977] |
| rusty ours, e x e (scalar) | 7.7 [12] | 28 [49] | 107 [190] | 28 [42] | 28 [42] | 404 [731] | 138 [204] | 138 [220] | 2003 [3665] |
| rusty ours, e x e (simd128) | 3.4 [7.8] | 7.0 [11] | 22 [36] | 3.2 [5.8] | 3.1 [5.4] | 64 [101] | 12 [22] | 11 [22] | 297 [495] |
| gf256 scalar | 12 [18] | 19 [32] | 82 [152] | 31 [49] | 31 [50] | 303 [564] | 149 [233] | 148 [236] | 1498 [2788] |
| gf256 scalar code, +simd128 (autovec) | 11 [17] | 19 [32] | 82 [152] | 31 [48] | 31 [50] | 303 [559] | 148 [232] | 148 [244] | 1499 [2796] |
| raptorq | 267 [473] | 264 [472] | 260 [450] | 720 [1273] | 721 [1262] | 724 [1267] | 3169 [5556] | 3147 [5459] | 3161 [5476] |
| raptorq, +simd128 | 267 [470] | 265 [454] | 260 [455] | 719 [1230] | 718 [1223] | 724 [1252] | 3151 [5422] | 3149 [5445] | 3159 [5410] |
| cu-fec window=k | 126 [207] | 248 [400] | 997 [1650] | 493 [809] | 495 [818] | 3943 [6530] | - | - | - |
| cu-fec window=k, +simd128 | 126 [207] | 248 [401] | 998 [1624] | 495 [825] | 496 [811] | 3946 [6096] | - | - | - |
| baseline byte-loop XOR (reference) | 9.6 [15] | - | - | 39 [60] | 39 [63] | - | 195 [310] | 195 [312] | - |

wasm correctness flag: 142 of 142 configs restored the exact bytes.

**Noise warning.** Within a set of 3 runs, one (sometimes two) of the runs was about 1.5x to 1.8x slower than the best, across all
candidates in that run, which is the "up to 2x" seen in October. The first number is the best run, the bracket the slowest; the
median of 3 (`results/D_wasm_summary.tsv`) sits in between and equals the best in many rows but not all. Read absolute values as
+/-70 %. The ordering of the well-separated candidates is stable: in each of the 3 runs, rusty e x e with simd128 beat the scalar
prototype, raptorq and cu-fec in all 32 comparable configurations. Median and p95 per run are in `results/D_wasm_run*.tsv`.

Reading: with `+simd128`, rusty_erasure with the e x e wrapper decodes **k <= 40 with 1 loss in about 3 us**, k=200 with 1 loss in
12 us (both the XOR path), and 8 losses at k=40 in 64 us, at k=200 in 300 us. Upstream `recover` with simd128 is 1.2x faster than the
scalar prototype for one loss at k=40 (25 vs 31 us) and 2.4x slower at k=200 (360 vs 149 us), because of the k x k inversion; for 8
losses at k=40 it is 2.8x faster (107 vs 303 us). Without simd128 it is slower than the prototype at every k >= 40. Without
`+simd128`, rusty e x e (28 us at k=40 1 loss) is about equal to the scalar prototype (31 us, which includes cloning every shard);
SIMD pays off for multi-loss decode: 8 losses at k=40 is 64 us with simd128 against 303 us scalar prototype. `+simd128` makes no
difference for raptorq or cu-fec (no explicit SIMD; autovectorisation did nothing visible). raptorq is flat 260 to 270 us at k=10,
720 us at k=40, 3.2 ms at k=200 whatever the loss. cu-fec costs 126 us (window 10, 1 loss) to 3.9 ms (window 40, 8 losses),
about 20x to 30x its native time: it is per-byte code and its const-generic state (about 100 KB) is zeroed on every call.

## RaptorQ reception overhead

`results/3_raptorq_overhead.tsv`; 4000 random subsets per cell, order randomised. "mixed": n symbols drawn uniformly from the
k sources plus 2k+8 repairs; "repairs-only": all n symbols are repairs.

**RaptorQ decode failures given exactly n received symbols (4000 trials each)**

| scenario | k | n=k | n=k+1 | n=k+2 |
|---|---|---|---|---|
| mixed | 4 | 23/4000 | 0/4000 | 0/4000 |
| mixed | 10 | 24/4000 | 0/4000 | 0/4000 |
| mixed | 20 | 13/4000 | 0/4000 | 0/4000 |
| mixed | 40 | 11/4000 | 0/4000 | 0/4000 |
| mixed | 200 | 27/4000 | 0/4000 | 0/4000 |
| repairs-only | 4 | 17/4000 | 0/4000 | 0/4000 |
| repairs-only | 10 | 17/4000 | 0/4000 | 0/4000 |
| repairs-only | 20 | 21/4000 | 0/4000 | 0/4000 |
| repairs-only | 40 | 16/4000 | 0/4000 | 0/4000 |
| repairs-only | 200 | 22/4000 | 0/4000 | 0/4000 |


Exactly k symbols fail in 0.3 % to 0.7 % of subsets at every k; k+1 and k+2 never failed in 4000 trials (so the failure rate at
k+1 is below about 1e-3; I did not push the trial count to resolve RFC 6330's quoted ~1e-2 per extra symbol). In practice raptorq
wants one repair beyond the number of losses: at k=10 that is 10 % extra for the same recovery power an MDS code gets with exactly
as many repairs as losses.

## 4. cu-fec (RFC 8681 sliding-window RLC), native

Window w of 1180-byte symbols (all pushed, repairs cover the whole window), full density GF(2^8), random distinct repair keys,
20 000 trials per row, random lost positions. Cost columns are native us; wasm costs are in section D.

```
## cost
window	losses	encode_one_repair_us(med)	p95	decode_us(med)	p95
20	1	5.6	5.6	8.6	8.6
20	2	5.6	5.6	14.6	14.6
20	3	5.6	5.6	20.7	20.8
20	4	5.6	5.6	26.9	27.1
40	1	11.0	11.0	15.9	15.9
40	2	11.0	11.0	27.4	27.5
40	3	11.0	11.0	39.0	39.2
40	4	11.0	11.0	50.7	55.3
## failure probability (random lost positions, random distinct repair keys; decode includes all received sources)
window	losses	repairs_received	trials	failures	fail_rate
20	1	1	20000	0	0.00000
20	1	2	20000	0	0.00000
20	2	2	20000	63	0.00315
20	2	3	20000	0	0.00000
20	3	3	20000	89	0.00445
20	3	4	20000	1	0.00005
20	4	4	20000	81	0.00405
20	4	5	20000	0	0.00000
40	1	1	20000	0	0.00000
40	1	2	20000	0	0.00000
40	2	2	20000	66	0.00330
40	2	3	20000	0	0.00000
40	3	3	20000	93	0.00465
40	3	4	20000	0	0.00000
40	4	4	20000	81	0.00405
40	4	5	20000	0	0.00000
```

Not MDS: with exactly L repairs for L losses it fails 0.3 % to 0.5 % of the time for L = 2..4 (0 of 20 000 for L=1); one spare
repair brought it to <= 5e-5 (1 failure in 20 000 at w=20, L=3; none elsewhere). Encode is 5.6 us (w=20) / 11 us (w=40) per repair, decode 8.6 us to 27 us (w=20) and 16 us to 51 us
(w=40) for 1 to 4 losses, scalar. I did not test genuinely overlapping sliding windows (repairs covering different, partly
disjoint ranges), only the block-shaped use, so the "sliding" advantage is unmeasured here.

## E. Variable symbol sizes

```
rusty_erasure encode with unequal lengths: Err(ShardLength { index: 9, expected: 1180, got: 100 })
cu-fec push_source(100 B) into a 1180 B config: Some(SymbolSizeMismatch)
rusty_erasure padded block (9 x 1180 B + 100 B): short symbol + 1 other lost, 9/9 recover with exact length
rusty-ours k=10 r=2 all-1180: encode 1.17 us, decode(2 lost, warm) 2.40 us
rusty-ours k=10 r=2 short-last(100): encode 1.17 us, decode(2 lost, warm) 2.40 us
rusty-ours k=10 r=2 random-100..1180: encode 1.16 us, decode(2 lost, warm) 2.41 us
pad one 100 B datagram into a 1182 B shard (alloc + zero + copy): 0.050 us
raptorq: 15 packets for a 10720 B object; packet payload sizes: {1180}
raptorq decode of the short-last object (2 sources lost): len Some(10720), exact=true
raptorq all-1180 k=10 r=2: encode 42.1 us, decode(2 lost) 36.2 us
raptorq short-last k=10 r=2: encode 42.9 us, decode(2 lost) 36.1 us
```

- **rusty_erasure** (and cu-fec) require equal-length shards: unequal lengths return `ShardLength` / `SymbolSizeMismatch`. The
  workable scheme is zero-padding to the block's longest symbol, with the true length carried **inside** the coded bytes
  (2-byte prefix, so a lost short symbol is restored with its length): tested, 9 of 9 patterns losing the short symbol plus one
  other recover exact bytes. Cost is nil: encode 1.17 us and 2-loss decode 2.40 us are the same for all-1180, short-last and random
  sizes (padding is just more bytes to multiply), and the pad itself costs 0.05 us. What it does cost is bandwidth: every
  repair symbol is the full padded length, and the prefix adds 2 bytes (repairs are 1182 B here).
- **raptorq** takes the object as bytes, pads the last symbol itself and returns exactly `transfer_length` bytes (10 720 here).
  Its repair packets are always a full symbol. Trap: `ObjectTransmissionInformation::with_defaults(len, 1180)` rounds the symbol
  to a multiple of 8 (1176) and silently changes K; use `::new(len, 1180, 1, 1, 1)`. A block with a short last symbol costs the
  same as a full one.

## Verdict for this use (small, variable k, closed per drain, wasm receiver)

1. **rusty_erasure =0.4.3 with our XOR-first matrix, decoded through the e x e wrapper (or `xor_gen` for a single loss).**
   Fastest on both sides: native encode 1.9 us at k=10 r=2; wasm decode 3 us (1 loss) to 8 us (2 losses) for k <= 10 and 3 us to 64 us
   for k=40 up to r=8, with `+simd128`. +6.8 KB gzip scalar, +9.0 KB with simd128 (lean build). Byte-identical to the oracle on
   every tested pattern. Conditions: (a) do not use upstream `recover` for k >= 40 (k^3); (b) `+simd128` for the whole bundle is an
   owner decision (without it the benefit over the in-house code is mostly server-side and multi-loss); (c) the survey's project-risk
   caveats stand (young crate, one maintainer), so pin `=0.4.3` or vendor, and keep the gf256 prototype as the permanent differential
   oracle, with the mutation test above.
2. **In-house gf256 (scalar).** Smallest (+2.7 KB gzip), correct, 1-loss decode is cheap at any k (XOR path; 12 to 149 us in wasm
   including shard clones). Its weakness is multi-repair encode: 12 us (k=10 r=2), 339 us (k=40 r=8), 1.8 ms (k=200 r=8) native, 6x to
   12x slower than rusty, so a server SIMD kernel would have to be written. Reasonable fallback if the owner refuses the dependency
   and k stays small.
3. **cu-fec**, only if the design genuinely moves to sliding windows: +5 KB, but 20x slower in wasm than native, not MDS, no SIMD.
4. **raptorq**: rejected for this use. +113 KB gzip, a 60 us native / 270 us wasm floor even at k=1..10, 3.2 ms at k=200, and it needs
   about one repair more than losses at every k.

## Caveats and what was not measured

- One host (Zen 2, AVX2, no GFNI), one wasm engine (V8 in Node, not a browser; Liftoff/TurboFan tiers not separated). Wasm run-to-run
  variance is large (see above). No ARM/NEON numbers.
- Wasm decode rows that build a coder (rusty) include the table setup; gf256/raptorq rows include cloning the shards/packets that
  their API consumes.
- Rate of decode failure for the Cauchy codes is zero by construction (MDS) and was checked by the exhaustive/random patterns,
  not by a proof for every (k, r) up to k+r = 256.
- `smalldec` is our code on top of rusty's public API; it passed the same patterns, but it is not upstream-tested.
- Not measured: tape-reed-solomon and the other survey candidates (out of scope); memory use; GFNI/AVX-512 paths; cu-fec with
  overlapping windows; `+simd128` effect on the rest of the client bundle; a Rust-side cap on wasm stack for cu-fec's 100 KB state.
