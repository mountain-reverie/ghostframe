#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
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


# ---- S0 (plan task S0): the split gate and the per-block r rule. Additive: nothing above this line changed. ----
S2_RATES = (100, 150, 200)
S2_BURSTS = (3.0, 5.0, 8.0)
S2_RHO_M = 0.2   # the overhead the plan's m = ceil(frame_bundles (1 + rho) / 255) is sized with


def plan_rf(p, b, contig=False, broken=False, rho_m=S2_RHO_M, target=TARGET):
    """Per-block adaptive r (the plan's S3 rule): m = clamp(ceil(k (1 + rho_m) / 255), 1, 8) blocks per
    frame, block j gets r_for(k_j, p, max(1, b / m)). Interleaved (bundle i -> block i mod m) unless
    contig. broken: r = 0 on block 0 (break check)."""
    def plan(k, rho):
        m = min(8, max(1, math.ceil(k * (1 + rho_m) / 255 - 1e-9)))
        if contig:
            base, extra = divmod(k, m)
            idx, pos = [], 0
            for j in range(m):
                sz = base + (1 if j < extra else 0)
                idx.append(list(range(pos, pos + sz)))
                pos += sz
        else:
            idx = [list(range(j, k, m)) for j in range(m)]
        out = []
        for j, ix in enumerate(idx):
            r = 0 if (broken and j == 0 and m > 1) else adapt_r_for(len(ix), p, max(1.0, b / m), target)
            out.append((ix, r, "rs"))
        return out
    return plan


def adapt_r_for(k, p, b, target=TARGET):
    import adapt_oracle
    return adapt_oracle.r_for(k, p, b, target)


def spec_rf(p, b, **kw):
    return {"fam": "split", "plan": plan_rf(p, b, **kw), "kmax": 64, "rmin": 1}


def split2_arms(loss):
    _, p, b = loss
    arms = [("T20k223", spec_time(20, 223, p, b), 0.0)]
    for rho in (0.1, 0.2, 0.3):
        arms.append((f"il_rho{rho}", sp.SPECS["a_il"], rho))
    arms.append(("il_rf", spec_rf(p, b), 0.0))
    arms.append(("il_rf_t1e-4", spec_rf(p, b, target=1e-4), 0.0))   # extra: the per-block rule at a tenth of the target
    arms.append(("contig_rho0.2", sp.SPECS["a_contig"], 0.2))
    arms.append(("contig_rf", spec_rf(p, b, contig=True), 0.0))
    return arms


def run_split2(rate, mix, spec, seed, frames, loss, rho):
    """run_cell plus the p95 recovery wait of lost H.264 source bundles (FEC delay only, as split_sim)."""
    ln, lr, lb = loss
    d = u.gen_demand(mix, rate, sp.n_src_for(rate, mix, frames), random.Random(1000 + seed))
    tl = u.build_timeline(d, rate, spec, rho, random.Random(seed))
    lost = u.ge_chain(len(tl.dep), lr, lb, random.Random(zlib.crc32(f"L{seed}|{rate}|{mix}|{ln}".encode())))
    av = u.decode(tl, lost, random.Random(seed + 9))
    ev = u.evaluate(tl, av, lost)
    fsrc = [s for (k, _), sids in tl.unit_src.items() if k == 'f' for s in sids]
    un, fw = 0, []
    for s in fsrc:
        pk = tl.s_pkt[s]
        if lost[pk]:
            if av[s] is None:
                un += 1
            else:
                fw.append(av[s] - tl.arr[pk])
    fw.sort()
    fr = ev["fr"]
    return {"late33": 100 * fr["late33"], "late66": 100 * fr["late66"], "resid_f": 100 * un / max(1, len(fsrc)),
            "w95": u.pct(fw, 0.95) if fw else 0.0, "ovh": 100 * ev["ovh"], "frames": fr["n"]}


def job2(a):
    key, (rate, mix, arm, seed, frames, loss) = a
    spec, rho = next((sp_, r_) for (n_, sp_, r_) in split2_arms(loss) if n_ == arm)   # closures do not pickle
    return key, run_split2(rate, mix, spec, seed, frames, loss, rho)


def split2_work(nseed, frames_div=1, rates=S2_RATES, bursts=S2_BURSTS, mixes=("h264", "mix")):
    work = []
    for rate in rates:
        for mix in mixes:
            for b in bursts:
                loss = (f"ge2b{int(b)}", 0.02, b)
                for (arm, _spec, _rho) in split2_arms(loss):
                    for sd in range(1, nseed + 1):
                        work.append(((rate, mix, loss[0], arm, sd),
                                     (rate, mix, arm, sd, max(60, frames_for(rate) // frames_div), loss)))
    return work


def split2_selftest():
    # structure of the per-block rule: cover, k_b + r <= 255, r >= 1, interleaved i mod m
    for k in (60, 200, 300, 394, 600, 1500):
        for b in (3.0, 8.0):
            pl = plan_rf(0.02, b)(k, 0.0)
            m = len(pl)
            assert 1 <= m <= 8
            assert sorted(i for ix, _, _ in pl for i in ix) == list(range(k))
            assert all(len(ix) + r <= 255 and r >= 1 for ix, r, _ in pl), (k, b)
            if m > 1:
                assert pl[1][0][:3] == [1, 1 + m, 1 + 2 * m]
                pc = plan_rf(0.02, b, contig=True)(k, 0.0)
                assert pc[0][0] == list(range(len(pc[0][0])))
    # break check 1: r = 0 on one block must be seen (as in section 6)
    loss = ("ge2b3", 0.02, 3.0)
    good = run_split2(150, "h264", spec_rf(0.02, 3.0), 1, 200, loss, 0.0)
    bad = run_split2(150, "h264", spec_rf(0.02, 3.0, broken=True), 1, 200, loss, 0.0)
    print(f"selftest split2: il_rf late33 {good['late33']:.2f}% resid {good['resid_f']:.3f}% ovh {good['ovh']:.1f} | "
          f"r=0 on one block late33 {bad['late33']:.2f}% resid {bad['resid_f']:.3f}% ovh {bad['ovh']:.1f}")
    assert good["ovh"] > 5 and bad["ovh"] < good["ovh"]
    assert bad["resid_f"] > 3 * good["resid_f"] + 0.05 and bad["late33"] > good["late33"] + 5
    # break check 2: contiguous must be worse than interleaved at 200 Mbit/s b8, outside the spread (5 seeds)
    loss8 = ("ge2b8", 0.02, 8.0)
    ils, cts = [], []
    for sd in range(1, 6):
        ils.append(run_split2(200, "h264", spec_rf(0.02, 8.0), sd, 300, loss8, 0.0)["late33"])
        cts.append(run_split2(200, "h264", spec_rf(0.02, 8.0, contig=True), sd, 300, loss8, 0.0)["late33"])
    mi, mc = sum(ils) / 5, sum(cts) / 5
    spread = max(max(ils) - min(ils), max(cts) - min(cts))
    print(f"selftest split2: 200M b8 late33 interleaved {mi:.2f} ({min(ils):.2f}-{max(ils):.2f}) vs contiguous "
          f"{mc:.2f} ({min(cts):.2f}-{max(cts):.2f})")
    assert mc - mi > spread, "contiguous not worse than interleaved outside the spread"
    print("split2 selftest ok")


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
    split2_selftest()
    print("adapt_sim selftest ok")


def main():
    if "--selftest" in sys.argv:
        return selftest()
    quick = "--quick" in sys.argv
    part = sys.argv[sys.argv.index("--part") + 1] if "--part" in sys.argv else "all"
    jobs = int(sys.argv[sys.argv.index("--jobs") + 1]) if "--jobs" in sys.argv else (os.cpu_count() or 4)
    nseed = 2 if quick else SEEDS
    if "--seeds" in sys.argv:
        nseed = int(sys.argv[sys.argv.index("--seeds") + 1])
    div = 4 if quick else 1
    if part == "split2":
        return main_split2(jobs, nseed, div)
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


def main_split2(jobs, nseed, div):
    here = os.path.dirname(os.path.abspath(__file__))
    work = split2_work(nseed, div)
    print(f"{len(work)} split2 cells, {jobs} workers", file=sys.stderr)
    with Pool(jobs) as pool:
        res = dict(pool.imap_unordered(job2, work, chunksize=1))
    with open(os.path.join(here, "split2_out.txt"), "w") as f:
        f.write("# adapt_sim.py --part split2 (plan S0): one line per seed. key: rate mix loss arm seed | late33 late66 resid_f w95 ovh (%/ms)\n")
        for k in sorted(res, key=lambda x: tuple(str(y) for y in x)):
            r = res[k]
            f.write(f"{k[0]} {k[1]} {k[2]} {k[3]} {k[4]} | {r['late33']:.3f} {r['late66']:.3f} {r['resid_f']:.4f} {r['w95']:.2f} {r['ovh']:.2f}\n")
    print("done", file=sys.stderr)


if __name__ == "__main__":
    main()
