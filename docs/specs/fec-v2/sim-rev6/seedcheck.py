#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Seed-to-seed spread of a few headline cells (run: python3 -I seedcheck.py > seedcheck_out.txt)."""
import os
import random
import sys
import zlib

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import uniform_sim as u

CASES = [
    ("rs_time T25 k64 rmin1", {"fam": "rs_time", "T": 25, "kmax": 64, "rmin": 1}),
    ("rs_time T5 k64 rmin1", {"fam": "rs_time", "T": 5, "kmax": 64, "rmin": 1}),
    ("rq frame/burst rmin2", {"fam": "rq", "rmin": 2, "kcap": 10 ** 9}),
    ("rlc w64 flush", {"fam": "rlc", "w": 64, "flush": True}),
]
for rate in (10, 30):
    for mix, lname, lr, lb in (("h264", "ge2b3", 0.02, 3.0), ("h264", "iid2", 0.02, 1.0), ("tiles", "ge2b3", 0.02, 3.0)):
        for name, spec in CASES:
            out = []
            for seed in range(5):
                d = u.gen_demand(mix, rate, 60000, random.Random(1000 + seed))
                tl = u.build_timeline(d, rate, spec, 0.2, random.Random(seed))
                lost = u.ge_chain(len(tl.dep), lr, lb, random.Random(zlib.crc32(f"s{seed}".encode())))
                ev = u.evaluate(tl, u.decode(tl, lost, random.Random(seed + 9)), lost)
                out.append((ev["resid"] * 100, ev["fr"]["late33"] * 100 if ev["fr"] else float("nan"), ev["w95"]))
            r = [o[0] for o in out]
            l = [o[1] for o in out]
            w = [o[2] for o in out]
            print(f"{rate:>3}M {mix:<5} {lname:<6} {name:<24} resid% min/max {min(r):.3f}/{max(r):.3f}  "
                  f"late33% min/max {min(l):.2f}/{max(l):.2f}  w95 min/max {min(w):.1f}/{max(w):.1f}")
