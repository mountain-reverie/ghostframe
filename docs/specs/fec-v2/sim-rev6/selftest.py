#!/usr/bin/env python3
"""Sanity checks for uniform_sim.py (run: python3 -I selftest.py). Each check has a known answer."""
import math
import os
import random
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import uniform_sim as u


def run(fam_spec, rho, rate, mix, loss, burst=1.0, n=60000, seed=7):
    rng = random.Random(seed)
    d = u.gen_demand(mix, rate, n, rng)
    tl = u.build_timeline(d, rate, fam_spec, rho, random.Random(1))
    lost = u.ge_chain(len(tl.dep), loss, burst, random.Random(3))
    av = u.decode(tl, lost, random.Random(4))
    return tl, lost, av, u.evaluate(tl, av, lost)


# 1. RS(5,1) blocks (kfit, 20 %) at 10 Mbit, i.i.d. 2 %: residual from the binomial.
p, k, r = 0.02, 5, 1
n = k + r
exp = sum(math.comb(n, j) * p ** j * (1 - p) ** (n - j) * j * k / n for j in range(r + 1, n + 1)) / k
tl, lost, av, ev = run({"fam": "rs_time", "T": 1000, "kfit": True, "rmin": 1}, 0.2, 10, "tiles", 0.02)
print(f"RS(5,1) residual: sim {ev['resid']:.5f} vs binomial {exp:.5f}  (ovh {ev['ovh']:.3f})")
assert abs(ev["resid"] - exp) / exp < 0.35, "residual off the closed form"

# 2. 2 Mbit, closing at each drain gives k=1 blocks: repetition code, residual p^2, overhead 100 %.
tl, lost, av, ev = run({"fam": "rs_drain", "kmax": 64, "rmin": 1}, 0.2, 2, "tiles", 0.02)
print(f"2 Mbit drain-closed: ovh {ev['ovh']:.3f} resid {ev['resid']:.5f} vs p^2 {0.02**2:.5f}")
assert ev["ovh"] > 0.9 and abs(ev["resid"] - 0.0004) < 0.0004

# 3. No FEC: nothing is ever recovered, residual = loss.
tl, lost, av, ev = run({"fam": "none"}, 0, 10, "h264", 0.02)
assert ev["unrec"] == ev["n_lost"] and abs(ev["resid"] - 0.02) < 0.004

# 4. RLC decoder on a hand-made case: window 8, sources 0..7 sent, then repairs.
class T: pass
def rlc_case(lost_src, n_rep, seed=1):
    t = T()
    t.spec = {"w": 8}; t.fam = "rlc"
    t.src_order = list(range(8))
    t.pkind = [0] * 8 + [1] * n_rep
    t.pref = list(range(8)) + [8] * n_rep
    t.dep = [float(i) for i in range(8 + n_rep)]
    t.arr = [float(i) + 0.5 for i in range(8 + n_rep)]
    t.s_pkt = list(range(8))
    lost = [i in lost_src for i in range(8)] + [False] * n_rep
    av = [None if i in lost_src else t.arr[i] for i in range(8)]
    out = u.decode_rlc(t, lost, av, random.Random(seed))
    return sum(1 for i in lost_src if out[i] is not None)
assert rlc_case({3}, 1) == 1, "one repair must fix one loss"
assert rlc_case({3, 5}, 1) == 0, "one repair cannot fix two losses"
two_ok = sum(1 for sd in range(400) if rlc_case({3, 5}, 2, sd) == 2)
print(f"RLC: 2 losses, 2 repairs, solved in {two_ok}/400 draws (expect ~ 400*(1-1/256) = 399 or 398)")
assert two_ok >= 390

# 5. R6-e (E1): the exact tail oracle and the adaptive-r simulator carry their own checks.
import adapt_oracle
import adapt_sim
adapt_oracle.selftest()
adapt_sim.selftest()
print("selftest ok")
