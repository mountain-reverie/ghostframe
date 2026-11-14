#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Item 4 (fec-v2-rev6.md 8.1): H.264 frames above ~70 Mbit/s, split GF(2^8) blocks vs one block.

Run: python3 -I split_sim.py [--quick] [--jobs N]     (self-test: python3 -I split_sim.py --selftest)
Writes split_sim_out.txt next to this file.

Schemes (equal nominal overhead rho = R / k_frame, R = max(1, ceil(rho k_frame)) repairs per frame):
  a_il      m = ceil((k+R)/256) GF(2^8) RS blocks per frame, bundle i -> block i mod m, r = ceil(R/m) each
  a_contig  same m and r, bundles assigned contiguously (control)
  b_mds     one ideal-MDS block over the frame (GF(2^16), Leopard-like), r = R
  c_rq      one RaptorQ block per frame, r = R (reception overhead 1e-2 / 1e-4 / 1e-6)
  d_T5/d_T33  rev-6 default: time-capped RS, k_max 64, T 5 ms / 33.3 ms (uniform_sim rs_time)
For a-c, tile bursts (mix) get plain RS blocks of <= 64 at the same rho; (d) codes tiles too.
Repairs of a frame queue behind its sources, round-robin across blocks. Everything else (sender,
bottleneck, loss, metrics) is uniform_sim's. Estimates, not measurements.
"""
import math
import os
import random
import sys
import zlib
from multiprocessing import Pool

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import uniform_sim as u

GF8_MAX = 256


def nrep_frame(k, rho):
    return max(1, math.ceil(rho * k - 1e-9)) if rho > 0 else 0


def split_count(k, R):
    """Smallest m with every block (k_b + r) <= 256 for the interleaved/contiguous layouts."""
    m = max(1, math.ceil((k + R) / GF8_MAX))
    while math.ceil(k / m) + math.ceil(R / m) > GF8_MAX:
        m += 1
    return m


def plan_il(k, rho, broken=False):
    R = nrep_frame(k, rho)
    m = split_count(k, R)
    r = math.ceil(R / m)
    out = [(list(range(j, k, m)), r, "rs") for j in range(m)]
    if broken and m > 1:
        out[0] = (out[0][0], 0, "rs")
    return out


def plan_il_broken(k, rho):
    return plan_il(k, rho, broken=True)


def plan_contig(k, rho):
    R = nrep_frame(k, rho)
    m = split_count(k, R)
    r = math.ceil(R / m)
    base, extra = divmod(k, m)
    out, pos = [], 0
    for j in range(m):
        sz = base + (1 if j < extra else 0)
        out.append((list(range(pos, pos + sz)), r, "rs"))
        pos += sz
    return out


def plan_mds(k, rho):
    return [(list(range(k)), nrep_frame(k, rho), "rs")]


def plan_rq(k, rho):
    return [(list(range(k)), nrep_frame(k, rho), "rq")]


def split_spec(plan):
    return {"fam": "split", "plan": plan, "kmax": 64, "rmin": 1}


SCHEMES = [
    ("a_il", split_spec(plan_il)),
    ("a_contig", split_spec(plan_contig)),
    ("b_mds", split_spec(plan_mds)),
    ("c_rq", split_spec(plan_rq)),
    ("d_T5", {"fam": "rs_time", "T": 5, "kmax": 64, "rmin": 1}),
    ("d_T33", {"fam": "rs_time", "T": 1000.0 / 30, "kmax": 64, "rmin": 1}),
]
SPECS = dict(SCHEMES)
SPECS["a_il_broken"] = split_spec(plan_il_broken)

LOSSES = [("iid1", 0.01, 1.0), ("iid2", 0.02, 1.0), ("ge2b3", 0.02, 3.0),
          ("ge2b8", 0.02, 8.0), ("ge2b16", 0.02, 16.0)]
RATES = (70, 100, 150, 200)
RHOS = (0.1, 0.2, 0.3)


def n_src_for(rate, mix, frames):
    h, t = {"h264": (0.70, 0.0), "mix": (0.45, 0.20)}[mix]
    avg_p = h * rate * 125.0 * u.FRAME_MS / u.PKT
    return int(frames * avg_p * (h + t) / h) + 50


def run_cell(rate, mix, scheme, rho, seed, frames, losses):
    """One timeline, several loss draws. Returns {lossname: metrics}."""
    spec = SPECS[scheme]
    d = u.gen_demand(mix, rate, n_src_for(rate, mix, frames), random.Random(1000 + seed))
    tl = u.build_timeline(d, rate, spec, rho, random.Random(seed))
    fsrc = [s for (k, _), sids in tl.unit_src.items() if k == 'f' for s in sids]
    res = {}
    for (ln, lr, lb) in losses:
        lost = u.ge_chain(len(tl.dep), lr, lb, random.Random(zlib.crc32(f"L{seed}|{rate}|{mix}|{ln}".encode())))
        av = u.decode(tl, lost, random.Random(seed + 9))
        ev = u.evaluate(tl, av, lost)
        un, waits = 0, []
        for s in fsrc:
            p = tl.s_pkt[s]
            if lost[p]:
                if av[s] is None:
                    un += 1
                else:
                    waits.append(av[s] - tl.arr[p])
        waits.sort()
        fr = ev["fr"]
        res[ln] = {"late33": 100 * fr["late33"], "late66": 100 * fr["late66"],
                   "resid_f": 100 * un / len(fsrc), "w95": u.pct(waits, 0.95),
                   "ovh": 100 * tl.n_rep / tl.n_src, "frames": fr["n"]}
    return res


def job(a):
    return a[:6], run_cell(*a)


def selftest():
    # 1. block structure: m, r, block sizes, <=256, disjoint cover; equal total overhead within m
    for k in (100, 250, 350, 494, 700, 1480):
        for rho in RHOS:
            R = nrep_frame(k, rho)
            for plan in (plan_il, plan_contig):
                p = plan(k, rho)
                m = len(p)
                assert sorted(i for idx, _, _ in p for i in idx) == list(range(k))
                assert all(len(idx) + r <= GF8_MAX for idx, r, _ in p)
                tot = sum(r for _, r, _ in p)
                assert R <= tot < R + m, (k, rho, R, tot, m)
                assert m == 1 or m >= math.ceil((k + R) / 256)
    p = plan_il(500, 0.2)
    assert len(p) == 3 and p[1][0][:3] == [1, 4, 7], p[1][0][:3]
    # 2. a broken (a) (r = 0 on one block) must show a clearly worse residual
    kw = dict(rate=100, mix="h264", rho=0.2, seed=1, frames=300, losses=[("iid2", 0.02, 1.0), ("ge2b3", 0.02, 3.0)])
    good = run_cell(scheme="a_il", **kw)
    bad = run_cell(scheme="a_il_broken", **kw)
    for ln in ("iid2", "ge2b3"):
        g, b = good[ln], bad[ln]
        print(f"selftest {ln}: a_il resid_f {g['resid_f']:.4f}% late33 {g['late33']:.2f}% | "
              f"broken resid_f {b['resid_f']:.4f}% late33 {b['late33']:.2f}% ovh {g['ovh']:.1f}/{b['ovh']:.1f}")
        assert b["resid_f"] > 3 * g["resid_f"] + 0.05, "broken (a) not detected"
        assert b["late33"] > g["late33"] + 5, "broken (a) not detected in late frames"
        assert b["ovh"] < g["ovh"], "broken (a) should send fewer repairs"
    # 3. measurement non-zero: the loss path really loses and (b) really repairs
    m = run_cell(scheme="b_mds", rate=100, mix="h264", rho=0.2, seed=1, frames=300, losses=[("iid2", 0.02, 1.0)])["iid2"]
    assert m["frames"] >= 290 and m["ovh"] > 15
    print("selftest ok")


def main():
    if "--selftest" in sys.argv:
        selftest()
        return
    quick = "--quick" in sys.argv
    jobs = os.cpu_count() or 4
    if "--jobs" in sys.argv:
        jobs = int(sys.argv[sys.argv.index("--jobs") + 1])
    frames = 150 if quick else int(os.environ.get("SPLIT_FRAMES", "500"))
    sfr = 150 if quick else int(os.environ.get("SPLIT_SEED_FRAMES", "500"))
    nseeds = 2 if quick else 5
    names = [n for n, _ in SCHEMES]

    work = [(rate, mix, sc, rho, 0, frames, LOSSES)
            for rate in RATES for mix in ("h264", "mix") for rho in RHOS for sc in names]
    key = [("ge2b3", 0.02, 3.0), ("ge2b8", 0.02, 8.0)]
    seedwork = [(rate, mix, sc, 0.2, sd, sfr, key)
                for rate in (100, 200) for mix in ("h264", "mix") for sc in names for sd in range(1, nseeds + 1)]
    print(f"{len(work)} grid cells ({frames} frames) + {len(seedwork)} seed cells ({sfr} frames), {jobs} workers",
          file=sys.stderr)
    with Pool(jobs) as pool:
        out = dict(pool.imap_unordered(job, work + seedwork, chunksize=1))

    here = os.path.dirname(os.path.abspath(__file__))
    f = open(os.path.join(here, "split_sim_out.txt"), "w")
    f.write(f"# split_sim.py: item 4 (rev6 8.1). Model estimates. Grid: seed 0, ~{frames} frames per cell "
            f"(~{frames/30:.0f} s of video), one seed.\n"
            "# a_il = m GF(2^8) blocks interleaved, a_contig = m contiguous, b_mds = one ideal GF(2^16) MDS block,\n"
            "# c_rq = RaptorQ per frame, d_T5/d_T33 = time-capped RS k_max 64 (T 5 ms / 33.3 ms). rho = R/k_frame.\n"
            "# late33/66 = % H.264 frames completing > 33/66 ms after loss-free completion (or never); "
            "res = % H.264 source bundles unrecovered; w95 = p95 recovery wait (ms, FEC delay only).\n"
            "# ovh = repair/source bundles actually sent (whole run, so includes tile repairs in mix).\n\n")
    f.write("## 1. Grid (one seed). Per cell: late33 / late66 / res / w95 for each scheme\n")
    for mix in ("h264", "mix"):
        for rho in RHOS:
            f.write(f"\n### mix={mix} rho={rho:.1f}\n")
            f.write(f"{'rate':>4} {'loss':<7} " + " ".join(f"{n:^35}" for n in names) + "\n")
            f.write(f"{'':>4} {'':<7} " + " ".join(f"{'l33':>6} {'l66':>6} {'res':>7} {'w95':>6} {'ovh':>5}" + " " for _ in names) + "\n")
            for rate in RATES:
                for ln, _, _ in LOSSES:
                    cells = []
                    for sc in names:
                        r = out[(rate, mix, sc, rho, 0, frames)][ln]
                        cells.append(f"{r['late33']:>6.2f} {r['late66']:>6.2f} {r['resid_f']:>7.3f} {u.pct([r['w95']], 0):>6.1f} {r['ovh']:>5.1f} ")
                    f.write(f"{rate:>4} {ln:<7} " + " ".join(cells) + "\n")

    f.write(f"\n## 2. Seeds ({nseeds}, ~{sfr} frames each), rho 0.2: min/max over seeds (mean) of late33 % and res %\n")
    f.write(f"{'rate':>4} {'mix':<5} {'loss':<7} {'scheme':<9} {'late33 min/max (mean)':>26} {'late66 min/max (mean)':>26} {'res min/max (mean)':>28} {'w95 mean':>8}\n")
    gap = {}
    for rate in (100, 200):
        for mix in ("h264", "mix"):
            for ln, _, _ in key:
                for sc in names:
                    rs = [out[(rate, mix, sc, 0.2, sd, sfr)][ln] for sd in range(1, nseeds + 1)]
                    def mm(k):
                        v = [r[k] for r in rs]
                        return min(v), max(v), sum(v) / len(v)
                    l3, l6, rf = mm("late33"), mm("late66"), mm("resid_f")
                    w = sum(r["w95"] for r in rs) / len(rs)
                    gap[(rate, mix, ln, sc)] = (l3[2], l6[2], rf[2])
                    f.write(f"{rate:>4} {mix:<5} {ln:<7} {sc:<9} {l3[0]:>7.2f}/{l3[1]:<7.2f} ({l3[2]:>6.2f}) "
                            f"{l6[0]:>7.2f}/{l6[1]:<7.2f} ({l6[2]:>6.2f}) {rf[0]:>8.3f}/{rf[1]:<8.3f} ({rf[2]:>7.3f}) {w:>8.1f}\n")

    f.write("\n## 3. Gap (a_il - b_mds), late33 in points, grid cells (seed 0); largest 12 by |gap|\n")
    gaps = []
    for mix in ("h264", "mix"):
        for rho in RHOS:
            for rate in RATES:
                for ln, _, _ in LOSSES:
                    a = out[(rate, mix, "a_il", rho, 0, frames)][ln]
                    b = out[(rate, mix, "b_mds", rho, 0, frames)][ln]
                    gaps.append((a["late33"] - b["late33"], a["late66"] - b["late66"], rate, mix, rho, ln))
    gaps.sort(key=lambda g: -abs(g[0]))
    n_over = sum(1 for g in gaps if g[0] > 1.0)
    f.write(f"cells with a_il - b_mds > 1.0 point late33: {n_over} of {len(gaps)}\n")
    for g in gaps[:12]:
        f.write(f"  d33 {g[0]:+6.2f}  d66 {g[1]:+6.2f}  {g[2]}M {g[3]} rho {g[4]:.1f} {g[5]}\n")
    f.close()
    print("wrote split_sim_out.txt", file=sys.stderr)


if __name__ == "__main__":
    main()
