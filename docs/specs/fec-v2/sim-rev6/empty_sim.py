#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Empty-close experiment for RS blocks (fec-v2-rev6.md section 4 rule 3).
Run: python3 -I empty_sim.py [--quick]. Writes empty_sim_out.txt next to this file.
Reuses uniform_sim's sender (rs_time family; spec["empty"] adds the Empty close)."""
import os, random, sys, zlib
from multiprocessing import Pool
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import uniform_sim as U

RATES = (2, 10, 30, 100)
MIXES = ("h264", "tiles")
LOSS = [("iid1", 0.01, 1.0), ("iid5", 0.05, 1.0), ("ge2b3", 0.02, 3.0)]
SEEDS = range(5)
RHO = 0.2
VARIANTS = {
    "kT":          {"fam": "rs_time", "T": "formula", "kmax": 64, "rmin": 1},
    "kT+empty":    {"fam": "rs_time", "T": "formula", "kmax": 64, "rmin": 1, "empty": True},
    "kT+empty r2": {"fam": "rs_time", "T": "formula", "kmax": 64, "rmin": 1, "empty": True, "empty_rmin": 2},
    "kT+empty k>=4": {"fam": "rs_time", "T": "formula", "kmax": 64, "rmin": 1, "empty": True, "empty_kmin": 4},
    "kT+empty k>=8": {"fam": "rs_time", "T": "formula", "kmax": 64, "rmin": 1, "empty": True, "empty_kmin": 8},
}


def job(a):
    rate, mix, vname, seed, n_src = a
    spec = VARIANTS[vname]
    s0 = zlib.crc32(f"{rate}|{mix}|{seed}".encode())
    demand = U.gen_demand(mix, rate, n_src, random.Random(s0))
    tl = U.build_timeline(demand, rate, spec, RHO, random.Random(s0 + 1))
    ks = [len(b["srcs"]) for b in tl.blocks if b["srcs"]]
    meank = sum(ks) / len(ks)
    wk = sum(k * k for k in ks) / sum(ks)
    eb = [len(b["srcs"]) for b in tl.blocks if b.get("by") == "empty" and b["srcs"]]
    st = dict(fe=len(eb) / len(ks), ek=(sum(eb) / len(eb)) if eb else float("nan"),
              s3=sum(k for k in ks if k <= 3) / sum(ks), r1=sum(1 for k in ks if k <= 3) / len(ks))          # mean k seen by a source
    out = []
    n = len(tl.dep)
    for (ln, lr, lb) in LOSS:
        lost = U.ge_chain(n, lr, lb, random.Random(zlib.crc32(f"{s0}|{ln}".encode())))
        av = U.decode(tl, lost, random.Random(s0 + 7))
        r = U.evaluate(tl, av, lost)
        f = r["fr"]
        out.append((ln, dict(late=100 * f["late33"] if f else float("nan"), ex99=f["ex99"] if f else float("nan"), ex50=f["ex50"] if f else float("nan"), c0=r["cls"][0][2], w50=r["w50"], w95=r["w95"], w99=r["w99"],
                             ovh=100 * r["ovh"], resid=100 * r["resid"], k=meank, wk=wk, nb=len(ks))))
    return (rate, mix, vname, seed, out, st)


def main():
    n_src = 15000 if "--quick" in sys.argv else 60000
    work = [(r, m, v, s, n_src) for r in RATES for m in MIXES for v in VARIANTS for s in SEEDS]
    with Pool(os.cpu_count() or 4) as p:
        res = p.map(job, work, chunksize=1)
    agg = {}
    struct = {}
    for rate, mix, v, seed, out, st in res:
        struct.setdefault((rate, mix, v), []).append(st)
        for ln, d in out:
            agg.setdefault((rate, mix, ln, v), []).append(d)
    here = os.path.dirname(os.path.abspath(__file__))
    with open(os.path.join(here, "empty_sim_out.txt"), "w") as f:
        f.write(f"# Empty close for RS blocks, rho {RHO}, rmin 1, kmax 64, T = formula; mean of 5 seeds, {n_src} source bundles each.\n"
                "# late = H.264 frames >33 ms late (h264 rows); w95/w99 = recovery wait of lost bundles, ms (tiles rows); "
                "ovh = repairs/source; k = mean block k (per block / per source); resid = unrecovered %\n")
        f.write(f"{'rate':>4} {'mix':<5} {'loss':<6} {'variant':<14} {'late%':>12} {'ex50':>5} {'ex99':>6} {'w50':>5} {'c0w95':>6} {'w95':>6} {'w99':>7} {'ovh%':>6} {'k':>6} {'k/src':>6} {'resid%':>7}\n")
        for key in sorted(agg, key=lambda k: (k[0], k[1], k[2], list(VARIANTS).index(k[3]))):
            L = agg[key]
            m = lambda n: sum(d[n] for d in L) / len(L)
            lt = [d["late"] for d in L]
            f.write(f"{key[0]:>4} {key[1]:<5} {key[2]:<6} {key[3]:<14} {m('late'):>6.2f}({min(lt):.1f}-{max(lt):.1f}) "
                    f"{m('ex50'):>5.1f} {m('ex99'):>6.1f} {m('w50'):>5.1f} {m('c0'):>6.1f} {m('w95'):>6.1f} {m('w99'):>7.1f} {m('ovh'):>6.1f} {m('k'):>6.1f} {m('wk'):>6.1f} {m('resid'):>7.3f}\n")
        f.write("\n# Block structure (independent of loss; mean of 5 seeds): fe = share of blocks closed on Empty, "
                "ek = their mean k, s3 = share of source bundles in blocks with k <= 3, b3 = share of blocks with k <= 3\n")
        f.write(f"{'rate':>4} {'mix':<5} {'variant':<14} {'fe%':>6} {'ek':>6} {'s3%':>6} {'b3%':>6}\n")
        for key in sorted(struct, key=lambda k: (k[0], k[1], list(VARIANTS).index(k[2]))):
            L = struct[key]
            m = lambda n: sum(d[n] for d in L) / len(L)
            f.write(f"{key[0]:>4} {key[1]:<5} {key[2]:<14} {100*m('fe'):>6.1f} {m('ek'):>6.1f} {100*m('s3'):>6.1f} {100*m('r1'):>6.1f}\n")


if __name__ == "__main__":
    main()
