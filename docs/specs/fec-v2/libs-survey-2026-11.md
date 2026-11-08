# Packet erasure-code libraries for ghostframe: survey update (2026-11-07)

This updates `docs/specs/fec-v2/fec-v2-design.md` §2.5/§4.2 and `sota-notes.md` §6 (Oct 2026). Metadata comes from the crates.io API and the GitHub API on 2026-11-07, plus the published crate sources (downloaded and grepped, not built, except where noted). **[M]** marks numbers I measured today, using the existing `wasm-size/` method: `wasm32-unknown-unknown`, `opt-level=s`, LTO, stripped, Node 26.11, this host. The probes are in `scratchpad/bench/`. The decode timing includes building the coder and cloning the shards, as the earlier probes did. Node timings are single runs that swing by up to about 2x between runs, so read them as orders of magnitude.

## 1. What changed since October

- **reed-solomon-simd is unchanged.** It is still at 3.1.0 (2025-10-14), and the last commit on its default branch is that release. A wasm32 backend exists only as open PR #76 (2026-06-01). The author says it was "written almost entirely with Qwen", and it has had no review. Its own native numbers show decode at small k is about 300-360 µs for 8:8 to 32:32 at 1 KiB. That is the fixed FWHT cost, so SIMD would not rescue small-k decode in wasm.
- **Two new pure-Rust GF(2^8) crates have wasm simd128 kernels:**
  - `rusty_erasure` 0.4.3 (Aug-Oct 2026), a remake of ISA-L.
  - `tape-reed-solomon` 0.2.4 (Jul-Aug 2026), Firedancer/klauspost-compatible.
- **A Rust implementation of RFC 8681 (sliding-window RLC) now exists:** `cu-fec` 1.3.0, from the copper-rs project.
- **Not found:** any Rust implementation of RFC 6865 or RFC 5510, a FlEC port, or FEC in gst-plugins-rs.

## 2. Comparison table

S = systematic. "XOR row" means the first repair symbol is a plain XOR of the sources, which gives a single-loss fast path.

| crate / lib | ver, last release | license | field / code | max shards | shard size | S | XOR row | no_std | SIMD x86 / NEON / wasm | deps | wasm Δ gzip | small-k perf |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| **rusty_erasure** [1] | 0.4.3, 2026-10-17 | MIT OR Apache-2.0 | GF(2^8) RS, any matrix (Cauchy, Vandermonde, klauspost, or `Matrix::from_bytes`) | 256 | any length, tails handled | yes | not with `Matrix::cauchy`; **yes with our own matrix via `from_bytes`** | core + facade: `no_std + alloc` | SSSE3, AVX2, AVX2-GFNI / NEON / **simd128** (compile time) | none | **+7.1 KB scalar, +9.2 KB simd [M]** | wasm simd k=10 r=2 1180 B: enc ~5 µs, dec ~7-11 µs; k=40 r=8: enc ~50-80 µs, dec ~105-170 µs [M] |
| **tape-reed-solomon** [2] | 0.2.4, 2026-08-11 | Apache-2.0 | GF(2^8) RS (klauspost Vandermonde) + GF(2^16) | 256 / 65536 | any (GF8); even (GF16) | yes | **no** (checked: klauspost's first parity row is not all ones) [M] | no (std) | SSSE3, AVX2, AVX-512, GFNI / NEON / simd128 | none | **+72 KB scalar, +94 KB simd [M]** | wasm simd k=10 r=2: enc ~8 µs, dec ~11 µs; k=40 r=8 ~240/280 µs [M] |
| reed-solomon-simd [3] | 3.1.0, 2025-10-14 | MIT AND BSD-3 | GF(2^16) Leopard FFT | 32768 + 32768 | even | yes | no | yes (since 3.1) | AVX2, SSSE3 / NEON / none (PR #76 open) | few | +10 KB (Oct) | wasm decode k=10 ~890 µs (Oct); native decode ~300 µs [3b] |
| reed-solomon-erasure [4] | 6.0.0, 2022-09-23 (repo idle since 2023-08) | MIT | GF(2^8)/GF(2^16) klauspost | 256 | equal lengths | yes | no | partial | C SIMD only | smallvec, … | +76 KB (Oct; 64 KB table) | wasm k=40 r=8 ~600 µs |
| rustfs-erasure-codec [5] | 9.0.0, 2026-09-16 | MIT | GF(2^8)/(2^16) RS + Leopard GF8/16 | 256 / 65536 | Leopard: multiple of 64 | yes | no | yes | SSSE3…GFNI, VSX / NEON / **none** | hashlink, libm, smallvec, … | not measured | — |
| reed-solomon-novelpoly [6] | 2.0.0, 2024-01-25 | Apache-2.0 AND MIT | GF(2^16) additive FFT | 65536 | even | yes | no | partial | AVX / none / none | derive_more, static_init, thiserror | not measured | README: "static offset for small n", tuned for n>100 |
| leopard-codec [7] | 0.2.0, 2025-09-18 | Apache-2.0 | GF(2^8) Leopard only | 256 | **multiple of 64** | yes | no | no | none (scalar) | bytes, thiserror | not measured | FFT fixed cost |
| reed-solomon-16 [8] | 0.1.0, 2022-01-04 | MIT AND BSD-3 | GF(2^16) | — | even | yes | no | — | — | — | — | superseded by reed-solomon-simd |
| raptorq [9] | 2.0.1, 2026-03-09 | Apache-2.0 | RFC 6330 fountain | 56403 K | any | yes | n/a | yes | auto-vectorised | — | +114 KB (Oct) | ~2-symbol overhead per block; 258 µs wasm k=10 |
| raptor-code [10] | 1.0.12, 2026-10-21 | MIT | Raptor (RFC 5053) | — | any | yes | n/a | **yes** | none | none (log optional) | not measured | non-MDS; Gaussian elimination as symbols arrive |
| cu-fec [11] | 1.3.0, 2026-10-15 | Apache-2.0 AND BSD-3 | **RFC 8681 sliding-window RLC**, GF(2)/GF(2^8), TinyMT (RFC 8682) | const-generic window | fixed per config | yes | GF(2) dense row only | yes, allocator-free | **none** (`forbid(unsafe_code)`) | none | not measured | random, not MDS: L losses need ~L+1 repairs for 99.998 % |
| rlnc [12] | 0.8.7, 2025-10-15 | BSD-3 | RLNC GF(2^8) | — | — | no (random) | no | — | none | rand =0.9.2, rayon | — | — |
| rlnc-simdx [13] | 1.3.1, 2026-07-31 | Apache-2.0 | RLNC GF(2^8) | — | — | both available | no | yes | GFNI…SSSE3 / NEON / simd128 | none | not measured | 266 downloads, 0 stars |
| tambur-rs [14] | 0.1.1, 2026-07-19 | Apache-2.0 | Tambur streaming code (Cauchy) | — | multiple of 4/8 | yes | — | partial | — | bytes, rayon | — | 36 downloads |
| gf256 [15] / galois_2p8 [16] | 0.3.1 2025 / 0.1.2 2022 | BSD-3 / MIT | field arithmetic only | — | — | — | — | — | — | — | — | building blocks only |
| webrtc-rs `rtc-interceptor` flexfec [17] | rtc 0.21.1, 2026-10 | MIT/Apache | FlexFEC draft-03, XOR | — | — | yes | XOR only | — | — | — | — | tied to RTP |

**C libraries.** These are usable from the server and the native client through FFI only. None of them helps the wasm client, so they would split the codec into two implementations that must stay byte-identical.

| library | license | notes |
|---|---|---|
| ISA-L [18] | BSD-3 | v2.32.1 (2026-07-01). Takes any matrix through `ec_init_tables`. Its Vandermonde matrix has an all-ones first parity row, but it is not MDS outside the "safe region". Rust bindings: `erasure-isa-l` (MIT, 2026-03), `isal-rs`. |
| cm256 [19] | BSD-3 | GF(256) Cauchy, k < 256, first recovery row is XOR. Last push 2024-10. |
| longhair [20] | BSD-3 (source header) | GitHub detects no LICENSE file. Last push 2024-08. |
| wirehair [21] | BSD-3 | Fountain code, about N + 0.02 symbols needed to recover. Active (2026-09). |
| leopard [22] | BSD-3 | — |
| nanors [23] | MIT | Repo now `ursoid-tech/nanors`, pushed 2026-10. Cauchy GF(2^8) and GF(2^16), plus an additive-FFT variant. Used by Sunshine and Moonlight. |
| OpenFEC [24] | CeCILL-C | RS GF(2^8) per RFC 5510, LDPC-Staircase. |
| libRaptorQ [25] | LGPL-3 (from memory, **not verified**) | Last push 2022. |

**Standards with Rust implementations:**
- RFC 8681 (RLC): `cu-fec`. Its README claims RFC 8682 TinyMT test vectors; I did not run interoperability tests.
- RFC 6865 and RFC 5510 (FECFRAME RS): none found on crates.io.
- FlexFEC (RFC 8627): only webrtc-rs's draft-03 XOR variant, which is RTP-bound.
- GStreamer: its ULPFEC and ST 2022-1 FEC elements are C, in gst-plugins-good [26]. gst-plugins-rs `net/rtp` has no FEC module (I listed the tree).
- FlEC: exists only as PQUIC plugins [27]. I found no Rust or quiche port.

## 3. Wasm measurements, side by side [M]

All four probes ran in the same session.

| probe | gzip Δ vs empty probe (6.2 KB) | k=10 r=2 1180 B enc / dec | k=40 r=8 enc / dec |
|---|---|---|---|
| in-house `gf256` prototype, scalar | +5.3 KB | 18-86 / 20-78 µs (noisy) | ~450-500 / ~450-530 µs |
| in-house `gf256` prototype, built with `+simd128` (no explicit kernel) | +5.3 KB | 18 / 19 µs | 300 / 306 µs |
| rusty_erasure, scalar | +7.1 KB | 25 / 28 µs | 393 / 449 µs |
| **rusty_erasure, `+simd128`** | **+9.2 KB** | **5-9 / 7-11 µs** | **50-80 / 105-170 µs** |
| tape-reed-solomon, `+simd128` | +94 KB | 8 / 11 µs | 239 / 283 µs |
| reed-solomon-erasure, `+simd128` | +76 KB | 28 / 29 µs | 572 / 658 µs |

A single loss at k=200 decodes in 157 µs with the prototype (the XOR path) and in 616 µs with rusty_erasure using `Matrix::cauchy`. The latter has no XOR row, which shows the value of normalising the matrix.

## 4. Ranked recommendation

**1. `rusty_erasure` (core + accel), driven by our own normalised Cauchy matrix through `Matrix::from_bytes`.** It gives exactly the code §4.2 specifies: systematic, MDS, repair row 0 = XOR, k + r ≤ 256. It also gives the SIMD kernel the design still owed: nibble tables on SSSE3/AVX2, GFNI, NEON and wasm `u8x16_swizzle`.
- It has zero dependencies, is `no_std + alloc`, accepts shards of any length, and is MIT OR Apache.
- It costs about +9 KB gzip in wasm and decodes k=10 r=2 in about 10 µs.
- `DecodePlan` caches the inversion for each erasure pattern, and a `Coder` per (k, r) caches the tables (k·r·32 B).
- **Caveats:**
  - The project is about 10 weeks old, has 1 GitHub star and one maintainer organisation ("remade-with-rust"), and its README carries an unrelated advertisement plus heavy marketing. I have not verified its claims of Kani, Miri and fuzzing.
  - The wasm SIMD path needs `+simd128` for the whole bundle (still the owner decision from §4.2).
  - MSRV is 1.95.
- **Mitigation:** pin `=0.4.3`, or vendor the roughly 5 kLOC under its licence. Keep the in-house prototype as the differential oracle, as the AGENTS.md habit asks: break the code and watch the test fail.

**2. The in-house GF(2^8) Cauchy codec (the §4.2 plan), with a hand-written SIMD kernel.** It has no third-party risk and the smallest wasm (+5 KB). It is already scalar-fast enough at k ≤ 20. The cost is writing and maintaining about four SIMD kernels, which rusty_erasure's `accel` crate shows how to write. Choose this if the owner will not take a young dependency. The option of copying the kernel technique remains open either way.

**3. Conditional: `cu-fec`, only if the design moves from blocks to a sliding window** (the Tambur / RFC 8681 direction in §2.5). It is standard and allocator-free. But it is scalar, random rather than MDS (L losses need about L+1 repairs), and young (295 downloads).

**Rejected for our use:**
- tape-reed-solomon: about 94 KB of wasm, no XOR row, no custom matrix.
- reed-solomon-simd and reed-solomon-novelpoly: a fixed FFT cost at small k; reed-solomon-simd still has no wasm SIMD.
- reed-solomon-erasure: unmaintained since 2022, 76 KB.
- leopard-codec: shards must be a multiple of 64 bytes, and it has the same FFT fixed cost.
- raptorq and raptor-code: reception overhead at k ≈ 10, and raptorq costs 114 KB.
- RLNC crates: no MDS guarantee, coefficient overhead.
- All C libraries: no wasm, and they would mean two implementations of the code.

## 5. Not verified

- **No native benchmarks.** I took no native numbers for rusty_erasure or tape at k ≤ 40 with 1.2 KB shards. Their READMEs report 4 KiB-1 MiB shards on other CPUs.
- **Untested matrix.** I did not test `from_bytes` with the normalised matrix, though the API accepts any (k+r)×k matrix.
- **Not run:** tambur-rs, rlnc-simdx and rustfs-erasure-codec were not built, and their maintenance quality was not judged beyond the metadata.
- **libRaptorQ** licence is unconfirmed.
- **Not checked:** quiche for FEC branches.
- **Possible bias in the wasm numbers.** They come from Node/V8 only, and the first run of each harness includes JIT warm-up.

## URLs

[1] https://crates.io/crates/rusty_erasure · https://github.com/remade-with-rust/rusty_erasure
[2] https://crates.io/crates/tape-reed-solomon · https://github.com/spool-labs/reed-solomon
[3] https://crates.io/crates/reed-solomon-simd · https://github.com/AndersTrier/reed-solomon-simd ; [3b] PR #76: https://github.com/AndersTrier/reed-solomon-simd/pull/76
[4] https://crates.io/crates/reed-solomon-erasure · https://github.com/rust-rse/reed-solomon-erasure
[5] https://crates.io/crates/rustfs-erasure-codec · https://github.com/houseme/rustfs-erasure-codec
[6] https://crates.io/crates/reed-solomon-novelpoly · https://github.com/paritytech/reed-solomon-novelpoly
[7] https://crates.io/crates/leopard-codec
[8] https://crates.io/crates/reed-solomon-16
[9] https://crates.io/crates/raptorq · https://github.com/cberner/raptorq
[10] https://crates.io/crates/raptor-code · https://github.com/ypo/raptor
[11] https://crates.io/crates/cu-fec · https://github.com/copper-project/copper-rs
[12] https://crates.io/crates/rlnc
[13] https://crates.io/crates/rlnc-simdx
[14] https://crates.io/crates/tambur-rs
[15] https://crates.io/crates/gf256
[16] https://crates.io/crates/galois_2p8
[17] https://github.com/webrtc-rs/rtc/tree/master/rtc-interceptor/src/flexfec
[18] https://github.com/intel/isa-l · https://crates.io/crates/erasure-isa-l
[19] https://github.com/catid/cm256
[20] https://github.com/catid/longhair
[21] https://github.com/catid/wirehair
[22] https://github.com/catid/leopard
[23] https://github.com/sleepybishop/nanors
[24] https://tracker.debian.org/media/packages/o/openfec/copyright-1.4.2.12dfsg-1
[25] https://github.com/LucaFulchir/libRaptorQ
[26] https://gstreamer.freedesktop.org/documentation/rtp/rtpulpfecenc.html · https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/tree/main/net/rtp/src
[27] https://arxiv.org/abs/2208.07741
