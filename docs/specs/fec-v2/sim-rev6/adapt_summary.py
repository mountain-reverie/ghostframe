#!/usr/bin/env python3
# SPDX-License-Identifier: MIT OR Apache-2.0
"""Reads adapt_sim_out.txt, prints the comparisons FINDINGS.md section 8 quotes (python3 -I adapt_summary.py).
A difference is called real only when it exceeds the larger of the two cells' seed ranges (max - min)."""
import collections
import os
import re
import sys

here = os.path.dirname(os.path.abspath(__file__))
PAT = re.compile(r"^(\w+)\s+\(([^)]*)\)\s+(\d+)M (\w+)\s+(\S+)\s+late33\s+([\d.]+)\(\s*([\d.]+)-\s*([\d.]+)\) late66\s+([\d.]+)\(\s*([\d.]+)-\s*([\d.]+)\) "
                 r"resid\s+([\d.]+)\(\s*[\d.]+-\s*[\d.]+\) tw95\s+(\S+)\(\s*\S+-\s*\S+\) ovh\s+([\d.]+)\(\s*([\d.]+)-\s*([\d.]+)\)")
D = {}
for line in open(os.path.join(here, "adapt_sim_out.txt")):
    m = PAT.match(line)
    if not m:
        continue
    part, key, rate, mix, loss = m.group(1), tuple(x.strip().strip("'") for x in m.group(2).split(",")), int(m.group(3)), m.group(4), m.group(5)
    v = [float(m.group(i)) for i in (6, 7, 8, 9, 12, 14, 15, 16)]
    tw = float(m.group(13)) if m.group(13) != "nan" else float("nan")
    D[(part, key, rate, mix, loss)] = dict(l33=v[0], l33lo=v[1], l33hi=v[2], l66=v[3], res=v[4], tw=tw, ovh=v[5], olo=v[6], ohi=v[7])


def rng(c):
    return c["l33hi"] - c["l33lo"]


def verdict(a, b):
    """a vs b late33: 'a<b', 'a>b' only when the gap exceeds both ranges, else '~'."""
    d = a["l33"] - b["l33"]
    s = max(rng(a), rng(b))
    return "~" if abs(d) <= s else ("a<b" if d < 0 else "a>b")


def part_cap():
    print("== (i) T=20 against T=25/28/33 (late33), per k_max; counts over rate x mix x loss cells")
    for rates, name in (((2, 10, 30), "<= 30 Mbit/s"), ((100, 200), "100-200 Mbit/s")):
        for km in ("64", "128", "223"):
            c = collections.Counter()
            worse = []
            for rate in rates:
                for mix in ("h264", "mix"):
                    for loss in ("ge2b3", "ge2b8"):
                        a = D.get(("cap", ("20", km), rate, mix, loss))
                        for T in ("25", "28", "33"):
                            b = D.get(("cap", (T, km), rate, mix, loss))
                            if a and b:
                                v = verdict(a, b)
                                c[(T, v)] += 1
                                if v == "a>b":
                                    worse.append((rate, mix, loss, T, round(a["l33"] - b["l33"], 2)))
            print(f"  {name} k{km}: " + ", ".join(f"T{T}: better {c[(T,'a<b')]} / same {c[(T,'~')]} / worse {c[(T,'a>b')]}" for T in ("25", "28", "33")))
            for w in worse:
                print("     T20 worse:", w)
    print("== (iii) 2 and 10 Mbit/s, k64: T20 vs T28: late33 and overhead")
    for rate in (2, 10):
        for mix in ("h264", "mix"):
            for loss in ("ge2b3", "ge2b8"):
                a, b = D.get(("cap", ("20", "64"), rate, mix, loss)), D.get(("cap", ("28", "64"), rate, mix, loss))
                if a and b:
                    print(f"  {rate}M {mix} {loss}: T20 late33 {a['l33']:.2f} ({a['l33lo']:.2f}-{a['l33hi']:.2f}) ovh {a['ovh']:.1f} | "
                          f"T28 late33 {b['l33']:.2f} ({b['l33lo']:.2f}-{b['l33hi']:.2f}) ovh {b['ovh']:.1f}  [{verdict(a,b)}]")


def part_best():
    print("== (ii) per cell: best config (min late33 mean), the indistinguishable set, cheapest member (lowest T, then k_max)")
    for rate in (2, 10, 30, 100, 200):
        for mix in ("h264", "mix"):
            for loss in ("ge2b3", "ge2b8"):
                cs = {k[1]: c for k, c in D.items() if k[0] == "cap" and k[2] == rate and k[3] == mix and k[4] == loss}
                if not cs:
                    continue
                kb = min(cs, key=lambda k: cs[k]["l33"])
                best = cs[kb]
                same = [k for k, c in cs.items() if verdict(c, best) == "~" or c is best]
                cheap = min(same, key=lambda k: (float(k[0]), int(k[1])))
                print(f"  {rate:>3}M {mix:<4} {loss}: best T{kb[0]}/k{kb[1]} late33 {best['l33']:.2f} ({best['l33lo']:.2f}-{best['l33hi']:.2f}) ovh {best['ovh']:.1f}; "
                      f"{len(same)}/{len(cs)} indistinguishable; cheapest T{cheap[0]}/k{cheap[1]} late33 {cs[cheap]['l33']:.2f} ovh {cs[cheap]['ovh']:.1f}")


def part_ladder():
    print("== ladder cells: notch late33 mean (range), ovh; first notch indistinguishable from the best")
    for rate in (10, 30, 100):
        for p in ("0.005", "0.02", "0.05"):
            for b in ("1.0", "3.0", "8.0"):
                loss = f"p{p}b{b}"
                cs = {k[1][0]: c for k, c in D.items() if k[0] == "lad" and k[2] == rate and k[4] == loss}
                if not cs:
                    continue
                best = min(cs.values(), key=lambda c: c["l33"])
                first = next(n for n in ("n0", "n1", "n2a", "n2") if verdict(cs[n], best) == "~" or cs[n] is best)
                print(f"  {rate:>3}M p{p} b{b}: " + " ".join(f"{n} {cs[n]['l33']:5.2f}({rng(cs[n]):4.2f}) o{cs[n]['ovh']:4.1f}" for n in ("n0", "n1", "n2a", "n2")) + f"  -> {first}")


def part_split():
    print("== (iv) split gate: late33 mean (range) ovh; T20 single block vs a_il at rho 0.1/0.2/0.3")
    for rate in (100, 200):
        for mix in ("h264", "mix"):
            for loss in ("ge2b3", "ge2b8"):
                row = []
                for tag, key in (("T20k128", ("T20", "128")), ("T20k223", ("T20", "223")), ("a_il.1", ("a_il", "0.1")), ("a_il.2", ("a_il", "0.2")), ("a_il.3", ("a_il", "0.3"))):
                    c = D.get(("split", key, rate, mix, loss))
                    if c:
                        row.append(f"{tag} {c['l33']:5.2f}({rng(c):4.2f}) o{c['ovh']:4.1f}")
                print(f"  {rate}M {mix:<4} {loss}: " + " | ".join(row))


def part_low():
    print("== low rate (10 seeds): T20 vs T28, small-k cap 32 (plan) / 3 / 1 : late33 mean(range) ovh mean(range)")
    for rate in (2, 10):
        for mix in ("h264", "mix"):
            for loss in ("iid2", "ge2b3", "ge2b8"):
                row = []
                for T in ("20", "28"):
                    for cap in ("32", "3", "1"):
                        c = D.get(("low", (T, cap), rate, mix, loss))
                        if c:
                            row.append(f"T{T}c{cap} {c['l33']:4.1f}({rng(c):3.1f}) o{c['ovh']:5.1f}({c['olo']:.0f}-{c['ohi']:.0f})")
                print(f"  {rate}M {mix:<4} {loss:<5}: " + " | ".join(row))


def load_split2():
    """split2_out.txt (adapt_sim.py --part split2): {(rate, mix, loss, arm): {metric: [per seed]}}."""
    out = collections.defaultdict(lambda: collections.defaultdict(list))
    path = os.path.join(here, "split2_out.txt")
    if not os.path.exists(path):
        return out
    for line in open(path):
        if line.startswith("#") or "|" not in line:
            continue
        a, b = line.split("|")
        rate, mix, loss, arm, sd = a.split()
        v = [float(x) for x in b.split()]
        for name, x in zip(("l33", "l66", "res", "w95", "ovh"), v):
            out[(int(rate), mix, loss, arm)][name].append(x)
    return out


def ms(v):
    return sum(v) / len(v), min(v), max(v)


def part_split2():
    D2 = load_split2()
    arms = ("T20k223", "il_rho0.1", "il_rho0.2", "il_rho0.3", "il_rf", "il_rf_t1e-4", "contig_rho0.2", "contig_rf")
    print("== S0: late33 mean(min-max) ovh%  | per cell; '~' = within the larger seed range of the single block")
    for mix in ("h264", "mix"):
        for rate in (100, 150, 200):
            for loss in ("ge2b3", "ge2b5", "ge2b8"):
                row = []
                base = D2.get((rate, mix, loss, "T20k223"))
                for a in arms:
                    c = D2.get((rate, mix, loss, a))
                    if not c:
                        continue
                    m, lo, hi = ms(c["l33"])
                    row.append(f"{a} {m:5.2f}({lo:4.2f}-{hi:5.2f}) o{ms(c['ovh'])[0]:4.1f} w{ms(c['w95'])[0]:4.0f}")
                print(f"  {rate}M {mix:<4} {loss}: " + " | ".join(row))
    print("== S0 (i): split vs single block. d = single - arm (points); real when d exceeds both seed ranges")
    for arm in ("il_rho0.2", "il_rf", "il_rf_t1e-4"):
        print(f"  arm {arm}")
        for mix in ("h264", "mix"):
            for rate in (100, 150, 200):
                for loss in ("ge2b3", "ge2b5", "ge2b8"):
                    a, b = D2.get((rate, mix, loss, "T20k223")), D2.get((rate, mix, loss, arm))
                    if not a or not b:
                        continue
                    (ma, la, ha), (mb, lb, hb) = ms(a["l33"]), ms(b["l33"])
                    spread = max(ha - la, hb - lb)
                    d = ma - mb
                    tag = "REAL" if d > spread else ("worse" if -d > spread else "~")
                    print(f"    {rate}M {mix:<4} {loss}: single {ma:5.2f} {arm} {mb:5.2f} d {d:+5.2f} spread {spread:4.2f} {tag} "
                          f"ovh {ms(a['ovh'])[0]:4.1f} vs {ms(b['ovh'])[0]:4.1f}")
    print("== S0 (ii): il_rf vs il_rho0.2 vs il_rho0.3: late33 / ovh / resid; il_rf late-33 vs each")
    for mix in ("h264", "mix"):
        for rate in (100, 150, 200):
            for loss in ("ge2b3", "ge2b5", "ge2b8"):
                f, r2, r3 = (D2.get((rate, mix, loss, x)) for x in ("il_rf", "il_rho0.2", "il_rho0.3"))
                if not (f and r2 and r3):
                    continue
                def v(c, sp_=None):
                    m, lo, hi = ms(c["l33"])
                    return m, hi - lo
                (mf, sf), (m2, s2), (m3, s3) = v(f), v(r2), v(r3)
                print(f"    {rate}M {mix:<4} {loss}: rf {mf:5.2f}({sf:4.2f}) o{ms(f['ovh'])[0]:4.1f} res{ms(f['res'])[0]:6.3f} | "
                      f"rho.2 {m2:5.2f}({s2:4.2f}) o{ms(r2['ovh'])[0]:4.1f} res{ms(r2['res'])[0]:6.3f} | "
                      f"rho.3 {m3:5.2f}({s3:4.2f}) o{ms(r3['ovh'])[0]:4.1f}  rf-rho.2 {mf-m2:+5.2f} "
                      f"{'in-spread' if abs(mf-m2) <= max(sf, s2) else ('rf better' if mf < m2 else 'rf worse')} ovh {ms(f['ovh'])[0]-ms(r2['ovh'])[0]:+5.1f}")
    print("== S0 (iii): recovery-wait p95 (ms, mean of seeds): single block vs split arms")
    for mix in ("h264", "mix"):
        for rate in (100, 150, 200):
            for loss in ("ge2b3", "ge2b5", "ge2b8"):
                row = [f"{a} {ms(D2[(rate, mix, loss, a)]['w95'])[0]:5.1f}({min(D2[(rate, mix, loss, a)]['w95']):4.1f}-{max(D2[(rate, mix, loss, a)]['w95']):5.1f})"
                       for a in ("T20k223", "il_rho0.2", "il_rf", "il_rf_t1e-4", "contig_rf") if (rate, mix, loss, a) in D2]
                print(f"    {rate}M {mix:<4} {loss}: " + " | ".join(row))
    print("== S0 (iv): contiguous gap (late33): contig_rho0.2 and contig_rf against interleaved il_rho0.2 and il_rf")
    for mix in ("h264", "mix"):
        for rate in (100, 150, 200):
            for loss in ("ge2b3", "ge2b5", "ge2b8"):
                g = lambda a: D2.get((rate, mix, loss, a))
                if not g("il_rho0.2"):
                    continue
                def f(a):
                    m, lo, hi = ms(g(a)["l33"])
                    return f"{m:5.2f}({lo:4.2f}-{hi:5.2f})"
                print(f"    {rate}M {mix:<4} {loss}: il_rho0.2 {f('il_rho0.2')} contig_rho0.2 {f('contig_rho0.2')} | il_rf {f('il_rf')} contig_rf {f('contig_rf')}")


def part_split2_paired():
    """Per-seed pairs (same seed = same demand and loss seed; the packet counts differ by arm, so the pairing is
    approximate): mean difference, its standard error, and the seeds where the arm is no later than the single block."""
    D2 = load_split2()
    print("== S0 paired: single - arm per seed: mean +- s.e.m., seeds where arm <= single")
    for arm in ("il_rho0.2", "il_rf", "il_rf_t1e-4"):
        print(f"  arm {arm}")
        for mix in ("h264", "mix"):
            for rate in (100, 150, 200):
                for loss in ("ge2b3", "ge2b5", "ge2b8"):
                    a, b = D2.get((rate, mix, loss, "T20k223")), D2.get((rate, mix, loss, arm))
                    if not a or not b:
                        continue
                    d = [x - y for x, y in zip(a["l33"], b["l33"])]
                    n = len(d)
                    m = sum(d) / n
                    sd = (sum((x - m) ** 2 for x in d) / (n - 1)) ** 0.5
                    print(f"    {rate}M {mix:<4} {loss}: d {m:+5.2f} +- {sd / n ** 0.5:4.2f}  arm<=single in {sum(1 for x in d if x >= 0)}/{n}")
    print("== S0 paired (ii): arm - il_rho0.2, per seed")
    for arm in ("il_rf", "il_rf_t1e-4"):
        print(f"  arm {arm}")
        for mix in ("h264", "mix"):
            for rate in (100, 150, 200):
                for loss in ("ge2b3", "ge2b5", "ge2b8"):
                    a, b = D2.get((rate, mix, loss, arm)), D2.get((rate, mix, loss, "il_rho0.2"))
                    if not a or not b:
                        continue
                    d = [x - y for x, y in zip(a["l33"], b["l33"])]
                    n = len(d)
                    m = sum(d) / n
                    sd = (sum((x - m) ** 2 for x in d) / (n - 1)) ** 0.5
                    print(f"    {rate}M {mix:<4} {loss}: late33 d {m:+5.2f} +- {sd / n ** 0.5:4.2f} (arm worse in {sum(1 for x in d if x > 0)}/{n}) "
                          f"ovh {ms(a['ovh'])[0]:4.1f} vs {ms(b['ovh'])[0]:4.1f}")


if __name__ == "__main__" and "--split2" in sys.argv:
    part_split2()
    part_split2_paired()
    sys.exit(0)


if __name__ == "__main__":
    part_cap(); part_best(); part_ladder(); part_split(); part_low()
