# SPDX-License-Identifier: MIT OR Apache-2.0
"""Aggregate three wasm bench runs: per config, median of the 3 medians, and min..max spread."""
import sys, collections, statistics
rows = collections.defaultdict(list)
for f in sys.argv[1:]:
    for line in open(f):
        p = line.rstrip("\n").split("\t")
        if len(p) < 10: continue
        rows[tuple(p[:5])].append((float(p[5]), float(p[6]), float(p[7]), float(p[8]), p[9]))
print("variant\tmode\tk\tr\tlost\tenc_med_us\tdec_med_us\tdec_med_spread(min..max over runs)\tdec_p95_med\tok")
for key, v in rows.items():
    dm = [x[2] for x in v]
    print("\t".join([*key, f"{statistics.median(x[0] for x in v):.1f}", f"{statistics.median(dm):.1f}",
                     f"{min(dm):.1f}..{max(dm):.1f}", f"{statistics.median(x[3] for x in v):.1f}",
                     "ok" if all(x[4] == "ok" for x in v) else "FAIL"]))
