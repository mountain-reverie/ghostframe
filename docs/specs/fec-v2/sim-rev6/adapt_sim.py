#!/usr/bin/env python3
"""R6-e plan task E1: the T cap, the ladder, and split-or-not, with r from the exact tail rule.

Run: python3 -I adapt_sim.py [--quick] [--jobs N] [--part cap|ladder|split|lowrate|all]   (--selftest)
Writes adapt_sim_out.txt next to this file. Estimates, not measurements.

Every block's r is adapt_oracle.r_for(k, p, b, 1e-3) with the TRUE (p, b) of the loss process
(a perfect estimator; the estimator's own error is E3/E11's), instead of a fixed overhead.
Cells: one timeline (seed s: demand, timeline and loss all re-drawn), `frames` H.264 frames of video.
Metrics: late33/late66 = % of H.264 frames completing > 1 / 2 frame intervals after the loss-free
completion (or never); tile_w95 = p95 recovery wait (ms, FEC delay only) of recovered lost tile
bundles (mix only); resid_f = % H.264 source bundles unrecovered; ovh = repairs / sources, whole run.
"""
import math
import os
import random
import sys
import zlib
from multiprocessing import Pool

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import uniform_sim as u
import split_sim as sp

TARGET = 1e-3
SEEDS = 5


def frames_for(rate):
    return 600 if rate <= 30 else 300


def spec_time(T, kmax, p, b, small_k_cap=32):
    return {"fam": "rs_time", "T": T, "kmax": kmax, "rmin": 1, "radapt": (p, b, TARGET, 32, small_k_cap)}


def run_cell(rate, mix, spec, seed, frames, loss, rho=0.0):
    ln, lr, lb = loss
    d = u.gen_demand(mix, rate, sp.n_src_for(rate, mix, frames), random.Random(1000 + seed))
    tl = u.build_timeline(d, rate, spec, rho, random.Random(seed))
    lost = u.ge_chain(len(tl.dep), lr, lb, random.Random(zlib.crc32(f"L{seed}|{rate}|{mix}|{ln}".encode())))
    av = u.decode(tl, lost, random.Random(seed + 9))
    ev = u.evaluate(tl, av, lost)
    fsrc = [s for (k, _), sids in tl.unit_src.items() if k == 'f' for s in sids]
    un = sum(1 for s in fsrc if lost[tl.s_pkt[s]] and av[s] is None)
    tw = sorted(av[s] - tl.arr[tl.s_pkt[s]] for s in range(tl.n_src)
                if tl.s_kind[s] == 'b' and tl.s_pkt[s] >= 0 and lost[tl.s_pkt[s]] and av[s] is not None)
    fr = ev["fr"]
    return {"late33": 100 * fr["late33"], "late66": 100 * fr["late66"], "resid_f": 100 * un / max(1, len(fsrc)),
            "tile_w95": u.pct(tw, 0.95) if tw else float("nan"), "ovh": 100 * ev["ovh"], "frames": fr["n"],
            "w95": ev["w95"]}


def job(a):
    key, args = a
    return key, run_cell(*args)


def stats(vals):
    vals = [v for v in vals if v == v]
    if not vals:
        return float("nan"), float("nan"), float("nan")
    return sum(vals) / len(vals), min(vals), max(vals)


def cell(m, k):
    return f"{m[k][0]:6.2f}({m[k][1]:5.2f}-{m[k][2]:5.2f})"


def selftest():
    # the adaptive spec must send fewer repairs at lower loss and more at higher (non-zero, monotone)
    a = run_cell(10, "h264", spec_time(20, 64, 0.005, 1.0), 1, 200, ("iid0.5", 0.005, 1.0))
    b = run_cell(10, "h264", spec_time(20, 64, 0.05, 1.0), 1, 200, ("iid5", 0.05, 1.0))
    print(f"selftest ovh {a['ovh']:.1f}% (0.5%) vs {b['ovh']:.1f}% (5%)")
    assert 0 < a["ovh"] < b["ovh"]
    # a wrong rule (r = 0 for every block) must be seen as loss: residual of the order of the loss rate
    z = run_cell(10, "h264", {"fam": "rs_time", "T": 20, "kmax": 64, "rmin": 0, "radapt": (0.05, 1.0, 1.0, 0, 0)},
                 1, 200, ("iid5", 0.05, 1.0))
    print(f"selftest r=0: resid_f {z['resid_f']:.2f}% ovh {z['ovh']:.2f}% vs rule resid_f {b['resid_f']:.3f}%")
    assert z["ovh"] == 0 and z["resid_f"] > 2.0 and b["resid_f"] < 0.5
    print("adapt_sim selftest ok")


def main():
    if "--selftest" in sys.argv:
        return selftest()
    quick = "--quick" in sys.argv
    part = sys.argv[sys.argv.index("--part") + 1] if "--part" in sys.argv else "all"
    jobs = int(sys.argv[sys.argv.index("--jobs") + 1]) if "--jobs" in sys.argv else (os.cpu_count() or 4)
    nseed = 2 if quick else SEEDS
    div = 4 if quick else 1
    here = os.path.dirname(os.path.abspath(__file__))
    out = open(os.path.join(here, "adapt_sim_out.txt"), "a" if part != "all" and part != "cap" else "w")
    work = []

    def add(tag, rate, mix, spec, loss, rho=0.0, seeds=None):
        for sd in range(1, (seeds or nseed) + 1):
            work.append(((tag, rate, mix, loss[0], sd), (rate, mix, spec, sd, max(60, frames_for(rate) // div), loss, rho)))

    G3 = ("ge2b3", 0.02, 3.0)
    G8 = ("ge2b8", 0.02, 8.0)
    TS = (5, 10, 15, 20, 25, 28, 33)
    KS = (64, 128, 223)
    RATES = (2, 10, 30, 100, 200)
    if part in ("cap", "all"):
        for rate in RATES:
            for mix in ("h264", "mix"):
                for loss in (G3, G8):
                    for T in TS:
                        for km in KS:
                            add(("cap", T, km), rate, mix, spec_time(T, km, loss[1], loss[2]), loss)
    if part in ("ladder", "all"):
        # loss level x burst, T 20, k_max in KS, plus the T_base notch: (T_base, 64)
        for rate in (10, 30, 100):
            for p in (0.005, 0.02, 0.05):
                for b in (1.0, 3.0, 8.0):
                    loss = (f"p{p}b{b}", p, b)
                    tb = min(max(5.0, 6.0 * u.PKT * 8.0 / (rate * 1000.0)), 20.0)
                    for (T, km, nm) in ((tb, 64, "n0"), (10, 128, "n1"), (20, 128, "n2a"), (20, 223, "n2")):
                        add(("lad", nm), rate, "h264", spec_time(T, km, p, b), loss)
    if part in ("split", "all"):
        for rate in (100, 200):
            for mix in ("h264", "mix"):
                for loss in (G3, G8):
                    for km in (128, 223):
                        add(("split", "T20", km), rate, mix, spec_time(20, km, loss[1], loss[2]), loss)
                    # a_il at 20 % and at the T20/k223 overhead is filled in by a second pass below
                    for rho in (0.1, 0.2, 0.3):
                        add(("split", "a_il", rho), rate, mix, sp.SPECS["a_il"], loss, rho)
    if part in ("lowrate", "all"):
        # 2 and 10 Mbit/s: T 20 against the design's 28 ms, and the small-k cap of the r rule
        # (32 = the plan's wording, 3, 1 = the ceil(k/4) cap with no exception for k < 4)
        for rate in (2, 10):
            for mix in ("h264", "mix"):
                for loss in (("iid2", 0.02, 1.0), G3, G8):
                    for T in (20, 28):
                        for cap in (32, 3, 1):
                            add(("low", T, cap), rate, mix, spec_time(T, 64, loss[1], loss[2], cap), loss, seeds=10)
    print(f"{len(work)} cells, {jobs} workers", file=sys.stderr)
    with Pool(jobs) as pool:
        res = dict(pool.imap_unordered(job, work, chunksize=1))
    import collections
    agg = collections.defaultdict(list)
    for key, r in res.items():
        agg[key[:4]].append(r)
    M = {k: {m: stats([r[m] for r in v]) for m in ("late33", "late66", "resid_f", "tile_w95", "ovh", "w95")} for k, v in agg.items()}
    out.write("# key: late33 / late66 (% H.264 frames), tile_w95 (ms), ovh (%): mean(min-max) over seeds\n")
    for k in sorted(M, key=lambda x: tuple(str(y) for y in x)):
        m = M[k]
        out.write(f"{k[0][0]:<6} {str(k[0][1:]):<18} {k[1]:>4}M {k[2]:<5} {k[3]:<12} late33 {cell(m,'late33')} late66 {cell(m,'late66')} "
                  f"resid {cell(m,'resid_f')} tw95 {m['tile_w95'][0]:6.1f}({m['tile_w95'][1]:5.1f}-{m['tile_w95'][2]:5.1f}) ovh {cell(m,'ovh')}\n")
    out.close()
    print("done", file=sys.stderr)


if __name__ == "__main__":
    main()
